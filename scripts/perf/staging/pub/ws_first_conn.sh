#!/usr/bin/env bash
# W1e: what does the FIRST proxied connection of a tunnel cost, and why?
#
# THE OBSERVATION THIS EXISTS TO EXPLAIN
# ---------------------------------------
# `ws_dl1.sh` measures six paired single-connection downloads, relay against
# QUIC direct, and the first pair is low on the QUIC arm in BOTH runs of this
# campaign: 18.6 % below the median of the later pairs at 384 MiB and 21.1 %
# below at 96 MiB. It is not a position effect -- the arm order alternates and
# the QUIC arm runs FIRST in the even pairs, where it reads full rate. It is the
# first QUIC transfer of the stage, and only that one.
#
# It is also a RATE effect and not a fixed setup cost: the penalty in time grows
# with the transfer, 0.303 s at 96 MiB and 0.871 s at 384 MiB. A handshake or a
# stream open would cost the same in both (one RTT here is 19 ms).
#
# WHAT SEPARATES THE CANDIDATE EXPLANATIONS
# ------------------------------------------
# Three explanations fit that shape and the admin API tells them apart, which is
# the whole design of this stage -- measure the rate AND read the server's own
# view of the transport around the same transfer:
#
#   cold congestion control   path=direct, direct_stream_opens +1, fallbacks +0
#   served on the warm relay  path=relay,  opens +0,              fallbacks +1
#   carriers dialled lazily   direct_pool rises from 0 across the first transfer
#
# THE CONTROL IS A RELAY TUNNEL, AND IT IS NOT OPTIONAL
# -----------------------------------------------------
# "The first transfer is slow" is only interesting if it is NOT true of every
# tunnel. A plain relay tunnel registered at the same moment and driven with the
# same sequence answers that. Its own first transfer carries the server's TCP
# carrier warm-up and this workstation's, so whatever it shows is the part that
# belongs to neither transport in particular.
#
# The two tunnels are registered up front and then driven ALTERNATELY, so line
# drift is common to both. They cannot be interleaved within a transfer -- a
# tunnel has exactly one first connection, which is the quantity under test.
#
# THE RESIDUAL, AND THE AXIS THAT CLOSES IT (`DELAYS`)
# -----------------------------------------------------
# This stage ran and attributed the cost: the direct pool already reads 1 BEFORE
# a byte moves and `direct_fallbacks` stays 0, so it is neither a lazy pool nor
# a fallback in disguise -- it is a cold congestion controller. But it measured
# the penalty at 4,2 % where §35 measured 18,6 % on the same line, and the one
# structural difference between the two is TIME: this stage registers both
# tunnels up front and spends ~40 s on the no-traffic rows and the control arm
# before the QUIC arm's first byte, while `ws_dl1` transfers almost immediately.
#
# That makes the residual a testable statement rather than a caveat: if the cost
# is a function of the TIME SINCE REGISTRATION it falls as the delay grows; if
# it is a function of the ORDER of the transfer it does not move at all.
#
# `DELAYS="0 5 20 60"` runs that axis. A tunnel has exactly ONE first
# connection, so each delay needs its OWN freshly registered pair -- the axis
# cannot be walked on one tunnel, which is also why it costs what it costs.
# Each cell then measures the SAME tunnel's later transfers, so the penalty is a
# within-tunnel ratio and the line cancels out of it.
#
# `DELAYS` unset keeps the legacy single-pass body byte-for-byte, because its
# output is what §40 cites.
#
# COST: legacy, 6 arms x XFER_MB -- every arm a download, so every arm AWS
# egress: 2.25 GiB at the default. The delay axis costs
# REPS x len(DELAYS) x (XFERS + CTRL_XFERS) x XFER_MB, which at the defaults is
# 22.5 GiB. Declared, because a stage whose cost is invisible gets re-run
# without thinking.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
XFER_MB="${XFER_MB:-384}"
XFERS="${XFERS:-3}"
RP=5053; R=9047; Q=9048; PER=$(( XFER_MB*1048576 ))
# --- the delay axis (unset = legacy body, byte-for-byte) --------------------
DELAYS="${DELAYS:-}"
REPS="${REPS:-3}"            # repetitions of the whole delay sweep
CTRL_XFERS="${CTRL_XFERS:-2}"  # transfers on the relay control per cell
# The cell re-registers on the SAME two ports rather than walking upward. What
# the axis varies is the AGE of a tunnel, and a tunnel torn down and raised
# again is fresh whatever its port number -- while a port nobody has used before
# is an untested premise: this deployment's public range is opened by an AWS
# security group, not by `--min-port/--max-port` (the server reports both null),
# so a stage that invents port numbers can fail for a reason that has nothing to
# do with its question. `down_one` waits for the server to FORGET the entry
# before the next cell raises it, which is what makes reuse safe.

UP=()
up() { # <port> <extra flags>
    vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsfirst-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
    local i; for i in $(seq 80); do
        adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
            && { UP+=("$1"); return 0; }
        sleep 0.5
    done
    return 1
}
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT

