#!/usr/bin/env bash
# vhost benchmark harness — netns topology, run with sudo.
#
# Measures throughput/latency for vhost data-plane configurations:
#   1. tcp-1c   : TCP relay, 1 carrier (no --udp)
#   2. tcp-4c   : TCP relay, 4 carriers (--carriers 4, no --udp)
#   3. udp-1c   : QUIC direct, 1 carrier (--udp)
#   4. udp-4c   : QUIC direct, 4 carriers (--udp --carriers 4)
# across HTTP throughput (large file) and latency (concurrent small requests),
# run twice: once clean, once under impairment (40ms delay + 1% loss).
#
# Usage: sudo scripts/vhost_bench.sh [seconds-per-test (default 5)]
# Output: two markdown tables on stdout (clean and impaired links).

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"
SECRET="vhbench$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SERVER_IP_NS0_PROV="10.211.0.2"
SERVER_IP_PROV="10.211.0.1"
SERVER_IP_NS0_CLI="10.213.0.2"
SERVER_IP_CLI="10.213.0.1"
DUR="${1:-5}"
LOG=$(mktemp -d)
ORIGIN_PORT=8888
ORIGIN_DATA_FILE="$LOG/testfile.bin"

# Create ~256MB random file for throughput testing
dd if=/dev/urandom of="$ORIGIN_DATA_FILE" bs=1M count=256 2>/dev/null

cleanup() {
    set +e
    pkill -P $$ 2>/dev/null
    # Kill ALL bore (server + clients) and origins — a leftover `bore server`
    # pins the namespace and makes `ip netns del` leak it.
    pkill -f 'target/release/bore' 2>/dev/null
    pkill -f 'http\.server' 2>/dev/null
    pkill -x hey 2>/dev/null; pkill -x wrk 2>/dev/null
    sleep 0.3
    pkill -9 -f 'target/release/bore' 2>/dev/null
    for ns in ns0 nsp nsc; do
        ip netns exec "$ns" tc qdisc del dev vethsp root 2>/dev/null
        ip netns exec "$ns" nft delete table inet bore_vhost_bench 2>/dev/null
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-20}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# ── Topology ──────────────────────────────────────────────────────────────────
# ns0 = server, nsp = provider+origin, nsc = client
# ns0 ↔ nsp: 10.211.0.2/.1 /30
# ns0 ↔ nsc: 10.213.0.2/.1 /30
# Delete-before-create: reclaim namespaces leaked by a previous crashed run.
for ns in ns0 nsp nsc; do
    ip netns del "$ns" 2>/dev/null || true
done
ip netns add ns0; ip netns add nsp; ip netns add nsc

# ns0 ↔ nsp
ip link add vethsp type veth peer name vethps
ip link set vethsp netns ns0; ip link set vethps netns nsp
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_PROV/30" dev vethsp
ip netns exec nsp ip addr add "$SERVER_IP_PROV/30" dev vethps
ip netns exec ns0 ip link set vethsp up; ip netns exec nsp ip link set vethps up

# ns0 ↔ nsc
ip link add vethsc type veth peer name vethcs
ip link set vethsc netns ns0; ip link set vethcs netns nsc
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_CLI/30" dev vethsc
ip netns exec nsc ip addr add "$SERVER_IP_CLI/30" dev vethcs
ip netns exec ns0 ip link set vethsc up; ip netns exec nsc ip link set vethcs up

# Loopback
for ns in ns0 nsp nsc; do ip netns exec "$ns" ip link set lo up; done

# Routes
ip netns exec nsp ip route add default via "$SERVER_IP_NS0_PROV"
ip netns exec nsc ip route add default via "$SERVER_IP_NS0_CLI"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1 2>/dev/null

# ── Start server ───────────────────────────────────────────────────────────────
ip netns exec ns0 "$BORE" server \
    --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
    --secret "$SECRET" \
    --vhost-base-domain bore.local \
    --vhost-http-port 80 \
    --udp --vhost-quic-port 443 \
    >"$LOG/server.log" 2>&1 &
sleep 1

# Throughput target file served by the origin (created in the origin's --directory).
head -c 67108864 /dev/urandom > "$LOG/testfile.bin" 2>/dev/null || \
    dd if=/dev/zero of="$LOG/testfile.bin" bs=1M count=64 2>/dev/null

# ── Self-signed wildcard cert for HTTPS ───────────────────────────────────────
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$LOG/key.pem" -out "$LOG/cert.pem" \
    -days 1 -subj "/CN=bore.local" -addext "subjectAltName=DNS:*.bore.local,DNS:bore.local" \
    >/dev/null 2>&1

# ── Start origin HTTP server in provider ns ────────────────────────────────────
ip netns exec nsp python3 -m http.server "$ORIGIN_PORT" --bind 127.0.0.1 \
    --directory "$LOG" >"$LOG/origin.log" 2>&1 &
