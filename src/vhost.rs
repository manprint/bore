//! Vhost subdomain reverse-proxy: HTTP(S) frontend routed by Host header.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
#[cfg(feature = "udp")]
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::admin::{AdminRegistry, NewEntry, Role};
use crate::edge;
use crate::mux;
use crate::pool::{CarrierPool, PendingCarriers, TokenGuard};
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, UdpDirectTuning, NETWORK_TIMEOUT, PROXY_BUFFER_SIZE,
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
        headers: Vec<(String, String)>,
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
            let headers = merge_headers(&cfg.default_headers, &res.headers);
            RouteDecision::Accept { headers }
        }
        None => {
            // Unreserved: accept with default headers only.
            let headers = merge_headers(&cfg.default_headers, &BTreeMap::new());
            RouteDecision::Accept { headers }
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
    pub headers: Vec<(String, String)>,
    /// Live QUIC direct connection to the provider, when one is established.
    #[cfg(feature = "udp")]
    pub direct: std::sync::RwLock<Option<crate::holepunch::DirectConn>>,
    /// Monotonic generation used to clear `direct` only for the connection that
    /// originally installed it, so an old closed-monitor never stomps a newer one.
    #[cfg(feature = "udp")]
    pub direct_generation: AtomicU64,
    /// Number of proxied requests that successfully opened a direct QUIC stream.
    #[cfg(feature = "udp")]
    pub direct_stream_opens: AtomicU64,
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

trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncReadWrite for T {}

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
    pending_carriers: PendingCarriers,
    max_carriers: u16,
    carriers: u16,
    server_udp_enabled: bool,
    vhost_quic_port: u16,
    pending_vhost_udp: PendingVhostUdp,
    _secret: Option<String>,
    udp_tuning: UdpDirectTuning,
) -> Result<()> {
    // Validate against live config (resolve_route checks reservations).
    let cfg = vhost_config.read().unwrap().clone();
    let headers = match resolve_route(&cfg, &subdomain, &client_id) {
        RouteDecision::Accept { headers } => headers,
        RouteDecision::Reject { reason } => {
            warn!(%subdomain, %reason, "vhost registration rejected");
            control.send(ServerMessage::Error(reason)).await?;
            return Ok(());
        }
    };

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
            let pool = Arc::new(CarrierPool::new(opener));
            let entry = Arc::new(VhostEntry {
                pool: Arc::clone(&pool),
                headers,
                #[cfg(feature = "udp")]
                direct: std::sync::RwLock::new(None),
                #[cfg(feature = "udp")]
                direct_generation: AtomicU64::new(0),
                #[cfg(feature = "udp")]
                direct_stream_opens: AtomicU64::new(0),
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

    let _admin_reg = admin.register(NewEntry {
        role: Role::Vhost,
        peer,
        secret_id: Some(subdomain.clone()),
        public_port: None,
        notes,
        basic_auth,
        https: false,
        force_https: false,
        udp: false,
    });

    // Compute and send the public URLs based on current config.
    let mode = resolve_mode(&cfg, cert_present(&cfg)).unwrap_or(VhostMode::Http);
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
/// any configured) before the bidirectional splice begins.
pub async fn relay_vhost(
    public: impl AsyncRead + AsyncWrite + Unpin,
    entry: &VhostEntry,
    head: Vec<u8>,
) -> Result<()> {
    let mut provider: Pin<Box<dyn AsyncReadWrite>> = {
        #[cfg(feature = "udp")]
        {
            // In vhost UDP the server opens the QUIC streams and the provider
            // accepts them. If the direct connection is down or opening a stream
            // fails, fall back per-request to the existing TCP carrier pool.
            let direct = entry.direct.read().unwrap().clone();
            match direct {
                Some(direct) => match direct.open_stream().await {
                    Ok(stream) => {
                        entry
                            .direct_stream_opens
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Box::pin(stream)
                    }
                    Err(err) => {
                        debug!(%err, "vhost QUIC open_stream failed; using TCP carrier");
                        let opener = entry.pool.pick().context("no live vhost carrier")?;
                        Box::pin(opener.open().await.context("vhost provider unavailable")?)
                    }
                },
                None => {
                    let opener = entry.pool.pick().context("no live vhost carrier")?;
                    Box::pin(opener.open().await.context("vhost provider unavailable")?)
                }
            }
        }
        #[cfg(not(feature = "udp"))]
        {
            let opener = entry.pool.pick().context("no live vhost carrier")?;
            Box::pin(opener.open().await.context("vhost provider unavailable")?)
        }
    };
    provider.write_all(&[mux::STREAM_READY]).await?;

    let mut public = public;
    if entry.headers.is_empty() {
        // Zero-overhead pure-splice path: forward the already-read head as-is.
        provider.write_all(&head).await?;
    } else {
        let rewritten = rewrite_head(&head, &entry.headers);
        provider.write_all(&rewritten).await?;
    }

    tokio::io::copy_bidirectional_with_sizes(
        &mut public,
        &mut provider,
        PROXY_BUFFER_SIZE,
        PROXY_BUFFER_SIZE,
    )
    .await?;
    Ok(())
}

/// Rewrite a request head: insert/override configured headers, keep the rest.
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
/// **MVP limitation:** only the first request head of the connection is rewritten.
/// Subsequent keep-alive requests are spliced raw.
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

// ─── Frontend handlers ────────────────────────────────────────────────────────

/// Handle one inbound HTTP connection on the vhost frontend port.
///
/// Reads the request head, extracts the subdomain from the Host header, and
/// relays the connection to the registered provider (with header injection if
/// configured). Returns a clean 502 when no provider is registered.
pub async fn handle_http(
    mut stream: TcpStream,
    registry: &VhostRegistry,
    vhost_config: &Option<SharedVhostConfig>,
    mode: VhostMode,
) -> Result<()> {
    use tokio::time::timeout;

    let head = timeout(NETWORK_TIMEOUT, edge::read_request_head(&mut stream))
        .await
        .context("timed out reading HTTP request head")??;

    // Redirect mode: return 308 instead of proxying.
    if mode.redirects_http() {
        let cfg = vhost_config.as_ref().map(|c| c.read().unwrap().clone());
        let https_port = cfg.as_deref().map(|c| c.https_port).unwrap_or(443);
        edge::redirect_to_https(stream, https_port, None).await?;
        return Ok(());
    }

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
        debug!(%sub, "vhost http 502: no provider registered");
        return send_bad_gateway(stream).await;
    };

    relay_vhost(stream, &entry, head).await
}

/// Handle one inbound HTTPS connection on the vhost frontend port.
///
/// Terminates TLS with the wildcard acceptor, then routes identically to
/// [`handle_http`] on the decrypted stream — same `Host`-header → subdomain
/// extraction against the configured base domain, same single registry lookup.
pub async fn handle_https(
    stream: TcpStream,
    registry: &VhostRegistry,
    vhost_config: &Option<SharedVhostConfig>,
    vhost_tls: &Arc<RwLock<Option<Arc<TlsAcceptor>>>>,
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

    relay_vhost(tls_stream, &entry, head).await
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
}
