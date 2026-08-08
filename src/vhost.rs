//! Vhost subdomain reverse-proxy: HTTP(S) frontend routed by Host header.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
#[cfg(feature = "udp")]
use std::sync::atomic::Ordering;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::Deserialize;
use time;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::admin::{AdminRegistry, NewEntry, Role};
use crate::basicauth::{self, BasicAuth};
use crate::edge;
use crate::mux;
use crate::pool::{CarrierPool, PendingCarriers, TokenGuard};
use crate::shared::{
    proxy_buffer_size, ClientMessage, Delimited, ServerMessage, UdpDirectTuning, NETWORK_TIMEOUT,
    UDP_NONCE_LEN,
};
use crate::transport;

// ─── Config data types ────────────────────────────────────────────────────────

fn default_http_port() -> u16 {
    80
}
fn default_https_port() -> u16 {
    443
}
fn default_mode() -> VhostModeCfg {
    VhostModeCfg::Auto
}

/// Top-level `vhost.yml` config.
#[derive(Clone, Debug, Deserialize)]
pub struct VhostConfig {
    /// Base domain, e.g. `bore.mydomain.com`.
    pub base_domain: String,
    /// Frontend mode. Defaults to `auto` (derive from cert presence).
    #[serde(default = "default_mode")]
    pub mode: VhostModeCfg,
    /// HTTP port (default 80).
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// HTTPS port (default 443).
    #[serde(default = "default_https_port")]
    pub https_port: u16,
    /// TLS certificate chain (PEM).
    #[serde(default)]
    pub cert_file: Option<PathBuf>,
    /// TLS private key (PEM).
    #[serde(default)]
    pub key_file: Option<PathBuf>,
    /// Headers injected on every route (per-subdomain overrides these).
    #[serde(default)]
    pub default_headers: BTreeMap<String, String>,
    /// Response headers injected on every routed response.
    #[serde(default)]
    pub default_response_headers: BTreeMap<String, String>,
    /// Static subdomain → client-id reservations.
    #[serde(default)]
    pub reservations: Vec<Reservation>,
}

/// A static subdomain reservation in `vhost.yml`.
#[derive(Clone, Debug, Deserialize)]
pub struct Reservation {
    /// The client id allowed to register this subdomain.
    pub client_id: String,
    /// The subdomain label (single DNS label, e.g. `myapp`).
    pub subdomain: String,
    /// Extra headers injected for this subdomain (merged over `default_headers`).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Extra response headers injected for this subdomain.
    #[serde(default)]
    pub response_headers: BTreeMap<String, String>,
}

/// Frontend mode as expressed in `vhost.yml` (or via CLI override).
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VhostModeCfg {
    /// Serve HTTP only (port 80).
    Http,
    /// Serve HTTPS only (port 443). Requires a certificate.
    Https,
    /// Serve both HTTP and HTTPS. Requires a certificate.
    Both,
    /// Serve HTTPS (port 443) and redirect HTTP (port 80) → HTTPS. Requires cert.
    RedirectHttps,
    /// Derive from cert presence: `Http` if no cert, `Both` if cert present.
    #[default]
    Auto,
}

/// Resolved runtime frontend mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VhostMode {
    /// HTTP only.
    Http,
    /// HTTPS only.
    Https,
    /// Both HTTP and HTTPS.
    Both,
    /// HTTPS + HTTP redirect.
    RedirectHttps,
}

impl VhostMode {
    /// Whether this mode listens on the HTTP port.
    pub fn serves_http(self) -> bool {
        matches!(
            self,
            VhostMode::Http | VhostMode::Both | VhostMode::RedirectHttps
        )
    }

    /// Whether this mode listens on the HTTPS port.
    pub fn serves_https(self) -> bool {
        matches!(
            self,
            VhostMode::Https | VhostMode::Both | VhostMode::RedirectHttps
        )
    }

    /// Whether HTTP requests should be redirected to HTTPS.
    pub fn redirects_http(self) -> bool {
        matches!(self, VhostMode::RedirectHttps)
    }
}

// ─── Pure functions ───────────────────────────────────────────────────────────

/// Parse a `vhost.yml` string into a [`VhostConfig`].
pub fn parse_config(yaml: &str) -> Result<VhostConfig> {
    let cfg: VhostConfig = serde_yaml::from_str(yaml)?;
    Ok(cfg)
}

