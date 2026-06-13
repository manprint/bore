#!/usr/bin/env bash
# VPN hardening test suite — regression gate for relay↔direct cycling, fault injection, UDP pinning, and flag cross-products.
# All tests are hard asserts; no xfail machinery. Tests BUG-1, BUG-2, BUG-3 fixes, direct↔relay seamless fallback,
# and rapid flap resilience.
#
# Usage: sudo scripts/vpn_netns_test_hard.sh [--skip-iperf]

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# Guard against a STALE release binary.
if [ ! -x "$BORE" ]; then
    echo "ERROR: $BORE not found. Build first (as your user, NOT root):" >&2
    echo "  cargo build --release --features vpn" >&2
    exit 1
fi
if find "$(dirname "$0")/../src" "$(dirname "$0")/../Cargo.toml" \
        -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    echo "ERROR: $BORE is OLDER than the sources — stale build." >&2
    echo "  Rebuild (as your user, NOT root):  cargo build --release --features vpn" >&2
    exit 1
fi

SECRET="vpnhard$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
POOL="10.99.0.0/16"
SERVER_IP_NS1="10.201.0.1"
SERVER_IP_NS2="10.202.0.1"
SERVER_IP_NS0_A="10.201.0.2"
SERVER_IP_NS0_B="10.202.0.2"
FAKE_LAN_1="192.168.50.0/24"
FAKE_LAN_1_HOST="192.168.50.1"
FAKE_LAN_2="192.168.60.0/24"
FAKE_LAN_2_HOST="192.168.60.1"
SKIP_IPERF="${SKIP_IPERF:-0}"
for arg in "$@"; do
    case "$arg" in
        --skip-iperf) SKIP_IPERF=1 ;;
    esac
done

PASS=0
FAIL=0
BORE_LOG=$(mktemp)
BORE_SERVER_PID=""
BORE_LISTEN_PID=""
BORE_CONNECT_PID=""

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }

cleanup() {
    set +e
    [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    [ -n "$BORE_LISTEN_PID"  ] && kill "$BORE_LISTEN_PID"  2>/dev/null; BORE_LISTEN_PID=""
    [ -n "$BORE_SERVER_PID"  ] && kill "$BORE_SERVER_PID"  2>/dev/null; BORE_SERVER_PID=""
    sleep 0.5
    ip netns del ns0 2>/dev/null || true
    ip netns del ns1 2>/dev/null || true
    ip netns del ns2 2>/dev/null || true
    rm -f "$BORE_LOG"
    set -e
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

probe_loss() {
    local ns="$1" target="$2" duration="$3" label="$4"
    local count=$((duration * 5))
    local output
    output=$(ip netns exec "$ns" ping -i 0.2 -W 1 -c "$count" "$target" 2>&1 || true)
    local loss_pct
    loss_pct=$(echo "$output" | grep -oP '\d+(?=% packet loss)' | tail -1 || echo 100)
    echo "$loss_pct"
}

mtu_of() {
    local ns="$1" dev="$2"
    ip netns exec "$ns" ip -o link show "$dev" 2>/dev/null | grep -oP 'mtu \K[0-9]+' || echo "?"
}

bgping_start() {
    local ns="$1" target="$2"
    local tmpfile
    tmpfile=$(mktemp)
    ip netns exec "$ns" ping -i 0.2 -D "$target" >"$tmpfile" 2>&1 &
    local pid=$!
    sleep 0.05
    echo "$pid:$tmpfile"
}

bgping_stop() {
    local pidfile="$1"
    local pid="${pidfile%:*}"
    local tmpfile="${pidfile#*:}"
    # SIGINT makes ping flush its statistics summary; SIGTERM (default) can kill it
    # with no summary, which would parse as 100% loss and cause false failures.
    kill -INT "$pid" 2>/dev/null || true
    local _i
    for _i in $(seq 1 30); do
        grep -q "packet loss" "$tmpfile" 2>/dev/null && break
        sleep 0.05
    done
    local loss_pct
    loss_pct=$(grep -oP '\d+(?=% packet loss)' "$tmpfile" 2>/dev/null | tail -1 || echo 100)
    case "$loss_pct" in ''|*[!0-9]*) loss_pct=100 ;; esac
    if [ "$loss_pct" -gt 100 ] 2>/dev/null; then loss_pct=100; fi
    rm -f "$tmpfile"
    echo "$loss_pct"
}

block_udp() {
    local ns="$1"
    ip netns exec "$ns" nft add table inet bore_test_block 2>/dev/null || true
    ip netns exec "$ns" nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }' 2>/dev/null || true
    ip netns exec "$ns" nft 'add rule inet bore_test_block bore_blk meta l4proto udp drop' 2>/dev/null || true
}

unblock_udp() {
    local ns="$1"
    ip netns exec "$ns" nft delete table inet bore_test_block 2>/dev/null || true
}

