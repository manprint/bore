#!/usr/bin/env bash
# S1: the core secret-tunnel transport comparison — TCP relay vs hole-punched
# QUIC direct — run PAIRED across three topologies.
#
# Run from the WORKSTATION: a secret tunnel has two clients and only the
# workstation can reach both hosts. The traffic driver always runs on the
# CONSUMER's host, against the consumer's own `--local-proxy-port` on
# loopback, because that is where a real user's application sits.
#
# Topologies (TOPO=):
#   vm-ws   provider on the test VM, consumer on this workstation   (download
#           from a same-region provider to a home NAT — the common shape)
#   ws-vm   provider on this workstation, consumer on the test VM   (upload
#           out of a home NAT; the asymmetric twin, and the one where the
#           workstation's NAT is on the RECEIVING side of the punch)
#   vm-vm   both on the test VM (one host, two processes): no WAN between the
#           peers, so the direct arm isolates the CPU and stack cost of QUIC
#           from every network effect. It is a CONTROL, not a user scenario.
#
# Method, unchanged from the public campaign because the confounder is the
# same instance: PAIRED arms (drift cancels in the ratio), FIXED BYTES (both
# halves spend the same burst allowance), order alternating within the pair,
# quote the MEDIAN RATIO. What IS new: every arm's path is read from the
# CONSUMER's admin row and printed, and a --udp arm that fell back to the
# relay is labelled `relay(fb=N)` instead of being averaged into a "direct"
# median. A direct number that was silently a relay number is the one mistake
# this whole campaign exists to avoid.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/seclib.sh"

TOPO="${TOPO:-vm-ws}"
MB="${MB:-128}"
CONNS="${CONNS:-4}"
PAIRS="${PAIRS:-5}"
PER=$(( MB * 1048576 / CONNS ))
PP="$SEC_PROXY_PORT"

case "$TOPO" in
  vm-ws|ws-vm|vm-vm) ;;
  *) echo "TOPO must be vm-ws, ws-vm or vm-vm (got '$TOPO')" >&2; exit 2 ;;
esac

# --- per-topology role placement -------------------------------------------
# The provider always serves the RAW origin on its own loopback; the consumer
# always publishes on its own loopback. Only WHERE each runs changes.
start_provider() { # <secret-id> <flags...>
    local id="$1"; shift
    case "$TOPO" in
        vm-ws|vm-vm) vm_provider "$RP" "$id" "$@" ;;
        ws-vm)       ws_provider "$RP" "$id" "$@"; PROV_PID="$LASTPID" ;;
    esac
}
start_consumer() { # <secret-id> <flags...>
    local id="$1"; shift
    case "$TOPO" in
        vm-ws)       ws_consumer "$PP" "$id" "$@"; CONS_PID="$LASTPID" ;;
        ws-vm|vm-vm) vm_consumer "$PP" "$id" "$@" ;;
    esac
}
# drive <get|put> <bytes-per-conn> <conns> -> MB/s
drive() {
    local dir="$1" per="$2" conns="$3"
    case "$TOPO" in
        vm-ws)
            python3 "$WS_RAWCLI" "$dir" 127.0.0.1 "$PP" "$per" "$conns" 2>/dev/null \
                | grep -oE 'MBs=[0-9.]+' | cut -d= -f2 ;;
        ws-vm|vm-vm)
            vm "python3 $VM_RAWCLI $dir 127.0.0.1 $PP $per $conns" 2>/dev/null \
                | grep -oE 'MBs=[0-9.]+' | cut -d= -f2 ;;
    esac
}

# The provider's origin. On the VM it is the campaign's own raw_origin.py,
# started idempotently; on the workstation the same file from this repository.

