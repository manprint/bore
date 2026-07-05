//! Embedded SSH ingress gateway (russh-backed) for `bore server`.
//!
//! Lets a stock OpenSSH client create public, vhost and secret tunnels with
//! `ssh -R`/`-L` and no `bore` binary on the client side. The gateway is
//! ingress-only: from the accepted SSH channel inward, the existing server
//! data path (registries, relay, admin, weblog, `--max-conns`) is reused
//! unmodified. See `docs/SSH_GATEWAY.md` for the design and
//! `docs/plans/plan_SshGateway/` for the implementation plan.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch, Semaphore};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::timeout;
use tracing::{info, trace, warn};

use crate::admin::{ActiveGuard, AdminRegistry, NewEntry, Registration, Role, Transport};
use crate::basicauth::BasicAuth;
use crate::edge;
use crate::mux;
use crate::pool::CarrierPool;
use crate::prefixed::Prefixed;
use crate::secret;
use crate::server::{bind_public_listener, DEFAULT_MAX_CONNS};
use crate::shared::{proxy_buffer_size, tune_tcp, CountingStream};
use crate::sshgw_auth::{KeyGrant, KeyStore, PasswordStore};
use crate::vhost::{
    self, cert_present, public_urls, resolve_mode, resolve_route, RouteDecision, SharedVhostConfig,
    VhostEntry, VhostMode, VhostRegistry,
};

/// Interval between server-initiated SSH keepalive probes on an authenticated
/// gateway connection. Parity with `CTRL_CLIENT_HEARTBEAT` (`src/secret.rs`),
/// deliberately far below `SSH_CTRL_TIMEOUT` so a healthy idle tunnel never
/// trips the reaper.
pub const SSH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Silence duration after which an SSH gateway connection is treated as dead
/// and torn down (all its forwards, registry entries and admin rows released).
/// Parity with `SECRET_CTRL_TIMEOUT` (`src/secret.rs`) — the same zombie-entry
/// reaper invariant applies here (I-SSH3). Enforced by russh's own
/// `keepalive_max` (see `SSH_KEEPALIVE_MAX_MISSES` and
/// `SshGateway::russh_config`'s doc), not a from-scratch timer.
pub const SSH_CTRL_TIMEOUT: Duration = Duration::from_secs(60);

/// `russh::server::Config::keepalive_max`: number of consecutive unanswered
/// server keepalive probes tolerated before russh disconnects the
/// connection on the next one. Derived so the fatal probe lands at
/// `SSH_CTRL_TIMEOUT`: russh disconnects once `alive_timeouts >
/// keepalive_max`, and `alive_timeouts` increments once per
/// `SSH_KEEPALIVE_INTERVAL` tick, so the `(SSH_KEEPALIVE_MAX_MISSES + 1)`-th
/// tick is the fatal one.
pub const SSH_KEEPALIVE_MAX_MISSES: usize =
    (SSH_CTRL_TIMEOUT.as_secs() / SSH_KEEPALIVE_INTERVAL.as_secs()) as usize - 1;

/// Grace period given to a freshly-accepted connection to complete
/// authentication before it is disconnected.
pub const SSH_PREAUTH_GRACE: Duration = Duration::from_secs(30);

/// Default per-channel SSH flow-control window (`russh::server::Config::
/// window_size`), in bytes. Each proxied connection is one `forwarded-tcpip`
/// (public/vhost) or relayed (secret) SSH channel, and SSH channel throughput
/// is bounded by `window / RTT` (the classic SSH bandwidth-delay-product cap
/// the OpenSSH-HPN patches exist to lift). russh's own default is a mere 2 MiB
/// — enough to saturate a LAN, but on a 100 ms path it caps a single proxied
/// connection at ~20 MiB/s regardless of link speed, well below what bore's
/// native yamux carrier (whose per-stream window auto-tunes to the BDP, see
/// `mux::config`) delivers on the same path. We raise the default to 16 MiB
/// (~160 MiB/s at 100 ms, ~1.6 GiB/s at 10 ms) so the SSH ingress path is not
/// gratuitously slower than the native client, and expose `--ssh-window-size`/
/// `BORE_SSH_WINDOW_SIZE` for operators who want to trade memory for even
/// higher single-connection throughput on very-high-BDP links. It is a receive
/// credit ceiling, not a preallocation — an idle or LAN-speed tunnel never
/// buffers anywhere near it.
pub const SSH_DEFAULT_WINDOW_SIZE: u32 = 16 * 1024 * 1024;

/// Per-channel SSH maximum packet size (`russh::server::Config::
/// maximum_packet_size`), in bytes — the SSH transport default (RFC 4254
/// negotiates the min of both peers, so a stock OpenSSH client caps this at
/// its own 32 KiB regardless; raising it server-side only helps a peer that
/// also raised it, and never hurts).
pub const SSH_MAX_PACKET_SIZE: u32 = 32 * 1024;

/// Floor for a configured `--ssh-window-size`: russh requires the window to be
/// at least one maximum packet, and a window below one packet would wedge the
/// channel. Clamped up to this with a warning rather than accepted.
pub const SSH_MIN_WINDOW_SIZE: u32 = SSH_MAX_PACKET_SIZE;

/// Maximum number of authentication attempts (any method) allowed on one
/// connection before russh disconnects it.
pub const SSH_MAX_AUTH_ATTEMPTS: usize = 3;

/// How long a granted `tcpip-forward` waits for `exec`/`env` parameters
/// before registering the tunnel with whatever it has. There is no
/// round-trip dependency between the two SSH requests (`tcpip-forward` is a
/// global request; `exec`/`env` are channel requests on an independently
/// opened channel), so this only covers genuine scheduling/network jitter —
/// most sessions resolve in well under this. A pure `-N` session (no
/// channel, ever) always pays the full grace period; that is the expected
/// cost of supporting the common "just forward a port" case without
/// special-casing it.
///
/// Was 500 ms; raised to 5 s after `t_ssh_pub3/4/5`/`t_ssh_vh2` (which all
/// gate on an `exec`-carried param — `notes=`/`https=on`/`force-https=on`/
/// `basic-auth=`) were caught flaking on CI (never once green since this
/// suite landed — every `ssh`-gateway CI run had failures) and reproduced
/// locally by pinning the test binary to 2 CPUs under load: the real `ssh`
/// CLI subprocess simply did not get scheduled in time to send its `exec`
/// request within 500 ms, so the server silently registered the tunnel
/// with defaults instead of the requested params. 500 ms was never a
/// protocol requirement, just an untested guess; 5 s is still well under
/// every caller's own timeout budget (the test harness's own network waits
/// were bumped to 20 s alongside this) and imperceptible for a real
/// interactive session.
const PARAMS_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on how long a secret-consumer `direct-tcpip` (`ssh -L`) channel
/// waits to open a substream to the provider before giving up and closing the
/// accepted channel. Opening is done in a spawned task (never on the russh
/// dispatch loop — see [`GatewayHandler::channel_open_direct_tcpip`]), so this
/// only bounds that task, but a bound is still needed: a provider whose control
/// connection is wedged-but-TCP-alive would otherwise leave the substream-open
/// pending until the provider's own 60 s reaper fires.
const SSH_DIRECT_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Informational (never channel-closing — see `shell_request`) line written
/// when a client's implicit shell request arrives with nothing established
/// yet on this connection: either a genuine interactive-login mistake, or a
/// secret consumer whose first proxied connection just hasn't happened yet.
const NO_FORWARD_YET_MESSAGE: &str =
    "bore ssh-gateway: interactive shells are not supported; use -R/-L forwarding. \
     No forward is established on this connection yet — if that's unexpected, check \
     your ssh command. This channel stays open either way.\r\n";

/// Validated configuration for the embedded SSH gateway, built from
/// `bore server`'s `--ssh-*` flags.
#[derive(Debug, Clone)]
pub struct SshGatewayConfig {
    /// Dedicated TCP port the gateway listens on, if any. `None` means the
    /// gateway is enabled but reachable only once control-port demux lands
    /// (a later phase, D8) — never a startup error.
    pub port: Option<u16>,
    /// Path to the ed25519 host key (PEM, OpenSSH format). Generated on first
    /// use if it does not exist yet (D9).
    pub host_key_file: PathBuf,
    /// Directory of `authorized_keys`-format files granting public-key auth.
    pub authorized_keys_dir: Option<PathBuf>,
    /// Argon2id password file granting password auth.
    pub passwords_file: Option<PathBuf>,
    /// Banner text sent to clients before authentication (SSH
    /// `SSH_MSG_USERAUTH_BANNER`, via [`GatewayHandler::authentication_banner`]).
    pub banner: Option<String>,
    /// Per-channel SSH flow-control window in bytes ([`SSH_DEFAULT_WINDOW_SIZE`]);
    /// governs single-proxied-connection throughput on high-BDP links.
    pub window_size: u32,
}

impl SshGatewayConfig {
    /// Fail fast on a configuration that could never authenticate anyone.
    pub fn validate(&self) -> Result<()> {
        if self.authorized_keys_dir.is_none() && self.passwords_file.is_none() {
            bail!(
                "--ssh-gateway requires --ssh-authorized-keys-dir and/or \
                 --ssh-passwords-file (no credential source configured)"
            );
        }
        Ok(())
    }
}

/// Load the ed25519 host key from `path`, generating and persisting a fresh
/// one (mode 0600, via [`PrivateKey::write_openssh_file`]) if it does not
/// exist yet. Logs the SHA256 fingerprint either way (D9).
fn load_or_generate_host_key(path: &Path) -> Result<PrivateKey> {
    let key = if path.exists() {
        PrivateKey::read_openssh_file(path)
            .with_context(|| format!("failed to read SSH host key {}", path.display()))?
    } else {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .context("failed to generate ed25519 SSH host key")?;
        key.write_openssh_file(path, LineEnding::LF)
            .with_context(|| format!("failed to write SSH host key {}", path.display()))?;
        info!(path = %path.display(), "ssh-gateway: generated new ed25519 host key");
        key
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    info!(path = %path.display(), %fingerprint, "ssh-gateway: host key ready");
    Ok(key)
}

/// The embedded SSH gateway: host key, credential stores, and the registries/
/// helpers tunnel serving needs. Constructed once from `Server::set_ssh_gateway`
/// and shared (via `Arc`) across every accepted connection; never re-derives
/// its registries — they are clones of the `Server`'s own.
pub struct SshGateway {
    config: SshGatewayConfig,
    host_key: PrivateKey,
    keys: Option<KeyStore>,
    passwords: Option<PasswordStore>,
    /// Wired for Phase 5.3 (`tcpip_forward` secret-provider handling).
    #[allow(dead_code)]
    providers: secret::Registry,
    /// Wired for Phase 5.2 (`tcpip_forward` vhost-subdomain handling).
    vhost_registry: VhostRegistry,
    /// SSH-session ownership of each SSH-registered vhost label, for
    /// same-identity takeover (Phase 5.4, D2/I-5). Gateway-internal only —
    /// never plumbed from `Server`, since only SSH registrations can ever be
    /// evicted (native tunnels are a different trust domain).
    vhost_owners: Arc<DashMap<String, ForwardOwner>>,
    /// Same as `vhost_owners`, keyed by secret-tunnel id.
    secret_owners: Arc<DashMap<String, ForwardOwner>>,
    /// Live vhost config (reservations, base domain, TLS mode). `None` when
    /// the server has no `vhost.yml` configured, in which case any
    /// `vhost/<label>` forward request is rejected outright.
    vhost_config: Option<SharedVhostConfig>,
    /// Admin registration, `transport: Ssh` (I-3: RAII teardown per forward).
    admin: AdminRegistry,
    /// Per-connection inbound cap, shared with the rest of the server's
    /// `--max-conns` (this bounds proxied connections, exactly like the
    /// native public/vhost/secret accept loops). Whether the SSH *control*
    /// connection itself also consumes a permit depends on how it arrived:
    /// the dedicated `--ssh-port` listener acquires one for the control
    /// connection's whole lifetime, whereas the shared control-port demux
    /// path does NOT — matching native bore control connections, which are
    /// likewise unmetered. Either way this semaphore is what actually bounds
    /// proxied traffic; the control-connection accounting difference only
    /// affects whether an idle control connection counts against `--max-conns`.
    conn_permits: Arc<Semaphore>,
    /// `permit="port/<n>"` range validation, and the pool `port == 0`
    /// auto-assign picks from.
    port_range: RangeInclusive<u16>,
    /// Wired for Phase 4.3 (public-tunnel listener bind address).
    bind_tunnels: IpAddr,
    /// Global relay byte counters, shared with every other ingress path
    /// (native public/vhost/secret) so `/admin/status`'s totals cover SSH
    /// traffic too.
    total_rx_bytes: Arc<AtomicU64>,
    total_tx_bytes: Arc<AtomicU64>,
    /// Server's TLS acceptor, if `--cert-file`/`--key-file` are configured —
    /// a clone of the `Server`'s own, snapshotted at `set_ssh_gateway` time
    /// (which always runs after `set_tls` in `main.rs`). Used to terminate
    /// TLS on a PUBLIC tunnel's own port when the client requests `https=on`
    /// (`edge::accept`, same helper the native `bore local --https` path
    /// uses) — `None` here means an `https=on` request is rejected with a
    /// warning, exactly like the native client's equivalent check.
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// Server's `--domain`/bind domain, used as the redirect target host for
    /// `force-https=on` when the client's `Host` header is absent (same
    /// fallback the native `edge::accept` path uses).
    bind_domain: Option<String>,
}

impl SshGateway {
    /// Build the gateway: validates `config`, loads/generates the host key,
    /// and wires the credential stores. `providers`/`vhost_registry`/
    /// `vhost_config`/`admin`/`conn_permits`/`port_range`/`bind_tunnels`/
    /// `total_rx_bytes`/`total_tx_bytes` must be clones of the `Server`'s
    /// own — never re-derived.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut config: SshGatewayConfig,
        providers: secret::Registry,
        vhost_registry: VhostRegistry,
        vhost_config: Option<SharedVhostConfig>,
        admin: AdminRegistry,
        conn_permits: Arc<Semaphore>,
        port_range: RangeInclusive<u16>,
        bind_tunnels: IpAddr,
        total_rx_bytes: Arc<AtomicU64>,
        total_tx_bytes: Arc<AtomicU64>,
        tls: Option<tokio_rustls::TlsAcceptor>,
        bind_domain: Option<String>,
    ) -> Result<Self> {
        config.validate()?;
        // A window below one maximum packet would wedge every channel; clamp up
        // with a warning rather than shipping a dead gateway.
        if config.window_size < SSH_MIN_WINDOW_SIZE {
            warn!(
                requested = config.window_size,
                clamped_to = SSH_MIN_WINDOW_SIZE,
                "ssh-gateway: --ssh-window-size below the one-packet floor; clamping up"
            );
            config.window_size = SSH_MIN_WINDOW_SIZE;
        }
        let host_key = load_or_generate_host_key(&config.host_key_file)?;
        let keys = config.authorized_keys_dir.clone().map(KeyStore::new);
        let passwords = config.passwords_file.clone().map(PasswordStore::new);
        Ok(Self {
            config,
            host_key,
            keys,
            passwords,
            providers,
            vhost_registry,
            vhost_owners: Arc::new(DashMap::new()),
            secret_owners: Arc::new(DashMap::new()),
            vhost_config,
            admin,
            conn_permits,
            port_range,
            bind_tunnels,
            total_rx_bytes,
            total_tx_bytes,
            tls,
            bind_domain,
        })
    }

    /// Dedicated TCP port the gateway listens on, if any.
    pub fn port(&self) -> Option<u16> {
        self.config.port
    }

    /// A fresh `russh::server::Config` for one accepted connection: the
    /// loaded host key, the pre-auth grace period, the auth-attempt cap, and
    /// the zombie-entry reaper (I-3).
    ///
    /// The reaper is entirely russh's own built-in keepalive machinery —
    /// `keepalive_interval`/`keepalive_max` — NOT a from-scratch
    /// `last_inbound`-style tracker driven by `Handler` callbacks. That was
    /// tried first and reverted: russh calls no `Handler` method at all for
    /// either side's keepalive traffic (a client's own `ServerAliveInterval`
    /// probe is an unrecognized `GLOBAL_REQUEST` auto-replied
    /// `REQUEST_FAILURE` internally; the reply to *our* probe is consumed by
    /// the same internal match with an explicit "ignore keepalives" comment
    /// — confirmed by reading `russh::server::session::Session::run` and
    /// `encrypted::reply`). A callback-driven tracker can therefore never see
    /// a purely keepalive-sustained idle connection as alive, and would
    /// falsely reap not just an idle tunnel but any tunnel that has been
    /// relaying real forwarded traffic for over `SSH_CTRL_TIMEOUT` without a
    /// *new* client request in between (forwarded-tcpip data flows over a
    /// server-opened channel via the gateway's own finalize task, which
    /// never touches a client-driven tracker either).
    ///
    /// `alive_timeouts` (the counter `keepalive_max` gates on) is immune to
    /// exactly the failure mode that motivated a custom tracker in the first
    /// place: it resets only on `common.received_data`, which is set from
    /// genuinely decoded incoming packets and NOT from this connection's own
    /// internal `Handle`-driven dispatch (e.g. `channel_open_forwarded_tcpip`
    /// for a newly accepted public connection) — so a "busy tunnel, dead
    /// client" connection still gets reaped on schedule. `keepalive_max` is
    /// tuned so the 3rd unanswered probe (at `keepalive_max + 1` intervals)
    /// lands at `SSH_CTRL_TIMEOUT`.
    ///
    /// `inactivity_timeout` stays at `SSH_PREAUTH_GRACE` and is shared across
    /// the whole connection lifetime (pre- and post-auth) — that field alone
    /// resets on any internal dispatch and so cannot substitute for the
    /// keepalive-based reaper post-auth, but it still correctly guards the
    /// pre-auth phase, where no internal dispatch exists yet.
    pub fn russh_config(&self) -> Arc<russh::server::Config> {
        Arc::new(russh::server::Config {
            keys: vec![self.host_key.clone()],
            inactivity_timeout: Some(SSH_PREAUTH_GRACE),
            keepalive_interval: Some(SSH_KEEPALIVE_INTERVAL),
            keepalive_max: SSH_KEEPALIVE_MAX_MISSES,
            max_auth_attempts: SSH_MAX_AUTH_ATTEMPTS,
            // Lift russh's default 2 MiB per-channel window so a single proxied
            // connection is not BDP-capped far below the native carrier on a
            // high-RTT path (see SSH_DEFAULT_WINDOW_SIZE). `window_size` is
            // clamped to `>= SSH_MIN_WINDOW_SIZE` in `new`.
            window_size: self.config.window_size,
            maximum_packet_size: SSH_MAX_PACKET_SIZE,
            ..Default::default()
        })
    }

    /// A fresh per-connection [`Handler`] bound to this gateway. `peer` is
    /// the client's remote address (from the accepted `TcpStream`), used to
    /// populate `admin::Entry::peer` for every forward this connection ever
    /// registers.
    pub fn handler(self: &Arc<Self>, peer: SocketAddr) -> GatewayHandler {
        GatewayHandler {
            gateway: Arc::clone(self),
            peer,
            grant: None,
            state: Arc::new(ConnState::default()),
        }
    }

    /// Accept-to-completion for one SSH gateway connection: builds the
    /// `Handler` and drives the russh session (whose own keepalive/reaper
    /// timers are configured by `russh_config`, see its doc for I-3).
    pub async fn serve_connection<S: mux::Transport>(
        self: &Arc<Self>,
        stream: S,
        addr: SocketAddr,
    ) -> Result<(), russh::Error> {
        let config = self.russh_config();
        let handler = self.handler(addr);
        russh::server::run_stream(config, stream, handler)
            .await?
            .await
    }
}

