#!/usr/bin/env bash
# Q1: WHOSE limit is the upload ceiling? -- qualifying the access link's asymmetry
#
# THE QUESTION, AND WHY IT IS NOT ACADEMIC
# ----------------------------------------
# Three campaigns in this repository independently concluded that this
# workstation's link is "asymmetric upward" -- that it uploads roughly twice as
# fast as it downloads -- and each of them correctly ruled out the AWS
# instance's allowance bucket by reading `bw_in_allowance_exceeded` /
# `bw_out_allowance_exceeded` around the arms and finding zero. The attribution
# ("the asymmetry belongs to the workstation's own link") was therefore right.
# The CHARACTERISATION was not: all three ran over WiFi.
#
#   vhost  2026-09-10   download 390 Mbit/s   upload 885
#   public 2026-09-11   download 208-368      upload 544-576
#   vpn    2026-09-12   download 150-416      upload 414-705
#   WIRED  2026-09-12   download 930          upload 737   <- asymmetric DOWNWARD
#
# One of those numbers refuses to fit: the vhost campaign measured **885 Mbit/s
# of upload over WiFi**, which is HIGHER than the 737 the wired link just
# produced. A radio cannot beat the cable it shares an uplink with. So either
# that 885 went somewhere other than the test VM, or the 737 is not the home
# uplink at all but the FAR END's ingress -- and if it is the far end's, then
# every "percent of bare" this campaign is about to compute has a reference that
# belongs to the VM rather than to the line.
#
# A tunnel measured against the wrong reference produces a ratio that is precise
# and meaningless. That is what this stage exists to prevent.
#
# METHOD
# ------
# The discriminator is DESTINATION. A limit that follows the source (the home
# uplink, the NIC, the kernel) reads the same toward every destination; a limit
# that belongs to one destination does not. So the same upload is offered to:
#
#   1. the test VM over iperf3          -- the reference every stage compares to
#   2. a public iperf3 server           -- different provider, different path
#   3. Cloudflare's upload endpoint     -- different protocol (HTTPS POST) too
#
# and the download is read from the same three, so the asymmetry is measured
# rather than inferred from one direction.
#
# The VM's ENA allowance counters are read as a DELTA immediately around each
# VM arm -- before and after, never once at the end -- because a counter that is
# nonzero at the end cannot say which arm spent it. Zero across an arm is what
# rules the instance out; the earlier campaigns did exactly this and it is why
# their attribution survives even though their numbers do not.
#
# P=1 against P=8 is kept on every endpoint, because it is the one-line
# diagnosis (V-9): a per-flow limit -- window, loss, Mathis -- opens with
# parallelism and a policer or a hard link rate does not.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/lib.sh"
export LC_ALL=C

SECS="${SECS:-8}"
REPS="${REPS:-2}"
IPERF_PORT="${IPERF_PORT:-5299}"
# A public iperf3 endpoint. Overridable, and its absence is reported loudly
# rather than silently skipped -- a missing second opinion is the whole point.
# A list, not one host: the second opinion is the whole point of this stage,
# so a single unreachable server must not silently remove it. European first
# (a 150 ms transatlantic RTT would cap a single stream for reasons that have
# nothing to do with this link).
PUBLIC_IPERF_LIST="${PUBLIC_IPERF_LIST:-speedtest.serverius.net:5002 ping.online.net:5201 iperf.par2.as49434.net:9200 iperf.he.net:5201}"
PUBLIC_IPERF=""; PUBLIC_IPERF_PORT=""
CF_UP="${CF_UP:-https://speed.cloudflare.com/__up}"
CF_DOWN="${CF_DOWN:-https://speed.cloudflare.com/__down?bytes=400000000}"

echo "### access-link asymmetry qualification -- $(date -Is)"
echo "  egress: $(ip route get 1.1.1.1 2>/dev/null | head -1)"
nic=$(ip route show default | awk '/^default/{print $5; exit}')
echo "  nic: $nic speed=$(cat /sys/class/net/"$nic"/speed 2>/dev/null) Mbit/s duplex=$(cat /sys/class/net/"$nic"/duplex 2>/dev/null)"

