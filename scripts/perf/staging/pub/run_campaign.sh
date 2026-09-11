#!/usr/bin/env bash
# One command that runs the whole public-tunnel campaign and collects every
# artefact needed to write it up afterwards.
#
# What it does, in order:
#   1. proves which server build the run is measuring (a campaign whose numbers
#      cannot name their build is not evidence);
#   2. starts the 2 s CPU/RAM sampler on the server, the VM and this
#      workstation, and the allowance timeline against the server;
#   3. runs the VM-side stages strictly serially;
#   4. stops the samplers and pulls everything into out/pub-<timestamp>/.
#
# It does NOT run the workstation-side topology (ws_pub.sh): that one competes
# with the VM stages for the same server and the same allowance budget, so it
# is run separately, afterwards.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/../lib.sh"

STAGES="${STAGES:-p1 p2 p3 p4 flavours conc netem eff stab}"
TS="$(date +%Y%m%d-%H%M%S)"
DEST="$OUT/pub-$TS"
mkdir -p "$DEST"

say "server build under test"
{
  adm config | jq -r '"version_field=\(.server_version // "not reported")"'
  srv "sudo -n docker exec ${BORE_SRV_CONTAINER} /bore --version 2>/dev/null" 2>/dev/null
  srv "sudo -n docker inspect --format 'image={{.Config.Image}} started={{.State.StartedAt}}' ${BORE_SRV_CONTAINER}" 2>/dev/null
  echo "vm_binary=$(vm '~/bore --version' 2>/dev/null | tr -d '\r')"
  echo "client_image=$(vm 'sudo -n docker image inspect ghcr.io/manprint/bore:client --format "{{index .Config.Labels \"org.opencontainers.image.revision\"}}"' 2>/dev/null | tr -d '\r')"
  echo "stages=$STAGES"
  echo "started=$(date -Is)"
} | tee "$DEST/BUILD.txt"

say "resource samplers + allowance timeline"
"$HERE/../res/start_samplers.sh" 20000 2>&1 | sed 's/^/  /'
"$HERE/../res/ena_timeline.sh" 20000 10 > "$DEST/ena.timeline" 2>/dev/null &
ENA_PID=$!
trap 'kill "$ENA_PID" 2>/dev/null' EXIT

say "VM stages: $STAGES"
# Run detached on the VM so an ssh hiccup on this side cannot abort a
# multi-hour campaign halfway through.
vm "rm -f ~/res/pub_driver.log; setsid nohup ~/pub/pub_driver.sh $STAGES >/dev/null 2>&1 </dev/null & true" >/dev/null 2>&1
while true; do
    sleep 60
    line="$(vm 'tail -1 ~/res/pub_driver.log 2>/dev/null' 2>/dev/null | tr -d '\r')"
    echo "  $(date -Is) $line"
    case "$line" in *"ALL PUBLIC STAGES DONE"*) break;; esac
done

say "stopping samplers and collecting"
kill "$ENA_PID" 2>/dev/null
"$HERE/../res/stop_samplers.sh" 2>&1 | sed 's/^/  /'
vmcp -r "$BORE_VM_USER@$BORE_VM:~/res/pub_*.log" "$DEST/" 2>/dev/null
vmcp "$BORE_VM_USER@$BORE_VM:~/res/pub_driver.log" "$DEST/" 2>/dev/null
for x in stat proc; do
    cp "$OUT/pres_srv.$x" "$DEST/" 2>/dev/null
    cp "$OUT/pres_vm.$x"  "$DEST/" 2>/dev/null
    cp "$OUT/pres_ws.$x" "$DEST/" 2>/dev/null
done
echo "finished=$(date -Is)" >> "$DEST/BUILD.txt"
say "artefacts in $DEST"
ls -la "$DEST"
