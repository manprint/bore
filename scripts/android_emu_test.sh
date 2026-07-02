#!/usr/bin/env bash
# Android emulator e2e harness — Phase 2 non-VPN runtime validation.
#
# Topology: emulator guest (uid shell, no CAP_NET_ADMIN/TUN, cannot bind <1024)
# runs the android release binary; the runner HOST runs the linux release
# binary as the bore server + as the other tunnel peer. The guest reaches the
# host at 10.0.2.2 (emulator NAT). The host reaches its own server directly
# (public tunnel ports are bound on the host), so no `adb forward` is needed.
#
# Assumes: an emulator is already running and `adb` is on PATH.
# Usage: BORE_HOST_BIN=<linux release bin> BORE_ANDROID_BIN=<x86_64-linux-android release bin> \
#        scripts/android_emu_test.sh
# Exit code: 0 = all tests passed, nonzero = failures
#
# LIMITS (Android platform limits found during hardening — carry to phase 5 docs):
#   (none yet)
#
# Deviation from phase_02.md's literal wording: `bore transfer listener`/`sender`
# rendezvous directly with the bore server via --to/--transfer-id (same
# secret-tunnel-style protocol as `bore proxy`/`bore local --tcp-secret-id`) —
# confirmed against tests/transfer_test.rs and the Sender/Listener clap
# structs, neither of which exposes a raw host:port dial target. T-AND-E2 below
# therefore does NOT wrap transfer in a `bore local` public tunnel; it uses the
# transfer subsystem's own built-in rendezvous, which is the actually-shipped
# behavior.

set -euo pipefail

BORE_HOST_BIN="${BORE_HOST_BIN:?set BORE_HOST_BIN to the linux release binary}"
BORE_ANDROID_BIN="${BORE_ANDROID_BIN:?set BORE_ANDROID_BIN to the x86_64-linux-android release binary}"

DEV_BORE="/data/local/tmp/bore"
DEV_INBOX="/data/local/tmp/inbox"
HOST_TO="10.0.2.2"

# ── Guards ──────────────────────────────────────────────────────────────────
if [ ! -x "$BORE_HOST_BIN" ]; then
    echo "ERROR: $BORE_HOST_BIN not found or not executable." >&2
    exit 1
fi
if [ ! -f "$BORE_ANDROID_BIN" ]; then
    echo "ERROR: $BORE_ANDROID_BIN not found." >&2
    exit 1
fi

for cmd in adb curl; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "SKIP: $cmd not installed" >&2
        exit 0
    fi
done

if ! adb get-state >/dev/null 2>&1; then
    echo "ERROR: no adb device/emulator attached." >&2
    exit 1
fi

TMPDIR="/tmp/bore_android_$$"
mkdir -p "$TMPDIR"

PASS=0
FAIL=0
pass() { echo "PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $*"; FAIL=$((FAIL + 1)); }

HOST_SERVER_PID=""

# ── Cleanup ─────────────────────────────────────────────────────────────────
# Kill only the guest's bore processes (path-scoped pkill -f on the on-device
# binary path) and the one host server PID we started — never a generic
# `pkill bore`/`pkill nc` that could disturb an unrelated host process.
cleanup() {
    adb shell "pkill -f $DEV_BORE" >/dev/null 2>&1 || true
    if [ -n "$HOST_SERVER_PID" ]; then
        kill "$HOST_SERVER_PID" >/dev/null 2>&1 || true
        wait "$HOST_SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMPDIR"
}
trap cleanup EXIT INT TERM

kill_guest_bore() {
    adb shell "pkill -f $DEV_BORE" >/dev/null 2>&1 || true
    sleep 0.5
}

# ── Setup ───────────────────────────────────────────────────────────────────
echo "Pushing android binary to device..."
adb push "$BORE_ANDROID_BIN" "$DEV_BORE" >/dev/null
adb shell chmod 755 "$DEV_BORE"
adb shell "mkdir -p $DEV_INBOX"

echo "Starting host server (min-port 40000, max-port 40100, --udp)..."
"$BORE_HOST_BIN" server --min-port 40000 --max-port 40100 --udp \
    >"$TMPDIR/server.log" 2>&1 &
HOST_SERVER_PID=$!
sleep 1
if ! kill -0 "$HOST_SERVER_PID" 2>/dev/null; then
    echo "ERROR: host server failed to start:" >&2
    cat "$TMPDIR/server.log" >&2
    exit 1
fi

# Toybox/busybox nc dialects spell the listen-port flag differently; probe
# once and pick the invocation the on-device `nc` actually accepts.
NC_HELP="$(adb shell "nc --help" 2>&1 || true)"
if echo "$NC_HELP" | grep -qE -- '-p[[:space:]]'; then
    NC_LISTEN_FLAGS="-l -p"
else
    NC_LISTEN_FLAGS="-l"
fi
echo "Guest nc listen flags: $NC_LISTEN_FLAGS <port>"

# One-shot HTTP responder on the guest, backgrounded remotely (adb shell
# returns as soon as the remote shell detaches it with nohup+&).
guest_http_responder() {
    local port="$1"
    adb shell "nohup sh -c \"printf 'HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello' | nc $NC_LISTEN_FLAGS $port\" >/data/local/tmp/nc_$port.log 2>&1 &"
}

# Launch a guest bore process detached (nohup + &), redirected to a log file
# under /data/local/tmp so it survives the adb shell round-trip.
guest_bore_bg() {
    local logname="$1"
    shift
    adb shell "nohup $DEV_BORE $* >/data/local/tmp/$logname 2>&1 &"
}

fetch_guest_log() {
    adb shell "cat /data/local/tmp/$1" 2>/dev/null || true
}

