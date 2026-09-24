//! `BodyFramer`: a pure, incremental parser that tells the streaming pump
//! (0.3) exactly which prefix of each read is upload-body bytes to forward
//! verbatim, for both `Content-Length` and chunked-transfer-coded bodies.
//!
//! This module never touches a socket and never allocates a copy of the body:
//! [`BodyFramer::feed`] only inspects the bytes it is given and reports how
//! many of them (a prefix) belong to the body, so the caller can forward that
//! slice unmodified. Chunk framing bytes (size line, per-chunk `CRLF`, the
//! final `0\r\n\r\n`) are forwarded too — a chunked upload is passed through to
//! the downloader byte-for-byte, never re-encoded.

use super::Framing;

/// A `BodyFramer` failure. Once returned, the framer stays in this state:
/// every subsequent [`BodyFramer::feed`] call returns the same error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingError {
    /// A chunk-size line does not start with a hex digit, contains a byte
    /// that isn't a valid chunk-size/extension character, or its value
    /// overflows (more than 16 hex digits).
    ChunkSize,
    /// A chunk-size line (including any extension) exceeded [`MAX_CHUNK_LINE`]
    /// bytes.
    ChunkLineTooLong,
    /// A line that must end in `"\r\n"` was terminated by something else,
    /// most commonly a bare `"\n"`.
    MissingCrlf,
    /// The chunked body's trailer section is non-empty. Trailers are not
    /// supported; only the empty trailer (`"0\r\n\r\n"`) is accepted.
    TrailersNotSupported,
}

/// The result of feeding a slice of freshly read bytes to a [`BodyFramer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The number of bytes, counted from the start of the fed slice, that
    /// belong to the body and must be forwarded verbatim. Always a prefix of
    /// the input: `forward <= input.len()`, and `forward < input.len()` only
    /// when `done` is `true` (the remaining bytes are past the end of the
    /// body and must be ignored).
    pub forward: usize,
    /// Whether the body is now fully framed.
    pub done: bool,
}

/// Cap on a single chunk-size line (size digits plus any `;ext` extension),
/// in bytes, before the line is rejected as [`FramingError::ChunkLineTooLong`].
pub const MAX_CHUNK_LINE: usize = 4096;

/// Chunked-transfer-coding parser state. Byte-at-a-time inside a line;
/// `Data` skips its whole remaining count in one step.
enum ChunkState {
    /// Reading the hex chunk-size digits at the start of a chunk-size line.
    SizeDigits {
        value: u64,
        digits: u32,
        line_len: usize,
    },
    /// Reading a chunk-extension (`;name=value`) after the size digits.
    Ext { value: u64, line_len: usize },
    /// Expecting the `'\n'` that ends the chunk-size line.
    SizeLf { value: u64 },
    /// Skipping `remaining` bytes of chunk data.
    Data { remaining: u64 },
    /// Expecting the `'\r'` right after a chunk's data.
    DataCr,
    /// Expecting the `'\n'` right after that `'\r'`.
    DataLf,
    /// Reading the trailer section: expecting `'\r'` immediately (an empty
    /// trailer) or rejecting a non-empty one.
    TrailerStart,
    /// Expecting the `'\n'` that ends the empty trailer section.
    TrailerLf,
    /// The body is fully framed.
    Done,
}

/// Per-framing private state.
enum Inner {
    ContentLength { remaining: u64, declared: u64 },
    Chunked { state: ChunkState },
}

/// Incremental upload-body framer: tracks how much of each freshly read
/// buffer is body bytes to forward, for either a `Content-Length` or a
/// chunked body.
pub struct BodyFramer {
    inner: Inner,
    error: Option<FramingError>,
}

impl BodyFramer {
    /// Create a framer for the given declared body framing.
    ///
    /// A `Content-Length` of `0` is done immediately, before any `feed` call.
    pub fn new(framing: Framing) -> Self {
        match framing {
            Framing::ContentLength(n) => BodyFramer {
                inner: Inner::ContentLength {
                    remaining: n,
                    declared: n,
                },
                error: None,
            },
            Framing::Chunked => BodyFramer {
                inner: Inner::Chunked {
                    state: ChunkState::SizeDigits {
                        value: 0,
                        digits: 0,
                        line_len: 0,
                    },
                },
                error: None,
            },
        }
    }

