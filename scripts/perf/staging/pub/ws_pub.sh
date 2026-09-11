#!/usr/bin/env bash
# Workstation-side public-tunnel measurement: the forwarder runs on the test VM
# (same region as the server), the CONSUMER is this workstation on a domestic
# link. This is the real-world topology — a user on their own machine opening a
# port that someone else's laptop connects to — and it is the ONLY topology
# that exercises the consumer-side path a public tunnel actually serves.
#
# The numbers here are NOT comparable with the VM-side ones and must never be
# quoted together: the link is the bottleneck, not the tunnel. Measured, not
# assumed — `pub/ws_conns.sh` showed a single connection already saturating it
# (35.23 MB/s at one connection against 27.92 at four), so the aggregate does
# not scale with connections and nothing in the tunnel is the bound. Note the
# vhost campaign's ~45 MB/s figure for this radio link holds for DOWNLOAD only:
# this stage measured 68-72 MB/s upload, with the instance's own allowance
# counters at zero in both directions.
#
# What this topology is good for is LATENCY (the two medians agreed to 0.03 ms
# over 80 probes each) and proving the path works at all from outside AWS. It is
# NOT good for transport ratios: the eight download pairs span 0.694 to 2.607,
# and the apparent direct-path win at four connections disappears when isolated
# at one (median 0.943 over six pairs). Do not quote a ratio from this stage.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP="${BORE_RAW_ORIGIN_PORT:-5053}"
PAIRS="${PAIRS:-4}"
MB="${MB:-96}"
CONNS="${CONNS:-4}"
PER=$(( MB * 1048576 / CONNS ))
CARR="${CARR:---carriers 1}"

tsnap()   { adm tunnels | jq -c --argjson p "$1" '.[]|select(.public_port==$p)' 2>/dev/null; }
tfld()    { tsnap "$1" | jq -r --arg f "$2" '.[$f] // empty' 2>/dev/null; }
pub_present() { adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1; }

# Start the raw origin on the VM once; it is idempotent. But "already running"
# is not the same as "running the CURRENT program": H-7 in this campaign had a
# long-lived origin that predated an edit to its own source, and every arm that
# used the new verb silently measured nothing. So reap it first when the process
# is older than its script file — that PID only, never a pattern-wide pkill.
#
# TWO traps here, and H-12 in this campaign walked into both.
#
# 1. A remote `pgrep -f <name>` matches the `bash -c` that is running it, since
#    the remote command string contains the name. Bracketing the PATTERN
#    (`raw_origin.p[y]`) is only half the fix: it stops the pattern from
#    matching its own text, but NOT from matching some other occurrence of the
#    plain name in the same command — and the start command below contains
#    `python3 $HOME/raw_origin.py $RP` for the obvious reason. So a
#    `pgrep || start` one-liner ALWAYS believes the origin is already running,
#    however carefully the pattern is bracketed, and never starts it.
# 2. "A process matching this name exists" is the wrong question anyway. What
#    the stage needs is "something is SERVING on that port", so the existence
#    check is a real TCP connection to it. The reaper below still needs a PID,
#    so it keeps a bracketed `pgrep` — safe because its own command text
#    contains `raw_origin.py)` (from `stat`) and never `raw_origin.py $RP`.
#
# The symptom when this is wrong: the origin never starts, every arm still runs
# and every arm reads `0.00 MB/s` — H-7's shape, a whole stage of zeros in the
# exact format of a real measurement. The preflight further down now refuses to
# measure in that state instead of publishing it.
vm "pid=\$(pgrep -f 'raw_origin.p[y] $RP' | head -1); \
    if [ -n \"\$pid\" ]; then \
      age=\$(ps -o etimes= -p \$pid | tr -d ' '); \
      fage=\$(( \$(date +%s) - \$(stat -c %Y \$HOME/raw_origin.py) )); \
      [ \"\$age\" -gt \"\$fage\" ] && kill -9 \$pid; \
    fi; true" >/dev/null 2>&1
vm "timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP' 2>/dev/null || \
    (setsid nohup python3 \$HOME/raw_origin.py $RP > \$HOME/out/raworigin.log 2>&1 </dev/null &); sleep 1; true" >/dev/null 2>&1
echo "  origin on the VM: $(vm "timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP' 2>/dev/null && pgrep -f 'raw_origin.p[y] $RP' | head -1 || echo 'NOT SERVING'" 2>/dev/null | tr -d '\r')"

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
# ICMP to the gateway may be filtered (it is, on this deployment), and a
# `ping | cut` that silently yields an empty string is exactly the class of
# harness lie this campaign keeps finding. Measure a TCP handshake to the
# control port instead: it always works, it is the same first round trip every
# arm below pays, and it cannot come back blank without saying so.
echo "  TCP handshake RTT to the gateway: $(python3 -c '
import socket, statistics, sys, time
host, port = sys.argv[1], int(sys.argv[2])
ms = []
for _ in range(5):
    t0 = time.perf_counter()
    try:
        s = socket.create_connection((host, port), timeout=5)
        ms.append((time.perf_counter() - t0) * 1000.0)
        s.close()
    except OSError as err:
        print("unreachable (%s)" % err)
        raise SystemExit(0)
    time.sleep(0.2)
print("min=%.2fms median=%.2fms max=%.2fms" % (min(ms), statistics.median(ms), max(ms)))
' "$BORE_GW" "${BORE_CTRL_PORT:-443}" 2>&1)"

vm_up "$RELAY" $CARR        || { echo "relay arm failed to register"; exit 1; }
vm_up "$QUIC"  $CARR --udp  || { echo "quic arm failed to register"; exit 1; }
# The warm-up is also the PREFLIGHT, and it is parsed rather than discarded.
# A registered tunnel proves the CONTROL path, not the DATA path: with nothing
# listening on the forwarder's local port every arm still runs and every arm
# reads 0.00 MB/s — H-7's shape, and how H-12 wasted a whole stage. So refuse
# to measure until both transports have actually moved bytes.
for warm in "$RELAY" "$QUIC"; do
    got=$(python3 "$RAWCLI" get "$BORE_GW" "$warm" 1048576 1 2>&1 | tr -d '\r')
    echo "  preflight port=$warm: $got"
    case "$got" in
        *"bytes=0 "*|"")
            echo "PREFLIGHT FAILED on port $warm: registered, but it moved no bytes."
            echo "  The usual cause is no raw origin on the VM. Check it with:"
            echo "    pgrep -af 'raw_origin.p[y] $RP'"
            exit 2 ;;
    esac
done
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
