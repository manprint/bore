#!/usr/bin/env python3
"""Turn `ws_conns_var`'s cell rows into the two numbers the question needs.

The stage prints one row per cell carrying the rate AND the counters read as
deltas around that very transfer.  Reading the correlation off the page is how
a reader ends up believing whichever pattern they went looking for, so it is
computed here instead, from the stage's own output, with no second measurement
and no hand-copied numbers.

    ws_conns_var.out  ->  per (arm, rung):  spread of the repeated cell
                                            Spearman rho of rate vs each counter

WHY SPEARMAN AND NOT PEARSON
----------------------------
`bw_in_allowance_exceeded` is a count of shaping events and is zero in most
cells and in the hundreds in a few: that is not a linear predictor of a rate and
nothing says it should be.  The claim under test is ORDINAL -- "the slow cells
are the shaped ones" -- so the rank correlation is the statistic that states it,
and it is not moved by one cell with a huge count.

WHAT THE OUTPUT IS ALLOWED TO CONCLUDE
--------------------------------------
With n around 15 a |rho| of ~0.5 is roughly the 5 % two-sided point, so the
report prints n beside every rho and refuses to call anything at n < 6.  A rho
is evidence of association and never of direction: the allowance bucket
throttling the transfer, and a faster transfer tripping the bucket harder, both
produce a non-zero rho and have OPPOSITE signs -- which is exactly why the sign
is printed rather than the magnitude alone.
"""
import re, sys, math
from collections import defaultdict

ROW = re.compile(
    r'^\s*rep\s+(\d+)\s+n=(\d+)\s+(\S+)\s+(\S+)\s+([\d.]+)s\s*\|\s*'
    r'srv\s+in\+(\S+)\s+out\+(\S+)\s+pps\+(\S+)\s+ct\+(\S+)\s+'
    r'busy=(\S+)\s+steal=(\S+)\s*\|\s*'
    r'vm\s+in\+(\S+)\s+out\+(\S+)\s+pps\+(\S+)\s+ct\+(\S+)\s+'
    r'busy=(\S+)\s+steal=(\S+)')

def num(x):
    try:
        return float(str(x).rstrip('%'))
    except ValueError:
        return None

def ranks(xs):
    """Fractional ranks, ties averaged -- ties are the norm here (most counters
    read 0), and integer ranks would invent an ordering among them."""
    order = sorted(range(len(xs)), key=lambda i: xs[i])
    r = [0.0] * len(xs)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and xs[order[j + 1]] == xs[order[i]]:
            j += 1
        avg = (i + j) / 2.0 + 1
        for k in range(i, j + 1):
            r[order[k]] = avg
        i = j + 1
    return r

def spearman(a, b):
    if len(a) < 3:
        return None
    ra, rb = ranks(a), ranks(b)
    n = len(a)
    ma, mb = sum(ra) / n, sum(rb) / n
    num_ = sum((x - ma) * (y - mb) for x, y in zip(ra, rb))
    da = math.sqrt(sum((x - ma) ** 2 for x in ra))
    db = math.sqrt(sum((y - mb) ** 2 for y in rb))
    if da == 0 or db == 0:
        return None          # a constant column cannot correlate with anything
    return num_ / (da * db)

