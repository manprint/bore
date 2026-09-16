// Unit tests (5.3): OPFS, resume and cancellation for ARCHIVES and for
// selections.
//
// An archive is not in the manifest, so everything a raw resume takes for
// granted has to be built here: the identity of a partial (the selection
// digest, which now covers the mode and the whole entry list), what a staged
// chunk is checked against (the digest THIS peer recorded, plus the rolling
// root at FINAL, which covers the resumed leaves too), how far a resume may
// reach (the contiguous prefix and nothing else) and what makes a second
// attempt refuse to touch verified bytes (the `(length, chunkCount, root)`
// tuple the first attempt authenticated).
//
// Real WebCrypto, real framing, the shipped repository over in-memory OPFS
// and IndexedDB doubles; only the transports are fake.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  attemptKey,
  bytesToHex,
  hexToBytes,
  manifestMac,
  openFrame,
  sealFrame,
  sha256Hex,
} from "../../src/crypto.js";
import { canonicalize, manifestValue } from "../../src/protocol.js";
import { createReceiver } from "../../src/receiver.js";
import { createSender } from "../../src/sender.js";
import {
  ARCHIVE_ENTRY_ID,
  CHUNK_BYTES,
  FRAME_DATA,
  FRAME_FINAL,
  archiveFinalPayload,
} from "../../src/framing.js";
import {
  MAX_RESUME_RANGES,
  capRanges,
  chunkPartName,
  createRepository,
  partDirSegments,
  partialKey,
  verifiedPrefix,
} from "../../src/storage.js";
import { fakeBackends } from "./fakes.mjs";

const ROOM_KEY = "ab".repeat(32);
const ROOM_ID = "00".repeat(16);
const SELF_PEER = "22".repeat(16);
const SOURCE_PEER = "11".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER = "dd".repeat(16);
const ATTEMPT = "ee".repeat(16);
const ARCHIVE_ID = String(ARCHIVE_ENTRY_ID);

function tick(ms = 10) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(done, timeoutMs = 20_000) {
  const start = Date.now();
  while (!done()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error("timed out");
    }
    await tick(5);
  }
}

/** Deterministic filler; every byte differs from its neighbours. */
function filler(length, salt = 0) {
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) {
    out[i] = (i * 31 + salt * 7 + 11) % 251;
  }
  return out;
}

// ---------------------------------------------------------------------------
// A folder offer: two files, together longer than two logical chunks, so the
// archive spans several of them and a resume has something to skip.
// ---------------------------------------------------------------------------

const FILES = [
  ["albero/uno.bin", filler(900 * 1024, 1)],
  ["albero/due.bin", filler(900 * 1024, 2)],
];

async function folderManifest() {
  const entries = [];
  let id = 0;
  entries.push({
    id: String(id++),
    path: "albero",
    size: "0",
    mtime: "1757779200",
    chunks: [],
    chunkCount: "0",
    root: null,
  });
  for (const [path, bytes] of FILES) {
    const chunks = [];
    for (let at = 0; at < bytes.length; at += CHUNK_BYTES) {
      chunks.push(await sha256Hex(bytes.slice(at, at + CHUNK_BYTES)));
    }
    entries.push({
      id: String(id++),
      path,
      size: String(bytes.length),
      mtime: "1757779200",
      chunks,
      chunkCount: String(chunks.length),
      root: "ff".repeat(32),
    });
  }
  return {
    offer: OFFER,
    mode: "multi",
    label: "albero",
    kind: "folder",
    chunkSize: String(CHUNK_BYTES),
    createdAt: "2026-09-16T00:00:00Z",
    entries,
  };
}

async function macOf(manifest) {
  return bytesToHex(
    await manifestMac(
      hexToBytes(ROOM_KEY),
      hexToBytes(ROOM_ID),
      new TextEncoder().encode(canonicalize(manifestValue(manifest))),
    ),
  );
}

/** The digest the two ends derive for one selection of this offer. */
async function selectionDigestFor(mac, entryIds, mode) {
  return sha256Hex(
    new TextEncoder().encode(
      canonicalize({ entryIds, manifestMac: mac, mode, offerId: OFFER }),
    ),
  );
}

