#!/usr/bin/env bash
# Paired re-run of the workstation-as-consumer configurations, after the first
# pass proved the apparatus can invalidate itself.
#
# WHAT WENT WRONG THE FIRST TIME: the staging server is a t4g.micro, whose
# INBOUND network allowance is a burst budget with a low baseline. The first two
# configurations of the serial run measured 40-59 MB/s; the remaining three
# measured 7-9 MB/s, and the server's `bw_in_allowance_exceeded` counter rose by
# 4 029 161 across the suite (from 78 k cumulative since boot). The three slow
# configurations were measurements of AWS shaping, not of bore. The budget
# recovers within minutes of idling: a probe five minutes later read 42.93 MB/s
# with a ZERO allowance delta.
#
# SO: two tunnels are registered at once — the REFERENCE (relay c=8, whose
# behaviour the first pass established) and the configuration under test — and
# the two are measured alternately in 10 s bursts separated by a cooldown, with
# the server's allowance delta recorded per burst. Whatever the instance is
# doing at that moment is common to both halves and cancels in the ratio; a
# burst whose allowance delta is non-zero is flagged, not silently averaged in.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"



DUR="${DUR:-10}"; PAIRS="${PAIRS:-3}"; COOL="${COOL:-40}"; PAR="${PAR:-4}"
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld(){ adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }
ena(){ $SSH $SRVU@$S "sudo -n ethtool -S $IFACE | grep bw_in_allowance_exceeded | tr -dc '0-9'" 2>/dev/null; }

up(){ local l=$1; shift
  $SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$ORIGIN_PORT --subdomain $l --id $l \
      --to '$BORE_TO' --secret '$BORE_SECRET' $* > $VM_HOME/out/$l.log 2>&1 < /dev/null & true" >/dev/null 2>&1
  for i in $(seq 60); do present "$l" && return 0; sleep 0.5; done; return 1; }
down(){ $SSH $VMU@$V "pkill -9 -f 'subdomain $1' 2>/dev/null; true" >/dev/null 2>&1
  for i in $(seq 25); do present "$1" || break; sleep 1; done; }

burst(){ # <label> -> "MB/s ena_delta"
  local l=$1 i tmp e0 e1; tmp=$(mktemp -d)
  e0=$(ena)
  for i in $(seq "$PAR"); do
    ( curl -s -o /dev/null -m "$DUR" -w '%{speed_download}\n' \
        "https://$l.$GW/stream/$((96*1073741824))" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  e1=$(ena)
  LC_ALL=C awk -v a="${e0:-0}" -v b="${e1:-0}" '{s+=$1} END{printf "%.2f %d", s/1048576, b-a}' "$tmp"/*
  rm -rf "$tmp"
}

REF_FLAGS=(--carriers 8)
echo "paired workstation-as-consumer: reference = relay c=8, ${PAR} parallel streams, ${DUR}s bursts, ${COOL}s cooldown"
for cfg in "c0auto:--carriers 0" "quic-c1:--carriers 1 --udp" "quic-c4:--carriers 4 --udp" "quic-c0auto:--carriers 0 --udp"; do
  tag=${cfg%%:*}; flags=${cfg#*:}
  LR="wr$(date +%s%N | cut -c7-13)"; LT="wt$(date +%s%N | cut -c7-13)"
  if ! up "$LR" "${REF_FLAGS[@]}"; then echo "  $tag: reference registration FAILED"; continue; fi
  # shellcheck disable=SC2086
  if ! up "$LT" $flags; then echo "  $tag: registration FAILED"; down "$LR"; continue; fi
  curl -fsS -o /dev/null -m 30 "https://$LR.$GW/100k" 2>/dev/null
  curl -fsS -o /dev/null -m 30 "https://$LT.$GW/100k" 2>/dev/null
  d0=$(fld "$LT" direct_stream_opens); curl -fsS -o /dev/null -m 30 "https://$LT.$GW/100k" 2>/dev/null
  path=relay; [ "$(fld "$LT" direct_stream_opens)" -gt "${d0:-0}" ] && path=direct
  echo "  === $tag (proven path=$path) against relay c=8 ==="
  rs=()
  for n in $(seq "$PAIRS"); do
    sleep "$COOL"
    if [ $((n % 2)) -eq 1 ]; then read -r a ae <<< "$(burst "$LR")"; sleep "$COOL"; read -r b be <<< "$(burst "$LT")"
    else read -r b be <<< "$(burst "$LT")"; sleep "$COOL"; read -r a ae <<< "$(burst "$LR")"; fi
    r=$(LC_ALL=C awk -v a="$a" -v b="$b" 'BEGIN{if(a>0) printf "%.3f", b/a; else print "na"}')
    rs+=("$r")
    printf "    pair %s: ref=%-8s cfg=%-8s ratio=%-7s allowance_misses ref=+%s cfg=+%s\n" "$n" "$a" "$b" "$r" "$ae" "$be"
  done
  # LC_ALL=C on the sort (V-11): a ratio of exactly `1` among decimals is the
  # shape that breaks under a comma-decimal locale, and ratios are what this
  # line publishes.
  printf '%s\n' "${rs[@]}" | LC_ALL=C sort -n | awk '{v[NR]=$1} END{printf "    median ratio cfg/ref: %s\n", v[int((NR+1)/2)]}'
  echo "    pool cfg: carriers=$(fld "$LT" carriers) target=$(fld "$LT" carrier_target) path=$(fld "$LT" current_path) fallbacks=$(fld "$LT" direct_fallbacks)"
  down "$LT"; down "$LR"
done
echo DONE
