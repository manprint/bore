#!/usr/bin/env bash
# P5: the BUILD WINDOW -- the only phase in this campaign that is allowed to
# spend CPU, and the only one that produces no measurement.
#
# WHY IT IS A SCRIPT AND NOT A LIST OF COMMANDS
# ---------------------------------------------
# Everything else in this campaign runs from a driver that records provenance,
# bounds each step with a timeout, leaves a resume marker and refuses to share
# the machine with a driver that is measuring. The build window was the one
# phase still done by hand, and it is the phase most likely to be repeated: a
# campaign that finds a defect fixes it here, and then has to come back. A
# hand-run build window also has no record of WHICH binary the stages after it
# measured, which is precisely the provenance every stage log prints.
#
# THE CONTENTION RULE IS NOT ADVISORY
# -----------------------------------
# "Mai compilare mentre una fase misura. La CPU e' parte dello strumento."
# A `cargo build -j N` saturates the machine that is also one end of the flow
# being measured, so a stage that overlaps a build is not a slow stage: it is a
# corrupted one, and nothing in its output says so. This script therefore
# refuses to start beside any campaign driver, by PROCESS IDENTITY rather than
# by text (a text match on a script name matches the shell running the check --
# measured twice in this campaign, once as a guard that never started and once
# as a `pkill` that killed the session issuing it).
#
# FOOTPRINT
# ---------
# `nice -n 19`, `-j 3` and `--test-threads=2` are not politeness: the
# workstation is also a desktop and the standing campaign rule is to keep its
# CPU/RAM footprint low. A build that makes the box unusable is a build that
# gets interrupted.
#
# ORDER, AND WHY IT IS THIS ORDER
# -------------------------------
#   fmt     -- cheapest, and a formatting failure is the one that would
#              otherwise be discovered by CI after everything else passed
#   clippy  -- typechecks the whole crate INCLUDING tests (`--all-targets`), so
#              it is the first step that can reject uncompiled test code; it is
#              deliberately before `cargo test`, which would otherwise spend
#              minutes building before reporting the same error
#   test    -- the unit/integration gates
#   build   -- release binary for the netns harness and for the stages after
#              this window; NEVER while a stage is running (see above)
#   netns   -- the field gates, each `sudo -n` on an EXACT path: the sudoers
#              entry is per-path and `sudo bash scripts/...` PROMPTS, which in
#              a non-interactive driver is a hang, not a failure
#
# Every step leaves a marker, so an interrupted window resumes instead of
# restarting. Delete `out/eth/_done.p5.*` to force a step to re-run.
set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../../.." || exit 2
REPO="$PWD"

OUT="${BORE_PERF_OUT:-$REPO/out/eth}"
mkdir -p "$OUT"
LOG="$OUT/_driver_p5.log"
export LC_ALL=C

# Cargo's own parallelism knob plus the test harness's. Overridable for a box
# with a different budget, but the defaults are the campaign's standing rule.
JOBS="${P5_JOBS:-3}"
TEST_THREADS="${P5_TEST_THREADS:-2}"
FEATURES="${P5_FEATURES:-vpn}"
# P6's gateway needs the ssh-gateway feature too, and it gets its OWN target
# directory so the campaign binary at `target/release/bore` never moves.
JUMP_FEATURES="${P5_JUMP_FEATURES:-vpn,ssh-gateway}"
JUMP_TARGET_DIR="${JUMP_TARGET_DIR:-$REPO/target/jump}"
# The netns harnesses share ns0/ns1/ns2 and MUST run serially; this script runs
# them one after another and never in parallel. Set to empty to skip them.
NETNS="${P5_NETNS:-scripts/vpn_netns_test.sh}"

say() { printf '%s %s\n' "$(date -Is)" "$*" | tee -a "$LOG"; }

# The contention guard lives in ONE file and this script is its fifth caller.
# It used to carry its own copy, matching `ps -eo pid,args` against the drivers'
# names as TEXT -- which is how `rerun_eth_p7.sh` once refused to start by
# naming a MONITOR that merely mentioned a driver in its grep pattern (trap 25).
# Any shell command in this session that types `rerun_eth.sh` -- a `pgrep`, a
# `tail | grep` -- was enough to lock the build window out, and the message it
# printed was indistinguishable from a real conflict.
#
# `driverlib.sh` identifies a process POSITIONALLY, from argv[0]/argv[1] of
# `/proc/<pid>/cmdline`, and its list includes this script, so the drivers
# refuse to start while a build is running exactly as this refuses to start
# while one is measuring. The path is repo-root relative because this script
# `cd`s to the root above.
# shellcheck disable=SC1091
. scripts/perf/staging/driverlib.sh

found="$(other_driver)"
if [ -n "$found" ]; then
    say "REFUSING: a campaign driver is measuring -- the CPU is part of the instrument."
    printf '%s\n' "$found" | sed 's/^/    /'
    exit 3
