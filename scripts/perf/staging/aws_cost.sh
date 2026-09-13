#!/usr/bin/env bash
# What a campaign COSTS, measured instead of estimated.
#
# WHY THIS EXISTS
# ---------------
# Every other quantity in this harness is measured and every conclusion refuses
# to rest on an estimate. The bill was the exception: nobody knew which stages
# moved the bytes, so the only available lever was "run less of everything",
# which is the one lever guaranteed to cost measurement quality.
#
# THE ONE FACT THAT ORDERS EVERYTHING: only ONE DIRECTION is billed.
# Traffic INTO AWS is free. Traffic OUT of AWS to the internet is charged. So an
# upload arm (workstation -> VM) costs nothing and a download arm (VM ->
# workstation) is the whole bill. Cutting bytes symmetrically therefore halves
# the statistical power to save nothing, and the correct lever is DOWNLOAD
# SECONDS AT LINE RATE, spent only where a decision hangs on them.
#
# A second term is invisible from this end: the test VM and the staging server
# are in the SAME VPC (both 172.31/16) but the harness addresses them by their
# PUBLIC IPs, so relay traffic between them leaves the VPC and re-enters instead
# of staying internal. That is a deployment property, not a harness one --
# changing it would change the path these campaigns measure, so this script
# reports the legs separately and leaves the decision to a human.
#
# METHOD
# ------
# The kernel's own interface counters on each AWS host, read as a DELTA around
# whatever ran in between -- the same discipline `asym_qualify.sh` uses for the
# ENA allowance counters, and for the same reason: a cumulative number at the
# end cannot say which stage spent it.
#
#   aws_cost.sh snap <tag>     record a snapshot under out/eth/_cost.<tag>
#   aws_cost.sh diff <a> <b>   print the bytes that moved between two snapshots
#   aws_cost.sh since <tag>    diff <tag> against right now
#
# Counters are cumulative since boot and are read over ssh, which costs a few
# kilobytes -- three orders of magnitude below anything it measures, and the
# drivers already ssh per stage for their baselines.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }
export LC_ALL=C

OUT="${BORE_PERF_OUT:-$PWD/out/eth}"
mkdir -p "$OUT"

S=(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10)
READ='n=$(ip route show default | awk "/^default/{print \$5; exit}");
      printf "%s %s\n" "$(cat /sys/class/net/$n/statistics/tx_bytes)" \
                       "$(cat /sys/class/net/$n/statistics/rx_bytes)"'

host_counters() { # <user@host> [keyfile] -> "tx rx"
    local target="$1" key="${2:-}"
    if [ -n "$key" ]; then
        "${S[@]}" -i "$key" "$target" "$READ" 2>/dev/null
    else
        "${S[@]}" "$target" "$READ" 2>/dev/null
    fi
}

snap() {
    local tag="$1" f="$OUT/_cost.$1"
    {
        printf 'at %s\n' "$(date -Is)"
        printf 'vm %s\n'  "$(host_counters "$BORE_VM_USER@$BORE_VM" "${BORE_SSH_KEY:-}"  | tr -d '\r')"
        printf 'srv %s\n' "$(host_counters "$BORE_SRV_USER@$BORE_SRV"                     | tr -d '\r')"
        # This end, for the cross-check: the workstation's rx must roughly equal
        # the two AWS hosts' billed tx. A large mismatch means bytes went
        # somewhere neither host accounts for, and the reading is wrong.
        local nic; nic=$(ip route show default | awk '/^default/{print $5; exit}')
        printf 'ws %s %s\n' "$(cat /sys/class/net/"$nic"/statistics/tx_bytes)" \
                            "$(cat /sys/class/net/"$nic"/statistics/rx_bytes)"
    } > "$f"
    echo "snapshot -> $f"
    sed 's/^/  /' "$f"
}

# `$3` inside the awk program is awk's THIRD FIELD, not the shell's third
# argument -- the index must travel as an awk VARIABLE. Written the obvious way
# this read field number `$3+1` of the record (a byte counter, ~1.1e12), came
# back empty, and the caller's arithmetic died with
# `syntax error: operand expected`. Loud, at least: an empty operand inside
# `$(( ))` is a hard error, not a zero. Had this helper been used in a context
# that tolerates an empty string, the instrument would have reported 0 GiB
# moved and looked like good news.
field() { awk -v k="$2" -v i="$3" '$1==k{print $(i+1)}' "$1"; }

report() { # <file a> <file b>
    local a="$1" b="$2"
    local vmt=$(( $(field "$b" vm 1) - $(field "$a" vm 1) ))
    local vmr=$(( $(field "$b" vm 2) - $(field "$a" vm 2) ))
    local st=$((  $(field "$b" srv 1) - $(field "$a" srv 1) ))
    local sr=$((  $(field "$b" srv 2) - $(field "$a" srv 2) ))
    local wt=$((  $(field "$b" ws 1) - $(field "$a" ws 1) ))
    local wr=$((  $(field "$b" ws 2) - $(field "$a" ws 2) ))
    echo "  window: $(field "$a" at 1) -> $(field "$b" at 1)"
    printf '  %-22s %12s %12s\n' host 'tx GiB' 'rx GiB'
    LC_ALL=C awk -v a="$vmt" -v b="$vmr" -v c="$st" -v d="$sr" -v e="$wt" -v f="$wr" 'BEGIN{
        g=1073741824
        printf "  %-22s %12.2f %12.2f\n", "test VM",  a/g, b/g
        printf "  %-22s %12.2f %12.2f\n", "staging server", c/g, d/g
        printf "  %-22s %12.2f %12.2f\n", "workstation", e/g, f/g
        printf "\n"
        printf "  BILLED (AWS egress, both hosts): %.2f GiB\n", (a+c)/g
        printf "  FREE   (into AWS):               %.2f GiB\n", (b+d)/g
        printf "  this end received:               %.2f GiB\n", f/g
        printf "\n"
        printf "  Cross-check: the workstation received %.2f GiB against %.2f GiB of AWS tx.\n", f/g, (a+c)/g
        printf "  The excess is the VM<->server leg, which is billed TWICE today because\n"
        printf "  the harness addresses both hosts by their PUBLIC IPs while they sit in\n"
        printf "  the same VPC: %.2f GiB.\n", ((a+c)-f)/g
    }'
}

case "${1:-}" in
    snap)  [ -n "${2:-}" ] || { echo "usage: aws_cost.sh snap <tag>" >&2; exit 2; }; snap "$2" ;;
    diff)  [ -n "${3:-}" ] || { echo "usage: aws_cost.sh diff <a> <b>" >&2; exit 2; }
           report "$OUT/_cost.$2" "$OUT/_cost.$3" ;;
    since) [ -n "${2:-}" ] || { echo "usage: aws_cost.sh since <tag>" >&2; exit 2; }
           snap "__now" >/dev/null; report "$OUT/_cost.$2" "$OUT/_cost.__now" ;;
    *) sed -n '2,40p' "${BASH_SOURCE[0]}"; exit 2 ;;
esac
