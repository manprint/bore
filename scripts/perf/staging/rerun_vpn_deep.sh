#!/usr/bin/env bash
# VPN deep dive: the options and knobs the campaign never exercised.
#
# WHY THIS EXISTS
# ---------------
# The brief is "the VPN must have no defects", so the useful question is not
# "is it fast" but "which part of the shipped product has never been measured".
# `docs/performance/VPN_DEEP_DIVE_PLAN.md` holds the coverage matrix that answers
# it -- extracted from `VpnListenArgs`/`VpnConnectArgs` and the `BORE_*` reads in
# the source, then crossed against the stages that actually pass each flag.
#
# Two gaps drove the first two stages here, and both have the same shape: the
# VALUE is unit-tested, the WIRING is not.
#
#  1. `vpn_wiring` -- `vpn_txqueue.sh` walks the TUN queue ladder with
#     `ip link set`, going around bore entirely. So the ladder is real queue
#     physics, but nothing shows that `create_tun` applies the shipped
#     `VPN_TUN_TXQUEUELEN = 128`, or that `BORE_VPN_TUN_TXQUEUELEN` reaches the
#     device including its `0` and clamp edges. Read from the kernel, no traffic.
#
#  2. `vpn_routes` -- `filter_accepted` has six unit tests pinning the
#     default-deny policy, and the field oracles pass `--accept-all-routes` 33
#     times. They pass `--accept-routes` ZERO times, `--refuse-all-routes` zero,
#     and `--no-route-manage` zero. A policy that resolves correctly and then
#     installs the route it refused is a correctness defect no unit test on a
#     pure function can see. Read from `ip route show`, no traffic.
#
# Both are cheap and neither generates load, so they run first: a knob that does
# not reach the device would invalidate the throughput stages that assume it did.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

export BORE_PERF_OUT="$REPO/out/eth"
mkdir -p "$BORE_PERF_OUT"
LOG="$BORE_PERF_OUT/_driver_deep.log"
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
    say "REFUSING: another campaign driver is running -- they must not share the link."
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

say "################ VPN deep dive ################"
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

# --------------------------------------------------------------- wiring first
# No traffic in either. If a knob does not reach the device, every ladder that
# set it measured the default and said otherwise -- so this runs before anything
# that depends on a knob being live.
run_stage vpn_wiring   1800 scripts/perf/staging/vpn/vpn_wiring.sh
run_stage vpn_routes   2400 scripts/perf/staging/vpn/vpn_routes.sh

# ------------------------------------------------------- then the four that
# cost time rather than bytes. `vpn_traversal_opts` is the only stage in the
# campaign that exercises --stun-server / --nat-udp-preferred-port /
# --try-port-prediction / --upnp at all, and it prices them where they act: on
# TIME TO DIRECT, not on throughput. It runs AFTER the two wiring stages
# because an arm is only worth timing once the knobs are known to reach the
# device.
run_stage vpn_traversal 3600 scripts/perf/staging/vpn/vpn_traversal_opts.sh

# `vpn_quic_timers` is the only stage anywhere in this repository that kills a
# LIVE direct path on the real path and times the recovery. It is therefore the
# only field evidence for DEC-2 (fall back to the warm relay in place, no
# reconnect) and the only thing that prices BORE_DIRECT_QUIC_IDLE_MS /
# _KEEPALIVE_MS. It installs an nft rule; `vpnlib`'s cleanup removes it on
# every exit path and `vpn_assert_clean` fails the run if one survives.
run_stage vpn_quic_timers 3600 scripts/perf/staging/vpn/vpn_quic_timers.sh

# Last because it is the longest and the least likely to change a conclusion:
# V-13 already falsified carriers on the direct path. What it did NOT measure
# is the RELAY ladder (a different mechanism -- per-datagram round-robin across
# substream pairs, DEC-7) or the multi-flow direct case that BW-F2's flow
# pinning makes the only one where a second carrier is reachable at all.
run_stage vpn_carriers 5400 scripts/perf/staging/vpn/vpn_carriers.sh

baseline deep-end
say "################ VPN deep dive complete ################"
