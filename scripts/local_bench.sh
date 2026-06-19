#!/usr/bin/env bash
# `bore local` public-tunnel benchmark harness — netns topology, run with sudo.
#
# Measures throughput/latency for the PUBLIC tunnel data plane (external client →
# server public port → bore client → local origin), across:
#   1. tcp-1c : TCP relay, 1 carrier (no --udp)
#   2. tcp-4c : TCP relay, 4 carriers (--carriers 4, no --udp)
#   3. udp-1c : QUIC direct, 1 carrier (--udp)
#   4. udp-4c : QUIC direct, 4 carriers (--udp --carriers 4)
# run twice: once on a clean link, once under impairment (40ms delay + 1% loss)
# on the server↔bore-client hop — the hop the tunnelled data rides and where the
# QUIC direct path is expected to beat the TCP relay.
#
# Usage: sudo scripts/local_bench.sh [seconds-per-test (default 5)]
# Output: two markdown tables on stdout (clean and impaired links).

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"
SRC_DIR="$(cd "$(dirname "$0")/../src" && pwd)"

# Refuse to bench a stale binary (older than any src/ file) — the netns harness
# convention. Rebuild as your user (not root): cargo build --release.
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first: cargo build --release" >&2
    exit 1
fi
if [ -n "$(find "$SRC_DIR" -newer "$BORE" -name '*.rs' -print -quit 2>/dev/null)" ]; then
    echo "ERROR: $BORE is older than src/*.rs. Rebuild: cargo build --release" >&2
    exit 1
fi

SECRET="lbench$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP_NS0_SVC="10.221.0.2"
SERVER_IP_SVC="10.221.0.1"
SERVER_IP_NS0_CLI="10.223.0.2"
SERVER_IP_CLI="10.223.0.1"
QUIC_PORT=7836
PUB_PORT=9000
ORIGIN_PORT=8888
DUR="${1:-5}"
LOG=$(mktemp -d)

