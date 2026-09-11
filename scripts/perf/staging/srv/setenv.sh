#!/usr/bin/env bash
# Set / unset one environment line in the staging compose and restart the
# server, then wait until it answers again.
#
# Every line this writes is preceded by a comment naming the campaign, so an
# operator reading the compose later knows why it is there and can delete it.
#   setenv.sh set  BORE_UDP_MEMORY_BUDGET 512MiB "reason"
#   setenv.sh unset BORE_UDP_MEMORY_BUDGET
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"


C=$VM_HOME/bore-staging-brp/compose.yml
OP=${1:?set|unset}; VAR=${2:?var}; VAL=${3:-}; WHY=${4:-perf campaign 2026-09-11}

$SSH "cp $C $C.bak.\$(date +%s) 2>/dev/null; python3 - <<'PY'
import re
p='$C'
s=open(p).read()
var='$VAR'; op='$OP'; val='''$VAL'''; why='''$WHY'''
# drop any previous campaign line for this var (commented marker + the setting)
s=re.sub(r'\n *# perf-campaign[^\n]*\n *- *'+re.escape(var)+r'=[^\n]*', '', s)
s=re.sub(r'\n *- *'+re.escape(var)+r'=[^\n]*', '', s)
if op=='set':
    anchor='      - BORE_UDP_MAX_STREAMS=8192'
    assert anchor in s, 'anchor missing'
    s=s.replace(anchor, anchor+'\n      # perf-campaign 2026-09-11: '+why+'\n      - '+var+'='+val, 1)
open(p,'w').write(s)
print('ok')
PY"
echo "  compose now:"; $SSH "grep -n -A1 'perf-campaign' $C; grep -n '$VAR' $C" 2>/dev/null | sed 's/^/    /'
$SSH "cd $VM_HOME/bore-staging-brp && sudo -n docker compose up -d --force-recreate bore-server >/dev/null 2>&1; echo restarted"
for i in $(seq 60); do
  if $SSH "sudo -n docker inspect -f '{{.State.Running}}' bore-server" 2>/dev/null | grep -q true; then
    sleep 2
    if curl -fsS -m 5 -o /dev/null "https://$GW/" 2>/dev/null || [ $i -gt 5 ]; then echo "  server up after ${i}s"; break; fi
  fi
  sleep 1
done
$SSH "sudo -n docker logs bore-server 2>&1 | tail -25" | sed 's/^/    LOG /'
