#!/usr/bin/env bash
# §42's open question, and nothing else: WHY does the same public-ladder cell
# stop repeating once it carries four connections?
#
# THE SHAPE OF THE QUESTION
# -------------------------
# `ws_conns_procs` measured an escursione of 19-48 % (median 38,6 %) on cells at
# n=4 and n=8, against 2,4-5,9 % at n=1 and n=2. That killed §36's conclusion --
# two non-overlapping triples are an ORDINARY event at that dispersion -- and it
# left the public ladder citable only up to n=2. The candidates §42.4 names are
# three, and none of them had been measured:
#
#   1. the server's public accept path under concurrency
#   2. the instance's scheduling (the hypervisor taking the core away)
#   3. micro-bursting of the ENA allowance bucket
#
# This stage separates 2 and 3 by MEASURING them, per repetition, as deltas
# around the very transfer whose rate is being recorded. What survives is 1,
# by elimination -- which is an honest way to reach it only because the other
# two are read from the kernel and the hypervisor rather than assumed.
#
# WHY `steal` IS IN HERE AND NOT JUST THE ALLOWANCE COUNTERS
# -----------------------------------------------------------
# "Instance scheduling" is not a metaphor: on a shared-tenancy instance the
# hypervisor's decision to run somebody else is reported by the guest kernel,
# in field 9 of /proc/stat's `cpu` line. A campaign that lists scheduling as a
# candidate and then does not read the one counter that reports it has not
# looked. `origin_cpu` excluded CPU as a SATURATION story (3-20 % of a core);
# steal is the other question entirely -- not "was the CPU busy" but "was the
# CPU TAKEN" -- and a 38 % excursion needs only tens of milliseconds of it in
# the wrong place.
#
# THE CONTROL IS IN THE SAME RUN, AND IT IS THE POINT
# -----------------------------------------------------
# §42.3 compares n=1 (stable) with n=4 (unstable) ACROSS runs, hours apart. So
# "the variance appears at n>=4" and "that afternoon was noisy" are the same
# measurement. This stage runs the n=1 cell inside the same repetitions, on the
# same line, in the same hour. Two outcomes, and they are opposite:
#
#   * n=1 tight, n=4 wide  -> the rung really is the variable; §42.3 stands.
#   * both wide            -> §42.3 is about the DAY, not the rung, and the
#                             ladder's instability is a property of the line at
#                             this hour. That would retire §42.3 as written.
#
# An in-run control cannot be talked out of either answer, which is the whole
# reason for the extra fourteen minutes it costs.
#
# WHAT THIS STAGE DELIBERATELY DOES NOT DO
# -----------------------------------------
# It does not walk a ladder. Adding rungs buys resolution in the wrong axis:
# the quantity under study is the DISPERSION of one cell, and dispersion needs
# repetitions, not neighbours. The rungs are exactly two, and the second one is
# a control.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"

RP=5053                      # raw_origin.py on the VM (already running)
R=9061; Q=9062               # ports of this stage's two public tunnels
AGG_MB="${AGG_MB:-460}"      # bytes per RUNG, so both rungs run for a
                             # comparable number of seconds (V-19, and the
                             # correction ws_conns.sh carries in full)
REPS="${REPS:-15}"           # repetitions of the cell under study
N_HI="${N_HI:-4}"            # the cell under study: the first unstable rung
N_LO="${N_LO:-1}"            # the in-run control: the rung §42.3 calls stable
CTRL_EVERY="${CTRL_EVERY:-3}" # run the control on one repetition in three
COOL="${COOL:-75}"

UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsvar-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT

# --- the counter snapshot ---------------------------------------------------
# Installed as a FILE on each host rather than passed as a quoted one-liner.
# Three levels of shell quoting around an awk program that itself needs `$1`
# is how a snapshot silently starts returning the empty string -- and an empty
# snapshot makes every delta read as 0, which is indistinguishable from the
# good news this stage exists to check for. A file has one level of quoting.
SNAP_SRC="$WORK/bore_snap.sh"
cat > "$SNAP_SRC" <<'SNAP_EOF'
#!/usr/bin/env bash
# One line of key=value pairs: every ENA allowance counter this instance
# publishes, plus the three /proc/stat fields needed to derive busy and steal.
I=$(ip route show default | awk '/^default/{print $5; exit}')
sudo -n ethtool -S "$I" 2>/dev/null | awk '/allowance_exceeded/{sub(/:/,"",$1); printf "%s=%s ", $1, $2}'
awk '/^cpu /{printf "tot=%d idle=%d steal=%d\n", $2+$3+$4+$5+$6+$7+$8+$9, $5, $9}' /proc/stat
SNAP_EOF