/// Extract the subdomain label from a Host header value against a base domain.
///
/// Strip optional `:port` suffix, lowercase, require the host to end with
/// `.<base_domain>`, and validate the remaining label as `[a-z0-9-]+` (no dot,
/// not starting or ending with `-`). Returns `None` for any violation.
pub fn extract_subdomain(host: &str, base_domain: &str) -> Option<String> {
    // Strip port suffix.
    let host = match host.rfind(':') {
        Some(i) => {
            // Only strip if what follows is numeric (port).
            if host[i + 1..].chars().all(|c| c.is_ascii_digit()) {
                &host[..i]
            } else {
                host
            }
        }
        None => host,
    };
    let host = host.to_lowercase();
    let base = base_domain.to_lowercase();

    // Must end with ".<base_domain>".
    let suffix = format!(".{base}");
    let label = host.strip_suffix(suffix.as_str())?;

    // Label must be non-empty, no dot (single label only), valid chars.
    if label.is_empty() || label.contains('.') {
        return None;
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    if label.starts_with('-') || label.ends_with('-') {
        return None;
    }
    Some(label.to_string())
}

/// Outcome of a route decision.
#[derive(Debug, PartialEq)]
pub enum RouteDecision {
    /// Accept; `headers` is the merged header list to inject (may be empty).
    Accept {
        /// Resolved headers to inject on the first request head.
        request_headers: Vec<(String, String)>,
        /// Resolved headers to inject on the first response head.
        response_headers: Vec<(String, String)>,
    },
    /// Reject with the given human-readable reason.
    Reject {
        /// Human-readable rejection reason sent back to the client.
        reason: String,
    },
}

/// Decide whether `(subdomain, client_id)` may register, and compute inject headers.
pub fn resolve_route(cfg: &VhostConfig, subdomain: &str, client_id: &str) -> RouteDecision {
    let reservation = cfg
        .reservations
        .iter()
        .find(|r| r.subdomain.to_lowercase() == subdomain.to_lowercase());

    match reservation {
        Some(res) if res.client_id != client_id => RouteDecision::Reject {
            reason: format!("subdomain '{subdomain}' is reserved for a different client"),
        },
        Some(res) => {
            let request_headers = merge_headers(&cfg.default_headers, &res.headers);
            let response_headers =
                merge_headers(&cfg.default_response_headers, &res.response_headers);
            RouteDecision::Accept {
                request_headers,
                response_headers,
            }
        }
        None => {
            // Unreserved: accept with default headers only.
            let request_headers = merge_headers(&cfg.default_headers, &BTreeMap::new());
            let response_headers = merge_headers(&cfg.default_response_headers, &BTreeMap::new());
            RouteDecision::Accept {
                request_headers,
                response_headers,
            }
        }
    }
}

/// Merge default headers with per-subdomain headers; per-subdomain wins on conflict.
pub fn merge_headers(
    defaults: &BTreeMap<String, String>,
    per_sub: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in defaults {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in per_sub {
        merged.insert(k.clone(), v.clone());
    }
    merged.into_iter().collect()
}

/// Resolve the runtime frontend mode from config + cert presence.
///
/// Returns an error if the configured mode requires a cert but none is present.
pub fn resolve_mode(cfg: &VhostConfig, cert_present: bool) -> Result<VhostMode> {
    let mode = match cfg.mode {
        VhostModeCfg::Http => VhostMode::Http,
        VhostModeCfg::Https => {
            if !cert_present {
                bail!("vhost mode 'https' requires a certificate (--cert-file / key_file in vhost.yml)");
            }
            VhostMode::Https
        }
        VhostModeCfg::Both => {
            if !cert_present {
                bail!("vhost mode 'both' requires a certificate");
            }
            VhostMode::Both
        }
        VhostModeCfg::RedirectHttps => {
            if !cert_present {
                bail!("vhost mode 'redirect-https' requires a certificate");
            }
            VhostMode::RedirectHttps
        }
        VhostModeCfg::Auto => {
            if cert_present {
                VhostMode::Both
            } else {
                VhostMode::Http
            }
        }
    };
    Ok(mode)
}

/// Whether a usable TLS certificate is configured: both the chain and the key
/// must be present. Single source of truth for the cert-present predicate.
pub fn cert_present(cfg: &VhostConfig) -> bool {
    cfg.cert_file.is_some() && cfg.key_file.is_some()
}

/// Compute effective admin-display HTTPS flags for a vhost entry.
///
/// When the entry's policy is `None`, inherit the global mode. When `Some(p)`,
/// resolve the policy against vhost capability (can only serve HTTPS if mode
/// serves it AND cert is present).
pub fn vhost_display_flags(
    policy: Option<crate::shared::HttpsPolicy>,
    mode: VhostMode,
    vhost_capable: bool,
) -> (bool, bool) {
    match policy {
        None => (mode.serves_https(), mode.redirects_http()),
        Some(p) => {
            let (https, force_https, _downgraded) =
                crate::shared::resolve_https_policy(p, vhost_capable);
            (https, force_https)
        }
    }
}

/// Decide whether to 308-redirect a plain HTTP request to HTTPS for a vhost entry.
///
/// When the entry's policy is `None`, use global mode (byte-identical to today).
/// When `Some(Redirect)`, redirect only if vhost is capable. Other policies never redirect.
pub fn should_redirect(
    entry_policy: Option<crate::shared::HttpsPolicy>,
    global_mode: VhostMode,
    vhost_capable: bool,
) -> bool {
    match entry_policy {
        None => global_mode.redirects_http(),
        Some(p) => matches!(p, crate::shared::HttpsPolicy::Redirect) && vhost_capable,
    }
}

/// Compute the public URL(s) for a registered vhost subdomain.
///
/// Port is omitted from the URL when it matches the scheme default (80/443).
pub fn public_urls(
    subdomain: &str,
    base_domain: &str,
    mode: VhostMode,
    http_port: u16,
    https_port: u16,
) -> (Option<String>, Option<String>) {
    let http_url = if mode.serves_http() && !mode.redirects_http() {
        let port_str = if http_port == 80 {
            String::new()
        } else {
            format!(":{http_port}")
        };
        Some(format!("http://{subdomain}.{base_domain}{port_str}"))
    } else {
        None
    };

    let https_url = if mode.serves_https() {
        let port_str = if https_port == 443 {
            String::new()
        } else {
            format!(":{https_port}")
        };
        Some(format!("https://{subdomain}.{base_domain}{port_str}"))
    } else {
        None
    };

    (http_url, https_url)
}

// ─── Registry types ───────────────────────────────────────────────────────────

/// One registered vhost provider: its carrier pool + resolved inject headers.
pub struct VhostEntry {
    /// Carrier pool for this provider (may have >1 connection with `--carriers`).
    pub pool: Arc<CarrierPool>,
    /// Resolved header list to inject on the first request head (may be empty).
    pub request_headers: Vec<(String, String)>,
    /// Resolved header list to inject on the first response head (may be empty).
    pub response_headers: Vec<(String, String)>,
    /// Pool of live QUIC direct connections to the provider. Empty until at least
    /// one is established; a provider with `--carriers N --udp` fills it with up to
    /// `N` connections so proxied requests spread across them (per-connection
    /// crypto/congestion parallelism), exactly as the TCP carrier pool does.
    #[cfg(feature = "udp")]
    pub direct: DirectPool,
    /// Number of proxied requests that successfully opened a direct QUIC stream.
    #[cfg(feature = "udp")]
    pub direct_stream_opens: AtomicU64,
    /// Live count of connections currently proxied through this vhost subdomain.
    pub active: Arc<AtomicUsize>,
    /// Whether this provider requested access logging with real caller IP forwarding.
    /// Wired from `HelloVhost.webserver_log`.
    pub webserver_log: bool,
    /// Per-tunnel HTTPS policy. `None` = inherit the server default (global mode).
    pub https_policy: Option<crate::shared::HttpsPolicy>,
    /// Whether the server originates a TLS client session toward the provider's
    /// local backend (the backend is itself an HTTPS server). When `true`, the
    /// relay wraps the provider-facing `LinkStream` in a `tokio-rustls` client
    /// before splicing; certificate verification is skipped (accept-any). `false`
    /// keeps the plaintext backend path byte-identical to the pre-feature server.
    pub backend_tls: bool,
    /// SNI/server name for the backend TLS ClientHello. `None` ⇒ `localhost`.
    /// Ignored when `backend_tls` is `false`.
    pub backend_tls_sni: Option<String>,
    // ── Execution-info fields (parity with the public `TunnelView`) ───────────
    // These let the admin Vhost section present the same columns as Tunnels.
    // `VhostEntry` is self-sufficient here (no admin-registry join, see
    // docs/frontend/ADMIN_VHOST_PARITY_PLAN.md).
    /// Remote address of the provider's control connection.
    pub peer: SocketAddr,
    /// When this provider registered (for an uptime readout).
    pub since: Instant,
    /// Free-form operator note (`--notes`).
    pub notes: Option<String>,
    /// Whether the provider enforces HTTP Basic auth itself (display only).
    pub basic_auth: bool,
    /// Whether the provider requested the QUIC direct data path (`--udp`).
    pub udp: bool,
    /// Whether the provider runs with `--auto-reconnect` (display only).
    pub auto_reconnect: bool,
    /// Provider's local target host (`-l/--local-host` of the forwarded service).
    pub local_host: Option<String>,
    /// Provider's local target port (0 = unknown).
    pub local_port: u16,
    /// Relay bytes sent toward the provider (server→provider), summed off the hot
    /// path from the totals `copy_bidirectional_with_sizes` returns.
    pub relay_tx_bytes: Arc<AtomicU64>,
    /// Relay bytes received from the provider (provider→server).
    pub relay_rx_bytes: Arc<AtomicU64>,
    /// Server-side HTTP Basic auth enforced on the gateway's behalf (SSH
    /// ingress only — a native `bore vhost` provider enforces its own auth
    /// client-side before proxying, see [`VhostEntry::basic_auth`]; an SSH
    /// `-R` forward has no such client process, so the gateway must gate the
    /// inbound request itself before opening a link toward the peer).
    pub gateway_basic_auth: Option<BasicAuth>,
    /// Client implementation (Bore native or SSH gateway).
    pub transport: crate::admin::Transport,
    /// Identity presented by SSH client authentication (SSH only; None for native Bore).
    pub identity: Option<String>,
}

/// Upper bound on QUIC direct connections pooled per vhost subdomain. The provider
/// is authenticated (token derived from the tunnel secret), so this only guards
/// against an accidental connection storm, not an untrusted peer.
#[cfg(feature = "udp")]
pub const MAX_DIRECT_CARRIERS: usize = 32;

/// Resolve the number of QUIC direct carriers a provider should open, clamped to
/// `[1, MAX_DIRECT_CARRIERS]`.
///
/// The server installs at most [`MAX_DIRECT_CARRIERS`] connections per subdomain
/// and closes any surplus (VH-2). If the provider opened more than that, each
/// surplus connection would be closed by the server, trigger a renewal, and be
/// reopened — an endless open/close churn that never converges. Clamping the
/// provider's target to the same cap means every connection it opens is kept, so
/// the pool reaches a stable steady state.
#[cfg(feature = "udp")]
pub fn clamp_direct_carriers(requested: u16) -> u16 {
    requested.clamp(1, MAX_DIRECT_CARRIERS as u16)
}

/// Round-robin pool of live QUIC direct connections to one vhost provider.
///
/// The UDP analog of [`CarrierPool`]: each member is an independent QUIC
/// connection, and proxied requests are spread across them so per-connection AEAD
/// and congestion-control work parallelizes across cores instead of funneling
/// through a single connection. Members are added on connect and removed when
/// their QUIC connection closes (keyed by a monotonic id so a stale close-monitor
/// never evicts a newer member).
#[cfg(feature = "udp")]
#[derive(Default)]
pub struct DirectPool {
    conns: RwLock<Vec<DirectMember>>,
    next: AtomicU64,
    ids: AtomicU64,
}

#[cfg(feature = "udp")]
struct DirectMember {
    id: u64,
    conn: crate::holepunch::DirectConn,
}

#[cfg(feature = "udp")]
impl DirectPool {
    /// Install a connection, returning its id (used by [`DirectPool::remove`] when
    /// the connection closes). Returns `None` when the pool is already full, so the
    /// caller can drop the excess connection.
    pub fn install(&self, conn: crate::holepunch::DirectConn) -> Option<u64> {
        let mut conns = self.conns.write().unwrap();
        if conns.len() >= MAX_DIRECT_CARRIERS {
            return None;
        }
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        conns.push(DirectMember { id, conn });
        Some(id)
    }

    /// Remove the connection with `id`, if still present.
    pub fn remove(&self, id: u64) {
        self.conns.write().unwrap().retain(|m| m.id != id);
    }

    /// Pick the next connection round-robin, or `None` when the pool is empty.
    pub fn pick(&self) -> Option<crate::holepunch::DirectConn> {
        let conns = self.conns.read().unwrap();
        if conns.is_empty() {
            return None;
        }
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) % conns.len() as u64) as usize;
        Some(conns[idx].conn.clone())
    }

    /// Number of live pooled connections.
    pub fn len(&self) -> usize {
        self.conns.read().unwrap().len()
    }

    /// Whether the pool currently has no live connections.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Close and remove every pooled connection. Used when an owning registry
    /// entry ends so detached close monitors cannot keep QUIC carriers alive.
    pub fn close_all(&self) {
        let mut conns = self.conns.write().unwrap();
        for member in conns.iter() {
            member.conn.close();
        }
        conns.clear();
    }
}

/// Registry of live vhost providers, keyed by subdomain label.
pub type VhostRegistry = Arc<DashMap<String, Arc<VhostEntry>>>;

/// Pending vhost direct-path nonces keyed by subdomain.
pub type PendingVhostUdp = Arc<DashMap<String, [u8; UDP_NONCE_LEN]>>;

/// Shared hot-swappable vhost config behind a read-write lock.
pub type SharedVhostConfig = Arc<RwLock<Arc<VhostConfig>>>;

/// Removes a vhost provider registration when the provider connection ends.
struct Deregister {
    registry: VhostRegistry,
    pending_udp: Option<PendingVhostUdp>,
    subdomain: String,
}

