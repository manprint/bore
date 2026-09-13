#!/usr/bin/env bash
# D5: `--carriers` on BOTH VPN paths, with the flow count as a real axis.
#
# WHAT IS ALREADY SETTLED, AND WHAT IS NOT
# ----------------------------------------
# V-13 measured `--carriers` on the DIRECT path and falsified it: 4 carriers
# delivered LESS than 1 (359 vs 388 Mbit/s) at 1.7x the CPU. That result is not
# re-litigated here -- but it was measured with the uplink saturated by a fixed
# UDP offer, and BW-F2 says the direct path is FLOW-PINNED: `flow_carrier`
# hashes the inner 5-tuple so one inner connection always rides one carrier.
# A single flow therefore CANNOT use a second carrier, by construction. So the
# only form of the direct question still open is: do several inner flows spread
# across carriers, and does spreading buy anything?
#
# The RELAY path has never been measured with carriers at all on the real path,
# and it is a different mechanism: N AEAD substream pairs with PER-DATAGRAM
# round-robin (DEC-7), not flow pinning. Per-datagram striping is exactly what
# BW-F2 forbids on the direct path because reordering reads as loss to the
# tunnelled TCP -- on the relay it is safe only because the substreams are
# reliable and ordered. Whether it HELPS is an open question with a plausible
# answer in both directions, which is what makes it worth measuring.
#
# THE AXIS
# --------
#   path      relay | direct   -- verified, never assumed (`wait_path`)
#   carriers  1 | 2 | 4        -- negotiated min(listener, connector, server)
#   flows     1 | 4            -- one flow cannot use a second carrier on direct
#
# Every repetition samples a BARE control first (V-9), so each cell is reported
# as a ratio against a line measured in the same minutes, and the absolute
# numbers are reported beside it rather than instead of it.
#
# Usage: vpn_carriers.sh     (REPS, SECS and CELLS_SPEC overridable)
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
SECS="${SECS:-10}"

# `path|carriers|flows`. Kept deliberately short: the relay ladder is the open
# question, the direct rows are the multi-flow check V-13 did not run.
#
# THE `flows=8` ROWS WERE ADDED AFTER A MEASUREMENT, NOT BEFORE ONE.
# `vpn_profile`, run twice hours apart, shows the RELAY COLLAPSING as inner
# flows multiply while the DIRECT path stays flat:
#
#   relay  up  : 550 -> 318 -> 250 -> 206   (run 1, flows 1/2/4/8)
#   relay  up  : 575 -> 596 -> 373 -> 204   (run 2)
#   direct up  : 695 -> 692 -> 695 -> 696   (run 1)
#   direct up  : 710 -> 699 -> 701 -> 703   (run 2)
#
# At eight inner flows the relay delivers ~36 % of what it delivers at one, and
# the shape reproduces across both runs. The mechanism is available in this
# repository's own design: the relay multiplexes every inner packet into ONE
# ordered, reliable byte stream, so eight inner TCP flows share one congestion
# window and block each other head-of-line; the direct path carries them as
# unreliable QUIC DATAGRAMS, where an inner flow's loss or reorder is invisible
# to the others.
#
# If that is the mechanism, then carriers -- which stripe the relay across N
# substream pairs (DEC-7) -- are the remedy the product already ships, and the
# collapse should ease as N rises. The ladder therefore has to reach the flow
# count where the collapse actually is: at flows=4 the relay is at 373 of 575
# and the effect is half visible; at flows=8 it is at 204 and unmistakable.
# Three more cells, 3 reps x 10 s each, all UPLOAD -- which AWS does not bill.
#
# `CELLS_SPEC` OVERRIDES THE LIST, and until now the usage line above LIED about
# that: it advertised `CELLS` as overridable while the array below was a plain
# assignment that clobbered anything the caller exported. The distinction
# matters because the open question this stage left is not "run it again" but
# "run FEWER cells MANY more times" -- at flows=4 and flows=8 the intervals
# overlap almost entirely and one cell is bimodal (0,916 against two values at
# 0,38), which is a dispersion problem and dispersion needs repetitions, not
# neighbours. Space separated, same `path|carriers|flows` spelling.
CELLS_SPEC="${CELLS_SPEC:-}"
if [ -n "$CELLS_SPEC" ]; then
    # shellcheck disable=SC2206
    CELLS=($CELLS_SPEC)
else
CELLS=(
    "relay|1|1"  "relay|2|1"  "relay|4|1"
    "relay|1|4"  "relay|2|4"  "relay|4|4"
    "relay|1|8"  "relay|2|8"  "relay|4|8"
    "direct|1|4" "direct|4|4"
    "direct|1|8" "direct|4|8"
)
fi
# A cell spelled wrong is a cell that silently never runs, so the shape is
# checked HERE rather than discovered as a missing row in the medians table.
for _c in "${CELLS[@]}"; do
    case "$_c" in
        relay\|[0-9]*\|[0-9]*|direct\|[0-9]*\|[0-9]*) ;;
        *) echo "bad cell spec '$_c' (want path|carriers|flows, path in relay|direct)"; exit 2 ;;
    esac
done

vpn_hdr "VPN --carriers on both paths -- $REPS reps, ${SECS}s per cell"
echo "  relay stripes per DATAGRAM across N substream pairs (DEC-7);"
echo "  direct PINS a flow to one carrier (BW-F2), so flows=1 there is a control."
echo "  every cell is a ratio against a bare control sampled in the same rep (V-9)."
echo

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
declare -A SAMP     # SAMP[cell|metric] = samples

