//! UDP hole-punching with STUN reflexive-address discovery and a QUIC carrier,
//! used for the `udp` direct-path mode of secret tunnels (see [`crate::secret`]).
//!
//! The server only ever brokers candidate addresses over the (authenticated)
//! control channel — it never sees the punched data path. Two peers of the same
//! secret tunnel each open a UDP socket, learn their public (reflexive) mapping
//! via STUN, exchange candidates through the server, simultaneously send UDP
//! packets to open their NAT mappings, then establish a QUIC connection over that
//! socket. Each proxied connection uses its own native QUIC bidirectional stream,
//! so the direct path avoids TCP/yamux head-of-line blocking. If any step fails
//! the caller falls back to the server relay.
//!
//! Authentication of the direct path is a shared token derived from the tunnel
//! secret and a server-issued nonce ([`derive_token`]): both peers prove
//! knowledge of it on the first bytes of the QUIC stream before `yamux` starts,
//! so the self-signed QUIC certificate need not be verified.
//!
//! The QUIC carrier (and thus the actual hole-punch) requires the `udp` feature,
//! which pulls in `quinn`. The signaling primitives the *server* needs to
//! broker a direct path — STUN reflexive discovery, the STUN responder, and the
//! token derivation — carry no `quinn` dependency and are always compiled, so a
//! lean-built server can still rendezvous for `udp`-enabled clients.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::shared::{UdpCandidateKind, UdpDirectTuning, CONTROL_PORT};

/// Number of consecutive ports predicted past the reflexive one when
/// `--try-port-prediction` is enabled (best-effort symmetric-NAT traversal).
const PREDICT_RANGE: u16 = 4;

#[cfg(feature = "udp")]
use crate::shared::NETWORK_TIMEOUT;
#[cfg(feature = "udp")]
use quinn::rustls;
#[cfg(feature = "udp")]
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
#[cfg(feature = "udp")]
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
#[cfg(feature = "udp")]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
#[cfg(feature = "udp")]
use tracing::trace;

/// Length of the shared authentication token (HMAC-SHA256 output).
pub const TOKEN_LEN: usize = 32;

/// Per-attempt timeout for a STUN binding request (kept short so a missing STUN
/// server fails fast and the caller can fall back to the relay).
const STUN_TIMEOUT: Duration = Duration::from_secs(1);

/// ALPN protocol identifier for the direct QUIC carrier.
#[cfg(feature = "udp")]
const ALPN: &[u8] = b"bore-udp";

/// How long to keep a quiet QUIC connection alive with keep-alive pings, and the
/// idle timeout after which it is considered dead. The keep-alive (every 3s)
/// keeps a long but quiet transfer alive; the idle timeout (10s) makes a peer
/// that vanished without a graceful close (hard kill, network partition) be
/// detected within ~10s so the consumer can re-negotiate or fall back.
#[cfg(feature = "udp")]
const QUIC_KEEPALIVE: Duration = Duration::from_secs(3);
#[cfg(feature = "udp")]
const QUIC_MAX_IDLE: Duration = Duration::from_secs(10);

type HmacSha256 = Hmac<Sha256>;

/// Derive the shared QUIC authentication token from the tunnel secret (if any)
/// and the server-issued session nonce. Both peers compute the same value.
pub fn derive_token(secret: Option<&str>, nonce: &[u8]) -> [u8; TOKEN_LEN] {
    let key = secret.map(str::as_bytes).unwrap_or(&[]);
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(nonce);
    let mut token = [0u8; TOKEN_LEN];
    token.copy_from_slice(&mac.finalize().into_bytes());
    token
}

/// Constant-time comparison of two tokens.
#[cfg(feature = "udp")]
fn tokens_match(a: &[u8; TOKEN_LEN], b: &[u8; TOKEN_LEN]) -> bool {
    let mut diff = 0u8;
    for i in 0..TOKEN_LEN {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Bind a UDP socket for a hole-punch session. `port` 0 picks a random ephemeral
/// port (the default); a fixed port lets a strict *egress* firewall be opened for
/// exactly that port (use the same value on both peers) and makes the public
/// mapping predictable on a port-preserving NAT. A fixed port is bound with
/// `SO_REUSEADDR` so an `--auto-reconnect` cycle can re-bind it immediately after
/// the previous socket closes. (A fixed port does not help symmetric NATs, which
/// remap per destination regardless of the local port.)
pub async fn bind_socket(port: u16) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create UDP socket")?;
    configure_udp_socket_buffers(&socket, &UdpDirectTuning::default());

    // Reuse the address so a reconnect can rebind the fixed port without waiting
    // for the previous socket to be fully released.
    if port != 0 {
        let _ = socket.set_reuse_address(true);
    }
    socket
        .set_nonblocking(true)
        .context("failed to set UDP socket non-blocking")?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, port).into();
    socket.bind(&addr.into()).with_context(|| {
        if port == 0 {
            "failed to bind UDP socket".to_string()
        } else {
            format!("failed to bind fixed UDP port {port} (free? allowed?)")
        }
    })?;
    UdpSocket::from_std(socket.into()).context("failed to register UDP socket with tokio")
}

#[cfg(all(feature = "udp", windows))]
fn configure_udp_socket_buffers<S: std::os::windows::io::AsSocket>(
    socket: &S,
    tuning: &UdpDirectTuning,
) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

#[cfg(all(feature = "udp", target_os = "linux"))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    // CRITICAL for direct-path throughput: the kernel silently clamps
    // SO_SNDBUF/SO_RCVBUF to net.core.{w,r}mem_max (Ubuntu/Debian default
    // 212992 = 208 KiB). A single congestion-controlled QUIC datagram flow is
    // then capped at ~buffer/RTT — e.g. 208 KiB / 20 ms ≈ 10 MB/s — no matter
    // how large a window Quinn negotiates and with the CPU near idle. bore VPN
    // runs with CAP_NET_ADMIN, so use SO_{SND,RCV}BUFFORCE (nix `*BufForce`)
    // which bypass the *mem_max ceiling entirely. Fall back to the clamped
    // setsockopt (socket2) when the cap is absent (EPERM), and verify the
    // result so a clamp that survives is logged LOUDLY (not at debug) with the
    // exact remediation.
    use nix::sys::socket::{getsockopt, setsockopt, sockopt};

    let fd = socket.as_fd();

    // Try the forced setters first; on EPERM (no CAP_NET_ADMIN) fall back to the
    // clamped path so an unprivileged build still gets the best the kernel allows.
    let recv_forced = setsockopt(&fd, sockopt::RcvBufForce, &tuning.udp_socket_recv_buffer).is_ok();
    if !recv_forced {
        let _ = setsockopt(&fd, sockopt::RcvBuf, &tuning.udp_socket_recv_buffer);
    }
    let send_forced = setsockopt(&fd, sockopt::SndBufForce, &tuning.udp_socket_send_buffer).is_ok();
    if !send_forced {
        let _ = setsockopt(&fd, sockopt::SndBuf, &tuning.udp_socket_send_buffer);
    }

    // getsockopt(SO_{SND,RCV}BUF) returns the kernel's internal value, which is
    // 2× the requested size on Linux (kernel doubles for bookkeeping). Compare
    // against the requested size to detect a surviving clamp.
    let actual_recv = getsockopt(&fd, sockopt::RcvBuf).unwrap_or(0);
    let actual_send = getsockopt(&fd, sockopt::SndBuf).unwrap_or(0);
    // A clamp leaves the effective buffer well under the request; the kernel
    // doubling means "healthy" is actual >= requested, so flag actual < requested.
    let recv_clamped = actual_recv < tuning.udp_socket_recv_buffer;
    let send_clamped = actual_send < tuning.udp_socket_send_buffer;

    if recv_clamped || send_clamped {
        tracing::warn!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            recv_forced,
            send_forced,
            "UDP socket buffer clamped below request — direct-path throughput \
             will be limited to roughly buffer/RTT. Run with CAP_NET_ADMIN \
             (privileged) for SO_*BUFFORCE, or raise net.core.rmem_max and \
             net.core.wmem_max (e.g. sysctl -w net.core.rmem_max=16777216 \
             net.core.wmem_max=16777216)"
        );
    } else {
        info!(
            requested_recv = tuning.udp_socket_recv_buffer,
            effective_recv = actual_recv,
            requested_send = tuning.udp_socket_send_buffer,
            effective_send = actual_send,
            forced = recv_forced && send_forced,
            "configured UDP socket buffers"
        );
    }
}

#[cfg(all(feature = "udp", unix, not(target_os = "linux")))]
fn configure_udp_socket_buffers<S: std::os::fd::AsFd>(socket: &S, tuning: &UdpDirectTuning) {
    let socket = socket2::SockRef::from(socket);
    if let Err(err) = socket.set_recv_buffer_size(tuning.udp_socket_recv_buffer) {
        debug!(%err, requested = tuning.udp_socket_recv_buffer, "failed to raise UDP receive buffer");
    }
    if let Err(err) = socket.set_send_buffer_size(tuning.udp_socket_send_buffer) {
        debug!(%err, requested = tuning.udp_socket_send_buffer, "failed to raise UDP send buffer");
    }

    debug!(
        requested_recv = tuning.udp_socket_recv_buffer,
        actual_recv = ?socket.recv_buffer_size().ok(),
        requested_send = tuning.udp_socket_send_buffer,
        actual_send = ?socket.send_buffer_size().ok(),
        "configured UDP socket buffers"
    );
}

#[cfg(not(feature = "udp"))]
fn configure_udp_socket_buffers<S>(_socket: &S, _tuning: &UdpDirectTuning) {}

/// Where a STUN target came from. This is used only for logging/diagnostics: the
/// candidate addresses themselves remain plain `SocketAddr`s on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StunSource {
    /// User supplied `--stun-server` / `BORE_STUN_SERVER`.
    Override,
    /// Built-in public STUN default (Cloudflare/Google).
    PublicDefault,
    /// The bore server's own UDP control/STUN endpoint, used last.
    BoreFallback,
    /// STUN server selected by the peer and advertised by the rendezvous server.
    PeerHint,
    /// A single explicitly resolved target used by legacy/internal callers.
    Single,
}

