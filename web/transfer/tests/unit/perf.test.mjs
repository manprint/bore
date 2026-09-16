// Reporting rules for the benchmark harness (3.9). The numbers are measured
// elsewhere; what is pinned here is that the harness cannot publish a number
// it did not measure, and cannot hide the samples behind a median.
import { test } from "node:test";
import assert from "node:assert/strict";
import { throughputMiBs, median, benchLine, benchConfig, stageLines } from "../perf/report.mjs";

test("perf_harness_fails_loudly_when_an_arm_produces_no_bytes", () => {
  assert.equal(throughputMiBs(0, 1000), null);
  assert.equal(throughputMiBs(1024 * 1024, 0), null);
  assert.equal(throughputMiBs(Number.NaN, 10), null);
  // An arm with no sample says FAILED; it never contributes a 0 to a median.
  const line = benchLine("browser", 32, []);
  assert.match(line, /median=FAILED/);
  assert.match(line, /samples=\[\]/);
  assert.equal(median([]), null);
});

test("perf_reports_every_sample_beside_the_median", () => {
  const rate = throughputMiBs(64 * 1024 * 1024, 2000);
  assert.equal(rate, 32);
  const line = benchLine("crypto-open", 64, [397.46, 264.01, 408]);
  assert.match(line, /median=397\.46MiB\/s/);
  for (const sample of ["397.46", "264.01", "408.00"]) {
    assert.ok(line.includes(sample), `${sample} missing from ${line}`);
  }
  // Numeric ordering, not textual: "408" must not sort before "264.01".
  assert.equal(median([408, 264.01, 397.46]), 397.46);
  assert.equal(median([4, 1, 3, 2]), 2.5);
});

test("perf_config_reads_flags_then_env_then_defaults", () => {
  assert.deepEqual(benchConfig(["--sizes", "16,64", "--reps", "5"], {}), {
    sizes: [16, 64],
    reps: 5,
  });
  assert.deepEqual(benchConfig([], { BORE_PERF_SIZES_MIB: "128", BORE_PERF_REPS: "2" }), {
    sizes: [128],
    reps: 2,
  });
  assert.deepEqual(benchConfig([], {}), { sizes: [8, 32], reps: 3 });
  // A malformed value falls back to the default instead of producing 0 reps.
  assert.deepEqual(benchConfig(["--reps", "nonsense"], {}).reps, 3);
});

test("perf_stage_breakdown_ranks_by_median_and_keeps_raw_samples", () => {
  const lines = stageLines("browser-chromium", 32, [
    { "dst.open": { ms: 300, calls: 1365, bytes: 0 }, "dst.stage": { ms: 100, calls: 32, bytes: 0 } },
    { "dst.open": { ms: 320, calls: 1365, bytes: 0 }, "dst.stage": { ms: 90, calls: 32, bytes: 0 } },
    { "dst.open": { ms: 280, calls: 1365, bytes: 0 }, "dst.stage": { ms: 110, calls: 32, bytes: 0 } },
  ]);
  // Heaviest stage first, so the line to act on is the first one read.
  assert.match(lines[0], /stage=dst\.open /);
  assert.match(lines[0], /median_ms=300\.0/);
  assert.match(lines[0], /calls=1365/);
  // Raw samples travel beside every median, V-11's rule applied per stage.
  assert.match(lines[0], /samples=\[300\.0 320\.0 280\.0\]/);
  assert.match(lines[1], /stage=dst\.stage .*median_ms=100\.0/);
  // Shares are of the summed medians and add up.
  assert.match(lines[0], /share=75\.0%/);
  assert.match(lines[1], /share=25\.0%/);
  // A stage nobody measured is absent, never a zero row.
  assert.equal(stageLines("x", 8, [{}, {}]).length, 0);
});

test("perf_accounting_is_inert_without_a_sink_and_exact_with_one", async () => {
  const { perfEnd, perfStart, perfOn } = await import("../../src/perf.js");
  delete globalThis.__borePerf;
  // Disabled: nothing is sampled, nothing is stored, nothing throws — the
  // shipped path must not pay for the harness.
  assert.equal(perfOn(), false);
  assert.equal(perfStart(), -1);
  perfEnd("dst.open", perfStart(), 1024);
  assert.equal(globalThis.__borePerf, undefined);
  // Enabled: elapsed time, call count and bytes accumulate per stage.
  globalThis.__borePerf = {};
  const at = perfStart();
  assert.notEqual(at, -1);
  perfEnd("dst.open", at, 1024);
  perfEnd("dst.open", perfStart(), 512);
  const row = globalThis.__borePerf["dst.open"];
  assert.equal(row.calls, 2);
  assert.equal(row.bytes, 1536);
  assert.ok(row.ms >= 0);
  // A start taken while disabled is ignored even if the sink appears later.
  perfEnd("dst.stage", -1, 4096);
  assert.equal(globalThis.__borePerf["dst.stage"], undefined);
  delete globalThis.__borePerf;
});
