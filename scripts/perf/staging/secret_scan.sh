#!/usr/bin/env bash
# Refuse to let a campaign coordinate reach the repository.
#
# WHY THE PATTERNS ARE NOT IN THIS FILE
# -------------------------------------
# A scanner that hardcodes a staging address has, by writing it down, published
# it -- and this file red-checked that on its first run, by flagging a real
# address sitting in this very comment. The
# patterns are therefore DERIVED at runtime from `~/.config/bore-perf/env.sh`
# (mode 600, outside the repository), which is the same file every stage reads
# for its coordinates. Nothing secret is written here, and the scanner stays
# correct when the coordinates change.
#
# WHY IT RUNS BEFORE THE COMMIT, AND OFTEN
# ----------------------------------------
# It was run once, as a pre-commit step, and found three live hits in prose
# written the same session -- an evidence section that quoted a reachability
# test verbatim, IP addresses and all. Prose is exactly where this leaks: code
# reads coordinates from the environment by construction, while a document
# written to explain a measurement quotes what the measurement printed. So run
# it after writing documentation, not only before pushing.
#
#   secret_scan.sh           scan tracked modifications + untracked files
#   secret_scan.sh --staged  scan only what is staged (the pre-commit form)
#   secret_scan.sh --out [d] scan the RESULT files .out/.tsv/.md/.txt (gitignored,
#                            so invisible to the other modes; bore's own *.log are
#                            skipped -- they carry addresses by design)
#                            the other modes) -- run before quoting one in a doc
#
# Exit 0 = clean, 1 = hits found (printed with file and line), 2 = cannot run.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
export LC_ALL=C

ENVF="$HOME/.config/bore-perf/env.sh"
[ -r "$ENVF" ] || { echo "no $ENVF -- cannot derive the patterns" >&2; exit 2; }
# shellcheck disable=SC1090
. "$ENVF"

# THE ONE COORDINATE THIS PROJECT PUBLISHES ON PURPOSE.
#
# `DEFAULT_SERVER` in `src/main.rs` is compiled into EVERY released binary, so
# the host it names is public by construction: anyone running `bore local 8080`
# with no `--to` opens a tunnel through it, and README.md documents it as the
# product's front door. A gate that refused that host would refuse the shipped
# default -- there would be no way to document what the binary already does, and
# the pressure would be to weaken the gate for everything instead.
#
# It is READ FROM THE SOURCE rather than written down here, for two reasons.
# Changing where the binary points stays the one-line edit that
# `default_server_address_is_not_duplicated` promises, and the allowance cannot
# drift from what the binary actually ships: point `DEFAULT_SERVER` somewhere
# else and the old host becomes scannable again in the same commit.
#
# A source file we cannot read, or a const we cannot parse, yields NO allowance.
# The gate fails CLOSED -- the only safe direction for a gate, and the reason
# this is a `sed` over one anchored line rather than a fuzzy search.
#
# SCOPE: this exempts EXACTLY that host, and only where a coordinate IS it. The
# server's IP address, the test VM, the tunnel secret, the admin token and the
# ssh key name are untouched and still refused.
PUBLIC_HOST=$(sed -n 's/^const DEFAULT_SERVER: &str = "\(.*\)";$/\1/p' src/main.rs 2>/dev/null \
    | sed -e 's#^[a-zA-Z][a-zA-Z0-9+.-]*://##' -e 's#[:/].*$##' | head -n 1)

