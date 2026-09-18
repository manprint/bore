// Per-attempt WebRTC actor (Phase 4.2): one `RTCPeerConnection` and one
// DataChannel for exactly one TransferId/AttemptId pair, built only after
// the server's `transfer.direct_start` and torn down with it.
//
// Roles are FIXED by the server and never derived here: the recipient is
// always the SDP offerer and the only side that calls `createDataChannel`;
// the source is always the answerer and takes the one channel it is given.
// Everything this module puts on the control channel is a signalling body
// the server forwards verbatim to the counterpart — it is never parsed,
// stored or logged by the server, and SDP and candidates never reach the
// console, the UI or an error string here either.

import { perfFragmentBytes, perfWaterMarks } from "./perf.js";
import { STATS_POLL_MS, createAttemptTrace, summarizeStats } from "./diagnostics.js";

/** Channel label and subprotocol, both fixed by the plan. */
export const CHANNEL_LABEL = "bore-transfer-v1";
export const CHANNEL_PROTOCOL = "bore-transfer-v1";
/** Remote candidates held before the remote description lands. */
export const MAX_REMOTE_CANDIDATES = 128;
/** A channel that cannot carry this many plaintext bytes is unusable. */
export const MIN_MESSAGE_BYTES = 1024;
/** Header + AEAD tag reserved inside one SCTP message. */
export const FRAGMENT_HEADROOM = 64;
/** Largest plaintext fragment, the same value the relay path uses. */
export const MAX_FRAGMENT_BYTES = 24 * 1024;
/**
 * Above this queued ciphertext the sender stops reading new slices — PER
 * CARRIER, and MEASURED rather than chosen (2026-09-18, two hosts, 21 ms
 * RTT, 128 MiB, five repetitions per depth, `scripts/perf/web_transfer_wan.sh`):
 *
 *   per carrier   median MiB/s   failures in 5   peak queued
 *   2 MiB         18.76          1 fallback      2.36 MB
 *   1 MiB         20.19          1 sctp-failure  1.32 MB
 *   512 KiB       21.45          0               0.80 MB
 *
 * So the deepest queue is the SLOWEST and the least stable, exactly as the
 * native uplink's own depth ladder concluded (V-13: "deeper is better" is
 * false and there is an optimum). At the previous 4 MiB the direct path did
 * not merely slow down, it DIED: `RTCError` `errorDetail: "sctp-failure"`,
 * `sctpCauseCode: 12`, after which the transfer finished on the relay — the
 * fallback worked, but a path that aborts mid-transfer is not a path.
 */
export const RTC_HIGH_WATER = 512 * 1024;
/**
 * Carriers one attempt may run on: the same ceiling the server's signalling
 * enforces, restated here so a malformed `carriers` in `transfer.direct_start`
 * cannot make the page open an unbounded number of peer connections.
 */
export const MAX_CARRIERS = 8;
/** Reading resumes once the channel has drained below this (a quarter of the
 * high mark, the ratio the ladder above was measured with). */
export const RTC_LOW_WATER = 128 * 1024;
/** A drain that never happens must not park the pipeline forever. */
export const DRAIN_TIMEOUT_MS = 10_000;
/** A `disconnected` that heals inside this window is not a failure. */
export const DISCONNECT_GRACE_MS = 2_000;
/** Largest ciphertext message accepted off the channel. */
export const MAX_INBOUND_BYTES = 64 * 1024;

/**
 * Keeps only the STUN entries of a server-provided ICE list and drops every
 * credential with them. The server already validates the list, so this is
 * defence in depth and the one place a TURN URL could enter the page — the
 * plan forbids TURN, and a relay server would see the connection metadata
 * this feature exists to avoid handing to anyone.
 * @param {unknown} servers the `iceServers` array from `transfer.direct_start`
 * @returns {{urls: string}[]} an `RTCConfiguration.iceServers` value
 */
export function filterIceServers(servers) {
  const out = [];
  if (!Array.isArray(servers)) {
    return out;
  }
  for (const entry of servers) {
    const url =
      typeof entry === "string"
        ? entry
        : entry !== null && typeof entry === "object" && typeof entry.urls === "string"
          ? entry.urls
          : null;
    if (url === null || !/^stuns?:[^\s]+$/i.test(url)) {
      continue;
    }
    out.push({ urls: url });
  }
  return out;
}

