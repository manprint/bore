#!/usr/bin/env bash
# outage_netns_test.sh — T-OUT-*: a network outage or an ISP IP change must
# never cost minutes of downtime, in ANY mode (plan 005, O-1).
#
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup:
#   sudo -n /abs/path/scripts/outage_netns_test.sh
#
# Topology (names prefixed `bout` so they never collide with another harness;
# never run two netns harnesses concurrently anyway):
#
#   bout_s (server 10.77.0.1) ── bout_r (router) ── bout_c (client site 10.78.0.2/.3/.4)
#                                       └────────── bout_v (visitor 10.79.0.2, never affected)
#
# Every event happens in bout_r with iptables FORWARD rules:
#   blackhole  DROP everything from/to 10.78.0.0/24
#   IP change  DROP the current client address forever, both directions, and
#              move bout_c's default route to the next address: the sockets
#              that exist keep the old source and are dead for good, new ones
#              use the new one — exactly an ISP IP change.
#
# Reference scenarios (docs/plans/005_plan-OutageRecovery/overview.md):
#   T-OUT-FLICK      4 s blackhole: no mode reconnects; a rate-limited download
#                    spanning the flick completes byte-exact
#   T-OUT-OUTAGE     20 s blackhole: every mode serves again within 15 s of restoration
#   T-OUT-IPCHANGE0  instant address change: every mode serves again within 25 s
#   T-OUT-IPCHANGE   20 s blackhole then an address change: every mode serves
#                    again within 15 s of restoration; fixed and sticky public
#                    ports come back
#
# Shipped defaults only: no BORE_* timing override is set anywhere below.
#
# Options (sudo's env_reset drops environment variables, so every knob is also
# an option; the variable of the same meaning is honoured when it survives):
#   --bore PATH          BORE=PATH      binary under test (red-check: point it
#                                       at a baseline build; the staleness guard
#                                       is skipped when it is given)
#   --events "LIST"      OUT_EVENTS     subset/order of FLICK OUTAGE IPCHANGE0 IPCHANGE
#   --keep-logs          OUT_KEEP_LOGS=1  keep the logs on success too
#   --window N           OUT_WINDOW=15  recovery window after restoration (s)
#   --window0 N          OUT_WINDOW0=25 recovery window for IPCHANGE0 (s)

set -euo pipefail
# The timings below are $EPOCHREALTIME arithmetic in awk: under a
# comma-decimal locale bash prints "1727.5" as "1727,5" and awk reads 1727
# (V-11's trap). Pin the C locale for the whole run.
export LC_ALL=C

# Shipped defaults only: a BORE_* variable inherited from the caller would
# silently change a timeout or the server the clients dial.
for v in $(compgen -e | grep '^BORE_' || true); do unset "$v"; done

while [ $# -gt 0 ]; do
    case "$1" in
        --bore)      BORE="$2"; shift 2 ;;
        --events)    OUT_EVENTS="$2"; shift 2 ;;
        --keep-logs) OUT_KEEP_LOGS=1; shift ;;
        --window)    OUT_WINDOW="$2"; shift 2 ;;
        --window0)   OUT_WINDOW0="$2"; shift 2 ;;
        *) echo "ERROR: unknown option $1" >&2; exit 2 ;;
    esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -n "${BORE:-}" ]; then
    BORE_OVERRIDE=1
else
    BORE_OVERRIDE=0
    BORE="$ROOT/target/release/bore"
fi

if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi
if [ "$BORE_OVERRIDE" -eq 0 ] && find "$ROOT/src" "$ROOT/Cargo.toml" \
        -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi
if ! "$BORE" server --help 2>&1 | grep -q -- '--ssh-gateway' \
        || ! "$BORE" server --help 2>&1 | grep -q -- '--vpn'; then
    echo "ERROR: $BORE was not built with the vpn and ssh-gateway features." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn,ssh-gateway" >&2
    exit 1
fi

for cmd in ip iptables curl python3 ssh ssh-keygen sha256sum ping timeout awk; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

OUT_EVENTS="${OUT_EVENTS:-FLICK OUTAGE IPCHANGE0 IPCHANGE}"
OUT_WINDOW="${OUT_WINDOW:-15}"
OUT_WINDOW0="${OUT_WINDOW0:-25}"

