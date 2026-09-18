// T-WEB-NOAUTO, T-WEB-EQUAL-PEERS, T-WEB-REPUBLISH: the adversarial
// multipeer suite. A/B/C share one room link with equal rights; nothing
// moves without an explicit click — no transfer request, RTC object, relay
// connection, file read or transfer row appears before one.
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, expect } from "@playwright/test";
import { openPersistentPeer, opfsWorks, spawnRoomEnv } from "./helpers.mjs";
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

// --- 4.4: the real three-peer scenario -------------------------------------
//
// One room, three browser contexts, one server. A publishes; B has working
// WebRTC and must ride a DataChannel; C has WebRTC but every candidate is
// forced through a TURN server that does not exist, so ICE fails on its own
// and the SAME click must land on the encrypted relay. Nothing is set by
// hand: no state is poked, `direct_failed` is never called from the test,
// and the server's UDP is not blocked — signalling and the 10 s deadline are
// the real ones. Then B and C publish, the catalogue converges, two
// transfers run at once from different sources, and B cancels one of its own
// without touching the other.
//
// The relay is throttled to 1 MiB/s so the relay legs are paced by the
// SERVER and not by this machine: that is what makes "cancel mid-transfer"
// and "the badge is still connecting" observable rather than raced.
let scenario = null;
let scenarioDir = null;
const files = {};

