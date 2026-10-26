//! HTTP Basic auth enforcement for tunnels.
//!
//! Basic auth is an HTTP concept, so this gate protects HTTP traffic: it reads
//! the request head, checks the `Authorization` header, and replies `401` when it
//! is missing or wrong. Whether bytes are HTTP is decided by the *shape* of the
//! request line (`token SP ...`, see [`sniff`]), never by a list of known
//! methods, so WebDAV, the HTTP/2 preface, lowercase or unknown methods are all
//! gated. The gate is fail-closed: a connection that sends nothing (or only an
//! undecidable fragment) within the timeout is dropped, not forwarded. Only bytes
//! that positively are not HTTP (TLS records, binary, an SSH banner, ...) are
//! forwarded unprotected (see [`gate`]). The same gate is used by the server for
//! public tunnels and by a secret-tunnel provider for its own connections.

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::shared::NETWORK_TIMEOUT;

/// Cap on the request-head bytes read while checking authorization.
const MAX_HEAD: usize = 8 * 1024;

/// The fixed `401` response sent to an unauthenticated HTTP client.
pub(crate) const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized\r\n\
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
/// - If the bytes are an authorized HTTP request, returns `Forward` with the head
///   already read (to be replayed downstream).
/// - If they are an unauthorized HTTP request, writes `401` and returns `Reject`.
/// - If they positively are not HTTP (see [`sniff`]), returns `Forward` with
///   whatever was read — non-HTTP traffic is not gated.
/// - If the peer sends nothing, or only a fragment that cannot be classified,
///   within [`NETWORK_TIMEOUT`] (or hits EOF / [`MAX_HEAD`] undecided), returns
///   `Reject` **without** forwarding: forwarding an idle connection would let the
///   peer wait out the timeout and then speak HTTP to the service ungated.
pub async fn gate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    auth: &BasicAuth,
) -> Result<Gate> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    // Bound the whole read. A peer that is slow or silent hits the timeout and is
    // judged on what it sent so far (nothing/undecided => rejected below).
    let read_head = async {
        let mut verdict = Sniff::NeedMore;
        loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break; // EOF before a full head
            }
            buf.extend_from_slice(&chunk[..n]);
            if verdict == Sniff::NeedMore {
                verdict = sniff(&buf);
            }
            let done = match verdict {
                // Positively not HTTP: forward without waiting for more.
                Sniff::NotHttp => true,
                Sniff::Http => head_complete(&buf) || buf.len() >= MAX_HEAD,
                // Never ending "method": stop reading, fail closed below.
                Sniff::NeedMore => buf.len() >= MAX_HEAD,
            };
            if done {
                break;
            }
        }
        Ok::<(), std::io::Error>(())
    };
    let _ = timeout(NETWORK_TIMEOUT, read_head).await;

    match sniff(&buf) {
        Sniff::NotHttp => Ok(Gate::Forward(buf)),
        Sniff::NeedMore => {
            // Nothing, or a fragment we cannot classify: never forward.
            let _ = stream.shutdown().await;
            Ok(Gate::Reject)
        }
        Sniff::Http if auth.authorized(&buf) => Ok(Gate::Forward(buf)),
        Sniff::Http => {
            stream.write_all(UNAUTHORIZED.as_bytes()).await?;
            stream.flush().await?;
            // Close cleanly so a TLS peer sees a `close_notify`, not an unexpected EOF.
            let _ = stream.shutdown().await;
            Ok(Gate::Reject)
        }
    }
}

/// Classification of the first bytes of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sniff {
    /// Looks like an HTTP request line: gate it.
    Http,
    /// Positively not HTTP: forward unprotected.
    NotHttp,
    /// Not enough bytes yet to tell.
    NeedMore,
}

