#!/usr/bin/env bash
# Start the 2 s sampler on all three hosts for the workstation/dufs and
# server-parameter phases, which the driver-stage sampling of §7.1 never
# covered. Bracketed pkill pattern so the ssh command string carrying it is
# not itself matched (that self-kill silently produced empty files once).
set -u
B="$(cd "$(dirname "$0")" && pwd)"
S=$SRV; V=$V

N="${1:-2400}"
# The kill and the start MUST be separate ssh invocations. In one command
# string the pattern matches the string's own later `~/res_sampler.sh`
# occurrence, so the remote shell pkills itself before ever reaching setsid --
# which silently produced zero samples twice.
for h in $S $V; do
  $SSH ubuntu@$h "pkill -f 'res_sample[r].sh'" >/dev/null 2>&1 || true
done
for h in $S $V; do
  $SSH ubuntu@$h "setsid nohup ~/res_sampler.sh $N 2 $VM_HOME/wsres >/dev/null 2>&1 </dev/null & true" >/dev/null 2>&1
done
pkill -f 'res_sample[r].sh' >/dev/null 2>&1
setsid nohup "$B/res_sampler.sh" "$N" 2 "$B/../out/wsres_local" >/dev/null 2>&1 </dev/null &
sleep 6
for h in $S $V; do printf '  %s: %s samples\n' "$h" "$($SSH ubuntu@$h 'wc -l < $VM_HOME/wsres.stat' 2>/dev/null)"; done
printf '  workstation: %s samples\n' "$(wc -l < "$B/../out/wsres_local.stat" 2>/dev/null)"
exit 0
