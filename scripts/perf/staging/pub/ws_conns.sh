#!/usr/bin/env bash
# The workstation download numbers raise a mechanism question the pairs cannot
# answer. On the RELAY the same tunnel reads ~25 MB/s pulling and ~71 MB/s
# pushing, in the same minutes, over the same radio link — and `--carriers 4`
# does not help (measured 0.889). The remaining candidate is a PER-CONNECTION
# window: the bytes travel origin -> client -> yamux substream -> server ->
# public TCP, and the 40 ms RTT sits on the LAST hop, downstream of the
# substream. `mux::config` leaves the connection window unbounded and lets each
# stream auto-tune, but auto-tuning observes the YAMUX link's own BDP, which is
# the in-region 1 ms hop, so it has no reason to grow for a consumer 40 ms away.
#
# That makes a falsifiable prediction: if a per-connection window is the bound,
# the AGGREGATE rate scales with the NUMBER OF CONNECTIONS (each substream has
# its own window) while the per-connection rate stays flat. If instead the
# radio link is the bound, the aggregate is flat and the per-connection rate
# falls as 1/N. Bytes per connection are held CONSTANT so every cell pays the
# same ramp.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; R=9041; Q=9042; PER=$((24*1048576))
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsconns-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" "$2" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
echo "=== download vs connection count, 24 MiB PER CONNECTION, carriers=1 ==="
printf '  %-6s %11s %11s %11s %11s\n' conns relay_agg relay_per quic_agg quic_per
for n in 1 2 4 8; do
    a=$(g "$R" "$n"); cool 75
    b=$(g "$Q" "$n"); cool 75
    printf '  %-6s %11s %11s %11s %11s\n' "$n" "${a:-0}" \
      "$(LC_ALL=C awk -v v="${a:-0}" -v n="$n" 'BEGIN{printf "%.2f", v/n}')" "${b:-0}" \
      "$(LC_ALL=C awk -v v="${b:-0}" -v n="$n" 'BEGIN{printf "%.2f", v/n}')"
done
echo "  a flat relay_per column with a rising relay_agg means a per-connection window;"
echo "  a flat relay_agg with a falling relay_per means the link."
