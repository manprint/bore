#!/usr/bin/env bash
# vhost through the SSH ingress gateway, from the fast same-region VM.
#
# The SSH leg is TCP relay only by design: no --udp, no --carriers. The
# comparison that matters is SSH-gateway vhost against NATIVE TCP-relay vhost,
# same origin, same VM, same server, back to back.
#
# Never pass -N: it skips opening a session channel, so the gateway's banner and
# its parameter warnings can never be delivered (I-SSH7 in CLAUDE.md).
# stdin from /dev/null is enough to keep the session alive; do not wrap the ssh
# in a pipeline, or a kill on the wrapper leaves an orphaned ssh holding the label.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . "${BORE_PERF_ENV:-$H/env.sh}"
BORE=$H/bore; OHA=$H/oha; OP=5052; OUT=$H/out; mkdir -p $OUT
GW=${BORE_HOST}; GWP=443
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -CONT "$p" 2>/dev/null; kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
cnt(){ adm vhost | jq --arg l "$1" '[.[]|select(.subdomain==$l)]|length'; }
present(){ [ "$(cnt "$1")" != 0 ]; }
entry(){ adm vhost | jq -c --arg l "$1" '.[]|select(.subdomain==$l)|{active,carriers,transport,relay_tx_bytes}'; }
txb(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.relay_tx_bytes'; }
lab(){ echo "$1$(date +%s%N | cut -c6-13)"; }
mb(){ awk -v b="$1" 'BEGIN{printf "%7.2f", b/1048576}'; }
origin_up(){ curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0
  setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null &
  for i in $(seq 40); do curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping 2>/dev/null && return 0; sleep 0.25; done; return 1; }

SSHOPT=(-T -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
        -o PubkeyAuthentication=no -o PreferredAuthentications=password
        -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o LogLevel=ERROR)

gw_up(){ # <label> [exec params]
  local l=$1; shift
  sshpass -p "$SSHGW_PASS" ssh "${SSHOPT[@]}" -R "vhost/$l:80:127.0.0.1:$OP" \
    -p $GWP "$SSHGW_USER@$GW" "$@" > $OUT/$l.ssh 2>&1 < /dev/null &
  GWPID=$!; KIDS+=("$GWPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $GWPID 2>/dev/null || return 1; sleep 0.5; done
  return 1
}
nat_up(){ # <label> [flags]
  local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$l.log 2>&1 &
  NATPID=$!; KIDS+=("$NATPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $NATPID 2>/dev/null || return 1; sleep 0.5; done
  return 1
}
released(){ # <label> <tries>  -> prints how long until the server dropped it
  local l=$1 n=$2 t0; t0=$(date +%s)
  for i in $(seq $n); do present "$l" || { echo "released after $(( $(date +%s)-t0 ))s"; return 0; }; sleep 2; done
  echo "*** STILL REGISTERED after $(( $(date +%s)-t0 ))s: $(entry "$l")"; return 1
}
# sustained download measured from the server's own byte counter
sust(){ # <label> <seconds>
  local l=$1 s=$2 a b t0 t1
  a=$(txb "$l"); t0=$(date +%s.%N)
  curl -fsS -o /dev/null --max-time "$s" "https://$l.$GW/stream/$((64*1073741824))" 2>/dev/null
  t1=$(date +%s.%N); b=$(txb "$l")
  awk -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{printf "%7.2f MB/s (%.2f GB)", (b-a)/1048576/(e-s), (b-a)/1073741824}'
}
parsust(){ # <label> <n> <bytes each>
  # Never a bare `wait`: it would also block on the long-lived origin and the
  # ssh/bore provider, which are children of this same shell.
  local l=$1 n=$2 sz=$3 a b t0 t1; local ps=()
  a=$(txb "$l"); t0=$(date +%s.%N)
  for i in $(seq "$n"); do curl -s -o /dev/null -m 90 "https://$l.$GW/stream/$sz" & ps+=("$!"); done
  for p in "${ps[@]}"; do wait "$p" 2>/dev/null; done
  t1=$(date +%s.%N); b=$(txb "$l")
  awk -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{printf "%7.2f MB/s aggregate (%.2f GB in %.1fs)", (b-a)/1048576/(e-s), (b-a)/1073741824, e-s}'
}

origin_up || { echo "origin failed"; exit 1; }
head -c 268435456 /dev/zero > $H/up256.bin

case "${1:-all}" in
s1|all)
echo "== S1 establish a vhost over the SSH gateway =="
L=$(lab s1)
if gw_up "$L"; then
  echo "  entry:   $(entry "$L")"
  echo "  serving: $(curl -s -o /dev/null -m 20 -w 'http=%{http_code} ttfb=%{time_starttransfer}' https://$L.$GW/1k)"
  echo "  gateway banner:"; sed 's/^/    | /' $OUT/$L.ssh | head -22
  kill -9 $GWPID 2>/dev/null; echo "  $(released "$L" 30)"
else echo "  FAILED: $(tail -3 $OUT/$L.ssh)"; fi
;;&

s2|all)
echo "== S2 throughput: SSH gateway versus native TCP relay, interleaved =="
for round in 1 2; do
  L=$(lab s2s)
  if gw_up "$L"; then
    echo "  r$round ssh    single 8s: $(sust "$L" 8)"
    echo "  r$round ssh    8 parallel: $(parsust "$L" 8 $((256*1048576)))"
    echo "  r$round ssh    upload 256MB: $(mb "$(curl -s -o /dev/null -m 90 -X PUT -T $H/up256.bin -H 'Expect:' -w '%{speed_upload}' https://$L.$GW/sink)") MB/s"
    kill -9 $GWPID 2>/dev/null; released "$L" 20 >/dev/null
  fi
  L=$(lab s2n)
  if nat_up "$L" --carriers 1; then
    echo "  r$round native single 8s: $(sust "$L" 8)"
    echo "  r$round native 8 parallel: $(parsust "$L" 8 $((256*1048576)))"
    echo "  r$round native upload 256MB: $(mb "$(curl -s -o /dev/null -m 90 -X PUT -T $H/up256.bin -H 'Expect:' -w '%{speed_upload}' https://$L.$GW/sink)") MB/s"
    kill -9 $NATPID 2>/dev/null; released "$L" 20 >/dev/null
  fi
done
;;&

s3|all)
echo "== S3 latency and request rate: SSH gateway versus native =="
for kind in ssh native; do
  L=$(lab s3)
  if [ $kind = ssh ]; then gw_up "$L" || { echo "  ssh FAILED"; continue; }; PID=$GWPID
  else nat_up "$L" --carriers 1 || { echo "  native FAILED"; continue; }; PID=$NATPID; fi
  curl -fsS -o /dev/null -m 20 "https://$L.$GW/ping"
  for c in 1 8 32 64; do
    echo "  $kind 1k c=$c  : $(timeout 25 $OHA -z 6s -c $c --no-tui --output-format json "https://$L.$GW/1k" 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99) ok=\(.summary.successRate)"')"
  done
  echo "  $kind 100k c=8: $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$L.$GW/100k" 2>/dev/null \
    | jq -r '"rps=\((.summary.requestsPerSec|round)) MB/s=\((.summary.sizePerSec/1048576*100|round)/100) p95=\(.metrics.latency_ms.p95)"')"
  echo "  $kind newconn c=8: $(timeout 25 $OHA -z 6s -c 8 --no-tui --disable-keepalive --output-format json "https://$L.$GW/1k" 2>/dev/null \
    | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95)"')"
  kill -9 $PID 2>/dev/null; sleep 3
