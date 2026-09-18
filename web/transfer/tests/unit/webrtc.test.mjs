// Direct-attempt actor (4.2): fixed roles, one channel, bounded candidate
// queue, the readiness contract and the failure/cleanup rules — all against
// doubles that implement the WebRTC call shape, never a real engine.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import {
  CHANNEL_LABEL,
  CHANNEL_PROTOCOL,
  DISCONNECT_GRACE_MS,
  MAX_FRAGMENT_BYTES,
  MAX_REMOTE_CANDIDATES,
  MAX_CARRIERS,
  RTC_HIGH_WATER,
  RTC_LOW_WATER,
  createAttemptRtc,
  createCarrierGroup,
  filterIceServers,
  fragmentBytesFor,
} from "../../src/webrtc.js";

const TID = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AID = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

function evt(type, extra) {
  return Object.assign(new Event(type), extra);
}

class FakeChannel extends EventTarget {
  constructor(label, init = {}) {
    super();
    this.label = label;
    this.protocol = init.protocol ?? "";
    this.ordered = init.ordered !== false;
    this.readyState = "connecting";
    this.bufferedAmount = 0;
    this.bufferedAmountLowThreshold = 0;
    this.binaryType = "blob";
    this.sent = [];
    this.closeCalls = 0;
  }

  send(bytes) {
    if (this.readyState !== "open") {
      throw new Error("channel is not open");
    }
    this.sent.push(bytes);
    this.bufferedAmount += bytes.byteLength ?? bytes.length ?? 0;
  }

  close() {
    this.closeCalls += 1;
    if (this.readyState !== "closed") {
      this.readyState = "closed";
      this.dispatchEvent(evt("close"));
    }
  }

  becomeOpen() {
    this.readyState = "open";
    this.dispatchEvent(evt("open"));
  }

  deliver(data) {
    this.dispatchEvent(evt("message", { data }));
  }

  drain() {
    this.bufferedAmount = 0;
    this.dispatchEvent(evt("bufferedamountlow"));
  }
}

class FakePc extends EventTarget {
  constructor(config) {
    super();
    this.config = config;
    this.connectionState = "new";
    this.iceConnectionState = "new";
    this.localDescription = null;
    this.remoteDescription = null;
    this.sctp = { maxMessageSize: 262_144 };
    this.created = [];
    this.addedCandidates = [];
    this.closeCalls = 0;
  }

  createDataChannel(label, init) {
    const channel = new FakeChannel(label, init);
    this.created.push(channel);
    return channel;
  }

  async createOffer() {
    return { type: "offer", sdp: "v=0\r\nlocal-offer" };
  }

  async createAnswer() {
    return { type: "answer", sdp: "v=0\r\nlocal-answer" };
  }

  async setLocalDescription(description) {
    this.localDescription = description;
  }

  async setRemoteDescription(description) {
    this.remoteDescription = description;
  }

  async addIceCandidate(init) {
    this.addedCandidates.push(init);
  }

  close() {
    this.closeCalls += 1;
  }

  emitCandidate(candidate) {
    this.dispatchEvent(evt("icecandidate", { candidate }));
  }

  moveTo(state) {
    this.connectionState = state;
    this.dispatchEvent(evt("connectionstatechange"));
  }

  handOverChannel(channel) {
    this.dispatchEvent(evt("datachannel", { channel }));
  }
}

