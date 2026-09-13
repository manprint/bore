#!/usr/bin/env bash
# P7 of the Ethernet window: the three measurements the main sweep cannot make.
#
# WHY A SECOND DRIVER INSTEAD OF MORE STAGES IN THE FIRST
# -------------------------------------------------------
# `rerun_eth.sh` was already executing when these three stages were written, and
# a shell reads a script incrementally as it runs -- editing a live one is how a
# driver starts executing a line that did not exist when the line above it ran.
# So P7 is a separate file. It shares `run_stage` verbatim (markers, per-stage
# timeout, host-check, settle) so a stage behaves identically under either.
#
# WHAT IS IN IT, AND WHY EACH IS NOT OPTIONAL
# -------------------------------------------
#  1. `vpn_relay_attrib` -- the relay arm's ceiling is currently unattributed.
#     Every campaign in this repository relays through ONE t4g.micro, and the
#     relay arm is a DOUBLE TRANSIT through it, not a third transport over the
#     same path. Until the allowance counters are read as a delta bracketing the
#     arm AND the same double transit is measured with a non-bore relay, no
#     relay percentage anywhere is a sentence about bore's code.
#
#  2. `vpn_rtt_load` -- `vpn_txqueue` and `vpn_sndbuf` both report ~31 ms RTT
#     minimum under load against a ~19 ms idle bare path, and neither samples
#     the BARE path under load. That 12 ms is therefore split between "the
#     tunnel adds it" and "any saturating flow adds it" by assumption. Latency
#     is half the stated goal of this campaign; an unattributed 12 ms is not a
#     detail.
#
#  3. `vpn_direct_deficit_r2` -- the first run announced `VM bore pid not found`
#     and left the VM CPU column at 0.00 in every arm. The selector compared
#     /proc/<pid>/comm to the literal "bore" while the binary is deployed as
#     `bore-vpn`. Fixed and red-checked against a live link; the stage is re-run
#     under a SEPARATE name so the original output stays readable beside it.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

export BORE_PERF_OUT="$REPO/out/eth"
mkdir -p "$BORE_PERF_OUT"
LOG="$BORE_PERF_OUT/_driver_p7.log"
export LC_ALL=C

say() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

# The contention guard lives in ONE file, sourced here. It used to be copied
# into each driver, and the copies shared a matching rule that reported a
# MONITOR reading this driver's log as the driver itself -- see driverlib.sh.
# shellcheck disable=SC1091
# Repo-root relative, NOT `dirname $BASH_SOURCE`: every driver has already
# `cd`-ed to the repo root by this point, so a relative BASH_SOURCE would be
# resolved against the NEW directory and only work when the driver happened
# to be invoked from the root.
. scripts/perf/staging/driverlib.sh

found="$(other_driver)"
if [ -n "$found" ]; then
    say "REFUSING: the main sweep is still running -- P7 must not share the link."
    printf '%s\n' "$found" | sed 's/^/    /'
    exit 3
fi

baseline() {
    local tag="$1" d u
    ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
        "$BORE_VM_USER@$BORE_VM" \
        "pgrep -x iperf3 >/dev/null || (nohup iperf3 -s -p 5299 >/dev/null 2>&1 &); sleep 1" \
        >/dev/null 2>&1
    d=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299 -R -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    sleep 2
    u=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299    -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    say "BASELINE[$tag] download=${d:-FAILED} upload=${u:-FAILED} Mbit/s"
}