/// How long the pre-TLS ([`demux_pre_tls`]) and post-TLS ([`demux_post_tls`])
/// demux peeks wait for a client to speak first before assuming it is an SSH
/// client waiting on the server's own banner (sslh-style, D8): a stock
/// OpenSSH client — raw or tunneled through TLS via a `ProxyCommand` — obeys
/// the SSH protocol's banner-first convention and sends nothing until it
/// sees `SSH-2.0-...` from us. Every other supported protocol on this port
/// talks first: a TLS `ClientHello`, an HTTP request line, or bore's own
/// `Hello` (written eagerly — yamux is lazy, so nothing happens until the
/// client writes). All of those arrive within milliseconds, so this can be
/// generous without meaningfully delaying anyone.
pub const SSH_PEEK_TIMEOUT: Duration = Duration::from_secs(2);

/// Pure classification of a control connection's very first byte (D8, 6.1).
/// `Http`/`Bore` are kept distinct from `Tls` (rather than collapsed into
/// one "not SSH" bucket) because the demux actually branches on that
/// distinction too, not just on SSH-or-not: once the gateway demux is
/// active, a `Tls` byte (0x16) goes through the TLS acceptor (when
/// configured), while `Http`/`Bore` route DIRECTLY to `route_connection`,
/// BYPASSING the TLS acceptor entirely — this is what lets a plain HTTP or
/// plain bore client keep working on a port that also has TLS configured
/// (T-SSH-DMX1: SSH + TLS + HTTP + native bore all live on the one port
/// simultaneously). `Http` vs `Bore` themselves are not branched on further
/// here — `route_connection`'s own existing peek re-derives that from the
/// same first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// No byte yet (an SSH client waiting on our banner), or a literal
    /// `b'S'` (the start of `SSH-2.0-...`).
    Ssh,
    /// TLS `ClientHello` (0x16).
    Tls,
    /// An HTTP request-line verb byte (`admin_http::is_http_first_byte`).
    Http,
    /// Anything else — the bore protocol's yamux framing (first byte 0x00),
    /// or any other unrecognized byte (existing behavior: falls through to
    /// the bore protocol path, which will itself reject a genuinely bad
    /// client).
    Bore,
}

/// Classifies the pre-TLS first byte (`None` means "no byte within
/// [`SSH_PEEK_TIMEOUT`]").
pub fn demux_classify_first_byte(byte: Option<u8>) -> Route {
    match byte {
        None => Route::Ssh,
        Some(b'S') => Route::Ssh,
        Some(0x16) => Route::Tls,
        Some(b) if crate::admin_http::is_http_first_byte(b) => Route::Http,
        Some(_) => Route::Bore,
    }
}

/// Binary outcome of the post-TLS SSH-over-TLS check ([`demux_post_tls`],
/// 6.2, D4) — inside TLS there is nothing left to disambiguate beyond
/// "is this SSH", since HTTP-vs-bore is already the existing
/// `route_connection` peek's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixRoute {
    /// The literal `SSH-` version-string prefix.
    Ssh,
    /// Anything else.
    NotSsh,
}

/// Classifies a (post-TLS) byte prefix: the literal `SSH-` version-string
/// prefix (RFC 4253 §4.2) routes to SSH; anything else — including a prefix
/// shorter than 4 bytes (EOF before the check completed) — is not.
pub fn demux_classify_prefix(bytes: &[u8]) -> PrefixRoute {
    if bytes.starts_with(b"SSH-") {
        PrefixRoute::Ssh
    } else {
        PrefixRoute::NotSsh
    }
}

/// Outcome of [`demux_pre_tls`] — the peeked byte (if any) is preserved via
/// [`Prefixed`] in every arm, so no data is ever lost regardless of which is
/// taken.
pub enum PreTlsRoute<S> {
    /// Hand this stream to [`SshGateway::serve_connection`].
    Ssh(Prefixed<S>),
    /// A TLS `ClientHello`: accept it (if TLS is configured on this port),
    /// then apply [`demux_post_tls`] (6.2, SSH-over-TLS).
    Tls(Prefixed<S>),
    /// Neither SSH nor TLS: route DIRECTLY to `route_connection`, BYPASSING
    /// any configured TLS acceptor entirely — once the gateway demux is
    /// active, a plain HTTP or plain bore client on this port is no longer
    /// forced through a TLS handshake it never initiated.
    Direct(Prefixed<S>),
}

/// Pre-TLS demux (6.1, D8): peeks one byte with [`SSH_PEEK_TIMEOUT`] and
/// classifies it via [`demux_classify_first_byte`]. Only called when the SSH
/// gateway is enabled (I-1: the disabled path never calls this, so it adds
/// no read/timeout/wrapper there). An EOF or read error is reported as
/// [`PreTlsRoute::Direct`] with nothing buffered — the existing downstream
/// peek (`route_connection`'s own, or nothing at all when neither admin nor
/// vhost is configured) sees the SAME already-dead socket and handles it
/// exactly as it does today.
pub async fn demux_pre_tls<S: mux::Transport>(mut socket: S) -> PreTlsRoute<S> {
    let mut first = [0u8; 1];
    match timeout(SSH_PEEK_TIMEOUT, socket.read(&mut first)).await {
        Ok(Ok(1)) => {
            let prefixed = Prefixed::new(first.to_vec(), socket);
            match demux_classify_first_byte(Some(first[0])) {
                Route::Ssh => PreTlsRoute::Ssh(prefixed),
                Route::Tls => PreTlsRoute::Tls(prefixed),
                Route::Http | Route::Bore => PreTlsRoute::Direct(prefixed),
            }
        }
        Ok(Ok(_)) | Ok(Err(_)) => PreTlsRoute::Direct(Prefixed::new(Vec::new(), socket)),
        Err(_) => PreTlsRoute::Ssh(Prefixed::new(Vec::new(), socket)),
    }
}

/// Outcome of [`demux_post_tls`], mirroring [`PreTlsRoute`] one layer deeper
/// (after a successful TLS handshake).
pub enum PostTlsRoute<S> {
    /// Hand this (TLS-wrapped) stream to [`SshGateway::serve_connection`].
    Ssh(Prefixed<S>),
    /// Continue the existing `route_connection` logic on this stream.
    NotSsh(Prefixed<S>),
}

/// Post-TLS demux (6.2, D4 — SSH-over-TLS): peeks up to 4 bytes with the
/// SAME [`SSH_PEEK_TIMEOUT`] semantics as [`demux_pre_tls`], since a real
/// SSH client tunneled through TLS (e.g. via an `openssl s_client`
/// `ProxyCommand`) still obeys the SSH banner-first convention once the TLS
/// session is up — the same "silence means SSH" reasoning applies one layer
/// deeper. A short read followed by EOF/error is [`PostTlsRoute::NotSsh`]
/// (matches [`demux_pre_tls`]'s EOF handling: the existing downstream peek
/// sees the same dead socket). Only called when the SSH gateway is enabled.
pub async fn demux_post_tls<S: mux::Transport>(mut socket: S) -> PostTlsRoute<S> {
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    let read_all = async {
        while filled < buf.len() {
            match socket.read(&mut buf[filled..]).await {
                Ok(0) => return Err(()),
                Ok(n) => filled += n,
                Err(_) => return Err(()),
            }
        }
        Ok(())
    };
    match timeout(SSH_PEEK_TIMEOUT, read_all).await {
        Ok(Ok(())) => {
            let prefixed = Prefixed::new(buf.to_vec(), socket);
            match demux_classify_prefix(&buf) {
                PrefixRoute::Ssh => PostTlsRoute::Ssh(prefixed),
                PrefixRoute::NotSsh => PostTlsRoute::NotSsh(prefixed),
            }
        }
        Ok(Err(())) => PostTlsRoute::NotSsh(Prefixed::new(buf[..filled].to_vec(), socket)),
        Err(_) => PostTlsRoute::Ssh(Prefixed::new(buf[..filled].to_vec(), socket)),
    }
}

/// Per-connection `russh::server::Handler`. Holds only what one connection
/// needs; everything shared lives on [`SshGateway`]. `state` is separately
/// `Arc`-shared with this connection's forward tasks (see [`ConnState`]) —
/// those tasks run concurrently with this handler's own callbacks, which is
/// why they cannot simply reach into `&mut self`.
pub struct GatewayHandler {
    gateway: Arc<SshGateway>,
    /// The client's remote address.
    peer: SocketAddr,
    /// Grant from a successful auth (authorized-keys options, or a
    /// synthesized unrestricted grant for a password match). `None` until
    /// authenticated.
    grant: Option<KeyGrant>,
    /// Shared with every forward task spawned on this connection.
    state: Arc<ConnState>,
}

impl GatewayHandler {
    /// Identity granted by a successful auth, if any (authorized-keys
    /// comment/fingerprint, or the matched password label).
    pub fn identity(&self) -> Option<&str> {
        self.grant.as_ref().map(|grant| grant.identity.as_str())
    }

    /// This connection's grant, or an unrestricted placeholder if somehow
    /// unset. Every callback that can reach this (`exec_request`,
    /// `tcpip_forward`, `cancel_tcpip_forward`) only ever fires after a
    /// successful `auth_publickey`/`auth_password`, which always populates
    /// `grant` first — this fallback is defense-in-depth, never the normal
    /// path.
    fn grant(&self) -> KeyGrant {
        self.grant.clone().unwrap_or_else(|| KeyGrant {
            identity: "unknown".to_string(),
            permit: None,
            max_conns: None,
            notes: None,
        })
    }

