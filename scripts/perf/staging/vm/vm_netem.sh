#!/usr/bin/env bash
# Protocol-selective impairment from the fast same-region VM.
#
# A tunnelled download uses two legs of the VM's egress: provider -> server
# (the tunnel data plane: TCP for the relay, UDP/QUIC for the direct path) and
# curl -> server (always TCP, the consumer leg). Impairing one IP protocol at a
# time separates the tunnel transport from the consumer leg, which is what makes
# the relay-versus-direct comparison meaningful.
#
# Baseline RTT here is 1.84 ms, so "+40 ms" models a distant consumer while
# keeping the server and origin identical.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"; . $H/env.sh
BORE=$H/bore; OP=5052; OUT=$H/out; mkdir -p $OUT
SRV=$SRV; GW=$GW
KIDS=()
IFACE(){ ip route get "$SRV" | awk '{print $5;exit}'; }
tcq(){ sudo -n tc "$1" "$2" dev "$(IFACE)" "${@:3}"; }
clear_netem(){ tcq qdisc del root >/dev/null 2>&1; return 0; }
cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; clear_netem; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
dso(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.direct_stream_opens'; }
txb(){ adm vhost | jq -r --arg l "$1" '.[]|select(.subdomain==$l)|.relay_tx_bytes'; }
lab(){ echo "$1$(date +%s%N | cut -c6-13)"; }

# impair(<proto: tcp|udp>, <delay ms>, <loss %>) — only traffic toward the server
impair(){ local proto=$1 d=$2 l=$3 pm=""
  case $proto in tcp) pm="match ip protocol 6 0xff";; udp) pm="match ip protocol 17 0xff";; esac
  clear_netem
  tcq qdisc add root handle 1: prio bands 3 >/dev/null 2>&1 || return 1
  local ne=()
  [ "$d" != 0 ] && ne+=(delay "${d}ms")
  [ "$l" != 0 ] && ne+=(loss "${l}%")
  tcq qdisc add parent 1:3 handle 30: netem "${ne[@]}" >/dev/null 2>&1 || return 1
  tcq filter add protocol ip parent 1:0 prio 1 u32 match ip dst "$SRV/32" $pm flowid 1:3 >/dev/null 2>&1 || return 1
  return 0
}

up(){ local l=$1; shift
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $OUT/$l.log 2>&1 &
  LASTPID=$!; KIDS+=("$LASTPID")
  for i in $(seq 60); do present "$l" && return 0; kill -0 $LASTPID 2>/dev/null || return 1; sleep 0.5; done; return 1; }

# one measurement set on an already-registered tunnel
meas(){ # <label>
  local l=$1 a b t0 t1 dl up_ lat
  a=$(txb "$l"); t0=$(date +%s.%N)
  curl -fsS -o /dev/null --max-time 12 "https://$l.$GW/stream/$((32*1073741824))" 2>/dev/null
  t1=$(date +%s.%N); b=$(txb "$l")
  dl=$(LC_ALL=C awk -v a="$a" -v b="$b" -v s="$t0" -v e="$t1" 'BEGIN{printf "%.2f", (b-a)/1048576/(e-s)}')
  up_=$(LC_ALL=C awk -v x="$(curl -s -o /dev/null -m 60 -X PUT -T $H/up256.bin -H 'Expect:' -w '%{speed_upload}' https://$l.$GW/sink)" 'BEGIN{printf "%.2f", x/1048576}')
  lat=$(timeout 25 $H/oha -z 6s -c 8 --no-tui --output-format json "https://$l.$GW/1k" 2>/dev/null \
        | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) ok=\(.summary.successRate)"')
  echo "dl=${dl} MB/s up=${up_} MB/s $lat"
}

run_pair(){ # <human condition label>
  local name=$1
  for mode in "" "--udp"; do
    local l tag; l=$(lab ne); tag="${mode:-relay-tcp}"; [ -n "$mode" ] && tag=direct-quic
    if up "$l" --carriers 1 $mode; then
      curl -fsS -o /dev/null -m 30 "https://$l.$GW/100k" 2>/dev/null
      local d; d=$(dso "$l")
      local path=relay-tcp; [ "${d:-0}" -gt 0 ] && path=direct-quic
      echo "    $tag (proven path=$path): $(meas "$l")"
      kill -9 $LASTPID 2>/dev/null
      for i in $(seq 20); do present "$l" || break; sleep 1; done
    else echo "    $tag REGISTRATION FAILED"; fi
  done
}

curl -fsS -m 2 -o /dev/null http://127.0.0.1:$OP/ping || { setsid nohup python3 $H/bench_origin.py $OP > $OUT/origin.log 2>&1 < /dev/null & sleep 2; }
[ -f $H/up256.bin ] || head -c 268435456 /dev/zero > $H/up256.bin
echo "iface toward server: $(IFACE)  baseline rtt: $(ping -c3 -q $SRV 2>/dev/null | awk -F/ '/rtt|round-trip/{print $5" ms"}')"

for cond in "clean:none:0:0" "tcp-loss1:tcp:0:1" "udp-loss1:udp:0:1" \
            "tcp-rtt40:tcp:40:0" "udp-rtt40:udp:40:0" "tcp-rtt40-loss1:tcp:40:1"; do
  IFS=: read -r name proto d l <<< "$cond"
  echo "===== $name ====="
  if [ "$proto" = none ]; then clear_netem
  else impair "$proto" "$d" "$l" && echo "    impaired: $proto delay=${d}ms loss=${l}% toward $SRV" || { echo "    IMPAIR FAILED"; continue; }
  fi
  run_pair "$name"
done
clear_netem

echo "===== G6 redo: UDP fully blackholed mid-session on a --udp tunnel ====="
L=$(lab g6r)
if up "$L" --carriers 1 --udp; then
  P=$LASTPID
  curl -fsS -o /dev/null -m 30 "https://$L.$GW/100k" 2>/dev/null
  d1=$(dso "$L"); echo "  direct opens after warmup: $d1"
  echo "  throughput before:      $(meas "$L")"
  if impair udp 0 100; then
    echo "  UDP toward the server is now 100% dropped"
    sleep 3
    echo "  single request during:  $(curl -s -o /dev/null -m 30 -w 'http=%{http_code} t=%{time_total}' https://$L.$GW/ping)"
    echo "  throughput during:     $(meas "$L")"
    d2=$(dso "$L")
    echo "  entry during: opens=$d2 server_fallbacks=$(adm metrics | jq -r .direct_fallbacks)"
    clear_netem; sleep 6
    echo "  throughput after clear: $(meas "$L")"
    d3=$(dso "$L")
    echo "  direct opens: warmup=$d1 during=$d2 after=$d3"
    echo "  (opens NOT rising during the blackhole, then rising after, = fallback in place then direct resumed)"
  else echo "  IMPAIR FAILED"; fi
  kill -9 $P 2>/dev/null
  for i in $(seq 20); do present "$L" || { echo "  released after ${i}s"; break; }; sleep 1; done
fi
clear_netem
echo DONE
