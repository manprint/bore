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
/** Above this queued ciphertext the sender stops reading new slices. */
export const RTC_HIGH_WATER = 4 * 1024 * 1024;
/** Reading resumes once the channel has drained below this. */
export const RTC_LOW_WATER = 1 * 1024 * 1024;
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
  /** Remote candidates that arrived before the remote description. */
  const pendingCandidates = [];
  /** Everything registered on `pc`/`channel`, removed exactly once. */
  const listeners = [];
  const waiters = new Set();

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

  function releaseWaiters() {
    for (const resolve of [...waiters]) {
      waiters.delete(resolve);
      resolve();
    }
  }

  /** Idempotent teardown: listeners, channel, peer connection, timers. */
  function close() {
    clearDisconnectTimer();
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
    on(channel, "open", () => declareReady());
    on(channel, "bufferedamountlow", () => releaseWaiters());
    on(channel, "close", () => {
      if (ready) {
        fail("channel-closed");
      } else {
        fail("channel-closed");
      }
    });
    on(channel, "error", () => fail("channel-closed"));
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
    return { transferId, attemptId, ...extra };
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
        emitCandidate(null);
        return;
      }
      emitCandidate(candidate);
    });
    on(pc, "iceconnectionstatechange", () => {
      if (pc === null) {
        return;
      }
      if (pc.iceConnectionState === "failed") {
        fail("ice-failed");
      }
    });
    on(pc, "connectionstatechange", () => {
      if (pc === null || done) {
        return;
      }
      const state = pc.connectionState;
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
          // End of the peer's candidates: nothing to add, and an engine that
          // wants the marker gets it from its own gathering state.
          return;
        }
        if (typeof candidate !== "string") {
          fail("protocol");
          return;
        }
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
        if (signal?.aborted) {
          onAbort();
          return;
        }
        // A channel that never drains must not park the pipeline forever:
        // the timeout ends the wait and the next send decides.
        const timer = setTimeout(finish, DRAIN_TIMEOUT_MS);
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
      close();
      events.onClosed?.();
    },
    sink,
    /** Introspection for tests only; the app reads `events`. */
    state() {
      return {
        role,
        ready,
        done,
        fragmentBytes,
        pendingCandidates: pendingCandidates.length,
        channelState: channel?.readyState ?? null,
      };
    },
  };
}