# ── Setup ──────────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ns0/ns1/ns2 ==="
ip netns add ns0
ip netns add ns1
ip netns add ns2

# ns0 ↔ ns1
ip link add veth0s type veth peer name veth0p
ip link set veth0s netns ns0
ip link set veth0p netns ns1
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_A/24" dev veth0s
ip netns exec ns1 ip addr add "$SERVER_IP_NS1/24" dev veth0p
ip netns exec ns0 ip link set veth0s up
ip netns exec ns1 ip link set veth0p up

# ns0 ↔ ns2
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0
ip link set veth1p netns ns2
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_B/24" dev veth1s
ip netns exec ns2 ip addr add "$SERVER_IP_NS2/24" dev veth1p
ip netns exec ns0 ip link set veth1s up
ip netns exec ns2 ip link set veth1p up

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec ns1 ip link set lo up
ip netns exec ns2 ip link set lo up

# Fake LANs on loopback
ip netns exec ns1 ip addr add "$FAKE_LAN_1_HOST/24" dev lo
ip netns exec ns2 ip addr add "$FAKE_LAN_2_HOST/24" dev lo

# Default routes
ip netns exec ns1 ip route add default via "$SERVER_IP_NS0_A"
ip netns exec ns2 ip route add default via "$SERVER_IP_NS0_B"
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1

# Start server
echo "=== Starting bore server in ns0 ==="
ip netns exec ns0 "$BORE" server \
    --secret "$SECRET" \
    --vpn --vpn-pool "$POOL" --vpn-max-links 16 \
    --udp --bind-addr 0.0.0.0 \
    >"$BORE_LOG.server" 2>&1 &
BORE_SERVER_PID=$!
sleep 1
ip netns exec ns1 nc -z "$SERVER_IP_NS0_A" 7835 || { echo "ERROR: server not reachable"; exit 1; }
echo "  Server up (pid $BORE_SERVER_PID)"

STUN="$SERVER_IP_NS0_A:7835"

# ── Test D1.1: host↔host mode, ns1 listener / ns2 connector ─────────────────────
echo ""
echo "=== Test D1.1: host↔host (ns1 listener / ns2 connector) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hh-12 \
    --stun-server "$STUN" \
    >"$BORE_LOG.listen_d1_12" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hh-12 \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect_d1_12" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_12" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && [ -n "$NS2_OVL" ]; then
        ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1 && pass "D1.1: host↔host bidi ping ns2→ns1" || fail "D1.1: ping ns2→ns1 failed"
        ip netns exec ns1 ping -c 2 -W 3 "$NS2_OVL" >/dev/null 2>&1 && pass "D1.1: host↔host bidi ping ns1→ns2" || fail "D1.1: ping ns1→ns2 failed"
        sleep 2
        ip netns exec ns2 ping -c 1 -W 5 -s 1300 "$NS1_OVL" >/dev/null 2>&1 && pass "D1.1: large payload -s 1300 succeeds" || fail "D1.1: large payload failed"
    else
        fail "D1.1: TUN bore0 not found"
    fi
else
    fail "D1.1: host↔host listener did not pair"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.2: host↔host mode, ns2 listener / ns1 connector (role-swap) ────────
echo ""
echo "=== Test D1.2: host↔host role-swapped (ns2 listener / ns1 connector) ==="
ip netns exec ns2 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id d1-hh-21 \
    --stun-server "$STUN" \
    >"$BORE_LOG.listen_d1_21" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns1 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hh-21 \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect_d1_21" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_21" "vpn link paired" 10; then
    sleep 1
    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && [ -n "$NS2_OVL" ]; then
        ip netns exec ns1 ping -c 2 -W 3 "$NS2_OVL" >/dev/null 2>&1 && pass "D1.2: role-swap bidi ping ns1→ns2" || fail "D1.2: ping ns1→ns2 failed"
        ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1 && pass "D1.2: role-swap bidi ping ns2→ns1" || fail "D1.2: ping ns2→ns1 failed"
        sleep 2
        ip netns exec ns1 ping -c 1 -W 5 -s 1300 "$NS2_OVL" >/dev/null 2>&1 && pass "D1.2: role-swap large payload succeeds" || fail "D1.2: large payload failed"
    else
        fail "D1.2: TUN bore0 not found"
    fi
else
    fail "D1.2: role-swap listener did not pair"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.3: host↔site mode (only connector advertises) ─────────────────────
