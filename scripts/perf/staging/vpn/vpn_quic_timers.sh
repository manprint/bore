#!/usr/bin/env bash
# D4: the direct path's QUIC timers, priced in DEAD TIME.
#
# THE QUESTION
# ------------
# `BORE_DIRECT_QUIC_IDLE_MS`, `BORE_DIRECT_QUIC_KEEPALIVE_MS` and
# `BORE_DIRECT_QUIC_INITIAL_RTT_MS` are shipped knobs that no stage exercises
# (`VPN_DEEP_DIVE_PLAN.md` §2). Their unit gates -- `direct_quic_liveness_*`,
# `direct_initial_rtt_unset_is_the_shipped_constant` -- pin how the VALUES are
# resolved and clamped. Nothing shows what they BUY, because the quantity they
# move is not throughput: it is how long a tunnel carries no traffic after the
# direct path dies, and how quickly a new one comes up.
#
# THE STIMULUS, AND WHY IT HAS TO BE THIS ONE
# -------------------------------------------
# `--relay-only` is not the experiment: an arm with no direct path never loses
# one. The product's promise (DEC-2) is that a link on the DIRECT path whose
# UDP disappears falls back to the WARM relay in place -- no reconnect, TUN
# preserved, nonce counter preserved -- and the idle/keepalive pair is what
# decides how many seconds of silence that costs.
#
# So the stimulus is a real blackhole: `nft` drops UDP to and from the far end
# while the TCP relay, which goes to the SERVER and not to that host, keeps
# working. The rule lives in its own table and the library removes it on every
# exit path, including a stage that dies mid-arm.
#
# WHAT IS MEASURED, PER ARM
# -------------------------
#   ttd_ms    link start -> direct                      (initial-rtt's term)
#   dead_ms   blackhole on -> first byte through again  (idle/keepalive's term)
#   back_ms   blackhole off -> direct again             (the 30 s retry grid)
#
# `dead_ms` is measured with TRAFFIC, not with a log line: the log says when
# bore decided the path was gone, the ping says when the tunnel started
# carrying packets again, and only the second is what a user experiences.
# P-12's rule, applied to a timer.
#
# Usage: vpn_quic_timers.sh     (REPS, ARMS overridable)
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
DEAD_TIMEOUT="${DEAD_TIMEOUT:-60}"     # generous: the shipped idle is 10 s
BACK_TIMEOUT="${BACK_TIMEOUT:-100}"    # > one full 30 s upgrade grid

# `label|environment` entries, one per array element. An ARRAY rather than one
# delimited string: the environment of an arm contains spaces, and every
# string-splitting form of this axis that was tried first either lost the
# spaces or mis-paired a label with the next arm's environment.
#
# The shipped pair leads so every later arm is read against a control drawn
# from the same minutes (V-9). The knobs go on BOTH ends: a QUIC idle timeout
# is negotiated from each peer's own transport parameters, so setting one end
# only would measure a mixture of two configurations.
ARMS=(
    "shipped|"
    "fast|BORE_DIRECT_QUIC_IDLE_MS=3000 BORE_DIRECT_QUIC_KEEPALIVE_MS=1000"
    "slow|BORE_DIRECT_QUIC_IDLE_MS=30000 BORE_DIRECT_QUIC_KEEPALIVE_MS=10000"
    "rtt-low|BORE_DIRECT_QUIC_INITIAL_RTT_MS=10"
    "rtt-high|BORE_DIRECT_QUIC_INITIAL_RTT_MS=333"
)

vpn_hdr "VPN direct-path QUIC timers -- dead time, $REPS reps"
echo "  stimulus: nft drops UDP to/from the far end; the TCP relay is untouched."
echo "  dead_ms is measured with PACKETS (ping through the tunnel), not with a log line."
echo

