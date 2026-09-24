//! Pure HTTP/1.1 request-head parsing for the fast link transfer relay.
//!
//! This module never touches a socket: it works on already-read byte slices,
//! so every rule here is unit-testable without I/O.

use super::{DEFAULT_UPLOAD_FILENAME, FAST_LINK_ID_ALPHABET, FAST_LINK_ID_LEN, MAX_HEADERS};
use crate::transfer_link::validate_filename;

/// A parsed HTTP/1.x request line and headers, borrowing from the original
/// buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct RequestHead<'a> {
    /// The request method, e.g. `"GET"`.
    pub method: &'a str,
    /// The request target as sent on the wire (origin-form), e.g. `"/abc"`.
    pub target: &'a str,
    headers: Vec<(&'a str, &'a str)>,
}

impl<'a> RequestHead<'a> {
    /// The value of the first header matching `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    }

    /// The number of headers matching `name`, case-insensitively.
    pub fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .count()
    }
}

/// Failure to parse a request head.
#[derive(Debug, PartialEq, Eq)]
pub enum HeadError {
    /// More than [`MAX_HEADERS`] header lines.
    TooLarge,
    /// The head does not follow the expected grammar.
    Malformed,
}

/// Return the index just past the first `"\r\n\r\n"` in `buf`, i.e. the length
/// of a complete request head including its terminating blank line.
///
/// Returns `None` when no full head is present yet; the caller decides how to
/// respond once `buf` has grown past its own head-size cap.
pub fn head_len(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Parse a complete request head (ending in `"\r\n\r\n"`) into its method,
/// target and headers.
pub fn parse_head(head: &[u8]) -> Result<RequestHead<'_>, HeadError> {
    let text = std::str::from_utf8(head).map_err(|_| HeadError::Malformed)?;
    // `head` is expected to end with "\r\n\r\n"; strip the trailing blank line
    // before splitting into individual "\r\n"-terminated lines.
    let body = text.strip_suffix("\r\n\r\n").ok_or(HeadError::Malformed)?;
    let mut lines = body.split("\r\n");

    let request_line = lines.next().ok_or(HeadError::Malformed)?;
    let mut parts = request_line.split(' ');
    let method = parts.next().ok_or(HeadError::Malformed)?;
    let target = parts.next().ok_or(HeadError::Malformed)?;
    let version = parts.next().ok_or(HeadError::Malformed)?;
    if parts.next().is_some() || method.is_empty() || target.is_empty() {
        return Err(HeadError::Malformed);
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HeadError::Malformed);
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // obs-fold: no longer allowed.
            return Err(HeadError::Malformed);
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::Malformed);
        };
        if name.is_empty() || name.ends_with(' ') || name.ends_with('\t') {
            return Err(HeadError::Malformed);
        }
        headers.push((name, value.trim()));
        if headers.len() > MAX_HEADERS {
            return Err(HeadError::Malformed);
        }
    }

    Ok(RequestHead {
        method,
        target,
        headers,
    })
}

/// The declared framing of an upload request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// A fixed-length body of the given size.
    ContentLength(u64),
    /// A chunked-transfer-coded body.
    Chunked,
}

/// Determine the upload body framing from `Content-Length`/`Transfer-Encoding`.
///
/// Returns `Err` with the HTTP status the caller should answer with.
pub fn upload_framing(h: &RequestHead) -> Result<Framing, u16> {
    let te_count = h.header_count("transfer-encoding");
    let cl_count = h.header_count("content-length");

    if te_count > 0 && cl_count > 0 {
        return Err(400);
    }

    if te_count > 0 {
        if te_count > 1 {
            return Err(400);
        }
        let value = h.header("transfer-encoding").unwrap_or("");
        let codings: Vec<String> = value
            .split(',')
            .map(|c| c.trim().to_ascii_lowercase())
            .collect();
        // Compare against the single expected coding.
        if codings.len() == 1 && codings[0] == "chunked" {
            return Ok(Framing::Chunked);
        }
        return Err(501);
    }

    if cl_count > 0 {
        if cl_count > 1 {
            return Err(400);
        }
        let value = h.header("content-length").unwrap_or("");
        if value.is_empty() || value.len() > 20 || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(400);
        }
        return match value.parse::<u64>() {
            Ok(n) => Ok(Framing::ContentLength(n)),
            Err(_) => Err(400),
        };
    }

    Err(411)
}

/// Whether the request declared `Expect: 100-continue`.
pub fn expects_continue(h: &RequestHead) -> Result<bool, u16> {
    match h.header("expect") {
        None => Ok(false),
        Some(value) => {
            if value.trim().eq_ignore_ascii_case("100-continue") {
                Ok(true)
            } else {
                Err(417)
            }
        }
    }
}

