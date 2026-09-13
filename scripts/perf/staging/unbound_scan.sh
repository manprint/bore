#!/usr/bin/env bash
# Find variables a stage READS but nothing ever ASSIGNS -- the defect that kills
# a stage in 0 seconds under `set -u` and leaves the campaign a hole.
#
# WHY THIS EXISTS INSTEAD OF A SHELLCHECK FLAG
# --------------------------------------------
# `ws_tunnel.sh` died at `line 19: B: unbound variable`, zero seconds, rc=1. The
# driver logged FAIL and moved on, and the stage's absence was noticed hours
# later when someone counted markers. The same `$B` had already done it to two
# sibling stages the same evening.
#
# `bash -n` cannot see it: the script is syntactically perfect. And shellcheck's
# SC2154 ("referenced but not assigned") **deliberately ignores ALL-CAPS
# names**, because it assumes they come from the environment -- MEASURED here,
# not read in a manual: a file containing nothing but `echo "$UNDEF_THING"`
# produces no SC2154 at all, while the same file with `$undef_thing` produces
# one. Every variable in this harness is upper case. So the one tool that names
# this check is structurally blind to it here, and the check has to be ours.
#
# WHAT COUNTS AS "ASSIGNED", AND WHY THE LIST IS EXACTLY THIS
# -----------------------------------------------------------
#   * anywhere in the file itself, in any order (bash binds at run time, so a
#     variable assigned at the bottom and read inside a function is fine)
#   * in a sourced library: `lib.sh` and every `*lib.sh` under `scripts/perf`
#   * in `env.sh.example`, which is the TEMPLATE of the coordinates file kept
#     outside the repository. The real one cannot be read by a linter and must
#     not be -- so the template is the contract, and a stage reading a
#     coordinate absent from the template is a bug in one of the two.
#   * the shell's own specials and the ordinary environment
#
# WHAT IS NOT A FINDING, DELIBERATELY
# -----------------------------------
# `${VAR:-default}`, `${VAR-}`, `${VAR:=x}`, `${VAR+x}` and friends are SAFE
# under `set -u` -- they are how this harness declares an optional knob. Only a
# BARE `$VAR` / `${VAR}` can abort a run, so only a bare read is reported.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
ROOT="$PWD"

python3 - "$ROOT" <<'PY'
import os, re, sys

root = sys.argv[1]
base = os.path.join(root, "scripts", "perf")

SPECIAL = set("0123456789") | {
    "#", "@", "*", "?", "$", "!", "-", "_", "IFS", "PATH", "HOME", "USER",
    "PWD", "OLDPWD", "SHELL", "TERM", "LANG", "LC_ALL", "LOGNAME", "HOSTNAME",
    "SECONDS", "RANDOM", "LINENO", "PPID", "BASHPID", "BASH_SOURCE", "FUNCNAME",
    "BASH_REMATCH", "REPLY", "EUID", "UID", "TMPDIR", "EDITOR", "SSH_AUTH_SOCK",
    "COLUMNS", "PIPESTATUS", "BASH_VERSION", "SHLVL", "XDG_RUNTIME_DIR",
}

# An assignment in any of these shapes binds the name.
ASSIGN = [
    # The context class is WIDE on purpose: `t1=$(date +%s) back=""` is one
    # command with two assignment prefixes, and `case` arms assign after `) `.
    # A narrow class missed both. Matching an argument that merely looks like
    # `A=1` over-binds, which costs a detection and never a false alarm.
    re.compile(r'(?:^|[\s;&|({}])\s*(?:export\s+|local\s+|declare\s+(?:-\w+\s+)*|typeset\s+|readonly\s+)?([A-Za-z_][A-Za-z0-9_]*)\+?=', re.M),
    re.compile(r'\bfor\s+([A-Za-z_][A-Za-z0-9_]*)\s+in\b', re.M),
    re.compile(r'\bprintf\s+-v\s+([A-Za-z_][A-Za-z0-9_]*)', re.M),
    re.compile(r'\bgetopts\s+\S+\s+([A-Za-z_][A-Za-z0-9_]*)', re.M),
    re.compile(r'^\s*(?:export|declare|local|readonly)\s+(?:-\w+\s+)*([A-Za-z_][A-Za-z0-9_]*)\s*$', re.M),
]
# A `local`/`declare`/`export`/`readonly` CLAUSE binds every name on the line,
# not just the first: `local l=$1 s=$2 a b t0 t1` declares six. Missing that was
# most of this scanner's second round of false positives. Over-binding here is
# the SAFE direction for a lint -- it can only cost a detection, while
# under-binding costs credibility, and a gate nobody can pass is a gate
# everybody ignores.
# Not `[^\n;&|)]`: `local l="$(date +%s)" p ok=0 i r` would be cut at the `)`
# of the substitution and the four later names lost -- which is how a genuinely
# initialised `ok=0` was reported as unbound.
DECL_LINE = re.compile(r'\b(?:local|declare|typeset|export|readonly)\b([^\n;]*)')
DECL_NAME = re.compile(r'^([A-Za-z_][A-Za-z0-9_]*)(?:=|$)')

# `read -r a b c` and `mapfile -t arr` bind every name after the options.
READ_MULTI = re.compile(r'\b(?:read|mapfile|readarray)\b((?:\s+-\w+)*)((?:\s+[A-Za-z_][A-Za-z0-9_]*)+)')
# A function's parameters arrive positionally, but this harness also uses the
# `local a b c` form with assignment later; that is covered above.

# A BARE read only. `${V:-x}` and every other modifier is safe under set -u.
BARE = re.compile(r'\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)')
MODIFIED = re.compile(r'\$\{[A-Za-z_][A-Za-z0-9_]*[^A-Za-z0-9_}]')