    /// Registers `-R vhost/<label>` (or a bare label on port 80/443) as a
    /// vhost subdomain provider. Unlike a public tunnel there is no OS-level
    /// listener to bind — the shared vhost HTTP(S) frontend already accepts
    /// on the server's configured ports and looks up `vhost_registry` per
    /// request — so the spawned task's only job is to hold the registry +
    /// admin registrations alive (via `_guard`/`_registration`) until
    /// `cancel_tcpip_forward` or connection teardown aborts it (I-3, same
    /// RAII discipline as [`run_public_forward`]).
    async fn tcpip_forward_vhost(
        &self,
        address: &str,
        port: u16,
        label: String,
        grant: KeyGrant,
        session: &mut Session,
    ) -> Result<bool, russh::Error> {
        let Some(cfg) = self.gateway.vhost_config.clone() else {
            self.state.queue_message(
                "bore ssh-gateway: server has no vhost.yml configured; \
                 vhost/<label> forwards are unavailable"
                    .to_string(),
            );
            return Ok(false);
        };
        if !permit_allows(&grant, "vhost/", &label) {
            self.state.queue_message(format!(
                "bore ssh-gateway: this key's permit= list does not allow vhost/{label}"
            ));
            return Ok(false);
        }
        if matches!(
            peek_takeover(
                &self.gateway.vhost_registry,
                &self.gateway.vhost_owners,
                &label,
                &grant.identity,
            ),
            TakeoverDecision::Reject
        ) {
            self.state.queue_message(format!(
                "bore ssh-gateway: subdomain '{label}' already in use"
            ));
            return Ok(false);
        }

        let ssh_handle = session.handle();
        let gateway = Arc::clone(&self.gateway);
        let state = Arc::clone(&self.state);
        let peer = self.peer;
        let connected_address = address.to_string();
        let key = (connected_address.clone(), port);
        let key_for_task = key.clone();
        let label_for_message = label.clone();
        let (abort_tx, abort_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            let (exec, env) = await_params(&state).await;
            let params = parse_params(exec.as_deref(), &env, &grant);

            let live_cfg = cfg.read().unwrap().clone();
            let (request_headers, response_headers) =
                match resolve_route(&live_cfg, &label, &grant.identity) {
                    RouteDecision::Accept {
                        request_headers,
                        response_headers,
                    } => (request_headers, response_headers),
                    RouteDecision::Reject { reason } => {
                        state.queue_message(format!("bore ssh-gateway: {reason}"));
                        return;
                    }
                };

            let gateway_basic_auth = params.basic_auth.as_deref().and_then(BasicAuth::parse);
            let has_basic_auth = gateway_basic_auth.is_some();

            match apply_takeover(
                &gateway.vhost_registry,
                &gateway.vhost_owners,
                &label,
                &grant.identity,
                "subdomain",
            ) {
                TakeoverOutcome::Reject(reason) => {
                    state.queue_message(format!("bore ssh-gateway: {reason}"));
                    return;
                }
                TakeoverOutcome::Proceed => {}
            }

            let opener = SshOpener::new(ssh_handle.clone(), connected_address, port);
            let pool = Arc::new(CarrierPool::new(mux::LinkOpener::Ssh(Arc::new(opener))));
            let entry = Arc::new(VhostEntry {
                pool: Arc::clone(&pool),
                request_headers,
                response_headers,
                #[cfg(feature = "udp")]
                direct: vhost::DirectPool::default(),
                #[cfg(feature = "udp")]
                direct_stream_opens: AtomicU64::new(0),
                active: Arc::new(AtomicUsize::new(0)),
                webserver_log: params.webserver_log,
                peer,
                since: Instant::now(),
                notes: params.notes.clone(),
                basic_auth: has_basic_auth,
                udp: false,
                auto_reconnect: false,
                local_host: None,
                local_port: 0,
                relay_tx_bytes: Arc::new(AtomicU64::new(0)),
                relay_rx_bytes: Arc::new(AtomicU64::new(0)),
                gateway_basic_auth,
            });
            match gateway.vhost_registry.entry(label.clone()) {
                Entry::Occupied(_) => {
                    state.queue_message(format!(
                        "bore ssh-gateway: subdomain '{label}' already in use"
                    ));
                    return;
                }
                Entry::Vacant(slot) => {
                    slot.insert(Arc::clone(&entry));
                }
            }
            let Ok(abort) = abort_rx.await else {
                gateway.vhost_registry.remove(&label);
                return;
            };
            let token = next_forward_token();
            gateway.vhost_owners.insert(
                label.clone(),
                ForwardOwner {
                    identity: grant.identity.clone(),
                    abort,
                    handle: ssh_handle.clone(),
                    conn: Arc::downgrade(&state),
                    key: key_for_task,
                    token,
                },
            );
            let _guard = VhostSshGuard {
                registry: gateway.vhost_registry.clone(),
                owners: Arc::clone(&gateway.vhost_owners),
                label: label.clone(),
                entry: Arc::clone(&entry),
                token,
            };

            let mode = resolve_mode(&live_cfg, cert_present(&live_cfg)).unwrap_or(VhostMode::Http);
            let (http_url, https_url) = public_urls(
                &label,
                &live_cfg.base_domain,
                mode,
                live_cfg.http_port,
                live_cfg.https_port,
            );
            let urls: Vec<String> = [http_url, https_url].into_iter().flatten().collect();

            let registration = gateway.admin.register(NewEntry {
                role: Role::Vhost,
                peer,
                secret_id: Some(label.clone()),
                public_port: None,
                notes: params.notes.clone(),
                basic_auth: has_basic_auth,
                https: false,
                force_https: false,
                carriers: 0,
                auto_reconnect: false,
                webserver_log: params.webserver_log,
                udp: false,
                vpn_relay_only: false,
                vpn_pin_mtu: false,
                vpn_mtu: None,
                vpn_forward_accept: false,
                vpn_nat_masquerade: false,
                vpn_route_policy: None,
                vpn_advertised: vec![],
                vpn_nat_udp_port: None,
                local_proxy_port: None,
                local_host: None,
                local_port: None,
                nat_udp_preferred_port: None,
                nat_udp_release_timeout: None,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                max_conns: None,
                transport: Transport::Ssh,
                identity: Some(grant.identity.clone()),
            });

            for line in vhost_info_banner(VhostBannerInfo {
                urls: &urls,
                mode,
                identity: &grant.identity,
                notes: params.notes.as_deref(),
                basic_auth: has_basic_auth,
                webserver_log: params.webserver_log,
                request_headers: &entry.request_headers,
                response_headers: &entry.response_headers,
            }) {
                state.deliver(&ssh_handle, line).await;
            }

            // Nothing left to do but stay alive: the shared vhost HTTP(S)
            // frontend drives all traffic through `pool`/`registration` via
            // the registry lookup. This task's only remaining purpose is to
            // hold `_guard`/`pool`/`registration` until aborted.
            //
            // `drop(state)` is load-bearing, not cleanup theater: this task's
            // own `state: Arc<ConnState>` clone (captured above for
            // `await_params`/`queue_message`) would otherwise stay alive for
            // as long as this `pending()` future exists — i.e. forever — and
            // `ConnState`'s refcount can then never reach zero, so `Drop for
            // ConnState` (which aborts every task in `self.forwards`,
            // INCLUDING this one) never runs on an ungraceful connection
            // death (found via T-SSH-N1's real half-open netns repro: the
            // keepalive reaper correctly errors the SSH session, logged, but
            // the admin row survived forever — this reference cycle is why).
            // `cancel_tcpip_forward` and takeover's `apply_takeover` both
            // route around this (they abort via a directly-held handle, not
            // via `ConnState::drop`), which is why only the "whole session
            // just dies" path ever hit it.
            drop(state);
            let _pool = pool;
            let _registration = registration;
            std::future::pending::<()>().await;
        });
        // Handed to the task via `abort_rx` (a oneshot, not a shared cell):
        // the task's first await point (`await_params`, up to `PARAMS_GRACE`)
        // guarantees this send lands well before the task reaches the point
        // where it needs its own `AbortHandle` for `vhost_owners` (Phase 5.4)
        // — a task cannot otherwise learn its own `JoinHandle`-derived handle
        // from inside itself.
        let _ = abort_tx.send(task.abort_handle());
        self.state
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, task);

        self.state
            .queue_message(format!("vhost tunnel requested: {label_for_message}"));
        Ok(true)
    }

    /// Registers `-R secret/<id>` (or a bare label on port 0) as a secret-tunnel
    /// provider. Mirrors [`secret::serve_provider`]'s registry-insert + admin-
    /// register sequence (`src/secret.rs:254`) so the untouched native consumer
    /// relay (`serve_consumer`) reaches this provider transparently — its
    /// `pool.pick().open_ready()` opens an SSH `forwarded-tcpip` channel behind
    /// the same [`mux::LinkOpener`] abstraction it already uses for a native
    /// yamux carrier.
    async fn tcpip_forward_secret(
        &self,
        address: &str,
        port: u16,
        id: String,
        grant: KeyGrant,
        session: &mut Session,
    ) -> Result<bool, russh::Error> {
        if !permit_allows(&grant, "secret/", &id) {
            self.state.queue_message(format!(
                "bore ssh-gateway: this key's permit= list does not allow secret/{id}"
            ));
            return Ok(false);
        }
        if matches!(
            peek_takeover(
                &self.gateway.providers,
                &self.gateway.secret_owners,
                &id,
                &grant.identity
            ),
            TakeoverDecision::Reject
        ) {
            self.state
                .queue_message(format!("tcp-secret-id '{id}' already in use"));
            return Ok(false);
        }

        let ssh_handle = session.handle();
        let gateway = Arc::clone(&self.gateway);
        let state = Arc::clone(&self.state);
        let peer = self.peer;
        let connected_address = address.to_string();
        let key = (connected_address.clone(), port);
        let key_for_task = key.clone();
        let id_for_message = id.clone();
        let (abort_tx, abort_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            let (exec, env) = await_params(&state).await;
            let params = parse_params(exec.as_deref(), &env, &grant);

            match apply_takeover(
                &gateway.providers,
                &gateway.secret_owners,
                &id,
                &grant.identity,
                "tcp-secret-id",
            ) {
                TakeoverOutcome::Reject(reason) => {
                    state.queue_message(format!("bore ssh-gateway: {reason}"));
                    return;
                }
                TakeoverOutcome::Proceed => {}
            }

            let opener = SshOpener::new(ssh_handle.clone(), connected_address, port);
            let pool = Arc::new(CarrierPool::new(mux::LinkOpener::Ssh(Arc::new(opener))));
            match gateway.providers.entry(id.clone()) {
                Entry::Occupied(_) => {
                    state.queue_message(format!("tcp-secret-id '{id}' already in use"));
                    return;
                }
                Entry::Vacant(slot) => {
                    slot.insert(Arc::clone(&pool));
                }
            }
            let Ok(abort) = abort_rx.await else {
                gateway.providers.remove(&id);
                return;
            };
            let token = next_forward_token();
            gateway.secret_owners.insert(
                id.clone(),
                ForwardOwner {
                    identity: grant.identity.clone(),
                    abort,
                    handle: ssh_handle.clone(),
                    conn: Arc::downgrade(&state),
                    key: key_for_task,
                    token,
                },
            );
            let _guard = SecretSshGuard {
                registry: gateway.providers.clone(),
                owners: Arc::clone(&gateway.secret_owners),
                id: id.clone(),
                pool: Arc::clone(&pool),
                token,
            };

            let registration = gateway.admin.register(NewEntry {
                role: Role::SecretProvider,
                peer,
                secret_id: Some(id.clone()),
                public_port: None,
                notes: params.notes.clone(),
                basic_auth: false,
                https: false,
                force_https: false,
                carriers: 1,
                auto_reconnect: false,
                webserver_log: false,
                udp: false,
                vpn_relay_only: false,
                vpn_pin_mtu: false,
                vpn_mtu: None,
                vpn_forward_accept: false,
                vpn_nat_masquerade: false,
                vpn_route_policy: None,
                vpn_advertised: vec![],
                vpn_nat_udp_port: None,
                local_proxy_port: None,
                local_host: None,
                local_port: None,
                nat_udp_preferred_port: None,
                nat_udp_release_timeout: None,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                max_conns: None,
                transport: Transport::Ssh,
                identity: Some(grant.identity.clone()),
            });

            for line in secret_provider_info_banner(&id, &grant.identity, params.notes.as_deref()) {
                state.deliver(&ssh_handle, line).await;
            }

            // Nothing left to do but stay alive: native/SSH consumers reach
            // `pool` through the registry lookup in `secret::relay`/
            // `channel_open_direct_tcpip`. This task's only remaining purpose
            // is to hold `_guard`/`pool`/`registration` until aborted (I-3).
            //
            // `drop(state)` is load-bearing — see the matching comment in
            // `tcpip_forward_vhost`: without it, this task's own
            // `Arc<ConnState>` clone survives for as long as this `pending()`
            // future exists (forever), so `ConnState`'s refcount never
            // reaches zero and `Drop for ConnState` (which would abort this
            // very task) never runs on an ungraceful connection death.
            drop(state);
            let _pool = pool;
            let _registration = registration;
            std::future::pending::<()>().await;
        });
        // See the matching comment in `tcpip_forward_vhost`: handed via a
        // oneshot because a task cannot learn its own `AbortHandle` from
        // inside itself.
        let _ = abort_tx.send(task.abort_handle());
        self.state
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, task);

        self.state
            .queue_message(format!("secret tunnel requested: {id_for_message}"));
        Ok(true)
    }

    /// The (session, id)-scoped admin entry for a secret id this session
    /// consumes via `direct-tcpip`, creating it on first use (D11).
    /// Returns this session's `ConsumerEntry` for `id`, creating it (and its
    /// admin row) lazily on the first `direct-tcpip` channel for that id. The
    /// `bool` is `true` only on that first call — used to fire the "attached
    /// to secret" info banner exactly once per session, not once per
    /// proxied connection.
    fn consumer_entry(&self, id: &str) -> (Arc<ConsumerEntry>, bool) {
        let mut consumers = self
            .state
            .secret_consumers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = consumers.get(id) {
            return (Arc::clone(entry), false);
        }
        let grant = self.grant();
        let registration = self.gateway.admin.register(NewEntry {
            role: Role::SecretConsumer,
            peer: self.peer,
            secret_id: Some(id.to_string()),
            public_port: None,
            notes: grant.notes.clone(),
            basic_auth: false,
            https: false,
            force_https: false,
            carriers: 0,
            auto_reconnect: false,
            webserver_log: false,
            udp: false,
            vpn_relay_only: false,
            vpn_pin_mtu: false,
            vpn_mtu: None,
            vpn_forward_accept: false,
            vpn_nat_masquerade: false,
            vpn_route_policy: None,
            vpn_advertised: vec![],
            vpn_nat_udp_port: None,
            local_proxy_port: None,
            local_host: None,
            local_port: None,
            nat_udp_preferred_port: None,
            nat_udp_release_timeout: None,
            stun_server: None,
            upnp: false,
            try_port_prediction: false,
            max_conns: None,
            transport: Transport::Ssh,
            identity: Some(grant.identity.clone()),
        });
        let active = registration.active();
        let (relay_tx, relay_rx) = registration.relay_bytes();
        let entry = Arc::new(ConsumerEntry {
            _registration: registration,
            active,
            relay_tx,
            relay_rx,
        });
        consumers.insert(id.to_string(), Arc::clone(&entry));
        (entry, true)
    }
}

/// State shared between a [`GatewayHandler`]'s callbacks — which all run,
/// one at a time, on the connection's single sequential dispatch task
/// (russh invariant, confirmed empirically: SPIKE_FINDINGS.md) — and the
/// per-forward tasks spawned for each granted `tcpip-forward`. Every field
/// needs interior mutability because it crosses that task boundary; the
/// dispatch task never holds a lock across an `.await`.
#[derive(Default)]
struct ConnState {
    /// `env` requests seen so far on this connection's (at most one, in
    /// practice) session channel.
    env: Mutex<Vec<(String, String)>>,
    /// The `exec` command line, once `exec_request` fires. A [`watch`]
    /// channel (rather than [`tokio::sync::Notify`]) so a finalize task that
    /// starts waiting *after* `exec_request` already fired still observes it
    /// immediately — `watch::Receiver::changed` is keyed off a version
    /// counter, not a fire-and-forget wakeup, so there is no missed-update
    /// race to reason about.
    exec: watch::Sender<Option<String>>,
    /// Success/diagnostic lines queued until a session channel exists to
    /// write them to. `tcpip-forward` is always processed before any channel
    /// exists (confirmed empirically — SPIKE_FINDINGS.md), so text meant for
    /// the user has nowhere to go until (if ever) `channel_open_session`
    /// fires. A true `-N` client (`SessionType=none`) never opens a channel
    /// at all — confirmed empirically, not just per the RFC text — so these
    /// are simply never delivered for `-N`; OpenSSH's own client still
    /// prints "Allocated port N for remote forward" in that case. Once
    /// `channel_open_session` fires, `session_channel` below is set and
    /// every later line bypasses this queue entirely (delivered immediately
    /// via `ConnState::deliver`) — this queue only matters for the brief
    /// window before the channel opens.
    pending_messages: Mutex<Vec<String>>,
    /// The session channel this connection opened, if any (set once by
    /// `channel_open_session`). Lets a forward task that finishes its own
    /// registration *after* the channel already opened (common — vhost/
    /// secret/public registration crosses `PARAMS_GRACE`) push a line
    /// directly via the cloned `Handle::data`, which works outside the
    /// per-call `Session` reference entirely (see `ConnState::deliver`).
    session_channel: Mutex<Option<ChannelId>>,
    /// Live per-forward tasks, keyed by `(bind_address, allocated_port)`, so
    /// `cancel_tcpip_forward` can abort exactly the right one without
    /// disturbing sibling forwards on the same connection (I-3).
    forwards: Mutex<HashMap<(String, u16), JoinHandle<()>>>,
    /// Secret-consumer admin bookkeeping (D11), keyed by secret id: created
    /// lazily on the first `direct-tcpip` channel for that id, reused by
    /// every later channel on the same id from this SSH session so there is
    /// ONE admin row per (session, id) regardless of how many concurrent
    /// proxied connections are open — never one row per channel (BUG-S1
    /// parity, see `secret::serve_consumer`'s `carrier` handling). Dropped
    /// (removing the admin rows) along with the rest of `ConnState`.
    secret_consumers: Mutex<HashMap<String, Arc<ConsumerEntry>>>,
}

