#!/usr/bin/env bash
# J3: does an OPERATOR'S SESSION survive what the network does to it?
#
# THE QUESTION THIS ANSWERS, AND WHY IT IS THE LAST ONE
# -----------------------------------------------------
# P6's first four questions are all about speed: how long to get in, how long a
# keystroke takes, what the transport costs, what carriers cost. The fifth is
# about whether the thing stays up, and it is the only one whose failure an
# operator experiences as "my session dropped" rather than as "this is slow".
#
# The product makes THREE promises here, all of them in CLAUDE.md, none of them
# previously measured on a real path:
#
#   1. "Missing/dead/open-failed QUIC falls back for the SAME channel to warm
#      TCP, NEVER kills the outer SSH session."
#   2. "One alias = exactly ONE Role::SshJumpHost admin row across carriers and
#      reconnect storms."
#   3. The session survives a rekey (the OpenSSH client's own RekeyLimit fires
#      first under load; both directions were verified non-wedging in-process).
#
# An in-process test cannot test the first: killing QUIC on loopback is not the
# same event as a path that stops delivering datagrams while TCP to the same
# host keeps working. That is what `vpn_tun_endpoint.sh blackhole` produces --
# an nft rule dropping UDP to and from ONE address, in its own table, so the
# warm TCP relay to the very same server stays up. It is the only mechanism on
# this workstation that makes the direct path die the way it dies in the field.
#
# WHAT IS MEASURED
# ----------------
#   phase 1  baseline   direct path verified from the admin API, session held
#   phase 2  blackout   UDP to the VM dropped; the session must NOT die, a NEW
#                       channel must still open (over the warm relay), and the
#                       server must report the path as relay within a budget
#   phase 3  recovered  blackhole removed; the path must return to direct, and
#                       the SAME session must still be alive to see it
#   phase 4  rekey      the session is held while the client's RekeyLimit fires,
#                       and a keystroke is measured on the far side of it
#
# Every phase samples the same keystroke latency, so the cost of running on the
# relay is a number in the same table as the proof that it did not break.
#
# WHAT A FAILURE LOOKS LIKE, DELIBERATELY
# ---------------------------------------
# Each check prints PASS or FAIL with the observation that decided it. A check
# that could not be evaluated prints SKIP and says why -- never PASS. That
# distinction is this campaign's most expensive lesson: a stage that cannot
# measure must say so, because "no failures observed" and "no observations" look
# identical in a summary table.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/jumplib.sh"

ROOTSH="$(cd "$HERE/../../../.." && pwd)/scripts/vpn_tun_endpoint.sh"

REPS="${REPS:-2}"
SAMPLES="${SAMPLES:-5}"
BLACKOUT_SECS="${BLACKOUT_SECS:-60}"
HOLD_SECS="${HOLD_SECS:-70}"          # long enough for a 20 s RekeyLimit to fire
# Budgets are DECLARED, not discovered after the fact. Fallback must be quick
# because the channel open is bounded at DIRECT_OPEN_TIMEOUT (3 s) and the QUIC
# idle timeout is 10 s; recovery rides the renewal path, which is slower.
FALLBACK_BUDGET="${FALLBACK_BUDGET:-30}"
RECOVER_BUDGET="${RECOVER_BUDGET:-90}"
JUMP_SERVER_EXTRA="${JUMP_SERVER_EXTRA:---udp --vhost-quic-port 7847}"

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
declare -A SAMP          # SAMP[phase]      = keystroke samples, all reps
declare -A RSAMP         # RSAMP[rep|phase]  = keystroke samples of ONE rep
declare -A WARM          # WARM[rep|phase]   = the discarded warm-up sample
declare -a CHECKS     # "PASS|name|observation"

note()  { CHECKS+=("$1|$2|$3"); printf '    [%s] %-26s %s\n' "$1" "$2" "$3"; }
keep()  { case "$1" in FAILED|"") return 1;; *) return 0;; esac; }

