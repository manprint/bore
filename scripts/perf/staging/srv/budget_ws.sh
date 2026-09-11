#!/usr/bin/env bash
# Price BORE_UDP_MEMORY_BUDGET from the DOMESTIC consumer, on the QUIC direct
# path, A/B/A/B.
#
# Why only from there. The budget derives the per-connection window as
# clamp(budget / max_carriers, 16MiB, 256MiB) and the stream window as a
# sixteenth of it. On a deployment with a large --max-carriers, any practical
# budget lands on the 16MiB floor, so the stream window becomes 1MiB. At a
# same-region 2ms RTT that is still several times the bandwidth-delay product
# and cannot bind. At a domestic consumer's ~20ms and a few hundred Mbit/s the
# BDP is the SAME ORDER as the derived window, which is the one place the
# budget can cost throughput. Measuring it anywhere else answers nothing.
#
# A/B/A/B rather than A/B: this path drifts by more than the effect being
# looked for, so each ON needs an OFF beside it. Every arm prints the instance
# allowance delta and the path the tunnel actually used, so a shaped arm or an
# arm that quietly fell back to the relay is visible instead of averaged in.
#
# usage: srv/budget_ws.sh [budget]        (default 512MiB)
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

BUDGET="${1:-512MiB}"
NSTREAM="${NSTREAM:-4}"; BURST="${BURST:-10}"; COOL="${COOL:-60}"
HERE="$(cd "$(dirname "$0")" && pwd)"

burst_dl() {
    local l=$1 tmp i; tmp=$(mktemp -d)
    for i in $(seq "$NSTREAM"); do
        ( curl -s -o /dev/null --max-time "$BURST" -r 0-1073741823 \
              -w '%{size_download}\n' "https://$l.$GW/big1g.bin" > "$tmp/$i" 2>/dev/null ) &
    done
    wait 2>/dev/null
    LC_ALL=C awk -v s="$BURST" '{t+=$1} END{printf "%.2f", t/1048576/s}' "$tmp"/*
    rm -rf "$tmp"
}

# The report goes to stderr and ONLY the rate to stdout: an earlier version
# echoed both and the caller's command substitution swallowed the report,
# losing two of the four arms.
arm() {
    local tag=$1 l e0 e1 v i
    l=$(label bq)
    vm "setsid nohup $VM_HOME/bore vhost 127.0.0.1:$DUFS_PORT --subdomain $l --id $l \
        --to '$BORE_TO' --secret '$BORE_SECRET' --udp --carriers 1 \
        > $VM_HOME/out/$l.log 2>&1 < /dev/null & true" >/dev/null 2>&1
    for i in $(seq 70); do present "$l" && break; sleep 0.5; done
    present "$l" || { echo "    $tag: REGISTRATION FAILED" >&2; printf 'nan'; return 1; }
    curl -fsS -o /dev/null -m 30 -r 0-1023 "https://$l.$GW/big1g.bin" 2>/dev/null
    sleep "$COOL"
    e0=$(ena); v=$(burst_dl "$l"); e1=$(ena)
    printf '    %-10s %6s MB/s (%4s Mbit/s)  allowance +%s  path=%s\n' \
        "$tag:" "$v" "$(mbps "$v")" "$(( ${e1:-0} - ${e0:-0} ))" "$(fld "$l" current_path)" >&2
    vm "pkill -9 -f 'subdomain $l' 2>/dev/null; true" >/dev/null 2>&1
    printf '%s' "$v"
}

say "BORE_UDP_MEMORY_BUDGET=$BUDGET from the domestic consumer, QUIC direct, A/B/A/B"
vm "$VM_HOME/vm_dufs_setup.sh start" 2>&1 | sed 's/^/    /'
for _ in $(seq 20); do
    p=$("$HERE/../res/bw_probe.sh" 2>&1 | tail -1); echo "  gate: $p"
    case "$p" in *AVAILABLE*) break;; esac
    sleep 120
done

"$HERE/setenv.sh" set BORE_UDP_MEMORY_BUDGET "$BUDGET" "F-13 aggregate bound" >/dev/null 2>&1
sleep 5; on1=$(arm on_1)
"$HERE/setenv.sh" unset BORE_UDP_MEMORY_BUDGET >/dev/null 2>&1
sleep 5; of1=$(arm off_1)
"$HERE/setenv.sh" set BORE_UDP_MEMORY_BUDGET "$BUDGET" "F-13 aggregate bound" >/dev/null 2>&1
sleep 5; on2=$(arm on_2)
"$HERE/setenv.sh" unset BORE_UDP_MEMORY_BUDGET >/dev/null 2>&1
sleep 5; of2=$(arm off_2)

echo "  ON  : $on1 $on2  -> median $(printf '%s\n%s\n' "$on1" "$on2" | med) MB/s"
echo "  OFF : $of1 $of2  -> median $(printf '%s\n%s\n' "$of1" "$of2" | med) MB/s"
say done
