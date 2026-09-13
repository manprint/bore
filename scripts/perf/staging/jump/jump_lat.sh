#!/usr/bin/env bash
# J1: the jump host's latency profile, relay against server-direct QUIC.
#
# THE QUESTION
# ------------
# A jump host carries interactive SSH. The deliverable is therefore the round
# trip, and the honest form of the question is not "is direct faster" but
# "WHICH of the four terms in a session open does the transport actually move".
# So every repetition records the same five numbers, and the three setup terms
# are nested on purpose:
#
#     tcp_ms    TCP connect to the gateway                 (the floor)
#     wchan_ms  ... + outer SSH handshake + direct-tcpip   (`ssh -W`)
#     open_ms   ... + inner SSH handshake                  (full `ssh -J`)
#     chan_ms   a new channel on an established session    (no handshake)
#     echo_ms   a byte there and back on a live session    (no setup at all)
#
# `open_ms - wchan_ms` is the inner sshd's handshake, which this project cannot
# change; quoting open_ms alone would attribute it to the gateway.
#
# BUT `wchan_ms - tcp_ms` IS NOT "the gateway's cost" EITHER, and this comment
# used to say it was. That was true only while a one-second sleep sat inside it
# (russh's `auth_rejection_time_initial`, see I-SSH12 -- 1022 of 1226 ms). With
# that removed the term decomposes, MEASURED on the real path at 22 ms RTT:
#
#     version exchange + key exchange ....  86 ms  ~4 RTT  RFC 4253, not ours
#     authentication (`none` + publickey).  61 ms  ~3 RTT  RFC 4252, not ours
#     channel open + provider dial + banner 46 ms  ~2 RTT  OURS
#
# So of ~210 ms, ~147 are the outer SSH handshake that no line of this
# repository can shorten, and 46 ms are the gateway -- themselves mostly two
# WAN crossings, because the provider sits on the far side of it. Read this
# column as "outer handshake + channel open", and only the second addend as a
# thing to optimise.
#
# ARMS interleave inside each repetition (drift cancels only when it is common
# to both), and the direct arm is VERIFIED from the server's admin API rather
# than assumed -- a provider always starts on the warm TCP relay, so an
# unverified "direct" arm is a relay measurement wearing the wrong label.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE/jumplib.sh"

REPS="${REPS:-5}"
SAMPLES="${SAMPLES:-7}"          # chan/echo samples per repetition per arm
JUMP_SERVER_EXTRA="${JUMP_SERVER_EXTRA:---udp --vhost-quic-port 7847}"
ARMS="${ARMS:-relay direct direct4}"

trap jump_cleanup EXIT

echo "### SSH jump host latency -- $REPS reps -- $(date -Is)"
echo "  gateway on the TEST VM (never staging), provider publishes this workstation's sshd"
echo "  arms: $ARMS   samples per arm per rep: $SAMPLES"
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
declare -A SAMP   # SAMP[arm|metric] = space separated samples

# A FAILED sample is a fact, not a number: it is printed in the raw block and
# kept out of every median. Averaging it in as 0 -- or dropping it silently --
# are the two ways a broken cell becomes a good-looking result.
keep() { case "$1" in FAILED|"") return 1;; *) return 0;; esac; }

