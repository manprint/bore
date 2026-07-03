#!/usr/bin/env bash
# Android emulator e2e harness — Phase 4.2 VPN runtime validation.
#
# Topology: emulator guest (rooted via `adb root`, target: default AOSP image)
# runs the android VPN connector + the android_vpn_spike example; the runner
# HOST runs the linux release binary as both the bore server (unprivileged)
# and the VPN listener (needs CAP_NET_ADMIN for the TUN → sudo). The guest
# reaches the host's server at 10.0.2.2 (emulator NAT alias); the host reaches
# its own server via loopback. Both sides pair through the SAME bore server,
# so ICMP between their overlay addresses crosses the relay/direct link, not
# the emulator's own network — physical NAT topology is irrelevant to whether
# the ping succeeds.
#
# Assumes: an emulator is already running (rooted, `target: default`) and
# `adb` is on PATH.
# Usage: BORE_HOST_BIN=<linux release bin, --features vpn> \
#        BORE_ANDROID_BIN=<x86_64-linux-android release bin, --features vpn> \
#        BORE_SPIKE_BIN=<x86_64-linux-android release examples/android_vpn_spike> \
#        scripts/android_vpn_test.sh
# Exit code: 0 = all tests passed, nonzero = failures
#
# Host-side `bore vpn listen` needs CAP_NET_ADMIN → sudo. Hosted GitHub
# runners have passwordless sudo, so plain `sudo` is used (the `-n`
# exact-path NOPASSWD rule elsewhere in this repo is a dev-box constraint;
# this script never reaches its sudo calls on the dev box since it exits
# early at the "no adb device" guard).

set -euo pipefail

BORE_HOST_BIN="${BORE_HOST_BIN:?set BORE_HOST_BIN to the linux --features vpn release binary}"
BORE_ANDROID_BIN="${BORE_ANDROID_BIN:?set BORE_ANDROID_BIN to the x86_64-linux-android --features vpn release binary}"
BORE_SPIKE_BIN="${BORE_SPIKE_BIN:?set BORE_SPIKE_BIN to the x86_64-linux-android release examples/android_vpn_spike binary}"

DEV_BORE="/data/local/tmp/bore"
DEV_SPIKE="/data/local/tmp/android_vpn_spike"
GUEST_TO="10.0.2.2"
HOST_TO="127.0.0.1"
SECRET="andvpn$(shuf -i 1000-9999 -n1 2>/dev/null || echo 4242)"
POOL="10.199.0.0/16"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE_HOST_BIN" ]; then
    echo "ERROR: $BORE_HOST_BIN not found or not executable." >&2
    exit 1
fi
if [ ! -f "$BORE_ANDROID_BIN" ]; then
    echo "ERROR: $BORE_ANDROID_BIN not found." >&2
    exit 1
fi
if [ ! -f "$BORE_SPIKE_BIN" ]; then
    echo "ERROR: $BORE_SPIKE_BIN not found." >&2
    exit 1
fi

for cmd in adb sudo; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

if ! adb get-state >/dev/null 2>&1; then
    echo "ERROR: no adb device/emulator attached." >&2
    exit 1
fi

TMPDIR="/tmp/bore_android_vpn_$$"
mkdir -p "$TMPDIR"

PASS=0
FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL + 1)); }

HOST_SERVER_PID=""

# ── Cleanup ─────────────────────────────────────────────────────────────────
# Kill only what this script started: the guest's bore/spike processes
# (path-scoped pkill -f) and the host's server + any sudo'd vpn listeners
# (id-scoped pkill -f, since sudo forking means `$!` is sudo's own pid, not
# the child's). Never a generic `pkill bore`.
cleanup() {
    adb shell "pkill -f $DEV_BORE" >/dev/null 2>&1 || true
    adb shell "pkill -f $DEV_SPIKE" >/dev/null 2>&1 || true
    sudo pkill -f "vpn listen.*android-vpn-e2e" >/dev/null 2>&1 || true
    sleep 1
    if [ -n "$HOST_SERVER_PID" ]; then
        kill "$HOST_SERVER_PID" >/dev/null 2>&1 || true
        wait "$HOST_SERVER_PID" 2>/dev/null || true
    fi
    adb root >/dev/null 2>&1 || true
    rm -rf "$TMPDIR"
}
trap cleanup EXIT INT TERM

# Waits up to `timeout` seconds (0.5s poll) for `pattern` (grep -E) to appear
# in `file`.
wait_for_log() {
    local file="$1" pattern="$2" timeout="${3:-10}"
    local polls=$((timeout * 2))
    local i=0
    while [ "$i" -lt "$polls" ]; do
        if [ -f "$file" ] && grep -qE "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.5
        i=$((i + 1))
    done
    return 1
}

