//! Shared harness for `tests/web_transfer_test.rs`: ephemeral ports, valid
//! flag sets and in-process servers with the web-transfer registry enabled.
//! All tests pick dynamic ports and run serially (`--test-threads=1`).

use anyhow::Result;
use bore_cli::{
    server::Server,
    web_transfer::{resolve_server_config, WebTransferRegistry, WebTransferServerArgs},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

/// A reserved loopback port (bind-then-drop; small reuse race, retried by
/// callers via `wait_port`).
pub async fn free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}

/// Waits until `port` accepts (or stops accepting) on 127.0.0.1.
pub async fn wait_port(port: u16, listening: bool) {
    for _ in 0..500 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() == listening {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Flag set enabling the service on loopback HTTP with exact defaults.
pub fn enabled_args() -> WebTransferServerArgs {
    enabled_args_with_grace(bore_cli::web_transfer::WebTransferLimits::default().owner_grace_secs)
}

/// Flag set enabling the service with a chosen owner grace (tests only).
pub fn enabled_args_with_grace(owner_grace_secs: u64) -> WebTransferServerArgs {
    WebTransferServerArgs {
        base_url: Some("http://127.0.0.1:8080/".to_string()),
        owner_grace_secs,
        ..WebTransferServerArgs::default()
    }
}

/// Starts a real in-process server with the web-transfer registry enabled on
/// `control_port`. Returns the shared registry (for state assertions) once
/// the port accepts; the server task is detached like the other suites do.
pub async fn spawn_enabled_server(control_port: u16) -> Result<Arc<WebTransferRegistry>> {
    let mut server = Server::new(1024..=65535, None);
    server.set_control_port(control_port);
    let config = resolve_server_config(&enabled_args(), false, control_port)?
        .expect("loopback config resolves");
    server.set_web_transfer(config)?;
    let registry = assert_registry_present(&server);
    tokio::spawn(server.listen());
    wait_port(control_port, true).await;
    Ok(registry)
}

/// Shared registry assertion helper: an enabled server owns exactly one
/// registry whose totals are the documented defaults.
pub fn assert_registry_present(server: &Server) -> Arc<WebTransferRegistry> {
    let registry = server.web_transfer().expect("registry enabled");
    assert_eq!(
        registry.totals(),
        bore_cli::web_transfer::WebTransferLimits::default()
    );
    registry
}

/// A loopback TCP proxy the test can kill: breaking the proxy drops both
/// directions of every relayed connection, which is a real transport loss
/// for the owner loop (no root, no timing luck, no production hooks).
pub struct ProxyReset {
    accept: tokio::task::JoinHandle<()>,
    conns: std::sync::Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    port: u16,
}

impl ProxyReset {
    /// Kills the listener and every relayed connection, waiting for their
    /// actual destruction so the port is rebindable the moment this returns
    /// (abort alone is asynchronous and would race a rebind).
    pub async fn kill(self) {
        self.accept.abort();
        let handles: Vec<_> = self.conns.lock().await.drain(..).collect();
        for handle in &handles {
            handle.abort();
        }
        // The lock guard is released before these awaits; the accept task
        // only needs the lock for a push, so no deadlock is possible.
        let _ = self.accept.await;
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Local port the proxy listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Serves `listen_port` by relaying everything to `target_port` (`0` picks
/// an ephemeral port, reported by [`ProxyReset::port`] — no bind-then-drop
/// race between two reserved ports).
pub async fn spawn_proxy(listen_port: u16, target_port: u16) -> Result<ProxyReset> {
    use tokio::net::{TcpListener, TcpStream};
    let listener = TcpListener::bind(("127.0.0.1", listen_port)).await?;
    let port = listener.local_addr()?.port();
    let conns = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let accept = tokio::spawn({
        let conns = std::sync::Arc::clone(&conns);
        async move {
            loop {
                let Ok((inbound, _)) = listener.accept().await else {
                    break;
                };
                let Ok(outbound) = TcpStream::connect(("127.0.0.1", target_port)).await else {
                    continue;
                };
                let handle = tokio::spawn(async move {
                    let (mut ri, mut wi) = inbound.into_split();
                    let (mut ro, mut wo) = outbound.into_split();
                    let _ = tokio::join!(
                        tokio::io::copy(&mut ri, &mut wo),
                        tokio::io::copy(&mut ro, &mut wi),
                    );
                });
                conns.lock().await.push(handle);
            }
        }
    });
    Ok(ProxyReset {
        accept,
        conns,
        port,
    })
}
