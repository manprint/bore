//! Coordinated two-peer diagnostics for `bore test-udp --tcp-secret-id`.
//!
//! The ordinary `holepunch::diagnose` command inspects one host. This module adds
//! a paired mode: two peers register with the server under the same id, exchange
//! UDP candidates, try the real direct QUIC path, and then run the same latency
//! and throughput probes over the TCP relay fallback.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::{info, trace, warn};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::holepunch::{self, NatClass, StunObservation};
use crate::mux;
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, UdpTestOptions, UdpTestPeerSummary, UdpTestRole,
    PROXY_BUFFER_SIZE, UDP_NONCE_LEN,
};
use crate::transport::{self, Endpoint};

const LATENCY_SAMPLES: u16 = 8;
const OP_LATENCY: u8 = 1;
const OP_BANDWIDTH: u8 = 2;
const OP_METRICS: u8 = 3;
const METRICS_MAX_JSON: usize = 16 * 1024;
const DEFAULT_CHUNK: usize = 64 * 1024;

/// Pending `test-udp` sessions, keyed by `tcp-secret-id`.
pub type Registry = Arc<DashMap<String, PendingPeer>>;

/// Server-side state for the first diagnostic peer waiting for its counterpart.
pub struct PendingPeer {
    token: Uuid,
    peer: SocketAddr,
    opener: mux::Opener,
    candidates: Vec<SocketAddr>,
    summary: UdpTestPeerSummary,
    options: UdpTestOptions,
    start_tx: oneshot::Sender<PeerStart>,
}

struct PeerStart {
    role: UdpTestRole,
    nonce: [u8; UDP_NONCE_LEN],
    peer_opener: mux::Opener,
    peer_candidates: Vec<SocketAddr>,
    peer_summary: UdpTestPeerSummary,
    options: UdpTestOptions,
}

struct PendingGuard {
    registry: Registry,
    id: String,
    token: Uuid,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let token = self.token;
        let _ = self
            .registry
            .remove_if(&self.id, |_, pending| pending.token == token);
    }
}

