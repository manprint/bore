#!/usr/bin/env bash
# Sample the AWS ENA instance-allowance counters on the bore server.
#
# Why this exists: a throughput plateau on a cloud instance has three possible
# causes -- the application, the guest CPU, or the hypervisor's own token
# buckets. Only the third is invisible from inside the guest's usual tools.
# `ethtool -S <if>` exposes bw_in_allowance_exceeded / bw_out_allowance_exceeded
# / pps_allowance_exceeded, which say directly whether the instance is being
# throttled. Pair it with server_cpu_sample.sh: if CPU is near the core count
# the guest is the wall; if CPU is low and these counters are climbing, the
# instance allowance is the wall and no application change will help.
#
# The counters are CUMULATIVE SINCE BOOT. A raw reading is meaningless -- an
# instance that has been up for weeks shows millions of events. Only the DELTA
# across a measured window attributes throttling to the traffic under test.
#
# Output, one line per sample, whitespace separated:
#   <epoch> <bw_in_exc> <bw_out_exc> <pps_exc> <rx_bytes> <rx_pkts> <tx_bytes> <tx_pkts>
# The last four come from /proc/net/dev, so tx_bytes/tx_pkts gives the mean wire
# packet size -- needed to tell a genuine pps problem from a small-packet one.
#
# Requires passwordless `sudo -n ethtool` on the server (read-only). Without it
# the three allowance fields come back empty and only /proc/net/dev is usable.
#
# Usage: BORE_SERVER_IP=... BORE_SERVER_KEY=... server_ena_sample.sh [n] [interval]
set -uo pipefail
: "${BORE_SERVER_IP:?set BORE_SERVER_IP}"
: "${BORE_SERVER_KEY:?set BORE_SERVER_KEY}"
SRVUSER="${BORE_SERVER_USER:-ubuntu}"
N="${1:-70}"; IV="${2:-2}"
# Server-side interface name; discovered from the default route when unset.
IFACE="${BORE_SERVER_IFACE:-}"

read -r -d '' REMOTE <<'REMOTE_EOF'
IF="$1"; N="$2"; IV="$3"
[ -n "$IF" ] || IF=$(ip -o -4 route show default | awk '{print $5; exit}')
for _ in $(seq "$N"); do
  a=$(sudo -n ethtool -S "$IF" 2>/dev/null \
      | awk '/bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded/ {printf "%s ", $2}')
  [ -n "$a" ] || a="- - - "
  d=$(awk -v i="$IF:" '$1 == i {printf "%s %s %s %s", $2, $3, $10, $11}' /proc/net/dev)
  echo "$(date +%s) ${a}${d}"
  sleep "$IV"
done
REMOTE_EOF

timeout $((N * IV + 60)) ssh -o BatchMode=yes -o ServerAliveInterval=30 \
  -i "$BORE_SERVER_KEY" "$SRVUSER@$BORE_SERVER_IP" \
  "bash -s -- '$IFACE' '$N' '$IV'" <<< "$REMOTE" 2>/dev/null
