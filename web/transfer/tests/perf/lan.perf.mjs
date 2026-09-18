// T-WEB-PERF-LAN (plan 001, V003-C1) — the SOURCE leg of a two-host run.
//
// Every other benchmark in this folder puts both peers in one process on one
// machine, which is the one link the direct path does not exist for (V-9: a
// loopback pair reaches itself on a host candidate and the "relay" arm is a
// localhost TCP hop). This leg keeps the source here and leaves the recipient
// to a REAL second device — another laptop, or the Android phone V003-F01 was
// reported from — because that is the only arrangement that can answer
// whether the direct path is worth taking in deployment.
//
// It drives only the source, on purpose: a phone cannot be driven by
// Playwright, and a harness that required it would measure the two machines
// that happen to have node on them. What the source can see is enough —
// the recipient acknowledges only VERIFIED ranges, so a transfer the source
// sees complete is a transfer the recipient hashed and accepted.
//
// Driven by `scripts/perf/web_transfer_lan.sh`, which owns the server, the
// room and the summary. Run directly only to debug the leg itself.
import { test, expect } from "@playwright/test";
import { mkdtempSync, openSync, writeSync, closeSync, rmSync } from "node:fs";
import { randomFillSync } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openPersistentPeer } from "../e2e/helpers.mjs";
import { throughputMiBs, median } from "./report.mjs";

const MIB = 1024 * 1024;
const roomUrl = process.env.BORE_LAN_ROOM_URL ?? "";
const sizeMiB = Number(process.env.BORE_LAN_SIZE_MB ?? "400");
const reps = Number(process.env.BORE_LAN_REPS ?? "3");
const arm = process.env.BORE_LAN_ARM ?? "direct";
const joinWaitMs = Number(process.env.BORE_LAN_WAIT_MS ?? "600000");

/**
 * A payload written in 8 MiB blocks: a 400 MiB `Buffer` is 400 MiB of RSS in
 * the harness, and a harness that pages out is a variable inside the
 * measurement.
 */
function writePayload(path, bytes) {
  const block = Buffer.allocUnsafe(8 * MIB);
  const fd = openSync(path, "w");
  try {
    let left = bytes;
    while (left > 0) {
      const take = Math.min(block.length, left);
      // Incompressible, so nothing on the path can flatter the number.
      randomFillSync(block, 0, take);
      writeSync(fd, block, 0, take);
      left -= take;
    }
  } finally {
    closeSync(fd);
  }
}

/** Polls the source's own view of its outgoing transfers. */
async function senderRows(page) {
  return page.evaluate(() => window.__BORE_TEST__.senderState());
}

