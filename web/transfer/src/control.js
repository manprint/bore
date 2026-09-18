// Control WebSocket client: one authenticated session per room tab.
//
// Bounded by construction: a single socket, text messages at most 320 KiB
// (server-enforced), heartbeat ping every 20 s, reconnect with backoff from
// 250 ms doubling to 5 s. Reconnect replays `hello`; republish decisions
// need the fresh catalog, so they run on `snapshot.end` in main.js — this
// module only re-authenticates. Nothing here constructs RTCPeerConnection,
// opens the relay, reads files or emits `transfer.request`: in this phase
// the only outbound messages are `hello`, `ping` and `peer.rename`.

export const CONTROL_SUBPROTOCOL = "bore-transfer-v1";
export const HELLO_TIMEOUT_MS = 10_000;
export const PING_INTERVAL_MS = 20_000;
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
 */
export function createControlSession({ url, memberToken, displayName, events }) {
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
    pingTimer = setInterval(() => {
      sendRaw(JSON.stringify({ v: 1, type: "ping", body: {} }));
    }, PING_INTERVAL_MS);
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
      try {
        socket?.close(HELLO_TIMEOUT_CLOSE);
      } catch {
        /* already gone */
      }
    }, HELLO_TIMEOUT_MS);
    socket.addEventListener("open", () => {
      const hello = { v: 1, type: "hello", body: { memberToken } };
      if (displayName !== null && displayName !== undefined) {
        hello.body.displayName = displayName;
      }
      sendRaw(JSON.stringify(hello));
    });
    socket.addEventListener("message", (event) => {
      if (typeof event.data !== "string") {
        return;
      }
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
    /** Drops the live socket to trigger the reconnect backoff (network-change
     * recovery); no-op when stopped or socketless. The close handler
     * schedules the redial, so this never strands the session. */
    cycle() {
      if (stopped) {
        return;
      }
      try {
        socket?.close();
      } catch {
        /* already gone; the close handler still runs */
      }
    },
    /** True while the hello handshake completed on the live socket. */
    get ready() {
      return helloAcked && socket !== null && socket.readyState === 1;
    },
  };
}