/** One actor plus everything a test needs to drive and observe it. */
function harness(role, { maxMessageSize, drainTimeoutMs } = {}) {
  const signals = [];
  const events = { ready: [], failed: [], messages: [], closed: 0 };
  let pc = null;
  const actor = createAttemptRtc({
    role,
    transferId: TID,
    attemptId: AID,
    iceServers: ["stun:stun.example:3478"],
    // The deadline is a seam, not a clock to wait out: a test that needed the
    // shipped ten seconds would either be ten seconds long or would not test
    // the deadline at all.
    ...(drainTimeoutMs === undefined ? {} : { drainTimeoutMs }),
    sendSignal: (type, body) => {
      signals.push({ type, body });
      return true;
    },
    createPeerConnection: (config) => {
      pc = new FakePc(config);
      if (maxMessageSize !== undefined) {
        pc.sctp = maxMessageSize === null ? undefined : { maxMessageSize };
      }
      return pc;
    },
    events: {
      onReady: (info) => events.ready.push(info),
      onFailed: (reason) => events.failed.push(reason),
      onMessage: (data) => events.messages.push(data),
      onClosed: () => {
        events.closed += 1;
      },
    },
  });
  return {
    actor,
    signals,
    events,
    pc: () => pc,
    types: () => signals.map((s) => s.type),
  };
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** A group of N carriers over the same doubles, plus what a test needs. */
function groupHarness(role, carriers, { drainTimeoutMs } = {}) {
  const signals = [];
  const events = { ready: [], failed: [], messages: [], closed: 0 };
  const pcs = [];
  const actor = createCarrierGroup({
    role,
    transferId: TID,
    attemptId: AID,
    carriers,
    iceServers: ["stun:stun.example:3478"],
    ...(drainTimeoutMs === undefined ? {} : { drainTimeoutMs }),
    sendSignal: (type, body) => {
      signals.push({ type, body });
      return true;
    },
    createPeerConnection: (config) => {
      const pc = new FakePc(config);
      pcs.push(pc);
      return pc;
    },
    events: {
      onReady: (info) => events.ready.push(info),
      onFailed: (reason) => events.failed.push(reason),
      onMessage: (data) => events.messages.push(data),
      onClosed: () => {
        events.closed += 1;
      },
    },
  });
  return { actor, signals, events, pcs };
}

/**
 * Brings one carrier of an offerer group to `open`: its own offer goes out,
 * the peer answers it, and the channel it created opens.
 */
async function openCarrier(h, index) {
  const pc = h.pcs[index];
  h.actor.handleSignal("rtc.answer", {
    transferId: TID,
    attemptId: AID,
    ...(index === 0 ? {} : { carrier: index }),
    sdp: "v=0\r\nremote-answer",
  });
  await wait(0);
  pc.created[0].becomeOpen();
  await wait(0);
  return pc.created[0];
}

test("web-transfer webrtc", async (t) => {
  await t.test("receiver_creates_exactly_one_ordered_reliable_channel", async () => {
    const h = harness("offerer");
    await h.actor.start();
    const pc = h.pc();
    assert.equal(pc.created.length, 1, "the offerer creates exactly one channel");
    const [channel] = pc.created;
    assert.equal(channel.label, CHANNEL_LABEL);
    assert.equal(channel.protocol, CHANNEL_PROTOCOL);
    assert.equal(channel.ordered, true, "the channel is ordered and reliable");
    assert.equal(channel.binaryType, "arraybuffer");
    // Only STUN reaches the engine, and no credential travels with it.
    assert.deepEqual(pc.config.iceServers, [{ urls: "stun:stun.example:3478" }]);
  });

  await t.test("source_never_creates_channel_and_accepts_only_expected_one", async () => {
    const h = harness("answerer");
    await h.actor.start();
    const pc = h.pc();
    assert.equal(pc.created.length, 0, "the answerer never calls createDataChannel");
    const channel = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    pc.handOverChannel(channel);
    await h.actor.handleSignal("rtc.offer", { transferId: TID, attemptId: AID, sdp: "v=0 remote" });
    channel.becomeOpen();
    assert.deepEqual(h.events.failed, []);
    assert.equal(h.events.ready.length, 1);
    // A SECOND channel on the same connection is not part of the contract.
    pc.handOverChannel(new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL }));
    assert.deepEqual(h.events.failed, ["protocol"]);
  });

  await t.test("offer_answer_and_candidates_follow_fixed_roles", async () => {
    const offer = harness("offerer");
    await offer.actor.start();
    assert.deepEqual(offer.types(), ["rtc.offer"]);
    assert.equal(offer.signals[0].body.sdp, "v=0\r\nlocal-offer");
    assert.equal(offer.signals[0].body.transferId, TID);
    assert.equal(offer.signals[0].body.attemptId, AID);
    // An offerer that receives an offer is talking to something that is not
    // playing the fixed roles.
    await offer.actor.handleSignal("rtc.offer", { sdp: "v=0 stray" });
    assert.deepEqual(offer.events.failed, ["protocol"]);

    const answer = harness("answerer");
    await answer.actor.start();
    assert.deepEqual(answer.types(), [], "the answerer signals nothing until the offer lands");
    await answer.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    assert.deepEqual(answer.types(), ["rtc.answer"]);
    assert.equal(answer.signals[0].body.sdp, "v=0\r\nlocal-answer");
    // An answerer never receives an answer.
    await answer.actor.handleSignal("rtc.answer", { sdp: "v=0 stray" });
    assert.deepEqual(answer.events.failed, ["protocol"]);

    // Trickle: every local candidate is forwarded, and the end of gathering
    // travels as the ONE null-candidate shape the server normalises to.
    const trickle = harness("offerer");
    await trickle.actor.start();
    trickle.pc().emitCandidate({ candidate: "candidate:1 udp", sdpMid: "0", sdpMLineIndex: 0 });
    trickle.pc().emitCandidate(null);
    assert.deepEqual(trickle.types(), ["rtc.offer", "rtc.ice", "rtc.ice"]);
    assert.deepEqual(trickle.signals[1].body, {
      transferId: TID,
      attemptId: AID,
      candidate: "candidate:1 udp",
      sdpMid: "0",
      sdpMLineIndex: 0,
    });
    assert.deepEqual(trickle.signals[2].body, {
      transferId: TID,
      attemptId: AID,
      candidate: null,
    });
  });

  await t.test("remote_candidates_queue_is_bounded_until_description", async () => {
    const h = harness("offerer");
    await h.actor.start();
    for (let i = 0; i < MAX_REMOTE_CANDIDATES; i++) {
      await h.actor.handleSignal("rtc.ice", { candidate: `candidate:${i} udp` });
    }
    assert.equal(h.actor.state().pendingCandidates, MAX_REMOTE_CANDIDATES);
    assert.equal(h.pc().addedCandidates.length, 0, "nothing is added before the description");
    // The 129th is past the same budget the server enforces.
    await h.actor.handleSignal("rtc.ice", { candidate: "candidate:over udp" });
    assert.deepEqual(h.events.failed, ["protocol"]);

    const ok = harness("offerer");
    await ok.actor.start();
    await ok.actor.handleSignal("rtc.ice", { candidate: "candidate:a udp", sdpMid: "0" });
    await ok.actor.handleSignal("rtc.ice", { candidate: "candidate:b udp" });
    assert.equal(ok.pc().addedCandidates.length, 0);
    await ok.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.equal(ok.pc().addedCandidates.length, 2, "the queue drains once the description is set");
    assert.equal(ok.actor.state().pendingCandidates, 0);
    // After the description, candidates go straight through.
    await ok.actor.handleSignal("rtc.ice", { candidate: "candidate:c udp" });
    assert.equal(ok.pc().addedCandidates.length, 3);
    // The end marker REACHES the agent (see the next test), exactly once.
    await ok.actor.handleSignal("rtc.ice", { candidate: null });
    assert.deepEqual(ok.pc().addedCandidates[3], null);
    await ok.actor.handleSignal("rtc.ice", { candidate: null });
    assert.equal(ok.pc().addedCandidates.length, 4, "the marker is idempotent");
  });

  // V003-F05. The marker used to be DROPPED here, on the argument that an
  // engine can read the end from its own gathering state — which is about the
  // LOCAL candidates and says nothing about the peer's. WebRTC 1.0 passes an
  // end-of-candidates indication to `addIceCandidate()`, with `null` meaning
  // "for every media description".
  await t.test("remote_end_of_candidates_is_applied_after_queued_candidates", async () => {
    const early = harness("offerer");
    await early.actor.start();
    // The marker arrives BEFORE the description, together with candidates
    // that must go in first: it waits in one bounded slot.
    await early.actor.handleSignal("rtc.ice", { candidate: "candidate:a udp", sdpMid: "0" });
    await early.actor.handleSignal("rtc.ice", { candidate: null });
    await early.actor.handleSignal("rtc.ice", { candidate: null });
    assert.equal(early.pc().addedCandidates.length, 0, "nothing is added before the description");
    assert.equal(
      early.actor.state().pendingCandidates,
      1,
      "the marker does not occupy a candidate slot",
    );
    await early.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.deepEqual(
      early.pc().addedCandidates,
      [{ candidate: "candidate:a udp", sdpMid: "0" }, null],
      "the candidate goes in first and the marker exactly once after it",
    );
    // A marker arriving later cannot repeat it.
    await early.actor.handleSignal("rtc.ice", { candidate: null });
    assert.equal(early.pc().addedCandidates.length, 2);

    // A whole queue drains before the marker, in order.
    const many = harness("offerer");
    await many.actor.start();
    for (let i = 0; i < 5; i += 1) {
      await many.actor.handleSignal("rtc.ice", { candidate: `candidate:${i} udp` });
    }
    await many.actor.handleSignal("rtc.ice", { candidate: null });
    await many.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    const added = many.pc().addedCandidates;
    assert.equal(added.length, 6);
    assert.deepEqual(
      added.slice(0, 5).map((init) => init.candidate),
      ["candidate:0 udp", "candidate:1 udp", "candidate:2 udp", "candidate:3 udp", "candidate:4 udp"],
    );
    assert.equal(added[5], null, "the marker is last");
    // The 128-candidate bound is untouched by the marker.
    assert.equal(many.actor.state().pendingCandidates, 0);
  });

  // V003-F02. The deadline used to RESOLVE while the queue was still above
  // the high-water mark, and `sendArchive` answered by reading and queueing
  // the next 1 MiB chunk into a browser queue that had stopped draining.
  await t.test("stalled_channel_never_queues_after_drain_deadline", async () => {
    const h = harness("answerer", { drainTimeoutMs: 30 });
    await h.actor.start();
    const channel = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    h.pc().handOverChannel(channel);
    await h.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    channel.becomeOpen();
    assert.equal(h.events.ready.length, 1);

    const sink = h.actor.sink;
    channel.bufferedAmount = RTC_HIGH_WATER + 1;
    const queuedBefore = channel.bufferedAmount;
    const sentBefore = channel.sent.length;
    const outcome = await sink
      .waitLow(new AbortController().signal)
      .then(() => "resolved", (error) => error.name);

    // The wait REJECTS, which is what the sender reads as "this attempt was
    // abandoned": it stops reading the file and waits for the relay attempt.
    assert.equal(outcome, "AbortError");
    // Exactly one failure, with the fixed reason the wire already carries.
    assert.deepEqual(h.events.failed, ["timeout"]);
    // And nothing more can be queued: the channel is gone with the attempt.
    assert.throws(
      () => sink.send(new Uint8Array(16)),
      (error) => error.name === "AbortError",
    );
    assert.equal(channel.sent.length, sentBefore, "no byte was queued after the deadline");
    assert.equal(channel.bufferedAmount, queuedBefore, "the queue never grew");

    // The one case resolving is right for: the queue really did drain and
    // only the engine's event was missed.
    const missed = harness("answerer", { drainTimeoutMs: 30 });
    await missed.actor.start();
    const quiet = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    missed.pc().handOverChannel(quiet);
    await missed.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    quiet.becomeOpen();
    quiet.bufferedAmount = RTC_HIGH_WATER + 1;
    const parked = missed.actor.sink
      .waitLow(new AbortController().signal)
      .then(() => "resolved", (error) => error.name);
    quiet.bufferedAmount = 0;
    assert.equal(await parked, "resolved");
    assert.deepEqual(missed.events.failed, []);
  });

  await t.test("ready_requires_open_valid_channel_and_min_message_size", async () => {
    // A channel that opens before the answer is NOT ready: the server refuses
    // a `direct_ready` from a side that has not finished its own SDP step.
    const early = harness("offerer");
    await early.actor.start();
    early.pc().created[0].becomeOpen();
    assert.deepEqual(early.events.ready, [], "no ready before the remote description");
    await early.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.equal(early.events.ready.length, 1);

    // A connection that has already failed never becomes ready.
    const dead = harness("offerer");
    await dead.actor.start();
    dead.pc().moveTo("failed");
    assert.deepEqual(dead.events.failed, ["ice-failed"]);
    assert.deepEqual(dead.events.ready, []);

    // A channel that cannot carry the floor is unusable, not a smaller one.
    const tiny = harness("offerer", { maxMessageSize: 512 });
    await tiny.actor.start();
    tiny.pc().created[0].becomeOpen();
    await tiny.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.deepEqual(tiny.events.ready, []);
    assert.deepEqual(tiny.events.failed, ["unsupported"]);

    // The wrong label or an unordered channel is refused too.
    const wrong = harness("answerer");
    await wrong.actor.start();
    const channel = new FakeChannel("something-else", { protocol: CHANNEL_PROTOCOL });
    wrong.pc().handOverChannel(channel);
    await wrong.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    channel.becomeOpen();
    assert.deepEqual(wrong.events.ready, []);
    assert.deepEqual(wrong.events.failed, ["protocol"]);
  });

  await t.test("fragment_size_respects_negotiated_max", async () => {
    // The relay's 24 KiB is the CEILING on every path, never raised.
    assert.equal(fragmentBytesFor(262_144), MAX_FRAGMENT_BYTES);
    assert.equal(fragmentBytesFor(65_536), MAX_FRAGMENT_BYTES);
    // A peer that says less gets less, with room for header and tag.
    assert.equal(fragmentBytesFor(16_384), 16_384 - 64);
    assert.equal(fragmentBytesFor(1_088), 1_024);
    // Below the floor the channel is unusable.
    assert.equal(fragmentBytesFor(1_087), null);
    assert.equal(fragmentBytesFor(0), null);
    // An engine that publishes no limit gets the conservative value.
    assert.equal(fragmentBytesFor(undefined), MAX_FRAGMENT_BYTES);

    const h = harness("offerer", { maxMessageSize: 8_192 });
    await h.actor.start();
    h.pc().created[0].becomeOpen();
    await h.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.equal(h.events.ready[0].fragmentBytes, 8_192 - 64);
    assert.equal(h.actor.sink.fragmentBytes, 8_192 - 64);
  });

  // 4.6 catalogue entry 2. The fragment is a property of the PEER, not a
  // constant this side picks, and the harness may only make it SMALLER — the
  // measured winner is recorded per engine in `WEB_TRANSFER_PERF.md`, and a
  // knob that could raise it past what the peer accepts would break the
  // channel instead of measuring it.
  await t.test("fragment_size_is_derived_from_peer_max_message_size", async () => {
    for (const [maxMessageSize, expected] of [
      [262_144, MAX_FRAGMENT_BYTES],
      [65_536, MAX_FRAGMENT_BYTES],
      [24_640, MAX_FRAGMENT_BYTES],
      [16_384, 16_384 - 64],
      [1_088, 1_024],
    ]) {
      const h = harness("offerer", { maxMessageSize });
      await h.actor.start();
      h.pc().created[0].becomeOpen();
      await h.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
      assert.equal(h.events.ready[0].fragmentBytes, expected, `peer ${maxMessageSize}`);
      assert.equal(h.actor.sink.fragmentBytes, expected);
      h.actor.close();
    }

    // The harness override: smaller is measured, larger is ignored.
    try {
      globalThis.__borePerf = { fragmentBytes: 8_192 };
      const small = harness("offerer", { maxMessageSize: 262_144 });
      await small.actor.start();
      small.pc().created[0].becomeOpen();
      await small.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
      assert.equal(small.events.ready[0].fragmentBytes, 8_192);
      small.actor.close();

      globalThis.__borePerf = { fragmentBytes: 1024 * 1024 };
      const big = harness("offerer", { maxMessageSize: 16_384 });
      await big.actor.start();
      big.pc().created[0].becomeOpen();
      await big.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
      assert.equal(
        big.events.ready[0].fragmentBytes,
        16_384 - 64,
        "the peer's number is the ceiling and the harness cannot raise it",
      );
      big.actor.close();
    } finally {
      delete globalThis.__borePerf;
    }
  });

  // 4.6 catalogue entry 3. BW-F2's browser form: one transfer is ONE channel.
  // Spreading the fragments of one file over several channels reorders them
  // on a path that is only ordered per channel — the same trap that made the
  // native direct path flow-pinned instead of round-robin — and on a single
  // ordered channel it buys nothing anyway.
  await t.test("one_channel_per_transfer_and_extra_channels_are_refused", async () => {
    const offerer = harness("offerer");
    await offerer.actor.start();
    assert.equal(offerer.pc().created.length, 1, "the offerer creates exactly one channel");
    // Starting again does not open a second one.
    await offerer.actor.start();
    assert.equal(offerer.pc().created.length, 1, "start is idempotent about the channel");
    // Nor does the peer handing one over to the side that already made its own.
    offerer.pc().handOverChannel(new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL }));
    assert.deepEqual(offerer.events.failed, ["protocol"]);

    const answerer = harness("answerer");
    await answerer.actor.start();
    assert.equal(answerer.pc().created.length, 0, "the answerer never creates a channel");
    const first = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    answerer.pc().handOverChannel(first);
    await answerer.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    first.becomeOpen();
    assert.equal(answerer.events.ready.length, 1);
    const second = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    answerer.pc().handOverChannel(second);
    assert.deepEqual(answerer.events.failed, ["protocol"], "a second channel ends the attempt");
    // And the extra channel never became the sink: nothing was written to it.
    assert.deepEqual(second.sent, []);
  });

  await t.test("high_low_water_pauses_before_next_file_read", async () => {
    const h = harness("answerer");
    await h.actor.start();
    const channel = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
    h.pc().handOverChannel(channel);
    await h.actor.handleSignal("rtc.offer", { sdp: "v=0 remote" });
    channel.becomeOpen();
    assert.equal(h.events.ready.length, 1);
    // The low-water threshold is declared to the engine, so the drain event
    // exists at all.
    assert.equal(channel.bufferedAmountLowThreshold, RTC_LOW_WATER);

    const sink = h.actor.sink;
    // Below high water the pipeline never waits.
    channel.bufferedAmount = 0;
    await sink.waitLow(new AbortController().signal);

    // Above it, `waitLow` parks until the engine says the queue drained —
    // the browser half of "await room, never queue past it".
    channel.bufferedAmount = RTC_HIGH_WATER + 1;
    let resolved = false;
    const parked = sink.waitLow(new AbortController().signal).then(() => {
      resolved = true;
    });
    await wait(20);
    assert.equal(resolved, false, "the send loop is paused while the channel is full");
    channel.drain();
    await parked;
    assert.equal(resolved, true);

    // An abort releases it too, as an error the pipeline unwinds on.
    channel.bufferedAmount = RTC_HIGH_WATER + 1;
    const controller = new AbortController();
    const aborted = sink.waitLow(controller.signal).then(
      () => "resolved",
      (error) => error.name,
    );
    controller.abort();
    assert.equal(await aborted, "AbortError");
  });

  await t.test("transient_disconnect_has_two_second_grace", async () => {
    const healed = harness("offerer");
    await healed.actor.start();
    healed.pc().created[0].becomeOpen();
    await healed.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    healed.pc().moveTo("disconnected");
    await wait(DISCONNECT_GRACE_MS / 2);
    assert.deepEqual(healed.events.failed, [], "a blip is not a failure");
    healed.pc().moveTo("connected");
    await wait(DISCONNECT_GRACE_MS + 200);
    assert.deepEqual(healed.events.failed, [], "a healed link never falls back");

    const stuck = harness("offerer");
    await stuck.actor.start();
    stuck.pc().created[0].becomeOpen();
    await stuck.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    stuck.pc().moveTo("disconnected");
    await wait(DISCONNECT_GRACE_MS + 200);
    assert.deepEqual(stuck.events.failed, ["ice-failed"]);
  });

  await t.test("a_write_to_a_dead_channel_ends_the_attempt_not_the_transfer", async () => {
    // The engine reports a dead channel two ways and the event is the slower
    // one (a transient disconnection is even held for two seconds first), so
    // the WRITE has to be what notices. It must report `send-error` exactly
    // once and raise an `AbortError`, which is the sender's signal that only
    // this attempt was abandoned — a plain error there failed the whole
    // transfer and the relay ticket then landed on a record that was gone.
    const h = harness("offerer");
    await h.actor.start();
    const channel = h.pc().created[0];
    channel.becomeOpen();
    await h.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    assert.equal(h.events.ready.length, 1);
    h.actor.sink.send(new Uint8Array(8));
    assert.equal(channel.sent.length, 1);
    // The state flips before the `close` event is delivered — the real
    // ordering, and the one the grace period widens.
    channel.readyState = "closed";
    assert.throws(
      () => h.actor.sink.send(new Uint8Array(8)),
      (error) => error.name === "AbortError",
    );
    assert.deepEqual(h.events.failed, ["send-error"]);
    // The close event lands afterwards and adds nothing: one failure, one
    // notice, and a further write still unwinds as an attempt abort.
    channel.close();
    assert.throws(
      () => h.actor.sink.send(new Uint8Array(8)),
      (error) => error.name === "AbortError",
    );
    assert.deepEqual(h.events.failed, ["send-error"]);
    // An engine that throws from an `open` channel is the same case.
    const other = harness("offerer");
    await other.actor.start();
    const dying = other.pc().created[0];
    dying.becomeOpen();
    await other.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    dying.send = () => {
      throw new Error("engine refused the write");
    };
    assert.throws(
      () => other.actor.sink.send(new Uint8Array(8)),
      (error) => error.name === "AbortError",
    );
    assert.deepEqual(other.events.failed, ["send-error"]);
  });

  await t.test("failure_is_reported_once_and_cleanup_is_idempotent", async () => {
    const h = harness("offerer");
    await h.actor.start();
    const pc = h.pc();
    const channel = pc.created[0];
    channel.becomeOpen();
    await h.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });

    // Text is outside the contract and ends the attempt.
    channel.deliver("not bytes");
    assert.deepEqual(h.events.failed, ["protocol"]);
    assert.equal(pc.closeCalls, 1, "the peer connection is closed exactly once");

    // Every later event is inert: one failure, one teardown.
    channel.deliver(new ArrayBuffer(8));
    pc.moveTo("failed");
    h.actor.close();
    h.actor.close();
    assert.deepEqual(h.events.failed, ["protocol"]);
    assert.deepEqual(h.events.messages, []);
    assert.equal(pc.closeCalls, 1);

    // An oversized message is refused the same way, and a good one is not.
    const sized = harness("offerer");
    await sized.actor.start();
    const good = sized.pc().created[0];
    good.becomeOpen();
    await sized.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
    good.deliver(new ArrayBuffer(24 * 1024 + 32));
    assert.equal(sized.events.messages.length, 1);
    good.deliver(new ArrayBuffer(64 * 1024 + 1));
    assert.deepEqual(sized.events.failed, ["protocol"]);
    assert.equal(sized.events.messages.length, 1, "the oversized message is not delivered");
  });

  await t.test("no_media_or_permission_api_is_called", async () => {
    const source = readFileSync(fileURLToPath(new URL("../../src/webrtc.js", import.meta.url)), "utf8");
    for (const forbidden of [
      "getUserMedia",
      "getDisplayMedia",
      "mediaDevices",
      "navigator.permissions",
      "addTrack",
      "addTransceiver",
    ]) {
      assert.equal(source.includes(forbidden), false, `${forbidden} must never appear`);
    }
    // And the live actor touches nothing on `navigator` either.
    const touched = [];
    const realNavigator = globalThis.navigator;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: new Proxy(
        {},
        {
          get(_target, property) {
            touched.push(String(property));
            return undefined;
          },
        },
      ),
    });
    try {
      const h = harness("offerer");
      await h.actor.start();
      h.pc().created[0].becomeOpen();
      await h.actor.handleSignal("rtc.answer", { sdp: "v=0 remote" });
      h.actor.close();
    } finally {
      if (realNavigator === undefined) {
        delete globalThis.navigator;
      } else {
        Object.defineProperty(globalThis, "navigator", {
          configurable: true,
          value: realNavigator,
        });
      }
    }
    assert.deepEqual(touched, []);
  });

  await t.test("ice_server_list_keeps_stun_only_and_drops_credentials", () => {
    assert.deepEqual(
      filterIceServers([
        "stun:stun.example:3478",
        "stuns:secure.example:5349",
        "turn:turn.example:3478",
        { urls: "turns:turn.example:5349", username: "u", credential: "c" },
        { urls: "stun:object.example:3478", username: "u", credential: "c" },
        "http://not-a-stun-url",
        null,
        42,
      ]),
      [
        { urls: "stun:stun.example:3478" },
        { urls: "stuns:secure.example:5349" },
        { urls: "stun:object.example:3478" },
      ],
    );
    assert.deepEqual(filterIceServers(undefined), []);
    assert.deepEqual(filterIceServers([]), []);
  });

  await t.test("an_engine_without_webrtc_is_reported_as_unsupported", async () => {
    const failures = [];
    const actor = createAttemptRtc({
      role: "offerer",
      transferId: TID,
      attemptId: AID,
      iceServers: [],
      sendSignal: () => true,
      createPeerConnection: () => {
        throw new Error("no WebRTC here");
      },
      events: { onFailed: (reason) => failures.push(reason) },
    });
    await actor.start();
    assert.deepEqual(failures, ["unsupported"]);
  });
});

