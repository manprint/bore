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

HOSTLINE="$(LC_ALL=C awk -v t0="$T0" -v t1="$T1" -v hz="$HZ" -v gib="$GIB" '
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
}' "$STAT")"; HRC=$?
printf '%s\n' "$HOSTLINE"
[ "$HRC" = 0 ] || exit "$HRC"

PROCLINE=""
PROCTOT=0
if [ -n "$PROC" ] && [ -r "$PROC" ]; then
    # Per-process: cputimes is a lifetime counter, so the delta across the
    # window is this process' own CPU seconds inside it. Summed per command
    # name, because a forwarder under test may be several processes.
    #
    # `#total` is the sum over every matched command and is consumed by the
    # attribution check below, never printed.
    OUT="$(LC_ALL=C awk -v t0="$T0" -v t1="$T1" '
    # columns: ts pid rss cputime comm
    $1 >= t0 && $1 <= t1 {
        key=$5 "/" $2
        if (!(key in lo)) { lo[key]=$4; name[key]=$5 }
        hi[key]=$4
        if ($3+0 > rss[$5]) rss[$5]=$3+0
    }
    END {
        grand=0
        for (k in hi) { d=hi[k]-lo[k]; if (d > 0) { tot[name[k]] += d; grand += d } }
        out=""
        for (c in tot) out = out sprintf(" %s_cpu_s=%d %s_peak_rss_kb=%d", c, tot[c], c, rss[c])
        if (out != "") printf " process:%s\n", out
        printf "#total %d\n", grand
    }' "$PROC")"
    PROCLINE="$(printf '%s\n' "$OUT" | grep -v '^#total ')"
    PROCTOT="$(printf '%s\n' "$OUT" | sed -n 's/^#total //p')"
    [ -n "$PROCLINE" ] && printf '%s\n' "$PROCLINE"
fi

# H-17: host-wide CPU is only attributable on a DEDICATED host.
#
# On a shared machine /proc/stat counts every neighbour — a browser, an IDE, a
# build — and the sampler's process regex does not see them, so `busy` and
# `cpu_s_per_gib` describe the box, not the thing under test. Measured on
# 2026-09-11: a workstation window read busy 41.07 -> 76.89 CPU s between two
# arms while every matched process accounted for 1 s of the 35.8 s difference.
# Read at face value that says the transport doubled the CPU bill; it says
# nothing of the sort.
#
# The check is deliberately loose (4x) and one-directional: softirq, IRQ and
# any unmatched helper legitimately put the host above the matched processes,
# and on a dedicated host that gap is tens of percent, not multiples. Warn,
# never fail — the numbers are still the right ones to record, they are just
# not the ones to quote.
if [ -n "$PROC" ] && [ -r "$PROC" ]; then
    BUSY="$(printf '%s\n' "$HOSTLINE" | sed -n 's/^busy=\([0-9.]*\).*/\1/p')"
    LC_ALL=C awk -v busy="${BUSY:-0}" -v pt="${PROCTOT:-0}" '
    BEGIN {
        gap = busy + 0 - (pt + 0)
        # Two conditions, because either alone misfires on real data:
        #   * a RATIO alone passed a workstation window reading busy=73.70 with
        #     29 s of matched processes (2.5x, under a 4x bar) while 44.7 CPU s
        #     were still someone else s — it would have been quoted as 9.21
        #     s/GiB for a process that spent 2.75;
        #   * a FRACTION alone fires on a dedicated server whose process did no
        #     work at all (busy=3.06, pt=0 — 100 % unattributed, and 0.02 cores,
        #     i.e. nothing worth warning about).
        # So: the unattributed work must be both a MAJORITY of the bill and
        # large in absolute terms. Softirq and IRQ legitimately put a dedicated
        # host 10-25 % above its processes, which is what the 50 % bar allows.
        if (gap <= 10) exit 0
        if (busy + 0 <= 0 || gap / (busy + 0) <= 0.5) exit 0
        printf "  WARNING: host busy=%.2f CPU s but the sampled processes account for %d s.\n", busy, pt
        printf "           On a shared machine `busy` and cpu_s_per_gib are NOT attributable to\n"
        printf "           the process under test (H-17) — quote the per-process delta instead,\n"
        printf "           or widen the sampler regex if the missing work is really yours.\n"
    }' >&2
fi
