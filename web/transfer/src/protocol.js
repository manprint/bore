// Web-transfer protocol v1: canonical JSON, manifest and control envelopes.
// Mirror of src/web_transfer_protocol.rs — same names, same rules, same
// rejections. Crypto primitives (HKDF/HMAC/roots/frames) live in crypto.js.
export const PROTOCOL_VERSION = 1;
export const CONTROL_SUBPROTOCOL = "bore-transfer-v1";
export const MAX_CONTROL_BYTES = 320 * 1024;
export const MAX_MANIFEST_BYTES = 256 * 1024;
export const MAX_DISPLAY_NAME_CHARS = 48;
export const MAX_PATH_BYTES = 4096;
export const MAX_PATH_SEGMENT_BYTES = 255;

export const CLIENT_TYPES = [
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

export const SERVER_TYPES = [
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

export const ERROR_CODES = [
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

export function clientTypeRequiresRequestId(type) {
  return type !== "hello" && type !== "ping";
}

export function isHex(value, chars) {
  return (
    typeof value === "string" &&
    value.length === chars &&
    /^[0-9a-f]+$/.test(value)
  );
}

function compareCodePoints(a, b) {
  const ai = a[Symbol.iterator]();
  const bi = b[Symbol.iterator]();
  for (;;) {
    const an = ai.next();
    const bn = bi.next();
    if (an.done && bn.done) return 0;
    if (an.done) return -1;
    if (bn.done) return 1;
    const diff = an.value.codePointAt(0) - bn.value.codePointAt(0);
    if (diff !== 0) return diff;
  }
}

// Canonical JSON: code-point key order, no whitespace, safe integers only.
export function canonicalize(value) {
  if (value === null) return "null";
  if (value === true) return "true";
  if (value === false) return "false";
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value)) {
      throw new Error(`no canonical form for number ${value}`);
    }
    return String(value);
  }
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(",")}]`;
  }
  if (typeof value === "object") {
    const keys = Object.keys(value).sort(compareCodePoints);
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalize(value[k])}`).join(",")}}`;
  }
  throw new Error(`no canonical form for ${typeof value}`);
}

function exactObject(value, what, fields) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${what} must be an object`);
  }
  for (const key of Object.keys(value)) {
    if (!fields.includes(key)) {
      throw new Error(`${what} has unknown field ${JSON.stringify(key)}`);
    }
  }
  for (const field of fields) {
    if (!(field in value)) {
      throw new Error(`${what} misses field ${JSON.stringify(field)}`);
    }
  }
  return value;
}

function getString(obj, what, field) {
  if (typeof obj[field] !== "string") {
    throw new Error(`${what} needs string ${JSON.stringify(field)}`);
  }
  return obj[field];
}

function parseEnvelope(raw, known, side) {
  if (raw.length > MAX_CONTROL_BYTES) {
    throw new Error(`${side} control message exceeds 320 KiB`);
  }
  let value;
  try {
    value = JSON.parse(raw);
  } catch (e) {
    throw new Error(`${side} control message is not JSON: ${e.message}`);
  }
  // requestId is optional: only v/type/body are required here.
  const loose = exactObjectOptional(value, side);
  if (loose.v !== PROTOCOL_VERSION) {
    throw new Error(`unsupported control version ${loose.v}`);
  }
  if (typeof loose.type !== "string" || !known.includes(loose.type)) {
    throw new Error(`unknown ${side} control type ${JSON.stringify(loose.type)}`);
  }
  let requestId = null;
  if ("requestId" in value) {
    if (!isHex(value.requestId, 32)) {
      throw new Error("requestId must be 32 lowercase hex chars");
    }
    requestId = value.requestId;
  }
  if (typeof loose.body !== "object" || loose.body === null || Array.isArray(loose.body)) {
    throw new Error(`${side} control body must be an object`);
  }
  return { type: loose.type, requestId, body: loose.body };
}

