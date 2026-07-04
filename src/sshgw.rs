//! Embedded SSH ingress gateway (russh-backed) for `bore server`.
//!
//! Lets a stock OpenSSH client create public, vhost and secret tunnels with
//! `ssh -R`/`-L` and no `bore` binary on the client side. The gateway is
//! ingress-only: from the accepted SSH channel inward, the existing server
//! data path (registries, relay, admin, weblog, `--max-conns`) is reused
//! unmodified. See `docs/SSH_GATEWAY.md` for the design and
//! `docs/plans/plan_SshGateway/` for the implementation plan.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handle, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinHandle;
use tracing::{info, trace, warn};

use crate::admin::{ActiveGuard, AdminRegistry, NewEntry, Registration, Role, Transport};
use crate::secret;
use crate::server::{bind_public_listener, DEFAULT_MAX_CONNS};
use crate::shared::{proxy_buffer_size, tune_tcp, CountingStream};
use crate::sshgw_auth::{KeyGrant, KeyStore, PasswordStore};
use crate::vhost::VhostRegistry;

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

/// Maximum number of authentication attempts (any method) allowed on one
/// connection before russh disconnects it.
pub const SSH_MAX_AUTH_ATTEMPTS: usize = 3;

/// How long a granted `tcpip-forward` waits for `exec`/`env` parameters
/// before registering the tunnel with whatever it has. There is no
/// round-trip dependency between the two SSH requests (`tcpip-forward` is a
/// global request; `exec`/`env` are channel requests on an independently
/// opened channel), so this only covers genuine network jitter — most
/// sessions resolve in well under this. A pure `-N` session (no channel,
/// ever) always pays the full grace period; that is the expected cost of
/// supporting the common "just forward a port" case without special-casing
/// it. Precedent: the `t_ssh_spike2_forwarded_tcpip` probe sleeps 300 ms in
/// an analogous spot.
const PARAMS_GRACE: Duration = Duration::from_millis(500);

/// One-line message written to the channel (then EOF+close) when a client
/// requests an interactive shell — the gateway is ingress-only and never
/// grants one.
const SHELL_DENIED_MESSAGE: &str =
    "bore ssh-gateway: interactive shells are not supported; use -R/-L forwarding.\r\n";

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
    /// Banner text sent to clients before authentication.
    pub banner: Option<String>,
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
    /// Wired for Phase 4.3 (`tcpip_forward` public-tunnel handling).
    #[allow(dead_code)]
    providers: secret::Registry,
    /// Wired for Phase 5 (vhost mapping).
    #[allow(dead_code)]
    vhost_registry: VhostRegistry,
    /// Admin registration, `transport: Ssh` (I-3: RAII teardown per forward).
    admin: AdminRegistry,
    /// Per-connection inbound cap, shared with the rest of the server's
    /// `--max-conns` (this bounds proxied connections, exactly like the
    /// native public/vhost/secret accept loops — never the control
    /// connection itself, which already holds its own single permit for its
    /// whole lifetime from `Server::listen`'s ssh-gateway accept loop).
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
}

