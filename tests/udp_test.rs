//! End-to-end tests for the `udp` direct-path mode of secret tunnels.
//!
//! These run on loopback: STUN discovery returns `127.0.0.1`, the hole-punch is
//! a no-op, and QUIC connects locally — but the full negotiation, token
//! handshake, and yamux-over-QUIC carrier are exercised. The relay fallback is
//! checked too.
#![cfg(feature = "udp")]

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

async fn spawn_server(udp: bool) {
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_udp(udp);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
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

async fn round_trip(addr: std::net::SocketAddr, msg: &[u8]) -> Result<Vec<u8>> {
    let mut conn = TcpStream::connect(addr).await?;
    conn.write_all(msg).await?;
    let mut buf = vec![0u8; msg.len()];
    time::timeout(Duration::from_secs(5), conn.read_exact(&mut buf)).await??;
    Ok(buf)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_direct_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "udpsvc",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
        1024,
    )
    .await?;
    tokio::spawn(provider.listen());
    // Let the provider register and offer its candidates to the server.
    time::sleep(Duration::from_millis(300)).await;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "udpsvc",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
    )
    .await?;
    assert!(
        proxy.is_direct(),
        "consumer should negotiate a direct UDP path"
    );
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    assert_eq!(round_trip(addr, b"udp hello").await?, b"udp hello");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_direct_survives_consumer_reconnect() -> Result<()> {
    // A reconnecting consumer must get the direct path again (regression: the
    // provider used to set the path up once and reject the second consumer with
    // "unexpected udp punch" / a per-consumer token mismatch).
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "rc",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
        1024,
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(300)).await;

    let mk_proxy = || {
        let stun = stun.clone();
        async move {
            Proxy::new(
                "localhost",
                "127.0.0.1:0".parse().unwrap(),
                "rc",
                None,
                false,
                true,
                Some(&stun),
                false,
                false,
                0,
            )
            .await
        }
    };

    // First consumer: direct, works, then disconnects.
    let proxy1 = mk_proxy().await?;
    assert!(proxy1.is_direct(), "first consumer should be direct");
    let addr1 = proxy1.local_addr()?;
    let h1 = tokio::spawn(proxy1.listen());
    time::sleep(Duration::from_millis(100)).await;
    assert_eq!(round_trip(addr1, b"first").await?, b"first");
    h1.abort(); // simulate Ctrl-C on the proxy
    time::sleep(Duration::from_millis(400)).await;

    // Reconnecting consumer must negotiate the direct path again.
    let proxy2 = mk_proxy().await?;
    assert!(
        proxy2.is_direct(),
        "reconnecting consumer should get the direct path again"
    );
    let addr2 = proxy2.local_addr()?;
    tokio::spawn(proxy2.listen());
    time::sleep(Duration::from_millis(100)).await;
    assert_eq!(round_trip(addr2, b"again").await?, b"again");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_consumer_detects_provider_drop() -> Result<()> {
    // When the provider dies, the consumer's direct QUIC path dies too. The
    // consumer must notice (even though its control channel to the server stays
    // up) and return from `listen()` so `--auto-reconnect` can re-negotiate —
    // regression: it used to keep using the dead path ("failed to open stream").
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "drop",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
        1024,
    )
    .await?;
    let h_provider = tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(300)).await;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "drop",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
    )
    .await?;
    assert!(proxy.is_direct(), "consumer should be direct");
    let addr = proxy.local_addr()?;
    let h_proxy = tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;
    assert_eq!(round_trip(addr, b"alive").await?, b"alive");

    // Kill the provider: aborting its listen drops the punch channel, the direct
    // QUIC endpoint closes, and the consumer's path dies.
    h_provider.abort();

    // The consumer's listen() must return (not hang on the dead path).
    let returned = time::timeout(Duration::from_secs(10), h_proxy).await;
    assert!(
        returned.is_ok(),
        "consumer should detect the dead direct path and stop serving"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_relay_upgrades_to_direct_when_provider_appears() -> Result<()> {
    // A consumer that starts on the relay (no UDP provider yet) must upgrade to
    // the direct path on its own once a UDP provider appears — without dropping.
    // Proven indirectly: after the upgrade, killing the provider makes the
    // consumer's listen() return (it now tracks the direct path); a still-relay
    // consumer would keep running because its control channel stays up.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    // Consumer connects first, with no provider yet → starts on the relay.
    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "up",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
    )
    .await?;
    assert!(!proxy.is_direct(), "consumer should start on the relay");
    let h_proxy = tokio::spawn(proxy.listen());

    // Now a UDP provider appears.
    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "up",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
        1024,
    )
    .await?;
    let h_provider = tokio::spawn(provider.listen());

    // Wait past one upgrade interval for the consumer to switch to direct.
    time::sleep(Duration::from_secs(13)).await;

    // Kill the provider: only a consumer that upgraded to direct will notice and
    // return from listen().
    h_provider.abort();
    let returned = time::timeout(Duration::from_secs(8), h_proxy).await;
    assert!(
        returned.is_ok(),
        "consumer should have upgraded to direct and then detected the provider drop"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_falls_back_to_relay_without_udp_provider() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await; // STUN available so the consumer gathers quickly
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let echo = spawn_echo_service().await?;
    // Provider does NOT opt into udp, so it is not in the UDP registry.
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "relaysvc",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        1024,
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(200)).await;

    // Consumer requests udp but the server has no UDP-capable provider for the id.
    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "relaysvc",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
    )
    .await?;
    assert!(
        !proxy.is_direct(),
        "consumer should fall back to the relay when no UDP provider exists"
    );
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    assert_eq!(round_trip(addr, b"relay hi").await?, b"relay hi");
    Ok(())
}

