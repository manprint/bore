#!/usr/bin/env bash
# V7: the UDP SOCKET send-buffer ladder -- the knob that sets the standing queue.
#
# THE MEASUREMENT THAT MOTIVATED THIS LADDER
# ------------------------------------------
# 2026-09-12, workstation -> AWS eu-south-1, RTT sampled by TCP handshake to a
# port outside the tunnel so the tunnel cannot flatter itself:
#
#     idle                                     22 ms
#     BARE UDP pushing 375 Mbit/s              19 ms     <-- no queue at all
#     bore direct pushing the SAME 375 Mbit/s  190-320 ms
#
# Same link, same instant, same wire rate, 100x the queue. That rules out the
# access link, the WiFi radio (which logged +8 retries and +0 drops across
# 112 MB), the far end and the inner TCP: the queue is the sender's own.
#
# `DIRECT_UDP_SOCKET_SEND_BUFFER` is 16 MiB and is applied with SO_SNDBUFFORCE,
# which bypasses `net.core.wmem_max` precisely so it CAN be that large. 16 MiB
# at 375 Mbit/s is 350 ms of queue, which is the observed number. Stock iperf3
# runs on the default 208 KiB and shows none.
#
# WHY THE BIG BUFFER IS THERE, AND WHY THAT REASON DOES NOT APPLY TO SEND
# ----------------------------------------------------------------------
# P-13 established that an untuned UDP socket caps a congestion-controlled QUIC
# flow at roughly `buffer / RTT`, and moved the buffer call inside the endpoint
# constructors so no call site could forget it. That argument is about the
# RECEIVE buffer: bytes that arrive with nowhere to go are DROPPED, and the
# sender learns only via loss. A SEND buffer is the opposite quantity -- it is
# how much the sender may hand the qdisc before it is told to wait -- so making
# it large removes the backpressure that would have kept the queue short. The
# receive side is therefore NOT part of this ladder and must not be lowered
# with it.
#
# WHAT WOULD FALSIFY THE HYPOTHESIS
# ---------------------------------
# If goodput falls as the buffer shrinks with no latency won, the deep buffer is
# load-bearing and the default stays. That is exactly what happened to the QUIC
# DATAGRAM send-buffer ladder (`vpn_sndbuf.sh`), whose hypothesis was equally
# plausible and was falsified -- which is why this is a ladder and not a patch.
#
# Each rung reports goodput AND loaded RTT, because a knob that trades one for
# the other must be priced, not argued.
#
# WALKING A DIFFERENT KNOB
# -----------------------
# `KNOB` names the environment variable each rung sets, so the same ladder --
# same interleaving, same bare control, same paired latency sample -- prices any
# queue on this path. The two that matter here are the UDP SOCKET send buffer
# (BORE_DIRECT_UDP_SEND_BUF, the default) and the QUIC DATAGRAM send buffer
# (BORE_DIRECT_DGRAM_SEND_BUF), which sit in series: the first ladder moved the
# carrier's own rtt from 89 ms to 35 ms and left the TUNNEL rtt untouched, which
# is what says the dominant queue is the other one.
#
# Usage: [REPS=2] [SECS=15] [KNOB=BORE_DIRECT_UDP_SEND_BUF] [RUNGS="..."] vpn_udpbuf.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-2}"
SECS="${SECS:-15}"
RUNGS="${RUNGS:-16777216 4194304 1048576 262144}"
KNOB="${KNOB:-BORE_DIRECT_UDP_SEND_BUF}"

vpn_hdr "VPN UDP socket send-buffer ladder (direct path), ${REPS} reps x ${SECS}s"
echo "  knob : $KNOB"
echo "  rungs (bytes): $RUNGS"
echo "  per rung: inner TCP upload, tunnel RTT under that load, and the QUIC carrier's own rtt"
echo

declare -A UP RA RM QR
for k in $RUNGS bare; do UP[$k]=""; RA[$k]=""; RM[$k]=""; QR[$k]=""; done

bare_arm() {
    [ "$(vm_iperf_server)" = 1 ] || { echo "    bare: FAILED(no iperf3 server)"; return; }
    local u; u="$(tcp_mbps "$BORE_VM" "$SECS" 1)"
    UP[bare]="${UP[bare]} $u"
    printf "    %-10s up %8s Mbit/s\n" bare "$u"
}

