#!/usr/bin/env bash
# The OPEN QUESTIONS of the wired window, and only those.
#
# §8 of `docs/performance/RELAZIONE_FINALE_CABLATO_2026-09-13.md` lists what the
# campaign could not close. Four of those items share a property that makes them
# worth a driver of their own: each is small, each has a stated method, and each
# currently forces a published claim to be hedged. They are not new questions --
# they are the bill the campaign left.
#
#  1. `pub_ws_conns_var`  -- why the public ladder stops repeating at n>=4.
#     Until it runs, §42 caps the ladder's citation at n=2.
#  2. `pub_ws_first_conn_delay` -- is the first connection's cost a function of
#     the ORDER of the transfer or of the TIME since registration? Until it
#     runs, §40 cites the worst case (18,6 %) rather than the truth.
#  3. `vpn_carriers_relay_deep` -- do relay carriers recover when the flows are
#     as many as the carriers? At one flow the answer is a clean no; at 4 and 8
#     the intervals overlap and one cell is bimodal.
#  4. `jump_stab_r2` -- `jump_stab` samples its baseline only BEFORE the relay
#     arm, so "the relay costs" and "time passed" are the same measurement.
#
# EVERY ONE OF THEM IS A DISPERSION QUESTION, AND THAT SETS THE SHAPE.
# Few cells, many repetitions, controls sampled inside the same run. The
# campaign's own §42.5 rule is what these stages are built to satisfy: a phase
# that publishes a comparison between cells must also publish the spread of the
# repeated cell.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

export BORE_PERF_OUT="$REPO/out/eth"
mkdir -p "$BORE_PERF_OUT"
LOG="$BORE_PERF_OUT/_driver_open.log"
export LC_ALL=C

say() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

# shellcheck disable=SC1091
. scripts/perf/staging/driverlib.sh

found="$(other_driver)"
if [ -n "$found" ]; then
    say "REFUSING: another campaign driver is running -- the link is not free."
    printf '%s\n' "$found" | sed 's/^/    /'
    exit 3
fi

baseline() {
    local tag="$1" d u
    ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o LogLevel=ERROR -o ConnectTimeout=10 \
        -i "$BORE_SSH_KEY" "$BORE_VM_USER@$BORE_VM" \
        "pgrep -x iperf3 >/dev/null || (nohup iperf3 -s -p 5299 >/dev/null 2>&1 &); sleep 1" \
        >/dev/null 2>&1
    d=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299 -R -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    sleep 2
    u=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299    -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    say "BASELINE[$tag] download=${d:-FAILED} upload=${u:-FAILED} Mbit/s"
}

# Verbatim from rerun_eth_p7.sh: markers, per-stage timeout, host-check, settle.
# A stage must behave identically under every driver or its result is not
# comparable with the same stage's result under another one.
run_stage() {
    local name="$1" tmo="$2" script="$3"; shift 3
    local marker="$BORE_PERF_OUT/_done.$name"
    if [ -f "$marker" ]; then say "SKIP  $name (marker present)"; return 0; fi
    if [ ! -x "$REPO/$script" ]; then say "MISS  $name ($script not executable)"; return 0; fi
    say "BEGIN $name  (timeout ${tmo}s)  $*"
    local t0=$SECONDS rc
    ( cd "$REPO" && env "$@" timeout -k 30 "$tmo" "$REPO/$script" ) \
        >"$BORE_PERF_OUT/$name.out" 2>&1
    rc=$?
    local el=$((SECONDS - t0))
    if [ $rc -eq 0 ]; then
        touch "$marker"; say "END   $name rc=0 elapsed=${el}s"
    else
        say "FAIL  $name rc=$rc elapsed=${el}s  (see $name.out; no marker, will retry)"
    fi
    local ifaces routes
    ifaces=$(ip -br link 2>/dev/null | { grep -c '^bore' || true; })
    routes=$(ip route 2>/dev/null | { grep -c 'bore' || true; })
    if [ "${ifaces:-0}" != 0 ] || [ "${routes:-0}" != 0 ]; then
        say "      host-check: LEFTOVER bore ifaces=$ifaces routes=$routes"
    fi
    sleep 10
    return 0
}

