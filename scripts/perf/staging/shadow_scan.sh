#!/usr/bin/env bash
# Find a stage that turns one of the LIBRARY's scalars into its own array.
#
# THE BUG THIS EXISTS FOR, MEASURED AND NOT IMAGINED
# ---------------------------------------------------
# `lib.sh` publishes `S="$BORE_SRV"` -- the staging server's address. Eight
# stages then declared their sample table as `declare -A S`.
#
# Bash does NOT clear a scalar when it becomes an array. It keeps the old value
# as element **[0]**:
#
#     $ S="1.2.3.4"; declare -A S; S[a]=9; for k in "${!S[@]}"; do ...
#       key=[0] val=[1.2.3.4]
#       key=[a] val=[9]
#
# So every one of those stages printed the server's real IP address into its own
# raw-samples block, under the key `0`, on every run. Found in
# `out/eth/pub_ws_conns_r2.out`, sitting between the quic and relay samples.
#
# It never reached a median -- `med()` refuses a non-numeric sample, and that
# guard earned its keep here -- but it reached an EVIDENCE FILE, which is
# exactly how coordinates escape this project: through prose and output, never
# through code. `secret_scan.sh` did not catch it either, because `out/` is
# gitignored and that scanner looks at what git would carry.
#
# The remedy is a NAME, not a discipline: a stage's own tables are `SAMP`, `R`,
# `D`, `U` -- never a single letter the library already owns.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
ROOT="$PWD"

python3 - "$ROOT" <<'PY'
import os, re, sys

root = sys.argv[1]
base = os.path.join(root, "scripts", "perf")

files = []
for dirpath, _, fnames in os.walk(base):
    for fn in sorted(fnames):
        if fn.endswith(".sh"):
            files.append(os.path.join(dirpath, fn))
files.sort()

libs = [f for f in files if os.path.basename(f) == "lib.sh" or f.endswith("lib.sh")]

# A SCALAR assignment in a library: `NAME=value` where value is not `(`.
SCALAR = re.compile(r'(?:^|[;&|]\s*)\s*(?:export\s+|readonly\s+)?([A-Z][A-Z0-9_]*)=(?!\()', re.M)
scalars = {}
for lib in libs:
    for m in SCALAR.finditer(open(lib, encoding="utf-8", errors="replace").read()):
        scalars.setdefault(m.group(1), os.path.relpath(lib, root))

# An ARRAY declaration in a stage.
ARRAY = re.compile(r'(?:^|;)\s*(?:declare|typeset|local)\s+-A?a?A?\s+([A-Z][A-Z0-9_]*)|(?:^|;)\s*([A-Z][A-Z0-9_]*)=\(', re.M)
SOURCES_LIB = re.compile(r'^\s*\.\s+.*lib\.sh', re.M)

bad = 0
for f in files:
    if f in libs:
        continue
    text = open(f, encoding="utf-8", errors="replace").read()
    if not SOURCES_LIB.search(text):
        continue
    hits = []
    for m in ARRAY.finditer(text):
        name = m.group(1) or m.group(2)
        if name in scalars:
            hits.append((text.count("\n", 0, m.start()) + 1, name, scalars[name]))
    if hits:
        bad += 1
        print("SHADOW %s" % os.path.relpath(f, root))
        for ln, name, lib in sorted(hits):
            print("        line %-5d $%s is a SCALAR in %s -- its value survives as "
                  "element [0] and is printed with the samples" % (ln, name, lib))

print()
if bad:
    print("FAIL -- %d file(s) shadow a library scalar with an array" % bad)
    sys.exit(1)
print("PASS -- %d file(s), no stage shadows a library scalar" % len(files))
PY