SECRET="outtest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
S_IP="10.77.0.1"
C_NET="10.78.0.0/24"
C_GW="10.78.0.254"
V_IP="10.79.0.2"
CTRL="7835"
VHOST_HTTP="8080"
QUIC_PORT="7836"
VHOST_DOMAIN="out.test"
JUMP_DOMAIN="jump.out.test"
T="/tmp/bore_outage_$$"
NSS="bout_s bout_r bout_c bout_v"

# The client site's address now, and the ones it moves to on each IP change.
C_ADDRS=(10.78.0.2 10.78.0.3 10.78.0.4)
C_IDX=0

PASS=0
FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; exit 1; }

cleanup() {
    set +e
    for ns in $NSS; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
    done
    sleep 0.3
    for ns in $NSS; do
        ip netns del "$ns" 2>/dev/null
    done
    if [ "${FAIL:-0}" -eq 0 ] && [ "${OUT_KEEP_LOGS:-0}" != 1 ]; then
        rm -rf "$T" 2>/dev/null
    else
        echo "Logs kept at $T for inspection." >&2
    fi
}
trap cleanup EXIT INT TERM

in_ns() { local ns="$1"; shift; ip netns exec "$ns" "$@"; }
now() { echo "$EPOCHREALTIME"; }
since() { awk -v a="$1" -v b="$EPOCHREALTIME" 'BEGIN { printf "%.1f", b - a }'; }
count() { local n; n=$(grep -c -- "$2" "$1" 2>/dev/null) || true; echo "${n:-0}"; }

# ── Topology ─────────────────────────────────────────────────────────────────
echo "=== Setup: topology ==="
for ns in $NSS; do ip netns del "$ns" 2>/dev/null || true; done
mkdir -p "$T"
for ns in $NSS; do
    ip netns add "$ns"
    in_ns "$ns" ip link set lo up
done
link_pair() { # ns-a dev-a ns-b dev-b
    ip link add "$2" type veth peer name "$4"
    ip link set "$2" netns "$1"
    ip link set "$4" netns "$3"
    in_ns "$1" ip link set "$2" up
    in_ns "$3" ip link set "$4" up
}
link_pair bout_s bo-s bout_r bo-rs
link_pair bout_c bo-c bout_r bo-rc
link_pair bout_v bo-v bout_r bo-rv
in_ns bout_r ip addr add 10.77.0.254/24 dev bo-rs
in_ns bout_r ip addr add "$C_GW/24" dev bo-rc
in_ns bout_r ip addr add 10.79.0.254/24 dev bo-rv
in_ns bout_r sysctl -qw net.ipv4.ip_forward=1
in_ns bout_s ip addr add "$S_IP/24" dev bo-s
in_ns bout_s ip route add default via 10.77.0.254
for a in "${C_ADDRS[@]}"; do in_ns bout_c ip addr add "$a/24" dev bo-c; done
in_ns bout_c ip route add default via "$C_GW" src "${C_ADDRS[0]}"
in_ns bout_v ip addr add "$V_IP/24" dev bo-v
in_ns bout_v ip route add default via 10.79.0.254
in_ns bout_c ping -c1 -W2 "$S_IP" >/dev/null || die "bout_c cannot reach the server"
in_ns bout_v ping -c1 -W2 "$S_IP" >/dev/null || die "bout_v cannot reach the server"

# ── Services ─────────────────────────────────────────────────────────────────
echo "=== Setup: services, keys ==="
mkdir -p "$T/www" "$T/vwww" "$T/rx" "$T/tx" "$T/keys"
echo "out-ok" >"$T/www/probe.txt"
head -c $((2 * 1024 * 1024)) /dev/urandom >"$T/www/big.bin"
BIG_SHA=$(sha256sum "$T/www/big.bin" | awk '{print $1}')
echo "visitor-ok" >"$T/vwww/probe.txt"
in_ns bout_c python3 -m http.server 8000 --bind 127.0.0.1 --directory "$T/www" \
    >"$T/http_c.log" 2>&1 &
in_ns bout_v python3 -m http.server 8100 --bind 127.0.0.1 --directory "$T/vwww" \
    >"$T/http_v.log" 2>&1 &
# Line echo for the ssh-jump provider (stands in for an sshd).
in_ns bout_c python3 - >"$T/echo.log" 2>&1 <<'PYEOF' &
import socket, threading
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 2222))
srv.listen(64)
def serve(c):
    with c:
        while True:
            data = c.recv(65536)
            if not data:
                return
            c.sendall(data)
while True:
    conn, _ = srv.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