install_snap() {
    vmcp "$SNAP_SRC" "$BORE_VM_USER@$BORE_VM:/tmp/bore_snap.sh"  >/dev/null 2>&1 || return 1
    [ -n "$BORE_SRV" ] && { vmcp "$SNAP_SRC" "$BORE_SRV_USER@$BORE_SRV:/tmp/bore_snap.sh" >/dev/null 2>&1 || return 1; }
    return 0
}
snap_vm()  { vm  'bash /tmp/bore_snap.sh' 2>/dev/null; }
snap_srv() { srv 'bash /tmp/bore_snap.sh' 2>/dev/null; }

# `val <line> <key>` -- an ABSENT key yields the empty string, never 0.
# A missing counter and a counter reading zero mean opposite things, and this
# whole stage is an argument from zeros: every one of them has to be a zero
# somebody measured.
val() { printf '%s\n' "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2; exit}'; }

# `delta <before> <after> <key>` -- empty when either side lacked the key.
# A key present on ONE side only is not a delta of zero -- it is a snapshot
# that changed shape mid-run. Each side is checked on its own; concatenating
# them first makes a missing `before` indistinguishable from a present one.
delta() {
    local b a; b=$(val "$1" "$3"); a=$(val "$2" "$3")
    case "$b" in ''|*[!0-9]*) printf '?'; return;; esac
    case "$a" in ''|*[!0-9]*) printf '?'; return;; esac
    printf '%s' "$(( a - b ))"
}

# Allowance deltas of one host, compact, with a '?' for a counter the instance
# does not publish rather than a fabricated 0.
allow_line() { # before after
    printf 'in+%s out+%s pps+%s ct+%s' \
        "$(delta "$1" "$2" bw_in_allowance_exceeded)" \
        "$(delta "$1" "$2" bw_out_allowance_exceeded)" \
        "$(delta "$1" "$2" pps_allowance_exceeded)" \
        "$(delta "$1" "$2" conntrack_allowance_exceeded)"
}
# busy% and steal% of one host across the interval.
cpu_line() { # before after
    local dt di ds; dt=$(delta "$1" "$2" tot); di=$(delta "$1" "$2" idle); ds=$(delta "$1" "$2" steal)
    case "$dt" in ''|'?'|0) printf 'busy=? steal=?'; return;; esac
    LC_ALL=C awk -v t="$dt" -v i="$di" -v s="$ds" 'BEGIN{printf "busy=%.1f%% steal=%.2f%%", 100*(t-i)/t, 100*s/t}'
}
# Non-zero allowance anywhere in a line? Used only to FLAG a repetition, never
# to drop it: a shaped repetition is evidence about the shaping.
# A '?' is NOT hot: an absent counter is an unmeasured thing, and flagging it
# as shaping would invent the very evidence this stage is trying to read.
allow_hot() { printf '%s\n' "$1" | tr ' ' '\n' | awk -F+ '$2 ~ /^[0-9]+$/ && $2+0 > 0 {h=1} END{exit !h}'; }

per_conn() { printf '%s' "$(( AGG_MB * 1048576 / $1 ))"; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$(per_conn "$2")" "$2" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

declare -A SAMP    # SAMP[arm|n]  = space separated MB/s
ROWS=()            # one formatted line per cell, printed again at the end

echo "### public ladder -- the DISPERSION of one repeated cell -- $(date -Is)"
echo "  cell under study: n=$N_HI   in-run control: n=$N_LO (every $CTRL_EVERY reps)"
echo "  reps=$REPS  ${AGG_MB} MiB per rung  cooldown=${COOL}s  carriers=1"
echo "  per repetition, read as deltas AROUND the transfer: the server's and the"
echo "  VM's ENA allowance counters, and each host's busy/steal from /proc/stat."
echo

install_snap || { echo "INSTRUMENT FAILURE: could not install the counter snapshot on both hosts"; exit 2; }
# THE PREMISE OF EVERY DELTA BELOW. A snapshot that comes back empty makes
# every allowance delta read '?' and every steal read '?', which is honest --
# but it would also let the stage run for an hour and answer nothing. Check it
# once, loudly, before spending the hour.
_s=$(snap_srv); _v=$(snap_vm)
printf '  snapshot server: %s\n' "${_s:-<EMPTY>}"
printf '  snapshot vm    : %s\n' "${_v:-<EMPTY>}"
case "$(val "${_v:-}" tot)" in ''|*[!0-9]*) echo "INSTRUMENT FAILURE: the VM's snapshot carries no /proc/stat"; exit 2;; esac
if [ -z "$(val "${_s:-}" bw_in_allowance_exceeded)" ]; then
    echo "  NOTE: the server publishes no ENA allowance counters -- candidate 3 will"
    echo "        read '?' throughout and CANNOT be excluded by this run."
fi
echo

up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register"; exit 1; }

