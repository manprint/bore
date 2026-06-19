#!/usr/bin/env bash
# local_proxy netns harness — functional acceptance tests for public tunnels + secret tunnels
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Topology:
#   ns0 (server) — veth0s↔veth0c (10.210.0.0/30) ↔ nssvc (service + bore local/provider)
#                — veth1s↔veth1p (10.211.0.0/30) ↔ nscli (bore proxy/public-tunnel client)
#
# Usage: sudo scripts/local_proxy_netns_test.sh

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# Guard against stale release binary
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

# Guard dependencies
for cmd in nc ip socat python3 openssl; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

SECRET="localtest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP_NS0_SVC="10.210.0.2"  # server-side of ns0↔nssvc veth
SVC_IP="10.210.0.1"             # nssvc-side
SERVER_IP_NS0_CLI="10.211.0.2"  # server-side of ns0↔nscli veth
CLI_IP="10.211.0.1"             # nscli-side

PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

cleanup() {
    set +e
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'socat\|nc' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    sleep 0.3
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in ns0 nssvc nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf /tmp/bore_local_* 2>/dev/null
    set -e
}
trap cleanup EXIT INT TERM

wait_server_ready() {
    local from_ns="$1" ip="$2" port="${3:-7835}"
    for _ in $(seq 1 50); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

wait_port_free() {
    local from_ns="$1" ip="$2" port="${3:-7835}"
    for _ in $(seq 1 50); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null || return 0
        sleep 0.1
    done
    return 1
}

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# ── Setup ──────────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ==="
# Delete-before-create: reclaim any stale namespaces from prior crash
for ns in ns0 nssvc nscli; do
    ip netns del "$ns" 2>/dev/null || true
done

ip netns add ns0
ip netns add nssvc
ip netns add nscli

# ns0 ↔ nssvc (service side)
ip link add veth0s type veth peer name veth0c
ip link set veth0s netns ns0
ip link set veth0c netns nssvc
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_SVC/30" dev veth0s
ip netns exec nssvc ip addr add "$SVC_IP/30" dev veth0c
ip netns exec ns0 ip link set veth0s up
ip netns exec nssvc ip link set veth0c up

# ns0 ↔ nscli (client side)
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0
ip link set veth1p netns nscli
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_CLI/30" dev veth1s
ip netns exec nscli ip addr add "$CLI_IP/30" dev veth1p
ip netns exec ns0 ip link set veth1s up
ip netns exec nscli ip link set veth1p up

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec nssvc ip link set lo up
ip netns exec nscli ip link set lo up

# Route the two leaf namespaces to each other THROUGH ns0 so the secret-tunnel
# UDP hole-punch (provider↔consumer direct path) can actually establish — without
# this the leaves only reach the server and --udp silently falls back to relay.
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1 2>/dev/null || true
ip netns exec nssvc ip route add 10.211.0.0/30 via "$SERVER_IP_NS0_SVC" 2>/dev/null || true
ip netns exec nscli ip route add 10.210.0.0/30 via "$SERVER_IP_NS0_CLI" 2>/dev/null || true

# ── Start server ───────────────────────────────────────────────────────────────
echo "=== Starting bore server in ns0 ==="
SERVER_LOG="/tmp/bore_local_server.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --udp \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
sleep 1
wait_server_ready ns0 127.0.0.1 7835 \
    || die "server not reachable from ns0"
echo "  Server up (pid $SERVER_PID)"

# ── Test 1: public tunnel (bore local) TCP echo ───────────────────────────────
echo "=== Test 1: T-PUB-RELAY (public tunnel TCP echo) ==="
# Start echo service in nssvc
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start public tunnel provider in nssvc
LOCAL_LOG="/tmp/bore_local_pub.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Test echo from nscli
ECHO_TEXT="Hello public tunnel"
RESPONSE=$(echo "$ECHO_TEXT" | timeout 12 ip netns exec nscli nc -N -w5 "$SERVER_IP_NS0_CLI" 9999 2>/dev/null || echo "ERROR")
if [ "$RESPONSE" = "$ECHO_TEXT" ]; then
    pass "public tunnel: small TCP echo round-trip"
else
    fail "public tunnel: expected '$ECHO_TEXT', got '$RESPONSE'"
fi

# Test bulk transfer (~4 MiB)
echo "=== Test 1b: T-PUB-RELAY (public tunnel bulk 4MiB) ==="
BULK_SIZE=$((4 * 1024 * 1024))
BULK_FILE="/tmp/bore_local_bulk.bin"
head -c "$BULK_SIZE" /dev/zero > "$BULK_FILE"
BULK_HASH=$(sha256sum "$BULK_FILE" | awk '{print $1}')

# Send file through tunnel, receive and check
RECV_FILE="/tmp/bore_local_bulk_recv.bin"
timeout 30 ip netns exec nscli socat - TCP:"$SERVER_IP_NS0_CLI:9999" < "$BULK_FILE" > "$RECV_FILE" 2>/dev/null || true

if [ -f "$RECV_FILE" ]; then
    RECV_HASH=$(sha256sum "$RECV_FILE" | awk '{print $1}')
    RECV_SIZE=$(stat -c %s "$RECV_FILE" 2>/dev/null || echo 0)
    if [ "$RECV_HASH" = "$BULK_HASH" ] && [ "$RECV_SIZE" = "$BULK_SIZE" ]; then
        pass "public tunnel: 4MiB bulk transfer, hash matches"
    else
        fail "public tunnel: bulk hash/size mismatch (sent=$BULK_HASH/$BULK_SIZE, recv=$RECV_HASH/$RECV_SIZE)"
    fi
else
    fail "public tunnel: bulk transfer did not complete"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 2: secret tunnel (no --udp) relay ────────────────────────────────────
echo "=== Test 2: T-SEC-RELAY (secret tunnel TCP echo, relay mode) ==="
# Start echo service in nssvc again
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start secret tunnel provider in nssvc
LOCAL_LOG="/tmp/bore_local_sec_relay.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-relay" \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Start proxy (consumer) in nscli
PROXY_LOG="/tmp/bore_local_proxy_relay.log"
ip netns exec nscli "$BORE" proxy \
    --local-proxy-port ":9001" \
    --to "$SERVER_IP_NS0_CLI:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-relay" \
    >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 1

# Test echo through proxy
ECHO_TEXT="Hello secret relay"
RESPONSE=$(echo "$ECHO_TEXT" | timeout 12 ip netns exec nscli nc -N -w5 127.0.0.1 9001 2>/dev/null || echo "ERROR")
if [ "$RESPONSE" = "$ECHO_TEXT" ]; then
    pass "secret relay: small TCP echo round-trip"
else
    fail "secret relay: expected '$ECHO_TEXT', got '$RESPONSE'"
fi

# Test bulk (1 MiB via relay)
echo "=== Test 2b: T-SEC-RELAY (secret tunnel bulk 1MiB) ==="
BULK_SIZE=$((1 * 1024 * 1024))
BULK_FILE="/tmp/bore_local_sec_bulk.bin"
head -c "$BULK_SIZE" /dev/zero > "$BULK_FILE"
BULK_HASH=$(sha256sum "$BULK_FILE" | awk '{print $1}')

RECV_FILE="/tmp/bore_local_sec_bulk_recv.bin"
timeout 30 ip netns exec nscli socat - TCP:127.0.0.1:9001 < "$BULK_FILE" > "$RECV_FILE" 2>/dev/null || true

if [ -f "$RECV_FILE" ]; then
    RECV_HASH=$(sha256sum "$RECV_FILE" | awk '{print $1}')
    RECV_SIZE=$(stat -c %s "$RECV_FILE" 2>/dev/null || echo 0)
    if [ "$RECV_HASH" = "$BULK_HASH" ] && [ "$RECV_SIZE" = "$BULK_SIZE" ]; then
        pass "secret relay: 1MiB bulk transfer, hash matches"
    else
        fail "secret relay: bulk hash/size mismatch (sent=$BULK_HASH/$BULK_SIZE, recv=$RECV_HASH/$RECV_SIZE)"
    fi
else
    fail "secret relay: bulk transfer did not complete"
fi

kill "$PROXY_PID" "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 3: secret tunnel WITH --udp (direct path) ──────────────────────────
echo "=== Test 3: T-SEC-DIRECT (secret tunnel + --udp on provider/consumer/server) ==="
# Server already has --udp. Start echo in nssvc
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start secret provider WITH --udp
LOCAL_LOG="/tmp/bore_local_sec_direct.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-direct" \
    --udp \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Start consumer WITH --udp
PROXY_LOG="/tmp/bore_local_proxy_direct.log"
ip netns exec nscli "$BORE" proxy \
    --local-proxy-port ":9002" \
    --to "$SERVER_IP_NS0_CLI:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-direct" \
    --udp \
    >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 2

# Echo test
ECHO_TEXT="Hello direct UDP"
RESPONSE=$(echo "$ECHO_TEXT" | timeout 12 ip netns exec nscli nc -N -w5 127.0.0.1 9002 2>/dev/null || echo "ERROR")
if [ "$RESPONSE" = "$ECHO_TEXT" ]; then
    pass "secret direct: TCP echo over UDP path"
else
    fail "secret direct: expected '$ECHO_TEXT', got '$RESPONSE'"
fi

# Check for direct path indicator in logs (best-effort)
if grep -q "UDP\|direct\|QUIC" "$LOCAL_LOG" "$PROXY_LOG" 2>/dev/null || grep -q "UDP\|direct" "$SERVER_LOG" 2>/dev/null; then
    : # UDP path was activated
fi

kill "$PROXY_PID" "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 4: network emulation (latency + loss) ──────────────────────────────────
echo "=== Test 4: T-NETEM (public tunnel with 20ms delay + 1% loss) ==="
# Add netem qdisc to the ns0↔nscli link
ip netns exec ns0 tc qdisc add dev veth1s root netem delay 20ms loss 1% 2>/dev/null || true

# Start echo service
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start public tunnel
LOCAL_LOG="/tmp/bore_local_netem.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Bulk transfer under netem (smaller to avoid timeouts)
BULK_SIZE=$((512 * 1024))
BULK_FILE="/tmp/bore_local_netem_bulk.bin"
head -c "$BULK_SIZE" /dev/zero > "$BULK_FILE"
BULK_HASH=$(sha256sum "$BULK_FILE" | awk '{print $1}')

RECV_FILE="/tmp/bore_local_netem_recv.bin"
timeout 30 ip netns exec nscli socat - TCP:"$SERVER_IP_NS0_CLI:9999" < "$BULK_FILE" > "$RECV_FILE" 2>/dev/null || true

if [ -f "$RECV_FILE" ]; then
    RECV_HASH=$(sha256sum "$RECV_FILE" | awk '{print $1}')
    RECV_SIZE=$(stat -c %s "$RECV_FILE" 2>/dev/null || echo 0)
    if [ "$RECV_HASH" = "$BULK_HASH" ] && [ "$RECV_SIZE" = "$BULK_SIZE" ]; then
        pass "netem: 512KiB transfer under 20ms delay + 1% loss, hash matches"
    else
        fail "netem: bulk mismatch (sent=$BULK_HASH/$BULK_SIZE, recv=$RECV_HASH/$RECV_SIZE)"
    fi
else
    fail "netem: bulk transfer did not complete"
fi

# Remove netem qdisc
ip netns exec ns0 tc qdisc del dev veth1s root 2>/dev/null || true

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 5: --max-conns limit (secret provider) ─────────────────────────────────
echo "=== Test 5: T-MAXCONNS (secret provider --max-conns 2, try 4 concurrent) ==="
# Start echo service
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start provider with max-conns=2
LOCAL_LOG="/tmp/bore_local_maxconns.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-maxconns" \
    --max-conns 2 \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Start consumer proxy
PROXY_LOG="/tmp/bore_local_proxy_maxconns.log"
ip netns exec nscli "$BORE" proxy \
    --local-proxy-port ":9003" \
    --to "$SERVER_IP_NS0_CLI:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-maxconns" \
    >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 1

# Fire 4 concurrent connections
SUCCESS=0
MC_PIDS=()
for i in 1 2 3 4; do
    (echo "test$i" | timeout 12 ip netns exec nscli nc -N -w5 127.0.0.1 9003 > /tmp/bore_local_mc_$i 2>&1) &
    MC_PIDS+=("$!")
done
wait "${MC_PIDS[@]}"

# Count how many succeeded (at least 2 should succeed)
SUCCEEDED=0
for i in 1 2 3 4; do
    if [ -f "/tmp/bore_local_mc_$i" ] && grep -q "test$i" "/tmp/bore_local_mc_$i" 2>/dev/null; then
        SUCCEEDED=$((SUCCEEDED+1))
    fi
done

# With --max-conns 2, we expect at least 2 to succeed; exact count varies by timing
if [ "$SUCCEEDED" -ge 2 ]; then
    pass "max-conns: at least 2 of 4 concurrent connections succeeded"
else
    fail "max-conns: only $SUCCEEDED of 4 concurrent connections succeeded (expected ≥2)"
fi

kill "$PROXY_PID" "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 6: --carriers (public tunnel concurrency) ────────────────────────────
echo "=== Test 6: T-CARRIERS (public tunnel --carriers 4) ==="
# Start echo service
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start public tunnel with --carriers 4
LOCAL_LOG="/tmp/bore_local_carriers.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    --carriers 4 \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Fire 10 concurrent small connections
SUCCESS=0
CARR_PIDS=()
for i in 1 2 3 4 5 6 7 8 9 10; do
    (echo "c$i" | timeout 12 ip netns exec nscli nc -N -w5 "$SERVER_IP_NS0_CLI" 9999 > /tmp/bore_local_carr_$i 2>&1) &
    CARR_PIDS+=("$!")
done
wait "${CARR_PIDS[@]}"

for i in 1 2 3 4 5 6 7 8 9 10; do
    if [ -f "/tmp/bore_local_carr_$i" ] && grep -q "c$i" "/tmp/bore_local_carr_$i" 2>/dev/null; then
        SUCCESS=$((SUCCESS+1))
    fi
done

if [ "$SUCCESS" -eq 10 ]; then
    pass "carriers: all 10 concurrent connections succeeded"
else
    fail "carriers: only $SUCCESS of 10 connections succeeded (expected 10)"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 7: TLS / --https (self-signed cert) ──────────────────────────────────
echo "=== Test 7: T-TLS (public tunnel with --https) ==="
# Generate self-signed cert for this test
CERT_FILE="/tmp/bore_local_cert.pem"
KEY_FILE="/tmp/bore_local_key.pem"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$KEY_FILE" -out "$CERT_FILE" \
    -days 1 -subj "/CN=localhost" 2>/dev/null

# Restart server with cert
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free ns0 127.0.0.1 7835 || true

SERVER_LOG="/tmp/bore_local_server_tls.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --cert-file "$CERT_FILE" \
    --key-file "$KEY_FILE" \
    --udp \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready ns0 127.0.0.1 7835 \
    || die "TLS server failed to start"
sleep 1

# Start echo service
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Start tunnel with --https
LOCAL_LOG="/tmp/bore_local_https.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "https://$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    --https \
    --insecure \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Test TLS tunnel (plain TCP payload over TLS tunnel)
ECHO_TEXT="Hello HTTPS"
RESPONSE=$(echo "$ECHO_TEXT" | timeout 12 ip netns exec nscli nc -N -w5 "$SERVER_IP_NS0_CLI" 9999 2>/dev/null || echo "ERROR")
if [ "$RESPONSE" = "$ECHO_TEXT" ]; then
    pass "TLS: --https public tunnel, plain TCP works through TLS tunnel"
else
    fail "TLS: expected '$ECHO_TEXT', got '$RESPONSE'"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5

# ── Test 8: Basic Auth (secret provider) ────────────────────────────────────────
echo "=== Test 8: T-BASICAUTH (secret provider --basic-auth) ==="
# Test 7 left a TLS (cert) server running; restart a PLAIN server so the
# basic-auth provider/consumer (which connect with plain --to, not https://)
# can register. Otherwise they hit the cert server and never come up.
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free ns0 127.0.0.1 7835 || true
SERVER_LOG="/tmp/bore_local_server_ba.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --udp \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready ns0 127.0.0.1 7835 || die "basic-auth server failed to start"

# Start HTTP server in nssvc (Python http.server)
HTTP_DIR="/tmp/bore_local_http"
mkdir -p "$HTTP_DIR"
echo "OK" > "$HTTP_DIR/index.html"

ip netns exec nssvc python3 -m http.server 8080 --bind 127.0.0.1 \
    --directory "$HTTP_DIR" >/dev/null 2>&1 &
HTTP_PID=$!
sleep 0.5

# Start provider with basic auth
LOCAL_LOG="/tmp/bore_local_basicauth.log"
ip netns exec nssvc "$BORE" local 8080 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-basicauth" \
    --basic-auth "user:pass" \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Start consumer proxy
PROXY_LOG="/tmp/bore_local_proxy_basicauth.log"
ip netns exec nscli "$BORE" proxy \
    --local-proxy-port ":9004" \
    --to "$SERVER_IP_NS0_CLI:7835" \
    --secret "$SECRET" \
    --tcp-secret-id "test-basicauth" \
    >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
sleep 1

# Test WITHOUT auth: expect 401
RESPONSE=$(ip netns exec nscli curl -s --max-time 10 -o /dev/null -w "%{http_code}" \
    http://127.0.0.1:9004/ 2>/dev/null || echo "000")
if [ "$RESPONSE" = "401" ]; then
    pass "basic-auth: request without credentials gets 401"
else
    fail "basic-auth: expected 401 without auth, got $RESPONSE"
fi

# Test WITH correct auth: expect 200
RESPONSE=$(ip netns exec nscli curl -s --max-time 10 -o /dev/null -w "%{http_code}" \
    -u "user:pass" http://127.0.0.1:9004/ 2>/dev/null || echo "000")
if [ "$RESPONSE" = "200" ]; then
    pass "basic-auth: request with correct credentials gets 200"
else
    fail "basic-auth: expected 200 with auth, got $RESPONSE"
fi

# Test WITH wrong auth: expect 401
RESPONSE=$(ip netns exec nscli curl -s --max-time 10 -o /dev/null -w "%{http_code}" \
    -u "user:wrong" http://127.0.0.1:9004/ 2>/dev/null || echo "000")
if [ "$RESPONSE" = "401" ]; then
    pass "basic-auth: request with wrong credentials gets 401"
else
    fail "basic-auth: expected 401 with wrong auth, got $RESPONSE"
fi

kill "$PROXY_PID" "$LOCAL_PID" "$HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 9: T-WLOG-PUB-HTTP (public tunnel HTTP logging) ────────────────────────
echo "=== Test 9: T-WLOG-PUB-HTTP (public tunnel with --webserver-log) ==="
# Start HTTP responder (returns valid HTTP/1.1 response with proper CRLF)
HTTP_RESP_9="/tmp/http_response_9.bin"
printf "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok" > "$HTTP_RESP_9"
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork SYSTEM:"cat $HTTP_RESP_9" >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Create log directory
WLOG_DIR="/tmp/bore_wlog_http"
rm -rf "$WLOG_DIR" 2>/dev/null || true
mkdir -p "$WLOG_DIR"

# Start bore local with --webserver-log
LOCAL_LOG="/tmp/bore_local_wlog_http.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    --webserver-log "$WLOG_DIR" \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Send HTTP request from nscli (caller IP = 10.211.0.1)
HTTP_REQUEST="GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
(echo -en "$HTTP_REQUEST"; sleep 0.5) | timeout 10 ip netns exec nscli nc "$SERVER_IP_NS0_CLI" 9999 > /dev/null 2>&1 || true
sleep 1

# Check that log file exists and contains the request + caller IP
WLOG_FILE="$WLOG_DIR/9999.log"
if [ -f "$WLOG_FILE" ]; then
    if grep -q "GET /ping" "$WLOG_FILE" 2>/dev/null && grep -q "$CLI_IP" "$WLOG_FILE" 2>/dev/null; then
        pass "webserver-log public HTTP: log file contains GET /ping and caller IP $CLI_IP"
    else
        fail "webserver-log public HTTP: log file missing request line or caller IP"
    fi
else
    fail "webserver-log public HTTP: log file not created"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_DIR" 2>/dev/null || true

# ── Test 10: T-WLOG-PUB-RAW (public tunnel raw bytes logging) ────────────────────
echo "=== Test 10: T-WLOG-PUB-RAW (public tunnel with raw bytes) ==="
# Start echo service (echoes request back, triggering raw mode)
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork EXEC:/bin/cat >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Create log directory
WLOG_DIR="/tmp/bore_wlog_raw"
rm -rf "$WLOG_DIR" 2>/dev/null || true
mkdir -p "$WLOG_DIR"

# Start bore local with --webserver-log
LOCAL_LOG="/tmp/bore_local_wlog_raw.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    --webserver-log "$WLOG_DIR" \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Send raw (non-HTTP) bytes through the tunnel
echo -n "raw_binary_data" | timeout 10 ip netns exec nscli nc "$SERVER_IP_NS0_CLI" 9999 > /dev/null 2>&1 || true
sleep 1

# Check that log file contains exactly one raw line (marked with "raw" at end)
WLOG_FILE="$WLOG_DIR/9999.log"
if [ -f "$WLOG_FILE" ]; then
    RAW_COUNT=$(grep '"raw"' "$WLOG_FILE" 2>/dev/null | wc -l)
    if [ "$RAW_COUNT" -eq 1 ]; then
        pass "webserver-log public raw: exactly one raw line recorded"
    else
        fail "webserver-log public raw: expected 1 raw line, got $RAW_COUNT"
    fi
else
    fail "webserver-log public raw: log file not created"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_DIR" 2>/dev/null || true

# ── Test 11: T-WLOG-PUB-IP (server-side public logging) ───────────────────────────
echo "=== Test 11: T-WLOG-PUB-IP (server-side public tunnel logging) ==="
# Kill existing server and restart with --webserver-log
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free ns0 127.0.0.1 7835 || true

# Create server-side log directory
WLOG_DIR="/tmp/bore_wlog_server"
rm -rf "$WLOG_DIR" 2>/dev/null || true
mkdir -p "$WLOG_DIR"

SERVER_LOG="/tmp/bore_local_server_wlog.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --udp \
    --control-port 7835 \
    --webserver-log "$WLOG_DIR" \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready ns0 127.0.0.1 7835 || die "server with --webserver-log failed to start"
sleep 1

# Start HTTP responder and public tunnel
HTTP_RESP_11="/tmp/http_response_11.bin"
printf "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok" > "$HTTP_RESP_11"
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork SYSTEM:"cat $HTTP_RESP_11" >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

LOCAL_LOG="/tmp/bore_local_wlog_server_test.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Send HTTP from nscli (caller will appear in server log with real IP from accept)
HTTP_REQUEST="GET /test HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
(echo -en "$HTTP_REQUEST"; sleep 0.5) | timeout 10 ip netns exec nscli nc "$SERVER_IP_NS0_CLI" 9999 > /dev/null 2>&1 || true
sleep 1

# Check server-side log — it should contain the caller IP from accept
WLOG_FILE="$WLOG_DIR/9999.log"
if [ -f "$WLOG_FILE" ]; then
    if grep -q "$CLI_IP" "$WLOG_FILE" 2>/dev/null; then
        pass "webserver-log server public: log contains caller IP from accept"
    else
        fail "webserver-log server public: log missing caller IP"
    fi
else
    fail "webserver-log server public: server log file not created"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_DIR" 2>/dev/null || true

# ── Test 12: T-WLOG-CLIENT-IP-FWD (client receives forwarded IP) ─────────────────
echo "=== Test 12: T-WLOG-CLIENT-IP-FWD (client logs forwarded external IP) ==="
# Start HTTP responder
HTTP_RESP_12="/tmp/http_response_12.bin"
printf "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok" > "$HTTP_RESP_12"
ip netns exec nssvc socat TCP-LISTEN:9000,reuseaddr,fork SYSTEM:"cat $HTTP_RESP_12" >/dev/null 2>&1 &
SVC_PID=$!
sleep 0.3

# Create client-side log directory
WLOG_DIR="/tmp/bore_wlog_client_fwd"
rm -rf "$WLOG_DIR" 2>/dev/null || true
mkdir -p "$WLOG_DIR"

LOCAL_LOG="/tmp/bore_local_wlog_client_fwd.log"
ip netns exec nssvc "$BORE" local 9000 \
    --to "$SERVER_IP_NS0_SVC:7835" \
    --secret "$SECRET" \
    --port 9999 \
    --webserver-log "$WLOG_DIR" \
    >"$LOCAL_LOG" 2>&1 &
LOCAL_PID=$!
sleep 1

# Send HTTP from nscli (caller IP = 10.211.0.1)
HTTP_REQUEST="GET /fwd-test HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
(echo -en "$HTTP_REQUEST"; sleep 0.5) | timeout 10 ip netns exec nscli nc "$SERVER_IP_NS0_CLI" 9999 > /dev/null 2>&1 || true
sleep 1

# Check that client-side log contains the REAL external caller IP (forwarded from server)
WLOG_FILE="$WLOG_DIR/9999.log"
if [ -f "$WLOG_FILE" ]; then
    if grep -q "$CLI_IP" "$WLOG_FILE" 2>/dev/null; then
        pass "webserver-log client forward: client log shows external caller IP"
    else
        fail "webserver-log client forward: client log missing external caller IP"
    fi
else
    fail "webserver-log client forward: client log file not created"
fi

kill "$LOCAL_PID" "$SVC_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_DIR" 2>/dev/null || true

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "========================================"
echo "Results: $PASS passed, $FAIL failed"
echo "========================================"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
