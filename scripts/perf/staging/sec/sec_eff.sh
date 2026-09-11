#!/usr/bin/env bash
# S3: what each transport COSTS, in CPU seconds per delivered GiB, on all
# three machines at once.
#
# This is the measurement that makes the secret-tunnel direct path's case, and
# it is the one the public and vhost campaigns could not make: there, "direct"
# still terminated on the server, so the server paid for the bytes either way.
# Here the direct path does not touch the server at all — the server brokers
# the punch and then sees nothing but control heartbeats — so the relay's
# server-side bill is a cost the direct arm simply does not incur, and a
# per-GiB figure is the only way to state that without it being an artefact of
# how much traffic each arm happened to move.
#
# Every number is read from /proc/stat on the HOST (never the container): the
# vhost campaign measured softirq at 37-40 % of the bill, and reading the
# container alone understates the cost by a third. Steal is reported but never
# counted as work — on a burstable instance it is the credit bucket, not bore.
#
# Requires the samplers to be running (res/start_samplers.sh); the windows are
# stamped here and reduced afterwards by res/cpu_window.sh, exactly as
# `pub/vm_pub_eff.sh` does, so the two campaigns' efficiency figures are
# directly comparable.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/seclib.sh"

TOPO="${TOPO:-vm-ws}"
GIB="${GIB:-2}"
CONNS="${CONNS:-4}"
PER=$(( GIB * 1073741824 / CONNS ))
PP="$SEC_PROXY_PORT"

start_provider() { local id="$1"; shift
    case "$TOPO" in vm-ws|vm-vm) vm_provider "$RP" "$id" "$@" ;;
                    ws-vm) ws_provider "$RP" "$id" "$@"; PROV_PID="$LASTPID" ;; esac; }
start_consumer() { local id="$1"; shift
    case "$TOPO" in vm-ws) ws_consumer "$PP" "$id" "$@"; CONS_PID="$LASTPID" ;;
                    ws-vm|vm-vm) vm_consumer "$PP" "$id" "$@" ;; esac; }
drive() { local dir="$1" per="$2" conns="$3"
    case "$TOPO" in
        vm-ws) python3 "$WS_RAWCLI" "$dir" 127.0.0.1 "$PP" "$per" "$conns" 2>/dev/null ;;
        ws-vm|vm-vm) vm "python3 $VM_RAWCLI $dir 127.0.0.1 $PP $per $conns" 2>/dev/null ;;
    esac | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

case "$TOPO" in
    vm-ws|vm-vm) vm "pgrep -f 'raw_origin.py $RP' >/dev/null 2>&1 || \
        (setsid nohup python3 ~/raw_origin.py $RP >~/out/raworigin.log 2>&1 </dev/null & true)" >/dev/null 2>&1 ;;
    ws-vm) pgrep -f "raw_origin.py $RP" >/dev/null 2>&1 || {
               python3 "$HERE/../../raw_origin.py" "$RP" >"$OUT/raworigin.log" 2>&1 &
               SEC_KIDS+=("$!"); sleep 1; } ;;
esac

say "secret efficiency: $GIB GiB per arm over $CONNS conns, topology $TOPO"
echo "  reduce each window with:"
echo "    res/cpu_window.sh out/pres_<host>.stat <t0> <t1> $GIB out/pres_<host>.proc"

arm() { # <label> <flags...>
    local label="$1"; shift
    local id; id="$(sec_id)"
    PROV_PID=""; CONS_PID=""
    start_provider "$id" "$@"
    sec_wait secretprovider "$id" || { echo "  $label: provider never registered"; sec_down "$id" ${PROV_PID:-}; return 1; }
    start_consumer "$id" "$@"
    sec_wait secretconsumer "$id" || { echo "  $label: consumer never registered"; sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}; return 1; }
    drive get 1048576 1 >/dev/null 2>&1
    local t0 t1 r path fb
    t0=$(date +%s)
    r=$(drive get "$PER" "$CONNS")
    t1=$(date +%s)
    path="$(sec_path "$id")"; fb="$(sec_fallbacks "$id")"
    # The server's own relay counters: on the direct arm they must stay
    # essentially flat, and that flatness IS the claim. Reading them makes the
    # claim falsifiable instead of rhetorical.
    local rtx rrx
    rtx="$(sec_field secretconsumer "$id" relay_tx_bytes 0)"
    rrx="$(sec_field secretconsumer "$id" relay_rx_bytes 0)"
    printf '  %-8s %8s MB/s  path=%-7s fb=%-3s relay_tx=%-12s relay_rx=%-12s window=%s-%s gib=%s\n' \
        "$label" "$r" "$path" "$fb" "$rtx" "$rrx" "$t0" "$t1" "$GIB"
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    cool
}

arm relay
arm direct --udp
echo
echo DONE
