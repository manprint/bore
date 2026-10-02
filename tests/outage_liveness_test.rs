//! Outage recovery (plan `005_plan-OutageRecovery`).
//!
//! Field report: after a ~20 s network outage (most likely an ISP IP change)
//! a `bore vhost --auto-reconnect` client stayed down ~16 minutes. Neither end
//! noticed that the control connection was dead until the kernel gave up on
//! it (`tcp_retries2`, ≈924 s), and the server kept the dead connection's
//! subdomain until then.
//!
//! These tests drive the real client and server through [`BlackholeProxy`], a
//! TCP forwarder that can stop forwarding in both directions while keeping
//! both sockets ESTABLISHED — exactly what a dead path looks like to the two
//! endpoints: no FIN, no RST, just silence.
//!
//! Every test owns its own ports (`#[tokio::test]` runtimes release their
//! listeners only after the body returned) and holds `SERIAL` because the
//! liveness knobs are process-wide environment variables.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    admin::Role,
    client::{Client, ProviderMeta},
    mux,
    secret::Proxy,
    server::Server,
    shared::{ClientMessage, Delimited, ServerMessage, TunnelOptions},
    vhost::{VhostConfig, VhostModeCfg},
};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{self, Instant};

lazy_static! {
    /// The liveness knobs are environment variables read per call, so tests
    /// that set them must not overlap.
    static ref SERIAL: Mutex<()> = Mutex::new(());
}

const PORT_WAIT_BUDGET: Duration = Duration::from_secs(30);

// ─── Environment ─────────────────────────────────────────────────────────────

/// Sets the liveness environment for one test and clears it on drop.
struct LivenessEnv;

impl LivenessEnv {
    /// `heartbeat_ms`: the client's beat; `silence_ms`: the client's
    /// server-silence deadline (`0` disables it).
    fn set(heartbeat_ms: u64, silence_ms: u64) -> Self {
        std::env::set_var("BORE_CTRL_HEARTBEAT_MS", heartbeat_ms.to_string());
        std::env::set_var("BORE_CTRL_SERVER_SILENCE_MS", silence_ms.to_string());
        Self
    }
}

impl Drop for LivenessEnv {
    fn drop(&mut self) {
        std::env::remove_var("BORE_CTRL_HEARTBEAT_MS");
        std::env::remove_var("BORE_CTRL_SERVER_SILENCE_MS");
    }
}

// ─── Blackhole proxy ─────────────────────────────────────────────────────────

/// One proxied connection's observable state.
#[derive(Default)]
struct ProxiedConn {
    /// The client side of this connection reached EOF (the client closed it).
    client_closed: AtomicBool,
    /// The server side of this connection reached EOF (the server closed it).
    server_closed: AtomicBool,
    /// Blackhole THIS connection only (the proxy-wide flag covers them all).
    blackholed: AtomicBool,
}

impl ProxiedConn {
    fn set_blackhole(&self, on: bool) {
        self.blackholed.store(on, Ordering::SeqCst);
    }
}

/// A TCP forwarder `client <-> proxy <-> upstream` that can be blackholed:
/// while blackholed it forwards nothing in either direction and closes
/// nothing, so both endpoints see a live socket that has gone silent. Bytes
/// already read are held, not dropped, and delivered on release — the same
/// thing TCP retransmission does once a flick ends.
struct BlackholeProxy {
    addr: SocketAddr,
    blackholed: Arc<AtomicBool>,
    conns: Arc<std::sync::Mutex<Vec<Arc<ProxiedConn>>>>,
}

impl BlackholeProxy {
    async fn start(upstream: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let blackholed = Arc::new(AtomicBool::new(false));
        let conns: Arc<std::sync::Mutex<Vec<Arc<ProxiedConn>>>> = Arc::default();
        let (bh, cs) = (blackholed.clone(), conns.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(upstream).await else {
                    continue;
                };
                let state = Arc::new(ProxiedConn::default());
                cs.lock().unwrap().push(state.clone());
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                tokio::spawn(pump(cr, sw, bh.clone(), state.clone(), true));
                tokio::spawn(pump(sr, cw, bh.clone(), state, false));
            }
        });
        Ok(Self {
            addr,
            blackholed,
            conns,
        })
    }

    fn to(&self) -> String {
        self.addr.to_string()
    }

    fn set_blackhole(&self, on: bool) {
        self.blackholed.store(on, Ordering::SeqCst);
    }

    fn conns(&self) -> Vec<Arc<ProxiedConn>> {
        self.conns.lock().unwrap().clone()
    }
}