impl Drop for Deregister {
    fn drop(&mut self) {
        self.registry.remove(&self.subdomain);
        if let Some(pending) = &self.pending_udp {
            pending.remove(&self.subdomain);
        }
    }
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Upper bound on the server-originated TLS handshake to an HTTPS backend
/// (`entry.backend_tls`). A backend that is slow, non-TLS, or unreachable must
/// fail the proxied connection within this window rather than hang it.
const BACKEND_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(feature = "udp")]
fn new_nonce() -> [u8; UDP_NONCE_LEN] {
    use ring::rand::{SecureRandom, SystemRandom};

    let mut nonce = [0u8; UDP_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("system CSPRNG must not fail");
    nonce
}

#[cfg(feature = "udp")]
async fn send_vhost_udp_offer(
    control: &mut Delimited<mux::Stream>,
    subdomain: &str,
    port: u16,
    pending_vhost_udp: &PendingVhostUdp,
    tuning: UdpDirectTuning,
) -> Result<()> {
    let nonce = new_nonce();
    pending_vhost_udp.insert(subdomain.to_string(), nonce);
    control
        .send(ServerMessage::VhostUdp {
            port,
            nonce,
            tuning,
        })
        .await?;
    info!(subdomain, port, "offered vhost direct udp path");
    Ok(())
}

/// Server side: register this connection as the vhost provider for `subdomain`.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "udp"), allow(unused_variables))]
pub async fn serve_vhost_provider(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    registry: VhostRegistry,
    vhost_config: SharedVhostConfig,
    subdomain: String,
    client_id: String,
    admin: AdminRegistry,
    peer: SocketAddr,
    notes: Option<String>,
    basic_auth: bool,
    udp: bool,
    webserver_log: bool,
    auto_reconnect: bool,
    pending_carriers: PendingCarriers,
    max_carriers: u16,
    carriers: u16,
    server_udp_enabled: bool,
    vhost_quic_port: u16,
    pending_vhost_udp: PendingVhostUdp,
    _secret: Option<String>,
    udp_tuning: UdpDirectTuning,
    local_host: Option<String>,
    local_port: u16,
    https_policy: Option<crate::shared::HttpsPolicy>,
    backend_tls: bool,
    backend_tls_sni: Option<String>,
) -> Result<()> {
    // Validate against live config (resolve_route checks reservations).
    let cfg = vhost_config.read().unwrap().clone();
    let (request_headers, response_headers) = match resolve_route(&cfg, &subdomain, &client_id) {
        RouteDecision::Accept {
            request_headers,
            response_headers,
        } => (request_headers, response_headers),
        RouteDecision::Reject { reason } => {
            warn!(%subdomain, %reason, "vhost registration rejected");
            control.send(ServerMessage::Error(reason)).await?;
            return Ok(());
        }
    };

    // Compute vhost capability: can serve HTTPS if mode allows it and cert is present.
    let mode = resolve_mode(&cfg, cert_present(&cfg)).unwrap_or(VhostMode::Http);
    let vhost_capable = mode.serves_https();

    // Atomic insert: reject if subdomain already live.
    let pool = match registry.entry(subdomain.clone()) {
        Entry::Occupied(_) => {
            warn!(%subdomain, "vhost subdomain already in use");
            control
                .send(ServerMessage::Error(format!(
                    "subdomain '{subdomain}' in use"
                )))
                .await?;
            return Ok(());
        }
        Entry::Vacant(slot) => {
            let pool = Arc::new(CarrierPool::new(mux::LinkOpener::Mux(opener)));
            let entry = Arc::new(VhostEntry {
                pool: Arc::clone(&pool),
                request_headers,
                response_headers,
                #[cfg(feature = "udp")]
                direct: DirectPool::default(),
                #[cfg(feature = "udp")]
                direct_stream_opens: AtomicU64::new(0),
                active: Arc::new(AtomicUsize::new(0)),
                webserver_log,
                https_policy,
                backend_tls,
                backend_tls_sni,
                peer,
                since: Instant::now(),
                notes: notes.clone(),
                basic_auth,
                udp,
                auto_reconnect,
                local_host: local_host.clone(),
                local_port,
                relay_tx_bytes: Arc::new(AtomicU64::new(0)),
                relay_rx_bytes: Arc::new(AtomicU64::new(0)),
                gateway_basic_auth: None,
                transport: crate::admin::Transport::Bore,
                identity: None,
            });
            slot.insert(entry);
            pool
        }
    };
    let _guard = Deregister {
        registry: registry.clone(),
        pending_udp: if udp {
            Some(pending_vhost_udp.clone())
        } else {
            None
        },
        subdomain: subdomain.clone(),
    };

    // Compute effective admin display flags.
    let (adm_https, adm_force_https) = vhost_display_flags(https_policy, mode, vhost_capable);

    let _admin_reg = admin.register(NewEntry {
        role: Role::Vhost,
        peer,
        secret_id: Some(subdomain.clone()),
        public_port: None,
        notes,
        basic_auth,
        https: adm_https,
        force_https: adm_force_https,
        carriers: 0,
        auto_reconnect,
        webserver_log,
        udp,
        vpn_relay_only: false,
        vpn_pin_mtu: false,
        vpn_mtu: None,
        vpn_forward_accept: false,
        vpn_nat_masquerade: false,
        vpn_route_policy: None,
        vpn_advertised: vec![],
        vpn_nat_udp_port: None,
        local_proxy_port: None,
        local_host,
        local_port: (local_port != 0).then_some(local_port),
        nat_udp_preferred_port: None,
        nat_udp_release_timeout: None,
        stun_server: None,
        upnp: false,
        try_port_prediction: false,
        max_conns: None,
        transport: crate::admin::Transport::Bore,
        identity: None,
    });

    // Compute and send the public URLs based on current config.
    let (http_url, https_url) = public_urls(
        &subdomain,
        &cfg.base_domain,
        mode,
        cfg.http_port,
        cfg.https_port,
    );
    control
        .send(ServerMessage::VhostReady {
            http_url,
            https_url,
        })
        .await?;
    info!(%subdomain, "vhost provider registered");

    // Carrier pool setup (same pattern as secret provider).
    let effective = carriers.clamp(1, max_carriers.max(1));
    let mut carrier_rx = if carriers > 1 {
        let extra = effective - 1;
        let token = Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        pending_carriers.insert(token.clone(), tx);
        control
            .send(ServerMessage::CarrierToken {
                token: token.clone(),
                extra,
            })
            .await?;
        info!(%subdomain, extra, "vhost carrier pool offered");
        Some((rx, TokenGuard::new(pending_carriers.clone(), token)))
    } else {
        None
    };

    #[cfg(feature = "udp")]
    if udp && server_udp_enabled {
        send_vhost_udp_offer(
            &mut control,
            &subdomain,
            vhost_quic_port,
            &pending_vhost_udp,
            udp_tuning,
        )
        .await?;
    }
    #[cfg(feature = "udp")]
    if udp && !server_udp_enabled {
        debug!(%subdomain, "vhost udp requested but server udp is disabled; using TCP relay");
    }
    #[cfg(not(feature = "udp"))]
    if udp {
        debug!(%subdomain, "vhost udp requested but binary was built without udp support; using TCP relay");
    }

    // 2.3: Send the HTTPS downgrade warning LAST, after every one-shot handshake
    // message (VhostReady + CarrierToken). The client's one-shot vhost reads
    // (client.rs VhostReady/CarrierToken) BAIL on an unexpected Warning; only the
    // main control loop handles it non-fatally. The vhost UDP offer (if any) is
    // also consumed by the client's main loop, so ordering against it is moot.
    if let Some(p) = https_policy {
        if matches!(
            p,
            crate::shared::HttpsPolicy::On | crate::shared::HttpsPolicy::Redirect
        ) && !vhost_capable
        {
            let msg = format!(
                "vhost server not configured for HTTPS (mode={mode:?}, cert={}); serving this subdomain over HTTP",
                if cert_present(&cfg) { "present" } else { "missing" }
            );
            warn!(%subdomain, "{msg}");
            let _ = control.send(ServerMessage::Warning(msg)).await;
        }
    }

    // Heartbeat loop until the provider disconnects.
    let mut hb = interval(HEARTBEAT_INTERVAL);
    hb.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = hb.tick() => {
                if control.send(ServerMessage::Heartbeat).await.is_err() {
                    return Ok(());
                }
            }
            message = control.recv() => {
                match message? {
                    Some(ClientMessage::HelloVhost { .. })
                    | Some(ClientMessage::HelloSecret { .. })
                    | Some(ClientMessage::ConnectSecret { .. })
                    | Some(ClientMessage::Authenticate(_)) => {
                        warn!(%subdomain, "unexpected message from vhost provider");
                    }
                    Some(ClientMessage::VhostUdpRenew { subdomain: renew_subdomain }) => {
                        if renew_subdomain != subdomain {
                            warn!(%subdomain, requested = %renew_subdomain, "unexpected vhost udp renew request");
                        }
                        #[cfg(feature = "udp")]
                        if renew_subdomain == subdomain && udp && server_udp_enabled {
                            send_vhost_udp_offer(
                                &mut control,
                                &subdomain,
                                vhost_quic_port,
                                &pending_vhost_udp,
                                udp_tuning,
                            )
                            .await?;
                        }
                        #[cfg(any(not(feature = "udp"), feature = "udp"))]
                        if renew_subdomain == subdomain && (!udp || !server_udp_enabled) {
                            debug!(%subdomain, "ignoring vhost udp renew request while udp is disabled");
                        }
                    }
                    Some(_) => warn!(%subdomain, "unexpected message from vhost provider"),
                    None => return Ok(()),
                }
            }
            joined = crate::pool::recv_carrier(carrier_rx.as_mut()) => {
                if let Some(carrier) = joined {
                    if pool.push(carrier, effective as usize) {
                        info!(%subdomain, size = pool.len(), "vhost carrier joined pool");
                    }
                }
            }
        }
    }
}

