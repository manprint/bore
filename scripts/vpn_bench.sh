#!/usr/bin/env bash
# VPN benchmark harness (Phase 4.4) — netns topology, run with sudo.
#
# Measures throughput/latency for the four data-plane configurations:
#   1. relay, 1 carrier        (--relay-only)
#   2. relay, 4 carriers       (--relay-only --carriers 4)
#   3. direct                  (default upgrade path)
#   4. direct, 4 TUN queues    (--tun-queues 4)
# across iperf3 TCP, iperf3 UDP (500M target), and ping -f latency/loss.
#
# Usage: sudo scripts/vpn_bench.sh [seconds-per-test (default 5)]
# Output: a markdown table on stdout (paste into docs/vpn/VPN.md, Performance).

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"
SECRET="vpnbench$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
POOL="10.99.0.0/16"
SERVER_IP_NS0_A="10.201.0.2"
SERVER_IP_NS1="10.201.0.1"
SERVER_IP_NS0_B="10.202.0.2"
SERVER_IP_NS2="10.202.0.1"
DUR="${1:-5}"
LOG=$(mktemp -d)

command -v iperf3 >/dev/null || { echo "iperf3 required" >&2; exit 1; }

cleanup() {
    set +e
    pkill -P $$ 2>/dev/null
    ip netns del ns0 2>/dev/null
    ip netns del ns1 2>/dev/null
    ip netns del ns2 2>/dev/null
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

# ── Topology (same as vpn_netns_test.sh) ──────────────────────────────────────
ip netns add ns0; ip netns add ns1; ip netns add ns2
ip link add veth0s type veth peer name veth0p
ip link set veth0s netns ns0; ip link set veth0p netns ns1
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_A/24" dev veth0s
ip netns exec ns1 ip addr add "$SERVER_IP_NS1/24" dev veth0p
ip netns exec ns0 ip link set veth0s up; ip netns exec ns1 ip link set veth0p up
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0; ip link set veth1p netns ns2
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_B/24" dev veth1s
ip netns exec ns2 ip addr add "$SERVER_IP_NS2/24" dev veth1p
ip netns exec ns0 ip link set veth1s up; ip netns exec ns2 ip link set veth1p up
for ns in ns0 ns1 ns2; do ip netns exec "$ns" ip link set lo up; done
ip netns exec ns1 ip route add default via "$SERVER_IP_NS0_A"
ip netns exec ns2 ip route add default via "$SERVER_IP_NS0_B"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1

ip netns exec ns0 "$BORE" server --secret "$SECRET" \
    --vpn --vpn-pool "$POOL" --vpn-max-links 16 --udp --bind-addr 0.0.0.0 \
    >"$LOG/server.log" 2>&1 &
sleep 1
STUN="$SERVER_IP_NS0_A:7835"

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-20}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# bench <name> <expect-pattern> <listener-extra-args...> -- <connector-extra-args...>
bench() {
    local name="$1" expect="$2"; shift 2
    local largs=() cargs=() in_c=0
    for a in "$@"; do
        if [ "$a" = "--" ]; then in_c=1; continue; fi
        [ "$in_c" = 0 ] && largs+=("$a") || cargs+=("$a")
    done

    ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "bench-$name" "${largs[@]}" >"$LOG/$name.l.log" 2>&1 &
    local LPID=$!
    sleep 0.5
    ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "bench-$name" "${cargs[@]}" >"$LOG/$name.c.log" 2>&1 &
    local CPID=$!

    if ! wait_for_log "$LOG/$name.l.log" "$expect" 25; then
        echo "| $name | SETUP FAILED | — | — | — |"
        kill $LPID $CPID 2>/dev/null; sleep 1; return
    fi
    sleep 1
    local OVL
    OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

    ip netns exec ns1 iperf3 -s -D --logfile /dev/null
    sleep 0.2
    local TCP UDP UDPLOSS LAT
    TCP=$(timeout $((DUR + 10)) ip netns exec ns2 iperf3 -c "$OVL" -t "$DUR" -J 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin); print(round(d['end']['sum_received']['bits_per_second']/1e6))" 2>/dev/null || echo "—")
    UDP=$(timeout $((DUR + 10)) ip netns exec ns2 iperf3 -c "$OVL" -t "$DUR" -u -b 500M -J 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin); s=d['end']['sum']; print(f\"{round(s['bits_per_second']/1e6)} ({s['lost_percent']:.1f}% loss)\")" 2>/dev/null || echo "—")
    ip netns exec ns1 pkill iperf3 2>/dev/null || true
    LAT=$(ip netns exec ns2 ping -c 50 -i 0.02 -q "$OVL" 2>/dev/null | \
        awk -F'/' '/rtt/ {print $5 " ms"}' || echo "—")

    echo "| $name | ${TCP} Mbps | ${UDP} Mbps | ${LAT} |"
    kill $LPID $CPID 2>/dev/null
    sleep 1.5
}

echo "## VPN data-plane benchmark ($(date -u +%F), netns, ${DUR}s per test)"
echo ""
echo "| Configuration | iperf3 TCP | iperf3 UDP 500M | ping avg |"
echo "|---|---|---|---|"
bench "relay-1c"   "vpn link paired"     --relay-only -- --relay-only
bench "relay-4c"   "vpn link paired"     --relay-only --carriers 4 -- --relay-only --carriers 4
bench "direct"     "upgraded to direct"  --stun-server "$STUN" -- --stun-server "$STUN"
bench "direct-4q"  "upgraded to direct"  --stun-server "$STUN" --tun-queues 4 -- --stun-server "$STUN" --tun-queues 4
echo ""
echo "Acceptance (Phase 4.4): no configuration regresses >5% vs baseline;"
echo "direct > relay in netns throughput; relay-4c >= relay-1c."
