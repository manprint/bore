#!/usr/bin/env bash
# Compare the THREE forwarder flavours on bandwidth-shaped work, budget-neutrally.
#
# A sequential ladder cannot do this on a t4g.micro: the first flavour measured
# spends the instance's inbound allowance and every later one reads the baseline
# (measured: docker's x4 rung collapsed to 9.90 MB/s with allowance +597 390
# while its own x1/x2 rungs were clean). So all three forwarders are registered
# at once against the SAME dufs, and each round hits them in rotation with a
# cooldown between bursts. A drifting budget then hits all three arms of a round
# roughly equally, and the per-burst allowance delta is printed so a shaped
# burst can be discarded rather than averaged in.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"




DPORT=$DUFS_PORT
# Was `WORK="$B/work"` with `$B` undefined -- fatal under `set -u`, and had
# it resolved it would have written a 1 GB payload into a directory git does
# not ignore. lib.sh's WORK is outside the tree.
mkdir -p "$WORK"
BIGUP="$WORK/up1g.bin"; [ -f "$BIGUP" ] || head -c 1073741824 /dev/zero > "$BIGUP"

ROUNDS="${ROUNDS:-3}"
NSTREAM="${NSTREAM:-4}"
BURST="${BURST:-10}"
COOL="${COOL:-30}"
CARR="${CARR:---carriers 8}"

adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
ena(){ $SSH $SRVU@$SRV "sudo -n ethtool -S $IFACE | grep bw_in_allowance_exceeded | tr -dc '0-9'" 2>/dev/null; }
med(){ LC_ALL=C sort -n | awk '{v[NR]=$1} END{if(NR==0){print "n/a";exit} print (NR%2)?v[(NR+1)/2]:(v[NR/2]+v[NR/2+1])/2}'; }

declare -A LBL
start(){ # $1 = flavour
  local f=$1 l="d${f:0:2}$(date +%s%N | cut -c8-13)"
  LBL[$f]=$l
  case $f in
  native) $SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$DPORT --subdomain $l --id $l \
      --to '$BORE_TO' --secret '$BORE_SECRET' $CARR > $VM_HOME/out/$l.log 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  docker) $SSH $VMU@$V "setsid nohup sudo -n docker run --rm --name bore-$l --network host \
      ghcr.io/manprint/bore:client vhost 127.0.0.1:$DPORT --subdomain $l --id $l \
      --to '$BORE_TO' --secret '$BORE_SECRET' $CARR > $VM_HOME/out/$l.log 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  ssh)    $SSH $VMU@$V "setsid nohup sshpass -p '$SSHGW_PASS' ssh -T -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null -o PubkeyAuthentication=no -o PreferredAuthentications=password \
      -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o LogLevel=ERROR \
      -R vhost/$l:80:127.0.0.1:$DPORT -p 443 '$SSHGW_USER@$GW' \
      > $VM_HOME/out/$l.ssh 2>&1 < /dev/null & true" >/dev/null 2>&1 ;;
  esac
  for i in $(seq 70); do present "$l" && { echo "  $f registered as $l"; return 0; }; sleep 0.5; done
  echo "  $f FAILED to register"; return 1
}
stopall(){
  for f in "${!LBL[@]}"; do local l=${LBL[$f]}
    case $f in
      native) $SSH $VMU@$V "pkill -9 -f 'subdomain $l' 2>/dev/null; true" >/dev/null 2>&1 ;;
      docker) $SSH $VMU@$V "sudo -n docker rm -f bore-$l 2>/dev/null; true" >/dev/null 2>&1 ;;
      ssh)    $SSH $VMU@$V "pkill -9 -f 'vhost/$l:80' 2>/dev/null; true" >/dev/null 2>&1 ;;
    esac
  done
}
trap 'stopall' EXIT INT TERM

# a burst bounded by TIME, not by bytes: every flavour then pays the same
# allowance for the same wall-clock and the comparison cannot be biased by one
# arm finishing early
# curl emits its --write-out line even when --max-time aborts the transfer
# (rc 28, size_download = the partial byte count), verified before use — so the
# burst is bounded by curl itself and nothing has to be signalled or killed.
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

echo "### paired flavour comparison: ${NSTREAM} streams, ${BURST}s bursts, ${COOL}s cooldown, ${ROUNDS} rounds"
echo "### carriers flag for native/docker: '$CARR' (the ssh leg is TCP-relay-only by design, I-SSH2)"
$SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" 2>/dev/null | sed 's/^/  /'
FL=(native docker ssh)
for f in "${FL[@]}"; do start "$f" || exit 1; done
echo

# ROTATE the order every round. One 4-stream 10 s burst is ~500 MB, which is
# about the whole inbound burst budget, so within a round only the flavour
# measured FIRST sees an unshaped link (measured: native 49.82 with +10 880
# allowance misses, then docker 23.37 with +43 040 and ssh 21.31 with +43 872).
# A fixed order would therefore hand the same flavour the clean slot every
# time. Rotating makes each flavour occupy each position once, so the position
# is reported next to every figure and can be read out of the comparison.
rot(){ local n=$1 i out=(); for i in "${!FL[@]}"; do out+=("${FL[$(( (i+n) % ${#FL[@]} ))]}"); done; printf '%s\n' "${out[@]}"; }

for dir in dl up; do
  echo "  === $dir ==="
  declare -A ACC=(); declare -A POS=(); for f in "${FL[@]}"; do ACC[$f]=""; POS[$f]=""; done
  for r in $(seq "$ROUNDS"); do
    line="    round $r:"
    pos=0
    mapfile -t ORDER < <(rot $((r-1)))
    for f in "${ORDER[@]}"; do
      pos=$((pos+1))
      sleep "$COOL"
      e0=$(ena)
      if [ "$dir" = dl ]; then v=$(burst_dl "${LBL[$f]}"); else v=$(burst_up "${LBL[$f]}"); fi
      e1=$(ena)
      ACC[$f]="${ACC[$f]}$v
"
      POS[$f]="${POS[$f]}p$pos=$v "
      line="$line  [$pos]$f=$v(+$((${e1:-0}-${e0:-0})))"
      [ "$dir" = up ] && $SSH $VMU@$V "$VM_HOME/vm_dufs_setup.sh clean" >/dev/null 2>&1
    done
    echo "$line"
  done
  for f in "${FL[@]}"; do
    m=$(printf '%s' "${ACC[$f]}" | grep -v '^$' | med)
    echo "    median $dir $f: $m MB/s ($(LC_ALL=C awk -v m="$m" 'BEGIN{printf "%.0f", m*8.388608}') Mbit/s)   by position: ${POS[$f]}"
  done
  echo "    (position 1 = measured on a fresh budget; 2 and 3 are progressively shaped)"
done

echo "  === pools at the end ==="
for f in "${FL[@]}"; do l=${LBL[$f]}
  echo "    $f: carriers=$(fld "$l" carriers) target=$(fld "$l" carrier_target) path=$(fld "$l" current_path) active=$(fld "$l" active)"
done
stopall; trap - EXIT INT TERM
echo DONE
