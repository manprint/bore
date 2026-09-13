#!/usr/bin/env bash
# V14: the operational modes that are not plain host-to-host, over the REAL path.
#
# WHAT IS STILL UNMEASURED AFTER THE REST OF THIS CAMPAIGN
# --------------------------------------------------------
# Every other stage runs the 1:1 host-to-host shape: two TUNs, traffic addressed
# to the peer's own overlay address. That is one of four shipped modes. The
# other three put the KERNEL on the data path a second time and none of them had
# ever been measured over a real WAN:
#
#   gateway   `--advertise CIDR`          -- packets are FORWARDED past the TUN,
#                                            through the FORWARD chain and the
#                                            masquerade, to a third host
#   netmap    `--advertise REAL@VIRTUAL`  -- the same, plus a stateless 1:1
#                                            address rewrite in nft on every
#                                            packet in both directions
#   forward   `--forward-accept`          -- the same, on a host whose FORWARD
#                                            policy is DROP
#
# The netns suite proves all three are CORRECT. It cannot say what they cost,
# because netns has no WAN: a rewrite that is free at 10 Gbit/s on loopback is
# not obviously free at 375 Mbit/s through a tunnel whose uplink task is already
# the busiest thread in the process.
#
# HOW THE FAR SIDE IS ADDRESSED WITHOUT PUTTING IT IN THIS FILE
# -------------------------------------------------------------
# The gateway's LAN is the VM's own VPC subnet, and neither it nor the VM's
# private address appears here: both are DISCOVERED from the VM at run time.
# That is not only hygiene -- a hardcoded subnet would also be wrong the first
# time the VM is rebuilt in another VPC.
#
# The comparison is the point, so all three arms are measured against the SAME
# tunnel shape in the SAME repetition, with the plain overlay address as the
# control. The difference between "to the peer's overlay address" and "to the
# peer's LAN address through the same tunnel" is exactly one kernel forwarding
# hop, which is the quantity being priced.
#
# Usage: [REPS=2] [SECS=10] [VIRT=10.88.0.0] [FORWARD_DENY=0] vpn_modes.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-2}"
SECS="${SECS:-10}"
VIRT_BASE="${VIRT:-10.88.0.0}"
FORWARD_DENY="${FORWARD_DENY:-0}"

vpn_hdr "VPN gateway / netmap / forward-accept over the real path, ${REPS} reps x ${SECS}s"

# --- discover the far side's LAN, from the far side -------------------------
# Pinned to the DEVICE the default route uses. Taking "the first kernel route
# with a src" instead would happily return a docker bridge on a VM that happens
# to run containers, and every arm downstream would then be measuring a subnet
# that has no hosts in it.
LAN_DEV="$(vm "ip -4 -o route show default | awk '{for(i=1;i<=NF;i++) if(\$i==\"dev\") {print \$(i+1); exit}}'" 2>/dev/null | tr -dc 'A-Za-z0-9_-')"
LAN_CIDR="$(vm "ip -4 -o route show dev ${LAN_DEV:-lo} | awk '/proto kernel/ {print \$1; exit}'" 2>/dev/null | tr -dc '0-9./')"
VM_PRIV="$(vm "ip -4 -o addr show dev ${LAN_DEV:-lo} | awk '{print \$4; exit}'" 2>/dev/null | cut -d/ -f1 | tr -dc '0-9.')"
# A THIRD host on that LAN is the only way to exercise the FORWARD chain (see
# below). Supplied by the operator, never guessed: this stage must not probe
# addresses on someone else's network on its own initiative.
LAN_HOST="${LAN_HOST:-}"
if [ -z "$LAN_CIDR" ] || [ -z "$VM_PRIV" ]; then
    echo "FAILED: could not discover the VM's LAN subnet / private address"; exit 1
fi
PLEN="${LAN_CIDR#*/}"
# The netmap virtual address that corresponds to the VM's own private address.
# 1:1 netmap preserves host bits, so this is the same host bits under the
# virtual network address -- computed, never guessed, because a wrong address
# here would read as "netmap does not forward" when it is the test that is wrong.
VIRT_CIDR="$VIRT_BASE/$PLEN"
VM_VIRT="$(python3 - "$LAN_CIDR" "$VM_PRIV" "$VIRT_CIDR" <<'PY'
import ipaddress, sys
real = ipaddress.ip_network(sys.argv[1], strict=False)
host = ipaddress.ip_address(sys.argv[2])
virt = ipaddress.ip_network(sys.argv[3], strict=False)
assert real.prefixlen == virt.prefixlen, "netmap requires equal prefix lengths"
offset = int(host) - int(real.network_address)
print(ipaddress.ip_address(int(virt.network_address) + offset))
PY
)"
echo "  far LAN: <discovered>/$PLEN on ${LAN_DEV:-?}"
echo "  netmap:  real LAN exposed as $VIRT_CIDR, gateway host as $VM_VIRT"
echo
# WHAT THE `lanaddr` ARM DOES AND DOES NOT PROVE -- stated here rather than
# implied by a column heading. A packet addressed to the GATEWAY'S OWN LAN
# address is delivered locally: it traverses PREROUTING and INPUT, never
# FORWARD. So this arm prices the extra routing lookup and -- in the netmap arm
# -- the full nft rewrite, which does run in PREROUTING and therefore IS
# measured. It does NOT price the FORWARD hop, and calling it "gateway
# throughput" would be the kind of label that survives into a summary and
# becomes a claim. The FORWARD chain needs a third host, which is what LAN_HOST
# is for; without one the forwarding probe is SKIPPED and said so, never
# silently reported as passing.
if [ -z "$LAN_HOST" ]; then
    echo "  NOTE: LAN_HOST unset -- the FORWARD-chain probe is skipped."
    echo "        The lanaddr/netmap arms below price PREROUTING + the rewrite, NOT forwarding."
    echo