run_stage() {
    local name="$1" tmo="$2" script="$3"; shift 3
    local marker="$BORE_PERF_OUT/_done.$name"
    if [ -f "$marker" ]; then say "SKIP  $name (marker present)"; return 0; fi
    if [ ! -x "$REPO/$script" ]; then say "MISS  $name ($script not executable)"; return 0; fi
    say "BEGIN $name  (timeout ${tmo}s)  $*"
    local t0=$SECONDS rc
    ( cd "$REPO" && env "$@" timeout -k 30 "$tmo" "$REPO/$script" ) \
        >"$BORE_PERF_OUT/$name.out" 2>&1
    rc=$?
    local el=$((SECONDS - t0))
    if [ $rc -eq 0 ]; then
        touch "$marker"; say "END   $name rc=0 elapsed=${el}s"
    else
        say "FAIL  $name rc=$rc elapsed=${el}s  (see $name.out; no marker, will retry)"
    fi
    local ifaces routes
    ifaces=$(ip -br link 2>/dev/null | { grep -c '^bore' || true; })
    routes=$(ip route 2>/dev/null | { grep -c 'bore' || true; })
    if [ "${ifaces:-0}" != 0 ] || [ "${routes:-0}" != 0 ]; then
        say "      host-check: LEFTOVER bore ifaces=$ifaces routes=$routes"
    fi
    sleep 10
    return 0
}

say "################ P7: attribution ################"
say "repo:    $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null) $(git -C "$REPO" status --porcelain 2>/dev/null | wc -l) file(s) dirty"
say "binary:  $(sha256sum "$REPO/target/release/bore" 2>/dev/null | cut -c1-16) $("$REPO/target/release/bore" --version 2>/dev/null | head -1)"
say "nic:     $(ip route show default | awk '/^default/{print $5; exit}') speed=$(cat /sys/class/net/"$(ip route show default | awk '/^default/{print $5; exit}')"/speed 2>/dev/null) Mbit/s"
# The FAR END's binary, which no driver recorded until now. Every VPN stage has
# two peers and the provenance line above describes only one of them, so a
# result that depends on far-end behaviour -- hub mode's authenticated check
# round is the case that forced this -- could not be attributed to a build.
# Read over ssh once, before any measurement, never during one.
say "far bin: $(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
        -i "$BORE_SSH_KEY" "$BORE_VM_USER@$BORE_VM" \
        'sha256sum $HOME/bore-vpn 2>/dev/null | cut -c1-16; $HOME/bore-vpn --version 2>/dev/null | head -1' \
        2>/dev/null | tr '\n' ' ')"

baseline p7-start