/// One SSH session's live admin entry for a secret id it consumes, shared by
/// every concurrent `direct-tcpip` channel open for that id.
struct ConsumerEntry {
    /// Holds the admin row alive; dropped (removing the row) with this entry.
    _registration: Registration,
    /// Incremented/decremented per live channel via [`ActiveGuard`].
    active: Arc<AtomicUsize>,
    relay_tx: Arc<AtomicU64>,
    relay_rx: Arc<AtomicU64>,
}

impl ConnState {
    /// Queue a line for delivery the next time (if ever) a session channel
    /// is open. See the `pending_messages` field doc for why this can't
    /// simply write to a channel directly. Prefer [`ConnState::deliver`] from
    /// an async context with a `Handle` in hand — this is the fallback used
    /// before a channel exists, or from the few sync `Handler` callbacks
    /// that reject a request before any channel/handle is relevant.
    fn queue_message(&self, line: String) {
        self.pending_messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(line);
    }

    /// Take every message queued so far, in order, leaving the queue empty.
    fn drain_messages(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .pending_messages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// Records the session channel this connection opened, so later calls to
    /// [`ConnState::deliver`] can write to it directly instead of queueing.
    fn set_session_channel(&self, id: ChannelId) {
        *self
            .session_channel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id);
    }

    fn session_channel(&self) -> Option<ChannelId> {
        *self
            .session_channel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether this connection has (or ever had) at least one live forward
    /// registered (`-R vhost/secret-provider/public`). Used by
    /// `shell_request` as a best-effort signal for whether to print the
    /// "nothing established yet" hint — NOT to decide whether to keep the
    /// channel open (that's now unconditional, see `shell_request`'s doc).
    fn has_forwards(&self) -> bool {
        !self
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// Whether this connection has attached to at least one secret-consumer
    /// id (`-L <port>:secret/<id>:1`) yet. Unlike `has_forwards`, this is
    /// necessarily a lagging signal — a consumer only registers here once
    /// its FIRST proxied connection actually opens a `direct-tcpip` channel,
    /// which can be well after the session channel's shell request fires.
    fn has_secret_consumers(&self) -> bool {
        !self
            .secret_consumers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// Delivers one line of info/diagnostic text to the user, right now if
    /// the session channel is already open (via `Handle::data`, which works
    /// from any task holding a cloned `Handle` — no `Session`/dispatch-loop
    /// access needed), or queued for the channel's eventual first open
    /// otherwise. This is what lets a forward task (vhost/public/secret)
    /// report its *final*, fully-resolved state — which can finish seconds
    /// after the channel opened, well past `channel_open_session`'s one-shot
    /// drain — rather than only ever seeing whatever was queued at the
    /// instant the channel appeared.
    async fn deliver(&self, handle: &Handle, line: String) {
        if let Some(id) = self.session_channel() {
            let _ = handle.data(id, format!("{line}\r\n").into_bytes()).await;
        } else {
            self.queue_message(line);
        }
    }
}

impl Drop for ConnState {
    /// Aborts every forward still registered when the last handle to this
    /// connection's state disappears (the `GatewayHandler` itself, plus any
    /// still-running `tcpip_forward` finalize task — see `exec`'s doc for why
    /// one can outlive a fast-closing connection). Aborting the finalize/
    /// accept-loop task drops its held `Registration`, which removes the
    /// admin entry (I-3); already-proxied connections on that forward finish
    /// on their own, matching how dropping a native tunnel's `Registration`
    /// never kills in-flight proxied connections either.
    fn drop(&mut self) {
        let forwards = std::mem::take(
            &mut *self
                .forwards
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, task) in forwards {
            task.abort();
        }
    }
}

impl Handler for GatewayHandler {
    type Error = russh::Error;

    /// Sends the operator-configured `--ssh-banner` (`SSH_MSG_USERAUTH_BANNER`)
    /// before authentication succeeds — russh calls this once auth starts. A
    /// `None` banner (the default) sends nothing, exactly as before.
    async fn authentication_banner(&mut self) -> Result<Option<String>, Self::Error> {
        Ok(self.gateway.config.banner.clone())
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        let Some(keys) = &self.gateway.keys else {
            return Ok(Auth::reject());
        };
        match keys.check(public_key) {
            Some(grant) => {
                self.grant = Some(grant);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        let Some(passwords) = &self.gateway.passwords else {
            return Ok(Auth::reject());
        };
        match passwords.check(password).await {
            Some(label) => {
                self.grant = Some(KeyGrant {
                    identity: label,
                    permit: None,
                    max_conns: None,
                    notes: None,
                });
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let channel_id = channel.id();
        // Order matters: record the channel BEFORE draining, so a forward
        // task's concurrent `ConnState::deliver` call either lands in this
        // drain (queued-before-set) or pushes directly via `Handle::data`
        // (set-before-its-check) — never silently lost in between.
        self.state.set_session_channel(channel_id);
        for line in self.state.drain_messages() {
            session.data(channel_id, format!("{line}\r\n").into_bytes())?;
        }
        Ok(())
    }

    /// `-L <local>:<id|secret/id>:0` (an OpenSSH client's local forwarding):
    /// looks up the secret provider's pool and splices this channel to a
    /// freshly opened provider substream with the same carrier-failover
    /// semantics as the native consumer relay (`secret::open_with_failover`,
    /// BUG-S4). Admin bookkeeping follows D11: one `Role::SecretConsumer` row
    /// per (session, id), created lazily, `active` incremented per live
    /// channel — never one row per channel.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let id = match parse_direct_tcpip_dest(host_to_connect, port_to_connect) {
            Ok(id) => id,
            Err(err) => {
                self.state.queue_message(format!("bore ssh-gateway: {err}"));
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            }
        };

        let Some(pool) = self
            .gateway
            .providers
            .get(&id)
            .map(|entry| Arc::clone(entry.value()))
        else {
            self.state
                .queue_message(format!("bore ssh-gateway: unknown secret id '{id}'"));
            reply.reject(ChannelOpenFailure::ConnectFailed).await;
            return Ok(());
        };

        // Admin bookkeeping (sync, cheap) stays on the dispatch loop; the
        // provider substream open (a real round trip) is deliberately deferred
        // into the spawned relay task below. Awaiting `open_with_failover`
        // here would block russh's single sequential handler-dispatch loop —
        // stalling this consumer session's keepalives and every other channel
        // on it — for as long as a wedged provider takes to fail (up to the
        // provider's own 60 s reaper). We accept the channel unconditionally
        // (a valid, registered id) and let the task close it on open failure,
        // which an `ssh -L` client observes as an immediately-closed forwarded
        // connection — the correct signal, without holding the dispatch loop.
        let (consumer_entry, is_new_consumer) = self.consumer_entry(&id);
        let total_rx_bytes = Arc::clone(&self.gateway.total_rx_bytes);
        let total_tx_bytes = Arc::clone(&self.gateway.total_tx_bytes);

        if is_new_consumer {
            let grant = self.grant();
            let provider_identity = self
                .gateway
                .secret_owners
                .get(&id)
                .map(|owner| owner.identity.clone());
            let lines = secret_consumer_info_banner(
                &id,
                &grant.identity,
                grant.notes.as_deref(),
                provider_identity.as_deref(),
            );
            let handle = session.handle();
            let state = Arc::clone(&self.state);
            tokio::spawn(async move {
                for line in lines {
                    state.deliver(&handle, line).await;
                }
            });
        }

        reply.accept().await;
        tokio::spawn(async move {
            let _active_guard = ActiveGuard::new(Arc::clone(&consumer_entry.active));
            let provider = match timeout(
                SSH_DIRECT_OPEN_TIMEOUT,
                secret::open_with_failover(&pool, &id),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    warn!(%id, %err, "ssh-gateway: secret consumer could not reach provider; closing channel");
                    return;
                }
                Err(_) => {
                    warn!(%id, timeout_secs = SSH_DIRECT_OPEN_TIMEOUT.as_secs(), "ssh-gateway: secret provider open timed out; closing channel");
                    return;
                }
            };
            let ssh_stream = channel.into_stream();
            let mut provider = provider;
            let mut counted = CountingStream::new(
                ssh_stream,
                Arc::clone(&consumer_entry.relay_rx),
                Arc::clone(&consumer_entry.relay_tx),
                total_rx_bytes,
                total_tx_bytes,
            );
            let buf = proxy_buffer_size();
            if let Err(err) =
                tokio::io::copy_bidirectional_with_sizes(&mut counted, &mut provider, buf, buf)
                    .await
            {
                trace!(%id, %err, "ssh-gateway: secret consumer channel closed");
            }
        });
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        // A client that omits `-N` and passes no `exec` command (the common,
        // ordinary way to invoke `-R`/`-L`) still gets OpenSSH's *default*
        // behavior: request an interactive shell on the session channel.
        // This is NOT the client asking for a real shell — it is the
        // client's default absent any override — so denying it by closing
        // the channel with a nonzero exit status is the wrong move: OpenSSH
        // treats "my primary session's command exited nonzero" as reason to
        // disconnect the WHOLE connection, tearing down every active
        // `-R`/`-L` forward on it too (BUG — a real report: a bare `ssh -p
        // 443 -R secret/id:0:localhost:8080 host` closed immediately with
        // "interactive shells are not supported" and killed the just-granted
        // forward). Fixed by NEVER closing this channel with a nonzero exit
        // from here — it is held open instead, silently, exactly like a
        // `-N` session's channel would sit, so `ConnState::deliver` can use
        // it as the info/keepalive channel for the tunnel info banner (§7).
        //
        // This intentionally does NOT special-case "zero forwards" (a
        // genuine interactive-login mistake) into a hard rejection anymore,
        // even though an earlier version of this fix tried to: a secret
        // *consumer* (`-L <port>:secret/<id>:1`) has NO equivalent of
        // `tcpip-forward` to announce itself in advance — the server learns
        // about it only when a real proxied connection arrives on the
        // client's local port, which can be arbitrarily later than this
        // shell request. Closing the channel for "no forward YET" would
        // reintroduce the exact same bug for any consumer whose first
        // connection hasn't arrived yet. A best-effort, INFORMATIONAL (never
        // channel-closing) hint is printed instead when nothing is known
        // yet, so a genuine mistake still gets a clear answer without ever
        // risking a legitimate tunnel.
        if !self.state.has_forwards() && !self.state.has_secret_consumers() {
            session.data(channel, NO_FORWARD_YET_MESSAGE.as_bytes().to_vec())?;
        }
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.state
            .env
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((variable_name.to_string(), variable_value.to_string()));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let exec = String::from_utf8_lossy(data).into_owned();
        let grant = self.grant();
        let env = self
            .state
            .env
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let params = parse_params(Some(&exec), &env, &grant);
        for warning in &params.warnings {
            session.data(
                channel,
                format!("bore ssh-gateway: {warning}\r\n").into_bytes(),
            )?;
        }
        let _ = self.state.exec.send(Some(exec));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let grant = self.grant();
        let spec = match parse_forward_spec(address, *port) {
            Ok(spec) => spec,
            Err(err) => {
                self.state.queue_message(format!("bore ssh-gateway: {err}"));
                return Ok(false);
            }
        };
        let requested_port = match spec {
            ForwardSpec::Public { port } => port,
            ForwardSpec::Vhost { label } => {
                // RFC 4254 7.1: a `tcpip-forward` requesting the dynamic port 0
                // MUST get the allocated port echoed back in the SUCCESS reply,
                // or OpenSSH treats the reply as malformed and disconnects right
                // after receiving it. Vhost forwards have no real listening
                // port, so synthesize a fixed non-zero placeholder; the same
                // value is reused as `connected_port` in every later
                // `channel_open_forwarded_tcpip` for this label so the client's
                // forward-table lookup (keyed by address+port) still matches.
                let port16 = match u16::try_from(*port) {
                    Ok(0) | Err(_) => 1,
                    Ok(p) => p,
                };
                *port = u32::from(port16);
                return self
                    .tcpip_forward_vhost(address, port16, label, grant, session)
                    .await;
            }
            ForwardSpec::SecretProvider { id } => {
                // Same RFC 4254 7.1 echo-back rule as the vhost branch above:
                // a secret provider forward has no real listening port either.
                let port16 = match u16::try_from(*port) {
                    Ok(0) | Err(_) => 1,
                    Ok(p) => p,
                };
                *port = u32::from(port16);
                return self
                    .tcpip_forward_secret(address, port16, id, grant, session)
                    .await;
            }
        };

        let listener = match bind_permitted(
            self.gateway.bind_tunnels,
            &self.gateway.port_range,
            &grant,
            requested_port,
        )
        .await
        {
            Ok(listener) => listener,
            Err(err) => {
                self.state.queue_message(format!("bore ssh-gateway: {err}"));
                return Ok(false);
            }
        };
        let bound_port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => {
                self.state.queue_message(format!("bore ssh-gateway: {err}"));
                return Ok(false);
            }
        };
        *port = u32::from(bound_port);
        self.state.queue_message(format!(
            "public tunnel ready: {}:{bound_port}",
            self.gateway.bind_tunnels
        ));

        let ssh_handle = session.handle();
        let gateway = Arc::clone(&self.gateway);
        let state = Arc::clone(&self.state);
        let peer = self.peer;
        let connected_address = address.to_string();
        let key = (connected_address.clone(), bound_port);

        let task = tokio::spawn(async move {
            let (exec, env) = await_params(&state).await;
            let mut params = parse_params(exec.as_deref(), &env, &grant);
            // Mirror the native client's check (`server.rs::serve_tunnel`):
            // `https=on` without a server certificate can never work — warn
            // and drop it (I-2) rather than silently serving plain TCP or
            // rejecting the whole tunnel the client already got a SUCCESS
            // reply for.
            if params.https && gateway.tls.is_none() {
                state.queue_message(
                    "bore ssh-gateway: https: server has no TLS certificate configured; \
                     serving this tunnel as plain TCP"
                        .to_string(),
                );
                params.https = false;
                params.force_https = false;
            }
            // `drop(state)` is load-bearing — same reference-cycle bug as
            // `tcpip_forward_vhost`/`tcpip_forward_secret` (see their matching
            // comment): this task's own `Arc<ConnState>` clone (captured above
            // for `await_params`/`queue_message`) would otherwise stay alive
            // for as long as `run_public_forward`'s accept loop runs — i.e.
            // forever — so `ConnState`'s refcount never reaches zero and
            // `Drop for ConnState` (which aborts every task in
            // `self.forwards`, INCLUDING this one, freeing the bound
            // `listener`/port and the admin entry) never runs on an
            // ungraceful connection death (e.g. Ctrl+C on the client, which
            // closes the TCP connection without a `cancel-tcpip-forward`).
            // This was missed when 523fa32 fixed the identical bug for the
            // vhost/secret finalize tasks — this public-tunnel path has the
            // same "captured early, never used again, long-lived tail
            // future" shape and was never patched.
            let effective_max_conns = params.max_conns.unwrap_or(DEFAULT_MAX_CONNS);
            for line in public_info_banner(PublicBannerInfo {
                bound_port,
                identity: &grant.identity,
                notes: params.notes.as_deref(),
                grant_max_conns: grant.max_conns,
                effective_max_conns,
                basic_auth: params.basic_auth.is_some(),
                https: params.https,
                force_https: params.force_https,
                webserver_log: params.webserver_log,
            }) {
                state.deliver(&ssh_handle, line).await;
            }
            drop(state);
            let tunnel_opts = crate::shared::TunnelOptions {
                https: params.https,
                force_https: params.force_https,
                basic_auth: params.basic_auth.clone(),
                ..Default::default()
            };
            let registration = gateway.admin.register(NewEntry {
                role: Role::Public,
                peer,
                secret_id: None,
                public_port: Some(bound_port),
                notes: params.notes.clone(),
                basic_auth: params.basic_auth.is_some(),
                https: params.https,
                force_https: params.force_https,
                carriers: 1,
                auto_reconnect: false,
                webserver_log: params.webserver_log,
                udp: false,
                vpn_relay_only: false,
                vpn_pin_mtu: false,
                vpn_mtu: None,
                vpn_forward_accept: false,
                vpn_nat_masquerade: false,
                vpn_route_policy: None,
                vpn_advertised: vec![],
                vpn_nat_udp_port: None,
                local_proxy_port: None,
                local_host: None,
                local_port: None,
                nat_udp_preferred_port: None,
                nat_udp_release_timeout: None,
                stun_server: None,
                upnp: false,
                try_port_prediction: false,
                max_conns: Some(effective_max_conns),
                transport: Transport::Ssh,
                identity: Some(grant.identity.clone()),
            });
            let active = registration.active();
            let (relay_tx, relay_rx) = registration.relay_bytes();
            run_public_forward(
                listener,
                ssh_handle,
                connected_address,
                bound_port,
                Arc::clone(&gateway.conn_permits),
                Arc::new(Semaphore::new(effective_max_conns)),
                active,
                relay_rx,
                relay_tx,
                Arc::clone(&gateway.total_rx_bytes),
                Arc::clone(&gateway.total_tx_bytes),
                registration,
                tunnel_opts,
                gateway.tls.clone(),
                gateway.bind_domain.clone(),
            )
            .await;
        });
        self.state
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, task);

        Ok(true)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let Ok(port16) = u16::try_from(port) else {
            return Ok(false);
        };
        let mut forwards = self
            .state
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target = cancel_target(forwards.keys(), address, port16);
        match target.and_then(|key| forwards.remove(&key)) {
            Some(task) => {
                task.abort();
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

/// Chooses which `forwards` entry a `cancel-tcpip-forward(address, port)`
/// targets. A public forward is keyed by its real `(bind_address,
/// allocated_port)` and matches EXACTLY. Vhost/secret forwards, however, were
/// registered under a SYNTHESIZED placeholder port (the client requested the
/// dynamic port 0 and RFC 4254 §7.1 forces us to echo back a nonzero value),
/// and depending on OpenSSH version a `cancel-tcpip-forward` may carry either
/// the client's ORIGINAL port (`0`) or the echoed placeholder — so an exact
/// `(address, port)` match can miss and leave the forward alive until the whole
/// session tears down (found in the ssh-gateway bug hunt). For a vhost/secret
/// address the `(connection, address)` pair is unique, so after an exact-match
/// miss we fall back to the entry sharing that address string, port-agnostic.
/// Public specs never take the fallback (multiple public forwards can share a
/// bind address on different ports — matching by address alone could abort the
/// wrong one).
fn cancel_target<'a, I>(keys: I, address: &str, port: u16) -> Option<(String, u16)>
where
    I: Iterator<Item = &'a (String, u16)>,
{
    let mut addr_match = None;
    for key in keys {
        if key.0 == address {
            if key.1 == port {
                return Some(key.clone()); // exact match always wins
            }
            addr_match.get_or_insert_with(|| key.clone());
        }
    }
    match parse_forward_spec(address, u32::from(port)) {
        Ok(ForwardSpec::Vhost { .. } | ForwardSpec::SecretProvider { .. }) => addr_match,
        _ => None,
    }
}

/// Parses one `permit=` list entry into the port range it grants. Only the
/// `"port/<n>"` and `"port/<a>-<b>"` shapes are port rules; anything else
/// (a future vhost/secret label rule) is simply not one, so it is filtered
/// out rather than rejected here — `permitted_port_ranges` callers only ever
/// care about port rules because Phase 4.3 only forwards `Public` specs.
fn parse_port_rule(entry: &str) -> Option<RangeInclusive<u16>> {
    let rest = entry.strip_prefix("port/")?;
    match rest.split_once('-') {
        Some((a, b)) => {
            let a: u16 = a.parse().ok()?;
            let b: u16 = b.parse().ok()?;
            (a <= b).then_some(a..=b)
        }
        None => {
            let n: u16 = rest.parse().ok()?;
            Some(n..=n)
        }
    }
}

/// The port ranges `grant` permits. `permit: None` means unrestricted (the
/// whole server `port_range`); otherwise each `permit` entry that parses as
/// a port rule contributes its own range, before any intersection with the
/// server's own range.
fn permitted_port_ranges(
    grant: &KeyGrant,
    port_range: &RangeInclusive<u16>,
) -> Vec<RangeInclusive<u16>> {
    match &grant.permit {
        None => vec![port_range.clone()],
        Some(entries) => entries.iter().filter_map(|e| parse_port_rule(e)).collect(),
    }
}

/// Overlap of two inclusive ranges, or `None` if they do not overlap.
fn intersect(a: &RangeInclusive<u16>, b: &RangeInclusive<u16>) -> Option<RangeInclusive<u16>> {
    let start = (*a.start()).max(*b.start());
    let end = (*a.end()).min(*b.end());
    (start <= end).then_some(start..=end)
}

/// Whether `grant.permit` allows a `<prefix><label>`-style forward for
/// `label` (`prefix` is `"vhost/"` or `"secret/"`). `permit: None` means
/// unrestricted, same as [`permitted_port_ranges`]. Otherwise requires at
/// least one `<prefix><glob>` entry whose glob matches `label`.
fn permit_allows(grant: &KeyGrant, prefix: &str, label: &str) -> bool {
    match &grant.permit {
        None => true,
        Some(entries) => entries
            .iter()
            .filter_map(|e| e.strip_prefix(prefix))
            .any(|glob| glob_match(glob, label)),
    }
}

/// Minimal glob match: `*` matches zero or more of any character, every
/// other character matches literally. No `?`, no character classes — the
/// `permit=` grammar only needs prefix/suffix/contains wildcards.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => inner(&p[1..], t) || (!t.is_empty() && inner(p, &t[1..])),
            Some(c) => t.first() == Some(c) && inner(&p[1..], &t[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Removes an SSH-registered vhost subdomain from the registry when the
/// `tcpip-forward` task ends (aborted by `cancel_tcpip_forward`, evicted by a
/// same-identity takeover, or the whole connection tearing down via `Drop for
/// ConnState`). Mirrors `vhost`'s own private `Deregister` guard, kept
/// separate since that one isn't `pub`.
///
/// `entry`/`token` identify exactly the registration this guard owns: a
/// takeover (Phase 5.4) can replace `registry[label]`/`owners[label]` with a
/// NEW registration before this (evicted) guard's `Drop` actually runs (task
/// abort is asynchronous — the future is only dropped at its next poll), so
/// an unconditional `remove(&label)` here would delete the WINNER's fresh
/// entry instead of the loser's own. `remove_if` + an identity check makes
/// the removal a no-op once this guard's registration is no longer the one
/// installed.
struct VhostSshGuard {
    registry: VhostRegistry,
    owners: Arc<DashMap<String, ForwardOwner>>,
    label: String,
    entry: Arc<VhostEntry>,
    token: u64,
}

impl Drop for VhostSshGuard {
    fn drop(&mut self) {
        self.registry
            .remove_if(&self.label, |_, v| Arc::ptr_eq(v, &self.entry));
        self.owners
            .remove_if(&self.label, |_, o| o.token == self.token);
    }
}

/// Removes an SSH-backed secret provider's pool from the registry when its
/// forward task ends, mirroring [`VhostSshGuard`] (I-3) including the
/// identity-checked `remove_if` (Phase 5.4 takeover safety).
struct SecretSshGuard {
    registry: secret::Registry,
    owners: Arc<DashMap<String, ForwardOwner>>,
    id: String,
    pool: Arc<CarrierPool>,
    token: u64,
}

impl Drop for SecretSshGuard {
    fn drop(&mut self) {
        self.registry
            .remove_if(&self.id, |_, v| Arc::ptr_eq(v, &self.pool));
        self.owners
            .remove_if(&self.id, |_, o| o.token == self.token);
    }
}

/// Monotonic source for [`ForwardOwner::token`]/the matching guard `token`
/// field — cheaper than comparing `Arc` pointers across the two different
/// concrete types (`VhostEntry`/`CarrierPool`) a single owners map must track.
static NEXT_FORWARD_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_forward_token() -> u64 {
    NEXT_FORWARD_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Tracks which SSH session owns each SSH-registered vhost label / secret id
/// (Phase 5.4, D2/I-5). Entries exist ONLY for SSH-registered names — a name
/// already held by a *native* tunnel has no entry here, so a collision
/// against one is always rejected, never evicted (SSH identities and the
/// HMAC secret are different trust domains). Inserted (overwriting any stale
/// leftover) at successful registration; removed by the owning
/// `VhostSshGuard`/`SecretSshGuard` on that registration's own teardown, or
/// by [`apply_takeover`] when a same-identity newcomer evicts it.
struct ForwardOwner {
    /// Identity that registered this name (`KeyGrant::identity`).
    identity: String,
    /// Cancels the incumbent's finalize task on eviction — dropping its
    /// `VhostSshGuard`/`SecretSshGuard` and admin `Registration` (same
    /// teardown `cancel_tcpip_forward` uses).
    abort: AbortHandle,
    /// The incumbent's own SSH session, used to disconnect it on eviction
    /// (D2 step 2).
    handle: Handle,
    /// The incumbent connection's shared state — Weak so an evicted-but-not-
    /// yet-dropped `ConnState` is never kept alive by this bookkeeping map
    /// (that would delay its own `Drop`-driven teardown of every OTHER
    /// forward on that same connection).
    conn: Weak<ConnState>,
    /// The `(bind_address, port)` key this forward is registered under in
    /// its own connection's `forwards` map — used to check whether evicting
    /// it leaves that connection with zero remaining forwards.
    key: (String, u16),
    /// Matches the owning `VhostSshGuard`/`SecretSshGuard`'s own `token`.
    token: u64,
}

/// Pure decision table (D2/I-5) for a name collision: `incumbent` is
/// `Some((identity, is_ssh))` when the name is already taken, `None` when
/// free. `is_ssh == false` means a *native* tunnel holds it (no identity to
/// compare — always rejected). Both identities must be non-empty for a match
/// to count, defense-in-depth against an empty/placeholder identity ever
/// matching another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TakeoverDecision {
    /// No incumbent: register the newcomer.
    Insert,
    /// Incumbent is SSH-owned by the SAME identity: evict it, then register.
    Evict,
    /// Incumbent is native, or SSH-owned by a DIFFERENT identity: refuse.
    Reject,
}

fn takeover_decision(incumbent: Option<(&str, bool)>, newcomer: &str) -> TakeoverDecision {
    match incumbent {
        None => TakeoverDecision::Insert,
        Some((_, false)) => TakeoverDecision::Reject,
        Some((identity, true)) => {
            if !identity.is_empty() && !newcomer.is_empty() && identity == newcomer {
                TakeoverDecision::Evict
            } else {
                TakeoverDecision::Reject
            }
        }
    }
}

/// Outcome of [`apply_takeover`]: either the caller may proceed to insert its
/// own entry (the name was free, or an eviction just freed it), or it must
/// give up with the given user-facing reason.
enum TakeoverOutcome {
    Proceed,
    Reject(String),
}

/// Non-mutating peek at what [`apply_takeover`] would decide for `label`
/// right now. Used to reply to the SSH `tcpip-forward` global request
/// SYNCHRONOUSLY with `Ok(false)` on a `Reject` — `ExitOnForwardFailure=yes`
/// (T-SSH-TAKE2) only works if the rejection reaches the client as THIS
/// request's own `REQUEST_FAILURE`, not a message queued for a possibly
/// never-opened later channel. The full registration (including the actual
/// eviction) still happens inside the spawned finalize task via
/// `apply_takeover` itself, which re-decides authoritatively at the point it
/// actually mutates the registry — this peek is optimistic, not a
/// reservation, so a residual race between two brand-new registrations
/// landing in the same instant is still possible (same pre-existing,
/// accepted scope as the registry's own vacant-insert race).
fn peek_takeover<T>(
    registry: &DashMap<String, Arc<T>>,
    owners: &DashMap<String, ForwardOwner>,
    label: &str,
    newcomer_identity: &str,
) -> TakeoverDecision {
    let incumbent = registry.get(label).map(|_| match owners.get(label) {
        Some(owner) => (owner.identity.clone(), true),
        None => (String::new(), false),
    });
    takeover_decision(
        incumbent
            .as_ref()
            .map(|(identity, is_ssh)| (identity.as_str(), *is_ssh)),
        newcomer_identity,
    )
}

/// Applies the takeover decision table to a name collision on `registry`
/// (Phase 5.4). Holds `registry`'s per-key shard lock (the `Entry` API) for
/// the full check-and-decide step so a second concurrent registration for
/// the same `label` cannot interleave between "decide" and "remove" — but
/// the actual incumbent teardown (abort + maybe-disconnect) intentionally
/// runs AFTER that lock is dropped (two-step, per the plan's race-safety
/// note): it can take a moment and must never block other labels sharing
/// this DashMap's shard.
fn apply_takeover<T>(
    registry: &DashMap<String, Arc<T>>,
    owners: &DashMap<String, ForwardOwner>,
    label: &str,
    newcomer_identity: &str,
    kind: &str,
) -> TakeoverOutcome {
    let evicted = match registry.entry(label.to_string()) {
        Entry::Vacant(_) => return TakeoverOutcome::Proceed,
        Entry::Occupied(entry) => {
            let incumbent = match owners.get(label) {
                Some(owner) => (owner.identity.clone(), true),
                None => (String::new(), false),
            };
            match takeover_decision(Some((incumbent.0.as_str(), incumbent.1)), newcomer_identity) {
                TakeoverDecision::Insert => unreachable!("Some(_) incumbent never yields Insert"),
                TakeoverDecision::Reject => {
                    return TakeoverOutcome::Reject(format!("{kind} '{label}' already in use"))
                }
                TakeoverDecision::Evict => {
                    entry.remove();
                    owners.remove(label).map(|(_, owner)| owner)
                }
            }
        }
    };

    if let Some(owner) = evicted {
        owner.abort.abort();
        if let Some(conn) = owner.conn.upgrade() {
            let remaining = {
                let mut forwards = conn
                    .forwards
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                forwards.remove(&owner.key);
                forwards.len()
            };
            if remaining == 0 {
                let handle = owner.handle.clone();
                tokio::spawn(async move {
                    let _ = handle
                        .disconnect(
                            Disconnect::ByApplication,
                            "evicted by newer session with same identity".to_string(),
                            String::new(),
                        )
                        .await;
                });
            } else {
                conn.queue_message(format!(
                    "bore ssh-gateway: {kind} '{label}' evicted by newer session with same identity"
                ));
            }
        }
    }
    TakeoverOutcome::Proceed
}

/// Binds a public-tunnel listener honoring both the server's `port_range`
/// and the connecting key's `permit=` port rules. A nonzero `requested_port`
/// is rejected outright (no socket touched) if it falls outside the
/// permitted intersection; `requested_port == 0` auto-assigns a random port
/// from that intersection, retrying like [`bind_public_listener`]'s own
/// auto-assign loop does.
async fn bind_permitted(
    bind_tunnels: IpAddr,
    port_range: &RangeInclusive<u16>,
    grant: &KeyGrant,
    requested_port: u16,
) -> Result<TcpListener, String> {
    let allowed: Vec<RangeInclusive<u16>> = permitted_port_ranges(grant, port_range)
        .iter()
        .filter_map(|r| intersect(r, port_range))
        .collect();
    if allowed.is_empty() {
        return Err(
            "this key's permit= list allows no ports in the server's port range".to_string(),
        );
    }

    if requested_port != 0 {
        if !allowed.iter().any(|r| r.contains(&requested_port)) {
            return Err(format!(
                "port {requested_port} is not permitted for this key"
            ));
        }
        return bind_public_listener(bind_tunnels, port_range, requested_port)
            .await
            .map_err(|err| err.to_string());
    }

    for _ in 0..150 {
        let range = &allowed[fastrand::usize(..allowed.len())];
        let candidate = fastrand::u16(range.clone());
        if let Ok(listener) = bind_public_listener(bind_tunnels, port_range, candidate).await {
            return Ok(listener);
        }
    }
    Err("failed to find an available port within the permitted range".to_string())
}

/// Waits up to [`PARAMS_GRACE`] for an `exec` request to arrive on this
/// connection, then returns whatever `exec` string (if any) and `env`
/// snapshot are available at that point. If `exec` already arrived before
/// this was even called (defensive — the spike findings say `tcpip-forward`
/// always precedes any channel request), it returns immediately.
async fn await_params(state: &ConnState) -> (Option<String>, Vec<(String, String)>) {
    let mut exec_rx = state.exec.subscribe();
    if exec_rx.borrow().is_none() {
        let _ = tokio::time::timeout(PARAMS_GRACE, exec_rx.changed()).await;
    }
    let exec = exec_rx.borrow().clone();
    let env = state
        .env
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    (exec, env)
}

/// Runs one public-tunnel forward's accept loop until the listener's task is
/// aborted (`cancel_tcpip_forward`, or the whole connection tearing down via
/// `Drop for ConnState`) or the SSH control channel is gone (opening a
/// `forwarded-tcpip` channel fails). Mirrors the native `Role::Public` accept
/// loop (`src/server.rs`), including edge/TLS termination and `force-https`
/// redirects (`https=on`/`force-https=on` params, §5.6) and basic-auth
/// gating (`basic-auth=` param) — minus weblog/direct-UDP, which the SSH
/// gateway does not support yet. There is no `STREAM_READY` anywhere on this
/// path (I-4). `registration` is held for the loop's entire lifetime so the
/// admin entry disappears (RAII) exactly when this task ends.
///
/// Edge handling AND the `forwarded-tcpip` channel-open are both awaited
/// **inline in the accept loop**, not inside the per-connection spawned
/// task — deliberately, unlike the native `Role::Public` loop (which spawns
/// eagerly because its substream-open is a synchronous `pool.pick()`, never
/// blocking). Here `channel_open_forwarded_tcpip` is a real round trip over
/// the single SSH control connection: once that connection has died, the
/// call can sit for a while rather than fail instantly. Spawning it per
/// connection let the loop keep accepting — and once the peer app (or, as in
/// `t_ssh_cancel1_session_close_frees_forwards`, a test polling the port
/// after the session died) opened connections faster than those channel-opens
/// resolved, the pile-up of concurrent requests over the one dead control
/// connection starved the gateway's own liveness detection, delaying
/// `Drop for ConnState`/the reaper far past their expected bound. Awaiting
/// both inline naturally throttles new accepts to the pace the SSH control
/// connection can actually service, which is what keeps that detection
/// prompt (regression caught by `t_ssh_cancel1_session_close_frees_forwards`).
#[allow(clippy::too_many_arguments)]
async fn run_public_forward(
    listener: TcpListener,
    ssh_handle: Handle,
    connected_address: String,
    connected_port: u16,
    conn_permits: Arc<Semaphore>,
    tunnel_permits: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
    relay_rx: Arc<AtomicU64>,
    relay_tx: Arc<AtomicU64>,
    total_rx_bytes: Arc<AtomicU64>,
    total_tx_bytes: Arc<AtomicU64>,
    registration: Registration,
    tunnel_opts: crate::shared::TunnelOptions,
    tls: Option<tokio_rustls::TlsAcceptor>,
    bind_domain: Option<String>,
) {
    let _registration = registration;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(%err, "ssh-gateway: failed to accept public tunnel connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        tune_tcp(&stream);

        // Bound the number of concurrently proxied connections, both
        // globally (server `--max-conns`) and per-tunnel (grant/exec
        // `max-conns`, default `DEFAULT_MAX_CONNS`). At capacity, drop the
        // connection rather than exhausting memory and file descriptors.
        let conn_permit = match Arc::clone(&conn_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(?addr, "ssh-gateway: too many active connections, dropping");
                continue;
            }
        };
        let tunnel_permit = match Arc::clone(&tunnel_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    ?addr,
                    "ssh-gateway: per-tunnel connection cap reached, dropping"
                );
                continue;
            }
        };

        // Terminate TLS / gate basic-auth / redirect at the edge exactly
        // like the native public-tunnel path (`edge::accept`). With no
        // https/force-https/basic-auth requested this is a zero-cost
        // pass-through (no peek, no wait for the peer to speak first).
        let edge = match edge::accept(
            stream,
            tunnel_opts.clone(),
            tls.as_ref(),
            connected_port,
            bind_domain.as_deref(),
        )
        .await
        {
            Ok(Some(edge)) => edge,
            Ok(None) => continue, // redirected to https:// or rejected (401)
            Err(err) => {
                trace!(%err, "ssh-gateway: edge handling failed");
                continue;
            }
        };

        let ssh_channel = match ssh_handle
            .channel_open_forwarded_tcpip(
                connected_address.clone(),
                u32::from(connected_port),
                addr.ip().to_string(),
                u32::from(addr.port()),
            )
            .await
        {
            Ok(channel) => channel,
            // `ChannelOpenFailure` (SSH_MSG_CHANNEL_OPEN_FAILURE) is the
            // peer explicitly rejecting THIS ONE channel — most commonly
            // `ConnectFailed` because the client's own local destination
            // refused the connection (wrong port, service down/restarting,
            // firewalled) — not a sign the control connection is dead.
            // Killing the whole forward here means one bad connection
            // attempt permanently drops every future connection until the
            // client reconnects (found via a real repro: `spawn_echo_service`
            // in `tests/ssh_gateway_test.rs` bound `"localhost"`, which
            // resolved IPv6-only on a CI runner while `-R` specs target the
            // literal IPv4 `127.0.0.1` — every proxied connection got a
            // genuine `ConnectFailed` and nuked the tunnel for every
            // subsequent connection too, incl. unrelated in-flight ones).
            // Any OTHER error (`Disconnect`, `SendError`, IO failure — the
            // channel-message channel closing under us) means the session
            // really is gone, so `return` there is still correct.
            Err(russh::Error::ChannelOpenFailure(reason)) => {
                warn!(?reason, ?addr, "ssh-gateway: forwarded-tcpip channel rejected by client, dropping this connection");
                continue;
            }
            Err(err) => {
                warn!(%err, "ssh-gateway: failed to open forwarded-tcpip channel, stopping forward");
                return;
            }
        };

        let active = Arc::clone(&active);
        let relay_rx = Arc::clone(&relay_rx);
        let relay_tx = Arc::clone(&relay_tx);
        let total_rx_bytes = Arc::clone(&total_rx_bytes);
        let total_tx_bytes = Arc::clone(&total_tx_bytes);
        tokio::spawn(async move {
            let _conn_permit = conn_permit;
            let _tunnel_permit = tunnel_permit;
            let _active = ActiveGuard::new(active);
            let mut ssh_stream = ssh_channel.into_stream();
            let mut counted =
                CountingStream::new(edge, relay_rx, relay_tx, total_rx_bytes, total_tx_bytes);
            let buf = proxy_buffer_size();
            if let Err(err) =
                tokio::io::copy_bidirectional_with_sizes(&mut counted, &mut ssh_stream, buf, buf)
                    .await
            {
                trace!(%err, "ssh-gateway: proxied connection closed");
            }
        });
    }
}

/// Opens a `forwarded-tcpip` channel toward the SSH peer that registered a
/// vhost/secret tunnel via `-R`. Implements [`mux::ChannelOpen`] so it can be
/// stored in a [`mux::LinkOpener::Ssh`] and driven by the shared
/// [`CarrierPool`](crate::pool::CarrierPool)/relay code exactly like a mux
/// [`mux::Opener`] — the vhost/secret relay paths never know the peer is SSH.
struct SshOpener {
    handle: Handle,
    connected_address: String,
    connected_port: u16,
}

impl SshOpener {
    fn new(handle: Handle, connected_address: String, connected_port: u16) -> Self {
        Self {
            handle,
            connected_address,
            connected_port,
        }
    }
}

impl mux::ChannelOpen for SshOpener {
    fn open(
        &self,
        forward_ip: Option<&str>,
    ) -> Pin<Box<dyn Future<Output = io::Result<mux::LinkStream>> + Send + '_>> {
        // SSH has no STREAM_READY marker (I-4): the caller IP travels as the
        // channel-open request's own originator-IP field instead.
        let originator_ip = forward_ip.unwrap_or("0.0.0.0").to_string();
        let handle = self.handle.clone();
        let connected_address = self.connected_address.clone();
        let connected_port = self.connected_port;
        Box::pin(async move {
            let channel = handle
                .channel_open_forwarded_tcpip(
                    connected_address,
                    u32::from(connected_port),
                    originator_ip,
                    0,
                )
                .await
                .map_err(io::Error::other)?;
            Ok(Box::new(channel.into_stream()) as mux::LinkStream)
        })
    }
}

/// Where a granted `tcpip-forward` request routes to (D1's address grammar):
/// a native `bore local` public tunnel, a vhost subdomain, or a secret-tunnel
/// provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardSpec {
    /// Public tunnel. `port` is the requested port; `0` means "assign one
    /// from the server's port range".
    Public {
        /// Requested port, or 0 to auto-assign.
        port: u16,
    },
    /// Vhost subdomain forward.
    Vhost {
        /// Subdomain label: lowercase `[a-z0-9-]+`, single label, same
        /// charset as [`crate::vhost::extract_subdomain`].
        label: String,
    },
    /// Secret-tunnel provider forward.
    SecretProvider {
        /// Secret tunnel id, same charset as a vhost label.
        id: String,
    },
}

/// Error parsing a `tcpip-forward`/`direct-tcpip` address into a
/// [`ForwardSpec`] or secret-consumer target id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(pub String);

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpecError {}

/// Validates a vhost/secret label: lowercase `[a-z0-9-]+`, single label (no
/// dots), not starting or ending with `-` — the exact charset
/// `vhost::extract_subdomain` (`src/vhost.rs`) accepts.
fn validate_label(label: &str) -> Result<String, SpecError> {
    let valid = !label.is_empty()
        && !label.contains('.')
        && label
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-');
    if valid {
        Ok(label.to_string())
    } else {
        Err(SpecError(format!(
            "invalid label {label:?}: must be lowercase [a-z0-9-]+, a single label, no leading/trailing hyphen"
        )))
    }
}

/// Parses a `tcpip-forward` bind address/port into a [`ForwardSpec`] (D1):
/// - empty / `localhost` / `127.0.0.1` / `0.0.0.0` / `*` → [`ForwardSpec::Public`];
/// - `vhost/<label>` → [`ForwardSpec::Vhost`], any port;
/// - `secret/<id>` → [`ForwardSpec::SecretProvider`], any port;
/// - a bare label on port 80/443 → [`ForwardSpec::Vhost`];
/// - a bare label on port 0 → [`ForwardSpec::SecretProvider`];
/// - a bare label on any other port is ambiguous and rejected — use a
///   `vhost/` or `secret/` prefix to disambiguate.
pub fn parse_forward_spec(addr: &str, port: u32) -> Result<ForwardSpec, SpecError> {
    let port16 = u16::try_from(port).map_err(|_| SpecError(format!("port {port} out of range")))?;

    if addr.is_empty() || matches!(addr, "localhost" | "127.0.0.1" | "0.0.0.0" | "*") {
        return Ok(ForwardSpec::Public { port: port16 });
    }
    if let Some(label) = addr.strip_prefix("vhost/") {
        return validate_label(label).map(|label| ForwardSpec::Vhost { label });
    }
    if let Some(id) = addr.strip_prefix("secret/") {
        return validate_label(id).map(|id| ForwardSpec::SecretProvider { id });
    }
    match port16 {
        80 | 443 => validate_label(addr).map(|label| ForwardSpec::Vhost { label }),
        0 => validate_label(addr).map(|id| ForwardSpec::SecretProvider { id }),
        _ => Err(SpecError(format!(
            "ambiguous forward address {addr:?} on port {port16}; use a vhost/ or secret/ prefix"
        ))),
    }
}

/// Parses a `direct-tcpip` destination host/port into a secret-consumer
/// target id (Phase 5.3 routes `ssh -L` through this). The port is ignored:
/// unlike `-R` (which supports a literal `0` for dynamic remote-port
/// allocation), OpenSSH's `-L` CLI parser rejects a literal `0` destination
/// port outright (`Bad local forwarding specification`), so a real client
/// can never send one here — callers must use some nonzero placeholder (e.g.
/// `-L <local>:<id>:1`), and only the host label (`<id>` or `secret/<id>`)
/// determines routing.
pub fn parse_direct_tcpip_dest(host: &str, port: u32) -> Result<String, SpecError> {
    let _ = port;
    let id = host.strip_prefix("secret/").unwrap_or(host);
    validate_label(id).map_err(|_| SpecError(format!("invalid secret tunnel id {host:?}")))
}

/// Client-transport-only keys: features the native `bore` client implements
/// that have no equivalent over SSH ingress. Recognized so they produce a
/// clear warning instead of a silent no-op or an "unknown parameter" one.
const TRANSPORT_ONLY_KEYS: &[&str] = &[
    "udp",
    "carriers",
    "stun-server",
    "upnp",
    "try-port-prediction",
    "nat-udp-preferred-port",
    "auto-reconnect",
];

/// Per-forward parameters parsed from an `exec` request string and/or
/// `env` requests, merged with a [`crate::sshgw_auth::KeyGrant`]'s own
/// options (precedence: grant > exec > env, per I-2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    /// Free-text notes for the admin dashboard.
    pub notes: Option<String>,
    /// Per-tunnel connection cap.
    pub max_conns: Option<usize>,
    /// HTTP basic-auth credentials (`user:pass`) for a vhost forward.
    pub basic_auth: Option<String>,
    /// Enable per-tunnel access logging.
    pub webserver_log: bool,
    /// Terminate TLS on a PUBLIC tunnel's own port (server must have a
    /// certificate configured — `--cert-file`/`--key-file`). No effect on
    /// vhost/secret forwards: vhost already serves HTTPS server-side via
    /// `vhost.yml`/`--vhost-mode`, and secret tunnels are opaque TCP.
    pub https: bool,
    /// Redirect plain HTTP on a PUBLIC tunnel's own port to `https://`.
    /// Meaningless without `https=on` — set alongside it without one, this
    /// is disabled with a warning rather than silently ignored (I-2).
    pub force_https: bool,
    /// One warning per unsupported or unrecognized key, in encounter order —
    /// nothing is silently dropped (I-2).
    pub warnings: Vec<String>,
}

/// Splits a `key=value ...` string into tokens, honoring double-quoted
/// values that may contain spaces (e.g. `notes="two words"`). Quote
/// characters themselves are stripped, not part of the token.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Splits each whitespace-delimited (quote-aware) token on its first `=`
/// into a `(key, value)` pair. A token without an `=` is returned as an
/// `Err(token)` rather than dropped — I-2 forbids silently ignoring a
/// malformed parameter (e.g. `https:on` typoed for `https=on`).
fn parse_kv_tokens(s: &str) -> Vec<Result<(String, String), String>> {
    tokenize(s)
        .into_iter()
        .map(|tok| {
            tok.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or(tok)
        })
        .collect()
}

/// Maps `BORE_<KEY>` environment entries to the same `key=value` grammar as
/// `exec` params (e.g. `BORE_MAX_CONNS` → `max-conns`). Entries without the
/// `BORE_` prefix are ignored — they are not bore parameters.
fn env_params(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("BORE_")
                .map(|rest| (rest.to_ascii_lowercase().replace('_', "-"), v.clone()))
        })
        .collect()
}

