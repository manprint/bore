//! The fast link session: one HTTP(S) connection in, dispatch to an upload or
//! a download, the single-receiver slot state machine, the streaming handoff
//! between an uploader and its one downloader, re-arm, expiry and metrics.
//!
//! See `docs/plans/004_plan-FastLinkTransfer/phase_01.md` §0.4 (D13, I-3, I-4,
//! I-7, I-8) for the full contract this file implements.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::{timeout, Instant};
use tracing::{debug, info, warn};

use super::pump::{pump as run_pump, PumpConfig, PumpCounters, PumpEnd, PumpState, PUMP_DEPTH};
use super::{
    abort_close, chunk, download_head, expects_continue, generate_id, head_len, host_matches,
    linger_close, parse_download_target, parse_head, parse_upload_target, preview_verdict,
    simple_response, upload_framing, upload_head, usage_text, BodyFramer, FastLinkConfig, Preview,
    CONTINUE, LAST_CHUNK, LINGER_TIMEOUT, MAX_HEAD_BYTES, PREVIEW_HTML, REPLAY_WINDOW_BYTES,
    STALL_TIMEOUT,
};
use crate::basicauth::UNAUTHORIZED;
use crate::mux::Transport;
use crate::transfer_link::encode_path_segment;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The fast link transfer engine: one instance owns every in-flight upload
/// and holds the single fixed [`FastLinkConfig`] it was constructed with.
pub struct FastLink {
    config: FastLinkConfig,
    slots: DashMap<String, Arc<Slot>>,
    active: Arc<Semaphore>,
    metrics: FastLinkMetrics,
    total_rx: Arc<AtomicU64>,
    total_tx: Arc<AtomicU64>,
    wait_timeout: Duration,
    stall_timeout: Duration,
    replay_window: usize,
    /// The port a plain-HTTP request is redirected to (D9). The `Host` header
    /// of a plain request names the HTTP port, never the HTTPS one, so the
    /// redirect cannot be derived from it.
    https_port: u16,
}

/// Read-only snapshot of the fixed configuration, safe to publish (never
/// includes the credential).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FastLinkConfigView {
    /// The configured vhost host name.
    pub host: String,
    /// How long, in seconds, a slot waits for a downloader before expiring.
    pub wait_timeout_seconds: u64,
    /// Maximum number of concurrent uploads.
    pub max_active: u64,
    /// Upload bytes retained in RAM to allow a re-arm.
    pub replay_window_bytes: u64,
}

/// Read-only snapshot of the running counters.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FastLinkMetricsView {
    /// Slots currently waiting for a downloader.
    pub waiting: u64,
    /// Slots currently streaming to a downloader.
    pub streaming: u64,
    /// Total uploads ever started.
    pub uploads_total: u64,
    /// Uploads that finished completely.
    pub completed_total: u64,
    /// Uploads that ended in an unrecoverable failure.
    pub failed_total: u64,
    /// Uploads whose wait deadline elapsed with no downloader.
    pub expired_total: u64,
    /// Downloads dropped inside the replay window and successfully re-armed.
    pub rearmed_total: u64,
    /// Preview (bot/`Range`) requests answered without consuming a transfer.
    pub previews_blocked_total: u64,
    /// Upload requests rejected for a missing/wrong credential.
    pub auth_failures_total: u64,
    /// Uploads rejected because `--fast-link-transfer-max-active` was
    /// already reached.
    pub rejected_busy_total: u64,
    /// Total bytes ever streamed through the pump.
    pub bytes_total: u64,
}

/// Internal atomic counters backing [`FastLinkMetricsView`]. `bytes_total` is
/// an `Arc` because it is the very counter [`PumpCounters::bytes_total`] is
/// built from — every byte the pump moves updates this same cell.
struct FastLinkMetrics {
    waiting: AtomicU64,
    streaming: AtomicU64,
    uploads_total: AtomicU64,
    completed_total: AtomicU64,
    failed_total: AtomicU64,
    expired_total: AtomicU64,
    rearmed_total: AtomicU64,
    previews_blocked_total: AtomicU64,
    auth_failures_total: AtomicU64,
    rejected_busy_total: AtomicU64,
    bytes_total: Arc<AtomicU64>,
}

/// A single download slot: one generated id, one uploader, at most one
/// downloader at a time.
struct Slot {
    filename: String,
    length: Option<u64>,
    state: StdMutex<SlotState>,
    handoff: mpsc::Sender<Handoff>,
}

/// The slot's state machine (D13). Only the downloader ever moves
/// `Waiting -> Streaming` (under the lock, on a successful claim); only the
/// uploader ever moves `Streaming -> Waiting` (re-arm) or `-> Closed`
/// (success or failure). `SlotGuard::drop` is the only path that can close a
/// still-`Waiting` slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Waiting,
    Streaming,
    Closed,
}

/// A downloader connection, handed from the download task (D) to the waiting
/// upload task (U) once a claim on the slot succeeds.
struct Handoff {
    stream: Box<dyn Transport>,
    permit: Option<OwnedSemaphorePermit>,
    peer: Option<SocketAddr>,
}

/// The outcome of trying to close a `Waiting` slot from the expiry path,
/// racing a downloader that may already be claiming it (D13, T-FL-S12).
enum Taken {
    /// The slot was `Waiting` (now `Closed`) or already `Closed`: no
    /// downloader is coming.
    Closed,
    /// The slot was already `Streaming`: a downloader is claiming or has
    /// claimed it and the handoff is still expected to arrive.
    InFlight,
}

/// How a completed streaming attempt should be handled by the wait loop.
enum StreamOutcome {
    /// The connection is fully handled (success or a final failure already
    /// written and the uploader closed); the caller must return.
    Done,
    /// The download was dropped inside the replay window; the slot is back
    /// to `Waiting` and the caller should keep waiting with the same
    /// deadline.
    Rearm,
}

/// RAII slot cleanup (I-8): whichever task drops this last (normally the
/// uploader's task, on every exit path) closes the slot (if it is not
/// already) and removes it from the registry, bringing the waiting/streaming
/// gauges back to a state with no trace of this transfer.
struct SlotGuard {
    fast_link: Arc<FastLink>,
    id: String,
    slot: Arc<Slot>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.fast_link.transition(&self.slot, SlotState::Closed);
        self.fast_link
            .slots
            .remove_if(&self.id, |_, v| Arc::ptr_eq(v, &self.slot));
    }
}

impl FastLink {
    /// Build a new engine from a validated configuration. `total_rx`/
    /// `total_tx` are the server's overall byte counters, shared with every
    /// [`PumpCounters`] this engine ever builds.
    pub fn new(config: FastLinkConfig, total_rx: Arc<AtomicU64>, total_tx: Arc<AtomicU64>) -> Self {
        let wait_timeout = config.wait_timeout;
        let max_active = config.max_active;
        FastLink {
            config,
            slots: DashMap::new(),
            active: Arc::new(Semaphore::new(max_active)),
            metrics: FastLinkMetrics {
                waiting: AtomicU64::new(0),
                streaming: AtomicU64::new(0),
                uploads_total: AtomicU64::new(0),
                completed_total: AtomicU64::new(0),
                failed_total: AtomicU64::new(0),
                expired_total: AtomicU64::new(0),
                rearmed_total: AtomicU64::new(0),
                previews_blocked_total: AtomicU64::new(0),
                auth_failures_total: AtomicU64::new(0),
                rejected_busy_total: AtomicU64::new(0),
                bytes_total: Arc::new(AtomicU64::new(0)),
            },
            total_rx,
            total_tx,
            wait_timeout,
            stall_timeout: STALL_TIMEOUT,
            replay_window: REPLAY_WINDOW_BYTES,
            https_port: 443,
        }
    }

    /// Set the HTTPS port plain-HTTP requests are redirected to (default
    /// 443, which the redirect omits).
    pub fn set_https_port(&mut self, port: u16) {
        self.https_port = port;
    }

    /// The configured vhost host name.
    pub fn host(&self) -> &str {
        &self.config.host
    }

    /// The configured vhost subdomain label.
    pub fn label(&self) -> &str {
        &self.config.label
    }

    /// Whether an incoming `Host` header value names this server.
    pub fn matches_host(&self, host_header: &str) -> bool {
        host_matches(host_header, &self.config.host)
    }

    /// Read-only snapshot of the configuration, safe to publish.
    pub fn config_view(&self) -> FastLinkConfigView {
        FastLinkConfigView {
            host: self.config.host.clone(),
            wait_timeout_seconds: self.wait_timeout.as_secs(),
            max_active: self.config.max_active as u64,
            replay_window_bytes: self.replay_window as u64,
        }
    }

