//! End-to-end tests for the public-tunnel **carrier pool** (`--carriers`): the
//! client opens several parallel TCP connections and the server spreads proxied
//! connections across them (round-robin) to avoid yamux's single-connection
//! head-of-line blocking. These assert the data path stays correct across the
//! pool — round-trips, half-close, large payloads — and that the server's
//! `--max-carriers` cap is honoured (a request above it degrades gracefully).

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::Client,
    server::Server,
    shared::{TunnelOptions, CONTROL_PORT},
};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Serialize tests: they all share the fixed `CONTROL_PORT`.
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

/// Spawn a server with a specific `--max-carriers` cap.
async fn spawn_server(max_carriers: u16) {
    wait_for_control_port(false).await;
    let mut server = Server::new(1024..=65535, None);
    server.set_max_carriers(max_carriers);
    tokio::spawn(server.listen());
    wait_for_control_port(true).await;
}

/// Spawn a public-tunnel client requesting `carriers` parallel carriers. Returns
/// the local listener (the "service") and the public address visitors connect to.
async fn spawn_pool_client(carriers: u16) -> Result<(TcpListener, SocketAddr)> {
    let listener = TcpListener::bind("localhost:0").await?;
    let local_port = listener.local_addr()?.port();
    let options = TunnelOptions {
        carriers,
        ..Default::default()
    };
    let client = Client::new(
        "localhost",
        local_port,
        "localhost",
        0,
        None,
        false,
        options,
        None,
    )
    .await?;
    let remote_addr = ([127, 0, 0, 1], client.remote_port()).into();
    tokio::spawn(client.listen());
    Ok((listener, remote_addr))
}

/// Spawn an echo service on `listener` that echoes a 4-byte message per connection.
fn spawn_echo(listener: TcpListener) {
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
}

/// Drive `n` concurrent connections, each round-tripping its own distinct 4-byte
/// message, asserting no cross-talk across the carrier pool.
async fn drive_concurrent(addr: SocketAddr, n: u32) -> Result<()> {
    let mut handles = Vec::new();
    for i in 0..n {
        handles.push(tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await?;
            let msg = i.to_be_bytes();
            stream.write_all(&msg).await?;
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            assert_eq!(buf, msg, "connection {i} round-tripped the wrong bytes");
            anyhow::Ok(())
        }));
    }
    for h in handles {
        h.await??;
    }
    Ok(())
}

#[tokio::test]
async fn carrier_pool_round_trips_concurrent_connections() -> Result<()> {
    // A pool of 4 carriers carrying 40 concurrent connections: every connection
    // must round-trip its own message intact (proves data flows correctly over all
    // pooled carriers, with the server round-robining across them).
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(8).await;
    let (listener, addr) = spawn_pool_client(4).await?;
    spawn_echo(listener);

    drive_concurrent(addr, 40).await?;
    Ok(())
}

#[tokio::test]
async fn carrier_pool_half_close() -> Result<()> {
    // Half-closing one direction must still work when the data path rides a pooled
    // carrier (native yamux half-close, same guarantee as the single-connection
    // path).
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(8).await;
    let (listener, addr) = spawn_pool_client(3).await?;

    let (mut cli, (mut srv, _)) = tokio::try_join!(TcpStream::connect(addr), listener.accept())?;

    let mut buf = b"message before shutdown".to_vec();
    cli.write_all(&buf).await?;
    cli.shutdown().await?;

    srv.read_exact(&mut buf).await?;
    assert_eq!(buf, b"message before shutdown");
    assert_eq!(srv.read(&mut buf).await?, 0); // EOF on the half-closed direction

    let mut buf = b"hello from the other side!".to_vec();
    srv.write_all(&buf).await?;
    cli.read_exact(&mut buf).await?;
    assert_eq!(buf, b"hello from the other side!");

    Ok(())
}

#[tokio::test]
async fn carrier_pool_large_payload() -> Result<()> {
    // A payload larger than the copy buffers must arrive byte-for-byte over a
    // pooled carrier (guards buffer sizing / bidirectional copy on the pool path).
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(8).await;
    let (listener, addr) = spawn_pool_client(4).await?;

    const LEN: usize = 1 << 20; // 1 MiB
    let payload: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();

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
async fn carrier_pool_request_above_cap_still_works() -> Result<()> {
    // A client asking for more carriers than the server's `--max-carriers` must be
    // clamped, not rejected: the tunnel still serves traffic over the capped pool.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(2).await; // cap = 2
    let (listener, addr) = spawn_pool_client(16).await?; // asks for 16
    spawn_echo(listener);

    drive_concurrent(addr, 20).await?;
    Ok(())
}

#[tokio::test]
async fn carrier_pool_disabled_server_side_degrades_to_single() -> Result<()> {
    // With the server cap at 1 the pool is disabled server-side: the client gets
    // `extra = 0`, opens no extra carriers, and the tunnel works as a single
    // connection — a request for a pool must never break the tunnel.
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(1).await;
    let (listener, addr) = spawn_pool_client(8).await?;
    spawn_echo(listener);

    drive_concurrent(addr, 10).await?;
    Ok(())
}
