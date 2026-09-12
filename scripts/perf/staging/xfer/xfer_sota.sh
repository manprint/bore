#!/usr/bin/env bash
# X3: how `bore transfer` compares with the tools people already use.
#
# Read the comparison honestly. `scp`, `rsync` and `tar | ssh` open a DIRECT TCP
# connection to a host that has a routable address and a running SSH daemon.
# bore's reason to exist is the case where neither is true — a NATed receiver
# with no port forwarding and no public endpoint — which none of the three can
# do at all. So this is not "who is faster at the same job": it is what bore's
# protocol costs relative to the best case, over the same link, with the same
# bytes, in the same minutes.
#
# Both bore arms are measured, because they are different products:
#   * relay  — sender -> server -> listener. Two WAN legs and a third host's
#              bandwidth. Always available; it is the fallback.
#   * direct — hole-punched QUIC, one leg, server off the data path (S-1).
#              This is the arm that is comparable with the SSH tools.
#
# Two shapes, because they stress different things:
#   * one large file  -> transport: window, congestion control, encryption cost.
#   * many small ones -> per-file cost on BOTH sides, which is what the X-1..X-6
#                        defects of this campaign were about. On a link that is
#                        the bottleneck the large-file arm converges for every
#                        tool (the finding is then "bore reaches the ceiling
#                        too"); the many-file arm keeps separating them.
#
# ONE run moves the bytes exactly once per tool, and `cool` sits between tools:
# the staging server is a t4g.micro whose network allowance is a token bucket
# (vhost campaign §2.17.3), so back-to-back GiB-scale runs measure the bucket.
# Only the relay arm crosses the server, but the gap is applied uniformly so
# that no tool is measured in another tool's shadow.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/xferlib.sh"

MB="${MB:-1024}"
FILES="${FILES:-1}"
PAR="${PAR:-8}"
DIR="${DIR:-up}"                 # up = ws -> vm, down = vm -> ws
GAP="${GAP:-40}"
TOOLS="${TOOLS:-link bore-direct bore-relay scp rsync tar-ssh}"

case "$DIR" in up|down) ;; *) echo "bad DIR '$DIR' (up|down)" >&2; exit 2 ;; esac

# Keep the transcript readable: the host-key notice is not a result.
SSH_OPTS+=(-o LogLevel=ERROR)
SSH="$SSH -o LogLevel=ERROR"

# --- the fixture lives on whichever host is sending ---------------------------
if [ "$DIR" = up ]; then
    SRC="$(ws_fixture "$MB" "$FILES")" || exit 1
else
    SRC="$(vm_fixture "$MB" "$FILES")" || exit 1
fi
[ -n "$SRC" ] || { echo "fixture failed" >&2; exit 1; }
BASE="$(basename "$SRC")"

WS_DST="$WS_WORK/sota"
VM_DST="$VM_WORK/sota"

reset_dst() {
    if [ "$DIR" = up ]; then vm "rm -rf '$VM_DST'; mkdir -p '$VM_DST'" >/dev/null 2>&1
    else rm -rf "$WS_DST"; mkdir -p "$WS_DST"; fi
}
# What actually landed, in bytes — a tool that failed halfway must not be
# reported as fast. Read from the RECEIVING host, never inferred from rc alone.
landed() {
    if [ "$DIR" = up ]; then vm "du -sb '$VM_DST' 2>/dev/null | cut -f1"
    else du -sb "$WS_DST" 2>/dev/null | cut -f1; fi
}

EXPECT=$(( MB * 1048576 ))
row() { # row <tool> <wall_s> <rc> <note>
    local tool="$1" wall="$2" rc="$3" note="$4"
    local ok="ok"
    [ "$rc" -ne 0 ] && ok="rc=$rc"
    # `link` deliberately writes to /dev/null — it is the ceiling, not a copy.
    if [ "$tool" != link ]; then
    local got; got="$(landed)"; got="${got:-0}"
    # 2% slack: du counts directory blocks, and the tools differ on whether the
    # top-level directory is recreated at the destination.
    if [ "$got" -lt $(( EXPECT - EXPECT / 50 )) ]; then ok="SHORT ${got}B"; fi
    fi
    if [ "$ok" = ok ]; then
        printf '  %-12s %8s %10s   %s\n' "$tool" "$wall" "$(xfer_mbs "$MB" "$wall")" "$note"
    else
        printf '  %-12s %8s %10s   %s [%s]\n' "$tool" "$wall" FAIL "$note" "$ok"
    fi
}

