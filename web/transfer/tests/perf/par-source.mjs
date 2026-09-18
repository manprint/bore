// Ad-hoc: publish N files, then wait for all of them to complete.
import { chromium } from "@playwright/test";
import { mkdtempSync, openSync, writeSync, closeSync, linkSync, rmSync } from "node:fs";
import { randomFillSync } from "node:crypto";
import { join } from "node:path";
const roomUrl = process.argv[2];
const sizeMiB = Number(process.argv[3] ?? "32");
const n = Number(process.argv[4] ?? "2");
const MIB = 1048576;
const dir = mkdtempSync("/tmp/wt-par-");
const master = join(dir, "m.bin");
{
  const block = Buffer.allocUnsafe(8 * MIB);
  const fd = openSync(master, "w");
  let left = sizeMiB * MIB;
  while (left > 0) { const t = Math.min(block.length, left); randomFillSync(block, 0, t); writeSync(fd, block, 0, t); left -= t; }
  closeSync(fd);
}
const ctx = await chromium.launchPersistentContext(mkdtempSync("/tmp/wt-parprof-"), {
  acceptDownloads: true, ignoreHTTPSErrors: true,
});
await ctx.addInitScript(() => { window.__BORE_TEST__ = window.__BORE_TEST__ ?? {}; window.__borePerf = {}; });
const page = ctx.pages()[0] ?? (await ctx.newPage());
await page.goto(roomUrl);
await page.locator("#room-status").filter({ hasText: "Connesso" }).first().waitFor({ timeout: 60000 });
while ((await page.locator("#peer-list li").count()) === 0) await new Promise((r) => setTimeout(r, 200));
// ONE setInputFiles call per file: a single call with several paths is one
// SELECTION, and the app publishes it as one multi-file offer — which is one
// transfer, and therefore one SCTP association. That is the opposite of what
// this probe measures.
for (let i = 0; i < n; i += 1) {
  const p = join(dir, `par-${i}.bin`);
  linkSync(master, p);
  await page.locator("#file-input").setInputFiles([p]);
  await new Promise((r) => setTimeout(r, 400));
}
console.log(`WTPUBLISHED n=${n} sizeMiB=${sizeMiB}`);
let firstByteAt = null;
for (;;) {
  const rows = await page.evaluate(() => window.__BORE_TEST__.senderState());
  if (firstByteAt === null && rows.some((r) => r.sentBytes > 0)) firstByteAt = Date.now();
  if (firstByteAt !== null && rows.length === 0) break;
  await new Promise((r) => setTimeout(r, 50));
}
const ms = Date.now() - firstByteAt;
const total = n * sizeMiB;
console.log(`WTPARSRC n=${n} totalMiB=${total} ms=${ms} aggregateMiBs=${(total / (ms / 1000)).toFixed(2)}`);
const traces = await page.evaluate(() => { const r = window.__BORE_TEST__.readDirectDiagnostics?.(); return r ? [...r.finished, ...r.live] : []; });
for (const t of traces) {
  const st = (t.stats ?? []).at(-1) ?? {};
  console.log(`WTTRACE pair=${st.pair?.localType ?? "?"}/${st.pair?.remoteType ?? "?"} rtt=${st.pair?.rttMs ?? "?"} chan_bytes=${st.channel?.bytesSent ?? "?"} waits=${t.drain.waits} longest=${t.drain.longestMs} total=${t.drain.waitedMs} reason=${t.reason ?? "none"}`);
}
await ctx.close().catch(() => {});
rmSync(dir, { recursive: true, force: true });
