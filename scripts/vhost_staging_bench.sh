#!/usr/bin/env bash
# vhost STAGING benchmark harness — client-side only, no server access required.
#
# Measures the vhost data plane against a REMOTE bore server (staging), varying
# only client-side flags (--udp, --carriers, --webserver-log, --backend-tls).
# Server configuration is a constant here: this script never changes it.
#
# Server-side truth comes from the admin API in READ-ONLY mode
# (/admin/api/v1/{vhost,metrics}): registration readiness, direct-QUIC stream
# opens (proves the direct path is really in use), RSS, byte counters, rates.
#
# Origin: scripts/bench_origin.py — one write per response on a NODELAY socket.
# A general static server (dufs, python -m http.server) splits head and body and
# pays the 40 ms Nagle/delayed-ACK penalty on small responses, which would be
# misread as tunnel latency (measured: 41 ms p50 on loopback).
#
# Requirements (all local): bore release build, python3, oha, curl, jq.
#
# Usage:
#   BORE_SECRET=... ADMIN_TOKEN=... scripts/vhost_staging_bench.sh <case>...
#
# Cases:
#   tcp-c1 tcp-c2 tcp-c4 tcp-c8     TCP relay, N carriers
#   udp-c1 udp-c2 udp-c4 udp-c8     QUIC direct, N carriers
#   tcp-wlog udp-wlog               + --webserver-log (logging cost)
#   sustained-tcp sustained-udp     SUSTAINED_SECS of continuous download
#   sustained-tcp-cN sustained-udp-cN  same, with N carriers
#                                   (characterizes burstable-instance decay)
#
# Env: HOST TO LABEL_PREFIX ORIGIN_PORT LAT_SECS SUSTAINED_SECS SKIP_BULK OUT
set -uo pipefail

HOST="${HOST:-brp.0912345.xyz}"
TO="${TO:-https://$HOST}"
LABEL_PREFIX="${LABEL_PREFIX:-bench}"
ORIGIN_PORT="${ORIGIN_PORT:-5052}"
LAT_SECS="${LAT_SECS:-10}"
SUSTAINED_SECS="${SUSTAINED_SECS:-45}"
SKIP_BULK="${SKIP_BULK:-0}"
ADMIN="https://$HOST/admin/api/v1"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BORE="${BORE:-$ROOT/target/release/bore}"
OUT="${OUT:-$ROOT/target/vhost-staging-bench}"
ORIGIN="${ORIGIN:-$ROOT/scripts/bench_origin.py}"
STAMP="$(date +%Y%m%d-%H%M%S)"

: "${BORE_SECRET:?set BORE_SECRET}"
: "${ADMIN_TOKEN:?set ADMIN_TOKEN (read-only admin API access)}"
[ -x "$BORE" ] || { echo "missing $BORE — cargo build --release" >&2; exit 1; }
for t in python3 oha curl jq; do command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 1; }; done
mkdir -p "$OUT"
RESULTS="$OUT/results-$STAMP.jsonl"
: > "$RESULTS"