    /// Feed a freshly read buffer and report how much of it is body bytes.
    ///
    /// Once the framer is done, every further call returns `forward: 0,
    /// done: true` without inspecting `input`. Once the framer has errored,
    /// every further call returns that same error.
    pub fn feed(&mut self, input: &[u8]) -> Result<Progress, FramingError> {
        if let Some(err) = self.error {
            return Err(err);
        }
        if self.is_done() {
            return Ok(Progress {
                forward: 0,
                done: true,
            });
        }
        match &mut self.inner {
            Inner::ContentLength { remaining, .. } => {
                let take = (*remaining).min(input.len() as u64);
                *remaining -= take;
                Ok(Progress {
                    forward: take as usize,
                    done: *remaining == 0,
                })
            }
            Inner::Chunked { state } => match feed_chunked(state, input) {
                Ok(progress) => Ok(progress),
                Err(err) => {
                    self.error = Some(err);
                    Err(err)
                }
            },
        }
    }

    /// Whether the body has been fully framed.
    pub fn is_done(&self) -> bool {
        if self.error.is_some() {
            return false;
        }
        match &self.inner {
            Inner::ContentLength { remaining, .. } => *remaining == 0,
            Inner::Chunked { state } => matches!(state, ChunkState::Done),
        }
    }

    /// Whether this framer decodes a chunked body (as opposed to a fixed
    /// `Content-Length`).
    pub fn is_chunked(&self) -> bool {
        matches!(self.inner, Inner::Chunked { .. })
    }

    /// The declared total body length, when known ahead of time. Always
    /// `None` for a chunked body (its total length is only known once it is
    /// fully framed).
    pub fn declared_length(&self) -> Option<u64> {
        match &self.inner {
            Inner::ContentLength { declared, .. } => Some(*declared),
            Inner::Chunked { .. } => None,
        }
    }
}

/// Advance the chunked state machine over as much of `input` as forms
/// complete progress, returning the number of bytes consumed and whether the
/// body is now fully framed (through the empty trailer's terminating CRLF,
/// inclusive).
fn feed_chunked(state: &mut ChunkState, input: &[u8]) -> Result<Progress, FramingError> {
    let mut i = 0usize;
    while i < input.len() {
        match state {
            ChunkState::Done => break,

            ChunkState::Data { remaining } => {
                let avail = (input.len() - i) as u64;
                let take = (*remaining).min(avail);
                i += take as usize;
                *remaining -= take;
                if *remaining == 0 {
                    *state = ChunkState::DataCr;
                }
            }

            ChunkState::SizeDigits {
                value,
                digits,
                line_len,
            } => {
                let b = input[i];
                *line_len += 1;
                if *line_len > MAX_CHUNK_LINE {
                    return Err(FramingError::ChunkLineTooLong);
                }
                if let Some(d) = hex_digit(b) {
                    if *digits >= 16 {
                        return Err(FramingError::ChunkSize);
                    }
                    let updated = value
                        .checked_mul(16)
                        .and_then(|v| v.checked_add(d as u64))
                        .ok_or(FramingError::ChunkSize)?;
                    *value = updated;
                    *digits += 1;
                    i += 1;
                } else if *digits == 0 {
                    return Err(FramingError::ChunkSize);
                } else if b == b';' || b == b' ' || b == b'\t' {
                    *state = ChunkState::Ext {
                        value: *value,
                        line_len: *line_len,
                    };
                    i += 1;
                } else if b == b'\r' {
                    *state = ChunkState::SizeLf { value: *value };
                    i += 1;
                } else {
                    return Err(FramingError::ChunkSize);
                }
            }

            ChunkState::Ext { value, line_len } => {
                let b = input[i];
                *line_len += 1;
                if *line_len > MAX_CHUNK_LINE {
                    return Err(FramingError::ChunkLineTooLong);
                }
                if b == b'\r' {
                    *state = ChunkState::SizeLf { value: *value };
                    i += 1;
                } else if b == b'\n' {
                    return Err(FramingError::MissingCrlf);
                } else if b < 0x20 && b != b'\t' {
                    return Err(FramingError::ChunkSize);
                } else {
                    i += 1;
                }
            }

            ChunkState::SizeLf { value } => {
                let b = input[i];
                if b == b'\n' {
                    *state = if *value == 0 {
                        ChunkState::TrailerStart
                    } else {
                        ChunkState::Data { remaining: *value }
                    };
                    i += 1;
                } else {
                    return Err(FramingError::MissingCrlf);
                }
            }

            ChunkState::DataCr => {
                let b = input[i];
                if b == b'\r' {
                    *state = ChunkState::DataLf;
                    i += 1;
                } else {
                    return Err(FramingError::MissingCrlf);
                }
            }

            ChunkState::DataLf => {
                let b = input[i];
                if b == b'\n' {
                    *state = ChunkState::SizeDigits {
                        value: 0,
                        digits: 0,
                        line_len: 0,
                    };
                    i += 1;
                } else {
                    return Err(FramingError::MissingCrlf);
                }
            }

            ChunkState::TrailerStart => {
                let b = input[i];
                if b == b'\r' {
                    *state = ChunkState::TrailerLf;
                    i += 1;
                } else {
                    return Err(FramingError::TrailersNotSupported);
                }
            }

            ChunkState::TrailerLf => {
                let b = input[i];
                if b == b'\n' {
                    *state = ChunkState::Done;
                    i += 1;
                } else {
                    return Err(FramingError::MissingCrlf);
                }
            }
        }
    }
    Ok(Progress {
        forward: i,
        done: matches!(state, ChunkState::Done),
    })
}

