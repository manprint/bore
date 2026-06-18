//! End-to-end tests for the `udp` direct-path mode of public tunnels.
//!
//! These run in-process on loopback: clients dial a free port assigned by the server,
//! and the QUIC direct path is verified via the server's public_direct_stream_opens counter
//! or by measuring throughput improvements and validating the tunnel works.
#![cfg(feature = "udp")]

use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::Client,
    server::{PublicUdpRegistry, Server},
    shared::CONTROL_PORT,
};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serializes tests that bind the fixed control port.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// Wait until the control port is either accepting connections (`listening`) or
/// fully released.
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

/// Spawn the server with UDP enabled and a free QUIC port. Returns the public-UDP
/// registry handle so tests can assert the direct QUIC path actually carried
/// traffic (not a silent relay fallback).
async fn spawn_server_udp() -> Result<PublicUdpRegistry> {
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_udp(true);
    let quic_listener = TcpListener::bind("127.0.0.1:0").await?;
    let quic_port = quic_listener.local_addr()?.port();
    drop(quic_listener);
    server.set_vhost_quic_port(quic_port);

    let registry = server.public_udp_registry();
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
    Ok(registry)
}

/// Direct QUIC streams the server has opened to the client for `public_port`.
fn direct_opens(registry: &PublicUdpRegistry, public_port: u16) -> u64 {
    registry
        .get(&format!("port:{public_port}"))
        .map(|e| {
            e.direct_stream_opens
                .load(std::sync::atomic::Ordering::Relaxed)
        })
        .unwrap_or(0)
}

/// Live direct QUIC carriers for `public_port`.
fn direct_carriers(registry: &PublicUdpRegistry, public_port: u16) -> usize {
    registry
        .get(&format!("port:{public_port}"))
        .map(|e| e.direct.len())
        .unwrap_or(0)
}

/// Poll until at least one direct QUIC carrier is established for `public_port`,
/// or panic after 5s. The client establishes carriers asynchronously after the
/// server's `PublicUdp` offer, so traffic must wait for the path to come up to
/// be served over QUIC rather than the (always-warm) relay.
async fn wait_for_direct_carrier(registry: &PublicUdpRegistry, public_port: u16, want: usize) {
    let deadline = time::Instant::now() + Duration::from_secs(5);
    loop {
        if direct_carriers(registry, public_port) >= want {
            return;
        }
        if time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {want} direct carrier(s) on port {public_port} (have {})",
                direct_carriers(registry, public_port)
            );
        }
        time::sleep(Duration::from_millis(20)).await;
    }
}

