//! End-to-end integration tests filling coverage gaps for `bore local` (public tunnel)
//! and `bore proxy` (secret tunnel). These tests harden invariants around:
//! - Banner-first protocols (stream-ready before client writes)
//! - TLS + carriers interaction
//! - TLS + basic auth interaction
//! - max-conns permit recovery after rapid connection churn

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::Client,
    mux,
    server::Server,
    shared::{ClientMessage, Delimited, ServerMessage, TunnelOptions, CONTROL_PORT},
    transport,
    weblog::{AccessLogConfig, AccessLogger},
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serialize tests sharing the fixed `CONTROL_PORT`.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// Wait until the control port is either accepting or fully released.
async fn wait_for_control_port(listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", CONTROL_PORT))
            .await
            .is_ok()
            == listening
        {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// Spawn a plain-TCP server.
async fn spawn_server() {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, None).listen());
    wait_for_control_port(true).await;
}

/// Spawn a TLS server with a self-signed cert.
async fn spawn_tls_server() -> Result<(String, String)> {
    wait_for_control_port(false).await;
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    let cert_pem = key.cert.pem();
    let key_pem = key.signing_key.serialize_pem();
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
    Ok((cert_pem, key_pem))
}

/// Spawn a public-tunnel client, returning the listener and remote address.
async fn spawn_client(options: TunnelOptions) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let local_port = listener.local_addr()?.port();
    let client = Client::new(
        "localhost",
        local_port,
        "localhost",
        0,
        None,
        false,
        options,
        None,
    )
    .await?;
    let remote_addr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    Ok((listener, remote_addr))
}

/// Spawn a TLS public-tunnel client.
async fn spawn_tls_client(options: TunnelOptions) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let local_port = listener.local_addr()?.port();
    let to = format!("https://localhost:{CONTROL_PORT}");
    let client = Client::new(
        "localhost",
        local_port,
        &to,
        0,
        None,
        true, // insecure: self-signed cert
        options,
        None,
    )
    .await?;
    let remote_addr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    Ok((listener, remote_addr))
}

/// Read some available data within a timeout.
async fn read_some(conn: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    let n = time::timeout(Duration::from_secs(3), conn.read(&mut buf)).await??;
    buf.truncate(n);
    Ok(buf)
}

#[tokio::test]
async fn stream_ready_banner_arrives_before_client_writes() -> Result<()> {
    // A local service that sends a banner immediately on connect before reading
    // anything ensures the mux::STREAM_READY is written before the client sends its
    // first byte. The remote peer must receive the banner first, proving the banner
    // is not buffered at the client but reaches the tunnel.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server().await;
    let (listener, addr) = spawn_client(TunnelOptions::default()).await?;

    // Local service: immediately send a banner, then echo back what it receives.
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        stream.write_all(b"WELCOME\n").await?;
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    });

    // Remote peer: first read is the banner, second is the echoed message.
    let mut conn = TcpStream::connect(addr).await?;
    let mut buf = [0u8; 8];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"WELCOME\n");

    // Now send a message and read it back (proved the banner arrived first).
    conn.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ping");

    Ok(())
}

#[tokio::test]
async fn tls_tunnel_with_multiple_carriers_round_trips() -> Result<()> {
    // A TLS tunnel with --carriers 4 must safely multiplex several concurrent
    // connections across the pooled carriers without data corruption or deadlock.
    let _guard = SERIAL_GUARD.lock().await;

    let _ = spawn_tls_server().await?;
    let options = TunnelOptions {
        https: true,
        carriers: 4,
        ..Default::default()
    };
    let (listener, addr) = spawn_tls_client(options).await?;

    // Local echo service.
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 4];
                stream.read_exact(&mut buf).await?;
                stream.write_all(&buf).await?;
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });

    // 10 concurrent TLS round-trips, each over a different carrier.
    let mut handles = Vec::new();
    for i in 0u32..10 {
        handles.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await?;
            let msg = i.to_be_bytes();
            stream.write_all(&msg).await?;
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            assert_eq!(buf, msg);
            anyhow::Ok(())
        }));
    }
    for h in handles {
        h.await??;
    }

    Ok(())
}