/// Parse a `PUT` request target into an upload filename.
///
/// Returns `Err` with the HTTP status the caller should answer with.
pub fn parse_upload_target(target: &str) -> Result<String, u16> {
    if !target.starts_with('/') {
        return Err(400);
    }
    if target.contains('?') || target.contains('#') {
        return Err(400);
    }
    let rest = &target[1..];
    if rest.is_empty() {
        return Ok(DEFAULT_UPLOAD_FILENAME.to_string());
    }
    if rest.contains('/') {
        return Err(400);
    }
    let decoded = percent_decode(rest).ok_or(400u16)?;
    let filename = String::from_utf8(decoded).map_err(|_| 400u16)?;
    validate_filename(&filename).map_err(|_| 400u16)?;
    Ok(filename)
}

/// Parse a `GET`/`HEAD` request target into a download id, if the first path
/// segment is a syntactically valid one.
pub fn parse_download_target(target: &str) -> Option<String> {
    let target = target.split('?').next().unwrap_or(target);
    if !target.starts_with('/') {
        return None;
    }
    let rest = &target[1..];
    let segment = rest.split('/').next().unwrap_or(rest);
    if segment.len() != FAST_LINK_ID_LEN {
        return None;
    }
    if !segment.bytes().all(|b| FAST_LINK_ID_ALPHABET.contains(&b)) {
        return None;
    }
    Some(segment.to_string())
}

/// Why a download request must not be allowed to consume the transfer.
#[derive(Debug, PartialEq, Eq)]
pub enum Preview {
    /// A known link-preview bot's User-Agent.
    Bot,
    /// A `Range` request other than `bytes=0-` (a probing/partial fetch).
    Range,
}

/// User-Agent substrings (already lowercase) that identify a link-preview bot.
/// Deliberately specific — never a generic marker like `"bot"`, which would
/// also match ordinary device names (e.g. a Cubot phone's User-Agent).
const BOT_UA_MARKERS: &[&str] = &[
    "slackbot",
    "slack-imgproxy",
    "discordbot",
    "telegrambot",
    "whatsapp",
    "facebookexternalhit",
    "facebot",
    "meta-externalagent",
    "twitterbot",
    "linkedinbot",
    "skypeuripreview",
    "mattermost-bot",
    "googlebot",
    "bingbot",
    "applebot",
    "embedly",
    "iframely",
    "redditbot",
    "bitlybot",
    "pinterestbot",
    "vkshare",
];

/// Whether a download request must be answered without consuming the
/// transfer: a link-preview bot, or a `Range` request other than the whole
/// resource.
pub fn preview_verdict(h: &RequestHead) -> Option<Preview> {
    if let Some(ua) = h.header("user-agent") {
        let lower = ua.to_ascii_lowercase();
        if BOT_UA_MARKERS.iter().any(|m| lower.contains(m)) {
            return Some(Preview::Bot);
        }
    }
    if let Some(range) = h.header("range") {
        let normalized: String = range
            .trim()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if normalized != "bytes=0-" {
            return Some(Preview::Range);
        }
    }
    None
}