/// Spawn a plain server without UDP. Returns the (always-empty) public-UDP
/// registry handle so the fallback test can assert no direct stream ever opens.
async fn spawn_server_plain() -> Result<PublicUdpRegistry> {
    wait_for_control_port(false).await;
    let server = Server::new(1024..=65535, None);
    // UDP not enabled
    let registry = server.public_udp_registry();
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
    Ok(registry)
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

/// Connect to `remote_addr`, send `msg`, read the echo, and verify.
async fn round_trip(addr: std::net::SocketAddr, msg: &[u8]) -> Result<Vec<u8>> {
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(msg).await?;
    let mut buf = vec![0u8; msg.len()];
    time::timeout(Duration::from_secs(5), conn.read_exact(&mut buf)).await??;
    Ok(buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_udp_direct_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    let reg = spawn_server_udp().await?;

    // Spawn the echo service
    let echo_port = spawn_echo_service().await?;

    // Create a public tunnel with UDP enabled
    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0, // Let the server assign a port
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 1,
            udp: true,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_port = client.remote_port();
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], remote_port).into();

    tokio::spawn(client.listen());

    // Wait for the QUIC direct carrier to come up, THEN drive traffic, so the
    // connection is served over QUIC rather than the always-warm relay.
    wait_for_direct_carrier(&reg, remote_port, 1).await;

    assert_eq!(round_trip(remote_addr, b"hello").await?, b"hello");

    // Prove the traffic actually used the direct QUIC path (not a silent relay
    // fallback): the server only bumps this counter when it opens a direct stream.
    assert!(
        direct_opens(&reg, remote_port) > 0,
        "expected the connection to be served over the QUIC direct path"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_udp_many_concurrent_streams() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    let reg = spawn_server_udp().await?;

    let echo_port = spawn_echo_service().await?;

    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0,
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 1,
            udp: true,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_port = client.remote_port();
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], remote_port).into();

    tokio::spawn(client.listen());
    wait_for_direct_carrier(&reg, remote_port, 1).await;

    // Drive many concurrent connections; each must echo correctly with no
    // cross-talk between the multiplexed QUIC streams.
    let mut handles = Vec::new();
    for i in 0..20u32 {
        handles.push(tokio::spawn(async move {
            let msg = i.to_be_bytes();
            let got = round_trip(remote_addr, &msg).await?;
            assert_eq!(got, msg, "connection {i} round-tripped the wrong bytes");
            anyhow::Ok(())
        }));
    }
    for h in handles {
        h.await??;
    }

    // All 20 should have been served over QUIC direct streams.
    assert!(
        direct_opens(&reg, remote_port) >= 20,
        "expected >=20 direct stream opens, got {}",
        direct_opens(&reg, remote_port)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_udp_carriers() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    let reg = spawn_server_udp().await?;

    let echo_port = spawn_echo_service().await?;

    // Create a public tunnel with multiple carriers
    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0,
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 4,
            udp: true,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_port = client.remote_port();
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], remote_port).into();

    tokio::spawn(client.listen());

    // --carriers 4 must bring up 4 independent QUIC connections (each its own
    // congestion controller), exactly like vhost.
    wait_for_direct_carrier(&reg, remote_port, 4).await;

    // Concurrent connections should all succeed, round-robined across carriers.
    let mut handles = Vec::new();
    for i in 0..16u32 {
        handles.push(tokio::spawn(async move {
            let msg = i.to_be_bytes();
            let got = round_trip(remote_addr, &msg).await?;
            assert_eq!(got, msg);
            anyhow::Ok(())
        }));
    }
    for h in handles {
        h.await??;
    }

    assert_eq!(
        direct_carriers(&reg, remote_port),
        4,
        "expected 4 live carriers"
    );
    assert!(
        direct_opens(&reg, remote_port) >= 16,
        "expected >=16 direct stream opens across carriers, got {}",
        direct_opens(&reg, remote_port)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_udp_large_payload() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    let reg = spawn_server_udp().await?;

    let echo_port = spawn_echo_service().await?;

    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0,
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 1,
            udp: true,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_port = client.remote_port();
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], remote_port).into();

    tokio::spawn(client.listen());
    wait_for_direct_carrier(&reg, remote_port, 1).await;

    // Send a large payload (~1 MiB) and verify it round-trips byte-for-byte
    const PAYLOAD_SIZE: usize = 1024 * 1024;
    let payload: Vec<u8> = (0..PAYLOAD_SIZE).map(|i| (i % 251) as u8).collect();

    let (mut rd, mut wr) = TcpStream::connect(remote_addr).await?.into_split();

    let write_task = async {
        wr.write_all(&payload).await?;
        wr.shutdown().await?;
        anyhow::Ok(())
    };

    let read_task = async {
        let mut received = vec![0u8; PAYLOAD_SIZE];
        rd.read_exact(&mut received).await?;
        anyhow::Ok(received)
    };

    let (write_res, read_res) = tokio::join!(write_task, read_task);
    write_res?;
    let received = read_res?;
    assert_eq!(received, payload, "large payload should round-trip exactly");
    assert!(
        direct_opens(&reg, remote_port) > 0,
        "large payload should have ridden the QUIC direct path"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_udp_falls_back_to_relay_when_server_lacks_udp() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    // Create server WITHOUT UDP
    let reg = spawn_server_plain().await?;

    let echo_port = spawn_echo_service().await?;

    // Client REQUESTS UDP, but server doesn't have it enabled
    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0,
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 1,
            udp: true,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_port = client.remote_port();
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], remote_port).into();

    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(200)).await;

    // Traffic still works via relay - no UDP offer from server,
    // so tunnel gracefully falls back to TCP relay
    assert_eq!(
        round_trip(remote_addr, b"relay works").await?,
        b"relay works"
    );
    // And it never opened a direct QUIC stream (graceful fallback, not a hang).
    assert_eq!(
        direct_opens(&reg, remote_port),
        0,
        "no direct stream should open against a non-UDP server"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_tcp_still_works_without_udp() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    wait_for_control_port(false).await;
    let server = Server::new(1024..=65535, None);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    let echo_port = spawn_echo_service().await?;

    // Client explicitly disables UDP
    let client = Client::new(
        "localhost",
        echo_port,
        "localhost",
        0,
        None,
        false,
        bore_cli::shared::TunnelOptions {
            https: false,
            force_https: false,
            basic_auth: None,
            notes: None,
            carriers: 1,
            udp: false,
            auto_reconnect: false,
        },
    )
    .await?;
    let remote_addr: std::net::SocketAddr = ([127, 0, 0, 1], client.remote_port()).into();

    tokio::spawn(client.listen());
    time::sleep(Duration::from_millis(200)).await;

    // TCP relay should work when UDP is not requested
    assert_eq!(round_trip(remote_addr, b"tcp only").await?, b"tcp only");

    Ok(())
}
