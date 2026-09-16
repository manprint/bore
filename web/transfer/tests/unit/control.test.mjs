// Unit tests: control-session handshake, reconnect policy and request IDs
// with a fake WebSocket (no network, no timers beyond the 250 ms floor).
import { describe, it, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { CONTROL_SUBPROTOCOL, createControlSession, reconnectDelayMs } from "../../src/control.js";

const instances = [];

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