/// Serve one server-side `test-udp` diagnostic peer.
#[allow(clippy::too_many_arguments)]
pub async fn serve_peer(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    acceptor: mux::Acceptor,
    registry: Registry,
    id: String,
    peer: SocketAddr,
    candidates: Vec<SocketAddr>,
    summary: UdpTestPeerSummary,
    options: UdpTestOptions,
) -> Result<()> {
    if let Some((_key, pending)) = registry.remove(&id) {
        let nonce = new_nonce();
        let effective = merge_options(pending.options, options);
        let first = PeerStart {
            role: UdpTestRole::Listener,
            nonce,
            peer_opener: opener.clone(),
            peer_candidates: candidates.clone(),
            peer_summary: summary.clone(),
            options: effective,
        };
        if pending.start_tx.send(first).is_ok() {
            info!(%id, first = %pending.peer, second = %peer, "paired udp diagnostic peers");
            control
                .send(ServerMessage::TestUdpStart {
                    role: UdpTestRole::Dialer,
                    nonce,
                    peer_candidates: pending.candidates,
                    peer_summary: pending.summary,
                    options: effective,
                })
                .await?;
            return relay_loop(control, acceptor, pending.opener).await;
        }
        warn!(%id, "waiting udp diagnostic peer disappeared before pairing");
    }

    wait_for_peer(
        control, opener, acceptor, registry, id, peer, candidates, summary, options,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn wait_for_peer(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    acceptor: mux::Acceptor,
    registry: Registry,
    id: String,
    peer: SocketAddr,
    candidates: Vec<SocketAddr>,
    summary: UdpTestPeerSummary,
    options: UdpTestOptions,
) -> Result<()> {
    let (start_tx, start_rx) = oneshot::channel();
    let token = Uuid::new_v4();
    registry.insert(
        id.clone(),
        PendingPeer {
            token,
            peer,
            opener,
            candidates,
            summary,
            options,
            start_tx,
        },
    );
    let guard = PendingGuard {
        registry,
        id: id.clone(),
        token,
    };
    control.send(ServerMessage::TestUdpWaiting).await?;
    info!(%id, %peer, "udp diagnostic peer waiting");

    let start = tokio::select! {
        start = start_rx => start.context("diagnostic pairing cancelled")?,
        message = control.recv::<ClientMessage>() => {
            match message? {
                Some(_) => bail!("unexpected diagnostic control message while waiting"),
                None => return Ok(()),
            }
        }
    };
    drop(guard);

    control
        .send(ServerMessage::TestUdpStart {
            role: start.role,
            nonce: start.nonce,
            peer_candidates: start.peer_candidates,
            peer_summary: start.peer_summary,
            options: start.options,
        })
        .await?;
    relay_loop(control, acceptor, start.peer_opener).await
}

async fn relay_loop(
    mut control: Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    peer_opener: mux::Opener,
) -> Result<()> {
    loop {
        tokio::select! {
            message = control.recv::<ClientMessage>() => {
                match message? {
                    Some(_) => warn!("unexpected diagnostic control message after pairing"),
                    None => return Ok(()),
                }
            }
            inbound = acceptor.accept() => {
                let Some(mut source) = inbound else {
                    return Ok(());
                };
                let peer_opener = peer_opener.clone();
                tokio::spawn(async move {
                    let mut target = match peer_opener.open().await {
                        Ok(target) => target,
                        Err(err) => {
                            trace!(%err, "failed to open diagnostic peer stream");
                            return;
                        }
                    };
                    if target.write_all(&[mux::STREAM_READY]).await.is_err() {
                        return;
                    }
                    let _ = tokio::io::copy_bidirectional_with_sizes(
                        &mut source,
                        &mut target,
                        PROXY_BUFFER_SIZE,
                        PROXY_BUFFER_SIZE,
                    )
                    .await;
                });
            }
        }
    }
}

/// Run the paired `bore test-udp` diagnostic client.
#[allow(clippy::too_many_arguments)]
pub async fn run_peer_test(
    to: &str,
    tcp_secret_id: &str,
    secret: Option<&str>,
    insecure: bool,
    stun_server: Option<&str>,
    port_map: bool,
    port_prediction: bool,
    preferred_port: u16,
    options: UdpTestOptions,
) -> Result<()> {
    println!("bore paired UDP / NAT diagnostic");
    println!("================================");
    println!("Session id        : {tcp_secret_id}");

    let endpoint = Endpoint::parse(to);
    let socket = holepunch::bind_socket(preferred_port).await?;
    let mut local =
        inspect_local_nat(&socket, Some((&endpoint.host, endpoint.port)), stun_server).await;
    let stun_chain = holepunch::live_stun_target_names(&endpoint.host, endpoint.port, stun_server);
    println!();
    println!("Live tunnel STUN chain : {}", stun_chain.join(", "));
    let stun_targets = match holepunch::resolve_live_stun_targets(
        &endpoint.host,
        endpoint.port,
        stun_server,
    )
    .await
    {
        Ok(targets) => targets,
        Err(err) => {
            println!("Live tunnel STUN resolve: FAILED ({err}); using non-STUN candidates only");
            Vec::new()
        }
    };
    let discovery = holepunch::gather_candidates_from_stun_targets(
        &socket,
        &stun_targets,
        port_map,
        port_prediction,
    )
    .await;
    match &discovery.selected_stun {
        Some(stun) => println!(
            "Live tunnel STUN used  : {} ({}, {}) -> {}",
            stun.requested,
            stun.addr,
            stun.source.as_str(),
            stun.reflexive
        ),
        None => println!("Live tunnel STUN used  : <none>"),
    }
    let candidates = discovery.candidates;
    local.summary.candidate_count = candidates.len();
    print_local_nat_report(&local, &candidates);

    let socket_tcp = transport::connect(&endpoint, insecure).await?;
    let (opener, acceptor) = mux::client(socket_tcp);
    let mut control = Delimited::new(
        opener
            .open()
            .await
            .context("failed to open diagnostic control stream")?,
    );
    control
        .send(ClientMessage::TestUdpJoin {
            id: tcp_secret_id.to_string(),
            candidates: candidates.clone(),
            summary: local.summary.clone(),
            options,
        })
        .await?;
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control)
            .await?;
    }

    let start = wait_for_start(&mut control).await?;
    print_pairing_report(
        &start.peer_summary,
        &start.peer_candidates,
        start.role,
        start.options,
    );

    let token = holepunch::derive_token(secret, &start.nonce);
    let mut tcp_path = TestPath::Tcp { opener, acceptor };

    let udp = run_udp_path(
        socket,
        start.role,
        start.peer_candidates.clone(),
        token,
        start.options,
    )
    .await;

    let tcp = match run_path_suite(
        "TCP relay fallback",
        &mut tcp_path,
        start.role,
        start.options,
    )
    .await
    {
        Ok(metrics) => Some(metrics),
        Err(err) => {
            println!();
            println!("TCP relay fallback : FAILED ({err})");
            None
        }
    };

    let local_metrics = PeerMetrics { udp, tcp };
    let peer_metrics = match timeout(
        Duration::from_secs(10),
        exchange_metrics(&mut tcp_path, start.role, &local_metrics),
    )
    .await
    {
        Ok(Ok(metrics)) => Some(metrics),
        Ok(Err(err)) => {
            println!();
            println!("Peer metrics       : unavailable ({err})");
            None
        }
        Err(_) => {
            println!();
            println!("Peer metrics       : unavailable (timed out)");
            None
        }
    };
    print_final_report(
        &local.summary,
        &start.peer_summary,
        &local_metrics,
        peer_metrics.as_ref(),
    );
    Ok(())
}

