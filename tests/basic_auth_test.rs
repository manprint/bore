//! End-to-end tests for HTTP Basic auth on public and secret tunnels.
//!
//! Public tunnels are gated on the server; secret tunnels on the provider. In
//! both cases an unauthenticated HTTP request gets a `401`, a correctly
//! authenticated one reaches the local service. Whether bytes are HTTP is decided
//! by the shape of the request line, not a method list (WebDAV, the h2c preface,
//! lowercase methods are all gated), a connection that idles past the gate's
//! window is dropped instead of forwarded, and only positively non-HTTP traffic
//! is forwarded unprotected (basic auth is HTTP-only).

use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::{TunnelOptions, CONTROL_PORT},
};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

const CREDS: &str = "user:pass";
// base64("user:pass") == "dXNlcjpwYXNz".
const AUTH_HEADER: &str = "Authorization: Basic dXNlcjpwYXNz\r\n";

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

async fn spawn_server() {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, None).listen());
    wait_for_control_port(true).await;
}

/// Echo service: bounces back whatever bytes it receives.
async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    let n = stream.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).await?;
                }
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });
    Ok(port)
}

/// Read whatever is available within a short window.
async fn read_some(conn: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    let n = time::timeout(Duration::from_secs(3), conn.read(&mut buf)).await??;
    buf.truncate(n);
    Ok(buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_basic_auth_rejects_and_allows() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;

    let client = Client::new(
        "localhost",
        echo,
        "localhost",
        0,
        None,
        false,
        TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: Some(CREDS.into()),
            notes: None,
            ..Default::default()
        },
        None,
    )
    .await?;
    let port = client.remote_port();
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(100)).await;

    // No credentials → 401.
    let mut c = TcpStream::connect(("127.0.0.1", port)).await?;
    c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await?;
    let resp = String::from_utf8_lossy(&read_some(&mut c).await?).into_owned();
    assert!(
        resp.starts_with("HTTP/1.1 401"),
        "expected 401, got: {resp}"
    );
    assert!(
        resp.contains("WWW-Authenticate: Basic"),
        "401 must carry a Basic challenge, got: {resp}"
    );

    // Correct credentials → request reaches the echo service (echoed back).
    let mut c2 = TcpStream::connect(("127.0.0.1", port)).await?;
    let req = format!("GET / HTTP/1.1\r\nHost: x\r\n{AUTH_HEADER}\r\n");
    c2.write_all(req.as_bytes()).await?;
    let mut got = vec![0u8; req.len()];
    time::timeout(Duration::from_secs(3), c2.read_exact(&mut got)).await??;
    assert_eq!(
        got,
        req.as_bytes(),
        "authorized request must reach the local service"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_basic_auth_passes_non_http() -> Result<()> {
    // Non-HTTP traffic cannot be authenticated and is forwarded unprotected.
    // (Anything shaped like a request line is HTTP and gated — see below.)
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;

    let client = Client::new(
        "localhost",
        echo,
        "localhost",
        0,
        None,
        false,
        TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: Some(CREDS.into()),
            notes: None,
            ..Default::default()
        },
        None,
    )
    .await?;
    let port = client.remote_port();
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(100)).await;

    let mut c = TcpStream::connect(("127.0.0.1", port)).await?;
    c.write_all(b"RAWBYTES\r\n").await?; // token + CRLF: not a request line
    let mut got = [0u8; 10];
    time::timeout(Duration::from_secs(3), c.read_exact(&mut got)).await??;
    assert_eq!(&got, b"RAWBYTES\r\n", "non-HTTP traffic must pass through");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_provider_basic_auth_rejects_and_allows() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;

    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "authed",
        None,
        false,
        false,
        None,
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta {
            notes: None,
            basic_auth: Some(CREDS.into()),
            auto_reconnect: false,
            https_policy: None,
            backend_tls: false,
            backend_tls_sni: None,
        },
        None,
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "authed",
        None,
        false,
        false,
        None,
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0, // release timeout
        1, // carriers
        None,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    // No credentials → 401 (enforced on the provider, relayed back to the visitor).
    let mut c = TcpStream::connect(addr).await?;
    c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await?;
    let resp = String::from_utf8_lossy(&read_some(&mut c).await?).into_owned();
    assert!(
        resp.starts_with("HTTP/1.1 401"),
        "expected 401, got: {resp}"
    );

    // Correct credentials → reaches the echo service.
    let mut c2 = TcpStream::connect(addr).await?;
    let req = format!("GET / HTTP/1.1\r\nHost: x\r\n{AUTH_HEADER}\r\n");
    c2.write_all(req.as_bytes()).await?;
    let mut got = vec![0u8; req.len()];
    time::timeout(Duration::from_secs(3), c2.read_exact(&mut got)).await??;
    assert_eq!(
        got,
        req.as_bytes(),
        "authorized request must reach the local service"
    );

    Ok(())
}

