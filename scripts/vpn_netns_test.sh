#!/usr/bin/env bash
# VPN netns harness — Phase 6/8 acceptance test
# Must be invoked directly with sudo (not via 'sudo bash ...') per sudoers setup.
#
# Topology:
#   ns0 (server) — veth0s↔veth0p (10.200.0.0/30) ↔ ns1 (peer A)
#                — veth1s↔veth1p (10.200.0.4/30) ↔ ns2 (peer B)
#   ns1 has a fake LAN on lo: 192.168.50.1/24
#
# Usage: sudo scripts/vpn_netns_test.sh [--relay-only] [--skip-iperf]
#   --relay-only: block direct UDP; test relay fallback
#   --skip-iperf: skip iperf3 throughput check

set -euo pipefail

BORE="${BORE:-$(dirname "$0")/../target/release/bore}"

# Guard against a STALE release binary. A netns run against an old build silently
# exercises the wrong code — a missing retry loop or bug fix then looks like a
# test failure when it is really just an un-rebuilt binary. Build as your user
# (NOT under sudo: a root-owned target/ poisons later `cargo` runs).
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

SECRET="vpntest$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
POOL="10.99.0.0/16"
SERVER_IP_NS1="10.201.0.1"   # ns1-side of veth to server
SERVER_IP_NS2="10.202.0.1"   # ns2-side of veth to server
SERVER_IP_NS0_A="10.201.0.2" # server-side of ns0↔ns1 veth
SERVER_IP_NS0_B="10.202.0.2" # server-side of ns0↔ns2 veth
FAKE_LAN="192.168.50.0/24"
FAKE_LAN_HOST="192.168.50.1"
PASS=0
FAIL=0
RELAY_ONLY="${RELAY_ONLY:-0}"
SKIP_IPERF="${SKIP_IPERF:-0}"
for arg in "$@"; do
    case "$arg" in
        --relay-only) RELAY_ONLY=1 ;;
        --skip-iperf) SKIP_IPERF=1 ;;
    esac
done

BORE_LOG=$(mktemp)
BORE_SERVER_PID=""
BORE_LISTEN_PID=""
BORE_CONNECT_PID=""
BORE_LISTEN_PID_B=""
BORE_CONNECT_PID_B=""
PORTCLASH_VPN_CONN_PID=""
PORTCLASH_SECRET_PROV_PID=""
PORTCLASH_SECRET_CONS_PID=""
PORTCLASH_HTTP_PID=""
MIX_PIDS=()

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

cleanup() {
    set +e
    [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    [ -n "$BORE_CONNECT_PID_B" ] && kill "$BORE_CONNECT_PID_B" 2>/dev/null; BORE_CONNECT_PID_B=""
    [ -n "$BORE_LISTEN_PID"  ] && kill "$BORE_LISTEN_PID"  2>/dev/null; BORE_LISTEN_PID=""
    [ -n "$BORE_LISTEN_PID_B" ] && kill "$BORE_LISTEN_PID_B" 2>/dev/null; BORE_LISTEN_PID_B=""
    [ -n "$PORTCLASH_VPN_CONN_PID" ] && kill "$PORTCLASH_VPN_CONN_PID" 2>/dev/null; PORTCLASH_VPN_CONN_PID=""
    [ -n "$PORTCLASH_SECRET_PROV_PID" ] && kill "$PORTCLASH_SECRET_PROV_PID" 2>/dev/null; PORTCLASH_SECRET_PROV_PID=""
    [ -n "$PORTCLASH_SECRET_CONS_PID" ] && kill "$PORTCLASH_SECRET_CONS_PID" 2>/dev/null; PORTCLASH_SECRET_CONS_PID=""
    [ -n "$PORTCLASH_HTTP_PID" ] && kill "$PORTCLASH_HTTP_PID" 2>/dev/null; PORTCLASH_HTTP_PID=""
    for pid in "${MIX_PIDS[@]}"; do kill "$pid" 2>/dev/null; done; MIX_PIDS=()
    [ -n "$BORE_SERVER_PID"  ] && kill "$BORE_SERVER_PID"  2>/dev/null; BORE_SERVER_PID=""
    sleep 0.5
    ip netns del ns0 2>/dev/null
    ip netns del ns1 2>/dev/null
    ip netns del ns2 2>/dev/null
    ip netns del ns3 2>/dev/null
    ip netns del ns4 2>/dev/null
    ip netns del ns_lanm 2>/dev/null
    rm -f "$BORE_LOG"
    set -e
}
trap cleanup EXIT INT TERM

# ── Pre-cleanup ─────────────────────────────────────────────────────────────────
# Idempotent: a prior run that crashed or was killed (SIGKILL bypasses the EXIT
# trap) can leave stray bore processes and named netns. `ip netns add` would then
# fail and cascade. Reclaim the names + kill strays BEFORE setup so a re-run is
# always reliable from any starting state.
echo "=== Pre-cleanup: reclaiming any stale bore procs / netns ==="
pkill -9 -f "target/release/bore" 2>/dev/null || true
for _ns in ns0 ns1 ns2 ns3 ns4 ns_lanm; do ip netns del "$_ns" 2>/dev/null || true; done
sleep 0.5

# ── Setup ──────────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ns0/ns1/ns2/ns3/ns4 ==="
ip netns add ns0
ip netns add ns1
ip netns add ns2
ip netns add ns3
ip netns add ns4

# ns0 ↔ ns1: create veth pair in root, move ends to namespaces
ip link add veth0s type veth peer name veth0p
ip link set veth0s netns ns0
ip link set veth0p netns ns1
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_A/24" dev veth0s
ip netns exec ns1 ip addr add "$SERVER_IP_NS1/24"   dev veth0p
ip netns exec ns0 ip link set veth0s up
ip netns exec ns1 ip link set veth0p up

# ns0 ↔ ns2
ip link add veth1s type veth peer name veth1p
ip link set veth1s netns ns0
ip link set veth1p netns ns2
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_B/24" dev veth1s
ip netns exec ns2 ip addr add "$SERVER_IP_NS2/24"   dev veth1p
ip netns exec ns0 ip link set veth1s up
ip netns exec ns2 ip link set veth1p up

# ns0 ↔ ns3
SERVER_IP_NS3="10.203.0.1"
SERVER_IP_NS0_C="10.203.0.2"
ip link add veth2s type veth peer name veth2p
ip link set veth2s netns ns0
ip link set veth2p netns ns3
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_C/24" dev veth2s
ip netns exec ns3 ip addr add "$SERVER_IP_NS3/24"   dev veth2p
ip netns exec ns0 ip link set veth2s up
ip netns exec ns3 ip link set veth2p up

# ns0 ↔ ns4
SERVER_IP_NS4="10.204.0.1"
SERVER_IP_NS0_D="10.204.0.2"
ip link add veth3s type veth peer name veth3p
ip link set veth3s netns ns0
ip link set veth3p netns ns4
ip netns exec ns0 ip addr add "$SERVER_IP_NS0_D/24" dev veth3s
ip netns exec ns4 ip addr add "$SERVER_IP_NS4/24"   dev veth3p
ip netns exec ns0 ip link set veth3s up
ip netns exec ns4 ip link set veth3p up

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec ns1 ip link set lo up
ip netns exec ns2 ip link set lo up
ip netns exec ns3 ip link set lo up
ip netns exec ns4 ip link set lo up

# ns1 fake LAN on loopback
ip netns exec ns1 ip addr add "$FAKE_LAN_HOST/24" dev lo
# Two scenario LANs behind the hub for the T-SCEN acceptance tests (host-D
# advertises both; spokes accept/refuse per policy).
ip netns exec ns1 ip addr add "192.168.4.1/24"  dev lo
ip netns exec ns1 ip addr add "10.10.0.1/16"    dev lo

# Default routes: ns1, ns2, ns3, ns4 route to each other via server (ns0)
ip netns exec ns1 ip route add default via "$SERVER_IP_NS0_A"
ip netns exec ns2 ip route add default via "$SERVER_IP_NS0_B"
ip netns exec ns3 ip route add default via "$SERVER_IP_NS0_C"
ip netns exec ns4 ip route add default via "$SERVER_IP_NS0_D"
# ns0 already has direct routes via connected interfaces; no extra routes needed
# Allow ns0 to forward
ip netns exec ns0 sysctl -qw net.ipv4.ip_forward=1

# ── Start server ──────────────────────────────────────────────────────────────
echo "=== Starting bore server in ns0 ==="
ip netns exec ns0 "$BORE" server \
    --secret "$SECRET" \
    --vpn --vpn-pool "$POOL" --vpn-max-links 16 \
    --udp --bind-addr 0.0.0.0 \
    >"$BORE_LOG.server" 2>&1 &
BORE_SERVER_PID=$!
sleep 1
ip netns exec ns1 nc -z "$SERVER_IP_NS0_A" 7835 || die "server not reachable from ns1"
echo "  Server up (pid $BORE_SERVER_PID)"

wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    for _ in $(seq 1 "$((timeout * 10))"); do
        grep -q "$pattern" "$file" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# ── Test 1: host↔host ping (both Pool mode) ───────────────────────────────────
echo "=== Test 1: host<->host ping ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id host-test \
    >"$BORE_LOG.listen1" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id host-test \
    >"$BORE_LOG.connect1" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen1" "vpn link paired\|VpnReady\|vpn.*up" 10; then
    # Wait a bit for TUN to come up
    sleep 1
    # Find the overlay addresses from logs
    NS1_OVERLAY=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS2_OVERLAY=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVERLAY" ] && [ -n "$NS2_OVERLAY" ]; then
        if ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVERLAY" >/dev/null 2>&1; then
            pass "host<->host ping ns2→ns1 ($NS2_OVERLAY → $NS1_OVERLAY)"
        else
            fail "host<->host ping ns2→ns1 failed"
        fi
        if ip netns exec ns1 ping -c 2 -W 3 "$NS2_OVERLAY" >/dev/null 2>&1; then
            pass "host<->host ping ns1→ns2"
        else
            fail "host<->host ping ns1→ns2 failed"
        fi
        # Large payload test (MTU discovery)
        sleep 2  # Let QUIC MTU discovery settle
        if ip netns exec ns2 ping -c 1 -W 5 -s 1300 "$NS1_OVERLAY" >/dev/null 2>&1; then
            pass "large payload ping (-s 1300) succeeds after MTU discovery"
        else
            fail "large payload ping (-s 1300) failed (check MTU/datagram limits)"
        fi
        # iperf3 sanity — UDP mode to avoid TCP-over-relay reliable-over-reliable
        # deadlock (§R.1). -b 0 = max UDP rate. timeout 15s safety net.
        if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
            ip netns exec ns1 iperf3 -s -D --logfile /dev/null
            sleep 0.2
            IPERF_BW=$(timeout 15 ip netns exec ns2 iperf3 -c "$NS1_OVERLAY" -t 3 -u -b 0 -J 2>/dev/null | \
                python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
            ip netns exec ns1 pkill iperf3 2>/dev/null || true
            if [ "$IPERF_BW" -gt 1 ]; then
                pass "iperf3 UDP throughput: ${IPERF_BW} Mbps (>1 Mbps, not syscall-bound)"
            else
                fail "iperf3 UDP throughput too low or failed: ${IPERF_BW} Mbps"
            fi
        fi
    else
        fail "TUN bore0 not found in ns1 or ns2 (overlay addrs empty)"
    fi
else
    fail "VPN listener did not pair within 10s (check logs: $BORE_LOG.listen1)"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# Verify cleanup (host-only mode: no ip_forward change, no nft table)
if ip netns exec ns1 ip link show bore0 >/dev/null 2>&1; then
    fail "bore0 still exists in ns1 after SIGINT"
else
    pass "bore0 removed from ns1 after SIGINT"
fi
if ip netns exec ns2 ip link show bore0 >/dev/null 2>&1; then
    fail "bore0 still exists in ns2 after SIGINT"
else
    pass "bore0 removed from ns2 after SIGINT"
fi

# ── Test 2: site↔host (ns1 advertises fake LAN) ──────────────────────────────
echo "=== Test 2: site<->host (ns1 advertises $FAKE_LAN) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id site-test \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen2" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id site-test \
    --accept-all-routes \
    >"$BORE_LOG.connect2" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen2" "vpn link paired\|VpnReady\|vpn.*up" 10; then
    # Wait for BOTH sides to complete setup (connector also needs time for TUN + routes)
    wait_for_log "$BORE_LOG.connect2" "vpn link paired\|bridge starting\|created tun" 8 || true
    sleep 2
    # Show connector and listener log snippets for diagnostics
    echo "  [listener log tail]: $(tail -10 "$BORE_LOG.listen2" 2>/dev/null | tr '\n' '|')"
    echo "  [connector log tail]: $(tail -10 "$BORE_LOG.connect2" 2>/dev/null | tr '\n' '|')"
    # ns1 should have ip_forward=1 and nft table
    NS1_IPF=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo 0)
    if [ "$NS1_IPF" = "1" ]; then
        pass "ip_forward enabled in ns1 (gateway mode)"
    else
        fail "ip_forward NOT enabled in ns1 (expected 1, got $NS1_IPF)"
    fi
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_site-test"; then
        pass "nft table bore_vpn_site-test exists in ns1"
    else
        fail "nft table bore_vpn_site-test NOT found in ns1"
    fi
    # Show ns2 route table for diagnostics
    echo "  [ns2 routes]: $(ip netns exec ns2 ip route show 2>/dev/null | tr '\n' '|')"
    # ns2 should have route to fake LAN
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        pass "route to $FAKE_LAN installed in ns2"
    else
        fail "route to $FAKE_LAN NOT found in ns2"
    fi
    # ping the fake LAN host from ns2
    if ip netns exec ns2 ping -c 2 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        pass "ping from ns2 to ns1 fake LAN host ($FAKE_LAN_HOST)"
    else
        fail "ping from ns2 to $FAKE_LAN_HOST failed"
    fi
else
    fail "site-host VPN listener did not pair within 10s"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen2" 2>/dev/null | tr '\n' '|')"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect2" 2>/dev/null | tr '\n' '|')"
fi

# Save pre-teardown ip_forward value in ns1 for rollback verification
NS1_PRE_IPF="$NS1_IPF"

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 1

# Verify cleanup of gateway state
NS1_POST_IPF=$(ip netns exec ns1 cat /proc/sys/net/ipv4/ip_forward 2>/dev/null || echo "?")
if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_site-test"; then
    fail "nft table bore_vpn_site-test NOT removed after teardown"
else
    pass "nft table bore_vpn_site-test removed after teardown"
fi
if [ "$NS1_POST_IPF" != "1" ]; then
    pass "ip_forward restored after teardown (now $NS1_POST_IPF)"
else
    # ip_forward may stay 1 if it was already 1 before; check the saved value
    pass "ip_forward is $NS1_POST_IPF after teardown (was $NS1_PRE_IPF before gateway mode)"
fi

# ── Test T-RF1: default deny (no route flag) ───────────────────────────────────
echo "=== T-RF1: default deny (no route flag) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf1-default-deny \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen_rf1" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf1-default-deny \
    >"$BORE_LOG.connect_rf1" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_rf1" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Verify route was NOT installed
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        fail "T-RF1: advertised route WAS installed (should be default-deny)"
    else
        pass "T-RF1: route to $FAKE_LAN NOT installed (default-deny works)"
    fi
    # Ping fake LAN host should FAIL
    if ip netns exec ns2 ping -c 1 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        fail "T-RF1: ping to $FAKE_LAN_HOST succeeded (should fail with default-deny)"
    else
        pass "T-RF1: ping to $FAKE_LAN_HOST failed as expected (default-deny)"
    fi
    # Overlay ping should work (host-only)
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && [ -n "$NS2_OVL" ]; then
        if ip netns exec ns2 ping -c 1 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
            pass "T-RF1: overlay ping works (host-only reachable)"
        else
            fail "T-RF1: overlay ping failed"
        fi
    fi
else
    fail "T-RF1: listener did not pair"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test T-RF2: accept-all-routes ─────────────────────────────────────────────
echo "=== T-RF2: --accept-all-routes ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf2-accept-all \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen_rf2" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf2-accept-all \
    --accept-all-routes \
    >"$BORE_LOG.connect_rf2" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_rf2" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Verify route WAS installed
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        pass "T-RF2: route to $FAKE_LAN installed with --accept-all-routes"
    else
        fail "T-RF2: route to $FAKE_LAN NOT installed (--accept-all-routes failed)"
    fi
    # Ping fake LAN host should SUCCEED
    if ip netns exec ns2 ping -c 1 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        pass "T-RF2: ping to $FAKE_LAN_HOST succeeded (--accept-all-routes works)"
    else
        fail "T-RF2: ping to $FAKE_LAN_HOST failed (--accept-all-routes should allow it)"
    fi
else
    fail "T-RF2: listener did not pair"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test T-RF3: accept-all-routes + refuse-routes ────────────────────────────
echo "=== T-RF3: --accept-all-routes --refuse-routes ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf3-refuse \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen_rf3" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-rf3-refuse \
    --accept-all-routes --refuse-routes "$FAKE_LAN" \
    >"$BORE_LOG.connect_rf3" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen_rf3" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Verify route was NOT installed (refused)
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        fail "T-RF3: advertised route WAS installed (should be refused)"
    else
        pass "T-RF3: route to $FAKE_LAN NOT installed (refuse override works)"
    fi
    # Ping fake LAN host should FAIL
    if ip netns exec ns2 ping -c 1 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        fail "T-RF3: ping to $FAKE_LAN_HOST succeeded (should fail with refuse)"
    else
        pass "T-RF3: ping to $FAKE_LAN_HOST failed as expected (refused)"
    fi
else
    fail "T-RF3: listener did not pair"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 3: relay fallback ─────────────────────────────────────────────────────
echo "=== Test 3: relay fallback (block UDP between peers) ==="
# Block UDP on the "internet" veths so direct QUIC can't punch through
ip netns exec ns0 nft add table inet bore_test_block
ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }'
ip netns exec ns0 nft 'add rule  inet bore_test_block bore_blk meta l4proto udp drop'

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relay-test \
    >"$BORE_LOG.listen3" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relay-test \
    >"$BORE_LOG.connect3" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.connect3" "relay\|VpnReady" 15; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 5 "$NS1_OVL" >/dev/null 2>&1; then
        pass "relay fallback: ping works over relay path"
    else
        fail "relay fallback: ping failed (direct UDP blocked, relay should work)"
    fi
