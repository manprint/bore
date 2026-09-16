// Application state: one small reducer over control-plane events.
//
// Pure by construction (no DOM, no sockets, no storage): the browser test
// hook in 2.6 and the Node unit tests drive exactly these transitions.
// Remote strings (names, labels, paths) are data here; only view.js may
// place them into the DOM, and only as text nodes.

/// Connection lifecycle of the control channel.
export const CONNECTION = {
  CONNECTING: "connecting",
  CONNECTED: "connected",
  RECONNECTING: "reconnecting",
  UNAVAILABLE: "unavailable",
  INCOMPLETE: "incomplete",
};

/// Lifecycle of one transfer row (3.5). `verified` means the bytes are on
/// disk and wait for an explicit save; nothing here ever starts I/O.
export const TRANSFER = {
  REQUESTING: "requesting",
  TRANSFERRING: "transferring",
  VERIFIED: "verified",
  DONE: "done",
  CANCELLED: "cancelled",
  FAILED: "failed",
};

/// Transport a transfer's row may show. `CONNECTING` is not a transport: it
/// is the honest answer while the path is being negotiated, and the row keeps
/// it until the RECIPIENT has verified a chunk over the transport that was
/// committed. A committed path that has carried nothing is not yet a fact.
export const PATH = {
  CONNECTING: "connecting",
  DIRECT: "direct",
  RELAY: "relay",
};

/// Terminal states: no cancel, no progress, no bytes in flight.
const TRANSFER_TERMINAL = new Set([
  TRANSFER.VERIFIED,
  TRANSFER.DONE,
  TRANSFER.CANCELLED,
  TRANSFER.FAILED,
]);

/// Rows kept after they went terminal (oldest dropped first). Bounded so a
/// long-lived tab cannot grow the table without limit.
const TRANSFER_ROW_BUDGET = 20;

export function createInitialState() {
  return {
    connection: CONNECTION.CONNECTING,
    statusText: "",
    selfPeerId: null,
    displayName: null,
    peers: new Map(),
    offers: new Map(),
    // transferId → row (see `transfer.started`); insertion ordered.
    transfers: new Map(),
    // Offer IDs with a partial staged on disk: shown as resumable, never
    // resumed on their own.
    resumable: new Set(),
    // Offer IDs whose SOURCE no longer produces the bytes a verified
    // partial holds. The partial is kept — it is what this peer verified —
    // and the only way past it is an explicit restart from zero, so this
    // set exists to make that gesture available and nothing else.
    sourceChanged: new Set(),
    closed: false,
  };
}