run_stage vpn_relay_attrib       3600 scripts/perf/staging/vpn/vpn_relay_attrib.sh
run_stage vpn_rtt_load           3600 scripts/perf/staging/vpn/vpn_rtt_load.sh
# V-17 belongs in the attribution block and was written without a driver, which
# is the same defect as a gate nobody runs: it measures bytes-on-the-wire per
# delivered byte and so decides whether the direct path's residual few per cent
# is framing arithmetic (finished work) or retransmission (P-13's signature, a
# hunt). Four tunable ladders came back flat; this is the term they leave.
run_stage vpn_overhead           2400 scripts/perf/staging/vpn/vpn_overhead.sh
# V-18, and it belongs beside the attribution rather than in the deep dive: the
# relay carries a reliable byte stream, so its TUN MTU of 1350 -- a constraint
# the DIRECT path imposes -- costs a relay-bound link one AEAD seal, one frame
# header and one TUN entry per 1350 bytes instead of per 8000. Either the ladder
# is flat (the shipped default is free, and the ceiling is the deployment) or it
# is not (a relay-bound user is paying for a limit only the other path has).
run_stage vpn_relay_mtu          3600 scripts/perf/staging/vpn/vpn_relay_mtu.sh
# The PUBLIC relay's own version of the same question, and it belongs here for
# the same reason: its last hop is SERVER -> workstation, so it shares the
# t4g.micro with the VPN relay and shares vpn_relay_attrib's control. Its first
# run measured 24 MiB per connection -- 0.2 s on a 922 Mbit/s line, which is
# mostly TCP slow start -- took ONE sample per cell, and always walked the
# ladder in the same direction; it then reported a 36 % drop at n=8 that nothing
# in its output could distinguish from noise. 256 MiB per connection, three
# repetitions, alternating ladder direction, raw samples printed.
run_stage pub_ws_conns_r2        3600 scripts/perf/staging/pub/ws_conns.sh
# Same script, new name: the original output and its `VM bore pid not found`
# note stay on disk beside the corrected run.
run_stage vpn_direct_deficit_r2  3600 scripts/perf/staging/vpn/vpn_direct_deficit.sh
# The probe that FOUND the open defect. It is expected to FAIL today -- that is
# the point: it is the gate the fix has to turn green, so it must exist and run
# before the fix, not after. A failing stage leaves no marker and is retried,
# which is correct here.
run_stage vpn_ctrl_leak          2400 scripts/perf/staging/vpn/vpn_ctrl_leak.sh
# Re-run with the settle bug fixed (an arm whose MTU never settled used to be
# measured anyway and silently averaged in) and REPS raised from 2 to 4, since
# two repetitions cannot rank seven arms.
run_stage vpn_cc_matrix_r2       3600 scripts/perf/staging/vpn/vpn_cc_matrix.sh
# Same settle bug, same fix: 3 of its 8 samples were measured on an unsettled
# MTU. The cleaned result is already flat, but on five samples.
run_stage vpn_udpbuf_r2          3600 scripts/perf/staging/vpn/vpn_udpbuf.sh
# Same class again: it holds one link per path and walked the flow ladder while
# the MTU was still climbing, putting the 1288 trough on the flows=2 rung.
run_stage vpn_profile_r2         1800 scripts/perf/staging/vpn/vpn_profile.sh
# The hub stage finished in 19 s having measured NO bandwidth: its hub-address
# parse matched nothing, and the `if [ -n "$HUB" ]` that guarded the throughput
# run then skipped it silently. It also printed a spoke-isolation verdict its
# own topology cannot support (both spokes are on this host, so the destination
# is a local address and the kernel answers it without transiting the hub), and
# reported the authenticated-check-round count -- the stage's actual purpose --
# as a bare number beside a successful direct upgrade won by the LEGACY blind
# punch. All three are fixed; this re-run is what produces the hub's first
# throughput figure and a verdict on the round.
run_stage vpn_hub_r2             2400 scripts/perf/staging/vpn/vpn_hub.sh
# The first soak lost SIX SEVENTHS of its throughput by the third reconnect --
# 712 Mbit/s in cycles 1 and 2, then 95.35 / 159.87 / 118.51 in cycle 3 -- while
# the connector's descriptor count climbed one per reconnect (12 -> 13 -> 14)
# and RSS went 24 -> 54 MiB. The bare baseline taken 29 s later read 733 up, so
# the line was healthy; but that control came from the NEXT stage's header
# rather than from inside the cycle, which is the one thing V-9 forbids. The
# stage now carries its own bare control and the far end's allowance delta per
# cycle. Five cycles, to see whether the third is a cliff or a slope.
run_stage vpn_stability_r2       3600 scripts/perf/staging/vpn/vpn_stability.sh CYCLES=5

# --- public tunnel, re-measured at a WIRED transfer size -------------------
# These three ran at 96 MiB per arm, which was two seconds over WiFi and is
# 0.83 s at 922 Mbit/s -- mostly TCP slow start. Their wired output says so
# plainly: pub_ws_dl1 produced paired ratios 0.745 0.984 0.991 1.323 0.979
# 1.005 and pub_ws_carr produced 0.623 1.029 1.148, on arms that differ by one
# variable. `XFER_MB` now defaults to 384 MiB (~3.3 s at line rate) and
# `XFER_MB=96` reproduces the originals, which stay on disk beside these.
# They are LAST on purpose: the VPN attribution above is what this phase exists
# for, and a driver that runs out of window must lose these, not that.
run_stage pub_ws_asym_r2         3600 scripts/perf/staging/pub/ws_asym.sh
run_stage pub_ws_dl1_r2          3600 scripts/perf/staging/pub/ws_dl1.sh
run_stage pub_ws_carr_r2         3600 scripts/perf/staging/pub/ws_carr.sh
# `pub_ws_dl` is deliberately NOT re-run: it asks the same question as
# pub_ws_dl1 at four connections, and pub_ws_conns_r2 above covers the
# connection-count axis with repetitions. Its 96 MiB result stands as the WiFi-
# sized figure it is.

baseline p7-end
say "################ P7 complete ################"
