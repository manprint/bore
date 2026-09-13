#!/usr/bin/env bash
# vhost benchmark run FROM a fast same-region VM.
#
# Why this exists: from my workstation every clean-path number was capped by a
# ~300 Mbit/s uplink crossed twice over half-duplex WiFi, so the transports were
# indistinguishable. This VM has ~12.5 Gbps and 1.9 ms RTT to the server, which
# is roughly 40x anything a t4g.micro can emit, so double transit on the VM link
# is no longer a constraint and the server becomes the thing being measured.
#
# Usage: vm_bench.sh v0|v1|v2 [seconds]
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"
. $H/env.sh
BORE=$H/bore
OHA=$H/oha
OP=5052
OUT=$H/out; mkdir -p $OUT
SECS="${2:-25}"
PROV=""
cleanup(){ [ -n "$PROV" ] && kill "$PROV" 2>/dev/null; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
log(){ echo "$*"; }
mb(){ awk -v b="$1" 'BEGIN{printf "%7.2f", b/1048576}'; }

origin_up(){
  curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0
  setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null &
  for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0; sleep 0.25; done
  return 1
}

# total CPU busy percentage across all cores, over a window
cpu_start(){ awk '/^cpu /{t=0;for(i=2;i<=NF;i++)t+=$i; print t, $5}' /proc/stat; }
cpu_delta(){ # "<total> <idle>" from cpu_start
  local prev=($1); local now=($(cpu_start))
  awk -v pt=${prev[0]} -v pi=${prev[1]} -v nt=${now[0]} -v ni=${now[1]} \
    'BEGIN{dt=nt-pt; di=ni-pi; if(dt<=0){print "n/a"} else {printf "%.1f%%", 100*(dt-di)/dt}}'
}

up_tunnel(){ # label flags...
  local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain $l --id $l --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$l.log 2>&1 &
  PROV=$!
  for i in $(seq 60); do
    adm vhost 2>/dev/null | jq -e --arg l "$l" 'any(.[]; .subdomain==$l)' >/dev/null && return 0
    kill -0 $PROV 2>/dev/null || { log "    client died: $(tail -2 $OUT/$l.log)"; return 1; }
    sleep 0.5
  done
  return 1
}
down_tunnel(){ kill $PROV 2>/dev/null; PROV=""; sleep 3; }

path_of(){ # label -> relay-tcp | direct-quic
  local l=$1 d0 d1
  d0=$(adm vhost | jq -r --arg l "$l" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  curl -fsS -o /dev/null -m 30 "https://$l.${BORE_HOST}/100k"
  d1=$(adm vhost | jq -r --arg l "$l" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  [ "${d1:-0}" -gt "${d0:-0}" ] && echo direct-quic || echo relay-tcp
}

case "${1:-v0}" in

v0)
  log "===== V0 calibration ====="
  origin_up || { log "origin failed"; exit 1; }
  log "-- origin loopback ceiling (must not be the bottleneck)"
  log "   GET  /stream/2GB : $(mb "$(curl -s -o /dev/null -m 60 -w '%{speed_download}' http://127.0.0.1:$OP/stream/2147483648)") MB/s"
  head -c 33554432 /dev/urandom > $H/up32.bin
  log "   PUT  32 MiB      : $(mb "$(curl -s -o /dev/null -m 60 -X PUT -T $H/up32.bin -H 'Expect:' -w '%{speed_upload}' http://127.0.0.1:$OP/sink)") MB/s"
  log "-- TCP RTT to server:443, 20 handshakes"
  for i in $(seq 20); do curl -s -o /dev/null -w '%{time_connect}\n' --connect-timeout 5 https://${BORE_HOST}/ ; done \
    | LC_ALL=C sort -n | awk '{a[NR]=$1} END{printf "   min=%.2f p50=%.2f p90=%.2f max=%.2f ms\n", a[1]*1000, a[int(NR*0.5)+1]*1000, a[int(NR*0.9)]*1000, a[NR]*1000}'
  log "-- server-owned asset delivery (single transit, server HTTP stack)"
  for c in 1 8 32; do
    log "   /admin/ui/app.js c=$c : $(timeout 30 $OHA -z 8s -c $c --no-tui --output-format json -H "Authorization: Bearer $ADMIN_TOKEN" https://${BORE_HOST}/admin/ui/app.js 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec*10|round)/10) MB/s=\((.summary.sizePerSec/1048576*100|round)/100) p50=\(.metrics.latency_ms.p50)ms ok=\(.summary.successRate)"')"
  done
  ;;

v1)
  log "===== V1 transport A/B, ${SECS}s sustained per case ====="
  origin_up || { log "origin failed"; exit 1; }
  for case in tcp-c1 udp-c1 tcp-c2 udp-c2 tcp-c4 udp-c4 tcp-c1; do
    car=${case##*-c}; flags=(--carriers $car)
    [ "${case%%-*}" = udp ] && flags+=(--udp)
    l=v1$(date +%s%N | cut -c1-13)
    if ! up_tunnel $l "${flags[@]}"; then log "  $case REGISTRATION FAILED"; continue; fi
    curl -fsS -o /dev/null -m 30 "https://$l.${BORE_HOST}/ping" || { log "  $case warmup failed"; down_tunnel; continue; }
    p=$(path_of $l)
    tx0=$(adm vhost | jq -r --arg l "$l" '.[]|select(.subdomain==$l)|.relay_tx_bytes')
    c0=$(cpu_start); t0=$(date +%s.%N)
    curl -fsS -o /dev/null --max-time $SECS "https://$l.${BORE_HOST}/stream/$((64*1073741824))" 2>/dev/null
    t1=$(date +%s.%N); vmcpu=$(cpu_delta "$c0")
    tx1=$(adm vhost | jq -r --arg l "$l" '.[]|select(.subdomain==$l)|.relay_tx_bytes')
    log "  $case path=$p $(awk -v a=$tx0 -v b=$tx1 -v s=$t0 -v e=$t1 'BEGIN{printf "%7.2f MB/s over %.1fs (%.2f GB)", (b-a)/1048576/(e-s), e-s, (b-a)/1073741824}')  vm_cpu=$vmcpu  warns=$(grep -ciE 'warn|error' $OUT/$l.log)"
    down_tunnel
  done
  ;;

v2)
  log "===== V2 latency at 1.9 ms RTT ====="
  origin_up || { log "origin failed"; exit 1; }
  for case in tcp-c1 udp-c1; do
    car=1; flags=(--carriers 1); [ "${case%%-*}" = udp ] && flags+=(--udp)
    l=v2$(date +%s%N | cut -c1-13)
    if ! up_tunnel $l "${flags[@]}"; then log "  $case REGISTRATION FAILED"; continue; fi
    curl -fsS -o /dev/null -m 30 "https://$l.${BORE_HOST}/ping"
    p=$(path_of $l)
    log "  -- $case path=$p"
    log "     direct-to-server reference (no tunnel): $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json -H "Authorization: Bearer $ADMIN_TOKEN" https://${BORE_HOST}/admin/api/v1/metrics 2>/dev/null | jq -r '"p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) rps=\((.summary.requestsPerSec|round))"')"
    for c in 1 8 32; do
      log "     keepalive 1k  c=$c : $(timeout 25 $OHA -z 6s -c $c --no-tui --output-format json "https://$l.${BORE_HOST}/1k" 2>/dev/null | jq -r '"p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99) rps=\((.summary.requestsPerSec|round))"')"
    done
    log "     new-conn 1k   c=8 : $(timeout 25 $OHA -z 6s -c 8 --no-tui --disable-keepalive --output-format json "https://$l.${BORE_HOST}/1k" 2>/dev/null | jq -r '"p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) rps=\((.summary.requestsPerSec|round))"')"
    log "     asset 100k    c=8 : $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$l.${BORE_HOST}/100k" 2>/dev/null | jq -r '"p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) rps=\((.summary.requestsPerSec|round))"')"
    down_tunnel
  done
  ;;
esac
echo "DONE"