test("one_carrier_is_the_actor_itself_and_puts_no_carrier_on_the_wire", async () => {
  // The count is a ceiling, and at its lowest value the group must not exist
  // at all: an attempt that asked for one carrier runs the code that ran
  // before carriers, and its signalling carries no field that did not exist
  // then. This is what `carriers <= 1` promises, and it is the cheapest thing
  // to break by "unifying" the two paths.
  const h = groupHarness("offerer", 1);
  await h.actor.start();
  assert.equal(h.pcs.length, 1, "one peer connection");
  assert.ok(h.signals.length > 0, "the offerer offers");
  for (const signal of h.signals) {
    assert.ok(
      !Object.hasOwn(signal.body, "carrier"),
      `carrier 0 stays off the wire: ${JSON.stringify(signal.body)}`,
    );
  }
  // Not the group's shape: the single actor's.
  assert.equal(h.actor.state().carriers, undefined);
  assert.equal(h.actor.state().role, "offerer");
  h.actor.close();
});

test("a_deliberate_close_records_why_it_happened_on_every_carrier", async () => {
  // B-A037's diagnosis surface. A field report arrived as four carrier
  // traces ending in `{"ev":"closed"}` with `reason: null` while their ICE
  // pairs read `succeeded` at 10-15 ms rtt — and a bare `closed` mark cannot
  // be told from any other, so "the transfer finished", "the user cancelled"
  // and "the SERVER declared this attempt failed" looked identical. They mean
  // opposite things. The cause now rides the mark, through the trace's own
  // allow-list, so it can only ever be a short lowercase enumeration.
  const h = groupHarness("offerer", 2);
  await h.actor.start();
  h.actor.close("server-failed", "timeout");
  const traces = h.actor.diagnostics();
  assert.equal(traces.length, 2, "one trace per carrier");
  for (const trace of traces) {
    const closed = trace.events.filter((event) => event.ev === "closed");
    assert.equal(closed.length, 1, `one closed mark: ${JSON.stringify(trace.events)}`);
    assert.equal(closed[0].cause, "server-failed");
    assert.equal(closed[0].code, "timeout");
  }

  // A teardown with nothing to say still leaves a bare mark: the field is
  // additive, never invented.
  const q = groupHarness("offerer", 1);
  await q.actor.start();
  q.actor.close();
  for (const trace of [].concat(q.actor.diagnostics())) {
    const closed = trace.events.filter((event) => event.ev === "closed");
    assert.equal(closed.length, 1);
    assert.equal(closed[0].cause, undefined);
    assert.equal(closed[0].code, undefined);
  }

  // And the allow-list is the gate, not the caller: a cause that is not a
  // short lowercase enumeration is DROPPED rather than written.
  const r = groupHarness("offerer", 1);
  await r.actor.start();
  r.actor.close("Server said: 10.0.0.7 timed out", "ok");
  for (const trace of [].concat(r.actor.diagnostics())) {
    const closed = trace.events.filter((event) => event.ev === "closed");
    assert.equal(closed[0].cause, undefined);
    assert.equal(closed[0].code, "ok");
  }
});

