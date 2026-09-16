// OPFS + IndexedDB download repository (protocol v1, relay phase).
// Transactional receiving without secrets: OPFS holds verified chunk bytes,
// IndexedDB holds the resume record. Manifests, keys and tokens never touch
// either store. All browser entry points are injected so unit tests run on
// fakes; production passes the real `navigator`/`indexedDB` globals.
//
// Staging layout: one file per verified chunk, `<entryId>.<chunkIndex>.part`.
// A chunk is durable the moment its writable closes, and writing chunk N
// costs one chunk. The alternative — one output file rewritten with
// `createWritable({ keepExistingData: true })` — makes every engine copy the
// whole output per chunk, which is quadratic: measured over 128 × 1 MiB,
// 7021 ms (chromium) / 4156 ms (firefox) / 5464 ms (webkit) against
// 238 / 1374 / 157 ms here. The finished download is handed out as a `Blob`
// composed of the part files in order, which costs 9-17 ms and no extra
// bytes on disk, so there is no assembly pass to crash halfway through.

import { perfEnd, perfNoStageWorker, perfStart } from "./perf.js";

/** IndexedDB database / store for resume records. */
export const PARTIALS_DB = "bore-transfer-v1";
export const PARTIALS_STORE = "partials";
/** OPFS root directory for staged parts. */
export const OPFS_ROOT = "bore-transfer-v1";
/** Largest resume descriptor the wire accepts (server-enforced too). */
export const MAX_RESUME_RANGES = 4096;
/** Quota headroom: size + min(64 MiB, 5% of size). */
export const QUOTA_HEADROOM_MIN = 64n * 1024n * 1024n;
/** Bundled staging worker (3.11); same fixed asset names as the shell. */
export const STAGE_WORKER_URL = "/transfer/assets/stage-worker.js";
/**
 * Startup handshake budget for the staging worker. A worker that has not
 * said hello by then is treated as absent: the main-thread path still works,
 * so waiting longer would only delay the first chunk.
 */
export const STAGE_WORKER_HELLO_MS = 3000;

/** True for canonical lowercase hex (room/offer/digest segments). */
export function isHexSegment(value) {
  return typeof value === "string" && value.length > 0 && /^[0-9a-f]+$/.test(value);
}

/**
 * OPFS directory segments holding one selection's parts. Every segment is
 * generated hex — manifest paths never become filesystem paths, so a hostile
 * manifest cannot escape the staging tree.
 */
export function partDirSegments(roomId, offerId, selectionDigest) {
  for (const segment of [roomId, offerId, selectionDigest]) {
    if (!isHexSegment(segment)) {
      throw new Error("partial path needs hex segments");
    }
  }
  return [OPFS_ROOT, roomId, offerId, selectionDigest];
}

/** File name of one staged chunk: numeric entry ID and chunk index only. */
export function chunkPartName(entryId, chunkIndex) {
  if (!/^[0-9]+$/.test(String(entryId))) {
    throw new Error("partial path needs a numeric entry ID");
  }
  if (!Number.isSafeInteger(chunkIndex) || chunkIndex < 0) {
    throw new Error("partial path needs a non-negative chunk index");
  }
  return `${entryId}.${chunkIndex}.part`;
}

/** IndexedDB key for one resume record (structured array, never a URL). */
export function partialKey(roomId, sourcePeerId, offerId, selectionDigest) {
  return [roomId, sourcePeerId, offerId, selectionDigest];
}

/** Sorts and coalesces `[start, end)` chunk ranges (numbers, disjoint). */
export function coalesceRanges(ranges) {
  const sorted = [...(ranges ?? [])].sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const out = [];
  for (const [start, end] of sorted) {
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end < start) {
      throw new Error("resume range must be a safe [start, end) pair");
    }
    const last = out[out.length - 1];
    if (last !== undefined && start <= last[1]) {
      last[1] = Math.max(last[1], end);
    } else {
      out.push([start, end]);
    }
  }
  return out;
}

/**
 * Bounds ranges for the wire: at most `MAX_RESUME_RANGES`, reduced to the
 * stable contiguous prefix from chunk 0 when over the cap (the sender
 * re-sends everything past the prefix).
 */
export function capRanges(ranges, max = MAX_RESUME_RANGES) {
  const merged = coalesceRanges(ranges);
  if (merged.length <= max) {
    return merged;
  }
  const prefix = [];
  let next = 0;
  for (const [start, end] of merged) {
    if (start !== next) {
      break;
    }
    prefix.push([start, end]);
    next = end;
  }
  return prefix;
}

