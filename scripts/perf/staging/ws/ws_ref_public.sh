#!/usr/bin/env bash
# The workstation's own link, measured against public reference endpoints, so
# every tunnel number in §4/§5 can be read as a fraction of what this link can
# do at all. Deliberately does NOT touch the AWS hosts.
#
# Two apparatus notes that cost time in this campaign:
#   * speed.cloudflare.com/__down now answers HTTP 403 (it did not when the
#     before-document was written), so the download reference is Hetzner
#     Falkenstein. __up still works and is used for upload.
#   * a SINGLE stream is Mathis-bound at this RTT and measures nothing about the
#     link; only the parallel aggregate does.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576, b*8/1000000}'; }
DL="https://fsn1-speed.hetzner.com/100MB.bin"
UP="https://speed.cloudflare.com/__up"

echo "=== R1 the link itself ==="
for i in /sys/class/net/*; do
  n=$(basename "$i"); [ "$n" = lo ] && continue
  echo "  $n: $(cat "$i/operstate" 2>/dev/null)"
done
iw dev $WIFI_IFACE link 2>/dev/null | grep -E 'SSID|freq|signal|bitrate|MCS' | sed 's/^[[:space:]]*/    /'

echo
echo "=== R2 download ceiling, parallel streams to Hetzner FSN1 ==="
for n in 1 2 4 8 16; do
  tmp=$(mktemp -d)
  for i in $(seq "$n"); do
    ( curl -s -o /dev/null -m 90 -w '%{speed_download}\n' "$DL" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  echo "  x$n aggregate: $(mbs "$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/*)")"
  rm -rf "$tmp"
done

echo
echo "=== R2 upload ceiling, parallel streams to Cloudflare __up ==="
for n in 1 2 4; do
  tmp=$(mktemp -d)
  for i in $(seq "$n"); do
    ( head -c 209715200 /dev/zero | curl -s -o /dev/null -m 120 -X POST --data-binary @- \
        -H 'Content-Type: application/octet-stream' -H 'Expect:' -w '%{speed_upload}\n' \
        "$UP" > "$tmp/$i" 2>/dev/null ) &
  done
  wait
  echo "  x$n aggregate: $(mbs "$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/*)")"
  rm -rf "$tmp"
done
echo DONE
