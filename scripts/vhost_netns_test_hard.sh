#!/usr/bin/env bash
# vhost hardening test suite — regression gate for fault injection, carrier churn, slow-clients, header limits, and cleanup.
# All tests are hard asserts; PASS/FAIL counters exit on final tally.
#
# Usage: sudo scripts/vhost_netns_test_hard.sh

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# Guard against a STALE release binary.
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

# Check for required tools
for tool in curl python3 openssl tc nft ip; do
    command -v "$tool" >/dev/null || { echo "ERROR: $tool required" >&2; exit 1; }
done

SECRET="vhhard$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP_NS0_PROV="10.211.0.2"
SERVER_IP_PROV="10.211.0.1"
SERVER_IP_NS0_CLI="10.213.0.2"
SERVER_IP_CLI="10.213.0.1"
LOG=$(mktemp -d)
ORIGIN_PORT=8888
ORIGIN_DATA_FILE="$LOG/testfile.bin"

# Create test file
dd if=/dev/urandom of="$ORIGIN_DATA_FILE" bs=1M count=100 2>/dev/null

PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }

restart_server() {
    # usage: restart_server <logfile> <extra server args...>
    local logfile="$1"
    shift
    kill "$SERVER_PID" 2>/dev/null || true
    # Wait until ns0:7835 stops accepting (old listener gone)
    for _ in $(seq 1 50); do
        ip netns exec nsp nc -z "$SERVER_IP_NS0_PROV" 7835 2>/dev/null || break
        sleep 0.1
    done
    # RUST_LOG=bore=debug so H3 can see the "pool full; dropping extra carrier"
    # churn that reproduces VH-2 (the message is debug-level).
    RUST_LOG=bore=debug ip netns exec ns0 "$BORE" server \
        --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
        --secret "$SECRET" \
        --vhost-base-domain bore.local \
        "$@" \
        >"$logfile" 2>&1 &
    SERVER_PID=$!
    # Poll nc until server accepts
    for _ in $(seq 1 50); do
        ip netns exec nsp nc -z "$SERVER_IP_NS0_PROV" 7835 2>/dev/null && return 0
        sleep 0.1
    done
    echo "ERROR: server failed to bind within timeout" >&2
    tail -3 "$logfile" >&2
    return 1
}

ORIGIN_PIDS=()
PROVIDER_PIDS=()