test("each_carrier_negotiates_under_its_own_index", async () => {
  const h = groupHarness("offerer", 3);
  await h.actor.start();
  assert.equal(h.pcs.length, 3, "one peer connection per carrier");
  const offers = h.signals.filter((s) => s.type === "rtc.offer");
  assert.equal(offers.length, 3);
  assert.deepEqual(
    offers.map((s) => s.body.carrier ?? 0),
    [0, 1, 2],
    "one offer each, indexed",
  );
  assert.ok(!Object.hasOwn(offers[0].body, "carrier"), "carrier 0 is implicit");
  // An answer is routed to the carrier it names, and to no other.
  await openCarrier(h, 2);
  const states = h.actor.state().members;
  assert.equal(states[2].ready, true);
  assert.equal(states[0].ready, false);
  assert.equal(states[1].ready, false);
  h.actor.close();
});

test("the_attempt_is_ready_on_the_first_carrier_and_dead_only_on_the_last", async () => {
  // A ceiling and not a reservation: three carriers out of four is a working
  // direct path, and reporting the attempt failed while any carrier still
  // carries bytes would send a healthy transfer to the relay.
  const h = groupHarness("offerer", 3);
  await h.actor.start();
  await openCarrier(h, 0);
  assert.equal(h.events.ready.length, 1, "ready once, on the first carrier");
  await openCarrier(h, 1);
  assert.equal(h.events.ready.length, 1, "ready is a per-attempt statement");
  assert.equal(h.actor.state().readyCount, 2);
  // Two of three die: the attempt is still alive.
  h.pcs[0].created[0].close();
  await wait(0);
  assert.deepEqual(h.events.failed, [], "one carrier is not the attempt");
  h.pcs[1].created[0].close();
  await wait(0);
  // Carrier 2 never opened; closing the group ends it. The failure is
  // reported only when nothing is left to carry bytes.
  assert.deepEqual(h.events.failed, [], "the third carrier is still trying");
  h.actor.close();
});

