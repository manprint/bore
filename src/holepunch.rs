//! UDP hole-punching with STUN reflexive-address discovery and a QUIC carrier,
//! used for the `udp` direct-path mode of secret tunnels (see [`crate::secret`]).
//!
//! The server only ever brokers candidate addresses over the (authenticated)
//! control channel — it never sees the punched data path. Two peers of the same
//! secret tunnel each open a UDP socket, learn their public (reflexive) mapping
//! via STUN, exchange candidates through the server, simultaneously send UDP
//! packets to open their NAT mappings, then establish a QUIC connection over that
//! socket. `yamux` runs over a single QUIC bidirectional stream exactly as it
//! does over TCP, so the rest of the data path is reused unchanged. If any step
//! fails the caller falls back to the server relay.
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
use tracing::{debug, warn};

use crate::shared::CONTROL_PORT;

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
    sync::Arc,
    task::{Context, Poll},
};
#[cfg(feature = "udp")]
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
#[cfg(feature = "udp")]
use tracing::{info, trace};

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

/// Max concurrent native QUIC bidi streams the provider lets a consumer open on a
/// direct connection (one stream per proxied connection). Set generous so the
/// provider's `--max-conns` semaphore is the real bound, mirroring how the relay
/// leaves yamux's stream limit generous.
#[cfg(feature = "udp")]
const MAX_DIRECT_STREAMS: u32 = 4096;

/// Per-stream QUIC receive window for the direct UDP path.
///
/// Quinn's default is deliberately conservative (roughly sized for a 100 Mbit/s,
/// 100 ms path). Bore's direct path is often used for large file transfers and
/// high-BDP links, so keep this value near the transport config rather than
/// buried in `transport_config()`. If a future agent is tuning bulk throughput,
/// start here: this is the main single-stream flow-control window advertised to
/// the peer. Raising it improves high-latency/high-bandwidth transfers at the
/// cost of more worst-case buffering per active QUIC stream.
#[cfg(feature = "udp")]
const DIRECT_QUIC_STREAM_RECEIVE_WINDOW: u32 = 16 * 1024 * 1024;

/// Total QUIC receive window for one direct connection.
///
/// This caps aggregate data buffered across all native QUIC streams on the same
/// direct peer connection. Keep it comfortably above the per-stream window so a
/// single bulk stream can fill the pipe while still leaving room for concurrent
/// proxied connections. Increase with care on small machines: this is a memory
/// budget, not just a speed knob.
#[cfg(feature = "udp")]
const DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW: u32 = 64 * 1024 * 1024;

/// Upper bound on bytes sent but not yet acknowledged on the direct QUIC path.
///
/// Match the aggregate receive window by default. If paired `test-udp` shows the
/// sender stalling below the expected bandwidth-delay product while CPU and loss
/// are low, this is the companion knob to raise after the receive windows.
#[cfg(feature = "udp")]
const DIRECT_QUIC_SEND_WINDOW: u64 = 64 * 1024 * 1024;

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
    if port == 0 {
        return UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("failed to bind UDP socket");
    }
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("failed to create UDP socket")?;
    // Reuse the address so a reconnect can rebind the fixed port without waiting
    // for the previous socket to be fully released.
    let _ = socket.set_reuse_address(true);
    socket
        .set_nonblocking(true)
        .context("failed to set UDP socket non-blocking")?;
    let addr: SocketAddr = (Ipv4Addr::UNSPECIFIED, port).into();
    socket
        .bind(&addr.into())
        .with_context(|| format!("failed to bind fixed UDP port {port} (free? allowed?)"))?;
    UdpSocket::from_std(socket.into()).context("failed to register UDP socket with tokio")
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
    let mut candidates = Vec::new();
    let local_port = socket.local_addr().map(|a| a.port()).unwrap_or(0);

    match discover_reflexive(socket, stun).await {
        Ok(addr) => {
            debug!(%addr, "discovered reflexive address");
            candidates.push(addr);

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
        }
        Err(err) => warn!(
            %err, %stun,
            "STUN reflexive discovery FAILED — no public address, offering only a local \
             candidate; the direct UDP path is unlikely (peer can't route to it). Check UDP \
             egress to the STUN server, or pass --stun-server with a public STUN (e.g. \
             stun.l.google.com:19302). Falling back to the relay if the peer can't reach us."
        ),
    }

    // Router-mapped candidate via UPnP-IGD, when explicitly enabled.
    #[cfg(feature = "udp")]
    if port_map {
        match upnp_candidate(local_port).await {
            Ok(addr) => {
                warn!(%addr, "UPnP-IGD port mapping ENABLED — added router-mapped candidate");
                if !candidates.contains(&addr) {
                    candidates.push(addr);
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
        }
    }
    candidates
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
        None => {
            let stun_port = if port == 443 || port == 80 {
                CONTROL_PORT
            } else {
                port
            };
            format!("{host}:{stun_port}")
        }
    };
    let mut addrs = tokio::net::lookup_host(&target)
        .await
        .with_context(|| format!("failed to resolve STUN server {target}"))?;
    let addr = addrs.next();
    addr.with_context(|| format!("no addresses for STUN server {target}"))
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

/// Send a STUN binding request and parse the reflexive address from the reply.
pub async fn discover_reflexive(socket: &UdpSocket, stun: SocketAddr) -> Result<SocketAddr> {
    let (request, txid) = stun::binding_request();
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        socket.send_to(&request, stun).await?;
        match timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) if from.ip() == stun.ip() => {
                if let Some(addr) = stun::parse_response(&buf[..n], &txid) {
                    return Ok(addr);
                }
            }
            Ok(Ok(_)) => continue, // stray datagram; keep waiting
            Ok(Err(err)) => return Err(err).context("STUN recv failed"),
            Err(_) => continue, // timed out; retry
        }
    }
    bail!("no STUN response from {stun}")
}

