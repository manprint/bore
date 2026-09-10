#!/usr/bin/env bash
# vhost stability, lifecycle and race checks, run from the fast same-region VM.
# Goal is operational stability: no ghost registrations, no lost connections on
# a path switch, predictable behaviour when the origin or the provider dies.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OHA=$H/oha; OP=5052; OUT=$H/out; mkdir -p $OUT
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -CONT "$p" 2>/dev/null; kill -9 "$p" 2>/dev/null; done
           sudo -n tc qdisc del dev "$(ip route get ${BORE_SERVER_IP} | awk '{print $5;exit}')" root 2>/dev/null; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
entry(){ adm vhost | jq -c --arg l "$1" '.[]|select(.subdomain==$l)|{active,carriers,udp,relay_tx_bytes,direct_stream_opens}'; }
lab(){ echo "$1$(date +%s%N | cut -c4-13)"; }
origin_up(){ curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0
  setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null &
  for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0; sleep 0.25; done; return 1; }
up(){ local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$l.log 2>&1 &
  LASTPID=$!; KIDS+=("$LASTPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $LASTPID 2>/dev/null || return 1; sleep 0.5; done; return 1; }
wait_gone(){ local l=$1 n=$2 t0; t0=$(date +%s)
  for i in $(seq $n); do present "$l" || { echo "    released after $(( $(date +%s)-t0 ))s"; return 0; }; sleep 2; done
  echo "    *** STILL REGISTERED after $(( $(date +%s)-t0 ))s: $(entry "$l")"; return 1; }
NETEM(){ local if_=$(ip route get ${BORE_SERVER_IP} | awk '{print $5;exit}'); sudo -n "$@" dev "$if_"; }

origin_up || { echo "origin failed"; exit 1; }

case "${1:-all}" in
g1|all)
echo "== G1 wedged provider holds the label (both transports, 180 s watch) =="
for mode in "" "--udp"; do
  L=$(lab g1); tag="${mode:-tcp}"
  if up "$L" --carriers 1 $mode; then
    P=$LASTPID
    curl -s -o /dev/null --limit-rate 5M -m 300 "https://$L.${BORE_HOST}/stream/2147483648" & C=$!; KIDS+=("$C")
    sleep 4; echo "  $tag before freeze: $(entry "$L")"
    kill -STOP $P
    for t in 30 90 180; do sleep 30; echo "  $tag t+$((t))s present=$(present "$L" && echo yes || echo no) $(entry "$L")"; done
    echo "  $tag re-register while wedged: $(timeout 20 $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "${L}x" --to "$BORE_TO" --secret "$BORE_SECRET" 2>&1 | tail -1)"
    kill -CONT $P 2>/dev/null; kill -9 $P $C 2>/dev/null
    wait_gone "$L" 30
  else echo "  $tag REGISTRATION FAILED"; fi
done
;;& 

g2|all)
echo "== G2 four providers race for one label =="
L=$(lab g2)
for i in 1 2 3 4; do
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L$i" --to "$BORE_TO" --secret "$BORE_SECRET" > $OUT/$L-$i.log 2>&1 &
  KIDS+=("$!")
done
sleep 8
echo "  entries with that label: $(adm vhost | jq --arg l "$L" '[.[]|select(.subdomain==$l)]|length')"
echo "  winners/losers: $(grep -l 'in use' $OUT/$L-*.log 2>/dev/null | wc -l) rejected of 4"
echo "  serving: $(curl -s -o /dev/null -m 20 -w 'http=%{http_code}' https://$L.${BORE_HOST}/ping)"
for p in "${KIDS[@]: -4}"; do kill -9 $p 2>/dev/null; done
wait_gone "$L" 30
;;&

g3|all)
echo "== G3 reconnect storm: 20 register/deregister cycles on one label =="
L=$(lab g3); fails=0; slow=0
for i in $(seq 20); do
  if up "$L" --carriers 1; then
    P=$LASTPID; kill $P 2>/dev/null
    t0=$(date +%s)
    ok=0; for j in $(seq 30); do present "$L" || { ok=1; break; }; sleep 0.5; done
    d=$(( $(date +%s)-t0 )); [ $d -gt 3 ] && slow=$((slow+1))
    [ $ok = 1 ] || fails=$((fails+1))
  else fails=$((fails+1)); fi
done
echo "  cycles=20 failed_registrations_or_leaks=$fails releases_slower_than_3s=$slow"
echo "  final: present=$(present "$L" && echo yes || echo no)"
;;&

g4|all)
echo "== G4 provider killed mid-response: what does the HTTP client see? =="
L=$(lab g4)
if up "$L" --carriers 1; then
  P=$LASTPID
  ( curl -s -o /dev/null --limit-rate 5M -m 60 -w '  client: code=%{http_code} got=%{size_download} time=%{time_total} exit-on-next-line\n' \
      "https://$L.${BORE_HOST}/stream/1073741824"; echo "  curl exit=$?" ) & C=$!
  sleep 4; kill -9 $P 2>/dev/null
  wait $C 2>/dev/null
  wait_gone "$L" 20
