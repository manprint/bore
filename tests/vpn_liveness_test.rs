//! VPN control liveness (plan `005_plan-OutageRecovery`, phase 2).
//!
//! The field report (a ~20 s outage leaving clients down for ~16 minutes)
//! has a VPN shape too: a VPN link's control connection is a yamux substream,
//! so a peer whose path died is invisible to `send` (it buffers) and to `recv`
//! (it waits), and the server kept the dead side's id — or, after pairing, its
//! half of the link — until the kernel gave up on the socket.
//!
//! These tests speak the control protocol by hand, so they pin the SERVER's
//! behaviour alone: a client that declares `ctrl_heartbeat_ms` is heartbeated
//! while it waits, may beat back, and is reaped on transport silence; a client
//! that declares nothing gets the legacy path, byte for byte, and is never
//! reaped (DEC-VE2: the declaration is the compatibility gate).
//!
//! [`BlackholeProxy`] stops forwarding in both directions while keeping both
//! sockets ESTABLISHED — what a dead path looks like to the two endpoints.
//!
//! Every test owns its own control port: a `#[tokio::test]` runtime releases
//! its listeners only after the body returned.

#![cfg(all(
    feature = "vpn",
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "android"
    )
))]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bore_cli::admin::{AdminRegistry, Role};
use bore_cli::server::Server;
use bore_cli::shared::{ClientMessage, Delimited, ServerMessage, VpnAddrRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{self, Instant};

/// Transport reap floor for these tests (production: 15 s).
const FLOOR: Duration = Duration::from_secs(1);
/// What a test client declares (`max(3 × 200 ms, FLOOR)` = `FLOOR`).
const DECLARED_MS: u32 = 200;
/// How long a reap may take: the floor plus a few heartbeat ticks.
const REAP_BUDGET: Duration = Duration::from_secs(4);
const PORT_WAIT_BUDGET: Duration = Duration::from_secs(30);

// ─── Blackhole proxy ─────────────────────────────────────────────────────────

/// A TCP forwarder `client <-> proxy <-> upstream` that, while blackholed,
/// forwards nothing in either direction and closes nothing.
struct BlackholeProxy {
    addr: SocketAddr,
    blackholed: Arc<AtomicBool>,
}

impl BlackholeProxy {
    async fn start(upstream: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let blackholed = Arc::new(AtomicBool::new(false));
        let bh = blackholed.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(upstream).await else {
                    continue;
                };
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                tokio::spawn(pump(cr, sw, bh.clone()));
                tokio::spawn(pump(sr, cw, bh.clone()));
            }
        });
        Ok(Self { addr, blackholed })
    }

    fn set_blackhole(&self, on: bool) {
        self.blackholed.store(on, Ordering::SeqCst);
    }
}

async fn wait_released(blackholed: &AtomicBool) {
    while blackholed.load(Ordering::SeqCst) {
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    blackholed: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        wait_released(&blackholed).await;
        let n = from.read(&mut buf).await.unwrap_or_default();
        if n == 0 {
            let _ = to.shutdown().await;
            return;
        }
        // A read that completed just as the blackhole went up is held.
        wait_released(&blackholed).await;
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

// ─── Server and protocol helpers ─────────────────────────────────────────────

async fn wait_port(port: u16, listening: bool) {
    let deadline = Instant::now() + PORT_WAIT_BUDGET;
    loop {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "port {port} never became {} within {PORT_WAIT_BUDGET:?}",
            if listening { "reachable" } else { "free" },
        );
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// A VPN-enabled server on `control` with the transport reap floor lowered.
async fn spawn_server(control: u16) -> Result<AdminRegistry> {
    wait_port(control, false).await;
    let mut server = Server::new(1024..=65535, None).transport_reap_floor(FLOOR);
    server.set_control_port(control);
    server.set_vpn(true);
    server.set_vpn_pool("10.97.0.0/16".parse()?)?;
    server.set_vpn_max_links(10);
    let admin = server.admin_registry();
    tokio::spawn(server.listen());
    wait_port(control, true).await;
    Ok(admin)
}

fn control_addr(control: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], control))
}