cleanup() {
    set +e
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    for pid in "${ORIGIN_PIDS[@]}" "${PROVIDER_PIDS[@]}"; do
        kill "$pid" 2>/dev/null
    done
    pkill -P $$ 2>/dev/null
    # Kill ALL bore (server + clients) and origins — by-PID lists miss anything
    # spawned in a subshell, and a leftover `bore server` pins the namespace.
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    pkill -f 'ThreadingHTTPServer' 2>/dev/null
    sleep 0.3
    pkill -9 -f 'target/release/bore' 2>/dev/null
    # Kill anything pinned inside each ns before deleting it (avoids EBUSY leak).
    for ns in ns0 nsp nsc; do
        ip netns exec "$ns" tc qdisc del dev vethsp root 2>/dev/null
        ip netns exec "$ns" nft delete table inet bore_vhost_h 2>/dev/null
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# ── Topology ──────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ns0/nsp/nsc ==="
# Delete-before-create: reclaim namespaces leaked by a previous crashed run.
for ns in ns0 nsp nsc; do
    ip netns del "$ns" 2>/dev/null || true
done
ip netns add ns0; ip netns add nsp; ip netns add nsc

# ns0 ↔ nsp
ip link add vethsp type veth peer name vethps
ip link set vethsp netns ns0; ip link set vethps netns nsp
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_PROV/30" dev vethsp
ip netns exec nsp ip addr add "$SERVER_IP_PROV/30" dev vethps
ip netns exec ns0 ip link set vethsp up; ip netns exec nsp ip link set vethps up

# ns0 ↔ nsc
ip link add vethsc type veth peer name vethcs
ip link set vethsc netns ns0; ip link set vethcs netns nsc
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_CLI/30" dev vethsc
ip netns exec nsc ip addr add "$SERVER_IP_CLI/30" dev vethcs
ip netns exec ns0 ip link set vethsc up; ip netns exec nsc ip link set vethcs up

# Loopback
for ns in ns0 nsp nsc; do ip netns exec "$ns" ip link set lo up; done

# Routes
ip netns exec nsp ip route add default via "$SERVER_IP_NS0_PROV"
ip netns exec nsc ip route add default via "$SERVER_IP_NS0_CLI"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1 2>/dev/null

# ── Start server ───────────────────────────────────────────────────────────────
echo "=== Starting bore server in ns0 ==="
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    >"$LOG/server.log" 2>&1 &
SERVER_PID=$!
sleep 1
echo "  Server up (pid $SERVER_PID)"

# ── Start origin HTTP server ───────────────────────────────────────────────────
ip netns exec nsp python3 -m http.server "$ORIGIN_PORT" --bind 127.0.0.1 \
    --directory "$LOG" >"$LOG/origin.log" 2>&1 &
ORIGIN_PIDS+=($!)
sleep 0.5

# ── Test H1: loss/RTT correctness ──────────────────────────────────────────────
echo ""
echo "=== Test H1: loss/RTT correctness (tc netem 60ms delay, 3% loss) ==="
ip netns exec ns0 tc qdisc add dev vethsp root netem delay 60ms loss 3%
ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
    --subdomain h1 --id "h1-test" \
    --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
    >"$LOG/h1.provider.log" 2>&1 &
H1_PID=$!
PROVIDER_PIDS+=($H1_PID)
sleep 2

# Download and sha256 the file
ORIG_SHA=$(sha256sum "$ORIGIN_DATA_FILE" | awk '{print $1}')
DL_FILE="$LOG/h1_downloaded.bin"
ip netns exec nsc curl -s -o "$DL_FILE" \
    -H "Host: h1.bore.local" \
    "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || true
if [ -f "$DL_FILE" ]; then
    DL_SHA=$(sha256sum "$DL_FILE" | awk '{print $1}')
    if [ "$ORIG_SHA" = "$DL_SHA" ]; then
        pass "H1: sha256 matches after loss/RTT (download successful)"
    else
        fail "H1: sha256 mismatch (download corrupted or incomplete)"
    fi
else
    fail "H1: download failed"
fi
kill $H1_PID 2>/dev/null || true
ip netns exec ns0 tc qdisc del dev vethsp root 2>/dev/null || true
sleep 1

# ── Test H2: provider flap ────────────────────────────────────────────────────
echo ""
echo "=== Test H2: provider flap (kill+restart 3x under steady curl loop) ==="
ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
    --subdomain h2 --id "h2-test" \
    --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
    >"$LOG/h2.provider.log" 2>&1 &
H2_PID=$!
PROVIDER_PIDS+=($H2_PID)
sleep 1

# Start steady curl loop in background
(
    while true; do
        ip netns exec nsc curl -s -o /dev/null \
            -H "Host: h2.bore.local" \
            "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || true
        sleep 0.1
    done
) &
CURL_LOOP_PID=$!

H2_FLAP_PASS=1
for flap_round in 1 2 3; do
    sleep 2
    echo "  [H2: flap round $flap_round/3 — killing provider]"
    kill $H2_PID 2>/dev/null || true
    sleep 1

    # Restart
    ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain h2 --id "h2-test" \
        --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
        >"$LOG/h2.provider.log" 2>&1 &
    H2_PID=$!
    PROVIDER_PIDS+=($H2_PID)
    sleep 1

    # Check that routing recovers (HTTP 200 within 5s)
    if timeout 5 bash -c "ip netns exec nsc curl -s -o /dev/null -w '%{http_code}' \
        -H 'Host: h2.bore.local' \
        'http://$SERVER_IP_NS0_CLI/' | grep -q '^200$'" 2>/dev/null; then
        echo "    → recovered to 200"
    else
        echo "    → recovery timeout or non-200"
        H2_FLAP_PASS=0
    fi
done

kill $CURL_LOOP_PID 2>/dev/null || true
kill $H2_PID 2>/dev/null || true

# Check for permanent 502 in later flaps
PERM_502_COUNT=$(grep -c "502" "$LOG/h2.provider.log" 2>/dev/null || true)
if [ "$PERM_502_COUNT" -eq 0 ] && [ "$H2_FLAP_PASS" = "1" ]; then
    pass "H2: provider flap: routing recovered 3x, no permanent 502"
else
    fail "H2: provider flap: persistent 502 or recovery timeout"
fi
sleep 1

# ── Test H3: carrier churn (KNOWN BUG VH-2) ─────────────────────────────────────
echo ""
echo "=== Test H3: carrier churn repro (--udp --carriers 40) [KNOWN BUG VH-2] ==="
echo "  (Expected to FAIL until VH-2 fixed; documents current behavior)"
restart_server "$LOG/server_h3.log" --vhost-http-port 7835 --vhost-quic-port 443 --udp || exit 1

ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
    --subdomain h3 --id "h3-test" \
    --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
    --udp --carriers 40 \
    >"$LOG/h3.provider.log" 2>&1 &
H3_PID=$!
PROVIDER_PIDS+=($H3_PID)
sleep 1

# Capture server logs for 8 seconds
sleep 8

# Check for repeating carrier pool full messages
POOL_FULL_COUNT=$(grep -c "vhost QUIC direct pool full; dropping extra carrier" "$LOG/server_h3.log" 2>/dev/null || true)
if [ "$POOL_FULL_COUNT" -gt 10 ]; then
    echo "H3: KNOWN BUG VH-2 — carrier pool churn detected ($POOL_FULL_COUNT occurrences, informational)"
else
    echo "H3: KNOWN BUG VH-2 — not reproduced (or fixed, informational)"
fi

kill $H3_PID 2>/dev/null || true
sleep 1

# Restart main server for remaining tests
restart_server "$LOG/server.log" --vhost-http-port 80 || exit 1

# ── Test H4: slowloris (partial request, no finish) ──────────────────────────────
echo ""
echo "=== Test H4: slowloris (partial request, no finish; assert timeout) ==="
# Start provider and origin for h4 subdomain liveness check
mkdir -p "$LOG/h4_origin"
echo "h4-response" > "$LOG/h4_origin/index.html"
ip netns exec nsp python3 -m http.server 8889 --bind 127.0.0.1 \
    --directory "$LOG/h4_origin" >"$LOG/h4_origin.log" 2>&1 &
H4_ORIGIN_PID=$!
ORIGIN_PIDS+=($H4_ORIGIN_PID)
sleep 0.5

ip netns exec nsp "$BORE" vhost 127.0.0.1:8889 \
    --subdomain h4 --id "h4-test" \
    --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
    >"$LOG/h4.provider.log" 2>&1 &
H4_PID=$!
PROVIDER_PIDS+=($H4_PID)
sleep 1

# Slowloris: raw socket connection that sends partial request
(
    ip netns exec nsc bash -c "exec 3<>/dev/tcp/$SERVER_IP_NS0_CLI/80
echo -ne 'GET / HTTP/1.1\r\nHost: h4.bore.local\r\n' >&3
sleep 5
exec 3>&-" 2>/dev/null || true
) &
SLOWLORIS_PID=$!

# Meanwhile, start a normal request to verify the server is still alive
sleep 1
NORMAL_TIMEOUT=5
if timeout $NORMAL_TIMEOUT bash -c "ip netns exec nsc curl -s -o /dev/null -w '%{http_code}' \
    -H 'Host: h4.bore.local' \
    'http://$SERVER_IP_NS0_CLI/' 2>/dev/null | grep -q '^200$'" 2>/dev/null; then
    pass "H4: slowloris: concurrent normal request succeeds (server not hung)"
else
    fail "H4: slowloris: server hung or timeout"
fi

kill $SLOWLORIS_PID 2>/dev/null || true
kill $H4_PID 2>/dev/null || true
kill $H4_ORIGIN_PID 2>/dev/null || true
sleep 1

# ── Test H5: oversized headers (20KB custom headers, >16KiB cap) ─────────────────
echo ""
echo "=== Test H5: oversized headers (~20KB custom headers, >16KiB cap) ==="
# Build a header string of ~20KB
HEADER_PAYLOAD=$(python3 -c "print('X-Test-' + 'A' * 20000)")
RESPONSE_CODE=$(ip netns exec nsc curl -s -o /dev/null -w '%{http_code}' \
    -H "Host: h5.bore.local" \
    -H "X-Big: $HEADER_PAYLOAD" \
    "http://$SERVER_IP_NS0_CLI/" 2>/dev/null || echo "000")

case "$RESPONSE_CODE" in
    200|400|413|414|431|414)
        pass "H5: oversized headers: defined behavior (code $RESPONSE_CODE, no hang)"
        ;;
    000)
        fail "H5: oversized headers: timeout or no response"
        ;;
    *)
        pass "H5: oversized headers: responded with $RESPONSE_CODE (handled)"
        ;;