# Pick the second endpoint before measuring anything, and say which one.
for cand in $PUBLIC_IPERF_LIST; do
    h="${cand%:*}"; pt="${cand##*:}"
    if timeout 12 iperf3 -c "$h" -p "$pt" -t 1 -J >/dev/null 2>&1; then
        PUBLIC_IPERF="$h"; PUBLIC_IPERF_PORT="$pt"; break
    fi
done
if [ -n "$PUBLIC_IPERF" ]; then
    echo "  second endpoint: $PUBLIC_IPERF:$PUBLIC_IPERF_PORT"
else
    echo "  second endpoint: NONE REACHABLE -- the destination discriminator is"
    echo "                   reduced to Cloudflare alone; say so in any conclusion."
fi
echo

ena() {
    vm "cat /sys/class/net/*/device/ethtool_stats 2>/dev/null \
        || ethtool -S \$(ip route show default | awk '/^default/{print \$5; exit}') 2>/dev/null" 2>/dev/null \
      | awk '/bw_in_allowance_exceeded|bw_out_allowance_exceeded|pps_allowance_exceeded/{printf "%s=%s ", $1, $2}'
}

vm_iperf() { # dir secs par -> Mbit/s
    local dir="$1" secs="$2" par="$3" flag=""
    [ "$dir" = download ] && flag="-R"
    timeout $((secs + 25)) iperf3 -c "$BORE_VM" -p "$IPERF_PORT" $flag -P "$par" -t "$secs" -J 2>/dev/null \
      | jq -r '.end.sum_received.bits_per_second/1e6 | floor' 2>/dev/null
}

pub_iperf() {
    local dir="$1" secs="$2" par="$3" flag=""
    [ "$dir" = download ] && flag="-R"
    timeout $((secs + 30)) iperf3 -c "$PUBLIC_IPERF" -p "$PUBLIC_IPERF_PORT" $flag -P "$par" -t "$secs" -J 2>/dev/null \
      | jq -r '.end.sum_received.bits_per_second/1e6 | floor' 2>/dev/null
}

# Cloudflare, via the interface counters rather than curl's own arithmetic: the
# counter is the kernel's view of what crossed the wire and cannot be inflated
# by overlapping per-stream timers, which is how a "1089 Mbit/s" aggregate once
# appeared on a 1000 Mbit/s NIC.
# A ZERO HERE USED TO MEAN "the instrument failed", and printed as a number.
# MEASURED 2026-09-12: the download arm reported `0` in all four cells while the
# upload arm reported 778-780 -- the NIC counter delta was genuinely ~0, i.e.
# curl fetched nothing (endpoint changed, blocked, or answered an error page)
# and the stage published a zero that reads as "this link cannot download".
# Publishing a failure as a measurement is the most expensive way a harness can
# be wrong, so curl's OWN accounting is now cross-checked against the kernel's:
# if curl moved essentially nothing, the cell prints FAILED plus the HTTP code
# it got, and `add` (which skips empty values) keeps it out of every median.
cf() {
    local dir="$1" secs="$2" par="$3" i r1 r2 t1 t2 pids=() tmp moved=0 codes=""
    tmp=$(mktemp -d)
    r1=$(cat /sys/class/net/"$nic"/statistics/rx_bytes)
    t1=$(cat /sys/class/net/"$nic"/statistics/tx_bytes)
    for i in $(seq 1 "$par"); do
        if [ "$dir" = download ]; then
            curl -s -o /dev/null --max-time "$secs" \
                 -w '%{http_code} %{size_download}\n' "$CF_DOWN" > "$tmp/$i" 2>/dev/null &
        else
            head -c 200000000 /dev/zero 2>/dev/null \
              | curl -s -o /dev/null --max-time "$secs" -X POST --data-binary @- \
                     -w '%{http_code} %{size_upload}\n' "$CF_UP" > "$tmp/$i" 2>/dev/null &
        fi
        pids+=($!)
    done
    wait "${pids[@]}" 2>/dev/null
    r2=$(cat /sys/class/net/"$nic"/statistics/rx_bytes)
    t2=$(cat /sys/class/net/"$nic"/statistics/tx_bytes)
    for i in $(seq 1 "$par"); do
        read -r code sz < "$tmp/$i" 2>/dev/null || { code=000; sz=0; }
        codes="$codes${codes:+,}${code:-000}"
        moved=$(( moved + ${sz%%.*} ))
    done
    rm -rf "$tmp"
    # One megabyte total across every stream is far below anything this link
    # produces in a second, so it can only be an error page or a refusal.
    if [ "$moved" -lt 1000000 ]; then
        printf 'FAILED(http=%s)' "$codes"
        return
    fi
    if [ "$dir" = download ]; then
        awk -v b=$((r2-r1)) -v s="$secs" 'BEGIN{printf "%d", b*8/s/1e6}'
    else
        awk -v b=$((t2-t1)) -v s="$secs" 'BEGIN{printf "%d", b*8/s/1e6}'
    fi
}

