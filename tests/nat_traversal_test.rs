//! NAT-lab traversal baseline (plan Fase 0).
//!
//! Runs the REAL production traversal stack (STUN gather → punch → QUIC
//! direct) through deterministic userspace NAT emulation (`support/natlab.rs`)
//! — no public STUN, no root, no netns. Each test is one row of the baseline
//! table in `docs/test/TEST_UDP.md`: it pins today's direct/relay outcome per
//! NAT-profile pair, so a later traversal change (Fase 2 connectivity checks)
//! must flip the RED rows without regressing the green ones.
//!
//! Linux-only: the lab binds per-box loopback addresses (127.0.0.0/8), which
//! needs no setup on Linux but is not available by default on macOS/Windows.
#![cfg(all(feature = "udp", target_os = "linux"))]

#[path = "support/natlab.rs"]
mod natlab;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bore_cli::holepunch::{
    self, connect_direct, derive_check_key, derive_token, dialer_checks_then_quic,
    listener_checks_then_quic, run_stun_responder, CandidateDiscovery, CheckConfig, CheckOutcome,
    CheckRole, DirectConn, DirectListener, StunSource, StunTarget, CHECK_WINDOW,
};
use bore_cli::shared::{UdpCandidateKind, UdpDirectTuning, UDP_NONCE_LEN};
use natlab::{Filtering, Mapping, NatBox, NatPolicy, PortAlloc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::time::{sleep, timeout};

/// Listener-side wait for the dialer's QUIC handshake. Must exceed the
/// dialer's start delay + punch (~550ms) + its 3s connect budget.
const ACCEPT_WAIT: Duration = Duration::from_secs(6);

/// One lab peer: its punch socket, discovery result, and (optional) NAT box.
struct Peer {
    socket: UdpSocket,
    discovery: CandidateDiscovery,
    nat: Option<Arc<NatBox>>,
}

/// Outcome of one traversal attempt.
///
/// CRITICAL: holds the two peers' `NatBox`es. Dropping a `NatBox` aborts its
/// forwarding tasks, silently black-holing an established direct connection —
/// exactly what happened when `attempt_direct` consumed the `Peer`s and the
/// boxes died before the ping/pong proof ran. Keep this struct alive for as
/// long as the connections are used.
struct Attempt {
    listener: Result<DirectConn>,
    dialer: Result<DirectConn>,
    /// Dialer-side check outcome (`None` on the legacy blind path).
    dialer_outcome: Option<CheckOutcome>,
    _nats: (Option<Arc<NatBox>>, Option<Arc<NatBox>>),
}

/// Stand up the world STUN responder on its own loopback "public" IP.
async fn world_stun() -> Result<SocketAddr> {
    let sock = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 99), 0))
        .await
        .context("bind world STUN")?;
    let addr = sock.local_addr()?;
    tokio::spawn(run_stun_responder(sock));
    Ok(addr)
}

/// Bind a punch socket and run real candidate discovery, with the STUN server
/// reached through the peer's NAT (or directly when the peer has none).
async fn setup_peer(
    nat: Option<Arc<NatBox>>,
    stun_world: SocketAddr,
    prediction: bool,
) -> Result<Peer> {
    setup_peer_opts(
        nat,
        stun_world,
        holepunch::GatherOptions::from_flags(false, prediction),
    )
    .await
}

/// Like [`setup_peer`], with full gather options (Fase 5: manual candidates,
/// `--udp-no-stun`).
async fn setup_peer_opts(
    nat: Option<Arc<NatBox>>,
    stun_world: SocketAddr,
    opts: holepunch::GatherOptions,
) -> Result<Peer> {
    // Production path (Fase 1): single-owner traversal socket for discovery,
    // then the handoff releases the raw socket for punch/QUIC — proving the
    // handoff works through an emulated NAT, not just on loopback.
    let tsock = holepunch::UdpTraversalSocket::bind(0).await?;
    let stun_addr = match &nat {
        Some(nat) => nat.alias(stun_world).await?,
        None => stun_world,
    };
    let targets = [StunTarget {
        requested: "lab-stun".to_string(),
        addr: stun_addr,
        source: StunSource::Single,
    }];
    let discovery = holepunch::gather_candidates_traversal(&tsock, &targets, &opts).await;
    let socket = tsock.into_socket().await?;
    Ok(Peer {
        socket,
        discovery,
        nat,
    })
}

