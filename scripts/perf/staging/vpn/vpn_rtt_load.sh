#!/usr/bin/env bash
# V-15: how much latency does the TUNNEL add, and how much does the LOAD add?
#
# THE GAP THIS CLOSES
# -------------------
# The wired re-run left one number unattributed. `vpn_txqueue` and `vpn_sndbuf`
# both report an RTT MINIMUM of ~31 ms under load against a bare path whose idle
# RTT is ~19 ms -- a 12 ms floor that no tunable moves -- with ~8 ms of variance
# stacked on top. Neither stage samples the BARE path's RTT *under load*, so
# that floor cannot be split between:
#
#     "the tunnel adds it"   (a cost of encapsulation: TUN, AEAD, QUIC)
#     "the load adds it"     (any flow saturating this link would queue too)
#
# Those have opposite consequences. The first is a defect to hunt; the second is
# physics, and hunting it would waste the window. So the stage measures the same
# quantity four ways per arm -- idle, under upload, under download -- and the
# BARE arm is one of them.
#
# ONE INSTRUMENT, BECAUSE TWO WOULD DECIDE THE ANSWER
# ---------------------------------------------------
# The obvious probe is `ping`, and it is not available: ICMP to the test VM is
# dropped by its security group, which is why `vpnlib.sh` carries both `rtt_ms`
# (ICMP, used on the overlay) and `tcp_connect_ms` (TCP handshake, used on the
# bare path). Measuring the tunnel with ICMP and the bare path with a TCP
# handshake would compare a kernel-path round trip against one that also pays
# accept-queue scheduling -- on a loaded host, a difference of the same order as
# the 12 ms being attributed. The instrument would produce the finding.
#
# So both arms are probed with a TCP handshake to the SAME listener: a
# `attrib_net.py duplex` endpoint on the VM, bound to 0.0.0.0 and therefore
# reachable at the VM's real address (bare) and at its overlay address (tunnel).
# A connect-then-close costs it one short-lived thread and moves no data. iperf3
# is deliberately NOT reused as the probe target -- stray connections to a
# running `iperf3 -s` are control connections, and confusing the load generator
# with the probe is how a measurement becomes its own artefact.
#
# THE PROBE RUNS DURING THE LOAD
# ------------------------------
# A sample taken before or after a transfer measures an idle path and would
# report "the tunnel adds nothing" with perfect confidence. The load is started
# first, given time for the congestion window to open, and the probes are taken
# strictly inside that window. The load's own throughput is printed beside every
# RTT: a probe taken during a transfer that failed is a probe of an idle path,
# and the only way to notice is to look at what the transfer delivered.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/vpnlib.sh"

REPS="${REPS:-3}"
LOAD_SECS="${LOAD_SECS:-14}"
PROBES="${PROBES:-15}"
PAR="${PAR:-4}"
ARMS="${ARMS:-bare relay direct}"
PY_VM_PORT="${PY_VM_PORT:-5311}"
TOOL="$HERE/attrib_net.py"
RTOOL="attrib_net.py"

vpn_hdr "VPN latency under load -- arms: $ARMS -- $REPS reps"
echo "  probe: TCP handshake to the SAME listener on both paths (ICMP to the VM is"
echo "         dropped by its security group, so ping cannot be the common instrument)"
echo "  load : iperf3 P=$PAR for ${LOAD_SECS}s; probes taken strictly inside it"
echo

[ -f "$TOOL" ] || { echo "missing instrument $TOOL"; exit 2; }

VM_PIDS=()
remote_cleanup() {
    local p
    for p in "${VM_PIDS[@]:-}"; do
        [ -n "$p" ] && vm "kill -TERM $p 2>/dev/null; true" >/dev/null 2>&1
    done
}
trap 'remote_cleanup; vpn_cleanup; vpn_assert_clean' EXIT
trap 'remote_cleanup; vpn_cleanup; vpn_assert_clean; exit 130' INT TERM

echo "=== provisioning the probe target ==="
vmcp "$TOOL" "$BORE_VM_USER@$BORE_VM:~/$RTOOL" >/dev/null 2>&1 || { echo "  scp to VM failed"; exit 1; }
if vm "ss -lnt 2>/dev/null | grep -q ':$PY_VM_PORT '" >/dev/null 2>&1; then
    echo "  port $PY_VM_PORT already in use on the VM -- NOT touching it"; exit 1
fi
pid=$(vm "setsid nohup python3 ~/$RTOOL duplex $PY_VM_PORT >/dev/null 2>&1 </dev/null & echo \$!" 2>/dev/null | tr -dc '0-9')
[ -n "$pid" ] && VM_PIDS+=("$pid")
sleep 1
vm "ss -lnt 2>/dev/null | grep -q ':$PY_VM_PORT '" >/dev/null 2>&1 \
    || { echo "  probe endpoint FAILED to bind $PY_VM_PORT"; exit 1; }
echo "  probe endpoint on the VM, port $PY_VM_PORT (pid $pid)"
echo

bring_up() {                    # relay|direct
    local want="$1" extra=""
    [ "$want" = relay ] && extra="--relay-only"
    VPN_LINK_ID="${VPN_RUN_ID}$(date +%s%N | tail -c 6)"
    VPN_WS_TAGS=(); VPN_VM_IDS=()
    vm_up listen $extra
    sleep 3
    ws_up rl connect $extra
    ws_ready rl 45 >/dev/null || { echo "    link did not come up"; return 1; }
    if [ "$want" = direct ]; then
        local got; got="$(wait_path rl direct 75)"
        [ "$got" = direct ] || { echo "    link stayed on $got, wanted direct"; return 1; }
    fi
    ARM_MTU="$(wait_mtu_settle rl 24 90)"
    return 0
}

