// Stage accounting for the performance harness (3.10). The browser arm of
// T-WEB-PERF measured ~40 MiB/s against a 470 MiB/s transport and a
// 435-605 MiB/s cipher, and a ratio that large is not something to guess at:
// the pipeline is timed where it actually spends, per stage, inside the page.
//
// Disabled unless the harness installs the sink (`globalThis.__borePerf`),
// which it does with an init script before the app loads. With the sink
// absent `start()` is one property read returning a constant and `end()` is
// one property read returning nothing — no clock is sampled, nothing is
// allocated, and the shipped path keeps its behaviour byte for byte.

/** Sentinel for "not timing": `end()` ignores it. */
const OFF = -1;

/** True when the harness installed a sink. */
export function perfOn() {
  return globalThis.__borePerf !== undefined;
}

/**
 * Opens a stage timing, or returns `OFF` when accounting is disabled.
 * @returns {number}
 */
export function perfStart() {
  return globalThis.__borePerf === undefined ? OFF : globalThis.performance.now();
}

/**
 * Closes a stage timing opened by {@link perfStart}, adding the elapsed time
 * (and optionally the bytes that passed through it) to the sink.
 * @param {string} stage stage name, e.g. `src.seal`
 * @param {number} startedAt the value {@link perfStart} returned
 * @param {number} [bytes] plaintext bytes this stage handled
 */
export function perfEnd(stage, startedAt, bytes = 0) {
  const sink = globalThis.__borePerf;
  if (sink === undefined || startedAt === OFF) {
    return;
  }
  const row = sink[stage] ?? (sink[stage] = { ms: 0, calls: 0, bytes: 0 });
  row.ms += globalThis.performance.now() - startedAt;
  row.calls += 1;
  row.bytes += bytes;
}

/**
 * True when the harness asked for the staging worker to be left out, so a
 * run can measure the main-thread OPFS path and the worker path on the same
 * machine in the same session (3.11). Inert — and `false` — with no sink.
 * @returns {boolean}
 */
export function perfNoStageWorker() {
  return globalThis.__borePerf?.noStageWorker === true;
}

/**
 * Fragment size the harness asked the direct path to use, or `null` for the
 * shipped derivation. 4.6 has to measure 8/16/24 KiB on a real DataChannel —
 * the per-message cost of SCTP is not the same on every engine — and the only
 * honest way to do that is to move the size the product actually sends.
 * NEVER a way to exceed the peer: the caller still takes the minimum of this
 * and what `fragmentBytesFor` derived. Inert — and `null` — with no sink.
 * @returns {number|null}
 */
export function perfFragmentBytes() {
  const wanted = globalThis.__borePerf?.fragmentBytes;
  return typeof wanted === "number" && Number.isFinite(wanted) && wanted > 0
    ? Math.floor(wanted)
    : null;
}

/**
 * Backpressure marks the harness asked the direct path to use, or `null` for
 * the shipped 4 MiB / 1 MiB. The depth of this queue is a LATENCY policy
 * (BW-F3, V-13) and the native side learned that "deeper is better" is false
 * and has an optimum, so the browser's marks are MEASURED rather than argued.
 * Both must be present and `low < high`, else the shipped pair stands — a
 * malformed knob must never become a queue of zero.
 * @returns {{high: number, low: number}|null}
 */
export function perfWaterMarks() {
  const high = globalThis.__borePerf?.highWater;
  const low = globalThis.__borePerf?.lowWater;
  if (
    typeof high !== "number" ||
    typeof low !== "number" ||
    !Number.isFinite(high) ||
    !Number.isFinite(low) ||
    low < 0 ||
    high <= low
  ) {
    return null;
  }
  return { high: Math.floor(high), low: Math.floor(low) };
}

/**
 * Records a stage that has no duration of its own (a counter), e.g. how many
 * frames a batch carried.
 * @param {string} stage stage name
 * @param {number} [bytes] bytes to attribute
 */
export function perfCount(stage, bytes = 0) {
  const sink = globalThis.__borePerf;
  if (sink === undefined) {
    return;
  }
  const row = sink[stage] ?? (sink[stage] = { ms: 0, calls: 0, bytes: 0 });
  row.calls += 1;
  row.bytes += bytes;
}
