#!/usr/bin/env bash
# J2: does a BULK channel delay an INTERACTIVE channel on the same session?
#
# THE QUESTION, AND WHY IT BELONGS TO THE JUMP HOST SPECIFICALLY
# --------------------------------------------------------------
# A jump host carries interactive SSH, and an operator's session almost never
# carries only keystrokes: a `scp` through the same `ProxyJump`, an `rsync`, a
# port-forward pulling a file. Every one of those is ANOTHER CHANNEL on the SAME
# SSH connection. So the deliverable is not "how fast is a keystroke on an idle
# session" -- jump_lat answers that -- it is "what happens to the keystroke when
# the session is also moving a gigabyte".
#
# This repository already ships a fix for the failure mode: per-channel
# head-of-line blocking inside russh, where one slow or streaming client blocked
# ALL others on the same tunnel, fixed by vendoring russh with window-tied
# backpressure (`crates/russh/HOL_FIX.md`). That fix has unit gates. It does NOT
# have a REAL-PATH gate -- and this project's own standing lesson is that an
# in-process test false-passes exactly this class: loopback drains
# opportunistically and never builds the queue that makes the bug visible.
# This stage is that gate.
#
# WHAT IS MEASURED
# ----------------
#   echo_idle    keystroke RTT on a live session, nothing else running
#   echo_loaded  the SAME measurement, sampled strictly INSIDE a bulk transfer
#                running on a SECOND channel of the SAME connection
#
# The ratio is the answer. A number near 1 means the channels are independent;
# a large one means the bulk channel owns the connection while it runs, which
# for a jump host is the difference between usable and unusable.
#
# WHY THE LOAD IS TIME-BOUNDED, NOT BYTE-BOUNDED
# -----------------------------------------------
# The samples must land INSIDE the load or they measure an idle session and
# report "no interference" with perfect confidence (the same trap `vpn_rtt_load`
# names). A byte count lands differently on every arm because the arms have
# different throughput; a duration lands the same on all of them.
#
# ARMS interleave inside each repetition, and the direct arm is VERIFIED from
# the server's admin API rather than assumed -- a provider always starts on the
# warm TCP relay, so an unverified "direct" arm is a relay measurement wearing
# the wrong label.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/jumplib.sh"

REPS="${REPS:-3}"
SAMPLES="${SAMPLES:-9}"           # echo samples per phase per arm per repetition
LOAD_SECS="${LOAD_SECS:-15}"
JUMP_SERVER_EXTRA="${JUMP_SERVER_EXTRA:---udp --vhost-quic-port 7847}"
ARMS="${ARMS:-relay direct}"

trap 'jump_cleanup; rm -f "${LOAD_COUNT_FILE:-/nonexistent}"' EXIT

echo "### SSH jump host -- channel isolation under bulk, $REPS reps -- $(date -Is)"
echo "  gateway on the TEST VM (never staging); provider publishes this workstation's sshd"
echo "  arms: $ARMS   echo samples per phase: $SAMPLES   load: ${LOAD_SECS}s on a second channel"
echo

echo "=== provisioning ==="
jump_build_and_deploy || { echo "build/deploy failed"; exit 1; }
jump_start_gateway    || { echo "gateway failed";    exit 1; }
# THE PREMISE, before the measurement. `open_ms - wchan_ms` is the inner sshd's
# handshake and `wchan_ms - tcp_ms` is the gateway's own cost, so an absent
# inner sshd does not lose one column -- it corrupts the other by subtraction.
jump_ensure_inner_target || { echo "INSTRUMENT FAILURE: no inner SSH target"; exit 2; }
echo

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
keep() { case "$1" in FAILED|"") return 1;; *) return 0;; esac; }

