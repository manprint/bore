// T-WEB-ZIP (5.2): a whole folder downloads as ONE deterministic ZIP64
// archive, over the relay and over the direct path, and the archive the
// browser saves is the tree the publisher walked — paths, bytes and order.
// The same offer downloaded twice produces byte-identical archives, and the
// publisher never materializes the archive (no Blob, no object URL, no
// temporary file) because it is generated straight into the sealed stream.
import { test, expect } from "@playwright/test";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  rmSync,
  readFileSync,
} from "node:fs";
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

configure({ useWebWorkers: false });

let env = null;
let roomDir = null;
/** path → bytes, exactly what the archive must contain. */
const expected = new Map();
/** Archive hashes seen across the two legs; they must all agree. */
const archiveHashes = [];

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-zip-"));
  const tree = join(roomDir, "albero");
  mkdirSync(join(tree, "sub"), { recursive: true });
  // One entry per case the archive has to carry: text, non-ASCII name, an
  // empty file, and a payload longer than one logical chunk so the archive
  // spans several of them.
  const grande = Buffer.alloc(1024 * 1024 + 4096);
  for (let i = 0; i < grande.length; i++) {
    grande[i] = (i * 31 + 7) % 251;
  }
  const files = [
    ["alfa.txt", Buffer.from("alfa")],
    ["caffè.txt", Buffer.from("unicode")],
    ["zero.bin", Buffer.alloc(0)],
    ["sub/grande.bin", grande],
  ];
  for (const [name, bytes] of files) {
    writeFileSync(join(tree, name), bytes);
    expected.set(`albero/${name}`, bytes);
  }
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

/**
 * One peer on a real on-disk profile (OPFS is unusable in an ephemeral
 * WebKit context, V002-F02). `noWebRtc` picks the transport: without it the
 * server commits the direct path, with it the relay.
 */
async function openPeer(url, { noWebRtc = false, init } = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    noWebRtc,
    init,
  });
}

/** Counts every way a page could materialize the archive in memory. */
const countMaterializations = () => {
  window.__materialized = { objectUrls: [], blobs: 0 };
  const realCreate = URL.createObjectURL;
  URL.createObjectURL = function (object) {
    window.__materialized.objectUrls.push(object?.size ?? -1);
    return realCreate.call(this, object);
  };
  const RealBlob = window.Blob;
  window.Blob = function (parts, options) {
    window.__materialized.blobs += 1;
    return new RealBlob(parts ?? [], options);
  };
  window.Blob.prototype = RealBlob.prototype;
};

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", {
    timeout: 15_000,
  });
}

