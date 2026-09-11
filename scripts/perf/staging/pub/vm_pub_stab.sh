#!/usr/bin/env bash
# P7: stability, recovery and leak behaviour of a public tunnel under real
# network conditions. Throughput is NOT the subject here; every arm asks a
# yes/no question about correctness that a benchmark cannot answer.
#
#   s1 soak        60 min of steady low-rate traffic; the tunnel must keep the
#                  SAME public port, never fall back permanently, and the
#                  server's RSS must not climb.
#   s2 recover     kill the QUIC path (drop UDP), watch the fallback, restore
#                  it, and measure how long until the direct pool is whole.
#                  Before P-7 this never recovered at all.
#   s3 reconnect   kill the client hard; the port must be released and
#                  re-grantable, and --auto-reconnect must take it back.
#   s4 zombie      the P-4 case: wedge the client with SIGSTOP (TCP stays up,
#                  the process answers nothing) and check the port is reaped.
#   s5 churn       500 short connections; no permit leak, no fd leak.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

# MetricsView's RSS field is `mem_rss_bytes` (verified against
# src/admin_views.rs — guessing a field name cost a whole smoke run once).
srss() { adm metrics 2>/dev/null | jq -r '.mem_rss_bytes // empty' 2>/dev/null; }
mfld() { adm metrics 2>/dev/null | jq -r --arg f "$1" '.[$f] // empty' 2>/dev/null; }