def main(path):
    # THE STAGE PRINTS EVERY CELL TWICE, ON PURPOSE, AND THIS PARSER MUST NOT.
    #
    # `ws_conns_var` prints each row inline as it measures (so a run in progress
    # is readable) and then prints all of them again under "every cell again",
    # which is the evidence block a reader quotes. Parsing the whole file counts
    # each cell twice: the medians survive it -- a doubled multiset has the same
    # median -- but `n` doubles, and `n` is what decides whether a rho means
    # anything. MEASURED: the first run of this script reported 30 repetitions of
    # a 15-repetition cell, which would have made every correlation look twice as
    # well-supported as it is. Deduplicating identical rows would be the wrong
    # fix: two cells CAN legitimately produce the same numbers, and a parser must
    # not silently discard a real measurement to work around its own double read.
    # So the second block is skipped by POSITION, which is exactly what it is.
    cells = defaultdict(list)
    for line in open(path, encoding='utf-8', errors='replace'):
        if line.startswith('=== every cell again'):
            break
        m = ROW.match(line)
        if not m:
            continue
        rep, n, arm, mbs = m.group(1), m.group(2), m.group(3), m.group(4)
        rate = num(mbs)
        if rate is None:           # FAILED: a fact, never a number
            continue
        cells[(arm, int(n))].append({
            'rate': rate,
            'srv_in':   num(m.group(6)),  'srv_out': num(m.group(7)),
            'srv_pps':  num(m.group(8)),
            'srv_busy': num(m.group(10)), 'srv_steal': num(m.group(11)),
            'vm_busy':  num(m.group(16)), 'vm_steal': num(m.group(17)),
        })

    if not cells:
        print("no cell rows parsed -- is this a ws_conns_var output?")
        return 2

    print("=== spread of the repeated cell ===")
    print(f"  {'arm':<6} {'n':<3} {'min':>8} {'median':>8} {'max':>8} {'escursione':>11} {'reps':>5}")
    for (arm, n), rows in sorted(cells.items(), key=lambda kv: (kv[0][1], kv[0][0])):
        v = sorted(r['rate'] for r in rows)
        k = len(v)
        med = v[k // 2] if k % 2 else (v[k // 2 - 1] + v[k // 2]) / 2
        exc = 100 * (v[-1] - v[0]) / med if med else 0
        print(f"  {arm:<6} {n:<3} {v[0]:8.2f} {med:8.2f} {v[-1]:8.2f} {exc:10.1f}% {k:5d}")

    print()
    print("=== rank correlation of the RATE with each candidate (Spearman rho) ===")
    print("  a NEGATIVE rho against a counter means the cells that tripped it were")
    print("  the SLOW ones -- the shape that implicates it. A positive rho means")
    print("  the FAST cells tripped it, i.e. the counter is following the traffic.")
    keys = ['srv_in', 'srv_out', 'srv_pps', 'srv_busy', 'srv_steal', 'vm_busy', 'vm_steal']
    print(f"  {'arm':<6} {'n':<3} " + " ".join(f"{k:>10}" for k in keys) + f" {'reps':>5}")
    for (arm, n), rows in sorted(cells.items(), key=lambda kv: (kv[0][1], kv[0][0])):
        rate = [r['rate'] for r in rows]
        out = []
        for k in keys:
            col = [r[k] for r in rows]
            if any(c is None for c in col):
                out.append(f"{'?':>10}")
                continue
            rho = spearman(rate, col)
            out.append(f"{'flat':>10}" if rho is None else f"{rho:10.2f}")
        note = "" if len(rows) >= 6 else "  (n<6: not read)"
        print(f"  {arm:<6} {n:<3} " + " ".join(out) + f" {len(rows):5d}{note}")

    print()
    print("=== the slowest cells, with everything that was true around them ===")
    print("  A correlation over fifteen points can be carried by one cell, and a")
    print("  campaign that only printed rho would never know which. These are the")
    print("  three slowest cells of each series, beside the counters read around")
    print("  that very transfer -- so a reader can check whether the association")
    print("  the rho reports is one event or a pattern.")
    hdr = f"  {'arm':<6} {'n':<3} {'MiB/s':>8} {'srv_in':>8} {'srv_pps':>9} {'srv_busy':>9} {'srv_steal':>10} {'vm_busy':>8} {'vm_steal':>9}"
    print(hdr)
    for (arm, n), rows in sorted(cells.items(), key=lambda kv: (kv[0][1], kv[0][0])):
        for r in sorted(rows, key=lambda r: r['rate'])[:3]:
            def f(k, w, suffix=''):
                v = r[k]
                return f"{'?':>{w}}" if v is None else f"{v:{w}.2f}{suffix}" if isinstance(v, float) else f"{v:>{w}}"
            print(f"  {arm:<6} {n:<3} {r['rate']:8.2f} {f('srv_in',8)} {f('srv_pps',9)} "
                  f"{f('srv_busy',9)} {f('srv_steal',10)} {f('vm_busy',8)} {f('vm_steal',9)}")

    print()
    print("  'flat' means the counter never moved across this cell, which is the")
    print("  strongest possible statement about it: a candidate that does not vary")
    print("  cannot explain a rate that does.")
    return 0

if __name__ == '__main__':
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else 'out/eth/pub_ws_conns_var.out'))