ts_of() { grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z' <<<"$1" | head -1; }
ms_of() { local t; t="$(ts_of "$1")"; [ -n "$t" ] && date -d "$t" +%s%3N 2>/dev/null; }
now_ms() { date +%s%3N; }

# One ping through the overlay. Returns 0 when a packet made it.
# `$B_PEER` AND NOT `$B_ADDR`/`$A_PEER`: the far end, seen from this
# workstation. The connector is brought up with `--vpn-addr $B_ADDR
# --vpn-peer-addr $B_PEER`, so `$A_PEER` (10.77.0.2) is OUR OWN tun address --
# the LISTENER's name for us.
#
# This probe used to ping `$A_PEER`, and a ping to one's own tun address is
# answered by the local stack WITHOUT A PACKET EVER ENTERING THE TUNNEL. It
# therefore succeeded whether the tunnel was alive, dead or never built, which
# is why all 15 arms of the first P7b run reported
# `NEVER-SILENT(blackhole did not bite)`: the blackhole bit perfectly well, and
# the instrument could not see it. The mirror image of "a zero that means the
# instrument failed" -- a probe that can only ever succeed.
#
# Every other VPN stage already used `$B_PEER`; these were the two that did not.
tun_alive() { ping -c1 -W1 -n "$B_PEER" >/dev/null 2>&1; }

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
declare -A SAMP

# Counted across the whole run so the stage can refuse to exit 0 having
# measured nothing -- the defect `vpn_hub` had (19 s, rc=0, no bandwidth) and
# that this stage had too, for all 15 of its arms.
SEAMLESS=0
NOT_SCORED=0

run_arm() {
    local label="$1" envs="$2"
    local log t0 tdir ttd="FAILED" dead="FAILED" back="FAILED" t_off end

    VPN_LINK_ID="${VPN_RUN_ID}q$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    VPN_ENV="$envs" vm_up listen >/dev/null 2>&1
    sleep 3
    VPN_ENV="$envs" ws_up qt connect >/dev/null 2>&1
    if ! ws_ready qt 60 >/dev/null; then
        printf '    %-10s FAILED(link never came up)\n' "$label"
        vpn_cleanup; sleep 3; return
    fi

    if [ "$(wait_path qt direct 100)" != direct ]; then
        # Never scored: an arm that stayed on the relay has no direct path to
        # kill, so its dead time would be zero for a reason that has nothing
        # to do with the knob under test.
        printf '    %-10s FAILED(never reached direct -- nothing to blackhole)\n' "$label"
        vpn_cleanup; sleep 3; return
    fi
    log="$(ws_log qt 6000)"
    t0="$(ms_of "$(head -1 <<<"$log")")"
    tdir="$(ms_of "$(grep -E 'bridge switched to direct path|vpn path upgraded to direct' <<<"$log" | head -1)")"
    [ -n "$t0" ] && [ -n "$tdir" ] && ttd=$(( tdir - t0 ))

    # The tunnel must be carrying traffic BEFORE the stimulus, or "it stopped"
    # is not a finding.
    if ! tun_alive; then
        printf '    %-10s FAILED(overlay not pingable before the blackhole)\n' "$label"
        vpn_cleanup; sleep 3; return
    fi

    # --- kill the direct path and time the silence
    local t_on; t_on="$(now_ms)"
    blackhole_on
    end=$(( $(date +%s) + DEAD_TIMEOUT ))
    # Wait for the outage to actually start: on a keepalive-driven path the
    # first ping after the rule lands may still be answered from in-flight
    # state. An arm that never goes silent is reported, not scored as 0.
    local went_silent=no
    while [ "$(date +%s)" -lt "$end" ]; do
        tun_alive || { went_silent=yes; break; }
        sleep 0.2
    done
    if [ "$went_silent" = yes ]; then
        while [ "$(date +%s)" -lt "$end" ]; do
            if tun_alive; then dead=$(( $(now_ms) - t_on )); break; fi
            sleep 0.2
        done
    else
        # A TUNNEL THAT NEVER WENT SILENT HAS **TWO** OPPOSITE EXPLANATIONS, and
        # this branch used to collapse them into the pessimistic one.
        #
        #   (a) the stimulus never landed -- an instrument failure, and the arm
        #       must not be scored;
        #   (b) the direct path died and the bridge fell back to the WARM RELAY
        #       in place, losing no packet -- DEC-2's seamless fallback working
        #       exactly as specified, in which case `dead` is genuinely 0 and is
        #       the best result this stage can report.
        #
        # They are told apart by asking the SERVER what path the link is on
        # (P-12: read the state, not the log). If it flipped to `relay` the
        # stimulus plainly landed, so silence-that-never-came is the product's
        # doing and 0 is a measurement. If it is still `direct`, nothing was
        # killed and there is nothing to time.
        #
        # MEASURED 2026-09-13: all 15 arms of the first P7b run printed
        # `NEVER-SILENT(blackhole did not bite)` and every median read `n/a` --
        # and the stage exited 0, so a resume marker was written and the next
        # run would have SKIPPED it.
        local after; after="$(wait_path qt relay 30)"
        if [ "$after" = relay ]; then
            dead=0
            SEAMLESS=$((SEAMLESS + 1))
        else
            printf '    %-10s ttd=%-8s dead=INSTRUMENT(stimulus never landed; path still %s)\n' \
                   "$label" "${ttd}ms" "$after"
            NOT_SCORED=$((NOT_SCORED + 1))
            blackhole_off; vpn_cleanup; sleep 3; return
        fi
    fi

    # --- restore and time the return to direct
    t_off="$(now_ms)"
    blackhole_off
    if [ "$(wait_path qt direct "$BACK_TIMEOUT")" = direct ]; then
        back=$(( $(now_ms) - t_off ))
    fi

    for m in ttd dead back; do
        local v="${!m}"
        case "$v" in FAILED) ;; *) SAMP["$label|$m"]+=" $v" ;; esac
    done
    printf '    %-10s ttd=%-9s dead=%-9s back=%-9s\n' \
           "$label" "${ttd}ms" "${dead}ms" "${back}ms"
    vpn_cleanup; sleep 4
}

