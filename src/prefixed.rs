//! A stream wrapper that replays an already-read prefix before delegating to the
//! underlying transport.
//!
//! Used wherever we must read some bytes to make a routing/authorization decision
//! and then hand the *whole* stream (those bytes included) to the next stage —
//! e.g. the basic-auth gate forwards the buffered HTTP request head, and the admin
//! router peeks the first byte of a TLS-decrypted control connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Wraps a stream `S`, yielding `prefix` bytes first on reads and then the bytes
/// from `S`. Writes go straight to `S`.
pub struct Prefixed<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Prefixed<S> {
    /// Build a stream that replays `prefix` before reading from `inner`.
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Prefixed {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.prefix.len() {
            // Serve buffered bytes first, up to whatever the caller has room for.
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn replays_prefix_then_inner() {
        // inner is an in-memory duplex; the read side gets prefix ++ inner bytes.
        let (mut a, b) = tokio::io::duplex(64);
        a.write_all(b"WORLD").await.unwrap();
        a.shutdown().await.unwrap();
        let mut p = Prefixed::new(b"HELLO".to_vec(), b);
        let mut out = Vec::new();
        p.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"HELLOWORLD");
    }

    #[tokio::test]
    async fn small_reads_drain_prefix_incrementally() {
        let (_a, b) = tokio::io::duplex(64);
        let mut p = Prefixed::new(b"ABCDE".to_vec(), b);
        let mut one = [0u8; 1];
        p.read_exact(&mut one).await.unwrap();
        assert_eq!(&one, b"A");
        let mut rest = [0u8; 4];
        p.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"BCDE");
    }
}
