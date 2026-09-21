// Unit tests: persistent short room-link parsing and canonical rebuilding.
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import {
  buildShortRoomUrl,
  parseShortRoomUrl,
} from "../../src/secrets.js";
import {
  decodeRoomLinkSeed,
  encodeRoomLinkSeed,
} from "../../src/crypto.js";

const ORIGIN = "https://files.example";
const SEED_TEXT = "YVCYDRDYkjNIIyoIIHIH_w";
const SEED = decodeRoomLinkSeed(SEED_TEXT);
const GOOD = `${ORIGIN}/transfer/#${SEED_TEXT}`;
const LEGACY_ROOM_ID = "c5e230000f48c492799fe9ea32d18d8c";
const LEGACY_MEMBER = "a".repeat(64);
const LEGACY_KEY = "b".repeat(64);

describe("room links", () => {
  it("parses_and_builds_the_canonical_short_link", () => {
    assert.deepEqual(parseShortRoomUrl(GOOD), {
      seed: SEED,
      seedText: SEED_TEXT,
    });
    assert.equal(buildShortRoomUrl(`${ORIGIN}/`, SEED), GOOD);
    assert.equal(buildShortRoomUrl(`${ORIGIN}///`, SEED), GOOD);
    assert(!buildShortRoomUrl(`${ORIGIN}///`, SEED).includes("//transfer"));
    assert.equal(encodeRoomLinkSeed(SEED), SEED_TEXT);
  });

  it("rejects_legacy_forms_queries_and_noncanonical_seeds", () => {
    const bad = [
      `${ORIGIN}/transfer/${LEGACY_ROOM_ID}#m=${LEGACY_MEMBER}&k=${LEGACY_KEY}`,
      `${ORIGIN}/transfer/#m=${LEGACY_MEMBER}&k=${LEGACY_KEY}`,
      `${ORIGIN}/transfer/#${"!".repeat(22)}`,
      `${ORIGIN}/transfer/#${SEED_TEXT.slice(0, 21)}`,
      `${ORIGIN}/transfer/#${SEED_TEXT}x`,
      `${ORIGIN}/transfer/#${SEED_TEXT}=`,
      `${ORIGIN}/transfer/#${SEED_TEXT.slice(0, 21)}+`,
      `${ORIGIN}/transfer/#${SEED_TEXT.slice(0, 21)}/`,
      `${ORIGIN}/transfer/#${SEED_TEXT.slice(0, 21)}%20`,
      `${ORIGIN}/transfer/#${SEED_TEXT.slice(0, 21)}é`,
      `${ORIGIN}/transfer/#`,
      `${ORIGIN}/transfer/#${SEED_TEXT}?query=1`,
      `${ORIGIN}/transfer/?room=1#${SEED_TEXT}`,
      `${ORIGIN}/transfer/#${SEED_TEXT}&extra`,
      `${ORIGIN}/other/#${SEED_TEXT}`,
    ];
    for (const href of bad) {
      assert.throws(() => parseShortRoomUrl(href), undefined, href);
    }
  });

  it("does_not_export_the_legacy_storage_or_fragment_apis", async () => {
    const secrets = await import("../../src/secrets.js");
    for (const obsolete of [
      "SESSION_KEY_PREFIX",
      "storageKey",
      "saveSecrets",
      "loadSecrets",
      "clearSecrets",
      "scrubFragment",
      "parseRoomUrl",
      "buildRoomUrl",
    ]) {
      assert.equal(secrets[obsolete], undefined, obsolete);
    }
  });
});