/// Parses `exec`/`env` request data into [`Params`], applying the SSH
/// gateway's key=value grammar (I-2). Precedence is grant > exec > env: a
/// [`crate::sshgw_auth::KeyGrant`]'s own `max-conns`/`notes` always win, an
/// `exec` value wins over the same key set via `env`, and any key naming a
/// client-transport-only feature or that isn't recognized at all produces a
/// warning rather than being silently accepted or dropped.
pub fn parse_params(
    exec: Option<&str>,
    env: &[(String, String)],
    grant: &crate::sshgw_auth::KeyGrant,
) -> Params {
    let mut merged: Vec<(String, String)> = env_params(env);
    let mut malformed: Vec<String> = Vec::new();
    if let Some(exec) = exec {
        for tok in parse_kv_tokens(exec) {
            match tok {
                Ok(kv) => merged.push(kv),
                Err(tok) => malformed.push(tok),
            }
        }
    }

    let mut params = Params::default();
    for tok in &malformed {
        params.warnings.push(format!(
            "malformed parameter {tok:?} (expected key=value); ignored"
        ));
    }
    for (key, value) in &merged {
        match key.as_str() {
            "notes" => params.notes = Some(value.clone()),
            "max-conns" => match value.parse() {
                Ok(n) => params.max_conns = Some(n),
                Err(_) => params
                    .warnings
                    .push(format!("max-conns: invalid value {value:?}")),
            },
            "basic-auth" => params.basic_auth = Some(value.clone()),
            "webserver-log" => params.webserver_log = value == "on",
            // A vhost/secret tunnel's identity over SSH ingress is the
            // authenticated SSH key/label (`grant.identity`), used directly for
            // vhost route resolution and reserved-subdomain ownership — never a
            // client-supplied `id=`. Honoring `id=` would let any authenticated
            // key claim another identity's reserved routes, so it is refused
            // with a warning, not silently ignored (I-2).
            "id" => params.warnings.push(
                "id: not supported via SSH ingress; the tunnel identity is your \
                 authenticated SSH key/label"
                    .to_string(),
            ),
            "https" => params.https = value == "on",
            "force-https" => params.force_https = value == "on",
            k if TRANSPORT_ONLY_KEYS.contains(&k) => params.warnings.push(format!(
                "{k}: not available via SSH ingress; use the native bore client"
            )),
            k => params.warnings.push(format!("{k}: unknown parameter")),
        }
    }

    if let Some(max_conns) = grant.max_conns {
        params.max_conns = Some(max_conns);
    }
    if let Some(notes) = &grant.notes {
        params.notes = Some(notes.clone());
    }

    // Mirror the native client's `--force-https requires --https` rule
    // (`main.rs`'s `#[clap(requires = "https")]`): rather than silently
    // ignoring it or rejecting the whole tunnel, disable it with an explicit
    // warning (I-2) and keep the tunnel plain.
    if params.force_https && !params.https {
        params.warnings.push(
            "force-https: requires https=on; ignoring force-https for this tunnel".to_string(),
        );
        params.force_https = false;
    }

    params
}