s1_soak() {
    local mins="${SOAK_MIN:-60}"
    echo "===== S1 soak: ${mins} min, steady low rate, QUIC direct ====="
    up_native "$RP" 0 --carriers 1 --udp || { echo "  REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID" port0="$LASTPORT"
    python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
    local end=$(( $(date +%s) + mins * 60 )) n=0 relay=0
    local rss0; rss0=$(srss)
    echo "  server RSS at start: ${rss0:-?} bytes"
    echo "  t   port opens fb  pool path  active  8MiB_MBs"
    while [ "$(date +%s)" -lt "$end" ]; do
        local r; r=$(raw_get "$p" $((8 * 1048576)) 1)
        n=$((n + 1))
        local snap; snap=$(tsnap "$p")
        local path; path=$(printf '%s' "$snap" | jq -r '.current_path // "?"')
        [ "$path" = relay ] && relay=$((relay + 1))
        printf '  %-4s %s %s\n' "$((n))" \
            "$(printf '%s' "$snap" | jq -r '"\(.public_port) \(.direct_stream_opens) \(.direct_fallbacks) \(.direct_pool) \(.current_path) \(.active)"')" \
            "${r:-0}"
        sleep 25
    done
    local p1; p1=$(tfld "$p" public_port)
    local rss1; rss1=$(srss)
    echo "  samples=$n on_relay=$relay port_start=$port0 port_end=${p1:-gone}"
    echo "  server RSS start=${rss0:-?} end=${rss1:-?} delta=$(( ${rss1:-0} - ${rss0:-0} )) bytes"
    # A leak shows as a monotonically climbing RSS across a soak that moves the
    # same bytes every 25 s. Quoted, not judged: the server also serves the
    # operator's own unrelated tunnels, so a threshold here would be noise."
    [ "${p1:-}" = "$port0" ] && echo "  PASS: the public port never moved" \
        || echo "  FAIL: the public port changed or the tunnel died"
    down "$pid" "$p"
}

s2_recover() {
    echo "===== S2 direct-path loss and recovery (P-7) ====="
    echo "  Needs root to drop UDP toward the server's QUIC port."
    local q="${QUIC_PORT:-443}"
    up_native "$RP" 0 --carriers 1 --udp || { echo "  REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID"
    python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
    echo "  before: $(tsnap "$p" | jq -r '"path=\(.current_path) opens=\(.direct_stream_opens) pool=\(.direct_pool)"')"
    sudo -n iptables -I OUTPUT -p udp --dport "$q" -j DROP 2>/dev/null || {
        echo "  SKIP: cannot install the iptables rule"; down "$pid" "$p"; return 0; }
    local t0; t0=$(date +%s)
    local fell=""
    for i in $(seq 30); do
        python3 "$RAWCLI" get "$GW" "$p" 65536 1 5 >/dev/null 2>&1
        local path; path=$(tfld "$p" current_path)
        [ "$path" = relay ] && { fell=$(( $(date +%s) - t0 )); break; }
        sleep 2
    done
    echo "  fell back to the relay after: ${fell:-never}s"
    sudo -n iptables -D OUTPUT -p udp --dport "$q" -j DROP 2>/dev/null
    local t1; t1=$(date +%s) back=""
    for i in $(seq 60); do
        python3 "$RAWCLI" get "$GW" "$p" 65536 1 5 >/dev/null 2>&1
        local path; path=$(tfld "$p" current_path)
        [ "$path" = direct ] && { back=$(( $(date +%s) - t1 )); break; }
        sleep 2
    done
    echo "  direct path back after: ${back:-never}s"
    echo "  after:  $(tsnap "$p" | jq -r '"path=\(.current_path) opens=\(.direct_stream_opens) fb=\(.direct_fallbacks) pool=\(.direct_pool)"')"
    [ -n "$back" ] && echo "  PASS: the tunnel recovered its direct path in place" \
        || echo "  FAIL: still degraded (this is exactly the P-7 symptom)"
    down "$pid" "$p"
}

s3_reconnect() {
    echo "===== S3 hard client death and port release ====="
    up_native "$RP" 0 --carriers 1 || { echo "  REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID"
    kill -9 "$pid" 2>/dev/null
    local t0; t0=$(date +%s) freed=""
    for i in $(seq 60); do
        present "$p" || { freed=$(( $(date +%s) - t0 )); break; }
        sleep 1
    done
    echo "  port $p released after: ${freed:-never}s"
    # And it must be usable again, not merely absent from the listing.
    if up_native "$RP" "$p" --carriers 1; then
        echo "  PASS: the freed port accepted a fresh tunnel"
        down "$LASTPID" "$LASTPORT"
    else
        echo "  FAIL: the freed port refused a fresh tunnel"
    fi
}

s4_zombie() {
    echo "===== S4 wedged client (SIGSTOP) — the P-4 case ====="
    echo "  A stopped process keeps its TCP connection ALIVE and answers"
    echo "  nothing: the exact shape a frozen laptop presents. Without the"
    echo "  control-liveness reaper the public port is held until the server"
    echo "  restarts. The reaper's production deadline is 60 s, so this waits"
    echo "  past it."
    up_native "$RP" 0 --carriers 1 || { echo "  REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID"
    kill -STOP "$pid" 2>/dev/null
    local t0; t0=$(date +%s) freed=""
    for i in $(seq 150); do
        present "$p" || { freed=$(( $(date +%s) - t0 )); break; }
        sleep 1
    done
    kill -CONT "$pid" 2>/dev/null; kill -9 "$pid" 2>/dev/null
    echo "  port $p reaped after: ${freed:-never (still held after 150s)}"
    [ -n "$freed" ] && echo "  PASS: the wedged client was reaped" \
        || echo "  FAIL: zombie public port (P-4)"
}

s5_churn() {
    echo "===== S5 connection churn: 500 short connections ====="
    up_native "$RP" 0 --carriers 4 || { echo "  REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID"
    local a; a=$(tfld "$p" active)
    local t0; t0=$(date +%s)
    for i in $(seq 25); do
        python3 "$RAWCLI" get "$GW" "$p" 4096 20 20 >/dev/null 2>&1
    done
    local t1; t1=$(date +%s)
    sleep 5
    local b; b=$(tfld "$p" active)
    echo "  500 connections in $((t1 - t0))s; active before=$a after=$b"
    echo "  server rejections=$(mfld conn_rejections) budget_refusals=$(mfld direct_budget_refusals)"
    [ "${b:-1}" = 0 ] && echo "  PASS: every permit came back" \
        || echo "  FAIL: $b connections still counted active after the churn"
    # The tunnel must still work after the churn, not merely look idle.
    echo "  post-churn 8 MiB: $(raw_get "$p" $((8 * 1048576)) 1) MB/s"
    down "$pid" "$p"
}

start_origins || exit 1
case "${1:-all}" in
  all)  s2_recover; echo; s3_reconnect; echo; s4_zombie; echo; s5_churn; echo; s1_soak ;;
  soak) s1_soak ;;
  recover) s2_recover ;;
  reconnect) s3_reconnect ;;
  zombie) s4_zombie ;;
  churn) s5_churn ;;
  *) echo "usage: $0 [all|soak|recover|reconnect|zombie|churn]"; exit 2 ;;
esac
echo DONE
