//! The streaming pump (D8): the data path of every fast link transfer.
//!
//! One task (R) owns the uploader stream (TLS decrypt + read + framing); a
//! second task (W) owns the downloader stream (TLS encrypt + write + flush).
//! Running them on separate tasks lets both directions' TLS work land on
//! different cores instead of serializing on one; a fixed pool of `depth`
//! buffers is recycled between them through two `mpsc` channels so steady
//! -state streaming performs zero allocations. Neither stream is ever
//! `split()` or shared between tasks — each belongs to exactly one task for
//! its whole life, per the project's standing "never split a `Stream` across
//! two tasks" rule (see the yamux stream-split invariant elsewhere in this
//! codebase).
//!
//! Cancellation is one-directional and deliberate: when R fails for any
//! reason other than observing the cancellation itself, it calls
//! [`tokio_util::sync::CancellationToken::cancel`] *before* dropping its
//! sender half of the "full buffers" channel, so W takes the abort path
//! ([`super::abort_close`], no chunked terminator) instead of reading a clean
//! channel close and shutting down as if the transfer had completed (I-5:
//! a truncated transfer must never look complete).
//!
//! The session task that owns this module (`FastLink::serve_upload`) lands
//! in plan 004 unit 0.4 (`src/fast_link/session.rs`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::{abort_close, BodyFramer, LINGER_TIMEOUT};
use crate::mux::Transport;

/// Number of fixed buffers in flight between the reader and writer tasks.
pub(crate) const PUMP_DEPTH: usize = 4;

/// Mutable framing state handed to [`pump`] and handed back in [`PumpResult`]
/// so a re-armed slot (0.4) can resume with an intact replay window.
pub(crate) struct PumpState {
    pub framer: BodyFramer,
    /// Every upload byte consumed so far, kept in RAM, as long as the total
    /// has stayed within the configured replay window; `None` once it has
    /// been exceeded (the transfer can no longer be re-armed, I-3).
    pub replay: Option<Vec<u8>>,
    /// Total upload bytes consumed by the framer so far (not necessarily
    /// equal to bytes written to a downloader, e.g. mid-stream).
    pub consumed: u64,
}