# one_arm <get|put> <flags...> -> "<MBs> <path> <fallbacks> <ttd_ms>"
#
# The warm-up connection is NOT optional and is not a courtesy to QUIC: the
# direct path is negotiated on the FIRST proxied connection, so a measurement
# that includes it charges the punch to the transfer. `ttd` is measured
# SEPARATELY and reported beside the rate, which is the honest way to present
# a cost that a real user does pay once.
one_arm() {
    local dir="$1"; shift
    local id; id="$(sec_id)"
    PROV_PID=""; CONS_PID=""
    start_provider "$id" "$@" || { echo "0 startfail 0 never"; return 1; }
    if ! sec_wait secretprovider "$id"; then
        sec_down "$id" ${PROV_PID:-}; echo "0 noprovider 0 never"; return 1
    fi
    start_consumer "$id" "$@" || { sec_down "$id" ${PROV_PID:-}; echo "0 startfail 0 never"; return 1; }
    if ! sec_wait secretconsumer "$id"; then
        sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}; echo "0 noconsumer 0 never"; return 1
    fi

    # Warm-up: 1 MiB, one connection. Enough to trigger the direct open and
    # far too little to matter for the allowance budget.
    drive get 1048576 1 >/dev/null 2>&1
    local ttd="n/a"
    case " $* " in *" --udp "*) ttd="$(sec_time_to_direct "$id" 30)" ;; esac

    local r; r=$(drive "$dir" "$PER" "$CONNS")
    local path fb
    path="$(sec_path "$id")"; fb="$(sec_fallbacks "$id")"
    # A --udp arm that is on the relay is NOT a direct measurement. Say so in
    # the label so no downstream median can quietly merge the two.
    case " $* " in
        *" --udp "*) [ "$path" = relay ] && path="relay(fb=$fb)" ;;
    esac
    sec_down "$id" ${PROV_PID:-} ${CONS_PID:-}
    echo "${r:-0} $path ${fb:-0} $ttd"
}

# A pair enters the median only when BOTH of its arms are real measurements,
# and there are exactly two ways an arm is not one:
#
#   * it never ran        -> one_arm printed rate 0 with path startfail /
#                            noprovider / noconsumer;
#   * the --udp arm is on the RELAY -> path relay(fb=N). That arm measures the
#                            relay twice, so its ratio sits near 1.0 and
#                            folding it in reports a TRAVERSAL failure as a
#                            performance result. The fallback RATE is S2's
#                            job (sec_ttd.sh), never S1's median.
#
# This is not hypothetical: s1-vm-ws `get` reported a median of 0.965 with two
# startfail rows, because a failed arm entered the list as the ratio 0 and
# dragged the median down to the smallest of the three arms that actually ran
# (0.965 / 1.247 / 1.218, true median 1.218). The rows stay printed either way
# — an excluded pair must remain VISIBLE, or the exclusion becomes the new way
# to hide a failure.
arm_is_measurement() { # <rate> <path> -> 0 when the arm measured something
    case "$2" in startfail|noprovider|noconsumer|"relay(fb="*) return 1 ;; esac
    case "$1" in ''|0|0.0|0.00) return 1 ;; esac
    return 0
}

paired() { # <title> <dirn>
    local title="$1" dirn="$2"
    echo
    echo "===== $title ($dirn, topology $TOPO) ====="
    printf '  %-5s %10s %10s %8s   %s\n' pair relay direct ratio "paths (ttd ms)"
    local rs=() i a b pa pb ta tb dropped=0
    for i in $(seq "$PAIRS"); do
        if [ $((i % 2)) = 1 ]; then
            read -r a pa _ ta <<<"$(one_arm "$dirn")"; cool
            read -r b pb _ tb <<<"$(one_arm "$dirn" --udp)"; cool
        else
            read -r b pb _ tb <<<"$(one_arm "$dirn" --udp)"; cool
            read -r a pa _ ta <<<"$(one_arm "$dirn")"; cool
        fi
        local r
        if arm_is_measurement "$a" "$pa" && arm_is_measurement "$b" "$pb"; then
            r=$(ratio "$b" "$a"); rs+=("$r")
        else
            r="-"; dropped=$((dropped+1))
        fi
        printf '  %-5s %10s %10s %8s   %s / %s (%s)\n' "$i" "$a" "$b" "$r" "$pa" "$pb" "$tb"
    done
    if [ "${#rs[@]}" -gt 0 ]; then
        echo "  median ratio direct/relay: $(printf '%s\n' "${rs[@]}" | med)   (n=${#rs[@]} of $PAIRS pairs)"
    else
        echo "  median ratio direct/relay: n/a — no pair produced two valid arms"
    fi
    if [ "$dropped" -gt 0 ]; then
        echo "  EXCLUDED $dropped of $PAIRS pairs: an arm failed to start, or the --udp arm stayed on the relay (see the rows above)"
    fi
}

sec_start_origin "$TOPO" || exit 1
say "secret A/B: topology $TOPO, ${MB} MiB per arm over $CONNS conns, $PAIRS pairs, ${COOL}s cooldown"
echo "    provider origin: raw TCP 127.0.0.1:$RP   consumer proxy: 127.0.0.1:$PP"
paired "S1 relay vs QUIC direct" get
paired "S1 relay vs QUIC direct" put
echo
echo DONE
