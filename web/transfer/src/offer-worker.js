// Offer preparation worker: deterministic manifest hashing off the main
// thread. The exported `prepareOffer` is UI-free and AbortController-driven
// so Node unit tests drive it directly with duck-typed files; the
// `onmessage` wrapper below only exists inside real workers.
//
// Rules (mirrored server-side, which re-checks every one of them):
// one picker/drop action is one offer; the selection arrives already walked
// (see `folders.js`) as files and, for a folder, the EMPTY directories that
// the file paths cannot imply; 1 MiB slices read sequentially per file with
// at most two files hashing concurrently; paths NFC, relative,
// collision-checked before any publish; WebCrypto only.
import {
  canonicalize,
  manifestValue,
  parseManifest,
  validateManifestPath,
} from "./protocol.js";
import { bytesToHex, fileRoot, hexToBytes, manifestMac, sha256Hex } from "./crypto.js";

export const WORKER_SLICE_BYTES = 1024 * 1024;
export const WORKER_MAX_CONCURRENT_FILES = 2;
export const WORKER_MAX_MANIFEST_BYTES = 256 * 1024;

export function workerEntryRelativePath(file) {
  const relative =
    typeof file.webkitRelativePath === "string" && file.webkitRelativePath !== ""
      ? file.webkitRelativePath
      : file.name;
  return relative.startsWith("/") ? relative.slice(1) : relative;
}

function abortError() {
  const error = new Error("offer preparation aborted");
  error.aborted = true;
  return error;
}

function checkAborted(signal) {
  if (signal !== null && signal !== undefined && signal.aborted) {
    throw abortError();
  }
}

async function hashFileChunks(file, size, signal, onBytes) {
  const hexes = [];
  let offset = 0;
  while (offset < size) {
    checkAborted(signal);
    const end = Math.min(offset + WORKER_SLICE_BYTES, size);
    const bytes = new Uint8Array(await file.slice(offset, end).arrayBuffer());
    if (bytes.length !== end - offset) {
      throw new Error("short file read during hashing");
    }
    offset = end;
    onBytes(bytes.length);
    hexes.push(await sha256Hex(bytes));
    checkAborted(signal);
  }
  return hexes;
}

async function mapPool(items, limit, signal, fn) {
  const results = new Array(items.length);
  let next = 0;
  async function pump() {
    for (;;) {
      checkAborted(signal);
      const index = next;
      next += 1;
      if (index >= items.length) {
        return;
      }
      results[index] = await fn(items[index], index);
    }
  }
  const lanes = Math.min(limit, Math.max(items.length, 1));
  await Promise.all(Array.from({ length: lanes }, pump));
  checkAborted(signal);
  return results;
}

/**
 * Builds a signed offer manifest from selected files.
 * @param {object} job `{ offerId, kind, label, createdAt, files, roomIdHex, roomKeyHex, limits }`
 * - `files`: `[{ file, relativePath }]` for a file, or
 *   `[{ directory: true, relativePath, mtimeSec }]` for an empty directory;
 *   `file` duck-typed (`name/size/lastModified/slice/arrayBuffer`), read but
 *   never stored. A directory entry is hashed not at all: size 0, no chunks
 *   and a null root, which is the shape the server accepts only for a
 *   `folder` offer.
 * - `limits`: `{ maxEntriesPerOffer, maxOfferBytes }` (checked before
 *   hashing and again after serialization).
 * - `signal`: AbortSignal; `onProgress({ offerId, doneBytes, totalBytes })`.
 * @returns `{ offerId, manifest, manifestCanonical, macHex, observed }`
 * with per-entry `{ path, size, mtimeSec }` for freshness checks.
 */