/// The candidates a peer would put on the wire for a NAT-separated peer:
/// reflexive + predicted + router-mapped (manual, Fase 5) only — local
/// candidates would short-circuit the lab, since both "LANs" live on one
/// host.
fn wan_candidates(d: &CandidateDiscovery) -> Vec<SocketAddr> {
    d.candidates
        .iter()
        .zip(d.candidate_kinds.iter())
        .filter(|(_, k)| {
            matches!(
                k,
                UdpCandidateKind::Reflexive
                    | UdpCandidateKind::Predicted
                    | UdpCandidateKind::RouterMapped
            )
        })
        .map(|(a, _)| *a)
        .collect()
}

/// Rewrite WAN candidates through the receiving peer's NAT aliases — the lab's
/// stand-in for "the default route goes through the NAT".
async fn route_via(nat: &Option<Arc<NatBox>>, cands: &[SocketAddr]) -> Result<Vec<SocketAddr>> {
    let mut out = Vec::with_capacity(cands.len());
    for &c in cands {
        out.push(match nat {
            Some(nat) => nat.alias(c).await?,
            None => c,
        });
    }
    Ok(out)
}

/// Full traversal attempt on the PRODUCTION Fase-2 path (both peers support
/// authenticated connectivity checks, as every new/new secret pair does):
/// the check round replaces the blind punch, learns peer-reflexive sources,
/// nominates, then QUIC — mirroring the brokered order (listener first).
async fn attempt_direct(listener: Peer, dialer: Peer) -> Attempt {
    let token = derive_token(Some("natlab"), &[7u8; UDP_NONCE_LEN]);
    let key = derive_check_key(&token);
    let tuning = UdpDirectTuning::default();
    let l_peers = route_via(&listener.nat, &wan_candidates(&dialer.discovery))
        .await
        .expect("listener candidate rewrite");
    let d_peers = route_via(&dialer.nat, &wan_candidates(&listener.discovery))
        .await
        .expect("dialer candidate rewrite");

    let l_sock = listener.socket;
    let d_sock = dialer.socket;
    let l_task = async move {
        let cfg = CheckConfig {
            key,
            generation: 1,
            role: CheckRole::Listener,
            window: CHECK_WINDOW,
            plan: None,
        };
        let (dl, _outcome) = listener_checks_then_quic(l_sock, &l_peers, &cfg, tuning).await?;
        timeout(ACCEPT_WAIT, dl.accept(token))
            .await
            .context("listener accept timed out")?
    };
    let d_task = async move {
        // The broker tells the provider first; give its round a head start.
        sleep(Duration::from_millis(300)).await;
        let cfg = CheckConfig {
            key,
            generation: 1,
            role: CheckRole::Dialer,
            window: CHECK_WINDOW,
            plan: None,
        };
        dialer_checks_then_quic(d_sock, d_peers, &cfg, token, tuning, None).await
    };
    let (l, d) = tokio::join!(l_task, d_task);
    let (d, dialer_outcome) = match d {
        Ok((conn, outcome)) => (Ok(conn), Some(outcome)),
        Err(e) => (Err(e), None),
    };
    Attempt {
        listener: l,
        dialer: d,
        dialer_outcome,
        _nats: (listener.nat, dialer.nat),
    }
}

/// The LEGACY blind path (punch + dial-all, no checks) — what a pair with an
/// old peer still runs. Kept as a regression oracle: a row that was RED on
/// this path must STAY red here even after Fase 2 flips it on the checked
/// path above.
async fn attempt_direct_legacy(listener: Peer, dialer: Peer) -> Attempt {
    let token = derive_token(Some("natlab"), &[7u8; UDP_NONCE_LEN]);
    let tuning = UdpDirectTuning::default();
    let l_peers = route_via(&listener.nat, &wan_candidates(&dialer.discovery))
        .await
        .expect("listener candidate rewrite");
    let d_peers = route_via(&dialer.nat, &wan_candidates(&listener.discovery))
        .await
        .expect("dialer candidate rewrite");

    let l_sock = listener.socket;
    let d_sock = dialer.socket;
    let l_task = async move {
        let dl = DirectListener::new(l_sock, l_peers, tuning).await?;
        timeout(ACCEPT_WAIT, dl.accept(token))
            .await
            .context("listener accept timed out")?
    };
    let d_task = async move {
        sleep(Duration::from_millis(300)).await;
        connect_direct(d_sock, d_peers, token, tuning).await
    };
    let (l, d) = tokio::join!(l_task, d_task);
    Attempt {
        listener: l,
        dialer: d,
        dialer_outcome: None,
        _nats: (listener.nat, dialer.nat),
    }
}

