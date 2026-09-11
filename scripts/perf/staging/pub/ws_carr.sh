#!/usr/bin/env bash
# W1 measured the RELAY losing to QUIC direct by 1.238x on download from this
# workstation — the OPPOSITE of the in-region verdict (§6: relay wins 1.51x).
# One mechanism explains a reversal that appears only with a distant consumer:
# on the relay path all four proxied connections ride ONE TCP carrier as yamux
# substreams, so a 40 ms consumer that drains slowly makes them share one
# congestion window and one head-of-line; on the direct path each proxied
# connection is its own QUIC stream. If that is the mechanism, `--carriers 4`
# must close the gap. If it is not, carriers will change nothing and the
# reversal needs another explanation. Both tunnels are up SIMULTANEOUSLY and
# hit in alternating order, so neither pays the other's residue.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; P1=9033; P4=9034; PER=$((96*1048576/4))
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers $2 \
        > \$HOME/out/wscarr-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$P1" 1 || { echo "carriers=1 arm failed to register"; exit 1; }
up "$P4" 4 || { echo "carriers=4 arm failed to register"; exit 1; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 4 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
carr() { adm tunnels | jq -r --argjson p "$1" '.[]|select(.public_port==$p)|.carriers' 2>/dev/null; }
echo "=== relay download, carriers 1 vs 4, from the workstation (96 MiB / 4 conns) ==="
echo "  server reports carriers: port=$P1 -> $(carr "$P1")  port=$P4 -> $(carr "$P4")"
printf '  %-6s %10s %10s %8s\n' round c1_MBs c4_MBs c4/c1
RS=()
for i in 1 2 3; do
    if [ $((i % 2)) = 1 ]; then a=$(g "$P1"); cool 75; b=$(g "$P4"); cool 75
    else b=$(g "$P4"); cool 75; a=$(g "$P1"); cool 75; fi
    r=$(LC_ALL=C awk -v a="${b:-0}" -v b="${a:-0}" 'BEGIN{if(b>0)printf "%.3f",a/b; else print "nan"}')
    RS+=("$r"); printf '  %-6s %10s %10s %8s\n' "$i" "${a:-0}" "${b:-0}" "$r"
done
echo "  median c4/c1: $(printf '%s\n' "${RS[@]}" | med)"
