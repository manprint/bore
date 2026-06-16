#!/usr/bin/env bash
# VPN benchmark + diagnostics harness — netns topology, run with sudo.
#
# A CLEAN netns (RTT ~0, no loss, MTU 1500) hides every real-world bandwidth
# problem: the direct path measures multi-Gbps and carriers look pointless. The
# real link (WAN RTT, a rate cap, a sub-1452 path MTU, sometimes loss) is where
# the single-flow loss bound, the QUIC send-buffer datagram drops, and the PMTU
# black-hole flap actually bite. So this harness emulates a WAN with `tc netem`
# on the server's forwarding interfaces (the direct path ns1<->ns2 transits ns0)
# and reports, per data-plane config:
#   - iperf3 TCP single-stream  (the realistic single-flow case)
#   - iperf3 TCP -P N           (parallel — isolates "single inner TCP flow" cap)
#   - iperf3 UDP at RATE        (raw datagram capacity + loss%)
#   - fixed-volume transfer     (iperf3 -n FILE_GB → completes or STALLs)
#   - real file + sha256        (integrity, primary config only)
#   - ping RTT
#   - QUIC diag summary parsed from the new DEBUG logs (direct_diag):
#       buffer_drop_est (send-buffer drops), lost_pct, cwnd, black_holes, MTU moves
#
# Env knobs (all optional):
#   DUR=5          seconds per iperf test
#   WAN=1          1 = apply netem WAN emulation, 0 = clean netns (legacy)
#   RTT_MS=10      one-way delay per direction (RTT ~= 2*RTT_MS)
#   RATE=250mbit   per-direction rate cap (matches a 250Mbit uplink)
#   LOSS=0         netem loss percent, e.g. 0.05
#   MTU=1400       veth path MTU (sub-1452 → triggers the PMTU flap, like the real path)
#   FILE_GB=2      fixed-volume / file-transfer size
#   PARALLEL=8     streams for the TCP -P test
#   SWEEP="1 2 4 8" direct-path carrier counts to sweep
#
# Usage: sudo scripts/vpn_bench.sh [DUR]

set -euo pipefail

BORE="${BORE:-$(cd "$(dirname "$0")/.." && pwd)/target/release/bore}"
SECRET="vpnbench$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
POOL="10.99.0.0/16"
SERVER_IP_NS0_A="10.201.0.2"
SERVER_IP_NS1="10.201.0.1"
SERVER_IP_NS0_B="10.202.0.2"
SERVER_IP_NS2="10.202.0.1"
# Positional args (sudo strips env, so prefer these):
#   $1=DUR $2=FILE_GB $3=SWEEP $4=WAN $5=LOSS $6=MTU $7=RATE $8=RTT_MS
# Env still works when not run through sudo.
DUR="${1:-${DUR:-5}}"
FILE_GB="${2:-${FILE_GB:-2}}"
SWEEP="${3:-${SWEEP:-1 2 4 8}}"
WAN="${4:-${WAN:-1}}"
LOSS="${5:-${LOSS:-0}}"
MTU="${6:-${MTU:-1400}}"
RATE="${7:-${RATE:-250mbit}}"
RTT_MS="${8:-${RTT_MS:-10}}"
PARALLEL="${PARALLEL:-8}"
RUST_LOG_DIAG="info,bore_cli::vpn=debug,bore_cli::holepunch=debug"
LOG=$(mktemp -d)

command -v iperf3 >/dev/null || { echo "iperf3 required" >&2; exit 1; }
[ -x "$BORE" ] || { echo "bore binary not found at $BORE (cargo build --release --features vpn)" >&2; exit 1; }

cleanup() {
    set +e
    pkill -P $$ 2>/dev/null
    for ns in ns0 ns1 ns2; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill 2>/dev/null
        ip netns del "$ns" 2>/dev/null
    done
    rm -rf "$LOG"
    set -e
}
trap cleanup EXIT INT TERM

# ── Topology: ns1(listener) ── ns0(server/router) ── ns2(connector) ──────────
ip netns add ns0; ip netns add ns1; ip netns add ns2
ip link add veth0s type veth peer name veth0p
ip link set veth0s netns ns0; ip link set veth0p netns ns1
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_A/24" dev veth0s
ip netns exec ns1 ip addr add "$SERVER_IP_NS1/24" dev veth0p
ip netns exec ns0 ip link set veth0s up; ip netns exec ns1 ip link set veth0p up
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0; ip link set veth1p netns ns2
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_B/24" dev veth1s
ip netns exec ns2 ip addr add "$SERVER_IP_NS2/24" dev veth1p
ip netns exec ns0 ip link set veth1s up; ip netns exec ns2 ip link set veth1p up
for ns in ns0 ns1 ns2; do ip netns exec "$ns" ip link set lo up; done
ip netns exec ns1 ip route add default via "$SERVER_IP_NS0_A"
ip netns exec ns2 ip route add default via "$SERVER_IP_NS0_B"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1

