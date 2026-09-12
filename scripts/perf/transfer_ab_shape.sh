#!/usr/bin/env bash
# Interleaved A/B of the transfer shape sweep: two binaries, same cells, alternating arms.
#
# WHY THIS EXISTS AND NOT JUST TWO transfer_scale.sh RUNS
# ------------------------------------------------------
# Running one whole sweep with binary A and then one whole sweep with binary B does not
# compare A with B. It compares each binary with whatever the machine was doing at the time,
# and on this workstation that difference is large enough to invent a finding: in the run
# this script was written for, the SAME code read 1600 MB/s in the first pair of cells and
# 1384 MB/s in the second, a 14 % drift that is bigger than most of the effects being
# measured. Alternating the arms (before, after, before, after) puts that drift on BOTH arms,
# which is the only way a difference between them means anything.
#
# It is also how the campaign's one disagreeing cell was settled: a single-repetition sweep
# showed the new build 13 % SLOWER at 100 files, in the opposite direction to all four of its
# neighbours. Interleaved, at seven repetitions, the two arms read 1365.3 and 1383.8 — parity.
# The "regression" was one lucky sample of the old binary.
#
# TWO TRAPS, BOTH OF WHICH PRODUCE PLAUSIBLE NUMBERS RATHER THAN ERRORS
# --------------------------------------------------------------------
#   * A cell that CRASHES must not look like a cell that measured something. An earlier
#     driver piped the harness through `awk` and printed an empty column when the expected
#     row was missing; the table looked merely sparse. Here a missing row prints the
#     harness's own last lines, loudly.
#   * Harness ports must sit BELOW `net.ipv4.ip_local_port_range` (32768-60999 on a stock
#     kernel). A "unique port per run" is unique only against other LISTENERS: the previous
#     cell's own sender->server connections can take the number the next cell wants to bind,
#     and they do — `failed to bind the control listener on 0.0.0.0:47106: Address already
#     in use`. The default base here is 21400.
#
# The run directory must be on tmpfs or the disk becomes the variable (see
# docs/performance/TRANSFER_EVIDENCE_2026-09-12.md §7.1); TMPDIR is set for you.
#
# Usage:
#   BEFORE=/path/to/old/bore [FILES="10 100 1000"] [MB=2048] [PAR=8] [REPS=7] [PAIRS=2] \
#     scripts/perf/transfer_ab_shape.sh
#
# BEFORE is the binary to compare against; AFTER defaults to this tree's release build. To
# get a pre-campaign baseline:
#   git worktree add /tmp/base-wt <commit> && (cd /tmp/base-wt && cargo build --release)
set -uo pipefail
cd "$(dirname "$0")/../.."

BEFORE="${BEFORE:?set BEFORE=/path/to/the/other/bore}"
AFTER="${AFTER:-target/release/bore}"
FILES="${FILES:-10 100 1000 5000}"
MB="${MB:-2048}"
PAR="${PAR:-8}"
REPS="${REPS:-7}"
PAIRS="${PAIRS:-2}"          # before/after pairs per cell; 2 = four runs per cell
PORT_BASE="${PORT_BASE:-21400}"
OUT="${OUT:-out/xfer}"
LOGS="$OUT/ab-logs"
mkdir -p "$LOGS"

for b in "$BEFORE" "$AFTER"; do
    [ -x "$b" ] || { echo "not executable: $b" >&2; exit 2; }
done

# A "unique" port is only unique against listeners; stay below the ephemeral range.
read -r EPH_LO _ < /proc/sys/net/ipv4/ip_local_port_range
if [ "$PORT_BASE" -ge "$EPH_LO" ]; then
    echo "PORT_BASE $PORT_BASE is inside the ephemeral range (>= $EPH_LO); pick a lower one" >&2
    exit 2
fi

echo "### transfer A/B shape sweep — $(date -Is)"
echo "###   before : $BEFORE"
echo "###   after  : $AFTER"
echo "###   MB=$MB PAR=$PAR REPS=$REPS PAIRS=$PAIRS files='$FILES'"
echo

i=0
for n in $FILES; do
    echo "===== $n files ====="
    for _ in $(seq "$PAIRS"); do
        for arm in before after; do
            i=$((i+1))
            port=$((PORT_BASE + i))
            [ "$arm" = before ] && bin="$BEFORE" || bin="$AFTER"
            log="$LOGS/$n-$arm-$i.log"
            env TMPDIR=/dev/shm BORE_BIN="$bin" PORT="$port" MB="$MB" FILES="$n" \
                PAR="$PAR" REPS="$REPS" nice -n 19 scripts/perf/transfer_scale.sh \
                > "$log" 2>&1
            line=$(awk -v n="$n" '$1==n' "$log")
            if [ -z "$line" ]; then
                # Loud, with the harness's own words. A silent empty column is how a crash
                # gets published as a measurement.
                printf '  %-8s FAILED (%s): %s\n' "$arm" "$log" \
                    "$(tail -3 "$log" | tr '\n' ' ')"
            else
                printf '  %-8s %s\n' "$arm" "$line"
            fi
        done
    done
done
echo
echo "### $(date -Is) DONE — compare arms WITHIN a cell, never across cells."
