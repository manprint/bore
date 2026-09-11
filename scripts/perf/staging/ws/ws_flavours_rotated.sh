#!/usr/bin/env bash
# Three rounds, rotated order, each round gated on a recovered budget.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
B="$(cd "$(dirname "$0")" && pwd)"
echo "########## rotated flavour comparison  $(date -Is)"
for w in $(seq 12); do
  p=$("$B/../res/bw_probe.sh" 2>&1 | tail -1); echo "  gate: $p"
  case "$p" in *AVAILABLE*) break;; esac
  sleep 60
done
ROUNDS=3 NSTREAM=4 BURST=10 COOL=75 "$B/ws_flavours.sh"