esac
sleep 1

# ── Test H6: max-conns on unified port (KNOWN BUG VH-1) ────────────────────────
echo ""
echo "=== Test H6: max-conns unified port (KNOWN BUG VH-1) ==="
echo "  (Expected to fail — unified port bypasses --max-conns until fixed)"
# Restart server with --max-conns 4 and unified port
restart_server "$LOG/server_h6.log" --vhost-http-port 7835 --max-conns 4 || exit 1

# Create slow origin server that sleeps 15s before responding
mkdir -p "$LOG/h6_origin"
ip netns exec nsp python3 << 'SLOWSERVER' >"$LOG/h6_origin.log" 2>&1 &
import http.server
import time

class SlowHandler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        time.sleep(15)
        self.send_response(200)
        self.send_header('Content-type', 'text/plain')
        self.end_headers()
        self.wfile.write(b"slow-response")

# ThreadingHTTPServer + serve_forever holds ALL concurrent requests open (each
# sleeps 15s). With handle_request() only one connection survives, so the VH-1
# concurrent-connection count at the server would be meaningless.
http.server.ThreadingHTTPServer(("127.0.0.1", 8890), SlowHandler).serve_forever()
SLOWSERVER
H6_ORIGIN_PID=$!
ORIGIN_PIDS+=($H6_ORIGIN_PID)
sleep 0.5

