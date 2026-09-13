#!/usr/bin/env bash
# D3: does the connector's route POLICY reach the kernel routing table?
#
# THE GAP THIS FILLS
# ------------------
# `vpn::filter_accepted` is well unit-tested: six cases in `src/vpn.rs` pin
# default-deny, accept-all with refuse-all, the accept list, the refuse list and
# the exact-or-supernet matching rule. Those tests prove the POLICY.
#
# Nothing proves the WIRING. Grepping the two field oracles for the flags:
#
#     --accept-all-routes   netns 33 hits, perf 2 stages     covered
#     --refuse-routes       netns  4 hits                    covered
#     --accept-routes       netns  0 hits, perf 0 stages     NOT COVERED
#     --refuse-all-routes   netns  0 hits, perf 0 stages     NOT COVERED
#     --no-route-manage     netns  0 hits, perf 0 stages     NOT COVERED
#
# A route policy that resolves correctly and then fails to install (or, worse,
# installs something it refused) is a security-relevant defect that no unit test
# on a pure function can see. So this reads `ip route show` -- the kernel, never
# the log -- after each case.
#
# `--no-route-manage` deserves its own line: it promises that bore will not touch
# routing at all. Nothing anywhere verifies that promise, and a flag whose whole
# content is "do nothing" is exactly the kind that rots silently.
#
# NO TRAFFIC is generated. The advertised CIDR is deliberately a subnet that
# exists nowhere (192.168.77.0/24) -- this stage asks what the routing table
# says, never what a packet does, so an unreachable destination is correct and
# an existing one would risk disturbing the host's real routing.
#
# Usage: [ADV=192.168.77.0/24] vpn_routes.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

ADV="${ADV:-192.168.77.0/24}"

vpn_hdr "VPN connector route policy -- read from the kernel routing table"
echo "  listener advertises $ADV (a subnet that exists nowhere: this stage reads"
echo "  the routing table, it never sends a packet)"
echo "  default is DENY (I-MC8): a connector with no accept flag installs nothing"
echo

PASS=0; FAIL=0

# Is the advertised CIDR in this host's routing table, pointing at a bore TUN?
route_present() {
    ip route show 2>/dev/null | grep -qE "^${ADV//./\\.}[[:space:]].*dev bore" && echo yes || echo no
}

# <label> <want yes|no> <connector flags...>
case_route() {
    local label="$1" want="$2"; shift 2
    local got
    VPN_LINK_ID="${VPN_RUN_ID}r$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen --advertise "$ADV" >/dev/null 2>&1
    sleep 3
    ws_up rt connect "$@" >/dev/null 2>&1
    if ! ws_ready rt 60 >/dev/null; then
        printf '  %-46s want %-4s FAILED(link never came up)\n' "$label" "$want"
        FAIL=$((FAIL + 1)); vpn_cleanup; sleep 2; return
    fi
    # The route is installed as part of bringing the link up, but give the
    # apply step a moment: a race here would report a policy failure.
    sleep 3
    got="$(route_present)"
    if [ "$got" = "$want" ]; then
        printf '  %-46s want %-4s got %-4s PASS\n' "$label" "$want" "$got"
        PASS=$((PASS + 1))
    else
        printf '  %-46s want %-4s got %-4s **FAIL**\n' "$label" "$want" "$got"
        ip route show 2>/dev/null | grep -E 'bore|192\.168\.77' | sed 's/^/        /'
        FAIL=$((FAIL + 1))
    fi
    vpn_cleanup; sleep 2
}

echo "=== the policy, end to end ==="
# Literally no flag: that is what "default-deny" has to mean, and passing
# --refuse-all-routes here would test the explicit form instead of the default.
case_route "default (NO flags at all) -- DEFAULT-DENY"   no
case_route "--refuse-all-routes (explicit, same result)" no  --refuse-all-routes
case_route "--accept-all-routes"                         yes --accept-all-routes
case_route "--accept-routes exact ($ADV)"                yes --accept-routes "$ADV"
case_route "--accept-routes supernet (192.168.0.0/16)"   yes --accept-routes 192.168.0.0/16
case_route "--accept-routes unrelated (10.0.0.0/8)"      no  --accept-routes 10.0.0.0/8
case_route "--accept-all + --refuse-routes the route"    no  --accept-all-routes --refuse-routes "$ADV"
case_route "--refuse-all-routes beats --accept-all"      no  --accept-all-routes --refuse-all-routes
case_route "--no-route-manage (bore must not route)"     no  --accept-all-routes --no-route-manage

echo
echo "=== verdict ==="
printf '  pass=%s fail=%s\n' "$PASS" "$FAIL"
if [ "$FAIL" = 0 ]; then
    echo "  the accepted set reaches the routing table and the refused set does not."
else
    echo "  a route installed where the policy refused it, or refused where it accepted,"
    echo "  is a correctness defect -- the unit tests cannot see this layer."
fi
echo
echo "DONE"
[ "$FAIL" = 0 ]