HEREDOC = re.compile(r"<<-?\s*(['\"])([A-Za-z_][A-Za-z0-9_]*)\1")


def blank_quoted_heredocs(text):
    """Blank the body of every `<<'EOF'` heredoc.

    A QUOTED delimiter means the shell performs no expansion at all inside the
    body -- that is precisely why this harness uses it to carry python and awk
    programs, and why `$l` inside one is not a shell read. An unquoted `<<EOF`
    IS expanded and is deliberately left alone.
    """
    out, pos = [], 0
    for m in HEREDOC.finditer(text):
        delim = m.group(2)
        nl = text.find("\n", m.end())
        if nl < 0:
            break
        if nl < pos:
            continue
        body_start = nl + 1
        end = re.search(r"^\s*%s\s*$" % re.escape(delim), text[body_start:], re.M)
        if not end:
            continue
        body_end = body_start + end.start()
        if body_start < pos:
            continue
        out.append(text[pos:body_start])
        out.append(re.sub(r"[^\n]", " ", text[body_start:body_end]))
        pos = body_end
    out.append(text[pos:])
    return "".join(out)


def normalize(text):
    """Blank out comments AND single-quoted strings.

    Single quotes matter more than they look: this harness passes jq and awk
    programs as single-quoted arguments, and those programs have variables of
    their OWN -- `jq --arg l "$1" '...select(.x==$l)'` reads `$l` in jq, not in
    the shell. Counting those as shell reads made the first version of this
    scanner report 128 files out of 132, which is a gate nobody can pass.

    A comment also has to respect quoting: `#` inside a string is not a comment.
    So both are decided by ONE state machine rather than two guesses.
    """
    text = blank_quoted_heredocs(text)
    out = []
    state = "n"          # n=normal  s=in single quotes  d=in double quotes
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        if state == "n":
            if ch == "\\" and i + 1 < n:
                out.append(ch); out.append(text[i + 1]); i += 2; continue
            if ch == "'":
                state = "s"; out.append(" ")
            elif ch == '"':
                state = "d"; out.append(ch)
            elif ch == "#" and (not out or out[-1] in " \t\n"):
                while i < n and text[i] != "\n":
                    i += 1
                continue
            else:
                out.append(ch)
        elif state == "s":
            if ch == "'":
                state = "n"; out.append(" ")
            else:
                out.append("\n" if ch == "\n" else " ")
        else:  # in double quotes
            if ch == "\\" and i + 1 < n:
                out.append(ch); out.append(text[i + 1]); i += 2; continue
            if ch == '"':
                state = "n"
            out.append(ch)
        i += 1
    return "".join(out)


def assigned(text):
    names = set()
    for rx in ASSIGN:
        names.update(rx.findall(text))
    for _opts, rest in READ_MULTI.findall(text):
        names.update(rest.split())
    for clause in DECL_LINE.findall(text):
        for tok in clause.split():
            m = DECL_NAME.match(tok)
            if m:
                names.add(m.group(1))
    return names


def bare_reads(text):
    """Names read WITHOUT a default. Positions covered by ${V<modifier>} skipped."""
    safe_spans = [m.span() for m in MODIFIED.finditer(text)]
    found = {}
    for m in BARE.finditer(text):
        if any(a <= m.start() < b for a, b in safe_spans):
            continue
        name = m.group(1) or m.group(2)
        if name in SPECIAL:
            continue
        found.setdefault(name, text.count("\n", 0, m.start()) + 1)
    return found


files = []
for dirpath, _, fnames in os.walk(base):
    for fn in sorted(fnames):
        if fn.endswith(".sh"):
            files.append(os.path.join(dirpath, fn))
files.sort()

# The shared vocabulary: every library, plus the coordinates TEMPLATE.
shared = set()
for f in files:
    if os.path.basename(f) == "lib.sh" or f.endswith("lib.sh"):
        shared |= assigned(normalize(open(f, encoding="utf-8", errors="replace").read()))
tpl = os.path.join(base, "staging", "env.sh.example")
if os.path.exists(tpl):
    shared |= assigned(normalize(open(tpl, encoding="utf-8", errors="replace").read()))

# SCOPE: the GATE is the campaign harness under `scripts/perf/staging`, which is
# what a driver runs unattended and where an unbound variable becomes a missing
# stage nobody notices. The older one-off tools directly under `scripts/perf`
# are run by hand with their environment exported at the prompt, and their
# contract is in their own header; they are reported as NOTE and do not fail the
# gate -- a mixed gate would be failed permanently and therefore never read.
gate_bad, note_bad = 0, 0
notes = []
for f in files:
    rel = os.path.relpath(f, root)
    text = normalize(open(f, encoding="utf-8", errors="replace").read())
    known = assigned(text) | shared
    miss = {n: ln for n, ln in bare_reads(text).items() if n not in known}
    if not miss:
        continue
    lines = ["%s %s" % ("UNBOUND" if "/staging/" in f else "NOTE   ", rel)]
    for n in sorted(miss):
        lines.append("        line %-5d $%s  -- read bare, assigned nowhere" % (miss[n], n))
    if "/staging/" in f:
        gate_bad += 1
        print("\n".join(lines))
    else:
        note_bad += 1
        notes.append("\n".join(lines))

if notes:
    print()
    print("--- outside the campaign harness (advisory, not a gate) ---")
    print("\n".join(notes))

print()
if gate_bad:
    print("FAIL -- %d staging file(s) read a variable nothing assigns (%d checked)"
          % (gate_bad, len(files)))
    sys.exit(1)
print("PASS -- %d file(s) checked, every bare read in the campaign harness is bound"
      % len(files))
if note_bad:
    print("       (%d file(s) outside it have advisory findings, listed above)" % note_bad)
PY
