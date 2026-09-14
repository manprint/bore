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

/// One real control-WebSocket peer (Phase 2.2): a tungstenite client with the
/// exact `Origin` and subprotocol the server requires, speaking application
/// text messages. Pongs and pings are answered by the library; the test only
/// ever sees text, close and transport errors.
pub struct WsPeer {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl WsPeer {
    /// Handshakes `ws://{host}/transfer/ws/control/{room}` with `Origin` and
    /// `bore-transfer-v1`; asserts the 101 and the subprotocol echo.
    pub async fn connect(host: &str, room_hex: &str, origin: &str) -> Result<Self> {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
        let url = format!("ws://{host}/transfer/ws/control/{room_hex}");
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_str(origin)?);
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("bore-transfer-v1"),
        );
        let (ws, response) = tokio_tungstenite::connect_async(request).await?;
        anyhow::ensure!(
            response.status()
                == tokio_tungstenite::tungstenite::http::StatusCode::SWITCHING_PROTOCOLS,
            "expected 101, got {}",
            response.status()
        );
        let echo = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        anyhow::ensure!(
            echo.split(',')
                .map(str::trim)
                .any(|t| t == "bore-transfer-v1"),
            "missing subprotocol echo in {response:?}"
        );
        Ok(Self { ws })
    }

    /// Sends one application text message.
    pub async fn send_text(&mut self, text: String) -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        self.ws.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Sends `hello` (no `requestId`, per the protocol).
    pub async fn hello(
        &mut self,
        member_token_hex: &str,
        display_name: Option<&str>,
    ) -> Result<()> {
        let name = display_name
            .map(|n| format!(r#","displayName":{n:?}"#))
            .unwrap_or_default();
        self.send_text(format!(
            r#"{{"v":1,"type":"hello","body":{{"memberToken":"{member_token_hex}"{name}}}}}"#
        ))
        .await
    }

    /// Next application text message within `wait`; `None` means the peer was
    /// closed or the transport broke. Ping/pong/binary frames are skipped.
    pub async fn next_text(&mut self, wait: Duration) -> Result<Option<String>> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            let next = tokio::time::timeout(wait, self.ws.next()).await?;
            match next {
                None => return Ok(None),
                Some(Err(_)) => return Ok(None),
                Some(Ok(Message::Text(text))) => return Ok(Some(text.to_string())),
                Some(Ok(Message::Close(_))) => return Ok(None),
                Some(Ok(_)) => continue,
            }
        }
    }
}
