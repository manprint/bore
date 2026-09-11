#!/usr/bin/env bash
# Sample the server's instance network-allowance counters on a timeline for the
# whole duration of a campaign stage, so every measurement can be checked
# afterwards against whether the instance was being shaped while it ran.
#
# The counters are cumulative since boot and therefore meaningless as absolute
# values; what matters is the per-interval delta. One line per sample:
#
#   <iso8601> in=+<d> out=+<d> pps=+<d> conntrack=+<d>
#
#   res/ena_timeline.sh <seconds> [interval] > out/ena.timeline
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

DUR="${1:-600}"; IV="${2:-10}"
read_counters() {
    srv "sudo -n ethtool -S $BORE_SRV_IFACE 2>/dev/null \
         | grep -E 'bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded|conntrack_allowance_exceeded' \
         | tr -d ' '" 2>/dev/null
}
prev="$(read_counters)"
[ -n "$prev" ] || { echo "no allowance counters (server unreachable or no ethtool)"; exit 1; }
end=$(( $(date +%s) + DUR ))
while [ "$(date +%s)" -lt "$end" ]; do
    sleep "$IV"
    cur="$(read_counters)"
    [ -n "$cur" ] || continue
    join -t: <(printf '%s\n' "$prev" | sort) <(printf '%s\n' "$cur" | sort) \
      | awk -F: -v ts="$(date -Is)" '
          {d=$3-$2; k=$1; sub(/_allowance_exceeded/,"",k); printf "%s%s=+%d ", (NR==1?ts" ":""), k, d}
          END{print ""}'
    prev="$cur"
done