async fn wait_released(blackholed: &AtomicBool, conn: &ProxiedConn) {
    while blackholed.load(Ordering::SeqCst) || conn.blackholed.load(Ordering::SeqCst) {
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    blackholed: Arc<AtomicBool>,
    state: Arc<ProxiedConn>,
    from_client: bool,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        wait_released(&blackholed, &state).await;
        let n = from.read(&mut buf).await.unwrap_or_default();
        if n == 0 {
            if from_client {
                state.client_closed.store(true, Ordering::SeqCst);
            } else {
                state.server_closed.store(true, Ordering::SeqCst);
            }
            let _ = to.shutdown().await;
            return;
        }
        // A read that completed just as the blackhole went up is held.
        wait_released(&blackholed, &state).await;
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

// ─── Servers and helpers ─────────────────────────────────────────────────────

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

fn http_vhost(http_port: u16) -> VhostConfig {
    VhostConfig {
        base_domain: "outage.test".to_string(),
        mode: VhostModeCfg::Http,
        http_port,
        https_port: 443,
        cert_file: None,
        key_file: None,
        default_headers: Default::default(),
        default_response_headers: Default::default(),
        reservations: vec![],
    }
}

/// A server on `control` whose public range is `public`, with the transport
/// reap floor lowered to `floor`. Returns its admin registry.
async fn spawn_server(
    control: u16,
    public: std::ops::RangeInclusive<u16>,
    floor: Duration,
    vhost_http: Option<u16>,
) -> Result<bore_cli::admin::AdminRegistry> {
    wait_port(control, false).await;
    let mut server = Server::new(public, None).transport_reap_floor(floor);
    server.set_control_port(control);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    if let Some(http) = vhost_http {
        server.set_vhost(http_vhost(http))?;
    }
    let admin = server.admin_registry();
    tokio::spawn(server.listen());
    wait_port(control, true).await;
    Ok(admin)
}

fn control_addr(control: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], control))
}

async fn echo_service() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = conn.write_all(b"alive").await;
            });
        }
    });
    Ok(port)
}

async fn vhost_provider(to: &str, local: u16, label: &str) -> Result<Client> {
    Client::new_vhost_provider(
        "127.0.0.1",
        local,
        to,
        label,
        "client",
        None,
        false,
        1,
        ProviderMeta::default(),
        None,
    )
    .await
}

async fn secret_provider(to: &str, local: u16, id: &str) -> Result<Client> {
    Client::new_secret_provider(
        "127.0.0.1",
        local,
        to,
        id,
        None,
        false,
        false,
        None,
        Default::default(),
        0,
        0,
        64,
        1,
        ProviderMeta::default(),
        None,
    )
    .await
}

async fn secret_consumer(to: &str, id: &str) -> Result<Proxy> {
    secret_consumer_with_carriers(to, id, 1).await
}

async fn secret_consumer_with_carriers(to: &str, id: &str, carriers: u16) -> Result<Proxy> {
    Proxy::new(
        to,
        "127.0.0.1:0".parse()?,
        id,
        None,
        false,
        false,
        None,
        Default::default(),
        0,
        0,
        carriers,
        None,
        false,
    )
    .await
}

fn count_role(admin: &bore_cli::admin::AdminRegistry, role: Role) -> usize {
    admin.snapshot().iter().filter(|e| e.role == role).count()
}