/**
 * Plaintext fragment size for one channel: never above the relay's 24 KiB
 * (the manifest and the receive path are written for it) and never above
 * what the peer said it can carry, with room for the frame header and the
 * AEAD tag. Below the floor the channel is unusable, which is a failure and
 * not a smaller fragment.
 * @param {number|undefined|null} maxMessageSize `pc.sctp.maxMessageSize`
 * @returns {number|null} the fragment size, or `null` when unusable
 */
export function fragmentBytesFor(maxMessageSize) {
  if (typeof maxMessageSize !== "number" || !Number.isFinite(maxMessageSize)) {
    // An engine that does not publish the limit gets the conservative value;
    // every engine this ships on is far above it.
    return MAX_FRAGMENT_BYTES;
  }
  if (maxMessageSize < MIN_MESSAGE_BYTES + FRAGMENT_HEADROOM) {
    return null;
  }
  return Math.min(MAX_FRAGMENT_BYTES, Math.floor(maxMessageSize) - FRAGMENT_HEADROOM);
}

function isOfferer(role) {
  return role === "offerer";
}

/**
 * One direct attempt.
 *
 * @param {object} options
 * @param {"offerer"|"answerer"} options.role the server's fixed role
 * @param {string} options.transferId
 * @param {string} options.attemptId
 * @param {unknown} options.iceServers the `transfer.direct_start` list
 * @param {(type: string, body: object) => boolean} options.sendSignal puts one
 * signalling message on the control channel
 * @param {(config: object) => RTCPeerConnection} [options.createPeerConnection]
 * injected for tests; defaults to the global constructor
 * @param {number} [options.drainTimeoutMs] the drain deadline, injected by
 * tests so the deadline can be REACHED inside a test instead of waited out;
 * production always uses `DRAIN_TIMEOUT_MS`
 * @param {object} options.events `{ onReady({fragmentBytes}), onMessage(bytes),
 * onFailed(reason), onClosed() }` — all optional
 */
