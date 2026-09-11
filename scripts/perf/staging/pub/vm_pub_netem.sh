#!/usr/bin/env bash
# P8: the netem matrix. How each transport degrades as the path degrades.
#
# Shaping is applied on the VM's egress toward the server, so it affects the
# UPLOAD direction of the tunnel's data path and the QUIC control traffic. The
# download direction is shaped by the server's own path, which this harness
# cannot touch — stated explicitly because a reader will otherwise assume the
# numbers are symmetric.
#
# Root required (tc). Every rule is removed on EVERY exit path; a leaked qdisc
# would silently poison every later measurement on this VM.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

DEV="${NETEM_DEV:-$(ip route get 1.1.1.1 2>/dev/null | grep -oP 'dev \K\S+' | head -1)}"
MB="${MB:-64}"
CONNS="${CONNS:-4}"
PER=$(( MB * 1048576 / CONNS ))

clear_netem() { sudo -n tc qdisc del dev "$DEV" root 2>/dev/null; }
set_netem()   { clear_netem; [ -z "$1" ] && return 0; sudo -n tc qdisc add dev "$DEV" root netem $1; }
trap 'clear_netem; stop_all 2>/dev/null; cleanup; exit 130' INT TERM
trap 'clear_netem; cleanup' EXIT

sudo -n tc qdisc show dev "$DEV" >/dev/null 2>&1 || { echo "no root tc on $DEV; skipping"; exit 0; }

start_origins || exit 1
say "netem matrix on $DEV, ${MB} MiB over $CONNS conns per cell"

# Register BOTH transports at once so a cell measures them under the SAME
# shaping instance, not under two separately-applied ones.
up_native "$RP" "${PUB_RELAY:-9011}" --carriers 1 || { echo "relay arm failed"; exit 1; }
RPID=$LASTPID; RPORT=$LASTPORT
up_native "$RP" "${PUB_QUIC:-9012}" --carriers 1 --udp || { echo "quic arm failed"; down "$RPID" "$RPORT"; exit 1; }
QPID=$LASTPID; QPORT=$LASTPORT
stop_all() { down "$RPID" "$RPORT"; down "$QPID" "$QPORT"; }
python3 "$RAWCLI" get "$GW" "$RPORT" 1048576 1 >/dev/null 2>&1
python3 "$RAWCLI" get "$GW" "$QPORT" 1048576 1 >/dev/null 2>&1
echo "  relay port=$RPORT  quic port=$QPORT path=$(tfld "$QPORT" current_path)"

printf '\n  %-26s %10s %10s %8s  %s\n' condition relay_MBs quic_MBs ratio "quic path"
for cell in \
    "clean:" \
    "loss 1%:loss 1%" \
    "loss 3%:loss 3%" \
    "loss 10%:loss 10%" \
    "delay 40ms:delay 40ms" \
    "delay 40ms loss 1%:delay 40ms loss 1%" \
    "delay 100ms:delay 100ms" \
    "delay 40ms reorder:delay 40ms reorder 5% 50%" \
    ; do
    name="${cell%%:*}"; spec="${cell#*:}"
    set_netem "$spec"
    sleep 3
    r=$(raw_get "$RPORT" "$PER" "$CONNS"); cool 20
    q=$(raw_get "$QPORT" "$PER" "$CONNS"); cool 20
    printf '  %-26s %10s %10s %8s  %s\n' "$name" "${r:-0}" "${q:-0}" "$(ratio "${q:-0}" "${r:-0}")" \
        "$(tfld "$QPORT" current_path)"
done
clear_netem

echo
echo "  ratio > 1 means QUIC direct is faster under that condition."
echo "  A quic path reading 'relay' means the direct path was refused or lost"
echo "  under that condition and the connection was served on the warm relay —"
echo "  which is the designed behaviour, not a failure, but it makes the cell's"
echo "  quic column a RELAY number and it must not be quoted as a QUIC one."
stop_all
echo DONE