async fn ctrl(to: SocketAddr) -> Result<Delimited<bore_cli::mux::Stream>> {
    let stream = TcpStream::connect(to).await?;
    bore_cli::shared::tune_tcp(&stream);
    let (opener, _acceptor) = bore_cli::mux::client(stream);
    Ok(Delimited::new(opener.open().await?))
}

fn hello(id: &str, max_clients: u16, ctrl_heartbeat_ms: u32) -> ClientMessage {
    ClientMessage::HelloVpn {
        max_clients,
        id: id.to_string(),
        advertised: vec![],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
        relay_only: false,
        pin_mtu: false,
        mtu: None,
        forward_accept: false,
        nat_masquerade: false,
        route_policy: None,
        nat_udp_preferred_port: 0,
        ctrl_heartbeat_ms,
    }
}

fn connect(id: &str, ctrl_heartbeat_ms: u32) -> ClientMessage {
    ClientMessage::ConnectVpn {
        id: id.to_string(),
        advertised: vec![],
        addr: VpnAddrRequest::Pool,
        notes: None,
        carriers: 1,
        relay_only: false,
        pin_mtu: false,
        mtu: None,
        forward_accept: false,
        nat_masquerade: false,
        route_policy: None,
        nat_udp_preferred_port: 0,
        ctrl_heartbeat_ms,
    }
}

/// The next message that is not a heartbeat, within `within`.
async fn next_non_heartbeat(
    c: &mut Delimited<bore_cli::mux::Stream>,
    within: Duration,
) -> Option<ServerMessage> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match time::timeout(left, c.recv::<ServerMessage>()).await {
            Ok(Ok(Some(ServerMessage::Heartbeat))) => continue,
            Ok(Ok(Some(msg))) => return Some(msg),
            _ => return None,
        }
    }
}

/// The `ctrl_heartbeat` flag of a `VpnReady`, panicking on anything else.
fn ready_flag(msg: Option<ServerMessage>, who: &str) -> bool {
    match msg {
        Some(ServerMessage::VpnReady { ctrl_heartbeat, .. }) => ctrl_heartbeat,
        other => panic!("{who}: expected VpnReady, got {other:?}"),
    }
}

fn count_role(admin: &AdminRegistry, role: Role) -> usize {
    admin.snapshot().iter().filter(|e| e.role == role).count()
}

