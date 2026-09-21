// T-WEB-DOWNLOAD-RELAY: explicit click downloads and verifies exact bytes
// over the relay, then saves through the staged panel; insufficient quota
// never requests; an OPFS-less browser keeps publishing but cannot fetch.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import {
  Uint8ArrayReader,
  Uint8ArrayWriter,
  ZipReader,
  configure,
} from "@zip.js/zip.js";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

configure({ useWebWorkers: false });

let env = null;
let roomDir = null;
let fileBytes = null;
let fileHashHex = null;
const multiFiles = new Map();

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-download-"));
  fileBytes = Buffer.alloc(2 * 1024 * 1024 + 7);
  for (let i = 0; i < fileBytes.length; i++) {
    fileBytes[i] = (i * 7 + 3) % 251;
  }
  fileHashHex = createHash("sha256").update(fileBytes).digest("hex");
  writeFileSync(join(roomDir, "big.bin"), fileBytes);
  for (const [name, size, seed] of [
    ["one.bin", 128 * 1024 + 1, 11],
    ["two.bin", 192 * 1024 + 2, 17],
    ["three.bin", 256 * 1024 + 3, 23],
    ["four.bin", 320 * 1024 + 4, 29],
    ["cancel-while-connecting.bin", 8 * 1024 * 1024 + 5, 31],
  ]) {
    const bytes = Buffer.alloc(size);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = (i * seed + 7) % 251;
    }
    multiFiles.set(name, bytes);
    writeFileSync(join(roomDir, name), bytes);
  }
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

// Every peer here runs on a real on-disk profile: OPFS is unusable in an
// ephemeral WebKit context (V002-F02), and the three engines must run the
// same code for the comparison to mean anything.
async function openPeer(url, init) {
  // `devices[...]` carries `defaultBrowserType`, not `browserName`.
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  // This suite is the RELAY gate: since 4.2 the direct path is the default,
  // so the peers run as an engine without WebRTC and the server falls back
  // to the relay at once. `direct.spec.mjs` is the direct half.
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    init,
    noWebRtc: true,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 15_000 });
}

