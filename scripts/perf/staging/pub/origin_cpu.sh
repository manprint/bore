#!/usr/bin/env bash
# W1f: at n=8 the connection ladder stops measuring the TUNNEL. Whose ceiling is it?
#
# THE OBSERVATION
# ---------------
# `ws_conns.sh` walks 1/2/4/8 concurrent downloads through a relay tunnel and a
# `--udp` one. At n=4 the two transports differ by 39 % (532 against 740
# Mbit/s), which is the relay's single ordered TCP stream doing exactly what it
# must. At n=8 they land on 618 and 620 -- the same number within 0.3 %.
#
# Two transports that share almost nothing, and that disagree by 39 % one rung
# earlier, do not arrive at the same figure by accident. At n=8 something they
# BOTH depend on is binding, and until it is named the top rung of that ladder
# is not a measurement of bore.
#
# It is not the line: `asym_qualify` measures 933 Mbit/s at P=8 to this same VM,
# 50 % more. Three candidates remain, and the THIRD is the one that was almost
# missed:
#
#   the origin   `raw_origin.py`, ONE asyncio process on the VM
#   the VM       c7i-flex.large, 2 vCPU, also running bore
#   THE CLIENT   `raw_client.py`, ONE asyncio process on the WORKSTATION,
#                driving all n connections from a single event loop
#
# The client earned its place by evidence, not by symmetry. `ws_tunnel` runs on
# the same line the same night and sustains **934 Mbit/s with x8 parallel
# streams** through a vhost tunnel -- with a DIFFERENT origin on the same VM and
# with `curl` (n separate processes) as the client. A Python origin per se is
# therefore not a 74 MB/s ceiling. What the public ladder has and `ws_tunnel`
# does not is one asyncio process pumping every connection at BOTH ends.
#
# THE MEASUREMENT
# ---------------
# Run the SAME rungs through the SAME tunnels and sample, across each arm, the
# origin process's CPU, the VM's total busy time, AND the local client's CPU.
# Then:
#
#   client CPU ~= one full core at n=8 -> the INSTRUMENT is the ceiling
#   origin CPU ~= one full core        -> the origin is
#   VM busy ~= 100 % with both under   -> the VM is, and bore is on it too
#   all three comfortable              -> none of them; look further
#
# n=4 is carried as the CONTROL and is not decoration: the two transports
# provably differ there, so whatever the origin costs at n=4 is a cost that did
# NOT bind. A CPU figure with nothing to compare it against says nothing.
#
# COST: 4 arms x AGG_MB, every one a download, so ~1.8 GiB of egress. Declared.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053; R=9051; Q=9052
AGG_MB="${AGG_MB:-460}"
RUNGS="${RUNGS:-4 8}"
COOL="${COOL:-75}"

UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/origincpu-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down; rm -f "${CLIENT_TIME:-/nonexistent}"' EXIT

# The origin's pid on the VM, and its cumulative CPU in TICKS. Reading
# /proc/<pid>/stat is the kernel's own accounting; `top`/`ps %CPU` would be an
# average over a window this stage does not control.
origin_pid() { vm "pgrep -f 'raw_origin.py $RP' | head -1" 2>/dev/null | tr -dc '0-9'; }
origin_ticks() { # <pid>
    [ -n "${1:-}" ] || { echo ""; return; }
    vm "awk '{print \$14+\$15}' /proc/$1/stat 2>/dev/null" 2>/dev/null | tr -dc '0-9'
}
# Non-idle CPU-seconds for the WHOLE VM, from /proc/stat: total minus idle and
# iowait, over all cores.
vm_busy_ticks() {
    vm "awk '/^cpu /{print \$2+\$3+\$4+\$7+\$8+\$9}' /proc/stat" 2>/dev/null | tr -dc '0-9'
}
vm_hz()    { vm "getconf CLK_TCK" 2>/dev/null | tr -dc '0-9'; }
vm_cores() { vm "nproc" 2>/dev/null | tr -dc '0-9'; }

per_conn() { printf '%s' "$(( AGG_MB * 1048576 / $1 ))"; }

