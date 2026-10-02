//! Stream multiplexing over a single TCP connection, built on [`yamux`].
//!
//! `bore` forwards every proxied connection as an independent substream over one
//! long-lived TCP connection between client and server. This removes the TCP and
//! authentication handshake that the previous protocol paid for every proxied
//! connection.
//!
//! The `yamux` [`Connection`] is poll-based and must be driven by a single owner.
//! This module hides that behind a small actor: a background task owns the
//! connection, accepts inbound substreams onto a channel ([`Acceptor`]), and
//! services outbound-open requests sent over another channel ([`Opener`]).

use std::future::poll_fn;
#[cfg(feature = "ssh-gateway")]
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::task::AtomicWaker;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;
use yamux::{Config, Connection, Mode};

/// A multiplexed substream exposing Tokio's async I/O traits.
pub type Stream = Compat<TrackedStream>;

/// Everything that can still make a connection useful, counted in one place:
/// every live [`Opener`], the [`Acceptor`], and every substream handed out.
///
/// The `yamux::Connection` lives in a detached driver task, so nothing the
/// caller holds owns it and nothing the caller drops takes it away. Before this
/// existed the driver left its loop only on `Step::Done` — which needs the
/// *peer* to close — and both peers run this same driver, so neither ever
/// initiated it: a mutual liveness deadlock that held one `ESTABLISHED` socket
/// per finished connection at both ends. MEASURED on the real path: a VPN
/// connector with `--auto-reconnect` leaked exactly one control connection per
/// reconnect (1→2→3→4, none reaped in 120 s), confirmed independently through
/// `/proc/<pid>/fd`. See `docs/vpn/VPN_CTRL_CONN_LEAK.md`.
#[derive(Debug, Default)]
struct Liveness {
    handles: AtomicUsize,
    waker: AtomicWaker,
}

/// One count on a connection's [`Liveness`]. Held by each `Opener`, by the
/// `Acceptor`, and by every substream; dropping the last one wakes the driver,
/// which then closes the connection.
///
/// Counting SUBSTREAMS is what makes this safe: substreams routinely outlive
/// the `Opener` (the relay hands a stream to a task and drops the opener), so
/// "close when the opener is gone" would tear down live traffic. Only a
/// connection with no handles AND no streams has nothing left to do.
#[derive(Debug)]
struct ConnRef(Arc<Liveness>);

impl ConnRef {
    fn new(liveness: &Arc<Liveness>) -> Self {
        liveness.handles.fetch_add(1, Ordering::Relaxed);
        ConnRef(Arc::clone(liveness))
    }
}

impl Clone for ConnRef {
    fn clone(&self) -> Self {
        ConnRef::new(&self.0)
    }
}

impl Drop for ConnRef {
    fn drop(&mut self) {
        // `AcqRel` so the driver's `Acquire` load cannot observe a stale count:
        // the wake and the decrement must not be reordered around each other.
        if self.0.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.waker.wake();
        }
    }
}

/// A `yamux` substream that keeps its connection's driver alive for as long as
/// it exists.
///
/// This is the inner type of [`Stream`] and is otherwise transparent: every
/// read/write delegates to the substream unchanged. It exists only to carry a
/// [`ConnRef`], so a caller that holds a substream after dropping the `Opener`
/// and `Acceptor` — the ordinary relay shape — still owns a live connection.
#[derive(Debug)]
pub struct TrackedStream {
    inner: yamux::Stream,
    _alive: ConnRef,
}

impl futures_util::io::AsyncRead for TrackedStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl futures_util::io::AsyncWrite for TrackedStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

/// Any byte stream `yamux` can run over (a plain TCP socket, a TLS stream, ...).
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Transport for T {}

/// When this connection last heard from its peer, and the switch that kills it.
///
/// Liveness is measured at the TRANSPORT, below yamux: every byte the socket
/// yields stamps it, whatever substream (or yamux control frame) it belongs to.
/// That is the property that makes a liveness deadline safe on a busy tunnel. A
/// control frame queues behind bulk data in the peer's socket buffer (≈4 MiB at
/// 1 Mbit/s is ≈32 s), so a deadline on control MESSAGES would kill a healthy
/// congested tunnel; a deadline on BYTES cannot, because a path that is moving
/// data is by definition delivering bytes. A dead path delivers none.
///
/// Field report (2026-10-02): after a ~20 s outage with an ISP IP change a vhost
/// client stayed down ~16 minutes. Nothing in bore noticed the dead connection;
/// the kernel did, after `tcp_retries2` (≈924 s) — the client's own heartbeats
/// kept unacked data in flight, so SO_KEEPALIVE never fired.
#[derive(Debug)]
struct Activity {
    base: tokio::time::Instant,
    /// Milliseconds since `base` at the last non-empty read. `Relaxed` is
    /// enough: it is a monotonic hint, nothing is published through it.
    last_inbound_ms: AtomicU64,
    /// Cancelling it ends the driver task WITHOUT the graceful close: the
    /// `yamux::Connection` is dropped, which closes and wakes every substream
    /// (yamux 0.13 `Active::drop_all_streams`), and the socket is released.
    cancel: CancellationToken,
}

