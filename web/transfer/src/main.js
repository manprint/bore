import "./styles.css";
import {
  buildRoomUrl,
  clearSecrets,
  loadSecrets,
  parseRoomUrl,
  saveSecrets,
  scrubFragment,
} from "./secrets.js";
import {
  CONNECTION,
  createInitialState,
  messageToEvent,
  partitionRepublish,
  reduce,
} from "./state.js";
import { createControlSession } from "./control.js";
import { createOfferManager } from "./offers.js";
import { createView } from "./view.js";

// Browser bootstrap (Phase 2.5): secrets from the fragment into
// sessionStorage (scrubbed immediately), one control session, peer/catalog
// rendering, and local offer preparation (hash in a worker, publish on the
// control channel). Downloads still do not exist: no code path below can
// emit `transfer.request`, WebRTC or relay traffic (audited by the unit
// source scan and the e2e frame audit).

const app = document.getElementById("app");
let state = createInitialState();
let secrets = null;
let roomId = null;
let session = null;
let serverLimits = null;
let offers = null;
/** Republish candidates waiting out their ghost session's removal. */
const deferredRepublish = new Set();

function makeOffers() {
  return createOfferManager({
    createWorker: () => new Worker("/transfer/assets/offer-worker.js", { type: "module" }),
    roomIdHex: roomId,
    roomKeyHex: secrets.roomKey,
    sendControl: (message) => {
      if (session === null) {
        return false;
      }
      return session.send(message.type, message.requestId ?? null, message.body);
    },
    events: {
      onProgress: () => renderOffers(),
      onDone: () => {
        renderOffers();
        view.announce("Offerta pronta");
      },
      onError: (_offerId, message) => {
        renderOffers();
        view.announce(`Offerta non riuscita: ${message}`);
      },
      onWithdrawn: () => renderOffers(),
    },
  });
}

function offersUi() {
  const ui = new Map();
  if (offers === null) {
    return ui;
  }
  for (const [offerId, record] of offers.offers()) {
    ui.set(offerId, { status: record.status, progress01: record.progress ?? 0 });
  }
  return ui;
}

function renderOffers() {
  view.render(state, offersUi());
}

const view = createView(document, app, {
  onRename: (name) => {
    if (session === null || !session.rename(name)) {
      view.announce("Non connesso: nome non inviato");
    }
  },
  onSelectFiles: (files, origin) => {
    if (offers === null) {
      return;
    }
    // One picker/drop action is one offer: the kind follows the selection
    // (one file → file, several flat files → files, folder button → folder),
    // never the button that opened the picker.
    const kind = origin === "folder" ? "folder" : files.length > 1 ? "files" : "file";
    const result = offers.prepareSelection({ kind, files });
    if (result.error) {
      view.announce(result.error);
      return;
    }
    renderOffers();
  },
  onWithdraw: (offerId) => {
    if (offers === null) {
      return;
    }
    const result = offers.withdrawOffer(offerId);
    if (result.error) {
      view.announce(result.error);
      return;
    }
    renderOffers();
  },
  onCopyLink: async () => {
    if (secrets === null || roomId === null) {
      return;
    }
    // Reconstructed ONLY here, inside the click handler, then discarded.
    const url = buildRoomUrl(window.location.origin, roomId, secrets);
    try {
      await navigator.clipboard.writeText(url);
      view.announce("Link copiato negli appunti");
    } catch {
      view.announce("Copia non riuscita");
    }
  },
});

function render() {
  view.render(state, offersUi());
}

function setConnection(connection, statusText) {
  state = reduce(state, { kind: "connection", connection, statusText });
  render();
}

function teardownUnavailable(statusText) {
  if (session !== null) {
    session.stop();
    session = null;
  }
  if (roomId !== null) {
    try {
      clearSecrets(window.sessionStorage, roomId);
    } catch {
      /* storage may be unavailable; nothing else to purge */
    }
    secrets = null;
  }
  setConnection(CONNECTION.UNAVAILABLE, statusText);
  view.announce(statusText);
}