impl StunSource {
    /// Stable lowercase label used in logs and human-readable diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            StunSource::Override => "override",
            StunSource::PublicDefault => "public-default",
            StunSource::BoreFallback => "bore-fallback",
            StunSource::PeerHint => "peer-hint",
            StunSource::Single => "single",
        }
    }
}

/// A resolved STUN endpoint plus the original host:port that produced it.
#[derive(Debug, Clone)]
pub struct StunTarget {
    /// The configured host:port, before DNS resolution.
    pub requested: String,
    /// The resolved UDP endpoint used for the binding request.
    pub addr: SocketAddr,
    /// Why this target is in the candidate chain.
    pub source: StunSource,
}

/// The STUN server that successfully produced this peer's reflexive address.
#[derive(Debug, Clone)]
pub struct SelectedStun {
    /// Configured host:port before DNS resolution.
    pub requested: String,
    /// Resolved UDP endpoint that answered the binding request.
    pub addr: SocketAddr,
    /// Why this STUN target was part of the chain.
    pub source: StunSource,
    /// Public reflexive address reported by the STUN server.
    pub reflexive: SocketAddr,
}

/// Candidate gathering result with enough metadata for useful operator logs.
#[derive(Debug, Clone)]
pub struct CandidateDiscovery {
    /// Candidate addresses to send over the bore control channel.
    pub candidates: Vec<SocketAddr>,
    /// Roles for the candidate list, in the same order as `candidates`.
    pub candidate_kinds: Vec<UdpCandidateKind>,
    /// Local UDP socket address used for discovery and punching.
    pub local_addr: Option<SocketAddr>,
    /// STUN server that produced the selected reflexive candidate, if any.
    pub selected_stun: Option<SelectedStun>,
    /// Number of resolved STUN targets attempted.
    pub attempted_stun: usize,
}

/// Host:port of the bore server's own STUN responder for a control endpoint.
/// `https://`/`http://` endpoints may front TCP on 443/80, while the STUN
/// responder still lives on bore's control UDP port.
pub fn bore_stun_target(host: &str, port: u16) -> String {
    let stun_port = if port == 443 || port == 80 {
        CONTROL_PORT
    } else {
        port
    };
    format!("{host}:{stun_port}")
}

fn live_stun_target_specs(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Vec<(String, StunSource)> {
    live_stun_target_specs_with_hint(host, port, override_server, None)
}

fn push_unique_stun_target(
    targets: &mut Vec<(String, StunSource)>,
    target: String,
    source: StunSource,
) {
    if !targets.iter().any(|(existing, _)| existing == &target) {
        targets.push((target, source));
    }
}

fn live_stun_target_specs_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Vec<(String, StunSource)> {
    if let Some(server) = override_server {
        return vec![(server.to_string(), StunSource::Override)];
    }

    let mut targets = Vec::new();
    if let Some(peer_hint) = peer_hint.filter(|hint| !hint.is_empty()) {
        push_unique_stun_target(&mut targets, peer_hint.to_string(), StunSource::PeerHint);
    }
    for server in PUBLIC_STUN {
        push_unique_stun_target(
            &mut targets,
            (*server).to_string(),
            StunSource::PublicDefault,
        );
    }
    push_unique_stun_target(
        &mut targets,
        bore_stun_target(host, port),
        StunSource::BoreFallback,
    );
    targets
}

/// The live tunnel STUN chain before DNS resolution. Useful for logs/help/tests.
pub fn live_stun_target_names(host: &str, port: u16, override_server: Option<&str>) -> Vec<String> {
    live_stun_target_specs(host, port, override_server)
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

/// The live STUN chain with an optional peer-selected STUN server tried first.
/// An explicit local override still wins and disables both defaults and hints.
pub fn live_stun_target_names_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Vec<String> {
    live_stun_target_specs_with_hint(host, port, override_server, peer_hint)
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

async fn resolve_stun_target(target: &str) -> Result<SocketAddr> {
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host(target)
        .await
        .with_context(|| format!("failed to resolve STUN server {target}"))?
        .collect();
    addrs
        .iter()
        .copied()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addrs.pop())
        .with_context(|| format!("no addresses for STUN server {target}"))
}

/// Resolve the live tunnel STUN chain. With an explicit override the chain has a
/// single element. Without one, public STUN on common ports is tried first and
/// the bore server's own STUN endpoint is kept as the final fallback.
pub async fn resolve_live_stun_targets(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Result<Vec<StunTarget>> {
    resolve_live_stun_targets_with_hint(host, port, override_server, None).await
}

/// Resolve the live tunnel STUN chain, optionally trying the peer-selected STUN
/// first. If the hinted STUN is unreachable, candidate gathering continues with
/// the remaining public/default and bore-server fallback targets.
pub async fn resolve_live_stun_targets_with_hint(
    host: &str,
    port: u16,
    override_server: Option<&str>,
    peer_hint: Option<&str>,
) -> Result<Vec<StunTarget>> {
    let mut targets = Vec::new();
    for (requested, source) in
        live_stun_target_specs_with_hint(host, port, override_server, peer_hint)
    {
        match resolve_stun_target(&requested).await {
            Ok(addr) => {
                debug!(
                    stun_server = %requested,
                    %addr,
                    stun_source = source.as_str(),
                    "resolved STUN server"
                );
                targets.push(StunTarget {
                    requested,
                    addr,
                    source,
                });
            }
            Err(err) => warn!(
                %err,
                stun_server = %requested,
                stun_source = source.as_str(),
                "failed to resolve STUN server; trying next candidate"
            ),
        }
    }
    if targets.is_empty() {
        bail!("no STUN servers could be resolved")
    }
    Ok(targets)
}

/// Gather this peer's candidate addresses: the STUN-discovered reflexive address
/// (for traversal across NATs) plus the primary local address (for same-LAN
/// peers). Optionally adds a router-mapped candidate (`port_map`, UPnP-IGD) and
/// predicted symmetric-NAT ports (`port_prediction`). Best-effort: an empty list
/// means no usable candidate was found.
pub async fn gather_candidates(
    socket: &UdpSocket,
    stun: SocketAddr,
    port_map: bool,
    port_prediction: bool,
) -> Vec<SocketAddr> {
    let target = StunTarget {
        requested: stun.to_string(),
        addr: stun,
        source: StunSource::Single,
    };
    gather_candidates_from_stun_targets(socket, &[target], port_map, port_prediction)
        .await
        .candidates
}

/// Gather this peer's candidate addresses using a fallback chain of STUN
/// targets. The first STUN server that returns a reflexive address is selected;
/// later servers are skipped to keep live tunnel setup fast. The local candidate
/// is still added even if every STUN probe fails, so same-LAN peers can connect
/// and all other cases fall back to the relay cleanly.
pub async fn gather_candidates_from_stun_targets(
    socket: &UdpSocket,
    stun_targets: &[StunTarget],
    port_map: bool,
    port_prediction: bool,
) -> CandidateDiscovery {
    let mut candidates = Vec::new();
    let mut candidate_kinds = Vec::new();
    let local_addr = socket.local_addr().ok();
    let local_port = local_addr.map(|a| a.port()).unwrap_or(0);
    let mut selected_stun = None;

    info!(
        udp_local_addr = ?local_addr,
        requested_stun = stun_targets.len(),
        "starting UDP candidate discovery"
    );

    for target in stun_targets {
        debug!(
            stun_server = %target.requested,
            stun_addr = %target.addr,
            stun_source = target.source.as_str(),
            "probing STUN server for UDP candidates"
        );
        match discover_reflexive(socket, target.addr).await {
            Ok(addr) => {
                info!(
                    stun_server = %target.requested,
                    stun_addr = %target.addr,
                    stun_source = target.source.as_str(),
                    reflexive = %addr,
                    udp_local_addr = ?local_addr,
                    "selected STUN server for UDP candidates"
                );
                candidates.push(addr);
                candidate_kinds.push(UdpCandidateKind::Reflexive);
                selected_stun = Some(SelectedStun {
                    requested: target.requested.clone(),
                    addr: target.addr,
                    source: target.source,
                    reflexive: addr,
                });

                // Symmetric NATs allocate a *different* external port per
                // destination, so the port toward the peer differs from the one seen
                // by STUN — often sequentially. When explicitly enabled, advertise a
                // few ports just past the reflexive one as extra candidates. Strictly
                // opt-in: advertising/punching extra ports may look like a scan to
                // strict firewalls.
                if port_prediction {
                    let base = addr.port();
                    let mut added = 0u16;
                    for delta in 1..=PREDICT_RANGE {
                        if let Some(port) = base.checked_add(delta) {
                            candidates.push(SocketAddr::new(addr.ip(), port));
                            candidate_kinds.push(UdpCandidateKind::Predicted);
                            added += 1;
                        }
                    }
                    warn!(
                        reflexive_port = base,
                        predicted = added,
                        "port prediction ENABLED — advertising predicted symmetric-NAT ports \
                         (best-effort; may look like a scan to strict firewalls)"
                    );
                }
                break;
            }
            Err(err) => warn!(
                %err,
                stun_server = %target.requested,
                stun_addr = %target.addr,
                stun_source = target.source.as_str(),
                "STUN reflexive discovery failed; trying next STUN server"
            ),
        }
    }

    if selected_stun.is_none() {
        warn!(
            attempted = stun_targets.len(),
            "all STUN probes failed — no public address discovered; offering only non-STUN \
             candidates. Direct UDP is unlikely across NAT/firewalls and will fall back to \
             the relay if the peer cannot reach them"
        );
    }

    // Router-mapped candidate via UPnP-IGD, when explicitly enabled.
    #[cfg(feature = "udp")]
    if port_map {
        match upnp_candidate(local_port).await {
            Ok(addr) => {
                warn!(%addr, "UPnP-IGD port mapping ENABLED — added router-mapped candidate");
                if !candidates.contains(&addr) {
                    candidates.push(addr);
                    candidate_kinds.push(UdpCandidateKind::RouterMapped);
                }
            }
            Err(err) => debug!(%err, "UPnP-IGD port mapping failed; skipping that candidate"),
        }
    }
    #[cfg(not(feature = "udp"))]
    let _ = port_map;

    // A local candidate lets two peers behind the same NAT connect directly.
    if let Some(ip) = primary_local_ip() {
        let local = SocketAddr::new(ip, local_port);
        if !candidates.contains(&local) {
            candidates.push(local);
            candidate_kinds.push(UdpCandidateKind::Local);
        }
    }
    info!(
        udp_local_addr = ?local_addr,
        selected_stun = selected_stun.as_ref().map(|s| s.requested.as_str()),
        candidates = ?candidates,
        "finished UDP candidate discovery"
    );
    CandidateDiscovery {
        candidates,
        candidate_kinds,
        local_addr,
        selected_stun,
        attempted_stun: stun_targets.len(),
    }
}

/// Ask the local router (UPnP-IGD) to map an external UDP port to our socket and
/// return the resulting public `ip:port` candidate. Helps strict *home* routers
/// with a public WAN IP; useless behind carrier-grade NAT (the mapped address is
/// then itself a private/CGNAT address).
#[cfg(feature = "udp")]
async fn upnp_candidate(local_port: u16) -> Result<SocketAddr> {
    use igd_next::aio::tokio as igd;
    use igd_next::{PortMappingProtocol, SearchOptions};

    let local_ip = primary_local_ip().context("no local IPv4 for UPnP mapping")?;
    let local = SocketAddr::new(local_ip, local_port);
    let options = SearchOptions {
        timeout: Some(Duration::from_secs(2)),
        ..Default::default()
    };
    let gateway = igd::search_gateway(options)
        .await
        .context("no UPnP-IGD gateway found")?;
    let external_port = gateway
        .add_any_port(PortMappingProtocol::UDP, local, 120, "bore")
        .await
        .context("UPnP-IGD port mapping request failed")?;
    let wan = gateway
        .get_external_ip()
        .await
        .context("UPnP-IGD external IP query failed")?;
    Ok(SocketAddr::new(wan, external_port))
}

/// Resolve the STUN server address: the explicit override (`host:port`), or the
/// control endpoint's host and port (a self-hosted bore server with `--udp`
/// doubles as the STUN server).
///
/// The STUN responder binds the server's control port (UDP). When the control
/// endpoint uses a TLS/HTTP default port (`https://` → 443, `http://` → 80),
/// that port fronts the control connection but is *not* where STUN listens, so
/// the default STUN target falls back to the well-known [`CONTROL_PORT`]. Pass
/// an explicit `--stun-server` for any non-standard deployment.
pub async fn resolve_stun(
    host: &str,
    port: u16,
    override_server: Option<&str>,
) -> Result<SocketAddr> {
    let target = match override_server {
        Some(server) => server.to_string(),
        None => bore_stun_target(host, port),
    };
    resolve_stun_target(&target).await
}

/// Determine the primary local IPv4 address by inspecting the kernel's chosen
/// source address for an outbound (unconnected, never-sent) socket.
/// Determine this host's primary local IPv4 address for diagnostic reports and
/// same-LAN UDP candidates.
pub fn primary_local_ip() -> Option<IpAddr> {
    let probe = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // No packets are sent; `connect` only sets the default peer so the kernel
    // resolves a route and assigns a source address we can read back.
    probe.connect((Ipv4Addr::new(8, 8, 8, 8), 53)).ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(IpAddr::V4(ip)),
        _ => None,
    }
}

/// Quick STUN probe: bind PORT, discover reflexive via a single STUN server,
/// return Some(true) if the NAT preserved the port, Some(false) if remapped,
/// None if the STUN probe itself failed. The socket is closed on return.
pub async fn check_reflexive_port(port: u16, stun_addr: SocketAddr) -> Option<bool> {
    let socket = bind_socket(port).await.ok()?;
    match discover_reflexive(&socket, stun_addr).await {
        Ok(addr) => Some(addr.port() == port),
        Err(_) => None,
    }
}

/// Send a STUN binding request and parse the reflexive address from the reply.
pub async fn discover_reflexive(socket: &UdpSocket, stun: SocketAddr) -> Result<SocketAddr> {
    let (request, txid) = stun::binding_request();
    let mut buf = [0u8; 512];
    for attempt in 0..3 {
        socket.send_to(&request, stun).await?;
        match timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from.ip() == stun.ip() => {
                if let Some(addr) = stun::parse_response(&buf[..n], &txid) {
                    return Ok(addr);
                }
                debug!(%stun, attempt, "STUN response has mismatched txid; retrying");
            }
            Ok(Ok((_n, from))) => {
                debug!(%stun, %from, attempt, "STUN response from unexpected source; retrying");
                continue;
            }
            Ok(Err(err)) => {
                return Err(err).context(format!("STUN recv failed (attempt {attempt})"))
            }
            Err(_) => {
                if attempt < 2 {
                    debug!(%stun, retry = attempt + 1, "STUN request timed out, retrying");
                }
                continue;
            }
        }
    }
    warn!(%stun, "no STUN response after 3 attempts");
    bail!("no STUN response from {stun}")
}

/// Public STUN servers (distinct providers) used first by live UDP candidate
/// discovery (unless `--stun-server` overrides it) and probed by `bore test-udp`
/// to classify local NAT mapping behaviour. Cloudflare uses the standard STUN
/// port (3478), which commonly passes firewall policy that blocks bore's control
/// UDP port; Google adds provider diversity and fallback coverage.
pub const PUBLIC_STUN: &[&str] = &[
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];

/// One STUN server's view of our public (reflexive) mapping, gathered on a single
/// shared socket so the *variation* across servers reveals the NAT's mapping
/// behaviour.
#[derive(Debug, Clone)]
pub struct StunObservation {
    /// The STUN server queried (host:port).
    pub server: String,
    /// The reflexive address that server reported for our socket.
    pub reflexive: SocketAddr,
}

/// NAT mapping behaviour classified from multiple [`StunObservation`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatClass {
    /// No STUN server answered — UDP is most likely blocked outbound.
    Blocked,
    /// A reflexive address equals a local address: a public IP, no NAT.
    Open,
    /// Only one server answered: egress works but mapping can't be classified.
    Inconclusive,
    /// Same public `ip:port` toward every server: endpoint-independent mapping
    /// (full/restricted cone). Hole-punching works.
    Cone,
    /// Public port varies per destination: endpoint-dependent mapping (symmetric
    /// NAT). `sequential` is true when the observed ports increase in small,
    /// regular steps (so `--try-port-prediction` has a chance).
    Symmetric {
        /// Whether the per-destination ports look sequentially allocated.
        sequential: bool,
    },
}