# Anything that identifies the environment or authenticates to it. A value that
# is empty contributes NO pattern -- an empty alternative would match every line.
# A value that is exactly the published host contributes none either, and SAYS SO
# on stderr: a gate that narrows itself quietly is a gate nobody audits.
PATS=()
add_pat() {
    [ -n "${1:-}" ] || return 0
    if [ -n "$PUBLIC_HOST" ] && [ "$1" = "$PUBLIC_HOST" ]; then
        echo "secret_scan: '$PUBLIC_HOST' is src/main.rs's DEFAULT_SERVER -- published by construction, not scanned" >&2
        return 0
    fi
    PATS+=("$(printf '%s' "$1" | sed 's/[][\.*^$(){}?+|/]/\\&/g')")
}
add_pat "${BORE_SRV:-}"
add_pat "${BORE_VM:-}"
add_pat "${BORE_GW:-}"
add_pat "${BORE_SECRET:-}"
add_pat "${ADMIN_TOKEN:-}"
add_pat "$(basename "${BORE_SSH_KEY:-}" 2>/dev/null)"
# The reflexive address of this end, as the STUN servers see it. Not in env.sh,
# so it is looked up -- and a lookup that fails simply contributes no pattern.
add_pat "$(timeout 5 curl -fsS https://api.ipify.org 2>/dev/null)"
# Generic, safe to name in the clear: private VPC range, key material headers.
PATS+=('172\.31\.[0-9]+\.[0-9]+' 'BEGIN (RSA |OPENSSH |EC )?PRIVATE KEY' 'AKIA[0-9A-Z]{16}')

RE=$(IFS='|'; printf '%s' "${PATS[*]}")

hits=0
report() { hits=$((hits + 1)); printf '  %s\n' "$1"; }

# `--out` scans the RESULT files, which the other two modes cannot see: `out/`
# is gitignored, so neither the diff nor `ls-files --others` reaches it. That
# blind spot is not theoretical -- `pub_ws_conns_r2.out` carried the staging
# server's real address in its raw-samples block for a whole run (a library
# scalar surviving as element [0] of a stage's array; see `shadow_scan.sh`) and
# this scanner said CLEAN throughout.
#
# It is NOT part of the commit gate, because those files are never committed and
# a permanent warning about a historical file is a warning nobody reads. It is
# the check to run BEFORE QUOTING A RESULT INTO A DOCUMENT -- which is the one
# moment a coordinate crosses from an ignored file into a tracked one, and the
# way every leak this campaign has had actually happened.
if [ "${1:-}" = "--out" ]; then
    dir="${2:-${BORE_PERF_OUT:-out/eth}}"
    [ -d "$dir" ] || { echo "secret_scan --out: no such directory: $dir" >&2; exit 2; }
    # RESULT files only -- `.out`, `.tsv`, `.md`, `.txt`. The `*.log` files in
    # the same directory are bore's own debug logs and are FULL of addresses by
    # construction (candidate lists, reflexive addresses, punch targets): that
    # is what they are for. Scanning them buries the one line that matters under
    # hundreds that do not, and a report nobody can read is a report nobody
    # reads. A document quotes a stage's `.out`, never a client's log.
    while IFS= read -r line; do report "$line"; done < <(
        find "$dir" -type f \( -name '*.out' -o -name '*.tsv' -o -name '*.md' -o -name '*.txt' \) \
            -exec grep -nHE "$RE" {} + 2>/dev/null )
    if [ "$hits" -eq 0 ]; then
        echo "secret_scan --out: CLEAN ($dir, ${#PATS[@]} patterns)"
        exit 0
    fi
    echo "secret_scan --out: $hits HIT(S) in $dir -- do not quote these files into a document" >&2
    exit 1
fi

if [ "${1:-}" = "--staged" ]; then
    while IFS= read -r line; do report "$line"; done < <(
        git diff --cached 2>/dev/null | grep -nE "^\+.*($RE)" )
else
    while IFS= read -r line; do report "$line"; done < <(
        git diff HEAD 2>/dev/null | grep -nE "^\+.*($RE)" )
    while IFS= read -r f; do
        [ -f "$f" ] || continue
        grep -nHE "$RE" "$f" 2>/dev/null
    done < <(git ls-files --others --exclude-standard) | while IFS= read -r line; do
        printf '  %s\n' "$line"
    done
    # The subshell above cannot raise `hits`, so count separately.
    n=$(git ls-files --others --exclude-standard \
        | while IFS= read -r f; do [ -f "$f" ] && grep -lE "$RE" "$f" 2>/dev/null; done | wc -l)
    hits=$((hits + n))
fi

if [ "$hits" -eq 0 ]; then
    echo "secret_scan: CLEAN (${#PATS[@]} patterns)"
    exit 0
fi
echo "secret_scan: $hits HIT(S) -- do not commit" >&2
exit 1
