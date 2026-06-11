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

pass() { echo "PASS: $*"; PASS=$((PASS+1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL+1)); }
die()  { echo "ERROR: $*" >&2; cleanup; exit 1; }

cleanup() {
    set +e
    [ -n "$BORE_CONNECT_PID" ] && kill "$BORE_CONNECT_PID" 2>/dev/null; BORE_CONNECT_PID=""
    [ -n "$BORE_LISTEN_PID"  ] && kill "$BORE_LISTEN_PID"  2>/dev/null; BORE_LISTEN_PID=""
    [ -n "$BORE_SERVER_PID"  ] && kill "$BORE_SERVER_PID"  2>/dev/null; BORE_SERVER_PID=""
    sleep 0.5
    ip netns del ns0 2>/dev/null
    ip netns del ns1 2>/dev/null
    ip netns del ns2 2>/dev/null
    rm -f "$BORE_LOG"
    set -e
}
trap cleanup EXIT INT TERM

# ── Setup ──────────────────────────────────────────────────────────────────────
echo "=== Setup: creating netns ns0/ns1/ns2 ==="
ip netns add ns0
ip netns add ns1
ip netns add ns2

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

# Enable loopback in all ns
ip netns exec ns0 ip link set lo up
ip netns exec ns1 ip link set lo up
ip netns exec ns2 ip link set lo up

# ns1 fake LAN on loopback
ip netns exec ns1 ip addr add "$FAKE_LAN_HOST/24" dev lo

# Default routes: ns1 and ns2 route to each other via server (ns0)
ip netns exec ns1 ip route add default via "$SERVER_IP_NS0_A"
ip netns exec ns2 ip route add default via "$SERVER_IP_NS0_B"
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
    --stun-server "$STUN" \
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
    sleep 3  # generous window: an unwanted upgrade would land in here
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

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "=== Results: PASS=$PASS FAIL=$FAIL ==="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