# ── WAN emulation: rate + delay (+loss) on ns0's two egress legs, so the
#    forwarded direct path ns1<->ns2 is shaped in BOTH directions; small MTU on
#    every veth so the QUIC path MTU lands below quinn's 1452 probe ceiling. ──
if [ "$WAN" = 1 ]; then
    # QUIC's INITIAL packets are padded to 1200 B of UDP payload (~1228 B IP),
    # so a path MTU below ~1280 black-holes the handshake and the direct upgrade
    # never happens (looks like SETUP FAILED, not a bore bug). Clamp + warn.
    if [ "$MTU" -lt 1280 ]; then
        echo "WARNING: MTU=$MTU < 1280 breaks the QUIC handshake; clamping to 1280." >&2
        MTU=1280
    fi
    LOSSARG=""; [ "$LOSS" != 0 ] && LOSSARG="loss ${LOSS}%"
    ip netns exec ns0 tc qdisc add dev veth0s root netem delay "${RTT_MS}ms" rate "$RATE" $LOSSARG
    ip netns exec ns0 tc qdisc add dev veth1s root netem delay "${RTT_MS}ms" rate "$RATE" $LOSSARG
    ip netns exec ns1 ip link set veth0p mtu "$MTU"
    ip netns exec ns2 ip link set veth1p mtu "$MTU"
    ip netns exec ns0 ip link set veth0s mtu "$MTU"
    ip netns exec ns0 ip link set veth1s mtu "$MTU"
    WANDESC="WAN: rtt≈$((RTT_MS*2))ms rate=$RATE loss=${LOSS}% mtu=$MTU"
else
    WANDESC="clean netns (no impairment)"
fi

ip netns exec ns0 "$BORE" server --secret "$SECRET" \
    --vpn --vpn-pool "$POOL" --vpn-max-links 16 --max-carriers 16 --udp --bind-addr 0.0.0.0 \
    >"$LOG/server.log" 2>&1 &
sleep 1
STUN="$SERVER_IP_NS0_A:7835"

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-25}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

ipf_mbps() { # parse iperf3 -J sum_received Mbps from stdin
    python3 -c "import sys,json
try:
    d=json.load(sys.stdin); print(round(d['end']['sum_received']['bits_per_second']/1e6))
except Exception: print('—')" 2>/dev/null || echo "—"
}

# diag_summary <name>: parse the new DEBUG diag lines from both peer logs.
# Runs with errexit off — grep returns nonzero on no-match (every relay config),
# which would otherwise trip `set -e`/pipefail.
diag_summary() {
    local name="$1" f="$LOG/$name.l.log $LOG/$name.c.log"
    local bd lp cw bh mv qmtu
    set +e
    bd=$(grep -ohP 'buffer_drop_est=\K[0-9]+' $f 2>/dev/null | sort -n | tail -1); bd=${bd:-0}
    lp=$(grep -ohP 'lost_pct=\K[0-9.]+'       $f 2>/dev/null | sort -n | tail -1); lp=${lp:-0}
    cw=$(grep -ohP 'cwnd=\K[0-9]+'            $f 2>/dev/null | sort -n | head -1); cw=${cw:-0}
    bh=$(grep -ohP 'black_holes=\K[0-9]+'     $f 2>/dev/null | sort -n | tail -1); bh=${bh:-0}
    mv=$(grep -hE 'tun MTU adjusted'          $f 2>/dev/null | wc -l); mv=${mv:-0}
    qmtu=$(grep -ohP 'quic_mtu=\K[0-9]+'      $f 2>/dev/null | tail -1); qmtu=${qmtu:-?}
    set -e
    echo "    diag[$name]: buffer_drop_est(max)=$bd  lost_pct(max)=$lp  cwnd(min)=$cw  black_holes=$bh  mtu_changes=$mv  quic_mtu(last)=$qmtu"
}

