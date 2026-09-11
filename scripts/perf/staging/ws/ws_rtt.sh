#!/usr/bin/env bash
# RTT with sub-millisecond resolution. `/usr/bin/time -f %e` only resolves 10 ms,
# the same order as the value, so it reported a suspiciously round 17.00/16.00.
# curl's %{time_connect} is the TCP handshake itself, in microseconds.
# ICMP is blocked to both AWS hosts, so this is the only way to get an RTT.
# (`asort` is a gawk extension and silently produced nothing under mawk; the
# median comes from sort(1) instead.)
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
for hp in "$V:22" "$SRV:443" "$GW:443"; do
  h=${hp%%:*}; p=${hp##*:}
  f=$(mktemp)
  for i in $(seq 12); do
    curl -s -o /dev/null --connect-timeout 5 --max-time 6 -w '%{time_connect}\n' \
      "http://$h:$p/" 2>/dev/null | grep -v '^0.000000$' >> "$f" || true
  done
  sort -n "$f" | LC_ALL=C awk -v l="$hp" '{v[NR]=$1*1000; s+=$1*1000}
    END{if(NR) printf "  %-26s min=%.2f median=%.2f mean=%.2f ms over %d\n", l, v[1], v[int((NR+1)/2)], s/NR, NR; else printf "  %-26s no samples\n", l}'
  rm -f "$f"
done