PIDS=()
cleanup() {
    set +e
    # Only processes started here. Never a blanket pkill: it would kill unrelated
    # tunnels running on the workstation.
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

adm() { curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN/$1"; }
vfield() { adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }

start_origin() {
    curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$ORIGIN_PORT/ping" 2>/dev/null && return 0
    python3 "$ORIGIN" "$ORIGIN_PORT" > "$OUT/origin.log" 2>&1 &
    PIDS+=("$!")
    for _ in $(seq 40); do
        curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$ORIGIN_PORT/ping" 2>/dev/null && return 0
        sleep 0.25
    done
    echo "origin failed to start" >&2; return 1
}

# start_tunnel <label> <flags...> ; echoes the client pid
start_tunnel() {
    local label="$1"; shift
    "$BORE" vhost "127.0.0.1:$ORIGIN_PORT" --subdomain "$label" --id "$label" \
        --to "$TO" --secret "$BORE_SECRET" "$@" > "$OUT/$label.client.log" 2>&1 &
    local pid=$!
    PIDS+=("$pid")
    for _ in $(seq 60); do
        # Readiness from the server's own view, not from a client log line.
        adm vhost 2>/dev/null | jq -e --arg l "$label" 'any(.[]; .subdomain==$l)' >/dev/null && { echo "$pid"; return 0; }
        kill -0 "$pid" 2>/dev/null || { echo "client died: $(tail -2 "$OUT/$label.client.log")" >&2; return 1; }
        sleep 0.5
    done
    echo "timeout registering $label" >&2; return 1
}

stop_tunnel() { # <pid> <label>: stop and wait for the server to drop the entry
    local pid="$1" label="$2"
    kill "$pid" 2>/dev/null
    for _ in $(seq 40); do
        adm vhost | jq -e --arg l "$label" 'any(.[]; .subdomain==$l)' >/dev/null || return 0
        sleep 0.5
    done
    echo "WARNING: entry $label still registered 20s after provider death (zombie)" >&2
}

# per-second server-side samples for the duration of a block
start_sampler() {
    local file="$1" label="$2"
    : > "$file"
    ( local prev=""
      while :; do
        local j; j="$(adm vhost 2>/dev/null | jq -c --arg l "$label" '.[]|select(.subdomain==$l)|{tx:.relay_tx_bytes,rx:.relay_rx_bytes,active:.active}' 2>/dev/null)"
        local m; m="$(adm metrics 2>/dev/null | jq -c '{rss:.mem_rss_bytes,df:.direct_fallbacks,cr:.conn_rejections}' 2>/dev/null)"
        [ -n "$j" ] && echo "{\"t\":$(date +%s),\"v\":$j,\"m\":${m:-null}}" >> "$file"
        sleep 1
      done ) >/dev/null 2>&1 &
    SAMPLER_PID=$!
    PIDS+=("$SAMPLER_PID")
}

sampler_rates() { # max/mean of per-second tx delta, in MB/s, from the jsonl
    jq -s '[ . as $a | range(1; ($a|length)) | ($a[.].v.tx - $a[.-1].v.tx) / (($a[.].t - $a[.-1].t)|if .==0 then 1 else . end) ]
           | if length==0 then {tx_mb_max:null,tx_mb_mean:null} else
             {tx_mb_max:((max/1048576*100|round)/100), tx_mb_mean:((add/length/1048576*100|round)/100)} end' "$1" 2>/dev/null || echo '{}'
}
sampler_peaks() {
    jq -s '{rss_max:([.[].m.rss]|map(select(.!=null))|max), active_max:([.[].v.active]|max),
            fallbacks:([.[].m.df]|map(select(.!=null))|max), rejections:([.[].m.cr]|map(select(.!=null))|max)}' "$1" 2>/dev/null || echo '{}'
}

lat() { # <url> <conns> [extra oha flags]
    local url="$1" conns="$2"; shift 2
    timeout $((LAT_SECS + 30)) oha -z "${LAT_SECS}s" -c "$conns" --no-tui --output-format json "$@" "$url" 2>/dev/null \
      | jq -c '{rps:((.summary.requestsPerSec*10|round)/10), p50:.metrics.latency_ms.p50, p95:.metrics.latency_ms.p95,
                p99:.metrics.latency_ms.p99, max:.metrics.latency_ms.max, ok:.summary.successRate,
                mb_s:((.summary.sizePerSec/1048576*100|round)/100), bytes:.summary.totalData,
                codes:.statusCodeDistribution, errs:.errorDistribution}' 2>/dev/null || echo '{"err":"empty"}'
}

bulk() { # <url>
    timeout 320 curl -fsS -o /dev/null -m 300 -w '{"mb_s":%{speed_download},"ttfb":%{time_starttransfer},"total":%{time_total},"got":%{size_download}}' "$1" \
      | jq -c '.mb_s=((.mb_s/1048576*100|round)/100)' 2>/dev/null || echo '{"err":"empty"}'
}

par() { # <url> <conns> <reqs>
    timeout 320 oha -n "$3" -c "$2" --no-tui --output-format json "$1" 2>/dev/null \
      | jq -c '{mb_s:((.summary.sizePerSec/1048576*100|round)/100), rps:((.summary.requestsPerSec*100|round)/100),
                p95:.metrics.latency_ms.p95, total_s:((.summary.total*100|round)/100), ok:.summary.successRate}' 2>/dev/null || echo '{"err":"empty"}'
}

upload() { # <url> <bytes>
    head -c "$2" /dev/zero > "$OUT/up.bin"
    timeout 320 curl -fsS -o /dev/null -m 300 -X PUT -T "$OUT/up.bin" \
      -w '{"mb_s":%{speed_upload},"total":%{time_total}}' "$1" \
      | jq -c '.mb_s=((.mb_s/1048576*100|round)/100)' 2>/dev/null || echo '{"err":"empty"}'
}