function exactObjectOptional(value, side) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${side} control message must be an object`);
  }
  const allowed = ["v", "type", "requestId", "body"];
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) {
      throw new Error(`${side} control message has unknown field ${JSON.stringify(key)}`);
    }
  }
  for (const field of ["v", "type", "body"]) {
    if (!(field in value)) {
      throw new Error(`${side} control message misses field ${JSON.stringify(field)}`);
    }
  }
  return value;
}

export function parseClientEnvelope(raw) {
  const env = parseEnvelope(raw, CLIENT_TYPES, "client");
  const needs = clientTypeRequiresRequestId(env.type);
  if (needs && env.requestId === null) {
    throw new Error(`client ${env.type} requires requestId`);
  }
  if (!needs && env.requestId !== null) {
    throw new Error(`client ${env.type} must not carry requestId`);
  }
  return env;
}

export function parseServerEnvelope(raw) {
  const env = parseEnvelope(raw, SERVER_TYPES, "server");
  const needs = env.type === "ack" || env.type === "error";
  if (needs && env.requestId === null) {
    throw new Error(`server ${env.type} must echo requestId`);
  }
  if (!needs && env.requestId !== null) {
    throw new Error(`server ${env.type} must not carry requestId`);
  }
  return env;
}

export function parseDecimalU64(what, s) {
  if (typeof s !== "string" || s.length === 0 || !/^[0-9]+$/.test(s)) {
    throw new Error(`${what} must be a decimal string, got ${JSON.stringify(s)}`);
  }
  if (s.length > 1 && s.startsWith("0")) {
    throw new Error(`${what} must not be zero-padded`);
  }
  const n = BigInt(s);
  if (n > 18446744073709551615n) {
    throw new Error(`${what} overflows u64`);
  }
  return n;
}

export function validateManifestPath(path) {
  if (typeof path !== "string" || path.length === 0) {
    throw new Error("manifest path is empty");
  }
  if (new TextEncoder().encode(path).length > MAX_PATH_BYTES) {
    throw new Error("manifest path exceeds 4096 bytes");
  }
  if (path.startsWith("/") || path.endsWith("/") || path.includes("\\")) {
    throw new Error("manifest path must be relative without backslashes");
  }
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(path)) {
    throw new Error("manifest path holds control characters");
  }
  if (path.normalize("NFC") !== path) {
    throw new Error("manifest path must be NFC-normalized");
  }
  for (const segment of path.split("/")) {
    if (segment === "" || segment === "." || segment === "..") {
      throw new Error("manifest path holds an empty or dot segment");
    }
    if (new TextEncoder().encode(segment).length > MAX_PATH_SEGMENT_BYTES) {
      throw new Error("manifest path segment exceeds 255 bytes");
    }
  }
}

export function validateDisplayName(name) {
  const chars = [...name].length;
  if (chars === 0 || chars > MAX_DISPLAY_NAME_CHARS) {
    throw new Error("display name must be 1..=48 characters");
  }
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(name)) {
    throw new Error("display name holds control characters");
  }
}

export function parseManifest(value) {
  const obj = exactObject(value, "manifest", ["offer", "mode", "entries"]);
  if (!isHex(obj.offer, 32)) {
    throw new Error("manifest offer must be 32 lowercase hex chars");
  }
  if (obj.mode !== "single" && obj.mode !== "multi") {
    throw new Error(`manifest mode must be single|multi, got ${JSON.stringify(obj.mode)}`);
  }
  if (!Array.isArray(obj.entries) || obj.entries.length === 0) {
    throw new Error("manifest needs at least one entry");
  }
  const entries = obj.entries.map((entry) => {
    const e = exactObject(entry, "manifest entry", ["path", "size", "mtime", "chunks"]);
    validateManifestPath(getString(e, "manifest entry", "path"));
    const size = parseDecimalU64("entry size", getString(e, "manifest entry", "size"));
    parseDecimalU64("entry mtime", getString(e, "manifest entry", "mtime"));
    if (!Array.isArray(e.chunks)) {
      throw new Error("manifest entry needs chunks array");
    }
    for (const chunk of e.chunks) {
      if (!isHex(chunk, 64)) {
        throw new Error("chunk hash must be 64 lowercase hex chars");
      }
    }
    const want = size === 0n ? 0 : Number((size + 1048575n) / 1048576n);
    if (e.chunks.length !== want) {
      throw new Error(`entry ${JSON.stringify(e.path)} needs ${want} chunk hashes`);
    }
    return e;
  });
  if (obj.mode === "single" && entries.length !== 1) {
    throw new Error("single manifest needs exactly one entry");
  }
  return { offer: obj.offer, mode: obj.mode, entries };
}

export function manifestValue(manifest) {
  return {
    entries: manifest.entries.map((e) => ({
      chunks: e.chunks,
      mtime: e.mtime,
      path: e.path,
      size: e.size,
    })),
    mode: manifest.mode,
    offer: manifest.offer,
  };
}

export function parseRelayAttach(raw) {
  if (raw.length > MAX_CONTROL_BYTES) {
    throw new Error("relay.attach exceeds 320 KiB");
  }
  let value;
  try {
    value = JSON.parse(raw);
  } catch (e) {
    throw new Error(`relay.attach is not JSON: ${e.message}`);
  }
  const obj = exactObject(value, "relay.attach", [
    "v",
    "peerId",
    "transferId",
    "attemptId",
    "role",
    "ticket",
  ]);
  if (obj.v !== PROTOCOL_VERSION) {
    throw new Error(`unsupported relay.attach version ${obj.v}`);
  }
  for (const [field, chars] of [
    ["peerId", 32],
    ["transferId", 32],
    ["attemptId", 32],
    ["ticket", 32],
  ]) {
    if (!isHex(obj[field], chars)) {
      throw new Error(`relay.attach has a bad ${field}`);
    }
  }
  if (obj.role !== "source" && obj.role !== "recipient") {
    throw new Error(`relay role must be source|recipient, got ${JSON.stringify(obj.role)}`);
  }
  return obj;
}
