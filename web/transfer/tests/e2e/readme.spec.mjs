// T-WEB-README-RELAY (3.8): the README's own commands, run verbatim.
//
// A guide is only true if its commands are. This leg starts the server and
// the room with the exact invocations printed in README.md, asserts the two
// documented stdout lines, then walks the documented browser flow — publish,
// explicit download, cancel, resume, explicit save — and finally the
// documented Ctrl+C, after which every page must say the room is gone
// instead of reconnecting to something that can never return.
import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { boreBin, freePort, waitPort, openPersistentPeer, opfsWorks } from "./helpers.mjs";

// The README documents the throttle as the one operator knob; using it here
// makes a mid-transfer cancel land at the SERVER's pace, not this machine's.
const RELAY_RATE = 1024 * 1024;

let server = null;
let owner = null;
let roomUrl = null;
let dir = null;
let bytes = null;
let hashHex = null;

async function readmeRoom() {
  const port = await freePort();
  // README, "Server setup":
  //   bore server --control-port 7835 --web-transfer-base-url https://files.example.com
  // with the documented loopback development origin.
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
  const asset = await fetch(`http://127.0.0.1:${port}/transfer/assets/app.js`).then((r) => r.text());
  if (!asset.includes("peer-list")) {
    server.kill("SIGKILL");
    throw new Error("stale embedded app bundle: run cargo build --all-features after npm run build");
  }

  // README, "Opening a room":
  //   bore transfer web --to https://files.example.com
  //   room: https://…/transfer/8f1c…#m=…&k=…
  //   room active; press Ctrl+C to close
  owner = spawn(boreBin, ["transfer", "web", "--to", `http://127.0.0.1:${port}`], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  owner.on("error", (error) => {
    throw new Error(`cannot spawn ${boreBin} transfer web: ${error.message}`);
  });
  let stdout = "";
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`no room URL on stdout; saw ${JSON.stringify(stdout)}`)), 30_000);
    owner.stdout.on("data", (chunk) => {
      stdout += String(chunk);
      const lines = stdout.split("\n");
      if (lines.length >= 2 && lines[1].startsWith("room active")) {
        clearTimeout(timer);
        resolve(lines);
      }
    });
    owner.stdout.on("error", reject);
  });
  // Exactly the two documented lines, in the documented order.
  expect(url[0]).toMatch(/^room: http:\/\/127\.0\.0\.1:\d+\/transfer\/[0-9a-f]{32}#m=[0-9a-f]{64}&k=[0-9a-f]{64}$/);
  expect(url[1].trim()).toBe("room active; press Ctrl+C to close");
  expect(url.slice(2).join("").trim()).toBe("");
  return url[0].slice("room: ".length).trim();
}

test.beforeAll(async () => {
  roomUrl = await readmeRoom();
  dir = mkdtempSync(join(tmpdir(), "bore-readme-"));
  bytes = Buffer.alloc(3 * 1024 * 1024 + 5);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = (i * 13 + 5) % 251;
  }
  hashHex = createHash("sha256").update(bytes).digest("hex");
  writeFileSync(join(dir, "readme.bin"), bytes);
}, 90_000);

test.afterAll(async () => {
  owner?.kill("SIGKILL");
  server?.kill("SIGKILL");
  if (dir) {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function openPeer(url) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  // The README still documents the relay flow; 4.5 rewrites it direct-first
  // and this gate with it.
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    noWebRtc: true,
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
      throw new Error("readme e2e poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

test.describe.serial("readme-relay", () => {
  test("the documented commands and the documented flow", async () => {
    test.setTimeout(240_000);
    const a = await openPeer(roomUrl);
    const b = await openPeer(roomUrl);
    for (const peer of [a, b]) {
      await expect(peer.page.locator("#room-status")).toContainText("Connesso", { timeout: 20_000 });
    }
    expect(await opfsWorks(b.page)).toBe(true);

    // README step 2: publishing announces, it does not send.
    await a.page.locator("#file-input").setInputFiles([join(dir, "readme.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const button = b.page.locator(`button[data-download="${offer.offerId}"]`);
    await expect(button).toHaveText("Scarica");
    // Nothing moved on its own.
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);

    // README step 3: one click, one transfer.
    await button.click();
    await poll(
      b.page,
      () => {
        const bar = document.querySelector(".transfer-row progress");
        return bar !== null && bar.value >= 20 ? bar.value : null;
      },
      90_000,
    );

    // README "Cancel and resume": cancel keeps the partial, and only a
    // second click restarts it.
    await b.page.locator(".transfer-row button[data-cancel]").click();
    await expect(b.page.locator(".transfer-row .transfer-state")).toContainText("Annullato");
    await expect(button).toHaveText("Riprendi", { timeout: 20_000 });
    await button.click();

    // README step 4: verified, then saved on purpose.
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
    await expect(b.page.locator("#save-name")).toHaveText("readme.bin");
    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    expect(saved.length).toBe(bytes.length);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(hashHex);

    // Nothing went wrong on either page while the room was alive. Checked
    // HERE and not at the end: closing the room resets the control socket,
    // and WebKit reports that reset as a page console error — engine noise
    // about the very thing the next assertion is about to require.
    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
    }

    // README "Ctrl+C destroys the room immediately": SIGINT is what Ctrl+C
    // sends, and the pages must report the room gone rather than reconnect.
    owner.kill("SIGINT");
    for (const peer of [a, b]) {
      await expect(peer.page.locator("#room-status")).toContainText("non disponibile", {
        timeout: 60_000,
      });
    }

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });
});