# The server's own view of one tunnel, as four fields on one line. An empty
# answer is printed as `?` and NEVER as a zero: "the API did not answer" and
# "the counter did not move" are opposite readings and used to look identical
# (the standing rule this campaign has paid for six times).
view() { # <port> -> "path opens fallbacks pool"
    # MEASURED: on empty input `jq` prints NOTHING and exits 0, so a `|| echo`
    # tail never fires and the caller gets an empty string -- which printf then
    # renders as a short, silent row. An unanswered API must LOOK unanswered.
    local v
    v="$(adm tunnels 2>/dev/null | jq -r --argjson p "$1" '
        [ .[] | select(.public_port == $p) ] as $t
        | if ($t|length) == 0 then "? ? ? ?"
          else "\($t[0].current_path // "?") \($t[0].direct_stream_opens // "?") \($t[0].direct_fallbacks // "?") \($t[0].direct_pool // "?")"
          end' 2>/dev/null)"
    [ -n "$v" ] || v="? ? ? ?"
    printf '%s' "$v"
}

g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 1 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

down_one() { # <port> -- kill this tunnel and WAIT for the server to forget it.
    # Not cosmetic: the next cell re-registers on a NEARBY port and a server
    # still holding the previous entry makes the new tunnel's "before any
    # traffic" row describe the old one. Bounded, then given up on loudly.
    vm "pkill -9 -f \"local $RP --port $1\" 2>/dev/null; true" >/dev/null 2>&1
    local i js
    for i in $(seq 60); do
        # AN UNANSWERED API IS NOT AN ABSENT PORT. `adm | jq -e` exits non-zero
        # both when the tunnel is gone AND when `adm` produced nothing at all,
        # so the obvious `|| return 0` reports "released" for a server that
        # never answered -- the same "a zero that means the instrument failed"
        # shape this campaign keeps paying for. The JSON is captured first and
        # its emptiness is a separate, louder case.
        js="$(adm tunnels 2>/dev/null)"
        if [ -z "$js" ]; then
            echo "    WARNING: the admin API did not answer while releasing port $1"
            sleep 1; continue
        fi
        printf '%s' "$js" | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 || return 0
        sleep 0.5
    done
    echo "    WARNING: port $1 still registered after 30 s"
    return 1
}

delay_axis() {
    local rep d arm port_r port_q idx=0
    declare -A PEN   # PEN[arm|delay] = space separated penalty percentages
    declare -A FIRST LATER
    echo "=== first-connection cost vs TIME SINCE REGISTRATION ==="
    echo "  delays: $DELAYS s   reps=$REPS   ${XFER_MB} MiB per transfer, ONE connection"
    echo "  per cell: a FRESH pair is registered, the delay is waited out, then"
    echo "  $XFERS transfers on the --udp arm and $CTRL_XFERS on the relay control."
    echo "  penalty = 1 - first / median(later), WITHIN the same tunnel."
    echo

    for rep in $(seq 1 "$REPS"); do
        local order="$DELAYS"
        # Even repetitions walk the delays downward, so a line that drifts
        # during the sweep shows up as disagreement BETWEEN repetitions rather
        # than as a slope along the axis -- the same correction ws_conns.sh
        # carries, and for the same reason.
        [ $((rep % 2)) -eq 0 ] && order="$(printf '%s\n' $DELAYS | tac | tr '\n' ' ')"
        echo "  --- rep $rep  (delays: $order)"
        for d in $order; do
            idx=$((idx + 1))
            port_r=$R; port_q=$Q
            UP=()
            if ! up "$port_r" "" || ! up "$port_q" "--udp"; then
                echo "    delay=${d}s  FAILED to register the pair (r=$port_r q=$port_q)"
                down_one "$port_r"; down_one "$port_q"; continue
            fi
            # The clock starts when the SERVER says the tunnel exists, which is
            # what "time since registration" has to mean -- not when the ssh
            # that launched it returned.
            sleep "$d"
            local pre_q pre_r
            pre_q="$(view "$port_q")"; pre_r="$(view "$port_r")"
            printf '    delay=%-3ss  quic  before: %s\n' "$d" "$pre_q"
            printf '    delay=%-3ss  relay before: %s\n' "$d" "$pre_r"

            local n mbs
            for arm in quic relay; do
                local port lim
                case "$arm" in quic) port=$port_q; lim=$XFERS ;; relay) port=$port_r; lim=$CTRL_XFERS ;; esac
                local got=""
                for n in $(seq 1 "$lim"); do
                    mbs=$(g "$port")
                    case "${mbs:-}" in ''|*[!0-9.]*) mbs=FAILED ;; esac
                    got+=" $mbs"
                    printf '      %-5s xfer %-2s %-9s %s\n' "$arm" "$n" "$mbs" "$(view "$port")"
                    cool 75
                done
                # first vs the median of the rest, inside this one tunnel.
                # shellcheck disable=SC2086
                set -- $got
                local f="$1"; shift
                local l; l=$(printf '%s\n' "$@" | med)
                FIRST["$arm|$d"]+=" $f"; LATER["$arm|$d"]+=" $l"
                case "$f$l" in
                    *FAILED*|*n/a*) printf '      %-5s penalty: n/a (a transfer failed)\n' "$arm" ;;
                    *) local pct; pct=$(LC_ALL=C awk -v f="$f" -v l="$l" 'BEGIN{if(l+0==0){print "n/a"}else{printf "%.1f", 100*(1-f/l)}}')
                       PEN["$arm|$d"]+=" $pct"
                       printf '      %-5s penalty: %s%% (first %s vs later %s)\n' "$arm" "$pct" "$f" "$l" ;;
                esac
            done
            down_one "$port_q"; down_one "$port_r"
        done
    done

    echo
    echo "=== penalty of the first transfer, by delay (%) ==="
    printf '  %-6s %-8s %-10s %-10s %s\n' arm delay median first later
    for arm in quic relay; do
        for d in $DELAYS; do
            printf '  %-6s %-8s %-10s %-10s %s\n' "$arm" "${d}s" \
                "$(printf '%s\n' ${PEN["$arm|$d"]:-} | med)" \
                "$(printf '%s\n' ${FIRST["$arm|$d"]:-} | med)" \
                "$(printf '%s\n' ${LATER["$arm|$d"]:-} | med)"
        done
    done
    echo
    echo "=== raw penalties ==="
    for k in "${!PEN[@]}"; do printf '  %-12s%s\n' "$k" "${PEN[$k]}"; done | sort

    echo
    echo "=== how to read it ==="
    echo "  A quic penalty that FALLS as the delay grows means the cost is a"
    echo "  function of the time since registration -- the direct path is still"
    echo "  settling, and §35's 18,6 % and §40's 4,2 % are the same phenomenon"
    echo "  measured at two different ages. A penalty FLAT across the axis means"
    echo "  it is the transfer's ORDER and not its age, and the two figures then"
    echo "  need a different reconciliation than this one."
    echo "  The relay column is the control: if it moves with the delay too, the"
    echo "  effect belongs to registration in general and not to the direct path."
    echo "  Read the 'before' rows in every cell: a quic tunnel whose path still"
    echo "  reads relay at delay=0 is a THIRD answer -- the first transfer was"
    echo "  never on the direct path at all."

    local got=0
    for k in "${!PEN[@]}"; do [ -n "${PEN[$k]}" ] && got=1; done
    [ "$got" = 1 ] || { echo; echo "INSTRUMENT FAILURE: no cell produced a penalty."; return 2; }
    return 0
}