/// Whether an IPv4 address is in the carrier-grade NAT range `100.64.0.0/10`.
fn is_cgnat(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xc0) == 0x40
        }
        IpAddr::V6(_) => false,
    }
}

/// Whether an address is non-routable on the public internet (RFC1918, loopback,
/// link-local, or CGNAT) — a "public" reflexive in this range means another NAT
/// sits upstream.
fn is_non_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local() || is_cgnat(ip),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Short parenthetical tag describing an address's routability, for the report.
fn routability_note(ip: IpAddr) -> &'static str {
    if is_cgnat(ip) {
        " (CGNAT 100.64/10)"
    } else if is_non_routable(ip) {
        " (private)"
    } else {
        ""
    }
}

/// Classify NAT mapping behaviour from STUN observations taken on one socket.
/// `local_ips` are this host's own addresses (to detect a public IP with no NAT).
pub fn classify_nat(local_ips: &[IpAddr], obs: &[StunObservation]) -> NatClass {
    if obs.is_empty() {
        return NatClass::Blocked;
    }
    if obs.iter().any(|o| local_ips.contains(&o.reflexive.ip())) {
        return NatClass::Open;
    }
    if obs.len() == 1 {
        return NatClass::Inconclusive;
    }
    let ports: BTreeSet<u16> = obs.iter().map(|o| o.reflexive.port()).collect();
    let ips: BTreeSet<IpAddr> = obs.iter().map(|o| o.reflexive.ip()).collect();
    if ports.len() == 1 && ips.len() == 1 {
        return NatClass::Cone;
    }
    let sorted: Vec<u16> = ports.into_iter().collect();
    let sequential = sorted
        .windows(2)
        .all(|w| (1..=8).contains(&w[1].saturating_sub(w[0])));
    NatClass::Symmetric { sequential }
}

/// Resolve `host:port` and run one STUN reflexive probe on `socket`.
async fn probe_one(socket: &UdpSocket, hostport: &str) -> Result<SocketAddr> {
    let addr = tokio::net::lookup_host(hostport)
        .await
        .with_context(|| format!("resolve {hostport}"))?
        .next()
        .with_context(|| format!("no addresses for {hostport}"))?;
    discover_reflexive(socket, addr).await
}

/// Query the local UPnP-IGD gateway for its external (WAN) IP, without creating a
/// mapping — a diagnostic probe for whether `--upnp` can do anything here.
#[cfg(feature = "udp")]
async fn upnp_external_ip() -> Result<IpAddr> {
    use igd_next::aio::tokio as igd;
    use igd_next::SearchOptions;
    let options = SearchOptions {
        timeout: Some(Duration::from_secs(2)),
        ..Default::default()
    };
    let gateway = igd::search_gateway(options)
        .await
        .context("no UPnP-IGD gateway found")?;
    gateway
        .get_external_ip()
        .await
        .context("UPnP-IGD external IP query failed")
}

