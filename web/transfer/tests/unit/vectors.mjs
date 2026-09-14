// T-WEB-E2EE-FIXTURE vector runner: reads the checked-in fixtures, recomputes
// every derived value with the JS implementation and prints the results as one
// JSON object. The Rust test `e2ee_fixture_matches_js_implementation` runs
// this file live and compares each field against its own (ring-based)
// computation. Usage: node vectors.mjs <crypto-vectors.json>.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  attemptKey,
  bytesToHex,
  fileRoot,
  frameNonce,
  hexToBytes,
  hmacSign,
  manifestKey,
  openFrame,
  sealFrame,
  sha256Hex,
} from "../../src/crypto.js";
import { canonicalize, parseManifest } from "../../src/protocol.js";

const here = dirname(fileURLToPath(import.meta.url));
const fixtureDir = join(here, "..", "..", "..", "..", "tests", "fixtures", "web_transfer", "v1");
const vectorsPath = process.argv[2] ?? join(fixtureDir, "crypto-vectors.json");
const vectors = JSON.parse(readFileSync(vectorsPath, "utf8"));
const manifestRaw = readFileSync(join(fixtureDir, "manifest.json"), "utf8");
const inputs = vectors.inputs;

const roomKey = hexToBytes(inputs.room_key);
const roomId = hexToBytes(inputs.room_id);
const transferId = hexToBytes(inputs.transfer_id);
const attemptId = hexToBytes(inputs.attempt_id);
const plaintext = hexToBytes(inputs.plaintext_hex);
const seq = inputs.seq;

const manifest = parseManifest(JSON.parse(manifestRaw));
const canonical = canonicalize({
  entries: manifest.entries,
  mode: manifest.mode,
  offer: manifest.offer,
});
const mkey = await manifestKey(roomKey, roomId);
const mac = await hmacSign(mkey, new TextEncoder().encode(canonical));
const akey = await attemptKey(roomKey, transferId, attemptId);
const data = await sealFrame(akey, seq, 1, plaintext);
const total = new Uint8Array(8);
new DataView(total.buffer).setBigUint64(0, BigInt(plaintext.length), false);
const final = await sealFrame(akey, seq, 2, total);
// Self-check: what we seal must open (catches runner-side mistakes, not
// implementation agreement — that is the Rust test's job).
await openFrame(akey, data, 0);
await openFrame(akey, final, 0);

const leafHello = hexToBytes(await sha256Hex(plaintext));
const rootSingle = await fileRoot(1, [leafHello]);
const rootEmpty = await fileRoot(0, []);
const multiLeaves = [];
for (const seed of inputs.multichunk_seeds) {
  multiLeaves.push(hexToBytes(await sha256Hex(new TextEncoder().encode(seed))));
}
const rootMulti = await fileRoot(multiLeaves.length, multiLeaves);

console.log(
  JSON.stringify({
    canonical_manifest_hex: bytesToHex(new TextEncoder().encode(canonical)),
    leaf_hello_hex: bytesToHex(leafHello),
    manifest_key_hex: bytesToHex(mkey),
    manifest_mac_hex: bytesToHex(mac),
    root_empty_hex: bytesToHex(rootEmpty),
    root_single_hex: bytesToHex(rootSingle),
    root_multi_hex: bytesToHex(rootMulti),
    multichunk_leaves_hex: multiLeaves.map(bytesToHex),
    attempt_key_hex: bytesToHex(akey),
    nonce_hex: bytesToHex(frameNonce(seq)),
    frame_data_hex: bytesToHex(data),
    frame_final_hex: bytesToHex(final),
  }),
);