impl Activity {
    fn new() -> Self {
        Self {
            base: tokio::time::Instant::now(),
            last_inbound_ms: AtomicU64::new(0),
            cancel: CancellationToken::new(),
        }
    }

    fn stamp(&self) {
        let now = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_inbound_ms.store(now, Ordering::Relaxed);
    }
}

/// Liveness handle for one multiplexed connection; cheap to clone.
///
/// Obtained from [`Opener::activity`] or [`Acceptor::activity`].
#[derive(Clone, Debug)]
pub struct ConnActivity(Arc<Activity>);

impl ConnActivity {
    /// Time since the peer last delivered ANY byte on this connection (or since
    /// the connection was set up, if it never did).
    pub fn inbound_idle(&self) -> Duration {
        let last = Duration::from_millis(self.0.last_inbound_ms.load(Ordering::Relaxed));
        self.0.base.elapsed().saturating_sub(last)
    }

    /// Whether a liveness deadline has been reached. `None` never fires — the
    /// "this peer never promised to talk" case, which must keep the legacy path.
    pub fn reap_due(&self, deadline: Option<Duration>) -> bool {
        deadline.is_some_and(|deadline| self.inbound_idle() >= deadline)
    }

    /// Tear the connection down NOW: every substream ends promptly and the
    /// socket is released. Reserved for liveness trips — a path already proven
    /// dead, whose graceful close would park on a socket that never drains.
    /// Clean exits keep the graceful close. Idempotent.
    pub fn terminate(&self) {
        self.0.cancel.cancel();
    }

    /// Whether [`terminate`](Self::terminate) has been called.
    pub fn is_terminated(&self) -> bool {
        self.0.cancel.is_cancelled()
    }

    /// Whether `other` watches the same connection as `self`.
    pub fn same_connection(&self, other: &ConnActivity) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The socket as the driver sees it: unchanged, except that every read that
/// delivers bytes stamps the connection's [`Activity`].
struct ActivityIo<S> {
    inner: S,
    activity: Arc<Activity>,
}

impl<S: AsyncRead + Unpin> AsyncRead for ActivityIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() > before {
            self.activity.stamp();
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ActivityIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Readiness marker the substream opener writes immediately after opening.
///
/// `yamux` opens substreams lazily: the peer is not notified until the opener
/// sends its first frame. Forwarded connections must be established before any
/// payload flows (the local service may speak first), so the opener writes this
/// byte to announce the substream, and the acceptor consumes it before splicing.
pub const STREAM_READY: u8 = 0;

/// Generous cap on concurrent substreams. The meaningful bound on proxied
/// connections is enforced by the server's `--max-conns` semaphore; this only
/// keeps `yamux` itself from ever being the limiting factor.
///
/// `yamux` asserts `max_connection_receive_window >= max_num_streams * 256 KiB`
/// (computed even when the window is unbounded). On 32-bit targets that product
/// must stay under `usize::MAX` (~4 GiB), so the cap is lowered there — still
/// far above the default `--max-conns` of 1024.
#[cfg(target_pointer_width = "64")]
const MAX_NUM_STREAMS: usize = 1 << 16;
#[cfg(not(target_pointer_width = "64"))]
const MAX_NUM_STREAMS: usize = 1 << 13;

// Guard against re-introducing the 32-bit overflow: this is exactly the product
// `yamux` multiplies (and would panic on) in its config assertions.
const _: () = assert!(
    MAX_NUM_STREAMS
        .checked_mul(yamux::DEFAULT_CREDIT as usize)
        .is_some(),
    "MAX_NUM_STREAMS * yamux::DEFAULT_CREDIT must not overflow usize on this target",
);

fn config() -> Config {
    let mut cfg = Config::default();
    // Let each stream's receive window auto-tune to the bandwidth-delay product
    // for throughput; concurrency (and thus total memory) is bounded elsewhere.
    cfg.set_max_connection_receive_window(None);
    cfg.set_max_num_streams(MAX_NUM_STREAMS);
    cfg
}

fn disconnected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "multiplexer connection closed")
}

/// Handle for opening new outbound substreams. Cheap to clone.
#[derive(Clone)]
pub struct Opener {
    requests: mpsc::Sender<oneshot::Sender<io::Result<Stream>>>,
    /// Keeps the connection's driver alive: a pool that holds only an opener
    /// (the server's `CarrierPool` does exactly that) still owns a connection.
    _alive: ConnRef,
    activity: ConnActivity,
}

impl Opener {
    /// Liveness handle of the connection this opener belongs to.
    pub fn activity(&self) -> ConnActivity {
        self.activity.clone()
    }

    /// Open a new outbound substream to the peer.
    pub async fn open(&self) -> io::Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.requests.send(tx).await.map_err(|_| disconnected())?;
        rx.await.map_err(|_| disconnected())?
    }
}

/// Any stream usable as a forwarded connection's data path once opened and
/// readiness-marked. Boxed to erase the underlying transport (a yamux
/// substream today; an SSH channel in a later phase).
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// A boxed, transport-erased forwarded-connection stream.
pub type LinkStream = Box<dyn Duplex>;

