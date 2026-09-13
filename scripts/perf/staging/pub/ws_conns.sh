#!/usr/bin/env bash
# The workstation download numbers raise a mechanism question the pairs cannot
# answer. On the RELAY the same tunnel reads ~25 MB/s pulling and ~71 MB/s
# pushing, in the same minutes, over the same radio link — and `--carriers 4`
# does not help (measured 0.889). The remaining candidate is a PER-CONNECTION
# window: the bytes travel origin -> client -> yamux substream -> server ->
# public TCP, and the RTT sits on the LAST hop, downstream of the substream.
# `mux::config` leaves the connection window unbounded and lets each stream
# auto-tune, but auto-tuning observes the YAMUX link's own BDP, which is the
# in-region 1 ms hop, so it has no reason to grow for a consumer 19 ms away.
#
# That makes a falsifiable prediction: if a per-connection window is the bound,
# the AGGREGATE rate scales with the NUMBER OF CONNECTIONS (each substream has
# its own window) while the per-connection rate stays flat. If instead the
# access link is the bound, the aggregate is flat and the per-connection rate
# falls as 1/N. Every rung moves the SAME AGGREGATE, so every cell runs for a
# comparable number of seconds and the ramp is the same fraction of each (see
# the note on `AGG_MB` below -- equal bytes per CONNECTION does the opposite).
#
# THREE THINGS THE WiFi VERSION OF THIS STAGE GOT WRONG, FIXED HERE
# ------------------------------------------------------------------
# 1. **24 MiB per connection is not a throughput measurement on a wired link.**
#    At 922 Mbit/s that transfer lasts 0.2 s, of which TCP slow start is most;
#    the number it produces is a RAMP, not a rate. Measured 2026-09-12: the
#    whole stage took 615 s of which 600 s was cooldown, i.e. ~15 s of actual
#    transfer across eight cells. Every rung now moves `AGG_MB` (460 MiB), so
#    the shortest cell runs about 5 s and the longest about 8 s -- seconds
#    rather than milliseconds, and comparable to each other. This is the same
#    trap V-15's corollary names for controls, in a different dress: a
#    measurement shorter than the ramp measures the ramp.
# 2. **One repetition per cell, printed as a result.** Every other stage in this
#    campaign repeats and prints its raw samples, because a median with no
#    samples beside it has not been read (V-11). A single sample cannot tell a
#    36 % drop from noise — and the WiFi run produced exactly that shape at
#    n=8 (relay 92.41 -> 58.96 MB/s) with nothing to say whether it was real.
# 3. **The rungs always ran in the same order**, so a line that drifted during
#    the stage would present as a rung effect. Odd repetitions now walk the
#    ladder up and even ones walk it down.
#
# WHERE THE CONTROL FOR THIS STAGE LIVES, AND WHY IT IS NOT HERE
# ---------------------------------------------------------------
# The relay arm's last hop is SERVER -> workstation, so the reference it needs
# is a bare workstation<->SERVER transfer — not the workstation<->VM baseline
# every other stage uses, which measures a different host. That measurement
# exists and is deliberately not duplicated: `vpn/vpn_relay_attrib.sh` takes it
# with `attrib_net.py` (the staging server has neither iperf3 nor socat, and
# installing packages on the host carrying the operator's live tunnels is not a
# benchmark's decision), together with the server's own ENA allowance delta and
# CPU across the arm. Read the two stages together; a percentage quoted from
# this one alone attributes the server's uplink to bore.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; R=9041; Q=9042
# BYTES PER RUNG, NOT BYTES PER CONNECTION -- and this is a correction, not a
# saving dressed up as one.
#
# The original design held bytes-per-connection constant and justified it as
# "so every cell pays the same ramp". A ramp is a TIME, not a byte count, so
# equal bytes per connection does not produce equal ramps: at n=8 each
# connection ramps toward one EIGHTH of the link, which takes LESS time, while
# the cell itself runs EIGHT TIMES longer (2.2 s at n=1 against 17.8 s at n=8
# on this line). The cell that needed the most duration got the least.
#
# Holding the RUNG's aggregate constant fixes both ends of that. Every rung now
# moves `AGG_MB` in total, so at the measured rates the shortest cell is about
# 5 s and the longest about 8 s -- every one of them thirty to sixty times the
# ~133 ms a flow needs to reach this line's rate at 19 ms RTT (V-19), instead of
# one cell at 2.2 s and another at 17.8 s.
#
# It also halves what this stage costs AWS in egress, which is why it was
# revisited -- but it would be the right shape at any price. `PER_FIXED=<bytes>`
# restores the original per-connection form for a run that needs to reproduce
# the old figures exactly.
AGG_MB="${AGG_MB:-460}"
PER_FIXED="${PER_FIXED:-}"
REPS="${REPS:-3}"
RUNGS="${RUNGS:-1 2 4 8}"
COOL="${COOL:-75}"
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsconns-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }
# Bytes THIS connection carries, so that the rung's aggregate is `AGG_MB`.
per_conn() { # <n>
    if [ -n "$PER_FIXED" ]; then printf '%s' "$PER_FIXED"; return; fi
    printf '%s' "$(( AGG_MB * 1048576 / $1 ))"
}
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$(per_conn "$2")" "$2" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

