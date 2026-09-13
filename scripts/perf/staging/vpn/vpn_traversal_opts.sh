#!/usr/bin/env bash
# D2: the four UDP-traversal options, priced in TIME TO DIRECT.
#
# THE QUESTION
# ------------
# `--stun-server`, `--nat-udp-preferred-port`, `--try-port-prediction` and
# `--upnp` are shipped, documented, and exercised by NO stage in this campaign
# (`docs/performance/VPN_DEEP_DIVE_PLAN.md` §1). They also do not belong on a
# bandwidth axis: none of them moves a byte faster. What they change is whether
# -- and how quickly -- a link stops being a relay link.
#
# So the deliverable is TIME TO DIRECT: from the connector's first breath to
# the bridge announcing the direct path, read out of the log's own timestamps
# rather than from a poll loop, because a 2 s poll cannot resolve a difference
# this campaign cares about.
#
# TWO THINGS THIS STAGE REFUSES TO DO
# -----------------------------------
# 1. **Score an arm whose flag never applied.** `--upnp` on a router with no
#    PCP and no IGD logs a `debug` line and carries on; its link still reaches
#    direct, at exactly the baseline's speed, and the arm would report a
#    beautiful null result about a feature it never exercised. That is the
#    `vpn_hub` mistake (§14 of the evidence) in a new costume. Every arm
#    therefore states its OWN evidence line, read from the log, and an arm
#    without it is printed NOT-APPLIED and kept out of every median.
#
# 2. **Blend the two populations.** The direct upgrade's ticker fires
#    IMMEDIATELY on its first iteration and then every `DIRECT_RETRY_INTERVAL`
#    (30 s), so a link that wins its first attempt lands in single-digit
#    seconds and a link that needs a second lands past 30. A median across both
#    describes neither -- the same bimodality S-5 found in `direct_ready_ms`.
#    Attempts are therefore counted from the log and reported beside the time.
#
# WHICH END CARRIES THE FLAGS, AND WHY ONLY ONE
# ---------------------------------------------
# The workstation sits behind a home NAT; the VM has a public address and no
# NAT in front of it. Port prediction, a managed port-mapping lease and a
# preferred port are all statements about a NAT, so on the VM they would be
# no-ops with a warning. The flags go on the WORKSTATION (connector) side and
# the VM runs the listener unchanged in every arm -- which also keeps the far
# end constant, so an arm-to-arm difference has one source.
#
# Usage: vpn_traversal_opts.sh     (REPS, PREF_PORT, ALT_STUN overridable)
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
TTD_TIMEOUT="${TTD_TIMEOUT:-100}"      # > one full 30 s grid plus an attempt
PREF_PORT="${PREF_PORT:-45999}"
ALT_STUN="${ALT_STUN:-stun.cloudflare.com:3478}"

vpn_hdr "VPN traversal options -- time to direct, $REPS reps"
echo "  arms carry their flag on the CONNECTOR (this workstation, behind NAT);"
echo "  the VM listener is identical in every arm."
echo "  an arm whose flag left no evidence in the log is NOT-APPLIED, never scored."
echo

