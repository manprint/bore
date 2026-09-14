//! Web-transfer protocol v1 codecs: control envelopes, canonical JSON,
//! manifest validation, content roots, key derivation and encrypted frames.
//!
//! Normative contract: `docs/transfer/WEB_TRANSFER_PROTOCOL.md`. Shared
//! fixtures under `tests/fixtures/web_transfer/v1/` pin every byte; the JS
//! mirror lives in `web/transfer/src/{protocol,crypto}.js`. Pure functions
//! only — no sockets, no tasks, no server state.

use std::{collections::BTreeMap, fmt, str::FromStr};

use anyhow::{bail, Result};
use ring::{aead, digest, hkdf};

use crate::web_transfer::{
    AttemptId, OfferId, PeerId, RelayTicket, RoomId, RoomKey, TransferId,
    WEB_TRANSFER_MAX_CONTROL_BYTES,
};

/// Browser control-protocol version; envelopes with another `v` are rejected.
pub const PROTOCOL_VERSION: u16 = crate::web_transfer::WEB_TRANSFER_PROTOCOL_VERSION;
/// Exact WebSocket subprotocol required on `/transfer/ws/control/<room>`.
pub const CONTROL_SUBPROTOCOL: &str = "bore-transfer-v1";

/// Client → server control message names, in protocol order.
pub const CLIENT_TYPES: &[&str] = &[
    "hello",
    "ping",
    "peer.rename",
    "offer.publish",
    "offer.withdraw",
    "transfer.request",
    "transfer.source_ready",
    "transfer.reject",
    "rtc.offer",
    "rtc.answer",
    "rtc.ice",
    "transfer.direct_ready",
    "transfer.direct_failed",
    "transfer.cancel",
    "transfer.progress",
    "transfer.complete",
];

/// Server → client control message names, in protocol order.
pub const SERVER_TYPES: &[&str] = &[
    "welcome",
    "snapshot.begin",
    "snapshot.peer",
    "snapshot.offer",
    "snapshot.end",
    "ack",
    "error",
    "pong",
    "peer.joined",
    "peer.renamed",
    "peer.left",
    "offer.added",
    "offer.removed",
    "transfer.incoming",
    "transfer.direct_start",
    "transfer.path_commit",
    "transfer.relay_ticket",
    "transfer.cancelled",
    "transfer.completed",
    "room_closed",
];

/// Fixed error-code set; anything else on the wire maps to `INTERNAL`.
pub const ERROR_CODES: &[&str] = &[
    "UNSUPPORTED_VERSION",
    "ROOM_UNAVAILABLE",
    "UNAUTHORIZED",
    "INVALID_MESSAGE",
    "RATE_LIMITED",
    "LIMIT_EXCEEDED",
    "OFFER_NOT_FOUND",
    "OFFER_CHANGED",
    "TRANSFER_NOT_FOUND",
    "NOT_PARTICIPANT",
    "SOURCE_OFFLINE",
    "SOURCE_CHANGED",
    "DIRECT_FAILED",
    "RELAY_BUSY",
    "STORAGE_QUOTA",
    "CANCELLED",
    "INTERNAL",
];

/// Returns true for a known error code.
pub fn is_known_error_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

/// Client messages that must carry a `requestId` (every mutation; only
/// `hello`/`ping` are request-free).
pub fn client_type_requires_request_id(typ: &str) -> bool {
    typ != "hello" && typ != "ping"
}

/// Correlates a mutating control request with its `ack`/`error` reply.
/// 128-bit value, wire form is canonical lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId([u8; 16]);

impl RequestId {
    /// Builds the ID from raw bytes (generation sites only).
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Raw bytes; crate-internal to keep construction sites auditable.
    /// Envelope builders are the consumers.
    #[allow(dead_code)]
    pub(crate) fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", hex::encode(self.0))
    }
}

impl FromStr for RequestId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 {
            return Err(format!("RequestId: need 32 hex chars, got {}", s.len()));
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("RequestId: need canonical lowercase hex".to_string());
        }
        let raw = hex::decode(s).map_err(|e| format!("RequestId: invalid hex: {e}"))?;
        let bytes: [u8; 16] = raw
            .try_into()
            .map_err(|_| "RequestId: contradiction in hex length".to_string())?;
        Ok(Self(bytes))
    }
}

/// A validated control envelope: `{v, type, requestId?, body}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEnvelope {
    /// Message name from the fixed type tables.
    pub typ: String,
    /// Present on every client mutation; echoed by server `ack`/`error`.
    pub request_id: Option<RequestId>,
    /// Message payload; always a JSON object after validation.
    pub body: serde_json::Value,
}

