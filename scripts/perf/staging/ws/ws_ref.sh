#!/usr/bin/env bash
# Workstation-side reference measurements — the ceiling the tunnel is judged
# against. Run BEFORE any tunnel measurement from this host.
#
# Three separate ceilings, because a single number cannot tell them apart:
#   R1  the radio link itself (this host is on WiFi 6; there is no wired link up)
#   R2  the ISP link, measured against a well-provisioned public endpoint
#   R3  the capacity of THIS path to the AWS VM, measured without bore at all,
#       via ssh with an AEAD cipher and no compression. ssh is a lower bound:
#       if bore beats it, the bound was ssh; if bore is far below it, the path
#       was not the limit.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"


mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%.2f MB/s (%.0f Mbit/s)", b/1048576, b*8/1000000}'; }

echo "=== R1 radio link ==="
iw dev $WIFI_IFACE link 2>/dev/null | sed -n 's/^\t/  /p' | grep -E 'signal|bitrate|freq|SSID'
echo "  (PHY bitrate is the modulation rate, not goodput; 802.11 overhead and"
echo "   half-duplex airtime typically leave 40-60 % of it for TCP)"

echo
echo "=== R2 ISP link, public reference endpoint (Cloudflare) ==="
for n in 209715200; do
  d=$(curl -s -o /dev/null -m 90 -w '%{speed_download}' "https://speed.cloudflare.com/__down?bytes=$n" 2>/dev/null || echo 0)
  echo "  down 200 MiB: $(mbs "${d%%.*}")"
done
u=$(head -c 104857600 /dev/zero | curl -s -o /dev/null -m 90 -X POST --data-binary @- -H 'Content-Type: application/octet-stream' \
      -w '%{speed_upload}' "https://speed.cloudflare.com/__up" 2>/dev/null || echo 0)
echo "  up   100 MiB: $(mbs "${u%%.*}")"

echo
echo "=== R3 path capacity to the AWS VM without bore (ssh, aes128-gcm, no compression) ==="
t0=$(date +%s.%N)
$SSH -c aes128-gcm@openssh.com -o Compression=no $VMU@$V 'dd if=/dev/zero bs=1M count=1024 2>/dev/null' > /dev/null
t1=$(date +%s.%N)
echo "  VM -> workstation 1 GiB: $(LC_ALL=C awk -v s="$t0" -v e="$t1" 'BEGIN{printf "%.2f MB/s (%.0f Mbit/s)", 1024/(e-s), 1024*8/(e-s)}')"
t0=$(date +%s.%N)
dd if=/dev/zero bs=1M count=512 2>/dev/null | $SSH -c aes128-gcm@openssh.com -o Compression=no $VMU@$V 'cat > /dev/null'
t1=$(date +%s.%N)
echo "  workstation -> VM 512 MiB: $(LC_ALL=C awk -v s="$t0" -v e="$t1" 'BEGIN{printf "%.2f MB/s (%.0f Mbit/s)", 512/(e-s), 512*8/(e-s)}')"

echo
echo "=== RTT workstation -> server (TCP connect to 443; ICMP blocked) ==="
T=()
for i in $(seq 10); do T+=("$(curl -s -o /dev/null -m 8 -w '%{time_connect}' "https://$GW:443/" 2>/dev/null || echo 9)"); done
printf '%s\n' "${T[@]}" | LC_ALL=C sort -g | awk '{a[NR]=$1} END{printf "  n=%d min=%.2f median=%.2f max=%.2f ms\n", NR, a[1]*1000, a[int((NR+1)/2)]*1000, a[NR]*1000}'
echo "=== RTT workstation -> VM (TCP connect to 22) ==="
T=()
for i in $(seq 10); do T+=("$(curl -s -o /dev/null -m 8 -w '%{time_connect}' "telnet://$V:22" 2>/dev/null || echo 9)"); done
printf '%s\n' "${T[@]}" | LC_ALL=C sort -g | awk '{a[NR]=$1} END{printf "  n=%d min=%.2f median=%.2f max=%.2f ms\n", NR, a[1]*1000, a[int((NR+1)/2)]*1000, a[NR]*1000}'
echo DONE
