#!/usr/bin/env bash
# §13 says the direct path costs 13.58 CPU s/GiB against the relay's 5.35-7.04,
# and the per-stream reduction says where: softirq is 8.05 s/GiB on QUIC and
# 2.74 on the relay — 2.9x. softirq is per-PACKET kernel work, so the question
# is whether the direct path really moves ~3x the packets for the same bytes.
# It should not have to: the path MTU is 1500 both ways, so a QUIC datagram and
# a TCP segment carry about the same payload. The difference would then be
# AGGREGATION — TCP gets TSO/GRO in the driver, UDP gets GSO/GRO only in
# software and only if quinn/the kernel enable it.
#
# Measures bytes AND packets on the server's own interface across one relay
# window and one direct window, and reports packets per GiB and the average
# on-wire packet size, which is the number that tells GSO/GRO apart from
# per-datagram syscalls. Read from the SERVER, not guessed from the client.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
IF="${BORE_SRV_IFACE:-ens5}"
snap() { srv "cat /proc/net/dev | awk -v i=\"$IF:\" '\$1==i{print \$2, \$3, \$10, \$11}'" 2>/dev/null; }
RP=5053; R=9047; Q=9048; SECS=20
UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wspkt-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT
up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }
# The consumer is the VM, in region, so the tunnel and not a domestic link is
# what is loaded. 4 connections, a fixed 20 s window (H-8's `window` argument),
# 64 GiB requested so the window and not the size ends the run.
arm() { # <port> <label>
    local s0 s1 res path
    s0=$(snap)
    res=$(vm "python3 \$HOME/raw_client.py get '$BORE_GW' $1 $((64*1073741824/4)) 4 30 $SECS 2>&1 | tail -1" 2>/dev/null | tr -d '\r')
    s1=$(snap)
    path=$(adm tunnels | jq -r --argjson p "$1" '.[]|select(.public_port==$p)|.current_path' 2>/dev/null)
    echo "  $2 path=$path $res"
    LC_ALL=C awk -v a="$s0" -v b="$s1" -v lbl="$2" 'BEGIN{
        split(a,x," "); split(b,y," ");
        rb=y[1]-x[1]; rp=y[2]-x[2]; tb=y[3]-x[3]; tp=y[4]-x[4];
        gib=tb/1073741824;
        printf "  %-7s tx: %.3f GiB  %d pkt  avg %.0f B/pkt  -> %.0f pkt/GiB\n", lbl, gib, tp, (tp?tb/tp:0), (gib>0?tp/gib:0);
        printf "  %-7s rx: %.3f GiB  %d pkt  avg %.0f B/pkt\n", lbl, rb/1073741824, rp, (rp?rb/rp:0);
    }'
}
echo "=== server-side packet accounting on $IF, 20 s windows, 4 conns ==="
arm "$R" relay
cool 75
arm "$Q" direct
echo "  (tx = server -> consumer, the download direction; avg B/pkt at or near"
echo "   the 1500 MTU means no aggregation, well above it means GSO/TSO)"
