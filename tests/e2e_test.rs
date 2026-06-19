use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bore_cli::{client::Client, server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
use rstest::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

#[path = "support/websocket.rs"]
mod websocket;

lazy_static! {
    /// Guard to make sure that tests are run serially, not concurrently.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// Wait until the control port is either accepting connections (`listening`) or
/// fully released. All tests share the fixed `CONTROL_PORT`, so this gates each
/// test on a clean port state rather than racing a previous test's teardown.
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

/// Spawn the server, waiting until the control port is actually accepting.
async fn spawn_server(secret: Option<&str>) {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, secret).listen());
    wait_for_control_port(true).await;
}

/// Spawns a client with randomly assigned ports, returning the listener and remote address.
async fn spawn_client(secret: Option<&str>) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let local_port = listener.local_addr()?.port();
    let client = Client::new(
        "localhost",
        local_port,
        "localhost",
        0,
        secret,
        false,
        Default::default(),
        None,
    )
    .await?;
    let remote_addr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    Ok((listener, remote_addr))
}

#[rstest]
#[tokio::test]
async fn basic_proxy(#[values(None, Some(""), Some("abc"))] secret: Option<&str>) -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(secret).await;
    let (listener, addr) = spawn_client(secret).await?;

    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 11];
        stream.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"hello world");

        stream.write_all(b"I can send a message too!").await?;
        anyhow::Ok(())
    });

    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"hello world").await?;

    let mut buf = [0u8; 25];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"I can send a message too!");

    // Ensure that the client end of the stream is closed now.
    assert_eq!(stream.read(&mut buf).await?, 0);

    // Also ensure that additional connections do not produce any data.
    let mut stream = TcpStream::connect(addr).await?;
    assert_eq!(stream.read(&mut buf).await?, 0);

    Ok(())
}

#[rstest]
#[case(None, Some("my secret"))]
#[case(Some("my secret"), None)]
#[tokio::test]
async fn mismatched_secret(
    #[case] server_secret: Option<&str>,
    #[case] client_secret: Option<&str>,
) {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(server_secret).await;
    assert!(spawn_client(client_secret).await.is_err());
}

#[tokio::test]
async fn invalid_address() -> Result<()> {
    // We don't need the serial guard for this test because it doesn't create a server.
    async fn check_address(to: &str, use_secret: bool) -> Result<()> {
        match Client::new(
            "localhost",
            5000,
            to,
            0,
            use_secret.then_some("a secret"),
            false,
            Default::default(),
            None,
        )
        .await
        {
            Ok(_) => Err(anyhow!("expected error for {to}, use_secret={use_secret}")),
            Err(_) => Ok(()),
        }
    }
    tokio::try_join!(
        check_address("google.com", false),
        check_address("google.com", true),
        check_address("nonexistent.domain.for.demonstration", false),
        check_address("nonexistent.domain.for.demonstration", true),
        check_address("malformed !$uri$%", false),
        check_address("malformed !$uri$%", true),
    )?;
    Ok(())
}

#[tokio::test]
async fn very_long_frame() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None).await;
    let mut attacker = TcpStream::connect(("localhost", CONTROL_PORT)).await?;

    // Slowly send a very long frame.
    for _ in 0..10 {
        let result = attacker.write_all(&[42u8; 100000]).await;
        if result.is_err() {
            return Ok(());
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    panic!("did not exit after a 1 MB frame");
}

#[test]
#[should_panic]
fn empty_port_range() {
    let min_port = 5000;
    let max_port = 3000;
    let _ = Server::new(min_port..=max_port, None);
}

#[tokio::test]
async fn half_closed_tcp_stream() -> Result<()> {
    // Check that "half-closed" TCP streams will not result in spontaneous hangups.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None).await;
    let (listener, addr) = spawn_client(None).await?;

    let (mut cli, (mut srv, _)) = tokio::try_join!(TcpStream::connect(addr), listener.accept())?;

    // Send data before half-closing one of the streams.
    let mut buf = b"message before shutdown".to_vec();
    cli.write_all(&buf).await?;

    // Only close the write half of the stream. This is a half-closed stream. In the
    // TCP protocol, it is represented as a FIN packet on one end. The entire stream
    // is only closed after two FINs are exchanged and ACKed by the other end.
    cli.shutdown().await?;

    srv.read_exact(&mut buf).await?;
    assert_eq!(buf, b"message before shutdown");
    assert_eq!(srv.read(&mut buf).await?, 0); // EOF

    // Now make sure that the other stream can still send data, despite
    // half-shutdown on client->server side.
    let mut buf = b"hello from the other side!".to_vec();
    srv.write_all(&buf).await?;
    cli.read_exact(&mut buf).await?;
    assert_eq!(buf, b"hello from the other side!");

    // We don't have to think about CLOSE_RD handling because that's not really
    // part of the TCP protocol, just the POSIX streams API. It is implemented by
    // the OS ignoring future packets received on that stream.

    Ok(())
}

#[tokio::test]
async fn large_payload_transfer() -> Result<()> {
    // Proxy a payload larger than any internal copy buffer, in both directions,
    // and assert it arrives byte-for-byte intact. Guards against regressions in
    // the bidirectional copy / buffer sizing.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None).await;
    let (listener, addr) = spawn_client(None).await?;

    const LEN: usize = 1 << 20; // 1 MiB, larger than the proxy copy buffers.
    let payload: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();

    // Local service drains the full payload, then echoes it back.
    let expected = payload.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; LEN];
        stream.read_exact(&mut buf).await?;
        assert_eq!(buf, expected);
        stream.write_all(&expected).await?;
        anyhow::Ok(())
    });

    let mut stream = TcpStream::connect(addr).await?;
    let (mut rd, mut wr) = stream.split();
    let mut received = vec![0u8; LEN];
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
    assert_eq!(received, payload);

    Ok(())
}