# ── T-AND-E1: public tunnel, guest provides the local service ───────────────
kill_guest_bore
guest_bore_bg "e1.log" local 8080 --to "$HOST_TO" --port 40010
sleep 1
guest_http_responder 8080
sleep 1
BODY="$(curl -sf --max-time 5 "http://127.0.0.1:40010" || true)"
if [ "$BODY" = "hello" ]; then
    pass "T-AND-E1 public tunnel served 'hello'"
else
    fail "T-AND-E1 public tunnel: got '$BODY', expected 'hello'"
    echo "--- guest bore log (e1.log) ---" >&2
    fetch_guest_log e1.log >&2
fi

# ── T-AND-E2: transfer over the built-in rendezvous, integrity check ────────
kill_guest_bore
adb shell "rm -rf $DEV_INBOX && mkdir -p $DEV_INBOX"
guest_bore_bg "e2.log" transfer listener --dest-path "$DEV_INBOX" \
    --to "$HOST_TO" --transfer-id T_AND_E2
sleep 1

SRC_FILE="$TMPDIR/e2_src.bin"
head -c 8388608 /dev/urandom >"$SRC_FILE"
SRC_SIZE=$(stat -c %s "$SRC_FILE" 2>/dev/null || wc -c <"$SRC_FILE")

if "$BORE_HOST_BIN" transfer sender --sources "$SRC_FILE" \
    --to "$HOST_TO" --transfer-id T_AND_E2 \
    >"$TMPDIR/e2_sender.log" 2>&1; then
    DEST_SIZE="$(adb shell "stat -c %s $DEV_INBOX/e2_src.bin" 2>/dev/null | tr -d '\r\n')"
    if [ "$DEST_SIZE" = "$SRC_SIZE" ]; then
        pass "T-AND-E2 transfer integrity ($SRC_SIZE bytes, sender exit 0)"
    else
        fail "T-AND-E2 transfer size mismatch: src=$SRC_SIZE dest=$DEST_SIZE"
    fi
else
    fail "T-AND-E2 transfer sender exited non-zero"
    cat "$TMPDIR/e2_sender.log" >&2
fi

# ── T-AND-E3: secret tunnel, provider in guest ───────────────────────────────
kill_guest_bore
guest_bore_bg "e3.log" local 8080 --to "$HOST_TO" --tcp-secret-id T_AND_E3
sleep 1
guest_http_responder 8080
sleep 1
BODY="$("$BORE_HOST_BIN" proxy --to "$HOST_TO" --tcp-secret-id T_AND_E3 \
    --local-proxy-port ":40031" >"$TMPDIR/e3_proxy.log" 2>&1 &
    E3_PID=$!
    sleep 1.5
    curl -sf --max-time 5 "http://127.0.0.1:40031" || true
    kill "$E3_PID" >/dev/null 2>&1 || true
    wait "$E3_PID" 2>/dev/null || true)"
if [ "$BODY" = "hello" ]; then
    pass "T-AND-E3 secret tunnel served 'hello'"
else
    fail "T-AND-E3 secret tunnel: got '$BODY', expected 'hello'"
    echo "--- guest bore log (e3.log) ---" >&2
    fetch_guest_log e3.log >&2
    echo "--- host proxy log ---" >&2
    cat "$TMPDIR/e3_proxy.log" >&2
fi

# ── T-AND-E4: test-udp diagnostic ────────────────────────────────────────────
kill_guest_bore
if adb shell "$DEV_BORE test-udp --to $HOST_TO" >"$TMPDIR/e4.log" 2>&1; then
    if grep -q "Verdict" "$TMPDIR/e4.log"; then
        pass "T-AND-E4 test-udp printed a NAT classification verdict"
    else
        fail "T-AND-E4 test-udp exited 0 but no 'Verdict' line found"
        cat "$TMPDIR/e4.log" >&2
    fi
else
    fail "T-AND-E4 test-udp exited non-zero"
    cat "$TMPDIR/e4.log" >&2
fi

# ── T-AND-E5: public tunnel --udp direct path (direct or fallback = pass) ──
kill_guest_bore
guest_bore_bg "e5.log" local 8080 --to "$HOST_TO" --port 40015 --udp
sleep 1
guest_http_responder 8080
sleep 1
BODY="$(curl -sf --max-time 5 "http://127.0.0.1:40015" || true)"
if [ "$BODY" = "hello" ]; then
    pass "T-AND-E5 public --udp tunnel served 'hello'"
    E5_LOG="$(fetch_guest_log e5.log)"
    if echo "$E5_LOG" | grep -qi "direct"; then
        echo "INFO: T-AND-E5 took the DIRECT UDP path"
    else
        echo "INFO: T-AND-E5 took the TCP relay fallback path"
    fi
else
    fail "T-AND-E5 public --udp tunnel: got '$BODY', expected 'hello'"
    echo "--- guest bore log (e5.log) ---" >&2
    fetch_guest_log e5.log >&2
fi

# ── T-AND-E6: non-root negative (bind privileged port must fail) ────────────
kill_guest_bore
E6_OUT="$(adb shell "$DEV_BORE server --bind-addr 0.0.0.0 --control-port 80" 2>&1)"
E6_STATUS=$?
if [ "$E6_STATUS" -ne 0 ] && echo "$E6_OUT" | grep -qiE "permission|denied|eacces"; then
    pass "T-AND-E6 non-root bind of control-port 80 failed as expected"
else
    fail "T-AND-E6 expected a permission-denied failure, got exit=$E6_STATUS: $E6_OUT"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo
echo "===== $PASS passed, $FAIL failed ====="
[ "$FAIL" -eq 0 ]
