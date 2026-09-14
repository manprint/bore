// Web-transfer protocol v1: canonical JSON, manifest and control envelopes.
// Mirror of src/web_transfer_protocol.rs — same names, same rules, same
// rejections. Crypto primitives (HKDF/HMAC/roots/frames) live in crypto.js.
export const PROTOCOL_VERSION = 1;
export const CONTROL_SUBPROTOCOL = "bore-transfer-v1";
export const MAX_CONTROL_BYTES = 320 * 1024;
export const MAX_MANIFEST_BYTES = 256 * 1024;
export const MAX_DISPLAY_NAME_CHARS = 48;
export const MAX_LABEL_CHARS = 128;
export const MAX_PATH_BYTES = 4096;
export const MAX_PATH_SEGMENT_BYTES = 255;
export const CHUNK_BYTES = 1024 * 1024;

// Documented admission defaults, mirrored from WebTransferLimits::default.
// parseManifest accepts overrides so tests pin small caps.
export const DEFAULT_MANIFEST_LIMITS = {
  maxEntriesPerOffer: 10000,
  maxOfferBytes: 1099511627776,
};

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
  // `ack` echoes the mutation's requestId; `error` echoes it when the
  // offending message carried one (hello/ping failures travel without it).
  if (env.type === "ack" && env.requestId === null) {
    throw new Error("server ack must echo requestId");
  }
  if (env.type !== "ack" && env.type !== "error" && env.requestId !== null) {
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

export function validateLabel(label) {
  if (typeof label !== "string") {
    throw new Error("manifest label must be a string");
  }
  const trimmed = label.normalize("NFC").trim();
  if (trimmed.length === 0) {
    throw new Error("manifest label must not be empty");
  }
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f]/.test(trimmed)) {
    throw new Error("manifest label must not carry control characters");
  }
  if ([...trimmed].length > MAX_LABEL_CHARS) {
    throw new Error("manifest label exceeds 128 characters");
  }
  return trimmed;
}

export function validateCreatedAt(s) {
  if (typeof s !== "string" || s.length === 0 || s.length > 32) {
    throw new Error("manifest createdAt must be ASCII within 32 bytes");
  }
  if (!/^[\x20-\x7e]+$/.test(s) || !s.endsWith("Z") || !s.includes("T")) {
    throw new Error("manifest createdAt must look like 2026-09-14T21:00:00Z");
  }
  const inner = s.slice(0, -1);
  const sep = inner.indexOf("T");
  const date = inner.slice(0, sep);
  const time = inner.slice(sep + 1);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(date) || !/^\d{2}:\d{2}:\d{2}(\.\d{1,9})?$/.test(time)) {
    throw new Error("manifest createdAt must look like 2026-09-14T21:00:00Z");
  }
  const month = Number(date.slice(5, 7));
  const day = Number(date.slice(8, 10));
  const hour = Number(time.slice(0, 2));
  const minute = Number(time.slice(3, 5));
  const second = Number(time.slice(6, 8));
  if (month < 1 || month > 12 || day < 1 || day > 31 || hour > 23 || minute > 59 || second > 59) {
    throw new Error("manifest createdAt carries an impossible date or time");
  }
}

function foldPath(path) {
  return path.normalize("NFC").toLowerCase();
}

