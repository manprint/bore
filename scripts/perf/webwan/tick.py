#!/usr/bin/env python3
"""One live line per measurement, read off the last two NDJSON samples.

Its own file rather than an inline `python3 -c`: the inline form had to
escape quotes through two shells and did not survive the trip.
"""
import json
import sys

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        row = json.loads(line)
    except json.JSONDecodeError:
        continue
    d = row["d"]
    if row["side"] == "src":
        print(f"{d['rateMiBs']:7.2f} MiB/s", end="")
    else:
        print(f"  ttfb={d['ttfbMs']}ms verify={d['verifyMs']}ms path={d['path']}", end="")
print()
