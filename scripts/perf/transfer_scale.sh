#!/usr/bin/env bash
# How does `bore transfer` scale with the SHAPE of the payload, at constant bytes?
#
# The question this answers is the one a user actually has: "I have 4 GiB to
# move — does it matter whether that is one file or fifty thousand?" It should
# not matter much; the transport is the same and the bytes are the same. Where
# it does matter, the cost is in the transfer's own bookkeeping, and this
# harness is what makes that cost visible instead of inferred.
#
# Everything runs on loopback against a local relay server, deliberately: a
# loopback path takes the NETWORK out of the measurement, so what is left is
# the CPU and the syscalls the transfer itself spends per file and per chunk.
# A WAN number measured with a slow link tells you about the link. This tells
# you about the code.
#
#   scripts/perf/transfer_scale.sh                 # the default sweep
#   FILES="1 100 10000" MB=512 scripts/perf/transfer_scale.sh
#   PAR="1 4 16" FILES=2000 scripts/perf/transfer_scale.sh
#
# Env:
#   MB        total payload per run, MiB (default 512)
#   FILES     file counts to sweep (default "1 10 1000 10000")
#   PAR       --parallel values to sweep (default "8")
#   REPS      repetitions per cell (default 1; the median is reported)
#   KEEP=1    keep the run directory
#   BORE_BIN  binary under test (default target/release/bore)
#
# Output: one row per cell with wall seconds, MB/s, and the CPU seconds each
# side spent — because two runs at the same MB/s with different CPU bills are
# not the same result, and the whole point of a shape sweep is to find where
# the CPU goes.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="${BORE_BIN:-$ROOT/target/release/bore}"
MB="${MB:-512}"
FILES="${FILES:-1 10 1000 10000}"
PAR="${PAR:-8}"
REPS="${REPS:-1}"
PORT="${PORT:-47851}"
# Throwaway HMAC secret for a 127.0.0.1-only relay server — not a credential, and
# deliberately a literal: it is also the discriminator that makes this harness's own
# processes safe to kill by pattern (`pgrep -f -- '--secret xferscale'`) without a blanket
# `pkill bore` that would take down whatever else the operator has running.
SEC="${SEC:-xferscale}"

[ -x "$BIN" ] || { echo "no bore binary at $BIN — run: cargo build --release" >&2; exit 2; }

RUN=$(mktemp -d -p "${TMPDIR:-/tmp}" xfersc.XXXXXX)
SRV=""
cleanup(){
    [ -n "$SRV" ] && kill "$SRV" 2>/dev/null
    [ "${KEEP:-0}" = 1 ] || rm -rf "$RUN"
}
trap cleanup EXIT

med(){ sort -n | awk '{v[NR]=$1} END{ if(NR==0){print "-";exit} print (NR%2)?v[(NR+1)/2]:(v[NR/2]+v[NR/2+1])/2 }'; }
# CPU seconds to two decimals: a whole-second reading is useless here, because
# a fast cell finishes in about a second and every bill would round to 0 or 1.
cpu_of(){ # <pid> -> CPU seconds (user+sys), two decimals
    local p=$1
    [ -r "/proc/$p/stat" ] || { echo 0; return; }
    awk '{printf "%.2f", ($14+$15)/'"$(getconf CLK_TCK)"'}' "/proc/$p/stat" 2>/dev/null || echo 0
}

