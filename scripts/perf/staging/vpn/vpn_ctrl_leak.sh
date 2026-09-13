#!/usr/bin/env bash
# V-16: does a VPN connector leak a control connection per reconnect?
#
# WHY THIS IS A STAGE AND NOT A SCRATCH SCRIPT
# --------------------------------------------
# This measurement FOUND the one product defect the VPN campaign still has open,
# and the script that made it lived in a session scratchpad and is gone. A
# finding whose instrument no longer exists cannot be re-checked, cannot gate a
# fix, and cannot be handed to anyone. So it lives here now, and the fix -- when
# it lands -- is gated by this file.
#
# Design and mechanism: docs/vpn/VPN_CTRL_CONN_LEAK.md
#
# WHAT WAS MEASURED
# -----------------
#   fresh link          ctrl_conns=1  fds=12
#   after reconnect 1   ctrl_conns=2  fds=13
#   after reconnect 2   ctrl_conns=3  fds=14
#   after reconnect 3   ctrl_conns=4  fds=15
#   t+30..120 s         ctrl_conns=4  fds=15      <- never reaped
#
# One ESTABLISHED TCP connection to the server's control port is correct. Four
# is three leaked descriptors and three leaked sockets on the server too.
#
# TWO RULES THIS STAGE OBEYS, BOTH LEARNED THE HARD WAY
# ------------------------------------------------------
# 1. COUNT FROM THE KERNEL, NEVER FROM THE LOG (P-12). The log says what bore
#    believes it did; `ss` says what the machine is actually holding. The count
#    comes through the root helper's `fdlist` verb because /proc/<pid>/fd on a
#    root process is mode 0500 -- a `sudo ls` at the call site would prompt
#    (NOPASSWD sudo here is per EXACT path) and the stage would report zero
#    descriptors forever, which reads as "no leak".
# 2. KILL BY PID, NEVER BY PATTERN. `ssh host "pkill -f 'vpn listen --id X'"`
#    puts that string in the remote shell's own argv, so the pattern matches the
#    shell running it. The listener is located by link id and then filtered by
#    EXCLUSION (reject sudo/env/sh/bash), which is also what makes it robust
#    against the binary being deployed under a different name.
#
# The settle window is not decoration: a connection in FIN_WAIT/TIME_WAIT is not
# a leak, it is TCP. Only what survives the window counts.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

CYCLES="${CYCLES:-4}"
SETTLE="${SETTLE:-120}"
# THE CONTROL PORT IS NOT A CONSTANT, AND HARDCODING IT MADE THIS GATE BLIND.
#
# This probe counts ESTABLISHED connections toward the server's control port. It
# read `7835` -- bore's default -- while `BORE_TO` in this deployment is
# `https://<host>` with no port, so the client connects on **443** through the
# TLS front. The filter therefore matched nothing, `ctrl_conns` returned 0 on
# every sample, and the verdict block printed
#
#     FAIL -- 0 established control connections where 1 is correct.
#             -1 leaked, unreaped after 120s.
#
# with its own descriptor dump listing FIVE established connections immediately
# below it. A gate that cannot see the thing it gates does not fail loudly: it
# fails with a number, and "-1 leaked" is the only reason anyone looked.
#
# So the port is DERIVED from the address the client is actually told to use,
# and `CTRL_PORT` stays an override for a deployment this derivation gets wrong.
derive_ctrl_port() {
    local to="${BORE_TO:-}" hostport scheme
    scheme="${to%%://*}"
    hostport="${to#*://}"
    hostport="${hostport%%/*}"
    case "$hostport" in
        \[*\]:*) printf '%s' "${hostport##*]:}"; return ;;   # [v6]:port
        \[*\])   : ;;                                        # [v6], no port
        *:*)      printf '%s' "${hostport##*:}"; return ;;    # host:port
    esac
    case "$scheme" in
        https) printf '443' ;;
        http)  printf '80'  ;;
        *)     printf '7835' ;;
    esac
}
CTRL_PORT="${CTRL_PORT:-$(derive_ctrl_port)}"
RUNDIR=/run/bore-vpn-bench
TAG=cl

vpn_hdr "VPN control-connection leak probe -- $CYCLES reconnects, ${SETTLE}s settle"
echo "  correct behaviour: exactly ONE established connection to the control"
echo "  port (:$CTRL_PORT, derived from BORE_TO) at all times, whatever the"
echo "  reconnect count."
echo