/// Poll `attempt` until it succeeds or `within` elapses; returns the time taken.
async fn eventually<F, Fut, T>(within: Duration, mut attempt: F) -> Option<(Duration, T)>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let start = Instant::now();
    loop {
        if let Some(v) = attempt().await {
            return Some((start.elapsed(), v));
        }
        if start.elapsed() >= within {
            return None;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

// ─── Server transport reapers (1.2) ──────────────────────────────────────────

/// The field case. A vhost provider whose path dies must lose its subdomain
/// within the transport deadline, so its reconnect is not refused "in use".
///
/// RED-CHECK: without the reaper in `serve_vhost_provider` the second
/// registration is refused for the whole test (the legacy 60 s reaper does not
/// fire inside it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_transport_reaps_a_silent_declared_vhost_provider() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 0); // the client must not trip: isolate the server
    const CONTROL: u16 = 18601;
    const HTTP: u16 = 18602;
    spawn_server(CONTROL, 18603..=18603, Duration::from_secs(1), Some(HTTP)).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let first = vhost_provider(&proxy.to(), local, "field").await?;
    tokio::spawn(first.listen());
    // Healthy: the label is held and a second registration is refused.
    time::sleep(Duration::from_millis(700)).await;
    assert!(
        vhost_provider(&format!("127.0.0.1:{CONTROL}"), local, "field")
            .await
            .is_err(),
        "a live provider must keep its subdomain"
    );

    proxy.set_blackhole(true);
    let freed = eventually(Duration::from_secs(4), || async {
        vhost_provider(&format!("127.0.0.1:{CONTROL}"), local, "field")
            .await
            .ok()
    })
    .await;
    let (took, second) = freed.expect(
        "the subdomain of a provider whose path died was never released; \
         its reconnect would be refused until the kernel gives up (~15 min)",
    );
    assert!(took < Duration::from_secs(4), "took {took:?}");
    drop(second);
    Ok(())
}

/// Same shape for a public tunnel on a fixed port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_transport_reaps_a_silent_declared_public_tunnel() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 0);
    const CONTROL: u16 = 18611;
    const PORT: u16 = 18612;
    spawn_server(CONTROL, PORT..=PORT, Duration::from_secs(1), None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let first = Client::new(
        "127.0.0.1",
        local,
        &proxy.to(),
        PORT,
        None,
        false,
        TunnelOptions::default(),
        None,
    )
    .await?;
    assert_eq!(first.remote_port(), PORT);
    tokio::spawn(first.listen());
    wait_port(PORT, true).await;

    proxy.set_blackhole(true);
    let freed = eventually(Duration::from_secs(4), || async {
        Client::new(
            "127.0.0.1",
            local,
            &format!("127.0.0.1:{CONTROL}"),
            PORT,
            None,
            false,
            TunnelOptions::default(),
            None,
        )
        .await
        .ok()
    })
    .await;
    assert!(
        freed.is_some(),
        "the public port of a client whose path died was never released"
    );
    Ok(())
}

/// Same shape for a secret provider id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_transport_reaps_a_silent_declared_secret_provider() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 0);
    const CONTROL: u16 = 18621;
    spawn_server(CONTROL, 18622..=18622, Duration::from_secs(1), None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let first = secret_provider(&proxy.to(), local, "db").await?;
    tokio::spawn(first.listen());
    time::sleep(Duration::from_millis(500)).await;
    assert!(
        secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db")
            .await
            .is_err(),
        "a live provider must keep its id"
    );

    proxy.set_blackhole(true);
    let freed = eventually(Duration::from_secs(4), || async {
        secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db")
            .await
            .ok()
    })
    .await;
    assert!(
        freed.is_some(),
        "the id of a secret provider whose path died was never released"
    );
    Ok(())
}

/// A secret consumer has no name to hold, but its admin row is a zombie
/// until it is reaped (the inflated "Secret Tunnels" count).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_transport_reaps_a_silent_declared_secret_consumer() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 0);
    const CONTROL: u16 = 18631;
    let admin = spawn_server(CONTROL, 18632..=18632, Duration::from_secs(1), None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db").await?;
    tokio::spawn(provider.listen());
    let consumer = secret_consumer(&proxy.to(), "db").await?;
    tokio::spawn(consumer.listen());
    let seen = eventually(Duration::from_secs(3), || async {
        (count_role(&admin, Role::SecretConsumer) == 1).then_some(())
    })
    .await;
    assert!(seen.is_some(), "the consumer never registered");

    proxy.set_blackhole(true);
    let gone = eventually(Duration::from_secs(4), || async {
        (count_role(&admin, Role::SecretConsumer) == 0).then_some(())
    })
    .await;
    assert!(
        gone.is_some(),
        "the admin row of a consumer whose path died was never reaped"
    );
    assert_eq!(
        count_role(&admin, Role::SecretProvider),
        1,
        "the healthy provider is untouched"
    );
    Ok(())
}

/// I-5 (DEC-VE2's shape): a client that declared NO interval — every client
/// built before plan 005 — is never transport-reaped, however long it is
/// silent. Reaping it would kill healthy idle legacy tunnels.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn undeclared_client_is_never_transport_reaped() -> Result<()> {
    let _serial = SERIAL.lock().await;
    const CONTROL: u16 = 18641;
    const PORT: u16 = 18642;
    spawn_server(CONTROL, PORT..=PORT, Duration::from_secs(1), None).await?;

    // A legacy client, by hand: no `ctrl_heartbeat`, no `ctrl_heartbeat_ms`,
    // and it never sends another byte.
    let tcp = TcpStream::connect(("127.0.0.1", CONTROL)).await?;
    let (opener, _acceptor) = mux::client(tcp);
    let mut control = Delimited::new(opener.open().await?);
    let legacy: TunnelOptions = serde_json::from_str(
        r#"{"https":false,"force_https":false,"basic_auth":null,"notes":null}"#,
    )?;
    control.send(ClientMessage::Hello(PORT, legacy)).await?;
    let reply = control.recv::<ServerMessage>().await?;
    assert!(matches!(reply, Some(ServerMessage::Hello(p)) if p == PORT));

    time::sleep(Duration::from_secs(3)).await;

    let tcp2 = TcpStream::connect(("127.0.0.1", CONTROL)).await?;
    let (opener2, _acceptor2) = mux::client(tcp2);
    let mut control2 = Delimited::new(opener2.open().await?);
    control2
        .send(ClientMessage::Hello(PORT, TunnelOptions::default()))
        .await?;
    let busy = control2.recv::<ServerMessage>().await?;
    assert!(
        matches!(busy, Some(ServerMessage::Error(_))),
        "an undeclared client must keep its port: {busy:?}"
    );
    drop((opener, control));
    Ok(())
}

