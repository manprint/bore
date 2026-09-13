#!/usr/bin/env bash
# V8: congestion controller and GSO on the direct path.
#
# WHAT THIS IS FOR
# ----------------
# The direct path delivers ~250 Mbit/s of inner TCP where the bare path
# delivers ~400 and the tunnel's own wire capacity is ~375 (measured with inner
# UDP). The shortfall is not CPU (12% of one core, busiest thread 3%), not loss
# (`lost_pct=0.00`), and not either send buffer (both ladders walked, neither
# moved goodput). What IS anomalous is the congestion window: 13-15 MiB against
# a ~940 KiB bandwidth-delay product, with no loss to check it.
#
# That is the signature of a LOSSLESS bottleneck. The wireless driver queues
# rather than drops, so a controller that grows until it sees loss never stops,
# and the surplus is delivered as bursts -- which is exactly what the latency
# sample shows: minimum RTT under load stays at the idle value (~20 ms) while
# the average reaches 150 ms. A standing queue would raise the MINIMUM; bursts
# raise only the average. That distinction is why this stage exists and why it
# is not another buffer ladder.
#
# The inner TCP flow is then window-limited by the inflated RTT:
# `net.ipv4.tcp_wmem` max is 4 MiB here, and 4 MiB / 145 ms = 231 Mbit/s, which
# is what we measure. So latency IS the throughput bug on this path, and any arm
# that lowers RTT without losing wire capacity should raise goodput.
#
# ARMS
#   bare      no tunnel, the control, sampled every repetition
#   bbr       the shipped default
#   cubic     loss-based; on a lossless bottleneck expected to be no better,
#             which is worth knowing rather than assuming
#   newreno   the least aggressive reference
#   nogso     BBR with segmentation offload off -- prices the burst hypothesis
#             directly, at whatever CPU it costs
#
# Usage: [REPS=2] [SECS=15] [ARMS="bare bbr cubic newreno nogso"] vpn_cc_matrix.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

# Two repetitions cannot rank seven arms: with one bad sample anywhere, the
# per-arm median IS that sample. Raised to 4 after the 2026-09-12 wired run
# produced two disjoint groups that split across the arms rather than along
# them. Cheap here -- an arm is ~70 s.
REPS="${REPS:-4}"
SECS="${SECS:-15}"
ARMS="${ARMS:-bare bbr cubic newreno nogso both1m both512 both256}"

arm_env() { # <arm> -> the environment that defines it
    case "$1" in
        bbr)     echo "BORE_DIRECT_QUIC_CC=bbr" ;;
        cubic)   echo "BORE_DIRECT_QUIC_CC=cubic" ;;
        newreno) echo "BORE_DIRECT_QUIC_CC=newreno" ;;
        nogso)   echo "BORE_DIRECT_QUIC_CC=bbr BORE_DIRECT_QUIC_GSO=0" ;;
        # Both send stages bounded TOGETHER. Walking them one at a time showed
        # latency is conserved: shrinking the socket buffer alone moved the
        # carrier's own rtt from 89 ms to 35 ms while the TUNNEL rtt stayed at
        # 134 ms, because the queue simply re-formed one stage earlier. Only a
        # bound on every stage in series can shorten the path end to end.
        both1m)  echo "BORE_DIRECT_UDP_SEND_BUF=1048576 BORE_DIRECT_DGRAM_SEND_BUF=1048576" ;;
        both512) echo "BORE_DIRECT_UDP_SEND_BUF=524288 BORE_DIRECT_DGRAM_SEND_BUF=524288" ;;
        both256) echo "BORE_DIRECT_UDP_SEND_BUF=262144 BORE_DIRECT_DGRAM_SEND_BUF=262144" ;;
        *)       echo "" ;;
    esac
}

vpn_hdr "VPN direct path: congestion controller / GSO matrix, ${REPS} reps x ${SECS}s"
echo "  arms: $ARMS"
echo "  goodput = inner TCP upload; rtt sampled THROUGHOUT that same transfer"
echo

declare -A UP RA RM QC
for k in $ARMS; do UP[$k]=""; RA[$k]=""; RM[$k]=""; QC[$k]=""; done

bare_arm() {
    [ "$(vm_iperf_server)" = 1 ] || { echo "    bare: FAILED(no iperf3 server)"; return; }
    local u; u="$(tcp_mbps "$BORE_VM" "$SECS" 1)"
    UP[bare]="${UP[bare]} $u"
    printf "    %-9s up %8s Mbit/s\n" bare "$u"
}