#[tokio::test]
async fn public_tunnel_websocket_round_trip() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(Some("ws-secret")).await;
    let (listener, addr) = spawn_client(Some("ws-secret")).await?;
    websocket::spawn_websocket_echo_listener(listener, None);

    let mut stream = TcpStream::connect(addr).await?;
    websocket::assert_websocket_round_trip(&mut stream, "public-ws.local", "/socket").await?;

    Ok(())
}

#[tokio::test]
async fn many_concurrent_connections() -> Result<()> {
    // Drive many simultaneous proxied connections through a single tunnel and
    // assert each one round-trips its own distinct message. Guards concurrency.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None).await;
    let (listener, addr) = spawn_client(None).await?;

    const N: u32 = 30;

    // Local service: echo a 4-byte message for every incoming connection.
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

    let mut handles = Vec::new();
    for i in 0..N {
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
async fn tunnel_survives_aborted_connections() -> Result<()> {
    // A burst of external connections that hang up immediately must not tear
    // down the tunnel: a normal connection still has to work afterwards. Guards
    // the server's accept loop staying alive through connection churn.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None).await;
    let (listener, addr) = spawn_client(None).await?;

    // Local echo service, tolerant of peers that close before sending.
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0u8; 4];
                if stream.read_exact(&mut buf).await.is_ok() {
                    let _ = stream.write_all(&buf).await;
                }
                anyhow::Ok(())
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    });

    // Churn: connect then immediately drop, without sending anything.
    for _ in 0..20 {
        let s = TcpStream::connect(addr).await?;
        drop(s);
    }
    time::sleep(Duration::from_millis(100)).await;

    // The tunnel must still serve a normal request.
    let mut stream = TcpStream::connect(addr).await?;
    stream.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ping");

    Ok(())
}

#[tokio::test]
async fn concurrent_connections_are_bounded() -> Result<()> {
    // The local service holds every connection open without responding, so each
    // proxied connection keeps its server permit. A flood beyond `max_conns` must
    // then be dropped rather than growing memory and file descriptors without
    // limit.
    let _guard = SERIAL_GUARD.lock().await;

    const MAX: usize = 5;
    const EXTRA: usize = 20;

    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_max_conns(MAX);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;

    // Local service that accepts and then holds connections open indefinitely.
    let local = TcpListener::bind("localhost:0").await?;
    let local_port = local.local_addr()?.port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (stream, _) = local.accept().await?;
            held.push(stream); // keep alive, never read or write
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
        Default::default(),
        None,
    )
    .await?;
    let addr: SocketAddr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());

    // Open more connections than the cap allows, nudging each so the server
    // actually forwards it and takes a permit.
    let mut socks = Vec::new();
    for _ in 0..(MAX + EXTRA) {
        let mut s = TcpStream::connect(addr).await?;
        let _ = s.write_all(b"x").await;
        socks.push(s);
    }

    // Excess connections (beyond MAX) must be dropped by the server. Poll until
    // at least EXTRA of them have observed EOF.
    let mut closed = 0;
    for _ in 0..60 {
        closed = 0;
        for s in &mut socks {
            let mut buf = [0u8; 1];
            // A dropped connection shows up as a graceful EOF (`Ok(0)`, a FIN) or a
            // reset (`Err`, an RST — Windows sends one when the socket is closed
            // with our unread "x" still buffered). Both mean the server dropped it;
            // a still-proxied connection just times out with no data ready.
            match time::timeout(Duration::from_millis(10), s.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => closed += 1,
                _ => {}
            }
        }
        if closed >= EXTRA {
            break;
        }
        time::sleep(Duration::from_millis(50)).await;
    }
    assert!(closed >= EXTRA, "expected >= {EXTRA} dropped, got {closed}");

    Ok(())
}
