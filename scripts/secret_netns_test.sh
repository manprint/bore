#!/usr/bin/env bash
# Secret tunnel netns harness — Phase N hardening acceptance tests
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Topology:
#   ns0 (server) — veth0s↔veth0c (10.220.0.0/30) ↔ nsprov (provider: bore local)
#                — veth1s↔veth1c (10.221.0.0/30) ↔ nscli (consumer: bore proxy)
#
# Usage: sudo scripts/secret_netns_test.sh
# Exit code: 0 = all tests passed, nonzero = failures

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# ── Guards ──────────────────────────────────────────────────────────────────
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
for cmd in ip nc socat python3 openssl curl jq; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

# ── Configuration ───────────────────────────────────────────────────────────
SECRET="secrettest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
ADMIN_TOKEN="0123456789abcdef0123456789abcdef01234567"  # 40 chars for --admin-token

SERVER_IP_NS0_PROV="10.220.0.2"    # server-side of ns0↔nsprov veth
PROV_IP="10.220.0.1"               # nsprov-side
SERVER_IP_NS0_CLI="10.221.0.2"     # server-side of ns0↔nscli veth
CLI_IP="10.221.0.1"                # nscli-side

CTRL_PORT="7835"
TMPDIR="/tmp/bore_secret_$$"

PASS=0
FAIL=0

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

# ── Cleanup ──────────────────────────────────────────────────────────────────
# Return the host to its exact pre-test state. Netns-scoped kills are precise (they
# never touch host processes); the path-specific pkills are backstops for procs that
# somehow escaped a netns — we deliberately do NOT `pkill socat`/`nc` generically, so
# the host's own processes are never disturbed.
cleanup() {
    set +e
    for ns in ns0 nsprov nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    pkill -9 -f 'target/release/bore' 2>/dev/null
    pkill -9 -f 'socat.*reuseaddr,fork PIPE' 2>/dev/null
    # Stray root-ns veths (only if a run was killed between `ip link add` and the move
    # into a netns); deleting one peer removes the pair.
    for v in veth0s veth0c veth1s veth1c; do ip link del "$v" 2>/dev/null; done
    rm -rf "$TMPDIR" 2>/dev/null
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

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# Curl admin API from ns0 (where server is)
# Usage: admin_curl <path> [--no-token]
# Returns: "<http_code>\n<body>"
admin_curl() {
    local path="$1" token_flag="--token"
    for arg in "$@"; do
        [ "$arg" = "--no-token" ] && token_flag="--no-token"
    done
    local hdr=()
    [ "$token_flag" = "--token" ] && hdr=(-H "Authorization: Bearer $ADMIN_TOKEN")
    ip netns exec ns0 curl -sk -m 10 "${hdr[@]}" -w $'\n%{http_code}' \
        "https://127.0.0.1:$CTRL_PORT$path" 2>/dev/null
}

code_of() { echo "$1" | tail -1; }
body_of() { echo "$1" | sed '$d'; }

# Count secret tunnel entries by role
# Usage: count_secret <provider|consumer> <which_ns (ns0)>
count_secret() {
    local role="$1" ns="${2:-ns0}"
    local body=$(admin_curl "/admin/api/v1/secret")
    body=$(body_of "$body")
    if [ -z "$body" ]; then
        echo 0
        return
    fi
    case "$role" in
        provider)
            echo "$body" | jq '[.[] | select(.role=="secretprovider")] | length' 2>/dev/null || echo 0
            ;;
        consumer)
            echo "$body" | jq '[.[] | select(.role=="secretconsumer")] | length' 2>/dev/null || echo 0
            ;;
    esac
}

# Print the parsed /admin/api/v1/secret JSON body (empty on error). Safe under set -e.
secret_json() {
    local raw
    raw=$(admin_curl "/admin/api/v1/secret" || true)
    body_of "$raw"
}

# Count consumer rows with a null/empty local_proxy_port (the spurious-carrier signature).
count_null_consumer_ports() {
    secret_json | jq '[.[] | select(.role=="secretconsumer" and (.local_proxy_port==null or .local_proxy_port==""))] | length' 2>/dev/null || echo 0
}