/// Decode a single ASCII hex digit, or `None` if `b` isn't one.
fn hex_digit(b: u8) -> Option<u8> {
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
    fn cl_forwards_exactly_the_declared_length_and_reports_excess() {
        let mut framer = BodyFramer::new(Framing::ContentLength(10));
        let input = vec![7u8; 25];
        let progress = framer.feed(&input).unwrap();
        assert_eq!(progress.forward, 10);
        assert!(progress.done);
        assert!(framer.is_done());
        assert!(!framer.is_chunked());
        assert_eq!(framer.declared_length(), Some(10));
    }

    #[test]
    fn cl_zero_is_done_at_construction() {
        let framer = BodyFramer::new(Framing::ContentLength(0));
        assert!(framer.is_done());
        assert_eq!(framer.declared_length(), Some(0));
    }

    #[test]
    fn chunked_every_split_point_matches_a_whole_feed() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\n";
        let len = body.len();
        for i in 0..=len {
            for j in i..=len {
                let mut framer = BodyFramer::new(Framing::Chunked);
                assert!(framer.is_chunked());
                let pieces = [&body[..i], &body[i..j], &body[j..]];
                let mut total_forward = 0usize;
                let mut forwarded = Vec::new();
                let mut done = false;
                for piece in pieces {
                    let progress = framer.feed(piece).unwrap();
                    forwarded.extend_from_slice(&piece[..progress.forward]);
                    total_forward += progress.forward;
                    done = progress.done;
                }
                assert_eq!(total_forward, len, "split ({i}, {j})");
                assert!(done, "split ({i}, {j})");
                assert!(framer.is_done(), "split ({i}, {j})");
                assert_eq!(&forwarded[..], &body[..], "split ({i}, {j})");
            }
        }
    }

    /// Tiny fixed-seed xorshift generator — no new dependency, deterministic
    /// across runs.
    struct Xorshift(u64);

    impl Xorshift {
        fn new(seed: u64) -> Self {
            Xorshift(seed | 1)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A value in `lo..=hi`.
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next_u64() % (hi - lo + 1)
        }
    }

    /// Reference chunked decoder used only to validate the random-body
    /// generator below; independent of [`BodyFramer`].
    fn reference_decode_chunked(encoded: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let line_end = encoded[i..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .expect("chunk-size line");
            let size_line = std::str::from_utf8(&encoded[i..i + line_end]).unwrap();
            let size_str = size_line.split(';').next().unwrap();
            let size = usize::from_str_radix(size_str, 16).unwrap();
            i += line_end + 2;
            if size == 0 {
                assert_eq!(&encoded[i..i + 2], b"\r\n", "empty trailer");
                i += 2;
                break;
            }
            out.extend_from_slice(&encoded[i..i + size]);
            i += size;
            assert_eq!(&encoded[i..i + 2], b"\r\n", "chunk-data CRLF");
            i += 2;
        }
        assert_eq!(i, encoded.len(), "no trailing bytes after the terminator");
        out
    }

    #[test]
    fn chunked_random_sizes_roundtrip() {
        let mut rng = Xorshift::new(0xC0FFEE_u64);
        for _ in 0..200 {
            let chunk_count = rng.range(1, 3);
            let mut payload = Vec::new();
            let mut encoded = Vec::new();
            for _ in 0..chunk_count {
                let size = rng.range(1, 70_000) as usize;
                let chunk = vec![(rng.next_u64() & 0xff) as u8; size];
                encoded.extend_from_slice(format!("{size:x}\r\n").as_bytes());
                encoded.extend_from_slice(&chunk);
                encoded.extend_from_slice(b"\r\n");
                payload.extend_from_slice(&chunk);
            }
            encoded.extend_from_slice(b"0\r\n\r\n");

            // Sanity-check the generator itself with an independent decoder.
            assert_eq!(reference_decode_chunked(&encoded), payload);

            // Feed the encoded stream to BodyFramer in randomly sized pieces.
            let mut framer = BodyFramer::new(Framing::Chunked);
            let mut offset = 0usize;
            let mut total_forward = 0usize;
            let mut done = false;
            while offset < encoded.len() {
                let remaining = encoded.len() - offset;
                let take = rng.range(1, remaining as u64) as usize;
                let progress = framer.feed(&encoded[offset..offset + take]).unwrap();
                total_forward += progress.forward;
                offset += take;
                done = progress.done;
            }
            if !done {
                let progress = framer.feed(&[]).unwrap();
                done = progress.done;
            }
            assert!(done);
            assert!(framer.is_done());
            assert_eq!(total_forward, encoded.len());
        }
    }

    fn assert_rejects(body: &[u8]) {
        let mut framer = BodyFramer::new(Framing::Chunked);
        let mut result = Ok(Progress {
            forward: 0,
            done: false,
        });
        let mut offset = 0usize;
        while offset < body.len() {
            result = framer.feed(&body[offset..offset + 1]);
            offset += 1;
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err(), "expected an error for {body:?}");
    }

    #[test]
    fn chunked_rejects() {
        assert_rejects(b"g\r\n");
        assert_rejects(b"\r\n");
        assert_rejects(b"fffffffffffffffff\r\n"); // 17 hex digits
        assert_rejects(b"ffffffffffffffff1\r\n"); // overflow
        assert_rejects(b"5;\n"); // bare LF in the chunk-extension
        assert_rejects(b"3\r\nabcX"); // chunk data not followed by CRLF

        let mut framer = BodyFramer::new(Framing::Chunked);
        let err = framer.feed(b"0\r\nX: y\r\n\r\n").unwrap_err();
        assert_eq!(err, FramingError::TrailersNotSupported);

        let mut long_ext = b"1;".to_vec();
        long_ext.extend(std::iter::repeat_n(b'x', 5000));
        long_ext.extend_from_slice(b"\r\n");
        let mut framer = BodyFramer::new(Framing::Chunked);
        let err = framer.feed(&long_ext).unwrap_err();
        assert_eq!(err, FramingError::ChunkLineTooLong);
    }

    #[test]
    fn chunked_excess_after_done_is_not_forwarded() {
        let mut framer = BodyFramer::new(Framing::Chunked);
        let mut body = b"3\r\nabc\r\n0\r\n\r\n".to_vec();
        body.extend_from_slice(b"extra garbage that must never be forwarded");
        let progress = framer.feed(&body).unwrap();
        assert_eq!(progress.forward, b"3\r\nabc\r\n0\r\n\r\n".len());
        assert!(progress.done);

        let again = framer.feed(b"more garbage").unwrap();
        assert_eq!(again.forward, 0);
        assert!(again.done);
    }

    #[test]
    fn error_is_sticky() {
        let mut framer = BodyFramer::new(Framing::Chunked);
        let err1 = framer.feed(b"g\r\n").unwrap_err();
        let err2 = framer.feed(b"anything").unwrap_err();
        let err3 = framer.feed(b"").unwrap_err();
        assert_eq!(err1, err2);
        assert_eq!(err2, err3);
    }
}
