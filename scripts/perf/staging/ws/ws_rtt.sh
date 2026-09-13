#!/usr/bin/env bash
# RTT with sub-millisecond resolution. `/usr/bin/time -f %e` only resolves 10 ms,
# the same order as the value, so it reported a suspiciously round 17.00/16.00.
# curl's %{time_connect} is the TCP handshake itself, in microseconds.
# ICMP is blocked to both AWS hosts, so this is the only way to get an RTT.
# (`asort` is a gawk extension and silently produced nothing under mawk; the
# median comes from sort(1) instead.)
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
# The LABEL is the role, never the address. A results file that names the host
# it probed has published a coordinate, and these files get quoted into
# documents -- which is exactly how the three live hits of the first
# `secret_scan` run got there. The role is also what a reader needs: nobody
# reads an RTT table to find out which IP answered.
for hp in "test-vm:$V:22" "server:$SRV:443" "gateway:$GW:443"; do
  lbl=${hp%%:*}; rest=${hp#*:}; h=${rest%%:*}; p=${rest##*:}
  f=$(mktemp)
  for i in $(seq 12); do
    curl -s -o /dev/null --connect-timeout 5 --max-time 6 -w '%{time_connect}\n' \
      "http://$h:$p/" 2>/dev/null | grep -v '^0.000000$' >> "$f" || true
  done
  # LC_ALL=C on the SORT as well as on the awk (V-11). Under a comma-decimal
  # locale `sort -n` orders {397.46, 264.01, 408} as {408, 264.01, 397.46} --
  # MEASURED here, on this workstation, under it_IT.UTF-8. It happens not to
  # bite today because curl prints every sample as `0.nnnnnn`, where numeric
  # and lexical order coincide; one sample of 1 s or more and the median this
  # stage publishes would be wrong with nothing in the output to show it.
  LC_ALL=C sort -n "$f" | LC_ALL=C awk -v l="$lbl" '{v[NR]=$1*1000; s+=$1*1000}
    END{if(NR) printf "  %-26s min=%.2f median=%.2f mean=%.2f ms over %d\n", l, v[1], v[int((NR+1)/2)], s/NR, NR; else printf "  %-26s no samples\n", l}'
  rm -f "$f"
done
