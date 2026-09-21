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
    AttemptId, OfferId, PeerId, RelayTicket, RoomId, RoomKey, TransferId, WebTransferLimits,
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
    // Phase 4 signaling: the server FORWARDS these three verbatim-shaped to
    // the counterpart of the current attempt, so the same names travel in
    // both directions. It never stores, parses or logs what they carry.
    "rtc.offer",
    "rtc.answer",
    "rtc.ice",
    // The counterpart's notice that the direct attempt is over, carrying a
    // FIXED reason code (never the peer's own string) and bounded ranges.
    "transfer.direct_failed",
    // The recipient's verified-byte report, FORWARDED to the source with the
    // path the server itself committed. The source has no other way to learn
    // what the far end verified, and no way at all to read the path off its
    // own socket (a relay leg and a DataChannel both just carry bytes).
    "transfer.progress",
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
    // Signalling about an attempt the transfer has moved past. A code missing
    // from this list is rewritten to `INTERNAL` by `error_envelope`, which is
    // the guard working as intended and is exactly how the omission was
    // caught: the client read ten `INTERNAL`s for `rtc.ice` while the server
    // had built no internal error at all.
    "STALE_ATTEMPT",
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

/// Parses untrusted JSON in ONE pass, refusing an object that repeats a key.
///
/// `serde_json` keeps the LAST value for a duplicate key, silently. On a
/// control wire that turns one message into two readings of itself: the
/// browser's own `JSON.parse` also keeps the last, but a proxy, a log or a
/// future parser need not, and every check the server performed would then
/// have applied to a value the peer never meant. The protocol has no use for
/// a repeated key, so the honest answer is to refuse the message rather than
/// to pick a winner.
///
/// The detection has to happen WHILE parsing: once `serde_json` has built its
/// map the duplicate is already gone, so a check on the finished value always
/// passes. Building the value here rather than parsing twice keeps the cost
/// of the check at zero — a 320 KiB control message is parsed once, exactly
/// as before.
pub fn parse_json_no_duplicate_keys(raw: &str, what: &str) -> Result<serde_json::Value> {
    use serde::de::DeserializeSeed;

    let mut de = serde_json::Deserializer::from_str(raw);
    let value = NoDupes(what)
        .deserialize(&mut de)
        .map_err(|e| anyhow::anyhow!("{what} is not JSON: {e}"))?;
    de.end()
        .map_err(|e| anyhow::anyhow!("{what} has trailing content: {e}"))?;
    Ok(value)
}

/// Deserialization seed that builds a `serde_json::Value` and fails on the
/// first object key seen twice at the same level. Recursive because a
/// manifest is an object of arrays of objects, and the ENTRY is where a
/// repeat would be useful to an attacker (two `path`s, two `size`s).
struct NoDupes<'a>(&'a str);

impl<'de> serde::de::DeserializeSeed<'de> for NoDupes<'_> {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for NoDupes<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "any JSON value")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = map.next_value_seed(NoDupes(self.0))?;
            if out.insert(key.clone(), value).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "{} repeats the key {key:?}",
                    self.0
                )));
            }
        }
        Ok(serde_json::Value::Object(out))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(item) = seq.next_element_seed(NoDupes(self.0))? {
            out.push(item);
        }
        Ok(serde_json::Value::Array(out))
    }

    fn visit_bool<E>(self, v: bool) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::from(v))
    }
    fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::from(v))
    }
    fn visit_f64<E>(self, v: f64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::from(v))
    }
    fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::String(v.to_string()))
    }
    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }
    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        d.deserialize_any(self)
    }
}

/// Shared envelope checks: size cap, object shape, exact top-level fields,
/// version, known type and requestId presence rules.
fn parse_envelope(raw: &str, known: &[&str], side: &str) -> Result<ParsedEnvelope> {
    if raw.len() > WEB_TRANSFER_MAX_CONTROL_BYTES {
        bail!("{side} control message exceeds 320 KiB");
    }
    let value = parse_json_no_duplicate_keys(raw, &format!("{side} control message"))?;
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

/// Validates one server → client control message. `ack` echoes the mutation's
/// `requestId`; `error` echoes it when the offending message carried one —
/// version/rate errors on request-free messages (`hello`/`ping`) travel
/// without it rather than closing a healthy session.
pub fn parse_server_envelope(raw: &str) -> Result<ParsedEnvelope> {
    let env = parse_envelope(raw, SERVER_TYPES, "server")?;
    if env.typ == "ack" && env.request_id.is_none() {
        bail!("server ack must echo requestId");
    }
    if env.typ != "ack" && env.typ != "error" && env.request_id.is_some() {
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

/// Builds a canonical server `error` WITHOUT `requestId`, for failures on
/// request-free messages (`hello`/`ping` version or rate errors). The peer
/// stays connected; the parser accepts this shape (see
/// [`parse_server_envelope`]).
pub fn error_envelope_anon(code: &str, message: Option<&str>) -> String {
    let code = if is_known_error_code(code) {
        code
    } else {
        "INTERNAL"
    };
    let mut body = BTreeMap::new();
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
    server_envelope("error", None, body)
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

// --- Phase 2.2: control-session bodies (strict keys, additive reads) ---
//
// Envelope-level validation (`parse_*_envelope`) already pins `{v,type,
// requestId?,body}`. The helpers below pin each 2.2 body shape: unknown body
// fields are rejected; unknown FUTURE body fields would be too — the 2.4/2.5
// browser only ever sends what its phase documents.

/// Rejects unknown keys in a control body (stable `INVALID_MESSAGE` input).
fn check_body_keys<'a>(
    body: &'a serde_json::Value,
    typ: &str,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    let obj = body
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("{typ} body must be an object"))?;
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!("{typ} body has unknown field {key:?}");
        }
    }
    Ok(obj)
}

/// Validated `hello` body: `{memberToken, displayName?}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelloBody {
    /// 64-char lowercase-hex member token (raw; hashed then dropped).
    pub member_token: String,
    /// Optional raw display name (normalized by the registry).
    pub display_name: Option<String>,
}

/// Parses a `hello` body with exact keys.
pub fn parse_hello_body(env: &ParsedEnvelope) -> Result<HelloBody> {
    if env.typ != "hello" {
        bail!("expected hello, got {:?}", env.typ);
    }
    let obj = check_body_keys(&env.body, "hello", &["memberToken", "displayName"])?;
    let member_token = obj
        .get("memberToken")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("hello needs string memberToken"))?
        .to_string();
    let display_name = match obj.get("displayName") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .ok_or_else(|| anyhow::anyhow!("hello displayName must be a string"))?
                .to_string(),
        ),
    };
    Ok(HelloBody {
        member_token,
        display_name,
    })
}

/// Parses a `ping` body, which must be exactly `{}`.
pub fn parse_ping_body(env: &ParsedEnvelope) -> Result<()> {
    if env.typ != "ping" {
        bail!("expected ping, got {:?}", env.typ);
    }
    check_body_keys(&env.body, "ping", &[])?;
    Ok(())
}

/// Parses a `peer.rename` body into `(requestId, raw displayName)`.
pub fn parse_rename_body(env: &ParsedEnvelope) -> Result<(RequestId, String)> {
    if env.typ != "peer.rename" {
        bail!("expected peer.rename, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("peer.rename requires requestId"))?;
    let obj = check_body_keys(&env.body, "peer.rename", &["displayName"])?;
    let display_name = obj
        .get("displayName")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("peer.rename needs string displayName"))?
        .to_string();
    Ok((request_id, display_name))
}

/// Largest accepted resume range count per request.
pub const MAX_RESUME_RANGES: usize = 4096;

/// Validated `transfer.request` body.
#[derive(Clone, Debug)]
pub struct TransferRequestBody {
    /// Offered object being pulled.
    pub offer_id: OfferId,
    /// Selected entry IDs: sorted, unique, decimal strings.
    pub entry_ids: Vec<String>,
    /// Claimed selection digest (verified against the stored manifest+MAC).
    pub selection_digest: [u8; 32],
    /// Transfer mode; only `raw` in this phase.
    pub mode: String,
    /// Optional resume descriptor (shape-checked here, verified at send).
    pub resume: Option<ResumeDescriptorBody>,
}

/// Validated resume descriptor: verified chunk ranges plus output length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeDescriptorBody {
    /// Sorted non-overlapping `[start, endExclusive]` chunk ranges.
    pub verified_ranges: Vec<(u64, u64)>,
    /// Expected output length in bytes.
    pub output_length: u64,
}

fn parse_entry_ids(obj: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<String>> {
    let raw = obj
        .get("entryIds")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("transfer.request needs entryIds array"))?;
    let mut ids = Vec::with_capacity(raw.len().min(64));
    for entry in raw {
        let id = entry
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("entry ID must be a string"))?;
        // Canonical decimal u32 IDs, mirroring the manifest rule.
        parse_decimal_u64("entry ID", id)?;
        ids.push(id.to_string());
    }
    let mut sorted = ids.clone();
    sorted.sort();
    if sorted != ids {
        bail!("entry IDs must be sorted");
    }
    sorted.dedup();
    if sorted.len() != ids.len() {
        bail!("entry IDs must be sorted and unique");
    }
    Ok(sorted)
}

/// Parses a `transfer.request` body into `(requestId, parts)`.
pub fn parse_transfer_request_body(
    env: &ParsedEnvelope,
) -> Result<(RequestId, TransferRequestBody)> {
    if env.typ != "transfer.request" {
        bail!("expected transfer.request, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.request requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.request",
        &["offerId", "entryIds", "selectionDigest", "mode", "resume"],
    )?;
    let offer_id = obj
        .get("offerId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.request needs string offerId"))?
        .parse::<OfferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let entry_ids = parse_entry_ids(obj)?;
    let digest_hex = obj
        .get("selectionDigest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.request needs string selectionDigest"))?;
    let selection_digest = crate::web_transfer::parse_hex_id::<32>("selectionDigest", digest_hex)
        .map_err(|e| anyhow::anyhow!(e))?;
    let mode = obj
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.request needs string mode"))?
        .to_string();
    let resume = match obj.get("resume") {
        None => None,
        Some(value) => Some(parse_resume_descriptor(value)?),
    };
    Ok((
        request_id,
        TransferRequestBody {
            offer_id,
            entry_ids,
            selection_digest,
            mode,
            resume,
        },
    ))
}

/// Parses a resume descriptor value.
pub fn parse_resume_descriptor(value: &serde_json::Value) -> Result<ResumeDescriptorBody> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("resume descriptor must be an object"))?;
    for key in obj.keys() {
        if key != "verifiedRanges" && key != "outputLength" {
            bail!("resume descriptor has unknown field {key:?}");
        }
    }
    let ranges_raw = obj
        .get("verifiedRanges")
        .ok_or_else(|| anyhow::anyhow!("resume descriptor needs verifiedRanges array"))?;
    let verified_ranges = parse_verified_ranges(ranges_raw)?;
    let output_length = obj
        .get("outputLength")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("resume descriptor needs integer outputLength"))?;
    Ok(ResumeDescriptorBody {
        verified_ranges,
        output_length,
    })
}

/// Parses a bounded, sorted, disjoint `[[start, endExclusive], ...]` array.
/// Shared by `transfer.request`'s resume descriptor and the direct-path
/// failure notice, so a range list has exactly one set of rules whichever
/// message carries it.
pub fn parse_verified_ranges(value: &serde_json::Value) -> Result<Vec<(u64, u64)>> {
    let ranges_raw = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("verifiedRanges must be an array"))?;
    if ranges_raw.len() > MAX_RESUME_RANGES {
        bail!("resume descriptor exceeds 4096 ranges");
    }
    let mut verified_ranges = Vec::with_capacity(ranges_raw.len().min(64));
    for range in ranges_raw {
        let pair = range
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("resume range must be a [start, end] pair"))?;
        if pair.len() != 2 {
            bail!("resume range must be a [start, end] pair");
        }
        let start = pair[0]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("resume range bounds must be integers"))?;
        let end = pair[1]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("resume range bounds must be integers"))?;
        if start >= end {
            bail!("resume range must satisfy start < end");
        }
        verified_ranges.push((start, end));
    }
    verified_ranges.sort();
    for window in verified_ranges.windows(2) {
        if window[0].1 > window[1].0 {
            bail!("resume ranges must not overlap");
        }
    }
    Ok(verified_ranges)
}

/// Selection digest: `SHA256(canonical {entryIds, manifestMac, mode,
/// offerId})`. The server recomputes it from the stored manifest and MAC;
/// any mismatch means the source changed since publish.
pub fn selection_digest(
    offer_id: &OfferId,
    manifest_mac: &[u8; 32],
    entry_ids: &[String],
    mode: &str,
) -> [u8; 32] {
    let mut body = BTreeMap::new();
    body.insert(
        "entryIds".to_string(),
        serde_json::Value::Array(
            entry_ids
                .iter()
                .map(|id| serde_json::Value::String(id.clone()))
                .collect(),
        ),
    );
    body.insert(
        "manifestMac".to_string(),
        serde_json::Value::String(hex::encode(manifest_mac)),
    );
    body.insert(
        "mode".to_string(),
        serde_json::Value::String(mode.to_string()),
    );
    body.insert(
        "offerId".to_string(),
        serde_json::Value::String(offer_id.to_string()),
    );
    let canonical = canonical_json(&serde_json::Value::Object(body.into_iter().collect()))
        .expect("digest input is canonical JSON");
    sha256(canonical.as_bytes())
}