/// Shared envelope checks: size cap, object shape, exact top-level fields,
/// version, known type and requestId presence rules.
fn parse_envelope(raw: &str, known: &[&str], side: &str) -> Result<ParsedEnvelope> {
    if raw.len() > WEB_TRANSFER_MAX_CONTROL_BYTES {
        bail!("{side} control message exceeds 320 KiB");
    }
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("{side} control message is not JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{side} control message must be an object"))?;
    for key in obj.keys() {
        if !["v", "type", "requestId", "body"].contains(&key.as_str()) {
            bail!("{side} control message has unknown field {key:?}");
        }
    }
    let version = obj
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("{side} control message needs integer v"))?;
    if version != u64::from(PROTOCOL_VERSION) {
        bail!("unsupported control version {version}");
    }
    let typ = obj
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{side} control message needs string type"))?;
    if !known.contains(&typ) {
        bail!("unknown {side} control type {typ:?}");
    }
    let request_id = match obj.get("requestId") {
        None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("requestId must be a string"))?;
            Some(s.parse::<RequestId>().map_err(|e| anyhow::anyhow!(e))?)
        }
    };
    let body = obj
        .get("body")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{side} control message needs body"))?;
    if !body.is_object() {
        bail!("{side} control body must be an object");
    }
    Ok(ParsedEnvelope {
        typ: typ.to_string(),
        request_id,
        body,
    })
}

/// Validates one client → server control message.
pub fn parse_client_envelope(raw: &str) -> Result<ParsedEnvelope> {
    let env = parse_envelope(raw, CLIENT_TYPES, "client")?;
    let needs = client_type_requires_request_id(&env.typ);
    if needs && env.request_id.is_none() {
        bail!("client {} requires requestId", env.typ);
    }
    if !needs && env.request_id.is_some() {
        bail!("client {} must not carry requestId", env.typ);
    }
    Ok(env)
}

/// Validates one server → client control message.
pub fn parse_server_envelope(raw: &str) -> Result<ParsedEnvelope> {
    let env = parse_envelope(raw, SERVER_TYPES, "server")?;
    let needs = env.typ == "ack" || env.typ == "error";
    if needs && env.request_id.is_none() {
        bail!("server {} must echo requestId", env.typ);
    }
    if !needs && env.request_id.is_some() {
        bail!("server {} must not carry requestId", env.typ);
    }
    Ok(env)
}

/// Builds a canonical server `ack` echoing `request_id`.
pub fn ack_envelope(request_id: RequestId, result: Option<serde_json::Value>) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "requestId".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    if let Some(result) = result {
        body.insert("result".to_string(), result);
    }
    server_envelope("ack", Some(request_id), body)
}

/// Builds a canonical server `error` echoing `request_id`.
pub fn error_envelope(request_id: RequestId, code: &str, message: Option<&str>) -> String {
    let code = if is_known_error_code(code) {
        code
    } else {
        "INTERNAL"
    };
    let mut body = BTreeMap::new();
    body.insert(
        "requestId".to_string(),
        serde_json::Value::String(request_id.to_string()),
    );
    body.insert(
        "code".to_string(),
        serde_json::Value::String(code.to_string()),
    );
    if let Some(message) = message {
        body.insert(
            "message".to_string(),
            serde_json::Value::String(message.to_string()),
        );
    }
    server_envelope("error", Some(request_id), body)
}

fn server_envelope(
    typ: &str,
    request_id: Option<RequestId>,
    body: BTreeMap<String, serde_json::Value>,
) -> String {
    let mut top = BTreeMap::new();
    top.insert(
        "body".to_string(),
        serde_json::Value::Object(body.into_iter().collect()),
    );
    if let Some(id) = request_id {
        top.insert(
            "requestId".to_string(),
            serde_json::Value::String(id.to_string()),
        );
    }
    top.insert(
        "type".to_string(),
        serde_json::Value::String(typ.to_string()),
    );
    top.insert(
        "v".to_string(),
        serde_json::Value::Number(serde_json::Number::from(u64::from(PROTOCOL_VERSION))),
    );
    canonical_json(&serde_json::Value::Object(top.into_iter().collect()))
        .expect("envelope builder emits canonical JSON")
}

/// Largest JSON safe integer (`2^53 - 1`).
pub const JSON_SAFE_INTEGER_MAX: u64 = 9007199254740991;

/// Canonical JSON: keys sorted by Unicode code-point order (UTF-8 byte order),
/// no whitespace, integers within the JSON safe range, no floats.
pub fn canonical_json(value: &serde_json::Value) -> Result<String> {
    let mut out = String::new();
    write_canonical(value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &serde_json::Value, out: &mut String) -> Result<()> {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(true) => out.push_str("true"),
        serde_json::Value::Bool(false) => out.push_str("false"),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u > JSON_SAFE_INTEGER_MAX {
                    bail!("integer {u} exceeds JSON safe range");
                }
                out.push_str(&u.to_string());
            } else if let Some(i) = n.as_i64() {
                if i.unsigned_abs() > JSON_SAFE_INTEGER_MAX {
                    bail!("integer {i} exceeds JSON safe range");
                }
                out.push_str(&i.to_string());
            } else {
                bail!("non-integer numbers have no canonical form");
            }
        }
        serde_json::Value::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("string escapes"));
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            // BTreeMap order is byte-lexicographic, which equals Unicode
            // code-point order for UTF-8; sort explicitly to stay correct
            // even if the map type ever changes.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("string escapes"));
                out.push(':');
                write_canonical(&map[*key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Parses a decimal-string u64 (manifest sizes/timestamps); rejects empty,
/// signed, padded or overflowing input.
pub fn parse_decimal_u64(what: &str, s: &str) -> Result<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{what} must be a decimal string, got {s:?}");
    }
    if s.len() > 1 && s.starts_with('0') {
        bail!("{what} must not be zero-padded");
    }
    s.parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{what} overflows u64"))
}