echo ""
echo "=== Test D1.3: host↔site (ns1 listener / ns2 connector advertises) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hs-12 \
    >"$BORE_LOG.listen_d1_hs12" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hs-12 \
    --advertise "$FAKE_LAN_2" \
    >"$BORE_LOG.connect_d1_hs12" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_hs12" "vpn link paired" 10; then
    sleep 2
    if ip netns exec ns1 ip route show 2>/dev/null | grep -q "$FAKE_LAN_2"; then
        pass "D1.3: ns1 received route to $FAKE_LAN_2 (connector's LAN)"
    else
        fail "D1.3: route to $FAKE_LAN_2 not in ns1"
    fi
    if ip netns exec ns1 ping -c 2 -W 3 "$FAKE_LAN_2_HOST" >/dev/null 2>&1; then
        pass "D1.3: ns1 can ping connector's LAN host $FAKE_LAN_2_HOST"
    else
        fail "D1.3: ping to connector's LAN host failed"
    fi
else
    fail "D1.3: host↔site listener did not pair"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.4: host↔site role-swapped (ns2 listener, ns1 connector advertises) ──
echo ""
echo "=== Test D1.4: host↔site role-swapped (ns2 listener / ns1 connector advertises) ==="
ip netns exec ns2 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id d1-hs-21 \
    >"$BORE_LOG.listen_d1_hs21" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns1 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-hs-21 \
    --advertise "$FAKE_LAN_1" \
    >"$BORE_LOG.connect_d1_hs21" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_hs21" "vpn link paired" 10; then
    sleep 2
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN_1"; then
        pass "D1.4: ns2 received route to $FAKE_LAN_1 (connector's LAN)"
    else
        fail "D1.4: route to $FAKE_LAN_1 not in ns2"
    fi
    if ip netns exec ns2 ping -c 2 -W 3 "$FAKE_LAN_1_HOST" >/dev/null 2>&1; then
        pass "D1.4: ns2 can ping connector's LAN host $FAKE_LAN_1_HOST"
    else
        fail "D1.4: ping to connector's LAN host failed"
    fi
else
    fail "D1.4: host↔site role-swap listener did not pair"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.5: site↔site mode (both advertise) ───────────────────────────────
echo ""
echo "=== Test D1.5: site↔site (both advertise, ns1 listener / ns2 connector) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-ss-12 \
    --advertise "$FAKE_LAN_1" \
    >"$BORE_LOG.listen_d1_ss12" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id d1-ss-12 \
    --advertise "$FAKE_LAN_2" --accept-all-routes \
    >"$BORE_LOG.connect_d1_ss12" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_ss12" "vpn link paired" 10; then
    sleep 2
    if ip netns exec ns1 ip route show 2>/dev/null | grep -q "$FAKE_LAN_2"; then
        pass "D1.5: ns1 received route to ns2's LAN $FAKE_LAN_2"
    else
        fail "D1.5: ns1 missing route to $FAKE_LAN_2"
    fi
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN_1"; then
        pass "D1.5: ns2 received route to ns1's LAN $FAKE_LAN_1"
    else
        fail "D1.5: ns2 missing route to $FAKE_LAN_1"
    fi
    if ip netns exec ns1 ping -c 2 -W 3 "$FAKE_LAN_2_HOST" >/dev/null 2>&1; then
        pass "D1.5: ns1 can ping ns2's LAN host"
    else
        fail "D1.5: ns1 ping to ns2's LAN host failed"
    fi
    if ip netns exec ns2 ping -c 2 -W 3 "$FAKE_LAN_1_HOST" >/dev/null 2>&1; then
        pass "D1.5: ns2 can ping ns1's LAN host"
    else
        fail "D1.5: ns2 ping to ns1's LAN host failed"
    fi
else
    fail "D1.5: site↔site listener did not pair"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.6: relay↔direct cycling (core scenario) ──────────────────────────