/// Validated `transfer.source_ready` body: the attempt plus the selection
/// digest the source re-verified against its live `File` (freshness
/// attestation, not just an echo).
#[derive(Clone, Debug)]
pub struct SourceReadyBody {
    /// Transfer being readied.
    pub transfer_id: TransferId,
    /// Attempt the source answers for.
    pub attempt_id: AttemptId,
    /// Digest over the source's current selection (must match stored).
    pub selection_digest: [u8; 32],
}

/// Parses a `transfer.source_ready` body.
pub fn parse_source_ready_body(env: &ParsedEnvelope) -> Result<(RequestId, SourceReadyBody)> {
    if env.typ != "transfer.source_ready" {
        bail!("expected transfer.source_ready, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.source_ready requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.source_ready",
        &["transferId", "attemptId", "selectionDigest"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.source_ready needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.source_ready needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let digest_hex = obj
        .get("selectionDigest")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.source_ready needs string selectionDigest"))?;
    let selection_digest = crate::web_transfer::parse_hex_id::<32>("selectionDigest", digest_hex)
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok((
        request_id,
        SourceReadyBody {
            transfer_id,
            attempt_id,
            selection_digest,
        },
    ))
}

/// Parses a `transfer.reject` body into `(requestId, transferId)`.
pub fn parse_reject_body(env: &ParsedEnvelope) -> Result<(RequestId, TransferId)> {
    if env.typ != "transfer.reject" {
        bail!("expected transfer.reject, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.reject requires requestId"))?;
    let obj = check_body_keys(&env.body, "transfer.reject", &["transferId", "code"])?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.reject needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok((request_id, transfer_id))
}

// ---------------------------------------------------------------------------
// Phase 4 direct-path signaling. Every body below is FORWARD-ONLY: the server
// validates shape and bounds, hands the value to exactly one counterpart and
// keeps nothing. SDP and ICE candidates are never parsed, rewritten, stored
// or logged — they are opaque strings with a length cap and nothing else.

/// Validated `rtc.offer` / `rtc.answer` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcSdpBody {
    /// Transfer the description belongs to.
    pub transfer_id: TransferId,
    /// Attempt the description belongs to.
    pub attempt_id: AttemptId,
    /// Opaque session description, 1..=64 KiB of UTF-8. Never inspected.
    pub sdp: String,
    /// Which carrier of the attempt this description negotiates. Absent on
    /// the wire means `0`, so a single-carrier attempt sends exactly the
    /// message it sent before carriers existed.
    pub carrier: u8,
}

/// Parses `rtc.offer` (`typ == "rtc.offer"`) or `rtc.answer`.
pub fn parse_rtc_sdp_body(env: &ParsedEnvelope, typ: &str) -> Result<(RequestId, RtcSdpBody)> {
    debug_assert!(typ == "rtc.offer" || typ == "rtc.answer");
    if env.typ != typ {
        bail!("expected {typ}, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("{typ} requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        typ,
        &["transferId", "attemptId", "sdp", "carrier"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{typ} needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{typ} needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let sdp = obj
        .get("sdp")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{typ} needs string sdp"))?;
    // The bound is on BYTES, not chars: the cap exists to bound what the
    // server forwards, and a multi-byte char costs what it costs.
    if sdp.is_empty() || sdp.len() > crate::web_transfer::WEB_TRANSFER_MAX_SDP_BYTES {
        bail!("{typ} sdp is out of range");
    }
    Ok((
        request_id,
        RtcSdpBody {
            transfer_id,
            attempt_id,
            sdp: sdp.to_string(),
            carrier: parse_carrier(obj, typ)?,
        },
    ))
}

/// Reads the optional `carrier` index, bounded by
/// [`crate::web_transfer::WEB_TRANSFER_MAX_DIRECT_CARRIERS`]. Absent is `0`,
/// which is what an older peer — and any single-carrier attempt — sends.
fn parse_carrier(obj: &serde_json::Map<String, serde_json::Value>, typ: &str) -> Result<u8> {
    let Some(value) = obj.get("carrier") else {
        return Ok(0);
    };
    let index = value
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("{typ} carrier must be an unsigned integer"))?;
    if index >= crate::web_transfer::WEB_TRANSFER_MAX_DIRECT_CARRIERS as u64 {
        bail!("{typ} carrier is out of range");
    }
    Ok(index as u8)
}

/// Validated `rtc.ice` body. `candidate == None` IS the end-of-candidates
/// marker (the browser's own null candidate); it carries no other fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcIceBody {
    /// Transfer the candidate belongs to.
    pub transfer_id: TransferId,
    /// Attempt the candidate belongs to.
    pub attempt_id: AttemptId,
    /// Opaque candidate line, <= 4 KiB. `None` ends this side's gathering.
    pub candidate: Option<String>,
    /// Opaque media stream identification, <= 64 bytes.
    pub sdp_mid: Option<String>,
    /// Media description index, `u16` or absent.
    pub sdp_m_line_index: Option<u16>,
    /// Which carrier of the attempt this candidate belongs to; absent on the
    /// wire means `0`.
    pub carrier: u8,
}

impl RtcIceBody {
    /// Whether this message is the end-of-candidates marker.
    pub fn is_end_of_candidates(&self) -> bool {
        self.candidate.is_none()
    }
}

/// Parses an `rtc.ice` body.
pub fn parse_rtc_ice_body(env: &ParsedEnvelope) -> Result<(RequestId, RtcIceBody)> {
    if env.typ != "rtc.ice" {
        bail!("expected rtc.ice, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("rtc.ice requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "rtc.ice",
        &[
            "transferId",
            "attemptId",
            "candidate",
            "sdpMid",
            "sdpMLineIndex",
            "carrier",
        ],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("rtc.ice needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("rtc.ice needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    // An absent key, an explicit `null` and an empty string all mean the same
    // thing to a browser: this side is done gathering. Normalize to `None` so
    // the forwarded marker has exactly one shape on the wire.
    let candidate = match obj.get("candidate") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.is_empty() => None,
        Some(serde_json::Value::String(s)) => {
            if s.len() > crate::web_transfer::WEB_TRANSFER_MAX_ICE_CANDIDATE_BYTES {
                bail!("rtc.ice candidate is too long");
            }
            Some(s.clone())
        }
        Some(_) => bail!("rtc.ice candidate must be a string or null"),
    };
    let sdp_mid = match obj.get("sdpMid") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => {
            if s.len() > crate::web_transfer::WEB_TRANSFER_MAX_ICE_SDP_MID_BYTES {
                bail!("rtc.ice sdpMid is too long");
            }
            Some(s.clone())
        }
        Some(_) => bail!("rtc.ice sdpMid must be a string or null"),
    };
    let sdp_m_line_index = match obj.get("sdpMLineIndex") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let n = value
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("rtc.ice sdpMLineIndex must be an integer"))?;
            Some(
                u16::try_from(n)
                    .map_err(|_| anyhow::anyhow!("rtc.ice sdpMLineIndex does not fit u16"))?,
            )
        }
    };
    // The marker carries nothing else, and the media fields are DROPPED
    // rather than refused. This used to `bail!` on the argument that "a done
    // gathering that also names a media section is a shape nobody produces".
    // That is false, and it was measured: WebRTC's end-of-candidates
    // indication is PER m-section, so Firefox and WebKit deliver
    // `candidate: ""` together with `sdpMid: "0"` and `sdpMLineIndex: 0`,
    // while Chromium delivers a null event. In `T-WEB-DIRECT-FALLBACK` the
    // server answered `INVALID_MESSAGE` to **62** `rtc.ice` messages from
    // one Firefox peer, so the marker never reached the other side and the
    // peer could only learn that gathering had ended by timing out. A
    // browser sending what the specification tells it to send must never be
    // refused. Nothing is lost by dropping them: one carrier is one
    // PeerConnection with exactly one m-section, and WHICH carrier ended is
    // what `carrier` says — it is NOT a media field, which is why it
    // survives here.
    let (sdp_mid, sdp_m_line_index) = if candidate.is_none() {
        (None, None)
    } else {
        (sdp_mid, sdp_m_line_index)
    };
    Ok((
        request_id,
        RtcIceBody {
            transfer_id,
            attempt_id,
            candidate,
            sdp_mid,
            sdp_m_line_index,
            carrier: parse_carrier(obj, "rtc.ice")?,
        },
    ))
}

/// Parses `transfer.direct_ready` into `(requestId, transferId, attemptId)`.
/// `transfer.direct_ready`, parsed: who is asking, about which attempt, and
/// — on an UPGRADE, from the recipient only — what it has already verified.
pub struct DirectReadyBody {
    /// The transfer this ready is about.
    pub transfer_id: TransferId,
    /// The attempt whose channel the sender declares usable.
    pub attempt_id: AttemptId,
    /// Verified ranges, empty except on a recipient's upgrade ready.
    pub resume_ranges: Vec<(u64, u64)>,
}

/// Parses `transfer.direct_ready {transferId, attemptId, resumeRanges?}`.
pub fn parse_direct_ready_body(env: &ParsedEnvelope) -> Result<(RequestId, DirectReadyBody)> {
    if env.typ != "transfer.direct_ready" {
        bail!("expected transfer.direct_ready, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_ready requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.direct_ready",
        &["transferId", "attemptId", "resumeRanges"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_ready needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_ready needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    // Present only on an UPGRADE, and only from the recipient: it is the one
    // party that knows what is verified on disk, and by the time a probe is
    // ready the relay has carried bytes the server never counted. Absent is
    // the ordinary first negotiation, where the server's own `record.resume`
    // is already the whole truth.
    let resume_ranges = match obj.get("resumeRanges") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => parse_verified_ranges(value)?,
    };
    Ok((
        request_id,
        DirectReadyBody {
            transfer_id,
            attempt_id,
            resume_ranges,
        },
    ))
}

/// The complete set of reason codes a direct attempt can end with. A peer's
/// `reason` is MAPPED into this set and never forwarded verbatim: the string
/// is attacker-controlled and the counterpart only needs to know whether to
/// wait for a relay ticket, which every one of these implies.
pub const DIRECT_FAIL_REASONS: &[&str] = &[
    "ice-failed",
    "channel-closed",
    "send-error",
    "unsupported",
    "timeout",
    "protocol",
    "unknown",
];

/// Maps a peer-supplied reason onto [`DIRECT_FAIL_REASONS`]; anything else
/// (including a missing reason) becomes `"unknown"`.
pub fn direct_fail_reason(raw: Option<&str>) -> &'static str {
    match raw {
        Some(value) => DIRECT_FAIL_REASONS
            .iter()
            .copied()
            .find(|known| *known == value)
            .unwrap_or("unknown"),
        None => "unknown",
    }
}

/// Validated `transfer.direct_failed` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectFailedBody {
    /// Transfer whose direct attempt ended.
    pub transfer_id: TransferId,
    /// Attempt that ended (a stale one is ignored by the registry).
    pub attempt_id: AttemptId,
    /// Reason code, already mapped into [`DIRECT_FAIL_REASONS`].
    pub reason: &'static str,
    /// Verified `[start, endExclusive)` chunk ranges the recipient holds,
    /// sorted and disjoint. Bounded by the same parser the request uses.
    pub verified_ranges: Vec<(u64, u64)>,
}

/// Parses a `transfer.direct_failed` body.
pub fn parse_direct_failed_body(env: &ParsedEnvelope) -> Result<(RequestId, DirectFailedBody)> {
    if env.typ != "transfer.direct_failed" {
        bail!("expected transfer.direct_failed, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_failed requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.direct_failed",
        &["transferId", "attemptId", "reason", "resumeRanges"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_failed needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.direct_failed needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let reason = direct_fail_reason(obj.get("reason").and_then(serde_json::Value::as_str));
    let verified_ranges = match obj.get("resumeRanges") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(value) => parse_verified_ranges(value)?,
    };
    Ok((
        request_id,
        DirectFailedBody {
            transfer_id,
            attempt_id,
            reason,
            verified_ranges,
        },
    ))
}

/// Builds `transfer.direct_start {transferId, attemptId, attemptNumber, role,
/// iceServers, deadlineMs, carriers?, upgrade?}`. `role` is the SDP role this peer
/// plays and is fixed by the protocol: the recipient is always `offerer`, the
/// source always `answerer`.
///
/// `carriers` is emitted ONLY above 1, so a single-carrier attempt produces
/// the message byte for byte as it was before carriers existed — and a peer
/// that does not know the field reads one carrier, which is what it can do.
/// Everything one `transfer.direct_start` says, so the builder takes ONE
/// argument per idea instead of a positional list nobody can read.
pub struct DirectStart<'a> {
    /// The transfer being opened.
    pub transfer_id: TransferId,
    /// The attempt this negotiation belongs to.
    pub attempt_id: AttemptId,
    /// The number the transfer is on, or moving to on an upgrade.
    pub attempt_number: u64,
    /// The SDP role, fixed by the protocol.
    pub role: &'a str,
    /// ICE servers, as configured.
    pub ice_servers: &'a [String],
    /// How long the peers have to finish negotiating.
    pub deadline_ms: u64,
    /// Carrier CEILING for this attempt.
    pub carriers: u8,
    /// This negotiation runs BESIDE a relay that is still carrying.
    pub upgrade: bool,
}

/// Builds one peer's `transfer.direct_start` from [`DirectStart`].
pub fn transfer_direct_start_envelope(start: &DirectStart<'_>) -> String {
    let DirectStart {
        transfer_id,
        attempt_id,
        attempt_number,
        role,
        ice_servers,
        deadline_ms,
        carriers,
        upgrade,
    } = *start;
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    body.insert(
        "attemptNumber".to_string(),
        serde_json::Value::from(attempt_number),
    );
    body.insert(
        "role".to_string(),
        serde_json::Value::String(role.to_string()),
    );
    body.insert(
        "iceServers".to_string(),
        serde_json::Value::Array(
            ice_servers
                .iter()
                .map(|url| serde_json::Value::String(url.clone()))
                .collect(),
        ),
    );
    body.insert(
        "deadlineMs".to_string(),
        serde_json::Value::from(deadline_ms),
    );
    if carriers > 1 {
        body.insert("carriers".to_string(), serde_json::Value::from(carriers));
    }
    // Emitted ONLY on a probe, so an ordinary first negotiation is the
    // message byte for byte as it was before upgrades existed. It tells the
    // peer the one thing it cannot work out for itself: that something is
    // still carrying, and must keep carrying until the commit arrives.
    if upgrade {
        body.insert("upgrade".to_string(), serde_json::Value::Bool(true));
    }
    server_envelope("transfer.direct_start", None, body)
}

/// Builds the forwarded `rtc.offer` / `rtc.answer`. The SDP travels through
/// unchanged and unread; only the envelope is rebuilt, so the peer's own
/// `requestId` never leaks to the counterpart.
pub fn rtc_sdp_envelope(typ: &str, body: &RtcSdpBody) -> String {
    debug_assert!(typ == "rtc.offer" || typ == "rtc.answer");
    let mut out = BTreeMap::new();
    out.insert(
        "transferId".to_string(),
        serde_json::Value::String(body.transfer_id.to_string()),
    );
    out.insert(
        "attemptId".to_string(),
        serde_json::Value::String(body.attempt_id.to_string()),
    );
    out.insert(
        "sdp".to_string(),
        serde_json::Value::String(body.sdp.clone()),
    );
    insert_carrier(&mut out, body.carrier);
    server_envelope(typ, None, out)
}

/// Builds the forwarded `rtc.ice`. The end-of-candidates marker is a `null`
/// candidate and nothing else.
pub fn rtc_ice_envelope(body: &RtcIceBody) -> String {
    let mut out = BTreeMap::new();
    out.insert(
        "transferId".to_string(),
        serde_json::Value::String(body.transfer_id.to_string()),
    );
    out.insert(
        "attemptId".to_string(),
        serde_json::Value::String(body.attempt_id.to_string()),
    );
    out.insert(
        "candidate".to_string(),
        match &body.candidate {
            Some(value) => serde_json::Value::String(value.clone()),
            None => serde_json::Value::Null,
        },
    );
    if let Some(mid) = &body.sdp_mid {
        out.insert("sdpMid".to_string(), serde_json::Value::String(mid.clone()));
    }
    if let Some(index) = body.sdp_m_line_index {
        out.insert("sdpMLineIndex".to_string(), serde_json::Value::from(index));
    }
    insert_carrier(&mut out, body.carrier);
    server_envelope("rtc.ice", None, out)
}

/// Writes `carrier` only when it is not 0. Carrier 0 is the only carrier a
/// single-carrier attempt has, so omitting it is what keeps that attempt's
/// forwarded signalling identical to the wire before carriers existed.
fn insert_carrier(out: &mut BTreeMap<String, serde_json::Value>, carrier: u8) {
    if carrier != 0 {
        out.insert("carrier".to_string(), serde_json::Value::from(carrier));
    }
}

/// Builds the forwarded `transfer.direct_failed {transferId, attemptId,
/// reason, resumeRanges?}`. `reason` is always one of
/// [`DIRECT_FAIL_REASONS`]; `resumeRanges` is omitted when empty so a failure
/// with nothing to resume is the smallest envelope there is.
pub fn transfer_direct_failed_envelope(
    transfer_id: TransferId,
    attempt_id: AttemptId,
    reason: &str,
    resume_ranges: &[(u64, u64)],
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    body.insert(
        "reason".to_string(),
        serde_json::Value::String(direct_fail_reason(Some(reason)).to_string()),
    );
    if !resume_ranges.is_empty() {
        body.insert(
            "resumeRanges".to_string(),
            serde_json::Value::Array(
                resume_ranges
                    .iter()
                    .map(|(start, end)| {
                        serde_json::Value::Array(vec![
                            serde_json::Value::from(*start),
                            serde_json::Value::from(*end),
                        ])
                    })
                    .collect(),
            ),
        );
    }
    server_envelope("transfer.direct_failed", None, body)
}

/// Parses a `transfer.cancel` body into `(requestId, transferId)`.
pub fn parse_cancel_body(env: &ParsedEnvelope) -> Result<(RequestId, TransferId)> {
    if env.typ != "transfer.cancel" {
        bail!("expected transfer.cancel, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.cancel requires requestId"))?;
    let obj = check_body_keys(&env.body, "transfer.cancel", &["transferId", "reason"])?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.cancel needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok((request_id, transfer_id))
}

/// Validated `transfer.complete` body.
#[derive(Clone, Debug)]
pub struct CompleteBody {
    /// Finished transfer.
    pub transfer_id: TransferId,
    /// Finished attempt.
    pub attempt_id: AttemptId,
    /// Final content root the recipient verified.
    pub root: [u8; 32],
}

/// Parses a `transfer.complete` body.
pub fn parse_complete_body(env: &ParsedEnvelope) -> Result<(RequestId, CompleteBody)> {
    if env.typ != "transfer.complete" {
        bail!("expected transfer.complete, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.complete requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.complete",
        &["transferId", "attemptId", "root"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.complete needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.complete needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let root_hex = obj
        .get("root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.complete needs string root"))?;
    let root = crate::web_transfer::parse_hex_id::<32>("root", root_hex)
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok((
        request_id,
        CompleteBody {
            transfer_id,
            attempt_id,
            root,
        },
    ))
}

/// Builds `transfer.incoming`; `entryId` tells the source which file a raw
/// request selected from a multi-entry offer.
pub fn transfer_incoming_envelope(
    transfer_id: TransferId,
    offer_id: OfferId,
    from_peer: PeerId,
    attempt_id: AttemptId,
    mode: &str,
    entry_id: Option<u32>,
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "offerId".to_string(),
        serde_json::Value::String(offer_id.to_string()),
    );
    body.insert(
        "fromPeerId".to_string(),
        serde_json::Value::String(from_peer.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    // The SOURCE has to know which selection it is about to serve: it
    // recomputes the selection digest itself (that is what makes a forged
    // request fail at the source and not only at the server), and the digest
    // covers the mode. Additive field, appended last on a
    // server-to-client message the browser reads by key.
    body.insert(
        "mode".to_string(),
        serde_json::Value::String(mode.to_string()),
    );
    if let Some(entry_id) = entry_id {
        body.insert(
            "entryId".to_string(),
            serde_json::Value::String(entry_id.to_string()),
        );
    }
    server_envelope("transfer.incoming", None, body)
}

/// Builds `transfer.relay_ticket {transferId, attemptId, ticket}` carrying
/// only the recipient's own ticket.
pub fn transfer_relay_ticket_envelope(
    transfer_id: TransferId,
    attempt_id: AttemptId,
    ticket_hex: &str,
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    body.insert(
        "ticket".to_string(),
        serde_json::Value::String(ticket_hex.to_string()),
    );
    server_envelope("transfer.relay_ticket", None, body)
}

/// Validated `transfer.progress` body.
#[derive(Clone, Copy, Debug)]
pub struct ProgressBody {
    /// Transfer being reported on.
    pub transfer_id: TransferId,
    /// Attempt the report belongs to.
    pub attempt_id: AttemptId,
    /// Plaintext bytes the recipient has VERIFIED, resumed chunks included.
    pub received_bytes: u64,
}

/// Parses a `transfer.progress` body. `receivedBytes` travels as a decimal
/// string (the manifest's convention for every 64-bit quantity); a plain
/// number is accepted too, so a hand-written client is not tripped by it.
pub fn parse_progress_body(env: &ParsedEnvelope) -> Result<(RequestId, ProgressBody)> {
    if env.typ != "transfer.progress" {
        bail!("expected transfer.progress, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("transfer.progress requires requestId"))?;
    let obj = check_body_keys(
        &env.body,
        "transfer.progress",
        &["transferId", "attemptId", "receivedBytes"],
    )?;
    let transfer_id = obj
        .get("transferId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.progress needs string transferId"))?
        .parse::<TransferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let attempt_id = obj
        .get("attemptId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("transfer.progress needs string attemptId"))?
        .parse::<AttemptId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let received_bytes = match obj.get("receivedBytes") {
        Some(serde_json::Value::String(text)) => text
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("transfer.progress receivedBytes is not a u64"))?,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("transfer.progress receivedBytes is not a u64"))?,
        None => bail!("transfer.progress needs receivedBytes"),
    };
    Ok((
        request_id,
        ProgressBody {
            transfer_id,
            attempt_id,
            received_bytes,
        },
    ))
}

/// Builds the FORWARDED `transfer.progress {transferId, attemptId,
/// receivedBytes, path}`. `path` is the server's OWN committed path, never
/// anything the reporting peer said: a peer may attest what it verified, and
/// nothing else.
pub fn transfer_progress_envelope(
    transfer_id: TransferId,
    attempt_id: AttemptId,
    received_bytes: u64,
    path: &str,
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    body.insert(
        "receivedBytes".to_string(),
        serde_json::Value::String(received_bytes.to_string()),
    );
    body.insert(
        "path".to_string(),
        serde_json::Value::String(path.to_string()),
    );
    server_envelope("transfer.progress", None, body)
}

/// Builds `transfer.path_commit {transferId, attemptId, path}` announcing the
/// transport both legs are now spliced through (`"relay"` in Phase 3).
///
/// `resume_ranges` carries the recipient's verified `[start, end)` chunk
/// ranges to the SOURCE, and is the only way the source can skip them: the
/// descriptor travels on `transfer.request`, which the source never sees.
/// The field is additive and omitted entirely when the recipient holds
/// nothing, so a full transfer's envelope is byte-identical to Phase 3.1's.
pub fn transfer_path_commit_envelope(
    transfer_id: TransferId,
    attempt_id: AttemptId,
    path: &str,
    resume_ranges: &[(u64, u64)],
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "attemptId".to_string(),
        serde_json::Value::String(attempt_id.to_string()),
    );
    body.insert(
        "path".to_string(),
        serde_json::Value::String(path.to_string()),
    );
    if !resume_ranges.is_empty() {
        body.insert(
            "resumeRanges".to_string(),
            serde_json::Value::Array(
                resume_ranges
                    .iter()
                    .map(|(start, end)| {
                        serde_json::Value::Array(vec![
                            serde_json::Value::from(*start),
                            serde_json::Value::from(*end),
                        ])
                    })
                    .collect(),
            ),
        );
    }
    server_envelope("transfer.path_commit", None, body)
}

/// Builds `transfer.cancelled {transferId, byPeerId}`.
pub fn transfer_cancelled_envelope(transfer_id: TransferId, by_peer: PeerId) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "byPeerId".to_string(),
        serde_json::Value::String(by_peer.to_string()),
    );
    server_envelope("transfer.cancelled", None, body)
}

/// Builds `transfer.completed {transferId, root}`.
pub fn transfer_completed_envelope(transfer_id: TransferId, root: &[u8; 32]) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "transferId".to_string(),
        serde_json::Value::String(transfer_id.to_string()),
    );
    body.insert(
        "root".to_string(),
        serde_json::Value::String(hex::encode(root)),
    );
    server_envelope("transfer.completed", None, body)
}