fi

# A live bore process at either end of a measured link is also a reason not to
# rebuild: `cargo build` replaces `target/release/bore` under a running stage.
# Reported, not enforced -- the user's own tunnels are not this script's to kill
# and the campaign rule is never to blanket-match `bore`.
live=$(pgrep -af 'target/release/bore|target/jump/bore' 2>/dev/null | head -5)
if [ -n "$live" ]; then
    say "NOTE: bore processes from this repository are running:"
    printf '%s\n' "$live" | sed 's/^/    /'
    say "      a rebuild replaces the binary under them; stop them first if a stage owns them."
fi

# A MARKER IS ONLY VALID FOR THE SOURCE THAT PRODUCED IT.
#
# The binary-gate rule below says a marker is only valid for the binary that
# produced it. The SAME rule applies one level up and was missing: `fmt`,
# `clippy_*` and `test_*` all attest something about `src/` and `tests/`, and
# their markers survived an edit to either.
#
# MEASURED on 2026-09-13, in this gate, while it was running: three consecutive
# invocations fixed a clippy error, then a failing unit test, then a failing
# e2e test -- and the fourth invocation printed `SKIP fmt`, `SKIP clippy_default`,
# `SKIP clippy_vpn`, `SKIP clippy_jump` although `src/secret.rs` and
# `tests/e2e_test.rs` had been edited AFTER those markers were written. The
# window reported itself green having last checked formatting and lints against
# code that no longer existed. (It happened to be sound that day only because
# the same commands had been run by hand in between -- which is luck, not a
# gate.)
#
# So a source-dependent step's marker carries the fingerprint of the tree it
# attests, and is ignored when the tree has moved. The fingerprint is the set of
# (path, mtime, size) over the tracked source, which is cheap and does not need
# git: a step must re-run for an UNCOMMITTED edit, which is exactly the state
# this window works in.
source_fingerprint() {
    find "$REPO/src" "$REPO/tests" "$REPO/crates" -name '*.rs' -type f -printf '%p %T@ %s\n' \
        2>/dev/null | LC_ALL=C sort | sha256sum | cut -c1-16
}
SRC_FP="$(source_fingerprint)"
# Steps whose verdict is about the SOURCE. `build_*` are deliberately absent:
# cargo already decides for itself whether to rebuild, and the binary-sha record
# below is what carries provenance forward. `netns_*` are absent for the same
# reason as the performance stages -- they are bounded by the binary, and the
# binary rule already covers them.
SRC_STEPS="${P5_SRC_STEPS:-fmt clippy_default clippy_vpn clippy_jump test_default test_vpn test_jump}"

step() {
    local name="$1" tmo="$2"; shift 2
    local marker="$OUT/_done.p5.$name"
    local fp_file="$marker.srcfp"
    local src_dependent=0 st
    for st in $SRC_STEPS; do [ "$st" = "$name" ] && src_dependent=1; done
    if [ -f "$marker" ] && [ "$src_dependent" = 1 ]; then
        if [ "$(cat "$fp_file" 2>/dev/null)" != "$SRC_FP" ]; then
            say "STALE $name -- the source changed since this marker; re-running"
            rm -f "$marker"
        fi
    fi
    if [ -f "$marker" ]; then say "SKIP  $name (marker present)"; return 0; fi
    say "BEGIN $name  (timeout ${tmo}s)"
    local t0=$SECONDS rc
    ( cd "$REPO" && timeout -k 30 "$tmo" "$@" ) >"$OUT/p5_$name.out" 2>&1
    rc=$?
    local el=$((SECONDS - t0))
    if [ $rc -eq 0 ]; then
        touch "$marker"
        [ "$src_dependent" = 1 ] && printf '%s' "$SRC_FP" > "$fp_file"
        say "END   $name rc=0 elapsed=${el}s"
        return 0
    fi
    say "FAIL  $name rc=$rc elapsed=${el}s  (see $OUT/p5_$name.out; no marker, will retry)"
    # The first 40 lines of a compiler error are the error; the rest is the
    # backtrace of the build. Printing them into the driver log means a failure
    # is legible without opening a second file.
    sed -n '1,40p' "$OUT/p5_$name.out" | sed 's/^/    /'
    return 1
}

say "################ P5 build window ################"
say "repo:    $(git -C "$REPO" rev-parse --short HEAD 2>/dev/null) $(git -C "$REPO" status --porcelain 2>/dev/null | wc -l) file(s) dirty"
say "cargo:   $(cargo --version 2>/dev/null)   features=$FEATURES jobs=$JOBS test-threads=$TEST_THREADS"
say "host:    $(nproc) cpu  $(free -m 2>/dev/null | awk '/^Mem:/{print $2"MB"}')"

fail=0

