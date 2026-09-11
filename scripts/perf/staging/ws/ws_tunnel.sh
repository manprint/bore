#!/usr/bin/env bash
# The user-facing leg: origin and provider on the AWS VM, server on AWS, and
# the CONSUMER is this workstation over its own ISP link. This is the path a
# real browser takes, and the one the operator wants saturated.
#
# It is NOT the topology of §1.3's discarded measurements: there the workstation
# was the PROVIDER, so every byte crossed its link twice. Here it crosses once.
#
# Each configuration is measured for: one sustained stream, four parallel
# streams, eight parallel streams (download); one and four parallel PUTs
# (upload); and request latency. Parallel streams matter because a single
# tunnelled TCP flow is Mathis-bound at this RTT and cannot saturate the link
# on its own — the aggregate is the honest answer to "can bore fill the pipe".
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"



UPF="$B/up256.bin"
DUR="${DUR:-20}"
CFG="${1:-all}"
[ -f "$UPF" ] || head -c 268435456 /dev/zero > "$UPF"

adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576, b*8/1000000}'; }

PROV=""
up(){ # <label> <flags...>
  local l=$1; shift
  $SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$ORIGIN_PORT --subdomain $l --id $l \
      --to '$BORE_TO' --secret '$BORE_SECRET' $* > $VM_HOME/out/$l.log 2>&1 < /dev/null & echo \$!" >/dev/null 2>&1
  PROV="$l"
  for i in $(seq 60); do present "$l" && return 0; sleep 0.5; done
  return 1
}
down(){ $SSH $VMU@$V "pkill -9 -f 'subdomain $1 ' 2>/dev/null; pkill -9 -f '\\-\\-subdomain $1\$' 2>/dev/null; true" >/dev/null 2>&1
  for i in $(seq 25); do present "$1" || break; sleep 1; done; }

# N parallel sustained downloads, aggregate MB/s from the client side
par_dl(){ # <label> <n>
  local l=$1 n=$2 i tmp; tmp=$(mktemp -d)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m $((DUR+10)) --max-time $DUR -w '%{speed_download}\n' \
        "https://$l.$GW/stream/$((96*1073741824))" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/* 2>/dev/null
  rm -rf "$tmp"
}
par_up(){ # <label> <n>
  local l=$1 n=$2 i tmp; tmp=$(mktemp -d)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m 120 -X PUT -T "$UPF" -H 'Expect:' -w '%{speed_upload}\n' \
        "https://$l.$GW/sink$i" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/* 2>/dev/null
  rm -rf "$tmp"
}

echo "workstation as consumer; provider+origin on the AWS VM; ${DUR}s per download point"
echo "upload file: 256 MiB per stream"
echo

run_one(){ # <tag> <flags...>
  local tag=$1; shift
  local l="ws$(date +%s%N | cut -c6-13)"
  if ! up "$l" "$@"; then echo "  $tag REGISTRATION FAILED"; return 1; fi
  curl -fsS -o /dev/null -m 40 "https://$l.$GW/100k" 2>/dev/null
  local d0 d1 path=relay
  d0=$(fld "$l" direct_stream_opens)
  curl -fsS -o /dev/null -m 40 "https://$l.$GW/100k" 2>/dev/null
  d1=$(fld "$l" direct_stream_opens)
  [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct
  printf "  %-20s path=%-6s\n" "$tag" "$path"
  printf "      dl x1 : %s\n" "$(mbs "$(par_dl "$l" 1)")"
  printf "      dl x4 : %s\n" "$(mbs "$(par_dl "$l" 4)")"
  printf "      dl x8 : %s\n" "$(mbs "$(par_dl "$l" 8)")"
  printf "      up x1 : %s\n" "$(mbs "$(par_up "$l" 1)")"
  printf "      up x4 : %s\n" "$(mbs "$(par_up "$l" 4)")"
  printf "      lat   : %s\n" "$(timeout 30 oha -z 8s -c 8 --no-tui --output-format json "https://$l.$GW/1k" 2>/dev/null \
      | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99)"')"
  printf "      pool  : carriers=%s target=%s current_path=%s fallbacks=%s\n" \
      "$(fld "$l" carriers)" "$(fld "$l" carrier_target)" "$(fld "$l" current_path)" "$(fld "$l" direct_fallbacks)"
  down "$l"
  echo
}

case "$CFG" in
  quick) run_one "relay-tcp c=4" --carriers 4 ;;
  # The five configurations that answer distinct questions; the ones left out of
  # `all` (relay c=2, quic c=0auto) only interpolate between them and each costs
  # ~2.5 min of the operator's bandwidth.
  core)
    run_one "relay-tcp c=1"     --carriers 1
    run_one "relay-tcp c=8"     --carriers 8
    run_one "relay-tcp c=0auto" --carriers 0
    run_one "direct-quic c=1"   --carriers 1 --udp
    run_one "direct-quic c=4"   --carriers 4 --udp
    ;;
  all|*)
    run_one "relay-tcp c=1"     --carriers 1
    run_one "relay-tcp c=4"     --carriers 4
    run_one "relay-tcp c=8"     --carriers 8
    run_one "relay-tcp c=0auto" --carriers 0
    run_one "direct-quic c=1"   --carriers 1 --udp
    run_one "direct-quic c=4"   --carriers 4 --udp
    run_one "direct-quic c=0auto" --carriers 0 --udp
    ;;
esac
echo DONE
