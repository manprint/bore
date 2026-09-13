#!/usr/bin/env bash
# Re-run every WORKSTATION-SIDE performance stage over the wired link.
#
# WHY THIS EXISTS
# ---------------
# Every campaign in this repository that used the workstation as a traffic
# endpoint ran it over WiFi, and on 2026-09-12 the wired baseline came back
# 928 Mbit/s down / 737 up against the 150/414 the VPN campaign had recorded as
# "bare". A bare control that is itself the bottleneck does not just shrink the
# absolute numbers -- it can hide a ceiling entirely, because a tunnel that
# delivers 92% of a crippled path looks healthy and a tunnel that delivers 40%
# of a real one does not. So every stage whose data crossed this workstation's
# access link is re-run here, and nothing else is: the `vm/`, `pub/vm_*` and
# `srv/` stages ran VM-to-server inside one AWS region and never touched it.
#
# WHAT IT GUARANTEES
# ------------------
#  * SERIAL. One stage at a time, always. Two stages would contend for the one
#    access link being measured, and the netns-based ones share ns0/ns1/ns2 by
#    name -- a second run's pre-cleanup wipes the first's namespaces mid-flight
#    and fabricates failures.
#  * ISOLATED RESULTS. `BORE_PERF_OUT` points at out/eth/, so the WiFi evidence
#    the published documents cite stays exactly where it is and the two can be
#    read side by side.
#  * RESUMABLE. A stage that finished leaves a marker; re-running the driver
#    skips it. A stage that failed leaves none, so it is retried.
#  * BOUNDED. Every stage runs under `timeout`. A harness that wedges costs its
#    own budget and not the whole night.
#  * DRIFT VISIBLE. A bare iperf3 control is re-read before every block and
#    written into the log, because a number measured at hour six is only
#    comparable to one from hour one if the line did not move in between.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

# shellcheck disable=SC1090
. ~/.config/bore-perf/env.sh || { echo "no ~/.config/bore-perf/env.sh" >&2; exit 2; }

export BORE_PERF_OUT="$REPO/out/eth"
mkdir -p "$BORE_PERF_OUT"
LOG="$BORE_PERF_OUT/_driver.log"
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

# A bare control, read the same way in every block so drift is a number and not
# an impression. Upload and download are separate facts on this link.
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

# run_stage <name> <timeout-seconds> <script> [env assignments...]
# The env assignments are applied to THIS stage only; a stage that needs a
# different rate ladder than the shipped default says so at the call site, so
# the published default stays whatever the script itself declares.
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
        touch "$marker"
        say "END   $name rc=0 elapsed=${el}s"
    else
        say "FAIL  $name rc=$rc elapsed=${el}s  (see $name.out; no marker, will retry)"
    fi
    # Every stage claims to leave the host clean. Verify it rather than trust it.
    # `grep -c` exits 1 on no match, which under `set -u`/pipefail would end the
    # driver mid-sweep, so both counts are guarded -- the same trap that once
    # truncated the netns suite into a green run (measurement rule 6).
    local ifaces routes
    ifaces=$(ip -br link 2>/dev/null | { grep -c '^bore' || true; })
    routes=$(ip route 2>/dev/null | { grep -c 'bore' || true; })
    if [ "${ifaces:-0}" != 0 ] || [ "${routes:-0}" != 0 ]; then
        say "      host-check: LEFTOVER bore ifaces=$ifaces routes=$routes"
    fi
    sleep 10   # let the far end's allowance bucket and the local sockets settle
    return 0
}

say "################ ethernet re-run begins ################"
# Provenance. A number is only repeatable if the thing that produced it is
# identified, so the commit, the binary and the link are recorded before any
# measurement rather than reconstructed from memory afterwards.
say "repo:    $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null) $(git -C "$REPO" status --porcelain 2>/dev/null | wc -l) file(s) dirty"
say "binary:  $(sha256sum "$REPO/target/release/bore" 2>/dev/null | cut -c1-16) $("$REPO/target/release/bore" --version 2>/dev/null | head -1)"
say "egress:  $(ip route get 1.1.1.1 2>/dev/null | head -1)"
say "nic:     $(ip route show default | awk '/^default/{print $5; exit}') speed=$(cat /sys/class/net/"$(ip route show default | awk '/^default/{print $5; exit}')"/speed 2>/dev/null) Mbit/s"

# ---------------------------------------------------------------- block A: VPN
# The most distorted campaign: the workstation is one of the two VPN peers, so
# its access link is in the data path of every single stage.
say "===== BLOCK A: VPN ====="
baseline A-start

# THE stage every "percent of bare" in this repository depends on, and it was
# wired to no driver at all -- it had been run by hand, left a `.out` with no
# marker, and its answer sat unread. `link_baseline` qualifies the line; this
# one answers WHOSE the ceiling is, by offering the same load to three
# independent destinations and reading the far end's allowance counters as a
# delta around each VM arm. First, because every later number quotes it.
run_stage asym_qualify       2400 scripts/perf/staging/asym_qualify.sh
run_stage link_baseline      900  scripts/perf/staging/vpn/link_baseline.sh
run_stage vpn_ab            3600  scripts/perf/staging/vpn/vpn_ab.sh
run_stage vpn_direct_deficit 3600 scripts/perf/staging/vpn/vpn_direct_deficit.sh
# The shipped ladder tops out at 540M because that was already above the WiFi
# bare. The wired uplink delivers ~737, so the ladder is extended to find where
# the tunnel actually turns over instead of where the radio did.
run_stage vpn_wire_ceiling  5400  scripts/perf/staging/vpn/vpn_wire_ceiling.sh \
          RATES="300M 450M 600M 700M 800M"