test.describe.serial("multipeer scenario", () => {
  test.beforeAll(async () => {
    scenario = await spawnRoomEnv({ relayRate: 1024 * 1024 });
    scenarioDir = mkdtempSync(join(tmpdir(), "bore-scenario-"));
    // Two whole chunks and a tail; big enough that a relay leg takes
    // seconds at the throttled rate, small enough to keep the suite quick.
    for (const [name, size] of [
      ["canary.bin", 2 * 1024 * 1024 + 7],
      ["beta.bin", 8 * 1024 * 1024 + 5],
      ["gamma.bin", 4 * 1024 * 1024 + 3],
      // One whole chunk and a tail, for the paced room below.
      ["slow.bin", 1024 * 1024 + 7],
    ]) {
      const bytes = Buffer.alloc(size);
      for (let i = 0; i < size; i += 1) {
        bytes[i] = (i * 13 + name.length) % 251;
      }
      writeFileSync(join(scenarioDir, name), bytes);
      files[name] = { bytes, hash: createHash("sha256").update(bytes).digest("hex") };
    }
  }, 120_000);

  test.afterAll(async () => {
    scenario?.cleanup();
    if (scenarioDir) {
      rmSync(scenarioDir, { recursive: true, force: true });
    }
  });

  // Persistent profiles for the same reason every download suite uses them:
  // OPFS is unusable in an ephemeral WebKit context (V002-F02).
  async function openScenarioPeer(options = {}) {
    const { browserName, defaultBrowserType, channel } = test.info().project.use;
    return openPersistentPeer(scenario.roomUrl, {
      browserName: browserName ?? defaultBrowserType,
      channel,
      ...options,
    });
  }

  async function pollPage(page, fn, timeoutMs = 60_000) {
    const start = Date.now();
    for (;;) {
      const value = await page.evaluate(fn);
      if (value) {
        return value;
      }
      if (Date.now() - start > timeoutMs) {
        throw new Error("scenario poll timed out");
      }
      await new Promise((r) => setTimeout(r, 120));
    }
  }

  /**
   * Page errors that are not the app's. Forcing ICE through a TURN server
   * that does not exist is the fault this scenario injects, and Firefox
   * announces it on the console in its own words ("WebRTC: ICE failed, add
   * a TURN server..."). That line is the injection working, exactly as the
   * disconnect noise in `T-WEB-REPUBLISH` is; everything else still has to
   * be silent.
   */
  function appFailures(peer) {
    return peer.failures.filter((failure) => !/ICE failed|add a TURN server/i.test(failure));
  }

  function relaySockets(urls) {
    return urls.filter((url) => url.includes("/transfer/ws/relay/"));
  }

  /** Publishes one local file and waits for its own card to appear. */
  async function publish(page, name) {
    await page.locator("#file-input").setInputFiles([join(scenarioDir, name)]);
    await expect(page.locator(`.offer-card h4:has-text("${name}")`)).toBeVisible({
      timeout: 30_000,
    });
    return pollPage(page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      const self = window.__BORE_TEST__.selfPeerId;
      const mine = catalog.filter((offer) => offer.peerId === self);
      return mine.length > 0 ? mine[mine.length - 1].offerId : null;
    });
  }

  /** Saves the verified file and returns its sha256, then clears the panel. */
  async function saveAndHash(peer, name) {
    await expect(peer.page.locator("#save-name")).toHaveText(name);
    const download = await Promise.all([
      peer.page.waitForEvent("download", { timeout: 60_000 }),
      peer.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    await expect(peer.page.locator("#save-file")).toBeHidden({ timeout: 20_000 });
    return createHash("sha256").update(saved).digest("hex");
  }

  test("T-WEB-MULTIPEER A direct to B, relay to C, symmetry, concurrency, one cancel", async () => {
    test.setTimeout(420_000);
    const a = await openScenarioPeer();
    const b = await openScenarioPeer();
    // C is the only peer whose ICE is policy-restricted. A stays untouched,
    // so the SAME source serves one direct and one relay leg — the single
    // variable is the recipient's network.
    const c = await openScenarioPeer({ iceRelayOnly: true });
    for (const peer of [a, b, c]) {
      await expectConnected(peer.page);
    }
    expect(await opfsWorks(b.page)).toBe(true);
    expect(await opfsWorks(c.page)).toBe(true);

    // --- A publishes; both others see exactly one offer, owned by A.
    const canary = await publish(a.page, "canary.bin");
    for (const peer of [b, c]) {
      await expect(peer.page.locator(`button[data-download="${canary}"]`)).toBeVisible({
        timeout: 30_000,
      });
    }

    // --- B clicks once: a real DataChannel carries the bytes.
    await b.page.locator(`button[data-download="${canary}"]`).click();
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 120_000 });
    expect(await saveAndHash(b, "canary.bin")).toBe(files["canary.bin"].hash);
    {
      const commits = await b.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((entry) => entry.path)).toEqual(["direct"]);
      const events = await b.page.evaluate(() => [...window.__BORE_TEST__.directEvents]);
      expect(events.filter((entry) => entry.kind === "ready").length).toBe(1);
      expect(events.filter((entry) => entry.kind === "failed")).toEqual([]);
      const counters = await hookCounters(b.page);
      // Not one relay socket, and exactly the negotiated number of peer
      // connections: the server carried none of it. The count comes from the
      // page's own record of what the server asked for, never from a constant
      // here — pinning it to 1 broke the day the shipped default became 4.
      expect(relaySockets(counters.wsUrls)).toEqual([]);
      expect(counters.rtc).toBe(
        events.find((entry) => entry.kind === "ready")?.carriers ?? 1,
      );
      // The source's own view of this leg is the server's word.
      const notices = await a.page.evaluate(() => [...window.__BORE_TEST__.progressNotices]);
      expect(notices.length).toBeGreaterThan(0);
      expect(new Set(notices.map((entry) => entry.path))).toEqual(new Set(["direct"]));
    }

    // --- C clicks once: ICE cannot pair, the deadline expires, the SAME
    // click finishes on the relay with a DIFFERENT attempt.
    await c.page.locator(`button[data-download="${canary}"]`).click();
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
    expect(await saveAndHash(c, "canary.bin")).toBe(files["canary.bin"].hash);
    {
      const commits = await c.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((entry) => entry.path)).toEqual(["relay"]);
      const starts = await c.page.evaluate(() =>
        [...window.__BORE_TEST__.directEvents].map((entry) => entry.attemptId),
      );
      if (starts.length > 0) {
        // The relay runs on a FRESH attempt: the direct one is over, and a
        // commit that reused its id would mean the deadline proved nothing.
        expect(commits[0].attemptId).not.toBe(starts[0]);
      }
      const counters = await hookCounters(c.page);
      expect(relaySockets(counters.wsUrls).length).toBe(1);
      // One click means one request: the fallback asked for nothing more.
      expect(counters.outboundTypes.filter((t) => t === "transfer.request").length).toBe(1);
    }
    // A served both legs from one offer and both arms agree on the bytes.
    {
      const notices = await a.page.evaluate(() => [...window.__BORE_TEST__.progressNotices]);
      expect(new Set(notices.map((entry) => entry.path))).toEqual(new Set(["direct", "relay"]));
    }

    // --- Symmetry: B and C publish their own files and every peer converges
    // on three offers, one per owner. A receiving peer never becomes a
    // source of what it received: B holds exactly ONE offer, its own.
    const beta = await publish(b.page, "beta.bin");
    const gamma = await publish(c.page, "gamma.bin");
    for (const peer of [a, b, c]) {
      await expect(async () => {
        const catalog = await hookCatalog(peer.page);
        expect(catalog.map((offer) => offer.offerId).sort()).toEqual(
          [canary, beta, gamma].sort(),
        );
        expect(new Set(catalog.map((offer) => offer.peerId)).size).toBe(3);
      }).toPass({ timeout: 45_000 });
    }
    for (const [peer, own] of [
      [a, canary],
      [b, beta],
      [c, gamma],
    ]) {
      const catalog = await hookCatalog(peer.page);
      const self = await peer.page.evaluate(() => window.__BORE_TEST__.selfPeerId);
      expect(catalog.filter((offer) => offer.peerId === self).map((offer) => offer.offerId)).toEqual(
        [own],
      );
    }

    // --- Two transfers at once, from two different sources, and one cancel.
    // B pulls C's file and C pulls B's: C is relay-only, so BOTH legs fall
    // back and both are paced by the server's 1 MiB/s.
    await b.page.locator(`button[data-download="${gamma}"]`).click();
    await c.page.locator(`button[data-download="${beta}"]`).click();
    // B's own leg, by id: every row here is addressed explicitly so the
    // cancel cannot land on the wrong transfer.
    const mine = await pollPage(b.page, () => {
      const rows = window.__BORE_TEST__.receiverState();
      return rows.length === 1 ? rows[0].transferId : null;
    });
    const row = `.transfer-row[data-transfer="${mine}"]`;
    await expect(async () => {
      const value = await b.page.locator(`${row} progress`).getAttribute("value");
      expect(Number(value)).toBeGreaterThanOrEqual(20);
    }).toPass({ timeout: 180_000 });
    await b.page.locator(`button[data-cancel="${mine}"]`).click();
    await expect(b.page.locator(`${row} .transfer-state`)).toContainText("Annullato");
    expect(await b.page.evaluate(() => window.__BORE_TEST__.receiverState().length)).toBe(0);

    // The OTHER transfer is untouched and finishes with the exact bytes.
    await expect(c.page.locator("#save-file")).toBeVisible({ timeout: 180_000 });
    expect(await saveAndHash(c, "beta.bin")).toBe(files["beta.bin"].hash);

    // Nothing auto-seeded and nothing auto-started anywhere: three offers,
    // one per peer, and no peer ever published what it downloaded.
    for (const peer of [a, b, c]) {
      const catalog = await hookCatalog(peer.page);
      expect(catalog).toHaveLength(3);
    }
    {
      const counters = await hookCounters(a.page);
      // A never requested anything: it published and served, nothing else.
      expect(counters.outboundTypes).not.toContain("transfer.request");
      expect(counters.outboundTypes.filter((t) => t === "offer.publish").length).toBe(1);
      expect((await hookCounters(b.page)).outboundTypes.filter((t) => t === "offer.publish").length).toBe(1);
      expect((await hookCounters(c.page)).outboundTypes.filter((t) => t === "offer.publish").length).toBe(1);
    }
    for (const peer of [a, b, c]) {
      expect(appFailures(peer)).toEqual([]);
      await peer.cleanup();
    }
  });

  test("T-WEB-PATH-AUTH the path stays connecting until a verified byte, then follows the recipient", async () => {
    test.setTimeout(300_000);
    // Its OWN room, paced at 256 KiB/s. The bucket's burst is twice the
    // rate, so a room fast enough for the scenario above delivers a 2 MiB
    // file entirely out of the burst and the commit and the first verified
    // chunk land in the same instant — the window this test reads would not
    // exist. 256 KiB/s puts ~2 s between them for a 1 MiB chunk.
    const paced = await spawnRoomEnv({ relayRate: 256 * 1024 });
    const openPaced = async (options = {}) => {
      const { browserName, defaultBrowserType, channel } = test.info().project.use;
      return openPersistentPeer(paced.roomUrl, {
        browserName: browserName ?? defaultBrowserType,
        channel,
        ...options,
      });
    };
    const a = await openPaced();
    // Relay-only ICE buys a guaranteed ~10 s negotiating window on the
    // recipient and, after the fallback, a commit that is seconds ahead of
    // the first verified chunk at 1 MiB/s. Both windows are what this test
    // reads; neither is raced.
    const c = await openPaced({ iceRelayOnly: true });
    await expectConnected(a.page);
    await expectConnected(c.page);
    expect(await opfsWorks(c.page)).toBe(true);

    const offerId = await publish(a.page, "slow.bin");
    await expect(c.page.locator(`button[data-download="${offerId}"]`)).toBeVisible({
      timeout: 30_000,
    });

    // The badge log is written BY THE DOM, not by a poller: a 100 ms grid
    // installed after the click cannot promise it saw the window before the
    // first byte, and on Firefox it MEASURABLY did not — relay-only ICE
    // fails in well under a second, so bytes were already flowing when the
    // first sample was taken and the test read `firstByte === 0`. A
    // MutationObserver samples when the row appears and at every change
    // after it, so the window is observed by construction.
    const installPathLog = (page) =>
      page.evaluate(() => {
        const hook = window.__BORE_TEST__;
        window.__pathLog = [];
        const snap = () => {
          const badge = document.querySelector(".transfer-row .transfer-path");
          const rows = typeof hook.receiverState === "function" ? hook.receiverState() : [];
          window.__pathLog.push({
            path: badge === null ? null : badge.getAttribute("data-path"),
            recv: rows.length > 0 ? Number(rows[0].receivedBytes ?? 0) : 0,
            commits: hook.pathCommits.length,
            progress: hook.progressNotices.length,
          });
        };
        const root = document.getElementById("transfers") ?? document.body;
        new MutationObserver(snap).observe(root, {
          subtree: true,
          childList: true,
          characterData: true,
          attributes: true,
          attributeFilter: ["data-path"],
        });
        snap();
      });
    await installPathLog(a.page);
    await installPathLog(c.page);

    await c.page.locator(`button[data-download="${offerId}"]`).click();

    // Sample BOTH pages until the recipient has saved. A sample is
    // `(pathCommits, progressNotices, badge)`; the rule under test is that
    // the badge is `connecting` in every sample taken before the first
    // verified byte reached that page — a committed path that has carried
    // nothing is not a fact yet.
    const read = (page) =>
      page.evaluate(() => {
        const hook = window.__BORE_TEST__;
        const badge = document.querySelector(".transfer-row .transfer-path");
        const rows = typeof hook.receiverState === "function" ? hook.receiverState() : [];
        return {
          commits: hook.pathCommits.length,
          progress: hook.progressNotices.length,
          recv: rows.length > 0 ? Number(rows[0].receivedBytes ?? 0) : 0,
          path: badge === null ? null : badge.getAttribute("data-path"),
        };
      });
    const samples = { a: [], c: [] };
    const deadline = Date.now() + 240_000;
    for (;;) {
      samples.a.push(await read(a.page));
      samples.c.push(await read(c.page));
      if (await c.page.locator("#save-file").isVisible()) {
        break;
      }
      if (Date.now() > deadline) {
        throw new Error("the recipient never verified the file");
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    samples.a.push(await read(a.page));
    samples.c.push(await read(c.page));

    // The recipient: no entry in the DOM's own log claims a path while
    // nothing has been received, the window really existed, the badge never
    // claims `direct` — the direct attempt never carried a byte — and the
    // transfer ends on `relay`. The cut is the first byte and NOT the
    // commit, because how long the direct attempt lives is the ENGINE's
    // business: Chromium runs out the server's 10 s deadline while Firefox
    // reports ICE failed in well under a second.
    const logC = await c.page.evaluate(() => window.__pathLog);
    // The PREFIX before the first received byte is what the rule is about.
    // Past completion the recipient's row leaves `receiverState` (the bytes
    // are staged and the panel asks to save them), so `recv` reads 0 again
    // there — a whole-log `every` would read that tail as a violation.
    const firstByte = logC.findIndex((entry) => entry.recv > 0);
    expect(firstByte).toBeGreaterThan(-1);
    const before = logC.slice(0, firstByte).filter((entry) => entry.path !== null);
    expect(before.length).toBeGreaterThan(0);
    expect(new Set(before.map((entry) => entry.path))).toEqual(new Set(["connecting"]));
    const shownC = logC.filter((entry) => entry.path !== null);
    expect(shownC.some((entry) => entry.path === "direct")).toBe(false);
    expect(shownC[shownC.length - 1].path).toBe("relay");
    expect(samples.c.some((sample) => sample.path === "direct")).toBe(false);
    expect(samples.c[samples.c.length - 1].path).toBe("relay");

    // The SOURCE: its badge follows the server's `transfer.progress`, which
    // carries the path the server committed and is sent only once the
    // recipient reported verified bytes. Before the first notice the badge
    // may not claim a path, even though the commit already arrived.
    const logA = (await a.page.evaluate(() => window.__pathLog)).filter(
      (entry) => entry.path !== null,
    );
    const sourceEarly = logA.filter((entry) => entry.progress === 0);
    expect(sourceEarly.length).toBeGreaterThan(0);
    expect(new Set(sourceEarly.map((entry) => entry.path))).toEqual(new Set(["connecting"]));
    // The window really was observed: at least one of those entries had the
    // commit already in hand, so `connecting` there is the rule and not
    // merely a message that had not arrived.
    expect(sourceEarly.some((entry) => entry.commits > 0)).toBe(true);
    const notices = await a.page.evaluate(() => [...window.__BORE_TEST__.progressNotices]);
    expect(notices.length).toBeGreaterThan(0);
    expect(new Set(notices.map((entry) => entry.path))).toEqual(new Set(["relay"]));
    expect(logA[logA.length - 1].path).toBe("relay");
    expect(samples.a[samples.a.length - 1].path).toBe("relay");

    expect(await saveAndHash(c, "slow.bin")).toBe(files["slow.bin"].hash);
    for (const peer of [a, c]) {
      expect(appFailures(peer)).toEqual([]);
      await peer.cleanup();
    }
    paced.cleanup();
  });
});