/// Poll `cond` until it holds or `within` elapses; returns the time taken.
async fn eventually(within: Duration, mut cond: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    loop {
        if cond() {
            return Some(start.elapsed());
        }
        if start.elapsed() >= within {
            return None;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// Register a declared 1:1 listener for `id`; `Ok(true)` when the server
/// accepted it (it heartbeats a declared waiting listener at once),
/// `Ok(false)` when it was refused (the id is still held).
async fn listener_registers(control: u16, id: &str) -> Result<bool> {
    let mut c = ctrl(control_addr(control)).await?;
    c.send(hello(id, 0, DECLARED_MS)).await?;
    match time::timeout(Duration::from_secs(2), c.recv::<ServerMessage>()).await {
        Ok(Ok(Some(ServerMessage::Heartbeat))) => Ok(true),
        Ok(Ok(Some(ServerMessage::VpnError(_)))) => Ok(false),
        other => panic!("unexpected answer to a listener registration: {other:?}"),
    }
}

// ─── Waiting 1:1 listener ────────────────────────────────────────────────────

/// A declared waiting listener is heartbeated; a legacy one hears nothing —
/// an old client bails on any message other than `VpnReady` while it waits.
///
/// RED-CHECK: forcing `declared = false` in `serve_vpn_listener` leaves the
/// declared listener in silence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_listener_gets_heartbeats_only_when_declared() -> Result<()> {
    const CONTROL: u16 = 18901;
    spawn_server(CONTROL).await?;

    let mut declared = ctrl(control_addr(CONTROL)).await?;
    declared.send(hello("wait-new", 0, DECLARED_MS)).await?;
    let mut legacy = ctrl(control_addr(CONTROL)).await?;
    legacy.send(hello("wait-old", 0, 0)).await?;

    // The declared side beats back: it promised to, and a declared listener
    // that stays silent is (correctly) reaped after the 1 s floor.
    let window = time::sleep(Duration::from_millis(1500));
    tokio::pin!(window);
    let mut beat = time::interval(Duration::from_millis(DECLARED_MS.into()));
    let mut beats = 0;
    loop {
        tokio::select! {
            _ = &mut window => break,
            _ = beat.tick() => declared.send(ClientMessage::Heartbeat).await?,
            msg = declared.recv::<ServerMessage>() => match msg {
                Ok(Some(ServerMessage::Heartbeat)) => beats += 1,
                other => panic!("a waiting listener must only be heartbeated, got {other:?}"),
            },
        }
    }
    assert!(
        beats >= 2,
        "a declared waiting listener must be heartbeated (got {beats} in 1.5 s)"
    );
    let got = time::timeout(Duration::from_millis(1500), legacy.recv::<ServerMessage>()).await;
    assert!(
        got.is_err(),
        "a legacy waiting listener must hear nothing before VpnReady, got {got:?}"
    );
    Ok(())
}

/// A declared waiting listener may beat back: the server decodes its
/// `Heartbeat` frames, keeps it registered far past the reap deadline, and
/// still pairs it.
///
/// RED-CHECK: answering a waiting listener's `Heartbeat` like any other
/// message (the legacy "client spoke, so it is gone" arm) frees the id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_listener_accepts_client_heartbeats_when_declared() -> Result<()> {
    const CONTROL: u16 = 18902;
    let admin = spawn_server(CONTROL).await?;

    let mut listener = ctrl(control_addr(CONTROL)).await?;
    listener.send(hello("beats", 0, DECLARED_MS)).await?;
    // Three reap deadlines of waiting, beating every 200 ms.
    let until = Instant::now() + FLOOR * 3;
    let mut beat = time::interval(Duration::from_millis(DECLARED_MS.into()));
    while Instant::now() < until {
        tokio::select! {
            _ = beat.tick() => listener.send(ClientMessage::Heartbeat).await?,
            msg = listener.recv::<ServerMessage>() => assert!(
                matches!(msg, Ok(Some(ServerMessage::Heartbeat))),
                "a waiting listener must only be heartbeated, got {msg:?}"
            ),
        }
    }
    assert_eq!(
        count_role(&admin, Role::VpnListener),
        1,
        "a declared listener that beats must never be reaped"
    );
    assert!(
        !listener_registers(CONTROL, "beats").await?,
        "a listener that beats must keep its id"
    );

    let mut connector = ctrl(control_addr(CONTROL)).await?;
    connector.send(connect("beats", 0)).await?;
    let wait = Duration::from_secs(3);
    ready_flag(next_non_heartbeat(&mut connector, wait).await, "connector");
    ready_flag(next_non_heartbeat(&mut listener, wait).await, "listener");
    Ok(())
}

/// Each side's `VpnReady` reports ITS OWN declaration, never the other
/// side's: the connector's handler builds both copies.
///
/// RED-CHECK: dropping `*ctrl_heartbeat = declared` in the listener handler
/// reports `false` to the declared listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vpn_ready_carries_ctrl_heartbeat_flag() -> Result<()> {
    const CONTROL: u16 = 18909;
    spawn_server(CONTROL).await?;
    let wait = Duration::from_secs(3);

    // Declared listener, legacy connector.
    let mut listener = ctrl(control_addr(CONTROL)).await?;
    listener.send(hello("flag-l", 0, DECLARED_MS)).await?;
    time::sleep(Duration::from_millis(200)).await;
    let mut connector = ctrl(control_addr(CONTROL)).await?;
    connector.send(connect("flag-l", 0)).await?;
    assert!(
        !ready_flag(next_non_heartbeat(&mut connector, wait).await, "connector"),
        "a legacy connector must not be told to beat"
    );
    assert!(
        ready_flag(next_non_heartbeat(&mut listener, wait).await, "listener"),
        "a declared listener must be told the server reaps it"
    );

    // The mirror image: legacy listener, declared connector.
    let mut listener = ctrl(control_addr(CONTROL)).await?;
    listener.send(hello("flag-c", 0, 0)).await?;
    time::sleep(Duration::from_millis(200)).await;
    let mut connector = ctrl(control_addr(CONTROL)).await?;
    connector.send(connect("flag-c", DECLARED_MS)).await?;
    assert!(
        ready_flag(next_non_heartbeat(&mut connector, wait).await, "connector"),
        "a declared connector must be told the server reaps it"
    );
    assert!(
        !ready_flag(next_non_heartbeat(&mut listener, wait).await, "listener"),
        "a legacy listener must not be told to beat"
    );
    Ok(())
}