if [ -n "$DELAYS" ]; then
    delay_axis; rc=$?
    echo; echo "DONE"
    exit $rc
fi

echo "=== first-connection cost, ${XFER_MB} MiB per transfer, ONE connection, $XFERS transfers per arm ==="
echo "  relay tunnel on $R (control), --udp tunnel on $Q; registered up front, driven alternately"
echo

up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register";  exit 1; }

# BEFORE ANY TRAFFIC. This row is the one that can answer the lazy-carrier
# question on its own: a direct pool that is already populated here cannot be
# what the first transfer is paying for.
printf '  %-7s %-4s %-9s %-12s %-7s %-7s %-6s\n' arm xfer MBs path opens fallb pool
printf '  %-7s %-4s %-9s %s\n' relay  0 '(no traffic)' "$(view "$R")"
printf '  %-7s %-4s %-9s %s\n' quic   0 '(no traffic)' "$(view "$Q")"

declare -A SAMP
for n in $(seq 1 "$XFERS"); do
    for arm in relay quic; do
        case "$arm" in relay) port=$R ;; quic) port=$Q ;; esac
        pre="$(view "$port")"
        mbs=$(g "$port")
        post="$(view "$port")"
        case "${mbs:-}" in ''|*[!0-9.]*) mbs=FAILED ;; *) SAMP["$arm"]+=" $mbs" ;; esac
        printf '  %-7s %-4s %-9s %s\n' "$arm" "$n" "$mbs" "$post"
        printf '  %-7s %-4s %-9s %s\n' "" "" '  (before)' "$pre"
        cool 75
    done
done

echo
echo "=== rate by transfer number (MB/s) ==="
for arm in relay quic; do printf '  %-7s%s\n' "$arm" "${SAMP["$arm"]:-  (none)}"; done

echo
echo "=== reading ==="
echo "  If transfer 1 on the quic arm is slow while the relay control is flat,"
echo "  the cost belongs to the DIRECT path. Then the counters say which of the"
echo "  three it is: path=relay with fallbacks +1 means it was never direct;"
echo "  path=direct with opens +1 and a pool already populated before any"
echo "  traffic means a cold congestion controller and nothing else; a pool that"
echo "  only fills across transfer 1 means the carriers are dialled lazily."
echo "  If BOTH arms are slow on transfer 1 the cost is not the transport's --"
echo "  it is the path, this workstation, or the origin waking up."
echo
echo "DONE"