fi

# The far end's FORWARD policy decides whether --forward-accept has anything to
# do. Reported rather than assumed: on a host that already accepts, the flag is
# a no-op and a "no regression" result would be vacuous.
FWD_POLICY="$(vm "sudo -n iptables -S FORWARD 2>/dev/null | head -1" 2>/dev/null | awk '{print $3}')"
echo "  far end FORWARD policy: ${FWD_POLICY:-unknown}"
echo

declare -A G
for k in overlay lanaddr netmap bare; do G[$k]=""; done

bare_arm() {
    [ "$(vm_iperf_server)" = 1 ] || { echo "    bare: FAILED(no iperf3 server)"; return; }
    local u; u="$(tcp_mbps "$BORE_VM" "$SECS" 1)"
    G[bare]="${G[bare]} $u"
    printf "    %-10s %8s Mbit/s\n" bare "$u"
}

# link_arm <name> <target-ip> <listen extra...>
# Brings a link up in ONE mode and measures ONE target through it. The tunnel is
# rebuilt per arm because the mode is a property of the link, not of the flow --
# there is no way to change `--advertise` on a live link, and pretending
# otherwise would measure the previous mode.
link_arm() {
    local name="$1" target="$2"; shift 2
    VPN_LINK_ID="${VPN_RUN_ID}m$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()

    vm_up listen "$@"
    sleep 3
    ws_up mod connect --accept-all-routes
    if ! ws_ready mod 60 >/dev/null; then
        echo "    $name: FAILED(link never came up)"; vpn_cleanup; sleep 2; return
    fi
    if [ "$(wait_path mod direct 95)" != direct ]; then
        echo "    $name: FAILED(stayed on relay -- a relay number next to direct ones would be a lie)"
        vpn_cleanup; sleep 2; return
    fi
    # An arm whose TUN MTU never stopped moving is NOT a sample: inner goodput is
    # proportional to MSS (Mathis), and a fresh link climbs 1350 -> 1288 -> 1414
    # over ~25 s. `wait_mtu_settle` returns 1 when it gives up, and this call
    # used to discard the value AND the status -- so a link that never settled
    # was measured anyway and nothing said so. Measured consequence, wired
    # 2026-09-12: `vpn_cc_matrix` and `vpn_udpbuf` each produced samples ~4 %
    # low whose `rtt min` was 17-22 ms against 31 ms for every settled sample,
    # two disjoint groups splitting ACROSS the arms rather than along them.
    ARM_MTU="$(wait_mtu_settle mod 24 90)" || {
        echo "    $name: FAILED(mtu never settled, last=$ARM_MTU) -- not averaged in"
        vpn_cleanup; sleep 2; return; }

    # The route is READ BACK, never assumed: a mode whose route did not install
    # would send the traffic out the default interface and return a perfectly
    # respectable number that has nothing to do with the tunnel. This is the
    # single most likely way for this stage to lie.
    local dev; dev="$(ip route get "$target" 2>/dev/null | grep -oE 'dev [a-z0-9]+' | head -1 | awk '{print $2}')"
    case "$dev" in
        bore*) ;;
        *) echo "    $name: FAILED(route to target goes via ${dev:-none}, not the tunnel)"
           vpn_cleanup; sleep 2; return ;;
    esac

    if [ "$(vm_iperf_server)" != 1 ]; then
        echo "    $name: FAILED(no iperf3 server)"; vpn_cleanup; sleep 2; return
    fi
    # Reachability first, throughput second: 0 Mbit/s and "unreachable" are
    # different findings and only the second one names the FORWARD chain.
    if ! ping -c 3 -W 3 -q "$target" >/dev/null 2>&1; then
        echo "    $name: FAILED(target unreachable through the tunnel -- forwarding, not bandwidth)"
        vpn_cleanup; sleep 2; return
    fi
    local u; u="$(tcp_mbps "$target" "$SECS" 1)"
    G[$name]="${G[$name]} ${u:-0}"
    printf "    %-10s %8s Mbit/s  (via %s)\n" "$name" "${u:-0}" "$dev"

    vpn_cleanup
    sleep 3
}