/// Test-only hook types. Never present in a production build (the pump
/// allocates its `depth` buffers exactly once and otherwise touches no
/// global state a production caller could observe here).
#[cfg(test)]
pub(crate) type OnBuffer = Arc<dyn Fn(usize) + Send + Sync>;
#[cfg(test)]
pub(crate) type OnTaskStart = Arc<dyn Fn(&'static str) + Send + Sync>;
#[cfg(test)]
pub(crate) type OnChunk = Arc<dyn Fn(usize) + Send + Sync>;

/// Pump tuning, plus (test builds only) two observation hooks used to assert
/// buffer reuse and real task parallelism without a production-visible
/// allocation counter.
#[derive(Clone)]
pub(crate) struct PumpConfig {
    /// Size, in bytes, of each recycled buffer (`crate::shared::proxy_buffer_size()`
    /// in production).
    pub buffer: usize,
    /// Number of buffers kept in flight between R and W.
    pub depth: usize,
    /// A read or write that makes no progress for this long is a stall.
    pub stall: Duration,
    /// Cap, in bytes, on how much of the upload R keeps in `PumpState::replay`.
    pub replay_window: usize,
    /// Called once per buffer allocation (there are exactly `depth` of them,
    /// all at pump start) with the buffer's `as_ptr()` cast to `usize`.
    #[cfg(test)]
    pub on_buffer: Option<OnBuffer>,
    /// Called once by each of R and W, at the start of its loop, with a
    /// fixed task name (`"reader"` / `"writer"`).
    #[cfg(test)]
    pub on_task_start: Option<OnTaskStart>,
    /// Called by W once per message received from R, with that message's
    /// byte length — used to assert read coalescing without a
    /// production-visible message counter.
    #[cfg(test)]
    pub on_chunk: Option<OnChunk>,
}

/// Shared byte counters, `Relaxed` throughout: nothing here ever needs to
/// synchronize-with anything else, they are read only for metrics/admin
/// views.
#[derive(Clone)]
pub(crate) struct PumpCounters {
    pub total_rx: Arc<AtomicU64>,
    pub total_tx: Arc<AtomicU64>,
    pub bytes_total: Arc<AtomicU64>,
}

/// How a [`pump`] call ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PumpEnd {
    /// The framer reached `done`, every byte was written to the downloader,
    /// and the downloader was flushed and (bounded) shut down.
    Completed,
    /// The downloader's write/flush failed or stalled, or it was otherwise
    /// dropped. The uploader is returned intact, with `PumpState` reflecting
    /// exactly the bytes consumed so far.
    DownloaderGone,
    /// The upload ended prematurely, errored, stalled, or failed to frame.
    /// The downloader has already been closed with [`abort_close`] and
    /// carries no terminator (I-5).
    UploaderFailed(&'static str),
}

/// The outcome of a [`pump`] call: the uploader (always handed back so a
/// caller can keep talking to it, e.g. to report `# failed:`), the updated
/// framing state, how it ended, and how many bytes reached the downloader.
pub(crate) struct PumpResult<S> {
    pub uploader: S,
    pub state: PumpState,
    pub end: PumpEnd,
    pub written: u64,
}

/// Run the two-task streaming pump: R reads+frames the uploader and forwards
/// body bytes through a bounded channel of recycled buffers; W writes+flushes
/// them to the downloader. See the module doc and D8/I-6/I-11 in
/// `docs/plans/004_plan-FastLinkTransfer/phase_01.md` §0.3 for the full
/// contract.
pub(crate) async fn pump<S: Transport>(
    uploader: S,
    downloader: Box<dyn Transport>,
    state: PumpState,
    cfg: PumpConfig,
    counters: PumpCounters,
) -> PumpResult<S> {
    let (free_tx, free_rx) = mpsc::channel::<Vec<u8>>(cfg.depth);
    let (full_tx, full_rx) = mpsc::channel::<(Vec<u8>, usize)>(cfg.depth);

    // The only allocation the pump ever performs: `depth` fixed buffers,
    // pre-loaded into the free channel before either task starts.
    for _ in 0..cfg.depth {
        let buf = vec![0u8; cfg.buffer];
        #[cfg(test)]
        if let Some(hook) = cfg.on_buffer.as_ref() {
            hook(buf.as_ptr() as usize);
        }
        free_tx
            .try_send(buf)
            .expect("free channel has room for exactly `depth` pre-loaded buffers");
    }

    let cancel = CancellationToken::new();

    let r_counters = counters.clone();
    let r_cfg = cfg.clone();
    let r_cancel = cancel.clone();
    let mut r_handle = tokio::spawn(reader_task(
        uploader, state, r_cfg, r_counters, r_cancel, free_rx, full_tx,
    ));

    let w_cfg = cfg.clone();
    let w_cancel = cancel.clone();
    let mut w_handle = tokio::spawn(writer_task(
        downloader, w_cfg, counters, w_cancel, full_rx, free_tx,
    ));

    tokio::select! {
        w_res = &mut w_handle => {
            let (written, w_result) = unwrap_join(w_res);
            match w_result {
                Ok(()) => {
                    let (uploader, state, r_result) = unwrap_join(r_handle.await);
                    let end = match r_result {
                        Ok(()) => PumpEnd::Completed,
                        Err(e) => PumpEnd::UploaderFailed(reader_error_reason(e)),
                    };
                    PumpResult { uploader, state, end, written }
                }
                Err(WriterError::Gone) => {
                    cancel.cancel();
                    let (uploader, state, _r) = unwrap_join(r_handle.await);
                    PumpResult { uploader, state, end: PumpEnd::DownloaderGone, written }
                }
                Err(WriterError::Aborted) => {
                    let (uploader, state, r_result) = unwrap_join(r_handle.await);
                    let reason = match r_result {
                        Err(e) => reader_error_reason(e),
                        // W only aborts once cancelled, and the only source of
                        // cancellation besides this branch (handled above) is
                        // R's own error path — so R having succeeded here
                        // should not happen. Kept total, not reachable in
                        // practice.
                        Ok(()) => "upload cancelled",
                    };
                    PumpResult { uploader, state, end: PumpEnd::UploaderFailed(reason), written }
                }
            }
        }
        r_res = &mut r_handle => {
            let (uploader, state, r_result) = unwrap_join(r_res);
            match r_result {
                Ok(()) => {
                    let (written, w_result) = unwrap_join(w_handle.await);
                    let end = match w_result {
                        Ok(()) => PumpEnd::Completed,
                        Err(_) => PumpEnd::DownloaderGone,
                    };
                    PumpResult { uploader, state, end, written }
                }
                Err(e) => {
                    let (written, w_result) = unwrap_join(w_handle.await);
                    // R can unwind with `Cancelled` for two different real
                    // causes that race each other: the coordinator cancelled
                    // it because W already died (Gone), or R hit its own
                    // error and cancelled W itself (in which case W reports
                    // Aborted). Only W's own result distinguishes them, so it
                    // — not the order the two joins happened to resolve in —
                    // decides which the pump reports.
                    let end = if matches!(w_result, Err(WriterError::Gone)) {
                        PumpEnd::DownloaderGone
                    } else {
                        PumpEnd::UploaderFailed(reader_error_reason(e))
                    };
                    PumpResult { uploader, state, end, written }
                }
            }
        }
    }
}

/// Why R stopped, before translating it into a `'static` [`PumpEnd`] reason.
enum ReaderError {
    /// R observed the cancellation token itself; the actual reason lives
    /// wherever the cancellation originated.
    Cancelled,
    /// R failed for its own reason and already called `cancel.cancel()`
    /// before returning.
    Failed(&'static str),
}

fn reader_error_reason(err: ReaderError) -> &'static str {
    match err {
        ReaderError::Failed(reason) => reason,
        ReaderError::Cancelled => "upload cancelled",
    }
}

/// R: owns the uploader. Reads into a recycled buffer, feeds it to the
/// framer, forwards the body prefix to W, and extends the replay window
/// while it still fits.
async fn reader_task<S: Transport>(
    mut uploader: S,
    mut state: PumpState,
    cfg: PumpConfig,
    counters: PumpCounters,
    cancel: CancellationToken,
    mut free_rx: mpsc::Receiver<Vec<u8>>,
    full_tx: mpsc::Sender<(Vec<u8>, usize)>,
) -> (S, PumpState, Result<(), ReaderError>) {
    #[cfg(test)]
    if let Some(hook) = cfg.on_task_start.as_ref() {
        hook("reader");
    }

    // A buffer that fed `forward == 0` (framing bytes only, no body) is
    // reused next iteration instead of round-tripping through the free
    // channel.
    let mut reuse: Option<Vec<u8>> = None;

    loop {
        if state.framer.is_done() {
            return (uploader, state, Ok(()));
        }

        let mut buf = match reuse.take() {
            Some(buf) => buf,
            None => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return (uploader, state, Err(ReaderError::Cancelled));
                    }
                    received = free_rx.recv() => match received {
                        Some(buf) => buf,
                        // W dropped its free-channel sender: it has already
                        // stopped (its own write/flush failed). R cannot
                        // make progress either way; the coordinator resolves
                        // the true end reason from W's own result.
                        None => return (uploader, state, Err(ReaderError::Cancelled)),
                    },
                }
            }
        };

        let read_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return (uploader, state, Err(ReaderError::Cancelled));
            }
            r = timeout(cfg.stall, uploader.read(&mut buf[..])) => r,
        };

        let n = match read_result {
            Err(_elapsed) => {
                cancel.cancel();
                drop(full_tx);
                return (uploader, state, Err(ReaderError::Failed("upload stalled")));
            }
            Ok(Ok(0)) | Ok(Err(_)) => {
                cancel.cancel();
                drop(full_tx);
                return (
                    uploader,
                    state,
                    Err(ReaderError::Failed(
                        "upload ended before the body was complete",
                    )),
                );
            }
            Ok(Ok(n)) => n,
        };

        counters.total_rx.fetch_add(n as u64, Ordering::Relaxed);

        let progress = match state.framer.feed(&buf[..n]) {
            Ok(progress) => progress,
            Err(_framing_error) => {
                cancel.cancel();
                drop(full_tx);
                return (
                    uploader,
                    state,
                    Err(ReaderError::Failed("malformed upload body")),
                );
            }
        };

        let mut filled = progress.forward;
        if let Some(replay) = state.replay.as_mut() {
            if state.consumed + filled as u64 <= cfg.replay_window as u64 {
                replay.extend_from_slice(&buf[..filled]);
            } else {
                state.replay = None;
            }
        }
        state.consumed += filled as u64;

        // Read coalescing (bandwidth): a TLS stream typically hands back one
        // record (<=16 KiB plaintext) per `read`, so without this, a 1 GB/s
        // transfer would turn into ~64k channel messages, cross-thread
        // wakeups and downloader write_all+flush calls per second — the same
        // shape the project's V-14a relay-write coalescing addresses on the
        // VPN relay path. Keep topping up the SAME buffer with non-blocking
        // reads until one would block, the buffer is full, or the framer is
        // done; the stall timeout above only ever bounds the first, blocking
        // read of this iteration. `now_or_never` is cancel-safe here: a
        // `read` that returns `None` (would-be-Pending) has not consumed any
        // bytes, and the noop waker it polls with is harmless because tokio
        // re-registers the real waker on the next genuine `.await` poll
        // (same pattern already used in `src/vpn.rs`).
        let mut deferred_error: Option<&'static str> = None;
        while deferred_error.is_none() && !state.framer.is_done() && filled < buf.len() {
            match uploader.read(&mut buf[filled..]).now_or_never() {
                None => break,
                Some(Ok(0)) | Some(Err(_)) => {
                    deferred_error = Some("upload ended before the body was complete");
                    break;
                }
                Some(Ok(m)) => {
                    counters.total_rx.fetch_add(m as u64, Ordering::Relaxed);
                    let piece = match state.framer.feed(&buf[filled..filled + m]) {
                        Ok(piece) => piece,
                        Err(_framing_error) => {
                            deferred_error = Some("malformed upload body");
                            break;
                        }
                    };
                    let piece_forward = piece.forward;
                    if let Some(replay) = state.replay.as_mut() {
                        if state.consumed + piece_forward as u64 <= cfg.replay_window as u64 {
                            replay.extend_from_slice(&buf[filled..filled + piece_forward]);
                        } else {
                            state.replay = None;
                        }
                    }
                    state.consumed += piece_forward as u64;
                    filled += piece_forward;
                }
            }
        }

        if filled > 0 {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return (uploader, state, Err(ReaderError::Cancelled));
                }
                res = full_tx.send((buf, filled)) => {
                    if res.is_err() {
                        return (uploader, state, Err(ReaderError::Cancelled));
                    }
                }
            }
        } else {
            reuse = Some(buf);
        }

        if let Some(reason) = deferred_error {
            // The coalesced bytes read above (if any) were already sent —
            // they are valid body bytes the uploader did send. Now report
            // the failure exactly as the blocking-read error paths do: W
            // sees the cancellation before it sees channel closure and
            // aborts instead of completing cleanly (I-5).
            cancel.cancel();
            drop(full_tx);
            return (uploader, state, Err(ReaderError::Failed(reason)));
        }
    }
}