test("the_sink_writes_to_the_least_loaded_carrier", async () => {
  // Work-conserving on purpose. A fixed round-robin parks the writer on a
  // carrier whose queue is full while the others sit idle — the head-of-line
  // that carriers exist to remove.
  const h = groupHarness("offerer", 3);
  await h.actor.start();
  const channels = [
    await openCarrier(h, 0),
    await openCarrier(h, 1),
    await openCarrier(h, 2),
  ];
  // Three writes with everything idle: one each, because each write makes
  // the carrier it chose the most loaded.
  for (let n = 0; n < 3; n += 1) {
    h.actor.sink.send(new Uint8Array(100));
  }
  assert.deepEqual(
    channels.map((channel) => channel.sent.length),
    [1, 1, 1],
    "the load spreads",
  );
  // Now make carrier 1 the emptiest by hand: every following write goes
  // there until it is no longer the emptiest.
  channels[1].bufferedAmount = 0;
  h.actor.sink.send(new Uint8Array(10));
  assert.equal(channels[1].sent.length, 2);
  // And the aggregate the sender reads is the sum, not one carrier's.
  assert.equal(
    h.actor.sink.bufferedAmount,
    channels.reduce((total, channel) => total + channel.bufferedAmount, 0),
  );
  h.actor.close();
});

