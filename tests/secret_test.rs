use std::time::Duration;

use anyhow::Result;
use bore_cli::{client::Client, server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
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
    let provider =
        Client::new_secret_provider("localhost", port, "localhost", "svc-a", Some("s3cr3t")).await;
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
    let first =
        Client::new_secret_provider("localhost", port, "localhost", "dup", Some("s3cr3t")).await?;
    tokio::spawn(first.listen()); // keep the registration alive
    time::sleep(Duration::from_millis(50)).await;

    let second =
        Client::new_secret_provider("localhost", port, "localhost", "dup", Some("s3cr3t")).await;
    assert!(second.is_err(), "duplicate tcp-secret-id must be rejected");

    Ok(())
}

#[tokio::test]
async fn secret_registration_requires_correct_secret() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    spawn_server(Some("right")).await;

    let (_local, port) = local_port().await?;
    let wrong =
        Client::new_secret_provider("localhost", port, "localhost", "svc", Some("wrong")).await;
    assert!(wrong.is_err(), "wrong secret must be rejected");

    let missing = Client::new_secret_provider("localhost", port, "localhost", "svc2", None).await;
    assert!(missing.is_err(), "missing secret must be rejected");

    Ok(())
}
