#!/usr/bin/env bash
# vhost --udp carriers=1 concurrency-stall reproduction + regression gate.
#
# ROOT CAUSE under test — mechanism (a), QUIC CONNECTION-level flow control:
#   For a secret vhost the RESPONSE bytes flow client(provider) -> server over a
#   QUIC bidi stream, so the SERVER is the receiver. The server only returns
#   connection-level flow-control credit as it DRAINS a stream into the public
#   socket. A slow/paused public reader (browser) stalls that drain, so quinn
#   buffers up to `stream_receive_window` (16 MiB) of unread data per stalled
#   stream AGAINST the shared `connection_receive_window` (64 MiB). Only ~4 such
#   stalled streams exhaust the whole connection window -> EVERY other stream on
#   that one QUIC connection (carriers=1) starves: new requests hang "pending".
#
#   --carriers N spreads streams over N independent QUIC connections (N separate
#   windows) so a fresh request lands on an un-exhausted one -> mitigated.
#   Plain TCP relay (no --udp) uses yamux per-stream windows with no shared
#   connection cliff -> unaffected.
#
# This harness runs the provider as ROOT (CAP_NET_ADMIN present) so the UDP
# socket buffers are FORCED to 16 MiB and mechanism (b) — the non-root
# SO_*BUF clamp — is removed. What remains is mechanism (a) in isolation.
#
# PASS criteria:
#   - non-udp:        fast small request completes under load          (control)
#   - --udp carriers>=fan-out: fast small request completes under load (fixed/mitigated)
#   - --udp carriers=1: documents current behaviour. Before the flow-control fix
#                       this STALLS (repro). After the fix it must complete.
#
# Usage: sudo scripts/vhost_udp_concurrency_repro.sh
set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"
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
for tool in curl python3 nft ip; do
    command -v "$tool" >/dev/null || { echo "ERROR: $tool required" >&2; exit 1; }
done

SECRET="udprepro$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SRV_PROV="10.221.0.2"     # ns0 side, provider link
PROV="10.221.0.1"
SRV_CLI="10.223.0.2"      # ns0 side, client link
CLI="10.223.0.1"
LOG=$(mktemp -d)
ORIGIN_PORT=8890

# Slow-reader fan-out and file sizes. BIG must exceed stream_receive_window
# (16 MiB) so a stalled stream can pin a full per-stream buffer; SLOW_N * 16 MiB
# must exceed connection_receive_window (64 MiB) to guarantee exhaustion.
SLOW_N=8
BIG_MIB=48
SMALL_KIB=64
SLOW_RATE=8k           # curl --limit-rate: keep the slow streams pinned for the test
FAST_TIMEOUT=30        # hard cap for the fast probe before it counts as a full hang
LAT_MAX=3.0            # under load a fast small request must complete within this many s

PASS=0; FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }

cleanup() {
    set +e
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in ns0 nsp nsc; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-15}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# ── Topology: ns0 = server, nsp = provider, nsc = browser/client ───────────────
echo "=== Setup: netns ns0/nsp/nsc ==="
for ns in ns0 nsp nsc; do ip netns del "$ns" 2>/dev/null || true; done
ip netns add ns0; ip netns add nsp; ip netns add nsc

ip link add vethsp type veth peer name vethps
ip link set vethsp netns ns0; ip link set vethps netns nsp
ip netns exec ns0 ip addr add "$SRV_PROV/30" dev vethsp
ip netns exec nsp ip addr add "$PROV/30" dev vethps
ip netns exec ns0 ip link set vethsp up; ip netns exec nsp ip link set vethps up

ip link add vethsc type veth peer name vethcs
ip link set vethsc netns ns0; ip link set vethcs netns nsc
ip netns exec ns0 ip addr add "$SRV_CLI/30" dev vethsc
ip netns exec nsc ip addr add "$CLI/30" dev vethcs
ip netns exec ns0 ip link set vethsc up; ip netns exec nsc ip link set vethcs up

for ns in ns0 nsp nsc; do ip netns exec "$ns" ip link set lo up; done
ip netns exec nsp ip route add default via "$SRV_PROV"
ip netns exec nsc ip route add default via "$SRV_CLI"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1 2>/dev/null

# ── Origin: a tiny file server (stand-in for dufs) in the provider netns ────────
dd if=/dev/urandom of="$LOG/big.bin"   bs=1M  count="$BIG_MIB"   2>/dev/null
dd if=/dev/urandom of="$LOG/small.bin" bs=1K  count="$SMALL_KIB" 2>/dev/null
SMALL_SHA=$(sha256sum "$LOG/small.bin" | awk '{print $1}')
ip netns exec nsp python3 -m http.server "$ORIGIN_PORT" --bind 127.0.0.1 \
    --directory "$LOG" >"$LOG/origin.log" 2>&1 &
sleep 0.5