PYEOF
CLIENT_KEY="$T/client_key"
ssh-keygen -t ed25519 -N '' -f "$CLIENT_KEY" -C gwtest >/dev/null 2>&1 || die "ssh-keygen failed"
cp "$CLIENT_KEY.pub" "$T/keys/gwtest"
chmod 600 "$CLIENT_KEY"
SSH_OPTS=(-i "$CLIENT_KEY" -o BatchMode=yes -o StrictHostKeyChecking=no
    -o UserKnownHostsFile=/dev/null -o GlobalKnownHostsFile=/dev/null
    -o ConnectTimeout=5 -o LogLevel=ERROR -p "$CTRL")

# ── Server ───────────────────────────────────────────────────────────────────
echo "=== Setup: server ==="
in_ns bout_s "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" --control-port "$CTRL" \
    --min-port 40000 --max-port 40999 \
    --udp --vhost-quic-port "$QUIC_PORT" \
    --vhost-base-domain "$VHOST_DOMAIN" --vhost-http-port "$VHOST_HTTP" \
    --vpn --vpn-pool 10.99.0.0/24 \
    --ssh-gateway --ssh-jump-base-domain "$JUMP_DOMAIN" \
    --ssh-host-key-file "$T/ssh_host_key.pem" \
    --ssh-authorized-keys-dir "$T/keys" \
    --web-transfer-base-url "http://127.0.0.1:$CTRL/" \
    >"$T/server.log" 2>&1 &
for _ in $(seq 1 50); do
    in_ns bout_v bash -c "exec 3<>/dev/tcp/$S_IP/$CTRL" 2>/dev/null && break
    sleep 0.1
done

# ── Tunnels: every mode, all --auto-reconnect, shipped defaults ──────────────
echo "=== Setup: tunnels ==="
TO="$S_IP:$CTRL"
declare -A PIDS
start() { # name ns args...
    local name="$1" ns="$2"; shift 2
    in_ns "$ns" "$BORE" "$@" >"$T/$name.log" 2>&1 </dev/null &
    PIDS[$name]=$!
}
start pub_rand  bout_c local 8000 -l 127.0.0.1 --to "$TO" --secret "$SECRET" --auto-reconnect
start pub_fixed bout_c local 8000 -l 127.0.0.1 --to "$TO" --secret "$SECRET" --port 40100 --auto-reconnect
start pub_udp   bout_c local 8000 -l 127.0.0.1 --to "$TO" --secret "$SECRET" --port 40101 \
    --udp --carriers 2 --auto-reconnect
start vh_udp    bout_c vhost 127.0.0.1:8000 --subdomain a --id out-a --to "$TO" --secret "$SECRET" \
    --udp --carriers 4 --auto-reconnect
start vh_plain  bout_c vhost 127.0.0.1:8000 --subdomain b --id out-b --to "$TO" --secret "$SECRET" --auto-reconnect
start sec_prov  bout_c local 8000 -l 127.0.0.1 --to "$TO" --secret "$SECRET" \
    --tcp-secret-id sp1 --udp --auto-reconnect
start sec_cons  bout_v proxy --local-proxy-port :9001 --to "$TO" --secret "$SECRET" \
    --tcp-secret-id sp1 --udp --auto-reconnect
start sec_prov2 bout_v local 8100 -l 127.0.0.1 --to "$TO" --secret "$SECRET" \
    --tcp-secret-id sp2 --auto-reconnect
start sec_cons2 bout_c proxy --local-proxy-port :9002 --to "$TO" --secret "$SECRET" \
    --tcp-secret-id sp2 --auto-reconnect
start ssh_jump  bout_c sshjhost 127.0.0.1:2222 --subdomain jh --to "$TO" --secret "$SECRET" \
    --auto-reconnect
start xfer      bout_c transfer listener --dest-path "$T/rx" --to "$TO" --secret "$SECRET" \
    --transfer-id tx1 --persistent --rename --relay-only
start owner     bout_c transfer web --to "$TO" --secret "$SECRET"
start vpn_r_l   bout_c vpn listen  --to "$S_IP" --secret "$SECRET" --id vr --tun-name vr0 \
    --relay-only --auto-reconnect
start vpn_r_c   bout_v vpn connect --to "$S_IP" --secret "$SECRET" --id vr --tun-name vr0 \
    --relay-only --auto-reconnect
start vpn_d_l   bout_c vpn listen  --to "$S_IP" --secret "$SECRET" --id vd --tun-name vd0 \
    --auto-reconnect
