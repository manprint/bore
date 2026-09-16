// OPFS staging worker: part-file writes off the main thread, using
// `createSyncAccessHandle`.
//
// Why a worker at all. 3.10 profiled the recipient and found the dominant
// remaining cost is not the network and not the crypto but the OPFS write
// protocol itself: `createWritable` + `write` + `close` is 247 ms of 373
// (chromium) and 449 of 717 (firefox) per 32 MiB. `createSyncAccessHandle`
// replaces those three asynchronous steps with one handle and synchronous
// `write`/`flush`, and it exists ONLY inside a worker — which is the whole
// reason this file is separate from `storage.js`.
//
// What must not change. The part file closes BEFORE the IndexedDB record is
// committed, so a crash can leave a staged chunk with no record (harmless,
// re-fetched) but never a record with no bytes. That order lives on the main
// thread: this worker answers only after `close()` returns, and `storage.js`
// commits after the answer. Resume granularity stays one verified chunk per
// file; this worker never merges chunks and never uses `keepExistingData`.
//
// The exported functions are UI-free so Node unit tests drive them directly
// against an in-memory OPFS double; the `onmessage` wrapper below only runs
// inside a real worker.

/** True when this realm can open synchronous access handles. */
export function syncHandlesAvailable(scope = globalThis) {
  return (
    typeof scope.FileSystemFileHandle === "function" &&
    typeof scope.FileSystemFileHandle.prototype?.createSyncAccessHandle === "function"
  );
}

/** Walks (creating) the staging directory chain. */
async function navigate(getDirectory, segments) {
  let dir = await getDirectory();
  for (const segment of segments) {
    dir = await dir.getDirectoryHandle(segment, { create: true });
  }
  return dir;
}

/**
 * Writes one verified chunk as its own part file and returns once the bytes
 * are durable. Truncates first: a part file is always rewritten whole, so a
 * shorter chunk can never leave a longer file's tail behind.
 *
 * @param {() => Promise<object>} getDirectory OPFS root factory
 * @param {string[]} dirSegments staging directory chain
 * @param {string} name part file name
 * @param {Uint8Array} bytes chunk bytes
 */
export async function stageWrite(getDirectory, dirSegments, name, bytes) {
  const dir = await navigate(getDirectory, dirSegments);
  const handle = await dir.getFileHandle(name, { create: true });
  const access = await handle.createSyncAccessHandle();
  try {
    access.truncate(0);
    access.write(bytes, { at: 0 });
    access.flush();
  } finally {
    access.close();
  }
}

// Real-worker wrapper. `storage.js` sends `{ id, dirSegments, name, bytes }`
// and reads back `{ id, ok }`; a refused capability is reported once, at
// startup, so the main thread can fall back before the first chunk arrives
// rather than after it.
if (typeof globalThis.postMessage === "function" && typeof globalThis.document === "undefined") {
  const getDirectory = () => globalThis.navigator.storage.getDirectory();
  // Writes are serialized: one chunk at a time is all the recipient ever
  // has in flight, and a queue here would only add memory for no pace.
  let tail = Promise.resolve();
  globalThis.onmessage = (event) => {
    const { id, dirSegments, name, bytes } = event.data ?? {};
    tail = tail.then(async () => {
      try {
        await stageWrite(getDirectory, dirSegments, name, bytes);
        globalThis.postMessage({ id, ok: true });
      } catch (error) {
        // Hand the bytes BACK with the failure. The main thread transferred
        // ownership to avoid a copy, so without this a handled error would
        // leave it with a detached buffer and nothing to retry.
        const reply = { id, ok: false, error: String(error?.message ?? error) };
        if (bytes?.buffer !== undefined && bytes.buffer.byteLength > 0) {
          reply.bytes = bytes;
          globalThis.postMessage(reply, [bytes.buffer]);
        } else {
          globalThis.postMessage(reply);
        }
      }
    });
  };
  globalThis.postMessage({ hello: true, sync: syncHandlesAvailable() });
}