/// Probe this host's UDP / NAT / firewall situation for hole-punching and print a
/// human-readable report with actionable advice. Opens no tunnel; reachable via
/// `bore test-udp`.
///
/// `bore_target` is the `--to` server's `(host, port)` — when given, the bore
/// server's own STUN responder is probed too (testing reachability of *your*
/// deployment's UDP). `stun_override` is an extra `--stun-server host:port`.
/// `preferred_port` mirrors `--nat-udp-preferred-port`: when non-zero the probe
/// binds that exact UDP port, so you can test whether the port you intend to open
/// in a firewall actually works (0 = a random ephemeral port).
pub async fn diagnose(
    bore_target: Option<(String, u16)>,
    stun_override: Option<&str>,
    preferred_port: u16,
) -> Result<()> {
    println!("bore UDP / NAT diagnostic");
    println!("=========================");

    // 1. Socket + local address.
    let socket = bind_socket(preferred_port).await?;
    let local_port = socket.local_addr()?.port();
    let local_ip = primary_local_ip();
    let port_kind = if preferred_port == 0 {
        "ephemeral"
    } else {
        "fixed (--nat-udp-preferred-port)"
    };
    println!();
    println!("Local UDP socket : 0.0.0.0:{local_port} ({port_kind})");
    match local_ip {
        Some(ip) => println!("Primary local IP : {ip}{}", routability_note(ip)),
        None => println!("Primary local IP : <none found>"),
    }

    // 2. Probe public STUN servers on the SAME socket — the variation across
    //    servers is what reveals cone vs symmetric mapping.
    println!();
    println!("STUN probes (a public IP here means UDP egress works):");
    let mut public_obs: Vec<StunObservation> = Vec::new();
    for server in PUBLIC_STUN {
        match probe_one(&socket, server).await {
            Ok(refl) => {
                println!("  [ ok ] {server:<26} -> {refl}");
                public_obs.push(StunObservation {
                    server: (*server).to_string(),
                    reflexive: refl,
                });
            }
            Err(err) => println!("  [FAIL] {server:<26} -> {err}"),
        }
    }
    if let Some(server) = stun_override {
        match probe_one(&socket, server).await {
            Ok(refl) => println!("  [ ok ] {server:<26} -> {refl}  (--stun-server)"),
            Err(err) => println!("  [FAIL] {server:<26} -> {err}  (--stun-server)"),
        }
    }

    // 3. Probe the bore server's own STUN responder, if --to was given.
    let mut bore_reachable: Option<bool> = None;
    if let Some((host, port)) = bore_target.as_ref() {
        match resolve_stun(host, *port, None).await {
            Ok(addr) => match discover_reflexive(&socket, addr).await {
                Ok(refl) => {
                    println!("  [ ok ] bore server {addr:<20} -> {refl}  (your --to)");
                    bore_reachable = Some(true);
                }
                Err(err) => {
                    println!("  [FAIL] bore server {addr:<20} -> {err}  (your --to)");
                    bore_reachable = Some(false);
                }
            },
            Err(err) => println!("  [FAIL] bore server resolve -> {err}  (your --to)"),
        }
    }

    // 4. Classify and report a verdict.
    let local_ips: Vec<IpAddr> = local_ip.into_iter().collect();
    let class = classify_nat(&local_ips, &public_obs);
    println!();
    println!("Verdict");
    println!("-------");
    match &class {
        NatClass::Blocked => {
            println!("UDP appears BLOCKED outbound: no public STUN server answered.");
            println!("  -> Direct UDP hole-punching is impossible from this host.");
            println!("  -> Tunnels still work over the TCP relay (--udp simply has no effect).");
            println!("  Fix: allow outbound UDP, or run from a network that permits it.");
        }
        NatClass::Open => {
            println!("PUBLIC IP / no NAT: this socket is directly reachable.");
            println!("  -> Hole-punching trivially works; an ideal provider.");
        }
        NatClass::Inconclusive => {
            println!("UDP egress WORKS but only one server answered — cannot classify the");
            println!("  NAT mapping (need >=2 distinct STUN servers). Re-run to retry.");
        }
        NatClass::Cone => {
            println!("CONE NAT (endpoint-independent mapping): same public port to every server.");
            println!("  -> Hole-punching WORKS from your side. If the direct path still fails,");
            println!("     the *peer* is the blocker (symmetric/CGNAT/UDP-blocked on their end).");
        }
        NatClass::Symmetric { sequential } => {
            println!(
                "SYMMETRIC NAT (endpoint-dependent mapping): public port changes per destination."
            );
            if *sequential {
                println!(
                    "  Ports look SEQUENTIAL -> --try-port-prediction has a chance (best-effort)."
                );
            } else {
                println!("  Ports look RANDOM -> port prediction is unlikely to help.");
            }
            println!(
                "  -> Direct path works only if the *other* peer is cone/open. Symmetric+symmetric"
            );
            println!("     or symmetric+CGNAT cannot punch and falls back to the relay.");
        }
    }

    // 5. Extra signals: port preservation, CGNAT/double-NAT, bore-server hairpin.
    if let Some(first) = public_obs.first() {
        let refl = first.reflexive;
        println!();
        if refl.port() == local_port {
            println!(
                "Port preservation: YES (local {local_port} == public {}).",
                refl.port()
            );
        } else {
            println!(
                "Port preservation: no  (local {local_port} -> public {}).",
                refl.port()
            );
        }
        if is_cgnat(refl.ip()) {
            println!(
                "CGNAT detected: public address {} is in 100.64.0.0/10.",
                refl.ip()
            );
            println!("  -> P2P is unlikely; the relay is the reliable path here.");
        } else if is_non_routable(refl.ip()) {
            println!(
                "Double-NAT: the 'public' address {} is itself private — another NAT upstream.",
                refl.ip()
            );
        }
    }
    if bore_reachable == Some(false) && !public_obs.is_empty() {
        println!();
        println!("Note: public STUN works but YOUR bore server's UDP did NOT answer.");
        println!("  Likely co-location/hairpin (this host shares the server's machine/LAN),");
        println!("  or UDP to the control port is not open server-side. Run the provider from a");
        println!("  different network, or pass --stun-server <public:port> so candidates still");
        println!("  get a public IP.");
    }

    // 6. UPnP-IGD reachability (home routers).
    println!();
    #[cfg(feature = "udp")]
    match upnp_external_ip().await {
        Ok(ip) => {
            println!(
                "UPnP-IGD router : FOUND, external IP {ip}{}.",
                routability_note(ip)
            );
            println!("  -> --upnp can map a router port here (helps strict home NATs).");
        }
        Err(err) => println!("UPnP-IGD router : none ({err}); --upnp would have no effect here."),
    }
    #[cfg(not(feature = "udp"))]
    println!("UPnP-IGD router : probe skipped (built without the `udp` feature).");

    Ok(())
}

/// Open NAT mappings toward every peer candidate by sending a few small
/// datagrams. QUIC path validation does the real liveness check afterward.
#[cfg(feature = "udp")]
async fn punch(socket: &UdpSocket, peers: &[SocketAddr]) {
    for _ in 0..5 {
        for peer in peers {
            let _ = socket.send_to(b"bore-punch", peer).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One native QUIC bidirectional stream wrapped as an `AsyncRead`/`AsyncWrite`
/// carrier for a single proxied connection. Keeps the connection and endpoint
/// alive for as long as the stream is in use.
#[cfg(feature = "udp")]
pub struct QuicTransport {
    recv: quinn::RecvStream,
    send: quinn::SendStream,
    _conn: Connection,
    _endpoint: Endpoint,
}

/// An authenticated direct QUIC connection between a consumer and a provider.
/// Proxied connections are carried over **native QUIC streams** (one bidi each,
/// via [`DirectConn::open_stream`] / [`DirectConn::accept_stream`]), so a lost
/// packet on one connection's stream does not stall the others (no head-of-line
/// blocking — unlike multiplexing yamux over a single QUIC stream). Cheap to clone
/// (both fields are handles).
#[cfg(feature = "udp")]
#[derive(Clone)]
pub struct DirectConn {
    conn: Connection,
    endpoint: Endpoint,
}

/// Outcome of a best-effort datagram send on the direct QUIC path.
///
/// `TooLarge` is a transient PER-PACKET condition (the packet is bigger than the
/// current QUIC path-MTU allows), NOT a link failure — the caller drops the
/// packet and keeps the link alive. Genuine link death is an `Err` instead.
#[cfg(feature = "udp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramSend {
    /// Queued for transmission.
    Sent,
    /// Dropped: larger than the current path-MTU datagram limit.
    TooLarge,
}

#[cfg(feature = "udp")]
impl DirectConn {
    /// Open a new native QUIC bidi stream for one proxied connection (consumer).
    pub async fn open_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.open_bi().await.context("open_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Accept the next native QUIC bidi stream for one proxied connection (provider).
    pub async fn accept_stream(&self) -> Result<QuicTransport> {
        let (send, recv) = self.conn.accept_bi().await.context("accept_bi failed")?;
        Ok(QuicTransport {
            recv,
            send,
            _conn: self.conn.clone(),
            _endpoint: self.endpoint.clone(),
        })
    }

    /// Resolve when the QUIC connection closes (peer gone, idle timeout, or a
    /// graceful close), so the consumer can re-negotiate or fall back to the relay.
    pub async fn closed(&self) {
        self.conn.closed().await;
    }

    /// Gracefully close the QUIC connection so the peer immediately reverts or renews.
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"vhost direct path closed");
    }

    /// Snapshot the current QUIC connection statistics for diagnostics.
    pub fn stats(&self) -> quinn::ConnectionStats {
        self.conn.stats()
    }

    /// Snapshot the current path MTU-dependent datagram size, if available.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
    }

    /// Send an IP packet as a QUIC unreliable datagram. Non-blocking.
    ///
    /// Returns `Ok(DatagramSend::TooLarge)` — NOT `Err` — when the packet
    /// exceeds the current QUIC path-MTU datagram limit. That happens whenever
    /// the TUN MTU runs ahead of the path MTU: throughout the initial MTU
    /// discovery window, and briefly after every switch to the direct path (the
    /// TUN starts at its configured MTU; the PMTU monitor narrows it once QUIC
    /// settles). The caller MUST drop such a packet and keep going — it is a
    /// transient per-packet condition, not a link failure. The VPN bridge
    /// counts these and warns after >10 s.
    ///
    /// `Err` is reserved for genuine link death (`ConnectionLost`, datagrams
    /// `Disabled`/`UnsupportedByPeer`) so the bridge tears down and reconnects.
    ///
    /// quinn 0.11 silently drops the *oldest* queued datagram when the send
    /// buffer is full, so calling this from the uplink hot loop is safe without
    /// backpressure.
    pub fn send_datagram(&self, pkt: bytes::Bytes) -> Result<DatagramSend> {
        match self.conn.send_datagram(pkt) {
            Ok(()) => Ok(DatagramSend::Sent),
            Err(quinn::SendDatagramError::TooLarge) => Ok(DatagramSend::TooLarge),
            Err(e) => Err(anyhow::anyhow!("send_datagram: {e}")),
        }
    }

    /// Read the next QUIC datagram. Resolves when a datagram arrives or the
    /// connection closes (in which case `Err` signals path death to the bridge).
    pub async fn read_datagram(&self) -> Result<bytes::Bytes> {
        self.conn.read_datagram().await.context("read_datagram")
    }

    /// The connection's resolved remote address (the winning peer candidate).
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Open an ADDITIONAL authenticated QUIC connection to the SAME peer over the
    /// SAME endpoint/socket (VPN direct-path carrier, Fix #3a). The hole-punched
    /// 5-tuple is already open, so no new punch is needed — quinn assigns the new
    /// connection fresh connection IDs and the peer's server endpoint demuxes it.
    /// Each carrier gets its OWN congestion controller, so N carriers give a
    /// single high-BDP VPN flow ~N× the in-flight window (parallel-stream effect),
    /// which a lone loss-bound flow cannot reach. Consumer-side handshake (writes
    /// its token first, then reads the peer's), mirroring [`connect_direct`].
    pub async fn open_sibling(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        let peer = self.conn.remote_address();
        let conn = self
            .endpoint
            .connect(peer, "bore")
            .context("failed to start direct carrier connect")?
            .await
            .context("direct carrier QUIC handshake failed")?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .context("carrier auth open_bi failed")?;
        send.write_all(&token).await?;
        send.flush().await?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("direct carrier token mismatch");
        }
        let _ = send.finish();
        debug!(%peer, "direct carrier connection established (consumer, token verified)");
        Ok(DirectConn {
            conn,
            endpoint: self.endpoint.clone(),
        })
    }
}

