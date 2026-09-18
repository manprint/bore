// T-WEB-PERF-WAN, source leg. Runs on the LOCAL host and publishes; the
// recipient is a browser on the other host driven by `recipient.mjs`.
//
// Standalone on purpose, like its twin: the two legs must be readable side
// by side, and the source is not the side that verifies anything.
import { chromium } from "@playwright/test";
import { mkdtempSync, rmSync, openSync, writeSync, closeSync, linkSync } from "node:fs";
import { randomFillSync } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";

const roomUrl = process.argv[2];
const sizeMiB = Number(process.argv[3] ?? "256");
const reps = Number(process.argv[4] ?? "3");
const arm = process.argv[5] ?? "direct";
const payloadRoot = process.argv[6] ?? tmpdir();
const MIB = 1024 * 1024;

/** 8 MiB blocks: a 256 MiB Buffer is 256 MiB of RSS inside the harness. */
function writePayload(path, bytes) {
  const block = Buffer.allocUnsafe(8 * MIB);
  const fd = openSync(path, "w");
  try {
    let left = bytes;
    while (left > 0) {
      const take = Math.min(block.length, left);
      randomFillSync(block, 0, take);
      writeSync(fd, block, 0, take);
      left -= take;
    }
  } finally {
    closeSync(fd);
  }
}

const dir = mkdtempSync(join(payloadRoot, "wt-source-"));
const master = join(dir, "master.bin");
const bytes = Math.round(sizeMiB * MIB);
writePayload(master, bytes);

const profile = mkdtempSync(join(tmpdir(), "wt-profile-"));
const context = await chromium.launchPersistentContext(profile, {
  acceptDownloads: true,
  ignoreHTTPSErrors: true,
});
// The queue depth is a MEASURED policy on the native side (BW-F3, V-13) and
// has to be measurable here too: `WT_HIGH_WATER`/`WT_LOW_WATER` (bytes) reach
// the page through `__borePerf`, which `perfWaterMarks` already reads. Unset
// means the shipped pair, which is itself one of the arms worth running.
const marks = {
  high: Number(process.env.WT_HIGH_WATER ?? 0),
  low: Number(process.env.WT_LOW_WATER ?? 0),
};
await context.addInitScript(([high, low]) => {
  window.__BORE_TEST__ = window.__BORE_TEST__ ?? {};
  window.__borePerf = {};
  if (high > 0 && low > 0 && low < high) {
    window.__borePerf.highWater = high;
    window.__borePerf.lowWater = low;
  }
}, [marks.high, marks.low]);
const page = context.pages()[0] ?? (await context.newPage());
const errors = [];
page.on("pageerror", (e) => { const line = `pageerror: ${e.message}`; errors.push(line); console.log(`WTERR src ${line}`); });
page.on("console", (m) => { if (m.type() === "error") { const line = `console: ${m.text()}`; errors.push(line); console.log(`WTERR src ${line}`); } });
await page.goto(roomUrl);
await page.locator("#room-status").filter({ hasText: "Connesso" }).first().waitFor({ timeout: 60_000 });
// Wait for the other host's browser: the list holds the OTHER peers.
const joinDeadline = Date.now() + 300_000;
while ((await page.locator("#peer-list li").count()) === 0) {
  if (Date.now() > joinDeadline) throw new Error("the remote recipient never joined the room");
  await new Promise((r) => setTimeout(r, 200));
}
console.log("WTREADY");

for (let rep = 0; rep < reps; rep += 1) {
  const name = `wan-${arm}-${rep}.bin`;
  const path = join(dir, name);
  // A hardlink, not a copy: the bytes are identical on purpose (one payload
  // per run, so no repetition pays a different disk), and only the NAME has
  // to differ, because a second request for the same live selection is
  // idempotently re-acked and the repetition would measure nothing.
  linkSync(master, path);
  await page.evaluate(([high, low]) => {
    window.__borePerf = {};
    if (high > 0 && low > 0 && low < high) {
      window.__borePerf.highWater = high;
      window.__borePerf.lowWater = low;
    }
  }, [marks.high, marks.low]);
  const offeredAt = Date.now();
  await page.locator("#file-input").setInputFiles([path]);

  // t0 is the first byte this side actually put on the wire.
  let started = null;
  // A harness that waits in silence can say nothing at all about a hang,
  // which is the failure that costs the most wall clock: this prints WHAT it
  // is waiting for, every five seconds, for as long as it waits.
  let saidAt = 0;
  while (started === null) {
    const rows = await page.evaluate(() => window.__BORE_TEST__.senderState());
    const live = rows.find((r) => r.sentBytes > 0);
    if (live !== undefined) started = { id: live.transferId, at: Date.now() };
    else if (Date.now() - offeredAt > 600_000) throw new Error("the recipient never started");
    else {
      if (Date.now() - saidAt > 5_000) {
        saidAt = Date.now();
        console.log(
          `WTWAIT src +${Math.round((Date.now() - offeredAt) / 1000)}s rows=${JSON.stringify(rows)}`,
        );
      }
      await new Promise((r) => setTimeout(r, 20));
    }
  }
  // The transfer leaves `senderState()` on the server's `transfer.completed`,
  // and the recipient acknowledges only VERIFIED ranges: this instant is the
  // other side having hashed the file, not this side having emptied a buffer.
  let progressAt = 0;
  for (;;) {
    const rows = await page.evaluate(() => window.__BORE_TEST__.senderState());
    if (!rows.some((r) => r.transferId === started.id)) break;
    if (Date.now() - started.at > 20 * 60_000) throw new Error("transfer did not finish");
    if (Date.now() - progressAt > 5_000) {
      // SENT vs VERIFIED, every five seconds: a transfer that is slow and a
      // transfer that is stalled read identically from a wall-clock deadline
      // and are opposite diagnoses. This is the line that tells them apart.
      progressAt = Date.now();
      const row = rows.find((r) => r.transferId === started.id);
      console.log(
        `WTWAIT src +${Math.round((Date.now() - started.at) / 1000)}s sent=${row?.sentBytes ?? "?"} verified=${row?.verifiedBytes ?? "?"} of ${row?.totalBytes ?? "?"}`,
      );
    }
    await new Promise((r) => setTimeout(r, 25));
  }
  const endedAt = Date.now();
  const stages = await page.evaluate(() => window.__borePerf ?? {});
  const traces = await page.evaluate(() => {
    const read = window.__BORE_TEST__.readDirectDiagnostics?.();
    return read ? [...read.finished, ...read.live] : [];
  });
  console.log(`WTJSON ${JSON.stringify({
    rep,
    arm,
    bytes,
    publishMs: started.at - offeredAt,
    sendMs: endedAt - started.at,
    rateMiBs: bytes / MIB / ((endedAt - started.at) / 1000),
    stages,
    trace: traces.at(-1) ?? null,
    errors: errors.splice(0),
  })}`);
  rmSync(path, { force: true });
  // Pace the control plane outside every measured window: publish and
  // request are mutations against a 4/s bucket.
  await new Promise((r) => setTimeout(r, 1200));
}
console.log("WTDONE");
await context.close().catch(() => {});
rmSync(dir, { recursive: true, force: true });
rmSync(profile, { recursive: true, force: true });