// ─── Client server-silence deadline (1.3) ────────────────────────────────────

/// Server reap floor for the client-side tests: far beyond every test's
/// horizon, so what is observed is the CLIENT noticing, never the server.
const SERVER_NEVER_REAPS: Duration = Duration::from_secs(600);

/// Run `listen` healthy for longer than the deadline (a healthy client must
/// never trip), then blackhole the path and require `listen` to return the
/// server-silence error well before the kernel would have (≈15 min).
async fn assert_listen_trips<F>(proxy: &BlackholeProxy, listen: F, deadline: Duration)
where
    F: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let task = tokio::spawn(listen);
    time::sleep(deadline + Duration::from_millis(500)).await;
    assert!(
        !task.is_finished(),
        "a healthy client tripped its server-silence deadline"
    );

    proxy.set_blackhole(true);
    let start = Instant::now();
    let joined = time::timeout(Duration::from_secs(6), task)
        .await
        .expect("listen never returned: the dead path was not noticed (the production symptom)");
    let took = start.elapsed();
    let err = joined
        .expect("listen panicked")
        .expect_err("a lost connection must be an error, so --auto-reconnect logs why");
    assert!(
        err.to_string().contains("silent"),
        "unexpected error: {err:#}"
    );
    assert!(
        took < deadline + Duration::from_millis(2500),
        "took {took:?} for a {deadline:?} deadline"
    );
}

/// The field case, public flavour: the path dies, the client notices within
/// its deadline and returns, so `--auto-reconnect` reconnects.
///
/// RED-CHECK: without the liveness arm in `Client::listen` this times out —
/// `listen` would have waited for the kernel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_client_returns_when_the_server_goes_silent() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18651;
    const PORT: u16 = 18652;
    spawn_server(CONTROL, PORT..=PORT, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let client = Client::new(
        "127.0.0.1",
        local,
        &proxy.to(),
        PORT,
        None,
        false,
        TunnelOptions::default(),
        None,
    )
    .await?;
    assert_listen_trips(&proxy, client.listen(), Duration::from_millis(1500)).await;
    Ok(())
}

/// Same for a vhost provider (the exact field configuration, minus docker).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vhost_client_returns_when_the_server_goes_silent() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18661;
    const HTTP: u16 = 18662;
    spawn_server(CONTROL, 18663..=18663, SERVER_NEVER_REAPS, Some(HTTP)).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let client = vhost_provider(&proxy.to(), local, "field").await?;
    assert_listen_trips(&proxy, client.listen(), Duration::from_millis(1500)).await;
    Ok(())
}