async fn wait_for_start(control: &mut Delimited<mux::Stream>) -> Result<StartInfo> {
    loop {
        match control.recv().await? {
            Some(ServerMessage::TestUdpWaiting) => {
                println!();
                println!("Server pairing     : waiting for the peer with the same id...");
            }
            Some(ServerMessage::TestUdpStart {
                role,
                nonce,
                peer_candidates,
                peer_summary,
                options,
            }) => {
                return Ok(StartInfo {
                    role,
                    nonce,
                    peer_candidates,
                    peer_summary,
                    options,
                });
            }
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no --secret was provided")
            }
            Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => continue,
            Some(other) => bail!("unexpected diagnostic response: {other:?}"),
            None => bail!("server closed the diagnostic control channel"),
        }
    }
}

struct StartInfo {
    role: UdpTestRole,
    nonce: [u8; UDP_NONCE_LEN],
    peer_candidates: Vec<SocketAddr>,
    peer_summary: UdpTestPeerSummary,
    options: UdpTestOptions,
}

#[cfg(feature = "udp")]
async fn run_udp_path(
    socket: tokio::net::UdpSocket,
    role: UdpTestRole,
    peer_candidates: Vec<SocketAddr>,
    token: [u8; holepunch::TOKEN_LEN],
    options: UdpTestOptions,
) -> Option<PathMetrics> {
    println!();
    println!("UDP direct path    : trying QUIC hole punching");
    let conn = match establish_direct(socket, role, peer_candidates, token).await {
        Ok(conn) => conn,
        Err(err) => {
            println!("UDP direct path    : FAILED ({err})");
            println!("                    TCP relay fallback will still be tested.");
            return None;
        }
    };
    let mut path = TestPath::Direct(conn);
    match run_path_suite("UDP direct path", &mut path, role, options).await {
        Ok(metrics) => Some(metrics),
        Err(err) => {
            println!("UDP direct tests   : FAILED ({err})");
            None
        }
    }
}

#[cfg(not(feature = "udp"))]
async fn run_udp_path(
    _socket: tokio::net::UdpSocket,
    _role: UdpTestRole,
    _peer_candidates: Vec<SocketAddr>,
    _token: [u8; holepunch::TOKEN_LEN],
    _options: UdpTestOptions,
) -> Option<PathMetrics> {
    println!();
    println!("UDP direct path    : skipped (binary built without the `udp` feature)");
    None
}