run_case() {
    local case="$1" label="$2"
    local flags=() carriers=1 sustained=0
    case "$case" in
        tcp-c*)        carriers="${case#tcp-c}" ;;
        udp-c*)        carriers="${case#udp-c}"; flags+=(--udp) ;;
        tcp-wlog)      flags+=(--webserver-log) ;;
        udp-wlog)      flags+=(--udp --webserver-log) ;;
        sustained-tcp-c*) sustained=1; carriers="${case#sustained-tcp-c}" ;;
        sustained-udp-c*) sustained=1; carriers="${case#sustained-udp-c}"; flags+=(--udp) ;;
        sustained-tcp) sustained=1 ;;
        sustained-udp) sustained=1; flags+=(--udp) ;;
        *) echo "unknown case: $case" >&2; return 1 ;;
    esac
    flags+=(--carriers "$carriers")

    echo "== $case (label=$label flags=${flags[*]})" >&2
    local pid; pid="$(start_tunnel "$label" "${flags[@]}")" || return 1
    local base="https://$label.$HOST"
    curl -fsS -o /dev/null -m 30 "$base/ping" || { echo "warmup failed" >&2; stop_tunnel "$pid" "$label"; return 1; }

    # Which path is actually carrying data? Server-side counter, not an assumption.
    local d0 d1 path="relay-tcp"
    d0="$(vfield "$label" direct_stream_opens)"
    curl -fsS -o /dev/null -m 30 "$base/100k"
    d1="$(vfield "$label" direct_stream_opens)"
    [ "${d1:-0}" -gt "${d0:-0}" ] && path="direct-quic"

    # Reference: server responsiveness without any tunnel in the path. Lets a
    # latency regression be attributed to the instance rather than to the relay.
    local aref
    aref="$(for _ in 1 2 3 4 5; do curl -fsS -o /dev/null -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" \
              -w '%{time_starttransfer}\n' "$ADMIN/summary"; done | jq -Rrn '[inputs|tonumber]|sort|{admin_ttfb_p50_ms:((.[2]*100000|round)/100)}')"

    start_sampler "$OUT/$case.samples.jsonl" "$label"
    local sp="$SAMPLER_PID"
    step() { echo "   [$(date +%T)] $*" >&2; }
    local res; res="$(jq -nc --arg c "$case" --arg p "$path" --argjson car "$carriers" '{case:$c,path:$p,carriers:$car}')"

    if [ "$sustained" = 1 ]; then
        # One long transfer: shows whether a burstable instance decays mid-run.
        local t0 t1
        t0="$(date +%s)"
        timeout $((SUSTAINED_SECS + 30)) curl -fsS -o /dev/null --max-time "$SUSTAINED_SECS" \
            "$base/stream/$((32 * 1073741824))" -w '' 2>/dev/null
        t1="$(date +%s)"
        res="$(jq -c --argjson s "$(sampler_rates "$OUT/$case.samples.jsonl")" --argjson w "$((t1-t0))" \
                 '. + {sustained:$s, wall_s:$w}' <<<"$res")"
    else
        # latency, keep-alive, tiny object, rising concurrency
        for c in 1 8 32; do
            step "lat keepalive c=$c"
            res="$(jq -c --argjson r "$(lat "$base/1k" "$c")" --arg k "lat_ka_c$c" '.+{($k):$r}' <<<"$res")"
        done
        step "lat new-conn c=8"
        res="$(jq -c --argjson r "$(lat "$base/1k" 8 --disable-keepalive)" '.+{lat_newconn_c8:$r}' <<<"$res")"
        step "asset 100k c=8"
        res="$(jq -c --argjson r "$(lat "$base/100k" 8)" '.+{asset_100k_c8:$r}' <<<"$res")"

        if [ "$SKIP_BULK" != 1 ]; then
            step "bulk single 200m"
            res="$(jq -c --argjson r "$(bulk "$base/200m")" '.+{bulk_single:$r}' <<<"$res")"
            step "parallel 8x10m"
            res="$(jq -c --argjson r "$(par "$base/10m" 8 24)" '.+{bulk_par8:$r}' <<<"$res")"
            step "upload 32m"
            res="$(jq -c --argjson r "$(upload "$base/sink" $((32*1048576)))" '.+{upload_32m:$r}' <<<"$res")"
            step "lat under bulk c=8"
            curl -fsS -o /dev/null -m 120 "$base/200m" & local bg=$!
            PIDS+=("$bg")
            res="$(jq -c --argjson r "$(lat "$base/1k" 8)" '.+{lat_under_bulk_c8:$r}' <<<"$res")"
            kill "$bg" 2>/dev/null
        fi
    fi

    kill "$sp" 2>/dev/null
    res="$(jq -c --argjson s "$(sampler_rates "$OUT/$case.samples.jsonl")" --argjson p "$(sampler_peaks "$OUT/$case.samples.jsonl")" \
             '. + {server_rate:$s, server_peak:$p}' <<<"$res")"
    res="$(jq -c --argjson w "$(grep -ciE 'warn|error' "$OUT/$label.client.log")" '.+{client_warns:$w}' <<<"$res")"
    res="$(jq -c --argjson a "${aref:-{\}}" '.+{server_ref:$a}' <<<"$res")"
    stop_tunnel "$pid" "$label"
    echo "$res" >> "$RESULTS"
    echo "$res"
}

start_origin || exit 1
tx0="$(adm metrics | jq -r .bandwidth_tx_bytes)"
i=0
for c in "$@"; do
    i=$((i+1))
    run_case "$c" "${LABEL_PREFIX}$(date +%s)$i" || echo "case $c FAILED" >&2
done
tx1="$(adm metrics | jq -r .bandwidth_tx_bytes)"
echo >&2
echo "server egress for this run: $(awk -v a="$tx0" -v b="$tx1" 'BEGIN{printf "%.2f GB", (b-a)/1073741824}')" >&2
echo "results: $RESULTS" >&2
