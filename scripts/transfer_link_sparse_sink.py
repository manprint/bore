#!/usr/bin/env python3
"""Write stdin to a sparse file without changing its bytes.

Every input byte is hashed before it is written or represented by a hole.  A
zero-only block is advanced with seek; mixed blocks are written normally.  The
utility is a test sink only: it is deliberately not part of the bore binary.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("output")
    parser.add_argument("--block-size", type=int, default=1024 * 1024)
    args = parser.parse_args()
    if args.block_size <= 0:
        parser.error("--block-size must be positive")

    digest = hashlib.sha256()
    total = 0
    zero_blocks = 0
    with open(args.output, "wb") as output:
        while True:
            block = sys.stdin.buffer.read(args.block_size)
            if not block:
                break
            digest.update(block)
            total += len(block)
            if not any(block):
                output.seek(len(block), os.SEEK_CUR)
                zero_blocks += 1
            else:
                output.write(block)
        output.truncate(total)

    json.dump(
        {"bytes": total, "sha256": digest.hexdigest(), "zero_blocks": zero_blocks},
        sys.stdout,
    )
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