done
;;&

s4|all)
echo "== S4 head-of-line: slow readers must not block fast requests =="
for kind in ssh native; do
  L=$(lab s4)
  if [ $kind = ssh ]; then gw_up "$L" || continue; PID=$GWPID
  else nat_up "$L" --carriers 1 || continue; PID=$NATPID; fi
  SLOW=()
  for i in $(seq 6); do curl -s -o /dev/null --limit-rate 200k -m 90 "https://$L.$GW/stream/104857600" & SLOW+=("$!"); KIDS+=("$!"); done
  sleep 4
  echo "  $kind with 6 slow readers pinned:"
  for i in 1 2 3; do echo "    $(curl -s -o /dev/null -m 15 -w 'http=%{http_code} ttfb=%{time_starttransfer} total=%{time_total}' https://$L.$GW/1k)"; done
  echo "    rps: $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$L.$GW/1k" 2>/dev/null | jq -r '"rps=\((.summary.requestsPerSec|round)) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99) ok=\(.summary.successRate)"')"
  echo "    entry: $(entry "$L")"
  for p in "${SLOW[@]}"; do kill -9 $p 2>/dev/null; done
  kill -9 $PID 2>/dev/null; echo "    $(released "$L" 25)"
done
;;&

s5|all)
echo "== S5 inapplicable parameters must warn, never be swallowed (I-SSH8) =="
L=$(lab s5)
if gw_up "$L" "https=on force-https=on basic-auth=user:pass max-conns=7 notes=perftest"; then
  sleep 3
  echo "  gateway output:"; sed 's/^/    | /' $OUT/$L.ssh | head -28
  kill -9 $GWPID 2>/dev/null; echo "  $(released "$L" 25)"