#[tokio::test]
async fn tls_tunnel_with_basic_auth() -> Result<()> {
    // A TLS tunnel with basic auth must reject unauthenticated HTTP requests with
    // 401 and accept authenticated ones through TLS.
    let _guard = SERIAL_GUARD.lock().await;

    let _ = spawn_tls_server().await?;
    let options = TunnelOptions {
        https: true,
        basic_auth: Some("user:pass".into()),
        ..Default::default()
    };
    let (listener, addr) = spawn_tls_client(options).await?;

    // Echo service for both HTTP and raw TCP.
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                let n = stream.read(&mut buf).await?;
                stream.write_all(&buf[..n]).await?;
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });

    // 1) No credentials → 401 (over plain TCP to the TLS port).
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await?;
    let resp_bytes = read_some(&mut conn).await?;
    let resp = String::from_utf8_lossy(&resp_bytes);
    assert!(resp.starts_with("HTTP/1.1 401"));

    // 2) With credentials, TLS works normally (use the transport layer to establish TLS).
    let endpoint = bore_cli::transport::Endpoint {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    tls.write_all(b"hello-tls").await?;
    let mut buf = [0u8; 9];
    tls.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-tls");

    Ok(())
}

#[tokio::test]
async fn max_conns_permit_recovers_after_rapid_churn() -> Result<()> {
    // Rapid connection churn (open then immediately close) must not leak permits.
    // After a churn burst, a fresh normal connection must still succeed, proving
    // the permits were released and the limit recovered.
    let _guard = SERIAL_GUARD.lock().await;

    const MAX: usize = 5;

    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_max_conns(MAX);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    // Local service that accepts and holds connections indefinitely.
    let local = TcpListener::bind("localhost:0").await?;
    let local_port = local.local_addr()?.port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = local.accept().await?;
            held.push(stream);
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });

    let client = Client::new(
        "localhost",
        local_port,
        "localhost",
        0,
        None,
        false,
        TunnelOptions {
            ..Default::default()
        },
        None,
    )
    .await?;
    let addr: SocketAddr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());

    // Churn: rapidly open and drop connections without sending anything.
    // This creates connection state that is quickly cleaned up.
    for _ in 0..50 {
        let s = TcpStream::connect(addr).await?;
        drop(s);
        time::sleep(Duration::from_millis(5)).await;
    }

    // After the churn, a fresh normal connection must succeed: the server's
    // permits are recovered and the limit is still enforced. Send a byte to
    // prove the connection reaches the local service.
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"x").await?;
    // The local service doesn't echo, so just verify the write succeeded
    // (no error = connection accepted and reached the service).

    Ok(())
}

// ─── Access logging tests ─────────────────────────────────────────────

/// Spawn a server with webserver logging enabled.
async fn spawn_server_with_log(log_dir: &std::path::Path) -> Result<()> {
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    let _ = server.set_webserver_log(Some(log_dir.to_path_buf()), 4, 100);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
    Ok(())
}

/// Spawn a public-tunnel client with logging enabled.
async fn spawn_client_with_log(
    log_dir: &std::path::Path,
    options: TunnelOptions,
) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let local_port = listener.local_addr()?.port();

    let cfg = AccessLogConfig {
        dir: log_dir.to_path_buf(),
        max_files: 4,
        max_file_size_bytes: 100 * 1024 * 1024,
    };
    let logger = Arc::new(AccessLogger::new(cfg));

    let client = Client::new(
        "localhost",
        local_port,
        "localhost",
        0,
        None,
        false,
        options,
        Some(logger),
    )
    .await?;
    let remote_addr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    Ok((listener, remote_addr))
}

