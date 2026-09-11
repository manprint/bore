#!/usr/bin/env bash
# P1-P4: the core public-tunnel transport comparison, run from the test VM.
#
# Design notes that matter more than the numbers:
#
#  * PAIRED, not independent. The vhost campaign measured 29 % drift in the
#    control arm over a few minutes, so an A-then-B comparison of two
#    independently-collected medians says nothing. Each pair measures both
#    transports back to back and reports the RATIO; drift is common to both
#    halves and cancels. The order alternates within the pair so a systematic
#    first-versus-second effect cancels across pairs. Quote the MEDIAN RATIO.
#
#  * FIXED BYTES, not fixed time. Both halves of a pair must spend the same
#    amount of the instance's burst allowance, or the second half is measured
#    against a budget the first half drained.
#
#  * RAW TCP, not HTTP. A public tunnel forwards arbitrary TCP; measuring it
#    through an HTTP origin folds HTTP parsing into the result. The HTTP arm is
#    kept as a separate, clearly-labelled section so the two are comparable
#    with the vhost campaign without being confused with each other.
#
#  * The direct path is CONFIRMED, never assumed: `current_path` and
#    `direct_stream_opens` are read from /admin/api/v1/tunnels around every
#    measurement, and an arm that asked for --udp but ran on the relay is
#    reported as such instead of being quoted as a QUIC number.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

MB="${MB:-128}"              # MiB moved per measurement, across all conns
CONNS="${CONNS:-4}"
PAIRS="${PAIRS:-5}"
PER=$(( MB * 1048576 / CONNS ))

start_origins || exit 1
say "public A/B: ${MB} MiB per arm over $CONNS conns, $PAIRS pairs, ${COOL}s cooldown"
echo "    origin: raw TCP 127.0.0.1:$RP (no HTTP), also HTTP 127.0.0.1:$OP"
echo "    public tunnel port assigned by the server from its configured range"

# one_arm <dir:get|put> <flags...> -> "<MBs> <path> <opens> <fallbacks>"
one_arm() {
    local dir="$1"; shift
    if ! up_native "$RP" 0 "$@"; then echo "FAIL relay 0 0"; return 1; fi
    local p="$LASTPORT" pid="$LASTPID"
    # Warm the tunnel: the FIRST proxied connection on a --udp tunnel is what
    # triggers the direct open, and including that open in the measurement
    # would charge QUIC for a handshake the relay never pays mid-stream.
    python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
    local o0 r o1 fb path
    o0=$(tfld "$p" direct_stream_opens); o0=${o0:-0}
    if [ "$dir" = get ]; then r=$(raw_get "$p" "$PER" "$CONNS"); else r=$(raw_put "$p" "$PER" "$CONNS"); fi
    o1=$(tfld "$p" direct_stream_opens); o1=${o1:-0}
    fb=$(tfld "$p" direct_fallbacks); fb=${fb:-0}
    path=$(tfld "$p" current_path); path=${path:-unknown}
    down "$pid" "$p"
    echo "${r:-0} $path $((o1 - o0)) $fb"
}

paired() { # <title> <dirn> <A-tag> <A-flags> <B-tag> <B-flags>
    local title="$1" dirn="$2" ta="$3" fa="$4" tb="$5" fb="$6"
    echo
    echo "===== $title ($dirn) ====="
    printf '  %-5s %10s %10s %8s   %s\n' pair "$ta" "$tb" "ratio" "paths"
    local rs=() i a b pa pb oa ob
    for i in $(seq "$PAIRS"); do
        if [ $((i % 2)) = 1 ]; then
            read -r a pa oa _ <<<"$(one_arm "$dirn" $fa)"; cool
            read -r b pb ob _ <<<"$(one_arm "$dirn" $fb)"; cool
        else
            read -r b pb ob _ <<<"$(one_arm "$dirn" $fb)"; cool
            read -r a pa oa _ <<<"$(one_arm "$dirn" $fa)"; cool
        fi
        local r; r=$(ratio "$a" "$b"); rs+=("$r")
        printf '  %-5s %10s %10s %8s   %s(o=%s) %s(o=%s)\n' "$i" "$a" "$b" "$r" "$pa" "$oa" "$pb" "$ob"
    done
    echo "  median ratio $ta/$tb: $(printf '%s\n' "${rs[@]}" | med)"
}