bh_on()  { sudo -n "$ROOTSH" blackhole on "$BORE_VM" >/dev/null 2>&1; }
bh_off() { sudo -n "$ROOTSH" blackhole off        >/dev/null 2>&1; }

cleanup() { bh_off; jump_cleanup; }
trap cleanup EXIT

# Wait for the server to report a path, returning the SECONDS it took (or
# FAILED). The elapsed time is the deliverable here, not a boolean: "it fell
# back" and "it fell back in 4 s" are different products.
wait_path_secs() {
    local want="$1" budget="$2" i
    for i in $(seq 1 "$budget"); do
        [ "$(jump_path)" = "$want" ] && { echo "$i"; return 0; }
        sleep 1
    done
    echo FAILED
}

# THE FIRST KEYSTROKE AFTER A SESSION EVENT IS NOT A KEYSTROKE MEASUREMENT.
#
# MEASURED (§44.9, and visible only because the raw samples are printed): the
# first sample of `recovered` read 246.2 ms and 213.1 ms in the two repetitions
# against ~147 and ~120 for every other sample in the same cell. One outlier in
# a ten-sample cell moved `recovered` to 1,21x baseline -- WORSE than the relay
# it was supposed to be a control for -- and the phase then read as "returning
# to direct costs more than the blackout", which is not a thing that happened.
#
# The event is real (a channel is opened over a path that has just changed, or
# a rekey has just completed) and the cost is real, but it is a ONE-OFF cost of
# the event and not the steady-state keystroke this table compares. So it is
# taken, PRINTED, and excluded from the cell -- excluded rather than averaged,
# because a per-phase median is the wrong statistic for a value that occurs
# exactly once. Discarding silently would be the worse fix: the warm-up column
# below is where "the path just switched" is actually visible.
sample_phase() { # <phase> <n> <rep>
    local ph="$1" n="$2" rep="${3:-0}" i e w
    w=$(jump_echo_ms)
    keep "$w" && WARM["$rep|$ph"]="$w"
    for i in $(seq 1 "$n"); do
        e=$(jump_echo_ms)
        if keep "$e"; then SAMP["$ph"]+=" $e"; RSAMP["$rep|$ph"]+=" $e"; fi
    done
    printf '    %-6s %-26s warm-up %-8s then%s\n' "" "(keystrokes: $ph)" \
        "${WARM["$rep|$ph"]:-FAILED}" "${RSAMP["$rep|$ph"]:- (none)}"
}

# A ControlMaster that LOGS, so the rekey phase has an oracle. The overriding
# options come FIRST: ssh takes the first value it obtains for each parameter,
# and JSSH_OPTS pins LogLevel=ERROR.
STAB_LOG="$WORK/jump-master-$JUMP_RUN_ID.log"
stab_master_open() {
    rm -f "$JUMP_CTL_SOCK" "$STAB_LOG"
    timeout 40 ssh -o LogLevel=DEBUG1 -o "RekeyLimit=256K 20" -E "$STAB_LOG" \
        "${JSSH_OPTS[@]}" -M -N -f \
        -o ControlPath="$JUMP_CTL_SOCK" -o ControlPersist=600 \
        -p "$JUMP_INNER_PORT" \
        -J "$JUMP_SSH_USER@$BORE_VM:$JUMP_SSH_PORT" \
        "$JUMP_INNER_USER@$JUMP_TARGET" >/dev/null 2>&1 || return 1
    [ -S "$JUMP_CTL_SOCK" ]
}
# `ssh -O check` asks the MASTER PROCESS whether it is alive, which is exactly
# the question: a socket file left behind by a dead master would answer a
# file test but not this one.
master_alive() {
    ssh -O check -o ControlPath="$JUMP_CTL_SOCK" "$JUMP_INNER_USER@$JUMP_TARGET" >/dev/null 2>&1
}
# A session that ended is not a session that can be reused: the control socket
# survives its master and every later `ssh -o ControlPath=...` against it fails,
# which is how ONE dead session turned into ten FAILs in the first run.
#
# IT MUST REOPEN WITH THIS STAGE'S OWN OPENER, not `jumplib`'s. `stab_master_open`
# is the one that sets `RekeyLimit` and `-E $STAB_LOG`, and the rekey phase's
# only oracle is that log. Reopening with the plain `jump_master_open` leaves a
# perfectly working session that can never be observed to rekey -- the check
# then prints SKIP forever, which is honest and useless.
master_reopen() {
    jump_master_close 2>/dev/null
    rm -f "$JUMP_CTL_SOCK"
    stab_master_open
}
# `grep -c` prints 0 AND exits 1 when it counts nothing, so a `|| echo 0` tail
# emits TWO lines and the later `-gt` comparison dies with an integer error.
# Capture, then default.
kexinits() {
    local n
    n=$(grep -c 'SSH2_MSG_KEXINIT sent' "$STAB_LOG" 2>/dev/null)
    echo "${n:-0}"
}

