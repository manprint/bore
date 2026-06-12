#!/usr/bin/env bash
# Quick debug: test a bore vhost provider in isolation (single veth pair).
# Run as root:  sudo scripts/vhost_debug.sh
set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"
SPID=""; VPID=""; HPID=""

cleanup() {
    set +e
    kill "$SPID" "$VPID" "$HPID" 2>/dev/null
    # Kill ALL bore + origin by pattern — a leftover `bore server` pins the ns.
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    sleep 0.2
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in dbg_ns0 dbg_ns1; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    ip link del vdbg_s 2>/dev/null
    rm -rf /tmp/vhost_debug_cert.pem /tmp/vhost_debug_key.pem /tmp/vhost_debug_origin 2>/dev/null
}
trap cleanup EXIT INT TERM

# Delete-before-create: reclaim namespaces leaked by a previous crashed run.
for ns in dbg_ns0 dbg_ns1; do
    ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
    ip netns del "$ns" 2>/dev/null || true
done

ip netns add dbg_ns0
ip netns add dbg_ns1
ip link add vdbg_s type veth peer name vdbg_p
ip link set vdbg_s netns dbg_ns0
ip link set vdbg_p netns dbg_ns1
ip netns exec dbg_ns0 ip addr add 10.210.0.1/30 dev vdbg_s
ip netns exec dbg_ns1 ip addr add 10.210.0.2/30 dev vdbg_p
ip netns exec dbg_ns0 ip link set vdbg_s up
ip netns exec dbg_ns1 ip link set vdbg_p up
ip netns exec dbg_ns0 ip link set lo up
ip netns exec dbg_ns1 ip link set lo up

# Self-signed wildcard cert.
openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout /tmp/vhost_debug_key.pem -out /tmp/vhost_debug_cert.pem \
    -days 1 -subj "/CN=bore.local" \
    -addext "subjectAltName=DNS:*.bore.local,DNS:bore.local" 2>/dev/null

# Origin with a test body.
mkdir -p /tmp/vhost_debug_origin
echo "debug-test-body" > /tmp/vhost_debug_origin/index.html

echo "=== Starting server (dbg_ns0) ==="
ip netns exec dbg_ns0 "$BORE" -v server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret debug \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --vhost-https-port 443 --vhost-cert-file /tmp/vhost_debug_cert.pem \
    --vhost-key-file /tmp/vhost_debug_key.pem \
    --vhost-mode both \
    --control-port 7835 &
SPID=$!
sleep 1
ip netns exec dbg_ns1 nc -z 10.210.0.1 7835 && echo "Server reachable" || echo "Server NOT reachable"

echo "=== Starting python3 origin (dbg_ns1, port 8000) ==="
ip netns exec dbg_ns1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory /tmp/vhost_debug_origin >/dev/null 2>&1 &
HPID=$!
sleep 0.5

echo "=== Starting bore vhost provider (dbg_ns1, timeout 8s) ==="
timeout 8 ip netns exec dbg_ns1 "$BORE" -v vhost 127.0.0.1:8000 \
    --subdomain test --id testuser \
    --to 10.210.0.1:7835 --secret debug &
VPID=$!
sleep 2

echo "=== Curl test (dbg_ns1 → server frontend) ==="
ip netns exec dbg_ns1 curl -s --resolve test.bore.local:80:10.210.0.1 \
    http://test.bore.local/ || echo "(curl failed)"

echo ""
echo "=== Waiting for provider timeout ==="
sleep 7
echo "(server/provider -v logs printed above; cleanup on EXIT)"
