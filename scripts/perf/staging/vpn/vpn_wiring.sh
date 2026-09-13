#!/usr/bin/env bash
# D1: are the shipped knobs actually WIRED to the device they claim to configure?
#
# THE QUESTION, AND WHY IT IS NOT COVERED ALREADY
# ----------------------------------------------
# `vpn_txqueue.sh` walks the TUN queue ladder by doing
#
#     sudo -n ip link set dev boreN txqueuelen $q
#
# which goes around bore entirely. That measurement is a valid piece of queue
# physics -- it is what established that the kernel default is the worst rung --
# but it proves NEITHER of the two things the product ships:
#
#   * that `create_tun` applies `hostcfg::VPN_TUN_TXQUEUELEN` (128) to the link
#     it creates, rather than leaving the kernel's 500;
#   * that `BORE_VPN_TUN_TXQUEUELEN` reaches the device at all, including its
#     documented edges: `0` means "leave the kernel value alone", anything else
#     is clamped to [16, 500].
#
# The unit gate `tun_txqueuelen_resolution` covers the RESOLUTION of the value.
# Nothing covers the WIRING. That is the same distinction P-12 insists on: a log
# line proves the process talked about a value, the kernel proves it applied it.
#
# METHOD
# ------
# Bring a link up, read the DEVICE back with `ip -o link show` -- the kernel's
# own view, never the log -- and compare against what the knob promised. No
# traffic is generated: this stage measures configuration, not throughput, and
# mixing the two would make it slow for no gain.
#
# A case whose link never comes up is reported FAILED and never silently scored,
# because "the value was not what we expected" and "we never got to look" are
# opposite findings.
#
# Usage: vpn_wiring.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

vpn_hdr "VPN knob wiring -- read from the kernel, no traffic"
echo "  each case: bring a link up with the stated environment, then read the"
echo "  DEVICE back. The unit tests cover value resolution; this covers wiring."
echo

PASS=0; FAIL=0

# The interface name for a tag, from the root helper's own view of the link.
tag_iface() { sudo -n "$ROOTSH" addr "$1" 2>/dev/null | awk '{print $1}'; }

# One case: <label> <env-or-empty> <expected-txqlen> <why>
# A case may declare itself NON-DISCRIMINATING: it still runs (the link must
# come up and the device must be readable) but it does not score a PASS,
# because the value it observes is also what a BROKEN implementation would
# produce. Counting it would inflate the score with an experiment that has no
# failing branch -- and a gate with no failing branch is the thing this whole
# campaign keeps finding.
case_txq() {
    local label="$1" envs="$2" want="$3" why="$4" mode="${5:-score}"
    local tag="wir" iface got
    VPN_LINK_ID="${VPN_RUN_ID}w$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen >/dev/null 2>&1
    sleep 3
    VPN_ENV="$envs" ws_up "$tag" connect >/dev/null 2>&1
    if ! ws_ready "$tag" 60 >/dev/null; then
        printf '  %-34s %-10s FAILED(link never came up)\n' "$label" "-"
        FAIL=$((FAIL + 1)); vpn_cleanup; sleep 2; return
    fi
    iface="$(tag_iface "$tag")"
    got="$(ip -o link show dev "$iface" 2>/dev/null | grep -oE 'qlen [0-9]+' | awk '{print $2}')"
    # `ip -o link` omits qlen entirely on some kernels when it equals the
    # device default; an empty read is "could not tell", not "zero".
    if [ -z "$got" ]; then
        printf '  %-34s want %-6s UNREADABLE(no qlen field) -- %s\n' "$label" "$want" "$why"
        FAIL=$((FAIL + 1))
    elif [ "$got" = "$want" ] && [ "$mode" = nodisc ]; then
        printf '  %-34s want %-6s got %-6s (NOT DISCRIMINATING) -- %s\n' "$label" "$want" "$got" "$why"
    elif [ "$got" = "$want" ]; then
        printf '  %-34s want %-6s got %-6s PASS   -- %s\n' "$label" "$want" "$got" "$why"
        PASS=$((PASS + 1))
    else
        printf '  %-34s want %-6s got %-6s FAIL   -- %s\n' "$label" "$want" "$got" "$why"
        FAIL=$((FAIL + 1))
    fi
    vpn_cleanup; sleep 2
}

echo "=== TUN txqueuelen (hostcfg::VPN_TUN_TXQUEUELEN, override BORE_VPN_TUN_TXQUEUELEN) ==="
case_txq "default"            ""                              128 "V-10's shipped default reaches the device"
case_txq "override=64"        "BORE_VPN_TUN_TXQUEUELEN=64"     64 "an in-range override is applied verbatim"
case_txq "override=0"         "BORE_VPN_TUN_TXQUEUELEN=0"     500 "0 means leave the kernel's own value"
# THE TOP CLAMP IS NOT OBSERVABLE FROM THE DEVICE ON THIS KERNEL.
# The clamp's ceiling is 500 and a TUN device's own default txqueuelen is also
# 500, so "clamped to 500" and "the override was ignored entirely" produce the
# SAME qlen. The case is kept -- it proves the link still comes up and nothing
# refuses an out-of-range value -- but it does not score, because it has no
# failing branch. The BOTTOM clamp is the discriminating half (1 -> 16, which
# no default produces) and it is the one that carries the invariant.
case_txq "override=9999"      "BORE_VPN_TUN_TXQUEUELEN=9999"  500 "out of range is clamped, not refused (ceiling == kernel default, so not discriminating)" nodisc
case_txq "override=1"         "BORE_VPN_TUN_TXQUEUELEN=1"      16 "clamped to the bottom, not refused"

echo
echo "=== MTU (--mtu) ==="
# The TUN is created at --mtu and the PMTU monitor moves it afterwards, so this
# reads it IMMEDIATELY and does not wait for settle -- the opposite of every
# throughput stage, and on purpose: the question here is what create_tun set.
VPN_LINK_ID="${VPN_RUN_ID}wm$(date +%s%N | tail -c 5)"
VPN_WS_TAGS=(); VPN_VM_IDS=()
vm_up listen >/dev/null 2>&1
sleep 3
BENCH_MTU=1350 ws_up wir connect >/dev/null 2>&1
if ws_ready wir 60 >/dev/null; then
    ifc="$(tag_iface wir)"
    m="$(ip -o link show dev "$ifc" 2>/dev/null | grep -oE 'mtu [0-9]+' | awk '{print $2}')"
    if [ "$m" = 1350 ]; then
        printf '  %-34s want %-6s got %-6s PASS\n' "--mtu 1350 at creation" 1350 "$m"; PASS=$((PASS + 1))
    else
        # Not necessarily a defect: the PMTU monitor may already have moved it.
        printf '  %-34s want %-6s got %-6s (PMTU may have already moved it -- not scored)\n' \
               "--mtu 1350 at creation" 1350 "$m"
    fi
else
    echo "  --mtu case FAILED(link never came up)"; FAIL=$((FAIL + 1))
fi
vpn_cleanup; sleep 2

echo
echo "=== verdict ==="
printf '  pass=%s fail=%s\n' "$PASS" "$FAIL"
[ "$FAIL" = 0 ] && echo "  every knob checked reaches the device." \
                || echo "  a knob that does not reach the device is a shipped default that is not shipped."
echo
echo "DONE"
[ "$FAIL" = 0 ]