/**
 * The contiguous verified PREFIX of a range list: `[[0, n]]`, or `[]` when
 * chunk 0 is missing.
 *
 * An ARCHIVE resumes only on a prefix. Its chunks are not in any manifest,
 * so what identifies chunk `i` is its position in a stream the source
 * regenerates from byte zero; the recipient stages them in arrival order
 * and rebuilds the rolling root by concatenating leaves in index order.
 * A hole in the middle would leave `leaves` with a gap it cannot fill
 * without receiving the missing chunk anyway, so a sparse archive resume
 * buys nothing and costs the one invariant the root depends on. A raw file
 * keeps its sparse ranges: there every chunk has a manifest digest, so any
 * subset of them is independently verifiable.
 */
export function verifiedPrefix(ranges) {
  const merged = coalesceRanges(ranges);
  if (merged.length === 0 || merged[0][0] !== 0 || merged[0][1] === 0) {
    return [];
  }
  return [[0, merged[0][1]]];
}

/** Flattens `[start, end)` ranges into ascending chunk indexes. */
export function rangesToIndexes(ranges) {
  const out = [];
  for (const [start, end] of coalesceRanges(ranges)) {
    for (let i = start; i < end; i++) {
      out.push(i);
    }
  }
  return out;
}

/**
 * Pure quota verdict with BigInt math. `estimate` is `{ quota, usage }` in
 * bytes (either may be undefined when the browser withholds them — then the
 * check passes as unknown rather than blocking every download).
 */
export function checkQuotaAgainst(estimate, entrySize) {
  const size = BigInt(entrySize);
  const headroom = QUOTA_HEADROOM_MIN < size / 20n ? QUOTA_HEADROOM_MIN : size / 20n;
  const need = size + headroom;
  const quota = estimate?.quota;
  const usage = estimate?.usage;
  if (typeof quota !== "number" || typeof usage !== "number") {
    return { ok: true, unknown: true, need };
  }
  const free = BigInt(Math.max(0, Math.floor(quota) - Math.floor(usage)));
  if (free < need) {
    return { ok: false, need, free };
  }
  return { ok: true, need, free };
}

/**
 * Sanitizes a manifest path for the download attribute: basename only, no
 * controls or separators, bounded length, never empty.
 */
export function sanitizeDownloadName(path, fallback = "download.bin") {
  const base = String(path ?? "").split("/").filter(Boolean).pop() ?? "";
  const clean = base.replace(/[\0-\x1f\x7f\\/:*?"<>|]/g, "").trim().slice(0, 255);
  return clean === "" || clean === "." || clean === ".." ? fallback : clean;
}

function idbRequest(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error("indexeddb failed"));
  });
}

/**
 * @param {object} options injected browser entry points (all optional;
 * defaults read the live globals, so tests pass fakes)
 * @param {() => Promise<object>} options.getDirectory OPFS root factory
 * @param {(name: string, version: number) => IDBOpenDBRequest} options.openIDB
 * @param {() => Promise<{quota?: number, usage?: number}>} options.estimateStorage
 * @param {() => object|null} options.createStageWorker staging-worker factory
 * (see `stage-worker.js`); `null` or a throw keeps every write on this thread
 */
