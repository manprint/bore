#!/usr/bin/env bash
# How many PACKETS does each transport spend to deliver the same bytes?
#
# WHY THIS EXISTS, AND WHY IT IS NOT A GENERIC "OVERHEAD" STAGE
# --------------------------------------------------------------
# `ws_conns_var` reads the server's ENA allowance counters as a delta around
# every transfer, and the very first repetition produced a split nobody had
# seen because nobody had looked:
#
#   n=4 relay  92 MiB/s-ish   bw_in_allowance_exceeded +206   pps +65
#   n=4 quic   92 MiB/s-ish   bw_in_allowance_exceeded +  0   pps +23957
#
# The same payload, over the same second, and the two arms trip DIFFERENT
# instance limits: the relay runs into the bandwidth bucket and the direct path
# runs into the PACKETS-PER-SECOND bucket, ~4 800 shaping events per second
# against an idle line of 0,27 (measured; `pub_ws_conns_var_idle_control.out`).
#
# A packets-per-second limit is a statement about PACKET SIZE, and packet size
# on this path is a thing the product chooses. Public `--udp` carries its
# payload on QUIC STREAMS, so quinn packetises at whatever path MTU its own
# discovery has settled on -- 1200 bytes before discovery succeeds, ~1450 after.
# Those two differ by 20 % in packets for identical bytes, on a path where
# packets are the scarce resource. So the question is small and answerable:
#
#     what is the AVERAGE PACKET SIZE the server actually sees and sends,
#     per arm, per direction, over the same transfer size?
#
# WHAT WOULD MAKE THIS A DEFECT AND WHAT WOULD MAKE IT PHYSICS
# --------------------------------------------------------------
# Physics: the direct path carries the payload plus QUIC framing plus an AEAD
# tag in every packet, and it acknowledges. It will ALWAYS use more packets per
# delivered byte than a TCP relay with TSO/GSO on both ends. A ratio near the
# framing arithmetic is finished work.
# A defect: an average inbound packet size stuck near 1200 while the path
# demonstrably carries 1500 means MTU discovery did not complete, and a fifth of
# the packet budget is being spent on nothing. That is worth a look at
# `DIRECT_INITIAL_RTT`/quinn's MTU probe, and it is the only outcome here that
# implies a code change.
#
# THE CONTROL IS AN IDLE BRACKET, AND IT IS NOT OPTIONAL
# -------------------------------------------------------
# `/proc/net/dev` counts EVERY packet the interface handled, and this server
# also carries the operator's live tunnels. So each arm is bracketed, and so is
# an equal stretch of idleness -- if the idle bracket is not negligible against
# the loaded ones, no per-arm number here means anything and the stage says so
# instead of publishing it.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"

RP=5053; R=9065; Q=9066
AGG_MB="${AGG_MB:-460}"
CONNS="${CONNS:-4}"
REPS="${REPS:-3}"
COOL="${COOL:-75}"

UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/pktsize-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT

# A SEPARATE snapshot file from `bore_snap.sh` on purpose: that one belongs to
# `ws_conns_var`, and two stages sharing one remote file is how a change made
# for one silently rewrites the other's instrument.
SNAP_SRC="$WORK/bore_snap_pkt.sh"
cat > "$SNAP_SRC" <<'SNAP_EOF'
#!/usr/bin/env bash
# rx/tx bytes and packets of the default-route interface, plus the ENA
# allowance counters, as one line of key=value pairs.
I=$(ip route show default | awk '/^default/{print $5; exit}')
# Fields after the interface name: 2=rx_bytes 3=rx_packets 4=rx_errs
# 5=rx_drop ... 10=tx_bytes 11=tx_packets 12=tx_errs 13=tx_drop. The DROPS are
# in here because the server carries a lifetime rx_drop of ~5.3e5 against
# ~1.0e9 rx packets, and a packet the host dropped is the one form of loss on
# this path that neither the tunnel nor the instance's allowance reports.
awk -v i="$I:" '$1==i {printf "rxb=%s rxp=%s rxd=%s txb=%s txp=%s txd=%s ", $2, $3, $5, $10, $11, $13}' /proc/net/dev
sudo -n ethtool -S "$I" 2>/dev/null | awk '/pps_allowance_exceeded|bw_in_allowance_exceeded|bw_out_allowance_exceeded/{sub(/:/,"",$1); printf "%s=%s ", $1, $2}'
# Receiver-side pressure. A download that arrives faster than the application
# drains it shows up HERE and nowhere else: the NIC counted the packet, the
# kernel then dropped it out of a socket queue. Absent on some kernels, so the
# keys are emitted only when present -- `?` beats a fabricated 0.
awk '/^TcpExt:/{if(h==""){for(i=1;i<=NF;i++)k[i]=$i;h=1;next}
                for(i=1;i<=NF;i++) if(k[i]=="PruneCalled"||k[i]=="TCPRcvQDrop"||k[i]=="RcvPruned")
                    printf "%s=%s ", k[i], $i}' /proc/net/netstat 2>/dev/null
