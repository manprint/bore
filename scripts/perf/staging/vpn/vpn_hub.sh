#!/usr/bin/env bash
# V5: hub mode (`--max-clients N>1`) over the REAL path.
#
# WHY THIS STAGE EXISTS
# ---------------------
# Hub mode was, until this campaign, a full traversal generation behind the 1:1
# path: it offered a bare candidate list, so the broker could not compute an
# adaptive plan, so neither side could build a check config, so both fell
# through to the legacy blind punch — with no authenticated check round, no plan
# ordering, no sprayed escape, no winning-pair cache and no S-5. It was
# self-consistent, which is exactly why nothing ever failed and nobody noticed.
#
# The netns suite proves the fix does not regress hub mode. It cannot prove the
# thing that matters here, because netns has no real NAT: that a hub spoke on a
# real home NAT now runs the AUTHENTICATED round and reaches direct. That is
# what this stage measures.
#
# TOPOLOGY
# --------
# The hub runs on the VM; BOTH spokes run on this workstation, as two separate
# processes with their own TUNs. Two spokes behind the SAME NAT is not an
# artificial arrangement — it is the ordinary shape of two laptops in one office
# — and it is the one this workstation can actually produce. Hub mode requires
# server pool addressing (no static /30), so the overlay addresses are read back
# from the logs rather than assumed.
#
# Usage: [SECS=10] vpn_hub.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

SECS="${SECS:-10}"

vpn_hdr "VPN hub mode over the real path (hub on VM, 2 spokes on this workstation)"
echo

VPN_LINK_ID="${VPN_RUN_ID}h$(date +%s%N | tail -c 5)"
VPN_WS_TAGS=(); VPN_VM_IDS=()
VPN_VM_IDS+=("$VPN_LINK_ID")

# Hub listener: pool addressing, host-only (no --advertise), 3 client slots.
vm "mkdir -p ~/out; setsid nohup sudo -n env RUST_LOG=bore_cli=debug,bore=debug,info \
    $VM_BORE vpn listen --id '$VPN_LINK_ID' --to '$BORE_TO' --secret '$BORE_SECRET' \
    --max-clients 3 --mtu $BENCH_MTU \
    > ~/out/$VPN_LINK_ID.log 2>&1 < /dev/null & disown" >/dev/null 2>&1
sleep 5
echo "hub started; waiting for spokes"

# Registered here, NOT inside the function. `spoke_up` is invoked in a command
# substitution to capture its readiness line, and a command substitution runs in
# a SUBSHELL: a `VPN_WS_TAGS+=` performed inside one is discarded when it exits,
# so the EXIT trap would never learn the tag and the endpoint would survive the
# run. That is precisely what happened on the first hub attempt, which left two
# TUN devices behind on the workstation.
VPN_WS_TAGS+=(hubA hubB)

spoke_up() { # <tag>
    sudo -n "$ROOTSH" start "$1" vpn connect \
        --id "$VPN_LINK_ID" --to "$BORE_TO" --secret "$BORE_SECRET" \
        --mtu "$BENCH_MTU" --accept-all-routes >/dev/null
    ws_ready "$1" 60
}

echo "spoke A: $(spoke_up hubA)"
sleep 2
echo "spoke B: $(spoke_up hubB)"
echo

# Give both spokes a full direct-upgrade round. The retry grid is 30 s, so the
# window must exceed one interval plus an attempt or a slow first round reads as
# a failure to upgrade when it was only a failure to wait.
echo "waiting up to 90s for per-spoke direct upgrades..."
for i in $(seq 1 30); do
    a="$(ws_path hubA)"; b="$(ws_path hubB)"
    [ "$a" = direct ] && [ "$b" = direct ] && break
    sleep 3
done
echo "  spoke A path: $(ws_path hubA)"
echo "  spoke B path: $(ws_path hubB)"
echo

# THE point of the stage: did the authenticated round run, or did the pair fall
# through to the legacy blind punch? The hub logs one line per peer round.
echo "=== hub side: authenticated check rounds ==="
# `try_hub_peer_direct` builds a CheckConfig only when the brokered punch
# carried a v2 payload; without one it takes the `None` arm, which is the legacy
# blind punch, and this line is never emitted. So the count is the discriminator
# between the two paths -- and it is NOT the same question as "did the peer
# reach direct": on an easy NAT the blind punch reaches direct too, which is
# precisely why this went unnoticed for a generation. Both are printed.
ROUNDS="$(vm "grep -c 'hub peer check round finished' ~/out/$VPN_LINK_ID.log 2>/dev/null" | tail -1 | tr -dc '0-9')"
UPGRADES="$(vm "grep -c 'hub peer upgraded to direct' ~/out/$VPN_LINK_ID.log 2>/dev/null" | tail -1 | tr -dc '0-9')"
printf '  authenticated rounds: %s     direct upgrades: %s\n' "${ROUNDS:-?}" "${UPGRADES:-?}"
if [ "${ROUNDS:-0}" -gt 0 ] 2>/dev/null; then
    echo "  VERDICT: hub runs the authenticated round (Fase 2/3/7 reach hub mode)."