else echo "  REGISTRATION FAILED"; fi
;;&

g5|all)
echo "== G5 origin faults: dead origin, closed port, unknown subdomain =="
# A dedicated origin on its own port so killing it cannot disturb the other cases.
python3 $H/bench_origin.py 5053 > $OUT/origin5053.log 2>&1 &
O2=$!; KIDS+=("$O2")
for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:5053/ping 2>/dev/null && break; sleep 0.25; done
L=$(lab g5)
$BORE vhost 127.0.0.1:5053 --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" > $OUT/$L.log 2>&1 &
P=$!; KIDS+=("$P")
for i in $(seq 60); do present "$L" && break; sleep 0.5; done
if present "$L"; then
  echo "  origin alive:            $(curl -s -o /dev/null -m 20 -w 'http=%{http_code} t=%{time_total}' https://$L.${BORE_HOST}/ping)"
  kill -9 $O2 2>/dev/null; sleep 2
  echo "  origin dead, tunnel up:  $(curl -s -o /dev/null -m 25 -w 'http=%{http_code} t=%{time_total}' https://$L.${BORE_HOST}/ping)"
  echo "  second request:          $(curl -s -o /dev/null -m 25 -w 'http=%{http_code} t=%{time_total}' https://$L.${BORE_HOST}/ping)"
  echo "  entry after origin died: $(entry "$L")  provider_alive=$(kill -0 $P 2>/dev/null && echo yes || echo no)"
  python3 $H/bench_origin.py 5053 > $OUT/origin5053b.log 2>&1 &
  O3=$!; KIDS+=("$O3")
  for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:5053/ping 2>/dev/null && break; sleep 0.25; done
  echo "  origin back, same tunnel: $(curl -s -o /dev/null -m 25 -w 'http=%{http_code} t=%{time_total}' https://$L.${BORE_HOST}/ping)"
  kill -9 $O3 $P 2>/dev/null; wait_gone "$L" 20
else echo "  REGISTRATION FAILED: $(tail -2 $OUT/$L.log)"; fi
# a tunnel pointed at a port nothing listens on
L2=$(lab g5b)
$BORE vhost 127.0.0.1:1 --subdomain "$L2" --id "$L2" --to "$BORE_TO" --secret "$BORE_SECRET" > $OUT/$L2.log 2>&1 &
P2=$!; KIDS+=("$P2")
for i in $(seq 40); do present "$L2" && break; sleep 0.5; done
if present "$L2"; then
  echo "  origin closed (port 1):  $(curl -s -o /dev/null -m 25 -w 'http=%{http_code} t=%{time_total}' https://$L2.${BORE_HOST}/ping)"
  kill -9 $P2 2>/dev/null; wait_gone "$L2" 20
else echo "  L2 did not register"; fi
echo "  unknown subdomain:       $(curl -s -o /dev/null -m 20 -w 'http=%{http_code} t=%{time_total}' https://doesnotexist$(date +%s).${BORE_HOST}/ping)"
;;&

g6|all)
echo "== G6 UDP blackholed mid-session on a --udp tunnel: fallback in place? =="
L=$(lab g6)
if up "$L" --carriers 1 --udp; then
  P=$LASTPID
  d0=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  curl -fsS -o /dev/null -m 20 "https://$L.${BORE_HOST}/100k"
  d1=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  echo "  direct before: opens $d0 -> $d1"
  echo "  throughput before: $(curl -s -o /dev/null -m 30 -w '%{speed_download}' https://$L.${BORE_HOST}/stream/524288000 | awk '{printf "%.2f MB/s", $1/1048576}')"
  NETEM tc qdisc add root handle 1: prio bands 3 2>/dev/null
  NETEM tc qdisc add parent 1:3 handle 30: netem loss 100% 2>/dev/null
  NETEM tc filter add protocol ip parent 1:0 prio 1 u32 match ip dst ${BORE_SERVER_IP}/32 match ip protocol 17 0xff flowid 1:3 2>/dev/null \
    && echo "  UDP to server blackholed" || echo "  NETEM FAILED"
  sleep 3
  echo "  request during blackhole: $(curl -s -o /dev/null -m 30 -w 'http=%{http_code} time=%{time_total}' https://$L.${BORE_HOST}/ping)"
  echo "  throughput during:  $(curl -s -o /dev/null -m 40 -w '%{speed_download}' https://$L.${BORE_HOST}/stream/524288000 | awk '{printf "%.2f MB/s", $1/1048576}')"
  d2=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  echo "  entry during: $(entry "$L")  fallbacks=$(adm metrics | jq -r .direct_fallbacks)"
  NETEM tc qdisc del root 2>/dev/null
  sleep 5
  echo "  throughput after clear: $(curl -s -o /dev/null -m 40 -w '%{speed_download}' https://$L.${BORE_HOST}/stream/524288000 | awk '{printf "%.2f MB/s", $1/1048576}')"
  d3=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.direct_stream_opens')
  echo "  direct opens: before=$d1 during=$d2 after=$d3 (a rise after clear means direct came back in place)"
  kill -9 $P 2>/dev/null; wait_gone "$L" 20