/// Same for a secret provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_provider_client_returns_when_the_server_goes_silent() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18671;
    spawn_server(CONTROL, 18672..=18672, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let client = secret_provider(&proxy.to(), local, "db").await?;
    assert_listen_trips(&proxy, client.listen(), Duration::from_millis(1500)).await;
    Ok(())
}

/// Read the echo service's reply through the public port.
async fn public_request(port: u16) -> Option<Vec<u8>> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let mut reply = Vec::new();
    time::timeout(Duration::from_secs(2), conn.read_to_end(&mut reply))
        .await
        .ok()?
        .ok()?;
    (reply == b"alive").then_some(reply)
}

/// I-3: a flick shorter than the deadline costs NOTHING — no disconnection,
/// no reconnect, and the tunnel serves again as soon as the path is back.
///
/// The flick is most of the deadline on purpose (2.5 s of 4 s, plus up to one
/// 500 ms server-beat gap): a check that fired early — against the tick period,
/// say — must trip here, not pass by luck.
///
/// RED-CHECK: comparing against `deadline / 4` disconnects the client.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_survives_a_short_flick() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 4000);
    const CONTROL: u16 = 18681;
    const PORT: u16 = 18682;
    spawn_server(CONTROL, PORT..=PORT, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let client = Client::new(
        "127.0.0.1",
        local,
        &proxy.to(),
        PORT,
        None,
        false,
        TunnelOptions::default(),
        None,
    )
    .await?;
    let task = tokio::spawn(client.listen());
    assert!(
        public_request(PORT).await.is_some(),
        "healthy tunnel serves"
    );

    proxy.set_blackhole(true);
    time::sleep(Duration::from_millis(2500)).await;
    proxy.set_blackhole(false);

    time::sleep(Duration::from_secs(4)).await;
    assert!(
        !task.is_finished(),
        "a 2.5 s flick against a 4 s deadline disconnected the client: {:?}",
        task.await
    );
    assert!(
        public_request(PORT).await.is_some(),
        "the tunnel does not serve after the flick"
    );
    assert_eq!(
        proxy.conns().len(),
        1,
        "the client must not have reconnected"
    );
    Ok(())
}

/// A trip takes the carrier connections down with the main one. Carriers
/// carry no heartbeat of their own, so nothing else would ever notice that
/// their path died, and each would hold its proxied connections hanging.
///
/// RED-CHECK: without `terminate_all` the carrier connection is still open
/// after the main one is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_terminates_carrier_connections_on_trip() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18691;
    const PORT: u16 = 18692;
    spawn_server(CONTROL, PORT..=PORT, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let client = Client::new(
        "127.0.0.1",
        local,
        &proxy.to(),
        PORT,
        None,
        false,
        TunnelOptions {
            carriers: 2,
            ..Default::default()
        },
        None,
    )
    .await?;
    let both = eventually(Duration::from_secs(3), || async {
        (proxy.conns().len() == 2).then_some(())
    })
    .await;
    assert!(both.is_some(), "main + one carrier through the proxy");
    assert_listen_trips(&proxy, client.listen(), Duration::from_millis(1500)).await;

    // Hold the blackhole a little longer: the server must not be the one that
    // closes anything (it never reaps in this test, and it cannot see the
    // client's FIN until the path is back).
    time::sleep(Duration::from_millis(300)).await;
    proxy.set_blackhole(false);
    let closed = eventually(Duration::from_secs(3), || async {
        proxy
            .conns()
            .iter()
            .all(|c| c.client_closed.load(Ordering::SeqCst))
            .then_some(())
    })
    .await;
    assert!(
        closed.is_some(),
        "the client left a carrier connection open after its main one tripped"
    );
    Ok(())
}

// ─── Secret consumer (1.4) ───────────────────────────────────────────────────

/// One request through a consumer's local port, answered by the echo service
/// behind the provider.
async fn consumer_request(local: SocketAddr) -> Option<Vec<u8>> {
    let mut conn = TcpStream::connect(local).await.ok()?;
    let mut reply = Vec::new();
    time::timeout(Duration::from_secs(2), conn.read_to_end(&mut reply))
        .await
        .ok()?
        .ok()?;
    (reply == b"alive").then_some(reply)
}