sleep 0.5

# bench <name> <provider-args...>
# Measures throughput (concurrency) and latency (p50/p99) over the provider's subdomain.
bench() {
    local name="$1"; shift
    local pargs=("$@")

    ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain "$name" --id "bench-$name" \
        --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
        "${pargs[@]}" \
        >"$LOG/$name.provider.log" 2>&1 &
    local PROV_PID=$!
    sleep 1

    # Wait for routing / vhost registration
    sleep 2

    # Throughput: use hey if available, else curl loop
    local THROUGHPUT="—"
    local P50="—"
    local P99="—"
    local ERR_PCT="—"

    if command -v hey >/dev/null 2>&1; then
        # hey: throughput + latency percentiles in one shot
        local hey_out
        hey_out=$(ip netns exec nsc hey -z "${DUR}s" -c 50 -q 5 \
            -H "Host: $name.bore.local" \
            "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            THROUGHPUT=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
            P50=$(echo "$hey_out" | grep -oP '\[50\]\s*\K[0-9.]+ms' || echo "—")
            P99=$(echo "$hey_out" | grep -oP '\[99\]\s*\K[0-9.]+ms' || echo "—")
            ERR_PCT=$(echo "$hey_out" | grep -oP 'Non-2xx or 3xx responses:\s*\K[0-9]+' | xargs -I{} bash -c 'echo "scale=1; {} * 100 / 50" | bc' 2>/dev/null || echo "—")
        fi
    elif command -v wrk >/dev/null 2>&1; then
        # wrk: similar output
        local wrk_out
        wrk_out=$(ip netns exec nsc wrk -t 4 -c 50 -d "${DUR}s" \
            -H "Host: $name.bore.local" \
            "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || echo "")
        if [ -n "$wrk_out" ]; then
            THROUGHPUT=$(echo "$wrk_out" | grep -oP 'Requests/sec\s+\K[0-9.]+' | head -1 || echo "—")
            P50=$(echo "$wrk_out" | grep -oP '\s50%\s+\K[0-9.]+m?s' || echo "—")
            P99=$(echo "$wrk_out" | grep -oP '\s99%\s+\K[0-9.]+m?s' || echo "—")
        fi
    else
        # Fallback: parallel curl, sum download speeds (bytes/s). Collect ONLY the
        # curl PIDs and wait on those — a bare `wait` would also wait on the
        # forever-running bore server/origin and hang the whole benchmark.
        local cpids=() speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nsc curl -s -o /dev/null -w '%{speed_download}\n' \
                -H "Host: $name.bore.local" \
                "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null >>"$speedfile" || true ) &
            cpids+=("$!")
        done
        wait "${cpids[@]}" 2>/dev/null
        local sum_mbps
        sum_mbps=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
        THROUGHPUT="${sum_mbps} MB/s (10x parallel)"
    fi

    # Latency: measure p50/p99 via multiple small requests if hey/wrk unavailable
    if [ "$P50" = "—" ] && [ "$P99" = "—" ]; then
        local times=()
        for _ in $(seq 1 100); do
            local t
            t=$(timeout 10 ip netns exec nsc curl -s -o /dev/null -w '%{time_total}\n' \
                -H "Host: $name.bore.local" \
                "http://$SERVER_IP_NS0_CLI/" 2>/dev/null || echo "0")
            times+=("$t")
        done
        # Simple percentile: sort and pick index
        readarray -t sorted < <(printf '%s\n' "${times[@]}" | sort -n)
        P50="${sorted[$((${#sorted[@]} / 2))]}s"
        P99="${sorted[$((${#sorted[@]} * 99 / 100))]}s"
    fi

    echo "| $name | $THROUGHPUT | $P50 | $P99 | $ERR_PCT |"
    kill "$PROV_PID" 2>/dev/null || true
    sleep 1
}

# bench_wlog_delta <config-name> <bore-vhost-args...>
# Measures throughput WITH and WITHOUT --webserver-log, prints delta.
bench_wlog_delta() {
    local name="$1"; shift
    local pargs=("$@")
    local wlog_dir="/tmp/bore_wlog_vhost_bench"

    # Measure WITHOUT --webserver-log
    ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain "$name" --id "bench-$name-nowlog" \
        --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
        "${pargs[@]}" \
        >"$LOG/$name.provider.nowlog.log" 2>&1 &
    local PROV_PID=$!
    sleep 3

    local THROUGHPUT_NO_WLOG="—"
    if command -v hey >/dev/null 2>&1; then
        local hey_out
        hey_out=$(ip netns exec nsc hey -z "${DUR}s" -c 50 -q 5 \
            -H "Host: $name.bore.local" \
            "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            THROUGHPUT_NO_WLOG=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
        fi
    else
        # Fallback to curl
        local speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nsc curl -s -o /dev/null -w '%{speed_download}\n' \
                -H "Host: $name.bore.local" \
                "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null >>"$speedfile" || true ) &
        done
        wait
        THROUGHPUT_NO_WLOG=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
    fi
    kill "$PROV_PID" 2>/dev/null || true
    sleep 1

    # Measure WITH --webserver-log
    rm -rf "$wlog_dir" 2>/dev/null || true
    mkdir -p "$wlog_dir"

    ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain "$name" --id "bench-$name-wlog" \
        --to "$SERVER_IP_NS0_PROV:7835" --secret "$SECRET" \
        --webserver-log "$wlog_dir" \
        "${pargs[@]}" \
        >"$LOG/$name.provider.wlog.log" 2>&1 &
    local PROV_PID=$!
    sleep 3

    local THROUGHPUT_WITH_WLOG="—"
    if command -v hey >/dev/null 2>&1; then
        local hey_out
        hey_out=$(ip netns exec nsc hey -z "${DUR}s" -c 50 -q 5 \
            -H "Host: $name.bore.local" \
            "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null || echo "")
        if [ -n "$hey_out" ]; then
            THROUGHPUT_WITH_WLOG=$(echo "$hey_out" | grep -oP 'Requests/sec:\s*\K[0-9.]+' | head -1 || echo "—")
        fi
    else
        # Fallback to curl
        local speedfile
        speedfile=$(mktemp)
        for _ in $(seq 1 10); do
            ( timeout 30 ip netns exec nsc curl -s -o /dev/null -w '%{speed_download}\n' \
                -H "Host: $name.bore.local" \
                "http://$SERVER_IP_NS0_CLI/testfile.bin" 2>/dev/null >>"$speedfile" || true ) &
        done
        wait
        THROUGHPUT_WITH_WLOG=$(awk '{s+=$1} END {printf "%.0f", s/1e6}' "$speedfile" 2>/dev/null || echo "?")
        rm -f "$speedfile"
    fi
    kill "$PROV_PID" 2>/dev/null || true
    sleep 1

    # Calculate delta %
    local delta="—"
    if [ "$THROUGHPUT_NO_WLOG" != "—" ] && [ "$THROUGHPUT_WITH_WLOG" != "—" ] && \
       [ "$THROUGHPUT_NO_WLOG" != "?" ] && [ "$THROUGHPUT_WITH_WLOG" != "?" ]; then
        delta=$(echo "scale=1; ($THROUGHPUT_NO_WLOG - $THROUGHPUT_WITH_WLOG) / $THROUGHPUT_NO_WLOG * 100" | bc 2>/dev/null || echo "—")
    fi

    # Check status
    local status="PASS"
    if [ "$delta" != "—" ]; then
        if (( $(echo "$delta > 5" | bc -l 2>/dev/null) )); then
            status="FAIL (delta > 5%)"
        fi
    fi

    echo "| $name | $THROUGHPUT_NO_WLOG | $THROUGHPUT_WITH_WLOG | $delta% | $status |"
    rm -rf "$wlog_dir" 2>/dev/null || true
}

# ── BENCHMARK MATRIX: CLEAN LINK ───────────────────────────────────────────────
echo "## vhost throughput/latency benchmark ($(date -u +%F), netns, ${DUR}s per test)"
echo ""
echo "### Clean link"
echo ""
echo "| Configuration | Throughput | p50 | p99 | Error% |"
echo "|---|---|---|---|---|"
bench "tcp-1c"
bench "tcp-4c" --carriers 4
bench "udp-1c" --udp
bench "udp-4c" --udp --carriers 4

echo ""
echo "Note: Single-curl throughput does not reflect multi-carrier or QUIC wins; concurrency is key."
echo "Expected: TCP saturates on clean links; QUIC benefits under loss/RTT."
echo ""

# ── IMPAIRMENT: 40ms delay + 1% loss ───────────────────────────────────────────
ip netns exec ns0 tc qdisc add dev vethsp root netem delay 40ms loss 1% rate 100mbit

echo "### Impaired link (40ms delay, 1% loss, 100 Mbps rate-limit)"
echo ""
echo "| Configuration | Throughput | p50 | p99 | Error% |"
echo "|---|---|---|---|---|"
bench "tcp-1c" 2>&1 | sed 's/^/[impaired] /'
bench "tcp-4c" --carriers 4 2>&1 | sed 's/^/[impaired] /'
bench "udp-1c" --udp 2>&1 | sed 's/^/[impaired] /'
bench "udp-4c" --udp --carriers 4 2>&1 | sed 's/^/[impaired] /'

# Remove impairment
ip netns exec ns0 tc qdisc del dev vethsp root 2>/dev/null || true

echo ""
echo "Acceptance: QUIC gains over TCP under loss; multi-carrier > single-carrier."

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
