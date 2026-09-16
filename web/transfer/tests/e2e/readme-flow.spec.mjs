// T-WEB-README-FINAL-FLOW (5.5): the seven steps of "A complete run, three
// browsers", performed with nothing but what README.md documents.
//
// The two earlier README gates prove the COMMANDS are real (`readme.spec.mjs`)
// and that the documented direct/relay wording matches the product
// (`readme-direct.spec.mjs`). This one proves the WALKTHROUGH is real: a
// reader who follows the numbered list and nothing else ends up with the
// archive, the second publisher, the relay peer, the cancel, the resume and
// the dead room the guide promises.
import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { boreBin, freePort, waitPort, openPersistentPeer, opfsWorks } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

// The README names this as the one operator knob on the relay path. Pacing it
// is also what gives step 6 a cancel that lands mid-transfer instead of after
// the file is already there.
const RELAY_RATE = 1024 * 1024;

let server = null;
let owner = null;
let roomUrl = null;
let dir = null;
let beta = null;
let betaHash = null;

function filler(length, salt) {
  const out = Buffer.alloc(length);
  for (let i = 0; i < length; i += 1) {
    out[i] = (i * 29 + salt * 7 + 11) % 251;
  }
  return out;
}

/** The two documented commands, run verbatim. */
async function readmeRoom() {
  const port = await freePort();
  // README, "Server setup".
  server = spawn(
    boreBin,
    [
      "server",
      "--control-port",
      String(port),
      "--web-transfer-base-url",
      `http://127.0.0.1:${port}/`,
      "--web-transfer-relay-rate",
      String(RELAY_RATE),
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  server.on("error", (error) => {
    throw new Error(`cannot spawn ${boreBin}: ${error.message} (run cargo build --all-features first)`);
  });
  await waitPort(port);

  // README, "Opening a room".
  owner = spawn(boreBin, ["transfer", "web", "--to", `http://127.0.0.1:${port}`], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  owner.on("error", (error) => {
    throw new Error(`cannot spawn ${boreBin} transfer web: ${error.message}`);
  });
  let stdout = "";
  const lines = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error(`no room URL on stdout; saw ${JSON.stringify(stdout)}`)),
      30_000,
    );
    owner.stdout.on("data", (chunk) => {
      stdout += String(chunk);
      const seen = stdout.split("\n");
      if (seen.length >= 2 && seen[1].startsWith("room active")) {
        clearTimeout(timer);
        resolve(seen);
      }
    });
    owner.stdout.on("error", reject);
  });
  expect(lines[1].trim()).toBe("room active; press Ctrl+C to close");
  return lines[0].slice("room: ".length).trim();
}

test.beforeAll(async () => {
  roomUrl = await readmeRoom();
  dir = mkdtempSync(join(tmpdir(), "bore-readme-flow-"));
  // Step 2's folder: nesting and two sizes, small enough that three engines
  // can afford it and large enough to be a real archive.
  mkdirSync(join(dir, "progetto", "note"), { recursive: true });
  writeFileSync(join(dir, "progetto", "relazione.bin"), filler(1536 * 1024, 1));
  writeFileSync(join(dir, "progetto", "note", "appunti.txt"), Buffer.from("appunti del progetto\n"));
  // Step 5's file, and step 6's cancel window at the documented throttle.
  beta = filler(4 * 1024 * 1024 + 3, 2);
  betaHash = createHash("sha256").update(beta).digest("hex");
  writeFileSync(join(dir, "beta.bin"), beta);
}, 120_000);

