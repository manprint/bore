#!/usr/bin/env bash
# dufs real-world case on BOTH data planes: TCP relay vs QUIC direct (--udp).
#
# §5 measured the three forwarder FLAVOURS (native/docker/ssh) and all three
# necessarily ran on the relay -- the SSH leg is TCP-only by design (I-SSH2) and
# the other two were started without --udp. This closes that gap by pricing the
# same dufs workload on the direct path, from the same domestic workstation.
#
# Same budget-neutral design as ws_flavours.sh: both forwarders registered at
# once against the SAME dufs, bursts bounded by wall clock, order ROTATED every
# round, per-burst allowance delta printed. A t4g.micro's inbound token bucket
# is spent by roughly one 4-stream 10 s download, so a sequential A-then-B
# comparison would measure the bucket, not the transport.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"




DPORT=$DUFS_PORT
# Same defect and same remedy as ws_flavours.sh: `$B` was never defined.
mkdir -p "$WORK"
BIGUP="$WORK/up1g.bin"; [ -f "$BIGUP" ] || head -c 1073741824 /dev/zero > "$BIGUP"

ROUNDS="${ROUNDS:-3}"
NSTREAM="${NSTREAM:-4}"
BURST="${BURST:-10}"
COOL="${COOL:-75}"

adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
ena(){ $SSH $SRVU@$SRV "sudo -n ethtool -S $IFACE | grep bw_in_allowance_exceeded | tr -dc '0-9'" 2>/dev/null; }
med(){ LC_ALL=C sort -n | awk '{v[NR]=$1} END{if(NR==0){print "n/a";exit} print (NR%2)?v[(NR+1)/2]:(v[NR/2]+v[NR/2+1])/2}'; }