    /// Read-only snapshot of the running counters.
    pub fn metrics_view(&self) -> FastLinkMetricsView {
        FastLinkMetricsView {
            waiting: self.metrics.waiting.load(Ordering::Relaxed),
            streaming: self.metrics.streaming.load(Ordering::Relaxed),
            uploads_total: self.metrics.uploads_total.load(Ordering::Relaxed),
            completed_total: self.metrics.completed_total.load(Ordering::Relaxed),
            failed_total: self.metrics.failed_total.load(Ordering::Relaxed),
            expired_total: self.metrics.expired_total.load(Ordering::Relaxed),
            rearmed_total: self.metrics.rearmed_total.load(Ordering::Relaxed),
            previews_blocked_total: self.metrics.previews_blocked_total.load(Ordering::Relaxed),
            auth_failures_total: self.metrics.auth_failures_total.load(Ordering::Relaxed),
            rejected_busy_total: self.metrics.rejected_busy_total.load(Ordering::Relaxed),
            bytes_total: self.metrics.bytes_total.load(Ordering::Relaxed),
        }
    }

    /// Test-only: shrink the wait/stall timeouts so scenarios run fast.
    #[doc(hidden)]
    pub fn set_timeouts_for_test(&mut self, wait: Duration, stall: Duration) {
        self.wait_timeout = wait;
        self.stall_timeout = stall;
    }

    /// Test-only: shrink the replay window so a re-arm/failure boundary is
    /// reachable without moving megabytes of payload.
    #[doc(hidden)]
    pub fn set_replay_window_for_test(&mut self, bytes: usize) {
        self.replay_window = bytes;
    }

    /// Test-only: the number of slots currently registered (I-8: every
    /// finished scenario must observe `0`).
    #[doc(hidden)]
    pub fn slots_len(&self) -> usize {
        self.slots.len()
    }

    /// Serve one already-accepted HTTP(S) connection: parse its head (already
    /// read into `buffered`), then dispatch to an upload, a download, the
    /// usage page, or a plain-HTTP redirect/rejection.
    ///
    /// `permit` is the connection's own `--max-conns` permit, if any: an
    /// upload task simply holds it for its own connection's life, while a
    /// download task hands it into the [`Handoff`] so it stays held for as
    /// long as the streaming pump runs.
    pub async fn serve<S: Transport>(
        self: &Arc<Self>,
        mut stream: S,
        buffered: Vec<u8>,
        peer: Option<SocketAddr>,
        secure: bool,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let Some(head_len) = head_len(&buffered) else {
            let status: u16 = if buffered.len() >= MAX_HEAD_BYTES {
                431
            } else {
                400
            };
            let body: &[u8] = if status == 431 {
                b"request head too large\n"
            } else {
                b"malformed request\n"
            };
            let resp = simple_response(status, "text/plain", body, &[]);
            let _ = stream.write_all(&resp).await;
            linger_close(&mut stream).await;
            return;
        };

        let (method, target, host_header) = {
            let head_bytes = &buffered[..head_len];
            match parse_head(head_bytes) {
                Ok(h) => (
                    h.method.to_string(),
                    h.target.to_string(),
                    h.header("host").unwrap_or("").to_string(),
                ),
                Err(_) => {
                    let resp = simple_response(400, "text/plain", b"malformed request\n", &[]);
                    let _ = stream.write_all(&resp).await;
                    linger_close(&mut stream).await;
                    return;
                }
            }
        };

        let authority = authority_for(&self.config.host, &host_header);

        if !secure {
            if method == "GET" || method == "HEAD" {
                let host = &self.config.host;
                let location = match self.https_port {
                    443 => format!("https://{host}{target}"),
                    port => format!("https://{host}:{port}{target}"),
                };
                let resp = simple_response(308, "text/plain", b"", &[("Location", &location)]);
                let _ = stream.write_all(&resp).await;
            } else {
                let resp = simple_response(
                    403,
                    "text/plain",
                    b"fast link transfer requires HTTPS\n",
                    &[],
                );
                let _ = stream.write_all(&resp).await;
            }
            linger_close(&mut stream).await;
            return;
        }

        match method.as_str() {
            "PUT" => {
                self.serve_upload(stream, buffered, head_len, &authority, permit)
                    .await;
            }
            "GET" | "HEAD" => {
                let path_only = target.split('?').next().unwrap_or(&target);
                if path_only == "/" {
                    let is_head = method == "HEAD";
                    let body = usage_text(&authority);
                    let resp = response_for_method(
                        200,
                        "text/plain; charset=utf-8",
                        body.as_bytes(),
                        &[],
                        is_head,
                    );
                    let _ = stream.write_all(&resp).await;
                    linger_close(&mut stream).await;
                } else {
                    self.serve_download(stream, buffered, head_len, method == "HEAD", peer, permit)
                        .await;
                }
            }
            _ => {
                let resp = simple_response(
                    405,
                    "text/plain",
                    b"method not allowed\n",
                    &[("Allow", "GET, HEAD, PUT")],
                );
                let _ = stream.write_all(&resp).await;
                linger_close(&mut stream).await;
            }
        }
    }