step fmt 300 nice -n 19 cargo fmt --all -- --check || fail=1

# `--all-targets` is what makes this step typecheck the TESTS too. Without it a
# test module that does not compile is discovered by `cargo test` after a full
# dependency build, which is the slowest possible way to learn it.
step clippy_default 1800 nice -n 19 cargo clippy --all-targets -j "$JOBS" -- -D warnings || fail=1
step clippy_vpn 1800 nice -n 19 cargo clippy --all-targets --features "$FEATURES" -j "$JOBS" -- -D warnings || fail=1

# The JUMP feature set is a THIRD combination, and nothing else in this window
# compiles it. P6's gateway needs `--features vpn,ssh-gateway`, and until this
# step existed the first thing to compile that combination was
# `jump_build_and_deploy` -- INSIDE a measurement stage, after a ten-minute
# build, where a clippy error surfaces as a failed stage rather than as a failed
# gate. The build window is where a build is allowed to fail.
# CARGO_TARGET_DIR pinned to the jump tree here too, and that is not cosmetic:
# clippy and build SHARE their dependency artefacts, so linting this feature set
# in the default `target/` would rebuild every feature-sensitive dependency
# (russh arrives with `ssh-gateway`) and then `build_release --features vpn`
# would rebuild them all back. Two full workspace rebuilds, inside the one
# window where the CPU is supposed to be free. Pinning it also WARMS the tree
# that `build_jump` is about to use, so the lint is nearly free.
step clippy_jump 1800 env CARGO_TARGET_DIR="$JUMP_TARGET_DIR" \
    nice -n 19 cargo clippy --all-targets --features "$JUMP_FEATURES" -j "$JOBS" -- -D warnings || fail=1

# A failed clippy means the tests cannot build either; running them would only
# reprint the same error after a long wait.
if [ $fail -eq 0 ]; then
    step test_default 2400 nice -n 19 cargo test -j "$JOBS" -- --test-threads="$TEST_THREADS" || fail=1
    step test_vpn 2400 nice -n 19 cargo test --features "$FEATURES" -j "$JOBS" -- --test-threads="$TEST_THREADS" || fail=1
    # AND THE JUMP FEATURE SET'S TESTS, which this gate used to LINT and never
    # RUN. `clippy_jump` checks `--features vpn,ssh-gateway`, but neither
    # `test_default` (default features) nor `test_vpn` (`--features vpn`)
    # compiles `src/sshgw.rs` at all -- it is behind `ssh-gateway`. So the whole
    # SSH ingress suite (the units in `sshgw.rs` plus `tests/ssh_gateway_test.rs`
    # and `tests/ssh_jump_test.rs`) was outside the gate, and a change there
    # could pass P5 untested. Found 2026-09-13 while adding the I-SSH12 gates:
    # they went green by hand and the gate would never have run them.
    #
    # Same `CARGO_TARGET_DIR` as `clippy_jump` for the same reason given there:
    # sharing the campaign's `target/` would thrash two feature sets against
    # each other and rebuild the world twice inside the one window where the CPU
    # is meant to be free.
    step test_jump 2400 env CARGO_TARGET_DIR="$JUMP_TARGET_DIR" \
        nice -n 19 cargo test --features "$JUMP_FEATURES" -j "$JOBS" -- --test-threads="$TEST_THREADS" || fail=1
fi

if [ $fail -ne 0 ]; then
    say "STOPPING before the release build: a gate failed and the binary would be built from"
    say "code that does not pass its own gates. Fix, then re-run -- passed steps are skipped."
    exit 1
fi

# DEFINED HERE AND NOT BELOW, AND THAT IS THE WHOLE POINT OF THE MOVE.
# bash executes top to bottom: a function is callable only after the line that
# defines it has RUN. This block used to sit AFTER the `build_release` step that
# calls it, so `invalidate_binary_gates` resolved to nothing -- exit 127 on a
# line with no `|| fail=1`, i.e. a silent no-op. The one mechanism written to
# make sure the M-1 fix would actually be verified on the real path would have
# done nothing at all, and would have said nothing about it. Same family as
# every other defect this campaign has paid for: a gate whose failure mode is
# silence.
# A MARKER IS ONLY VALID FOR THE BINARY THAT PRODUCED IT.
#
# `vpn_ctrl_leak` is the field gate for M-1 (one control connection stranded at
# both ends per VPN reconnect). Its verdict is a statement about the PRODUCT, so
# a run against the pre-fix binary answers a different question from a run
# against the fixed one -- and the resume marker cannot tell them apart: a
# driver re-run after the build would print `SKIP vpn_ctrl_leak (marker
# present)` and the fix would never be verified on the real path.
#
# MEASURED in this window: the binary was built 2026-09-12 18:05 and `src/mux.rs`
# carrying the `Liveness`/`TrackedStream` fix was written 2026-09-13 00:23, so
# P7 ran that gate against code that does not contain the fix. That run is the
# BEFORE and is worth keeping; what must not happen is it standing in for the
# AFTER.
#
# So: when the release binary's checksum CHANGES, the markers of stages whose
# verdict is about product behaviour are invalidated. Performance stages are
# deliberately NOT invalidated -- they are the before-and-after comparison and
# re-running them all is the campaign's whole cost; they are listed instead, so
# the provenance is visible rather than assumed.
BIN_GATES="${P5_BIN_GATES:-vpn_ctrl_leak}"
SHA_FILE="$OUT/_binary.sha"
old_sha="$(cat "$SHA_FILE" 2>/dev/null || true)"

