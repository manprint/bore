//! End-to-end tests for the admin status page served on the control port.

use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::CONTROL_PORT,
    transport::{self, Endpoint},
};
use lazy_static::lazy_static;
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

// A realistic >=32-char admin token.
const TOKEN: &str = "0123456789abcdef0123456789abcdef01234567";

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

async fn spawn_echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
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

fn self_signed() -> Result<(String, String)> {
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

/// Issue one HTTP/1.1 GET over `stream` and return the full response text.
async fn http_get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    token: Option<&str>,
) -> Result<String> {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = Vec::new();
    time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_disabled_does_not_serve_http() -> Result<()> {
    // With no --admin-token the control port speaks only the bore protocol: an
    // HTTP request is not served (the connection is dropped), preserving behaviour.
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    tokio::spawn(Server::new(1024..=65535, None).listen());
    wait_port(CONTROL_PORT, true).await;

    let mut s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    s.write_all(b"GET /admin/status HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await?;
    let mut buf = vec![0u8; 128];
    let n = time::timeout(Duration::from_secs(2), s.read(&mut buf))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);
    let resp = String::from_utf8_lossy(&buf[..n]);
    assert!(
        !resp.contains("HTTP/1.1 2") && !resp.contains("<html"),
        "admin must not serve HTTP when disabled, got: {resp:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_enabled_serves_page_and_guards_data() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;

    // The HTML shell is served without a token.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let page = http_get(s, "/admin/status", None).await?;
    assert!(page.starts_with("HTTP/1.1 200"), "page status: {page:.40}");
    assert!(page.contains("text/html"), "page content-type");
    assert!(page.contains("bore"), "page should contain the app shell");

    // The data endpoint requires the token.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let unauth = http_get(s, "/admin/status/data", None).await?;
    assert!(
        unauth.starts_with("HTTP/1.1 401"),
        "data without token must be 401: {unauth:.40}"
    );

    // A wrong token is also rejected.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let wrong = http_get(s, "/admin/status/data", Some("wrong-token")).await?;
    assert!(wrong.starts_with("HTTP/1.1 401"), "wrong token must be 401");

    // The correct token returns the JSON snapshot.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.starts_with("HTTP/1.1 200"), "data status: {data:.40}");
    assert!(data.contains("application/json"), "data content-type");
    assert!(data.contains("\"server\""));
    assert!(data.contains("\"tunnels\""));

    // An unknown path is a 404.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let nf = http_get(s, "/nope", Some(TOKEN)).await?;
    assert!(nf.starts_with("HTTP/1.1 404"), "unknown path must be 404");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_data_reflects_live_tunnels() -> Result<()> {
    let _g = SERIAL_GUARD.lock().await;
    wait_port(CONTROL_PORT, false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(CONTROL_PORT, true).await;

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "livesvc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        1024,
        1, // carriers
        ProviderMeta {
            notes: Some("hello-note".into()),
            basic_auth: None,
        },
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "livesvc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        1, // carriers
        Some("consumer-note".into()),
    )
    .await?;
    let consumer = tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(200)).await;

    // Both the provider and the consumer (and the notes) show up.
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.contains("livesvc"), "secret id must appear: {data}");
    assert!(data.contains("hello-note"), "provider note must appear");
    assert!(data.contains("consumer-note"), "consumer note must appear");
    assert!(
        data.contains("secret-provider"),
        "provider role must appear"
    );
    assert!(
        data.contains("secret-consumer"),
        "consumer role must appear"
    );

    // After the consumer disconnects, it disappears from the snapshot.
    consumer.abort();
    time::sleep(Duration::from_millis(400)).await;
    let s = TcpStream::connect(("127.0.0.1", CONTROL_PORT)).await?;
    let data2 = http_get(s, "/admin/status/data", Some(TOKEN)).await?;
    assert!(
        !data2.contains("secret-consumer"),
        "consumer entry must be gone after disconnect: {data2}"
    );
    assert!(
        data2.contains("secret-provider"),
        "provider must still be present"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_served_over_tls() -> Result<()> {
    const PORT: u16 = 17960;
    let _g = SERIAL_GUARD.lock().await;
    wait_port(PORT, false).await;

    let (cert, key) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert.as_bytes(), key.as_bytes())?;
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_admin_token(Some(TOKEN.into()));
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    // The page must be served over the same TLS the control port uses (https).
    let ep = Endpoint {
        host: "localhost".to_string(),
        port: PORT,
        tls: true,
    };
    let stream = transport::connect(&ep, true).await?; // insecure: self-signed
    let page = http_get(stream, "/admin/status", None).await?;
    assert!(page.starts_with("HTTP/1.1 200"), "tls page: {page:.40}");
    assert!(page.contains("bore"), "tls page should contain the shell");

    // And the guarded data endpoint over TLS too.
    let stream = transport::connect(&ep, true).await?;
    let data = http_get(stream, "/admin/status/data", Some(TOKEN)).await?;
    assert!(data.starts_with("HTTP/1.1 200"), "tls data: {data:.40}");
    assert!(data.contains("\"server\""));

    Ok(())
}