function controlUrl() {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/transfer/ws/control/${roomId}`;
}

// Network-change recovery (2.6): a dead link is otherwise silent until the
// next heartbeat or close frame — on mobile networks that strands the room
// for a full cycle. Cycling drops the socket so the backoff redials at once;
// the reconnect then re-hellos and the snapshot path republishes.
window.addEventListener("offline", () => session?.cycle());
window.addEventListener("online", () => session?.cycle());

function startSession() {
  setConnection(CONNECTION.CONNECTING, "Connessione alla room…");
  offers = makeOffers();
  session = createControlSession({
    url: controlUrl(),
    memberToken: secrets.memberToken,
    displayName: null,
    events: {
      onMessage: (message) => {
        if (message?.type === "welcome" && message?.body?.limits) {
          serverLimits = message.body.limits;
          offers?.setServerLimits(serverLimits);
        }
        if (
          (message?.type === "ack" || message?.type === "error") &&
          offers?.handleReply(message.type, message.body, message.requestId ?? null)
        ) {
          renderOffers();
          return;
        }
        const event = messageToEvent(message);
        if (event === null) {
          return;
        }
        state = reduce(state, event);
        if (event.kind === "room.closed") {
          teardownUnavailable("Room non disponibile");
          return;
        }
        if (event.kind === "snapshot.end" && offers !== null) {
          // Republish after reconnect: absent IDs go now, IDs still held
          // by our ghost session wait for their `offer.removed` (2.6).
          const live = new Set(state.offers.keys());
          const { now, later } = partitionRepublish(offers.republishCandidates(), live);
          for (const id of now) {
            offers.republish(id);
          }
          for (const id of later) {
            deferredRepublish.add(id);
          }
        }
        if (event.kind === "offer.removed" && deferredRepublish.has(event.offerId)) {
          deferredRepublish.delete(event.offerId);
          offers?.republish(event.offerId);
        }
        render();
      },
      onHelloAck: () => {
        setConnection(CONNECTION.CONNECTED, "Connesso alla room");
      },
      onClose: (code, terminal) => {
        if (terminal) {
          teardownUnavailable("Room non disponibile");
          return;
        }
        setConnection(CONNECTION.RECONNECTING, "Riconnessione…");
      },
      onStateChange: (next) => {
        if (next === "reconnecting") {
          setConnection(CONNECTION.RECONNECTING, "Riconnessione…");
        }
      },
    },
  });
  session.start();
}

// Test-only introspection (same `__BORE_TEST__` pattern as the transfer
// note): exposes the live catalog for the e2e MAC cross-check. Absent
// without the hook; never used by production code paths.
if (typeof globalThis.__BORE_TEST__ === "object" && globalThis.__BORE_TEST__ !== null) {
  const hook = globalThis.__BORE_TEST__;
  hook.getCatalogSnapshot = () =>
    [...state.offers].map(([offerId, offer]) => ({
      offerId,
      peerId: offer.peerId,
      manifest: offer.manifest,
      mac: offer.mac,
    }));
  // 2.6 recorders: outbound control types, constructed socket URLs,
  // main-thread slice reads, RTC constructions, live transfer rows. Arrays
  // only — the app never reads them back.
  if (!Array.isArray(hook.outboundTypes)) {
    hook.outboundTypes = [];
  }
  if (!Array.isArray(hook.wsUrls)) {
    hook.wsUrls = [];
  }
  if (!Array.isArray(hook.fileReads)) {
    hook.fileReads = [];
  }
  if (typeof hook.rtcConstructed !== "number") {
    hook.rtcConstructed = 0;
  }
  if (typeof hook.transferRows !== "function") {
    hook.transferRows = () => document.querySelectorAll(".transfer-row").length;
  }
  const RealWebSocket = window.WebSocket;
  window.WebSocket = function (url, protocols) {
    hook.wsUrls.push(String(url));
    return new RealWebSocket(url, protocols);
  };
  if (window.RTCPeerConnection) {
    const RealRTC = window.RTCPeerConnection;
    window.RTCPeerConnection = function (...args) {
      hook.rtcConstructed += 1;
      return new RealRTC(...args);
    };
  }
  const origSlice = Blob.prototype.slice;
  Blob.prototype.slice = function (...args) {
    hook.fileReads.push({ size: this.size });
    return origSlice.apply(this, args);
  };
}

// Boot: fragment secrets move to sessionStorage and are scrubbed at once;
// a reload without fragment recovers from the same tab only.
try {
  const parsed = parseRoomUrl(window.location.href);
  roomId = parsed.roomId;
  secrets = { memberToken: parsed.memberToken, roomKey: parsed.roomKey };
  saveSecrets(window.sessionStorage, roomId, secrets);
  scrubFragment(window.history, `${window.location.origin}/transfer/${roomId}`);
} catch {
  const match = /^\/transfer\/([0-9a-f]{32})$/.exec(window.location.pathname);
  if (match !== null) {
    roomId = match[1];
    try {
      const loaded = loadSecrets(window.sessionStorage, roomId);
      if (loaded !== null) {
        secrets = loaded;
      }
    } catch {
      secrets = null;
    }
  }
}

if (roomId === null || secrets === null) {
  setConnection(CONNECTION.INCOMPLETE, "Link incompleto");
} else {
  startSession();
}
render();