rung_arm() { # <bytes>
    local b="$1"
    VPN_ENV="$KNOB=$b"
    VPN_LINK_ID="${VPN_RUN_ID}u$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen
    sleep 3
    ws_up ub connect
    if ! ws_ready ub 45 >/dev/null; then
        echo "    $b: FAILED(link never came up)"; vpn_cleanup; sleep 2; return
    fi
    if [ "$(wait_path ub direct 75)" != direct ]; then
        echo "    $b: FAILED(never reached direct -- a relay number here would be a lie)"
        vpn_cleanup; sleep 2; return
    fi
    # An arm whose TUN MTU never stopped moving is NOT a sample: inner goodput is
    # proportional to MSS (Mathis), and a fresh link climbs 1350 -> 1288 -> 1414
    # over ~25 s. `wait_mtu_settle` returns 1 when it gives up, and this call
    # used to discard the value AND the status -- so a link that never settled
    # was measured anyway and nothing said so. Measured consequence, wired
    # 2026-09-12: `vpn_cc_matrix` and `vpn_udpbuf` each produced samples ~4 %
    # low whose `rtt min` was 17-22 ms against 31 ms for every settled sample,
    # two disjoint groups splitting ACROSS the arms rather than along them.
    ARM_MTU="$(wait_mtu_settle ub 24 90)" || {
        echo "    $b: FAILED(mtu never settled, last=$ARM_MTU) -- not averaged in"
        vpn_cleanup; sleep 2; return; }
    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "    $b: FAILED(no iperf3 server)"; vpn_cleanup; sleep 2; return
    fi

    # Read the buffer the kernel actually granted. SO_SNDBUF reports ~2x the
    # request, and a rung that was silently clamped would otherwise be averaged
    # in as though it had been honoured.
    local granted
    # The field is `effective_send`, NOT `actual_send`: the latter is a
    # Debug-printed Option on a different log statement and this grep matched
    # nothing, so the column read "?" for the whole ladder -- i.e. the one check
    # that says whether the rungs are distinct at all was silently absent.
    #
    # It matters more than a cosmetic column. `net.core.wmem_max` on this host
    # is 4 MiB, so a 16 MiB rung is only a distinct rung if bore's
    # SO_SNDBUFFORCE path (CAP_NET_ADMIN, which the VPN has) actually bypassed
    # the clamp; if it had fallen back, the top rungs would be the SAME buffer
    # and their agreement would be an artefact rather than a result.
    #
    # Linux reports SO_SNDBUF as TWICE the requested value, so the expected
    # reading is 2x the rung. That doubling is printed beside it rather than
    # divided out, because silently halving a kernel number is how a harness
    # starts disagreeing with `ss`.
    granted="$(ws_log ub 4000 | grep -oE 'effective_send=[0-9]+' | tail -1 | grep -oE '[0-9]+')"
    local forced; forced="$(ws_log ub 4000 | grep -oE 'forced=[a-z]+' | tail -1 | cut -d= -f2)"
    local want=$(( b * 2 ))
    if [ -n "$granted" ] && [ "$granted" != "$want" ]; then
        granted="$granted(WANTED $want -- CLAMPED)"
    elif [ -n "$granted" ]; then
        granted="$granted(=2x$b, forced=${forced:-?})"
    fi

    # Throughput and latency in the SAME transfer: the relationship between them
    # is the whole point of the rung.
    ( tcp_mbps "$B_PEER" "$SECS" 1 > "$WORK/.ub.$$" 2>/dev/null ) & LP=$!
    sleep 1
    local r; r="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LP 2>/dev/null
    local u; u="$(cat "$WORK/.ub.$$" 2>/dev/null)"; rm -f "$WORK/.ub.$$"
    local ravg rmin
    rmin="$(echo "$r" | awk '{print $1}')"; ravg="$(echo "$r" | awk '{print $2}')"

    # The carrier's own view of the path, which is where the 190-320 ms showed up.
    local q
    q="$(ws_log ub 6000 | grep 'direct carrier quic stats' | tail -3 \
         | grep -oE 'rtt_ms=[0-9.]+' | cut -d= -f2 \
         | awk '{s+=$1;n++} END{if(n) printf "%.0f", s/n; else printf "?"}')"

    UP[$b]="${UP[$b]} ${u:-0}"; RA[$b]="${RA[$b]} ${ravg:-0}"
    RM[$b]="${RM[$b]} ${rmin:-0}"; QR[$b]="${QR[$b]} ${q:-0}"
    printf "    %-10s up %8s Mbit/s   tunnel rtt avg %6s min %6s ms   quic rtt %4s ms   granted %s\n" \
           "$b" "${u:-0}" "${ravg:-?}" "${rmin:-?}" "${q:-?}" "${granted:-?}"

    vpn_cleanup
    sleep 3
}

for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    bare_arm
    for b in $RUNGS; do rung_arm "$b"; done
done

echo
med() { printf '%s\n' $1 | tr ' ' '\n' | grep -vE '^$' | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
echo "  medians:"
bm="$(med "${UP[bare]}")"
printf "    %-12s up %8s Mbit/s\n" "bare" "$bm"
for b in $RUNGS; do
    awk -v b="$b" -v u="$(med "${UP[$b]}")" -v ra="$(med "${RA[$b]}")" \
        -v rmn="$(med "${RM[$b]}")" -v q="$(med "${QR[$b]}")" -v base="$bm" 'BEGIN{
        pct = (base+0>0 && u+0>0) ? 100*u/base : 0;
        printf "    %-12s up %8s Mbit/s (%5.1f%% of bare)   rtt avg %7s min %7s   quic rtt %5s ms\n",
               b, u, pct, ra, rmn, q }'
done
echo
echo "DONE"
