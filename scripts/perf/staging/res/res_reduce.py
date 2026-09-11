#!/usr/bin/env python3
"""Reduce res_sampler output to one line per host, plus an optional window.

Host CPU is reported as a percentage of ONE core so it can be compared with the
core count directly (200 % on a 2-vCPU box means both cores saturated), which is
how §2.17 priced bore's cost per byte. Per-process CPU is differentiated from
cumulative cputime, so a process that started mid-window is not credited with
time it did not spend inside it.

usage: res_reduce.py <prefix> [label] [t_start] [t_end]
"""
import sys, os
from collections import defaultdict

pref = sys.argv[1]
label = sys.argv[2] if len(sys.argv) > 2 else os.path.basename(pref)
t0 = float(sys.argv[3]) if len(sys.argv) > 3 else 0
t1 = float(sys.argv[4]) if len(sys.argv) > 4 else 1e18

rows = []
for line in open(pref + ".stat"):
    f = line.split()
    if len(f) < 12:
        continue
    ts = float(f[0])
    if not (t0 <= ts <= t1):
        continue
    rows.append([float(x) for x in f])
if len(rows) < 2:
    print(f"{label}: not enough samples in window")
    sys.exit(0)

# The reducer usually runs on the workstation over samples copied from another
# host, so the core count must be told, not inferred: os.cpu_count() here would
# report the workstation's 16 while pricing the server's 2.
ncpu = int(os.environ.get("NCPU") or os.cpu_count() or 1)
busy_pct, usr, sys_, sirq, steal, loads, memused = [], [], [], [], [], [], []
for a, b in zip(rows, rows[1:]):
    _, u0, n0, s0, i0, io0, ir0, si0, st0, ld0, mt0, ma0 = a[:12]
    _, u1, n1, s1, i1, io1, ir1, si1, st1, ld1, mt1, ma1 = b[:12]
    tot = (u1 + n1 + s1 + i1 + io1 + ir1 + si1 + st1) - (u0 + n0 + s0 + i0 + io0 + ir0 + si0 + st0)
    if tot <= 0:
        continue
    busy = (u1 - u0) + (n1 - n0) + (s1 - s0) + (ir1 - ir0) + (si1 - si0) + (st1 - st0)
    busy_pct.append(100.0 * busy / tot)
    usr.append(100.0 * ((u1 - u0) + (n1 - n0)) / tot)
    sys_.append(100.0 * (s1 - s0) / tot)
    sirq.append(100.0 * ((si1 - si0) + (ir1 - ir0)) / tot)
    steal.append(100.0 * (st1 - st0) / tot)
    loads.append(ld1)
    memused.append((mt1 - ma1) / 1024.0)


def q(v, p):
    if not v:
        return 0.0
    v = sorted(v)
    return v[min(len(v) - 1, int(p * (len(v) - 1)))]


print(f"{label}: cores={ncpu}  host CPU (share of the whole box) "
      f"mean={sum(busy_pct)/len(busy_pct):.1f}% p95={q(busy_pct,0.95):.1f}% max={max(busy_pct):.1f}%"
      f"  | usr={sum(usr)/len(usr):.1f}% sys={sum(sys_)/len(sys_):.1f}% softirq={sum(sirq)/len(sirq):.1f}%"
      f" STEAL={sum(steal)/len(steal):.2f}%  load_max={max(loads):.2f}"
      f"  mem_used mean={sum(memused)/len(memused):.0f}MiB max={max(memused):.0f}MiB")
print(f"    (as core-equivalents on this box: mean={ncpu*sum(busy_pct)/len(busy_pct)/100:.2f} "
      f"max={ncpu*max(busy_pct)/100:.2f} of {ncpu})")

# per-process: cputime delta over the window, peak RSS
first, last, peak = {}, {}, defaultdict(float)
name = {}
if os.path.exists(pref + ".proc"):
    for line in open(pref + ".proc"):
        f = line.split()
        if len(f) < 5:
            continue
        ts, pid, rss, cput, comm = float(f[0]), f[1], float(f[2]), float(f[3]), f[4]
        if not (t0 <= ts <= t1):
            continue
        key = (pid, comm)
        if key not in first:
            first[key] = (ts, cput)
        last[key] = (ts, cput)
        peak[key] = max(peak[key], rss)
        name[key] = comm
agg = defaultdict(lambda: [0.0, 0.0, 0])
for key in first:
    dt = last[key][0] - first[key][0]
    dc = last[key][1] - first[key][1]
    a = agg[name[key]]
    a[0] += dc
    a[1] = max(a[1], peak[key])
    a[2] += 1
span = rows[-1][0] - rows[0][0]
for comm, (cpus, rss, n) in sorted(agg.items(), key=lambda kv: -kv[1][0]):
    if cpus <= 0 and rss <= 0:
        continue
    print(f"    proc {comm:<10} n={n:<4} cpu={cpus:.1f}s over {span:.0f}s "
          f"({100.0*cpus/span if span else 0:.0f}% of one core)  peak_rss={rss/1024:.1f}MiB")
