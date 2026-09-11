#!/usr/bin/env bash
# Stage 2 — F-15 / phase 03, the headline. Identical method to the original
# campaign's `vm_bulklat.sh` (so the numbers are directly comparable) plus the
# two cases that did not exist before this plan:
#
#   * `--carriers 0` — the adaptive pool (phase 03.3). The pool starts at 1 and
#     the SERVER asks for more when bulk occupies every carrier, so this is the
#     case the plan claims makes the fix reachable without the operator
#     choosing a number.
#   * the `carriers` / `carrier_target` pair read back from the admin API after
#     the bulk transfers are running, which is the only way to see the pool grow
#     in vivo.
#
# Also records the QUIC-direct arm, where carriers are NOT the mechanism
# (phase 03.4 demotes the bulk stream instead), so the two rows must be read
# differently: on the relay a rising `carriers` is the fix working, on the
# direct path the fix is invisible in the entry and only shows in the latency.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OHA=$H/oha; OP=5052; OUT=$H/out; mkdir -p "$OUT"

DUR="${DUR:-6}"
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
lab(){ echo "$1$(date +%s%N | cut -c6-13)"; }
lat(){ timeout 30 $OHA -z ${DUR}s -c 8 --no-tui --output-format json "https://$1.$GW/1k" 2>/dev/null \
       | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99)"'; }
pool(){ echo "carriers=$(fld "$1" carriers) target=$(fld "$1" carrier_target) path=$(fld "$1" current_path)"; }

curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" || { setsid nohup python3 "$H/bench_origin.py" $OP > "$OUT/origin.log" 2>&1 < /dev/null & sleep 2; }

echo "F-15 / phase 03 in vivo: small-request latency with 0, 1 and 2 bulk transfers in flight"
echo "client=$($BORE --version)  ${DUR}s per latency point, c=8, /1k"
echo
for spec in "relay-tcp c=1::--carriers 1" \
            "relay-tcp c=2::--carriers 2" \
            "relay-tcp c=4::--carriers 4" \
            "relay-tcp c=8::--carriers 8" \
            "relay-tcp c=0auto::--carriers 0" \
            "direct-quic c=1::--carriers 1 --udp" \
            "direct-quic c=4::--carriers 4 --udp" \
            "direct-quic c=0auto::--carriers 0 --udp"; do
  tag="${spec%%::*}"; flags="${spec##*::}"
  L=$(lab bl)
  # shellcheck disable=SC2086
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" $flags > "$OUT/$L.log" 2>&1 &
  P=$!; KIDS+=("$P")
  ok=0; for i in $(seq 60); do present "$L" && { ok=1; break; }; kill -0 $P 2>/dev/null || break; sleep 0.5; done
  [ $ok = 1 ] || { echo "  $tag REGISTRATION FAILED: $(tail -2 "$OUT/$L.log")"; continue; }
  curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
  echo "  $tag idle      : $(lat "$L")   $(pool "$L")"

  curl -s -o /dev/null -m 60 "https://$L.$GW/stream/$((8*1073741824))" & BK=$!; KIDS+=("$BK")
  sleep 4
  echo "  $tag under bulk: $(lat "$L")   $(pool "$L")"
  kill -9 $BK 2>/dev/null

  curl -s -o /dev/null -m 60 "https://$L.$GW/stream/$((8*1073741824))" & B1=$!; KIDS+=("$B1")
  curl -s -o /dev/null -m 60 "https://$L.$GW/stream/$((8*1073741824))" & B2=$!; KIDS+=("$B2")
  sleep 4
  echo "  $tag under 2x  : $(lat "$L")   $(pool "$L")"
  # give the adaptive pool a little longer under load: growth is rate limited to
  # one step per 2 s, so a 4 s window can only ever show two steps.
  sleep 6
  echo "  $tag 2x +6s     : $(pool "$L")  (growth is rate-limited to one step / 2 s)"
  kill -9 $B1 $B2 $P 2>/dev/null
  for i in $(seq 25); do present "$L" || break; sleep 1; done
  echo
done
echo DONE
