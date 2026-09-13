#!/usr/bin/env bash
# V-18: the relay path has no datagram limit -- so why is its TUN 1350?
#
# THE QUESTION
# ------------
# The shipped TUN MTU is 1350 and the reason is the DIRECT path: an inner packet
# becomes one QUIC datagram, so it must fit inside the path's max_datagram_size
# (measured 1414 here) or it is TooLarge-dropped. The RELAY path has no such
# limit at all. It is a reliable, ordered TCP byte stream carrying
# `[u32 len][AEAD frame]`, and the outer TCP segments whatever it is given.
#
# So on a relay-bound link -- a UDP-hostile network, `--relay-only`, a failed
# upgrade -- the 1350 buys nothing and costs on every packet:
#
#   * per-packet COST: one AEAD seal, one open, one channel hop, one frame
#     header, one TUN write entry. At 1350 that is ~52 000 of each per second
#     per 570 Mbit/s; at 8000 it is ~8 800.
#   * WIRE efficiency: headers are amortised over a payload 5.9x larger.
#
# This stage decides whether that reasoning is worth anything, because reasoning
# about per-packet cost has been wrong here before: four tunable ladders in this
# campaign came back FLAT, and V-10's ladder produced an optimum on WiFi that
# does not exist on the wire.
#
# METHOD
# ------
# `--relay-only` throughout, so no arm can silently become a direct measurement.
# One link per rung -- unavoidable, because the MTU is fixed when the link is
# built, and it is the variable. The rungs are therefore INTERLEAVED inside each
# repetition and a BARE control is sampled in every repetition (V-9), so drift
# is common to all rungs and cancels in the ratio.
#
# The MTU is read back FROM THE KERNEL for each rung (P-12: the flag proves what
# was asked for, `ip link` proves what the packets got), and the TUN's own
# packet/byte counters are read as a delta across each arm, so "fewer, bigger
# packets" is measured rather than assumed.
#
# Usage: [REPS=3] [SECS=10] [RUNGS="1350 1500 4000 8000"] vpn_relay_mtu.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
SECS="${SECS:-10}"
PAR="${PAR:-1}"
RUNGS="${RUNGS:-1350 1500 4000 8000}"

vpn_hdr "VPN relay TUN-MTU ladder (relay-only) -- rungs: $RUNGS"
echo "  the relay is a reliable byte stream: nothing here can be TooLarge-dropped."
echo "  both ends take the same --mtu, so the ladder is symmetric by construction."
echo

trap vpn_cleanup EXIT

# The TUN's own counters, from the kernel. Returns "rx_bytes rx_pkts tx_bytes tx_pkts".
tun_counters() {
    local name; name="$(sudo -n "$ROOTSH" addr "$1" 2>/dev/null | awk '{print $1}')"
    [ -n "$name" ] || { echo "0 0 0 0"; return; }
    ip -s -j link show dev "$name" 2>/dev/null \
      | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)[0]["stats64"]
    print(d["rx"]["bytes"], d["rx"]["packets"], d["tx"]["bytes"], d["tx"]["packets"])
except Exception:
    print("0 0 0 0")' 2>/dev/null || echo "0 0 0 0"
}

declare -A D U M P   # down, up, measured mtu, bytes-per-packet samples

one_rung() { # <mtu> -> "down up kernel_mtu bytes_per_tx_pkt"
    local mtu="$1"
    VPN_LINK_ID="${VPN_RUN_ID}$(date +%s%N | tail -c 6)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()
    # Set it as a plain variable, never as a prefix assignment on a FUNCTION
    # call: bash does not reliably scope those to the call, and a value that
    # leaked (or failed to arrive) would make the rung a measurement of a
    # different MTU than the one it prints.
    BENCH_MTU="$mtu"
    vm_up listen --relay-only
    sleep 3
    ws_up rmtu connect --relay-only
    if ! ws_ready rmtu 45 >/dev/null; then vpn_cleanup; echo "FAILED(link)"; return; fi
    # The relay does no PMTU discovery, so the MTU is final the moment the link
    # is up -- but it is READ rather than assumed, because a kernel that refused
    # the value would otherwise be reported as a measurement of it.
    local kmtu; kmtu="$(ws_mtu rmtu)"
    if [ "${kmtu:-0}" != "$mtu" ]; then
        vpn_cleanup; echo "FAILED(kernel mtu=$kmtu wanted $mtu)"; return
    fi
    if [ "$(vm_iperf_server)" != 1 ]; then vpn_cleanup; echo "FAILED(no iperf3 server)"; return; fi

    local c0 c1 down up
    c0="$(tun_counters rmtu)"
    down="$(tcp_mbps "$B_PEER" "$SECS" "$PAR" -R)"
    sleep 2
    up="$(tcp_mbps "$B_PEER" "$SECS" "$PAR")"
    c1="$(tun_counters rmtu)"
    local path; path="$(ws_path rmtu)"
    vpn_cleanup
    if [ "$path" != relay ]; then echo "FAILED(path=$path)"; return; fi
    # Bytes per transmitted packet: the whole point of a bigger MTU, and the one
    # number that says the ladder did what it claims rather than merely reading
    # differently.
    local bpp
    bpp="$(LC_ALL=C awk -v a="$c0" -v b="$c1" 'BEGIN{
        split(a,x," "); split(b,y," ")
        dp = y[4]-x[4]; db = y[3]-x[3]
        if (dp > 0) printf "%d", db/dp; else printf "n/a" }')"
    echo "$down $up $kmtu $bpp"
}

