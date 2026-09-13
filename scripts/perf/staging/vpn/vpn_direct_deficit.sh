#!/usr/bin/env bash
# V6: WHY the direct path is slower than the relay on upload.
#
# THE FINDING THIS STAGE EXISTS TO EXPLAIN
# ----------------------------------------
# Measured 2026-09-12, workstation -> AWS eu-south-1, 3 interleaved reps against
# a bare control sampled in the same repetition:
#
#     upload    bare 414.5    relay 415.7 (100.3%)    direct 259.7 (62.7%)
#     download  bare 150.7    relay 132.1  (87.6%)    direct 138.7 (92.0%)
#
# The relay REACHES the bare path on upload. The direct path loses 37% of it.
# That exonerates every explanation that applies to both transports -- the
# access link, the AEAD seal, the TUN, the inner TCP, the far end -- because the
# relay carries all of them and pays nothing. Whatever this is, it is specific
# to the QUIC datagram path.
#
# This is also NOT new. The same shape has appeared in every campaign in this
# repository: P-13 found the shared server endpoint running on an unconfigured
# 208 KiB socket, where the direct arm took 3.885 GiB to deliver 2.279 and ran
# at half the relay's goodput; the secret campaign found the direct path billing
# the endpoints several times the relay's CPU per delivered GiB. A deficit that
# recurs across four independent subsystems is a property of the transport, not
# four coincidences, so this stage measures the transport rather than the VPN.
#
# WHAT IS SAMPLED, AND WHY EACH ONE DISCRIMINATES
# -----------------------------------------------
# Every quantity below is a DELTA across the transfer itself, and every arm is
# run in the same repetition as the others, because this workstation drifts 14%
# on identical code.
#
#   goodput        The number under investigation.
#   cpu_s_per_gib  Per PROCESS, both ends. The established currency of this
#                  repository's campaigns: a path that is CPU-bound bills more
#                  per delivered GiB, and the ratio is immune to the link rate
#                  moving underneath the measurement.
#   busiest thread The discriminator `cpu_s_per_gib` cannot provide. The VPN
#                  uplink is ONE task; if that single thread is pegged, total
#                  process CPU can look modest on a 4-core box while the path is
#                  hard-limited. A thread at ~100% of one core IS the answer.
#   lost_pct/cwnd  Straight from `direct carrier quic stats (5s delta)`, which
#                  the product already logs. Loss and a small congestion window
#                  are the two ways a QUIC flow underruns a link that a TCP flow
#                  on the same path saturates.
#   buffer_drop    App datagrams submitted minus datagrams framed on the wire.
#                  quinn silently drops the OLDEST queued datagram when the send
#                  buffer is full, and the tunnelled TCP reads that as loss.
#   udp errors     The kernel's own view (`nstat`): SndbufErrors is what a
#                  userspace sender never sees and never reports.
#   wire/goodput   UDP bytes actually put on the wire divided by bytes
#                  delivered. This is the ratio that convicted P-13 (1.78x) and
#                  it separates "sent too much" from "sent too slowly" -- two
#                  faults with identical goodput and opposite fixes.
#
# Usage: [REPS=2] [SECS=20] [ARMS="bare relay direct"] vpn_direct_deficit.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-2}"
SECS="${SECS:-20}"
ARMS="${ARMS:-bare relay direct}"
RUNDIR=/run/bore-vpn-bench

vpn_hdr "VPN direct-path deficit teardown, ${REPS} reps x ${SECS}s"
echo "  arms: $ARMS   (upload = workstation -> VM, the direction with the deficit)"
echo

# --- CPU accounting ---------------------------------------------------------
TICK="$(getconf CLK_TCK 2>/dev/null || echo 100)"

# Process CPU (utime+stime) in ticks for a local pid, 0 when gone.
proc_ticks() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0; }

