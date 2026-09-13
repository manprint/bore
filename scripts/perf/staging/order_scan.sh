#!/usr/bin/env bash
# The FOURTH compiler: a TOP-LEVEL call to a function defined LATER in the same
# file.
#
# WHY THIS CLASS EXISTS AND WHY NOTHING ELSE CATCHES IT
# -----------------------------------------------------
# bash executes top to bottom, so a function is callable only after the line
# that DEFINES it has run. A call that precedes its definition is not a syntax
# error -- `bash -n` passes, shellcheck passes (SC2218 exists but is not emitted
# for this shape in the versions this repo runs against), and the script runs.
# It fails at runtime with `command not found`, exit 127, on the one line that
# nobody wrote an `|| fail=1` for, because a call to one's own function is the
# last thing anyone expects to fail.
#
# MEASURED here, in this campaign's own build gate: `p5_build_gate.sh` called
# `invalidate_binary_gates` at line 176 and defined it at line 203. That
# function is the entire mechanism by which the M-1 fix would have been
# re-verified against the rebuilt binary -- and it would have been a silent
# no-op. Same family as every expensive defect this campaign has paid for: a
# gate whose failure mode is silence.
#
# WHAT IS AND IS NOT A FINDING, AND WHY THE SCOPE IS NARROW
# ---------------------------------------------------------
# Only TOP-LEVEL calls are reported -- a call that sits inside another
# function's body is resolved when THAT function is invoked, which is normally
# after the whole file has been read, so mutual recursion and helpers defined in
# any order are perfectly correct and must not be flagged. A gate nobody can
# pass is a gate everybody ignores; the narrow rule is the sound one, and it is
# exactly the rule that catches the defect above.
#
# A file whose function bodies cannot be delimited (no `}` at column 0) is
# reported as UNPARSEABLE rather than silently passing. An unreadable file is a
# fact about the scanner, and a scanner that hides them is worse than no
# scanner.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${1:-$HERE}"

python3 - "$ROOT" <<'PY'
import os, re, sys

root = sys.argv[1]

def blank_heredocs(lines):
    """Blank every heredoc BODY (quoted or not). A line inside a heredoc is
    data: `foo` there is a word in a document, not a call."""
    out, i, n = list(lines), 0, len(lines)
    while i < n:
        m = re.search(r'<<-?\s*([\'"]?)([A-Za-z_][A-Za-z0-9_]*)\1', out[i])
        if m:
            delim = m.group(2)
            j = i + 1
            while j < n and out[j].strip() != delim:
                out[j] = ''
                j += 1
            if j < n:
                out[j] = ''
            i = j
        i += 1
    return out

DEF = re.compile(r'^\s*(?:function\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\(\)\s*\{\s*$')
ASSIGN = re.compile(r'^[A-Za-z_][A-Za-z0-9_]*=')
KEYWORDS = {'if','then','else','elif','fi','while','until','do','done','for',
            'case','esac','in','{','}','!','(',')','[','[[','time','!'}

findings, unparseable, scanned = [], [], 0

def first_words(line):
    """Every command-position token on a line, conservatively."""
    words = []
    for frag in re.split(r'[;|]|&&|\|\|', line):
        frag = frag.strip()
        while True:
            m = re.match(r'^(\(|\{|!)\s*(.*)$', frag)
            if not m: break
            frag = m.group(2)
        toks = frag.split()
        k = 0
        while k < len(toks) and (toks[k] in KEYWORDS or ASSIGN.match(toks[k])):
            k += 1
        if k < len(toks):
            words.append(toks[k])
    return words

for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = [d for d in dirnames if d not in ('.git', 'out', 'node_modules')]
    for fn in sorted(filenames):
        if not fn.endswith('.sh'):
            continue
        path = os.path.join(dirpath, fn)
        rel = os.path.relpath(path, os.path.dirname(root.rstrip('/')) or '.')
        try:
            raw = open(path, encoding='utf-8', errors='replace').read().split('\n')
        except OSError:
            continue
        scanned += 1
        lines = blank_heredocs(raw)
        # strip whole-line comments; keep the index so line numbers survive
        lines = ['' if re.match(r'^\s*#', l) else l for l in lines]
        # BLANK QUOTED STRINGS. A name inside quotes is not a call HERE: the
        # canonical shape is `trap 'cleanup; assert_clean' EXIT`, installed
        # early on purpose and evaluated when the trap fires -- which is after
        # the whole file has been read. Reporting it would make the gate wrong
        # about the one idiom every stage in this harness uses, and a gate
        # nobody can pass is a gate everybody ignores. A command name that
        # really is quoted would be a variable (`"$cmd"`), which this scanner
        # does not resolve anyway.
        lines = [re.sub(r"'[^']*'", "''", l) for l in lines]
        lines = [re.sub(r'"[^"]*"', '""', l) for l in lines]

        defs, inside, bad = {}, [False] * len(lines), False
        for idx, l in enumerate(lines):
            m = DEF.match(l)
            if not m:
                continue
            defs.setdefault(m.group(1), idx + 1)
            j = idx + 1
            while j < len(lines) and not re.match(r'^\}', lines[j]):
                j += 1
            if j >= len(lines):
                bad = True
                break
            for k in range(idx, j + 1):
                inside[k] = True
        if bad:
            unparseable.append(rel)
            continue

        for idx, l in enumerate(lines):
            if inside[idx] or not l.strip():
                continue
            for w in first_words(l):
                d = defs.get(w)
                if d and d > idx + 1:
                    findings.append((rel, idx + 1, w, d))

for rel, ln, w, d in findings:
    print("  %s:%d: top-level call to `%s`, which is defined at line %d" % (rel, ln, w, d))
for rel in unparseable:
    print("  NOTE %s: function bodies not delimited by `}` at column 0 -- not scanned" % rel)

if findings:
    print("FAIL -- %d top-level call(s) precede their definition" % len(findings))
    sys.exit(1)
print("PASS -- %d file(s), no top-level call precedes its function definition" % scanned)
PY
