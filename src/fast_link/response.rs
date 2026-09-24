//! Pure response byte-builders for the fast link transfer relay, plus two
//! small async helpers for closing a connection without leaking a half-open
//! socket.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use super::{LINGER_MAX_BYTES, LINGER_TIMEOUT};
use crate::transfer_link::{content_disposition, FilenameError, MIME_OCTET_STREAM};

/// `100 Continue` interim response.
pub const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// The chunked-encoding terminator.
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

/// Minimal HTML page shown to a link-preview bot. Deliberately carries no
/// filename or other transfer-specific detail.
pub const PREVIEW_HTML: &[u8] = b"<!doctype html><title>bore fast link</title>\
<p>A file is waiting to be downloaded. Open this link with a browser, curl or wget to receive it.</p>";

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// Build a complete, self-contained HTTP/1.1 response with a known-length
/// body: status line, `Content-Type`, `Content-Length`, `Cache-Control:
/// no-store`, `X-Content-Type-Options: nosniff`, any `extra` headers, then
/// `Connection: close` and the body.
pub fn simple_response(
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 256);
    out.extend_from_slice(format!("HTTP/1.1 {status} {}\r\n", reason_phrase(status)).as_bytes());
    out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Cache-Control: no-store\r\n");
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    for (name, value) in extra {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

/// The fixed response head written to the uploader before the chunked status
/// body begins.
pub fn upload_head() -> &'static [u8] {
    b"HTTP/1.1 200 OK\r\n\
      Content-Type: text/plain; charset=utf-8\r\n\
      Cache-Control: no-store\r\n\
      X-Content-Type-Options: nosniff\r\n\
      Transfer-Encoding: chunked\r\n\
      Connection: close\r\n\r\n"
}

/// The response head written to a downloader: either a fixed `Content-Length`
/// or, when `length` is unknown (a chunked upload), `Transfer-Encoding:
/// chunked` followed by the uploader's own chunked bytes verbatim.
pub fn download_head(filename: &str, length: Option<u64>) -> Result<Vec<u8>, FilenameError> {
    let disposition = content_disposition(filename)?;
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
    out.extend_from_slice(format!("Content-Type: {MIME_OCTET_STREAM}\r\n").as_bytes());
    out.extend_from_slice(format!("Content-Disposition: {disposition}\r\n").as_bytes());
    match length {
        Some(n) => out.extend_from_slice(format!("Content-Length: {n}\r\n").as_bytes()),
        None => out.extend_from_slice(b"Transfer-Encoding: chunked\r\n"),
    }
    out.extend_from_slice(b"Cache-Control: no-store\r\n");
    out.extend_from_slice(b"Referrer-Policy: no-referrer\r\n");
    out.extend_from_slice(b"X-Content-Type-Options: nosniff\r\n");
    out.extend_from_slice(b"X-Robots-Tag: noindex, nofollow\r\n");
    out.extend_from_slice(b"Accept-Ranges: none\r\n");
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok(out)
}

/// Encode `payload` as one chunked-transfer-coding chunk: `"{size in
/// hex}\r\n{payload}\r\n"`. `payload` must not be empty (an empty chunk is the
/// terminator, [`LAST_CHUNK`], not an ordinary data chunk).
pub fn chunk(payload: &[u8]) -> Vec<u8> {
    debug_assert!(
        !payload.is_empty(),
        "chunk() must not be called with an empty payload"
    );
    let mut out = Vec::with_capacity(payload.len() + 16);
    out.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
    out
}

/// The plain-text usage instructions shown at `GET /` (and its `HEAD`).
pub fn usage_text(authority: &str) -> String {
    format!(
        "curl -u USER:PASS -T file.tar https://{authority}\n\
         tar -cpf - dir | curl -N -u USER:PASS -T - https://{authority}/dir.tar\n\
         open the printed link once: curl -fO, wget or a browser\n\
         nothing is stored on the server\n"
    )
}

/// Flush, then shut the write half down and drain-and-discard whatever the
/// peer still sends, all best-effort and bounded by [`LINGER_TIMEOUT`] /
/// [`LINGER_MAX_BYTES`]. Used after a complete response so a TLS peer sees a
/// clean `close_notify` instead of an unexpected EOF, without blocking on a
/// peer that never closes its own side.
pub async fn linger_close<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    let _ = stream.flush().await;
    let _ = timeout(LINGER_TIMEOUT, stream.shutdown()).await;
    let mut discarded = 0usize;
    let mut buf = [0u8; 4096];
    let drain = async {
        loop {
            if discarded >= LINGER_MAX_BYTES {
                break;
            }
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => discarded += n,
            }
        }
    };
    let _ = timeout(LINGER_TIMEOUT, drain).await;
}

/// Flush and shut the write half down, best-effort, bounded by
/// [`LINGER_TIMEOUT`]. Used to close a connection mid-response, without
/// writing any further terminator, so a truncated transfer is visibly
/// truncated to the peer rather than looking complete.
pub async fn abort_close<S: AsyncWrite + Unpin>(stream: &mut S) {
    let _ = stream.flush().await;
    let _ = timeout(LINGER_TIMEOUT, stream.shutdown()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_bytes_exact() {
        let resp = simple_response(404, "text/plain", b"nope", &[]);
        let expected = b"HTTP/1.1 404 Not Found\r\n\
Content-Type: text/plain\r\n\
Content-Length: 4\r\n\
Cache-Control: no-store\r\n\
X-Content-Type-Options: nosniff\r\n\
Connection: close\r\n\r\nnope";
        assert_eq!(resp, expected);

        let head = download_head("file.bin", Some(5)).unwrap();
        let expected_head = b"HTTP/1.1 200 OK\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"file.bin\"; filename*=UTF-8''file.bin\r\n\
Content-Length: 5\r\n\
Cache-Control: no-store\r\n\
Referrer-Policy: no-referrer\r\n\
X-Content-Type-Options: nosniff\r\n\
X-Robots-Tag: noindex, nofollow\r\n\
Accept-Ranges: none\r\n\
Connection: close\r\n\r\n";
        assert_eq!(head, expected_head);

        let head = download_head("file.bin", None).unwrap();
        let expected_head = b"HTTP/1.1 200 OK\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"file.bin\"; filename*=UTF-8''file.bin\r\n\
Transfer-Encoding: chunked\r\n\
Cache-Control: no-store\r\n\
Referrer-Policy: no-referrer\r\n\
X-Content-Type-Options: nosniff\r\n\
X-Robots-Tag: noindex, nofollow\r\n\
Accept-Ranges: none\r\n\
Connection: close\r\n\r\n";
        assert_eq!(head, expected_head);

        assert_eq!(chunk(b"ab"), b"2\r\nab\r\n");
    }
}