/// The field case for a VPN: a declared listener waiting for its peer loses
/// its path. Its id must be free again within the transport deadline, so its
/// reconnect is not refused "already in use".
///
/// RED-CHECK: replacing the waiting loop's `reap_if_due(&transport)` with
/// `None` keeps the id held for the whole test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_waiting_listener_is_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18903;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut listener = ctrl(proxy.addr).await?;
    listener.send(hello("field", 0, DECLARED_MS)).await?;
    let got = time::timeout(Duration::from_secs(2), listener.recv::<ServerMessage>()).await;
    assert!(matches!(got, Ok(Ok(Some(ServerMessage::Heartbeat)))));
    assert!(
        !listener_registers(CONTROL, "field").await?,
        "a live listener must keep its id"
    );

    proxy.set_blackhole(true);
    let took = eventually(REAP_BUDGET, || count_role(&admin, Role::VpnListener) == 0).await;
    let took = took.expect(
        "a waiting listener whose path died was never reaped; its reconnect \
         would be refused until the kernel gives up (~15 min)",
    );
    assert!(took < REAP_BUDGET, "took {took:?}");
    assert!(
        listener_registers(CONTROL, "field").await?,
        "the reaped listener's id must be free again"
    );
    Ok(())
}

/// DEC-VE2: a listener that declared nothing is never transport-reaped —
/// reaping a client that cannot beat would kill a healthy idle link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_waiting_listener_is_never_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18904;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut listener = ctrl(proxy.addr).await?;
    listener.send(hello("legacy", 0, 0)).await?;
    eventually(Duration::from_secs(2), || {
        count_role(&admin, Role::VpnListener) == 1
    })
    .await
    .expect("the legacy listener never registered");

    proxy.set_blackhole(true);
    time::sleep(FLOOR * 3).await;
    assert_eq!(
        count_role(&admin, Role::VpnListener),
        1,
        "a legacy listener must never be transport-reaped"
    );
    Ok(())
}

// ─── Paired 1:1 link ─────────────────────────────────────────────────────────

/// Pair `listener` and `connector` on `id`, both already connected; returns
/// once both received `VpnReady`.
async fn pair(
    listener: &mut Delimited<bore_cli::mux::Stream>,
    listener_ms: u32,
    connector: &mut Delimited<bore_cli::mux::Stream>,
    connector_ms: u32,
    id: &str,
) -> Result<()> {
    listener.send(hello(id, 0, listener_ms)).await?;
    time::sleep(Duration::from_millis(200)).await;
    connector.send(connect(id, connector_ms)).await?;
    let wait = Duration::from_secs(3);
    ready_flag(next_non_heartbeat(connector, wait).await, "connector");
    ready_flag(next_non_heartbeat(listener, wait).await, "listener");
    Ok(())
}