declare -A R
# A cell that is not a number never enters a median: FAILED is a fact and is
# printed in the table and the raw block, but averaging it in -- or silently
# dropping it -- are the two ways a broken cell becomes a good-looking result.
add() { case "$2" in ''|*[!0-9.]*) return 0 ;; esac; R["$1"]+=" $2"; }

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    for par in 1 8; do
        # --- the test VM, with the allowance counters bracketing each arm
        b=$(ena); d=$(vm_iperf download "$SECS" "$par"); a=$(ena)
        printf '    vm       download P=%-2s %-6s Mbit/s   allowance before[%s] after[%s]\n' \
               "$par" "${d:-FAILED}" "${b:-n/a}" "${a:-n/a}"
        [ -n "${d:-}" ] && add "vm|download|$par" "$d"
        sleep 2
        b=$(ena); u=$(vm_iperf upload "$SECS" "$par"); a=$(ena)
        printf '    vm       upload   P=%-2s %-6s Mbit/s   allowance before[%s] after[%s]\n' \
               "$par" "${u:-FAILED}" "${b:-n/a}" "${a:-n/a}"
        [ -n "${u:-}" ] && add "vm|upload|$par" "$u"
        sleep 2

        # --- a second, unrelated destination
        if [ -n "$PUBLIC_IPERF" ]; then
            d=$(pub_iperf download "$SECS" "$par"); sleep 2
            u=$(pub_iperf upload   "$SECS" "$par"); sleep 2
        else d=""; u=""; fi
        printf '    public   download P=%-2s %-6s   upload P=%-2s %-6s Mbit/s  (%s)\n' \
               "$par" "${d:-UNREACHABLE}" "$par" "${u:-UNREACHABLE}" "$PUBLIC_IPERF"
        [ -n "${d:-}" ] && add "public|download|$par" "$d"
        [ -n "${u:-}" ] && add "public|upload|$par" "$u"

        # --- a third destination AND a third protocol
        d=$(cf download "$SECS" "$par"); sleep 2
        u=$(cf upload   "$SECS" "$par"); sleep 2
        printf '    cloudflr download P=%-2s %-6s   upload P=%-2s %-6s Mbit/s  (kernel counters)\n' \
               "$par" "${d:-FAILED}" "$par" "${u:-FAILED}"
        [ -n "${d:-}" ] && add "cf|download|$par" "$d"
        [ -n "${u:-}" ] && add "cf|upload|$par" "$u"
    done
done

echo
echo "=== medians (Mbit/s) ==="
printf '  %-10s %-12s %-12s %-12s %-12s\n' endpoint 'down P=1' 'down P=8' 'up P=1' 'up P=8'
for ep in vm public cf; do
    printf '  %-10s %-12s %-12s %-12s %-12s\n' "$ep" \
      "$(printf '%s\n' ${R["$ep|download|1"]:-} | med)" \
      "$(printf '%s\n' ${R["$ep|download|8"]:-} | med)" \
      "$(printf '%s\n' ${R["$ep|upload|1"]:-}   | med)" \
      "$(printf '%s\n' ${R["$ep|upload|8"]:-}   | med)"
done

echo
echo "=== raw samples ==="
for k in "${!R[@]}"; do printf '  %-22s%s\n' "$k" "${R[$k]}"; done | sort

echo
echo "=== reading ==="
echo "  An upload ceiling that is the SAME toward all three destinations belongs to"
echo "  this end -- the home uplink, the NIC or the kernel -- and is the correct"
echo "  reference for every 'percent of bare' in the campaigns."
echo "  An upload ceiling that appears ONLY toward the test VM belongs to the VM's"
echo "  ingress, and every VPN/secret ratio computed against it is measuring the"
echo "  far end. The allowance deltas above say whether the instance noticed."
echo
echo "DONE"
