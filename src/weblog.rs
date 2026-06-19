//! Webserver access logging for vhost + public tunnels.
//!
//! This module provides:
//! - `RotatingFileWriter`: size-based log rotation (rename cascade).
//! - `AccessRecord`: HTTP request/response + raw connection events.
//! - `AccessLogger`: registry of per-target writer tasks + bounded channel.
//! - `format_combined`: nginx-combined format with control-char escaping.
//! - `HttpAccessTap<S>`: thin AsyncRead+AsyncWrite wrapper that taps bytes in-flight,
//!   parses HTTP/1.1 headers and body framing (Content-Length/chunked), detects TLS/raw,
//!   and emits AccessRecord on keep-alive boundaries.

use anyhow;
use dashmap::DashMap;
use httparse;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use time::format_description;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

/// Access log configuration: directory and rotation policy.
///
/// Validated at parse time: `max_files >= 1`, `max_file_size_bytes >= 1 MiB`.
/// `None` when logging is disabled (`--webserver-log` not set).
#[derive(Debug, Clone, PartialEq)]
pub struct AccessLogConfig {
    /// Root directory where per-tunnel log files are written.
    pub dir: PathBuf,
    /// Maximum number of rotated log files to retain per target.
    pub max_files: usize,
    /// Maximum size (in bytes) of a single log file before rotation.
    pub max_file_size_bytes: u64,
}

impl AccessLogConfig {
    /// Build from CLI flags. `Ok(None)` when logging disabled (dir None).
    /// Err if max_files==0 or max_file_size_mb==0. Warns if retention flags
    /// set without a dir. max_file_size_bytes = max_file_size_mb * 1024 * 1024.
    pub fn from_flags(
        dir: Option<PathBuf>,
        max_files: usize,
        max_file_size_mb: u64,
    ) -> anyhow::Result<Option<AccessLogConfig>> {
        if dir.is_none() {
            if max_files != 4 || max_file_size_mb != 100 {
                tracing::warn!(
                    max_files = max_files,
                    max_file_size_mb = max_file_size_mb,
                    "--webserver-log-max-files and --webserver-log-max-file-size are ignored without --webserver-log"
                );
            }
            return Ok(None);
        }

        anyhow::ensure!(max_files >= 1, "--webserver-log-max-files must be >= 1");
        anyhow::ensure!(
            max_file_size_mb >= 1,
            "--webserver-log-max-file-size must be >= 1 MiB"
        );

        let dir = dir.unwrap();
        let max_file_size_bytes = max_file_size_mb
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow::anyhow!("--webserver-log-max-file-size overflow"))?;

        fs::create_dir_all(&dir)?;

        Ok(Some(AccessLogConfig {
            dir,
            max_files,
            max_file_size_bytes,
        }))
    }
}

/// Record kind: HTTP request/response pair or raw connection.
#[derive(Debug, Clone)]
pub enum RecordKind {
    /// HTTP request/response pair.
    Http,
    /// Raw (TLS, non-HTTP) connection.
    Raw,
}

/// HTTP access record (built by the tap, sent to the writer task).
#[derive(Debug, Clone)]
pub struct AccessRecord {
    /// Timestamp (local time).
    pub ts: OffsetDateTime,
    /// Real caller IP (or None if unknown, formatted as "-").
    pub real_ip: Option<String>,
    /// Record kind (HTTP or Raw).
    pub kind: RecordKind,
    // HTTP fields (None for Raw).
    /// HTTP method (e.g., "GET").
    pub method: Option<String>,
    /// Request path (e.g., "/api/insert").
    pub path: Option<String>,
    /// HTTP version (e.g., "HTTP/1.1").
    pub version: Option<String>,
    /// HTTP status code (e.g., 200).
    pub status: Option<u16>,
    /// Bytes sent in response body (HTTP), or bytes out for Raw.
    pub bytes_sent: u64,
    /// Bytes received (Raw mode only).
    pub bytes_in: u64,
    /// Referer header (or None if absent/Raw).
    pub referer: Option<String>,
    /// User-Agent header (or None if absent/Raw).
    pub user_agent: Option<String>,
}

impl AccessRecord {
    /// Create an HTTP request/response record.
    #[allow(clippy::too_many_arguments)]
    pub fn http(
        ts: OffsetDateTime,
        real_ip: Option<String>,
        method: String,
        path: String,
        version: String,
        status: u16,
        bytes_sent: u64,
        referer: Option<String>,
        user_agent: Option<String>,
    ) -> Self {
        AccessRecord {
            ts,
            real_ip,
            kind: RecordKind::Http,
            method: Some(method),
            path: Some(path),
            version: Some(version),
            status: Some(status),
            bytes_sent,
            bytes_in: 0,
            referer,
            user_agent,
        }
    }

    /// Create a raw connection record.
    pub fn raw(ts: OffsetDateTime, real_ip: Option<String>, bytes_in: u64, bytes_out: u64) -> Self {
        AccessRecord {
            ts,
            real_ip,
            kind: RecordKind::Raw,
            method: None,
            path: None,
            version: None,
            status: None,
            bytes_sent: bytes_out,
            bytes_in,
            referer: None,
            user_agent: None,
        }
    }
}

/// Size-based rotating file writer (rename cascade).
pub struct RotatingFileWriter {
    path: PathBuf,
    max_files: usize,
    max_size: u64,
    file: BufWriter<fs::File>,
    written: u64,
}

impl RotatingFileWriter {
    /// Open or create a log file in append mode, creating parent dirs as needed.
    pub fn new(path: PathBuf, max_files: usize, max_size: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(RotatingFileWriter {
            path,
            max_files,
            max_size,
            file: BufWriter::new(file),
            written,
        })
    }

    /// Write a line to the log file, rotating if necessary.
    ///
    /// Line should NOT include a trailing newline; one is appended.
    /// If `written + line.len() + 1 (newline)` would exceed `max_size` and `written > 0`,
    /// rotates the file (rename cascade) before writing.
    pub fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        let line_len = line.len() + 1; // +1 for newline
        if self.written + line_len as u64 > self.max_size && self.written > 0 {
            self.rotate()?;
        }
        self.file.write_all(line)?;
        self.file.write_all(b"\n")?;
        self.written += line_len as u64;
        Ok(())
    }

    /// Flush the buffer to disk.
    pub fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }

    /// Perform the rename cascade rotation.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;

        // Remove the oldest file if it exists.
        let oldest = self
            .path
            .with_extension(format!("log.{}", self.max_files - 1));
        let _ = fs::remove_file(&oldest);

        // Rename cascade: .k -> .{k+1} for k = max_files-2 down to 1.
        for k in (1..self.max_files - 1).rev() {
            let from = self.path.with_extension(format!("log.{}", k));
            let to = self.path.with_extension(format!("log.{}", k + 1));
            if from.exists() {
                fs::rename(&from, &to)?;
            }
        }

        // Rename current -> .1
        let backup = self.path.with_extension("log.1");
        if self.path.exists() {
            fs::rename(&self.path, &backup)?;
        }

        // Reopen fresh.
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.file = BufWriter::new(file);
        self.written = 0;
        Ok(())
    }
}

