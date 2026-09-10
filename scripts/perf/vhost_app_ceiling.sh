#!/usr/bin/env bash
# Does the APPLICATION limit throughput, independently of the VM it runs on?
#
# A single throughput number from a deployment cannot answer that: it conflates
# the code, the guest CPU and the link. This script removes two of the three --
# a private bore server on THIS machine over loopback, so there is no link worth
# speaking of -- leaving CPU as the only variable. Two pinnings:
#   --cores "0,1"  emulates a 2-core production box
#   --cores all    shows whether bore uses more cores when it has them
#
# The transferable metric is the s/GB column: CPU seconds per GiB moved. Absolute
# MB/s is a property of this machine; s/GB is a property of the code and the CPU,
# so dividing a target link rate by it gives the core count that rate needs.
#
# LOOPBACK MTU IS 65536. Per-packet cost is therefore understated by roughly 8x
# against a 1500-byte path, which inflates absolute MB/s and deflates s/GB.
# Treat the MB/s column as an upper bound and calibrate s/GB against a real
# deployment (scripts/perf/vhost_remote_efficiency.sh) before extrapolating.
#
# Usage:
#   ./vhost_app_ceiling.sh                    # both pinnings, full sweep
#   ./vhost_app_ceiling.sh --cores 0,1        # one pinning only
#   BORE_PERF_OUT=/tmp/x ./vhost_app_ceiling.sh
set -uo pipefail
ROOT="${BORE_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
BORE="${BORE_BIN:-$ROOT/target/release/bore}"
[ -x "$BORE" ] || { echo "no bore binary at $BORE -- cargo build --release" >&2; exit 1; }
PINNINGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --cores) PINNINGS+=("$2"); shift 2 ;;
    *) echo "usage: $0 [--cores 0,1] [--cores all]" >&2; exit 1 ;;
  esac
