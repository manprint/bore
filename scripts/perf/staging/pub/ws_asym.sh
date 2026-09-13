#!/usr/bin/env bash
# Answers ONE question the W1/W2 tables raise and cannot answer themselves:
# the workstation pulls at ~27 MB/s and pushes at ~71 MB/s through the SAME
# tunnel, on the same radio link, in the same minutes. Either the radio is
# asymmetric that way, or the DOWNLOAD direction is the one the instance shapes
# — download is server-OUTBOUND, and a t4g.micro's outbound allowance is the
# confounder this whole campaign is built around. So measure one arm of each
# direction with the server's own allowance counters read immediately before
# and after, which is the only way to tell the two apart.

# TRANSFER SIZE, AND WHY IT IS NOW A VARIABLE
# -------------------------------------------
# The 96 MiB in this stage was sized for WiFi, where it lasted about two
# seconds. Wired the same transfer lasts 0.83 s at 922 Mbit/s, most of it TCP
# slow start, so the number it produces is a RAMP rather than a rate -- and the
# spread says so: the wired run of this campaign's public stages produced paired
# ratios from 0.623 to 1.323 on arms that should have agreed. `XFER_MB` is the
# TOTAL moved per arm; the wired default is 384 MiB (~3.3 s at line rate) and
# `XFER_MB=96` reproduces the original figures exactly.
#
# WHY IT NOW REPEATS, AND WHY THE ORDER ALTERNATES
# ------------------------------------------------
# This stage decides which of two explanations owns the download/upload
# asymmetry -- the LINE, or the instance's outbound allowance -- and it used to
# decide it from ONE sample of each direction. One sample cannot separate a 20 %
# effect from noise, and V-11's rule is that a harness printing a summary
# statistic must print the raw samples beside it; with a single sample there is
# no statistic at all. It now runs `REPS` (3) paired repetitions and prints the
# median AND every sample.
#
# The ORDER alternates because a fixed order is itself a confounder here: with
# download always first, the download arm always runs on an allowance bucket
# refilled by the previous cooldown while the upload arm always runs 75 s later
# -- exactly the asymmetry the stage is trying to attribute. Odd repetitions
# measure download first, even ones upload first, so that term cancels.
#
# The cost is deliberate: repeating triples the billed bytes of the stage
# (download is AWS egress; upload is free), about 0.75 extra GiB. A number that
# cannot be defended is worth less than the bandwidth it took to produce.

# THE STATISTIC THIS DESIGN PAYS FOR IS THE PAIRED RATIO, NOT THE RATIO OF
# THE MEDIANS -- and this stage printed the wrong one of the two.
# ---------------------------------------------------------------------------
# Pairing is the whole reason the two arms run inside one repetition: drift
# cancels only when it is common to both. Dividing the download MEDIAN by the
# upload MEDIAN throws that away -- MEASURED here, it divided rep 1's download
# by rep 2's upload, two arms about five minutes apart, and read 1.191 where
# every repetition taken whole reads 1.060, 1.250 and 1.116 (median 1.116).
# The paired median is the headline; the ratio of the medians is still printed
# so a reader can line this run up against the single-sample runs that came
# before it, labelled as what it is.
#
# WHERE THE REFERENCE FOR THAT RATIO LIVES, AND WHY IT IS NOT HERE
# ---------------------------------------------------------------
# A tunnel down/up ratio means nothing on its own: the LINE is asymmetric too
# (V-9 measured this workstation at ~925 Mbit/s down and ~740 up, i.e. 1.25).
# The comparison is therefore this stage's paired median against
# `asym_qualify.sh`, which offers the same load to three independent
# destinations and so can tell a limit that follows the SOURCE from one that
# belongs to a destination. It is not duplicated here for the same reason
# `ws_conns.sh` does not duplicate its own control: one measurement, one
# owner. It does mean the two must be run in the SAME window -- a line
# qualified yesterday is not the reference for a ratio measured today.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
XFER_MB="${XFER_MB:-384}"
REPS="${REPS:-3}"
RP=5053; P=9031; PER=$(( XFER_MB*1048576/4 ))

vm "timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP'" >/dev/null 2>&1 || { echo "origin not serving on the VM"; exit 2; }
vm "setsid nohup \$HOME/bore local $RP --port $P --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 \
    > \$HOME/out/wsasym.log 2>&1 </dev/null & true" >/dev/null 2>&1
for i in $(seq 80); do adm tunnels | jq -e --argjson p "$P" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 && break; sleep 0.5; done
trap 'vm "pkill -9 -f \"local $RP --port $P\" 2>/dev/null; true" >/dev/null 2>&1' EXIT

