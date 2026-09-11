#!/usr/bin/env bash
# Stop the resource samplers on all three hosts and pull the two remote files
# back next to the local one.
#
# The kill pattern is bracketed AND the whole body lives in this file rather
# than in a command line, because `pkill -f res_sampler.sh` matches ANY command
# line mentioning the sampler -- including the shell invocation that is trying
# to do the killing. That self-kill cost three attempts during the campaign,
# and bracketing alone does not fix it when the invoking command line also
# spells the name out.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

for h in srv vm; do
    "$h" "pkill -f 'res_sample[r].sh' >/dev/null 2>&1; true" >/dev/null 2>&1
done
pkill -f 'res_sample[r].sh' >/dev/null 2>&1
sleep 1

for x in stat proc; do
    [ -n "$BORE_SRV" ] && vmcp "$BORE_SRV_USER@$BORE_SRV:~/pres.$x" "$OUT/pres_srv.$x" 2>/dev/null
    vmcp "$BORE_VM_USER@$BORE_VM:~/pres.$x" "$OUT/pres_vm.$x" 2>/dev/null
done
wc -l "$OUT"/pres_*.stat 2>/dev/null
echo "reduce with:  NCPU=<cores of that host> res/res_reduce.py $OUT/pres_srv 'label' [t0 t1]"
