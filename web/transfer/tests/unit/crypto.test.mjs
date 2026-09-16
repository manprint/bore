// Crypto unit tests: roots, keys, nonces and encrypted frames against the
// shared fixtures. Mirror of the Rust codec tests.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import {
  FINAL_ARCHIVE_BYTES,
  FINAL_RAW_BYTES,
  attemptKey,
  bytesToHex,
  fileRoot,
  frameNonce,
  hexToBytes,
  hmacSign,
  hmacVerify,
  importAttemptAesKey,
  manifestKey,
  openFrame,
  openFrameWithKey,
  sealFrame,
  sealFrameWithKey,
  sha256Hex,
} from "../../src/crypto.js";

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
const vectors = () => JSON.parse(fixture("crypto-vectors.json"));

describe("web-transfer crypto", () => {
  it("manifest_hmac_fixture_matches", async () => {
    const { inputs, expected } = vectors();
    const key = await manifestKey(
      hexToBytes(inputs.room_key),
      hexToBytes(inputs.room_id),
    );
    assert.equal(bytesToHex(key), expected.manifest_key_hex);
    const canonical = new TextEncoder().encode(
      fixture("manifest.canonical.json"),
    );
    const mac = await hmacSign(key, canonical);
    assert.equal(bytesToHex(mac), expected.manifest_mac_hex);
    assert.ok(await hmacVerify(key, canonical, mac));
    const bad = mac.slice();
    bad[0] ^= 1;
    assert.equal(await hmacVerify(key, canonical, bad), false);
  });

  it("file_root_empty_single_and_multichunk_vectors_match", async () => {
    const { inputs, expected } = vectors();
    assert.equal(bytesToHex(await fileRoot(0, [])), expected.root_empty_hex);
    const leafHello = hexToBytes(
      await sha256Hex(hexToBytes(inputs.plaintext_hex)),
    );
    assert.equal(bytesToHex(leafHello), expected.leaf_hello_hex);
    assert.equal(
      bytesToHex(await fileRoot(1, [leafHello])),
      expected.root_single_hex,
    );
    const leaves = [];
    for (const seed of inputs.multichunk_seeds) {
      leaves.push(hexToBytes(await sha256Hex(new TextEncoder().encode(seed))));
    }
    assert.deepEqual(leaves.map(bytesToHex), expected.multichunk_leaves_hex);
    assert.equal(
      bytesToHex(await fileRoot(3, leaves)),
      expected.root_multi_hex,
    );
    await assert.rejects(fileRoot(2, leaves));
  });

  it("attempt_key_nonce_and_ciphertext_match_fixture", async () => {
    const { inputs, expected } = vectors();
    const raw = await attemptKey(
      hexToBytes(inputs.room_key),
      hexToBytes(inputs.transfer_id),
      hexToBytes(inputs.attempt_id),
    );
    assert.equal(bytesToHex(raw), expected.attempt_key_hex);
    assert.equal(bytesToHex(frameNonce(7)), expected.nonce_seq7_hex);
    assert.notDeepEqual(frameNonce(7), frameNonce(8));
    // The sender imports the bytes once as a non-extractable encrypt-only
    // key; sealing through the handle is byte-identical to the fixture.
    const handle = await importAttemptAesKey(raw);
    assert.equal(handle.extractable, false);
    assert.deepEqual(handle.usages, ["encrypt"]);
    await assert.rejects(importAttemptAesKey(new Uint8Array(31)));
    const plaintext = hexToBytes(inputs.plaintext_hex);
    const total = new Uint8Array(8);
    new DataView(total.buffer).setBigUint64(0, BigInt(plaintext.length), false);
    let lastSealed = null;
    for (const [ftype, body, name] of [
      [1, plaintext, "frame_data_seq7_hex"],
      [2, total, "frame_final_seq7_hex"],
    ]) {
      const viaHandle = await sealFrameWithKey(handle, inputs.seq, ftype, body);
      assert.equal(bytesToHex(viaHandle), expected[name]);
      const decoded = await openFrame(raw, viaHandle, 0);
      assert.deepEqual(decoded.plaintext, body);
      lastSealed = viaHandle;
    }
    await assert.rejects(sealFrameWithKey(handle, 0, 1, new Uint8Array(0)));
    await assert.rejects(sealFrameWithKey(handle, 0, 9, new Uint8Array(1)));
    // The handle opens what the bytes open (and rejects what they reject).
    const opener = await importAttemptAesKey(raw, ["decrypt"]);
    assert.deepEqual(opener.usages, ["decrypt"]);
    const reopened = await openFrameWithKey(opener, lastSealed, inputs.seq);
    assert.deepEqual(reopened.plaintext, total);
    await assert.rejects(openFrameWithKey(opener, lastSealed, inputs.seq + 1));
  });

  it("encrypted_frames_round_trip_for_every_type", async () => {
    const { inputs, expected } = vectors();
    const key = hexToBytes(expected.attempt_key_hex);
    const plaintext = hexToBytes(inputs.plaintext_hex);
    const total = new Uint8Array(8);
    new DataView(total.buffer).setBigUint64(0, BigInt(plaintext.length), false);
    for (const [ftype, body, name] of [
      [1, plaintext, "frame_data_seq7_hex"],
      [2, total, "frame_final_seq7_hex"],
    ]) {
      const msg = await sealFrame(key, inputs.seq, ftype, body);
      const view = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
      assert.equal(view.getUint32(0, false), 0x42575431);
      assert.equal(view.getUint16(4, false), 1);
      assert.equal(msg[6], ftype);
      assert.equal(msg[7], 0);
      assert.equal(view.getUint32(8, false), inputs.seq);
      const decoded = await openFrame(key, msg, 0);
      assert.equal(decoded.ftype, ftype);
      assert.equal(decoded.seq, inputs.seq);
      assert.deepEqual(decoded.plaintext, body);
      assert.equal(bytesToHex(msg), expected[name]);
    }
  });

  it("wrong_key_modified_aad_reserved_bits_and_reused_sequence_are_rejected", async () => {
    const key = new Uint8Array(32).fill(9);
    const msg = await sealFrame(key, 3, 1, new TextEncoder().encode("payload"));
    const wrong = key.slice();
    wrong[0] ^= 1;
    await assert.rejects(openFrame(wrong, msg, 0));
    const tampered = msg.slice();
    tampered[8] ^= 1;
    await assert.rejects(openFrame(key, tampered, 0));
    const flagged = msg.slice();
    flagged[7] = 1;
    await assert.rejects(openFrame(key, flagged, 0));
    await assert.rejects(openFrame(key, msg, 4));
    await openFrame(key, msg, 3);
  });

  it("final_frames_carry_the_raw_total_or_the_archive_tuple_and_nothing_else", async () => {
    // A FINAL frame carries ONE of two shapes, and no third: `u64be(total)`
    // for a raw transfer, or the archive tuple for a `zip` one. The archive
    // shape exists because none of its three quantities is in the manifest
    // — the archive is generated, so this frame is where the recipient
    // learns what it should have received.
    const key = new Uint8Array(32).fill(7);
    assert.equal(FINAL_RAW_BYTES, 8);
    assert.equal(FINAL_ARCHIVE_BYTES, 48);
    const handle = await importAttemptAesKey(key, ["encrypt"]);
    for (const length of [FINAL_RAW_BYTES, FINAL_ARCHIVE_BYTES]) {
      const sealed = await sealFrame(key, 0, 2, new Uint8Array(length));
      assert.equal((await openFrame(key, sealed, 0)).plaintext.length, length);
      // The two sealers agree, here as everywhere.
      const viaHandle = await sealFrameWithKey(
        handle,
        0,
        2,
        new Uint8Array(length),
      );
      assert.deepEqual([...viaHandle], [...sealed]);
    }
    // Every other length is refused, one byte either way around each shape.
    for (const length of [0, 7, 9, 16, 47, 49, 64]) {
      await assert.rejects(sealFrame(key, 0, 2, new Uint8Array(length)));
      await assert.rejects(
        sealFrameWithKey(handle, 0, 2, new Uint8Array(length)),
      );
    }
  });

  it("decoder_rejects_oversized_fragment_integer_overflow_unknown_type_and_trailing_bytes", async () => {
    const key = new Uint8Array(32).fill(4);
    await assert.rejects(sealFrame(key, 0, 1, new Uint8Array(0)));
    await assert.rejects(sealFrame(key, 0, 1, new Uint8Array(24 * 1024 + 1)));
    await assert.rejects(
      sealFrame(key, 0, 2, new TextEncoder().encode("1234567")),
    );
    let msg = await sealFrame(key, 0, 1, new TextEncoder().encode("hi"));
    msg = msg.slice();
    msg[6] = 9;
    await assert.rejects(openFrame(key, msg, 0));
    msg = await sealFrame(key, 0, 1, new TextEncoder().encode("hi"));
    msg = msg.slice();
    new DataView(msg.buffer).setUint32(12, msg.length - 16 + 16, false);
    await assert.rejects(openFrame(key, msg, 0));
    msg = await sealFrame(key, 0, 1, new TextEncoder().encode("hi"));
    const tailed = new Uint8Array(msg.length + 4);
    tailed.set(msg, 0);
    await assert.rejects(openFrame(key, tailed, 0));
    msg = (await sealFrame(key, 0, 1, new TextEncoder().encode("hi"))).slice();
    new DataView(msg.buffer).setUint32(12, 0xffffffff, false);
    await assert.rejects(openFrame(key, msg, 0));
  });
});