awk '/^cpu /{printf "tot=%d idle=%d steal=%d ", $2+$3+$4+$5+$6+$7+$8+$9, $5, $9}' /proc/stat
echo
SNAP_EOF

snap_srv() { srv 'bash /tmp/bore_snap_pkt.sh' 2>/dev/null; }

# THE THIRD HOST, WHICH EVERY STAGE IN THIS CAMPAIGN HAD FORGOTTEN.
#
# `ws_conns_var` brackets the server and the VM. Neither is the RECEIVER: on a
# download the bytes end their journey on THIS workstation, whose NIC, softirq
# path and TCP receive queues are as capable of bounding the rate as anything
# upstream -- and no stage had ever read them around a transfer. `origin_cpu`
# measured the client's CPU as a percentage, which answers "was it saturated"
# and not "did it drop anything".
#
# Same file, run locally: the instrument must be identical at both ends or the
# two columns are not comparable.
snap_ws() { bash "$SNAP_SRC" 2>/dev/null; }

# THE SENDING END, WHICH IS THE ONLY UNCONFOUNDED VIEW OF PACKET SIZE.
#
# The server's INBOUND average mixes two things that cannot be separated after
# the fact: the payload arriving from the VM, and the workstation's TCP ACKs for
# the leg the server is simultaneously sending. Worse, the server's TCP receive
# path may COALESCE (GRO) while its UDP receive path may not, which would inflate
# the relay's bytes-per-packet against the direct path's for a reason that has
# nothing to do with either protocol's packet size.
#
# The VM's TRANSMIT counters have neither problem: they count what the sender put
# on the wire, one entry per packet, before anything downstream can merge them.
# That is the number that answers "did quinn raise its datagram size".
snap_vm() { vm 'bash /tmp/bore_snap_pkt.sh' 2>/dev/null; }
val() { printf '%s\n' "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2; exit}'; }
dlt() { local b a; b=$(val "$1" "$3"); a=$(val "$2" "$3")
        case "$b" in ''|*[!0-9]*) printf '?'; return;; esac
        case "$a" in ''|*[!0-9]*) printf '?'; return;; esac
        printf '%s' "$(( a - b ))"; }
# Average bytes per packet, or `?` when either term is missing. NEVER 0: a
# missing counter and a zero-sized packet are not the same statement.
avg() { LC_ALL=C awk -v b="$1" -v p="$2" 'BEGIN{
        if (b=="?"||p=="?"||p+0<=0) { print "?" ; exit } printf "%.0f", b/p }'; }
cpu_ws() { local dt di; dt=$(dlt "$1" "$2" tot); di=$(dlt "$1" "$2" idle)
    case "$dt" in ''|'?'|0) printf 'busy=?'; return;; esac
    LC_ALL=C awk -v t="$dt" -v i="$di" 'BEGIN{printf "busy=%.1f%%", 100*(t-i)/t}'; }

g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$(( AGG_MB * 1048576 / CONNS ))" "$CONNS" 2>/dev/null \
        | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

declare -A SAMP

echo "### packets per delivered byte, relay against QUIC direct -- $(date -Is)"
echo "  ${AGG_MB} MiB per arm over $CONNS connections, $REPS reps, cooldown ${COOL}s"
echo "  counters are the SERVER's /proc/net/dev, bracketed around each arm."
echo

vmcp "$SNAP_SRC" "$BORE_SRV_USER@$BORE_SRV:/tmp/bore_snap_pkt.sh" >/dev/null 2>&1 \
    || { echo "INSTRUMENT FAILURE: could not install the snapshot on the server"; exit 2; }
vmcp "$SNAP_SRC" "$BORE_VM_USER@$BORE_VM:/tmp/bore_snap_pkt.sh" >/dev/null 2>&1 \
    || { echo "INSTRUMENT FAILURE: could not install the snapshot on the VM"; exit 2; }
probe=$(snap_srv)
printf '  snapshot: %s\n' "${probe:-<EMPTY>}"
case "$(val "${probe:-}" rxp)" in ''|*[!0-9]*) echo "INSTRUMENT FAILURE: no /proc/net/dev counters"; exit 2;; esac

