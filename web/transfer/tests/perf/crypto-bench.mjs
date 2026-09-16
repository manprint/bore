// T-WEB-PERF, `crypto` arm: what the shipped AES-GCM framing and the chunk
// digest cost, with no network and no storage in the way.
//
// This arm exists to make the browser number readable. The end-to-end rate is
// bounded by the smaller of the transport (the `pipe` arm) and the CPU (this
// one); measured separately, a slow run says WHICH of the two moved.
import { webcrypto } from "node:crypto";
import {
  importAttemptAesKey,
  sealFrameWithKey,
  openFrameWithKey,
  sha256Hex,
} from "../../src/crypto.js";
import { CHUNK_BYTES, FRAGMENT_BYTES } from "../../src/framing.js";
import { benchLine, throughputMiBs, benchConfig } from "./report.mjs";

if (!globalThis.crypto) {
  globalThis.crypto = webcrypto;
}

const { sizes, reps } = benchConfig();
const MIB = 1024 * 1024;

/// Seals `bytes` in product-sized fragments and returns the frames plus the
/// elapsed milliseconds. The plaintext is generated once, outside the timed
/// region: this arm measures the cipher, not the allocator.
async function sealAll(key, plaintext) {
  const frames = [];
  const started = performance.now();
  let seq = 0;
  for (let at = 0; at < plaintext.length; at += FRAGMENT_BYTES) {
    const fragment = plaintext.subarray(at, Math.min(at + FRAGMENT_BYTES, plaintext.length));
    frames.push(await sealFrameWithKey(key, seq, 1, fragment));
    seq += 1;
  }
  return { frames, ms: performance.now() - started };
}

async function openAll(key, frames) {
  const started = performance.now();
  let seq = 0;
  let opened = 0;
  for (const frame of frames) {
    const plain = await openFrameWithKey(key, frame, seq);
    opened += plain.plaintext.length;
    seq += 1;
  }
  return { opened, ms: performance.now() - started };
}

/// The receiver hashes every 1 MiB chunk before it accepts it: that digest is
/// part of the download's cost and belongs in the same picture.
async function digestChunks(plaintext) {
  const started = performance.now();
  for (let at = 0; at < plaintext.length; at += CHUNK_BYTES) {
    await sha256Hex(plaintext.subarray(at, Math.min(at + CHUNK_BYTES, plaintext.length)));
  }
  return performance.now() - started;
}

const keyBytes = new Uint8Array(32).fill(0x2b);
const sealKey = await importAttemptAesKey(keyBytes, ["encrypt"]);
const openKey = await importAttemptAesKey(keyBytes, ["decrypt"]);

console.log(
  `PERF host=crypto (node ${process.version}, AES-GCM ${FRAGMENT_BYTES}B fragments, SHA-256 ${
    CHUNK_BYTES / MIB
  }MiB chunks)`,
);
for (const sizeMiB of sizes) {
  const plaintext = new Uint8Array(sizeMiB * MIB);
  // A repeating pattern, not zeros: a compressible constant would flatter
  // nothing here (AES-GCM is not data-dependent) but it also costs nothing to
  // be honest about the input.
  for (let i = 0; i < plaintext.length; i += 1) {
    plaintext[i] = (i * 7 + 3) % 251;
  }
  const seal = [];
  const open = [];
  const digest = [];
  const receiver = [];
  for (let rep = 0; rep < reps; rep += 1) {
    const sealed = await sealAll(sealKey, plaintext);
    const opening = await openAll(openKey, sealed.frames);
    const digestMs = await digestChunks(plaintext);
    if (opening.opened !== plaintext.length) {
      // Loud, never a silent slow sample: an arm that lost bytes measured
      // something other than the pipeline.
      throw new Error(`crypto arm opened ${opening.opened} of ${plaintext.length} bytes`);
    }
    seal.push(throughputMiBs(plaintext.length, sealed.ms));
    open.push(throughputMiBs(plaintext.length, opening.ms));
    digest.push(throughputMiBs(plaintext.length, digestMs));
    // What the recipient's CPU actually has to do per byte.
    receiver.push(throughputMiBs(plaintext.length, opening.ms + digestMs));
  }
  console.log(benchLine("crypto-seal", sizeMiB, seal));
  console.log(benchLine("crypto-open", sizeMiB, open));
  console.log(benchLine("crypto-digest", sizeMiB, digest));
  console.log(benchLine("crypto-receiver", sizeMiB, receiver));
}