/// Opens a fresh channel toward an SSH gateway's registered peer, erasing the
/// `russh` `Handle`/channel-open plumbing behind a plain async call.
///
/// Native `async fn` in a trait isn't dyn-compatible (needed here for
/// `Arc<dyn ChannelOpen>` in [`LinkOpener::Ssh`]) without the `async-trait`
/// crate or nightly; hand-desugaring to a boxed future avoids adding a
/// dependency for one method.
#[cfg(feature = "ssh-gateway")]
pub trait ChannelOpen: Send + Sync {
    /// Open a channel and return it boxed as a [`LinkStream`]. `forward_ip`,
    /// when known, is the originating peer's address — implementors thread
    /// it into the channel-open request itself (SSH has no separate
    /// [`STREAM_READY`] marker to carry it; SSH-sourced links must NOT write
    /// that marker at all, see [`LinkOpener::open_ready`]).
    ///
    /// `caller`, when known, is the proxied connection's full source address.
    /// Unlike `forward_ip` (whose presence is a *wire* signal on the mux path
    /// — Some ⟺ the native client asked for webserver logging — and which
    /// carries the IP only), `caller` exists solely so SSH links can fill the
    /// RFC 4254 `forwarded-tcpip` originator address AND port truthfully,
    /// regardless of any logging option. It never touches the mux wire.
    fn open(
        &self,
        forward_ip: Option<&str>,
        caller: Option<std::net::SocketAddr>,
    ) -> Pin<Box<dyn Future<Output = io::Result<LinkStream>> + Send + '_>>;
}

/// How to open a fresh substream toward a tunnel's registered peer. Wraps the
/// transport so the public/vhost/secret relay paths don't need to know
/// whether the peer connected over the classic yamux mux or an SSH gateway
/// channel. [`CarrierPool`](crate::pool::CarrierPool) stores this instead of
/// a bare [`Opener`].
#[derive(Clone)]
pub enum LinkOpener {
    /// The classic yamux-multiplexed substream opener.
    Mux(Opener),
    /// An SSH gateway forwarded/direct-tcpip channel opener.
    #[cfg(feature = "ssh-gateway")]
    Ssh(Arc<dyn ChannelOpen>),
}

impl LinkOpener {
    /// Open a link without announcing it. Only meaningful for callers that
    /// need to interleave more setup before the peer sees any data (e.g.
    /// picking between a direct and a relayed path and announcing readiness
    /// once on whichever succeeded). Most callers want
    /// [`LinkOpener::open_ready`] instead.
    ///
    /// Note this is NOT a no-op for SSH links: unlike the mux path, an SSH
    /// channel open is itself the peer-visible announcement (there is no
    /// separate marker to skip), so `open` and `open_ready` do the same
    /// amount of work for `LinkOpener::Ssh` — the distinction only matters
    /// for `LinkOpener::Mux`.
    pub async fn open(&self) -> io::Result<LinkStream> {
        match self {
            LinkOpener::Mux(opener) => opener.open().await.map(|s| Box::new(s) as LinkStream),
            #[cfg(feature = "ssh-gateway")]
            LinkOpener::Ssh(opener) => opener.open(None, None).await,
        }
    }

    /// Open a link, announce it (write the STREAM_READY marker with the
    /// optional caller IP for a mux link; thread the caller address into the
    /// channel-open request itself for an SSH link), and return the boxed
    /// stream ready to splice. A failure at any step is reported as one
    /// error so carrier-failover callers can treat it identically to an
    /// open failure.
    ///
    /// SSH links skip the marker (I-4): a stock `ssh` client on the other
    /// end doesn't know about it and would see it as leading garbage on the
    /// forwarded connection.
    ///
    /// `caller` is used only by SSH links (the RFC 4254 originator fields);
    /// the mux wire is governed exclusively by `forward_ip` and stays
    /// byte-identical whether or not `caller` is passed.
    pub async fn open_ready(
        &self,
        forward_ip: Option<&str>,
        caller: Option<std::net::SocketAddr>,
    ) -> io::Result<LinkStream> {
        #[cfg(not(feature = "ssh-gateway"))]
        let _ = caller;
        match self {
            LinkOpener::Mux(opener) => {
                let mut stream = opener.open().await?;
                write_stream_ready(&mut stream, forward_ip).await?;
                stream.flush().await?;
                Ok(Box::new(stream))
            }
            #[cfg(feature = "ssh-gateway")]
            LinkOpener::Ssh(opener) => opener.open(forward_ip, caller).await,
        }
    }
}

/// Handle for accepting inbound substreams opened by the peer.
pub struct Acceptor {
    inbound: mpsc::Receiver<Stream>,
    /// Keeps the connection's driver alive; see [`Liveness`].
    _alive: ConnRef,
    activity: ConnActivity,
}

impl Acceptor {
    /// Liveness handle of the connection this acceptor belongs to.
    pub fn activity(&self) -> ConnActivity {
        self.activity.clone()
    }

    /// Wait for the next inbound substream, or `None` once the connection closes.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.inbound.recv().await
    }
}