export function createRepository(options = {}) {
  const getDirectory =
    options.getDirectory ??
    (() => globalThis.navigator?.storage?.getDirectory());
  const openIDB =
    options.openIDB ??
    ((name, version) => globalThis.indexedDB?.open(name, version));
  const estimateStorage =
    options.estimateStorage ?? (() => globalThis.navigator?.storage?.estimate());
  const createStageWorker =
    options.createStageWorker ??
    (() =>
      typeof globalThis.Worker === "function" && !perfNoStageWorker()
        ? new globalThis.Worker(STAGE_WORKER_URL, { type: "module" })
        : null);

  // Staging worker (3.11): `createSyncAccessHandle` exists only inside a
  // worker, and it is what removes the recipient's dominant remaining cost.
  // `off` is terminal for the repository's life — an engine that cannot do
  // this once will not start doing it mid-download, and retrying per chunk
  // would pay the startup handshake again for nothing.
  let stage = null;
  let stageState = "unknown";
  let stageBoot = null;

  function stopStage(nextState) {
    stageState = nextState;
    stageBoot = null;
    const worker = stage?.worker;
    stage = null;
    try {
      worker?.terminate();
    } catch {
      /* already gone */
    }
  }

  /** Terminal for this repository: never try the worker again. */
  function disableStage() {
    stopStage("off");
  }

  /**
   * Resolves to the worker bridge, or `null` when this engine (or this
   * build) has no usable one. The handshake runs once; every later call
   * returns the same answer.
   */
  function stageWorker() {
    if (stageState === "off") {
      return Promise.resolve(null);
    }
    if (stageState === "ready") {
      return Promise.resolve(stage);
    }
    if (stageBoot !== null) {
      return stageBoot;
    }
    stageBoot = new Promise((resolve) => {
      let worker = null;
      try {
        worker = createStageWorker();
      } catch {
        worker = null;
      }
      if (worker === null || worker === undefined) {
        disableStage();
        resolve(null);
        return;
      }
      const pending = new Map();
      const bridge = { worker, pending, seq: 0 };
      let settled = false;
      const giveUp = () => {
        if (settled) {
          // A failure AFTER the handshake: reject what is in flight, and
          // stay off. The caller retries that chunk on this thread.
          for (const entry of pending.values()) {
            entry.fail(new Error("staging worker failed"));
          }
          pending.clear();
          disableStage();
          return;
        }
        settled = true;
        disableStage();
        resolve(null);
      };
      const timer = setTimeout(giveUp, STAGE_WORKER_HELLO_MS);
      worker.onerror = giveUp;
      worker.onmessageerror = giveUp;
      worker.onmessage = (event) => {
        const data = event?.data ?? {};
        if (data.hello === true) {
          clearTimeout(timer);
          if (settled) {
            return;
          }
          settled = true;
          if (data.sync !== true) {
            // The worker started but the engine has no sync access handles:
            // the main-thread path IS the definition of correctness, so use
            // it rather than a worker that buys nothing.
            disableStage();
            resolve(null);
            return;
          }
          stage = bridge;
          stageState = "ready";
          resolve(bridge);
          return;
        }
        const settle = pending.get(data.id);
        if (settle === undefined) {
          return;
        }
        pending.delete(data.id);
        settle.done(data);
      };
    });
    return stageBoot;
  }

  // One connection for the repository's life: opening and closing the
  // database around every operation costs three connection cycles per
  // verified chunk (measured 41 / 567 / 439 ms per 128 chunks on
  // chromium / firefox / webkit) for nothing.
  let connection = null;
  let opening = null;

  /**
   * Capability probe: OPFS root + WebCrypto + IndexedDB open, using the
   * injected factories (so fakes count). Opening creates an empty database
   * at most — no records, no bytes.
   */
  async function supported() {
    try {
      if (typeof globalThis.crypto?.subtle?.digest !== "function") {
        return false;
      }
      await getDirectory();
      await db();
      return true;
    } catch {
      return false;
    }
  }

  async function db() {
    if (connection !== null) {
      return connection;
    }
    if (opening === null) {
      opening = (async () => {
        const open = openIDB(PARTIALS_DB, 1);
        if (!open || typeof open.then === "function") {
          throw new Error("indexeddb unavailable");
        }
        return new Promise((resolve, reject) => {
          open.onupgradeneeded = () => {
            try {
              open.result.createObjectStore(PARTIALS_STORE);
            } catch {
              /* exists after a raced upgrade */
            }
          };
          open.onsuccess = () => resolve(open.result);
          open.onerror = () => reject(open.error ?? new Error("indexeddb open failed"));
          open.onblocked = () => reject(new Error("indexeddb blocked"));
        });
      })().then(
        (database) => {
          connection = database;
          opening = null;
          // A version change from another tab invalidates this handle.
          if (typeof database.addEventListener === "function") {
            database.addEventListener("close", () => {
              connection = null;
            });
          }
          return database;
        },
        (error) => {
          opening = null;
          throw error;
        },
      );
    }
    return opening;
  }

  async function withStore(mode, fn) {
    const database = await db();
    const tx = database.transaction(PARTIALS_STORE, mode);
    const result = await fn(tx.objectStore(PARTIALS_STORE));
    await new Promise((resolve, reject) => {
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error ?? new Error("indexeddb transaction failed"));
      tx.onabort = () => reject(tx.error ?? new Error("indexeddb transaction aborted"));
    });
    return result;
  }

  async function navigate(segments, create) {
    let dir = await getDirectory();
    for (const segment of segments) {
      dir = await dir.getDirectoryHandle(segment, { create });
    }
    return dir;
  }

  return {
    supported,

    /** Closes the shared database handle and the staging worker. */
    close() {
      try {
        connection?.close();
      } catch {
        /* best effort */
      }
      connection = null;
      // Not `disableStage`: a repository reopened after a purge must be able
      // to start a worker again, or the tab silently loses the fast path.
      stopStage("unknown");
    },

    /** Quota verdict for `entrySize` bytes (string or bigint). */
    async quota(entrySize) {
      const estimate = await estimateStorage().catch(() => ({}));
      return checkQuotaAgainst(estimate ?? {}, BigInt(entrySize));
    },

    async loadRecord(key) {
      return withStore("readonly", (store) => idbRequest(store.get(key)));
    },

    async saveRecord(key, record) {
      await withStore("readwrite", (store) => idbRequest(store.put(record, key)));
    },

    async deleteRecord(key) {
      await withStore("readwrite", (store) => idbRequest(store.delete(key)));
    },

    async listKeys() {
      return withStore("readonly", (store) => idbRequest(store.getAllKeys()));
    },

    /**
     * Writes one verified chunk as its own part file (durable on close).
     *
     * Prefers the staging worker (`createSyncAccessHandle`, 3.11) and falls
     * back to this thread whenever the worker is absent, refuses the
     * capability or fails — the main-thread path is the definition of
     * correctness and both paths write the same bytes to the same name.
     * `bytes` is TRANSFERRED when the view owns its whole buffer, so the
     * chunk crosses without a copy; a handled worker error hands the buffer
     * back, so the fallback below still has bytes to write. Only a worker
     * that dies outright loses them, and that failure is the ordinary
     * transfer error the recipient already knows how to resume from.
     */
    async writeChunk(dirSegments, name, bytes) {
      const bridge = await stageWorker();
      if (bridge !== null) {
        const stageAt = perfStart();
        let usable = bytes;
        try {
          usable = await new Promise((resolve, reject) => {
            bridge.seq += 1;
            const id = bridge.seq;
            bridge.pending.set(id, {
              done: (data) => {
                if (data.ok === true) {
                  resolve(null);
                  return;
                }
                const error = new Error(data.error ?? "staging worker refused a chunk");
                error.bytes = data.bytes ?? null;
                reject(error);
              },
              fail: reject,
            });
            // A view over a larger buffer cannot be transferred without
            // taking its neighbours with it, so it is cloned instead.
            const owns = bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength;
            bridge.worker.postMessage(
              { id, dirSegments, name, bytes },
              owns ? [bytes.buffer] : [],
            );
          });
          perfEnd("opfs.sync", stageAt, bytes.byteLength);
          return;
        } catch (error) {
          // Whatever went wrong, this chunk still has to land: fall through
          // to the path that has always worked, and stop using the worker.
          disableStage();
          const returned = error?.bytes ?? null;
          if (returned === null) {
            throw error;
          }
          usable = returned;
        }
        bytes = usable;
      }
      const openAt = perfStart();
      const dir = await navigate(dirSegments, true);
      const handle = await dir.getFileHandle(name, { create: true });
      const writable = await handle.createWritable();
      perfEnd("opfs.open", openAt);
      try {
        const writeAt = perfStart();
        await writable.write(bytes);
        perfEnd("opfs.write", writeAt, bytes.length);
      } finally {
        // Close persists before the IDB commit lands: crash-safe order.
        const closeAt = perfStart();
        await writable.close();
        perfEnd("opfs.close", closeAt);
      }
    },

    /** Reads one staged chunk back, or null when it is gone. */
    async readChunk(dirSegments, name) {
      try {
        const dir = await navigate(dirSegments, false);
        const handle = await dir.getFileHandle(name, { create: false });
        const file = await handle.getFile();
        return new Uint8Array(await file.arrayBuffer());
      } catch {
        return null;
      }
    },

    /**
     * The finished download: a Blob composed of the ordered part files. The
     * parts stay on disk and are read on demand, so this costs no copy and
     * no second output file.
     */
    async stagedBlob(dirSegments, names) {
      const dir = await navigate(dirSegments, false);
      const parts = [];
      for (const name of names) {
        const handle = await dir.getFileHandle(name, { create: false });
        parts.push(await handle.getFile());
      }
      return new Blob(parts);
    },

    /** Removes one staged part (best effort). */
    async removePart(dirSegments, name) {
      try {
        const dir = await navigate(dirSegments, false);
        await dir.removeEntry(name);
      } catch {
        /* already gone */
      }
    },

    /** Removes a whole selection's staging directory (best effort). */
    async removeDir(dirSegments) {
      if (dirSegments.length === 0) {
        return;
      }
      try {
        const parent = await navigate(dirSegments.slice(0, -1), false);
        await parent.removeEntry(dirSegments[dirSegments.length - 1], { recursive: true });
      } catch {
        /* already gone */
      }
    },
  };
}