class FakeSocket {
  constructor() {
    this.sent = [];
    this.closed = false;
    this.bufferedAmount = 0;
    this.onopen = null;
    this.onmessage = null;
    this.onclose = null;
    this.onerror = null;
  }

  send(data) {
    this.sent.push(data);
  }

  close() {
    this.closed = true;
  }

  emit(data) {
    this.onmessage?.({ data });
  }
}

// ---------------------------------------------------------------------------
// Source harness: serves the folder offer as an archive over a fake relay.
// ---------------------------------------------------------------------------

function countedFile(bytes, name) {
  const real = new File([bytes], name, { lastModified: 1757779200000 });
  const reads = [];
  return {
    reads,
    file: {
      size: real.size,
      lastModified: real.lastModified,
      slice: (start, end) => {
        reads.push([start, end]);
        return real.slice(start, end);
      },
    },
  };
}

async function sourceHarness() {
  const manifest = await folderManifest();
  const mac = await macOf(manifest);
  const sockets = [];
  const control = [];
  const counted = new Map();
  const files = new Map();
  for (const [path, bytes] of FILES) {
    const entry = countedFile(bytes, path.split("/").pop());
    counted.set(path, entry);
    files.set(path, { file: entry.file });
  }
  const errors = [];
  const sender = createSender({
    offers: {
      offers: () =>
        new Map([[OFFER, { status: "live", manifest, macHex: mac, files }]]),
      withdrawOffer: () => ({}),
    },
    sendControl: (message) => {
      control.push(message);
      return true;
    },
    createSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    relayBase: "ws://127.0.0.1:9",
    roomIdHex: ROOM_ID,
    roomKeyHex: ROOM_KEY,
    getSelfPeerId: () => SOURCE_PEER,
    events: { onError: (id, code) => errors.push([id, code]) },
  });
  return { sender, sockets, control, counted, manifest, mac, errors };
}

/**
 * Runs one whole archive send and returns what went on the wire.
 * `resumeRanges` rides the commit exactly as the server forwards it.
 */
async function sendArchiveOnce(resumeRanges = []) {
  const h = await sourceHarness();
  assert.equal(
    h.sender.handleControl({
      type: "transfer.incoming",
      body: {
        transferId: TRANSFER,
        offerId: OFFER,
        fromPeerId: SELF_PEER,
        attemptId: ATTEMPT,
        mode: "zip",
      },
    }),
    true,
  );
  await tick();
  assert.equal(
    h.sender.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER, attemptId: ATTEMPT, ticket: "ab".repeat(16) },
    }),
    true,
  );
  await tick(20);
  assert.equal(h.sockets.length, 1);
  h.sockets[0].onopen();
  assert.equal(
    h.sender.handleControl({
      type: "transfer.path_commit",
      body: {
        transferId: TRANSFER,
        attemptId: ATTEMPT,
        path: "relay",
        resumeRanges,
      },
    }),
    true,
  );
  await waitFor(
    () => h.sender.transfers().get(TRANSFER)?.state === "done-pending",
  );
  const key = await attemptKey(
    hexToBytes(ROOM_KEY),
    hexToBytes(TRANSFER),
    hexToBytes(ATTEMPT),
  );
  // The attach line is JSON; every frame after it is a sealed binary frame.
  const frames = h.sockets[0].sent.slice(1);
  const data = [];
  let final = null;
  let seq = 0;
  for (const raw of frames) {
    const opened = await openFrame(key, new Uint8Array(raw), seq);
    assert.equal(opened.seq, seq);
    seq += 1;
    if (opened.ftype === FRAME_DATA) {
      data.push(...opened.plaintext);
    } else {
      final = opened.plaintext;
    }
  }
  assert.ok(final !== null, "the archive ends with a FINAL frame");
  const view = new DataView(final.buffer, final.byteOffset, final.byteLength);
  const readBytes = [...h.counted.values()].reduce(
    (total, entry) =>
      total + entry.reads.reduce((sum, [start, end]) => sum + (end - start), 0),
    0,
  );
  return {
    data: new Uint8Array(data),
    total: Number(view.getBigUint64(0, false)),
    chunkCount: Number(view.getBigUint64(8, false)),
    root: bytesToHex(final.slice(16)),
    readBytes,
  };
}