cleanup() {
    set +e
    pkill -P $$ 2>/dev/null
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    pkill -x hey 2>/dev/null; pkill -x wrk 2>/dev/null
    sleep 0.3
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in ns0 nssvc nscli; do
        ip netns exec "$ns" tc qdisc del dev vethsv root 2>/dev/null
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

# ── Topology ──────────────────────────────────────────────────────────────────
# ns0 = server, nssvc = bore-local client + origin, nscli = external client.
# ns0 ↔ nssvc: 10.221.0.2/.1 /30   (carries the tunnelled data — impaired below)
# ns0 ↔ nscli: 10.223.0.2/.1 /30   (external client → public port)
for ns in ns0 nssvc nscli; do
    ip netns del "$ns" 2>/dev/null || true
done
ip netns add ns0; ip netns add nssvc; ip netns add nscli

ip link add vethsv type veth peer name vethvs
ip link set vethsv netns ns0; ip link set vethvs netns nssvc
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_SVC/30" dev vethsv
ip netns exec nssvc ip addr add "$SERVER_IP_SVC/30" dev vethvs
ip netns exec ns0 ip link set vethsv up; ip netns exec nssvc ip link set vethvs up

ip link add vethsc type veth peer name vethcs
ip link set vethsc netns ns0; ip link set vethcs netns nscli
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_CLI/30" dev vethsc
ip netns exec nscli ip addr add "$SERVER_IP_CLI/30" dev vethcs
ip netns exec ns0 ip link set vethsc up; ip netns exec nscli ip link set vethcs up

for ns in ns0 nssvc nscli; do ip netns exec "$ns" ip link set lo up; done
ip netns exec nssvc ip route add default via "$SERVER_IP_NS0_SVC"
ip netns exec nscli ip route add default via "$SERVER_IP_NS0_CLI"

# ── Start server (UDP on → QUIC endpoint binds even without vhost, DEC-LU1) ─────
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --udp --vhost-quic-port "$QUIC_PORT" \
    >"$LOG/server.log" 2>&1 &
sleep 1

# ── Origin HTTP server (in the bore-client ns) serving a 64 MiB file ────────────
head -c 67108864 /dev/urandom > "$LOG/testfile.bin" 2>/dev/null || \
    dd if=/dev/zero of="$LOG/testfile.bin" bs=1M count=64 2>/dev/null
ip netns exec nssvc python3 -m http.server "$ORIGIN_PORT" --bind 127.0.0.1 \
    --directory "$LOG" >"$LOG/origin.log" 2>&1 &
sleep 0.5

# bench <name> <bore-local-args...>
# Brings up a public tunnel on $PUB_PORT, drives the external client against it.
bench() {
    local name="$1"; shift
    local pargs=("$@")

    ip netns exec nssvc "$BORE" local "$ORIGIN_PORT" \
        --to "$SERVER_IP_NS0_SVC:7835" --secret "$SECRET" \
        --port "$PUB_PORT" \
        "${pargs[@]}" \
        >"$LOG/$name.client.log" 2>&1 &
    local CLI_PID=$!
    # Allow control + (for --udp) the QUIC direct carrier(s) to establish.
    sleep 3

    local THROUGHPUT="—" P50="—" P99="—" ERR_PCT="—"
    local url="http://$SERVER_IP_NS0_CLI:$PUB_PORT/testfile.bin"

    if command -v hey >/dev/null 2>&1; then
        local hey_out
        hey_out=$(ip netns exec nscli hey -z "${DUR}s" -c 50 -q 5 "$url" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            THROUGHPUT=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
            P50=$(echo "$hey_out" | grep -oP '\[50\]\s*\K[0-9.]+ms' || echo "—")
            P99=$(echo "$hey_out" | grep -oP '\[99\]\s*\K[0-9.]+ms' || echo "—")
            ERR_PCT=$(echo "$hey_out" | grep -oP 'Non-2xx or 3xx responses:\s*\K[0-9]+' | xargs -I{} bash -c 'echo "scale=1; {} * 100 / 50" | bc' 2>/dev/null || echo "—")
        fi
    elif command -v wrk >/dev/null 2>&1; then
        local wrk_out
        wrk_out=$(ip netns exec nscli wrk -t 4 -c 50 -d "${DUR}s" "$url" 2>/dev/null || echo "")
        if [ -n "$wrk_out" ]; then
            THROUGHPUT=$(echo "$wrk_out" | grep -oP 'Requests/sec\s+\K[0-9.]+' | head -1 || echo "—")
            P50=$(echo "$wrk_out" | grep -oP '\s50%\s+\K[0-9.]+m?s' || echo "—")
            P99=$(echo "$wrk_out" | grep -oP '\s99%\s+\K[0-9.]+m?s' || echo "—")
        fi
    else
        # Fallback: 10 parallel curls, sum download speed (bytes/s).
        local cpids=() speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nscli curl -s -o /dev/null -w '%{speed_download}\n' \
                "$url" 2>/dev/null >>"$speedfile" || true ) &
            cpids+=("$!")
        done
        wait "${cpids[@]}" 2>/dev/null
        local sum_mbps
        sum_mbps=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
        THROUGHPUT="${sum_mbps} MB/s (10x parallel)"
    fi

    if [ "$P50" = "—" ] && [ "$P99" = "—" ]; then
        local times=()
        for _ in $(seq 1 100); do
            local t
            t=$(timeout 10 ip netns exec nscli curl -s -o /dev/null -w '%{time_total}\n' \
                "http://$SERVER_IP_NS0_CLI:$PUB_PORT/" 2>/dev/null || echo "0")
            times+=("$t")
        done
        readarray -t sorted < <(printf '%s\n' "${times[@]}" | sort -n)
        P50="${sorted[$((${#sorted[@]} / 2))]}s"
        P99="${sorted[$((${#sorted[@]} * 99 / 100))]}s"
    fi

    # Direct-vs-relay observability: did the QUIC path actually carry traffic?
    local direct="—"
    grep -q "direct udp carrier ready" "$LOG/$name.client.log" 2>/dev/null && direct="yes" || direct="no"

    echo "| $name | $THROUGHPUT | $P50 | $P99 | $ERR_PCT | $direct |"
    kill "$CLI_PID" 2>/dev/null || true
    sleep 1
}

# bench_wlog_delta <config-name> <bore-local-args...>
# Measures throughput WITH and WITHOUT --webserver-log, prints delta.
# Pass if delta is within 5%.
bench_wlog_delta() {
    local name="$1"; shift
    local pargs=("$@")
    local wlog_dir="/tmp/bore_wlog_bench"

    # Measure WITHOUT --webserver-log
    ip netns exec nssvc "$BORE" local "$ORIGIN_PORT" \
        --to "$SERVER_IP_NS0_SVC:7835" --secret "$SECRET" \
        --port "$PUB_PORT" \
        "${pargs[@]}" \
        >"$LOG/$name.no_wlog.log" 2>&1 &
    local CLI_PID=$!
    sleep 3

    local tp_no_wlog="—"
    local url="http://$SERVER_IP_NS0_CLI:$PUB_PORT/testfile.bin"
    if command -v hey >/dev/null 2>&1; then
        local hey_out
        hey_out=$(ip netns exec nscli hey -z "${DUR}s" -c 50 -q 5 "$url" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            tp_no_wlog=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
        fi
    else
        # Fallback to curl
        local speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nscli curl -s -o /dev/null -w '%{speed_download}\n' \
                "$url" 2>/dev/null >>"$speedfile" || true ) &
        done
        wait
        tp_no_wlog=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
    fi
    kill "$CLI_PID" 2>/dev/null || true
    sleep 1

    # Measure WITH --webserver-log
    rm -rf "$wlog_dir" 2>/dev/null || true
    mkdir -p "$wlog_dir"

    ip netns exec nssvc "$BORE" local "$ORIGIN_PORT" \
        --to "$SERVER_IP_NS0_SVC:7835" --secret "$SECRET" \
        --port "$PUB_PORT" \
        --webserver-log "$wlog_dir" \
        "${pargs[@]}" \
        >"$LOG/$name.with_wlog.log" 2>&1 &
    local CLI_PID=$!
    sleep 3

    local tp_with_wlog="—"
    if command -v hey >/dev/null 2>&1; then
        local hey_out
        hey_out=$(ip netns exec nscli hey -z "${DUR}s" -c 50 -q 5 "$url" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            tp_with_wlog=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
        fi
    else
        # Fallback to curl
        local speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nscli curl -s -o /dev/null -w '%{speed_download}\n' \
                "$url" 2>/dev/null >>"$speedfile" || true ) &
        done
        wait
        tp_with_wlog=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
    fi
    kill "$CLI_PID" 2>/dev/null || true
    sleep 1

    # Calculate delta %
    local delta="—"
    if [ "$tp_no_wlog" != "—" ] && [ "$tp_with_wlog" != "—" ] && [ "$tp_no_wlog" != "?" ] && [ "$tp_with_wlog" != "?" ]; then
        # delta = (no_wlog - with_wlog) / no_wlog * 100
        delta=$(echo "scale=1; ($tp_no_wlog - $tp_with_wlog) / $tp_no_wlog * 100" | bc 2>/dev/null || echo "—")
    fi

    # Check if within threshold (5%)
    local status="PASS"
    if [ "$delta" != "—" ]; then
        # Compare as floats: if delta > 5, fail
        if (( $(echo "$delta > 5" | bc -l 2>/dev/null) )); then
            status="FAIL (delta > 5%)"
        fi
    fi

    echo "| $name | $tp_no_wlog | $tp_with_wlog | $delta% | $status |"
    rm -rf "$wlog_dir" 2>/dev/null || true
}

echo "## bore local throughput/latency benchmark ($(date -u +%F), netns, ${DUR}s per test)"
echo ""
echo "### Clean link"
echo ""
echo "| Configuration | Throughput | p50 | p99 | Error% | Direct? |"
echo "|---|---|---|---|---|---|"
bench "tcp-1c"
bench "tcp-4c" --carriers 4
bench "udp-1c" --udp
bench "udp-4c" --udp --carriers 4

echo ""
echo "Note: single-curl throughput hides multi-carrier/QUIC wins; concurrency is key."
echo ""

# ── Impairment on the server↔bore-client hop (the tunnelled-data path) ──────────
ip netns exec ns0 tc qdisc add dev vethsv root netem delay 40ms loss 1% rate 100mbit

echo "### Impaired link (40ms delay, 1% loss, 100 Mbps on server↔client hop)"
echo ""
echo "| Configuration | Throughput | p50 | p99 | Error% | Direct? |"
echo "|---|---|---|---|---|---|"
bench "tcp-1c" 2>&1 | sed 's/^/[impaired] /'
bench "tcp-4c" --carriers 4 2>&1 | sed 's/^/[impaired] /'
bench "udp-1c" --udp 2>&1 | sed 's/^/[impaired] /'
bench "udp-4c" --udp --carriers 4 2>&1 | sed 's/^/[impaired] /'

ip netns exec ns0 tc qdisc del dev vethsv root 2>/dev/null || true

echo ""
echo "Acceptance: QUIC (BBR) should gain over the TCP relay under loss; the Direct?"
echo "column must read 'yes' for udp-* rows (else it silently fell back to relay)."

# ── T-WLBENCH: webserver-log bandwidth impact ────────────────────────────────────
echo ""
echo "### T-WLBENCH: --webserver-log throughput impact"
echo ""
echo "| Configuration | No logging (req/s) | With logging (req/s) | Delta % | Status |"
echo "|---|---|---|---|---|"
bench_wlog_delta "wlog-tcp-1c"
bench_wlog_delta "wlog-tcp-4c" --carriers 4
bench_wlog_delta "wlog-udp-1c" --udp
bench_wlog_delta "wlog-udp-4c" --udp --carriers 4

echo ""
echo "Acceptance: throughput with --webserver-log must be within 5% of without."
echo "The logging must never cause the data path to block or throttle."