say "################ OPEN QUESTIONS ################"
say "stages:  ${STAGES:-conns_var}"
say "repo:    $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null) $(git -C "$REPO" status --porcelain 2>/dev/null | wc -l) file(s) dirty"
say "binary:  $(sha256sum "$REPO/target/release/bore" 2>/dev/null | cut -c1-16) $("$REPO/target/release/bore" --version 2>/dev/null | head -1)"
say "nic:     $(ip route show default | awk '/^default/{print $5; exit}') speed=$(cat /sys/class/net/"$(ip route show default | awk '/^default/{print $5; exit}')"/speed 2>/dev/null) Mbit/s"
say "far bin: $(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o LogLevel=ERROR -o ConnectTimeout=10 \
        -i "$BORE_SSH_KEY" "$BORE_VM_USER@$BORE_VM" \
        'sha256sum $HOME/bore 2>/dev/null | cut -c1-16; $HOME/bore --version 2>/dev/null | head -1' \
        2>/dev/null | tr '\n' ' ')"

baseline open-start

# WHY THE STAGE LIST IS A PARAMETER AND NOT A FIXED SEQUENCE
# -----------------------------------------------------------
# Three of these four stages need a change to the script they invoke before
# they answer anything, and those changes are written while the first stage is
# already measuring. A driver is read INCREMENTALLY by the shell running it, so
# editing this file mid-run is how a driver executes a line that did not exist
# when the line above it ran -- and the alternative that looks safe is worse:
# leaving the three listed here unconditionally means the driver reaches them
# with the UNMODIFIED scripts, runs them, and writes a `_done.` marker over a
# result that answers the old question under the new question's name. A marker
# on the wrong answer is more expensive than no answer at all.
#
# So the set is named explicitly. `STAGES=all` once every script is ready.
STAGES="${STAGES:-conns_var}"
want() { case " $STAGES " in *" all "*) return 0;; *" $1 "*) return 0;; *) return 1;; esac; }

# 1. The one that unblocks a published claim. FIRST, because a driver that runs
#    out of window must lose the others and not this.
want conns_var  && run_stage pub_ws_conns_var        5400 scripts/perf/staging/pub/ws_conns_var.sh
# 1b. Straight out of 1's first repetition: the two arms trip DIFFERENT instance
#     limits for the same payload, and a packets-per-second limit is a statement
#     about packet size. Short, and it runs early because it is the only stage
#     here whose outcome could imply a code change.
#     It also closes a gap stage 1 could not: stage 1 brackets the server and the
#     VM, and NEITHER is the receiver -- on a download the bytes end on this
#     workstation, whose NIC drops and TCP receive queues no stage in this
#     campaign had ever read around a transfer. `udp_pktsize` runs the identical
#     snapshot at all THREE hosts, at the same n=4 that stage 1 finds unstable.
want pktsize    && run_stage pub_udp_pktsize         2400 scripts/perf/staging/pub/udp_pktsize.sh
# 2. The delay axis. Three points, not four: 5 s sits between 0 and 20 and buys
#    the least, and each point costs a freshly registered tunnel plus five
#    transfers -- a tunnel has exactly one first connection, so the axis cannot
#    be walked on one link.
want first_conn && run_stage pub_ws_first_conn_delay 5400 scripts/perf/staging/pub/ws_first_conn.sh DELAYS="0 20 60" REPS=3
# 3. Relay carriers where the question is still open. FOUR cells, not six, and
#    twelve repetitions instead of three: the carriers=2 middle rung is dropped
#    because the open question is whether carriers recover AT ALL when the flows
#    are as many as the carriers, and dispersion needs repetitions rather than
#    neighbours (§42.5). The relay's spread on one cell reaches 3,2x, so three
#    repetitions cannot separate two medians no matter how many cells there are.
want carriers   && run_stage vpn_carriers_relay_deep 7200 scripts/perf/staging/vpn/vpn_carriers.sh \
    REPS=12 CELLS_SPEC="relay|1|4 relay|4|4 relay|1|8 relay|4|8"
# 4. The second baseline, after the return to direct.
want jump_stab  && run_stage jump_stab_r2            3600 scripts/perf/staging/jump/jump_stab.sh

baseline open-end
say "################ OPEN QUESTIONS complete ################"
