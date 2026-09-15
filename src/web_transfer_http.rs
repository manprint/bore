//! Web-transfer HTTP surface: same-origin room shell, static assets, the
//! authenticated control-WebSocket handshake and the Phase 2.2 control
//! session actor.
//!
//! The router only handles requests whose `Host` exactly matches the
//! configured [`WebTransferBaseUrl`] authority (case-insensitive hostname,
//! explicit/default port normalized). Anything else returns
//! [`TryServeOutcome::Fallthrough`] so the caller replays the already-read
//! head into the existing vhost-first/admin-fallback chain byte-for-byte.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use crate::prefixed::Prefixed;
use crate::web_transfer::{
    authenticate_hello, build_resync, control_liveness_expired, generate_peer_id, HelloAuth,
    MemberToken, PeerSession, RoomId, WebTransferBaseUrl, WebTransferRegistry,
    WEB_TRANSFER_AUTH_FAIL_DELAY, WEB_TRANSFER_CTRL_SEND_TIMEOUT, WEB_TRANSFER_HELLO_TIMEOUT,
    WEB_TRANSFER_MAX_CONTROL_BYTES, WEB_TRANSFER_PEER_ID_RETRIES, WEB_TRANSFER_REAPER_TICK,
};
use crate::web_transfer_protocol::{
    ack_envelope, canonical_json, check_manifest_byte_cap, error_envelope, error_envelope_anon,
    manifest_value, parse_client_envelope, parse_hello_body, parse_manifest, parse_ping_body,
    parse_publish_body, parse_rename_body, parse_withdraw_body, pong_envelope,
    room_closed_envelope, room_event_message, HelloBody, RequestId, CONTROL_SUBPROTOCOL,
};

/// Exact `Content-Security-Policy` served on every web-transfer response.
pub const WEB_TRANSFER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Read/write buffer for the control WebSocket (bytes).
pub const WS_READ_BUFFER: usize = 4 * 1024;
/// Target write buffer for the control WebSocket (bytes).
pub const WS_WRITE_BUFFER: usize = 4 * 1024;
/// Backpressure ceiling for the control WebSocket write buffer (bytes).
pub const WS_MAX_WRITE_BUFFER: usize = 256 * 1024;
/// Largest accepted control frame/message on the WebSocket (bytes).
pub const WS_MAX_MESSAGE: usize = 320 * 1024;

/// Pinned WebSocket transport bounds: 4 KiB read/write buffers, 256 KiB max
/// write buffer, 320 KiB max frame/message.
pub fn websocket_config() -> WebSocketConfig {
    let mut cfg = WebSocketConfig::default();
    cfg.read_buffer_size = WS_READ_BUFFER;
    cfg.write_buffer_size = WS_WRITE_BUFFER;
    cfg.max_write_buffer_size = WS_MAX_WRITE_BUFFER;
    cfg.max_message_size = Some(WS_MAX_MESSAGE);
    cfg.max_frame_size = Some(WS_MAX_MESSAGE);
    cfg
}