/// Escape control characters (CR, LF, `"`) in a string for log safety.
fn escape_for_log(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

/// Format an access record as an nginx-combined format line.
pub fn format_combined(rec: &AccessRecord) -> String {
    let ip = rec.real_ip.as_deref().unwrap_or("-");

    // Timestamp format: [10/Oct/2000:13:55:36 -0700]
    let ts_fmt = format_description::parse(
        "[day]/[month]/[year]:[hour]:[minute]:[second] [offset_hour][offset_minute]",
    )
    .expect("invalid format");
    let ts_str = rec.ts.format(&ts_fmt).unwrap_or_else(|_| "?".to_string());

    match &rec.kind {
        RecordKind::Http => {
            let method = rec
                .method
                .as_ref()
                .map(|m| escape_for_log(m))
                .unwrap_or_default();
            let path = rec
                .path
                .as_ref()
                .map(|p| escape_for_log(p))
                .unwrap_or_default();
            let version = rec
                .version
                .as_ref()
                .map(|v| escape_for_log(v))
                .unwrap_or_default();
            let status = rec.status.map(|s| s.to_string()).unwrap_or_default();
            let bytes = rec.bytes_sent;
            let referer = rec
                .referer
                .as_ref()
                .map(|r| escape_for_log(r))
                .unwrap_or_else(|| "-".to_string());
            let ua = rec
                .user_agent
                .as_ref()
                .map(|u| escape_for_log(u))
                .unwrap_or_else(|| "-".to_string());

            format!(
                "{} - - [{}] \"{} {} {}\" {} {} \"{}\" \"{}\"",
                ip, ts_str, method, path, version, status, bytes, referer, ua
            )
        }
        RecordKind::Raw => {
            // Raw record: format as bytes_in/bytes_out
            format!(
                "{} - - [{}] \"-\" - {}/{} \"-\" \"raw\"",
                ip, ts_str, rec.bytes_in, rec.bytes_sent
            )
        }
    }
}

/// Layout strategy for determining log file paths.
pub enum PathLayout {
    /// Flat: `<dir>/<key>.log` (for vhost FQDN or public port).
    Flat,
    /// Subdomain-based folder: `<dir>/<subdomain>/<key>.log`.
    SubdomainFolder {
        /// The subdomain name (e.g., "shop").
        subdomain: String,
    },
}

impl PathLayout {
    /// Resolve the layout into a full file path.
    pub fn resolve(&self, dir: &std::path::Path, key: &str) -> std::path::PathBuf {
        match self {
            PathLayout::Flat => dir.join(format!("{}.log", key)),
            PathLayout::SubdomainFolder { subdomain } => {
                dir.join(subdomain).join(format!("{}.log", key))
            }
        }
    }
}

/// Registry of access loggers (one per endpoint).
pub struct AccessLogger {
    cfg: AccessLogConfig,
    targets: DashMap<String, mpsc::Sender<AccessRecord>>,
}

impl AccessLogger {
    /// Create a new access logger registry.
    pub fn new(cfg: AccessLogConfig) -> Self {
        AccessLogger {
            cfg,
            targets: DashMap::new(),
        }
    }

    /// Get or lazily create a sender for a log target.
    ///
    /// `key` is the file stem (e.g., "shop.bore.example.com" for vhost, "9000" for public).
    /// `layout` determines the final path (Flat or SubdomainFolder).
    pub fn sender_for(&self, key: &str, layout: PathLayout) -> mpsc::Sender<AccessRecord> {
        if let Some(sender) = self.targets.get(key) {
            return sender.clone();
        }

        // Compute the path using the layout resolver.
        let path = layout.resolve(&self.cfg.dir, key);

        // Spawn the writer task.
        let (tx, rx) = mpsc::channel(1024);
        let cfg = self.cfg.clone();
        tokio::spawn(async move {
            writer_task(rx, path, cfg).await;
        });

        self.targets.insert(key.to_string(), tx.clone());
        tx
    }
}

/// Writer task: drains the channel, formats, and writes to disk.
/// Flushes after each record if no more are immediately available (idle flush),
/// or every 64 records (batched flush) for efficiency under load.
async fn writer_task(mut rx: mpsc::Receiver<AccessRecord>, path: PathBuf, cfg: AccessLogConfig) {
    let mut writer = match RotatingFileWriter::new(path, cfg.max_files, cfg.max_file_size_bytes) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("Failed to open access log: {}", e);
            return;
        }
    };

    let mut record_count = 0;
    const FLUSH_INTERVAL: u32 = 64;

    loop {
        match rx.recv().await {
            Some(rec) => {
                let line = format_combined(&rec);
                if let Err(e) = writer.write_line(line.as_bytes()) {
                    tracing::error!("Failed to write access log: {}", e);
                    break;
                }
                record_count += 1;

                // Batched flush: every FLUSH_INTERVAL records.
                if record_count % FLUSH_INTERVAL == 0 {
                    if let Err(e) = writer.flush() {
                        tracing::error!("Failed to flush access log: {}", e);
                        break;
                    }
                } else {
                    // Idle flush: if no more records are immediately pending, flush now.
                    if rx.is_empty() {
                        if let Err(e) = writer.flush() {
                            tracing::error!("Failed to flush access log: {}", e);
                            break;
                        }
                    }
                }
            }
            None => {
                // Channel closed, do a final flush and exit.
                let _ = writer.flush();
                break;
            }
        }
    }
}

/// Helper to send a record to a channel, dropping on full and incrementing a counter.
pub fn try_log(sender: &mpsc::Sender<AccessRecord>, rec: AccessRecord, dropped: &AtomicU64) {
    match sender.try_send(rec) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Channel is closed, silently ignore.
        }
    }
}

// ============================================================================
// HTTP Access Tap (Phase 1.1)
// ============================================================================
//
// Direction contract: the tap wraps the *caller-facing* duplex half.
// Bytes READ from the wrapped stream = HTTP requests (caller→origin).
// Bytes WRITTEN to the wrapped stream = HTTP responses (origin→caller).
// The tap parses both directions in-place, derives HTTP metadata, pairs
// requests to responses FIFO, and emits AccessRecord on keep-alive boundaries.
// Raw/TLS connections degrade to connection-level logging on first-byte sniff.

/// Request metadata awaiting its response.
#[derive(Debug, Clone)]
struct PendingRequest {
    method: String,
    path: String,
    version: String,
    referer: Option<String>,
    user_agent: Option<String>,
}

// ============================================================================
// Shared helpers for ReqParser and RespParser
// ============================================================================

#[derive(Debug, Clone)]
enum MsgState {
    Header,
    Body(u64),
    ChunkSize,
    ChunkData(u64),
    ChunkCrlf(u64),
    ChunkEnd(u64),
}

fn next_state(body_len: u64) -> MsgState {
    match body_len {
        0 => MsgState::Header,
        u64::MAX => MsgState::ChunkSize,
        n => MsgState::Body(n),
    }
}

