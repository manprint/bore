// Unit tests: source transfer actor — auto-accept, relay attach, send
// pipeline, backpressure and abort. Fake offer manager, control channel,
// sockets and counted file slices; real WebCrypto and real framing math.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  attemptKey,
  bytesToHex,
  hexToBytes,
  openFrame,
  sha256Hex,
} from "../../src/crypto.js";
import { canonicalize } from "../../src/protocol.js";
import {
  SEND_HIGH_WATER,
  SENDER_MAX_CONCURRENT,
  createSender,
} from "../../src/sender.js";

const ROOM_KEY = "ab".repeat(32);
const ROOM_ID = "00".repeat(16);
const SELF_PEER = "11".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER = "dd".repeat(16);
const ATTEMPT = "ee".repeat(16);
const FROM_PEER = "ff".repeat(16);

function tick(ms = 10) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function finishTransfer(h, transferId = TRANSFER) {
  await waitFor(() => h.sender.transfers().get(transferId)?.state === "done-pending");
  assert.equal(
    h.sender.handleControl({ type: "transfer.completed", body: { transferId } }),
    true,
  );
}

async function waitFor(done, timeoutMs = 5000) {
  const start = Date.now();
  while (!done()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error("timed out waiting for sender");
    }
    await tick(5);
  }
}

/** Builds a manifest entry set for `bytes` (1 MiB chunks, real digests). */
async function manifestFor(bytes, mtimeSec = 1757779200) {
  const chunks = [];
  for (let at = 0; at < bytes.length; at += 1024 * 1024) {
    chunks.push(bytesToHex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes.slice(at, at + 1024 * 1024)))));
  }
  return {
    entries: [
      {
        id: "0",
        path: "a.bin",
        size: String(bytes.length),
        mtime: String(mtimeSec),
        chunks,
        chunkCount: String(chunks.length),
        root: "00".repeat(32),
      },
    ],
  };
}

function countedFile(bytes, name = "a.bin", lastModified = 1757779200000) {
  const real = new File([bytes], name, { lastModified });
  const calls = [];
  return {
    file: {
      size: real.size,
      lastModified: real.lastModified,
      slice: (start, end) => {
        calls.push([start, end]);
        return real.slice(start, end);
      },
    },
    calls,
  };
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
}

function harness({ fileBytes, mtimeSec, status = "live" } = {}) {
  const bytes = fileBytes ?? new Uint8Array([1, 2, 3, 4, 5]);
  const counted = countedFile(bytes);
  const sockets = [];
  const control = [];
  const withdrawn = [];
  const events = {
    started: [],
    progress: [],
    chunks: [],
    entryDone: [],
    done: [],
    paths: [],
    cancelled: [],
    errors: [],
    sourceChanged: [],
  };
  let manifestPromise = manifestFor(bytes, mtimeSec);
  const manager = {
    records: null,
    offers() {
      return this.records;
    },
    withdrawOffer(offerId) {
      withdrawn.push(offerId);
      return {};
    },
  };
  const sender = createSender({
    offers: manager,
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
    events: {
      onStarted: (info) => events.started.push(info),
      onProgress: (info) => events.progress.push(info),
      onChunk: (transferId, index) => events.chunks.push([transferId, index]),
      onEntryDone: (transferId) => events.entryDone.push(transferId),
      onDone: (transferId, info) => events.done.push([transferId, info]),
      onPath: (transferId, path) => events.paths.push([transferId, path]),
      onCancelled: (transferId, offerId) => events.cancelled.push([transferId, offerId]),
      onError: (transferId, code) => events.errors.push([transferId, code]),
      // Same wiring as main.js: a changed source is withdrawn at once.
      onSourceChanged: (offerId) => {
        events.sourceChanged.push(offerId);
        manager.withdrawOffer(offerId);
      },
    },
  });
  async function publish() {
    const manifest = await manifestPromise;
    manager.records = new Map([
      [
        OFFER,
        {
          status,
          manifest,
          macHex: "ef".repeat(32),
          files: new Map([["a.bin", { file: counted.file }]]),
        },
      ],
    ]);
    return manifest;
  }
  return { sender, manager, control, sockets, events, withdrawn, counted, publish, bytes };
}

