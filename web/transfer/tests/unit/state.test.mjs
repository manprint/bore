// Unit tests: reducer ordering/reset, text-node rendering, terminal close,
// republish candidates, and the no-transfer-request source scan.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  CONNECTION,
  createInitialState,
  messageToEvent,
  partitionRepublish,
  reduce,
  selectRepublishCandidates,
  sortedEntries,
} from "../../src/state.js";
import { createView } from "../../src/view.js";

const here = dirname(fileURLToPath(import.meta.url));
const srcDir = join(here, "..", "..", "src");

// Recording fake document: text flows only through `textContent`; any
// `innerHTML` use (property or attribute) is recorded for the assertion.
function fakeDoc(seen) {
  function node(tag) {
    const self = {
      tagName: tag,
      children: [],
      attributes: {},
      listeners: {},
      _text: "",
      _class: "",
      set textContent(value) {
        seen.texts.push(String(value));
        self._text = String(value);
      },
      get textContent() {
        return self._text;
      },
      set innerHTML(value) {
        seen.innerHTML.push(String(value));
      },
      set className(value) {
        self._class = String(value);
      },
      get className() {
        return self._class;
      },
      appendChild(child) {
        self.children.push(child);
        return child;
      },
      removeChild(child) {
        self.children = self.children.filter((x) => x !== child);
      },
      setAttribute(key, value) {
        if (key.toLowerCase() === "innerhtml") {
          seen.innerHTML.push(String(value));
        }
        self.attributes[key] = String(value);
      },
      addEventListener(type, fn) {
        (self.listeners[type] ??= []).push(fn);
      },
      get firstChild() {
        return self.children[0] ?? null;
      },
      value: "",
      disabled: false,
    };
    return self;
  }
  const root = node("main");
  return {
    doc: { createElement: (tag) => node(tag) },
    root,
    walk(start = root, out = []) {
      out.push(start);
      for (const child of start.children) {
        this.walk(child, out);
      }
      return out;
    },
  };
}

function peerState(entries) {
  let state = createInitialState();
  for (const event of entries) {
    state = reduce(state, event);
  }
  return state;
}