/// Replays an already-read request head before delegating to the inner stream.
///
/// The server reads the head once to classify authority/path, then hands the
/// WHOLE stream (head included) to the WebSocket handshake — never a second
/// socket, never a manual parser.
pub struct ReplayStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> ReplayStream<S> {
    /// Builds a stream yielding `prefix` bytes first, then `inner` bytes.
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ReplayStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ReplayStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Extracts the `Host` header value from a raw request head.
pub fn extract_host(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("host") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Extracts one header value (case-insensitive name) from a raw head.
fn extract_header(head: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.lines().skip(1) {
        if let Some((n, value)) = line.split_once(':') {
            if n.trim().eq_ignore_ascii_case(name) {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Extracts `(method, target)` from the request line.
pub fn parse_request_line(head: &[u8]) -> Option<(String, String)> {
    let line = head
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(&[]);
    let text = String::from_utf8_lossy(line);
    let mut parts = text.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    if method.is_empty() || !target.starts_with('/') {
        return None;
    }
    Some((method, target))
}

/// Strictly splits `host[:port]` into `(hostname, port)`. Bracketed IPv6
/// literals keep their brackets on the hostname side so both sides compare the
/// same shape; malformed brackets, suffixes and ports reject the authority.
fn split_host_port(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() || value.contains([' ', '\t', '@', '/', '?', '#']) {
        return None;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']')?;
        let literal = &rest[..end];
        literal.parse::<std::net::Ipv6Addr>().ok()?;
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            let raw = suffix.strip_prefix(':')?;
            let port = raw.parse::<u16>().ok().filter(|port| *port != 0)?;
            Some(port)
        };
        return Some((format!("[{literal}]"), port));
    }
    if value.contains(['[', ']']) {
        return None;
    }
    match value.matches(':').count() {
        0 => Some((value.to_string(), None)),
        1 => {
            let (host, raw) = value.rsplit_once(':')?;
            if host.is_empty() {
                return None;
            }
            let port = raw.parse::<u16>().ok().filter(|port| *port != 0)?;
            Some((host.to_string(), Some(port)))
        }
        _ => None,
    }
}

/// Whether `Host` exactly equals the configured authority: case-insensitive
/// hostname, explicit/default port normalized (https 443, http 80).
pub fn host_matches_authority(host_header: &str, base_url: &WebTransferBaseUrl) -> bool {
    let host_header = host_header.trim();
    if host_header.is_empty() || host_header.contains([' ', '\t', '@', '/', '?', '#']) {
        return false;
    }
    let origin = base_url.origin();
    let scheme = origin.split("://").next().unwrap_or("");
    let default_port = match scheme {
        "https" => Some(443u16),
        "http" => Some(80u16),
        _ => None,
    };
    let Some((got_host, got_port)) = split_host_port(host_header) else {
        return false;
    };
    let Some((want_host, want_port)) = split_host_port(base_url.authority()) else {
        return false;
    };
    if !got_host.eq_ignore_ascii_case(&want_host) {
        return false;
    }
    match (got_port, want_port) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        (None, Some(b)) => Some(b) == default_port,
        (Some(a), None) => Some(a) == default_port,
    }
}

/// Canonical 32-char lowercase-hex room/transfer ID.
fn is_canonical_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Router decision for a request whose Host already matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebDecision {
    /// Not under `/transfer/` — the caller falls through to vhost/admin.
    NotWeb,
    /// `GET|HEAD /transfer/<room>` — generic shell, 200 even if absent.
    RoomShell {
        /// Canonical room ID from the path (never checked for existence).
        room: String,
    },
    /// `GET|HEAD /transfer/assets/{app.js,app.css,offer-worker.js}`.
    Asset {
        /// One of the three bundled files.
        name: &'static str,
    },
    /// `GET /transfer/ws/control/<room>` — WebSocket handshake.
    ControlWs {
        /// Canonical room ID from the path.
        room: String,
    },
    /// `GET /transfer/ws/relay/<room>/<transfer>` — reserved until Phase 3.
    RelayReserved {
        /// Canonical room ID from the path.
        room: String,
        /// Canonical transfer ID from the path.
        transfer: String,
    },
    /// Syntactically a web path but malformed — generic 404, never fallthrough.
    BadRequest,
    /// Right path, wrong method — 405.
    MethodNotAllowed,
}

/// Classifies `(method, target)` assuming the Host matched. Rejects
/// percent-encoded separators, duplicate slashes, dot segments, query strings
/// on WS endpoints and noncanonical IDs.
pub fn classify(method: &str, target: &str) -> WebDecision {
    let (path, has_query) = match target.find('?') {
        Some(i) => (&target[..i], true),
        None => (target, false),
    };
    if !path.starts_with('/') {
        return WebDecision::BadRequest;
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") {
        return WebDecision::BadRequest;
    }
    if path.contains("//") {
        return WebDecision::BadRequest;
    }
    if path.split('/').any(|seg| seg == "." || seg == "..") {
        return WebDecision::BadRequest;
    }
    if let Some(rest) = path.strip_prefix("/transfer/assets/") {
        if rest == "app.js" || rest == "app.css" || rest == "offer-worker.js" {
            if method != "GET" && method != "HEAD" {
                return WebDecision::MethodNotAllowed;
            }
            let name: &'static str = match rest {
                "app.js" => "app.js",
                "app.css" => "app.css",
                _ => "offer-worker.js",
            };
            return WebDecision::Asset { name };
        }
        return WebDecision::BadRequest;
    }
    if let Some(rest) = path.strip_prefix("/transfer/ws/control/") {
        if rest.is_empty() || rest.contains('/') {
            return WebDecision::BadRequest;
        }
        if has_query || !is_canonical_id(rest) {
            return WebDecision::BadRequest;
        }
        if method != "GET" {
            return WebDecision::MethodNotAllowed;
        }
        return WebDecision::ControlWs {
            room: rest.to_string(),
        };
    }
    if let Some(rest) = path.strip_prefix("/transfer/ws/relay/") {
        let mut parts = rest.split('/');
        let (room, transfer) = (parts.next(), parts.next());
        if parts.next().is_some() {
            return WebDecision::BadRequest;
        }
        match (room, transfer) {
            (Some(room), Some(transfer)) => {
                if has_query || !is_canonical_id(room) || !is_canonical_id(transfer) {
                    return WebDecision::BadRequest;
                }
                if method != "GET" {
                    return WebDecision::MethodNotAllowed;
                }
                return WebDecision::RelayReserved {
                    room: room.to_string(),
                    transfer: transfer.to_string(),
                };
            }
            _ => return WebDecision::BadRequest,
        }
    }
    if let Some(room) = path.strip_prefix("/transfer/") {
        if room.is_empty() || room.contains('/') {
            // `/transfer/` bare or deeper unknown shape — malformed web path.
            return WebDecision::BadRequest;
        }
        if !is_canonical_id(room) {
            return WebDecision::BadRequest;
        }
        if method != "GET" && method != "HEAD" {
            return WebDecision::MethodNotAllowed;
        }
        return WebDecision::RoomShell {
            room: room.to_string(),
        };
    }
    WebDecision::NotWeb
}

/// Outcome of [`try_serve`]: either the request was a web request (handled,
/// including rejections) or it must fall through with its head replayed.
pub enum TryServeOutcome<S> {
    /// The connection was served (static/shell/WS/rejection). Done.
    Handled,
    /// Not a web request — replay `head` into the vhost/admin chain.
    Fallthrough(Prefixed<S>, Vec<u8>),
}

fn response_head(
    code: u16,
    content_type: &str,
    body_len: usize,
    control_hsts: Option<&str>,
) -> String {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let hsts = control_hsts
        .map(|value| format!("Strict-Transport-Security: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {body_len}\r\n\
         Cache-Control: no-cache\r\n\
         Content-Security-Policy: {csp}\r\n\
         Referrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Permissions-Policy: camera=(), microphone=(), geolocation=()\r\n\
         {hsts}\
         Connection: close\r\n\r\n",
        csp = WEB_TRANSFER_CSP,
    )
}

async fn write_web_response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    code: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
    control_hsts: Option<&str>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let head = response_head(code, content_type, body.len(), control_hsts);
    stream.write_all(head.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn shell_bytes() -> (&'static [u8], &'static str) {
    match crate::web_transfer::WEB_TRANSFER_ASSETS
        .iter()
        .find(|(url, _, _)| *url == "/transfer/assets/index.html")
    {
        Some((_, bytes, content_type)) => (*bytes, *content_type),
        None => (&[], "text/html; charset=utf-8"),
    }
}

fn asset_bytes(name: &str) -> Option<(&'static [u8], &'static str)> {
    let want = match name {
        "app.js" => "/transfer/assets/app.js",
        "app.css" => "/transfer/assets/app.css",
        "offer-worker.js" => "/transfer/assets/offer-worker.js",
        _ => return None,
    };
    crate::web_transfer::WEB_TRANSFER_ASSETS
        .iter()
        .find(|(url, _, _)| *url == want)
        .map(|(_, bytes, content_type)| (*bytes, *content_type))
}

/// Whether the offered `Sec-WebSocket-Protocol` value negotiates the exact
/// control subprotocol.
pub fn negotiates_control_subprotocol(value: Option<&str>) -> bool {
    match value {
        Some(v) => v
            .split(',')
            .map(str::trim)
            .any(|t| t == CONTROL_SUBPROTOCOL),
        None => false,
    }
}

fn reject_upgrade(value: Option<&str>) -> ErrorResponse {
    let _ = value;
    Response::builder()
        .status(400)
        .body(None)
        .expect("400 response builds")
}

/// Serves one already-read request head when the registry is enabled.
/// Returns [`TryServeOutcome::Handled`] for every web-authority `/transfer/`
/// path (including generic rejections) and [`TryServeOutcome::Fallthrough`]
/// otherwise, so legacy routing stays byte-for-byte. `peer` is the outer
/// TCP/TLS peer address (pre-auth rate key, never logged).
pub async fn try_serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: Prefixed<S>,
    head: Vec<u8>,
    registry: &WebTransferRegistry,
    control_hsts: Option<&str>,
    peer: SocketAddr,
) -> TryServeOutcome<S> {
    let Some((method, target)) = parse_request_line(&head) else {
        return TryServeOutcome::Fallthrough(stream, head);
    };
    let Some(host) = extract_host(&head) else {
        return TryServeOutcome::Fallthrough(stream, head);
    };
    let base_url = registry.config().base_url.clone();
    if !host_matches_authority(&host, &base_url) {
        return TryServeOutcome::Fallthrough(stream, head);
    }
    match classify(&method, &target) {
        WebDecision::NotWeb => TryServeOutcome::Fallthrough(stream, head),
        WebDecision::BadRequest => {
            let _ =
                write_web_response(&mut stream, 404, "text/plain", b"", false, control_hsts).await;
            TryServeOutcome::Handled
        }
        WebDecision::MethodNotAllowed => {
            let _ =
                write_web_response(&mut stream, 405, "text/plain", b"", false, control_hsts).await;
            TryServeOutcome::Handled
        }
        WebDecision::RoomShell { room: _ } => {
            // Existence is never revealed: every syntactically valid ID gets
            // the same generic shell with 200, present or absent.
            let (body, content_type) = shell_bytes();
            let head_only = method == "HEAD";
            let _ = write_web_response(
                &mut stream,
                200,
                content_type,
                body,
                head_only,
                control_hsts,
            )
            .await;
            TryServeOutcome::Handled
        }
        WebDecision::Asset { name } => {
            match asset_bytes(name) {
                Some((body, content_type)) => {
                    let head_only = method == "HEAD";
                    let _ = write_web_response(
                        &mut stream,
                        200,
                        content_type,
                        body,
                        head_only,
                        control_hsts,
                    )
                    .await;
                }
                None => {
                    let _ = write_web_response(
                        &mut stream,
                        404,
                        "text/plain",
                        b"",
                        false,
                        control_hsts,
                    )
                    .await;
                }
            }
            TryServeOutcome::Handled
        }
        WebDecision::RelayReserved {
            room: _,
            transfer: _,
        } => {
            // Reserved for the Phase 3 opaque relay — 404, never fallthrough,
            // so the shape is web-owned from the start.
            let _ =
                write_web_response(&mut stream, 404, "text/plain", b"", false, control_hsts).await;
            TryServeOutcome::Handled
        }
        WebDecision::ControlWs { room } => {
            // Generic pre-handshake gate: exact Origin, exact subprotocol and
            // a well-formed upgrade — one 400 for every failure shape so no
            // failure oracles anything about the room.
            let origin = extract_header(&head, "origin");
            let protocol = extract_header(&head, "sec-websocket-protocol");
            if origin.as_deref() != Some(base_url.origin())
                || !negotiates_control_subprotocol(protocol.as_deref())
            {
                let _ =
                    write_web_response(&mut stream, 400, "text/plain", b"", false, control_hsts)
                        .await;
                return TryServeOutcome::Handled;
            }
            let replay = ReplayStream::new(head, stream);
            let subprotocol = CONTROL_SUBPROTOCOL.to_string();
            #[allow(clippy::result_large_err)]
            let callback =
                move |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
                    let offered = req
                        .headers()
                        .get("Sec-WebSocket-Protocol")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if !offered.split(',').map(str::trim).any(|t| t == subprotocol) {
                        return Err(reject_upgrade(None));
                    }
                    resp.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        subprotocol.parse().map_err(|_| reject_upgrade(None))?,
                    );
                    Ok(resp)
                };
            match tokio_tungstenite::accept_hdr_async_with_config(
                replay,
                callback,
                Some(websocket_config()),
            )
            .await
            {
                Ok(ws) => {
                    // Classified as canonical hex above; the parse is
                    // infallible in practice — defend without panicking.
                    let Ok(room_id) = room.parse::<RoomId>() else {
                        let mut ws = ws;
                        close_ws(&mut ws, WS_CLOSE_AUTH).await;
                        return TryServeOutcome::Handled;
                    };
                    serve_control_websocket(ws, registry.clone(), room_id, peer).await;
                    TryServeOutcome::Handled
                }
                Err(_) => TryServeOutcome::Handled,
            }
        }
    }
}

/// Private WebSocket close codes on the control path.
pub const WS_CLOSE_AUTH: u16 = 4001;
/// Room unavailable after a valid authentication.
pub const WS_CLOSE_GONE: u16 = 4004;
/// Rate or capacity refusal.
pub const WS_CLOSE_RATE: u16 = 4008;
/// Protocol version refusal.
pub const WS_CLOSE_VERSION: u16 = 4010;

fn close_reason(code: u16) -> &'static str {
    match code {
        WS_CLOSE_AUTH => "unauthorized",
        WS_CLOSE_GONE => "room unavailable",
        WS_CLOSE_RATE => "rate limited",
        WS_CLOSE_VERSION => "version",
        _ => "error",
    }
}

