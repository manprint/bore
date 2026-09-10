#!/usr/bin/env bash
# G9 slow-reader ladder across direct-UDP window profiles — phase 02.4.
#
# WHAT QUESTION THIS ANSWERS (open question 6 of the vhost staging campaign):
#   The 256 MiB / 16 MiB connection/stream window pair removed the carriers=1
#   stall cliff. A small-host profile shrinks both. Does the cliff stay away?
#   That has to be MEASURED, because the property that prevents the stall is
#   the RATIO (how many stalled streams a connection tolerates), and shrinking
#   both windows keeps the ratio while lowering the absolute tolerance.
#
# METHOD
#   For each window profile, run the G9 ladder: N slow readers pinned on a file
#   larger than one stream window, then a single fast small request timed under
#   that load. Record its latency, the server's RSS, and whether it stalled.
#   A profile "shows a stall" at a rung if the fast request needs LAT_MAX or
#   more seconds — the same criterion scripts/vhost_udp_concurrency_repro.sh
#   uses for its R3 gate.
#
#   Runs against a PRIVATE server in a netns, never against staging: staging is
#   frozen by operator decision, and this test is precisely the shape that once
#   knocked an unrelated tunnel offline there.
#
# TOPOLOGY (same as scripts/vhost_udp_concurrency_repro.sh)
#   ns0 = bore server, nsp = provider + origin, nsc = the browser-like client.
#
# PROFILES
#   default     the shipped 256/16 MiB pair (the control)
#   budget512   --udp-memory-budget 512MiB --max-carriers 4  -> 128/8 MiB
#   budget128   --udp-memory-budget 128MiB --max-carriers 4  -> 32/2 MiB
#   ratio8      explicit 64/8 MiB — the 8:1 pair the plan HYPOTHESISED, kept so
#               the measurement can distinguish "small windows" from "small
#               ratio". The shipped derivation always holds 16:1.
#
# Usage: sudo -n /abs/path/scripts/vhost_udp_window_ladder.sh [options]
#        (exact-path sudo only; `sudo bash scripts/...` prompts)
#   --rungs "4 8 16 24 32"                    slow-reader counts to walk
#   --profiles "default budget512 ..."        window profiles to compare
#   --lat-max 3.0                             stall threshold, seconds
#   --ramp 8                                  seconds for slow readers to pin
#
# Options, not environment variables: sudo's env_reset drops exported settings,
# so a `VAR=x sudo script` invocation would silently run the defaults instead.
# Exit:  0 = every profile served every rung inside --lat-max
set -uo pipefail

BORE="${BORE:-$(cd "$(dirname "$0")/.." && pwd)/target/release/bore}"
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
[ "$(id -u)" = 0 ] || { echo "ERROR: needs root for netns" >&2; exit 1; }
for tool in curl python3 ip awk; do
    command -v "$tool" >/dev/null || { echo "ERROR: $tool required" >&2; exit 1; }
done

SECRET="udpladder$(shuf -i 1000-9999 -n1 2>/dev/null || echo 1234)"
SRV_PROV="10.221.0.2"; PROV="10.221.0.1"
SRV_CLI="10.223.0.2";  CLI="10.223.0.1"
LOG=$(mktemp -d)
ORIGIN_PORT=8891
RESULTS="$LOG/results.tsv"

RUNGS="4 8 16 24 32"
PROFILES="default budget512 budget128 ratio8"
BIG_MIB=48                     # > one stream window at every profile
SMALL_KIB=64
SLOW_RATE=8k
FAST_TIMEOUT=30
LAT_MAX="3.0"                  # same stall criterion as the R3 gate
RAMP=8                         # seconds for the slow readers to pin the window

while [ $# -gt 0 ]; do
    case "$1" in
        --rungs)    RUNGS="$2"; shift 2 ;;
        --profiles) PROFILES="$2"; shift 2 ;;
        --lat-max)  LAT_MAX="$2"; shift 2 ;;
        --ramp)     RAMP="$2"; shift 2 ;;
        --big-mib)  BIG_MIB="$2"; shift 2 ;;
        -h|--help)  sed -n '1,40p' "$0"; exit 0 ;;
        *)          echo "ERROR: unknown option $1" >&2; exit 1 ;;
    esac
