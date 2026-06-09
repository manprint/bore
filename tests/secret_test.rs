use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    admin::Role,
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

#[path = "support/websocket.rs"]
mod websocket;

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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
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
        0,
        0, // release timeout
        1, // carriers
        None,
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
async fn secret_tunnel_websocket_relay_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("ws-secret")).await;

    let ws_port = websocket::spawn_websocket_echo_service(None).await?;
    let provider = Client::new_secret_provider(
        "localhost",
        ws_port,
        "localhost",
        "ws-relay",
        Some("ws-secret"),
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1024,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(provider.listen());

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "ws-relay",
        Some("ws-secret"),
        false,
        false,
        None,
        false,
        false,
        0,
        0,
        1,
        None,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    let mut conn = TcpStream::connect(addr).await?;
    websocket::assert_websocket_round_trip(&mut conn, "secret-relay.local", "/chat").await?;
    Ok(())
}

#[cfg(feature = "udp")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_tunnel_websocket_direct_udp_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, Some("ws-udp-secret"));
    server.set_udp(true);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    let ws_port = websocket::spawn_websocket_echo_service(None).await?;
    let provider = Client::new_secret_provider(
        "localhost",
        ws_port,
        "localhost",
        "ws-direct",
        Some("ws-udp-secret"),
        false,
        true,
        None,
        false,
        false,
        0,
        0,
        1024,
        1,
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(200)).await;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "ws-direct",
        Some("ws-udp-secret"),
        false,
        true,
        None,
        false,
        false,
        0,
        0,
        1,
        None,
    )
    .await?;
    assert!(proxy.is_direct(), "expected the secret websocket test to negotiate direct UDP");
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    let mut conn = TcpStream::connect(addr).await?;
    websocket::assert_websocket_round_trip(&mut conn, "secret-direct.local", "/chat").await?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_multiple_consumers_concurrent() -> Result<()> {
    // One provider must serve many simultaneous `bore proxy` consumers on the same
    // id. Each consumer is its own server-side `serve_consumer`; the server relays
    // every consumer's substreams to the *one* shared provider connection over
    // yamux. Distinct per-consumer payloads assert there is no cross-talk.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("s3cr3t")).await;

    let echo_port = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo_port,
        "localhost",
        "multi",
        Some("s3cr3t"),
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1024,
        1, // carriers
        ProviderMeta::default(),
    )
    .await?;
    tokio::spawn(provider.listen());

    // Bring up three independent consumers on the same id.
    let mut addrs = Vec::new();
    for _ in 0..3 {
        let proxy = Proxy::new(
            "localhost",
            "127.0.0.1:0".parse()?,
            "multi",
            Some("s3cr3t"),
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
        addrs.push(proxy.local_addr()?);
        tokio::spawn(proxy.listen());
    }
    time::sleep(Duration::from_millis(100)).await;

    // Drive all three concurrently with a large, distinct payload each; every
    // consumer must get exactly its own bytes back.
    let mut tasks = Vec::new();
    for (i, addr) in addrs.into_iter().enumerate() {
        tasks.push(tokio::spawn(async move {
            const LEN: usize = 256 * 1024;
            let payload: Vec<u8> = (0..LEN).map(|j| (j.wrapping_add(i) % 251) as u8).collect();
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
            anyhow::ensure!(received == expected, "consumer {i} got mismatched bytes");
            anyhow::Ok(())
        }));
    }
    for t in tasks {
        t.await??;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secret_single_consumer_many_connections() -> Result<()> {
    // A single consumer opening many simultaneous connections must have each one
    // served independently (per-connection substream over the consumer's mux).
    let _guard = SERIAL_GUARD.lock().await;
    let addr = spawn_secret_tunnel("fanout", Some("s3cr3t")).await?;

    let mut tasks = Vec::new();
    for i in 0..16u8 {
        tasks.push(tokio::spawn(async move {
            let mut conn = TcpStream::connect(addr).await?;
            let msg = [i; 32];
            conn.write_all(&msg).await?;
            let mut buf = [0u8; 32];
            time::timeout(Duration::from_secs(5), conn.read_exact(&mut buf)).await??;
            anyhow::ensure!(buf == msg, "connection {i} got mismatched bytes");
            anyhow::Ok(())
        }));
    }
    for t in tasks {
        t.await??;
    }

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
        0,
        0, // release timeout
        1, // carriers
        None,
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
        0,
        0, // release timeout
        1, // carriers
        None,
    )
    .await;
    assert!(bad.is_err(), "proxy with wrong secret must be rejected");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_registry_reflects_connections() -> Result<()> {
    // The in-memory admin registry must reflect live connections: a public
    // tunnel, a secret provider (with notes + a basic-auth flag), and a secret
    // consumer (with notes) all appear with the right fields, and an entry
    // disappears when its connection ends.
    let _guard = SERIAL_GUARD.lock().await;
    wait_for_control_port(false).await;
    let server = Server::new(1024..=65535, None);
    let admin = server.admin_registry();
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    let echo = spawn_echo_service().await?;

    // Public tunnel with basic-auth + notes.
    let pub_client = Client::new(
        "localhost",
        echo,
        "localhost",
        0,
        None,
        false,
        TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: Some("a:b".into()),
            notes: Some("pub note".into()),
            ..Default::default()
        },
    )
    .await?;
    let pub_port = pub_client.remote_port();
    tokio::spawn(pub_client.listen());

    // Secret provider with notes + basic-auth flag.
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "admined",
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
            notes: Some("prov note".into()),
            basic_auth: Some("u:p".into()),
        },
    )
    .await?;
    tokio::spawn(provider.listen());

    // Secret consumer with notes.
    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "admined",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1, // carriers
        Some("cons note".into()),
    )
    .await?;
    let consumer = tokio::spawn(proxy.listen());

    time::sleep(Duration::from_millis(200)).await;
    let snap = admin.snapshot();

    let public = snap
        .iter()
        .find(|e| e.role == Role::Public)
        .expect("public entry");
    assert_eq!(public.public_port, Some(pub_port));
    assert_eq!(public.notes.as_deref(), Some("pub note"));
    assert!(public.basic_auth, "public basic_auth flag must be set");

    let prov = snap
        .iter()
        .find(|e| e.role == Role::SecretProvider)
        .expect("provider entry");
    assert_eq!(prov.secret_id.as_deref(), Some("admined"));
    assert_eq!(prov.notes.as_deref(), Some("prov note"));
    assert!(prov.basic_auth, "provider basic_auth flag must be set");

    let cons = snap
        .iter()
        .find(|e| e.role == Role::SecretConsumer)
        .expect("consumer entry");
    assert_eq!(cons.secret_id.as_deref(), Some("admined"));
    assert_eq!(cons.notes.as_deref(), Some("cons note"));

    // Drop the consumer; its entry must disappear from the registry.
    consumer.abort();
    time::sleep(Duration::from_millis(300)).await;
    assert!(
        admin
            .snapshot()
            .iter()
            .all(|e| e.role != Role::SecretConsumer),
        "consumer entry must be removed after disconnect"
    );

    Ok(())
}