test.afterAll(async () => {
  owner?.kill("SIGKILL");
  server?.kill("SIGKILL");
  if (dir) {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function openPeer(options = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(roomUrl, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    ...options,
  });
}

async function connected(peer) {
  await expect(peer.page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
}

async function poll(page, fn, timeoutMs = 60_000, arg = undefined) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("readme-flow poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

/** The offer this peer has just published, read from the catalog. */
async function published(peer) {
  return poll(peer.page, () => {
    const catalog = window.__BORE_TEST__.getCatalogSnapshot();
    const self = window.__BORE_TEST__.selfPeerId;
    const mine = catalog.filter((offer) => offer.peerId === self);
    return mine.length > 0 ? mine[mine.length - 1].offerId : null;
  });
}

test.describe.serial("readme-flow", () => {
  test("T-WEB-README-FINAL-FLOW the seven documented steps, three browsers", async () => {
    test.setTimeout(600_000);

    // (1) The room is open and the CLI selected nothing: it is not a peer and
    // it holds no offer.
    const a = await openPeer();
    await connected(a);
    await expect(a.page.locator("#peer-list li")).toHaveCount(1, { timeout: 20_000 });
    expect(await a.page.locator(".offer-card").count()).toBe(0);

    // (2) A publishes a folder with `Aggiungi cartella`. ONE offer, and a tree
    // every other browser reads from the announcement alone.
    await a.page.locator("#folder-input").setInputFiles(join(dir, "progetto"));
    const folder = await published(a);
    expect(await a.page.locator(".offer-card").count()).toBe(1);

    // (3) B joins and downloads the folder as one archive over the direct path.
    const b = await openPeer();
    await connected(b);
    const zipButton = b.page.locator(`button[data-download-zip="${folder}"]`);
    await expect(zipButton).toBeVisible({ timeout: 30_000 });
    await expect(zipButton).toHaveText("Scarica ZIP");
    {
      // Nothing started on its own, exactly as the guide says.
      const counters = await hookCounters(b.page);
      expect(counters.outboundTypes).not.toContain("transfer.request");
      expect(counters.transferRows).toBe(0);
    }
    expect(await opfsWorks(b.page)).toBe(true);
    await zipButton.click();
    await expect(b.page.locator(".transfer-path")).toHaveAttribute("data-path", "direct", {
      timeout: 120_000,
    });
    await expect(b.page.locator(".transfer-path .path-word")).toHaveText("diretto");
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 240_000 });
    const archive = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(archive.suggestedFilename()).toBe("progetto.zip");
    expect(readFileSync(await archive.path()).length).toBeGreaterThan(1024);

    // (4) C joins from a network that blocks the direct path: same button,
    // same archive, one automatic replacement attempt, and the row says
    // `relay`.
    const c = await openPeer({ noWebRtc: true });
    await connected(c);
    expect(await opfsWorks(c.page)).toBe(true);
    const cZip = c.page.locator(`button[data-download-zip="${folder}"]`);
    await expect(cZip).toBeVisible({ timeout: 30_000 });
    await cZip.click();
    await expect(c.page.locator(".transfer-path")).toHaveAttribute("data-path", "relay", {
      timeout: 240_000,
    });
    await expect(c.page.locator(".transfer-path .path-word")).toHaveText("relay");
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const relayArchive = await Promise.all([
      c.page.waitForEvent("download", { timeout: 60_000 }),
      c.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    // Same file by the guide's own claim: "same file, same hash, same archive".
    expect(createHash("sha256").update(readFileSync(await relayArchive.path())).digest("hex")).toBe(
      createHash("sha256").update(readFileSync(await archive.path())).digest("hex"),
    );

    // (5) B publishes something of its own with `Aggiungi file`. Two offers,
    // two publishers — and B receiving A's folder did NOT make B a source for
    // it: B's only announcement is the one it just made on purpose.
    await b.page.locator("#file-input").setInputFiles(join(dir, "beta.bin"));
    const file = await published(b);
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(2, { timeout: 30_000 });
    }
    expect(
      (await hookCounters(b.page)).outboundTypes.filter((t) => t === "offer.publish").length,
    ).toBe(1);
    // And A can download B's file, which is the symmetry the guide claims.
    expect(await opfsWorks(a.page)).toBe(true);
    await a.page.locator(`button[data-download="${file}"]`).click();
    await expect(a.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const got = await Promise.all([
      a.page.waitForEvent("download", { timeout: 60_000 }),
      a.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(createHash("sha256").update(readFileSync(await got.path())).digest("hex")).toBe(betaHash);
    // A received a file and announced nothing new: a download is not a
    // republish. A published exactly once in this run — the folder, in step 2.
    expect(
      (await hookCounters(a.page)).outboundTypes.filter((t) => t === "offer.publish").length,
    ).toBe(1);
    await expect(a.page.locator(".offer-card")).toHaveCount(2);

    // (6) Somebody changes their mind. C pulls the same file over the paced
    // relay, cancels mid-transfer, and the offer offers to resume — on a
    // click, and on nothing else.
    const cFile = c.page.locator(`button[data-download="${file}"]`);
    await cFile.click();
    // C already finished the archive, so its page holds two rows: every
    // selector below names the ONE this step is about.
    const cTransfer = await poll(c.page, (offerId) => {
      const row = window.__BORE_TEST__.receiverState().find((t) => t.offerId === offerId);
      return row?.transferId ?? null;
    }, 60_000, file);
    await poll(
      c.page,
      (id) => {
        const bar = document.querySelector(`.transfer-row[data-transfer="${id}"] progress`);
        return bar !== null && bar.value >= 15 ? bar.value : null;
      },
      120_000,
      cTransfer,
    );
    await c.page.locator(`button[data-cancel="${cTransfer}"]`).click();
    await expect(
      c.page.locator(`.transfer-row[data-transfer="${cTransfer}"] .transfer-state`),
    ).toContainText("Annullato");
    await expect(cFile).toHaveText("Riprendi", { timeout: 30_000 });
    // A reload does not restart it: resume is click-only.
    await c.page.reload();
    await connected(c);
    await expect(c.page.locator(`button[data-download="${file}"]`)).toHaveText("Riprendi", {
      timeout: 30_000,
    });
    expect(await c.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);
    await c.page.locator(`button[data-download="${file}"]`).click();
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
    const resumed = await Promise.all([
      c.page.waitForEvent("download", { timeout: 60_000 }),
      c.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(createHash("sha256").update(readFileSync(await resumed.path())).digest("hex")).toBe(
      betaHash,
    );

    // Checked here, while the room is alive: closing it resets the control
    // socket and WebKit reports that reset as a page error — engine noise
    // about the very thing step 7 is about to require.
    for (const peer of [a, b, c]) {
      expect(peer.failures.filter((line) => !/RTCDataChannel/.test(line))).toEqual([]);
    }

    // (7) A closes the room with Ctrl+C. Every page says so; the URL is dead.
    owner.kill("SIGINT");
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator("#room-status")).toContainText("non disponibile", {
        timeout: 90_000,
      });
    }
    for (const peer of [a, b, c]) {
      await peer.cleanup();
    }
  });
});
