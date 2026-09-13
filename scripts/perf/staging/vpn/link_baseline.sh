#!/usr/bin/env bash
# V0: qualify the ACCESS LINK before believing any tunnel number.
#
# WHY THIS STAGE EXISTS
# ---------------------
# Every other stage in this directory quotes a tunnel figure as a percentage of
# a bare control sampled in the same repetition, which makes the RATIO valid on
# any link. It does not make the ABSOLUTE numbers meaningful: a campaign run
# while the access line is delivering a third of its usual download produces
# correct ratios attached to figures nobody should quote as the product's
# capability. Worse, a reader six months from now has no way to tell the two
# situations apart from the result file alone.
#
# This stage writes down what the line was doing, from sources that do not pass
# through bore at all, so that every result file in the same run can be
# re-qualified later. It is deliberately the CHEAPEST stage to run and should be
# run FIRST and LAST in a campaign.
#
# WHAT IT SEPARATES, AND WHY EACH SOURCE IS HERE
# ----------------------------------------------
#   local link   The PHY rate and the radio's own error counters, sampled as a
#                DELTA across a real download. A WiFi link that is dropping
#                frames and one that is merely slow look identical from a
#                throughput number and opposite from `tx retries`/`rx drop
#                misc`. Measured on 2026-09-12: +8 retries and +0 drops across
#                112 MB, which is what exonerated the radio when the download
#                had collapsed to a third of its usual figure.
#   ookla        The USER'S OWN oracle, and the only one that reaches a server
#                inside the ISP. A cap that survives against the subscriber's
#                own ISP server is upstream of everything this repository can
#                influence. Optional: set OOKLA_BIN, or leave it out.
#   cdn          Two public CDNs, single and parallel. A CDN is a poor oracle on
#                its own (it is frequently the bottleneck itself) but agreement
#                between a CDN, Ookla and iperf3 is strong evidence about the
#                line rather than about any one peer.
#   iperf3       The bare path this campaign's controls actually use, at P=1 and
#                P=8 WITH RETRANSMITS. The P=1 vs P=8 comparison is the whole
#                diagnosis in one line: a per-flow limit (window, loss, Mathis)
#                opens up with parallelism, a policer does not.
#   ena          The far end's own allowance counters. A burstable instance that
#                has been shaped reports it, and egress-from-VM is the DOWNLOAD
#                direction — the one most easily blamed on the tunnel.
#
# Usage: [SECS=10] [OOKLA_BIN=/path/to/speedtest] link_baseline.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../lib.sh"

SECS="${SECS:-10}"
IPERF_PORT="${IPERF_PORT:-5299}"
STAMP="$(date +%Y%m%dT%H%M%S)"
REC="$OUT/link-baseline-$STAMP.txt"

say() { echo "$@" | tee -a "$REC"; }

say "### access-link baseline -- $(date -Is)"
say

# --- 1. local link ----------------------------------------------------------
IFACE="$(ip -o -4 route show default | awk '{print $5}' | head -1)"
say "=== local egress interface ==="
say "  iface: ${IFACE:-none}"
if [ -n "$IFACE" ] && [ -d "/sys/class/net/$IFACE/wireless" ]; then
    say "  type : wireless"
    iw dev "$IFACE" link 2>/dev/null | grep -E 'SSID|freq|signal|bitrate' | sed 's/^/  /' | tee -a "$REC"
else
    say "  type : wired"
    ethtool "$IFACE" 2>/dev/null | grep -E 'Speed|Duplex' | sed 's/^/  /' | tee -a "$REC"
fi
say

# Radio counters are only meaningful as a delta across real traffic, so the
# snapshot is taken around the CDN probe below rather than in isolation.
wsnap() {
    [ -d "/sys/class/net/$IFACE/wireless" ] || { echo "0 0 0"; return; }
    iw dev "$IFACE" station dump 2>/dev/null | awk -F'\t' '
        /rx bytes/{rb=$3} /tx retries/{tr=$3} /rx drop misc/{rd=$3}
        END{print rb+0, tr+0, rd+0}'
}

# --- 2. CDN, single and parallel -------------------------------------------
CURL="$(command -v curl)"
cdn_par() { # <n> <url>
    local n="$1" u="$2" i
    for i in $(seq 1 "$n"); do
        "$CURL" -sS -o /dev/null -m 30 -w '%{speed_download}\n' "$u" &
    done | awk '{s+=$1} END{printf "%.1f", s*8/1e6}'
    wait
}
CF="https://cachefly.cachefly.net/100mb.test"
OV="https://proof.ovh.net/files/100Mb.dat"
say "=== public CDN download (Mbit/s aggregate) ==="
W0="$(wsnap)"
say "  cachefly  x1 : $(cdn_par 1 "$CF")"
say "  cachefly  x8 : $(cdn_par 8 "$CF")"
say "  ovh       x8 : $(cdn_par 8 "$OV")"
W1="$(wsnap)"
if [ -d "/sys/class/net/$IFACE/wireless" ]; then
    say "  radio delta across the probe (a clean radio shows ~0 drops):"
    echo "$W0 $W1" | awk '{printf "    rx bytes +%d   tx retries +%d   rx drop misc +%d\n", $4-$1, $5-$2, $6-$3}' | tee -a "$REC"