/// Splice one inbound public HTTP(S) connection to a registered vhost provider.
///
/// `entry` is the already-resolved registry entry (carrier pool + inject headers),
/// cloned out by the caller so no DashMap guard is held across an await. `head` is
/// the already-read request head, forwarded (with header injection if the entry has
/// any configured) before the bidirectional splice begins. `addr` is the remote
/// caller's address (for logging); `subdomain` and `fqdn` are for access logging.
#[allow(clippy::too_many_arguments)]
pub async fn relay_vhost(
    mut public: impl AsyncRead + AsyncWrite + Unpin,
    addr: SocketAddr,
    entry: &VhostEntry,
    head: Vec<u8>,
    grx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    gtx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    subdomain: &str,
    fqdn: &str,
    log_ctx: LogContext,
) -> Result<()> {
    // Server-side Basic auth (SSH ingress only, see `VhostEntry::gateway_basic_auth`).
    // Gate before opening any provider link so an unauthorized caller never
    // consumes a carrier.
    if let Some(auth) = &entry.gateway_basic_auth {
        if !auth.authorized(&head) {
            public.write_all(basicauth::UNAUTHORIZED.as_bytes()).await?;
            public.flush().await?;
            let _ = public.shutdown().await;
            return Ok(());
        }
    }

    // Caller IP forwarding (Phase 3), computed up front so every branch below
    // can announce readiness with it.
    let forward_ip = if entry.webserver_log {
        Some(addr.ip().to_string())
    } else {
        None
    };
    let mut provider: mux::LinkStream = {
        #[cfg(feature = "udp")]
        {
            // In vhost UDP the server opens the QUIC streams and the provider
            // accepts them. Pick a direct connection round-robin from the pool; if
            // none is live or opening a stream fails, fall back per-request to the
            // existing TCP carrier pool.
            let direct = entry.direct.pick();
            match direct {
                Some(direct) => match direct.open_stream().await {
                    Ok(mut stream) => {
                        entry
                            .direct_stream_opens
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        mux::write_stream_ready(&mut stream, forward_ip.as_deref()).await?;
                        Box::new(stream)
                    }
                    Err(err) => {
                        debug!(%err, "vhost QUIC open_stream failed; using TCP carrier");
                        let opener = entry.pool.pick().context("no live vhost carrier")?;
                        opener
                            .open_ready(forward_ip.as_deref(), Some(addr))
                            .await
                            .context("vhost provider unavailable")?
                    }
                },
                None => {
                    let opener = entry.pool.pick().context("no live vhost carrier")?;
                    opener
                        .open_ready(forward_ip.as_deref(), Some(addr))
                        .await
                        .context("vhost provider unavailable")?
                }
            }
        }
        #[cfg(not(feature = "udp"))]
        {
            let opener = entry.pool.pick().context("no live vhost carrier")?;
            opener
                .open_ready(forward_ip.as_deref(), Some(addr))
                .await
                .context("vhost provider unavailable")?
        }
    };

    // Backend TLS origination (I-1/D6): when the tunnelled backend is itself an
    // HTTPS/TLS listener, the server (the TLS client endpoint) wraps the provider
    // link in a client TLS session BEFORE any HTTP head is written, so the request
    // rides ciphertext to the backend and the response is decrypted here for
    // header injection. Gated on `entry.backend_tls`; with the flag off this block
    // is skipped and the path below is byte-identical to the plaintext relay.
    // `provider` stays spliced in ONE task (I-3): the wrap consumes it and rebinds
    // the same variable, never `tokio::io::split`.
    if entry.backend_tls {
        let connector = crate::transport::insecure_tls_connector()?;
        let sni = entry.backend_tls_sni.as_deref().unwrap_or("localhost");
        let server_name = crate::transport::backend_server_name(sni)?;
        let tls = tokio::time::timeout(
            BACKEND_TLS_HANDSHAKE_TIMEOUT,
            connector.connect(server_name, provider),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "backend TLS handshake timed out",
            )
        })?
        .map_err(|e| std::io::Error::other(format!("backend TLS handshake failed: {e}")))?;
        provider = Box::new(tls) as mux::LinkStream;
    }

    // Keep a copy of the head for logging (before it's moved/rewritten).
    let head_for_logging = head.clone();
    let request_head = if entry.request_headers.is_empty() {
        // Zero-overhead pure-splice path: forward the already-read head as-is.
        head
    } else {
        rewrite_head(&head, &entry.request_headers)
    };
    provider.write_all(&request_head).await?;

    let _guard = crate::admin::ActiveGuard::new(active);

    if entry.response_headers.is_empty() {
        let buf = proxy_buffer_size();
        // Count bytes LIVE as they flow (per-subdomain + global), not only on
        // close, so the admin Vhost TX/RX columns update for open connections.
        let counted = crate::shared::CountingStream::new(
            public,
            entry.relay_rx_bytes.clone(),
            entry.relay_tx_bytes.clone(),
            grx.clone(),
            gtx.clone(),
        );
        // Wrap in tap if logger is present (Phase 2.2 — I-WL1 guard).
        let result = if let Some(ref logger) = log_ctx.logger {
            let tx = logger.sender_for(
                fqdn,
                crate::weblog::PathLayout::SubdomainFolder {
                    subdomain: subdomain.to_string(),
                },
            );
            let mut tap = crate::weblog::HttpAccessTap::new(
                counted,
                Some(addr.ip().to_string()),
                tx,
                log_ctx.dropped.clone(),
            );

            // The request head was already consumed by handle_http/handle_https before
            // the tap was attached. Parse it here and inject into the tap so it can be
            // paired with the response (I-WL2).
            // BUG-1 fix: also compute body_len and pass it to prime the parser's body-skip state.
            if let Ok(req_head) = std::str::from_utf8(&head_for_logging) {
                let mut headers = [httparse::EMPTY_HEADER; 64];
                let mut req = httparse::Request::new(&mut headers);
                if let Ok(httparse::Status::Complete(_)) = req.parse(req_head.as_bytes()) {
                    let method = req.method.unwrap_or("").to_string();
                    let path = req.path.unwrap_or("").to_string();
                    let version = match req.version {
                        Some(0) => "HTTP/1.0".to_string(),
                        Some(1) => "HTTP/1.1".to_string(),
                        _ => "HTTP/?".to_string(),
                    };
                    let mut referer = None;
                    let mut user_agent = None;
                    for h in req.headers.iter().filter(|h| !h.name.is_empty()) {
                        match h.name.to_lowercase().as_str() {
                            "referer" => {
                                referer = String::from_utf8(h.value.to_vec()).ok();
                            }
                            "user-agent" => {
                                user_agent = String::from_utf8(h.value.to_vec()).ok();
                            }
                            _ => {}
                        }
                    }
                    let body_len = crate::weblog::body_length(req.headers);
                    tap.inject_pending_request(
                        method, path, version, referer, user_agent, body_len,
                    );
                }
            }

            tokio::io::copy_bidirectional_with_sizes(&mut tap, &mut provider, buf, buf).await?
        } else {
            let mut counted = counted;
            tokio::io::copy_bidirectional_with_sizes(&mut counted, &mut provider, buf, buf).await?
        };
        // Byte counts are accumulated LIVE by `CountingStream` (per-subdomain +
        // global) as bytes flow; nothing to add on close.
        let _ = result;
        return Ok(());
    }

    // BUG-2 fix: thread logging context and request head to relay_response_injected.
    let log_context = log_ctx.logger.as_ref().map(|logger| ResponseLogContext {
        head: head_for_logging,
        logger: logger.clone(),
        dropped: log_ctx.dropped.clone(),
        addr,
        subdomain: subdomain.to_string(),
        fqdn: fqdn.to_string(),
    });

    // Count bytes LIVE on the response-header-injection path too (this path
    // previously never updated the per-subdomain/global counters at all).
    // Wrapping the public side captures both directions: request bytes are read
    // from it, response bytes are written to it.
    let counted_public = crate::shared::CountingStream::new(
        public,
        entry.relay_rx_bytes.clone(),
        entry.relay_tx_bytes.clone(),
        grx.clone(),
        gtx.clone(),
    );
    relay_response_injected(
        counted_public,
        provider,
        &entry.response_headers,
        log_context,
    )
    .await?;
    Ok(())
}

async fn relay_response_injected(
    public: impl AsyncRead + AsyncWrite + Unpin,
    provider: impl AsyncRead + AsyncWrite + Unpin,
    inject: &[(String, String)],
    log_context: Option<ResponseLogContext>,
) -> Result<()> {
    let (mut public_read, mut public_write) = tokio::io::split(public);
    let (mut provider_read, mut provider_write) = tokio::io::split(provider);

    let forward_request =
        async { copy_one_direction_with_shutdown(&mut public_read, &mut provider_write).await };

    let forward_response = async {
        let response_head = read_head_async(&mut provider_read).await?;
        if !response_head.is_empty() {
            let rewritten = rewrite_head(&response_head, inject);
            public_write.write_all(&rewritten).await?;
            public_write.flush().await?;
        }

        // BUG-2 fix: emit one access log entry if logger is present.
        // MVP: only the first request on a response-rewrite keep-alive connection is logged.
        if let Some(log_ctx) = log_context {
            // Parse request line and headers from the already-read head.
            if let Ok(req_head_str) = std::str::from_utf8(&log_ctx.head) {
                let mut headers = [httparse::EMPTY_HEADER; 64];
                let mut req = httparse::Request::new(&mut headers);
                if let Ok(httparse::Status::Complete(_)) = req.parse(req_head_str.as_bytes()) {
                    let method = req.method.unwrap_or("").to_string();
                    let path = req.path.unwrap_or("").to_string();
                    let version = match req.version {
                        Some(0) => "HTTP/1.0".to_string(),
                        Some(1) => "HTTP/1.1".to_string(),
                        _ => "HTTP/?".to_string(),
                    };
                    let mut referer = None;
                    let mut user_agent = None;
                    for h in req.headers.iter().filter(|h| !h.name.is_empty()) {
                        match h.name.to_lowercase().as_str() {
                            "referer" => {
                                referer = String::from_utf8(h.value.to_vec()).ok();
                            }
                            "user-agent" => {
                                user_agent = String::from_utf8(h.value.to_vec()).ok();
                            }
                            _ => {}
                        }
                    }

                    // Parse response status code from response_head.
                    let mut resp_headers = [httparse::EMPTY_HEADER; 64];
                    let mut resp = httparse::Response::new(&mut resp_headers);
                    if let Ok(httparse::Status::Complete(_)) = resp.parse(&response_head) {
                        let status = resp.code.unwrap_or(0);
                        let rec = crate::weblog::AccessRecord::http(
                            time::OffsetDateTime::now_utc(),
                            Some(log_ctx.addr.ip().to_string()),
                            method,
                            path,
                            version,
                            status,
                            0, // bytes_sent: best-effort 0 (counting would require copying body path)
                            referer,
                            user_agent,
                        );
                        let tx = log_ctx.logger.sender_for(
                            &log_ctx.fqdn,
                            crate::weblog::PathLayout::SubdomainFolder {
                                subdomain: log_ctx.subdomain.clone(),
                            },
                        );
                        crate::weblog::try_log(&tx, rec, &log_ctx.dropped);
                    }
                }
            }
        }

        copy_one_direction_with_shutdown(&mut provider_read, &mut public_write).await
    };

    let (_, _) = tokio::try_join!(forward_request, forward_response)?;
    Ok(())
}

async fn copy_one_direction_with_shutdown<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; proxy_buffer_size()];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        writer.write_all(&buf[..read]).await?;
        writer.flush().await?;
    }
}

/// Rewrite an HTTP head: insert/override configured headers, keep the rest.
///
/// Only modifies headers whose names appear in `inject`; preserves all other
/// headers and the request line unchanged. Operates on raw bytes (no lossy UTF-8
/// conversion) so header values with non-ASCII bytes survive intact.
///
/// The head buffer may contain bytes *past* the header terminator — request-body
/// bytes (or a pipelined follow-up) that arrived in the same read. Those are
/// preserved verbatim after the rewritten headers. If the buffer has no complete
/// `\r\n\r\n` terminator (e.g. it was truncated at the read cap), no rewrite is
/// attempted and the bytes are returned unchanged so the stream never desyncs.
///
/// **MVP limitation:** only the first parsed HTTP head on that direction is rewritten.
/// Subsequent keep-alive heads are spliced raw.
pub fn rewrite_head(head: &[u8], inject: &[(String, String)]) -> Vec<u8> {
    // Locate the end of the header block. Everything after it is body bytes that
    // must be forwarded as-is; without a complete terminator, do not rewrite.
    let Some(sep) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
        return head.to_vec();
    };
    let headers_region = &head[..sep];
    let rest = &head[sep + 4..];

    let mut out = Vec::with_capacity(head.len() + 256);
    // Split on LF and strip a trailing CR, so each piece is one header (or the
    // request line) with its line ending removed.
    let mut lines = headers_region.split(|&b| b == b'\n').map(trim_cr);

    // Keep the request line intact.
    if let Some(request_line) = lines.next() {
        out.extend_from_slice(request_line);
        out.extend_from_slice(b"\r\n");
    }

    // Keep existing headers that are NOT overridden (case-insensitive name match).
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let should_drop = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let name = trim_ascii(&line[..colon]);
                inject
                    .iter()
                    .any(|(k, _)| k.as_bytes().eq_ignore_ascii_case(name))
            }
            None => false,
        };
        if !should_drop {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }

    // Append the injected headers, then close the header block and replay any
    // already-read body bytes verbatim.
    for (name, value) in inject {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(rest);
    out
}

/// Strip a single trailing `\r` from a line (the LF was already split off).
fn trim_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// Trim leading/trailing ASCII whitespace from a byte slice.
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

// ─── Logging context ──────────────────────────────────────────────────────────

