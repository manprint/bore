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
      // ACROSS CARRIERS, both the count and the kill. A direct attempt is
      // several peer connections now (one per carrier, see
      // --web-transfer-direct-carriers), each with its own channel, and the
      // attempt dies only when the LAST one is gone: a per-channel counter
      // both fired late (each carrier sees its own share of the bytes) and
      // killed one carrier of N, which the product correctly survives -- so
      // the transition this gate exists for never happened and the gate timed
      // out instead of failing. NO BACKTICK in here: template literal.
      const shared = (window.__BORE_KILL__ = window.__BORE_KILL__ || {
        seen: 0,
        channels: [],
      });
      shared.channels.push(channel);
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
        shared.seen += data && data.byteLength !== undefined ? data.byteLength : (data && data.size) || 0;
        if (shared.seen >= LIMIT && named()) {
          try { window.__BORE_TEST__.killedAt = shared.seen; } catch {}
          for (const open of shared.channels) {
            try { open.close(); } catch {}
          }
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

    // EXACTLY the negotiated number of peer connections per transfer, and no
    // relay socket anywhere: the room's control channel is the only WebSocket
    // either page opened. The number comes from the page's own record of what
    // the SERVER asked for (`transfer.direct_start` carries `carriers`), not
    // from a constant here — a gate pinned to 1 breaks the day the shipped
    // default moves, which is exactly what happened when it became 4.
    for (const peer of [a, b]) {
      const counters = await hookCounters(peer.page);
      const ready = (
        await peer.page.evaluate(() => [...window.__BORE_TEST__.directEvents])
      ).find((event) => event.kind === "ready");
      expect(ready, "the attempt came up").toBeDefined();
      expect(counters.rtc).toBe(ready.carriers);
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

  test("T-WEB-RELAY-ONLY a room opened with --relay-only never builds a peer connection", async () => {
    // The OPERATOR's policy, and the server is what enforces it: a room
    // opened with `--relay-only` must never send a peer the message that
    // starts a direct attempt, so a full-capability browser pair — WebRTC
    // available, nothing disabled on the page side — must still construct
    // ZERO `RTCPeerConnection`s. That is the difference between a policy and
    // a preference, and it is the half a user can see; the registry's half
    // is pinned by `a_relay_only_room_admits_the_relay_without_ever_opening_a_direct_attempt`.
    const room = await spawnRoomEnv({ relayOnly: true });
    try {
      const a = await openPeer(room.roomUrl);
      const b = await openPeer(room.roomUrl);
      await expectConnected(a.page);
      await expectConnected(b.page);
      expect(await opfsWorks(b.page)).toBe(true);

      await a.page.locator("#file-input").setInputFiles([join(roomDir, "direct.bin")]);
      const offer = await poll(a.page, () => {
        const catalog = window.__BORE_TEST__.getCatalogSnapshot();
        return catalog.length === 1 ? catalog[0] : null;
      });
      expect(
        await b.page.evaluate(
          (offerId) => window.__BORE_TEST__.requestDownload(offerId),
          offer.offerId,
        ),
      ).toEqual({ pending: true });

      await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
      const download = await Promise.all([
        b.page.waitForEvent("download", { timeout: 30_000 }),
        b.page.locator("#save-file").click(),
      ]).then(([event]) => event);
      expect(createHash("sha256").update(readFileSync(await download.path())).digest("hex")).toBe(
        fileHashHex,
      );

      for (const peer of [a, b]) {
        const counters = await hookCounters(peer.page);
        // No peer connection AT ALL, so no ICE candidate — no local and no
        // reflexive address — ever left either machine.
        expect(counters.rtc).toBe(0);
        expect(relaySockets(counters.wsUrls).length).toBe(1);
        const events = await peer.page.evaluate(() => [...window.__BORE_TEST__.directEvents]);
        expect(events).toEqual([]);
        const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
        expect(commits.map((c) => c.path)).toEqual(["relay"]);
        expect(peer.failures).toEqual([]);
        await peer.cleanup();
      }
    } finally {
      room.cleanup();
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
    // The kill really fired on the wire, not on a timer. When it did NOT,
    // the interesting fact is never the zero — it is what the direct leg did
    // instead, so the trace goes in the message: a leg that closed on its own
    // before the threshold (seen on a starved runner) reads as a
    // `channel-error` with the engine's own `detail`/`sctp` cause, which is a
    // different finding from a leg that never carried at all.
    const killedAt = await b.page.evaluate(() => window.__BORE_TEST__.killedAt ?? 0);
    if (killedAt < 2 * 1024 * 1024) {
      const why = await b.page.evaluate(() => window.__BORE_TEST__.readDirectDiagnostics());
      expect(
        killedAt,
        `the kill never fired on the wire: ${JSON.stringify([...why.finished, ...why.live])}`,
      ).toBeGreaterThanOrEqual(2 * 1024 * 1024);
    }

    // The SOURCE must not call a completed transfer failed. The relay leg is
    // torn down as soon as the recipient has verified, which lands while the
    // source is still draining its own write queue: before B-A028 that close
    // was reported as `FAILED` and the source's row read
    // `relay 100% · Non riuscito` for the transfer the recipient had just
    // saved. This is the red-check for it.
    expect(await a.page.evaluate(() => [...window.__BORE_TEST__.senderErrors])).toEqual([]);
    expect(
      await a.page.evaluate(() =>
        [...document.querySelectorAll(".transfer-row")]
          .map((row) => row.textContent ?? "")
          .join(" ~ "),
      ),
    ).not.toMatch(/Non riuscito/);

    // No control message either peer sent may be MALFORMED. `STALE_ATTEMPT`
    // is excluded and nothing else is: a candidate for the attempt that just
    // died is the ordinary trickle-ICE race (a Firefox source produced 62 of
    // them in this very test), while an `INVALID_MESSAGE` here would mean the
    // client is building a body the server cannot parse — which is exactly
    // what this assertion existed to catch, and what the shared code hid.
    for (const [who, peer] of [["source", a], ["recipient", b]]) {
      const errs = await peer.page.evaluate(() =>
        [...window.__BORE_TEST__.controlErrors].map((e) => `${e.code}:${e.sent ?? "?"}`),
      );
      expect({ who, errs: errs.filter((e) => !e.startsWith("STALE_ATTEMPT:")) }).toEqual({
        who,
        errs: [],
      });
    }

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

    // The negotiated peer connections and ONE relay leg per page: the
    // fallback opened exactly one, and nothing retried itself into a second.
    for (const peer of [a, b]) {
      const counters = await hookCounters(peer.page);
      const ready = (
        await peer.page.evaluate(() => [...window.__BORE_TEST__.directEvents])
      ).find((event) => event.kind === "ready");
      expect(counters.rtc).toBe(ready?.carriers ?? 1);
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

    // V003-C3: the dead attempt left EVIDENCE. Before this, a fallback in
    // the field produced a relay and nothing else — no selected pair type,
    // no timeline, nothing to tell a broken path from a stalled queue.
    const summarize = (diag) =>
      [...diag.finished, ...diag.live].map((trace) => ({
        reason: trace.reason,
        ready: trace.events.some((event) => event.ev === "ready"),
        timeouts: trace.drain.timeouts,
      }));
    const recipient = summarize(
      await b.page.evaluate(() => window.__BORE_TEST__.readDirectDiagnostics()),
    );
    const source = summarize(
      await a.page.evaluate(() => window.__BORE_TEST__.readDirectDiagnostics()),
    );
    console.log(`[fallback traces] ${JSON.stringify({ source, recipient })}`);
    expect(recipient.length).toBeGreaterThan(0);
    // The death was RECORDED, by whichever side recorded it (B-A036). This
    // test kills the channel FROM THE RECIPIENT, and whether an engine
    // delivers a `close` event to the side that called `close()` is that
    // engine's business: firefox does not, so the recipient's own traces can
    // legitimately carry no reason while its `failed` event — asserted above,
    // and the report the product actually depends on — carries one. The
    // source is the side the failure happened TO. Asserting the reason on the
    // recipient alone gated an engine, not the product.
    const dead = [...source, ...recipient].filter((trace) => trace.reason !== null);
    expect(dead.length, "no side recorded a reason for the dead attempt").toBeGreaterThan(0);
    for (const trace of dead) {
      expect(["channel-closed", "send-error", "timeout", "protocol"]).toContain(trace.reason);
    }
    // ONE TRACE PER CARRIER, so "the trace that has a reason" is not
    // necessarily the carrier that was carrying: with four carriers one can
    // die before it ever reached `ready`, and asserting `ready` on whichever
    // trace came first made this gate fail on webkit about half the time
    // while the product was doing exactly the right thing. What must hold is
    // that a carrier really was carrying.
    const carried = recipient.filter((trace) => trace.ready);
    expect(carried.length, "no carrier had ever been ready").toBeGreaterThan(0);
    // It is the QUEUE that is exonerated here, and the trace says so: this
    // attempt died on its channel, not on a drain that never came.
    for (const trace of carried) {
      expect(trace.timeouts).toBe(0);
    }

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });

  // T-WEB-DIRECT-DIAG (V003-C3). The page can now say WHICH kind of path an
  // attempt used and what ended it, and the same evidence must be safe to
  // hand to a stranger: the report is produced on a real engine and searched
  // for everything it must never contain.
  test("T-WEB-DIRECT-DIAG the direct path keeps a redacted, copyable trace", async () => {
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
    await b.page.evaluate(
      (offerId) => window.__BORE_TEST__.requestDownload(offerId),
      offer.offerId,
    );
    await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });
    const commits = await b.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
    expect(commits.map((c) => c.path)).toEqual(commits.map(() => "direct"));

    // The stats sample lands asynchronously, on a timer and again at close.
    const diag = await poll(
      b.page,
      () => {
        const read = window.__BORE_TEST__.readDirectDiagnostics();
        const traces = [...read.finished, ...read.live];
        return traces.some((trace) => trace.stats.length > 0) ? { traces } : null;
      },
      30_000,
    );
    const withStats = diag.traces.find((trace) => trace.stats.length > 0);
    const sample = withStats.stats.at(-1);
    // THE question the field case could not answer: which kind of candidate
    // pair carried it. On loopback it is `host`; what is gated is that the
    // page knows and can say it.
    expect(sample.pair, "the selected pair is described").toBeDefined();
    expect(["host", "srflx", "prflx", "relay"]).toContain(sample.pair.localType);
    expect(withStats.events.some((event) => event.ev === "ready")).toBe(true);
    expect(withStats.transferId).toBe(commits[0].transferId);

    // The copy gesture — the only way any of this leaves the page — and what
    // it puts on the clipboard.
    await b.page.evaluate(() => {
      window.__clipboard = null;
      const stub = { writeText: async (text) => { window.__clipboard = text; } };
      try {
        Object.defineProperty(navigator, "clipboard", { value: stub, configurable: true });
      } catch {
        navigator.clipboard.writeText = stub.writeText;
      }
    });
    await b.page.locator("#copy-diagnostics").click();
    const copied = await poll(b.page, () => window.__clipboard ?? null, 15_000);
    expect(copied).toContain("bore-web-transfer-direct-diagnostics");
    expect(copied).toContain("\"localType\"");

    // The redaction, on the real engine's own strings: no address, no
    // candidate line, no SDP, no file name, no room secret.
    expect(copied).not.toMatch(/\b\d{1,3}(\.\d{1,3}){3}\b/);
    expect(copied).not.toContain("candidate:");
    expect(copied).not.toContain("v=0");
    expect(copied).not.toContain("direct.bin");
    const secret = new URL(env.roomUrl).hash;
    for (const value of secret.replace("#", "").split("&")) {
      const hex = value.split("=")[1];
      if (hex) {
        expect(copied).not.toContain(hex);
      }
    }
    // And the general form of the same claim, which is what makes this gate
    // hold for a field nobody has added yet: EVERY string in the report is
    // either one of the server's own opaque ids or a short enumeration.
    // An address, a URL, a name or a candidate line is neither.
    const strings = [];
    const walk = (value) => {
      if (typeof value === "string") {
        strings.push(value);
        return;
      }
      if (Array.isArray(value)) {
        value.forEach(walk);
        return;
      }
      if (value !== null && typeof value === "object") {
        Object.values(value).forEach(walk);
      }
    };
    walk(JSON.parse(copied));
    for (const value of strings) {
      expect(
        /^[0-9a-f]{32}$/.test(value) ||
          /^[a-z][a-z-]{0,23}$/.test(value) ||
          value === "bore-web-transfer-direct-diagnostics",
        `"${value}" is neither an opaque id nor a short enumeration`,
      ).toBe(true);
    }

    for (const peer of [a, b]) {
      await peer.cleanup();
    }
  });

  // V003-C5. The peer's end-of-candidates marker used to be dropped on the
  // receiving side; it now reaches the ICE agent, once, after every candidate
  // that preceded it. The room runs with NO STUN so the negotiation is
  // host-only — the pure case, where the marker is the only thing that tells
  // the remote agent the list is complete.
  test("a host-only negotiation delivers the peer's end-of-candidates marker", async () => {
    const hostOnly = await spawnRoomEnv({ noStun: true });
    try {
      const a = await openPeer(hostOnly.roomUrl);
      const b = await openPeer(hostOnly.roomUrl);
      await expectConnected(a.page);
      await expectConnected(b.page);
      expect(await opfsWorks(b.page)).toBe(true);

      await a.page.locator("#file-input").setInputFiles([join(roomDir, "direct.bin")]);
      const offer = await poll(a.page, () => {
        const catalog = window.__BORE_TEST__.getCatalogSnapshot();
        return catalog.length === 1 ? catalog[0] : null;
      });
      await b.page.evaluate(
        (offerId) => window.__BORE_TEST__.requestDownload(offerId),
        offer.offerId,
      );
      await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 60_000 });

      // What this test OWNS is the marker; whether a host-only pair actually
      // forms is not something the marker decides (B-A035). A browser that
      // hides its host candidates behind mDNS `.local` names cannot pair on a
      // host with no responder — a property of the runner, not of the fix
      // under test — and the transfer then finishes on the relay, which is the
      // product working. So the marker is HARD and the path is recorded with
      // the evidence that explains it, the same lesson B-A012 learned one test
      // over: a preference asserted as a guarantee reports the environment.
      const observed = [];
      for (const peer of [a, b]) {
        const side = peer === a ? "source" : "recipient";
        const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
        // The marker was APPLIED, on both sides. The trace is the only
        // observer of it: `addIceCandidate(null)` has no return value and no
        // event, so what is gated is the call the actor makes.
        const diag = await peer.page.evaluate(() =>
          window.__BORE_TEST__.readDirectDiagnostics(),
        );
        const traces = [...diag.finished, ...diag.live];
        expect(
          traces.some((trace) => trace.events.some((e) => e.ev === "remote-candidates-done")),
          `${side}: the peer's end-of-candidates marker reached the ICE agent`,
        ).toBe(true);
        // Host-only really means host-only: no reflexive candidate exists to
        // hide a missing marker behind.
        const gathered = traces.flatMap((trace) => Object.keys(trace.candidates.local));
        expect(gathered).not.toContain("srflx");
        // A commit is only ever made on a transport a verified chunk proved,
        // so anything outside these two would be a real defect.
        for (const commit of commits) {
          expect(["direct", "relay"]).toContain(commit.path);
        }
        observed.push({
          side,
          paths: commits.map((c) => c.path),
          candidates: [...new Set(gathered)],
          reasons: traces.map((trace) => trace.reason).filter((reason) => reason !== null),
        });
      }
      // Printed either way: on the run where the pair does NOT form this is
      // the whole diagnosis, and it costs nothing on the run where it does.
      console.log(`[host-only EOC] ${JSON.stringify(observed)}`);

      const download = await Promise.all([
        b.page.waitForEvent("download", { timeout: 60_000 }),
        b.page.locator("#save-file").click(),
      ]).then(([event]) => event);
      const saved = readFileSync(await download.path());
      expect(createHash("sha256").update(saved).digest("hex")).toBe(fileHashHex);

      for (const peer of [a, b]) {
        await peer.cleanup();
      }
    } finally {
      hostOnly.cleanup();
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