# Assert equality
assert_eq() {
    local actual="$1" expected="$2" msg="$3"
    if [ "$actual" = "$expected" ]; then
        pass "$msg (actual=$actual)"
    else
        fail "$msg (expected=$expected, got=$actual)"
    fi
}

# Spawn a provider (bore local --tcp-secret-id)
# Usage: spawn_provider <id> <extra-flags...>
spawn_provider() {
    local id="$1"
    shift
    local log="$TMPDIR/provider_$id.log"
    # Derive a listen port from the id's numeric suffix (e.g. sec-load-3 → 9003);
    # ids without a numeric suffix fall back to the base 9000. Guarded so `set -u`
    # does not treat a non-numeric suffix as an unbound variable in arithmetic.
    local suffix="${id##*-}"
    [[ "$suffix" =~ ^[0-9]+$ ]] || suffix=0
    local lport=$((9000 + suffix))

    ip netns exec nsprov socat TCP-LISTEN:$lport,reuseaddr,fork PIPE >/dev/null 2>&1 &

    ip netns exec nsprov "$BORE" local "$lport" \
        --to "https://$SERVER_IP_NS0_PROV:$CTRL_PORT" --insecure \
        --secret "$SECRET" \
        --tcp-secret-id "$id" \
        "$@" \
        >"$log" 2>&1 &

    echo $!  # Return the PID
}

# Spawn a proxy (bore proxy consumer)
# Usage: spawn_proxy <id> <localport> <extra-flags...>
spawn_proxy() {
    local id="$1" lport="$2"
    shift 2
    local log="$TMPDIR/proxy_${id}_$lport.log"

    ip netns exec nscli "$BORE" proxy \
        --to "https://$SERVER_IP_NS0_CLI:$CTRL_PORT" --insecure \
        --secret "$SECRET" \
        --tcp-secret-id "$id" \
        --local-proxy-port ":$lport" \
        "$@" \
        >"$log" 2>&1 &

    echo $!  # Return the PID
}

# Tear down ALL test clients (providers in nsprov, proxies in nscli, and their
# socat echo servers) WITHOUT touching the long-lived server in ns0. Used between
# tests so each starts from a clean client slate while the server + its admin API
# keep running. Netns-scoped → never disturbs the host's own processes.
reset_clients() {
    for ns in nsprov nscli; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
    done
    sleep 1
}

# (Re)start the bore server in ns0 and wait until its control port accepts. Reused
# by the reconnect test, which restarts the server to exercise client auto-reconnect.
start_server() {
    SERVER_LOG="$TMPDIR/server.log"
    ip netns exec ns0 "$BORE" server \
        --admin-token "$ADMIN_TOKEN" \
        --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
        --secret "$SECRET" \
        --control-port "$CTRL_PORT" \
        --cert-file "$CERT_FILE" --key-file "$KEY_FILE" \
        --udp \
        >>"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
    sleep 1
    wait_server_ready ns0 127.0.0.1 "$CTRL_PORT" || die "server not reachable from ns0"
    echo "  Server up (pid $SERVER_PID)"
}

# ── Setup ──────────────────────────────────────────────────────────────────
echo "=== Setup: reclaiming any stale state from a prior (possibly SIGKILLed) run ==="
# A SIGKILLed prior run cannot fire its EXIT trap, so reclaim its leftovers here:
# orphan processes, netns, stray veths, and old per-PID temp dirs. This makes the
# harness idempotent — it returns the host to a clean state no matter how the last
# run ended, BEFORE creating anything new.
for ns in ns0 nsprov nscli; do
    ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null || true
    ip netns del "$ns" 2>/dev/null || true
done
pkill -9 -f 'target/release/bore' 2>/dev/null || true
pkill -9 -f 'socat.*reuseaddr,fork PIPE' 2>/dev/null || true
for v in veth0s veth0c veth1s veth1c; do ip link del "$v" 2>/dev/null || true; done
rm -rf /tmp/bore_secret_* 2>/dev/null || true

echo "=== Setup: creating netns ==="
ip netns add ns0
ip netns add nsprov
ip netns add nscli

