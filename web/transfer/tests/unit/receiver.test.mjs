// Unit tests: recipient actor — quota/resume handshake, strict frame
// acceptance, crash-safe commit order, cancel/purge lifecycles and the
// explicit save step. Fake control, sockets and repository; real WebCrypto.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  attemptKey,
  bytesToHex,
  hexToBytes,
  manifestMac,
  sealFrame,
  sha256Hex,
} from "../../src/crypto.js";
import { canonicalize, manifestValue } from "../../src/protocol.js";
import { FRAME_PIPELINE_DEPTH, createReceiver } from "../../src/receiver.js";
import { chunkPartName, createRepository } from "../../src/storage.js";
import { fakeBackends } from "./fakes.mjs";

/** The shared control-message corpus: the wire shape both ends agree on. */
const fixtures = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "..",
  "tests",
  "fixtures",
  "web_transfer",
  "v1",
);

function fixtureBody(name) {
  const corpus = JSON.parse(readFileSync(join(fixtures, "control-messages.json"), "utf8"));
  const entry = corpus.find((message) => message.name === name);
  assert.ok(entry, `fixture ${name} must exist`);
  return JSON.parse(entry.json);
}

const ROOM_KEY = "ab".repeat(32);
const ROOM_ID = "00".repeat(16);
const SELF_PEER = "22".repeat(16);
const SOURCE_PEER = "11".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER = "dd".repeat(16);
const ATTEMPT = "ee".repeat(16);

function tick(ms = 10) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(done, timeoutMs = 5000) {
  const start = Date.now();
  while (!done()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error("timed out waiting for receiver");
    }
    await tick(5);
  }
}

async function manifestFor(bytes, mtimeSec = 1757779200) {
  const chunks = [];
  for (let at = 0; at < bytes.length; at += 1024 * 1024) {
    chunks.push(
      bytesToHex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes.slice(at, at + 1024 * 1024)))),
    );
  }
  const manifest = {
    offer: OFFER,
    mode: "single",
    label: "a.bin",
    kind: "file",
    chunkSize: String(1024 * 1024),
    createdAt: "2026-09-15T00:00:00Z",
    entries: [
      {
        id: "0",
        path: "a.bin",
        size: String(bytes.length),
        mtime: String(mtimeSec),
        chunks,
        chunkCount: String(chunks.length),
        root: "ff".repeat(32),
      },
    ],
  };
  // The recipient refuses a manifest it cannot authenticate under the room
  // key, so a fixture manifest carries its real tag — a constant here would
  // only test the refusal.
  MACS.set(
    manifest,
    bytesToHex(
      await manifestMac(
        hexToBytes(ROOM_KEY),
        hexToBytes(ROOM_ID),
        new TextEncoder().encode(canonicalize(manifestValue(manifest))),
      ),
    ),
  );
  return manifest;
}

/** Room tag of a fixture manifest, as `manifestFor` computed it. */
const MACS = new WeakMap();
function macOf(manifest) {
  const mac = MACS.get(manifest);
  assert.ok(mac, "fixture manifest has no tag");
  return mac;
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

function harness({ backends = fakeBackends(), estimate } = {}) {
  const control = [];
  const sockets = [];
  const events = { progress: [], complete: [], staged: [], errors: [] };
  const repository =
    estimate !== undefined
      ? createRepository({ ...backends, estimateStorage: estimate })
      : createRepository(backends);
  const receiver = createReceiver({
    sendControl: (message) => {
      control.push(message);
      return true;
    },
    createSocket: (url, protocol) => {
      const socket = new FakeSocket();
      socket.url = url;
      socket.protocol = protocol;
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
      onComplete: (info) => events.complete.push(info),
      onStaged: (info) => events.staged.push(info),
      onError: (transferId, code) => events.errors.push([transferId, code]),
    },
  });
  return { receiver, repository, control, sockets, events };
}

async function startFlow(h, bytes, { resume = true } = {}) {
  const manifest = await manifestFor(bytes);
  const offer = { offerId: OFFER, manifest, macHex: macOf(manifest), sourcePeerId: SOURCE_PEER };
  const started = await h.receiver.startDownload(offer);
  assert.deepEqual(started, { pending: true });
  const request = h.control.find((m) => m.type === "transfer.request");
  assert.ok(request);
  if (resume) {
    assert.ok(!("resume" in request.body), "first download carries no resume");
  }
  const transferId = "aa".repeat(16);
  assert.equal(
    h.receiver.handleControl({
      type: "ack",
      requestId: request.requestId,
      body: { result: { transferId } },
    }),
    true,
  );
  assert.equal(
    h.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId, attemptId: ATTEMPT, ticket: "ab".repeat(16) },
    }),
    true,
  );
  await tick(20);
  assert.equal(h.sockets.length, 1);
  h.sockets[0].onopen();
  const attach = JSON.parse(h.sockets[0].sent[0]);
  assert.equal(attach.role, "recipient");
  assert.equal(attach.peerId, SELF_PEER);
  assert.equal(
    h.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId, attemptId: ATTEMPT, path: "relay" },
    }),
    true,
  );
  const key = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(transferId), hexToBytes(ATTEMPT));
  return { manifest, transferId, key };
}