# Established connections this pid holds toward the server's control port, and
# its descriptor count. Both from the kernel, in one privileged round trip each.
ctrl_conns() {
    sudo -n "$ROOTSH" fdlist "$TAG" 2>/dev/null \
      | awk -v p=":$CTRL_PORT" '/ESTAB/ && index($0, p) {n++} END{print n+0}'
}
fd_count() {
    sudo -n "$ROOTSH" res "$TAG" 2>/dev/null | awk '{print $3+0}'
}

# The remote listener, by identity. See rule 2 above.
vm_listener_pid() {
    vm "for p in \$(pgrep -f -- '--id $VPN_LINK_ID' 2>/dev/null); do
            c=\$(cat /proc/\$p/comm 2>/dev/null)
            case \"\$c\" in sudo|env|sh|bash|'') continue;; esac
            echo \$p; break
        done" 2>/dev/null | tr -dc '0-9'
}

VPN_LINK_ID="${VPN_RUN_ID}cl$(date +%s%N | tail -c 5)"
VPN_WS_TAGS=(); VPN_VM_IDS=()


echo "=== bringing the link up (connector has --auto-reconnect) ==="
vm_up listen
sleep 3
ws_up "$TAG" connect --auto-reconnect
if ! ws_ready "$TAG" 60 >/dev/null; then
    echo "  link never came up -- nothing to measure"; exit 1
fi
sleep 5
printf '  %-22s ctrl_conns=%-3s fds=%s\n' "fresh link" "$(ctrl_conns)" "$(fd_count)"
SERIES=" $(ctrl_conns)"

for c in $(seq 1 "$CYCLES"); do
    lp="$(vm_listener_pid)"
    if [ -z "$lp" ]; then
        echo "  cycle $c: could not locate the VM listener -- skipped"
        continue
    fi
    vm "sudo -n kill -TERM $lp" >/dev/null 2>&1
    sleep 6
    # Bring the listener back so the connector can actually re-establish; a
    # connector that never reconnects is measuring a different thing.
    vm_up listen
    if ! ws_ready "$TAG" 90 >/dev/null; then
        echo "  cycle $c: link did not come back within 90 s"
    fi
    sleep 8
    printf '  %-22s ctrl_conns=%-3s fds=%s\n' "after reconnect $c" "$(ctrl_conns)" "$(fd_count)"
    SERIES+=" $(ctrl_conns)"
done

echo
echo "=== settling ${SETTLE}s (a socket in FIN_WAIT/TIME_WAIT is TCP, not a leak) ==="
for t in $(seq 30 30 "$SETTLE"); do
    sleep 30
    printf '  %-22s ctrl_conns=%-3s fds=%s\n' "t+${t}s" "$(ctrl_conns)" "$(fd_count)"
done

FINAL="$(ctrl_conns)"
echo
echo "=== verdict ==="
echo "  series across reconnects:$SERIES"
echo "  after settle: ctrl_conns=$FINAL"
if [ "$FINAL" = 1 ]; then
    echo "  PASS -- one control connection survives, whatever the reconnect count."
    rc=0
elif [ "${FINAL:-0}" -lt 1 ]; then
    # ZERO IS NOT A LEAK OF -1. A tunnel that carried traffic through four
    # reconnects has a control connection by construction, so counting none
    # means the INSTRUMENT missed it -- wrong port, wrong pid, or a link that
    # never came up. Reporting that as a product verdict is how this campaign
    # loses a whole stage, so the stage refuses to produce one.
    echo "  INSTRUMENT FAILURE -- counted $FINAL connections toward :$CTRL_PORT."
    echo "          A live link always holds one, so this is the probe missing it,"
    echo "          not the product losing it. Check that :$CTRL_PORT is really the"
    echo "          port this client dials (BORE_TO=${BORE_TO%%://*}://<host>), and"
    echo "          that the link below is the one being sampled."
    echo
    echo "  descriptor table at the end:"
    sudo -n "$ROOTSH" fdlist "$TAG" 2>/dev/null | sed 's/^/    /'
    rc=1
else
    echo "  FAIL -- $FINAL established control connections where 1 is correct."
    echo "          $(( FINAL - 1 )) leaked, unreaped after ${SETTLE}s."
    echo
    echo "  descriptor table at the end:"
    sudo -n "$ROOTSH" fdlist "$TAG" 2>/dev/null | sed 's/^/    /'
    rc=1
fi
echo
echo "DONE"
exit $rc