run_arm() {
    local arm="$1" extra="" want="relay" pid path
    case "$arm" in
        direct)  extra="--udp"; want=direct ;;
        # P6 item 4: each SSH channel uses exactly ONE bidi stream, so carriers
        # here buy isolation, not bandwidth. The question is therefore whether
        # they COST latency -- which is only answerable with the arm present.
        direct4) extra="--udp --carriers 4"; want=direct ;;
        relay)   : ;;
        *)       echo "    $arm: unknown arm"; return 1 ;;
    esac

    pid=$(jump_start_provider "$extra") || { echo "    $arm: provider FAILED"; return 1; }
    path=$(jump_wait_path "$want" 90)
    if [ "$path" != "$want" ]; then
        # Printed, not averaged in. A path that did not match its label is the
        # one mistake this campaign exists to avoid.
        echo "    $arm: FAILED -- path is '$path', expected '$want'"
        kill -TERM "$pid" 2>/dev/null; sleep 2
        return 1
    fi

    local t w o
    t=$(jump_tcp_ms); w=$(jump_wchan_ms); o=$(jump_open_ms)
    keep "$t" && SAMP["$arm|tcp"]+=" $t"
    keep "$w" && SAMP["$arm|wchan"]+=" $w"
    keep "$o" && SAMP["$arm|open"]+=" $o"

    if jump_master_open; then
        local i c e
        for i in $(seq 1 "$SAMPLES"); do
            c=$(jump_chan_ms); e=$(jump_echo_ms)
            keep "$c" && SAMP["$arm|chan"]+=" $c"
            keep "$e" && SAMP["$arm|echo"]+=" $e"
        done
        jump_master_close
    else
        echo "    $arm: ControlMaster FAILED (chan/echo not sampled this rep)"
    fi

    printf '    %-7s tcp=%-8s wchan=%-8s open=%-8s path=%s\n' "$arm" "$t" "$w" "$o" "$path"
    kill -TERM "$pid" 2>/dev/null
    sleep 3
    return 0
}

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    for arm in $ARMS; do run_arm "$arm"; done
done

echo
echo "=== medians (ms), per arm ==="
printf '  %-7s %-10s %-10s %-10s %-10s %-10s\n' arm tcp wchan open chan echo
for arm in $ARMS; do
    row="  $(printf '%-7s' "$arm")"
    for m in tcp wchan open chan echo; do
        row+=" $(printf '%-10s' "$(printf '%s\n' ${SAMP["$arm|$m"]:-} | med)")"
    done
    echo "$row"
done

echo
echo "=== derived: where the time actually goes (median ms) ==="
for arm in $ARMS; do
    t=$(printf '%s\n' ${SAMP["$arm|tcp"]:-}   | med)
    w=$(printf '%s\n' ${SAMP["$arm|wchan"]:-} | med)
    o=$(printf '%s\n' ${SAMP["$arm|open"]:-}  | med)
    LC_ALL=C awk -v a="$arm" -v t="$t" -v w="$w" -v o="$o" 'BEGIN{
        if (t=="" || w=="" || o=="" || t=="n/a" || w=="n/a" || o=="n/a") {
            printf "  %-7s (incomplete -- an arm with no usable samples)\n", a; exit }
        printf "  %-7s network floor %.1f | gateway %.1f | inner sshd %.1f\n", a, t, w-t, o-w }'
done

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for arm in $ARMS; do
    for m in tcp wchan open chan echo; do
        printf '  %-7s %-6s%s\n' "$arm" "$m" "${SAMP["$arm|$m"]:-  (none)}"
    done
done

# A STAGE THAT MEASURED NOTHING MUST NOT EXIT 0.
#
# MEASURED on the first successful P6 provisioning: every arm of every
# repetition reported `provider FAILED`, every median printed `n/a`, every raw
# block printed `(none)` -- and the stage exited 0, so the driver wrote a
# `_done.` marker and the next invocation SKIPPED it. A re-run would have
# reported the campaign as complete with no data in it. The duration was the
# only visible signal: 12 s against a 3600 s timeout.
#
# This is the same defect `vpn_hub` had (19 s, rc=0, bandwidth silently never
# measured) and the reason the campaign plan carries the rule "compare a
# stage's duration with its budget BEFORE reading its result". A rule a human
# must remember is weaker than an exit code.
measured=0
for arm in $ARMS; do
    for m in tcp wchan open chan echo; do
        [ -n "${SAMP["$arm|$m"]:-}" ] && measured=$((measured + 1))
    done
done
if [ "$measured" -eq 0 ]; then
    echo
    echo "INSTRUMENT FAILURE: no arm produced a single usable sample."
    echo "  Nothing above is a measurement. Exiting non-zero so no resume marker"
    echo "  is written and the next run repeats this stage instead of skipping it."
    exit 2
fi

echo
echo "DONE"
