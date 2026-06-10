#!/usr/bin/env bash
# Quick debug: test bore vpn listen in isolation
set -euo pipefail
BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

cleanup() {
    set +e
    kill "$SPID" "$LPID" 2>/dev/null
    ip netns del dbg_ns0 2>/dev/null
    ip netns del dbg_ns1 2>/dev/null
    ip link del vdbg_s 2>/dev/null
}
trap cleanup EXIT

ip netns add dbg_ns0
ip netns add dbg_ns1
ip link add vdbg_s type veth peer name vdbg_p
ip link set vdbg_s netns dbg_ns0
ip link set vdbg_p netns dbg_ns1
ip netns exec dbg_ns0 ip addr add 10.210.0.1/24 dev vdbg_s
ip netns exec dbg_ns1 ip addr add 10.210.0.2/24 dev vdbg_p
ip netns exec dbg_ns0 ip link set vdbg_s up
ip netns exec dbg_ns1 ip link set vdbg_p up
ip netns exec dbg_ns0 ip link set lo up
ip netns exec dbg_ns1 ip link set lo up

echo "=== Starting server ==="
ip netns exec dbg_ns0 "$BORE" server --secret S --vpn --vpn-pool 10.99.0.0/16 &
SPID=$!
sleep 0.5
ip netns exec dbg_ns1 nc -z 10.210.0.1 7835 && echo "Server reachable" || echo "Server NOT reachable"

echo "=== Starting bore vpn listen (2s timeout) ==="
timeout 8 ip netns exec dbg_ns1 "$BORE" -v vpn listen \
    --to 10.210.0.1 --secret S --id dbg &
LPID=$!
sleep 7
echo "=== Listen log ==="
kill $LPID 2>/dev/null || true
wait $LPID 2>/dev/null || true
echo "(listener finished)"