/// Validates one manifest path: `/`-separated, NFC, bounded segments.
pub fn validate_manifest_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > crate::web_transfer::WEB_TRANSFER_MAX_PATH_BYTES {
        bail!("manifest path has bad length");
    }
    if path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        bail!("manifest path must be relative without backslashes");
    }
    if path.bytes().any(|b| b.is_ascii_control()) {
        bail!("manifest path holds control characters");
    }
    if !unicode_normalization::is_nfc(path) {
        bail!("manifest path must be NFC-normalized");
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("manifest path holds an empty or dot segment");
        }
        if segment.len() > crate::web_transfer::WEB_TRANSFER_MAX_PATH_SEGMENT_BYTES {
            bail!("manifest path segment exceeds 255 bytes");
        }
    }
    Ok(())
}

/// Validates a peer display name: 1..=48 characters, no control characters.
pub fn validate_display_name(name: &str) -> Result<()> {
    let chars = name.chars().count();
    if chars == 0 || chars > crate::web_transfer::WEB_TRANSFER_MAX_DISPLAY_NAME_CHARS {
        bail!("display name must be 1..=48 characters");
    }
    if name.chars().any(char::is_control) {
        bail!("display name holds control characters");
    }
    Ok(())
}

/// One manifest entry: a single file with its 1 MiB chunk hashes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Relative NFC path.
    pub path: String,
    /// Logical file size in bytes.
    pub size: u64,
    /// Last-modified Unix seconds.
    pub mtime: u64,
    /// One SHA-256 per 1 MiB chunk, in order.
    pub chunks: Vec<[u8; 32]>,
}

/// Manifest offer mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestMode {
    /// Exactly one file entry.
    Single,
    /// File tree / ZIP source.
    Multi,
}

impl ManifestMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Multi => "multi",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "single" => Ok(Self::Single),
            "multi" => Ok(Self::Multi),
            other => bail!("manifest mode must be single|multi, got {other:?}"),
        }
    }
}

/// A validated immutable offer manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Offer this manifest describes.
    pub offer: OfferId,
    /// Single-file or tree offer.
    pub mode: ManifestMode,
    /// File entries in wire order.
    pub entries: Vec<ManifestEntry>,
}

fn exact_object<'a>(
    value: &'a serde_json::Value,
    what: &str,
    fields: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{what} must be an object"))?;
    for key in obj.keys() {
        if !fields.contains(&key.as_str()) {
            bail!("{what} has unknown field {key:?}");
        }
    }
    for field in fields {
        if !obj.contains_key(*field) {
            bail!("{what} misses field {field:?}");
        }
    }
    Ok(obj)
}

fn get_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    what: &str,
    field: &str,
) -> Result<&'a str> {
    obj.get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{what} needs string {field:?}"))
}

/// Parses and validates a manifest value (unknown fields rejected).
pub fn parse_manifest(value: &serde_json::Value) -> Result<Manifest> {
    let obj = exact_object(value, "manifest", &["offer", "mode", "entries"])?;
    let offer = get_str(obj, "manifest", "offer")
        .and_then(|s| s.parse::<OfferId>().map_err(|e| anyhow::anyhow!(e)))?;
    let mode = ManifestMode::parse(get_str(obj, "manifest", "mode")?)?;
    let entries_raw = obj
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("manifest needs entries array"))?;
    if entries_raw.is_empty() {
        bail!("manifest needs at least one entry");
    }
    let mut entries = Vec::with_capacity(entries_raw.len().min(1024));
    for entry in entries_raw {
        let e = exact_object(
            entry,
            "manifest entry",
            &["path", "size", "mtime", "chunks"],
        )?;
        let path = get_str(e, "manifest entry", "path")?;
        validate_manifest_path(path)?;
        let size = parse_decimal_u64("entry size", get_str(e, "manifest entry", "size")?)?;
        let mtime = parse_decimal_u64("entry mtime", get_str(e, "manifest entry", "mtime")?)?;
        let chunks_raw = e
            .get("chunks")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("manifest entry needs chunks array"))?;
        let mut chunks = Vec::with_capacity(chunks_raw.len().min(1024));
        for chunk in chunks_raw {
            let hex = chunk
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("chunk hash must be a string"))?;
            chunks.push(
                crate::web_transfer::parse_hex_id::<32>("chunk", hex)
                    .map_err(|e| anyhow::anyhow!(e))?,
            );
        }
        // Chunk count must cover the size: ceil(size / 1 MiB), empty file → 0.
        let want = usize::try_from(size.div_ceil(1024 * 1024)).unwrap_or(usize::MAX);
        if chunks.len() != want {
            bail!("entry {path:?} needs {want} chunk hashes for {size} bytes");
        }
        entries.push(ManifestEntry {
            path: path.to_string(),
            size,
            mtime,
            chunks,
        });
    }
    if mode == ManifestMode::Single && entries.len() != 1 {
        bail!("single manifest needs exactly one entry");
    }
    Ok(Manifest {
        offer,
        mode,
        entries,
    })
}