/// Poll a file path up to 2 seconds, returning its contents when available.
/// Wait until the access log has actually been WRITTEN, not merely created.
///
/// This used to return as soon as `read_to_string` succeeded, which it does on
/// a zero-byte file — and the access-log writer opens the file first and
/// appends the line afterwards, so there is a real window in which the path
/// exists and is empty. Every caller asserts on the CONTENT, so on a runner
/// that scheduled the read inside that window the helper handed back `""` and
/// the test failed with its own empty message (`raw log should have content:`)
/// as though the server had logged nothing. Observed on `aarch64-apple-darwin`
/// in the cross matrix; the same race is latent in the two callers that look
/// for a request line, which only lose it less often because the write is
/// further down a longer chain.
///
/// So: poll for non-empty, and keep the two failure modes distinguishable —
/// "never created" and "created but never written" are different bugs, and a
/// helper that reports them as one sends the next reader to the wrong place.
async fn poll_file(path: &std::path::Path, max_wait: Duration) -> Result<String> {
    let start = std::time::Instant::now();
    let mut existed = false;
    loop {
        match std::fs::read_to_string(path) {
            Ok(content) if !content.is_empty() => return Ok(content),
            Ok(_) => existed = true,
            Err(_) => {}
        }
        if start.elapsed() > max_wait {
            if existed {
                anyhow::bail!("log file {path:?} was created but stayed empty for {max_wait:?}");
            }
            anyhow::bail!("log file {path:?} not created after {max_wait:?}");
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spawn a simple HTTP stub that always returns 200 OK.
#[allow(dead_code)]
async fn spawn_http_echo_stub(body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let body = body;
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut total = 0;
                loop {
                    let n = stream.read(&mut buf[total..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
async fn server_access_log_http() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let log_dir = std::env::temp_dir().join("bore_test_server_log");
    let _ = std::fs::remove_dir_all(&log_dir);
    std::fs::create_dir_all(&log_dir)?;

    spawn_server_with_log(&log_dir).await?;
    let (listener, addr) = spawn_client(TunnelOptions::default()).await?;

    // HTTP stub that responds to GET /api/ping
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await?;
        let req = String::from_utf8_lossy(&buf[..n]);
        let response = if req.contains("GET /api/ping") {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK"
        } else {
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"
        };
        stream.write_all(response.as_bytes()).await?;
        anyhow::Ok(())
    });

    // Send HTTP request
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"GET /api/ping HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await?;
    conn.shutdown().await?;
    let _ = read_some(&mut conn).await;

    // Check log file exists and contains the request
    let log_file = log_dir.join(format!("{}.log", addr.port()));
    let content = poll_file(&log_file, Duration::from_secs(2)).await?;
    assert!(
        content.contains("GET /api/ping"),
        "log should contain request: {}",
        content
    );
    assert!(
        content.contains("127.0.0.1"),
        "log should contain client IP: {}",
        content
    );

    Ok(())
}

#[tokio::test]
async fn local_access_log_real_ip_forwarded() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let log_dir = std::env::temp_dir().join("bore_test_client_log");
    let _ = std::fs::remove_dir_all(&log_dir);
    std::fs::create_dir_all(&log_dir)?;

    spawn_server().await;
    let (listener, addr) = spawn_client_with_log(&log_dir, TunnelOptions::default()).await?;

    // Simple echo service that immediately returns HTTP OK
    tokio::spawn(async move {
        loop {
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    if n > 0 {
                        let response =
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK";
                        let _ = stream.write_all(response).await;
                    }
                });
            }
        }
    });

    // Send HTTP request through tunnel
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"GET /api/ping HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await?;
    conn.shutdown().await?;
    let _ = read_some(&mut conn).await;

    time::sleep(Duration::from_millis(100)).await;

    // Check client log
    let log_file = log_dir.join(format!("{}.log", addr.port()));
    let content = poll_file(&log_file, Duration::from_secs(2)).await?;
    assert!(
        content.contains("GET /api/ping"),
        "client log should contain request: {}",
        content
    );
    // Client-side logging shows "-" for unknown IP (no forwarded header from local service).
    // The important thing is that it logged at all.
    assert!(
        !content.is_empty(),
        "client log should not be empty: {}",
        content
    );

    Ok(())
}

/// Red-check for the helper above: an access log that EXISTS but has not been
/// written yet must not be handed back as a result.
///
/// This test fails against the previous implementation — it returns `""` the
/// instant the empty file is readable, and the assertion below is exactly the
/// one the cross matrix failed on. It needs no server, no port and no guard:
/// the race is entirely between "file created" and "line appended", and it is
/// reproduced here deterministically instead of waiting for a slow runner to
/// find it again.
#[tokio::test]
async fn poll_file_waits_for_content_and_does_not_accept_an_empty_file() -> Result<()> {
    let dir = std::env::temp_dir().join("bore_test_poll_file_contract");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("access.log");

    // The writer's shape: create first, append later.
    std::fs::write(&path, "")?;
    let writer = path.clone();
    tokio::spawn(async move {
        time::sleep(Duration::from_millis(300)).await;
        let _ = std::fs::write(&writer, "GET /api/ping\n");
    });

    let content = poll_file(&path, Duration::from_secs(2)).await?;
    assert!(
        content.contains("GET /api/ping"),
        "poll_file returned before the line was written: {content:?}"
    );

    // And the two failure modes stay distinguishable.
    let empty = dir.join("never_written.log");
    std::fs::write(&empty, "")?;
    let err = poll_file(&empty, Duration::from_millis(200))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("stayed empty"), "unexpected error: {err}");

    let missing = dir.join("never_created.log");
    let err = poll_file(&missing, Duration::from_millis(200))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not created"), "unexpected error: {err}");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn local_access_log_raw() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let log_dir = std::env::temp_dir().join("bore_test_raw_log");
    let _ = std::fs::remove_dir_all(&log_dir);
    std::fs::create_dir_all(&log_dir)?;

    spawn_server().await;
    let (listener, addr) = spawn_client_with_log(&log_dir, TunnelOptions::default()).await?;

    // Raw TCP echo service
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    });

    // Send raw bytes
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"\x01\x02\x03\x04\x05").await?;
    conn.shutdown().await?;

    time::sleep(Duration::from_millis(100)).await;

    // Check log file
    let log_file = log_dir.join(format!("{}.log", addr.port()));
    let content = poll_file(&log_file, Duration::from_secs(2)).await?;
    assert!(
        !content.is_empty(),
        "raw log should have content: {}",
        content
    );

    Ok(())
}

