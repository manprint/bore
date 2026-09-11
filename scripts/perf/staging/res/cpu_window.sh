#!/usr/bin/env bash
# Reduce a `res_sampler.sh` capture to the CPU cost of one measurement window.
#
# This is the other half of `vm_pub_eff.sh`: that script prints
# `window=<t0>-<t1> ... gib=<n>` per case, this one turns a host's /proc/stat
# samples over the same window into CPU SECONDS, and divides.
#
# Why the HOST and not the container: the container's own accounting misses the
# softirq the kernel spends on its behalf, which the vhost campaign measured at
# 37-40 % of the bill. Reading the container alone understates the cost by a
# third, and "is the server application-limited?" is exactly the question that
# error changes the answer to.
#
# Steal is reported separately and NOT counted as work: on a burstable instance
# it is the only in-guest evidence that the credit bucket is throttling, so
# folding it into "busy" would attribute the cloud's rationing to bore.
#
#   cpu_window.sh <prefix.stat> <t0-epoch> <t1-epoch> [gib] [proc-prefix.proc]
#
# Prints one line:
#   busy=<s> user=<s> sys=<s> softirq=<s> irq=<s> iowait=<s> steal=<s> \
#     wall=<s> cores_busy=<n> [gib=<n> cpu_s_per_gib=<n>] [bore_cpu_s=<n>]
set -uo pipefail
STAT="${1:?usage: cpu_window.sh <prefix.stat> <t0> <t1> [gib] [prefix.proc]}"
T0="${2:?}"; T1="${3:?}"
GIB="${4:-}"
PROC="${5:-}"
HZ="$(getconf CLK_TCK 2>/dev/null || echo 100)"

[ -r "$STAT" ] || { echo "no such stat file: $STAT" >&2; exit 2; }

LC_ALL=C awk -v t0="$T0" -v t1="$T1" -v hz="$HZ" -v gib="$GIB" '
# columns: ts user nice sys idle iowait irq softirq steal load memtotal memavail
{
    ts=$1
    if (ts < t0 || ts > t1) next
    if (n == 0) { for (i=1;i<=9;i++) a[i]=$i; first=ts }
    for (i=1;i<=9;i++) b[i]=$i
    last=ts
    n++
}
END {
    if (n < 2) { print "no samples in the window (need at least two)"; exit 1 }
    user=(b[2]-a[2])/hz; nice=(b[3]-a[3])/hz; sys=(b[4]-a[4])/hz
    iow=(b[6]-a[6])/hz;  irq=(b[7]-a[7])/hz;  sirq=(b[8]-a[8])/hz
    steal=(b[9]-a[9])/hz
    busy=user+nice+sys+irq+sirq
    wall=last-first
    printf "busy=%.2f user=%.2f sys=%.2f softirq=%.2f irq=%.2f iowait=%.2f steal=%.2f wall=%d samples=%d cores_busy=%.2f",
        busy, user+nice, sys, sirq, irq, iow, steal, wall, n, (wall>0 ? busy/wall : 0)
    if (gib != "" && gib+0 > 0) printf " gib=%s cpu_s_per_gib=%.2f", gib, busy/(gib+0)
    printf "\n"
}' "$STAT"

if [ -n "$PROC" ] && [ -r "$PROC" ]; then
    # Per-process: cputimes is a lifetime counter, so the delta across the
    # window is this process' own CPU seconds inside it. Summed per command
    # name, because a forwarder under test may be several processes.
    LC_ALL=C awk -v t0="$T0" -v t1="$T1" '
    # columns: ts pid rss cputime comm
    $1 >= t0 && $1 <= t1 {
        key=$5 "/" $2
        if (!(key in lo)) { lo[key]=$4; name[key]=$5 }
        hi[key]=$4
        if ($3+0 > rss[$5]) rss[$5]=$3+0
    }
    END {
        for (k in hi) { d=hi[k]-lo[k]; if (d > 0) tot[name[k]] += d }
        out=""
        for (c in tot) out = out sprintf(" %s_cpu_s=%d %s_peak_rss_kb=%d", c, tot[c], c, rss[c])
        if (out != "") printf " process:%s\n", out
    }' "$PROC"
fi