# A bulk transfer on a SECOND channel of the SAME master connection. `/dev/zero`
# is fine: nothing on this path compresses, and the point is to fill a window,
# not to move meaningful bytes.
#
# THE LOAD MUST PROVE IT LOADED, AND THAT IS THE WHOLE GATE.
# ----------------------------------------------------------
# This stage exists to detect head-of-line blocking. Its failure mode is
# therefore SYMMETRIC WITH ITS PASS: if the second channel never opens -- the
# master is gone, the alias moved, sshd refuses another session, the transfer
# dies in its first millisecond -- then `loaded` samples an IDLE session,
# `loaded/idle` reads 1.00, and the stage reports PERFECT ISOLATION with
# perfect confidence. That is the campaign's most expensive defect class ("a
# zero that means the instrument failed") in the one place it would be least
# visible, because the wrong answer here is the answer everyone wants.
#
# So the load is COUNTED. The generator is bounded locally by `timeout`; when
# it dies the pipe closes, ssh sends EOF on the channel, and the remote
# `wc -c` -- which only prints at EOF, which is exactly why it is the counter
# -- writes the byte total. No signals, no `dd` statistics (MEASURED: GNU dd
# prints nothing at all when `timeout` ends it with SIGTERM), no FIFO.
#
# The remote end of this path is this same workstation's sshd, so the count
# file is written and read locally; the bytes still crossed the gateway twice.
#
# LOAD_PID is a GLOBAL and this is NOT a command substitution, deliberately.
# `lpid=$(start_load)` would run the whole function in a subshell, so `$!` would
# name a process this shell never forked -- and the later `wait` would return
# 127 immediately instead of waiting, letting the master close while a transfer
# was still running on it. The bug is silent: the samples still come out, and
# the next arm starts on a session that is being torn down underneath it.
LOAD_PID=""
LOAD_COUNT_FILE="$WORK/jump_hol_bytes.$$"
# A load that moved less than this did not load anything: at the slowest rate
# this path has ever measured, a single second carries several MiB.
LOAD_MIN_BYTES="${LOAD_MIN_BYTES:-8388608}"
# THE COUNT IS WRITTEN LOCALLY, and that is not a refactor.
#
# This used to be `ssh ... "wc -c > '$LOAD_COUNT_FILE'"`, which runs the
# REDIRECTION on the far side: `$LOAD_COUNT_FILE` is `$WORK/...`, a path under
# this workstation's `~/.cache`, and the inner target is a different machine
# (now a container) where it does not exist. Every rep printed
# `No such file or directory` and then, correctly, `LOAD FAILED -- 0 bytes
# crossed the second channel`. The guard did its job; the path was simply
# never the far side's to write.
#
# Capturing the far side's stdout locally is also strictly better: it needs
# nothing writable on the inner target at all, so the stage works against an
# inner host the operator does not administer.
start_load() {
    rm -f "$LOAD_COUNT_FILE"
    ( timeout "$LOAD_SECS" cat /dev/zero 2>/dev/null \
        | ssh -o ControlPath="$JUMP_CTL_SOCK" "$JUMP_INNER_USER@$JUMP_TARGET" \
              "wc -c" 2>/dev/null > "$LOAD_COUNT_FILE" ) &
    LOAD_PID=$!
}
load_bytes() { tr -dc '0-9' < "$LOAD_COUNT_FILE" 2>/dev/null; }

REP_IDLE=""; REP_LOADED=""
sample_echo() { # <arm> <phase> <n>
    local arm="$1" phase="$2" n="$3" i e
    for i in $(seq 1 "$n"); do
        e=$(jump_echo_ms)
        keep "$e" || continue
        SAMP["$arm|$phase"]+=" $e"
        case "$phase" in
            idle)   REP_IDLE+=" $e" ;;
            loaded) REP_LOADED+=" $e" ;;
        esac
    done
}

