#!/usr/bin/env bash
# Wire compatibility of the new server against the PRE-PLAN client, which is the
# one risk this plan carries into production: `bore vhost` gained
# `HelloVhost.ctrl_heartbeat` (phase 01) and `HelloVhost.auto_carriers` +
# `ServerMessage::SetCarrierTarget` (phase 03.3), and server->client additions
# are NOT symmetric with client->server — an old client cannot deserialize an
# unknown ServerMessage variant, which on this wire is a hard control-loop error.
#
# So there are exactly three things to prove in vivo:
#   I1  an old client still registers and serves against the new server
#   I2  an old client is NEVER reaped: it cannot send heartbeats, so a reaper
#       that did not gate on the declared capability would kill a healthy idle
#       tunnel every deadline (DEC-VE2). Watched past the 60 s deadline.
#   I3  the server never sends SetCarrierTarget to an old client. Proven by
#       putting the old client under the exact load that makes the server ask a
#       NEW client to grow, and showing the old tunnel keeps serving.
set -uo pipefail
H=$VM_HOME; . $H/env.sh
OLD=$H/bore.f15a3de; NEW=$H/bore; OP=5052; OUT=$H/out; GW=$GW
mkdir -p "$OUT"
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
lab(){ echo "$1$(date +%s%N | cut -c7-13)"; }
curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" || { setsid nohup python3 "$H/bench_origin.py" $OP > "$OUT/origin.log" 2>&1 < /dev/null & sleep 2; }

echo "old client: $($OLD --version)"
echo "new client: $($NEW --version)"
echo

echo "== I1 old client registers and serves =="
L=$(lab io)
$OLD vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" --carriers 1 > "$OUT/$L.log" 2>&1 &
P=$!; KIDS+=("$P")
ok=0; for i in $(seq 60); do present "$L" && { ok=1; break; }; kill -0 $P 2>/dev/null || break; sleep 0.5; done
if [ $ok != 1 ]; then echo "  FAILED to register: $(tail -3 "$OUT/$L.log")"; exit 1; fi
echo "  registered; GET /1k -> $(curl -s -o /dev/null -m 20 -w 'http=%{http_code} t=%{time_total}' "https://$L.$GW/1k")"
echo "  100 MiB stream: $(curl -s -o /dev/null -m 60 -w '%{speed_download}' "https://$L.$GW/stream/104857600" | awk '{printf "%.2f MB/s", $1/1048576}')"

echo "== I2 old client survives past the 60 s reap deadline while idle =="
for t in 30 60 75 95; do
  sleep 25
  echo "  t+~${t}s present=$(present "$L" && echo yes || echo NO)  serving=$(curl -s -o /dev/null -m 15 -w '%{http_code}' "https://$L.$GW/ping")"
done

echo "== I3 old client under the growth-triggering load (two bulk transfers) =="
curl -s -o /dev/null -m 45 "https://$L.$GW/stream/$((8*1073741824))" & B1=$!; KIDS+=("$B1")
curl -s -o /dev/null -m 45 "https://$L.$GW/stream/$((8*1073741824))" & B2=$!; KIDS+=("$B2")
sleep 10
echo "  entry: carriers=$(fld "$L" carriers) target=$(fld "$L" carrier_target) active=$(fld "$L" active)"
echo "  small request during bulk: $(curl -s -o /dev/null -m 20 -w 'http=%{http_code} t=%{time_total}' "https://$L.$GW/1k")"
sleep 8
echo "  still present=$(present "$L" && echo yes || echo NO)  serving=$(curl -s -o /dev/null -m 15 -w '%{http_code}' "https://$L.$GW/ping")"
kill -9 $B1 $B2 2>/dev/null

echo "== I4 old client asked for --carriers 0 (a flag it does not have) =="
L2=$(lab io0)
timeout 25 $OLD vhost 127.0.0.1:$OP --subdomain "$L2" --id "$L2" --to "$BORE_TO" --secret "$BORE_SECRET" --carriers 0 > "$OUT/$L2.log" 2>&1
echo "  exit=$? output: $(tail -2 "$OUT/$L2.log" | tr '\n' ' ')"

kill -9 $P 2>/dev/null
for i in $(seq 25); do present "$L" || { echo "  old tunnel released after ${i}s"; break; }; sleep 1; done
echo DONE