    /// The upload task (U): authenticate, register a slot, print the link
    /// and wait for a downloader, streaming the body through [`run_pump`]
    /// once one claims it, re-arming as long as the replay window survives.
    async fn serve_upload<S: Transport>(
        self: &Arc<Self>,
        mut uploader: S,
        buffered: Vec<u8>,
        head_len: usize,
        authority: &str,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let (framing, expect, filename) = {
            let head_bytes = &buffered[..head_len];
            let h = parse_head(head_bytes).expect("already validated in serve()");
            let framing = match upload_framing(&h) {
                Ok(f) => f,
                Err(status) => {
                    let resp = simple_response(status, "text/plain", error_body(status), &[]);
                    let _ = uploader.write_all(&resp).await;
                    linger_close(&mut uploader).await;
                    return;
                }
            };
            let expect = match expects_continue(&h) {
                Ok(e) => e,
                Err(status) => {
                    let resp = simple_response(status, "text/plain", error_body(status), &[]);
                    let _ = uploader.write_all(&resp).await;
                    linger_close(&mut uploader).await;
                    return;
                }
            };
            let filename = match parse_upload_target(h.target) {
                Ok(f) => f,
                Err(status) => {
                    let resp = simple_response(status, "text/plain", error_body(status), &[]);
                    let _ = uploader.write_all(&resp).await;
                    linger_close(&mut uploader).await;
                    return;
                }
            };
            (framing, expect, filename)
        };

        // I-7: auth is checked only on the already-buffered head bytes, and
        // strictly before any `100 Continue`, active permit or slot.
        if !self.config.auth.authorized(&buffered[..head_len]) {
            self.metrics
                .auth_failures_total
                .fetch_add(1, Ordering::Relaxed);
            let _ = uploader.write_all(UNAUTHORIZED.as_bytes()).await;
            linger_close(&mut uploader).await;
            return;
        }

        let _active_permit = match self.active.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.metrics
                    .rejected_busy_total
                    .fetch_add(1, Ordering::Relaxed);
                let resp = simple_response(
                    503,
                    "text/plain",
                    b"fast link busy: too many active uploads, retry later\n",
                    &[("Retry-After", "30")],
                );
                let _ = uploader.write_all(&resp).await;
                linger_close(&mut uploader).await;
                return;
            }
        };

        let length = match framing {
            super::Framing::ContentLength(n) => Some(n),
            super::Framing::Chunked => None,
        };

        let (id, slot, mut handoff_rx) = loop {
            let id = match generate_id() {
                Ok(id) => id,
                Err(_) => {
                    let resp = simple_response(
                        503,
                        "text/plain",
                        b"failed to allocate a link id, retry\n",
                        &[],
                    );
                    let _ = uploader.write_all(&resp).await;
                    linger_close(&mut uploader).await;
                    drop(_active_permit);
                    return;
                }
            };
            match self.slots.entry(id.clone()) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(entry) => {
                    let (tx, rx) = mpsc::channel(1);
                    let slot = Arc::new(Slot {
                        filename: filename.clone(),
                        length,
                        state: StdMutex::new(SlotState::Waiting),
                        handoff: tx,
                    });
                    entry.insert(Arc::clone(&slot));
                    break (id, slot, rx);
                }
            }
        };
        self.metrics.waiting.fetch_add(1, Ordering::Relaxed);
        self.metrics.uploads_total.fetch_add(1, Ordering::Relaxed);
        // Kept alive (never read) purely for its `Drop` (I-8): closes and
        // removes the slot on every exit path from this function.
        let _guard = SlotGuard {
            fast_link: Arc::clone(self),
            id: id.clone(),
            slot: Arc::clone(&slot),
        };
        // `_active_permit` is held for the whole upload (drops with this
        // function); `_connection_permit` is just kept alive for the
        // upload's own connection lifetime.
        let _connection_permit = permit;

        if expect && uploader.write_all(CONTINUE).await.is_err() {
            return;
        }
        if uploader.write_all(upload_head()).await.is_err() {
            return;
        }
        let link = format!(
            "https://{authority}/{id}/{}",
            encode_path_segment(&filename)
        );
        if uploader
            .write_all(&chunk(format!("{link}\n").as_bytes()))
            .await
            .is_err()
        {
            return;
        }
        let minutes = self.wait_minutes();
        let waiting_msg = format!(
            "# waiting for the download (expires in {minutes} min); nothing is stored on the server\n"
        );
        if uploader
            .write_all(&chunk(waiting_msg.as_bytes()))
            .await
            .is_err()
        {
            return;
        }
        if uploader.flush().await.is_err() {
            return;
        }

        let mut pump_state = PumpState {
            framer: BodyFramer::new(framing),
            replay: Some(Vec::new()),
            consumed: 0,
        };

        let leftover_len = buffered.len() - head_len;
        if leftover_len > 0 {
            // Borrow `buffered`'s tail without cloning it: feed it, then drop
            // the borrow before `buffered` itself is dropped at scope end.
            let progress = {
                let leftover = &buffered[head_len..];
                pump_state.framer.feed(leftover)
            };
            match progress {
                Ok(progress) => {
                    let forward = progress.forward;
                    let leftover = &buffered[head_len..head_len + forward];
                    if let Some(replay) = pump_state.replay.as_mut() {
                        if pump_state.consumed + forward as u64 <= self.replay_window as u64 {
                            replay.extend_from_slice(leftover);
                        } else {
                            pump_state.replay = None;
                        }
                    }
                    pump_state.consumed += forward as u64;
                }
                Err(_) => {
                    self.fail(&mut uploader, "malformed upload body").await;
                    return;
                }
            }
        }
        drop(buffered);

        let mut buf = vec![0u8; crate::shared::proxy_buffer_size()];
        let deadline = Instant::now() + self.wait_timeout;

        loop {
            let prefill_cap: Option<usize> = if !pump_state.framer.is_done()
                && pump_state.consumed < self.replay_window as u64
            {
                let remaining = self.replay_window as u64 - pump_state.consumed;
                Some(std::cmp::min(buf.len() as u64, remaining) as usize)
            } else {
                None
            };

            enum Event {
                Handoff(Option<Handoff>),
                Expired,
                Read(std::io::Result<usize>),
            }

            let event = if let Some(n) = prefill_cap {
                tokio::select! {
                    biased;
                    h = handoff_rx.recv() => Event::Handoff(h),
                    _ = tokio::time::sleep_until(deadline) => Event::Expired,
                    r = uploader.read(&mut buf[..n]) => Event::Read(r),
                }
            } else {
                tokio::select! {
                    biased;
                    h = handoff_rx.recv() => Event::Handoff(h),
                    _ = tokio::time::sleep_until(deadline) => Event::Expired,
                }
            };

            match event {
                Event::Handoff(Some(h)) => {
                    let (u, ps, outcome) = self
                        .stream_handoff(uploader, &slot, &id[..4], pump_state, h)
                        .await;
                    uploader = u;
                    pump_state = ps;
                    match outcome {
                        StreamOutcome::Done => return,
                        StreamOutcome::Rearm => continue,
                    }
                }
                Event::Handoff(None) => {
                    // The sender lives in `slot`, held alive by this same
                    // task; this should be unreachable in practice.
                    self.fail(&mut uploader, "internal error: handoff channel closed")
                        .await;
                    return;
                }
                Event::Expired => match self.take_or_close(&slot) {
                    Taken::Closed => {
                        self.write_expired_and_close(&mut uploader).await;
                        return;
                    }
                    Taken::InFlight => {
                        match timeout(super::HANDOFF_RECV_TIMEOUT, handoff_rx.recv()).await {
                            Ok(Some(h)) => {
                                let (u, ps, outcome) = self
                                    .stream_handoff(uploader, &slot, &id[..4], pump_state, h)
                                    .await;
                                uploader = u;
                                pump_state = ps;
                                match outcome {
                                    StreamOutcome::Done => return,
                                    StreamOutcome::Rearm => continue,
                                }
                            }
                            _ => {
                                self.write_expired_and_close(&mut uploader).await;
                                return;
                            }
                        }
                    }
                },
                Event::Read(Ok(0)) | Event::Read(Err(_)) => match self.take_or_close(&slot) {
                    Taken::Closed => {
                        self.fail(&mut uploader, "upload ended before the body was complete")
                            .await;
                        return;
                    }
                    Taken::InFlight => {
                        if let Ok(Some(mut h)) =
                            timeout(super::HANDOFF_RECV_TIMEOUT, handoff_rx.recv()).await
                        {
                            let resp = simple_response(
                                404,
                                "text/plain",
                                b"unknown or expired link\n",
                                &[],
                            );
                            let _ = h.stream.write_all(&resp).await;
                            abort_close(&mut h.stream).await;
                        }
                        self.fail(&mut uploader, "upload ended before the body was complete")
                            .await;
                        return;
                    }
                },
                Event::Read(Ok(n)) => {
                    self.total_rx.fetch_add(n as u64, Ordering::Relaxed);
                    match pump_state.framer.feed(&buf[..n]) {
                        Ok(progress) => {
                            let forward = progress.forward;
                            if let Some(replay) = pump_state.replay.as_mut() {
                                if pump_state.consumed + forward as u64 <= self.replay_window as u64
                                {
                                    replay.extend_from_slice(&buf[..forward]);
                                } else {
                                    pump_state.replay = None;
                                }
                            }
                            pump_state.consumed += forward as u64;
                        }
                        Err(_) => {
                            self.fail(&mut uploader, "malformed upload body").await;
                            return;
                        }
                    }
                }
            }
        }
    }

    /// The download task (D): resolve the id, answer previews/`HEAD`
    /// without consuming anything, claim the slot under its lock, and hand
    /// this connection over to the waiting upload task.
    async fn serve_download<S: Transport>(
        self: &Arc<Self>,
        mut stream: S,
        buffered: Vec<u8>,
        head_len: usize,
        is_head: bool,
        peer: Option<SocketAddr>,
        permit: Option<OwnedSemaphorePermit>,
    ) {
        let head_bytes = &buffered[..head_len];
        let h = parse_head(head_bytes).expect("already validated in serve()");

        let Some(id) = parse_download_target(h.target) else {
            let resp = simple_response(404, "text/plain", b"unknown or expired link\n", &[]);
            let _ = stream.write_all(&resp).await;
            linger_close(&mut stream).await;
            return;
        };

        let Some(slot) = self.slots.get(&id).map(|e| Arc::clone(e.value())) else {
            debug!(
                id_prefix = &id[..4],
                "fast link: download for unknown or expired id"
            );
            let resp = simple_response(404, "text/plain", b"unknown or expired link\n", &[]);
            let _ = stream.write_all(&resp).await;
            linger_close(&mut stream).await;
            return;
        };

        if is_head {
            let resp = match download_head(&slot.filename, slot.length) {
                Ok(b) => b,
                Err(_) => simple_response(404, "text/plain", b"unknown or expired link\n", &[]),
            };
            let _ = stream.write_all(&resp).await;
            let _ = timeout(LINGER_TIMEOUT, stream.shutdown()).await;
            return;
        }

        match preview_verdict(&h) {
            Some(Preview::Bot) => {
                self.metrics
                    .previews_blocked_total
                    .fetch_add(1, Ordering::Relaxed);
                let resp = simple_response(200, "text/html; charset=utf-8", PREVIEW_HTML, &[]);
                let _ = stream.write_all(&resp).await;
                linger_close(&mut stream).await;
                return;
            }
            Some(Preview::Range) => {
                self.metrics
                    .previews_blocked_total
                    .fetch_add(1, Ordering::Relaxed);
                let content_range;
                let resp = if let Some(len) = slot.length {
                    content_range = format!("bytes */{len}");
                    simple_response(416, "text/plain", b"", &[("Content-Range", &content_range)])
                } else {
                    simple_response(416, "text/plain", b"", &[])
                };
                let _ = stream.write_all(&resp).await;
                linger_close(&mut stream).await;
                return;
            }
            None => {}
        }

        let claimed = {
            let mut state = slot.state.lock().unwrap();
            match *state {
                SlotState::Waiting => {
                    self.metrics.waiting.fetch_sub(1, Ordering::Relaxed);
                    self.metrics.streaming.fetch_add(1, Ordering::Relaxed);
                    *state = SlotState::Streaming;
                    Ok(())
                }
                SlotState::Streaming => Err(409u16),
                SlotState::Closed => Err(404u16),
            }
        };

        let status = match claimed {
            Ok(()) => None,
            Err(409) => Some((409u16, &b"download already in progress\n"[..])),
            Err(_) => Some((404u16, &b"unknown or expired link\n"[..])),
        };
        if let Some((status, body)) = status {
            debug!(
                id_prefix = &id[..4],
                status, "fast link download claim rejected"
            );
            let resp = simple_response(status, "text/plain", body, &[]);
            let _ = stream.write_all(&resp).await;
            linger_close(&mut stream).await;
            return;
        }

        let handoff = Handoff {
            stream: Box::new(stream),
            permit,
            peer,
        };
        match slot.handoff.try_send(handoff) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(mut h))
            | Err(mpsc::error::TrySendError::Closed(mut h)) => {
                // Reachable only if the upload task left between this claim
                // and the send (it bounds its wait for an in-flight claim by
                // HANDOFF_RECV_TIMEOUT); answer instead of leaving it hanging.
                warn!(
                    id_prefix = &id[..4],
                    "fast link: upload left before the claimed download could be handed over"
                );
                let resp = simple_response(503, "text/plain", b"internal error\n", &[]);
                let _ = h.stream.write_all(&resp).await;
                abort_close(&mut h.stream).await;
            }
        }
    }

    /// Deliver a claimed downloader to the uploader's body: write the
    /// download head and any buffered replay, tell the uploader streaming
    /// has started, then run the two-task pump. Returns the uploader and
    /// pump state back to the caller's wait loop, plus how to proceed.
    async fn stream_handoff<S: Transport>(
        &self,
        mut uploader: S,
        slot: &Arc<Slot>,
        id_prefix: &str,
        pump_state: PumpState,
        mut handoff: Handoff,
    ) -> (S, PumpState, StreamOutcome) {
        let t0 = std::time::Instant::now();
        let peer = handoff.peer;

        // Announced at claim time, before the head and the replay go out:
        // against a slow downloader the replay alone (up to the whole 4 MiB
        // window) can take minutes, and the uploader must not sit on a
        // silent "waiting" line meanwhile. A drop during the replay is then
        // reported by the re-arm line that follows.
        let started = timeout(self.stall_timeout, async {
            uploader.write_all(&chunk(b"# download started\n")).await?;
            uploader.flush().await
        })
        .await;
        if !matches!(started, Ok(Ok(()))) {
            self.fail(&mut uploader, "upload connection closed unexpectedly")
                .await;
            return (uploader, pump_state, StreamOutcome::Done);
        }

        // Bounded like the pump's own writes: the replay can be the whole
        // 4 MiB window, and a downloader that connects and never reads must
        // not park the uploader past the stall timeout (nor past its expiry).
        let down_ok = match download_head(&slot.filename, slot.length) {
            Ok(head) => {
                let stream = &mut handoff.stream;
                let replay = pump_state.replay.as_deref().unwrap_or_default();
                matches!(
                    timeout(self.stall_timeout, async {
                        stream.write_all(&head).await?;
                        if !replay.is_empty() {
                            stream.write_all(replay).await?;
                        }
                        stream.flush().await
                    })
                    .await,
                    Ok(Ok(()))
                )
            }
            Err(_) => false,
        };

        // The replay reached the downloader outside the pump, so the pump's
        // own counters never see it: account for it here, or `# done:`, the
        // admin `bytes_total` and the server TX total would all miss up to
        // the whole replay window.
        let replayed = if down_ok {
            pump_state.replay.as_ref().map_or(0, |r| r.len() as u64)
        } else {
            0
        };
        if replayed > 0 {
            self.total_tx.fetch_add(replayed, Ordering::Relaxed);
            self.metrics
                .bytes_total
                .fetch_add(replayed, Ordering::Relaxed);
        }

        if !down_ok {
            drop(handoff.permit);
            debug!(
                id_prefix,
                ?peer,
                "fast link download connection dropped before the response head"
            );
            let outcome = self.rearm_or_fail(&mut uploader, slot, &pump_state).await;
            return (uploader, pump_state, outcome);
        }

        let counters = PumpCounters {
            total_rx: Arc::clone(&self.total_rx),
            total_tx: Arc::clone(&self.total_tx),
            bytes_total: Arc::clone(&self.metrics.bytes_total),
        };
        let cfg = PumpConfig {
            buffer: crate::shared::proxy_buffer_size(),
            depth: PUMP_DEPTH,
            stall: self.stall_timeout,
            replay_window: self.replay_window,
            #[cfg(test)]
            on_buffer: None,
            #[cfg(test)]
            on_task_start: None,
            #[cfg(test)]
            on_chunk: None,
        };

        let result = run_pump(uploader, handoff.stream, pump_state, cfg, counters).await;
        drop(handoff.permit);

        let mut uploader = result.uploader;
        let pump_state = result.state;
        let written = replayed + result.written;

        match result.end {
            PumpEnd::Completed => {
                self.metrics.completed_total.fetch_add(1, Ordering::Relaxed);
                self.transition(slot, SlotState::Closed);
                let secs = t0.elapsed().as_secs_f64();
                let mib = written as f64 / (1024.0 * 1024.0);
                let mib_s = if secs > 0.0 { mib / secs } else { 0.0 };
                let msg = format!("# done: {written} bytes in {secs:.1} s ({mib_s:.1} MiB/s)\n");
                let _ = uploader.write_all(&chunk(msg.as_bytes())).await;
                let _ = uploader.write_all(LAST_CHUNK).await;
                let _ = uploader.flush().await;
                let _ = timeout(LINGER_TIMEOUT, uploader.shutdown()).await;
                info!(
                    id_prefix,
                    ?peer,
                    bytes = pump_state.consumed,
                    duration_s = secs,
                    "fast link upload completed"
                );
                (uploader, pump_state, StreamOutcome::Done)
            }
            PumpEnd::DownloaderGone => {
                debug!(
                    id_prefix,
                    ?peer,
                    bytes = pump_state.consumed,
                    "fast link download dropped mid-stream"
                );
                let outcome = self.rearm_or_fail(&mut uploader, slot, &pump_state).await;
                (uploader, pump_state, outcome)
            }
            PumpEnd::UploaderFailed(reason) => {
                info!(
                    id_prefix,
                    ?peer,
                    reason,
                    bytes = pump_state.consumed,
                    "fast link upload failed"
                );
                self.fail(&mut uploader, reason).await;
                (uploader, pump_state, StreamOutcome::Done)
            }
        }
    }

    /// I-3: re-arm (return to `Waiting`) only when the whole replay window
    /// survived the dropped download; otherwise the transfer cannot be
    /// resumed and this is a final failure.
    async fn rearm_or_fail<S: Transport>(
        &self,
        uploader: &mut S,
        slot: &Arc<Slot>,
        pump_state: &PumpState,
    ) -> StreamOutcome {
        if pump_state.replay.is_some() {
            self.transition(slot, SlotState::Waiting);
            self.metrics.rearmed_total.fetch_add(1, Ordering::Relaxed);
            let msg: &[u8] =
                b"# download interrupted before the first 4 MiB; the link is still valid, waiting again\n";
            if uploader.write_all(&chunk(msg)).await.is_err() || uploader.flush().await.is_err() {
                self.fail(uploader, "upload connection closed unexpectedly")
                    .await;
                return StreamOutcome::Done;
            }
            StreamOutcome::Rearm
        } else {
            let reason = format!(
                "download interrupted after {} bytes; a stream cannot be replayed",
                pump_state.consumed
            );
            self.fail(uploader, &reason).await;
            StreamOutcome::Done
        }
    }

    /// A final, unrecoverable outcome (I-4): report it to the uploader with
    /// a `# failed:` line and close without the chunked terminator, so a
    /// truncated transfer is visibly truncated (never looks complete).
    async fn fail<S: Transport>(&self, uploader: &mut S, reason: &str) {
        self.metrics.failed_total.fetch_add(1, Ordering::Relaxed);
        let msg = format!("# failed: {reason}\n");
        let _ = timeout(LINGER_TIMEOUT, async {
            uploader.write_all(&chunk(msg.as_bytes())).await?;
            uploader.flush().await
        })
        .await;
        abort_close(uploader).await;
    }

    /// The wait deadline elapsed with nobody downloading: report it and
    /// close without the chunked terminator.
    async fn write_expired_and_close<S: Transport>(&self, uploader: &mut S) {
        self.metrics.expired_total.fetch_add(1, Ordering::Relaxed);
        let minutes = self.wait_minutes();
        let msg = format!("# expired: nobody downloaded the link within {minutes} min\n");
        let _ = timeout(LINGER_TIMEOUT, async {
            uploader.write_all(&chunk(msg.as_bytes())).await?;
            uploader.flush().await
        })
        .await;
        abort_close(uploader).await;
    }

    /// Try to close a still-`Waiting` slot at expiry, racing a downloader
    /// that may already have claimed it (D13, T-FL-S12): a single lock
    /// acquisition either closes the slot or reports that a claim is
    /// already in flight and its handoff is still expected.
    fn take_or_close(&self, slot: &Arc<Slot>) -> Taken {
        let mut state = slot.state.lock().unwrap();
        match *state {
            SlotState::Waiting => {
                self.metrics.waiting.fetch_sub(1, Ordering::Relaxed);
                *state = SlotState::Closed;
                Taken::Closed
            }
            SlotState::Streaming => Taken::InFlight,
            SlotState::Closed => Taken::Closed,
        }
    }

    /// The single state-transition helper (D13): updates the waiting/
    /// streaming gauges to match the move from whatever the slot's current
    /// state is to `to`. A no-op (including the gauges) when `to` already
    /// matches the current state, so it is safe to call unconditionally from
    /// [`SlotGuard::drop`].
    fn transition(&self, slot: &Slot, to: SlotState) {
        let mut state = slot.state.lock().unwrap();
        let from = *state;
        if from == to {
            return;
        }
        match from {
            SlotState::Waiting => {
                self.metrics.waiting.fetch_sub(1, Ordering::Relaxed);
            }
            SlotState::Streaming => {
                self.metrics.streaming.fetch_sub(1, Ordering::Relaxed);
            }
            SlotState::Closed => {}
        }
        match to {
            SlotState::Waiting => {
                self.metrics.waiting.fetch_add(1, Ordering::Relaxed);
            }
            SlotState::Streaming => {
                self.metrics.streaming.fetch_add(1, Ordering::Relaxed);
            }
            SlotState::Closed => {}
        }
        *state = to;
    }

    /// `ceil(wait_timeout / 60)`, at least 1 — the minute count printed in
    /// the waiting/expired status lines.
    fn wait_minutes(&self) -> u64 {
        self.wait_timeout.as_secs().div_ceil(60).max(1)
    }
}

