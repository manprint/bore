// T-WEB-ZIP-RESUME / T-WEB-ZIP-SOURCE-CHANGE (5.3): an archive interrupted
// mid-download resumes, and an archive whose SOURCE changed under a verified
// partial refuses rather than corrupting it.
//
// Both gates run against a THROTTLED room (`--web-transfer-relay-rate`), so
// a cancel lands mid-transfer at a pace this machine cannot outrun, and both
// run on three engines: a resume is the one path where the recipient's disk
// and the source's regeneration have to agree byte for byte, and "agree" is
// exactly the kind of claim an engine can break on its own.
import { test, expect } from "@playwright/test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  Uint8ArrayReader,
  Uint8ArrayWriter,
  ZipReader,
  configure,
} from "@zip.js/zip.js";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

configure({ useWebWorkers: false });

/** One logical chunk: the unit a resume is counted in. */
const CHUNK_BYTES = 1024 * 1024;
/** Slow enough that a cancel lands inside the transfer, not after it. */
const SLOW_RATE = 1024 * 1024;

let env = null;
let roomDir = null;
/** path → bytes, as published. */
const expected = new Map();

function filler(length, salt) {
  const out = Buffer.alloc(length);
  for (let i = 0; i < length; i++) {
    out[i] = (i * 31 + salt * 7 + 11) % 251;
  }
  return out;
}

test.beforeAll(async () => {
  env = await spawnRoomEnv({ relayRate: SLOW_RATE });
  roomDir = mkdtempSync(join(tmpdir(), "bore-zipresume-"));
  mkdirSync(join(roomDir, "albero", "sub"), { recursive: true });
  // Several logical chunks, so a cancel leaves a prefix worth resuming.
  const files = [
    ["alfa.txt", Buffer.from("alfa")],
    ["sub/grande.bin", filler(5 * CHUNK_BYTES, 1)],
  ];
  for (const [name, bytes] of files) {
    writeFileSync(join(roomDir, "albero", name), bytes);
    expected.set(`albero/${name}`, bytes);
  }
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openPeer(url) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  // The throttle these gates depend on is the RELAY's
  // (`--web-transfer-relay-rate`), and since 4.2 the direct path is the
  // default — over a loopback DataChannel the whole archive lands before a
  // cancel could be aimed at it. So both peers run as an engine without
  // WebRTC and the server falls back to the relay at once. The resume
  // machinery itself is transport-blind: it is the same pipeline either way.
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    noWebRtc: true,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", {
    timeout: 15_000,
  });
}

