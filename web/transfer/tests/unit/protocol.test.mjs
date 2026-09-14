// Protocol unit tests: canonical JSON, manifest validation, control
// envelopes. Mirror of the Rust codec tests; both pin the same fixtures.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  canonicalize,
  manifestValue,
  parseClientEnvelope,
  parseManifest,
  parseRelayAttach,
  parseServerEnvelope,
  validateDisplayName,
  validateManifestPath,
} from "../../src/protocol.js";

const fixtureDir = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "..",
  "tests",
  "fixtures",
  "web_transfer",
  "v1",
);
const fixture = (name) => readFileSync(join(fixtureDir, name), "utf8");

describe("web-transfer protocol", () => {
  it("canonical_manifest_fixture_matches_byte_for_byte", () => {
    const manifest = parseManifest(JSON.parse(fixture("manifest.json")));
    const canonical = canonicalize(manifestValue(manifest));
    assert.equal(canonical, fixture("manifest.canonical.json"));
  });

  it("control_fixture_round_trips_without_unknown_fields", () => {
    const messages = JSON.parse(fixture("control-messages.json"));
    assert.ok(messages.length >= 38, "fixture covers every control type");
    for (const { direction, json } of messages) {
      if (direction === "relay") {
        parseRelayAttach(json);
        continue;
      }
      const parse = direction === "client" ? parseClientEnvelope : parseServerEnvelope;
      const env = parse(json);
      const recanonical = canonicalize(JSON.parse(json));
      assert.deepEqual(parse(recanonical), env);
    }
    assert.throws(() =>
      parseClientEnvelope('{"v":1,"type":"ping","body":{},"extra":1}'),
    );
    assert.throws(() => parseClientEnvelope('{"v":1,"type":"nope","body":{}}'));
    assert.throws(() =>
      parseClientEnvelope('{"v":1,"type":"transfer.request","body":{}}'),
    );
    assert.throws(() => parseServerEnvelope('{"v":2,"type":"pong","body":{}}'));
  });

  it("manifest_path_rules_reject_nonportable_entries", () => {
    for (const bad of [
      "",
      "/abs",
      "trailing/",
      "back\\slash",
      "dot/./seg",
      "up/../seg",
      "double//slash",
      "café".normalize("NFD"),
      `${"s".repeat(256)}/f`,
    ]) {
      assert.throws(() => validateManifestPath(bad), undefined, bad);
    }
    validateManifestPath("dir/hello.txt");
    assert.throws(() => validateDisplayName(""));
    assert.throws(() => validateDisplayName("x".repeat(49)));
    validateDisplayName("Bobi");
  });
});
