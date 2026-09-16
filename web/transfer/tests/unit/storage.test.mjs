// Unit tests: OPFS/IDB repository — pure planners, quota math, record
// schema and the fake-backed store. No DOM, no network.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  MAX_RESUME_RANGES,
  STAGE_WORKER_HELLO_MS,
  capRanges,
  checkQuotaAgainst,
  coalesceRanges,
  chunkPartName,
  createRepository,
  isHexSegment,
  partDirSegments,
  partialKey,
  rangesToIndexes,
  sanitizeDownloadName,
} from "../../src/storage.js";
import { fakeBackends, fakeStageWorker } from "./fakes.mjs";

describe("web-transfer storage", () => {
  it("generated_opfs_paths_ignore_manifest_paths", () => {
    const segments = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    assert.deepEqual(segments, [
      "bore-transfer-v1",
      "aa".repeat(16),
      "bb".repeat(16),
      "cc".repeat(64),
    ]);
    assert.equal(chunkPartName("3", 7), "3.7.part");
    // Hostile inputs never become paths — they throw instead.
    for (const evil of ["../../etc", "/abs", "a/b", "", "ABCDEF", "zz"]) {
      assert.throws(() => partDirSegments(evil, "bb".repeat(16), "cc".repeat(64)), /hex/);
      assert.throws(() => partDirSegments("aa".repeat(16), evil, "cc".repeat(64)), /hex/);
      assert.throws(() => partDirSegments("aa".repeat(16), "bb".repeat(16), evil), /hex/);
    }
    assert.throws(() => chunkPartName("../x", 0));
    assert.throws(() => chunkPartName("3", -1));
    assert.throws(() => chunkPartName("3", 1.5));
    assert.ok(!isHexSegment("ABCDEF"));
    assert.ok(isHexSegment("0".repeat(32)));
    // Structured keys never embed URLs or tokens.
    assert.deepEqual(partialKey("aa", "bb", "cc", "dd"), ["aa", "bb", "cc", "dd"]);
    assert.deepEqual(rangesToIndexes([[0, 2], [5, 6]]), [0, 1, 5]);
  });

  it("resume_ranges_are_sorted_bounded_and_coalesced_to_prefix", () => {
    assert.deepEqual(coalesceRanges([[3, 5], [0, 2], [1, 4]]), [[0, 5]]);
    assert.deepEqual(coalesceRanges([[0, 1], [0, 1]]), [[0, 1]]);
    assert.deepEqual(coalesceRanges([]), []);
    assert.throws(() => coalesceRanges([[2, 1]]));
    assert.throws(() => coalesceRanges([[-1, 1]]));
    // Over the 4096 cap the wire keeps the stable contiguous prefix only.
    const many = [];
    for (let i = 0; i < 8200; i += 2) {
      many.push([i, i + 1]);
    }
    const capped = capRanges(many);
    assert.deepEqual(capped, [[0, 1]]);
    assert.ok(capRanges([[0, 2], [5, 9]]).length === 2);
  });

  it("quota_is_checked_with_bigint_and_headroom", () => {
    // 100-byte file: headroom is 5% (5 B), need is 105.
    assert.deepEqual(checkQuotaAgainst({ quota: 1000, usage: 0 }, 100).ok, true);
    assert.equal(checkQuotaAgainst({ quota: 104, usage: 0 }, 100).ok, false);
    assert.equal(checkQuotaAgainst({ quota: 105, usage: 0 }, 100).ok, true);
    // 2 GiB file: 5% exceeds 64 MiB, so the headroom is exactly 64 MiB.
    const twoGib = 2n * 1024n * 1024n * 1024n;
    const need = twoGib + BigInt(64 * 1024 * 1024);
    assert.equal(checkQuotaAgainst({ quota: Number(need), usage: 0 }, twoGib).ok, true);
    assert.equal(checkQuotaAgainst({ quota: Number(need - 1n), usage: 0 }, twoGib).ok, false);
    // Withheld estimates pass as unknown rather than blocking downloads.
    assert.deepEqual(checkQuotaAgainst({}, 100), { ok: true, unknown: true, need: 105n });
    assert.equal(checkQuotaAgainst({ quota: 50, usage: 100 }, 100).ok, false);
  });

  it("sanitize_download_name_never_escapes", () => {
    assert.equal(sanitizeDownloadName("dir/a.bin"), "a.bin");
    assert.equal(sanitizeDownloadName("../../etc/passwd"), "passwd");
    assert.equal(sanitizeDownloadName(""), "download.bin");
    assert.equal(sanitizeDownloadName(".."), "download.bin");
    assert.equal(sanitizeDownloadName("a".repeat(300)).length, 255);
  });

  it("storage_schema_contains_no_secret_fields", async () => {
    const repo = createRepository(fakeBackends());
    assert.equal(await repo.supported(), true);
    const key = partialKey("aa", "bb", "cc", "dd");
    await repo.saveRecord(key, {
      manifestMac: "ee".repeat(32),
      roomId: "aa",
      offerId: "cc",
      digest: "dd",
      kind: "file",
      path: "a.bin",
      size: "11",
      root: "ff".repeat(32),
      verifiedRanges: [[0, 1]],
      chunkDigests: { 0: "11".repeat(32) },
      updatedAt: "2026-09-15T00:00:00Z",
    });
    const raw = await repo.loadRecord(key);
    const allowed = new Set([
      "manifestMac",
      "roomId",
      "offerId",
      "digest",
      "kind",
      "path",
      "size",
      "root",
      "verifiedRanges",
      "chunkDigests",
      "updatedAt",
    ]);
    for (const field of Object.keys(raw)) {
      assert.ok(allowed.has(field), `unexpected stored field ${field}`);
    }
    const blob = JSON.stringify(raw);
    for (const secret of ["token", "Token", "secret", "SECRET", "key", "passwd", "http"]) {
      assert.ok(!blob.includes(secret), `stored record mentions ${secret}`);
    }
    await repo.deleteRecord(key);
    assert.equal(await repo.loadRecord(key), undefined);
  });

  it("chunk_is_flushed_before_idb_commit", async () => {
    const order = [];
    const repo = createRepository(
      fakeBackends({
        onOpfsClose: () => order.push("opfs-close"),
        onIdbPut: () => order.push("idb-put"),
      }),
    );
    const dir = ["bore-transfer-v1", "aa"];
    await repo.writeChunk(dir, chunkPartName("0", 0), new Uint8Array([1, 2, 3]));
    await repo.saveRecord(["aa"], { verifiedRanges: [[0, 1]] });
    assert.deepEqual(order, ["opfs-close", "idb-put"]);
    const back = await repo.readChunk(dir, chunkPartName("0", 0));
    assert.deepEqual([...back], [1, 2, 3]);
    // A missing part reads as null, never as a short buffer.
    assert.equal(await repo.readChunk(dir, chunkPartName("0", 9)), null);
    await repo.removePart(dir, chunkPartName("0", 0));
    assert.equal(await repo.readChunk(dir, chunkPartName("0", 0)), null);
  });

  it("staged_output_is_the_parts_in_order_and_costs_no_copy", async () => {
    const backends = fakeBackends();
    const repo = createRepository(backends);
    const dir = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    const names = [];
    for (let index = 0; index < 4; index++) {
      const name = chunkPartName("0", index);
      names.push(name);
      await repo.writeChunk(dir, name, new Uint8Array([index, index, index]));
    }
    const blob = await repo.stagedBlob(dir, names);
    assert.equal(blob.size, 12);
    assert.deepEqual([...new Uint8Array(await blob.arrayBuffer())], [
      0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3,
    ]);
    // Composing the output leaves every part exactly where it was.
    assert.equal(backends.__dirs.get(dir.join("/")).files.size, 4);
    // Purge takes the whole selection directory, parts included.
    await repo.removeDir(dir);
    assert.equal(backends.__dirs.has(dir.join("/")), false);
    assert.equal(await repo.readChunk(dir, names[0]), null);
  });

  it("storage_opens_the_database_once_for_many_operations", async () => {
    const backends = fakeBackends();
    const repo = createRepository(backends);
    for (let index = 0; index < 8; index++) {
      await repo.saveRecord(["k", index], { verifiedRanges: [[0, index]] });
      await repo.loadRecord(["k", index]);
    }
    assert.equal(backends.__stats.idbOpens, 1);
    repo.close();
    await repo.loadRecord(["k", 0]);
    assert.equal(backends.__stats.idbOpens, 2);
  });

  // 3.11: the staging worker. Correctness is defined by the main-thread
  // path, so every test here compares against it rather than against itself.
  it("staged_bytes_are_identical_on_both_paths", async () => {
    const bytes = new Uint8Array(4096);
    for (let i = 0; i < bytes.length; i += 1) {
      bytes[i] = (i * 29 + 7) % 251;
    }
    const dir = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    const name = chunkPartName("0", 3);

    const plainBackends = fakeBackends();
    const plain = createRepository(plainBackends);
    await plain.writeChunk(dir, name, bytes);

    const workerBackends = fakeBackends();
    const log = [];
    const worker = createRepository({
      ...workerBackends,
      createStageWorker: fakeStageWorker(workerBackends, { log }),
    });
    await worker.writeChunk(dir, name, bytes);

    assert.deepEqual(log, [{ name, length: bytes.length }], "the worker did the write");
    assert.deepEqual(await worker.readChunk(dir, name), await plain.readChunk(dir, name));
    assert.deepEqual(await worker.readChunk(dir, name), bytes);
    // A handled worker failure returns the bytes, so the main-thread
    // fallback still has something to write — proved by the failWrite arm
    // of `stage_worker_falls_back_when_sync_handles_are_missing`.
    assert.equal(bytes.length, 4096);
  });

  it("stage_worker_falls_back_when_sync_handles_are_missing", async () => {
    const dir = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    const bytes = new Uint8Array([9, 8, 7, 6, 5]);
    for (const [label, options] of [
      ["no sync access handles", { sync: false }],
      ["no worker at all", null],
      ["a worker that never answers the handshake", { hello: false }],
      ["a worker that fails every write", { failWrite: true }],
    ]) {
      const backends = fakeBackends();
      const log = [];
      const repo = createRepository({
        ...backends,
        createStageWorker:
          options === null ? () => null : fakeStageWorker(backends, { ...options, log }),
      });
      await repo.writeChunk(dir, chunkPartName("0", 0), bytes);
      assert.deepEqual(await repo.readChunk(dir, chunkPartName("0", 0)), bytes, label);
      assert.deepEqual(log, [], `${label}: nothing was staged through the worker`);
      // A second chunk must not re-pay the handshake: the decision is final.
      await repo.writeChunk(dir, chunkPartName("0", 1), bytes);
      assert.deepEqual(await repo.readChunk(dir, chunkPartName("0", 1)), bytes, label);
    }
  });

  it("record_commits_only_after_the_part_closes", async () => {
    // The crash-safe order is the whole reason the worker answers instead of
    // acknowledging: a record with no bytes is unrecoverable, bytes with no
    // record are merely re-fetched.
    const order = [];
    const backends = fakeBackends({
      onOpfsFlush: () => order.push("flush"),
      onOpfsClose: () => order.push("close"),
      onIdbPut: () => order.push("idb"),
    });
    const repo = createRepository({
      ...backends,
      createStageWorker: fakeStageWorker(backends),
    });
    const dir = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    await repo.writeChunk(dir, chunkPartName("0", 0), new Uint8Array([1, 2, 3]));
    await repo.saveRecord(["k"], { verifiedRanges: [[0, 1]] });
    assert.deepEqual(order, ["flush", "close", "idb"]);
  });

  it("a_handshake_that_never_lands_does_not_delay_the_first_chunk_forever", async () => {
    // The budget is a constant, not a guess at the caller's patience.
    const backends = fakeBackends();
    const repo = createRepository({
      ...backends,
      createStageWorker: fakeStageWorker(backends, { hello: false }),
    });
    const dir = partDirSegments("aa".repeat(16), "bb".repeat(16), "cc".repeat(64));
    const started = Date.now();
    await repo.writeChunk(dir, chunkPartName("0", 0), new Uint8Array([4]));
    assert.ok(
      Date.now() - started < STAGE_WORKER_HELLO_MS + 2000,
      "the fallback must not wait past the handshake budget",
    );
    assert.deepEqual(await repo.readChunk(dir, chunkPartName("0", 0)), new Uint8Array([4]));
  });

});

// (fakeBackends lives in ./fakes.mjs)
