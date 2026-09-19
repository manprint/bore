// Unit tests: 4.3 attempt coordination — one row per TransferId with a
// replaceable attempt under it, exactly one automatic direct→relay
// fallback, a path badge nothing but a verified chunk (recipient) or the
// server's own forwarded report (source) may set, and progress that never
// walks backwards across the change of transport. Real WebCrypto and real
// framing; only the transports are doubles.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  attemptKey,
  bytesToHex,
  hexToBytes,
  manifestMac,
  openFrame,
  sealFrame,
} from "../../src/crypto.js";
import { FRAME_FINAL } from "../../src/framing.js";
import { reorderBudget } from "../../src/receiver.js";
import { canonicalize, manifestValue } from "../../src/protocol.js";
import { createReceiver } from "../../src/receiver.js";
import { createSender } from "../../src/sender.js";
import { PATH, TRANSFER, createInitialState, reduce } from "../../src/state.js";
import { createRepository } from "../../src/storage.js";
import { fakeBackends } from "./fakes.mjs";

const ROOM_KEY = "ab".repeat(32);
const ROOM_ID = "00".repeat(16);
const SOURCE_PEER = "11".repeat(16);
const SINK_PEER = "22".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER_ID = "dd".repeat(16);
const ATTEMPT_A = "ee".repeat(16);
const ATTEMPT_B = "77".repeat(16);
const CHUNK = 1024 * 1024;
const FRAGMENT = 24 * 1024;

function tick(ms = 5) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(done, what = "state", timeoutMs = 5000) {
  const start = Date.now();
  while (!done()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await tick(5);
  }
}

/** A deterministic payload: every byte depends on its own offset. */
function payload(bytes) {
  const out = new Uint8Array(bytes);
  for (let at = 0; at < bytes; at++) {
    out[at] = (at * 31 + (at >> 13)) & 0xff;
  }
  return out;
}

const MACS = new WeakMap();

async function manifestFor(bytes, mtimeSec = 1757779200) {
  const chunks = [];
  for (let at = 0; at < bytes.length; at += CHUNK) {
    chunks.push(
      bytesToHex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytes.slice(at, at + CHUNK)))),
    );
  }
  const manifest = {
    offer: OFFER,
    mode: "single",
    label: "a.bin",
    kind: "file",
    chunkSize: String(CHUNK),
    createdAt: "2026-09-16T00:00:00Z",
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
  }

  send(data) {
    this.sent.push(data);
  }

  close() {
    this.closed = true;
    this.onclose?.();
  }

  emit(data) {
    this.onmessage?.({ data });
  }
}

/** The send half of a DataChannel, recording what the pipeline wrote. */
function fakeSink() {
  return {
    frames: [],
    fragmentBytes: FRAGMENT,
    highWater: 4 * 1024 * 1024,
    bufferedAmount: 0,
    closed: false,
    send(bytes) {
      this.frames.push(bytes);
    },
    async waitLow() {},
    close() {
      this.closed = true;
    },
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

/** The source side: a published offer and a sender wired to fake control. */
async function sourceHarness(bytes) {
  const counted = countedFile(bytes);
  const control = [];
  const sockets = [];
  const events = { progress: [], chunks: [], done: [], errors: [], paths: [], entryDone: [] };
  const manifest = await manifestFor(bytes);
  const manager = {
    records: new Map([
      [
        OFFER,
        {
          status: "live",
          manifest,
          macHex: macOf(manifest),
          files: new Map([["a.bin", { file: counted.file }]]),
        },
      ],
    ]),
    offers() {
      return this.records;
    },
    withdrawOffer() {
      return {};
    },
  };
  const sender = createSender({
    offers: manager,
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
    events: {
      onProgress: (info) => events.progress.push(info),
      onChunk: (transferId, index) => events.chunks.push(index),
      onEntryDone: (transferId) => events.entryDone.push(transferId),
      onDone: (transferId, info) => events.done.push([transferId, info]),
      onError: (transferId, code) => events.errors.push([transferId, code]),
      onPath: (transferId, path) => events.paths.push(path),
    },
  });
  sender.handleControl({
    type: "transfer.incoming",
    body: {
      transferId: TRANSFER_ID,
      offerId: OFFER,
      fromPeerId: SINK_PEER,
      attemptId: ATTEMPT_A,
    },
  });
  await tick(20);
  return { sender, control, sockets, events, counted, manifest };
}

/** The recipient side: a receiver over the in-memory OPFS/IDB doubles. */
async function sinkHarness(bytes, idle = {}) {
  const control = [];
  const sockets = [];
  const events = {
    progress: [],
    complete: [],
    staged: [],
    errors: [],
    paths: [],
    attemptFailed: [],
  };
  const backends = fakeBackends();
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
    getSelfPeerId: () => SINK_PEER,
    repository: createRepository(backends),
    ...idle,
    events: {
      onProgress: (info) => events.progress.push(info),
      onComplete: (info) => events.complete.push(info),
      onStaged: (info) => events.staged.push(info),
      onError: (transferId, code) => events.errors.push([transferId, code]),
      onPath: (transferId, path) => events.paths.push(path),
      onAttemptFailed: (transferId, attemptId, reason) =>
        events.attemptFailed.push([transferId, attemptId, reason]),
    },
  });
  const manifest = await manifestFor(bytes);
  const started = await receiver.startDownload({
    offerId: OFFER,
    manifest,
    macHex: macOf(manifest),
    sourcePeerId: SOURCE_PEER,
  });
  assert.deepEqual(started, { pending: true });
  const request = control.find((m) => m.type === "transfer.request");
  assert.ok(request, "the click produced exactly one request");
  assert.equal(
    receiver.handleControl({
      type: "ack",
      requestId: request.requestId,
      body: { result: { transferId: TRANSFER_ID } },
    }),
    true,
  );
  return { receiver, control, sockets, events, backends, manifest };
}

async function keyFor(attemptId) {
  return attemptKey(hexToBytes(ROOM_KEY), hexToBytes(TRANSFER_ID), hexToBytes(attemptId));
}

