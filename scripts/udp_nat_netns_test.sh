#!/usr/bin/env bash
# UDP NAT-traversal netns smoke — plan Fase 0 (real kernel NAT, full binaries)
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Complements the deterministic userspace NAT lab (tests/nat_traversal_test.rs):
# the lab covers the RFC 4787 profile matrix in-process; this script proves the
# SAME end-to-end flow (bore server + provider + consumer binaries) across two
# REAL netfilter NATs, with routing, conntrack and ICMP in the path.
#
# Topology (double NAT):
#   nsprov(10.1.0.2) ─ nsnat1{10.1.0.1 | 192.0.2.1 masq} ─┐
#                                                          ns0 "internet"
#   nscli (10.2.0.2) ─ nsnat2{10.2.0.1 | 192.0.3.1 masq} ─┘ (bore server --udp)
#
# Scenarios:
#   T-NAT-DIRECT        default masquerade both sides (EIM-ish + APDF)
#                       → hole-punch succeeds, data over the DIRECT path
#   T-NAT-RANDOM-RELAY  fully-random masquerade both sides (APDM-ish)
#                       → punch fails, data still flows over the RELAY
#   T-NAT-BLOCKED-RELAY UDP dropped on the consumer NAT
#                       → discovery fails, data still flows over the RELAY
#
# Usage: sudo scripts/udp_nat_netns_test.sh
# Exit code: 0 = all tests passed, nonzero = failures

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release" >&2
    exit 1
fi
if find "$(dirname "$0")/../src" "$(dirname "$0")/../Cargo.toml" \
        -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release" >&2
    exit 1
fi

for cmd in ip nft nc socat; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

# ── Configuration ───────────────────────────────────────────────────────────
SECRET="natsmoke$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP="192.0.2.100"        # ns0 side of the nsnat1 link (server bind)
SERVER_IP2="192.0.3.100"       # ns0 side of the nsnat2 link
CTRL_PORT="7835"
ECHO_PORT="9111"
PROXY_PORT="9555"
# Fixed dir, wiped at START (not exit) so a failed run leaves its logs behind.
TMPDIR="/tmp/bore_udpnat_last"
PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; exit 1; }

# ── Cleanup ─────────────────────────────────────────────────────────────────
cleanup() {
    set +e
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for v in vn1w vn2w vp1l vc2l; do ip link del "$v" 2>/dev/null; done
    set -e
}
trap cleanup EXIT INT TERM

rm -rf "$TMPDIR"
mkdir -p "$TMPDIR"

# ── Topology ────────────────────────────────────────────────────────────────
build_topology() {
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do ip netns add "$ns"; done
    for ns in ns0 nsnat1 nsnat2 nsprov nscli; do
        ip -n "$ns" link set lo up
    done

    # nsnat1 WAN ↔ ns0
    ip link add vn1w type veth peer name vn1s
    ip link set vn1w netns nsnat1; ip link set vn1s netns ns0
    ip -n nsnat1 addr add 192.0.2.1/24 dev vn1w
    ip -n ns0    addr add "$SERVER_IP/24" dev vn1s
    ip -n nsnat1 link set vn1w up; ip -n ns0 link set vn1s up

    # nsnat2 WAN ↔ ns0
    ip link add vn2w type veth peer name vn2s
    ip link set vn2w netns nsnat2; ip link set vn2s netns ns0
    ip -n nsnat2 addr add 192.0.3.1/24 dev vn2w
    ip -n ns0    addr add "$SERVER_IP2/24" dev vn2s
    ip -n nsnat2 link set vn2w up; ip -n ns0 link set vn2s up

    # nsprov LAN ↔ nsnat1
    ip link add vp1l type veth peer name vp1n
    ip link set vp1l netns nsprov; ip link set vp1n netns nsnat1
    ip -n nsprov addr add 10.1.0.2/24 dev vp1l
    ip -n nsnat1 addr add 10.1.0.1/24 dev vp1n
    ip -n nsprov link set vp1l up; ip -n nsnat1 link set vp1n up

    # nscli LAN ↔ nsnat2
    ip link add vc2l type veth peer name vc2n
    ip link set vc2l netns nscli; ip link set vc2n netns nsnat2
    ip -n nscli  addr add 10.2.0.2/24 dev vc2l
    ip -n nsnat2 addr add 10.2.0.1/24 dev vc2n
    ip -n nscli link set vc2l up; ip -n nsnat2 link set vc2n up

    # Routing: peers default via their NAT; NATs default via ns0; ns0 forwards.
    ip -n nsprov route add default via 10.1.0.1
    ip -n nscli  route add default via 10.2.0.1
    ip -n nsnat1 route add default via "$SERVER_IP"
    ip -n nsnat2 route add default via "$SERVER_IP2"
    ip netns exec ns0    sysctl -qw net.ipv4.ip_forward=1
    ip netns exec nsnat1 sysctl -qw net.ipv4.ip_forward=1
    ip netns exec nsnat2 sysctl -qw net.ipv4.ip_forward=1
}