run_cell() {
    local path="$1" car="$2" flows="$3" bare="$4"
    local key="$path/c$car/f$flows" mtu got extra=""
    [ "$path" = relay ] && extra="--relay-only"

    VPN_LINK_ID="${VPN_RUN_ID}c$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen --carriers "$car" $extra >/dev/null 2>&1
    sleep 3
    ws_up car connect --carriers "$car" $extra >/dev/null 2>&1
    if ! ws_ready car 60 >/dev/null; then
        printf '    %-18s FAILED(link never came up)\n' "$key"
        vpn_cleanup; sleep 3; return
    fi
    if [ "$(wait_path car "$path" 100)" != "$path" ]; then
        # The one mistake this campaign exists to avoid: a direct number that
        # was silently a relay number.
        printf '    %-18s FAILED(path is %s, not %s)\n' "$key" "$(ws_path car)" "$path"
        vpn_cleanup; sleep 3; return
    fi
    # MTU settle is an INPUT to every throughput measurement: measuring across
    # a 1350 -> 1288 -> 1414 walk blends two MSS values into one number.
    mtu="$(wait_mtu_settle car)"

    if [ "$(vm_iperf_server)" != 1 ]; then
        printf '    %-18s FAILED(no iperf3 server on the far end)\n' "$key"
        vpn_cleanup; sleep 3; return
    fi
    # `$B_PEER`, the FAR end. This read `$A_PEER` -- this workstation's own tun
    # address -- so every cell ran iperf3 against itself, found no server, and
    # returned the literal string `FAILED`. Every cell of every rep.
    got="$(tcp_mbps "$B_PEER" "$SECS" "$flows")"
    # AND THE GUARD MUST REJECT A NON-NUMBER, not just a zero. It matched
    # `0|0.0|""` only, so `FAILED` fell through to the scoring path below, where
    # `awk 'BEGIN{printf "%.3f", g/b}'` with g="FAILED" yields **0.000** -- and
    # that 0.000 was pushed into the `ratio` median as a sample. `med()` refused
    # the `FAILED` in the Mbit/s column and could not refuse the 0.000 beside
    # it, so the failure was invisible in exactly the column a reader compares.
    case "$got" in
        ''|*[!0-9.]*)
            printf '    %-18s FAILED(no throughput: %s) mtu=%s\n' "$key" "${got:-empty}" "$mtu"
            vpn_cleanup; sleep 3; return ;;
        0|0.0|0.00)
            printf '    %-18s FAILED(zero throughput) mtu=%s\n' "$key" "$mtu"
            vpn_cleanup; sleep 3; return ;;
    esac

    SAMP["$key|mbps"]+=" $got"
    if [ -n "$bare" ] && [ "$bare" != 0 ]; then
        local ratio; ratio="$(awk -v g="$got" -v b="$bare" 'BEGIN{printf "%.3f", g/b}')"
        SAMP["$key|ratio"]+=" $ratio"
        printf '    %-18s %-9s Mbit/s  %s of bare  mtu=%s\n' "$key" "$got" "$ratio" "$mtu"
    else
        printf '    %-18s %-9s Mbit/s  (no bare control this rep)  mtu=%s\n' "$key" "$got" "$mtu"
    fi
    vpn_cleanup; sleep 4
}

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    bare=""
    if [ "$(vm_iperf_server)" = 1 ]; then
        bare="$(tcp_mbps "$BORE_VM" "$SECS" 4)"
        echo "    bare control (P=4): ${bare:-FAILED} Mbit/s"
    else
        echo "    bare control: FAILED(no iperf3 server) -- this rep reports absolutes only"
    fi
    for cell in "${CELLS[@]}"; do
        IFS='|' read -r p c f <<<"$cell"
        run_cell "$p" "$c" "$f" "$bare"
    done
done

echo
echo "=== medians ==="
printf '  %-18s %-12s %s\n' cell 'Mbit/s' 'of bare'
for cell in "${CELLS[@]}"; do
    IFS='|' read -r p c f <<<"$cell"
    key="$p/c$c/f$f"
    printf '  %-18s %-12s %s\n' "$key" \
      "$(printf '%s\n' ${SAMP["$key|mbps"]:-}  | med)" \
      "$(printf '%s\n' ${SAMP["$key|ratio"]:-} | med)"
done

echo
echo "=== raw samples (V-11) ==="
for k in "${!SAMP[@]}"; do printf '  %-26s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== reading ==="
echo "  RELAY, flows=1: per-datagram round-robin means a single flow's packets"
echo "  do ride every carrier, so this row is the one place where carriers could"
echo "  help a single flow -- and also the one place reordering could hurt it."
echo "  DIRECT, flows=1 is absent on purpose: a pinned flow cannot use a second"
echo "  carrier, so measuring it would only re-measure carriers=1."
echo "  A cell that beats bare is a measurement error, not a result."
echo
scored=0
for cell in "${CELLS[@]}"; do
    IFS='|' read -r p c f <<<"$cell"
    [ -n "${SAMP["$p/c$c/f$f|mbps"]:-}" ] && scored=$((scored + 1))
done
if [ "$scored" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: not one cell produced a throughput sample."
    echo "  Nothing above is a measurement. Exiting non-zero so no resume marker"
    echo "  is written and the next run repeats this stage instead of skipping it."
    exit 2
fi

echo "DONE"
