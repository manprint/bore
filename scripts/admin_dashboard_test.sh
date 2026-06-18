#!/usr/bin/env bash
# Admin Dashboard e2e test harness — Phase 5.1 contract assertions.
# Drives the admin API + asset serving against the reference scenario (§1 of
# docs/frontend/ADMIN_DASHBOARD_PLAN.md) over a real netns topology.
#
# Topology:
#   ns0  (server) — veth pair 10.221.0.0/30 — nscli (bore local client)
#   ns0 = 10.221.0.2 (server), nscli = 10.221.0.1 (client, deterministic peer IP)
#   The server runs control TLS (--cert-file/--key-file) so the certs endpoint
#   has a cert to report; admin calls go over HTTPS (curl -k) from inside ns0.
#
# Usage: sudo scripts/admin_dashboard_test.sh   (invoked directly, per sudoers)

set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
BORE="${BORE:-$HERE/../target/release/bore}"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release --features vpn" >&2
    exit 1
fi
if find "$HERE/../src" "$HERE/../Cargo.toml" -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn" >&2
    exit 1
fi
for cmd in ip curl openssl; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "SKIP: $cmd not installed" >&2; exit 0; }
done
JQ_AVAIL=false; command -v jq >/dev/null 2>&1 && JQ_AVAIL=true

ADMIN_TOKEN="0123456789abcdef0123456789abcdef01234567"  # 40 chars
SRV_IP="10.221.0.2"      # ns0 (server) end of the veth
CLI_IP="10.221.0.1"      # nscli (client) end of the veth — the expected peer IP
PUB_PORT="20050"         # explicit public port requested by the client (plain — TX/RX test)
PUB_PORT2="20051"        # second tunnel: all-flags client (BUG-3 flag assertions)
CTRL_PORT="7835"
LOCAL_SVC_PORT="9999"    # local HTTP service the plain tunnel forwards to

PASS=0; FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; exit 1; }

TMPDIR="/tmp/bore_admin_$$"
SERVER_PID=""; CLIENT_PID=""; CLIENT2_PID=""; LOCAL_PID=""

