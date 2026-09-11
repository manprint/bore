#!/usr/bin/env bash
# Before building anything on the MTU lever: does the VM->server path actually
# carry more than 1500? quinn caps its DPLPMTUD search at 1452 by default, and
# raising that ceiling is only worth code if the path is jumbo-capable. Two AWS
# instances in ONE VPC get 9001; across the public internet they get 1500, and
# then the whole lever is worth 1.4% and must not be built.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
echo "VM local MTU: $(vm "ip -o link show | sed -n 's/.*\(ens[0-9]*\|eth[0-9]*\).*mtu \([0-9]*\).*/\1=\2/p'" 2>/dev/null | tr '\n' ' ')"
echo "VM default route: $(vm "ip route get 1.1.1.1 | head -1" 2>/dev/null)"
echo "server host from the VM: $BORE_GW"
for sz in 1472 1972 8972; do
  r=$(vm "ping -M do -c 2 -W 2 -s $sz $BORE_GW 2>&1 | tail -3 | tr '\n' ' '" 2>/dev/null)
  echo "  payload=$sz -> $r"
done
echo "tracepath: $(vm "tracepath -n -m 6 $BORE_GW 2>&1 | tail -4 | tr '\n' ' '" 2>/dev/null)"
