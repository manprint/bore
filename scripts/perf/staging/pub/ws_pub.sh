#!/usr/bin/env bash
# Workstation-side public-tunnel measurement: the forwarder runs on the test VM
# (same region as the server), the CONSUMER is this workstation on a domestic
# link. This is the real-world topology — a user on their own machine opening a
# port that someone else's laptop connects to — and it is the ONLY topology
# that exercises the consumer-side path a public tunnel actually serves.
#
# The numbers here are NOT comparable with the VM-side ones and must never be
# quoted together: the vhost campaign established that this workstation's radio
# link caps the path at ~45 MB/s regardless of the tunnel, so a slower number
# here means the link, not the tunnel. What this topology is good for is
# LATENCY, transport RATIOS, and proving the path works at all from outside AWS.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP="${BORE_RAW_ORIGIN_PORT:-5053}"
PAIRS="${PAIRS:-4}"
MB="${MB:-96}"
CONNS="${CONNS:-4}"
PER=$(( MB * 1048576 / CONNS ))
CARR="${CARR:---carriers 1}"

tsnap()   { adm tunnels | jq -c --argjson p "$1" '.[]|select(.port==$p)' 2>/dev/null; }
tfld()    { tsnap "$1" | jq -r --arg f "$2" '.[$f] // empty' 2>/dev/null; }
pub_present() { adm tunnels | jq -e --argjson p "$1" 'any(.[]; .port==$p)' >/dev/null 2>&1; }

# Start the raw origin on the VM once; it is idempotent.
vm "pgrep -f 'raw_origin.py $RP' >/dev/null 2>&1 || \
    (setsid nohup python3 \$HOME/raw_origin.py $RP > \$HOME/out/raworigin.log 2>&1 </dev/null &); sleep 1; true" >/dev/null 2>&1

VM_UP=()
vm_up() { # <public_port> <flags...>
    local p="$1"; shift
    vm "setsid nohup \$HOME/bore local $RP --port $p --to '$BORE_TO' --secret '$BORE_SECRET' $* \
        > \$HOME/out/wspub-$p.log 2>&1 </dev/null & true" >/dev/null 2>&1
    local i
    for i in $(seq 80); do pub_present "$p" && { VM_UP+=("$p"); return 0; }; sleep 0.5; done
    return 1
}
vm_down() { # NEVER a blanket pkill: unrelated operator tunnels live here.
    local p
    for p in "${VM_UP[@]:-}"; do vm "pkill -9 -f 'local $RP --port $p' 2>/dev/null; true" >/dev/null 2>&1; done
    VM_UP=()
}
trap 'vm_down; exit 130' INT TERM
trap 'vm_down' EXIT

rget()  { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" "$CONNS" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
rput()  { python3 "$RAWCLI" put "$BORE_GW" "$1" "$PER" "$CONNS" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
rping() { python3 "$RAWCLI" ping "$BORE_GW" "$1" "${2:-60}" 2>/dev/null; }

RELAY="${PUB_WS_RELAY:-9021}"
QUIC="${PUB_WS_QUIC:-9022}"

say "workstation consumer -> AWS server -> VM forwarder"
echo "  RTT to the gateway: $(ping -c 5 -q "$BORE_GW" 2>/dev/null | tail -1 | cut -d= -f2 || echo n/a)"

vm_up "$RELAY" $CARR        || { echo "relay arm failed to register"; exit 1; }
vm_up "$QUIC"  $CARR --udp  || { echo "quic arm failed to register"; exit 1; }
python3 "$RAWCLI" get "$BORE_GW" "$RELAY" 1048576 1 >/dev/null 2>&1
python3 "$RAWCLI" get "$BORE_GW" "$QUIC"  1048576 1 >/dev/null 2>&1
echo "  relay=$RELAY quic=$QUIC path=$(tfld "$QUIC" current_path) pool=$(tfld "$QUIC" direct_pool)"

echo
echo "===== W1 paired transport A/B from the workstation, download ====="
printf '  %-5s %10s %10s %8s  %s\n' pair relay_MBs quic_MBs ratio "quic path"
RS=()
for i in $(seq "$PAIRS"); do
    if [ $((i % 2)) = 1 ]; then a=$(rget "$RELAY"); cool 45; b=$(rget "$QUIC"); cool 45
    else b=$(rget "$QUIC"); cool 45; a=$(rget "$RELAY"); cool 45; fi
    r=$(LC_ALL=C awk -v a="${b:-0}" -v b="${a:-0}" 'BEGIN{if(b>0)printf "%.3f",a/b; else print "nan"}')
    RS+=("$r")
    printf '  %-5s %10s %10s %8s  %s\n' "$i" "${a:-0}" "${b:-0}" "$r" "$(tfld "$QUIC" current_path)"
done
echo "  median quic/relay: $(printf '%s\n' "${RS[@]}" | med)"

echo
echo "===== W2 upload ====="
printf '  %-5s %10s %10s %8s\n' pair relay_MBs quic_MBs ratio
RS=()
for i in $(seq "$PAIRS"); do
    if [ $((i % 2)) = 1 ]; then a=$(rput "$RELAY"); cool 45; b=$(rput "$QUIC"); cool 45
    else b=$(rput "$QUIC"); cool 45; a=$(rput "$RELAY"); cool 45; fi
    r=$(LC_ALL=C awk -v a="${b:-0}" -v b="${a:-0}" 'BEGIN{if(b>0)printf "%.3f",a/b; else print "nan"}')
    RS+=("$r"); printf '  %-5s %10s %10s %8s\n' "$i" "${a:-0}" "${b:-0}" "$r"
done
echo "  median quic/relay: $(printf '%s\n' "${RS[@]}" | med)"

echo
echo "===== W3 latency, one new connection per probe ====="
echo "  relay: $(rping "$RELAY" 80)"
echo "  quic : $(rping "$QUIC" 80)"
echo "  (a public tunnel port carries plain TCP, so this is one TCP handshake"
echo "   to the server plus one substream/stream open to the forwarder — there"
echo "   is no TLS on the tunnel port unless the tunnel asked for --https)"

echo
echo "===== W4 server view ====="
for p in "$RELAY" "$QUIC"; do
    echo "  port=$p $(tsnap "$p" | jq -r '"path=\(.current_path) carriers=\(.carriers) opens=\(.direct_stream_opens) fb=\(.direct_fallbacks) pool=\(.direct_pool)"' 2>/dev/null)"
done
echo DONE