# probe_during <target-ip> <idle|up|down>
# Prints "median_rtt_ms  load_mbps". On `idle` there is no load and the second
# column reads "-".
probe_during() {
    local ip="$1" mode="$2" lf="" lpid="" mbps="-" rtt
    if [ "$mode" != idle ]; then
        [ "$mode" = down ] && lf="-R"
        # The load runs in the background and its result is captured, so a
        # transfer that failed cannot masquerade as a quiet path.
        local out="$WORK/rttload.$$.json"
        # shellcheck disable=SC2086
        ( iperf3 -c "$ip" -p "$IPERF_PORT" -t "$LOAD_SECS" -P "$PAR" $lf -J >"$out" 2>/dev/null ) &
        lpid=$!
        sleep 3                 # let the congestion window open
    fi
    rtt="$(tcp_connect_ms "$ip" "$PY_VM_PORT" "$PROBES")"
    if [ -n "$lpid" ]; then
        wait "$lpid" 2>/dev/null
        mbps="$(jq -r '(.end.sum_received.bits_per_second // 0)/1e6 | (.*10|round)/10' "$out" 2>/dev/null)"
        rm -f "$out"
    fi
    echo "$rtt ${mbps:-FAILED}"
}

declare -A R
add() { case "$2" in ''|nan|FAILED) return;; esac; R["$1"]+=" $2"; }

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    for arm in $ARMS; do
        target="$BORE_VM"
        if [ "$arm" != bare ]; then
            bring_up "$arm" || { echo "    $arm: FAILED (link)"; vpn_cleanup; sleep 2; continue; }
            target="$B_PEER"
        fi
        if [ "$(vm_iperf_server)" != 1 ]; then
            echo "    $arm: FAILED (no iperf3 server on the VM)"
            [ "$arm" != bare ] && { vpn_cleanup; sleep 2; }
            continue
        fi
        for mode in idle up down; do
            read -r rtt mbps <<<"$(probe_during "$target" "$mode")"
            printf '    %-7s %-5s rtt %-9s ms   load %-9s Mbit/s\n' "$arm" "$mode" "$rtt" "$mbps"
            add "$arm|$mode" "$rtt"
            [ "$mode" != idle ] && add "$arm|${mode}_bw" "$mbps"
            sleep 2
        done
        if [ "$arm" != bare ]; then
            path="$(ws_path rl)"
            [ "$path" = "$arm" ] || echo "    $arm: WARNING path read back as '$path'"
            vpn_cleanup; sleep 3
        fi
    done
done

echo
echo "=== medians (ms) ==="
m() { printf '%s\n' ${R["$1"]:-} | med; }
printf '  %-8s %-11s %-11s %-11s %-11s %-11s\n' arm idle 'under up' 'under down' 'up Mbit/s' 'down Mbit/s'
for arm in $ARMS; do
    printf '  %-8s %-11s %-11s %-11s %-11s %-11s\n' "$arm" \
      "$(m "$arm|idle")" "$(m "$arm|up")" "$(m "$arm|down")" \
      "$(m "$arm|up_bw")" "$(m "$arm|down_bw")"
done

echo
echo "=== raw samples ==="
for k in "${!R[@]}"; do printf '  %-16s%s\n' "$k" "${R[$k]}"; done | sort

echo
echo "=== the attribution ==="
LC_ALL=C awk \
  -v bi="$(m 'bare|idle')" -v bu="$(m 'bare|up')" -v bd="$(m 'bare|down')" \
  -v ri="$(m 'relay|idle')" -v ru="$(m 'relay|up')" -v rd="$(m 'relay|down')" \
  -v di="$(m 'direct|idle')" -v du="$(m 'direct|up')" -v dd="$(m 'direct|down')" 'BEGIN{
  n="^[0-9.]+$"
  if (!(bi ~ n)) { print "  the bare arm produced no idle sample -- nothing is attributable"; exit }
  printf "  bare    idle %6.2f", bi
  if (bu ~ n) printf "   +%.2f under upload", bu-bi
  if (bd ~ n) printf "   +%.2f under download", bd-bi
  printf "   <- what ANY flow costs on this link\n"
  split("relay direct", nm, " ")
  ai[1]=ri; au[1]=ru; ad[1]=rd; ai[2]=di; au[2]=du; ad[2]=dd
  for (i=1;i<=2;i++) {
      if (!(ai[i] ~ n)) continue
      printf "  %-7s idle %6.2f   = bare + %.2f", nm[i], ai[i], ai[i]-bi
      printf "   <- the fixed cost of encapsulation\n"
      if (au[i] ~ n && bu ~ n)
          printf "  %-7s under upload   +%.2f over its own idle, against bare +%.2f  => tunnel queueing %+.2f ms\n", \
                 nm[i], au[i]-ai[i], bu-bi, (au[i]-ai[i])-(bu-bi)
      if (ad[i] ~ n && bd ~ n)
          printf "  %-7s under download +%.2f over its own idle, against bare +%.2f  => tunnel queueing %+.2f ms\n", \
                 nm[i], ad[i]-ai[i], bd-bi, (ad[i]-ai[i])-(bd-bi)
  }
  print ""
  print "  A tunnel-queueing term near zero means the milliseconds under load are the"
  print "  LINK saturating, not the tunnel buffering, and no tunable will remove them."
  print "  A large positive term is a standing queue inside the tunnel and IS a target."
}'
echo
echo "DONE"
