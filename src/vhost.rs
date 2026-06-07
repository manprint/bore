//! Vhost subdomain reverse-proxy: HTTP(S) frontend routed by Host header.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
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
use tracing::{info, warn};
use uuid::Uuid;

use crate::admin::{AdminRegistry, NewEntry, Role};
use crate::edge;
use crate::mux;
use crate::pool::{CarrierPool, PendingCarriers, TokenGuard};
use crate::shared::{ClientMessage, Delimited, ServerMessage, NETWORK_TIMEOUT, PROXY_BUFFER_SIZE};
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
}

/// Registry of live vhost providers, keyed by subdomain label.
pub type VhostRegistry = Arc<DashMap<String, Arc<VhostEntry>>>;

/// Shared hot-swappable vhost config behind a read-write lock.
pub type SharedVhostConfig = Arc<RwLock<Arc<VhostConfig>>>;

/// Removes a vhost provider registration when the provider connection ends.
struct Deregister {
    registry: VhostRegistry,
    subdomain: String,
}

impl Drop for Deregister {
    fn drop(&mut self) {
        self.registry.remove(&self.subdomain);
    }
}

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);

/// Server side: register this connection as the vhost provider for `subdomain`.
#[allow(clippy::too_many_arguments)]
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
    pending_carriers: PendingCarriers,
    max_carriers: u16,
    carriers: u16,
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
            });
            slot.insert(entry);
            pool
        }
    };
    let _guard = Deregister {
        registry: registry.clone(),
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
    let mode = resolve_mode(&cfg, cfg.cert_file.is_some()).unwrap_or(VhostMode::Http);
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

/// Splice one inbound public HTTP(S) connection to the registered vhost provider.
///
/// `head` is the already-read request head bytes that must be forwarded before
/// splicing. `inject` is the set of headers to rewrite into the head, or `None`
/// for the zero-overhead pure-splice path.
pub async fn relay_vhost(
    public: impl AsyncRead + AsyncWrite + Unpin,
    reg: &VhostRegistry,
    sub: &str,
    inject: Option<&[(String, String)]>,
    head: Vec<u8>,
) -> Result<()> {
    // Clone the pool out before any await — never hold the DashMap guard across an await.
    let entry = reg
        .get(sub)
        .map(|e| Arc::clone(e.value()))
        .with_context(|| format!("no vhost provider registered for '{sub}'"))?;

    let opener = entry.pool.pick().context("no live vhost carrier")?;
    let mut provider = opener.open().await.context("vhost provider unavailable")?;
    provider.write_all(&[mux::STREAM_READY]).await?;

    let mut public = public;
    match inject {
        Some(headers) if !headers.is_empty() => {
            let rewritten = rewrite_head(&head, headers);
            provider.write_all(&rewritten).await?;
        }
        _ => {
            // No headers to inject: forward the already-read head as-is.
            provider.write_all(&head).await?;
        }
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
/// headers and the request line unchanged.
///
/// **MVP limitation:** only the first request head of the connection is rewritten.
/// Subsequent keep-alive requests are spliced raw.
pub fn rewrite_head(head: &[u8], inject: &[(String, String)]) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");

    // Keep the request line intact.
    let request_line = lines.next().unwrap_or("");
    let inject_names: Vec<&str> = inject.iter().map(|(k, _)| k.as_str()).collect();

    let mut out = Vec::with_capacity(head.len() + 256);
    out.extend_from_slice(request_line.as_bytes());
    out.extend_from_slice(b"\r\n");

    // Keep existing headers that are NOT overridden.
    let mut found_end = false;
    for line in lines {
        if line.is_empty() {
            found_end = true;
            break;
        }
        // Check if this header name is being overridden.
        let should_drop = if let Some(colon) = line.find(':') {
            let name = line[..colon].trim();
            inject_names.iter().any(|n| n.eq_ignore_ascii_case(name))
        } else {
            false
        };
        if !should_drop {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }

    // Append the injected headers.
    for (name, value) in inject {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    if found_end {
        out.extend_from_slice(b"\r\n");
    }
    out
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
            return send_bad_gateway(stream).await;
        }
    };

    // Look up inject headers from the registry entry.
    let inject_headers: Option<Vec<(String, String)>> = registry
        .get(&sub)
        .map(|e| e.headers.clone())
        .filter(|h| !h.is_empty());

    if !registry.contains_key(&sub) {
        return send_bad_gateway(stream).await;
    }

    relay_vhost(stream, registry, &sub, inject_headers.as_deref(), head).await
}

/// Handle one inbound HTTPS connection on the vhost frontend port.
///
/// Terminates TLS with the wildcard acceptor, then routes identically to
/// [`handle_http`] on the decrypted stream.
pub async fn handle_https(
    stream: TcpStream,
    registry: &VhostRegistry,
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

    let host = extract_host_from_head(&head);
    // For HTTPS, we assume the base domain is derivable from the registry itself.
    // We route by trying to match any registered subdomain in the Host header.
    let sub = match host.and_then(|h| extract_subdomain_from_registry(h, registry)) {
        Some(s) => s,
        None => {
            let _ = tls_stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return Ok(());
        }
    };

    let inject_headers: Option<Vec<(String, String)>> = registry
        .get(&sub)
        .map(|e| e.headers.clone())
        .filter(|h| !h.is_empty());

    relay_vhost(tls_stream, registry, &sub, inject_headers.as_deref(), head).await
}

/// Try to find a matching subdomain from the Host header by checking all live
/// registry entries (the base domain is not known here; we match by suffix).
fn extract_subdomain_from_registry(host: &str, registry: &VhostRegistry) -> Option<String> {
    // Strip port from host.
    let host = match host.rfind(':') {
        Some(i) if host[i + 1..].chars().all(|c| c.is_ascii_digit()) => &host[..i],
        _ => host,
    };
    let host_lc = host.to_lowercase();
    // Check each registered subdomain: if host starts with "<sub>." it matches.
    for entry in registry.iter() {
        let sub = entry.key().to_lowercase();
        if host_lc == sub || host_lc.starts_with(&format!("{sub}.")) {
            return Some(entry.key().clone());
        }
    }
    None
}

/// Read up to `\r\n\r\n` from any `AsyncRead + Unpin` stream, capped at 8 KiB.
async fn read_head_async<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    const MAX: usize = 8 * 1024;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX {
            break;
        }
    }
    Ok(buf)
}