// ---------------------------------------------------------------------------
// §7 tunnel-info banners (docs/SSH_GATEWAY.md §7): a short, professional,
// unambiguous report delivered to the session channel once a forward
// finishes establishing (via `ConnState::deliver`, §2). Every line reports a
// fact the SERVER actually knows — never the client's own `-R`/`-L` local
// destination, which RFC4254's `tcpip-forward`/`direct-tcpip` messages never
// transmit to the server (there is no wire field for it; the client alone
// decides where to splice a channel's bytes locally). Claiming to know it
// would be lying to the user in the one place they're looking for the truth.
// ---------------------------------------------------------------------------

/// Right-pads a label to a fixed column so every value in a banner lines up,
/// e.g. `banner_line("Notes:", "(none)")` → `"  Notes:            (none)"`.
fn banner_line(label: &str, value: impl std::fmt::Display) -> String {
    format!("  {label:<18}{value}")
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "enabled"
    } else {
        "disabled"
    }
}

fn none_if_empty(value: Option<&str>) -> &str {
    value.filter(|v| !v.is_empty()).unwrap_or("(none)")
}

/// Human-readable label for a resolved vhost frontend mode.
fn vhost_mode_label(mode: crate::vhost::VhostMode) -> &'static str {
    use crate::vhost::VhostMode;
    match mode {
        VhostMode::Http => "HTTP only",
        VhostMode::Https => "HTTPS only",
        VhostMode::Both => "HTTP + HTTPS (no redirect)",
        VhostMode::RedirectHttps => "HTTPS (HTTP redirects to HTTPS)",
    }
}

