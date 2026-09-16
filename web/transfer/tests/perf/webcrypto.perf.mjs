// T-WEB-PERF, `webcrypto` probe (3.10): does issuing several AEAD opens at
// once buy anything on this engine?
//
// The stage breakdown says the answer is engine-dependent — on webkit the
// per-frame `decrypt` is 478 ms of an 808 ms 32 MiB transfer (0.35 ms per
// 24 KiB fragment), on chromium it is 34 ms — and the pipeline change the
// plan lists (issue a chunk's fragments together, keep the order) is only
// worth its complexity if the engine actually overlaps the calls. This probe
// asks exactly that, in the page, with nothing else running: the same
// fragments opened one at a time, then four at a time, then eight.
import { test, expect } from "@playwright/test";
import { spawnRoomEnv, openPersistentPeer } from "../e2e/helpers.mjs";
import { benchLine, throughputMiBs } from "./report.mjs";

let env = null;

test.beforeAll(async () => {
  env = await spawnRoomEnv({ relayRate: 0 });
}, 120_000);

test.afterAll(() => {
  env?.cleanup();
});

test("webcrypto concurrency probe", async () => {
  const engine = test.info().project.name;
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  const peer = await openPersistentPeer(env.roomUrl, {
    browserName: browserName ?? defaultBrowserType,
    channel,
  });
  const reps = 3;
  const results = { 1: [], 4: [], 8: [] };
  for (let rep = 0; rep < reps; rep += 1) {
    for (const depth of [1, 4, 8]) {
      const ms = await peer.page.evaluate(async (inflight) => {
        const FRAGMENT = 24 * 1024;
        const COUNT = 1376; // exactly the 32 MiB transfer's fragment count
        const key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, [
          "encrypt",
          "decrypt",
        ]);
        const plain = new Uint8Array(FRAGMENT);
        crypto.getRandomValues(plain.subarray(0, 1024));
        const iv = new Uint8Array(12);
        const sealed = new Uint8Array(
          await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, plain),
        );
        const started = performance.now();
        let at = 0;
        while (at < COUNT) {
          const batch = [];
          for (let i = 0; i < inflight && at < COUNT; i += 1, at += 1) {
            batch.push(crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, sealed));
          }
          await Promise.all(batch);
        }
        return performance.now() - started;
      }, depth);
      results[depth].push(throughputMiBs(1376 * 24 * 1024, ms));
    }
  }
  for (const depth of [1, 4, 8]) {
    console.log(benchLine(`webcrypto-open-${depth}-${engine}`, 32, results[depth]));
  }
  expect(peer.failures).toEqual([]);
  await peer.cleanup();
});