export function parseManifest(value, limits = DEFAULT_MANIFEST_LIMITS) {
  const obj = exactObject(value, "manifest", [
    "offer",
    "mode",
    "label",
    "kind",
    "chunkSize",
    "createdAt",
    "entries",
  ]);
  if (!isHex(obj.offer, 32)) {
    throw new Error("manifest offer must be 32 lowercase hex chars");
  }
  if (obj.mode !== "single" && obj.mode !== "multi") {
    throw new Error(`manifest mode must be single|multi, got ${JSON.stringify(obj.mode)}`);
  }
  const label = validateLabel(getString(obj, "manifest", "label"));
  if (obj.kind !== "file" && obj.kind !== "files" && obj.kind !== "folder") {
    throw new Error(`manifest kind must be file|files|folder, got ${JSON.stringify(obj.kind)}`);
  }
  const chunkSize = parseDecimalU64("manifest chunkSize", getString(obj, "manifest", "chunkSize"));
  if (chunkSize !== BigInt(CHUNK_BYTES)) {
    throw new Error(`manifest chunkSize must be ${CHUNK_BYTES}`);
  }
  validateCreatedAt(getString(obj, "manifest", "createdAt"));
  if (!Array.isArray(obj.entries) || obj.entries.length === 0) {
    throw new Error("manifest needs at least one entry");
  }
  if (obj.entries.length > limits.maxEntriesPerOffer) {
    throw new Error("manifest exceeds the per-offer entry cap");
  }
  let total = 0n;
  let prevFold = null;
  const entries = obj.entries.map((entry, position) => {
    const e = exactObject(entry, "manifest entry", [
      "id",
      "path",
      "size",
      "mtime",
      "chunks",
      "chunkCount",
      "root",
    ]);
    if (parseDecimalU64("entry id", getString(e, "manifest entry", "id")) !== BigInt(position)) {
      throw new Error("manifest entry IDs must be 0-based sequential");
    }
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
    const want = size === 0n ? 0 : Number((size + BigInt(CHUNK_BYTES - 1)) / BigInt(CHUNK_BYTES));
    if (e.chunks.length !== want) {
      throw new Error(`entry ${JSON.stringify(e.path)} needs ${want} chunk hashes`);
    }
    if (parseDecimalU64("entry chunkCount", getString(e, "manifest entry", "chunkCount")) !== BigInt(want)) {
      throw new Error(`entry ${JSON.stringify(e.path)} chunkCount must equal its chunk hash count`);
    }
    // Directories carry a null root with empty chunks; files carry a hex
    // root. Root equality against the rolling root is verified by the server
    // at publish and by the browser at download (async fileRoot) — never
    // here, where no async crypto runs.
    if (e.root === null) {
      if (e.chunks.length !== 0 || size !== 0n) {
        throw new Error(`entry ${JSON.stringify(e.path)} with null root must be an empty directory`);
      }
      if (obj.kind !== "folder") {
        throw new Error(`entry ${JSON.stringify(e.path)} directory needs kind folder`);
      }
    } else if (!isHex(e.root, 64)) {
      throw new Error(`entry ${JSON.stringify(e.path)} root must be hex or null`);
    }
    total += size;
    const fold = foldPath(e.path);
    if (prevFold !== null && fold <= prevFold) {
      throw new Error("manifest entries must be sorted with no path collisions");
    }
    prevFold = fold;
    return e;
  });
  if (total > BigInt(limits.maxOfferBytes)) {
    throw new Error("manifest exceeds the per-offer byte cap");
  }
  if (obj.mode === "single" && entries.length !== 1) {
    throw new Error("single manifest needs exactly one entry");
  }
  if (obj.kind === "file" && (obj.mode !== "single" || entries.length !== 1 || entries[0].root === null)) {
    throw new Error("kind file needs one single file entry");
  }
  if (obj.kind === "files" && (obj.mode !== "multi" || entries.some((e) => e.root === null))) {
    throw new Error("kind files needs multiple file entries");
  }
  if (obj.kind === "folder" && obj.mode !== "multi") {
    throw new Error("kind folder needs multi mode");
  }
  return {
    offer: obj.offer,
    mode: obj.mode,
    label,
    kind: obj.kind,
    chunkSize: obj.chunkSize,
    createdAt: obj.createdAt,
    entries,
  };
}

export function manifestValue(manifest) {
  return {
    chunkSize: manifest.chunkSize,
    createdAt: manifest.createdAt,
    entries: manifest.entries.map((e) => ({
      chunkCount: e.chunkCount,
      chunks: e.chunks,
      id: e.id,
      mtime: e.mtime,
      path: e.path,
      root: e.root,
      size: e.size,
    })),
    kind: manifest.kind,
    label: manifest.label,
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
