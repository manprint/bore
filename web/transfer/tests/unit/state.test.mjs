// Unit tests: reducer ordering/reset, text-node rendering, terminal close,
// republish candidates, and the no-transfer-request source scan.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  CONNECTION,
  TRANSFER,
  canCancelTransfer,
  createInitialState,
  errorText,
  messageToEvent,
  partitionRepublish,
  reduce,
  selectRepublishCandidates,
  sortedEntries,
  transferPercent,
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


// ---- Phase 3.5 transfer-row helpers -------------------------------------

const SELF = "22".repeat(16);
const OTHER = "11".repeat(16);
const THIRD = "33".repeat(16);
const OFFER = "cc".repeat(16);
const TRANSFER_ID = "dd".repeat(16);

function transferCallbacks(sink) {
  return {
    onRename: () => {},
    onSelectFiles: () => {},
    onWithdraw: () => {},
    onCopyLink: () => {},
    onDownload: (offerId) => sink.downloads.push(offerId),
    onCancelTransfer: (transferId) => sink.cancels.push(transferId),
  };
}

/** One remote single-file offer, plus our own peer and one peer offer. */
function catalogState() {
  return peerState([
    { kind: "welcome", peerId: SELF, displayName: null },
    { kind: "peer.joined", peerId: SELF, displayName: "Io" },
    { kind: "peer.joined", peerId: OTHER, displayName: "Altro" },
    {
      kind: "offer.added",
      peerId: OTHER,
      offerId: OFFER,
      manifest: {
        label: "a.bin",
        kind: "file",
        // A servable RAW entry: one file with at least one chunk. An entry
        // with no chunks is a directory or an empty file, and the card
        // offers those as an archive instead (5.2).
        entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }],
      },
      mac: "ab".repeat(32),
    },
  ]);
}

function startedRow(state, overrides = {}) {
  return reduce(state, {
    kind: "transfer.started",
    transferId: TRANSFER_ID,
    offerId: OFFER,
    direction: "in",
    sourcePeerId: OTHER,
    recipientPeerId: SELF,
    label: "a.bin",
    totalBytes: 1000,
    ...overrides,
  });
}