/// Best-effort WebSocket close; failures mean the peer is already gone.
async fn close_ws<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>, code: u16) {
    let _ = ws
        .close(Some(CloseFrame {
            code: CloseCode::Library(code),
            reason: close_reason(code).into(),
        }))
        .await;
}

/// Bounded direct write to the socket; `false` means the peer is too slow
/// and the session must end (its `PeerGuard` then cleans up).
async fn bounded_ws_send<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    text: String,
) -> bool {
    matches!(
        timeout(
            WEB_TRANSFER_CTRL_SEND_TIMEOUT,
            ws.send(Message::Text(text.into())),
        )
        .await,
        Ok(Ok(()))
    )
}

/// Bounded enqueue on a session's outgoing queue; `false` means the peer is
/// too slow and the session must end.
async fn queue_send(tx: &tokio::sync::mpsc::Sender<String>, text: String) -> bool {
    matches!(
        timeout(WEB_TRANSFER_CTRL_SEND_TIMEOUT, tx.send(text)).await,
        Ok(Ok(()))
    )
}

/// First-message verdict: only a valid `hello` authenticates.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstMessage {
    /// A well-formed `hello` candidate.
    Hello(HelloBody),
    /// Right shape, wrong `v`: answer `UNSUPPORTED_VERSION`, stay connected.
    VersionMismatch,
    /// Anything else first: uniform `4001` close after the auth delay.
    Invalid,
}

