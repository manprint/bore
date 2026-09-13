#!/usr/bin/env bash
# V13: leak and stability over the REAL path.
#
# WHAT THIS STAGE IS FOR
# ----------------------
# Every other stage in this campaign asks "how fast", and each one runs for
# seconds. A tunnel is a long-lived process: the defects that matter to an
# operator are the ones that need minutes and a few reconnects to appear --
# memory that only grows, tasks that are spawned per path switch and never
# joined, a TUN or a route that survives its link. None of those are visible in
# a 10 s throughput sample, and all of them have shipped in this repo before
# (the hub orphan-task leak F1, the progress-tracker tick leak B2, the SSH
# ConnState reference cycle).
#
# WHAT IS SAMPLED, AND WHY IT IS NOT JUST RSS
# -------------------------------------------
# RSS alone cannot distinguish a leak from an allocator that has not returned
# freed pages to the kernel -- a distinction this repo has already been fooled
# by once (H-18, the glibc mmap-threshold plateau in the secret campaign). So
# three quantities are sampled together:
#
#   RSS      -- grows for both a leak and a retention artefact
#   Threads  -- a task leak shows here and nowhere else; it never plateaus
#   FDs      -- a socket or TUN fd leak, the shape P-12 is about
#
# A leak is RSS rising WITH threads or fds rising. RSS rising alone, and
# plateauing, is the allocator. Both readings are printed so the reader can tell
# which one they are looking at rather than being handed a verdict.
#
# THE RECONNECT CYCLE IS THE POINT
# --------------------------------
# A steady-state link exercises almost none of the teardown code. Each cycle
# therefore KILLS THE VM LISTENER (SIGTERM by the run's own link id -- never a
# blanket pattern), which drops both paths and forces the connector's
# `--auto-reconnect` to rebuild the link, the TUN, the keys and the direct
# upgrade from scratch. That is the path where a leak per link, rather than per
# byte, would accumulate.
#
# Usage: [CYCLES=5] [LOAD_S=20] [ROUNDS=3] vpn_stability.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

# FIVE, not three: three cycles cannot distinguish a cliff at the third
# reconnect from a slope that began at the first, and the first run of this
# stage produced exactly that ambiguity.
CYCLES="${CYCLES:-5}"
LOAD_S="${LOAD_S:-20}"
ROUNDS="${ROUNDS:-3}"
RUNDIR=/run/bore-vpn-bench

ws_pid() { cat "$RUNDIR/$1.pid" 2>/dev/null || echo ""; }

# RSS in KiB and thread count, both from /proc/<pid>/status, which is
# world-readable even though the process is root's. The fd count is NOT: the
# /proc/<pid>/fd directory is mode 0500 and owned by root, so it is asked of the
# privileged helper. A stage that silently reported 0 fds would be worse than
# one that reports nothing.
# All three come from ONE call to the privileged helper's `res` verb. The fd
# count cannot be read any other way: /proc/<pid>/fd on a root process is mode
# 0500, and sudo on this workstation is NOPASSWD per EXACT path, so a `sudo ls`
# here would prompt -- and a stage that reported 0 descriptors forever would be
# worse than one that reported nothing.
ws_res() { sudo -n "$ROOTSH" res "$1" 2>/dev/null || echo "0 0 0"; }

# The far end's ENA allowance, read from the VM. `ena()` in lib.sh reads the
# staging SERVER, which is not on this stage's path at all. Cumulative, so it
# is only meaningful as a DELTA bracketing an arm -- a counter that is nonzero
# at the end cannot say which cycle spent it.
vm_ena() {
    vm "ethtool -S \$(ip route show default | awk '/^default/{print \$5; exit}') 2>/dev/null" 2>/dev/null \
      | awk '/bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded/{
                gsub(/_allowance_exceeded/,"",$1); printf "%s=%s ", $1, $2}'
}

vpn_hdr "VPN stability and leak hunt, real path -- ${CYCLES} cycles x ${ROUNDS} x ${LOAD_S}s"
echo "  each cycle: establish -> direct -> load rounds -> kill the listener -> auto-reconnect"
echo "  a leak is RSS rising TOGETHER with threads or fds; RSS alone that plateaus is the allocator"
echo

VPN_LINK_ID="${VPN_RUN_ID}y$(date +%s%N | tail -c 5)"
VPN_WS_TAGS=(); VPN_VM_IDS=()

# ONE connector for the whole stage, with --auto-reconnect: the process must
# survive every cycle, because a process that is restarted between cycles
# cannot leak across them and the measurement would be vacuous.
vm_up listen
sleep 3
ws_up sta connect --auto-reconnect
if ! ws_ready sta 60 >/dev/null; then
    echo "FAILED: link never came up"; exit 1
fi
WP="$(ws_pid sta)"
[ -n "$WP" ] || { echo "FAILED: no connector pid"; exit 1; }
echo "connector pid $WP"
echo

