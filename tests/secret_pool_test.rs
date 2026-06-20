//! End-to-end tests for the secret-tunnel **carrier pools** and the **native-QUIC
//! direct path**.
//!
//! - Relay provider pool (`bore local --tcp-secret-id --carriers N`): the server
//!   round-robins relayed substreams across the provider's N connections.
//! - Relay consumer pool (`bore proxy --carriers N`): the consumer spreads its
//!   forwarded substreams across N connections to the server.
//! - The UDP direct path multiplexes proxied connections over **native QUIC
//!   streams** (one bidi per connection), not yamux-over-one-stream.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use bore_cli::{
    client::{Client, ProviderMeta},
    secret::Proxy,
    server::Server,
    shared::CONTROL_PORT,
};
use lazy_static::lazy_static;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
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

async fn spawn_server() {
    wait_for_control_port(false).await;
    tokio::spawn(Server::new(1024..=65535, None).listen());
    wait_for_control_port(true).await;
}

/// Echo service: echoes whatever each connection sends, until EOF.
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

/// Spawn a secret-tunnel provider with `provider_carriers` parallel relay carriers.
#[allow(clippy::too_many_arguments)]
async fn spawn_provider(id: &str, echo_port: u16, provider_carriers: u16) -> Result<()> {
    let provider = Client::new_secret_provider(
        "localhost",
        echo_port,
        "localhost",
        id,
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1024,
        provider_carriers,
        ProviderMeta::default(),
        None,
    )
    .await?;
    tokio::spawn(provider.listen());
    Ok(())
}

/// Drive `n` concurrent connections through `addr`, each round-tripping its own
/// distinct 4-byte message (asserts no cross-talk across the pool).
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_pool_relays_concurrent_connections() -> Result<()> {
    // A provider with 4 relay carriers; the server round-robins relayed substreams
    // across the provider's 4 connections. Many concurrent connections must each
    // round-trip intact.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;
    spawn_provider("prov-pool", echo_port, 4).await?;

    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        "prov-pool",
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        1, // carriers
        None,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    time::sleep(Duration::from_millis(50)).await;

    drive_concurrent(addr, 40).await?;
    Ok(())
}

/// Spawn a consumer (`bore proxy`) with `consumer_carriers` parallel relay carriers.
async fn spawn_consumer(id: &str, consumer_carriers: u16) -> Result<SocketAddr> {
    let proxy = Proxy::new(
        "localhost",
        "127.0.0.1:0".parse()?,
        id,
        None,
        false,
        false,
        None,
        false,
        false,
        0,
        0, // release timeout
        consumer_carriers,
        None,
        false,
    )
    .await?;
    let addr = proxy.local_addr()?;
    tokio::spawn(proxy.listen());
    Ok(addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_pool_relays_concurrent_connections() -> Result<()> {
    // A single consumer with 4 relay carriers spreads its forwarded substreams
    // across 4 connections to the server. Many concurrent connections must each
    // round-trip intact (no cross-talk across the consumer's carrier pool).
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;
    spawn_provider("cons-pool", echo_port, 1).await?;
    let addr = spawn_consumer("cons-pool", 4).await?;
    time::sleep(Duration::from_millis(50)).await;

    drive_concurrent(addr, 40).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_pools_relay_concurrent_connections() -> Result<()> {
    // Provider pool *and* consumer pool together: both the consumer→server and
    // server→provider relay legs are spread across multiple connections.
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server().await;
    let echo_port = spawn_echo_service().await?;
    spawn_provider("both-pool", echo_port, 3).await?;
    let addr = spawn_consumer("both-pool", 3).await?;
    time::sleep(Duration::from_millis(50)).await;

    drive_concurrent(addr, 40).await?;
    Ok(())
}