/// Classifies the first control message without allocating a session.
/// Oversized input is dropped before parsing.
pub fn classify_first_message(raw: &str) -> FirstMessage {
    if raw.len() > WEB_TRANSFER_MAX_CONTROL_BYTES {
        return FirstMessage::Invalid;
    }
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return FirstMessage::Invalid,
    };
    if value.get("v").and_then(serde_json::Value::as_u64)
        != Some(u64::from(crate::web_transfer_protocol::PROTOCOL_VERSION))
    {
        return FirstMessage::VersionMismatch;
    }
    let env = match parse_client_envelope(raw) {
        Ok(env) => env,
        Err(_) => return FirstMessage::Invalid,
    };
    if env.typ != "hello" {
        return FirstMessage::Invalid;
    }
    match parse_hello_body(&env) {
        Ok(hello) => FirstMessage::Hello(hello),
        Err(_) => FirstMessage::Invalid,
    }
}

/// Lenient `(version, requestId)` peek for failure paths: version-gated
/// errors echo the `requestId` when the offending message carried a valid
/// one, and travel without it otherwise.
fn lenient_meta(raw: &str) -> (Option<u64>, Option<RequestId>) {
    let value: Option<serde_json::Value> = serde_json::from_str(raw).ok();
    let version = value
        .as_ref()
        .and_then(|v| v.get("v"))
        .and_then(serde_json::Value::as_u64);
    let request_id = value
        .as_ref()
        .and_then(|v| v.get("requestId"))
        .and_then(serde_json::Value::as_str)
        .and_then(|s| s.parse::<RequestId>().ok());
    (version, request_id)
}

