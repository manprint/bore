#!/usr/bin/env bash
# P6: concurrency. How does a public tunnel behave with many simultaneous
# proxied connections, and what does a FRESH connection cost while they are
# held open?
#
# This is the public-path counterpart of the vhost concurrency ladder (N-9).
# The vhost campaign measured a fresh request at 966 ms behind 256 held
# connections on staging and 11 ms flat on a private server, and concluded the
# instance's allowance bucket was the only surviving explanation. The public
# path differs in two ways worth measuring separately:
#   * there is no TLS and no HTTP head parsing on the tunnel port, so a fresh
#     connection is one TCP handshake plus one substream open, nothing else;
#   * the server opens a substream per INBOUND connection, so the cost of
#     concurrency lands in a different place than it does for vhost.
#
# The held connections are IDLE on purpose (they have asked for bytes the
# origin streams slowly), so what is measured is the cost of CONCURRENCY, not
# of bandwidth: an arm that saturates the link would measure the link.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

LADDER="${LADDER:-16 64 128 256 512}"
PROBES="${PROBES:-30}"

start_origins || exit 1

# Hold N connections open against the RAW origin, each asking for a large
# amount it will never finish inside the measurement window, then probe.
hold_and_probe() { # <public_port> <n>
    local p="$1" n="$2" pids=() i
    for i in $(seq "$n"); do
        python3 "$RAWCLI" get "$GW" "$p" $((4 * 1073741824)) 1 30 >/dev/null 2>&1 &
        pids+=("$!")
    done
    sleep 4
    local act; act=$(tfld "$p" active_conns)
    local res; res=$(raw_ping "$p" "$PROBES")
    for i in "${pids[@]}"; do kill -9 "$i" 2>/dev/null; done
    wait 2>/dev/null
    echo "held=$n active_at_server=${act:-?} $res"
}

for mode in relay quic; do
    flags="--carriers 1"; [ "$mode" = quic ] && flags="--carriers 1 --udp"
    echo
    echo "===== P6 concurrency ladder, $mode ====="
    if ! up_native "$RP" 0 $flags; then echo "  REGISTRATION FAILED"; continue; fi
    p="$LASTPORT"; pid="$LASTPID"
    python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
    echo "  baseline (nothing held): $(raw_ping "$p" "$PROBES")"
    for n in $LADDER; do
        echo "  $(hold_and_probe "$p" "$n")"
        sleep 5
    done
    echo "  path=$(tfld "$p" current_path) opens=$(tfld "$p" direct_stream_opens) fb=$(tfld "$p" direct_fallbacks)"
    down "$pid" "$p"
    cool 30
done

echo
echo "===== P6b carriers under concurrency (relay only) ====="
echo "  The carrier pool exists to break yamux head-of-line blocking. If it"
echo "  does anything for public tunnels, it must show up HERE and not in the"
echo "  single-stream ladder."
for c in 1 4 8; do
    if up_native "$RP" 0 --carriers "$c"; then
        p="$LASTPORT"; pid="$LASTPID"
        python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
        echo "  carriers=$c $(hold_and_probe "$p" 128)"
        down "$pid" "$p"
        sleep 10
    else echo "  carriers=$c REGISTRATION FAILED"; fi
done
echo DONE
