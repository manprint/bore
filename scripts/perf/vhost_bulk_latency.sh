#!/usr/bin/env bash
# F-15 follow-up: does giving the tunnel more carriers rescue small-request
# latency while a bulk transfer is in flight?
#
# The relay pins one proxied connection to one carrier, round-robin per
# connection. So with N carriers a small request has a 1/N chance of landing on
# the carrier the bulk transfer is saturating -- if the mechanism is carrier
# saturation, latency should improve with N. If it does not improve, the
# bottleneck is elsewhere (the server's own scheduling, or --max-conns).
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OHA=$H/oha; OP=5052; OUT=$H/out; mkdir -p $OUT
GW=${BORE_HOST}
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
lab(){ echo "$1$(date +%s%N | cut -c6-13)"; }
lat(){ timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$1.$GW/1k" 2>/dev/null \
       | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99)"'; }

curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping || { setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null & sleep 2; }

echo "F-15 follow-up: small-request latency with and without a bulk transfer in flight"
echo
for spec in "relay-tcp c=1::--carriers 1" "relay-tcp c=2::--carriers 2" "relay-tcp c=4::--carriers 4" \
            "relay-tcp c=8::--carriers 8" "direct-quic c=1::--carriers 1 --udp" "direct-quic c=4::--carriers 4 --udp"; do
  tag="${spec%%::*}"; flags="${spec##*::}"
  L=$(lab bl)
  # shellcheck disable=SC2086
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" $flags > $OUT/$L.log 2>&1 &
  P=$!; KIDS+=("$P")
  ok=0; for i in $(seq 60); do present "$L" && { ok=1; break; }; kill -0 $P 2>/dev/null || break; sleep 0.5; done
  [ $ok = 1 ] || { echo "  $tag REGISTRATION FAILED"; continue; }
  curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
  echo "  $tag idle      : $(lat "$L")"
  curl -s -o /dev/null -m 40 "https://$L.$GW/stream/$((8*1073741824))" & BK=$!; KIDS+=("$BK")
  sleep 3
  echo "  $tag under bulk: $(lat "$L")"
  kill -9 $BK 2>/dev/null
  # and with TWO bulk transfers, to see whether it is per-carrier or global
  curl -s -o /dev/null -m 40 "https://$L.$GW/stream/$((8*1073741824))" & B1=$!; KIDS+=("$B1")
  curl -s -o /dev/null -m 40 "https://$L.$GW/stream/$((8*1073741824))" & B2=$!; KIDS+=("$B2")
  sleep 3
  echo "  $tag under 2x  : $(lat "$L")"
  kill -9 $B1 $B2 $P 2>/dev/null
  for i in $(seq 20); do present "$L" || break; sleep 1; done
  echo
done
echo DONE