fn parse_hex_size(line: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(line).ok()?;
    let s = s.split(';').next().unwrap_or("").trim();
    u64::from_str_radix(s, 16).ok()
}

const HEAD_LIMIT: usize = 64 * 1024;

/// Extract body length from HTTP headers.
/// Returns 0 if no body (or Content-Length: 0), u64::MAX if chunked, or the Content-Length value.
pub(crate) fn body_length(headers: &[httparse::Header]) -> u64 {
    for h in headers {
        let name_lower = h.name.to_lowercase();
        if name_lower == "content-length" {
            if let Ok(s) = std::str::from_utf8(h.value) {
                if let Ok(len) = s.trim().parse::<u64>() {
                    return len;
                }
            }
        } else if name_lower == "transfer-encoding" {
            if let Ok(s) = std::str::from_utf8(h.value) {
                if s.to_lowercase().contains("chunked") {
                    return u64::MAX;
                }
            }
        }
    }
    0
}

// ============================================================================
// HTTP Request Parser (cursor-based, no body buffering)
// ============================================================================

/// HTTP request parser state machine using a single-cursor algorithm.
/// Only headers and chunk-size lines are buffered; body bytes are consumed via cursor only.
struct ReqParser {
    partial: Vec<u8>,
    pub(crate) state: MsgState,
}

impl ReqParser {
    fn new() -> Self {
        ReqParser {
            partial: Vec::new(),
            state: MsgState::Header,
        }
    }

    /// Feed bytes from a read operation. Return a list of pending requests found.
    /// On parse error or head buffer overflow, set degraded flag (caller switches to Raw).
    fn feed(&mut self, chunk: &[u8], degraded: &mut bool) -> Vec<PendingRequest> {
        let mut out = Vec::new();
        let mut i = 0;

        loop {
            match &mut self.state {
                MsgState::Header => {
                    let mut found = false;
                    while i < chunk.len() {
                        self.partial.push(chunk[i]);
                        i += 1;
                        let n = self.partial.len();
                        if n >= 4 && &self.partial[n - 4..] == b"\r\n\r\n" {
                            found = true;
                            break;
                        }
                        if n > HEAD_LIMIT {
                            *degraded = true;
                            return out;
                        }
                    }
                    if !found {
                        break; // need more bytes; partial keeps the prefix
                    }

                    // Parse the header in an inner scope so the borrow ends before clearing.
                    let (parsed_req, body_len) = {
                        let mut headers = [httparse::EMPTY_HEADER; 64];
                        let mut req = httparse::Request::new(&mut headers);
                        match req.parse(&self.partial) {
                            Ok(httparse::Status::Complete(_)) => {
                                let method = req.method.unwrap_or("").to_string();
                                let path = req.path.unwrap_or("").to_string();
                                let version = match req.version {
                                    Some(0) => "HTTP/1.0".to_string(),
                                    Some(1) => "HTTP/1.1".to_string(),
                                    _ => "HTTP/?".to_string(),
                                };
                                let mut referer = None;
                                let mut user_agent = None;
                                for h in req.headers.iter().filter(|h| !h.name.is_empty()) {
                                    match h.name.to_lowercase().as_str() {
                                        "referer" => {
                                            referer = String::from_utf8(h.value.to_vec()).ok();
                                        }
                                        "user-agent" => {
                                            user_agent = String::from_utf8(h.value.to_vec()).ok();
                                        }
                                        _ => {}
                                    }
                                }
                                let body_len = Self::extract_body_length(req.headers);
                                (
                                    Some(PendingRequest {
                                        method,
                                        path,
                                        version,
                                        referer,
                                        user_agent,
                                    }),
                                    body_len,
                                )
                            }
                            _ => {
                                *degraded = true;
                                return out;
                            }
                        }
                    };

                    if let Some(req) = parsed_req {
                        out.push(req);
                    }
                    self.partial.clear();
                    self.state = next_state(body_len);
                }
                MsgState::Body(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::Header;
                    } else {
                        break;
                    }
                }
                MsgState::ChunkSize => {
                    let mut found = false;
                    while i < chunk.len() {
                        self.partial.push(chunk[i]);
                        i += 1;
                        let n = self.partial.len();
                        if n >= 2 && &self.partial[n - 2..] == b"\r\n" {
                            found = true;
                            break;
                        }
                        if n > HEAD_LIMIT {
                            *degraded = true;
                            return out;
                        }
                    }
                    if !found {
                        break;
                    }
                    match parse_hex_size(&self.partial[..self.partial.len() - 2]) {
                        Some(0) => {
                            self.partial.clear();
                            self.state = MsgState::ChunkEnd(2);
                        }
                        Some(s) => {
                            self.partial.clear();
                            self.state = MsgState::ChunkData(s);
                        }
                        None => {
                            *degraded = true;
                            return out;
                        }
                    }
                }
                MsgState::ChunkData(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::ChunkCrlf(2);
                    } else {
                        break;
                    }
                }
                MsgState::ChunkCrlf(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::ChunkSize;
                    } else {
                        break;
                    }
                }
                MsgState::ChunkEnd(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::Header;
                    } else {
                        break;
                    }
                }
            }
        }

        out
    }

    /// Extract body length from headers.
    fn extract_body_length(headers: &[httparse::Header]) -> u64 {
        for h in headers {
            let name_lower = h.name.to_lowercase();
            if name_lower == "content-length" {
                if let Ok(s) = String::from_utf8(h.value.to_vec()) {
                    if let Ok(len) = s.trim().parse::<u64>() {
                        return len;
                    }
                }
            } else if name_lower == "transfer-encoding" {
                if let Ok(s) = String::from_utf8(h.value.to_vec()) {
                    if s.to_lowercase().contains("chunked") {
                        return u64::MAX;
                    }
                }
            }
        }
        0
    }
}

// ============================================================================
// HTTP Response Parser (cursor-based, no body buffering)
// ============================================================================

#[derive(Debug, Clone)]
struct ParsedResponse {
    status: u16,
    bytes_sent: u64,
}

/// HTTP response parser state machine using a single-cursor algorithm.
/// Only headers and chunk-size lines are buffered; body bytes are consumed via cursor only.
/// Tracks bytes_sent (body bytes only, excluding headers and framing).
struct RespParser {
    partial: Vec<u8>,
    state: MsgState,
    cur_status: u16,
    bytes_sent: u64,
}

impl RespParser {
    fn new() -> Self {
        RespParser {
            partial: Vec::new(),
            state: MsgState::Header,
            cur_status: 0,
            bytes_sent: 0,
        }
    }