# NOTE the array is SAMP and not S, and that is not a style choice.
# `lib.sh` publishes the scalar `S="$BORE_SRV"` -- the staging server's address.
# `declare -A S` on an existing SCALAR does not replace it: bash keeps the old
# value as element **[0]**. So every stage that named its sample array `S`
# printed the server's real IP address into its own raw-samples block, under the
# key `0`, in every run. MEASURED in `pub_ws_conns_r2.out`, where it sat between
# the quic and relay samples. It never reached a median (`med()` refuses a
# non-numeric sample -- that guard earned its keep here), but it reached an
# EVIDENCE FILE, which is precisely how coordinates escape: through prose and
# output, never through code.
declare -A SAMP      # SAMP[arm|n] = space separated MB/s samples
if [ -n "$PER_FIXED" ]; then
    echo "=== download vs connection count -- $((PER_FIXED/1048576)) MiB PER CONNECTION (legacy shape), carriers=1 ==="
else
    echo "=== download vs connection count -- ${AGG_MB} MiB PER RUNG (equal duration), carriers=1 ==="
fi
echo "  reps=$REPS rungs='$RUNGS' cooldown=${COOL}s between arms"
echo "  bytes per connection by rung:$(for n in $RUNGS; do printf ' n=%s:%sMiB' "$n" "$(( $(per_conn "$n") / 1048576 ))"; done)"
echo

for rep in $(seq 1 "$REPS"); do
    order="$RUNGS"
    # Even repetitions walk the ladder downward: a monotone drift in the line
    # then shows up as disagreement BETWEEN repetitions instead of as a slope
    # across the rungs, which is the only way to tell the two apart.
    [ $((rep % 2)) -eq 0 ] && order="$(printf '%s\n' $RUNGS | tac | tr '\n' ' ')"
    echo "  --- rep $rep  (rungs: $order)"
    for n in $order; do
        a=$(g "$R" "$n"); cool "$COOL"
        b=$(g "$Q" "$n"); cool "$COOL"
        [ -n "${a:-}" ] && SAMP["relay|$n"]+=" $a"
        [ -n "${b:-}" ] && SAMP["quic|$n"]+=" $b"
        printf '    n=%-3s relay=%-9s quic=%-9s\n' "$n" "${a:-FAILED}" "${b:-FAILED}"
    done
done

echo
echo "=== medians (MB/s aggregate, and per connection) ==="
printf '  %-6s %11s %11s %11s %11s\n' conns relay_agg relay_per quic_agg quic_per
for n in $RUNGS; do
    ra=$(printf '%s\n' ${SAMP["relay|$n"]:-} | med)
    qa=$(printf '%s\n' ${SAMP["quic|$n"]:-}  | med)
    printf '  %-6s %11s %11s %11s %11s\n' "$n" "${ra:-n/a}" \
      "$(awk -v v="$ra" -v n="$n" 'BEGIN{if(v==""||v=="n/a"){print "n/a"}else{printf "%.2f", v/n}}')" \
      "${qa:-n/a}" \
      "$(awk -v v="$qa" -v n="$n" 'BEGIN{if(v==""||v=="n/a"){print "n/a"}else{printf "%.2f", v/n}}')"
done

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for k in "${!SAMP[@]}"; do printf '  %-12s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== reading ==="
echo "  A flat relay_per column with a rising relay_agg means a per-connection"
echo "  window; a flat relay_agg with a falling relay_per means the link."
echo "  A relay_agg that RISES then FALLS is neither, and needs the samples above"
echo "  before it is called anything: with one sample per cell it is noise."
echo "  Whatever the shape, the reference for the relay arm's last hop is the bare"
echo "  workstation<->SERVER figure in vpn/vpn_relay_attrib.sh, not the"
echo "  workstation<->VM baseline -- they are different hosts."
echo
echo "DONE"
