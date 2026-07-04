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
#[cfg(feature = "ssh-gateway")]
use std::pin::Pin;
#[cfg(feature = "ssh-gateway")]
use std::sync::Arc;
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
    fn open(
        &self,
        forward_ip: Option<&str>,
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
            LinkOpener::Ssh(opener) => opener.open(None).await,
        }
    }

    /// Open a link, announce it (write the STREAM_READY marker with the
    /// optional caller IP for a mux link; thread the caller IP into the
    /// channel-open request itself for an SSH link), and return the boxed
    /// stream ready to splice. A failure at any step is reported as one
    /// error so carrier-failover callers can treat it identically to an
    /// open failure.
    ///
    /// SSH links skip the marker (I-4): a stock `ssh` client on the other
    /// end doesn't know about it and would see it as leading garbage on the
    /// forwarded connection.
    pub async fn open_ready(&self, forward_ip: Option<&str>) -> io::Result<LinkStream> {
        match self {
            LinkOpener::Mux(opener) => {
                let mut stream = opener.open().await?;
                write_stream_ready(&mut stream, forward_ip).await?;
                stream.flush().await?;
                Ok(Box::new(stream))
            }
            #[cfg(feature = "ssh-gateway")]
            LinkOpener::Ssh(opener) => opener.open(forward_ip).await,
        }
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

    #[tokio::test]
    async fn link_open_ready_writes_single_zero_byte() {
        // Real yamux pair: open a substream through LinkOpener and confirm the
        // peer sees exactly one byte, STREAM_READY, before any payload.
        let (a, b) = tokio::io::duplex(4096);
        let (opener, _client_acceptor) = client(a);
        let (_server_opener, mut server_acceptor) = server(b);

        let link = LinkOpener::Mux(opener);
        let _stream = link.open_ready(None).await.unwrap();

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
            stream: Arc<std::sync::Mutex<Option<tokio::io::DuplexStream>>>,
        }

        impl ChannelOpen for MockOpen {
            fn open(
                &self,
                forward_ip: Option<&str>,
            ) -> Pin<Box<dyn Future<Output = io::Result<LinkStream>> + Send + '_>> {
                *self.seen_forward_ip.lock().unwrap() = forward_ip.map(str::to_string);
                let stream = self.stream.lock().unwrap().take().expect("opened once");
                Box::pin(async move { Ok(Box::new(stream) as LinkStream) })
            }
        }

        let (a, b) = tokio::io::duplex(4096);
        let seen_forward_ip = Arc::new(std::sync::Mutex::new(None));
        let opener = MockOpen {
            seen_forward_ip: Arc::clone(&seen_forward_ip),
            stream: Arc::new(std::sync::Mutex::new(Some(a))),
        };

        let link = LinkOpener::Ssh(Arc::new(opener));
        let mut stream = link.open_ready(Some("203.0.113.7")).await.unwrap();

        // The caller IP was threaded into the channel-open request itself...
        assert_eq!(
            seen_forward_ip.lock().unwrap().as_deref(),
            Some("203.0.113.7")
        );
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
