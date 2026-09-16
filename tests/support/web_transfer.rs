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
                    use tokio::io::AsyncWriteExt;
                    let (mut ri, mut wi) = inbound.into_split();
                    let (mut ro, mut wo) = outbound.into_split();
                    // Each direction SHUTS DOWN the far write half at EOF. A
                    // proxy that swallows the FIN turns "the server closed
                    // the response" into "the client waits forever", which
                    // is a bug in this helper that would read as a bug in
                    // the product.
                    let _ = tokio::join!(
                        async {
                            let _ = tokio::io::copy(&mut ri, &mut wo).await;
                            let _ = wo.shutdown().await;
                        },
                        async {
                            let _ = tokio::io::copy(&mut ro, &mut wi).await;
                            let _ = wi.shutdown().await;
                        },
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

    /// [`WsPeer::connect`] from a chosen loopback source address.
    ///
    /// The pre-auth limiter is per IP (10/minute, burst 20), which is the
    /// product refusing a single host that opens rooms' worth of sockets. A
    /// load test needs many peers, and many peers are many addresses: every
    /// one of 127.0.0.0/8 is local, so each test peer dials from its own.
    pub async fn connect_from(
        local: std::net::Ipv4Addr,
        host: &str,
        room_hex: &str,
        origin: &str,
    ) -> Result<Self> {
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
        let socket = tokio::net::TcpSocket::new_v4()?;
        socket.bind(std::net::SocketAddr::from((local, 0)))?;
        let stream = socket.connect(host.parse()?).await?;
        let (ws, response) = tokio_tungstenite::client_async(
            request,
            tokio_tungstenite::MaybeTlsStream::Plain(stream),
        )
        .await?;
        anyhow::ensure!(
            response.status()
                == tokio_tungstenite::tungstenite::http::StatusCode::SWITCHING_PROTOCOLS,
            "expected 101, got {}",
            response.status()
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

    /// Drains until the server CLOSES, returning the close code it sent.
    ///
    /// A refusal is a close code, not a message, so a gate about refusals has
    /// to read the frame the other reader deliberately swallows. `None` means
    /// the transport died without one, which is a different answer and must
    /// not be folded into the same bucket.
    pub async fn close_code(&mut self, wait: Duration) -> Result<Option<u16>> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            let next = tokio::time::timeout(wait, self.ws.next()).await?;
            match next {
                None => return Ok(None),
                Some(Err(_)) => return Ok(None),
                Some(Ok(Message::Close(frame))) => {
                    return Ok(frame.map(|f| u16::from(f.code)));
                }
                Some(Ok(_)) => continue,
            }
        }
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

/// Phase 4 made `transfer.source_ready` open the DIRECT attempt, so a test
/// that wants the relay declines the direct path exactly as a browser with
/// no usable DataChannel does: it answers `transfer.direct_start` with
/// `transfer.direct_failed {reason:"unsupported"}` and the server falls back.
///
/// Consumes both `transfer.direct_start` envelopes, the decline's own ack
/// and the counterpart's forwarded notice, leaving each peer's next message
/// its own `transfer.relay_ticket`. Returns nothing: the FRESH attempt ID
/// the tickets are bound to rides on the tickets themselves, and reading it
/// from there is what proves the fallback minted a new one.
pub async fn decline_direct(
    source: &mut WsPeer,
    recipient: &mut WsPeer,
    transfer_id: &str,
    attempt_id: &str,
    request_id: &str,
    wait: Duration,
) -> Result<()> {
    async fn expect(peer: &mut WsPeer, typ: &str, wait: Duration) -> Result<serde_json::Value> {
        let text = peer
            .next_text(wait)
            .await?
            .ok_or_else(|| anyhow::anyhow!("control closed before {typ}"))?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        anyhow::ensure!(
            value["type"].as_str() == Some(typ),
            "expected {typ}, got {text}"
        );
        Ok(value["body"].clone())
    }
    // The recipient is told first (it is the offerer), but each peer has its
    // own queue, so the two reads are independent.
    let start = expect(recipient, "transfer.direct_start", wait).await?;
    anyhow::ensure!(start["role"].as_str() == Some("offerer"), "{start}");
    let start = expect(source, "transfer.direct_start", wait).await?;
    anyhow::ensure!(start["role"].as_str() == Some("answerer"), "{start}");
    source
        .send_text(
            serde_json::json!({
                "v": 1,
                "type": "transfer.direct_failed",
                "requestId": request_id,
                "body": {
                    "transferId": transfer_id,
                    "attemptId": attempt_id,
                    "reason": "unsupported",
                },
            })
            .to_string(),
        )
        .await?;
    let _ = expect(source, "ack", wait).await?;
    let notice = expect(recipient, "transfer.direct_failed", wait).await?;
    anyhow::ensure!(notice["reason"].as_str() == Some("unsupported"), "{notice}");
    Ok(())
}

/// One relay-leg socket (Phase 3.2): the same handshake gate as control
/// (exact `Origin`, `bore-transfer-v1`) on `/transfer/ws/relay/{room}/{id}`,
/// then raw message access — the tests speak attach/binary/close by hand.
pub struct RelayLeg {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl RelayLeg {
    /// Handshakes the relay route; asserts the 101 and subprotocol echo.
    pub async fn connect(
        host: &str,
        room_hex: &str,
        transfer_hex: &str,
        origin: &str,
    ) -> Result<Self> {
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
        let url = format!("ws://{host}/transfer/ws/relay/{room_hex}/{transfer_hex}");
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
        Ok(Self { ws })
    }

    /// Sends one text message (the attach).
    pub async fn send_text(&mut self, text: String) -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        self.ws.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Sends one binary message (one ciphertext frame).
    pub async fn send_binary(&mut self, bytes: Vec<u8>) -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        self.ws.send(Message::Binary(bytes.into())).await?;
        Ok(())
    }

    /// Sends a normal close and flushes it.
    pub async fn close(&mut self) -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        self.ws.send(Message::Close(None)).await?;
        Ok(())
    }

    /// Next message of any kind within `wait`; `None` on timeout, close or
    /// transport break. Ping/pong are answered, never returned.
    pub async fn next_msg(
        &mut self,
        wait: Duration,
    ) -> Result<Option<tokio_tungstenite::tungstenite::Message>> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        loop {
            let next = match tokio::time::timeout(wait, self.ws.next()).await {
                Ok(next) => next,
                Err(_) => return Ok(None),
            };
            match next {
                None => return Ok(None),
                Some(Err(_)) => return Ok(None),
                Some(Ok(Message::Ping(payload))) => {
                    self.ws.send(Message::Pong(payload)).await?;
                    continue;
                }
                Some(Ok(Message::Pong(_))) => continue,
                Some(Ok(message)) => return Ok(Some(message)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Benchmark reporting (3.9, T-WEB-PERF)
// ---------------------------------------------------------------------------

/// MiB/s for `bytes` moved in `elapsed`. Returns `None` for a zero-length
/// measurement: a rate with no time behind it is not a slow number, it is an
/// absent one, and a `0.0` would enter a median as if it had been measured
/// (the campaign rule: never publish a failed arm as a number).
pub fn throughput_mib_s(bytes: u64, elapsed: std::time::Duration) -> Option<f64> {
    if bytes == 0 || elapsed.is_zero() {
        return None;
    }
    Some((bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64())
}

/// Median of the samples, computed on a numeric copy — never on formatted
/// text. `sort -n` is locale-dependent and silently corrupts a median under a
/// comma-decimal locale (V-11); sorting `f64` here cannot be.
pub fn median(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a measured sample"));
    let mid = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

/// One reported line: every raw sample beside the median, because a file that
/// prints only medians hides exactly the bug that corrupts them (V-11) and
/// cannot be re-checked by anyone reading it later.
pub fn bench_line(arm: &str, size_mib: u64, samples: &[f64]) -> String {
    let raw = samples
        .iter()
        .map(|s| format!("{s:.2}"))
        .collect::<Vec<_>>()
        .join(" ");
    match median(samples) {
        Some(m) => format!("PERF {arm} size={size_mib}MiB median={m:.2}MiB/s samples=[{raw}]"),
        None => format!("PERF {arm} size={size_mib}MiB median=FAILED samples=[]"),
    }
}

// ---------------------------------------------------------------------------
// Sub-phase 6.1 — soak harness
// ---------------------------------------------------------------------------

/// The control socket type both peer helpers wrap.
type ControlWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A control peer whose socket is DRAINED by a background task.
///
/// The server's per-peer outgoing queue holds 64 messages. A soak with 32
/// peers publishing 64 offers each broadcasts about two thousand events to
/// every one of them, so a peer that reads only the message it is waiting for
/// fills that queue and is dropped for being slow — the run would then
/// measure the harness rather than the server. The task reads everything the
/// socket delivers and the test takes what it needs out of the backlog.
pub struct PumpedPeer {
    sink: futures_util::stream::SplitSink<ControlWs, tokio_tungstenite::tungstenite::Message>,
    inbox: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    /// Messages taken off the backlog while looking for another type.
    pub skipped: u64,
    /// Messages the draining task read and did not keep.
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// The draining task; aborted on drop (see `Drop`).
    reader: tokio::task::JoinHandle<()>,
}

/// Dropping the peer must CLOSE its socket.
///
/// The reading half lives in the task, so dropping only the sink leaves the
/// connection open and the server still counting the peer — which reads, from
/// a test that then re-admits peers, exactly like a leaked permit. Aborting
/// the task drops the stream, and with both halves gone the socket closes.
impl Drop for PumpedPeer {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl WsPeer {
    /// Hands the socket to a draining task, keeping only the listed message
    /// types (see [`PumpedPeer`]).
    ///
    /// The draining task must read everything the server sends, but it does
    /// not have to REMEMBER it: a soak broadcasts tens of thousands of
    /// catalog events that no assertion reads, and keeping them would make
    /// the harness the memory story instead of the server. An empty list
    /// keeps everything.
    pub fn into_pumped_keeping(self, keep: &[&str]) -> PumpedPeer {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        let keep: Vec<String> = keep.iter().map(|t| (*t).to_string()).collect();
        let (sink, mut stream) = self.ws.split();
        let (tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                if let Message::Text(text) = message {
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(value) => {
                            let typ = value["type"].as_str().unwrap_or_default();
                            // `error` is never dropped: a refusal under load
                            // IS the result of a load test.
                            if !keep.is_empty() && typ != "error" && !keep.iter().any(|k| k == typ)
                            {
                                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                continue;
                            }
                            if tx.send(value).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
        PumpedPeer {
            sink,
            inbox,
            skipped: 0,
            dropped,
            reader,
        }
    }
}

impl PumpedPeer {
    /// Sends one application text message.
    pub async fn send_text(&mut self, text: String) -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        self.sink.send(Message::Text(text.into())).await?;
        Ok(())
    }

    /// Application-level keepalive: the control reaper drops a peer after 60 s
    /// of silence, and a soak peer can legitimately have nothing to say for
    /// longer than that.
    pub async fn ping(&mut self) -> Result<()> {
        self.send_text(r#"{"v":1,"type":"ping","body":{}}"#.to_string())
            .await
    }

    /// Messages read by the draining task and not kept.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Next message of `typ` within `wait`, skipping (and counting) whatever
    /// else the backlog holds. An `error` envelope is never skipped: the
    /// control plane refusing the load IS the result.
    pub async fn expect(&mut self, typ: &str, wait: Duration) -> Result<serde_json::Value> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let value = tokio::time::timeout_at(deadline, self.inbox.recv())
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for {typ}"))?
                .ok_or_else(|| anyhow::anyhow!("control closed before {typ}"))?;
            let got = value["type"].as_str().unwrap_or_default().to_string();
            if got == typ {
                return Ok(value["body"].clone());
            }
            anyhow::ensure!(
                got != "error",
                "control error while waiting for {typ}: {value}"
            );
            self.skipped += 1;
        }
    }
}