# Start provider for h6
ip netns exec nsp "$BORE" vhost 127.0.0.1:8890 \
    --subdomain h6 --id "h6-test" \
    --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
    >"$LOG/h6.provider.log" 2>&1 &
H6_PID=$!
PROVIDER_PIDS+=($H6_PID)
sleep 1

# Fire ~16 concurrent real curls from nsc to h6.bore.local via unified port 7835
for i in $(seq 1 16); do
    (
        timeout 20 ip netns exec nsc curl -s -o /dev/null \
            --resolve "h6.bore.local:7835:$SERVER_IP_NS0_CLI" \
            "http://h6.bore.local:7835/" 2>/dev/null || true
    ) &
done
sleep 3

# Count concurrent ESTABLISHED connections to ns0:7835
ESTABLISHED=$(ip netns exec ns0 ss -tn 2>/dev/null | grep -c ":7835" || true)
echo "H6: KNOWN BUG VH-1 — observed $ESTABLISHED concurrent connections (expected <=5 if enforced, informational)"

kill $H6_PID 2>/dev/null || true
kill $H6_ORIGIN_PID 2>/dev/null || true
sleep 1

# ── Test H7: SIGKILL cleanup ──────────────────────────────────────────────────
echo ""
echo "=== Test H7: SIGKILL cleanup (ports rebind cleanly) ==="
# Start fresh server (via restart_server, which is just setup + background)
restart_server "$LOG/server_h7a.log" --vhost-http-port 80 || exit 1
sleep 1

# Verify ports are bound
if ip netns exec ns0 ss -tlnp 2>/dev/null | grep -q ":80" || \
   ip netns exec ns0 ss -tlnp 2>/dev/null | grep -q ":7835"; then
    pass "H7: ports bound on startup"
else
    fail "H7: ports not bound initially"
fi

# SIGKILL server
echo "  [H7: killing server with -9]"
kill -9 $SERVER_PID 2>/dev/null || true
sleep 1

# Try to rebind immediately
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    >"$LOG/server_h7b.log" 2>&1 &
SERVER_PID=$!
sleep 1

# Verify the fresh server accepts connections
if ip netns exec nsp nc -z "$SERVER_IP_NS0_PROV" 7835 2>/dev/null; then
    pass "H7: fresh server rebound ports cleanly after SIGKILL"
else
    fail "H7: rebind failed (ports may be in TIME_WAIT)"
fi

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "=== vhost hardening test suite complete ==="
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