invalidate_binary_gates() {
    local new_sha; new_sha="$(sha256sum "$REPO/target/release/bore" 2>/dev/null | cut -c1-16)"
    [ -n "$new_sha" ] || return 0
    printf '%s' "$new_sha" > "$SHA_FILE"
    if [ -z "$old_sha" ]; then
        say "provenance: recorded binary $new_sha (no previous record)"
        return 0
    fi
    if [ "$old_sha" = "$new_sha" ]; then
        say "provenance: binary unchanged ($new_sha) -- every marker stays valid"
        return 0
    fi
    say "provenance: binary $old_sha -> $new_sha"
    local g
    for g in $BIN_GATES; do
        if [ -f "$OUT/_done.$g" ]; then
            mv "$OUT/_done.$g" "$OUT/_done.$g.pre-$old_sha" 2>/dev/null
            say "      invalidated marker for $g (its verdict is about the PRODUCT; the"
            say "      previous run is kept as _done.$g.pre-$old_sha)"
        fi
    done
    say "      NOTE every other marker was produced by $old_sha. Those stages are"
    say "      MEASUREMENTS, not verdicts: they stand as the before-and-after and are"
    say "      not re-run automatically. Re-run one deliberately by removing its marker."
}

step build_release 2400 nice -n 19 cargo build --release --features "$FEATURES" -j "$JOBS" || fail=1
if [ $fail -eq 0 ]; then
    say "binary:  $(ls -l target/release/bore 2>/dev/null | awk '{print $5" bytes"}')  sha=$(sha256sum target/release/bore 2>/dev/null | cut -c1-12)"
    say "version: $(./target/release/bore --version 2>/dev/null)"
    invalidate_binary_gates
fi

# The jump binary, built HERE and not in P6. Two reasons, both measured lessons:
# a build saturates the machine that is one end of every flow (the CPU is part
# of the instrument), and a ten-minute compile inside a stage turns a build
# failure into a measurement failure. `jump_build_and_deploy` still runs in P6 --
# it must, because it is what checksums both ends -- but against a warm
# CARGO_TARGET_DIR it only copies.
#
# Its OWN target dir, never `target/release`: that path is the artefact every
# other stage is measured with and whose checksum their drivers record, and
# changing its feature set would both move the binary under a published
# campaign and force a full rebuild back again the next time a VPN stage ran.
step build_jump 2400 env CARGO_TARGET_DIR="$JUMP_TARGET_DIR" \
    nice -n 19 cargo build --release --features "$JUMP_FEATURES" -j "$JOBS" || fail=1
if [ $fail -eq 0 ]; then
    # The feature must be IN the binary, not merely on the command line. A stale
    # artefact or a silently skipped rebuild would otherwise be measured in P6
    # as a jump host that behaves oddly rather than as one that is not there.
    if "$JUMP_TARGET_DIR/release/bore" server --help 2>/dev/null | grep -q -- '--ssh-gateway'; then
        say "jump:    $(sha256sum "$JUMP_TARGET_DIR/release/bore" 2>/dev/null | cut -c1-12)  --ssh-gateway present"
    else
        say "FAIL  build_jump: the binary has no --ssh-gateway; the feature did not build in"
        rm -f "$OUT/_done.p5.build_jump"
        fail=1
    fi
fi

# The netns harness refuses to run against a release binary older than `src/`,
# so it must come after the build and never before it.
if [ -n "$NETNS" ] && [ $fail -eq 0 ]; then
    for h in $NETNS; do
        [ -x "$REPO/$h" ] || { say "MISS  netns $h (not executable)"; continue; }
        n="$(basename "$h" .sh)"
        # EXACT path under sudo: the NOPASSWD entry is per-path, and
        # `sudo bash scripts/...` prompts -- which here would hang, not fail.
        step "netns_$n" 3600 sudo -n "$REPO/$h" || fail=1
    done
fi

if [ $fail -eq 0 ]; then
    say "P5 COMPLETE -- gates green, binary rebuilt, netns gates run."
else
    say "P5 INCOMPLETE -- see the FAIL lines above."
fi
exit $fail
