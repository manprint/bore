use std::time::Duration;

use anyhow::Result;
use bore_cli::{client::Client, secret::Proxy, server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serializes tests that bind the fixed control port.
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

async fn spawn_server(secret: Option<&str>) {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, secret).listen());
    wait_for_control_port(true).await;
}

/// Bind a throwaway local listener; provider registration does not dial it.
async fn local_port() -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

#[tokio::test]
async fn secret_provider_registers() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("s3cr3t")).await;

    let (_local, port) = local_port().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        port,
        "localhost",
        "svc-a",
        Some("s3cr3t"),
        false,
        false,
        None,
        false,
        false,
    )
    .await;
    if let Err(err) = provider {
        panic!("provider should register: {err}");
    }

    Ok(())
}

#[tokio::test]
async fn secret_duplicate_id_rejected() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("s3cr3t")).await;

    let (_local, port) = local_port().await?;
    let first = Client::new_secret_provider(
        "localhost",
        port,
        "localhost",
        "dup",
        Some("s3cr3t"),
        false,
        false,
        None,
        false,
        false,
    )
    .await?;
    tokio::spawn(first.listen()); // keep the registration alive
    time::sleep(Duration::from_millis(50)).await;

    let second = Client::new_secret_provider(
        "localhost",
        port,
        "localhost",
        "dup",
        Some("s3cr3t"),
        false,
        false,
        None,
        false,
        false,
    )
    .await;
    assert!(second.is_err(), "duplicate tcp-secret-id must be rejected");

    Ok(())
}

#[tokio::test]
async fn secret_registration_requires_correct_secret() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("right")).await;

    let (_local, port) = local_port().await?;
    let wrong = Client::new_secret_provider(
        "localhost",
        port,
        "localhost",
        "svc",
        Some("wrong"),
        false,
        false,
        None,
        false,
        false,
    )
    .await;
    assert!(wrong.is_err(), "wrong secret must be rejected");

    let missing = Client::new_secret_provider(
        "localhost",
        port,
        "localhost",
        "svc2",
        None,
        false,
        false,
        None,
        false,
        false,
    )
    .await;
    assert!(missing.is_err(), "missing secret must be rejected");

    Ok(())
}

/// Spawn an echoing local service and return the port it listens on.
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

/// Bring up server + provider (echo) + proxy for id, returning the proxy address.
async fn spawn_secret_tunnel(id: &str, secret: Option<&str>) -> Result<std::net::SocketAddr> {
    spawn_server(secret).await;

    let echo_port = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo_port,
        "localhost",
        id,
        secret,
        false,
        false,
        None,
        false,
        false,
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        id,
        secret,
        false,
        false,
        None,
        false,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());

    // Let the provider registration settle before connections arrive.
    time::sleep(Duration::from_millis(50)).await;
    Ok(addr)
}

#[tokio::test]
async fn secret_tunnel_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    let addr = spawn_secret_tunnel("rt", Some("s3cr3t")).await?;

    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"hello secret").await?;
    let mut buf = [0u8; 12];
    conn.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello secret");

    Ok(())
}

#[tokio::test]
async fn secret_tunnel_large_payload() -> Result<()> {
    // Exercise the double-hop relay (consumer -> server -> provider) with a
    // payload larger than the proxy buffers, asserting byte-exact transfer.
    let _guard = SERIAL_GUARD.lock().await;
    let addr = spawn_secret_tunnel("big", None).await?;

    const LEN: usize = 1 << 20; // 1 MiB
    let payload: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();

    let mut conn = TcpStream::connect(addr).await?;
    let (mut rd, mut wr) = conn.split();
    let mut received = vec![0u8; LEN];
    let expected = payload.clone();
    let writer = async {
        wr.write_all(&payload).await?;
        wr.shutdown().await?;
        anyhow::Ok(())
    };
    let reader = async {
        rd.read_exact(&mut received).await?;
        anyhow::Ok(())
    };
    tokio::try_join!(writer, reader)?;
    assert_eq!(received, expected);

    Ok(())
}

#[tokio::test]
async fn secret_proxy_without_provider_closes() -> Result<()> {
    // A consumer connecting for an unregistered id must have its connection
    // closed (no provider to relay to), not hang.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("s3cr3t")).await;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "ghost",
        Some("s3cr3t"),
        false,
        false,
        None,
        false,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());

    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(b"anyone there?").await?;
    let mut buf = [0u8; 8];
    let n = time::timeout(Duration::from_secs(3), conn.read(&mut buf)).await??;
    assert_eq!(
        n, 0,
        "connection should be closed when no provider is registered"
    );

    Ok(())
}

#[tokio::test]
async fn secret_proxy_requires_correct_secret() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("right")).await;

    let bad = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "svc",
        Some("wrong"),
        false,
        false,
        None,
        false,
        false,
    )
    .await;
    assert!(bad.is_err(), "proxy with wrong secret must be rejected");

    Ok(())
}