echo "### SSH jump host -- stability, fallback and rekey -- $REPS reps -- $(date -Is)"
echo "  gateway on the TEST VM (never staging); provider publishes this workstation's sshd"
echo "  blackout ${BLACKOUT_SECS}s   hold ${HOLD_SECS}s   budgets: fallback ${FALLBACK_BUDGET}s, recover ${RECOVER_BUDGET}s"
echo

# The blackhole is a root firewall rule. If it cannot be installed the stage has
# no experiment, and must say so instead of measuring an undisturbed path and
# calling it resilience.
echo "=== preflight: the blackhole mechanism ==="
if bh_on && [ "$(sudo -n "$ROOTSH" blackhole status 2>/dev/null)" = "2" ]; then
    bh_off
    echo "  blackhole installs and removes cleanly (2 drop rules)"
else
    bh_off
    echo "  FAILED: cannot install the UDP blackhole via $ROOTSH"
    echo "  Without it the direct path cannot be killed on a real network, so the"
    echo "  fallback promise is UNMEASURED -- not passing. Stage stops here."
    echo "DONE"
    exit 1
fi
echo

echo "=== provisioning ==="
jump_build_and_deploy || { echo "build/deploy failed"; exit 1; }
jump_start_gateway    || { echo "gateway failed";    exit 1; }
# THE PREMISE, before the measurement. `open_ms - wchan_ms` is the inner sshd's
# handshake and `wchan_ms - tcp_ms` is the gateway's own cost, so an absent
# inner sshd does not lose one column -- it corrupts the other by subtraction.
jump_ensure_inner_target || { echo "INSTRUMENT FAILURE: no inner SSH target"; exit 2; }
echo

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    pid=$(jump_start_provider "--udp") || { echo "    provider FAILED"; continue; }

    path=$(jump_wait_path direct 90)
    if [ "$path" != "direct" ]; then
        note SKIP "direct path" "never upgraded (path=$path) -- nothing to blackhole"
        kill -TERM "$pid" 2>/dev/null; sleep 3; continue
    fi
    rows_base=$(jump_rows)
    read -r opens0 fb0 carr0 <<<"$(jump_counters)"

    if ! stab_master_open; then
        note SKIP "session held open" "ControlMaster did not establish"
        kill -TERM "$pid" 2>/dev/null; sleep 3; continue
    fi
    sample_phase baseline "$SAMPLES" "$rep"
    kex0=$(kexinits)

    # ---- phase 2: the direct path dies underneath a live session -----------
    bh_on || { note SKIP "blackout" "blackhole refused mid-run"; jump_master_close
               kill -TERM "$pid" 2>/dev/null; sleep 3; continue; }

    fb=$(wait_path_secs relay "$FALLBACK_BUDGET")
    if [ "$fb" = FAILED ]; then
        note FAIL "path falls back to relay" "still not relay after ${FALLBACK_BUDGET}s (server says '$(jump_path)')"
    else
        note PASS "path falls back to relay" "${fb}s (budget ${FALLBACK_BUDGET}s)"
    fi

    # AN ESTABLISHED SESSION ON THE DIRECT PATH DOES NOT SURVIVE ITS TRANSPORT,
    # AND CANNOT. This check used to be a FAIL, and it was the harness asserting
    # a promise the product does not make. An `ssh -J` session is carried inside
    # ONE `direct-tcpip` channel, whose bytes ride a QUIC stream end to end; a
    # stream that dies mid-flight cannot be moved to the relay without losing
    # the bytes in flight, and SSH has no resumption to paper over the gap. The
    # invariant in CLAUDE.md is about the channel OPEN -- "missing/dead/
    # open-failed QUIC falls back for the SAME channel to warm TCP" -- not about
    # migrating an established one.
    #
    # MEASURED 2026-09-13, polled every 5 s through a blackout: master alive at
    # t=5 and t=10 with the server still reporting `direct`, gone at t=15 in the
    # same sample where the path flipped to `relay`. It ends WITH the path, not
    # before it and not after it, which is what identifies the cause.
    #
    # So it is recorded as an OBSERVATION, not a verdict: `note OBSV` prints and
    # is counted in neither the PASS nor the FAIL column (see the tally).
    if master_alive; then
        note OBSV "established session" "still answering while the path is '$(jump_path)'"
    else
        note OBSV "established session" "ended with the direct path (expected: one channel, one QUIC stream)"
    fi
    # The socket outlives its master and poisons every later reuse. Clear it.
    jump_master_close 2>/dev/null; rm -f "$JUMP_CTL_SOCK"

    # THE ACTUAL PROMISE: with UDP dead, a FRESH session must still open, over
    # the warm TCP relay, at the ordinary cost. This is what an operator does
    # when a session drops -- they reconnect -- and it must not wait for the
    # tunnel to re-register.
    #
    # It must be a NEW session and not `jump_chan_ms`: a new CHANNEL is opened
    # through the ControlMaster socket, and that master is the one that just
    # died with the transport. The first run asked the corpse for a channel and
    # convicted the product when it did not answer.
    o=$(jump_open_ms)
    if keep "$o"; then
        note PASS "fresh session over relay" "opened in ${o} ms with UDP blackholed"
    else
        note FAIL "fresh session over relay" "no session could be opened while UDP was blackholed"
    fi

    # A path LABEL that flips is weaker evidence than a COUNTER that moves: the
    # label could flip because the carrier idled out with nothing trying to use
    # it, whereas `direct_fallbacks` only increments when a channel that asked
    # for UDP was actually served over the warm TCP relay. That is the promise,
    # so that is the number.
    read -r opens1 fb1 carr1 <<<"$(jump_counters)"
    # A COUNTER THAT COULD NOT BE READ IS NOT A COUNTER THAT DID NOT MOVE.
    # `jump_counters` goes through the admin API; when that answers nothing the
    # variables come back EMPTY, `${fb1:-0} -gt ${fb0:-0}` reads 0 > 0, and the
    # stage convicts the PRODUCT of a failure of its own instrument. Same shape
    # as every "a zero that means the instrument failed" defect this campaign
    # has paid for, and it would land on the one check that carries the promise.
    # NUMERIC, not merely non-empty: `jump_counters` answers `?` for an alias
    # that owns no row, and `[ "?" -gt 0 ]` is a shell error, not a verdict.
    num() { case "${1:-}" in ''|*[!0-9]*) return 1;; *) return 0;; esac; }
    if ! num "${fb0:-}" || ! num "${fb1:-}"; then
        note SKIP "a channel really fell back" \
             "counters unreadable (before='${fb0:-}' after='${fb1:-}') -- unmeasured"
    elif [ "$fb1" -gt "$fb0" ]; then
        note PASS "a channel really fell back" \
             "direct_fallbacks $fb0 -> $fb1, carriers ${carr0:-?} -> ${carr1:-?}"
    else
        note FAIL "a channel really fell back" \
             "direct_fallbacks did not move ($fb0 -> $fb1); opens ${opens0:-?} -> ${opens1:-?}"
    fi

    # Sampling needs a live session, and the one we started with is gone.
    if master_reopen; then
        sample_phase blackout "$SAMPLES" "$rep"
    else
        note SKIP "keystroke latency on the relay" "no session could be established to sample"
    fi

    rows_bh=$(jump_rows)
    # Same distinction: an unanswered API is not "zero rows", and zero rows is
    # not the invariant being tested either -- the invariant is EXACTLY ONE.
    if ! num "${rows_bh:-}"; then
        note SKIP "one alias, one admin row" "row count unreadable ('${rows_bh:-}') -- unmeasured"
    elif [ "$rows_bh" = "1" ]; then
        note PASS "one alias, one admin row" "1 row during the blackout"
    else
        note FAIL "one alias, one admin row" "$rows_bh rows (baseline ${rows_base:-?})"
    fi

    sleep "$BLACKOUT_SECS"

    # ---- phase 3: the path comes back --------------------------------------
    bh_off
    rc=$(wait_path_secs direct "$RECOVER_BUDGET")
    if [ "$rc" = FAILED ]; then
        note FAIL "path returns to direct" "still '$(jump_path)' after ${RECOVER_BUDGET}s"
    else
        note PASS "path returns to direct" "${rc}s (budget ${RECOVER_BUDGET}s)"
    fi
    # THIS one IS a promise, and the opposite direction of the one above: the
    # session sampled during the blackout is pinned to the warm TCP relay, and
    # the direct path COMING BACK must not disturb it. A path switch changes
    # where the NEXT channel goes; it must never tear down channels in flight.
    if master_alive; then
        note PASS "session survives recovery" "the relay-borne session still answers after the path returned to direct"
    else
        note FAIL "session survives recovery" "a live relay session was torn down by the path returning to direct"
    fi
    sample_phase recovered "$SAMPLES" "$rep"
    read -r opens2 fb2 carr2 <<<"$(jump_counters)"
    printf '    %-6s %-26s opens=%s fallbacks=%s carriers=%s\n' "" "(counters after recovery)" \
        "${opens2:-?}" "${fb2:-?}" "${carr2:-?}"

    # ---- phase 4: a rekey crosses the held session -------------------------
    # `RekeyLimit 256K 20` on the CLIENT; the server-side russh never initiates
    # one in practice, so the client's limit is the mechanism under test.
    sleep "$HOLD_SECS"
    kex1=$(kexinits)
    if [ "$kex1" -gt "$kex0" ]; then
        e=$(jump_echo_ms)
        if keep "$e"; then
            note PASS "rekey crossed" "$((kex1 - kex0)) rekey(s), keystroke ${e} ms afterwards"
        else
            note FAIL "rekey crossed" "$((kex1 - kex0)) rekey(s) then the session stopped answering"
        fi
        keep "$e" && SAMP["rekey"]+=" $e"
        sample_phase rekey "$SAMPLES" "$rep"
    else
        # Not a pass. The client may simply not have sent enough to trip it.
        note SKIP "rekey crossed" "no KEXINIT beyond the initial one in ${HOLD_SECS}s"
    fi

    if master_alive; then
        note PASS "session survives the hold" "alive after $((BLACKOUT_SECS + HOLD_SECS))s held open"
    else
        note FAIL "session survives the hold" "master died while simply being held"
    fi

    jump_master_close
    kill -TERM "$pid" 2>/dev/null
    sleep 5
