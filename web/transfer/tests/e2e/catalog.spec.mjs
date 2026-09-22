// T-WEB-OFFER: real file selection through the picker, worker preparation,
// server publish and catalog convergence across three engines.
//
// A selects one synthetic file and B selects two (one multichunk); every
// peer renders both offers grouped under their publishers; the MAC of A's
// offer is recomputed in Node from B's session key and compared; only the
// owner sees withdraw buttons; A withdraws; a changed source publishes as a
// distinct offer. Zero transfer/RTC/relay traffic throughout.
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import crypto from "node:crypto";
import { test, expect } from "@playwright/test";
import { canonicalize, spawnRoomEnv } from "./helpers.mjs";

let env;
let roomDir;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-offer-"));
  writeFileSync(join(roomDir, "hello.txt"), "hello world");
  const big = Buffer.alloc(2 * 1024 * 1024 + 7, 0x61);
  writeFileSync(join(roomDir, "big.bin"), big);
  writeFileSync(join(roomDir, "note.txt"), "second file");
  // A tree with everything that is easy to get wrong: nesting, an empty
  // directory, a non-ASCII name and a zero-byte file.
  mkdirSync(join(roomDir, "albero", "sub"), { recursive: true });
  mkdirSync(join(roomDir, "albero", "vuota"), { recursive: true });
  writeFileSync(join(roomDir, "albero", "alfa.txt"), "alfa");
  writeFileSync(join(roomDir, "albero", "caff\u00e8.txt"), "unicode");
  writeFileSync(join(roomDir, "albero", "zero.bin"), "");
  writeFileSync(join(roomDir, "albero", "sub", "nested.txt"), "nested bytes");
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

// One instrumented peer: catalog hook, RTC counter, frame audit.
async function openCatalogPeer(browser, url, { seedPicker = false } = {}) {
  const context = await browser.newContext();
  if (seedPicker) {
    // The picker itself is a native dialog no test can answer, so it is the
    // ONE thing replaced — and it is replaced by a REAL
    // `FileSystemDirectoryHandle`, taken from OPFS, so the traversal under
    // test still walks the engine's own handles, iterators and files.
    await context.addInitScript(() => {
      window.showDirectoryPicker = async () => {
        const store = await navigator.storage.getDirectory();
        try {
          await store.removeEntry("albero", { recursive: true });
        } catch {
          /* first run: nothing to remove */
        }
        const root = await store.getDirectoryHandle("albero", { create: true });
        const write = async (dir, name, text) => {
          const handle = await dir.getFileHandle(name, { create: true });
          const writable = await handle.createWritable();
          if (text.length > 0) {
            await writable.write(new TextEncoder().encode(text));
          }
          await writable.close();
        };
        await write(root, "alfa.txt", "alfa");
        await write(root, "caff\u00e8.txt", "unicode");
        await write(root, "zero.bin", "");
        const sub = await root.getDirectoryHandle("sub", { create: true });
        await write(sub, "nested.txt", "nested bytes");
        await root.getDirectoryHandle("vuota", { create: true });
        return root;
      };
    });
  }
  await context.addInitScript(() => {
    window.__BORE_TEST__ = window.__BORE_TEST__ ?? {};
    window.__rtcConstructed = 0;
    const RealRTC = window.RTCPeerConnection;
    if (RealRTC) {
      window.RTCPeerConnection = function (...args) {
        window.__rtcConstructed += 1;
        return new RealRTC(...args);
      };
    }
  });
  const page = await context.newPage();
  const failures = [];
  const frames = { sent: [], received: [], urls: [] };
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(`console: ${message.text()}`);
    }
  });
  page.on("websocket", (socket) => {
    frames.urls.push(socket.url());
    socket.on("framesent", (frame) => frames.sent.push(frame.payload));
    socket.on("framereceived", (frame) => frames.received.push(frame.payload));
  });
  await page.goto(url);
  return { context, page, failures, frames };
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 10_000 });
}

function outboundTypes(frames) {
  const types = new Set();
  for (const payload of frames.sent) {
    try {
      types.add(JSON.parse(String(payload)).type);
    } catch {
      /* non-JSON frame: reported separately */
    }
  }
  return [...types].sort();
}

async function catalogSnapshot(page) {
  return page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot());
}

function roomKeyOf(roomEnv) {
  // The short-link cutover deliberately keeps secrets out of browser
  // storage. The independent Node oracle is the test's cross-check view.
  return roomEnv.roomKey;
}

function checkMac(roomIdHex, roomKeyHex, manifest, macHex) {
  const key = crypto.hkdfSync(
    "sha256",
    Buffer.from(roomKeyHex, "hex"),
    Buffer.from("bore-web-manifest-v1"),
    Buffer.from(roomIdHex, "hex"),
    32,
  );
  return crypto.createHmac("sha256", key).update(canonicalize(manifest), "utf8").digest("hex") === macHex;
}

