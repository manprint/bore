#!/usr/bin/env bash
# P6: the SSH jump host campaign -- the one mode where BANDWIDTH IS NOT THE
# PRODUCT.
#
# WHY A DRIVER
# ------------
# `jump_lat.sh` was written and never wired to anything, which is the same
# defect as a gate nobody runs: it exists, it is correct, and it has never
# produced a number. Everything else in this campaign runs from a driver that
# records provenance, holds a per-stage timeout, leaves a resume marker and
# refuses to share the link with another driver. A stage invoked by hand has
# none of that, so its result cannot be compared with the ones that do.
#
# THE FIVE QUESTIONS, AND THE THREE STAGES THAT ANSWER THEM
# ----------------------------------------------------------
# The campaign plan (`docs/performance/ETH_CAMPAIGN_PLAN.md` §P6) asks five
# things of the jump host. For a long time ONE stage was wired, covering four of
# them and leaving the fifth -- the one whose failure a user experiences as "my
# session dropped" -- entirely unmeasured. The mapping is now explicit, so a
# question without a stage is visible in this file rather than discovered later:
#
#   1 session-open decomposition ....... jump_lat   (tcp / wchan / open)
#   2 application RTT on a live session  jump_lat   (chan / echo)
#   3 relay against direct QUIC ........ jump_lat   (arms, interleaved)
#   4 the cost of carriers ............. jump_lat   (direct4 arm)
#   5 stability: rekey, and warm-relay
#     fallback when the UDP dies ....... jump_stab
#   + channel isolation under bulk ..... jump_hol
#
# The last row is not in the plan's list and is here anyway: this repository
# VENDORS russh to fix per-channel head-of-line blocking, and that fix has only
# unit gates. An in-process test false-passes exactly this class -- loopback
# drains opportunistically and never builds the queue -- so until `jump_hol`
# there was no real-path evidence that the shipped fix works where it matters.
#
# WHAT IS MEASURED, AND WHY IT IS NOT THROUGHPUT
# ----------------------------------------------
# An `ssh -J` carries an interactive session and a `direct-tcpip` channel, so
# what a user feels is the ROUND TRIP. `jump_lat.sh` therefore records five
# nested times per repetition -- TCP connect, `ssh -W` (outer handshake plus the
# channel), full `ssh -J` (plus the inner handshake), a new channel on an
# established session, and a byte echoed on a live one -- so the gateway's own
# cost (`wchan - tcp`) is separable from the inner sshd's (`open - wchan`),
# which this project cannot change and must not be blamed for.
#
# THREE ARMS, INTERLEAVED INSIDE EACH REPETITION: warm TCP relay, server-direct
# QUIC, and direct with `--carriers 4`. The third exists because an SSH channel
# uses exactly ONE bidi stream, so carriers here can only buy isolation -- the
# open question is whether they COST latency, and an unasked question has no
# answer. Every direct arm is VERIFIED from the admin API before it is sampled:
# a provider always starts on the warm relay, so an unverified "direct" arm is a
# relay measurement wearing the wrong label.
#
# ORDER IS DELIBERATE. `jump_stab` runs LAST because it is the only stage that
# installs a root firewall rule on this workstation (an nft table dropping UDP
# to the test VM, so a live direct path dies while the TCP relay to the same
# host survives). It removes it in an EXIT trap and again in its own preflight,
# but a stage that edits the host's network plane belongs after the ones that
# only measure it.
#
# PREREQUISITE: the gateway binary needs `--features vpn,ssh-gateway`, built
# into `target/jump` and deployed to the TEST VM. Never to staging -- a staging
# redeploy restarts the server and drops the user's live tunnels, which requires
# explicit approval and is not part of this campaign. `jump_build_and_deploy`
# does the deploy; this driver only refuses to run beside another one.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

export BORE_PERF_OUT="$REPO/out/eth"
mkdir -p "$BORE_PERF_OUT"
LOG="$BORE_PERF_OUT/_driver_jump.log"
export LC_ALL=C

say() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