/// The authority printed in a generated link and used for the plain-HTTP
/// redirect: the configured host, plus `:<port>` only when the incoming
/// `Host` header itself ends in `:` followed by one or more ASCII digits.
/// Never echoes any other byte of the header (rev 2).
fn authority_for(host: &str, host_header: &str) -> String {
    if let Some(i) = host_header.rfind(':') {
        let after = &host_header[i + 1..];
        if !after.is_empty() && after.bytes().all(|b| b.is_ascii_digit()) {
            return format!("{host}:{after}");
        }
    }
    host.to_string()
}

/// Build `simple_response`'s bytes, optionally truncated to just the header
/// block (for a `HEAD` request that must report the same headers, including
/// `Content-Length`, without sending the body).
fn response_for_method(
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
    head_only: bool,
) -> Vec<u8> {
    let full = simple_response(status, content_type, body, extra);
    if head_only {
        let cut = full
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| i + 4)
            .unwrap_or(full.len());
        full[..cut].to_vec()
    } else {
        full
    }
}

/// A short, generic body for a status this module does not give a specific
/// message for.
fn error_body(status: u16) -> &'static [u8] {
    match status {
        400 => b"bad request\n",
        411 => b"length required\n",
        417 => b"expectation failed\n",
        501 => b"not implemented\n",
        _ => b"error\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basicauth::BasicAuth;

    const HOST: &str = "fast.bore.local";
    const AUTH: &str = "u:p";
    const DUPLEX_CAP: usize = 8 * 1024 * 1024;

    // -- generic helpers -----------------------------------------------

    /// Tiny fixed-seed xorshift generator, copied per-file per the project's
    /// convention (see `framing.rs`/`pump.rs`'s own copies) — no shared
    /// test-only crate, no new dependency.
    struct Xorshift(u64);

    impl Xorshift {
        fn new(seed: u64) -> Self {
            Xorshift(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut rng = Xorshift::new(seed);
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&rng.next_u64().to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Hand-rolled standard base64 (with `=` padding), duplicated from
    /// `basicauth.rs`'s private encoder (not accessible from here) purely to
    /// build test `Authorization` headers — never used for anything
    /// security-sensitive.
    fn b64(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[((n >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    fn build_link(
        wait: Duration,
        stall: Duration,
        replay_window: usize,
        max_active: usize,
    ) -> Arc<FastLink> {
        let config = FastLinkConfig {
            host: HOST.to_string(),
            label: "fast".to_string(),
            auth: BasicAuth::parse(AUTH).unwrap(),
            wait_timeout: wait,
            max_active,
        };
        let mut link = FastLink::new(
            config,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        link.set_timeouts_for_test(wait, stall);
        link.set_replay_window_for_test(replay_window);
        Arc::new(link)
    }

    fn default_link() -> Arc<FastLink> {
        build_link(
            Duration::from_secs(3600),
            Duration::from_secs(600),
            REPLAY_WINDOW_BYTES,
            32,
        )
    }

    /// Build a `PUT` request head (to pass directly as `serve`'s `buffered`
    /// argument — this module never reads the head off a socket itself).
    fn put_head(target: &str, framing_header: &str, auth: Option<&str>, expect: bool) -> Vec<u8> {
        let mut s = format!("PUT {target} HTTP/1.1\r\nHost: {HOST}\r\n{framing_header}\r\n");
        if let Some(a) = auth {
            s.push_str(&format!("Authorization: Basic {}\r\n", b64(a.as_bytes())));
        }
        if expect {
            s.push_str("Expect: 100-continue\r\n");
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    fn put_head_cl(target: &str, len: u64, auth: Option<&str>, expect: bool) -> Vec<u8> {
        put_head(target, &format!("Content-Length: {len}"), auth, expect)
    }

    fn put_head_chunked(target: &str, auth: Option<&str>, expect: bool) -> Vec<u8> {
        put_head(target, "Transfer-Encoding: chunked", auth, expect)
    }

    fn get_head(target: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let mut s = format!("GET {target} HTTP/1.1\r\nHost: {HOST}\r\n");
        for (n, v) in extra {
            s.push_str(&format!("{n}: {v}\r\n"));
        }
        s.push_str("\r\n");
        s.into_bytes()
    }

    fn head_head(target: &str) -> Vec<u8> {
        format!("HEAD {target} HTTP/1.1\r\nHost: {HOST}\r\n\r\n").into_bytes()
    }

    fn chunked_encode_one(payload: &[u8]) -> Vec<u8> {
        let mut out = chunk(payload);
        out.extend_from_slice(LAST_CHUNK);
        out
    }

    async fn read_to_end_bounded<R: tokio::io::AsyncRead + Unpin>(r: &mut R, secs: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let _ = timeout(Duration::from_secs(secs), r.read_to_end(&mut out)).await;
        out
    }

    /// Read the fixed upload response head, then decode exactly the first
    /// chunk (the printed link). Returns the link and any raw bytes already
    /// read past it, to be folded into a later [`decode_chunked`] call on the
    /// rest of the stream.
    async fn read_link<R: tokio::io::AsyncRead + Unpin>(client: &mut R) -> (String, Vec<u8>) {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut head_end = loop {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
            let n = client
                .read(&mut tmp)
                .await
                .expect("uploader stream closed before its head");
            assert!(n > 0, "uploader stream closed before its head");
            buf.extend_from_slice(&tmp[..n]);
        };
        // An `Expect: 100-continue` request gets an interim `100 Continue`
        // response first, which also ends in its own `\r\n\r\n`; skip past
        // that too before treating the rest as the real response head.
        if buf[..head_end].starts_with(CONTINUE) {
            loop {
                if let Some(pos) = find_subslice(&buf[head_end..], b"\r\n\r\n") {
                    head_end += pos + 4;
                    break;
                }
                let n = client
                    .read(&mut tmp)
                    .await
                    .expect("uploader stream closed before its head");
                assert!(n > 0, "uploader stream closed before its head");
                buf.extend_from_slice(&tmp[..n]);
            }
        }
        let mut rest = buf.split_off(head_end);
        let (size, chunk_end) = loop {
            if let Some(pos) = find_subslice(&rest, b"\r\n") {
                let size_line = std::str::from_utf8(&rest[..pos]).unwrap();
                let size = usize::from_str_radix(size_line.trim(), 16).unwrap();
                let need = pos + 2 + size + 2;
                if rest.len() >= need {
                    break (size, need);
                }
            }
            let n = client
                .read(&mut tmp)
                .await
                .expect("uploader stream closed before the link");
            assert!(n > 0, "uploader stream closed before the link");
            rest.extend_from_slice(&tmp[..n]);
        };
        let payload = rest[chunk_end - 2 - size..chunk_end - 2].to_vec();
        let leftover = rest.split_off(chunk_end);
        let link = String::from_utf8(payload).unwrap().trim_end().to_string();
        (link, leftover)
    }

    /// Decode as many whole chunks as possible from `encoded`, concatenating
    /// their payloads. The second element is `true` only if a proper
    /// `0\r\n\r\n` terminator was seen.
    fn decode_chunked(encoded: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let mut i = 0usize;
        loop {
            let Some(rel) = find_subslice(&encoded[i..], b"\r\n") else {
                return (out, false);
            };
            let line_end = i + rel;
            let Ok(size_line) = std::str::from_utf8(&encoded[i..line_end]) else {
                return (out, false);
            };
            let size_str = size_line.split(';').next().unwrap_or("");
            let Ok(size) = usize::from_str_radix(size_str.trim(), 16) else {
                return (out, false);
            };
            let data_start = line_end + 2;
            if size == 0 {
                return (
                    out,
                    encoded.len() >= data_start + 2
                        && &encoded[data_start..data_start + 2] == b"\r\n",
                );
            }
            let data_end = data_start + size;
            if encoded.len() < data_end + 2 {
                return (out, false);
            }
            out.extend_from_slice(&encoded[data_start..data_end]);
            if &encoded[data_end..data_end + 2] != b"\r\n" {
                return (out, false);
            }
            i = data_end + 2;
        }
    }

    fn split_response(bytes: &[u8]) -> (String, Vec<u8>) {
        let pos = find_subslice(bytes, b"\r\n\r\n").expect("response must have a head");
        let head = String::from_utf8_lossy(&bytes[..pos]).to_string();
        (head, bytes[pos + 4..].to_vec())
    }

    fn extract_id(link: &str) -> String {
        let after_scheme = link.trim_start_matches("https://");
        let (_host, rest) = after_scheme.split_once('/').expect("link must have a path");
        rest.split_once('/')
            .map(|(id, _)| id)
            .unwrap_or(rest)
            .to_string()
    }

    /// I-8: every scenario below finishes by asserting the registry is
    /// empty and both gauges are back at zero.
    fn assert_no_leftover_slots(link: &FastLink) {
        assert_eq!(link.slots_len(), 0, "a slot was left registered");
        let m = link.metrics_view();
        assert_eq!(m.waiting, 0, "the waiting gauge did not return to zero");
        assert_eq!(m.streaming, 0, "the streaming gauge did not return to zero");
    }

    // -- T-FL-S1 ---------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upload_prints_link_then_streams_to_one_downloader() {
        let link = default_link();
        let payload = random_bytes(0xA1CE, 10 * 1024 * 1024);

        let head = put_head_cl("/file.bin", payload.len() as u64, Some(AUTH), true);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);

        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });

        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .expect("the link must be printed promptly");
        assert!(
            regex_like_id_and_name(&url, "file.bin"),
            "unexpected link: {url}"
        );
        let id = extract_id(&url);

        let payload_clone = payload.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&payload_clone).await.unwrap();
        });

        let dreq = get_head(&format!("/{id}/file.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });

        let downloaded = timeout(
            Duration::from_secs(30),
            read_to_end_bounded(&mut dl_client, 25),
        )
        .await
        .expect("download must finish");
        let (dhead, dbody) = split_response(&downloaded);
        assert!(dhead.starts_with("HTTP/1.1 200 OK"));
        assert!(dhead.contains("Content-Length: 10485760"));
        assert_eq!(dbody, payload);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();

        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(
            terminated,
            "the uploader must end with the chunked terminator"
        );
        assert!(String::from_utf8_lossy(&decoded).contains("# done: 10485760 bytes"));

        let m = link.metrics_view();
        assert_eq!(m.completed_total, 1);
        assert_eq!(m.bytes_total, 10 * 1024 * 1024);
        assert_no_leftover_slots(&link);
    }

    /// Review 0.4 follow-up (found by the first real `curl` run): a body that
    /// sits in the replay window when the download starts reaches the
    /// downloader outside the pump, and must still be counted — in the
    /// `# done:` line, in `bytes_total` and in the server TX total.
    /// Red-check: without the accounting all three read 0 here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_body_delivered_from_the_replay_is_counted() {
        let link = default_link();
        let payload = random_bytes(0xC0DE, 64 * 1024);
        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);
        ul_write.write_all(&payload).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;

        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });
        let downloaded = read_to_end_bounded(&mut dl_client, 10).await;
        let (_h, body) = split_response(&downloaded);
        assert_eq!(body, payload);
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();

        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);
        assert!(
            String::from_utf8_lossy(&decoded).contains("# done: 65536 bytes"),
            "{}",
            String::from_utf8_lossy(&decoded)
        );
        assert_eq!(link.metrics_view().bytes_total, 64 * 1024);
        assert_eq!(link.total_tx.load(Ordering::Relaxed), 64 * 1024);
        assert_no_leftover_slots(&link);
    }

    fn regex_like_id_and_name(url: &str, name: &str) -> bool {
        let Some(rest) = url.strip_prefix(&format!("https://{HOST}/")) else {
            return false;
        };
        let Some((id, filename)) = rest.split_once('/') else {
            return false;
        };
        id.len() == super::super::FAST_LINK_ID_LEN
            && id
                .bytes()
                .all(|b| super::super::FAST_LINK_ID_ALPHABET.contains(&b))
            && filename == name
    }

    // -- T-FL-S2 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chunked_upload_is_passed_through_verbatim() {
        let link = default_link();
        let payload = random_bytes(0xBEEF, 512 * 1024);
        let encoded = chunked_encode_one(&payload);

        let head = put_head_chunked("/f.bin", Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);

        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });

        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        let encoded_clone = encoded.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&encoded_clone).await.unwrap();
        });

        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });

        let downloaded = timeout(
            Duration::from_secs(30),
            read_to_end_bounded(&mut dl_client, 25),
        )
        .await
        .unwrap();
        let (dhead, dbody) = split_response(&downloaded);
        assert!(dhead.contains("Transfer-Encoding: chunked"));
        assert!(!dhead.contains("Content-Length"));
        assert_eq!(dbody, encoded);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (_decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S3 -----------------------------------------------------------

    #[tokio::test]
    async fn auth_failure_answers_401_before_any_continue() {
        let link = default_link();

        // Case 1: no Authorization header at all.
        let head = put_head_cl("/f.bin", 5, None, true);
        let (mut client, server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone.serve(server, head, None, true, None).await;
        });
        let resp = read_to_end_bounded(&mut client, 5).await;
        serve_task.await.unwrap();
        assert!(resp.starts_with(b"HTTP/1.1 401"));
        assert!(!resp
            .windows(b"100 Continue".len())
            .any(|w| w == b"100 Continue"));
        assert_eq!(link.metrics_view().auth_failures_total, 1);

        // Case 2: wrong credential.
        let head = put_head_cl("/f.bin", 5, Some("u:wrong"), true);
        let (mut client, server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone.serve(server, head, None, true, None).await;
        });
        let resp = read_to_end_bounded(&mut client, 5).await;
        serve_task.await.unwrap();
        assert!(resp.starts_with(b"HTTP/1.1 401"));
        assert_eq!(link.metrics_view().auth_failures_total, 2);

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S4 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn head_and_previews_never_consume() {
        let link = default_link();
        let payload = random_bytes(0xD00D, 500 * 1024);

        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);
        let payload_clone = payload.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&payload_clone).await.unwrap();
        });

        // HEAD: must report headers (Content-Length) and no body, and must
        // not touch the slot's state.
        {
            let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
            let lc = Arc::clone(&link);
            let req = head_head(&format!("/{id}/f.bin"));
            lc.serve(s, req, None, true, None).await;
            let resp = read_to_end_bounded(&mut c, 5).await;
            let (rhead, rbody) = split_response(&resp);
            assert!(rhead.starts_with("HTTP/1.1 200 OK"));
            assert!(rhead.contains(&format!("Content-Length: {}", payload.len())));
            assert!(rbody.is_empty());
        }
        assert_eq!(link.metrics_view().waiting, 1);

        // Bot preview.
        {
            let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
            let lc = Arc::clone(&link);
            let req = get_head(
                &format!("/{id}/f.bin"),
                &[("User-Agent", "Slackbot-LinkExpanding 1.0")],
            );
            lc.serve(s, req, None, true, None).await;
            let resp = read_to_end_bounded(&mut c, 5).await;
            assert!(resp.starts_with(b"HTTP/1.1 200 OK"));
            assert!(String::from_utf8_lossy(&resp).contains("text/html"));
        }
        assert_eq!(link.metrics_view().waiting, 1);
        assert_eq!(link.metrics_view().previews_blocked_total, 1);

        // Range preview.
        {
            let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
            let lc = Arc::clone(&link);
            let req = get_head(&format!("/{id}/f.bin"), &[("Range", "bytes=0-1023")]);
            lc.serve(s, req, None, true, None).await;
            let resp = read_to_end_bounded(&mut c, 5).await;
            assert!(resp.starts_with(b"HTTP/1.1 416"));
        }
        assert_eq!(link.metrics_view().waiting, 1);
        assert_eq!(link.metrics_view().previews_blocked_total, 2);

        // A normal GET now completes the download.
        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });
        let downloaded = timeout(
            Duration::from_secs(30),
            read_to_end_bounded(&mut dl_client, 25),
        )
        .await
        .unwrap();
        let (_dhead, dbody) = split_response(&downloaded);
        assert_eq!(dbody, payload);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (_decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S5 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_get_while_streaming_is_409() {
        let link = default_link();
        let payload = random_bytes(0x5EED, 200 * 1024);

        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        // First downloader: a tiny duplex buffer keeps the pump's writer
        // blocked (still `Streaming`) until this test reads from it.
        let dreq1 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client1, dl_server1) = tokio::io::duplex(4096);
        let link_clone2 = Arc::clone(&link);
        let dserve_task1 = tokio::spawn(async move {
            link_clone2.serve(dl_server1, dreq1, None, true, None).await;
        });

        let payload_clone = payload.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&payload_clone).await.unwrap();
        });

        // Give the first claim time to land (Waiting -> Streaming) before
        // racing the second GET against it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(link.metrics_view().streaming, 1);

        let dreq2 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client2, dl_server2) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone3 = Arc::clone(&link);
        timeout(
            Duration::from_secs(5),
            link_clone3.serve(dl_server2, dreq2, None, true, None),
        )
        .await
        .expect("a 409 must be answered promptly");
        let resp2 = read_to_end_bounded(&mut dl_client2, 5).await;
        assert!(resp2.starts_with(b"HTTP/1.1 409"));

        // Now let the first download finish.
        let downloaded1 = timeout(
            Duration::from_secs(30),
            read_to_end_bounded(&mut dl_client1, 25),
        )
        .await
        .unwrap();
        let (_h, b1) = split_response(&downloaded1);
        assert_eq!(b1, payload);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task1.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (_decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);

        assert_eq!(link.metrics_view().completed_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S6 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_dropped_inside_the_window_rearms() {
        // The window (2 MiB) comfortably exceeds the whole payload (1 MiB),
        // so however far the pump races ahead of what the test has actually
        // read, `consumed` can never cross it — a deliberately robust
        // substitute for a literal "64 KiB window" figure, which a shared,
        // process-global `proxy_buffer_size()` (see `crate::shared`) makes
        // impossible to pin exactly in-process (deviation noted in the
        // worker's final report).
        let link = build_link(
            Duration::from_secs(3600),
            Duration::from_secs(600),
            2 * 1024 * 1024,
            32,
        );
        let payload = random_bytes(0x1A11, 1024 * 1024);

        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        // First download: a small duplex buffer, read a little, then drop.
        let dreq1 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client1, dl_server1) = tokio::io::duplex(4096);
        let link_clone2 = Arc::clone(&link);
        let _dserve_task1 = tokio::spawn(async move {
            link_clone2.serve(dl_server1, dreq1, None, true, None).await;
        });

        let payload_clone = payload.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&payload_clone).await.unwrap();
        });

        let mut small = vec![0u8; 2048];
        timeout(Duration::from_secs(5), dl_client1.read_exact(&mut small))
            .await
            .expect("must read at least a little before dropping")
            .unwrap();
        drop(dl_client1);

        // Wait for the re-arm to be observed.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if link.metrics_view().rearmed_total >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "re-arm never observed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Peek at whatever status bytes are already queued (best-effort,
        // does not consume more than is immediately available).
        let mut peek = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_millis(200), ul_read.read(&mut peek))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0);
        leftover.extend_from_slice(&peek[..n]);
        assert!(
            String::from_utf8_lossy(&leftover).contains("# download interrupted before the first")
        );

        // Second download completes it fully.
        let dreq2 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client2, dl_server2) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone3 = Arc::clone(&link);
        let dserve_task2 = tokio::spawn(async move {
            link_clone3.serve(dl_server2, dreq2, None, true, None).await;
        });
        let downloaded2 = timeout(
            Duration::from_secs(30),
            read_to_end_bounded(&mut dl_client2, 25),
        )
        .await
        .unwrap();
        let (_h, b2) = split_response(&downloaded2);
        assert_eq!(b2, payload);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task2.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (_decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);

        let m = link.metrics_view();
        assert_eq!(m.rearmed_total, 1);
        assert_eq!(m.completed_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S7 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_dropped_past_the_window_fails_both() {
        let link = build_link(
            Duration::from_secs(3600),
            Duration::from_secs(600),
            64 * 1024,
            32,
        );
        let payload = random_bytes(0xFA11, 300 * 1024);

        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        let payload_clone = payload.clone();
        let writer_task = tokio::spawn(async move {
            ul_write.write_all(&payload_clone).await.unwrap();
        });

        // A small download-side buffer, so delivery is genuinely paced by
        // what this test reads (an 8 MiB buffer would let the whole 300 KiB
        // payload land instantly, completing before the drop below could
        // interrupt anything).
        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(32 * 1024);
        let link_clone2 = Arc::clone(&link);
        let _dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });

        let mut past_window = vec![0u8; 200 * 1024];
        timeout(
            Duration::from_secs(10),
            dl_client.read_exact(&mut past_window),
        )
        .await
        .expect("must read past the window before dropping")
        .unwrap();
        drop(dl_client);

        writer_task.await.unwrap();
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();

        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(
            !terminated,
            "a failure must never end with the chunked terminator"
        );
        assert!(String::from_utf8_lossy(&decoded).contains("# failed:"));

        // The link is gone: a subsequent GET is 404.
        let dreq2 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut c2, s2) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone3 = Arc::clone(&link);
        link_clone3.serve(s2, dreq2, None, true, None).await;
        let resp2 = read_to_end_bounded(&mut c2, 5).await;
        assert!(resp2.starts_with(b"HTTP/1.1 404"));

        assert_eq!(link.metrics_view().failed_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S7b (review 0.4): the replay write is stall-bounded ---------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_downloader_that_never_reads_the_replay_is_bounded() {
        // A downloader that connects and never reads must not park the
        // upload on the pre-pump replay write: the stall timeout bounds it,
        // the replay is intact, so the link re-arms and a second download
        // still receives every byte. Red-check: without the bound the first
        // download blocks forever and the re-arm is never observed.
        let link = build_link(
            Duration::from_secs(3600),
            Duration::from_millis(300),
            2 * 1024 * 1024,
            32,
        );
        let payload = random_bytes(0x57A1, 1024 * 1024);

        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        // The whole body lands in the replay window before anyone claims.
        ul_write.write_all(&payload).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let dreq1 = get_head(&format!("/{id}/f.bin"), &[]);
        let (dl_client1, dl_server1) = tokio::io::duplex(16 * 1024);
        let link_clone2 = Arc::clone(&link);
        let _dserve_task1 = tokio::spawn(async move {
            link_clone2.serve(dl_server1, dreq1, None, true, None).await;
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while link.metrics_view().rearmed_total < 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "a never-reading downloader parked the upload (re-arm never observed)"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(dl_client1);

        let dreq2 = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client2, dl_server2) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone3 = Arc::clone(&link);
        let dserve_task2 = tokio::spawn(async move {
            link_clone3.serve(dl_server2, dreq2, None, true, None).await;
        });
        let downloaded2 = read_to_end_bounded(&mut dl_client2, 20).await;
        let (_h, b2) = split_response(&downloaded2);
        assert_eq!(b2, payload);

        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task2.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);
        // The start is announced at claim time, so the stalled first download
        // shows up as "started" then "interrupted", never as silence.
        let text = String::from_utf8_lossy(&decoded);
        let started = text.find("# download started").expect("start announced");
        let interrupted = text
            .find("# download interrupted")
            .expect("re-arm announced");
        assert!(started < interrupted, "{text}");
        assert_eq!(link.metrics_view().completed_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S8 -----------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn wait_timeout_expires_without_terminator() {
        let link = build_link(
            Duration::from_secs(2),
            Duration::from_secs(600),
            REPLAY_WINDOW_BYTES,
            32,
        );
        let head = put_head_cl("/f.bin", 10, Some(AUTH), false);
        let (mut client, server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone.serve(server, head, None, true, None).await;
        });

        let (_url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut client))
            .await
            .unwrap();

        tokio::time::advance(Duration::from_secs(3)).await;
        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();

        leftover.extend_from_slice(&read_to_end_bounded(&mut client, 5).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(!terminated);
        assert!(String::from_utf8_lossy(&decoded).contains("# expired:"));

        let dreq = get_head(&format!("/{}/f.bin", "0000000000000000"), &[]);
        let (mut c2, s2) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&link).serve(s2, dreq, None, true, None).await;
        let _ = read_to_end_bounded(&mut c2, 5).await;

        assert_eq!(link.metrics_view().expired_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S9 -----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn uploader_abort_truncates_the_download() {
        let link = default_link();
        let head = put_head_cl("/f.bin", 1024 * 1024, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });

        let partial = random_bytes(0x900D, 100 * 1024);
        ul_write.write_all(&partial).await.unwrap();
        ul_write.shutdown().await.unwrap();

        let downloaded = read_to_end_bounded(&mut dl_client, 25).await;
        let (_h, body) = split_response(&downloaded);
        assert!(body.len() < 1024 * 1024);

        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();

        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 10).await);
        let (decoded, terminated) = decode_chunked(&leftover);
        assert!(!terminated);
        assert!(String::from_utf8_lossy(&decoded).contains("# failed:"));

        assert_eq!(link.metrics_view().failed_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S10 ----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_active_rejects_with_503() {
        let link = build_link(
            Duration::from_millis(200),
            Duration::from_secs(600),
            REPLAY_WINDOW_BYTES,
            1,
        );

        let head1 = put_head_cl("/a.bin", 5, Some(AUTH), false);
        let (mut c1, s1) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone = Arc::clone(&link);
        let serve_task1 = tokio::spawn(async move {
            link_clone.serve(s1, head1, None, true, None).await;
        });
        // Let upload 1 claim the only active-permit slot.
        let _ = timeout(Duration::from_secs(5), read_link(&mut c1))
            .await
            .unwrap();

        let head2 = put_head_cl("/b.bin", 5, Some(AUTH), false);
        let (mut c2, s2) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&link).serve(s2, head2, None, true, None).await;
        let resp2 = read_to_end_bounded(&mut c2, 5).await;
        assert!(resp2.starts_with(b"HTTP/1.1 503"));
        assert!(String::from_utf8_lossy(&resp2).contains("Retry-After"));

        // Upload 1 self-expires shortly (200ms wait timeout), freeing its slot.
        timeout(Duration::from_secs(10), serve_task1)
            .await
            .unwrap()
            .unwrap();

        let m = link.metrics_view();
        assert_eq!(m.rejected_busy_total, 1);
        assert_eq!(m.expired_total, 1);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S11 ----------------------------------------------------------

    #[tokio::test]
    async fn plain_connections_are_refused_or_redirected() {
        let link = default_link();

        let head = put_head_cl("/f.bin", 5, Some(AUTH), false);
        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&link).serve(s, head, None, false, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 403"));

        let head = get_head("/some/target", &[]);
        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&link).serve(s, head, None, false, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 308"));
        assert!(String::from_utf8_lossy(&resp)
            .contains(&format!("Location: https://{HOST}/some/target")));

        // A non-standard HTTPS port is named; the plain request's own port
        // (its HTTP port) never leaks into the redirect.
        let mut odd = FastLink::new(
            FastLinkConfig {
                host: HOST.to_string(),
                label: "fast".to_string(),
                auth: BasicAuth::parse(AUTH).unwrap(),
                wait_timeout: Duration::from_secs(3600),
                max_active: 32,
            },
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        odd.set_https_port(8443);
        let odd = Arc::new(odd);
        let head = format!("GET /x HTTP/1.1\r\nHost: {HOST}:8080\r\n\r\n").into_bytes();
        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&odd).serve(s, head, None, false, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(
            String::from_utf8_lossy(&resp)
                .contains(&format!("Location: https://{HOST}:8443/x\r\n")),
            "{}",
            String::from_utf8_lossy(&resp)
        );

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S12 ----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expiry_racing_a_claim_serves_the_claimant() {
        let link = build_link(
            Duration::from_millis(250),
            Duration::from_secs(60),
            REPLAY_WINDOW_BYTES,
            32,
        );
        let payload = random_bytes(0xE12, 4096);
        let head = put_head_cl("/f.bin", payload.len() as u64, Some(AUTH), false);
        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, mut ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, head, None, true, None)
                .await;
        });

        let (url, _leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);
        ul_write.write_all(&payload).await.unwrap();

        // Deterministically force the D13 race: put the slot into
        // `Streaming` directly (as if a claim had already succeeded) before
        // the wait timeout fires.
        let slot = link
            .slots
            .get(&id)
            .map(|e| Arc::clone(e.value()))
            .expect("slot must exist");
        link.transition(&slot, SlotState::Streaming);

        // Let the (250ms) wait timeout elapse while state == Streaming.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now the real handoff arrives, within HANDOFF_RECV_TIMEOUT.
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        slot.handoff
            .send(Handoff {
                stream: Box::new(dl_server),
                permit: None,
                peer: None,
            })
            .await
            .expect("the handoff channel must still accept the claim");

        let downloaded = timeout(
            Duration::from_secs(10),
            read_to_end_bounded(&mut dl_client, 8),
        )
        .await
        .unwrap();
        let (dhead, dbody) = split_response(&downloaded);
        assert!(dhead.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(dbody, payload);

        timeout(Duration::from_secs(5), serve_task)
            .await
            .unwrap()
            .unwrap();
        let _ = read_to_end_bounded(&mut ul_read, 2).await;

        let m = link.metrics_view();
        assert_eq!(m.completed_total, 1);
        assert_eq!(m.expired_total, 0);
        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S14 ----------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn excess_bytes_after_the_body_are_not_forwarded() {
        let link = default_link();
        let body = b"HELLO";
        let mut buffered = put_head_cl("/f.bin", body.len() as u64, Some(AUTH), false);
        buffered.extend_from_slice(body);
        buffered.extend_from_slice(b"GARBAGE");

        let (upload_client, upload_server) = tokio::io::duplex(DUPLEX_CAP);
        let (mut ul_read, _ul_write) = tokio::io::split(upload_client);
        let link_clone = Arc::clone(&link);
        let serve_task = tokio::spawn(async move {
            link_clone
                .serve(upload_server, buffered, None, true, None)
                .await;
        });
        let (url, mut leftover) = timeout(Duration::from_secs(10), read_link(&mut ul_read))
            .await
            .unwrap();
        let id = extract_id(&url);

        let dreq = get_head(&format!("/{id}/f.bin"), &[]);
        let (mut dl_client, dl_server) = tokio::io::duplex(DUPLEX_CAP);
        let link_clone2 = Arc::clone(&link);
        let dserve_task = tokio::spawn(async move {
            link_clone2.serve(dl_server, dreq, None, true, None).await;
        });
        let downloaded = timeout(
            Duration::from_secs(10),
            read_to_end_bounded(&mut dl_client, 8),
        )
        .await
        .unwrap();
        let (_h, dbody) = split_response(&downloaded);
        assert_eq!(dbody, body);

        timeout(Duration::from_secs(10), serve_task)
            .await
            .unwrap()
            .unwrap();
        dserve_task.await.unwrap();
        leftover.extend_from_slice(&read_to_end_bounded(&mut ul_read, 5).await);
        let (_decoded, terminated) = decode_chunked(&leftover);
        assert!(terminated);

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S15 ----------------------------------------------------------

    #[tokio::test]
    async fn usage_and_method_table() {
        let link = default_link();

        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        let head = get_head("/", &[]);
        Arc::clone(&link).serve(s, head, None, true, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 200 OK"));
        assert!(String::from_utf8_lossy(&resp).contains("curl -u"));

        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        let head = format!("DELETE /x HTTP/1.1\r\nHost: {HOST}\r\n\r\n").into_bytes();
        Arc::clone(&link).serve(s, head, None, true, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 405"));
        assert!(String::from_utf8_lossy(&resp).contains("Allow: GET, HEAD, PUT"));

        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        let mut head = format!("GET /x HTTP/1.1\r\nHost: {HOST}\r\n").into_bytes();
        for i in 0..2000 {
            head.extend_from_slice(format!("X-{i}: aaaaaaaaaa\r\n").as_bytes());
        }
        // No terminating blank line, and well past MAX_HEAD_BYTES.
        Arc::clone(&link).serve(s, head, None, true, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 431"));

        assert_no_leftover_slots(&link);
    }

    // -- T-FL-S16 ----------------------------------------------------------

    #[tokio::test]
    async fn authorization_in_the_body_prefix_is_ignored() {
        let link = default_link();
        let fake = b"Authorization: Basic dTpw\r\nrest of the file\n";
        let mut buffered = put_head_cl("/f.bin", fake.len() as u64, None, false);
        buffered.extend_from_slice(fake);

        let (mut c, s) = tokio::io::duplex(DUPLEX_CAP);
        Arc::clone(&link).serve(s, buffered, None, true, None).await;
        let resp = read_to_end_bounded(&mut c, 5).await;
        assert!(resp.starts_with(b"HTTP/1.1 401"));

        assert_eq!(link.metrics_view().auth_failures_total, 1);
        assert_no_leftover_slots(&link);
    }
}