test.describe.serial("offer", () => {
  test("select, hash, publish and converge with verified MAC", async ({ browser }) => {
    const a = await openCatalogPeer(browser, env.roomUrl);
    const b = await openCatalogPeer(browser, env.roomUrl);
    const c = await openCatalogPeer(browser, env.roomUrl);
    for (const peer of [a, b, c]) {
      await expectConnected(peer.page);
    }
    // A selects one file: progress completes into a live card.
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "hello.txt")]);
    await expect(a.page.locator('.offer-card h4:has-text("hello.txt")')).toBeVisible({ timeout: 15_000 });
    // B selects two files (one multichunk): kind files, one offer.
    await b.page
      .locator("#file-input")
      .setInputFiles([join(roomDir, "big.bin"), join(roomDir, "note.txt")]);
    await expect(b.page.locator('.offer-card h4:has-text("2 files")')).toBeVisible({ timeout: 15_000 });
    // C renders both offers grouped under their publishers.
    await expect(c.page.locator(".offer-card")).toHaveCount(2, { timeout: 10_000 });
    const groups = await c.page.locator(".offer-group h3").allTextContents();
    expect(groups.length).toBe(2);
    // MAC cross-check from B's independent view and session key.
    const catalog = await catalogSnapshot(b.page);
    expect(catalog.length).toBe(2);
    const keyB = roomKeyOf(env);
    for (const offer of catalog) {
      expect(checkMac(env.roomId, keyB, offer.manifest, offer.mac)).toBe(true);
    }
    const fromA = catalog.find((o) => o.manifest.label === "hello.txt");
    expect(fromA.manifest.kind).toBe("file");
    expect(fromA.manifest.entries).toHaveLength(1);
    const fromB = catalog.find((o) => o.manifest.label === "2 files");
    expect(fromB.manifest.kind).toBe("files");
    expect(fromB.manifest.entries).toHaveLength(2);
    // Only owners see withdraw buttons on their own cards.
    expect(await a.page.locator('.offer-card:has-text("hello.txt") [data-withdraw]').count()).toBe(1);
    expect(await b.page.locator('.offer-card:has-text("hello.txt") [data-withdraw]').count()).toBe(0);
    expect(await c.page.locator('.offer-card:has-text("hello.txt") [data-withdraw]').count()).toBe(0);
    // A withdraws: every catalog converges back to one offer.
    await a.page.locator('.offer-card:has-text("hello.txt") [data-withdraw]').click();
    await expect(c.page.locator(".offer-card")).toHaveCount(1, { timeout: 10_000 });
    await expect(b.page.locator(".offer-card")).toHaveCount(1, { timeout: 10_000 });
    for (const peer of [a, b, c]) {
      expect(peer.failures).toEqual([]);
      await peer.context.close();
    }
  });

  test("changed source publishes a distinct offer with zero transfer traffic", async ({ browser }) => {
    const a = await openCatalogPeer(browser, env.roomUrl);
    const b = await openCatalogPeer(browser, env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "note.txt")]);
    await expect(a.page.locator('.offer-card h4:has-text("note.txt")')).toBeVisible({ timeout: 15_000 });
    const before = (await catalogSnapshot(a.page)).map((o) => o.manifest.entries[0].root);
    // Same path, new bytes: the worker hashes current content into a fresh
    // offer — published state never mutates in place.
    writeFileSync(join(roomDir, "note.txt"), "changed bytes here!!");
    await a.page.locator("#file-input").setInputFiles([join(roomDir, "note.txt")]);
    await expect(a.page.locator(".offer-card")).toHaveCount(2, { timeout: 15_000 });
    const after = (await catalogSnapshot(a.page)).map((o) => o.manifest.entries[0].root);
    expect(after.length).toBe(2);
    expect(after[0]).not.toBe(after[1]);
    expect(before).toEqual([after[0]]);
    // B sees both; no transfer, RTC or relay traffic anywhere.
    await expect(b.page.locator(".offer-card")).toHaveCount(2, { timeout: 10_000 });
    for (const peer of [a, b]) {
      for (const type of outboundTypes(peer.frames)) {
        expect(["hello", "peer.rename", "ping", "offer.publish", "offer.withdraw"]).toContain(type);
      }
      for (const url of peer.frames.urls) {
        expect(url).toContain("/transfer/ws/control/");
      }
      expect(await peer.page.evaluate(() => window.__rtcConstructed)).toBe(0);
      expect(peer.failures).toEqual([]);
      await peer.context.close();
    }
  });
});