/// Renders a manifest back to its canonical JSON value (decimal strings).
pub fn manifest_value(manifest: &Manifest) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = manifest
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "chunks": e.chunks.iter().map(hex::encode).collect::<Vec<_>>(),
                "mtime": e.mtime.to_string(),
                "path": e.path,
                "size": e.size.to_string(),
            })
        })
        .collect();
    serde_json::json!({
        "entries": entries,
        "mode": manifest.mode.as_str(),
        "offer": manifest.offer.to_string(),
    })
}

/// SHA-256 over arbitrary bytes.
fn sha256(data: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, data)
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
}

/// Fixed rolling-root over one entry's chunk leaves (SHA-256 each).
/// `leaves.len()` must equal `chunk_count`; the domain separator binds the
/// count so a truncated leaf list cannot collide with a shorter file.
pub fn file_root(chunk_count: u64, leaves: &[[u8; 32]]) -> Result<[u8; 32]> {
    if leaves.len() as u64 != chunk_count {
        bail!("root needs exactly {chunk_count} leaves");
    }
    let mut input = Vec::with_capacity(16 + 8 + leaves.len() * 32);
    input.extend_from_slice(b"bore-web-root-v1");
    input.extend_from_slice(&chunk_count.to_be_bytes());
    for leaf in leaves {
        input.extend_from_slice(leaf);
    }
    Ok(sha256(&input))
}

struct OneKey([u8; 32]);

impl hkdf::KeyType for OneKey {
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// HKDF-SHA256 to exactly 32 bytes.
fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let mut out = [0u8; 32];
    prk.expand(&[info], OneKey(out))
        .and_then(|okm| okm.fill(&mut out))
        .expect("HKDF-SHA256 to 32 bytes");
    out
}

/// Manifest authentication key: binds room key and room id.
pub fn manifest_key(room_key: &RoomKey, room_id: &RoomId) -> [u8; 32] {
    hkdf32(
        room_key_bytes(room_key),
        b"bore-web-manifest-v1",
        room_id_bytes(room_id),
    )
}

fn room_key_bytes(key: &RoomKey) -> &[u8; 32] {
    // Same module tree: the accessor is crate-visible.
    key.as_bytes()
}

fn room_id_bytes(id: &RoomId) -> &[u8; 16] {
    id.as_bytes()
}

/// HMAC-SHA256 over canonical manifest bytes.
pub fn manifest_mac(key: &[u8; 32], canonical: &[u8]) -> [u8; 32] {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&key, canonical)
        .as_ref()
        .try_into()
        .expect("HMAC-SHA256 is 32 bytes")
}

/// Verifies a manifest MAC in constant time.
pub fn verify_manifest_mac(key: &[u8; 32], canonical: &[u8], mac: &[u8; 32]) -> Result<()> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::verify(&key, canonical, mac).map_err(|_| anyhow::anyhow!("manifest MAC mismatch"))
}

/// Per-attempt payload key: binds room key, transfer and attempt.
pub fn attempt_key(
    room_key: &RoomKey,
    transfer_id: &TransferId,
    attempt_id: &AttemptId,
) -> [u8; 32] {
    let mut info = [0u8; 32];
    info[..16].copy_from_slice(transfer_id.as_bytes());
    info[16..].copy_from_slice(attempt_id.as_bytes());
    hkdf32(room_key_bytes(room_key), b"bore-web-attempt-v1", &info)
}

/// GCM nonce for one frame: sequence-bound, unique per attempt key.
pub fn frame_nonce(seq: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&(u64::from(seq)).to_be_bytes());
    nonce
}

/// Encrypted frame types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    /// File-content fragment, 1..=24576 plaintext bytes.
    Data = 1,
    /// End of stream: plaintext is exactly `u64be(total_bytes)`.
    Final = 2,
}

impl TryFrom<u8> for FrameType {
    type Error = anyhow::Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Data),
            2 => Ok(Self::Final),
            other => bail!("unknown frame type {other}"),
        }
    }
}

/// Magic `BWT1` opening every encrypted frame header.
pub const FRAME_MAGIC: u32 = 0x4257_5431;
/// Largest plaintext fragment (`DATA`).
pub const FRAME_MAX_PLAINTEXT: usize = 24 * 1024;
/// Largest ciphertext+tag the decoder buffers (32 KiB cap, checked first).
pub const FRAME_MAX_BODY: usize = 32 * 1024;
/// Header length in bytes.
pub const FRAME_HEADER_LEN: usize = 16;

/// A decoded, authenticated frame (sequence policy stays with the caller).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Frame kind.
    pub ftype: FrameType,
    /// Fragment sequence.
    pub seq: u32,
    /// Authenticated plaintext.
    pub plaintext: Vec<u8>,
}

/// Builds the 16-byte big-endian header for `body_len` trailing bytes.
fn frame_header(ftype: FrameType, seq: u32, body_len: u32) -> [u8; FRAME_HEADER_LEN] {
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[..4].copy_from_slice(&FRAME_MAGIC.to_be_bytes());
    header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    header[6] = ftype as u8;
    header[7] = 0;
    header[8..12].copy_from_slice(&seq.to_be_bytes());
    header[12..16].copy_from_slice(&body_len.to_be_bytes());
    header
}

