// Unit tests: encrypted-frame stream planning (pure, no WebCrypto).
import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { importAttemptAesKey, sealFrame } from "../../src/crypto.js";
import {
  CHUNK_BYTES,
  FRAGMENT_BYTES,
  MAX_MESSAGE_BYTES,
  chunkWindow,
  decodeFrameWithKey,
  fragmentWindow,
  finalPayload,
  planSend,
} from "../../src/framing.js";

describe("web-transfer framing", () => {
  it("chunk_windows_cover_the_entry_exactly", () => {
    assert.deepEqual(chunkWindow(11, 0), { offset: 0, length: 11 });
    const size = 2 * CHUNK_BYTES + 7;
    assert.deepEqual(chunkWindow(size, 0), { offset: 0, length: CHUNK_BYTES });
    assert.deepEqual(chunkWindow(size, 1), { offset: CHUNK_BYTES, length: CHUNK_BYTES });
    assert.deepEqual(chunkWindow(size, 2), { offset: 2 * CHUNK_BYTES, length: 7 });
    assert.throws(() => chunkWindow(size, 3));
    assert.throws(() => chunkWindow(-1, 0));
  });

  it("one_mib_chunk_fragments_stay_under_all_limits", () => {
    // A full 1 MiB chunk fragments into 43 pieces of at most 24 KiB; every
    // ciphertext message (16-byte header + fragment + 16-byte tag) stays
    // under the 32 KiB transport cap with room to spare.
    const fragments = fragmentWindow(0, CHUNK_BYTES);
    assert.equal(fragments.length, 43);
    let total = 0;
    for (const fragment of fragments) {
      assert.ok(fragment.length >= 1 && fragment.length <= FRAGMENT_BYTES);
      total += fragment.length;
      assert.ok(16 + fragment.length + 16 <= MAX_MESSAGE_BYTES);
    }
    assert.equal(total, CHUNK_BYTES);
    // Offsets are contiguous from the window start.
    assert.equal(fragments[0].offset, 0);
    for (let i = 1; i < fragments.length; i++) {
      assert.equal(fragments[i].offset, fragments[i - 1].offset + fragments[i - 1].length);
    }
    // Empty and tiny windows behave.
    assert.deepEqual(fragmentWindow(100, 0), []);
    assert.deepEqual(fragmentWindow(5, 7), [{ offset: 5, length: 7 }]);
    assert.throws(() => fragmentWindow(-1, 5));
  });

  it("final_payload_is_u64be_total", () => {
    assert.deepEqual([...finalPayload(0)], [0, 0, 0, 0, 0, 0, 0, 0]);
    assert.deepEqual([...finalPayload(67108864)], [0, 0, 0, 0, 4, 0, 0, 0]);
    assert.throws(() => finalPayload(-1));
  });

  it("decode_demands_the_exact_next_sequence", async () => {
    const raw = new Uint8Array(32).fill(5);
    const aes = await importAttemptAesKey(raw, ["encrypt", "decrypt"]);
    const first = await sealFrame(raw, 0, 1, new TextEncoder().encode("hi"));
    const second = await sealFrame(raw, 1, 1, new TextEncoder().encode("yo"));
    assert.equal((await decodeFrameWithKey(aes, first, 0)).seq, 0);
    // A gap is fatal even though the frame itself is authentic.
    await assert.rejects(decodeFrameWithKey(aes, second, 0));
    await assert.rejects(decodeFrameWithKey(aes, first, 1));
    assert.equal((await decodeFrameWithKey(aes, second, 1)).seq, 1);
  });

  it("plan_send_skips_verified_ranges_and_validates_bounds", () => {
    assert.deepEqual(planSend(3, []), { send: [0, 1, 2], rehashOnly: [] });
    assert.deepEqual(planSend(0, []), { send: [], rehashOnly: [] });
    assert.deepEqual(
      planSend(4, [
        [0, 2],
        [3, 4],
      ]),
      { send: [2], rehashOnly: [0, 1, 3] },
    );
    // Overlapping ranges coalesce; out-of-bounds and inverted ranges fail.
    assert.deepEqual(planSend(3, [[0, 2], [1, 3]]), { send: [], rehashOnly: [0, 1, 2] });
    assert.throws(() => planSend(3, [[0, 4]]));
    assert.throws(() => planSend(3, [[2, 1]]));
    assert.throws(() => planSend(3, [[-1, 1]]));
    assert.throws(() => planSend(3, [["a", 1]]));
  });
});
