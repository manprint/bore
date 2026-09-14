// T-WEB-NOAUTO, T-WEB-EQUAL-PEERS, T-WEB-REPUBLISH: the adversarial
// multipeer suite. A/B/C share one room link with equal rights; nothing
// moves without an explicit click — no transfer request, RTC object, relay
// connection, file read or transfer row appears before one.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { spawnRoomEnv } from "./helpers.mjs";
import { hookCatalog, hookCounters, installTestHooks } from "./fixtures.js";

let env;
let roomDir;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-multi-"));
  writeFileSync(join(roomDir, "alpha.txt"), "alpha bytes");
  writeFileSync(join(roomDir, "beta.txt"), "beta bytes");
  writeFileSync(join(roomDir, "gamma.txt"), "gamma bytes");
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openHookedPeer(browser, url) {
  const context = await browser.newContext();
  await installTestHooks(context);
  const page = await context.newPage();
  const failures = [];
  page.on("pageerror", (error) => failures.push(`pageerror: ${error.message}`));
  page.on("console", (message) => {
    if (message.type() === "error") {
      failures.push(`console: ${message.text()}`);
    }
  });
  await page.goto(url);
  return { context, page, failures };
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 10_000 });
}

async function publishFile(page, filename) {
  await page.locator("#file-input").setInputFiles([join(roomDir, filename)]);
  await expect(page.locator(`.offer-card h4:has-text("${filename}")`)).toBeVisible({ timeout: 15_000 });
}

