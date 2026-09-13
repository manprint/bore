#!/usr/bin/env bash
# V-17: is the direct tunnel's residual 5 % framing, or is it loss?
#
# THE QUESTION
# ------------
# Wired, the direct path delivers ~705 Mbit/s of inner TCP against a bare
# ~738 -- about 95.5 % -- and FOUR separate ladders have now failed to move it:
# the TUN device queue, the QUIC datagram send buffer, the UDP socket send
# buffer and the congestion controller are all flat. A deficit that no tunable
# touches is either arithmetic or a defect, and those two have opposite
# consequences: arithmetic is finished work, a defect is a hunt.
#
# A tunnel cannot deliver a payload without also putting headers on the wire, so
# SOME deficit is guaranteed. The question is whether the measured one is that
# and nothing else.
#
# METHOD: SOLVE FOR THE OVERHEAD INSTEAD OF ASSUMING IT
# -----------------------------------------------------
# Counting up the headers from a specification produces a number that agrees
# with whatever was assumed about connection-ID length, packet-number length,
# the AEAD tag and the DATAGRAM frame header. So this stage does the opposite:
# it MEASURES bytes-on-the-wire per delivered byte, and reports the per-packet
# overhead that ratio implies. That number is falsifiable. ~40-60 B is QUIC
# plus AEAD doing their job; 200 B is a finding; and a ratio far above what any
# per-packet overhead can explain is RETRANSMISSION, which is P-13's exact
# signature (it measured 3.885 GiB taken in to deliver 2.279 -- 1.78x).
#
#   bytes on the wire : the NIC's own tx_bytes counter, i.e. the kernel's view
#   bytes delivered   : iperf3's sum_received, i.e. receiver-side truth
#
# Both arms are measured the same way in the same repetition, so anything
# common to them cancels.
#
# THE CAVEAT, STATED RATHER THAN HIDDEN
# --------------------------------------
# A NIC counter counts EVERYTHING on that interface, including this harness's
# own ssh. Over a 15 s transfer at ~700 Mbit/s that is ~1.3 GB against a few
# tens of KB of ssh, i.e. well under 0.01 %, but it is a floor on the precision
# and no conclusion here should turn on a third decimal. The stage prints the
# idle drift it measured just before each arm so the size of that term is a
# number rather than an assurance.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
SECS="${SECS:-15}"
NIC="$(ip route show default | awk '/^default/{print $5; exit}')"

vpn_hdr "VPN framing overhead -- $REPS reps x ${SECS}s, nic $NIC"
echo "  measures wire bytes per delivered byte, and solves for the per-packet"
echo "  overhead that implies. Framing is arithmetic; retransmission is not."
echo

nic_tx()   { cat "/sys/class/net/$NIC/statistics/tx_bytes"; }
nic_txpkt(){ cat "/sys/class/net/$NIC/statistics/tx_packets"; }

# What crosses the wire with nothing running: the precision floor of the method.
idle_drift() {
    local a b
    a=$(nic_tx); sleep 2; b=$(nic_tx)
    LC_ALL=C awk -v d=$((b-a)) 'BEGIN{printf "%.0f", d/2}'
}


declare -A R
add() { case "$2" in ''|0|FAILED) return;; esac; R["$1"]+=" $2"; }

