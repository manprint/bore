#!/usr/bin/env bash
# Red-check for `secret_leak_hunt.sh`'s RSS verdict (H-18).
#
# The verdict rule decides whether a series of per-phase RSS deltas is a leak.
# It is the one part of that harness with no natural oracle — a run either
# accuses a process or does not, and both answers look equally confident — so
# the rule itself is exercised here against series whose right answer is known:
# two MEASURED plateaus that must pass, synthetic leaks above the instrument's
# resolution that must fail, synthetic leaks below it that must pass (and be
# understood as "below resolution", not "absent"), and zero-mean noise that
# must pass.
#
# The rule is not copied — it is extracted from the harness at run time, so
# this check cannot drift away from the code it checks.
#
#   scripts/perf/rss_verdict_check.sh
#
# Exits non-zero if any case disagrees with its expected verdict.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$HERE/secret_leak_hunt.sh"
[ -r "$SRC" ] || { echo "cannot read $SRC" >&2; exit 2; }

CONNS=400
# The harness default, restated so a change there shows up here as a diff.
RSS_PHASE_SLACK=5120

eval "$(sed -n '/^    rss_verdict(){/,/^    }$/p' "$SRC" | sed 's/^    //')"
command -v rss_verdict >/dev/null 2>&1 || {
    echo "could not extract rss_verdict from $SRC (did its shape change?)" >&2
    exit 2
}

PASS=0; FAIL=0
t(){ # t <expect ok|bad> <label> <deltas...>
    local want="$1" label="$2"; shift 2
    local out; out="$(rss_verdict probe "$@")"
    local got; got="$(printf '%s\n' "$out" | awk '{print $3}')"
    if [ "$got" = "$want" ]; then
        PASS=$((PASS+1)); printf 'PASS  %-40s %s\n' "$label" "$got"
    else
        FAIL=$((FAIL+1)); printf 'FAIL  %-40s want %s got %s\n      %s\n' "$label" "$want" "$got" "$out"
    fi
}

echo "=== rss_verdict red-check (CONNS=$CONNS, slack=$RSS_PHASE_SLACK KiB) ==="

# Measured on 2026-09-12, 8 x 400, the run that found H-18. Both processes
# plateau; both were accused by the pre-H-18 rule.
t ok  "measured consumer, plateaued"     21776 -1568 4020 -896 676 -44 -128 128
t ok  "measured server, plateaued"       4116 984 -624 -776 -212 480 0 2172

# A leak is linear. Above the resolution this instrument states, it must be
# caught however the jitter falls.
t bad "linear 1000 KiB/phase (2.5 KiB/conn)"  5000 1000 1000 1000 1000 1000 1000 1000
t bad "linear 800 KiB/phase (2.0 KiB/conn)"   5000 800 800 800 800 800 800 800
t bad "linear 900 + jitter"                   5000 880 920 900 910 890 900 900
t bad "one phase far above the slack"         5000 0 0 0 0 0 0 9000

# BELOW the stated resolution (5120 KiB / 2800 conns = 1.87 KiB each). These
# must pass: reporting them would mean claiming a sensitivity the instrument
# does not have. The remedy is more connections, not a smaller bound.
t ok  "linear 400 KiB/phase (1.0 KiB/conn)"   5000 400 400 400 400 400 400 400
t ok  "linear 300 KiB/phase (0.75 KiB/conn)"  5000 300 300 300 300 300 300 300

# Allocator noise: large, zero-mean, and not a leak.
t ok  "zero-mean 3 MiB swings"           5000 -3000 3000 -2500 2600 -3000 2900 -2900
t ok  "flat"                             5000 0 0 0 0 0 0 0
t ok  "monotone but under the slack"     5000 200 200 200 200 200 200 200

echo
echo "=== PASS: $PASS FAIL: $FAIL ==="
[ "$FAIL" = 0 ]
