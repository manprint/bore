#!/usr/bin/env bash
# S4: latency and concurrency through a secret tunnel, relay vs direct.
#
# Two distinct questions, deliberately not averaged together:
#
#  * NEW-CONNECTION latency — one fresh TCP connection per probe, serially.
#    That is what a real client pays per request and what the tunnel's
#    per-connection open costs. On the relay the connection crosses the
#    Internet TWICE (consumer -> server -> provider); on the direct path it
#    crosses once, and the difference is the clearest user-visible payoff of
#    hole punching. A concurrent probe would measure queueing instead.
#
#  * CONCURRENCY — the same probe while N connections are held open. The
#    direct path multiplexes every proxied connection onto ONE QUIC connection
#    as separate bidi streams, so this is where a head-of-line problem would
#    show; the relay opens a yamux substream per connection over TCP, where a
#    loss event stalls every stream on that carrier. They fail differently and
#    a single number would hide which.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/seclib.sh"

TOPO="${TOPO:-vm-ws}"
PROBES="${PROBES:-100}"
HOLDS="${HOLDS:-0 16 64 256}"
PP="$SEC_PROXY_PORT"

start_provider() { local id="$1"; shift
    case "$TOPO" in vm-ws|vm-vm) vm_provider "$RP" "$id" "$@" ;;
                    ws-vm) ws_provider "$RP" "$id" "$@"; PROV_PID="$LASTPID" ;; esac; }
start_consumer() { local id="$1"; shift
    case "$TOPO" in vm-ws) ws_consumer "$PP" "$id" "$@"; CONS_PID="$LASTPID" ;;
                    ws-vm|vm-vm) vm_consumer "$PP" "$id" "$@" ;; esac; }
# raw_client.py's own verbs: `ping` is N sequential new connections, `hold` is
# N connections opened and parked. Both run on the consumer's host.
rc() {
    case "$TOPO" in
        vm-ws) python3 "$WS_RAWCLI" "$@" ;;
        ws-vm|vm-vm) vm "python3 $VM_RAWCLI $*" ;;
    esac
}

sec_start_origin "$TOPO" || exit 1

say "secret latency/concurrency: $PROBES probes, holds [$HOLDS], topology $TOPO"

for mode in relay direct; do
    flags=""; [ "$mode" = direct ] && flags="--udp"
    id="$(sec_id)"; PROV_PID=""; CONS_PID=""
    start_provider "$id" $flags
    sec_wait secretprovider "$id" || { echo "  $mode: provider never registered"; sec_down "$id" ${PROV_PID:-}; continue; }
    start_consumer "$id" $flags
    sec_wait secretconsumer "$id" || { echo "  $mode: consumer never registered"; sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}; continue; }
    rc get 127.0.0.1 "$PP" 1048576 1 >/dev/null 2>&1
    path="$(sec_path "$id")"
    echo "  --- $mode (path=$path, fb=$(sec_fallbacks "$id")) ---"
    for h in $HOLDS; do
        holdpid=""
        if [ "$h" -gt 0 ]; then
            case "$TOPO" in
                vm-ws) python3 "$WS_RAWCLI" hold 127.0.0.1 "$PP" 600 "$h" >/dev/null 2>&1 &
                       holdpid=$!; SEC_KIDS+=("$holdpid") ;;
                ws-vm|vm-vm) vm "setsid nohup python3 $VM_RAWCLI hold 127.0.0.1 $PP 600 $h >~/out/hold-$id.log 2>&1 </dev/null & true" >/dev/null 2>&1 ;;
            esac
            sleep 3
        fi
        echo "    held=$h : $(rc ping 127.0.0.1 "$PP" "$PROBES" 2>/dev/null | tr -d '\r')"
        if [ "$h" -gt 0 ]; then
            [ -n "$holdpid" ] && { kill -9 "$holdpid" 2>/dev/null; wait "$holdpid" 2>/dev/null; }
            vm "pkill -9 -f 'hold 127.0.0.1 $PP' 2>/dev/null; true" >/dev/null 2>&1
            sleep 2
        fi
    done
    echo "    path after the ladder: $(sec_path "$id") fb=$(sec_fallbacks "$id")"
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    cool 20
done
echo
echo DONE