/// Start multiplexing as the connection initiator (dialer).
pub fn client<S: Transport>(socket: S) -> (Opener, Acceptor) {
    let activity = Arc::new(Activity::new());
    let io = ActivityIo {
        inner: socket,
        activity: Arc::clone(&activity),
    };
    spawn_driver_inner(
        Connection::new(io.compat(), config(), Mode::Client),
        activity,
        None,
    )
}

/// Start a client connection whose yamux driver belongs to a transfer-link
/// lifecycle scope. The scope is supplied before the driver is spawned, so
/// shutdown can cancel and join the driver just like data-plane child tasks.
pub(crate) fn client_scoped<S: Transport>(
    socket: S,
    scope: Arc<crate::client::ClientScope>,
) -> (Opener, Acceptor) {
    let activity = Arc::new(Activity::new());
    let io = ActivityIo {
        inner: socket,
        activity: Arc::clone(&activity),
    };
    spawn_driver_inner(
        Connection::new(io.compat(), config(), Mode::Client),
        activity,
        Some(scope),
    )
}

/// Start multiplexing as the connection responder (listener).
pub fn server<S: Transport>(socket: S) -> (Opener, Acceptor) {
    let activity = Arc::new(Activity::new());
    let io = ActivityIo {
        inner: socket,
        activity: Arc::clone(&activity),
    };
    spawn_driver_inner(
        Connection::new(io.compat(), config(), Mode::Server),
        activity,
        None,
    )
}

fn spawn_driver_inner<S: Transport>(
    conn: Connection<Compat<ActivityIo<S>>>,
    activity: Arc<Activity>,
    scope: Option<Arc<crate::client::ClientScope>>,
) -> (Opener, Acceptor) {
    let (open_tx, open_rx) = mpsc::channel(32);
    let (inbound_tx, inbound_rx) = mpsc::channel(32);
    let liveness: Arc<Liveness> = Arc::default();
    let handle = ConnActivity(Arc::clone(&activity));
    // Both handles are counted BEFORE the driver starts, so the driver can
    // never observe a zero count in the gap between spawning and returning.
    let opener = Opener {
        requests: open_tx,
        _alive: ConnRef::new(&liveness),
        activity: handle.clone(),
    };
    let acceptor = Acceptor {
        inbound: inbound_rx,
        _alive: ConnRef::new(&liveness),
        activity: handle,
    };
    let scope_for_spawn = scope.clone();
    let terminate = activity.cancel.clone();
    let task = async move {
        // `terminate` drops `drive` (and with it the `yamux::Connection`)
        // mid-flight: no graceful close, every substream is closed and woken.
        if let Some(scope) = scope {
            let cancel = scope.token();
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = terminate.cancelled() => {}
                _ = drive(conn, open_rx, inbound_tx, liveness) => {}
            }
        } else {
            tokio::select! {
                _ = terminate.cancelled() => {}
                _ = drive(conn, open_rx, inbound_tx, liveness) => {}
            }
        }
    };
    // A scoped connection is always registered before the task is started. The
    // legacy branch preserves the detached driver behavior byte-for-byte.
    match scope_for_spawn {
        Some(scope) => {
            let _ = scope.spawn(task);
        }
        None => {
            tokio::spawn(task);
        }
    }
    (opener, acceptor)
}

/// Drive the connection: this is the single owner of the `yamux::Connection`.
///
/// `yamux` only makes progress (for inbound, outbound, and already-open streams)
/// while the connection is polled, and every poll method needs `&mut`. So all of
/// it happens in one task, interleaving outbound-open requests with the inbound
/// driver inside a single `poll_fn`.
async fn drive<S: Transport>(
    mut conn: Connection<Compat<S>>,
    mut open_rx: mpsc::Receiver<oneshot::Sender<io::Result<Stream>>>,
    inbound_tx: mpsc::Sender<Stream>,
    liveness: Arc<Liveness>,
) {
    enum Step {
        Inbound(yamux::Stream),
        Opened(Result<yamux::Stream, yamux::ConnectionError>),
        Done,
    }

    // An open request currently being serviced by `poll_new_outbound`.
    let mut pending: Option<oneshot::Sender<io::Result<Stream>>> = None;
    // Stop pulling new open requests once every `Opener` has been dropped, but
    // keep driving the connection for streams that are still alive.
    let mut openers_gone = false;

    loop {
        let step = poll_fn(|cx| {
            // Register FIRST, then read the count: the reverse order can miss
            // the wake of a handle dropped between the two.
            liveness.waker.register(cx.waker());
            if liveness.handles.load(Ordering::Acquire) == 0 {
                // No opener, no acceptor, no substream: nothing can ever ask
                // this connection for anything again. Closing here is what
                // makes the peer's own driver reach `Step::Done`, so one side
                // noticing releases the socket at BOTH ends.
                return Poll::Ready(Step::Done);
            }
            if pending.is_none() && !openers_gone {
                match open_rx.poll_recv(cx) {
                    Poll::Ready(Some(reply)) => pending = Some(reply),
                    Poll::Ready(None) => openers_gone = true,
                    Poll::Pending => {}
                }
            }
            if pending.is_some() {
                if let Poll::Ready(result) = conn.poll_new_outbound(cx) {
                    return Poll::Ready(Step::Opened(result));
                }
            }
            match conn.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(stream))) => Poll::Ready(Step::Inbound(stream)),
                Poll::Ready(Some(Err(_)) | None) => Poll::Ready(Step::Done),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;

        match step {
            Step::Opened(result) => {
                if let Some(reply) = pending.take() {
                    let _ = reply.send(
                        result
                            .map(|s| track(s, &liveness))
                            .map_err(io::Error::other),
                    );
                }
            }
            Step::Inbound(stream) => {
                // If the `Acceptor` is gone, drop the stream but keep driving for
                // any streams still in flight.
                let _ = inbound_tx.send(track(stream, &liveness)).await;
            }
            Step::Done => break,
        }
    }

    let _ = poll_fn(|cx| conn.poll_close(cx)).await;
}