/// Validated `offer.publish` body: offer ID, raw manifest value and MAC.
/// The manifest itself is validated separately by [`parse_manifest`] so the
/// byte cap applies before struct decoding.
#[derive(Clone, Debug)]
pub struct PublishBody {
    /// Offer being published.
    pub offer_id: OfferId,
    /// Raw manifest value (checked for size, then parsed).
    pub manifest: serde_json::Value,
    /// 32-byte MAC (shape-checked only; the server holds no room key).
    pub mac: [u8; 32],
}

/// Rejects an oversized manifest before struct decoding: the compact form
/// must fit `WEB_TRANSFER_MAX_MANIFEST_BYTES`.
pub fn check_manifest_byte_cap(compact_len: usize) -> Result<()> {
    if compact_len > crate::web_transfer::WEB_TRANSFER_MAX_MANIFEST_BYTES {
        bail!("manifest exceeds 256 KiB");
    }
    Ok(())
}

/// Parses an `offer.publish` body into `(requestId, parts)`.
pub fn parse_publish_body(env: &ParsedEnvelope) -> Result<(RequestId, PublishBody)> {
    if env.typ != "offer.publish" {
        bail!("expected offer.publish, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("offer.publish requires requestId"))?;
    let obj = check_body_keys(&env.body, "offer.publish", &["offerId", "manifest", "mac"])?;
    let offer_id = obj
        .get("offerId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("offer.publish needs string offerId"))?
        .parse::<OfferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    let manifest = obj
        .get("manifest")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("offer.publish needs manifest"))?;
    if !manifest.is_object() {
        bail!("offer.publish manifest must be an object");
    }
    let mac_hex = obj
        .get("mac")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("offer.publish needs string mac"))?;
    let mac =
        crate::web_transfer::parse_hex_id::<32>("mac", mac_hex).map_err(|e| anyhow::anyhow!(e))?;
    Ok((
        request_id,
        PublishBody {
            offer_id,
            manifest,
            mac,
        },
    ))
}

/// Parses an `offer.withdraw` body into `(requestId, offerId)`.
pub fn parse_withdraw_body(env: &ParsedEnvelope) -> Result<(RequestId, OfferId)> {
    if env.typ != "offer.withdraw" {
        bail!("expected offer.withdraw, got {:?}", env.typ);
    }
    let request_id = env
        .request_id
        .ok_or_else(|| anyhow::anyhow!("offer.withdraw requires requestId"))?;
    let obj = check_body_keys(&env.body, "offer.withdraw", &["offerId"])?;
    let offer_id = obj
        .get("offerId")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("offer.withdraw needs string offerId"))?
        .parse::<OfferId>()
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok((request_id, offer_id))
}

