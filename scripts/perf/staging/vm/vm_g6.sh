#!/usr/bin/env bash
# F-14 in vivo, instrumented properly: what happens to a request that is in
# flight, and to the requests after it, when the direct UDP path dies under a
# live `--udp` vhost tunnel.
#
# The before-document measured `http=000` after 9.90 s for the first request
# (§2.14 G6 redo) and two counters that lied. Phase 05 shipped three fixes for
# this: a bounded direct open (`DIRECT_OPEN_TIMEOUT` 3 s, covering open AND
# `write_stream_ready`), `direct_stream_opens` counting only successful opens,
# and `VhostEntry.last_path` / `direct_fallbacks`. This script reads all three
# per request instead of once per phase, because "the first request is lost but
# the second is fine" and "every request is lost" are different bugs and the
# before-document could not tell them apart.
#
# The blackhole is applied in BOTH directions, which the before-document did not
# do: egress via tc/netem (VM -> server UDP) and ingress via iptables
# (server -> VM UDP). A one-way drop leaves the server's `open_bi` able to reach
# the provider, which is a different and less realistic failure.
set -uo pipefail
H=$VM_HOME; . $H/env.sh
BORE=$H/bore; OP=5052; OUT=$H/out; GW=$GW; SRV=$SRV
mkdir -p "$OUT"
IF=$(ip route get $SRV | awk '{print $5;exit}')
KIDS=()
clear_impair(){
  sudo -n tc qdisc del dev "$IF" root >/dev/null 2>&1
  sudo -n iptables -D INPUT -p udp -s $SRV -j DROP >/dev/null 2>&1
  return 0
}
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; clear_impair; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
met(){ adm metrics | jq -r ".$1"; }
state(){ printf 'opens=%s path=%s entry_fallbacks=%s srv_fallbacks=%s' \
  "$(fld "$1" direct_stream_opens)" "$(fld "$1" current_path)" \
  "$(fld "$1" direct_fallbacks)" "$(met direct_fallbacks)"; }
req(){ curl -s -o /dev/null -m 20 -w 'http=%{http_code} t=%{time_total}' "https://$1.$GW/1k"; }

impair(){
  clear_impair
  sudo -n tc qdisc add dev "$IF" root handle 1: prio bands 3 >/dev/null 2>&1 || return 1
  sudo -n tc qdisc add dev "$IF" parent 1:3 handle 30: netem loss 100% >/dev/null 2>&1 || return 1
  sudo -n tc filter add dev "$IF" protocol ip parent 1:0 prio 1 u32 \
      match ip dst $SRV/32 match ip protocol 17 0xff flowid 1:3 >/dev/null 2>&1 || return 1
  sudo -n iptables -I INPUT -p udp -s $SRV -j DROP >/dev/null 2>&1 || return 1
  return 0
}

L="g6$(date +%s%N | cut -c7-13)"
$BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" --carriers 1 --udp > "$OUT/$L.log" 2>&1 &
P=$!; KIDS+=("$P")
ok=0; for i in $(seq 60); do present "$L" && { ok=1; break; }; sleep 0.5; done
[ $ok = 1 ] || { echo "REGISTRATION FAILED"; exit 1; }
curl -fsS -o /dev/null -m 20 "https://$L.$GW/100k" 2>/dev/null
echo "iface=$IF  tunnel=$L"
echo "  warm: $(state "$L")"
echo "  throughput before: $(curl -s -o /dev/null -m 15 -w '%{speed_download}' "https://$L.$GW/stream/$((4*1073741824))" | awk '{printf "%.2f MB/s", $1/1048576}')"
echo "  warm after bulk: $(state "$L")"

if impair; then echo "  UDP to and from the server is now 100 % dropped, both directions"
else echo "  IMPAIRMENT FAILED"; exit 1; fi

for n in 1 2 3 4 5; do
  echo "  request #$n during the blackhole: $(req "$L")   $(state "$L")"
done
echo "  throughput during: $(curl -s -o /dev/null -m 20 -w '%{speed_download}' "https://$L.$GW/stream/$((4*1073741824))" | awk '{printf "%.2f MB/s", $1/1048576}')   $(state "$L")"
echo "  budget refusals: $(met direct_budget_refusals)"

clear_impair
echo "  blackhole cleared"
for n in 1 2 3; do sleep 4; echo "  request +$((n*4))s after clear: $(req "$L")   $(state "$L")"; done
echo "  throughput after: $(curl -s -o /dev/null -m 15 -w '%{speed_download}' "https://$L.$GW/stream/$((4*1073741824))" | awk '{printf "%.2f MB/s", $1/1048576}')   $(state "$L")"
kill -9 $P 2>/dev/null
for i in $(seq 25); do present "$L" || { echo "  released after ${i}s"; break; }; sleep 1; done
echo DONE