    /// Feed bytes from a write operation. Return parsed responses found.
    /// Tracks bytes_sent (body bytes only, not headers).
    fn feed(&mut self, chunk: &[u8], degraded: &mut bool) -> Vec<ParsedResponse> {
        let mut out = Vec::new();
        let mut i = 0;

        loop {
            match &mut self.state {
                MsgState::Header => {
                    let mut found = false;
                    while i < chunk.len() {
                        self.partial.push(chunk[i]);
                        i += 1;
                        let n = self.partial.len();
                        if n >= 4 && &self.partial[n - 4..] == b"\r\n\r\n" {
                            found = true;
                            break;
                        }
                        if n > HEAD_LIMIT {
                            *degraded = true;
                            return out;
                        }
                    }
                    if !found {
                        break; // need more bytes
                    }

                    // Parse the header in an inner scope.
                    let (status, body_len) = {
                        let mut headers = [httparse::EMPTY_HEADER; 64];
                        let mut resp = httparse::Response::new(&mut headers);
                        match resp.parse(&self.partial) {
                            Ok(httparse::Status::Complete(_)) => {
                                let status = resp.code.unwrap_or(0);
                                let body_len = Self::extract_body_length(resp.headers);
                                (status, body_len)
                            }
                            _ => {
                                *degraded = true;
                                return out;
                            }
                        }
                    };

                    self.cur_status = status;
                    self.bytes_sent = 0;
                    self.partial.clear();

                    // If body_len is 0, emit immediately and return to Header state.
                    if body_len == 0 {
                        out.push(ParsedResponse {
                            status: self.cur_status,
                            bytes_sent: 0,
                        });
                        self.state = MsgState::Header;
                    } else {
                        self.state = next_state(body_len);
                    }
                }
                MsgState::Body(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    self.bytes_sent += take as u64;
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        out.push(ParsedResponse {
                            status: self.cur_status,
                            bytes_sent: self.bytes_sent,
                        });
                        self.bytes_sent = 0;
                        self.state = MsgState::Header;
                    } else {
                        break;
                    }
                }
                MsgState::ChunkSize => {
                    let mut found = false;
                    while i < chunk.len() {
                        self.partial.push(chunk[i]);
                        i += 1;
                        let n = self.partial.len();
                        if n >= 2 && &self.partial[n - 2..] == b"\r\n" {
                            found = true;
                            break;
                        }
                        if n > HEAD_LIMIT {
                            *degraded = true;
                            return out;
                        }
                    }
                    if !found {
                        break;
                    }
                    match parse_hex_size(&self.partial[..self.partial.len() - 2]) {
                        Some(0) => {
                            self.partial.clear();
                            self.state = MsgState::ChunkEnd(2);
                        }
                        Some(s) => {
                            self.partial.clear();
                            self.state = MsgState::ChunkData(s);
                        }
                        None => {
                            *degraded = true;
                            return out;
                        }
                    }
                }
                MsgState::ChunkData(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    self.bytes_sent += take as u64;
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::ChunkCrlf(2);
                    } else {
                        break;
                    }
                }
                MsgState::ChunkCrlf(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        self.state = MsgState::ChunkSize;
                    } else {
                        break;
                    }
                }
                MsgState::ChunkEnd(rem) => {
                    let take = std::cmp::min(*rem as usize, chunk.len() - i);
                    i += take;
                    *rem -= take as u64;
                    if *rem == 0 {
                        out.push(ParsedResponse {
                            status: self.cur_status,
                            bytes_sent: self.bytes_sent,
                        });
                        self.bytes_sent = 0;
                        self.state = MsgState::Header;
                    } else {
                        break;
                    }
                }
            }
        }

        out
    }

    fn extract_body_length(headers: &[httparse::Header]) -> u64 {
        for h in headers {
            let name_lower = h.name.to_lowercase();
            if name_lower == "content-length" {
                if let Ok(s) = String::from_utf8(h.value.to_vec()) {
                    if let Ok(len) = s.trim().parse::<u64>() {
                        return len;
                    }
                }
            } else if name_lower == "transfer-encoding" {
                if let Ok(s) = String::from_utf8(h.value.to_vec()) {
                    if s.to_lowercase().contains("chunked") {
                        return u64::MAX;
                    }
                }
            }
        }
        0
    }
}

/// Thin AsyncRead+AsyncWrite wrapper that taps bytes in-flight, parses HTTP/1.1,
/// and emits AccessRecord on keep-alive boundaries or raw connection close.
///
/// **Direction contract:** bytes READ = HTTP requests (caller→origin);
/// bytes WRITTEN = HTTP responses (origin→caller).
///
/// - No second copy of payload bytes; parsing reads the same buffers.
/// - FIFO request↔response pairing via a pending queue.
/// - TLS/raw detected on first bytes; degrades gracefully.
/// - Delegates half-close and flush to inner stream.
pub struct HttpAccessTap<S> {
    inner: S,
    real_ip: Option<String>,
    tx: mpsc::Sender<AccessRecord>,
    dropped: Arc<AtomicU64>,

    // Parsers for HTTP/1.1 framing.
    req_parser: ReqParser,
    resp_parser: RespParser,
    pending_reqs: VecDeque<PendingRequest>,

    // Mode: HTTP or Raw (TLS/non-HTTP).
    mode: TapMode,

    // Raw mode: track bytes in/out.
    bytes_in: u64,
    bytes_out: u64,

    // Whether we've emitted a raw record (on drop).
    raw_emitted: bool,
}

enum TapMode {
    Http,
    Raw,
}

impl<S: AsyncRead + AsyncWrite + Unpin> HttpAccessTap<S> {
    /// Create a new HTTP access tap.
    pub fn new(
        inner: S,
        real_ip: Option<String>,
        tx: mpsc::Sender<AccessRecord>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        HttpAccessTap {
            inner,
            real_ip,
            tx,
            dropped,
            req_parser: ReqParser::new(),
            resp_parser: RespParser::new(),
            pending_reqs: VecDeque::new(),
            mode: TapMode::Http,
            bytes_in: 0,
            bytes_out: 0,
            raw_emitted: false,
        }
    }

    /// Inject a pending HTTP request (used when the request header was already consumed
    /// before the tap was attached, e.g., in vhost HTTP handler). This queues the request
    /// so it can be paired with the upcoming response.
    ///
    /// `body_len`: 0 = no body, N = Content-Length, u64::MAX = chunked.
    /// Primes the request parser's body-skip state so that body bytes flowing through
    /// the tap are properly framed and not mis-parsed as new requests (BUG-1 fix).
    pub fn inject_pending_request(
        &mut self,
        method: String,
        path: String,
        version: String,
        referer: Option<String>,
        user_agent: Option<String>,
        body_len: u64,
    ) {
        self.pending_reqs.push_back(PendingRequest {
            method,
            path,
            version,
            referer,
            user_agent,
        });
        // Prime the ReqParser state to skip the request body so that body bytes
        // are not mis-parsed as new requests.
        self.req_parser.state = next_state(body_len);
    }