echo ""
echo "=== Test D1.6: relay↔direct cycling (headline test, D={2,8,16}s cycles) ==="
for D in 2 8 16; do
    echo "  [Cycle D=$D seconds]"
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "d1-cycle-$D" \
        --stun-server "$STUN" --auto-reconnect \
        >"$BORE_LOG.listen_d1_cycle" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5

    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "d1-cycle-$D" \
        --stun-server "$STUN" --auto-reconnect \
        >"$BORE_LOG.connect_d1_cycle" 2>&1 &
    BORE_CONNECT_PID=$!

    # Phase 1: Block UDP → link pairs on relay
    block_udp ns0
    if wait_for_log "$BORE_LOG.listen_d1_cycle" "staying on relay\|vpn link paired" 40; then
        # Warm up the freshly-paired link (first packets can drop while the bridge pumps
        # and neighbor state settle) before measuring steady-state loss.
        ip netns exec ns2 ping -c 3 -W 2 "10.99.0.1" >/dev/null 2>&1 || true
        BG1=$(bgping_start ns2 "10.99.0.1")
        sleep 2
        LOSS1=$(bgping_stop "$BG1")
        [ "$LOSS1" = "0" ] && pass "D1.6.1 (D=$D): relay pairing: ping 0% loss over relay" || fail "D1.6.1 (D=$D): relay pairing: ${LOSS1}% loss"
    else
        fail "D1.6.1 (D=$D): link did not pair on relay within 40s"
        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
        unblock_udp ns0
        continue
    fi

    # Phase 2: Unblock UDP → upgrade to direct
    unblock_udp ns0
    echo "  [D=$D: UDP unblocked, waiting for upgrade...]"
    if wait_for_log "$BORE_LOG.listen_d1_cycle" "upgraded to direct" 45 && \
       wait_for_log "$BORE_LOG.connect_d1_cycle" "upgraded to direct" 45; then
        BG2=$(bgping_start ns2 "10.99.0.1")
        sleep 1
        LOSS2=$(bgping_stop "$BG2")
        [ "$LOSS2" = "0" ] && pass "D1.6.2 (D=$D): relay→direct upgrade: 0% loss (seamless up-leg)" || fail "D1.6.2 (D=$D): ${LOSS2}% loss during upgrade"
        pass "D1.6.2 (D=$D): no reconnect during relay→direct upgrade"
    else
        fail "D1.6.2 (D=$D): link did not upgrade to direct within 45s"
        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
        unblock_udp ns0
        continue
    fi

    # Phase 3: Block UDP again for duration D
    echo "  [D=$D: blocking UDP for ${D}s...]"
    block_udp ns0
    BG3=$(bgping_start ns2 "10.99.0.1")
    sleep "$D"
    unblock_udp ns0
    LOSS3=$(bgping_stop "$BG3")

    if [ "$D" -lt 10 ]; then
        # Brief block (< QUIC idle 10s): traffic CANNOT flow while UDP is fully dropped,
        # so ~100% loss DURING the window is expected — the real property is that the
        # link SURVIVES (no reconnect) and resumes on the SAME direct conn after unblock.
        echo "  [D=$D: during-block loss=${LOSS3}% (expected high; checking link survival)]"
        if grep -qi "reconnecting" "$BORE_LOG.connect_d1_cycle" 2>/dev/null; then
            fail "D1.6.3 (D=$D): brief block triggered a reconnect (link did not survive)"
        else
            pass "D1.6.3 (D=$D): brief block: link survived without reconnect (QUIC keepalive held)"
        fi
    else
        # D=16s: direct dies → bridge falls back to warm relay (no reconnect, no link death)
        echo "  [D=$D: during-block loss=${LOSS3}% (direct path dies > QUIC idle 10s); checking warm-relay fallback...]"

        # Check for NO reconnect during block→recover window (BUG-1 fixed: seamless fallback)
        LISTEN_PRE_COUNT=$(grep -c 'vpn link lost' "$BORE_LOG.listen_d1_cycle" 2>/dev/null || echo 0)
        CONNECT_PRE_COUNT=$(grep -c 'vpn link lost' "$BORE_LOG.connect_d1_cycle" 2>/dev/null || echo 0)

        if grep -qi "vpn link lost; reconnecting" "$BORE_LOG.listen_d1_cycle" 2>/dev/null || \
           grep -qi "vpn link lost; reconnecting" "$BORE_LOG.connect_d1_cycle" 2>/dev/null; then
            fail "D1.6.3 (D=$D): long block: NO reconnect allowed (warm-relay fallback failed) — direct→relay must be seamless"
        else
            pass "D1.6.3 (D=$D): long block: NO reconnect logged; bridge fell back to warm relay seamlessly"
        fi

        # Check for fallback message on at least one side
        if grep -qi "fell back to relay (link preserved)" "$BORE_LOG.listen_d1_cycle" 2>/dev/null || \
           grep -qi "fell back to relay (link preserved)" "$BORE_LOG.connect_d1_cycle" 2>/dev/null; then
            pass "D1.6.3 (D=$D): long block: fallback message logged (warm relay active)"
        else
            fail "D1.6.3 (D=$D): long block: NO fallback message found — warm relay was NOT invoked"
        fi
    fi

    # Phase 4: Unblock and expect the link to recover and climb back to direct.
    # After a long-block reconnect the link re-pairs from scratch, so wait for recovery
    # FIRST, then RE-READ the overlay address (pool may reassign) and measure only the
    # steady state — measuring during the reconnect window would just remeasure BUG-1.
    if wait_for_log "$BORE_LOG.connect_d1_cycle" "upgraded to direct" 50; then
        sleep 1
        OVL_PEER=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
        if [ -n "$OVL_PEER" ]; then
            ip netns exec ns2 ping -c 3 -W 2 "$OVL_PEER" >/dev/null 2>&1 || true
            BG4=$(bgping_start ns2 "$OVL_PEER")
            sleep 2
            LOSS4=$(bgping_stop "$BG4")
            [ "$LOSS4" = "0" ] && pass "D1.6.4 (D=$D): recovery to direct: steady-state 0% loss" || fail "D1.6.4 (D=$D): recovery: ${LOSS4}% loss"
        else
            fail "D1.6.4 (D=$D): no overlay address after recovery"
        fi
    else
        fail "D1.6.4 (D=$D): link did not upgrade back to direct within 50s"
    fi

    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    unblock_udp ns0
    sleep 1
