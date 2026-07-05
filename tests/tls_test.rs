use std::time::Duration;

use anyhow::Result;
use bore_cli::shared::TunnelOptions;
use bore_cli::transport::{self, Endpoint};
use bore_cli::{client::Client, server::Server};
use rcgen::generate_simple_self_signed;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;

#[path = "support/websocket.rs"]
mod websocket;

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

/// Spawn an echo service that handles many connections, returning its port.
async fn echo_service_loop() -> Result<u16> {
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
    let client = Client::new(
        "localhost",
        echo,
        &to,
        0,
        Some("sec"),
        true,
        Default::default(),
        None,
    )
    .await?;
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
    let client = Client::new(
        "localhost",
        echo,
        &to,
        0,
        Some("sec"),
        false,
        Default::default(),
        None,
    )
    .await;
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
    let client = Client::new(
        "localhost",
        echo,
        &to,
        0,
        Some("sec"),
        false,
        Default::default(),
        None,
    )
    .await?;
    let tunnel_port = client.remote_port();
    tokio::spawn(client.listen());

    let mut conn = TcpStream::connect(("127.0.0.1", tunnel_port)).await?;
    conn.write_all(b"hello-http").await?;
    let mut buf = [0u8; 10];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-http");

    Ok(())
}

#[tokio::test]
async fn tunnel_port_terminates_tls_and_keeps_plain() -> Result<()> {
    const PORT: u16 = 17903;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    // Control connection is TLS (server has a cert); the tunnel also enables TLS.
    let to = format!("https://localhost:{PORT}");
    let options = TunnelOptions {
        https: true,
        force_https: false,
        ..Default::default()
    };
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), true, options, None).await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    // 1) A TLS client to the tunnel port is terminated and reaches the echo service.
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: tunnel,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    tls.write_all(b"via-tls!!").await?;
    let mut buf = [0u8; 9];
    tls.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"via-tls!!");

    // 2) A plain TCP client to the same tunnel port still works.
    let mut plain = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    plain.write_all(b"via-raw!!").await?;
    let mut buf = [0u8; 9];
    plain.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"via-raw!!");

    Ok(())
}

#[tokio::test]
async fn public_tunnel_tls_terminated_websocket_round_trip() -> Result<()> {
    const PORT: u16 = 17906;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let ws_port = websocket::spawn_websocket_echo_service(None).await?;
    let to = format!("https://localhost:{PORT}");
    let options = TunnelOptions {
        https: true,
        force_https: false,
        ..Default::default()
    };
    let client = Client::new(
        "localhost",
        ws_port,
        &to,
        0,
        Some("sec"),
        true,
        options,
        None,
    )
    .await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: tunnel,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    websocket::assert_websocket_round_trip(&mut tls, "public-wss.local", "/chat").await?;

    Ok(())
}

/// A peer that opens a TLS tunnel connection, sends the first handshake byte, then
/// stalls must be dropped by the server's handshake timeout — it must not pin a
/// connection slot — and the tunnel must keep serving healthy connections.
#[tokio::test]
async fn stalled_tls_handshake_is_dropped_and_tunnel_keeps_serving() -> Result<()> {
    const PORT: u16 = 17905;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    let to = format!("https://localhost:{PORT}");
    let options = TunnelOptions {
        https: true,
        force_https: false,
        ..Default::default()
    };
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), true, options, None).await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    // Open a connection, send only the TLS handshake content-type byte (so the edge
    // enters the TLS branch), then stall — never completing the handshake.
    let mut stall = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    stall.write_all(&[0x16]).await?;
    stall.flush().await?;

    // The server must close the stalled connection (EOF or reset) within a bound a
    // bit larger than NETWORK_TIMEOUT. Without the handshake timeout this hangs.
    let mut sink = [0u8; 1];
    match time::timeout(Duration::from_secs(8), stall.read(&mut sink)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {} // closed by the server — expected
        Ok(Ok(n)) => panic!("stalled TLS handshake unexpectedly produced {n} bytes"),
        Err(_) => {
            panic!("server did not drop the stalled TLS handshake (handshake timeout missing)")
        }
    }

    // The tunnel is still healthy: a real TLS connection completes the round-trip,
    // proving the connection slot was released and the accept loop kept serving.
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: tunnel,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    tls.write_all(b"after-sl").await?;
    let mut buf = [0u8; 8];
    tls.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"after-sl");

    Ok(())
}

#[tokio::test]
async fn force_https_redirects_plain_http() -> Result<()> {
    const PORT: u16 = 17904;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_bind_domain("bore.tld".to_string());
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    let to = format!("https://localhost:{PORT}");
    let options = TunnelOptions {
        https: true,
        force_https: true,
        ..Default::default()
    };
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), true, options, None).await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    // 1) A plain HTTP request is answered with a 308 redirect to https://, keeping
    //    the Host authority and the request path.
    let mut http = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    let request = format!("GET /path?x=1 HTTP/1.1\r\nHost: example.com:{tunnel}\r\n\r\n");
    http.write_all(request.as_bytes()).await?;
    let mut buf = vec![0u8; 512];
    let n = http.read(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(
        response.starts_with("HTTP/1.1 308"),
        "expected 308 redirect, got: {response}"
    );
    assert!(
        response.contains(&format!("Location: https://example.com:{tunnel}/path?x=1")),
        "unexpected redirect target: {response}"
    );

    // 2) TLS still works on the same port.
    let endpoint = Endpoint {
        host: "127.0.0.1".to_string(),
        port: tunnel,
        tls: true,
    };
    let mut tls = transport::connect(&endpoint, true).await?;
    tls.write_all(b"tls-ok").await?;
    let mut buf = [0u8; 6];
    tls.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"tls-ok");

    // 3) Non-HTTP raw TCP is forwarded, not redirected.
    let mut raw = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    raw.write_all(b"RAWDATA!").await?;
    let mut buf = [0u8; 8];
    raw.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"RAWDATA!");

    Ok(())
}

