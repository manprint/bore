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

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
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
}
