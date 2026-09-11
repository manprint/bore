#!/usr/bin/env bash
# S5: does thinning QUIC's acknowledgements pay on the DIRECT path?
#
# `BORE_DIRECT_QUIC_ACK_THRESHOLD` installs a quinn `AckFrequencyConfig` with
# the given `ack_eliciting_threshold` on the direct endpoint, and it ships
# DISABLED: unset, `transport_config` never touches quinn's ack policy and the
# built config is byte-identical to every release before the knob existed. This
# stage exists to decide whether that default is the right one, and it is a
# decision that cannot be reasoned to:
#
#   * thinning ACKs saves packets, softirq and CPU on both peers — which is the
#     whole bill on the vm-vm control, where the RTT is a loopback-class
#     number and the path never loses anything;
#   * but every ACK is also the loss signal and the RTT sample the congestion
#     controller steers on. Fewer of them means a coarser RTT estimate and a
#     slower reaction to loss, and on a real WAN leg (vm-ws) that is paid in
#     goodput, not saved.
#
# So the two topologies are NOT a repetition: vm-vm prices the saving, vm-ws
# prices the risk, and the knob only becomes a default if it wins the second.
#
# Method is S1's, for the same reason S1 uses it: PAIRED arms with the order
# alternating inside the pair, fixed bytes so the two halves spend the same
# burst allowance, median of the ratios. The only difference is that BOTH arms
# here are `--udp` — the comparison is direct-vs-direct, and an arm that fell
# back to the relay is EXCLUDED, never averaged in (it would compare a relay
# against a direct and call the difference an ACK effect).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/seclib.sh"

TOPO="${TOPO:-vm-vm}"
MB="${MB:-128}"
CONNS="${CONNS:-4}"
PAIRS="${PAIRS:-5}"
# The threshold under test: how many ack-eliciting packets may arrive before
# the receiver must send an ACK. quinn's own default behaviour is "every
# second packet"; 10 is the first value large enough for the saving to be
# visible above run-to-run noise while still sampling the RTT several times
# per round trip at these window sizes.
ACK="${ACK:-10}"
PER=$(( MB * 1048576 / CONNS ))
PP="$SEC_PROXY_PORT"

case "$TOPO" in vm-ws|ws-vm|vm-vm) ;; *) echo "bad TOPO '$TOPO'" >&2; exit 2 ;; esac
case "$ACK" in ''|*[!0-9]*) echo "ACK must be a positive integer (got '$ACK')" >&2; exit 2 ;; esac
[ "$ACK" -gt 0 ] || { echo "ACK must be > 0" >&2; exit 2; }

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

# one_arm <get|put> -> "<MBs> <path> <fallbacks>"
# SEC_ENV is set by the CALLER and is the only thing that differs between the
# two arms; it reaches both peers because seclib applies it on both hosts.
one_arm() {
    local dir="$1"
    local id; id="$(sec_id)"
    PROV_PID=""; CONS_PID=""
    start_provider "$id" --udp || { echo "0 startfail 0"; return 1; }
    sec_wait secretprovider "$id" || { sec_down "$id" ${PROV_PID:-}; echo "0 noprovider 0"; return 1; }
    start_consumer "$id" --udp || { sec_down "$id" ${PROV_PID:-}; echo "0 startfail 0"; return 1; }
    sec_wait secretconsumer "$id" || { sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}; echo "0 noconsumer 0"; return 1; }
    drive get 1048576 1 >/dev/null 2>&1
    sec_time_to_direct "$id" 30 >/dev/null
    local r; r=$(drive "$dir" "$PER" "$CONNS")
    local path fb
    path="$(sec_path "$id")"; fb="$(sec_fallbacks "$id")"
    [ "$path" = relay ] && path="relay(fb=$fb)"
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    echo "${r:-0} $path ${fb:-0}"
}

# Same exclusion rule as sec_ab.sh, and for the same reason: an arm that did
# not measure the direct path must not enter the median.
arm_ok() { # <rate> <path>
    case "$2" in startfail|noprovider|noconsumer|"relay(fb="*) return 1 ;; esac
    case "$1" in ''|0|0.0|0.00) return 1 ;; esac
    return 0
}

paired() { # <dirn>
    local dirn="$1"
    echo
    echo "===== S5 direct: ack threshold $ACK vs shipped default ($dirn, topology $TOPO) ====="
    printf '  %-5s %10s %10s %8s   %s\n' pair default "ack=$ACK" ratio paths
    local rs=() i a b pa pb dropped=0
    for i in $(seq "$PAIRS"); do
        # Each arm runs inside its own command substitution, i.e. its own
        # subshell, so SEC_ENV is set there and cannot leak into the other arm.
        if [ $((i % 2)) = 1 ]; then
            read -r a pa _ <<<"$( SEC_ENV=""; one_arm "$dirn" )"; cool
            read -r b pb _ <<<"$( SEC_ENV="BORE_DIRECT_QUIC_ACK_THRESHOLD=$ACK"; one_arm "$dirn" )"; cool
        else
            read -r b pb _ <<<"$( SEC_ENV="BORE_DIRECT_QUIC_ACK_THRESHOLD=$ACK"; one_arm "$dirn" )"; cool
            read -r a pa _ <<<"$( SEC_ENV=""; one_arm "$dirn" )"; cool
        fi
        local r
        if arm_ok "$a" "$pa" && arm_ok "$b" "$pb"; then
            r=$(ratio "$b" "$a"); rs+=("$r")
        else
            r="-"; dropped=$((dropped+1))
        fi
        printf '  %-5s %10s %10s %8s   %s / %s\n' "$i" "$a" "$b" "$r" "$pa" "$pb"
    done
    if [ "${#rs[@]}" -gt 0 ]; then
        echo "  median ratio ack=$ACK / default: $(printf '%s\n' "${rs[@]}" | med)   (n=${#rs[@]} of $PAIRS pairs)"
    else
        echo "  median ratio ack=$ACK / default: n/a — no pair produced two direct arms"
    fi
    [ "$dropped" -gt 0 ] && echo "  EXCLUDED $dropped of $PAIRS pairs (see rows above)"
    return 0
}

sec_start_origin "$TOPO" || exit 1
say "secret ACK-frequency A/B: topology $TOPO, ${MB} MiB per arm over $CONNS conns, $PAIRS pairs, threshold $ACK"
echo "    both arms are --udp; the ONLY difference is BORE_DIRECT_QUIC_ACK_THRESHOLD"
paired get
paired put
echo
echo DONE