/// Builds `welcome`: fixture-pinned `{peerId, roomId}` plus the additive
/// `{displayName, limits, iceServers, relayOnly}` the browser needs at join
/// (unknown JSON fields are ignored by older readers, so this stays
/// compatible).
///
/// `relayOnly` is advisory for the page — the server is what ENFORCES it, by
/// never opening a direct attempt — and exists so the interface can say which
/// path a transfer will take before one runs, instead of letting the user
/// infer a policy from an absence.
pub fn welcome_envelope(
    peer: PeerId,
    room: RoomId,
    display_name: &str,
    limits: &WebTransferLimits,
    ice_servers: &[String],
    relay_only: bool,
) -> String {
    let mut limits_map = BTreeMap::new();
    for (name, value) in [
        ("max_rooms", limits.max_rooms),
        ("max_peers_global", limits.max_peers_global),
        ("max_peers_per_room", limits.max_peers_per_room),
        ("max_offers_per_peer", limits.max_offers_per_peer),
        ("max_entries_per_offer", limits.max_entries_per_offer),
        ("max_offer_bytes", limits.max_offer_bytes),
        (
            "max_metadata_per_room_bytes",
            limits.max_metadata_per_room_bytes,
        ),
        ("max_metadata_total_bytes", limits.max_metadata_total_bytes),
        ("max_transfers_per_peer", limits.max_transfers_per_peer),
        ("max_relays_global", limits.max_relays_global),
        ("relay_rate_bytes_per_s", limits.relay_rate_bytes_per_s),
        ("owner_grace_secs", limits.owner_grace_secs),
    ] {
        limits_map.insert(
            name.to_string(),
            serde_json::Value::Number(serde_json::Number::from(value)),
        );
    }
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "roomId".to_string(),
        serde_json::Value::String(room.to_string()),
    );
    body.insert(
        "displayName".to_string(),
        serde_json::Value::String(display_name.to_string()),
    );
    body.insert(
        "limits".to_string(),
        serde_json::Value::Object(limits_map.into_iter().collect()),
    );
    body.insert(
        "iceServers".to_string(),
        serde_json::Value::Array(
            ice_servers
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
    );
    body.insert("relayOnly".to_string(), serde_json::Value::Bool(relay_only));
    server_envelope("welcome", None, body)
}

/// Builds one `pong` (`{}` body).
pub fn pong_envelope() -> String {
    server_envelope("pong", None, BTreeMap::new())
}

/// Builds `room_closed {reason}`.
pub fn room_closed_envelope(reason: &str) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "reason".to_string(),
        serde_json::Value::String(reason.to_string()),
    );
    server_envelope("room_closed", None, body)
}

/// Builds `snapshot.begin {revision}`.
pub fn snapshot_begin_envelope(revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("snapshot.begin", None, body)
}

/// Builds `snapshot.end {revision}`.
pub fn snapshot_end_envelope(revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("snapshot.end", None, body)
}

/// Builds one `snapshot.peer {peerId, displayName?, revision}` — one message
/// per peer, never an aggregate. `displayName` serializes absent when `None`
/// (fixture shape); `revision` frames the entry in its snapshot.
pub fn snapshot_peer_envelope(peer: PeerId, display_name: Option<&str>, revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    if let Some(name) = display_name {
        body.insert(
            "displayName".to_string(),
            serde_json::Value::String(name.to_string()),
        );
    }
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("snapshot.peer", None, body)
}

/// Builds the full initial snapshot: begin, one message per peer in ID
/// order, one message per offer in ID order, end.
pub fn snapshot_messages(
    revision: u64,
    peers: &[(PeerId, Option<String>)],
    offers: &[SnapshotOffer<'_>],
) -> Vec<String> {
    let mut out = Vec::with_capacity(peers.len() + offers.len() + 2);
    out.push(snapshot_begin_envelope(revision));
    for (peer, name) in peers {
        out.push(snapshot_peer_envelope(*peer, name.as_deref(), revision));
    }
    for offer in offers {
        out.push(snapshot_offer_envelope(
            offer.peer,
            offer.offer,
            offer.manifest,
            offer.mac_hex,
            revision,
        ));
    }
    out.push(snapshot_end_envelope(revision));
    out
}

/// Builds `peer.joined {peerId, displayName?, revision}`.
pub fn peer_joined_envelope(peer: PeerId, display_name: Option<&str>, revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    if let Some(name) = display_name {
        body.insert(
            "displayName".to_string(),
            serde_json::Value::String(name.to_string()),
        );
    }
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("peer.joined", None, body)
}

/// Builds `peer.renamed {peerId, displayName, revision}`.
pub fn peer_renamed_envelope(peer: PeerId, display_name: &str, revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "displayName".to_string(),
        serde_json::Value::String(display_name.to_string()),
    );
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("peer.renamed", None, body)
}

/// Builds `peer.left {peerId, revision}`.
pub fn peer_left_envelope(peer: PeerId, revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("peer.left", None, body)
}

/// One catalog entry for snapshot serialization: the caller's parsed views
/// over the retained canonical manifest bytes.
pub struct SnapshotOffer<'a> {
    /// Owning member.
    pub peer: PeerId,
    /// Published offer.
    pub offer: OfferId,
    /// Canonical manifest object.
    pub manifest: &'a serde_json::Value,
    /// Lowercase hex MAC.
    pub mac_hex: &'a str,
}

/// Builds one `snapshot.offer {peerId, offerId, manifest, mac, revision}` —
/// one message per offer, never an aggregate.
pub fn snapshot_offer_envelope(
    peer: PeerId,
    offer: OfferId,
    manifest: &serde_json::Value,
    mac_hex: &str,
    revision: u64,
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "offerId".to_string(),
        serde_json::Value::String(offer.to_string()),
    );
    body.insert("manifest".to_string(), manifest.clone());
    body.insert(
        "mac".to_string(),
        serde_json::Value::String(mac_hex.to_string()),
    );
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("snapshot.offer", None, body)
}

/// Builds `offer.added {peerId, offerId, manifest, mac, revision}`.
pub fn offer_added_envelope(
    peer: PeerId,
    offer: OfferId,
    manifest: &serde_json::Value,
    mac_hex: &str,
    revision: u64,
) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "offerId".to_string(),
        serde_json::Value::String(offer.to_string()),
    );
    body.insert("manifest".to_string(), manifest.clone());
    body.insert(
        "mac".to_string(),
        serde_json::Value::String(mac_hex.to_string()),
    );
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("offer.added", None, body)
}

/// Builds `offer.removed {peerId, offerId, revision}` — IDs only, never
/// manifest contents.
pub fn offer_removed_envelope(peer: PeerId, offer: OfferId, revision: u64) -> String {
    let mut body = BTreeMap::new();
    body.insert(
        "peerId".to_string(),
        serde_json::Value::String(peer.to_string()),
    );
    body.insert(
        "offerId".to_string(),
        serde_json::Value::String(offer.to_string()),
    );
    body.insert(
        "revision".to_string(),
        serde_json::Value::Number(serde_json::Number::from(revision)),
    );
    server_envelope("offer.removed", None, body)
}

/// Maps one room broadcast event to its control message. `RoomClosed` maps
/// to `None`: the actor sends `room_closed` itself and then closes.
pub fn room_event_message(event: &crate::web_transfer::RoomEvent) -> Option<String> {
    use crate::web_transfer::RoomEvent;
    match event {
        RoomEvent::RoomClosed { .. } => None,
        RoomEvent::PeerJoined {
            peer,
            display_name,
            revision,
        } => Some(peer_joined_envelope(
            *peer,
            display_name.as_deref(),
            *revision,
        )),
        RoomEvent::PeerRenamed {
            peer,
            display_name,
            revision,
        } => Some(peer_renamed_envelope(*peer, display_name, *revision)),
        RoomEvent::PeerLeft { peer, revision } => Some(peer_left_envelope(*peer, *revision)),
        RoomEvent::OfferAdded {
            peer,
            offer,
            manifest,
            mac,
            revision,
        } => {
            let value: serde_json::Value = serde_json::from_slice(manifest).ok()?;
            Some(offer_added_envelope(
                *peer,
                *offer,
                &value,
                &hex::encode(mac),
                *revision,
            ))
        }
        RoomEvent::OfferRemoved {
            peer,
            offer,
            revision,
        } => Some(offer_removed_envelope(*peer, *offer, *revision)),
    }
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

/// One manifest entry: a file with its 1 MiB chunk hashes and verifiable
/// redundancy, or a directory (chunks empty, count zero, root null).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Zero-based position in the path-sorted entry array.
    pub id: u32,
    /// Relative NFC path.
    pub path: String,
    /// Logical file size in bytes (`"0"` for directories).
    pub size: u64,
    /// Last-modified Unix seconds.
    pub mtime: u64,
    /// One SHA-256 per 1 MiB chunk, in order (empty for directories).
    pub chunks: Vec<[u8; 32]>,
    /// Chunk count, always `chunks.len()`.
    pub chunk_count: u64,
    /// Rolling root over the chunks; `None` if and only if a directory.
    pub root: Option<[u8; 32]>,
}

/// Manifest offer mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestMode {
    /// Exactly one file entry.
    Single,
    /// File tree / ZIP source.
    Multi,
}

/// Offer selection kind: one file, flat files, or a folder tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestKind {
    /// Exactly one file (`single`, one file entry).
    File,
    /// Flat files (`multi`, files only).
    Files,
    /// Tree that may hold directories (`multi`).
    Folder,
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

impl ManifestKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Files => "files",
            Self::Folder => "folder",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "file" => Ok(Self::File),
            "files" => Ok(Self::Files),
            "folder" => Ok(Self::Folder),
            other => bail!("manifest kind must be file|files|folder, got {other:?}"),
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
    /// Human label shown in the catalog (1..128 chars, NFC).
    pub label: String,
    /// Selection kind.
    pub kind: ManifestKind,
    /// Logical chunk size; always `WEB_TRANSFER_CHUNK_BYTES`.
    pub chunk_size: u64,
    /// ISO-8601 UTC creation time (validated shape, stored verbatim).
    pub created_at: String,
    /// File entries in wire order (path-sorted, IDs 0-based sequential).
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

/// Validates a catalog label: NFC, trimmed, no control characters, 1..=128
/// Unicode scalar values.
pub fn validate_label(label: &str) -> Result<String> {
    use unicode_normalization::UnicodeNormalization;
    let normalized: String = label.nfc().collect();
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        bail!("manifest label must not be empty");
    }
    if trimmed.chars().any(|c| c.is_control()) {
        bail!("manifest label must not carry control characters");
    }
    if trimmed.chars().count() > 128 {
        bail!("manifest label exceeds 128 characters");
    }
    Ok(trimmed.to_string())
}

/// Validates an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS[.frac]Z`,
/// at most 32 bytes). The server never acts on the instant; the shape check
/// keeps the catalog bounded and unambiguous.
pub fn validate_created_at(s: &str) -> Result<()> {
    if s.is_empty() || s.len() > 32 || !s.is_ascii() {
        bail!("manifest createdAt must be ASCII within 32 bytes");
    }
    let inner = s
        .strip_suffix('Z')
        .ok_or_else(|| anyhow::anyhow!("manifest createdAt must look like 2026-09-14T21:00:00Z"))?;
    let (date, time) = inner
        .split_once('T')
        .ok_or_else(|| anyhow::anyhow!("manifest createdAt must look like 2026-09-14T21:00:00Z"))?;
    if date.len() != 10
        || date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || !date
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        bail!("manifest createdAt must look like 2026-09-14T21:00:00Z");
    }
    if time.len() < 8
        || time.as_bytes()[2] != b':'
        || time.as_bytes()[5] != b':'
        || !time.bytes().enumerate().all(|(i, b)| {
            i == 2 || i == 5 || b.is_ascii_digit() || (i > 7 && (b == b'.' || b.is_ascii_digit()))
        })
    {
        bail!("manifest createdAt must look like 2026-09-14T21:00:00Z");
    }
    let month: u32 = date[5..7].parse().unwrap_or(0);
    let day: u32 = date[8..10].parse().unwrap_or(0);
    let hour: u32 = time[0..2].parse().unwrap_or(99);
    let minute: u32 = time[3..5].parse().unwrap_or(99);
    let second: u32 = time[6..8].parse().unwrap_or(99);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        bail!("manifest createdAt carries an impossible date or time");
    }
    let rest = &time[8..];
    let frac_ok = rest.len() >= 2
        && rest.len() <= 10
        && rest.starts_with('.')
        && rest[1..].bytes().all(|b| b.is_ascii_digit());
    if !(rest.is_empty() || frac_ok) {
        bail!("manifest createdAt fractional seconds must look like .123");
    }
    Ok(())
}

/// Entry ID reserved for the ZIP payload itself.
///
/// A `mode: "zip"` transfer carries ONE synthetic entry — the archive — and it
/// needs an ID that no manifest entry can ever claim, so that a partial record,
/// a resume key or a progress report about the archive can never be confused
/// with one about a file. Manifest IDs are therefore `0..=0xffff_fffe` and this
/// value is refused on the way in, at the only door manifests come through.
pub const RESERVED_ZIP_ENTRY_ID: u32 = u32::MAX;

/// A validated `transfer.request` selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// One raw file entry, named by its manifest ID. Never a directory: a
    /// directory has no bytes to send.
    Raw(u32),
    /// The whole offer as one archive. The SET of selected IDs is exactly the
    /// manifest's, directories included; the archive writes them in manifest
    /// order, while the `entryIds` array on the wire keeps the canonical
    /// lexicographic order every request already uses.
    Zip,
}