cell() { # arm port n rep
    local arm="$1" port="$2" n="$3" rep="$4"
    local sb vb sa va mbs t0 t1
    sb=$(snap_srv); vb=$(snap_vm); t0=$(date +%s.%N)
    mbs=$(g "$port" "$n")
    t1=$(date +%s.%N); sa=$(snap_srv); va=$(snap_vm)

    local sal val_ scpu vcpu secs flag=''
    sal=$(allow_line "$sb" "$sa"); val_=$(allow_line "$vb" "$va")
    scpu=$(cpu_line "$sb" "$sa"); vcpu=$(cpu_line "$vb" "$va")
    secs=$(LC_ALL=C awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.1f", b-a}')
    allow_hot "$sal" && flag='  <-- server allowance SHAPED'
    allow_hot "$val_" && flag="$flag  <-- vm allowance SHAPED"

    [ -n "${mbs:-}" ] && SAMP["$arm|$n"]+=" $mbs"
    local row
    row=$(printf '    rep %-3s n=%-2s %-5s %-9s %5ss | srv %s %s | vm %s %s%s' \
        "$rep" "$n" "$arm" "${mbs:-FAILED}" "$secs" "$sal" "$scpu" "$val_" "$vcpu" "$flag")
    echo "$row"; ROWS+=("$row")
    cool "$COOL"
}

for rep in $(seq 1 "$REPS"); do
    # Arm order alternates so a line that drifts inside a repetition shows up as
    # disagreement between repetitions rather than as an arm effect.
    if [ $((rep % 2)) -eq 1 ]; then order="relay quic"; else order="quic relay"; fi
    for arm in $order; do
        case "$arm" in relay) p=$R;; quic) p=$Q;; esac
        cell "$arm" "$p" "$N_HI" "$rep"
    done
    if [ $((rep % CTRL_EVERY)) -eq 1 ]; then
        for arm in $order; do
            case "$arm" in relay) p=$R;; quic) p=$Q;; esac
            cell "$arm" "$p" "$N_LO" "$rep"
        done
    fi
done

# --- the answer -------------------------------------------------------------
# `escursione` is (max - min) / median, the SAME definition §42.2 and §42.3 use,
# so the numbers below can be read straight against them.
spread() { # samples on stdin
    LC_ALL=C awk '
        /^[0-9]+(\.[0-9]+)?$/ { v[++n] = $1 + 0 }
        END {
            if (n == 0) { print "n/a n/a n/a n/a 0"; exit }
            for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++)
                if (v[j] < v[i]) { t=v[i]; v[i]=v[j]; v[j]=t }
            med = (n % 2) ? v[(n+1)/2] : (v[n/2] + v[n/2+1]) / 2
            printf "%.2f %.2f %.2f %.1f%% %d", v[1], med, v[n], 100*(v[n]-v[1])/med, n
        }'
}

echo
echo "=== the dispersion of the repeated cell (MB/s aggregate) ==="
printf '  %-6s %-3s %9s %9s %9s %11s %4s\n' arm n min median max escursione reps
for n in "$N_HI" "$N_LO"; do
    for arm in relay quic; do
        # shellcheck disable=SC2086
        # The split into five positional parameters IS the point here.
        # shellcheck disable=SC2046,SC2086
        set -- $(printf '%s\n' ${SAMP["$arm|$n"]:-} | spread)
        printf '  %-6s %-3s %9s %9s %9s %11s %4s\n' "$arm" "$n" "$1" "$2" "$3" "$4" "$5"
    done
done

echo
echo "=== every cell again, in one block (this is the evidence; the table is a summary) ==="
printf '%s\n' "${ROWS[@]:-  (no cells ran)}"

echo
echo "=== raw samples ==="
for k in "${!SAMP[@]}"; do printf '  %-12s%s\n' "$k" "${SAMP[$k]}"; done | sort

echo
echo "=== how to read it ==="
echo "  The control first. If n=$N_LO is tight (a few per cent) while n=$N_HI is wide,"
echo "  the RUNG is the variable and §42.3 stands. If BOTH are wide, §42.3 was"
echo "  measuring the day and not the rung, and it is retired as written."
echo
echo "  Then the two candidates, per repetition, beside the rate that repetition"
echo "  produced:"
echo "    * allowance -- a slow repetition carrying a non-zero delta implicates the"
echo "      token bucket. All-zero deltas across a wide spread EXCLUDE it: the"
echo "      instance did not notice the traffic that varied by 38 %."
echo "    * steal -- the hypervisor taking the core is the only form 'instance"
echo "      scheduling' can take that the guest can see. Near-zero steal on the"
echo "      slow repetitions excludes it in the same way."
echo "  Whatever both columns exclude leaves the server's public accept path under"
echo "  concurrency as the surviving candidate -- BY ELIMINATION, which is worth"
echo "  something only because the other two were read and not assumed."

measured=0
for n in "$N_HI" "$N_LO"; do for arm in relay quic; do
    [ -n "${SAMP["$arm|$n"]:-}" ] && measured=$((measured + 1))
done; done
if [ "$measured" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: no cell produced a single usable sample."
    exit 2
fi

echo
echo "DONE"
