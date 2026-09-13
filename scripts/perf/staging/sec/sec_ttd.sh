#!/usr/bin/env bash
# S2: how OFTEN does the punch succeed, how LONG does it take, and when it
# fails, WHY — the three questions a secret-tunnel operator actually has.
#
# This is a census, not a benchmark: N independent tunnels are brought up and
# torn down, and for each one the harness records the path the consumer chose,
# the milliseconds from "tunnel registered" to `current_path == direct`, the
# fallback counter and the server's own reason string. The distribution is the
# result; a single successful punch proves nothing about a NAT pair, because
# hole punching is a race and a race has a failure rate.
#
# Why the reason string matters as much as the rate: `path_reason` separates
# "no udp-capable provider registered" (a configuration mistake — the operator
# forgot --udp on one side) from a genuine traversal failure. Reporting a
# single "fallback rate" that merges the two would blame the network for a
# missing flag, which is the fastest way to have a real traversal bug
# dismissed as "NAT is hard".
#
# TOPO, role placement and the no-blanket-kill rule are seclib's; see sec_ab.sh
# for the topology table.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/seclib.sh"

TOPO="${TOPO:-vm-ws}"
N="${N:-20}"
GAP="${GAP:-3}"          # seconds between tunnels; this moves almost no bytes
PP="$SEC_PROXY_PORT"
FLAGS="${FLAGS:---udp}"

case "$TOPO" in vm-ws|ws-vm|vm-vm) ;; *) echo "bad TOPO '$TOPO'" >&2; exit 2 ;; esac

start_provider() { local id="$1"; shift
    case "$TOPO" in vm-ws|vm-vm) vm_provider "$RP" "$id" "$@" ;;
                    ws-vm) ws_provider "$RP" "$id" "$@"; PROV_PID="$LASTPID" ;; esac; }
start_consumer() { local id="$1"; shift
    case "$TOPO" in vm-ws) ws_consumer "$PP" "$id" "$@"; CONS_PID="$LASTPID" ;;
                    ws-vm|vm-vm) vm_consumer "$PP" "$id" "$@" ;; esac; }
poke() { # one tiny proxied connection: what triggers the direct open
    case "$TOPO" in
        vm-ws) python3 "$WS_RAWCLI" get 127.0.0.1 "$PP" 65536 1 >/dev/null 2>&1 ;;
        ws-vm|vm-vm) vm "python3 $VM_RAWCLI get 127.0.0.1 $PP 65536 1" >/dev/null 2>&1 ;;
    esac
}

sec_start_origin "$TOPO" || exit 1
say "secret path census: $N tunnels, topology $TOPO, flags '$FLAGS'"
printf '  %-4s %-8s %10s %6s  %s\n' n path ttd_ms fb reason

DIRECT=0; RELAY=0; FAILED=0
TTDS=()
declare -A REASONS=()
for i in $(seq "$N"); do
    id="$(sec_id)"
    PROV_PID=""; CONS_PID=""
    start_provider "$id" $FLAGS
    if ! sec_wait secretprovider "$id"; then
        printf '  %-4s %-8s %10s %6s  %s\n' "$i" "PROVFAIL" "-" "-" "provider never registered"
        FAILED=$((FAILED+1)); sec_down "$id" ${PROV_PID:-}; sleep "$GAP"; continue
    fi
    start_consumer "$id" $FLAGS
    if ! sec_wait secretconsumer "$id"; then
        printf '  %-4s %-8s %10s %6s  %s\n' "$i" "CONSFAIL" "-" "-" "consumer never registered"
        FAILED=$((FAILED+1)); sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}; sleep "$GAP"; continue
    fi
    poke
    ttd="$(sec_time_to_direct "$id" 30)"
    path="$(sec_path "$id")"; fb="$(sec_fallbacks "$id")"; why="$(sec_reason "$id")"
    printf '  %-4s %-8s %10s %6s  %s\n' "$i" "$path" "$ttd" "$fb" "${why:-—}"
    if [ "$path" = direct ]; then
        DIRECT=$((DIRECT+1)); TTDS+=("$ttd")
    else
        RELAY=$((RELAY+1))
        key="${why:-no reason reported}"
        REASONS["$key"]=$(( ${REASONS["$key"]:-0} + 1 ))
    fi
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    sleep "$GAP"
done

echo
echo "===== census ($TOPO, $N attempts) ====="
echo "  direct : $DIRECT"
echo "  relay  : $RELAY"
echo "  failed : $FAILED  (tunnel never registered — not a traversal result)"
if [ "${#TTDS[@]}" -gt 0 ]; then
    echo "  time-to-direct ms: median $(printf '%s\n' "${TTDS[@]}" | med)  \
min $(printf '%s\n' "${TTDS[@]}" | LC_ALL=C sort -n | head -1)  \
max $(printf '%s\n' "${TTDS[@]}" | LC_ALL=C sort -n | tail -1)"
fi
if [ "${#REASONS[@]}" -gt 0 ]; then
    echo "  relay reasons:"
    for k in "${!REASONS[@]}"; do printf '    %4s  %s\n' "${REASONS[$k]}" "$k"; done
fi
echo
echo DONE