/// Public STUN servers (distinct providers) probed by [`diagnose`]. Two different
/// IPs are enough to tell endpoint-independent (cone) from endpoint-dependent
/// (symmetric) mapping; a third adds confidence and a sequential-port sample.
/// Public STUN servers (distinct providers) used by `bore test-udp` to classify
/// local NAT mapping behaviour.
pub const PUBLIC_STUN: &[&str] = &[
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.cloudflare.com:3478",
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
) -> Result<DirectConn> {
    if peers.is_empty() {
        bail!("no peer candidates to connect to");
    }
    punch(&socket, &peers).await;
    let endpoint = client_endpoint(socket)?;

    // Try all candidates concurrently under a single total budget (not a full
    // timeout *per* candidate): with N candidates the serial worst case was
    // N * NETWORK_TIMEOUT (6-21s for predicted/UPnP/local lists). `select_ok`
    // returns the first handshake that completes and verifies its token; the
    // losing connects are dropped (cancelled).
    let attempts: Vec<_> = peers
        .iter()
        .map(|&peer| {
            let endpoint = endpoint.clone();
            Box::pin(async move {
                let conn = endpoint.connect(peer, "bore")?.await?;
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
                    bail!("direct path token mismatch");
                }
                let _ = send.finish();
                info!(target_addr = %peer, peer = %conn.remote_address(),
                    "direct udp connection established (consumer, token verified)");
                anyhow::Ok(DirectConn { conn, endpoint })
            })
        })
        .collect();

    match timeout(NETWORK_TIMEOUT, futures_util::future::select_ok(attempts)).await {
        Ok(Ok((conn, _losers))) => Ok(conn),
        Ok(Err(err)) => Err(err).context("all direct candidates failed"),
        Err(_) => bail!("direct connect exhausted the {NETWORK_TIMEOUT:?} budget"),
    }
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
    pub async fn new(socket: UdpSocket, peers: Vec<SocketAddr>) -> Result<Self> {
        punch(&socket, &peers).await;
        let endpoint = server_endpoint(socket)?;
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
            bail!("direct path token mismatch");
        }
        send.write_all(&token).await?;
        send.flush().await?;
        let _ = send.finish();
        info!(%peer, "accepted direct udp connection (provider, token verified)");
        Ok(DirectConn {
            conn,
            endpoint: self.endpoint.clone(),
        })
    }
}

/// Build a QUIC client endpoint over an already-bound UDP socket.
#[cfg(feature = "udp")]
fn client_endpoint(socket: UdpSocket) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        None,
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC client endpoint")?;
    endpoint.set_default_client_config(client_config()?);
    Ok(endpoint)
}

/// Build a QUIC server endpoint over an already-bound UDP socket. It also carries
/// a default client config so it can fire outbound connections to punch its NAT
/// toward reconnecting consumers (see [`DirectListener::punch_via_endpoint`]).
#[cfg(feature = "udp")]
fn server_endpoint(socket: UdpSocket) -> Result<Endpoint> {
    let socket = into_std(socket)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config()?),
        socket,
        Arc::new(TokioRuntime),
    )
    .context("failed to create QUIC server endpoint")?;
    endpoint.set_default_client_config(client_config()?);
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
fn transport_config() -> quinn::TransportConfig {
    let mut cfg = quinn::TransportConfig::default();
    cfg.keep_alive_interval(Some(QUIC_KEEPALIVE));
    cfg.max_idle_timeout(Some(QUIC_MAX_IDLE.try_into().expect("valid idle timeout")));

    // High-throughput direct transfers need flow-control windows larger than
    // Quinn's defaults. These are deliberately constants above, not inline
    // literals, so the next person tuning real-world UDP/QUIC performance can
    // adjust the BDP/memory trade-off in one obvious place.
    cfg.stream_receive_window(DIRECT_QUIC_STREAM_RECEIVE_WINDOW.into());
    cfg.receive_window(DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW.into());
    cfg.send_window(DIRECT_QUIC_SEND_WINDOW);

    // One native QUIC stream per proxied connection: raise the concurrent-stream
    // limit well above quinn's small default so it is not the bottleneck.
    cfg.max_concurrent_bidi_streams(MAX_DIRECT_STREAMS.into());
    cfg
}

/// QUIC client config: accept any server certificate (the token handshake, not
/// the certificate, authenticates the peer).
#[cfg(feature = "udp")]
fn client_config() -> Result<ClientConfig> {
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
    config.transport_config(Arc::new(transport_config()));
    Ok(config)
}

/// QUIC server config with a self-signed certificate.
#[cfg(feature = "udp")]
fn server_config() -> Result<ServerConfig> {
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
    config.transport_config(Arc::new(transport_config()));
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

    #[test]
    fn cgnat_range_is_detected() {
        assert!(is_cgnat("100.64.0.1".parse().unwrap()));
        assert!(is_cgnat("100.127.255.255".parse().unwrap()));
        assert!(!is_cgnat("100.63.0.1".parse().unwrap()));
        assert!(!is_cgnat("100.128.0.1".parse().unwrap()));
        assert!(!is_cgnat("8.8.8.8".parse().unwrap()));
    }
}