/// Validates a selection against the offer's manifest — the only selection
/// rule the server enforces, because it is the only one it can check without
/// holding payload: `raw` is exactly one file, `zip` is exactly the whole
/// manifest.
pub fn validate_selection(
    mode: &str,
    entry_ids: &[String],
    manifest: &Manifest,
) -> Result<Selection> {
    fn entry_id(raw: &str) -> Result<u32> {
        let id = parse_decimal_u64("entry ID", raw)?;
        let id = u32::try_from(id).map_err(|_| anyhow::anyhow!("entry ID exceeds u32"))?;
        if id == RESERVED_ZIP_ENTRY_ID {
            bail!("entry ID {RESERVED_ZIP_ENTRY_ID} is reserved for the archive");
        }
        Ok(id)
    }
    match mode {
        "raw" => {
            if entry_ids.len() != 1 {
                bail!("raw selects exactly one entry");
            }
            let id = entry_id(&entry_ids[0])?;
            let entry = manifest
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("unknown entry ID"))?;
            if entry.root.is_none() {
                bail!("raw cannot select a directory entry");
            }
            Ok(Selection::Raw(id))
        }
        "zip" => {
            // Length + membership + no repeat is set equality, and it reads
            // the same whatever order the array arrived in.
            if entry_ids.len() != manifest.entries.len() {
                bail!("zip selects every manifest entry");
            }
            let mut seen = vec![false; manifest.entries.len()];
            for raw in entry_ids {
                let id = entry_id(raw)?;
                let position = manifest
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown entry ID"))?;
                if std::mem::replace(&mut seen[position], true) {
                    bail!("zip selects every manifest entry exactly once");
                }
            }
            Ok(Selection::Zip)
        }
        other => bail!("unknown transfer mode {other:?}"),
    }
}

/// Parses and validates a manifest value (unknown fields rejected).
/// `limits` bounds the entry count and the summed logical bytes with checked
/// arithmetic before any large allocation.
pub fn parse_manifest(
    value: &serde_json::Value,
    limits: &crate::web_transfer::WebTransferLimits,
) -> Result<Manifest> {
    use crate::web_transfer::WEB_TRANSFER_CHUNK_BYTES;
    let obj = exact_object(
        value,
        "manifest",
        &[
            "offer",
            "mode",
            "label",
            "kind",
            "chunkSize",
            "createdAt",
            "entries",
        ],
    )?;
    let offer = get_str(obj, "manifest", "offer")
        .and_then(|s| s.parse::<OfferId>().map_err(|e| anyhow::anyhow!(e)))?;
    let mode = ManifestMode::parse(get_str(obj, "manifest", "mode")?)?;
    let label = validate_label(get_str(obj, "manifest", "label")?)?;
    let kind = ManifestKind::parse(get_str(obj, "manifest", "kind")?)?;
    let chunk_size =
        parse_decimal_u64("manifest chunkSize", get_str(obj, "manifest", "chunkSize")?)?;
    if chunk_size != WEB_TRANSFER_CHUNK_BYTES as u64 {
        bail!("manifest chunkSize must be {}", WEB_TRANSFER_CHUNK_BYTES);
    }
    let created_at = get_str(obj, "manifest", "createdAt")?;
    validate_created_at(created_at)?;
    let entries_raw = obj
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("manifest needs entries array"))?;
    if entries_raw.is_empty() {
        bail!("manifest needs at least one entry");
    }
    if entries_raw.len() as u64 > limits.max_entries_per_offer {
        bail!("manifest exceeds the per-offer entry cap");
    }
    let mut entries = Vec::with_capacity(entries_raw.len().min(1024));
    let mut total: u64 = 0;
    let mut seen_paths: Vec<String> = Vec::with_capacity(entries_raw.len().min(1024));
    for (position, entry) in entries_raw.iter().enumerate() {
        let e = exact_object(
            entry,
            "manifest entry",
            &[
                "id",
                "path",
                "size",
                "mtime",
                "chunks",
                "chunkCount",
                "root",
            ],
        )?;
        let id = parse_decimal_u64("entry id", get_str(e, "manifest entry", "id")?)?;
        // Checked BEFORE the sequence rule so the reserved ID is refused as
        // itself: a manifest that claims it is not a mis-numbered manifest,
        // it is one trying to name the archive.
        if id == RESERVED_ZIP_ENTRY_ID as u64 {
            bail!("manifest entry ID {RESERVED_ZIP_ENTRY_ID} is reserved for the archive");
        }
        let want_id = u64::try_from(position).map_err(|_| anyhow::anyhow!("manifest too long"))?;
        if id != want_id {
            bail!("manifest entry IDs must be 0-based sequential");
        }
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
        let want = size.div_ceil(WEB_TRANSFER_CHUNK_BYTES as u64);
        if chunks.len() as u64 != want {
            bail!("entry #{position} needs {want} chunk hashes for {size} bytes");
        }
        let chunk_count = parse_decimal_u64(
            "entry chunkCount",
            get_str(e, "manifest entry", "chunkCount")?,
        )?;
        if chunk_count != want {
            bail!("entry #{position} chunkCount must equal its chunk hash count");
        }
        // Directories carry a null root with empty chunks; files carry the
        // rolling root recomputed here, so a lying root never enters state.
        let root = match e.get("root") {
            None | Some(serde_json::Value::Null) => {
                if !chunks.is_empty() || size != 0 {
                    bail!("entry #{position} with null root must be an empty directory");
                }
                None
            }
            Some(serde_json::Value::String(hex)) => {
                let bytes = crate::web_transfer::parse_hex_id::<32>("root", hex)
                    .map_err(|e| anyhow::anyhow!(e))?;
                let leaves: Vec<[u8; 32]> = chunks.clone();
                let expected = file_root(chunk_count, &leaves)?;
                if bytes != expected {
                    bail!("entry #{position} root does not match its chunks");
                }
                Some(bytes)
            }
            Some(_) => bail!("entry #{position} root must be hex or null"),
        };
        if root.is_none() && kind != ManifestKind::Folder {
            bail!("entry #{position} directory needs kind folder");
        }
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("manifest total overflows u64"))?;
        // Paths arrive sorted; a duplicate or an NFC+casefold collision is
        // not strictly greater than its predecessor. Casefolding here is
        // lowercase over NFC (exact for ASCII paths, documented
        // approximation elsewhere — full casefold tables are out of scope).
        let fold: String = {
            use unicode_normalization::UnicodeNormalization;
            path.nfc().collect::<String>().to_lowercase()
        };
        if let Some(prev) = seen_paths.last() {
            if fold <= *prev {
                bail!("manifest entries must be sorted with no path collisions");
            }
        }
        seen_paths.push(fold);
        entries.push(ManifestEntry {
            id: u32::try_from(id).map_err(|_| anyhow::anyhow!("manifest entry id exceeds u32"))?,
            path: path.to_string(),
            size,
            mtime,
            chunks,
            chunk_count,
            root,
        });
    }
    if total > limits.max_offer_bytes {
        bail!("manifest exceeds the per-offer byte cap");
    }
    if mode == ManifestMode::Single && entries.len() != 1 {
        bail!("single manifest needs exactly one entry");
    }
    match kind {
        ManifestKind::File => {
            if mode != ManifestMode::Single || entries.len() != 1 || entries[0].root.is_none() {
                bail!("kind file needs one single file entry");
            }
        }
        ManifestKind::Files => {
            if mode != ManifestMode::Multi || entries.iter().any(|e| e.root.is_none()) {
                bail!("kind files needs multiple file entries");
            }
        }
        ManifestKind::Folder => {
            if mode != ManifestMode::Multi {
                bail!("kind folder needs multi mode");
            }
        }
    }
    Ok(Manifest {
        offer,
        mode,
        label,
        kind,
        chunk_size,
        created_at: created_at.to_string(),
        entries,
    })
}