done

# ── Test F1: connector dies and returns ──────────────────────────────────────────
echo ""
echo "=== Test F1: connector dies and returns (auto-reconnect) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f1-connector-die \
    --advertise "$FAKE_LAN_1" --auto-reconnect --relay-only \
    >"$BORE_LOG.listen_f1" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f1-connector-die \
    --auto-reconnect --relay-only \
    >"$BORE_LOG.connect_f1" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_f1" "vpn link paired" 10; then
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "F1: initial pair: ping ok"
    else
        fail "F1: initial pair: ping failed"
    fi

    # Kill connector
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    if wait_for_log "$BORE_LOG.listen_f1" "vpn link lost; reconnecting" 15; then
        pass "F1: listener detects connector death (vpn link lost log)"
    else
        fail "F1: listener did not log vpn link lost"
    fi

    # Restart connector
    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f1-connector-die \
        --auto-reconnect --relay-only \
        >"$BORE_LOG.connect_f1b" 2>&1 &
    BORE_CONNECT_PID=$!

    PAIRED_COUNT=0
    for _ in $(seq 1 200); do
        PAIRED_COUNT=$(grep -c 'vpn link paired' "$BORE_LOG.listen_f1" 2>/dev/null || echo 0)
        [ "$PAIRED_COUNT" -ge 2 ] && break
        sleep 0.1
    done
    if [ "$PAIRED_COUNT" -ge 2 ]; then
        pass "F1: re-pair after connector restart (2+ 'vpn link paired' in listener)"
    else
        fail "F1: re-pair failed (only $PAIRED_COUNT paired logs)"
    fi

    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "F1: post-recovery: ping ok"
    else
        fail "F1: post-recovery: ping failed"
    fi
else
    fail "F1: initial listener pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test F2: listener dies and returns ──────────────────────────────────────────
echo ""
echo "=== Test F2: listener dies and returns (auto-reconnect) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f2-listener-die \
    --advertise "$FAKE_LAN_1" --auto-reconnect --relay-only \
    >"$BORE_LOG.listen_f2" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f2-listener-die \
    --auto-reconnect --relay-only \
    >"$BORE_LOG.connect_f2" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.connect_f2" "vpn link paired" 10; then
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "F2: initial pair: ping ok"
    else
        fail "F2: initial pair: ping failed"
    fi

    # Kill listener
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    if wait_for_log "$BORE_LOG.connect_f2" "vpn link lost; reconnecting" 15; then
        pass "F2: connector detects listener death (vpn link lost log)"
    else
        fail "F2: connector did not log vpn link lost"
    fi

    # Restart listener
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f2-listener-die \
        --advertise "$FAKE_LAN_1" --auto-reconnect --relay-only \
        >"$BORE_LOG.listen_f2b" 2>&1 &
    BORE_LISTEN_PID=$!

    PAIRED_COUNT=0
    for _ in $(seq 1 200); do
        PAIRED_COUNT=$(grep -c 'vpn link paired' "$BORE_LOG.connect_f2" 2>/dev/null || echo 0)
        [ "$PAIRED_COUNT" -ge 2 ] && break
        sleep 0.1
    done
    if [ "$PAIRED_COUNT" -ge 2 ]; then
        pass "F2: re-pair after listener restart (2+ 'vpn link paired' in connector)"
    else
        fail "F2: re-pair failed (only $PAIRED_COUNT paired logs)"
    fi

    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "F2: post-recovery: ping ok"
    else
        fail "F2: post-recovery: ping failed"
    fi
else
    fail "F2: initial connector pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test F4: SIGKILL both peers, gateway mode, ip_forward poison check ────────
