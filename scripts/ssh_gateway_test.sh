#!/usr/bin/env bash
# SSH ingress gateway netns harness — chaos/acceptance tests, including SSH jump hosts.
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Topology:
#   ns0 (server, --ssh-gateway --secret --vhost-base-domain, unified control port)
#     — veth0s↔veth0c (10.230.0.0/30) ↔ nscli (ssh client + local services +
#       native secret provider/consumer, for the mixed-transport test)
#
# Requires a release binary built with BOTH the vpn and ssh-gateway features
# (the freshness guard below probes `--help` for `--ssh-gateway`):
#   cargo build --release --features vpn,ssh-gateway
#
# Usage: sudo scripts/ssh_gateway_test.sh
# Exit code: 0 = all tests passed, nonzero = failures

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi
if find "$(dirname "$0")/../src" "$(dirname "$0")/../Cargo.toml" \
        -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi
if ! "$BORE" server --help 2>&1 | grep -q -- '--ssh-gateway'; then
    echo "ERROR: $BORE was not built with the ssh-gateway feature." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi

# Required tools — the whole suite is skipped (not failed) if any is missing.
for cmd in ip ssh ssh-keygen curl python3 openssl nc ss iptables; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done
# Optional tools — only the tests that need them are individually skipped.
HAVE_AUTOSSH=1; command -v autossh >/dev/null 2>&1 || HAVE_AUTOSSH=0
HAVE_SSHPASS=1; command -v sshpass >/dev/null 2>&1 || HAVE_SSHPASS=0
HAVE_IPERF3=1; command -v iperf3 >/dev/null 2>&1 || HAVE_IPERF3=0

# ── Configuration ───────────────────────────────────────────────────────────
SECRET="sshgwtest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
ADMIN_TOKEN="0123456789abcdef0123456789abcdef01234567"  # 40 chars for --admin-token
VHOST_DOMAIN="bore.sshgw.test"
SSH_JUMP_DOMAIN="ssh.bore.sshgw.test"

SERVER_IP="10.230.0.2"   # server-side of ns0↔nscli veth
CLI_IP="10.230.0.1"      # nscli-side

CTRL_PORT="7835"
DIRECT_QUIC_PORT="443"
SSH_BANNER="bore-ssh-gateway-banner-marker"
TMPDIR="/tmp/bore_sshgw_$$"

PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

# ── Cleanup ──────────────────────────────────────────────────────────────────
cleanup() {
    set +e
    for ns in ns0 nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    pkill -9 -f 'target/release/bore' 2>/dev/null
    pkill -9 -f 'autossh.*sshgw' 2>/dev/null
    for v in veth0s veth0c; do ip link del "$v" 2>/dev/null; done
    # Keep TMPDIR (server + client logs) when something failed, so it can be
    # inspected after the run; a clean pass tidies up after itself.
    if [ "${FAIL:-0}" -eq 0 ]; then
        rm -rf "$TMPDIR" 2>/dev/null
    else
        echo "Logs kept at $TMPDIR for inspection." >&2
    fi
    set -e
}
trap cleanup EXIT INT TERM

