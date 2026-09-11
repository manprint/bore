#!/usr/bin/env bash
# The ISP DOWNLOAD ceiling needs parallel streams: a single TCP flow to a
# distant endpoint is Mathis-bound (bandwidth <= MSS/(RTT*sqrt(loss))) and says
# nothing about the link. Cloudflare's __down endpoint answers 403 now, so the
# reference is Hetzner Falkenstein, which is well provisioned and ~20 ms away.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576, b*8/1000000}'; }
U="https://fsn1-speed.hetzner.com/100MB.bin"
echo "=== ISP download ceiling, parallel streams to $U ==="
for n in 1 2 4 8 16; do
  tmp=$(mktemp -d)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m 90 -w '%{speed_download}\n' "$U" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  echo "  x$n aggregate: $(mbs "$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/*)")"
  rm -rf "$tmp"
done
echo DONE