run_stage vpn_txqueue       3600  scripts/perf/staging/vpn/vpn_txqueue.sh
run_stage vpn_sndbuf        3600  scripts/perf/staging/vpn/vpn_sndbuf.sh
run_stage vpn_udpbuf        3600  scripts/perf/staging/vpn/vpn_udpbuf.sh
run_stage vpn_cc_matrix     3600  scripts/perf/staging/vpn/vpn_cc_matrix.sh
run_stage vpn_lat           1800  scripts/perf/staging/vpn/vpn_lat.sh
run_stage vpn_profile       1800  scripts/perf/staging/vpn/vpn_profile.sh
run_stage vpn_modes         2400  scripts/perf/staging/vpn/vpn_modes.sh
run_stage vpn_hub           2400  scripts/perf/staging/vpn/vpn_hub.sh
run_stage vpn_stability     3600  scripts/perf/staging/vpn/vpn_stability.sh

# ------------------------------------------------------------- block B: secret
# Peer-to-peer: the workstation is one of the two peers (S-1 -- the server is
# not on the direct path), so the access link is in the data path here too.
say "===== BLOCK B: secret ====="
baseline B-start
run_stage sec_ab            3600  scripts/perf/staging/sec/sec_ab.sh
run_stage sec_eff           3600  scripts/perf/staging/sec/sec_eff.sh
run_stage sec_lat           1800  scripts/perf/staging/sec/sec_lat.sh
run_stage sec_ack           2400  scripts/perf/staging/sec/sec_ack.sh
run_stage sec_ttd           1800  scripts/perf/staging/sec/sec_ttd.sh

# -------------------------------------------------- block C: vhost, ws-side
say "===== BLOCK C: vhost (workstation side) ====="
baseline C-start
run_stage ws_rtt             900  scripts/perf/staging/ws/ws_rtt.sh
run_stage ws_ref            1800  scripts/perf/staging/ws/ws_ref.sh
run_stage ws_ref_vm         1800  scripts/perf/staging/ws/ws_ref_vm.sh
run_stage ws_ref_public     1800  scripts/perf/staging/ws/ws_ref_public.sh
run_stage ws_tunnel_paired  3600  scripts/perf/staging/ws/ws_tunnel_paired.sh
run_stage ws_tunnel         2400  scripts/perf/staging/ws/ws_tunnel.sh
run_stage ws_dl_parallel    2400  scripts/perf/staging/ws/ws_dl_parallel.sh
run_stage ws_dufs           2400  scripts/perf/staging/ws/ws_dufs.sh
run_stage ws_dufs_rq        3600  scripts/perf/staging/ws/ws_dufs_relay_vs_quic.sh
run_stage ws_flavours_rot   3600  scripts/perf/staging/ws/ws_flavours_rotated.sh
run_stage ws_flavours       3600  scripts/perf/staging/ws/ws_flavours.sh

# ------------------------------------------------- block D: public, ws-side
say "===== BLOCK D: public (workstation side) ====="
baseline D-start
run_stage pub_ws_pub        2400  scripts/perf/staging/pub/ws_pub.sh
run_stage pub_ws_asym       2400  scripts/perf/staging/pub/ws_asym.sh
run_stage pub_ws_dl         2400  scripts/perf/staging/pub/ws_dl.sh
run_stage pub_ws_dl1        2400  scripts/perf/staging/pub/ws_dl1.sh
run_stage pub_ws_carr       2400  scripts/perf/staging/pub/ws_carr.sh
run_stage pub_ws_conns      2400  scripts/perf/staging/pub/ws_conns.sh
# Wired HERE and not left loose, which is the `asym_qualify` lesson: that stage
# was attached to no driver, left a `.out` with no marker, and its answer sat
# unread for a whole campaign. This one attributes the first-connection cost
# `ws_dl1` reproduced twice (evidence §35.3) -- 6 arms, every one a download,
# so ~2.25 GiB of egress at the default size.
run_stage pub_ws_first_conn 2400  scripts/perf/staging/pub/ws_first_conn.sh
# Attributes the n=8 rung of the ladder, where the two transports land on the
# same number within 0.3 % and therefore stop measuring bore (evidence §36.3).
# 4 arms, all downloads: ~1.8 GiB of egress.
run_stage pub_origin_cpu    2400  scripts/perf/staging/pub/origin_cpu.sh
# `origin_cpu` excluded CPU on all three hosts, which narrows the n=8 question
# without answering it -- an event loop that serialises binds on LATENCY and
# looks idle while it does. This stage varies the one thing left on the client
# side: 1 process x n connections against n processes x 1 connection. 24 cells,
# 460 MiB each, all downloads: ~11 GiB of egress.
run_stage pub_ws_conns_procs 3600 scripts/perf/staging/pub/ws_conns_procs.sh

# ----------------------------------------------------------- block E: transfer
say "===== BLOCK E: transfer ====="
baseline E-start
run_stage xfer_bw           3600  scripts/perf/staging/xfer/xfer_bw.sh
run_stage xfer_shape        3600  scripts/perf/staging/xfer/xfer_shape.sh
run_stage xfer_sota         3600  scripts/perf/staging/xfer/xfer_sota.sh

baseline final
say "################ ethernet re-run complete ################"
say "results: $BORE_PERF_OUT"
