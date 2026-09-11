#!/usr/bin/env bash
# Prove, ON THE DEPLOYED SERVER, that the two startup-time fixes of the
# public-tunnel campaign actually took effect in the running process.
#
# Why a separate script and not an eyeball on `docker logs`: both fixes are
# invisible in normal operation and were found precisely because nothing
# reported them. A log line only proves the server TALKED about it; what
# matters is the kernel's own view of the process, so every assertion below
# reads the kernel through the container's PID namespace from the host:
#
#   P-12  --max-conns reconciled with RLIMIT_NOFILE   -> /proc/<pid>/limits
#   P-13  the shared QUIC endpoint's socket buffers   -> ss -uapm, rb/tb
#
# It restarts nothing and moves no traffic, so it is safe to run against a
# live server at any time, and it is the "after" half of the campaign's
# evidence: the same two quantities are read the same way in the lab gates
# T-PUB-FDBUDGET and T-PUB-UDPBUF.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

[ -n "$BORE_SRV" ] || { echo "BORE_SRV is not set in env.sh"; exit 2; }
CONTAINER="${BORE_SRV_CONTAINER:-bore-server}"

PASS=0; FAIL=0
ok()   { echo "PASS: $*"; PASS=$((PASS+1)); }
bad()  { echo "FAIL: $*"; FAIL=$((FAIL+1)); }

say "build under test"
VER="$(srv "sudo -n docker exec $CONTAINER /bore --version 2>/dev/null" 2>/dev/null | tr -d '\r')"
echo "  ${VER:-unknown}"
[ -n "$VER" ] || { echo "the container is not answering --version"; exit 1; }

# The container's init PID as the HOST sees it. Everything below is read
# through it: the image is distroless (no shell, no `cat`, no `ss`), so
# reading from inside is not an option, and the host's /proc is the truth
# anyway.
PID="$(srv "sudo -n docker inspect -f '{{.State.Pid}}' $CONTAINER" 2>/dev/null | tr -d '\r')"
case "$PID" in ''|*[!0-9]*) echo "could not resolve the container PID (got '$PID')"; exit 1 ;; esac
echo "  container pid on the host: $PID"

# --- the configured bound ---------------------------------------------------
MAXC="$(adm config 2>/dev/null | jq -r '.max_conns // empty' 2>/dev/null)"
echo "  --max-conns as the server reports it: ${MAXC:-unset}"

say "P-12 — the file-descriptor limit covers --max-conns"
LIM="$(srv "sudo -n awk '/Max open files/{print \$4, \$5}' /proc/$PID/limits" 2>/dev/null | tr -d '\r')"
SOFT="${LIM%% *}"; HARD="${LIM##* }"
echo "  /proc/$PID/limits  soft=$SOFT hard=$HARD"
if [ -z "${MAXC:-}" ] || [ "$MAXC" = "null" ]; then
    echo "  (--max-conns is not set on this server: nothing to reconcile, skipping)"
elif [ -z "$SOFT" ]; then
    bad "P-12: could not read the descriptor limit"
elif [ "$SOFT" -gt "$MAXC" ]; then
    ok "P-12: soft limit $SOFT exceeds --max-conns $MAXC (headroom $((SOFT-MAXC)))"
else
    bad "P-12: soft limit $SOFT does NOT exceed --max-conns $MAXC — the kernel will \
refuse with EMFILE on every listener before the semaphore refuses gracefully"
fi
# The log line is corroboration, never the assertion: a server whose limit was
# already sufficient stays silent on purpose.
srv "sudo -n docker logs $CONTAINER 2>&1 | grep -m2 -E 'file-descriptor limit'" 2>/dev/null \
    | sed 's/^/  log: /' || echo "  log: (silent — the limit needed no change)"