/// Context for access logging (Phase 2).
pub struct LogContext {
    /// Optional access logger for HTTP request/response logging.
    pub logger: Option<Arc<crate::weblog::AccessLogger>>,
    /// Shared dropped-record counter for all loggers.
    pub dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Logging context for response-header-injected relay path.
struct ResponseLogContext {
    /// Request head (already parsed in handle_http/handle_https).
    head: Vec<u8>,
    /// Logger reference.
    logger: Arc<crate::weblog::AccessLogger>,
    /// Dropped-record counter.
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Caller's socket address.
    addr: SocketAddr,
    /// Subdomain label.
    subdomain: String,
    /// FQDN for log file.
    fqdn: String,
}

// ─── Frontend handlers ────────────────────────────────────────────────────────

/// Handle one inbound HTTP connection on the vhost frontend port.
///
/// Reads the request head, extracts the subdomain from the Host header, and
/// relays the connection to the registered provider (with header injection if
/// configured). Returns a clean 502 when no provider is registered.
#[allow(clippy::too_many_arguments)]
pub async fn handle_http(
    mut stream: TcpStream,
    addr: SocketAddr,
    registry: &VhostRegistry,
    vhost_config: &Option<SharedVhostConfig>,
    mode: VhostMode,
    grx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    gtx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    log_ctx: LogContext,
) -> Result<()> {
    use tokio::time::timeout;

    let head = timeout(NETWORK_TIMEOUT, edge::read_request_head(&mut stream))
        .await
        .context("timed out reading HTTP request head")??;

    let cfg = vhost_config.as_ref().map(|c| c.read().unwrap().clone());
    let base_domain = cfg.as_deref().map(|c| c.base_domain.as_str()).unwrap_or("");

    let host = extract_host_from_head(&head);
    let sub = match host.and_then(|h| extract_subdomain(h, base_domain)) {
        Some(s) => s,
        None => {
            debug!(
                host = host.unwrap_or(""),
                "vhost http 502: no routable subdomain"
            );
            return send_bad_gateway(stream).await;
        }
    };

    // Single registry lookup: clone the entry out (pool + inject headers) so no
    // DashMap guard is held across the await in `relay_vhost`.
    let Some(entry) = registry.get(&sub).map(|e| Arc::clone(e.value())) else {
        // Unknown subdomain: use global mode for redirect decision (byte-identical to today).
        if mode.redirects_http() {
            let https_port = cfg.as_deref().map(|c| c.https_port).unwrap_or(443);
            // Reuse the head already read above; re-reading would block a real
            // (non-half-closing) client until the network timeout.
            edge::write_https_redirect(stream, &head, https_port, None).await?;
            return Ok(());
        }
        debug!(%sub, "vhost http 502: no provider registered");
        return send_bad_gateway(stream).await;
    };

    // Per-subdomain redirect decision: compute vhost capability and check entry policy.
    let vhost_capable = mode.serves_https();
    if should_redirect(entry.https_policy, mode, vhost_capable) {
        let https_port = cfg.as_deref().map(|c| c.https_port).unwrap_or(443);
        // Reuse the head already read above; re-reading would block a real
        // (non-half-closing) client until the network timeout.
        edge::write_https_redirect(stream, &head, https_port, None).await?;
        return Ok(());
    }

    let fqdn = host.unwrap_or("").to_string();
    relay_vhost(
        stream,
        addr,
        &entry,
        head,
        grx,
        gtx,
        Arc::clone(&entry.active),
        &sub,
        &fqdn,
        log_ctx,
    )
    .await
}

/// Handle one inbound HTTPS connection on the vhost frontend port.
///
/// Terminates TLS with the wildcard acceptor, then routes identically to
/// [`handle_http`] on the decrypted stream — same `Host`-header → subdomain
/// extraction against the configured base domain, same single registry lookup.
#[allow(clippy::too_many_arguments)]
pub async fn handle_https(
    stream: TcpStream,
    addr: SocketAddr,
    registry: &VhostRegistry,
    vhost_config: &Option<SharedVhostConfig>,
    vhost_tls: &Arc<RwLock<Option<Arc<TlsAcceptor>>>>,
    grx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    gtx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    log_ctx: LogContext,
) -> Result<()> {
    use tokio::time::timeout;

    let acceptor = vhost_tls.read().unwrap().clone();
    let acceptor = match acceptor {
        Some(a) => a,
        None => {
            warn!("HTTPS vhost connection but no TLS acceptor configured");
            return Ok(());
        }
    };

    let mut tls_stream = acceptor
        .accept(stream)
        .await
        .context("TLS handshake failed")?;

    let head = timeout(NETWORK_TIMEOUT, read_head_async(&mut tls_stream))
        .await
        .context("timed out reading HTTPS request head")??;

    let cfg = vhost_config.as_ref().map(|c| c.read().unwrap().clone());
    let base_domain = cfg.as_deref().map(|c| c.base_domain.as_str()).unwrap_or("");

    let host = extract_host_from_head(&head);
    let sub = match host.and_then(|h| extract_subdomain(h, base_domain)) {
        Some(s) => s,
        None => {
            debug!(
                host = host.unwrap_or(""),
                "vhost https 502: no routable subdomain"
            );
            return send_bad_gateway(tls_stream).await;
        }
    };

    let Some(entry) = registry.get(&sub).map(|e| Arc::clone(e.value())) else {
        debug!(%sub, "vhost https 502: no provider registered");
        return send_bad_gateway(tls_stream).await;
    };

    let fqdn = host.unwrap_or("").to_string();
    relay_vhost(
        tls_stream,
        addr,
        &entry,
        head,
        grx,
        gtx,
        Arc::clone(&entry.active),
        &sub,
        &fqdn,
        log_ctx,
    )
    .await
}

/// Read up to `\r\n\r\n` from any `AsyncRead + Unpin` stream, capped at 16 KiB.
pub(crate) async fn read_head_async<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    const MAX: usize = 16 * 1024;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        // Scan only the newly-read region (plus 3 bytes of overlap) for the
        // terminator instead of re-scanning the whole buffer each iteration.
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if buf[scan_from..].windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX {
            break;
        }
    }
    Ok(buf)
}

