#!/usr/bin/env bash
# X2: the same bytes in more files.
#
# The loopback sweep found a 38x throughput collapse from 1 to 20 000 files at
# constant bytes, which turned out to be the receiver's own bookkeeping (a
# full resume-state rewrite every 8 chunks, a linear scan per chunk, a serial
# staging pass) and not the network. This is the same experiment on a real
# link and real storage, where the per-file floor is a round trip and an
# fdatasync on EBS rather than a memcpy: the point is to find where bore stops
# being the limit and the medium starts.
#
# Constant BYTES, varying FILE COUNT, is the only honest shape for this: a
# sweep that also changes the total moves two things at once.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/xferlib.sh"

TOPO="${TOPO:-ws-vm}"
MB="${MB:-512}"
FILES="${FILES:-1 100 1000 5000 20000}"
PAR="${PAR:-8}"
ARM="${ARM:-relay}"
FSYNC="${FSYNC:-on}"           # on | off  -> listener --no-fsync

case "$TOPO" in
    ws-vm) SRC_HOST=ws; LIS_HOST=vm ;;
    vm-ws) SRC_HOST=vm; LIS_HOST=ws ;;
    vm-vm) SRC_HOST=vm; LIS_HOST=vm ;;
    *) echo "bad TOPO '$TOPO'" >&2; exit 2 ;;
esac
case "$ARM" in relay) FLAGS="--relay-only" ;; direct) FLAGS="" ;; *) exit 2 ;; esac
LIS_FLAGS="$FLAGS"
[ "$FSYNC" = off ] && LIS_FLAGS="$FLAGS --no-fsync"

say "transfer shape: ${MB} MiB in N files, topology $TOPO, arm $ARM, fsync $FSYNC, --parallel $PAR"
printf '  %-8s %9s %11s %13s  %s\n' files wall_s MB/s per_file_ms path
for n in $FILES; do
    if [ "$SRC_HOST" = ws ]; then SRC="$(ws_fixture "$MB" "$n")"; else SRC="$(vm_fixture "$MB" "$n")"; fi
    [ -n "$SRC" ] || { echo "  $n: fixture failed"; continue; }
    id="$(xfer_id)"
    LASTPID=""
    if [ "$LIS_HOST" = ws ]; then
        xfer_listener_ws "$id" "$WS_WORK/dst-$id" $LIS_FLAGS; LIS_PID="$LASTPID"
    else
        xfer_listener_vm "$id" "$VM_WORK/dst-$id" $LIS_FLAGS; LIS_PID=""
    fi
    sleep 4
    if [ "$SRC_HOST" = ws ]; then xfer_sender_ws "$id" "$SRC" --parallel "$PAR" $FLAGS
    else xfer_sender_vm "$id" "$SRC" --parallel "$PAR" $FLAGS; fi
    if [ "$XFER_RC" -eq 0 ]; then
        printf '  %-8s %9s %11s %13s  %s\n' "$n" "$XFER_WALL" "$(xfer_mbs "$MB" "$XFER_WALL")" \
            "$(LC_ALL=C awk -v w="$XFER_WALL" -v n="$n" 'BEGIN{printf "%.3f", w*1000/n}')" \
            "$(xfer_path "$id" "$SRC_HOST")"
    else
        printf '  %-8s %9s %11s %13s  %s\n' "$n" FAIL - - "rc=$XFER_RC"
    fi
    xfer_down "$id" ${LIS_PID:-}
    if [ "$LIS_HOST" = ws ]; then rm -rf "$WS_WORK/dst-$id"; else vm "rm -rf '$VM_WORK/dst-$id'"; fi
    cool "${GAP:-20}"
done
echo
echo DONE
