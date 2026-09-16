// Unit tests: 4.6 — the catalogue of optimizations the rest of bore MEASURED
// on its own data path, translated to the browser. Each test guards ONE
// catalogue entry, and each entry is either adopted with a measurement in
// `docs/transfer/WEB_TRANSFER_PERF.md` or refused there with a reason.
//
// The two entries that live on the DataChannel itself (the fragment derived
// from the peer, and one channel per transfer) are gated in `webrtc.test.mjs`
// beside the actor's own doubles.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import {
  bytesToHex,
  hexToBytes,
  manifestMac,
  openFrame,
  sealFrame,
} from "../../src/crypto.js";
import { canonicalize, manifestValue } from "../../src/protocol.js";
import { createSender } from "../../src/sender.js";

const ROOM_KEY = "ab".repeat(32);
const ROOM_ID = "00".repeat(16);
const SOURCE_PEER = "11".repeat(16);
const SINK_PEER = "22".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER_ID = "dd".repeat(16);
const ATTEMPT_A = "ee".repeat(16);
const CHUNK = 1024 * 1024;
const FRAGMENT = 24 * 1024;

const VECTORS = JSON.parse(
  readFileSync(
    fileURLToPath(new URL("../../../../tests/fixtures/web_transfer/v1/crypto-vectors.json", import.meta.url)),
    "utf8",
  ),
);

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
  const mac = bytesToHex(
    await manifestMac(
      hexToBytes(ROOM_KEY),
      hexToBytes(ROOM_ID),
      new TextEncoder().encode(canonicalize(manifestValue(manifest))),
    ),
  );
  return { manifest, mac };
}

/**
 * A sink that behaves like a real queue: `send` ADDS to `bufferedAmount` and
 * only a drain removes it. A fake whose buffer never grows cannot show the
 * difference between waiting and queueing, which is the whole point here.
 */
function queueingSink({ highWater = 64 * 1024, drainOnWait = true } = {}) {
  let queued = 0;
  const sink = {
    frames: [],
    fragmentBytes: FRAGMENT,
    highWater,
    maxBuffered: 0,
    waitLowCalls: 0,
    closed: false,
    send(bytes) {
      this.frames.push(bytes);
      queued += bytes.length;
      this.maxBuffered = Math.max(this.maxBuffered, queued);
    },
    async waitLow() {
      this.waitLowCalls += 1;
      if (drainOnWait) {
        queued = 0;
      }
    },
    close() {
      this.closed = true;
    },
  };
  // A real transport makes progress whenever the pipeline yields, and the
  // only moment this one can model that is an observation: the pipeline
  // reads `bufferedAmount` to decide whether to wait, and reads it again
  // after FINAL until the transport owes nothing. A queue that only ever
  // grew would leave that last loop spinning for ever — which is a property
  // of the fake, not of the product.
  Object.defineProperty(sink, "bufferedAmount", {
    get() {
      const seen = queued;
      queued = Math.max(0, queued - FRAGMENT);
      return seen;
    },
    set(value) {
      queued = value;
    },
  });
  return sink;
}

/** The source side: one published offer and a sender wired to fake control. */
async function sourceHarness(bytes) {
  const real = new File([bytes], "a.bin", { lastModified: 1757779200000 });
  const control = [];
  const events = { entryDone: [], errors: [] };
  // Resolved by the sender's own event, so a test can wait for the pipeline
  // WITHOUT arming a timer of its own — and without a microtask poll, which
  // would starve the event loop the pipeline needs.
  let markDone = () => {};
  const entryDone = new Promise((resolve) => {
    markDone = resolve;
  });
  const { manifest, mac } = await manifestFor(bytes);
  const manager = {
    records: new Map([
      [
        OFFER,
        {
          status: "live",
          manifest,
          macHex: mac,
          files: new Map([["a.bin", { file: real }]]),
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
      throw new Error("this harness never takes the relay");
    },
    relayBase: "ws://127.0.0.1:9",
    roomIdHex: ROOM_ID,
    roomKeyHex: ROOM_KEY,
    getSelfPeerId: () => SOURCE_PEER,
    events: {
      onEntryDone: (transferId) => {
        events.entryDone.push(transferId);
        markDone();
      },
      onError: (transferId, code) => events.errors.push([transferId, code]),
    },
  });
  sender.handleControl({
    type: "transfer.incoming",
    body: { transferId: TRANSFER_ID, offerId: OFFER, fromPeerId: SINK_PEER, attemptId: ATTEMPT_A },
  });
  await tick(20);
  return { sender, control, events, entryDone };
}

/** Commits the direct path and lets the pipeline run to the FINAL frame. */
async function runDirect(src, sink) {
  assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink), true);
  assert.equal(
    src.sender.handleControl({
      type: "transfer.path_commit",
      body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
    }),
    true,
  );
  await waitFor(() => src.events.entryDone.length > 0, "the entry to finish", 20_000);
  // And then until the transfer is really quiescent. The pipeline still has
  // one step to take after the last entry — it waits for the transport to
  // write what it was handed — and a test that returned before that left a
  // live sender running under the NEXT test, where its timers are somebody
  // else's evidence.
  await waitFor(
    () => src.sender.transfers().get(TRANSFER_ID)?.state === "done-pending",
    "the transport to drain",
    20_000,
  );
  assert.deepEqual(src.events.errors, []);
}

