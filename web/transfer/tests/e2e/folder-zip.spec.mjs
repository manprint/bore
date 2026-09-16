// T-WEB-MULTIPEER-FINAL / T-WEB-OWNER-SEPARATION / T-WEB-SOURCE-ONLY (5.4):
// the acceptance suite. The twelve observable steps of the final scenario in
// the order a user performs them, plus the two rules that make the room what
// it is — the CLI that opened it is not a privileged browser, and a peer that
// RECEIVES never becomes a source of what it received.
//
// Every assertion here is against real evidence: bytes read back out of the
// saved archive, the path the recipient COMMITTED, the WebSocket URLs the
// page actually opened, and the catalog the app holds. UI text is read only
// where the text IS the product (the room-unavailable notice).
import { test, expect } from "@playwright/test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  Uint8ArrayReader,
  Uint8ArrayWriter,
  ZipReader,
  configure,
} from "@zip.js/zip.js";
import { openPersistentPeer, opfsWorks, spawnRoomEnv } from "./helpers.mjs";
import { hookCatalog, hookCounters } from "./fixtures.js";

configure({ useWebWorkers: false });

const CHUNK_BYTES = 1024 * 1024;
/** Paces every RELAY leg so a cancel lands inside a transfer, not after it. */
const SLOW_RATE = 1024 * 1024;
/** Short enough that step 12 can observe the room actually dying. */
const OWNER_GRACE = 5;

let env = null;
let roomDir = null;
/** `albero/<path>` → bytes, as published by A. */
const treeBytes = new Map();