# NOTE the array is SAMP and not S, and that is not a style choice.
# `lib.sh` publishes the scalar `S="$BORE_SRV"` -- the staging server's address.
# `declare -A S` on an existing SCALAR does not replace it: bash keeps the old
# value as element **[0]**. So every stage that named its sample array `S`
# printed the server's real IP address into its own raw-samples block, under the
# key `0`, in every run. MEASURED in `pub_ws_conns_r2.out`, where it sat between
# the quic and relay samples. It never reached a median (`med()` refuses a
# non-numeric sample -- that guard earned its keep here), but it reached an
# EVIDENCE FILE, which is precisely how coordinates escape: through prose and
# output, never through code.
declare -A SAMP
# A FAILED arm is a fact, not a zero: it is printed in the raw block and kept
# out of every median (the `asym_qualify` lesson -- a rate of 0 published
# because the instrument fetched nothing is the most expensive way a harness
# can be wrong).
keep() { case "$1" in ''|*[!0-9.]*) return 1;; *) return 0;; esac; }

# `arm` publishes its rate in LAST_MBS so the repetition can pair the two arms
# it just ran. Pairing by INDEX into the flat sample arrays would look simpler
# and would be wrong: `keep` drops a failed arm, so one failure silently shifts
# every later download against a different repetition's upload -- the exact
# class of error this rewrite is fixing, reintroduced one level down.
LAST_MBS=""
arm() { # <verb> <label>
    local i0 o0 i1 o1 res mbs
    i0=$(ena); o0=$(ena_out)
    res=$(python3 "$RAWCLI" "$1" "$BORE_GW" "$P" "$PER" 4 2>&1 | tr -d '\r')
    i1=$(ena); o1=$(ena_out)
    mbs=$(grep -oE 'MBs=[0-9.]+' <<<"$res" | cut -d= -f2)
    LAST_MBS="$mbs"
    keep "$mbs" && SAMP["$2"]+=" $mbs"
    printf '  %-9s %s\n' "$2" "$res"
    printf '  %-9s allowance delta: bw_in_exceeded=%s bw_out_exceeded=%s\n' \
        "" "$(( ${i1:-0} - ${i0:-0} ))" "$(( ${o1:-0} - ${o0:-0} ))"
}

echo "=== allowance attribution, relay tunnel on port $P, ${XFER_MB} MiB / 4 conns ==="
echo "  $REPS paired repetitions, arm order alternating, ${XFER_MB} MiB per arm"
echo
RATIOS=""
for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    d=""; u=""
    if [ $((rep % 2)) -eq 1 ]; then
        arm get "download"; d="$LAST_MBS"; cool 75; arm put "upload";   u="$LAST_MBS"
    else
        arm put "upload";   u="$LAST_MBS"; cool 75; arm get "download"; d="$LAST_MBS"
    fi
    if keep "$d" && keep "$u"; then
        r=$(LC_ALL=C awk -v d="$d" -v u="$u" 'BEGIN{ if (u+0>0) printf "%.3f", d/u }')
        keep "$r" && RATIOS+=" $r"
        printf '  %-9s paired down/up = %s\n' "" "${r:-n/a}"
    else
        # Printed, never silently skipped: a repetition that lost an arm has no
        # ratio, and saying so is what keeps the count of ratios honest.
        printf '  %-9s paired down/up = n/a (an arm failed this repetition)\n' ""
    fi
    [ "$rep" -lt "$REPS" ] && cool 75
done

echo
echo "=== medians (MB/s) ==="
for k in download upload; do
    printf '  %-9s %s\n' "$k" "$(printf '%s\n' ${SAMP["$k"]:-} | med)"
done
echo
echo "=== down/up ==="
printf '  paired ratios        %s\n' "${RATIOS:-  (none)}"
printf '  median of PAIRED     %s   <- the statistic this design pays for\n' \
    "$(printf '%s\n' ${RATIOS:-} | med)"
RM=$(LC_ALL=C awk -v d="$(printf '%s\n' ${SAMP[download]:-} | med)" \
                  -v u="$(printf '%s\n' ${SAMP[upload]:-} | med)" \
    'BEGIN{ if (d+0>0 && u+0>0) printf "%.3f", d/u }')
printf '  ratio of the medians %s   (kept only for continuity with the older\n' "${RM:-n/a}"
echo   "                              single-sample runs. It can divide one"
echo   "                              repetition download by ANOTHER repetition"
echo   "                              upload, so it is NOT the answer.)"
echo   "  reference: the LINE own asymmetry, from asym_qualify.sh run in THIS window"

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for k in download upload; do printf '  %-9s%s\n' "$k" "${SAMP["$k"]:-  (none)}"; done
# THE LEGEND USED TO NAME ONE LEG OF A TWO-LEG PATH, AND THAT MISREADS THE
# ALLOWANCE DELTAS ABOVE IT. The tunnel is a RELAY: every byte of either arm
# crosses the server TWICE -- in from the VM and out to the workstation for a
# download, the other way round for an upload. So both ENA counters are loaded
# in both arms, and `bw_in_exceeded` firing during a DOWNLOAD is not a
# contradiction: it convicts the VM -> server ingest leg of that download.
echo "  (relay path: workstation <-> server <-> VM. A download loads server-IN"
echo "   from the VM AND server-OUT to the workstation; an upload loads both the"
echo "   other way round. An allowance delta names the LEG, not the direction.)"
echo "  Allowance rule: a delta of ZERO across an arm rules the instance token"
echo "  bucket out FOR THAT ARM. A nonzero delta convicts it, and that arm is a"
echo "  budget measurement, not a tunnel measurement: read it, never average it."