#[cfg(feature = "udp")]
async fn establish_direct(
    socket: tokio::net::UdpSocket,
    role: UdpTestRole,
    peer_candidates: Vec<SocketAddr>,
    token: [u8; holepunch::TOKEN_LEN],
) -> Result<holepunch::DirectConn> {
    match role {
        UdpTestRole::Listener => {
            let listener = holepunch::DirectListener::new(socket, peer_candidates)
                .await
                .context("start diagnostic QUIC listener")?;
            Ok(timeout(Duration::from_secs(10), listener.accept(token))
                .await
                .context("timed out waiting for direct QUIC peer")??)
        }
        UdpTestRole::Dialer => holepunch::connect_direct(socket, peer_candidates, token).await,
    }
}

async fn run_path_suite(
    label: &str,
    path: &mut TestPath,
    role: UdpTestRole,
    options: UdpTestOptions,
) -> Result<PathMetrics> {
    println!();
    println!("{label} : running bidirectional latency");
    let mut metrics = PathMetrics::default();
    match role {
        UdpTestRole::Dialer => {
            metrics.latency_send = Some(
                run_latency_sender(path)
                    .await
                    .context("send latency probe")?,
            );
            run_latency_receiver(path)
                .await
                .context("receive latency probe")?;
        }
        UdpTestRole::Listener => {
            run_latency_receiver(path)
                .await
                .context("receive latency probe")?;
            metrics.latency_send = Some(
                run_latency_sender(path)
                    .await
                    .context("send latency probe")?,
            );
        }
    }

    if options.bandwidth {
        println!(
            "{label} : running bidirectional bandwidth ({})",
            format_bytes(options.transfer_quota)
        );
        match role {
            UdpTestRole::Dialer => {
                metrics.bandwidth_send = Some(
                    run_bandwidth_sender(path, options.transfer_quota)
                        .await
                        .context("send bandwidth probe")?,
                );
                metrics.bandwidth_recv = Some(
                    run_bandwidth_receiver(path)
                        .await
                        .context("receive bandwidth probe")?,
                );
            }
            UdpTestRole::Listener => {
                metrics.bandwidth_recv = Some(
                    run_bandwidth_receiver(path)
                        .await
                        .context("receive bandwidth probe")?,
                );
                metrics.bandwidth_send = Some(
                    run_bandwidth_sender(path, options.transfer_quota)
                        .await
                        .context("send bandwidth probe")?,
                );
            }
        }
    }

    print_path_metrics(label, &metrics);
    Ok(metrics)
}

async fn run_latency_sender(path: &mut TestPath) -> Result<LatencyMetrics> {
    let mut stream = path.open_stream().await?;
    stream.write_all(&[OP_LATENCY]).await?;
    stream.write_all(&LATENCY_SAMPLES.to_be_bytes()).await?;
    stream.flush().await?;
    let mut samples = Vec::with_capacity(LATENCY_SAMPLES as usize);
    for seq in 0..LATENCY_SAMPLES {
        let payload = u64::from(seq).to_be_bytes();
        let started = Instant::now();
        stream.write_all(&payload).await?;
        stream.flush().await?;
        let mut echoed = [0u8; 8];
        stream.read_exact(&mut echoed).await?;
        if echoed != payload {
            bail!("latency echo mismatch");
        }
        samples.push(started.elapsed());
    }
    let _ = stream.shutdown().await;
    Ok(LatencyMetrics::from_samples(&samples))
}

async fn run_latency_receiver(path: &mut TestPath) -> Result<()> {
    let mut stream = path.accept_stream().await?;
    let op = read_u8(&mut stream).await?;
    if op != OP_LATENCY {
        bail!("expected latency stream, got op {op}");
    }
    let count = read_u16(&mut stream).await?;
    for _ in 0..count {
        let mut payload = [0u8; 8];
        stream.read_exact(&mut payload).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;
    }
    let _ = stream.shutdown().await;
    Ok(())
}

