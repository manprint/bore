//! End-to-end tests for HTTP Basic auth on public and secret tunnels.
//!
//! Public tunnels are gated on the server; secret tunnels on the provider. In
//! both cases an unauthenticated HTTP request gets a `401`, a correctly
//! authenticated one reaches the local service, and non-HTTP traffic is forwarded
//! unprotected (basic auth is HTTP-only).

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
    c.write_all(b"RAWBYTES").await?; // 8 non-HTTP bytes
    let mut got = [0u8; 8];
    time::timeout(Duration::from_secs(3), c.read_exact(&mut got)).await??;
    assert_eq!(&got, b"RAWBYTES", "non-HTTP traffic must pass through");

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
        false,
        false,
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta {
            notes: None,
            basic_auth: Some(CREDS.into()),
            auto_reconnect: false,
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
        false,
        false,
        0,
        0, // release timeout
        1, // carriers
        None,
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