/// Hand a raw `yamux` substream to a caller with a [`ConnRef`] attached, so the
/// connection stays alive exactly as long as the substream does.
fn track(stream: yamux::Stream, liveness: &Arc<Liveness>) -> Stream {
    TrackedStream {
        inner: stream,
        _alive: ConnRef::new(liveness),
    }
    .compat()
}

/// Write the STREAM_READY marker with optional caller IP forwarding.
///
/// **Legacy (webserver_log=false):** writes `[0x00]` (BYTE-IDENTICAL to today).
///
/// **Extended (webserver_log=true):** writes `[0x00, ip_len:u8, ip_utf8]` where
/// `ip` is a string like "203.0.113.7:54321" (caller IP:port). If `forward_ip` is
/// `Some("")`, writes `ip_len=0` (server couldn't determine IP). If the IP is >255
/// bytes, truncates to 255.
pub async fn write_stream_ready<W: AsyncWrite + Unpin>(
    w: &mut W,
    forward_ip: Option<&str>,
) -> io::Result<()> {
    match forward_ip {
        None => {
            // Legacy path: write only the STREAM_READY marker.
            w.write_all(&[STREAM_READY]).await?;
        }
        Some(ip) => {
            // Extended path: write marker, length, then IP bytes (capped at 255).
            let ip_bytes = ip.as_bytes();
            let ip_len = (ip_bytes.len().min(255)) as u8;
            w.write_all(&[STREAM_READY]).await?;
            w.write_all(&[ip_len]).await?;
            w.write_all(&ip_bytes[..ip_len as usize]).await?;
        }
    }
    Ok(())
}