fn frame_key(key: &[u8; 32]) -> Result<aead::LessSafeKey> {
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, key)
        .map_err(|_| anyhow::anyhow!("bad frame key"))?;
    Ok(aead::LessSafeKey::new(unbound))
}

/// Seals one frame: header + `AES-256-GCM(key, nonce(seq), aad=header)`.
pub fn seal_frame(key: &[u8; 32], seq: u32, ftype: FrameType, plaintext: &[u8]) -> Result<Vec<u8>> {
    match ftype {
        FrameType::Data => {
            if plaintext.is_empty() || plaintext.len() > FRAME_MAX_PLAINTEXT {
                bail!("DATA plaintext must be 1..=24576 bytes");
            }
        }
        FrameType::Final => {
            if plaintext.len() != 8 {
                bail!("FINAL plaintext must be u64be(total), 8 bytes");
            }
        }
    }
    let seal = frame_key(key)?;
    let nonce_bytes = frame_nonce(seq);
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_bytes)
        .map_err(|_| anyhow::anyhow!("nonce construction"))?;
    // Ciphertext length is bounded before any allocation below.
    let body_len = u32::try_from(plaintext.len() + aead::AES_256_GCM.tag_len())
        .map_err(|_| anyhow::anyhow!("fragment too large"))?;
    let header = frame_header(ftype, seq, body_len);
    let mut buf = plaintext.to_vec();
    seal.seal_in_place_append_tag(nonce, aead::Aad::from(&header), &mut buf)
        .map_err(|_| anyhow::anyhow!("seal failed"))?;
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + buf.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&buf);
    Ok(out)
}

/// Opens one frame: strict header checks, `body_len` capped before
/// allocating, GCM authentication, then type-specific plaintext checks.
/// `min_seq` is the least acceptable sequence (replay/reorder gate); stale
/// attempt frames fail GCM first because their key differs.
pub fn open_frame(key: &[u8; 32], msg: &[u8], min_seq: u32) -> Result<DecodedFrame> {
    if msg.len() < FRAME_HEADER_LEN + aead::AES_256_GCM.tag_len() {
        bail!("frame shorter than header plus tag");
    }
    let (header, body) = msg.split_at(FRAME_HEADER_LEN);
    if u32::from_be_bytes(header[..4].try_into().expect("4 magic bytes")) != FRAME_MAGIC {
        bail!("bad frame magic");
    }
    if u16::from_be_bytes(header[4..6].try_into().expect("2 version bytes")) != PROTOCOL_VERSION {
        bail!("bad frame version");
    }
    let ftype = FrameType::try_from(header[6])?;
    if header[7] != 0 {
        bail!("frame reserved bits must be zero");
    }
    let seq = u32::from_be_bytes(header[8..12].try_into().expect("4 seq bytes"));
    let body_len = u32::from_be_bytes(header[12..16].try_into().expect("4 length bytes"));
    if body_len as usize > FRAME_MAX_BODY {
        bail!("frame body_len exceeds the 32 KiB cap");
    }
    if body_len as usize != body.len() {
        bail!("frame body_len does not match trailing bytes");
    }
    if seq < min_seq {
        bail!("frame sequence is stale");
    }
    let open = frame_key(key)?;
    let nonce_bytes = frame_nonce(seq);
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce_bytes)
        .map_err(|_| anyhow::anyhow!("nonce construction"))?;
    // Bounded by the body_len cap checked above.
    let mut buf = Vec::with_capacity(body.len());
    buf.extend_from_slice(body);
    let plaintext = open
        .open_in_place(nonce, aead::Aad::from(header), &mut buf)
        .map_err(|_| anyhow::anyhow!("frame authentication failed"))?;
    match ftype {
        FrameType::Data => {
            if plaintext.is_empty() || plaintext.len() > FRAME_MAX_PLAINTEXT {
                bail!("DATA plaintext must be 1..=24576 bytes");
            }
        }
        FrameType::Final => {
            if plaintext.len() != 8 {
                bail!("FINAL plaintext must be u64be(total), 8 bytes");
            }
        }
    }
    Ok(DecodedFrame {
        ftype,
        seq,
        plaintext: plaintext.to_vec(),
    })
}

/// Relay leg role in a `relay.attach` message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayRole {
    /// Uploads file bytes.
    Source,
    /// Downloads file bytes.
    Recipient,
}

impl RelayRole {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "source" => Ok(Self::Source),
            "recipient" => Ok(Self::Recipient),
            other => bail!("relay role must be source|recipient, got {other:?}"),
        }
    }
}

/// A validated `relay.attach` first message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayAttach {
    /// Attaching peer.
    pub peer_id: PeerId,
    /// Transfer being attached.
    pub transfer_id: TransferId,
    /// Attempt being attached.
    pub attempt_id: AttemptId,
    /// Which leg this socket serves.
    pub role: RelayRole,
    /// One-use ticket issued for this leg.
    pub ticket: RelayTicket,
}