async function poll(page, fn, timeoutMs = 25_000) {
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

test.describe.serial("download-relay", () => {
  test("click verifies exact bytes, then saves explicitly", async () => {
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    // A context that merely exposes OPFS would report the whole feature as
    // unsupported below; assert the capability so an engine regression fails
    // here, as a capability problem, instead of as a wrong return value.
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "big.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });

    // Explicit click through the 3.4 entry point (3.5 buttons call it too).
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    // The verified panel appears with the manifest name; nothing downloads
    // on its own (no navigation, no download event yet).
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 30_000 });
    await expect(b.page.locator("#save-name")).toHaveText("big.bin");

    // Explicit save: the download event carries the exact bytes.
    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 30_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(download.suggestedFilename()).toBe("big.bin");
    const path = await download.path();
    const saved = readFileSync(path);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);
    expect(saved.length).toBe(fileBytes.length);
    // Saving purges the staging: the panel hides and stays hidden.
    await expect(b.page.locator("#save-section")).toHaveCount(0, { timeout: 10_000 });

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("a five-file offer supports connecting cancel, single download and ZIP", async () => {
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles(
      [...multiFiles.keys()].map((name) => join(roomDir, name)),
    );
    const offer = await poll(b.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 && catalog[0].manifest.entries.length === 5
        ? catalog[0]
        : null;
    });
    const byPath = new Map(
      offer.manifest.entries.map((entry) => [entry.path, entry.id]),
    );

    const cancelEntry = byPath.get("cancel-while-connecting.bin");
    await b.page
      .locator(
        `button[data-download-entry="${offer.offerId}:${cancelEntry}"]`,
      )
      .click();
    const liveRow = b.page.locator(
      '.transfer-row:has([data-path="connecting"])',
    );
    await expect(liveRow.locator("button[data-cancel]")).toBeVisible({
      timeout: 15_000,
    });
    const cancelledId = await liveRow.getAttribute("data-transfer");
    const cancelledRow = b.page.locator(
      `.transfer-row[data-transfer="${cancelledId}"]`,
    );
    await liveRow.locator("button[data-cancel]").click();
    await expect(cancelledRow.locator(".transfer-state")).toContainText(
      "Annullato",
    );

    const wanted = "three.bin";
    const entryId = byPath.get(wanted);
    await b.page
      .locator(`button[data-download-entry="${offer.offerId}:${entryId}"]`)
      .click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
    await expect(b.page.locator("#save-name")).toHaveText(wanted);
    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 30_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    expect(saved.equals(multiFiles.get(wanted))).toBe(true);

    await b.page
      .locator(`button[data-download-zip="${offer.offerId}"]`)
      .click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
    const zipDownload = await Promise.all([
      b.page.waitForEvent("download", { timeout: 30_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const zipBytes = readFileSync(await zipDownload.path());
    const reader = new ZipReader(
      new Uint8ArrayReader(new Uint8Array(zipBytes)),
    );
    const entries = await reader.getEntries();
    expect(entries.map((entry) => entry.filename).sort()).toEqual(
      [...multiFiles.keys()].sort(),
    );
    for (const entry of entries) {
      const bytes = Buffer.from(
        await entry.getData(new Uint8ArrayWriter()),
      );
      expect(bytes.equals(multiFiles.get(entry.filename))).toBe(true);
    }
    await reader.close();

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("insufficient quota never requests", async () => {
    const a = await openPeer(env.roomUrl);
    // Starve the quota before the app boots.
    const b = await openPeer(env.roomUrl, () => {
      try {
        Object.defineProperty(window.navigator, "storage", {
          value: {
            getDirectory: window.navigator.storage.getDirectory.bind(window.navigator.storage),
            estimate: async () => ({ quota: 1024, usage: 0 }),
          },
          configurable: true,
        });
      } catch {
        /* override refused: the leg skips below */
      }
    });
    await expectConnected(a.page);
    await expectConnected(b.page);
    const probe = await b.page.evaluate(() => window.navigator.storage.estimate());
    if (probe.quota !== 1024) {
      test.skip(true, "quota override refused by this engine");
    }
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "big.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ error: "STORAGE_QUOTA" });
    await new Promise((r) => setTimeout(r, 1000));
    const outbound = (await hookCounters(b.page)).outboundTypes ?? [];
    expect(outbound).not.toContain("transfer.request");

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("without OPFS the browser publishes but cannot fetch", async () => {
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl, () => {
      try {
        Object.defineProperty(window.navigator, "storage", {
          value: {},
          configurable: true,
        });
      } catch {
        /* override refused: support probe decides below */
      }
    });
    await expectConnected(a.page);
    await expectConnected(b.page);
    const supported = await b.page.evaluate(
      () =>
        typeof window.navigator?.storage?.getDirectory === "function" &&
        typeof window.crypto?.subtle?.digest === "function" &&
        typeof window.indexedDB?.open === "function",
    );
    if (supported) {
      test.skip(true, "OPFS present: the no-opfs leg needs a stripped late");
    }
    // Publishing (worker hashing) needs no OPFS.
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "big.bin")]);
    await expect(a.page.locator('.offer-card h4:has-text("big.bin")')).toBeVisible({
      timeout: 15_000,
    });
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ error: "UNSUPPORTED" });
    // ...but publishing from the same tab still works (worker hashing needs
    // no OPFS).
    await b.page.locator("#file-input").setInputFiles([join(roomDir, "big.bin")]);
    await expect(b.page.locator('.offer-card h4:has-text("big.bin")')).toBeVisible({
      timeout: 15_000,
    });

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });
});

// T-WEB-CANCEL-RESUME / T-WEB-CANCEL-AUTH (3.5): the download UX — cancel
// from either participant, a reconnect that resumes nothing by itself, and
// a second click that skips what is already verified.
//
// The room is throttled server-side (`--web-transfer-relay-rate`), so a
// cancel lands mid-transfer at a pace this machine cannot outrun.
const SLOW_RATE = 1024 * 1024;

test.describe.serial("cancel-resume", () => {
  let slow = null;
  let slowDir = null;
  let slowBytes = null;
  let slowHash = null;

  test.beforeAll(async () => {
    slow = await spawnRoomEnv({ relayRate: SLOW_RATE });
    slowDir = mkdtempSync(join(tmpdir(), "bore-cancel-"));
    slowBytes = Buffer.alloc(6 * 1024 * 1024 + 11);
    for (let i = 0; i < slowBytes.length; i++) {
      slowBytes[i] = (i * 31 + 17) % 251;
    }
    slowHash = createHash("sha256").update(slowBytes).digest("hex");
    writeFileSync(join(slowDir, "slow.bin"), slowBytes);
  }, 60_000);

  test.afterAll(async () => {
    slow?.cleanup();
    if (slowDir) {
      rmSync(slowDir, { recursive: true, force: true });
    }
  });

  test("cancel keeps the partial, reconnect resumes nothing, second click completes", async () => {
    test.setTimeout(240_000);
    const a = await openPeer(slow.roomUrl);
    const b = await openPeer(slow.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(slowDir, "slow.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const button = b.page.locator(`button[data-download="${offer.offerId}"]`);
    await expect(button).toHaveText("Scarica");

    // The click is the only thing that starts it.
    await button.click();
    await poll(
      b.page,
      () => {
        const bar = document.querySelector(".transfer-row progress");
        return bar !== null && bar.value >= 25 ? bar.value : null;
      },
      60_000,
    );

    // Cancel from the recipient's own row.
    await b.page.locator(".transfer-row button[data-cancel]").click();
    await expect(b.page.locator(".transfer-row .transfer-state")).toContainText("Annullato");
    const afterCancel = await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length);
    expect(afterCancel).toBe(0);
    // Bytes really stopped: nothing moves for a second at 1 MiB/s.
    await new Promise((r) => setTimeout(r, 1000));
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);
    // The partial stays: the same single button now offers a resume.
    await expect(button).toHaveText("Riprendi", { timeout: 15_000 });

    // Reconnect (same tab, same profile, same OPFS): still no request.
    await b.page.reload();
    await expectConnected(b.page);
    await expect(button).toHaveText("Riprendi", { timeout: 20_000 });
    await new Promise((r) => setTimeout(r, 2000));
    const idle = await hookCounters(b.page);
    expect(idle.outboundTypes).not.toContain("transfer.request");
    expect(idle.transferRows).toBe(0);

    // Second click: the request carries what we hold, the source skips it,
    // and the saved bytes are still the exact file.
    await button.click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
    const resumed = await hookCounters(b.page);
    expect(resumed.resumeRequests.length).toBe(1);
    const ranges = resumed.resumeRequests[0];
    expect(Array.isArray(ranges)).toBe(true);
    expect(ranges.length).toBeGreaterThan(0);
    expect(ranges[0][0]).toBe(0);
    expect(ranges[0][1]).toBeGreaterThan(0);

    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const path = await download.path();
    const saved = readFileSync(path);
    expect(saved.length).toBe(slowBytes.length);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(slowHash);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("only the two participants can cancel", async () => {
    test.setTimeout(240_000);
    const a = await openPeer(slow.roomUrl);
    const b = await openPeer(slow.roomUrl);
    const c = await openPeer(slow.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    await expectConnected(c.page);

    await a.page.locator("#file-input").setInputFiles([join(slowDir, "slow.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    await b.page.locator(`button[data-download="${offer.offerId}"]`).click();
    const transferId = await poll(
      b.page,
      () => window.__BORE_TEST__.receiverState()[0]?.transferId ?? null,
      60_000,
    );

    // The third peer has no row and no button at all...
    expect(await c.page.locator(".transfer-row").count()).toBe(0);
    expect(await c.page.locator("button[data-cancel]").count()).toBe(0);
    // ...and the server refuses its cancel even sent by hand.
    await c.page.evaluate(
      (id) => window.__BORE_TEST__.controlSend("transfer.cancel", "cc".repeat(16), { transferId: id }),
      transferId,
    );
    const denied = await poll(
      c.page,
      () => {
        const errors = window.__BORE_TEST__.controlErrors ?? [];
        const last = errors[errors.length - 1];
        return last?.code ?? null;
      },
      20_000,
    );
    expect(denied).toBe("NOT_PARTICIPANT");
    // The transfer is untouched: B still holds it.
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(1);

    // The SOURCE cancels from its own row; the recipient hears it.
    await a.page.locator(".transfer-row button[data-cancel]").click();
    await expect(b.page.locator(".transfer-row .transfer-state")).toContainText("Annullato", {
      timeout: 30_000,
    });
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);

    for (const peer of [a, b, c]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });
});
