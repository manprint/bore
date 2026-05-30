use std::time::Duration;

use anyhow::Result;
use bore_cli::{client::Client, server::Server, transport};
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("localhost", port)).await.is_ok() == listening {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
}

/// Spawn an echo service, returning its local port.
async fn echo_service() -> Result<u16> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
        anyhow::Ok(())
    });
    Ok(port)
}

/// Generate a self-signed cert/key for "localhost".
fn self_signed() -> Result<(String, String)> {
    let key = generate_simple_self_signed(["localhost".to_string()])?;
    Ok((key.cert.pem(), key.signing_key.serialize_pem()))
}

#[tokio::test]
async fn tls_round_trip_with_insecure() -> Result<()> {
    const PORT: u16 = 17900;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service().await?;
    let to = format!("https://localhost:{PORT}");
    // Self-signed cert: requires --insecure on the client.
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), true).await?;
    let tunnel_port = client.remote_port();
    tokio::spawn(client.listen());

    let mut conn = TcpStream::connect(("127.0.0.1", tunnel_port)).await?;
    conn.write_all(b"hello-tls").await?;
    let mut buf = [0u8; 9];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-tls");

    Ok(())
}

#[tokio::test]
async fn tls_rejects_untrusted_cert_without_insecure() -> Result<()> {
    const PORT: u16 = 17901;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service().await?;
    let to = format!("https://localhost:{PORT}");
    // No --insecure: the self-signed certificate must fail verification.
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), false).await;
    assert!(
        client.is_err(),
        "self-signed certificate must be rejected without --insecure"
    );

    Ok(())
}

#[tokio::test]
async fn http_scheme_plain_round_trip() -> Result<()> {
    const PORT: u16 = 17902;
    wait_port(PORT, false).await;

    // No TLS on the server; the client uses the http:// scheme (plain transport).
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service().await?;
    let to = format!("http://localhost:{PORT}");
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), false).await?;
    let tunnel_port = client.remote_port();
    tokio::spawn(client.listen());

    let mut conn = TcpStream::connect(("127.0.0.1", tunnel_port)).await?;
    conn.write_all(b"hello-http").await?;
    let mut buf = [0u8; 10];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-http");

    Ok(())
}
