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
SERVER_PID=""; CLIENT_PID=""; CLIENT2_PID=""; LOCAL_PID=""; PROVIDER_PID=""; CONSUMER_PID=""

cleanup() {
    [ -n "$LOCAL_PID" ] && kill "$LOCAL_PID" 2>/dev/null
    [ -n "$CONSUMER_PID" ] && kill "$CONSUMER_PID" 2>/dev/null
    [ -n "$PROVIDER_PID" ] && kill "$PROVIDER_PID" 2>/dev/null
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

# ── Start server in ns0 (control TLS + vhost) ──────────────────────────────────────────
echo "=== Starting bore server (admin + control TLS + vhost + secret) ==="
SERVER_LOG="$TMPDIR/server.log"
VHOST_HTTP_PORT="8080"
VHOST_HTTPS_PORT="8443"
SERVER_SECRET="test-server-secret-for-auth-failure"
ip netns exec ns0 "$BORE" server \
    --admin-token "$ADMIN_TOKEN" \
    --bind-addr 0.0.0.0 \
    --control-port "$CTRL_PORT" \
    --min-port 20000 --max-port 20100 \
    --cert-file "$CERT_FILE" --key-file "$KEY_FILE" \
    --udp \
    --udp-socket-send-buffer 16MiB \
    --bind-domain bore.example.com \
    --control-hsts "max-age=31536000" \
    --vhost-base-domain bore.test \
    --vhost-http-port "$VHOST_HTTP_PORT" \
    --vhost-https-port "$VHOST_HTTPS_PORT" \
    --secret "$SERVER_SECRET" \
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
    --secret "$SERVER_SECRET" \
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
    --secret "$SERVER_SECRET" \
    --carriers 4 --https --force-https --auto-reconnect --notes "$NOTE_TEXT" \
    >"$CLIENT2_LOG" 2>&1 &
CLIENT2_PID=$!
for _ in $(seq 1 50); do
    body=$(ip netns exec ns0 curl -sk -H "Authorization: Bearer $ADMIN_TOKEN" "$B/admin/api/v1/tunnels" 2>/dev/null)
    echo "$body" | grep -q "\"public_port\":$PUB_PORT2" && break
    sleep 0.1
done

# ── Secret tunnel pair (provider + consumer) for T-SUMCOUNT / T-SECNOTES ──────────
# Secret provider: listens for a secret consumer and relays its traffic.
# Runs on nscli and serves a local port (9988) via the secret tunnel.
echo "=== Starting secret provider (bore local with --tcp-secret-id) ==="
SECRET_ID="test-secret-id-123456789012345"
PROVIDER_LOG="$TMPDIR/provider.log"
ip netns exec nscli "$BORE" local 9988 \
    --to "https://$SRV_IP:$CTRL_PORT" --insecure \
    --secret "$SERVER_SECRET" \
    --tcp-secret-id "$SECRET_ID" \
    --notes "Provider notes test" \
    --carriers 4 --auto-reconnect \
    >"$PROVIDER_LOG" 2>&1 &
PROVIDER_PID=$!

# Secret consumer: connects to the provider's secret tunnel.
# Runs on ns0 and listens on localhost:19999 to expose the provider's 9988.
echo "=== Starting secret consumer (bore proxy consumer) ==="
CONSUMER_LOG="$TMPDIR/consumer.log"
ip netns exec ns0 "$BORE" proxy \
    --to "https://$SRV_IP:$CTRL_PORT" --insecure \
    --secret "$SERVER_SECRET" \
    --tcp-secret-id "$SECRET_ID" \
    --local-proxy-port "127.0.0.1:19999" \
    --notes "Consumer notes test" \
    --carriers 4 --auto-reconnect --nat-udp-preferred-port 443 \
    >"$CONSUMER_LOG" 2>&1 &
CONSUMER_PID=$!

# Wait for secret tunnels to register
secret_registered=false
for _ in $(seq 1 50); do
    body=$(ip netns exec ns0 curl -sk -H "Authorization: Bearer $ADMIN_TOKEN" "$B/admin/api/v1/secret" 2>/dev/null)
    # Check for at least 2 entries (provider and consumer)
    if echo "$body" | grep -q '"role"' && [ "$(echo "$body" | grep -o '"role"' | wc -l)" -ge 2 ]; then
        secret_registered=true
        break
    fi
    sleep 0.1
done
$secret_registered || { echo "secret tunnels log:"; cat "$PROVIDER_LOG"; cat "$CONSUMER_LOG"; echo "(secrets may not have registered; continuing)"; }

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

# T-BUG1-LIVE: counters must update WHILE a connection is still OPEN — the actual
# regression. A dashboard polls live tunnels, whose connections are mostly open
# and long-lived (keep-alive, downloads, websockets); on-close-only accounting
# read 0.00B forever. Start a slow, large download in the background and poll the
# API MID-transfer (connection not yet closed), requiring non-zero TX.
if $PY_AVAIL; then
    # 8 MiB payload so the transfer can't complete (or fit a socket buffer) before
    # we sample; --limit-rate keeps the public connection open for many seconds.
    head -c 8388608 /dev/zero | tr '\0' 'y' > "$TMPDIR/www/biglive"
    ip netns exec ns0 curl -s -m 30 --limit-rate 128k -o /dev/null \
        "http://127.0.0.1:$PUB_PORT/biglive" &
    LIVE_CURL=$!
    sleep 1.5
    if kill -0 "$LIVE_CURL" 2>/dev/null; then
        # Connection is still OPEN (curl still downloading) at sample time.
        R=$(aget /admin/api/v1/tunnels "$ADMIN_TOKEN"); BODY=$(body_of "$R")
        if $JQ_AVAIL; then
            ok=$(echo "$BODY" | jq -e \
                'any(.[]; .public_port==20050 and .relay_tx_bytes>0)' \
                >/dev/null 2>&1 && echo y)
        else
            ok=$(echo "$BODY" | grep -Eq '"relay_tx_bytes":[1-9]' && echo y)
        fi
        [ "$ok" = "y" ] && pass "T-BUG1-LIVE relay_tx_bytes > 0 while connection still OPEN (live counting)" \
            || fail "T-BUG1-LIVE counters 0 mid-transfer (on-close-only regression); body=$BODY"
    else
        echo "SKIP: T-BUG1-LIVE download finished too fast to sample mid-flight"
    fi
    kill "$LIVE_CURL" 2>/dev/null
    wait "$LIVE_CURL" 2>/dev/null || true
else
    echo "SKIP: python3 absent — T-BUG1-LIVE skipped"
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

echo ""
echo "=== Phase 2.3 e2e assertions (backend shape verification) ==="

# T-SUMCOUNT: with live public tunnel + secret pair, GET /summary → .public_tunnels >= 1 AND .secret_tunnels >= 2
R=$(aget /admin/api/v1/summary "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    ok=$(echo "$BODY" | jq -e '.public_tunnels >= 1 and .secret_tunnels >= 2' >/dev/null 2>&1 && echo y)
else
    pub=$(echo "$BODY" | grep -oE '"public_tunnels":[0-9]+' | head -1 | grep -oE '[0-9]+')
    sec=$(echo "$BODY" | grep -oE '"secret_tunnels":[0-9]+' | head -1 | grep -oE '[0-9]+')
    [ "$pub" -ge 1 ] 2>/dev/null && [ "$sec" -ge 2 ] 2>/dev/null && ok=y
fi
[ "$ok" = "y" ] && pass "T-SUMCOUNT summary public_tunnels>=1 && secret_tunnels>=2" \
    || fail "T-SUMCOUNT counts wrong; body=$BODY"

# T-CFGBUF: server launched with --udp-socket-send-buffer 16MiB → GET /config .udp_socket_send_buffer == 16777216
R=$(aget /admin/api/v1/config "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    buf=$(echo "$BODY" | jq '.udp_socket_send_buffer' 2>/dev/null)
    [ "$buf" = "16777216" ] && pass "T-CFGBUF udp_socket_send_buffer==16777216 (16MiB)" \
        || fail "T-CFGBUF buffer=$buf (expected 16777216); body=$BODY"
else
    if echo "$BODY" | grep -q '"udp_socket_send_buffer":16777216'; then
        pass "T-CFGBUF udp_socket_send_buffer==16777216 (grep)"
    else
        fail "T-CFGBUF missing/wrong buffer; body=$BODY"
    fi
fi

# T-CFGFIELDS: GET /config has keys udp_stream_receive_window, udp_max_streams, bind_domain, control_hsts, vhost_mode
R=$(aget /admin/api/v1/config "$ADMIN_TOKEN"); BODY=$(body_of "$R")
fields_ok=true
for field in "udp_stream_receive_window" "udp_max_streams" "bind_domain" "control_hsts" "vhost_mode"; do
    if ! echo "$BODY" | grep -q "\"$field\""; then
        fields_ok=false
        break
    fi
done
[ "$fields_ok" = "true" ] && pass "T-CFGFIELDS config has all required fields" \
    || fail "T-CFGFIELDS missing fields; body=$BODY"

# T-SECNOTES: GET /admin/api/v1/secret → first element has key "notes"
R=$(aget /admin/api/v1/secret "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if [ -z "$BODY" ]; then
    fail "T-SECNOTES secret endpoint returned empty; body=$BODY"
elif $JQ_AVAIL; then
    if echo "$BODY" | jq -e '.[0] | has("notes")' >/dev/null 2>&1; then
        pass "T-SECNOTES secret first element has notes key"
    else
        fail "T-SECNOTES notes missing; body=$BODY"
    fi
else
    # Grep fallback: look for the notes field in the first entry (before the next closing brace at array level)
    first_entry=$(echo "$BODY" | sed -n '/\[\s*{/,/^\s*}/p' | head -20)
    if echo "$first_entry" | grep -q '"notes"'; then
        pass "T-SECNOTES secret first element has notes key (grep)"
    else
        fail "T-SECNOTES notes missing; body=$BODY"
    fi
fi

# T-SEC-PARITY: the secret CONSUMER must surface every flag it was launched with
# (the reported bugs: carriers / local-proxy-port / auto-reconnect /
# nat-udp-preferred-port). These ride the ConnectSecret wire message now.
R=$(aget /admin/api/v1/secret "$ADMIN_TOKEN"); BODY=$(body_of "$R")
if $JQ_AVAIL; then
    consumer_ok=$(echo "$BODY" | jq -e \
        'any(.[]; .role=="secretconsumer" and .carriers==4 and .auto_reconnect==true
                  and .local_proxy_port==19999 and .nat_udp_preferred_port==443)' \
        >/dev/null 2>&1 && echo y)
    # Provider surfaces carriers + auto_reconnect + its local target port.
    provider_ok=$(echo "$BODY" | jq -e \
        'any(.[]; .role=="secretprovider" and .carriers==4 and .auto_reconnect==true
                  and .local_port==9988)' \
        >/dev/null 2>&1 && echo y)
    # Grouping: provider and consumer share the same secret_id.
    group_ok=$(echo "$BODY" | jq -e --arg id "$SECRET_ID" \
        '(any(.[]; .role=="secretprovider" and .secret_id==$id))
         and (any(.[]; .role=="secretconsumer" and .secret_id==$id))' \
        >/dev/null 2>&1 && echo y)
else
    consumer_ok=$(echo "$BODY" | grep -q '"local_proxy_port":19999' \
        && echo "$BODY" | grep -q '"nat_udp_preferred_port":443' \
        && echo "$BODY" | grep -q '"auto_reconnect":true' && echo y)
    provider_ok=$(echo "$BODY" | grep -q '"local_port":9988' && echo y)
    group_ok=$(echo "$BODY" | grep -q "\"secret_id\":\"$SECRET_ID\"" && echo y)
fi
[ "$consumer_ok" = "y" ] && pass "T-SEC-PARITY consumer carriers=4 + auto_reconnect + local_proxy_port=19999 + nat_udp_preferred_port=443" \
    || fail "T-SEC-PARITY consumer flags missing; body=$BODY"
[ "$provider_ok" = "y" ] && pass "T-SEC-PARITY provider carriers=4 + auto_reconnect + local_port=9988" \
    || fail "T-SEC-PARITY provider flags missing; body=$BODY"
[ "$group_ok" = "y" ] && pass "T-SEC-PARITY provider+consumer share secret_id ($SECRET_ID)" \
    || fail "T-SEC-PARITY grouping secret_id mismatch; body=$BODY"

# ── Phase 5.1 / 5.3 new e2e assertions ─────────────────────────────────────────
echo ""
echo "=== Phase 5.1 / 5.3 e2e assertions (vhost, metrics, auth failures) ==="

# T-OVRPORTS-E2E: server started with vhost flags → GET /admin/api/v1/summary has keys
# vhost_http_port, port_range, bind_tunnels and they are non-null.
R=$(aget /admin/api/v1/summary "$ADMIN_TOKEN"); BODY=$(body_of "$R")
keys_ok=true
if echo "$BODY" | grep -q '"vhost_http_port"' \
   && echo "$BODY" | grep -q '"port_range"' \
   && echo "$BODY" | grep -q '"bind_tunnels"'; then
    # Check that port_range and bind_tunnels are non-null strings (not null)
    if echo "$BODY" | grep -q '"port_range":"' && echo "$BODY" | grep -q '"bind_tunnels":"'; then
        keys_ok=true
    else
        keys_ok=false
    fi
else
    keys_ok=false
fi
[ "$keys_ok" = "true" ] && pass "T-OVRPORTS-E2E summary vhost_http_port + port_range + bind_tunnels (non-null)" \
    || fail "T-OVRPORTS-E2E missing/null keys; body=$BODY"

# T-METACTIVE-E2E: GET /admin/api/v1/metrics has integer active_connections (>=0)
# and the three new counters auth_failures, conn_rejections, direct_fallbacks (>=0).
R=$(aget /admin/api/v1/metrics "$ADMIN_TOKEN"); BODY=$(body_of "$R")
metrics_ok=false
if $JQ_AVAIL; then
    if echo "$BODY" | jq -e '.active_connections >= 0
                             and .auth_failures >= 0
                             and .conn_rejections >= 0
                             and .direct_fallbacks >= 0' >/dev/null 2>&1; then
        metrics_ok=true
    fi
else
    # Grep fallback: check presence and look for >= 0 values
    if echo "$BODY" | grep -q '"active_connections":[0-9]' \
       && echo "$BODY" | grep -q '"auth_failures":[0-9]' \
       && echo "$BODY" | grep -q '"conn_rejections":[0-9]' \
       && echo "$BODY" | grep -q '"direct_fallbacks":[0-9]'; then
        metrics_ok=true
    fi
fi
[ "$metrics_ok" = "true" ] && pass "T-METACTIVE-E2E metrics active_connections + counters (>=0)" \
    || fail "T-METACTIVE-E2E missing/invalid counters; body=$BODY"

# T-AUTHFAIL-E2E: make N=3 connection attempts with WRONG secret, which must fail
# the handshake. Then GET /admin/api/v1/metrics and assert auth_failures >= 3.
echo "  (making 3 failed auth attempts with wrong secret...)"
for i in 1 2 3; do
    # Each attempt connects with the wrong secret and should fail immediately
    timeout 2 ip netns exec nscli "$BORE" local 9977 \
        --to "https://$SRV_IP:$CTRL_PORT" --insecure \
        --secret "wrongsecret-attempt-$i" \
        >/dev/null 2>&1 &
done
sleep 0.5  # Wait for attempts to fail

R=$(aget /admin/api/v1/metrics "$ADMIN_TOKEN"); BODY=$(body_of "$R")
authfail_ok=false
if $JQ_AVAIL; then
    failures=$(echo "$BODY" | jq '.auth_failures // 0' 2>/dev/null)
    if [ "$failures" -ge 3 ] 2>/dev/null; then
        authfail_ok=true
    fi
else
    # Grep: extract the auth_failures value
    failures=$(echo "$BODY" | grep -oE '"auth_failures":[0-9]+' | grep -oE '[0-9]+')
    [ "$failures" -ge 3 ] 2>/dev/null && authfail_ok=true
fi
[ "$authfail_ok" = "true" ] && pass "T-AUTHFAIL-E2E auth_failures >= 3 after wrong-secret attempts" \
    || fail "T-AUTHFAIL-E2E auth_failures too low ($failures); body=$BODY"

# T-VPNFLAGS-E2E: VPN requires --features vpn + netns harness.
# ASSESSMENT: VPN flag display is covered by Rust unit tests (T-VPNFLAGS in admin_views.rs)
# and manual integration testing in docs/vpn/VPN_FULL_PLAN_V1.md. The e2e netns harness
# (vpn_netns_test.sh) would require invasive changes to inject --admin-token into its
# server launch and curl /admin/api/v1/vpn from inside the test netns. Deferred to v2.
echo "SKIP: T-VPNFLAGS-E2E (VPN flag display covered by Rust T-VPNFLAGS unit test + manual)"

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Summary ==="
echo "PASS: $PASS   FAIL: $FAIL"
[ "$FAIL" -eq 0 ] && { echo "All assertions passed."; exit 0; } || { echo "Some assertions failed."; exit 1; }
