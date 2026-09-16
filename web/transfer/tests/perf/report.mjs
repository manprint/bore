// Shared reporting for the web-transfer benchmarks (3.9, T-WEB-PERF).
//
// Two rules the rest of this repository learned the hard way and this file
// enforces for every arm:
//  - an arm that produced no bytes reports FAILED, never 0 (a failed arm that
//    prints a number silently enters a median, and the median is what people
//    quote);
//  - a summary always carries its raw samples, because the bug that corrupts
//    a median is invisible in a file that prints only medians.

/// MiB/s for `bytes` moved in `ms` milliseconds, or `null` when there is
/// nothing to divide: no bytes, or no time.
export function throughputMiBs(bytes, ms) {
  if (!Number.isFinite(bytes) || !Number.isFinite(ms) || bytes <= 0 || ms <= 0) {
    return null;
  }
  return bytes / (1024 * 1024) / (ms / 1000);
}

/// Median over a numeric copy. Sorting numerically here is the point: a
/// locale-dependent textual sort reorders `408` before `264.01` and the
/// median silently reads the wrong sample.
export function median(samples) {
  const numeric = samples.filter((s) => Number.isFinite(s)).slice().sort((a, b) => a - b);
  if (numeric.length === 0) {
    return null;
  }
  const mid = numeric.length >> 1;
  return numeric.length % 2 === 0 ? (numeric[mid - 1] + numeric[mid]) / 2 : numeric[mid];
}

/// One reported line, in the same shape the Rust arm prints, so one driver
/// can collect every arm without knowing who produced it.
export function benchLine(arm, sizeMiB, samples) {
  const raw = samples.filter((s) => Number.isFinite(s)).map((s) => s.toFixed(2)).join(" ");
  const mid = median(samples);
  const value = mid === null ? "FAILED" : `${mid.toFixed(2)}MiB/s`;
  return `PERF ${arm} size=${sizeMiB}MiB median=${value} samples=[${raw}]`;
}

/// Parses `--sizes 8,32 --reps 3` (and the matching BORE_PERF_* env) once,
/// so every arm is driven the same way.
export function benchConfig(argv = process.argv.slice(2), env = process.env) {
  const flag = (name) => {
    const at = argv.indexOf(`--${name}`);
    return at >= 0 && at + 1 < argv.length ? argv[at + 1] : undefined;
  };
  const sizes = (flag("sizes") ?? env.BORE_PERF_SIZES_MIB ?? "8,32")
    .split(",")
    .map((s) => Number.parseInt(s.trim(), 10))
    .filter((s) => Number.isFinite(s) && s > 0);
  const reps = Number.parseInt(flag("reps") ?? env.BORE_PERF_REPS ?? "3", 10);
  return { sizes, reps: Number.isFinite(reps) && reps > 0 ? reps : 3 };
}

/// Per-stage breakdown for one arm (3.10). `samples` is one merged sink per
/// repetition (`{ stage: { ms, calls, bytes } }`); every stage reports the
/// MEDIAN of its own samples beside the raw ones, under the same rule as
/// `benchLine` — a stage nobody measured is absent, never a zero.
///
/// Stages are printed heaviest first, each with its share of the summed
/// medians, because the whole point is to read which one to attack.
export function stageLines(arm, sizeMiB, samples) {
  const names = new Set();
  for (const sink of samples) {
    for (const name of Object.keys(sink ?? {})) {
      names.add(name);
    }
  }
  const rows = [];
  for (const name of names) {
    const ms = samples.map((sink) => sink?.[name]?.ms).filter((v) => Number.isFinite(v));
    const mid = median(ms);
    if (mid === null) {
      continue;
    }
    const calls = median(samples.map((sink) => sink?.[name]?.calls).filter(Number.isFinite));
    rows.push({ name, mid, calls, raw: ms });
  }
  rows.sort((a, b) => b.mid - a.mid);
  const total = rows.reduce((sum, row) => sum + row.mid, 0);
  return rows.map(
    (row) =>
      `PERF-STAGE ${arm} size=${sizeMiB}MiB stage=${row.name} median_ms=${row.mid.toFixed(1)}` +
      ` share=${total > 0 ? ((row.mid / total) * 100).toFixed(1) : "0.0"}%` +
      ` calls=${row.calls ?? 0} samples=[${row.raw.map((v) => v.toFixed(1)).join(" ")}]`,
  );
}