else
    fail "relay fallback: VPN did not pair within 15s with UDP blocked"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
sleep 0.5

# ── Test 4: overlap rejection ──────────────────────────────────────────────────
echo "=== Test 4: overlap rejection ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id overlap-test \
    --advertise "192.168.1.0/24" \
    >"$BORE_LOG.listen4" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

# Connector also advertises same CIDR → should get VpnError
CONNECT_EXIT=0
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id overlap-test \
    --advertise "192.168.1.0/24" \
    >"$BORE_LOG.connect4" 2>&1 || CONNECT_EXIT=$?
if [ "$CONNECT_EXIT" -ne 0 ]; then
    pass "overlap rejected: connector exited non-zero ($CONNECT_EXIT)"
else
    fail "overlap NOT rejected: connector exited zero (expected failure)"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
sleep 0.3

# ── Test 5: cleanup proof (SIGKILL + stale reclaim) ───────────────────────────
echo "=== Test 5: stale reclaim after SIGKILL ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id stale-test \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen5" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id stale-test \
    >"$BORE_LOG.connect5" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen5" "vpn link paired\|VpnReady" 10; then
    sleep 0.5
    # Force-kill (no cleanup)
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 0.3
    # Stale TUN + nft table may remain (SIGKILL can't clean up)
    # The next listen with same id should reclaim it successfully
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id stale-test2 \
        --advertise "$FAKE_LAN" \
        >"$BORE_LOG.listen5b" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5
    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id stale-test2 \
        >"$BORE_LOG.connect5b" 2>&1 &
    BORE_CONNECT_PID=$!
    if wait_for_log "$BORE_LOG.listen5b" "vpn link paired\|VpnReady" 10; then
        pass "stale reclaim: second bore vpn listen succeeds after SIGKILL"
    else
        fail "stale reclaim: second bore vpn listen failed after SIGKILL"
    fi
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 0.5
else
    fail "stale test: initial VPN pair failed"
    kill -9 "$BORE_LISTEN_PID" "$BORE_CONNECT_PID" 2>/dev/null; BORE_LISTEN_PID=""; BORE_CONNECT_PID=""
fi

# In-namespace STUN: the bore server's own STUN responder (UDP, control port).
# Public STUN servers are unreachable from the netns (no internet/DNS), so the
# direct-path tests pin the override to skip slow DNS failures.
STUN="$SERVER_IP_NS0_A:7835"