else echo "  REGISTRATION FAILED"; fi
;;&

g7|all)
echo "== G7 ten simultaneous tunnels (the stated concurrency limit) =="
LS=()
for i in $(seq 10); do
  L=$(lab g7); LS+=("$L")
  m=""; [ $((i % 2)) = 0 ] && m="--udp"
  $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "$L" --to "$BORE_TO" --secret "$BORE_SECRET" --carriers 1 $m > $OUT/$L.log 2>&1 &
  KIDS+=("$!")
done
sleep 12
n=0; for L in "${LS[@]}"; do present "$L" && n=$((n+1)); done
echo "  registered: $n of 10"
echo "  server: $(adm metrics | jq -r '"rss=\((.mem_rss_bytes/1048576*10|round)/10)MiB vhost=\(.vhost_domains) rej=\(.conn_rejections) fallbacks=\(.direct_fallbacks)"')"
ok=0; for L in "${LS[@]}"; do [ "$(curl -s -o /dev/null -m 20 -w '%{http_code}' https://$L.${BORE_HOST}/ping)" = 200 ] && ok=$((ok+1)); done
echo "  serving 200: $ok of 10"
for p in "${KIDS[@]: -10}"; do kill -9 $p 2>/dev/null; done
sleep 8
n=0; for L in "${LS[@]}"; do present "$L" && n=$((n+1)); done
echo "  still registered 8 s after all providers killed: $n"
echo "  server rss after: $(adm metrics | jq -r '"\((.mem_rss_bytes/1048576*10|round)/10)MiB"')"
;;&
g8|all)
echo "== G8 concurrency and server memory: connections versus RSS =="
for mode in "" "--udp"; do
  L=$(lab g8); tag="${mode:-tcp}"
  if up "$L" --carriers 1 $mode; then
    P=$LASTPID
    base=$(adm metrics | jq -r .mem_rss_bytes)
    echo "  $tag baseline rss=$(awk -v b=$base 'BEGIN{printf "%.1f MiB", b/1048576}')"
    for n in 16 64 256 512; do
      HOLD=()
      for i in $(seq $n); do curl -s -o /dev/null --limit-rate 20k -m 40 "https://$L.${BORE_HOST}/stream/1048576" & HOLD+=("$!"); done
      sleep 6
      echo "    $tag n=$n active=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.active') \
rss=$(adm metrics | jq -r '(.mem_rss_bytes/1048576*10|round)/10')MiB rej=$(adm metrics | jq -r .conn_rejections) \
fresh_req=$(curl -s -o /dev/null -m 15 -w 'http=%{http_code} t=%{time_total}' https://$L.${BORE_HOST}/1k)"
      for p in "${HOLD[@]}"; do kill -9 $p 2>/dev/null; done
      sleep 3
    done
    echo "  $tag settled rss=$(adm metrics | jq -r '(.mem_rss_bytes/1048576*10|round)/10')MiB active=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.active')"
    kill -9 $P 2>/dev/null; wait_gone "$L" 20
  else echo "  $tag REGISTRATION FAILED"; fi
done
;;&

g9|all)
echo "== G9 stalled-stream cliff: slow readers must not starve a fast request =="
# The documented QUIC window ratio (256 MiB conn / 16 MiB stream) is sized to
# tolerate ~16 stalled streams at carriers=1. Walk past that number.
for mode in "--udp" ""; do
  L=$(lab g9); tag="${mode:-tcp}"
  if up "$L" --carriers 1 $mode; then
    P=$LASTPID
    curl -fsS -o /dev/null -m 20 "https://$L.${BORE_HOST}/100k"
    for n in 4 8 16 24 32; do
      SLOW=()
      for i in $(seq $n); do curl -s -o /dev/null --limit-rate 100k -m 120 "https://$L.${BORE_HOST}/stream/536870912" & SLOW+=("$!"); done
      sleep 8
      f1=$(curl -s -o /dev/null -m 15 -w '%{http_code}/%{time_total}' https://$L.${BORE_HOST}/1k)
      f2=$(curl -s -o /dev/null -m 15 -w '%{http_code}/%{time_total}' https://$L.${BORE_HOST}/1k)
      echo "    $tag slow=$n fast_req=$f1,$f2 active=$(adm vhost | jq -r --arg l "$L" '.[]|select(.subdomain==$l)|.active') rss=$(adm metrics | jq -r '(.mem_rss_bytes/1048576*10|round)/10')MiB"
      for p in "${SLOW[@]}"; do kill -9 $p 2>/dev/null; done
      sleep 4
    done
    kill -9 $P 2>/dev/null; wait_gone "$L" 20
  else echo "  $tag REGISTRATION FAILED"; fi
done
;;&

esac
echo "DONE"