# arm name -> extra bore flags
declare -A ARG=( [relay]="--carriers 8" [quic]="--udp --carriers 1" )
AR=(relay quic)
declare -A LBL
start(){ local a=$1 l="du${a:0:2}$(date +%s%N | cut -c8-13)"
  LBL[$a]=$l
  $SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$DPORT --subdomain $l --id $l \
      --to '$BORE_TO' --secret '$BORE_SECRET' ${ARG[$a]} > $VM_HOME/out/$l.log 2>&1 < /dev/null & true" >/dev/null 2>&1
  for i in $(seq 70); do present "$l" && { echo "  $a registered as $l (${ARG[$a]})"; return 0; }; sleep 0.5; done
  echo "  $a FAILED to register"; return 1
}
stopall(){ for a in "${!LBL[@]}"; do $SSH $VMU@$V "pkill -9 -f 'subdomain ${LBL[$a]}' 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'stopall' EXIT INT TERM

burst_dl(){ local l=$1 tmp; tmp=$(mktemp -d)
  for i in $(seq "$NSTREAM"); do
    ( curl -s -o /dev/null --max-time "$BURST" \
        -r 0-1073741823 -w '%{size_download}\n' "https://$l.$GW/big1g.bin" > "$tmp/$i" 2>/dev/null ) &
  done
  wait 2>/dev/null
  LC_ALL=C awk -v s="$BURST" '{t+=$1} END{printf "%.2f", t/1048576/s}' "$tmp"/*
  rm -rf "$tmp"
}
burst_up(){ local l=$1 tmp; tmp=$(mktemp -d)
  for i in $(seq "$NSTREAM"); do
    ( curl -s -o /dev/null --max-time "$BURST" -X PUT -T "$BIGUP" -H 'Expect:' \
        -w '%{size_upload}\n' "https://$l.$GW/upload/f$i.bin" > "$tmp/$i" 2>/dev/null ) &
  done
  wait 2>/dev/null
  LC_ALL=C awk -v s="$BURST" '{t+=$1} END{printf "%.2f", t/1048576/s}' "$tmp"/*
  rm -rf "$tmp"
}

echo "########## dufs: TCP relay vs QUIC direct  $(date -Is)"
echo "### ${NSTREAM} streams, ${BURST}s bursts, ${COOL}s cooldown, ${ROUNDS} rotated rounds"
$SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" 2>/dev/null | sed 's/^/  /'
for a in "${AR[@]}"; do start "$a" || exit 1; done

# prove which data plane each arm actually uses before quoting any number:
# direct_stream_opens counts SUCCESSFUL opens only (phase 05 honest counters),
# so a pair of small GETs that bumps it is proof of the direct path.
echo
for a in "${AR[@]}"; do l=${LBL[$a]}
  curl -fsS -o /dev/null -m 40 -r 0-1023 "https://$l.$GW/big1g.bin" 2>/dev/null
  d0=$(fld "$l" direct_stream_opens)
  curl -fsS -o /dev/null -m 40 -r 0-1023 "https://$l.$GW/big1g.bin" 2>/dev/null
  d1=$(fld "$l" direct_stream_opens)
  p=relay; [ "${d1:-0}" -gt "${d0:-0}" ] && p=direct
  echo "  $a: proven_path=$p  current_path=$(fld "$l" current_path)  direct_opens=$d1  fallbacks=$(fld "$l" direct_fallbacks)"
done
echo

rot(){ local n=$1 i out=(); for i in "${!AR[@]}"; do out+=("${AR[$(( (i+n) % ${#AR[@]} ))]}"); done; printf '%s\n' "${out[@]}"; }

for dir in dl up; do
  echo "  === $dir ==="
  declare -A ACC=(); declare -A POS=(); for a in "${AR[@]}"; do ACC[$a]=""; POS[$a]=""; done
  for r in $(seq "$ROUNDS"); do
    line="    round $r:"
    pos=0
    mapfile -t ORDER < <(rot $((r-1)))
    for a in "${ORDER[@]}"; do
      pos=$((pos+1))
      sleep "$COOL"
      e0=$(ena)
      if [ "$dir" = dl ]; then v=$(burst_dl "${LBL[$a]}"); else v=$(burst_up "${LBL[$a]}"); fi
      e1=$(ena)
      ACC[$a]="${ACC[$a]}$v
"
      POS[$a]="${POS[$a]}p$pos=$v "
      line="$line  [$pos]$a=$v(+$((${e1:-0}-${e0:-0})))"
      [ "$dir" = up ] && $SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" >/dev/null 2>&1
    done
    echo "$line"
  done
  for a in "${AR[@]}"; do
    m=$(printf '%s' "${ACC[$a]}" | grep -v '^$' | med)
    echo "    median $dir $a: $m MB/s ($(LC_ALL=C awk -v m="$m" 'BEGIN{printf "%.0f", m*8.388608}') Mbit/s)   by position: ${POS[$a]}"
  done
done

# small-request arm: 200 sequential GETs of a 4 KiB file, the shape §5.2 priced
# on the relay. Sequential on purpose -- one request at a time isolates the
# per-request cost from any bandwidth effect.
echo "  === small sequential GET (200 x 4KiB, alternating arms) ==="
for a in "${AR[@]}"; do l=${LBL[$a]}
  t=$(for i in $(seq 200); do curl -s -o /dev/null -w '%{time_total}\n' "https://$l.$GW/small/s1.bin" 2>/dev/null; done \
      | LC_ALL=C sort -n | awk '{v[NR]=$1} END{printf "p50=%.2fms p95=%.2fms", v[int(NR*0.5)]*1000, v[int(NR*0.95)]*1000}')
  echo "    $a: $t"
done

echo "  === entries at the end ==="
for a in "${AR[@]}"; do l=${LBL[$a]}
  echo "    $a: carriers=$(fld "$l" carriers) target=$(fld "$l" carrier_target) path=$(fld "$l" current_path) fallbacks=$(fld "$l" direct_fallbacks) opens=$(fld "$l" direct_stream_opens) active=$(fld "$l" active)"
done
stopall; trap - EXIT INT TERM
echo DONE