function findAll(fake, attribute) {
  return fake.walk().filter((node) => node.attributes[attribute] !== undefined);
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
    const view = createView(fake.doc, fake.root, transferCallbacks({ downloads: [], cancels: [] }));
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
        manifest: {
          label: evilLabel,
          kind: "file",
          entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }],
        },
        mac: "m",
      },
      { kind: "snapshot.end" },
    ]);
    state = { ...state, selfPeerId: "11", statusText: "ok" };
    view.render(state);
    assert.deepEqual(seen.innerHTML, []);
    assert.ok(seen.texts.some((t) => t.includes(evil)));
    assert.ok(seen.texts.some((t) => t.includes(evilLabel)));
    // Since 3.5 a remote offer carries a download button: its label is
    // ours and fixed, the remote label stays a text node on the card, and
    // render still creates no anchor (the save handler owns the only one).
    const download = fake.walk().filter((n) => n.attributes["data-download"] !== undefined);
    assert.equal(download.length, 1);
    assert.equal(download[0]._text, "Scarica");
    assert.ok(!download[0]._text.includes(evilLabel));
    assert.ok(!fake.walk().some((n) => n.tagName === "a"));
  });

  // T-WEB-XSS-CSRF (unit half) --------------------------------------------
  //
  // The test above proves a remote string cannot become MARKUP. This one
  // proves it cannot become a different STRING either: bidi overrides
  // reorder text at render time, so `fattura\u202Efdp.exe` reads as an
  // innocent PDF on the card the reader decides to download from, and
  // \u2066..\u2069 (isolates) can drag a control's own label inside a
  // remote run. Neither can survive into the document.
  it("remote_markup_and_bidi_do_not_execute_or_spoof_controls", () => {
    const seen = { texts: [], innerHTML: [] };
    const fake = fakeDoc(seen);
    const view = createView(fake.doc, fake.root, transferCallbacks({ downloads: [], cancels: [] }));
    const RLO = "\u202E";
    const PDF = "\u202C";
    const LRI = "\u2066";
    const PDI = "\u2069";
    const evilName = `${LRI}<b>Mario</b>${RLO}otarraB${PDI}`;
    const evilLabel = `fattura${RLO}fdp.exe${PDF}`;
    let state = peerState([
      { kind: "welcome", peerId: "11", displayName: null },
      { kind: "snapshot.begin" },
      { kind: "snapshot.peer", peerId: "11", displayName: "Io" },
      { kind: "snapshot.peer", peerId: "ff", displayName: evilName },
      {
        kind: "snapshot.offer",
        peerId: "ff",
        offerId: "00",
        manifest: {
          label: evilLabel,
          kind: "file",
          entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }],
        },
        mac: "m",
      },
      { kind: "snapshot.end" },
    ]);
    state = { ...state, selfPeerId: "11", statusText: `stato ${RLO}` };
    view.render(state);

    // No control character reaches the document, from any of the three
    // sources that carry one: a peer name, an offer label, and the status
    // line (which interpolates both).
    assert.deepEqual(seen.innerHTML, []);
    for (const control of [RLO, PDF, LRI, PDI]) {
      assert.ok(
        !seen.texts.some((t) => t.includes(control)),
        `a bidi control reached the document: ${JSON.stringify(control)}`,
      );
    }
    // The visible part of each string is still shown — stripping the
    // overrides must not silently eat the name.
    assert.ok(seen.texts.some((t) => t.includes("<b>Mario</b>")));
    assert.ok(seen.texts.some((t) => t.includes("fattura")));
    // And markup is still text, not elements: the renderer creates no
    // element a remote string named.
    assert.ok(!fake.walk().some((n) => n.tagName === "b"));
    // The remote label never becomes the text of a control: our buttons
    // keep their own fixed labels.
    const download = fake.walk().filter((n) => n.attributes["data-download"] !== undefined);
    assert.equal(download.length, 1);
    assert.equal(download[0]._text, "Scarica");
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
    // Static tripwire: no source may construct WebRTC, picker (beyond the
    // one guarded save enhancement), notification or localStorage traffic,
    // and only the recipient actor (receiver.js) may construct
    // `transfer.request` (explicit click path — the sender never requests).
    // Since 3.3 the sender owns exactly one relay construction site; the
    // receiver owns the other (it opens the recipient leg). The repository
    // alone touches IndexedDB; the save panel alone probes the file picker.
    // Type-name tables are legitimate (they enumerate the protocol);
    // construction sites are not — comments stripped before matching.
    const requestPattern = /type:\s*["']transfer\.request["']/;
    const relayPattern = /\/transfer\/ws\/relay/;
    const pickerPattern = /showSaveFilePicker/;
    const idbPattern = /indexedDB/;
    const forbiddenPatterns = [
      /new\s+RTCPeerConnection/,
      /Notification/,
      /localStorage/,
    ];
    // 5.1 ships directory selection, so the picker is no longer forbidden —
    // it is OWNED. One file names the API; every other file asks that file.
    const directoryPattern = /showDirectoryPicker/;
    for (const file of readdirSync(srcDir).filter((f) => f.endsWith(".js"))) {
      const source = readFileSync(join(srcDir, file), "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, "")
        .split("\n")
        .filter((line) => !line.trimStart().startsWith("//"))
        .join("\n");
      for (const pattern of forbiddenPatterns) {
        assert.ok(!pattern.test(source), `${file} must not match ${pattern}`);
      }
      if (file === "receiver.js") {
        assert.ok(requestPattern.test(source), "receiver.js must own the request site");
      } else {
        assert.ok(!requestPattern.test(source), `${file} must not match ${requestPattern}`);
      }
      if (file === "sender.js" || file === "receiver.js") {
        assert.ok(relayPattern.test(source), `${file} must own its relay site`);
      } else {
        assert.ok(!relayPattern.test(source), `${file} must not match ${relayPattern}`);
      }
      if (file === "main.js") {
        assert.ok(pickerPattern.test(source), "main.js must own the picker enhancement");
      } else {
        assert.ok(!pickerPattern.test(source), `${file} must not match ${pickerPattern}`);
      }
      if (file === "storage.js") {
        assert.ok(idbPattern.test(source), "storage.js must own the IndexedDB backend");
      } else {
        assert.ok(!idbPattern.test(source), `${file} must not match ${idbPattern}`);
      }
      if (file === "folders.js") {
        assert.ok(directoryPattern.test(source), "folders.js must own the directory picker");
      } else {
        assert.ok(!directoryPattern.test(source), `${file} must not match ${directoryPattern}`);
      }
    }
  });


  it("only_download_or_resume_click_dispatches_request", () => {
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, transferCallbacks(sink));
    let state = catalogState();
    view.render(state, new Map());
    // Rendering a catalog dispatches nothing: only a click can.
    assert.deepEqual(sink.downloads, []);
    const buttons = findAll(fake, "data-download");
    assert.equal(buttons.length, 1, "exactly one download entry point per offer");
    assert.equal(buttons[0]._text, "Scarica");
    buttons[0].listeners.click[0]();
    assert.deepEqual(sink.downloads, [OFFER]);
    // A partial on disk renames the SAME single button; it still waits for
    // a click, and there is no second dispatcher next to it.
    state = reduce(state, { kind: "transfer.resumable", offerIds: new Set([OFFER]) });
    view.render(state, new Map());
    const resume = findAll(fake, "data-download");
    assert.equal(resume.length, 1);
    assert.equal(resume[0]._text, "Riprendi");
    assert.deepEqual(sink.downloads, [OFFER], "re-rendering resumes nothing");
    resume[0].listeners.click[0]();
    assert.deepEqual(sink.downloads, [OFFER, OFFER]);
    // While a request is pending the button is disabled: a second click
    // would mint a second transfer for the same selection.
    state = startedRow(state);
    view.render(state, new Map());
    assert.equal(findAll(fake, "data-download")[0].disabled, true);
  });

  it("own_offer_has_no_download_button", () => {
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, transferCallbacks(sink));
    const state = reduce(catalogState(), {
      kind: "offer.added",
      peerId: SELF,
      offerId: "ee".repeat(16),
      manifest: {
        label: "mine.bin",
        kind: "file",
        entries: [{ id: "0", path: "mine.bin", chunks: ["ef".repeat(32)] }],
      },
      mac: "cd".repeat(32),
    });
    view.render(state, new Map());
    // Our own card carries the withdraw control and no download control.
    assert.deepEqual(
      findAll(fake, "data-download").map((n) => n.attributes["data-download"]),
      [OFFER],
    );
    assert.deepEqual(
      findAll(fake, "data-withdraw").map((n) => n.attributes["data-withdraw"]),
      ["ee".repeat(16)],
    );
  });

  it("a_reconnect_interrupts_every_live_row_and_keeps_the_rest", () => {
    // The reconnect gives this page a NEW peer ID and the server has already
    // cancelled what the old one was doing, telling only the other party.
    // Every live row ends; a finished or verified row does not move.
    const VERIFIED_ID = "ee".repeat(16);
    let state = startedRow(catalogState());
    state = reduce(state, {
      kind: "transfer.progress",
      transferId: TRANSFER_ID,
      doneBytes: 250,
      totalBytes: 1000,
      bytesPerSecond: 1024,
    });
    state = startedRow(state, { transferId: VERIFIED_ID, offerId: "ab".repeat(16) });
    state = reduce(state, {
      kind: "transfer.state",
      transferId: VERIFIED_ID,
      state: TRANSFER.VERIFIED,
    });
    const next = reduce(state, { kind: "transfers.interrupted", code: "INTERRUPTED" });
    const live = next.transfers.get(TRANSFER_ID);
    assert.equal(live.state, TRANSFER.FAILED);
    assert.equal(live.code, "INTERRUPTED");
    // The verified bytes stay counted: the partial is what "Riprendi" resumes.
    assert.equal(live.doneBytes, 250);
    assert.equal(next.transfers.get(VERIFIED_ID).state, TRANSFER.VERIFIED);
    // Once nothing is live the event is a no-op, down to object identity:
    // `main.js` announces the interruption only when a row actually ended.
    assert.equal(reduce(next, { kind: "transfers.interrupted" }), next);
    assert.notEqual(errorText("INTERRUPTED"), errorText("FAILED"));
  });

  it("only_participants_see_cancel", () => {
    const row = {
      state: TRANSFER.TRANSFERRING,
      sourcePeerId: OTHER,
      recipientPeerId: SELF,
    };
    assert.equal(canCancelTransfer(row, SELF), true);
    assert.equal(canCancelTransfer(row, OTHER), true);
    assert.equal(canCancelTransfer(row, THIRD), false, "a stranger never cancels");
    assert.equal(canCancelTransfer(row, null), false);
    // Terminal rows carry no cancel for anybody.
    for (const terminal of [TRANSFER.DONE, TRANSFER.VERIFIED, TRANSFER.CANCELLED, TRANSFER.FAILED]) {
      assert.equal(canCancelTransfer({ ...row, state: terminal }, SELF), false);
    }
    // ...and the button follows the rule in the DOM.
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, transferCallbacks(sink));
    let state = startedRow(catalogState());
    state = reduce(state, {
      kind: "transfer.progress",
      transferId: TRANSFER_ID,
      doneBytes: 250,
      totalBytes: 1000,
      bytesPerSecond: 1024,
    });
    view.render(state, new Map());
    const rows = findAll(fake, "data-transfer");
    assert.equal(rows.length, 1);
    assert.equal(transferPercent(state.transfers.get(TRANSFER_ID)), 25);
    const cancel = findAll(fake, "data-cancel");
    assert.equal(cancel.length, 1);
    cancel[0].listeners.click[0]();
    assert.deepEqual(sink.cancels, [TRANSFER_ID]);
    // A third peer's tab holds the same row data and shows no button.
    const stranger = { ...state, selfPeerId: THIRD };
    const otherFake = fakeDoc({ texts: [], innerHTML: [] });
    createView(otherFake.doc, otherFake.root, transferCallbacks(sink)).render(stranger, new Map());
    assert.equal(findAll(otherFake, "data-transfer").length, 1);
    assert.equal(findAll(otherFake, "data-cancel").length, 0);
  });

  it("reconnect_marks_partial_resume_available_without_request", () => {
    // A cancelled download leaves its partial: the offer becomes resumable
    // and the row goes terminal. Nothing in the reducer can request.
    let state = startedRow(catalogState());
    state = reduce(state, {
      kind: "transfer.state",
      transferId: TRANSFER_ID,
      state: TRANSFER.CANCELLED,
      code: "CANCELLED",
    });
    state = reduce(state, { kind: "transfer.resumable", offerIds: new Set([OFFER]) });
    assert.equal(state.transfers.get(TRANSFER_ID).state, TRANSFER.CANCELLED);
    assert.ok(state.resumable.has(OFFER));
    // A reconnect replays the snapshot: peers and offers reset, and the
    // resume markers are re-read, never acted on.
    state = reduce(state, { kind: "snapshot.begin" });
    state = reduce(state, { kind: "snapshot.peer", peerId: OTHER, displayName: "Altro" });
    state = reduce(state, {
      kind: "snapshot.offer",
      peerId: OTHER,
      offerId: OFFER,
      manifest: {
        label: "a.bin",
        kind: "file",
        entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }],
      },
      mac: "ab".repeat(32),
    });
    state = reduce(state, { kind: "snapshot.end" });
    assert.ok(state.resumable.has(OFFER), "the partial survives the reconnect");
    // The button reads "Riprendi" and stays enabled — waiting for a click.
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    createView(fake.doc, fake.root, transferCallbacks(sink)).render(
      { ...state, selfPeerId: SELF },
      new Map(),
    );
    const button = findAll(fake, "data-download")[0];
    assert.equal(button._text, "Riprendi");
    assert.equal(button.disabled, false);
    assert.deepEqual(sink.downloads, []);
  });

  it("room_close_purges_and_disables_all", () => {
    let state = startedRow(catalogState());
    state = reduce(state, { kind: "transfer.resumable", offerIds: new Set([OFFER]) });
    state = reduce(state, { kind: "room.closed" });
    assert.equal(state.closed, true);
    assert.equal(state.connection, CONNECTION.UNAVAILABLE);
    assert.equal(state.transfers.size, 0, "no row survives a room close");
    assert.equal(state.resumable.size, 0, "no partial is offered afterwards");
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    createView(fake.doc, fake.root, transferCallbacks(sink)).render(state, new Map());
    assert.equal(findAll(fake, "data-transfer").length, 0);
    // Every remaining control is disabled: nothing can start after close.
    for (const node of findAll(fake, "data-download")) {
      assert.equal(node.disabled, true);
    }
  });

  it("verified_row_ends_on_save_and_disappears_on_discard", () => {
    // Staging is not the end of a download: the row says so until the user
    // saves (then it is done) or discards (then it is gone with the bytes).
    let state = startedRow(catalogState());
    state = reduce(state, {
      kind: "transfer.state",
      transferId: TRANSFER_ID,
      state: TRANSFER.VERIFIED,
    });
    const verified = state.transfers.get(TRANSFER_ID);
    assert.equal(verified.state, TRANSFER.VERIFIED);
    assert.equal(verified.doneBytes, verified.totalBytes, "a verified row reads 100%");
    assert.equal(canCancelTransfer(verified, SELF), false);
    const saved = reduce(state, {
      kind: "transfer.state",
      transferId: TRANSFER_ID,
      state: TRANSFER.DONE,
    });
    assert.equal(saved.transfers.get(TRANSFER_ID).state, TRANSFER.DONE);
    const discarded = reduce(state, { kind: "transfer.removed", transferId: TRANSFER_ID });
    assert.equal(discarded.transfers.size, 0);
    // Removing an unknown row changes nothing at all.
    assert.equal(reduce(discarded, { kind: "transfer.removed", transferId: TRANSFER_ID }), discarded);
  });

  it("error_codes_map_to_safe_user_text", () => {
    // Every protocol code has stable Italian text...
    for (const code of [
      "UNSUPPORTED_VERSION",
      "ROOM_UNAVAILABLE",
      "UNAUTHORIZED",
      "INVALID_MESSAGE",
      "RATE_LIMITED",
      "LIMIT_EXCEEDED",
      "OFFER_NOT_FOUND",
      "OFFER_CHANGED",
      "TRANSFER_NOT_FOUND",
      "NOT_PARTICIPANT",
      "SOURCE_OFFLINE",
      "SOURCE_CHANGED",
      "DIRECT_FAILED",
      "RELAY_BUSY",
      "STORAGE_QUOTA",
      "CANCELLED",
      "INTERNAL",
      "MANIFEST_MAC",
      "MULTI_ENTRY",
    ]) {
      const text = errorText(code);
      assert.ok(text.length > 0);
      assert.ok(!text.includes(code), `${code} must not leak the raw code`);
    }
    // ...and anything else reads as a generic failure, so server-supplied
    // text can never reach the screen.
    const generic = errorText("INTERNAL_SERVER_SAYS_<script>alert(1)</script>");
    assert.equal(generic, "Trasferimento non riuscito");
    assert.equal(errorText(undefined), "Trasferimento non riuscito");
    assert.equal(errorText({ toString: () => "x" }), "Trasferimento non riuscito");
  });

  it("error_text_never_echoes_short_link_material", () => {
    const capability = [
      "YVCYDRDYkjNIIyoIIHIH_w",
      "cfa20bcf70b2a0fd23ba06651bdfb9c6291f66df8e9deb5b342974fc2e3d45d6",
      "585579f6a4e991c4b8e632e9d79fb73d89fe02e60ec43a8a69158f704cbc61d2",
    ];
    for (const code of ["ROOM_UNAVAILABLE", "INTERNAL", "FAILED", "UNKNOWN"]) {
      const text = errorText(code);
      for (const secret of capability) {
        assert.ok(!text.includes(secret), `${code} leaked short-link material`);
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

// ---- Sub-phase 5.4: the acceptance invariants, as pure state and render --
//
// The e2e scenario reads these three through a browser; they are pinned here
// as well because a browser assertion can only observe what a room happened
// to do on one run, and these are supposed to be true of every state.
describe("source-only catalog", () => {
  it("offer_source_is_original_peer_only", () => {
    // Two remote peers publish; the catalog records each offer against the
    // peer that published it, and nothing else can change that attribution.
    let state = peerState([
      { kind: "welcome", peerId: SELF, displayName: null },
      { kind: "peer.joined", peerId: SELF, displayName: "Io" },
      { kind: "peer.joined", peerId: OTHER, displayName: "Altro" },
      { kind: "peer.joined", peerId: THIRD, displayName: "Terzo" },
      {
        kind: "offer.added",
        peerId: OTHER,
        offerId: OFFER,
        manifest: { label: "a.bin", kind: "file", entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }] },
        mac: "ab".repeat(32),
      },
    ]);
    assert.equal(state.offers.get(OFFER).peerId, OTHER);

    // Downloading it — started, progressed, completed — never republishes
    // it under the recipient. The catalog is unchanged, offer for offer.
    const before = state.offers;
    state = startedRow(state);
    state = reduce(state, { kind: "transfer.progress", transferId: TRANSFER_ID, receivedBytes: 1000 });
    state = reduce(state, { kind: "transfer.state", transferId: TRANSFER_ID, state: TRANSFER.COMPLETED });
    assert.equal(state.offers, before, "a completed download must not touch the catalog");
    assert.equal(state.offers.get(OFFER).peerId, OTHER);

    // A republish is an explicit act with its OWN offer ID, and it is
    // attributed to whoever published it — here THIRD, who publishes the
    // same content under a new ID. The original keeps its own source.
    const republished = "ee".repeat(16);
    state = reduce(state, {
      kind: "offer.added",
      peerId: THIRD,
      offerId: republished,
      manifest: { label: "a.bin", kind: "file", entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }] },
      mac: "cd".repeat(32),
    });
    assert.notEqual(republished, OFFER);
    assert.equal(state.offers.get(republished).peerId, THIRD);
    assert.equal(state.offers.get(OFFER).peerId, OTHER);

    // And a source leaving takes ONLY its own offers with it.
    state = reduce(state, { kind: "peer.left", peerId: OTHER });
    assert.deepEqual([...state.offers.keys()], [republished]);
  });

  it("download_completion_never_publishes_output", () => {
    // The strongest form of the rule above: from an EMPTY catalog, a whole
    // incoming transfer produces no offer at all. Nothing in the reducer can
    // turn received bytes into something this peer serves.
    let state = peerState([
      { kind: "welcome", peerId: SELF, displayName: null },
      { kind: "peer.joined", peerId: SELF, displayName: "Io" },
      { kind: "peer.joined", peerId: OTHER, displayName: "Altro" },
    ]);
    assert.equal(state.offers.size, 0);
    for (const event of [
      {
        kind: "transfer.started",
        transferId: TRANSFER_ID,
        offerId: OFFER,
        direction: "in",
        sourcePeerId: OTHER,
        recipientPeerId: SELF,
        label: "a.bin",
        totalBytes: 10,
      },
      { kind: "transfer.path", transferId: TRANSFER_ID, path: "direct" },
      { kind: "transfer.progress", transferId: TRANSFER_ID, receivedBytes: 10 },
      { kind: "transfer.state", transferId: TRANSFER_ID, state: TRANSFER.COMPLETED },
      { kind: "transfer.removed", transferId: TRANSFER_ID },
    ]) {
      state = reduce(state, event);
      assert.equal(state.offers.size, 0, `${event.kind} must publish nothing`);
    }
  });

  it("zip_action_is_scoped_to_one_offer", () => {
    // Two remote offers — one file, one folder — plus one of our own. Every
    // download control must name exactly one offer ID, and there must be no
    // control that would take the room.
    const folder = "77".repeat(16);
    const own = "88".repeat(16);
    const state = peerState([
      { kind: "welcome", peerId: SELF, displayName: null },
      { kind: "peer.joined", peerId: SELF, displayName: "Io" },
      { kind: "peer.joined", peerId: OTHER, displayName: "Altro" },
      {
        kind: "offer.added",
        peerId: OTHER,
        offerId: OFFER,
        manifest: { label: "a.bin", kind: "file", entries: [{ id: "0", path: "a.bin", chunks: ["ab".repeat(32)] }] },
        mac: "ab".repeat(32),
      },
      {
        kind: "offer.added",
        peerId: OTHER,
        offerId: folder,
        manifest: {
          label: "albero",
          kind: "folder",
          entries: [
            { id: "0", path: "albero", size: "0", chunks: [], root: null },
            { id: "1", path: "albero/uno.txt", size: "11", chunks: ["cd".repeat(32)], root: "cd".repeat(32) },
            { id: "2", path: "albero/due.txt", size: "13", chunks: ["ef".repeat(32)], root: "ef".repeat(32) },
          ],
        },
        mac: "cd".repeat(32),
      },
      {
        kind: "offer.added",
        peerId: SELF,
        offerId: own,
        manifest: { label: "mio.bin", kind: "file", entries: [{ id: "0", path: "mio.bin", chunks: ["11".repeat(32)] }] },
        mac: "ef".repeat(32),
      },
    ]);
    const sink = { downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    createView(fake.doc, fake.root, transferCallbacks(sink)).render(state, new Map());

    // Offer-level controls: one per SOMEONE ELSE's offer, named by its ID.
    // The file offer gets the raw button, the folder the archive one, and
    // our own offer gets neither — it gets a withdraw.
    assert.deepEqual(
      findAll(fake, "data-download").map((n) => n.attributes["data-download"]),
      [OFFER],
    );
    assert.deepEqual(
      findAll(fake, "data-download-zip").map((n) => n.attributes["data-download-zip"]),
      [folder],
    );
    assert.deepEqual(
      findAll(fake, "data-withdraw").map((n) => n.attributes["data-withdraw"]),
      [own],
    );

    // Per-file controls inside the folder's tree name THAT offer and one
    // entry of it — never the room, never another offer.
    const perFile = findAll(fake, "data-download-entry").map(
      (n) => n.attributes["data-download-entry"],
    );
    assert.deepEqual(perFile.sort(), [`${folder}:1`, `${folder}:2`].sort());

    // Every download control in the whole tree is one of those three kinds,
    // so there is no room-wide archive action anywhere in the page.
    const scoped = new Set([
      ...findAll(fake, "data-download"),
      ...findAll(fake, "data-download-zip"),
      ...findAll(fake, "data-download-entry"),
      ...findAll(fake, "data-withdraw"),
      ...findAll(fake, "data-restart"),
      ...findAll(fake, "data-cancel"),
    ]);
    const buttons = fake.walk().filter((n) => n.tagName === "button");
    for (const button of buttons) {
      const label = button._text ?? "";
      if (!scoped.has(button)) {
        assert.ok(
          !/scarica|riprendi|riparti/i.test(label),
          `an unscoped button offers a download: ${JSON.stringify(label)}`,
        );
      }
    }

    // And a click carries the offer it was rendered for.
    findAll(fake, "data-download-zip")[0].listeners.click[0]();
    assert.deepEqual(sink.downloads, [folder]);
  });
});

// ---- Sub-phase 5.6: the interface's own promises --------------------------
//
// Three of these are about a badge that must never say more than has been
// proved, two about a gesture that must never be silent, and one about a
// progress bar an assistive technology can actually read.
describe("interface", () => {
  function uiCallbacks(sink) {
    return {
      onRename: () => {},
      onSelectFiles: (entries, origin) => sink.selected.push({ entries, origin }),
      onSelectionError: (message) => sink.errors.push(message),
      onWithdraw: () => {},
      onCopyLink: () => {},
      onDownload: (offerId) => sink.downloads.push(offerId),
      onRestartDownload: () => {},
      onCancelTransfer: (transferId) => sink.cancels.push(transferId),
    };
  }

  function nodeWithId(fake, id) {
    return fake.walk().find((node) => node.attributes.id === id) ?? null;
  }

  function badge(fake) {
    return fake.walk().find((node) => node._class === "transfer-path") ?? null;
  }

  function fire(node, type, event) {
    for (const listener of node.listeners[type] ?? []) {
      listener(event);
    }
  }

  /** A dropped flat file list: no handles, so the engine-agnostic path. */
  function fakeDataTransfer(names) {
    return {
      items: [],
      files: names.map((name) => ({
        name,
        size: 4,
        lastModified: 0,
        webkitRelativePath: "",
      })),
    };
  }

  it("path_badge_stays_connecting_until_a_chunk_is_verified", () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    let state = startedRow(catalogState());
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "connecting");
    assert.equal(
      fake.walk().find((n) => n._class === "path-word")._text,
      "in connessione",
    );
    // A shape as well as a word: colour is not the only carrier.
    assert.equal(fake.walk().find((n) => n._class === "path-mark")._text, "◌");
    assert.ok((badge(fake).attributes.title ?? "").length > 0);

    // Bytes on the wire are NOT a verified chunk: progress alone moves the
    // bar and leaves the badge exactly where it was.
    state = reduce(state, {
      kind: "transfer.progress",
      transferId: TRANSFER_ID,
      doneBytes: 500,
      totalBytes: 1000,
    });
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "connecting");

    // Only the committed-and-verified path puts a word there.
    state = reduce(state, { kind: "transfer.path", transferId: TRANSFER_ID, path: "direct" });
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "direct");
    assert.equal(fake.walk().find((n) => n._class === "path-word")._text, "diretto");
    assert.equal(fake.walk().find((n) => n._class === "path-mark")._text, "◆");
  });

  it("fallback_resets_the_badge_before_it_says_relay", () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    let state = startedRow(catalogState());
    state = reduce(state, { kind: "transfer.path", transferId: TRANSFER_ID, path: "direct" });
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "direct");

    // The direct attempt dies. The badge must NOT keep naming a transport
    // that is no longer carrying anything.
    state = reduce(state, { kind: "transfer.path_reset", transferId: TRANSFER_ID });
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "connecting");

    // And only the replacement attempt's own verified chunk says `relay`.
    state = reduce(state, { kind: "transfer.path", transferId: TRANSFER_ID, path: "relay" });
    view.render(state, new Map());
    assert.equal(badge(fake).attributes["data-path"], "relay");
    assert.equal(fake.walk().find((n) => n._class === "path-mark")._text, "▲");

    // A reset is local and bounded: it never touches a terminal row, and no
    // remote message maps to it.
    const terminal = reduce(state, {
      kind: "transfer.state",
      transferId: TRANSFER_ID,
      state: TRANSFER.DONE,
    });
    assert.equal(
      reduce(terminal, { kind: "transfer.path_reset", transferId: TRANSFER_ID }),
      terminal,
    );
    assert.equal(messageToEvent({ type: "transfer.path_reset", body: { transferId: TRANSFER_ID } }), null);
  });

  it("drop_of_files_publishes_one_offer_and_no_transfer", async () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    view.render(createInitialState(), new Map());
    const zone = nodeWithId(fake, "dropzone");
    assert.equal(zone.attributes["data-dragging"], "false");

    // A drag over the zone is visible, and leaving it puts the zone back.
    fire(zone, "dragenter", { preventDefault: () => {} });
    assert.equal(zone.attributes["data-dragging"], "true");
    fire(zone, "dragleave", {});
    assert.equal(zone.attributes["data-dragging"], "false");

    fire(zone, "drop", {
      preventDefault: () => {},
      dataTransfer: fakeDataTransfer(["uno.txt", "due.txt"]),
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(zone.attributes["data-dragging"], "false");
    assert.equal(sink.selected.length, 1, "one drop, one publish");
    assert.deepEqual(
      sink.selected[0].entries.map((entry) => entry.path).sort(),
      ["due.txt", "uno.txt"],
    );
    assert.equal(sink.selected[0].origin, "drop");
    // The gesture PUBLISHES. It never starts a transfer (D3).
    assert.deepEqual(sink.downloads, []);
    assert.deepEqual(sink.errors, []);
  });

  it("drop_while_locked_is_refused_with_a_message", async () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    view.render(
      { ...createInitialState(), connection: CONNECTION.UNAVAILABLE },
      new Map(),
    );
    const zone = nodeWithId(fake, "dropzone");
    assert.equal(zone.attributes["aria-disabled"], "true");
    fire(zone, "drop", {
      preventDefault: () => {},
      dataTransfer: fakeDataTransfer(["uno.txt"]),
    });
    await new Promise((resolve) => setTimeout(resolve, 0));
    // Refused, and SAID: a drop that produced nothing and no message reads
    // as a broken page.
    assert.deepEqual(sink.selected, []);
    assert.equal(sink.errors.length, 1);
    assert.match(sink.errors[0], /non disponibile/i);
  });

  it("empty_states_have_their_own_text", () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    view.render(createInitialState(), new Map());
    for (const id of ["catalog-empty", "transfers-empty"]) {
      const node = nodeWithId(fake, id);
      assert.ok(node !== null, `${id} must exist when its zone is empty`);
      assert.ok(node._text.length > 20, `${id} must say what to do`);
    }
    // The three zones exist, in order, and nothing else is a zone.
    const zones = fake.walk().filter((node) => node._class === "zone");
    assert.deepEqual(
      zones.map((node) => node.attributes.id),
      ["zone-room", "zone-offers", "zone-transfers"],
    );

    // A live transfer removes the transfers empty state and nothing else.
    view.render(startedRow(catalogState()), new Map());
    assert.equal(nodeWithId(fake, "transfers-empty"), null);
    assert.ok(nodeWithId(fake, "catalog-empty") === null);
    // And it comes back when the row goes.
    view.render(createInitialState(), new Map());
    assert.ok(nodeWithId(fake, "transfers-empty") !== null);
  });

  it("progressbar_exposes_value_min_max", () => {
    const sink = { selected: [], errors: [], downloads: [], cancels: [] };
    const fake = fakeDoc({ texts: [], innerHTML: [] });
    const view = createView(fake.doc, fake.root, uiCallbacks(sink));
    let state = startedRow(catalogState());
    view.render(state, new Map());
    const bar = fake.walk().find((node) => node._class === "transfer-progress");
    assert.equal(bar.attributes.role, "progressbar");
    assert.equal(bar.attributes["aria-valuemin"], "0");
    assert.equal(bar.attributes["aria-valuemax"], "100");
    assert.equal(bar.attributes["aria-valuenow"], "0");

    state = reduce(state, {
      kind: "transfer.progress",
      transferId: TRANSFER_ID,
      doneBytes: 370,
      totalBytes: 1000,
    });
    view.render(state, new Map());
    // The ARIA value tracks the visual one: a bar that says "busy" and
    // nothing else is not a progress bar to a screen reader.
    assert.equal(bar.attributes["aria-valuenow"], bar.attributes.value);
    assert.equal(bar.attributes["aria-valuenow"], "37");
  });
});

describe("web-transfer relay-only room", () => {
  it("reads the policy from welcome, and only as a literal true", () => {
    // `relayOnly` is ADVISORY here — the server is what enforces it, by never
    // opening a direct attempt — so the page's only job is to report it
    // faithfully. An older server omits the field, and "absent" has to read
    // as the historical behaviour, never as the policy: a page that showed
    // "relay (imposto)" on an ordinary room would be telling the user the
    // direct path is disabled when it is simply not in use yet.
    assert.equal(createInitialState().relayOnly, false);

    const on = messageToEvent({
      type: "welcome",
      body: { peerId: "11", roomId: "22", relayOnly: true },
    });
    assert.equal(on.relayOnly, true);
    assert.equal(reduce(createInitialState(), on).relayOnly, true);

    for (const body of [
      { peerId: "11", roomId: "22" },
      { peerId: "11", roomId: "22", relayOnly: false },
      { peerId: "11", roomId: "22", relayOnly: "true" },
      { peerId: "11", roomId: "22", relayOnly: 1 },
    ]) {
      const event = messageToEvent({ type: "welcome", body });
      assert.equal(event.relayOnly, false, JSON.stringify(body));
      assert.equal(reduce(createInitialState(), event).relayOnly, false);
    }
  });
});
