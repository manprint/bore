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
# The held connections are IDLE on purpose: they speak the origin's `HOLD`
# verb, which answers once and then moves NO bytes at all. An arm whose held
# connections were downloading would saturate the link and measure the link.
#
# They are also held from ONE process (`raw_client.py hold` opens them all
# with asyncio). One OS process per connection would put 512 Python
# interpreters on a 2-vCPU / 3.8 GiB VM and measure the driver's own memory
# pressure instead of the tunnel.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

LADDER="${LADDER:-16 64 128 256 512}"
PROBES="${PROBES:-30}"

start_origins || exit 1

# Hold N idle connections open against the RAW origin, then probe a FRESH
# connection through the same tunnel. `active_at_server` is read from the
# admin API so the arm reports how many the SERVER actually has, not how many
# the driver asked for — if they diverge, the number to trust is the server's
# and the arm says so.
HOLDSECS="${HOLDSECS:-45}"
hold_and_probe() { # <public_port> <n>
    local p="$1" n="$2" hp up
    local hf; hf="$OUT/hold-$p-$n.txt"
    python3 "$RAWCLI" hold "$GW" "$p" "$HOLDSECS" "$n" 30 >"$hf" 2>&1 &
    hp=$!
    # Wait for the driver's own "up" line rather than a fixed sleep: at 512
    # connections the ramp itself takes seconds, and probing mid-ramp measures
    # the ramp.
    local i
    for i in $(seq 60); do grep -q '^held=' "$hf" 2>/dev/null && break; sleep 0.5; done
    up=$(grep -oE 'up=[0-9]+' "$hf" | head -1 | cut -d= -f2)
    sleep 2
    local act; act=$(tfld "$p" active)
    local res; res=$(raw_ping "$p" "$PROBES")
    kill -9 "$hp" 2>/dev/null
    wait "$hp" 2>/dev/null
    echo "held=$n up=${up:-?} active_at_server=${act:-?} $res"
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