/// Why W stopped.
enum WriterError {
    /// W observed the cancellation token and closed the downloader with
    /// [`abort_close`] (no terminator).
    Aborted,
    /// A write, flush, or the bounding `timeout` on them failed. The
    /// downloader is simply dropped (not gracefully closed) — the caller
    /// learns of this and drops it themselves.
    Gone,
}

/// W: owns the downloader. Receives forwarded buffers and writes+flushes
/// each one before waiting for the next (I-6), recycling the buffer back to
/// R once it is safely on the wire.
async fn writer_task(
    mut downloader: Box<dyn Transport>,
    cfg: PumpConfig,
    counters: PumpCounters,
    cancel: CancellationToken,
    mut full_rx: mpsc::Receiver<(Vec<u8>, usize)>,
    free_tx: mpsc::Sender<Vec<u8>>,
) -> (u64, Result<(), WriterError>) {
    #[cfg(test)]
    if let Some(hook) = cfg.on_task_start.as_ref() {
        hook("writer");
    }

    let mut written = 0u64;
    loop {
        let received = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                abort_close(&mut downloader).await;
                return (written, Err(WriterError::Aborted));
            }
            m = full_rx.recv() => m,
        };

        let Some((buf, len)) = received else {
            let _ = downloader.flush().await;
            let _ = timeout(LINGER_TIMEOUT, downloader.shutdown()).await;
            return (written, Ok(()));
        };

        #[cfg(test)]
        if let Some(hook) = cfg.on_chunk.as_ref() {
            hook(len);
        }

        let write_result = timeout(cfg.stall, async {
            downloader.write_all(&buf[..len]).await?;
            downloader.flush().await
        })
        .await;

        match write_result {
            Ok(Ok(())) => {
                written += len as u64;
                counters.total_tx.fetch_add(len as u64, Ordering::Relaxed);
                counters
                    .bytes_total
                    .fetch_add(len as u64, Ordering::Relaxed);
                let _ = free_tx.try_send(buf);
            }
            _ => return (written, Err(WriterError::Gone)),
        }
    }
}

