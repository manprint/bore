#!/usr/bin/env bash
# Pull the published image the deployment tracks and recreate the server
# container, then PROVE which build is running.
#
# Why this exists as a script: every measurement in a campaign has to be able
# to name the build it was taken against, and "I redeployed" is not evidence.
# This prints the running binary's own version string before and after, and
# refuses to claim success unless the version actually changed (or `--same` is
# given, for a deliberate restart on the same build).
#
# WARNING: this RESTARTS the shared server. Any tunnel currently registered on
# it goes down and has to reconnect; clients without --auto-reconnect (or
# autossh) do not come back on their own. The tunnels registered right now are
# listed first so the operator can see what they are about to interrupt.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"

[ -n "$BORE_SRV" ] || { echo "BORE_SRV is not set in env.sh"; exit 2; }
COMPOSE="${BORE_SRV_COMPOSE:?BORE_SRV_COMPOSE must be set in env.sh}"
CONTAINER="${BORE_SRV_CONTAINER:-bore-server}"
DIR="$(dirname "$COMPOSE")"
ALLOW_SAME=0; [ "${1:-}" = "--same" ] && ALLOW_SAME=1

ver() { srv "sudo -n docker exec $CONTAINER /bore --version 2>/dev/null" 2>/dev/null | tr -d '\r'; }

say "what is registered right now (this restart will interrupt it)"
adm vhost    2>/dev/null | jq -r '.[]|"  vhost   \(.subdomain) from \(.client_addr // "?")"' 2>/dev/null
adm tunnels  2>/dev/null | jq -r '.[]|"  public  port \(.port) from \(.client_addr // "?")"' 2>/dev/null
adm secret   2>/dev/null | jq -r '.[]|"  secret  \(.secret_id // .id // "?")"' 2>/dev/null

BEFORE="$(ver)"
say "before: ${BEFORE:-unknown}"

say "pulling and recreating"
srv "cd '$DIR' && sudo -n docker compose -f '$COMPOSE' pull -q && \
     sudo -n docker compose -f '$COMPOSE' up -d --force-recreate" 2>&1 | sed 's/^/  /'

for _ in $(seq 60); do
    AFTER="$(ver)"
    [ -n "$AFTER" ] && break
    sleep 2
done
say "after:  ${AFTER:-unknown}"

if [ -z "${AFTER:-}" ]; then
    echo "FAIL: the container is not answering --version"; exit 1
fi
if [ "$AFTER" = "$BEFORE" ] && [ "$ALLOW_SAME" = 0 ]; then
    echo "FAIL: the version did not change — the registry still serves the old image."
    echo "      Wait for the image build to publish, then re-run. Use --same for a"
    echo "      deliberate restart on the same build."
    exit 1
fi

say "post-restart health"
adm config >/dev/null 2>&1 && echo "  admin API answering" || echo "  WARNING: admin API not answering"
srv "sudo -n docker logs --since 60s $CONTAINER 2>&1 | grep -iE 'error|panic|warn' | head -5" 2>/dev/null | sed 's/^/  /'
echo "OK: now running ${AFTER}"
