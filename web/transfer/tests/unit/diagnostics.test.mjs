// Direct-path diagnostics (V003-C3): the trace must say WHICH of the three
// ways a direct attempt dies actually happened — the ICE path, the channel,
// or a queue that stopped draining — and it must be impossible for it to
// carry an address, a candidate line, an SDP, a name or a secret.
//
// The redaction is tested the only way that means anything: every string a
// real engine could hand us is a distinct CANARY, and the whole serialized
// report is searched for every one of them. A deny-list would pass this file
// and still leak the next field somebody adds; the allow-list in
// `diagnostics.js` is what makes the assertion hold for fields nobody wrote
// yet, so this file also feeds fields the summarizer has never heard of.
import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_STATS_SAMPLES,
  MAX_TRACE_EVENTS,
  MAX_TRACED_ATTEMPTS,
  candidateTypeOf,
  createAttemptTrace,
  createTraceStore,
  enumValue,
  numberValue,
  summarizeStats,
} from "../../src/diagnostics.js";
import { CHANNEL_LABEL, CHANNEL_PROTOCOL, RTC_HIGH_WATER, createAttemptRtc } from "../../src/webrtc.js";

const TID = "cccccccccccccccccccccccccccccccc";
const AID = "dddddddddddddddddddddddddddddddd";

/** Every string a real engine or a real peer could put in front of us. */
const CANARIES = {
  localAddress: "192.0.2.77",
  remoteAddress: "198.51.100.9",
  relatedAddress: "203.0.113.4",
  stunUrl: "stun:stun.canary.example:3478",
  candidateLine: "candidate:CANARYFOUNDATION 1 udp 2113937151 192.0.2.77 54321 typ srflx",
  remoteCandidateLine: "candidate:CANARYREMOTE 1 udp 2113937151 198.51.100.9 12345 typ host",
  sdp: "v=0\r\no=- CANARYSDP 2 IN IP4 192.0.2.77\r\n",
  ufrag: "CANARYUFRAG",
  peerName: "CANARY-NAME-pangolin",
  token: "CANARYTOKEN0123456789abcdef",
  fileName: "CANARY-PATH-tapir.bin",
};

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
  }

  send(bytes) {
    if (this.readyState !== "open") {
      throw new Error("channel is not open");
    }
    this.sent.push(bytes);
  }

  close() {
    if (this.readyState !== "closed") {
      this.readyState = "closed";
      this.dispatchEvent(evt("close"));
    }
  }

  becomeOpen() {
    this.readyState = "open";
    this.dispatchEvent(evt("open"));
  }
}

/** A stats report shaped like a real one, every string a canary. */
function canaryReport() {
  return new Map([
    [
      "T1",
      {
        id: "T1",
        type: "transport",
        dtlsState: "connected",
        iceState: "connected",
        selectedCandidatePairId: "P1",
        selectedCandidatePairChanges: 2,
        bytesSent: 12_345,
        bytesReceived: 678,
        // Fields the summarizer has never heard of, carrying secrets.
        srtpCipher: CANARIES.token,
        localCertificateId: CANARIES.peerName,
      },
    ],
    [
      "P1",
      {
        id: "P1",
        type: "candidate-pair",
        state: "succeeded",
        nominated: true,
        localCandidateId: "L1",
        remoteCandidateId: "R1",
        currentRoundTripTime: 0.0234,
        availableOutgoingBitrate: 9_876_543,
        bytesSent: 4_194_304,
        bytesReceived: 1_024,
        requestsSent: 7,
        responsesReceived: 7,
        packetsDiscardedOnSend: 3,
        // A real Chromium pair carries none of these, but a future one might.
        lastPacketSentTimestamp: 1,
        remoteCandidate: CANARIES.remoteCandidateLine,
      },
    ],
    [
      "L1",
      {
        id: "L1",
        type: "local-candidate",
        candidateType: "srflx",
        protocol: "udp",
        address: CANARIES.localAddress,
        ip: CANARIES.localAddress,
        port: 54_321,
        url: CANARIES.stunUrl,
        relatedAddress: CANARIES.relatedAddress,
        relatedPort: 9,
        usernameFragment: CANARIES.ufrag,
        candidate: CANARIES.candidateLine,
        foundation: "CANARYFOUNDATION",
      },
    ],
    [
      "R1",
      {
        id: "R1",
        type: "remote-candidate",
        candidateType: "host",
        protocol: "udp",
        address: CANARIES.remoteAddress,
        port: 12_345,
        usernameFragment: CANARIES.ufrag,
        candidate: CANARIES.remoteCandidateLine,
      },
    ],
    [
      "S1",
      {
        id: "S1",
        type: "sctp-transport",
        state: "connected",
        smoothedRoundTripTime: 0.041,
        congestionWindow: 262_144,
        receiverWindow: 1_048_576,
        mtu: 1_200,
        unackData: 8,
      },
    ],
    [
      "D1",
      {
        id: "D1",
        type: "data-channel",
        state: "open",
        label: CANARIES.fileName,
        protocol: CHANNEL_PROTOCOL,
        messagesSent: 512,
        bytesSent: 4_194_304,
        messagesReceived: 2,
        bytesReceived: 64,
      },
    ],
  ]);
}