printf "  %-7s %-8s %10s %8s %6s %10s %8s\n" cycle phase "rss_kib" "threads" "fds" "up_mbit" "path"
sample() { # <cycle> <phase> [mbps]
    local r; r="$(ws_res sta)"
    printf "  %-7s %-8s %10s %8s %6s %10s %8s\n" \
        "$1" "$2" "$(echo "$r" | awk '{print $1}')" "$(echo "$r" | awk '{print $2}')" \
        "$(echo "$r" | awk '{print $3}')" "${3:--}" "$(ws_path sta)"
}

sample 0 start

for c in $(seq 1 "$CYCLES"); do
    # A cycle that never reached direct is reported as such rather than folded
    # into the series: relay and direct allocate differently, and a silent mix
    # would make the RSS column uninterpretable.
    got="$(wait_path sta direct 95)"
    [ "$got" = direct ] || echo "  (cycle $c ran on $got, not direct)"
    # Same rule as every other stage, but a soak REPORTS rather than returns: a
    # cycle that ran on an unsettled MTU is still a valid stability observation,
    # it is just not a comparable throughput sample.
    ARM_MTU="$(wait_mtu_settle sta 24 90)" \
        || echo "  (cycle $c ran on an UNSETTLED mtu, last=$ARM_MTU -- throughput not comparable)"

    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "  cycle $c: FAILED(no iperf3 server)"
    else
        # A BARE control sampled in the SAME cycle. This stage used to omit it,
        # which is the one thing V-9 says a stage may never do: on the first run
        # the tunnel fell from 712 to 95-160 Mbit/s by the third reconnect, and
        # with no control in the cycle there was no way to tell a degrading
        # tunnel from a degrading line. (The driver's next bare baseline, 29 s
        # later, read 733 up -- so the line was fine. That answer should come
        # from inside the cycle, not from the next stage's header.)
        ena_before="$(vm_ena)"
        nic_before="$(ws_nic_drops)"
        # ISO-8601, which is what the connector's log prints and what
        # `ws_quic_since` compares against.
        cyc_start="$(date -u +%Y-%m-%dT%H:%M:%S)"
        bare="$(tcp_mbps "$BORE_VM" "$LOAD_S" 1)"
        sleep 2
        tsum=""
        for r in $(seq 1 "$ROUNDS"); do
            u="$(tcp_mbps "$B_PEER" "$LOAD_S" 1)"
            sample "$c" "load$r" "$u"
            tsum+=" $u"
            sleep 3
        done
        ena_after="$(vm_ena)"
        tmed="$(printf '%s\n' $tsum | med)"
        LC_ALL=C awk -v c="$c" -v b="${bare:-0}" -v t="${tmed:-0}" \
                     -v eb="${ena_before:-n/a}" -v ea="${ena_after:-n/a}" 'BEGIN{
            r = (b > 0) ? t/b : 0
            printf "  cycle %s: bare %.2f Mbit/s   tunnel %.2f   tunnel/bare %.3f\n", c, b, t, r
            printf "           allowance before[%s] after[%s]\n", eb, ea
        }'
        # WHOSE loss is it? The QUIC carrier's own counters say whether the
        # path dropped anything at all, and this end's NIC counters say whether
        # WE dropped it. A cycle that reads slow with lost=0 is a different
        # defect from one that reads slow with lost>0 -- and the first campaign
        # to run this stage could not tell them apart (see the helper's note).
        echo "           carrier  $(ws_quic_since sta "$cyc_start")"
        echo "           ws nic   before[$nic_before] after[$(ws_nic_drops)]"
    fi

    # Idle sample: an allocator that has simply not returned pages settles here,
    # a leak does not.
    sleep 10
    sample "$c" idle

    [ "$c" = "$CYCLES" ] && break

    # Force the reconnect. SIGTERM so the listener's own RAII teardown runs --
    # SIGKILL would exercise the stale-reclaim path instead, which is a
    # different code path and not the one a steady-state operator hits.
    vm "sudo -n pkill -TERM -f 'vpn listen --id $VPN_LINK_ID' 2>/dev/null; true" >/dev/null 2>&1
    sleep 5
    sample "$c" killed
    vm_up listen
    # The connector's reconnect backoff plus a fresh direct round.
    sleep 8
    if ! ws_ready sta 90 >/dev/null; then
        echo "  cycle $c: FAILED(link did not come back after the listener restart)"
        break
    fi
    sample "$c" back
done

echo
echo "=== connector log: anything that should not repeat ==="
# Counted, not printed: the interesting quantity is whether a warning recurs
# once per cycle (a per-link leak) or once ever (a startup condition).
ws_log sta 20000 | grep -oE 'WARN [a-z_:]+|could not set TUN txqueuelen|clamp|leaked|stale' \
    | sort | uniq -c | sort -rn | head -12
echo
echo "=== path events across the whole run ==="
ws_log sta 20000 | grep -cE 'bridge switched to (direct|relay) path' | sed 's/^/  bridge switches: /'
ws_log sta 20000 | grep -cE 'falling back to relay' | sed 's/^/  relay fallbacks: /'
echo
echo "DONE"
