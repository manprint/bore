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
  validateCreatedAt,
  validateDisplayName,
  validateLabel,
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

  it("manifest_catalog_rules_reject_bad_labels_dates_ids_and_collisions", () => {
    const base = JSON.parse(fixture("manifest.json"));
    const withEntries = (entries) => ({ ...base, entries });
    const one = (entry) => withEntries([entry]);
    const fileEntry = {
      id: "0",
      path: "a.txt",
      size: "11",
      mtime: "1757779200",
      chunks: ["094c9eb526be7e2dea0b396331085eaba0d76f639116ccc014055c490b48daac"],
      chunkCount: "1",
      root: "4d2cfc38cbddafe4fdadf16cc2517f44f66b920939eab20bd4f7cc49dba9984e",
    };
    // Label / kind / chunkSize / createdAt.
    assert.throws(() => parseManifest({ ...base, label: "" }));
    assert.throws(() => parseManifest({ ...base, label: "x".repeat(129) }));
    assert.throws(() => parseManifest({ ...base, kind: "disk" }));
    assert.throws(() => parseManifest({ ...base, chunkSize: "512" }));
    assert.throws(() => parseManifest({ ...base, createdAt: "yesterday" }));
    assert.throws(() => parseManifest({ ...base, createdAt: "2026-13-01T00:00:00Z" }));
    validateCreatedAt("2026-09-14T21:00:00.123Z");
    assert.equal(validateLabel("  Demo  "), "Demo");
    // Entry IDs must be 0-based sequential.
    assert.throws(() => parseManifest(one({ ...fileEntry, id: "1" })));
    // chunkCount must equal the chunk hash count.
    assert.throws(() => parseManifest(one({ ...fileEntry, chunkCount: "2" })));
    // Null root needs an empty directory (and kind folder).
    assert.throws(() => parseManifest(one({ ...fileEntry, root: null })));
    const dirEntry = {
      id: "0",
      path: "docs",
      size: "0",
      mtime: "1757779200",
      chunks: [],
      chunkCount: "0",
      root: null,
    };
    assert.throws(() => parseManifest({ ...base, kind: "files", entries: [dirEntry] }));
    // Unsorted entries and NFC+casefold collisions.
    const second = { ...fileEntry, id: "1", path: "b.txt" };
    const bFirst = { ...second, id: "0" };
    const aSecond = { ...fileEntry, id: "1" };
    assert.throws(() => parseManifest(withEntries([bFirst, aSecond])));
    assert.throws(() => parseManifest(withEntries([fileEntry, { ...second, path: "A.TXT" }])));
    // Duplicate paths are not strictly sorted either.
    assert.throws(() => parseManifest(withEntries([fileEntry, { ...second, path: "a.txt" }])));
    // A non-canonical decimal is rejected.
    assert.throws(() => parseManifest(one({ ...fileEntry, size: "007" })));
    // Wrong root shape is rejected here; root VALUE equality against the
    // rolling root is verified by the server at publish (Rust
    // `manifest_rejects_noncanonical_decimal_and_wrong_root_shape`) and by
    // the browser at download — never in this sync parser.
    assert.throws(() => parseManifest(one({ ...fileEntry, root: "zz" })));
  });
});