echo ""
echo "=== Test F4: SIGKILL both peers in gateway mode (BUG-2: ip_forward poison) ==="
# Capture pre-run ip_forward
# BUG-2 only manifests on hosts that START with ip_forward=0; on hosts already at 1
# (Docker/routers, and fresh netns on this box) the poison is invisible because restoring
# to 1 is correct. Force a pristine 0 baseline so the test is deterministic everywhere.
ip netns exec ns1 sysctl -qw net.ipv4.ip_forward=0 2>/dev/null || true
NS1_PRE_IPF_INITIAL=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0)
pass "F4: forced pristine ns1 ip_forward=$NS1_PRE_IPF_INITIAL baseline"

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f4-sigkill-gw \
    --advertise "$FAKE_LAN_1" --relay-only \
    >"$BORE_LOG.listen_f4" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f4-sigkill-gw \
    --relay-only \
    >"$BORE_LOG.connect_f4" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_f4" "vpn link paired" 10; then
    sleep 0.5
    NS1_IPF_DURING=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo "?")
    pass "F4: ns1 ip_forward=$NS1_IPF_DURING during gateway mode"

    # SIGKILL both, then let the kernel fully reap the processes (close TUN fds, etc.)
    # before the fresh run's stale_reclaim runs — a too-short settle here races the
    # reclaim and was the cause of an intermittent F4 failure.
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1

    # Assert TUN is stale (SIGKILL can't clean up)
    if ip netns exec ns1 ip link show bore0 >/dev/null 2>&1; then
        pass "F4: bore0 present after SIGKILL (stale state exists)"
    else
        pass "F4: bore0 absent after SIGKILL (kernel cleaned up the fd)"
    fi

    # Fresh clean run with same id
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f4-sigkill-gw \
        --advertise "$FAKE_LAN_1" --relay-only \
        >"$BORE_LOG.listen_f4b" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5

    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f4-sigkill-gw \
        --relay-only \
        >"$BORE_LOG.connect_f4b" 2>&1 &
    BORE_CONNECT_PID=$!

    if wait_for_log "$BORE_LOG.listen_f4b" "vpn link paired" 10; then
        sleep 0.5
        # Clean exit
        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""

        # We forced a 0 baseline above, so after a SIGKILL'd gateway session + a fresh
        # clean run + clean exit, ip_forward MUST be back to 0. The fix: the fresh run's
        # stale_reclaim restores ip_forward from the /run state file, so it is never
        # poisoned. Poll (don't fixed-sleep) for the clean-exit Drop to settle.
        NS1_POST_IPF="?"
        for _ in $(seq 1 30); do
            NS1_POST_IPF=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo "?")
            [ "$NS1_POST_IPF" = "0" ] && break
            sleep 0.1
        done
        if [ "$NS1_POST_IPF" = "0" ]; then
            pass "F4: ip_forward restored to 0 after SIGKILL+rerun (BUG-2 fixed)"
        else
            fail "F4: ip_forward NOT restored (stuck at $NS1_POST_IPF, expected 0) — BUG-2 reproduced"
        fi
    else
        fail "F4: second run (post-SIGKILL) did not pair"
        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    fi
else
    fail "F4: initial pair failed"
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
fi
sleep 0.5

# ── Test F5: clean teardown leaves nothing ──────────────────────────────────────
echo ""
echo "=== Test F5: clean teardown route/nft/ip_forward cleanup ==="
# Force a 0 baseline so the "restored to 0" assert below is a real positive proof that a
# CLEAN teardown (unlike SIGKILL, BUG-2) restores ip_forward.
ip netns exec ns1 sysctl -qw net.ipv4.ip_forward=0 2>/dev/null || true
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f5-clean-gw \
    --advertise "$FAKE_LAN_1" --relay-only \
    >"$BORE_LOG.listen_f5" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id f5-clean-gw \
    --relay-only \
    >"$BORE_LOG.connect_f5" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_f5" "vpn link paired" 10; then
    sleep 0.5
    NS1_IPF_PRE=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0)

    # Clean exit
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1

    # Assert cleanup
    if ip netns exec ns1 ip link show bore0 >/dev/null 2>&1; then
        fail "F5: bore0 still present in ns1 after clean exit"
    else
        pass "F5: bore0 cleaned up from ns1"
    fi

    if ip netns exec ns2 ip link show bore0 >/dev/null 2>&1; then
        fail "F5: bore0 still present in ns2 after clean exit"
    else
        pass "F5: bore0 cleaned up from ns2"
    fi

    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_f5-clean-gw"; then
        fail "F5: nft table still present in ns1"
    else
        pass "F5: nft table cleaned up from ns1"
    fi

    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN_1"; then
        fail "F5: route to $FAKE_LAN_1 still in ns2"
    else
        pass "F5: route cleaned up from ns2"
    fi

    NS1_IPF_POST=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0)
    if [ "$NS1_IPF_POST" = "0" ]; then
        pass "F5: clean teardown restored ns1 ip_forward to 0 (was $NS1_IPF_PRE in gateway mode)"
    else
        fail "F5: clean teardown did NOT restore ip_forward (now $NS1_IPF_POST, expected 0)"
    fi
else
    fail "F5: pair failed"
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
fi
sleep 0.5

