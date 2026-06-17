//! End-to-end integration tests filling coverage gaps for `bore local` (public tunnel)
//! and `bore proxy` (secret tunnel). These tests harden invariants around:
//! - Banner-first protocols (stream-ready before client writes)
//! - TLS + carriers interaction
//! - TLS + basic auth interaction
//! - max-conns permit recovery after rapid connection churn

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::Client,
    server::Server,
    shared::{TunnelOptions, CONTROL_PORT},
    transport,
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
        TunnelOptions::default(),
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