/// A paired declared listener whose path dies is reaped on the server.
///
/// RED-CHECK: replacing the paired loop's `reap_if_due(&transport)` with
/// `None` keeps its admin entry for the whole test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_paired_listener_is_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18905;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut listener = ctrl(proxy.addr).await?;
    let mut connector = ctrl(control_addr(CONTROL)).await?;
    pair(&mut listener, DECLARED_MS, &mut connector, 0, "paired-l").await?;
    assert_eq!(count_role(&admin, Role::VpnListener), 1);

    proxy.set_blackhole(true);
    eventually(REAP_BUDGET, || count_role(&admin, Role::VpnListener) == 0)
        .await
        .expect("a paired listener whose path died was never reaped");
    Ok(())
}

/// A paired declared connector whose path dies is reaped on the server.
///
/// RED-CHECK: replacing the 1:1 connector loop's `reap_if_due(&transport)`
/// with `None` keeps its admin entry for the whole test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_paired_connector_is_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18906;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut listener = ctrl(control_addr(CONTROL)).await?;
    let mut connector = ctrl(proxy.addr).await?;
    pair(&mut listener, 0, &mut connector, DECLARED_MS, "paired-c").await?;
    assert_eq!(count_role(&admin, Role::VpnConnector), 1);

    proxy.set_blackhole(true);
    eventually(REAP_BUDGET, || count_role(&admin, Role::VpnConnector) == 0)
        .await
        .expect("a paired connector whose path died was never reaped");
    Ok(())
}

// ─── Hub ─────────────────────────────────────────────────────────────────────

/// A declared hub listener is told it is reaped, and is reaped when its path
/// dies.
///
/// RED-CHECK: replacing the hub loop's `reap_if_due(&transport)` with `None`
/// keeps its admin entry for the whole test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_hub_listener_is_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18907;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut hub = ctrl(proxy.addr).await?;
    hub.send(hello("hub-l", 4, DECLARED_MS)).await?;
    assert!(ready_flag(
        next_non_heartbeat(&mut hub, Duration::from_secs(3)).await,
        "hub"
    ));
    assert_eq!(count_role(&admin, Role::VpnListener), 1);

    proxy.set_blackhole(true);
    eventually(REAP_BUDGET, || count_role(&admin, Role::VpnListener) == 0)
        .await
        .expect("a hub listener whose path died was never reaped");
    Ok(())
}

/// A declared hub spoke is told it is reaped, and is reaped when its path
/// dies; the legacy hub that hears nothing from it is not.
///
/// RED-CHECK: replacing the spoke loop's `reap_if_due(&transport)` with
/// `None` keeps its admin entry for the whole test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declared_hub_spoke_is_transport_reaped() -> Result<()> {
    const CONTROL: u16 = 18908;
    let admin = spawn_server(CONTROL).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;

    let mut hub = ctrl(control_addr(CONTROL)).await?;
    hub.send(hello("hub-s", 4, 0)).await?;
    assert!(!ready_flag(
        next_non_heartbeat(&mut hub, Duration::from_secs(3)).await,
        "hub"
    ));
    let mut spoke = ctrl(proxy.addr).await?;
    spoke.send(connect("hub-s", DECLARED_MS)).await?;
    assert!(ready_flag(
        next_non_heartbeat(&mut spoke, Duration::from_secs(3)).await,
        "spoke"
    ));
    assert_eq!(count_role(&admin, Role::VpnConnector), 1);

    proxy.set_blackhole(true);
    eventually(REAP_BUDGET, || count_role(&admin, Role::VpnConnector) == 0)
        .await
        .expect("a hub spoke whose path died was never reaped");
    assert_eq!(
        count_role(&admin, Role::VpnListener),
        1,
        "the legacy hub must outlive its reaped spoke"
    );
    Ok(())
}