/** Feeds one whole manifest chunk as DATA frames, returning the next seq. */
async function feedChunk(emit, key, seq, bytes, index) {
  const offset = index * CHUNK;
  const end = Math.min(offset + CHUNK, bytes.length);
  for (let at = offset; at < end; at += FRAGMENT) {
    await emit(await sealFrame(key, seq, 1, bytes.subarray(at, Math.min(at + FRAGMENT, end))));
    seq += 1;
  }
  return seq;
}

function finalPayload(total) {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(total), false);
  return out;
}

describe("web-transfer attempt coordination", () => {
  it("path_commit_is_required_and_attempt_bound", async () => {
    // The commit is the server's statement that THIS attempt may carry
    // bytes. Without the attempt check a commit belonging to an abandoned
    // attempt would start the pipeline over a transport that is already
    // gone, and without the commit at all the source would write into a
    // channel the server never admitted.
    const bytes = payload(CHUNK);
    const src = await sourceHarness(bytes);
    const sink = fakeSink();
    assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_B, sink), false);
    assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink), true);
    // A commit for another attempt is not this transfer's business.
    assert.equal(
      src.sender.handleControl({
        type: "transfer.path_commit",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, path: "direct" },
      }),
      false,
    );
    await tick(30);
    assert.equal(sink.frames.length, 0, "an unbound commit started nothing");
    assert.equal(src.counted.calls.length, 0, "not one byte of the file was read");
    assert.equal(
      src.sender.handleControl({
        type: "transfer.path_commit",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
      }),
      true,
    );
    await waitFor(() => sink.frames.length > 0, "the committed attempt to send");

    // The recipient half: a direct commit naming another attempt is refused
    // outright, so nothing moves the row out of `direct`.
    const dst = await sinkHarness(bytes);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    assert.equal(
      dst.receiver.handleControl({
        type: "transfer.path_commit",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, path: "direct" },
      }),
      false,
    );
    assert.equal(dst.receiver.transfers().get(TRANSFER_ID).state, "direct");
    assert.equal(
      dst.receiver.handleControl({
        type: "transfer.path_commit",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
      }),
      true,
    );
    assert.equal(dst.receiver.transfers().get(TRANSFER_ID).state, "receiving");
  });

  it("a_channel_closing_after_final_completes_instead_of_failing", async () => {
    // The counterpart closes the DataChannel in the same turn it writes
    // FINAL — that is the ORDINARY end of a successful direct transfer, not
    // a failure. The frames it already handed us are still in the inbox and
    // still decrypting when the close event fires, so a decision taken on
    // the spot is taken on a state that has not happened yet.
    //
    // Answering immediately cleared `inbox`, which threw away the FINAL of a
    // transfer whose every byte was already verified on disk. Nothing failed
    // and nothing completed: the row sat at `100% · transferring` for ever
    // with the badge walked back to `connecting`, and only a reload and a
    // resume could finish it. The relay leg has drained before deciding
    // since it was written (`socket.onclose`); this is that rule on the
    // direct leg.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    const emit = async (frame) => {
      assert.equal(
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame),
        true,
      );
    };
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const seq = await feedChunk(emit, key, 0, bytes, 0);
    await emit(await sealFrame(key, seq, FRAME_FINAL, finalPayload(bytes.length)));
    // NO tick here: the close is observed while the pipeline is still busy,
    // which is the whole point. A `tick` would test a race that does not
    // happen.
    const ranges = await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A);
    assert.equal(
      ranges,
      null,
      "a channel that closed after FINAL has nothing to report: the transfer succeeded",
    );
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    assert.equal(live.state, "complete-pending", "FINAL was processed, not discarded");
    const complete = dst.control.find((m) => m.type === "transfer.complete");
    assert.ok(complete, "the recipient asked the server to complete the transfer");
    assert.deepEqual(
      dst.events.paths,
      ["direct"],
      "the transport that carried the bytes was named once and never withdrawn",
    );
    // And the completion really lands: the ack stages the verified file.
    assert.equal(
      dst.receiver.handleControl({
        type: "ack",
        requestId: complete.requestId,
        body: { result: { transferId: TRANSFER_ID } },
      }),
      true,
    );
    await waitFor(() => dst.events.staged.length === 1, "the staged file");
    assert.deepEqual(dst.events.errors, [], "nothing failed");
  });

  it("a_channel_that_dies_mid_transfer_is_reported_without_waiting", async () => {
    // The other half of the contract, and the one that pays for the first:
    // a transport that dies MID-transfer must be reported AT ONCE. Those
    // ranges are what the server puts in the relay attempt's commit, and
    // BOTH ends notice the same dead channel — so a recipient that pauses to
    // hash a chunk loses the race to the source, whose notice carries no
    // ranges at all, and the replacement attempt re-sends bytes that were
    // already on disk. Making the wait unconditional did exactly that, and
    // `T-WEB-DIRECT-FALLBACK` read the empty commit on two engines.
    //
    // Determinism, not luck: every frame is sealed FIRST, then handed over
    // in one turn, so the pipeline has had no turn of its own when the close
    // is observed. `finalIsQueued` is false, the decision is taken in that
    // same turn, and the answer is what is on disk — nothing yet.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const frames = [];
    let seq = 0;
    for (let at = 0; at < bytes.length; at += FRAGMENT) {
      frames.push(
        await sealFrame(key, seq, 1, bytes.subarray(at, Math.min(at + FRAGMENT, bytes.length))),
      );
      seq += 1;
    }
    for (const frame of frames) {
      assert.equal(
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame),
        true,
      );
    }
    const ranges = await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A);
    assert.deepEqual(
      ranges,
      [],
      "the report is what is on disk at the close, not what the pipeline might still produce",
    );
    assert.equal(
      dst.receiver.transfers().get(TRANSFER_ID).attemptClosed,
      true,
      "the attempt is closed in the same turn the close was observed",
    );
  });

  it("frames_arriving_out_of_order_across_carriers_are_put_back_in_order", async () => {
    // With several carriers the arrival order is the order N independent
    // associations happened to deliver in, and the receiver reorders on the
    // sequence in each frame's own CLEARTEXT header. The chunk still has to
    // verify against the manifest digest, which is what proves the bytes were
    // reassembled in the right order and not merely accepted.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 4), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const frames = [];
    await feedChunk(async (frame) => frames.push(frame), key, 0, bytes, 0);
    assert.ok(frames.length > 8, "a 1 MiB chunk is many fragments");
    // A deterministic shuffle standing for four carriers draining at
    // different rates: every fourth frame first, then the rest. No frame is
    // lost and none is duplicated — only the order changes.
    const order = [];
    for (let lane = 0; lane < 4; lane += 1) {
      for (let at = lane; at < frames.length; at += 4) {
        order.push(at);
      }
    }
    for (const at of order) {
      assert.equal(
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frames[at]),
        true,
      );
    }
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "the shuffled chunk to verify",
    );
    assert.deepEqual(dst.events.errors, [], "reordering is not a failure");
    assert.deepEqual(dst.events.attemptFailed, []);
  });

  it("a_single_carrier_attempt_keeps_the_strict_in_order_rule", async () => {
    // The red-check of the gate above: one transport delivers in order, so a
    // gap there is not skew — it is a stream that no longer means what the
    // attempt assumes, and it must still fail exactly as it did before the
    // reorder window existed. A window armed unconditionally would make this
    // transfer WAIT for a frame that is never coming.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 1), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    dst.receiver.deliverDirectFrame(
      TRANSFER_ID,
      ATTEMPT_A,
      await sealFrame(key, 0, 1, bytes.subarray(0, FRAGMENT)),
    );
    dst.receiver.deliverDirectFrame(
      TRANSFER_ID,
      ATTEMPT_A,
      await sealFrame(key, 2, 1, bytes.subarray(FRAGMENT, 2 * FRAGMENT)),
    );
    await waitFor(() => dst.events.errors.length === 1, "the gap to fail");
    assert.equal(dst.events.errors[0][1], "FAILED");
    assert.deepEqual(
      dst.events.attemptFailed,
      [],
      "a single carrier never reaches the window",
    );
  });

  it("the_reorder_window_is_bounded_and_ends_the_attempt_not_the_transfer", async () => {
    // A carrier that has STOPPED, rather than merely fallen behind, would
    // otherwise hold the window open for ever while the others fill it. The
    // bound turns that into the end of the ATTEMPT: the transfer continues on
    // the relay from what is already verified on disk, so the user sees a
    // slower transfer and not a failed one. Failing the TRANSFER here would
    // throw away every verified chunk for a transport problem.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 4), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    // Sequence 0 never arrives. Small frames so the FRAME ceiling is the one
    // that binds and the test stays quick.
    const ceiling = reorderBudget(4).frames;
    for (let seq = 1; seq <= ceiling + 1; seq += 1) {
      dst.receiver.deliverDirectFrame(
        TRANSFER_ID,
        ATTEMPT_A,
        await sealFrame(key, seq, 1, bytes.subarray(0, 64)),
      );
      if (dst.events.attemptFailed.length > 0) {
        break;
      }
    }
    await waitFor(
      () => dst.events.attemptFailed.length === 1,
      "the window to bound the attempt",
    );
    assert.equal(dst.events.attemptFailed[0][0], TRANSFER_ID);
    assert.equal(dst.events.attemptFailed[0][1], ATTEMPT_A);
    assert.equal(dst.events.attemptFailed[0][2], "stalled");
    assert.deepEqual(dst.events.errors, [], "the TRANSFER is untouched");
    assert.ok(
      dst.receiver.transfers().has(TRANSFER_ID),
      "the transfer is still open for the relay attempt",
    );
  });

  it("the_reorder_budget_grows_with_the_carrier_count_and_stops_at_the_cap", () => {
    // B-A040: the window fills at the rate of the OTHER carriers, so a fixed
    // ceiling means something different at every carrier count. One carrier
    // keeps exactly the budget that shipped before this existed.
    assert.deepEqual(reorderBudget(1), {
      frames: 512,
      bytes: 8 * 1024 * 1024,
    });
    assert.deepEqual(reorderBudget(4), {
      frames: 2048,
      bytes: 32 * 1024 * 1024,
    });
    // At 8 the frame budget reaches its absolute cap and stops there; the
    // byte budget reaches its own at the same count.
    assert.deepEqual(reorderBudget(8), {
      frames: 4096,
      bytes: 64 * 1024 * 1024,
    });
    // A peer that announces nonsense cannot enlarge the tab's heap.
    assert.deepEqual(reorderBudget(1024), reorderBudget(8));
    assert.deepEqual(reorderBudget(0), reorderBudget(1));
    assert.deepEqual(reorderBudget("x"), reorderBudget(1));
  });

  it("a_gap_that_fills_past_the_old_fixed_ceiling_keeps_the_attempt", async () => {
    // THE regression B-A040 fixed, in the shape it was measured in: at eight
    // carriers the window reached 8 407 808 bytes of ordinary skew — one
    // paused carrier while the other seven kept delivering — and the old
    // fixed 8 MiB ceiling abandoned a direct path that was working. Here the
    // gap DOES fill, so nothing has stopped and the attempt must survive.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 8), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    // Frames of 24 KiB, the size the direct path actually sends, so the held
    // bytes are the held bytes of the field report and not a scaled model.
    const fragment = bytes.subarray(0, 24 * 1024);
    const held = 400;
    for (let seq = 1; seq <= held; seq += 1) {
      dst.receiver.deliverDirectFrame(
        TRANSFER_ID,
        ATTEMPT_A,
        await sealFrame(key, seq, 1, fragment),
      );
    }
    assert.deepEqual(
      dst.events.attemptFailed,
      [],
      `${held} frames of 24 KiB is ${held * 24} KiB of skew — more than the ` +
        "old fixed ceiling — and at eight carriers it is within budget",
    );
    assert.ok(
      held * 24 * 1024 > 8 * 1024 * 1024,
      "the test must actually cross the ceiling it is about",
    );
  });

  it("a_gap_that_never_fills_ends_the_attempt_on_the_deadline", async () => {
    // The other half: the budget is the memory bound, the DEADLINE is the
    // verdict. A carrier that stopped is caught by time, so it is caught on a
    // slow path too — where the byte ceiling alone could take minutes.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes, { reorderStallMs: 60 });
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 8), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    // Sequence 0 never arrives, and the window stays far below its budget —
    // so the ONLY thing that can end this attempt is the deadline.
    dst.receiver.deliverDirectFrame(
      TRANSFER_ID,
      ATTEMPT_A,
      await sealFrame(key, 1, 1, bytes.subarray(0, 64)),
    );
    assert.deepEqual(dst.events.attemptFailed, [], "not yet: the gap is young");
    await tick(90);
    dst.receiver.deliverDirectFrame(
      TRANSFER_ID,
      ATTEMPT_A,
      await sealFrame(key, 2, 1, bytes.subarray(0, 64)),
    );
    await waitFor(
      () => dst.events.attemptFailed.length === 1,
      "the deadline to end the attempt",
    );
    assert.equal(dst.events.attemptFailed[0][2], "stalled");
    assert.ok(
      dst.receiver.transfers().has(TRANSFER_ID),
      "the transfer is still open for the relay attempt",
    );
  });

  it("a_duplicate_inside_the_reorder_window_ends_the_attempt", async () => {
    // A reliable transport cannot deliver the same frame twice, so a
    // duplicate is not skew either. It is caught before the AEAD, because the
    // window would otherwise hold it as if it were a different frame.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A, 2), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const ahead = await sealFrame(key, 3, 1, bytes.subarray(0, 64));
    dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, ahead);
    dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, ahead);
    await waitFor(() => dst.events.attemptFailed.length === 1, "the duplicate");
    assert.equal(dst.events.attemptFailed[0][2], "protocol");
    assert.deepEqual(dst.events.errors, []);
  });

  it("old_attempt_frames_callbacks_and_keys_are_ignored", async () => {
    // A DataChannel that dies mid-transfer can still deliver what the
    // browser had already queued. Those frames belong to an attempt that no
    // longer exists: taken at face value they would be decrypted with the
    // wrong key or, worse, counted against the new attempt's sequence.
    const bytes = payload(2 * CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    const keyB = await keyFor(ATTEMPT_B);
    assert.notEqual(bytesToHex(keyA), bytesToHex(keyB), "each attempt has its own key");
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    let seq = await feedChunk(
      async (frame) => {
        assert.equal(dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame), true);
      },
      keyA,
      0,
      bytes,
      0,
    );
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "the first chunk to verify",
    );
    // The direct attempt ends; the server mints a relay attempt.
    assert.deepEqual(await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A), [[0, 1]]);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    assert.equal(live.attemptId, ATTEMPT_B);
    assert.equal(live.expectedSeq, 0, "the new attempt's sequence restarts at zero");
    // Late frames of the dead attempt: refused by attempt id, before the
    // key is ever consulted.
    assert.equal(
      dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, await sealFrame(keyA, seq, 1, bytes.subarray(0, 64))),
      false,
    );
    assert.deepEqual(live.verifiedRanges, [[0, 1]], "nothing late was written");
    assert.deepEqual(dst.events.errors, [], "a late frame is not an error");
  });

  it("a_dead_attempt_is_reported_exactly_once_whichever_end_notices_first", async () => {
    // BOTH ends see the same dead channel: the recipient's own close event
    // and the source's forwarded `transfer.direct_failed`, in whichever
    // order two engines' timers produce. The recipient answers the ranges to
    // the FIRST of them and nothing to the second, so the server gets one
    // report per attempt no matter who won — and the ranges are never lost
    // by staying silent, which is what made a fallback re-send the whole
    // file whenever the source won that race.
    const bytes = payload(2 * CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    await feedChunk(
      async (frame) => {
        assert.equal(dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame), true);
      },
      keyA,
      0,
      bytes,
      0,
    );
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "the first chunk to verify",
    );
    assert.deepEqual(
      await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A),
      [[0, 1]],
      "whoever notices first is told what is on disk",
    );
    assert.equal(
      await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A),
      null,
      "the other end's notice for the SAME attempt reports nothing",
    );
    // A genuinely new attempt is not a duplicate: adopting one re-arms the
    // report, or a second failure could never be told.
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    assert.deepEqual(
      await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_B),
      [[0, 1]],
      "the replacement attempt can be reported in its turn",
    );
  });

  it("initial_direct_failure_opens_one_relay_without_new_request", async () => {
    // The user clicked once. A direct attempt that never carries a byte
    // must not cost a second click, and must not produce a second
    // `transfer.request` — the server already holds the admission for this
    // transfer and would answer a duplicate with a fresh TransferId.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    await tick(20);
    assert.deepEqual(await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A), []);
    assert.equal(dst.sockets.length, 0, "a dead direct attempt opened no relay by itself");
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    assert.equal(dst.sockets.length, 1, "the server's ticket opened exactly one relay leg");
    assert.equal(
      dst.control.filter((m) => m.type === "transfer.request").length,
      1,
      "the fallback asked for nothing new",
    );

    // Same on the source: abandoning the channel keeps the transfer, and
    // the relay ticket for the new attempt resumes it.
    const src = await sourceHarness(bytes);
    const sink = fakeSink();
    assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink), true);
    assert.equal(src.sender.detachDirect(TRANSFER_ID, ATTEMPT_A), true);
    assert.ok(src.sender.transfers().has(TRANSFER_ID), "the transfer survived its attempt");
    assert.equal(
      src.sender.handleControl({
        type: "transfer.relay_ticket",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
      }),
      true,
    );
    await tick(20);
    assert.equal(src.sockets.length, 1);
    assert.equal(src.sender.transfers().get(TRANSFER_ID).attemptId, ATTEMPT_B);
    assert.equal(
      src.control.filter((m) => m.type === "transfer.source_ready").length,
      1,
      "the source announced itself once for the whole transfer",
    );
  });

  it("mid_direct_failure_commits_only_complete_chunk_and_forwards_ranges", async () => {
    // Half a chunk proves nothing: only a chunk that matched its manifest
    // digest is on disk. Reporting a byte range instead of a chunk range
    // would make the relay attempt skip bytes nobody ever verified.
    const bytes = payload(2 * CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const deliver = async (frame) => {
      dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
    };
    let seq = await feedChunk(deliver, keyA, 0, bytes, 0);
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "chunk 0 to verify",
    );
    // Two fragments of chunk 1 — a real mid-chunk death.
    await deliver(await sealFrame(keyA, seq, 1, bytes.subarray(CHUNK, CHUNK + FRAGMENT)));
    await deliver(await sealFrame(keyA, seq + 1, 1, bytes.subarray(CHUNK + FRAGMENT, CHUNK + 2 * FRAGMENT)));
    await tick(20);
    assert.deepEqual(
      await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A),
      [[0, 1]],
      "only the complete chunk is forwarded",
    );
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    assert.deepEqual(live.chunkBuffers, [], "the partial chunk's fragments are dropped");
    assert.deepEqual(live.pendingFrames, []);
    assert.deepEqual(live.inbox, []);
  });

  it("relay_attempt_uses_fresh_key_nonce_and_sequence", async () => {
    // The fallback is a NEW attempt: new key, nonce sequence from zero, and
    // a FINAL that counts only what this attempt carried. Reusing either
    // would repeat a (key, nonce) pair across two transports.
    const bytes = payload(2 * CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    const keyB = await keyFor(ATTEMPT_B);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    await feedChunk(
      async (frame) => {
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
      },
      keyA,
      0,
      bytes,
      0,
    );
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "chunk 0 to verify",
    );
    await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    const socket = dst.sockets[0];
    socket.onopen();
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      // The ranges the server forwarded to the source ride the commit, and
      // both ends plan from exactly them.
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, path: "relay", resumeRanges: [[0, 1]] },
    });
    // The relay attempt sends ONLY chunk 1, sequence from zero.
    let seq = await feedChunk(async (frame) => socket.emit(frame), keyB, 0, bytes, 1);
    socket.emit(await sealFrame(keyB, seq, 2, finalPayload(CHUNK)));
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID)?.state === "complete-pending",
      "the resumed transfer to complete",
    );
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    assert.deepEqual(live.verifiedRanges, [[0, 2]], "both chunks are on disk once");
    assert.equal(live.attemptId, ATTEMPT_B);
  });

  it("the_path_commit_is_the_only_plan_authority", async () => {
    // The recipient holds chunk 0, but the commit carries NO ranges — the
    // shape a lost race produces: the source noticed the dead channel on its
    // own write and reported first, and a source is never believed about
    // ranges. The source is therefore sending from chunk 0, so the recipient
    // must plan from chunk 0 too. Planning from its own disk instead put the
    // first relayed chunk at the wrong position and read it as a digest
    // mismatch — a transfer that failed with every byte already available.
    const bytes = payload(2 * CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    const keyB = await keyFor(ATTEMPT_B);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    await feedChunk(
      async (frame) => {
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
      },
      keyA,
      0,
      bytes,
      0,
    );
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length === 1,
      "chunk 0 to verify",
    );
    await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    const socket = dst.sockets[0];
    socket.onopen();
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, path: "relay" },
    });
    let seq = await feedChunk(async (frame) => socket.emit(frame), keyB, 0, bytes, 0);
    seq = await feedChunk(async (frame) => socket.emit(frame), keyB, seq, bytes, 1);
    socket.emit(await sealFrame(keyB, seq, 2, finalPayload(2 * CHUNK)));
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID)?.state === "complete-pending",
      "the re-sent transfer to complete",
    );
    assert.deepEqual(dst.events.errors, [], "nothing read as a digest mismatch");
    assert.deepEqual(dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges, [[0, 2]]);
  });

  it("source_rehashes_skipped_ranges", async () => {
    // Skipped chunks are still READ and hashed: the file may have changed
    // since the first attempt, and a resume that trusted the recipient's
    // ranges would splice two different files together silently.
    const bytes = payload(2 * CHUNK);
    const src = await sourceHarness(bytes);
    const sink = fakeSink();
    assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink), true);
    src.sender.handleControl({
      type: "transfer.path_commit",
      body: {
        transferId: TRANSFER_ID,
        attemptId: ATTEMPT_A,
        path: "direct",
        resumeRanges: [[0, 1]],
      },
    });
    await waitFor(
      () => src.sender.transfers().get(TRANSFER_ID)?.state === "done-pending",
      "the resumed send to finish",
    );
    assert.deepEqual(src.events.chunks, [0, 1], "every chunk was rehashed, skipped one included");
    assert.ok(
      src.counted.calls.some(([start]) => start === 0),
      "the skipped chunk was read to be rehashed",
    );
    const fragments = Math.ceil(CHUNK / FRAGMENT);
    assert.equal(sink.frames.length, fragments + 1, "only the missing chunk travelled, plus FINAL");
    const key = await keyFor(ATTEMPT_A);
    const final = await openFrame(key, new Uint8Array(sink.frames.at(-1)), fragments);
    assert.equal(final.ftype, 2);
    assert.equal(
      new DataView(final.plaintext.buffer, final.plaintext.byteOffset, 8).getBigUint64(0, false),
      BigInt(CHUNK),
      "FINAL counts this attempt's bytes, not the whole entry",
    );
    const last = src.events.progress.at(-1);
    assert.equal(last.sentBytes, 2 * CHUNK, "progress accounts the skipped chunk as sent");
  });

  it("ui_keeps_one_transfer_and_monotonic_progress", async () => {
    // One click, one row. A fallback restarts the wire counter at zero, and
    // a row that followed it would read as lost progress while the verified
    // chunks are still on disk.
    let state = createInitialState();
    state = reduce(state, {
      kind: "transfer.started",
      transferId: TRANSFER_ID,
      offerId: OFFER,
      direction: "in",
      totalBytes: 2 * CHUNK,
      label: "a.bin",
    });
    assert.equal(state.transfers.get(TRANSFER_ID).path, PATH.CONNECTING);
    state = reduce(state, { kind: "transfer.progress", transferId: TRANSFER_ID, doneBytes: CHUNK });
    state = reduce(state, { kind: "transfer.path", transferId: TRANSFER_ID, path: PATH.DIRECT });
    assert.equal(state.transfers.get(TRANSFER_ID).path, PATH.DIRECT);
    // The fallback: a new attempt's first report is smaller than the last.
    state = reduce(state, { kind: "transfer.progress", transferId: TRANSFER_ID, doneBytes: 4096 });
    assert.equal(state.transfers.get(TRANSFER_ID).doneBytes, CHUNK, "progress never walks back");
    state = reduce(state, { kind: "transfer.path", transferId: TRANSFER_ID, path: PATH.RELAY });
    state = reduce(state, {
      kind: "transfer.progress",
      transferId: TRANSFER_ID,
      doneBytes: 2 * CHUNK,
    });
    const row = state.transfers.get(TRANSFER_ID);
    assert.equal(state.transfers.size, 1, "the fallback did not add a second row");
    assert.equal(row.doneBytes, 2 * CHUNK);
    assert.equal(row.path, PATH.RELAY);
    assert.equal(row.state, TRANSFER.TRANSFERRING);
    // Nothing walks a row back to `connecting` once a path is a fact.
    state = reduce(state, {
      kind: "transfer.path",
      transferId: TRANSFER_ID,
      path: PATH.CONNECTING,
    });
    assert.equal(state.transfers.get(TRANSFER_ID).path, PATH.RELAY);
  });

  it("first_verified_chunk_is_only_path_authority", async () => {
    // A committed path is an intention; a verified chunk is evidence. The
    // badge is the user's whole read on whether their bytes went peer to
    // peer, so it may not be set by a commit, and on the source it may not
    // be set by anything but the server's own forwarded report.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    assert.deepEqual(dst.events.paths, [], "the commit alone declared no path");
    await feedChunk(
      async (frame) => {
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
      },
      keyA,
      0,
      bytes,
      0,
    );
    await waitFor(() => dst.events.paths.length === 1, "the first verified chunk");
    assert.deepEqual(dst.events.paths, ["direct"]);
    assert.ok(
      dst.control.some(
        (m) => m.type === "transfer.progress" && m.body.attemptId === ATTEMPT_A && m.body.receivedBytes === String(CHUNK),
      ),
      "the verified bytes were reported to the server",
    );

    // The source learns the path ONLY from the server's forwarded report.
    const src = await sourceHarness(bytes);
    const sink = fakeSink();
    src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink);
    src.sender.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    assert.deepEqual(src.events.paths, [], "attaching a channel is not evidence");
    // A report for another attempt is not this attempt's evidence.
    assert.equal(
      src.sender.handleControl({
        type: "transfer.progress",
        body: {
          transferId: TRANSFER_ID,
          attemptId: ATTEMPT_B,
          path: "direct",
          receivedBytes: String(CHUNK),
        },
      }),
      false,
    );
    assert.deepEqual(src.events.paths, []);
    assert.equal(
      src.sender.handleControl({
        type: "transfer.progress",
        body: {
          transferId: TRANSFER_ID,
          attemptId: ATTEMPT_A,
          path: "direct",
          receivedBytes: String(CHUNK),
        },
      }),
      true,
    );
    assert.deepEqual(src.events.paths, ["direct"]);
  });

  it("completion_cancel_failure_race_has_one_winner", async () => {
    // A channel closing right after the last byte is the END, not a
    // failure: reporting it would allocate a relay attempt for a transfer
    // the server has already completed.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    const keyA = await keyFor(ATTEMPT_A);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    const deliver = async (frame) => {
      dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
    };
    const seq = await feedChunk(deliver, keyA, 0, bytes, 0);
    await deliver(await sealFrame(keyA, seq, 2, finalPayload(CHUNK)));
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID)?.state === "complete-pending",
      "the direct transfer to complete",
    );
    assert.equal(
      await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A),
      null,
      "a completed transfer reports no failure",
    );

    // Same shape on the source, where the FINAL has left but the server's
    // `transfer.completed` has not arrived yet.
    const src = await sourceHarness(bytes);
    const sink = fakeSink();
    src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink);
    src.sender.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await waitFor(
      () => src.sender.transfers().get(TRANSFER_ID)?.state === "done-pending",
      "the source to finish sending",
    );
    assert.equal(
      src.sender.detachDirect(TRANSFER_ID, ATTEMPT_A),
      false,
      "the channel closing after FINAL is the end, not a failure",
    );
    assert.ok(src.sender.transfers().has(TRANSFER_ID));
    src.sender.handleControl({
      type: "transfer.completed",
      body: { transferId: TRANSFER_ID },
    });
    assert.equal(src.events.done.length, 1, "exactly one terminal");
    assert.equal(src.sender.transfers().has(TRANSFER_ID), false);

    // Cancel is the other winner: both ends drop the attempt at once.
    const other = await sourceHarness(bytes);
    const otherSink = fakeSink();
    other.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, otherSink);
    assert.equal(other.sender.abortTransfer(TRANSFER_ID), true);
    assert.equal(other.sender.detachDirect(TRANSFER_ID, ATTEMPT_A), false);
    assert.equal(other.sender.transfers().has(TRANSFER_ID), false);
  });

  it("failed_relay_never_auto_loops", async () => {
    // One automatic fallback per request. A relay attempt that dies too is
    // a real failure the user must see: retrying it automatically would
    // spin the admission semaphore against a network that is down.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes);
    dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A);
    await tick(20);
    await dst.receiver.directFailed(TRANSFER_ID, ATTEMPT_A);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, ticket: "ab".repeat(16) },
    });
    await tick(20);
    assert.equal(dst.sockets.length, 1);
    dst.sockets[0].onopen();
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_B, path: "relay" },
    });
    // The relay leg dies before FINAL.
    dst.sockets[0].close();
    await waitFor(() => dst.events.errors.length === 1, "the relay failure to surface");
    assert.equal(dst.sockets.length, 1, "no attempt opened itself a second leg");
    assert.equal(
      dst.control.filter((m) => m.type === "transfer.request").length,
      1,
      "no automatic re-request",
    );
    assert.equal(dst.receiver.transfers().has(TRANSFER_ID), false);
    // A ticket for a third attempt lands on nothing.
    assert.equal(
      dst.receiver.handleControl({
        type: "transfer.relay_ticket",
        body: { transferId: TRANSFER_ID, attemptId: "33".repeat(16), ticket: "ab".repeat(16) },
      }),
      false,
    );
    assert.equal(dst.sockets.length, 1);
  });
});