/// Prove the direct path is really bidirectional: one QUIC stream, ping/pong.
async fn prove_bidirectional(provider: DirectConn, consumer: DirectConn) -> Result<()> {
    let provider_task = tokio::spawn(async move {
        let mut s = provider.accept_stream().await?;
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await?;
        s.write_all(b"pong").await?;
        s.flush().await?;
        // QUIC has no real flush: dropping the last Connection handle when
        // this task returns closes the conn (code 0) and can race the pong
        // out of existence. Park until the consumer closes its side.
        let mut done = [0u8; 1];
        let _ = s.read(&mut done).await;
        anyhow::Ok(buf)
    });
    let mut cs = consumer.open_stream().await?;
    cs.write_all(b"ping").await?;
    cs.flush().await?;
    let mut buf = [0u8; 4];
    timeout(Duration::from_secs(5), cs.read_exact(&mut buf))
        .await
        .context("pong timed out")??;
    assert_eq!(&buf, b"pong");
    drop(cs); // release the stream so the provider task unparks
    let got = timeout(Duration::from_secs(5), provider_task)
        .await
        .context("provider stream timed out")???;
    assert_eq!(&got, b"ping");
    Ok(())
}

/// The attempt must have produced a direct path on BOTH ends, and it must
/// carry real bidirectional data. Keeps the NAT boxes alive throughout.
async fn assert_direct(attempt: Attempt, scenario: &str) {
    let l = attempt
        .listener
        .as_ref()
        .unwrap_or_else(|e| panic!("{scenario}: listener side failed: {e:#}"));
    let d = attempt
        .dialer
        .as_ref()
        .unwrap_or_else(|e| panic!("{scenario}: dialer side failed: {e:#}"));
    prove_bidirectional(l.clone(), d.clone())
        .await
        .unwrap_or_else(|e| panic!("{scenario}: direct path not bidirectional: {e:#}"));
}

fn assert_relay(attempt: &Attempt, scenario: &str) {
    assert!(
        attempt.dialer.is_err(),
        "{scenario}: dialer must NOT reach direct (relay expected) — a pass here \
         without a traversal improvement means the lab leaked around the NAT"
    );
    assert!(
        attempt.listener.is_err(),
        "{scenario}: listener must NOT accept a direct conn (relay expected)"
    );
}

/// Fase 5 row — manual candidates with STUN fully skipped (`--udp-no-stun` +
/// `--udp-candidate`): both peers sit behind EIM+APDF port-preserving NATs
/// and declare their public endpoint BY HAND (`wan_ip:local_port` — exactly
/// the static port-forward / port-preserving case the manual plan targets).
/// No STUN packet is ever sent; the authenticated check round is the only
/// punch. Must go DIRECT.
#[tokio::test]
async fn manual_candidates_no_stun_direct() -> Result<()> {
    async fn manual_peer(n: u8) -> Result<Peer> {
        let nat = NatBox::numbered(n, NatPolicy::cone());
        let tsock = holepunch::UdpTraversalSocket::bind(0).await?;
        let port = tsock
            .local_addr()
            .context("traversal socket has no local addr")?
            .port();
        // Port-preserving NAT: the public mapping is wan_ip:local_port, known
        // a priori — what an operator would pass as --udp-candidate.
        let manual = SocketAddr::new(Ipv4Addr::new(127, 0, 0, n + 1).into(), port);
        let opts = holepunch::GatherOptions {
            manual_candidates: vec![manual],
            no_stun: true,
            ..Default::default()
        };
        let discovery = holepunch::gather_candidates_traversal(&tsock, &[], &opts).await;
        let socket = tsock.into_socket().await?;
        Ok(Peer {
            socket,
            discovery,
            nat: Some(nat),
        })
    }
    let listener = manual_peer(61).await?;
    let dialer = manual_peer(62).await?;
    assert!(
        listener.discovery.selected_stun.is_none(),
        "no STUN must have been probed"
    );
    assert_direct(
        attempt_direct(listener, dialer).await,
        "manual/no-stun cone pair",
    )
    .await;
    Ok(())
}

