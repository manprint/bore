#!/usr/bin/env bash
# Is the public ladder's top rung a property of BORE, or of the INSTRUMENT?
#
# THE QUESTION
# ------------
# `ws_conns` walks 1/2/4/8 connections with `raw_client.py`, which drives every
# connection from ONE asyncio process. At n=4 its two arms differ by 39 %, so
# the client cannot explain THAT rung -- the same process delivers 740 Mbit/s on
# one arm and 532 on the other. But at n=8 both arms converge on ~620 and stop,
# and a single-process driver is a candidate for a shared ceiling in a way it is
# not for a difference BETWEEN arms.
#
# `origin_cpu` has already excluded CPU on all three hosts (origin 3-4 % of one
# core, VM 9-20 % of the whole instance, client 10-17 % of one core). CPU is not
# how an event loop binds, though: a loop that serialises reads is LATENCY-bound
# and looks idle while it does it. So the exclusion narrows the question without
# answering it.
#
# THE EXPERIMENT
# --------------
# One variable, and it is the client's PROCESS TOPOLOGY:
#
#     one   1 process  x n connections   (what `ws_conns` does)
#     many  n processes x 1 connection   (what `ws_tunnel` does with curl)
#
# Same origin, same protocol, same tunnels, same bytes per rung, same line,
# same hour. If `many` beats `one` at n=8, the ladder's top rung was measuring
# `raw_client.py` and every conclusion drawn from it is about python. If the two
# agree, the ceiling is downstream of the client and the instrument is cleared.
#
# BOTH TOPOLOGIES ARE TIMED THE SAME WAY, and the way is the DRIVER'S OWN
# `secs=` -- sum of bytes over the LONGEST process's own transfer time. Reading
# one arm from the driver and the other from the shell would put a difference in
# how the number is DERIVED inside a comparison meant to have one variable.
#
# The shell's wall clock is the cross-check and not the headline, because it
# includes the python interpreter's startup. MEASURED in the smoke run: a 20 MiB
# cell read 59.99 MB/s by wall clock and 68.00 by the driver, a 12 % gap that is
# entirely ~0.2 s of startup on a 0.33 s transfer. At 460 MiB the same 0.2 s is
# ~4 %, and the `many` topology pays it n times IN PARALLEL rather than n times
# in series -- so it is nearly common-mode, but "nearly" is not a basis for a
# ratio this stage exists to publish to three decimal places.
#
# Using the longest process's own time is deliberately CONSERVATIVE for `many`:
# it charges that topology for the whole spread between its processes as if they
# had all run that long. If `many` wins anyway, it has won against a handicap.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C

RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
RP=5053                       # raw_origin.py on the VM (already running)
R=9043; Q=9044                # inside the server's 9000-9100 range
AGG_MB="${AGG_MB:-460}"
REPS="${REPS:-3}"
RUNGS="${RUNGS:-4 8}"
COOL="${COOL:-75}"
WORK="${TMPDIR:-/tmp}/ws_conns_procs.$$"
mkdir -p "$WORK"

UP=()
up() { vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wscprocs-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
      local i; for i in $(seq 80); do adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
        && { UP+=("$1"); return 0; }; sleep 0.5; done; return 1; }
cleanup() {
    local p
    for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done
    rm -rf "$WORK"
}
trap cleanup EXIT

# A cell that failed is the string FAILED, never a number, and never 0.
# A zero that means "the instrument did not run" is this campaign's most
# expensive defect class; it has cost eight stages so far.
keep() { case "${1:-}" in FAILED|""|*[!0-9.]*) return 1;; *) return 0;; esac; }

now() { date +%s.%N; }