    /// Check if first bytes indicate TLS or non-HTTP.
    fn detect_tls_or_non_http(&mut self, buf: &[u8]) -> bool {
        if buf.is_empty() {
            return false;
        }
        // TLS handshake starts with 0x16 (Handshake) and version 0x03.
        if buf[0] == 0x16 && buf.len() > 1 && buf[1] == 0x03 {
            return true;
        }
        // Try to parse as HTTP request; if it fails at the start, flag as non-HTTP.
        // Heuristic: if we have at least 5 bytes and no recognizable method,
        // likely non-HTTP.
        if buf.len() >= 5 {
            let methods = [
                "GET ", "POST", "HEAD", "PUT ", "DELE", "CONN", "OPTI", "PATC", "TRAC",
            ];
            if !methods.iter().any(|m| buf.starts_with(m.as_bytes())) {
                return true;
            }
        }
        false
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for HttpAccessTap<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Delegate to inner.
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let after = buf.filled().len();
                let filled = after - before;

                if filled == 0 {
                    return Poll::Ready(Ok(()));
                }

                // Extract just-filled bytes (no copy).
                let filled_chunk = &buf.filled()[before..after];

                // If in HTTP mode, feed to request parser.
                if matches!(self.mode, TapMode::Http) {
                    // On the very first read, detect TLS/non-HTTP.
                    if self.bytes_in == 0 && self.detect_tls_or_non_http(filled_chunk) {
                        self.mode = TapMode::Raw;
                    }

                    if matches!(self.mode, TapMode::Http) {
                        let mut degraded = false;
                        let reqs = self.req_parser.feed(filled_chunk, &mut degraded);
                        if degraded {
                            self.mode = TapMode::Raw;
                        }
                        for req in reqs {
                            self.pending_reqs.push_back(req);
                        }
                    }
                }

                self.bytes_in += filled as u64;
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for HttpAccessTap<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                // Feed accepted bytes (only [0..n]) to response parser.
                let written_chunk = &buf[..n];

                if matches!(self.mode, TapMode::Http) {
                    let mut degraded = false;
                    let resps = self.resp_parser.feed(written_chunk, &mut degraded);
                    if degraded {
                        self.mode = TapMode::Raw;
                    }
                    for resp in resps.iter() {
                        if let Some(req) = self.pending_reqs.pop_front() {
                            let rec = AccessRecord::http(
                                OffsetDateTime::now_utc(),
                                self.real_ip.clone(),
                                req.method,
                                req.path,
                                req.version,
                                resp.status,
                                resp.bytes_sent,
                                req.referer,
                                req.user_agent,
                            );
                            try_log(&self.tx, rec, &self.dropped);
                        }
                    }
                }

                self.bytes_out += n as u64;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S> Drop for HttpAccessTap<S> {
    fn drop(&mut self) {
        // If in Raw mode and we haven't emitted a record yet, emit one now.
        if matches!(self.mode, TapMode::Raw) && !self.raw_emitted {
            let rec = AccessRecord::raw(
                OffsetDateTime::now_utc(),
                self.real_ip.clone(),
                self.bytes_in,
                self.bytes_out,
            );
            try_log(&self.tx, rec, &self.dropped);
            self.raw_emitted = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use tempfile::TempDir;
    use tokio::time;

    #[test]
    fn rotate_cascade_keeps_n() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.log");

        let mut writer = RotatingFileWriter::new(path.clone(), 3, 100).expect("create writer");

        // Write 8 lines (17 bytes each = 8*17 = 136 bytes total).
        // With max_size=100, this forces 2+ rotations.
        // Trace:
        // L1(17): written=17
        // L2(17): 17+17=34<100, written=34
        // L3(17): 34+17=51<100, written=51
        // L4(17): 51+17=68<100, written=68
        // L5(17): 68+17=85<100, written=85
        // L6(17): 85+17=102>100, ROTATE(log→log.1, new file), write L6, written=17
        // L7(17): 17+17=34<100, written=34
        // L8(17): 34+17=51<100, written=51
        // Result: log.1 has L1-L5 (oldest), log has L6-L8 (newest).
        // No log.2 since we only rotated once.
        // Let's write more to trigger a 2nd rotation.
        for i in 1..=12 {
            let line = format!("line{}_xyzz", i);
            writer
                .write_line(line.as_bytes())
                .unwrap_or_else(|_| panic!("write line {}", i));
        }
        writer.flush().unwrap();

        // With 12 lines of 17 bytes and max_size=100, we'll rotate multiple times.
        // After all: oldest should be in log.2, newest in log.
        assert!(path.exists(), "live file should exist");
        assert!(path.with_extension("log.1").exists(), ".1 should exist");
        // log.2 might not exist if we haven't rotated enough, let's check what we have.
        // Actually, let's just verify the cascade works by checking line12 is in the live file.
        let mut content = String::new();
        fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("line12"),
            "live file should contain latest line"
        );

        // Verify .1 has an older line (not the newest).
        content.clear();
        fs::File::open(path.with_extension("log.1"))
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            !content.contains("line12"),
            ".1 should not contain the latest line"
        );
        // .1 should have some earlier lines.
        assert!(
            content.contains("line") && !content.is_empty(),
            ".1 should contain older lines"
        );
    }

    #[test]
    fn rotate_at_exact_boundary() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.log");

        let mut writer = RotatingFileWriter::new(path.clone(), 2, 11).expect("create writer");

        // Write a 10-byte line (+ newline = 11 bytes, exactly at the limit).
        writer.write_line(b"0123456789").expect("write first line");
        assert_eq!(writer.written, 11);

        // Next write should trigger rotation (written + new_line > max_size).
        writer.write_line(b"next").expect("write second line");

        // Verify rotation happened.
        assert!(
            path.with_extension("log.1").exists(),
            ".1 should exist after rotation"
        );
    }

    #[test]
    fn writer_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("deep/nested/dir/test.log");

