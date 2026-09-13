#!/usr/bin/env bash
# V3: latency and queueing, measured properly.
#
# THE CONTROL PROBLEM, AND HOW THIS SOLVES IT WITHOUT TOUCHING THE INFRASTRUCTURE
# ------------------------------------------------------------------------------
# "How much latency does the tunnel add?" needs the same path measured without
# the tunnel. That is not directly available here: the VM's security group
# admits only TCP/22 inbound, so there is no port on which to run a bare iperf3
# or a bare ping, and opening one would change the very infrastructure under
# measurement. Both endpoints are also behind stateful filters, so neither can
# start a flow to the other without a punch -- which is the tunnel.
#
# The control used instead is BETTER than a bare ping would have been: the
# direct path's own QUIC carrier continuously measures the OUTER path RTT and
# logs it (`direct carrier quic stats ... rtt_ms=`). That is the same path, the
# same 5-tuple and the same instant as the tunnel RTT it is being compared
# with, so the difference between the two is the tunnel's own contribution with
# every other variable held fixed by construction. A separate bare ping, even
# if one were reachable, would have been a different flow at a different time.
#
# The TCP handshake to port 22 is kept as a second, independent reference. It
# is roughly one round trip and is subject to the server's accept path, so it
# reads slightly high; it is here to confirm the order of magnitude, never to
# carry a conclusion on its own.
#
# UNDER LOAD, AND WHY BOTH DIRECTIONS SEPARATELY
# ----------------------------------------------
# A deep queue only shows up when it is full. The previous assessment deferred
# right-sizing the direct path's 8 MiB datagram send buffer, noting it
# "bufferbloats RTT (~116 ms vs a 20 ms link)". A send buffer bloats the
# direction it SENDS in, so measuring only one direction can exonerate a path
# that is badly bloated in the other. Download load and upload load are
# therefore separate samples.
#
# Usage: [PATHS="relay direct"] [SECS=12] vpn_lat.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

SECS="${SECS:-12}"
PATHS="${PATHS:-relay direct}"

fmt() { awk '{printf "min %6.2f  avg %6.2f  max %7.2f  mdev %6.2f", $1,$2,$3,$4}'; }

vpn_hdr "VPN latency and queueing, real path"
echo "  reference (TCP handshake to the test VM:22, median of 15): $(tcp_connect_ms "$BORE_VM" 22 15) ms"
echo

for want in $PATHS; do
    extra=""
    [ "$want" = relay ] && extra="--relay-only"
    VPN_LINK_ID="${VPN_RUN_ID}l$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen $extra
    sleep 3
    ws_up lat connect $extra
    ws_ready lat 45 >/dev/null || { echo "--- $want: link never came up"; vpn_cleanup; sleep 2; continue; }
    if [ "$want" = direct ]; then
        got="$(wait_path lat direct 75)"
        [ "$got" = direct ] || { echo "--- $want: stayed on $got"; vpn_cleanup; sleep 2; continue; }
    fi
    [ "$(vm_iperf_server)" = 1 ] || { echo "--- $want: no iperf3 server"; vpn_cleanup; sleep 2; continue; }

    echo "--- path=$want ---"

    # The outer-path control. Only the direct path has a QUIC carrier to ask;
    # the relay's outer transport is TCP through the server and has no
    # equivalent self-measurement, which is itself worth stating rather than
    # papering over with a number from a different source.
    if [ "$want" = direct ]; then
        outer="$(ws_log lat 4000 | grep -oE 'rtt_ms=[0-9.]+' | tail -3 | cut -d= -f2 | tr '\n' ' ')"
        echo "  outer QUIC path rtt (carrier self-report): ${outer}ms"
    else
        echo "  outer path rtt: not self-reported on the relay (TCP through the server)"
    fi

    echo "  tunnel rtt idle          : $(rtt_ms "$B_PEER" 40 | fmt) ms"

    # Load the DOWNLOAD direction, sample RTT throughout.
    ( tcp_mbps "$B_PEER" "$SECS" 1 -R >/dev/null 2>&1 ) & LP=$!
    sleep 1
    dl="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LP 2>/dev/null
    echo "  tunnel rtt, download load: $(echo "$dl" | fmt) ms"
    sleep 2

    # Load the UPLOAD direction.
    ( tcp_mbps "$B_PEER" "$SECS" 1 >/dev/null 2>&1 ) & LP=$!
    sleep 1
    ul="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
    wait $LP 2>/dev/null
    echo "  tunnel rtt, upload load  : $(echo "$ul" | fmt) ms"

    idle_avg="$(rtt_ms "$B_PEER" 20 | awk '{print $2}')"
    awk -v i="$idle_avg" -v d="$(echo "$dl" | awk '{print $2}')" -v u="$(echo "$ul" | awk '{print $2}')" 'BEGIN{
        if (i+0>0) printf "  bufferbloat: download x%.2f (+%.1f ms)   upload x%.2f (+%.1f ms)\n",
                          d/i, d-i, u/i, u-i }'

    echo "  path at stage end: $(ws_path lat)"
    vpn_cleanup
    sleep 3
    echo
done

echo "DONE"