# --- log-timestamp arithmetic ----------------------------------------------
# tracing's default formatter stamps every line; the log is therefore a clock
# the poll loop cannot beat. A line with no stamp yields nothing rather than a
# guess.
ts_of() { grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z' <<<"$1" | head -1; }
ms_of() { local t; t="$(ts_of "$1")"; [ -n "$t" ] && date -d "$t" +%s%3N 2>/dev/null; }

# NOTE the array is SAMP and not S, and that is not a style choice.
# `lib.sh` publishes the scalar `S="$BORE_SRV"` -- the staging server's address.
# `declare -A S` on an existing SCALAR does not replace it: bash keeps the old
# value as element **[0]**. So every stage that named its sample array `S`
# printed the server's real IP address into its own raw-samples block, under the
# key `0`, in every run. MEASURED in `pub_ws_conns_r2.out`, where it sat between
# the quic and relay samples. It never reached a median (`med()` refuses a
# non-numeric sample -- that guard earned its keep here), but it reached an
# EVIDENCE FILE, which is precisely how coordinates escape: through prose and
# output, never through code.
declare -A SAMP    # SAMP[arm|metric] = samples
declare -A NA   # NA[arm] = count of not-applied reps

# arm: <label> <evidence-regex> <flags...>
# The evidence regex is the arm's OWN proof that bore acted on the flag. It is
# deliberately a separate argument from the flag: "we passed it" and "it did
# something" are different claims, and only the second is worth a number.
run_arm() {
    local label="$1" evidence="$2"; shift 2
    local log t0 tdir retries cands applied=no ttd="FAILED"

    VPN_LINK_ID="${VPN_RUN_ID}t$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen >/dev/null 2>&1
    sleep 3
    ws_up trv connect "$@" >/dev/null 2>&1
    if ! ws_ready trv 60 >/dev/null; then
        printf '    %-14s FAILED(link never came up)\n' "$label"
        vpn_cleanup; sleep 3; return
    fi

    # Wait for the path, then read the times out of the log rather than out of
    # the wall clock the poll loop kept.
    wait_path trv direct "$TTD_TIMEOUT" >/dev/null
    log="$(ws_log trv 6000)"

    t0="$(ms_of "$(head -1 <<<"$log")")"
    tdir="$(ms_of "$(grep -E 'bridge switched to direct path|vpn path upgraded to direct' <<<"$log" | head -1)")"
    [ -n "$t0" ] && [ -n "$tdir" ] && ttd=$(( tdir - t0 ))

    # RETRIES, not attempts: the upgrade task logs `attempt=N` only when one
    # FAILS, so a link that won its first round prints none. Zero retries and a
    # small ttd is one population; a retry and a ttd past 30 s is the other,
    # and a median across both describes neither (S-5's bimodality).
    retries="$(grep -cE 'attempt=[0-9]+' <<<"$log")"
    # The gather logs the candidate LIST (`candidates=[a, b, c]`), never a
    # count -- so count the entries rather than grepping for a number that is
    # not there. An empty list reads 0, not 1.
    cands="$(grep -oE 'candidates=\[[^]]*\]' <<<"$log" | tail -1 \
             | sed 's/.*\[//; s/\]//' \
             | awk 'BEGIN{FS=","} {print (NF==1 && $1=="") ? 0 : NF}')"

    grep -qE "$evidence" <<<"$log" && applied=yes

    if [ "$applied" = no ]; then
        NA["$label"]=$(( ${NA["$label"]:-0} + 1 ))
        printf '    %-14s NOT-APPLIED (no line matching /%s/) ttd=%sms -- not scored\n' \
               "$label" "$evidence" "$ttd"
    elif [ "$ttd" = FAILED ]; then
        printf '    %-14s applied=yes  ttd=FAILED(never reached direct in %ss)\n' \
               "$label" "$TTD_TIMEOUT"
    else
        SAMP["$label|ttd"]+=" $ttd"
        [ -n "$cands" ] && SAMP["$label|cands"]+=" $cands"
        printf '    %-14s applied=yes  ttd=%-8s retries=%-3s candidates=%-3s\n' \
               "$label" "${ttd}ms" "$retries" "${cands:-?}"
    fi
    vpn_cleanup; sleep 4
}

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    # The control runs in EVERY repetition, so an arm is compared against a
    # baseline drawn from the same minutes of the same path (V-9).
    run_arm baseline   'finished UDP candidate discovery'
    run_arm pref-port  "udp_local_addr.*:$PREF_PORT"          --nat-udp-preferred-port "$PREF_PORT"
    run_arm predict    'port prediction ENABLED'              --try-port-prediction
    run_arm upnp       'managed port mapping ENABLED'         --upnp
    run_arm stun-alt   "stun_server=\"?${ALT_STUN%%:*}"       --stun-server "$ALT_STUN"
done

echo
echo "=== medians ==="
printf '  %-14s %-12s %-12s %s\n' arm 'ttd (ms)' 'candidates' 'not-applied reps'
for arm in baseline pref-port predict upnp stun-alt; do
    printf '  %-14s %-12s %-12s %s\n' "$arm" \
      "$(printf '%s\n' ${SAMP["$arm|ttd"]:-}   | med)" \
      "$(printf '%s\n' ${SAMP["$arm|cands"]:-} | med)" \
      "${NA[$arm]:-0}/$REPS"
done

echo
echo "=== raw samples (V-11: a median with no samples beside it has not been read) ==="
for k in "${!SAMP[@]}"; do printf '  %-22s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== reading ==="
echo "  A flag that never applied (not-applied = REPS) has not been measured here."
echo "  For --upnp that is a statement about THIS router, not about the feature:"
echo "  no PCP and no IGD means there is nothing to lease. Say which it was."
echo "  A ttd difference below ~2 s on a path whose first attempt already wins is"
echo "  noise; the number that would matter is an arm that turns a SECOND-attempt"
echo "  link (>30 s) into a first-attempt one."
echo
echo "DONE"