/// Serves one authenticated control session: hello (10 s), then the
/// single-task actor loop (socket reads, bounded outgoing queue, broadcast
/// events, 500 ms reaper tick). Returning drops the `PeerSession`, whose
/// `PeerGuard` removes exactly this peer and releases its permits.
pub async fn serve_control_websocket<S: AsyncRead + AsyncWrite + Unpin>(
    mut ws: WebSocketStream<S>,
    registry: WebTransferRegistry,
    room_id: RoomId,
    peer: SocketAddr,
) {
    // Phase A: the first message must be `hello` within the deadline. Every
    // pre-auth failure shares one 4008 (rate) or delay+4001 shape so absent
    // room, bad token and exhausted caps are indistinguishable.
    let hello = loop {
        let raw = match timeout(WEB_TRANSFER_HELLO_TIMEOUT, ws.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => text.to_string(),
            _ => {
                close_ws(&mut ws, WS_CLOSE_AUTH).await;
                return;
            }
        };
        if !registry.check_pre_auth(peer.ip()) {
            close_ws(&mut ws, WS_CLOSE_RATE).await;
            return;
        }
        match classify_first_message(&raw) {
            FirstMessage::Hello(hello) => break hello,
            FirstMessage::VersionMismatch => {
                if !bounded_ws_send(&mut ws, error_envelope_anon("UNSUPPORTED_VERSION", None)).await
                {
                    return;
                }
            }
            FirstMessage::Invalid => {
                sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
                close_ws(&mut ws, WS_CLOSE_AUTH).await;
                return;
            }
        }
    };
    // The raw token is hashed and dropped here; it never leaves this scope
    // (`MemberToken` itself is `Copy`, so the parsed value needs no drop —
    // the secret is the `String` inside `hello`, released below).
    let token: MemberToken = match hello.member_token.parse() {
        Ok(token) => token,
        Err(_) => {
            sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
            close_ws(&mut ws, WS_CLOSE_AUTH).await;
            return;
        }
    };
    let room = match authenticate_hello(&registry, room_id, &token) {
        HelloAuth::Ok { room } => room,
        HelloAuth::Deny => {
            sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
            close_ws(&mut ws, WS_CLOSE_AUTH).await;
            return;
        }
        HelloAuth::Gone => {
            close_ws(&mut ws, WS_CLOSE_GONE).await;
            return;
        }
    };
    let display_name = hello.display_name.clone();
    drop(hello);
    // The name is pre-validated so an in-loop `INVALID_MESSAGE` from the join
    // can only mean a peer-ID collision (bounded retries); caps and poison
    // stay uniform denies.
    if let Some(ref name) = display_name {
        if crate::web_transfer::normalize_display_name(name).is_err() {
            sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
            close_ws(&mut ws, WS_CLOSE_AUTH).await;
            return;
        }
    }
    let mut established = None;
    for _ in 0..WEB_TRANSFER_PEER_ID_RETRIES {
        match PeerSession::establish(&registry, &room, generate_peer_id(), display_name.clone()) {
            Ok(ok) => {
                established = Some(ok);
                break;
            }
            Err(e) if e.code() == "INVALID_MESSAGE" => continue,
            Err(_) => {
                sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
                close_ws(&mut ws, WS_CLOSE_AUTH).await;
                return;
            }
        }
    }
    let Some((mut session, mut out_rx, initial)) = established else {
        sleep(WEB_TRANSFER_AUTH_FAIL_DELAY).await;
        close_ws(&mut ws, WS_CLOSE_AUTH).await;
        return;
    };
    // Initial messages bypass the queue straight to the socket: the queue has
    // no drainer until the loop below starts, so parking on it here could
    // never complete.
    for message in initial {
        if !bounded_ws_send(&mut ws, message).await {
            return;
        }
    }
    // Phase B: single-task actor. No producer awaits while holding
    // `RoomState`: joins/renames lock, clone, unlock, then broadcast.
    let mut ticker = interval(WEB_TRANSFER_REAPER_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            inbound = ws.next() => {
                let Some(message) = inbound else { break };
                match message {
                    Err(_) => break,
                    Ok(Message::Text(text)) => {
                        if !handle_text(&registry, &mut session, &text).await {
                            break;
                        }
                    }
                    Ok(Message::Binary(_)) => {
                        // Binary carries no envelope channel: report, then
                        // close with the stable code.
                        let tx = session.sender();
                        let _ = queue_send(&tx, error_envelope_anon("INVALID_MESSAGE", None)).await;
                        close_ws(&mut ws, WS_CLOSE_AUTH).await;
                        break;
                    }
                    Ok(Message::Close(_)) => {
                        let _ = ws.close(None).await;
                        break;
                    }
                    Ok(_) => {
                        session.touch(Instant::now());
                    }
                }
            }
            outgoing = out_rx.recv() => {
                match outgoing {
                    Some(message) => {
                        if !bounded_ws_send(&mut ws, message).await {
                            break;
                        }
                    }
                    None => break,
                }
            }
            event = session.events_mut().recv() => {
                match event {
                    Ok(crate::web_transfer::RoomEvent::RoomClosed { reason }) => {
                        let tx = session.sender();
                        let _ = queue_send(&tx, room_closed_envelope(reason)).await;
                        let _ = ws.close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: reason.into(),
                        })).await;
                        break;
                    }
                    Ok(other) => {
                        let revision = match &other {
                            crate::web_transfer::RoomEvent::PeerJoined { revision, .. }
                            | crate::web_transfer::RoomEvent::PeerRenamed { revision, .. }
                            | crate::web_transfer::RoomEvent::PeerLeft { revision, .. }
                            | crate::web_transfer::RoomEvent::OfferAdded { revision, .. }
                            | crate::web_transfer::RoomEvent::OfferRemoved { revision, .. } => {
                                *revision
                            }
                            crate::web_transfer::RoomEvent::RoomClosed { .. } => {
                                session.revision_seen()
                            }
                        };
                        if let Some(message) = room_event_message(&other) {
                            session.set_revision_seen(revision);
                            let tx = session.sender();
                            if !queue_send(&tx, message).await {
                                break;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Too slow for incremental events: one fresh snapshot,
                        // one message per entry — never a larger aggregate.
                        match build_resync(session.room()) {
                            Ok(messages) => {
                                let mut revision = session.revision_seen();
                                let mut healthy = true;
                                for message in messages {
                                    if message.contains("\"snapshot.end\"") {
                                        if let Ok(env) = crate::web_transfer_protocol::parse_server_envelope(&message) {
                                            if let Some(rev) = env.body.get("revision").and_then(serde_json::Value::as_u64) {
                                                revision = rev;
                                            }
                                        }
                                    }
                                    let tx = session.sender();
                                    if !queue_send(&tx, message).await {
                                        healthy = false;
                                        break;
                                    }
                                }
                                session.set_revision_seen(revision);
                                if !healthy {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ticker.tick() => {
                if control_liveness_expired(session.last_recv(), Instant::now()) {
                    close_ws(&mut ws, WS_CLOSE_AUTH).await;
                    break;
                }
            }
        }
    }
}

/// Handles one post-authentication text message; `false` ends the session.
/// Version failures answer `UNSUPPORTED_VERSION` and stay connected; unknown
/// or malformed input answers `INVALID_MESSAGE`; unknown FUTURE types are
/// invalid to this phase. Errors echo the `requestId` when present.
async fn handle_text(registry: &WebTransferRegistry, session: &mut PeerSession, raw: &str) -> bool {
    async fn reply(session: &PeerSession, text: String) -> bool {
        let tx = session.sender();
        queue_send(&tx, text).await
    }
    let now = Instant::now();
    session.touch(now);
    let (version, request_id) = lenient_meta(raw);
    if version != Some(u64::from(crate::web_transfer_protocol::PROTOCOL_VERSION)) {
        let message = match request_id {
            Some(id) => error_envelope(id, "UNSUPPORTED_VERSION", None),
            None => error_envelope_anon("UNSUPPORTED_VERSION", None),
        };
        return reply(session, message).await;
    }
    let env = match parse_client_envelope(raw) {
        Ok(env) => env,
        Err(_) => return reply(session, error_envelope_anon("INVALID_MESSAGE", None)).await,
    };
    if !session.take_control(now) {
        let message = match request_id {
            Some(id) => error_envelope(id, "RATE_LIMITED", None),
            None => error_envelope_anon("RATE_LIMITED", None),
        };
        return reply(session, message).await;
    }
    if let Some(id) = request_id {
        if let Some(cached) = session.replay(id, now) {
            return reply(session, cached).await;
        }
    }
    match env.typ.as_str() {
        "ping" => match parse_ping_body(&env) {
            Ok(()) => reply(session, pong_envelope()).await,
            Err(_) => reply(session, error_envelope_anon("INVALID_MESSAGE", None)).await,
        },
        "peer.rename" => {
            if !session.take_mutation(now) {
                let message = match request_id {
                    Some(id) => error_envelope(id, "RATE_LIMITED", None),
                    None => error_envelope_anon("RATE_LIMITED", None),
                };
                return reply(session, message).await;
            }
            let (id, raw_name) = match parse_rename_body(&env) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let message = match request_id {
                        Some(fallback) => error_envelope(fallback, "INVALID_MESSAGE", None),
                        None => error_envelope_anon("INVALID_MESSAGE", None),
                    };
                    if let Some(cache_id) = request_id {
                        session.remember(cache_id, message.clone(), now);
                    }
                    return reply(session, message).await;
                }
            };
            match registry.rename_peer(session.room(), session.peer_id(), &raw_name) {
                Ok(name) => {
                    session.set_display_name(name.clone());
                    let mut result = std::collections::BTreeMap::new();
                    result.insert("displayName".to_string(), serde_json::Value::String(name));
                    let message = ack_envelope(
                        id,
                        Some(serde_json::Value::Object(result.into_iter().collect())),
                    );
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
                Err(e) => {
                    let message = error_envelope(id, e.code(), None);
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
            }
        }
        "hello" => reply(session, error_envelope_anon("INVALID_MESSAGE", None)).await,
        "offer.publish" => {
            if !session.take_mutation(now) {
                let message = match request_id {
                    Some(id) => error_envelope(id, "RATE_LIMITED", None),
                    None => error_envelope_anon("RATE_LIMITED", None),
                };
                return reply(session, message).await;
            }
            let (id, body) = match parse_publish_body(&env) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let message = match request_id {
                        Some(fallback) => error_envelope(fallback, "INVALID_MESSAGE", None),
                        None => error_envelope_anon("INVALID_MESSAGE", None),
                    };
                    if let Some(cache_id) = request_id {
                        session.remember(cache_id, message.clone(), now);
                    }
                    return reply(session, message).await;
                }
            };
            // Byte cap on the compact form before struct decoding.
            let compact_len = serde_json::to_string(&body.manifest)
                .map(|s| s.len())
                .unwrap_or(usize::MAX);
            if check_manifest_byte_cap(compact_len).is_err() {
                let message = error_envelope(id, "INVALID_MESSAGE", None);
                session.remember(id, message.clone(), now);
                return reply(session, message).await;
            }
            let manifest = match parse_manifest(&body.manifest, &registry.config().limits) {
                Ok(manifest) => manifest,
                Err(_) => {
                    let message = error_envelope(id, "INVALID_MESSAGE", None);
                    session.remember(id, message.clone(), now);
                    return reply(session, message).await;
                }
            };
            let canonical = match canonical_json(&manifest_value(&manifest)) {
                Ok(canonical) => canonical,
                Err(_) => {
                    let message = error_envelope(id, "INVALID_MESSAGE", None);
                    session.remember(id, message.clone(), now);
                    return reply(session, message).await;
                }
            };
            let canonical: std::sync::Arc<[u8]> = canonical.into_bytes().into();
            match registry.publish_offer(
                session.room(),
                session.peer_id(),
                body.offer_id,
                &manifest,
                canonical,
                body.mac,
            ) {
                Ok(_) => {
                    let mut result = std::collections::BTreeMap::new();
                    result.insert(
                        "offerId".to_string(),
                        serde_json::Value::String(body.offer_id.to_string()),
                    );
                    let message = ack_envelope(
                        id,
                        Some(serde_json::Value::Object(result.into_iter().collect())),
                    );
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
                Err(e) if e.code() == "LIMIT_EXCEEDED" => {
                    // Caps move with load: transient, never cached.
                    reply(session, error_envelope(id, e.code(), None)).await
                }
                Err(e) => {
                    let message = error_envelope(id, e.code(), None);
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
            }
        }
        "offer.withdraw" => {
            if !session.take_mutation(now) {
                let message = match request_id {
                    Some(id) => error_envelope(id, "RATE_LIMITED", None),
                    None => error_envelope_anon("RATE_LIMITED", None),
                };
                return reply(session, message).await;
            }
            let (id, offer_id) = match parse_withdraw_body(&env) {
                Ok(parsed) => parsed,
                Err(_) => {
                    let message = match request_id {
                        Some(fallback) => error_envelope(fallback, "INVALID_MESSAGE", None),
                        None => error_envelope_anon("INVALID_MESSAGE", None),
                    };
                    if let Some(cache_id) = request_id {
                        session.remember(cache_id, message.clone(), now);
                    }
                    return reply(session, message).await;
                }
            };
            match registry.withdraw_offer(session.room(), session.peer_id(), offer_id) {
                Ok(_) => {
                    let mut result = std::collections::BTreeMap::new();
                    result.insert(
                        "offerId".to_string(),
                        serde_json::Value::String(offer_id.to_string()),
                    );
                    let message = ack_envelope(
                        id,
                        Some(serde_json::Value::Object(result.into_iter().collect())),
                    );
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
                Err(e) => {
                    let message = error_envelope(id, e.code(), None);
                    session.remember(id, message.clone(), now);
                    reply(session, message).await
                }
            }
        }
        _ => {
            // Known future-phase types with no handler in this phase.
            let message = match request_id {
                Some(id) => error_envelope(id, "INVALID_MESSAGE", None),
                None => error_envelope_anon("INVALID_MESSAGE", None),
            };
            if let Some(cache_id) = request_id {
                session.remember(cache_id, message.clone(), now);
            }
            reply(session, message).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_transfer::{
        IceServerConfig, WebTransferBaseUrl, WebTransferConfig, WebTransferLimits,
    };

    fn test_base_url() -> WebTransferBaseUrl {
        WebTransferBaseUrl::parse("https://files.example/").expect("test base url")
    }

    fn loopback_base_url(port: u16) -> WebTransferBaseUrl {
        WebTransferBaseUrl::parse(&format!("http://127.0.0.1:{port}/")).expect("loopback url")
    }

    #[test]
    fn hello_is_required_first_and_bounded() {
        let token = "e".repeat(64);
        let good = format!(r#"{{"v":1,"type":"hello","body":{{"memberToken":"{token}"}}}}"#);
        assert!(matches!(
            classify_first_message(&good),
            FirstMessage::Hello(_)
        ));
        // Anything that is not a valid hello is Invalid: other types, bad
        // JSON, missing token, a requestId on hello, unknown body fields.
        assert_eq!(
            classify_first_message(r#"{"v":1,"type":"ping","body":{}}"#),
            FirstMessage::Invalid
        );
        assert_eq!(classify_first_message("not json"), FirstMessage::Invalid);
        assert_eq!(
            classify_first_message(r#"{"v":1,"type":"hello","body":{}}"#),
            FirstMessage::Invalid
        );
        assert_eq!(
            classify_first_message(&format!(
                r#"{{"v":1,"type":"hello","requestId":"{}","body":{{"memberToken":"{token}"}}}}"#,
                "d".repeat(32)
            )),
            FirstMessage::Invalid
        );
        assert_eq!(
            classify_first_message(&format!(
                r#"{{"v":1,"type":"hello","body":{{"memberToken":"{token}","extra":1}}}}"#
            )),
            FirstMessage::Invalid
        );
        // Wrong version answers UNSUPPORTED_VERSION and stays connected.
        assert_eq!(
            classify_first_message(&format!(
                r#"{{"v":2,"type":"hello","body":{{"memberToken":"{token}"}}}}"#
            )),
            FirstMessage::VersionMismatch
        );
        assert_eq!(
            classify_first_message(r#"{"v":2,"type":"ping","body":{}}"#),
            FirstMessage::VersionMismatch
        );
        // Oversized input is dropped before parsing.
        let big = format!(
            r#"{{"v":1,"type":"ping","body":{{}},"pad":"{}"}}"#,
            "x".repeat(400 * 1024)
        );
        assert_eq!(classify_first_message(&big), FirstMessage::Invalid);
    }

    #[test]
    fn web_route_requires_configured_authority() {
        let base = test_base_url();
        assert!(host_matches_authority("files.example", &base));
        assert!(host_matches_authority("FILES.EXAMPLE", &base));
        // Explicit default port normalizes onto the bare authority.
        assert!(host_matches_authority("files.example:443", &base));
        assert!(!host_matches_authority("files.example:8443", &base));
        assert!(!host_matches_authority("other.example", &base));
        assert!(!host_matches_authority("files.example.evil.com", &base));
        assert!(!host_matches_authority("", &base));
        assert!(!host_matches_authority("files.example/a", &base));

        let loopback = loopback_base_url(8080);
        assert!(host_matches_authority("127.0.0.1:8080", &loopback));
        assert!(!host_matches_authority("127.0.0.1:9090", &loopback));
        // Loopback http default is 80: a bare host does NOT match :8080.
        assert!(!host_matches_authority("127.0.0.1", &loopback));
        let loopback80 = WebTransferBaseUrl::parse("http://127.0.0.1/").expect("port-80 url");
        assert!(host_matches_authority("127.0.0.1", &loopback80));
        assert!(host_matches_authority("127.0.0.1:80", &loopback80));

        let ipv6 = WebTransferBaseUrl::parse("http://[::1]/").expect("IPv6 loopback url");
        assert!(host_matches_authority("[::1]", &ipv6));
        assert!(host_matches_authority("[::1]:80", &ipv6));
        for malformed in [
            "[::1]:abc",
            "[::1]:0",
            "[::1]:65536",
            "[::1]evil",
            "[]",
            "[::1",
            "::1",
        ] {
            assert!(
                !host_matches_authority(malformed, &ipv6),
                "malformed authority matched: {malformed}"
            );
        }
    }

    #[test]
    fn room_shell_does_not_reveal_existence() {
        // Both a live and an absent room classify identically — the handler
        // serves the same bytes with 200 either way (asserted in the e2e).
        let present = "0123456789abcdef0123456789abcdef";
        let absent = "ffffffffffffffffffffffffffffffff";
        assert_eq!(
            classify("GET", &format!("/transfer/{present}")),
            WebDecision::RoomShell {
                room: present.to_string()
            }
        );
        assert_eq!(
            classify("GET", &format!("/transfer/{absent}")),
            WebDecision::RoomShell {
                room: absent.to_string()
            }
        );
        // HEAD is served like GET; anything else is 405, never content.
        assert!(matches!(
            classify("HEAD", &format!("/transfer/{present}")),
            WebDecision::RoomShell { .. }
        ));
        assert_eq!(
            classify("POST", &format!("/transfer/{present}")),
            WebDecision::MethodNotAllowed
        );
    }

    #[test]
    fn web_assets_have_exact_security_headers_and_mime() {
        for (name, mime) in [
            ("app.js", "text/javascript; charset=utf-8"),
            ("app.css", "text/css; charset=utf-8"),
            ("offer-worker.js", "text/javascript; charset=utf-8"),
        ] {
            assert!(matches!(
                classify("GET", &format!("/transfer/assets/{name}")),
                WebDecision::Asset { .. }
            ));
            let (body, content_type) = asset_bytes(name).expect("embedded asset");
            assert!(!body.is_empty());
            assert_eq!(content_type, mime);
        }
        let head = response_head(200, "text/javascript; charset=utf-8", 10, None);
        for want in [
            "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            "Referrer-Policy: no-referrer",
            "X-Content-Type-Options: nosniff",
            "Permissions-Policy: camera=(), microphone=(), geolocation=()",
            "Cache-Control: no-cache",
        ] {
            assert!(head.contains(want), "missing {want}");
        }
        assert!(!head
            .to_ascii_lowercase()
            .contains("access-control-allow-origin"));
        // HSTS only rides TLS responses.
        assert!(response_head(
            200,
            "text/html; charset=utf-8",
            1,
            Some("max-age=31536000; includeSubDomains")
        )
        .contains("Strict-Transport-Security:"));
        assert!(!response_head(200, "text/html; charset=utf-8", 1, None)
            .contains("Strict-Transport-Security:"));
    }

    #[test]
    fn web_routes_reject_noncanonical_paths_methods_and_ids() {
        // Percent-encoded separators, duplicate slashes, dot segments.
        assert_eq!(classify("GET", "/transfer/%2fetc"), WebDecision::BadRequest);
        assert_eq!(classify("GET", "/transfer/%2Fetc"), WebDecision::BadRequest);
        assert_eq!(
            classify("GET", "/transfer//0123456789abcdef0123456789abcdef"),
            WebDecision::BadRequest
        );
        assert_eq!(
            classify("GET", "/transfer/../etc/passwd"),
            WebDecision::BadRequest
        );
        assert_eq!(classify("GET", "/transfer/./x"), WebDecision::BadRequest);
        // Noncanonical IDs: uppercase, short, long, non-hex.
        for bad in [
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef00",
            "0123456789abcdef0123456789abcdeg",
            "",
        ] {
            assert_eq!(
                classify("GET", &format!("/transfer/{bad}")),
                WebDecision::BadRequest,
                "{bad:?}"
            );
            assert_eq!(
                classify("GET", &format!("/transfer/ws/control/{bad}")),
                WebDecision::BadRequest,
                "{bad:?}"
            );
        }
        // Query on WS endpoints is rejected; room shell tolerates it.
        let room = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            classify("GET", &format!("/transfer/ws/control/{room}?x=1")),
            WebDecision::BadRequest
        );
        assert!(matches!(
            classify("GET", &format!("/transfer/{room}?x=1")),
            WebDecision::RoomShell { .. }
        ));
        // Wrong methods.
        assert_eq!(
            classify("POST", "/transfer/assets/app.js"),
            WebDecision::MethodNotAllowed
        );
        assert_eq!(
            classify("POST", &format!("/transfer/ws/control/{room}")),
            WebDecision::MethodNotAllowed
        );
        // Relay shape is reserved (owned, never fallthrough).
        let transfer = "ffffffffffffffffffffffffffffffff";
        assert_eq!(
            classify("GET", &format!("/transfer/ws/relay/{room}/{transfer}")),
            WebDecision::RelayReserved {
                room: room.to_string(),
                transfer: transfer.to_string()
            }
        );
        // Unknown transfer paths are web-owned 404s, other trees are NotWeb.
        assert_eq!(classify("GET", "/transfer/nope"), WebDecision::BadRequest);
        assert_eq!(classify("GET", "/admin/status"), WebDecision::NotWeb);
    }

    #[test]
    fn ws_requires_exact_origin_and_subprotocol() {
        assert!(negotiates_control_subprotocol(Some("bore-transfer-v1")));
        assert!(negotiates_control_subprotocol(Some(
            "other, bore-transfer-v1"
        )));
        assert!(!negotiates_control_subprotocol(None));
        assert!(!negotiates_control_subprotocol(Some("")));
        assert!(!negotiates_control_subprotocol(Some("bore-transfer-v2")));
        // Case-sensitive: the token is exact.
        assert!(!negotiates_control_subprotocol(Some("Bore-Transfer-V1")));
    }

    #[tokio::test]
    async fn replay_stream_returns_prefetched_head_once_then_inner_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut a, b) = tokio::io::duplex(64);
        a.write_all(b"WORLD").await.unwrap();
        a.shutdown().await.unwrap();
        let mut replay = ReplayStream::new(b"HELLO".to_vec(), b);
        let mut out = Vec::new();
        replay.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"HELLOWORLD");
    }

    #[test]
    fn websocket_config_pins_all_bounds() {
        let cfg = websocket_config();
        assert_eq!(cfg.read_buffer_size, 4 * 1024);
        assert_eq!(cfg.write_buffer_size, 4 * 1024);
        assert_eq!(cfg.max_write_buffer_size, 256 * 1024);
        assert_eq!(cfg.max_message_size, Some(320 * 1024));
        assert_eq!(cfg.max_frame_size, Some(320 * 1024));
    }

    #[test]
    fn nonmatching_hosts_preserve_vhost_and_admin_routing() {
        // A web-shaped path on a foreign Host is NotWeb at the classifier
        // level only when the path is outside /transfer/; for /transfer/
        // paths the HOST gate (not the classifier) decides — and a foreign
        // host never reaches the classifier. Both properties pin the narrow
        // interception: same paths, other host → fallthrough.
        let foreign = WebTransferBaseUrl::parse("https://files.example/").expect("base");
        assert!(!host_matches_authority("bore.local", &foreign));
        assert!(!host_matches_authority("admin.local", &foreign));
        // And non-transfer paths never classify as web even on the right host.
        assert_eq!(classify("GET", "/admin/status"), WebDecision::NotWeb);
        assert_eq!(classify("GET", "/"), WebDecision::NotWeb);
        let _ = WebTransferConfig::new(
            foreign,
            WebTransferLimits::default(),
            IceServerConfig {
                servers: Vec::new(),
            },
        )
        .expect("config builds");
    }
}