# The contention guard lives in ONE file, sourced here. It used to be copied
# into each driver, and the copies shared a matching rule that reported a
# MONITOR reading this driver's log as the driver itself -- see driverlib.sh.
# shellcheck disable=SC1091
# Repo-root relative, NOT `dirname $BASH_SOURCE`: every driver has already
# `cd`-ed to the repo root by this point, so a relative BASH_SOURCE would be
# resolved against the NEW directory and only work when the driver happened
# to be invoked from the root.
. scripts/perf/staging/driverlib.sh

found="$(other_driver)"
if [ -n "$found" ]; then
    say "REFUSING: another campaign driver is running -- they must not share the link."
    printf '%s\n' "$found" | sed 's/^/    /'
    exit 3
fi

baseline() {
    local tag="$1" d u
    ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
        "$BORE_VM_USER@$BORE_VM" \
        "pgrep -x iperf3 >/dev/null || (nohup iperf3 -s -p 5299 >/dev/null 2>&1 &); sleep 1" \
        >/dev/null 2>&1
    d=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299 -R -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    sleep 2
    u=$(timeout 40 iperf3 -c "$BORE_VM" -p 5299    -P 1 -t 8 -J 2>/dev/null \
        | jq -r '.end.sum_received.bits_per_second/1e6|floor' 2>/dev/null)
    say "BASELINE[$tag] download=${d:-FAILED} upload=${u:-FAILED} Mbit/s"
}

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

say "################ SSH jump host (P6) ################"
say "repo:    $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null) $(git -C "$REPO" status --porcelain 2>/dev/null | wc -l) file(s) dirty"
# `CARGO_TARGET_DIR=target/jump` puts the artefact at `target/jump/RELEASE/bore`.
# The path here was missing that component, so `sha256sum` failed into
# `2>/dev/null` and the provenance line printed an EMPTY checksum and an EMPTY
# version -- observed on the first P6 run as `binary:` followed by nothing. A
# provenance line that cannot find its subject has to SAY so: a campaign whose
# repeatability rests on recording which binary produced a number cannot have
# that record fail silently.
jump_bin="$REPO/target/jump/release/bore"
if [ -r "$jump_bin" ]; then
    say "binary:  $(sha256sum "$jump_bin" | cut -c1-16) $("$jump_bin" --version 2>/dev/null | head -1)"
else
    say "binary:  NOT BUILT YET at target/jump/release/bore (the first stage builds it)"
fi
say "nic:     $(ip route show default | awk '/^default/{print $5; exit}') speed=$(cat /sys/class/net/"$(ip route show default | awk '/^default/{print $5; exit}')"/speed 2>/dev/null) Mbit/s"
say "far bin: $(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
        -i "$BORE_SSH_KEY" "$BORE_VM_USER@$BORE_VM" \
        'sha256sum $HOME/bore-jump 2>/dev/null | cut -c1-16; $HOME/bore-jump --version 2>/dev/null | head -1' \
        2>/dev/null | tr '\n' ' ')"

# The line is read even though this campaign measures latency: a link that has
# started misbehaving shows up here first, and a latency figure taken on a
# degraded line is as wrong as a throughput one.
baseline p6-start

run_stage jump_lat  3600 scripts/perf/staging/jump/jump_lat.sh
run_stage jump_hol  3600 scripts/perf/staging/jump/jump_hol.sh
run_stage jump_stab 5400 scripts/perf/staging/jump/jump_stab.sh

baseline p6-end

# LEAVE THE WORKSTATION AS WE FOUND IT. The inner SSH target is a container
# holding a live public key and an sshd; the stages start it on demand (and
# deliberately leave it up between them, since each runs as its own process),
# so the driver is the one place that can know the campaign is over. Removing
# it here is what keeps the standing rule -- a benchmark leaves no daemon
# behind -- true for this campaign too.
say "removing the inner SSH target"
"$REPO/scripts/perf/staging/jump/jump_inner_target.sh" down >/dev/null 2>&1 || true

say "################ done ################"