/// The consumer (`bore proxy`) on the relay path notices the dead path too.
///
/// RED-CHECK: without the liveness arm in `Proxy::listen` this times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_consumer_returns_when_the_server_goes_silent() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18701;
    spawn_server(CONTROL, 18702..=18702, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db").await?;
    tokio::spawn(provider.listen());
    let consumer = secret_consumer(&proxy.to(), "db").await?;
    assert_listen_trips(&proxy, consumer.listen(), Duration::from_millis(1500)).await;
    Ok(())
}

/// A trip takes the consumer's relay carriers down with its main connection.
///
/// RED-CHECK: without `terminate_all` (and with the per-carrier watch below
/// disabled) a carrier connection stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_consumer_terminates_its_carriers_on_trip() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18711;
    spawn_server(CONTROL, 18712..=18712, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db").await?;
    tokio::spawn(provider.listen());
    let consumer = secret_consumer_with_carriers(&proxy.to(), "db", 2).await?;
    assert_eq!(
        proxy.conns().len(),
        2,
        "main + one carrier through the proxy"
    );
    assert_listen_trips(&proxy, consumer.listen(), Duration::from_millis(1500)).await;

    time::sleep(Duration::from_millis(300)).await;
    proxy.set_blackhole(false);
    let closed = eventually(Duration::from_secs(3), || async {
        proxy
            .conns()
            .iter()
            .all(|c| c.client_closed.load(Ordering::SeqCst))
            .then_some(())
    })
    .await;
    assert!(
        closed.is_some(),
        "the consumer left a carrier connection open after its main one tripped"
    );
    Ok(())
}

/// A carrier whose OWN path dies while the main connection stays healthy
/// leaves the pool, so connections are not routed into it. Every consumer
/// carrier is heartbeated by the server, so each one can tell.
///
/// RED-CHECK: without the per-carrier watch, requests routed to the dead
/// carrier hang and the carrier is never closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_consumer_drops_a_silent_carrier_and_keeps_serving() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18721;
    spawn_server(CONTROL, 18722..=18722, SERVER_NEVER_REAPS, None).await?;
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db").await?;
    tokio::spawn(provider.listen());
    let consumer = secret_consumer_with_carriers(&proxy.to(), "db", 2).await?;
    let bound = consumer.local_addr()?;
    let conns = proxy.conns();
    assert_eq!(conns.len(), 2, "main + one carrier through the proxy");
    let task = tokio::spawn(consumer.listen());

    // Kill the carrier's path only, and wait past its deadline.
    conns[1].set_blackhole(true);
    time::sleep(Duration::from_millis(1500 + 1500)).await;
    for i in 0..4 {
        assert!(
            consumer_request(bound).await.is_some(),
            "request {i} was routed into the dead carrier"
        );
    }
    assert!(!task.is_finished(), "the healthy main connection tripped");

    conns[1].set_blackhole(false);
    let closed = eventually(Duration::from_secs(3), || async {
        conns[1].client_closed.load(Ordering::SeqCst).then_some(())
    })
    .await;
    assert!(closed.is_some(), "the silent carrier was never dropped");
    assert!(
        !conns[0].client_closed.load(Ordering::SeqCst),
        "the main connection must stay up"
    );
    Ok(())
}

/// The consumer's heartbeats (now written through the bounded `beat_once`)
/// still reach the server: with the legacy control-message reaper lowered to
/// 1 s, a consumer that beats every 200 ms keeps its admin row.
///
/// RED-CHECK: a consumer that does not beat is reaped inside the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_consumer_heartbeats_reach_the_server() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 0);
    const CONTROL: u16 = 18731;
    wait_port(CONTROL, false).await;
    let mut server = Server::new(18732..=18732, None).secret_ctrl_timeout(Duration::from_secs(1));
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    let admin = server.admin_registry();
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    let local = echo_service().await?;

    let provider = secret_provider(&format!("127.0.0.1:{CONTROL}"), local, "db").await?;
    tokio::spawn(provider.listen());
    let consumer = secret_consumer(&format!("127.0.0.1:{CONTROL}"), "db").await?;
    let task = tokio::spawn(consumer.listen());
    time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        count_role(&admin, Role::SecretConsumer),
        1,
        "a beating consumer was reaped: its heartbeats do not reach the server"
    );
    assert!(!task.is_finished());
    Ok(())
}