done
echo "config: rungs=[$RUNGS] profiles=[$PROFILES] lat_max=${LAT_MAX}s ramp=${RAMP}s"

SERVER_PID=""
declare -a KILL_PIDS=()

cleanup() {
    set +e
    # Only explicit PIDs and this harness's own netns — never a blanket pkill,
    # which is not netns-scoped and would kill the operator's own tunnels.
    for p in "${KILL_PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
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

server_rss_kib() {
    [ -n "$SERVER_PID" ] || { echo 0; return; }
    awk '/^VmRSS:/{print $2}' "/proc/$SERVER_PID/status" 2>/dev/null || echo 0
}

# ── Topology ──────────────────────────────────────────────────────────────────
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

dd if=/dev/urandom of="$LOG/big.bin"   bs=1M count="$BIG_MIB" 2>/dev/null
dd if=/dev/urandom of="$LOG/small.bin" bs=1K count="$SMALL_KIB" 2>/dev/null
SMALL_SHA=$(sha256sum "$LOG/small.bin" | awk '{print $1}')
ip netns exec nsp python3 -m http.server "$ORIGIN_PORT" --bind 127.0.0.1 \
    --directory "$LOG" >"$LOG/origin.log" 2>&1 &
KILL_PIDS+=($!)
sleep 0.5

# profile_flags <name> -> server-side window flags for that profile
profile_flags() {
    case "$1" in
        default)   echo "" ;;
        budget512) echo "--udp-memory-budget 512MiB --max-carriers 4" ;;
        budget128) echo "--udp-memory-budget 128MiB --max-carriers 4" ;;
        ratio8)    echo "--udp-connection-receive-window 64MiB --udp-stream-receive-window 8MiB" ;;
        *)         echo "UNKNOWN" ;;
    esac
}

start_server() { # profile
    local flags; flags=$(profile_flags "$1")
    [ "$flags" = "UNKNOWN" ] && { echo "ERROR: unknown profile $1" >&2; exit 1; }
    RUST_LOG="${RUST_LOG:-info}" ip netns exec ns0 "$BORE" server \
        --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
        --secret "$SECRET" \
        --vhost-base-domain bore.local \
        --vhost-http-port 80 --vhost-quic-port 443 --udp \
        $flags >"$LOG/server.$1.log" 2>&1 &
    SERVER_PID=$!
    wait_for_log "$LOG/server.$1.log" "shared QUIC direct endpoint listening" 15 \
        || { echo "  server did not start for profile $1"; return 1; }
    # Report the windows the server actually chose, so the table is grounded in
    # what ran rather than in what the flags were meant to mean.
    grep -o 'connection_window_mib=[0-9]* stream_window_mib=[0-9]*' \
        "$LOG/server.$1.log" | tail -1
    return 0
}

stop_server() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
    SERVER_PID=""
    sleep 1
}

