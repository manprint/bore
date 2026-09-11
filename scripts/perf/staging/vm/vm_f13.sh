#!/usr/bin/env bash
# F-13 in vivo: the aggregate direct-UDP memory bound, on the real 903 MiB host.
#
# The before-document measured the damage (§2.13 G9): 32 slow readers on ONE
# --udp vhost tunnel took the server to 536.8 MiB RSS, two unrelated requests
# timed out at 10 s, and a second tunnel could not register. The plan shipped
# `--udp-memory-budget` for exactly this, and it is UNSET on this server by
# design (unset = byte-identical historical path), so the damage is expected to
# reproduce until the operator sets it. This script measures both states with
# the same ladder, and also prices the throughput the smaller windows cost,
# because a memory fix that halves bandwidth is not a fix.
#
# usage: vm_f13.sh <tag>
set -uo pipefail
H=$VM_HOME; . $H/env.sh
BORE=$H/bore; OP=5052; OUT=$H/out; GW=$GW
TAG="${1:-untagged}"
mkdir -p "$OUT"
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
met(){ adm metrics | jq -r ".$1"; }
rss(){ adm metrics | jq -r '(.mem_rss_bytes/1048576*10|round)/10'; }
lab(){ echo "$1$(date +%s%N | cut -c7-13)"; }
up(){ local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > "$OUT/$l.log" 2>&1 &
  LASTPID=$!; KIDS+=("$LASTPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $LASTPID 2>/dev/null || return 1; sleep 0.5; done; return 1; }

echo "##### F-13 ladder, tag=$TAG"
echo "  server windows as the admin API reports them: stream=$(adm config | jq -r .udp_stream_receive_window) conn=$(adm config | jq -r .udp_connection_receive_window)"
echo "  budget refusals so far: $(met direct_budget_refusals)   baseline rss=$(rss) MiB"

L=$(lab f13)
if ! up "$L" --carriers 1 --udp; then echo "  REGISTRATION FAILED"; exit 1; fi
curl -fsS -o /dev/null -m 20 "https://$L.$GW/100k" 2>/dev/null
echo "  tunnel up: path=$(fld "$L" current_path) opens=$(fld "$L" direct_stream_opens)"

echo "  --- one stream, 12 s, to price the window size in throughput ---"
echo "      $(curl -s -o /dev/null -m 12 -w '%{speed_download}' "https://$L.$GW/stream/$((8*1073741824))" | awk '{printf "%.2f MB/s", $1/1048576}')"

echo "  --- the ladder: N slow readers (16 KiB/s each) on this one tunnel ---"
HOLD=()
prev=0
for n in 4 8 16 24 32 48; do
  for i in $(seq $((n - prev))); do
    curl -s -o /dev/null --limit-rate 16k -m 400 "https://$L.$GW/stream/1073741824" 2>/dev/null & HOLD+=("$!"); KIDS+=("$!")
  done
  prev=$n
  sleep 8
  f1=$(curl -s -o /dev/null -m 10 -w '%{http_code}/%{time_total}' "https://$L.$GW/1k" 2>/dev/null)
  f2=$(curl -s -o /dev/null -m 10 -w '%{http_code}/%{time_total}' "https://$L.$GW/1k" 2>/dev/null)
  printf "      N=%-3s active=%-4s rss=%-7s fast=%s,%s refusals=%s path=%s\n" \
    "$n" "$(fld "$L" active)" "$(rss)MiB" "$f1" "$f2" "$(met direct_budget_refusals)" "$(fld "$L" current_path)"
done

echo "  --- while all $prev readers are still held: can a SECOND tunnel register? ---"
L2=$(lab f13b)
if up "$L2" --carriers 1; then
  echo "      second tunnel (tcp relay): registered, GET -> $(curl -s -o /dev/null -m 15 -w 'http=%{http_code} t=%{time_total}' "https://$L2.$GW/1k")"
else
  echo "      second tunnel (tcp relay): REGISTRATION FAILED (this is the F-13 collateral damage)"
fi
echo "      other vhost entries on the server right now: $(adm vhost | jq -r 'length')"
for p in "${HOLD[@]}"; do kill -9 "$p" 2>/dev/null; done
sleep 6
echo "  settled: rss=$(rss) MiB active=$(fld "$L" active) refusals=$(met direct_budget_refusals)"
cleanup
sleep 2
echo "##### END $TAG"
