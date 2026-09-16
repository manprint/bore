#!/usr/bin/env bash
# T-WEB-PERF (plan 001, sub-phase 3.9) — web-transfer benchmark driver.
#
# Three arms, one variable apart, run INTERLEAVED so a machine that changes
# under the run changes every arm and not one of them:
#
#   pipe    the server's opaque relay, no application crypto  -> transport ceiling
#   crypto  the shipped AES-GCM framing + chunk digest, no network -> CPU ceiling
#   browser the real page: read, encrypt, relay, decrypt, verify, stage OPFS
#
# Rules this script obeys, each of them paid for elsewhere in this repository:
#   - every raw sample is printed beside the median (a file that prints only
#     medians hides the bug that corrupts them);
#   - LC_ALL=C, because a comma-decimal locale silently reorders a numeric
#     sort and the median then reads the wrong sample (V-11);
#   - an arm that produced nothing prints FAILED and the script exits nonzero:
#     a failed arm must never enter a table as a number (V-9);
#   - absolute numbers belong to the machine that produced them. Quote the
#     RATIOS between arms measured in the same run.
set -euo pipefail
export LC_ALL=C

SIZES="${SIZES:-8,32}"
REPS="${REPS:-3}"
ENGINE="${ENGINE:-chromium}"
ARMS="${ARMS:-pipe,crypto,browser}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
out="${OUT:-$(mktemp -d)}/web_transfer_bench.raw"
mkdir -p "$(dirname "$out")"
: > "$out"

have_arm() { [[ ",$ARMS," == *",$1,"* ]]; }

# A stale binary measures code nobody is running: refuse instead of
# publishing a number for the wrong build.
if [[ ! -x target/debug/bore ]]; then
  echo "FAIL: target/debug/bore missing — run: cargo build --all-features" >&2
  exit 2
fi

echo "== bore web transfer bench =="
echo "sizes=${SIZES} MiB  reps=${REPS}  engine=${ENGINE}  arms=${ARMS}"
echo "host: $(uname -sr)  cpu: $(nproc) threads  date: $(date -Is)"
echo "note: loopback run — absolute rates are this machine's, ratios are the result"
echo "note: the 4.6 sweeps (fragment size, backpressure marks) are NOT part of this"
echo "      run; drive them directly, see docs/transfer/WEB_TRANSFER_PERF.md"
echo

for rep in $(seq 1 "$REPS"); do
  echo "-- repetition ${rep}/${REPS} --"
  if have_arm pipe; then
    BORE_PERF_SIZES_MIB="$SIZES" BORE_PERF_REPS=1 \
      cargo test --all-features --test web_transfer_test t_web_perf_relay -- \
      --ignored --nocapture --test-threads=1 2>/dev/null |
      grep -E "^PERF (pipe|host=)" | tee -a "$out" || true
  fi
  if have_arm crypto; then
    node web/transfer/tests/perf/crypto-bench.mjs --sizes "$SIZES" --reps 1 |
      grep -E "^PERF" | tee -a "$out" || true
  fi
  if have_arm browser; then
    BORE_PERF_SIZES_MIB="$SIZES" BORE_PERF_REPS=1 \
      npx --prefix web/transfer playwright test \
      --config web/transfer/playwright.perf.config.mjs --project="$ENGINE" \
      --grep-invert "sweep" 2>/dev/null |
      grep -E "^PERF" | tee -a "$out" || true
  fi
  echo
done

echo "== summary (median of ${REPS} interleaved repetitions, every sample shown) =="
echo "note: rows are MiB/s except *-over-* rows, which are ratios (direct/relay)"
awk '
  /^PERF [a-z]/ {
    arm = $2
    if (arm == "host=pipe" || arm ~ /^host=/) next
    size = $3; sub("size=", "", size)
    line = $0
    sub(/.*samples=\[/, "", line); sub(/\].*/, "", line)
    n = split(line, values, " ")
    for (i = 1; i <= n; i++) {
      key = arm "\t" size
      samples[key] = samples[key] " " values[i]
      count[key]++
    }
    if (!(arm in seen_arm)) { order[++arms] = arm; seen_arm[arm] = 1 }
    keys[arm "\t" size] = 1
  }
  END {
    printf "%-26s %-10s %12s  %s\n", "arm", "size", "median", "samples"
    failed = 0
    for (a = 1; a <= arms; a++) {
      arm = order[a]
      for (key in keys) {
        split(key, parts, "\t")
        if (parts[1] != arm) continue
        n = split(samples[key], values, " ")
        # numeric insertion sort: no external sort, no locale to corrupt it
        for (i = 2; i <= n; i++) {
          v = values[i] + 0; j = i - 1
          while (j >= 1 && values[j] + 0 > v) { values[j+1] = values[j]; j-- }
          values[j+1] = v
        }
        if (n == 0) { med = "FAILED"; failed = 1 }
        else if (n % 2) med = sprintf("%.2f", values[(n+1)/2] + 0)
        else med = sprintf("%.2f", (values[n/2] + values[n/2+1]) / 2)
        printf "%-26s %-10s %12s  %s\n", parts[1], parts[2], med, samples[key]
      }
    }
    if (failed) exit 1
  }
' "$out"

lines=$(grep -c "^PERF [a-z]" "$out" || true)
if [[ "$lines" -eq 0 ]]; then
  echo "FAIL: no arm produced a measurement" >&2
  exit 1
fi
if grep -q "median=FAILED" "$out"; then
  echo "FAIL: an arm produced no bytes — see $out" >&2
  exit 1
fi
echo
echo "raw lines: $out"
