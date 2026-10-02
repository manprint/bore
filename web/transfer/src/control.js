// Control WebSocket client: one authenticated session per room tab.
//
// Bounded by construction: a single socket, text messages at most 320 KiB
// (server-enforced), heartbeat ping every 5 s, a socket whose ping stays
// unanswered for 20 s abandoned as dead, reconnect with backoff from
// 250 ms doubling to 5 s. Reconnect replays `hello`; republish decisions
// need the fresh catalog, so they run on `snapshot.end` in main.js — this
// module only re-authenticates. Nothing here constructs RTCPeerConnection,
// opens the relay, reads files or emits `transfer.request`: in this phase
// the only outbound messages are `hello`, `ping` and `peer.rename`.

export const CONTROL_SUBPROTOCOL = "bore-transfer-v1";
export const HELLO_TIMEOUT_MS = 10_000;
/// Heartbeat cadence once the hello is acknowledged. Every `ping` is answered
/// with a `pong`, so it is also this page's probe of the path (plan 005).
export const PING_INTERVAL_MS = 5_000;
/// How long the OLDEST unanswered `ping` may stay unanswered before the
/// socket is treated as dead (plan 005, revision 1h).
///
/// A network outage, an IP change or a NAT that forgets its mapping kills
/// the TCP connection silently: no close frame, no FIN, and the browser
/// reports the socket OPEN until its operating system gives up on the
/// connection — about 15 minutes on Linux. Until then the tab looks
/// connected and is not: nothing it publishes or requests reaches the room.
///
/// The deadline is measured from a ping this page SENT, never from the last
/// message it received, so a quiet room is not silence. 20 s covers the
/// operating system's retransmission of a ping lost in a short flick
/// (Linux and macOS retransmit a lost segment after roughly 0.2, 0.6, 1.4,
/// 3, 6 and 12.6 s; Windows starts from 300 ms and reaches ~19 s), so a
/// flick that TCP repairs costs no reconnect, while a dead path is noticed
/// within one ping interval plus this deadline (≤ 25 s).
export const PONG_DEADLINE_MS = 20_000;
/// Close code this client uses when it abandons a socket that stopped
/// answering. Distinct from the server's codes; never terminal.
export const LIVENESS_CLOSE = 4003;
export const RECONNECT_MIN_MS = 250;
export const RECONNECT_MAX_MS = 5_000;
/// Close code this client uses for its OWN hello timeout. Distinct from the
/// server's 4001 on purpose: a slow server must never be counted as a
/// refusal (see `AUTH_REFUSALS_BEFORE_TERMINAL`).
export const HELLO_TIMEOUT_CLOSE = 4002;
/// Consecutive server refusals of a hello that once succeeded before the
/// session gives up. One retry heals an idle reap; a second refusal means
/// the room (or the token) is gone, and retrying it forever leaves the page
/// saying "reconnecting" about something that can never come back.
export const AUTH_REFUSALS_BEFORE_TERMINAL = 2;

/// The heartbeat tick's decision, pure so every boundary is unit-tested.
///
/// - `now`, `lastTickAt`: this tick and the previous one (monotonic ms;
///   `lastTickAt` is `null` on the first tick).
/// - `outstandingSince`: when the oldest ping still unanswered was sent, or
///   `null` when every ping has been answered (any inbound message answers).
///
/// A tick that comes far later than scheduled means this page's timers were
/// suspended or throttled — a background tab, a frozen tab, a sleeping
/// laptop. Nothing was observed in between, so the unanswered window starts
/// again instead of counting time the page was not running: a tab woken
/// from sleep never mistakes its own nap for a dead server.
///
/// Returns `{ abandon: true }`, or `{ abandon: false, outstandingSince }`
/// with the window to keep after this tick's ping goes out.
export function pingTickDecision({ now, lastTickAt, outstandingSince, intervalMs, deadlineMs }) {
  let since = outstandingSince;
  if (lastTickAt !== null && now - lastTickAt > 2 * intervalMs) {
    since = null;
  }
  if (since !== null && now - since >= deadlineMs) {
    return { abandon: true };
  }
  return { abandon: false, outstandingSince: since ?? now };
}

/// Monotonic milliseconds: a wall-clock step must not look like silence.
function monotonicNow() {
  return globalThis.performance?.now?.() ?? Date.now();
}

/// Backoff for attempt `n` (0-based): 250 ms doubling to the 5 s ceiling.
export function reconnectDelayMs(attempt) {
  const delay = RECONNECT_MIN_MS * 2 ** Math.min(attempt, 20);
  return Math.min(delay, RECONNECT_MAX_MS);
}

