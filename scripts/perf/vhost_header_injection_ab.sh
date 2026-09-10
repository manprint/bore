#!/usr/bin/env bash
# F-4 A/B: what does the vhost response-header injection path cost?
#
# With default_response_headers set, EVERY vhost response takes
# relay_response_injected + copy_one_direction_with_shutdown (a flush, i.e. a
# write syscall and a TLS record, after every write) instead of
# copy_bidirectional_with_sizes. §2.10 showed 82 % of the server's CPU is
# kernel, so an extra syscall per response is exactly the shape that matters.
#
# Everything runs on loopback on this 16-core workstation: no staging traffic,
# no WiFi, and the server is the only interesting consumer of CPU.
# The metric that answers the question is CPU-microseconds per request, taken
# from the server process's own utime+stime, not wall-clock rps.
set -uo pipefail
# Self-contained: runs a private bore server, vhost client, origin and load
# generator on loopback. Needs no staging access and no credentials.
#   BORE_PERF_OUT  writable directory for the cert, configs and results
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
F="${BORE_PERF_OUT:-$(mktemp -d -t bore-f4-XXXXXX)}"; mkdir -p "$F"
BORE=$ROOT/target/release/bore
CP=17835; HP=18080; HSP=18443; OP=15052
SEC=abtestsecret
OUT=$F/results.txt; : > $OUT
PIDS=()
cleanup(){ for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
log(){ echo "$*" | tee -a $OUT; }

# --- bootstrap the two configs and a self-signed wildcard cert -------------
# The A/B is exactly one line of YAML: default_response_headers present or not.
[ -f "$F/cert.pem" ] || openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
  -keyout "$F/key.pem" -out "$F/cert.pem" -subj "/CN=*.lo.test" \
  -addext "subjectAltName=DNS:*.lo.test,DNS:lo.test" 2>/dev/null
cat > "$F/cfg_noh.yml" <<'YML'
base_domain: lo.test
mode: both
reservations: []
YML
# The seven headers are copied verbatim from the staging server's /config.yml so
# the measurement reflects the deployment, not a synthetic policy.
cat > "$F/cfg_hdr.yml" <<'YML'
base_domain: lo.test
mode: both
default_response_headers:
  X-Frame-Options: "SAMEORIGIN"
  X-XSS-Protection: "1; mode=block"
  Referrer-Policy: "no-referrer-when-downgrade"
  Strict-Transport-Security: "max-age=31536000; includeSubDomains"
  Permissions-Policy: "geolocation=(self), microphone=(self), camera=(self), fullscreen=(self)"
  Content-Security-Policy: "default-src * 'unsafe-inline' 'unsafe-eval' data: blob:;"
  X-Content-Type-Options: "nosniff"
reservations: []
YML

# origin
curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null || {
  python3 "$ROOT/scripts/bench_origin.py" $OP > $F/origin.log 2>&1 &
  PIDS+=($!); for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && break; sleep 0.25; done; }

cputime(){ awk '{print $14+$15}' /proc/$1/stat 2>/dev/null || echo 0; }

run_case(){ # <cfg> <scheme> <label>
  local cfg=$1 scheme=$2 tag=$3 port url
  [ "$scheme" = https ] && port=$HSP || port=$HP
  local L=ab$RANDOM
  $BORE server --secret $SEC --control-port $CP \
    --vhost-config $F/$cfg --vhost-http-port $HP --vhost-https-port $HSP \
    --vhost-cert-file $F/cert.pem --vhost-key-file $F/key.pem \
    > $F/srv-$tag.log 2>&1 &
  local SP=$!; PIDS+=($SP)
  for i in $(seq 40); do ss -ltn 2>/dev/null | grep -q ":$CP " && break; sleep 0.25; done
  $BORE vhost 127.0.0.1:$OP --subdomain $L --id $L --to "127.0.0.1:$CP" --secret $SEC \
    > $F/cli-$tag.log 2>&1 &
  local CPID=$!; PIDS+=($CPID)
  sleep 2
  url="$scheme://$L.lo.test:$port"
  local ct=(--connect-to "$L.lo.test:$port:127.0.0.1:$port" --insecure)
  # prove the header policy is what we think it is
  local hdrs
  hdrs=$(curl -s -o /dev/null -D - "${ct[@]/#--connect-to/--connect-to}" "$url/1k" 2>/dev/null | grep -ciE '^(x-frame-options|x-xss-protection|referrer-policy|strict-transport-security|permissions-policy|content-security-policy|x-content-type-options):' || true)
  local c0 c1 res
  c0=$(cputime $SP)
  res=$(timeout 40 oha -z 20s -c 16 --no-tui "${ct[@]}" --output-format json "$url/1k" 2>/dev/null)
  c1=$(cputime $SP)
  local n rps p50 p99
  # oha 1.16 exposes no request count in .summary; sum the status distribution
  n=$(echo "$res" | jq -r '[.statusCodeDistribution|to_entries[]|.value]|add // 0')
  rps=$(echo "$res" | jq -r '(.summary.requestsPerSec|round)')
  p50=$(echo "$res" | jq -r '.metrics.latency_ms.p50')
  p99=$(echo "$res" | jq -r '.metrics.latency_ms.p99')
  [ "${n:-0}" -gt 0 ] || { log "  $tag OHA PRODUCED NO REQUESTS: $(tail -2 $F/srv-$tag.log)"; kill -9 $CPID $SP 2>/dev/null; return 1; }
  local bulk
  bulk=$(curl -s -o /dev/null -m 40 "${ct[@]}" -w '%{speed_download}' "$url/stream/1073741824")
  log "$(LC_ALL=C awk -v t="$tag" -v h="$hdrs" -v n="$n" -v r="$rps" -v p="$p50" -v q="$p99" \
       -v c0="$c0" -v c1="$c1" -v b="$bulk" 'BEGIN{
         printf "  %-12s injected_hdrs=%d rps=%d p50=%s p99=%s cpu_us_per_req=%.1f bulk=%.1f MB/s",
         t, h, r, p, q, (c1-c0)*10000/n, b/1048576}')"
  kill -9 $CPID $SP 2>/dev/null
  wait $CPID 2>/dev/null; wait $SP 2>/dev/null
  sleep 1
}

log "===== F-4 A/B: cost of the response-header injection path ====="
log "  server, vhost client, origin and load generator all on loopback, 16 cores"
log ""
for scheme in http https; do
  log "-- $scheme, order interleaved to survive drift --"
  for round in 1 2 3; do
    if [ $((round % 2)) = 1 ]; then
      run_case cfg_noh.yml $scheme "r$round-none"
      run_case cfg_hdr.yml $scheme "r$round-inject"
    else
      run_case cfg_hdr.yml $scheme "r$round-inject"
      run_case cfg_noh.yml $scheme "r$round-none"
    fi
  done
  log ""
done
log "DONE"