/// Bind a public tunnel to the echo service with basic auth; returns its port.
async fn spawn_public_authed(echo: u16) -> Result<u16> {
    let client = Client::new(
        "localhost",
        echo,
        "localhost",
        0,
        None,
        false,
        TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: Some(CREDS.into()),
            notes: None,
            ..Default::default()
        },
        None,
    )
    .await?;
    let port = client.remote_port();
    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(100)).await;
    Ok(port)
}

/// Whether the peer closed (EOF or reset) without sending a single byte.
async fn closed_without_data(conn: &mut TcpStream) -> bool {
    let mut b = [0u8; 64];
    matches!(
        time::timeout(Duration::from_secs(3), conn.read(&mut b)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_basic_auth_gates_methods_outside_the_old_allow_list() -> Result<()> {
    // Regression: the gate used to know nine method prefixes and forwarded
    // everything else — PROPFIND/MKCOL/COPY/..., the h2c `PRI` preface — to the
    // service with no authentication at all.
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;
    let port = spawn_public_authed(echo).await?;

    for head in [
        "PROPFIND / HTTP/1.1\r\nHost: x\r\n\r\n",
        "MKCOL /d HTTP/1.1\r\nHost: x\r\n\r\n",
        "COPY /a HTTP/1.1\r\nHost: x\r\nDestination: /b\r\n\r\n",
        "get / HTTP/1.1\r\nHost: x\r\n\r\n",
        "\r\nPROPFIND / HTTP/1.1\r\nHost: x\r\n\r\n",
        "PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n",
    ] {
        let mut c = TcpStream::connect(("127.0.0.1", port)).await?;
        c.write_all(head.as_bytes()).await?;
        let resp = String::from_utf8_lossy(&read_some(&mut c).await?).into_owned();
        assert!(
            resp.starts_with("HTTP/1.1 401"),
            "{head:?} must be answered 401, got: {resp:?}"
        );
    }

    // With credentials the same method reaches the service.
    let req = format!("PROPFIND / HTTP/1.1\r\nHost: x\r\n{AUTH_HEADER}\r\n");
    let mut c = TcpStream::connect(("127.0.0.1", port)).await?;
    c.write_all(req.as_bytes()).await?;
    let mut got = vec![0u8; req.len()];
    time::timeout(Duration::from_secs(3), c.read_exact(&mut got)).await??;
    assert_eq!(got, req.as_bytes(), "authorized PROPFIND must be forwarded");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_basic_auth_idle_connection_cannot_bypass_the_gate() -> Result<()> {
    // Regression: a connection silent for longer than the edge peek (3 s) plus
    // the gate window (3 s) used to be forwarded as "non-HTTP"; a request sent
    // afterwards then reached the service unauthenticated.
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;
    let port = spawn_public_authed(echo).await?;

    let mut c = TcpStream::connect(("127.0.0.1", port)).await?;
    time::sleep(Duration::from_secs(8)).await;
    // The server has already given up on us; whatever we send must not be
    // answered by the service (an echo would mirror the request back).
    let _ = c
        .write_all(b"GET /secret HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    let mut buf = vec![0u8; 256];
    let n = time::timeout(Duration::from_secs(3), c.read(&mut buf))
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    let got = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(
        !got.contains("/secret"),
        "idle connection reached the service ungated: {got:?}"
    );
    assert!(closed_without_data(&mut c).await || n == 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secret_provider_basic_auth_gates_webdav_and_idle() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo = spawn_echo_service().await?;

    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "authed2",
        None,
        false,
        false,
        None,
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        1024,
        1,
        ProviderMeta {
            notes: None,
            basic_auth: Some(CREDS.into()),
            auto_reconnect: false,
            https_policy: None,
            backend_tls: false,
            backend_tls_sni: None,
        },
        None,
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "authed2",
        None,
        false,
        false,
        None,
        bore_cli::holepunch::GatherOptions::from_flags(false, false),
        0,
        0,
        1,
        None,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    // WebDAV method without credentials → 401 from the provider.
    let mut c = TcpStream::connect(addr).await?;
    c.write_all(b"PROPFIND / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await?;
    let resp = String::from_utf8_lossy(&read_some(&mut c).await?).into_owned();
    assert!(resp.starts_with("HTTP/1.1 401"), "got: {resp:?}");

    // Idle past the gate window, then a request: never reaches the service.
    let mut c = TcpStream::connect(addr).await?;
    time::sleep(Duration::from_secs(5)).await;
    let _ = c
        .write_all(b"GET /secret HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    let mut buf = vec![0u8; 256];
    let n = time::timeout(Duration::from_secs(3), c.read(&mut buf))
        .await
        .map(|r| r.unwrap_or(0))
        .unwrap_or(0);
    let got = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(
        !got.contains("/secret"),
        "idle connection reached the service ungated: {got:?}"
    );
    Ok(())
}