/// Whether an incoming `Host` header value matches the fast link server's
/// configured host, ignoring an optional port suffix, case and a trailing dot.
pub fn host_matches(host_header: &str, configured: &str) -> bool {
    let host = match host_header.rfind(':') {
        Some(i) if host_header[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &host_header[..i],
        _ => host_header,
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    host == configured
}

/// Percent-decode a URL path segment. `+` is left untouched (this is a path,
/// not a query string). Returns `None` on a malformed escape.
fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(out)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_head_accepts_a_valid_head() {
        let head = b"GET /abc HTTP/1.1\r\nHost: x\r\nUser-Agent: curl\r\n\r\n";
        let h = parse_head(head).unwrap();
        assert_eq!(h.method, "GET");
        assert_eq!(h.target, "/abc");
        assert_eq!(h.header("host"), Some("x"));
        assert_eq!(h.header("HOST"), Some("x"));
    }

    #[test]
    fn parse_head_rejects_obs_fold() {
        let head = b"GET / HTTP/1.1\r\nX-Y: a\r\n b\r\n\r\n";
        assert_eq!(parse_head(head), Err(HeadError::Malformed));
    }

    #[test]
    fn parse_head_rejects_space_before_colon() {
        let head = b"GET / HTTP/1.1\r\nX-Y : a\r\n\r\n";
        assert_eq!(parse_head(head), Err(HeadError::Malformed));
    }

    #[test]
    fn parse_head_rejects_too_many_headers() {
        let mut s = String::from("GET / HTTP/1.1\r\n");
        for i in 0..65 {
            s.push_str(&format!("X-{i}: v\r\n"));
        }
        s.push_str("\r\n");
        assert_eq!(parse_head(s.as_bytes()), Err(HeadError::Malformed));
    }

    #[test]
    fn parse_head_rejects_non_utf8() {
        let head: &[u8] = b"GET /\xff HTTP/1.1\r\n\r\n";
        assert_eq!(parse_head(head), Err(HeadError::Malformed));
    }

    #[test]
    fn parse_head_rejects_unknown_version() {
        let head = b"GET / HTTP/2\r\n\r\n";
        assert_eq!(parse_head(head), Err(HeadError::Malformed));
    }

    fn head_with(extra: &str) -> Vec<u8> {
        format!("PUT / HTTP/1.1\r\n{extra}\r\n").into_bytes()
    }

    #[test]
    fn upload_framing_table() {
        let buf = head_with("Content-Length: 5\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Ok(Framing::ContentLength(5)));

        let buf = head_with("Content-Length: 5\r\nContent-Length: 6\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(400));

        let buf = head_with("Content-Length: abc\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(400));

        let buf = head_with("Content-Length: 123456789012345678901\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(400));

        let buf = head_with("Transfer-Encoding: chunked\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Ok(Framing::Chunked));

        let buf = head_with("Transfer-Encoding: gzip, chunked\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(501));

        let buf = head_with("Transfer-Encoding: chunked\r\nContent-Length: 5\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(400));

        let buf = head_with("");
        let h = parse_head(&buf).unwrap();
        assert_eq!(upload_framing(&h), Err(411));
    }

    #[test]
    fn expects_continue_table() {
        let buf = head_with("");
        let h = parse_head(&buf).unwrap();
        assert_eq!(expects_continue(&h), Ok(false));

        let buf = head_with("Expect: 100-continue\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(expects_continue(&h), Ok(true));

        let buf = head_with("Expect: 100-Continue\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(expects_continue(&h), Ok(true));

        let buf = head_with("Expect: gzip\r\n");
        let h = parse_head(&buf).unwrap();
        assert_eq!(expects_continue(&h), Err(417));
    }

    #[test]
    fn upload_target_table() {
        assert_eq!(
            parse_upload_target("/"),
            Ok(DEFAULT_UPLOAD_FILENAME.to_string())
        );
        assert_eq!(
            parse_upload_target("/miofile.tar"),
            Ok("miofile.tar".to_string())
        );
        assert_eq!(
            parse_upload_target("/my%20file.tar"),
            Ok("my file.tar".to_string())
        );
        assert_eq!(parse_upload_target("/a/b"), Err(400));
        assert_eq!(parse_upload_target("/x?y"), Err(400));
        assert_eq!(parse_upload_target("/%zz"), Err(400));
        assert_eq!(parse_upload_target("/%c3%28"), Err(400));
        assert_eq!(parse_upload_target("/.."), Err(400));
        assert_eq!(parse_upload_target("/%2F"), Err(400));
    }

    #[test]
    fn download_target_table() {
        let id = "abcdefghij012345";
        assert_eq!(
            parse_download_target(&format!("/{id}")),
            Some(id.to_string())
        );
        assert_eq!(
            parse_download_target(&format!("/{id}/nome.tar")),
            Some(id.to_string())
        );
        assert_eq!(
            parse_download_target(&format!("/{id}?q=1")),
            Some(id.to_string())
        );
        assert_eq!(parse_download_target("/abcdefghij01234"), None); // 15 chars
        assert_eq!(parse_download_target("/abcdefghij0123456"), None); // 17 chars
        assert_eq!(parse_download_target("/ABCDEFGHIJ012345"), None); // uppercase
        assert_eq!(parse_download_target("/abcdefghij01234!"), None); // out of alphabet
    }

    #[test]
    fn preview_table() {
        let buf = format!(
            "GET / HTTP/1.1\r\nUser-Agent: {}\r\n\r\n",
            "Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)"
        )
        .into_bytes();
        let h = parse_head(&buf).unwrap();
        assert_eq!(preview_verdict(&h), Some(Preview::Bot));

        let h = parse_head(
            b"GET / HTTP/1.1\r\nUser-Agent: Mozilla/5.0 (Linux; Cubot X19) AppleWebKit\r\n\r\n",
        )
        .unwrap();
        assert_eq!(preview_verdict(&h), None);

        let h = parse_head(b"GET / HTTP/1.1\r\nUser-Agent: curl/8.5.0\r\nRange: bytes=0-\r\n\r\n")
            .unwrap();
        assert_eq!(preview_verdict(&h), None);

        let h = parse_head(b"GET / HTTP/1.1\r\nRange: bytes=0-1023\r\n\r\n").unwrap();
        assert_eq!(preview_verdict(&h), Some(Preview::Range));

        let h = parse_head(b"GET / HTTP/1.1\r\nRange: bytes=10-\r\n\r\n").unwrap();
        assert_eq!(preview_verdict(&h), Some(Preview::Range));
    }

    #[test]
    fn host_matches_table() {
        assert!(host_matches("fast.bore.tld:8443", "fast.bore.tld"));
        assert!(host_matches("FAST.bore.tld", "fast.bore.tld"));
        assert!(host_matches("fast.bore.tld.", "fast.bore.tld"));
        assert!(!host_matches("xfast.bore.tld", "fast.bore.tld"));
        assert!(!host_matches("fast.bore.tld:abc", "fast.bore.tld"));
    }
}