/// The consumer dials all provider candidates concurrently under one total budget
/// (not a full timeout per candidate), so a long list of dead candidates fails
/// fast instead of summing `N * NETWORK_TIMEOUT`. No server is needed: this drives
/// `connect_direct` directly with unreachable peers.
#[tokio::test]
async fn udp_connect_budget_bounded() {
    use bore_cli::holepunch;

    let socket = holepunch::bind_socket(0).await.unwrap();
    // 8 non-routable TEST-NET-3 addresses (RFC 5737) that never answer.
    let peers: Vec<std::net::SocketAddr> = (1..=8)
        .map(|i| format!("203.0.113.{i}:9").parse().unwrap())
        .collect();
    let token = holepunch::derive_token(Some("s"), &[0u8; 16]);

    let start = std::time::Instant::now();
    let res = holepunch::connect_direct(socket, peers, token).await;
    let elapsed = start.elapsed();

    assert!(res.is_err(), "dial to dead candidates must fail");
    // Serial worst case would be 8 * NETWORK_TIMEOUT (~24s); the concurrent dial
    // caps the whole attempt near one NETWORK_TIMEOUT (3s) + the brief punch.
    assert!(
        elapsed < Duration::from_secs(8),
        "connect budget not bounded: took {elapsed:?}"
    );
}

/// The direct path enforces the provider's `--max-conns` (parity with the relay):
/// connections beyond the cap are dropped, not served. Here the provider's cap is
/// 2; two held-open connections consume both permits, and a third is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_direct_respects_max_conns() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(true).await;
    let stun = format!("127.0.0.1:{CONTROL_PORT}");

    let echo = spawn_echo_service().await?;
    let provider = Client::new_secret_provider(
        "localhost",
        echo,
        "localhost",
        "cap",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
        2, // max_conns = 2
    )
    .await?;
    tokio::spawn(provider.listen());
    time::sleep(Duration::from_millis(300)).await;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "cap",
        None,
        false,
        true,
        Some(&stun),
        false,
        false,
        0,
    )
    .await?;
    assert!(proxy.is_direct(), "consumer should be direct");
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(100)).await;

    // Open and hold two connections, confirming each is served (consuming a
    // permit that stays held while the connection is open).
    let mut held = Vec::new();
    for i in 0..2u8 {
        let mut c = TcpStream::connect(addr).await?;
        c.write_all(&[i]).await?;
        let mut b = [0u8; 1];
        time::timeout(Duration::from_secs(2), c.read_exact(&mut b))
            .await
            .expect("held connection should be served within the cap")?;
        assert_eq!(b[0], i);
        held.push(c);
    }

    // A third connection is over the cap: the provider drops its substream, so it
    // is never echoed — the local side sees EOF (or, at worst, no data).
    let mut over = TcpStream::connect(addr).await?;
    over.write_all(&[42]).await?;
    let mut b = [0u8; 1];
    let res = time::timeout(Duration::from_secs(2), over.read_exact(&mut b)).await;
    assert!(
        matches!(res, Ok(Err(_)) | Err(_)),
        "connection over --max-conns must not be served, got {res:?}"
    );

    drop(held);
    Ok(())
}
