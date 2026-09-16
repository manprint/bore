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
  RTC_HIGH_WATER,
  createAttemptRtc,
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
function harness(role, { maxMessageSize } = {}) {
  const signals = [];
  const events = { ready: [], failed: [], messages: [], closed: 0 };
  let pc = null;
  const actor = createAttemptRtc({
    role,
    transferId: TID,
    attemptId: AID,
    iceServers: ["stun:stun.example:3478"],
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
    // The end marker is not a candidate and adds nothing.
    await ok.actor.handleSignal("rtc.ice", { candidate: null });
    assert.equal(ok.pc().addedCandidates.length, 3);
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
    assert.equal(channel.bufferedAmountLowThreshold, 1024 * 1024);

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