# nat_rules <ns> <wan-if> <mode: default|random>
nat_rules() {
    local ns="$1" wan="$2" mode="$3"
    ip netns exec "$ns" nft flush ruleset
    ip netns exec "$ns" nft add table ip nat
    ip netns exec "$ns" nft add chain ip nat postrouting '{ type nat hook postrouting priority 100 ; }'
    if [ "$mode" = "random" ]; then
        # Fully-random per-flow port allocation ≈ endpoint-dependent mapping.
        ip netns exec "$ns" nft add rule ip nat postrouting oifname "$wan" masquerade fully-random
    else
        ip netns exec "$ns" nft add rule ip nat postrouting oifname "$wan" masquerade
    fi
    # Realistic router INPUT policy: DROP unsolicited WAN datagrams addressed
    # to the router itself. Without this, a peer's punch that wins the
    # crossfire race lands in the router's INPUT conntrack and CLAIMS the
    # reply tuple — the inside peer's own mapping then gets remapped to a
    # random port (observed live: advertised :59246, remapped :49254) and the
    # hole-punch deadlocks. A dropped packet's conntrack entry is never
    # confirmed, so port preservation survives. Home routers behave this way.
    ip netns exec "$ns" nft add table ip filter
    ip netns exec "$ns" nft add chain ip filter input '{ type filter hook input priority 0 ; }'
    ip netns exec "$ns" nft add rule ip filter input ct state established,related accept
    ip netns exec "$ns" nft add rule ip filter input iifname "$wan" ip protocol udp drop
}

block_udp() {
    local ns="$1" wan="$2"
    ip netns exec "$ns" nft add table ip filter
    ip netns exec "$ns" nft add chain ip filter forward '{ type filter hook forward priority 0 ; }'
    ip netns exec "$ns" nft add rule ip filter forward oifname "$wan" ip protocol udp drop
}

flush_conntrack() {
    ip netns exec "$1" conntrack -F 2>/dev/null || true
}

wait_tcp() {
    local ns="$1" ip="$2" port="$3"
    for _ in $(seq 1 50); do
        if ip netns exec "$ns" nc -z -w1 "$ip" "$port" 2>/dev/null; then return 0; fi
        sleep 0.2
    done
    return 1
}