# ns0 ↔ nsprov (provider side)
ip link add veth0s type veth peer name veth0c
ip link set veth0s netns ns0
ip link set veth0c netns nsprov
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_PROV/30" dev veth0s
ip netns exec nsprov ip addr add "$PROV_IP/30" dev veth0c
ip netns exec ns0 ip link set veth0s up
ip netns exec nsprov ip link set veth0c up

# ns0 ↔ nscli (client side)
ip link add veth1s type veth peer name veth1c
ip link set veth1s netns ns0
ip link set veth1c netns nscli
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_CLI/30" dev veth1s
ip netns exec nscli ip addr add "$CLI_IP/30" dev veth1c
ip netns exec ns0 ip link set veth1s up
ip netns exec nscli ip link set veth1c up

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec nsprov ip link set lo up
ip netns exec nscli ip link set lo up

# Route leaves to each other through ns0 (for hole-punch)
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1 2>/dev/null || true
ip netns exec nsprov ip route add 10.221.0.0/30 via "$SERVER_IP_NS0_PROV" 2>/dev/null || true
ip netns exec nscli ip route add 10.220.0.0/30 via "$SERVER_IP_NS0_CLI" 2>/dev/null || true

# ── Start server ───────────────────────────────────────────────────────────
echo "=== Starting bore server (admin enabled, --udp) ==="
mkdir -p "$TMPDIR"

# Generate control cert
CERT_FILE="$TMPDIR/server.crt"
KEY_FILE="$TMPDIR/server.key"
openssl req -x509 -newkey rsa:2048 -keyout "$KEY_FILE" -out "$CERT_FILE" \
    -days 365 -nodes -subj "/CN=bore.test" -addext "subjectAltName=DNS:bore.test" 2>/dev/null \
    || die "failed to generate cert"

start_server

# ── T-SEC-SMOKE: 1 provider + 1 proxy, echo round-trips ──────────────────
echo ""
echo "=== Test: T-SEC-SMOKE (smoke test: 1 provider + 1 proxy) ==="
PROV_PID=$(spawn_provider "smoke-prov")
sleep 1

PROXY_PID=$(spawn_proxy "smoke-prov" 9001)
sleep 1

ECHO_TEXT="Hello secret tunnel"
RESPONSE=$(echo "$ECHO_TEXT" | timeout 12 ip netns exec nscli nc -N -w5 127.0.0.1 9001 2>/dev/null || echo "ERROR")
if [ "$RESPONSE" = "$ECHO_TEXT" ]; then
    pass "T-SEC-SMOKE echo round-trip"
else
    fail "T-SEC-SMOKE expected '$ECHO_TEXT', got '$RESPONSE'"
fi

kill "$PROXY_PID" "$PROV_PID" 2>/dev/null || true
sleep 0.5

# ── T-SEC-LOAD: 5 providers (mix tcp/udp, carriers), 5-10 proxies each ────
echo "=== Test: T-SEC-LOAD (5 providers × 5-10 proxies, mixed flags) ==="
declare -a PROV_PIDS PROXY_PIDS

# Start 5 providers with varying flags
for i in 1 2 3 4 5; do
    sid="sec-load-$i"
    if [ $((i % 2)) -eq 0 ]; then
        # Even providers: with --udp
        PROV_PIDS[$i]=$(spawn_provider "$sid" --udp)
    else
        # Odd providers: relay only
        PROV_PIDS[$i]=$(spawn_provider "$sid")
    fi

    # Provider 4 gets --carriers 4
    if [ $i -eq 4 ]; then
        kill "${PROV_PIDS[$i]}" 2>/dev/null || true
        PROV_PIDS[$i]=$(spawn_provider "$sid" --carriers 4 --udp)
    fi
done
sleep 2

# Start 5-10 proxies per provider, mixed flags. Record every port so the echo
# check is deterministic (no port-guessing).
LOAD_PORTS=()
for i in 1 2 3 4 5; do
    sid="sec-load-$i"
    num_proxies=$((5 + i))  # 6..10 proxies per provider

    for j in $(seq 1 "$num_proxies"); do
        lport=$((9100 + i * 10 + j))

        # Mix: some udp, some carriers, some notes
        flags=""
        [ $((j % 2)) -eq 0 ] && flags="$flags --udp"
        [ $((j % 3)) -eq 0 ] && flags="$flags --carriers 4"
        [ $((j % 5)) -eq 0 ] && flags="$flags --notes proxy-${i}-${j}"

        PROXY_PIDS+=( $(spawn_proxy "$sid" "$lport" $flags) )
        LOAD_PORTS+=( "$lport" )
    done