function filler(length, salt) {
  const out = Buffer.alloc(length);
  for (let i = 0; i < length; i += 1) {
    out[i] = (i * 29 + salt * 13 + 7) % 251;
  }
  return out;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

test.beforeAll(async () => {
  env = await spawnRoomEnv({ relayRate: SLOW_RATE, ownerGrace: OWNER_GRACE });
  roomDir = mkdtempSync(join(tmpdir(), "bore-accept-"));
  mkdirSync(join(roomDir, "albero", "sub"), { recursive: true });
  for (const [name, bytes] of [
    ["uno.txt", Buffer.from("uno")],
    ["sub/grande.bin", filler(3 * CHUNK_BYTES, 1)],
  ]) {
    writeFileSync(join(roomDir, "albero", name), bytes);
    treeBytes.set(`albero/${name}`, bytes);
  }
  // B publishes one file; C publishes a folder big enough that a relayed
  // leg of it is still running when the cancel of step 9 is aimed at it.
  writeFileSync(join(roomDir, "beta.bin"), filler(2 * CHUNK_BYTES, 2));
  mkdirSync(join(roomDir, "gamma"), { recursive: true });
  writeFileSync(join(roomDir, "gamma", "grosso.bin"), filler(6 * CHUNK_BYTES, 3));
  writeFileSync(join(roomDir, "gamma", "nota.txt"), Buffer.from("gamma"));
}, 90_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openPeer(options = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(env.roomUrl, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    ...options,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", {
    timeout: 20_000,
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

/** Publishes one folder or file and returns the offer ID the peers see. */
async function publish(peer, selector, path, label) {
  await peer.page.locator(selector).setInputFiles(join(roomDir, path));
  await expect(
    peer.page.locator(`.offer-card h4:has-text("${label}")`),
  ).toBeVisible({ timeout: 30_000 });
  return poll(peer.page, () => {
    const catalog = window.__BORE_TEST__.getCatalogSnapshot();
    const self = window.__BORE_TEST__.selfPeerId;
    const mine = catalog.filter((offer) => offer.peerId === self);
    return mine.length > 0 ? mine[mine.length - 1].offerId : null;
  });
}

/** Saves the staged file/archive and returns `{ name, bytes }`. */
async function save(page) {
  const name = await page.locator("#save-name").textContent();
  const download = await Promise.all([
    page.waitForEvent("download", { timeout: 90_000 }),
    page.locator("#save-file").click(),
  ]).then(([event]) => event);
  const bytes = readFileSync(await download.path());
  await expect(page.locator("#save-file")).toBeHidden({ timeout: 30_000 });
  return { name: name?.trim() ?? "", bytes };
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

function relaySockets(urls) {
  return urls.filter((url) => url.includes("/transfer/ws/relay/"));
}

/**
 * Every control that can START a download must name exactly one offer. The
 * scan is over the WHOLE page, not the catalog, because the thing being
 * refused is a room-wide archive action existing anywhere at all.
 */
async function downloadControlsAreScoped(page) {
  return page.evaluate(() => {
    const scoped = [];
    const unscoped = [];
    for (const button of document.querySelectorAll("button")) {
      const named =
        button.getAttribute("data-download") ??
        button.getAttribute("data-download-zip") ??
        button.getAttribute("data-download-entry") ??
        button.getAttribute("data-restart");
      const label = (button.textContent ?? "").trim();
      if (named !== null) {
        scoped.push({ named, label, inCatalog: document.getElementById("catalog")?.contains(button) === true });
      } else if (/scarica|riprendi|riparti/i.test(label)) {
        unscoped.push(label);
      }
    }
    return { scoped, unscoped };
  });
}

test.describe.serial("acceptance", () => {
  test("T-WEB-OWNER-SEPARATION: the CLI holds a lease, not a seat, and joining first buys nothing", async () => {
    test.setTimeout(180_000);
    // B joins FIRST and publishes; the browser on the machine that ran the
    // CLI (A) arrives afterwards. If the owner were a privileged browser at
    // all, this is the ordering that would show it.
    const b = await openPeer();
    await expectConnected(b.page);
    // One peer in the list: the lease holder occupies no seat.
    await expect(b.page.locator("#peer-list li")).toHaveCount(1, { timeout: 20_000 });
    const beta = await publish(b, "#file-input", "beta.bin", "beta.bin");

    const a = await openPeer();
    await expectConnected(a.page);
    for (const peer of [a, b]) {
      await expect(peer.page.locator("#peer-list li")).toHaveCount(2, { timeout: 20_000 });
    }
    // A sees B's offer and can download it — and nothing else. The only
    // card carrying a withdraw is the one its own peer published, which for
    // A is none at all.
    await expect(a.page.locator(`button[data-download="${beta}"]`)).toBeVisible({
      timeout: 30_000,
    });
    expect(await a.page.locator("[data-withdraw]").count()).toBe(0);
    expect(await b.page.locator("[data-withdraw]").count()).toBe(1);
    // Neither page carries an owner marker: the catalog attributes every
    // offer to a peer ID, and no peer is distinguished.
    const catalog = await hookCatalog(a.page);
    expect(catalog.map((offer) => offer.offerId)).toEqual([beta]);
    const bSelf = await b.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
    expect(catalog[0].peerId).toBe(bSelf);

    // Withdraw and leave: the room survives its browsers entirely.
    await b.page.locator("[data-withdraw]").click();
    await expect(a.page.locator(".offer-card")).toHaveCount(0, { timeout: 20_000 });
    await b.cleanup();
    await expect(a.page.locator("#peer-list li")).toHaveCount(1, { timeout: 20_000 });
    await expectConnected(a.page);
    expect(a.failures).toEqual([]);
    await a.cleanup();
  });

  test("T-WEB-SOURCE-ONLY: a recipient serves nothing until it publishes, and then as itself", async () => {
    test.setTimeout(240_000);
    const a = await openPeer();
    const b = await openPeer();
    for (const peer of [a, b]) {
      await expectConnected(peer.page);
    }
    expect(await opfsWorks(b.page)).toBe(true);
    const beta = await publish(a, "#file-input", "beta.bin", "beta.bin");
    await expect(b.page.locator(`button[data-download="${beta}"]`)).toBeVisible({
      timeout: 30_000,
    });

    await b.page.locator(`button[data-download="${beta}"]`).click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
    const saved = await save(b.page);
    expect(saved.name).toBe("beta.bin");
    expect(sha256(saved.bytes)).toBe(sha256(readFileSync(join(roomDir, "beta.bin"))));

    // The whole point: B has the bytes and publishes NOTHING. One offer in
    // the room, owned by A, and B never sent an `offer.publish`.
    const aSelf = await a.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
    const bSelf = await b.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
    for (const peer of [a, b]) {
      const catalog = await hookCatalog(peer.page);
      expect(catalog.map((offer) => offer.offerId)).toEqual([beta]);
      expect(catalog[0].peerId).toBe(aSelf);
    }
    expect((await hookCounters(b.page)).outboundTypes).not.toContain("offer.publish");

    // An explicit republish by B is a DIFFERENT offer with a different
    // source, and it leaves A's untouched.
    const republished = await publish(b, "#file-input", "beta.bin", "beta.bin");
    expect(republished).not.toBe(beta);
    for (const peer of [a, b]) {
      await expect(async () => {
        const catalog = await hookCatalog(peer.page);
        expect(catalog.map((offer) => offer.offerId).sort()).toEqual([beta, republished].sort());
      }).toPass({ timeout: 45_000 });
      const catalog = await hookCatalog(peer.page);
      expect(catalog.find((offer) => offer.offerId === beta).peerId).toBe(aSelf);
      expect(catalog.find((offer) => offer.offerId === republished).peerId).toBe(bSelf);
    }
    for (const peer of [a, b]) {
      await peer.page.locator("[data-withdraw]").click();
    }
    for (const peer of [a, b]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(0, { timeout: 20_000 });
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("T-WEB-MULTIPEER-FINAL: the twelve observable steps of the final scenario", async () => {
    test.setTimeout(600_000);
    // (1) The CLI is already running and selected no file. (2) A joins and
    // the catalog is empty — a room with a live owner holds nothing.
    const a = await openPeer();
    await expectConnected(a.page);
    await expect(a.page.locator("#peer-list li")).toHaveCount(1, { timeout: 20_000 });
    expect(await a.page.locator(".offer-card").count()).toBe(0);
    expect((await hookCounters(a.page)).outboundTypes).not.toContain("transfer.request");

    // (3) A selects a folder, and ONLY then does the offer exist.
    const tree = await publish(a, "#folder-input", "albero", "albero");
    expect(await a.page.locator(".offer-card").count()).toBe(1);

    // (4) B joins, sees the tree read from the signed manifest alone, and
    // nothing starts: no request, no transfer row, no RTC object.
    const b = await openPeer();
    await expectConnected(b.page);
    await expect(b.page.locator(`button[data-download-zip="${tree}"]`)).toBeVisible({
      timeout: 30_000,
    });
    await expect(b.page.locator(".offer-card .tree-file")).toHaveCount(2, { timeout: 20_000 });
    {
      const counters = await hookCounters(b.page);
      expect(counters.outboundTypes).not.toContain("transfer.request");
      expect(counters.transferRows).toBe(0);
      expect(counters.rtc).toBe(0);
      expect(counters.fileReads).toBe(0);
    }

    // (5) One click on `Scarica ZIP`: the bytes ride a real DataChannel and
    // the archive holds the whole tree.
    expect(await opfsWorks(b.page)).toBe(true);
    await b.page.locator(`button[data-download-zip="${tree}"]`).click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 240_000 });
    const direct = await save(b.page);
    expect(direct.name).toBe("albero.zip");
    {
      const entries = await readArchive(direct.bytes);
      expect([...entries.keys()].sort()).toEqual([...treeBytes.keys()].sort());
      for (const [path, bytes] of treeBytes) {
        expect(Buffer.compare(entries.get(path), bytes)).toBe(0);
      }
      const commits = await b.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((entry) => entry.path)).toEqual(["direct"]);
      expect(relaySockets((await hookCounters(b.page)).wsUrls)).toEqual([]);
    }

    // (6) C joins with ICE restricted to relay against a room that offers no
    // TURN, clicks the SAME offer, and the same click finishes on the relay
    // with byte-identical output.
    const c = await openPeer({ iceRelayOnly: true });
    await expectConnected(c.page);
    expect(await opfsWorks(c.page)).toBe(true);
    await c.page.locator(`button[data-download-zip="${tree}"]`).click();
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const relayed = await save(c.page);
    expect(sha256(relayed.bytes)).toBe(sha256(direct.bytes));
    {
      const commits = await c.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((entry) => entry.path)).toEqual(["relay"]);
      const counters = await hookCounters(c.page);
      expect(relaySockets(counters.wsUrls).length).toBe(1);
      // One gesture, one request: the fallback asked for nothing more.
      expect(counters.outboundTypes.filter((t) => t === "transfer.request").length).toBe(1);
    }

    // (7) B and C publish their own; every peer converges on three offers
    // with three distinct owners.
    const beta = await publish(b, "#file-input", "beta.bin", "beta.bin");
    const gamma = await publish(c, "#folder-input", "gamma", "gamma");
    const selves = {};
    for (const [name, peer] of Object.entries({ a, b, c })) {
      selves[name] = await peer.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
    }
    for (const peer of [a, b, c]) {
      await expect(async () => {
        const catalog = await hookCatalog(peer.page);
        expect(catalog.map((offer) => offer.offerId).sort()).toEqual([tree, beta, gamma].sort());
        expect(new Set(catalog.map((offer) => offer.peerId)).size).toBe(3);
      }).toPass({ timeout: 60_000 });
    }

    // (8) A downloads from B and from C, and each transfer names the right
    // source: the attribution is the server's, not a label in the page.
    await a.page.locator(`button[data-download="${beta}"]`).click();
    await expect(a.page.locator("#save-file")).toBeVisible({ timeout: 240_000 });
    const fromB = await save(a.page);
    expect(sha256(fromB.bytes)).toBe(sha256(readFileSync(join(roomDir, "beta.bin"))));
    // B served it, and the server told B so: a progress notice reaches the
    // SOURCE alone, so its presence on B is the server's own attribution.
    await expect(async () => {
      const notices = await b.page.evaluate(() => [...window.__BORE_TEST__.progressNotices]);
      expect(notices.length).toBeGreaterThan(0);
    }).toPass({ timeout: 30_000 });

    await a.page.locator(`button[data-download-zip="${gamma}"]`).click();
    // This leg is relayed and paced, so the live row can be read while it
    // exists: the transfer itself names C as the source.
    const gammaSource = await poll(a.page, () => {
      const row = window.__BORE_TEST__.receiverState()[0];
      return row === undefined ? null : (row.sourcePeerId ?? "unknown");
    });
    expect(gammaSource).toBe(selves.c);
    await expect(a.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const fromC = await save(a.page);
    {
      const entries = await readArchive(fromC.bytes);
      expect([...entries.keys()].sort()).toEqual(["gamma/grosso.bin", "gamma/nota.txt"]);
      expect(
        Buffer.compare(entries.get("gamma/nota.txt"), Buffer.from("gamma")),
      ).toBe(0);
    }

    // (9) Two transfers at once from two sources, and exactly one cancel.
    // Both legs involve C, whose ICE cannot pair, so both ride the relay and
    // are paced by the server — the cancel has a window that does not depend
    // on how fast this machine is.
    await b.page.locator(`button[data-download-zip="${gamma}"]`).click();
    await c.page.locator(`button[data-download="${beta}"]`).click();
    const mine = await poll(b.page, () => {
      const rows = window.__BORE_TEST__.receiverState();
      return rows.length === 1 ? rows[0].transferId : null;
    });
    const row = `.transfer-row[data-transfer="${mine}"]`;
    await expect(async () => {
      const value = await b.page.locator(`${row} progress`).getAttribute("value");
      expect(Number(value)).toBeGreaterThanOrEqual(10);
    }).toPass({ timeout: 240_000 });
    await b.page.locator(`button[data-cancel="${mine}"]`).click();
    await expect(b.page.locator(`${row} .transfer-state`)).toContainText("Annullato");
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);
    // The concurrent one is untouched and delivers the exact bytes.
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const concurrent = await save(c.page);
    expect(sha256(concurrent.bytes)).toBe(sha256(readFileSync(join(roomDir, "beta.bin"))));

    // (10) Nothing any peer RECEIVED became an offer: three offers, one per
    // peer, each still owned by whoever published it.
    for (const peer of [a, b, c]) {
      const catalog = await hookCatalog(peer.page);
      expect(catalog).toHaveLength(3);
      expect(catalog.find((offer) => offer.offerId === tree).peerId).toBe(selves.a);
      expect(catalog.find((offer) => offer.offerId === beta).peerId).toBe(selves.b);
      expect(catalog.find((offer) => offer.offerId === gamma).peerId).toBe(selves.c);
    }
    for (const [peer, count] of [[a, 1], [b, 1], [c, 1]]) {
      const counters = await hookCounters(peer.page);
      expect(counters.outboundTypes.filter((t) => t === "offer.publish").length).toBe(count);
    }

    // (11) No room-wide archive control exists anywhere on any page: every
    // control that can start a download names exactly one offer, and lives
    // in the catalog.
    for (const peer of [a, b, c]) {
      const { scoped, unscoped } = await downloadControlsAreScoped(peer.page);
      expect(unscoped).toEqual([]);
      expect(scoped.length).toBeGreaterThan(0);
      for (const control of scoped) {
        expect(control.inCatalog).toBe(true);
        expect(control.named.startsWith(tree) || control.named.startsWith(beta) || control.named.startsWith(gamma)).toBe(true);
      }
    }

    // (12) Closing the owner ends the room for everyone, and the URL stops
    // working: the lease is what the room is.
    env.killOwner();
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator("#room-status")).toContainText("non disponibile", {
        timeout: (OWNER_GRACE + 45) * 1000,
      });
    }
    const late = await openPeer();
    await expect(late.page.locator("#room-status")).toContainText("non disponibile", {
      timeout: 60_000,
    });
    expect(await late.page.locator(".offer-card").count()).toBe(0);
    await late.cleanup();
    for (const peer of [a, b, c]) {
      await peer.cleanup();
    }
  });
});
