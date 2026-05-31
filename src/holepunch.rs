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

/// Bind a fresh UDP socket on an ephemeral port for a hole-punch session.
pub async fn bind_socket() -> Result<UdpSocket> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .context("failed to bind UDP socket")
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
        Err(err) => debug!(%err, "STUN reflexive discovery failed"),
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
fn primary_local_ip() -> Option<IpAddr> {
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

/// Reusable handle to a QUIC connection that keeps the endpoint and connection
/// alive for as long as the `yamux` carrier stream is in use.
#[cfg(feature = "udp")]
pub struct QuicTransport {
    recv: quinn::RecvStream,
    send: quinn::SendStream,
    _conn: Connection,
    _endpoint: Endpoint,
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
/// authenticate with `token`. Returns a carrier usable as a [`crate::mux`]
/// transport. The consumer opens the bidirectional stream.
#[cfg(feature = "udp")]
pub async fn connect_direct(
    socket: UdpSocket,
    peers: Vec<SocketAddr>,
    token: [u8; TOKEN_LEN],
) -> Result<QuicTransport> {
    punch(&socket, &peers).await;
    let endpoint = client_endpoint(socket)?;

    // Try each candidate (reflexive first) until one completes the handshake.
    let mut last_err = None;
    for peer in &peers {
        match timeout(NETWORK_TIMEOUT, async {
            let conn = endpoint.connect(*peer, "bore")?.await?;
            anyhow::Ok(conn)
        })
        .await
        {
            Ok(Ok(conn)) => {
                trace!(%peer, "QUIC connected");
                let (mut send, mut recv) = conn.open_bi().await.context("open_bi failed")?;
                // Consumer writes its token first, then reads the peer's.
                send.write_all(&token).await?;
                send.flush().await?;
                let mut peer_token = [0u8; TOKEN_LEN];
                recv.read_exact(&mut peer_token).await?;
                if !tokens_match(&token, &peer_token) {
                    bail!("direct path token mismatch");
                }
                info!(target_addr = %peer, peer = %conn.remote_address(),
                    "direct udp carrier established (consumer, token verified)");
                return Ok(QuicTransport {
                    recv,
                    send,
                    _conn: conn,
                    _endpoint: endpoint,
                });
            }
            Ok(Err(err)) => last_err = Some(err),
            Err(_) => last_err = Some(anyhow::anyhow!("connect to {peer} timed out")),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no peer candidates to connect to")))
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

    /// Accept the next direct connection and authenticate it with `token`. The
    /// provider reads the peer's token first, then sends its own. Returns a
    /// carrier usable as a [`crate::mux`] transport.
    pub async fn accept(&self, token: [u8; TOKEN_LEN]) -> Result<QuicTransport> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .context("QUIC endpoint closed")?;
        let conn = incoming.await.context("QUIC handshake failed")?;
        let peer = conn.remote_address();
        trace!(%peer, "QUIC accepted");
        let (mut send, mut recv) = conn.accept_bi().await.context("accept_bi failed")?;
        let mut peer_token = [0u8; TOKEN_LEN];
        recv.read_exact(&mut peer_token).await?;
        if !tokens_match(&token, &peer_token) {
            bail!("direct path token mismatch");
        }
        send.write_all(&token).await?;
        send.flush().await?;
        info!(%peer, "accepted direct udp connection (provider, token verified)");
        Ok(QuicTransport {
            recv,
            send,
            _conn: conn,
            _endpoint: self.endpoint.clone(),
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
        let mut txid = [0u8; 12];
        for b in txid.iter_mut() {
            *b = fastrand::u8(..);
        }
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

        let socket = bind_socket().await.unwrap();
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

        let socket = bind_socket().await.unwrap();
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
}
