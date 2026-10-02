// Unit tests: control-session handshake, reconnect policy and request IDs
// with a fake WebSocket (no network, no timers beyond the 250 ms floor).
import { describe, it, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import {
  CONTROL_SUBPROTOCOL,
  LIVENESS_CLOSE,
  PING_INTERVAL_MS,
  PONG_DEADLINE_MS,
  createControlSession,
  pingTickDecision,
  reconnectDelayMs,
} from "../../src/control.js";

const instances = [];
// Every session a test starts, so `afterEach` can stop it even when an
// assertion threw first: a session left running keeps its ping/reconnect
// timers armed and the test process never exits — a regression must FAIL,
// never hang the suite.
const sessions = [];

class FakeSocket {
  constructor(url, protocol) {
    this.url = url;
    this.protocol = protocol;
    this.readyState = 0;
    this.sent = [];
    this.closedWith = null;
    this.listeners = {};
    instances.push(this);
  }

  addEventListener(type, fn) {
    (this.listeners[type] ??= []).push(fn);
  }

  emit(type, arg) {
    for (const fn of this.listeners[type] ?? []) {
      fn(arg);
    }
  }

  send(text) {
    this.sent.push(text);
  }

  close(code) {
    this.closedWith = code;
    this.readyState = 3;
  }

  open() {
    this.readyState = 1;
    this.emit("open");
  }

  serverText(json) {
    this.emit("message", { data: JSON.stringify(json) });
  }

  serverClose(code) {
    this.emit("close", { code });
  }
}

const TOKEN = "a".repeat(64);
const URL = "ws://127.0.0.1:1/transfer/ws/control/0".concat("1".repeat(31));

function startSession(overrides = {}) {
  const events = { messages: [], closes: [], states: [], republishes: 0 };
  const session = createControlSession({
    url: URL,
    memberToken: TOKEN,
    displayName: null,
    events: {
      onMessage: (msg) => events.messages.push(msg),
      onHelloAck: () => events.messages.push({ helloAck: true }),
      onClose: (code, terminal) => events.closes.push({ code, terminal }),
      onStateChange: (next) => events.states.push(next),
      onRepublish: () => {
        events.republishes += 1;
      },
    },
    onRepublishNeeded: () => [],
    ...overrides,
  });
  sessions.push(session);
  session.start();
  return { session, events, socket: instances[instances.length - 1] };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

describe("control session", () => {
  beforeEach(() => {
    instances.length = 0;
    globalThis.WebSocket = FakeSocket;
  });

  afterEach(() => {
    for (const session of sessions.splice(0)) {
      session.stop();
    }
    delete globalThis.WebSocket;
  });

  it("reconnect_delays_double_from_250ms_to_5s", () => {
    assert.equal(reconnectDelayMs(0), 250);
    assert.equal(reconnectDelayMs(1), 500);
    assert.equal(reconnectDelayMs(2), 1000);
    assert.equal(reconnectDelayMs(4), 4000);
    assert.equal(reconnectDelayMs(5), 5000);
    assert.equal(reconnectDelayMs(40), 5000);
    assert.equal(CONTROL_SUBPROTOCOL, "bore-transfer-v1");
  });

  it("hello_sent_on_open_with_exact_subprotocol", () => {
    const { session, socket } = startSession({ displayName: "Ay" });
    assert.equal(socket.url, URL);
    assert.equal(socket.protocol, "bore-transfer-v1");
    assert.equal(session.ready, false);
    socket.open();
    assert.equal(socket.sent.length, 1);
    const hello = JSON.parse(socket.sent[0]);
    assert.deepEqual(hello, {
      v: 1,
      type: "hello",
      body: { memberToken: TOKEN, displayName: "Ay" },
    });
    assert.ok(!("requestId" in hello));
    session.stop();
  });

  it("terminal_close_never_reconnects", async () => {
    for (const code of [4004, 4010, 4001]) {
      instances.length = 0;
      const { session, socket } = startSession();
      socket.open();
      socket.serverClose(code);
      await sleep(600);
      assert.equal(
        instances.length,
        1,
        `code ${code} before any ack must not reconnect`,
      );
      assert.equal(socket.closedWith, null);
      session.stop();
    }
  });

  it("reaped_session_reconnects_with_fresh_hello", async () => {
    const { session, socket, events } = startSession();
    socket.open();
    socket.serverText({ v: 1, type: "welcome", body: { peerId: "1".repeat(32), roomId: "2".repeat(32) } });
    assert.equal(session.ready, true);
    socket.serverClose(4001);
    await sleep(600);
    assert.equal(instances.length, 2);
    const retry = instances[1];
    retry.open();
    const hello = JSON.parse(retry.sent[0]);
    assert.equal(hello.type, "hello");
    assert.equal(hello.body.memberToken, TOKEN);
    assert.deepEqual(
      events.closes[0],
      { code: 4001, terminal: false },
      "reaped close is not terminal",
    );
    session.stop();
  });

  it("a_room_that_stays_gone_stops_reconnecting_and_says_so", async () => {
    // The server answers a hello for a dead room exactly as it answers a
    // bad token (4001, existence is never oracled), so the page cannot tell
    // them apart — and must not spin on "reconnecting" forever either way.
    const { session, socket, events } = startSession();
    socket.open();
    socket.serverText({
      v: 1,
      type: "welcome",
      body: { peerId: "1".repeat(32), roomId: "2".repeat(32) },
    });
    socket.serverClose(4001);
    await sleep(600);
    assert.equal(instances.length, 2, "the first refusal still heals an idle reap");
    const retry = instances[1];
    retry.open();
    retry.serverClose(4001);
    await sleep(900);
    assert.equal(instances.length, 2, "a second refusal must not dial again");
    assert.deepEqual(events.closes.at(-1), { code: 4001, terminal: true });
    session.stop();
  });

  it("a_hello_timeout_is_not_a_refusal", async () => {
    // The local timeout closes with its own code: two slow answers from a
    // live server must never be mistaken for a room that is gone.
    const { session, socket, events } = startSession();
    socket.open();
    socket.serverText({
      v: 1,
      type: "welcome",
      body: { peerId: "1".repeat(32), roomId: "2".repeat(32) },
    });
    socket.serverClose(4002);
    await sleep(600);
    const retry = instances[1];
    retry.open();
    retry.serverClose(4002);
    await sleep(900);
    assert.ok(instances.length >= 3, "a timed-out hello keeps retrying");
    assert.equal(events.closes.at(-1).terminal, false);
    session.stop();
  });

  it("ping_tick_decision_table", () => {
    const base = { intervalMs: 100, deadlineMs: 400 };
    // First tick: nothing outstanding, the ping it sends opens the window.
    assert.deepEqual(
      pingTickDecision({ ...base, now: 1000, lastTickAt: null, outstandingSince: null }),
      { abandon: false, outstandingSince: 1000 },
    );
    // Outstanding but inside the deadline: keep the OLDEST send time.
    assert.deepEqual(
      pingTickDecision({ ...base, now: 1300, lastTickAt: 1200, outstandingSince: 1000 }),
      { abandon: false, outstandingSince: 1000 },
    );
    // Exactly at the deadline, timers running normally: abandon.
    assert.deepEqual(
      pingTickDecision({ ...base, now: 1400, lastTickAt: 1300, outstandingSince: 1000 }),
      { abandon: true },
    );
    // Same age, but the previous tick is more than two intervals back: the
    // page was suspended, so the window restarts instead of tripping.
    assert.deepEqual(
      pingTickDecision({ ...base, now: 5000, lastTickAt: 1300, outstandingSince: 1000 }),
      { abandon: false, outstandingSince: 5000 },
    );
    // A gap of exactly two intervals is still a normal (late) tick.
    assert.deepEqual(
      pingTickDecision({ ...base, now: 1400, lastTickAt: 1200, outstandingSince: 1000 }),
      { abandon: true },
    );
    // The shipped values: 5 s pings, 20 s deadline (≤ 25 s to notice).
    assert.equal(PING_INTERVAL_MS, 5_000);
    assert.equal(PONG_DEADLINE_MS, 20_000);
    assert.equal(LIVENESS_CLOSE, 4003);
  });

  it("an_unanswered_ping_abandons_a_dead_socket_and_redials", { timeout: 5_000 }, async () => {
    const { session, socket, events } = startSession({ pingIntervalMs: 30, pongDeadlineMs: 120 });
    socket.open();
    socket.serverText({ v: 1, type: "welcome", body: { peerId: "1".repeat(32), roomId: "2".repeat(32) } });
    assert.equal(session.ready, true);
    // The path dies: pings go out, nothing comes back, and the fake's
    // close() fires no event — exactly a blackholed connection.
    await sleep(200);
    assert.equal(socket.closedWith, LIVENESS_CLOSE, "the dead socket is abandoned");
    assert.ok(
      socket.sent.filter((t) => JSON.parse(t).type === "ping").length >= 2,
      "pings were sent before giving up",
    );
    assert.deepEqual(events.closes.at(-1), { code: LIVENESS_CLOSE, terminal: false });
    await sleep(350);
    assert.equal(instances.length, 2, "redialled without waiting for a close event");
    const retry = instances[1];
    retry.open();
    assert.equal(JSON.parse(retry.sent[0]).type, "hello");
    retry.serverText({ v: 1, type: "welcome", body: { peerId: "3".repeat(32), roomId: "2".repeat(32) } });
    assert.equal(session.ready, true);
    // The abandoned socket's close finally arrives: it must not touch the
    // session that replaced it.
    const closesBefore = events.closes.length;
    socket.serverClose(1006);
    await sleep(100);
    assert.equal(session.ready, true, "a late close of the old socket is ignored");
    assert.equal(events.closes.length, closesBefore);
    assert.equal(instances.length, 2);
    session.stop();
  });

  it("answered_pings_keep_the_session", { timeout: 5_000 }, async () => {
    const { session, socket } = startSession({ pingIntervalMs: 30, pongDeadlineMs: 120 });
    socket.open();
    socket.serverText({ v: 1, type: "welcome", body: { peerId: "1".repeat(32), roomId: "2".repeat(32) } });
    let answered = 0;
    const responder = setInterval(() => {
      const pings = socket.sent.filter((t) => JSON.parse(t).type === "ping").length;
      while (answered < pings) {
        answered += 1;
        socket.serverText({ v: 1, type: "pong", body: {} });
      }
    }, 5);
    await sleep(500);
    clearInterval(responder);
    assert.ok(answered >= 8, `pings kept flowing (${answered})`);
    assert.equal(socket.closedWith, null, "a path that answers is never abandoned");
    assert.equal(instances.length, 1);
    assert.equal(session.ready, true);
    session.stop();
  });

  it("a_hello_timeout_redials_without_waiting_for_close", { timeout: 5_000 }, async () => {
    const { session, socket, events } = startSession({ helloTimeoutMs: 50 });
    socket.open();
    await sleep(120);
    assert.equal(socket.closedWith, 4002);
    assert.deepEqual(events.closes.at(-1), { code: 4002, terminal: false });
    await sleep(300);
    assert.equal(instances.length, 2, "the redial does not depend on the close event");
    session.stop();
  });

  it("cycle_redials_without_waiting_for_close", { timeout: 5_000 }, async () => {
    const { session, socket } = startSession();
    socket.open();
    socket.serverText({ v: 1, type: "welcome", body: { peerId: "1".repeat(32), roomId: "2".repeat(32) } });
    session.cycle();
    assert.equal(socket.closedWith, 1000);
    await sleep(350);
    assert.equal(instances.length, 2, "an offline/online cycle redials at once");
    session.stop();
  });

  it("stop_silences_the_closed_socket", { timeout: 5_000 }, async () => {
    const { session, socket, events } = startSession();
    socket.open();
    socket.serverText({ v: 1, type: "welcome", body: { peerId: "1".repeat(32), roomId: "2".repeat(32) } });
    session.stop();
    socket.serverClose(1000);
    await sleep(350);
    assert.equal(events.closes.length, 0, "no callback after stop");
    assert.equal(instances.length, 1);
  });

  it("rename_request_id_is_canonical_and_offline_send_fails", () => {
    const { session, socket } = startSession();
    assert.equal(session.rename("Nope"), false);
    socket.open();
    assert.equal(session.rename("Bobi"), true);
    const rename = JSON.parse(socket.sent[1]);
    assert.equal(rename.type, "peer.rename");
    assert.match(rename.requestId, /^[0-9a-f]{32}$/);
    assert.equal(rename.body.displayName, "Bobi");
    session.stop();
  });
});
