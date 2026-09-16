// T-WEB-PERF, `browser` arm: the rate a real user sees — read the file,
// encrypt it, relay it, decrypt it, verify every chunk, stage it in OPFS —
// and, separately, what saving the staged file to disk costs.
//
// Read it beside the other two arms: `pipe` is what the server can carry and
// `crypto` is what the CPU can do, so a browser number far below both points
// at the staging layer and not at the cipher or the socket.
import { test, expect } from "@playwright/test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnRoomEnv, openPersistentPeer, opfsWorks } from "../e2e/helpers.mjs";
import { benchLine, throughputMiBs, benchConfig, stageLines, median } from "./report.mjs";

const { sizes, reps } = benchConfig([], process.env);
const MIB = 1024 * 1024;

let env = null;
let roomDir = null;

test.beforeAll(async () => {
  // The throttle is a product feature, not the thing under test: measured
  // through it, every arm would report the bucket (100 MiB/s by default).
  // `BORE_PERF_NO_STUN=1` drops the public STUN chain from the page's ICE
  // configuration. It is one variable, and the arm that needs it is the
  // direct one: a loopback pair reaches itself on a host candidate.
  env = await spawnRoomEnv({ relayRate: 0, noStun: process.env.BORE_PERF_NO_STUN === "1" });
  roomDir = mkdtempSync(join(tmpdir(), "bore-perf-"));
}, 120_000);

test.afterAll(async () => {
  env?.cleanup();
  if (roomDir) {
    rmSync(roomDir, { recursive: true, force: true });
  }
});

async function openPeer(
  url,
  { noStageWorker = false, noWebRtc = false, fragmentBytes = null, marks = null } = {},
) {
  const { browserName, defaultBrowserType, channel } = test.info().project.use;
  return openPersistentPeer(url, {
    browserName: browserName ?? defaultBrowserType,
    channel,
    // `noWebRtc` is how an arm declares its TRANSPORT. Since 4.2 the direct
    // path is the default, so a recipient that says nothing measures WebRTC;
    // the relay arm has to ask for the relay, exactly as every Phase 3 gate
    // now does. An arm whose label and transport disagree is worse than no
    // arm at all — the 3.11 rows were published as `relay` after direct had
    // already become the default (§4.6).
    noWebRtc,
    // Installing the sink BEFORE the app loads is what turns stage
    // accounting on: with it absent every mark is one property read.
    // `noStageWorker` additionally keeps that page on the main-thread OPFS
    // path, so 3.11's before/after is one variable wide and both arms run
    // on the same machine in the same repetition.
    init: `window.__borePerf = ${JSON.stringify({
      noStageWorker,
      ...(fragmentBytes === null ? {} : { fragmentBytes }),
      ...(marks === null ? {} : { highWater: marks.high, lowWater: marks.low }),
    })};
      (() => {
        // Main-thread responsiveness, which no throughput number can show:
        // a timer that should fire every 16 ms reports how long the page was
        // unable to run anything — the same stall a progress bar, a cancel
        // click or a scroll would have waited for.
        let last = performance.now();
        window.__boreLag = 0;
        setInterval(() => {
          const now = performance.now();
          const lag = now - last - 16;
          if (lag > window.__boreLag) { window.__boreLag = lag; }
          last = now;
        }, 16);
      })();`,
  });
}

/** Zeroes the recorded main-thread stall on both pages. */
async function resetLag(pages) {
  for (const page of pages) {
    await page.evaluate(() => {
      window.__boreLag = 0;
    });
  }
}

/** Worst main-thread stall seen on either page since the last reset. */
async function readLag(pages) {
  let worst = 0;
  for (const page of pages) {
    worst = Math.max(worst, await page.evaluate(() => window.__boreLag ?? 0));
  }
  return worst;
}

/** Zeroes both pages' stage sinks so a repetition measures only itself. */
async function resetStages(pages) {
  for (const page of pages) {
    await page.evaluate(() => {
      // Keep the mode flags: zeroing them would move a page onto the other
      // path — or back to the shipped fragment size — halfway through the run,
      // and every attempt reads them again when it builds its channel.
      const fresh = {};
      if (window.__borePerf?.noStageWorker === true) {
        fresh.noStageWorker = true;
      }
      if (typeof window.__borePerf?.fragmentBytes === "number") {
        fresh.fragmentBytes = window.__borePerf.fragmentBytes;
      }
      if (typeof window.__borePerf?.highWater === "number") {
        fresh.highWater = window.__borePerf.highWater;
        fresh.lowWater = window.__borePerf.lowWater;
      }
      window.__borePerf = fresh;
    });
  }
}