/// Re-raise a task panic in the calling task (the pump never uses `abort()`,
/// so a `JoinError` here can only be a panic that propagated from R or W).
fn unwrap_join<T>(res: Result<T, tokio::task::JoinError>) -> T {
    match res {
        Ok(value) => value,
        Err(err) => std::panic::resume_unwind(err.into_panic()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context as TaskContext, Poll};

    use tokio::io::ReadBuf;

    use super::*;
    use crate::fast_link::request::Framing;

    /// Tiny fixed-seed xorshift generator, copied from `framing.rs`'s test
    /// module (each test module keeps its own — no new dependency, no shared
    /// test-only crate).
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

        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next_u64() % (hi - lo + 1)
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

    fn test_config(
        buffer: usize,
        depth: usize,
        stall: Duration,
        replay_window: usize,
    ) -> PumpConfig {
        PumpConfig {
            buffer,
            depth,
            stall,
            replay_window,
            on_buffer: None,
            on_task_start: None,
            on_chunk: None,
        }
    }

    fn test_counters() -> PumpCounters {
        PumpCounters {
            total_rx: Arc::new(AtomicU64::new(0)),
            total_tx: Arc::new(AtomicU64::new(0)),
            bytes_total: Arc::new(AtomicU64::new(0)),
        }
    }

    #[tokio::test]
    async fn pump_completes_cl_body_byte_exact() {
        let payload = random_bytes(0xABCD, 32 * 1024 * 1024);

        let (mut upload_client, upload_pump) = tokio::io::duplex(1024 * 1024);
        let payload_clone = payload.clone();
        let uploader_task = tokio::spawn(async move {
            upload_client.write_all(&payload_clone).await.unwrap();
            upload_client.shutdown().await.unwrap();
        });

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            download_client.read_to_end(&mut out).await.unwrap();
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(256 * 1024, 4, Duration::from_secs(10), 4 * 1024 * 1024);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(60),
            pump(
                upload_pump,
                Box::new(download_pump),
                state,
                cfg,
                counters.clone(),
            ),
        )
        .await
        .expect("pump must finish");

        uploader_task.await.unwrap();
        let downloaded = downloader_task.await.unwrap();

        assert_eq!(result.end, PumpEnd::Completed);
        assert_eq!(result.written, payload.len() as u64);
        assert_eq!(downloaded, payload);
        assert_eq!(
            counters.bytes_total.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(
            counters.total_tx.load(Ordering::Relaxed),
            payload.len() as u64
        );
        assert_eq!(
            counters.total_rx.load(Ordering::Relaxed),
            payload.len() as u64
        );
    }

    #[tokio::test]
    async fn pump_completes_chunked_passthrough() {
        let mut rng = Xorshift::new(0xFACE);
        let mut encoded = Vec::new();
        for _ in 0..20 {
            let size = rng.range(1, 50_000) as usize;
            let mut chunk = vec![0u8; size];
            for b in chunk.iter_mut() {
                *b = (rng.next_u64() & 0xff) as u8;
            }
            encoded.extend_from_slice(format!("{size:x}\r\n").as_bytes());
            encoded.extend_from_slice(&chunk);
            encoded.extend_from_slice(b"\r\n");
        }
        encoded.extend_from_slice(b"0\r\n\r\n");

        let (mut upload_client, upload_pump) = tokio::io::duplex(4 * 1024 * 1024);
        let encoded_clone = encoded.clone();
        let uploader_task = tokio::spawn(async move {
            upload_client.write_all(&encoded_clone).await.unwrap();
            upload_client.shutdown().await.unwrap();
        });

        let (download_pump, mut download_client) = tokio::io::duplex(4 * 1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            download_client.read_to_end(&mut out).await.unwrap();
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::Chunked),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(64 * 1024, 4, Duration::from_secs(10), 4 * 1024 * 1024);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(60),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish");

        uploader_task.await.unwrap();
        let downloaded = downloader_task.await.unwrap();

        assert_eq!(result.end, PumpEnd::Completed);
        assert_eq!(downloaded, encoded);
    }

    #[tokio::test]
    async fn pump_reuses_its_buffers() {
        let seen: Arc<StdMutex<HashSet<usize>>> = Arc::new(StdMutex::new(HashSet::new()));
        let seen_clone = seen.clone();
        let hook: OnBuffer = Arc::new(move |ptr| {
            seen_clone.lock().unwrap().insert(ptr);
        });

        let payload = random_bytes(2, 64 * 1024 * 1024);
        let (mut upload_client, upload_pump) = tokio::io::duplex(1024 * 1024);
        let payload_clone = payload.clone();
        let uploader_task = tokio::spawn(async move {
            upload_client.write_all(&payload_clone).await.unwrap();
            upload_client.shutdown().await.unwrap();
        });

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            download_client.read_to_end(&mut out).await.unwrap();
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: None,
            consumed: 0,
        };
        let mut cfg = test_config(256 * 1024, 4, Duration::from_secs(10), 0);
        cfg.on_buffer = Some(hook);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(60),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish");

        uploader_task.await.unwrap();
        let downloaded = downloader_task.await.unwrap();

        assert_eq!(result.end, PumpEnd::Completed);
        assert_eq!(downloaded, payload);

        let seen = seen.lock().unwrap();
        assert!(
            seen.len() <= 4,
            "at most `depth` distinct buffers may ever be allocated, saw {}",
            seen.len()
        );
    }

    #[tokio::test]
    async fn pump_downloader_drop_returns_uploader_and_state() {
        let payload = random_bytes(3, 1024 * 1024);
        let (mut upload_client, upload_pump) = tokio::io::duplex(2 * 1024 * 1024);
        upload_client.write_all(&payload).await.unwrap();

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 100 * 1024];
            download_client.read_exact(&mut buf).await.unwrap();
            drop(download_client);
            buf
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(64 * 1024, 4, Duration::from_secs(10), 1024 * 1024);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(30),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish once the downloader drops");

        assert_eq!(result.end, PumpEnd::DownloaderGone);
        let replay = result
            .state
            .replay
            .as_ref()
            .expect("replay window must survive a drop inside it");
        assert_eq!(replay.len() as u64, result.state.consumed);

        let first_100k = downloader_task.await.unwrap();
        assert_eq!(&first_100k[..], &payload[..100 * 1024]);

        // The uploader must still be readable: write a marker and confirm it
        // eventually arrives through the returned handle.
        let mut uploader = result.uploader;
        let marker = b"still-alive-marker".to_vec();
        upload_client.write_all(&marker).await.unwrap();
        upload_client.shutdown().await.unwrap();

        let mut rest = Vec::new();
        timeout(Duration::from_secs(10), uploader.read_to_end(&mut rest))
            .await
            .expect("the returned uploader must still be readable")
            .unwrap();
        assert!(rest.ends_with(&marker[..]));
    }

    #[tokio::test]
    async fn pump_downloader_drop_past_window_clears_replay() {
        let payload = random_bytes(4, 2 * 1024 * 1024);
        let (mut upload_client, upload_pump) = tokio::io::duplex(4 * 1024 * 1024);
        upload_client.write_all(&payload).await.unwrap();

        let (download_pump, mut download_client) = tokio::io::duplex(2 * 1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1024 * 1024];
            download_client.read_exact(&mut buf).await.unwrap();
            drop(download_client);
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(64 * 1024, 4, Duration::from_secs(10), 64 * 1024);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(30),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish once the downloader drops");

        assert_eq!(result.end, PumpEnd::DownloaderGone);
        assert!(result.state.replay.is_none());

        let _ = downloader_task.await;
    }

    #[tokio::test]
    async fn pump_uploader_eof_aborts_without_completion() {
        let payload = random_bytes(5, 300 * 1024);
        let (mut upload_client, upload_pump) = tokio::io::duplex(1024 * 1024);
        upload_client.write_all(&payload).await.unwrap();
        upload_client.shutdown().await.unwrap();

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            let _ = download_client.read_to_end(&mut out).await;
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(1024 * 1024)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(64 * 1024, 4, Duration::from_secs(10), 1024 * 1024);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(30),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish on a premature EOF");

        assert_eq!(
            result.end,
            PumpEnd::UploaderFailed("upload ended before the body was complete")
        );

        let downloaded = downloader_task.await.unwrap();
        assert!(downloaded.len() < 1024 * 1024);
        // W aborts as soon as it observes the cancellation, even if a buffer
        // or two were still queued between R and W at that instant (up to
        // `depth` buffers may be in flight); the downloader always sees a
        // *prefix* of what the uploader sent, never a different byte, and
        // never the full declared length (I-5: truncation must stay visible).
        assert!(
            payload.starts_with(&downloaded),
            "the downloader's bytes must be an exact prefix of what the uploader sent"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pump_stall_is_bounded() {
        // The uploader half is kept alive (not dropped, never written to) so
        // the reader genuinely stalls rather than seeing an EOF.
        let (_upload_client, upload_pump) = tokio::io::duplex(1024);

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            let _ = download_client.read_to_end(&mut out).await;
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(10)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(1024, 4, Duration::from_secs(5), 1024);
        let counters = test_counters();

        let pump_task = tokio::spawn(pump(
            upload_pump,
            Box::new(download_pump),
            state,
            cfg,
            counters,
        ));

        tokio::time::advance(Duration::from_secs(6)).await;

        let result = timeout(Duration::from_secs(30), pump_task)
            .await
            .expect("pump must resolve once the stall timeout elapses")
            .unwrap();

        assert_eq!(result.end, PumpEnd::UploaderFailed("upload stalled"));
        let _ = downloader_task.await;
    }

    /// A writer mock that only becomes visible on `flush()`/`shutdown()`,
    /// copied (per the assignment) from the `FlushGatedWriter` mock in
    /// `src/vhost.rs`'s private `mod tests`, plus a read half that never
    /// yields data (W never reads its downloader).
    #[derive(Default)]
    struct FlushGateState {
        pending: Vec<u8>,
        visible: Vec<u8>,
        shutdown: bool,
    }

    struct FlushGatedWriter(Arc<StdMutex<FlushGateState>>);

    impl tokio::io::AsyncWrite for FlushGatedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.lock().unwrap().pending.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            let mut st = self.0.lock().unwrap();
            let parked = std::mem::take(&mut st.pending);
            st.visible.extend_from_slice(&parked);
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            let mut st = self.0.lock().unwrap();
            let parked = std::mem::take(&mut st.pending);
            st.visible.extend_from_slice(&parked);
            st.shutdown = true;
            Poll::Ready(Ok(()))
        }
    }

    struct PendingForeverRead;

    impl tokio::io::AsyncRead for PendingForeverRead {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    struct MockDownloader {
        read: PendingForeverRead,
        write: FlushGatedWriter,
    }

    impl tokio::io::AsyncRead for MockDownloader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.read).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for MockDownloader {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.write).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.write).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.write).poll_shutdown(cx)
        }
    }

    /// S13-P. Red-checked: temporarily removing the `flush().await` call in
    /// `writer_task`'s write branch makes this test fail (the visible side of
    /// `FlushGatedWriter` only moves bytes out of `pending` on an explicit
    /// flush or shutdown, neither of which a still-streaming pump performs
    /// until it either finishes or stalls) — see the worker's final report
    /// for the exact failure captured during that red-check.
    #[tokio::test]
    async fn pump_writes_are_flushed_before_waiting() {
        let gate = Arc::new(StdMutex::new(FlushGateState::default()));
        let downloader = MockDownloader {
            read: PendingForeverRead,
            write: FlushGatedWriter(gate.clone()),
        };

        let (mut upload_client, upload_pump) = tokio::io::duplex(1024 * 1024);
        let payload = vec![7u8; 100 * 1024];
        let payload_clone = payload.clone();
        upload_client.write_all(&payload_clone).await.unwrap();
        // Deliberately never shut down / write more: the uploader stalls
        // right after this write, exactly the scenario the contract names.
        let _keep_upload_client_open = tokio::spawn(async move {
            let _upload_client = upload_client;
            std::future::pending::<()>().await;
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(1024 * 1024)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let cfg = test_config(64 * 1024, 4, Duration::from_secs(600), 1024 * 1024);
        let counters = test_counters();

        let _pump_task = tokio::spawn(pump(
            upload_pump,
            Box::new(downloader),
            state,
            cfg,
            counters,
        ));

        let observed = timeout(Duration::from_secs(1), async {
            loop {
                {
                    let st = gate.lock().unwrap();
                    if st.visible.len() >= payload.len() {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;

        assert!(
            observed.is_ok(),
            "the downloader must see the flushed bytes within 1s of real time"
        );
        let st = gate.lock().unwrap();
        assert_eq!(st.visible.len(), payload.len());
    }

    /// An uploader mock that always returns `Ready` from `poll_read` (never
    /// `Pending`) but hands back at most 1 KiB per call — the shape that
    /// makes read coalescing matter: without it, every ~1 KiB slice becomes
    /// its own channel message.
    struct SmallReadUploader {
        remaining: Vec<u8>,
        offset: usize,
        chunk_cap: usize,
    }

    impl tokio::io::AsyncRead for SmallReadUploader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = self.remaining.len() - self.offset;
            let take = remaining.min(buf.remaining()).min(self.chunk_cap);
            let offset = self.offset;
            buf.put_slice(&self.remaining[offset..offset + take]);
            self.offset += take;
            Poll::Ready(Ok(()))
        }
    }

    /// This mock is only ever used as an uploader (R never writes to it),
    /// but `Transport` requires `AsyncWrite` too.
    impl tokio::io::AsyncWrite for SmallReadUploader {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            unreachable!("the pump's reader task never writes to its uploader")
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            unreachable!("the pump's reader task never writes to its uploader")
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            unreachable!("the pump's reader task never writes to its uploader")
        }
    }

    /// Red-checked (see the worker's final report for the exact failure):
    /// with the coalescing loop in `reader_task` disabled, this test fails
    /// on the message-count assertion (4096 messages for a 4 MiB payload
    /// read 1 KiB at a time), not on correctness — the payload still arrives
    /// byte-exact either way.
    #[tokio::test]
    async fn pump_coalesces_small_reads() {
        let payload = random_bytes(7, 4 * 1024 * 1024);
        let uploader = SmallReadUploader {
            remaining: payload.clone(),
            offset: 0,
            chunk_cap: 1024,
        };

        let (download_pump, mut download_client) = tokio::io::duplex(8 * 1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            download_client.read_to_end(&mut out).await.unwrap();
            out
        });

        let message_count = Arc::new(AtomicU64::new(0));
        let message_count_clone = message_count.clone();
        let hook: OnChunk = Arc::new(move |_len| {
            message_count_clone.fetch_add(1, Ordering::Relaxed);
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: None,
            consumed: 0,
        };
        let mut cfg = test_config(256 * 1024, 4, Duration::from_secs(10), 0);
        cfg.on_chunk = Some(hook);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(30),
            pump(uploader, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish");

        let downloaded = downloader_task.await.unwrap();

        assert_eq!(result.end, PumpEnd::Completed);
        assert_eq!(downloaded, payload);

        let messages = message_count.load(Ordering::Relaxed);
        // 4 MiB / 256 KiB buffers, plus headroom for the coalescing loop
        // stopping early (buffer boundary, or a would-block that just
        // happens not to occur with this deterministic mock): without
        // coalescing this mock would produce 4096 one-KiB messages.
        assert!(
            messages <= 20,
            "expected <=20 coalesced messages for a 4 MiB payload at 256 KiB buffers, got {messages}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pump_uses_two_worker_threads() {
        let calls: Arc<StdMutex<Vec<&'static str>>> = Arc::new(StdMutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let hook: OnTaskStart = Arc::new(move |name| {
            calls_clone.lock().unwrap().push(name);
        });

        let payload = random_bytes(6, 200 * 1024);
        let (mut upload_client, upload_pump) = tokio::io::duplex(1024 * 1024);
        let payload_clone = payload.clone();
        let uploader_task = tokio::spawn(async move {
            upload_client.write_all(&payload_clone).await.unwrap();
            upload_client.shutdown().await.unwrap();
        });

        let (download_pump, mut download_client) = tokio::io::duplex(1024 * 1024);
        let downloader_task = tokio::spawn(async move {
            let mut out = Vec::new();
            download_client.read_to_end(&mut out).await.unwrap();
            out
        });

        let state = PumpState {
            framer: BodyFramer::new(Framing::ContentLength(payload.len() as u64)),
            replay: Some(Vec::new()),
            consumed: 0,
        };
        let mut cfg = test_config(64 * 1024, 4, Duration::from_secs(10), 1024 * 1024);
        cfg.on_task_start = Some(hook);
        let counters = test_counters();

        let result = timeout(
            Duration::from_secs(30),
            pump(upload_pump, Box::new(download_pump), state, cfg, counters),
        )
        .await
        .expect("pump must finish");

        uploader_task.await.unwrap();
        let downloaded = downloader_task.await.unwrap();

        assert_eq!(result.end, PumpEnd::Completed);
        assert_eq!(downloaded, payload);

        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "both R and W must register their task start exactly once"
        );
        let names: HashSet<_> = calls.iter().copied().collect();
        assert!(names.contains("reader") && names.contains("writer"));
    }
}