/// T-HP-PUB1: Policy `On` without server cert falls back to HTTP + warning.
#[tokio::test]
async fn policy_on_no_cert_falls_back_to_http() -> Result<()> {
    const PORT: u16 = 17910;
    wait_port(PORT, false).await;

    // Server WITHOUT TLS.
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    let to = format!("http://localhost:{PORT}"); // Plain HTTP control (no TLS on server).
    let options = TunnelOptions {
        https_policy: Some(bore_cli::shared::HttpsPolicy::On),
        ..Default::default()
    };
    // Client connects with plain HTTP control (insecure=false) since server has no TLS.
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), false, options, None).await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    // A plain HTTP request to the tunnel should succeed (no TLS redirect or error,
    // the server fell back due to lack of capability).
    let mut http = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    http.write_all(b"hello-http").await?;
    let mut buf = [0u8; 10];
    http.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello-http");

    Ok(())
}

/// T-HP-PUB2: Policy `Redirect` with cert produces 308 on plain HTTP.
#[tokio::test]
async fn policy_redirect_with_cert_308() -> Result<()> {
    const PORT: u16 = 17911;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    server.set_bind_domain("bore.tld".to_string());
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    let to = format!("https://localhost:{PORT}");
    let options = TunnelOptions {
        https_policy: Some(bore_cli::shared::HttpsPolicy::Redirect),
        ..Default::default()
    };
    let client = Client::new("localhost", echo, &to, 0, Some("sec"), true, options, None).await?;
    let tunnel = client.remote_port();
    tokio::spawn(client.listen());

    // A plain HTTP request gets 308 redirect to https://.
    let mut http = TcpStream::connect(("127.0.0.1", tunnel)).await?;
    let request = format!("GET / HTTP/1.1\r\nHost: example.com:{tunnel}\r\n\r\n");
    http.write_all(request.as_bytes()).await?;
    let mut buf = vec![0u8; 512];
    let n = http.read(&mut buf).await?;
    let response = String::from_utf8_lossy(&buf[..n]);
    assert!(
        response.starts_with("HTTP/1.1 308"),
        "expected 308, got: {response}"
    );
    assert!(
        response.contains(&format!("Location: https://example.com:{tunnel}/")),
        "unexpected redirect location: {response}"
    );

    Ok(())
}

/// T-HP-PUB3: Raw (non-HTTP, non-TLS) bytes pass through unchanged under every policy.
#[tokio::test]
async fn policy_raw_passthrough_matrix() -> Result<()> {
    const PORT: u16 = 17912;
    wait_port(PORT, false).await;

    let (cert_pem, key_pem) = self_signed()?;
    let acceptor = transport::server_tls_from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    server.set_tls(acceptor);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service_loop().await?;
    let to = format!("https://localhost:{PORT}");

    // Test all three policies: Off, On, Redirect.
    for policy in [
        bore_cli::shared::HttpsPolicy::Off,
        bore_cli::shared::HttpsPolicy::On,
        bore_cli::shared::HttpsPolicy::Redirect,
    ] {
        let options = TunnelOptions {
            https_policy: Some(policy),
            ..Default::default()
        };
        let client =
            Client::new("localhost", echo, &to, 0, Some("sec"), true, options, None).await?;
        let tunnel = client.remote_port();
        tokio::spawn(client.listen());

        // Raw non-HTTP, non-TLS bytes: b"\x00\x01POSTGRES\x00"
        let raw_payload = b"\x00\x01POSTGRES\x00";
        let mut conn = TcpStream::connect(("127.0.0.1", tunnel)).await?;
        conn.write_all(raw_payload).await?;
        let mut buf = vec![0u8; raw_payload.len()];
        conn.read_exact(&mut buf).await?;
        assert_eq!(
            &buf[..],
            raw_payload,
            "raw payload mismatch for policy {:?}",
            policy
        );

        time::sleep(Duration::from_millis(50)).await;
    }

    Ok(())
}

/// T-HP-PUB4: Legacy client with `https_policy: None`, `https: true`, no cert gets fatal Error.
#[tokio::test]
async fn legacy_bools_still_reject_no_cert() -> Result<()> {
    const PORT: u16 = 17913;
    wait_port(PORT, false).await;

    // Server WITHOUT TLS.
    let mut server = Server::new(1024..=65535, Some("sec"));
    server.set_control_port(PORT);
    tokio::spawn(server.listen());
    wait_port(PORT, true).await;

    let echo = echo_service().await?;
    let to = format!("http://localhost:{PORT}");
    // Legacy: https_policy = None (not set), https = true, force_https = false.
    let options = TunnelOptions {
        https: true,
        ..Default::default()
    };
    let result = Client::new("localhost", echo, &to, 0, Some("sec"), false, options, None).await;

    // Expect connection to fail due to the fatal Error from the server.
    assert!(result.is_err(), "legacy https=true without cert must fail");

    Ok(())
}
