#!/usr/bin/env bash
# Price BORE_PROXY_BUFFER_SIZE. The deployed value is 128KiB, half the built-in
# default of 256KiB, and nothing in the deployment says why.
#
# The documentation's claim is specific: a larger buffer "helps high-latency,
# high-BDP links, not single-stream throughput on a fast LAN". So measuring
# only the clean 2.1 ms same-region path would be measuring the case the
# parameter is documented NOT to affect. Each run therefore reports BOTH:
#   clean   — the 2.1 ms path, where the answer should be "no difference"
#   +40ms   — netem on the provider's TCP leg, where the parameter should bite
# The buffer cannot be A/B'd inside one run (it is a process-wide OnceLock read
# at startup), so the caller restarts the server between invocations and the
# A/B/A/B ordering plus the median across repeats absorbs the instance drift.
set -uo pipefail
H=$VM_HOME; . $H/env.sh
BORE=$H/bore; OP=5052; OUT=$H/out; GW=$GW
TAG="${1:-untagged}"; REPS="${2:-3}"
# Third argument selects the conditions. The 40ms arm alone is the useful one
# when allowance budget is scarce: the clean arm costs the most budget and is
# the case the parameter is documented not to affect.
CONDS="${3:-clean delay40}"

mkdir -p "$OUT"
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; clear_impair; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
IF=$(ip route get $SRV | awk '{print $5;exit}')
# dev goes after <object> <verb>, never at the end of the argv -- the mistake
# that made an earlier blackhole a silent no-op.
clear_impair(){ sudo -n tc qdisc del dev "$IF" root >/dev/null 2>&1; }
add_delay(){ clear_impair
  sudo -n tc qdisc add dev "$IF" root handle 1: prio bands 3 >/dev/null 2>&1 || return 1
  sudo -n tc qdisc add dev "$IF" parent 1:3 handle 30: netem delay 40ms >/dev/null 2>&1 || return 1
  sudo -n tc filter add dev "$IF" protocol ip parent 1:0 prio 1 u32 \
      match ip dst $SRV/32 match ip protocol 6 0xff flowid 1:3 >/dev/null 2>&1 || return 1
}

curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" || { setsid nohup python3 "$H/bench_origin.py" $OP > "$OUT/origin.log" 2>&1 < /dev/null & sleep 2; }

one(){ # -> MB/s of a 12 s single stream
  local l="u$(date +%s%N | cut -c7-13)" p ok=0 i r
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" --carriers 1 > "$OUT/$l.log" 2>&1 &
  p=$!; KIDS+=("$p")
  for i in $(seq 80); do present "$l" && { ok=1; break; }; kill -0 $p 2>/dev/null || break; sleep 0.5; done
  [ $ok = 1 ] || { echo 0; return; }
  curl -fsS -o /dev/null -m 20 "https://$l.$GW/100k" 2>/dev/null
  r=$(curl -s -o /dev/null -m 15 -w '%{speed_download}' "https://$l.$GW/stream/$((8*1073741824))")
  kill -9 $p 2>/dev/null
  for i in $(seq 20); do present "$l" || break; sleep 0.5; done
  LC_ALL=C awk -v b="${r:-0}" 'BEGIN{printf "%.2f", b/1048576}'
}
med(){ LC_ALL=C sort -n | awk '{v[NR]=$1} END{if(NR==0){print "n/a";exit} print (NR%2)?v[(NR+1)/2]:(v[NR/2]+v[NR/2+1])/2}'; }

echo "##### proxy buffer, tag=$TAG"
echo "  admin reports: $(adm config | jq -r '.proxy_buffer_size // "FIELD ABSENT (server image predates the 2026-09-11 fix)"')"
for cond in $CONDS; do
  [ "$cond" = delay40 ] && { add_delay || { echo "  netem FAILED"; continue; }; } || clear_impair
  acc=""
  for r in $(seq "$REPS"); do
    v=$(one); acc="$acc$v
"; printf '    %s rep%s: %s MB/s\n' "$cond" "$r" "$v"
    # REPCOOL exists because back-to-back repetitions shape the instance: the
    # first buffer A/B of this campaign was invalidated exactly that way.
    sleep "${REPCOOL:-45}"
  done
  echo "  MEDIAN $cond: $(printf '%s' "$acc" | grep -v '^$' | med) MB/s"
done
clear_impair
echo DONE