# ── Setup ───────────────────────────────────────────────────────────────────
echo "adb root..."
adb root >/dev/null 2>&1 || true
adb wait-for-device

echo "Pushing android binaries to device..."
adb push "$BORE_ANDROID_BIN" "$DEV_BORE" >/dev/null
adb push "$BORE_SPIKE_BIN" "$DEV_SPIKE" >/dev/null
adb shell chmod 755 "$DEV_BORE" "$DEV_SPIKE"

echo "Starting host bore server (--secret, --vpn, --vpn-pool $POOL)..."
"$BORE_HOST_BIN" server --secret "$SECRET" --vpn --vpn-pool "$POOL" --vpn-max-links 8 \
    >"$TMPDIR/server.log" 2>&1 &
HOST_SERVER_PID=$!
sleep 1
if ! kill -0 "$HOST_SERVER_PID" 2>/dev/null; then
    echo "ERROR: host server failed to start:" >&2
    cat "$TMPDIR/server.log" >&2
    exit 1
fi

# ── T-AND-S1..S3: spike modes ────────────────────────────────────────────────
echo "=== T-AND-S1: spike (TUN create + self-ping) ==="
SPIKE_OUT="$(adb shell "$DEV_SPIKE spike; echo EXIT:\$?" 2>&1)"
if echo "$SPIKE_OUT" | grep -q "EXIT:0" && ! echo "$SPIKE_OUT" | grep -q "^FAIL"; then
    pass "T-AND-S1 spike"
else
    fail "T-AND-S1 spike: $SPIKE_OUT"
fi

echo "=== T-AND-S2: create-teardown, apply-revert ==="
CT_OUT="$(adb shell "$DEV_SPIKE create-teardown; echo EXIT:\$?" 2>&1)"
AR_OUT="$(adb shell "$DEV_SPIKE apply-revert; echo EXIT:\$?" 2>&1)"
if echo "$CT_OUT" | grep -q "EXIT:0" && ! echo "$CT_OUT" | grep -q "^FAIL" \
    && echo "$AR_OUT" | grep -q "EXIT:0" && ! echo "$AR_OUT" | grep -q "^FAIL"; then
    pass "T-AND-S2 create-teardown + apply-revert"
else
    fail "T-AND-S2 create-teardown/apply-revert: ct=[$CT_OUT] ar=[$AR_OUT]"
fi

echo "=== T-AND-S3: leak-then-reclaim ==="
LR_OUT="$(adb shell "$DEV_SPIKE leak-then-reclaim; echo EXIT:\$?" 2>&1)"
if echo "$LR_OUT" | grep -q "EXIT:0" && ! echo "$LR_OUT" | grep -q "^FAIL"; then
    pass "T-AND-S3 leak-then-reclaim"
else
    fail "T-AND-S3 leak-then-reclaim: $LR_OUT"
fi

