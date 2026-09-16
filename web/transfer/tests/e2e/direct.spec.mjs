// T-WEB-DIRECT: one click moves the file over a REAL WebRTC DataChannel.
//
// Nothing here fakes the channel: both peers are the shipped page, the
// signalling goes through the shipped server, and the assertions are what an
// observer outside the page can see — the committed path, the absence of any
// relay socket, one peer connection per transfer, and the saved bytes.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "./helpers.mjs";
import { hookCounters } from "./fixtures.js";

let env = null;
let roomDir = null;
let fileBytes = null;
let fileHashHex = null;
let bigBytes = null;
let bigHashHex = null;

test.beforeAll(async () => {
  env = await spawnRoomEnv();
  roomDir = mkdtempSync(join(tmpdir(), "bore-direct-"));
  // Several logical chunks and a short tail: the fragment loop, the chunk
  // boundary and the FINAL total all get exercised.
  fileBytes = Buffer.alloc(2 * 1024 * 1024 + 7);
  for (let i = 0; i < fileBytes.length; i++) {
    fileBytes[i] = (i * 11 + 5) % 251;
  }
  fileHashHex = createHash("sha256").update(fileBytes).digest("hex");
  writeFileSync(join(roomDir, "direct.bin"), fileBytes);
  // Eight chunks and a tail: a quarter of it is two WHOLE chunks, so a
  // failure at 25% leaves something real on disk for the relay to skip.
  bigBytes = Buffer.alloc(8 * 1024 * 1024 + 7);
  for (let i = 0; i < bigBytes.length; i++) {
    bigBytes[i] = (i * 7 + 3) % 253;
  }
  bigHashHex = createHash("sha256").update(bigBytes).digest("hex");
  writeFileSync(join(roomDir, "fallback.bin"), bigBytes);
}, 60_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

// Persistent profiles for the same reason the relay download suite uses
// them: OPFS is unusable in an ephemeral WebKit context (V002-F02).
async function openPeer(url, options = {}) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    ...options,
  });
}

async function expectConnected(page) {
  await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 15_000 });
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

function relaySockets(urls) {
  return urls.filter((url) => url.includes("/transfer/ws/relay/"));
}

/**
 * Init script for the RECIPIENT: closes the one DataChannel once `limit`
 * payload bytes have arrived on it. The recipient is the side that creates
 * the channel, so wrapping `createDataChannel` reaches the real object the
 * app is using — nothing about the transfer is faked, only the moment the
 * transport dies is chosen.
 */
function killChannelAfter(limit) {
  return `(() => {
    const LIMIT = ${limit};
    const Real = window.RTCPeerConnection;
    if (typeof Real !== "function") { return; }
    const create = Real.prototype.createDataChannel;
    Real.prototype.createDataChannel = function (...args) {
      const channel = create.apply(this, args);
      let seen = 0;
      // The kill waits for BYTES **and** for the badge to have named this
      // transport. Bytes off the socket are not verified work: hashing runs
      // behind the wire, and under load this engine has been seen holding
      // two whole unverified chunks when the channel died. That is a real
      // and correct product case — nothing verified means nothing to skip,
      // and the relay attempt re-sends — but it is not the case this gate
      // exists to prove, and killing there made the gate measure the hash
      // pipeline's lag.
      //
      // The badge and not "a chunk verified", because a chunk is verified a
      // moment BEFORE the path is committed: killing inside that window
      // leaves the direct attempt with no commit at all, so the assertions
      // below read one commit instead of two. The badge is the same fact,
      // observed after it became one. No backtick in here: this function is
      // a template literal.
      const named = () => {
        try {
          return document.querySelector(".transfer-path")?.dataset.path === "direct";
        } catch {
          return false;
        }
      };
      channel.addEventListener("message", (event) => {
        const data = event.data;
        seen += data && data.byteLength !== undefined ? data.byteLength : (data && data.size) || 0;
        if (seen >= LIMIT && channel.readyState === "open" && named()) {
          try { window.__BORE_TEST__.killedAt = seen; } catch {}
          channel.close();
        }
      });
      return channel;
    };
  })();`;
}

