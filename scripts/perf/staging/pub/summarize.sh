#!/usr/bin/env bash
# Turn one collected campaign directory into the table that goes into the
# write-up. Reads only the stage logs, so it can be re-run months later on an
# archived run without the deployment being reachable.
#
#   summarize.sh out/pub-20260911-055454
#
# It deliberately reports the MEDIAN RATIO for paired arms and never a mean of
# independently-collected arms: on this instance the control arm drifts by more
# than most of the effects being measured, which is why the arms are paired in
# the first place.
set -uo pipefail
D="${1:?usage: summarize.sh <out/pub-TIMESTAMP>}"
[ -d "$D" ] || { echo "no such directory: $D" >&2; exit 2; }

hdr() { printf '\n== %s ==\n' "$*"; }
have() { [ -s "$D/$1" ]; }

echo "### campaign summary: $D"
if have BUILD.txt; then sed 's/^/  /' "$D/BUILD.txt"; fi

if have pub_ab_p1.log; then
    hdr "P1 relay TCP vs QUIC direct (paired; ratio > 1 means the RELAY is faster)"
    grep -E '=====|median ratio|^  [0-9]+ ' "$D/pub_ab_p1.log" | sed 's/^/  /'
fi
if have pub_ab_p2.log; then
    hdr "P2 carrier ladder"
    grep -E 'carriers=' "$D/pub_ab_p2.log" | sed 's/^/  /'
fi
if have pub_ab_p3.log; then
    hdr "P3 per-connection latency (one new TCP connection per probe, serial)"
    grep -E 'new-conn|path=' "$D/pub_ab_p3.log" | sed 's/^/  /'
fi
if have pub_ab_p4.log; then
    hdr "P4 HTTP over the public tunnel"
    grep -E 'rps=|dl 4x|path=' "$D/pub_ab_p4.log" | sed 's/^/  /'
fi
if have pub_flavours.log; then
    hdr "P5 native vs docker vs OpenSSH -R on the relay (rotated, budget-neutral)"
    grep -E 'live on public port|median|round|latency|port=' "$D/pub_flavours.log" | sed 's/^/  /'
fi
if have pub_flavours_udp.log; then
    hdr "P5b native vs docker on the QUIC direct path"
    grep -E 'live on public port|median|round|latency|port=' "$D/pub_flavours_udp.log" | sed 's/^/  /'
fi
if have pub_conc.log; then
    hdr "P6 concurrency: cost of a FRESH connection behind N held ones"
    grep -E '=====|baseline|held=|carriers=|path=' "$D/pub_conc.log" | sed 's/^/  /'
fi
if have pub_netem.log; then
    hdr "P8 netem matrix (ratio > 1 means QUIC direct is faster under that condition)"
    grep -E '^  [a-z]|condition' "$D/pub_netem.log" | sed 's/^/  /'
fi
if have pub_eff.log; then
    hdr "P9 CPU cost — pair each window with res/cpu_window.sh against pres_srv.stat"
    grep -E '^CASE|^START|^END' "$D/pub_eff.log" | sed 's/^/  /'
    if have pres_srv.stat; then
        echo "  --- server CPU per case (host /proc/stat, softirq included) ---"
        here="$(cd "$(dirname "$0")" && pwd)"
        grep '^CASE' "$D/pub_eff.log" | while read -r _ tag rest; do
            w=$(printf '%s' "$rest" | grep -oE 'window=[0-9]+-[0-9]+' | cut -d= -f2)
            g=$(printf '%s' "$rest" | grep -oE 'gib=[0-9.]+' | cut -d= -f2)
            [ -n "$w" ] || continue
            printf '  %-12s %s\n' "$tag" \
                "$("$here/../res/cpu_window.sh" "$D/pres_srv.stat" "${w%-*}" "${w#*-}" "$g" 2>/dev/null | head -1)"
        done
    fi
fi
if have pub_stab.log; then
    hdr "P7 stability"
    grep -E '=====|PASS|FAIL|after:|released|reaped|samples=|RSS|post-churn|rejections' "$D/pub_stab.log" | sed 's/^/  /'
fi
if have ena.timeline; then
    hdr "instance allowance over the run (non-zero deltas only)"
    # One line per sample: "<iso8601> bw_in=+<d> bw_out=+<d> conntrack=+<d> pps=+<d>".
    # Only samples with a NON-ZERO delta are worth reading: those are the
    # moments the instance was actually being shaped. Field names are read from
    # the line itself rather than by position, so a future counter added to
    # ena_timeline.sh does not silently shift the filter onto the wrong column.
    LC_ALL=C awk '{
        hit=0
        for (i=2;i<=NF;i++) { split($i,kv,"=+"); if (kv[2]+0 > 0) hit=1 }
        if (hit) { shaped++; if (shaped <= 40) print "  " $0 }
        total++
    }
    END { printf "  (shaped samples: %d of %d)\n", shaped, total }' "$D/ena.timeline"
fi

echo
echo "### end of summary"