# cell <port> <n> <topo> -> MB/s on stdout, or FAILED
# Both topologies request exactly `per * n` bytes so the rungs are comparable.
cell() {
    local port="$1" n="$2" topo="$3" per t0 t1 i rc=0 total
    per=$(( AGG_MB * 1048576 / n ))
    total=$(( per * n ))
    rm -f "$WORK"/c.*
    t0=$(now)
    if [ "$topo" = one ]; then
        python3 "$RAWCLI" get "$BORE_GW" "$port" "$per" "$n" > "$WORK/c.1" 2>/dev/null || rc=1
    else
        for i in $(seq 1 "$n"); do
            python3 "$RAWCLI" get "$BORE_GW" "$port" "$per" 1 > "$WORK/c.$i" 2>/dev/null &
        done
        wait || rc=1
    fi
    t1=$(now)

    # EVERY process must have reported, moved the bytes it was asked for, and
    # reported errs=0. A short read that still prints a rate is the shape that
    # turns a broken cell into a good-looking result.
    local got=0 errs=0 files=0 f b e sc maxsec=0
    for f in "$WORK"/c.*; do
        [ -r "$f" ] || continue
        files=$((files + 1))
        b=$(sed -n 's/.*bytes=\([0-9]*\).*/\1/p' "$f" | head -1)
        e=$(sed -n 's/.*errs=\([0-9]*\).*/\1/p' "$f" | head -1)
        sc=$(sed -n 's/.*secs=\([0-9.]*\).*/\1/p' "$f" | head -1)
        [ -n "$b" ] && [ -n "$sc" ] || { printf 'FAILED n/a'; return 0; }
        got=$((got + b)); errs=$((errs + ${e:-0}))
        maxsec=$(LC_ALL=C awk -v a="$maxsec" -v b="$sc" 'BEGIN{ printf "%.6f", (b+0>a+0)?b:a }')
    done
    local want_files=1; [ "$topo" = many ] && want_files="$n"
    if [ "$rc" != 0 ] || [ "$files" != "$want_files" ] || [ "$errs" != 0 ] || [ "$got" != "$total" ]; then
        printf 'FAILED n/a'
        return 0
    fi
    # "<driver-clock rate> <shell-clock rate>", both from the same transfer.
    LC_ALL=C awk -v b="$got" -v d="$maxsec" -v a="$t0" -v z="$t1" 'BEGIN{
        w = z-a
        if (d+0>0) printf "%.2f", b/1048576/d; else printf "FAILED"
        if (w+0>0) printf " %.2f", b/1048576/w; else printf " n/a" }'
}

# `cell` prints TWO fields: the headline rate and the same transfer read off
# the shell's clock. It cannot set a variable for the second one -- it is called
# in a command substitution, which is a subshell, so an assignment inside it is
# discarded and the caller would print "n/a" forever with nothing to show that a
# value had been lost. That is trap 24's shape (a call whose failure mode is
# silence) and it is why both numbers travel on stdout.

echo "=== client process topology: 1 process x n conns  vs  n processes x 1 conn ==="
echo "  $AGG_MB MiB per rung, rungs '$RUNGS', $REPS reps, ${COOL}s between cells"
echo "  arms: relay (port $R) and --udp (port $Q), both carriers=1, same origin"
echo

up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register";  exit 1; }

declare -A SAMP   # SAMP[arm|n|topo] = space separated MB/s samples

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    for n in $RUNGS; do
        # Alternate which topology goes first, so a monotone drift in the line
        # cannot masquerade as a topology effect.
        if [ $((rep % 2)) -eq 1 ]; then topos="one many"; else topos="many one"; fi
        for topo in $topos; do
            for a in relay quic; do
                port=$R; [ "$a" = quic ] && port=$Q
                read -r v wall <<<"$(cell "$port" "$n" "$topo")"
                printf '    n=%-2s %-4s %-5s %8s MB/s   (wall clock: %s)\n' \
                    "$n" "$topo" "$a" "$v" "${wall:-n/a}"
                keep "$v" && SAMP["$a|$n|$topo"]+=" $v"
                sleep "$COOL"
            done
        done
    done
done

echo
echo "=== medians (MB/s) and the ratio that answers the question ==="
printf '  %-6s %-4s %-10s %-10s %-8s\n' arm n one many many/one
for a in relay quic; do
    for n in $RUNGS; do
        o=$(printf '%s\n' ${SAMP["$a|$n|one"]:-}  | med)
        m=$(printf '%s\n' ${SAMP["$a|$n|many"]:-} | med)
        r=$(LC_ALL=C awk -v o="$o" -v m="$m" 'BEGIN{ if (o+0>0 && m+0>0) printf "%.3f", m/o; else printf "n/a" }')
        printf '  %-6s %-4s %-10s %-10s %-8s\n' "$a" "$n" "$o" "$m" "$r"
    done
done

echo
echo "=== raw samples (a median with no samples beside it has not been read) ==="
for a in relay quic; do for n in $RUNGS; do for topo in one many; do
    printf '  %-6s n=%-2s %-5s%s\n' "$a" "$n" "$topo" "${SAMP["$a|$n|$topo"]:-  (none)}"
done; done; done

echo
echo "=== reading ==="
echo "  many/one near 1.00 at every rung: the client's process topology does not"
echo "  matter, raw_client.py is cleared, and the ladder's ceiling is downstream"
echo "  of it -- in the server, the VM's client, or the public path itself."
echo "  many/one well above 1.00 at n=8 and near 1.00 at n=4: the top rung of"
echo "  ws_conns was measuring the INSTRUMENT, and every conclusion drawn from"
echo "  it about bore at n=8 has to be withdrawn."
echo
echo "DONE"
