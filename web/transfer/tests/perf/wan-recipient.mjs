// T-WEB-PERF-WAN, recipient leg. Runs on the REMOTE host, standalone: it
// imports nothing from the repository, because the remote holds a browser
// and a room URL and nothing else.
//
// One JSON line per completed transfer on stdout, prefixed `WTJSON `, so the
// driver can read it without parsing the browser's own noise.
import { chromium } from "@playwright/test";
import { mkdtempSync, rmSync } from "node:fs";

const roomUrl = process.argv[2];
const want = Number(process.argv[3] ?? "1");
const noWebRtc = process.argv[4] === "relay";
const profileRoot = process.argv[5] ?? "/dev/shm";

const profile = mkdtempSync(`${profileRoot}/wt-recipient-`);
const context = await chromium.launchPersistentContext(profile, {
  acceptDownloads: true,
  ignoreHTTPSErrors: true,
  args: ["--no-sandbox"],
});
// The app installs every recorder when this object exists BEFORE it loads.
await context.addInitScript(() => {
  window.__BORE_TEST__ = window.__BORE_TEST__ ?? {};
  window.__borePerf = {};
});
if (noWebRtc) {
  // A real supported case, not a shortcut: a browser with WebRTC off by
  // policy behaves exactly like this, and the page answers `unsupported`
  // at once instead of waiting out the direct deadline.
  await context.addInitScript(() => {
    delete window.RTCPeerConnection;
    delete window.webkitRTCPeerConnection;
  });
}
const page = context.pages()[0] ?? (await context.newPage());
const errors = [];
page.on("pageerror", (e) => { const line = `pageerror: ${e.message}`; errors.push(line); console.log(`WTERR dst ${line}`); });
page.on("console", (m) => {
  if (m.type() === "error") {
    const line = `console: ${m.text()}`;
    errors.push(line);
    console.log(`WTERR dst ${line}`);
  }
});
await page.goto(roomUrl);
await page.locator("#room-status").filter({ hasText: "Connesso" }).first()
  .waitFor({ timeout: 60_000 })
  .catch(async () => {
    const text = await page.locator("#room-status").innerText().catch(() => "?");
    throw new Error(`room never connected, status reads ${JSON.stringify(text)}`);
  });
console.log("WTREADY");

const seen = new Set();
let done = 0;
let saidAt = 0;
const waitingSince = Date.now();
const deadline = Date.now() + 30 * 60_000;
while (done < want && Date.now() < deadline) {
  const offers = await page.evaluate(() => window.__BORE_TEST__.getCatalogSnapshot());
  if (offers.length === 0 && Date.now() - saidAt > 5_000) {
    // Same reason as the source's heartbeat: saying "no offer yet" is the
    // difference between a diagnosable hang and a silent one.
    saidAt = Date.now();
    console.log(`WTWAIT dst +${Math.round((Date.now() - waitingSince) / 1000)}s offers=0`);
  }
  for (const offer of offers) {
    if (seen.has(offer.offerId)) continue;
    seen.add(offer.offerId);
    const bytes = (offer.manifest?.entries ?? []).reduce((n, e) => n + (e.size ?? 0), 0);
    // Zero the stage sink so each transfer's decomposition is its own.
    await page.evaluate(() => { window.__borePerf = {}; });
    const t0 = Date.now();
    await page.evaluate((id) => window.__BORE_TEST__.requestDownload(id), offer.offerId);
    // The badge follows VERIFIED bytes, so the moment it shows a path is the
    // moment the first chunk was hashed and accepted: that is the latency a
    // user perceives, and no throughput number contains it.
    let firstPathAt = null;
    let path = null;
    let saidPathAt = 0;
    while (Date.now() - t0 < 20 * 60_000) {
      if (Date.now() - saidPathAt > 5_000) {
        saidPathAt = Date.now();
        const rows = await page
          .evaluate(() => window.__BORE_TEST__.receiverState?.() ?? null)
          .catch(() => null);
        console.log(
          `WTWAIT dst +${Math.round((Date.now() - t0) / 1000)}s no-commit rows=${JSON.stringify(rows)}`,
        );
      }
      const commits = await page.evaluate(() => [...(window.__BORE_TEST__.pathCommits ?? [])]);
      if (commits.length > 0) {
        firstPathAt = Date.now();
        path = commits[commits.length - 1].path;
        break;
      }
      if (await page.locator("#save-file").isVisible().catch(() => false)) break;
      await new Promise((r) => setTimeout(r, 10));
    }
    // Polled rather than a blind `waitFor`, so the harness can SAY what the
    // recipient is doing: bytes still arriving, or every byte in and the
    // rolling root being recomputed. Those are opposite diagnoses and a
    // wall-clock deadline reads them identically.
    let saidStageAt = 0;
    for (;;) {
      if (await page.locator("#save-file").isVisible().catch(() => false)) {
        break;
      }
      if (Date.now() - t0 > 20 * 60_000) {
        throw new Error("the recipient never staged the file");
      }
      if (Date.now() - saidStageAt > 5_000) {
        saidStageAt = Date.now();
        const snap = await page
          .evaluate(() => ({
            rows: window.__BORE_TEST__.receiverState?.() ?? null,
            // A control error is the difference between "still working" and
            // "the server said no and nobody is coming": it belongs beside
            // the state, not in a summary nobody reaches on a hang.
            receiverErrors: window.__BORE_TEST__.receiverErrors ?? null,
            controlErrors: window.__BORE_TEST__.controlErrors ?? null,
          }))
          .catch(() => null);
        console.log(
          `WTWAIT dst +${Math.round((Date.now() - t0) / 1000)}s staging ${JSON.stringify(snap)}`,
        );
      }
      await new Promise((r) => setTimeout(r, 25));
    }
    const verifiedAt = Date.now();
    const stages = await page.evaluate(() => window.__borePerf ?? {});
    const traces = await page.evaluate(() => {
      const read = window.__BORE_TEST__.readDirectDiagnostics?.();
      return read ? [...read.finished, ...read.live] : [];
    });
    const commits = await page.evaluate(() => [...(window.__BORE_TEST__.pathCommits ?? [])]);
    console.log(`WTJSON ${JSON.stringify({
      offerId: offer.offerId,
      bytes,
      path: path ?? commits.at(-1)?.path ?? null,
      commits: commits.map((c) => c.path),
      ttfbMs: firstPathAt === null ? null : firstPathAt - t0,
      verifyMs: verifiedAt - t0,
      stages,
      trace: traces.at(-1) ?? null,
      errors: errors.splice(0),
    })}`);
    // Dismiss the save card so the next transfer starts from a clean page.
    await page.evaluate(() => {
      document.querySelector("#discard-file")?.click();
    });
    await page.locator("#save-section").waitFor({ state: "detached", timeout: 30_000 }).catch(() => {});
    done += 1;
  }
  await new Promise((r) => setTimeout(r, 100));
}
console.log(`WTDONE ${done}`);
await context.close().catch(() => {});
rmSync(profile, { recursive: true, force: true });