BD=(); BU=()
for r in $(seq 1 "$REPS"); do
    echo "  --- rep $r"
    if [ "$(vm_iperf_server)" = 1 ]; then
        d="$(tcp_mbps "$BORE_VM" "$SECS" "$PAR" -R)"; sleep 1
        u="$(tcp_mbps "$BORE_VM" "$SECS" "$PAR")"
        BD+=("$d"); BU+=("$u")
        printf '    %-6s bare   down=%-8s up=%-8s\n' "" "$d" "$u"
    fi
    # Reverse the rung order on alternate repetitions: a warm-up effect or a
    # spent allowance must not land on the same rung every time.
    order="$RUNGS"
    [ $((r % 2)) -eq 0 ] && order="$(printf '%s\n' $RUNGS | tac | tr '\n' ' ')"
    for mtu in $order; do
        res="$(one_rung "$mtu")"
        printf '    mtu %-5s %s\n' "$mtu" "$res"
        case "$res" in FAILED*) continue ;; esac
        D["$mtu"]+=" $(echo "$res" | awk '{print $1}')"
        U["$mtu"]+=" $(echo "$res" | awk '{print $2}')"
        M["$mtu"]="$(echo "$res" | awk '{print $3}')"
        P["$mtu"]+=" $(echo "$res" | awk '{print $4}')"
        sleep 3
    done
done

echo
echo "=== medians (Mbit/s), relay-only ==="
bare_d="n/a"; bare_u="n/a"
[ ${#BD[@]} -gt 0 ] && bare_d="$(printf '%s\n' "${BD[@]}" | med)"
[ ${#BU[@]} -gt 0 ] && bare_u="$(printf '%s\n' "${BU[@]}" | med)"
printf '  %-8s %-10s %-10s %-10s %-10s %-10s\n' mtu down up 'down/bare' 'up/bare' 'bytes/pkt'
for mtu in $RUNGS; do
    d="$(printf '%s\n' ${D[$mtu]:-} | med)"
    u="$(printf '%s\n' ${U[$mtu]:-} | med)"
    b="$(printf '%s\n' ${P[$mtu]:-} | med)"
    LC_ALL=C awk -v m="$mtu" -v d="${d:-n/a}" -v u="${u:-n/a}" -v b="${b:-n/a}" \
                 -v bd="${bare_d:-0}" -v bu="${bare_u:-0}" 'BEGIN{
        rd = (bd+0 > 0 && d+0 > 0) ? sprintf("%.3f", d/bd) : "n/a"
        ru = (bu+0 > 0 && u+0 > 0) ? sprintf("%.3f", u/bu) : "n/a"
        printf "  %-8s %-10s %-10s %-10s %-10s %-10s\n", m, d, u, rd, ru, b }'
done
printf '  %-8s %-10s %-10s\n' bare "${bare_d:-n/a}" "${bare_u:-n/a}"

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for mtu in $RUNGS; do
    printf '  mtu %-5s down:%s\n' "$mtu" "${D[$mtu]:-  (none)}"
    printf '  mtu %-5s up  :%s\n' "$mtu" "${U[$mtu]:-  (none)}"
    printf '  mtu %-5s b/pkt:%s (kernel mtu %s)\n' "$mtu" "${P[$mtu]:-  (none)}" "${M[$mtu]:-n/a}"
done
printf '  bare      down:%s\n' "${BD[*]:-  (none)}"
printf '  bare      up  :%s\n' "${BU[*]:-  (none)}"

echo
echo "=== reading ==="
echo "  A FLAT ladder means the relay is not paying for its packet rate, and the"
echo "  shipped 1350 is free -- the fifth flat ladder of this campaign, and the"
echo "  answer stays 'the ceiling is elsewhere' (see vpn_relay_attrib)."
echo "  A RISING ladder means a relay-bound link is leaving throughput on the"
echo "  table for a limit that only the direct path has. The change it argues"
echo "  for is NOT 'raise the default': it is 'raise it while on relay and shrink"
echo "  before switching to direct', which the PMTU monitor already does in ~1 ms"
echo "  (measured: switch at .983622, MTU adjusted at .984358)."
echo
echo "DONE"
