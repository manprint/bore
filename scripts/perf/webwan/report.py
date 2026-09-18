#!/usr/bin/env python3
"""Summary for T-WEB-PERF-WAN.

Prints every raw sample beside every median (V-11: a file that prints only
medians hides the bug that corrupts them), takes the direct/relay ratio PER
REPETITION and only then medians it (V-13: dividing two medians taken minutes
apart hides the drift the alternating order exists to cancel), and refuses to
turn a missing arm into a number (V-9).
"""
import json
import statistics
import sys
from collections import defaultdict


def med(xs):
    return statistics.median(xs) if xs else None


def fmt(x, nd=2):
    return "FAILED" if x is None else f"{x:.{nd}f}"


def main(path):
    rows = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    # The label carries the carrier count when there is one, because a sweep
    # puts several carrier counts in ONE file and a report that folded them
    # together would median across the very axis it is measuring. The arm
    # itself stays pure: it is what the committed path is checked against.
    def label(row):
        arm = row["arm"]
        n = row.get("carriers")
        return arm if n in (None, "", 0) else f"{arm}/c{n}"

    src = defaultdict(dict)   # label -> rep -> payload
    dst = defaultdict(dict)
    base = {}                 # label -> arm
    for row in rows:
        key = label(row)
        base[key] = row["arm"]
        (src if row["side"] == "src" else dst)[key][row["rep"]] = row["d"]

    arms = sorted(set(src) | set(dst))
    print(f"{'arm':12} {'median':>10}  samples (MiB/s)")
    rates = {}
    for arm in arms:
        xs = [src[arm][r]["rateMiBs"] for r in sorted(src[arm])]
        rates[arm] = {r: src[arm][r]["rateMiBs"] for r in src[arm]}
        print(f"{arm:12} {fmt(med(xs)):>10}  [{' '.join(f'{x:.2f}' for x in xs)}]")

    # One ratio per carrier count: the relay arm measured at the SAME carrier
    # count is the control, because it shared that run's server and that run's
    # minutes of the line.
    for key in arms:
        if base.get(key) != "direct":
            continue
        peer = key.replace("direct", "relay", 1)
        if peer not in rates:
            continue
        shared = sorted(set(rates[key]) & set(rates[peer]))
        ratios = [rates[key][r] / rates[peer][r] for r in shared if rates[peer][r] > 0]
        name = f"{key}/relay" if "/" in key else "d/relay"
        print(f"{name:12} {fmt(med(ratios), 3):>10}x [{' '.join(f'{x:.3f}' for x in ratios)}]")

    print()
    print(f"{'arm':12} {'ttfb med':>10} {'verify med':>11}  ttfb samples (ms)")
    for arm in arms:
        if not dst[arm]:
            continue
        t = [dst[arm][r]["ttfbMs"] for r in sorted(dst[arm]) if dst[arm][r]["ttfbMs"] is not None]
        v = [dst[arm][r]["verifyMs"] for r in sorted(dst[arm])]
        print(f"{arm:12} {fmt(med(t), 0):>10} {fmt(med(v), 0):>11}  [{' '.join(str(x) for x in t)}]")

    print()
    print("per-attempt trace (source side: it is the side that waits on the queue)")
    for arm in arms:
        for r in sorted(src[arm]):
            tr = src[arm][r].get("trace")
            if not tr:
                print(f"  {arm:11} rep={r} trace=none")
                continue
            stats = (tr.get("stats") or [{}])[-1]
            pair = stats.get("pair", {})
            drain = tr.get("drain", {})
            print(
                f"  {arm:11} rep={r} pair={pair.get('localType','?')}/{pair.get('remoteType','?')}"
                f" rtt={pair.get('rttMs','?')}ms out_bitrate={pair.get('outBitrate','?')}"
                f" discarded={pair.get('discardedOnSend','?')}"
                f" waits={drain.get('waits','?')} longest={drain.get('longestMs','?')}ms"
                f" total={drain.get('waitedMs','?')}ms peak={drain.get('peakQueued','?')}"
                f" reason={tr.get('reason') or 'none'}"
            )

    print()
    print("path committed, per transfer (an arm whose label and transport disagree is a defect)")
    for arm in arms:
        for r in sorted(dst[arm]):
            d = dst[arm][r]
            want = base.get(arm, arm)
            bad = "" if all(p == want for p in d.get("commits", [])) else "  <-- MISMATCH"
            print(f"  {arm:11} rep={r} commits={d.get('commits')}{bad}")

    errs = [(row["side"], row["arm"], row["rep"], e)
            for row in rows for e in (row["d"].get("errors") or [])]
    if errs:
        print()
        print("page errors (any line here is a finding, not noise)")
        for side, arm, rep, e in errs:
            print(f"  {side} {arm} rep={rep}: {e}")


if __name__ == "__main__":
    main(sys.argv[1])