describe("room state", () => {
  it("state_reducer_orders_peers_and_offers_and_resets_on_snapshot", () => {
    let state = peerState([
      { kind: "peer.joined", peerId: "ff", displayName: "Zed" },
      { kind: "peer.joined", peerId: "11", displayName: "Ay" },
      {
        kind: "offer.added",
        peerId: "ff",
        offerId: "99",
        manifest: { label: "Z" },
        mac: "m",
      },
      {
        kind: "offer.added",
        peerId: "11",
        offerId: "00",
        manifest: { label: "A" },
        mac: "m",
      },
    ]);
    assert.deepEqual(
      sortedEntries(state.peers).map(([id]) => id),
      ["11", "ff"],
    );
    assert.deepEqual(
      sortedEntries(state.offers).map(([id]) => id),
      ["00", "99"],
    );
    // A fresh snapshot clears remote state first: stale entries vanish.
    state = reduce(state, { kind: "snapshot.begin" });
    assert.equal(state.peers.size, 0);
    assert.equal(state.offers.size, 0);
    state = reduce(state, { kind: "snapshot.peer", peerId: "11", displayName: "Ay" });
    assert.deepEqual([...state.peers.keys()], ["11"]);
    // Unknown events and unknown peers never crash or mutate.
    const before = state;
    assert.equal(reduce(state, { kind: "nope" }), before);
    assert.equal(reduce(state, { kind: "peer.renamed", peerId: "zz", displayName: "X" }), before);
    assert.equal(messageToEvent(null), null);
    assert.equal(messageToEvent({ type: "ack", body: {} }), null);
    assert.equal(messageToEvent({ type: "peer.left", body: {} }), null);
  });

  it("remote_strings_use_text_nodes", () => {
    const seen = { texts: [], innerHTML: [] };
    const fake = fakeDoc(seen);
    const view = createView(fake.doc, fake.root, {
      onRename: () => {},
      onSelectFiles: () => {},
      onWithdraw: () => {},
      onCopyLink: () => {},
    });
    const evil = '<img src=x onerror="alert(1)">';
    const evilLabel = "<script>alert(2)</script>";
    let state = peerState([
      { kind: "welcome", peerId: "11", displayName: evil },
      { kind: "snapshot.begin" },
      { kind: "snapshot.peer", peerId: "11", displayName: evil },
      { kind: "snapshot.peer", peerId: "ff", displayName: null },
      {
        kind: "snapshot.offer",
        peerId: "ff",
        offerId: "00",
        manifest: { label: evilLabel, kind: "file", entries: [{}, {}] },
        mac: "m",
      },
      { kind: "snapshot.end" },
    ]);
    state = { ...state, selfPeerId: "11", statusText: "ok" };
    view.render(state);
    assert.deepEqual(seen.innerHTML, []);
    assert.ok(seen.texts.some((t) => t.includes(evil)));
    assert.ok(seen.texts.some((t) => t.includes(evilLabel)));
    // No download affordance is created in this phase.
    const buttons = fake.walk().filter((n) => n.tagName === "button");
    const labels = buttons.map((b) => b._text);
    assert.ok(!labels.some((t) => /scarica|download/i.test(t)), labels.join("|"));
    assert.ok(!fake.walk().some((n) => n.tagName === "a"));
  });

  it("room_closed_is_terminal", () => {    let state = peerState([
      { kind: "peer.joined", peerId: "11", displayName: "Ay" },
      { kind: "room.closed" },
    ]);
    assert.equal(state.closed, true);
    assert.equal(state.connection, CONNECTION.UNAVAILABLE);
    // A close with an unknown room id maps the same way.
    assert.deepEqual(messageToEvent({ type: "room_closed", body: { reason: "expired" } }), {
      kind: "room.closed",
    });
  });

  it("reconnect_republishes_local_offers_but_never_creates_transfer", () => {
    const kept = "a".repeat(32);
    const droppedUnbacked = "b".repeat(32);
    const droppedUnacked = "c".repeat(32);
    const localOffers = new Map([
      [kept, { files: ["a.txt"] }],
      [droppedUnbacked, null],
      [droppedUnacked, { files: ["c.txt"] }],
    ]);
    const acked = new Set([kept, droppedUnbacked]);
    // Only backed AND acked offers republish; evicted or unknown ones drop.
    assert.deepEqual(selectRepublishCandidates(localOffers, acked), [kept]);
    assert.deepEqual(selectRepublishCandidates(new Map(), new Set()), []);
    // And republishing one is an offer.publish envelope, never a transfer.
    for (const id of selectRepublishCandidates(localOffers, acked)) {
      assert.match(id, /^[0-9a-f]{32}$/);
    }
    // Static tripwire: no 2.4 source may construct transfer, WebRTC, relay,
    // picker or notification traffic. Type-name tables are legitimate (they
    // enumerate the protocol); construction sites are not — comments
    // stripped before matching.
    const forbiddenPatterns = [
      /type:\s*["']transfer\.request["']/,
      /new\s+RTCPeerConnection/,
      /\/transfer\/ws\/relay/,
      /showSaveFilePicker/,
      /showDirectoryPicker/,
      /Notification/,
      /localStorage/,
      /indexedDB/,
    ];
    for (const file of readdirSync(srcDir).filter((f) => f.endsWith(".js"))) {
      const source = readFileSync(join(srcDir, file), "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, "")
        .split("\n")
        .filter((line) => !line.trimStart().startsWith("//"))
        .join("\n");
      for (const pattern of forbiddenPatterns) {
        assert.ok(!pattern.test(source), `${file} must not match ${pattern} in 2.4`);
      }
    }
  });

  it("republish_waits_out_ghost_offers", () => {
    // Absent IDs publish now; IDs still held by our ghost session wait for
    // their offer.removed (the server reaper always ends that wait).
    assert.deepEqual(partitionRepublish(["a", "b"], new Set(["b", "c"])), {
      now: ["a"],
      later: ["b"],
    });
    assert.deepEqual(partitionRepublish([], new Set(["b"])), { now: [], later: [] });
    assert.deepEqual(partitionRepublish(["a"], new Set()), { now: ["a"], later: [] });
  });

  it("test_hook_note_renders_only_with_hook_text", () => {
    const text = "Trasferimenti disponibili nella prossima versione";
    const seen = { texts: [], innerHTML: [] };
    const callbacks = {
      onRename: () => {},
      onSelectFiles: () => {},
      onWithdraw: () => {},
      onCopyLink: () => {},
    };
    globalThis.__BORE_TEST__ = { transferNote: text };
    try {
      const hooked = fakeDoc({ texts: [], innerHTML: [] });
      const hookedView = createView(hooked.doc, hooked.root, callbacks);
      hookedView.render(createInitialState());
      const notes = hooked.walk().filter((n) => n.attributes["data-testid"] === "transfer-note");
      assert.equal(notes.length, 1);
      assert.equal(notes[0]._text, text);
    } finally {
      delete globalThis.__BORE_TEST__;
    }
    // Without the hook no note element exists at all.
    const plain = fakeDoc(seen);
    createView(plain.doc, plain.root, callbacks).render(createInitialState());
    assert.ok(!plain.walk().some((n) => n.attributes["data-testid"] === "transfer-note"));
    void seen;
  });
});