// T-WEB-FOLDER-OFFER: a folder becomes ONE offer whose tree every peer can
// see and check before a single byte moves. Chromium walks real
// `FileSystemDirectoryHandle`s (the picker's dialog is the only thing
// replaced); Firefox and WebKit take the `webkitdirectory` input, which is
// the path their users actually have. The two produce the same manifest
// except for the empty directory, which the input path cannot report at all
// — the one documented difference, asserted here rather than assumed.
test.describe.serial("folder offer", () => {
  test("T-WEB-FOLDER-OFFER: a tree publishes as one verified offer and withdraws whole", async ({
    browser,
    browserName,
  }) => {
    const handlePath =
      browser.browserType().name() === "chromium" ||
      test.info().project.name === "chromium" ||
      (browserName ?? test.info().project.use.defaultBrowserType) === "chromium";
    const a = await openCatalogPeer(browser, env.roomUrl, { seedPicker: handlePath });
    const b = await openCatalogPeer(browser, env.roomUrl);
    const c = await openCatalogPeer(browser, env.roomUrl);
    for (const peer of [a, b, c]) {
      await expectConnected(peer.page);
    }

    if (handlePath) {
      await a.page.locator("#add-folder").click();
    } else {
      await a.page.locator("#folder-input").setInputFiles(join(roomDir, "albero"));
    }
    await expect(a.page.locator('.offer-card h4:has-text("albero")')).toBeVisible({
      timeout: 20_000,
    });

    // One selection, one offer — for every peer, not only its publisher.
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(1, { timeout: 15_000 });
    }

    // The manifest B holds is the tree A walked, MAC-checked from B's own
    // session key: a recipient believes the offer because it verified it.
    const catalog = await catalogSnapshot(b.page);
    expect(catalog).toHaveLength(1);
    const offer = catalog[0];
    expect(offer.manifest.kind).toBe("folder");
    expect(offer.manifest.label).toBe("albero");
    expect(checkMac(env.roomId, roomKeyOf(env), offer.manifest, offer.mac)).toBe(true);

    const files = [
      "albero/alfa.txt",
      "albero/caff\u00e8.txt",
      "albero/sub/nested.txt",
      "albero/zero.bin",
    ];
    const expectedPaths = handlePath
      ? [
          "albero/alfa.txt",
          "albero/caff\u00e8.txt",
          "albero/sub/nested.txt",
          "albero/vuota",
          "albero/zero.bin",
        ]
      : files;
    expect(offer.manifest.entries.map((entry) => entry.path)).toEqual(expectedPaths);
    // IDs are 0-based sequential and never the reserved archive ID.
    expect(offer.manifest.entries.map((entry) => entry.id)).toEqual(
      expectedPaths.map((_, index) => String(index)),
    );
    // The empty directory, where the engine can see one, is size 0 / no root;
    // a zero-byte FILE is size 0 WITH a root, and the two must not blur.
    const byPath = new Map(offer.manifest.entries.map((entry) => [entry.path, entry]));
    expect(byPath.get("albero/zero.bin").size).toBe("0");
    expect(byPath.get("albero/zero.bin").root).toEqual(expect.any(String));
    expect(byPath.get("albero/sub/nested.txt").size).toBe("12");
    if (handlePath) {
      expect(byPath.get("albero/vuota").size).toBe("0");
      expect(byPath.get("albero/vuota").root).toBe(null);
      expect(byPath.get("albero/vuota").chunks).toEqual([]);
    }

    // C renders the tree from that manifest alone: directory names appear as
    // disclosures, and every file name is on the page.
    await expect(c.page.locator(".offer-card .offer-tree")).toHaveCount(1, { timeout: 15_000 });
    const treeText = await c.page.locator(".offer-card .offer-tree").innerText();
    for (const path of expectedPaths) {
      const name = path.split("/").pop();
      expect(treeText).toContain(name);
    }
    expect(await c.page.locator(".offer-card .offer-tree summary").count()).toBeGreaterThan(0);
    // A small tree arrives expanded: the offer is readable without a click.
    expect(await c.page.locator(".offer-card .offer-tree details[open]").count()).toBeGreaterThan(0);

    // Nothing moved: no transfer was requested, no RTC was constructed and
    // no relay socket was opened by anyone.
    for (const peer of [a, b, c]) {
      for (const type of outboundTypes(peer.frames)) {
        expect(["hello", "peer.rename", "ping", "offer.publish", "offer.withdraw"]).toContain(type);
      }
      for (const url of peer.frames.urls) {
        expect(url).toContain("/transfer/ws/control/");
      }
      expect(await peer.page.evaluate(() => window.__rtcConstructed)).toBe(0);
    }

    // Withdraw removes the WHOLE offer — tree included — everywhere.
    await a.page.locator('.offer-card:has-text("albero") [data-withdraw]').click();
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(0, { timeout: 15_000 });
      await expect(peer.page.locator(".offer-tree")).toHaveCount(0);
      expect(peer.failures).toEqual([]);
      await peer.context.close();
    }
  });
});