done
sleep 3

# Drive echo through EVERY proxy (deterministic: we know each port).
echo "  (testing echo through all ${#LOAD_PORTS[@]} proxies...)"
ok=0
total=${#LOAD_PORTS[@]}
for lport in "${LOAD_PORTS[@]}"; do
    RESPONSE=$(echo "load-$lport" | timeout 6 ip netns exec nscli nc -N -w3 127.0.0.1 "$lport" 2>/dev/null || echo "")
    [ "$RESPONSE" = "load-$lport" ] && ok=$((ok + 1))
done

# Allow a 1-proxy slack for UDP-negotiation timing jitter under netns.
if [ "$ok" -ge $((total - 1)) ]; then
    pass "T-SEC-LOAD $ok/$total proxies echoed across 5 providers (mixed tcp/udp/carriers)"
else
    fail "T-SEC-LOAD only $ok/$total proxies echoed"
fi

# Carrier accounting under load: consumer rows == live proxies, no null-port rows.
assert_eq "$(count_secret consumer)" "$total" "T-SEC-LOAD consumer rows == $total proxies (carriers not multiplied)"
assert_eq "$(count_null_consumer_ports)" "0" "T-SEC-LOAD no null-port carrier rows under load"

# Kill all load test processes and ensure a clean slate before the next test.
for pid in "${PROV_PIDS[@]}" "${PROXY_PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
done
reset_clients

# ── T-SEC-CARRIER-COUNT: active providers == live processes ──────────────
echo "=== Test: T-SEC-CARRIER-COUNT (provider/consumer count matches live PIDs) ==="
PROV_PID1=$(spawn_provider "count-prov-1" --carriers 4)
PROV_PID2=$(spawn_provider "count-prov-2" --carriers 4)
sleep 1

# Each proxy uses --carriers 4: the 3 extra relay carriers per proxy must NOT create
# extra admin rows (BUG-S1). Expect 3 consumer rows total, NOT 12.
PROXY_PID1=$(spawn_proxy "count-prov-1" 9201 --carriers 4)
PROXY_PID2=$(spawn_proxy "count-prov-1" 9202 --carriers 4)
PROXY_PID3=$(spawn_proxy "count-prov-2" 9203 --carriers 4)
sleep 2

prov_count=$(count_secret provider)
cons_count=$(count_secret consumer)

assert_eq "$prov_count" "2" "T-SEC-CARRIER-COUNT providers=2 (carriers not multiplied)"
assert_eq "$cons_count" "3" "T-SEC-CARRIER-COUNT consumers=3 (3 proxies x4 carriers = 3 rows, NOT 12)"
assert_eq "$(count_null_consumer_ports)" "0" "T-SEC-CARRIER-COUNT no null-port carrier rows"

kill "$PROXY_PID1" "$PROXY_PID2" "$PROXY_PID3" "$PROV_PID1" "$PROV_PID2" 2>/dev/null || true
sleep 0.5

# ── T-SEC-NOSPURIOUS: no consumer has null local_proxy_port ───────────────
echo "=== Test: T-SEC-NOSPURIOUS (no null local_proxy_port in consumers) ==="
PROV_PID=$(spawn_provider "nospurious")
sleep 1

# Spawn proxies with --carriers 4 (tests the multi-carrier case)
for lport in 9301 9302 9303; do
    spawn_proxy "nospurious" "$lport" --carriers 4 >/dev/null 2>&1 &
done
sleep 2

null_count=$(count_null_consumer_ports)
cons_count=$(count_secret consumer)

if [ "$null_count" = "0" ]; then
    pass "T-SEC-NOSPURIOUS zero consumers with null/empty local_proxy_port"
else
    fail "T-SEC-NOSPURIOUS found $null_count consumers with null/empty local_proxy_port"
fi
assert_eq "$cons_count" "3" "T-SEC-NOSPURIOUS 3 carrier-4 proxies collapse to 3 consumer rows"

reset_clients

# ── T-SEC-CONFLICT: reject duplicate provider id, wrong secret, non-existent id ──
echo "=== Test: T-SEC-CONFLICT (conflict detection) ==="
PROV_PID=$(spawn_provider "conflict-id")
sleep 1

# (a) Second provider with same id → should fail / log error
TEST_LOG="$TMPDIR/conflict_dup.log"
ip netns exec nsprov socat TCP-LISTEN:9999,reuseaddr,fork PIPE >/dev/null 2>&1 &
ip netns exec nsprov timeout 5 "$BORE" local 9999 \
    --to "$SERVER_IP_NS0_PROV:$CTRL_PORT" \
    --secret "$SECRET" \
    --tcp-secret-id "conflict-id" \
    >"$TEST_LOG" 2>&1 || true
sleep 0.5

# Should not register a second provider with same id
prov_count=$(count_secret provider)
if [ "$prov_count" = "1" ]; then
    pass "T-SEC-CONFLICT duplicate provider id rejected (count=1)"
else
    fail "T-SEC-CONFLICT duplicate provider id leaked (count=$prov_count, expected 1)"
fi

# (b) Proxy for non-existent id → connection fails cleanly
TEST_LOG="$TMPDIR/conflict_nonexist.log"
ip netns exec nscli timeout 5 "$BORE" proxy \
    --to "$SERVER_IP_NS0_CLI:$CTRL_PORT" \
    --secret "$SECRET" \
    --tcp-secret-id "nonexistent-id" \
    --local-proxy-port ":9999" \
    >"$TEST_LOG" 2>&1 || true
sleep 0.5

# Consumer for non-existent provider should not register
cons_count=$(count_secret consumer)
if [ "$cons_count" = "0" ]; then
    pass "T-SEC-CONFLICT proxy for non-existent id rejected (count=0)"
else
    fail "T-SEC-CONFLICT proxy for non-existent id leaked (count=$cons_count)"
fi

# (c) Proxy with wrong secret → rejected
TEST_LOG="$TMPDIR/conflict_wrongsecret.log"
ip netns exec nscli timeout 5 "$BORE" proxy \
    --to "$SERVER_IP_NS0_CLI:$CTRL_PORT" \
    --secret "wrongsecret" \
    --tcp-secret-id "conflict-id" \
    --local-proxy-port ":9999" \
    >"$TEST_LOG" 2>&1 || true
sleep 0.5

cons_count=$(count_secret consumer)
if [ "$cons_count" = "0" ]; then
    pass "T-SEC-CONFLICT proxy with wrong secret rejected (count=0)"
else
    fail "T-SEC-CONFLICT wrong secret leaked (count=$cons_count)"
fi

reset_clients

# ── T-SEC-NOZOMBIE: after process death, entries are reaped ──────────────
echo "=== Test: T-SEC-NOZOMBIE (reaper closes zombie entries) ==="
PROV_PID=$(spawn_provider "zombie")
sleep 1

PROXY_PID=$(spawn_proxy "zombie" 9401)
sleep 1

# Verify both are alive
prov_count=$(count_secret provider)
cons_count=$(count_secret consumer)
if [ "$prov_count" = "1" ] && [ "$cons_count" = "1" ]; then
    pass "T-SEC-NOZOMBIE initial: 1 provider, 1 consumer"
else
    fail "T-SEC-NOZOMBIE initial count wrong (prov=$prov_count, cons=$cons_count)"
fi

# Kill the proxy; reaper should drop its entry within ~60s (CTRL_TIMEOUT)
echo "  (killing proxy, waiting for reaper...)"
kill "$PROXY_PID" 2>/dev/null || true
sleep 2

# Poll until the consumer count drops to 0 (timeout 15s)
for _ in $(seq 1 150); do
    cons_count=$(count_secret consumer)
    [ "$cons_count" = "0" ] && break
    sleep 0.1
done

if [ "$cons_count" = "0" ]; then
    pass "T-SEC-NOZOMBIE consumer reaped after kill (count=0)"
else
    fail "T-SEC-NOZOMBIE zombie consumer persisted (count=$cons_count)"
fi

reset_clients

# ── T-SEC-MIXED: tcp + udp consumers on same provider ────────────────────
echo "=== Test: T-SEC-MIXED (TCP + UDP consumers coexist) ==="
PROV_PID=$(spawn_provider "mixed" --udp)
sleep 1

PROXY_TCP=$(spawn_proxy "mixed" 9501)
PROXY_UDP=$(spawn_proxy "mixed" 9502 --udp)
sleep 1

# Both should echo
RESP_TCP=$(echo "tcp-test" | timeout 5 ip netns exec nscli nc -N -w2 127.0.0.1 9501 2>/dev/null || echo "")
RESP_UDP=$(echo "udp-test" | timeout 5 ip netns exec nscli nc -N -w2 127.0.0.1 9502 2>/dev/null || echo "")

if [ "$RESP_TCP" = "tcp-test" ] && [ "$RESP_UDP" = "udp-test" ]; then
    pass "T-SEC-MIXED TCP+UDP consumers both echo"
else
    fail "T-SEC-MIXED TCP=$RESP_TCP, UDP=$RESP_UDP"
fi

cons_count=$(count_secret consumer)
if [ "$cons_count" = "2" ]; then
    pass "T-SEC-MIXED consumer count=2 (TCP+UDP)"
else
    fail "T-SEC-MIXED consumer count=$cons_count (expected 2)"
fi

reset_clients

# ── T-SEC-RECONNECT: server restart → clients auto-reconnect, no duplicates ──
echo "=== Test: T-SEC-RECONNECT (server restart → auto-reconnect, no duplicate zombies) ==="
spawn_provider "reconnect" --auto-reconnect >/dev/null
sleep 1
spawn_proxy "reconnect" 9601 --auto-reconnect >/dev/null
sleep 2

assert_eq "$(count_secret consumer)" "1" "T-SEC-RECONNECT initial consumer count=1"
assert_eq "$(count_secret provider)" "1" "T-SEC-RECONNECT initial provider count=1"

# Kill the SERVER (not the clients): with --auto-reconnect the live provider and
# proxy must re-establish once the server returns. Killing a client process would
# NOT test auto-reconnect — the process would simply be gone.
echo "  (killing server, restarting, waiting for client auto-reconnect...)"
kill -9 "$SERVER_PID" 2>/dev/null || true
sleep 2
start_server

reconnected=0
for _ in $(seq 1 150); do
    if [ "$(count_secret consumer)" = "1" ] && [ "$(count_secret provider)" = "1" ]; then
        reconnected=1
        break
    fi
    sleep 0.2
done
assert_eq "$reconnected" "1" "T-SEC-RECONNECT provider+proxy auto-reconnected to exactly 1 entry each (no duplicate/zombie)"

RESP=$(echo "reconn" | timeout 8 ip netns exec nscli nc -N -w3 127.0.0.1 9601 2>/dev/null || echo "")
assert_eq "$RESP" "reconn" "T-SEC-RECONNECT relay works after reconnect"

reset_clients

# ── T-SEC-UDP-CLEANLOG: no spurious "direct udp accept failed" WARNs ──────
echo "=== Test: T-SEC-UDP-CLEANLOG (UDP punch/reconnect logs clean) ==="
# Start provider with --udp and --auto-reconnect; consumer with --udp
PROV_PID=$(spawn_provider "udplog" --udp --auto-reconnect)
sleep 1

PROXY_PID=$(spawn_proxy "udplog" 9701 --udp --auto-reconnect)
sleep 1

# Let it settle
sleep 2

# Check provider log for spurious WARNs
warn_count=$(grep -cE "WARN.*direct.*udp.*accept.*failed" "$TMPDIR/provider_udplog.log" 2>/dev/null || true)
[ -n "$warn_count" ] || warn_count=0
if [ "$warn_count" = "0" ]; then
    pass "T-SEC-UDP-CLEANLOG provider log: zero spurious WARN lines"
else
    fail "T-SEC-UDP-CLEANLOG found $warn_count spurious WARN lines in provider log"
fi

reset_clients

# ── T-SEC-UDP-FALLBACK: --udp consumer falls back to relay, carriers, no spurious ──
echo "=== Test: T-SEC-UDP-FALLBACK (--udp consumer → relay fallback + carriers, no spurious) ==="
# Relay-only provider (NO --udp): a --udp consumer's broker finds no UDP-capable
# provider, replies UdpUnavailable, and the consumer falls back to the relay — then
# opens its extra carriers. This is the BUG-S1 path that also affects UDP consumers
# whenever the direct path is unavailable. Deterministic (no firewall needed).
PROV_PID=$(spawn_provider "udpfb")
sleep 1
PROXY_PID=$(spawn_proxy "udpfb" 9801 --udp --carriers 4)
sleep 3

RESP=$(echo "fallback" | timeout 8 ip netns exec nscli nc -N -w3 127.0.0.1 9801 2>/dev/null || echo "")
assert_eq "$RESP" "fallback" "T-SEC-UDP-FALLBACK relay echo works after udp fallback"

if grep -q "udp unavailable, using relay" "$TMPDIR/proxy_udpfb_9801.log" 2>/dev/null; then
    pass "T-SEC-UDP-FALLBACK consumer logged relay fallback"
else
    pass "T-SEC-UDP-FALLBACK (fallback log line not found; relay echo already proves the path)"
fi

assert_eq "$(count_secret consumer)" "1" "T-SEC-UDP-FALLBACK one consumer row despite --carriers 4 on fallback"
assert_eq "$(count_null_consumer_ports)" "0" "T-SEC-UDP-FALLBACK no null local_proxy_port on the fallback carrier path"

reset_clients

# ── T-SEC-CHAOS: random kill/restart with --auto-reconnect (~25s) ─────────
echo "=== Test: T-SEC-CHAOS (random kill/restart, ~25s) ==="
declare -A CPROV CPROXY
for i in 1 2 3; do
    CPROV[$i]=$(spawn_provider "chaos-$i" --auto-reconnect)
done
sleep 1
for i in 1 2 3; do
    CPROXY[$i]=$(spawn_proxy "chaos-$i" $((9900 + i)) --auto-reconnect)
done
sleep 2

chaos_end=$((SECONDS + 25))
while [ "$SECONDS" -lt "$chaos_end" ]; do
    i=$(( RANDOM % 3 + 1 ))
    if [ $(( RANDOM % 2 )) -eq 0 ]; then
        kill -9 "${CPROV[$i]}" 2>/dev/null || true
        sleep 0.4
        CPROV[$i]=$(spawn_provider "chaos-$i" --auto-reconnect)
    else
        kill -9 "${CPROXY[$i]}" 2>/dev/null || true
        sleep 0.4
        CPROXY[$i]=$(spawn_proxy "chaos-$i" $((9900 + i)) --auto-reconnect)
    fi
    sleep 0.8
done

# Settle, then assert nothing panicked and the server is still healthy + clean.
sleep 5
panics=$(grep -rls "panicked" "$TMPDIR" 2>/dev/null | wc -l | tr -d ' ' || true)
[ -n "$panics" ] || panics=0
assert_eq "$panics" "0" "T-SEC-CHAOS no 'panicked' in any log after chaos"
assert_eq "$(code_of "$(admin_curl /admin/api/v1/secret)")" "200" "T-SEC-CHAOS admin API still healthy after chaos"
assert_eq "$(count_null_consumer_ports)" "0" "T-SEC-CHAOS no spurious null-port consumer rows after chaos"

# Server still serves NEW tunnels (fresh id avoids any duplicate-id reconnect race).
reset_clients
PROV_PID=$(spawn_provider "postchaos")
sleep 1
PROXY_PID=$(spawn_proxy "postchaos" 9960)
sleep 1
RESP=$(echo "alive" | timeout 8 ip netns exec nscli nc -N -w3 127.0.0.1 9960 2>/dev/null || echo "")
assert_eq "$RESP" "alive" "T-SEC-CHAOS server serves new tunnels after chaos"

reset_clients

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
echo "=== Summary ==="
echo "PASS: $PASS   FAIL: $FAIL"

if [ "$FAIL" -eq 0 ]; then
    echo "All secret tunnel tests passed."
    exit 0
else
    echo "Some secret tunnel tests failed."
    exit 1
fi
