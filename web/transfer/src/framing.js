// Encrypted-frame stream planning (protocol v1, relay phase): pure helpers
// turning 1 MiB logical chunks into ≤24 KiB DATA fragments plus the FINAL
// total. Sealing lives in crypto.js; `decodeFrame` below is the strict
// receive side (exact sequence, then AEAD open).

import { FINAL_ARCHIVE_BYTES, openFrame, openFrameWithKey } from "./crypto.js";

/** Logical chunk size: 1 MiB, the manifest's hashing unit. */
export const CHUNK_BYTES = 1024 * 1024;
/** Largest DATA plaintext fragment: 24 KiB (16-byte tag keeps bodies small). */
export const FRAGMENT_BYTES = 24 * 1024;
/** Largest ciphertext message: 16-byte header + fragment + 16-byte tag. */
export const MAX_MESSAGE_BYTES = 32 * 1024;
/** DATA frame type. */
export const FRAME_DATA = 1;
/** FINAL frame type (8-byte u64be total). */
export const FRAME_FINAL = 2;

/**
 * Byte window of one logical chunk.
 * @returns `{ offset, length }` (last chunk may be short).
 */
export function chunkWindow(entrySize, chunkIndex) {
  if (!Number.isSafeInteger(entrySize) || entrySize < 0) {
    throw new Error("entry size must be a safe non-negative integer");
  }
  if (!Number.isSafeInteger(chunkIndex) || chunkIndex < 0) {
    throw new Error("chunk index must be a safe non-negative integer");
  }
  const offset = chunkIndex * CHUNK_BYTES;
  if (offset >= entrySize && entrySize !== 0) {
    throw new Error("chunk index past the entry end");
  }
  return { offset, length: Math.min(CHUNK_BYTES, entrySize - offset) };
}

/**
 * Splits a byte window into fragments of at most `size` bytes (24 KiB by
 * default — the relay's value, and the ceiling on every path). A direct
 * DataChannel passes the size its peer negotiated, which may be smaller;
 * it is never larger, because the receive path is written for this ceiling.
 * @returns array of `{ offset, length }` in order (never empty for a
 * non-empty window; an empty window yields no fragments).
 */
export function fragmentWindow(offset, length, size = FRAGMENT_BYTES) {
  if (!Number.isSafeInteger(offset) || offset < 0) {
    throw new Error("fragment offset must be a safe non-negative integer");
  }
  if (!Number.isSafeInteger(length) || length < 0) {
    throw new Error("fragment length must be a safe non-negative integer");
  }
  if (!Number.isSafeInteger(size) || size <= 0 || size > FRAGMENT_BYTES) {
    throw new Error("fragment size must be a safe integer in 1..=24576");
  }
  const out = [];
  let at = offset;
  let left = length;
  while (left > 0) {
    const take = Math.min(size, left);
    out.push({ offset: at, length: take });
    at += take;
    left -= take;
  }
  return out;
}

/**
 * Entry ID the ARCHIVE reserves for itself.
 *
 * A `zip` transfer carries one synthetic entry — the archive — and it needs
 * an ID no manifest entry can claim, so a staged part file, a resume key or
 * a progress report about the archive is never confused with one about a
 * file. The server refuses this ID in a manifest (`RESERVED_ZIP_ENTRY_ID`,
 * `src/web_transfer_protocol.rs`), which is what makes it safe to use here.
 */
export const ARCHIVE_ENTRY_ID = 0xffff_ffff;

/**
 * Bytes of an archive FINAL payload: total, chunk count, root. Defined in
 * `crypto.js` beside the frame rule that admits it, so the codec and the
 * payload can never disagree about the length.
 */
export const ARCHIVE_FINAL_BYTES = FINAL_ARCHIVE_BYTES;

/**
 * FINAL payload for an ARCHIVE: total length, chunk count and rolling root,
 * none of which exist in the manifest — the archive is generated, so its
 * size and digests are only known once it has been generated. The frame is
 * sealed under the attempt key, so this tuple is authenticated by the same
 * AEAD that carries the data, and the recipient stores it on the first
 * attempt and demands the identical tuple on every later one.
 */
