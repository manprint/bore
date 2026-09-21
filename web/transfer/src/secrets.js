// Room-link URL helpers. The fragment remains visible by design: it is the
// shareable capability and the only input from which browser memory derives
// the room credentials. No browser storage or history mutation belongs here.

import {
  decodeRoomLinkSeed,
  encodeRoomLinkSeed,
  ROOM_LINK_SEED_TEXT_BYTES,
} from "./crypto.js";

/// Parses the only supported room-link form `/transfer/#<22 Base64URL chars>`.
/// The fragment is intentionally left intact by callers: it is the durable
/// capability used after refresh and by the copy-link action.
export function parseShortRoomUrl(href) {
  if (typeof href !== "string" || /[\t\n\r ]/.test(href)) {
    throw new Error("short room link is invalid");
  }
  let url;
  try {
    url = new URL(href, "http://room.invalid");
  } catch {
    throw new Error("short room link is invalid");
  }
  if (
    url.pathname !== "/transfer/" ||
    url.search !== "" ||
    url.hash.length !== ROOM_LINK_SEED_TEXT_BYTES + 1 ||
    !url.hash.startsWith("#")
  ) {
    throw new Error("short room link is invalid");
  }
  const seedText = url.hash.slice(1);
  try {
    return { seed: decodeRoomLinkSeed(seedText), seedText };
  } catch {
    throw new Error("short room link is invalid");
  }
}

/// Builds the canonical short-link form without a query or secret material
/// outside the visible fragment.
export function buildShortRoomUrl(origin, seed) {
  return `${origin.replace(/\/+$/, "")}/transfer/#${encodeRoomLinkSeed(seed)}`;
}