describe("web-transfer direct idle deadline", () => {
  const IDLE = { directIdleTimeoutMs: 120, directIdleCheckMs: 10 };

  it("a_silent_direct_attempt_is_abandoned_so_the_relay_can_finish_it", async () => {
    // A carrier that CLOSES reports itself. A carrier that goes silent does
    // not: the channel stays `open` and every frame the source writes
    // disappears. Nothing else in the path has a deadline — the server is
    // not on the direct path at all — so without this the row sits at its
    // last verified byte for ever.
    const bytes = payload(CHUNK * 2);
    const dst = await sinkHarness(bytes, IDLE);
    const key = await keyFor(ATTEMPT_A);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    await tick(20);
    // One chunk arrives and is verified; then the path goes quiet.
    await feedChunk(
      async (frame) => {
        dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_A, frame);
      },
      key,
      0,
      bytes,
      0,
    );
    await waitFor(
      () => dst.events.attemptFailed.length > 0,
      "the silent attempt to be abandoned",
    );
    assert.deepEqual(dst.events.attemptFailed[0], [
      TRANSFER_ID,
      ATTEMPT_A,
      "stalled",
    ]);
    // The ATTEMPT died, not the transfer: what was verified is still on disk
    // and is what the relay attempt will skip.
    assert.ok(dst.receiver.transfers().has(TRANSFER_ID));
  });

  it("a_recipient_that_is_merely_busy_is_never_mistaken_for_a_dead_path", async () => {
    // The other half, and the one that makes the deadline safe: a recipient
    // that is BEHIND makes the source's queue fill, so frames stop arriving
    // for a reason that has nothing to do with the path. A clock that did
    // not look at the inbox would abandon a healthy attempt under exactly
    // the load carriers exist to serve.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes, IDLE);
    assert.equal(dst.receiver.beginDirect(TRANSFER_ID, ATTEMPT_A), true);
    dst.receiver.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    });
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    live.pumping = true;
    live.lastFrameAt = Date.now() - 10_000;
    await tick(400);
    assert.equal(
      dst.events.attemptFailed.length,
      0,
      "work still in hand is not an idle path",
    );
    live.pumping = false;
    await waitFor(
      () => dst.events.attemptFailed.length > 0,
      "the deadline to resume once the recipient has nothing left to do",
    );
  });

  it("the_relay_leg_never_arms_the_direct_idle_clock", async () => {
    // The relay's stall is the server's to notice and its socket reports a
    // close; arming a second deadline on it would give a slow relay a way to
    // fail that it never had.
    const bytes = payload(CHUNK);
    const dst = await sinkHarness(bytes, IDLE);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: {
        transferId: TRANSFER_ID,
        attemptId: ATTEMPT_A,
        ticket: "t".repeat(32),
        role: "recipient",
      },
    });
    await tick(400);
    assert.equal(dst.events.attemptFailed.length, 0);
    assert.equal(
      dst.receiver.transfers().get(TRANSFER_ID).idleTimer,
      null,
      "no clock was armed for a relay attempt",
    );
  });
});