/// RFC 7230 `tchar`: the characters a request method (a `token`) may contain.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Decide from the shape of the request line whether `buf` is HTTP.
///
/// Deliberately **not** a list of known methods: an allow-list of "HTTP" means
/// every method it forgets (`PROPFIND`, `MKCOL`, the `PRI` of the h2c preface,
/// lowercase tokens that lenient servers accept) is forwarded without
/// authentication. Instead a request line is `token SP ...`, optionally preceded
/// by empty lines that servers must ignore (RFC 7230 §3.5). Anything starting
/// like that is HTTP, and the gate decides on credentials. Lenient parsers also
/// split on HTAB/VT/FF, so those count as separators.
///
/// Only bytes that cannot be a request line are `NotHttp`: a first byte outside
/// `tchar` (TLS records start 0x16, binary frames), a token followed by
/// something that is not a separator (`PING\r\n`), or the `SSH-` banner (which
/// is `token SP` shaped but is never HTTP).
fn sniff(buf: &[u8]) -> Sniff {
    let start = buf
        .iter()
        .position(|b| !matches!(b, b'\r' | b'\n'))
        .unwrap_or(buf.len());
    let rest = &buf[start..];
    if rest.is_empty() {
        return Sniff::NeedMore;
    }
    const SSH: &[u8] = b"SSH-";
    if rest.starts_with(SSH) {
        return Sniff::NotHttp;
    }
    if rest.len() < SSH.len() && SSH.starts_with(rest) {
        return Sniff::NeedMore; // could still become an SSH banner
    }
    if !is_tchar(rest[0]) {
        return Sniff::NotHttp;
    }
    match rest.iter().find(|b| !is_tchar(**b)) {
        None => Sniff::NeedMore, // still inside the method token
        Some(b' ' | b'\t' | 0x0b | 0x0c) => Sniff::Http,
        Some(_) => Sniff::NotHttp,
    }
}