/// One control session. `events` receives every inbound text message plus
/// lifecycle notifications; all callbacks are optional.
/**
 * @param {object} options
 * @param {string} options.url room control URL (`/transfer/ws/control/<id>`)
 * @param {string} options.memberToken 64-hex member token
 * @param {string|null} options.displayName optional initial name
 * @param {object} options.events `{ onMessage(msg), onHelloAck(), onClose(code, terminal), onStateChange() }`
 * @param {number} [options.pingIntervalMs] heartbeat cadence (tests shorten it)
 * @param {number} [options.pongDeadlineMs] unanswered-ping deadline (tests shorten it)
 * @param {number} [options.helloTimeoutMs] hello deadline (tests shorten it)
 */
export function createControlSession({
  url,
  memberToken,
  displayName,
  events,
  pingIntervalMs = PING_INTERVAL_MS,
  pongDeadlineMs = PONG_DEADLINE_MS,
  helloTimeoutMs = HELLO_TIMEOUT_MS,
}) {
  let socket = null;
  let attempt = 0;
  let helloTimer = null;
  let pingTimer = null;
  let reconnectTimer = null;
  let helloAcked = false;
  let everAcked = false;
  let authRefusals = 0;
  let stopped = false;
  let requestSeq = 0;
  // Oldest ping still unanswered on the live socket, and the previous tick.
  let pingOutstandingSince = null;
  let lastPingTickAt = null;

  function clearTimers() {
    if (helloTimer !== null) {
      clearTimeout(helloTimer);
      helloTimer = null;
    }
    if (pingTimer !== null) {
      clearInterval(pingTimer);
      pingTimer = null;
    }
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function sendRaw(text) {
    if (socket !== null && socket.readyState === 1) {
      // Test observability (2.6): when the `__BORE_TEST__` hook is present,
      // record every outbound control type. Read-only array push — no
      // behavior switch, inert without the hook.
      const hook = globalThis.__BORE_TEST__;
      if (hook && Array.isArray(hook.outboundTypes)) {
        try {
          const parsed = JSON.parse(text);
          const kind = parsed.type;
          if (typeof kind === "string") {
            hook.outboundTypes.push(kind);
            // An `error` reply names only the `requestId` it answers, so
            // without this map a rejected message is unattributable: the
            // gate sees `INVALID_MESSAGE` and cannot say WHICH message the
            // server refused. Recording the type by id is what makes a
            // control error diagnosable from the page alone.
            if (
              typeof parsed.requestId === "string" &&
              hook.outboundById !== null &&
              typeof hook.outboundById === "object"
            ) {
              hook.outboundById[parsed.requestId] = kind;
            }
          }
          // 3.5: the resume descriptor a request carried (or `null`), so an
          // e2e can tell a fresh download from a resumed one. Ranges only —
          // no filename, no token, no key.
          if (kind === "transfer.request" && Array.isArray(hook.resumeRequests)) {
            hook.resumeRequests.push(parsed.body?.resume?.verifiedRanges ?? null);
          }
        } catch {
          /* unparseable outbound: still send it */
        }
      }
      socket.send(text);
      return true;
    }
    return false;
  }

  function armPing() {
    pingOutstandingSince = null;
    lastPingTickAt = null;
    pingTimer = setInterval(pingTick, pingIntervalMs);
  }

  function pingTick() {
    const now = monotonicNow();
    const decision = pingTickDecision({
      now,
      lastTickAt: lastPingTickAt,
      outstandingSince: pingOutstandingSince,
      intervalMs: pingIntervalMs,
      deadlineMs: pongDeadlineMs,
    });
    lastPingTickAt = now;
    if (decision.abandon) {
      abandon(LIVENESS_CLOSE);
      return;
    }
    if (sendRaw(JSON.stringify({ v: 1, type: "ping", body: {} }))) {
      pingOutstandingSince = decision.outstandingSince;
    }
  }

  /// Gives up on the live socket NOW and redials. A dead path is exactly
  /// the case where `close()` cannot complete: the browser's closing
  /// handshake waits for a close frame that will never arrive (tens of
  /// seconds) before it fires `close`, so waiting for that event would
  /// strand the session for as long as the dead path itself. The abandoned
  /// socket's late events are ignored by the identity guard in `connect`.
  function abandon(code) {
    const dead = socket;
    clearTimers();
    socket = null;
    helloAcked = false;
    authRefusals = 0;
    pingOutstandingSince = null;
    try {
      dead?.close(code);
    } catch {
      /* already gone */
    }
    events.onClose?.(code, false);
    scheduleReconnect();
  }

  function scheduleReconnect() {
    if (stopped) {
      return;
    }
    const delay = reconnectDelayMs(attempt);
    attempt += 1;
    events.onStateChange?.("reconnecting");
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      connect();
    }, delay);
  }

  function handleMessage(raw) {
    let message;
    try {
      message = JSON.parse(raw);
    } catch {
      return;
    }
    events.onMessage?.(message);
  }

  function connect() {
    clearTimers();
    helloAcked = false;
    let next;
    try {
      next = new WebSocket(url, CONTROL_SUBPROTOCOL);
    } catch {
      scheduleReconnect();
      return;
    }
    socket = next;
    helloTimer = setTimeout(() => {
      helloTimer = null;
      abandon(HELLO_TIMEOUT_CLOSE);
    }, helloTimeoutMs);
    // Every listener below belongs to `next` only: once that socket has been
    // abandoned (or the session stopped), its late events — above all a
    // `close` that arrives tens of seconds after the redial — must not touch
    // the socket that replaced it.
    socket.addEventListener("open", () => {
      if (socket !== next) {
        return;
      }
      const hello = { v: 1, type: "hello", body: { memberToken } };
      if (displayName !== null && displayName !== undefined) {
        hello.body.displayName = displayName;
      }
      sendRaw(JSON.stringify(hello));
    });
    socket.addEventListener("message", (event) => {
      if (socket !== next || typeof event.data !== "string") {
        return;
      }
      // Any message answers every ping sent so far: the path delivers.
      pingOutstandingSince = null;
      if (!helloAcked) {
        helloAcked = true;
        everAcked = true;
        if (helloTimer !== null) {
          clearTimeout(helloTimer);
          helloTimer = null;
        }
        attempt = 0;
        authRefusals = 0;
        armPing();
        events.onHelloAck?.();
      }
      handleMessage(event.data);
    });
    const onClose = (event) => {
      if (socket !== next) {
        return;
      }
      clearTimers();
      socket = null;
      helloAcked = false;
      // Terminal codes never reconnect — except the FIRST 4001 on a session
      // the server already acked (idle reaped): a fresh hello heals that.
      // The room's own death is also a 4001 (existence is never oracled), so
      // the healing retry is bounded: a second consecutive refusal is the
      // answer, not a reason to keep dialling.
      if (event.code === 4001) {
        authRefusals += 1;
      } else {
        authRefusals = 0;
      }
      const terminal =
        event.code === 4004 ||
        event.code === 4010 ||
        (event.code === 4001 &&
          (!everAcked || authRefusals >= AUTH_REFUSALS_BEFORE_TERMINAL));
      if (terminal) {
        events.onClose?.(event.code, true);
        return;
      }
      events.onClose?.(event.code, false);
      scheduleReconnect();
    };
    socket.addEventListener("close", onClose);
    socket.addEventListener("error", () => {
      // The close event follows and carries the decision.
    });
  }

  return {
    /** Opens the session (first attempt is immediate). */
    start() {
      stopped = false;
      attempt = 0;
      connect();
    },
    /** Closes for good: no further reconnects, no timers. */
    stop() {
      stopped = true;
      clearTimers();
      try {
        socket?.close(1000);
      } catch {
        /* already gone */
      }
      socket = null;
    },
    /** Sends one `peer.rename`; returns false when offline. */
    rename(displayName) {
      requestSeq += 1;
      // Canonical 32-hex request ID, unique within this session.
      const requestId = `${Date.now().toString(16)}${requestSeq.toString(16).padStart(8, "0")}`
        .padStart(32, "0")
        .slice(-32);
      return this.send("peer.rename", requestId, { displayName });
    },
    /** Sends one raw control message; returns false when offline. */
    send(messageType, requestId, body) {
      const message = { v: 1, type: messageType, body };
      if (requestId !== null && requestId !== undefined) {
        message.requestId = requestId;
      }
      return sendRaw(JSON.stringify(message));
    },
    /** Drops the live socket and redials (network-change recovery); no-op
     * when stopped or socketless (a redial is then already scheduled). It
     * does not wait for the socket's `close` event: after a network change
     * the old connection is usually dead, and that event can take tens of
     * seconds to come. */
    cycle() {
      if (stopped || socket === null) {
        return;
      }
      abandon(1000);
    },
    /** True while the hello handshake completed on the live socket. */
    get ready() {
      return helloAcked && socket !== null && socket.readyState === 1;
    },
  };
}