// quinn's streams carry inherent `poll_read`/`poll_write` methods (with quinn's
// own error types) that shadow the trait methods, so delegate with fully
// qualified trait syntax to reach the tokio `AsyncRead`/`AsyncWrite` impls.
#[cfg(feature = "udp")]
impl AsyncRead for QuicTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

#[cfg(feature = "udp")]
impl AsyncWrite for QuicTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

/// Consumer side: punch toward `peers`, connect a QUIC client over `socket`, and
/// authenticate the connection with `token` on a dedicated stream. Returns the
/// authenticated [`DirectConn`]; proxied connections then ride native QUIC streams
/// opened on it. The consumer opens the auth stream.
#[cfg(feature = "udp")]
pub async fn connect_direct(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    if peers.is_empty() {
        bail!("no peer candidates to connect to");
    }
    configure_udp_socket_buffers(&socket, &tuning);
    let local_addr = socket.local_addr().ok();
    info!(
        udp_local_addr = ?local_addr,
        peer_candidates = ?peers,
        "consumer punching UDP peer candidates"
    );
    punch(&socket, &peers).await;
    let endpoint = client_endpoint(socket, &tuning)?;

    // Try all candidates concurrently under a single total budget (not a full
    // timeout *per* candidate): with N candidates the serial worst case was
    // N * NETWORK_TIMEOUT (6-21s for predicted/UPnP/local lists). `select_ok`
    // returns the first handshake that completes and verifies its token; the
    // losing connects are dropped (cancelled). Per-candidate errors are collected
    // in a shared Vec so the final warn includes each candidate's failure reason.
    let errors: Arc<Mutex<Vec<(SocketAddr, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let attempts: Vec<_> = peers
        .iter()
        .map(|&peer| {
            let endpoint = endpoint.clone();
            let errors = Arc::clone(&errors);
            Box::pin(async move {
                debug!(%peer, "attempting direct QUIC candidate");
                let connecting = match endpoint.connect(peer, "bore") {
                    Ok(connecting) => connecting,
                    Err(err) => {
                        let msg = format!("start failed: {err}");
                        debug!(%peer, %err, "failed to start direct QUIC candidate");
                        errors.lock().unwrap().push((peer, msg));
                        return Err(err.into());
                    }
                };
                let conn = match connecting.await {
                    Ok(conn) => conn,
                    Err(err) => {
                        let msg = format!("{err}");
                        debug!(%peer, %err, "direct QUIC candidate failed");
                        errors.lock().unwrap().push((peer, msg));
                        return Err(err.into());
                    }
                };
                trace!(%peer, "QUIC connected");
                // Authenticate the connection once, on a dedicated stream: consumer
                // writes its token first, then reads the peer's. Data streams opened
                // afterward are trusted (same authenticated QUIC connection).
                let (mut send, mut recv) = conn.open_bi().await.context("auth open_bi failed")?;
                send.write_all(&token).await?;
                send.flush().await?;
                let mut peer_token = [0u8; TOKEN_LEN];
                recv.read_exact(&mut peer_token).await?;
                if !tokens_match(&token, &peer_token) {
                    let msg = "token mismatch".to_string();
                    warn!(%peer, "direct QUIC candidate failed token verification");
                    errors.lock().unwrap().push((peer, msg));
                    bail!("direct path token mismatch");
                }
                let _ = send.finish();
                info!(target_addr = %peer, peer = %conn.remote_address(),
                    "direct udp connection established (consumer, token verified)");
                let dc = DirectConn { conn, endpoint };
                debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (consumer)");
                anyhow::Ok(dc)
            })
        })
        .collect();

    match timeout(NETWORK_TIMEOUT, futures_util::future::select_ok(attempts)).await {
        Ok(Ok((conn, _losers))) => Ok(conn),
        Ok(Err(err)) => {
            let err_summary: Vec<String> = errors
                .lock()
                .unwrap()
                .iter()
                .map(|(addr, msg)| format!("{addr} → {msg}"))
                .collect();
            warn!(
                candidates = ?peers,
                errors = ?err_summary,
                "all {n} direct QUIC candidates failed; falling back to relay",
                n = peers.len(),
            );
            Err(err).context("all direct candidates failed")
        }
        Err(_) => {
            warn!(
                timeout = ?NETWORK_TIMEOUT,
                candidates = ?peers,
                "direct QUIC connect exhausted {NETWORK_TIMEOUT:?} budget \
                 across {n} candidates; none responded — all candidates timed out \
                 (firewall/UDP blocked on both ends, or peer IP unreachable). \
                 Falling back to relay",
                n = peers.len(),
            );
            bail!("direct connect exhausted the {NETWORK_TIMEOUT:?} budget")
        }
    }
}

/// Provider side (QUIC client): dial a public bore server's vhost QUIC endpoint
/// and authenticate for `subdomain`. No hole-punching is needed because the
/// server is public; the provider later accepts native QUIC streams on the
/// returned connection, while the server opens them.
#[cfg(feature = "udp")]
pub async fn vhost_connect(
    socket: UdpSocket,
    server_addr: SocketAddr,
    subdomain: &str,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn> {
    let subdomain_len: u16 = subdomain
        .len()
        .try_into()
        .context("vhost subdomain too long for QUIC auth frame")?;
    configure_udp_socket_buffers(&socket, &tuning);
    let endpoint = client_endpoint(socket, &tuning)?;
    let conn = timeout(
        NETWORK_TIMEOUT,
        endpoint
            .connect(server_addr, "bore")
            .context("failed to start vhost QUIC connect")?,
    )
    .await
    .context("vhost QUIC connect timed out")?
    .context("vhost QUIC handshake failed")?;

    timeout(NETWORK_TIMEOUT, async {
        let (mut send, mut recv) = conn.open_bi().await.context("auth open_bi failed")?;
        send.write_all(&subdomain_len.to_be_bytes()).await?;
        send.write_all(subdomain.as_bytes()).await?;
        send.write_all(&token).await?;
        send.flush().await?;

        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("vhost direct path token mismatch");
        }

        let _ = send.finish();
        info!(server = %server_addr, subdomain, "vhost direct udp connection established");
        let dc = DirectConn { conn, endpoint };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (vhost consumer)");
        Ok(dc)
    })
    .await
    .context("vhost QUIC auth timed out")?
}

/// Provider side: a long-lived QUIC server endpoint that accepts direct
/// connections from punched consumers.
#[cfg(feature = "udp")]
pub struct DirectListener {
    endpoint: Endpoint,
}

#[cfg(feature = "udp")]
impl DirectListener {
    /// Punch toward `peers` and start a QUIC server endpoint over `socket`.
    pub async fn new(
        socket: UdpSocket,
        peers: Vec<SocketAddr>,
        tuning: UdpDirectTuning,
    ) -> Result<Self> {
        configure_udp_socket_buffers(&socket, &tuning);
        let local_addr = socket.local_addr().ok();
        info!(
            udp_local_addr = ?local_addr,
            peer_candidates = ?peers,
            "provider punching UDP peer candidates and starting QUIC listener"
        );
        punch(&socket, &peers).await;
        let endpoint = server_endpoint(socket, &tuning)?;
        Ok(DirectListener { endpoint })
    }

    /// Gracefully close the endpoint and all its connections, so the peer detects
    /// the shutdown immediately instead of waiting for the idle timeout.
    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"provider shutting down");
    }

    /// Re-open this endpoint's NAT mapping toward a new (e.g. reconnecting)
    /// consumer once the raw socket is owned by `quinn` and can no longer be used
    /// for [`punch`]. Fires a throwaway outbound QUIC connection per candidate:
    /// the consumer is a QUIC client and won't complete it, but the outbound
    /// packets punch the mapping so the consumer's own connection gets through.
    pub fn punch_via_endpoint(&self, peers: &[SocketAddr]) {
        info!(peer_candidates = ?peers, "provider re-punching UDP peer candidates");
        for &peer in peers {
            if let Ok(connecting) = self.endpoint.connect(peer, "bore") {
                tokio::spawn(async move {
                    let _ = timeout(NETWORK_TIMEOUT, connecting).await;
                });
            }
        }
    }

    /// Accept the next direct connection and authenticate it with `token` on a
    /// dedicated stream. The provider reads the peer's token first, then sends its
    /// own. Returns the authenticated [`DirectConn`]; the provider then accepts
    /// native QUIC streams on it (one per proxied connection).
    pub async fn accept(&self, token: [u8; TOKEN_LEN]) -> Result<DirectConn> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("QUIC endpoint closed")?;
        let conn = incoming.await.context("QUIC handshake failed")?;
        let peer = conn.remote_address();
        trace!(%peer, "QUIC accepted");
        let (mut send, mut recv) = conn.accept_bi().await.context("auth accept_bi failed")?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            warn!(
                %peer,
                "rejected direct QUIC connection: token mismatch \
                 (stray connection or mismatched secrets)"
            );
            bail!("direct path token mismatch");
        }
        send.write_all(&token).await?;
        send.flush().await?;
        let _ = send.finish();
        info!(%peer, "accepted direct udp connection (provider, token verified)");
        let dc = DirectConn {
            conn,
            endpoint: self.endpoint.clone(),
        };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (provider)");
        Ok(dc)
    }
}