// ---------------------------------------------------------------------------
// Recipient harness.
// ---------------------------------------------------------------------------

function recipientHarness(backends = fakeBackends()) {
  const control = [];
  const sockets = [];
  const events = { progress: [], staged: [], errors: [] };
  const repository = createRepository(backends);
  const receiver = createReceiver({
    sendControl: (message) => {
      control.push(message);
      return true;
    },
    createSocket: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    relayBase: "ws://127.0.0.1:9",
    roomIdHex: ROOM_ID,
    roomKeyHex: ROOM_KEY,
    getSelfPeerId: () => SELF_PEER,
    repository,
    events: {
      onProgress: (info) => events.progress.push(info),
      onStaged: (info) => events.staged.push(info),
      onError: (transferId, code, detail) =>
        events.errors.push([transferId, code, detail]),
    },
  });
  return { receiver, repository, control, sockets, events, backends };
}

/** Drives one archive download to the point where frames may be delivered. */
async function startArchive(h, manifest, mac, { fresh = false } = {}) {
  const started = await h.receiver.startDownload({
    offerId: OFFER,
    manifest,
    macHex: mac,
    sourcePeerId: SOURCE_PEER,
    mode: "zip",
    fresh,
  });
  assert.deepEqual(started, { pending: true });
  const request = h.control.find((m) => m.type === "transfer.request");
  assert.ok(request);
  h.receiver.handleControl({
    type: "ack",
    requestId: request.requestId,
    body: { result: { transferId: TRANSFER } },
  });
  h.receiver.handleControl({
    type: "transfer.relay_ticket",
    body: { transferId: TRANSFER, attemptId: ATTEMPT, ticket: "ab".repeat(16) },
  });
  await tick(20);
  h.sockets[0].onopen();
  h.receiver.handleControl({
    type: "transfer.path_commit",
    body: {
      transferId: TRANSFER,
      attemptId: ATTEMPT,
      path: "relay",
      resumeRanges: request.body.resume?.verifiedRanges ?? [],
    },
  });
  await tick(20);
  return request;
}

/**
 * Delivers `bytes` — the archive from `offset` — as DATA frames, split the
 * way the source splits them: at logical chunk boundaries first, then into
 * fragments. A fragment that straddled a chunk boundary would be a protocol
 * error (`fragment overrun`), so a test that produced one would be testing
 * its own harness.
 * @returns the next sequence number
 */
async function deliverData(h, key, seq, bytes, offset = 0) {
  let at = 0;
  while (at < bytes.length) {
    const inChunk = (offset + at) % CHUNK_BYTES;
    const take = Math.min(
      24 * 1024,
      CHUNK_BYTES - inChunk,
      bytes.length - at,
    );
    await deliver(h, key, seq++, FRAME_DATA, bytes.slice(at, at + take));
    at += take;
  }
  return seq;
}

/** Seals and delivers `plaintext` as one DATA/FINAL frame at `seq`. */
async function deliver(h, key, seq, ftype, plaintext) {
  const sealed = await sealFrame(key, seq, ftype, plaintext);
  h.sockets[0].emit(sealed.buffer.slice(0));
  await tick(15);
}

const ATTEMPT_KEY = () =>
  attemptKey(hexToBytes(ROOM_KEY), hexToBytes(TRANSFER), hexToBytes(ATTEMPT));