describe("web-transfer direct-path optimizations (4.6)", () => {
  it("sender_waits_for_drain_instead_of_queueing_past_high_water", async () => {
    // BW-F3 and V-13 in the browser: the queue lives in the tab's own heap,
    // so a pipeline that keeps reading while the transport is behind buys
    // nothing and pays memory and latency for it. The sender must WAIT.
    const bytes = payload(4 * CHUNK);
    const src = await sourceHarness(bytes);
    const sink = queueingSink({ highWater: 64 * 1024 });
    await runDirect(src, sink);

    assert.ok(sink.waitLowCalls >= 3, `the pipeline waited for the queue (${sink.waitLowCalls})`);
    // One chunk is the largest burst between two drain checks, and a
    // fragment is the granularity inside it: anything beyond that is the
    // pipeline queueing past the mark instead of waiting at it.
    assert.ok(
      sink.maxBuffered <= sink.highWater + CHUNK + FRAGMENT,
      `queued at most one chunk past the mark (${sink.maxBuffered})`,
    );
    // And it really did send the whole file — a pipeline that stalled would
    // satisfy the bound above for the wrong reason.
    assert.equal(
      sink.frames.reduce((n, f) => n + f.length, 0) > bytes.length,
      true,
      "every chunk travelled, plus its per-frame overhead",
    );
  });

  it("coalescing_never_waits_on_a_timer", async () => {
    // V-14a's rule, unchanged: batching must never become waiting. A frame
    // that is ready is written in the same turn of the event loop; no timer
    // is ever armed to see whether company arrives.
    const bytes = payload(CHUNK);
    const src = await sourceHarness(bytes);
    // A sink that never backs up, so nothing legitimately waits: any timer
    // armed on this path is one armed to collect company.
    const sink = queueingSink({ highWater: 8 * CHUNK });
    sink.send = function send(frame) {
      this.frames.push(frame);
    };

    const realSetTimeout = globalThis.setTimeout;
    const armed = [];
    globalThis.setTimeout = function patched(fn, ms, ...rest) {
      armed.push(ms ?? 0);
      return realSetTimeout(fn, ms, ...rest);
    };
    try {
      assert.equal(src.sender.attachDirect(TRANSFER_ID, ATTEMPT_A, sink), true);
      assert.equal(
        src.sender.handleControl({
          type: "transfer.path_commit",
          body: { transferId: TRANSFER_ID, attemptId: ATTEMPT_A, path: "direct" },
        }),
        true,
      );
      await src.entryDone;
    } finally {
      globalThis.setTimeout = realSetTimeout;
    }
    assert.deepEqual(armed, [], "the send path armed no timer");
    assert.deepEqual(src.events.errors, []);
  });

  it("frame_encode_allocates_once_and_matches_the_fixture_bytes", async () => {
    // V-14b in the browser. The format does NOT move: the cross-language
    // fixture is the oracle, byte for byte, and the allocation claim is
    // checked on the object the function actually returns — exactly sized,
    // at offset zero, i.e. one buffer for the frame and no slack.
    const key = hexToBytes(VECTORS.expected.attempt_key_hex);
    const seq = VECTORS.inputs.seq;
    const plaintext = hexToBytes(VECTORS.inputs.plaintext_hex);

    const data = await sealFrame(key, seq, 1, plaintext);
    assert.equal(bytesToHex(data), VECTORS.expected.frame_data_seq7_hex);
    assert.equal(data.byteOffset, 0, "the frame owns its buffer from byte zero");
    assert.equal(data.buffer.byteLength, data.byteLength, "no slack: one exact allocation");

    const finalBytes = new Uint8Array(8);
    new DataView(finalBytes.buffer).setBigUint64(0, BigInt(plaintext.length), false);
    const final = await sealFrame(key, seq, 2, finalBytes);
    assert.equal(bytesToHex(final), VECTORS.expected.frame_final_seq7_hex);
    assert.equal(final.byteOffset, 0);
    assert.equal(final.buffer.byteLength, final.byteLength);

    // The receive half is what the copy was removed from: a frame that sits
    // INSIDE a larger buffer must open from a view, without being copied out
    // first. If `openFrame` ever goes back to `slice`, this still passes —
    // but the stage numbers move, so the claim lives in the perf doc and the
    // behaviour lives here.
    const padded = new Uint8Array(data.length + 9);
    padded.set(data, 5);
    const opened = await openFrame(key, padded.subarray(5, 5 + data.length), seq);
    assert.equal(bytesToHex(opened.plaintext), VECTORS.inputs.plaintext_hex);
    assert.equal(opened.seq, seq);
  });
});
