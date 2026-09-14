// Unit tests: worker preparation (hashing, sorting, limits, abort,
// concurrency) and the offers manager (caps, withdraw, freshness).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  WORKER_MAX_CONCURRENT_FILES,
  WORKER_SLICE_BYTES,
  prepareOffer,
  workerEntryRelativePath,
} from "../../src/offer-worker.js";
import { createOfferManager } from "../../src/offers.js";
import { fileRoot, manifestKey, hmacSign, bytesToHex, hexToBytes } from "../../src/crypto.js";
import { canonicalize, manifestValue } from "../../src/protocol.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixtureDir = join(here, "..", "..", "..", "..", "tests", "fixtures", "web_transfer", "v1");
const fixture = (name) => readFileSync(join(fixtureDir, name), "utf8");

const ROOM_ID = "0f1e2d3c4b5a69780f1e2d3c4b5a6978";
const ROOM_KEY = "1a".repeat(32);
const LIMITS = { maxEntriesPerOffer: 10000, maxOfferBytes: 1099511627776 };

// Duck-typed file: real bytes via Blob, instrumented slice/arrayBuffer.
function testFile(name, bytes, { lastModified = 1757779200000, relativePath = "", delayMs = 0, onSlice = null } = {}) {
  const blob = new Blob([bytes]);
  let active = 0;
  return {
    file: {
      name,
      webkitRelativePath: relativePath,
      size: blob.size,
      lastModified,
      slice: (start, end) => {
        onSlice?.(start, end);
        const part = blob.slice(start, end);
        return {
          arrayBuffer: async () => {
            active += 1;
            try {
              if (delayMs > 0) {
                await new Promise((resolve) => setTimeout(resolve, delayMs));
              }
              return part.arrayBuffer();
            } finally {
              active -= 1;
            }
          },
        };
      },
    },
    getActive: () => active,
  };
}

function baseJob(overrides = {}) {
  return {
    offerId: "c".repeat(32),
    kind: "file",
    label: "Demo",
    createdAt: "2026-09-14T12:00:00Z",
    roomIdHex: ROOM_ID,
    roomKeyHex: ROOM_KEY,
    limits: LIMITS,
    signal: null,
    onProgress: null,
    ...overrides,
  };
}