/// Send a minimal 502 Bad Gateway response and close.
async fn send_bad_gateway(mut stream: TcpStream) -> Result<()> {
    let _ = stream
        .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
    Ok(())
}

/// Extract the Host header value from a raw HTTP request head.
fn extract_host_from_head(head: &[u8]) -> Option<&str> {
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

/// Poll cert/key files every 2 s. On mtime change, atomically swap the TLS
/// acceptor so in-flight connections are unaffected. Config reload (vhost.yml
pub async fn run_reload_task(
    vhost_config: Option<SharedVhostConfig>,
    vhost_tls: Arc<RwLock<Option<Arc<TlsAcceptor>>>>,
    config_path: Option<PathBuf>,
) {
    let Some(cfg_lock) = vhost_config else {
        return;
    };

    let (mut cert_path, mut key_path) = {
        let cfg = cfg_lock.read().unwrap().clone();
        (cfg.cert_file.clone(), cfg.key_file.clone())
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
                match std::fs::read_to_string(path) {
                    Ok(yaml) => match parse_config(&yaml) {
                        Ok(new_cfg) => {
                            cert_path = new_cfg.cert_file.clone();
                            key_path = new_cfg.key_file.clone();
                            *cfg_lock.write().unwrap() = Arc::new(new_cfg);
                            cfg_mtime = new_cfg_mtime;
                            cert_mtime = mtime_of(cert_path.as_deref());
                            key_mtime = mtime_of(key_path.as_deref());
                            info!("vhost config reloaded");
                        }
                        Err(err) => warn!(%err, "vhost config reload failed; keeping old config"),
                    },
                    Err(err) => warn!(%err, "vhost config read failed; keeping old config"),
                }
                continue; // cert paths may have changed; re-check on next tick
            }
        }

        // Reload TLS cert/key if either changed.
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
            }
        }

        // Re-read paths from current config in case they changed.
        {
            let cfg = cfg_lock.read().unwrap().clone();
            cert_path = cfg.cert_file.clone();
            key_path = cfg.key_file.clone();
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
}