cleanup() {
    [ -n "$LOCAL_PID" ] && kill "$LOCAL_PID" 2>/dev/null
    [ -n "$CLIENT2_PID" ] && kill "$CLIENT2_PID" 2>/dev/null
    [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in ns0 nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$TMPDIR" 2>/dev/null
}
trap cleanup EXIT INT TERM

# ── Setup netns ──────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ==="
for ns in ns0 nscli; do ip netns del "$ns" 2>/dev/null || true; done
ip netns add ns0
ip netns add nscli
ip link add veths type veth peer name vethc
ip link set veths netns ns0
ip link set vethc netns nscli
ip netns exec ns0   ip addr add "$SRV_IP/30" dev veths
ip netns exec nscli ip addr add "$CLI_IP/30" dev vethc
ip netns exec ns0   ip link set veths up
ip netns exec nscli ip link set vethc up
ip netns exec ns0   ip link set lo up
ip netns exec nscli ip link set lo up

# ── Cert for control TLS ───────────────────────────────────────────────────────
mkdir -p "$TMPDIR"
CERT_FILE="$TMPDIR/server.crt"; KEY_FILE="$TMPDIR/server.key"
openssl req -x509 -newkey rsa:2048 -keyout "$KEY_FILE" -out "$CERT_FILE" \
    -days 365 -nodes -subj "/CN=bore.test" -addext "subjectAltName=DNS:bore.test" 2>/dev/null \
    || die "failed to generate cert"

# ── Start server in ns0 (control TLS) ──────────────────────────────────────────
echo "=== Starting bore server (admin + control TLS) ==="
SERVER_LOG="$TMPDIR/server.log"
ip netns exec ns0 "$BORE" server \
    --admin-token "$ADMIN_TOKEN" \
    --bind-addr 0.0.0.0 \
    --control-port "$CTRL_PORT" \
    --min-port 20000 --max-port 20100 \
    --cert-file "$CERT_FILE" --key-file "$KEY_FILE" \
    --udp \
    >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

# Wait for the control port to accept TLS (admin shell answers 200).
B="https://127.0.0.1:$CTRL_PORT"
ready=false
for _ in $(seq 1 50); do
    code=$(ip netns exec ns0 curl -sk -o /dev/null -w '%{http_code}' "$B/admin/status" 2>/dev/null)
    [ "$code" = "200" ] && { ready=true; break; }
    sleep 0.1
done
$ready || { cat "$SERVER_LOG"; die "server control port not ready"; }
echo "  Server up (pid $SERVER_PID)"

# ── Local HTTP service the plain tunnel forwards to (for TX/RX accounting) ──────
# Optional: only runs if python3 is present; the byte-counter assertions skip
# otherwise. Serves a fixed 64 KiB payload so a download moves real bytes.
PY_AVAIL=false; command -v python3 >/dev/null 2>&1 && PY_AVAIL=true
PAYLOAD_BYTES=65536
if $PY_AVAIL; then
    mkdir -p "$TMPDIR/www"
    head -c "$PAYLOAD_BYTES" /dev/zero | tr '\0' 'x' > "$TMPDIR/www/payload"
    ( cd "$TMPDIR/www" && exec ip netns exec nscli python3 -m http.server "$LOCAL_SVC_PORT" ) \
        >"$TMPDIR/httpd.log" 2>&1 &
    LOCAL_PID=$!
fi

# ── Start a public tunnel from nscli (deterministic peer IP + port) ─────────────
echo "=== Starting public tunnel (bore local from $CLI_IP) ==="
CLIENT_LOG="$TMPDIR/client.log"
ip netns exec nscli "$BORE" local "$LOCAL_SVC_PORT" \
    --to "https://$SRV_IP:$CTRL_PORT" --insecure -p "$PUB_PORT" \
    >"$CLIENT_LOG" 2>&1 &
CLIENT_PID=$!

# Wait until the tunnel registers (its entry appears in the admin API).
registered=false
for _ in $(seq 1 50); do
    body=$(ip netns exec ns0 curl -sk -H "Authorization: Bearer $ADMIN_TOKEN" "$B/admin/api/v1/tunnels" 2>/dev/null)
    echo "$body" | grep -q "\"public_port\":$PUB_PORT" && { registered=true; break; }
    sleep 0.1
done
$registered || { echo "client log:"; cat "$CLIENT_LOG"; echo "(tunnel did not register; continuing)"; }

# ── Second tunnel: an all-flags client (BUG-3: carriers/force_https/auto_reconnect/notes) ──
echo "=== Starting all-flags public tunnel (port $PUB_PORT2) ==="
CLIENT2_LOG="$TMPDIR/client2.log"
NOTE_TEXT="superdufs lenovo lavoro 5353"
ip netns exec nscli "$BORE" local 9998 \
    --to "https://$SRV_IP:$CTRL_PORT" --insecure -p "$PUB_PORT2" \
    --carriers 4 --https --force-https --auto-reconnect --notes "$NOTE_TEXT" \
    >"$CLIENT2_LOG" 2>&1 &
CLIENT2_PID=$!
for _ in $(seq 1 50); do
    body=$(ip netns exec ns0 curl -sk -H "Authorization: Bearer $ADMIN_TOKEN" "$B/admin/api/v1/tunnels" 2>/dev/null)
    echo "$body" | grep -q "\"public_port\":$PUB_PORT2" && break
    sleep 0.1
done

# ── HTTP helper (admin calls over HTTPS from inside ns0) ─────────────────────────
# Usage: aget <path> [token]   → echoes "<http_code>\n<body>"
aget() {
    local path="$1" token="${2:-}"
    local hdr=()
    [ -n "$token" ] && hdr=(-H "Authorization: Bearer $token")
    ip netns exec ns0 curl -sk -m 10 "${hdr[@]}" -w $'\n%{http_code}' "$B$path" 2>/dev/null
}
code_of() { echo "$1" | tail -1; }
body_of() { echo "$1" | sed '$d'; }

echo ""
echo "=== T-REF: reference-scenario contract assertions ==="

# T-SHELL: /admin/status → 200 text/html
R=$(aget /admin/status); C=$(code_of "$R")
ct=$(ip netns exec ns0 curl -sk -o /dev/null -w '%{content_type}' "$B/admin/status" 2>/dev/null)
if [ "$C" = "200" ] && echo "$ct" | grep -q "text/html"; then
    pass "T-SHELL /admin/status → 200 text/html"
else fail "T-SHELL got code=$C ct=$ct"; fi

# T-ASSET: /admin/ui/app.js → 200 javascript
C=$(ip netns exec ns0 curl -sk -o /dev/null -w '%{http_code}' "$B/admin/ui/app.js" 2>/dev/null)
ct=$(ip netns exec ns0 curl -sk -o /dev/null -w '%{content_type}' "$B/admin/ui/app.js" 2>/dev/null)
if [ "$C" = "200" ] && echo "$ct" | grep -q "javascript"; then
    pass "T-ASSET /admin/ui/app.js → 200 $ct"
else fail "T-ASSET got code=$C ct=$ct"; fi

# T-ASSET path traversal → 404
C=$(ip netns exec ns0 curl -sk --path-as-is -o /dev/null -w '%{http_code}' "$B/admin/ui/../Cargo.toml" 2>/dev/null)
if [ "$C" = "404" ]; then pass "T-TRAVERSAL /admin/ui/../Cargo.toml → 404"; else fail "T-TRAVERSAL got $C"; fi

# T-AUTH-E2E: no token → 401, Bearer → 200 JSON
C=$(ip netns exec ns0 curl -sk -o /dev/null -w '%{http_code}' "$B/admin/api/v1/tunnels" 2>/dev/null)
[ "$C" = "401" ] && pass "T-AUTH no token → 401" || fail "T-AUTH no token got $C"
R=$(aget /admin/api/v1/tunnels "$ADMIN_TOKEN"); C=$(code_of "$R")
[ "$C" = "200" ] && pass "T-AUTH Bearer → 200" || fail "T-AUTH Bearer got $C"

# T-REF1: tunnels has the assigned port + the client's real IP as peer
R=$(aget /admin/api/v1/tunnels "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    ok=$(echo "$BODY" | jq -e --arg ip "$CLI_IP" \
        'any(.[]; .public_port==20050 and (.peer|startswith($ip+":")))' >/dev/null 2>&1 && echo y)
else
    ok=$(echo "$BODY" | grep -q "\"public_port\":$PUB_PORT" && echo "$BODY" | grep -q "\"peer\":\"$CLI_IP:" && echo y)
fi
[ "$ok" = "y" ] && pass "T-REF1 tunnels entry public_port=$PUB_PORT peer=$CLI_IP" \
    || fail "T-REF1 missing port/peer; body=$BODY"

# T-REF2: certs has integer days_remaining + RFC3339 not_after
R=$(aget /admin/api/v1/certs "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    if echo "$BODY" | jq -e '.[0].days_remaining|type=="number"' >/dev/null 2>&1 \
       && echo "$BODY" | jq -e '.[0].not_after|test("Z$")' >/dev/null 2>&1; then
        pass "T-REF2 certs days_remaining(int) + RFC3339 not_after"
    else fail "T-REF2 cert shape wrong; body=$BODY"; fi
else
    if echo "$BODY" | grep -Eq '"days_remaining":-?[0-9]+' && echo "$BODY" | grep -Eq '"not_after":"[^"]*Z"'; then
        pass "T-REF2 certs days_remaining + RFC3339 not_after (grep)"
    else fail "T-REF2 cert shape wrong; body=$BODY"; fi
fi

# T-REF3: config has control_port, no admin_token
R=$(aget /admin/api/v1/config "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if echo "$BODY" | grep -q '"control_port"' && ! echo "$BODY" | grep -q '"admin_token"'; then
    pass "T-REF3 config has control_port, no admin_token"
else fail "T-REF3 config wrong; body=$BODY"; fi

# T-REF4: metrics has uptime_secs + bandwidth_tx_bytes
R=$(aget /admin/api/v1/metrics "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    if echo "$BODY" | jq -e '.uptime_secs>=0 and .bandwidth_tx_bytes>=0' >/dev/null 2>&1; then
        pass "T-REF4 metrics uptime_secs>=0, bandwidth_tx_bytes>=0"
    else fail "T-REF4 metrics wrong; body=$BODY"; fi
else
    if echo "$BODY" | grep -q '"uptime_secs"' && echo "$BODY" | grep -q '"bandwidth_tx_bytes"'; then
        pass "T-REF4 metrics has uptime_secs + bandwidth_tx_bytes (grep)"
    else fail "T-REF4 metrics wrong; body=$BODY"; fi
fi

# T-VPN: vpn endpoint returns a links array (built with --features vpn)
R=$(aget /admin/api/v1/vpn "$ADMIN_TOKEN"); C=$(code_of "$R"); BODY=$(body_of "$R")
if [ "$C" = "200" ] && echo "$BODY" | grep -q '"links"'; then
    pass "T-VPN /admin/api/v1/vpn → 200 with links array"
else fail "T-VPN got code=$C body=$BODY"; fi

# T-COMPAT-E2E: legacy /admin/status/data shape unchanged
R=$(aget /admin/status/data "$ADMIN_TOKEN"); C=$(code_of "$R"); BODY=$(body_of "$R")
if [ "$C" = "200" ] && echo "$BODY" | grep -q '"server"' && echo "$BODY" | grep -q '"tunnels"'; then
    pass "T-COMPAT /admin/status/data legacy shape (server+tunnels)"
else fail "T-COMPAT got code=$C body=$BODY"; fi

echo ""
echo "=== Bug-fix contract assertions ==="

# T-BUG3: all-flags tunnel exposes carriers + force_https + auto_reconnect + notes.
R=$(aget /admin/api/v1/tunnels "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    ok=$(echo "$BODY" | jq -e --arg n "$NOTE_TEXT" \
        'any(.[]; .public_port==20051 and .carriers==4 and .force_https==true and .auto_reconnect==true and .notes==$n)' \
        >/dev/null 2>&1 && echo y)
else
    ok=$(echo "$BODY" | grep -q '"carriers":4' \
        && echo "$BODY" | grep -q '"force_https":true' \
        && echo "$BODY" | grep -q '"auto_reconnect":true' && echo y)
fi
[ "$ok" = "y" ] && pass "T-BUG3 carriers=4 + force_https + auto_reconnect + notes in JSON" \
    || fail "T-BUG3 missing flags; body=$BODY"

# T-BUG1: per-tunnel TX/RX increment after a real transfer (was always 0).
if $PY_AVAIL; then
    ip netns exec ns0 curl -s -m 10 -o /dev/null "http://127.0.0.1:$PUB_PORT/payload" 2>/dev/null
    sleep 0.3
    R=$(aget /admin/api/v1/tunnels "$ADMIN_TOKEN"); BODY=$(body_of "$R")
    if $JQ_AVAIL; then
        ok=$(echo "$BODY" | jq -e \
            'any(.[]; .public_port==20050 and .relay_tx_bytes>0 and .relay_rx_bytes>0)' \
            >/dev/null 2>&1 && echo y)
    else
        ok=$(echo "$BODY" | grep -Eq '"relay_tx_bytes":[1-9]' && echo y)
    fi
    [ "$ok" = "y" ] && pass "T-BUG1 relay_tx_bytes/relay_rx_bytes > 0 after transfer" \
        || fail "T-BUG1 counters still 0 after transfer; body=$BODY"
else
    echo "SKIP: python3 absent — T-BUG1 TX/RX transfer test skipped"
fi

# T-BUG4: certs are deduped. The harness configures only the control cert, so
# exactly one entry must be reported (a regression that double-counts would show
# 2). The same-file control+vhost merge is covered by the Rust unit tests.
R=$(aget /admin/api/v1/certs "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    n=$(echo "$BODY" | jq 'length' 2>/dev/null)
    [ "$n" = "1" ] && pass "T-BUG4 certs single entry (len=1, no duplicate)" \
        || fail "T-BUG4 certs len=$n; body=$BODY"
else
    cnt=$(echo "$BODY" | grep -o '"label"' | wc -l)
    [ "$cnt" = "1" ] && pass "T-BUG4 certs single entry (grep label count=1)" \
        || fail "T-BUG4 label count=$cnt; body=$BODY"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Summary ==="
echo "PASS: $PASS   FAIL: $FAIL"
[ "$FAIL" -eq 0 ] && { echo "All assertions passed."; exit 0; } || { echo "Some assertions failed."; exit 1; }