done

echo
echo "=== checks ==="
pass=0; fail=0; skip=0
for c in "${CHECKS[@]:-}"; do
    [ -n "$c" ] || continue
    # OBSV falls through all three on purpose: it records a measured PROPERTY
    # of the design (see "established session" in phase 2), not a promise kept
    # or broken, and counting it either way would be a lie in one direction.
    case "${c%%|*}" in PASS) pass=$((pass+1));; FAIL) fail=$((fail+1));; SKIP) skip=$((skip+1));; esac
done
printf '  PASS %d   FAIL %d   SKIP %d\n' "$pass" "$fail" "$skip"
echo "  A SKIP is not a pass: it marks a promise this run could not put to the test."

# AN AGGREGATE MEDIAN ACROSS REPETITIONS MIXES TWO POPULATIONS.
#
# MEASURED (§44.9): repetition 1 sat at ~128 ms of baseline and repetition 2 at
# ~113. Those are not noise around one value, they are two levels -- the second
# repetition ran on a different state of the path -- and pooling their samples
# produces a median that describes neither. The ratio is the quantity that
# survives it, and V-9 already says so for throughput: a ratio against a control
# sampled in the SAME repetition is valid whatever the line is doing, an
# absolute is not. This table applies that rule to latency.
echo
echo "=== keystroke latency, PER REPETITION (median ms, and vs that rep's own baseline) ==="
printf '  %-4s %-11s %-10s %-10s %s\n' rep phase median warm-up 'vs its own baseline'
declare -A RATIO
for rep in $(seq 1 "$REPS"); do
    rbase=$(printf '%s\n' ${RSAMP["$rep|baseline"]:-} | med)
    for ph in baseline blackout recovered rekey; do
        m=$(printf '%s\n' ${RSAMP["$rep|$ph"]:-} | med)
        [ "$m" = "n/a" ] && continue
        r=$(LC_ALL=C awk -v m="$m" -v b="$rbase" -v p="$ph" 'BEGIN{
            if (p=="baseline") { print "-"; exit }
            if (b+0<=0 || m+0<=0) { print "n/a"; exit }
            printf "%.3f", m/b }')
        case "$r" in -|n/a) ;; *) RATIO["$ph"]+=" $r" ;; esac
        printf '  %-4s %-11s %-10s %-10s %s\n' "$rep" "$ph" "$m" "${WARM["$rep|$ph"]:-n/a}" "$r"
    done