export function createAttemptRtc({
  role,
  transferId,
  attemptId,
  iceServers,
  sendSignal,
  createPeerConnection,
  drainTimeoutMs = DRAIN_TIMEOUT_MS,
  carrier = 0,
  events = {},
}) {
  const offerer = isOfferer(role);
  /** Set the moment a terminal decision is taken; makes every exit idempotent. */
  let done = false;
  let ready = false;
  let pc = null;
  let channel = null;
  let fragmentBytes = null;
  // Read ONCE per attempt: a mark that moved under a live channel would make
  // the pipeline and the engine's own `bufferedamountlow` threshold disagree.
  const marks = perfWaterMarks();
  const highWater = marks?.high ?? RTC_HIGH_WATER;
  const lowWater = marks?.low ?? RTC_LOW_WATER;
  let remoteDescriptionSet = false;
  let localSignalSent = false;
  let remoteSignalSeen = false;
  let disconnectTimer = null;
  let statsTimer = null;
  /** Remote candidates that arrived before the remote description. */
  const pendingCandidates = [];
  /**
   * The peer's end-of-candidates marker, held when it arrives BEFORE the
   * remote description — at most one, and applied after the queue drains.
   */
  let pendingEndOfCandidates = false;
  let endOfCandidatesApplied = false;
  /** Everything registered on `pc`/`channel`, removed exactly once. */
  const listeners = [];
  const waiters = new Set();
  /**
   * The attempt's own bounded, redacted trace (V003-C3). It is what answers
   * "why did this attempt end", and it is built here because this is the
   * only place that sees the ICE states, the channel events and the drain
   * waits in one timeline.
   */
  const trace = createAttemptTrace({ role, transferId, attemptId });

  function on(target, type, handler) {
    target.addEventListener(type, handler);
    listeners.push([target, type, handler]);
  }

  function clearDisconnectTimer() {
    if (disconnectTimer !== null) {
      clearTimeout(disconnectTimer);
      disconnectTimer = null;
    }
  }

  /**
   * Samples `getStats()` into the trace. It runs on a timer rather than at
   * the moment of failure because `getStats()` is ASYNC and a failure is
   * reported from synchronous event handlers that must tear the attempt down
   * at once: a sample taken a second earlier is evidence, a sample awaited
   * inside `fail()` is a teardown that races the next attempt. The final
   * best-effort sample below is issued BEFORE `close()` for the same reason.
   */
  function sampleStats() {
    if (pc === null) {
      return;
    }
    let pending;
    try {
      pending = pc.getStats();
    } catch {
      return;
    }
    if (pending === null || pending === undefined || typeof pending.then !== "function") {
      return;
    }
    pending.then(
      (report) => trace.stats(summarizeStats(report)),
      () => {
        /* an engine that refuses stats costs the trace one sample */
      },
    );
  }

  function startStatsPolling() {
    if (statsTimer !== null || pc === null || typeof globalThis.setInterval !== "function") {
      return;
    }
    statsTimer = globalThis.setInterval(sampleStats, STATS_POLL_MS);
    if (typeof statsTimer?.unref === "function") {
      statsTimer.unref();
    }
  }

  function stopStatsPolling() {
    if (statsTimer !== null) {
      globalThis.clearInterval(statsTimer);
      statsTimer = null;
    }
  }

  function releaseWaiters() {
    for (const resolve of [...waiters]) {
      waiters.delete(resolve);
      resolve();
    }
  }

  /** Idempotent teardown: listeners, channel, peer connection, timers. */
  function close() {
    clearDisconnectTimer();
    // One last sample while the connection still exists: `getStats()` on a
    // closed `RTCPeerConnection` answers nothing useful, and this is the
    // sample that describes the path AT the failure.
    sampleStats();
    stopStatsPolling();
    while (listeners.length > 0) {
      const [target, type, handler] = listeners.pop();
      try {
        target.removeEventListener(type, handler);
      } catch {
        /* a target already gone has nothing to remove */
      }
    }
    releaseWaiters();
    if (channel !== null) {
      try {
        channel.close();
      } catch {
        /* closing a closed channel is not an error here */
      }
      channel = null;
    }
    if (pc !== null) {
      try {
        pc.close();
      } catch {
        /* same */
      }
      pc = null;
    }
  }

  /**
   * Reports the attempt as unusable exactly once, with a FIXED reason code —
   * never an engine's own message, which would put SDP or a local path in a
   * string the server forwards.
   */
  function fail(reason) {
    if (done) {
      return;
    }
    done = true;
    trace.mark("fail", { reason });
    trace.failed(reason);
    close();
    events.onFailed?.(reason);
  }

  function declareReady() {
    if (done || ready || channel === null || pc === null) {
      return;
    }
    if (channel.readyState !== "open") {
      return;
    }
    if (pc.connectionState === "failed" || pc.connectionState === "closed") {
      return;
    }
    // The side that has not yet completed its own half of the SDP exchange
    // cannot be ready: the server refuses such a `transfer.direct_ready`, and
    // sending one anyway would burn the attempt.
    if (!localSignalSent || !remoteSignalSeen) {
      return;
    }
    if (
      channel.label !== CHANNEL_LABEL ||
      (channel.protocol !== "" && channel.protocol !== CHANNEL_PROTOCOL) ||
      channel.ordered === false
    ) {
      fail("protocol");
      return;
    }
    const derived = fragmentBytesFor(pc.sctp?.maxMessageSize);
    if (derived === null) {
      fail("unsupported");
      return;
    }
    // The peer's number is the CEILING and stays so: the harness may ask for a
    // SMALLER fragment to measure the per-message cost of SCTP, never a larger
    // one (4.6, catalogue entry 2).
    const wanted = perfFragmentBytes();
    const size = wanted === null ? derived : Math.min(derived, wanted);
    fragmentBytes = size;
    ready = true;
    trace.mark("ready", { fragmentBytes: size });
    // The first sample is taken HERE, on a healthy channel: a trace whose
    // only sample is the one at the failure cannot say what changed.
    sampleStats();
    events.onReady?.({ fragmentBytes: size });
  }

  function adoptChannel(next) {
    if (channel !== null && channel !== next) {
      // A second DataChannel on this connection is not part of the contract:
      // one transfer is one channel, and an extra one is either a bug or a
      // peer doing something the protocol does not describe.
      fail("protocol");
      return;
    }
    channel = next;
    try {
      channel.binaryType = "arraybuffer";
    } catch {
      /* an engine that refuses the assignment still delivers ArrayBuffer */
    }
    channel.bufferedAmountLowThreshold = lowWater;
    on(channel, "open", () => {
      trace.mark("channel-open");
      declareReady();
    });
    on(channel, "bufferedamountlow", () => releaseWaiters());
    on(channel, "close", () => {
      trace.mark("channel-close", { ready });
      if (ready) {
        fail("channel-closed");
      } else {
        fail("channel-closed");
      }
    });
    on(channel, "error", (event) => {
      // The DETAIL is the whole diagnostic value of this event. A bare
      // "channel-error" mark says a direct path died and nothing about why,
      // and on a real WAN that is the difference between "the peer went
      // away" and "we overran the SCTP send queue" — opposite remedies.
      // `RTCErrorEvent.error` is an `RTCError`: `errorDetail` is the
      // enumerated cause (`sctp-failure`, `data-channel-failure`, …),
      // `sctpCauseCode` the protocol code when there is one. Every read is
      // defensive: this runs on three engines and the shape is not uniform.
      // NO free-form message: `enumValue` would drop it anyway, and that
      // filter is the reason a trace can be handed to a user. `errorDetail`
      // is already an enumerated token and `sctpCauseCode` a number.
      const error = event?.error;
      trace.mark("channel-error", {
        detail: error?.errorDetail ?? null,
        sctp: error?.sctpCauseCode ?? null,
      });
      fail("channel-closed");
    });
    on(channel, "message", (event) => {
      const data = event.data;
      if (!(data instanceof ArrayBuffer)) {
        // Text and Blob are both outside the contract: the frame decoder
        // needs bytes it can address synchronously, and a peer sending
        // anything else is not speaking this protocol.
        fail("protocol");
        return;
      }
      if (data.byteLength > MAX_INBOUND_BYTES) {
        fail("protocol");
        return;
      }
      events.onMessage?.(data);
    });
    if (channel.readyState === "open") {
      declareReady();
    }
  }

  function signalBody(extra) {
    // Carrier 0 is the only carrier a single-carrier attempt has, and the
    // server omits it in the same way when it forwards: the wire of an
    // attempt that asked for one carrier is the wire that existed before
    // carriers, field for field.
    return carrier === 0
      ? { transferId, attemptId, ...extra }
      : { transferId, attemptId, carrier, ...extra };
  }

  function emitCandidate(candidate) {
    if (done) {
      return;
    }
    if (candidate === null) {
      // One wire shape for "no more candidates": the server normalises the
      // absent/null/empty forms into this one.
      sendSignal("rtc.ice", signalBody({ candidate: null }));
      return;
    }
    const body = { candidate: candidate.candidate };
    if (typeof candidate.sdpMid === "string") {
      body.sdpMid = candidate.sdpMid;
    }
    if (typeof candidate.sdpMLineIndex === "number") {
      body.sdpMLineIndex = candidate.sdpMLineIndex;
    }
    sendSignal("rtc.ice", signalBody(body));
  }

  function watchConnection() {
    on(pc, "icecandidate", (event) => {
      const candidate = event.candidate;
      if (candidate === null || candidate.candidate === "") {
        trace.mark("local-candidates-done");
        emitCandidate(null);
        return;
      }
      // Counted by TYPE only, here and in the trace: the line itself is
      // forwarded to the peer and never kept.
      trace.candidate("local", candidate.candidate);
      emitCandidate(candidate);
    });
    on(pc, "icegatheringstatechange", () => {
      if (pc === null) {
        return;
      }
      trace.mark("gathering", { state: pc.iceGatheringState });
    });
    on(pc, "iceconnectionstatechange", () => {
      if (pc === null) {
        return;
      }
      trace.mark("ice", { state: pc.iceConnectionState });
      if (pc.iceConnectionState === "failed") {
        fail("ice-failed");
      }
    });
    on(pc, "connectionstatechange", () => {
      if (pc === null || done) {
        return;
      }
      const state = pc.connectionState;
      trace.mark("conn", { state });
      if (state === "failed") {
        fail("ice-failed");
        return;
      }
      if (state === "closed") {
        fail("channel-closed");
        return;
      }
      if (state === "disconnected") {
        // A transient `disconnected` is common on a healthy path; only one
        // that persists is a failure.
        if (disconnectTimer === null) {
          disconnectTimer = setTimeout(() => {
            disconnectTimer = null;
            if (pc !== null && pc.connectionState === "disconnected") {
              trace.mark("disconnect-grace-expired");
              fail("ice-failed");
            }
          }, DISCONNECT_GRACE_MS);
          if (typeof disconnectTimer?.unref === "function") {
            disconnectTimer.unref();
          }
        }
        return;
      }
      clearDisconnectTimer();
      declareReady();
    });
    if (!offerer) {
      on(pc, "datachannel", (event) => adoptChannel(event.channel));
    }
  }

  /**
   * Hands the peer's END-OF-CANDIDATES indication to the ICE agent, exactly
   * once and only after every candidate that preceded it.
   *
   * [WebRTC 1.0](https://www.w3.org/TR/webrtc/) makes `addIceCandidate(null)`
   * the way an end-of-candidates indication reaches the agent, for all media
   * descriptions. This used to be DROPPED here, on the argument that an
   * engine can read the end from its own gathering state — but its own
   * gathering state is about the LOCAL candidates, and the remote agent has
   * no other way to learn that the peer is finished. Without it an agent can
   * keep a checklist alive waiting for candidates that will never come,
   * which delays the moment `failed` is reported on a path that cannot work
   * (V003-F05).
   */
  async function applyEndOfCandidates() {
    if (endOfCandidatesApplied || pc === null) {
      return;
    }
    endOfCandidatesApplied = true;
    pendingEndOfCandidates = false;
    trace.mark("remote-candidates-done");
    try {
      await pc.addIceCandidate(null);
    } catch {
      // An engine that refuses the marker loses one hint, never the attempt.
    }
  }

  async function flushCandidates() {
    while (pendingCandidates.length > 0) {
      const init = pendingCandidates.shift();
      try {
        await pc.addIceCandidate(init);
      } catch {
        // A candidate the engine refuses is one path lost, never the attempt:
        // ICE decides on the ones that work.
      }
    }
    if (pendingEndOfCandidates) {
      // The marker means "nothing after this one", so it is applied after
      // the queue and never in the middle of it.
      await applyEndOfCandidates();
    }
  }

  async function start() {
    if (done) {
      return;
    }
    const factory =
      createPeerConnection ??
      ((config) => new globalThis.RTCPeerConnection(config));
    try {
      pc = factory({ iceServers: filterIceServers(iceServers) });
    } catch {
      fail("unsupported");
      return;
    }
    if (pc === null || pc === undefined) {
      fail("unsupported");
      return;
    }
    watchConnection();
    trace.mark("start");
    startStatsPolling();
    if (!offerer) {
      // The answerer builds nothing until the offer lands.
      return;
    }
    try {
      adoptChannel(
        pc.createDataChannel(CHANNEL_LABEL, {
          ordered: true,
          protocol: CHANNEL_PROTOCOL,
        }),
      );
      if (done) {
        return;
      }
      const offer = await pc.createOffer();
      if (done) {
        return;
      }
      await pc.setLocalDescription(offer);
      if (done) {
        return;
      }
      localSignalSent = true;
      sendSignal("rtc.offer", signalBody({ sdp: pc.localDescription.sdp }));
    } catch {
      fail("protocol");
    }
  }

  async function handleSignal(type, body) {
    if (done || pc === null) {
      return;
    }
    try {
      if (type === "rtc.offer") {
        if (offerer || remoteDescriptionSet) {
          // An offerer never receives an offer, and a second one would
          // replace a description the channel is already built on.
          fail("protocol");
          return;
        }
        if (typeof body?.sdp !== "string") {
          fail("protocol");
          return;
        }
        await pc.setRemoteDescription({ type: "offer", sdp: body.sdp });
        remoteDescriptionSet = true;
        remoteSignalSeen = true;
        await flushCandidates();
        const answer = await pc.createAnswer();
        if (done) {
          return;
        }
        await pc.setLocalDescription(answer);
        if (done) {
          return;
        }
        localSignalSent = true;
        sendSignal("rtc.answer", signalBody({ sdp: pc.localDescription.sdp }));
        declareReady();
        return;
      }
      if (type === "rtc.answer") {
        if (!offerer || remoteDescriptionSet) {
          fail("protocol");
          return;
        }
        if (typeof body?.sdp !== "string") {
          fail("protocol");
          return;
        }
        await pc.setRemoteDescription({ type: "answer", sdp: body.sdp });
        remoteDescriptionSet = true;
        remoteSignalSeen = true;
        await flushCandidates();
        declareReady();
        return;
      }
      if (type === "rtc.ice") {
        const candidate = body?.candidate;
        if (candidate === null || candidate === undefined) {
          // End of the peer's candidates. It reaches the ICE agent, but only
          // after everything that arrived before it: an early marker waits in
          // ONE bounded slot (a second one is the same marker, not a second
          // queue entry) and `flushCandidates` applies it.
          if (!remoteDescriptionSet) {
            pendingEndOfCandidates = true;
            return;
          }
          await applyEndOfCandidates();
          return;
        }
        if (typeof candidate !== "string") {
          fail("protocol");
          return;
        }
        trace.candidate("remote", candidate);
        const init = { candidate };
        if (typeof body.sdpMid === "string") {
          init.sdpMid = body.sdpMid;
        }
        if (typeof body.sdpMLineIndex === "number") {
          init.sdpMLineIndex = body.sdpMLineIndex;
        }
        if (!remoteDescriptionSet) {
          if (pendingCandidates.length >= MAX_REMOTE_CANDIDATES) {
            // The peer's budget is the same 128 the server enforces; a queue
            // past it is a peer that is not playing by the protocol.
            fail("protocol");
            return;
          }
          pendingCandidates.push(init);
          return;
        }
        try {
          await pc.addIceCandidate(init);
        } catch {
          /* one lost path, never the attempt */
        }
      }
    } catch {
      fail("protocol");
    }
  }

  /**
   * The send half of the channel, in the shape both transports expose:
   * `send` is synchronous, `waitLow` is what the pipeline awaits before it
   * reads the next slice, and `close` is idempotent.
   */
  const sink = {
    get fragmentBytes() {
      return fragmentBytes ?? MAX_FRAGMENT_BYTES;
    },
    get bufferedAmount() {
      return channel?.bufferedAmount ?? 0;
    },
    get highWater() {
      return highWater;
    },
    send(bytes) {
      // A channel that dies under the pipeline ends the ATTEMPT, never the
      // transfer. The engine reports it two ways and the slower one is the
      // event (`close` arrives after this call, and a transient
      // disconnection is held for `DISCONNECT_GRACE_MS` before it counts),
      // so the write itself must be what reports it: `send-error` once, and
      // an `AbortError` for the pipeline, which the sender reads as "this
      // attempt was abandoned" and answers by waiting for the relay attempt.
      // Throwing a plain error here instead FAILED the whole transfer and
      // the relay ticket then landed on a record that no longer existed.
      if (channel === null || channel.readyState !== "open") {
        fail("send-error");
        throw new DOMException("direct channel is not open", "AbortError");
      }
      try {
        channel.send(bytes);
      } catch {
        fail("send-error");
        throw new DOMException("direct channel send failed", "AbortError");
      }
    },
    /**
     * Waits for the channel to drain below the low-water mark. Awaiting
     * `bufferedamountlow` (instead of queueing past it) is the browser half
     * of the rule the native uplink already follows: a deep queue buys
     * throughput and pays latency, and here the queue lives in the tab's own
     * heap.
     */
    async waitLow(signal) {
      if (channel === null || channel.bufferedAmount <= highWater) {
        return;
      }
      const startedAt = Date.now();
      const queued = channel.bufferedAmount;
      await new Promise((resolve, reject) => {
        let settled = false;
        const finish = () => {
          if (settled) {
            return;
          }
          settled = true;
          waiters.delete(finish);
          clearTimeout(timer);
          signal?.removeEventListener("abort", onAbort);
          trace.drained(Date.now() - startedAt, queued);
          resolve();
        };
        const onAbort = () => {
          if (settled) {
            return;
          }
          settled = true;
          waiters.delete(finish);
          clearTimeout(timer);
          reject(new DOMException("aborted", "AbortError"));
        };
        /**
         * The deadline used to RESOLVE, on the argument that "the next send
         * decides". It does not: an open channel accepts the next 1 MiB
         * chunk, so a path that has stopped draining was handed MORE bytes
         * every ten seconds and the queue — which lives in the tab's own
         * heap — grew without bound while the transfer looked alive
         * (V003-F02). A queue that has not drained inside the deadline is a
         * dead direct path: it ends the ATTEMPT with the fixed reason the
         * wire already has, and the relay attempt resumes from the
         * recipient's verified ranges exactly as it does for a channel that
         * closed. Resolving is kept for the one case it is right for: the
         * queue really is below the mark and only the engine's event was
         * missed.
         */
        const onDeadline = () => {
          if (settled) {
            return;
          }
          if (channel !== null && channel.bufferedAmount <= highWater) {
            finish();
            return;
          }
          settled = true;
          waiters.delete(finish);
          signal?.removeEventListener("abort", onAbort);
          const waited = Date.now() - startedAt;
          trace.drainTimedOut(waited, channel?.bufferedAmount ?? queued);
          trace.mark("drain-timeout", {
            waitedMs: waited,
            queued: channel?.bufferedAmount ?? queued,
          });
          reject(new DOMException("direct channel did not drain", "AbortError"));
          fail("timeout");
        };
        if (signal?.aborted) {
          onAbort();
          return;
        }
        const timer = setTimeout(onDeadline, drainTimeoutMs);
        if (typeof timer?.unref === "function") {
          timer.unref();
        }
        waiters.add(finish);
        signal?.addEventListener("abort", onAbort, { once: true });
      });
    },
    close() {
      close();
    },
  };

  return {
    start,
    handleSignal,
    close() {
      if (done) {
        return;
      }
      done = true;
      trace.mark("closed");
      close();
      events.onClosed?.();
    },
    sink,
    /**
     * The attempt's bounded, redacted trace (V003-C3). The page shows it on
     * an explicit user gesture and nowhere else; nothing here is sent to the
     * server, and by construction it holds no address, candidate line, SDP,
     * name or secret — see `diagnostics.js`.
     */
    diagnostics() {
      return { carrier, ...trace.snapshot() };
    },
    /** Introspection for tests only; the app reads `events`. */
    state() {
      return {
        role,
        carrier,
        ready,
        done,
        fragmentBytes,
        pendingCandidates: pendingCandidates.length,
        channelState: channel?.readyState ?? null,
      };
    },
  };
}