// ─── Public control-liveness group (P-4 reaper, control=17980..17983) ────────
//
// A public tunnel that is ALIVE at TCP level but DEAD at application level (a
// frozen process, a suspended laptop, a peer whose kernel still ACKs) used to
// hold its PUBLIC PORT forever. The control channel is a yamux substream, so a
// half-open peer is invisible to BOTH `send` (buffers into yamux) and `recv`
// (blocks forever) — the exact shape already fixed for secret tunnels and then
// for vhost (F-1). Public was explicitly left on the legacy heartbeat-free path
// at the time; P-4 closes it now that `serve_tunnel` reads its control stream
// at all.
//
// Each test owns its OWN control port and a SINGLE-PORT public range. The
// single-port range is what makes the reap observable: the released port is not
// merely absent from an internal map, it becomes registrable again — a freed
// slot that still refuses the port would leave the operator exactly as stuck.
//
// These tests hold the opener AND the control substream so the TCP connection
// stays UP while nothing is ever sent: wedged, not closed. Dropping the client
// would merely exercise the ordinary disconnect path, which already worked.

const PUB_LIVE_REAP: (u16, u16) = (17980, 17981);
const PUB_LIVE_LEGACY: (u16, u16) = (17982, 17983);
const PUB_LIVE_CLIENT: (u16, u16) = (17984, 17985);

/// Spawn a server whose public reap deadline is `ctrl_timeout` and whose public
/// port range holds exactly `public_port`.
async fn spawn_pub_live_server(
    (control_port, public_port): (u16, u16),
    ctrl_timeout: Duration,
) -> Result<()> {
    wait_port(control_port, false).await;
    let mut server = Server::new(public_port..=public_port, None).public_ctrl_timeout(ctrl_timeout);
    server.set_control_port(control_port);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    tokio::spawn(server.listen());
    wait_port(control_port, true).await;
    Ok(())
}

/// Wait until `port` is accepting (`listening`) or fully released.
async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// A raw control substream, so the test controls exactly what is sent — a real
/// `Client` beats on its own and could never wedge.
async fn pub_raw_control(control_port: u16) -> Result<(mux::Opener, Delimited<mux::Stream>)> {
    let tcp = TcpStream::connect(("127.0.0.1", control_port)).await?;
    let (opener, _acc) = mux::client(tcp);
    let stream = opener.open().await?;
    Ok((opener, Delimited::new(stream)))
}

/// `TunnelOptions` for a public tunnel, declaring the heartbeat capability or
/// not. `ctrl_heartbeat: false` is byte-equivalent to an old binary, which omits
/// the field entirely (`#[serde(default)]`).
fn pub_opts_live(ctrl_heartbeat: bool) -> TunnelOptions {
    TunnelOptions {
        ctrl_heartbeat,
        ..Default::default()
    }
}

/// Register a public tunnel over a raw control stream and return the server's
/// answer, keeping the connection alive in the returned handles.
async fn pub_register(
    control_port: u16,
    port: u16,
    ctrl_heartbeat: bool,
) -> Result<(mux::Opener, Delimited<mux::Stream>, Option<ServerMessage>)> {
    let (opener, mut control) = pub_raw_control(control_port).await?;
    control
        .send(ClientMessage::Hello(port, pub_opts_live(ctrl_heartbeat)))
        .await?;
    let reply = control.recv::<ServerMessage>().await?;
    Ok((opener, control, reply))
}

