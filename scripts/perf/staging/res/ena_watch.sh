#!/usr/bin/env bash
# Is the server currently being throttled by its instance network allowance?
# The counters are cumulative since boot, so only a DELTA over a known idle
# window answers it: a delta > 0 with no load means the instance is still
# shaping traffic that is not ours; a delta of 0 means the budget has recovered
# and a bandwidth measurement taken now is a measurement of bore.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

g(){ ssh -o BatchMode=yes -o StrictHostKeyChecking=no -i "$BORE_SSH_KEY" $SRVU@$S \
      "sudo -n ethtool -S $IFACE | grep -E 'bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded' | tr -d ' '" 2>/dev/null; }
a=$(g); sleep "${1:-30}"; b=$(g)
join -t: <(printf '%s\n' "$a" | sort) <(printf '%s\n' "$b" | sort) \
  | awk -F: -v w="${1:-30}" '{d=$3-$2; printf "  %-30s +%d over %ss (%.0f/s)\n", $1, d, w, d/w}'
