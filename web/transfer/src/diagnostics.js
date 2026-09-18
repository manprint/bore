// Per-attempt direct-path trace (V003-C3).
//
// A direct attempt that ends has, until now, told nobody WHY: the page
// abandons it on `iceConnectionState`/`connectionState`, on a channel that
// closed, on a send that threw or on a drain that never came, and every one
// of those produced the same thing — a relay attempt and one fixed reason on
// the wire. An operator asking "why did this phone fall back?" had no answer,
// and neither did the server log.
//
// This module is that answer, under three rules it must never break:
//
//  1. **Bounded.** One attempt keeps at most `MAX_TRACE_EVENTS` events and
//     `MAX_STATS_SAMPLES` stats samples, and the page keeps at most
//     `MAX_TRACED_ATTEMPTS` attempts. A diagnostic that grows with the
//     transfer is a leak wearing a useful hat.
//  2. **Redacted BY CONSTRUCTION.** Nothing is copied out of an engine value
//     unless this file names the field and checks its shape: numbers stay
//     numbers, and the only strings that survive are short enumerations
//     (`host`, `srflx`, `udp`, `connected`, …) matched against a pattern.
//     There is no deny-list anywhere — an IP, a port, a candidate line, an
//     SDP, a file name, a room secret or a display name cannot reach the
//     trace because no code path copies a field that could hold one.
//  3. **Opaque identity.** A trace names its transfer and attempt by the
//     server's own opaque IDs and nothing else: no peer name, no label, no
//     offer, no address.
//
// The trace is never sent anywhere on its own. The page shows it only when
// the user asks for it (5.6's copy button), and the server's own line carries
// the fixed reason plus those same opaque IDs.

/** Events kept per attempt; the oldest is dropped past this. */
export const MAX_TRACE_EVENTS = 64;
/** Stats samples kept per attempt (the first and the most recent ones). */
export const MAX_STATS_SAMPLES = 8;
/** Attempts kept by one page. */
export const MAX_TRACED_ATTEMPTS = 8;
/** How often the live attempt samples `getStats()`. */
export const STATS_POLL_MS = 1_000;

/** ICE candidate types the trace keeps; anything else becomes `other`. */
const CANDIDATE_TYPES = new Set(["host", "srflx", "prflx", "relay"]);
/** Shape a short enumeration must have to survive into the trace. */
const ENUM_SHAPE = /^[a-z][a-z-]{0,23}$/;

/**
 * Keeps a short enumeration and drops everything else. An engine that
 * answers with anything but a lower-case token yields `null`, so a value
 * carrying an address or a free-form message can never be published.
 * @param {unknown} value
 * @returns {string|null}
 */
export function enumValue(value) {
  return typeof value === "string" && ENUM_SHAPE.test(value) ? value : null;
}

/**
 * Keeps a finite number, rounded to `digits`, and drops everything else.
 * @param {unknown} value
 * @param {number} [digits]
 * @returns {number|null}
 */
export function numberValue(value, digits = 0) {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return null;
  }
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}

/**
 * The type of an ICE candidate, taken from a candidate LINE without keeping
 * any other token of it. The address, the port, the foundation and the
 * related address are never read.
 * @param {unknown} line a `candidate:` attribute value
 * @returns {"host"|"srflx"|"prflx"|"relay"|"other"}
 */
export function candidateTypeOf(line) {
  if (typeof line !== "string") {
    return "other";
  }
  const match = /(?:^|\s)typ\s+([a-z]+)(?:\s|$)/.exec(line);
  const type = match === null ? null : match[1];
  return type !== null && CANDIDATE_TYPES.has(type) ? type : "other";
}

function assign(out, key, value) {
  if (value !== null && value !== undefined) {
    out[key] = value;
  }
}

/**
 * Turns one `RTCStatsReport` into the numeric, enumerated summary the trace
 * publishes. Every field is named here; nothing is copied wholesale.
 *
 * The SELECTED pair is what answers "which kind of path did this attempt
 * actually use" — `host` on a LAN, `srflx` through a NAT — and it is the one
 * question the field case could not answer. Its type is kept; its address is
 * not, and is never read.
 *
 * @param {Map<string, object>|Iterable<[string, object]>|null|undefined} report
 * @returns {object|null} the summary, or `null` when there is nothing to say
 */