/// Validates a `relay.attach` JSON text (exact fields, strict IDs).
pub fn parse_relay_attach(raw: &str) -> Result<RelayAttach> {
    if raw.len() > WEB_TRANSFER_MAX_CONTROL_BYTES {
        bail!("relay.attach exceeds 320 KiB");
    }
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| anyhow::anyhow!("relay.attach is not JSON: {e}"))?;
    let obj = exact_object(
        &value,
        "relay.attach",
        &["v", "peerId", "transferId", "attemptId", "role", "ticket"],
    )?;
    let version = obj
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("relay.attach needs integer v"))?;
    if version != u64::from(PROTOCOL_VERSION) {
        bail!("unsupported relay.attach version {version}");
    }
    // Each ID parses through its own newtype so widths are enforced per field.
    let peer_id = get_str(obj, "relay.attach", "peerId")
        .and_then(|s| s.parse::<PeerId>().map_err(|e| anyhow::anyhow!(e)))?;
    let transfer_id = get_str(obj, "relay.attach", "transferId")
        .and_then(|s| s.parse::<TransferId>().map_err(|e| anyhow::anyhow!(e)))?;
    let attempt_id = get_str(obj, "relay.attach", "attemptId")
        .and_then(|s| s.parse::<AttemptId>().map_err(|e| anyhow::anyhow!(e)))?;
    let role = RelayRole::parse(get_str(obj, "relay.attach", "role")?)?;
    let ticket = get_str(obj, "relay.attach", "ticket")
        .and_then(|s| s.parse::<RelayTicket>().map_err(|e| anyhow::anyhow!(e)))?;
    Ok(RelayAttach {
        peer_id,
        transfer_id,
        attempt_id,
        role,
        ticket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/web_transfer/v1/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read fixture {name}"))
    }

    #[test]
    fn request_id_round_trips_canonical_hex_only() {
        let raw = "0123456789abcdef0123456789abcdef";
        let id: RequestId = raw.parse().unwrap();
        assert_eq!(id.to_string(), raw);
        assert_eq!(id.as_bytes().len(), 16);
        assert!("0123456789ABCDEF0123456789ABCDEF"
            .parse::<RequestId>()
            .is_err());
        assert!("short".parse::<RequestId>().is_err());
    }

    #[test]
    fn protocol_version_matches_domain() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(CONTROL_SUBPROTOCOL, "bore-transfer-v1");
    }

    #[test]
    fn canonical_manifest_fixture_matches_byte_for_byte() {
        let manifest_raw = fixture("manifest.json");
        let manifest_raw_value: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
        let manifest = parse_manifest(&manifest_raw_value).unwrap();
        // Round-trip through the typed manifest: canonical bytes must equal
        // the checked-in canonical fixture, not just re-parse.
        let canonical = canonical_json(&manifest_value(&manifest)).unwrap();
        let expected = fixture("manifest.canonical.json");
        assert_eq!(canonical.as_bytes(), expected.as_bytes());
    }

    #[test]
    fn manifest_hmac_fixture_matches() {
        let vectors: serde_json::Value =
            serde_json::from_str(&fixture("crypto-vectors.json")).unwrap();
        let room_key = hex_to_32(vectors["inputs"]["room_key"].as_str().unwrap());
        let room_id: RoomId = vectors["inputs"]["room_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let key = manifest_key(&RoomKey::from_bytes(room_key), &room_id);
        let canonical = fixture("manifest.canonical.json");
        let mac = manifest_mac(&key, canonical.as_bytes());
        assert_eq!(
            hex::encode(mac),
            vectors["expected"]["manifest_mac_hex"].as_str().unwrap()
        );
        verify_manifest_mac(&key, canonical.as_bytes(), &mac).unwrap();
        let mut bad = mac;
        bad[0] ^= 1;
        assert!(verify_manifest_mac(&key, canonical.as_bytes(), &bad).is_err());
    }

    #[test]
    fn file_root_empty_single_and_multichunk_vectors_match() {
        let vectors: serde_json::Value =
            serde_json::from_str(&fixture("crypto-vectors.json")).unwrap();
        let expected = &vectors["expected"];
        let empty = file_root(0, &[]).unwrap();
        assert_eq!(
            hex::encode(empty),
            expected["root_empty_hex"].as_str().unwrap()
        );
        let single_leaf = hex_to_32(expected["leaf_hello_hex"].as_str().unwrap());
        let single = file_root(1, &[single_leaf]).unwrap();
        assert_eq!(
            hex::encode(single),
            expected["root_single_hex"].as_str().unwrap()
        );
        let leaves: Vec<[u8; 32]> = expected["multichunk_leaves_hex"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| hex_to_32(v.as_str().unwrap()))
            .collect();
        let multi = file_root(3, &leaves).unwrap();
        assert_eq!(
            hex::encode(multi),
            expected["root_multi_hex"].as_str().unwrap()
        );
        assert!(file_root(2, &leaves).is_err());
    }

    #[test]
    fn attempt_key_and_nonce_vectors_match() {
        let vectors: serde_json::Value =
            serde_json::from_str(&fixture("crypto-vectors.json")).unwrap();
        let room_key =
            RoomKey::from_bytes(hex_to_32(vectors["inputs"]["room_key"].as_str().unwrap()));
        let transfer_id: TransferId = vectors["inputs"]["transfer_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let attempt_id: AttemptId = vectors["inputs"]["attempt_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let key = attempt_key(&room_key, &transfer_id, &attempt_id);
        assert_eq!(
            hex::encode(key),
            vectors["expected"]["attempt_key_hex"].as_str().unwrap()
        );
        assert_eq!(
            hex::encode(frame_nonce(7)),
            vectors["expected"]["nonce_seq7_hex"].as_str().unwrap()
        );
        // Nonce binds the sequence and nothing else.
        assert_ne!(frame_nonce(7), frame_nonce(8));
    }

    #[test]
    fn encrypted_frames_round_trip_for_every_type() {
        let vectors: serde_json::Value =
            serde_json::from_str(&fixture("crypto-vectors.json")).unwrap();
        let key = hex_to_32(vectors["expected"]["attempt_key_hex"].as_str().unwrap());
        let plaintext = hex::decode(vectors["inputs"]["plaintext_hex"].as_str().unwrap()).unwrap();
        for (ftype, body) in [
            (FrameType::Data, plaintext.clone()),
            (
                FrameType::Final,
                u64::try_from(plaintext.len())
                    .unwrap()
                    .to_be_bytes()
                    .to_vec(),
            ),
        ] {
            let msg = seal_frame(&key, 7, ftype, &body).unwrap();
            // Byte-offset table (see protocol doc §6): magic 0..4, version
            // 4..6, type 6, flags 7, seq 8..12, body_len 12..16.
            assert_eq!(&msg[..4], &FRAME_MAGIC.to_be_bytes());
            assert_eq!(&msg[4..6], &1u16.to_be_bytes());
            assert_eq!(msg[6], ftype as u8);
            assert_eq!(msg[7], 0);
            assert_eq!(&msg[8..12], &7u32.to_be_bytes());
            let decoded = open_frame(&key, &msg, 0).unwrap();
            assert_eq!(decoded.ftype, ftype);
            assert_eq!(decoded.seq, 7);
            assert_eq!(decoded.plaintext, body);
            // Fixture pins the exact wire bytes for this (key, seq, body).
            let name = if ftype == FrameType::Data {
                "frame_data_seq7_hex"
            } else {
                "frame_final_seq7_hex"
            };
            assert_eq!(
                hex::encode(&msg),
                vectors["expected"][name].as_str().unwrap()
            );
        }
    }

    #[test]
    fn wrong_key_modified_aad_reserved_bits_and_reused_sequence_are_rejected() {
        let key = [9u8; 32];
        let msg = seal_frame(&key, 3, FrameType::Data, b"payload").unwrap();
        let mut wrong = key;
        wrong[0] ^= 1;
        assert!(open_frame(&wrong, &msg, 0).is_err());
        // Modified AAD (sequence byte inside the authenticated header).
        let mut tampered = msg.clone();
        tampered[8] ^= 1;
        assert!(open_frame(&key, &tampered, 0).is_err());
        // Reserved flag set.
        let mut flagged = msg.clone();
        flagged[7] = 1;
        assert!(open_frame(&key, &flagged, 0).is_err());
        // Reused sequence below the caller's window.
        assert!(open_frame(&key, &msg, 4).is_err());
        assert!(open_frame(&key, &msg, 3).is_ok());
    }

    #[test]
    fn decoder_rejects_oversized_fragment_integer_overflow_unknown_type_and_trailing_bytes() {
        let key = [4u8; 32];
        assert!(seal_frame(&key, 0, FrameType::Data, &[]).is_err());
        assert!(seal_frame(
            &key,
            0,
            FrameType::Data,
            &vec![0u8; FRAME_MAX_PLAINTEXT + 1]
        )
        .is_err());
        assert!(seal_frame(&key, 0, FrameType::Final, b"1234567").is_err());
        let mut msg = seal_frame(&key, 0, FrameType::Data, b"hi").unwrap();
        // Unknown frame type.
        msg[6] = 9;
        assert!(open_frame(&key, &msg, 0).is_err());
        let mut msg = seal_frame(&key, 0, FrameType::Data, b"hi").unwrap();
        // body_len lies upward (integer mismatch, never trusted for sizing).
        let over = u32::from_be_bytes(msg[12..16].try_into().unwrap()) + 16;
        msg[12..16].copy_from_slice(&over.to_be_bytes());
        assert!(open_frame(&key, &msg, 0).is_err());
        let mut msg = seal_frame(&key, 0, FrameType::Data, b"hi").unwrap();
        // Trailing bytes past the declared body.
        msg.extend_from_slice(&[0u8; 4]);
        assert!(open_frame(&key, &msg, 0).is_err());
        // body_len at u32::MAX fails on the cap before any allocation.
        let mut msg = seal_frame(&key, 0, FrameType::Data, b"hi").unwrap();
        msg[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open_frame(&key, &msg, 0).is_err());
    }

    #[test]
    fn frame_decoder_caps_allocations_before_decoding_attacker_lengths() {
        let key = [7u8; 32];
        // Deterministic mutation sweep: every single-byte corruption of a
        // valid frame, plus every truncation, must error without panicking.
        let msg = seal_frame(&key, 5, FrameType::Data, b"sweep me").unwrap();
        for i in 0..msg.len() {
            for delta in [1u8, 0x80, 0xff] {
                let mut corrupt = msg.clone();
                corrupt[i] ^= delta;
                let _ = open_frame(&key, &corrupt, 0);
            }
        }
        for cut in 0..msg.len() {
            assert!(open_frame(&key, &msg[..cut], 0).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn control_fixture_round_trips_without_unknown_fields() {
        let raw = fixture("control-messages.json");
        let messages: Vec<serde_json::Value> = serde_json::from_str(&raw).unwrap();
        assert!(messages.len() >= 38, "fixture covers every control type");
        for entry in &messages {
            let direction = entry["direction"].as_str().unwrap();
            let json = entry["json"].as_str().unwrap();
            if direction == "relay" {
                let attach = parse_relay_attach(json).unwrap();
                assert_eq!(entry["name"].as_str().unwrap(), "relay.attach");
                let _ = attach;
                continue;
            }
            let env = if direction == "client" {
                parse_client_envelope(json).unwrap()
            } else {
                parse_server_envelope(json).unwrap()
            };
            // Canonical re-encode parses again: round-trip is byte-stable.
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            let recanonical = canonical_json(&value).unwrap();
            let env2 = if direction == "client" {
                parse_client_envelope(&recanonical).unwrap()
            } else {
                parse_server_envelope(&recanonical).unwrap()
            };
            assert_eq!(env, env2);
        }
        // Unknown top-level fields and unknown types are rejected.
        assert!(parse_client_envelope(r#"{"v":1,"type":"ping","body":{},"extra":1}"#).is_err());
        assert!(parse_client_envelope(r#"{"v":1,"type":"nope","body":{}}"#).is_err());
        assert!(parse_client_envelope(r#"{"v":1,"type":"transfer.request","body":{}}"#).is_err());
        assert!(parse_server_envelope(r#"{"v":2,"type":"pong","body":{}}"#).is_err());
    }

    /// T-WEB-E2EE-FIXTURE: the checked-in JS vector runner must produce
    /// byte-identical canonical manifest, roots, keys, nonces, ciphertext and
    /// tags to this (ring-based) implementation. Needs Node >= 20.
    #[test]
    fn e2ee_fixture_matches_js_implementation() {
        let vectors_path = format!(
            "{}/tests/fixtures/web_transfer/v1/crypto-vectors.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let runner = format!(
            "{}/web/transfer/tests/unit/vectors.mjs",
            env!("CARGO_MANIFEST_DIR")
        );
        let output = std::process::Command::new("node")
            .arg(&runner)
            .arg(&vectors_path)
            .output()
            .expect("T-WEB-E2EE-FIXTURE needs Node >= 20 on PATH");
        assert!(
            output.status.success(),
            "JS vector runner failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let js: serde_json::Value =
            serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
        let vectors: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&vectors_path).unwrap()).unwrap();
        let inputs = &vectors["inputs"];
        let room_key = RoomKey::from_bytes(hex_to_32(inputs["room_key"].as_str().unwrap()));
        let room_id: RoomId = inputs["room_id"].as_str().unwrap().parse().unwrap();
        let transfer_id: TransferId = inputs["transfer_id"].as_str().unwrap().parse().unwrap();
        let attempt_id: AttemptId = inputs["attempt_id"].as_str().unwrap().parse().unwrap();
        let seq = inputs["seq"].as_u64().unwrap() as u32;
        let plaintext = hex::decode(inputs["plaintext_hex"].as_str().unwrap()).unwrap();

        let manifest_raw = fixture("manifest.json");
        let manifest_raw_value: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
        let manifest = parse_manifest(&manifest_raw_value).unwrap();
        let canonical = canonical_json(&manifest_value(&manifest)).unwrap();
        assert_eq!(js["canonical_manifest_hex"], hex::encode(&canonical));
        let mkey = manifest_key(&room_key, &room_id);
        assert_eq!(js["manifest_key_hex"], hex::encode(mkey));
        let mac = manifest_mac(&mkey, canonical.as_bytes());
        assert_eq!(js["manifest_mac_hex"], hex::encode(mac));
        let akey = attempt_key(&room_key, &transfer_id, &attempt_id);
        assert_eq!(js["attempt_key_hex"], hex::encode(akey));
        assert_eq!(js["nonce_hex"], hex::encode(frame_nonce(seq)));
        let data = seal_frame(&akey, seq, FrameType::Data, &plaintext).unwrap();
        assert_eq!(js["frame_data_hex"], hex::encode(&data));
        let total = u64::try_from(plaintext.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        let final_msg = seal_frame(&akey, seq, FrameType::Final, &total).unwrap();
        assert_eq!(js["frame_final_hex"], hex::encode(&final_msg));
        let single_leaf = sha256(&plaintext);
        assert_eq!(
            js["root_single_hex"],
            hex::encode(file_root(1, &[single_leaf]).unwrap())
        );
    }

    fn hex_to_32(s: &str) -> [u8; 32] {
        hex::decode(s).unwrap().try_into().unwrap()
    }
}