function incoming(overrides = {}) {
  return {
    type: "transfer.incoming",
    body: { transferId: TRANSFER, offerId: OFFER, fromPeerId: FROM_PEER, attemptId: ATTEMPT, ...overrides },
  };
}

function ticket(transferId = TRANSFER, attemptId = ATTEMPT) {
  return {
    type: "transfer.relay_ticket",
    body: { transferId, attemptId, ticket: "ab".repeat(16) },
  };
}

function commit(transferId = TRANSFER, attemptId = ATTEMPT, extra = {}) {
  return {
    type: "transfer.path_commit",
    body: { transferId, attemptId, path: "relay", ...extra },
  };
}

describe("web-transfer sender", () => {
  it("sender_serves_a_published_offer_with_its_ack_in_flight", async () => {
    // The request may win the race against our own publish ack: `ready`
    // serves exactly like `live` (the server only sends incoming for offers
    // it already knows).
    const h = harness({ status: "ready" });
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(
      h.control.filter((m) => m.type === "transfer.source_ready").length,
      1,
    );
  });

  it("sender_waits_for_path_commit_before_first_file_read", async () => {
    const h = harness();
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    // Ready left, but no socket and no file reads yet.
    assert.ok(h.control.some((m) => m.type === "transfer.source_ready"));
    assert.equal(h.sockets.length, 0);
    assert.equal(h.counted.calls.length, 0);
    // Ticket opens the leg and attaches; still no reads.
    assert.equal(h.sender.handleControl(ticket()), true);
    assert.equal(h.sockets.length, 1);
    assert.equal(h.sockets[0].url, `ws://127.0.0.1:9/transfer/ws/relay/${ROOM_ID}/${TRANSFER}`);
    h.sockets[0].onopen();
    const attach = JSON.parse(h.sockets[0].sent[0]);
    assert.equal(attach.role, "source");
    assert.equal(attach.peerId, SELF_PEER);
    assert.equal(attach.attemptId, ATTEMPT);
    assert.equal(h.counted.calls.length, 0);
    // Commit starts the pipeline: reads begin, then frames flow.
    assert.equal(h.sender.handleControl(commit()), true);
    await waitFor(() => h.counted.calls.length > 0);
    await finishTransfer(h);
    assert.equal(h.events.done.length, 1);
    assert.ok(h.counted.calls.length >= 1);
  });

  it("relay_leg_is_never_closed_by_the_source_after_final", async () => {
    // `send` only QUEUES. When the close was the end-of-stream signal, a
    // transfer ended when the ENGINE decided to flush — WebKit ended a leg
    // at 7 087 168 bytes of 8 388 613 already written and the server failed
    // the whole transfer as `SourceGone`. The FINAL frame is the signal
    // now, so the source closes nothing, and it does not claim to be done
    // until the transport has actually written everything.
    const h = harness();
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    const socket = h.sockets[0];
    socket.onopen();
    // The transport reports bytes still owed from the first frame onward.
    socket.bufferedAmount = 4096;
    assert.equal(h.sender.handleControl(commit()), true);
    // Every frame has been handed over (data + FINAL) and nothing is done.
    await waitFor(() => socket.sent.length >= 2);
    const final = new Uint8Array(socket.sent[socket.sent.length - 1]);
    assert.equal(final[6], 2, "the last frame on the leg is FINAL");
    await tick(20);
    assert.notEqual(
      h.sender.transfers().get(TRANSFER)?.state,
      "done-pending",
      "done is claimed before the bytes are out",
    );
    // The transport drains: the source is done and STILL has not closed.
    socket.bufferedAmount = 0;
    await waitFor(() => h.sender.transfers().get(TRANSFER)?.state === "done-pending");
    assert.equal(socket.closed, false, "the source closed the leg itself");
    // The server's own close of the leg is what arrives next, and it is not
    // a failure.
    socket.onclose();
    assert.deepEqual(h.events.errors, []);
    assert.ok(h.sender.transfers().has(TRANSFER));
  });

  it("relay_close_after_final_keeps_the_record_for_the_recipients_report", async () => {
    // The relay leg's close IS the end-of-stream signal, so the source
    // closes its own socket right after FINAL. That close used to FORGET
    // the transfer, and with it every later control message about it — so
    // the recipient's `transfer.progress`, which is the source's ONLY way
    // to learn which transport carried the bytes, was dropped and the
    // source's path badge stayed "connecting" on every relayed transfer
    // that ever completed.
    const h = harness();
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    assert.equal(h.sender.handleControl(commit()), true);
    await waitFor(() => h.sender.transfers().get(TRANSFER)?.state === "done-pending");

    // The orderly close, exactly as the browser delivers it.
    h.sockets[0].onclose();
    assert.deepEqual(h.events.errors, [], "an orderly close is not a failure");
    assert.ok(
      h.sender.transfers().has(TRANSFER),
      "the record must outlive the leg: the transfer is not over yet",
    );
    // The transport itself is released — only the bookkeeping survives.
    const kept = h.sender.transfers().get(TRANSFER);
    assert.equal(kept.sink, null);
    assert.equal(kept.socket, null);

    // The recipient's report still lands, and it is what tells the source
    // the path.
    assert.equal(
      h.sender.handleControl({
        type: "transfer.progress",
        body: {
          transferId: TRANSFER,
          attemptId: ATTEMPT,
          receivedBytes: String(h.bytes.length),
          path: "relay",
        },
      }),
      true,
    );
    assert.deepEqual(h.events.paths, [[TRANSFER, "relay"]]);

    // And the server's terminal message is what finally frees it.
    assert.equal(
      h.sender.handleControl({ type: "transfer.completed", body: { transferId: TRANSFER } }),
      true,
    );
    assert.equal(h.events.done.length, 1);
    assert.equal(h.sender.transfers().has(TRANSFER), false);
  });

  it("relay_close_before_final_is_still_a_failure_that_frees_the_record", async () => {
    // The other half of the same branch: a close that arrives while bytes
    // are still owed is a failure, and it must keep freeing the record.
    const h = harness();
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    h.sockets[0].onclose();
    assert.deepEqual(h.events.errors, [[TRANSFER, "FAILED"]]);
    assert.equal(h.sender.transfers().has(TRANSFER), false);
  });

  it("sender_auto_accepts_only_valid_local_offer", async () => {
    // Unknown offer: reject, no ready, nothing tracked.
    {
      const h = harness();
      await h.publish();
      assert.equal(h.sender.handleControl(incoming({ offerId: "00".repeat(16) })), true);
      await tick();
      assert.equal(h.control.filter((m) => m.type === "transfer.source_ready").length, 0);
      const reject = h.control.find((m) => m.type === "transfer.reject");
      assert.equal(reject?.body?.code, "OFFER_NOT_FOUND");
      assert.equal(h.sender.transfers().size, 0);
    }
    // Mutated size: reject plus source-change (main.js withdraws).
    {
      const h = harness();
      const manifest = await h.publish();
      manifest.entries[0].size = "999";
      assert.equal(h.sender.handleControl(incoming()), true);
      await tick();
      const reject = h.control.find((m) => m.type === "transfer.reject");
      assert.equal(reject?.body?.code, "SOURCE_CHANGED");
      assert.deepEqual(h.events.sourceChanged, [OFFER]);
    }
    // Nine concurrent: the ninth hears BUSY.
    {
      const h = harness();
      await h.publish();
      for (let i = 0; i < SENDER_MAX_CONCURRENT; i++) {
        const id = i.toString(16).padStart(32, "0");
        assert.equal(h.sender.handleControl(incoming({ transferId: id, attemptId: id })), true);
      }
      await tick();
      assert.equal(h.control.filter((m) => m.type === "transfer.source_ready").length, SENDER_MAX_CONCURRENT);
      assert.equal(
        h.sender.handleControl(incoming({ transferId: "ff".repeat(16), attemptId: "fe".repeat(16) })),
        true,
      );
      await tick();
      const busy = h.control.filter((m) => m.type === "transfer.reject");
      assert.equal(busy.length, 1);
      assert.equal(busy[0].body.code, "BUSY");
    }
  });

  it("source_incoming_auto_ready_requires_local_file", async () => {
    // No local offer for that ID: the incoming is refused, no ready leaves,
    // no row appears — a source with nothing to send never opens a leg.
    {
      const h = harness();
      await h.publish();
      assert.equal(h.sender.handleControl(incoming({ offerId: "09".repeat(16) })), true);
      await tick();
      assert.equal(h.control.filter((m) => m.type === "transfer.source_ready").length, 0);
      assert.equal(h.control.find((m) => m.type === "transfer.reject")?.body?.code, "OFFER_NOT_FOUND");
      assert.deepEqual(h.events.started, []);
      assert.equal(h.sockets.length, 0);
      assert.equal(h.sender.transfers().size, 0);
    }
    // The offer exists but its file handle is gone (reload, evicted tab):
    // still no ready, still no leg.
    {
      const h = harness();
      const manifest = await h.publish();
      h.manager.records.get(OFFER).files = new Map();
      void manifest;
      assert.equal(h.sender.handleControl(incoming()), true);
      await tick();
      assert.equal(h.control.filter((m) => m.type === "transfer.source_ready").length, 0);
      assert.ok(h.control.some((m) => m.type === "transfer.reject"));
      assert.deepEqual(h.events.started, []);
      assert.equal(h.sender.transfers().size, 0);
    }
    // A valid local file auto-readies with no prompt and opens the row,
    // naming the requester as the party this tab is sending to.
    {
      const h = harness();
      await h.publish();
      assert.equal(h.sender.handleControl(incoming()), true);
      await tick();
      assert.equal(h.control.filter((m) => m.type === "transfer.source_ready").length, 1);
      assert.equal(h.events.started.length, 1);
      assert.equal(h.events.started[0].transferId, TRANSFER);
      assert.equal(h.events.started[0].offerId, OFFER);
      assert.equal(h.events.started[0].recipientPeerId, FROM_PEER);
      assert.equal(h.events.started[0].totalBytes, h.bytes.length);
    }
    // A remote cancel closes the row from the other side.
    {
      const h = harness();
      await h.publish();
      assert.equal(h.sender.handleControl(incoming()), true);
      await tick();
      assert.equal(
        h.sender.handleControl({ type: "transfer.cancelled", body: { transferId: TRANSFER } }),
        true,
      );
      assert.deepEqual(h.events.cancelled, [[TRANSFER, OFFER]]);
      assert.equal(h.sender.transfers().size, 0);
    }
  });

  it("resume_ranges_are_rehashed_but_not_sent", async () => {
    // Three chunks; chunk 1 verified: still read+hashed, never sent.
    const bytes = new Uint8Array(2 * 1024 * 1024 + 100);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = i % 251;
    }
    const h = harness({ fileBytes: bytes });
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    assert.equal(
      h.sender.handleControl(commit(TRANSFER, ATTEMPT, { resumeRanges: [[1, 2]] })),
      true,
    );
    await finishTransfer(h);
    assert.equal(h.events.done.length, 1);
    // Chunk 1 was sliced (rehash) …
    const readOffsets = h.counted.calls.map(([start]) => start);
    assert.ok(readOffsets.includes(1024 * 1024));
    // … but no sent frame covers it: seqs stay continuous over the two
    // shipped chunks and the plaintext reassembles to chunks 0+2.
    const frames = h.sockets[0].sent.filter((m) => ArrayBuffer.isView(m));
    const key = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(TRANSFER), hexToBytes(ATTEMPT));
    let seq = 0;
    const shipped = [];
    for (const raw of frames) {
      const { ftype, seq: got, plaintext } = await openFrame(key, new Uint8Array(raw), seq);
      assert.equal(got, seq);
      seq += 1;
      if (ftype === 1) {
        shipped.push(...plaintext);
      }
    }
    const expected = [...bytes.slice(0, 1024 * 1024), ...bytes.slice(2 * 1024 * 1024)];
    assert.deepEqual(shipped, expected);
    // FINAL counts what travelled on THIS attempt: the recipient checks it
    // against what it asked for, so the whole entry size would fail exactly
    // the resumes this phase exists to support.
    const last = new Uint8Array(frames[frames.length - 1]);
    const { ftype, plaintext } = await openFrame(key, last, seq - 1);
    assert.equal(ftype, 2);
    assert.equal(
      new DataView(plaintext.buffer, plaintext.byteOffset, 8).getBigUint64(0, false),
      BigInt(expected.length),
    );
    assert.notEqual(expected.length, bytes.length);
  });

  it("source_change_aborts_and_withdraws", async () => {
    const bytes = new Uint8Array([10, 20, 30, 40, 50]);
    const h = harness({ fileBytes: bytes });
    const manifest = await h.publish();
    // Same size and mtime, flipped content: incoming validation passes, so
    // the sender readies — the pipeline rehash is what catches it. (The
    // sender snapshots the File at incoming; the swap lands first.)
    const evil = new File([new Uint8Array([10, 20, 31, 40, 50])], "a.bin", {
      lastModified: 1757779200000,
    });
    h.manager.records.get(OFFER).files.set("a.bin", { file: evil });
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    assert.equal(h.sender.handleControl(commit()), true);
    await waitFor(() => h.events.errors.length === 1);
    assert.deepEqual(h.events.errors[0], [TRANSFER, "SOURCE_CHANGED"]);
    assert.deepEqual(h.withdrawn, [OFFER]);
    assert.ok(h.sockets[0].closed, "leg closed on source change");
    assert.ok(
      h.sockets[0].sent.every((m) => typeof m === "string"),
      "no payload frame left the tab",
    );
    assert.equal(manifest.entries[0].size, "5");
  });

  it("one_mib_chunk_fragments_stay_under_all_limits", async () => {
    const bytes = new Uint8Array(1024 * 1024);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = i % 251;
    }
    const h = harness({ fileBytes: bytes });
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    assert.equal(h.sender.handleControl(commit()), true);
    await finishTransfer(h);
    assert.equal(h.events.done.length, 1);
    const frames = h.sockets[0].sent.filter((m) => ArrayBuffer.isView(m));
    // 43 DATA fragments plus the FINAL frame, every message < 32 KiB.
    assert.equal(frames.length, 44);
    for (const raw of frames) {
      assert.ok(raw.byteLength <= 32 * 1024, `frame of ${raw.byteLength} bytes`);
    }
    // Decrypt round-trip: DATA reassembles to the file, FINAL carries it.
    const key = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(TRANSFER), hexToBytes(ATTEMPT));
    let seq = 0;
    const shipped = [];
    let total = null;
    for (const raw of frames) {
      const { ftype, plaintext } = await openFrame(key, new Uint8Array(raw), seq);
      seq += 1;
      if (ftype === 1) {
        shipped.push(...plaintext);
      } else {
        total = new DataView(plaintext.buffer).getBigUint64(0, false);
      }
    }
    assert.deepEqual(shipped, [...bytes]);
    assert.equal(total, BigInt(bytes.length));
  });

  it("sequence_is_monotonic_and_new_attempt_resets_with_new_key", async () => {
    const h = harness();
    await h.publish();
    const keys = [];
    for (const attempt of [ATTEMPT, "ed".repeat(16)]) {
      const id = attempt.slice(0, 31) + "0";
      assert.equal(h.sender.handleControl(incoming({ transferId: id, attemptId: attempt })), true);
      await tick();
      assert.equal(h.sender.handleControl(ticket(id, attempt)), true);
      h.sockets[h.sockets.length - 1].onopen();
      assert.equal(h.sender.handleControl(commit(id, attempt)), true);
      await finishTransfer(h, id);
      assert.equal(h.events.done.length, keys.length + 1);
      keys.push(bytesToHex(await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(id), hexToBytes(attempt))));
      const frames = h.sockets[h.sockets.length - 1].sent.filter((m) => ArrayBuffer.isView(m));
      const key = await attemptKey(hexToBytes(ROOM_KEY), hexToBytes(id), hexToBytes(attempt));
        let seq = 0;
      for (const raw of frames) {
        const opened = await openFrame(key, new Uint8Array(raw), seq);
        assert.equal(opened.seq, seq);
        seq += 1;
      }
      assert.ok(seq > 0);
    }
    assert.notEqual(keys[0], keys[1]);
    // Attempt IDs double as transfer IDs here only to keep the fake small;
    // what matters is the per-attempt reset, proven by both seq runs.
  });

  it("sender_high_low_water_blocks_future_file_reads", async () => {
    const h = harness();
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].bufferedAmount = SEND_HIGH_WATER + 1;
    h.sockets[0].onopen();
    assert.equal(h.sender.handleControl(commit()), true);
    await tick(60);
    assert.equal(h.counted.calls.length, 0, "no reads above the high-water mark");
    h.sockets[0].bufferedAmount = 0;
    await finishTransfer(h);
    assert.equal(h.events.done.length, 1);
    assert.ok(h.counted.calls.length > 0);
  });

  it("abort_stops_reads_crypto_and_socket", async () => {
    const bytes = new Uint8Array(2 * 1024 * 1024 + 10);
    const h = harness({ fileBytes: bytes });
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    let chunksSeen = 0;
    const events = h.events;
    const originalChunk = events.chunks.push.bind(events.chunks);
    events.chunks.push = (...args) => {
      const result = originalChunk(...args);
      chunksSeen += 1;
      if (chunksSeen === 1) {
        h.sender.abortTransfer(TRANSFER);
      }
      return result;
    };
    assert.equal(h.sender.handleControl(commit()), true);
    await waitFor(() => h.sender.transfers().size === 0);
    const reads = h.counted.calls.length;
    await tick(50);
    assert.equal(h.counted.calls.length, reads, "no reads after abort");
    assert.ok(h.sockets[0].closed, "leg closed on abort");
    const finals = h.sockets[0].sent.filter(
      (m) => ArrayBuffer.isView(m) && m.byteLength === 16 + 8 + 16,
    );
    assert.equal(finals.length, 0, "no FINAL after abort");
    assert.equal(h.events.done.length, 0);
    assert.equal(h.events.errors.length, 0, "abort is silent, not an error");
  });

  it("sender_peak_live_buffers_are_one_chunk_plus_one_frame", async () => {
    // Two chunks (43 + 1 fragments): the order log must show every send of
    // chunk 0 before the first read of chunk 1 — no read-ahead.
    const bytes = new Uint8Array(1024 * 1024 + 100);
    const h = harness({ fileBytes: bytes });
    await h.publish();
    assert.equal(h.sender.handleControl(incoming()), true);
    await tick();
    assert.equal(h.sender.handleControl(ticket()), true);
    h.sockets[0].onopen();
    const order = [];
    const origSlice = h.counted.file.slice.bind(h.counted.file);
    h.counted.file.slice = (start, end) => {
      order.push(`read@${start}`);
      return origSlice(start, end);
    };
    const socket = h.sockets[0];
    const origSend = socket.send.bind(socket);
    let sends = 0;
    socket.send = (data) => {
      if (ArrayBuffer.isView(data)) {
        sends += 1;
        order.push(`send#${sends}`);
      }
      return origSend(data);
    };
    assert.equal(h.sender.handleControl(commit()), true);
    await finishTransfer(h);
    assert.equal(h.events.done.length, 1);
    const firstRead1 = order.indexOf(`read@${1024 * 1024}`);
    assert.ok(firstRead1 > 0, "second chunk read");
    const sendsBefore = order.slice(0, firstRead1).filter((e) => e.startsWith("send#")).length;
    assert.equal(sendsBefore, 43, "all 43 chunk-0 fragments sent before chunk-1 read");
  });
});