# WHETHER THE SENDER'S PACKET COUNT IS A MEASURE OR A LOWER BOUND.
#
# With TSO (relay arm, TCP) or UDP GSO (quic arm, QUIC) the kernel hands the NIC
# ONE oversized entry and counts ONE tx packet for what leaves as many. The
# sender's counter is then a declared lower bound, not the packet count -- and it
# would print a plausible average either way, which is exactly the failure mode
# section 47.4 documents on the RECEIVE side. So the offload state is recorded
# before any number is produced, and the legend below reads it back.
#
# Installed as a FILE, like the snapshot and for the same reason: an awk program
# nested inside a command substitution inside a quoted shell string is how an
# instrument silently returns the empty string and the stage prints a blank
# column that nobody reads as a failure.
OFF_SRC="$WORK/bore_offload.sh"
cat > "$OFF_SRC" <<'OFF_EOF'
#!/usr/bin/env bash
I=$(ip route show default | awk '/^default/{print $5; exit}')
printf 'iface=%s ' "$I"
ethtool -k "$I" 2>/dev/null | awk -F: '
    /^(tcp-segmentation-offload|generic-segmentation-offload|generic-receive-offload|tx-udp-segmentation):/ {
        gsub(/^ +| +$/, "", $2); split($2, a, " "); printf "%s=%s ", $1, a[1] }'
echo
OFF_EOF
vmcp "$OFF_SRC" "$BORE_SRV_USER@$BORE_SRV:/tmp/bore_offload.sh" >/dev/null 2>&1 || true
vmcp "$OFF_SRC" "$BORE_VM_USER@$BORE_VM:/tmp/bore_offload.sh"   >/dev/null 2>&1 || true

echo "=== segmentation offload (decides whether a sender packet count is a measure or a floor) ==="
printf '  %-4s %s\n' ws  "$(bash "$OFF_SRC" 2>/dev/null)"
printf '  %-4s %s\n' vm  "$(vm  'bash /tmp/bore_offload.sh' 2>/dev/null)"
printf '  %-4s %s\n' srv "$(srv 'bash /tmp/bore_offload.sh' 2>/dev/null)"
echo "  a sender tx count with its segmentation offload ON is a LOWER BOUND on"
echo "  wire packets, not a measurement -- read the column accordingly."
echo
echo

up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register";  exit 1; }

arm() { # <label> <port> <rep>
    local lab="$1" port="$2" rep="$3" b a mbs wb wa vb va
    b=$(snap_srv); wb=$(snap_ws); vb=$(snap_vm)
    if [ "$port" = idle ]; then sleep 6; mbs=idle; else mbs=$(g "$port"); fi
    a=$(snap_srv); wa=$(snap_ws); va=$(snap_vm)
    local drxb drxp dtxb dtxp drxd dtxd
    drxb=$(dlt "$b" "$a" rxb); drxp=$(dlt "$b" "$a" rxp); drxd=$(dlt "$b" "$a" rxd)
    dtxb=$(dlt "$b" "$a" txb); dtxp=$(dlt "$b" "$a" txp); dtxd=$(dlt "$b" "$a" txd)
    local in_avg out_avg
    in_avg=$(avg "$drxb" "$drxp"); out_avg=$(avg "$dtxb" "$dtxp")
    [ "$mbs" != idle ] && [ -n "${mbs:-}" ] && {
        SAMP["$lab|mbs"]+=" $mbs"
        SAMP["$lab|in"]+=" $in_avg"; SAMP["$lab|out"]+=" $out_avg"
        SAMP["$lab|pps"]+=" $(dlt "$b" "$a" pps_allowance_exceeded)"
        SAMP["$lab|rxd"]+=" $drxd"; SAMP["$lab|txd"]+=" $dtxd"
        SAMP["$lab|vmtx"]+=" $(avg "$(dlt "$vb" "$va" txb)" "$(dlt "$vb" "$va" txp)")"
    }
    printf '    %-6s %-9s rx %8s pkts / %11s B = %5s B/pkt | tx %8s pkts / %11s B = %5s B/pkt | drops rx+%s tx+%s | pps_shaped +%s\n' \
        "$lab" "${mbs:-FAILED}" "$drxp" "$drxb" "$in_avg" "$dtxp" "$dtxb" "$out_avg" \
        "$drxd" "$dtxd" "$(dlt "$b" "$a" pps_allowance_exceeded)"
    # The SENDING end, on the same bracket: one entry per packet as emitted.
    printf '    %-6s %-9s vm tx %8s pkts / %11s B = %5s B/pkt | vm drops tx+%s\n' \
        "" "" "$(dlt "$vb" "$va" txp)" "$(dlt "$vb" "$va" txb)" \
        "$(avg "$(dlt "$vb" "$va" txb)" "$(dlt "$vb" "$va" txp)")" "$(dlt "$vb" "$va" txd)"
    # The receiving end, on the same bracket.
    printf '    %-6s %-9s ws rx %8s pkts / %11s B = %5s B/pkt | ws drops rx+%s | prune+%s rcvqdrop+%s | ws %s\n' \
        "" "" "$(dlt "$wb" "$wa" rxp)" "$(dlt "$wb" "$wa" rxb)" \
        "$(avg "$(dlt "$wb" "$wa" rxb)" "$(dlt "$wb" "$wa" rxp)")" \
        "$(dlt "$wb" "$wa" rxd)" "$(dlt "$wb" "$wa" PruneCalled)" "$(dlt "$wb" "$wa" TCPRcvQDrop)" \
        "$(cpu_ws "$wb" "$wa")"
    [ "$port" = idle ] || cool "$COOL"
}

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    arm idle  idle "$rep"
    if [ $((rep % 2)) -eq 1 ]; then
        arm relay "$R" "$rep"; arm quic "$Q" "$rep"
    else
        arm quic "$Q" "$rep";  arm relay "$R" "$rep"
    fi