export function summarizeStats(report) {
  if (report === null || report === undefined) {
    return null;
  }
  /** @type {Map<string, object>} */
  let entries;
  try {
    entries = report instanceof Map ? report : new Map(report);
  } catch {
    return null;
  }
  const out = {};
  let transportPairId = null;
  for (const entry of entries.values()) {
    if (entry?.type !== "transport") {
      continue;
    }
    const transport = {};
    assign(transport, "dtls", enumValue(entry.dtlsState));
    assign(transport, "ice", enumValue(entry.iceState));
    assign(transport, "pairChanges", numberValue(entry.selectedCandidatePairChanges));
    assign(transport, "bytesSent", numberValue(entry.bytesSent));
    assign(transport, "bytesReceived", numberValue(entry.bytesReceived));
    if (Object.keys(transport).length > 0) {
      out.transport = transport;
    }
    if (typeof entry.selectedCandidatePairId === "string") {
      transportPairId = entry.selectedCandidatePairId;
    }
  }
  let selected = null;
  if (transportPairId !== null) {
    const byId = entries.get(transportPairId);
    if (byId?.type === "candidate-pair") {
      selected = byId;
    }
  }
  if (selected === null) {
    for (const entry of entries.values()) {
      if (entry?.type !== "candidate-pair") {
        continue;
      }
      // Firefox and WebKit publish `selected`; Chromium publishes
      // `nominated` plus a `succeeded` state. Either is the pair in use.
      if (entry.selected === true || (entry.nominated === true && entry.state === "succeeded")) {
        selected = entry;
        break;
      }
    }
  }
  if (selected !== null) {
    const pair = {};
    assign(pair, "state", enumValue(selected.state));
    assign(pair, "rttMs", numberValue(selected.currentRoundTripTime * 1000, 1));
    assign(pair, "outBitrate", numberValue(selected.availableOutgoingBitrate));
    assign(pair, "bytesSent", numberValue(selected.bytesSent));
    assign(pair, "bytesReceived", numberValue(selected.bytesReceived));
    assign(pair, "requestsSent", numberValue(selected.requestsSent));
    assign(pair, "responsesReceived", numberValue(selected.responsesReceived));
    assign(pair, "discardedOnSend", numberValue(selected.packetsDiscardedOnSend));
    const local = entries.get(selected.localCandidateId);
    const remote = entries.get(selected.remoteCandidateId);
    // ONLY the type and the protocol: an address or a port is never read.
    const localType = enumValue(local?.candidateType);
    const remoteType = enumValue(remote?.candidateType);
    assign(pair, "localType", localType !== null && CANDIDATE_TYPES.has(localType) ? localType : null);
    assign(
      pair,
      "remoteType",
      remoteType !== null && CANDIDATE_TYPES.has(remoteType) ? remoteType : null,
    );
    assign(pair, "protocol", enumValue(local?.protocol));
    if (Object.keys(pair).length > 0) {
      out.pair = pair;
    }
  }
  for (const entry of entries.values()) {
    if (entry?.type === "sctp-transport") {
      const sctp = {};
      assign(sctp, "state", enumValue(entry.state));
      assign(sctp, "rttMs", numberValue(entry.smoothedRoundTripTime * 1000, 1));
      assign(sctp, "cwnd", numberValue(entry.congestionWindow));
      assign(sctp, "rwnd", numberValue(entry.receiverWindow));
      assign(sctp, "mtu", numberValue(entry.mtu));
      assign(sctp, "unackData", numberValue(entry.unackData));
      if (Object.keys(sctp).length > 0) {
        out.sctp = sctp;
      }
    }
    if (entry?.type === "data-channel") {
      const channel = {};
      assign(channel, "state", enumValue(entry.state));
      assign(channel, "messagesSent", numberValue(entry.messagesSent));
      assign(channel, "bytesSent", numberValue(entry.bytesSent));
      assign(channel, "messagesReceived", numberValue(entry.messagesReceived));
      assign(channel, "bytesReceived", numberValue(entry.bytesReceived));
      if (Object.keys(channel).length > 0) {
        out.channel = channel;
      }
    }
  }
  return Object.keys(out).length > 0 ? out : null;
}

/**
 * One attempt's trace: a bounded event list, bounded stats samples and the
 * running drain accounting the queue invariant is judged on.
 *
 * @param {object} options
 * @param {string} options.transferId opaque server id
 * @param {string} options.attemptId opaque server id
 * @param {string} options.role `offerer` or `answerer`
 * @param {() => number} [options.now] injected clock (tests)
 */
