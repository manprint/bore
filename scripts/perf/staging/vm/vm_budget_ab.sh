#!/usr/bin/env bash
# Price `--udp-memory-budget` in throughput, across a SERVER RESTART.
#
# The budget shrinks the direct-path windows (conn 256 MiB -> 16 MiB, stream
# 16 MiB -> 1 MiB at this server's --max-carriers), so the question is whether
# that costs bandwidth. It cannot be answered by comparing two absolute numbers
# taken minutes apart on an instance that drifts 30 %.
#
# So the RELAY is used as an in-run control: the budget cannot touch the TCP
# relay path at all, so `quic / relay` measured inside one run cancels whatever
# the instance is doing at that moment, exactly like the paired A/B of §2.15 —
# and the two ratios (before the restart, after it) are comparable.
set -uo pipefail
H=$VM_HOME; . $H/env.sh
BORE=$H/bore; OP=5052; OUT=$H/out; GW=$GW
TAG="${1:-untagged}"; PAIRS="${2:-4}"
mkdir -p "$OUT"
KIDS=(); cleanup(){ for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }

one(){ # <tag> <flags...> -> "MB/s path"
  local t=$1; shift
  local l="b$(date +%s%N | cut -c7-13)"
  $BORE vhost 127.0.0.1:$OP --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > "$OUT/$l.log" 2>&1 &
  local p=$!; KIDS+=("$p")
  local ok=0 i
  for i in $(seq 60); do present "$l" && { ok=1; break; }; kill -0 $p 2>/dev/null || break; sleep 0.5; done
  [ $ok = 1 ] || { echo "0 registration-failed"; return 1; }
  curl -fsS -o /dev/null -m 20 "https://$l.$GW/100k" 2>/dev/null
  local d0 d1 path=relay
  d0=$(fld "$l" direct_stream_opens)
  local r; r=$(curl -s -o /dev/null -m 10 -w '%{speed_download}' "https://$l.$GW/stream/$((8*1073741824))")
  d1=$(fld "$l" direct_stream_opens)
  [ "${d1:-0}" -gt "${d0:-0}" ] && path=direct
  kill -9 $p 2>/dev/null
  for i in $(seq 20); do present "$l" || break; sleep 0.5; done
  LC_ALL=C awk -v b="${r:-0}" -v p="$path" 'BEGIN{printf "%.2f %s", b/1048576, p}'
}

echo "##### budget A/B, tag=$TAG"
echo "  admin config windows: stream=$(adm config | jq -r .udp_stream_receive_window) conn=$(adm config | jq -r .udp_connection_receive_window)"
echo "  pair  relay_MBs  quic_MBs  ratio_quic_over_relay  paths"
rs=(); for n in $(seq "$PAIRS"); do
  if [ $((n % 2)) -eq 1 ]; then
    read -r a ap <<< "$(one relay --carriers 1)"; read -r b bp <<< "$(one quic --carriers 1 --udp)"
  else
    read -r b bp <<< "$(one quic --carriers 1 --udp)"; read -r a ap <<< "$(one relay --carriers 1)"
  fi
  r=$(LC_ALL=C awk -v a="$a" -v b="$b" 'BEGIN{if(a>0) printf "%.3f", b/a; else print "na"}')
  rs+=("$r")
  printf "  %-5s %-10s %-9s %-22s relay=%s quic=%s\n" "$n" "$a" "$b" "$r" "$ap" "$bp"
done
printf '%s\n' "${rs[@]}" | sort -n | awk '{v[NR]=$1} END{printf "  median ratio: %s\n", v[int((NR+1)/2)]}'
echo "##### END $TAG"