/** A peer connection double whose every string is a canary. */
class FakePc extends EventTarget {
  constructor() {
    super();
    this.connectionState = "new";
    this.iceConnectionState = "new";
    this.iceGatheringState = "new";
    this.localDescription = null;
    this.sctp = { maxMessageSize: 262_144 };
    this.created = [];
    this.addedCandidates = [];
    this.statsCalls = 0;
  }

  createDataChannel(label, init) {
    const channel = new FakeChannel(label, init);
    this.created.push(channel);
    return channel;
  }

  async createOffer() {
    return { type: "offer", sdp: CANARIES.sdp };
  }

  async createAnswer() {
    return { type: "answer", sdp: CANARIES.sdp };
  }

  async setLocalDescription(description) {
    this.localDescription = description;
  }

  async setRemoteDescription() {}

  async addIceCandidate(init) {
    this.addedCandidates.push(init);
  }

  async getStats() {
    this.statsCalls += 1;
    return canaryReport();
  }

  close() {}

  moveTo(state) {
    this.connectionState = state;
    this.dispatchEvent(evt("connectionstatechange"));
  }

  moveIce(state) {
    this.iceConnectionState = state;
    this.dispatchEvent(evt("iceconnectionstatechange"));
  }

  handOverChannel(channel) {
    this.dispatchEvent(evt("datachannel", { channel }));
  }

  emitCandidate(candidate) {
    this.dispatchEvent(evt("icecandidate", { candidate }));
  }
}

function actorHarness({ drainTimeoutMs } = {}) {
  const failed = [];
  let pc = null;
  const actor = createAttemptRtc({
    role: "answerer",
    transferId: TID,
    attemptId: AID,
    iceServers: [CANARIES.stunUrl],
    ...(drainTimeoutMs === undefined ? {} : { drainTimeoutMs }),
    sendSignal: () => true,
    createPeerConnection: () => {
      pc = new FakePc();
      return pc;
    },
    events: { onFailed: (reason) => failed.push(reason) },
  });
  return { actor, failed, pc: () => pc };
}

/** Lets the asynchronous `getStats()` samples land in the trace. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 10));

/** Drives one answerer attempt to a live channel. */
async function liveAttempt(h) {
  await h.actor.start();
  const channel = new FakeChannel(CHANNEL_LABEL, { protocol: CHANNEL_PROTOCOL });
  h.pc().handOverChannel(channel);
  await h.actor.handleSignal("rtc.offer", { sdp: CANARIES.sdp });
  channel.becomeOpen();
  return channel;
}

