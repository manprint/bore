// Web-transfer crypto helpers (protocol v1). WebCrypto only, no transport.
// Mirror of the Rust codecs in src/web_transfer_protocol.rs: every formula,
// domain separator and byte layout must stay identical on both sides.
const textEncoder = new TextEncoder();

function getSubtle() {
  const subtle = globalThis.crypto?.subtle;
  if (subtle === undefined) {
    throw new Error("WebCrypto subtle is unavailable (secure context required)");
  }
  return subtle;
}

export function hexToBytes(hex) {
  if (typeof hex !== "string" || hex.length % 2 !== 0 || !/^[0-9a-f]*$/.test(hex)) {
    throw new Error("need canonical lowercase hex");
  }
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(hex.slice(2 * i, 2 * i + 2), 16);
  }
  return out;
}

export function bytesToHex(bytes) {
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function sha256Hex(data) {
  const digest = await getSubtle().digest("SHA-256", data);
  return bytesToHex(new Uint8Array(digest));
}

export async function hkdf32(ikm, salt, info) {
  const key = await getSubtle().importKey("raw", ikm, "HKDF", false, ["deriveBits"]);
  const bits = await getSubtle().deriveBits(
    { name: "HKDF", hash: "SHA-256", salt, info },
    key,
    256,
  );
  return new Uint8Array(bits);
}

export async function hmacSign(keyBytes, data) {
  const key = await getSubtle().importKey(
    "raw",
    keyBytes,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
  const sig = await getSubtle().sign("HMAC", key, data);
  return new Uint8Array(sig);
}

export async function hmacVerify(keyBytes, data, mac) {
  const key = await getSubtle().importKey(
    "raw",
    keyBytes,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  return getSubtle().verify("HMAC", key, mac, data);
}

// Fixed rolling-root over one entry's chunk leaves (each 32 bytes).
export async function fileRoot(chunkCount, leaves) {
  if (leaves.length !== chunkCount) {
    throw new Error(`root needs exactly ${chunkCount} leaves`);
  }
  const head = new Uint8Array(16 + 8);
  head.set(textEncoder.encode("bore-web-root-v1"), 0);
  new DataView(head.buffer).setBigUint64(16, BigInt(chunkCount), false);
  const parts = [head, ...leaves];
  const total = parts.reduce((n, p) => n + p.length, 0);
  const input = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    input.set(part, at);
    at += part.length;
  }
  return new Uint8Array(await getSubtle().digest("SHA-256", input));
}

export function manifestKey(roomKey, roomId) {
  return hkdf32(roomKey, textEncoder.encode("bore-web-manifest-v1"), roomId);
}

export function attemptKey(roomKey, transferId, attemptId) {
  const info = new Uint8Array(32);
  info.set(transferId, 0);
  info.set(attemptId, 16);
  return hkdf32(roomKey, textEncoder.encode("bore-web-attempt-v1"), info);
}

// GCM nonce for one frame: u64be(seq) || u32be(0).
export function frameNonce(seq) {
  const nonce = new Uint8Array(12);
  new DataView(nonce.buffer).setBigUint64(0, BigInt(seq >>> 0), false);
  return nonce;
}

export const FRAME_MAGIC = 0x42575431;
export const FRAME_MAX_PLAINTEXT = 24 * 1024;
export const FRAME_MAX_BODY = 32 * 1024;
export const FRAME_HEADER_LEN = 16;

function frameHeader(ftype, seq, bodyLen) {
  const header = new Uint8Array(FRAME_HEADER_LEN);
  const view = new DataView(header.buffer);
  view.setUint32(0, FRAME_MAGIC, false);
  view.setUint16(4, 1, false);
  header[6] = ftype;
  header[7] = 0;
  view.setUint32(8, seq >>> 0, false);
  view.setUint32(12, bodyLen >>> 0, false);
  return header;
}

async function importAesKey(key) {
  return getSubtle().importKey("raw", key, "AES-GCM", false, ["encrypt", "decrypt"]);
}

// Frame types: 1 = DATA (1..=24576 plaintext bytes), 2 = FINAL (u64be total).
export async function sealFrame(key, seq, ftype, plaintext) {
  if (ftype === 1 && (plaintext.length === 0 || plaintext.length > FRAME_MAX_PLAINTEXT)) {
    throw new Error("DATA plaintext must be 1..=24576 bytes");
  }
  if (ftype === 2 && plaintext.length !== 8) {
    throw new Error("FINAL plaintext must be u64be(total), 8 bytes");
  }
  if (ftype !== 1 && ftype !== 2) {
    throw new Error(`unknown frame type ${ftype}`);
  }
  const aes = await importAesKey(key);
  const nonce = frameNonce(seq);
  // AES-GCM appends a 16-byte tag, so the header's body_len is known before
  // encrypting; the header itself is the AAD, exactly like the Rust codec.
  const header = frameHeader(ftype, seq, plaintext.length + 16);
  const body = new Uint8Array(
    await getSubtle().encrypt(
      { name: "AES-GCM", iv: nonce, additionalData: header },
      aes,
      plaintext,
    ),
  );
  const out = new Uint8Array(FRAME_HEADER_LEN + body.length);
  out.set(header, 0);
  out.set(body, FRAME_HEADER_LEN);
  return out;
}

export async function openFrame(key, msg, minSeq) {
  if (msg.length < FRAME_HEADER_LEN + 16) {
    throw new Error("frame shorter than header plus tag");
  }
  const header = msg.slice(0, FRAME_HEADER_LEN);
  const body = msg.slice(FRAME_HEADER_LEN);
  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  if (view.getUint32(0, false) !== FRAME_MAGIC) {
    throw new Error("bad frame magic");
  }
  if (view.getUint16(4, false) !== 1) {
    throw new Error("bad frame version");
  }
  const ftype = header[6];
  if (ftype !== 1 && ftype !== 2) {
    throw new Error(`unknown frame type ${ftype}`);
  }
  if (header[7] !== 0) {
    throw new Error("frame reserved bits must be zero");
  }
  const seq = view.getUint32(8, false);
  const bodyLen = view.getUint32(12, false);
  if (bodyLen > FRAME_MAX_BODY) {
    throw new Error("frame body_len exceeds the 32 KiB cap");
  }
  if (bodyLen !== body.length) {
    throw new Error("frame body_len does not match trailing bytes");
  }
  if (seq < minSeq) {
    throw new Error("frame sequence is stale");
  }
  const aes = await importAesKey(key);
  let plaintext;
  try {
    plaintext = new Uint8Array(
      await getSubtle().decrypt(
        { name: "AES-GCM", iv: frameNonce(seq), additionalData: header },
        aes,
        body,
      ),
    );
  } catch {
    throw new Error("frame authentication failed");
  }
  if (ftype === 1 && (plaintext.length === 0 || plaintext.length > FRAME_MAX_PLAINTEXT)) {
    throw new Error("DATA plaintext must be 1..=24576 bytes");
  }
  if (ftype === 2 && plaintext.length !== 8) {
    throw new Error("FINAL plaintext must be u64be(total), 8 bytes");
  }
  return { ftype, seq, plaintext };
}