# The busiest THREAD's ticks. /proc/<pid>/task/*/stat is world-readable even for
# a root-owned process, so this needs no privilege of its own.
busiest_thread_ticks() {
    local p="$1" best=0 t
    for t in /proc/"$p"/task/*/stat; do
        [ -r "$t" ] || continue
        local v; v="$(awk '{print $14+$15}' "$t" 2>/dev/null || echo 0)"
        [ "${v:-0}" -gt "$best" ] && best="$v"
    done
    echo "$best"
}

ws_pid() { cat "$RUNDIR/$1.pid" 2>/dev/null || echo ""; }

# nstat counters, as "name value" pairs. -z keeps history so this is a pure read.
udp_counters() { nstat -az 2>/dev/null | awk '/^Udp(SndbufErrors|RcvbufErrors|InErrors|OutDatagrams|InDatagrams)/{print $1"="$2}' | tr '\n' ' '; }
ctr() { echo "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2}'; }

# --- the quic stats the product already logs --------------------------------
# Counted, not timestamped: the lines arrive every 5 s, so the ones that appeared
# between the two counts are exactly the ones covering the transfer.
quic_lines_count() { ws_log "$1" 6000 | grep -c 'direct carrier quic stats' 2>/dev/null || echo 0; }
quic_window() { # <tag> <n_new>
    local n="$2"
    [ "${n:-0}" -gt 0 ] || { echo ""; return; }
    ws_log "$1" 6000 | grep 'direct carrier quic stats' | tail -n "$n"
}

summarize_quic() { # reads lines on stdin
    awk '
        { for (i=1;i<=NF;i++) { split($i,kv,"="); v[kv[1]]=kv[2] }
          n++
          lost += v["lost_pkts_d"]; sent += v["sent_packets_d"] + v["sent_pkts_d"]
          cong += v["cong_events_d"]
          rtt  += v["rtt_ms"]; cw += v["cwnd"]; mtu = v["quic_mtu"]
        }
        END {
          if (n==0) { print "    (no quic stats lines in window)"; exit }
          printf "    quic: rtt %.1f ms  cwnd %.0f  mtu %s  sent %d  lost %d (%.2f%%)  cong_events %d\n",
                 rtt/n, cw/n, mtu, sent, lost, (sent>0? lost*100/sent : 0), cong
        }'
}

# --- one arm ----------------------------------------------------------------
# Prints one result block; returns nothing the caller parses, because every
# number here is for a human deciding what to change next.
run_arm() { # <arm>
    local arm="$1"
    if [ "$arm" = bare ]; then
        [ "$(vm_iperf_server)" = 1 ] || { echo "    bare: FAILED(no iperf3 server)"; return; }
        local u; u="$(tcp_mbps "$BORE_VM" "$SECS" 1)"
        printf "    %-7s goodput %8s Mbit/s\n" bare "$u"
        return
    fi

    local extra=""
    [ "$arm" = relay ] && extra="--relay-only"
    VPN_LINK_ID="${VPN_RUN_ID}d$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen $extra
    sleep 3
    ws_up dd connect $extra
    if ! ws_ready dd 45 >/dev/null; then
        echo "    $arm: FAILED(link never came up)"; vpn_cleanup; sleep 2; return
    fi
    if [ "$arm" = direct ]; then
        local got; got="$(wait_path dd direct 75)"
        [ "$got" = direct ] || { echo "    direct: FAILED(stayed on $got -- a relay number labelled direct would be a lie)"; vpn_cleanup; sleep 2; return; }
    fi
    # An arm whose TUN MTU never stopped moving is NOT a sample: inner goodput is
    # proportional to MSS (Mathis), and a fresh link climbs 1350 -> 1288 -> 1414
    # over ~25 s. `wait_mtu_settle` returns 1 when it gives up, and this call
    # used to discard the value AND the status -- so a link that never settled
    # was measured anyway and nothing said so. Measured consequence, wired
    # 2026-09-12: `vpn_cc_matrix` and `vpn_udpbuf` each produced samples ~4 %
    # low whose `rtt min` was 17-22 ms against 31 ms for every settled sample,
    # two disjoint groups splitting ACROSS the arms rather than along them.
    ARM_MTU="$(wait_mtu_settle dd 24 90)" || {
        echo "    $arm: FAILED(mtu never settled, last=$ARM_MTU) -- not averaged in"
        vpn_cleanup; sleep 2; return; }
    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "    $arm: FAILED(no iperf3 server)"; vpn_cleanup; sleep 2; return
    fi

    local wp; wp="$(ws_pid dd)"
    # The VM runs bore under `sudo -n env ...`, so THREE processes match a
    # pattern like 'bore vpn listen': the sudo wrapper, the env wrapper and the
    # binary itself. `head -1` picks the lowest pid, which is the sudo wrapper
    # -- a process that has done no work since it exec'd, so its utime+stime
    # never moves and the VM CPU column read a confident 0.00. Select by the
    # link id (unique per run) and keep only the pid whose `comm` is the binary.
    #
    # And it must NOT be selected by comparing `comm` to the literal "bore":
    # the binary is DEPLOYED to the VM under its own name (`$VM_BORE`, today
    # `~/bore-vpn`), so `comm` reads `bore-vpn` and that comparison never
    # matched. Every VM cpu cell read 0.00 for the whole campaign because of it.
    # Select by EXCLUSION instead -- reject the wrapper processes by name -- so
    # renaming the deployed binary cannot silently blind the column again.
    local vpid; vpid="$(vm "for p in \$(pgrep -f -- '--id $VPN_LINK_ID' 2>/dev/null); do \
        c=\$(cat /proc/\$p/comm 2>/dev/null); \
        case \"\$c\" in sudo|env|sh|bash|'') continue;; esac; \
        echo \$p; break; done" \
        2>/dev/null | tr -dc '0-9')"
    # A missing pid is announced, never averaged in as a zero.
    [ -n "$vpid" ] || echo "    (note: VM bore pid not found -- VM cpu column is not measured this arm)"

    # --- snapshot -----------------------------------------------------------
    local c0 t0 q0 n0 vc0
    c0="$(proc_ticks "$wp")"; t0="$(busiest_thread_ticks "$wp")"
    n0="$(udp_counters)"; q0="$(quic_lines_count dd)"
    vc0="$(vm "awk '{print \$14+\$15}' /proc/$vpid/stat 2>/dev/null || echo 0" 2>/dev/null | tr -dc '0-9')"

    local u; u="$(tcp_mbps "$B_PEER" "$SECS" 1)"

    local c1 t1 q1 n1 vc1
    c1="$(proc_ticks "$wp")"; t1="$(busiest_thread_ticks "$wp")"
    n1="$(udp_counters)"; q1="$(quic_lines_count dd)"
    vc1="$(vm "awk '{print \$14+\$15}' /proc/$vpid/stat 2>/dev/null || echo 0" 2>/dev/null | tr -dc '0-9')"

    # --- derive -------------------------------------------------------------
    local mtu; mtu="$(ws_mtu dd 2>/dev/null || echo '?')"
    awk -v arm="$arm" -v u="${u:-0}" -v secs="$SECS" -v tick="$TICK" \
        -v c0="${c0:-0}" -v c1="${c1:-0}" -v t0="${t0:-0}" -v t1="${t1:-0}" \
        -v vc0="${vc0:-0}" -v vc1="${vc1:-0}" -v mtu="${mtu:-?}" \
        -v so0="$(ctr "$n0" UdpSndbufErrors)" -v so1="$(ctr "$n1" UdpSndbufErrors)" \
        -v od0="$(ctr "$n0" UdpOutDatagrams)" -v od1="$(ctr "$n1" UdpOutDatagrams)" '
    BEGIN {
        gib = u * 1e6 * secs / 8 / 1073741824;
        cpu  = (c1-c0)/tick; thr = (t1-t0)/tick; vcpu = (vc1-vc0)/tick;
        printf "    %-7s goodput %8.2f Mbit/s   tun_mtu %s\n", arm, u, mtu;
        if (gib > 0)
            printf "            cpu  ws %5.2f s (%5.2f s/GiB)   vm %5.2f s (%5.2f s/GiB)\n",
                   cpu, cpu/gib, vcpu, vcpu/gib;
        # A single task at ~100% of one core is a hard limit that total process
        # CPU on a multi-core box hides completely.
        printf "            busiest thread %5.2f s of %d s wall = %5.1f%% of one core\n",
               thr, secs, (secs>0 ? thr*100/secs : 0);
        if (so1-so0 > 0)
            printf "            UdpSndbufErrors +%d  <-- kernel refused sends\n", so1-so0;
        printf "            UdpOutDatagrams +%d\n", od1-od0;
    }'
    [ "$arm" = direct ] && quic_window dd "$(( q1 - q0 ))" | summarize_quic

    vpn_cleanup
    sleep 3
}

for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    for a in $ARMS; do run_arm "$a"; done
done

echo
echo "DONE"