/// Whether `buf` holds the end of an HTTP head (CRLF CRLF, or the bare-LF form
/// lenient servers also accept).
fn head_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n")
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

    // ---- gate: behavioural tests (in-memory duplex, paused clock) ----

    use std::time::Duration;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream};

    const GOOD: &str = "Authorization: Basic dXNlcjpwYXNz\r\n";

    fn auth() -> BasicAuth {
        BasicAuth::parse("user:pass").unwrap()
    }

    /// Run `gate` against `input`; the client end is kept open (no EOF) unless
    /// `close` is set. Returns the outcome and everything the gate wrote back.
    async fn run_gate(input: &[u8], close: bool) -> (Gate, Vec<u8>) {
        let (mut client, mut server): (DuplexStream, DuplexStream) = duplex(64 * 1024);
        client.write_all(input).await.unwrap();
        if close {
            client.shutdown().await.unwrap();
        }
        let outcome = gate(&mut server, &auth()).await.unwrap();
        drop(server);
        let mut written = Vec::new();
        let _ = client.read_to_end(&mut written).await;
        (outcome, written)
    }

    fn is_reject(g: &Gate) -> bool {
        matches!(g, Gate::Reject)
    }

    #[tokio::test(start_paused = true)]
    async fn every_http_method_without_credentials_is_rejected_with_401() {
        for method in [
            "GET",
            "POST",
            "PUT",
            "HEAD",
            "DELETE",
            "OPTIONS",
            "PATCH",
            "TRACE",
            "CONNECT",
            "PROPFIND",
            "PROPPATCH",
            "MKCOL",
            "COPY",
            "MOVE",
            "LOCK",
            "UNLOCK",
            "REPORT",
            "SEARCH",
            "PURGE",
            "LINK",
            "MKCALENDAR",
            "BREW",
            "M-SEARCH",
            "PRI",
            "get",
            "Propfind",
            "X_CUSTOM.METHOD~1",
        ] {
            let req = format!("{method} / HTTP/1.1\r\nHost: x\r\n\r\n");
            let (g, out) = run_gate(req.as_bytes(), false).await;
            assert!(is_reject(&g), "{method} must be rejected");
            assert!(
                out.starts_with(b"HTTP/1.1 401"),
                "{method} must get a 401, got {:?}",
                String::from_utf8_lossy(&out)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_http_method_with_credentials_is_forwarded_intact() {
        for method in ["GET", "PROPFIND", "MKCOL", "COPY", "REPORT", "get", "PRI"] {
            let req = format!("{method} / HTTP/1.1\r\nHost: x\r\n{GOOD}\r\n");
            let (g, out) = run_gate(req.as_bytes(), false).await;
            match g {
                Gate::Forward(prefix) => assert_eq!(prefix, req.as_bytes(), "{method}"),
                Gate::Reject => panic!("{method} with valid credentials must be forwarded"),
            }
            assert!(out.is_empty(), "nothing may be written on success");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn h2c_preface_without_credentials_is_rejected() {
        let (g, out) = run_gate(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", false).await;
        assert!(is_reject(&g));
        assert!(out.starts_with(b"HTTP/1.1 401"));
    }

    #[tokio::test(start_paused = true)]
    async fn leading_crlf_before_request_line_is_still_gated() {
        // RFC 7230 §3.5: a server SHOULD ignore at least one empty line before
        // the request line, so a leading CRLF must not hide the method.
        let (g, out) = run_gate(b"\r\n\r\nPROPFIND / HTTP/1.1\r\nHost: x\r\n\r\n", false).await;
        assert!(is_reject(&g));
        assert!(out.starts_with(b"HTTP/1.1 401"));
        let req = format!("\r\nPROPFIND / HTTP/1.1\r\n{GOOD}\r\n");
        let (g, _) = run_gate(req.as_bytes(), false).await;
        assert!(matches!(g, Gate::Forward(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn tab_vt_ff_separators_are_still_http() {
        // Lenient parsers split the request line on any of these.
        for sep in ["\t", "\x0b", "\x0c"] {
            let req = format!("PROPFIND{sep}/ HTTP/1.1\r\nHost: x\r\n\r\n");
            let (g, _) = run_gate(req.as_bytes(), false).await;
            assert!(is_reject(&g), "separator {sep:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn bare_lf_terminated_head_is_gated_without_waiting() {
        let start = tokio::time::Instant::now();
        let (g, _) = run_gate(b"GET / HTTP/1.1\nHost: x\n\n", false).await;
        assert!(is_reject(&g));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a complete LF-only head must not wait for the timeout"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_peer_is_never_forwarded_ungated() {
        // Connect, stay silent past the gate's window, then (in the caller)
        // speak: the gate must have closed the connection, not forwarded it.
        let (g, out) = run_gate(b"", false).await;
        assert!(is_reject(&g), "idle connection must not be forwarded");
        assert!(out.is_empty(), "nothing to say to a peer that sent nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn eof_before_any_byte_is_rejected() {
        let (g, _) = run_gate(b"", true).await;
        assert!(is_reject(&g));
    }

    #[tokio::test(start_paused = true)]
    async fn slowloris_partial_method_is_rejected_not_forwarded() {
        let (g, _) = run_gate(b"PROPFIND", false).await;
        assert!(
            is_reject(&g),
            "undecidable bytes at timeout must fail closed"
        );
        let (g, _) = run_gate(b"\r\n", false).await;
        assert!(is_reject(&g));
    }

    #[tokio::test(start_paused = true)]
    async fn partial_head_with_valid_credentials_is_forwarded_on_timeout() {
        // Authorization already seen, head just not terminated: same decision as
        // before (the request is authenticated).
        let req = format!("GET / HTTP/1.1\r\n{GOOD}");
        let (g, _) = run_gate(req.as_bytes(), false).await;
        assert!(matches!(g, Gate::Forward(p) if p == req.as_bytes()));
    }

    #[tokio::test(start_paused = true)]
    async fn non_http_traffic_that_arrives_is_forwarded() {
        let cases: [&[u8]; 5] = [
            b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03", // TLS ClientHello
            b"SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13\r\n",
            b"\x00\x01\x02\x03\x04\x05\x06\x07\x08",
            b"RAWBYTES\r\n",
            b"*1\r\n$4\r\nPING\r\n",
        ];
        for data in cases {
            let (g, out) = run_gate(data, false).await;
            match g {
                Gate::Forward(prefix) => assert_eq!(prefix, data),
                Gate::Reject => panic!("non-HTTP {:?} must be forwarded", data),
            }
            assert!(out.is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ssh_banner_split_across_reads_is_forwarded() {
        let (mut client, mut server) = duplex(4096);
        let task = tokio::spawn(async move { gate(&mut server, &auth()).await.unwrap() });
        client.write_all(b"SS").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.write_all(b"H-2.0-OpenSSH_9.6\r\n").await.unwrap();
        assert!(matches!(task.await.unwrap(), Gate::Forward(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn request_line_split_across_reads_is_gated() {
        let (mut client, mut server) = duplex(4096);
        let task = tokio::spawn(async move { gate(&mut server, &auth()).await.unwrap() });
        client.write_all(b"PROP").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        client
            .write_all(b"FIND / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        assert!(is_reject(&task.await.unwrap()));
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_undecidable_prefix_is_rejected() {
        // A "token" that never ends cannot be classified; cap it and fail closed.
        let junk = vec![b'A'; MAX_HEAD + 1];
        let (g, _) = run_gate(&junk, false).await;
        assert!(is_reject(&g));
    }
}
