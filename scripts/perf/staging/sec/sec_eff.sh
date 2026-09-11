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
# GIB and CONNS go straight into a shell arithmetic expansion, which on a
# non-integer does NOT abort the script: `$(( 0.25 * ... ))` prints a syntax
# error to stderr, leaves PER unset, and every arm then transfers whatever
# `$PER` expands to. A smoke run with GIB=0.25 moved 1 MiB in under a second
# and still printed a row, with an empty MB/s and t0 == t1 — a result shaped
# exactly like a real one. Reject it here instead.
case "$GIB" in ''|*[!0-9]*) echo "GIB must be a positive integer (got '$GIB')" >&2; exit 2 ;; esac
case "$CONNS" in ''|*[!0-9]*) echo "CONNS must be a positive integer (got '$CONNS')" >&2; exit 2 ;; esac
[ "$GIB" -gt 0 ] && [ "$CONNS" -gt 0 ] || { echo "GIB and CONNS must be > 0" >&2; exit 2; }
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

sec_start_origin "$TOPO" || exit 1

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
    # A --udp arm must be ON the direct path BEFORE the window opens, and this
    # is not pedantry: on 2026-09-11 the vm-ws direct arm opened its window
    # while the peer's QUIC listener was still coming up (the S-5 stall: the
    # provider's check round finished `nominated=None checks_ms=1126` at
    # 21:40:30.592 while the consumer had nominated and was already dialing at
    # 21:40:29.684), the transfer collapsed, and the stage printed
    # `0.00 MB/s path=unknown` in a 1-second window — a row shaped like a
    # result. sec_ab.sh already waited here; this stage did not.
    local ttd="n/a"
    case " $* " in *" --udp "*) ttd="$(sec_time_to_direct "$id" 30)" ;; esac
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
    # An arm that did not run on the transport it is named after is NOT a
    # measurement of that transport, and a CPU-per-GiB figure reduced from its
    # window would be attributed to the wrong path. Say so on the row, and say
    # it in a form no downstream reduction can mistake for a rate.
    local verdict=ok
    case "$label" in
        direct) [ "$path" = direct ] || verdict="INVALID (ran on '$path', not direct)" ;;
        relay)  [ "$path" = relay  ] || verdict="INVALID (ran on '$path', not relay)"  ;;
    esac
    [ "$t1" -gt "$t0" ] || verdict="INVALID (empty window, ${t0}-${t1})"
    printf '  %-8s %8s MB/s  path=%-7s fb=%-3s ttd=%-6s relay_tx=%-12s relay_rx=%-12s window=%s-%s gib=%s%s\n' \
        "$label" "$r" "$path" "$fb" "$ttd" "$rtx" "$rrx" "$t0" "$t1" "$GIB" \
        "$([ "$verdict" = ok ] || printf '  <<< %s' "$verdict")"
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    cool
    [ "$verdict" = ok ]
}

RC=0
arm relay      || RC=1
arm direct --udp || RC=1
[ "$RC" = 0 ] || echo "  one or more arms are INVALID — do not reduce their windows"
echo
echo DONE