/// Read the STREAM_READY marker with optional caller IP.
///
/// **Legacy (expect_ip=false):** reads exactly 1 byte (the marker), validates it
/// is `STREAM_READY`, returns `Ok(None)`. Byte-identical to today's behavior.
///
/// **Extended (expect_ip=true):** reads the marker byte, then reads `ip_len:u8`
/// followed by `ip_len` bytes, returning `Ok(Some(ip_string))`. If `ip_len=0`,
/// returns `Ok(Some(String::new()))` (empty string signals "IP unknown").
///
/// On any I/O error or marker validation failure, returns `Err`.
pub async fn read_stream_ready<R: AsyncRead + Unpin>(
    r: &mut R,
    expect_ip: bool,
) -> io::Result<Option<String>> {
    // Read and validate the marker.
    let mut marker = [0u8; 1];
    r.read_exact(&mut marker).await?;
    if marker[0] != STREAM_READY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid STREAM_READY marker",
        ));
    }

    if !expect_ip {
        // Legacy path: no IP extension, return None (marker consumed).
        return Ok(None);
    }

    // Extended path: read IP length and IP bytes.
    let mut ip_len = [0u8; 1];
    r.read_exact(&mut ip_len).await?;
    let len = ip_len[0] as usize;

    if len == 0 {
        // IP unknown (server couldn't determine it).
        return Ok(Some(String::new()));
    }

    let mut ip_bytes = vec![0u8; len];
    r.read_exact(&mut ip_bytes).await?;
    let ip_string = String::from_utf8(ip_bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid UTF-8 in IP: {e}"),
        )
    })?;
    Ok(Some(ip_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{timeout, Duration};

    // Connection liveness group.
    //
    // These four tests are ONE statement with four faces: a connection stays
    // alive exactly as long as something can still use it, and not one moment
    // longer. The first is the red-check for the measured leak (it times out
    // without `Liveness`); the other three refuse the over-eager fixes that
    // would pass the first one while tearing down live traffic — which is why
    // they are here rather than in a follow-up. Real sockets throughout:
    // `tokio::io::duplex` cannot express "the peer never closes".

    /// A mux connection whose handles are ALL gone has nothing left to do, and
    /// must close rather than park on the socket forever.
    ///
    /// RED-CHECK: with the `Liveness` count removed from `drive()`, this test
    /// times out — the production symptom exactly (one leaked `ESTABLISHED`
    /// control connection per VPN reconnect, at BOTH ends, because both peers
    /// run this same driver and each waits for the other to close first).
    #[tokio::test]
    async fn a_connection_whose_handles_are_all_dropped_closes_itself() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The PEER never closes first — it only reports what it observes.
        let peer = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (_opener, mut acceptor) = server(sock);
            acceptor.accept().await.is_none() // None <=> the client closed
        });

        let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
        drop(opener);
        drop(acceptor);

        let observed_close = timeout(Duration::from_secs(5), peer)
            .await
            .expect("client never closed a connection it had finished with")
            .unwrap();
        assert!(observed_close);
    }

    /// The other half of the same invariant, and the reason the obvious fix is
    /// wrong: substreams routinely OUTLIVE the `Opener` (the relay hands a
    /// stream to a task and drops the opener), so "exit when the Opener is
    /// gone" would tear down live traffic.
    #[tokio::test]
    async fn a_connection_with_a_live_substream_keeps_driving() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let peer = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (_o, mut acceptor) = server(sock);
            let mut s = acceptor.accept().await.expect("inbound substream");
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"pong").await.unwrap();
            s.flush().await.unwrap();
        });

        let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
        let mut stream = opener.open().await.unwrap();
        drop(opener); // the ordinary relay shape
        drop(acceptor);

        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = [0u8; 4];
        timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
            .await
            .expect("the connection was torn down under a live substream")
            .unwrap();
        assert_eq!(&buf, b"pong");
        peer.await.unwrap();
    }

    /// ...and it closes as soon as that last substream goes too. This is the
    /// test that pins the count itself: the two above are also satisfied by
    /// "close when the Opener AND Acceptor are gone", which would fail this one
    /// the other way round (it would close early and the write would fail).
    #[tokio::test]
    async fn a_connection_closes_when_its_last_substream_is_dropped() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The peer signals that it has ACCEPTED and READ the substream BEFORE
        // the client drops anything. Without this handshake the test races its
        // own subject: the client's liveness count reaches zero the instant it
        // drops, the driver returns `Step::Done` and closes, and whether the
        // peer surfaced the inbound substream first is pure scheduling.
        // MEASURED: three failures on Apple targets (`aarch64-apple-darwin`
        // twice, `macos-14` once) on commits touching ZERO Rust, every one of
        // them `acceptor.accept()` answering `None` at the `expect` below,
        // while every Linux run passed. The close is the BEHAVIOUR UNDER TEST,
        // so the fix orders the observation -- it must never delay the close,
        // which would be the one change that makes the test stop testing.
        let (read_tx, read_rx) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (_o, mut acceptor) = server(sock);
            let mut s = acceptor.accept().await.expect("inbound substream");
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            let _ = read_tx.send(());
            // Still open here: the client holds the substream.
            drop(s);
            acceptor.accept().await.is_none()
        });

        let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
        let mut stream = opener.open().await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        read_rx
            .await
            .expect("the peer never accepted and read the substream");
        drop(opener);
        drop(acceptor);
        drop(stream);

        let observed_close = timeout(Duration::from_secs(5), peer)
            .await
            .expect("the connection outlived its last substream")
            .unwrap();
        assert!(observed_close);
    }

    /// An `Opener` on its own keeps the connection: the server's `CarrierPool`
    /// stores exactly that (`LinkOpener::Mux(Opener)`) and opens substreams
    /// through it for the whole life of a tunnel, long after the acceptor that
    /// came with it was dropped.
    #[tokio::test]
    async fn an_opener_alone_keeps_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let peer = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (_o, mut acceptor) = server(sock);
            let mut s = acceptor.accept().await.expect("inbound substream");
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(b"pong").await.unwrap();
            s.flush().await.unwrap();
        });

        let (opener, acceptor) = client(TcpStream::connect(addr).await.unwrap());
        drop(acceptor);
        // A round trip AFTER the acceptor is gone, on a substream opened after
        // it is gone: nothing here would work if dropping it closed the link.
        let mut stream = timeout(Duration::from_secs(5), opener.open())
            .await
            .expect("dropping the Acceptor closed the connection")
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = [0u8; 4];
        timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
            .await
            .expect("no answer on a substream opened after the Acceptor died")
            .unwrap();
        assert_eq!(&buf, b"pong");
        peer.await.unwrap();
    }

    // Transport activity group (O-1).
    //
    // The liveness deadlines of plan 005 read `ConnActivity`, so these pin the
    // two things they rely on: ANY byte from the peer refreshes it (a data
    // substream as much as a control frame — the property that keeps a busy
    // tunnel from tripping a deadline its heartbeats are queued behind), and
    // `terminate` really releases the connection and every substream on it.

    /// A connected `(client, server)` pair over real loopback sockets.
    async fn activity_pair() -> ((Opener, Acceptor), (Opener, Acceptor)) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await.unwrap().0 });
        let client_side = client(TcpStream::connect(addr).await.unwrap());
        let server_side = server(accept.await.unwrap());
        (client_side, server_side)
    }

    #[tokio::test]
    async fn inbound_idle_grows_while_the_peer_is_silent_and_resets_on_a_frame() {
        let ((c_open, _c_acc), (_s_open, mut s_acc)) = activity_pair().await;
        let activity = s_acc.activity();

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            activity.inbound_idle() >= Duration::from_millis(250),
            "nothing arrived, yet idle reads {:?}",
            activity.inbound_idle()
        );

        // yamux opens lazily: the SYN rides the first data frame.
        let mut stream = c_open.open().await.unwrap();
        stream.write_all(b"x").await.unwrap();
        stream.flush().await.unwrap();
        let _inbound = timeout(Duration::from_secs(5), s_acc.accept())
            .await
            .unwrap()
            .expect("inbound substream");
        assert!(
            activity.inbound_idle() < Duration::from_millis(200),
            "a frame arrived, yet idle reads {:?}",
            activity.inbound_idle()
        );
    }

    #[tokio::test]
    async fn inbound_idle_is_refreshed_by_data_substream_bytes() {
        let ((c_open, _c_acc), (_s_open, mut s_acc)) = activity_pair().await;
        let activity = s_acc.activity();

        let mut tx = c_open.open().await.unwrap();
        tx.write_all(b"x").await.unwrap();
        tx.flush().await.unwrap();
        let mut rx = timeout(Duration::from_secs(5), s_acc.accept())
            .await
            .unwrap()
            .expect("inbound substream");
        let drain = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while matches!(rx.read(&mut buf).await, Ok(n) if n > 0) {}
        });

        // Only DATA flows for ~800 ms; idle must stay far below that.
        let mut worst = Duration::ZERO;
        for _ in 0..8 {
            tx.write_all(&[7u8; 512]).await.unwrap();
            tx.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            worst = worst.max(activity.inbound_idle());
        }
        assert!(
            worst < Duration::from_millis(350),
            "data bytes did not refresh the activity stamp (worst idle {worst:?})"
        );

        // And once the data stops, idle grows again.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(activity.inbound_idle() >= Duration::from_millis(300));
        drop(tx);
        drain.abort();
    }

    /// RED-CHECK: without the `terminate` arm in the driver task, the peer's
    /// `accept()` never returns (the client still holds every handle, so the
    /// M-1 count never reaches zero) and this times out.
    #[tokio::test]
    async fn terminate_closes_live_substreams_promptly() {
        let ((c_open, c_acc), (_s_open, mut s_acc)) = activity_pair().await;
        let mut stream = c_open.open().await.unwrap();
        stream.write_all(b"x").await.unwrap();
        stream.flush().await.unwrap();
        let mut inbound = timeout(Duration::from_secs(5), s_acc.accept())
            .await
            .unwrap()
            .expect("inbound substream");
        let mut first = [0u8; 1];
        inbound.read_exact(&mut first).await.unwrap();

        let activity = c_open.activity();
        assert!(!activity.is_terminated());
        activity.terminate();
        activity.terminate(); // idempotent
        assert!(activity.is_terminated());
        assert!(
            c_acc.activity().is_terminated(),
            "one connection, one switch"
        );

        // The local substream ends promptly — never parks.
        let mut buf = [0u8; 8];
        let local = timeout(Duration::from_secs(3), stream.read(&mut buf))
            .await
            .expect("a live substream parked after terminate");
        assert!(!matches!(local, Ok(n) if n > 0));

        // The socket is released, so the PEER sees the connection end too,
        // although the client still holds its opener, acceptor and stream.
        let peer_read = timeout(Duration::from_secs(3), inbound.read(&mut buf))
            .await
            .expect("the peer's substream never ended");
        assert!(!matches!(peer_read, Ok(n) if n > 0));
        let peer_accept = timeout(Duration::from_secs(3), s_acc.accept())
            .await
            .expect("the peer never saw the connection close");
        assert!(peer_accept.is_none());

        // Nothing can be opened on a terminated connection.
        assert!(timeout(Duration::from_secs(3), c_open.open())
            .await
            .expect("open parked on a terminated connection")
            .is_err());
        drop(c_acc);
    }

    #[tokio::test]
    async fn reap_due_none_never_fires() {
        let ((c_open, _c_acc), _server) = activity_pair().await;
        let activity = c_open.activity();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!activity.reap_due(None));
        assert!(activity.reap_due(Some(Duration::ZERO)));
    }

    #[tokio::test]
    async fn reap_due_fires_at_deadline() {
        let ((c_open, _c_acc), _server) = activity_pair().await;
        let activity = c_open.activity();
        assert!(!activity.reap_due(Some(Duration::from_millis(150))));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(activity.reap_due(Some(Duration::from_millis(150))));
        assert!(!activity.reap_due(Some(Duration::from_secs(10))));
    }

    #[tokio::test]
    async fn readiness_legacy_plain() {
        // Legacy path: write [0x00], read it back with expect_ip=false.
        let (mut client, mut server) = tokio::io::duplex(64);

        write_stream_ready(&mut client, None).await.unwrap();

        let result = read_stream_ready(&mut server, false).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn readiness_header_roundtrip() {
        // Extended path: write IP, read it back.
        let (mut client, mut server) = tokio::io::duplex(64);

        write_stream_ready(&mut client, Some("203.0.113.7:54321"))
            .await
            .unwrap();

        let result = read_stream_ready(&mut server, true).await.unwrap();
        assert_eq!(result, Some("203.0.113.7:54321".to_string()));
    }

    #[tokio::test]
    async fn readiness_empty_ip() {
        // Empty IP (server couldn't determine it).
        let (mut client, mut server) = tokio::io::duplex(64);

        write_stream_ready(&mut client, Some("")).await.unwrap();

        let result = read_stream_ready(&mut server, true).await.unwrap();
        assert_eq!(result, Some(String::new()));
    }

    #[tokio::test]
    async fn readiness_long_ip_truncated() {
        // IP > 255 bytes is truncated to 255.
        let (mut client, mut server) = tokio::io::duplex(512);

        let long_ip = "x".repeat(300);
        write_stream_ready(&mut client, Some(&long_ip))
            .await
            .unwrap();

        let result = read_stream_ready(&mut server, true).await.unwrap();
        assert_eq!(result.as_ref().unwrap().len(), 255);
        assert_eq!(result.as_ref().unwrap(), &"x".repeat(255));
    }

    #[tokio::test]
    async fn readiness_interop_old_client() {
        // Old client (no webserver_log field) deserializes to false; server writes bare byte.
        // This is implicitly tested by readiness_legacy_plain, but make it explicit:
        // if opts.webserver_log is false, we write None, which produces [0x00].
        let (mut client, mut server) = tokio::io::duplex(64);

        // Simulate server behavior: opts.webserver_log is false, so we write None.
        write_stream_ready(&mut client, None).await.unwrap();

        // Old client reads exactly one byte and should get STREAM_READY.
        let result = read_stream_ready(&mut server, false).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn link_open_ready_writes_single_zero_byte() {
        // Real yamux pair: open a substream through LinkOpener and confirm the
        // peer sees exactly one byte, STREAM_READY, before any payload.
        let (a, b) = tokio::io::duplex(4096);
        let (opener, _client_acceptor) = client(a);
        let (_server_opener, mut server_acceptor) = server(b);

        let link = LinkOpener::Mux(opener);
        // A caller addr never leaks onto the mux wire (SSH-only field).
        let caller = "203.0.113.7:54321".parse().ok();
        let _stream = link.open_ready(None, caller).await.unwrap();

        let mut accepted = server_acceptor.accept().await.expect("substream accepted");
        let mut marker = [0u8; 1];
        accepted.read_exact(&mut marker).await.unwrap();
        assert_eq!(marker[0], STREAM_READY);

        // Nothing else was written yet (no IP header, since forward_ip was None).
        let mut probe = [0u8; 1];
        let n = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            accepted.read(&mut probe).await
        })
        .await;
        assert!(n.is_err(), "no further bytes expected without forward_ip");
    }

    #[cfg(feature = "ssh-gateway")]
    #[tokio::test]
    async fn link_open_ready_ssh_writes_no_marker() {
        // A mock ChannelOpen that hands back one half of an in-memory duplex
        // and records the forward_ip it was asked to thread through, so the
        // test can assert on both without a real russh Handle/session.
        struct MockOpen {
            seen_forward_ip: Arc<std::sync::Mutex<Option<String>>>,
            seen_caller: Arc<std::sync::Mutex<Option<std::net::SocketAddr>>>,
            stream: Arc<std::sync::Mutex<Option<tokio::io::DuplexStream>>>,
        }

        impl ChannelOpen for MockOpen {
            fn open(
                &self,
                forward_ip: Option<&str>,
                caller: Option<std::net::SocketAddr>,
            ) -> Pin<Box<dyn Future<Output = io::Result<LinkStream>> + Send + '_>> {
                *self.seen_forward_ip.lock().unwrap() = forward_ip.map(str::to_string);
                *self.seen_caller.lock().unwrap() = caller;
                let stream = self.stream.lock().unwrap().take().expect("opened once");
                Box::pin(async move { Ok(Box::new(stream) as LinkStream) })
            }
        }

        let (a, b) = tokio::io::duplex(4096);
        let seen_forward_ip = Arc::new(std::sync::Mutex::new(None));
        let seen_caller = Arc::new(std::sync::Mutex::new(None));
        let opener = MockOpen {
            seen_forward_ip: Arc::clone(&seen_forward_ip),
            seen_caller: Arc::clone(&seen_caller),
            stream: Arc::new(std::sync::Mutex::new(Some(a))),
        };

        let caller: std::net::SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let link = LinkOpener::Ssh(Arc::new(opener));
        let mut stream = link
            .open_ready(Some("203.0.113.7"), Some(caller))
            .await
            .unwrap();

        // The caller IP was threaded into the channel-open request itself...
        assert_eq!(
            seen_forward_ip.lock().unwrap().as_deref(),
            Some("203.0.113.7")
        );
        // ...alongside the full caller address (IP AND port, for the RFC 4254
        // originator fields)...
        assert_eq!(*seen_caller.lock().unwrap(), Some(caller));
        // ...and NOT written as a leading STREAM_READY-style marker byte (I-4):
        // whatever the SSH peer sent first arrives untouched.
        let mut b = b;
        b.write_all(b"hello").await.unwrap();
        b.flush().await.unwrap();
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
    }
}
