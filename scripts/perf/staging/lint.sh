#!/usr/bin/env bash
# Static check for every script in the performance harness.
#
# WHY `bash -n` IS NOT ENOUGH, WHICH IS THE WHOLE REASON THIS FILE EXISTS
# -----------------------------------------------------------------------
# An apostrophe inside a single-quoted `awk` program CLOSES the program:
#
#     awk 'BEGIN{ print "it is bore's problem" }'
#                                    ^ the shell quote ends here
#
# The result is still syntactically valid bash, so `bash -n` reports success and
# the script runs -- printing garbage, or silently computing something else.
# This bug shipped into three separate new stages in one evening before it was
# caught. `shellcheck` catches it as SC1011/SC1012.
#
# So: run this before calling any harness script finished. It is fast, needs no
# network, no privileges and no deployment.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
ROOT="$PWD"

# WHAT THIS CHECKS, AND WHY IT IS A SHORT LIST
# --------------------------------------------
# A gate nobody can pass is a gate everybody ignores. Run across the 120-odd
# scripts already in this tree, full `shellcheck -S warning` reports hundreds of
# stylistic findings (tilde-in-quotes, declare-and-assign, non-constant source)
# that are deliberate here and have never caused a wrong number.
#
# So the DEFAULT is the set of checks that catch code which does something other
# than what it looks like -- the failures `bash -n` cannot see and a reviewer
# reads straight past:
#
#   SC1011/SC1012  an apostrophe closing a single-quoted awk/sed program
#   SC1010         a keyword (`done`, `fi`) swallowed as an argument
#   SC1072/SC1073/SC1078/SC1079   unterminated quoting, parse confusion
#   SC2216/SC2242  commands that silently do nothing
#
# `FULL=1` runs everything instead, for a file being written now.
CHECKS="${CHECKS:-SC1010,SC1011,SC1012,SC1072,SC1073,SC1078,SC1079,SC2216,SC2242}"
FULL="${FULL:-0}"

have_sc=1
command -v shellcheck >/dev/null 2>&1 || { have_sc=0; echo "WARNING: shellcheck absent -- only bash -n will run, which CANNOT see the"; echo "         quoting bug this file exists for. Install it before trusting a pass."; }

fail=0 n=0
while read -r f; do
    n=$((n + 1))
    if ! bash -n "$f" 2>/dev/null; then
        echo "SYNTAX  $f"; bash -n "$f" 2>&1 | sed 's/^/        /'; fail=1; continue
    fi
    if [ "$have_sc" = 1 ]; then
        if [ "$FULL" = 1 ]; then
            out="$(shellcheck -S warning -e SC2034,SC2086 "$f" 2>&1)" || {
                echo "LINT    ${f#$ROOT/}"; printf '%s\n' "$out" | sed 's/^/        /'; fail=1; }
        else
            out="$(shellcheck -S warning -i "$CHECKS" "$f" 2>&1)"
            if [ -n "$out" ]; then
                echo "LINT    ${f#$ROOT/}"; printf '%s\n' "$out" | sed 's/^/        /'; fail=1
            fi
        fi
    fi
done < <(find "$ROOT/scripts/perf" -name '*.sh' | sort)

# The python instrument too -- same rule, different compiler.
while read -r f; do
    n=$((n + 1))
    python3 -c "import ast,sys; ast.parse(open(sys.argv[1]).read())" "$f" 2>/dev/null \
        || { echo "SYNTAX  $f"; fail=1; }
done < <(find "$ROOT/scripts/perf" -name '*.py' | sort)

# The third compiler: a variable a stage READS that nothing ASSIGNS. Neither
# `bash -n` nor shellcheck can see it here -- shellcheck's SC2154 deliberately
# ignores ALL-CAPS names, and every name in this harness is upper case
# (measured: `echo "$UNDEF"` produces no finding, `echo "$undef"` produces one).
# It cost a whole stage: `ws_tunnel` died at `line 19: B: unbound variable`,
# zero seconds, and the campaign carried the hole for hours.
if ! "$ROOT/scripts/perf/staging/unbound_scan.sh"; then
    fail=1
fi

# The fourth: a stage array that shadows one of the library's SCALARS. Bash
# keeps the scalar's value as element [0] when it becomes an array, so the
# staging server's address was printed into eight stages' raw-sample blocks.
if ! "$ROOT/scripts/perf/staging/shadow_scan.sh"; then
    fail=1
fi

# The fifth: a TOP-LEVEL call to a function defined LATER in the same file.
# bash resolves a call when the line RUNS, so this is `command not found`,
# exit 127, on a line nobody guards -- a silent no-op, not a failure. Measured
# in this harness's own build gate, where the function that re-validates the
# product gates after a rebuild was called 27 lines before it was defined and
# would have done nothing, quietly.
if ! "$ROOT/scripts/perf/staging/order_scan.sh"; then
    fail=1
fi

# The sixth, and it is five lines because the rule is five words: a numeric sort
# must pin the locale. V-11 cost a median once (under it_IT, `sort -n` orders
# {397.46, 264.01, 408} as {408, 264.01, 397.46}) and `lib.sh`'s `med()` was
# fixed then -- but four stages had their OWN `sort -n` outside it, each one
# publishing a median or a min/max. The failure is invisible in a file that
# prints only summary statistics, which is exactly how it was found the first
# time.
echo
echo "== numeric sorts pin the locale (V-11) =="
bad_sorts=$(grep -rn 'sort -n' "$ROOT/scripts/perf" --include='*.sh' 2>/dev/null \
    | grep -v 'LC_ALL=C sort' | grep -vE ':[0-9]+:[[:space:]]*#' \
    | grep -v '/lint\.sh:' || true)
if [ -n "$bad_sorts" ]; then
    printf '%s\n' "$bad_sorts" | sed 's/^/  /'
    echo "FAIL -- a numeric sort without LC_ALL=C"
    fail=1
else
    echo "PASS -- every numeric sort pins LC_ALL=C"
fi

echo
if [ "$fail" = 0 ]; then
    echo "PASS -- $n file(s) clean"
else
    echo "FAIL -- see above ($n file(s) checked)"
fi
exit $fail