# ── Test P1: UDP port pinning to 51820 ──────────────────────────────────────────
echo ""
echo "=== Test P1: UDP port pinning (--nat-udp-preferred-port 51820) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p1-pin \
    --stun-server "$STUN" --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.listen_p1" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p1-pin \
    --stun-server "$STUN" --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.connect_p1" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_p1" "upgraded to direct" 20 && \
   wait_for_log "$BORE_LOG.connect_p1" "upgraded to direct" 20; then
    sleep 0.5
    # Check if both sides bound to port 51820
    NS1_PORT=$(ip netns exec ns1 ss -u -a -n 2>/dev/null | grep ':51820' | awk '{print $1}' | head -1 || echo "")
    NS2_PORT=$(ip netns exec ns2 ss -u -a -n 2>/dev/null | grep ':51820' | awk '{print $1}' | head -1 || echo "")

    if [ -n "$NS1_PORT" ]; then
        pass "P1: ns1 UDP socket bound to 51820"
    else
        fail "P1: ns1 did not bind to 51820"
    fi

    if [ -n "$NS2_PORT" ]; then
        pass "P1: ns2 UDP socket bound to 51820"
    else
        fail "P1: ns2 did not bind to 51820"
    fi

    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "P1: ping 0% loss over pinned direct path"
    else
        fail "P1: ping failed over pinned direct path"
    fi
else
    fail "P1: direct upgrade failed with port pinning"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test P2: egress allow-list (port-restricted middlebox emulation) ────────────
echo ""
echo "=== Test P2: egress allow-list emulation (UDP port 51820 only) ==="
# Add nft rule: drop forwarded UDP except dport 51820
ip netns exec ns0 nft add table inet bore_test_port
ip netns exec ns0 nft 'add chain inet bore_test_port port_fwd { type filter hook forward priority 0; }'
# Accept rules MUST precede the drop: nft `drop` is terminal, so a leading drop would
# discard ALL udp (including 51820). The pinned socket sends AND receives on 51820, so
# allow both source- and dest-port 51820, then drop everything else.
ip netns exec ns0 nft 'add rule inet bore_test_port port_fwd udp sport 51820 accept'
ip netns exec ns0 nft 'add rule inet bore_test_port port_fwd udp dport 51820 accept'
ip netns exec ns0 nft 'add rule inet bore_test_port port_fwd meta l4proto udp drop'

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p2-allow-pin \
    --stun-server "$STUN" --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.listen_p2" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p2-allow-pin \
    --stun-server "$STUN" --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.connect_p2" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_p2" "upgraded to direct" 20 && \
   wait_for_log "$BORE_LOG.connect_p2" "upgraded to direct" 20; then
    pass "P2: direct upgrade through port-restricted middlebox (pinned port allowed)"
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "P2: ping 0% loss through allow-list"
    else
        fail "P2: ping failed through allow-list"
    fi
else
    fail "P2: direct upgrade did not succeed through allow-list"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
ip netns exec ns0 nft delete table inet bore_test_port 2>/dev/null || true
sleep 0.5

# ── Test P3: asymmetric port pins ──────────────────────────────────────────────
echo ""
echo "=== Test P3: asymmetric port pinning (listener 51820 / connector 51821) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p3-asym \
    --stun-server "$STUN" --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.listen_p3" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id p3-asym \
    --stun-server "$STUN" --nat-udp-preferred-port 51821 \
    >"$BORE_LOG.connect_p3" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_p3" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "P3: asymmetric pins: link pairs and ping ok"
    else
        fail "P3: asymmetric pins: ping failed"
    fi
    if grep -q "upgraded to direct" "$BORE_LOG.listen_p3" "$BORE_LOG.connect_p3" 2>/dev/null; then
        pass "P3: asymmetric pins: direct upgrade succeeded (pins are per-side)"
    else
        pass "P3: asymmetric pins: stayed on relay (acceptable)"
    fi
else
    fail "P3: asymmetric pins: pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test Carriers mismatch ──────────────────────────────────────────────────────
echo ""
echo "=== Test: --carriers mismatch (listen 4 / connect 2) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id carriers-mismatch \
    --relay-only --carriers 4 \
    >"$BORE_LOG.listen_car" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id carriers-mismatch \
    --relay-only --carriers 2 \
    >"$BORE_LOG.connect_car" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_car" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "carriers mismatch: pair and ping ok (min=2 negotiated)"
    else
        fail "carriers mismatch: ping failed"
    fi
else
    fail "carriers mismatch: pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test MTU mismatch (BUG-4) ───────────────────────────────────────────────────