# bench <name> <expect> <listener-args...> -- <connector-args...>
bench() {
    local name="$1" expect="$2"; shift 2
    local largs=() cargs=() in_c=0
    for a in "$@"; do
        if [ "$a" = "--" ]; then in_c=1; continue; fi
        [ "$in_c" = 0 ] && largs+=("$a") || cargs+=("$a")
    done

    RUST_LOG="$RUST_LOG_DIAG" ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "bench-$name" "${largs[@]}" >"$LOG/$name.l.log" 2>&1 &
    local LPID=$!
    sleep 0.5
    RUST_LOG="$RUST_LOG_DIAG" ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "bench-$name" "${cargs[@]}" >"$LOG/$name.c.log" 2>&1 &
    local CPID=$!

    if ! wait_for_log "$LOG/$name.l.log" "$expect" 30; then
        echo "| $name | SETUP FAILED | — | — | — | — |"
        kill $LPID $CPID 2>/dev/null; sleep 1; return
    fi
    # Give the direct upgrade + PMTU settle a moment so the steady state is measured.
    sleep 4
    local OVL
    OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

    ip netns exec ns1 iperf3 -s -D --logfile /dev/null
    sleep 0.3
    local TCP1 TCPP UDP
    # connector -> listener = the heavy uplink direction (matches the real test).
    TCP1=$(timeout $((DUR + 12)) ip netns exec ns2 iperf3 -c "$OVL" -t "$DUR" -J 2>/dev/null | ipf_mbps)
    TCPP=$(timeout $((DUR + 12)) ip netns exec ns2 iperf3 -c "$OVL" -t "$DUR" -P "$PARALLEL" -J 2>/dev/null | ipf_mbps)
    UDP=$(timeout $((DUR + 12)) ip netns exec ns2 iperf3 -c "$OVL" -t "$DUR" -u -b "$RATE" -J 2>/dev/null | \
        python3 -c "import sys,json
try:
    s=json.load(sys.stdin)['end']['sum']; print(f\"{round(s['bits_per_second']/1e6)} ({s['lost_percent']:.2f}%)\")
except Exception: print('—')" 2>/dev/null || echo "—")
    local LAT
    LAT=$(ip netns exec ns2 ping -c 30 -i 0.05 -q "$OVL" 2>/dev/null | awk -F'/' '/rtt/ {print $5}' || echo "—")
    ip netns exec ns1 pkill iperf3 2>/dev/null || true

    echo "| $name | ${TCP1} | ${TCPP} | ${UDP} | ${LAT} ms |"
    diag_summary "$name"
    kill $LPID $CPID 2>/dev/null
    sleep 1.5
}

# transfer_test <name> <listener-args...> -- <connector-args...>
# Fixed-volume (iperf3 -n) completion/stall check + real file + sha256 integrity.
transfer_test() {
    local name="$1"; shift
    local largs=() cargs=() in_c=0
    for a in "$@"; do
        if [ "$a" = "--" ]; then in_c=1; continue; fi
        [ "$in_c" = 0 ] && largs+=("$a") || cargs+=("$a")
    done
    echo ""
    echo "### Fixed-volume + file transfer (${FILE_GB} GiB) — config: $name"
    RUST_LOG="$RUST_LOG_DIAG" ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "xfer-$name" "${largs[@]}" >"$LOG/xfer.l.log" 2>&1 &
    local LPID=$!; sleep 0.5
    RUST_LOG="$RUST_LOG_DIAG" ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
        --id "xfer-$name" "${cargs[@]}" >"$LOG/xfer.c.log" 2>&1 &
    local CPID=$!
    if ! wait_for_log "$LOG/xfer.l.log" "upgraded to direct" 30; then
        echo "  SETUP FAILED (no direct upgrade)"; kill $LPID $CPID 2>/dev/null; sleep 1; return
    fi
    sleep 4
    local OVL; OVL=$(ip netns exec ns1 ip addr show bore0 | grep "inet " | awk '{print $2}' | cut -d/ -f1)

    # 1) Fixed-volume via iperf3 -n: completes (rate) or STALL (timeout).
    ip netns exec ns1 iperf3 -s -D --logfile /dev/null; sleep 0.3
    local maxsec=$(( FILE_GB * 80 + 30 ))   # generous: 1 GiB/80s floor ≈ 100Mbit
    local NJSON
    NJSON=$(timeout "$maxsec" ip netns exec ns2 iperf3 -c "$OVL" -n "${FILE_GB}G" -J 2>/dev/null || true)
    if [ -z "$NJSON" ]; then
        echo "  iperf3 -n ${FILE_GB}G: **STALL / did not complete in ${maxsec}s** ⚠"
    else
        echo "$NJSON" | python3 -c "import sys,json
d=json.load(sys.stdin); e=d['end']['sum_received']
print(f\"  iperf3 -n {$FILE_GB}G: completed in {e['seconds']:.1f}s @ {round(e['bytes']*8/e['seconds']/1e6)} Mbps\")" 2>/dev/null || echo "  iperf3 -n: parse error"
    fi
    ip netns exec ns1 pkill iperf3 2>/dev/null || true; sleep 0.5

    # 2) Real file + sha256 over the overlay (http.server in ns1 ← curl in ns2).
    if command -v curl >/dev/null && command -v sha256sum >/dev/null; then
        fallocate -l "${FILE_GB}G" "$LOG/send.bin" 2>/dev/null || head -c "$((FILE_GB*1024))M" /dev/zero >"$LOG/send.bin"
        ( cd "$LOG" && ip netns exec ns1 python3 -m http.server 8088 --bind "$OVL" >/dev/null 2>&1 ) &
        local HPID=$!; sleep 1
        local t0=$SECONDS
        if timeout "$maxsec" ip netns exec ns2 curl -s -o "$LOG/recv.bin" "http://$OVL:8088/send.bin"; then
            local dt=$(( SECONDS - t0 )); [ "$dt" -lt 1 ] && dt=1
            local sz; sz=$(stat -c%s "$LOG/recv.bin" 2>/dev/null || echo 0)
            local sa sb; sa=$(sha256sum "$LOG/send.bin" | awk '{print $1}'); sb=$(sha256sum "$LOG/recv.bin" | awk '{print $1}')
            local mbps=$(( sz * 8 / dt / 1000000 ))
            if [ "$sa" = "$sb" ]; then
                echo "  file copy: ${dt}s @ ${mbps} Mbps, sha256 **MATCH** ($sz bytes)"
            else
                echo "  file copy: ${dt}s, sha256 **MISMATCH** ⚠ (sent $sa recv $sb, $sz/$(stat -c%s "$LOG/send.bin") bytes)"
            fi
        else
            echo "  file copy: **STALL / curl failed** ⚠"
        fi
        kill $HPID 2>/dev/null
        rm -f "$LOG/send.bin" "$LOG/recv.bin"
    fi
    diag_summary "xfer"
    # raw diag tail for the record
    echo "  --- diag tail (listener) ---"
    { grep -hE "tun MTU adjusted|PMTU flap|datagram accounting|carrier quic stats" "$LOG/xfer.l.log" 2>/dev/null | tail -6 | sed 's/^/    /'; } || true
    kill $LPID $CPID 2>/dev/null; sleep 1.5
}

echo "## VPN data-plane benchmark + diagnostics ($(date -u +%F))"
echo "$WANDESC; ${DUR}s/iperf test; bore=$BORE"
echo ""
echo "| Config | TCP-1 Mbps | TCP-P${PARALLEL} Mbps | UDP ${RATE} (loss) | ping |"
echo "|---|---|---|---|---|"

# Path baselines.
bench "relay-1c"  "vpn link paired"  --relay-only -- --relay-only
bench "relay-4c"  "vpn link paired"  --relay-only --carriers 4 -- --relay-only --carriers 4

# Direct carrier sweep — THE hypothesis (parallel QUIC flows vs single loss-bound).
for c in $SWEEP; do
    bench "direct-${c}c" "upgraded to direct" \
        --stun-server "$STUN" --carriers "$c" -- --stun-server "$STUN" --carriers "$c"
done

# Direct + multi TUN queue (pps/CPU lever).
bench "direct-4c4q" "upgraded to direct" \
    --stun-server "$STUN" --carriers 4 --tun-queues 4 -- --stun-server "$STUN" --carriers 4 --tun-queues 4

# NAT netmap parity (advertise real@exposed) — data plane is IP-opaque, so this
# must match plain direct throughput; confirms NAT adds no data-plane cost.
bench "direct-nat" "upgraded to direct" \
    --stun-server "$STUN" --carriers 4 --advertise "10.10.16.0/24@100.100.16.0/24" \
    -- --stun-server "$STUN" --carriers 4 --accept-all-routes

echo ""
echo "Read: TCP-1 = single inner flow (real-world worst case). TCP-P = parallel"
echo "(removes single-TCP cap). buffer_drop_est>0 sustained = QUIC send-buffer"
echo "drops (congestion-as-loss). black_holes/mtu_changes>0 = PMTU flap."

# Large-file transfer + integrity on the two most-relevant configs.
transfer_test "direct-1c" --stun-server "$STUN" --carriers 1 -- --stun-server "$STUN" --carriers 1
transfer_test "direct-4c" --stun-server "$STUN" --carriers 4 -- --stun-server "$STUN" --carriers 4
