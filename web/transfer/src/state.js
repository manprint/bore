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

export function createInitialState() {
  return {
    connection: CONNECTION.CONNECTING,
    statusText: "",
    selfPeerId: null,
    displayName: null,
    peers: new Map(),
    offers: new Map(),
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
    case "room.closed": {
      return { ...state, closed: true, connection: CONNECTION.UNAVAILABLE };
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