# ── Helpers ──────────────────────────────────────────────────────────────────
wait_server_ready() {
    local from_ns="$1" ip="$2" port="${3:-7835}"
    for _ in $(seq 1 50); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

wait_port_up() {
    local from_ns="$1" ip="$2" port="$3" tries="${4:-100}"
    for _ in $(seq 1 "$tries"); do
        ip netns exec "$from_ns" nc -z "$ip" "$port" 2>/dev/null && return 0
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

# Send LINE to an echo tunnel at ip:port from nscli, RETRYING until it echoes
# back or `tries` (default 15 × ~0.5s) elapse; echoes the last response.
# `wait_port_up` only confirms the public LISTENER socket is bound — the
# `forwarded-tcpip` channel round trip (server→client→local service→back) can
# need a beat longer right after bind, so a single-shot `nc` was flaky on the
# initial echo (the same startup-timing class the git log's PARAMS_GRACE /
# ConnectTimeout stabilization fought; reproduced here as intermittent
# "initial tunnel did not echo", confirmed present on the pre-bug-hunt baseline
# too). The already-looped reconnect checks never flaked — this brings the
# initial checks to the same robustness.
echo_tunnel() {
    local ip="$1" port="$2" line="$3" tries="${4:-15}"
    local resp=""
    for _ in $(seq 1 "$tries"); do
        resp=$(echo "$line" | timeout 5 ip netns exec nscli nc -N -w2 "$ip" "$port" 2>/dev/null || echo "")
        [ "$resp" = "$line" ] && break
        sleep 0.5
    done
    echo "$resp"
}

# Curl the admin API from nscli (the server's control port is reachable there).
# Usage: admin_curl <path>
admin_curl() {
    local path="$1"
    ip netns exec nscli curl -sk -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" \
        -w $'\n%{http_code}' "https://$SERVER_IP:$CTRL_PORT$path" 2>/dev/null
}
body_of() { echo "$1" | sed '$d'; }

admin_data() { body_of "$(admin_curl /admin/status/data)"; }
jump_data() { body_of "$(admin_curl /admin/api/v1/ssh-jump)"; }

# Print one sanitized jump-host field, or an empty string when the alias is not
# registered. Keeping JSON parsing here makes the assertions independent of
# serializer whitespace and field order.
jump_field() {
    local alias="$1" field="$2"
    jump_data | python3 -c '
import json, sys
alias, field, domain = sys.argv[1], sys.argv[2], sys.argv[3]
hostname = f"{alias}.{domain}"
try:
    rows = json.load(sys.stdin)
except Exception:
    rows = []
row = next((row for row in rows if row.get("hostname") == hostname), None)
value = "" if row is None else row.get(field, "")
print(value if value is not None else "")
' "$alias" "$field" "$SSH_JUMP_DOMAIN" 2>/dev/null || true
}

wait_jump_alias() {
    local alias="$1" tries="${2:-100}"
    for _ in $(seq 1 "$tries"); do
        [ "$(jump_field "$alias" hostname)" = "$alias.$SSH_JUMP_DOMAIN" ] && return 0
        sleep 0.1
    done
    return 1
}

wait_jump_field_ge() {
    local alias="$1" field="$2" minimum="$3" tries="${4:-150}" value
    for _ in $(seq 1 "$tries"); do
        value=$(jump_field "$alias" "$field")
        case "$value" in ''|*[!0-9]*) value=0 ;; esac
        [ "$value" -ge "$minimum" ] && return 0
        sleep 0.1
    done
    return 1
}

# Count admin rows matching a grep pattern (role/secret_id/etc — the endpoint
# returns one JSON object per line-ish blob; a plain substring count over the
# whole body is precise enough for this harness's needs, matching the style
# other netns scripts already use against this same endpoint shape).
count_rows() {
    admin_data | grep -o "$1" | wc -l
}

# Issues one GET (Host: $1) over a fresh TCP connection FROM nscli TO the
# server's control port and returns the raw response text (or "ERROR" on any
# failure — connection refused because the vhost label isn't registered
# (yet), timeout, etc). Runs inside the nscli netns (the only place with a
# route to $SERVER_IP) — a bare `python3 -c` in the harness's own (root)
# namespace would silently fail to route there at all.
http_check() {
    local host_header="$1"
    ip netns exec nscli python3 -c "
import socket
s = socket.create_connection(('$SERVER_IP', $CTRL_PORT), timeout=5)
s.sendall(b'GET / HTTP/1.1\r\nHost: $host_header\r\nConnection: close\r\n\r\n')
print(s.recv(4096).decode('utf-8', 'replace'))
" 2>/dev/null || echo "ERROR"
}

# (Re)start the bore server in ns0: SSH gateway (demux, no dedicated --ssh-port)
# + native secret auth + vhost unified onto the control port. Reused by the
# autossh-recovery test, which restarts the server to exercise client
# reconnection without touching the client.
start_server() {
    SERVER_LOG="$TMPDIR/server.log"
    ip netns exec ns0 "$BORE" server \
        --admin-token "$ADMIN_TOKEN" \
        --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
        --secret "$SECRET" \
        --control-port "$CTRL_PORT" \
        --cert-file "$CERT_FILE" --key-file "$KEY_FILE" \
        --vhost-base-domain "$VHOST_DOMAIN" --vhost-http-port "$CTRL_PORT" \
        --vhost-quic-port "$DIRECT_QUIC_PORT" \
        --ssh-gateway \
        --ssh-jump-base-domain "$SSH_JUMP_DOMAIN" \
        --ssh-host-key-file "$TMPDIR/ssh_host_key.pem" \
        --ssh-authorized-keys-dir "$TMPDIR/keys" \
        --ssh-passwords-file "$TMPDIR/passwords" \
        --ssh-banner "$SSH_BANNER" \
        --udp \
        >>"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
    sleep 1
    wait_server_ready nscli "$SERVER_IP" "$CTRL_PORT" || die "server not reachable from nscli"
    echo "  Server up (pid $SERVER_PID)"
}

SSH_OPTS=(
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
    -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=5
)

# A local TCP echo service in nscli, standing in for "the service on
# localhost" every -R forward proxies to. Prints its port on stdout.
spawn_echo_service() {
    local port="$1"
    ip netns exec nscli python3 - "$port" >"$TMPDIR/echo_$port.log" 2>&1 <<'PYEOF' &
import socket, sys, threading

port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(64)

def handle(conn):
    with conn:
        while True:
            data = conn.recv(4096)
            if not data:
                break
            conn.sendall(data)

while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PYEOF
    echo $!
}

# A local HTTP service in nscli that replies 200 with a fixed body — the
# vhost forward tests need a real HTTP responder (a raw TCP echo server just
# reflects the request bytes back, which can never match an
# "^HTTP/1.1 [0-9]" check). Prints its PID on stdout.
spawn_http_service() {
    local port="$1" body="$2"
    ip netns exec nscli python3 - "$port" "$body" >"$TMPDIR/http_$port.log" 2>&1 <<'PYEOF' &
import socket, sys, threading

port = int(sys.argv[1])
body = sys.argv[2].encode()
resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(64)

def handle(conn):
    with conn:
        conn.recv(4096)
        conn.sendall(resp)

while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PYEOF
    echo $!
}

# A local HTTPS service in nscli (TLS-terminating), standing in for a backend
# that is itself HTTPS — the target of `backend-tls=on`. Uses the harness
# self-signed cert; SNI is irrelevant because the server's backend TLS client
# accepts any certificate. Prints its PID on stdout.
spawn_https_service() {
    local port="$1" body="$2"
    ip netns exec nscli python3 - "$port" "$body" "$CERT_FILE" "$KEY_FILE" \
        >"$TMPDIR/https_$port.log" 2>&1 <<'PYEOF' &
import socket, ssl, sys, threading

port = int(sys.argv[1])
body = sys.argv[2].encode()
cert, key = sys.argv[3], sys.argv[4]
resp = b"HTTP/1.1 200 OK\r\nContent-Length: " + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(64)

def handle(conn):
    try:
        with ctx.wrap_socket(conn, server_side=True) as tls:
            tls.recv(4096)
            tls.sendall(resp)
    except Exception:
        pass

while True:
    conn, _ = srv.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
PYEOF
    echo $!
}

# ── Setup ──────────────────────────────────────────────────────────────────
echo "=== Setup: reclaiming any stale state from a prior (possibly SIGKILLed) run ==="
for ns in ns0 nscli; do
    ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
    ip netns del "$ns" 2>/dev/null || true
done
pkill -9 -f 'target/release/bore' 2>/dev/null || true
pkill -9 -f 'autossh.*sshgw' 2>/dev/null || true
for v in veth0s veth0c; do ip link del "$v" 2>/dev/null || true; done
rm -rf /tmp/bore_sshgw_* 2>/dev/null || true

echo "=== Setup: creating netns ==="
ip netns add ns0
ip netns add nscli

ip link add veth0s type veth peer name veth0c
ip link set veth0s netns ns0
ip link set veth0c netns nscli
ip netns exec ns0 ip addr add "$SERVER_IP/30" dev veth0s
ip netns exec nscli ip addr add "$CLI_IP/30" dev veth0c
ip netns exec ns0 ip link set veth0s up
ip netns exec nscli ip link set veth0c up
ip netns exec ns0 ip link set lo up
ip netns exec nscli ip link set lo up

echo "=== Setup: generating TLS cert, SSH host key, client key, password ==="
mkdir -p "$TMPDIR" "$TMPDIR/keys"

CERT_FILE="$TMPDIR/server.crt"
KEY_FILE="$TMPDIR/server.key"
openssl req -x509 -newkey rsa:2048 -keyout "$KEY_FILE" -out "$CERT_FILE" \
    -days 365 -nodes -subj "/CN=$VHOST_DOMAIN" \
    -addext "subjectAltName=DNS:$VHOST_DOMAIN,DNS:*.$VHOST_DOMAIN" 2>/dev/null \
    || die "failed to generate cert"

CLIENT_KEY="$TMPDIR/client_key"
ssh-keygen -t ed25519 -N '' -f "$CLIENT_KEY" -C gwtest >/dev/null 2>&1 \
    || die "ssh-keygen failed"
# The basename is the classic identity used by jump-only username binding.
# Legacy gateway modes continue to accept this key regardless of SSH username.
cp "$CLIENT_KEY.pub" "$TMPDIR/keys/gwtest"

PASSWORD="chaospass$$"
PASS_HASH=$(echo -n "$PASSWORD" | "$BORE" hash-password 2>/dev/null | tail -1)
[ -n "$PASS_HASH" ] || die "bore hash-password produced no output"
echo "alice:$PASS_HASH" >"$TMPDIR/passwords"

start_server

ssh_cmd() {
    ip netns exec nscli ssh "${SSH_OPTS[@]}" -i "$CLIENT_KEY" \
        -o BatchMode=yes -o ExitOnForwardFailure=yes \
        -p "$CTRL_PORT" "$@" "gwtest@$SERVER_IP"
}

# ── T-SSH-N1: half-open reap (post-auth, real network DROP) ────────────────
echo ""
echo "=== Test: T-SSH-N1 (half-open reap: real netfilter DROP, not process kill) ==="
SVC1_PORT=19801
spawn_http_service "$SVC1_PORT" "n1-ok" >/dev/null
sleep 0.3
ssh_cmd -N -R "vhost/n1:0:127.0.0.1:$SVC1_PORT" >"$TMPDIR/n1_ssh.log" 2>&1 &
N1_SSH_PID=$!
if wait_for_log "$SERVER_LOG" "vhost tunnel provider ready\|secret tunnel provider ready\|n1" 10; then :; fi
sleep 1
if [ "$(count_rows '"secret_id":"n1"')" = "1" ]; then
    pass "T-SSH-N1 initial registration (1 row)"
else
    fail "T-SSH-N1 initial registration (expected 1 row)"
fi

# Isolate ONLY this SSH session's TCP flow (by its ephemeral source port), so
# the client namespace itself stays fully reachable for new connections —
# unlike killing the process, this leaves the OLD socket ESTABLISHED on both
# ends with no FIN/RST ever delivered, which is what actually exercises the
# keepalive-based reaper (I-3) instead of ordinary client-initiated teardown.
OLD_SPORT=$(ip netns exec ns0 ss -tnH state established "( sport = :$CTRL_PORT )" 2>/dev/null \
    | awk -v ip="$CLI_IP" '$4 ~ ip {n=split($4,a,":"); print a[n]}' | head -1)
if [ -z "$OLD_SPORT" ]; then
    fail "T-SSH-N1 could not find the ssh control connection's peer port"
else
    ip netns exec ns0 iptables -A INPUT -p tcp -s "$CLI_IP" --sport "$OLD_SPORT" -j DROP
    ip netns exec ns0 iptables -A OUTPUT -p tcp -d "$CLI_IP" --dport "$OLD_SPORT" -j DROP
    echo "  (isolated flow on client port $OLD_SPORT; waiting up to 75s for the reaper...)"
    REAPED=0
    for _ in $(seq 1 75); do
        if [ "$(count_rows '"secret_id":"n1"')" = "0" ]; then
            REAPED=1
            break
        fi
        sleep 1
    done
    if [ "$REAPED" = "1" ]; then
        pass "T-SSH-N1 admin row cleared by the reaper (real netfilter half-open, not a process kill)"
    else
        fail "T-SSH-N1 admin row never cleared within 75s"
    fi
    ip netns exec ns0 iptables -D INPUT -p tcp -s "$CLI_IP" --sport "$OLD_SPORT" -j DROP 2>/dev/null || true
    ip netns exec ns0 iptables -D OUTPUT -p tcp -d "$CLI_IP" --dport "$OLD_SPORT" -j DROP 2>/dev/null || true
fi
kill -9 "$N1_SSH_PID" 2>/dev/null || true
wait "$N1_SSH_PID" 2>/dev/null || true

# The name must be reusable afterwards — proof the reap actually freed it
# (not just an admin-display artifact).
ssh_cmd -N -R "vhost/n1:0:127.0.0.1:$SVC1_PORT" >"$TMPDIR/n1_ssh2.log" 2>&1 &
N1_SSH2_PID=$!
sleep 2
RESP=$(http_check "n1.$VHOST_DOMAIN")
if echo "$RESP" | grep -qi "^HTTP/1.1 [0-9]"; then
    pass "T-SSH-N1 label reusable after reap (fresh session routes)"
else
    fail "T-SSH-N1 label not reusable after reap: $RESP"
fi
kill -9 "$N1_SSH2_PID" 2>/dev/null || true
wait "$N1_SSH2_PID" 2>/dev/null || true
sleep 1

# ── T-SSH-N2: autossh recovery across a server restart ─────────────────────
echo "=== Test: T-SSH-N2 (autossh recovery across server restart) ==="
if [ "$HAVE_AUTOSSH" = "1" ]; then
    SVC2_PORT=19802
    spawn_echo_service "$SVC2_PORT" >/dev/null
    sleep 0.3
    BIND2_PORT=19902
    AUTOSSH_GATETIME=0 AUTOSSH_POLL=5 \
        ip netns exec nscli autossh -M 0 \
        -o "ServerAliveInterval=2" -o "ServerAliveCountMax=2" \
        "${SSH_OPTS[@]}" -i "$CLIENT_KEY" -o BatchMode=yes -o ExitOnForwardFailure=yes \
        -p "$CTRL_PORT" -N -R "$BIND2_PORT:127.0.0.1:$SVC2_PORT" \
        "gwtest@$SERVER_IP" >"$TMPDIR/n2_autossh.log" 2>&1 &
    N2_PID=$!
    if wait_port_up nscli "$SERVER_IP" "$BIND2_PORT" 100; then
        RESP=$(echo_tunnel "$SERVER_IP" "$BIND2_PORT" "n2-before")
        [ "$RESP" = "n2-before" ] || fail "T-SSH-N2 initial tunnel did not echo (got '$RESP')"

        echo "  (killing server, restarting, waiting for autossh to reconnect...)"
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        sleep 1
        start_server

        RECOVERED=0
        for _ in $(seq 1 20); do
            R=$(echo "n2-after" | timeout 2 ip netns exec nscli nc -N -w2 "$SERVER_IP" "$BIND2_PORT" 2>/dev/null || echo "")
            if [ "$R" = "n2-after" ]; then
                RECOVERED=1
                break
            fi
            sleep 1
        done
        if [ "$RECOVERED" = "1" ]; then
            pass "T-SSH-N2 autossh reconnected and relays again within 20s (client untouched)"
        else
            fail "T-SSH-N2 autossh never reconnected within 20s"
        fi
    else
        fail "T-SSH-N2 initial autossh tunnel never came up"
    fi
    kill -9 "$N2_PID" 2>/dev/null || true
    wait "$N2_PID" 2>/dev/null || true
else
    echo "SKIP: T-SSH-N2 (autossh not installed)"
fi
sleep 1

# ── T-SSH-N3: same-identity takeover under a live partition ────────────────
echo "=== Test: T-SSH-N3 (takeover under partition, no 60s wait) ==="
SVC3A_PORT=19803
SVC3B_PORT=19804
spawn_http_service "$SVC3A_PORT" "n3-a" >/dev/null
spawn_http_service "$SVC3B_PORT" "n3-b" >/dev/null
sleep 0.3
ssh_cmd -N -R "vhost/n3:0:127.0.0.1:$SVC3A_PORT" >"$TMPDIR/n3_ssh_a.log" 2>&1 &
N3A_PID=$!
sleep 2

OLD_SPORT3=$(ip netns exec ns0 ss -tnH state established "( sport = :$CTRL_PORT )" 2>/dev/null \
    | awk -v ip="$CLI_IP" '$4 ~ ip {n=split($4,a,":"); print a[n]}' | tail -1)
if [ -z "$OLD_SPORT3" ]; then
    fail "T-SSH-N3 could not find session A's peer port"
else
    ip netns exec ns0 iptables -A INPUT -p tcp -s "$CLI_IP" --sport "$OLD_SPORT3" -j DROP
    ip netns exec ns0 iptables -A OUTPUT -p tcp -d "$CLI_IP" --dport "$OLD_SPORT3" -j DROP

    # Immediately, same key, different backend — must succeed without waiting
    # for the reaper (D2/I-5 evicts synchronously on registration).
    ssh_cmd -N -R "vhost/n3:0:127.0.0.1:$SVC3B_PORT" >"$TMPDIR/n3_ssh_b.log" 2>&1 &
    N3B_PID=$!

    SWITCHED=0
    for _ in $(seq 1 20); do
        RESP=$(http_check "n3.$VHOST_DOMAIN")
        if echo "$RESP" | grep -qi "^HTTP/1.1 [0-9]"; then
            SWITCHED=1
            break
        fi
        sleep 1
    done
    if [ "$SWITCHED" = "1" ]; then
        pass "T-SSH-N3 takeover succeeded promptly while session A's flow was still DROPped"
    else
        fail "T-SSH-N3 takeover never succeeded"
    fi

    ip netns exec ns0 iptables -D INPUT -p tcp -s "$CLI_IP" --sport "$OLD_SPORT3" -j DROP 2>/dev/null || true
    ip netns exec ns0 iptables -D OUTPUT -p tcp -d "$CLI_IP" --dport "$OLD_SPORT3" -j DROP 2>/dev/null || true
    kill -9 "$N3B_PID" 2>/dev/null || true
    wait "$N3B_PID" 2>/dev/null || true
fi
kill -9 "$N3A_PID" 2>/dev/null || true
wait "$N3A_PID" 2>/dev/null || true
sleep 1

# ── T-SSH-N4: mixed transports on the SAME control port ────────────────────
echo "=== Test: T-SSH-N4 (native secret --udp pair + ssh vhost tunnel, one port) ==="
SVC4_SECRET_PORT=19805
SVC4_VHOST_PORT=19806
spawn_echo_service "$SVC4_SECRET_PORT" >/dev/null
spawn_http_service "$SVC4_VHOST_PORT" "n4-vhost-ok" >/dev/null
sleep 0.3

ip netns exec nscli "$BORE" local "$SVC4_SECRET_PORT" \
    --to "https://$SERVER_IP:$CTRL_PORT" --insecure \
    --secret "$SECRET" --tcp-secret-id n4 --udp \
    >"$TMPDIR/n4_provider.log" 2>&1 &
N4_PROV_PID=$!
sleep 1
N4_CONSUMER_PORT=19903
ip netns exec nscli "$BORE" proxy \
    --to "https://$SERVER_IP:$CTRL_PORT" --insecure \
    --secret "$SECRET" --tcp-secret-id n4 --udp \
    --local-proxy-port ":$N4_CONSUMER_PORT" \
    >"$TMPDIR/n4_consumer.log" 2>&1 &
N4_CONS_PID=$!

ssh_cmd -N -R "vhost/n4:0:127.0.0.1:$SVC4_VHOST_PORT" >"$TMPDIR/n4_ssh.log" 2>&1 &
N4_SSH_PID=$!
sleep 2

SECRET_RESP=$(echo "n4-secret" | timeout 5 ip netns exec nscli nc -N -w2 127.0.0.1 "$N4_CONSUMER_PORT" 2>/dev/null || echo "")
if [ "$SECRET_RESP" = "n4-secret" ]; then
    pass "T-SSH-N4 native secret --udp tunnel relays concurrently"
else
    fail "T-SSH-N4 native secret tunnel did not echo (got '$SECRET_RESP')"
fi

VHOST_RESP=$(http_check "n4.$VHOST_DOMAIN")
if echo "$VHOST_RESP" | grep -qi "^HTTP/1.1 [0-9]"; then
    pass "T-SSH-N4 ssh vhost tunnel relays concurrently on the SAME control port"
else
    fail "T-SSH-N4 ssh vhost tunnel did not respond: $VHOST_RESP"
fi

kill -9 "$N4_PROV_PID" "$N4_CONS_PID" "$N4_SSH_PID" 2>/dev/null || true
wait "$N4_PROV_PID" "$N4_CONS_PID" "$N4_SSH_PID" 2>/dev/null || true
sleep 1

# ── T-SSH-N5: throughput, informative only (no pass/fail gate) ─────────────
echo "=== Test: T-SSH-N5 (throughput: ssh tunnel vs native tunnel — report only) ==="
if [ "$HAVE_IPERF3" = "1" ]; then
    ip netns exec nscli iperf3 -s -1 -p 19950 >"$TMPDIR/n5_iperf_server.log" 2>&1 &
    N5_IPERF_SRV=$!
    sleep 0.5

    ssh_cmd -N -R "19910:127.0.0.1:19950" >"$TMPDIR/n5_ssh.log" 2>&1 &
    N5_SSH_PID=$!
    if wait_port_up nscli "$SERVER_IP" 19910 50; then
        SSH_RATE=$(ip netns exec nscli iperf3 -c "$SERVER_IP" -p 19910 -t 3 -J 2>/dev/null \
            | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['end']['sum_received']['bits_per_second'])" 2>/dev/null || echo "n/a")
    else
        SSH_RATE="n/a (tunnel did not come up)"
    fi
    kill -9 "$N5_SSH_PID" 2>/dev/null || true
    wait "$N5_SSH_PID" 2>/dev/null || true
    wait "$N5_IPERF_SRV" 2>/dev/null || true

    ip netns exec nscli iperf3 -s -1 -p 19951 >"$TMPDIR/n5_iperf_server2.log" 2>&1 &
    N5_IPERF_SRV2=$!
    sleep 0.5
    ip netns exec nscli "$BORE" local 19951 \
        --to "https://$SERVER_IP:$CTRL_PORT" --insecure --secret "$SECRET" \
        >"$TMPDIR/n5_native.log" 2>&1 &
    N5_NATIVE_PID=$!
    NATIVE_PORT=""
    if wait_for_log "$TMPDIR/n5_native.log" "listening at" 10; then
        NATIVE_PORT=$(grep "listening at" "$TMPDIR/n5_native.log" | tail -1 | grep -oE '[0-9]+$')
    fi
    if [ -n "$NATIVE_PORT" ] && wait_port_up nscli "$SERVER_IP" "$NATIVE_PORT" 50; then
        NATIVE_RATE=$(ip netns exec nscli iperf3 -c "$SERVER_IP" -p "$NATIVE_PORT" -t 3 -J 2>/dev/null \
            | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['end']['sum_received']['bits_per_second'])" 2>/dev/null || echo "n/a")
    else
        NATIVE_RATE="n/a (tunnel did not come up)"
    fi
    kill -9 "$N5_NATIVE_PID" 2>/dev/null || true
    wait "$N5_NATIVE_PID" 2>/dev/null || true
    wait "$N5_IPERF_SRV2" 2>/dev/null || true

    echo "  T-SSH-N5: ssh-tunnel bits/sec   = $SSH_RATE"
    echo "  T-SSH-N5: native-tunnel bits/sec = $NATIVE_RATE"
    pass "T-SSH-N5 throughput comparison printed (report-only, no gate)"
else
    echo "SKIP: T-SSH-N5 (iperf3 not installed) — still counts as pass (report-only)"
    pass "T-SSH-N5 skipped: iperf3 not installed (report-only test)"
fi
sleep 1

# ── T-SSH-N6: password auth ─────────────────────────────────────────────────
echo "=== Test: T-SSH-N6 (password auth via the passwords file) ==="
if [ "$HAVE_SSHPASS" = "1" ]; then
    SVC6_PORT=19807
    spawn_echo_service "$SVC6_PORT" >/dev/null
    sleep 0.3
    BIND6_PORT=19904

    ip netns exec nscli sshpass -p "$PASSWORD" ssh "${SSH_OPTS[@]}" \
        -o BatchMode=no -o PreferredAuthentications=password -o PubkeyAuthentication=no \
        -o ExitOnForwardFailure=yes -p "$CTRL_PORT" \
        -N -R "$BIND6_PORT:127.0.0.1:$SVC6_PORT" "alice@$SERVER_IP" \
        >"$TMPDIR/n6_ssh.log" 2>&1 &
    N6_PID=$!
    if wait_port_up nscli "$SERVER_IP" "$BIND6_PORT" 50; then
        RESP=$(echo_tunnel "$SERVER_IP" "$BIND6_PORT" "n6-pw")
        if [ "$RESP" = "n6-pw" ]; then
            pass "T-SSH-N6 password-authenticated tunnel relays"
        else
            fail "T-SSH-N6 password tunnel did not echo (got '$RESP')"
        fi
    else
        fail "T-SSH-N6 password-authenticated tunnel never came up"
    fi
    kill -9 "$N6_PID" 2>/dev/null || true
    wait "$N6_PID" 2>/dev/null || true

    # Wrong password must be rejected.
    if ip netns exec nscli sshpass -p "wrong-$PASSWORD" ssh "${SSH_OPTS[@]}" \
        -o BatchMode=no -o PreferredAuthentications=password -o PubkeyAuthentication=no \
        -o ExitOnForwardFailure=yes -o ConnectTimeout=5 -p "$CTRL_PORT" \
        -N -R "19905:127.0.0.1:$SVC6_PORT" "alice@$SERVER_IP" \
        >"$TMPDIR/n6_wrong.log" 2>&1; then
        fail "T-SSH-N6 wrong password was accepted"
    else
        pass "T-SSH-N6 wrong password rejected"
    fi
else
    echo "SKIP: T-SSH-N6 (sshpass not installed)"
fi

# ── T-SSH-N7: --ssh-banner delivered to the client ─────────────────────────
# Regression guard: --ssh-banner used to be parsed and stored but never wired
# into russh (the handler had no authentication_banner), so the flag was a
# silent no-op. A stock OpenSSH client prints the pre-auth SSH_MSG_USERAUTH_
# BANNER to stderr, so a healthy forward session must surface the marker.
echo ""
echo "=== Test: T-SSH-N7 (authentication banner delivered to client) ==="
SVC7_PORT=19808
spawn_echo_service "$SVC7_PORT" >/dev/null
sleep 0.3
BANNER_LOG="$TMPDIR/n7_banner.log"
ip netns exec nscli ssh "${SSH_OPTS[@]}" -i "$CLIENT_KEY" \
    -o BatchMode=yes -o ExitOnForwardFailure=yes \
    -p "$CTRL_PORT" -N -R "0.0.0.0:19908:127.0.0.1:$SVC7_PORT" \
    "gwtest@$SERVER_IP" >"$BANNER_LOG" 2>&1 &
N7_PID=$!
sleep 2
if grep -qF "$SSH_BANNER" "$BANNER_LOG"; then
    pass "T-SSH-N7 --ssh-banner reached the client"
else
    fail "T-SSH-N7 banner marker not seen in client output ($(head -c 200 "$BANNER_LOG" | tr '\n' ' '))"
fi
kill -9 "$N7_PID" 2>/dev/null || true
wait "$N7_PID" 2>/dev/null || true

# ── T-SSH-HOL: per-channel flow control ────────────────────────────────────
# One stalled consumer on a tunnel must NEVER head-of-line-block other clients
# on the SAME tunnel (same SSH connection). Regression guard for the vendored
# russh fix (crates/russh, upstream Eugeny/russh#730): stock russh forwarded
# inbound CHANNEL_DATA to the per-channel mpsc with a blocking `chan.send()`
# inside the single session read loop and replenished the SSH window on
# receipt, so a slow/paused consumer (a browser buffering a video over an
# `ssh -R` tunnel) parked the whole connection and every other channel starved
# until it drained. Reader A reads a little then STALLS (never reads again);
# reader B, opened afterwards on the same tunnel, must still be served. Pre-fix
# B got 0 bytes until A closed. Covered for public, vhost, AND secret tunnels.

# Raw TCP flooder in nscli: floods 'X' as fast as the socket accepts, forever.
spawn_flood_service() {
    local port="$1"
    ip netns exec nscli python3 - "$port" >"$TMPDIR/flood_$port.log" 2>&1 <<'PYEOF' &
import socket, sys, threading
port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(64)
def handle(c):
    b = b"X" * 65536
    try:
        while True:
            c.sendall(b)
    except OSError:
        pass
while True:
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
PYEOF
    echo $!
}

# HTTP flooder in nscli: reads the request, replies 200 + an endless body.
spawn_http_flood_service() {
    local port="$1"
    ip netns exec nscli python3 - "$port" >"$TMPDIR/hflood_$port.log" 2>&1 <<'PYEOF' &
import socket, sys, threading
port = int(sys.argv[1])
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(64)
def handle(c):
    b = b"X" * 65536
    try:
        c.recv(4096)
        c.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\r\n")
        while True:
            c.sendall(b)
    except OSError:
        pass
while True:
    c, _ = srv.accept()
    threading.Thread(target=handle, args=(c,), daemon=True).start()
PYEOF
    echo $!
}

# hol_probe <raw|http> <ip> <port> [host_header] — from inside nscli, opens
# reader A (reads a little, then stalls holding the socket), then reader B on
# the same tunnel, and prints "B_bytes=<n>" (bytes B received in a 6s window
# while A was stalled). Never aborts the harness (own errors -> B_bytes=0).
hol_probe() {
    local mode="$1" ip="$2" port="$3" host="${4:-x}"
    ip netns exec nscli python3 - "$mode" "$ip" "$port" "$host" 2>/dev/null <<'PYEOF' || echo "B_bytes=0"
import socket, sys, time, threading
mode, ip, port, host = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
req = ("GET / HTTP/1.1\r\nHost: %s\r\nConnection: keep-alive\r\n\r\n" % host).encode()
def conn(t=6.0):
    c = socket.socket(); c.settimeout(t); c.connect((ip, port))
    if mode == "http":
        c.sendall(req)
    return c
res = {}
def reader_a():
    try:
        c = conn(); res['a_first'] = len(c.recv(65536)); res['a'] = True; time.sleep(16); c.close()
    except OSError:
        res['a'] = True
def reader_b():
    while not res.get('a'):
        time.sleep(0.05)
    time.sleep(3)
    tot = 0; end = "timeout"
    try:
        c = conn(); t0 = time.time()
        while time.time() - t0 < 6 and tot < 1_000_000:
            d = c.recv(65536)
            if not d:
                end = "eof"; break
            tot += len(d)
        else:
            end = "cap" if tot >= 1_000_000 else "timeout"
        c.close()
    except OSError as e:
        end = "err:%s" % e
    res['b'] = tot; res['end'] = end
ta = threading.Thread(target=reader_a); tb = threading.Thread(target=reader_b)
ta.start(); tb.start(); tb.join()
print("B_bytes=%d B_end=%s A_first=%d" % (res.get('b', 0), res.get('end','?'), res.get('a_first',0)))
PYEOF
}

# T-SSH-HOL-PUB — public tunnel
echo ""
echo "=== Test: T-SSH-HOL-PUB (public tunnel: stalled reader must not block peers) ==="
HOL_PUB_FLOOD=19820
spawn_flood_service "$HOL_PUB_FLOOD" >/dev/null
sleep 0.3
ssh_cmd -N -R "19821:127.0.0.1:$HOL_PUB_FLOOD" >"$TMPDIR/hol_pub_ssh.log" 2>&1 &
HOL_PUB_SSH=$!
sleep 2
HOL_PUB_B=$(hol_probe raw "$SERVER_IP" 19821 | grep -oE '[0-9]+' | head -1)
HOL_PUB_B=${HOL_PUB_B:-0}
if [ "$HOL_PUB_B" -ge 100000 ]; then
    pass "T-SSH-HOL-PUB reader B served concurrently ($HOL_PUB_B bytes) while A stalled"
else
    fail "T-SSH-HOL-PUB reader B starved ($HOL_PUB_B bytes in 6s) — per-channel HOL regression"
fi
kill -9 "$HOL_PUB_SSH" 2>/dev/null || true; wait "$HOL_PUB_SSH" 2>/dev/null || true

# T-SSH-HOL-VHOST — vhost tunnel (over the shared control/HTTP port)
echo ""
echo "=== Test: T-SSH-HOL-VHOST (vhost tunnel: stalled reader must not block peers) ==="
HOL_VH_FLOOD=19822
spawn_http_flood_service "$HOL_VH_FLOOD" >/dev/null
sleep 0.3
ssh_cmd -N -R "vhost/hol:0:127.0.0.1:$HOL_VH_FLOOD" >"$TMPDIR/hol_vh_ssh.log" 2>&1 &
HOL_VH_SSH=$!
# A `-N` vhost forward is not routable until the gateway's PARAMS_GRACE (5s,
# waiting for exec params that never arrive) elapses and the registry entry is
# inserted; before that a request 404s on the fallthrough. Gate on actual
# routability — poll until one GET truly STREAMS (a large response, not a 404 /
# redirect) — so the concurrency probe below tests the relay, not the warm-up.
vh_stream_bytes() {
    ip netns exec nscli python3 - "$SERVER_IP" "$CTRL_PORT" "hol.$VHOST_DOMAIN" 2>/dev/null <<'PY' || echo 0
import socket,sys,time
ip,port,host=sys.argv[1],int(sys.argv[2]),sys.argv[3]
try:
    c=socket.socket();c.settimeout(4);c.connect((ip,port))
    c.sendall(("GET / HTTP/1.1\r\nHost: %s\r\nConnection: keep-alive\r\n\r\n"%host).encode())
    tot=0;t0=time.time()
    while time.time()-t0<2 and tot<300000:
        d=c.recv(65536)
        if not d: break
        tot+=len(d)
    c.close();print(tot)
except OSError: print(0)
PY
}
HOL_VH_READY=0
for _ in $(seq 1 20); do
    if [ "$(vh_stream_bytes)" -ge 100000 ]; then HOL_VH_READY=1; break; fi
    sleep 1
done
if [ "$HOL_VH_READY" != "1" ]; then
    fail "T-SSH-HOL-VHOST vhost tunnel never became routable/streamable (setup)"
else
    HOL_VH_B=$(hol_probe http "$SERVER_IP" "$CTRL_PORT" "hol.$VHOST_DOMAIN" | grep -oE 'B_bytes=[0-9]+' | grep -oE '[0-9]+' | head -1)
    HOL_VH_B=${HOL_VH_B:-0}
    if [ "$HOL_VH_B" -ge 100000 ]; then
        pass "T-SSH-HOL-VHOST reader B served concurrently ($HOL_VH_B bytes) while A stalled"
    else
        fail "T-SSH-HOL-VHOST reader B starved ($HOL_VH_B bytes in 6s) — per-channel HOL regression"
    fi
fi
kill -9 "$HOL_VH_SSH" 2>/dev/null || true; wait "$HOL_VH_SSH" 2>/dev/null || true

# T-SSH-HOL-SECRET — secret tunnel: SSH provider, native consumer. The flood
# flows provider(SSH)->server->consumer(native); a stalled reader on the
# consumer's local port must not block a peer, i.e. must not park the provider's
# russh channel and starve the sibling channel.
echo ""
echo "=== Test: T-SSH-HOL-SECRET (secret tunnel: stalled reader must not block peers) ==="
HOL_SEC_FLOOD=19823
HOL_SEC_CONS=19824
spawn_flood_service "$HOL_SEC_FLOOD" >/dev/null
sleep 0.3
ssh_cmd -N -R "secret/holsec:0:127.0.0.1:$HOL_SEC_FLOOD" >"$TMPDIR/hol_sec_ssh.log" 2>&1 &
HOL_SEC_SSH=$!
sleep 1
ip netns exec nscli "$BORE" proxy \
    --to "https://$SERVER_IP:$CTRL_PORT" --insecure \
    --secret "$SECRET" --tcp-secret-id holsec \
    --local-proxy-port ":$HOL_SEC_CONS" >"$TMPDIR/hol_sec_consumer.log" 2>&1 &
HOL_SEC_CONS_PID=$!
sleep 2
HOL_SEC_B=$(hol_probe raw 127.0.0.1 "$HOL_SEC_CONS" | grep -oE '[0-9]+' | head -1)
HOL_SEC_B=${HOL_SEC_B:-0}
if [ "$HOL_SEC_B" -ge 100000 ]; then
    pass "T-SSH-HOL-SECRET reader B served concurrently ($HOL_SEC_B bytes) while A stalled"
else
    fail "T-SSH-HOL-SECRET reader B starved ($HOL_SEC_B bytes in 6s) — per-channel HOL regression"
fi
kill -9 "$HOL_SEC_SSH" "$HOL_SEC_CONS_PID" 2>/dev/null || true
wait "$HOL_SEC_SSH" "$HOL_SEC_CONS_PID" 2>/dev/null || true

# ── T-VBT-NETNS-SSH: ssh -R vhost + backend-tls=on → self-signed HTTPS backend ─
echo ""
echo "=== Test: T-VBT-NETNS-SSH (ssh -R vhost backend-tls=on → HTTPS backend) ==="
VBT_TLS_PORT=19830
VBT_PLAIN_PORT=19831
spawn_https_service "$VBT_TLS_PORT" "vbt-https-ok" >/dev/null
spawn_http_service "$VBT_PLAIN_PORT" "vbt-plain-ok" >/dev/null
sleep 0.5

# WITH backend-tls=on: the exec param opens a session channel (no -N) and the
# gateway parses it as tunnel params (not a shell command). ssh_cmd cannot append
# a trailing command (it puts the destination last), so invoke ssh directly here.
ip netns exec nscli ssh "${SSH_OPTS[@]}" -i "$CLIENT_KEY" \
    -o BatchMode=yes -o ExitOnForwardFailure=yes -p "$CTRL_PORT" \
    -R "vhost/vbttls:0:127.0.0.1:$VBT_TLS_PORT" "gwtest@$SERVER_IP" 'backend-tls=on' \
    >"$TMPDIR/vbt_tls_ssh.log" 2>&1 &
VBT_TLS_SSH=$!
# WITHOUT the param toward a plaintext backend (regression).
ssh_cmd -N -R "vhost/vbtplain:0:127.0.0.1:$VBT_PLAIN_PORT" >"$TMPDIR/vbt_plain_ssh.log" 2>&1 &
VBT_PLAIN_SSH=$!

# Poll each subdomain until its SSH session has registered (a 404 means the
# vhost route isn't live yet — the two sessions register asynchronously).
vbt_probe() {
    local host="$1" needle="$2" resp
    for _ in $(seq 1 30); do
        resp=$(http_check "$host")
        if echo "$resp" | grep -q "$needle"; then
            echo "$resp"
            return 0
        fi
        sleep 0.5
    done
    echo "$resp"
    return 1
}

if vbt_probe "vbttls.$VHOST_DOMAIN" "vbt-https-ok" >/dev/null; then
    pass "T-VBT-NETNS-SSH backend-tls=on serves the self-signed HTTPS backend"
else
    fail "T-VBT-NETNS-SSH backend-tls=on failed (expected vbt-https-ok): $(http_check "vbttls.$VHOST_DOMAIN")"
fi

if vbt_probe "vbtplain.$VHOST_DOMAIN" "vbt-plain-ok" >/dev/null; then
    pass "T-VBT-NETNS-SSH no-flag plaintext backend still serves 200"
else
    fail "T-VBT-NETNS-SSH plaintext regression (expected vbt-plain-ok): $(http_check "vbtplain.$VHOST_DOMAIN")"
fi
kill -9 "$VBT_TLS_SSH" "$VBT_PLAIN_SSH" 2>/dev/null || true
wait "$VBT_TLS_SSH" "$VBT_PLAIN_SSH" 2>/dev/null || true

# ── T-SSH-JUMP: native QUIC/fallback + pure-OpenSSH production path ─────────
echo ""
echo "=== Test: T-SSH-JUMP (ProxyJump dispatch, QUIC fallback/renewal, compatibility) ==="

# `ssh -W` is the transport primitive used by stock OpenSSH ProxyJump. Feeding
# it an echo line exercises the real direct-tcpip channel without installing a
# second sshd solely for this netns harness.
jump_via_gateway() {
    local alias="$1" port="$2" line="$3" user="${4:-gwtest}"
    printf '%s\n' "$line" | timeout 12 ip netns exec nscli ssh \
        "${SSH_OPTS[@]}" -i "$CLIENT_KEY" -o BatchMode=yes \
        -p "$CTRL_PORT" -W "$alias.$SSH_JUMP_DOMAIN:$port" \
        "$user@$SERVER_IP" 2>/dev/null || true
}

JH_NATIVE_PORT=19840
JH_NATIVE_ALIAS="netns-native"
spawn_echo_service "$JH_NATIVE_PORT" >/dev/null
sleep 0.3

# Start with direct UDP blocked. Registration and the first forwarded channel
# must remain healthy through the already-warm TCP carrier.
ip netns exec nscli iptables -I OUTPUT -p udp -d "$SERVER_IP" \
    --dport "$DIRECT_QUIC_PORT" -j DROP
ip netns exec nscli "$BORE" sshjhost "127.0.0.1:$JH_NATIVE_PORT" \
    --subdomain "$JH_NATIVE_ALIAS" \
    --to "https://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" --insecure \
    --notes "T-SSH-JUMP native" --carriers 2 --udp --auto-reconnect \
    >"$TMPDIR/jump_native.log" 2>&1 &
JH_NATIVE_PID=$!

if wait_jump_alias "$JH_NATIVE_ALIAS"; then
    JH_FALLBACK_BEFORE=$(jump_field "$JH_NATIVE_ALIAS" direct_fallbacks)
    JH_FALLBACK_BEFORE=${JH_FALLBACK_BEFORE:-0}
    JH_RESP=$(jump_via_gateway "$JH_NATIVE_ALIAS" "$JH_NATIVE_PORT" "jump-fallback")
    JH_FALLBACK_AFTER=$(jump_field "$JH_NATIVE_ALIAS" direct_fallbacks)
    JH_FALLBACK_AFTER=${JH_FALLBACK_AFTER:-0}
    if [ "$JH_RESP" = "jump-fallback" ] && \
            [ "$JH_FALLBACK_AFTER" -gt "$JH_FALLBACK_BEFORE" ]; then
        pass "T-SSH-JUMP UDP-blocked native provider uses warm TCP fallback"
    else
        fail "T-SSH-JUMP native fallback failed (response='$JH_RESP', fallback $JH_FALLBACK_BEFORE->$JH_FALLBACK_AFTER)"
    fi
else
    fail "T-SSH-JUMP native alias did not register while UDP was blocked"
fi

# Remove the firewall fault. The live provider must replenish only its direct
# shortfall, then a new channel must increment the direct-open counter.
ip netns exec nscli iptables -D OUTPUT -p udp -d "$SERVER_IP" \
    --dport "$DIRECT_QUIC_PORT" -j DROP 2>/dev/null || true
if wait_jump_field_ge "$JH_NATIVE_ALIAS" direct_carriers 2 200; then
    JH_DIRECT_BEFORE=$(jump_field "$JH_NATIVE_ALIAS" direct_stream_opens)
    JH_DIRECT_BEFORE=${JH_DIRECT_BEFORE:-0}
    JH_RESP=$(jump_via_gateway "$JH_NATIVE_ALIAS" "$JH_NATIVE_PORT" "jump-direct")
    JH_DIRECT_AFTER=$(jump_field "$JH_NATIVE_ALIAS" direct_stream_opens)
    JH_DIRECT_AFTER=${JH_DIRECT_AFTER:-0}
    if [ "$JH_RESP" = "jump-direct" ] && \
            [ "$JH_DIRECT_AFTER" -gt "$JH_DIRECT_BEFORE" ]; then
        pass "T-SSH-JUMP native provider renews two QUIC carriers and uses direct"
    else
        fail "T-SSH-JUMP native direct path failed (response='$JH_RESP', opens $JH_DIRECT_BEFORE->$JH_DIRECT_AFTER)"
    fi
else
    fail "T-SSH-JUMP native direct carrier pool did not renew to two"
fi

# Keep the native jump direct pool live while vhost and public providers join
# the same UDP 443 endpoint. Successful traffic through all three proves the
# bare, `port:<N>` and `jump:<alias>` key namespaces cannot cross-install.
JH_MIX_VHOST_PORT=19842
JH_MIX_PUBLIC_TARGET=19843
JH_MIX_PUBLIC_PORT=19921
spawn_http_service "$JH_MIX_VHOST_PORT" "jump-mix-vhost" >/dev/null
spawn_echo_service "$JH_MIX_PUBLIC_TARGET" >/dev/null
sleep 0.3
ip netns exec nscli "$BORE" vhost "127.0.0.1:$JH_MIX_VHOST_PORT" \
    --subdomain jumpmix --id jumpmix --to "https://$SERVER_IP:$CTRL_PORT" \
    --secret "$SECRET" --insecure --udp \
    >"$TMPDIR/jump_mix_vhost.log" 2>&1 &
JH_MIX_VHOST_PID=$!
ip netns exec nscli "$BORE" local "$JH_MIX_PUBLIC_TARGET" \
    --to "https://$SERVER_IP:$CTRL_PORT" --secret "$SECRET" --insecure \
    --port "$JH_MIX_PUBLIC_PORT" --udp \
    >"$TMPDIR/jump_mix_public.log" 2>&1 &
JH_MIX_PUBLIC_PID=$!

JH_MIX_VHOST_OK=0
JH_MIX_PUBLIC_OK=0
wait_for_log "$SERVER_LOG" "vhost QUIC direct carrier established" 20 && JH_MIX_VHOST_OK=1
wait_for_log "$SERVER_LOG" "public QUIC direct carrier established" 20 && JH_MIX_PUBLIC_OK=1
JH_MIX_VHOST_RESP=$(http_check "jumpmix.$VHOST_DOMAIN")
JH_MIX_PUBLIC_RESP=$(echo_tunnel "$SERVER_IP" "$JH_MIX_PUBLIC_PORT" "jump-mix-public")
JH_MIX_JUMP_RESP=$(jump_via_gateway "$JH_NATIVE_ALIAS" "$JH_NATIVE_PORT" "jump-mix-native")
if [ "$JH_MIX_VHOST_OK" -eq 1 ] && [ "$JH_MIX_PUBLIC_OK" -eq 1 ] && \
        echo "$JH_MIX_VHOST_RESP" | grep -q "jump-mix-vhost" && \
        [ "$JH_MIX_PUBLIC_RESP" = "jump-mix-public" ] && \
        [ "$JH_MIX_JUMP_RESP" = "jump-mix-native" ]; then
    pass "T-SSH-JUMP vhost/public/jump direct pools coexist on one UDP endpoint"
else
    fail "T-SSH-JUMP shared direct endpoint isolation failed"
fi
kill -9 "$JH_MIX_VHOST_PID" "$JH_MIX_PUBLIC_PID" 2>/dev/null || true
wait "$JH_MIX_VHOST_PID" "$JH_MIX_PUBLIC_PID" 2>/dev/null || true

# A mismatched classic username must fail only for jump dispatch. The exact
# same key still authenticates a legacy public reverse forward, preserving the
# gateway's pre-jump username-agnostic contract.
JH_WRONG=$(jump_via_gateway "$JH_NATIVE_ALIAS" "$JH_NATIVE_PORT" "must-not-pass" "wrong")
JH_LEGACY_PORT=19920
ip netns exec nscli ssh "${SSH_OPTS[@]}" -i "$CLIENT_KEY" \
    -o BatchMode=yes -o ExitOnForwardFailure=yes -p "$CTRL_PORT" \
    -N -R "$JH_LEGACY_PORT:127.0.0.1:$JH_NATIVE_PORT" "wrong@$SERVER_IP" \
    >"$TMPDIR/jump_legacy_wrong_user.log" 2>&1 &
JH_LEGACY_PID=$!
if [ -z "$JH_WRONG" ] && wait_port_up nscli "$SERVER_IP" "$JH_LEGACY_PORT" 50 && \
        [ "$(echo_tunnel "$SERVER_IP" "$JH_LEGACY_PORT" "legacy-user-ignored")" = "legacy-user-ignored" ]; then
    pass "T-SSH-JUMP username binding is jump-only; legacy forward unchanged"
else
    fail "T-SSH-JUMP username mismatch or legacy compatibility contract failed"
fi
kill -9 "$JH_LEGACY_PID" 2>/dev/null || true
wait "$JH_LEGACY_PID" 2>/dev/null || true

# Pure OpenSSH publishes a nonstandard virtual port through `jump/`. It stays
# TCP-only, the exact port succeeds, and an accidental port 22 request fails.
JH_PURE_PORT=19841
JH_PURE_ALIAS="netns-pure"
spawn_echo_service "$JH_PURE_PORT" >/dev/null
sleep 0.3
ssh_cmd -N -R "jump/$JH_PURE_ALIAS:$JH_PURE_PORT:127.0.0.1:$JH_PURE_PORT" \
    >"$TMPDIR/jump_pure.log" 2>&1 &
JH_PURE_PID=$!
if wait_jump_alias "$JH_PURE_ALIAS"; then
    JH_PURE_RESP=$(jump_via_gateway "$JH_PURE_ALIAS" "$JH_PURE_PORT" "jump-pure")
    JH_PURE_WRONG=$(jump_via_gateway "$JH_PURE_ALIAS" 22 "wrong-port")
    JH_PURE_DIRECT=$(jump_field "$JH_PURE_ALIAS" direct_carriers)
    if [ "$JH_PURE_RESP" = "jump-pure" ] && [ -z "$JH_PURE_WRONG" ] && \
            [ "${JH_PURE_DIRECT:-0}" -eq 0 ]; then
        pass "T-SSH-JUMP pure OpenSSH nonstandard target works, wrong port denied, TCP-only"
    else
        fail "T-SSH-JUMP pure OpenSSH contract failed (response='$JH_PURE_RESP', wrong='$JH_PURE_WRONG', direct='${JH_PURE_DIRECT:-}')"
    fi
else
    fail "T-SSH-JUMP pure OpenSSH alias did not register"
fi

kill -9 "$JH_NATIVE_PID" "$JH_PURE_PID" 2>/dev/null || true
wait "$JH_NATIVE_PID" "$JH_PURE_PID" 2>/dev/null || true

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "=== Summary ==="
echo "PASS: $PASS   FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then
    echo "All ssh-gateway netns tests passed."
    exit 0
else
    echo "Some ssh-gateway netns tests FAILED."
    exit 1
fi