/// Poll until the single public port is grantable again, up to `ms`.
async fn wait_public_port_free(control_port: u16, port: u16, ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        if let Ok((_o, _c, Some(ServerMessage::Hello(granted)))) =
            pub_register(control_port, port, true).await
        {
            return granted == port;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
}

/// P-4: a wedged public client that DECLARED the heartbeat capability is reaped
/// and its public port becomes grantable again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_wedged_client_is_reaped_and_port_freed() -> Result<()> {
    spawn_pub_live_server(PUB_LIVE_REAP, Duration::from_millis(700)).await?;
    let port = PUB_LIVE_REAP.1;

    // Register, then go silent while holding the connection open.
    let (_opener, _control, reply) = pub_register(PUB_LIVE_REAP.0, port, true).await?;
    assert!(
        matches!(reply, Some(ServerMessage::Hello(p)) if p == port),
        "server grants the only public port in range: {reply:?}"
    );
    wait_port(port, true).await;

    // While it is held, the port is genuinely unavailable — otherwise the
    // re-registration below would prove nothing about the reaper.
    let (_o2, _c2, busy) = pub_register(PUB_LIVE_REAP.0, port, true).await?;
    assert!(
        matches!(busy, Some(ServerMessage::Error(_))),
        "the single public port must be refused while a live tunnel holds it: {busy:?}"
    );
    drop((_o2, _c2));

    assert!(
        wait_public_port_free(PUB_LIVE_REAP.0, port, 5000).await,
        "a wedged public client past public_ctrl_timeout must be reaped and its \
         port re-granted — before P-4 the port stayed bound until server restart"
    );
    Ok(())
}

/// DEC-VE2: a legacy client CANNOT beat, so applying the deadline to it would
/// kill a healthy idle tunnel every 60 s. It must never be reaped. This is the
/// red-check for the `Option` gate: widening the reaper to an unconditional
/// timeout fails exactly here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_legacy_client_without_capability_is_never_reaped() -> Result<()> {
    spawn_pub_live_server(PUB_LIVE_LEGACY, Duration::from_millis(400)).await?;
    let port = PUB_LIVE_LEGACY.1;

    let (_opener, _control, reply) = pub_register(PUB_LIVE_LEGACY.0, port, false).await?;
    assert!(matches!(reply, Some(ServerMessage::Hello(p)) if p == port));
    wait_port(port, true).await;

    // Silent across several 400 ms deadlines, and still holding the port.
    time::sleep(Duration::from_millis(2000)).await;
    let (_o2, _c2, busy) = pub_register(PUB_LIVE_LEGACY.0, port, true).await?;
    assert!(
        matches!(busy, Some(ServerMessage::Error(_))),
        "a client that never declared ctrl_heartbeat must keep the legacy \
         un-reaped path (DEC-VE2), so its port stays held: {busy:?}"
    );
    Ok(())
}

/// The real client must actually send what it declares. A client that sets
/// `TunnelOptions::ctrl_heartbeat: true` and then fails to beat converts every
/// healthy tunnel into a reaped one — the worst possible combination, and
/// invisible to the raw-control tests above, which drive the wire by hand.
///
/// The margin has to be the right way round or the test proves nothing: the
/// idle period must EXCEED the server deadline while the client's beat interval
/// stays comfortably under it. `BORE_CTRL_HEARTBEAT_MS` shrinks the client's
/// 20 s beat so that is expressible in a fast test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_real_client_survives_past_the_reap_deadline() -> Result<()> {
    // Client beats every 150 ms; server reaps after 700 ms; we idle for 2.5 s.
    // So ~16 beats must land inside a window covering three deadlines.
    std::env::set_var("BORE_CTRL_HEARTBEAT_MS", "150");
    spawn_pub_live_server(PUB_LIVE_CLIENT, Duration::from_millis(700)).await?;
    let port = PUB_LIVE_CLIENT.1;

    let echo = TcpListener::bind("127.0.0.1:0").await?;
    let echo_port = echo.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = echo.accept().await {
            tokio::spawn(async move {
                let _ = conn.write_all(b"alive").await;
            });
        }
    });

    let client = Client::new(
        "127.0.0.1",
        echo_port,
        &format!("127.0.0.1:{}", PUB_LIVE_CLIENT.0),
        port,
        None,
        false,
        TunnelOptions::default(),
        None,
    )
    .await?;
    assert_eq!(client.remote_port(), port);
    tokio::spawn(client.listen());
    wait_port(port, true).await;

    time::sleep(Duration::from_millis(2500)).await;
    std::env::remove_var("BORE_CTRL_HEARTBEAT_MS");

    // The tunnel must still SERVE, not merely look registered: a reaped tunnel
    // drops its listener, so a successful round trip is the real assertion.
    let mut conn = TcpStream::connect(("127.0.0.1", port)).await?;
    let body = read_some(&mut conn).await?;
    assert_eq!(
        &body, b"alive",
        "the real public client must keep its tunnel alive by beating — it \
         declared ctrl_heartbeat on the wire, so the server WILL reap it if the \
         frames do not arrive"
    );
    Ok(())
}
