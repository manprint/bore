#!/usr/bin/env bash
# V4: the datagram send-buffer ladder -- the experiment that sets the default.
#
# WHAT IS BEING DECIDED
# ---------------------
# The direct path's QUIC datagram send buffer used to be a hardcoded 8 MiB. It
# is not an allocation but the VPN uplink's QUEUE: the 1:1 uplink sends through
# `send_batch_wait`, which AWAITS room rather than letting quinn drop the oldest
# queued datagram, so backpressure reaches the TUN -- and through it the inner
# TCP senders -- exactly when this buffer fills. Its depth is therefore a
# latency policy.
#
# WHY A LADDER AND NOT AN ARGUMENT
# --------------------------------
# The case for shrinking it is easy to make on paper (8 MiB is ~5x the measured
# BDP of this path) and that is precisely why it must be measured: the same
# reasoning would justify shrinking it until throughput collapses, and nothing
# in the argument says where to stop. The ladder prices both quantities that
# matter at each rung -- upload throughput AND loaded RTT -- so the default is
# chosen against evidence of the trade, not against the direction of it.
#
# The ladder is INTERLEAVED, not run as one sweep per rung. This workstation
# drifts 14 % on identical code and the far end is a burstable instance; a
# per-rung sweep would attribute that drift to the knob. Each repetition walks
# every rung, and the quoted figure per rung is the median across repetitions.
#
# The BARE path is measured in the same repetition as a control, because the
# question "did the buffer cost throughput" is only answerable against what the
# path could do at that moment.
#
# Usage: [REPS=3] [SECS=10] [RUNGS="8388608 2097152 524288"] vpn_sndbuf.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
SECS="${SECS:-10}"
RUNGS="${RUNGS:-8388608 2097152 524288}"

vpn_hdr "VPN datagram send-buffer ladder (direct path), ${REPS} reps x ${SECS}s"
echo "  rungs (bytes): $RUNGS"
echo "  each rung: upload Mbit/s, and tunnel RTT sampled THROUGHOUT that upload"
echo

declare -A UP RT
for k in $RUNGS bare; do UP[$k]=""; RT[$k]=""; done

bare_arm() {
    [ "$(vm_iperf_server)" = 1 ] || { echo "  bare: no iperf3 server"; return; }
    local u; u="$(tcp_mbps "$BORE_VM" "$SECS" 1)"
    UP[bare]="${UP[bare]} $u"
    echo "    bare            up ${u} Mbit/s"
}

rung_arm() { # <bytes>
    local b="$1"
    VPN_ENV="BORE_DIRECT_DGRAM_SEND_BUF=$b"
    VPN_LINK_ID="${VPN_RUN_ID}s$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen
    sleep 3
    ws_up snd connect
    if ! ws_ready snd 45 >/dev/null; then
        echo "    $b: link never came up"; vpn_cleanup; sleep 2; return
    fi
    if [ "$(wait_path snd direct 75)" != direct ]; then
        echo "    $b: never reached direct, rung skipped (a relay number here would be a lie)"
        vpn_cleanup; sleep 2; return
    fi
    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "    $b: no iperf3 server"; vpn_cleanup; sleep 2; return
    fi

    # Throughput and latency are sampled in the SAME transfer. Measuring them
    # in two transfers would let the path move between them, and the whole
    # point of this rung is the relationship BETWEEN the two numbers.
    ( tcp_mbps "$B_PEER" "$SECS" 1 > "$WORK/.up.$$" 2>/dev/null ) & LP=$!
    sleep 1
    local r; r="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LP 2>/dev/null
    local u; u="$(cat "$WORK/.up.$$" 2>/dev/null)"; rm -f "$WORK/.up.$$"
    local ravg rmin
    ravg="$(echo "$r" | awk '{print $2}')"; rmin="$(echo "$r" | awk '{print $1}')"

    UP[$b]="${UP[$b]} ${u:-0}"
    RT[$b]="${RT[$b]} ${ravg:-0}"
    # The MINIMUM under load is reported beside the average on purpose: a
    # minimum well above the idle RTT is a standing queue, which is what
    # distinguishes bufferbloat from ordinary jitter.
    printf "    %-9s bytes  up %8s Mbit/s   rtt avg %6s  min %6s ms\n" "$b" "${u:-0}" "${ravg:-?}" "${rmin:-?}"

    vpn_cleanup
    sleep 3
}

for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    bare_arm
    for b in $RUNGS; do rung_arm "$b"; done
done

echo
med() { printf '%s\n' $1 | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
echo "  medians:"
bm="$(med "${UP[bare]}")"
printf "    %-14s up %8s Mbit/s\n" "bare path" "$bm"
for b in $RUNGS; do
    um="$(med "${UP[$b]}")"; rm_="$(med "${RT[$b]}")"
    awk -v b="$b" -v u="$um" -v r="$rm_" -v base="$bm" 'BEGIN{
        pct = (base+0>0 && u+0>0) ? 100*u/base : 0;
        printf "    %-14s up %8s Mbit/s (%5.1f%% of bare)   rtt under load %6s ms\n",
               b " bytes", u, pct, r }'
done
echo
echo "DONE"
