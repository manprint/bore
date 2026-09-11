#!/usr/bin/env bash
# Last open question of the stage. Eight download pairs at FOUR connections put
# the direct path ahead in seven of them (sign test p=0.035), which is the
# reverse of the in-region verdict — but the connection sweep then showed the
# aggregate is LINK-limited and already saturated by ONE connection
# (35.23 MB/s at conns=1 against 27.92 at conns=4). On a link-limited path, and
# with the consumer hop being plain TCP for BOTH transports, the transport
# should make no difference at all. So either the four-connection regime is
# where the difference lives, or the difference is an artifact.
#
# This isolates it: same pairing, same alternation, ONE connection per arm. If
# the direct path still wins consistently the effect is real and belongs to the
# tunnel; if it vanishes, the W1 reversal was the multi-connection regime and
# must not be quoted as a transport verdict.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; R=9044; Q=9045; PER=$((96*1048576))
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsdl1-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 1 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
echo "=== W1c download, ONE connection, 96 MiB, six pairs, order alternating ==="
printf '  %-6s %10s %10s %8s\n' pair relay_MBs quic_MBs ratio
RS=()
for i in 1 2 3 4 5 6; do
    if [ $((i % 2)) = 1 ]; then a=$(g "$R"); cool 75; b=$(g "$Q"); cool 75
    else b=$(g "$Q"); cool 75; a=$(g "$R"); cool 75; fi
    r=$(LC_ALL=C awk -v a="${b:-0}" -v b="${a:-0}" 'BEGIN{if(b>0)printf "%.3f",a/b; else print "nan"}')
    RS+=("$r"); printf '  %-6s %10s %10s %8s\n' "$i" "${a:-0}" "${b:-0}" "$r"
done
echo "  median quic/relay: $(printf '%s\n' "${RS[@]}" | med)"
echo "  ratios: ${RS[*]}"
