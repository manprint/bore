#!/usr/bin/env python3
"""Create <files> incompressible files totalling <MiB> under <dir>.

Incompressible on purpose: nothing on bore's path compresses, but the
filesystem underneath can, and a sparse or compressible fixture makes the
sender's read cheaper than the transfer it is supposed to measure.
"""
import os
import sys

d, n, mb = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
total = mb * 1024 * 1024
each = max(1, total // n)
block = os.urandom(1 << 20)
os.makedirs(d, exist_ok=True)
for i in range(n):
    left = each
    with open(os.path.join(d, "f%07d.bin" % i), "wb") as fh:
        while left > 0:
            k = min(len(block), left)
            fh.write(block[:k])
            left -= k
print("  fixture: %d files, %d MiB total, in %s" % (n, total // (1 << 20), d))