test("web-transfer direct diagnostics", async (t) => {
  await t.test(
    "direct_diagnostics_distinguish_ice_channel_and_drain_without_private_addresses",
    async () => {
      // --- 1. the ICE path gave up -------------------------------------
      const ice = actorHarness();
      const iceChannel = await liveAttempt(ice);
      ice.pc().emitCandidate({ candidate: CANARIES.candidateLine, sdpMid: "0" });
      await ice.actor.handleSignal("rtc.ice", { candidate: CANARIES.remoteCandidateLine });
      ice.pc().moveIce("checking");
      ice.pc().moveTo("connected");
      ice.pc().moveIce("failed");
      await settle();
      const iceTrace = ice.actor.diagnostics();
      assert.deepEqual(ice.failed, ["ice-failed"]);
      assert.equal(iceTrace.reason, "ice-failed");
      assert.equal(iceTrace.transferId, TID);
      assert.equal(iceTrace.attemptId, AID);
      const iceEvents = iceTrace.events.map((e) => e.ev);
      assert.ok(iceEvents.includes("ice"), "the ICE timeline is in the trace");
      assert.ok(iceEvents.includes("channel-open"));
      assert.equal(iceTrace.drain.timeouts, 0, "the queue was not the cause");
      assert.equal(iceChannel.readyState, "closed");
      // The candidate TYPES survive; nothing else about a candidate does.
      assert.deepEqual(iceTrace.candidates.local, { srflx: 1 });
      assert.deepEqual(iceTrace.candidates.remote, { host: 1 });
      // And the stats say WHICH kind of pair carried it — the question the
      // field case could not answer.
      const sample = iceTrace.stats.at(-1);
      assert.equal(sample.pair.localType, "srflx");
      assert.equal(sample.pair.remoteType, "host");
      assert.equal(sample.pair.rttMs, 23.4);
      assert.equal(sample.sctp.cwnd, 262_144);
      assert.equal(sample.channel.messagesSent, 512);

      // --- 2. the channel closed under a healthy ICE path ---------------
      const closed = actorHarness();
      const channel = await liveAttempt(closed);
      channel.close();
      await settle();
      const closedTrace = closed.actor.diagnostics();
      assert.deepEqual(closed.failed, ["channel-closed"]);
      assert.equal(closedTrace.reason, "channel-closed");
      assert.ok(closedTrace.events.some((e) => e.ev === "channel-close"));
      assert.ok(
        !closedTrace.events.some((e) => e.ev === "ice" && e.state === "failed"),
        "a channel failure is not reported as an ICE failure",
      );
      assert.equal(closedTrace.drain.timeouts, 0);

      // --- 3. the queue stopped draining -------------------------------
      const stalled = actorHarness({ drainTimeoutMs: 30 });
      const stuck = await liveAttempt(stalled);
      stuck.bufferedAmount = RTC_HIGH_WATER + 1;
      const outcome = await stalled.actor.sink
        .waitLow(new AbortController().signal)
        .then(() => "resolved", (error) => error.name);
      await settle();
      const drainTrace = stalled.actor.diagnostics();
      assert.equal(outcome, "AbortError");
      assert.deepEqual(stalled.failed, ["timeout"]);
      assert.equal(drainTrace.reason, "timeout");
      assert.equal(drainTrace.drain.timeouts, 1);
      assert.ok(drainTrace.drain.longestMs >= 30);
      assert.ok(drainTrace.drain.peakQueued >= RTC_HIGH_WATER);
      assert.ok(drainTrace.events.some((e) => e.ev === "drain-timeout"));
      // The three causes are distinguishable by the reason AND by what the
      // trace holds: only this one blames the queue.
      assert.deepEqual(
        [iceTrace.reason, closedTrace.reason, drainTrace.reason],
        ["ice-failed", "channel-closed", "timeout"],
      );

      // --- the privacy canary ------------------------------------------
      const store = createTraceStore();
      for (const trace of [iceTrace, closedTrace, drainTrace]) {
        store.push(trace);
      }
      const report = store.text({ note: "copied by the user" });
      for (const [what, canary] of Object.entries(CANARIES)) {
        assert.equal(
          report.includes(canary),
          false,
          `${what} must never reach the diagnostic report`,
        );
      }
      // Belt and braces: no dotted quad and no `candidate:` line at all.
      assert.equal(/\b\d{1,3}(\.\d{1,3}){3}\b/.test(report), false, "no IPv4 literal");
      assert.equal(report.includes("candidate:"), false, "no candidate line");
      assert.equal(report.includes("v=0"), false, "no SDP");
      // And the report is still USEFUL: a gate that only proves absence
      // would pass on a diagnostic that says nothing.
      assert.ok(report.includes("\"reason\": \"ice-failed\""));
      assert.ok(report.includes("\"localType\": \"srflx\""));
      assert.ok(report.includes("\"drain\""));
    },
  );

  await t.test("summarize_stats_copies_only_named_numeric_and_enum_fields", () => {
    const summary = summarizeStats(canaryReport());
    const json = JSON.stringify(summary);
    for (const canary of Object.values(CANARIES)) {
      assert.equal(json.includes(canary), false, `${canary} must not survive`);
    }
    assert.deepEqual(summary.pair.localType, "srflx");
    assert.deepEqual(summary.pair.remoteType, "host");
    assert.equal(summary.pair.protocol, "udp");
    assert.equal(summary.pair.bytesSent, 4_194_304);
    assert.equal(summary.transport.pairChanges, 2);
    assert.equal(summary.sctp.mtu, 1_200);
    assert.equal(summary.channel.state, "open");
    // A report with no selected pair says what it can and nothing more.
    const bare = summarizeStats(
      new Map([["X", { id: "X", type: "candidate-pair", state: "failed" }]]),
    );
    assert.equal(bare, null);
    assert.equal(summarizeStats(null), null);
    assert.equal(summarizeStats(undefined), null);
  });

  await t.test("trace_and_store_are_bounded", () => {
    const trace = createAttemptTrace({ transferId: TID, attemptId: AID, role: "offerer" });
    for (let i = 0; i < MAX_TRACE_EVENTS * 3; i += 1) {
      trace.mark("tick", { i });
    }
    for (let i = 0; i < MAX_STATS_SAMPLES * 3; i += 1) {
      trace.stats({ pair: { bytesSent: i } });
    }
    const snapshot = trace.snapshot();
    assert.equal(snapshot.events.length, MAX_TRACE_EVENTS);
    assert.equal(snapshot.droppedEvents, MAX_TRACE_EVENTS * 3 - MAX_TRACE_EVENTS);
    assert.equal(snapshot.stats.length, MAX_STATS_SAMPLES);
    // The FIRST sample is kept: a trace whose oldest sample was evicted
    // cannot say what the path looked like while it still worked.
    assert.equal(snapshot.stats[0].pair.bytesSent, 0);
    assert.equal(snapshot.stats.at(-1).pair.bytesSent, MAX_STATS_SAMPLES * 3 - 1);

    // The elapsed time FREEZES when the attempt ends: the report is built
    // when the user presses the button, which can be minutes later, and an
    // elapsed measured then describes the reader and not the attempt. It
    // reported a 1.3-second transfer as 18 seconds the first time the perf
    // harness read it.
    let now = 1000;
    const timed = createAttemptTrace({
      transferId: TID,
      attemptId: AID,
      role: "offerer",
      now: () => now,
    });
    now = 1200;
    timed.mark("ready");
    now = 2500;
    timed.mark("fail", { reason: "timeout" });
    now = 99_000;
    const frozen = timed.snapshot();
    assert.equal(frozen.elapsedMs, 1500);
    assert.equal(frozen.live, false);
    const running = createAttemptTrace({
      transferId: TID,
      attemptId: AID,
      role: "offerer",
      now: () => now,
    });
    now += 250;
    assert.equal(running.snapshot().elapsedMs, 250);
    assert.equal(running.snapshot().live, true);

    const store = createTraceStore();
    for (let i = 0; i < MAX_TRACED_ATTEMPTS * 2; i += 1) {
      store.push({ attemptId: `a${i}` });
    }
    assert.equal(store.size, MAX_TRACED_ATTEMPTS);
    assert.equal(store.all()[0].attemptId, `a${MAX_TRACED_ATTEMPTS}`);
    // A lazily read entry is resolved when the report is built, and one that
    // throws costs its own row and never the report.
    const lazy = createTraceStore();
    lazy.push(() => ({ attemptId: "lazy" }));
    lazy.push(() => {
      throw new Error("gone");
    });
    assert.deepEqual(lazy.all(), [{ attemptId: "lazy" }]);
  });

  await t.test("redaction_helpers_refuse_anything_but_numbers_and_short_enums", () => {
    assert.equal(enumValue("host"), "host");
    assert.equal(enumValue("in-progress"), "in-progress");
    assert.equal(enumValue("192.0.2.1"), null);
    assert.equal(enumValue("candidate:1 1 udp 1 192.0.2.1 1 typ host"), null);
    assert.equal(enumValue("Mario Rossi"), null);
    assert.equal(enumValue("A".repeat(64)), null);
    assert.equal(enumValue(42), null);
    assert.equal(numberValue(1.2345, 2), 1.23);
    assert.equal(numberValue(Number.NaN), null);
    assert.equal(numberValue(Number.POSITIVE_INFINITY), null);
    assert.equal(numberValue("7"), null);
    assert.equal(candidateTypeOf("candidate:1 1 udp 1 192.0.2.1 1 typ srflx raddr 10.0.0.1"), "srflx");
    assert.equal(candidateTypeOf("candidate:1 1 udp 1 192.0.2.1 1 typ relay"), "relay");
    assert.equal(candidateTypeOf("candidate:1 1 udp 1 192.0.2.1 1 typ nonsense"), "other");
    assert.equal(candidateTypeOf(null), "other");
    // A trace field that is neither a number nor a short enum is DROPPED,
    // which is what keeps a future caller from widening the report.
    const trace = createAttemptTrace({ transferId: TID, attemptId: AID, role: "offerer" });
    trace.mark("x", { ok: 3, state: "connected", leak: CANARIES.localAddress, flag: true });
    const [event] = trace.snapshot().events;
    assert.equal(event.ok, 3);
    assert.equal(event.state, "connected");
    assert.equal(event.flag, true);
    assert.equal("leak" in event, false);
  });
});
