#!/usr/bin/env bash
# Measure the staging server's CPU cost per GB, which is the number that
# predicts how many cores any target link rate needs.
# Prints epoch windows so the host /proc/stat sampler can be matched to each
# case -- the container's own CPU% misses the softirq the host kernel spends on
# its behalf, and softirq is 37-40% of the bill.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OP=5052; OUT=$H/out; GW=$GW
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
txb(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.relay_tx_bytes'; }
dso(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.direct_stream_opens'; }
curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping || { setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null & sleep 2; }

case_run(){ # <tag> <flags...>
  local tag=$1; shift
  local L=ef$(date +%s%N | cut -c6-13)
  # shellcheck disable=SC2086
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$L.log 2>&1 &
  local P=$!; KIDS+=("$P")
  for i in $(seq 60); do present "$L" && break; kill -0 $P 2>/dev/null || break; sleep 0.5; done
  present "$L" || { echo "$tag REGISTRATION FAILED"; return 1; }
  curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
  local d0 a b t0 t1 d1
  d0=$(dso "$L")
  sleep 4                      # let the sampler see an idle baseline first
  a=$(txb "$L"); t0=$(date +%s)
  curl -fsS -o /dev/null --max-time 20 "https://$L.$GW/stream/$((64*1073741824))" 2>/dev/null
  t1=$(date +%s); b=$(txb "$L"); d1=$(dso "$L")
  local path=relay-tcp; [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct-quic
  LC_ALL=C awk -v t="$tag" -v p="$path" -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{
    printf "CASE %s path=%s window=%d-%d dur=%ds bytes=%d rate=%.2f MB/s gb=%.3f\n", t, p, s, e, e-s, b-a, (b-a)/1048576/(e-s), (b-a)/1073741824}'
  kill -9 $P 2>/dev/null
  for i in $(seq 20); do present "$L" || break; sleep 1; done
  sleep 5                      # idle gap so consecutive windows are separable
}

echo "START $(date +%s)"
for r in 1 2 3; do
  case_run "relay-r$r" --carriers 1
  case_run "quic-r$r" --carriers 1 --udp
done
echo "END $(date +%s)"