export function archiveFinalPayload(totalBytes, chunkCount, root) {
  if (!Number.isSafeInteger(totalBytes) || totalBytes < 0) {
    throw new Error("total must be a safe non-negative integer");
  }
  if (!Number.isSafeInteger(chunkCount) || chunkCount < 0) {
    throw new Error("chunk count must be a safe non-negative integer");
  }
  if (!(root instanceof Uint8Array) || root.length !== 32) {
    throw new Error("root must be 32 bytes");
  }
  const out = new Uint8Array(ARCHIVE_FINAL_BYTES);
  const view = new DataView(out.buffer);
  view.setBigUint64(0, BigInt(totalBytes), false);
  view.setBigUint64(8, BigInt(chunkCount), false);
  out.set(root, 16);
  return out;
}

/**
 * Reads an archive FINAL payload back. A payload of the wrong length is a
 * protocol error and not a short read: the frame is AEAD-authenticated, so
 * whatever arrived is exactly what the source sealed.
 */
export function parseArchiveFinalPayload(bytes) {
  if (!(bytes instanceof Uint8Array) || bytes.length !== ARCHIVE_FINAL_BYTES) {
    throw new Error(`archive FINAL payload must be ${ARCHIVE_FINAL_BYTES} bytes`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const totalBytes = Number(view.getBigUint64(0, false));
  const chunkCount = Number(view.getBigUint64(8, false));
  if (!Number.isSafeInteger(totalBytes) || !Number.isSafeInteger(chunkCount)) {
    throw new Error("archive FINAL payload is out of range");
  }
  return { totalBytes, chunkCount, root: bytes.slice(16) };
}

/** 8-byte big-endian total for the FINAL frame. */
export function finalPayload(totalBytes) {
  if (!Number.isSafeInteger(totalBytes) || totalBytes < 0) {
    throw new Error("total must be a safe non-negative integer");
  }
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(totalBytes), false);
  return out;
}

/**
 * Strict next-frame decoder for the receive path: the sequence must equal
 * `expectedSeq` exactly (a gap means lost ciphertext and aborts the
 * attempt), then the full AEAD open authenticates header and body.
 */
export async function decodeFrame(key, message, expectedSeq) {
  const opened = await openFrame(key, message, expectedSeq);
  if (opened.seq !== expectedSeq) {
    throw new Error(`frame sequence gap: want ${expectedSeq}, got ${opened.seq}`);
  }
  return opened;
}

/**
 * Strict next-frame decoder over an already-imported (possibly
 * non-extractable) key: exact sequence, then AEAD open.
 */
export async function decodeFrameWithKey(aesKey, message, expectedSeq) {
  const opened = await openFrameWithKey(aesKey, message, expectedSeq);
  if (opened.seq !== expectedSeq) {
    throw new Error(`frame sequence gap: want ${expectedSeq}, got ${opened.seq}`);
  }
  return opened;
}

/**
 * Send plan for one entry: which chunk indexes travel on the wire and which
 * are only rehashed (resume skips). `verifiedRanges` holds
 * `[startChunk, endChunkExclusive)` pairs in chunk units.
 * @returns `{ send: number[], rehashOnly: number[] }`, both ascending.
 */
export function planSend(chunkCount, verifiedRanges) {
  if (!Number.isSafeInteger(chunkCount) || chunkCount < 0) {
    throw new Error("chunk count must be a safe non-negative integer");
  }
  const verified = new Set();
  for (const range of verifiedRanges ?? []) {
    if (!Array.isArray(range) || range.length !== 2) {
      throw new Error("resume range must be a [start, end) pair");
    }
    const [start, end] = range;
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) ||
      start < 0 ||
      end < start ||
      end > chunkCount
    ) {
      throw new Error("resume range out of bounds");
    }
    for (let i = start; i < end; i++) {
      verified.add(i);
    }
  }
  const send = [];
  const rehashOnly = [];
  for (let i = 0; i < chunkCount; i++) {
    (verified.has(i) ? rehashOnly : send).push(i);
  }
  return { send, rehashOnly };
}