/// Send a minimal 502 Bad Gateway response and close. Generic over the stream so
/// both the plain HTTP and TLS-terminated HTTPS paths share one implementation.
async fn send_bad_gateway<S: AsyncWrite + Unpin>(mut stream: S) -> Result<()> {
    let _ = stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Send a minimal 503 Service Unavailable response and close. Used when the
/// `--max-conns` bound is hit on the unified control-port vhost path, so the
/// client gets a clean signal instead of a silent reset.
pub(crate) async fn send_service_unavailable<S: AsyncWrite + Unpin>(mut stream: S) -> Result<()> {
    let _ = stream
        .write_all(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Extract the Host header value from a raw HTTP request head.
pub(crate) fn extract_host_from_head(head: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("host") {
                return Some(value.trim());
            }
        }
    }
    None
}

// ─── Hot-reload task ──────────────────────────────────────────────────────────

/// Poll the vhost config + cert/key files every 2 s.
///
/// On a `vhost.yml` change the config is re-parsed and hot-swapped. On a cert/key
/// change — whether the file *contents* changed (mtime) or the config repointed to
/// a different file (path) — the TLS acceptor is atomically swapped so in-flight
/// connections are unaffected.
///
/// The frontend listener set (mode + ports) is bound once at startup and cannot be
/// changed without a restart; a reload that implies a different set is applied to
/// the config but logged as a warning so the operator knows a restart is needed.
pub async fn run_reload_task(
    vhost_config: Option<SharedVhostConfig>,
    vhost_tls: Arc<RwLock<Option<Arc<TlsAcceptor>>>>,
    config_path: Option<PathBuf>,
) {
    let Some(cfg_lock) = vhost_config else {
        return;
    };

    // Snapshot the startup config. The bound listener set (mode + ports) is fixed
    // for the life of the process, so a reload that changes it needs a restart.
    let (mut cert_path, mut key_path, bound_mode, bound_http_port, bound_https_port) = {
        let cfg = cfg_lock.read().unwrap().clone();
        (
            cfg.cert_file.clone(),
            cfg.key_file.clone(),
            resolve_mode(&cfg, cert_present(&cfg)).unwrap_or(VhostMode::Http),
            cfg.http_port,
            cfg.https_port,
        )
    };
    let mut cert_mtime = mtime_of(cert_path.as_deref());
    let mut key_mtime = mtime_of(key_path.as_deref());
    let mut cfg_mtime = mtime_of(config_path.as_deref());

    let mut ticker = interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await; // skip first immediate tick

    loop {
        ticker.tick().await;

        // Reload vhost.yml if it changed.
        if let Some(ref path) = config_path {
            let new_cfg_mtime = mtime_of(Some(path.as_path()));
            if new_cfg_mtime != cfg_mtime {
                cfg_mtime = new_cfg_mtime;
                match std::fs::read_to_string(path) {
                    Ok(yaml) => match parse_config(&yaml) {
                        Ok(new_cfg) => {
                            let new_cert = new_cfg.cert_file.clone();
                            let new_key = new_cfg.key_file.clone();
                            let paths_changed = new_cert != cert_path || new_key != key_path;

                            // Warn (don't fail) when the reload implies a listener set
                            // the running process can't honor without a restart.
                            let new_mode = resolve_mode(&new_cfg, cert_present(&new_cfg))
                                .unwrap_or(VhostMode::Http);
                            if new_mode != bound_mode {
                                warn!(
                                    ?bound_mode, ?new_mode,
                                    "vhost mode changed in config; restart required to (un)bind frontend listeners"
                                );
                            }
                            if new_cfg.http_port != bound_http_port
                                || new_cfg.https_port != bound_https_port
                            {
                                warn!(
                                    "vhost frontend port changed in config; restart required to rebind listeners"
                                );
                            }

                            cert_path = new_cert;
                            key_path = new_key;
                            *cfg_lock.write().unwrap() = Arc::new(new_cfg);
                            info!("vhost config reloaded");

                            // When the cert/key *paths* changed, force a TLS reload:
                            // resetting the tracked mtimes makes the block below fire
                            // even if the new file's own mtime happens to match.
                            if paths_changed {
                                cert_mtime = None;
                                key_mtime = None;
                            }
                        }
                        Err(err) => warn!(%err, "vhost config reload failed; keeping old config"),
                    },
                    Err(err) => warn!(%err, "vhost config read failed; keeping old config"),
                }
            }
        }

        // Reload TLS cert/key if either changed (content mtime, or forced above).
        let new_cert_mtime = mtime_of(cert_path.as_deref());
        let new_key_mtime = mtime_of(key_path.as_deref());
        if new_cert_mtime != cert_mtime || new_key_mtime != key_mtime {
            let cfg = cfg_lock.read().unwrap().clone();
            if let (Some(cert), Some(key)) = (cfg.cert_file.as_ref(), cfg.key_file.as_ref()) {
                match transport::load_server_tls(
                    cert.to_str().unwrap_or_default(),
                    key.to_str().unwrap_or_default(),
                ) {
                    Ok(new_acceptor) => {
                        *vhost_tls.write().unwrap() = Some(Arc::new(new_acceptor));
                        cert_mtime = new_cert_mtime;
                        key_mtime = new_key_mtime;
                        info!("vhost TLS certificate reloaded");
                    }
                    Err(err) => warn!(%err, "vhost TLS reload failed; keeping old cert"),
                }
            } else {
                // No cert/key in the current config: record the mtimes so we don't
                // retry the (impossible) reload on every tick.
                cert_mtime = new_cert_mtime;
                key_mtime = new_key_mtime;
            }
        }
    }
}

fn mtime_of(path: Option<&std::path::Path>) -> Option<std::time::SystemTime> {
    path.and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_subdomain ──────────────────────────────────────────────────

    #[test]
    fn extract_subdomain_basic() {
        assert_eq!(
            extract_subdomain("mysub.bore.example.com", "bore.example.com"),
            Some("mysub".to_string())
        );
    }

    #[test]
    fn extract_subdomain_strips_port() {
        assert_eq!(
            extract_subdomain("mysub.bore.example.com:443", "bore.example.com"),
            Some("mysub".to_string())
        );
    }

    #[test]
    fn extract_subdomain_case_insensitive() {
        assert_eq!(
            extract_subdomain("MySub.Bore.Example.Com", "bore.example.com"),
            Some("mysub".to_string())
        );
    }

    #[test]
    fn extract_subdomain_wrong_base_domain() {
        assert_eq!(
            extract_subdomain("mysub.other.example.com", "bore.example.com"),
            None
        );
    }

    #[test]
    fn extract_subdomain_nested_label_rejected() {
        assert_eq!(
            extract_subdomain("a.b.bore.example.com", "bore.example.com"),
            None
        );
    }

    #[test]
    fn extract_subdomain_empty_label() {
        assert_eq!(
            extract_subdomain(".bore.example.com", "bore.example.com"),
            None
        );
    }

    #[test]
    fn extract_subdomain_illegal_underscore() {
        assert_eq!(
            extract_subdomain("my_sub.bore.example.com", "bore.example.com"),
            None
        );
    }

    #[test]
    fn extract_subdomain_leading_hyphen() {
        assert_eq!(
            extract_subdomain("-sub.bore.example.com", "bore.example.com"),
            None
        );
    }

    #[test]
    fn extract_subdomain_trailing_hyphen() {
        assert_eq!(
            extract_subdomain("sub-.bore.example.com", "bore.example.com"),
            None
        );
    }

    // ── parse_config ──────────────────────────────────────────────────────

    #[test]
    fn parse_config_full() {
        let yaml = r#"
base_domain: bore.example.com
mode: both
http_port: 8080
https_port: 8443
cert_file: /etc/ssl/cert.pem
key_file: /etc/ssl/key.pem
default_headers:
  X-Forwarded-By: bore
reservations:
  - client_id: client-a
    subdomain: myapp
    headers:
      X-App: myapp
"#;
        let cfg = parse_config(yaml).unwrap();
        assert_eq!(cfg.base_domain, "bore.example.com");
        assert_eq!(cfg.mode, VhostModeCfg::Both);
        assert_eq!(cfg.http_port, 8080);
        assert_eq!(cfg.https_port, 8443);
        assert_eq!(cfg.reservations.len(), 1);
        assert_eq!(cfg.reservations[0].client_id, "client-a");
        assert_eq!(cfg.default_headers.get("X-Forwarded-By").unwrap(), "bore");
    }

    #[test]
    fn parse_config_minimal_defaults() {
        let yaml = "base_domain: bore.example.com\n";
        let cfg = parse_config(yaml).unwrap();
        assert_eq!(cfg.http_port, 80);
        assert_eq!(cfg.https_port, 443);
        assert!(cfg.cert_file.is_none());
        assert!(cfg.reservations.is_empty());
        assert_eq!(cfg.mode, VhostModeCfg::Auto);
    }

    #[test]
    fn parse_config_unknown_mode_errors() {
        let yaml = "base_domain: x.com\nmode: foobar\n";
        assert!(parse_config(yaml).is_err());
    }

    // ── resolve_route ─────────────────────────────────────────────────────

    fn cfg_with_reservation(client_id: &str, subdomain: &str) -> VhostConfig {
        let yaml = format!(
            "base_domain: bore.example.com\nreservations:\n  - client_id: {client_id}\n    subdomain: {subdomain}\n"
        );
        parse_config(&yaml).unwrap()
    }

    #[test]
    fn resolve_route_reserved_matching_accepts() {
        let cfg = cfg_with_reservation("client-a", "myapp");
        assert!(matches!(
            resolve_route(&cfg, "myapp", "client-a"),
            RouteDecision::Accept { .. }
        ));
    }

    #[test]
    fn resolve_route_reserved_other_id_rejects() {
        let cfg = cfg_with_reservation("client-a", "myapp");
        assert!(matches!(
            resolve_route(&cfg, "myapp", "client-b"),
            RouteDecision::Reject { .. }
        ));
    }

    #[test]
    fn resolve_route_unreserved_accepts() {
        let cfg = parse_config("base_domain: bore.example.com\n").unwrap();
        assert!(matches!(
            resolve_route(&cfg, "anysub", "anyone"),
            RouteDecision::Accept { .. }
        ));
    }

    // ── merge_headers ─────────────────────────────────────────────────────

    #[test]
    fn merge_headers_per_sub_overrides_default() {
        let defaults: BTreeMap<String, String> = [("X-A".to_string(), "default".to_string())]
            .into_iter()
            .collect();
        let per_sub: BTreeMap<String, String> = [("X-A".to_string(), "override".to_string())]
            .into_iter()
            .collect();
        let merged = merge_headers(&defaults, &per_sub);
        assert_eq!(merged, vec![("X-A".to_string(), "override".to_string())]);
    }

    #[test]
    fn merge_headers_disjoint_union() {
        let defaults: BTreeMap<String, String> =
            [("X-A".to_string(), "a".to_string())].into_iter().collect();
        let per_sub: BTreeMap<String, String> =
            [("X-B".to_string(), "b".to_string())].into_iter().collect();
        let merged = merge_headers(&defaults, &per_sub);
        assert_eq!(merged.len(), 2);
    }

    // ── resolve_mode ──────────────────────────────────────────────────────

    fn cfg_mode(mode: VhostModeCfg) -> VhostConfig {
        VhostConfig {
            base_domain: "bore.example.com".to_string(),
            mode,
            http_port: 80,
            https_port: 443,
            cert_file: None,
            key_file: None,
            default_headers: BTreeMap::new(),
            default_response_headers: BTreeMap::new(),
            reservations: vec![],
        }
    }

    #[test]
    fn resolve_mode_no_cert_forces_http() {
        let cfg = cfg_mode(VhostModeCfg::Auto);
        assert_eq!(resolve_mode(&cfg, false).unwrap(), VhostMode::Http);
    }

    #[test]
    fn resolve_mode_https_no_cert_errors() {
        let cfg = cfg_mode(VhostModeCfg::Https);
        assert!(resolve_mode(&cfg, false).is_err());
    }

    #[test]
    fn resolve_mode_both_no_cert_errors() {
        let cfg = cfg_mode(VhostModeCfg::Both);
        assert!(resolve_mode(&cfg, false).is_err());
    }

    #[test]
    fn resolve_mode_redirect_https_no_cert_errors() {
        let cfg = cfg_mode(VhostModeCfg::RedirectHttps);
        assert!(resolve_mode(&cfg, false).is_err());
    }

    #[test]
    fn resolve_mode_https_with_cert() {
        let cfg = cfg_mode(VhostModeCfg::Https);
        assert_eq!(resolve_mode(&cfg, true).unwrap(), VhostMode::Https);
    }

    #[test]
    fn resolve_mode_auto_with_cert_returns_both() {
        let cfg = cfg_mode(VhostModeCfg::Auto);
        assert_eq!(resolve_mode(&cfg, true).unwrap(), VhostMode::Both);
    }

    // ── public_urls ───────────────────────────────────────────────────────

    #[test]
    fn public_urls_http_default_port_no_suffix() {
        let (http, https) = public_urls("myapp", "bore.example.com", VhostMode::Http, 80, 443);
        assert_eq!(http, Some("http://myapp.bore.example.com".to_string()));
        assert_eq!(https, None);
    }

    #[test]
    fn public_urls_https_default_port_no_suffix() {
        let (http, https) = public_urls("myapp", "bore.example.com", VhostMode::Https, 80, 443);
        assert_eq!(http, None);
        assert_eq!(https, Some("https://myapp.bore.example.com".to_string()));
    }

    #[test]
    fn public_urls_non_default_ports_include_port() {
        let (http, https) = public_urls("myapp", "bore.example.com", VhostMode::Both, 8080, 8443);
        assert_eq!(http, Some("http://myapp.bore.example.com:8080".to_string()));
        assert_eq!(
            https,
            Some("https://myapp.bore.example.com:8443".to_string())
        );
    }

    #[test]
    fn public_urls_redirect_mode_no_http_url() {
        let (http, https) = public_urls(
            "myapp",
            "bore.example.com",
            VhostMode::RedirectHttps,
            80,
            443,
        );
        assert_eq!(http, None);
        assert_eq!(https, Some("https://myapp.bore.example.com".to_string()));
    }

    // ── rewrite_head ──────────────────────────────────────────────────────

    fn inject(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn rewrite_head_preserves_request_body() {
        // The head reader can over-read body bytes that arrived in the same TCP
        // segment as the headers; they must survive the rewrite (regression: they
        // used to be dropped, corrupting every POST/PUT on the inject path).
        let head = b"POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello";
        let out = rewrite_head(head, &inject(&[("X-Inj", "1")]));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with("\r\n\r\nhello"),
            "body must be preserved: {text}"
        );
        assert!(
            text.contains("X-Inj: 1\r\n"),
            "injected header must be present"
        );
        assert!(
            text.contains("Content-Length: 5\r\n"),
            "original headers kept"
        );
    }

    #[test]
    fn rewrite_head_overrides_named_header_case_insensitively() {
        let head = b"GET / HTTP/1.1\r\nHost: a\r\nX-A: old\r\n\r\n";
        let out = rewrite_head(head, &inject(&[("x-a", "new")]));
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("x-a: new\r\n"),
            "override must be injected: {text}"
        );
        assert!(
            !text.contains("X-A: old"),
            "old value must be dropped: {text}"
        );
        assert!(text.contains("Host: a\r\n"), "unrelated header kept");
    }

    #[test]
    fn rewrite_head_without_terminator_is_returned_unchanged() {
        // A head with no complete `\r\n\r\n` (e.g. truncated at the read cap) must
        // not be rewritten — that would desync the stream. Returned as-is.
        let head = b"POST /x HTTP/1.1\r\nHost: a\r\nX-Partial: incomplet";
        let out = rewrite_head(head, &inject(&[("X-Inj", "1")]));
        assert_eq!(out, head, "no terminator → returned verbatim");
    }

    #[test]
    fn rewrite_head_preserves_non_ascii_header_bytes() {
        // Raw-byte processing must not mangle non-ASCII header values.
        let head = b"GET / HTTP/1.1\r\nX-Name: caf\xC3\xA9\r\n\r\n";
        let out = rewrite_head(head, &inject(&[("X-Inj", "1")]));
        // The café bytes (0xC3 0xA9) must appear untouched.
        assert!(
            out.windows(2).any(|w| w == [0xC3, 0xA9]),
            "non-ascii bytes survive"
        );
    }

    // ── clamp_direct_carriers (VH-2) ──────────────────────────────────────

    #[cfg(feature = "udp")]
    #[test]
    fn clamp_direct_carriers_caps_at_max() {
        let max = MAX_DIRECT_CARRIERS as u16;
        assert_eq!(clamp_direct_carriers(0), 1, "zero floors to one");
        assert_eq!(clamp_direct_carriers(1), 1);
        assert_eq!(clamp_direct_carriers(16), 16, "under cap is unchanged");
        assert_eq!(clamp_direct_carriers(max), max);
        // Above the server's per-subdomain cap: must clamp so surplus carriers
        // don't churn (open → server-close → renew → reopen → …).
        assert_eq!(clamp_direct_carriers(max + 8), max, "above cap is clamped");
        assert_eq!(clamp_direct_carriers(u16::MAX), max);
    }

    // ── gateway_basic_auth (SSH ingress server-side gate) ───────────────────

    fn test_entry(gateway_basic_auth: Option<BasicAuth>) -> VhostEntry {
        let (a, _b) = tokio::io::duplex(4096);
        let (opener, _acceptor) = mux::client(a);
        VhostEntry {
            pool: Arc::new(CarrierPool::new(mux::LinkOpener::Mux(opener))),
            request_headers: vec![],
            response_headers: vec![],
            #[cfg(feature = "udp")]
            direct: DirectPool::default(),
            #[cfg(feature = "udp")]
            direct_stream_opens: AtomicU64::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            webserver_log: false,
            https_policy: None,
            backend_tls: false,
            backend_tls_sni: None,
            peer: "127.0.0.1:1".parse().unwrap(),
            since: Instant::now(),
            notes: None,
            basic_auth: gateway_basic_auth.is_some(),
            udp: false,
            auto_reconnect: false,
            local_host: None,
            local_port: 0,
            relay_tx_bytes: Arc::new(AtomicU64::new(0)),
            relay_rx_bytes: Arc::new(AtomicU64::new(0)),
            gateway_basic_auth,
            transport: crate::admin::Transport::Bore,
            identity: None,
        }
    }

    #[tokio::test]
    async fn gateway_basic_auth_none_is_noop() {
        // No `gateway_basic_auth` configured: relay_vhost must not touch the
        // head at all for auth purposes (native `bore vhost` gates client-side).
        let entry = test_entry(None);
        assert!(entry.gateway_basic_auth.is_none());
    }

    #[tokio::test]
    async fn vhost_entry_backend_tls_defaults_off() {
        // Scaffolding phase: the entry carries the backend-TLS fields, defaulting
        // to off / none so the plaintext backend path is unchanged.
        let entry = test_entry(None);
        assert!(!entry.backend_tls);
        assert!(entry.backend_tls_sni.is_none());
    }

    #[tokio::test]
    async fn gateway_basic_auth_rejects_unauthorized_before_opening_provider() {
        let auth = BasicAuth::parse("user:pass").unwrap();
        let entry = test_entry(Some(auth));
        let (public, mut peer) = tokio::io::duplex(4096);
        let head = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec();

        let result = relay_vhost(
            public,
            "127.0.0.1:2".parse().unwrap(),
            &entry,
            head,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&entry.active),
            "sub",
            "sub.example.com",
            LogContext {
                logger: None,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        )
        .await;

        assert!(result.is_ok());
        let mut buf = vec![0u8; 12];
        peer.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"HTTP/1.1 401");
    }

    #[tokio::test]
    async fn gateway_basic_auth_accepts_authorized_then_falls_through_to_relay() {
        // A correctly authorized request must NOT hit the 401 short-circuit;
        // it proceeds to the normal relay path (which then fails opening a
        // provider link, since the mock pool has no live carrier — proving
        // the auth gate itself let it through rather than rejecting it).
        let auth = BasicAuth::parse("user:pass").unwrap();
        let entry = test_entry(Some(auth));
        let (public, _peer) = tokio::io::duplex(4096);
        let head =
            b"GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n".to_vec();

        let result = relay_vhost(
            public,
            "127.0.0.1:2".parse().unwrap(),
            &entry,
            head,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&entry.active),
            "sub",
            "sub.example.com",
            LogContext {
                logger: None,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        )
        .await;

        // No live carrier ⇒ opening the provider link errors out; that error
        // (not an early Ok(())) proves the auth gate was passed.
        assert!(result.is_err());
    }

    // ── backend TLS origination (Phase 1: server-side wrap) ────────────────
    //
    // These drive `relay_vhost` directly with a `VhostEntry` whose carrier pool
    // is an in-memory mux link, and stand up a tunnelled backend on the peer
    // half. When `backend_tls` is set the server wraps the provider link in a
    // client TLS session; the backend here runs a real `tokio-rustls` server so
    // the handshake and the decrypted HTTP round-trip are exercised end to end.

    /// Build a `VhostEntry` backed by an in-memory mux carrier and return the
    /// provider-side transport half for the test to drive as the backend.
    fn backend_entry(
        backend_tls: bool,
        backend_tls_sni: Option<String>,
    ) -> (VhostEntry, tokio::io::DuplexStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (opener, _acc) = mux::client(a);
        let mut entry = test_entry(None);
        entry.pool = Arc::new(CarrierPool::new(mux::LinkOpener::Mux(opener)));
        entry.backend_tls = backend_tls;
        entry.backend_tls_sni = backend_tls_sni;
        (entry, b)
    }

    /// Read one HTTP request head (up to CRLFCRLF) then reply a fixed 200.
    async fn serve_one_http<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, body: &str) {
        let mut buf = vec![0u8; 4096];
        let mut total = 0;
        loop {
            let Ok(n) = s.read(&mut buf[total..]).await else {
                return;
            };
            if n == 0 {
                break;
            }
            total += n;
            if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if total >= buf.len() {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = s.write_all(response.as_bytes()).await;
        let _ = s.flush().await;
        // Clean TLS teardown: send close_notify so the server's read side sees a
        // graceful EOF rather than an unexpected-EOF error.
        let _ = s.shutdown().await;
    }

    /// Accept the server's mux stream, strip the STREAM_READY marker, then serve
    /// one HTTP request. If `tls` is set, wrap the stream in a TLS server first
    /// (simulating an HTTPS backend behind the tunnel).
    fn spawn_mux_backend(
        b: tokio::io::DuplexStream,
        tls: Option<(String, String)>,
        body: &'static str,
    ) {
        tokio::spawn(async move {
            let (_op, mut acceptor) = mux::server(b);
            let Some(mut stream) = acceptor.accept().await else {
                return;
            };
            if mux::read_stream_ready(&mut stream, false).await.is_err() {
                return;
            }
            match tls {
                Some((cert, key)) => {
                    let acceptor =
                        crate::transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())
                            .unwrap();
                    let Ok(mut tls_stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    serve_one_http(&mut tls_stream, body).await;
                }
                None => serve_one_http(&mut stream, body).await,
            }
        });
    }

    /// Accept the mux stream, strip the marker, then drop it immediately. A
    /// server attempting a TLS handshake against this sees EOF and fails fast
    /// (used to prove backend_tls against a non-TLS backend does not hang).
    fn spawn_mux_close_after_marker(b: tokio::io::DuplexStream) {
        tokio::spawn(async move {
            let (_op, mut acceptor) = mux::server(b);
            let Some(mut stream) = acceptor.accept().await else {
                return;
            };
            let _ = mux::read_stream_ready(&mut stream, false).await;
            // Drop `stream` → the server's handshake read gets EOF.
        });
    }

    /// Drive `relay_vhost` with a fixed GET, half-closing the browser side so
    /// the relay completes, and return `(relay_result, response_bytes)`.
    async fn drive_relay(entry: &VhostEntry, host: &str) -> (Result<()>, Vec<u8>) {
        let (public, mut peer) = tokio::io::duplex(64 * 1024);
        let head =
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").into_bytes();
        let relay = relay_vhost(
            public,
            "127.0.0.1:2".parse().unwrap(),
            entry,
            head,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&entry.active),
            "sub",
            "sub.example.com",
            LogContext {
                logger: None,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        );
        let read = async {
            // Signal no browser body so the relay's public→provider half EOFs.
            let _ = peer.shutdown().await;
            let mut resp = Vec::new();
            let _ = peer.read_to_end(&mut resp).await;
            resp
        };
        tokio::join!(relay, read)
    }

    #[tokio::test]
    async fn backend_tls_wrap_handshakes_with_self_signed() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let key_pem = cert.signing_key.serialize_pem();

        let (entry, b) = backend_entry(true, Some("localhost".to_string()));
        spawn_mux_backend(b, Some((cert_pem, key_pem)), "backend-tls-ok");

        let (res, resp) = tokio::time::timeout(Duration::from_secs(8), drive_relay(&entry, "sub"))
            .await
            .expect("relay must not hang");
        assert!(res.is_ok(), "relay over TLS backend failed: {res:?}");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"), "expected 200 from backend: {text}");
        assert!(
            text.contains("backend-tls-ok"),
            "expected backend body: {text}"
        );
    }

    #[tokio::test]
    async fn backend_tls_off_path_unchanged() {
        // backend_tls == false against a plaintext backend still serves 200
        // (guards I-1: the pre-feature relay path is untouched).
        let (entry, b) = backend_entry(false, None);
        spawn_mux_backend(b, None, "plaintext-ok");

        let (res, resp) = tokio::time::timeout(Duration::from_secs(8), drive_relay(&entry, "sub"))
            .await
            .expect("relay must not hang");
        assert!(res.is_ok(), "plaintext relay failed: {res:?}");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.contains("200 OK"), "expected 200: {text}");
        assert!(text.contains("plaintext-ok"), "expected body: {text}");
    }

    #[tokio::test]
    async fn backend_tls_bad_sni_fails_gracefully() {
        // An empty SNI is rejected by rustls' ServerName parse before any
        // handshake; the relay must return an error (never panic, never hang).
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (entry, b) = backend_entry(true, Some(String::new()));
        spawn_mux_backend(
            b,
            Some((cert.cert.pem(), cert.signing_key.serialize_pem())),
            "unused",
        );

        let (res, _resp) = tokio::time::timeout(Duration::from_secs(3), drive_relay(&entry, "sub"))
            .await
            .expect("bad SNI must fail fast, not hang");
        assert!(res.is_err(), "empty SNI must error, got: {res:?}");
    }

    #[tokio::test]
    async fn backend_tls_against_plaintext_backend_times_out_or_errors() {
        // backend_tls == true pointed at a non-TLS backend: the handshake must
        // fail (here the backend closes after the marker → EOF) within the
        // timeout, never hanging.
        let (entry, b) = backend_entry(true, Some("localhost".to_string()));
        spawn_mux_close_after_marker(b);

        let (res, _resp) = tokio::time::timeout(
            Duration::from_secs(BACKEND_TLS_HANDSHAKE_TIMEOUT.as_secs() + 2),
            drive_relay(&entry, "sub"),
        )
        .await
        .expect("non-TLS backend must not hang past the handshake timeout");
        assert!(res.is_err(), "TLS against plaintext backend must error");
    }

    #[test]
    fn vhost_display_flags_none_inherits_mode() {
        // policy=None with mode=RedirectHttps → (true, true)
        let (https, force_https) = vhost_display_flags(None, VhostMode::RedirectHttps, true);
        assert_eq!((https, force_https), (true, true));

        // policy=None with mode=Http → (false, false)
        let (https, force_https) = vhost_display_flags(None, VhostMode::Http, false);
        assert_eq!((https, force_https), (false, false));

        // policy=None with mode=Both → (true, false)
        let (https, force_https) = vhost_display_flags(None, VhostMode::Both, true);
        assert_eq!((https, force_https), (true, false));
    }

    #[test]
    fn vhost_display_flags_off_never_redirect() {
        let (https, force_https) = vhost_display_flags(
            Some(crate::shared::HttpsPolicy::Off),
            VhostMode::RedirectHttps,
            true,
        );
        assert_eq!((https, force_https), (false, false));
    }

    #[test]
    fn vhost_display_flags_on_capable() {
        let (https, force_https) =
            vhost_display_flags(Some(crate::shared::HttpsPolicy::On), VhostMode::Both, true);
        assert_eq!((https, force_https), (true, false));
    }

    #[test]
    fn vhost_display_flags_on_incapable() {
        let (https, force_https) =
            vhost_display_flags(Some(crate::shared::HttpsPolicy::On), VhostMode::Both, false);
        assert_eq!((https, force_https), (false, false));
    }

    #[test]
    fn vhost_display_flags_redirect_capable() {
        let (https, force_https) = vhost_display_flags(
            Some(crate::shared::HttpsPolicy::Redirect),
            VhostMode::Both,
            true,
        );
        assert_eq!((https, force_https), (true, true));
    }

    #[test]
    fn vhost_display_flags_redirect_incapable() {
        let (https, force_https) = vhost_display_flags(
            Some(crate::shared::HttpsPolicy::Redirect),
            VhostMode::Both,
            false,
        );
        assert_eq!((https, force_https), (false, false));
    }

    #[test]
    fn should_redirect_none_respects_global_mode() {
        // None with RedirectHttps → true (global decision)
        assert!(should_redirect(None, VhostMode::RedirectHttps, true));

        // None with Both → false (no global redirect)
        assert!(!should_redirect(None, VhostMode::Both, true));

        // None with Http → false (no global redirect)
        assert!(!should_redirect(None, VhostMode::Http, false));
    }

    #[test]
    fn should_redirect_off_never_redirects() {
        assert!(!should_redirect(
            Some(crate::shared::HttpsPolicy::Off),
            VhostMode::RedirectHttps,
            true
        ));
    }

    #[test]
    fn should_redirect_on_never_redirects() {
        assert!(!should_redirect(
            Some(crate::shared::HttpsPolicy::On),
            VhostMode::RedirectHttps,
            true
        ));
    }

    #[test]
    fn should_redirect_redirect_capable() {
        assert!(should_redirect(
            Some(crate::shared::HttpsPolicy::Redirect),
            VhostMode::Both,
            true
        ));
    }

    #[test]
    fn should_redirect_redirect_incapable() {
        assert!(!should_redirect(
            Some(crate::shared::HttpsPolicy::Redirect),
            VhostMode::Both,
            false
        ));
    }

    // ── flush-before-park (36cd70d — docs/VHOST_INJECTED_FLUSH_FIX.md) ───────
    //
    // These mocks encode the tokio-rustls write contract directly, so the tests
    // are deterministic (plain `#[test]`, single poll, no runtime/timing): a
    // TLS-backed splice loop that parks on read without flushing leaves bytes
    // invisible in `pending` exactly like encrypted records parked in the
    // rustls session buffer. The TLS integration tests can go green on
    // loopback (kernel buffers absorb everything); these cannot.

    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context as TaskContext, Poll};

    #[derive(Default)]
    struct FlushGateState {
        /// Written but not yet flushed — "parked in the rustls session".
        pending: Vec<u8>,
        /// Flushed — actually on the wire.
        visible: Vec<u8>,
        shutdown: bool,
    }

    /// `poll_write` accepts bytes into `pending` and returns `Ok` (like
    /// tokio-rustls under socket backpressure); only `poll_flush` /
    /// `poll_shutdown` publishes them to `visible`.
    struct FlushGatedWriter(Arc<StdMutex<FlushGateState>>);

    impl AsyncWrite for FlushGatedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().pending.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            let mut st = self.0.lock().unwrap();
            let parked = std::mem::take(&mut st.pending);
            st.visible.extend_from_slice(&parked);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            let mut st = self.0.lock().unwrap();
            let parked = std::mem::take(&mut st.pending);
            st.visible.extend_from_slice(&parked);
            st.shutdown = true;
            Poll::Ready(Ok(()))
        }
    }

    /// Yields queued chunks, then EOF or `Pending` forever. `Pending` is the
    /// keep-alive shape: the peer stays open but has nothing more to say. It
    /// intentionally never wakes — tests observe state through the shared
    /// `FlushGateState` instead of awaiting completion.
    struct ChunksThenPark {
        chunks: VecDeque<Vec<u8>>,
        eof_when_empty: bool,
    }

    impl AsyncRead for ChunksThenPark {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            match self.chunks.pop_front() {
                Some(chunk) => {
                    buf.put_slice(&chunk);
                    Poll::Ready(Ok(()))
                }
                None if self.eof_when_empty => Poll::Ready(Ok(())),
                None => Poll::Pending,
            }
        }
    }

    /// Combined duplex mock for APIs that take one `AsyncRead + AsyncWrite`.
    struct MockDuplex {
        read: ChunksThenPark,
        write: FlushGatedWriter,
    }

    impl AsyncRead for MockDuplex {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.read).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for MockDuplex {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.write).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.write).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.write).poll_shutdown(cx)
        }
    }

    /// One poll drives an async fn to its first genuine park — deterministic,
    /// no spawn, no yield loops, no timing.
    fn poll_once<F: std::future::Future>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    #[test]
    fn copy_loop_flushes_writes_before_parking_on_read() {
        let state = Arc::new(StdMutex::new(FlushGateState::default()));
        let mut writer = FlushGatedWriter(state.clone());
        let mut reader = ChunksThenPark {
            chunks: [b"first ".to_vec(), b"second".to_vec()].into(),
            eof_when_empty: false,
        };
        let mut fut = Box::pin(copy_one_direction_with_shutdown(&mut reader, &mut writer));
        assert!(
            poll_once(&mut fut).is_pending(),
            "keep-alive copy must park on read, not complete"
        );
        drop(fut);

        let st = state.lock().unwrap();
        assert_eq!(
            st.visible.as_slice(),
            b"first second",
            "all written bytes must be flushed to the wire before the loop parks on read"
        );
        assert!(st.pending.is_empty(), "no bytes may stay parked unflushed");
        assert!(!st.shutdown, "no EOF seen — half-close must not fire");
    }

    #[test]
    fn copy_loop_eof_propagates_half_close_and_publishes_tail() {
        let state = Arc::new(StdMutex::new(FlushGateState::default()));
        let mut writer = FlushGatedWriter(state.clone());
        let mut reader = ChunksThenPark {
            chunks: [b"payload".to_vec()].into(),
            eof_when_empty: true,
        };
        let mut fut = Box::pin(copy_one_direction_with_shutdown(&mut reader, &mut writer));
        match poll_once(&mut fut) {
            Poll::Ready(res) => res.expect("EOF path must complete cleanly"),
            Poll::Pending => panic!("EOF must complete the copy, not park"),
        }
        drop(fut);

        let st = state.lock().unwrap();
        assert_eq!(st.visible.as_slice(), b"payload");
        assert!(st.shutdown, "EOF must propagate half-close via shutdown");
    }

    #[test]
    fn injected_response_head_and_body_flushed_before_keepalive_park() {
        let public_state = Arc::new(StdMutex::new(FlushGateState::default()));
        let provider_state = Arc::new(StdMutex::new(FlushGateState::default()));
        let public = MockDuplex {
            // Request head was already consumed upstream; the connection stays
            // open (keep-alive) with nothing more to read.
            read: ChunksThenPark {
                chunks: VecDeque::new(),
                eof_when_empty: false,
            },
            write: FlushGatedWriter(public_state.clone()),
        };
        let provider = MockDuplex {
            read: ChunksThenPark {
                chunks: [
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n".to_vec(),
                    b"hello".to_vec(),
                ]
                .into(),
                // Keep-alive: the provider substream stays open after the body.
                eof_when_empty: false,
            },
            write: FlushGatedWriter(provider_state),
        };
        let inject = vec![("X-Injected".to_string(), "yes".to_string())];
        let mut fut = Box::pin(relay_response_injected(public, provider, &inject, None));
        assert!(
            poll_once(&mut fut).is_pending(),
            "keep-alive relay must park, not complete"
        );
        drop(fut);

        let st = public_state.lock().unwrap();
        let text = String::from_utf8_lossy(&st.visible);
        assert!(
            text.contains("X-Injected: yes"),
            "rewritten response head must be flushed to the wire: {text:?}"
        );
        assert!(
            text.ends_with("hello"),
            "body bytes must be flushed before parking on the provider read: {text:?}"
        );
        assert!(
            st.pending.is_empty(),
            "no bytes may stay parked unflushed while the relay waits for more data"
        );
    }
}