start vpn_d_c   bout_v vpn connect --to "$S_IP" --secret "$SECRET" --id vd --tun-name vd0 \
    --auto-reconnect
# Stock OpenSSH with the documented client options, and a supervisor loop in
# place of autossh (`AUTOSSH_GATETIME=0` semantics: always restart).
in_ns bout_c bash -c "
    while :; do
        echo ssh-start >>'$T/ssh_vhost.runs'
        ssh ${SSH_OPTS[*]} -N -o ServerAliveInterval=2 -o ServerAliveCountMax=7 \
            -o ExitOnForwardFailure=yes -R 'vhost/sshv:0:127.0.0.1:8000' gwtest@$S_IP
        sleep 1
    done" >"$T/ssh_vhost.log" 2>&1 </dev/null &

# ── Probes: 0 = the mode serves end to end right now ─────────────────────────
curl_ok() { # ns url expected [extra curl args...]
    local ns="$1" url="$2" want="$3"; shift 3
    [ "$(in_ns "$ns" curl -s -m 3 "$@" "$url" 2>/dev/null)" = "$want" ]
}
vhost_ok() { curl_ok bout_v "http://$S_IP:$VHOST_HTTP/probe.txt" out-ok -H "Host: $1.$VHOST_DOMAIN"; }
pub_port_now() { { grep -o "listening at $S_IP:[0-9]*" "$T/pub_rand.log" 2>/dev/null || true; } | tail -1 | awk -F: '{print $NF}'; }
probe_pub_rand()  { local p; p=$(pub_port_now); [ -n "$p" ] && curl_ok bout_v "http://$S_IP:$p/probe.txt" out-ok; }
probe_pub_fixed() { curl_ok bout_v "http://$S_IP:40100/probe.txt" out-ok; }
probe_pub_udp()   { curl_ok bout_v "http://$S_IP:40101/probe.txt" out-ok; }
probe_vh_udp()    { vhost_ok a; }
probe_vh_plain()  { vhost_ok b; }
probe_ssh_vhost() { vhost_ok sshv; }
probe_sec_prov()  { curl_ok bout_v "http://127.0.0.1:9001/probe.txt" out-ok; }
probe_sec_cons()  { curl_ok bout_c "http://127.0.0.1:9002/probe.txt" visitor-ok; }
probe_ssh_jump() {
    local line="jump-$RANDOM"
    [ "$(printf '%s\n' "$line" | timeout 6 ip netns exec bout_v ssh "${SSH_OPTS[@]}" \
        -W "jh.$JUMP_DOMAIN:2222" "gwtest@$S_IP" 2>/dev/null | head -1)" = "$line" ]
}
probe_xfer() {
    local name="p$RANDOM$RANDOM"
    echo "$name" >"$T/tx/$name"
    timeout 6 ip netns exec bout_v "$BORE" transfer sender --sources "$T/tx/$name" \
        --to "$TO" --secret "$SECRET" --transfer-id tx1 --relay-only \
        >>"$T/xfer_sender.log" 2>&1 </dev/null || return 1
    [ "$(cat "$T/rx/$name" 2>/dev/null)" = "$name" ]
}
# The owner keeps its room: it announced a loss for THIS event (its control
# connection cannot survive one — OWNER_BASE is the loss count before it), every
# loss has been followed by a resume, and the room never expired.
owner_losses()  { count "$T/owner.log" ", resuming"; }
owner_resumes() { count "$T/owner.log" "owner control resumed"; }
OWNER_BASE=-1
probe_owner() {
    grep -q "room: " "$T/owner.log" 2>/dev/null || return 1
    grep -q "room expired" "$T/owner.log" 2>/dev/null && return 1
    local losses
    losses=$(owner_losses)
    [ "$losses" -gt "$OWNER_BASE" ] && [ "$losses" -eq "$(owner_resumes)" ]
}
vpn_ok() { # tun
    local dst
    dst=$(in_ns bout_c ip -4 -o addr show dev "$1" 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)
    [ -n "$dst" ] && in_ns bout_v ping -c1 -W1 -I "$1" "$dst" >/dev/null 2>&1
}
probe_vpn_relay()  { vpn_ok vr0; }
probe_vpn_direct() { vpn_ok vd0; }

MODES=(pub_rand pub_fixed pub_udp vh_udp vh_plain ssh_vhost sec_prov sec_cons
    ssh_jump xfer owner vpn_relay vpn_direct)