# ── Server (always --udp; serves both the direct QUIC and TCP relay paths) ─────
echo "=== Starting bore server (--udp) in ns0 ==="
RUST_LOG="${RUST_LOG:-info}" ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 --vhost-quic-port 443 --udp \
    >"$LOG/server.log" 2>&1 &
sleep 1

# start_provider <sub> <id> <logfile> <extra args...>
start_provider() {
    local sub="$1" id="$2" logf="$3"; shift 3
    RUST_LOG="${RUST_LOG:-info}" ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain "$sub" --id "$id" \
        --to "$SRV_PROV:7835" --secret "$SECRET" \
        "$@" >"$logf" 2>&1 &
    echo $!
}

# run_scenario <name> <subdomain> <extra provider args...>
#   Launches SLOW_N pinned slow readers on big.bin, then times a single fast
#   small.bin request. A healthy path serves it near-instantly even under load;
#   a connection-window-starved path (carriers=1, pre-fix) hangs many seconds.
run_scenario() {
    local name="$1" sub="$2"; shift 2
    local id="repro-$sub" logf="$LOG/$sub.provider.log"
    echo ""
    echo "=== Scenario $name (subdomain=$sub, args: $*) ==="

    local pid; pid=$(start_provider "$sub" "$id" "$logf" "$@")
    # Direct carriers (if any) must be up so the test exercises the QUIC path.
    if printf '%s ' "$@" | grep -q -- '--udp'; then
        if ! wait_for_log "$logf" "direct udp carrier ready" 15; then
            fail "$name: direct udp carrier never came up"
            kill "$pid" 2>/dev/null || true; return
        fi
    fi
    # Routing sanity (small file, no load).
    if ! ip netns exec nsc curl -s -o /dev/null -m 10 \
            -H "Host: $sub.bore.local" "http://$SRV_CLI/small.bin"; then
        fail "$name: baseline small request failed (routing/setup)"
        kill "$pid" 2>/dev/null || true; return
    fi

    # Pin SLOW_N slow readers on the big file. They hold streams open and (when
    # the public reader is slow) pin per-stream recv buffers at the server.
    local slow_pids=()
    for i in $(seq 1 "$SLOW_N"); do
        ip netns exec nsc curl -s -o /dev/null --limit-rate "$SLOW_RATE" -m 120 \
            -H "Host: $sub.bore.local" "http://$SRV_CLI/big.bin" &
        slow_pids+=($!)
    done
    # Let the slow streams ramp and saturate the (shared) connection window.
    sleep 8

    # The probe: a fast small request under load. Time it.
    local t0 t1 dt rc out
    t0=$(date +%s.%N)
    out=$(ip netns exec nsc curl -s -o "$LOG/$sub.small.out" \
            -w '%{http_code}' -m "$FAST_TIMEOUT" \
            -H "Host: $sub.bore.local" "http://$SRV_CLI/small.bin" 2>/dev/null) && rc=0 || rc=$?
    t1=$(date +%s.%N)
    dt=$(awk "BEGIN{printf \"%.1f\", $t1-$t0}")

    local got_sha=""
    [ -f "$LOG/$sub.small.out" ] && got_sha=$(sha256sum "$LOG/$sub.small.out" | awk '{print $1}')

    echo "  fast small.bin: rc=$rc http=$out time=${dt}s sha_ok=$([ "$got_sha" = "$SMALL_SHA" ] && echo yes || echo no)"

    for sp in "${slow_pids[@]}"; do kill "$sp" 2>/dev/null || true; done
    kill "$pid" 2>/dev/null || true
    sleep 1

    local within_budget
    within_budget=$(awk "BEGIN{print ($dt < $LAT_MAX) ? 1 : 0}")
    if [ "$rc" = "0" ] && [ "$out" = "200" ] && [ "$got_sha" = "$SMALL_SHA" ] && [ "$within_budget" = "1" ]; then
        pass "$name: fast request served under load in ${dt}s (< ${LAT_MAX}s)"
    elif [ "$rc" = "0" ] && [ "$out" = "200" ] && [ "$got_sha" = "$SMALL_SHA" ]; then
        fail "$name: fast request STARVED — served but took ${dt}s (>= ${LAT_MAX}s) under load"
    else
        fail "$name: fast request HUNG under load (rc=$rc http=$out ${dt}s)"
    fi
}

# Control: plain TCP relay (yamux per-stream windows) must be immune.
run_scenario "R1 non-udp (control)"              t1
# Mitigation: enough carriers to spread the slow streams over many connections.
run_scenario "R2 --udp --carriers $((SLOW_N+2))" u4   --udp --carriers $((SLOW_N+2))
# The bug: a single QUIC connection — its connection_receive_window is the
# shared bottleneck the slow streams exhaust.
run_scenario "R3 --udp carriers=1"               u1   --udp

echo ""
echo "================  $PASS passed, $FAIL failed  ================"
[ "$FAIL" -eq 0 ]