export async function prepareOffer({
  offerId,
  kind,
  label,
  createdAt,
  files,
  roomIdHex,
  roomKeyHex,
  limits,
  signal = null,
  onProgress = null,
}) {
  if (!Array.isArray(files) || files.length === 0) {
    throw new Error("offer needs at least one file");
  }
  if (files.length > limits.maxEntriesPerOffer) {
    throw new Error("offer exceeds the per-offer entry cap");
  }
  // Validate and sort paths before any hashing; collisions fail here.
  const pending = files.map(({ file, relativePath, directory = false, mtimeSec = 0 }) => {
    const path =
      typeof relativePath === "string" ? relativePath : workerEntryRelativePath(file);
    validateManifestPath(path);
    if (directory === true) {
      if (file !== undefined && file !== null) {
        throw new Error("a directory entry carries no file");
      }
      return { file: null, path, directory: true, mtimeSec };
    }
    return { file, path, directory: false, mtimeSec: 0 };
  });
  pending.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  {
    let previous = null;
    for (const { path } of pending) {
      const fold = path.normalize("NFC").toLowerCase();
      if (previous !== null && fold <= previous) {
        throw new Error("offer entries must be sorted with no path collisions");
      }
      previous = fold;
    }
  }
  let totalBytes = 0n;
  for (const { file, directory } of pending) {
    totalBytes += directory ? 0n : BigInt(file.size);
  }
  if (totalBytes > BigInt(limits.maxOfferBytes)) {
    throw new Error("offer exceeds the per-offer byte cap");
  }
  const totalNumber = Number(totalBytes);
  let doneBytes = 0;
  const hashed = await mapPool(pending, WORKER_MAX_CONCURRENT_FILES, signal, async (entry) => {
    const { file, path, directory, mtimeSec: dirMtime } = entry;
    if (directory === true) {
      // Nothing to read and nothing to hash: the entry IS the statement that
      // this directory exists and holds nothing.
      return { path, size: 0, mtimeSec: dirMtime, chunkHexes: [], root: null };
    }
    const size = file.size;
    const mtimeSec = Math.floor(file.lastModified / 1000);
    const chunkHexes = await hashFileChunks(file, size, signal, (bytes) => {
      doneBytes += bytes;
      onProgress?.({ offerId, doneBytes, totalBytes: totalNumber });
    });
    const leaves = chunkHexes.map((hex) => hexToBytes(hex));
    const root = bytesToHex(await fileRoot(leaves.length, leaves));
    checkAborted(signal);
    return { path, size, mtimeSec, chunkHexes, root };
  });
  const mode = kind === "file" ? "single" : "multi";
  const manifest = {
    offer: offerId,
    mode,
    label,
    kind,
    chunkSize: String(WORKER_SLICE_BYTES),
    createdAt,
    entries: hashed.map(({ path, size, mtimeSec, chunkHexes, root }, position) => ({
      id: String(position),
      path,
      size: String(size),
      mtime: String(mtimeSec),
      chunks: chunkHexes,
      chunkCount: String(chunkHexes.length),
      root,
    })),
  };
  // Shape-check our own output through the shared validator (same rules the
  // server enforces), then authenticate. Root equality itself is verified by
  // the server at publish from these very hashes.
  parseManifest(manifest, limits);
  const canonical = canonicalize(manifestValue(manifest));
  if (new TextEncoder().encode(canonical).length > WORKER_MAX_MANIFEST_BYTES) {
    throw new Error("offer exceeds the manifest byte cap");
  }
  const mac = await manifestMac(hexToBytes(roomKeyHex), hexToBytes(roomIdHex), new TextEncoder().encode(canonical));
  checkAborted(signal);
  return {
    offerId,
    manifest,
    manifestCanonical: canonical,
    macHex: bytesToHex(mac),
    // A directory is marked, because a freshness check has nothing to
    // re-read for one and must not read its absence as a changed file.
    observed: hashed.map(({ path, size, mtimeSec, root }) => ({
      path,
      size,
      mtimeSec,
      directory: root === null,
    })),
  };
}

const pendingAborts = new Map();

if (typeof self !== "undefined" && typeof self.postMessage === "function") {
  self.onmessage = async (event) => {
    const job = event?.data;
    if (job === null || typeof job !== "object") {
      return;
    }
    if (job.type === "abort") {
      pendingAborts.get(job.offerId)?.abort();
      return;
    }
    if (job.type !== "prepare") {
      return;
    }
    const controller = new AbortController();
    pendingAborts.set(job.offerId, controller);
    const post =
      job.progress === true ? (progress) => self.postMessage({ type: "progress", ...progress }) : null;
    try {
      const result = await prepareOffer({ ...job, signal: controller.signal, onProgress: post });
      self.postMessage({
        type: "done",
        offerId: result.offerId,
        manifest: result.manifest,
        manifestCanonical: result.manifestCanonical,
        macHex: result.macHex,
        observed: result.observed,
      });
    } catch (error) {
      if (error?.aborted === true || controller.signal.aborted) {
        self.postMessage({ type: "aborted", offerId: job.offerId });
      } else {
        self.postMessage({ type: "error", offerId: job.offerId, message: String(error?.message ?? error) });
      }
    } finally {
      pendingAborts.delete(job.offerId);
    }
  };
}