async function poll(page, fn, timeoutMs = 30_000) {
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

async function readArchive(bytes) {
  const reader = new ZipReader(new Uint8ArrayReader(new Uint8Array(bytes)));
  const list = await reader.getEntries();
  const out = [];
  for (const entry of list) {
    out.push({
      filename: entry.filename,
      directory: entry.directory === true,
      data:
        entry.directory === true
          ? null
          : Buffer.from(await entry.getData(new Uint8ArrayWriter())),
    });
  }
  await reader.close();
  return out;
}

/**
 * Publishes the tree from A, downloads it as one archive from `peer`, saves
 * it and returns the saved bytes. The button is the ONLY entry point — the
 * card decides which download an offer supports, and a folder supports the
 * archive alone.
 */
async function downloadArchive(publisher, peer) {
  await publisher.page
    .locator("#folder-input")
    .setInputFiles(join(roomDir, "albero"));
  await expect(
    publisher.page.locator('.offer-card h4:has-text("albero")'),
  ).toBeVisible({
    timeout: 20_000,
  });
  const offer = await poll(peer.page, () => {
    const catalog = window.__BORE_TEST__.getCatalogSnapshot();
    return catalog.length === 1 ? catalog[0] : null;
  });
  const button = peer.page.locator(
    `button[data-download-zip="${offer.offerId}"]`,
  );
  await expect(button).toBeVisible({ timeout: 15_000 });
  await expect(button).toHaveText("Scarica ZIP");
  // A folder offer has no raw download: the archive is the whole of it.
  await expect(
    peer.page.locator(`button[data-download="${offer.offerId}"]`),
  ).toHaveCount(0);
  await button.click();

  try {
    await expect(peer.page.locator("#save-file")).toBeVisible({
      timeout: 60_000,
    });
  } catch (error) {
    if (process.env.BORE_E2E_LOG) {
      console.error(
        "recv",
        JSON.stringify(
          await peer.page.evaluate(() => ({
            state: window.__BORE_TEST__.receiverState?.(),
            errors: window.__BORE_TEST__.receiverErrors,
            inbound: window.__BORE_TEST__.inboundTypes,
            outbound: window.__BORE_TEST__.outboundTypes,
          })),
        ),
      );
      console.error(
        "send",
        JSON.stringify(
          await publisher.page.evaluate(() => ({
            state: window.__BORE_TEST__.senderState?.(),
            errors: window.__BORE_TEST__.senderErrors,
            outbound: window.__BORE_TEST__.outboundTypes,
          })),
        ),
      );
      console.error(
        "failures",
        JSON.stringify([peer.failures, publisher.failures]),
      );
    }
    throw error;
  }
  await expect(peer.page.locator("#save-name")).toHaveText("albero.zip");
  const download = await Promise.all([
    peer.page.waitForEvent("download", { timeout: 30_000 }),
    peer.page.locator("#save-file").click(),
  ]).then(([event]) => event);
  expect(download.suggestedFilename()).toBe("albero.zip");
  return readFileSync(await download.path());
}

/** The tree the archive must hold, whatever produced it. */
async function expectTree(bytes) {
  const read = await readArchive(bytes);
  const files = read.filter((entry) => !entry.directory);
  expect(new Set(files.map((entry) => entry.filename))).toEqual(
    new Set(expected.keys()),
  );
  for (const entry of files) {
    expect(entry.data.equals(expected.get(entry.filename))).toBe(true);
  }
  // Directory entries end in a slash and carry nothing; nothing escapes the
  // published root.
  for (const entry of read) {
    expect(entry.filename.startsWith("albero/")).toBe(true);
    expect(entry.filename.includes("..")).toBe(false);
    if (entry.directory) {
      expect(entry.filename.endsWith("/")).toBe(true);
    }
  }
  return createHash("sha256").update(bytes).digest("hex");
}

test.describe.serial("zip", () => {
  test("T-WEB-ZIP relay: a folder downloads as one archive and the source never holds it", async () => {
    // Two persistent profiles, a folder publish and a whole archive over a
    // rate-limited relay: the default 30 s is the harness's, not this
    // transfer's.
    test.setTimeout(240_000);
    const a = await openPeer(env.roomUrl, {
      noWebRtc: true,
      init: countMaterializations,
    });
    const b = await openPeer(env.roomUrl, { noWebRtc: true });
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    const saved = await downloadArchive(a, b);
    archiveHashes.push(await expectTree(saved));

    // The badge is a fact about bytes that arrived, not about a negotiation.
    await expect(b.page.locator("[data-path]")).toHaveAttribute(
      "data-path",
      "relay",
    );

    // The publisher generated the archive STRAIGHT into the sealed stream:
    // it never built a Blob of it and never handed one to the browser. The
    // recipient legitimately does both — that is how a verified file is
    // saved — so the assertion is about A alone.
    const materialized = await a.page.evaluate(() => window.__materialized);
    expect(materialized.blobs).toBe(0);
    expect(materialized.objectUrls).toEqual([]);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("T-WEB-ZIP direct: the same offer over the direct path is byte-identical", async () => {
    test.setTimeout(240_000);
    const a = await openPeer(env.roomUrl, { init: countMaterializations });
    const c = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(c.page);

    const saved = await downloadArchive(a, c);
    const hash = await expectTree(saved);
    archiveHashes.push(hash);

    // Determinism is the whole contract: the same manifest and the same
    // files produce the same archive, on another peer, over another
    // transport, in another browser profile. A resumed archive is
    // regenerated from byte zero and compared with what is already on disk,
    // so a single differing byte would make every resume fail.
    expect(new Set(archiveHashes).size).toBe(1);

    // The archive really did travel peer to peer: the server committed the
    // direct path and the badge only says so once a chunk has been verified
    // under it.
    const commits = await c.page.evaluate(
      () => window.__BORE_TEST__.pathCommits,
    );
    expect(commits.map((commit) => commit.path)).toEqual(
      commits.map(() => "direct"),
    );
    expect(commits.length).toBeGreaterThan(0);
    await expect(c.page.locator("[data-path]")).toHaveAttribute(
      "data-path",
      "direct",
    );

    const materialized = await a.page.evaluate(() => window.__materialized);
    expect(materialized.blobs).toBe(0);
    expect(materialized.objectUrls).toEqual([]);

    for (const peer of [a, c]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });
});
