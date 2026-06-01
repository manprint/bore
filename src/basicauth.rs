//! HTTP Basic auth enforcement for tunnels.
//!
//! Basic auth is an HTTP concept, so this gate only protects HTTP traffic: it
//! reads the request head, checks the `Authorization` header, and replies `401`
//! when it is missing or wrong. Non-HTTP connections cannot be authenticated and
//! are forwarded unprotected (see [`gate`]). The same gate is used by the server
//! for public tunnels and by a secret-tunnel provider for its own connections.

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::shared::NETWORK_TIMEOUT;

/// HTTP request methods used to recognise an HTTP request (same set as the edge
/// redirect detection).
const HTTP_METHODS: [&[u8]; 9] = [
    b"GET ", b"POST ", b"PUT ", b"HEAD ", b"DELE", b"OPTI", b"PATC", b"TRAC", b"CONN",
];

/// Cap on the request-head bytes read while checking authorization.
const MAX_HEAD: usize = 8 * 1024;

/// The fixed `401` response sent to an unauthenticated HTTP client.
const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized\r\n\
     WWW-Authenticate: Basic realm=\"bore\"\r\n\
     Content-Length: 0\r\n\
     Connection: close\r\n\r\n";

/// Parsed HTTP Basic auth credentials: the expected base64 of `user:pass`.
#[derive(Clone)]
pub struct BasicAuth {
    token_b64: String,
}

impl BasicAuth {
    /// Parse a `"user:pass"` credentials string. Returns `None` if it is empty or
    /// has no `':'` separator.
    pub fn parse(creds: &str) -> Option<Self> {
        if creds.is_empty() || !creds.contains(':') {
            return None;
        }
        Some(BasicAuth {
            token_b64: base64_encode(creds.as_bytes()),
        })
    }

    /// Whether an HTTP request head carries a matching `Authorization: Basic`
    /// header. The base64 token is compared in constant time.
    pub fn authorized(&self, head: &[u8]) -> bool {
        let text = String::from_utf8_lossy(head);
        for line in text.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if !name.trim().eq_ignore_ascii_case("authorization") {
                continue;
            }
            let value = value.trim();
            let mut parts = value.splitn(2, ' ');
            let scheme = parts.next().unwrap_or("");
            let token = parts.next().unwrap_or("").trim();
            if scheme.eq_ignore_ascii_case("basic") {
                return constant_time_eq(token.as_bytes(), self.token_b64.as_bytes());
            }
        }
        false
    }
}

/// Outcome of [`gate`].
pub enum Gate {
    /// Forward the connection. `prefix` is the bytes already consumed from the
    /// stream (the HTTP head, or the leading non-HTTP bytes); the caller must
    /// replay them downstream so nothing is lost.
    Forward(Vec<u8>),
    /// Unauthorized HTTP request — a `401` was already written; close the stream.
    Reject,
}

/// Read the start of `stream` and decide whether to forward it.
///
/// - If the bytes look like an authorized HTTP request, returns `Forward` with the
///   head already read (to be replayed downstream).
/// - If they look like an unauthorized HTTP request, writes `401` and returns
///   `Reject`.
/// - If they are not HTTP (or no data arrives within [`NETWORK_TIMEOUT`]), returns
///   `Forward` with whatever was read — non-HTTP traffic is not gated.
pub async fn gate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    auth: &BasicAuth,
) -> Result<Gate> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Bound the whole read: a non-HTTP peer that waits for the service to speak
    // first never sends a head, so we time out and forward it unprotected.
    let read_head = async {
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break; // EOF before a full head
            }
            buf.extend_from_slice(&chunk[..n]);
            // As soon as we can tell it is not HTTP, stop and forward.
            if buf.len() >= 8 && !looks_like_http(&buf) {
                break;
            }
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_HEAD {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    // On timeout, fall through with whatever we have (treated as non-HTTP below).
    let _ = timeout(NETWORK_TIMEOUT, read_head).await;

    if !looks_like_http(&buf) {
        return Ok(Gate::Forward(buf));
    }
    if auth.authorized(&buf) {
        Ok(Gate::Forward(buf))
    } else {
        stream.write_all(UNAUTHORIZED.as_bytes()).await?;
        stream.flush().await?;
        // Close cleanly so a TLS peer sees a `close_notify`, not an unexpected EOF.
        let _ = stream.shutdown().await;
        Ok(Gate::Reject)
    }
}

fn looks_like_http(head: &[u8]) -> bool {
    HTTP_METHODS.iter().any(|m| head.starts_with(m))
}

/// Length-checked, byte-wise constant-time equality (no early exit on the first
/// differing byte). The length comparison is not secret here (the base64 token /
/// admin token length is fixed by configuration). Shared with the admin page.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Standard base64 encoding (with `=` padding) of arbitrary bytes. Hand-rolled to
/// avoid a dependency and keep `#![forbid(unsafe_code)]`.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // "user:pass" — the canonical Basic auth example.
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(BasicAuth::parse("").is_none());
        assert!(BasicAuth::parse("nopassword").is_none());
        assert!(BasicAuth::parse("u:p").is_some());
    }

    #[test]
    fn authorized_accepts_correct_header() {
        let auth = BasicAuth::parse("user:pass").unwrap();
        let head = b"GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Basic dXNlcjpwYXNz\r\n\r\n";
        assert!(auth.authorized(head));
    }

    #[test]
    fn authorized_rejects_wrong_or_missing() {
        let auth = BasicAuth::parse("user:pass").unwrap();
        assert!(!auth.authorized(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(!auth.authorized(b"GET / HTTP/1.1\r\nAuthorization: Basic d3Jvbmc=\r\n\r\n"));
        // Case-insensitive header name and scheme are still accepted.
        let head = b"GET / HTTP/1.1\r\nauthorization: basic dXNlcjpwYXNz\r\n\r\n";
        assert!(auth.authorized(head));
    }
}