/// A consumer on the DIRECT path keeps serving through a server outage. The
/// direct path runs consumer<->provider and does not need the server, so
/// tripping on server silence there would turn a server outage into a tunnel
/// outage that did not exist before. The consumer reconnects only when the
/// direct path itself closes.
///
/// RED-CHECK: tripping regardless of the path ends `listen` and closes the
/// consumer's local port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_consumer_on_direct_survives_a_server_outage() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18741;
    wait_port(CONTROL, false).await;
    let mut server = Server::new(18742..=18742, None).transport_reap_floor(SERVER_NEVER_REAPS);
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    let stun = format!("127.0.0.1:{CONTROL}");
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = Client::new_secret_provider(
        "127.0.0.1",
        local,
        &format!("127.0.0.1:{CONTROL}"),
        "p2p",
        None,
        false,
        true,
        Some(&stun),
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        64,
        1,
        ProviderMeta::default(),
        None,
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(300)).await;

    let consumer = Proxy::new(
        &proxy.to(),
        "127.0.0.1:0".parse()?,
        "p2p",
        None,
        false,
        true,
        Some(&stun),
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        1,
        None,
        false,
    )
    .await?;
    assert!(
        consumer.is_direct(),
        "the consumer must negotiate the direct path"
    );
    let bound = consumer.local_addr()?;
    let task = tokio::spawn(consumer.listen());
    assert!(
        consumer_request(bound).await.is_some(),
        "healthy direct path serves"
    );

    // The server becomes unreachable for the consumer only.
    proxy.set_blackhole(true);
    time::sleep(Duration::from_millis(1500 + 1500)).await;
    assert!(
        !task.is_finished(),
        "a server outage tore down a working direct path: {:?}",
        task.await
    );
    for i in 0..3 {
        assert!(
            consumer_request(bound).await.is_some(),
            "request {i} failed although the direct path is alive"
        );
    }
    Ok(())
}

/// The provider's half of the same rule. A provider that loses the server
/// trips and reconnects (it must, to be found by NEW consumers), but the
/// direct path it already serves is not tied to its control connection, so a
/// consumer already on it keeps being served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_provider_trip_keeps_its_live_direct_path() -> Result<()> {
    let _serial = SERIAL.lock().await;
    let _env = LivenessEnv::set(200, 1500);
    const CONTROL: u16 = 18751;
    wait_port(CONTROL, false).await;
    let mut server = Server::new(18752..=18752, None).transport_reap_floor(SERVER_NEVER_REAPS);
    server.set_control_port(CONTROL);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_udp(true);
    tokio::spawn(server.listen());
    wait_port(CONTROL, true).await;
    let stun = format!("127.0.0.1:{CONTROL}");
    let proxy = BlackholeProxy::start(control_addr(CONTROL)).await?;
    let local = echo_service().await?;

    let provider = Client::new_secret_provider(
        "127.0.0.1",
        local,
        &proxy.to(),
        "p2p",
        None,
        false,
        true,
        Some(&stun),
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        64,
        1,
        ProviderMeta::default(),
        None,
    )
    .await?;
    let provider_task = tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(300)).await;

    let consumer = Proxy::new(
        &format!("127.0.0.1:{CONTROL}"),
        "127.0.0.1:0".parse()?,
        "p2p",
        None,
        false,
        true,
        Some(&stun),
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        1,
        None,
        false,
    )
    .await?;
    assert!(
        consumer.is_direct(),
        "the consumer must negotiate the direct path"
    );
    let bound = consumer.local_addr()?;
    tokio::spawn(consumer.listen());
    assert!(
        consumer_request(bound).await.is_some(),
        "healthy direct path serves"
    );

    proxy.set_blackhole(true);
    let joined = time::timeout(Duration::from_secs(6), provider_task)
        .await
        .expect("the provider never noticed the dead path");
    assert!(
        joined.expect("provider panicked").is_err(),
        "the provider must report the lost connection"
    );
    for i in 0..3 {
        assert!(
            consumer_request(bound).await.is_some(),
            "request {i} failed: the provider's trip took its live direct path down"
        );
    }
    Ok(())
}