# `timed` runs a command that moves the bytes and nothing else. Session setup
# that is NOT part of the transfer (starting a listener, waiting for it to
# register) stays outside; setup that IS part of it (the SSH handshake, the
# hole punch) stays inside, because a user waits for it.
timed() { # timed <tool> <note> <cmd...>
    local tool="$1" note="$2"; shift 2
    reset_dst
    local t0 t1 rc
    t0=$(date +%s.%N); "$@" >/dev/null 2>&1; rc=$?; t1=$(date +%s.%N)
    row "$tool" "$(LC_ALL=C awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')" "$rc" "$note"
    cool "$GAP"
}

# --- the link ceiling --------------------------------------------------------
# Not a file-transfer tool: one SSH channel carrying the bytes to /dev/null,
# with no filesystem on the receiving side and no per-file protocol at all.
# Every row below is bounded by this one; without it a table of near-identical
# numbers cannot be told apart from a table of tools that are all equally good.
link_ceiling() {
    if [ "$DIR" = up ]; then
        tar -C "$(dirname "$SRC")" -cf - "$BASE" | $SSH "$VMU@$V" 'cat > /dev/null'
    else
        $SSH "$VMU@$V" "tar -C '$(dirname "$SRC")' -cf - '$BASE'" > /dev/null
    fi
}

# --- bore --------------------------------------------------------------------
# The listener is a persistent service: a user starts it once and it waits. Its
# startup is not part of a transfer and is not timed. The sender is the whole
# of the measured work, hole punch included.
bore_run() { # bore_run <extra flags...>
    local id; id="$(xfer_id)"
    if [ "$DIR" = up ]; then
        xfer_listener_vm "$id" "$VM_DST" "$@"
    else
        xfer_listener_ws "$id" "$WS_DST" "$@"; LIS_PID="$LASTPID"
    fi
    sleep 5
    local t0 t1
    t0=$(date +%s.%N)
    if [ "$DIR" = up ]; then xfer_sender_ws "$id" "$SRC" --parallel "$PAR" "$@"
    else xfer_sender_vm "$id" "$SRC" --parallel "$PAR" "$@"; fi
    t1=$(date +%s.%N)
    BORE_WALL=$(LC_ALL=C awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
    BORE_RC=$XFER_RC
    BORE_PATH="$(xfer_path "$id" "$([ "$DIR" = up ] && echo ws || echo vm)")"
    xfer_down "$id" ${LIS_PID:-}; LIS_PID=""
}
bore_timed() { # bore_timed <tool> <note> <flags...>
    local tool="$1" note="$2"; shift 2
    reset_dst
    bore_run "$@"
    row "$tool" "$BORE_WALL" "$BORE_RC" "$note, path=$BORE_PATH"
    cool "$GAP"
}

say "state of the art: ${MB} MiB in $FILES file(s), dir=$DIR, --parallel $PAR"
printf '  %-12s %8s %10s   %s\n' tool wall_s MB/s note
for tool in $TOOLS; do
    case "$tool" in
        link)        timed link "ssh -> /dev/null, no filesystem, no protocol" link_ceiling ;;
        bore-direct) bore_timed bore-direct "hole-punched QUIC, server off the path" ;;
        bore-relay)  bore_timed bore-relay  "through the server, two WAN legs" --relay-only ;;
        scp)
            if [ "$DIR" = up ]; then timed scp "direct TCP + SSH" scp -q "${SSH_OPTS[@]}" -r "$SRC" "$VMU@$V:$VM_DST/"
            else timed scp "direct TCP + SSH" scp -q "${SSH_OPTS[@]}" -r "$VMU@$V:$SRC" "$WS_DST/"; fi ;;
        rsync)
            if [ "$DIR" = up ]; then timed rsync "direct TCP + SSH" rsync -a -e "$SSH" "$SRC" "$VMU@$V:$VM_DST/"
            else timed rsync "direct TCP + SSH" rsync -a -e "$SSH" "$VMU@$V:$SRC" "$WS_DST/"; fi ;;
        tar-ssh)
            if [ "$DIR" = up ]; then
                timed tar-ssh "best case: one stream, no per-file protocol" \
                    bash -c "tar -C '$(dirname "$SRC")' -cf - '$BASE' | $SSH $VMU@$V 'tar -C $VM_DST -xf -'"
            else
                timed tar-ssh "best case: one stream, no per-file protocol" \
                    bash -c "$SSH $VMU@$V \"tar -C '$(dirname "$SRC")' -cf - '$BASE'\" | tar -C '$WS_DST' -xf -"
            fi ;;
        *) echo "unknown tool '$tool'" >&2 ;;
    esac
done
reset_dst
echo
echo DONE