export function createAttemptTrace({ transferId, attemptId, role, now }) {
  const clock =
    typeof now === "function"
      ? now
      : () => (globalThis.performance?.now?.() ?? Date.now());
  const startedAt = clock();
  /** @type {{t: number, ev: string}[]} */
  const events = [];
  /** @type {{t: number, stats: object}[]} */
  const samples = [];
  const candidates = { local: {}, remote: {} };
  const drain = { waits: 0, timeouts: 0, waitedMs: 0, longestMs: 0, peakQueued: 0 };
  let dropped = 0;
  let reason = null;
  /**
   * When the attempt ENDED. The trace is read long after that — the report is
   * built when the user presses the button — so an elapsed time measured at
   * read time would describe the reader, not the attempt. It reported a
   * 1.3-second transfer as 18 seconds the first time it was used to measure
   * anything.
   */
  let endedAt = null;

  /** Milliseconds since the attempt was created, as an integer. */
  function at() {
    return Math.max(0, Math.round(clock() - startedAt));
  }

  /**
   * Appends one bounded event. `fields` may carry numbers and short
   * enumerations only — anything else is dropped, so a caller cannot widen
   * the trace by accident.
   * @param {string} name
   * @param {Record<string, unknown>} [fields]
   */
  function mark(name, fields) {
    const event = { t: at(), ev: String(name).slice(0, 32) };
    if (endedAt === null && (event.ev === "fail" || event.ev === "closed")) {
      endedAt = clock();
    }
    if (fields !== undefined && fields !== null) {
      for (const [key, value] of Object.entries(fields)) {
        if (typeof value === "number") {
          const kept = numberValue(value, 1);
          if (kept !== null) {
            event[key] = kept;
          }
          continue;
        }
        if (typeof value === "boolean") {
          event[key] = value;
          continue;
        }
        const kept = enumValue(value);
        if (kept !== null) {
          event[key] = kept;
        }
      }
    }
    if (events.length >= MAX_TRACE_EVENTS) {
      // The OLDEST goes: the end of an attempt is what explains it, and the
      // count of what was dropped is published so the trace never lies about
      // being complete.
      events.shift();
      dropped += 1;
    }
    events.push(event);
    return event;
  }

  /** One drain wait that ended, however it ended. */
  function drained(waitedMs, queued) {
    drain.waits += 1;
    drain.waitedMs = Math.round(drain.waitedMs + Math.max(0, waitedMs));
    drain.longestMs = Math.max(drain.longestMs, Math.round(Math.max(0, waitedMs)));
    drain.peakQueued = Math.max(drain.peakQueued, Math.round(Math.max(0, queued ?? 0)));
  }

  return {
    mark,
    /** Counts one gathered/received candidate by TYPE, never by value. */
    candidate(side, line) {
      const bucket = side === "remote" ? candidates.remote : candidates.local;
      const type = candidateTypeOf(line);
      bucket[type] = (bucket[type] ?? 0) + 1;
    },
    /** Records a summarized `getStats()` sample, bounded. */
    stats(summary) {
      if (summary === null || summary === undefined) {
        return;
      }
      if (samples.length >= MAX_STATS_SAMPLES) {
        // Index 1 and not 0: the FIRST sample is the one that says what the
        // path looked like when it was healthy.
        samples.splice(1, 1);
      }
      samples.push({ t: at(), stats: summary });
    },
    /** One drain wait that ended below the low-water mark. */
    drained,
    /** One drain wait that hit the deadline with the queue still high. */
    drainTimedOut(waitedMs, queued) {
      drain.timeouts += 1;
      drained(waitedMs, queued);
    },
    /** The fixed reason this attempt ended with, if it ended badly. */
    failed(code) {
      reason = enumValue(code);
    },
    /**
     * The whole trace, as plain JSON-able data. Safe to show to the user and
     * safe to paste into a bug report: by construction it holds no address,
     * no candidate line, no SDP, no name and no secret.
     */
    snapshot() {
      return {
        transferId,
        attemptId,
        role,
        reason,
        elapsedMs: endedAt === null ? at() : Math.max(0, Math.round(endedAt - startedAt)),
        live: endedAt === null,
        droppedEvents: dropped,
        candidates: { local: { ...candidates.local }, remote: { ...candidates.remote } },
        drain: { ...drain },
        events: events.map((event) => ({ ...event })),
        stats: samples.map((sample) => ({ t: sample.t, ...sample.stats })),
      };
    },
  };
}

/**
 * The page's bounded store of finished attempt traces, newest last.
 * @param {number} [max]
 */
export function createTraceStore(max = MAX_TRACED_ATTEMPTS) {
  /** @type {(object|(() => object))[]} */
  const kept = [];
  const resolve = (entry) => {
    if (typeof entry !== "function") {
      return entry;
    }
    try {
      return entry();
    } catch {
      return null;
    }
  };
  return {
    /**
     * Keeps one finished attempt. A FUNCTION may be passed instead of a
     * snapshot and is resolved when the report is read — the last
     * `getStats()` sample of an attempt lands asynchronously, a moment after
     * the attempt is dropped, and a snapshot taken eagerly would be the one
     * without it.
     */
    push(snapshot) {
      if (snapshot === null || snapshot === undefined) {
        return;
      }
      kept.push(snapshot);
      while (kept.length > max) {
        kept.shift();
      }
    },
    /**
     * Every kept trace, oldest first. An attempt that ran on several carriers
     * resolves to one snapshot PER CARRIER — each already names its own
     * carrier — so the list is flattened rather than nested: a report is read
     * by a person, and "three attempts, one of which is a list" is not.
     */
    all() {
      return kept
        .map(resolve)
        .flat()
        .filter((entry) => entry !== null && entry !== undefined);
    },
    get size() {
      return kept.length;
    },
    /**
     * The copyable report: a fixed header plus the traces, pretty-printed.
     * A report with nothing in it SAYS so — an empty clipboard reads as a
     * broken button.
     */
    text(extra = {}) {
      return JSON.stringify(
        {
          report: "bore-web-transfer-direct-diagnostics",
          version: 1,
          ...extra,
          attempts: kept.map(resolve).filter((entry) => entry !== null && entry !== undefined),
        },
        null,
        2,
      );
    },
  };
}