done

echo
echo "=== medians ==="
printf '  %-6s %-10s %-12s %-12s %-12s %-12s %-10s %s\n' arm 'MiB/s' 'VM tx B/pkt' 'in B/pkt' 'out B/pkt' 'pps shaped' 'rx drops' 'tx drops'
for lab in relay quic; do
    printf '  %-6s %-10s %-12s %-12s %-12s %-12s %-10s %s\n' "$lab" \
        "$(printf '%s\n' ${SAMP["$lab|mbs"]:-}  | med)" \
        "$(printf '%s\n' ${SAMP["$lab|vmtx"]:-} | med)" \
        "$(printf '%s\n' ${SAMP["$lab|in"]:-}  | med)" \
        "$(printf '%s\n' ${SAMP["$lab|out"]:-} | med)" \
        "$(printf '%s\n' ${SAMP["$lab|pps"]:-} | med)" \
        "$(printf '%s\n' ${SAMP["$lab|rxd"]:-} | med)" \
        "$(printf '%s\n' ${SAMP["$lab|txd"]:-} | med)"
done

echo
echo "=== raw samples ==="
for k in "${!SAMP[@]}"; do printf '  %-12s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== how to read it ==="
echo "  'VM tx' is the SENDER's own count on the leg that changes between arms."
echo "  It is the only unconfounded view of packet size here -- but only when the"
echo "  offload table above says segmentation is OFF for that arm's protocol."
echo "  With TSO on, the relay arm's tx count is a FLOOR, not a measurement; with"
echo "  tx-udp-segmentation on, so is the quic arm's. The two can be biased in"
echo "  OPPOSITE directions, which would manufacture exactly the difference this"
echo "  stage is looking for."
echo
echo "  'in' is the server RECEIVING, and section 47.4 of the evidence document"
echo "  showed it CANNOT decide packet size: it sums the payload from the VM and"
echo "  the ACK stream from the workstation on one interface, and the split is"
echo "  underdetermined (retransmissions on both legs and the receiver's GRO ratio"
echo "  are both unmeasured). Six cells out of six failed to close against the"
echo "  minimum-frame floor. Do NOT read a code-change decision off this column --"
echo "  that is what this stage did the first time and it was wrong."
echo
echo "  What the 'in' column DOES decide is the DIFFERENCE between the arms: the"
echo "  workstation leg is identical to within 0.05%, so it cancels, and the"
echo "  difference is attributable to the VM->server leg alone. Measured: the"
echo "  direct arm spends 11-19.5% more packets for the same delivered payload,"
echo "  each extra packet carrying only 80-96 B -- the same bytes cut finer."
echo
echo "  The idle rows are the control: if their packet counts are not small"
echo "  against the loaded rows, this server's other traffic is inside every"
echo "  number above and none of them is per-arm."
echo
echo "  Throughput here is NOT a transport comparison: both arms run at up to 97%"
echo "  of this workstation's access line, so the line is the active constraint"
echo "  (V-9). The packet counters are the informative columns; MiB/s is not."

scored=0
for lab in relay quic; do [ -n "${SAMP["$lab|in"]:-}" ] && scored=$((scored+1)); done
[ "$scored" -gt 0 ] || { echo; echo "INSTRUMENT FAILURE: no arm produced a packet-size sample."; exit 2; }

echo
echo "DONE"