describe("web-transfer zip resume", () => {
  it("raw_and_zip_partials_have_disjoint_selection_keys", async () => {
    const manifest = await folderManifest();
    const mac = await macOf(manifest);
    const allIds = manifest.entries.map((entry) => String(entry.id)).sort();
    const zipDigest = await selectionDigestFor(mac, allIds, "zip");
    // Every raw selection of the SAME offer, and the archive: no two agree.
    const digests = new Map([["zip", zipDigest]]);
    for (const entry of manifest.entries) {
      digests.set(
        `raw:${entry.id}`,
        await selectionDigestFor(mac, [String(entry.id)], "raw"),
      );
    }
    assert.equal(new Set(digests.values()).size, digests.size);
    // The digest is a PATH segment, so distinct digests are distinct
    // staging directories — a raw partial and an archive partial of one
    // offer can never reach each other's bytes, and neither can two
    // different raw selections.
    const dirs = [...digests.values()].map((digest) =>
      partDirSegments(ROOM_ID, OFFER, digest).join("/"),
    );
    assert.equal(new Set(dirs).size, dirs.length);
    // And the IndexedDB keys are disjoint for the same reason.
    const keys = [...digests.values()].map((digest) =>
      JSON.stringify(partialKey(ROOM_ID, SOURCE_PEER, OFFER, digest)),
    );
    assert.equal(new Set(keys).size, keys.length);
    // The mode alone separates them: the same entry list under the other
    // mode is a different digest, which is what stops a one-entry offer's
    // raw and zip partials from colliding.
    const single = [String(manifest.entries[1].id)];
    assert.notEqual(
      await selectionDigestFor(mac, single, "raw"),
      await selectionDigestFor(mac, single, "zip"),
    );
  });

  it("too_many_sparse_ranges_reduce_to_verified_prefix", () => {
    // Over the wire cap the descriptor keeps the stable contiguous prefix
    // and drops the rest: the source re-sends everything past it, which is
    // correct work, where an over-long descriptor is a refused request.
    const sparse = [];
    for (let i = 0; i < MAX_RESUME_RANGES + 50; i++) {
      sparse.push([i * 2, i * 2 + 1]);
    }
    const capped = capRanges(sparse);
    assert.equal(capped.length, 1);
    assert.deepEqual(capped, [[0, 1]]);
    // Under the cap nothing is reduced: a raw file verifies each chunk
    // against the manifest, so any subset of them resumes.
    assert.deepEqual(capRanges([[0, 1], [4, 6]]), [[0, 1], [4, 6]]);
    // An ARCHIVE always reduces: its chunks have no manifest digest and no
    // identity but their position in a regenerated stream, so a hole would
    // put the next arrival one index too high.
    assert.deepEqual(verifiedPrefix([[0, 3], [7, 9]]), [[0, 3]]);
    assert.deepEqual(verifiedPrefix([[2, 5]]), []);
    assert.deepEqual(verifiedPrefix([]), []);
    assert.deepEqual(verifiedPrefix([[0, 2], [2, 4]]), [[0, 4]]);
  });

  it("zip_resume_skips_verified_output_without_skipping_source_reads", async () => {
    const whole = await sendArchiveOnce([]);
    assert.ok(whole.chunkCount >= 2, "the fixture spans several chunks");
    assert.equal(whole.data.length, whole.total);

    const resumed = await sendArchiveOnce([[0, 1]]);
    // The WIRE carries one chunk less, and exactly the right one.
    assert.equal(resumed.data.length, whole.total - CHUNK_BYTES);
    assert.deepEqual(
      [...resumed.data],
      [...whole.data.slice(CHUNK_BYTES)],
      "a resumed attempt sends the archive from the first missing chunk",
    );
    // The SOURCE still read every byte: the archive is regenerated from
    // byte zero, because the rolling root covers the skipped leaves too and
    // a chunk that is never produced has no leaf.
    assert.equal(resumed.readBytes, whole.readBytes);
    assert.ok(resumed.readBytes > 0);
    // The tuple describes the WHOLE archive on both attempts, which is what
    // lets the recipient check it against its disk rather than its wire.
    assert.equal(resumed.total, whole.total);
    assert.equal(resumed.chunkCount, whole.chunkCount);
    assert.equal(resumed.root, whole.root);
  });

  it("zip_resume_rehashes_ranges_and_regenerates_from_byte_zero", async () => {
    const whole = await sendArchiveOnce([]);
    const manifest = await folderManifest();
    const mac = await macOf(manifest);
    const allIds = manifest.entries.map((entry) => String(entry.id)).sort();
    const digest = await selectionDigestFor(mac, allIds, "zip");
    const dir = partDirSegments(ROOM_ID, OFFER, digest);
    const key = partialKey(ROOM_ID, SOURCE_PEER, OFFER, digest);

    // A first attempt that staged chunk 0 and stopped.
    const h = recipientHarness();
    const chunk0 = whole.data.slice(0, CHUNK_BYTES);
    await h.repository.writeChunk(dir, chunkPartName(ARCHIVE_ID, 0), chunk0);
    await h.repository.saveRecord(key, {
      manifestMac: mac,
      roomId: ROOM_ID,
      offerId: OFFER,
      digest,
      kind: "zip",
      path: "albero.zip",
      size: CHUNK_BYTES,
      root: null,
      expected: null,
      verifiedRanges: [[0, 1]],
      chunkDigests: { 0: await sha256Hex(chunk0) },
      updatedAt: new Date().toISOString(),
    });

    const request = await startArchive(h, manifest, mac);
    // The request names exactly what is on disk, in the shape the server
    // parses (`outputLength` a number, never the manifest's string).
    assert.deepEqual(request.body.resume.verifiedRanges, [[0, 1]]);
    assert.equal(request.body.resume.outputLength, CHUNK_BYTES);
    assert.equal(typeof request.body.resume.outputLength, "number");

    // The source sends from chunk 1 and the FINAL covers the whole archive.
    const aes = await ATTEMPT_KEY();
    let seq = await deliverData(
      h,
      aes,
      0,
      whole.data.slice(CHUNK_BYTES),
      CHUNK_BYTES,
    );
    await deliver(
      h,
      aes,
      seq++,
      FRAME_FINAL,
      archiveFinalPayload(whole.total, whole.chunkCount, hexToBytes(whole.root)),
    );
    const complete = h.control.find((m) => m.type === "transfer.complete");
    assert.ok(complete, "the resumed archive completes");
    assert.equal(complete.body.root, whole.root);
    // The staged prefix was REHASHED, not trusted: it is in the root the
    // recipient computed, and that root matched the source's.
    const stored = await h.repository.loadRecord(key);
    assert.deepEqual(stored.expected, {
      totalBytes: whole.total,
      chunkCount: whole.chunkCount,
      root: whole.root,
    });

    // A corrupted staged part ends the prefix before it: the next attempt
    // asks for nothing and the archive is received from byte zero.
    const g = recipientHarness();
    const damaged = whole.data.slice(0, CHUNK_BYTES);
    damaged[17] ^= 0xff;
    await g.repository.writeChunk(dir, chunkPartName(ARCHIVE_ID, 0), damaged);
    await g.repository.saveRecord(key, {
      manifestMac: mac,
      roomId: ROOM_ID,
      offerId: OFFER,
      digest,
      kind: "zip",
      path: "albero.zip",
      size: CHUNK_BYTES,
      root: null,
      expected: null,
      verifiedRanges: [[0, 1]],
      chunkDigests: { 0: await sha256Hex(chunk0) },
      updatedAt: new Date().toISOString(),
    });
    await g.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: mac,
      sourcePeerId: SOURCE_PEER,
      mode: "zip",
    });
    const second = g.control.find((m) => m.type === "transfer.request");
    assert.ok(!("resume" in second.body), "a damaged prefix resumes nothing");
  });

  it("dynamic_final_tuple_is_persisted_then_must_match", async () => {
    const whole = await sendArchiveOnce([]);
    const manifest = await folderManifest();
    const mac = await macOf(manifest);
    const allIds = manifest.entries.map((entry) => String(entry.id)).sort();
    const digest = await selectionDigestFor(mac, allIds, "zip");
    const dir = partDirSegments(ROOM_ID, OFFER, digest);
    const key = partialKey(ROOM_ID, SOURCE_PEER, OFFER, digest);

    // A partial whose FIRST attempt reached FINAL and recorded the tuple.
    const h = recipientHarness();
    const chunk0 = whole.data.slice(0, CHUNK_BYTES);
    await h.repository.writeChunk(dir, chunkPartName(ARCHIVE_ID, 0), chunk0);
    await h.repository.saveRecord(key, {
      manifestMac: mac,
      roomId: ROOM_ID,
      offerId: OFFER,
      digest,
      kind: "zip",
      path: "albero.zip",
      size: CHUNK_BYTES,
      root: whole.root,
      // What a FIRST attempt authenticated. The length differs from what
      // the source is about to present, and NOTHING ELSE does — the root
      // below is the real one — so only the tuple check can refuse this
      // attempt, which is what this test is for.
      expected: {
        totalBytes: whole.total + 4096,
        chunkCount: whole.chunkCount,
        root: whole.root,
      },
      verifiedRanges: [[0, 1]],
      chunkDigests: { 0: await sha256Hex(chunk0) },
      updatedAt: new Date().toISOString(),
    });
    await startArchive(h, manifest, mac);

    // The source now presents an archive that is internally consistent —
    // its root covers exactly the bytes it just sent — and disagrees with
    // the one this peer already authenticated.
    const aes = await ATTEMPT_KEY();
    let seq = await deliverData(
      h,
      aes,
      0,
      whole.data.slice(CHUNK_BYTES),
      CHUNK_BYTES,
    );
    await deliver(
      h,
      aes,
      seq++,
      FRAME_FINAL,
      archiveFinalPayload(
        whole.total,
        whole.chunkCount,
        hexToBytes(whole.root),
      ),
    );
    assert.deepEqual(
      h.events.errors.map(([, code]) => code),
      ["SOURCE_CHANGED"],
      "a tuple that contradicts the verified partial is a changed source",
    );
    assert.ok(!h.control.some((m) => m.type === "transfer.complete"));
    // And the verified bytes are STILL THERE: only an explicit restart may
    // throw away what this peer verified.
    const kept = await h.repository.readChunk(
      dir,
      chunkPartName(ARCHIVE_ID, 0),
    );
    assert.ok(kept !== null);
    assert.equal(kept.length, CHUNK_BYTES);
    const stored = await h.repository.loadRecord(key);
    // The attempt kept staging while it ran — every one of those chunks was
    // verified against its own digest — and the refusal at FINAL threw none
    // of them away. What matters is that the prefix never SHRANK.
    assert.equal(stored.verifiedRanges[0][0], 0);
    assert.ok(stored.verifiedRanges[0][1] >= 1);

    // The restart is the one gesture that discards them.
    const fresh = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: mac,
      sourcePeerId: SOURCE_PEER,
      mode: "zip",
      fresh: true,
    });
    assert.deepEqual(fresh, { pending: true });
    const requests = h.control.filter((m) => m.type === "transfer.request");
    assert.ok(!("resume" in requests[requests.length - 1].body));
    assert.equal(
      await h.repository.readChunk(dir, chunkPartName(ARCHIVE_ID, 0)),
      null,
    );
  });

  it("zip_cancel_keeps_only_complete_chunks", async () => {
    const whole = await sendArchiveOnce([]);
    const manifest = await folderManifest();
    const mac = await macOf(manifest);
    const allIds = manifest.entries.map((entry) => String(entry.id)).sort();
    const digest = await selectionDigestFor(mac, allIds, "zip");
    const dir = partDirSegments(ROOM_ID, OFFER, digest);
    const key = partialKey(ROOM_ID, SOURCE_PEER, OFFER, digest);

    const h = recipientHarness();
    await startArchive(h, manifest, mac);
    const aes = await ATTEMPT_KEY();
    // One whole chunk, then half of the next: the fragment that completes
    // no chunk leaves nothing behind.
    const upTo = CHUNK_BYTES + 200 * 1024;
    await deliverData(h, aes, 0, whole.data.slice(0, upTo));
    assert.deepEqual(h.events.errors, [], "no failure before the cancel");
    assert.equal(h.receiver.abortTransfer(TRANSFER), true);
    await tick(20);

    const stored = await h.repository.loadRecord(key);
    assert.deepEqual(stored.verifiedRanges, [[0, 1]]);
    assert.equal(stored.kind, "zip");
    assert.equal(stored.size, CHUNK_BYTES);
    // The partial chunk was never written; the complete one was, and it is
    // exactly the bytes the source produced.
    assert.equal(await h.repository.readChunk(dir, chunkPartName(ARCHIVE_ID, 1)), null);
    const first = await h.repository.readChunk(dir, chunkPartName(ARCHIVE_ID, 0));
    assert.equal(first.length, CHUNK_BYTES);
    assert.deepEqual([...first], [...whole.data.slice(0, CHUNK_BYTES)]);
  });

  it("withdraw_room_close_source_change_purge_all_selection_partials", async () => {
    const manifest = await folderManifest();
    const mac = await macOf(manifest);
    const allIds = manifest.entries.map((entry) => String(entry.id)).sort();
    const other = "ba".repeat(16);

    async function seedEvery(h) {
      const seeded = [];
      const selections = [
        [allIds, "zip", ARCHIVE_ID],
        [[String(manifest.entries[1].id)], "raw", String(manifest.entries[1].id)],
        [[String(manifest.entries[2].id)], "raw", String(manifest.entries[2].id)],
      ];
      for (const [ids, mode, partId] of selections) {
        const digest = await selectionDigestFor(mac, ids, mode);
        const dir = partDirSegments(ROOM_ID, OFFER, digest);
        const key = partialKey(ROOM_ID, SOURCE_PEER, OFFER, digest);
        await h.repository.writeChunk(
          dir,
          chunkPartName(partId, 0),
          filler(4096, 3),
        );
        await h.repository.saveRecord(key, {
          manifestMac: mac,
          kind: mode === "zip" ? "zip" : "file",
          verifiedRanges: [[0, 1]],
          chunkDigests: {},
        });
        seeded.push({ dir, key, partId });
      }
      // A partial of ANOTHER offer, which must survive a withdraw of this one.
      const strangerDigest = await selectionDigestFor(mac, ["0"], "raw");
      const strangerDir = partDirSegments(ROOM_ID, other, strangerDigest);
      const strangerKey = partialKey(ROOM_ID, SOURCE_PEER, other, strangerDigest);
      await h.repository.writeChunk(
        strangerDir,
        chunkPartName("0", 0),
        filler(4096, 4),
      );
      await h.repository.saveRecord(strangerKey, {
        manifestMac: mac,
        kind: "file",
        verifiedRanges: [[0, 1]],
        chunkDigests: {},
      });
      return { seeded, strangerDir, strangerKey };
    }

    // Withdraw: every selection of THIS offer goes, bytes included.
    const h = recipientHarness();
    const first = await seedEvery(h);
    await h.receiver.purgeOffer(OFFER);
    for (const { dir, key, partId } of first.seeded) {
      assert.equal(await h.repository.loadRecord(key), undefined);
      assert.equal(await h.repository.readChunk(dir, chunkPartName(partId, 0)), null);
    }
    assert.notEqual(await h.repository.loadRecord(first.strangerKey), undefined);
    assert.notEqual(
      await h.repository.readChunk(first.strangerDir, chunkPartName("0", 0)),
      null,
    );
    assert.equal((await h.receiver.resumableOffers()).has(OFFER), false);

    // Room close: everything goes, including the other offer's partial.
    const g = recipientHarness();
    const second = await seedEvery(g);
    await g.receiver.purgeAll();
    for (const { dir, key, partId } of second.seeded) {
      assert.equal(await g.repository.loadRecord(key), undefined);
      assert.equal(await g.repository.readChunk(dir, chunkPartName(partId, 0)), null);
    }
    assert.equal(await g.repository.loadRecord(second.strangerKey), undefined);
    assert.equal(
      await g.repository.readChunk(second.strangerDir, chunkPartName("0", 0)),
      null,
    );
    assert.equal((await g.receiver.resumableOffers()).size, 0);
  });
});
