//! Edge handling for the public tunnel port.
//!
//! Each incoming connection on a tunnel port is inspected by peeking its first
//! bytes (without consuming them):
//!
//! - a TLS `ClientHello` (when the tunnel enabled `--https`) is terminated with
//!   the server's certificate and the decrypted stream is forwarded;
//! - a plain HTTP request (when `--force-https` is set) is answered with a `308`
//!   redirect to the `https://` URL;
//! - anything else is forwarded as-is (plain TCP, so raw clients keep working).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;
use tracing::trace;

use crate::basicauth::{self, BasicAuth, Gate};
use crate::prefixed::Prefixed;
use crate::shared::{TunnelOptions, NETWORK_TIMEOUT};

/// First byte of a TLS handshake record (`ContentType::handshake`).
const TLS_HANDSHAKE: u8 = 0x16;

/// HTTP request methods used to detect a plain HTTP request for the redirect.
const HTTP_METHODS: [&[u8]; 9] = [
    b"GET ", b"POST ", b"PUT ", b"HEAD ", b"DELE", b"OPTI", b"PATC", b"TRAC", b"CONN",
];

/// Maximum number of request bytes read while building a redirect.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// A stream the edge can forward: anything readable, writable, and `Send`. Used
/// to type-erase a [`Prefixed`] wrapper over either a plain or a TLS stream.
pub trait EdgeIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> EdgeIo for T {}

/// A forwarded edge connection: plain TCP, a server-terminated TLS stream, or a
/// stream whose buffered head must be replayed (after the basic-auth gate read it).
pub enum TunnelStream {
    /// Plain TCP, forwarded as-is.
    Plain(TcpStream),
    /// TLS terminated at the server (boxed: much larger than a bare socket).
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    /// A stream with an already-read prefix to replay (basic-auth gated).
    Buffered(Box<dyn EdgeIo>),
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            TunnelStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            TunnelStream::Buffered(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TunnelStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            TunnelStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            TunnelStream::Buffered(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Plain(s) => Pin::new(s).poll_flush(cx),
            TunnelStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            TunnelStream::Buffered(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TunnelStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            TunnelStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            TunnelStream::Buffered(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TunnelStream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            TunnelStream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
            TunnelStream::Buffered(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            TunnelStream::Plain(s) => s.is_write_vectored(),
            TunnelStream::Tls(s) => s.is_write_vectored(),
            TunnelStream::Buffered(s) => s.is_write_vectored(),
        }
    }
}

/// Inspect an incoming tunnel connection.
///
/// Returns the stream to forward to the client, or `Ok(None)` when the connection
/// was fully handled here (a redirect, or a closed peer).
pub async fn accept(
    stream: TcpStream,
    opts: TunnelOptions,
    tls: Option<&TlsAcceptor>,
    port: u16,
    fallback_host: Option<&str>,
) -> Result<Option<TunnelStream>> {
    // Parse the optional basic-auth credentials once (validated at startup, so a
    // malformed value here is treated as "no auth").
    let auth = opts.basic_auth.as_deref().and_then(BasicAuth::parse);

    // Fast path: with no inspection requested, forward immediately. This both
    // preserves the original behaviour and lets the local service speak first
    // (peeking would otherwise block until the remote peer sends something).
    if !opts.https && !opts.force_https && auth.is_none() {
        return Ok(Some(TunnelStream::Plain(stream)));
    }

    // Peek the first bytes without consuming them. Bounded by a timeout so a peer
    // that waits for the service to speak first can't stall the connection: on
    // timeout (or EOF) we simply forward it as plain.
    let mut head = [0u8; 8];
    let n = match timeout(NETWORK_TIMEOUT, stream.peek(&mut head)).await {
        Ok(result) => result.context("failed to peek connection")?,
        Err(_) => 0,
    };
    let head = &head[..n];

    if n > 0 && opts.https && head[0] == TLS_HANDSHAKE {
        let acceptor = tls.context("TLS requested but no certificate is configured")?;
        let tls_stream = acceptor
            .accept(stream)
            .await
            .context("TLS handshake failed")?;
        // Basic auth is checked on the decrypted HTTP stream.
        if let Some(auth) = &auth {
            return gate_stream(tls_stream, auth).await;
        }
        return Ok(Some(TunnelStream::Tls(Box::new(tls_stream))));
    }

    if n > 0 && opts.force_https && looks_like_http(head) {
        redirect_to_https(stream, port, fallback_host).await?;
        return Ok(None);
    }

    // Plain forward path: gate basic auth on the raw HTTP stream if requested.
    if let Some(auth) = &auth {
        return gate_stream(stream, auth).await;
    }

    Ok(Some(TunnelStream::Plain(stream)))
}

/// Run the basic-auth gate on `stream`, returning a [`TunnelStream::Buffered`]
/// (head replayed) to forward, or `None` when the request was rejected with `401`.
async fn gate_stream<S: EdgeIo + 'static>(
    mut stream: S,
    auth: &BasicAuth,
) -> Result<Option<TunnelStream>> {
    match basicauth::gate(&mut stream, auth).await? {
        Gate::Forward(prefix) => Ok(Some(TunnelStream::Buffered(Box::new(Prefixed::new(
            prefix, stream,
        ))))),
        Gate::Reject => Ok(None),
    }
}

fn looks_like_http(head: &[u8]) -> bool {
    HTTP_METHODS.iter().any(|method| head.starts_with(method))
}

/// Consume the HTTP request head and reply with a `308` redirect to `https://`.
async fn redirect_to_https(
    mut stream: TcpStream,
    port: u16,
    fallback_host: Option<&str>,
) -> Result<()> {
    let request = timeout(NETWORK_TIMEOUT, read_request_head(&mut stream))
        .await
        .context("timed out reading HTTP request")??;

    let path = request_path(&request);
    let authority = host_authority(&request, port, fallback_host);
    let location = format!("https://{authority}{path}");

    let response = format!(
        "HTTP/1.1 308 Permanent Redirect\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    trace!(%location, "redirected HTTP request to HTTPS");
    Ok(())
}

/// Read up to the end of the request headers (`\r\n\r\n`), capped.
async fn read_request_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_HEAD {
            break;
        }
    }
    Ok(buf)
}

/// Extract the request-target (path) from the request line, defaulting to `/`.
fn request_path(request: &[u8]) -> String {
    let line = request
        .split(|&b| b == b'\r' || b == b'\n')
        .next()
        .unwrap_or(&[]);
    let mut parts = line.split(|&b| b == b' ');
    parts.next(); // method
    match parts.next() {
        Some(target) if !target.is_empty() => String::from_utf8_lossy(target).into_owned(),
        _ => "/".to_string(),
    }
}

/// Determine the authority (`host[:port]`) for the redirect, preferring the
/// request's `Host` header.
fn host_authority(request: &[u8], port: u16, fallback_host: Option<&str>) -> String {
    let text = String::from_utf8_lossy(request);
    let host = text
        .lines()
        .skip(1)
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
        })
        .map(|(_, value)| value.trim().to_string());

    match host {
        Some(host) if host.contains(':') => host,
        Some(host) if !host.is_empty() => format!("{host}:{port}"),
        _ => format!("{}:{port}", fallback_host.unwrap_or("localhost")),
    }
}