test.describe.serial("multipeer", () => {
  test("T-WEB-EQUAL-PEERS symmetric publish and withdraw", async ({ browser }) => {
    const a = await openHookedPeer(browser, env.roomUrl);
    const b = await openHookedPeer(browser, env.roomUrl);
    const c = await openHookedPeer(browser, env.roomUrl);
    for (const peer of [a, b, c]) {
      await expectConnected(peer.page);
    }
    // Each publishes a distinct canary; all three catalogs converge.
    await publishFile(a.page, "alpha.txt");
    await publishFile(b.page, "beta.txt");
    await publishFile(c.page, "gamma.txt");
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(3, { timeout: 10_000 });
      const catalog = await hookCatalog(peer.page);
      expect(catalog.map((o) => o.manifest.label).sort()).toEqual(["alpha.txt", "beta.txt", "gamma.txt"]);
    }
    // Withdraw buttons exist only on each peer's own cards.
    for (const [peer, own] of [
      [a, "alpha.txt"],
      [b, "beta.txt"],
      [c, "gamma.txt"],
    ]) {
      expect(await peer.page.locator(".offer-card [data-withdraw]").count()).toBe(1);
      const mine = await peer.page.locator(".offer-card:has([data-withdraw]) h4").allTextContents();
      expect(mine).toEqual([own]);
    }
    // Everyone withdraws only their own; the catalog empties everywhere.
    for (const peer of [a, b, c]) {
      await peer.page.locator(".offer-card [data-withdraw]").click();
    }
    for (const peer of [a, b, c]) {
      await expect(peer.page.locator(".offer-card")).toHaveCount(0, { timeout: 10_000 });
      expect(peer.failures).toEqual([]);
      await peer.context.close();
    }
  });

  test("T-WEB-NOAUTO nothing moves without a click", async ({ browser }) => {
    test.setTimeout(120_000);
    const a = await openHookedPeer(browser, env.roomUrl);
    const b = await openHookedPeer(browser, env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    await publishFile(a.page, "alpha.txt");
    await expect(b.page.locator('.offer-card h4:has-text("alpha.txt")')).toBeVisible({ timeout: 10_000 });
    // Baseline after offer hashing: two full heartbeat periods pass with
    // pings flowing (the session is provably alive) and nothing else.
    const pingBaseline = (await hookCounters(b.page)).outboundTypes.filter((t) => t === "ping").length;
    const base = await hookCounters(b.page);
    await new Promise((resolve) => setTimeout(resolve, 42_000));
    await expectConnected(b.page);
    const after = await hookCounters(b.page);
    expect(after.outboundTypes.filter((t) => t === "ping").length).toBeGreaterThan(pingBaseline);
    expect(after.fileReads).toBe(base.fileReads);
    expect(after.rtc).toBe(0);
    expect(after.transferRows).toBe(0);
    for (const url of after.wsUrls) {
      expect(url).toContain("/transfer/ws/control/");
    }
    for (const type of new Set(after.outboundTypes)) {
      expect(["hello", "ping", "peer.rename", "offer.publish", "offer.withdraw"]).toContain(type);
    }
    expect(after.outboundTypes).not.toContain("transfer.request");
    expect(b.failures).toEqual([]);
    expect(a.failures).toEqual([]);
    await a.context.close();
    await b.context.close();
  });

  test("T-WEB-REPUBLISH reconnect republishes, fresh tab stays passive", async ({ browser }) => {
    test.setTimeout(180_000);
    const a = await openHookedPeer(browser, env.roomUrl);
    await expectConnected(a.page);
    await publishFile(a.page, "alpha.txt");
    const before = await hookCatalog(a.page);
    expect(before).toHaveLength(1);
    const offerId = before[0].offerId;
    const ghostPeer = before[0].peerId;
    // Real network fault (no reload, files survive): the socket dies, the
    // client backs off and re-hellos as a new peer on return. Detection
    // rides the 20 s heartbeat, so the offline state surfaces a cycle late.
    await a.context.setOffline(true);
    await expect(a.page.locator("#room-status")).toContainText("Riconnessione", { timeout: 30_000 });
    await new Promise((resolve) => setTimeout(resolve, 3_000));
    await a.context.setOffline(false);
    // The ghost session lingers server-side (no FIN on this transport), so
    // the candidate waits out the 60 s server reaper: convergence means the
    // SAME offer id under a DIFFERENT (new-session) peer — a ghost survival
    // keeps the old peer id and fails this poll.
    let converged = null;
    await expect(async () => {
      const catalog = await hookCatalog(a.page);
      const same = catalog.filter((o) => o.offerId === offerId);
      expect(same).toHaveLength(1);
      expect(same[0].peerId).not.toBe(ghostPeer);
      converged = same;
    }).toPass({ timeout: 75_000 });
    expect(converged).toHaveLength(1);
    await new Promise((resolve) => setTimeout(resolve, 3_000));
    expect((await hookCatalog(a.page)).filter((o) => o.offerId === offerId)).toHaveLength(1);
    await expect(a.page.locator("#room-status")).toContainText("Connesso", { timeout: 10_000 });
    // A fresh tab with the same link neither impersonates the old peer nor
    // recreates anything: hello and pings only.
    const fresh = await openHookedPeer(browser, env.roomUrl);
    await expectConnected(fresh.page);
    await expect(fresh.page.locator(".offer-card")).toHaveCount(1, { timeout: 10_000 });
    await new Promise((resolve) => setTimeout(resolve, 5_000));
    const audit = await hookCounters(fresh.page);
    expect(audit.outboundTypes).not.toContain("transfer.request");
    expect(audit.outboundTypes).not.toContain("offer.publish");
    expect(audit.fileReads).toBe(0);
    expect(audit.rtc).toBe(0);
    for (const peer of [a, fresh]) {
      // The offline window makes each engine log failed WebSocket
      // handshakes in its own words (Chromium ERR_INTERNET_DISCONNECTED,
      // Firefox "can't establish a connection", WebKit similar):
      // fault-injection noise, not app logging — everything else silent.
      const real = peer.failures.filter(
        (failure) =>
          !failure.includes("ERR_INTERNET_DISCONNECTED") &&
          !failure.includes("establish a connection to the server") &&
          !/websocket connection .* failed/i.test(failure),
      );
      expect(real).toEqual([]);
      await peer.context.close();
    }
  });
});