else echo "  FAILED: $(tail -5 $OUT/$L.ssh)"; fi
;;&

s6|all)
echo "== S6 wedged client: SSH gateway eviction (I-SSH10) versus native (F-1) =="
for kind in ssh native; do
  L=$(lab s6)
  if [ $kind = ssh ]; then gw_up "$L" || continue; WPID=$(pgrep -P $GWPID -x ssh | head -1); [ -z "$WPID" ] && WPID=$GWPID; TOP=$GWPID
  else nat_up "$L" --carriers 1 || continue; WPID=$NATPID; TOP=$NATPID; fi
  curl -s -o /dev/null --limit-rate 5M -m 300 "https://$L.$GW/stream/2147483648" & C=$!; KIDS+=("$C")
  sleep 4
  echo "  $kind before freeze: $(entry "$L")  (freezing pid $WPID)"
  kill -STOP $WPID 2>/dev/null || echo "  could not freeze"
  for t in 20 40 60 90 120; do sleep 20; echo "  $kind t+${t}s present=$(present "$L" && echo yes || echo no) $(entry "$L")"; done
  echo "  $kind re-register while wedged:"
  if [ $kind = ssh ]; then
    timeout 25 sshpass -p "$SSHGW_PASS" ssh "${SSHOPT[@]}" -R "vhost/$L:80:127.0.0.1:$OP" -p $GWP "$SSHGW_USER@$GW" </dev/null 2>&1 | grep -viE '^$' | head -3 | sed 's/^/    | /'
  else
    timeout 25 $BORE vhost 127.0.0.1:$OP --subdomain "$L" --id "${L}x" --to "$BORE_TO" --secret "$BORE_SECRET" 2>&1 | tail -1 | sed 's/^/    | /'
  fi
  kill -CONT $WPID 2>/dev/null; kill -9 $WPID $TOP $C 2>/dev/null
  echo "  $kind $(released "$L" 30)"
done
;;&

s7|all)
echo "== S7 same-identity takeover (I-SSH5) =="
L=$(lab s7)
if gw_up "$L"; then
  A=$GWPID
  echo "  incumbent serving: $(curl -s -o /dev/null -m 20 -w '%{http_code}' https://$L.$GW/ping)"
  if gw_up "$L"; then
    Bp=$GWPID; sleep 4
    echo "  after a second session with the SAME identity:"
    echo "    entries for label: $(cnt "$L")"
    echo "    serving:           $(curl -s -o /dev/null -m 20 -w '%{http_code}' https://$L.$GW/ping)"
    echo "    first session alive: $(kill -0 $A 2>/dev/null && echo yes || echo no)"
    echo "    second session output: $(head -3 $OUT/$L.ssh | tr '\n' ' ')"
    kill -9 $A $Bp 2>/dev/null
  else echo "    second session rejected: $(tail -2 $OUT/$L.ssh)"; kill -9 $A 2>/dev/null; fi
  echo "  $(released "$L" 25)"
fi
echo "== S8 native provider cannot be taken over by an SSH identity =="
L=$(lab s8)
if nat_up "$L" --carriers 1; then
  NP=$NATPID
  timeout 20 sshpass -p "$SSHGW_PASS" ssh "${SSHOPT[@]}" -R "vhost/$L:80:127.0.0.1:$OP" -p $GWP "$SSHGW_USER@$GW" </dev/null > $OUT/$L.ssh 2>&1
  echo "  ssh attempt against a native-held label: $(grep -viE '^$' $OUT/$L.ssh | head -2 | tr '\n' ' ')"
  echo "  entries: $(cnt "$L")  serving: $(curl -s -o /dev/null -m 20 -w '%{http_code}' https://$L.$GW/ping)"
  kill -9 $NP 2>/dev/null; echo "  $(released "$L" 25)"
fi
;;&
esac
echo "DONE"
