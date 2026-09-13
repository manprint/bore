#!/usr/bin/env bash
# V9: the TUN transmit queue -- the dominant queue on the direct path.
#
# HOW THIS ONE WAS FOUND, AND WHY IT WAS MISSED TWICE
# ---------------------------------------------------
# Two buffer ladders (UDP socket send, QUIC datagram send) each moved the QUIC
# carrier's own rtt and left the TUNNEL rtt untouched: bounding one stage simply
# re-formed the queue one stage earlier. Bounding BOTH took the carrier rtt from
# 108-212 ms to 30-45 ms and STILL left the tunnel at 134-198 ms, which located
# the remaining queue upstream of quinn entirely.
#
# `tc -s qdisc show dev boreN` reports backlog 0 throughout, which looks like
# proof of no queue and is not. A TUN device has TWO queues in series: the qdisc
# (fq_codel here, already installed by the system default) and then the device's
# own skb queue, bounded by `txqueuelen`, which the application reads from. The
# qdisc only builds a backlog once the DEVICE queue is full, so a deep device
# queue makes the qdisc -- and every AQM property it was chosen for -- inert.
#
# The device queue is bounded in PACKETS, and this path has TUN offload enabled:
# `ip -s link` shows 603 MB carried in 17 827 entries, ~34 KB each. So the
# default `txqueuelen 500` is not 500 x 1414 B = 707 KB, it is up to 500 x 34 KB
# = ~17 MB, which at 375 Mbit/s is roughly 360 ms. That is the missing latency,
# and it is bore's to fix: bore creates this TUN and never sets the value.
#
# WHY THIS IS A LADDER AND NOT A PATCH
# ------------------------------------
# The first sweep showed the trade is sharp and NOT monotonic:
#     500 -> 255.6 Mbit/s, rtt avg 138.7 ms
#     128 -> 272.6 Mbit/s, rtt avg 128.2 ms
#      32 ->  91.5 Mbit/s, rtt avg  26.4 ms   <-- latency fixed, throughput gone
#       8 ->   5.6 Mbit/s, rtt avg  24.3 ms
# A value chosen by argument would have landed on 32 and cost two thirds of the
# bandwidth. The collapse is its own finding: below some depth the uplink task
# cannot keep the pipe full between reads, and the inner TCP reads the resulting
# tail drops as congestion.
#
# So each rung reports the qdisc's backlog and drop counters too, because those
# say WHICH queue is absorbing the traffic at that depth -- the difference
# between "the AQM is now working" and "the device is tail-dropping".
#
# Usage: [REPS=2] [SECS=12] [RUNGS="500 256 192 128 64"] vpn_txqueue.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-2}"
SECS="${SECS:-12}"
RUNGS="${RUNGS:-500 256 192 128 64}"

vpn_hdr "VPN TUN txqueuelen ladder (direct path), ${REPS} reps x ${SECS}s"
echo "  rungs (packets): $RUNGS      [500 = the kernel default bore currently leaves in place]"
echo "  note: with TUN offload each queue ENTRY is a GSO super-packet (~34 KB measured)"
echo

declare -A UP RA RX
for k in $RUNGS bare; do UP[$k]=""; RA[$k]=""; RX[$k]=""; done

VPN_LINK_ID="${VPN_RUN_ID}t$(date +%s%N | tail -c 5)"
VPN_WS_TAGS=(); VPN_VM_IDS=()

vm_up listen
sleep 3
ws_up tq connect
ws_ready tq 45 >/dev/null || { echo "link never came up"; vpn_cleanup; exit 1; }
[ "$(wait_path tq direct 75)" = direct ] || { echo "never reached direct"; vpn_cleanup; exit 1; }
# This stage holds ONE link across the whole ladder -- which is why its ten
# samples came back within 699.9-706.5 Mbit/s at rtt_min 30.6-31.3 while the
# stages that build a fresh link per rung produced a contaminated low group
# (see docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md section 9). Settling
# ONCE is therefore correct here; discarding the RESULT was not. If the single
# link never settles the entire ladder is uncomparable, so this aborts rather
# than annotating.
LADDER_MTU="$(wait_mtu_settle tq 24 90)" \
    || { echo "mtu never settled (last=$LADDER_MTU); the whole ladder would be uncomparable"; vpn_cleanup; exit 1; }
echo "  ladder mtu=$LADDER_MTU (one link held across every rung)"
IF="$(ws_log tq 4000 | grep -oE '(iface|resolved_name|tun)=[A-Za-z0-9]+' | grep -oE 'bore[0-9]+' | tail -1)"
[ "$(vm_iperf_server)" = 1 ] || { echo "no iperf3 server"; vpn_cleanup; exit 1; }
echo "  iface=$IF mtu=$(ws_mtu tq) qdisc=$(tc qdisc show dev "$IF" 2>/dev/null | awk '{print $2}' | head -1)"
echo

qstat() { tc -s qdisc show dev "$IF" 2>/dev/null | tr '\n' ' ' \
    | grep -oE 'dropped [0-9]+|backlog [0-9]+b [0-9]+p' | tr '\n' ' '; }

# ONE link for the whole ladder on purpose: txqueuelen is a property of the
# device, not of the session, so re-establishing the tunnel per rung would add
# a fresh punch, a fresh PMTU climb and a fresh congestion-control ramp to every
# comparison and confound the only variable under test.
for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    for q in $RUNGS; do
        sudo -n ip link set dev "$IF" txqueuelen "$q" 2>/dev/null || { echo "    $q: FAILED(cannot set txqueuelen)"; continue; }
        sleep 2
        local_before="$(qstat)"
        ( tcp_mbps "$B_PEER" "$SECS" 1 > "$WORK/.tq.$$" 2>/dev/null ) & LP=$!
        sleep 1
        rr="$(rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )))"
        wait $LP 2>/dev/null
        u="$(cat "$WORK/.tq.$$" 2>/dev/null)"; rm -f "$WORK/.tq.$$"
        ravg="$(echo "$rr" | awk '{print $2}')"; rmin="$(echo "$rr" | awk '{print $1}')"
        UP[$q]="${UP[$q]} ${u:-0}"; RA[$q]="${RA[$q]} ${ravg:-0}"; RM_[$q]="${rmin:-0}"
        printf "    txqueuelen %4s : up %8s Mbit/s   rtt avg %7s min %7s ms   qdisc %s\n" \
               "$q" "${u:-0}" "${ravg:-?}" "${rmin:-?}" "$(qstat)"
        sleep 2
    done
done

# Restore the kernel default before teardown so a later stage on this host does
# not silently inherit the last rung.
sudo -n ip link set dev "$IF" txqueuelen 500 2>/dev/null || true
vpn_cleanup

echo
med() { printf '%s\n' $1 | tr ' ' '\n' | grep -vE '^$' | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
echo "  medians:"
for q in $RUNGS; do
    awk -v q="$q" -v u="$(med "${UP[$q]}")" -v ra="$(med "${RA[$q]}")" 'BEGIN{
        printf "    txqueuelen %4s : up %8s Mbit/s   rtt avg %7s ms\n", q, u, ra }'
done
echo
echo "DONE"