impl SshGateway {
    /// Build the gateway: validates `config`, loads/generates the host key,
    /// and wires the credential stores. `providers`/`vhost_registry`/`admin`/
    /// `conn_permits`/`port_range`/`bind_tunnels`/`total_rx_bytes`/
    /// `total_tx_bytes` must be clones of the `Server`'s own — never
    /// re-derived.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: SshGatewayConfig,
        providers: secret::Registry,
        vhost_registry: VhostRegistry,
        admin: AdminRegistry,
        conn_permits: Arc<Semaphore>,
        port_range: RangeInclusive<u16>,
        bind_tunnels: IpAddr,
        total_rx_bytes: Arc<AtomicU64>,
        total_tx_bytes: Arc<AtomicU64>,
    ) -> Result<Self> {
        config.validate()?;
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
            admin,
            conn_permits,
            port_range,
            bind_tunnels,
            total_rx_bytes,
            total_tx_bytes,
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
    pub async fn serve_connection(
        self: &Arc<Self>,
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
    ) -> Result<(), russh::Error> {
        let config = self.russh_config();
        let handler = self.handler(addr);
        russh::server::run_stream(config, stream, handler)
            .await?
            .await
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
    /// Success/diagnostic lines queued for the first session channel this
    /// connection opens. `tcpip-forward` is always processed before any
    /// channel exists (confirmed empirically — SPIKE_FINDINGS.md), so text
    /// meant for the user has nowhere to go until (if ever)
    /// `channel_open_session` fires; for a pure `-N` session (the common
    /// case) it is simply never delivered, which is fine — OpenSSH's own
    /// client already prints "Allocated port N for remote forward".
    pending_messages: Mutex<Vec<String>>,
    /// Live per-forward tasks, keyed by `(bind_address, allocated_port)`, so
    /// `cancel_tcpip_forward` can abort exactly the right one without
    /// disturbing sibling forwards on the same connection (I-3).
    forwards: Mutex<HashMap<(String, u16), JoinHandle<()>>>,
}

impl ConnState {
    /// Queue a line for delivery the next time (if ever) a session channel
    /// is open. See the `pending_messages` field doc for why this can't
    /// simply write to a channel directly.
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
        for line in self.state.drain_messages() {
            session.data(channel_id, format!("{line}\r\n").into_bytes())?;
        }
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
        session.data(channel, SHELL_DENIED_MESSAGE.as_bytes().to_vec())?;
        session.exit_status_request(channel, 1)?;
        session.eof(channel)?;
        session.close(channel)?;
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
            ForwardSpec::Vhost { .. } | ForwardSpec::SecretProvider { .. } => {
                self.state.queue_message(
                    "bore ssh-gateway: vhost/secret forwards are not implemented yet; \
                     use a plain -R (public tunnel) forward"
                        .to_string(),
                );
                return Ok(false);
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
            let params = parse_params(exec.as_deref(), &env, &grant);
            let effective_max_conns = params.max_conns.unwrap_or(DEFAULT_MAX_CONNS);
            let registration = gateway.admin.register(NewEntry {
                role: Role::Public,
                peer,
                secret_id: None,
                public_port: Some(bound_port),
                notes: params.notes.clone(),
                basic_auth: false,
                https: false,
                force_https: false,
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
        let key = (address.to_string(), port16);
        let removed = self
            .state
            .forwards
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        match removed {
            Some(task) => {
                task.abort();
                Ok(true)
            }
            None => Ok(false),
        }
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
/// loop (`src/server.rs`) minus edge/TLS/weblog/direct-UDP, which the SSH
/// gateway does not support yet — there is no `STREAM_READY` anywhere on
/// this path (I-4). `registration` is held for the loop's entire lifetime so
/// the admin entry disappears (RAII) exactly when this task ends.
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
                CountingStream::new(stream, relay_rx, relay_tx, total_rx_bytes, total_tx_bytes);
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
/// target id (Phase 5.3 routes `ssh -L` through this). Only `<id>` or
/// `secret/<id>` on port 0 are accepted; anything else is rejected with a
/// message suitable for the channel-open failure reason.
pub fn parse_direct_tcpip_dest(host: &str, port: u32) -> Result<String, SpecError> {
    if port != 0 {
        return Err(SpecError(format!(
            "direct-tcpip to {host}:{port} not supported; use port 0 with a secret tunnel id"
        )));
    }
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
    /// Explicit tunnel id override.
    pub id: Option<String>,
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
/// into a `(key, value)` pair. Tokens without an `=` are dropped.
fn parse_kv_tokens(s: &str) -> Vec<(String, String)> {
    tokenize(s)
        .into_iter()
        .filter_map(|tok| {
            tok.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
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
    if let Some(exec) = exec {
        merged.extend(parse_kv_tokens(exec));
    }

    let mut params = Params::default();
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
            "id" => params.id = Some(value.clone()),
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

    params
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
        }
    }

    fn build(config: SshGatewayConfig) -> Result<SshGateway> {
        SshGateway::new(
            config,
            secret::Registry::default(),
            VhostRegistry::default(),
            AdminRegistry::default(),
            Arc::new(Semaphore::new(1)),
            1024..=65535,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
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
}