# Sessions each client has established: a FLICK must not add one.
sessions() { # mode
    case "$1" in
        pub_rand|pub_fixed|pub_udp|vh_udp|vh_plain|ssh_jump) count "$T/$1.log" "reconnect: connected" ;;
        sec_prov) echo "$(( $(count "$T/sec_prov.log" "reconnect: connected") + $(count "$T/sec_cons.log" "reconnect: connected") ))" ;;
        sec_cons) echo "$(( $(count "$T/sec_prov2.log" "reconnect: connected") + $(count "$T/sec_cons2.log" "reconnect: connected") ))" ;;
        ssh_vhost) count "$T/ssh_vhost.runs" "ssh-start" ;;
        xfer) count "$T/xfer.log" "registered secret tunnel" ;;
        owner) owner_losses ;;
        vpn_relay) echo "$(( $(count "$T/vpn_r_l.log" "reconnecting") + $(count "$T/vpn_r_c.log" "reconnecting") ))" ;;
        vpn_direct) echo "$(( $(count "$T/vpn_d_l.log" "reconnecting") + $(count "$T/vpn_d_c.log" "reconnecting") ))" ;;
    esac
}

# ── Baseline ─────────────────────────────────────────────────────────────────
echo "=== Baseline: every mode serves before any event ==="
for m in "${MODES[@]}"; do
    ok=0
    for _ in $(seq 1 60); do
        if "probe_$m"; then ok=1; break; fi
        sleep 0.5
    done
    if [ "$ok" -eq 1 ]; then pass "baseline: $m serves"; else fail "baseline: $m never served"; fi
done
# The direct VPN arm must really be on the direct path, or it would only be a
# second relay arm.
for _ in $(seq 1 60); do
    grep -q "bridge switched to direct path" "$T/vpn_d_l.log" 2>/dev/null && break
    sleep 0.5
done
if grep -q "bridge switched to direct path" "$T/vpn_d_l.log" 2>/dev/null; then
    pass "baseline: vpn_direct reached the direct path"
else
    fail "baseline: vpn_direct never reached the direct path"
fi
if [ "$FAIL" -ne 0 ]; then
    echo "=== Results: PASS=$PASS FAIL=$FAIL (baseline failed; events not run) ==="
    exit 1
fi

# ── Events ───────────────────────────────────────────────────────────────────
blackhole_on()  { in_ns bout_r iptables -I FORWARD -s "$C_NET" -j DROP; in_ns bout_r iptables -I FORWARD -d "$C_NET" -j DROP; }
blackhole_off() { in_ns bout_r iptables -D FORWARD -s "$C_NET" -j DROP; in_ns bout_r iptables -D FORWARD -d "$C_NET" -j DROP; }
change_ip() {
    local old="${C_ADDRS[$C_IDX]}"
    C_IDX=$((C_IDX + 1))
    local new="${C_ADDRS[$C_IDX]}"
    in_ns bout_r iptables -I FORWARD -s "$old" -j DROP
    in_ns bout_r iptables -I FORWARD -d "$old" -j DROP
    in_ns bout_c ip route replace default via "$C_GW" src "$new"
    echo "  client site address $old -> $new (old address blackholed for good)"
}

# Polls every mode in parallel from t0 until t0 + window + a 10 s stability
# tail. A mode's recovery time is the FIRST SUCCESS AFTER ITS LAST FAILURE: a
# path that serves for a moment (QUIC migrates to the new address at once) and
# then drops while its control connection is replaced is not "recovered" until
# it serves for good. Every failed probe is traced for diagnosis.
measure() { # event window t0
    local ev="$1" win="$2" t0="$3" m pids=()
    local end
    end=$(awk -v a="$t0" -v w="$win" 'BEGIN { printf "%.3f", a + w + 10 }')
    for m in "${MODES[@]}"; do
        (
            settled=never
            while awk -v e="$end" -v n="$EPOCHREALTIME" 'BEGIN { exit !(n < e) }'; do
                if "probe_$m"; then
                    [ "$settled" = never ] && settled=$(since "$t0")
                else
                    echo "$(since "$t0") fail" >>"$T/trace_${ev}_$m"
                    settled=never
                fi
                sleep 0.5
            done
            echo "$settled" >"$T/rec_${ev}_$m"
        ) &
        pids+=("$!")
    done
    wait "${pids[@]}" || true
    for m in "${MODES[@]}"; do
        local took
        took=$(cat "$T/rec_${ev}_$m" 2>/dev/null || echo never)
        if [ "$took" != never ] && awk -v t="$took" -v w="$win" 'BEGIN { exit !(t <= w) }'; then
            pass "T-OUT-$ev: $m serves again for good after ${took}s (window ${win}s)"
        else
            fail "T-OUT-$ev: $m did not serve for good within ${win}s (settled: $took; failures: $(tr '\n' ' ' <"$T/trace_${ev}_$m" 2>/dev/null | cut -c1-200))"
        fi
    done
}