else
    echo "  VERDICT: hub is on the LEGACY BLIND PUNCH -- no authenticated round ran."
    echo "           A direct upgrade here was won by the blind punch, which this"
    echo "           NAT permits; it is not evidence that the round works. Check the"
    echo "           DEPLOYED hub binary before reading this as a code defect."
fi
vm "grep -E 'hub peer check round finished|computed adaptive traversal plan|hub peer upgraded to direct' ~/out/$VPN_LINK_ID.log 2>/dev/null | tail -8"
echo
echo "=== spoke A side ==="
ws_log hubA 4000 | grep -E 'adaptive traversal plan|connectivity-check round finished|upgraded to direct' | tail -5
echo

# Addresses are pool-assigned, so they are read back rather than assumed.
AA="$(sudo -n "$ROOTSH" addr hubA 2>/dev/null | awk '{print $2}' | cut -d/ -f1)"
BA="$(sudo -n "$ROOTSH" addr hubB 2>/dev/null | awk '{print $2}' | cut -d/ -f1)"
# Read from the hub's OWN kernel rather than parsed out of a spoke's log: the
# previous pattern matched nothing, HUB stayed empty, and the `if [ -n "$HUB" ]`
# below then skipped the reachability probe AND the throughput measurement
# without saying so -- the stage reported success having measured no bandwidth.
HUB="$(vm "ip -o -4 addr show 2>/dev/null | awk '/bore/{split(\$4,a,\"/\"); print a[1]; exit}'" 2>/dev/null | tr -dc '0-9.')"
echo "overlay: spokeA=$AA spokeB=$BA hub=${HUB:-unknown}"

# Spoke isolation (D2) is a SECURITY property, so it is checked, not assumed:
# a host-only hub must not let one spoke reach another.
if [ -n "$AA" ] && [ -n "$BA" ]; then
    # BOTH spokes run on this workstation, so spoke B's overlay address is a
    # LOCAL address here, and the kernel answers any packet to a local address
    # from the `local` routing table without ever putting it on a wire:
    #
    #   $ ip route get 10.99.0.3
    #   local 10.99.0.3 dev lo table local src 10.99.0.3
    #
    # The ping therefore never reaches the hub, and a hub enforcing isolation
    # perfectly would still read REACHABLE. This probe used to print exactly
    # that and call it "isolation broken" -- a verdict the measurement could
    # not support in either direction.
    echo -n "  spoke A -> spoke B (spoke isolation): "
    if ip route get "$BA" 2>/dev/null | head -1 | grep -q '^local '; then
        echo "NOT MEASURABLE -- both spokes are on this host"
        echo "      (dst is local; the kernel answers it without transiting the hub.)"
        echo "      Answering it needs the two spokes on DIFFERENT hosts. The netns"
        echo "      suite already covers isolation with real separate namespaces"
        echo "      (T-HUB* in scripts/vpn_netns_test.sh) and remains its oracle."
    elif ping -c 2 -W 2 -q "$BA" >/dev/null 2>&1; then
        echo "REACHABLE -- isolation broken"
    else
        echo "unreachable (correct)"
    fi
fi

# A stage that measured nothing must not exit 0. The driver writes its resume
# marker on rc=0 and never comes back, so a silent green here is how a stage
# disappears from a campaign while still appearing in its log -- which is
# exactly what happened on 2026-09-12 (19 s against a 2400 s budget, rc=0, no
# bandwidth number anywhere in the output). Failing means no marker, a FAIL
# line, and a retry on the next driver run.
RC=0
if [ -z "${HUB:-}" ]; then
    echo "  hub overlay address NOT RESOLVED -- reachability and throughput were"
    echo "  NOT measured. This stage has no bandwidth result; do not read its"
    echo "  success as one."
    RC=1
fi
if [ -n "${HUB:-}" ]; then
    echo -n "  spoke A -> hub: "
    ping -c 3 -W 2 -q "$HUB" 2>/dev/null | tail -1 || echo "no reply"
    if [ "$(vm_iperf_server)" = 1 ]; then
        echo "  throughput spoke A -> hub: $(tcp_mbps "$HUB" "$SECS" 1) Mbit/s"
    fi
fi

echo
if [ "$RC" -ne 0 ]; then
    echo "FAILED -- no bandwidth measured (see above)"
fi
echo "DONE"
exit "$RC"
