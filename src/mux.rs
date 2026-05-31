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
use std::io;
use std::task::Poll;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, Mode};

/// A multiplexed substream exposing Tokio's async I/O traits.
pub type Stream = Compat<yamux::Stream>;

/// Any byte stream `yamux` can run over (a plain TCP socket, a TLS stream, ...).
pub trait Transport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Transport for T {}

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
}

impl Opener {
    /// Open a new outbound substream to the peer.
    pub async fn open(&self) -> io::Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.requests.send(tx).await.map_err(|_| disconnected())?;
        rx.await.map_err(|_| disconnected())?
    }
}

/// Handle for accepting inbound substreams opened by the peer.
pub struct Acceptor {
    inbound: mpsc::Receiver<Stream>,
}

impl Acceptor {
    /// Wait for the next inbound substream, or `None` once the connection closes.
    pub async fn accept(&mut self) -> Option<Stream> {
        self.inbound.recv().await
    }
}

/// Start multiplexing as the connection initiator (dialer).
pub fn client<S: Transport>(socket: S) -> (Opener, Acceptor) {
    spawn_driver(Connection::new(socket.compat(), config(), Mode::Client))
}

/// Start multiplexing as the connection responder (listener).
pub fn server<S: Transport>(socket: S) -> (Opener, Acceptor) {
    spawn_driver(Connection::new(socket.compat(), config(), Mode::Server))
}

fn spawn_driver<S: Transport>(conn: Connection<Compat<S>>) -> (Opener, Acceptor) {
    let (open_tx, open_rx) = mpsc::channel(32);
    let (inbound_tx, inbound_rx) = mpsc::channel(32);
    tokio::spawn(drive(conn, open_rx, inbound_tx));
    (
        Opener { requests: open_tx },
        Acceptor {
            inbound: inbound_rx,
        },
    )
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
                            .map(FuturesAsyncReadCompatExt::compat)
                            .map_err(io::Error::other),
                    );
                }
            }
            Step::Inbound(stream) => {
                // If the `Acceptor` is gone, drop the stream but keep driving for
                // any streams still in flight.
                let _ = inbound_tx.send(stream.compat()).await;
            }
            Step::Done => break,
        }
    }

    let _ = poll_fn(|cx| conn.poll_close(cx)).await;
}
