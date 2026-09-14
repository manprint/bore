// Room-URL secrets: parsing, session-scoped storage and fragment scrubbing.
//
// Pure by construction: every browser API is an injected dependency
// (`storage` with the sessionStorage shape, `history` with replaceState),
// so the contract is unit-tested in Node without a DOM. Callers must never
// copy these values anywhere else — no localStorage, IndexedDB, attributes,
// text nodes, exceptions or console output.

export const SESSION_KEY_PREFIX = "bore-transfer-v1:";

const ROOM_ID_RE = /^[0-9a-f]{32}$/;
const SECRET_RE = /^[0-9a-f]{64}$/;

/// Parses a room URL of the form `/transfer/<32hex>#m=<64hex>&k=<64hex>`.
/// The fragment holds exactly `m` and `k`, once each, lowercase hex only —
/// anything else (extras, duplicates, malformed values, missing parts)
/// throws, and the caller must show "Link incompleto" without contacting
/// the control plane.
export function parseRoomUrl(href) {
  const url = new URL(href, "http://room.invalid");
  const roomMatch = /^\/transfer\/([0-9a-f]{32})$/.exec(url.pathname);
  if (roomMatch === null) {
    throw new Error("room link must look like /transfer/<32hex>");
  }
  const roomId = roomMatch[1];
  if (!ROOM_ID_RE.test(roomId)) {
    throw new Error("room id must be canonical lowercase hex");
  }
  const raw = url.hash.startsWith("#") ? url.hash.slice(1) : url.hash;
  const seen = new Map();
  for (const part of raw.split("&")) {
    if (part === "") {
      continue;
    }
    const eq = part.indexOf("=");
    if (eq < 0) {
      throw new Error("room fragment must be m=<member>&k=<key>");
    }
    const key = part.slice(0, eq);
    const value = part.slice(eq + 1);
    if (key !== "m" && key !== "k") {
      throw new Error(`unknown room fragment key ${JSON.stringify(key)}`);
    }
    if (seen.has(key)) {
      throw new Error(`duplicate room fragment key ${JSON.stringify(key)}`);
    }
    seen.set(key, value);
  }
  const memberToken = seen.get("m");
  const roomKey = seen.get("k");
  if (
    typeof memberToken !== "string" ||
    !SECRET_RE.test(memberToken) ||
    typeof roomKey !== "string" ||
    !SECRET_RE.test(roomKey)
  ) {
    throw new Error("room fragment must carry 64-hex m and k");
  }
  return { roomId, memberToken, roomKey };
}

/// sessionStorage key for one room's secrets.
export function storageKey(roomId) {
  return `${SESSION_KEY_PREFIX}${roomId}`;
}

/// Saves both secrets under the room-scoped key. The only sanctioned store.
export function saveSecrets(storage, roomId, secrets) {
  storage.setItem(
    storageKey(roomId),
    JSON.stringify({ m: secrets.memberToken, k: secrets.roomKey }),
  );
}

/// Recovers both secrets saved by this tab earlier, or null when this tab
/// never saw the fragment (a fresh tab must not impersonate an old peer).
export function loadSecrets(storage, roomId) {
  const raw = storage.getItem(storageKey(roomId));
  if (raw === null || raw === undefined) {
    return null;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (
    parsed === null ||
    typeof parsed !== "object" ||
    typeof parsed.m !== "string" ||
    !SECRET_RE.test(parsed.m) ||
    typeof parsed.k !== "string" ||
    !SECRET_RE.test(parsed.k)
  ) {
    return null;
  }
  return { memberToken: parsed.m, roomKey: parsed.k };
}

/// Drops both secrets for one room (room close, invalid room, logout).
export function clearSecrets(storage, roomId) {
  storage.removeItem(storageKey(roomId));
}

/// Removes the fragment from the address bar right after the secrets move
/// to sessionStorage. The temporary clean URL never carries secret material.
export function scrubFragment(history, cleanHref) {
  history.replaceState(null, "", cleanHref);
}

/// Rebuilds the shareable room URL. Call ONLY inside the copy-link click
/// handler: the temporary string is written to the clipboard and discarded,
/// never rendered, stored or logged.
export function buildRoomUrl(origin, roomId, secrets) {
  const url = `${origin}/transfer/${roomId}#m=${secrets.memberToken}&k=${secrets.roomKey}`;
  return url;
}