done
[ ${#PINNINGS[@]} -gt 0 ] || PINNINGS=("0,1" all)
F="${BORE_PERF_OUT:-$(mktemp -d -t bore-ceiling-XXXXXX)}"; mkdir -p "$F"
CP=17836; HP=18081; HSP=18444; OP=15053; SEC=ceilingsecret
OUT=$F/results.txt; : > $OUT
PIDS=()
cleanup(){ for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
log(){ echo "$*" | tee -a $OUT; }

[ -f "$F/cert.pem" ] || openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
  -keyout "$F/key.pem" -out "$F/cert.pem" -subj "/CN=*.lo.test" \
  -addext "subjectAltName=DNS:*.lo.test,DNS:lo.test" 2>/dev/null
# the staging response-header policy, so the measured path is the deployed one
cat > "$F/cfg.yml" <<'YML'
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

curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null || {
  python3 $ROOT/scripts/bench_origin.py $OP > $F/origin.log 2>&1 &
  PIDS+=($!); for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && break; sleep 0.25; done; }

cputime(){ awk '{print ($14+$15)/100}' /proc/$1/stat 2>/dev/null || echo 0; }
nthreads(){ awk '{print $20}' /proc/$1/stat 2>/dev/null || echo 0; }

# <pin> <transport flags> <carriers> <parallel streams>
run(){
  local pin=$1 flags=$2 car=$3 par=$4
  local L=c$RANDOM taskset=()
  [ "$pin" != all ] && taskset=(taskset -c "$pin")
  "${taskset[@]}" $BORE server --secret $SEC --control-port $CP \
    --vhost-config $F/cfg.yml --vhost-http-port $HP --vhost-https-port $HSP \
    --vhost-cert-file $F/cert.pem --vhost-key-file $F/key.pem --udp \
    --vhost-quic-port $HSP > $F/srv-$L.log 2>&1 &
  local SP=$!; PIDS+=($SP)
  # A stale server still holding the control port would serve every request while
  # the process we think we are measuring has already exited with "address in
  # use" -- real throughput, wrong pinning, zero CPU. Verify OUR process is the
  # one that bound the port before measuring anything.
  local bound=0
  for i in $(seq 60); do
    grep -q 'server listening' "$F/srv-$L.log" 2>/dev/null && { bound=1; break; }
    kill -0 $SP 2>/dev/null || break
    sleep 0.25
  done
  if [ $bound = 0 ] || ! kill -0 $SP 2>/dev/null || [ ! -r /proc/$SP/stat ]; then
    log "  pin=$pin flags='$flags' c=$car par=$par SERVER DID NOT START: $(tail -1 "$F/srv-$L.log" 2>/dev/null)"
    kill -9 $SP 2>/dev/null; return 1
  fi
  # shellcheck disable=SC2086
  $BORE vhost 127.0.0.1:$OP --subdomain $L --id $L --to "127.0.0.1:$CP" --secret $SEC \
    --carriers $car $flags > $F/cli-$L.log 2>&1 &
  local CPID=$!; PIDS+=($CPID)
  sleep 3
  local ct=(--connect-to "$L.lo.test:$HSP:127.0.0.1:$HSP" --insecure)
  local url="https://$L.lo.test:$HSP"
  local werr
  werr=$(curl -sS -o /dev/null -m 20 "${ct[@]}" -w 'http=%{http_code}' "$url/100k" 2>&1) || true
  case "$werr" in *http=200*) ;; *)
    log "  pin=$pin flags='$flags' c=$car par=$par WARMUP FAILED: $werr"
    log "      client: $(grep -iE 'error|warn|refus|fail' $F/cli-$L.log | tail -2 | tr '\n' ' ')"
    log "      server: $(grep -iE 'error|warn|refus|fail' $F/srv-$L.log | tail -2 | tr '\n' ' ')"
    kill -9 $CPID $SP 2>/dev/null; return 1 ;;
  esac
  local c0 c1 t0 t1 bytes=0 ps=() sz=$((3*1073741824))
  c0=$(cputime $SP); t0=$(date +%s.%N)
  for i in $(seq $par); do
    curl -s -o /dev/null -m 60 "${ct[@]}" -w '%{size_download}\n' "$url/stream/$sz" > $F/dl.$i & ps+=("$!")
  done
  for p in "${ps[@]}"; do wait "$p" 2>/dev/null; done
  t1=$(date +%s.%N); c1=$(cputime $SP)
  for i in $(seq $par); do bytes=$((bytes + $(cat $F/dl.$i 2>/dev/null || echo 0))); done
  local nt; nt=$(nthreads $SP)
  # was the path direct? the client logs a fallback if not
  local path=relay-tcp
  grep -qi 'direct udp carrier ready' $F/cli-$L.log 2>/dev/null && path=direct-quic
  [ -z "$flags" ] && path=relay-tcp
  log "$(LC_ALL=C awk -v pin="$pin" -v p="$path" -v c="$car" -v par="$par" -v b="$bytes" \
      -v s="$t0" -v e="$t1" -v c0="$c0" -v c1="$c1" -v nt="$nt" 'BEGIN{
        d=e-s; mb=b/1048576/d; gb=b/1073741824; cpu=c1-c0;
        printf "  pin=%-4s %-11s c=%-2s par=%-2s  %8.1f MB/s (%5.2f Gbit/s)  cpu=%5.2fs/%0.2fGB = %5.2f s/GB  cores_used=%4.2f  threads=%s",
        pin, p, c, par, mb, mb*8/1000, cpu, gb, (gb>0?cpu/gb:0), (d>0?cpu/d:0), nt}')"
  kill -9 $CPID $SP 2>/dev/null; wait $CPID 2>/dev/null; wait $SP 2>/dev/null; sleep 1
}

for port in $CP $HP $HSP; do
  if ss -ltn 2>/dev/null | grep -q ":$port "; then
    echo "port $port already in use -- stop the stale listener first:" >&2
    ss -ltnp 2>/dev/null | grep ":$port " >&2
    exit 1
  fi
done

log "===== application ceiling: private server, loopback, CPU pinned ====="
log "  reference lines: 1 Gbit/s = 119 MiB/s;  5 Gbit/s = 596 MiB/s"
log "  NOTE loopback MTU is 65536, so per-packet cost is understated ~8x versus a"
log "  1500-byte path. The s/GB column is the number to carry across; absolute"
log "  MB/s here is an upper bound on what a real NIC would give."
log "  results: $OUT"
# The sweep specs are "<transport flags>|<carriers>|<parallel streams>". The
# separator is "|" and not ":" on purpose: an empty flags field with ":" splits
# into four fields for three variables, silently shifting carriers into parallel
# and killing the client with "a value is required for --carriers".
for pin in "${PINNINGS[@]}"; do
  log ""
  if [ "$pin" = all ]; then
    log "-- all $(nproc) cores: does bore use more cores when it has them --"
    specs=("|1|1" "|1|4" "|4|4" "|4|8" "|8|8" "--udp|1|4" "--udp|4|8")
  else
    log "-- cores $pin: a small production box --"
    specs=("|1|1" "|1|4" "|2|4" "|4|4" "|4|8" "--udp|1|1" "--udp|1|4" "--udp|4|4")
  fi
  for spec in "${specs[@]}"; do
    IFS="|" read -r fl car par <<< "$spec"
    run "$pin" "$fl" "$car" "$par"
  done
done
log ""
log "DONE"
