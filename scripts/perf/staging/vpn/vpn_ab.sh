#!/usr/bin/env bash
# V1: the core VPN transport comparison over a REAL path -- TCP relay vs
# hole-punched QUIC direct -- run PAIRED, in both directions.
#
# METHOD, and why each part of it is there
# ----------------------------------------
# PAIRED arms. This workstation drifts 14% on identical code (measured in the
# transfer campaign), and the VM is a burstable instance whose network
# allowance is the dominant confounder of every previous staging campaign. Two
# whole sweeps therefore never compare two transports: the arms alternate
# inside each repetition and the quoted figure is the MEDIAN RATIO, in which
# drift common to both arms cancels.
#
# The relay arm is `--relay-only`, not "measure before the upgrade lands". The
# upgrade completes in ~50 ms on this path (measured), so there is no usable
# relay window on an ordinary link; asking for the relay explicitly is the only
# way to measure it honestly.
#
# The direct arm is WAITED FOR and VERIFIED. A link always starts on the relay,
# so an arm that never upgraded would otherwise report relay throughput under
# the label "direct" -- the single mistake this campaign exists to avoid. An
# arm whose path does not match its label is printed as FAILED and excluded.
#
# BOTH DIRECTIONS. Download (VM -> workstation) and upload (workstation -> VM)
# are different measurements on an asymmetric home link, and the NAT is on the
# receiving side in one of them and the sending side in the other.
#
# Usage: [REPS=5] [SECS=10] [PAR=1] [TOPO=ws-vm] vpn_ab.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-5}"
SECS="${SECS:-10}"
PAR="${PAR:-1}"

vpn_hdr "VPN relay vs direct, real path, ${REPS} reps x ${SECS}s, parallel=${PAR}"
echo "  workstation <-> $BORE_VM via $BORE_TO, MTU auto-tuned (start ${BENCH_MTU}), settled value reported per arm"
echo "  every arm's path is read back from the endpoint log and verified"
echo

# bring_up <relay|direct> -- returns 0 with the link on the requested path.
# The VM is always the listener: it is the stable end, and it makes the
# workstation the dialer, which is the side whose home NAT is the interesting
# one for traversal.
bring_up() {
    local want="$1" extra=""
    [ "$want" = relay ] && extra="--relay-only"
    VPN_LINK_ID="${VPN_RUN_ID}$(date +%s%N | tail -c 6)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()
    vm_up listen $extra
    sleep 3
    ws_up ab connect $extra
    ws_ready ab 45 >/dev/null || { echo "  link did not come up"; return 1; }
    if [ "$want" = direct ]; then
        local got; got="$(wait_path ab direct 75)"
        [ "$got" = direct ] || { echo "  link stayed on $got, wanted direct"; return 1; }
    fi
    # Wait for the MTU to stop moving before any byte is measured. The direct
    # path climbs from its conservative start to the path's real MTU over the
    # first ~15 s; measuring across that climb blends two different MSS values
    # into one number. The relay has no PMTU discovery and settles immediately,
    # so this costs it only the quiet window.
    ARM_MTU="$(wait_mtu_settle ab 24 90)"
    return 0
}

tear_down() { vpn_cleanup; sleep 2; }

# one_arm <relay|direct> -> prints "down_mbps up_mbps" or "FAILED"
one_arm() {
    local want="$1" down up
    bring_up "$want" || { echo "FAILED(link)"; return; }
    # A server that never bound would make every arm read 0 Mbit/s, and 0 looks
    # like a tunnel that cannot carry traffic rather than a harness that never
    # started a receiver. Check it, and say which of the two it was.
    if [ "$(vm_iperf_server)" != 1 ]; then tear_down; echo "FAILED(no iperf3 server)"; return; fi
    # -R makes the VM send: that is the download direction for this host.
    down="$(tcp_mbps "$B_PEER" "$SECS" "$PAR" -R)"
    sleep 2
    up="$(tcp_mbps "$B_PEER" "$SECS" "$PAR")"
    local path; path="$(ws_path ab)"
    tear_down
    if [ "$path" != "$want" ]; then echo "FAILED(path=$path)"; return; fi
    echo "$down $up $ARM_MTU"
}

# The bare path, sampled in every repetition. It is the only reference that
# says whether a tunnel number is a tunnel limit or the path's own limit, and
# sampling it per repetition rather than once means it tracks the same drift
# the arms are exposed to.
BD=(); BU=()
bare_arm() {
    [ "$(vm_iperf_server)" = 1 ] || { echo "  bare: no iperf3 server"; return; }
    local d u
    d="$(tcp_mbps "$BORE_VM" "$SECS" "$PAR" -R)"; sleep 1
    u="$(tcp_mbps "$BORE_VM" "$SECS" "$PAR")"
    BD+=("$d"); BU+=("$u")
    echo "  rep $1 bare  : $d $u"
}

RD=(); RU=(); DD=(); DU=()
for r in $(seq 1 "$REPS"); do
    # Alternate which arm goes first so a slow warm-up or a spent allowance
    # cannot land on the same arm every time.
    if [ $(( r % 2 )) -eq 1 ]; then order="relay direct"; else order="direct relay"; fi
    bare_arm "$r"
    for arm in $order; do
        res="$(one_arm "$arm")"
        echo "  rep $r $arm: $res"
        case "$res" in
            FAILED*) continue ;;
        esac
        d="$(echo "$res" | awk '{print $1}')"; u="$(echo "$res" | awk '{print $2}')"
        if [ "$arm" = relay ]; then RD+=("$d"); RU+=("$u"); else DD+=("$d"); DU+=("$u"); fi
    done
done

echo
med2() { printf '%s\n' "$@" | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0){print "n/a"} else print a[int((NR+1)/2)] }'; }
rm_=$(med2 "${RD[@]:-}"); dm=$(med2 "${DD[@]:-}")
ru=$(med2 "${RU[@]:-}"); du=$(med2 "${DU[@]:-}")
bd=$(med2 "${BD[@]:-}"); bu=$(med2 "${BU[@]:-}")
echo "  download (VM -> workstation)  bare ${bd}   relay ${rm_}   direct ${dm} Mbit/s"
echo "  upload   (workstation -> VM)  bare ${bu}   relay ${ru}   direct ${du} Mbit/s"
awk -v bd="$bd" -v bu="$bu" -v rd="$rm_" -v dd="$dm" -v ru="$ru" -v du="$du" 'BEGIN{
  if (bd+0>0) printf "  vs bare, download: relay %.1f%%  direct %.1f%%\n", 100*rd/bd, 100*dd/bd;
  if (bu+0>0) printf "  vs bare, upload  : relay %.1f%%  direct %.1f%%\n", 100*ru/bu, 100*du/bu;
}' 
awk -v a="$rm_" -v b="$dm" -v c="$ru" -v d="$du" 'BEGIN{
  if (a+0>0 && b+0>0) printf "  direct/relay download %.3f\n", b/a;
  if (c+0>0 && d+0>0) printf "  direct/relay upload   %.3f\n", d/c;
}'
echo
echo "DONE"