/** The digest the receiver derives for a whole single-entry raw selection. */
async function selectionDigestFor(mac, entryId = "0") {
  return sha256Hex(
    new TextEncoder().encode(
      canonicalize({ entryIds: [entryId], manifestMac: mac, mode: "raw", offerId: OFFER }),
    ),
  );
}

/** Writes staged parts and a resume record exactly as a cancelled run left them. */
async function seedPartial(repository, entry, parts, ranges, mac) {
  const digest = await selectionDigestFor(mac, entry.id);
  const dir = ["bore-transfer-v1", ROOM_ID, OFFER, digest];
  for (const [index, bytes] of parts) {
    await repository.writeChunk(dir, chunkPartName(entry.id, index), bytes);
  }
  await repository.saveRecord([ROOM_ID, SOURCE_PEER, OFFER, digest], {
    manifestMac: mac,
    roomId: ROOM_ID,
    offerId: OFFER,
    digest,
    kind: "file",
    path: entry.path,
    size: entry.size,
    root: entry.root,
    verifiedRanges: ranges,
    chunkDigests: {},
    updatedAt: "2026-09-15T00:00:00Z",
  });
  return { digest, dir };
}

describe("web-transfer receiver", () => {
  it("a_manifest_that_does_not_authenticate_is_refused_before_anything_leaves", async () => {
    // The manifest reaches the recipient THROUGH the server, which holds no
    // room key and so cannot produce this tag. Without the check, a forged
    // manifest would substitute its own roots and every later per-chunk
    // digest would verify against the forgery — a download that reports
    // success and hands over attacker-chosen bytes.
    const h = harness();
    const manifest = await manifestFor(new Uint8Array([1, 2, 3]));
    for (const macHex of [
      "ef".repeat(32), // a tag from another key
      macOf(manifest).slice(0, 63) + (macOf(manifest)[63] === "0" ? "1" : "0"), // one nibble off
      "",
      null,
    ]) {
      assert.deepEqual(
        await h.receiver.startDownload({
          offerId: OFFER,
          manifest,
          macHex,
          sourcePeerId: SOURCE_PEER,
        }),
        { error: "MANIFEST_MAC" },
      );
    }
    // Same manifest, one entry substituted under the ORIGINAL tag: the
    // substitution is exactly what the tag covers.
    const forged = { ...manifest, entries: [{ ...manifest.entries[0], root: "aa".repeat(32) }] };
    assert.deepEqual(
      await h.receiver.startDownload({
        offerId: OFFER,
        manifest: forged,
        macHex: macOf(manifest),
        sourcePeerId: SOURCE_PEER,
      }),
      { error: "MANIFEST_MAC" },
    );
    assert.ok(!h.control.some((m) => m.type === "transfer.request"), "no request left the tab");
    assert.equal(h.sockets.length, 0, "no relay socket was opened");
  });

  it("a_multi_entry_offer_is_refused_as_unsupported_not_as_missing", async () => {
    // A folder/multi-file offer is announced and signed like any other, so it
    // is on screen and clickable; only the download side is single-entry in
    // this release. Reporting OFFER_NOT_FOUND for something plainly visible
    // sends the user hunting for a room problem that does not exist.
    const h = harness();
    const bytes = new Uint8Array([1, 2, 3]);
    const one = await manifestFor(bytes);
    const many = {
      ...one,
      kind: "folder",
      entries: [one.entries[0], { ...one.entries[0], id: "1", path: "b.bin" }],
    };
    // Re-sign, or the MAC gate would answer first and hide the case.
    const mac = bytesToHex(
      await manifestMac(
        hexToBytes(ROOM_KEY),
        hexToBytes(ROOM_ID),
        new TextEncoder().encode(canonicalize(manifestValue(many))),
      ),
    );
    assert.deepEqual(
      await h.receiver.startDownload({
        offerId: OFFER,
        manifest: many,
        macHex: mac,
        sourcePeerId: SOURCE_PEER,
      }),
      { error: "MULTI_ENTRY" },
    );
    assert.ok(!h.control.some((m) => m.type === "transfer.request"), "no request left the tab");
    assert.equal(h.sockets.length, 0, "no relay socket was opened");
  });

  it("no_opfs_disables_only_downloads", async () => {
    const h = harness({
      backends: {
        getDirectory: async () => {
          throw new Error("no OPFS");
        },
        openIDB: () => {
          throw new Error("no IDB");
        },
        estimateStorage: async () => ({}),
      },
    });
    const manifest = await manifestFor(new Uint8Array([1, 2, 3]));
    const result = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: macOf(manifest),
      sourcePeerId: SOURCE_PEER,
    });
    assert.deepEqual(result, { error: "UNSUPPORTED" });
    assert.ok(!h.control.some((m) => m.type === "transfer.request"), "no request left the tab");
  });

  it("receiver_rejects_gap_duplicate_old_attempt_wrong_tag_digest_offset_and_root", async () => {
    // Gap: seq 0 then seq 2.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes.slice(0, 24)));
      h.sockets[0].emit(await sealFrame(key, 2, 1, bytes.slice(24, 48)));
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Duplicate: seq 0 twice.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      const frame = await sealFrame(key, 0, 1, bytes.slice(0, 24));
      h.sockets[0].emit(frame);
      h.sockets[0].emit(frame);
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Old attempt: sealed under another attempt key.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId } = await startFlow(h, bytes);
      const other = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(transferId), hexToBytes("ed".repeat(16)));
      h.sockets[0].emit(await sealFrame(other, 0, 1, bytes.slice(0, 24)));
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Wrong tag: flipped ciphertext bit.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      const frame = new Uint8Array(await sealFrame(key, 0, 1, bytes.slice(0, 24)));
      frame[frame.length - 1] ^= 1;
      h.sockets[0].emit(frame.buffer);
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Digest mismatch: valid AEAD, wrong chunk content.
    {
      const h = harness();
      const bytes = new Uint8Array(100).fill(7);
      const { transferId, key } = await startFlow(h, bytes);
      const evil = new Uint8Array(100).fill(8);
      h.sockets[0].emit(await sealFrame(key, 0, 1, evil.slice(0, 100)));
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Offset overrun: a second frame for a one-fragment chunk.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes.slice(0, 100)));
      await tick(20);
      h.sockets[0].emit(await sealFrame(key, 1, 1, bytes.slice(0, 100)));
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
    // Bad root: FINAL total disagrees with the manifest.
    {
      const h = harness();
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes.slice(0, 100)));
      await tick(20);
      const total = new Uint8Array(8);
      new DataView(total.buffer).setBigUint64(0, 999n, false);
      h.sockets[0].emit(await sealFrame(key, 1, 2, total));
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"]);
    }
  });

  it("crypto_failure_writes_zero_bytes", async () => {
    // The rejection table above proves a bad frame FAILS the transfer. This
    // proves the other half, the one that matters for the disk: a frame that
    // does not authenticate must leave nothing behind. A receiver that wrote
    // first and verified second would hand an attacker a way to place chosen
    // bytes in the recipient's storage without ever holding the room key.
    for (const [name, corrupt] of [
      [
        "wrong key",
        async (key, bytes) => {
          const other = await attemptKey(
            hexToBytes(ROOM_KEY),
            hexToBytes("aa".repeat(16)),
            hexToBytes("ed".repeat(16)),
          );
          void key;
          return new Uint8Array(await sealFrame(other, 0, 1, bytes.slice(0, 24)));
        },
      ],
      [
        "flipped ciphertext bit",
        async (key, bytes) => {
          const frame = new Uint8Array(await sealFrame(key, 0, 1, bytes.slice(0, 24)));
          frame[16] ^= 1;
          return frame;
        },
      ],
      [
        "flipped tag bit",
        async (key, bytes) => {
          const frame = new Uint8Array(await sealFrame(key, 0, 1, bytes.slice(0, 24)));
          frame[frame.length - 1] ^= 0x80;
          return frame;
        },
      ],
      [
        "truncated frame",
        async (key, bytes) => {
          const frame = new Uint8Array(await sealFrame(key, 0, 1, bytes.slice(0, 24)));
          return frame.slice(0, frame.length - 4);
        },
      ],
    ]) {
      let writes = 0;
      const backends = fakeBackends({ onOpfsClose: () => (writes += 1) });
      const h = harness({ backends });
      const bytes = new Uint8Array(100);
      const { transferId, key } = await startFlow(h, bytes);
      const before = writes;
      h.sockets[0].emit((await corrupt(key, bytes)).buffer);
      await waitFor(() => h.events.errors.length === 1);
      assert.deepEqual(h.events.errors[0], [transferId, "FAILED"], name);
      assert.equal(writes, before, `${name} wrote ${writes - before} chunk(s) to storage`);
      assert.equal(h.events.complete.length, 0, `${name} completed`);
      assert.equal(h.events.staged.length, 0, `${name} staged a file`);
    }

    // ... and the same harness DOES write when the frame authenticates, so
    // the assertion above is about the rejection and not about a receiver
    // that never writes at all.
    {
      let writes = 0;
      const backends = fakeBackends({ onOpfsClose: () => (writes += 1) });
      const h = harness({ backends });
      const bytes = new Uint8Array(100).fill(3);
      const { key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes.slice(0, 100)));
      await waitFor(() => writes > 0);
      assert.ok(writes > 0, "a good frame must reach storage");
    }
  });

  it("pipelined_open_preserves_frame_order_and_bytes", async () => {
    // A burst larger than the pipeline depth, delivered in ONE tick: the
    // frames are opened several at a time and must still land in arrival
    // order, byte for byte. The depth is what the engines reward (webkit
    // 39.60 -> 81.84 MiB/s at 32 MiB, firefox 41.13 -> 44.63, chromium flat).
    const h = harness();
    const size = 24 * 1024 * (FRAME_PIPELINE_DEPTH * 2 + 3);
    const bytes = new Uint8Array(size).map((_, i) => (i * 7 + 3) % 251);
    const { manifest, key, transferId } = await startFlow(h, bytes);
    let seq = 0;
    for (let at = 0; at < bytes.length; at += 24 * 1024) {
      h.sockets[0].emit(await sealFrame(key, seq, 1, bytes.subarray(at, at + 24 * 1024)));
      seq += 1;
    }
    const total = new Uint8Array(8);
    new DataView(total.buffer).setBigUint64(0, BigInt(bytes.length), false);
    h.sockets[0].emit(await sealFrame(key, seq, 2, total));
    await waitFor(() => h.control.some((m) => m.type === "transfer.complete"));
    const complete = h.control.find((m) => m.type === "transfer.complete");
    assert.equal(complete.body.root, manifest.entries[0].root);
    assert.deepEqual(h.events.errors, []);
    assert.equal(
      h.receiver.handleControl({
        type: "ack",
        requestId: complete.requestId,
        body: { result: {} },
      }),
      true,
    );
    await waitFor(() => h.events.staged.length === 1);
    // The staged file really holds the bytes, in order: a pipeline that
    // reordered a single fragment would pass the digests of no chunk.
    const staged = h.receiver.staged().get(transferId);
    const landed = new Uint8Array(await staged.blob.arrayBuffer());
    assert.equal(landed.length, bytes.length);
    assert.deepEqual(landed, bytes);
  });

  it("relay_frames_arrive_as_arraybuffer_not_blob", async () => {
    const h = harness();
    const bytes = new Uint8Array(1024).map((_, i) => i % 251);
    const { key, transferId } = await startFlow(h, bytes);
    // The leg is binary and every frame is opened one by one, so taking the
    // messages as ArrayBuffer skips a Blob per message. MEASURED (3.10):
    // with the default `blob`, `await data.arrayBuffer()` alone was 366 ms
    // of a 630 ms 32 MiB transfer and the whole path ran at 50.8 MiB/s
    // against 82.8 with this line — one variable, five repetitions each.
    assert.equal(h.sockets[0].binaryType, "arraybuffer");
    // And an ArrayBuffer really is accepted by the frame path, which is what
    // the change relies on (the Uint8Array branch stays for the fakes).
    const frame = await sealFrame(key, 0, 1, bytes);
    h.sockets[0].emit(frame.buffer.slice(frame.byteOffset, frame.byteOffset + frame.byteLength));
    await waitFor(() => h.events.progress.length > 0);
    assert.equal(h.events.progress[0].transferId, transferId);
  });

  it("cancel_retains_partial_but_withdraw_room_close_purge", async () => {
    const backends = fakeBackends();
    const h = harness({ backends });
    const bytes = new Uint8Array(1024 * 1024 + 100);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = i % 251;
    }
    const manifest = await manifestFor(bytes);
    const { transferId, key } = await startFlow(h, bytes);
    // Commit chunk 0 whole (43 full fragments + tail), then cancel.
    let seq = 0;
    for (let at = 0; at < 1024 * 1024; at += 24 * 1024) {
      h.sockets[0].emit(
        await sealFrame(key, seq, 1, bytes.slice(at, Math.min(at + 24 * 1024, 1024 * 1024))),
      );
      seq += 1;
    }
    await tick(50);
    assert.equal(h.events.errors.length, 0);
    // Cancel keeps the partial: bytes and record stay.
    assert.equal(h.receiver.cancelTransfer(transferId), true);
    const keys = await h.repository.listKeys();
    assert.equal(keys.length, 1);
    const stored = await h.repository.loadRecord(keys[0]);
    assert.ok(stored !== undefined && stored !== null, "partial record retained");
    assert.deepEqual(stored.verifiedRanges, [[0, 1]]);
    // A fresh download resumes it: the request carries chunk 0 as verified.
    const second = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: macOf(manifest),
      sourcePeerId: SOURCE_PEER,
    });
    assert.deepEqual(second, { pending: true });
    const request2 = h.control.filter((m) => m.type === "transfer.request").pop();
    assert.deepEqual(request2.body.resume.verifiedRanges, [[0, 1]]);
    // Withdraw purges bytes and record.
    await h.receiver.purgeOffer(OFFER);
    assert.equal((await h.repository.listKeys()).length, 0);
  });

  it("verified_file_requires_explicit_save_or_discard_to_purge", async () => {
    const revoked = [];
    const realRevoke = globalThis.URL.revokeObjectURL.bind(globalThis.URL);
    globalThis.URL.revokeObjectURL = (url) => {
      revoked.push(url);
      return realRevoke(url);
    };
    try {
      const backends = fakeBackends();
      const h = harness({ backends });
      const bytes = new Uint8Array([9, 8, 7, 6]);
      const { manifest, transferId, key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes));
      await tick(30);
      const total = new Uint8Array(8);
      new DataView(total.buffer).setBigUint64(0, BigInt(bytes.length), false);
      h.sockets[0].emit(await sealFrame(key, 1, 2, total));
      await tick(30);
      const complete = h.control.find((m) => m.type === "transfer.complete");
      assert.ok(complete, "recipient completes after verify");
      assert.equal(complete.body.root, manifest.entries[0].root);
      // The recipient learns completion from its complete-request ack
      // (the server echoes `transfer.completed` only to the source).
      const completeReq = h.control.filter((m) => m.type === "transfer.complete").pop();
      assert.ok(completeReq);
      assert.equal(
        h.receiver.handleControl({
          type: "ack",
          requestId: completeReq.requestId,
          body: { result: {} },
        }),
        true,
      );
      await waitFor(() => h.events.staged.length === 1);
      // Verified but untouched: bytes and record persist until save/discard.
      const staged = h.receiver.staged().get(transferId);
      assert.ok(staged && typeof staged.url === "string");
      const keysBefore = await h.repository.listKeys();
      assert.equal(keysBefore.length, 1);
      assert.equal(await h.receiver.confirmSaved(transferId), true);
      assert.ok(revoked.includes(staged.url), "URL revoked on save");
      assert.equal((await h.repository.listKeys()).length, 0);
      assert.equal(h.receiver.staged().size, 0);
      // Again, then discard instead of saving.
      const second = await h.receiver.startDownload({
        offerId: OFFER,
        manifest,
        macHex: macOf(manifest),
        sourcePeerId: SOURCE_PEER,
      });
      assert.deepEqual(second, { pending: true });
    } finally {
      globalThis.URL.revokeObjectURL = realRevoke;
    }
  });

  it("object_urls_are_revoked", async () => {
    const revoked = [];
    const realRevoke = globalThis.URL.revokeObjectURL.bind(globalThis.URL);
    globalThis.URL.revokeObjectURL = (url) => {
      revoked.push(url);
      return undefined;
    };
    try {
      const h = harness();
      const bytes = new Uint8Array([1, 2, 3]);
      const { manifest, transferId, key } = await startFlow(h, bytes);
      h.sockets[0].emit(await sealFrame(key, 0, 1, bytes));
      await tick(30);
      const total = new Uint8Array(8);
      new DataView(total.buffer).setBigUint64(0, BigInt(bytes.length), false);
      h.sockets[0].emit(await sealFrame(key, 1, 2, total));
      await tick(30);
      // The recipient learns completion from its complete-request ack
      // (the server echoes `transfer.completed` only to the source).
      const completeReq = h.control.filter((m) => m.type === "transfer.complete").pop();
      assert.ok(completeReq);
      assert.equal(
        h.receiver.handleControl({
          type: "ack",
          requestId: completeReq.requestId,
          body: { result: {} },
        }),
        true,
      );
      await waitFor(() => h.events.staged.length === 1);
      const staged = h.receiver.staged().get(transferId);
      assert.equal(await h.receiver.discardStaged(transferId), true);
      assert.deepEqual(revoked, [staged.url]);
      assert.equal(await h.receiver.confirmSaved(transferId), false);
      assert.equal(await h.receiver.discardStaged(transferId), false);
      // Anchor path: the in-flight fetch keeps its URL while staging purges.
      const manifest2 = manifest;
      const again = await h.receiver.startDownload({
        offerId: OFFER,
        manifest: manifest2,
        macHex: macOf(manifest),
        sourcePeerId: SOURCE_PEER,
      });
      assert.deepEqual(again, { pending: true });
      const request2 = h.control.filter((m) => m.type === "transfer.request").pop();
      const transferId2 = "ab".repeat(16);
      assert.equal(
        h.receiver.handleControl({
          type: "ack",
          requestId: request2.requestId,
          body: { result: { transferId: transferId2 } },
        }),
        true,
      );
      assert.equal(
        h.receiver.handleControl({
          type: "transfer.relay_ticket",
          body: { transferId: transferId2, attemptId: ATTEMPT, ticket: "cd".repeat(16) },
        }),
        true,
      );
      await tick(20);
      h.sockets[1].onopen();
      assert.equal(
        h.receiver.handleControl({
          type: "transfer.path_commit",
          body: { transferId: transferId2, attemptId: ATTEMPT, path: "relay" },
        }),
        true,
      );
      const key2 = await attemptKey(
        hexToBytes(ROOM_KEY),
        hexToBytes(transferId2),
        hexToBytes(ATTEMPT),
      );
      h.sockets[1].emit(await sealFrame(key2, 0, 1, bytes));
      await tick(30);
      const total2 = new Uint8Array(8);
      new DataView(total2.buffer).setBigUint64(0, BigInt(bytes.length), false);
      h.sockets[1].emit(await sealFrame(key2, 1, 2, total2));
      await tick(30);
      const completeReq2 = h.control.filter((m) => m.type === "transfer.complete").pop();
      assert.equal(
        h.receiver.handleControl({
          type: "ack",
          requestId: completeReq2.requestId,
          body: { result: {} },
        }),
        true,
      );
      await waitFor(() => h.events.staged.length === 2);
      const staged2 = h.receiver.staged().get(transferId2);
      assert.equal(
        await h.receiver.confirmSaved(transferId2, { keepUrl: true, keepBytes: true }),
        true,
      );
      assert.ok(!revoked.includes(staged2.url), "anchor fetch keeps its URL");
      assert.equal(h.receiver.staged().size, 0);
      // Record purged, bytes retained for the in-flight anchor fetch.
      assert.equal((await h.repository.listKeys()).length, 0);
      const digest2 = h.control
        .filter((m) => m.type === "transfer.request")
        .pop().body.selectionDigest;
      const retained = await h.repository.readChunk(
        ["bore-transfer-v1", ROOM_ID, OFFER, digest2],
        chunkPartName("0", 0),
      );
      assert.deepEqual([...retained], [1, 2, 3]);
    } finally {
      globalThis.URL.revokeObjectURL = realRevoke;
    }
  });
  it("resume_rehashes_and_drops_corrupt_or_truncated_chunks", async () => {
    const backends = fakeBackends();
    const h = harness({ backends });
    const bytes = new Uint8Array(2 * 1024 * 1024 + 100);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = (i * 31 + 7) % 251;
    }
    const manifest = await manifestFor(bytes);
    const entry = manifest.entries[0];
    assert.equal(entry.chunkCount, "3");
    const corrupt = bytes.slice(1024 * 1024, 2 * 1024 * 1024);
    corrupt[5] ^= 0xff;
    const { dir } = await seedPartial(
      h.repository,
      entry,
      [
        [0, bytes.slice(0, 1024 * 1024)],
        [1, corrupt],
        // Truncated: the writable closed before the chunk was whole.
        [2, bytes.slice(2 * 1024 * 1024, 2 * 1024 * 1024 + 40)],
      ],
      [[0, 3]],
      macOf(manifest),
    );

    const started = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: macOf(manifest),
      sourcePeerId: SOURCE_PEER,
    });
    assert.deepEqual(started, { pending: true });
    const request = h.control.filter((m) => m.type === "transfer.request").pop();
    // Only the intact chunk survives the rehash.
    assert.deepEqual(request.body.resume.verifiedRanges, [[0, 1]]);
    assert.equal(request.body.resume.outputLength, Number(entry.size));
    const stored = await h.repository.loadRecord([ROOM_ID, SOURCE_PEER, OFFER, request.body.selectionDigest]);
    assert.deepEqual(stored.verifiedRanges, [[0, 1]]);
    assert.deepEqual(Object.keys(stored.chunkDigests), ["0"]);
    // The rejected bytes are gone, so they cannot be presented as verified.
    assert.ok((await h.repository.readChunk(dir, chunkPartName("0", 0))) !== null);
    assert.equal(await h.repository.readChunk(dir, chunkPartName("0", 1)), null);
    assert.equal(await h.repository.readChunk(dir, chunkPartName("0", 2)), null);
  });

  it("receiver_places_chunks_by_plan_when_the_source_skips_verified_ranges", async () => {
    const backends = fakeBackends();
    const h = harness({ backends });
    const bytes = new Uint8Array(2 * 1024 * 1024 + 100);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = (i * 17 + 3) % 251;
    }
    const manifest = await manifestFor(bytes);
    const entry = manifest.entries[0];
    const { dir } = await seedPartial(
      h.repository,
      entry,
      [[0, bytes.slice(0, 1024 * 1024)]],
      [[0, 1]],
      macOf(manifest),
    );

    const started = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: macOf(manifest),
      sourcePeerId: SOURCE_PEER,
    });
    assert.deepEqual(started, { pending: true });
    const request = h.control.filter((m) => m.type === "transfer.request").pop();
    assert.deepEqual(request.body.resume.verifiedRanges, [[0, 1]]);

    const transferId = "aa".repeat(16);
    h.receiver.handleControl({
      type: "ack",
      requestId: request.requestId,
      body: { result: { transferId } },
    });
    h.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId, attemptId: ATTEMPT, ticket: "ab".repeat(16) },
    });
    await tick(20);
    h.sockets[0].onopen();
    h.receiver.handleControl({
      type: "transfer.path_commit",
      // The server echoes the request's resume on the commit, and the
      // commit is what BOTH ends plan from — see `applyCommitPlan`.
      body: { transferId, attemptId: ATTEMPT, path: "relay", resumeRanges: [[0, 1]] },
    });
    const key = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(transferId), hexToBytes(ATTEMPT));

    // The source skips chunk 0: the first frame on the wire is chunk 1.
    let seq = 0;
    for (const [from, to] of [
      [1024 * 1024, 2 * 1024 * 1024],
      [2 * 1024 * 1024, bytes.length],
    ]) {
      for (let at = from; at < to; at += 24 * 1024) {
        h.sockets[0].emit(await sealFrame(key, seq, 1, bytes.slice(at, Math.min(at + 24 * 1024, to))));
        seq += 1;
      }
      await tick(40);
    }
    assert.deepEqual(h.events.errors, []);
    const total = new Uint8Array(8);
    // FINAL counts only what travelled: chunks 1 and 2.
    new DataView(total.buffer).setBigUint64(0, BigInt(bytes.length - 1024 * 1024), false);
    h.sockets[0].emit(await sealFrame(key, seq, 2, total));
    await tick(40);
    const completeReq = h.control.filter((m) => m.type === "transfer.complete").pop();
    assert.ok(completeReq, "a resumed transfer still completes");
    h.receiver.handleControl({
      type: "ack",
      requestId: completeReq.requestId,
      body: { result: {} },
    });
    await waitFor(() => h.events.staged.length === 1);

    // Every chunk sits at its own offset, so the staged output is the file.
    const names = [0, 1, 2].map((index) => chunkPartName(entry.id, index));
    const blob = await h.repository.stagedBlob(dir, names);
    assert.equal(blob.size, bytes.length);
    assert.deepEqual([...new Uint8Array(await blob.arrayBuffer())], [...bytes]);
  });

  it("direct_failed_notice_fails_the_matching_transfer", async () => {
    const h = harness();
    const bytes = new Uint8Array(10);
    const { transferId } = await startFlow(h, bytes);
    // The server names the transfer in `message` and in no other field
    // (`error_envelope_anon`): the shape comes from the shared fixture, so
    // this branch cannot go dead again while its test keeps passing.
    const wire = fixtureBody("error.direct_failed");
    assert.equal(wire.body.code, "DIRECT_FAILED");
    assert.equal(typeof wire.body.message, "string");
    assert.ok(!("transferId" in wire.body));
    assert.equal(
      h.receiver.handleControl({
        type: "error",
        body: { ...wire.body, message: transferId },
      }),
      true,
    );
    assert.deepEqual(h.events.errors, [[transferId, "DIRECT_FAILED"]]);
    assert.equal(h.receiver.transfers().size, 0);
    // A notice for an unknown transfer is not consumed.
    assert.equal(
      h.receiver.handleControl({
        type: "error",
        body: { code: "DIRECT_FAILED", message: "ff".repeat(16) },
      }),
      false,
    );
  });

  it("cancel_aborts_before_control_send", async () => {
    const h = harness();
    const bytes = new Uint8Array(10);
    const { transferId } = await startFlow(h, bytes);
    const live = h.receiver.transfers().get(transferId);
    assert.ok(live);
    // Record what was already true WHEN the cancel message was built: the
    // abort must have happened first, so a control channel that blocks (or
    // is already gone) cannot leave the leg reading bytes.
    let abortedAtSend = null;
    const socket = h.sockets[0];
    h.control.length = 0;
    const originalPush = h.control.push.bind(h.control);
    h.control.push = (message) => {
      if (message.type === "transfer.cancel" && abortedAtSend === null) {
        abortedAtSend = live.abort.signal.aborted;
      }
      return originalPush(message);
    };
    assert.equal(h.receiver.cancelTransfer(transferId), true);
    assert.equal(abortedAtSend, true, "abort must precede the control send");
    const cancel = h.control.find((m) => m.type === "transfer.cancel");
    assert.ok(cancel);
    assert.equal(cancel.body.transferId, transferId);
    assert.match(cancel.requestId, /^[0-9a-f]{32}$/);
    assert.equal(socket.closed, true, "the relay leg closes with the abort");
    assert.equal(h.receiver.transfers().size, 0);
    // Cancelling twice is a local no-op and sends nothing further.
    assert.equal(h.receiver.cancelTransfer(transferId), false);
    assert.equal(h.control.filter((m) => m.type === "transfer.cancel").length, 1);
  });

  it("cancel_retains_the_partial_and_offers_it_for_resume", async () => {
    // A cancel must leave what was verified: the offer comes back as
    // resumable and the next click sends the ranges it already holds.
    const h = harness();
    const bytes = new Uint8Array(3);
    bytes.set([7, 8, 9]);
    const manifest = await manifestFor(bytes);
    const entry = manifest.entries[0];
    await seedPartial(h.repository, entry, [[0, bytes]], [[0, 1]], macOf(manifest));
    const resumable = await h.receiver.resumableOffers();
    assert.deepEqual([...resumable], [OFFER]);
    const started = await h.receiver.startDownload({
      offerId: OFFER,
      manifest,
      macHex: macOf(manifest),
      sourcePeerId: SOURCE_PEER,
    });
    assert.deepEqual(started, { pending: true });
    const request = h.control.filter((m) => m.type === "transfer.request").pop();
    assert.deepEqual(request.body.resume.verifiedRanges, [[0, 1]]);
    // The shape the server parses: ranges as integers and `outputLength` as
    // a NUMBER, not the manifest's decimal string (which it rejects as
    // INVALID_MESSAGE, taking the whole resume with it).
    const wire = fixtureBody("transfer.request.resume");
    assert.equal(typeof wire.body.resume.outputLength, "number");
    assert.equal(typeof request.body.resume.outputLength, "number");
    assert.equal(request.body.resume.outputLength, Number(entry.size));
    assert.deepEqual(Object.keys(request.body.resume).sort(), ["outputLength", "verifiedRanges"]);
  });
});