# One tree of `n` files totalling MB MiB. Sizes are equal, so the only thing
# that varies across the sweep is the file COUNT — the whole design of the
# experiment is that one variable moves.
make_tree(){ # <dir> <n>
    local dir=$1 n=$2
    rm -rf "$dir"; mkdir -p "$dir"
    python3 - "$dir" "$n" "$MB" <<'PY'
import os, sys
d, n, mb = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
total = mb * 1024 * 1024
each = max(1, total // n)
# One 64 KiB random block, reused: the payload must not be compressible in a
# way that varies across the sweep, and generating 512 MiB of fresh entropy per
# cell would cost more than the measurement.
block = os.urandom(65536)
for i in range(n):
    left = each
    with open(os.path.join(d, f"f{i:07d}.bin"), "wb") as fh:
        while left > 0:
            fh.write(block[: min(len(block), left)])
            left -= min(len(block), left)
PY
}

# Extra flags appended verbatim to each side, so one sweep can be re-taken under a
# different policy (e.g. LIS_EXTRA=--no-fsync) without editing the harness.
LIS_EXTRA="${LIS_EXTRA:-}"
SND_EXTRA="${SND_EXTRA:-}"

one_run(){ # <files> <parallel> -> "wall mbs cpu_send cpu_recv"
    local n=$1 par=$2
    local id="sc$$-$n-$par-$RANDOM"
    local dst="$RUN/dst-$n-$par"
    rm -rf "$dst"; mkdir -p "$dst"
    RUST_LOG=warn "$BIN" transfer listener --to "http://127.0.0.1:$PORT" --secret "$SEC" \
        --transfer-id "$id" --dest-path "$dst" --relay-only $LIS_EXTRA > "$RUN/lis.log" 2>&1 &
    local lis=$!
    # The listener must be registered before the sender dials, or the sender
    # fails the rendezvous rather than the transfer.
    local i
    for i in $(seq 100); do grep -q "waiting for transfer" "$RUN/lis.log" 2>/dev/null && break; sleep 0.1; done
    local t0 t1
    t0=$(date +%s.%N)
    RUST_LOG=warn "$BIN" transfer sender --to "http://127.0.0.1:$PORT" --secret "$SEC" \
        --transfer-id "$id" --sources "$RUN/src" --parallel "$par" --relay-only $SND_EXTRA \
        > "$RUN/snd.log" 2>&1 &
    local snd=$!
    # Sample CPU just before each process exits: /proc/<pid>/stat is gone after.
    local cs=0 cl=0
    while kill -0 $snd 2>/dev/null; do
        cs=$(cpu_of $snd); cl=$(cpu_of $lis)
        sleep 0.2
    done
    wait $snd; local rc=$?
    t1=$(date +%s.%N)
    wait $lis 2>/dev/null
    if [ "$rc" != 0 ]; then
        echo "FAILED rc=$rc $(tail -2 "$RUN/snd.log" | tr '\n' ' ')"
        return 1
    fi
    local wall mbs
    wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
    mbs=$(awk -v mb="$MB" -v w="$wall" 'BEGIN{printf "%.1f", (w>0)? mb/w : 0}')
    echo "$wall $mbs $cs $cl"
}

echo "### transfer shape sweep: ${MB} MiB per run, $(basename "$BIN") $("$BIN" --version | awk '{print $4}') — $(date -Is)"
RUST_LOG=warn "$BIN" server --secret "$SEC" --control-port "$PORT" > "$RUN/srv.log" 2>&1 &
SRV=$!
sleep 1
kill -0 $SRV 2>/dev/null || { echo "server failed to start:"; cat "$RUN/srv.log"; exit 1; }

printf '  %-8s %-4s %8s %10s %9s %9s %12s %10s\n' files par wall_s MB/s cpu_send cpu_recv per_file_ms cpu_s_per_GiB
for n in $FILES; do
    make_tree "$RUN/src" "$n"
    for par in $PAR; do
        walls=(); mbss=(); css=(); cls=()
        for _ in $(seq "$REPS"); do
            out=$(one_run "$n" "$par") || { echo "  $n $par  $out"; continue 2; }
            walls+=("$(echo "$out" | awk '{print $1}')")
            mbss+=("$(echo "$out" | awk '{print $2}')")
            css+=("$(echo "$out" | awk '{print $3}')")
            cls+=("$(echo "$out" | awk '{print $4}')")
        done
        w=$(printf '%s\n' "${walls[@]}" | med)
        m=$(printf '%s\n' "${mbss[@]}" | med)
        cs=$(printf '%s\n' "${css[@]}" | med)
        cl=$(printf '%s\n' "${cls[@]}" | med)
        pf=$(awk -v w="$w" -v n="$n" 'BEGIN{printf "%.3f", (n>0)? w*1000/n : 0}')
        tot=$(awk -v a="$cs" -v b="$cl" 'BEGIN{printf "%.2f", a+b}')
        pg=$(awk -v t="$tot" -v mb="$MB" 'BEGIN{printf "%.2f", t*1024/mb}')
        printf '  %-8s %-4s %8s %10s %9s %9s %12s %10s\n' "$n" "$par" "$w" "$m" "$cs" "$cl" "$pf" "$pg"
    done
done
echo
echo DONE
