// T-WEB-README-DIRECT (4.5): the README's direct-first promise, run.
//
// The README now says that every transfer tries the direct path first, that
// the relay takes over by itself, and that the row names the path that
// carried the bytes. Those are three claims a reader will act on, so this
// gate runs the documented commands verbatim and then performs the SAME
// documented gesture twice — once in a browser that can open a DataChannel
// and once in a browser whose ICE cannot pair — asserting that the user does
// nothing different, that the saved bytes are identical, and that the badge
// tells the two apart.
//
// The server is started with NO STUN flag on purpose: that is the documented
// default, and on loopback the host candidates it produces are enough, so the
// gate proves the default configuration works without reaching a public
// service.
import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { boreBin, freePort, waitPort, openPersistentPeer, opfsWorks } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

let server = null;
let owner = null;
let roomUrl = null;
let dir = null;
let bytes = null;
let hashHex = null;

/** The two commands the README prints, with the documented loopback origin. */
async function readmeRoom() {
  const port = await freePort();
  // README, "Server setup":
  //   bore server --control-port 7835 --web-transfer-base-url https://files.example.com
  server = spawn(
    boreBin,
    ["server", "--control-port", String(port), "--web-transfer-base-url", `http://127.0.0.1:${port}/`],
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
  // README, "Opening a room": bore transfer web --to https://files.example.com
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
      const split = stdout.split("\n");
      if (split.length >= 2 && split[1].startsWith("room active")) {
        clearTimeout(timer);
        resolve(split);
      }
    });
    owner.stdout.on("error", reject);
  });
  expect(lines[0]).toMatch(
    /^room: http:\/\/127\.0\.0\.1:\d+\/transfer\/[0-9a-f]{32}#m=[0-9a-f]{64}&k=[0-9a-f]{64}$/,
  );
  expect(lines[1].trim()).toBe("room active; press Ctrl+C to close");
  return lines[0].slice("room: ".length).trim();
}

test.beforeAll(async () => {
  roomUrl = await readmeRoom();
  dir = mkdtempSync(join(tmpdir(), "bore-readme-direct-"));
  bytes = Buffer.alloc(2 * 1024 * 1024 + 3);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = (i * 29 + 11) % 253;
  }
  hashHex = createHash("sha256").update(bytes).digest("hex");
  writeFileSync(join(dir, "readme-direct.bin"), bytes);
}, 90_000);

test.afterAll(async () => {
  owner?.kill("SIGKILL");
  server?.kill("SIGKILL");
  if (dir) {
    rmSync(dir, { recursive: true, force: true });
  }
});

/** A documented peer; `iceRelayOnly` is the "UDP/WebRTC blocked" case. */
async function openPeer(url, options = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    ...options,
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
      throw new Error("readme-direct poll timed out");
    }
    await new Promise((r) => setTimeout(r, 150));
  }
}

/** A console line the ENGINE writes about the ICE failure the test imposes. */
function appFailures(failures) {
  return failures.filter((line) => !/ICE failed|add a TURN server/i.test(line));
}

test.describe.serial("readme-direct", () => {
  test("T-WEB-README-DIRECT one documented gesture, both documented paths", async () => {
    test.setTimeout(240_000);
    const a = await openPeer(roomUrl);
    // The documented "UDP/WebRTC blocked" reader: WebRTC is present and ICE
    // is forced to `relay` with no TURN configured, so it fails on its own
    // exactly as a blocked network makes it fail.
    const direct = await openPeer(roomUrl);
    const blocked = await openPeer(roomUrl, { iceRelayOnly: true });
    for (const peer of [a, direct, blocked]) {
      await expect(peer.page.locator("#room-status")).toContainText("Connesso", { timeout: 20_000 });
    }
    for (const peer of [direct, blocked]) {
      expect(await opfsWorks(peer.page)).toBe(true);
    }

    // README step 2: publishing announces; one file, published once.
    await a.page.locator("#file-input").setInputFiles([join(dir, "readme-direct.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });

    // README step 3: ONE click each, and nothing else. The two readers differ
    // in one thing only — whether their ICE can pair.
    for (const peer of [direct, blocked]) {
      const button = peer.page.locator(`button[data-download="${offer.offerId}"]`);
      await expect(button).toHaveText("Scarica");
      await button.click();
    }

    // README step 4: verified, then saved on purpose — on both paths.
    const saved = {};
    for (const [name, peer] of [
      ["direct", direct],
      ["blocked", blocked],
    ]) {
      await expect(peer.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
      await expect(peer.page.locator("#save-name")).toHaveText("readme-direct.bin");
      const download = await Promise.all([
        peer.page.waitForEvent("download", { timeout: 60_000 }),
        peer.page.locator("#save-file").click(),
      ]).then(([event]) => event);
      saved[name] = readFileSync(await download.path());
    }

    // "Same click, same file, same hash; only the badge differs."
    for (const name of ["direct", "blocked"]) {
      expect(saved[name].length).toBe(bytes.length);
      expect(createHash("sha256").update(saved[name]).digest("hex")).toBe(hashHex);
    }

    // The badge names the path that CARRIED the bytes, and the two readers
    // took different ones from the same gesture.
    await expect(direct.page.locator(".transfer-path")).toHaveAttribute("data-path", "direct");
    await expect(blocked.page.locator(".transfer-path")).toHaveAttribute("data-path", "relay");

    // "The fallback never costs a second click": one `transfer.request` each,
    // and the direct reader never opened a relay socket at all.
    for (const peer of [direct, blocked]) {
      const counters = await hookCounters(peer.page);
      expect(counters.outboundTypes.filter((t) => t === "transfer.request").length).toBeLessThan(2);
    }
    const directCounters = await hookCounters(direct.page);
    expect(directCounters.wsUrls.filter((url) => url.includes("/transfer/ws/relay/")).length).toBe(0);
    expect(directCounters.rtc).toBe(1);
    const blockedCounters = await hookCounters(blocked.page);
    expect(blockedCounters.wsUrls.filter((url) => url.includes("/transfer/ws/relay/")).length).toBe(1);

    // The publisher did nothing different for the two readers: one file, one
    // offer, and never a `transfer.request` of its own.
    const sourceCounters = await hookCounters(a.page);
    expect(sourceCounters.outboundTypes.filter((t) => t === "transfer.request").length).toBe(0);
    expect(sourceCounters.outboundTypes.filter((t) => t === "offer.publish").length).toBe(1);

    for (const peer of [a, direct, blocked]) {
      expect(appFailures(peer.failures)).toEqual([]);
      await peer.cleanup();
    }
  });
});