test("a_carrier_that_dies_under_a_write_is_skipped_not_fatal", async () => {
  const h = groupHarness("offerer", 2);
  await h.actor.start();
  const first = await openCarrier(h, 0);
  const second = await openCarrier(h, 1);
  // The emptiest carrier dies between the choice and the write, which is
  // exactly the race a live channel can lose.
  first.readyState = "closed";
  h.actor.sink.send(new Uint8Array(100));
  assert.equal(second.sent.length, 1, "the frame went to the live carrier");
  assert.deepEqual(h.events.failed, [], "the attempt is still alive");
  // With nothing left, the write fails the way a single channel does, so the
  // sender reads it as an abandoned attempt and waits for the relay.
  second.readyState = "closed";
  assert.throws(
    () => h.actor.sink.send(new Uint8Array(100)),
    (error) => error?.name === "AbortError",
  );
  h.actor.close();
});

test("an_attempt_is_reported_dead_however_its_last_carrier_left", async () => {
  // A carrier can leave by more than one door: its own failure, or a write
  // that throws under the sink. While only the first door was counted, a
  // MIXED death left the tally one short of the carrier count for ever, so
  // `onFailed` never fired. MEASURED on a real browser pair: the page never
  // learned its direct attempt was dead, so it never attached the relay leg
  // the server had already ticketed, and the transfer sat until the 30 s
  // pairing timeout with the file half delivered.
  const h = groupHarness("offerer", 2);
  await h.actor.start();
  const first = await openCarrier(h, 0);
  await openCarrier(h, 1);

  // Door one: a write that throws.
  first.readyState = "closed";
  h.actor.sink.send(new Uint8Array(10));
  assert.deepEqual(h.events.failed, [], "one carrier of two is not the attempt");

  // Door two: the other carrier fails on its own.
  h.pcs[1].created[0].close();
  await wait(0);
  assert.equal(
    h.events.failed.length,
    1,
    "the attempt is reported dead exactly once, whichever door the last carrier took",
  );
  h.actor.close();
});