# one_arm <bare|direct> <target-ip> <mtu>
# awk prints the human lines on stdout and one machine-readable RESULT line;
# bash splits them. The earlier shape wrote RESULT to stderr into a process
# substitution, which races with the caller and can drop a sample silently --
# and a measurement harness that can lose a sample without saying so is the one
# thing this campaign refuses.
one_arm() {
    local arm="$1" ip="$2" mtu="$3" t0 p0 t1 p1 mbps drift out
    drift="$(idle_drift)"
    t0=$(nic_tx); p0=$(nic_txpkt)
    mbps="$(tcp_mbps "$ip" "$SECS" 1)"
    t1=$(nic_tx); p1=$(nic_txpkt)
    if [ -z "$mbps" ] || [ "$mbps" = 0 ]; then
        echo "    $arm: FAILED (no throughput)"; return
    fi

    out="$(LC_ALL=C awk -v arm="$arm" -v wire=$((t1-t0)) -v pkts=$((p1-p0)) -v mbps="$mbps" \
                        -v secs="$SECS" -v mtu="$mtu" -v drift="$drift" 'BEGIN{
        deliv = mbps * 1e6 / 8 * secs
        if (deliv <= 0 || pkts <= 0) { printf "    %-7s FAILED (no bytes)\n", arm; exit }
        ratio      = wire / deliv
        bpp        = wire / pkts
        payload_pp = deliv / pkts
        ovh        = bpp - payload_pp
        printf "    %-7s %7.2f Mbit/s   wire/deliv %.4f   %6.0f B/frame  payload %6.0f  => overhead %5.0f B\n", \
               arm, mbps, ratio, bpp, payload_pp, ovh
        printf "    %-7s (mtu %s, %d frames, idle drift %.0f B/s)\n", "", mtu, pkts, drift
        printf "RESULT %.6f %.2f\n", ratio, ovh
    }')"

    printf '%s\n' "$out" | grep -v '^RESULT '
    local r o
    r=$(printf '%s\n' "$out" | awk '/^RESULT /{print $2}')
    o=$(printf '%s\n' "$out" | awk '/^RESULT /{print $3}')
    add "$arm|ratio" "$r"
    add "$arm|ovh"   "$o"
}

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    [ "$(vm_iperf_server)" = 1 ] || { echo "    no iperf3 server on the VM"; continue; }

    # bare first, then the tunnel, in the same repetition.
    one_arm bare "$BORE_VM" 1500
    sleep 2

    VPN_LINK_ID="${VPN_RUN_ID}ov$(date +%s%N | tail -c 5)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()
    vm_up listen
    sleep 3
    ws_up ov connect
    if ! ws_ready ov 45 >/dev/null; then
        echo "    direct: link never came up"; vpn_cleanup; sleep 2; continue
    fi
    if [ "$(wait_path ov direct 75)" != direct ]; then
        echo "    direct: never reached direct -- a relay number here would be mislabelled"
        vpn_cleanup; sleep 2; continue
    fi
    m="$(wait_mtu_settle ov 24 90)"
    [ "$(vm_iperf_server)" = 1 ] && one_arm direct "$B_PEER" "$m"
    vpn_cleanup; sleep 3
done

echo
echo "=== medians ==="
m() { printf '%s\n' ${R["$1"]:-} | med; }
printf '  %-8s wire/delivered %-10s implied per-frame overhead %s B\n' bare   "$(m 'bare|ratio')"   "$(m 'bare|ovh')"
printf '  %-8s wire/delivered %-10s implied per-frame overhead %s B\n' direct "$(m 'direct|ratio')" "$(m 'direct|ovh')"

echo
echo "=== raw samples ==="
for k in "${!R[@]}"; do printf '  %-14s%s\n' "$k" "${R[$k]}"; done | sort

echo
echo "=== reading ==="
LC_ALL=C awk -v br="$(m 'bare|ratio')" -v dr="$(m 'direct|ratio')" \
             -v bo="$(m 'bare|ovh')"   -v do_="$(m 'direct|ovh')" 'BEGIN{
  n="^[0-9.]+$"
  if (!(br ~ n) || !(dr ~ n)) { print "  one arm produced no ratio -- nothing to conclude"; exit }
  printf "  bare puts %.2f%% more on the wire than it delivers; direct puts %.2f%%.\n", 100*(br-1), 100*(dr-1)
  eff = br/dr
  printf "  framing alone therefore predicts the tunnel reaches %.1f%% of bare.\n", 100*eff
  print  "  Compare that with the measured direct/bare throughput ratio from vpn_ab"
  print  "  and vpn_txqueue (~0.95). If they agree, the residual deficit IS the"
  print  "  headers and no tunable can remove it."
  if (do_ ~ n) {
      printf "  Direct carries %.0f B of per-frame overhead against bare %.0f B.\n", do_, bo
      d = do_ - bo
      printf "  The tunnel adds %.0f B per frame.\n", d
      if (d > 120)
          print "  => ABOVE what QUIC + AEAD + UDP/IP can account for (~40-60 B). Either the\n     frames are smaller than the MTU suggests, or bytes are being RETRANSMITTED.\n     That is the P-13 signature and it is a defect, not arithmetic."
      else if (d > 0)
          print "  => consistent with QUIC + AEAD + the outer UDP/IP header. Arithmetic."
      else
          print "  => negative, which is not physical: suspect the NIC counter window."
  }
}'
echo
echo "DONE"