fi
say

# --- 3. Ookla, the subscriber's own oracle ----------------------------------
OK="${OOKLA_BIN:-$(command -v speedtest || true)}"
say "=== ookla (the user's own oracle; reaches a server inside the ISP) ==="
if [ -n "$OK" ] && [ -x "$OK" ]; then
    # Retried once on purpose: the first invocation on a fresh binary, and an
    # occasional server-side refusal, both produce empty output. A baseline
    # stage that silently reports "no result" for a transient is worse than no
    # stage, because the reader concludes the oracle disagreed.
    ok_json=""
    for _try in 1 2 3; do
        ok_json="$("$OK" --accept-license --accept-gdpr -f json 2>/dev/null)"
        [ -n "$ok_json" ] && break
        sleep 5
    done
    printf '%s' "$ok_json" | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: print("  (no result)"); raise SystemExit
print("  %-28s %5.1f ms  down %6.0f  up %6.0f Mbit/s  loss %s" % (
    d["server"]["name"][:28], d["ping"]["latency"],
    d["download"]["bandwidth"]*8/1e6, d["upload"]["bandwidth"]*8/1e6,
    d.get("packetLoss","n/a")))' | tee -a "$REC"
else
    say "  not run (set OOKLA_BIN=/path/to/speedtest to include it)"
fi
say

# --- 4. bare iperf3, the control every other stage uses ---------------------
say "=== bare iperf3 to the test VM (the control the tunnel stages compare against) ==="
LIST="$(vm "pkill -F ~/iperf3.pid 2>/dev/null; rm -f ~/iperf3.pid;
    setsid nohup iperf3 -s -p $IPERF_PORT > ~/iperf3.log 2>&1 < /dev/null & echo \$! > ~/iperf3.pid; sleep 1;
    ss -lntp 2>/dev/null | grep -c ':$IPERF_PORT '" 2>/dev/null | tail -1)"
if [ "$LIST" != 1 ]; then
    say "  no iperf3 server on the VM -- bare control unavailable this run"
else
    # An iperf3 server serves ONE test at a time and refuses the next connection
    # for a moment after a test ends; back-to-back arms therefore fail with
    # "Connection reset by peer" on a perfectly healthy path. The gap and the
    # single retry are what stop a harness artefact from being read as a link
    # fault -- exactly the confusion this whole stage exists to prevent.
    arm() { # <label> <streams> [extra]
        local lab="$1" n="$2"; shift 2
        local j t
        for t in 1 2; do
            j="$(iperf3 -c "$BORE_VM" -p "$IPERF_PORT" -t "$SECS" -P "$n" -J "$@" 2>/dev/null)"
            # Retry only on a REAL failure. `jq -e .error` exits non-zero both
            # for a clean run (.error is null) and for unparseable output, so a
            # run that produced no JSON at all would otherwise look like a pass.
            printf '%s' "$j" | jq -e 'has("error")|not' >/dev/null 2>&1 && break
            sleep 4
        done
        printf '%s' "$j" | jq -r --arg l "$lab" \
            'if .error then "  \($l): FAILED (\(.error))"
             else "  \($l): \((.end.sum_received.bits_per_second/1e6)|round) Mbit/s  retr=\(.end.sum_sent.retransmits // "n/a")" end'
        sleep 3
    }
    # P=1 vs P=8 is the diagnosis: a per-flow limit opens up with parallelism,
    # a policer does not.
    arm "download P=1" 1 -R | tee -a "$REC"
    arm "download P=8" 8 -R | tee -a "$REC"
    arm "upload   P=1" 1    | tee -a "$REC"
    arm "upload   P=8" 8    | tee -a "$REC"
    vm "kill \$(cat ~/iperf3.pid) 2>/dev/null; rm -f ~/iperf3.pid" >/dev/null 2>&1
fi
say

# --- 5. the far end's own allowance ----------------------------------------
say "=== test VM: instance type and ENA allowance counters ==="
vm 'TOK=$(curl -sS -m 5 -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 60" 2>/dev/null);
    printf "  instance-type: %s\n" "$(curl -sS -m 5 -H "X-aws-ec2-metadata-token: $TOK" http://169.254.169.254/latest/meta-data/instance-type 2>/dev/null)";
    IF=$(ip -o -4 route show default | awk "{print \$5}");
    sudo -n ethtool -S "$IF" 2>/dev/null | grep allowance_exceeded | sed "s/^/  /"' 2>/dev/null | tee -a "$REC"

say
say "recorded: $REC"