async fn run_bandwidth_sender(path: &mut TestPath, quota: u64) -> Result<BandwidthMetrics> {
    let mut stream = path.open_stream().await?;
    stream.write_all(&[OP_BANDWIDTH]).await?;
    stream.write_all(&quota.to_be_bytes()).await?;
    stream.flush().await?;
    let buf = pattern_buffer();
    let mut remaining = quota;
    let started = Instant::now();
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        stream.write_all(&buf[..n]).await?;
        remaining -= n as u64;
    }
    stream.flush().await?;
    let elapsed = started.elapsed();
    let ack_observed = match timeout(Duration::from_secs(10), read_u8(&mut stream)).await {
        Ok(Ok(1)) => true,
        Ok(Ok(_)) => false,
        Ok(Err(err)) => {
            trace!(%err, "bandwidth ack not observed before stream closed");
            false
        }
        Err(_) => false,
    };
    let _ = stream.shutdown().await;
    Ok(BandwidthMetrics::new(quota, elapsed, ack_observed))
}

async fn run_bandwidth_receiver(path: &mut TestPath) -> Result<BandwidthMetrics> {
    let mut stream = path.accept_stream().await?;
    let op = read_u8(&mut stream).await?;
    if op != OP_BANDWIDTH {
        bail!("expected bandwidth stream, got op {op}");
    }
    let quota = read_u64(&mut stream).await?;
    let mut buf = vec![0u8; DEFAULT_CHUNK];
    let mut remaining = quota;
    let started = Instant::now();
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        stream.read_exact(&mut buf[..n]).await?;
        remaining -= n as u64;
    }
    let elapsed = started.elapsed();
    stream.write_all(&[1]).await?;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
    Ok(BandwidthMetrics::new(quota, elapsed, true))
}

async fn exchange_metrics(
    path: &mut TestPath,
    role: UdpTestRole,
    local: &PeerMetrics,
) -> Result<PeerMetrics> {
    match role {
        UdpTestRole::Dialer => {
            send_metrics(path, local).await?;
            recv_metrics(path).await
        }
        UdpTestRole::Listener => {
            let peer = recv_metrics(path).await?;
            send_metrics(path, local).await?;
            Ok(peer)
        }
    }
}