# rung <profile> <slow_n>
rung() {
    local profile="$1"
    local slow_n="$2"
    local sub="l${slow_n}"
    local logf="$LOG/$profile.$sub.provider.log"

    RUST_LOG="${RUST_LOG:-info}" ip netns exec nsp "$BORE" vhost 127.0.0.1:"$ORIGIN_PORT" \
        --subdomain "$sub" --id "ladder-$sub" \
        --to "$SRV_PROV:7835" --secret "$SECRET" --udp \
        >"$logf" 2>&1 &
    local prov=$!
    KILL_PIDS+=("$prov")
    if ! wait_for_log "$logf" "direct udp carrier ready" 15; then
        printf '%s\t%s\t%s\t%s\t%s\n' "$profile" "$slow_n" "NO-DIRECT" "-" "-" >>"$RESULTS"
        kill "$prov" 2>/dev/null; sleep 1; return
    fi
    ip netns exec nsc curl -s -o /dev/null -m 10 \
        -H "Host: $sub.bore.local" "http://$SRV_CLI/small.bin" >/dev/null 2>&1

    local slow=()
    for _ in $(seq 1 "$slow_n"); do
        ip netns exec nsc curl -s -o /dev/null --limit-rate "$SLOW_RATE" -m 180 \
            -H "Host: $sub.bore.local" "http://$SRV_CLI/big.bin" &
        slow+=($!)
    done
    sleep "$RAMP"
    local rss; rss=$(server_rss_kib)

    local t0 t1 dt code
    t0=$(date +%s.%N)
    code=$(ip netns exec nsc curl -s -o "$LOG/$sub.out" -w '%{http_code}' \
             -m "$FAST_TIMEOUT" -H "Host: $sub.bore.local" \
             "http://$SRV_CLI/small.bin" 2>/dev/null)
    t1=$(date +%s.%N)
    dt=$(awk "BEGIN{printf \"%.2f\", $t1-$t0}")

    local sha=""; [ -f "$LOG/$sub.out" ] && sha=$(sha256sum "$LOG/$sub.out" | awk '{print $1}')
    local verdict="ok"
    if [ "$code" != "200" ] || [ "$sha" != "$SMALL_SHA" ]; then verdict="HUNG"
    elif [ "$(awk "BEGIN{print ($dt >= $LAT_MAX) ? 1 : 0}")" = "1" ]; then verdict="STALL"
    fi

    for sp in "${slow[@]}"; do kill "$sp" 2>/dev/null; done
    kill "$prov" 2>/dev/null
    sleep 2

    printf '%s\t%s\t%s\t%s\t%s\n' "$profile" "$slow_n" "$dt" \
        "$(awk -v k="$rss" 'BEGIN{printf "%.1f", k/1024}')" "$verdict" >>"$RESULTS"
    echo "  $profile / $slow_n slow readers: fast=${dt}s rss=$(awk -v k="$rss" 'BEGIN{printf "%.1f", k/1024}')MiB $verdict"
}

: > "$RESULTS"
declare -A WINDOWS
for profile in $PROFILES; do
    echo ""
    echo "=== Profile $profile ($(profile_flags "$profile" | sed 's/^$/shipped defaults/')) ==="
    if ! start_server "$profile"; then
        echo "  SKIPPED"
        continue
    fi
    WINDOWS[$profile]=$(grep -o 'connection_window_mib=[0-9]*' "$LOG/server.$profile.log" | tail -1)
    for n in $RUNGS; do rung "$profile" "$n"; done
    stop_server
done

echo ""
echo "### G9 slow-reader ladder — fast-request latency under load"
echo ""
printf '| profile |'; for n in $RUNGS; do printf ' %s readers |' "$n"; done; echo ""
printf '| --- |'; for _ in $RUNGS; do printf ' --- |'; done; echo ""
for profile in $PROFILES; do
    printf '| %s |' "$profile"
    for n in $RUNGS; do
        line=$(awk -F'\t' -v p="$profile" -v n="$n" '$1==p && $2==n {print $3" "$5}' "$RESULTS")
        [ -z "$line" ] && line="— —"
        printf ' %s |' "$(echo "$line" | awk '{printf "%ss %s", $1, ($2=="ok"?"":$2)}')"
    done
    echo ""
done

echo ""
echo "### Server RSS at each rung (MiB)"
echo ""
printf '| profile |'; for n in $RUNGS; do printf ' %s readers |' "$n"; done; echo ""
printf '| --- |'; for _ in $RUNGS; do printf ' --- |'; done; echo ""
for profile in $PROFILES; do
    printf '| %s |' "$profile"
    for n in $RUNGS; do
        v=$(awk -F'\t' -v p="$profile" -v n="$n" '$1==p && $2==n {print $4}' "$RESULTS")
        printf ' %s |' "${v:-—}"
    done
    echo ""
done

BAD=$(awk -F'\t' '$5!="ok"' "$RESULTS" | wc -l)
echo ""
if [ "$BAD" -eq 0 ]; then
    echo "RESULT: every profile served every rung under ${LAT_MAX}s — no stall cliff."
else
    echo "RESULT: $BAD rung(s) stalled or hung:"
    awk -F'\t' '$5!="ok"{printf "  %s at %s readers: %s (%ss)\n", $1, $2, $5, $3}' "$RESULTS"
fi
[ "$BAD" -eq 0 ]