/**
 * One direct attempt carried by N `RTCPeerConnection`s.
 *
 * The direct path's throughput is bound PER SCTP ASSOCIATION — the send
 * buffer divided by the round-trip time — and every DataChannel of one
 * `RTCPeerConnection` shares that association's window. MEASURED between two
 * hosts 31 ms apart: one association 5.38 MiB/s, two 10.57, four 41.42, with
 * the receiving host's CPU idle throughout and the sender spending 84 % of
 * the transfer parked in `waitLow`. So carriers are separate peer
 * connections, and more channels on one connection would buy nothing.
 *
 * The group is a thin multiplexer over the single-carrier actor above, which
 * is unchanged: each carrier negotiates its own offer/answer/candidates under
 * its own index, and this object decides which one each frame goes to. The
 * frame stream stays ONE stream — the sequence in each frame's own header is
 * what puts it back in order at the far end — so nothing about the payload,
 * the keys or the framing changes with the carrier count.
 *
 * `carriers <= 1` returns the single actor itself, so an attempt that asked
 * for one carrier runs the code that ran before this existed.
 *
 * @param {object} options the single-carrier options, plus `carriers`
 */
export function createCarrierGroup({ carriers = 1, events = {}, ...rest }) {
  const count = Math.max(1, Math.min(Number(carriers) || 1, MAX_CARRIERS));
  if (count === 1) {
    return createAttemptRtc({ ...rest, carrier: 0, events });
  }
  /** Carriers that have not failed, in index order. */
  const members = [];
  let readyAnnounced = false;
  let closed = false;
  let gone = 0;

  /**
   * Marks one carrier dead, exactly once, and reports the ATTEMPT dead when
   * the last one goes.
   *
   * Counting deaths here and nowhere else is what makes the report reliable:
   * a carrier can leave by three different doors — its own `onFailed`, a
   * write that throws under `send`, or the group tearing it down — and while
   * only the first was counted, a mix of doors left the tally short of the
   * carrier count for ever. `events.onFailed` then never fired, the page never
   * learned its direct attempt was dead, and the transfer sat on a relay leg
   * it had been ticketed for but never attached. `report` is false for a
   * deliberate teardown, which is not a failure and must never be announced
   * as one.
   *
   * @param {object} member the carrier
   * @param {string} reason what ended it
   * @param {boolean} report whether it may end the attempt
   */
  function markDead(member, reason, report) {
    if (member.dead) {
      return;
    }
    member.dead = true;
    gone += 1;
    if (report && gone >= count) {
      events.onFailed?.(reason);
    }
  }

  for (let index = 0; index < count; index += 1) {
    const member = { index, actor: null, ready: false, dead: false };
    member.actor = createAttemptRtc({
      ...rest,
      carrier: index,
      events: {
        onReady: (info) => {
          member.ready = true;
          // The FIRST carrier to open is what makes the attempt usable: the
          // count is a ceiling and not a reservation, so an attempt that
          // establishes three of four carries on three. Announcing once is
          // what keeps `transfer.direct_ready` a per-attempt statement.
          if (!readyAnnounced) {
            readyAnnounced = true;
            events.onReady?.(info);
          }
        },
        onMessage: (data) => events.onMessage?.(data),
        // One carrier dying is not the attempt dying: the sink simply stops
        // choosing it. Only when the last one is gone does the attempt have
        // no direct path left, and then the reason reported is the one that
        // ended it.
        onFailed: (reason) => markDead(member, reason, true),
        onClosed: () => markDead(member, "channel-closed", false),
      },
    });
    members.push(member);
  }

  /** Live carriers: open, not failed, with room to be chosen. */
  function live() {
    return members.filter((member) => member.ready && !member.dead);
  }

  /**
   * The carrier with the fewest bytes queued. Work-conserving on purpose: a
   * fixed round-robin would park the writer on a carrier whose queue is full
   * while the others sit idle, which is the head-of-line the carriers exist
   * to remove.
   */
  function leastLoaded() {
    let best = null;
    for (const member of live()) {
      const queued = member.actor.sink.bufferedAmount;
      if (best === null || queued < best.queued) {
        best = { member, queued };
      }
    }
    return best?.member ?? null;
  }

  const sink = {
    get fragmentBytes() {
      // The SMALLEST any live carrier accepts: a frame is written to
      // whichever carrier is least loaded at the time, so it has to fit all
      // of them.
      let smallest = MAX_FRAGMENT_BYTES;
      for (const member of live()) {
        smallest = Math.min(smallest, member.actor.sink.fragmentBytes);
      }
      return smallest;
    },
    get bufferedAmount() {
      let total = 0;
      for (const member of live()) {
        total += member.actor.sink.bufferedAmount;
      }
      return total;
    },
    get highWater() {
      // The MEMBERS' own mark, summed — never the module constant. A harness
      // that lowers the mark (`__borePerf.highWater`, read once per actor)
      // was inert here, so the group kept bounding at 4 MiB per carrier
      // whatever it was told; see `waitLow` for what that cost.
      let total = 0;
      for (const member of live()) {
        total += member.actor.sink.highWater;
      }
      return total > 0 ? total : RTC_HIGH_WATER;
    },
    send(bytes) {
      // A carrier that dies under the write is skipped and the frame goes to
      // the next one: losing one of N costs a retry here, never the attempt.
      for (let attempt = 0; attempt < count; attempt += 1) {
        const member = leastLoaded();
        if (member === null) {
          break;
        }
        try {
          member.actor.sink.send(bytes);
          return;
        } catch (error) {
          markDead(member, "send-error", true);
          if (error?.name !== "AbortError") {
            throw error;
          }
        }
      }
      throw new DOMException("no direct carrier is open", "AbortError");
    },
    async waitLow(signal) {
      const waiting = live();
      if (waiting.length === 0) {
        throw new DOMException("no direct carrier is open", "AbortError");
      }
      // Each carrier against ITS OWN mark. This used to compare against the
      // module constant, so the shipped 4 MiB applied no matter what the
      // actor was configured with — and with N carriers the group could hold
      // N x 4 MiB. MEASURED over a real WAN (21 ms RTT, 128 MiB): the peak
      // queued depth read 4.46 MB with the mark at 4 MiB, at 1 MiB and at
      // 256 KiB — three settings, one number, which is what a knob nobody
      // reads looks like from the outside.
      if (
        waiting.some(
          (member) => member.actor.sink.bufferedAmount <= member.actor.sink.highWater,
        )
      ) {
        return;
      }
      // The FIRST carrier to drain releases the writer. `any` and not `race`:
      // a carrier that misses its own drain deadline fails itself, and while
      // even one of the others drains the attempt is still serving. Only when
      // every carrier has failed does the wait fail, and then it fails the
      // way a single channel does, so the sender reads it as an abandoned
      // attempt and waits for the relay.
      try {
        await Promise.any(
          waiting.map((member) => member.actor.sink.waitLow(signal)),
        );
      } catch (error) {
        if (signal?.aborted) {
          throw new DOMException("aborted", "AbortError");
        }
        if (error instanceof AggregateError) {
          throw new DOMException("no direct carrier drained", "AbortError");
        }
        throw error;
      }
    },
    close() {
      for (const member of members) {
        member.actor.sink.close();
      }
    },
  };

  return {
    async start() {
      await Promise.all(members.map((member) => member.actor.start()));
    },
    handleSignal(type, body) {
      // Absent means carrier 0 — the shape a peer that knows nothing about
      // carriers sends, and the shape carrier 0 itself sends.
      const index = Number(body?.carrier ?? 0);
      const member = members[index];
      if (member === undefined) {
        return;
      }
      return member.actor.handleSignal(type, body);
    },
    close() {
      if (closed) {
        return;
      }
      closed = true;
      for (const member of members) {
        member.actor.close();
      }
      events.onClosed?.();
    },
    sink,
    /** One snapshot per carrier; each names its own index. */
    diagnostics() {
      return members.map((member) => member.actor.diagnostics());
    },
    /** Introspection for tests only. */
    state() {
      const states = members.map((member) => member.actor.state());
      return {
        carriers: count,
        ready: states.some((one) => one.ready),
        readyCount: states.filter((one) => one.ready).length,
        done: states.every((one) => one.done),
        members: states,
      };
    },
  };
}