async fn send_metrics(path: &mut TestPath, local: &PeerMetrics) -> Result<()> {
    let mut stream = path.open_stream().await?;
    let json = serde_json::to_vec(local)?;
    if json.len() > METRICS_MAX_JSON {
        bail!("metrics payload too large");
    }
    stream.write_all(&[OP_METRICS]).await?;
    stream.write_all(&(json.len() as u32).to_be_bytes()).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

async fn recv_metrics(path: &mut TestPath) -> Result<PeerMetrics> {
    let mut stream = path.accept_stream().await?;
    let op = read_u8(&mut stream).await?;
    if op != OP_METRICS {
        bail!("expected metrics stream, got op {op}");
    }
    let len = read_u32(&mut stream).await? as usize;
    if len > METRICS_MAX_JSON {
        bail!("metrics payload too large");
    }
    let mut json = vec![0u8; len];
    stream.read_exact(&mut json).await?;
    let metrics = serde_json::from_slice(&json)?;
    let _ = stream.shutdown().await;
    Ok(metrics)
}

enum TestPath {
    Tcp {
        opener: mux::Opener,
        acceptor: mux::Acceptor,
    },
    #[cfg(feature = "udp")]
    Direct(holepunch::DirectConn),
}

impl TestPath {
    async fn open_stream(&mut self) -> Result<TestStream> {
        match self {
            TestPath::Tcp { opener, .. } => Ok(TestStream::Mux(
                opener
                    .open()
                    .await
                    .context("open diagnostic TCP relay stream")?,
            )),
            #[cfg(feature = "udp")]
            TestPath::Direct(conn) => Ok(TestStream::Quic(conn.open_stream().await?)),
        }
    }

    async fn accept_stream(&mut self) -> Result<TestStream> {
        match self {
            TestPath::Tcp { acceptor, .. } => {
                let mut stream = acceptor
                    .accept()
                    .await
                    .context("diagnostic TCP relay closed")?;
                let mut marker = [0u8; 1];
                stream.read_exact(&mut marker).await?;
                if marker[0] != mux::STREAM_READY {
                    bail!("invalid diagnostic relay marker");
                }
                Ok(TestStream::Mux(stream))
            }
            #[cfg(feature = "udp")]
            TestPath::Direct(conn) => Ok(TestStream::Quic(conn.accept_stream().await?)),
        }
    }
}

enum TestStream {
    Mux(mux::Stream),
    #[cfg(feature = "udp")]
    Quic(holepunch::QuicTransport),
}

impl AsyncRead for TestStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TestStream::Mux(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(feature = "udp")]
            TestStream::Quic(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TestStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            TestStream::Mux(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(feature = "udp")]
            TestStream::Quic(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TestStream::Mux(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(feature = "udp")]
            TestStream::Quic(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            TestStream::Mux(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(feature = "udp")]
            TestStream::Quic(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[derive(Debug, Clone)]
struct LocalNatReport {
    summary: UdpTestPeerSummary,
    probes: Vec<ProbeLine>,
    class: NatClass,
}

#[derive(Debug, Clone)]
struct ProbeLine {
    server: String,
    ok: bool,
    reflexive: Option<SocketAddr>,
    error: Option<String>,
}

async fn inspect_local_nat(
    socket: &tokio::net::UdpSocket,
    bore_target: Option<(&str, u16)>,
    stun_override: Option<&str>,
) -> LocalNatReport {
    let local_udp = socket
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "0.0.0.0:0".to_string());
    let local_port = socket.local_addr().map(|addr| addr.port()).unwrap_or(0);
    let primary = holepunch::primary_local_ip();
    let mut probes = Vec::new();
    let mut observations = Vec::new();

    for server in holepunch::PUBLIC_STUN {
        let line = probe_stun(socket, server).await;
        if let Some(reflexive) = line.reflexive {
            observations.push(StunObservation {
                server: (*server).to_string(),
                reflexive,
            });
        }
        probes.push(line);
    }
    if let Some(server) = stun_override {
        probes.push(probe_stun(socket, server).await);
    }

    let mut bore_stun = None;
    if let Some((host, port)) = bore_target {
        match holepunch::resolve_stun(host, port, None).await {
            Ok(addr) => match holepunch::discover_reflexive(socket, addr).await {
                Ok(refl) => {
                    bore_stun = Some(true);
                    probes.push(ProbeLine {
                        server: format!("bore server {addr}"),
                        ok: true,
                        reflexive: Some(refl),
                        error: None,
                    });
                }
                Err(err) => {
                    bore_stun = Some(false);
                    probes.push(ProbeLine {
                        server: format!("bore server {addr}"),
                        ok: false,
                        reflexive: None,
                        error: Some(err.to_string()),
                    });
                }
            },
            Err(err) => {
                bore_stun = Some(false);
                probes.push(ProbeLine {
                    server: "bore server resolve".to_string(),
                    ok: false,
                    reflexive: None,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    let local_ips: Vec<IpAddr> = primary.into_iter().collect();
    let class = holepunch::classify_nat(&local_ips, &observations);
    let reflexive: Vec<String> = observations
        .iter()
        .map(|obs| obs.reflexive.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let port_preserved = observations
        .first()
        .map(|obs| obs.reflexive.port() == local_port);
    let summary = UdpTestPeerSummary {
        nat_class: nat_class_label(&class).to_string(),
        local_udp,
        primary_local_ip: local_ips.first().map(ToString::to_string),
        reflexive,
        bore_stun,
        candidate_count: 0,
        port_preserved,
    };

    LocalNatReport {
        summary,
        probes,
        class,
    }
}

async fn probe_stun(socket: &tokio::net::UdpSocket, server: &str) -> ProbeLine {
    let result = async {
        let addr = tokio::net::lookup_host(server)
            .await
            .with_context(|| format!("resolve {server}"))?
            .find(SocketAddr::is_ipv4)
            .with_context(|| format!("no IPv4 addresses for {server}"))?;
        holepunch::discover_reflexive(socket, addr).await
    }
    .await;
    match result {
        Ok(reflexive) => ProbeLine {
            server: server.to_string(),
            ok: true,
            reflexive: Some(reflexive),
            error: None,
        },
        Err(err) => ProbeLine {
            server: server.to_string(),
            ok: false,
            reflexive: None,
            error: Some(err.to_string()),
        },
    }
}

fn new_nonce() -> [u8; UDP_NONCE_LEN] {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut nonce = [0u8; UDP_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .expect("system CSPRNG must not fail");
    nonce
}

fn merge_options(a: UdpTestOptions, b: UdpTestOptions) -> UdpTestOptions {
    let transfer_quota = if a.transfer_quota == 0 {
        b.transfer_quota
    } else if b.transfer_quota == 0 {
        a.transfer_quota
    } else {
        a.transfer_quota.min(b.transfer_quota)
    };
    UdpTestOptions {
        bandwidth: a.bandwidth || b.bandwidth,
        transfer_quota,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PeerMetrics {
    udp: Option<PathMetrics>,
    tcp: Option<PathMetrics>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PathMetrics {
    latency_send: Option<LatencyMetrics>,
    bandwidth_send: Option<BandwidthMetrics>,
    bandwidth_recv: Option<BandwidthMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LatencyMetrics {
    samples: u16,
    min_ms: f64,
    avg_ms: f64,
    max_ms: f64,
}

impl LatencyMetrics {
    fn from_samples(samples: &[Duration]) -> Self {
        let mut min = f64::MAX;
        let mut max: f64 = 0.0;
        let mut sum = 0.0;
        for sample in samples {
            let ms = sample.as_secs_f64() * 1000.0;
            min = min.min(ms);
            max = max.max(ms);
            sum += ms;
        }
        Self {
            samples: samples.len() as u16,
            min_ms: min,
            avg_ms: sum / samples.len().max(1) as f64,
            max_ms: max,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BandwidthMetrics {
    bytes: u64,
    seconds: f64,
    mbps: f64,
    ack_observed: bool,
}

impl BandwidthMetrics {
    fn new(bytes: u64, elapsed: Duration, ack_observed: bool) -> Self {
        let seconds = elapsed.as_secs_f64().max(0.000_001);
        let mbps = (bytes as f64 * 8.0) / seconds / 1_000_000.0;
        Self {
            bytes,
            seconds,
            mbps,
            ack_observed,
        }
    }
}

async fn read_u8(stream: &mut TestStream) -> Result<u8> {
    let mut buf = [0u8; 1];
    stream.read_exact(&mut buf).await?;
    Ok(buf[0])
}

async fn read_u16(stream: &mut TestStream) -> Result<u16> {
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    Ok(u16::from_be_bytes(buf))
}

async fn read_u32(stream: &mut TestStream) -> Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;
    Ok(u32::from_be_bytes(buf))
}

async fn read_u64(stream: &mut TestStream) -> Result<u64> {
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf).await?;
    Ok(u64::from_be_bytes(buf))
}

fn pattern_buffer() -> Vec<u8> {
    (0..DEFAULT_CHUNK).map(|i| (i % 251) as u8).collect()
}

fn nat_class_label(class: &NatClass) -> &'static str {
    match class {
        NatClass::Blocked => "blocked",
        NatClass::Open => "open/public",
        NatClass::Inconclusive => "inconclusive",
        NatClass::Cone => "cone",
        NatClass::Symmetric { sequential: true } => "symmetric-sequential",
        NatClass::Symmetric { sequential: false } => "symmetric-random",
    }
}

fn print_local_nat_report(report: &LocalNatReport, candidates: &[SocketAddr]) {
    println!();
    println!("Local NAT report");
    println!("----------------");
    println!("UDP socket        : {}", report.summary.local_udp);
    println!(
        "Primary local IP  : {}",
        report
            .summary
            .primary_local_ip
            .as_deref()
            .unwrap_or("<none>")
    );
    println!("NAT class         : {}", nat_class_label(&report.class));
    for probe in &report.probes {
        if probe.ok {
            println!(
                "  [ ok ] {:<28} -> {}",
                probe.server,
                probe.reflexive.expect("ok probe has reflexive address")
            );
        } else {
            println!(
                "  [FAIL] {:<28} -> {}",
                probe.server,
                probe.error.as_deref().unwrap_or("unknown error")
            );
        }
    }
    println!("Candidates        : {}", candidates.len());
    for candidate in candidates {
        println!("  - {candidate}");
    }
}

fn print_pairing_report(
    peer: &UdpTestPeerSummary,
    peer_candidates: &[SocketAddr],
    role: UdpTestRole,
    options: UdpTestOptions,
) {
    println!();
    println!("Server pairing     : paired");
    println!("Local role         : {role:?}");
    println!("Peer NAT class     : {}", peer.nat_class);
    println!("Peer UDP socket    : {}", peer.local_udp);
    println!("Peer candidates    : {}", peer_candidates.len());
    println!(
        "Bandwidth test     : {}",
        if options.bandwidth {
            format!(
                "enabled, {} per direction/path",
                format_bytes(options.transfer_quota)
            )
        } else {
            "disabled".to_string()
        }
    );
}

fn print_path_metrics(label: &str, metrics: &PathMetrics) {
    if let Some(latency) = &metrics.latency_send {
        println!(
            "{label} : latency sent avg {:.2} ms (min {:.2}, max {:.2}, n={})",
            latency.avg_ms, latency.min_ms, latency.max_ms, latency.samples
        );
    }
    if let Some(bw) = &metrics.bandwidth_send {
        println!(
            "{label} : sent {} in {:.2}s ({:.2} Mbit/s{})",
            format_bytes(bw.bytes),
            bw.seconds,
            bw.mbps,
            if bw.ack_observed {
                ""
            } else {
                ", final ACK not observed"
            }
        );
    }
    if let Some(bw) = &metrics.bandwidth_recv {
        println!(
            "{label} : received {} in {:.2}s ({:.2} Mbit/s)",
            format_bytes(bw.bytes),
            bw.seconds,
            bw.mbps
        );
    }
}

fn print_final_report(
    local: &UdpTestPeerSummary,
    peer: &UdpTestPeerSummary,
    metrics: &PeerMetrics,
    peer_metrics: Option<&PeerMetrics>,
) {
    println!();
    println!("Final report");
    println!("============");
    println!(
        "Local NAT         : {} ({})",
        local.nat_class, local.local_udp
    );
    println!(
        "Peer NAT          : {} ({})",
        peer.nat_class, peer.local_udp
    );
    println!(
        "UDP direct        : {}",
        if metrics.udp.is_some() {
            "working"
        } else {
            "not available"
        }
    );
    println!(
        "TCP fallback      : {}",
        if metrics.tcp.is_some() {
            "working"
        } else {
            "FAILED"
        }
    );
    if let Some(peer_metrics) = peer_metrics {
        println!(
            "Peer UDP direct   : {}",
            if peer_metrics.udp.is_some() {
                "working"
            } else {
                "not available"
            }
        );
        println!(
            "Peer TCP fallback : {}",
            if peer_metrics.tcp.is_some() {
                "working"
            } else {
                "FAILED"
            }
        );
    }
    if metrics.udp.is_none() && metrics.tcp.is_some() {
        println!("Recommendation    : use the TCP relay fallback; direct UDP failed on this pair.");
    } else if metrics.udp.is_some() {
        println!(
            "Recommendation    : direct UDP is usable; TCP relay remains available as fallback."
        );
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, f64)] = &[
        ("GiB", 1024.0 * 1024.0 * 1024.0),
        ("MiB", 1024.0 * 1024.0),
        ("KiB", 1024.0),
    ];
    for (unit, scale) in UNITS {
        if bytes as f64 >= *scale {
            return format!("{:.2} {unit}", bytes as f64 / scale);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_options_enables_bandwidth_and_uses_lower_quota() {
        let a = UdpTestOptions {
            bandwidth: false,
            transfer_quota: 500,
        };
        let b = UdpTestOptions {
            bandwidth: true,
            transfer_quota: 200,
        };
        let merged = merge_options(a, b);
        assert!(merged.bandwidth);
        assert_eq!(merged.transfer_quota, 200);
    }

    #[test]
    fn latency_metrics_are_aggregated() {
        let metrics = LatencyMetrics::from_samples(&[
            Duration::from_millis(3),
            Duration::from_millis(1),
            Duration::from_millis(2),
        ]);
        assert_eq!(metrics.samples, 3);
        assert_eq!(metrics.min_ms, 1.0);
        assert_eq!(metrics.avg_ms, 2.0);
        assert_eq!(metrics.max_ms, 3.0);
    }
}