run_arm() {
    local arm="$1" extra="" want="relay" pid
    case "$arm" in
        direct) extra="--udp"; want=direct ;;
        relay)  : ;;
        *) echo "    $arm: unknown arm"; return 1 ;;
    esac

    pid=$(jump_start_provider "$extra") || { echo "    $arm: provider FAILED"; return 1; }
    local path; path=$(jump_wait_path "$want" 90)
    if [ "$path" != "$want" ]; then
        echo "    $arm: FAILED -- path is '$path', expected '$want'"
        kill -TERM "$pid" 2>/dev/null; sleep 2; return 1
    fi

    if jump_master_open; then
        # REP_* hold only THIS repetition's samples. The per-rep line used to
        # print `med` over the CUMULATIVE array, so rep 2's line was the median
        # of reps 1 and 2 -- a number belonging to no single repetition, which
        # is the same mistake as dividing two medians from different ones.
        REP_IDLE=""; REP_LOADED=""
        sample_echo "$arm" idle "$SAMPLES"

        start_load
        sleep 1                      # let the transfer actually get going
        local t0 t1 win
        t0=$(date +%s%3N)
        sample_echo "$arm" loaded "$SAMPLES"
        t1=$(date +%s%3N)
        win=$(( t1 - t0 ))
        wait "$LOAD_PID" 2>/dev/null

        local lb; lb="$(load_bytes)"
        if [ -z "$lb" ] || [ "$lb" -lt "$LOAD_MIN_BYTES" ] 2>/dev/null; then
            # The samples are DISCARDED, not published: without a load they are
            # idle samples, and publishing them as `loaded` is how this gate
            # would report perfect isolation for a session that carried nothing.
            echo "    $arm: LOAD FAILED -- ${lb:-0} bytes crossed the second channel"
            echo "           (floor $LOAD_MIN_BYTES). Loaded samples discarded for this rep."
            # Quoted and with NO extra space: REP_LOADED already starts with
            # one, so `% $REP_LOADED` builds a pattern with two and matches
            # nothing -- the samples would stay in, and the gate would go on
            # reporting the isolation it just failed to measure.
            SAMP["$arm|loaded"]="${SAMP["$arm|loaded"]%"$REP_LOADED"}"
            REP_LOADED=""
        elif [ "$win" -gt $(( (LOAD_SECS - 1) * 1000 )) ]; then
            # Sampling outlasted the load, so the tail of it measured an idle
            # session and dragged the loaded median DOWN -- toward "isolated".
            echo "    $arm: NOTE sampling took ${win}ms against a $(( (LOAD_SECS-1) * 1000 ))ms"
            echo "           load window: the last samples were taken after it ended."
        fi

        jump_master_close
        printf '    %-7s path=%-7s idle=%-8s loaded=%-8s load=%s MiB\n' \
            "$arm" "$path" \
            "$(printf '%s\n' ${REP_IDLE:-}   | med)" \
            "$(printf '%s\n' ${REP_LOADED:-} | med)" \
            "$(( ${lb:-0} / 1048576 ))"
    else
        echo "    $arm: ControlMaster FAILED (not sampled this rep)"
    fi

    kill -TERM "$pid" 2>/dev/null
    sleep 3
}

# ORDER ALTERNATES. A fixed arm order is a confounder in its own right -- the
# arm that always runs first always runs on a freshly started gateway and an
# empty path, which is precisely the trap `ws_conns.sh` names ("the rungs always
# ran in the same order, so a line that drifted during the stage presented as a
# rung effect"). Odd repetitions walk the arms forward, even ones backward.
rev_arms() { local a out=""; for a in $ARMS; do out="$a $out"; done; printf '%s' "$out"; }
for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    if [ $((rep % 2)) -eq 1 ]; then order="$ARMS"; else order="$(rev_arms)"; fi
    for arm in $order; do run_arm "$arm"; done
done

echo
echo "=== keystroke latency, idle against loaded (median ms) ==="
printf '  %-7s %-10s %-10s %-10s %-10s\n' arm idle loaded 'loaded/idle' 'worst loaded'
for arm in $ARMS; do
    i=$(printf '%s\n' ${SAMP["$arm|idle"]:-}   | med)
    l=$(printf '%s\n' ${SAMP["$arm|loaded"]:-} | med)
    w=$(printf '%s\n' ${SAMP["$arm|loaded"]:-} | tr ' ' '\n' | LC_ALL=C sort -g | tail -1)
    LC_ALL=C awk -v a="$arm" -v i="$i" -v l="$l" -v w="${w:-n/a}" 'BEGIN{
        r = (i+0>0 && l+0>0) ? sprintf("%.2fx", l/i) : "n/a"
        printf "  %-7s %-10s %-10s %-10s %-10s\n", a, i, l, r, w }'
done

echo
echo "  Reading it: the ratio is the isolation. Near 1.0 means a bulk channel and"
echo "  an interactive one coexist, which is what the window-tied backpressure in"
echo "  the vendored russh is for. A large ratio -- or a 'worst loaded' far above"
echo "  the median -- means the bulk channel owns the connection while it runs,"
echo "  and an operator typing through this jump host would feel it."

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for arm in $ARMS; do
    for ph in idle loaded; do
        printf '  %-7s %-7s%s\n' "$arm" "$ph" "${SAMP["$arm|$ph"]:-  (none)}"
    done
done

# A STAGE THAT MEASURED NOTHING MUST NOT EXIT 0 -- see the note in jump_lat.sh.
# Exiting 0 with every cell empty makes the driver write a resume marker, and
# the next run SKIPS the stage: the campaign then reports itself complete with
# no data in it.
measured=0
for arm in $ARMS; do
    for ph in idle loaded; do
        [ -n "${SAMP["$arm|$ph"]:-}" ] && measured=$((measured + 1))
    done
done
if [ "$measured" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: no usable sample in any cell."
    echo "  Exiting non-zero so no resume marker is written."
    exit 2
fi

echo
echo "DONE"
