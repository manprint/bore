// Unit tests: room-URL secrets (parse, store, recover, scrub, rebuild).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  SESSION_KEY_PREFIX,
  buildRoomUrl,
  clearSecrets,
  loadSecrets,
  parseRoomUrl,
  saveSecrets,
  scrubFragment,
  storageKey,
} from "../../src/secrets.js";

const ROOM = "0123456789abcdef0123456789abcdef";
const MEMBER = "a".repeat(64);
const KEY = "b".repeat(64);
const GOOD = `https://files.example/transfer/${ROOM}#m=${MEMBER}&k=${KEY}`;

function memStorage() {
  const map = new Map();
  return {
    getItem: (key) => (map.has(key) ? map.get(key) : null),
    setItem: (key, value) => map.set(key, String(value)),
    removeItem: (key) => map.delete(key),
    keys: () => [...map.keys()],
  };
}

function memHistory() {
  return {
    replaced: null,
    replaceState(_state, _title, url) {
      this.replaced = url;
    },
  };
}

describe("room secrets", () => {
  it("fragment_is_validated_stored_per_room_and_scrubbed", () => {
    const parsed = parseRoomUrl(GOOD);
    assert.equal(parsed.roomId, ROOM);
    assert.equal(parsed.memberToken, MEMBER);
    assert.equal(parsed.roomKey, KEY);
    assert.equal(storageKey(ROOM), `${SESSION_KEY_PREFIX}${ROOM}`);

    const storage = memStorage();
    saveSecrets(storage, ROOM, parsed);
    assert.deepEqual(storage.keys(), [storageKey(ROOM)]);

    const history = memHistory();
    scrubFragment(history, `https://files.example/transfer/${ROOM}`);
    assert.equal(history.replaced, `https://files.example/transfer/${ROOM}`);
    assert.ok(!history.replaced.includes("#"));
    assert.ok(!history.replaced.includes(MEMBER));
    assert.ok(!history.replaced.includes(KEY));
  });

  it("reload_recovers_only_same_tab_secrets", () => {
    const storage = memStorage();
    assert.equal(loadSecrets(storage, ROOM), null);
    saveSecrets(storage, ROOM, { memberToken: MEMBER, roomKey: KEY });
    assert.deepEqual(loadSecrets(storage, ROOM), { memberToken: MEMBER, roomKey: KEY });
    // Another room and a fresh tab recover nothing.
    assert.equal(loadSecrets(storage, "f".repeat(32)), null);
    assert.equal(loadSecrets(memStorage(), ROOM), null);
    // Corrupt or misshapen entries recover nothing (never throw).
    storage.setItem(storageKey(ROOM), "not-json");
    assert.equal(loadSecrets(storage, ROOM), null);
    storage.setItem(storageKey(ROOM), JSON.stringify({ m: "short", k: KEY }));
    assert.equal(loadSecrets(storage, ROOM), null);
    // Clearing drops the room silently, twice.
    clearSecrets(storage, ROOM);
    clearSecrets(storage, ROOM);
    assert.equal(loadSecrets(storage, ROOM), null);
  });

  it("copy_link_reconstructs_only_inside_user_action", () => {
    const rebuilt = buildRoomUrl("https://files.example", ROOM, {
      memberToken: MEMBER,
      roomKey: KEY,
    });
    assert.equal(rebuilt, GOOD);
    // Round-trips through the validator (what the next tab parses).
    assert.deepEqual(parseRoomUrl(rebuilt), { roomId: ROOM, memberToken: MEMBER, roomKey: KEY });
  });

  it("no_secret_is_rendered_logged_or_persisted_elsewhere", () => {
    const storage = memStorage();
    saveSecrets(storage, ROOM, { memberToken: MEMBER, roomKey: KEY });
    // Exactly one key, room-scoped; the value carries only m/k.
    assert.deepEqual(storage.keys(), [`${SESSION_KEY_PREFIX}${ROOM}`]);
    const stored = JSON.parse(storage.getItem(storageKey(ROOM)));
    assert.deepEqual(Object.keys(stored).sort(), ["k", "m"]);
    // Storage keys never embed secret material.
    assert.ok(!storageKey(ROOM).includes(MEMBER));
    assert.ok(!storageKey(ROOM).includes(KEY));
  });

  it("malformed_links_throw_without_side_effects", () => {
    const bad = [
      "https://files.example/transfer/short#m=1&k=2",
      `https://files.example/transfer/${ROOM.toUpperCase()}#m=${MEMBER}&k=${KEY}`,
      `https://files.example/transfer/${ROOM}#m=${MEMBER}`,
      `https://files.example/transfer/${ROOM}#m=${MEMBER}&k=${KEY}&x=1`,
      `https://files.example/transfer/${ROOM}#m=${MEMBER}&m=${MEMBER}&k=${KEY}`,
      `https://files.example/transfer/${ROOM}#m=${"Z".repeat(64)}&k=${KEY}`,
      `https://files.example/transfer/${ROOM}#k=${KEY}`,
      `https://files.example/transfer/${ROOM}`,
      "https://files.example/other",
    ];
    for (const href of bad) {
      assert.throws(() => parseRoomUrl(href), undefined, href);
    }
  });
});
