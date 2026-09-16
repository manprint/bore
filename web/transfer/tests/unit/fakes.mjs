// In-memory OPFS + IndexedDB doubles honoring the repository's call shape.
import { stageWrite } from "../../src/stage-worker.js";
export function fakeBackends(hooks = {}) {
  const dirs = new Map();
  const records = new Map();
  const stats = { idbOpens: 0 };
  const keyOf = (key) => JSON.stringify(key);

  function dirNode(segments, create) {
    const path = segments.join("/");
    if (!dirs.has(path)) {
      if (!create) {
        throw new Error("NotFoundError");
      }
      dirs.set(path, { files: new Map() });
    }
    return dirs.get(path);
  }

  function fakeDir(segments) {
    return {
      async getDirectoryHandle(name, { create } = {}) {
        dirNode([...segments, name], create);
        return fakeDir([...segments, name]);
      },
      async getFileHandle(name, { create } = {}) {
        const node = dirNode(segments, create);
        if (!node.files.has(name)) {
          if (!create) {
            throw new Error("NotFoundError");
          }
          node.files.set(name, new Uint8Array(0));
        }
        const stash = node.files;
        return {
          async createWritable({ keepExistingData } = {}) {
            let bytes = keepExistingData ? stash.get(name) : new Uint8Array(0);
            return {
              // Accepts both call shapes: a bare chunk (the part writer) and
              // the positional `{ position, data }` form.
              async write(input) {
                const { position, data } =
                  input instanceof Uint8Array ? { position: bytes.length, data: input } : input;
                const end = position + data.length;
                if (end > bytes.length) {
                  const grown = new Uint8Array(end);
                  grown.set(bytes, 0);
                  bytes = grown;
                }
                bytes.set(data, position);
              },
              async close() {
                stash.set(name, bytes);
                hooks.onOpfsClose?.();
              },
            };
          },
          async getFile() {
            const bytes = stash.get(name);
            if (bytes === undefined) {
              throw new Error("NotFoundError");
            }
            return new Blob([bytes]);
          },
          // Worker-only in a real engine; the double exposes it so the
          // staging worker's own code can be driven under Node.
          async createSyncAccessHandle() {
            let bytes = stash.get(name) ?? new Uint8Array(0);
            let closed = false;
            return {
              truncate(length) {
                bytes = bytes.slice(0, length);
              },
              write(data, { at = 0 } = {}) {
                const end = at + data.length;
                if (end > bytes.length) {
                  const grown = new Uint8Array(end);
                  grown.set(bytes, 0);
                  bytes = grown;
                }
                bytes.set(data, at);
                return data.length;
              },
              flush() {
                hooks.onOpfsFlush?.();
              },
              close() {
                if (closed) {
                  return;
                }
                closed = true;
                stash.set(name, bytes);
                hooks.onOpfsClose?.();
              },
            };
          },
        };
      },
      async removeEntry(name, { recursive } = {}) {
        const node = dirNode(segments, false);
        if (node.files.has(name)) {
          node.files.delete(name);
          return;
        }
        const path = [...segments, name].join("/");
        if (!dirs.has(path)) {
          throw new Error("NotFoundError");
        }
        if (!recursive && dirs.get(path).files.size > 0) {
          throw new Error("InvalidModificationError");
        }
        for (const existing of [...dirs.keys()]) {
          if (existing === path || existing.startsWith(`${path}/`)) {
            dirs.delete(existing);
          }
        }
      },
    };
  }

  function fakeStore() {
    return {
      get: (key) => fakeRequest(() => records.get(keyOf(key))),
      put: (record, key) => fakeRequest(() => {
        records.set(keyOf(key), record);
        hooks.onIdbPut?.();
      }),
      delete: (key) => fakeRequest(() => records.delete(keyOf(key))),
      getAllKeys: () =>
        fakeRequest(() => [...records.keys()].map((raw) => JSON.parse(raw))),
    };
  }

  function fakeRequest(exec) {
    const request = {};
    queueMicrotask(() => {
      try {
        request.result = exec();
        request.onsuccess?.();
      } catch (error) {
        request.error = error;
        request.onerror?.();
      }
    });
    return request;
  }

  return {
    getDirectory: async () => fakeDir([]),
    openIDB: () => {
      stats.idbOpens += 1;
      const request = {};
      queueMicrotask(() => {
        request.result = {
          transaction: () => {
            const tx = { oncomplete: null, onerror: null, onabort: null, error: null };
            tx.objectStore = () => fakeStore();
            setTimeout(() => tx.oncomplete?.(), 0);
            return tx;
          },
          close: () => {},
        };
        request.onsuccess?.();
      });
      return request;
    },
    estimateStorage: async () => ({ quota: 2 ** 53, usage: 0 }),
    __records: records,
    __dirs: dirs,
    __stats: stats,
  };
}

/**
 * A staging-worker double speaking the real `storage.js` protocol and
 * executing the real `stageWrite` against the injected OPFS double, so the
 * worker path under test is the shipped one and only the transport is fake.
 *
 * @param {object} backends result of `fakeBackends()`
 * @param {object} options `{ sync, hello, failWrite, log }` —
 *   `sync: false` makes the worker report no sync handles, `hello: false`
 *   makes it never answer the handshake, `failWrite` rejects every write,
 *   `log` collects `{ name }` per accepted write.
 */
export function fakeStageWorker(backends, { sync = true, hello = true, failWrite = false, log } = {}) {
  return function factory() {
    const worker = {
      onmessage: null,
      onerror: null,
      onmessageerror: null,
      terminated: false,
      postMessage({ id, dirSegments, name, bytes }) {
        queueMicrotask(async () => {
          if (worker.terminated) {
            return;
          }
          try {
            if (failWrite) {
              throw new Error("staging refused");
            }
            await stageWrite(backends.getDirectory, dirSegments, name, bytes);
            log?.push({ name, length: bytes.length });
            worker.onmessage?.({ data: { id, ok: true } });
          } catch (error) {
            // The real worker hands the bytes back with a handled failure,
            // so the main thread can still write them; the double must too,
            // or the fallback would look unreachable.
            worker.onmessage?.({
              data: { id, ok: false, error: String(error.message), bytes },
            });
          }
        });
      },
      terminate() {
        worker.terminated = true;
      },
    };
    if (hello) {
      queueMicrotask(() => worker.onmessage?.({ data: { hello: true, sync } }));
    }
    return worker;
  };
}