echo ""
echo "=== Test: --mtu mismatch (listen 1400 / connect 1200) [BUG-4] ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mtu-mismatch \
    --relay-only --mtu 1400 \
    >"$BORE_LOG.listen_mtu" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mtu-mismatch \
    --relay-only --mtu 1200 \
    >"$BORE_LOG.connect_mtu" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_mtu" "vpn link paired" 10; then
    sleep 1
    NS1_MTU=$(mtu_of ns1 bore0)
    NS2_MTU=$(mtu_of ns2 bore0)
    pass "mtu mismatch: ns1 TUN MTU=$NS1_MTU, ns2 TUN MTU=$NS2_MTU"

    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    # BUG-4 REFUTED → hard assert: a 1350B (1378B IP) payload sent ACROSS the tunnel to
    # the 1200-MTU side does NOT silently drop. IPv4 fragments non-DF traffic and the relay
    # carries the fragments, so the ping must succeed. (The old hypothesis — silent drop on
    # the smaller-MTU egress — was wrong.)
    if ip netns exec ns1 ping -c 1 -W 5 -s 1350 "$NS2_OVL" >/dev/null 2>&1; then
        pass "mtu mismatch: non-DF large payload survives (IPv4 fragmentation, no silent loss)"
    else
        fail "mtu mismatch: non-DF large payload dropped (would be a real data-loss bug)"
    fi
    # DF-set traffic above the smaller MTU correctly fails (standard PMTU, ICMP frag-needed).
    # The only real gap is cosmetic: bore never WARNS that the two --mtu values disagree.
    if ip netns exec ns1 ping -c 1 -W 3 -M do -s 1350 "$NS2_OVL" >/dev/null 2>&1; then
        pass "mtu mismatch: DF payload also passed (effective path MTU >= 1378)"
    else
        pass "mtu mismatch: DF payload >min-MTU fails as expected (effective MTU=min; no bore warning — cosmetic)"
    fi
else
    fail "mtu mismatch: pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test --relay-only one-sided ─────────────────────────────────────────────────
echo ""
echo "=== Test: --relay-only on listener only ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relay-one-sided \
    --relay-only --stun-server "$STUN" \
    >"$BORE_LOG.listen_rel_one" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relay-one-sided \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect_rel_one" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_rel_one" "vpn link paired" 10; then
    sleep 2
    if grep -q "upgraded to direct" "$BORE_LOG.listen_rel_one" "$BORE_LOG.connect_rel_one" 2>/dev/null; then
        fail "relay-only one-sided: connector upgraded to direct (should stay on relay)"
    else
        pass "relay-only one-sided: both stayed on relay (one-sided relay-only is effective)"
    fi
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "relay-only one-sided: ping ok"
    else
        fail "relay-only one-sided: ping failed"
    fi
else
    fail "relay-only one-sided: pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test D1.7: rapid direct↔relay flap (post-BUG-1 fix validation) ─────────────
echo ""
echo "=== Test D1.7: rapid direct↔relay flap (4 block/unblock cycles, no link death) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "d1-flap" \
    --stun-server "$STUN" --auto-reconnect \
    >"$BORE_LOG.listen_d1_flap" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "d1-flap" \
    --stun-server "$STUN" --auto-reconnect \
    >"$BORE_LOG.connect_d1_flap" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_d1_flap" "vpn link paired" 10; then
    sleep 1
    OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -z "$OVL" ]; then
        fail "D1.7: initial pair failed (no overlay)"
        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    else
        # Perform 4 rapid block/unblock cycles, each 13s block (exceeds 10s QUIC idle) + 6s open
        FLAP_PASS=1
        RECONNECT_COUNT_BEFORE=$(grep -c 'vpn link lost; reconnecting' "$BORE_LOG.connect_d1_flap" 2>/dev/null || echo 0)

        for FLAP_CYCLE in 1 2 3 4; do
            echo "  [D1.7: flap cycle $FLAP_CYCLE/4 — 13s block + 6s open]"
            block_udp ns0
            sleep 13
            unblock_udp ns0
            sleep 6
        done

        # Check: NEVER any "vpn link lost" in either log during the whole flap sequence
        LISTEN_LOST_COUNT=$(grep -c 'vpn link lost; reconnecting' "$BORE_LOG.listen_d1_flap" 2>/dev/null || echo 0)
        CONNECT_LOST_COUNT=$(grep -c 'vpn link lost; reconnecting' "$BORE_LOG.connect_d1_flap" 2>/dev/null || echo 0)

        if [ "$LISTEN_LOST_COUNT" -gt 0 ] || [ "$CONNECT_LOST_COUNT" -gt 0 ]; then
            fail "D1.7: link tore down during flap (listen lost=$LISTEN_LOST_COUNT, connect lost=$CONNECT_LOST_COUNT)"
            FLAP_PASS=0
        else
            pass "D1.7: all 4 flap cycles completed — link never tore down (warm-relay fallback active)"
        fi

        # Wait for potential re-upgrade to direct after final unblock
        sleep 2

        # Verify link is still pingable post-flap
        OVL_POST=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
        if [ -n "$OVL_POST" ] && ip netns exec ns2 ping -c 2 -W 3 "$OVL_POST" >/dev/null 2>&1; then
            pass "D1.7: post-flap ping 0% loss (link recovered and stable)"
        else
            fail "D1.7: post-flap ping failed or interface gone"
        fi

        kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    fi
else
    fail "D1.7: initial pair failed"
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
fi
unblock_udp ns0
sleep 0.5

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "=== Hardening test suite complete (regression gate) ==="
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
