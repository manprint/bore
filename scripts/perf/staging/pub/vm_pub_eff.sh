#!/usr/bin/env bash
# P9: CPU cost per GiB moved, per transport — the number that predicts how many
# cores a target link rate needs, and the only honest way to answer "is the
# server application-limited?".
#
# MB/s cannot answer it on a burstable instance: the allowance bucket caps the
# rate long before the CPU does, so a rate ceiling proves nothing. CPU seconds
# per GiB is invariant to the cap.
#
# Prints an epoch WINDOW per case so the host-side /proc/stat sampler can be
# matched to it. The container's own CPU% misses the softirq the host kernel
# spends on its behalf, and the vhost campaign measured that at 37-40 % of the
# bill — reading the container alone understates the cost by a third.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

SECS="${SECS:-20}"
ROUNDS="${ROUNDS:-3}"
CONNS="${CONNS:-4}"

start_origins || exit 1

case_run() { # <tag> <flags...>
    local tag="$1"; shift
    up_native "$RP" 0 "$@" || { echo "CASE $tag REGISTRATION FAILED"; return 1; }
    local p="$LASTPORT" pid="$LASTPID"
    python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
    sleep 4                      # let the sampler see an idle baseline first
    local t0 out t1
    t0=$(date +%s)
    # Ask for far more than can be moved in the window and cut it off by
    # timeout: the window is then exactly SECS for every case, and the byte
    # count is whatever the path managed.
    out=$(timeout $((SECS + 10)) python3 "$RAWCLI" get "$GW" "$p" $((8 * 1073741824)) "$CONNS" "$SECS" 2>/dev/null)
    t1=$(date +%s)
    local by; by=$(printf '%s' "$out" | grep -oE 'bytes=[0-9]+' | cut -d= -f2)
    local path; path=$(tfld "$p" current_path)
    LC_ALL=C awk -v t="$tag" -v p="${path:-?}" -v b="${by:-0}" -v s="$t0" -v e="$t1" 'BEGIN{
        d=e-s; if(d<=0) d=1
        printf "CASE %s path=%s window=%d-%d dur=%ds bytes=%d rate=%.2f MB/s gib=%.3f\n",
               t, p, s, e, d, b, b/1048576/d, b/1073741824}'
    down "$pid" "$p"
    sleep 8                      # idle gap so consecutive windows are separable
}

echo "START $(date +%s)"
for r in $(seq "$ROUNDS"); do
    case_run "relay-r$r"    --carriers 1
    case_run "quic-r$r"     --carriers 1 --udp
    case_run "relay8-r$r"   --carriers 8
done
echo "END $(date +%s)"
echo DONE