/// Renders a manifest back to its canonical JSON value (decimal strings,
/// `null` roots for directories). Round-trips byte-identically through
/// [`canonical_json`] when the input was canonical.
pub fn manifest_value(manifest: &Manifest) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = manifest
        .entries
        .iter()
        .map(|e| {
            let root = match e.root {
                Some(bytes) => serde_json::Value::String(hex::encode(bytes)),
                None => serde_json::Value::Null,
            };
            serde_json::json!({
                "chunkCount": e.chunk_count.to_string(),
                "chunks": e.chunks.iter().map(hex::encode).collect::<Vec<_>>(),
                "id": e.id.to_string(),
                "mtime": e.mtime.to_string(),
                "path": e.path,
                "root": root,
                "size": e.size.to_string(),
            })
        })
        .collect();
    serde_json::json!({
        "chunkSize": manifest.chunk_size.to_string(),
        "createdAt": manifest.created_at,
        "entries": entries,
        "kind": manifest.kind.as_str(),
        "label": manifest.label,
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

/// FINAL plaintext of a RAW transfer: `u64be(total)`.
pub const FINAL_RAW_LEN: usize = 8;

/// FINAL plaintext of an ARCHIVE transfer:
/// `u64be(total) || u64be(chunk_count) || root[32]`.
///
/// An archive is GENERATED, so none of those three quantities is in the
/// manifest and none of them can be checked against it: this frame is where
/// the recipient learns what it should have received, and the AEAD over it
/// is what makes that claim the source's own. Which length is expected
/// follows from the transfer's mode, so the two shapes are never ambiguous.
/// The server never opens a frame on the live path — it relays ciphertext —
/// so this rule is the codec's, shared with the browser and the fixtures.
pub const FINAL_ARCHIVE_LEN: usize = 8 + 8 + 32;

const FINAL_LEN_ERROR: &str =
    "FINAL plaintext must be u64be(total) (8 bytes) or the archive tuple (48 bytes)";

/// The FINAL plaintext lengths this protocol defines.
fn is_final_len(len: usize) -> bool {
    len == FINAL_RAW_LEN || len == FINAL_ARCHIVE_LEN
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
            if !is_final_len(plaintext.len()) {
                bail!(FINAL_LEN_ERROR);
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
            if !is_final_len(plaintext.len()) {
                bail!(FINAL_LEN_ERROR);
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
    let value = parse_json_no_duplicate_keys(raw, "relay.attach")?;
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

    /// 6.3: a repeated key is refused, at every nesting level, on both the
    /// control envelope and the relay attach.
    ///
    /// `serde_json` keeps the last value silently, and so does the browser's
    /// `JSON.parse` — but nothing on the wire GUARANTEES the two agree, and a
    /// message that reads differently to two parsers is a message the server
    /// cannot honestly validate.
    #[test]
    fn duplicate_json_keys_are_rejected() {
        // Top level.
        let err = parse_client_envelope(r#"{"v":1,"type":"ping","body":{},"body":{"x":1}}"#)
            .expect_err("a repeated top-level key must be refused");
        assert!(format!("{err:#}").contains("repeats the key"), "{err:#}");

        // Inside the body.
        let err = parse_client_envelope(
            r#"{"v":1,"type":"hello","requestId":"00000000000000000000000000000001","body":{"memberToken":"a","memberToken":"b"}}"#,
        )
        .expect_err("a repeated body key must be refused");
        assert!(format!("{err:#}").contains("repeats the key"), "{err:#}");

        // Inside an array element two levels down — a manifest entry is
        // exactly this shape, and two `path`s is the useful attack.
        let err = parse_client_envelope(
            r#"{"v":1,"type":"offer.publish","requestId":"00000000000000000000000000000001","body":{"manifest":{"entries":[{"path":"a","path":"b"}]}}}"#,
        )
        .expect_err("a repeated key in a nested entry must be refused");
        assert!(format!("{err:#}").contains("repeats the key"), "{err:#}");

        // The relay attach shares the rule.
        let err = parse_relay_attach(r#"{"v":1,"role":"source","role":"recipient"}"#)
            .expect_err("a repeated relay.attach key must be refused");
        assert!(format!("{err:#}").contains("repeats the key"), "{err:#}");

        // ... and a message with no repeat still parses, unchanged.
        // `ping` carries no requestId by contract, so the positive case is
        // written the way the protocol actually looks.
        let ok = parse_client_envelope(r#"{"v":1,"type":"ping","body":{}}"#)
            .expect("a well-formed message still parses");
        assert_eq!(ok.typ, "ping");

        // Trailing content after a complete value is refused too: two
        // concatenated messages in one frame are two readings of one frame.
        let err = parse_json_no_duplicate_keys(r#"{"a":1}{"b":2}"#, "test")
            .expect_err("trailing content must be refused");
        assert!(format!("{err:#}").contains("trailing"), "{err:#}");
    }

    /// 6.3: every decoder that reads peer bytes survives a hostile corpus —
    /// no panic, no unbounded allocation, and a stable error for each.
    ///
    /// The corpus is DETERMINISTIC on purpose: a fuzzer finds inputs, a
    /// corpus keeps them. Each case is one of the classes the phase contract
    /// names (truncated, oversize, unknown field, duplicate, invalid UTF-8
    /// escape, invalid number, invalid hex, integer boundary, reordered
    /// lifecycle), and the assertion is the same for all of them: `Err`, not
    /// a panic and not a success.
    #[test]
    fn all_web_decoders_are_panic_free_and_allocation_bounded() {
        let limits = crate::web_transfer::WebTransferLimits::default();
        let mut corpus: Vec<String> = vec![
            String::new(),
            "{".into(),
            "}".into(),
            "[]".into(),
            "null".into(),
            "0".into(),
            "\"".into(),
            "{\"v\":1}".into(),
            "{\"v\":1,\"type\":\"ping\"}".into(),
            // Unknown top-level field.
            r#"{"v":1,"type":"ping","requestId":"00000000000000000000000000000001","body":{},"extra":1}"#.into(),
            // Wrong version, and a version that does not fit u64.
            r#"{"v":2,"type":"ping","requestId":"00000000000000000000000000000001","body":{}}"#.into(),
            r#"{"v":99999999999999999999999,"type":"ping","body":{}}"#.into(),
            // Unknown type, and a type that is not a string.
            r#"{"v":1,"type":"nope","requestId":"00000000000000000000000000000001","body":{}}"#.into(),
            r#"{"v":1,"type":7,"body":{}}"#.into(),
            // requestId of every wrong shape.
            r#"{"v":1,"type":"ping","requestId":"","body":{}}"#.into(),
            r#"{"v":1,"type":"ping","requestId":"00000000000000000000000000000001aa","body":{}}"#.into(),
            r#"{"v":1,"type":"ping","requestId":"0000000000000000000000000000000G","body":{}}"#.into(),
            r#"{"v":1,"type":"ping","requestId":"0000000000000000000000000000000A","body":{}}"#.into(),
            r#"{"v":1,"type":"ping","requestId":1,"body":{}}"#.into(),
            // Body that is not an object.
            r#"{"v":1,"type":"ping","requestId":"00000000000000000000000000000001","body":[]}"#.into(),
            // Lone surrogate escape — valid JSON grammar, invalid Unicode.
            r#"{"v":1,"type":"peer.rename","requestId":"00000000000000000000000000000001","body":{"displayName":"\ud800"}}"#.into(),
            // Deep nesting: the parser must refuse, never recurse to death.
            format!("{}{}", "[".repeat(512), "]".repeat(512)),
        ];
        // One control message just over the cap, and one just under it.
        corpus.push(format!(
            r#"{{"v":1,"type":"peer.rename","requestId":"00000000000000000000000000000001","body":{{"displayName":"{}"}}}}"#,
            "a".repeat(WEB_TRANSFER_MAX_CONTROL_BYTES + 1)
        ));

        for raw in &corpus {
            // Every text decoder sees every case. The contract is uniform:
            // an error, never a panic and never a silent success.
            let client = parse_client_envelope(raw);
            let server = parse_server_envelope(raw);
            let attach = parse_relay_attach(raw);
            assert!(
                attach.is_err(),
                "relay.attach accepted a corpus case: {raw:.80}"
            );
            // A valid envelope in the corpus is a bug in the corpus, not in
            // the decoder: every case above is malformed by construction.
            assert!(
                client.is_err() && server.is_err(),
                "a malformed corpus case parsed: {raw:.80}"
            );
        }

        // Envelope-VALID messages whose BODY is hostile: the envelope is not
        // the place that rejects them, and asserting otherwise would pin the
        // wrong layer. Each one must fail in its own body parser.
        let body_corpus: Vec<(&str, String)> = vec![
            // An integer past u64 where a byte count is expected.
            (
                "transfer.progress",
                r#"{"v":1,"type":"transfer.progress","requestId":"00000000000000000000000000000001","body":{"transferId":"00000000000000000000000000000001","attemptId":"00000000000000000000000000000001","bytes":18446744073709551616}}"#.into(),
            ),
            // A display name that is not a string.
            (
                "peer.rename",
                r#"{"v":1,"type":"peer.rename","requestId":"00000000000000000000000000000001","body":{"displayName":7}}"#.into(),
            ),
            // An unknown body field: the body tables are exact.
            (
                "peer.rename",
                r#"{"v":1,"type":"peer.rename","requestId":"00000000000000000000000000000001","body":{"displayName":"ok","extra":1}}"#.into(),
            ),
            // A transfer id of the wrong width.
            (
                "transfer.cancel",
                r#"{"v":1,"type":"transfer.cancel","requestId":"00000000000000000000000000000001","body":{"transferId":"00"}}"#.into(),
            ),
            // An SDP that is not a string, and one past the cap.
            (
                "rtc.offer",
                r#"{"v":1,"type":"rtc.offer","requestId":"00000000000000000000000000000001","body":{"transferId":"00000000000000000000000000000001","attemptId":"00000000000000000000000000000001","sdp":[]}}"#.into(),
            ),
        ];
        for (typ, raw) in &body_corpus {
            let env = parse_client_envelope(raw)
                .unwrap_or_else(|e| panic!("corpus case is not envelope-valid: {typ}: {e:#}"));
            let refused = match *typ {
                "transfer.progress" => parse_progress_body(&env).is_err(),
                "peer.rename" => parse_rename_body(&env).is_err(),
                "transfer.cancel" => parse_cancel_body(&env).is_err(),
                "rtc.offer" => parse_rtc_sdp_body(&env, "rtc.offer").is_err(),
                other => panic!("corpus names a body parser nobody drives: {other}"),
            };
            assert!(refused, "a hostile {typ} body parsed: {raw:.120}");
        }

        // Value-level decoders, driven directly with hostile values.
        for value in [
            serde_json::json!(null),
            serde_json::json!(0),
            serde_json::json!("x"),
            serde_json::json!({}),
            serde_json::json!([[0]]),
            serde_json::json!([[1, 0]]),
            serde_json::json!([[0, u64::MAX]]),
            serde_json::json!([["0", "1"]]),
            serde_json::json!(vec![vec![0u64, 1u64]; MAX_RESUME_RANGES + 1]),
        ] {
            let _ = parse_verified_ranges(&value);
            let _ = parse_resume_descriptor(&value);
            let _ = parse_manifest(&value, &limits);
        }

        // Binary frames: every truncation of a valid frame, plus a header
        // that claims a body it does not carry.
        let key = [7u8; 32];
        let frame = seal_frame(&key, 0, FrameType::Data, b"payload").expect("seal");
        for cut in 0..frame.len() {
            assert!(
                open_frame(&key, &frame[..cut], 0).is_err(),
                "a truncated frame opened at {cut} bytes"
            );
        }
        let mut lying = frame.clone();
        let claimed = u32::MAX.to_be_bytes();
        lying[12..16].copy_from_slice(&claimed);
        assert!(
            open_frame(&key, &lying, 0).is_err(),
            "a frame claiming 4 GiB of body opened"
        );
        // ... and the untouched frame still opens, so the loop above proves
        // rejection and not a broken fixture.
        assert!(open_frame(&key, &frame, 0).is_ok(), "the valid frame broke");
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
    fn request_free_errors_parse_without_request_id() {
        // Version/rate failures on `hello`/`ping` travel without `requestId`
        // (2.2); the fixture error with an echo still parses, and `ack`
        // without one is still rejected.
        let anon = error_envelope_anon("UNSUPPORTED_VERSION", None);
        let env = parse_server_envelope(&anon).unwrap();
        assert_eq!(env.typ, "error");
        assert!(env.request_id.is_none());
        let id: RequestId = "dddddddddddddddddddddddddddddddd".parse().unwrap();
        let echoed = error_envelope(id, "RATE_LIMITED", None);
        assert_eq!(parse_server_envelope(&echoed).unwrap().request_id, Some(id));
        assert!(parse_server_envelope(r#"{"v":1,"type":"ack","body":{}}"#).is_err());
        // Unknown codes collapse to INTERNAL, never pass through.
        let weird = error_envelope_anon("NOPE", None);
        let body: serde_json::Value = serde_json::from_str(&weird).unwrap();
        assert_eq!(
            body["body"]["code"],
            serde_json::Value::String("INTERNAL".to_string())
        );
    }

    #[test]
    fn welcome_carries_fixture_core_plus_additive_join_state() {
        use crate::web_transfer::{PeerId, RoomId, WebTransferLimits};
        let peer = PeerId::from_bytes([0x11u8; 16]);
        let room = RoomId::from_bytes([0x22u8; 16]);
        let raw = welcome_envelope(
            peer,
            room,
            "Bobi",
            &WebTransferLimits::default(),
            &["stun:x".to_string()],
            false,
        );
        let env = parse_server_envelope(&raw).unwrap();
        assert_eq!(env.typ, "welcome");
        // Fixture-pinned core first, additive join state after.
        assert_eq!(
            env.body["peerId"],
            serde_json::Value::String(peer.to_string())
        );
        assert_eq!(
            env.body["roomId"],
            serde_json::Value::String(room.to_string())
        );
        assert_eq!(
            env.body["displayName"],
            serde_json::Value::String("Bobi".to_string())
        );
        assert_eq!(
            env.body["limits"]["max_peers_per_room"],
            serde_json::Value::Number(32.into())
        );
        assert_eq!(
            env.body["iceServers"][0],
            serde_json::Value::String("stun:x".to_string())
        );
    }

    #[test]
    fn canonical_manifest_fixture_matches_byte_for_byte() {
        let manifest_raw = fixture("manifest.json");
        let manifest_raw_value: serde_json::Value = serde_json::from_str(&manifest_raw).unwrap();
        let manifest = parse_manifest(
            &manifest_raw_value,
            &crate::web_transfer::WebTransferLimits::default(),
        )
        .unwrap();
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

    /// Minimal valid extended manifest for the 2.3 validator tests: one file
    /// entry with a recomputed root (never hardcoded, so the formula path is
    /// what the tests pin).
    fn valid_manifest_json(offer_hex: &str) -> serde_json::Value {
        let leaf: [u8; 32] =
            hex::decode("094c9eb526be7e2dea0b396331085eaba0d76f639116ccc014055c490b48daac")
                .unwrap()
                .try_into()
                .unwrap();
        let root = hex::encode(file_root(1, &[leaf]).unwrap());
        serde_json::json!({
            "offer": offer_hex,
            "mode": "single",
            "label": "Demo file",
            "kind": "file",
            "chunkSize": "1048576",
            "createdAt": "2026-09-14T12:00:00Z",
            "entries": [{
                "id": "0",
                "path": "hello.txt",
                "size": "11",
                "mtime": "1757779200",
                "chunks": ["094c9eb526be7e2dea0b396331085eaba0d76f639116ccc014055c490b48daac"],
                "chunkCount": "1",
                "root": root,
            }],
        })
    }

    fn default_test_limits() -> crate::web_transfer::WebTransferLimits {
        crate::web_transfer::WebTransferLimits::default()
    }

    #[test]
    fn manifest_accepts_exact_fixture() {
        let raw: serde_json::Value = serde_json::from_str(&fixture("manifest.json")).unwrap();
        let manifest = parse_manifest(&raw, &default_test_limits()).unwrap();
        assert_eq!(
            manifest.offer.to_string(),
            "cccccccccccccccccccccccccccccccc"
        );
        assert_eq!(manifest.mode, ManifestMode::Multi);
        assert_eq!(manifest.label, "Demo");
        assert_eq!(manifest.kind, ManifestKind::Files);
        assert_eq!(manifest.chunk_size, 1024 * 1024);
        assert_eq!(manifest.created_at, "2026-09-14T12:00:00Z");
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(manifest.entries[0].id, 0);
        assert_eq!(manifest.entries[1].id, 1);
        assert!(manifest.entries[0].root.is_some());
        // Canonical re-encode matches the checked-in bytes exactly.
        let canonical = canonical_json(&manifest_value(&manifest)).unwrap();
        assert_eq!(
            canonical.as_bytes(),
            fixture("manifest.canonical.json").as_bytes()
        );
        // The minimal single-file manifest validates too.
        let single = parse_manifest(
            &valid_manifest_json("dddddddddddddddddddddddddddddddd"),
            &default_test_limits(),
        )
        .unwrap();
        assert_eq!(single.kind, ManifestKind::File);
    }

    #[test]
    fn manifest_rejects_every_path_escape_and_normalization_collision() {
        let limits = default_test_limits();
        let base = valid_manifest_json("dddddddddddddddddddddddddddddddd");
        let with_path = |path: &str| {
            let mut value = base.clone();
            value["entries"][0]["path"] = serde_json::Value::String(path.to_string());
            value
        };
        for bad in [
            "../evil.txt",
            "/abs.txt",
            "a//b.txt",
            "a/./b.txt",
            "a/../b.txt",
            "back\\slash.txt",
            "trailing/",
            "bad\0byte.txt",
            "cafe\u{301}", // NFD: e + combining acute, not NFC
        ] {
            assert!(parse_manifest(&with_path(bad), &limits).is_err(), "{bad:?}");
        }
        // NFC+casefold collisions across entries, and duplicates.
        let second = |id: &str, path: &str| {
            let mut e = base["entries"][0].clone();
            e["id"] = serde_json::Value::String(id.to_string());
            e["path"] = serde_json::Value::String(path.to_string());
            e
        };
        let colliding = {
            let mut value = base.clone();
            value["mode"] = serde_json::Value::String("multi".to_string());
            value["kind"] = serde_json::Value::String("files".to_string());
            value["entries"] =
                serde_json::Value::Array(vec![second("0", "a.txt"), second("1", "A.TXT")]);
            value
        };
        assert!(parse_manifest(&colliding, &limits).is_err());
        let duplicate = {
            let mut value = base.clone();
            value["mode"] = serde_json::Value::String("multi".to_string());
            value["kind"] = serde_json::Value::String("files".to_string());
            value["entries"] =
                serde_json::Value::Array(vec![second("0", "a.txt"), second("1", "a.txt")]);
            value
        };
        assert!(parse_manifest(&duplicate, &limits).is_err());
    }

    #[test]
    fn manifest_rejects_unsorted_nonsequential_or_overflowing_entries() {
        let limits = default_test_limits();
        let base = valid_manifest_json("dddddddddddddddddddddddddddddddd");
        let entry = |id: &str, path: &str| {
            let mut e = base["entries"][0].clone();
            e["id"] = serde_json::Value::String(id.to_string());
            e["path"] = serde_json::Value::String(path.to_string());
            e
        };
        let multi = |entries: Vec<serde_json::Value>| {
            let mut value = base.clone();
            value["mode"] = serde_json::Value::String("multi".to_string());
            value["kind"] = serde_json::Value::String("files".to_string());
            value["entries"] = serde_json::Value::Array(entries);
            value
        };
        // Descending paths with sequential IDs: sort violation, not ID.
        assert!(parse_manifest(
            &multi(vec![entry("0", "b.txt"), entry("1", "a.txt")]),
            &limits
        )
        .is_err());
        // Gap and non-zero start.
        assert!(parse_manifest(
            &multi(vec![entry("0", "a.txt"), entry("2", "b.txt")]),
            &limits
        )
        .is_err());
        assert!(parse_manifest(
            &multi(vec![entry("1", "a.txt"), entry("2", "b.txt")]),
            &limits
        )
        .is_err());
        // Duplicate IDs.
        assert!(parse_manifest(
            &multi(vec![entry("0", "a.txt"), entry("0", "b.txt")]),
            &limits
        )
        .is_err());
        // Gigantic ID beyond u32.
        assert!(parse_manifest(
            &multi(vec![entry("0", "a.txt"), entry("4294967296", "b.txt")]),
            &limits
        )
        .is_err());
        // A well-formed two-entry manifest passes.
        let ok = parse_manifest(
            &multi(vec![entry("0", "a.txt"), entry("1", "b.txt")]),
            &limits,
        )
        .unwrap();
        assert_eq!(ok.entries.len(), 2);
    }

    #[test]
    fn manifest_rejects_noncanonical_decimal_and_wrong_root_shape() {
        let limits = default_test_limits();
        let base = valid_manifest_json("dddddddddddddddddddddddddddddddd");
        let mutate = |field: &str, value: serde_json::Value| {
            let mut manifest = base.clone();
            manifest["entries"][0][field] = value;
            manifest
        };
        for (field, value) in [
            ("size", serde_json::Value::String("007".to_string())),
            ("size", serde_json::Value::String("".to_string())),
            (
                "size",
                serde_json::Value::String("18446744073709551616".to_string()),
            ),
            ("mtime", serde_json::Value::String("-1".to_string())),
            ("chunkCount", serde_json::Value::String("2".to_string())),
            ("chunkCount", serde_json::Value::String("00".to_string())),
            ("id", serde_json::Value::String("00".to_string())),
            ("root", serde_json::Value::String("zz".to_string())),
            ("root", serde_json::Value::String("0".repeat(64))),
            ("root", serde_json::Value::Null),
        ] {
            assert!(
                parse_manifest(&mutate(field, value), &limits).is_err(),
                "{field} must reject"
            );
        }
        // Unknown entry and manifest fields are rejected.
        let mut extra = base.clone();
        extra["entries"][0]["wat"] = serde_json::Value::Bool(true);
        assert!(parse_manifest(&extra, &limits).is_err());
        let mut extra_top = base.clone();
        extra_top["wat"] = serde_json::Value::Bool(true);
        assert!(parse_manifest(&extra_top, &limits).is_err());
        // Label / kind / chunkSize / createdAt shapes.
        for patch in [
            serde_json::json!({"label": ""}),
            serde_json::json!({"label": "x".repeat(129)}),
            serde_json::json!({"kind": "disk"}),
            serde_json::json!({"chunkSize": "512"}),
            serde_json::json!({"createdAt": "yesterday"}),
            serde_json::json!({"createdAt": "2026-13-01T00:00:00Z"}),
            serde_json::json!({"createdAt": "2026-09-14 12:00:00"}),
        ] {
            let mut manifest = base.clone();
            for (key, value) in patch.as_object().unwrap() {
                manifest[key] = value.clone();
            }
            assert!(parse_manifest(&manifest, &limits).is_err(), "{patch}");
        }
        // Directory entries validate: null root, empty chunks, kind folder.
        let dir = serde_json::json!({
            "offer": "dddddddddddddddddddddddddddddddd",
            "mode": "multi",
            "label": "Tree",
            "kind": "folder",
            "chunkSize": "1048576",
            "createdAt": "2026-09-14T12:00:00Z",
            "entries": [{
                "id": "0",
                "path": "docs",
                "size": "0",
                "mtime": "1757779200",
                "chunks": [],
                "chunkCount": "0",
                "root": null,
            }],
        });
        parse_manifest(&dir, &limits).unwrap();
        // ...but not under kind files.
        let mut dir_files = dir.clone();
        dir_files["kind"] = serde_json::Value::String("files".to_string());
        assert!(parse_manifest(&dir_files, &limits).is_err());
        // A null root on a file entry is rejected even with matching counts.
        let mut null_file = base.clone();
        null_file["entries"][0]["root"] = serde_json::Value::Null;
        assert!(parse_manifest(&null_file, &limits).is_err());
    }

    #[test]
    fn manifest_limits_apply_before_large_allocation() {
        use crate::web_transfer::WebTransferLimits;
        let tight = WebTransferLimits {
            max_entries_per_offer: 2,
            max_offer_bytes: 100,
            ..WebTransferLimits::default()
        };
        let base = valid_manifest_json("dddddddddddddddddddddddddddddddd");
        // Three entries exceed the count cap (checked before allocation).
        let entry = base["entries"][0].clone();
        let three = {
            let mut value = base.clone();
            value["mode"] = serde_json::Value::String("multi".to_string());
            value["kind"] = serde_json::Value::String("files".to_string());
            value["entries"] = serde_json::Value::Array(vec![
                entry.clone(),
                {
                    let mut second = entry.clone();
                    second["id"] = serde_json::Value::String("1".to_string());
                    second["path"] = serde_json::Value::String("b.txt".to_string());
                    second
                },
                {
                    let mut third = entry.clone();
                    third["id"] = serde_json::Value::String("2".to_string());
                    third["path"] = serde_json::Value::String("c.txt".to_string());
                    third
                },
            ]);
            value
        };
        assert!(parse_manifest(&three, &tight).is_err());
        // Eleven bytes exceed the 100-byte total only when a bigger entry
        // joins: assert the total gate with a synthetic large size.
        let mut big = base.clone();
        big["entries"][0]["size"] = serde_json::Value::String("95".to_string());
        big["entries"][0]["chunks"] = serde_json::Value::Array(vec![]);
        big["entries"][0]["chunkCount"] = serde_json::Value::String("0".to_string());
        big["entries"][0]["root"] =
            serde_json::Value::String(hex::encode(file_root(0, &[]).unwrap()));
        // size 95 claims chunks it does not carry: still rejected (counts
        // first), proving allocation follows validation, not the reverse.
        assert!(parse_manifest(&big, &tight).is_err());
        // Byte cap on the compact form.
        assert!(check_manifest_byte_cap(256 * 1024).is_ok());
        assert!(check_manifest_byte_cap(256 * 1024 + 1).is_err());
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

    /// A FINAL frame carries ONE of two shapes, and no third: `u64be(total)`
    /// for a raw transfer, or the archive tuple for a `zip` one. The archive
    /// shape exists because none of its three quantities is in the manifest.
    #[test]
    fn final_frames_carry_the_raw_total_or_the_archive_tuple_and_nothing_else() {
        let key = [7u8; 32];
        assert_eq!(FINAL_RAW_LEN, 8);
        assert_eq!(FINAL_ARCHIVE_LEN, 48);
        let raw = [0u8; FINAL_RAW_LEN];
        let archive = [0u8; FINAL_ARCHIVE_LEN];
        let sealed_raw = seal_frame(&key, 0, FrameType::Final, &raw).unwrap();
        assert_eq!(
            open_frame(&key, &sealed_raw, 0).unwrap().plaintext.len(),
            FINAL_RAW_LEN
        );
        let sealed_archive = seal_frame(&key, 1, FrameType::Final, &archive).unwrap();
        assert_eq!(
            open_frame(&key, &sealed_archive, 0)
                .unwrap()
                .plaintext
                .len(),
            FINAL_ARCHIVE_LEN
        );
        // Every other length is refused on BOTH sides — one byte either way
        // around each shape, and the empty frame.
        for len in [0usize, 7, 9, 16, 47, 49, 64] {
            let plaintext = vec![0u8; len];
            assert!(
                seal_frame(&key, 0, FrameType::Final, &plaintext).is_err(),
                "sealed a {len}-byte FINAL"
            );
        }
        // And a peer that seals a length this codec refuses cannot make it
        // open either: the rule is applied after the AEAD, not instead of it.
        let mut forged = sealed_archive.clone();
        forged.truncate(forged.len() - 1);
        assert!(open_frame(&key, &forged, 0).is_err());
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
        let manifest = parse_manifest(
            &manifest_raw_value,
            &crate::web_transfer::WebTransferLimits::default(),
        )
        .unwrap();
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

#[cfg(test)]
mod transfer_bodies_tests {
    use super::*;

    fn envelope(typ: &str, request_id: Option<&str>, body: serde_json::Value) -> String {
        let mut top = std::collections::BTreeMap::new();
        top.insert("v".to_string(), serde_json::Value::from(1u64));
        top.insert(
            "type".to_string(),
            serde_json::Value::String(typ.to_string()),
        );
        if let Some(id) = request_id {
            top.insert(
                "requestId".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
        top.insert("body".to_string(), body);
        canonical_json(&serde_json::Value::Object(top.into_iter().collect())).unwrap()
    }

    const RID: &str = "dddddddddddddddddddddddddddddddd";
    const OFFER: &str = "cccccccccccccccccccccccccccccccc";
    const TRANSFER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ATTEMPT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEX64: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    #[test]
    fn transfer_bodies_parse_strictly_and_builders_round_trip() {
        // request
        let raw = envelope(
            "transfer.request",
            Some(RID),
            serde_json::json!({
                "offerId": OFFER,
                "entryIds": ["0"],
                "selectionDigest": HEX64,
                "mode": "raw",
            }),
        );
        let env = parse_client_envelope(&raw).unwrap();
        let (id, body) = parse_transfer_request_body(&env).unwrap();
        assert_eq!(id.to_string(), RID);
        assert_eq!(body.offer_id.to_string(), OFFER);
        assert_eq!(body.entry_ids, vec!["0".to_string()]);
        assert_eq!(body.mode, "raw");
        assert!(body.resume.is_none());
        // unsorted/duplicated IDs, bad mode shape, oversized resume rejected
        for bad in [
            serde_json::json!({"offerId": OFFER, "entryIds": ["1", "0"], "selectionDigest": HEX64, "mode": "raw"}),
            serde_json::json!({"offerId": OFFER, "entryIds": ["0", "0"], "selectionDigest": HEX64, "mode": "raw"}),
            serde_json::json!({"offerId": OFFER, "entryIds": ["0"], "selectionDigest": HEX64, "mode": "raw", "extra": 1}),
        ] {
            let env = parse_client_envelope(&envelope("transfer.request", Some(RID), bad)).unwrap();
            assert!(parse_transfer_request_body(&env).is_err());
        }
        // resume descriptor bounds
        let resume_ok: serde_json::Value = serde_json::json!({
            "verifiedRanges": [[0, 4], [8, 9]],
            "outputLength": 11,
        });
        parse_resume_descriptor(&resume_ok).unwrap();
        assert!(parse_resume_descriptor(
            &serde_json::json!({"verifiedRanges": [[5, 3]], "outputLength": 0})
        )
        .is_err());
        assert!(parse_resume_descriptor(
            &serde_json::json!({"verifiedRanges": [[0, 4], [2, 9]], "outputLength": 0})
        )
        .is_err());
        // ready / reject / cancel / complete
        let env = parse_client_envelope(&envelope(
            "transfer.source_ready",
            Some(RID),
            serde_json::json!({"transferId": TRANSFER, "attemptId": ATTEMPT, "selectionDigest": HEX64}),
        ))
        .unwrap();
        let (_, ready) = parse_source_ready_body(&env).unwrap();
        assert_eq!(ready.transfer_id.to_string(), TRANSFER);
        let env = parse_client_envelope(&envelope(
            "transfer.reject",
            Some(RID),
            serde_json::json!({"transferId": TRANSFER}),
        ))
        .unwrap();
        assert!(parse_reject_body(&env).is_ok());
        let env = parse_client_envelope(&envelope(
            "transfer.cancel",
            Some(RID),
            serde_json::json!({"transferId": TRANSFER}),
        ))
        .unwrap();
        assert!(parse_cancel_body(&env).is_ok());
        let env = parse_client_envelope(&envelope(
            "transfer.complete",
            Some(RID),
            serde_json::json!({"transferId": TRANSFER, "attemptId": ATTEMPT, "root": HEX64}),
        ))
        .unwrap();
        let (_, complete) = parse_complete_body(&env).unwrap();
        assert_eq!(complete.attempt_id.to_string(), ATTEMPT);
        // builders round-trip through the server envelope parser
        let peer: PeerId = "11111111111111111111111111111111".parse().unwrap();
        let offer: OfferId = OFFER.parse().unwrap();
        let transfer: TransferId = TRANSFER.parse().unwrap();
        let attempt: AttemptId = ATTEMPT.parse().unwrap();
        for (typ, raw) in [
            (
                "transfer.incoming",
                transfer_incoming_envelope(transfer, offer, peer, attempt, "raw", Some(0)),
            ),
            (
                "transfer.relay_ticket",
                transfer_relay_ticket_envelope(transfer, attempt, &"ab".repeat(16)),
            ),
            (
                "transfer.cancelled",
                transfer_cancelled_envelope(transfer, peer),
            ),
            (
                "transfer.completed",
                transfer_completed_envelope(transfer, &[7u8; 32]),
            ),
            (
                "transfer.path_commit",
                transfer_path_commit_envelope(transfer, attempt, "relay", &[]),
            ),
        ] {
            let env = parse_server_envelope(&raw).unwrap();
            assert_eq!(env.typ, typ);
        }
        let commit: serde_json::Value = serde_json::from_str(&transfer_path_commit_envelope(
            transfer,
            attempt,
            "relay",
            &[],
        ))
        .unwrap();
        assert_eq!(commit["body"]["path"].as_str(), Some("relay"));
        assert_eq!(commit["body"]["attemptId"].as_str(), Some(ATTEMPT));
        // A recipient holding nothing gets the Phase 3.1 envelope unchanged;
        // ranges appear only when there is something for the source to skip.
        assert!(commit["body"].get("resumeRanges").is_none());
        let resumed: serde_json::Value = serde_json::from_str(&transfer_path_commit_envelope(
            transfer,
            attempt,
            "relay",
            &[(0, 4), (8, 9)],
        ))
        .unwrap();
        assert_eq!(
            resumed["body"]["resumeRanges"],
            serde_json::json!([[0, 4], [8, 9]])
        );
        // selection digest is deterministic and input-sensitive
        let mac = [0xeeu8; 32];
        let first = selection_digest(&offer, &mac, &["0".to_string()], "raw");
        assert_eq!(
            selection_digest(&offer, &mac, &["0".to_string()], "raw"),
            first
        );
        assert_ne!(
            selection_digest(&offer, &mac, &["1".to_string()], "raw"),
            first
        );
        assert_ne!(
            selection_digest(&offer, &[0xefu8; 32], &["0".to_string()], "raw"),
            first
        );
    }
}

#[cfg(test)]
mod direct_signaling_body_tests {
    use super::*;

    fn envelope(typ: &str, body: serde_json::Value) -> ParsedEnvelope {
        let raw = serde_json::json!({
            "v": 1,
            "type": typ,
            "requestId": "dddddddddddddddddddddddddddddddd",
            "body": body,
        })
        .to_string();
        parse_client_envelope(&raw).unwrap()
    }

    const TID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const AID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn the_carrier_index_is_optional_bounded_and_the_same_on_all_three_signals() {
        // Absent means carrier 0, which is what an older peer sends and what
        // every single-carrier attempt sends; anything at or past the mask's
        // width is refused at the PARSER, so the server's own bitmask never
        // sees an index it cannot represent.
        let max = crate::web_transfer::WEB_TRANSFER_MAX_DIRECT_CARRIERS as u64;
        for typ in ["rtc.offer", "rtc.answer"] {
            let absent = envelope(
                typ,
                serde_json::json!({ "transferId": TID, "attemptId": AID, "sdp": "v=0" }),
            );
            assert_eq!(parse_rtc_sdp_body(&absent, typ).unwrap().1.carrier, 0);
            let third = envelope(
                typ,
                serde_json::json!({
                    "transferId": TID, "attemptId": AID, "sdp": "v=0", "carrier": 3,
                }),
            );
            assert_eq!(parse_rtc_sdp_body(&third, typ).unwrap().1.carrier, 3);
            for bad in [
                serde_json::json!(max),
                serde_json::json!(-1),
                serde_json::json!("2"),
                serde_json::json!(1.5),
            ] {
                let env = envelope(
                    typ,
                    serde_json::json!({
                        "transferId": TID, "attemptId": AID, "sdp": "v=0", "carrier": bad,
                    }),
                );
                assert!(
                    parse_rtc_sdp_body(&env, typ).is_err(),
                    "{typ} must refuse carrier {bad}"
                );
            }
        }
        let ice = envelope(
            "rtc.ice",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "candidate": "candidate:1 1 udp 1 1.2.3.4 1 typ host",
                "sdpMid": "0", "sdpMLineIndex": 0, "carrier": 2,
            }),
        );
        assert_eq!(parse_rtc_ice_body(&ice).unwrap().1.carrier, 2);
        // The end-of-candidates marker ends gathering for ONE carrier, so it
        // must be able to say which — the carrier is not a media field.
        let done = envelope(
            "rtc.ice",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "candidate": null, "carrier": 2,
            }),
        );
        let (_, body) = parse_rtc_ice_body(&done).unwrap();
        assert!(body.is_end_of_candidates());
        assert_eq!(body.carrier, 2);
    }

    #[test]
    fn sdp_bodies_bound_bytes_and_reject_foreign_fields() {
        let ok = envelope(
            "rtc.offer",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "sdp": "v=0",
            }),
        );
        let (_, body) = parse_rtc_sdp_body(&ok, "rtc.offer").unwrap();
        assert_eq!(body.sdp, "v=0");
        // A body for the other type does not parse as this one.
        assert!(parse_rtc_sdp_body(&ok, "rtc.answer").is_err());
        for bad in [
            serde_json::json!({"transferId": TID, "attemptId": AID, "sdp": ""}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "sdp": 5}),
            serde_json::json!({"transferId": TID, "attemptId": AID}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "sdp": "v=0", "type": "offer"}),
            serde_json::json!({"transferId": "nope", "attemptId": AID, "sdp": "v=0"}),
        ] {
            let env = envelope("rtc.offer", bad);
            assert!(parse_rtc_sdp_body(&env, "rtc.offer").is_err());
        }
        // Exactly at the cap parses; one byte over does not.
        let at_cap = "x".repeat(crate::web_transfer::WEB_TRANSFER_MAX_SDP_BYTES);
        let env = envelope(
            "rtc.offer",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "sdp": at_cap,
            }),
        );
        assert!(parse_rtc_sdp_body(&env, "rtc.offer").is_ok());
        let over = "x".repeat(crate::web_transfer::WEB_TRANSFER_MAX_SDP_BYTES + 1);
        let env = envelope(
            "rtc.offer",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "sdp": over,
            }),
        );
        assert!(parse_rtc_sdp_body(&env, "rtc.offer").is_err());
    }

    #[test]
    fn ice_bodies_normalize_the_end_marker_and_bound_every_field() {
        // Absent, null and empty are one marker with one wire shape.
        for marker in [
            serde_json::json!({"transferId": TID, "attemptId": AID}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": null}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": ""}),
        ] {
            let env = envelope("rtc.ice", marker);
            let (_, body) = parse_rtc_ice_body(&env).unwrap();
            assert!(body.is_end_of_candidates());
            let out: serde_json::Value = serde_json::from_str(&rtc_ice_envelope(&body)).unwrap();
            assert!(out["body"]["candidate"].is_null());
            assert!(out["body"].get("sdpMid").is_none());
        }
        // What Firefox and WebKit actually send when gathering ends: the
        // empty candidate NAMES its m-section. Accepted, normalized to the
        // one marker shape, never refused (red-check: the old `bail!` here
        // reads `INVALID_MESSAGE` for 62 messages of a real Firefox run).
        for marker in [
            serde_json::json!({
                "transferId": TID, "attemptId": AID,
                "candidate": "", "sdpMid": "0", "sdpMLineIndex": 0,
            }),
            serde_json::json!({
                "transferId": TID, "attemptId": AID,
                "candidate": null, "sdpMid": "0",
            }),
        ] {
            let env = envelope("rtc.ice", marker);
            let (_, body) = parse_rtc_ice_body(&env).unwrap();
            assert!(body.is_end_of_candidates());
            assert_eq!(body.sdp_mid, None);
            assert_eq!(body.sdp_m_line_index, None);
            let out: serde_json::Value = serde_json::from_str(&rtc_ice_envelope(&body)).unwrap();
            assert!(out["body"]["candidate"].is_null());
            assert!(out["body"].get("sdpMid").is_none());
        }
        let env = envelope(
            "rtc.ice",
            serde_json::json!({
                "transferId": TID, "attemptId": AID,
                "candidate": "candidate:1 1 udp", "sdpMid": "0", "sdpMLineIndex": 0,
            }),
        );
        let (_, body) = parse_rtc_ice_body(&env).unwrap();
        assert_eq!(body.candidate.as_deref(), Some("candidate:1 1 udp"));
        assert_eq!(body.sdp_m_line_index, Some(0));
        let long_candidate =
            "c".repeat(crate::web_transfer::WEB_TRANSFER_MAX_ICE_CANDIDATE_BYTES + 1);
        let long_mid = "m".repeat(crate::web_transfer::WEB_TRANSFER_MAX_ICE_SDP_MID_BYTES + 1);
        let long_mid2 = long_mid.clone();
        for bad in [
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": long_candidate}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": "c", "sdpMid": long_mid}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": "c", "sdpMLineIndex": 70000}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": "c", "sdpMLineIndex": -1}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": 7}),
            // A marker's media fields are dropped, but they are still BOUNDED
            // first: normalization is not an escape from the length checks.
            serde_json::json!({"transferId": TID, "attemptId": AID, "candidate": "", "sdpMid": long_mid2}),
        ] {
            let env = envelope("rtc.ice", bad);
            assert!(parse_rtc_ice_body(&env).is_err());
        }
    }

    #[test]
    fn direct_failure_reasons_are_a_fixed_set_and_ranges_are_bounded() {
        // A peer's own string never travels: it is mapped or it is unknown.
        assert_eq!(direct_fail_reason(Some("ice-failed")), "ice-failed");
        assert_eq!(
            direct_fail_reason(Some("<script>alert(1)</script>")),
            "unknown"
        );
        assert_eq!(direct_fail_reason(None), "unknown");
        let env = envelope(
            "transfer.direct_failed",
            serde_json::json!({
                "transferId": TID, "attemptId": AID,
                "reason": "totally made up",
                "resumeRanges": [[2, 4], [0, 2]],
            }),
        );
        let (_, body) = parse_direct_failed_body(&env).unwrap();
        assert_eq!(body.reason, "unknown");
        // Sorted and merged by the shared range parser, exactly as a resume
        // descriptor on `transfer.request` would be.
        assert_eq!(body.verified_ranges, vec![(0, 2), (2, 4)]);
        for bad in [
            serde_json::json!({"transferId": TID, "attemptId": AID, "resumeRanges": [[4, 2]]}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "resumeRanges": [[0, 3], [1, 4]]}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "resumeRanges": [[0]]}),
            serde_json::json!({"transferId": TID, "attemptId": AID, "extra": 1}),
        ] {
            let env = envelope("transfer.direct_failed", bad);
            assert!(parse_direct_failed_body(&env).is_err());
        }
        let ranges = vec![(0u64, 2u64)];
        let out: serde_json::Value = serde_json::from_str(&transfer_direct_failed_envelope(
            TID.parse().unwrap(),
            AID.parse().unwrap(),
            "not a code",
            &ranges,
        ))
        .unwrap();
        assert_eq!(out["body"]["reason"].as_str(), Some("unknown"));
        assert_eq!(out["body"]["resumeRanges"][0][1].as_u64(), Some(2));
        // Nothing to resume means the field is absent, not an empty array.
        let out: serde_json::Value = serde_json::from_str(&transfer_direct_failed_envelope(
            TID.parse().unwrap(),
            AID.parse().unwrap(),
            "timeout",
            &[],
        ))
        .unwrap();
        assert!(out["body"].get("resumeRanges").is_none());
    }

    #[test]
    fn direct_start_and_ready_envelopes_carry_the_fixed_contract() {
        let out: serde_json::Value =
            serde_json::from_str(&transfer_direct_start_envelope(&DirectStart {
                transfer_id: TID.parse().unwrap(),
                attempt_id: AID.parse().unwrap(),
                attempt_number: 3,
                role: "offerer",
                ice_servers: &["stun:stun.example:3478".to_string()],
                deadline_ms: 10_000,
                carriers: 1,
                upgrade: false,
            }))
            .unwrap();
        assert_eq!(out["type"].as_str(), Some("transfer.direct_start"));
        assert_eq!(out["body"]["role"].as_str(), Some("offerer"));
        assert_eq!(out["body"]["attemptNumber"].as_u64(), Some(3));
        assert_eq!(out["body"]["deadlineMs"].as_u64(), Some(10_000));
        assert_eq!(
            out["body"]["iceServers"][0].as_str(),
            Some("stun:stun.example:3478")
        );
        assert!(out.get("requestId").is_none());
        // It parses back as a server message, so the fixture and the wire
        // agree about which direction each of these names travels in.
        let raw = transfer_direct_start_envelope(&DirectStart {
            transfer_id: TID.parse().unwrap(),
            attempt_id: AID.parse().unwrap(),
            attempt_number: 1,
            role: "answerer",
            ice_servers: &[],
            deadline_ms: 10_000,
            carriers: 1,
            upgrade: false,
        });
        assert!(parse_server_envelope(&raw).is_ok());
        for typ in [
            "rtc.offer",
            "rtc.answer",
            "rtc.ice",
            "transfer.direct_failed",
        ] {
            assert!(SERVER_TYPES.contains(&typ), "{typ} must be forwardable");
            assert!(CLIENT_TYPES.contains(&typ), "{typ} must be sendable");
        }
        let env = envelope(
            "transfer.direct_ready",
            serde_json::json!({
                "transferId": TID, "attemptId": AID,
            }),
        );
        let (_, ready) = parse_direct_ready_body(&env).unwrap();
        assert_eq!(ready.transfer_id.to_string(), TID);
        assert_eq!(ready.attempt_id.to_string(), AID);
        let env = envelope(
            "transfer.direct_ready",
            serde_json::json!({
                "transferId": TID, "attemptId": AID, "why": "no",
            }),
        );
        assert!(parse_direct_ready_body(&env).is_err());
    }
}
