#!/usr/bin/env bash
# V15: the SECOND deficit -- the tunnel's own wire ceiling.
#
# THE QUESTION THIS STAGE EXISTS FOR
# ----------------------------------
# After the TUN queue was bounded (V-10), the direct path carries ~375 Mbit/s of
# wire traffic while bare UDP on the same 5-tuple, at the same minute, carries
# 537-547. That gap is not the one V-10 closed, it is not CPU (12 % of one core,
# busiest thread 3.1 %) and it is not loss on the link (`lost_pct=0.00`).
# Per-packet overhead accounts for about 3 %, not 30 %.
#
# Every measurement of it so far has been taken through an inner TCP flow, which
# is the wrong instrument for this question: inner TCP reacts to the very
# quantity being measured, so a ceiling and a congestion response are
# indistinguishable from its throughput. This stage drives the tunnel with UDP
# at a FIXED OFFERED RATE instead. A pipe with a hard ceiling delivers the
# ceiling and loses the rest; a pipe that is merely being congestion-controlled
# delivers what it is offered until the offered rate exceeds what the path can
# do. Those two look identical to TCP and completely different here.
#
# THE CONTROL IS THE SAME LADDER RUN BARE
# ---------------------------------------
# Each rung is measured twice in the same repetition: through the tunnel, and
# straight to the VM's public address. Without the bare rung a rate that the
# ACCESS LINK cannot carry that afternoon (V-9: this line has been measured at
# 150 Mbit/s down on a bad day) would be attributed to the tunnel.
#
# ONE LINK IS HELD ACROSS THE WHOLE LADDER, for the same reason V-10's ladder
# does: rebuilding it per rung would put a fresh PMTU search, a fresh direct
# round and a fresh congestion controller inside the independent variable.
#
# WHERE A LOST DATAGRAM WENT
# --------------------------
# Loss is only a finding if it can be attributed, so three counters are read
# around every rung:
#   UdpSndbufErrors  -- the kernel refused the send: the socket buffer is short
#   quinn `lost_pct` -- the QUIC layer saw it leave and never be acknowledged
#   TUN tx dropped   -- the device queue tail-dropped it before bore ever read it
# A rung that loses datagrams with all three flat has lost them somewhere none
# of these three watches, which is itself the most interesting outcome.
#
# DELIVERED IS NOT WHAT IPERF3 PRINTS FIRST
# ----------------------------------------
# `iperf3 -u -b R` reports `bits_per_second` as the rate it OFFERED, not the
# rate that arrived: at 540 Mbit/s offered with 22 % loss it still prints
# 539.9. A ladder that quoted that column would show the tunnel keeping up with
# every rung it is actually failing. Delivered is `offered x (1 - loss)`, and
# that is what this stage prints -- with the offer beside it, so the reader can
# see which of the two a figure came from.
#
# LATENCY IS PART OF EVERY RUNG, NOT A SEPARATE STAGE
# ---------------------------------------------------
# Several of the knobs this ladder walks are QUEUES, and a queue bought with
# throughput is paid for in delay. A stage that printed only delivered Mbit/s
# would make "deeper is better" look true at every rung and would be the exact
# mistake V-10 was fixing. The tunnel RTT is therefore sampled DURING the
# tunnel rung -- inside the same transfer, never beside it -- and both the
# average and the MINIMUM are printed: a minimum well above the idle RTT is a
# standing queue, which is what distinguishes bufferbloat from jitter.
#
# ATTRIBUTING THE CEILING ONCE IT IS FOUND
# ----------------------------------------
# A ceiling at the TUN means bore did not READ fast enough. Two candidates, and
# they are told apart by one number: bore's busiest THREAD. A process at 40 % of
# a machine can still be at 100 % of the one core that matters, and that is what
# a single serialised uplink task looks like. If no thread is near a core, the
# limit is not the CPU -- it is the rate the QUIC layer is willing to send at,
# which is why `CCS` can walk the congestion controller across the same ladder.
#
# THE ARMS AXIS
# -------------
# Once the ceiling is located, naming the MECHANISM means walking one candidate
# at a time with everything else held: the same link shape, the same rates, the
# same repetitions, the bare control in every repetition. `ARMS` is that axis.
# Each arm is `label|ENV|--ws --flags|STREAMS|--vm --flags`, arms separated by
# `;`:
#
#   ARMS='default||||;q4x4||--tun-queues 4|4|;c4|||4|--carriers 4'
#
# The env goes to BOTH ends (harmless on the receiver, and one variable is
# easier to reason about than two). The flags are SPLIT because the two ends do
# not take the same ones: `--tun-queues` is a local property of each end's TUN
# and only the sender's matters here, while `--carriers` is NEGOTIATED as
# min(listener, connector, server) and setting it on one end alone silently
# measures 1. Field 5 exists so that distinction is expressed in the arm rather
# than discovered in the results.
#
# STREAMS is the inner iperf3 flow count (default 1), and it is part of the arm
# rather than a global because it is not independent of the flags: the kernel
# hashes flows across a multi-queue TUN's queues, so `--tun-queues 4` driven by
# ONE flow puts every packet on ONE queue and measures the single-queue path
# under a different name. The rung's offered rate is divided across the streams,
# so the offer is the same quantity in every arm.
#
# `ARMS` unset keeps the previous behaviour exactly: one arm per entry in `CCS`.
# That is deliberate -- the controller matrix already published from this stage
# must stay reproducible from the same command line.
#
# Usage: [REPS=2] [SECS=10] [CCS="bbr"] [ARMS=...] [RATES="200M 300M 375M 450M 540M"] vpn_wire_ceiling.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/vpnlib.sh"