describe("web-transfer relay to direct upgrade (7.5)", () => {
  /** A download running on the relay, with chunk 0 already verified. */
  async function relaying(bytes) {
    const dst = await sinkHarness(bytes);
    dst.receiver.handleControl({
      type: "transfer.relay_ticket",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, ticket: "ab".repeat(16) },
    });
    await tick(20);
    assert.equal(dst.sockets.length, 1, "the ticket opened exactly one relay leg");
    dst.sockets[0].onopen?.();
    assert.equal(
      dst.receiver.handleControl({
        type: "transfer.path_commit",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "relay" },
      }),
      true,
    );
    const keyA = await keyFor(ATTEMPT_A);
    const emit = (frame) => dst.sockets[0].emit(frame);
    const seq = await feedChunk(emit, keyA, 0, bytes, 0);
    await waitFor(
      () => dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges.length > 0,
      "the relay to deliver chunk 0",
    );
    // ...AND to have named itself, which is a LATER fact (B-A038).
    // `transfer.verifiedRanges` moves BEFORE the `await
    // repository.saveRecord(...)` of the same chunk and `nameTransport`
    // runs after it, so a precondition that waits only for the range
    // returns INSIDE that window. The upgrade below then moves the attempt
    // id, and the naming still pending fires once — as `direct`. Seen on a
    // CI runner as `paths == ['direct']` on a transfer whose relay chunk was
    // correctly on disk: the harness had accused the product of forgetting
    // to name a transport it was about to name.
    await waitFor(
      () => dst.events.paths.includes("relay"),
      "the relay leg to name its transport",
    );
    return { dst, keyA, seq };
  }

  it("a_probe_never_disturbs_the_relay_that_is_carrying", async () => {
    // The whole point of the re-upgrade is that it costs nothing when it
    // fails: the relay is DELIVERING while the probe negotiates, so a probe
    // that touched the live attempt would turn a working download into a
    // gamble. Nothing about the live attempt may move until the server's
    // own commit says so.
    const bytes = payload(CHUNK * 2);
    const { dst } = await relaying(bytes);
    const live = dst.receiver.transfers().get(TRANSFER_ID);

    assert.equal(dst.receiver.prepareUpgrade(TRANSFER_ID, ATTEMPT_B, 2), true);
    assert.equal(live.attemptId, ATTEMPT_A, "the carrying attempt is untouched");
    assert.equal(live.transport, "relay");
    assert.equal(dst.sockets[0].closed, false, "the relay leg is still open");
    assert.deepEqual(
      dst.receiver.upgradeRanges(TRANSFER_ID, ATTEMPT_B),
      [[0, 1]],
      "the probe reports what the relay already delivered",
    );
    // A frame of the live attempt during the probe: still accepted, because
    // the relay never stopped carrying.
    assert.equal(
      dst.receiver.deliverDirectFrame(
        TRANSFER_ID,
        ATTEMPT_B,
        new Uint8Array(32),
      ),
      false,
      "a frame on the probe is refused while the probe is not committed",
    );

    // Abandoning it is equally free.
    assert.equal(dst.receiver.abandonUpgrade(TRANSFER_ID, ATTEMPT_B), true);
    assert.equal(live.attemptId, ATTEMPT_A);
    assert.equal(live.transport, "relay");
    assert.equal(dst.sockets[0].closed, false);
    assert.deepEqual(dst.events.errors, [], "a probe that came to nothing is not an error");
  });

  it("the_commit_moves_the_recipient_onto_the_probe_and_resumes_where_the_relay_stopped", async () => {
    const bytes = payload(CHUNK * 2);
    const { dst } = await relaying(bytes);
    assert.equal(dst.receiver.prepareUpgrade(TRANSFER_ID, ATTEMPT_B, 2), true);
    const ranges = dst.receiver.upgradeRanges(TRANSFER_ID, ATTEMPT_B);

    assert.equal(
      dst.receiver.handleControl({
        type: "transfer.path_commit",
        body: {
          transferId: TRANSFER_ID,
          attemptId: ATTEMPT_B,
          path: "direct",
          resumeRanges: ranges,
        },
      }),
      true,
    );
    const live = dst.receiver.transfers().get(TRANSFER_ID);
    assert.equal(live.attemptId, ATTEMPT_B, "the download adopted the probe");
    assert.equal(live.transport, "direct");
    assert.equal(live.expectedSeq, 0, "the new attempt's sequence restarts at zero");
    assert.equal(live.reorderWindow, true, "a multi-carrier probe arms the window");
    assert.equal(dst.sockets[0].closed, true, "the relay leg was released");
    assert.deepEqual(live.plan, [1], "the chunk the relay delivered is not sent again");
    assert.equal(live.upgrade, null);

    const keyB = await keyFor(ATTEMPT_B);
    let seq = await feedChunk(
      (frame) => dst.receiver.deliverDirectFrame(TRANSFER_ID, ATTEMPT_B, frame),
      keyB,
      0,
      bytes,
      1,
    );
    dst.receiver.deliverDirectFrame(
      TRANSFER_ID,
      ATTEMPT_B,
      await sealFrame(keyB, seq, FRAME_FINAL, finalPayload(CHUNK)),
    );
    await waitFor(
      () => dst.control.some((m) => m.type === "transfer.complete"),
      "the upgraded download to finish",
    );
    assert.deepEqual(
      dst.receiver.transfers().get(TRANSFER_ID).verifiedRanges,
      [[0, 2]],
      "the relay's chunk and the direct path's chunk are one file",
    );
    assert.deepEqual(dst.events.paths, ["relay", "direct"], "both transports were named in turn");
    const complete = dst.control.find((m) => m.type === "transfer.complete");
    assert.equal(
      dst.receiver.handleControl({
        type: "ack",
        requestId: complete.requestId,
        body: { result: { transferId: TRANSFER_ID } },
      }),
      true,
    );
    await waitFor(() => dst.events.staged.length === 1, "the staged file");
    assert.deepEqual(dst.events.errors, []);
  });

  it("the_source_stages_a_probe_and_switches_onto_it_only_on_the_commit", async () => {
    const bytes = payload(CHUNK * 2);
    const src = await sourceHarness(bytes);
    assert.equal(
      src.sender.handleControl({
        type: "transfer.relay_ticket",
        body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, ticket: "ab".repeat(16) },
      }),
      true,
    );
    await tick(20);
    assert.equal(src.sockets.length, 1);
    src.sender.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "relay" },
    });
    await waitFor(() => src.sockets[0].sent.length > 1, "the relay pipeline to start");

    const sink = fakeSink();
    assert.equal(src.sender.attachUpgrade(TRANSFER_ID, ATTEMPT_B, sink), true);
    await tick(20);
    assert.equal(
      src.sender.transfers().get(TRANSFER_ID).attemptId,
      ATTEMPT_A,
      "staging a probe does not move the attempt that is sending",
    );
    assert.equal(sink.frames.length, 0, "nothing is written into an uncommitted probe");

    assert.equal(
      src.sender.handleControl({
        type: "transfer.path_commit",
        body: {
          transferId: TRANSFER_ID,
          attemptId: ATTEMPT_B,
          path: "direct",
          resumeRanges: [[0, 1]],
        },
      }),
      true,
    );
    const fragments = Math.ceil(CHUNK / FRAGMENT);
    await waitFor(() => sink.frames.length === fragments + 1, "the upgraded source to send");
    const live = src.sender.transfers().get(TRANSFER_ID);
    assert.equal(live.attemptId, ATTEMPT_B);
    assert.equal(live.direct, true);
    assert.equal(live.socket, null, "the relay leg was released");
    // Only the chunk the relay had NOT delivered travelled, plus FINAL: the
    // upgrade resumes, it does not restart.
    const keyB = await keyFor(ATTEMPT_B);
    const first = await openFrame(keyB, new Uint8Array(sink.frames[0]), 0);
    assert.deepEqual(
      Array.from(first.plaintext.subarray(0, 16)),
      Array.from(bytes.subarray(CHUNK, CHUNK + 16)),
      "the first frame of the probe is the chunk the relay never delivered",
    );
    assert.deepEqual(src.events.errors, [], "switching path is not a failure");
  });
});