say "P-13 — the shared QUIC endpoint's socket buffers were configured"
QPORT="$(adm config 2>/dev/null | jq -r '.vhost_quic_port // empty' 2>/dev/null)"
if [ -z "${QPORT:-}" ] || [ "$QPORT" = "null" ] || [ "$QPORT" = "0" ]; then
    echo "  (this server has no QUIC endpoint — --udp is off; skipping)"
else
    echo "  --vhost-quic-port: $QPORT"
    # `ss` runs on the HOST, entered into the container's network namespace.
    # -m is what carries skmem:(r…,rb…,t…,tb…): rb/tb are the kernel's own
    # receive and send buffer sizes for that socket, which is the whole
    # question P-13 asks.
    SK="$(srv "sudo -n nsenter -t $PID -n ss -uapm 2>/dev/null | grep -A1 ':$QPORT' | tr '\n' ' '" 2>/dev/null | tr -d '\r')"
    RB="$(printf '%s' "$SK" | sed -n 's/.*rb\([0-9]*\).*/\1/p')"
    TB="$(printf '%s' "$SK" | sed -n 's/.*tb\([0-9]*\).*/\1/p')"
    DEF="$(srv "sudo -n sysctl -n net.core.rmem_default" 2>/dev/null | tr -d '\r')"
    DEF="${DEF:-212992}"
    echo "  socket: rb=${RB:-?} tb=${TB:-?}   net.core.rmem_default=$DEF"
    if [ -z "$RB" ] || [ -z "$TB" ]; then
        bad "P-13: could not read the QUIC socket's buffers from the kernel"
    else
        # 4x the kernel default is the same threshold the lab gate uses: it is
        # far above anything the default path produces and far below the 16 MiB
        # request, so it passes whether or not net.core.rmem_max clamped it.
        [ "$RB" -gt $((DEF*4)) ] && ok "P-13: receive buffer $RB is far above the untuned default $DEF" \
                                 || bad "P-13: receive buffer $RB is at or near the untuned default $DEF — \
the endpoint is running on an unconfigured socket, which caps a download at roughly buffer/RTT"
        [ "$TB" -gt $((DEF*4)) ] && ok "P-13: send buffer $TB is far above the untuned default $DEF" \
                                 || bad "P-13: send buffer $TB is at or near the untuned default $DEF"
    fi
    # Either line is acceptable: "configured" when the kernel granted the
    # request, "clamped" when net.core.rmem_max cut it short — the second is
    # still the fix working, and it names the sysctl remedy.
    if srv "sudo -n docker logs $CONTAINER 2>&1 | grep -qE 'configured UDP socket buffers|UDP socket buffer clamped below request'" 2>/dev/null; then
        ok "P-13: the server logged the buffer configuration at startup"
        srv "sudo -n docker logs $CONTAINER 2>&1 | grep -m2 -E 'configured UDP socket buffers|UDP socket buffer clamped' " 2>/dev/null | cut -c1-240 | sed 's/^/  log: /'
    else
        bad "P-13: the server never logged configuring the UDP socket buffers"
    fi
fi

say "P-11 — the config endpoint publishes the CONFIGURED slot total, not a gauge"
CFG="$(adm config 2>/dev/null | jq -r '.udp_direct_slots // "null"' 2>/dev/null)"
MET="$(adm metrics 2>/dev/null | jq -r '.udp_direct_slots_available // "null"' 2>/dev/null)"
echo "  config.udp_direct_slots=$CFG  metrics.udp_direct_slots_available=$MET"
if [ "$CFG" = "null" ]; then
    echo "  (no --udp-memory-budget configured: both are null by design, skipping)"
elif [ "$MET" = "null" ]; then
    bad "P-11: the config total is published but the live gauge is missing from /metrics"
elif [ "$MET" -le "$CFG" ]; then
    ok "P-11: the gauge ($MET) sits at or below the configured total ($CFG), as it must"
else
    bad "P-11: the live gauge ($MET) exceeds the configured total ($CFG)"
fi

echo
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" = 0 ]