done

echo
echo "=== the answer: median of the WITHIN-REPETITION ratios ==="
printf '  %-11s %-10s %s\n' phase 'vs baseline' 'samples'
for ph in blackout recovered rekey; do
    printf '  %-11s %-10s %s\n' "$ph" \
        "$(printf '%s\n' ${RATIO["$ph"]:-} | med)" "${RATIO["$ph"]:-  (none)}"
done
echo
echo "  'recovered' IS the second baseline this phase was missing: it is sampled"
echo "  after the path has returned to direct, so recovered-vs-baseline is drift"
echo "  and nothing else, and blackout-vs-baseline is only the relay's price once"
echo "  that drift is beside it. A recovered ratio near 1,00 makes the blackout"
echo "  row readable as a cost; a recovered ratio as far from 1 as the blackout"
echo "  row says this phase measured the passage of time, and the answer is"
echo "  jump_lat's interleaved arms instead."
echo "  The blackout row is a COST, not a fault: the fault would be the session"
echo "  not being there to measure."

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
echo "  pooled across repetitions -- kept because §44.9 is quoted against it,"
echo "  and NOT the basis of the tables above:"
for ph in baseline blackout recovered rekey; do
    printf '  %-11s%s\n' "$ph" "${SAMP["$ph"]:-  (none)}"
done
echo "  per repetition, which is what the tables above are built from:"
for rep in $(seq 1 "$REPS"); do
    for ph in baseline blackout recovered rekey; do
        printf '  rep %-2s %-11s warm=%-9s%s\n' "$rep" "$ph" \
            "${WARM["$rep|$ph"]:-n/a}" "${RSAMP["$rep|$ph"]:-  (none)}"
    done
done

# A STAGE THAT MEASURED NOTHING MUST NOT EXIT 0 -- see the note in jump_lat.sh.
# Exiting 0 with every cell empty makes the driver write a resume marker, and
# the next run SKIPS the stage: the campaign then reports itself complete with
# no data in it.
measured=0
for ph in baseline blackout recovered rekey; do
    [ -n "${SAMP["$ph"]:-}" ] && measured=$((measured + 1))
done
if [ "$measured" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: no usable sample in any cell."
    echo "  Exiting non-zero so no resume marker is written."
    exit 2
fi

echo
echo "DONE"