/// Baseline row 1 — open/open (no NAT anywhere): direct must work.
#[tokio::test]
async fn open_open_direct() -> Result<()> {
    let stun = world_stun().await?;
    let a = setup_peer(None, stun, false).await?;
    let b = setup_peer(None, stun, false).await?;
    assert!(
        a.discovery.selected_stun.is_some(),
        "open peer must see STUN"
    );
    assert_direct(attempt_direct(a, b).await, "open/open").await;
    Ok(())
}

/// Baseline row 2 — cone/cone (EIM+APDF both sides): the punch crossfire
/// opens both filters; direct must work.
#[tokio::test]
async fn cone_cone_direct() -> Result<()> {
    let stun = world_stun().await?;
    let a = setup_peer(Some(NatBox::numbered(11, NatPolicy::cone())), stun, false).await?;
    let b = setup_peer(Some(NatBox::numbered(12, NatPolicy::cone())), stun, false).await?;
    assert_direct(attempt_direct(a, b).await, "cone/cone").await;
    Ok(())
}

/// Baseline row 3 — FLIPPED by Fase 2 (was the RED row): dialer behind
/// EIM+ADF, listener behind symmetric (APDM+APDF). The blind punch could
/// never learn the listener's real per-destination port; the authenticated
/// check round does — the listener's requests pass the dialer's ADF filter,
/// the dialer learns the peer-reflexive source, triggers a check back, and
/// nominates it for the QUIC dial. Must be DIRECT with prflx learned.
#[tokio::test]
async fn eim_adf_dialer_vs_symmetric_listener_direct_via_checks() -> Result<()> {
    let stun = world_stun().await?;
    let adf_cone = NatPolicy {
        mapping: Mapping::Eim,
        filtering: Filtering::Adf,
        alloc: PortAlloc::Preserve,
        hairpin: false,
        drop_all: false,
    };
    let listener = setup_peer(
        Some(NatBox::numbered(21, NatPolicy::symmetric())),
        stun,
        false,
    )
    .await?;
    let dialer = setup_peer(Some(NatBox::numbered(22, adf_cone)), stun, false).await?;
    let attempt = attempt_direct(listener, dialer).await;
    let outcome = attempt
        .dialer_outcome
        .clone()
        .expect("checked path must produce an outcome");
    assert!(
        outcome.learned_prflx,
        "the dialer must learn the symmetric listener's real port as a \
         peer-reflexive candidate (that IS the Fase 2 win)"
    );
    assert_direct(attempt, "EIM+ADF dialer vs APDM listener (checks)").await;
    Ok(())
}

/// Row 3 on the LEGACY blind path (old peer in the pair): must STAY relay —
/// the flip above comes from the checks, not from a lab change.
#[tokio::test]
async fn eim_adf_dialer_vs_symmetric_listener_legacy_stays_relay() -> Result<()> {
    let stun = world_stun().await?;
    let adf_cone = NatPolicy {
        mapping: Mapping::Eim,
        filtering: Filtering::Adf,
        alloc: PortAlloc::Preserve,
        hairpin: false,
        drop_all: false,
    };
    let listener = setup_peer(
        Some(NatBox::numbered(23, NatPolicy::symmetric())),
        stun,
        false,
    )
    .await?;
    let dialer = setup_peer(Some(NatBox::numbered(24, adf_cone)), stun, false).await?;
    let listener_nat = listener.nat.clone().expect("listener NAT");
    let attempt = attempt_direct_legacy(listener, dialer).await;
    assert_relay(&attempt, "EIM+ADF dialer vs APDM listener (legacy)");
    // Deterministic evidence the middlebox (not a lab wiring bug) blocked the
    // path: the symmetric listener's NAT must have filtered inbound datagrams.
    assert!(
        listener_nat.dropped_inbound().await > 0,
        "the symmetric NAT never filtered anything — lab wiring suspect"
    );
    Ok(())
}

/// Baseline row 4 — symmetric dialer vs EIM+ADF listener: works TODAY. The
/// dialer's fresh per-destination mapping reaches the listener because ADF
/// only checks the source IP (opened by the listener's own punch), and QUIC
/// accepts the unknown source (token is the gate, invariant I-6/D7).
#[tokio::test]
async fn symmetric_dialer_vs_eim_adf_listener_direct() -> Result<()> {
    let stun = world_stun().await?;
    let adf_cone = NatPolicy {
        mapping: Mapping::Eim,
        filtering: Filtering::Adf,
        alloc: PortAlloc::Preserve,
        hairpin: false,
        drop_all: false,
    };
    let listener = setup_peer(Some(NatBox::numbered(31, adf_cone)), stun, false).await?;
    let dialer = setup_peer(
        Some(NatBox::numbered(32, NatPolicy::symmetric())),
        stun,
        false,
    )
    .await?;
    assert_direct(
        attempt_direct(listener, dialer).await,
        "APDM dialer vs EIM+ADF listener",
    )
    .await;
    Ok(())
}