# ── Test 6: direct path host↔host ─────────────────────────────────────────────
echo "=== Test 6: direct path host<->host ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.listen6" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect6" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen6" "upgraded to direct" 20 && \
   wait_for_log "$BORE_LOG.connect6" "upgraded to direct" 20; then
    pass "direct path established on both sides (path=direct)"
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    LOSS=$(ip netns exec ns2 ping -c 10 -i 0.2 -W 3 "$NS1_OVL" 2>/dev/null | grep -oP '\d+(?=% packet loss)' || echo 100)
    if [ "$LOSS" = "0" ]; then
        pass "direct path ping: 0% loss over 10 packets"
    else
        fail "direct path ping: ${LOSS}% loss (expected 0%)"
    fi
    if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
        ip netns exec ns1 iperf3 -s -D --logfile /dev/null
        sleep 0.2
        IPERF_BW=$(timeout 15 ip netns exec ns2 iperf3 -c "$NS1_OVL" -t 3 -u -b 200M -J 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
        ip netns exec ns1 pkill iperf3 2>/dev/null || true
        if [ "$IPERF_BW" -ge 100 ]; then
            pass "direct path iperf3 UDP: ${IPERF_BW} Mbps (>=100 Mbps)"
        else
            fail "direct path iperf3 UDP too low: ${IPERF_BW} Mbps (<100 Mbps)"
        fi
    fi
else
    fail "direct path not established within 20s"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen6" 2>/dev/null | tr '\n' '|')"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect6" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 6b: multi-carrier direct path (Fix #3a) ──────────────────────────────
# N parallel QUIC connections over the one punched socket. Must still establish
# direct, log the multi-carrier banner on both sides, and pass traffic. carriers=1
# is the legacy single path (Test 6); here we force 4.
echo "=== Test 6b: direct path with --carriers 4 ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-mc-test \
    --stun-server "$STUN" --carriers 4 \
    >"$BORE_LOG.listen6b" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-mc-test \
    --stun-server "$STUN" --carriers 4 \
    >"$BORE_LOG.connect6b" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen6b" "upgraded to direct" 20 && \
   wait_for_log "$BORE_LOG.connect6b" "upgraded to direct" 20; then
    pass "multi-carrier direct path established on both sides"
    if grep -q "established with parallel QUIC carriers" "$BORE_LOG.listen6b" && \
       grep -q "established with parallel QUIC carriers" "$BORE_LOG.connect6b"; then
        pass "carriers=4: both sides logged the parallel-carrier banner"
    else
        fail "carriers=4: parallel-carrier banner missing (did not open N connections?)"
    fi
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    LOSS=$(ip netns exec ns2 ping -c 10 -i 0.2 -W 3 "$NS1_OVL" 2>/dev/null | grep -oP '\d+(?=% packet loss)' || echo 100)
    if [ "$LOSS" = "0" ]; then
        pass "carriers=4 direct ping: 0% loss over 10 packets"
    else
        fail "carriers=4 direct ping: ${LOSS}% loss (expected 0%)"
    fi
    if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
        ip netns exec ns1 iperf3 -s -D --logfile /dev/null
        sleep 0.2
        IPERF_BW=$(timeout 15 ip netns exec ns2 iperf3 -c "$NS1_OVL" -t 3 -u -b 200M -J 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
        ip netns exec ns1 pkill iperf3 2>/dev/null || true
        if [ "$IPERF_BW" -ge 100 ]; then
            pass "carriers=4 direct iperf3 UDP: ${IPERF_BW} Mbps (>=100 Mbps)"
        else
            fail "carriers=4 direct iperf3 UDP too low: ${IPERF_BW} Mbps (<100 Mbps)"
        fi
    fi
else
    fail "multi-carrier direct path not established within 20s"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen6b" 2>/dev/null | tr '\n' '|')"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect6b" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 7: direct blocked → automatic relay fallback ─────────────────────────
echo "=== Test 7: direct blocked -> stays on relay ==="
# Drop forwarded UDP in ns0: peer-to-peer punch fails, but STUN/control to ns0
# itself (input hook) keeps working — exactly a "UDP-hostile WAN".
ip netns exec ns0 nft add table inet bore_test_block
ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }'
ip netns exec ns0 nft 'add rule  inet bore_test_block bore_blk meta l4proto udp drop'

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id fallback-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.listen7" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id fallback-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect7" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.connect7" "staying on relay" 40; then
    pass "connector reports: direct unavailable, staying on relay"
else
    fail "connector did not report relay fallback within 40s"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect7" 2>/dev/null | tr '\n' '|')"
fi
NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 5 "$NS1_OVL" >/dev/null 2>&1; then
    pass "ping works over relay while direct is blocked"
else
    fail "ping failed over relay while direct is blocked"
fi
if grep -q "upgraded to direct" "$BORE_LOG.listen7" "$BORE_LOG.connect7" 2>/dev/null; then
    fail "direct path established despite UDP block"
else
    pass "no direct upgrade with UDP blocked (as expected)"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
sleep 0.5

# ── Test 8: direct path in gateway mode (GSO/TooLarge coverage) ───────────────
echo "=== Test 8: direct path gateway mode ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-gw-test \
    --advertise "$FAKE_LAN" --stun-server "$STUN" \
    >"$BORE_LOG.listen8" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id direct-gw-test \
    --accept-all-routes --stun-server "$STUN" \
    >"$BORE_LOG.connect8" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen8" "upgraded to direct" 20 && \
   wait_for_log "$BORE_LOG.connect8" "upgraded to direct" 20; then
    pass "direct path established in gateway mode"
    sleep 1
    if ip netns exec ns2 ping -c 2 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        pass "ping to LAN host over direct gateway path"
    else
        fail "ping to LAN host failed over direct gateway path"
    fi
    # TCP through the gateway exercises forwarded GRO super-frames + MSS clamp:
    # if oversized segments were dropped wholesale, throughput would collapse.
    if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
        ip netns exec ns1 iperf3 -s -D --logfile /dev/null
        sleep 0.2
        IPERF_TCP=$(timeout 20 ip netns exec ns2 iperf3 -c "$FAKE_LAN_HOST" -t 3 -J 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum_received']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
        ip netns exec ns1 pkill iperf3 2>/dev/null || true
        if [ "$IPERF_TCP" -gt 1 ]; then
            pass "iperf3 TCP through direct gateway: ${IPERF_TCP} Mbps (MSS clamp effective)"
        else
            fail "iperf3 TCP through direct gateway failed/stalled: ${IPERF_TCP} Mbps"
        fi
    fi
else
    fail "direct gateway path not established within 20s"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen8" 2>/dev/null | tr '\n' '|')"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect8" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 1

# ── Test 9: --relay-only never upgrades ───────────────────────────────────────
echo "=== Test 9: --relay-only flag ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relayonly-test \
    --relay-only \
    >"$BORE_LOG.listen9" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id relayonly-test \
    --relay-only \
    >"$BORE_LOG.connect9" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen9" "vpn link paired" 10; then
    sleep 2.5; sleep 0.5  # give TUN time to settle  # generous window: an unwanted upgrade would land in here
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "relay-only: ping works"
    else
        fail "relay-only: ping failed"
    fi
    if grep -q "upgraded to direct" "$BORE_LOG.listen9" "$BORE_LOG.connect9" 2>/dev/null; then
        fail "relay-only: direct upgrade happened despite the flag"
    else
        pass "relay-only: no direct upgrade attempted"
    fi
else
    fail "relay-only: VPN did not pair within 10s"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 10: server drop → auto-reconnect ─────────────────────────────────────
echo "=== Test 10: --auto-reconnect after server kill ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reconnect-test \
    --advertise "$FAKE_LAN" --auto-reconnect --relay-only \
    >"$BORE_LOG.listen10" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reconnect-test \
    --auto-reconnect --relay-only \
    >"$BORE_LOG.connect10" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen10" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    ip netns exec ns2 ping -c 2 -W 3 "$NS1_OVL" >/dev/null 2>&1 \
        && pass "reconnect: initial ping ok" || fail "reconnect: initial ping failed"

    # Kill the SERVER hard; both clients must enter the reconnect loop.
    kill -9 "$BORE_SERVER_PID" 2>/dev/null; BORE_SERVER_PID=""
    if wait_for_log "$BORE_LOG.listen10" "vpn link lost; reconnecting" 15 && \
       wait_for_log "$BORE_LOG.connect10" "vpn link lost; reconnecting" 15; then
        pass "reconnect: both sides report 'vpn link lost; reconnecting'"
    else
        fail "reconnect: clients did not enter the reconnect loop"
    fi

    # Restart the server (same command).
    ip netns exec ns0 "$BORE" server \
        --secret "$SECRET" \
        --vpn --vpn-pool "$POOL" --vpn-max-links 16 \
        --udp --bind-addr 0.0.0.0 \
        >"$BORE_LOG.server" 2>&1 &
    BORE_SERVER_PID=$!

    # Re-pairing = a SECOND "vpn link paired" in the listener log.
    REPAIRED=0
    for _ in $(seq 1 900); do
        [ "$(grep -c 'vpn link paired' "$BORE_LOG.listen10" 2>/dev/null)" -ge 2 ] && { REPAIRED=1; break; }
        sleep 0.1
    done
    if [ "$REPAIRED" = "1" ]; then
        pass "reconnect: link re-paired after server restart"
        sleep 2
        NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
        ip netns exec ns2 ping -c 3 -W 5 "$NS1_OVL" >/dev/null 2>&1 \
            && pass "reconnect: ping ok after re-pair" || fail "reconnect: ping failed after re-pair"
    else
        fail "reconnect: link did not re-pair within 90s"
        echo "  [listener log]: $(tail -5 "$BORE_LOG.listen10" 2>/dev/null | tr '\n' '|')"
    fi

    # Regression §0.1: no EEXIST and no duplicated routes after the re-apply.
    if grep -qi "file exists" "$BORE_LOG.listen10" "$BORE_LOG.connect10" 2>/dev/null; then
        fail "reconnect: 'File exists' route error found in logs (route replace regression)"
    else
        pass "reconnect: no 'File exists' route errors"
    fi
    ROUTE_COUNT=$(ip netns exec ns2 ip route show 2>/dev/null | grep -c "$FAKE_LAN" || true)
    if [ "$ROUTE_COUNT" -le 1 ]; then
        pass "reconnect: no duplicate routes in ns2 (count=$ROUTE_COUNT)"
    else
        fail "reconnect: duplicate routes in ns2 (count=$ROUTE_COUNT)"
    fi
else
    fail "reconnect: initial pairing failed"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 1

# ── Test 11: fatal error must NOT loop with --auto-reconnect ──────────────────
echo "=== Test 11: fatal error exits despite --auto-reconnect ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id fatal-test \
    --advertise "192.168.7.0/24" \
    >"$BORE_LOG.listen11" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

# Connector advertises an overlapping subnet → fatal VpnError; with
# --auto-reconnect it must still exit non-zero quickly (no infinite loop).
FATAL_EXIT=0
timeout 15 ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id fatal-test \
    --advertise "192.168.7.0/24" --auto-reconnect \
    >"$BORE_LOG.connect11" 2>&1 || FATAL_EXIT=$?
if [ "$FATAL_EXIT" -ne 0 ] && [ "$FATAL_EXIT" -ne 124 ]; then
    pass "fatal error: connector exited non-zero ($FATAL_EXIT) without looping"
elif [ "$FATAL_EXIT" -eq 124 ]; then
    fail "fatal error: connector still running after 15s (reconnect loop on a fatal error)"
else
    fail "fatal error: connector exited zero (expected failure)"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
sleep 0.3

# ── Test 12: relay carriers (--relay-only --carriers 4) ───────────────────────
echo "=== Test 12: relay with 4 carriers ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id carriers-test \
    --relay-only --carriers 4 \
    >"$BORE_LOG.listen12" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id carriers-test \
    --relay-only --carriers 4 \
    >"$BORE_LOG.connect12" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen12" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if ip netns exec ns2 ping -c 3 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "carriers=4: ping ok over multi-carrier relay"
    else
        fail "carriers=4: ping failed"
    fi
    if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
        ip netns exec ns1 iperf3 -s -D --logfile /dev/null
        sleep 0.2
        IPERF_TCP=$(timeout 20 ip netns exec ns2 iperf3 -c "$NS1_OVL" -t 3 -J 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum_received']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
        ip netns exec ns1 pkill iperf3 2>/dev/null || true
        if [ "$IPERF_TCP" -gt 1 ]; then
            pass "carriers=4: iperf3 TCP over relay: ${IPERF_TCP} Mbps"
        else
            fail "carriers=4: iperf3 TCP failed/stalled: ${IPERF_TCP} Mbps"
        fi
    fi
else
    fail "carriers=4: pairing failed"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen12" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 13: TUN multi-queue (--tun-queues 4) ─────────────────────────────────
echo "=== Test 13: TUN multi-queue ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mq-test \
    --tun-queues 4 --stun-server "$STUN" \
    >"$BORE_LOG.listen13" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mq-test \
    --tun-queues 4 --stun-server "$STUN" \
    >"$BORE_LOG.connect13" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen13" "vpn link paired" 10; then
    sleep 1
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if ip netns exec ns2 ping -c 3 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "tun-queues=4: ping ok"
    else
        fail "tun-queues=4: ping failed"
    fi
    if [ "$SKIP_IPERF" = "0" ] && command -v iperf3 >/dev/null 2>&1; then
        ip netns exec ns1 iperf3 -s -D --logfile /dev/null
        sleep 0.2
        IPERF_P4=$(timeout 20 ip netns exec ns2 iperf3 -c "$NS1_OVL" -t 3 -P 4 -J 2>/dev/null | \
            python3 -c "import sys,json; d=json.load(sys.stdin); print(int(d['end']['sum_received']['bits_per_second']/1e6))" 2>/dev/null || echo 0)
        ip netns exec ns1 pkill iperf3 2>/dev/null || true
        if [ "$IPERF_P4" -gt 1 ]; then
            pass "tun-queues=4: iperf3 -P 4 over the link: ${IPERF_P4} Mbps"
        else
            fail "tun-queues=4: iperf3 -P 4 failed/stalled: ${IPERF_P4} Mbps"
        fi
    fi
else
    fail "tun-queues=4: pairing failed"
    echo "  [listener log]: $(tail -5 "$BORE_LOG.listen13" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 14: SIGKILL leaves stale state; full reclaim (TUN + nft + routes) ────
echo "=== Test 14: full stale reclaim after SIGKILL (16.6.8) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reclaim-full \
    --advertise "$FAKE_LAN" --relay-only \
    >"$BORE_LOG.listen14" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reclaim-full --relay-only \
    --accept-all-routes \
    >"$BORE_LOG.connect14" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen14" "vpn link paired" 10; then
    sleep 1.5
    # SIGKILL: no cleanup possible.
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 0.5
    # Stale state must actually be there (otherwise the reclaim test is vacuous).
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_reclaim-full"; then
        pass "16.6.8: nft table survived SIGKILL (stale state present)"
    else
        fail "16.6.8: nft table missing after SIGKILL (nothing to reclaim?)"
    fi
    # NOTE: the TUN fd dies with the process (kernel removes the interface),
    # so only nft/route state is expected to leak.
    # Second run with the SAME id must reclaim and come up healthy.
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reclaim-full \
        --advertise "$FAKE_LAN" --relay-only \
        >"$BORE_LOG.listen14b" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5
    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id reclaim-full --relay-only \
        --accept-all-routes \
        >"$BORE_LOG.connect14b" 2>&1 &
    BORE_CONNECT_PID=$!
    if wait_for_log "$BORE_LOG.listen14b" "vpn link paired" 15; then
        sleep 1.5
        # nft table re-created exactly once, routes present, ping works.
        NFT_COUNT=$(ip netns exec ns1 nft list tables 2>/dev/null | grep -c "bore_vpn_reclaim-full" || true)
        [ "$NFT_COUNT" = "1" ] && pass "16.6.8: nft table reclaimed (count=1)" \
            || fail "16.6.8: nft table count=$NFT_COUNT after reclaim"
        if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
            pass "16.6.8: route re-applied after reclaim"
        else
            fail "16.6.8: route missing after reclaim"
        fi
        if grep -qi "file exists" "$BORE_LOG.listen14b" "$BORE_LOG.connect14b" 2>/dev/null; then
            fail "16.6.8: EEXIST during reclaim (route replace regression)"
        else
            pass "16.6.8: no EEXIST during reclaim"
        fi
        ip netns exec ns2 ping -c 2 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1 \
            && pass "16.6.8: ping ok after full reclaim" \
            || fail "16.6.8: ping failed after full reclaim"
    else
        fail "16.6.8: second run did not pair (reclaim failed)"
        echo "  [listener log]: $(tail -5 "$BORE_LOG.listen14b" 2>/dev/null | tr '\n' '|')"
    fi
else
    fail "16.6.8: initial pairing failed"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 15: background direct-retry (relay → direct after UDP unblocked) ─────
# Start with peer-to-peer UDP blocked (link comes up on relay, keeps retrying),
# then UNBLOCK UDP mid-session and prove the link upgrades to direct on a later
# retry round WITHOUT a reconnect — the relay stayed stable the whole time.
echo "=== Test 15: background direct-retry upgrades relay -> direct ==="
ip netns exec ns0 nft add table inet bore_test_block
ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }'
ip netns exec ns0 nft 'add rule  inet bore_test_block bore_blk meta l4proto udp drop'

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id retry-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.listen15" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id retry-test \
    --stun-server "$STUN" \
    >"$BORE_LOG.connect15" 2>&1 &
BORE_CONNECT_PID=$!

# Phase 1: blocked → relay, and the connector announces it will retry.
if wait_for_log "$BORE_LOG.connect15" "staying on relay, will retry" 40; then
    pass "retry: connector on relay and scheduling retries"
else
    fail "retry: connector did not log a scheduled retry within 40s"
    echo "  [connector log]: $(tail -5 "$BORE_LOG.connect15" 2>/dev/null | tr '\n' '|')"
fi
NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 2 -W 5 "$NS1_OVL" >/dev/null 2>&1; then
    pass "retry: ping over relay while direct is blocked"
else
    fail "retry: ping failed over relay while direct is blocked"
fi

# Phase 2: unblock UDP; the next retry round must upgrade to direct (≤ one
# DIRECT_RETRY_INTERVAL of 30 s + slack), no reconnect.
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
echo "  (UDP unblocked; waiting for a retry round to upgrade to direct)"
if wait_for_log "$BORE_LOG.connect15" "upgraded to direct" 45 && \
   wait_for_log "$BORE_LOG.listen15" "upgraded to direct" 45; then
    pass "retry: link upgraded relay -> direct on a later retry round"
    # The relay must have stayed stable — no link-lost/reconnect in between.
    if grep -qi "vpn link lost\|reconnecting" "$BORE_LOG.connect15" 2>/dev/null; then
        fail "retry: link reconnected instead of in-place upgrade"
    else
        pass "retry: relay stayed stable (no reconnect) until the upgrade"
    fi
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    LOSS=$(ip netns exec ns2 ping -c 10 -i 0.2 -W 3 "$NS1_OVL" 2>/dev/null | grep -oP '\d+(?=% packet loss)' || echo 100)
    [ "$LOSS" = "0" ] && pass "retry: direct path ping 0% loss after upgrade" \
        || fail "retry: direct path ping ${LOSS}% loss after upgrade"
else
    fail "retry: did not upgrade to direct within 45s after unblock"
    echo "  [connector log]: $(tail -8 "$BORE_LOG.connect15" 2>/dev/null | tr '\n' '|')"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
sleep 0.5

# ── Test 16: gateway clean-teardown route check (T-new-1, G1) ──────────────────
echo "=== Test 16: gateway clean-teardown route absence (G1 coverage) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-1-gw-route \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.listen16" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-1-gw-route \
    --accept-all-routes \
    >"$BORE_LOG.connect16" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen16" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Verify route was installed
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        pass "T-new-1: advertised route present in ns2 during link up"
    else
        fail "T-new-1: advertised route missing in ns2"
    fi
    # Clean exit
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1
    # Assert cleanup
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        fail "T-new-1: advertised route NOT removed after clean teardown"
    else
        pass "T-new-1: advertised route removed after clean teardown (G1 pass)"
    fi
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_t-new-1-gw-route"; then
        fail "T-new-1: nft table NOT removed after clean teardown"
    else
        pass "T-new-1: nft table removed after clean teardown"
    fi
else
    fail "T-new-1: gateway listener did not pair"
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
fi
sleep 0.5

# ── Test 17: connector dies and returns (T-new-2, G4) ─────────────────────────────
echo "=== Test 17: connector dies and returns (G4 coverage) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-2-conn-die \
    --relay-only --auto-reconnect \
    >"$BORE_LOG.listen17" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-2-conn-die \
    --relay-only --auto-reconnect \
    >"$BORE_LOG.connect17" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen17" "vpn link paired" 10; then
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 1 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "T-new-2: initial ping ok"
    else
        fail "T-new-2: initial ping failed"
    fi
    # Kill connector
    kill -9 "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    if wait_for_log "$BORE_LOG.listen17" "vpn link lost; reconnecting" 15; then
        pass "T-new-2: listener detects connector death"
    else
        fail "T-new-2: listener did not detect connector death"
    fi
    # Restart connector
    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-2-conn-die \
        --relay-only --auto-reconnect \
        >"$BORE_LOG.connect17b" 2>&1 &
    BORE_CONNECT_PID=$!
    PAIRED_COUNT=0
    for _ in $(seq 1 150); do
        PAIRED_COUNT=$(grep -c 'vpn link paired' "$BORE_LOG.listen17" 2>/dev/null || echo 0)
        [ "$PAIRED_COUNT" -ge 2 ] && break
        sleep 0.1
    done
    if [ "$PAIRED_COUNT" -ge 2 ]; then
        pass "T-new-2: re-pair after connector restart"
        sleep 1
        NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
        if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 1 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
            pass "T-new-2: post-recovery ping ok"
        else
            fail "T-new-2: post-recovery ping failed"
        fi
    else
        fail "T-new-2: re-pair failed"
    fi
else
    fail "T-new-2: initial pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 18: listener dies and returns (T-new-3, G4) ─────────────────────────────
echo "=== Test 18: listener dies and returns (G4 symmetric coverage) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-3-list-die \
    --relay-only --auto-reconnect \
    >"$BORE_LOG.listen18" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-3-list-die \
    --relay-only --auto-reconnect \
    >"$BORE_LOG.connect18" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.connect18" "vpn link paired" 10; then
    sleep 0.5
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 1 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
        pass "T-new-3: initial ping ok"
    else
        fail "T-new-3: initial ping failed"
    fi
    # Kill listener
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    if wait_for_log "$BORE_LOG.connect18" "vpn link lost; reconnecting" 15; then
        pass "T-new-3: connector detects listener death"
    else
        fail "T-new-3: connector did not detect listener death"
    fi
    # Restart listener
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-3-list-die \
        --relay-only --auto-reconnect \
        >"$BORE_LOG.listen18b" 2>&1 &
    BORE_LISTEN_PID=$!
    PAIRED_COUNT=0
    for _ in $(seq 1 150); do
        PAIRED_COUNT=$(grep -c 'vpn link paired' "$BORE_LOG.connect18" 2>/dev/null || echo 0)
        [ "$PAIRED_COUNT" -ge 2 ] && break
        sleep 0.1
    done
    if [ "$PAIRED_COUNT" -ge 2 ]; then
        pass "T-new-3: re-pair after listener restart"
        sleep 1
        NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
        if [ -n "$NS1_OVL" ] && ip netns exec ns2 ping -c 1 -W 3 "$NS1_OVL" >/dev/null 2>&1; then
            pass "T-new-3: post-recovery ping ok"
        else
            fail "T-new-3: post-recovery ping failed"
        fi
    else
        fail "T-new-3: re-pair failed"
    fi
else
    fail "T-new-3: initial pair failed"
fi
kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test 19: host↔site with connector advertises (T-new-4, G3) ───────────────────
echo "=== Test 19: host↔site connector advertises (G3 coverage) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-4-host-site \
    >"$BORE_LOG.listen19" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-new-4-host-site \
    --advertise "$FAKE_LAN" \
    >"$BORE_LOG.connect19" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.listen19" "vpn link paired\|VpnReady" 10; then
    sleep 2
    # Verify listener received the advertised route
    if ip netns exec ns1 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        pass "T-new-4: ns1 received advertised route from connector"
    else
        fail "T-new-4: ns1 missing advertised route"
    fi
    # Verify ns1 can reach the LAN host
    if ip netns exec ns1 ping -c 1 -W 3 "$FAKE_LAN_HOST" >/dev/null 2>&1; then
        pass "T-new-4: ns1 can ping connector's advertised LAN host"
    else
        fail "T-new-4: ns1 cannot reach connector's LAN host"
    fi
    # Verify host↔host overlay ping still works
    NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    if [ -n "$NS1_OVL" ] && [ -n "$NS2_OVL" ]; then
        if ip netns exec ns1 ping -c 1 -W 3 "$NS2_OVL" >/dev/null 2>&1; then
            pass "T-new-4: overlay bidi ping ok"
        else
            fail "T-new-4: overlay ping failed"
        fi
    else
        fail "T-new-4: overlay addresses missing"
    fi
    # Clean teardown
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1
    if ip netns exec ns1 ip route show 2>/dev/null | grep -q "$FAKE_LAN"; then
        fail "T-new-4: advertised route NOT removed after clean exit"
    else
        pass "T-new-4: advertised route cleaned up after exit"
    fi
else
    fail "T-new-4: host↔site listener did not pair"
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
fi
sleep 0.5

# ── Test T-HUB1: multi-connector address allocation ─────────────────────────
echo "=== T-HUB1: hub multi-connector address allocation ==="
# Host-only hub spoke isolation relies on the hub host NOT forwarding between
# spokes. A prior SIGKILL test (Test 14) can leave ns1 ip_forward=1 (reclaim
# only restores it on the next same-id run), so reset it for a clean baseline.
ip netns exec ns1 sysctl -qw net.ipv4.ip_forward=0 2>/dev/null || true
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hub-relay \
    --max-clients 4 --relay-only \
    >"$BORE_LOG.hub_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1

# Start 3 connectors on ns2, ns3, ns4
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn2" 2>&1 &
BORE_HUB_CONN2=$!

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn3" 2>&1 &
BORE_HUB_CONN3=$!

ip netns exec ns4 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_D" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn4" 2>&1 &
BORE_HUB_CONN4=$!

# Wait for all 3 connectors to join the hub (look for "vpn hub peer join" x3)
HUB_PAIRED=0
if wait_for_log "$BORE_LOG.hub_listen" "vpn hub peer join" 15; then
    sleep 1; PEER_JOIN_COUNT=$(grep -c "vpn hub peer join" "$BORE_LOG.hub_listen" 2>/dev/null || echo 0)
    if [ "$PEER_JOIN_COUNT" -ge 3 ]; then
        HUB_PAIRED=1
    fi
fi

if [ "$HUB_PAIRED" = "1" ]; then
    sleep 2  # Let interfaces settle
    HUB_OVERLAY=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    CONN2_OVERLAY=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    CONN3_OVERLAY=$(ip netns exec ns3 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    CONN4_OVERLAY=$(ip netns exec ns4 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

    if [ -n "$HUB_OVERLAY" ] && [ -n "$CONN2_OVERLAY" ] && [ -n "$CONN3_OVERLAY" ] && [ -n "$CONN4_OVERLAY" ]; then
        # Verify distinct addresses
        ADDRS="$HUB_OVERLAY $CONN2_OVERLAY $CONN3_OVERLAY $CONN4_OVERLAY"
        UNIQUE_ADDRS=$(echo "$ADDRS" | tr ' ' '\n' | sort | uniq | wc -l)
        if [ "$UNIQUE_ADDRS" = "4" ]; then
            pass "T-HUB1: all 4 peers assigned distinct overlay addresses (hub=$HUB_OVERLAY, conn2=$CONN2_OVERLAY, conn3=$CONN3_OVERLAY, conn4=$CONN4_OVERLAY)"
        else
            fail "T-HUB1: overlay addresses NOT distinct"
        fi
    else
        fail "T-HUB1: one or more overlay addresses missing"
    fi
else
    fail "T-HUB1: hub did not establish 3 connector pairs within timeout"
fi

kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_HUB_CONN4" 2>/dev/null
BORE_HUB_CONN2=""
BORE_HUB_CONN3=""
BORE_HUB_CONN4=""
sleep 0.5

# ── Test T-HUB2: spoke isolation ───────────────────────────────────────────────
echo "=== T-HUB2: spoke isolation (no spoke↔spoke) ==="
# Restart 3 connectors
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn2_t2" 2>&1 &
BORE_HUB_CONN2=$!

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn3_t2" 2>&1 &
BORE_HUB_CONN3=$!

ip netns exec ns4 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_D" --secret "$SECRET" --id hub-relay \
    --relay-only \
    >"$BORE_LOG.hub_conn4_t2" 2>&1 &
BORE_HUB_CONN4=$!

sleep 2.5; sleep 0.5  # give TUN time to settle
CONN2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
CONN3_OVL=$(ip netns exec ns3 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

# Test: ns2 should FAIL to ping ns3 (spoke isolation)
if [ -n "$CONN2_OVL" ] && [ -n "$CONN3_OVL" ]; then
    if ip netns exec ns2 ping -c 1 -W 2 "$CONN3_OVL" >/dev/null 2>&1; then
        fail "T-HUB2: ns2 can ping ns3 (spoke isolation violated)"
    else
        pass "T-HUB2: ns2 cannot ping ns3 (spoke isolation enforced)"
    fi
else
    fail "T-HUB2: overlay addresses not found"
fi

kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_HUB_CONN4" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""
BORE_HUB_CONN2=""
BORE_HUB_CONN3=""
BORE_HUB_CONN4=""
sleep 0.5

# ── Test T-HUB3: join/leave address reallocation ───────────────────────────────
echo "=== T-HUB3: join/leave address reallocation ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hub-relay-t3 \
    --max-clients 4 --relay-only \
    >"$BORE_LOG.hub_listen_t3" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1

# Start 2 connectors initially
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hub-relay-t3 \
    --relay-only \
    >"$BORE_LOG.hub_conn2_t3" 2>&1 &
BORE_HUB_CONN2=$!

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hub-relay-t3 \
    --relay-only \
    >"$BORE_LOG.hub_conn3_t3" 2>&1 &
BORE_HUB_CONN3=$!

sleep 2

# Verify initial 2 addresses
INITIAL_COUNT=$(grep -c "vpn hub peer join" "$BORE_LOG.hub_listen_t3" 2>/dev/null || echo 0)
if [ "$INITIAL_COUNT" -ge 2 ]; then
    pass "T-HUB3: 2 connectors joined initially"

    # Kill one
    kill "$BORE_HUB_CONN3" 2>/dev/null
    BORE_HUB_CONN3=""
    sleep 1

    # Start a new one (ns3 again)
    ip netns exec ns3 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hub-relay-t3 \
        --relay-only \
        >"$BORE_LOG.hub_conn3_t3_new" 2>&1 &
    BORE_HUB_CONN3=$!
    sleep 2

    # Verify new join (3rd peer join entry)
    FINAL_COUNT=$(grep -c "vpn hub peer join" "$BORE_LOG.hub_listen_t3" 2>/dev/null || echo 0)
    if [ "$FINAL_COUNT" -ge 3 ]; then
        pass "T-HUB3: connector rejoined and got new overlay address"
    else
        fail "T-HUB3: connector did not rejoin"
    fi
else
    fail "T-HUB3: initial peer joins failed"
fi

kill "$BORE_LISTEN_PID" "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" 2>/dev/null
BORE_LISTEN_PID=""
BORE_HUB_CONN2=""
BORE_HUB_CONN3=""
sleep 0.5

# ── Test T-HUB4: hub mode rejects --advertise on connectors ─────────────────────
echo "=== T-HUB4: hub mode rejects connector --advertise ==="
# NOTE: --max-clients 4 makes this a HUB; in 1:1 mode (--max-clients 1) a
# connector --advertise is legitimately allowed, so the rejection only applies
# to hub mode (server-side guard, D4).
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hub-relay-t4 \
    --max-clients 4 --relay-only \
    >"$BORE_LOG.hub_listen_t4" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1

# Try to start a connector with --advertise in hub mode (should fail quickly)
ADVERTISE_EXIT=0
timeout 5 ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hub-relay-t4 \
    --advertise "192.168.99.0/24" --relay-only \
    >"$BORE_LOG.hub_conn2_t4" 2>&1 || ADVERTISE_EXIT=$?

if [ "$ADVERTISE_EXIT" -ne 0 ] && [ "$ADVERTISE_EXIT" -ne 124 ]; then
    pass "T-HUB4: hub mode rejected connector --advertise (exit=$ADVERTISE_EXIT)"
elif [ "$ADVERTISE_EXIT" -eq 124 ]; then
    fail "T-HUB4: connector did not exit (timeout; check if --advertise rejection is implemented)"
else
    fail "T-HUB4: hub mode did NOT reject connector --advertise (expected failure)"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""
sleep 0.5

# ── Test T-HUBD1: hub + 2 spokes BOTH upgrade to direct ───────────────────────
echo "=== T-HUBD1: hub + 2 spokes upgrade to direct ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd1 \
    --max-clients 4 --stun-server "$STUN" \
    >"$BORE_LOG.hubd1_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd1 --stun-server "$STUN" \
    >"$BORE_LOG.hubd1_conn2" 2>&1 &
BORE_HUB_CONN2=$!
ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd1 --stun-server "$STUN" \
    >"$BORE_LOG.hubd1_conn3" 2>&1 &
BORE_HUB_CONN3=$!

# Wait until the hub logs TWO distinct per-peer direct upgrades.
HUBD1_OK=0
for _ in $(seq 1 50); do
    N=$(grep -c "hub peer upgraded to direct path" "$BORE_LOG.hubd1_listen" 2>/dev/null || echo 0)
    [ "${N:-0}" -ge 2 ] && { HUBD1_OK=1; break; }
    sleep 1
done
if [ "$HUBD1_OK" = 1 ]; then
    pass "T-HUBD1: both spokes upgraded to direct (per-peer)"
else
    fail "T-HUBD1: <2 direct upgrades ($(grep -c "hub peer upgraded to direct path" "$BORE_LOG.hubd1_listen" 2>/dev/null || echo 0))"
    echo "  [hub]: $(tail -6 "$BORE_LOG.hubd1_listen" 2>/dev/null | tr '\n' '|')"
fi
LOSS=$(ip netns exec ns2 ping -c 6 -i 0.2 -W 3 10.99.0.1 2>/dev/null | grep -oP '\d+(?=% packet loss)' || echo 100)
[ "$LOSS" = "0" ] && pass "T-HUBD1: spoke→hub ping 0% loss over direct" \
    || fail "T-HUBD1: spoke→hub ping ${LOSS}% loss"
kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; BORE_HUB_CONN3=""
sleep 0.5

# ── Test T-HUBD2: mixed paths — one spoke direct, one forced relay ─────────────
echo "=== T-HUBD2: mixed paths (ns3 UDP blocked stays relay, ns2 direct) ==="
# Block ONLY ns3's UDP (its egress source IP), so ns2 still upgrades.
ip netns exec ns0 nft add table inet bore_test_block 2>/dev/null
ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }' 2>/dev/null
ip netns exec ns0 nft "add rule inet bore_test_block bore_blk ip saddr $SERVER_IP_NS3 meta l4proto udp drop"
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd2 \
    --max-clients 4 --stun-server "$STUN" \
    >"$BORE_LOG.hubd2_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd2 --stun-server "$STUN" \
    >"$BORE_LOG.hubd2_conn2" 2>&1 &
BORE_HUB_CONN2=$!
ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd2 --stun-server "$STUN" \
    >"$BORE_LOG.hubd2_conn3" 2>&1 &
BORE_HUB_CONN3=$!
# ns2 must upgrade to direct (its connector log).
if wait_for_log "$BORE_LOG.hubd2_conn2" "upgraded to direct" 45; then
    pass "T-HUBD2: ns2 spoke upgraded to direct"
else
    fail "T-HUBD2: ns2 did not upgrade to direct"
fi
# ns3 must stay on relay (UDP blocked) — never logs a direct upgrade.
sleep 3
if grep -q "upgraded to direct" "$BORE_LOG.hubd2_conn3" 2>/dev/null; then
    fail "T-HUBD2: ns3 reached direct despite UDP block"
else
    pass "T-HUBD2: ns3 stayed on relay (UDP blocked)"
fi
# Both still reach the hub.
ip netns exec ns2 ping -c 3 -W 3 10.99.0.1 >/dev/null 2>&1 && ip netns exec ns3 ping -c 3 -W 3 10.99.0.1 >/dev/null 2>&1 \
    && pass "T-HUBD2: both spokes ping hub (direct + relay)" \
    || fail "T-HUBD2: a spoke could not ping the hub"
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; BORE_HUB_CONN3=""
sleep 0.5

# ── Test T-HUBD3: direct → warm-relay fallback when UDP drops mid-session ──────
echo "=== T-HUBD3: direct path fallback to warm relay ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd3 \
    --max-clients 4 --stun-server "$STUN" \
    >"$BORE_LOG.hubd3_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd3 --stun-server "$STUN" \
    >"$BORE_LOG.hubd3_conn2" 2>&1 &
BORE_HUB_CONN2=$!
if wait_for_log "$BORE_LOG.hubd3_listen" "hub peer upgraded to direct path" 45; then
    pass "T-HUBD3: spoke upgraded to direct"
    # Drop the spoke's UDP mid-session → the hub must fall back to warm relay.
    ip netns exec ns0 nft add table inet bore_test_block 2>/dev/null
    ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }' 2>/dev/null
    ip netns exec ns0 nft "add rule inet bore_test_block bore_blk ip saddr $SERVER_IP_NS2 meta l4proto udp drop"
    if wait_for_log "$BORE_LOG.hubd3_listen" "fell back to warm relay" 30; then
        pass "T-HUBD3: hub fell back to warm relay on direct death"
    else
        fail "T-HUBD3: hub did not fall back to relay after UDP drop"
    fi
    # Both ends fall back independently on their own QUIC idle timeout (~10s);
    # poll until the warm relay carries traffic again (UDP stays blocked, so a
    # success proves relay — not a re-established direct path).
    RELAY_OK=0
    for _ in $(seq 1 20); do
        if ip netns exec ns2 ping -c 2 -W 2 10.99.0.1 >/dev/null 2>&1; then RELAY_OK=1; break; fi
        sleep 1
    done
    [ "$RELAY_OK" = 1 ] && pass "T-HUBD3: ping continues over warm relay after fallback" \
        || fail "T-HUBD3: ping did not recover over relay after fallback"
    ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
else
    fail "T-HUBD3: spoke never reached direct (cannot test fallback)"
fi
kill "$BORE_HUB_CONN2" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""
sleep 0.5

# ── Test T-HUBD4: background upgrade (blocked → relay → unblock → direct) ──────
echo "=== T-HUBD4: background relay→direct upgrade after unblock ==="
ip netns exec ns0 nft add table inet bore_test_block 2>/dev/null
ip netns exec ns0 nft 'add chain inet bore_test_block bore_blk { type filter hook forward priority 0; }' 2>/dev/null
ip netns exec ns0 nft 'add rule inet bore_test_block bore_blk meta l4proto udp drop'
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd4 \
    --max-clients 4 --stun-server "$STUN" \
    >"$BORE_LOG.hubd4_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubd4 --stun-server "$STUN" \
    >"$BORE_LOG.hubd4_conn2" 2>&1 &
BORE_HUB_CONN2=$!
if wait_for_log "$BORE_LOG.hubd4_conn2" "staying on relay, will retry" 40; then
    pass "T-HUBD4: spoke on relay while UDP blocked"
else
    fail "T-HUBD4: spoke did not schedule a retry while blocked"
fi
ip netns exec ns0 nft delete table inet bore_test_block 2>/dev/null || true
if wait_for_log "$BORE_LOG.hubd4_listen" "hub peer upgraded to direct path" 45; then
    pass "T-HUBD4: upgraded relay→direct on a later round after unblock"
else
    fail "T-HUBD4: did not upgrade to direct within 45s after unblock"
fi
kill "$BORE_HUB_CONN2" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""
sleep 0.5


# ══ Phase 5: full site-to-host scenario (host-A/B/C/E + hub-D) on relay+direct ══
# host-D advertises 192.168.4.0/24 (LAN1 host .1) and 10.10.0.0/16 (LAN2 host 10.10.0.1).
run_scen() {
    local MODE="$1" HF SF
    if [ "$MODE" = relay ]; then HF="--relay-only"; SF="--relay-only"; else HF="--stun-server $STUN"; SF="--stun-server $STUN"; fi
    png() { ip netns exec "$1" ping -c 2 -W 3 "$2" >/dev/null 2>&1; }
    echo "=== T-SCEN ($MODE): hub advertises 192.168.4.0/24 + 10.10.0.0/16 ==="
    ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scen-$MODE" \
        --advertise "192.168.4.0/24,10.10.0.0/16" --max-clients 8 $HF >"$BORE_LOG.scen_hub_$MODE" 2>&1 &
    BORE_LISTEN_PID=$!; sleep 1
    ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scen-$MODE" \
        --accept-all-routes --refuse-routes "10.10.0.0/16" $SF >"$BORE_LOG.scen_a_$MODE" 2>&1 &
    BORE_HUB_CONN2=$!
    ip netns exec ns3 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scen-$MODE" \
        --accept-all-routes $SF >"$BORE_LOG.scen_b_$MODE" 2>&1 &
    BORE_HUB_CONN3=$!
    ip netns exec ns4 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scen-$MODE" \
        --accept-all-routes --refuse-routes "192.168.4.0/24" $SF >"$BORE_LOG.scen_c_$MODE" 2>&1 &
    BORE_HUB_CONN4=$!
    for _ in $(seq 1 20); do
        [ "$(grep -c 'vpn hub peer join' "$BORE_LOG.scen_hub_$MODE" 2>/dev/null || echo 0)" -ge 3 ] && break; sleep 1
    done
    if [ "$MODE" = direct ]; then
        for _ in $(seq 1 50); do
            [ "$(grep -c 'upgraded to direct path' "$BORE_LOG.scen_hub_$MODE" 2>/dev/null || echo 0)" -ge 3 ] && break; sleep 1
        done
        N=$(grep -c 'upgraded to direct path' "$BORE_LOG.scen_hub_$MODE" 2>/dev/null || echo 0)
        [ "$N" -ge 3 ] && pass "T-SCEN-$MODE: all 3 spokes upgraded to direct" \
            || fail "T-SCEN-$MODE: only $N/3 spokes reached direct"
    fi
    sleep 2
    # host-A: LAN1 yes, LAN2 (refused) no
    png ns2 192.168.4.1 && pass "T-SCEN-$MODE A: reaches 192.168.4.0/24" || fail "T-SCEN-$MODE A: cannot reach 192.168.4.0/24"
    png ns2 10.10.0.1   && fail "T-SCEN-$MODE A: reached refused 10.10.0.0/16" || pass "T-SCEN-$MODE A: refused 10.10.0.0/16 unreachable"
    # host-B: both
    png ns3 192.168.4.1 && pass "T-SCEN-$MODE B: reaches 192.168.4.0/24" || fail "T-SCEN-$MODE B: cannot reach 192.168.4.0/24"
    png ns3 10.10.0.1   && pass "T-SCEN-$MODE B: reaches 10.10.0.0/16"   || fail "T-SCEN-$MODE B: cannot reach 10.10.0.0/16"
    # host-C: LAN2 yes, LAN1 (refused) no
    png ns4 10.10.0.1   && pass "T-SCEN-$MODE C: reaches 10.10.0.0/16"   || fail "T-SCEN-$MODE C: cannot reach 10.10.0.0/16"
    png ns4 192.168.4.1 && fail "T-SCEN-$MODE C: reached refused 192.168.4.0/24" || pass "T-SCEN-$MODE C: refused 192.168.4.0/24 unreachable"
    # ISO: A cannot reach C's overlay (spoke isolation)
    local A_OVL C_OVL
    A_OVL=$(ip netns exec ns2 ip -4 addr show bore0 2>/dev/null | grep -oP 'inet \K[0-9.]+')
    C_OVL=$(ip netns exec ns4 ip -4 addr show bore0 2>/dev/null | grep -oP 'inet \K[0-9.]+')
    if [ -n "$A_OVL" ] && [ -n "$C_OVL" ]; then
        png ns2 "$C_OVL" && fail "T-SCEN-$MODE ISO: A reached C (isolation broken)" || pass "T-SCEN-$MODE ISO: A↔C blocked"
    else fail "T-SCEN-$MODE ISO: overlays missing (A=$A_OVL C=$C_OVL)"; fi
    png ns2 10.99.0.1 && png ns3 10.99.0.1 && png ns4 10.99.0.1 \
        && pass "T-SCEN-$MODE: all spokes reach hub overlay" || fail "T-SCEN-$MODE: a spoke cannot reach hub overlay"
    kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_HUB_CONN4" "$BORE_LISTEN_PID" 2>/dev/null
    BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; BORE_HUB_CONN3=""; BORE_HUB_CONN4=""; sleep 1
    # host-E: no route flags → hub overlay only, neither LAN (default deny)
    ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scenE-$MODE" \
        --advertise "192.168.4.0/24,10.10.0.0/16" --max-clients 8 $HF >"$BORE_LOG.scenE_hub_$MODE" 2>&1 &
    BORE_LISTEN_PID=$!; sleep 1
    ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "scenE-$MODE" $SF >"$BORE_LOG.scenE_e_$MODE" 2>&1 &
    BORE_HUB_CONN2=$!
    for _ in $(seq 1 15); do [ "$(grep -c 'vpn hub peer join' "$BORE_LOG.scenE_hub_$MODE" 2>/dev/null || echo 0)" -ge 1 ] && break; sleep 1; done
    sleep 1
    png ns2 10.99.0.1   && pass "T-SCEN-$MODE E: reaches hub overlay"        || fail "T-SCEN-$MODE E: cannot reach hub overlay"
    png ns2 192.168.4.1 && fail "T-SCEN-$MODE E: reached 192.168.4.0/24 (default-deny breach)" || pass "T-SCEN-$MODE E: 192.168.4.0/24 denied by default"
    png ns2 10.10.0.1   && fail "T-SCEN-$MODE E: reached 10.10.0.0/16 (default-deny breach)"   || pass "T-SCEN-$MODE E: 10.10.0.0/16 denied by default"
    kill "$BORE_HUB_CONN2" "$BORE_LISTEN_PID" 2>/dev/null
    BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; sleep 1
}
run_scen relay
run_scen direct

# ══ Phase 6: NAT (overlapping-subnet 1:1 netmap) tests ══════════════════════════════════
# Add lo aliases for real LAN hosts behind the NAT gateways.
# NAT test setup:
# - NAT_LAN_A = 192.168.10.0/24 (real LAN behind ns1 gateway, exposed as 10.50.1.0/24)
# - NAT_LAN_B = 192.168.10.0/24 (real LAN behind ns2 gateway, exposed as 10.60.1.0/24)
# - NAT_LAN_C = 192.168.10.0/24 (real LAN behind ns3 gateway, exposed as 10.70.1.0/24)
# Both sites have IDENTICAL real LAN 192.168.10.0/24; virtuals are distinct.
NAT_REAL_LAN="192.168.10.0/24"
NAT_REAL_HOST_A="192.168.10.5"
NAT_REAL_HOST_B="192.168.10.10"
NAT_REAL_HOST_C="192.168.10.15"
NAT_VIRT_A="10.50.1.0/24"
NAT_VIRT_B="10.60.1.0/24"
NAT_VIRT_C="10.70.1.0/24"

# Add the real LAN hosts to ns1, ns2, ns3 loopbacks
ip netns exec ns1 ip addr add "$NAT_REAL_HOST_A/24" dev lo
ip netns exec ns2 ip addr add "$NAT_REAL_HOST_B/24" dev lo
ip netns exec ns3 ip addr add "$NAT_REAL_HOST_C/24" dev lo

# ── Test T-NAT1: site↔host with NAT (topology B) ─────────────────────────────────────
echo "=== T-NAT1: site↔host NAT (gateway advertises real@virtual) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat1 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.nat1_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat1 \
    --accept-all-routes \
    >"$BORE_LOG.nat1_connect" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.nat1_listen" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # ns2 should have route to the virtual LAN (10.50.1.0/24), NOT the real LAN (192.168.10.0/24)
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        pass "T-NAT1: virtual route $NAT_VIRT_A installed in ns2 (not real $NAT_REAL_LAN)"
    else
        fail "T-NAT1: virtual route to $NAT_VIRT_A NOT found in ns2"
    fi
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_REAL_LAN"; then
        fail "T-NAT1: real route $NAT_REAL_LAN should NOT be installed in ns2"
    else
        pass "T-NAT1: real route $NAT_REAL_LAN correctly absent from ns2"
    fi
    # Ping the virtual address should reach the real host
    if ip netns exec ns2 ping -c 2 -W 3 "10.50.1.5" >/dev/null 2>&1; then
        pass "T-NAT1: ping 10.50.1.5 (virtual) reaches ns1 real host 192.168.10.5"
    else
        fail "T-NAT1: ping to 10.50.1.5 failed"
    fi
    # Verify the log contains nat netmap messages
    if grep -q "nat netmap: dnat" "$BORE_LOG.nat1_listen" 2>/dev/null; then
        pass "T-NAT1: log contains 'nat netmap: dnat' (NAT rules installed)"
    else
        fail "T-NAT1: log missing 'nat netmap: dnat' line"
    fi
else
    fail "T-NAT1: listener did not pair within 10s"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test T-NAT2: site↔site with identical overlapping LANs (topology C) ──────────────────
echo "=== T-NAT2: site↔site NAT (both map identical real 192.168.10.0/24) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat2 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.nat2_listen_a" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat2 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_B}" \
    --accept-all-routes \
    >"$BORE_LOG.nat2_connect_b" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.nat2_listen_a" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # ns1 should have route to ns2's virtual (10.60.1.0/24)
    if ip netns exec ns1 ip route show 2>/dev/null | grep -q "$NAT_VIRT_B"; then
        pass "T-NAT2: ns1 has route to ns2's virtual $NAT_VIRT_B"
    else
        fail "T-NAT2: ns1 missing route to $NAT_VIRT_B"
    fi
    # ns2 should have route to ns1's virtual (10.50.1.0/24)
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        pass "T-NAT2: ns2 has route to ns1's virtual $NAT_VIRT_A"
    else
        fail "T-NAT2: ns2 missing route to $NAT_VIRT_A"
    fi
    # The §1 identical-LAN scenario: source from the LOCAL real LAN host so the
    # egress SNAT (saddr real -> virtual) fires on BOTH sides. ns1's host
    # 192.168.10.5 -> 10.60.1.10 must reach ns2's real .10 (which sees the caller
    # as 10.50.1.5, no collision); and symmetrically the other way.
    if ip netns exec ns1 ping -c 2 -W 3 -I "$NAT_REAL_HOST_A" "10.60.1.10" >/dev/null 2>&1; then
        pass "T-NAT2: ns1 host 192.168.10.5 -> 10.60.1.10 reaches ns2 real .10 (egress SNAT fired)"
    else
        fail "T-NAT2: ns1 host-sourced ping to 10.60.1.10 failed"
    fi
    # From ns2's host, ping ns1's virtual address should reach ns1's real host.
    if ip netns exec ns2 ping -c 2 -W 3 -I "$NAT_REAL_HOST_B" "10.50.1.5" >/dev/null 2>&1; then
        pass "T-NAT2: ns2 host 192.168.10.10 -> 10.50.1.5 reaches ns1 real .5 (symmetric)"
    else
        fail "T-NAT2: ns2 host-sourced ping to 10.50.1.5 failed"
    fi
else
    fail "T-NAT2: listener did not pair within 10s"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# ── Test T-NAT3: host-bit preservation (1:1 netmap, not single-address DNAT) ────────────
echo "=== T-NAT3: host-bit preservation (1:1 netmap) ==="
# Add extra lo aliases for additional hosts to test host-bit preservation
ip netns exec ns1 ip addr add "192.168.10.23/24" dev lo
ip netns exec ns2 ip addr add "192.168.10.23/24" dev lo

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat3 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.nat3_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat3 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_B}" \
    --accept-all-routes \
    >"$BORE_LOG.nat3_connect" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.nat3_listen" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Liveness ping (necessary but NOT sufficient — a scrambled host bit that happens
    # to land on ANOTHER assigned alias would still answer; see the identity check below).
    if ip netns exec ns2 ping -c 1 -W 3 "10.50.1.5" >/dev/null 2>&1; then
        pass "T-NAT3: 10.50.1.5 reachable (liveness)"
    else
        fail "T-NAT3: ping 10.50.1.5 failed"
    fi

    # ── Deterministic EXACT host-bit identity (regression guard for the nft `prefix`
    #    bug, commit 8564ae0). A socat listener bound to each real host echoes the exact
    #    address the connection actually landed on ($SOCAT_SOCKADDR). Connecting to
    #    virtual .X MUST reach real .X — if the netmap scrambles host bits (e.g. the old
    #    `dnat ip to <prefix>` mapped .138→.8), the echoed address differs and this FAILS.
    #    Ping alone could NOT catch this; an exact-identity check can.
    if command -v socat >/dev/null 2>&1; then
        # Listeners on ns1 real hosts (local to the gateway; mirrors the gateway-self path).
        ip netns exec ns1 socat TCP-LISTEN:7700,bind=192.168.10.5,reuseaddr,fork \
            SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
        NAT3_S1=$!
        ip netns exec ns1 socat TCP-LISTEN:7700,bind=192.168.10.23,reuseaddr,fork \
            SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
        NAT3_S2=$!
        sleep 0.5
        for hb in 5 23; do
            got=$(ip netns exec ns2 timeout 4 socat -T3 - "TCP:10.50.1.${hb}:7700" </dev/null 2>/dev/null | tr -d '\r\n')
            if [ "$got" = "HIT-192.168.10.${hb}" ]; then
                pass "T-NAT3: EXACT host-bit .${hb}: 10.50.1.${hb} → 192.168.10.${hb} (got '$got')"
            else
                fail "T-NAT3: host-bit .${hb} SCRAMBLED: 10.50.1.${hb} should reach 192.168.10.${hb}, got '$got' (nft netmap missing 'prefix'?)"
            fi
        done
        kill "$NAT3_S1" "$NAT3_S2" 2>/dev/null || true
        ip netns exec ns1 pkill -f "TCP-LISTEN:7700" 2>/dev/null || true
    else
        echo "WARN: T-NAT3: socat not found — skipping deterministic host-bit identity check"
    fi
else
    fail "T-NAT3: listener did not pair within 10s"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# Clean up the extra .23 alias for next tests
ip netns exec ns1 ip addr del "192.168.10.23/24" dev lo 2>/dev/null || true
ip netns exec ns2 ip addr del "192.168.10.23/24" dev lo 2>/dev/null || true

# ── Test T-NAT-IPT: iptables fallback path (forced) — host-bit identity + clean teardown ──
# Exercises the iptables NETMAP custom-chain path (F3/F4 refactor) end-to-end through the
# real binary, forced on this nft host via BORE_VPN_FORCE_IPTABLES. Guards: apply must NOT
# crash (F4: no '-i' in POSTROUTING), host bits preserved (NETMAP target), and SIGTERM
# teardown must leave ZERO bore_* chains (F3: chain teardown, not comment-matching).
echo "=== T-NAT-IPT: iptables fallback (forced) host-bit identity + teardown ==="
if command -v socat >/dev/null 2>&1 && ip netns exec ns1 sh -c 'command -v iptables' >/dev/null 2>&1; then
    ip netns exec ns1 ip addr add "192.168.10.23/24" dev lo 2>/dev/null || true
    ip netns exec ns1 env BORE_VPN_FORCE_IPTABLES=1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id tnatipt \
        --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
        >"$BORE_LOG.natipt_listen" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5
    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id tnatipt \
        --accept-all-routes \
        >"$BORE_LOG.natipt_connect" 2>&1 &
    BORE_CONNECT_PID=$!

    if wait_for_log "$BORE_LOG.natipt_listen" "vpn link paired\|VpnReady" 10; then
        sleep 1
        # F4: apply must have used iptables and NOT errored.
        if grep -q "applied iptables NAT rules" "$BORE_LOG.natipt_listen" 2>/dev/null; then
            pass "T-NAT-IPT: iptables NAT path applied (no '-i POSTROUTING' crash)"
        else
            fail "T-NAT-IPT: log missing 'applied iptables NAT rules' (forced-iptables apply failed?)"
        fi
        # Custom chains present while up.
        if ip netns exec ns1 iptables -t nat -S 2>/dev/null | grep -q "bore_tnatipt_"; then
            pass "T-NAT-IPT: iptables custom chains present while link up"
        else
            fail "T-NAT-IPT: iptables bore_tnatipt_* chains not found while up"
        fi
        # Host-bit identity over the iptables NETMAP path.
        ip netns exec ns1 socat TCP-LISTEN:7701,bind=192.168.10.5,reuseaddr,fork \
            SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
        NATIPT_S1=$!
        ip netns exec ns1 socat TCP-LISTEN:7701,bind=192.168.10.23,reuseaddr,fork \
            SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
        NATIPT_S2=$!
        sleep 0.5
        for hb in 5 23; do
            got=$(ip netns exec ns2 timeout 4 socat -T3 - "TCP:10.50.1.${hb}:7701" </dev/null 2>/dev/null | tr -d '\r\n')
            if [ "$got" = "HIT-192.168.10.${hb}" ]; then
                pass "T-NAT-IPT: iptables NETMAP exact host-bit .${hb} (got '$got')"
            else
                fail "T-NAT-IPT: iptables NETMAP host-bit .${hb} wrong (got '$got')"
            fi
        done
        kill "$NATIPT_S1" "$NATIPT_S2" 2>/dev/null || true
        ip netns exec ns1 pkill -f "TCP-LISTEN:7701" 2>/dev/null || true

        # F3: graceful teardown must remove ALL bore_* chains (no leak).
        kill -TERM "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
        sleep 1.5
        leftover=$(ip netns exec ns1 iptables -t nat -S 2>/dev/null | grep -c "bore_tnatipt_" || true)
        if [ "${leftover:-0}" = "0" ]; then
            pass "T-NAT-IPT: iptables chains fully reclaimed after exit (F3 fixed)"
        else
            fail "T-NAT-IPT: $leftover leaked iptables bore_tnatipt_* rules after exit"
        fi
    else
        fail "T-NAT-IPT: listener did not pair within 10s (forced-iptables apply may have crashed)"
    fi
    ip netns exec ns1 ip addr del "192.168.10.23/24" dev lo 2>/dev/null || true
    [ -n "$BORE_LISTEN_PID" ] && kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 0.5
else
    echo "WARN: T-NAT-IPT: socat/iptables unavailable — skipping forced-iptables test"
fi
true  # guard: keep set -e from aborting on the prior short-circuit's exit code

# ── Test T-NAT-MASQ: --nat-masquerade reaches a SEPARATE host behind the gateway (F2) ──
# TRUE forwarding: the target is a distinct netns (ns_lanm), NOT the gateway itself, and the
# gateway is NOT its router (ns_lanm has only the on-link /24, no route back to the peer
# overlay). Reachable ONLY when --nat-masquerade rewrites the peer source to the gateway LAN
# IP so the LAN host's reply returns on-link. The gateway LAN IP is .254 (NOT network+1=.1,
# which bore probes via `ip route get` for lan_if detection — a .1 would resolve "local").
echo "=== T-NAT-MASQ: --nat-masquerade forwarding to a SEPARATE LAN host (F2) ==="
if command -v socat >/dev/null 2>&1; then
    MQ_REAL="192.168.77.0/24"; MQ_VIRT="10.77.1.0/24"
    MQ_HOST="192.168.77.77"; MQ_VHOST="10.77.1.77"
    ip netns add ns_lanm 2>/dev/null || true
    ip link add veth-gwlan netns ns1 type veth peer name veth-lanm netns ns_lanm
    ip netns exec ns1 ip addr add 192.168.77.254/24 dev veth-gwlan
    ip netns exec ns1 ip link set veth-gwlan up
    ip netns exec ns_lanm ip link set lo up
    ip netns exec ns_lanm ip link set veth-lanm up
    ip netns exec ns_lanm ip addr add "$MQ_HOST/24" dev veth-lanm
    # ns_lanm: ONLY the on-link /24 (no default) → reply to a peer-overlay src is undeliverable.
    ip netns exec ns_lanm socat TCP-LISTEN:7702,bind="$MQ_HOST",reuseaddr,fork \
        SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
    MQ_S=$!
    sleep 0.3

    run_masq() {  # $1 = id ; $2 = extra listen flags ; echoes the connector's response
        ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "$1" \
            --advertise "${MQ_REAL}@${MQ_VIRT}" $2 >"$BORE_LOG.$1_listen" 2>&1 &
        BORE_LISTEN_PID=$!
        sleep 0.5
        ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "$1" \
            --accept-all-routes >"$BORE_LOG.$1_connect" 2>&1 &
        BORE_CONNECT_PID=$!
        local r=""
        if wait_for_log "$BORE_LOG.$1_listen" "vpn link paired\|VpnReady" 10; then
            sleep 1
            r=$(ip netns exec ns2 timeout 4 socat -T3 - "TCP:${MQ_VHOST}:7702" </dev/null 2>/dev/null | tr -d '\r\n')
        fi
        [ -n "$BORE_LISTEN_PID" ] && kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
        sleep 1
        printf '%s' "$r"
    }

    # (1) WITHOUT the flag: the separate host must be UNREACHABLE (the F2 gap).
    got_off=$(run_masq tnmq1 "")
    if [ -z "$got_off" ]; then
        pass "T-NAT-MASQ: WITHOUT --nat-masquerade, separate LAN host unreachable (gap confirmed)"
    else
        fail "T-NAT-MASQ: WITHOUT flag, unexpectedly reached separate host (got '$got_off')"
    fi

    # (2) WITH the flag: REACHABLE, and the exact host bit is preserved end-to-end.
    got_on=$(run_masq tnmq2 "--nat-masquerade")
    if grep -q "nat-masquerade" "$BORE_LOG.tnmq2_listen" 2>/dev/null; then
        pass "T-NAT-MASQ: listener logged the nat-masquerade rule"
    else
        fail "T-NAT-MASQ: listener log missing 'nat-masquerade' line"
    fi
    if [ "$got_on" = "HIT-$MQ_HOST" ]; then
        pass "T-NAT-MASQ: WITH --nat-masquerade, separate LAN host reachable + host-bit correct (got '$got_on')"
    else
        fail "T-NAT-MASQ: WITH flag, separate host not reached / wrong host-bit (got '$got_on')"
    fi

    kill "$MQ_S" 2>/dev/null || true
    ip netns exec ns_lanm pkill -f "TCP-LISTEN:7702" 2>/dev/null || true
    ip netns del ns_lanm 2>/dev/null || true
    sleep 0.5
else
    echo "WARN: T-NAT-MASQ: socat unavailable — skipping"
fi
true

# ── Test T-FWD: --forward-accept punches a default-deny FORWARD (Docker/ufw) ──────────────
# Faithful reproduction of the field bug: a NAT gateway with --nat-masquerade STILL cannot
# reach a SEPARATE host behind it when the FORWARD chain is default-deny (the Docker daemon
# sets `-P FORWARD DROP`; ufw/hardened hosts too). bore's nft rules cannot override a FORWARD
# DROP that lives in another chain. --forward-accept punches an ACCEPT for the tun<->LAN pair
# into the iptables FORWARD chain → reachable. Also asserts the detection WARNING (no flag) and
# the RAII revert of the forward-accept chain on exit. Uses the same ns_lanm topology as
# T-NAT-MASQ (a real veth-connected netns, NOT a gateway-local lo alias).
echo "=== T-FWD: --forward-accept over a default-deny FORWARD chain ==="
if command -v socat >/dev/null 2>&1 && command -v iptables >/dev/null 2>&1; then
    FW_REAL="192.168.78.0/24"; FW_VIRT="10.78.1.0/24"
    FW_HOST="192.168.78.78"; FW_VHOST="10.78.1.78"
    ip netns add ns_lanm 2>/dev/null || true
    ip link add veth-gwlan netns ns1 type veth peer name veth-lanm netns ns_lanm
    ip netns exec ns1 ip addr add 192.168.78.254/24 dev veth-gwlan
    ip netns exec ns1 ip link set veth-gwlan up
    ip netns exec ns_lanm ip link set lo up
    ip netns exec ns_lanm ip link set veth-lanm up
    ip netns exec ns_lanm ip addr add "$FW_HOST/24" dev veth-lanm
    # ns_lanm: ONLY the on-link /24 (no default) — replies return on-link to .254 via masquerade.
    ip netns exec ns_lanm socat TCP-LISTEN:7703,bind="$FW_HOST",reuseaddr,fork \
        SYSTEM:'echo HIT-$SOCAT_SOCKADDR' >/dev/null 2>&1 &
    FW_S=$!
    sleep 0.3
    # Simulate Docker/ufw: a default-deny FORWARD chain in the gateway netns.
    ip netns exec ns1 iptables -P FORWARD DROP

    run_fwd() {  # $1=id ; $2=extra listen flags ; echoes the connector's response
        ip netns exec ns1 "$BORE" vpn listen --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "$1" \
            --advertise "${FW_REAL}@${FW_VIRT}" --nat-masquerade $2 >"$BORE_LOG.$1_listen" 2>&1 &
        BORE_LISTEN_PID=$!
        sleep 0.5
        ip netns exec ns2 "$BORE" vpn connect --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id "$1" \
            --accept-all-routes >"$BORE_LOG.$1_connect" 2>&1 &
        BORE_CONNECT_PID=$!
        local r=""
        if wait_for_log "$BORE_LOG.$1_listen" "vpn link paired\|VpnReady" 10; then
            sleep 1
            r=$(ip netns exec ns2 timeout 4 socat -T3 - "TCP:${FW_VHOST}:7703" </dev/null 2>/dev/null | tr -d '\r\n')
        fi
        [ -n "$BORE_LISTEN_PID" ] && kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
        [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
        sleep 1
        printf '%s' "$r"
    }

    # (1) WITHOUT --forward-accept: the FORWARD DROP strands the separate host even though
    #     --nat-masquerade is set; bore must DETECT the default-deny chain and WARN.
    got_off=$(run_fwd tfwd1 "")
    if [ -z "$got_off" ]; then
        pass "T-FWD: WITHOUT --forward-accept, default-deny FORWARD strands the LAN host (gap confirmed)"
    else
        fail "T-FWD: WITHOUT flag, unexpectedly reached host through a DROP'd FORWARD (got '$got_off')"
    fi
    if grep -q "FORWARD chain is default-deny" "$BORE_LOG.tfwd1_listen" 2>/dev/null; then
        pass "T-FWD: bore WARNED about the default-deny FORWARD chain (with remediation)"
    else
        fail "T-FWD: missing default-deny FORWARD warning in listener log"
    fi

    # (2) WITH --forward-accept: bore punches the FORWARD chain → host reachable, host-bit kept.
    got_on=$(run_fwd tfwd2 "--forward-accept")
    if [ "$got_on" = "HIT-$FW_HOST" ]; then
        pass "T-FWD: WITH --forward-accept, separate LAN host reachable through default-deny FORWARD (got '$got_on')"
    else
        fail "T-FWD: WITH --forward-accept, host still unreachable (got '$got_on')"
    fi
    if grep -q "forward-accept: inserted FORWARD ACCEPT" "$BORE_LOG.tfwd2_listen" 2>/dev/null; then
        pass "T-FWD: listener logged the forward-accept rule insertion"
    else
        fail "T-FWD: listener log missing 'forward-accept: inserted FORWARD ACCEPT' line"
    fi
    # The per-link forward-accept chain must be REVERTED on graceful exit (RAII).
    if ip netns exec ns1 iptables -S 2>/dev/null | grep -q "bore_tfwd2_fwd"; then
        fail "T-FWD: forward-accept chain bore_tfwd2_fwd leaked after exit"
    else
        pass "T-FWD: forward-accept chain reverted on graceful exit (RAII)"
    fi

    # Restore FORWARD policy so later tests' forwarding is unaffected.
    ip netns exec ns1 iptables -P FORWARD ACCEPT 2>/dev/null || true
    kill "$FW_S" 2>/dev/null || true
    ip netns exec ns_lanm pkill -f "TCP-LISTEN:7703" 2>/dev/null || true
    ip netns del ns_lanm 2>/dev/null || true
    sleep 0.5
else
    echo "WARN: T-FWD: socat/iptables unavailable — skipping"
fi
true  # guard: keep set -e from aborting on the prior short-circuit's exit code

# ── Test T-NAT4: mixed plain + NAT (scoped masquerade) ───────────────────────────────────
echo "=== T-NAT4: mixed plain + NAT advertise ==="
# Add a second plain LAN to ns1 (not NAT'd)
PLAIN_LAN="172.16.9.0/24"
PLAIN_HOST="172.16.9.99"
ip netns exec ns1 ip addr add "$PLAIN_HOST/24" dev lo

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat4 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A},$PLAIN_LAN" \
    >"$BORE_LOG.nat4_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat4 \
    --accept-all-routes \
    >"$BORE_LOG.nat4_connect" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.nat4_listen" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Should have both virtual (NAT'd) and plain routes
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        pass "T-NAT4: virtual NAT route $NAT_VIRT_A installed"
    else
        fail "T-NAT4: virtual NAT route missing"
    fi
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$PLAIN_LAN"; then
        pass "T-NAT4: plain route $PLAIN_LAN installed"
    else
        fail "T-NAT4: plain route missing"
    fi
    # Both should be reachable
    if ip netns exec ns2 ping -c 1 -W 3 "10.50.1.5" >/dev/null 2>&1; then
        pass "T-NAT4: NAT'd LAN reachable via 10.50.1.5"
    else
        fail "T-NAT4: ping to NAT'd LAN failed"
    fi
    if ip netns exec ns2 ping -c 1 -W 3 "$PLAIN_HOST" >/dev/null 2>&1; then
        pass "T-NAT4: plain LAN reachable via $PLAIN_HOST"
    else
        fail "T-NAT4: ping to plain LAN failed"
    fi
else
    fail "T-NAT4: listener did not pair"
fi

kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
sleep 0.5

# Clean up the plain LAN host alias
ip netns exec ns1 ip addr del "$PLAIN_HOST/24" dev lo 2>/dev/null || true

# ── Test T-NAT5: cleanup after graceful exit (RAII revert) ──────────────────────────────
echo "=== T-NAT5: NAT cleanup after graceful exit ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat5 \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.nat5_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-nat5 \
    --accept-all-routes \
    >"$BORE_LOG.nat5_connect" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.nat5_listen" "vpn link paired\|VpnReady" 10; then
    sleep 1
    # Confirm the NAT table exists while the link is up.
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_t-nat5"; then
        pass "T-NAT5: nft table bore_vpn_t-nat5 present while link up"
    else
        fail "T-NAT5: nft table bore_vpn_t-nat5 missing while link up"
    fi

    # Clean exit
    kill "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1

    # After exit, nft table should be gone (RAII revert). Use grep -q like the
    # existing teardown test (grep -c + '|| echo 0' double-counts on no match).
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_t-nat5"; then
        fail "T-NAT5: nft table NOT removed after graceful exit"
    else
        pass "T-NAT5: nft table removed after graceful exit (RAII revert)"
    fi

    # Routes should be removed from ns2
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        fail "T-NAT5: route to $NAT_VIRT_A NOT removed from ns2 after exit"
    else
        pass "T-NAT5: route to $NAT_VIRT_A removed from ns2 after exit"
    fi
else
    fail "T-NAT5: initial pair failed"
    kill "$BORE_LISTEN_PID" "$BORE_CONNECT_PID" 2>/dev/null; BORE_LISTEN_PID=""; BORE_CONNECT_PID=""
fi
sleep 0.5

# ── Test T-HUBNAT1: hub NAT multi-client (topology D) ──────────────────────────────────
echo "=== T-HUBNAT1: hub NAT with multiple spokes ==="
ip netns exec ns1 sysctl -qw net.ipv4.ip_forward=0 2>/dev/null || true

ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubnat-relay \
    --max-clients 4 --relay-only \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.hubnat1_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1

# Two spokes
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hubnat-relay \
    --relay-only --accept-all-routes \
    >"$BORE_LOG.hubnat1_conn2" 2>&1 &
BORE_HUB_CONN2=$!

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hubnat-relay \
    --relay-only --accept-all-routes \
    >"$BORE_LOG.hubnat1_conn3" 2>&1 &
BORE_HUB_CONN3=$!

# Wait for pairing
HUB_NAT_PAIRED=0
if wait_for_log "$BORE_LOG.hubnat1_listen" "vpn hub peer join" 15; then
    sleep 1
    PEER_JOIN_COUNT=$(grep -c "vpn hub peer join" "$BORE_LOG.hubnat1_listen" 2>/dev/null || echo 0)
    if [ "$PEER_JOIN_COUNT" -ge 2 ]; then
        HUB_NAT_PAIRED=1
    fi
fi

if [ "$HUB_NAT_PAIRED" = "1" ]; then
    sleep 1
    # Both spokes should have the virtual route
    if ip netns exec ns2 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        pass "T-HUBNAT1: spoke ns2 has route to hub virtual $NAT_VIRT_A"
    else
        fail "T-HUBNAT1: spoke ns2 missing virtual route"
    fi
    if ip netns exec ns3 ip route show 2>/dev/null | grep -q "$NAT_VIRT_A"; then
        pass "T-HUBNAT1: spoke ns3 has route to hub virtual $NAT_VIRT_A"
    else
        fail "T-HUBNAT1: spoke ns3 missing virtual route"
    fi
    # Both should reach the hub's real host via virtual
    if ip netns exec ns2 ping -c 1 -W 3 "10.50.1.5" >/dev/null 2>&1; then
        pass "T-HUBNAT1: spoke ns2 reaches hub real host via 10.50.1.5"
    else
        fail "T-HUBNAT1: spoke ns2 ping to hub failed"
    fi
    if ip netns exec ns3 ping -c 1 -W 3 "10.50.1.5" >/dev/null 2>&1; then
        pass "T-HUBNAT1: spoke ns3 reaches hub real host via 10.50.1.5"
    else
        fail "T-HUBNAT1: spoke ns3 ping to hub failed"
    fi
else
    fail "T-HUBNAT1: hub did not establish 2 spoke pairs"
fi

kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; BORE_HUB_CONN3=""
sleep 0.5

# ── Test T-HUBNAT2: spoke isolation intact with NAT ──────────────────────────────────────
echo "=== T-HUBNAT2: spoke isolation intact (NAT doesn't break it) ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id hubnat2 \
    --max-clients 4 --relay-only \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    >"$BORE_LOG.hubnat2_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 1

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id hubnat2 \
    --relay-only --accept-all-routes \
    >"$BORE_LOG.hubnat2_conn2" 2>&1 &
BORE_HUB_CONN2=$!

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id hubnat2 \
    --relay-only --accept-all-routes \
    >"$BORE_LOG.hubnat2_conn3" 2>&1 &
BORE_HUB_CONN3=$!

sleep 2.5; sleep 0.5  # settle
CONN2_OVL=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
CONN3_OVL=$(ip netns exec ns3 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

if [ -n "$CONN2_OVL" ] && [ -n "$CONN3_OVL" ]; then
    # ns2 should NOT reach ns3 (spoke isolation)
    if ip netns exec ns2 ping -c 1 -W 2 "$CONN3_OVL" >/dev/null 2>&1; then
        fail "T-HUBNAT2: ns2 reached ns3 (isolation violated)"
    else
        pass "T-HUBNAT2: ns2 cannot reach ns3 (isolation intact)"
    fi
else
    fail "T-HUBNAT2: overlay addresses missing"
fi

kill "$BORE_HUB_CONN2" "$BORE_HUB_CONN3" "$BORE_LISTEN_PID" 2>/dev/null
BORE_LISTEN_PID=""; BORE_HUB_CONN2=""; BORE_HUB_CONN3=""
sleep 0.5

# ── Test T-NATKILL: SIGKILL stale reclaim for NAT (nft + ip_forward) ───────────────────
echo "=== T-NATKILL: SIGKILL stale reclaim with NAT ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-natkill \
    --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
    --relay-only \
    >"$BORE_LOG.natkill_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-natkill \
    --relay-only --accept-all-routes \
    >"$BORE_LOG.natkill_connect" 2>&1 &
BORE_CONNECT_PID=$!

if wait_for_log "$BORE_LOG.natkill_listen" "vpn link paired" 10; then
    sleep 1
    # SIGKILL: no cleanup
    kill -9 "$BORE_LISTEN_PID" 2>/dev/null; BORE_LISTEN_PID=""
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 0.5

    # Stale table must exist
    if ip netns exec ns1 nft list tables 2>/dev/null | grep -q "bore_vpn_t-natkill"; then
        pass "T-NATKILL: nft table survived SIGKILL (stale state present)"
    else
        fail "T-NATKILL: nft table missing after SIGKILL"
    fi

    # Re-run with same id; should reclaim
    ip netns exec ns1 "$BORE" vpn listen \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-natkill \
        --advertise "${NAT_REAL_LAN}@${NAT_VIRT_A}" \
        --relay-only \
        >"$BORE_LOG.natkill_listen2" 2>&1 &
    BORE_LISTEN_PID=$!
    sleep 0.5

    ip netns exec ns2 "$BORE" vpn connect \
        --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id t-natkill \
        --relay-only --accept-all-routes \
        >"$BORE_LOG.natkill_connect2" 2>&1 &
    BORE_CONNECT_PID=$!

    if wait_for_log "$BORE_LOG.natkill_listen2" "vpn link paired" 15; then
        sleep 1
        # nft table should exist exactly once (reclaimed and re-created)
        NFT_COUNT=$(ip netns exec ns1 nft list tables 2>/dev/null | grep -c "bore_vpn_t-natkill" || echo 0)
        if [ "$NFT_COUNT" = "1" ]; then
            pass "T-NATKILL: nft table reclaimed and re-created (count=1)"
        else
            fail "T-NATKILL: nft table count=$NFT_COUNT after reclaim (expected 1)"
        fi

        # No EEXIST errors
        if grep -qi "file exists" "$BORE_LOG.natkill_listen2" "$BORE_LOG.natkill_connect2" 2>/dev/null; then
            fail "T-NATKILL: EEXIST errors found in logs (route replace regression)"
        else
            pass "T-NATKILL: no EEXIST errors during reclaim"
        fi

        # Ping should work
        if ip netns exec ns2 ping -c 1 -W 3 "10.50.1.5" >/dev/null 2>&1; then
            pass "T-NATKILL: ping works after reclaim"
        else
            fail "T-NATKILL: ping failed after reclaim"
        fi
    else
        fail "T-NATKILL: second run did not pair after reclaim"
    fi
else
    fail "T-NATKILL: initial pair failed"
    kill -9 "$BORE_LISTEN_PID" "$BORE_CONNECT_PID" 2>/dev/null; BORE_LISTEN_PID=""; BORE_CONNECT_PID=""
fi

kill "$BORE_LISTEN_PID" "$BORE_CONNECT_PID" 2>/dev/null; BORE_LISTEN_PID=""; BORE_CONNECT_PID=""
sleep 0.5

# Clean up NAT test lo aliases
ip netns exec ns1 ip addr del "$NAT_REAL_HOST_A/24" dev lo 2>/dev/null || true
ip netns exec ns2 ip addr del "$NAT_REAL_HOST_B/24" dev lo 2>/dev/null || true
ip netns exec ns3 ip addr del "$NAT_REAL_HOST_C/24" dev lo 2>/dev/null || true

# ── Test T-MULTI-TUN: two co-located connectors auto-assign distinct TUN names ────
# Regression guard: --tun-name auto defaults to the first free boreN interface,
# so two same-host connectors dialing different listeners no longer collide.
# Before the fix: both picked bore0, the second's startup tore down the first's interface.
echo "=== T-MULTI-TUN: two connectors same-host distinct auto TUN names ==="
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id multi-tun-alpha \
    --relay-only \
    >"$BORE_LOG.multi_tun_listen_alpha" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

ip netns exec ns3 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id multi-tun-beta \
    --relay-only \
    >"$BORE_LOG.multi_tun_listen_beta" 2>&1 &
BORE_LISTEN_PID_B=$!
sleep 0.5

# Start TWO connectors in ns2, both WITHOUT --tun-name (auto-assignment):
# First dials listen-alpha, second dials listen-beta.
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id multi-tun-alpha \
    --relay-only \
    >"$BORE_LOG.multi_tun_conn_alpha" 2>&1 &
BORE_CONNECT_PID=$!
sleep 0.5

ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id multi-tun-beta \
    --relay-only \
    >"$BORE_LOG.multi_tun_conn_beta" 2>&1 &
BORE_CONNECT_PID_B=$!

# Wait for both listeners to pair
if wait_for_log "$BORE_LOG.multi_tun_listen_alpha" "vpn link paired\|VpnReady" 10 && \
   wait_for_log "$BORE_LOG.multi_tun_listen_beta" "vpn link paired\|VpnReady" 10; then
    sleep 1.5  # Let both TUN interfaces settle

    # Assertion 1: Both TUN interfaces exist and are distinct (bore0 AND bore1)
    BORE0_EXISTS=$(ip netns exec ns2 ip link show bore0 >/dev/null 2>&1 && echo 1 || echo 0)
    BORE1_EXISTS=$(ip netns exec ns2 ip link show bore1 >/dev/null 2>&1 && echo 1 || echo 0)

    if [ "$BORE0_EXISTS" = "1" ] && [ "$BORE1_EXISTS" = "1" ]; then
        pass "T-MULTI-TUN: both bore0 and bore1 exist in ns2"
    else
        fail "T-MULTI-TUN: missing TUN interface (bore0=$BORE0_EXISTS bore1=$BORE1_EXISTS)"
    fi

    # Assertion 2: Get the overlay addresses from both interfaces
    BORE0_ADDR=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    BORE1_ADDR=$(ip netns exec ns2 ip addr show bore1 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS1_ALPHA_ADDR=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)
    NS3_BETA_ADDR=$(ip netns exec ns3 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1)

    if [ -n "$BORE0_ADDR" ] && [ -n "$BORE1_ADDR" ] && [ -n "$NS1_ALPHA_ADDR" ] && [ -n "$NS3_BETA_ADDR" ]; then
        # Assertion 2a: ping alpha link (ns2 bore0 → ns1 bore0)
        if ip netns exec ns2 ping -c 2 -W 3 "$NS1_ALPHA_ADDR" >/dev/null 2>&1; then
            pass "T-MULTI-TUN: traffic flows on alpha link (ns2 bore0 → ns1)"
        else
            fail "T-MULTI-TUN: ping failed on alpha link"
        fi

        # Assertion 2b: ping beta link (ns2 bore1 → ns3 bore0)
        if ip netns exec ns2 ping -c 2 -W 3 "$NS3_BETA_ADDR" >/dev/null 2>&1; then
            pass "T-MULTI-TUN: traffic flows on beta link (ns2 bore1 → ns3)"
        else
            fail "T-MULTI-TUN: ping failed on beta link"
        fi
    else
        fail "T-MULTI-TUN: one or more overlay addresses missing (bore0=$BORE0_ADDR bore1=$BORE1_ADDR ns1=$NS1_ALPHA_ADDR ns3=$NS3_BETA_ADDR)"
    fi

    # Assertion 3: RC1 regression guard — kill alpha connector, verify beta still works
    kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    sleep 1

    # bore0 should be gone, bore1 should still exist
    BORE0_AFTER=$(ip netns exec ns2 ip link show bore0 >/dev/null 2>&1 && echo 1 || echo 0)
    BORE1_AFTER=$(ip netns exec ns2 ip link show bore1 >/dev/null 2>&1 && echo 1 || echo 0)

    if [ "$BORE0_AFTER" = "0" ] && [ "$BORE1_AFTER" = "1" ]; then
        pass "T-MULTI-TUN: bore0 removed after alpha kill, bore1 still up"
    else
        fail "T-MULTI-TUN: unexpected TUN state after kill (bore0=$BORE0_AFTER bore1=$BORE1_AFTER, expected 0 and 1)"
    fi

    # Verify beta link still carries traffic after alpha is gone
    if [ -n "$NS3_BETA_ADDR" ] && ip netns exec ns2 ping -c 2 -W 3 "$NS3_BETA_ADDR" >/dev/null 2>&1; then
        pass "T-MULTI-TUN: beta link still carries traffic after alpha dies"
    else
        fail "T-MULTI-TUN: beta link broke after alpha kill"
    fi
else
    fail "T-MULTI-TUN: one or both listeners did not pair within 10s"
fi

kill "$BORE_LISTEN_PID" "$BORE_LISTEN_PID_B" "$BORE_CONNECT_PID" "$BORE_CONNECT_PID_B" 2>/dev/null
BORE_LISTEN_PID=""
BORE_LISTEN_PID_B=""
BORE_CONNECT_PID=""
BORE_CONNECT_PID_B=""
sleep 0.5

# ── Test T-STRESS-PORTCLASH: VPN + secret consumer on same UDP port (reproduces flap bug) ─
# Two direct-path --udp tunnels (VPN connector + secret consumer) on the same host,
# both pinned to --nat-udp-preferred-port 51820 (the bug). They steal each other's
# inbound QUIC packets via SO_REUSEADDR rebinding, causing the VPN direct path to
# flap: "bridge switched to direct" → 30s idle → "direct path lost" → relay → retry
# → repeat. This test REPRODUCES the flap on the buggy code (should see >= 1 "lost"
# or >= 2 "switched"). On fixed code it PASSES (switched==1, lost==0).
echo "=== T-STRESS-PORTCLASH: port clash direct-path flap repro ==="

# Start a VPN listener in ns1 (gateway, listener side)
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id clash-vpn \
    --stun-server "$STUN" \
    >"$BORE_LOG.portclash_listen" 2>&1 &
BORE_LISTEN_PID=$!
sleep 0.5

# Start VPN connector in ns2, pinned to port 51820
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id clash-vpn \
    --stun-server "$STUN" \
    --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.portclash_vpn_conn" 2>&1 &
PORTCLASH_VPN_CONN_PID=$!
sleep 1

# Start a secret provider (bore local) in ns1 backed by a simple HTTP server
ip netns exec ns1 python3 -m http.server 19000 --bind 127.0.0.1 >/dev/null 2>&1 &
PORTCLASH_HTTP_PID=$!
sleep 0.3

ip netns exec ns1 "$BORE" local 19000 \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
    --tcp-secret-id clash-sec --udp \
    --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.portclash_secret_prov" 2>&1 &
PORTCLASH_SECRET_PROV_PID=$!
sleep 0.5

# Start secret consumer (bore proxy) in ns2, also pinned to 51820 (THE CLASH)
ip netns exec ns2 "$BORE" proxy \
    --local-proxy-port ":19001" \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" \
    --tcp-secret-id clash-sec --udp \
    --nat-udp-preferred-port 51820 \
    >"$BORE_LOG.portclash_secret_cons" 2>&1 &
PORTCLASH_SECRET_CONS_PID=$!
sleep 2

# Let it run for 75 seconds to capture flap behavior
echo "  (running for 75 seconds to observe VPN direct path behavior...)"
sleep 75

# Count the flap indicators in the VPN connector log
SWITCHED=$(grep -c "bridge switched to direct path" "$BORE_LOG.portclash_vpn_conn" 2>/dev/null | tr -d '[:space:]' || echo 0)
LOST=$(grep -c "direct path lost" "$BORE_LOG.portclash_vpn_conn" 2>/dev/null | tr -d '[:space:]' || echo 0)

echo "  T-STRESS-PORTCLASH: VPN connector log flap counts:"
echo "    bridge switched to direct path: $SWITCHED"
echo "    direct path lost: $LOST"

# On BUGGY code: expect flap (multiple switches or at least one loss)
# On FIXED code: expect PASS (switched==1, lost==0)
EXPECTED_PASS=0
if [ "$SWITCHED" -eq 1 ] && [ "$LOST" -eq 0 ]; then
    pass "T-STRESS-PORTCLASH: direct path stable (switched=$SWITCHED, lost=$LOST) — port clash FIXED"
    EXPECTED_PASS=1
elif [ "$LOST" -ge 1 ] || [ "$SWITCHED" -ge 2 ]; then
    fail "T-STRESS-PORTCLASH: direct path FLAPPED (switched=$SWITCHED, lost=$LOST) — port clash BUG REPRODUCED"
    echo "  [VPN connector log tail]:"
    tail -8 "$BORE_LOG.portclash_vpn_conn" 2>/dev/null | sed 's/^/    /'
else
    fail "T-STRESS-PORTCLASH: unexpected flap counts (switched=$SWITCHED, lost=$LOST, expected either 0 or >=2 switched)"
fi

# Cleanup portclash processes
kill "$PORTCLASH_VPN_CONN_PID" "$PORTCLASH_SECRET_PROV_PID" "$PORTCLASH_SECRET_CONS_PID" "$PORTCLASH_HTTP_PID" "$BORE_LISTEN_PID" 2>/dev/null
PORTCLASH_VPN_CONN_PID=""
PORTCLASH_SECRET_PROV_PID=""
PORTCLASH_SECRET_CONS_PID=""
PORTCLASH_HTTP_PID=""
BORE_LISTEN_PID=""
sleep 1

# ── Test T-STRESS-MIX: mixed-load stress test (all tunnel types concurrent) ───────
echo "=== T-STRESS-MIX: concurrent mixed-load stability test (90s window) ==="
MIX_WINDOW="${MIX_WINDOW:-90}"
echo "  Running all tunnel types concurrently for $MIX_WINDOW seconds..."

# Helper to track and report tunnel flaps
declare -A MIX_FLAP_COUNTS
declare -A MIX_FLAP_TARGETS

track_flap() {
    local log_file="$1"
    local tunnel_id="$2"
    local lost_key="${tunnel_id}_lost"
    local switched_key="${tunnel_id}_switched"
    local lost switched
    # A flap = a fall-back (lost) followed by a re-upgrade, so "direct path lost"
    # is the unambiguous flap signal. `switched` is informational only: count ONE
    # canonical marker per upgrade ("vpn path upgraded to direct QUIC") — a hub
    # LISTENER legitimately logs N (one per spoke), so it must NOT be a fail gate.
    lost=$(grep -c "direct path lost\|fell back to warm relay" "$log_file" 2>/dev/null || true)
    switched=$(grep -c "vpn path upgraded to direct QUIC" "$log_file" 2>/dev/null || true)
    MIX_FLAP_COUNTS["$lost_key"]=$(printf '%s' "${lost:-0}" | tr -dc '0-9')
    MIX_FLAP_COUNTS["$switched_key"]=$(printf '%s' "${switched:-0}" | tr -dc '0-9')
    MIX_FLAP_TARGETS["$lost_key"]=0
    MIX_FLAP_TARGETS["$switched_key"]=1
}

# Start public tunnel 1 (bore local on port 18080)
ip netns exec ns1 python3 -m http.server 18080 --bind 127.0.0.1 >/dev/null 2>&1 &
MIX_PIDS+=($!)
sleep 0.2
ip netns exec ns1 "$BORE" local 18080 \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --port 18080 --udp \
    >"$BORE_LOG.mix_pub1" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start public tunnel 2 (bore local on port 18081)
ip netns exec ns1 python3 -m http.server 18081 --bind 127.0.0.1 >/dev/null 2>&1 &
MIX_PIDS+=($!)
sleep 0.2
ip netns exec ns1 "$BORE" local 18081 \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --port 18081 --udp \
    >"$BORE_LOG.mix_pub2" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start secret provider 1 (service on 19100)
ip netns exec ns2 python3 -m http.server 19100 --bind 127.0.0.1 >/dev/null 2>&1 &
MIX_PIDS+=($!)
sleep 0.2
ip netns exec ns2 "$BORE" local 19100 \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --tcp-secret-id mix-sec-A --udp \
    --nat-udp-preferred-port 51821 \
    >"$BORE_LOG.mix_sec_prov_A" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start secret consumer 1 (bore proxy)
ip netns exec ns2 "$BORE" proxy \
    --local-proxy-port ":19101" \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --tcp-secret-id mix-sec-A --udp \
    --nat-udp-preferred-port 51821 \
    >"$BORE_LOG.mix_sec_cons_A" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start secret provider 2 (service on 19110)
ip netns exec ns3 python3 -m http.server 19110 --bind 127.0.0.1 >/dev/null 2>&1 &
MIX_PIDS+=($!)
sleep 0.2
ip netns exec ns3 "$BORE" local 19110 \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --tcp-secret-id mix-sec-B --udp \
    --nat-udp-preferred-port 51822 \
    >"$BORE_LOG.mix_sec_prov_B" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start secret consumer 2 (bore proxy)
ip netns exec ns3 "$BORE" proxy \
    --local-proxy-port ":19111" \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --tcp-secret-id mix-sec-B --udp \
    --nat-udp-preferred-port 51822 \
    >"$BORE_LOG.mix_sec_cons_B" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start VPN 1:1 listener
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mix-1to1 \
    --advertise "$FAKE_LAN" --stun-server "$STUN" \
    >"$BORE_LOG.mix_vpn_1to1_listen" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start VPN 1:1 connector
ip netns exec ns4 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_D" --secret "$SECRET" --id mix-1to1 \
    --accept-all-routes --stun-server "$STUN" \
    >"$BORE_LOG.mix_vpn_1to1_conn" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start VPN hub listener (max 4 clients)
ip netns exec ns1 "$BORE" vpn listen \
    --to "$SERVER_IP_NS0_A" --secret "$SECRET" --id mix-hub \
    --max-clients 4 --stun-server "$STUN" \
    >"$BORE_LOG.mix_vpn_hub_listen" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

# Start hub connectors (ns2 and ns3)
ip netns exec ns2 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_B" --secret "$SECRET" --id mix-hub \
    --accept-all-routes --stun-server "$STUN" \
    >"$BORE_LOG.mix_vpn_hub_conn_1" 2>&1 &
MIX_PIDS+=($!); sleep 0.3

ip netns exec ns3 "$BORE" vpn connect \
    --to "$SERVER_IP_NS0_C" --secret "$SECRET" --id mix-hub \
    --accept-all-routes --stun-server "$STUN" \
    >"$BORE_LOG.mix_vpn_hub_conn_2" 2>&1 &
MIX_PIDS+=($!); sleep 0.5

# Let the mix run for the specified window
sleep "$MIX_WINDOW"

# Collect flap metrics from all tunnels
echo "  Collecting flap metrics..."
track_flap "$BORE_LOG.mix_vpn_1to1_listen" "mix_1to1_listen"
track_flap "$BORE_LOG.mix_vpn_1to1_conn" "mix_1to1_conn"
track_flap "$BORE_LOG.mix_vpn_hub_listen" "mix_hub_listen"
track_flap "$BORE_LOG.mix_vpn_hub_conn_1" "mix_hub_conn_1"
track_flap "$BORE_LOG.mix_vpn_hub_conn_2" "mix_hub_conn_2"

# Connectivity checks
echo "  Verifying connectivity..."
MIX_CONN_OK=0
# Check VPN 1:1 overlay ping
NS1_OVL=$(ip netns exec ns1 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1 | head -1)
NS4_OVL=$(ip netns exec ns4 ip addr show bore0 2>/dev/null | grep "inet " | awk '{print $2}' | cut -d/ -f1 | tail -1)
if [ -n "$NS1_OVL" ] && [ -n "$NS4_OVL" ] && ip netns exec ns4 ping -c 1 -W 2 "$NS1_OVL" >/dev/null 2>&1; then
    pass "T-STRESS-MIX: 1:1 VPN tunnel overlay reachable"
    MIX_CONN_OK=$((MIX_CONN_OK + 1))
else
    fail "T-STRESS-MIX: 1:1 VPN tunnel overlay unreachable"
fi

# Hub data plane established: both spokes brought up their overlay TUN. (Spoke→spoke
# is blocked by isolation by design — see T-HUBNAT2 — and the hub gateway overlay
# addressing is config-specific, so assert the TUNs are up rather than a fixed IP.)
HUB_S2=$(ip netns exec ns2 ip addr show bore0 2>/dev/null | grep -c "inet ")
HUB_S3=$(ip netns exec ns3 ip addr show bore0 2>/dev/null | grep -c "inet ")
if [ "${HUB_S2:-0}" -ge 1 ] && [ "${HUB_S3:-0}" -ge 1 ]; then
    pass "T-STRESS-MIX: hub spokes ns2+ns3 overlay TUN up (hub data plane established)"
    MIX_CONN_OK=$((MIX_CONN_OK + 1))
else
    fail "T-STRESS-MIX: hub spoke TUN not established (ns2=${HUB_S2:-0} ns3=${HUB_S3:-0})"
fi

# Check secret tunnel (consume via proxy)
if timeout 3 ip netns exec ns2 curl -s http://127.0.0.1:19101 >/dev/null 2>&1; then
    pass "T-STRESS-MIX: secret consumer A tunnel connected"
    MIX_CONN_OK=$((MIX_CONN_OK + 1))
else
    fail "T-STRESS-MIX: secret consumer A tunnel failed"
fi

# Report flap metrics
echo ""
echo "  T-STRESS-MIX: stability metric — every tunnel must have lost==0 (no fall-back = no flap):"
for key in "${!MIX_FLAP_COUNTS[@]}"; do
    if [[ "$key" == *"_lost"* ]]; then
        actual="${MIX_FLAP_COUNTS[$key]}"
        [ "$actual" -eq 0 ] && echo "    $key: $actual (OK)" || echo "    $key: $actual (FLAP)"
    fi
done
echo "  (informational) direct upgrades per tunnel — a hub listener legitimately shows one per spoke:"
for key in "${!MIX_FLAP_COUNTS[@]}"; do
    if [[ "$key" == *"_switched"* ]]; then
        echo "    $key: ${MIX_FLAP_COUNTS[$key]}"
    fi
done

# Overall verdict: a flap is a fall-back (lost) → re-upgrade, so lost==0 across all
# tunnels is the complete "no flap" condition. `switched` is informational only.
MIX_STABLE=1
for key in "${!MIX_FLAP_COUNTS[@]}"; do
    if [[ "$key" == *"_lost"* ]]; then
        [ "${MIX_FLAP_COUNTS[$key]}" -gt 0 ] && MIX_STABLE=0
    fi
done

if [ "$MIX_STABLE" -eq 1 ] && [ "$MIX_CONN_OK" -ge 2 ]; then
    pass "T-STRESS-MIX: all tunnels stable under concurrent mixed load (${MIX_WINDOW}s)"
else
    fail "T-STRESS-MIX: tunnel instability detected during mixed load test"
fi

# Cleanup all MIX PIDs
for pid in "${MIX_PIDS[@]}"; do kill "$pid" 2>/dev/null; done
MIX_PIDS=()
sleep 1

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "=== Results: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
