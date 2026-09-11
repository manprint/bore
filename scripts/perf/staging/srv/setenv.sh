#!/usr/bin/env bash
# Set / unset one environment line in the deployed server compose and restart
# the server, then wait until it answers again.
#
# Every line this writes is preceded by a comment naming the campaign, so an
# operator reading the compose later knows why it is there and can delete it.
#   setenv.sh set  BORE_UDP_MEMORY_BUDGET 512MiB "why"
#   setenv.sh unset BORE_UDP_MEMORY_BUDGET
#
# NOTE every remote call here uses `srv`, never `$SSH`: `$SSH` in lib.sh carries
# the ssh OPTIONS only, with no host. An earlier version of this file used
# `$SSH "cmd"`, which asks ssh to connect to a host literally named by the
# command string, and the compose path it used was `$VM_HOME/...` — the TEST VM's
# home — for a file that lives on the SERVER. Both are fixed: the path is
# `$BORE_SRV_COMPOSE` from env.sh.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

[ -n "$BORE_SRV" ] || { echo "BORE_SRV is not set in env.sh; cannot vary server parameters" >&2; exit 2; }
C="${BORE_SRV_COMPOSE:?BORE_SRV_COMPOSE must be set in env.sh}"
CONTAINER="$BORE_SRV_CONTAINER"
OP=${1:?set|unset}; VAR=${2:?var}; VAL=${3:-}; WHY=${4:-perf campaign}
STAMP=$(date +%F)

srv "cp $C $C.bak.\$(date +%s) 2>/dev/null; python3 - <<'PY'
import re, sys
p='$C'
s=open(p).read()
var='$VAR'; op='$OP'; val='''$VAL'''; why='''$WHY'''; stamp='$STAMP'
# drop any previous campaign line for this var (commented marker + the setting)
s=re.sub(r'\n *# perf-campaign[^\n]*\n *- *'+re.escape(var)+r'=[^\n]*', '', s)
s=re.sub(r'\n *- *'+re.escape(var)+r'=[^\n]*', '', s)
if op=='set':
    # Anchor on the LAST active environment entry of the service rather than one
    # named variable, so this works against any deployment's compose file.
    m=list(re.finditer(r'\n( +)- [A-Z][A-Z0-9_]*=[^\n]*', s))
    if not m:
        sys.exit('no environment entries found in '+p)
    last=m[-1]; indent=last.group(1)
    ins='\n'+indent+'# perf-campaign '+stamp+': '+why+'\n'+indent+'- '+var+'='+val
    s=s[:last.end()]+ins+s[last.end():]
open(p,'w').write(s)
print('ok')
PY"
echo "  compose now:"; srv "grep -n -A1 'perf-campaign' $C; grep -n '$VAR' $C" 2>/dev/null | sed 's/^/    /'
srv "cd \$(dirname $C) && sudo -n docker compose -f $C up -d --force-recreate $CONTAINER >/dev/null 2>&1; echo restarted"
for i in $(seq 60); do
  if srv "sudo -n docker inspect -f '{{.State.Running}}' $CONTAINER" 2>/dev/null | grep -q true; then
    sleep 2
    if curl -fsS -m 5 -o /dev/null "https://$GW/" 2>/dev/null || [ "$i" -gt 5 ]; then echo "  server up after ${i}s"; break; fi
  fi
  sleep 1
done
srv "sudo -n docker logs $CONTAINER 2>&1 | tail -25" | sed 's/^/    LOG /'