test("wait_low_returns_as_soon_as_any_carrier_drains", async () => {
  const h = groupHarness("offerer", 2, { drainTimeoutMs: 50 });
  await h.actor.start();
  const first = await openCarrier(h, 0);
  const second = await openCarrier(h, 1);
  first.bufferedAmount = RTC_HIGH_WATER + 1;
  second.bufferedAmount = RTC_HIGH_WATER + 1;
  let settled = false;
  const waiting = h.actor.sink.waitLow().then(() => {
    settled = true;
  });
  await wait(0);
  assert.equal(settled, false, "both carriers are above the mark");
  // ONE of them drains. The writer is released: waiting for the other would
  // be the head-of-line again, one level up.
  second.drain();
  await waiting;
  assert.equal(settled, true);
  h.actor.close();
});

test("the_carrier_count_is_bounded_however_the_server_names_it", async () => {
  // The page must not open an unbounded number of peer connections because a
  // message said so. The server enforces the same ceiling; this is the half
  // that does not depend on the server being the one that sent it.
  for (const [asked, expected] of [
    [0, 1],
    [-4, 1],
    ["3", 3],
    [MAX_CARRIERS + 9, MAX_CARRIERS],
    [Number.NaN, 1],
  ]) {
    const h = groupHarness("offerer", asked);
    await h.actor.start();
    assert.equal(h.pcs.length, expected, `carriers=${asked}`);
    h.actor.close();
  }
});
