#!/usr/bin/env bash
# Answers ONE question the W1/W2 tables raise and cannot answer themselves:
# the workstation pulls at ~27 MB/s and pushes at ~71 MB/s through the SAME
# tunnel, on the same radio link, in the same minutes. Either the radio is
# asymmetric that way, or the DOWNLOAD direction is the one the instance shapes
# — download is server-OUTBOUND, and a t4g.micro's outbound allowance is the
# confounder this whole campaign is built around. So measure one arm of each
# direction with the server's own allowance counters read immediately before
# and after, which is the only way to tell the two apart.

. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; P=9031; PER=$((96*1048576/4))

vm "timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP'" >/dev/null 2>&1 || { echo "origin not serving on the VM"; exit 2; }
vm "setsid nohup \$HOME/bore local $RP --port $P --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 \
    > \$HOME/out/wsasym.log 2>&1 </dev/null & true" >/dev/null 2>&1
for i in $(seq 80); do adm tunnels | jq -e --argjson p "$P" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 && break; sleep 0.5; done
trap 'vm "pkill -9 -f \"local $RP --port $P\" 2>/dev/null; true" >/dev/null 2>&1' EXIT

arm() { # <verb> <label>
    local i0 o0 i1 o1 res
    i0=$(ena); o0=$(ena_out)
    res=$(python3 "$RAWCLI" "$1" "$BORE_GW" "$P" "$PER" 4 2>&1 | tr -d '\r')
    i1=$(ena); o1=$(ena_out)
    printf '  %-9s %s\n' "$2" "$res"
    printf '  %-9s allowance delta: bw_in_exceeded=%s bw_out_exceeded=%s\n' \
        "" "$(( ${i1:-0} - ${i0:-0} ))" "$(( ${o1:-0} - ${o0:-0} ))"
}
echo "=== allowance attribution, relay tunnel on port $P, 96 MiB / 4 conns ==="
arm get "download"
cool 75
arm put "upload"
echo "  (download = server OUTBOUND to the workstation; upload = server INBOUND)"
