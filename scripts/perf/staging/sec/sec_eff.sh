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

# Kernel packet counters on each PEER, read from /proc/net/snmp.
#
# S3 answers "what does each transport cost in CPU". It cannot, on its own, say
# WHY: a sender paying 3.5x more CPU per GiB on the direct path is either doing
# more work per packet (user-space crypto and pacing) or sending far more
# packets (no segmentation offload — TCP hands 2 GiB to the NIC in a few
# thousand large segments, QUIC writes every datagram itself). Those are
# different stories with different fixes, and the difference is visible in
# packets-per-delivered-GiB.
#
# TWO instruments, because one of them lies for exactly this comparison:
#
#   * /proc/net/snmp. `Tcp: OutSegs` is incremented by `tcp_skb_pcount()`, so it
#     already counts GSO segments and is a true segment count. `Udp:
#     OutDatagrams` is incremented once per sendmsg, so with UDP GSO — which
#     quinn uses — one count can be many wire packets. Measured on the vm-vm
#     smoke: 84 041 "datagrams" for 1 GiB, i.e. 12.8 KB each, far above any MTU.
#     Comparing that against TCP's true segment count would flatter QUIC by
#     whatever the GSO batch size happens to be. So the UDP figure is reported
#     as KERNEL SUBMISSIONS and never as packets.
#   * `ip -s link` on the default route's interface, which is counted in the
#     driver. The test VM has `tcp-segmentation-offload: off` with GSO on, so
#     segmentation happens in the stack before the driver and these ARE wire
#     packets, for both transports, on the same footing.
#
# Both are HOST-WIDE, which is the H-17 caveat one layer down: on the dedicated
# test VM that is exact, on the shared workstation the transfer's millions of
# packets still dwarf a desktop's thousands, but the VM row is the one to quote.
# Read from the kernel, never from the application (P-12).
#
# NOTE the vm-vm topology cannot answer this question at all: its direct arm is
# loopback and never reaches the NIC, while its relay arm crosses it twice. Same
# reason §3.2 gives for not calling vm-vm a CPU isolation. Use vm-ws.
nic_raw() { # <ws|vm> -> `ip -s link` for the default-route interface
    case "$1" in
        ws) ip -s link show "$(ip -o -4 route show to default | cut -d' ' -f5 | head -1)" 2>/dev/null ;;
        vm) vm 'ip -s link show $(ip -o -4 route show to default | cut -d" " -f5 | head -1)' 2>/dev/null | tr -d '\r' ;;
    esac
}

net_counters() { # <ws|vm> -> "tcp_out tcp_in udp_out udp_in nic_tx nic_rx"
    local raw
    case "$1" in
        ws) raw="$(cat /proc/net/snmp 2>/dev/null)" ;;
        vm) raw="$(vm 'cat /proc/net/snmp' 2>/dev/null | tr -d '\r')" ;;
        *)  echo "0 0 0 0"; return ;;
    esac
    printf '%s\n' "$raw" | awk '
        /^Tcp:/ { if (!ts) { for (i=2;i<=NF;i++) tk[$i]=i; ts=1 }
                  else { tout=$(tk["OutSegs"]); tin=$(tk["InSegs"]) } }
        /^Udp:/ { if (!us) { for (i=2;i<=NF;i++) uk[$i]=i; us=1 }
                  else { uout=$(uk["OutDatagrams"]); uin=$(uk["InDatagrams"]) } }
        END { printf "%d %d %d %d", tout+0, tin+0, uout+0, uin+0 }'
    # `ip -s link` prints a header line then a values line, per direction.
    nic_raw "$1" | awk '
        /RX:/ { getline; rxp=$2 }
        /TX:/ { getline; txp=$2 }
        END { printf " %d %d\n", txp+0, rxp+0 }'
}

# Which two hosts are the PEERS of this topology (the server is never on the
# direct path, S-1, so its packet counters answer a different question).
peer_hosts() {
    case "$TOPO" in
        vm-ws) echo "vm ws" ;;
        ws-vm) echo "ws vm" ;;
        vm-vm) echo "vm" ;;
    esac
}

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
    # Packet counters bracket the SAME window as the CPU samplers, so the two
    # reductions describe one transfer and not two.
    local h c0 c1
    declare -A PKT0=()
    for h in $(peer_hosts); do PKT0[$h]="$(net_counters "$h")"; done
    local t0 t1 r path fb
    t0=$(date +%s)
    r=$(drive get "$PER" "$CONNS")
    t1=$(date +%s)
    declare -A PKT1=()
    for h in $(peer_hosts); do PKT1[$h]="$(net_counters "$h")"; done
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
    # One line per peer: packets moved in the window and, since GiB is known,
    # packets per delivered GiB — the figure that separates "more work per
    # packet" from "more packets".
    for h in $(peer_hosts); do
        set -- ${PKT0[$h]}; local a1=$1 a2=$2 a3=$3 a4=$4 a5=${5:-0} a6=${6:-0}
        set -- ${PKT1[$h]}; local b1=$1 b2=$2 b3=$3 b4=$4 b5=${5:-0} b6=${6:-0}
        awk -v h="$h" -v lbl="$label" -v gib="$GIB" \
            -v to=$((b1-a1)) -v ti=$((b2-a2)) -v uo=$((b3-a3)) -v ui=$((b4-a4)) \
            -v nt=$((b5-a5)) -v nr=$((b6-a6)) \
            'BEGIN {
                 # /proc/net/snmp SNMP counters are 32-bit on some kernels, so a
                 # wrap inside the window shows up as a negative delta. Say so
                 # rather than print a nonsense rate: a wrapped counter is not a
                 # measurement and must not be reduced as one.
                 if (to < 0 || ti < 0 || uo < 0 || ui < 0 || nt < 0 || nr < 0) {
                     printf "    pkts %-3s %-7s COUNTER WRAPPED in the window — not a measurement\n", h, lbl
                     exit
                 }
                 printf "    pkts %-3s %-7s tcp_segs_out=%-9d udp_sends_out=%-9d  NIC tx=%-10d rx=%-10d  per_gib: nic_tx=%.0f\n",
                     h, lbl, to, uo, nt, nr, nt/gib }'
    done
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