REPS="${REPS:-2}"
SECS="${SECS:-10}"
RATES="${RATES:-200M 300M 375M 450M 540M}"
CCS="${CCS:-bbr}"
# Arms default to the controller sweep so every command line that produced a
# published table still produces it.
ARMS="${ARMS:-}"
if [ -z "$ARMS" ]; then
    for _cc in $CCS; do ARMS="${ARMS:+$ARMS;}cc=$_cc|BORE_DIRECT_QUIC_CC=$_cc|"; done
fi
RUNDIR=/run/bore-vpn-bench

ws_pid()  { cat "$RUNDIR/$1.pid" 2>/dev/null || echo ""; }
ws_ifname(){ sudo -n "$ROOTSH" addr "$1" 2>/dev/null | awk '{print $1}'; }
# UdpSndbufErrors out of /proc/net/snmp, read by COLUMN NAME. The Udp block is
# a header line followed by a values line and the column order is not stable
# across kernel versions, so a fixed field index is a bug waiting for a kernel
# upgrade.
snd_err() {
    awk '/^Udp:/{ if ($2 ~ /[A-Za-z]/) { for (i=2;i<=NF;i++) if ($i=="SndbufErrors") c=i }
                  else if (c) { print $c; exit } }' /proc/net/snmp 2>/dev/null || echo 0
}
tun_drops(){ local n="$1"; [ -n "$n" ] && cat "/sys/class/net/$n/statistics/tx_dropped" 2>/dev/null || echo 0; }
quic_lost(){ ws_log "$1" 3000 | grep -oE 'lost_pct=[0-9.]+' | tail -1 | cut -d= -f2; }
TICK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
proc_ticks() { awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null || echo 0; }
# The BUSIEST single thread, not the process total. A serialised uplink task
# saturates one core while the process reads 25 % of a four-core machine, and
# the process figure is what makes that look like idle headroom.
busiest_thread_ticks() {
    local p="$1" best=0 t
    for t in /proc/"$p"/task/*/stat; do
        [ -r "$t" ] || continue
        local v; v="$(awk '{print $14+$15}' "$t" 2>/dev/null || echo 0)"
        [ "${v:-0}" -gt "$best" ] && best="$v"
    done
    echo "$best"
}

vpn_hdr "VPN wire-ceiling ladder (direct path), ${REPS} reps x ${SECS}s"
echo "  offered rates: $RATES"
echo "  each rung: delivered Mbit/s and loss%, through the tunnel AND bare, same repetition"
echo

# Keyed by "arm|rate": the arm IS the independent variable, so a median that
# pooled the arms would average away the exact thing being measured.
declare -A DT LT DB LB RA
ARM_LABELS=()

# `;` splits arms, `|` splits an arm's five fields. Read with IFS rather than by
# word-splitting, because both the env string and the flag string legitimately
# contain spaces. A trailing field may be omitted; `ARM_PAR` then defaults to 1.
OLD_IFS="$IFS"; IFS=';' read -ra ARM_SPECS <<<"$ARMS"; IFS="$OLD_IFS"

for spec in "${ARM_SPECS[@]}"; do
  [ -n "$spec" ] || continue
  IFS='|' read -r ARM_LABEL ARM_ENV ARM_FLAGS ARM_PAR ARM_VMFLAGS <<<"$spec"
  ARM_PAR="${ARM_PAR:-1}"
  ARM_VMFLAGS="${ARM_VMFLAGS:-}"
  ARM_LABEL="${ARM_LABEL:-arm${#ARM_LABELS[@]}}"
  ARM_LABELS+=("$ARM_LABEL")
  for r in $RATES; do
      k="$ARM_LABEL|$r"; DT[$k]=""; LT[$k]=""; DB[$k]=""; LB[$k]=""; RA[$k]=""
  done

  echo "=== arm: $ARM_LABEL   env='${ARM_ENV:-none}'   ws-flags='${ARM_FLAGS:-none}'   streams=$ARM_PAR   vm-flags='${ARM_VMFLAGS:-none}' ==="
  VPN_ENV="$ARM_ENV"
  VPN_LINK_ID="${VPN_RUN_ID}w$(date +%s%N | tail -c 5)"
  VPN_WS_TAGS=(); VPN_VM_IDS=()

  # shellcheck disable=SC2086
  vm_up listen $ARM_VMFLAGS
  sleep 3
  # shellcheck disable=SC2086
  ws_up wc connect $ARM_FLAGS
  if ! ws_ready wc 60 >/dev/null; then echo "  FAILED: link never came up"; vpn_cleanup; continue; fi
  if [ "$(wait_path wc direct 95)" != direct ]; then
      echo "  FAILED: never reached direct -- this stage has nothing to say about the relay"
      vpn_cleanup; continue
  fi
  MTU="$(wait_mtu_settle wc)"
  IFN="$(ws_ifname wc)"
  WP="$(ws_pid wc)"
  echo "  link up: path=direct tun=$IFN mtu=$MTU pid=${WP:-?}"
  if [ "$(vm_iperf_server)" != 1 ]; then echo "  FAILED: no iperf3 server"; vpn_cleanup; continue; fi
  echo

  printf "  %-6s %-7s %9s %9s %7s %9s %9s %8s %7s %7s %8s %8s\n" \
      rate arm "offered" "delivrd" "loss%" "sndbuf+" "tundrop+" "quic_ls" "cpu%" "thr%" "rtt_avg" "rtt_min"
  for rep in $(seq 1 "$REPS"); do
      echo "  --- rep $rep ---"
      for r in $RATES; do
          # Bare first, then tunnel: the reverse order lets the tunnel's own
          # queues still be draining into the bare rung.
          read -r db lb <<<"$(udp_loss "$BORE_VM" "$SECS" "$r" "$ARM_PAR")"
          bdel="$(awk -v d="${db:-0}" -v l="${lb:-0}" 'BEGIN{printf "%.1f", d*(1-l/100)}')"
          k="$ARM_LABEL|$r"
          DB[$k]="${DB[$k]} $bdel"; LB[$k]="${LB[$k]} ${lb:-0}"
          printf "  %-6s %-7s %9s %9s %7s %9s %9s %8s %7s %7s %8s %8s\n" \
              "$r" bare "${db:-0}" "$bdel" "${lb:-0}" - - - - - - -
          sleep 2

          s0="$(snd_err)"; t0="$(tun_drops "$IFN")"
          c0="$(proc_ticks "${WP:-0}")"; h0="$(busiest_thread_ticks "${WP:-0}")"
          # Sampled INSIDE the transfer, in the background, so the latency and
          # the throughput on one line describe the same instant of the path.
          # `SECS-2` seconds of pings at 5 Hz finish before iperf3 does, which
          # keeps the samples inside the loaded window rather than straddling
          # its edge.
          ( rtt_ms "$B_PEER" $(( (SECS - 2) * 5 )) > "$WORK/.wcrtt.$$" 2>/dev/null ) & RTTP=$!
          read -r dt lt <<<"$(udp_loss "$B_PEER" "$SECS" "$r" "$ARM_PAR")"
          wait $RTTP 2>/dev/null
          rr="$(cat "$WORK/.wcrtt.$$" 2>/dev/null)"; rm -f "$WORK/.wcrtt.$$"
          rtavg="$(echo "$rr" | awk '{print ($2==""?"-":$2)}')"
          rtmin="$(echo "$rr" | awk '{print ($1==""?"-":$1)}')"
          s1="$(snd_err)"; t1="$(tun_drops "$IFN")"
          c1="$(proc_ticks "${WP:-0}")"; h1="$(busiest_thread_ticks "${WP:-0}")"
          tdel="$(awk -v d="${dt:-0}" -v l="${lt:-0}" 'BEGIN{printf "%.1f", d*(1-l/100)}')"
          DT[$k]="${DT[$k]} $tdel"; LT[$k]="${LT[$k]} ${lt:-0}"
          case "$rtavg" in ''|-) ;; *) RA[$k]="${RA[$k]} $rtavg" ;; esac
          awk -v r="$r" -v o="${dt:-0}" -v d="$tdel" -v l="${lt:-0}" -v q="$(quic_lost wc)" \
              -v s0="${s0:-0}" -v s1="${s1:-0}" -v t0="${t0:-0}" -v t1="${t1:-0}" \
              -v c0="${c0:-0}" -v c1="${c1:-0}" -v h0="${h0:-0}" -v h1="${h1:-0}" \
              -v secs="$SECS" -v tick="$TICK" -v ra="${rtavg:--}" -v rm="${rtmin:--}" 'BEGIN{
              printf "  %-6s %-7s %9s %9s %7s %9s %9s %8s %6.1f%% %6.1f%% %8s %8s\n",
                     r, "tunnel", o, d, l, s1-s0, t1-t0, (q==""?"-":q),
                     100*(c1-c0)/tick/secs, 100*(h1-h0)/tick/secs, ra, rm }'
          sleep 3
      done
  done
  vpn_cleanup
  sleep 3
  echo
done

echo
med() { printf '%s\n' $1 | LC_ALL=C sort -n | awk '{a[NR]=$1} END{ if(NR==0) print "n/a"; else print a[int((NR+1)/2)] }'; }
echo "  medians (DELIVERED Mbit/s, loss%), PER ARM:"
for a in "${ARM_LABELS[@]}"; do
    echo "  --- arm $a ---"
    printf "  %-6s %18s %18s %10s %10s\n" rate "bare" "tunnel" "tun/bare" "rtt ms"
    for r in $RATES; do
        k="$a|$r"
        b="$(med "${DB[$k]}")"; bl="$(med "${LB[$k]}")"
        t="$(med "${DT[$k]}")"; tl="$(med "${LT[$k]}")"
        awk -v r="$r" -v b="$b" -v bl="$bl" -v t="$t" -v tl="$tl" -v rt="$(med "${RA[$k]}")" 'BEGIN{
            printf "  %-6s %11s (%4.1f%%) %11s (%4.1f%%) %9.3f %10s\n", r, b, bl, t, tl, (b+0>0? t/b : 0), rt }'
    done
    # V-11: a summary statistic without its samples hides the bug that produced
    # it. Every median above is printed beside the values it came from.
    echo "  raw samples (tunnel delivered): "
    for r in $RATES; do printf "    %-6s %s\n" "$r" "${DT[$a|$r]}"; done
    echo "  raw samples (tunnel rtt avg, ms): "
    for r in $RATES; do printf "    %-6s %s\n" "$r" "${RA[$a|$r]}"; done
done
echo
echo "  reading: a flat 'tunnel delivered' across rising offered rates is a CEILING;"
echo "           delivered tracking the offer until it stops is the PATH running out."
echo
echo "DONE"
