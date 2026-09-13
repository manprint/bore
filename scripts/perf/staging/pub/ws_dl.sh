#!/usr/bin/env bash
# Four more DOWNLOAD pairs for the workstation topology, and only download.
# Reason: W1 measured the direct path AHEAD of the relay on download — the
# reverse of the in-region verdict — on four pairs out of four, which is a sign
# test at p=0.0625: suggestive, not settled. The same hour also produced the
# same relay configuration at 23.71 and at 38.83 MB/s, a 1.64x spread, so the
# absolute rate carries nothing and only the PAIRED ratio does. Eight pairs all
# pointing one way is p=0.0039; that is worth twelve minutes.
# TRANSFER SIZE, AND WHY IT IS NOW A VARIABLE
# -------------------------------------------
# The 96 MiB in this stage was sized for WiFi, where it lasted about two
# seconds. Wired the same transfer lasts 0.83 s at 922 Mbit/s, most of it TCP
# slow start, so the number it produces is a RAMP rather than a rate -- and the
# spread says so: the wired run of this campaign's public stages produced paired
# ratios from 0.623 to 1.323 on arms that should have agreed. `XFER_MB` is the
# TOTAL moved per arm; the wired default is 384 MiB (~3.3 s at line rate) and
# `XFER_MB=96` reproduces the original figures exactly.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
XFER_MB="${XFER_MB:-384}"
RP=5053; R=9036; Q=9037; PER=$(( XFER_MB*1048576/4 ))
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsdl-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$R" "" || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 4 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
pth() { adm tunnels | jq -r --argjson p "$1" '.[]|select(.public_port==$p)|"\(.current_path)/o=\(.direct_stream_opens)/fb=\(.direct_fallbacks)"' 2>/dev/null; }
echo "=== W1b four more download pairs (${XFER_MB} MiB / 4 conns, order alternating) ==="
printf '  %-6s %10s %10s %8s  %s\n' pair relay_MBs quic_MBs ratio "quic path"
RS=()
for i in 1 2 3 4; do
    if [ $((i % 2)) = 1 ]; then a=$(g "$R"); cool 75; b=$(g "$Q"); cool 75
    else b=$(g "$Q"); cool 75; a=$(g "$R"); cool 75; fi
    r=$(LC_ALL=C awk -v a="${b:-0}" -v b="${a:-0}" 'BEGIN{if(b>0)printf "%.3f",a/b; else print "nan"}')
    RS+=("$r"); printf '  %-6s %10s %10s %8s  %s\n' "$i" "${a:-0}" "${b:-0}" "$r" "$(pth "$Q")"
done
echo "  median quic/relay: $(printf '%s\n' "${RS[@]}" | med)"
echo "  relay final: $(pth "$R")"
