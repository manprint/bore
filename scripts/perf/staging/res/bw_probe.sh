#!/usr/bin/env bash
# Gate for every bandwidth measurement in this campaign: an 8 s single-stream
# download from the VM-side provider, plus the server's inbound-allowance delta
# across exactly that window. Two possible answers:
#   rate ~50+ MB/s and a small delta  -> the instance has burst budget, measure
#   rate ~7 MB/s and a large delta    -> the instance is pinned at its baseline
#                                        and any comparison made now is a
#                                        comparison of AWS shaping, not of bore
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"



adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
ena(){ $SSH $SRVU@$S "sudo -n ethtool -S $IFACE | grep -E 'bw_in_allowance_exceeded' | tr -dc '0-9'" 2>/dev/null; }
L="pb$(date +%s%N | cut -c7-13)"
$SSH $VMU@$V "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$ORIGIN_PORT --subdomain $L --id $L \
    --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 > $VM_HOME/out/$L.log 2>&1 < /dev/null & true" >/dev/null 2>&1
for i in $(seq 60); do present "$L" && break; sleep 0.5; done
present "$L" || { echo "  probe: REGISTRATION FAILED"; exit 1; }
e0=$(ena)
r=$(curl -s -o /dev/null -m 8 -w '%{speed_download}' "https://$L.$GW/stream/$((8*1073741824))")
e1=$(ena)
$SSH $VMU@$V "pkill -9 -f 'subdomain $L' 2>/dev/null; true" >/dev/null 2>&1
LC_ALL=C awk -v r="${r:-0}" -v a="${e0:-0}" -v b="${e1:-0}" 'BEGIN{
  printf "  probe: %.2f MB/s (%.0f Mbit/s)  bw_in_allowance_exceeded +%d  -> %s\n",
    r/1048576, r*8/1000000, b-a, (r/1048576 > 30 ? "budget AVAILABLE" : "THROTTLED at baseline")}'
