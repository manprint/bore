#!/usr/bin/env bash
# Transport A/B strong enough to survive the 29 % control drift, plus the
# latency suite for native TCP relay against native QUIC direct.
#
# Design: PAIRED, not independent. Each pair measures both transports back to
# back within seconds and reports the ratio; slow drift is common to both halves
# of a pair and cancels in the ratio. The order alternates within the pair so a
# systematic first-versus-second effect cancels across pairs. The statistic is
# the MEDIAN of the per-pair ratios, which is what should be quoted.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . $H/env.sh
BORE=$H/bore; OHA=$H/oha; OP=5052; OUT=$H/out; mkdir -p $OUT
GW=${BORE_HOST}
KIDS=()
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
txb(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.relay_tx_bytes'; }
dso(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.direct_stream_opens'; }
lab(){ echo "$1$(date +%s%N | cut -c6-13)"; }
DUR="${DUR:-12}"

up(){ local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$l.log 2>&1 &
  LASTPID=$!; KIDS+=("$LASTPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $LASTPID 2>/dev/null || return 1; sleep 0.5; done; return 1; }
down(){ kill -9 "$1" 2>/dev/null; for i in $(seq 20); do present "$2" || break; sleep 1; done; }

# one sustained download, rate from the server's own byte counter
rate(){ # <label>
  local l=$1 a b t0 t1
  a=$(txb "$l"); t0=$(date +%s.%N)
  curl -fsS -o /dev/null --max-time "$DUR" "https://$l.$GW/stream/$((64*1073741824))" 2>/dev/null
  t1=$(date +%s.%N); b=$(txb "$l")
  LC_ALL=C awk -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{printf "%.2f", (b-a)/1048576/(e-s)}'
}
# measure one configuration end to end and print just the rate
one(){ # <tag> <flags...>
  local tag=$1; shift
  local l; l=$(lab ab)
  if ! up "$l" "$@"; then echo "FAIL"; return 1; fi
  curl -fsS -o /dev/null -m 30 "https://$l.$GW/100k" 2>/dev/null
  local d0 r d1
  d0=$(dso "$l"); r=$(rate "$l"); d1=$(dso "$l")
  local path=relay; [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct
  down "$LASTPID" "$l"
  echo "$r $path"
}

echo "===== A1 paired transport A/B, ${DUR}s per half, 8 pairs ====="
echo "  pair  tcp_MBs  udp_MBs  ratio_tcp_over_udp  paths"
RATIOS=()
for i in $(seq 8); do
  if [ $((i % 2)) = 1 ]; then
    read -r t tp <<< "$(one tcp --carriers 1)"
    read -r u upth <<< "$(one udp --carriers 1 --udp)"
  else
    read -r u upth <<< "$(one udp --carriers 1 --udp)"
    read -r t tp <<< "$(one tcp --carriers 1)"
  fi
  r=$(LC_ALL=C awk -v t="$t" -v u="$u" 'BEGIN{if(u>0) printf "%.3f", t/u; else print "nan"}')
  RATIOS+=("$r")
  printf "  %-5s %8s %8s %19s  tcp=%s udp=%s\n" "$i" "$t" "$u" "$r" "$tp" "$upth"
done
echo "  median ratio: $(printf '%s\n' "${RATIOS[@]}" | sort -g | awk '{a[NR]=$1} END{if(NR%2)print a[(NR+1)/2]; else printf "%.3f", (a[NR/2]+a[NR/2+1])/2}')"
echo "  (>1 means the TCP relay is faster; the median is the number to quote)"

echo
echo "===== A2 paired carrier A/B on the TCP relay, ${DUR}s per half, 5 pairs ====="
echo "  pair  c1_MBs  c4_MBs  ratio_c4_over_c1"
R2=()
for i in $(seq 5); do
  if [ $((i % 2)) = 1 ]; then
    read -r a _ <<< "$(one c1 --carriers 1)"; read -r b _ <<< "$(one c4 --carriers 4)"
  else
    read -r b _ <<< "$(one c4 --carriers 4)"; read -r a _ <<< "$(one c1 --carriers 1)"
  fi
  r=$(LC_ALL=C awk -v a="$a" -v b="$b" 'BEGIN{if(a>0) printf "%.3f", b/a; else print "nan"}')
  R2+=("$r"); printf "  %-5s %7s %7s %17s\n" "$i" "$a" "$b" "$r"
done
echo "  median ratio: $(printf '%s\n' "${R2[@]}" | sort -g | awk '{a[NR]=$1} END{if(NR%2)print a[(NR+1)/2]; else printf "%.3f", (a[NR/2]+a[NR/2+1])/2}')"
echo "  (>1 means 4 carriers help on a clean path)"

echo
echo "===== A3 latency suite, native TCP relay versus native QUIC direct ====="
for mode in "" "--udp"; do
  tag=relay-tcp; [ -n "$mode" ] && tag=direct-quic
  L=$(lab a3)
  if up "$L" --carriers 1 $mode; then
    P=$LASTPID
    curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
    d0=$(dso "$L")
    for c in 1 8 32 64; do
      echo "  $tag 1k c=$c   : $(timeout 25 $OHA -z 6s -c $c --no-tui --output-format json "https://$L.$GW/1k" 2>/dev/null \
        | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99) ok=\(.summary.successRate)"')"
    done
    echo "  $tag 100k c=8  : $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$L.$GW/100k" 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec|round)) MBs=\((.summary.sizePerSec/1048576*100|round)/100) p95=\(.metrics.latency_ms.p95)"')"
    echo "  $tag newconn c=8: $(timeout 25 $OHA -z 6s -c 8 --no-tui --disable-keepalive --output-format json "https://$L.$GW/1k" 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95)"')"
    # latency while a bulk transfer is in flight: the case a real page load hits
    curl -s -o /dev/null -m 40 "https://$L.$GW/stream/$((6*1073741824))" & BK=$!; KIDS+=("$BK")
    sleep 3
    echo "  $tag 1k c=8 under bulk: $(timeout 25 $OHA -z 6s -c 8 --no-tui --output-format json "https://$L.$GW/1k" 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99)"')"
    kill -9 $BK 2>/dev/null
    d1=$(dso "$L")
    echo "  $tag direct_stream_opens $d0 -> $d1"
    down "$P" "$L"
  else echo "  $tag REGISTRATION FAILED"; fi
done
echo DONE
