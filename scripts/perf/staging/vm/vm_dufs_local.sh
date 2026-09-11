#!/usr/bin/env bash
# dufs measured on the VM's own loopback, with the SAME request shapes the
# workstation uses through the tunnel. Without this, every through-tunnel number
# is unattributable: 66 ms for an 8 KiB file could be the tunnel, the WAN RTT,
# or dufs itself, and only a local baseline separates them.
set -uo pipefail
H=$VM_HOME; U="http://127.0.0.1:$DUFS_PORT"
W=$H/dufscfg; mkdir -p "$W"
rate(){ LC_ALL=C awk -v by="$1" -v s="$2" -v e="$3" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", by/1048576/(e-s), by*8/1000000/(e-s)}'; }

echo "=== dufs on loopback, no tunnel ==="
echo "  single 256 MiB range read: $(curl -s -o /dev/null -m 60 -r 0-268435455 -w '%{speed_download}' "$U/big1g.bin" | awk '{printf "%.2f MB/s", $1/1048576}')"

: > "$W/med.cfg"
for i in $(seq 10); do printf 'url = "%s/med/m%s.bin"\noutput = "/dev/null"\n' "$U" "$i" >> "$W/med.cfg"; done
t0=$(date +%s.%N); curl -sZ --parallel-max 10 -K "$W/med.cfg" -w '%{size_download}\n' > "$W/o" 2>/dev/null; t1=$(date +%s.%N)
by=$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$W/o")
echo "  MED 10 x 20 MiB parallel: $(rate "$by" "$t0" "$t1")"

: > "$W/small.cfg"
for i in $(seq 500); do printf 'url = "%s/small/s%s.bin"\noutput = "/dev/null"\n' "$U" "$i" >> "$W/small.cfg"; done
for pm in 1 8 32; do
  t0=$(date +%s.%N); curl -sZ --parallel-max "$pm" -K "$W/small.cfg" >/dev/null 2>&1; t1=$(date +%s.%N)
  LC_ALL=C awk -v s="$t0" -v e="$t1" -v pm="$pm" 'BEGIN{printf "  SMALL 500 x 8 KiB, parallel-max %s: %.2fs, %.2f ms per file, %.0f files/s\n", pm, e-s, (e-s)*1000/500, 500/(e-s)}'
done
head -c 8192 /dev/zero > "$W/s8k.bin"
: > "$W/sup.cfg"
for i in $(seq 500); do printf 'url = "%s/upload/l%s.bin"\nupload-file = "%s"\noutput = "/dev/null"\n' "$U" "$i" "$W/s8k.bin" >> "$W/sup.cfg"; done
for pm in 1 32; do
  t0=$(date +%s.%N); curl -sZ --parallel-max "$pm" -K "$W/sup.cfg" -H 'Expect:' >/dev/null 2>&1; t1=$(date +%s.%N)
  LC_ALL=C awk -v s="$t0" -v e="$t1" -v pm="$pm" 'BEGIN{printf "  SMALL 500 PUTs, parallel-max %s: %.2fs, %.2f ms per file, %.0f files/s\n", pm, e-s, (e-s)*1000/500, 500/(e-s)}'
done
echo "  oha -c 8 on one small file: $(timeout 30 $H/oha -z 8s -c 8 --no-tui --output-format json "$U/small/s1.bin" 2>/dev/null | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95)"')"
$H/vm_dufs_setup.sh clean >/dev/null 2>&1
echo DONE