function sortedEntries(map) {
  return [...map.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

/// Folds one normalized event into state. Events come from
/// `messageToEvent`; unknown shapes are ignored (never crash on remote
/// input). A fresh `snapshot.begin` clears all remote state first, so a
/// reconnect can never merge stale peers/offers with new ones. Ordering is
/// applied at render time via `sortedEntries`, never by trusting arrival
/// order here.
export function reduce(state, event) {
  switch (event.kind) {
    case "connection": {
      return { ...state, connection: event.connection, statusText: event.statusText ?? state.statusText };
    }
    case "welcome": {
      return {
        ...state,
        selfPeerId: event.peerId,
        displayName: event.displayName,
      };
    }
    case "snapshot.begin": {
      return { ...state, peers: new Map(), offers: new Map() };
    }
    case "snapshot.peer":
    case "peer.joined": {
      const peers = new Map(state.peers);
      peers.set(event.peerId, { displayName: event.displayName ?? null });
      return { ...state, peers };
    }
    case "peer.renamed": {
      if (!state.peers.has(event.peerId)) {
        return state;
      }
      const peers = new Map(state.peers);
      peers.set(event.peerId, { displayName: event.displayName ?? null });
      return { ...state, peers };
    }
    case "peer.left": {
      if (!state.peers.has(event.peerId)) {
        return state;
      }
      const peers = new Map(state.peers);
      peers.delete(event.peerId);
      const offers = new Map(state.offers);
      for (const [offerId, offer] of offers) {
        if (offer.peerId === event.peerId) {
          offers.delete(offerId);
        }
      }
      return { ...state, peers, offers };
    }
    case "snapshot.offer":
    case "offer.added": {
      const offers = new Map(state.offers);
      offers.set(event.offerId, {
        peerId: event.peerId,
        manifest: event.manifest,
        mac: event.mac,
      });
      return { ...state, offers };
    }
    case "offer.removed": {
      if (!state.offers.has(event.offerId)) {
        return state;
      }
      const offers = new Map(state.offers);
      offers.delete(event.offerId);
      return { ...state, offers };
    }
    case "transfer.started": {
      const transfers = new Map(state.transfers);
      // A new attempt on the same offer replaces the finished rows of that
      // offer, so a resume reads as one row moving, not a growing history.
      for (const [id, row] of transfers) {
        if (row.offerId === event.offerId && TRANSFER_TERMINAL.has(row.state)) {
          transfers.delete(id);
        }
      }
      transfers.set(event.transferId, {
        transferId: event.transferId,
        offerId: event.offerId,
        direction: event.direction,
        sourcePeerId: event.sourcePeerId,
        recipientPeerId: event.recipientPeerId,
        label: event.label ?? null,
        totalBytes: event.totalBytes ?? 0,
        doneBytes: 0,
        bytesPerSecond: null,
        path: event.path ?? PATH.CONNECTING,
        state: event.state ?? TRANSFER.REQUESTING,
        code: null,
      });
      while (transfers.size > TRANSFER_ROW_BUDGET) {
        const oldest = [...transfers].find(([, row]) => TRANSFER_TERMINAL.has(row.state));
        if (oldest === undefined) {
          break;
        }
        transfers.delete(oldest[0]);
      }
      return { ...state, transfers };
    }
    case "transfer.progress": {
      const row = state.transfers.get(event.transferId);
      if (row === undefined || TRANSFER_TERMINAL.has(row.state)) {
        return state;
      }
      const transfers = new Map(state.transfers);
      transfers.set(event.transferId, {
        ...row,
        state: TRANSFER.TRANSFERRING,
        // MONOTONIC across attempts: a fallback restarts the wire counter at
        // zero, and a row that walked backwards would read as lost progress
        // when the verified chunks are still on disk.
        doneBytes: Math.max(row.doneBytes, event.doneBytes ?? row.doneBytes),
        totalBytes: event.totalBytes ?? row.totalBytes,
        bytesPerSecond: event.bytesPerSecond ?? row.bytesPerSecond,
      });
      return { ...state, transfers };
    }
    case "transfer.path": {
      const row = state.transfers.get(event.transferId);
      if (row === undefined || TRANSFER_TERMINAL.has(row.state)) {
        return state;
      }
      if (event.path !== PATH.DIRECT && event.path !== PATH.RELAY) {
        // Only a real transport may be declared here: nothing walks a row
        // back to `connecting` once bytes have been verified on a path.
        return state;
      }
      if (row.path === event.path) {
        return state;
      }
      const transfers = new Map(state.transfers);
      transfers.set(event.transferId, { ...row, path: event.path });
      return { ...state, transfers };
    }
    case "transfer.path_reset": {
      // The ONE way a row walks back to `connecting`, and it is local: no
      // remote message maps to this kind (see `messageToEvent`). The direct
      // attempt this row had committed to is over, so the badge would
      // otherwise keep naming a transport that is no longer carrying
      // anything — a claim the whole path contract exists to refuse. The
      // replacement attempt puts a word back only once the recipient has
      // verified a chunk on it.
      const row = state.transfers.get(event.transferId);
      if (
        row === undefined ||
        TRANSFER_TERMINAL.has(row.state) ||
        row.path === PATH.CONNECTING
      ) {
        return state;
      }
      const transfers = new Map(state.transfers);
      transfers.set(event.transferId, { ...row, path: PATH.CONNECTING });
      return { ...state, transfers };
    }
    case "transfer.state": {
      const row = state.transfers.get(event.transferId);
      if (row === undefined) {
        return state;
      }
      const transfers = new Map(state.transfers);
      transfers.set(event.transferId, {
        ...row,
        state: event.state,
        code: event.code ?? null,
        doneBytes:
          event.state === TRANSFER.DONE || event.state === TRANSFER.VERIFIED
            ? row.totalBytes
            : row.doneBytes,
      });
      return { ...state, transfers };
    }
    case "transfer.removed": {
      if (!state.transfers.has(event.transferId)) {
        return state;
      }
      const transfers = new Map(state.transfers);
      transfers.delete(event.transferId);
      return { ...state, transfers };
    }
    case "transfer.resumable": {
      return { ...state, resumable: new Set(event.offerIds ?? []) };
    }
    case "transfer.source_changed": {
      const sourceChanged = new Set(state.sourceChanged);
      if (event.changed === false) {
        sourceChanged.delete(event.offerId);
      } else {
        sourceChanged.add(event.offerId);
      }
      return { ...state, sourceChanged };
    }
    case "room.closed": {
      // Room close is authoritative: rows, partial markers and everything
      // they could offer to click go at once.
      return {
        ...state,
        closed: true,
        connection: CONNECTION.UNAVAILABLE,
        transfers: new Map(),
        resumable: new Set(),
        sourceChanged: new Set(),
      };
    }
    default:
      return state;
  }
}

/// Maps one parsed server control message `{type, body}` to a reducer
/// event, or null when the message carries no state (ack/error/pong) or is
/// unknown. Never throws on remote input.
export function messageToEvent(message) {
  if (message === null || typeof message !== "object") {
    return null;
  }
  const { type, body } = message;
  if (typeof type !== "string" || body === null || typeof body !== "object") {
    return null;
  }
  switch (type) {
    case "welcome":
      if (typeof body.peerId !== "string" || typeof body.roomId !== "string") {
        return null;
      }
      return {
        kind: "welcome",
        peerId: body.peerId,
        displayName: typeof body.displayName === "string" ? body.displayName : null,
      };
    case "snapshot.begin":
      return { kind: "snapshot.begin" };
    case "snapshot.peer":
      if (typeof body.peerId !== "string") {
        return null;
      }
      return {
        kind: "snapshot.peer",
        peerId: body.peerId,
        displayName: typeof body.displayName === "string" ? body.displayName : null,
      };
    case "snapshot.offer":
      if (typeof body.peerId !== "string" || typeof body.offerId !== "string") {
        return null;
      }
      return {
        kind: "snapshot.offer",
        peerId: body.peerId,
        offerId: body.offerId,
        manifest: body.manifest ?? null,
        mac: typeof body.mac === "string" ? body.mac : null,
      };
    case "snapshot.end":
      return { kind: "snapshot.end" };
    case "peer.joined":
      if (typeof body.peerId !== "string") {
        return null;
      }
      return {
        kind: "peer.joined",
        peerId: body.peerId,
        displayName: typeof body.displayName === "string" ? body.displayName : null,
      };
    case "peer.renamed":
      if (typeof body.peerId !== "string" || typeof body.displayName !== "string") {
        return null;
      }
      return { kind: "peer.renamed", peerId: body.peerId, displayName: body.displayName };
    case "peer.left":
      if (typeof body.peerId !== "string") {
        return null;
      }
      return { kind: "peer.left", peerId: body.peerId };
    case "offer.added":
      if (typeof body.peerId !== "string" || typeof body.offerId !== "string") {
        return null;
      }
      return {
        kind: "offer.added",
        peerId: body.peerId,
        offerId: body.offerId,
        manifest: body.manifest ?? null,
        mac: typeof body.mac === "string" ? body.mac : null,
      };
    case "offer.removed":
      if (typeof body.peerId !== "string" || typeof body.offerId !== "string") {
        return null;
      }
      return { kind: "offer.removed", peerId: body.peerId, offerId: body.offerId };
    case "room_closed":
      return { kind: "room.closed" };
    default:
      return null;
  }
}

export { sortedEntries };

/// Splits republish candidates against the live catalog: absent IDs publish
/// now; IDs still held by a ghost session wait for their `offer.removed`
/// (the server reaps a dead session within its liveness window, so the wait
/// always ends — and publishing into a live ghost would conflict).
export function partitionRepublish(candidates, liveOfferIds) {
  const now = [];
  const later = [];
  for (const id of candidates) {
    if (liveOfferIds.has(id)) {
      later.push(id);
    } else {
      now.push(id);
    }
  }
  return { now, later };
}

/// Offer IDs worth republishing after a reconnect: local offers still backed
/// by an in-memory `File` (or directory handle) AND already acknowledged by
/// the server before the drop. Anything else is forgotten, never resent —
/// and a republish is an `offer.publish`, never a `transfer.request`.
export function selectRepublishCandidates(localOffers, ackedOfferIds) {
  const candidates = [];
  for (const [offerId, entry] of localOffers) {
    if (entry !== null && entry !== undefined && ackedOfferIds.has(offerId)) {
      candidates.push(offerId);
    }
  }
  return candidates;
}

/// True when `selfPeerId` may cancel this row: only the two participants,
/// and only while the transfer is live. A third peer never gets the button,
/// and the server refuses the message as well (`NOT_PARTICIPANT`).
export function canCancelTransfer(row, selfPeerId) {
  if (row === null || row === undefined || typeof selfPeerId !== "string") {
    return false;
  }
  if (TRANSFER_TERMINAL.has(row.state)) {
    return false;
  }
  return row.sourcePeerId === selfPeerId || row.recipientPeerId === selfPeerId;
}

/// Verified percentage, 0..100, clamped and integer (0 when size unknown).
export function transferPercent(row) {
  const total = Number(row?.totalBytes ?? 0);
  const done = Number(row?.doneBytes ?? 0);
  if (!Number.isFinite(total) || total <= 0) {
    return 0;
  }
  return Math.max(0, Math.min(100, Math.floor((done / total) * 100)));
}

/// Stable Italian text for one protocol error code. The message NEVER
/// includes server-supplied text: an unknown code reads as a generic
/// failure, so a hostile or buggy peer cannot put its own words on screen.
const ERROR_TEXT = new Map([
  ["UNSUPPORTED_VERSION", "Versione non supportata: aggiorna la pagina"],
  ["ROOM_UNAVAILABLE", "Room non disponibile"],
  ["UNAUTHORIZED", "Accesso non autorizzato"],
  ["INVALID_MESSAGE", "Messaggio non valido"],
  ["RATE_LIMITED", "Troppe richieste: riprova tra poco"],
  ["LIMIT_EXCEEDED", "Limite della room raggiunto"],
  ["OFFER_NOT_FOUND", "Offerta non più disponibile"],
  ["OFFER_CHANGED", "Offerta cambiata: ripubblica il file"],
  ["TRANSFER_NOT_FOUND", "Trasferimento non più attivo"],
  ["NOT_PARTICIPANT", "Non partecipi a questo trasferimento"],
  ["SOURCE_OFFLINE", "Sorgente non raggiungibile"],
  ["SOURCE_CHANGED", "File modificato alla sorgente"],
  ["DIRECT_FAILED", "Percorso interrotto: riprova"],
  ["RELAY_BUSY", "Relay occupato: riprova tra poco"],
  ["STORAGE_QUOTA", "Spazio su disco insufficiente"],
  ["CANCELLED", "Trasferimento annullato"],
  ["INTERNAL", "Errore del server"],
  ["OFFLINE", "Non connesso alla room"],
  ["UNSUPPORTED", "Download non supportato da questo browser"],
  ["OWN_OFFER", "Questa offerta è tua"],
  ["MANIFEST_MAC", "Offerta non autentica: ignorata"],
  ["MULTI_ENTRY", "Offerta con più file: scegli un file o scarica lo ZIP"],
  ["FAILED", "Trasferimento non riuscito"],
]);

export function errorText(code) {
  if (typeof code !== "string") {
    return "Trasferimento non riuscito";
  }
  return ERROR_TEXT.get(code) ?? "Trasferimento non riuscito";
}