/** Reads both pages' sinks back and merges them into one table. */
async function readStages(pages) {
  const merged = {};
  for (const page of pages) {
    const sink = await page.evaluate(() => window.__borePerf ?? {});
    for (const [stage, row] of Object.entries(sink)) {
      const into = merged[stage] ?? (merged[stage] = { ms: 0, calls: 0, bytes: 0 });
      into.ms += row.ms;
      into.calls += row.calls;
      into.bytes += row.bytes;
    }
  }
  return merged;
}

async function poll(page, fn, arg, timeoutMs = 300_000) {
  const start = Date.now();
  for (;;) {
    const value = await page.evaluate(fn, arg);
    if (value) {
      return value;
    }
    if (Date.now() - start > timeoutMs) {
      throw new Error("perf poll timed out");
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

/** Deterministic payload: the same bytes for every arm and repetition. */
function payloadOf(sizeMiB) {
  const bytes = Buffer.alloc(sizeMiB * MIB);
  for (let i = 0; i < bytes.length; i += 1) {
    bytes[i] = (i * 7 + 3) % 251;
  }
  return bytes;
}

/** One empty sample sink per arm. */
function newSamples() {
  return { transfer: [], save: [], wall: [], stages: [], lag: [] };
}

/**
 * One measured repetition for one recipient: publish a fresh file, request
 * it, wait until every chunk is verified, save it, and collect the page's own
 * clock, the polled wall clock, the save time, the stage decomposition and
 * the worst main-thread stall.
 */
async function measureOnce(a, b, into, name, bytes) {
  // A fresh name per repetition and per arm: a second request for the SAME
  // live selection is idempotently re-acked, and the repetition would measure
  // nothing at all.
  const path = join(roomDir, name);
  writeFileSync(path, bytes);
  await a.page.locator("#file-input").setInputFiles([path]);
  const offer = await poll(
    b.page,
    (wanted) =>
      window.__BORE_TEST__
        .getCatalogSnapshot()
        .find((offer) => offer.manifest?.entries?.some((entry) => entry.path === wanted)) ?? null,
    name,
  );

  await resetStages([a.page, b.page]);
  await resetLag([b.page]);
  const startedAt = Date.now();
  const started = await b.page.evaluate(
    (offerId) => window.__BORE_TEST__.requestDownload(offerId),
    offer.offerId,
  );
  expect(started).toEqual({ pending: true });
  await expect(b.page.locator("#save-file")).toBeVisible({ timeout: 300_000 });
  const verifiedAt = Date.now();
  into.lag.push(await readLag([b.page]));
  into.stages.push(await readStages([a.page, b.page]));

  const download = await Promise.all([
    b.page.waitForEvent("download", { timeout: 300_000 }),
    b.page.locator("#save-file").click(),
  ]).then(([event]) => event);
  const savedPath = await download.path();
  const savedAt = Date.now();
  expect(savedPath).toBeTruthy();

  // The page's own clock decides the transfer number: `verifiedAt` comes from
  // a DOM poll whose grid (100/250/500 ms) quantized 32 MiB to the same
  // ~40 MiB/s at every size (3.10). The polled wall is still reported, as its
  // own arm, so the two stay visible together.
  const e2eMs = into.stages[into.stages.length - 1]?.["dst.e2e"]?.ms;
  into.transfer.push(throughputMiBs(bytes.length, e2eMs));
  into.wall.push(throughputMiBs(bytes.length, verifiedAt - startedAt));
  into.save.push(throughputMiBs(bytes.length, savedAt - verifiedAt));
  await expect(b.page.locator("#save-section")).toHaveCount(0, { timeout: 30_000 });
  rmSync(path, { force: true });
  // Pace the control plane between arms, outside every measured window:
  // publish + request are mutations against a 4/s bucket.
  await new Promise((r) => setTimeout(r, 1100));
}

/**
 * Runs every arm of one comparison across the configured sizes. Both arms
 * live inside ONE repetition, never all of one then all of the other: the
 * line under a benchmark moves, and a split run would attribute that move to
 * the change (V-13). The ORDER alternates too, because a fixed order makes
 * "first in the repetition" a second variable and hands it entirely to one
 * arm (§8.85).
 */
async function runComparison(a, recipients, tag, onSize) {
  for (const sizeMiB of sizes) {
    const bytes = payloadOf(sizeMiB);
    const samples = new Map(recipients.map(({ label }) => [label, newSamples()]));
    for (let rep = 0; rep < reps; rep += 1) {
      const order = rep % 2 === 0 ? recipients : [...recipients].reverse();
      for (const { label, peer: b } of order) {
        await measureOnce(a, b, samples.get(label), `perf-${tag}-${sizeMiB}-${rep}-${label}.bin`, bytes);
      }
    }
    await onSize(sizeMiB, samples);
  }
}

/** The four throughput lines, the stall row and the stage decomposition. */
function reportArm(arm, sizeMiB, into) {
  console.log(benchLine(arm, sizeMiB, into.transfer));
  console.log(benchLine(`${arm}-wall`, sizeMiB, into.wall));
  console.log(benchLine(`${arm}-save`, sizeMiB, into.save));
  // Lower is better here, unlike every other line: it is milliseconds of
  // main-thread stall, so it is printed as its own labelled row.
  console.log(
    `PERF ${arm}-mainthread-stall-ms size=${sizeMiB}MiB median=${median(into.lag).toFixed(1)}ms samples=[${into.lag
      .map((v) => v.toFixed(1))
      .join(" ")}]`,
  );
  // Where the time actually goes: medians across the same repetitions the
  // throughput line summarises, so the two are read together.
  for (const line of stageLines(arm, sizeMiB, into.stages)) {
    console.log(line);
  }
}

/** Every peer reports no page error, then closes its profile. */
async function closeAll(peers) {
  for (const peer of peers) {
    expect(peer.failures).toEqual([]);
    await peer.cleanup();
  }
}

test("browser end-to-end throughput", async () => {
  const engine = test.info().project.name;
  const a = await openPeer(env.roomUrl, { noWebRtc: true });
  // Two recipients, one variable apart: the shipped page (staging worker,
  // `createSyncAccessHandle`) and the same page kept on the main-thread
  // `createWritable` path that defined correctness before 3.11. BOTH are on
  // the relay, which is what this comparison has always been published
  // against — the transport is the OTHER test's variable, never this one's.
  const recipients = [
    { label: "worker", peer: await openPeer(env.roomUrl, { noWebRtc: true }) },
    { label: "mainthread", peer: await openPeer(env.roomUrl, { noStageWorker: true, noWebRtc: true }) },
  ];
  for (const page of [a.page, ...recipients.map((r) => r.peer.page)]) {
    await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
  }
  // Without OPFS there is no download to measure; say so instead of
  // publishing a number for a path that did not run.
  for (const { label, peer } of recipients) {
    expect(await opfsWorks(peer.page), `${label} recipient has OPFS`).toBe(true);
  }

  console.log(`PERF host=browser (${engine}, relay, OPFS staging, loopback)`);
  await runComparison(a, recipients, "stage", async (sizeMiB, samples) => {
    for (const { label } of recipients) {
      reportArm(label === "worker" ? `browser-${engine}` : `browser-${label}-${engine}`, sizeMiB, samples.get(label));
    }
  });

  await closeAll([a, ...recipients.map((r) => r.peer)]);
});

// T-WEB-PERF-DIRECT (4.6): the two TRANSPORTS, one variable apart, measured in
// the SAME repetition and published as a ratio. A number for the direct path
// taken in a different run than the relay's is not a comparison — the line
// under both moves, and the ratio is the only quantity that survives it.
test("direct versus relay throughput", async () => {
  const engine = test.info().project.name;
  const a = await openPeer(env.roomUrl);
  const recipients = [
    // The shipped default: no flag, WebRTC present, direct path.
    { label: "direct", peer: await openPeer(env.roomUrl) },
    // A real engine without WebRTC — the supported case a policy-disabled
    // browser is in, and the one that falls back IMMEDIATELY instead of
    // spending the 10 s deadline on every repetition.
    { label: "relay", peer: await openPeer(env.roomUrl, { noWebRtc: true }) },
  ];
  for (const page of [a.page, ...recipients.map((r) => r.peer.page)]) {
    await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
  }
  for (const { label, peer } of recipients) {
    expect(await opfsWorks(peer.page), `${label} recipient has OPFS`).toBe(true);
  }

  console.log(`PERF host=browser (${engine}, direct vs relay, OPFS staging, loopback)`);
  await runComparison(a, recipients, "path", async (sizeMiB, samples) => {
    for (const { label } of recipients) {
      reportArm(`${label}-${engine}`, sizeMiB, samples.get(label));
    }
    // The ratio is taken PER REPETITION and only then medianed: dividing two
    // medians taken minutes apart would hide exactly the drift the alternating
    // order exists to cancel.
    const direct = samples.get("direct").transfer;
    const relay = samples.get("relay").transfer;
    const ratios = direct.map((d, i) =>
      Number.isFinite(d) && Number.isFinite(relay[i]) && relay[i] > 0 ? d / relay[i] : null,
    );
    const mid = median(ratios);
    console.log(
      `PERF direct-over-relay-${engine} size=${sizeMiB}MiB median=${mid === null ? "FAILED" : `${mid.toFixed(3)}x`} samples=[${ratios
        .map((r) => (Number.isFinite(r) ? r.toFixed(3) : "FAILED"))
        .join(" ")}]`,
    );
  });

  // A direct arm that produced no bytes is THE defect, not a missing number:
  // fail loudly rather than publish a silent relay figure under a direct
  // label. Each RECIPIENT is asserted on its own page — the source serves both
  // arms, so its commit list legitimately holds one of each.
  for (const { label, peer } of recipients) {
    const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
    expect(commits.length, `the ${label} arm negotiated a path`).toBeGreaterThan(0);
    expect(commits.map((c) => c.path)).toEqual(commits.map(() => label));
  }
  const directSockets = await recipients[0].peer.page.evaluate(() =>
    [...window.__BORE_TEST__.wsUrls].filter((url) => url.includes("/transfer/ws/relay/")),
  );
  expect(directSockets, "the direct arm opened no relay socket").toEqual([]);

  await closeAll([a, ...recipients.map((r) => r.peer)]);
});

// 4.6 catalogue entry 2: the fragment is derived from the peer's
// `maxMessageSize`, and 24 KiB is the protocol ceiling — but the per-message
// cost of SCTP is not the same on every engine, so the winner is MEASURED and
// not assumed. Every arm is a direct transfer differing only in fragment size,
// all inside the same repetition and with the order rotated.
test("direct fragment size sweep", async () => {
  const engine = test.info().project.name;
  const wanted = (process.env.BORE_PERF_FRAGMENTS ?? "8192,16384,24576")
    .split(",")
    .map((v) => Number.parseInt(v.trim(), 10))
    .filter((v) => Number.isFinite(v) && v > 0);
  const a = await openPeer(env.roomUrl);
  const recipients = [];
  for (const size of wanted) {
    recipients.push({
      label: `frag${size}`,
      peer: await openPeer(env.roomUrl, { fragmentBytes: size }),
    });
  }
  for (const page of [a.page, ...recipients.map((r) => r.peer.page)]) {
    await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
  }
  for (const { label, peer } of recipients) {
    expect(await opfsWorks(peer.page), `${label} recipient has OPFS`).toBe(true);
  }

  console.log(`PERF host=browser (${engine}, direct fragment sweep, loopback)`);
  await runComparison(a, recipients, "frag", async (sizeMiB, samples) => {
    for (const { label } of recipients) {
      reportArm(`${label}-${engine}`, sizeMiB, samples.get(label));
    }
  });

  // The size is the RECIPIENT's: it creates the channel, so it is the side
  // whose ready event carries the number the source then fragments to.
  for (const { label, peer } of recipients) {
    const ready = await peer.page.evaluate(() =>
      [...window.__BORE_TEST__.directEvents].filter((e) => e.kind === "ready"),
    );
    expect(ready.length, `${label} opened a direct channel`).toBeGreaterThan(0);
  }

  await closeAll([a, ...recipients.map((r) => r.peer)]);
});

// 4.6 catalogue entry 1: the depth of the send queue is a LATENCY policy, and
// the native side learned that "deeper is better" is false and has an optimum
// (V-13). In a tab the queue is worse than a socket buffer — it is the tab's
// own heap, and past the engine's buffer it is usrsctp's, which drops and
// retransmits. So the marks are MEASURED, not argued, and this arm is what
// says whether 4 MiB / 1 MiB is the right pair.
test("direct backpressure sweep", async () => {
  const engine = test.info().project.name;
  const pairs = (process.env.BORE_PERF_MARKS ?? "4194304:1048576,1048576:262144,262144:65536")
    .split(",")
    .map((pair) => pair.split(":").map((v) => Number.parseInt(v.trim(), 10)))
    .filter(([high, low]) => Number.isFinite(high) && Number.isFinite(low) && high > low);
  const a = await openPeer(env.roomUrl);
  const recipients = [];
  for (const [high, low] of pairs) {
    recipients.push({
      label: `hw${Math.round(high / 1024)}k`,
      peer: await openPeer(env.roomUrl, { marks: { high, low } }),
    });
  }
  for (const page of [a.page, ...recipients.map((r) => r.peer.page)]) {
    await expect(page.locator("#room-status")).toContainText("Connesso", { timeout: 30_000 });
  }
  for (const { label, peer } of recipients) {
    expect(await opfsWorks(peer.page), `${label} recipient has OPFS`).toBe(true);
  }

  console.log(`PERF host=browser (${engine}, direct backpressure sweep, loopback)`);
  await runComparison(a, recipients, "marks", async (sizeMiB, samples) => {
    for (const { label } of recipients) {
      reportArm(`${label}-${engine}`, sizeMiB, samples.get(label));
    }
  });

  for (const { label, peer } of recipients) {
    const commits = await peer.page.evaluate(() => [...window.__BORE_TEST__.pathCommits]);
    expect(commits.length, `${label} negotiated a path`).toBeGreaterThan(0);
    expect(commits.map((c) => c.path)).toEqual(commits.map(() => "direct"));
  }

  await closeAll([a, ...recipients.map((r) => r.peer)]);
});
