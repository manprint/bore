#!/usr/bin/env bash
# X1: how much bandwidth does `bore transfer` get on a real link, and which
# knob decides it.
#
# One large file per run, so the file-count cost measured by X2 is out of the
# picture and what is left is the transport. Two variables and nothing else:
#   * the ARM  — `--relay-only` (sender -> server -> listener, two WAN legs)
#                vs the direct QUIC path (one leg, server off the path).
#   * `--parallel` — N streams. This is the whole point of the sweep: on the
#     direct arm a single stream is bounded by `window / RTT` (the secret
#     campaign's window-floor finding), so the parallel=1 column measures the
#     receive window and the saturating column measures the link.
#
# The path is read back from the SENDER's own log, never assumed: a `--udp`
# run that failed to punch is a relay measurement wearing a direct label.
#
# OPEN QUESTION RAISED BY THE WIRED RUN (2026-09-13), AND ONE HYPOTHESIS ALREADY
# FALSIFIED. The direct arm at `--parallel 1` read 45.8 MiB/s where every other
# cell in the table read 86-88 (the line rate for this direction), and doubling
# to `--parallel 2` recovered it exactly (87.8). A per-stream bound that halves
# with one stream and vanishes with two is the shape of `something / RTT`, and
# `CHUNK_SIZE / RTT` is 1 MiB / 19 ms = 52.6 MiB/s, which is the right size.
# **That hypothesis is dead on arrival**: the RELAY arm runs the SAME chunk
# protocol at the SAME RTT and read 87.5 MiB/s at `--parallel 1`, so nothing in
# the chunk loop can be the bound. Nor is it flow control — `holepunch::
# transport_config` is the ONE place a `quinn::TransportConfig` is built in this
# crate (checked), and it installs a 16 MiB stream window, which at 19 ms is
# 842 MB/s. What is left is specific to ONE QUIC stream on the direct path:
# congestion control, pacing, or receive-side CPU on the 2-vCPU listener. It is
# a SINGLE sample and is not a finding yet. Settling it needs the loss and RTT
# beside the rate (V-15's rule, which this stage does not yet obey) and a second
# repetition; `REPS=2 PARS="1 2" ARMS=direct` is the cheap form.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/xferlib.sh"

TOPO="${TOPO:-ws-vm}"          # ws-vm = sender on ws, listener on vm
MB="${MB:-1024}"
PARS="${PARS:-1 2 4 8 16}"
ARMS="${ARMS:-relay direct}"
REPS="${REPS:-1}"

case "$TOPO" in ws-vm|vm-ws|vm-vm) ;; *) echo "bad TOPO '$TOPO'" >&2; exit 2 ;; esac

# Where the bytes start and where they land, per topology.
case "$TOPO" in
    ws-vm) SRC_HOST=ws; LIS_HOST=vm ;;
    vm-ws) SRC_HOST=vm; LIS_HOST=ws ;;
    vm-vm) SRC_HOST=vm; LIS_HOST=vm ;;
esac

say "transfer bandwidth: ${MB} MiB single file, topology $TOPO, arms '$ARMS'"
if [ "$SRC_HOST" = ws ]; then SRC="$(ws_fixture "$MB" 1)"; else SRC="$(vm_fixture "$MB" 1)"; fi
[ -n "$SRC" ] || { echo "fixture failed" >&2; exit 1; }

printf '  %-7s %-4s %-4s %9s %11s %10s %11s  %s\n' arm par rep wall_s MB/s xfer_s MiB/s path
for arm in $ARMS; do
    case "$arm" in
        relay)  FLAGS="--relay-only" ;;
        direct) FLAGS="" ;;
        *) echo "bad arm '$arm'" >&2; exit 2 ;;
    esac
    for par in $PARS; do
        for rep in $(seq "$REPS"); do
            id="$(xfer_id)"
            LASTPID=""
            if [ "$LIS_HOST" = ws ]; then
                xfer_listener_ws "$id" "$WS_WORK/dst-$id" $FLAGS
                LIS_PID="$LASTPID"
            else
                xfer_listener_vm "$id" "$VM_WORK/dst-$id" $FLAGS
                LIS_PID=""
            fi
            # The listener must be registered before the sender dials, or the
            # sender fails the rendezvous rather than the transfer.
            sleep 4
            if [ "$SRC_HOST" = ws ]; then
                xfer_sender_ws "$id" "$SRC" --parallel "$par" $FLAGS
            else
                xfer_sender_vm "$id" "$SRC" --parallel "$par" $FLAGS
            fi
            if [ "$XFER_RC" -eq 0 ]; then
                # shellcheck disable=SC2046
                printf '  %-7s %-4s %-4s %9s %11s %10s %11s  %s\n' "$arm" "$par" "$rep" \
                    "$XFER_WALL" "$(xfer_mbs "$MB" "$XFER_WALL")" \
                    $(xfer_reported "$id" "$SRC_HOST") "$(xfer_path "$id" "$SRC_HOST")"
            else
                printf '  %-7s %-4s %-4s %9s %11s %10s %11s  %s\n' "$arm" "$par" "$rep" FAIL - - - "rc=$XFER_RC"
            fi
            xfer_down "$id" ${LIS_PID:-}
            if [ "$LIS_HOST" = ws ]; then rm -rf "$WS_WORK/dst-$id"; else vm "rm -rf '$VM_WORK/dst-$id'"; fi
            cool "${GAP:-20}"
        done
    done
done
echo
echo DONE
