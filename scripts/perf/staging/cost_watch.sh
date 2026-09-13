#!/usr/bin/env bash
# Per-stage AWS byte cost, measured at stage boundaries.
#
# WHY THIS EXISTS
# ---------------
# "Which stage costs the money" was being answered by reading transfer sizes out
# of the scripts and multiplying. That is a guess dressed as arithmetic: it
# misses the baselines each driver takes, the retries, a stage that fell back to
# relay and moved twice what it planned, and the whole VM<->server leg. The
# campaign's own rule is that a number nobody measured is not a number, and that
# applies to the campaign's bill as much as to its throughput.
#
# HOW IT MEASURES WITHOUT DISTURBING ANYTHING
# --------------------------------------------
# It polls only LOCAL marker files (`out/eth/_done.<stage>`) and reaches for the
# network exactly once per stage transition -- which lands in the driver's gap
# BETWEEN stages, never inside a measurement. One ssh per host reads two
# counters out of /sys; a few KB against arms that move gigabytes.
#
# DIRECTION IS THE WHOLE POINT
# ----------------------------
# AWS bills EGRESS (AWS -> here) and charges nothing for INGRESS. So an upload
# arm is free and a download arm is not, and the two must never be summed into
# one "traffic" figure -- doing that is what makes a harness cut the free half.
#
# WHY THE HOSTS ARE KEPT APART
# ----------------------------
# The workstation is one hop from the staging server, but the BYTES usually come
# from the test VM, which reaches the server by its PUBLIC address although both
# sit in the same VPC. So a download is paid for twice: once as VM egress and
# once as server egress. Summing the two hides exactly the term an operator
# could remove, so the table keeps them apart and derives the hairpin as
# `(vm_tx + srv_tx) - ws_rx`.
#
#   cost_watch.sh                 follow the live driver until it stops
#   cost_watch.sh report          print the table collected so far
#
# Output: out/eth/_cost_stage.tsv
#   stage, seconds, vm_tx, srv_tx, ws_rx, ingress -- GiB
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
OUT="${BORE_PERF_OUT:-$PWD/out/eth}"
TSV="$OUT/_cost_stage.tsv"
POLL="${POLL:-5}"
export LC_ALL=C

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

counters() { # -> "vmtx vmrx srvtx srvrx wsrx"
    local R='n=$(ip route show default | awk "/^default/{print \$5; exit}");
             printf "%s %s\n" "$(cat /sys/class/net/$n/statistics/tx_bytes)" \
                              "$(cat /sys/class/net/$n/statistics/rx_bytes)"'
    local vm srv nic
    vm=$(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
            -i "${BORE_SSH_KEY:-}" "$BORE_VM_USER@$BORE_VM" "$R" 2>/dev/null | tr -d '\r')
    srv=$(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
            "$BORE_SRV_USER@$BORE_SRV" "$R" 2>/dev/null | tr -d '\r')
    # A missing host reading is NOT a zero -- it is a refusal to report, and the
    # caller drops the whole interval rather than publish half of it.
    [ -n "$vm" ] && [ -n "$srv" ] || return 1
    nic=$(ip route show default | awk '/^default/{print $5; exit}')
    printf '%s %s %s\n' "$vm" "$srv" "$(cat /sys/class/net/"$nic"/statistics/rx_bytes)"
}

report() {
    [ -s "$TSV" ] || { echo "no measurements yet ($TSV)"; return 0; }
    echo "### AWS bytes per stage -- BILLED is egress (AWS -> here); ingress is free"
    printf '  %-26s %6s %8s %8s %8s %9s %8s\n' \
           stage secs 'vm tx' 'srv tx' 'ws rx' 'hairpin' 'free in'
    LC_ALL=C awk -F'\t' '
        { o[++n]=$1; s[$1]=$2; v[$1]=$3; r[$1]=$4; w[$1]=$5; f[$1]=$6
          ts+=$2; tv+=$3; tr+=$4; tw+=$5; tf+=$6 }
        END{
            for(i=1;i<=n;i++){ k=o[i]
                printf "  %-26s %6d %8.2f %8.2f %8.2f %9.2f %8.2f\n",
                       k, s[k], v[k], r[k], w[k], v[k]+r[k]-w[k], f[k] }
            printf "  %-26s %6d %8.2f %8.2f %8.2f %9.2f %8.2f\n",
                   "TOTAL", ts, tv, tr, tw, tv+tr-tw, tf }' "$TSV"
    echo
    echo '  Reading it: rank by the BILLED total (vm tx + srv tx), not by seconds --'
    echo '  a long stage that spent its time in cooldown is free, a short one pulling'
    echo '  at line rate is not. The hairpin column is the part that never reached'
    echo '  this end: VM<->server traffic, billed because the harness addresses both'
    echo '  hosts by their PUBLIC IPs while they share a VPC. It is the one term that'
    echo '  can be removed without shortening a single measurement -- and the one term'
    echo '  that changes the path under test, so it is an operator decision taken'
    echo '  BETWEEN campaigns, never in the middle of one.'
}

case "${1:-}" in report) report; exit 0 ;; esac

echo "### cost_watch -- polling $OUT for stage markers every ${POLL}s"
declare -A SEEN
for m in "$OUT"/_done.*; do [ -e "$m" ] && SEEN["$(basename "$m")"]=1; done
echo "  ${#SEEN[@]} stage(s) already done before this watcher started -- not attributed"

prev=$(counters) || { echo "cannot read the counters -- refusing to start"; exit 2; }
prev_t=$(date +%s)
echo "  baseline taken; following the driver"

while pgrep -f 'staging/rerun_[a-z0-9_]*\.sh' >/dev/null 2>&1; do
    sleep "$POLL"
    for m in "$OUT"/_done.*; do
        [ -e "$m" ] || continue
        b=$(basename "$m"); [ -n "${SEEN[$b]:-}" ] && continue
        SEEN["$b"]=1
        stage="${b#_done.}"
        now=$(counters) || { echo "  $stage: counters unreadable -- interval dropped"; continue; }
        now_t=$(date +%s)
        read -r a1 a2 a3 a4 a5 <<<"$prev"
        read -r b1 b2 b3 b4 b5 <<<"$now"
        LC_ALL=C awk -v s="$stage" -v d="$((now_t-prev_t))" \
            -v vt=$((b1-a1)) -v vr=$((b2-a2)) -v st=$((b3-a3)) -v sr=$((b4-a4)) \
            -v wr=$((b5-a5)) \
            'BEGIN{g=1073741824; printf "%s\t%d\t%.3f\t%.3f\t%.3f\t%.3f\n",
                   s, d, vt/g, st/g, wr/g, (vr+sr)/g}' >> "$TSV"
        tail -1 "$TSV" | LC_ALL=C awk -F'\t' \
            '{printf "  %-26s %5ds  billed %.2f GiB (hairpin %.2f)  free in %.2f GiB\n",
              $1,$2,$3+$4,$3+$4-$5,$6}'
        prev="$now"; prev_t="$now_t"
    done
done

echo "  driver finished"
report