/// Server side: build a QUIC endpoint for the vhost direct path.
#[cfg(feature = "udp")]
pub(crate) fn vhost_server_endpoint(
    socket: UdpSocket,
    tuning: &UdpDirectTuning,
) -> Result<Endpoint> {
    server_endpoint(socket, tuning)
}

/// Server side (QUIC server): authenticate one accepted vhost direct-path
/// connection and return the verified subdomain plus the trusted connection.
#[cfg(feature = "udp")]
pub async fn vhost_server_handshake(
    conn: quinn::Connection,
    endpoint: Endpoint,
    lookup: impl Fn(&str) -> Option<[u8; TOKEN_LEN]>,
) -> Result<(String, DirectConn)> {
    let peer = conn.remote_address();
    timeout(NETWORK_TIMEOUT, async {
        let (mut send, mut recv) = conn.accept_bi().await.context("auth accept_bi failed")?;

        let mut sub_len = [0u8; 2];
        recv.read_exact(&mut sub_len).await?;
        let sub_len = u16::from_be_bytes(sub_len) as usize;

        let mut subdomain = vec![0u8; sub_len];
        recv.read_exact(&mut subdomain).await?;
        let subdomain = String::from_utf8(subdomain).context("vhost auth subdomain is not UTF-8")?;

        let mut received = [0u8; TOKEN_LEN];
        recv.read_exact(&mut received).await?;

        let expected = lookup(&subdomain).context("unknown vhost direct-path subdomain")?;
        if !tokens_match(&expected, &received) {
            warn!(%peer, subdomain = %subdomain, "rejected vhost direct udp connection: token mismatch");
            bail!("vhost direct path token mismatch");
        }

        send.write_all(&expected).await?;
        send.flush().await?;
        let _ = send.finish();
        info!(%peer, subdomain = %subdomain, "accepted vhost direct udp connection");
        let dc = DirectConn { conn, endpoint };
        debug!(max_datagram = ?dc.max_datagram_size(), "direct conn established (vhost provider)");
        Ok((subdomain, dc))
    })
    .await
    .context("vhost QUIC auth timed out")?
}

/// Build a QUIC client endpoint over an already-bound UDP socket.
#[cfg(feature = "udp")]
fn client_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC client endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

/// Build a QUIC server endpoint over an already-bound UDP socket. It also carries
/// a default client config so it can fire outbound connections to punch its NAT
/// toward reconnecting consumers (see [`DirectListener::punch_via_endpoint`]).
#[cfg(feature = "udp")]
fn server_endpoint(socket: UdpSocket, tuning: &UdpDirectTuning) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config(tuning)?),
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC server endpoint")?;
    endpoint.set_default_client_config(client_config(tuning)?);
    Ok(endpoint)
}

/// Convert a Tokio UDP socket into a nonblocking std socket for `quinn`.
#[cfg(feature = "udp")]
fn into_std(socket: UdpSocket) -> Result<StdUdpSocket> {
    let socket = socket.into_std().context("failed to detach UDP socket")?;
    socket
        .set_nonblocking(true)
        .context("failed to set socket nonblocking")?;
    Ok(socket)
}

#[cfg(feature = "udp")]
fn transport_config(tuning: &UdpDirectTuning) -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(QUIC_KEEPALIVE));
    cfg.max_idle_timeout(Some(QUIC_MAX_IDLE.try_into().expect("valid idle timeout")));

    // High-throughput direct transfers need flow-control windows larger than
    // Quinn's defaults. The values come from the brokered tuning struct, so the
    // server can override them without changing the code path that consumes it.
    cfg.stream_receive_window(tuning.stream_receive_window.into());
    cfg.receive_window(tuning.connection_receive_window.into());
    cfg.send_window(tuning.send_window);

    // TCP relay often benefits from kernel BBR. Use Quinn's BBR controller for
    // the direct QUIC path too, so high-BDP peer-to-peer transfers are not stuck
    // with the default CUBIC behavior when the network favors model-based pacing.
    cfg.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));

    // One native QUIC stream per proxied connection: raise the concurrent-stream
    // limit well above quinn's small default so it is not the bottleneck.
    cfg.max_concurrent_bidi_streams(tuning.max_direct_streams.into());

    // VPN datagram path: pre-allocate large buffers so RX/TX bursts of IP
    // packets don't stall waiting for the application loop to drain them.
    cfg.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    cfg.datagram_send_buffer_size(8 * 1024 * 1024);

    cfg
}

/// QUIC client config: accept any server certificate (the token handshake, not
/// the certificate, authenticates the peer).
#[cfg(feature = "udp")]
fn client_config(tuning: &UdpDirectTuning) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(SkipVerify))
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .context("invalid QUIC client crypto")?;
    let mut config = ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// QUIC server config with a self-signed certificate.
#[cfg(feature = "udp")]
fn server_config(tuning: &UdpDirectTuning) -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["bore".to_string()])
        .context("failed to generate self-signed certificate")?;
    let cert_der = cert.cert.der().clone();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .context("failed to configure QUIC TLS")?
    .with_no_client_auth()
    .with_single_cert(
        vec![cert_der],
        rustls::pki_types::PrivateKeyDer::Pkcs8(key_der),
    )
    .context("invalid QUIC server certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("invalid QUIC server crypto")?;
    let mut config = ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(Arc::new(transport_config(tuning)));
    Ok(config)
}

/// A certificate verifier that accepts any server certificate. Safe here because
/// the peer is authenticated by the shared token, not by its certificate.
#[cfg(feature = "udp")]
#[derive(Debug)]
struct SkipVerify;

#[cfg(feature = "udp")]
impl rustls::client::danger::ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Minimal STUN (RFC 5389) binding-request client and a server responder, used
/// for reflexive-address discovery. Only XOR-MAPPED-ADDRESS is supported.
pub mod stun {
    use super::*;
    use std::net::{Ipv6Addr, SocketAddrV4, SocketAddrV6};

    const MAGIC_COOKIE: u32 = 0x2112_A442;
    const BINDING_REQUEST: u16 = 0x0001;
    const BINDING_SUCCESS: u16 = 0x0101;
    const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

    /// Build a STUN binding request, returning the bytes and the transaction id.
    pub fn binding_request() -> (Vec<u8>, [u8; 12]) {
        use ring::rand::{SecureRandom, SystemRandom};
        let mut txid = [0u8; 12];
        SystemRandom::new()
            .fill(&mut txid)
            .expect("system CSPRNG must not fail");
        let mut msg = Vec::with_capacity(20);
        msg.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes()); // message length: no attributes
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        (msg, txid)
    }

    /// Parse the XOR-MAPPED-ADDRESS from a STUN binding success response.
    pub fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
        if buf.len() < 20 {
            return None;
        }
        let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
        if msg_type != BINDING_SUCCESS {
            return None;
        }
        if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC_COOKIE {
            return None;
        }
        if &buf[8..20] != txid {
            return None;
        }
        let mut pos = 20;
        while pos + 4 <= buf.len() {
            let attr_type = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
            let attr_len = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
            let value_start = pos + 4;
            if value_start + attr_len > buf.len() {
                return None;
            }
            if attr_type == ATTR_XOR_MAPPED_ADDRESS {
                return parse_xor_mapped(&buf[value_start..value_start + attr_len], txid);
            }
            // Attributes are padded to a 4-byte boundary.
            pos = value_start + attr_len.div_ceil(4) * 4;
        }
        None
    }

    fn parse_xor_mapped(value: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
        if value.len() < 4 {
            return None;
        }
        let family = value[1];
        let xport = u16::from_be_bytes([value[2], value[3]]);
        let port = xport ^ (MAGIC_COOKIE >> 16) as u16;
        match family {
            0x01 if value.len() >= 8 => {
                let xaddr = u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                let addr = Ipv4Addr::from(xaddr ^ MAGIC_COOKIE);
                Some(SocketAddr::V4(SocketAddrV4::new(addr, port)))
            }
            0x02 if value.len() >= 20 => {
                let mut key = [0u8; 16];
                key[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
                key[4..].copy_from_slice(txid);
                let mut addr = [0u8; 16];
                for i in 0..16 {
                    addr[i] = value[4 + i] ^ key[i];
                }
                Some(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(addr),
                    port,
                    0,
                    0,
                )))
            }
            _ => None,
        }
    }

    /// Build a STUN binding success response echoing `source` as a
    /// XOR-MAPPED-ADDRESS. Only IPv4 sources are encoded.
    pub fn binding_response(request: &[u8], source: SocketAddr) -> Option<Vec<u8>> {
        if request.len() < 20
            || u16::from_be_bytes([request[0], request[1]]) != BINDING_REQUEST
            || u32::from_be_bytes([request[4], request[5], request[6], request[7]]) != MAGIC_COOKIE
        {
            return None;
        }
        let SocketAddr::V4(v4) = source else {
            return None;
        };
        let xport = v4.port() ^ (MAGIC_COOKIE >> 16) as u16;
        let xaddr = u32::from(*v4.ip()) ^ MAGIC_COOKIE;

        let mut msg = Vec::with_capacity(32);
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes()); // attribute length
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&request[8..20]); // echo transaction id
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes()); // value length
        msg.push(0); // reserved
        msg.push(0x01); // family: IPv4
        msg.extend_from_slice(&xport.to_be_bytes());
        msg.extend_from_slice(&xaddr.to_be_bytes());
        Some(msg)
    }
}