/// Baseline row 5 — symmetric/symmetric (APDM+APDF both): must stay relay,
/// with or without future connectivity checks (no false-positive direct).
#[tokio::test]
async fn symmetric_symmetric_stays_relay() -> Result<()> {
    let stun = world_stun().await?;
    let listener = setup_peer(
        Some(NatBox::numbered(41, NatPolicy::symmetric())),
        stun,
        false,
    )
    .await?;
    let dialer = setup_peer(
        Some(NatBox::numbered(42, NatPolicy::symmetric())),
        stun,
        false,
    )
    .await?;
    let attempt = attempt_direct(listener, dialer).await;
    assert_relay(&attempt, "APDM/APDM");
    Ok(())
}

/// Baseline row 6 — same-LAN: local candidates connect directly (no NAT in
/// the path). Uses manually exchanged local addresses, as the wire does.
#[tokio::test]
async fn same_lan_local_candidates_direct() -> Result<()> {
    let l_sock = holepunch::bind_socket(0).await?;
    let d_sock = holepunch::bind_socket(0).await?;
    let l_addr: SocketAddr = format!("127.0.0.1:{}", l_sock.local_addr()?.port()).parse()?;
    let d_addr: SocketAddr = format!("127.0.0.1:{}", d_sock.local_addr()?.port()).parse()?;
    let token = derive_token(Some("natlab"), &[7u8; UDP_NONCE_LEN]);
    let tuning = UdpDirectTuning::default();
    let l_task = async move {
        let dl = DirectListener::new(l_sock, vec![d_addr], tuning).await?;
        timeout(ACCEPT_WAIT, dl.accept(token))
            .await
            .context("listener accept timed out")?
    };
    let d_task = async move {
        sleep(Duration::from_millis(100)).await;
        connect_direct(d_sock, vec![l_addr], token, tuning).await
    };
    let (l, d) = tokio::join!(l_task, d_task);
    prove_bidirectional(l?, d?).await
}

/// Baseline row 7 — outbound UDP blocked on the dialer side: discovery finds
/// no reflexive, nothing leaves the box, both sides settle on the relay.
#[tokio::test]
async fn udp_blocked_dialer_stays_relay() -> Result<()> {
    let stun = world_stun().await?;
    let listener = setup_peer(Some(NatBox::numbered(51, NatPolicy::cone())), stun, false).await?;
    let dialer = setup_peer(
        Some(NatBox::numbered(52, NatPolicy::blocked())),
        stun,
        false,
    )
    .await?;
    assert!(
        dialer.discovery.selected_stun.is_none(),
        "blocked peer must not discover a reflexive address"
    );
    let attempt = attempt_direct(listener, dialer).await;
    assert_relay(&attempt, "UDP blocked dialer");
    Ok(())
}

/// Baseline row 8 — sequential symmetric dialer + port prediction ON vs cone
/// listener: the predicted port matches the NAT's next allocation, so the
/// listener's punch pre-opens it and direct works TODAY. Pins the one case
/// `--try-port-prediction` is for.
#[tokio::test]
async fn sequential_symmetric_dialer_with_prediction_direct() -> Result<()> {
    let stun = world_stun().await?;
    let seq_symmetric = NatPolicy {
        mapping: Mapping::Apdm,
        filtering: Filtering::Apdf,
        alloc: PortAlloc::Sequential(1),
        hairpin: false,
        drop_all: false,
    };
    let listener = setup_peer(Some(NatBox::numbered(61, NatPolicy::cone())), stun, false).await?;
    let dialer = setup_peer(Some(NatBox::numbered(62, seq_symmetric)), stun, true).await?;
    assert!(
        dialer
            .discovery
            .candidate_kinds
            .contains(&UdpCandidateKind::Predicted),
        "prediction must add predicted candidates"
    );
    assert_direct(
        attempt_direct(listener, dialer).await,
        "sequential APDM dialer + prediction vs cone listener",
    )
    .await;
    Ok(())
}