# The client's OWN cpu, measured the same way the kernel would report it for any
# other process. `/usr/bin/time -o` keeps the accounting off the pipeline, so
# the parse below is untouched. Where `/usr/bin/time` is missing the column
# reads `?` -- never 0, which would say the client used no CPU.
CLIENT_TIME="$WORK/origincpu_client.$$"
HAVE_TIME=0; [ -x /usr/bin/time ] && HAVE_TIME=1
g() {
    rm -f "$CLIENT_TIME"
    if [ "$HAVE_TIME" = 1 ]; then
        /usr/bin/time -f '%U %S' -o "$CLIENT_TIME" \
            python3 "$RAWCLI" get "$BORE_GW" "$1" "$(per_conn "$2")" "$2" 2>/dev/null \
            | grep -oE 'MBs=[0-9.]+' | cut -d= -f2
    else
        python3 "$RAWCLI" get "$BORE_GW" "$1" "$(per_conn "$2")" "$2" 2>/dev/null \
            | grep -oE 'MBs=[0-9.]+' | cut -d= -f2
    fi
}
client_cpu() { # user+sys seconds of the last client run, or empty
    [ -s "$CLIENT_TIME" ] || return 0
    LC_ALL=C awk 'NF==2 {printf "%.2f", $1+$2}' "$CLIENT_TIME" 2>/dev/null
}

echo "=== whose ceiling is the n=8 rung? origin CPU sampled across each arm ==="
up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register";  exit 1; }

OPID="$(origin_pid)"; HZ="$(vm_hz)"; CORES="$(vm_cores)"
# An UNFOUND origin pid is not a zero: without it every CPU column below is
# meaningless, and printing zeros there is how a stage publishes "the origin
# used no CPU" when what happened is that it was never located.
if [ -z "$OPID" ] || [ -z "$HZ" ]; then
    echo "  INSTRUMENT FAILURE: origin pid='${OPID:-}' CLK_TCK='${HZ:-}' on the VM."
    echo "  Without both, no CPU column on this page means anything. Stage stops."
    echo "DONE"; exit 2
fi
echo "  origin pid $OPID on the VM, CLK_TCK $HZ, ${CORES:-?} core(s); ${AGG_MB} MiB per rung"
echo

printf '  %-6s %-7s %-10s %-11s %-11s %-9s %-9s %s\n' \
    rung arm MBs 'origin CPUs' 'CPUs/GiB' 'origin%core' 'VM busy%' 'client%core'
for n in $RUNGS; do
    for arm in relay quic; do
        case "$arm" in relay) port=$R ;; quic) port=$Q ;; esac
        o0="$(origin_ticks "$OPID")"; b0="$(vm_busy_ticks)"; t0=$(date +%s.%N)
        mbs="$(g "$port" "$n")"
        t1=$(date +%s.%N); o1="$(origin_ticks "$OPID")"; b1="$(vm_busy_ticks)"
        ccpu="$(client_cpu)"
        LC_ALL=C awk -v n="$n" -v a="$arm" -v m="${mbs:-FAILED}" \
            -v o0="${o0:-}" -v o1="${o1:-}" -v b0="${b0:-}" -v b1="${b1:-}" \
            -v hz="$HZ" -v cores="${CORES:-1}" -v t0="$t0" -v t1="$t1" -v agg="$AGG_MB" \
            -v ccpu="${ccpu:-}" 'BEGIN{
            if (o0=="" || o1=="") { printf "  %-6s %-7s %-10s %s\n", n, a, m, "(origin CPU unreadable -- not zero)"; exit }
            wall = t1 - t0
            cpu  = (o1 - o0) / hz
            gib  = agg / 1024.0
            busy = (b1 - b0) / hz
            cc = (ccpu=="" ? "?" : sprintf("%.0f", (wall>0? ccpu/wall*100 : 0)))
            printf "  %-6s %-7s %-10s %-11.2f %-11.2f %-9.0f %-9.0f %s\n",
                   n, a, m, cpu, cpu/gib, (wall>0? cpu/wall*100 : 0),
                   (wall>0? busy/wall/cores*100 : 0), cc }'
        cool "$COOL"
    done
done

echo
echo "=== reading ==="
echo "  Each %core column is that process against ONE core; 'VM busy%' is the whole"
echo "  instance against all of them. The n=4 row is the control: the two transports"
echo "  provably differ there, so whatever CPU they spend there did NOT bind."
echo "  A column near 100 % at n=8 and comfortably below at n=4 names the ceiling."
echo "  If it is 'client%core', the top rung of ws_conns measures raw_client.py --"
echo "  the INSTRUMENT -- and the ladder must be read to n=4 only until the client"
echo "  is replaced by n separate processes. The same conclusion for the origin"
echo "  column, one host further along. Corroboration either way: ws_tunnel moves"
echo "  934 Mbit/s with x8 streams on this same line, with curl (n processes) as"
echo "  its client and a different origin on the same VM."
echo
echo "DONE"