test.describe.serial("direct", () => {
  test("T-WEB-DIRECT one click moves the file on a real DataChannel", async () => {
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl);
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "direct.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
    await expect(b.page.locator("#save-name")).toHaveText("direct.bin");

    // The committed path is `direct` on BOTH sides, and it is the only one.
    for (const peer of [a, b]) {
      const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.length).toBeGreaterThan(0);
      expect(commits.map((c) => c.path)).toEqual(commits.map(() => "direct"));
    }

    // Both actors reported a ready channel, neither reported a failure.
    for (const peer of [a, b]) {
      const events = await peer.page.evaluate(() => [...window.__BORE_TEST__.directEvents]);
      expect(events.filter((e) => e.kind === "ready").length).toBe(1);
      expect(events.filter((e) => e.kind === "failed")).toEqual([]);
      const ready = events.find((e) => e.kind === "ready");
      expect(ready.fragmentBytes).toBeGreaterThanOrEqual(1024);
      expect(ready.fragmentBytes).toBeLessThanOrEqual(24 * 1024);
    }
    // The recipient is the offerer, the source the answerer — the roles the
    // server fixed, read back from the pages that played them.
    expect(
      (await b.page.evaluate(() => window.__BORE_TEST__.directEvents))[0].recipient,
    ).toBe(true);
    expect(
      (await a.page.evaluate(() => window.__BORE_TEST__.directEvents))[0].recipient,
    ).toBe(false);

    // ONE peer connection per transfer, and no relay socket anywhere: the
    // room's control channel is the only WebSocket either page opened.
    for (const peer of [a, b]) {
      const counters = await hookCounters(peer.page);
      expect(counters.rtc).toBe(1);
      expect(relaySockets(counters.wsUrls)).toEqual([]);
    }

    // Not one payload byte was read while the path was being negotiated:
    // the source's read counter is the SAME at `transfer.incoming` and at
    // `transfer.path_commit`, and larger only afterwards. Only the commit
    // authorises a read, and it cannot exist before both sides are ready.
    const marks = await a.page.evaluate(() => [...window.__BORE_TEST__.inboundMarks]);
    const incoming = marks.find((m) => m.type === "transfer.incoming");
    const commit = marks.find((m) => m.type === "transfer.path_commit");
    const directStart = marks.find((m) => m.type === "transfer.direct_start");
    expect(incoming).toBeDefined();
    expect(directStart).toBeDefined();
    expect(commit).toBeDefined();
    expect(directStart.reads).toBe(incoming.reads);
    expect(commit.reads).toBe(incoming.reads);
    expect((await hookCounters(a.page)).fileReads).toBeGreaterThan(commit.reads);

    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 30_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(download.suggestedFilename()).toBe("direct.bin");
    const saved = readFileSync(await download.path());
    expect(saved.length).toBe(fileBytes.length);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("a peer without WebRTC falls back to the relay on the same click", async () => {
    // The direct attempt is declined the moment it opens, so the SAME click
    // completes over the encrypted relay with a fresh attempt — no second
    // gesture, no waiting out the 10 s deadline.
    const a = await openPeer(env.roomUrl, { noWebRtc: true });
    const b = await openPeer(env.roomUrl, { noWebRtc: true });
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "direct.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const clickedAt = Date.now();
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
    // Well inside the server's 10 s direct deadline: the fallback is the
    // page saying `unsupported`, not the timer expiring.
    expect(Date.now() - clickedAt).toBeLessThan(10_000);

    for (const peer of [a, b]) {
      const events = await peer.page.evaluate(() => [...window.__BORE_TEST__.directEvents]);
      expect(events.map((e) => e.kind)).toEqual(["failed"]);
      expect(events[0].reason).toBe("unsupported");
      const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((c) => c.path)).toEqual(commits.map(() => "relay"));
      const counters = await hookCounters(peer.page);
      expect(counters.rtc).toBe(0);
      expect(relaySockets(counters.wsUrls).length).toBe(1);
    }

    // The relay attempt is a DIFFERENT attempt than the direct one it
    // replaced: a fresh key and a nonce sequence that restarts at zero.
    const startIds = await b.page.evaluate(() =>
      [...window.__BORE_TEST__.directEvents].map((e) => e.attemptId),
    );
    const commitIds = await b.page.evaluate(() =>
      [...window.__BORE_TEST__.pathCommits].map((c) => c.attemptId),
    );
    expect(commitIds[0]).not.toBe(startIds[0]);

    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 30_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);

    for (const peer of [a, b]) {
      expect(peer.failures).toEqual([]);
      await peer.cleanup();
    }
  });

  test("T-WEB-DIRECT-FALLBACK a channel that dies mid-transfer finishes on the relay", async () => {
    // Two transports, 8 MiB and a staging pass: well past the suite default.
    test.setTimeout(180_000);
    // The DataChannel is closed from the RECIPIENT after a quarter of the
    // file, which is the shape a real network failure takes: some chunks are
    // verified and on disk, one is half-written, and the source is still
    // sending. The same TransferId must finish over the relay, on a new
    // attempt, without re-sending what is already verified and without the
    // user touching anything.
    const a = await openPeer(env.roomUrl);
    const b = await openPeer(env.roomUrl, { init: killChannelAfter(2 * 1024 * 1024) });
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "fallback.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 120_000 });
    await expect(b.page.locator("#save-name")).toHaveText("fallback.bin");
    // The kill really fired on the wire, not on a timer.
    expect(await b.page.evaluate(() => window.__BORE_TEST__.killedAt ?? 0)).toBeGreaterThanOrEqual(
      2 * 1024 * 1024,
    );

    // One transfer, two attempts: direct first, then relay, same TransferId.
    for (const peer of [a, b]) {
      const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((c) => c.path)).toEqual(["direct", "relay"]);
      expect(commits[0].transferId).toBe(commits[1].transferId);
      expect(commits[0].attemptId).not.toBe(commits[1].attemptId);
      expect(await peer.page.evaluate(() => window.__BORE_TEST__.transferRows())).toBe(1);
    }

    // The relay attempt skips what the direct one verified: the ranges the
    // recipient reported ride the source's own `path_commit`.
    const relayCommit = (
      await a.page.evaluate(() => [...window.__BORE_TEST__.pathCommits])
    )[1];
    expect(Array.isArray(relayCommit.resumeRanges)).toBe(true);
    expect(relayCommit.resumeRanges.length).toBeGreaterThan(0);
    expect(relayCommit.resumeRanges[0][0]).toBe(0);
    expect(relayCommit.resumeRanges[0][1]).toBeGreaterThanOrEqual(1);

    // The recipient reported the dead attempt EXACTLY ONCE, naming the
    // attempt that died and carrying what it had verified.
    //
    // The REASON is whichever end noticed first, and both are true
    // statements about the same dead channel: the recipient's own `close`
    // event says `channel-closed`, while a source whose next write failed
    // synchronously says `send-error` and the recipient — told before its
    // own event fires — echoes what it was told. Which one wins is decided
    // by two engines' timers and changes under load, so pinning one of them
    // would gate the scheduler and not the product. What may never vary is
    // that there is exactly ONE report, that it names the failed attempt,
    // and that it carries the ranges: that report is the only place the
    // verified ranges exist, and losing it is what makes the replacement
    // attempt re-send the whole file.
    const events = await b.page.evaluate(() => [...window.__BORE_TEST__.directEvents]);
    const failed = events.filter((e) => e.kind === "failed");
    expect(failed.length).toBe(1);
    expect(["channel-closed", "send-error"]).toContain(failed[0].reason);
    expect(failed[0].ranges.length).toBeGreaterThan(0);
    expect(failed[0].attemptId).toBe((await a.page.evaluate(
      () => [...window.__BORE_TEST__.pathCommits],
    ))[0].attemptId);

    // One peer connection and one relay leg per page: the fallback opened
    // exactly one, and nothing retried itself into a second.
    for (const peer of [a, b]) {
      const counters = await hookCounters(peer.page);
      expect(counters.rtc).toBe(1);
      expect(relaySockets(counters.wsUrls).length).toBe(1);
      expect(counters.outboundTypes.filter((t) => t === "transfer.request").length).toBeLessThan(2);
    }
    // The badge followed the bytes: it reads `relay` now. It DID pass back
    // through `in connessione` on the way — 5.6 made that walk-back the rule,
    // because a badge that keeps naming a dead transport is a claim the page
    // cannot support — and `T-WEB-PATH-UI` is the gate that owns the order.
    await expect(b.page.locator(".transfer-path")).toHaveAttribute("data-path", "relay");

    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    expect(download.suggestedFilename()).toBe("fallback.bin");
    const saved = readFileSync(await download.path());
    expect(saved.length).toBe(bigBytes.length);
    expect(createHash("sha256").update(saved).digest("hex")).toBe(bigHashHex);

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });

  test("T-WEB-DIRECT-TIMEOUT ICE that never pairs falls back without a second click", async () => {
    // The server's 10 s direct deadline is inside this budget by design.
    test.setTimeout(180_000);
    // WebRTC is present and a peer connection is really built, but every
    // candidate is forced through a TURN server that does not exist, so ICE
    // cannot pair. No payload may ride the direct attempt, and the relay
    // must start on its own — the server's 10 s deadline is the backstop.
    const a = await openPeer(env.roomUrl, { iceRelayOnly: true });
    const b = await openPeer(env.roomUrl, { iceRelayOnly: true });
    await expectConnected(a.page);
    await expectConnected(b.page);
    expect(await opfsWorks(b.page)).toBe(true);

    await a.page.locator("#file-input").setInputFiles([join(roomDir, "direct.bin")]);
    const offer = await poll(a.page, () => {
      const catalog = window.__BORE_TEST__.getCatalogSnapshot();
      return catalog.length === 1 ? catalog[0] : null;
    });
    const started = await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    expect(started).toEqual({ pending: true });

    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 120_000 });

    for (const peer of [a, b]) {
      // Nothing was ever committed to the direct path, so not one payload
      // byte could ride it.
      const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.map((c) => c.path)).toEqual(["relay"]);
      const starts = await peer.page.evaluate(() =>
        [...window.__BORE_TEST__.directEvents].map((e) => e.attemptId),
      );
      if (starts.length > 0) {
        expect(commits[0].attemptId).not.toBe(starts[0]);
      }
      const counters = await hookCounters(peer.page);
      expect(relaySockets(counters.wsUrls).length).toBe(1);
      // One click, one request: the fallback asked the server for nothing.
      expect(counters.outboundTypes.filter((t) => t === "transfer.request").length).toBeLessThan(2);
    }
    // The source read the file only after the RELAY commit.
    const marks = await a.page.evaluate(() => [...window.__BORE_TEST__.inboundMarks]);
    const incoming = marks.find((m) => m.type === "transfer.incoming");
    const commit = marks.find((m) => m.type === "transfer.path_commit");
    expect(incoming).toBeDefined();
    expect(commit).toBeDefined();
    expect(commit.reads).toBe(incoming.reads);

    const download = await Promise.all([
      b.page.waitForEvent("download", { timeout: 60_000 }),
      b.page.locator("#save-file").click(),
    ]).then(([event]) => event);
    const saved = readFileSync(await download.path());
    expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });
});