/// Run a minimal STUN responder on `socket`, replying to binding requests with
/// the observed source address. Lets a self-hosted bore server double as the
/// STUN server so no external infrastructure is required.
pub async fn run_stun_responder(socket: UdpSocket) {
    let mut buf = [0u8; 512];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, from)) => {
                if let Some(reply) = stun::binding_response(&buf[..n], from) {
                    if socket.send_to(&reply, from).await.is_ok() {
                        debug!(%from, "STUN reflexive address returned");
                    }
                }
            }
            Err(err) => {
                debug!(%err, "STUN responder recv error");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "udp")]
    use crate::shared::UDP_NONCE_LEN;

    /// `SO_*BUFFORCE` must lift the UDP buffer past `net.core.{w,r}mem_max` when
    /// the process holds CAP_NET_ADMIN. This is the direct-path throughput fix:
    /// without it the kernel silently clamps the 16 MiB request to the sysctl
    /// ceiling, capping a single QUIC flow at ~buffer/RTT. The test only asserts
    /// the strong (forced) outcome when it actually has the capability — under an
    /// unprivileged CI runner it degrades to documenting the clamp, never fails.
    #[cfg(all(feature = "udp", target_os = "linux"))]
    #[test]
    fn udp_buffers_forced_past_sysctl_clamp() {
        use nix::sys::socket::{getsockopt, sockopt};
        use socket2::{Domain, Protocol, Socket, Type};

        let wmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/wmem_max")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let socket =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("create socket");
        let tuning = UdpDirectTuning::default();
        configure_udp_socket_buffers(&socket, &tuning);

        let actual_send = getsockopt(&socket, sockopt::SndBuf).expect("getsockopt SndBuf");

        // Can we force? Probe with a fresh socket so the assertion above is not
        // self-referential.
        let probe =
            Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).expect("create probe");
        let can_force = nix::sys::socket::setsockopt(
            &probe,
            sockopt::SndBufForce,
            &tuning.udp_socket_send_buffer,
        )
        .is_ok();

        if can_force {
            // Forced: effective buffer must reach the request (kernel reports ~2×),
            // and crucially exceed the sysctl ceiling that would otherwise clamp it.
            assert!(
                actual_send >= tuning.udp_socket_send_buffer,
                "forced send buffer {actual_send} < requested {} — force ineffective",
                tuning.udp_socket_send_buffer
            );
            if wmem_max > 0 && wmem_max < tuning.udp_socket_send_buffer {
                assert!(
                    actual_send > wmem_max,
                    "forced send buffer {actual_send} did not exceed wmem_max {wmem_max} \
                     — clamp not bypassed"
                );
            }
        } else {
            // No CAP_NET_ADMIN here: the clamped value must still be the best the
            // kernel allows (>= ceiling), proving the fallback path ran.
            if wmem_max > 0 {
                assert!(
                    actual_send >= wmem_max.min(tuning.udp_socket_send_buffer),
                    "fallback send buffer {actual_send} below kernel ceiling {wmem_max}"
                );
            }
        }
    }

    #[test]
    fn token_is_deterministic_and_keyed() {
        let nonce = [7u8; 16];
        assert_eq!(
            derive_token(Some("s"), &nonce),
            derive_token(Some("s"), &nonce)
        );
        assert_ne!(
            derive_token(Some("s"), &nonce),
            derive_token(Some("t"), &nonce)
        );
        assert_ne!(derive_token(None, &nonce), derive_token(Some("s"), &nonce));
    }

    #[test]
    fn stun_round_trip_ipv4() {
        let (req, txid) = stun::binding_request();
        let source: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let resp = stun::binding_response(&req, source).expect("ipv4 response");
        assert_eq!(stun::parse_response(&resp, &txid), Some(source));
    }

    #[test]
    fn stun_rejects_wrong_transaction_id() {
        let (req, _) = stun::binding_request();
        let source: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let resp = stun::binding_response(&req, source).unwrap();
        assert_eq!(stun::parse_response(&resp, &[0u8; 12]), None);
    }

    #[tokio::test]
    async fn stun_default_falls_back_to_control_port_for_tls_ports() {
        // https:// (443) and http:// (80) front the control connection but not
        // the STUN responder, which lives on the control port.
        let by_port = |p| async move { resolve_stun("127.0.0.1", p, None).await.unwrap() };
        assert_eq!(
            by_port(443).await,
            format!("127.0.0.1:{CONTROL_PORT}").parse().unwrap()
        );
        assert_eq!(
            by_port(80).await,
            format!("127.0.0.1:{CONTROL_PORT}").parse().unwrap()
        );
        // A non-default port is the control port itself; use it as-is.
        assert_eq!(by_port(9000).await, "127.0.0.1:9000".parse().unwrap());
        // An explicit override always wins.
        let over = resolve_stun("127.0.0.1", 443, Some("127.0.0.1:1234"))
            .await
            .unwrap();
        assert_eq!(over, "127.0.0.1:1234".parse().unwrap());
    }

    #[test]
    fn live_stun_chain_prefers_public_servers_then_bore_fallback() {
        let chain = live_stun_target_names("bore.example.com", 443, None);
        assert_eq!(
            chain,
            vec![
                "stun.cloudflare.com:3478".to_string(),
                "stun.l.google.com:19302".to_string(),
                "stun1.l.google.com:19302".to_string(),
                format!("bore.example.com:{CONTROL_PORT}"),
            ]
        );
    }

    #[test]
    fn live_stun_chain_override_is_absolute() {
        assert_eq!(
            live_stun_target_names("bore.example.com", 443, Some("stun.example.net:3478")),
            vec!["stun.example.net:3478".to_string()]
        );
    }

    #[test]
    fn live_stun_chain_uses_peer_hint_first_and_deduplicates() {
        let chain = live_stun_target_names_with_hint(
            "bore.example.com",
            443,
            None,
            Some("stun.l.google.com:19302"),
        );
        assert_eq!(
            chain,
            vec![
                "stun.l.google.com:19302".to_string(),
                "stun.cloudflare.com:3478".to_string(),
                "stun1.l.google.com:19302".to_string(),
                format!("bore.example.com:{CONTROL_PORT}"),
            ]
        );

        let specs = live_stun_target_specs_with_hint(
            "bore.example.com",
            443,
            None,
            Some("stun.l.google.com:19302"),
        );
        assert_eq!(specs[0].1, StunSource::PeerHint);
    }

    #[test]
    fn live_stun_chain_override_ignores_peer_hint() {
        assert_eq!(
            live_stun_target_names_with_hint(
                "bore.example.com",
                443,
                Some("stun.operator.example:3478"),
                Some("stun.l.google.com:19302"),
            ),
            vec!["stun.operator.example:3478".to_string()]
        );
    }

    #[tokio::test]
    async fn candidate_discovery_tries_next_stun_after_failed_probe() {
        let bad = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            for _ in 0..3 {
                if let Ok((_, from)) = bad.recv_from(&mut buf).await {
                    let _ = bad.send_to(b"not a stun response", from).await;
                }
            }
        });

        let good = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good.local_addr().unwrap();
        tokio::spawn(run_stun_responder(good));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let targets = [
            StunTarget {
                requested: "bad-stun".to_string(),
                addr: bad_addr,
                source: StunSource::PublicDefault,
            },
            StunTarget {
                requested: "good-stun".to_string(),
                addr: good_addr,
                source: StunSource::BoreFallback,
            },
        ];

        let discovery = gather_candidates_from_stun_targets(&socket, &targets, false, false).await;
        let selected = discovery
            .selected_stun
            .expect("second STUN target should be selected");
        let reflexive: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

        assert_eq!(selected.requested, "good-stun");
        assert_eq!(selected.addr, good_addr);
        assert_eq!(selected.source, StunSource::BoreFallback);
        assert_eq!(selected.reflexive, reflexive);
        assert_eq!(discovery.attempted_stun, 2);
        assert!(discovery.candidates.contains(&reflexive));
    }

    #[tokio::test]
    async fn candidate_discovery_falls_back_after_failed_peer_hint() {
        let bad = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bad_addr = bad.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            if let Ok((_, from)) = bad.recv_from(&mut buf).await {
                let _ = bad.send_to(b"not a stun response", from).await;
            }
        });

        let good = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good.local_addr().unwrap();
        tokio::spawn(run_stun_responder(good));

        let socket = bind_socket(0).await.unwrap();
        let targets = [
            StunTarget {
                requested: "provider-hinted-stun".to_string(),
                addr: bad_addr,
                source: StunSource::PeerHint,
            },
            StunTarget {
                requested: "fallback-stun".to_string(),
                addr: good_addr,
                source: StunSource::PublicDefault,
            },
        ];

        let discovery = gather_candidates_from_stun_targets(&socket, &targets, false, false).await;
        let selected = discovery
            .selected_stun
            .expect("fallback STUN target should be selected");

        assert_eq!(selected.requested, "fallback-stun");
        assert_eq!(selected.source, StunSource::PublicDefault);
        assert_eq!(discovery.attempted_stun, 2);
    }

    #[tokio::test]
    async fn port_prediction_advertises_consecutive_ports() {
        // Stand up a local STUN responder and gather with prediction on.
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let candidates = gather_candidates(&socket, stun, false, true).await;

        // The reflexive candidate (loopback source) and PREDICT_RANGE ports past it.
        let reflexive: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert!(
            candidates.contains(&reflexive),
            "missing reflexive candidate"
        );
        for delta in 1..=PREDICT_RANGE {
            if let Some(p) = port.checked_add(delta) {
                let predicted: SocketAddr = format!("127.0.0.1:{p}").parse().unwrap();
                assert!(
                    candidates.contains(&predicted),
                    "missing predicted port {p}"
                );
            }
        }
    }

    #[tokio::test]
    async fn port_prediction_off_adds_no_extra_ports() {
        let responder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun = responder.local_addr().unwrap();
        tokio::spawn(run_stun_responder(responder));

        let socket = bind_socket(0).await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let candidates = gather_candidates(&socket, stun, false, false).await;
        // No predicted port should appear when prediction is disabled.
        for delta in 1..=PREDICT_RANGE {
            if let Some(p) = port.checked_add(delta) {
                let predicted: SocketAddr = format!("127.0.0.1:{p}").parse().unwrap();
                assert!(
                    !candidates.contains(&predicted),
                    "unexpected predicted port {p}"
                );
            }
        }
    }

    fn obs(server: &str, addr: &str) -> StunObservation {
        StunObservation {
            server: server.to_string(),
            reflexive: addr.parse().unwrap(),
        }
    }

    #[test]
    fn classify_blocked_when_no_observations() {
        assert_eq!(classify_nat(&[], &[]), NatClass::Blocked);
    }

    #[test]
    fn classify_open_when_reflexive_is_a_local_ip() {
        let local: IpAddr = "203.0.113.9".parse().unwrap();
        let obs = [obs("a", "203.0.113.9:40000")];
        assert_eq!(classify_nat(&[local], &obs), NatClass::Open);
    }

    #[test]
    fn classify_inconclusive_with_single_observation() {
        let obs = [obs("a", "198.51.100.1:40000")];
        assert_eq!(classify_nat(&[], &obs), NatClass::Inconclusive);
    }

    #[test]
    fn classify_cone_when_mapping_is_stable() {
        // Endpoint-independent: same public ip:port toward every server.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:40000"),
            obs("c", "198.51.100.1:40000"),
        ];
        assert_eq!(classify_nat(&[], &obs), NatClass::Cone);
    }

    #[test]
    fn classify_symmetric_sequential() {
        // Endpoint-dependent with small regular steps -> prediction has a chance.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:40001"),
            obs("c", "198.51.100.1:40002"),
        ];
        assert_eq!(
            classify_nat(&[], &obs),
            NatClass::Symmetric { sequential: true }
        );
    }

    #[test]
    fn classify_symmetric_random() {
        // Endpoint-dependent with large/irregular gaps -> prediction won't help.
        let obs = [
            obs("a", "198.51.100.1:40000"),
            obs("b", "198.51.100.1:51234"),
            obs("c", "198.51.100.1:33001"),
        ];
        assert_eq!(
            classify_nat(&[], &obs),
            NatClass::Symmetric { sequential: false }
        );
    }

    #[tokio::test]
    async fn bind_socket_honours_fixed_port_and_ephemeral() {
        // A fixed port binds exactly that port.
        let fixed = bind_socket(0).await.unwrap();
        let want = fixed.local_addr().unwrap().port(); // grab a free port, then reuse it
        drop(fixed);
        let sock = bind_socket(want).await.unwrap();
        assert_eq!(sock.local_addr().unwrap().port(), want);
        // SO_REUSEADDR lets a fresh socket rebind the same port after the first drops.
        drop(sock);
        let again = bind_socket(want).await.unwrap();
        assert_eq!(again.local_addr().unwrap().port(), want);
        // Port 0 is ephemeral (non-zero, and almost surely different).
        let eph = bind_socket(0).await.unwrap();
        assert_ne!(eph.local_addr().unwrap().port(), 0);
    }

    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn vhost_server_handshake_rejects_wrong_token() {
        let tuning = UdpDirectTuning::default();
        let server_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server_socket.local_addr().unwrap();
        let endpoint = vhost_server_endpoint(server_socket, &tuning).unwrap();
        let expected = derive_token(Some("shared-secret"), &[9u8; UDP_NONCE_LEN]);

        let server_task = {
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let incoming = endpoint.accept().await.expect("incoming connection");
                let conn = incoming.await.expect("QUIC handshake should complete");
                vhost_server_handshake(conn, endpoint, |subdomain| {
                    (subdomain == "myapp").then_some(expected)
                })
                .await
            })
        };

        let wrong = derive_token(Some("different-secret"), &[9u8; UDP_NONCE_LEN]);
        let client = vhost_connect(
            bind_socket(0).await.unwrap(),
            server_addr,
            "myapp",
            wrong,
            tuning,
        )
        .await;
        assert!(
            client.is_err(),
            "client must fail when the server rejects the vhost direct token"
        );

        let server = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("server handshake task timed out")
            .unwrap();
        assert!(
            server.is_err(),
            "server must reject the wrong vhost direct token"
        );
    }

    #[test]
    fn cgnat_range_is_detected() {
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
        assert!(!is_cgnat("100.63.0.1".parse().unwrap()));
        assert!(!is_cgnat("100.128.0.1".parse().unwrap()));
        assert!(!is_cgnat("8.8.8.8".parse().unwrap()));
    }

    /// Two in-process QUIC endpoints exchange a datagram round-trip.
    /// Proves that: (a) `send_datagram` / `read_datagram` compile and work,
    /// (b) the datagram buffers configured in `transport_config` are accepted,
    /// (c) the received bytes match the sent bytes.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn quic_datagram_loopback_echo() {
        use tokio::sync::oneshot;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();

        // done_tx signals the server task to exit after the echo completes.
        let (done_tx, done_rx) = oneshot::channel::<()>();

        let srv_task = tokio::spawn(async move {
            let incoming = srv_ep.accept().await.expect("no incoming");
            let conn = incoming.await.expect("QUIC handshake failed");
            let dc = DirectConn {
                conn,
                endpoint: srv_ep,
            };
            let pkt = dc.read_datagram().await.expect("server recv failed");
            dc.send_datagram(pkt.clone())
                .expect("server echo send failed");
            // Keep the connection alive until the client confirms it received the echo.
            let _ = done_rx.await;
            pkt
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let cli_dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        let payload = bytes::Bytes::from("hello-vpn-datagram");
        cli_dc
            .send_datagram(payload.clone())
            .expect("client send failed");
        let echoed = cli_dc
            .read_datagram()
            .await
            .expect("client echo recv failed");
        assert_eq!(echoed, payload);

        // Signal server it can exit.
        let _ = done_tx.send(());

        let srv_recv = tokio::time::timeout(std::time::Duration::from_secs(3), srv_task)
            .await
            .expect("server task timed out")
            .unwrap();
        assert_eq!(srv_recv, payload);
    }

    /// Sending a datagram that exceeds any realistic QUIC datagram limit is
    /// reported as `DatagramSend::TooLarge` (a droppable per-packet condition),
    /// NOT as `Err` (which would kill the VPN link). Regression guard for the
    /// "send_datagram: datagram too large" link-death bug.
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn datagram_too_large_is_droppable_not_fatal() {
        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();
        // Server just needs to exist; it doesn't need to read the datagram.
        let _srv = tokio::spawn(async move {
            if let Some(inc) = srv_ep.accept().await {
                let _ = inc.await;
            }
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let cli_dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        // 65 KB is always larger than any QUIC datagram limit.
        let huge = bytes::Bytes::from(vec![0u8; 65_000]);
        let result = cli_dc.send_datagram(huge);
        assert_eq!(
            result.unwrap(),
            DatagramSend::TooLarge,
            "oversized datagram must be droppable (TooLarge), never a fatal Err"
        );

        // A datagram that fits the path limit is reported as Sent.
        let small = bytes::Bytes::from(vec![0u8; 64]);
        assert_eq!(cli_dc.send_datagram(small).unwrap(), DatagramSend::Sent);
    }

    /// Regression for the "send_datagram: datagram too large" link-death bug:
    /// a Direct `send_batch` must report oversized packets as a DROP COUNT
    /// (`Ok(dropped)`), never as `Err`. The VPN uplink pump treats `Err` as
    /// link death and tears the whole tunnel down, so an oversized packet
    /// leaking out as `Err` here is exactly the bug. Also proves a mixed batch
    /// still delivers its in-limit packets (drop only the oversized ones).
    #[cfg(feature = "vpn")]
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn direct_send_batch_drops_oversized_without_error() {
        use crate::vpn::link::make_direct;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();
        let _srv = tokio::spawn(async move {
            if let Some(inc) = srv_ep.accept().await {
                let _ = inc.await;
            }
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };
        let (mut sender, _recver) = make_direct(dc);

        // 65 KB is always larger than any QUIC datagram limit.
        let huge = bytes::Bytes::from(vec![0u8; 65_000]);
        let small = bytes::Bytes::from(vec![0u8; 64]);

        // Oversized packet → counted as 1 drop, NOT an Err.
        assert_eq!(
            sender
                .send_batch(std::slice::from_ref(&huge))
                .await
                .expect("oversized packet must never be a fatal Err"),
            1,
        );
        // In-limit packet → zero drops.
        assert_eq!(
            sender
                .send_batch(std::slice::from_ref(&small))
                .await
                .unwrap(),
            0,
        );
        // Mixed batch → only the oversized packet is dropped; the rest go out.
        let mixed = [small.clone(), huge.clone(), small.clone()];
        assert_eq!(
            sender.send_batch(&mixed).await.unwrap(),
            1,
            "mixed batch must drop only the oversized packet",
        );
    }

    /// recv_batch drains multiple queued datagrams in one call (Direct path).
    /// Proves the drain pattern: queue N datagrams on sender, one recv_batch
    /// call on receiver returns >1 packet.
    #[cfg(feature = "vpn")]
    #[cfg(feature = "udp")]
    #[tokio::test]
    async fn recv_batch_drains_queued_datagrams() {
        use crate::vpn::link::make_direct;

        let tuning = UdpDirectTuning::default();

        let srv_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let srv_addr = srv_sock.local_addr().unwrap();
        let srv_ep = server_endpoint(srv_sock, &tuning).unwrap();

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

        let srv_task = tokio::spawn(async move {
            let incoming = srv_ep.accept().await.expect("no incoming");
            let conn = incoming.await.expect("QUIC handshake failed");
            let dc = DirectConn {
                conn,
                endpoint: srv_ep,
            };

            // Server sends 5 datagrams to the client.
            for i in 0..5 {
                let pkt = bytes::Bytes::from(format!("pkt-{}", i));
                dc.send_datagram(pkt).expect("server send failed");
            }

            // Keep alive until client signals done.
            let _ = done_rx.await;
        });

        let cli_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cli_ep = client_endpoint(cli_sock, &tuning).unwrap();
        let conn = cli_ep.connect(srv_addr, "bore").unwrap().await.unwrap();
        let dc = DirectConn {
            conn,
            endpoint: cli_ep,
        };

        let (_sender, mut recver) = make_direct(dc);

        // Give the server a moment to queue all 5 datagrams.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // First recv_batch call should drain multiple queued datagrams without yielding.
        let mut batch = Vec::new();
        recver
            .recv_batch(&mut batch)
            .await
            .expect("recv_batch failed");

        // Expect >= 2 (proves the drain pattern; exact number depends on QUIC internals).
        assert!(
            batch.len() >= 2,
            "recv_batch should drain multiple queued packets, got only {}",
            batch.len()
        );

        let _ = done_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), srv_task).await;
    }
}