# Runs one full listen/connect pairing and asserts bidirectional ping.
# $1 = link id, $2 = extra listen flags, $3 = extra connect flags,
# $4 = descriptive test id (for PASS/FAIL messages), $5 = 1 to also report
# the direct-vs-relay path (informational only, never fails the test).
run_link_test() {
    local id="$1" listen_extra="$2" connect_extra="$3" test_id="$4" report_path="$5"
    local listen_log="$TMPDIR/${id}_listen.log"
    local guest_log="/data/local/tmp/${id}_connect.log"

    echo "--- $test_id: pairing id=$id ---"
    # SC2086: intentional word-splitting of caller-provided extra flags.
    # SC2024: the redirect is opened by this (unprivileged) shell before
    # exec'ing sudo, so the child inherits the already-open fd — no
    # permission issue writing into a TMPDIR file this user owns.
    # shellcheck disable=SC2086,SC2024
    sudo "$BORE_HOST_BIN" vpn listen --to "$HOST_TO" --secret "$SECRET" --id "$id" \
        $listen_extra >"$listen_log" 2>&1 &
    sleep 1

    # shellcheck disable=SC2086
    adb shell "nohup $DEV_BORE vpn connect --to $GUEST_TO --secret $SECRET --id $id \
        $connect_extra >$guest_log 2>&1 &"

    # "vpn link paired" is the ONLY safe match: the failure branch's log line
    # ("server closed before sending VpnReady; may be too old or not
    # VPN-capable") also contains the substring "VpnReady", so alternating on
    # that would false-positive a genuine pairing failure as success.
    if ! wait_for_log "$listen_log" "vpn link paired" 20; then
        fail "$test_id: host listener never reported link pairing"
        cat "$listen_log" >&2 || true
        sudo pkill -f "id $id" >/dev/null 2>&1 || true
        adb shell "pkill -f $DEV_BORE" >/dev/null 2>&1 || true
        return
    fi
    # Give the TUN + route table a moment to settle after pairing.
    sleep 2

    # `|| true` on each: a missing bore0 (failed pairing) must fail just this
    # one test via the empty-string check below, not abort the whole script
    # under `set -e -o pipefail`.
    local host_overlay guest_overlay
    host_overlay="$(ip -4 -o addr show bore0 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1 || true)"
    guest_overlay="$(adb shell "ip -4 -o addr show bore0" 2>/dev/null | tr -d '\r' | awk '{print $4}' | cut -d/ -f1 | head -1 || true)"

    if [ -z "$host_overlay" ] || [ -z "$guest_overlay" ]; then
        fail "$test_id: could not discover overlay addrs (host=[$host_overlay] guest=[$guest_overlay])"
    else
        echo "host_overlay=$host_overlay guest_overlay=$guest_overlay"
        local guest_ping_ok=0 host_ping_ok=0
        if adb shell "ping -c 3 $host_overlay" 2>&1 | tee "$TMPDIR/${id}_guest_ping.log" | grep -qE "^3 packets transmitted, 3 (packets )?received|3 received"; then
            guest_ping_ok=1
        fi
        if ping -c 3 "$guest_overlay" >"$TMPDIR/${id}_host_ping.log" 2>&1; then
            host_ping_ok=1
        fi
        if [ "$guest_ping_ok" -eq 1 ] && [ "$host_ping_ok" -eq 1 ]; then
            pass "$test_id: bidirectional ping ok"
        else
            fail "$test_id: guest_ping_ok=$guest_ping_ok host_ping_ok=$host_ping_ok"
            cat "$TMPDIR/${id}_guest_ping.log" >&2 || true
            cat "$TMPDIR/${id}_host_ping.log" >&2 || true
        fi
    fi

    if [ "$report_path" = "1" ]; then
        if grep -q "vpn path upgraded to direct QUIC" "$listen_log" 2>/dev/null; then
            echo "$test_id path: DIRECT"
        else
            echo "$test_id path: RELAY (fallback — expected on emulator NAT, not a failure)"
        fi
    fi

    sudo pkill -f "id $id" >/dev/null 2>&1 || true
    adb shell "pkill -f $DEV_BORE" >/dev/null 2>&1 || true
    sleep 1
}

# ── T-AND-L1: relay link, bidirectional ping ─────────────────────────────────
run_link_test "android-vpn-e2e-relay" "--relay-only" "--relay-only" "T-AND-L1" "0"

# ── T-AND-L2: direct best-effort, informational only ────────────────────────
run_link_test "android-vpn-e2e-direct" "" "" "T-AND-L2" "1"

# ── T-AND-L3: non-root negative ──────────────────────────────────────────────
echo "=== T-AND-L3: non-root guard ==="
adb unroot >/dev/null 2>&1 || true
adb wait-for-device
L3_OUT="$(adb shell "$DEV_BORE vpn connect --to $GUEST_TO --secret $SECRET --id android-vpn-e2e-l3; echo EXIT:\$?" 2>&1)"
adb root >/dev/null 2>&1 || true
adb wait-for-device
if ! echo "$L3_OUT" | grep -q "EXIT:0" && echo "$L3_OUT" | grep -q "tsu / Magisk su"; then
    pass "T-AND-L3 non-root rejected with root-hint message"
else
    fail "T-AND-L3 non-root guard: $L3_OUT"
fi

# ── T-AND-L4: --advertise guard (host-only) ─────────────────────────────────
echo "=== T-AND-L4: --advertise guard ==="
L4_OUT="$(adb shell "$DEV_BORE vpn connect --to $GUEST_TO --secret $SECRET --id android-vpn-e2e-l4 --advertise 192.168.1.0/24; echo EXIT:\$?" 2>&1)"
if ! echo "$L4_OUT" | grep -q "EXIT:0" && echo "$L4_OUT" | grep -q "host-only"; then
    pass "T-AND-L4 --advertise rejected (host-only)"
else
    fail "T-AND-L4 --advertise guard: $L4_OUT"
fi

# ── T-AND-L5: --tun-queues guard (multi-queue) ──────────────────────────────
echo "=== T-AND-L5: --tun-queues guard ==="
L5_OUT="$(adb shell "$DEV_BORE vpn connect --to $GUEST_TO --secret $SECRET --id android-vpn-e2e-l5 --tun-queues 2; echo EXIT:\$?" 2>&1)"
if ! echo "$L5_OUT" | grep -q "EXIT:0" && echo "$L5_OUT" | grep -q "multi-queue"; then
    pass "T-AND-L5 --tun-queues rejected (multi-queue)"
else
    fail "T-AND-L5 --tun-queues guard: $L5_OUT"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "===================="
echo "PASS: $PASS  FAIL: $FAIL"
echo "===================="
[ "$FAIL" -eq 0 ]
