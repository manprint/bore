#!/usr/bin/env bash
# The whole server-parameter programme, in one unattended run.
#
# Ordered so the cheap, no-restart measurements happen first against the
# deployment as the operator left it, and every restart afterwards buys a
# measurement that cannot be taken any other way.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
B="$(cd "$(dirname "$0")" && pwd)"

VSSH="ssh -o BatchMode=yes -o StrictHostKeyChecking=no -i $K $VMU@$V"
O="$B/../out"; mkdir -p "$O"
st(){ echo; echo "##########  $* — $(date -Is)"; }

st "A. F-13 slow-reader ladder, budget UNSET (as deployed)"
$VSSH "~/vm_f13.sh baseline" 2>&1 | tee "$O/f13_baseline.txt"

st "B. proxy buffer at the deployed 128KiB"
$VSSH "~/vm_buf.sh buf128 3" 2>&1 | tee "$O/buf_128.txt"

st "C. switch to the built-in default 256KiB and restart"
"$B/setenv.sh" set BORE_PROXY_BUFFER_SIZE 256KiB "A/B against the deployed 128KiB" 2>&1 | tail -20
$VSSH "~/vm_buf.sh buf256 3" 2>&1 | tee "$O/buf_256.txt"

st "D. back to 128KiB, second pass (A/B/A ordering absorbs the drift)"
"$B/setenv.sh" set BORE_PROXY_BUFFER_SIZE 128KiB "A/B second pass" 2>&1 | tail -6
$VSSH "~/vm_buf.sh buf128b 3" 2>&1 | tee "$O/buf_128b.txt"

st "E. enable BORE_UDP_MEMORY_BUDGET=512MiB and restart"
"$B/setenv.sh" set BORE_UDP_MEMORY_BUDGET 512MiB "F-13 aggregate bound; measured 2026-09-11" 2>&1 | tail -25
"$B/../res/cfgkeys.sh" 2>&1 | sed 's/^/    /'

st "F. F-13 ladder with the budget on"
$VSSH "~/vm_f13.sh budget512" 2>&1 | tee "$O/f13_budget512.txt"

st "G. price the budget (relay as an in-run control)"
$VSSH "~/vm_budget_ab.sh budget512 4" 2>&1 | tee "$O/budget_ab_on.txt"
echo; echo "ALLDONE $(date -Is)"
