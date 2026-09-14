// T-WEB-OFFER: real file selection through the picker, worker preparation,
// server publish and catalog convergence across three engines.
//
// A selects one synthetic file and B selects two (one multichunk); every
// peer renders both offers grouped under their publishers; the MAC of A's
// offer is recomputed in Node from B's session key and compared; only the
// owner sees withdraw buttons; A withdraws; a changed source publishes as a
// distinct offer. Zero transfer/RTC/relay traffic throughout.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
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
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

// One instrumented peer: catalog hook, RTC counter, frame audit.
async function openCatalogPeer(browser, url) {
  const context = await browser.newContext();
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

async function roomKeyOf(page) {
  return page.evaluate((id) => {
    const raw = window.sessionStorage.getItem(`bore-transfer-v1:${id}`);
    return JSON.parse(raw).k;
  }, env.roomId);
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
    const keyB = await roomKeyOf(b.page);
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