# run_scenario <label> <nat-mode> <block-consumer-udp: yes|no> <expect: direct|relay>
run_scenario() {
    local label="$1" mode="$2" block="$3" expect="$4"
    local id="udpnat-${label}"
    local sdir="$TMPDIR/$label"
    mkdir -p "$sdir"

    nat_rules nsnat1 vn1w "$mode"
    nat_rules nsnat2 vn2w "$mode"
    if [ "$block" = "yes" ]; then block_udp nsnat2 vn2w; fi
    flush_conntrack nsnat1
    flush_conntrack nsnat2

    # Echo service in nsprov.
    ip netns exec nsprov socat "TCP-LISTEN:$ECHO_PORT,reuseaddr,fork" PIPE &
    local echo_pid=$!
    sleep 0.3

    # Provider (bore local, secret tunnel, --udp). STUN target = the server IP
    # on the provider's own side: ns0 is multihomed and an unconnected UDP
    # reply picks its source by route, so a cross-side STUN target would answer
    # from the "wrong" IP and be discarded by the source check.
    ip netns exec nsprov env RUST_LOG=info "$BORE" local "$ECHO_PORT" \
        --to "http://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" \
        --tcp-secret-id "$id" --udp \
        --stun-server "$SERVER_IP:$CTRL_PORT" \
        >"$sdir/provider.log" 2>&1 &
    local prov_pid=$!
    sleep 1.5

    # Consumer (bore proxy, --udp). STUN = server IP on the consumer's side
    # (see the provider note above).
    ip netns exec nscli env RUST_LOG=info "$BORE" proxy \
        --to "http://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" \
        --tcp-secret-id "$id" --udp \
        --stun-server "$SERVER_IP2:$CTRL_PORT" \
        --local-proxy-port "127.0.0.1:$PROXY_PORT" \
        >"$sdir/consumer.log" 2>&1 &
    local cons_pid=$!

    if ! wait_tcp nscli 127.0.0.1 "$PROXY_PORT"; then
        fail "$label: consumer proxy port never came up"
        kill -9 "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
        return
    fi
    # Give the direct-path negotiation time to settle before probing.
    sleep 2

    local got
    got=$(ip netns exec nscli sh -c "printf 'hello-nat\n' | nc -w3 127.0.0.1 $PROXY_PORT" 2>/dev/null | head -1)
    if [ "$got" = "hello-nat" ]; then
        pass "$label: end-to-end data through double NAT"
    else
        fail "$label: no echo through tunnel (got '$got')"
    fi

    sleep 0.5
    local cons_direct prov_direct
    cons_direct=$(grep -c "direct udp connection established (consumer" "$sdir/consumer.log" || true)
    prov_direct=$(grep -c "accepted direct udp connection (provider" "$sdir/provider.log" || true)

    if [ "$expect" = "direct" ]; then
        if [ "$cons_direct" -ge 1 ] && [ "$prov_direct" -ge 1 ]; then
            pass "$label: direct path established (consumer+provider logs)"
        else
            fail "$label: expected DIRECT path (consumer=$cons_direct provider=$prov_direct)"
        fi
    else
        if [ "$cons_direct" -eq 0 ]; then
            pass "$label: stayed on relay as expected"
        else
            fail "$label: unexpected direct path (NAT scenario should force relay)"
        fi
        if grep -qE "falling back to relay|fallback_reason|direct path unavailable|UdpUnavailable|no STUN response|all STUN probes failed" \
            "$sdir/consumer.log"; then
            pass "$label: consumer logged an explicit relay-fallback reason"
        else
            fail "$label: no relay-fallback reason in consumer log"
        fi
    fi

    kill -9 "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
    wait "$echo_pid" "$prov_pid" "$cons_pid" 2>/dev/null || true
    sleep 0.3
}

# ── Run ─────────────────────────────────────────────────────────────────────
build_topology

ip netns exec ns0 env RUST_LOG=info "$BORE" server \
    --secret "$SECRET" --udp >"$TMPDIR/server.log" 2>&1 &
SERVER_PID=$!
if ! wait_tcp ns0 "$SERVER_IP" "$CTRL_PORT"; then
    die "bore server never came up (see $TMPDIR/server.log)"
fi

run_scenario "T-NAT-DIRECT" default no direct
run_scenario "T-NAT-RANDOM-RELAY" random no relay
run_scenario "T-NAT-BLOCKED-RELAY" default yes relay

kill -9 "$SERVER_PID" 2>/dev/null || true

echo
echo "── Results ──────────────────────────────────────────"
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
