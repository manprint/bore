#!/usr/bin/env bash
# Start the 2 s CPU/RAM sampler on all three actors: the server host, the test
# VM and this workstation.
#
# Two defects this file used to carry, both of which silently produced empty or
# missing data rather than an error:
#
#  * it read $SRV/$V/$SSH/$VM_HOME without sourcing lib.sh, so it only worked
#    when the caller happened to have sourced it first;
#  * it wrote the remote samples to `~/wsres.*` while stop_samplers.sh pulled
#    `~/pres.*`. The two never met, and the campaign collected nothing from the
#    remote hosts for the phases that used this script. Both ends now agree on
#    ONE prefix, defined here and exported as SAMPLER_PREFIX.
#
# The kill and the start MUST stay separate ssh invocations. In one command
# string the pkill pattern matches the string's own later occurrence of the
# sampler name, so the remote shell kills itself before reaching setsid — that
# produced zero samples twice before it was understood.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

N="${1:-2400}"
IV="${2:-2}"
REMOTE_PREFIX="${SAMPLER_PREFIX:-pres}"
LOCAL_PREFIX="$OUT/pres_ws"

for h in srv vm; do
    "$h" "pkill -f 'res_sample[r].sh' >/dev/null 2>&1; true" >/dev/null 2>&1
done
pkill -f 'res_sample[r].sh' >/dev/null 2>&1
sleep 1

for h in srv vm; do
    "$h" "setsid nohup ~/res_sampler.sh $N $IV \$HOME/$REMOTE_PREFIX >/dev/null 2>&1 </dev/null & true" >/dev/null 2>&1
done
setsid nohup "$(cd "$(dirname "$0")" && pwd)/res_sampler.sh" "$N" "$IV" "$LOCAL_PREFIX" >/dev/null 2>&1 </dev/null &

sleep 6
printf '  server:      %s samples\n' "$(srv "wc -l < \$HOME/$REMOTE_PREFIX.stat" 2>/dev/null | tr -d '\r')"
printf '  vm:          %s samples\n' "$(vm  "wc -l < \$HOME/$REMOTE_PREFIX.stat" 2>/dev/null | tr -d '\r')"
printf '  workstation: %s samples\n' "$(wc -l < "$LOCAL_PREFIX.stat" 2>/dev/null)"
echo "  (a sampler reporting 0 or empty here never started; do not run the"
echo "   campaign and discover it afterwards)"
exit 0