# A `--port 0` tunnel asks for the port it held. Checked only after events in
# which the server has reaped the dead registration before the client returns
# (OUTAGE, IPCHANGE: 20 s > the 15 s reap): after IPCHANGE0 both deadlines are
# 15 s and a reconnect that wins the race is correctly given another port.
sticky_check() { # event port-before
    local after
    after=$(pub_port_now)
    if [ -n "$2" ] && [ "$after" = "$2" ]; then
        pass "T-OUT-$1: the random public port came back ($2)"
    else
        fail "T-OUT-$1: the random public port changed ($2 -> $after)"
    fi
}

event_flick() {
    echo "=== T-OUT-FLICK: 4 s blackhole, same address ==="
    local m before=()
    for m in "${MODES[@]}"; do before+=("$(sessions "$m")"); done
    in_ns bout_v curl -s -m 60 --limit-rate 200k -H "Host: a.$VHOST_DOMAIN" \
        -o "$T/flick.bin" "http://$S_IP:$VHOST_HTTP/big.bin" >"$T/flick_curl.log" 2>&1 &
    local dl=$!
    sleep 2
    blackhole_on; sleep 4; blackhole_off
    if wait "$dl" && [ "$(sha256sum "$T/flick.bin" | awk '{print $1}')" = "$BIG_SHA" ]; then
        pass "T-OUT-FLICK: a download spanning the flick completed byte-exact"
    else
        fail "T-OUT-FLICK: the download spanning the flick did not complete intact"
    fi
    # Give every deadline a full window to fire if it was going to.
    sleep 12
    local i=0
    for m in "${MODES[@]}"; do
        local after
        after=$(sessions "$m")
        if [ "$after" = "${before[$i]}" ] && "probe_$m"; then
            pass "T-OUT-FLICK: $m kept its session (sessions ${before[$i]} -> $after) and serves"
        else
            fail "T-OUT-FLICK: $m reconnected or stopped serving (sessions ${before[$i]} -> $after)"
        fi
        i=$((i + 1))
    done
}

event_outage() {
    echo "=== T-OUT-OUTAGE: 20 s blackhole, same address ==="
    local port_before
    port_before=$(pub_port_now)
    OWNER_BASE=$(owner_losses)
    blackhole_on; sleep 20; blackhole_off
    measure OUTAGE "$OUT_WINDOW" "$(now)"
    sticky_check OUTAGE "$port_before"
}

event_ipchange0() {
    echo "=== T-OUT-IPCHANGE0: instant address change, no outage ==="
    local t0
    OWNER_BASE=$(owner_losses)
    t0=$(now)
    change_ip
    measure IPCHANGE0 "$OUT_WINDOW0" "$t0"
}

event_ipchange() {
    echo "=== T-OUT-IPCHANGE: 20 s blackhole, then an address change ==="
    local port_before
    port_before=$(pub_port_now)
    OWNER_BASE=$(owner_losses)
    blackhole_on; sleep 20
    change_ip
    blackhole_off
    measure IPCHANGE "$OUT_WINDOW" "$(now)"
    sticky_check IPCHANGE "$port_before"
}

for ev in $OUT_EVENTS; do
    case "$ev" in
        FLICK) event_flick ;;
        OUTAGE) event_outage ;;
        IPCHANGE0) event_ipchange0 ;;
        IPCHANGE) event_ipchange ;;
        *) die "unknown event $ev" ;;
    esac
done

# Every client must still be the process it started as: none crashed out, and
# none gave up — an auto-reconnecting client that exits has failed this test
# even if a probe happened to pass before it did.
for f in "${!PIDS[@]}"; do
    if ! kill -0 "${PIDS[$f]}" 2>/dev/null; then
        fail "$f is no longer running: $(grep -iE 'panicked|Error' "$T/$f.log" | tail -1)"
    elif grep -qiE "panicked" "$T/$f.log" 2>/dev/null; then
        fail "$f panicked: $(grep -i 'panicked' "$T/$f.log" | head -1)"
    fi
done

echo ""
echo "=== Results: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ]
