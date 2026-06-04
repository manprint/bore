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

#[cfg(target_os = "linux")]
use procfs::process::Process;
#[cfg(target_os = "linux")]
use procfs::{
    page_size, ticks_per_second, CpuInfo, CpuTime, Current, CurrentSI, KernelStats, LoadAverage,
    Meminfo,
};

use crate::auth::Authenticator;
use crate::holepunch::{self, NatClass, StunObservation};
use crate::mux;
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, UdpDirectTuning, UdpTestOptions, UdpTestPeerSummary,
    UdpTestRole, PROXY_BUFFER_SIZE, UDP_NONCE_LEN,
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
    tuning: UdpDirectTuning,
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
    tuning: UdpDirectTuning,
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
            tuning,
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
                    tuning,
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
            tuning: start.tuning,
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
    let host_start = capture_host_sample();
    let run_started = Instant::now();

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
        start.tuning,
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

    let host_end = capture_host_sample();
    let host_metrics = build_host_metrics(
        host_start.as_ref(),
        host_end.as_ref(),
        run_started.elapsed(),
    );
    let local_metrics = PeerMetrics {
        host: host_metrics,
        udp,
        tcp,
    };
    let peer_metrics = match timeout(
        Duration::from_secs(30),
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
                tuning,
            }) => {
                return Ok(StartInfo {
                    role,
                    nonce,
                    peer_candidates,
                    peer_summary,
                    options,
                    tuning,
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
    tuning: UdpDirectTuning,
}

#[cfg(feature = "udp")]
async fn run_udp_path(
    socket: tokio::net::UdpSocket,
    role: UdpTestRole,
    peer_candidates: Vec<SocketAddr>,
    token: [u8; holepunch::TOKEN_LEN],
    tuning: UdpDirectTuning,
    options: UdpTestOptions,
) -> Option<PathMetrics> {
    println!();
    println!("UDP direct path    : trying QUIC hole punching");
    let conn = match establish_direct(socket, role, peer_candidates, token, tuning).await {
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
    _tuning: UdpDirectTuning,
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
    tuning: UdpDirectTuning,
) -> Result<holepunch::DirectConn> {
    match role {
        UdpTestRole::Listener => {
            let listener = holepunch::DirectListener::new(socket, peer_candidates, tuning)
                .await
                .context("start diagnostic QUIC listener")?;
            Ok(timeout(Duration::from_secs(10), listener.accept(token))
                .await
                .context("timed out waiting for direct QUIC peer")??)
        }
        UdpTestRole::Dialer => {
            holepunch::connect_direct(socket, peer_candidates, token, tuning).await
        }
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

    metrics.transport = path.transport_snapshot();

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

    fn transport_snapshot(&self) -> Option<DirectTransportMetrics> {
        match self {
            TestPath::Tcp { .. } => None,
            #[cfg(feature = "udp")]
            TestPath::Direct(conn) => Some(DirectTransportMetrics::from_direct(conn)),
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct PeerMetrics {
    host: HostMetrics,
    udp: Option<PathMetrics>,
    tcp: Option<PathMetrics>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct PathMetrics {
    latency_send: Option<LatencyMetrics>,
    bandwidth_send: Option<BandwidthMetrics>,
    bandwidth_recv: Option<BandwidthMetrics>,
    transport: Option<DirectTransportMetrics>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct HostMetrics {
    duration_secs: f64,
    process_cpu_seconds: Option<f64>,
    process_cpu_pct: Option<f64>,
    process_rss_bytes: Option<u64>,
    process_vsize_bytes: Option<u64>,
    process_threads: Option<u64>,
    process_minor_faults: Option<u64>,
    process_major_faults: Option<u64>,
    system_cpu_busy_pct: Option<f64>,
    system_cpu_idle_pct: Option<f64>,
    system_load: Option<LoadAverageMetrics>,
    memory: Option<MemoryMetrics>,
    cpu_cores: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct LoadAverageMetrics {
    one: f32,
    five: f32,
    fifteen: f32,
    cur: u32,
    max: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct MemoryMetrics {
    mem_total_bytes: u64,
    mem_free_bytes: u64,
    mem_available_bytes: Option<u64>,
    swap_total_bytes: u64,
    swap_free_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct DirectTransportMetrics {
    udp_tx: UdpStatsMetrics,
    udp_rx: UdpStatsMetrics,
    frame_tx: FrameStatsMetrics,
    frame_rx: FrameStatsMetrics,
    path: PathStatsMetrics,
    max_datagram_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct UdpStatsMetrics {
    datagrams: u64,
    bytes: u64,
    ios: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct FrameStatsMetrics {
    acks: u64,
    ack_frequency: u64,
    crypto: u64,
    connection_close: u64,
    data_blocked: u64,
    datagram: u64,
    handshake_done: u64,
    immediate_ack: u64,
    max_data: u64,
    max_stream_data: u64,
    max_streams_bidi: u64,
    max_streams_uni: u64,
    new_connection_id: u64,
    new_token: u64,
    path_challenge: u64,
    path_response: u64,
    ping: u64,
    reset_stream: u64,
    retire_connection_id: u64,
    stream_data_blocked: u64,
    streams_blocked_bidi: u64,
    streams_blocked_uni: u64,
    stop_sending: u64,
    stream: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct PathStatsMetrics {
    rtt_ms: f64,
    cwnd_bytes: u64,
    congestion_events: u64,
    lost_packets: u64,
    lost_bytes: u64,
    sent_packets: u64,
    sent_plpmtud_probes: u64,
    lost_plpmtud_probes: u64,
    black_holes_detected: u64,
    current_mtu_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct LatencyMetrics {
    samples: u16,
    min_ms: f64,
    median_ms: f64,
    avg_ms: f64,
    stdev_ms: f64,
    max_ms: f64,
}

impl LatencyMetrics {
    fn from_samples(samples: &[Duration]) -> Self {
        let mut values: Vec<f64> = samples
            .iter()
            .map(|sample| sample.as_secs_f64() * 1000.0)
            .collect();
        values.sort_by(|a, b| a.total_cmp(b));
        let min = *values.first().unwrap_or(&0.0);
        let max = *values.last().unwrap_or(&0.0);
        let sum: f64 = values.iter().sum();
        let avg = sum / values.len().max(1) as f64;
        let median = match values.len() {
            0 => 0.0,
            len if len % 2 == 0 => {
                let upper = len / 2;
                (values[upper - 1] + values[upper]) / 2.0
            }
            len => values[len / 2],
        };
        let variance = if values.is_empty() {
            0.0
        } else {
            values
                .iter()
                .map(|value| {
                    let diff = *value - avg;
                    diff * diff
                })
                .sum::<f64>()
                / values.len() as f64
        };
        Self {
            samples: values.len() as u16,
            min_ms: min,
            median_ms: median,
            avg_ms: avg,
            stdev_ms: variance.sqrt(),
            max_ms: max,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct HostSample {
    process: Option<ProcessSample>,
    kernel_stats: Option<KernelStats>,
    load: Option<LoadAverage>,
    memory: Option<Meminfo>,
    cpu_cores: Option<usize>,
}

#[cfg(not(target_os = "linux"))]
#[derive(Clone)]
struct HostSample;

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ProcessSample {
    cpu_ticks: u64,
    rss_bytes: u64,
    vsize_bytes: u64,
    threads: u64,
    minor_faults: u64,
    major_faults: u64,
}

#[cfg(target_os = "linux")]
fn capture_host_sample() -> Option<HostSample> {
    let process = Process::myself()
        .ok()
        .and_then(|process| process.stat().ok())
        .map(|stat| {
            let page_size = page_size();
            ProcessSample {
                cpu_ticks: stat.utime + stat.stime,
                rss_bytes: stat.rss.saturating_mul(page_size),
                vsize_bytes: stat.vsize,
                threads: stat.num_threads.max(0) as u64,
                minor_faults: stat.minflt,
                major_faults: stat.majflt,
            }
        });
    Some(HostSample {
        process,
        kernel_stats: KernelStats::current().ok(),
        load: LoadAverage::current().ok(),
        memory: Meminfo::current().ok(),
        cpu_cores: CpuInfo::current().ok().map(|info| info.num_cores()),
    })
}

#[cfg(not(target_os = "linux"))]
fn capture_host_sample() -> Option<HostSample> {
    None
}

#[cfg(target_os = "linux")]
fn build_host_metrics(
    start: Option<&HostSample>,
    end: Option<&HostSample>,
    elapsed: Duration,
) -> HostMetrics {
    let duration_secs = elapsed.as_secs_f64().max(0.000_001);
    let mut metrics = HostMetrics {
        duration_secs,
        ..HostMetrics::default()
    };

    let Some(end) = end else {
        return metrics;
    };

    if let Some(process) = &end.process {
        metrics.process_rss_bytes = Some(process.rss_bytes);
        metrics.process_vsize_bytes = Some(process.vsize_bytes);
        metrics.process_threads = Some(process.threads);
        metrics.process_minor_faults = Some(process.minor_faults);
        metrics.process_major_faults = Some(process.major_faults);
    }
    metrics.cpu_cores = end.cpu_cores.or(start.and_then(|sample| sample.cpu_cores));
    metrics.memory = end.memory.as_ref().map(|memory| MemoryMetrics {
        mem_total_bytes: memory.mem_total,
        mem_free_bytes: memory.mem_free,
        mem_available_bytes: memory.mem_available,
        swap_total_bytes: memory.swap_total,
        swap_free_bytes: memory.swap_free,
    });
    metrics.system_load = end.load.as_ref().map(|load| LoadAverageMetrics {
        one: load.one,
        five: load.five,
        fifteen: load.fifteen,
        cur: load.cur,
        max: load.max,
    });

    if let (Some(start), Some(end_process)) = (
        start.and_then(|sample| sample.process.as_ref()),
        end.process.as_ref(),
    ) {
        let ticks = ticks_per_second() as f64;
        let cpu_ticks = end_process.cpu_ticks.saturating_sub(start.cpu_ticks);
        let process_cpu_seconds = cpu_ticks as f64 / ticks;
        metrics.process_cpu_seconds = Some(process_cpu_seconds);
        metrics.process_cpu_pct = Some(process_cpu_seconds / duration_secs * 100.0);
    }

    if let (Some(start), Some(end_stats)) = (
        start.and_then(|sample| sample.kernel_stats.as_ref()),
        end.kernel_stats.as_ref(),
    ) {
        let start_cpu = &start.total;
        let end_cpu = &end_stats.total;
        let start_busy = cpu_busy_ticks(start_cpu);
        let end_busy = cpu_busy_ticks(end_cpu);
        let start_total = cpu_total_ticks(start_cpu);
        let end_total = cpu_total_ticks(end_cpu);
        let busy_delta = end_busy.saturating_sub(start_busy) as f64;
        let total_delta = end_total.saturating_sub(start_total) as f64;
        if total_delta > 0.0 {
            metrics.system_cpu_busy_pct = Some(busy_delta / total_delta * 100.0);
            metrics.system_cpu_idle_pct = Some(100.0 - metrics.system_cpu_busy_pct.unwrap_or(0.0));
        }
    }

    metrics
}

#[cfg(not(target_os = "linux"))]
fn build_host_metrics(
    _start: Option<&HostSample>,
    _end: Option<&HostSample>,
    elapsed: Duration,
) -> HostMetrics {
    HostMetrics {
        duration_secs: elapsed.as_secs_f64().max(0.000_001),
        ..HostMetrics::default()
    }
}

#[cfg(target_os = "linux")]
fn cpu_busy_ticks(cpu: &CpuTime) -> u64 {
    cpu.user
        + cpu.nice
        + cpu.system
        + cpu.irq.unwrap_or(0)
        + cpu.softirq.unwrap_or(0)
        + cpu.steal.unwrap_or(0)
        + cpu.guest.unwrap_or(0)
        + cpu.guest_nice.unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn cpu_total_ticks(cpu: &CpuTime) -> u64 {
    cpu_busy_ticks(cpu) + cpu.idle + cpu.iowait.unwrap_or(0)
}

impl DirectTransportMetrics {
    fn from_direct(conn: &crate::holepunch::DirectConn) -> Self {
        let stats = conn.stats();
        Self {
            udp_tx: UdpStatsMetrics::from(stats.udp_tx),
            udp_rx: UdpStatsMetrics::from(stats.udp_rx),
            frame_tx: FrameStatsMetrics::from(stats.frame_tx),
            frame_rx: FrameStatsMetrics::from(stats.frame_rx),
            path: PathStatsMetrics::from(stats.path),
            max_datagram_size_bytes: conn.max_datagram_size().map(|size| size as u64),
        }
    }
}

impl From<quinn::UdpStats> for UdpStatsMetrics {
    fn from(stats: quinn::UdpStats) -> Self {
        Self {
            datagrams: stats.datagrams,
            bytes: stats.bytes,
            ios: stats.ios,
        }
    }
}

impl From<quinn::PathStats> for PathStatsMetrics {
    fn from(stats: quinn::PathStats) -> Self {
        Self {
            rtt_ms: stats.rtt.as_secs_f64() * 1000.0,
            cwnd_bytes: stats.cwnd,
            congestion_events: stats.congestion_events,
            lost_packets: stats.lost_packets,
            lost_bytes: stats.lost_bytes,
            sent_packets: stats.sent_packets,
            sent_plpmtud_probes: stats.sent_plpmtud_probes,
            lost_plpmtud_probes: stats.lost_plpmtud_probes,
            black_holes_detected: stats.black_holes_detected,
            current_mtu_bytes: stats.current_mtu as u64,
        }
    }
}

impl FrameStatsMetrics {
    fn from(stats: quinn::FrameStats) -> Self {
        Self {
            acks: stats.acks,
            ack_frequency: stats.ack_frequency,
            crypto: stats.crypto,
            connection_close: stats.connection_close,
            data_blocked: stats.data_blocked,
            datagram: stats.datagram,
            handshake_done: stats.handshake_done as u64,
            immediate_ack: stats.immediate_ack,
            max_data: stats.max_data,
            max_stream_data: stats.max_stream_data,
            max_streams_bidi: stats.max_streams_bidi,
            max_streams_uni: stats.max_streams_uni,
            new_connection_id: stats.new_connection_id,
            new_token: stats.new_token,
            path_challenge: stats.path_challenge,
            path_response: stats.path_response,
            ping: stats.ping,
            reset_stream: stats.reset_stream,
            retire_connection_id: stats.retire_connection_id,
            stream_data_blocked: stats.stream_data_blocked,
            streams_blocked_bidi: stats.streams_blocked_bidi,
            streams_blocked_uni: stats.streams_blocked_uni,
            stop_sending: stats.stop_sending,
            stream: stats.stream,
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
            "{label} : latency sent avg {:.2} ms (median {:.2}, stdev {:.2}, min {:.2}, max {:.2}, n={})",
            latency.avg_ms, latency.median_ms, latency.stdev_ms, latency.min_ms, latency.max_ms, latency.samples
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
    if let Some(transport) = &metrics.transport {
        print_transport_metrics(label, transport);
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
    print_host_metrics("Local host", &metrics.host);
    if let Some(peer_metrics) = peer_metrics {
        print_host_metrics("Peer host", &peer_metrics.host);
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
    if let Some(udp) = &metrics.udp {
        print_path_bottleneck_hints("UDP direct path", udp);
    }
    if let Some(tcp) = &metrics.tcp {
        print_path_bottleneck_hints("TCP relay fallback", tcp);
    }
    if metrics.udp.is_none() && metrics.tcp.is_some() {
        println!("Recommendation    : use the TCP relay fallback; direct UDP failed on this pair.");
    } else if metrics.udp.is_some() {
        println!(
            "Recommendation    : direct UDP is usable; TCP relay remains available as fallback."
        );
    }
}

fn print_host_metrics(label: &str, host: &HostMetrics) {
    println!("{label}          : sampled over {:.2}s", host.duration_secs);
    if let Some(cpu_seconds) = host.process_cpu_seconds {
        let pct = host.process_cpu_pct.unwrap_or(0.0);
        println!(
            "{label} CPU      : {:.2}s ({:.1}% of one core)",
            cpu_seconds, pct
        );
    }
    if let Some(rss) = host.process_rss_bytes {
        println!("{label} RSS      : {}", format_bytes(rss));
    }
    if let Some(vsize) = host.process_vsize_bytes {
        println!("{label} VSIZE    : {}", format_bytes(vsize));
    }
    if let Some(threads) = host.process_threads {
        println!("{label} threads  : {threads}");
    }
    if let Some(minor_faults) = host.process_minor_faults {
        println!("{label} faults   : minor {minor_faults}");
    }
    if let Some(major_faults) = host.process_major_faults {
        println!("{label} faults   : major {major_faults}");
    }
    if let Some(system_cpu_busy_pct) = host.system_cpu_busy_pct {
        println!(
            "{label} system   : busy {:.1}%, idle {:.1}%",
            system_cpu_busy_pct,
            host.system_cpu_idle_pct.unwrap_or(0.0)
        );
    }
    if let Some(load) = &host.system_load {
        println!(
            "{label} load     : {:.2} / {:.2} / {:.2} (cur {}, max {})",
            load.one, load.five, load.fifteen, load.cur, load.max
        );
    }
    if let Some(memory) = &host.memory {
        println!(
            "{label} memory   : avail {} / total {}, swap free {} / total {}",
            memory
                .mem_available_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "<n/a>".to_string()),
            format_bytes(memory.mem_total_bytes),
            format_bytes(memory.swap_free_bytes),
            format_bytes(memory.swap_total_bytes)
        );
    }

    let hints = host_bottleneck_hints(host);
    if !hints.is_empty() {
        println!("{label} hint    : {}", hints.join(", "));
    }
}

fn print_path_bottleneck_hints(label: &str, metrics: &PathMetrics) {
    let hints = path_bottleneck_hints(metrics);
    if !hints.is_empty() {
        println!("{label} analysis : {}", hints.join(", "));
    }
}

fn host_bottleneck_hints(host: &HostMetrics) -> Vec<String> {
    let mut hints = Vec::new();
    if host.process_cpu_pct.is_some_and(|pct| pct >= 85.0) {
        hints.push("process CPU saturation".to_string());
    }
    if host.system_cpu_busy_pct.is_some_and(|pct| pct >= 85.0) {
        hints.push("machine CPU saturation".to_string());
    }
    if let Some(memory) = &host.memory {
        if memory
            .mem_available_bytes
            .is_some_and(|available| available < memory.mem_total_bytes / 10)
        {
            hints.push("low available RAM".to_string());
        }
    }
    if host
        .system_load
        .as_ref()
        .and_then(|load| host.cpu_cores.map(|cores| (load.one, cores)))
        .is_some_and(|(load_one, cores)| load_one > cores as f32)
    {
        hints.push("load average above core count".to_string());
    }
    if host.process_major_faults.is_some_and(|faults| faults > 0) {
        hints.push("major page faults observed".to_string());
    }
    hints
}

fn transport_bottleneck_hints(transport: &DirectTransportMetrics) -> Vec<String> {
    let mut hints = Vec::new();
    if transport.path.rtt_ms >= 100.0 {
        hints.push(format!("high QUIC RTT ({:.2} ms)", transport.path.rtt_ms));
    }
    if transport.path.lost_packets > 0 || transport.path.lost_bytes > 0 {
        hints.push("QUIC loss visible".to_string());
    }
    if transport.path.black_holes_detected > 0 {
        hints.push("PMTUD black holes detected".to_string());
    }
    if transport.frame_tx.data_blocked > 0 || transport.frame_rx.data_blocked > 0 {
        hints.push("connection flow-control pressure".to_string());
    }
    if transport.frame_tx.stream_data_blocked > 0 || transport.frame_rx.stream_data_blocked > 0 {
        hints.push("stream flow-control pressure".to_string());
    }
    if transport.frame_tx.streams_blocked_bidi > 0 || transport.frame_rx.streams_blocked_bidi > 0 {
        hints.push("bidi stream limit pressure".to_string());
    }
    if transport.path.current_mtu_bytes < 1200 {
        hints.push("small MTU may cap throughput".to_string());
    }
    hints
}

fn path_bottleneck_hints(metrics: &PathMetrics) -> Vec<String> {
    let mut hints = Vec::new();
    if let Some(latency) = &metrics.latency_send {
        if latency.avg_ms >= 100.0 {
            hints.push(format!("high latency ({:.2} ms avg)", latency.avg_ms));
        } else if latency.stdev_ms >= latency.avg_ms.max(1.0) {
            hints.push(format!(
                "jittery latency ({:.2} ms stdev)",
                latency.stdev_ms
            ));
        }
    }
    if let Some(bw) = &metrics.bandwidth_send {
        if bw.mbps < 1.0 {
            hints.push(format!("slow send throughput ({:.2} Mbit/s)", bw.mbps));
        }
    }
    if let Some(bw) = &metrics.bandwidth_recv {
        if bw.mbps < 1.0 {
            hints.push(format!("slow receive throughput ({:.2} Mbit/s)", bw.mbps));
        }
    }
    if let Some(transport) = &metrics.transport {
        hints.extend(transport_bottleneck_hints(transport));
    }
    hints
}

fn print_transport_metrics(label: &str, transport: &DirectTransportMetrics) {
    println!(
        "{label} QUIC    : rtt {:.2} ms, cwnd {}, mtu {}, max datagram {}, loss {} pkts / {} B, sent {} pkts",
        transport.path.rtt_ms,
        format_bytes(transport.path.cwnd_bytes),
        format_bytes(transport.path.current_mtu_bytes),
        transport
            .max_datagram_size_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "<n/a>".to_string()),
        transport.path.lost_packets,
        format_bytes(transport.path.lost_bytes),
        transport.path.sent_packets,
    );
    println!(
        "{label} UDP     : tx {} datagrams / {} B / {} ios, rx {} datagrams / {} B / {} ios",
        transport.udp_tx.datagrams,
        format_bytes(transport.udp_tx.bytes),
        transport.udp_tx.ios,
        transport.udp_rx.datagrams,
        format_bytes(transport.udp_rx.bytes),
        transport.udp_rx.ios,
    );
    println!(
        "{label} frames  : tx acks {}, stream {}, datagram {}, blocked {} / {}, rx acks {}, stream {}, datagram {}, blocked {} / {}",
        transport.frame_tx.acks,
        transport.frame_tx.stream,
        transport.frame_tx.datagram,
        transport.frame_tx.data_blocked,
        transport.frame_tx.stream_data_blocked,
        transport.frame_rx.acks,
        transport.frame_rx.stream,
        transport.frame_rx.datagram,
        transport.frame_rx.data_blocked,
        transport.frame_rx.stream_data_blocked,
    );

    let hints = transport_bottleneck_hints(transport);
    if !hints.is_empty() {
        println!("{label} hint    : {}", hints.join(", "));
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
        assert_eq!(metrics.median_ms, 2.0);
        assert_eq!(metrics.avg_ms, 2.0);
        assert!((metrics.stdev_ms - 0.816_496_580_927_726).abs() < 1e-12);
        assert_eq!(metrics.max_ms, 3.0);
    }

    #[test]
    fn host_bottleneck_hints_flag_cpu_and_memory_pressure() {
        let host = HostMetrics {
            duration_secs: 12.0,
            process_cpu_seconds: Some(10.5),
            process_cpu_pct: Some(87.5),
            process_rss_bytes: Some(512 * 1024 * 1024),
            process_vsize_bytes: Some(768 * 1024 * 1024),
            process_threads: Some(12),
            process_minor_faults: Some(42),
            process_major_faults: Some(1),
            system_cpu_busy_pct: Some(91.0),
            system_cpu_idle_pct: Some(9.0),
            system_load: Some(LoadAverageMetrics {
                one: 5.0,
                five: 4.0,
                fifteen: 3.0,
                cur: 6,
                max: 8,
            }),
            memory: Some(MemoryMetrics {
                mem_total_bytes: 1_000,
                mem_free_bytes: 120,
                mem_available_bytes: Some(80),
                swap_total_bytes: 256,
                swap_free_bytes: 128,
            }),
            cpu_cores: Some(4),
        };

        let hints = host_bottleneck_hints(&host);
        assert!(hints
            .iter()
            .any(|hint| hint.contains("process CPU saturation")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("machine CPU saturation")));
        assert!(hints.iter().any(|hint| hint.contains("low available RAM")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("load average above core count")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("major page faults observed")));
    }

    #[test]
    fn path_bottleneck_hints_flag_transport_pressure() {
        let transport = DirectTransportMetrics {
            udp_tx: UdpStatsMetrics {
                datagrams: 10,
                bytes: 1_000,
                ios: 2,
            },
            udp_rx: UdpStatsMetrics {
                datagrams: 8,
                bytes: 900,
                ios: 1,
            },
            frame_tx: FrameStatsMetrics {
                data_blocked: 2,
                stream_data_blocked: 1,
                streams_blocked_bidi: 1,
                ..FrameStatsMetrics::default()
            },
            frame_rx: FrameStatsMetrics::default(),
            path: PathStatsMetrics {
                rtt_ms: 150.0,
                cwnd_bytes: 64 * 1024,
                congestion_events: 1,
                lost_packets: 3,
                lost_bytes: 256,
                sent_packets: 42,
                sent_plpmtud_probes: 1,
                lost_plpmtud_probes: 0,
                black_holes_detected: 1,
                current_mtu_bytes: 1100,
            },
            max_datagram_size_bytes: Some(1200),
        };
        let metrics = PathMetrics {
            latency_send: Some(LatencyMetrics {
                samples: 3,
                min_ms: 110.0,
                median_ms: 150.0,
                avg_ms: 140.0,
                stdev_ms: 16.0,
                max_ms: 160.0,
            }),
            bandwidth_send: Some(BandwidthMetrics {
                bytes: 10,
                seconds: 1.0,
                mbps: 0.08,
                ack_observed: false,
            }),
            bandwidth_recv: Some(BandwidthMetrics {
                bytes: 12,
                seconds: 1.0,
                mbps: 0.10,
                ack_observed: true,
            }),
            transport: Some(transport),
        };

        let hints = path_bottleneck_hints(&metrics);
        assert!(hints.iter().any(|hint| hint.contains("high latency")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("slow send throughput")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("slow receive throughput")));
        assert!(hints.iter().any(|hint| hint.contains("high QUIC RTT")));
        assert!(hints.iter().any(|hint| hint.contains("QUIC loss visible")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("PMTUD black holes detected")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("connection flow-control pressure")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("stream flow-control pressure")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("bidi stream limit pressure")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("small MTU may cap throughput")));
    }

    #[test]
    fn telemetry_round_trips_through_json() {
        let peer = PeerMetrics {
            host: HostMetrics {
                duration_secs: 9.5,
                process_cpu_seconds: Some(3.25),
                process_cpu_pct: Some(34.2),
                process_rss_bytes: Some(123_456),
                process_vsize_bytes: Some(234_567),
                process_threads: Some(4),
                process_minor_faults: Some(11),
                process_major_faults: Some(0),
                system_cpu_busy_pct: Some(28.0),
                system_cpu_idle_pct: Some(72.0),
                system_load: Some(LoadAverageMetrics {
                    one: 0.5,
                    five: 0.4,
                    fifteen: 0.3,
                    cur: 1,
                    max: 4,
                }),
                memory: Some(MemoryMetrics {
                    mem_total_bytes: 8_000,
                    mem_free_bytes: 2_000,
                    mem_available_bytes: Some(3_000),
                    swap_total_bytes: 1_000,
                    swap_free_bytes: 900,
                }),
                cpu_cores: Some(8),
            },
            udp: Some(PathMetrics {
                latency_send: Some(LatencyMetrics {
                    samples: 2,
                    min_ms: 1.0,
                    median_ms: 1.5,
                    avg_ms: 1.5,
                    stdev_ms: 0.5,
                    max_ms: 2.0,
                }),
                bandwidth_send: Some(BandwidthMetrics {
                    bytes: 2048,
                    seconds: 1.0,
                    mbps: 0.016,
                    ack_observed: true,
                }),
                bandwidth_recv: None,
                transport: Some(DirectTransportMetrics {
                    udp_tx: UdpStatsMetrics {
                        datagrams: 1,
                        bytes: 2,
                        ios: 3,
                    },
                    udp_rx: UdpStatsMetrics {
                        datagrams: 4,
                        bytes: 5,
                        ios: 6,
                    },
                    frame_tx: FrameStatsMetrics {
                        acks: 7,
                        ack_frequency: 8,
                        stream: 9,
                        ..FrameStatsMetrics::default()
                    },
                    frame_rx: FrameStatsMetrics::default(),
                    path: PathStatsMetrics {
                        rtt_ms: 23.0,
                        cwnd_bytes: 64_000,
                        congestion_events: 0,
                        lost_packets: 0,
                        lost_bytes: 0,
                        sent_packets: 10,
                        sent_plpmtud_probes: 0,
                        lost_plpmtud_probes: 0,
                        black_holes_detected: 0,
                        current_mtu_bytes: 1400,
                    },
                    max_datagram_size_bytes: Some(1350),
                }),
            }),
            tcp: None,
        };

        let encoded = serde_json::to_string(&peer).expect("serialize telemetry");
        let decoded: PeerMetrics = serde_json::from_str(&encoded).expect("deserialize telemetry");
        assert_eq!(decoded, peer);
    }
}