one_arm() { # <arm>
    local a="$1"
    VPN_ENV="$(arm_env "$a")"
    VPN_LINK_ID="${VPN_RUN_ID}c$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen
    sleep 3
    ws_up cc connect
    if ! ws_ready cc 45 >/dev/null; then
        echo "    $a: FAILED(link never came up)"; vpn_cleanup; sleep 2; return
    fi
    if [ "$(wait_path cc direct 75)" != direct ]; then
        echo "    $a: FAILED(never reached direct)"; vpn_cleanup; sleep 2; return
    fi
    # The MTU is an INPUT to this measurement, not a detail: inner goodput is
    # proportional to MSS (Mathis), and the TUN climbs 1350 -> 1288 -> 1414 over
    # the first ~25 s of a fresh link. This stage builds a fresh link per arm,
    # so an arm measured before the climb finishes runs at an MSS up to 9 %
    # short of the one every other arm used -- and the difference lands on
    # whichever arm happened to be slow to settle, which reads as a property of
    # the congestion controller.
    #
    # It used to be `wait_mtu_settle cc >/dev/null 2>&1`, discarding the value
    # AND the exit status; `wait_mtu_settle` returns 1 when it times out without
    # the MTU ever holding still. So a link that never settled was measured
    # anyway and nothing recorded it.
    #
    # MEASURED CONSEQUENCE (2026-09-12, wired, 2 reps x 7 arms): the 14 samples
    # split into 11 at 700-707 Mbit/s with rtt_min 30.7-31.3 ms and 3 at
    # 668-680 Mbit/s with rtt_min 16.8-21.9 ms -- two DISJOINT groups, and the
    # split falls across the arms rather than along them. Per-arm medians
    # computed over that are an artefact of which arm caught a low sample.
    local mtu rc_mtu
    mtu="$(wait_mtu_settle cc 24 90)"; rc_mtu=$?
    if [ "$rc_mtu" != 0 ]; then
        echo "    $a: FAILED(mtu never settled, last=$mtu) -- not averaged in"
        vpn_cleanup; sleep 2; return
    fi
    ARM_MTU="$mtu"
    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "    $a: FAILED(no iperf3 server)"; vpn_cleanup; sleep 2; return
    fi

    ( tcp_mbps "$B_PEER" "$SECS" 1 > "$WORK/.cc.$$" 2>/dev/null ) & LP=$!
    sleep 1
    local r; r="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LP 2>/dev/null
    local u; u="$(cat "$WORK/.cc.$$" 2>/dev/null)"; rm -f "$WORK/.cc.$$"
    local rmin ravg
    rmin="$(echo "$r" | awk '{print $1}')"; ravg="$(echo "$r" | awk '{print $2}')"

    # The congestion window is the quantity under suspicion, so it is reported
    # rather than inferred: an arm that fixes the latency by shrinking cwnd to
    # about one BDP looks completely different from one that merely got lucky.
    local cw q
    cw="$(ws_log cc 6000 | grep 'direct carrier quic stats' | tail -3 \
          | grep -oE 'cwnd=[0-9]+' | cut -d= -f2 \
          | awk '{s+=$1;n++} END{if(n) printf "%.0f", s/n; else printf "0"}')"
    q="$(ws_log cc 6000 | grep 'direct carrier quic stats' | tail -3 \
         | grep -oE 'rtt_ms=[0-9.]+' | cut -d= -f2 \
         | awk '{s+=$1;n++} END{if(n) printf "%.0f", s/n; else printf "0"}')"

    UP[$a]="${UP[$a]} ${u:-0}"; RA[$a]="${RA[$a]} ${ravg:-0}"
    RM[$a]="${RM[$a]} ${rmin:-0}"; QC[$a]="${QC[$a]} ${cw:-0}"
    # MTU printed on every line: it is an input to the number beside it, and a
    # column that is constant is the cheapest possible proof that it was.
    printf "    %-9s up %8s Mbit/s   rtt avg %7s min %7s ms   quic rtt %4s ms   mtu %s  cwnd %s\n" \
           "$a" "${u:-0}" "${ravg:-?}" "${rmin:-?}" "${q:-?}" "${ARM_MTU:-?}" "${cw:-?}"

    vpn_cleanup
    sleep 3
}

for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    for a in $ARMS; do
        if [ "$a" = bare ]; then bare_arm; else one_arm "$a"; fi
    done
done

echo
med() { printf '%s\n' $1 | tr ' ' '\n' | grep -vE '^$' | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
bm="$(med "${UP[bare]}")"
echo "  medians (bare = $bm Mbit/s):"
for a in $ARMS; do
    [ "$a" = bare ] && continue
    awk -v a="$a" -v u="$(med "${UP[$a]}")" -v ra="$(med "${RA[$a]}")" \
        -v rmn="$(med "${RM[$a]}")" -v cw="$(med "${QC[$a]}")" -v base="$bm" 'BEGIN{
        pct = (base+0>0 && u+0>0) ? 100*u/base : 0;
        printf "    %-9s up %8s Mbit/s (%5.1f%% of bare)   rtt avg %7s min %7s   cwnd %10s\n",
               a, u, pct, ra, rmn, cw }'
done
echo
echo "DONE"