describe("offer preparation", () => {
  it("offer_worker_builds_exact_fixture_for_empty_and_multichunk_files", async () => {
    // Fixture roots recomputed from fixture leaves: the formula path, not
    // hardcoded digests.
    const manifest = JSON.parse(fixture("manifest.json"));
    for (const entry of manifest.entries) {
      const leaves = entry.chunks.map((hex) => hexToBytes(hex));
      const root = bytesToHex(await fileRoot(leaves.length, leaves));
      assert.equal(root, entry.root);
    }
    // Synthetic empty + 1 MiB + 1 byte files: exact chunk math end to end.
    const big = new Uint8Array(1024 * 1024 + 1);
    big.fill(0x61);
    const prepared = await prepareOffer(
      baseJob({
        kind: "files",
        label: "Pair",
        files: [
          { file: testFile("empty.txt", new Uint8Array(0)).file, relativePath: "empty.txt" },
          { file: testFile("big.bin", big).file, relativePath: "big.bin" },
        ],
      }),
    );
    assert.equal(prepared.manifest.entries.length, 2);
    assert.deepEqual(
      prepared.manifest.entries.map((e) => [e.id, e.path]),
      [
        ["0", "big.bin"],
        ["1", "empty.txt"],
      ],
    );
    const [bigEntry, emptyEntry] = prepared.manifest.entries;
    assert.equal(bigEntry.chunkCount, "2");
    assert.equal(bigEntry.chunks.length, 2);
    assert.equal(emptyEntry.chunkCount, "0");
    assert.deepEqual(emptyEntry.chunks, []);
    // Canonical form is stable and HMAC-verified (self-consistent).
    const canonical = canonicalize(manifestValue(prepared.manifest));
    assert.equal(canonical, prepared.manifestCanonical);
    const key = await manifestKey(hexToBytes(ROOM_KEY), hexToBytes(ROOM_ID));
    const { hmacVerify } = await import("../../src/crypto.js");
    assert.equal(await hmacVerify(key, new TextEncoder().encode(canonical), hexToBytes(prepared.macHex)), true);
    assert.deepEqual(
      prepared.observed.map((o) => [o.path, o.size, o.mtimeSec]),
      [
        ["big.bin", 1024 * 1024 + 1, 1757779200],
        ["empty.txt", 0, 1757779200],
      ],
    );
  });

  it("offer_worker_normalizes_sorts_and_assigns_ids", async () => {
    const a = testFile("b.txt", new Uint8Array([1]));
    const b = testFile("a.txt", new Uint8Array([2]));
    const prepared = await prepareOffer(
      baseJob({
        kind: "files",
        files: [
          { file: a.file, relativePath: "b.txt" },
          { file: b.file, relativePath: "caf\u00e9.txt" },
        ],
      }),
    );
    // Sorted by path (byte order), sequential 0-based IDs, NFC kept.
    assert.deepEqual(
      prepared.manifest.entries.map((e) => [e.id, e.path]),
      [
        ["0", "b.txt"],
        ["1", "café.txt"],
      ],
    );
  });

  it("all_forbidden_paths_and_casefold_collisions_fail_before_publish", async () => {
    for (const bad of ["../x", "/abs", "a//b", "a/./b", "back\\x", "trail/", ""]) {
      await assert.rejects(
        prepareOffer(baseJob({ files: [{ file: testFile("x", new Uint8Array([1])).file, relativePath: bad }] })),
        undefined,
        bad,
      );
    }
    const first = testFile("a.txt", new Uint8Array([1]));
    const second = testFile("A.TXT", new Uint8Array([2]));
    await assert.rejects(
      prepareOffer(
        baseJob({
          kind: "files",
          files: [
            { file: first.file, relativePath: "a.txt" },
            { file: second.file, relativePath: "A.TXT" },
          ],
        }),
      ),
    );
    // workerEntryRelativePath prefers supplied paths, tolerates one slash.
    assert.equal(workerEntryRelativePath({ name: "n", webkitRelativePath: "" }), "n");
    assert.equal(workerEntryRelativePath({ name: "n", webkitRelativePath: "/d/f" }), "d/f");
  });

  it("worker_concurrency_is_two_and_slice_is_one_mib", async () => {
    let live = 0;
    let peak = 0;
    const slices = [];
    const mk = (name, size) =>
      testFile(name, new Uint8Array(size), {
        onSlice: (start, end) => slices.push(end - start),
      });
    const slow = (name, size) => {
      const base = mk(name, size);
      const file = base.file;
      const origSlice = file.slice.bind(file);
      return {
        file: {
          ...file,
          slice: (start, end) => {
            const part = origSlice(start, end);
            return {
              arrayBuffer: async () => {
                live += 1;
                peak = Math.max(peak, live);
                try {
                  await new Promise((resolve) => setTimeout(resolve, 15));
                  return part.arrayBuffer();
                } finally {
                  live -= 1;
                }
              },
            };
          },
        },
      };
    };
    const prepared = await prepareOffer(
      baseJob({
        kind: "files",
        files: [
          { file: slow("a.bin", 3 * 1024 * 1024).file, relativePath: "a.bin" },
          { file: slow("b.bin", 3 * 1024 * 1024).file, relativePath: "b.bin" },
          { file: slow("c.bin", 1024).file, relativePath: "c.bin" },
        ],
      }),
    );
    assert.equal(prepared.manifest.entries.length, 3);
    assert.equal(peak, WORKER_MAX_CONCURRENT_FILES);
    assert.ok(slices.length > 0);
    for (const length of slices) {
      assert.ok(length > 0 && length <= WORKER_SLICE_BYTES, length);
    }
    // 3 MiB file touches exactly three full slices.
    assert.equal(slices.filter((length) => length === WORKER_SLICE_BYTES).length, 6);
  });

  it("abort_stops_file_reads_and_emits_no_publish", async () => {
    let reads = 0;
    const slow = testFile("slow.bin", new Uint8Array(4 * 1024 * 1024), {
      delayMs: 40,
      onSlice: () => {
        reads += 1;
      },
    });
    const controller = new AbortController();
    const pending = prepareOffer(
      baseJob({
        kind: "files",
        files: [
          { file: slow.file, relativePath: "slow.bin" },
          { file: testFile("late.bin", new Uint8Array([9])).file, relativePath: "late.bin" },
        ],
        signal: controller.signal,
      }),
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    controller.abort();
    const error = await pending.then(
      () => null,
      (e) => e,
    );
    assert.ok(error?.aborted === true, `expected abort, got ${error}`);
    assert.ok(reads < 4, `reads stopped promptly, got ${reads}`);
  });

  it("manifest_hmac_matches_rust_fixture", async () => {
    const vectors = JSON.parse(fixture("crypto-vectors.json"));
    const canonical = fixture("manifest.canonical.json");
    const key = await manifestKey(
      hexToBytes(vectors.inputs.room_key),
      hexToBytes(vectors.inputs.room_id),
    );
    const { hmacSign: sign } = await import("../../src/crypto.js");
    const mac = bytesToHex(await sign(key, new TextEncoder().encode(canonical)));
    assert.equal(mac, vectors.expected.manifest_mac_hex);
  });

  it("local_caps_prevent_hashing_or_publish", () => {
    const created = [];
    const sent = [];
    const manager = createOfferManager({
      createWorker: () => {
        created.push(true);
        return { postMessage: () => {}, onmessage: null };
      },
      roomIdHex: ROOM_ID,
      roomKeyHex: ROOM_KEY,
      sendControl: (message) => {
        sent.push(message);
        return true;
      },
      events: {},
    });
    manager.setServerLimits({ maxEntriesPerOffer: 1, maxOfferBytes: 10 });
    // Two entries exceed the local count cap: no worker, no traffic.
    const two = [testFile("a.txt", new Uint8Array([1])).file, testFile("b.txt", new Uint8Array([2])).file];
    assert.ok(manager.prepareSelection({ kind: "files", files: two }).error);
    assert.equal(created.length, 0);
    assert.equal(sent.length, 0);
    // Eleven bytes exceed the local byte cap the same way.
    const big = [testFile("big.txt", new Uint8Array(11)).file];
    assert.ok(manager.prepareSelection({ kind: "file", files: big }).error);
    assert.equal(created.length, 0);
    assert.equal(sent.length, 0);
  });

  it("withdraw_clears_only_owned_file_map_after_ack", async () => {
    const posted = [];
    const sent = [];
    let workerHandler = null;
    const manager = createOfferManager({
      createWorker: () => ({
        postMessage: (message) => posted.push(message),
        set onmessage(fn) {
          workerHandler = fn;
        },
        get onmessage() {
          return workerHandler;
        },
      }),
      roomIdHex: ROOM_ID,
      roomKeyHex: ROOM_KEY,
      sendControl: (message) => {
        sent.push(message);
        return true;
      },
      events: {},
    });
    const file = testFile("a.txt", new Uint8Array([1, 2, 3]));
    const { offerId } = manager.prepareSelection({ kind: "file", files: [file.file] });
    assert.equal(posted.length, 1);
    assert.equal(posted[0].type, "prepare");
    // Worker done: publish goes out, maps retained.
    const manifest = { offer: offerId, mode: "single", entries: [] };
    workerHandler({
      data: { type: "done", offerId, manifest, manifestCanonical: "{}", macHex: "e".repeat(64), observed: [] },
    });
    assert.equal(sent.length, 1);
    assert.equal(sent[0].type, "offer.publish");
    const publishRid = sent[0].requestId;
    assert.equal(manager.offers().size, 1);
    // Withdraw sends offer.withdraw; maps drop only on the terminal ack.
    manager.withdrawOffer(offerId);
    assert.equal(sent.length, 2);
    assert.equal(sent[1].type, "offer.withdraw");
    assert.equal(manager.offers().size, 1);
    const withdrawRid = sent[1].requestId;
    assert.notEqual(withdrawRid, publishRid);
    manager.handleReply("ack", { requestId: withdrawRid }, withdrawRid);
    assert.equal(manager.offers().size, 0);
    // Unknown offers withdraw nowhere.
    assert.ok(manager.withdrawOffer(offerId).error);
    assert.equal(sent.length, 2);
    // Unknown replies are unconsumed.
    assert.equal(manager.handleReply("ack", {}, "0".repeat(32)), false);
  });
});
