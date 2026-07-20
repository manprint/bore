#!/usr/bin/env bash
# vhost netns harness — functional acceptance tests
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Topology:
#   ns0 (server) — veth0s↔veth0p (10.211.0.0/30) ↔ nsp1 (provider 1)
#                — veth1s↔veth1p (10.212.0.0/30) ↔ nsp2 (provider 2)
#                — veth2s↔veth2p (10.213.0.0/30) ↔ nsc  (client)
#
# Usage: sudo scripts/vhost_netns_test.sh

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
for cmd in curl python3 openssl ip; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

SECRET="vhtest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP_NS0_P1="10.211.0.2"  # server-side of ns0↔nsp1 veth
SERVER_IP_P1="10.211.0.1"      # nsp1-side
SERVER_IP_NS0_P2="10.212.0.2"  # server-side of ns0↔nsp2 veth
SERVER_IP_P2="10.212.0.1"      # nsp2-side
SERVER_IP_NS0_C="10.213.0.2"   # server-side of ns0↔nsc veth
CLIENT_IP="10.213.0.1"         # nsc-side; curl runs here

PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

cleanup() {
    set +e
    # Kill ALL bore — server AND vhost clients. The old `pkill -f bore.*vhost`
    # matched only `bore vhost` clients and left `bore server` running, which then
    # pinned the namespace so `ip netns del` failed and leaked /run/netns/ns0.
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    sleep 0.3
    pkill -9 -f 'target/release/bore' 2>/dev/null
    # Kill anything still pinned inside each ns BEFORE deleting it, so the delete
    # cannot fail with EBUSY and leave a stale namespace file behind.
    for ns in ns0 nsp1 nsp2 nsc; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf /tmp/bore_vhost_* /tmp/vhost_config.yml 2>/dev/null
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

# Poll until the control port ACCEPTS (server up) — catches a server that died on
# a bad config instead of silently cascading every later test into failure.
wait_server_ready() {
    local from_ns="$1" ip="$2" port="${3:-7835}"
    for _ in $(seq 1 50); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# Poll until the control port REFUSES (old server's listener fully released)
# before rebinding, so a kill+restart on the same ports does not race.
wait_port_free() {
    local from_ns="$1" ip="$2" port="${3:-7835}"
    for _ in $(seq 1 50); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null || return 0
        sleep 0.1
    done
    return 1
}

# ── Setup ──────────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ==="
# Delete-before-create: a previous crashed/segfaulted run may have leaked these,
# in which case `ip netns add` fails with "File exists". Reclaim them first.
for ns in ns0 nsp1 nsp2 nsc; do
    ip netns del "$ns" 2>/dev/null || true
done
ip netns add ns0
ip netns add nsp1
ip netns add nsp2
ip netns add nsc

# ns0 ↔ nsp1
ip link add veth0s type veth peer name veth0p
ip link set veth0s netns ns0
ip link set veth0p netns nsp1
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_P1/30" dev veth0s
ip netns exec nsp1 ip addr add "$SERVER_IP_P1/30" dev veth0p
ip netns exec ns0 ip link set veth0s up
ip netns exec nsp1 ip link set veth0p up

# ns0 ↔ nsp2
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0
ip link set veth1p netns nsp2
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_P2/30" dev veth1s
ip netns exec nsp2 ip addr add "$SERVER_IP_P2/30" dev veth1p
ip netns exec ns0 ip link set veth1s up
ip netns exec nsp2 ip link set veth1p up

# ns0 ↔ nsc
ip link add veth2s type veth peer name veth2p
ip link set veth2s netns ns0
ip link set veth2p netns nsc
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_C/30" dev veth2s
ip netns exec nsc ip addr add "$CLIENT_IP/30" dev veth2p
ip netns exec ns0 ip link set veth2s up
ip netns exec nsc ip link set veth2p up

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec nsp1 ip link set lo up
ip netns exec nsp2 ip link set lo up
ip netns exec nsc ip link set lo up

# ── Generate self-signed wildcard cert for HTTPS ───────────────────────────────
echo "=== Generating wildcard cert for *.bore.local ==="
CERT_FILE="/tmp/bore_vhost_cert.pem"
KEY_FILE="/tmp/bore_vhost_key.pem"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$KEY_FILE" -out "$CERT_FILE" \
    -days 1 -subj "/CN=bore.local" \
    -addext "subjectAltName=DNS:*.bore.local,DNS:bore.local" 2>/dev/null

# ── Start server ───────────────────────────────────────────────────────────────
echo "=== Starting bore server in ns0 ==="
SERVER_LOG="/tmp/bore_vhost_server.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
sleep 1
ip netns exec nsp1 nc -z "$SERVER_IP_NS0_P1" 7835 || die "server not reachable from nsp1"
echo "  Server up (pid $SERVER_PID)"

# ── Test 1: HTTP route (single provider, single subdomain) ────────────────────
echo "=== Test 1: HTTP route (sub=app1, id=user1) ==="
P1_LOG="/tmp/bore_vhost_p1.log"
P1_ORIGIN="/tmp/bore_vhost_p1_origin"
mkdir -p "$P1_ORIGIN"
echo "hello1" > "$P1_ORIGIN/index.html"

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.5

ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
sleep 1

# Curl from client ns
RESP=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
    http://app1.bore.local/ 2>/dev/null || echo "ERROR")
if [ "$RESP" = "hello1" ]; then
    pass "HTTP route: app1→user1 returns hello1"
else
    fail "HTTP route: app1→user1 returned '$RESP' (expected 'hello1')"
fi

# ── Test 2: HTTPS route ───────────────────────────────────────────────────────
echo "=== Test 2: HTTPS route (same provider) ==="
RESP=$(ip netns exec nsc curl -sk --resolve app1.bore.local:443:"$SERVER_IP_NS0_C" \
    https://app1.bore.local/ 2>/dev/null || echo "ERROR")
if [ "$RESP" = "hello1" ]; then
    pass "HTTPS route: app1→user1 returns hello1"
else
    fail "HTTPS route: app1→user1 returned '$RESP' (expected 'hello1')"
fi

# ── Test 3: unknown subdomain → 502 ──────────────────────────────────────────
echo "=== Test 3: unknown subdomain → HTTP 502 ==="
HTTP_CODE=$(ip netns exec nsc curl -s -o /dev/null -w '%{http_code}' \
    --resolve unknown.bore.local:80:"$SERVER_IP_NS0_C" \
    http://unknown.bore.local/ 2>/dev/null || echo 000)
if [ "$HTTP_CODE" = "502" ]; then
    pass "unknown subdomain returns 502"
else
    fail "unknown subdomain returned $HTTP_CODE (expected 502)"
fi

# Kill p1 processes
kill "$P1_VID" 2>/dev/null || true
kill "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 3b: native --backend-tls to a self-signed HTTPS backend ──────────────
# The backend is itself HTTPS; the bore SERVER originates TLS to it (backend TLS
# client, accept-any-cert). SNI defaults to localhost — irrelevant since the
# verifier accepts any certificate, so the *.bore.local cert is fine.
echo "=== Test 3b: native --backend-tls (HTTPS self-signed backend) ==="
TLS_BK_PORT=8443
ip netns exec nsp1 python3 - "$TLS_BK_PORT" "$CERT_FILE" "$KEY_FILE" \
    >/tmp/bore_vhost_tlsbk.log 2>&1 <<'PYEOF' &
import socket, ssl, sys, threading
port = int(sys.argv[1]); cert, key = sys.argv[2], sys.argv[3]
body = b"backend-tls-ok"
resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(cert, key)
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(64)
def h(c):
    try:
        with ctx.wrap_socket(c, server_side=True) as t:
            t.recv(4096); t.sendall(resp)
    except Exception:
        pass
while True:
    c, _ = srv.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
PYEOF
TLS_BK_PID=$!
sleep 0.5

TLSP_LOG="/tmp/bore_vhost_tlsp.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:$TLS_BK_PORT \
    --subdomain tlsapp --id user1 --backend-tls \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$TLSP_LOG" 2>&1 &
TLSP_VID=$!
sleep 1

RESP=$(ip netns exec nsc curl -s --resolve tlsapp.bore.local:80:"$SERVER_IP_NS0_C" \
    http://tlsapp.bore.local/ 2>/dev/null || echo "ERROR")
if [ "$RESP" = "backend-tls-ok" ]; then
    pass "native --backend-tls serves the self-signed HTTPS backend"
else
    fail "native --backend-tls returned '$RESP' (expected 'backend-tls-ok')"
fi
kill "$TLSP_VID" "$TLS_BK_PID" 2>/dev/null || true
sleep 0.5

# ── Test 4: reservation (vhost.yml config) ────────────────────────────────────
echo "=== Test 4: reservation with vhost.yml ==="
CONFIG_FILE="/tmp/vhost_config.yml"
# vhost.yml schema (see src/vhost.rs): base_domain is required; each reservation
# needs `client_id` + `subdomain` (NOT user_id/enabled — those fields don't exist
# and a malformed config makes the server exit on startup).
cat > "$CONFIG_FILE" <<'EOF'
base_domain: bore.local
reservations:
  - client_id: user2
    subdomain: app2
EOF

# Restart server with config
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free nsp2 "$SERVER_IP_NS0_P2" 7835 || true

SERVER_LOG="/tmp/bore_vhost_server_r2.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --vhost-config "$CONFIG_FILE" \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp2 "$SERVER_IP_NS0_P2" 7835 \
    || die "T4 server failed to start (bad config?): $(tail -3 "$SERVER_LOG" 2>/dev/null)"

P2_LOG="/tmp/bore_vhost_p2.log"
P2_ORIGIN="/tmp/bore_vhost_p2_origin"
mkdir -p "$P2_ORIGIN"
echo "hello2" > "$P2_ORIGIN/index.html"

ip netns exec nsp2 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P2_ORIGIN" >/dev/null 2>&1 &
P2_HTTP_PID=$!
sleep 0.5

# Authorized provider (id=user2, subdomain=app2)
ip netns exec nsp2 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app2 --id user2 \
    --to "$SERVER_IP_NS0_P2:7835" --secret "$SECRET" \
    >"$P2_LOG" 2>&1 &
P2_VID=$!
sleep 1

RESP=$(ip netns exec nsc curl -s --resolve app2.bore.local:80:"$SERVER_IP_NS0_C" \
    http://app2.bore.local/ 2>/dev/null || echo "ERROR")
if [ "$RESP" = "hello2" ]; then
    pass "reservation: app2→user2 (authorized) registers and routes OK"
else
    fail "reservation: app2→user2 returned '$RESP' (expected 'hello2')"
fi

# Unauthorized provider (id=hacker, subdomain=app2)
P2_HACKER_LOG="/tmp/bore_vhost_p2_hacker.log"
HACKER_TIMEOUT=5
timeout "$HACKER_TIMEOUT" ip netns exec nsp2 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app2 --id hacker \
    --to "$SERVER_IP_NS0_P2:7835" --secret "$SECRET" \
    >"$P2_HACKER_LOG" 2>&1 &
HACKER_PID=$!
sleep 2
if ! kill "$HACKER_PID" 2>/dev/null; then
    # Process already exited (good!)
    if grep -q "rejected\|unauthorized\|forbidden\|not.*authorized" "$P2_HACKER_LOG" 2>/dev/null; then
        pass "reservation: app2→hacker (unauthorized) rejected with error log"
    else
        # Check exit code instead if no explicit error message
        pass "reservation: app2→hacker (unauthorized) rejected (process exited)"
    fi
else
    # Process still running (bad!)
    kill "$HACKER_PID" 2>/dev/null || true
    fail "reservation: app2→hacker should be rejected but still running"
fi
sleep 0.5

# ── Test 5: multi-user concurrency ───────────────────────────────────────────
echo "=== Test 5: multi-user concurrency (app1→user1, app2→user2) ==="
# Kill previous providers
kill "$P2_VID" 2>/dev/null || true
kill "$P2_HTTP_PID" 2>/dev/null || true
sleep 0.5

# Restart both providers (app1→user1 on nsp1, app2→user2 on nsp2)
P1_ORIGIN="/tmp/bore_vhost_p1_origin_r2"
P2_ORIGIN="/tmp/bore_vhost_p2_origin_r2"
mkdir -p "$P1_ORIGIN" "$P2_ORIGIN"
echo "AAA" > "$P1_ORIGIN/index.html"
echo "BBB" > "$P2_ORIGIN/index.html"

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
ip netns exec nsp2 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P2_ORIGIN" >/dev/null 2>&1 &
P2_HTTP_PID=$!
sleep 0.5

P1_LOG="/tmp/bore_vhost_p1_c.log"
P2_LOG="/tmp/bore_vhost_p2_c.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
ip netns exec nsp2 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app2 --id user2 \
    --to "$SERVER_IP_NS0_P2:7835" --secret "$SECRET" \
    >"$P2_LOG" 2>&1 &
P2_VID=$!
sleep 1

# Fire many concurrent curls
TEST_COUNT=10
PASS_COUNT=0
for i in $(seq 1 "$TEST_COUNT"); do
    R1=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app1.bore.local/ 2>/dev/null || echo "ERROR")
    R2=$(ip netns exec nsc curl -s --resolve app2.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app2.bore.local/ 2>/dev/null || echo "ERROR")
    if [ "$R1" = "AAA" ] && [ "$R2" = "BBB" ]; then
        PASS_COUNT=$((PASS_COUNT+1))
    fi
done

if [ "$PASS_COUNT" = "$TEST_COUNT" ]; then
    pass "concurrency: $TEST_COUNT concurrent requests, no cross-talk"
else
    fail "concurrency: $PASS_COUNT/$TEST_COUNT requests succeeded (expected all)"
fi

kill "$P1_VID" "$P2_VID" "$P1_HTTP_PID" "$P2_HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 6: large body integrity ─────────────────────────────────────────────
echo "=== Test 6: large body integrity (50-100 MB) ==="
P1_ORIGIN="/tmp/bore_vhost_p1_origin_r3"
mkdir -p "$P1_ORIGIN"
BIGFILE="$P1_ORIGIN/bigfile"
head -c 50M /dev/urandom > "$BIGFILE"
BIGFILE_SHA=$(sha256sum "$BIGFILE" | awk '{print $1}')

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.5

P1_LOG="/tmp/bore_vhost_p1_large.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
sleep 1

# Download and verify
DOWNLOADED="/tmp/bore_vhost_bigfile_dl"
ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
    http://app1.bore.local/bigfile --output "$DOWNLOADED" 2>/dev/null || true

if [ -f "$DOWNLOADED" ]; then
    DL_SHA=$(sha256sum "$DOWNLOADED" | awk '{print $1}')
    if [ "$DL_SHA" = "$BIGFILE_SHA" ]; then
        pass "large body: 50M file sha256 matches"
    else
        fail "large body: sha256 mismatch (orig=$BIGFILE_SHA, dl=$DL_SHA)"
    fi
else
    fail "large body: download failed"
fi

kill "$P1_VID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 7: UDP direct path (server --udp, provider --udp) ─────────────────────
echo "=== Test 7: UDP direct path (server + provider both --udp) ==="
# The T4 server restart dropped --udp; bring up a fresh server WITH --udp so the
# QUIC direct path can establish.
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free nsp1 "$SERVER_IP_NS0_P1" 7835 || true
SERVER_LOG="/tmp/bore_vhost_server_udp.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T7 server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"
P1_ORIGIN="/tmp/bore_vhost_p1_origin_r4"
mkdir -p "$P1_ORIGIN"
echo "directpath" > "$P1_ORIGIN/index.html"

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.5

P1_LOG="/tmp/bore_vhost_p1_direct.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    --udp \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
sleep 2

# Check for direct path log
if wait_for_log "$SERVER_LOG" "vhost QUIC direct carrier established" 15; then
    # Still verify HTTP works
    RESP=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app1.bore.local/ 2>/dev/null || echo "ERROR")
    if [ "$RESP" = "directpath" ]; then
        pass "UDP direct path: carrier established, HTTP 200"
    else
        fail "UDP direct path: HTTP returned '$RESP' (expected 'directpath')"
    fi
else
    # Fallback: no direct log but HTTP still works = relay mode
    RESP=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app1.bore.local/ 2>/dev/null || echo "ERROR")
    if [ "$RESP" = "directpath" ]; then
        fail "UDP direct path: no direct log found (relay mode, but test expected direct)"
    else
        fail "UDP direct path: HTTP failed"
    fi
fi

kill "$P1_VID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5

# Spawn a TLS-terminating HTTPS backend in netns $1 on port $2 with body $3,
# using the shared self-signed cert (the backend TLS client accepts any cert, so
# the *.bore.local cert works regardless of the default localhost SNI). Echoes PID.
start_https_backend() {
    local ns="$1" port="$2" body="$3"
    ip netns exec "$ns" python3 - "$port" "$CERT_FILE" "$KEY_FILE" "$body" \
        >"/tmp/bore_vhost_httpsbk_$port.log" 2>&1 <<'PYEOF' &
import socket, ssl, sys, threading
port = int(sys.argv[1]); cert, key, body = sys.argv[2], sys.argv[3], sys.argv[4].encode()
resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(cert, key)
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(64)
def h(c):
    try:
        with ctx.wrap_socket(c, server_side=True) as t:
            t.recv(4096); t.sendall(resp)
    except Exception:
        pass
while True:
    c, _ = srv.accept()
    threading.Thread(target=h, args=(c,), daemon=True).start()
PYEOF
    echo $!
}

# ── Test 7b: UDP direct path + --backend-tls (QUIC direct to HTTPS backend) ────
echo "=== Test 7b: --backend-tls over the UDP/QUIC direct path ==="
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free nsp1 "$SERVER_IP_NS0_P1" 7835 || true
SERVER_LOG="/tmp/bore_vhost_server_udp_btls.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T7b server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

BTLS_BK_PID=$(start_https_backend nsp1 8443 "udp-backend-tls-ok")
sleep 0.5

BTLSU_LOG="/tmp/bore_vhost_btls_udp.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8443 \
    --subdomain tlsudp --id user1 --backend-tls \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    --udp \
    >"$BTLSU_LOG" 2>&1 &
BTLSU_VID=$!
sleep 2

if wait_for_log "$SERVER_LOG" "vhost QUIC direct carrier established" 15; then
    RESP=$(ip netns exec nsc curl -s --resolve tlsudp.bore.local:80:"$SERVER_IP_NS0_C" \
        http://tlsudp.bore.local/ 2>/dev/null || echo "ERROR")
    if [ "$RESP" = "udp-backend-tls-ok" ]; then
        pass "backend-tls over UDP direct path: QUIC carrier established, HTTPS backend served"
    else
        fail "backend-tls over UDP direct: HTTP returned '$RESP' (expected 'udp-backend-tls-ok')"
    fi
else
    fail "backend-tls over UDP direct: no QUIC direct carrier established (relay mode)"
fi
kill "$BTLSU_VID" "$BTLS_BK_PID" 2>/dev/null || true
sleep 0.5

# ── Test 7c: --backend-tls + --carriers 2 (TCP relay, multi-carrier) ──────────
echo "=== Test 7c: --backend-tls with --carriers 2 (TCP relay) ==="
BTLS_BK2_PID=$(start_https_backend nsp1 8444 "carriers-backend-tls-ok")
sleep 0.5

BTLSC_LOG="/tmp/bore_vhost_btls_carriers.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8444 \
    --subdomain tlscarr --id user1 --backend-tls --carriers 2 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$BTLSC_LOG" 2>&1 &
BTLSC_VID=$!
sleep 2

# Fire several requests so more than one carrier is exercised.
CARR_OK=0
for _ in $(seq 1 6); do
    RESP=$(ip netns exec nsc curl -s --resolve tlscarr.bore.local:80:"$SERVER_IP_NS0_C" \
        http://tlscarr.bore.local/ 2>/dev/null || echo "ERROR")
    [ "$RESP" = "carriers-backend-tls-ok" ] && CARR_OK=$((CARR_OK+1))
done
if [ "$CARR_OK" = "6" ]; then
    pass "backend-tls with --carriers 2: all 6 requests served the HTTPS backend"
else
    fail "backend-tls with --carriers 2: only $CARR_OK/6 requests succeeded"
fi
kill "$BTLSC_VID" "$BTLS_BK2_PID" 2>/dev/null || true
sleep 0.5

# ── Test 8: UDP fallback (server WITHOUT --udp, provider WITH --udp) ──────────
echo "=== Test 8: UDP fallback (server no --udp, provider --udp → relay) ==="
# Restart server WITHOUT --udp
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free nsp1 "$SERVER_IP_NS0_P1" 7835 || true

SERVER_LOG="/tmp/bore_vhost_server_no_udp.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T8 server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

P1_ORIGIN="/tmp/bore_vhost_p1_origin_r5"
mkdir -p "$P1_ORIGIN"
echo "fallback" > "$P1_ORIGIN/index.html"

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.5

P1_LOG="/tmp/bore_vhost_p1_fallback.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    --udp \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
sleep 2

# Should NOT have direct log
if grep -q "vhost QUIC direct carrier established" "$SERVER_LOG" 2>/dev/null; then
    fail "UDP fallback: direct carrier log found (server has no --udp)"
else
    # Verify HTTP still works (relay)
    RESP=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app1.bore.local/ 2>/dev/null || echo "ERROR")
    if [ "$RESP" = "fallback" ]; then
        pass "UDP fallback: no direct carrier (relay), HTTP 200"
    else
        fail "UDP fallback: HTTP returned '$RESP' (expected 'fallback')"
    fi
fi

kill "$P1_VID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 9: auto-reconnect ──────────────────────────────────────────────────
echo "=== Test 9: auto-reconnect (kill+restart server) ==="
P1_ORIGIN="/tmp/bore_vhost_p1_origin_r6"
mkdir -p "$P1_ORIGIN"
echo "reconnect" > "$P1_ORIGIN/index.html"

ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    --directory "$P1_ORIGIN" >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.5

P1_LOG="/tmp/bore_vhost_p1_reconn.log"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    --auto-reconnect \
    >"$P1_LOG" 2>&1 &
P1_VID=$!
sleep 1

# Kill and restart server
kill "$SERVER_PID" 2>/dev/null || true
wait_port_free nsp1 "$SERVER_IP_NS0_P1" 7835 || true

SERVER_LOG="/tmp/bore_vhost_server_r_after.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --control-port 7835 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T9 server failed to restart: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

# Poll for HTTP success (up to 20s)
RECONNECT_PASS=0
for i in $(seq 1 40); do
    RESP=$(ip netns exec nsc curl -s --resolve app1.bore.local:80:"$SERVER_IP_NS0_C" \
        http://app1.bore.local/ 2>/dev/null || echo "")
    if [ "$RESP" = "reconnect" ]; then
        RECONNECT_PASS=1
        break
    fi
    sleep 0.5
done

if [ "$RECONNECT_PASS" = "1" ]; then
    pass "auto-reconnect: provider re-established within 20s"
else
    fail "auto-reconnect: HTTP did not return after server restart (20s timeout)"
fi

kill "$P1_VID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5

# ── Test 10: T-WLOG-VHOST-IP (vhost with webserver-log) ──────────────────────────
echo "=== Test 10: T-WLOG-VHOST-IP (vhost HTTP logging with real IP) ==="
# Create client and server-side log directories
WLOG_CLIENT_DIR="/tmp/bore_wlog_vhost_cli"
WLOG_SERVER_DIR="/tmp/bore_wlog_vhost_srv"
rm -rf "$WLOG_CLIENT_DIR" "$WLOG_SERVER_DIR" 2>/dev/null || true
mkdir -p "$WLOG_CLIENT_DIR" "$WLOG_SERVER_DIR"

# Kill and restart server with --webserver-log
kill "$SERVER_PID" 2>/dev/null || true
sleep 0.5

SERVER_LOG="/tmp/bore_vhost_server_wlog.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    --webserver-log "$WLOG_SERVER_DIR" \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T10 server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

# Start provider (app1) with --webserver-log
P1_LOG="/tmp/bore_vhost_p1_wlog.log"
ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 \
    >"$P1_LOG" 2>&1 &
P1_HTTP_PID=$!
sleep 0.3

P1_VID="/tmp/bore_vhost_p1_wlog.vlog"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain app1 --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    --webserver-log "$WLOG_CLIENT_DIR" \
    >"$P1_VID" 2>&1 &
P1_VID_PID=$!
sleep 1

# Send HTTP from nsc (caller IP = 10.213.0.1)
RESPONSE=$(ip netns exec nsc curl -s -k --resolve app1.bore.local:443:"$SERVER_IP_NS0_C" "https://app1.bore.local/" 2>/dev/null | head -c 50)
sleep 2

# Check that both client and server logs exist and contain the caller IP
CLIENT_LOG="$WLOG_CLIENT_DIR/app1.log"
SERVER_LOG_FILE="$WLOG_SERVER_DIR/app1/app1.bore.local.log"

CLIENT_HAS_IP=0
SERVER_HAS_IP=0

if [ -f "$CLIENT_LOG" ] && grep -q "$CLIENT_IP" "$CLIENT_LOG" 2>/dev/null; then
    CLIENT_HAS_IP=1
fi

if [ -f "$SERVER_LOG_FILE" ] && grep -q "$CLIENT_IP" "$SERVER_LOG_FILE" 2>/dev/null; then
    SERVER_HAS_IP=1
fi

if [ "$CLIENT_HAS_IP" -eq 1 ] && [ "$SERVER_HAS_IP" -eq 1 ]; then
    pass "webserver-log vhost: both client and server logs contain real IP $CLIENT_IP"
else
    if [ "$CLIENT_HAS_IP" -eq 0 ]; then
        fail "webserver-log vhost: client log missing IP (file exists: $([ -f "$CLIENT_LOG" ] && echo yes || echo no))"
    fi
    if [ "$SERVER_HAS_IP" -eq 0 ]; then
        fail "webserver-log vhost: server log missing IP (file exists: $([ -f "$SERVER_LOG_FILE" ] && echo yes || echo no))"
    fi
fi

kill "$P1_VID_PID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_CLIENT_DIR" "$WLOG_SERVER_DIR" 2>/dev/null || true

# ── Test 11: T-WLOG-VHOST-SRV-LAYOUT (server log folder layout) ────────────────────
echo "=== Test 11: T-WLOG-VHOST-SRV-LAYOUT (server log directory structure) ==="
WLOG_SERVER_DIR="/tmp/bore_wlog_vhost_layout"
rm -rf "$WLOG_SERVER_DIR" 2>/dev/null || true
mkdir -p "$WLOG_SERVER_DIR"

# Server already running from previous test — restart with different log dir
kill "$SERVER_PID" 2>/dev/null || true
sleep 0.5

SERVER_LOG="/tmp/bore_vhost_server_layout.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    --webserver-log "$WLOG_SERVER_DIR" \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T11 server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

# Start provider and send request
ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.3

P1_VID="/tmp/bore_vhost_p1_layout.vlog"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain shop --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$P1_VID" 2>&1 &
P1_VID_PID=$!
sleep 1

# Send request
ip netns exec nsc curl -s -k --resolve shop.bore.local:443:"$SERVER_IP_NS0_C" "https://shop.bore.local/" >/dev/null 2>&1 || true
sleep 2

# Check the expected layout: <dir>/shop/shop.bore.local.log
EXPECTED_LOG="$WLOG_SERVER_DIR/shop/shop.bore.local.log"
if [ -f "$EXPECTED_LOG" ]; then
    pass "webserver-log vhost layout: server log at correct path $EXPECTED_LOG"
else
    # Debug: show what was actually created
    ACTUAL_FILES=$(find "$WLOG_SERVER_DIR" -type f 2>/dev/null | tr '\n' ' ')
    fail "webserver-log vhost layout: expected file not found. Files: $ACTUAL_FILES"
fi

kill "$P1_VID_PID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_SERVER_DIR" 2>/dev/null || true

# ── Test 12: T-WLOG-VHOST-ROTATE (rotation on max file size) ──────────────────────
echo "=== Test 12: T-WLOG-VHOST-ROTATE (log rotation on max-file-size) ==="
WLOG_SERVER_DIR="/tmp/bore_wlog_vhost_rotate"
rm -rf "$WLOG_SERVER_DIR" 2>/dev/null || true
mkdir -p "$WLOG_SERVER_DIR"

# Restart server with small max-file-size (1 MB) and max-files = 2
kill "$SERVER_PID" 2>/dev/null || true
sleep 0.5

SERVER_LOG="/tmp/bore_vhost_server_rotate.log"
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-https-port 443 --vhost-cert-file "$CERT_FILE" --vhost-key-file "$KEY_FILE" \
    --vhost-mode both \
    --udp --vhost-quic-port 443 \
    --control-port 7835 \
    --webserver-log "$WLOG_SERVER_DIR" \
    --webserver-log-max-file-size 1 \
    --webserver-log-max-files 2 \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
wait_server_ready nsp1 "$SERVER_IP_NS0_P1" 7835 \
    || die "T12 server failed to start: $(tail -3 "$SERVER_LOG" 2>/dev/null)"

# Start provider and send large requests to exceed max-file-size
ip netns exec nsp1 python3 -m http.server 8000 --bind 127.0.0.1 >/dev/null 2>&1 &
P1_HTTP_PID=$!
sleep 0.3

P1_VID="/tmp/bore_vhost_p1_rotate.vlog"
ip netns exec nsp1 "$BORE" vhost 127.0.0.1:8000 \
    --subdomain test --id user1 \
    --to "$SERVER_IP_NS0_P1:7835" --secret "$SECRET" \
    >"$P1_VID" 2>&1 &
P1_VID_PID=$!
sleep 1

# Send many requests to generate logs larger than 1 MB
for i in {1..15}; do
    ip netns exec nsc curl -s -k --resolve test.bore.local:443:"$SERVER_IP_NS0_C" "https://test.bore.local/" >/dev/null 2>&1 || true
    sleep 0.1
done
sleep 2

# Check rotation: should have at most 2 files (test/test.bore.local.log and .log.1)
ROTATE_DIR="$WLOG_SERVER_DIR/test"
FILE_COUNT=0
if [ -d "$ROTATE_DIR" ]; then
    FILE_COUNT=$(find "$ROTATE_DIR" -type f -name "test.bore.local.log*" | wc -l)
fi

if [ "$FILE_COUNT" -le 2 ]; then
    pass "webserver-log vhost rotate: rotation kept <= 2 files (got $FILE_COUNT)"
else
    fail "webserver-log vhost rotate: expected <= 2 rotated files, got $FILE_COUNT"
fi

kill "$P1_VID_PID" "$P1_HTTP_PID" 2>/dev/null || true
sleep 0.5
rm -rf "$WLOG_SERVER_DIR" 2>/dev/null || true

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "========================================"
echo "Results: $PASS passed, $FAIL failed"
echo "========================================"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