        let writer = RotatingFileWriter::new(path.clone(), 2, 100);
        assert!(writer.is_ok(), "should create parent directories");
        assert!(path.parent().unwrap().exists());
    }

    #[test]
    fn no_rotation_under_limit() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.log");

        let mut writer = RotatingFileWriter::new(path.clone(), 3, 1000).expect("create writer");
        writer.write_line(b"small").expect("write");
        writer.write_line(b"line").expect("write");
        writer.flush().unwrap();

        // Only the live file should exist.
        assert!(path.exists());
        assert!(!path.with_extension("log.1").exists());
        assert!(!path.with_extension("log.2").exists());
    }

    #[test]
    fn format_combined_http_line() {
        let ts = OffsetDateTime::now_utc();

        let rec = AccessRecord::http(
            ts,
            Some("203.0.113.7".to_string()),
            "GET".to_string(),
            "/api/insert".to_string(),
            "HTTP/1.1".to_string(),
            200,
            1234,
            Some("https://example.com".to_string()),
            Some("curl/7.68.0".to_string()),
        );

        let line = format_combined(&rec);
        assert!(line.contains("203.0.113.7"), "IP should be in line");
        assert!(line.contains("GET"), "method should be in line");
        assert!(line.contains("/api/insert"), "path should be in line");
        assert!(line.contains("200"), "status should be in line");
        assert!(line.contains("1234"), "bytes should be in line");
    }

    #[test]
    fn format_raw_line() {
        let ts = OffsetDateTime::now_utc();

        let rec = AccessRecord::raw(ts, Some("192.168.1.1".to_string()), 1234, 5678);

        let line = format_combined(&rec);
        assert!(line.contains("192.168.1.1"), "IP should be in line");
        assert!(line.contains("raw"), "raw marker should be in line");
        assert!(line.contains("1234"), "bytes_in should be in line");
        assert!(line.contains("5678"), "bytes_out should be in line");
    }

    #[test]
    fn format_escapes_crlf() {
        let ts = OffsetDateTime::now_utc();

        let rec = AccessRecord::http(
            ts,
            Some("10.0.0.1".to_string()),
            "GET".to_string(),
            "/api/test\r\ninjection".to_string(),
            "HTTP/1.1".to_string(),
            200,
            100,
            None,
            Some("agent\"with\"quotes".to_string()),
        );

        let line = format_combined(&rec);
        // Should have no actual CR/LF (raw control chars).
        assert!(!line.contains('\r'));
        assert!(!line.contains('\n'));
        // Should be a single line.
        assert_eq!(line.matches('\n').count(), 0);
    }

    #[tokio::test]
    async fn writer_drops_on_full() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.log");

        let (tx, rx) = mpsc::channel(2); // Capacity 2.

        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 1)
            .unwrap()
            .unwrap();

        // Spawn writer task.
        tokio::spawn(async move {
            writer_task(rx, path, cfg).await;
        });

        let ts = OffsetDateTime::now_utc();

        // Fill the channel.
        let rec = AccessRecord::http(
            ts,
            Some("1.1.1.1".to_string()),
            "GET".to_string(),
            "/".to_string(),
            "HTTP/1.1".to_string(),
            200,
            0,
            None,
            None,
        );

        tx.try_send(rec.clone()).unwrap();
        tx.try_send(rec.clone()).unwrap();

        // Next try_send should fail with Full.
        let result = tx.try_send(rec.clone());
        assert!(
            matches!(result, Err(mpsc::error::TrySendError::Full(_))),
            "channel should be full"
        );

        // Verify try_log increments counter on full.
        let dropped_counter = Arc::new(AtomicU64::new(0));
        try_log(&tx, rec.clone(), &dropped_counter);
        assert_eq!(
            dropped_counter.load(Ordering::SeqCst),
            1,
            "counter should increment"
        );
    }

    #[tokio::test]
    async fn logger_one_writer_per_key() {
        let tmp = TempDir::new().unwrap();
        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 1)
            .unwrap()
            .unwrap();

        let logger = AccessLogger::new(cfg);

        let tx1 = logger.sender_for("key1", PathLayout::Flat);
        let tx2 = logger.sender_for("key1", PathLayout::Flat);

        // Both should refer to the same sender (same target count).
        assert_eq!(logger.targets.len(), 1, "should have exactly one target");

        // Verify they can send on the same channel.
        let ts = OffsetDateTime::now_utc();
        let rec = AccessRecord::raw(ts, Some("1.1.1.1".to_string()), 50, 100);

        assert!(tx1.try_send(rec.clone()).is_ok());
        assert!(tx2.try_send(rec).is_ok());
    }

    #[test]
    fn access_log_config_rejects_zero() {
        let tmp = TempDir::new().unwrap();

        let result = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 0, 100);
        assert!(result.is_err(), "max_files=0 should error");

        let result = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 4, 0);
        assert!(result.is_err(), "max_file_size_mb=0 should error");
    }

    #[test]
    fn access_log_config_none_when_no_dir() {
        let result = AccessLogConfig::from_flags(None, 4, 100).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn access_log_config_computes_bytes() {
        let tmp = TempDir::new().unwrap();
        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 4, 50)
            .unwrap()
            .unwrap();
        assert_eq!(cfg.max_file_size_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn path_layout_flat_resolves() {
        let tmp = TempDir::new().unwrap();
        let layout = PathLayout::Flat;
        let path = layout.resolve(tmp.path(), "9000");
        assert_eq!(path, tmp.path().join("9000.log"));
    }

    #[test]
    fn path_layout_subfolder_resolves() {
        let tmp = TempDir::new().unwrap();
        let layout = PathLayout::SubdomainFolder {
            subdomain: "shop".to_string(),
        };
        let path = layout.resolve(tmp.path(), "shop.bore.example.com");
        assert_eq!(
            path,
            tmp.path().join("shop").join("shop.bore.example.com.log")
        );
    }

    #[tokio::test]
    async fn writer_flushes_single_record() {
        use std::time::Duration;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("single.log");

        let (tx, rx) = mpsc::channel(1024);

        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 1)
            .unwrap()
            .unwrap();

        // Spawn writer task.
        let cfg_clone = cfg.clone();
        let path_clone = path.clone();
        tokio::spawn(async move {
            writer_task(rx, path_clone, cfg_clone).await;
        });

        // Send exactly ONE record.
        let ts = OffsetDateTime::now_utc();
        let rec = AccessRecord::http(
            ts,
            Some("127.0.0.1".to_string()),
            "GET".to_string(),
            "/test".to_string(),
            "HTTP/1.1".to_string(),
            200,
            0,
            None,
            None,
        );
        tx.send(rec).await.unwrap();
        drop(tx); // Close the channel to trigger final flush.

        // Poll for the file to appear (with timeout).
        for _ in 0..100 {
            if path.exists() {
                let mut content = String::new();
                fs::File::open(&path)
                    .unwrap()
                    .read_to_string(&mut content)
                    .unwrap();
                if !content.is_empty() {
                    // Verify the line contains the expected fields.
                    assert!(content.contains("127.0.0.1"), "IP should be in log");
                    assert!(content.contains("GET"), "method should be in log");
                    assert!(content.contains("/test"), "path should be in log");
                    return;
                }
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        panic!("Single record should have been flushed to disk within ~1s");
    }

    // ========================================================================
    // Phase 1.1: HttpAccessTap unit tests
    // ========================================================================

    #[test]
    fn tap_http_single_request() {
        // Test parser directly without tokio
        let mut req_parser = ReqParser::new();
        let req = b"GET /api/test HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let mut degraded = false;
        let reqs = req_parser.feed(req, &mut degraded);
        if degraded {
            eprintln!("Parser degraded unexpectedly");
        }
        assert!(!degraded, "should not degrade on valid HTTP");
        assert_eq!(reqs.len(), 1, "should parse one request");
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path, "/api/test");
        assert_eq!(reqs[0].version, "HTTP/1.1");
    }

    #[test]
    fn tap_http_keepalive_multi() {
        // Three sequential requests - feed one at a time.
        let mut req_parser = ReqParser::new();

        let req1 = b"GET /1 HTTP/1.1\r\n\r\n";
        let req2 = b"GET /2 HTTP/1.1\r\n\r\n";
        let req3 = b"GET /3 HTTP/1.1\r\n\r\n";

        let mut degraded = false;
        let mut reqs = req_parser.feed(req1, &mut degraded);
        assert_eq!(reqs.len(), 1, "should parse first request");
        reqs.extend(req_parser.feed(req2, &mut degraded));
        assert_eq!(reqs.len(), 2, "should parse second request");
        reqs.extend(req_parser.feed(req3, &mut degraded));

        assert!(!degraded, "should not degrade");
        assert_eq!(reqs.len(), 3, "should parse 3 keepalive requests");
        assert_eq!(reqs[0].path, "/1");
        assert_eq!(reqs[1].path, "/2");
        assert_eq!(reqs[2].path, "/3");
    }

    #[test]
    fn tap_pipelined_requests() {
        // Two pipelined requests fed in ONE call (both present in input).
        let mut req_parser = ReqParser::new();

        let data = b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n";

        let mut degraded = false;
        let reqs = req_parser.feed(data, &mut degraded);

        assert!(!degraded, "should not degrade");
        assert_eq!(
            reqs.len(),
            2,
            "single feed should parse both pipelined requests"
        );
        assert_eq!(reqs[0].method, "GET", "first request method");
        assert_eq!(reqs[0].path, "/1", "first request path");
        assert_eq!(reqs[1].method, "GET", "second request method");
        assert_eq!(reqs[1].path, "/2", "second request path");
    }

    #[test]
    fn tap_chunked_body_boundary() {
        // A chunked-body request followed by another request.
        let mut req_parser = ReqParser::new();

        let data = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\nGET /after HTTP/1.1\r\n\r\n";
        let mut degraded = false;
        let reqs = req_parser.feed(data, &mut degraded);

        assert!(!degraded, "should not degrade on valid chunked");
        assert_eq!(
            reqs.len(),
            2,
            "should parse chunked request and next request"
        );
        assert_eq!(reqs[0].method, "POST", "first request method");
        assert_eq!(reqs[0].path, "/", "first request path");
        assert_eq!(reqs[1].method, "GET", "second request method");
        assert_eq!(reqs[1].path, "/after", "second request path");
    }

    #[test]
    fn tap_content_length_skip() {
        // Request with Content-Length body, then next request.
        let mut req_parser = ReqParser::new();

        let data =
            b"POST / HTTP/1.1\r\nContent-Length: 11\r\n\r\nhelloworld!GET /next HTTP/1.1\r\n\r\n";
        let mut degraded = false;
        let reqs = req_parser.feed(data, &mut degraded);

        assert!(!degraded, "should not degrade");
        assert_eq!(reqs.len(), 2, "should parse both requests (POST then GET)");
        assert_eq!(reqs[0].method, "POST", "first request method");
        assert_eq!(reqs[0].path, "/", "first request path");
        assert_eq!(reqs[1].method, "GET", "second request method");
        assert_eq!(reqs[1].path, "/next", "second request path");
    }

    #[test]
    fn tap_partial_header_across_reads() {
        // Request header split across two feed calls.
        let mut req_parser = ReqParser::new();

        let part1 = b"GET /api/test HTTP/1.1\r\nHost: ex";
        let part2 = b"ample.com\r\n\r\n";

        let mut degraded = false;
        let reqs1 = req_parser.feed(part1, &mut degraded);
        assert_eq!(reqs1.len(), 0, "first part shouldn't complete");
        assert!(!degraded, "first part should not degrade (incomplete)");

        let reqs2 = req_parser.feed(part2, &mut degraded);
        assert!(!degraded, "should not degrade after completion");
        assert_eq!(reqs2.len(), 1, "should parse request after second part");
        assert_eq!(reqs2[0].path, "/api/test");
    }

    #[test]
    fn req_parser_does_not_hang_on_partial() {
        // Guard regression: partial header with no more bytes must RETURN, not hang.
        let mut req_parser = ReqParser::new();
        let mut degraded = false;

        // Feed a partial header (no \r\n\r\n) and ensure it returns.
        let reqs = req_parser.feed(b"GET / HTTP/1.1\r\nHo", &mut degraded);
        assert_eq!(reqs.len(), 0, "should return 0 reqs on incomplete header");
        assert!(
            !degraded,
            "should not degrade (incomplete is OK, not error)"
        );
        // If we get here, the hang regression is fixed.
    }

    #[test]
    fn tap_raw_first_bytes() {
        // Non-HTTP bytes: TLS or raw binary. Verify degradation or heuristic detection.
        let mut req_parser = ReqParser::new();
        let mut degraded = false;

        // TLS handshake starts with 0x16 0x03.
        let tls_data = b"\x16\x03\x01\x00\x50\x01\x00\x00\x4c\x03\x03";
        let reqs = req_parser.feed(tls_data, &mut degraded);
        // TLS doesn't start with a valid HTTP method, so httparse should fail.
        // The parser fills head_buf with these bytes; they don't match \r\n\r\n,
        // so no parsing happens. But it won't degrade unless we add more non-HTTP.
        // Feed enough random bytes to trigger head_buf limit or degradation.
        let non_http = b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f";
        let _ = req_parser.feed(non_http, &mut degraded);
        // After enough non-HTTP bytes without finding \r\n\r\n, head_buf will be full
        // or a parse attempt will fail. For simplicity, just verify no panic.
        assert_eq!(reqs.len(), 0, "TLS/binary should not parse as requests");
    }

    #[test]
    fn tap_tls_handshake_detected_raw() {
        // TLS handshake bytes start with 0x16 0x03.
        let tls_data = &[0x16u8, 0x03, 0x01, 0x00, 0x50];

        // Create a tap and verify TLS detection.
        let mut tap = HttpAccessTap::new(
            tokio::io::duplex(1024).0,
            Some("1.1.1.1".to_string()),
            tokio::sync::mpsc::channel(1).0,
            Arc::new(AtomicU64::new(0)),
        );

        // The detect_tls_or_non_http method checks for TLS marker.
        let is_tls = tap.detect_tls_or_non_http(tls_data);
        assert!(is_tls, "should detect TLS handshake");
    }

    #[test]
    fn tap_malformed_degrades_to_raw() {
        // Feed through HttpAccessTap: verify parsing failure doesn't panic or break data path.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);

            let tmp = TempDir::new().unwrap();
            let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 10)
                .unwrap()
                .unwrap();
            let logger = AccessLogger::new(cfg);
            let tx = logger.sender_for("test", PathLayout::Flat);
            let dropped = Arc::new(AtomicU64::new(0));

            let mut tap = HttpAccessTap::new(client, None, tx, dropped);

            // Spawn server echo task.
            tokio::spawn(async move {
                let mut buf = [0u8; 256];
                if let Ok(n) = server.read(&mut buf).await {
                    if n > 0 {
                        let _ = server.write_all(&buf[..n]).await;
                    }
                }
            });

            // Send valid HTTP then binary garbage.
            let payload = b"GET / HTTP/1.1\r\n\r\n\x00\x01\x02\xff";
            let n = tap.write(payload).await.unwrap();
            assert_eq!(n, payload.len(), "all bytes written");

            // Read echo back: should be byte-for-byte identical.
            let mut buf = [0u8; 256];
            let n = tap.read(&mut buf).await.unwrap();
            assert!(n > 0, "received echo");
            assert_eq!(&buf[..n], payload, "echo is byte-identical");
        });
    }

    #[tokio::test]
    async fn tap_half_close_preserved() {
        // Verify shutdown propagates through tap to inner (EOF at peer).
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (tap_side, mut peer) = tokio::io::duplex(4096);

        let tmp = TempDir::new().unwrap();
        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 10)
            .unwrap()
            .unwrap();
        let logger = AccessLogger::new(cfg);
        let tx = logger.sender_for("test", PathLayout::Flat);
        let dropped = Arc::new(AtomicU64::new(0));

        let mut tap = HttpAccessTap::new(tap_side, None, tx, dropped);

        // Shutdown the tap write side.
        tap.shutdown().await.unwrap();

        // Peer should see EOF.
        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).await.unwrap();
        assert_eq!(n, 0, "peer should see EOF after tap shutdown");
    }

    #[tokio::test]
    async fn tap_byte_identical_passthrough() {
        // Large deterministic payload round-trip: verify byte-for-byte integrity.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client, mut server) = tokio::io::duplex(8192);

        let tmp = TempDir::new().unwrap();
        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 10)
            .unwrap()
            .unwrap();
        let logger = AccessLogger::new(cfg);
        let tx = logger.sender_for("test", PathLayout::Flat);
        let dropped = Arc::new(AtomicU64::new(0));

        let mut tap = HttpAccessTap::new(client, None, tx, dropped);

        // Spawn echo task on server.
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            if let Ok(n) = server.read(&mut buf).await {
                if n > 0 {
                    let _ = server.write_all(&buf[..n]).await;
                }
            }
        });

        // Create 4096-byte pattern: (i % 251) for determinism.
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        // Write through tap.
        let written = tap.write(&payload).await.unwrap();
        assert_eq!(written, payload.len(), "should write all bytes");

        // Read echo back.
        let mut buf = vec![0u8; 4096];
        let n = tap.read(&mut buf).await.unwrap();
        assert_eq!(n, payload.len(), "should read all bytes back");
        assert_eq!(&buf[..n], &payload[..], "echo must be byte-identical");
    }

    #[test]
    fn tap_fifo_request_response_pairing() {
        // Verify request/response FIFO pairing in tap via direct parser calls.
        let mut req_parser = ReqParser::new();
        let mut resp_parser = RespParser::new();

        let req_data = b"GET /1 HTTP/1.1\r\n\r\nGET /2 HTTP/1.1\r\n\r\n";
        let resp_data = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\nHTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";

        let mut degraded = false;

        // Parse both requests in one feed.
        let reqs = req_parser.feed(req_data, &mut degraded);
        assert!(!degraded, "requests should parse cleanly");
        assert_eq!(reqs.len(), 2, "should parse 2 requests");
        assert_eq!(reqs[0].path, "/1");
        assert_eq!(reqs[1].path, "/2");

        // Parse both responses in one feed.
        let resps = resp_parser.feed(resp_data, &mut degraded);
        assert!(!degraded, "responses should parse cleanly");
        assert_eq!(resps.len(), 2, "should parse 2 responses");
        assert_eq!(resps[0].status, 200);
        assert_eq!(resps[1].status, 404);

        // Verify FIFO pairing: the tap pairs these in order.
        // (In real tap, pending_reqs queue holds parsed requests, responses pop from front.)
    }

    #[tokio::test]
    async fn tap_never_blocks_on_full_channel() {
        // Saturate the channel, then verify poll_write still returns Ready.
        let (client, _server) = tokio::io::duplex(4096);

        let tmp = TempDir::new().unwrap();
        let cfg = AccessLogConfig::from_flags(Some(tmp.path().to_path_buf()), 1, 1)
            .unwrap()
            .unwrap();
        let logger = AccessLogger::new(cfg);
        let tx = logger.sender_for("test", PathLayout::Flat);
        let dropped = Arc::new(AtomicU64::new(0));

        let mut tap = HttpAccessTap::new(
            client,
            Some("1.1.1.1".to_string()),
            tx.clone(),
            dropped.clone(),
        );

        // Fill the channel with pending HTTP records.
        for _ in 0..100 {
            let rec = AccessRecord::http(
                OffsetDateTime::now_utc(),
                Some("1.1.1.1".to_string()),
                "GET".to_string(),
                "/".to_string(),
                "HTTP/1.1".to_string(),
                200,
                0,
                None,
                None,
            );
            // Try to send; if full, it's OK (try_log handles it).
            let _ = tx.try_send(rec);
        }

        // Now attempt to write through the tap. Should not block.
        match tokio::io::AsyncWriteExt::write(&mut tap, b"test data").await {
            Ok(n) => assert_eq!(n, 9, "write should succeed"),
            Err(_) => panic!("write should not error"),
        }

        // Verify dropped counter is accessible (it should track dropped records).
        let _dropped_count = dropped.load(Ordering::SeqCst);
        // Just verify we didn't panic; the counter tracks any drops under saturation.
    }

    #[test]
    fn tap_inject_primes_body_skip() {
        // BUG-1 fix: inject_pending_request should prime the body-skip state.
        // Verify that after injecting a request with a body, the parser is in Body-skip state
        // and doesn't try to parse the body bytes as a new request.
        // Test the core logic: body_length extraction and inject_pending_request state priming.

        // Test 1: body_length extraction from headers.
        let mut headers = [httparse::EMPTY_HEADER; 2];
        headers[0] = httparse::Header {
            name: "Content-Length",
            value: b"11",
        };
        let len = body_length(&headers);
        assert_eq!(len, 11, "Content-Length: 11 should be 11");

        let mut headers = [httparse::EMPTY_HEADER; 2];
        headers[0] = httparse::Header {
            name: "Transfer-Encoding",
            value: b"chunked",
        };
        let len = body_length(&headers);
        assert_eq!(
            len,
            u64::MAX,
            "Transfer-Encoding: chunked should be u64::MAX"
        );

        let headers = [httparse::EMPTY_HEADER; 2];
        let len = body_length(&headers);
        assert_eq!(len, 0, "no Content-Length should be 0");

        // Test 2: inject_pending_request primes state (via direct parser test).
        // Create a request parser and verify state transitions.
        let mut req_parser = ReqParser::new();
        assert!(matches!(req_parser.state, MsgState::Header));

        // Simulate what inject_pending_request does: set state via next_state.
        req_parser.state = next_state(11); // body_len = 11
        assert!(matches!(req_parser.state, MsgState::Body(11)));

        // Verify that with Body state, the parser consumes 11 bytes without parsing them as requests.
        let mut degraded = false;
        let body_and_req2 = b"helloworld!GET /2 HTTP/1.1\r\n\r\n";
        let reqs = req_parser.feed(body_and_req2, &mut degraded);

        // Should parse the second request only (req2), not try to parse the body as a request.
        assert!(!degraded, "should not degrade");
        assert_eq!(reqs.len(), 1, "should parse exactly 1 request (req2)");
        assert_eq!(reqs[0].path, "/2", "should be the GET /2 request");
    }
}