# A plain loop, never a pipeline: `run_arm` writes the results into `S`, and a
# `while read` on the right-hand side of a pipe runs in a SUBSHELL whose array
# dies with it -- the stage would print an empty table with no error at all.
for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    for entry in "${ARMS[@]}"; do
        run_arm "${entry%%|*}" "${entry#*|}"
    done
done

echo
echo "=== medians (ms) ==="
printf '  %-10s %-12s %-12s %s\n' arm 'ttd' 'dead' 'back to direct'
for entry in "${ARMS[@]}"; do
    arm="${entry%%|*}"
    printf '  %-10s %-12s %-12s %s\n' "$arm" \
      "$(printf '%s\n' ${SAMP["$arm|ttd"]:-}  | med)" \
      "$(printf '%s\n' ${SAMP["$arm|dead"]:-} | med)" \
      "$(printf '%s\n' ${SAMP["$arm|back"]:-} | med)"
done

echo
echo "=== raw samples (V-11) ==="
for k in "${!SAMP[@]}"; do printf '  %-22s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== reading ==="
echo "  dead_ms is the user-visible cost of a direct path dying. If the fast arm"
echo "  does not beat the shipped one, the idle/keepalive pair is NOT what ends"
echo "  the outage on this path -- the fallback is driven by something else, and"
echo "  the knob is documentation rather than a control."
echo "  back_ms is quantised by DIRECT_RETRY_INTERVAL (30 s): a value under it"
echo "  means the retry grid happened to land soon after the rule came off, not"
echo "  that an arm recovers faster."
echo
printf '  seamless (fell back with no packet lost): %d arm(s)\n' "$SEAMLESS"
printf '  not scored (stimulus never landed):       %d arm(s)\n' "$NOT_SCORED"

# A STAGE THAT MEASURED NOTHING MUST NOT EXIT 0 -- otherwise the driver writes a
# resume marker and the next run skips it, reporting a campaign as complete with
# no data in it.
scored=0
for entry in "${ARMS[@]}"; do
    arm="${entry%%|*}"
    [ -n "${SAMP["$arm|dead"]:-}" ] && scored=$((scored + 1))
done
if [ "$scored" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: not one arm produced a dead-time sample."
    echo "  Nothing above is a measurement. Exiting non-zero so no resume marker"
    echo "  is written and the next run repeats this stage instead of skipping it."
    exit 2
fi

echo "DONE"