async function poll(page, fn, timeoutMs = 60_000, arg = undefined) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("e2e poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

/** Publishes the tree from `publisher` and returns the offer the peers see. */
async function publishTree(publisher, peer) {
  await publisher.page
    .locator("#folder-input")
    .setInputFiles(join(roomDir, "albero"));
  await expect(
    publisher.page.locator('.offer-card h4:has-text("albero")'),
  ).toBeVisible({ timeout: 20_000 });
  return poll(peer.page, () => {
    const catalog = window.__BORE_TEST__.getCatalogSnapshot();
    return catalog.length === 1 ? catalog[0] : null;
  });
}

/** Saves the staged archive and returns its bytes. */
async function saveArchive(page) {
  const download = await Promise.all([
    page.waitForEvent("download", { timeout: 60_000 }),
    page.locator("#save-file").click(),
  ]).then(([event]) => event);
  expect(download.suggestedFilename()).toBe("albero.zip");
  return readFileSync(await download.path());
}

/** Reads an archive back as `path → bytes`, directories dropped. */
async function readArchive(bytes) {
  const reader = new ZipReader(new Uint8ArrayReader(new Uint8Array(bytes)));
  const out = new Map();
  for (const entry of await reader.getEntries()) {
    if (entry.directory === true) {
      continue;
    }
    out.set(entry.filename, Buffer.from(await entry.getData(new Uint8ArrayWriter())));
  }
  await reader.close();
  return out;
}

/**
 * Records the recipient's received-byte counter for the life of one
 * attempt. A resume's whole claim is that fewer bytes travel than the
 * archive holds, and the row is gone by the time the file is staged, so the
 * number has to be sampled while it exists.
 */
async function watchReceivedBytes(page) {
  await page.evaluate(() => {
    window.__maxReceived = 0;
    clearInterval(window.__receivedTimer);
    window.__receivedTimer = setInterval(() => {
      const row = window.__BORE_TEST__.receiverState()[0];
      if (row !== undefined) {
        window.__maxReceived = Math.max(window.__maxReceived, row.receivedBytes);
      }
    }, 50);
  });
}

async function stopWatching(page) {
  return page.evaluate(() => {
    clearInterval(window.__receivedTimer);
    return window.__maxReceived ?? 0;
  });
}

test.describe.serial("zip-resume", () => {
  test("T-WEB-ZIP-RESUME: a cancelled archive resumes and the saved ZIP is the whole tree", async () => {
    test.setTimeout(300_000);
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    const offer = await publishTree(a, b);
    const button = b.page.locator(`button[data-download-zip="${offer.offerId}"]`);
    await expect(button).toHaveText("Scarica ZIP");

    // One click starts it; the progress bar is the recipient's own.
    await button.click();
    await poll(
      b.page,
      () => {
        const row = window.__BORE_TEST__.receiverState()[0];
        // At least one WHOLE chunk verified and on disk: bytes off the
        // socket are not resumable work, and a resume gate that cancelled
        // on byte count would be testing the scheduler.
        return row !== undefined && row.verifiedRanges > 0 ? row.receivedBytes : null;
      },
      120_000,
    );
    const staged = await poll(
      b.page,
      (chunk) => {
        const row = window.__BORE_TEST__.receiverState()[0];
        return row !== undefined && row.receivedBytes > chunk ? row.receivedBytes : null;
      },
      60_000,
      CHUNK_BYTES,
    );
    expect(staged).toBeGreaterThan(CHUNK_BYTES);

    await b.page.locator(".transfer-row button[data-cancel]").click();
    await expect(b.page.locator(".transfer-row .transfer-state")).toContainText(
      "Annullato",
    );
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);
    // The partial is kept and ANNOUNCED — the card says so, and the button
    // still says what the offer supports.
    await expect(
      b.page.locator(
        `.offer-card[data-offer="${offer.offerId}"] .offer-meta:has-text("Disponibile per ripresa")`,
      ),
    ).toBeVisible({ timeout: 20_000 });

    // A reconnect resumes NOTHING by itself: same tab, same profile, same
    // OPFS, and no request leaves.
    await b.page.reload();
    await expectConnected(b.page);
    await expect(
      b.page.locator(
        `.offer-card[data-offer="${offer.offerId}"] .offer-meta:has-text("Disponibile per ripresa")`,
      ),
    ).toBeVisible({ timeout: 20_000 });
    await new Promise((r) => setTimeout(r, 2000));
    const idle = await hookCounters(b.page);
    expect(idle.outboundTypes).not.toContain("transfer.request");
    expect(idle.transferRows).toBe(0);

    // Second click: the request names the prefix on disk, the source skips
    // exactly it, and the archive that lands is the whole tree.
    await watchReceivedBytes(b.page);
    await b.page
      .locator(`button[data-download-zip="${offer.offerId}"]`)
      .click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 240_000 });
    const received = await stopWatching(b.page);
    const resumed = await hookCounters(b.page);
    expect(resumed.resumeRequests.length).toBe(1);
    const ranges = resumed.resumeRequests[0];
    expect(Array.isArray(ranges)).toBe(true);
    expect(ranges.length).toBe(1);
    expect(ranges[0][0]).toBe(0);
    expect(ranges[0][1]).toBeGreaterThan(0);

    const saved = await saveArchive(b.page);
    const read = await readArchive(saved);
    expect(new Set(read.keys())).toEqual(new Set(expected.keys()));
    for (const [path, bytes] of expected) {
      expect(read.get(path).equals(bytes)).toBe(true);
    }
    // The prefix the recipient named never travelled: the second attempt's
    // own byte counter cannot exceed the archive minus exactly that prefix.
    // This is the whole point of a resume and it is only observable here —
    // the row is gone by the time the file is staged.
    const skipped = ranges[0][1] * CHUNK_BYTES;
    expect(skipped).toBeGreaterThanOrEqual(CHUNK_BYTES);
    expect(received).toBeLessThanOrEqual(saved.length - skipped);
    expect(received).toBeGreaterThan(0);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("T-WEB-ZIP-SOURCE-CHANGE: a changed source cannot overwrite a verified partial", async () => {
    // V-9's rule applies to a test budget too: 300 s described the
    // workstation. On the ubuntu CI runner this one test exceeded it while
    // the whole WebKit zip-resume file took 5.1 minutes — two cores, three
    // engines and a full tree published, resumed and re-verified.
    test.setTimeout(600_000);
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);

    const offer = await publishTree(a, b);
    const button = b.page.locator(`button[data-download-zip="${offer.offerId}"]`);
    await button.click();
    await poll(
      b.page,
      () => {
        const row = window.__BORE_TEST__.receiverState()[0];
        return row !== undefined && row.verifiedRanges > 0 ? row.receivedBytes : null;
      },
      120_000,
    );
    await b.page.locator(".transfer-row button[data-cancel]").click();
    await expect(b.page.locator(".transfer-row .transfer-state")).toContainText(
      "Annullato",
    );
    await expect(
      b.page.locator(
        `.offer-card[data-offer="${offer.offerId}"] .offer-meta:has-text("Disponibile per ripresa")`,
      ),
    ).toBeVisible({ timeout: 20_000 });

    // The source now holds DIFFERENT bytes at the same path and the same
    // length, which is the one shape the manifest cannot catch: the offer
    // still authenticates, the entry size still matches, and only the
    // archive's rolling root can tell.
    const swapped = filler(5 * CHUNK_BYTES, 9);
    expect(
      await a.page.evaluate(
        ([offerId, bytes]) =>
          window.__BORE_TEST__.replaceOfferFile(
            offerId,
            new Uint8Array(bytes),
            "grande.bin",
            1757779200000,
            "albero/sub/grande.bin",
          ),
        [offer.offerId, [...swapped]],
      ),
    ).toBe(true);

    // The resume is refused, and it is refused as a CHANGED SOURCE.
    await b.page
      .locator(`button[data-download-zip="${offer.offerId}"]`)
      .click();
    await expect(
      b.page.locator(
        `.offer-card[data-offer="${offer.offerId}"] .offer-error:has-text("La sorgente è cambiata")`,
      ),
    ).toBeVisible({ timeout: 240_000 });
    expect(
      await b.page.evaluate(() => [...window.__BORE_TEST__.receiverErrors]),
    ).toEqual(expect.arrayContaining([expect.stringContaining("SOURCE_CHANGED")]));
    // Nothing was saved and nothing was staged.
    await expect(b.page.locator("#save-file")).toBeHidden();

    // The verified bytes are STILL on disk: the app never discards them on
    // its own, and the only way forward is a second, explicit gesture.
    const restart = b.page.locator(`button[data-restart="${offer.offerId}"]`);
    await expect(restart).toBeVisible();
    expect(
      await b.page.evaluate(async () => {
        const root = await navigator.storage.getDirectory();
        const dir = await root.getDirectoryHandle("bore-transfer-v1");
        let parts = 0;
        for await (const [, handle] of dir.entries()) {
          for await (const [, offerDir] of handle.entries()) {
            for await (const [, selection] of offerDir.entries()) {
              for await (const entry of selection.keys()) {
                if (entry.endsWith(".part")) {
                  parts += 1;
                }
              }
            }
          }
        }
        return parts;
      }),
    ).toBeGreaterThan(0);

    // The restart throws them away and downloads the archive the source
    // actually holds now.
    await restart.click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 240_000 });
    const saved = await saveArchive(b.page);
    const read = await readArchive(saved);
    expect(new Set(read.keys())).toEqual(new Set(expected.keys()));
    expect(read.get("albero/sub/grande.bin").equals(swapped)).toBe(true);
    expect(read.get("albero/alfa.txt").equals(expected.get("albero/alfa.txt"))).toBe(
      true,
    );

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });
});
