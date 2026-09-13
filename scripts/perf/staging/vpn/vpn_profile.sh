#!/usr/bin/env bash
# V2: the per-path profile -- flow ladder, latency idle, latency UNDER LOAD,
# and UDP loss -- run over ONE link per path.
#
# WHY ONE LINK PER PATH RATHER THAN ONE PER MEASUREMENT
# ----------------------------------------------------
# Bringing a VPN link up costs ~25 s (register, pair, TUN, punch, upgrade), and
# a link's direct path is re-punched on every fresh link, so a per-measurement
# setup would spend most of the campaign establishing links and would sample a
# different punched 5-tuple for every data point. One link per path keeps the
# transport fixed across the whole ladder, which is what makes the ladder's
# steps comparable to each other.
#
# THE FOUR STAGES, AND THE QUESTION EACH ANSWERS
# ----------------------------------------------
# 1. FLOW LADDER (P = 1, 2, 4, 8). The prior assessment's F1 concluded that the
#    ~40 % gap to line rate is the single inner TCP flow hitting the Mathis
#    bound on the path's loss, not a defect in bore. That is a testable claim:
#    if it is right, throughput scales with flow count and the per-flow figure
#    falls; if the tunnel itself were the limit, the aggregate would be flat.
#    This stage is the real-path test of that conclusion.
#
# 2. LATENCY IDLE. The tunnel's RTT against the bare path's. ICMP to the VM is
#    dropped by its security group, so the bare reference is a TCP handshake to
#    the one port the group does admit (22) -- a full round trip, measured the
#    same way for every sample, which is what makes it a fair reference even
#    though it is not ICMP.
#
# 3. LATENCY UNDER LOAD -- the stage this campaign was most owed. The direct
#    path's datagram send buffer is 8 MiB, and the previous assessment deferred
#    right-sizing it with the note that it "bufferbloats RTT (~116 ms vs a 20 ms
#    link) and makes backpressure engage late". A buffer that deep is invisible
#    on an idle link and dominates a loaded one, so idle RTT cannot see it and
#    only this stage can. Measured as ping RTT sampled WHILE a bulk transfer
#    saturates the same tunnel.
#
# 4. UDP LOSS. Inner TCP hides path loss behind retransmission; a UDP flow does
#    not. This is the only stage that prices what the inner TCP in stage 1 is
#    actually up against, and it is what makes the Mathis verdict falsifiable
#    rather than merely plausible.
#
# Usage: [PATHS="relay direct"] [SECS=10] [LADDER="1 2 4 8"] vpn_profile.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

SECS="${SECS:-10}"
LADDER="${LADDER:-1 2 4 8}"
PATHS="${PATHS:-relay direct}"
UDP_RATE="${UDP_RATE:-300M}"

vpn_hdr "VPN per-path profile, real path, ${SECS}s per point"
echo "  workstation <-> test-vm via the staging server, MTU ${BENCH_MTU} pinned"
echo

# The bare-path latency reference, taken once: the underlying route does not
# change between arms, and sampling it per-arm would add a confounder rather
# than remove one.
BARE="$(tcp_connect_ms "$BORE_VM" 22 15)"
echo "  bare path (TCP handshake to the test VM:22, median of 15): ${BARE} ms"
echo

for want in $PATHS; do
    extra=""
    [ "$want" = relay ] && extra="--relay-only"
    VPN_LINK_ID="${VPN_RUN_ID}p$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen $extra
    sleep 3
    ws_up prof connect $extra
    if ! ws_ready prof 45 >/dev/null; then
        echo "--- $want: link never came up, stage skipped"; vpn_cleanup; sleep 2; continue
    fi
    if [ "$want" = direct ]; then
        got="$(wait_path prof direct 75)"
        if [ "$got" != direct ]; then
            echo "--- $want: link stayed on $got, stage skipped (never reported as direct)"
            vpn_cleanup; sleep 2; continue
        fi
    fi
    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "--- $want: no iperf3 server, stage skipped"; vpn_cleanup; sleep 2; continue
    fi

    # This stage holds ONE link per path and then walks a flow ladder on it, so
    # the MTU climb happens DURING the ladder unless it is waited out first --
    # and the rungs are measured in sequence, which puts the 1288 trough
    # (roughly t+5 s to t+25 s of a fresh link) squarely on whichever rung
    # happens to be second.
    #
    # MEASURED, wired 2026-09-12, before this wait existed: the direct ladder
    # read 820.98 / **536.85** / 872.33 / 874.08 Mbit/s down at flows
    # 1 / 2 / 4 / 8. The dip is not a concurrency effect -- flows 4 and 8 are
    # the fastest rungs -- it is the second rung sitting in the trough. The
    # shortfall (38 %) is larger than the MSS ratio alone predicts (9 %),
    # consistent with the MTU also CHANGING mid-transfer, which costs
    # TooLarge-dropped packets on top of the smaller segments.
    MTU="$(wait_mtu_settle prof 24 90)" \
        || { echo "--- $want: mtu never settled (last=$MTU), stage skipped -- a ladder"
             echo "    measured across an MTU change is a blend of two configurations"
             vpn_cleanup; sleep 2; continue; }

    echo "--- path=$want (mtu $MTU) ---"

    # 1. flow ladder, both directions
    for p in $LADDER; do
        d="$(tcp_mbps "$B_PEER" "$SECS" "$p" -R)"
        sleep 1
        u="$(tcp_mbps "$B_PEER" "$SECS" "$p")"
        # Per-flow throughput is what separates "the tunnel is the limit" from
        # "each flow is the limit": a flat aggregate with a falling per-flow
        # figure is the Mathis shape.
        awk -v p="$p" -v d="$d" -v u="$u" 'BEGIN{
            printf "  flows=%-2s  down %8.2f Mbit/s (%7.2f per flow)   up %8.2f Mbit/s (%7.2f per flow)\n",
                   p, d, d/p, u, u/p }'
        sleep 1
    done

    # 2. latency idle
    idle="$(rtt_ms "$B_PEER" 25)"
    echo "  rtt idle        : $(echo "$idle" | awk '{printf "min %.2f avg %.2f max %.2f mdev %.2f", $1,$2,$3,$4}') ms"

    # 3. latency under load. The ping runs for the whole transfer, so the
    #    reported max is the queue's real depth, not a lucky sample of it.
    ( tcp_mbps "$B_PEER" "$SECS" 1 -R >/dev/null 2>&1 ) &
    LOADPID=$!
    sleep 1
    loaded="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LOADPID 2>/dev/null
    echo "  rtt under load  : $(echo "$loaded" | awk '{printf "min %.2f avg %.2f max %.2f mdev %.2f", $1,$2,$3,$4}') ms"
    awk -v i="$(echo "$idle" | awk '{print $2}')" -v l="$(echo "$loaded" | awk '{print $2}')" 'BEGIN{
        if (i+0>0) printf "  bufferbloat     : avg RTT x%.2f under load (+%.1f ms)\n", l/i, l-i }'

    # 4. UDP loss
    read -r umb uloss <<<"$(udp_loss "$B_PEER" "$SECS" "$UDP_RATE")"
    echo "  udp @ $UDP_RATE     : ${umb} Mbit/s delivered, ${uloss}% lost"

    echo "  path at stage end: $(ws_path prof)"
    vpn_cleanup
    sleep 3
    echo
done

echo "DONE"
