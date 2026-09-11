#!/usr/bin/env bash
# R3 — the workstation<->VM path capacity WITHOUT bore in it, so the tunnel
# results in §4/§5 have a ceiling to be judged against.
#
# There is no open inbound port on the VM other than SSH, and no AWS credentials
# here to add one, so the reference has to ride SSH. A single SSH channel is
# capped by the OpenSSH client's own 2 MiB window (documented in
# `ssh-gateway-throughput`), which is a property of ssh and not of the path — so
# the honest reference is the AGGREGATE over N independent SSH connections, each
# with its own window. It is a LOWER bound on the path (every byte is
# AES-GCM'd twice and copied through two userspace processes), and it is
# labelled as such.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"


MB=192
mbs(){ LC_ALL=C awk -v b="$1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576, b*8/1000000}'; }

rtt(){ # <host> <port> — ICMP is blocked to both AWS hosts, so TCP connect it is
  local h=$1 p=$2 i t s=0 n=0
  for i in $(seq 10); do
    t=$( { LC_ALL=C /usr/bin/time -f %e bash -c "exec 3<>/dev/tcp/$h/$p" ; } 2>&1 )
    case "$t" in ''|*[!0-9.]*) continue;; esac
    s=$(LC_ALL=C awk -v s="$s" -v t="$t" 'BEGIN{print s+t}'); n=$((n+1))
  done
  [ "$n" -gt 0 ] && LC_ALL=C awk -v s="$s" -v n="$n" 'BEGIN{printf "%.2f ms over %d samples", s*1000/n, n}' || echo "n/a"
}

echo "=== R3 workstation <-> VM, no tunnel ==="
echo "  RTT to VM:22      : $(rtt $V 22)"
echo "  RTT to server:443 : $(rtt $S 443)"
echo
for n in 1 4 8; do
  tmp=$(mktemp -d); t0=$(date +%s.%N)
  for i in $(seq "$n"); do
    ( $SSH $VMU@$V "head -c $((MB*1048576)) /dev/zero" 2>/dev/null | wc -c > "$tmp/$i" ) &
  done
  wait; t1=$(date +%s.%N)
  by=$(LC_ALL=C awk '{s+=$1} END{printf "%.0f", s}' "$tmp"/*)
  echo "  DOWN x$n (ssh, lower bound): $(LC_ALL=C awk -v b="$by" -v s="$t0" -v e="$t1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576/(e-s), b*8/1000000/(e-s)}')"
  rm -rf "$tmp"
done
for n in 1 4 8; do
  t0=$(date +%s.%N)
  for i in $(seq "$n"); do
    ( head -c $((MB*1048576)) /dev/zero | $SSH $VMU@$V "cat > /dev/null" 2>/dev/null ) &
  done
  wait; t1=$(date +%s.%N)
  by=$((n*MB*1048576))
  echo "  UP   x$n (ssh, lower bound): $(LC_ALL=C awk -v b="$by" -v s="$t0" -v e="$t1" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", b/1048576/(e-s), b*8/1000000/(e-s)}')"
done
echo DONE