for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r ---"
    bare_arm
    # Control: the peer's own overlay address, same link shape, no forwarding.
    link_arm overlay "$B_PEER"
    # Gateway: the SAME host, addressed on its LAN address, so the only
    # difference from the control is the forwarding hop.
    link_arm lanaddr "$VM_PRIV" --advertise "$LAN_CIDR" --nat-masquerade
    # Netmap: the same again, plus the stateless 1:1 rewrite.
    link_arm netmap  "$VM_VIRT" --advertise "$LAN_CIDR@$VIRT_CIDR" --nat-masquerade
done

# --- the FORWARD chain, which needs a third host ----------------------------
# Two probes, in the order that makes the second one mean something:
#   1. reachability of a LAN host BEHIND the gateway   -> forwarding works
#   2. the same with the far end set to FORWARD DROP   -> --forward-accept
# Reachability is ICMP only. Throughput to a host that is not part of this
# benchmark is someone else's traffic, and a stage that helps itself to it would
# be a worse citizen than the deficit it is chasing is a problem.
if [ -n "$LAN_HOST" ]; then
    echo
    echo "  --- FORWARD chain, via LAN_HOST behind the gateway ---"
    fwd_probe() { # <label> <listen extra...>
        local lab="$1"; shift
        VPN_LINK_ID="${VPN_RUN_ID}f$(date +%s%N | tail -c 5)"
        VPN_WS_TAGS=(); VPN_VM_IDS=()
        vm_up listen --advertise "$LAN_CIDR" --nat-masquerade "$@"
        sleep 3
        ws_up mod connect --accept-all-routes
        if ! ws_ready mod 60 >/dev/null; then
            echo "    $lab: FAILED(link never came up)"; vpn_cleanup; sleep 2; return
        fi
        local dev; dev="$(ip route get "$LAN_HOST" 2>/dev/null | grep -oE 'dev [a-z0-9]+' | head -1 | awk '{print $2}')"
        case "$dev" in
            bore*) ;;
            *) echo "    $lab: FAILED(route to LAN_HOST goes via ${dev:-none}, not the tunnel)"
               vpn_cleanup; sleep 2; return ;;
        esac
        if ping -c 3 -W 3 -q "$LAN_HOST" >/dev/null 2>&1; then
            echo "    $lab: LAN host REACHABLE through the gateway (via $dev)"
        else
            echo "    $lab: LAN host unreachable (forwarding refused or no such host)"
        fi
        vpn_cleanup; sleep 3
    }
    fwd_probe "policy ${FWD_POLICY:-unknown}, no flag"

    # Changing the far end's firewall policy is off by default and restored in a
    # trap: an interrupted run must not leave the VM refusing to forward. Only
    # meaningful where the policy is currently ACCEPT -- flipping a host that
    # already drops would prove nothing and restore the wrong value.
    if [ "$FORWARD_DENY" = 1 ] && [ "${FWD_POLICY:-}" = ACCEPT ]; then
        echo "    (setting far end FORWARD policy to DROP; restored on exit)"
        restore_fwd() { vm "sudo -n iptables -P FORWARD ACCEPT 2>/dev/null; true" >/dev/null 2>&1; }
        trap 'restore_fwd; vpn_cleanup; vpn_assert_clean; exit 130' INT TERM
        trap 'restore_fwd; vpn_cleanup; vpn_assert_clean' EXIT
        vm "sudo -n iptables -P FORWARD DROP" >/dev/null 2>&1
        fwd_probe "policy DROP, no flag        (must be UNREACHABLE)"
        fwd_probe "policy DROP, --forward-accept (must be REACHABLE)" --forward-accept
        restore_fwd
        echo "    far end FORWARD policy restored to ACCEPT: $(vm "sudo -n iptables -S FORWARD 2>/dev/null | head -1" 2>/dev/null | awk '{print $3}')"
        trap 'vpn_cleanup; vpn_assert_clean; exit 130' INT TERM
        trap 'vpn_cleanup; vpn_assert_clean' EXIT
    elif [ "$FORWARD_DENY" = 1 ]; then
        echo "    (FORWARD_DENY=1 ignored: far end policy is ${FWD_POLICY:-unknown}, not ACCEPT)"
    fi
fi

echo
med() { printf '%s\n' $1 | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
echo "  medians:"
bm="$(med "${G[bare]}")"
om="$(med "${G[overlay]}")"
printf "    %-10s %8s Mbit/s\n" bare "$bm"
for k in overlay lanaddr netmap; do
    m="$(med "${G[$k]}")"
    awk -v k="$k" -v m="$m" -v base="$bm" -v ov="$om" 'BEGIN{
        pb = (base+0>0 && m+0>0) ? 100*m/base : 0;
        po = (ov+0>0   && m+0>0) ? 100*m/ov   : 0;
        printf "    %-10s %8s Mbit/s (%5.1f%% of bare, %5.1f%% of the plain overlay)\n", k, m, pb, po }'
done
echo "    raw samples:"
for k in bare overlay lanaddr netmap; do printf "      %-10s %s\n" "$k" "${G[$k]:-none}"; done
echo
echo "DONE"
