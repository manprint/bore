#!/usr/bin/env bash
# SSH ingress gateway netns harness — Phase 7.2 chaos/acceptance tests.
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

SERVER_IP="10.230.0.2"   # server-side of ns0↔nscli veth
CLI_IP="10.230.0.1"      # nscli-side

CTRL_PORT="7835"
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
code_of() { echo "$1" | tail -1; }
body_of() { echo "$1" | sed '$d'; }

admin_data() { body_of "$(admin_curl /admin/status/data)"; }

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
        --ssh-gateway \
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
cp "$CLIENT_KEY.pub" "$TMPDIR/keys/authorized_keys"

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