case "${1:-all}" in
  all|p1)
    paired "P1 relay TCP vs QUIC direct" get "relay" "--carriers 1" "quic" "--carriers 1 --udp"
    paired "P1 relay TCP vs QUIC direct" put "relay" "--carriers 1" "quic" "--carriers 1 --udp"
    ;;&
  all|p2)
    echo
    echo "===== P2 carrier ladder on the TCP relay ====="
    echo "  A public tunnel's relay carriers are the same mechanism as vhost's;"
    echo "  measured here because the public data path opens a substream per"
    echo "  INBOUND connection, so head-of-line behaviour differs from vhost's."
    for c in 1 2 4 8; do
        read -r r p o _ <<<"$(one_arm get --carriers "$c")"
        printf '  carriers=%-2s dl %8s MB/s  path=%s opens=%s\n' "$c" "$r" "$p" "$o"
        cool
    done
    for c in 1 2 4 8; do
        read -r r p o _ <<<"$(one_arm put --carriers "$c")"
        printf '  carriers=%-2s up %8s MB/s  path=%s opens=%s\n' "$c" "$r" "$p" "$o"
        cool
    done
    ;;&
  all|p3)
    echo
    echo "===== P3 latency through the public tunnel ====="
    echo "  Raw TCP: ONE NEW CONNECTION PER PROBE, serially. That is what a"
    echo "  real client pays and what the tunnel's per-connection open costs;"
    echo "  a concurrent probe would measure queueing instead."
    for mode in relay quic; do
        flags="--carriers 1"; [ "$mode" = quic ] && flags="--carriers 1 --udp"
        if up_native "$RP" 0 $flags; then
            p="$LASTPORT"; pid="$LASTPID"
            python3 "$RAWCLI" get "$GW" "$p" 1048576 1 >/dev/null 2>&1
            echo "  $mode raw new-conn x100 : $(raw_ping "$p" 100)"
            echo "  $mode path=$(tfld "$p" current_path) opens=$(tfld "$p" direct_stream_opens) pool=$(tfld "$p" direct_pool)"
            down "$pid" "$p"
        else echo "  $mode REGISTRATION FAILED"; fi
        cool 20
    done
    ;;&
  all|p4)
    echo
    echo "===== P4 HTTP through the public tunnel (comparable with the vhost campaign) ====="
    echo "  NOTE the public tunnel port carries PLAIN HTTP: there is no TLS"
    echo "  termination on it unless the tunnel asked for --https, so this is"
    echo "  cheaper than the vhost numbers by exactly one TLS session."
    for mode in relay quic; do
        flags="--carriers 1"; [ "$mode" = quic ] && flags="--carriers 1 --udp"
        if up_native "$OP" 0 $flags; then
            p="$LASTPORT"; pid="$LASTPID"
            curl -fsS -o /dev/null -m 20 "http://$GW:$p/100k" 2>/dev/null
            for c in 1 8 32; do
                echo "  $mode 1k c=$c : $(timeout 30 "$OHA" -z 6s -c "$c" --no-tui --output-format json \
                  "http://$GW:$p/1k" 2>/dev/null | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95) p99=\(.metrics.latency_ms.p99) ok=\(.summary.successRate)"')"
            done
            echo "  $mode newconn c=8 : $(timeout 30 "$OHA" -z 6s -c 8 --no-tui --disable-keepalive --output-format json \
              "http://$GW:$p/1k" 2>/dev/null | jq -r '"rps=\((.summary.requestsPerSec|round)) p50=\(.metrics.latency_ms.p50) p95=\(.metrics.latency_ms.p95)"')"
            echo "  $mode dl 4x32MiB : $(http_get "$p" "/stream/$((128*1048576))" 60) MB/s (single stream)"
            echo "  $mode path=$(tfld "$p" current_path) opens=$(tfld "$p" direct_stream_opens) fb=$(tfld "$p" direct_fallbacks)"
            down "$pid" "$p"
        else echo "  $mode REGISTRATION FAILED"; fi
        cool
    done
    ;;
esac
echo
echo DONE