/// `"2 configured: X-Foo, X-Bar"` / `"(none)"` — synthetic (names only, no
/// values) so the banner stays short and never echoes header *values* that
/// could be sensitive, while still answering "were my headers applied?".
fn header_summary(headers: &[(String, String)]) -> String {
    if headers.is_empty() {
        return "(none)".to_string();
    }
    let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    format!("{} configured: {}", headers.len(), names.join(", "))
}

/// `"200 (requested)"` / `"64 (key policy)"` / `"100 (default)"` — the
/// precedence is grant > exec/env > default (I-2's documented order); once
/// `parse_params` resolves it to a single value we can no longer tell exec
/// apart from env, so both collapse to "requested".
fn max_conns_provenance(grant_max_conns: Option<usize>, effective: usize) -> String {
    if grant_max_conns.is_some() {
        format!("{effective} (key policy)")
    } else if effective == DEFAULT_MAX_CONNS {
        format!("{effective} (default)")
    } else {
        format!("{effective} (requested)")
    }
}

/// Inputs for [`vhost_info_banner`], grouped into a struct rather than a long
/// positional argument list (`clippy::too_many_arguments`).
struct VhostBannerInfo<'a> {
    urls: &'a [String],
    mode: crate::vhost::VhostMode,
    identity: &'a str,
    notes: Option<&'a str>,
    basic_auth: bool,
    webserver_log: bool,
    request_headers: &'a [(String, String)],
    response_headers: &'a [(String, String)],
}

/// Banner for a newly-established **vhost** forward.
fn vhost_info_banner(info: VhostBannerInfo<'_>) -> Vec<String> {
    let mut lines = vec!["Vhost tunnel established".to_string()];
    if info.urls.is_empty() {
        lines.push(banner_line("Public URL:", "(none — no cert configured)"));
    }
    for (i, url) in info.urls.iter().enumerate() {
        let label = if i == 0 { "Public URL:" } else { "" };
        lines.push(banner_line(label, url));
    }
    lines.push(banner_line("Mode:", vhost_mode_label(info.mode)));
    lines.push(banner_line("Identity:", info.identity));
    lines.push(banner_line("Notes:", none_if_empty(info.notes)));
    lines.push(banner_line("Basic-auth:", on_off(info.basic_auth)));
    lines.push(banner_line("Webserver-log:", on_off(info.webserver_log)));
    lines.push(banner_line(
        "Max-conns:",
        "n/a for vhost (server-wide --max-conns applies; no per-tunnel cap)",
    ));
    lines.push(banner_line(
        "Request headers:",
        header_summary(info.request_headers),
    ));
    lines.push(banner_line(
        "Response headers:",
        header_summary(info.response_headers),
    ));
    lines
}

/// Inputs for [`public_info_banner`], grouped into a struct rather than a
/// long positional argument list (`clippy::too_many_arguments`).
struct PublicBannerInfo<'a> {
    bound_port: u16,
    identity: &'a str,
    notes: Option<&'a str>,
    grant_max_conns: Option<usize>,
    effective_max_conns: usize,
    basic_auth: bool,
    https: bool,
    force_https: bool,
    webserver_log: bool,
}

/// Banner for a newly-established **public** (`-R <port>`) forward.
fn public_info_banner(info: PublicBannerInfo<'_>) -> Vec<String> {
    vec![
        "Public tunnel established".to_string(),
        banner_line("Public port:", info.bound_port),
        banner_line("Identity:", info.identity),
        banner_line("Notes:", none_if_empty(info.notes)),
        banner_line(
            "Max-conns:",
            max_conns_provenance(info.grant_max_conns, info.effective_max_conns),
        ),
        banner_line("Basic-auth:", on_off(info.basic_auth)),
        banner_line("HTTPS:", on_off(info.https)),
        banner_line("Force-HTTPS:", on_off(info.force_https)),
        banner_line("Webserver-log:", on_off(info.webserver_log)),
    ]
}

/// Banner for a newly-established **secret provider** (`-R secret/<id>:0`)
/// forward, including the exact command the other side (the "consumer")
/// needs to reach it. The host/port in that command are deliberately left as
/// placeholders naming what they mean rather than a guessed value: the
/// gateway cannot reliably know its own externally-reachable hostname, and
/// guessing wrong is worse than an honest placeholder.
fn secret_provider_info_banner(id: &str, identity: &str, notes: Option<&str>) -> Vec<String> {
    vec![
        "Secret provider tunnel established".to_string(),
        banner_line("Secret ID:", id),
        banner_line("Identity:", identity),
        banner_line("Notes:", none_if_empty(notes)),
        banner_line(
            "Max-conns:",
            "n/a for secret provider (not enforced per-tunnel)",
        ),
        banner_line(
            "Basic-auth:",
            "n/a for secret provider (opaque TCP, no HTTP layer)",
        ),
        String::new(),
        "Consumer command (run on the other side, same host/port you used here):".to_string(),
        format!("  ssh -p <same-port> -L <local-port>:secret/{id}:1 <same-host>"),
    ]
}

