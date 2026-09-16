// T-WEB-CROSS (6.4): the compatibility claim is about PAIRS, not engines.
//
// Every other spec runs the same engine on both sides, once per project.
// That proves each engine can talk to ITSELF. The claim this feature makes
// to a user is different — "send a file from your Firefox to their Safari" —
// and the two sides of a transfer negotiate: SDP, ICE, the DataChannel's
// own limits, and the frame sizes derived from them. A pair is therefore
// its own experiment, and the pairs that cross engines are the ones nobody
// else runs.
//
// The matrix the plan fixes:
//   * same-engine pairs, both transports — chromium, firefox, webkit;
//   * crossed pairs on the DIRECT path — chromium↔firefox, chromium↔webkit,
//     because direct is where the negotiation lives;
//   * every pair again on the RELAY path, which must stay engine-blind
//     (the server moves opaque frames and never parses one).
//
// It runs under ONE project: the peers name their own engines, so running
// this spec once per project would repeat the same matrix three times.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";

let env = null;
let roomDir = null;
let fileBytes = null;
let fileHashHex = null;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-cross-"));
  // Small on purpose: this gate asks whether a pair can agree at all, and
  // ten transfers of a large file would measure the runner instead.
  fileBytes = Buffer.alloc(768 * 1024);
  for (let i = 0; i < fileBytes.length; i += 1) {
    fileBytes[i] = (i * 13 + 7) % 251;
  }
  fileHashHex = createHash("sha256").update(fileBytes).digest("hex");
  writeFileSync(join(roomDir, "cross.bin"), fileBytes);
}, 120_000);

test.afterAll(() => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function poll(page, fn, timeoutMs = 60_000) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("e2e poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 20_000 });
}

/**
 * One transfer between two named engines, asserting the transport it took.
 *
 * `noWebRtc` is how the relay arm is forced: the page finds no usable
 * `RTCPeerConnection`, answers the direct attempt with `unsupported` and the
 * server falls back inside the SAME transfer — the product's own path, not
 * a second code path written for the test.
 */
async function transferBetween(source, recipient, { relay }) {
  const a = await openPersistentPeer(env.roomUrl, {
    browserName: source,
    noWebRtc: relay,
  });
  const b = await openPersistentPeer(env.roomUrl, {
    browserName: recipient,
    noWebRtc: relay,
  });
  try {
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page), `${recipient} has no usable OPFS`).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "cross.bin")]);
    const offer = await poll(b.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 120_000 });
    await expect(b.page.locator("#save-name")).toHaveText("cross.bin");

    // The transport BOTH sides committed to, read from the pages that
    // played it — a pair that disagreed about its own path would still
    // deliver the bytes, and would still be a defect.
    const want = relay ? "relay" : "direct";
    for (const [who, peer] of [
      [source, a],
      [recipient, b],
    ]) {
      const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.length, `${who} committed no path`).toBeGreaterThan(0);
      expect(commits.at(-1).path, `${who} committed the wrong path`).toBe(want);
    }

    // And the bytes, because an interop claim that stops at "it connected"
    // is the claim a fragmentation bug survives.
    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    expect(saved.length).toBe(fileBytes.length);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);

    expect(a.failures).toEqual([]);
    expect(b.failures).toEqual([]);
  } finally {
    await a.cleanup();
    await b.cleanup();
  }
}

// Crossed pairs exist only on the direct path; on the relay every pair is
// crossed as far as the server is concerned, which is why all three run
// there too.
const DIRECT_PAIRS = [
  ["chromium", "chromium"],
  ["firefox", "firefox"],
  ["webkit", "webkit"],
  ["chromium", "firefox"],
  ["chromium", "webkit"],
];
const RELAY_PAIRS = [
  ["chromium", "chromium"],
  ["firefox", "firefox"],
  ["webkit", "webkit"],
  ["chromium", "firefox"],
  ["chromium", "webkit"],
];

test.describe.serial("T-WEB-CROSS", () => {
  // One project drives the whole matrix; the others would repeat it.
  test.skip(
    () => test.info().project.name !== "chromium",
    "the cross matrix names its own engines and runs once",
  );

  for (const [source, recipient] of DIRECT_PAIRS) {
    test(`T-WEB-CROSS direct ${source} -> ${recipient}`, async () => {
      test.setTimeout(180_000);
      await transferBetween(source, recipient, { relay: false });
    });
  }

  for (const [source, recipient] of RELAY_PAIRS) {
    test(`T-WEB-CROSS relay ${source} -> ${recipient}`, async () => {
      test.setTimeout(180_000);
      await transferBetween(source, recipient, { relay: true });
    });
  }
});