test.describe("web transfer — two-host LAN leg", () => {
  test.skip(roomUrl === "", "set BORE_LAN_ROOM_URL (use scripts/perf/web_transfer_lan.sh)");
  test.setTimeout(joinWaitMs + reps * 900_000);

  test(`T-WEB-PERF-LAN source leg, arm=${arm}`, async () => {
    const { browserName, defaultBrowserType, channel } = test.info().project.use;
    const dir = mkdtempSync(join(tmpdir(), "bore-lan-"));
    const source = await openPersistentPeer(roomUrl, {
      browserName: browserName ?? defaultBrowserType,
      channel,
      // The arm's ONE variable. Removing `RTCPeerConnection` on this side
      // makes the page answer `unsupported` at once, so the relay attempt
      // starts immediately instead of at the direct deadline — and the
      // recipient needs no flag, which is what lets the other host be a
      // phone.
      noWebRtc: arm === "relay",
      // The driver serves a self-signed certificate for a LAN address unless
      // a real one was supplied; the SOURCE is a headless browser nobody can
      // click through the warning on.
      ignoreHttpsErrors: true,
      init: "window.__borePerf = {};",
    });
    try {
      await expect(source.page.locator("#room-status")).toContainText("Connesso", {
        timeout: 60_000,
      });
      console.log(`PERF-LAN waiting for a recipient to open the room (${joinWaitMs} ms)`);
      // The recipient is a human opening a link on another device. Wait for
      // it to be THERE before the first payload is written, so the disk time
      // is never inside the window somebody is staring at. The list holds the
      // OTHER peers, so one is enough — and it is a lower bound, not an
      // equality, because an operator who opened the link on two devices to
      // find the faster one must not be told the harness is broken.
      await expect
        .poll(() => source.page.locator("#peer-list li").count(), {
          timeout: joinWaitMs,
          message: "no recipient opened the room",
        })
        .toBeGreaterThan(0);
      console.log("PERF-LAN recipient joined");

      const bytes = Math.round(sizeMiB * MIB);
      const rates = [];
      for (let rep = 0; rep < reps; rep += 1) {
        const name = `lan-${arm}-${rep}.bin`;
        const path = join(dir, name);
        writePayload(path, bytes);
        await source.page.locator("#file-input").setInputFiles([path]);
        console.log(`PERF-LAN rep=${rep} offered ${name} — start the download on the recipient`);

        // t0 is the first byte this side actually sent, not the moment the
        // file was offered: the gap between them is the operator's thumb.
        const waitDeadline = Date.now() + joinWaitMs;
        const started = await (async () => {
          for (;;) {
            const row = (await senderRows(source.page)).find((candidate) => candidate.sentBytes > 0);
            if (row !== undefined) {
              return { id: row.transferId, at: Date.now() };
            }
            expect(Date.now(), "the recipient never started the download").toBeLessThan(waitDeadline);
            await new Promise((r) => setTimeout(r, 50));
          }
        })();
        // The transfer leaves `senderState()` when the server's
        // `transfer.completed` arrives, and the recipient acknowledges only
        // ranges whose digest it VERIFIED — so this instant is the recipient
        // having hashed the file, not this side having emptied a buffer.
        const deadline = Date.now() + 900_000;
        for (;;) {
          const rows = await senderRows(source.page);
          if (!rows.some((row) => row.transferId === started.id)) {
            break;
          }
          expect(Date.now(), "the transfer did not finish inside 15 minutes").toBeLessThan(deadline);
          await new Promise((r) => setTimeout(r, 100));
        }
        const rate = throughputMiBs(bytes, Date.now() - started.at);
        rates.push(rate);
        console.log(
          `PERF lan-${arm} rep=${rep} size=${sizeMiB}MiB rate=${rate.toFixed(2)}MiB/s`,
        );
        rmSync(path, { force: true });
        await new Promise((r) => setTimeout(r, 1100));
      }

      const mid = median(rates);
      console.log(
        `PERF lan-${arm} size=${sizeMiB}MiB median=${mid === null ? "FAILED" : `${mid.toFixed(2)}MiB/s`} ` +
          `samples=[${rates.map((r) => r.toFixed(2)).join(" ")}]`,
      );
      // The trace is the half a rate cannot carry: which KIND of pair carried
      // it, how long the source sat on a queue that had stopped draining and
      // what the attempt ended with (V003-C3).
      const traces = await source.page.evaluate(() => {
        const read = window.__BORE_TEST__.readDirectDiagnostics();
        return [...read.finished, ...read.live];
      });
      traces.forEach((trace, index) => {
        const last = trace.stats.at(-1) ?? {};
        const pair = last.pair ?? {};
        const channelStats = last.channel ?? {};
        console.log(
          `PERF lan-path-${arm} attempt=${index} ` +
            `pair=${pair.localType ?? "?"}/${pair.remoteType ?? "?"} rtt=${pair.rttMs ?? "?"}ms ` +
            `pair_bytes=${pair.bytesSent ?? "?"} discarded=${pair.discardedOnSend ?? "?"} ` +
            `chan_bytes=${channelStats.bytesSent ?? "?"} ` +
            `drain_waits=${trace.drain.waits} drain_timeouts=${trace.drain.timeouts} ` +
            `drain_longest=${trace.drain.longestMs}ms drain_total=${trace.drain.waitedMs}ms ` +
            `peak_queued=${trace.drain.peakQueued} elapsed=${trace.elapsedMs}ms ` +
            `reason=${trace.reason ?? "none"}`,
        );
      });
      // An arm whose label and transport disagree is worse than no arm: a
      // `direct` run that silently fell back to the relay would publish the
      // relay's number under the direct name (§4.6).
      const commits = await source.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
      expect(commits.length, "the arm negotiated a path").toBeGreaterThan(0);
      expect(commits.map((c) => c.path), `every commit is ${arm}`).toEqual(
        commits.map(() => arm),
      );
    } finally {
      await source.cleanup();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