/// Banner for a newly-established **secret consumer** (`-L <port>:secret/<id>:1`)
/// session — fired once per session (see `consumer_entry`'s `is_new`), not
/// once per proxied connection.
fn secret_consumer_info_banner(
    id: &str,
    identity: &str,
    notes: Option<&str>,
    provider_identity: Option<&str>,
) -> Vec<String> {
    vec![
        format!("Attached to secret '{id}'"),
        banner_line("Secret ID:", id),
        banner_line("Identity:", identity),
        banner_line("Notes:", none_if_empty(notes)),
        banner_line(
            "Provider identity:",
            provider_identity.unwrap_or("(unknown — provider may be a native bore client)"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn base_config(dir: &Path) -> SshGatewayConfig {
        SshGatewayConfig {
            port: None,
            host_key_file: dir.join("host_key.pem"),
            authorized_keys_dir: None,
            passwords_file: None,
            banner: None,
            window_size: SSH_DEFAULT_WINDOW_SIZE,
        }
    }

    fn build(config: SshGatewayConfig) -> Result<SshGateway> {
        build_with_vhost_config(config, None)
    }

    fn build_with_vhost_config(
        config: SshGatewayConfig,
        vhost_config: Option<SharedVhostConfig>,
    ) -> Result<SshGateway> {
        SshGateway::new(
            config,
            secret::Registry::default(),
            VhostRegistry::default(),
            vhost_config,
            AdminRegistry::default(),
            Arc::new(Semaphore::new(1)),
            1024..=65535,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            None,
            None,
        )
    }

    #[test]
    fn sshgw_config_validation() {
        let dir = tempfile::tempdir().unwrap();

        let cfg = base_config(dir.path());
        assert!(cfg.validate().is_err(), "no auth source must be rejected");

        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));
        assert!(cfg.validate().is_ok(), "keys-dir alone is sufficient");

        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        assert!(cfg.validate().is_ok(), "passwords-file alone is sufficient");

        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));
        cfg.passwords_file = Some(dir.path().join("passwords"));
        assert!(cfg.validate().is_ok(), "both sources together are fine");
    }

    #[test]
    fn host_key_generated_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.authorized_keys_dir = Some(dir.path().join("keys"));

        assert!(!cfg.host_key_file.exists());
        let gateway = build(cfg.clone()).expect("first construction generates a host key");
        assert!(cfg.host_key_file.exists());
        let first = gateway.host_key.fingerprint(HashAlg::Sha256);

        let gateway2 = build(cfg).expect("second construction reloads the same host key");
        let second = gateway2.host_key.fingerprint(HashAlg::Sha256);
        assert_eq!(
            first, second,
            "reloaded host key must have the same fingerprint"
        );
    }

    fn grant(identity: &str) -> crate::sshgw_auth::KeyGrant {
        crate::sshgw_auth::KeyGrant {
            identity: identity.to_string(),
            permit: None,
            max_conns: None,
            notes: None,
        }
    }

    #[test]
    fn spec_matrix() {
        let ok_cases = [
            ("", 9005, ForwardSpec::Public { port: 9005 }),
            ("localhost", 9005, ForwardSpec::Public { port: 9005 }),
            ("127.0.0.1", 9005, ForwardSpec::Public { port: 9005 }),
            ("0.0.0.0", 9005, ForwardSpec::Public { port: 9005 }),
            ("*", 9005, ForwardSpec::Public { port: 9005 }),
            (
                "vhost/foo",
                9005,
                ForwardSpec::Vhost {
                    label: "foo".to_string(),
                },
            ),
            (
                "vhost/foo",
                0,
                ForwardSpec::Vhost {
                    label: "foo".to_string(),
                },
            ),
            (
                "secret/bar",
                9005,
                ForwardSpec::SecretProvider {
                    id: "bar".to_string(),
                },
            ),
            (
                "secret/bar",
                0,
                ForwardSpec::SecretProvider {
                    id: "bar".to_string(),
                },
            ),
            (
                "mysub",
                80,
                ForwardSpec::Vhost {
                    label: "mysub".to_string(),
                },
            ),
            (
                "mysub",
                443,
                ForwardSpec::Vhost {
                    label: "mysub".to_string(),
                },
            ),
            (
                "tcp-id",
                0,
                ForwardSpec::SecretProvider {
                    id: "tcp-id".to_string(),
                },
            ),
        ];
        for (addr, port, expected) in ok_cases {
            assert_eq!(
                parse_forward_spec(addr, port).unwrap(),
                expected,
                "addr={addr:?} port={port}"
            );
        }

        let err_cases = [
            ("mysub", 8080),  // ambiguous: bare label, non-80/443/0 port
            ("My_Sub", 80),   // uppercase/underscore not allowed
            ("a.b", 80),      // dot not allowed in a single label
            ("-bad", 80),     // leading hyphen not allowed
            ("bad-", 443),    // trailing hyphen not allowed
            ("vhost/", 9005), // empty label after prefix
        ];
        for (addr, port) in err_cases {
            assert!(
                parse_forward_spec(addr, port).is_err(),
                "addr={addr:?} port={port} should be rejected"
            );
        }
    }

    #[test]
    fn params_precedence() {
        let mut g = grant("id1");
        g.max_conns = Some(3);
        let params = parse_params(Some("max-conns=9"), &[], &g);
        assert_eq!(params.max_conns, Some(3), "grant value must win over exec");

        let env = [("BORE_MAX_CONNS".to_string(), "7".to_string())];
        let params = parse_params(None, &env, &grant("id2"));
        assert_eq!(
            params.max_conns,
            Some(7),
            "env value must be used when exec is absent"
        );
    }

    #[test]
    fn params_quoting() {
        let params = parse_params(
            Some(r#"notes="two words" basic-auth=u:p"#),
            &[],
            &grant("id"),
        );
        assert_eq!(params.notes.as_deref(), Some("two words"));
        assert_eq!(params.basic_auth.as_deref(), Some("u:p"));
        assert!(params.warnings.is_empty());
    }

    #[test]
    fn params_https_force_https() {
        let params = parse_params(Some("https=on force-https=on"), &[], &grant("id"));
        assert!(params.https);
        assert!(params.force_https);
        assert!(params.warnings.is_empty());

        // force-https without https: disabled with a warning, not silently
        // kept or a hard reject (I-2).
        let params = parse_params(Some("force-https=on"), &[], &grant("id"));
        assert!(!params.https);
        assert!(!params.force_https);
        assert_eq!(params.warnings.len(), 1);
        assert!(params.warnings[0].contains("force-https"));
        assert!(params.warnings[0].contains("requires https=on"));

        // Anything other than exactly "on" leaves both off, same convention
        // as webserver-log=.
        let params = parse_params(Some("https=yes"), &[], &grant("id"));
        assert!(!params.https);
    }

    #[test]
    fn params_malformed_token_warns_not_silently_dropped() {
        // A token with no `=` (e.g. `https:on` typoed for `https=on`, the
        // exact mistake that triggered a real "params never applied, no
        // warning anywhere" bug report) must WARN, never silently vanish
        // (I-2) — `parse_kv_tokens` used to `filter_map` these away with no
        // trace at all.
        let params = parse_params(Some("https:on force-https=on"), &[], &grant("id"));
        assert!(params
            .warnings
            .iter()
            .any(|w| w.contains("https:on") && w.contains("expected key=value")));
        // The malformed token contributes nothing: `https` stays off, so the
        // well-formed `force-https=on` alongside it also gets disabled with
        // its own separate warning (two warnings total, not a silent drop
        // of either).
        assert!(params
            .warnings
            .iter()
            .any(|w| w.contains("force-https") && w.contains("requires https=on")));
        assert_eq!(params.warnings.len(), 2);
        assert!(!params.https);
        assert!(!params.force_https);

        // A well-formed token elsewhere in the same string still parses
        // normally — one malformed token doesn't poison the rest.
        let params = parse_params(Some("notes=ok https:on"), &[], &grant("id"));
        assert_eq!(params.notes.as_deref(), Some("ok"));
        assert_eq!(params.warnings.len(), 1);
        assert!(params.warnings[0].contains("https:on"));
    }

    #[test]
    fn params_id_warns_not_silently_ignored() {
        // `id=` is a documented native-client param, but over SSH ingress the
        // identity is the authenticated key/label — honoring a client-supplied
        // id would break the auth model, so it must WARN, never silently apply
        // or silently drop (I-2). Regression guard for the dead-field bug.
        let params = parse_params(Some("id=custom"), &[], &grant("real-id"));
        assert_eq!(
            params.warnings.len(),
            1,
            "id= must emit exactly one warning"
        );
        assert!(params.warnings[0].contains("id"));
        assert!(
            params.warnings[0].contains("not supported"),
            "got: {}",
            params.warnings[0]
        );
    }

    #[test]
    fn russh_config_uses_configured_window_and_beats_russh_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        let gw = build(cfg).unwrap();
        let config = gw.russh_config();
        assert_eq!(config.window_size, SSH_DEFAULT_WINDOW_SIZE);
        assert_eq!(config.maximum_packet_size, SSH_MAX_PACKET_SIZE);
        assert!(
            config.window_size > 2 * 1024 * 1024,
            "default window must exceed russh's own 2 MiB (the BDP cap we lift)"
        );
    }

    #[test]
    fn window_size_below_floor_is_clamped_up() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        cfg.window_size = 1; // absurdly small, would wedge a channel
        let gw = build(cfg).unwrap();
        assert_eq!(gw.russh_config().window_size, SSH_MIN_WINDOW_SIZE);
    }

    #[tokio::test]
    async fn authentication_banner_reflects_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        cfg.banner = Some("welcome to bore".to_string());
        let gw = Arc::new(build(cfg).unwrap());
        let mut handler = gw.handler("127.0.0.1:2222".parse().unwrap());
        assert_eq!(
            handler.authentication_banner().await.unwrap(),
            Some("welcome to bore".to_string()),
            "configured --ssh-banner must reach the client (regression: the flag was dead)"
        );

        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        let gw = Arc::new(build(cfg).unwrap());
        let mut handler = gw.handler("127.0.0.1:2222".parse().unwrap());
        assert_eq!(
            handler.authentication_banner().await.unwrap(),
            None,
            "no banner configured must send nothing"
        );
    }

    #[test]
    fn params_warnings_for_transport_keys() {
        let params = parse_params(Some("udp=on carriers=4"), &[], &grant("id"));
        assert_eq!(params.warnings.len(), 2);
        assert!(params.warnings[0].contains("udp"));
        assert!(params.warnings[1].contains("carriers"));
        assert!(params
            .warnings
            .iter()
            .all(|w| w.contains("not available via SSH ingress")));
    }

    #[test]
    fn direct_tcpip_dest() {
        assert_eq!(
            parse_direct_tcpip_dest("tcp-id", 0).unwrap(),
            "tcp-id".to_string()
        );
        assert_eq!(
            parse_direct_tcpip_dest("secret/tcp-id", 0).unwrap(),
            "tcp-id".to_string()
        );
        // The port is ignored — a real OpenSSH `-L` client can never send a
        // literal 0 (its CLI parser rejects that syntax), so any nonzero
        // placeholder a client sends must still route correctly.
        assert_eq!(
            parse_direct_tcpip_dest("tcp-id", 1).unwrap(),
            "tcp-id".to_string()
        );
        assert_eq!(
            parse_direct_tcpip_dest("secret/tcp-id", 80).unwrap(),
            "tcp-id".to_string()
        );
        assert!(parse_direct_tcpip_dest("example.com", 80).is_err());
    }

    #[test]
    fn port_rule_parsing() {
        assert_eq!(parse_port_rule("port/9005"), Some(9005..=9005));
        assert_eq!(parse_port_rule("port/9000-9010"), Some(9000..=9010));
        assert_eq!(parse_port_rule("port/9010-9000"), None, "reversed range");
        assert_eq!(parse_port_rule("vhost/foo"), None, "not a port rule");
        assert_eq!(parse_port_rule("port/abc"), None, "not a number");
    }

    #[test]
    fn permitted_ranges_unrestricted_without_permit() {
        let g = grant("id");
        assert_eq!(
            permitted_port_ranges(&g, &(1024..=65535)),
            vec![1024..=65535]
        );
    }

    #[test]
    fn permitted_ranges_from_permit_list() {
        let mut g = grant("id");
        g.permit = Some(vec!["port/9000-9010".to_string(), "port/9999".to_string()]);
        assert_eq!(
            permitted_port_ranges(&g, &(1024..=65535)),
            vec![9000..=9010, 9999..=9999]
        );
    }

    #[test]
    fn range_intersect() {
        assert_eq!(intersect(&(1..=10), &(5..=20)), Some(5..=10));
        assert_eq!(intersect(&(1..=4), &(5..=20)), None, "disjoint");
        assert_eq!(intersect(&(1..=10), &(1..=10)), Some(1..=10), "identical");
    }

    #[tokio::test]
    async fn bind_permitted_rejects_out_of_range_port() {
        let g = grant("id");
        let err = bind_permitted(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            &(20000..=20010),
            &g,
            9005,
        )
        .await
        .unwrap_err();
        assert!(err.contains("not permitted"), "got: {err}");
    }

    #[tokio::test]
    async fn bind_permitted_rejects_empty_intersection() {
        let mut g = grant("id");
        g.permit = Some(vec!["port/1-100".to_string()]);
        let err = bind_permitted(IpAddr::V4(Ipv4Addr::UNSPECIFIED), &(20000..=20010), &g, 0)
            .await
            .unwrap_err();
        assert!(err.contains("allows no ports"), "got: {err}");
    }

    #[tokio::test]
    async fn bind_permitted_allows_requested_port_in_range() {
        let g = grant("id");
        let listener = bind_permitted(IpAddr::V4(Ipv4Addr::LOCALHOST), &(20000..=20010), &g, 0)
            .await
            .expect("auto-assign within an unrestricted range must succeed");
        let port = listener.local_addr().unwrap().port();
        assert!((20000..=20010).contains(&port));
    }

    #[test]
    fn keepalive_max_misses_lands_on_ctrl_timeout() {
        // The (SSH_KEEPALIVE_MAX_MISSES + 1)-th missed probe is the fatal
        // one (russh disconnects once alive_timeouts > keepalive_max), so
        // that many intervals must equal SSH_CTRL_TIMEOUT exactly (I-3).
        let fatal_tick = SSH_KEEPALIVE_INTERVAL * (SSH_KEEPALIVE_MAX_MISSES as u32 + 1);
        assert_eq!(fatal_tick, SSH_CTRL_TIMEOUT);
    }

    #[test]
    fn russh_config_wires_keepalive_reaper() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = base_config(dir.path());
        cfg.passwords_file = Some(dir.path().join("passwords"));
        let gw = build(cfg).unwrap();
        let config = gw.russh_config();
        assert_eq!(config.keepalive_interval, Some(SSH_KEEPALIVE_INTERVAL));
        assert_eq!(config.keepalive_max, SSH_KEEPALIVE_MAX_MISSES);
        assert_eq!(config.inactivity_timeout, Some(SSH_PREAUTH_GRACE));
        assert_eq!(config.max_auth_attempts, SSH_MAX_AUTH_ATTEMPTS);
    }

    #[test]
    fn glob_match_wildcards() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo", "foo"));
        assert!(!glob_match("foo", "foobar"));
        assert!(glob_match("foo*", "foobar"));
        assert!(!glob_match("foo*", "barfoo"));
        assert!(glob_match("*bar", "foobar"));
        assert!(!glob_match("*bar", "barfoo"));
        assert!(glob_match("foo*bar", "foo-baz-bar"));
        assert!(!glob_match("foo*bar", "foo-baz"));
        assert!(glob_match("*-staging", "app-1-staging"));
    }

    #[test]
    fn permit_allows_vhost_and_secret_globs() {
        let unrestricted = grant("alice");
        assert!(permit_allows(&unrestricted, "vhost/", "anything"));
        assert!(permit_allows(&unrestricted, "secret/", "anything"));

        let mut scoped = grant("bob");
        scoped.permit = Some(vec!["vhost/bob-*".to_string(), "port/9000".to_string()]);
        assert!(permit_allows(&scoped, "vhost/", "bob-app"));
        assert!(!permit_allows(&scoped, "vhost/", "eve-app"));
        // A port/ rule contributes nothing to the vhost/secret grammar.
        assert!(!permit_allows(&scoped, "secret/", "bob-app"));

        let mut secret_only = grant("carol");
        secret_only.permit = Some(vec!["secret/carol-*".to_string()]);
        assert!(permit_allows(&secret_only, "secret/", "carol-db"));
        assert!(!permit_allows(&secret_only, "secret/", "other-db"));
        assert!(!permit_allows(&secret_only, "vhost/", "carol-db"));

        let mut none_allowed = grant("dave");
        none_allowed.permit = Some(vec!["port/1000".to_string()]);
        assert!(!permit_allows(&none_allowed, "vhost/", "dave-app"));
    }

    #[test]
    fn takeover_decision_table() {
        // Free: always insert, regardless of the newcomer's identity.
        assert_eq!(takeover_decision(None, "alice"), TakeoverDecision::Insert);
        assert_eq!(takeover_decision(None, ""), TakeoverDecision::Insert);

        // Same-identity SSH incumbent: evict.
        assert_eq!(
            takeover_decision(Some(("alice", true)), "alice"),
            TakeoverDecision::Evict
        );

        // Different-identity SSH incumbent: reject.
        assert_eq!(
            takeover_decision(Some(("alice", true)), "bob"),
            TakeoverDecision::Reject
        );

        // Native incumbent (no identity to compare): always reject, even if
        // the newcomer's identity happens to equal the placeholder.
        assert_eq!(
            takeover_decision(Some(("", false)), "alice"),
            TakeoverDecision::Reject
        );

        // Defense-in-depth: an empty identity on either side never matches.
        assert_eq!(
            takeover_decision(Some(("", true)), ""),
            TakeoverDecision::Reject
        );
        assert_eq!(
            takeover_decision(Some(("alice", true)), ""),
            TakeoverDecision::Reject
        );
    }

    #[test]
    fn apply_takeover_vacant_proceeds_without_touching_owners() {
        let registry: DashMap<String, Arc<()>> = DashMap::new();
        let owners: DashMap<String, ForwardOwner> = DashMap::new();
        assert!(matches!(
            apply_takeover(&registry, &owners, "label", "alice", "thing"),
            TakeoverOutcome::Proceed
        ));
        assert!(owners.is_empty());
    }

    #[test]
    fn apply_takeover_rejects_native_incumbent() {
        let registry: DashMap<String, Arc<()>> = DashMap::new();
        registry.insert("label".to_string(), Arc::new(()));
        let owners: DashMap<String, ForwardOwner> = DashMap::new();
        match apply_takeover(&registry, &owners, "label", "alice", "thing") {
            TakeoverOutcome::Reject(reason) => assert!(reason.contains("already in use")),
            TakeoverOutcome::Proceed => panic!("native incumbent must never be evicted"),
        }
        assert!(registry.contains_key("label"), "native entry left intact");
    }

    #[test]
    fn cancel_target_matches() {
        // Public: exact (address, port) required; no port-agnostic fallback
        // (two public forwards can share a bind address on different ports).
        let keys = [("".to_string(), 9001u16), ("".to_string(), 9002u16)];
        assert_eq!(
            cancel_target(keys.iter(), "", 9001),
            Some(("".to_string(), 9001))
        );
        assert_eq!(
            cancel_target(keys.iter(), "", 9999),
            None,
            "public forward must not match a different port"
        );

        // Vhost/secret registered under the synthesized placeholder port 1;
        // a client canceling with the original 0 (or any other port) still
        // resolves to the unique entry sharing that address.
        let keys = [("vhost/app".to_string(), 1u16)];
        assert_eq!(
            cancel_target(keys.iter(), "vhost/app", 0),
            Some(("vhost/app".to_string(), 1)),
            "vhost cancel with original port 0 must still find the placeholder entry"
        );
        assert_eq!(
            cancel_target(keys.iter(), "vhost/app", 1),
            Some(("vhost/app".to_string(), 1)),
            "vhost cancel echoing the placeholder must also match"
        );

        let keys = [("secret/db".to_string(), 1u16)];
        assert_eq!(
            cancel_target(keys.iter(), "secret/db", 0),
            Some(("secret/db".to_string(), 1))
        );

        // Unknown address matches nothing.
        assert_eq!(cancel_target(keys.iter(), "vhost/other", 0), None);
    }

    #[test]
    fn demux_classify_first_byte_table() {
        assert_eq!(demux_classify_first_byte(None), Route::Ssh);
        assert_eq!(demux_classify_first_byte(Some(b'S')), Route::Ssh);
        assert_eq!(demux_classify_first_byte(Some(0x16)), Route::Tls);
        assert_eq!(demux_classify_first_byte(Some(b'G')), Route::Http);
        assert_eq!(demux_classify_first_byte(Some(0x00)), Route::Bore);
        assert_eq!(demux_classify_first_byte(Some(0xFF)), Route::Bore);
    }

    #[test]
    fn demux_classify_prefix_table() {
        assert_eq!(demux_classify_prefix(b"SSH-2.0-OpenSSH"), PrefixRoute::Ssh);
        assert_eq!(demux_classify_prefix(b"SUBS"), PrefixRoute::NotSsh);
        assert_eq!(demux_classify_prefix(b"GET "), PrefixRoute::NotSsh);
        assert_eq!(
            demux_classify_prefix(b"SSH"),
            PrefixRoute::NotSsh,
            "short of the full prefix"
        );
        assert_eq!(
            demux_classify_prefix(b""),
            PrefixRoute::NotSsh,
            "empty (EOF before any byte)"
        );
    }
}
