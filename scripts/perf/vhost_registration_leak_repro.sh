#!/usr/bin/env bash
# Round 3. R1-R11 all released the entry in 0-1 s, but every one of them let the
# kernel deliver a FIN or RST. The secret-tunnel reaper in CLAUDE.md exists for a
# different shape: a peer that is gone at the application layer while its TCP
# connection stays perfectly alive, so send() buffers and recv() blocks forever.
# vhost has no equivalent reaper. These scenarios produce exactly that shape.
#
# Transfers are rate limited so a transfer stays in flight for the whole window
# without spending bandwidth budget.
set -uo pipefail
# Paths and credentials come from the environment, never from this file:
#   BORE_PERF_ENV  path to an env file exporting BORE_HOST, BORE_TO, BORE_SECRET,
#                  ADMIN_URL, ADMIN_TOKEN (chmod 600, kept outside the repo)
#   BORE_PERF_OUT  writable directory for logs and results (default: mktemp -d)
# See docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md section 9.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
B="${BORE_PERF_OUT:-$(mktemp -d -t bore-perf-XXXXXX)}"; mkdir -p "$B/logs"
. "${BORE_PERF_ENV:?set BORE_PERF_ENV to an env file exporting BORE_HOST/BORE_TO/BORE_SECRET/ADMIN_URL/ADMIN_TOKEN}"
BORE=$ROOT/target/release/bore
NETEM=$ROOT/scripts/vhost_staging_netem.sh
SRV=${BORE_SERVER_IP}
OUT=$B/zombie3.txt; : > $OUT
ZPID=""; CP=""
cleanup(){ [ -n "$ZPID" ] && { kill -CONT $ZPID 2>/dev/null; kill -9 $ZPID 2>/dev/null; }
           [ -n "$CP" ] && kill -9 $CP 2>/dev/null
           sudo -n $NETEM clear >/dev/null 2>&1; }
trap 'cleanup; exit 130' INT TERM; trap cleanup EXIT
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
log(){ echo "$*" | tee -a $OUT; }
entry(){ adm vhost | jq -c --arg l "$1" '.[]|select(.subdomain==$l)|{active,carriers,relay_tx_bytes}'; }
present(){ adm vhost | jq -e --arg l "$1" 'any(.[];.subdomain==$l)' >/dev/null 2>&1; }

start(){ local l=$1; shift
  $BORE vhost 127.0.0.1:5052 --subdomain "$l" --id "$l" --to "$BORE_TO" --secret "$BORE_SECRET" "$@" > $B/logs/$l.log 2>&1 &
  ZPID=$!
  for i in $(seq 40); do present "$l" && return 0; sleep 0.5; done
  return 1
}
# a slow transfer that is still running when we interfere
slow_get(){ curl -s -o /dev/null --limit-rate 2M -m 300 "https://$1.${BORE_HOST}/stream/2147483648" & CP=$!; }

watch_gone(){ local l=$1 n=$2 t0; t0=$(date +%s)
  for i in $(seq $n); do
    present "$l" || { log "   entry released after $(( $(date +%s)-t0 ))s"; return 0; }
    sleep 5
  done
  log "   *** LEAK: still registered after $(( $(date +%s)-t0 ))s: $(entry "$l")"
  return 1
}

curl -fsS -m 3 -o /dev/null http://127.0.0.1:5052/ping || { log "origin down"; exit 1; }

# R14: provider frozen mid-transfer. TCP stays alive and the kernel keeps
# ACKing, so the server sees a healthy connection with a dead application.
for CAR in 1 2; do
  L=z3f$(date +%s)c$CAR
  log ""; log "== R14 provider SIGSTOP mid-transfer, carriers=$CAR (label=$L) =="
  if start "$L" --carriers $CAR; then
    slow_get "$L"; sleep 5
    log "   before freeze: $(entry "$L")"
    kill -STOP $ZPID; log "   provider frozen (TCP alive, application dead)"
    for t in 15 30 60 120; do sleep $((t - ${prev:-0})); prev=$t
      log "   t+${t}s: $(entry "$L")  present=$(present "$L" && echo yes || echo no)"
    done
    unset prev
    log "   re-registration attempt while frozen:"
    timeout 20 $BORE vhost 127.0.0.1:5052 --subdomain "$L" --id "${L}b" --to "$BORE_TO" --secret "$BORE_SECRET" 2>&1 | tail -2 | sed 's/^/     /' | tee -a $OUT
    kill -CONT $ZPID 2>/dev/null; kill -9 $ZPID 2>/dev/null; ZPID=""
    kill -9 $CP 2>/dev/null; CP=""
    watch_gone "$L" 24
  else log "   REGISTRATION FAILED"; fi
done

# R15: half-open control connection. Blackhole only the provider's own source
# ports so the admin API stays reachable, then kill the provider: no FIN, no RST.
L=z3ho$(date +%s)
log ""; log "== R15 half-open control conn (per-source-port blackhole) (label=$L) =="
if start "$L" --carriers 2; then
  slow_get "$L"; sleep 5
  PORTS=$(ss -tnp 2>/dev/null | grep "$SRV:443" | grep -oE '192\.168\.1\.11:[0-9]+' | cut -d: -f2 | sort -u)
  log "   provider+client source ports: $(echo $PORTS | tr '\n' ' ')"
  IF=$(ip route get $SRV | awk '{print $5; exit}')
  sudo -n tc qdisc del dev $IF root 2>/dev/null
  sudo -n tc qdisc add dev $IF root handle 1: prio bands 3
  sudo -n tc qdisc add dev $IF parent 1:3 handle 30: netem loss 100%
  n=0
  for p in $PORTS; do
    sudo -n tc filter add dev $IF protocol ip parent 1:0 prio 1 u32 \
      match ip dst $SRV/32 match ip protocol 6 0xff match ip sport $p 0xffff flowid 1:3 && n=$((n+1))
  done
  log "   blackholed $n source ports toward the server (admin API unaffected)"
  kill -9 $ZPID 2>/dev/null; ZPID=""
  kill -9 $CP 2>/dev/null; CP=""
  log "   provider SIGKILLed with its egress blackholed"
  for t in 15 30 60 120 180; do sleep $((t - ${prev:-0})); prev=$t
    log "   t+${t}s: present=$(present "$L" && echo yes || echo no)  $(entry "$L")"
  done
  unset prev
  sudo -n $NETEM clear >/dev/null 2>&1
  log "   blackhole cleared"
  watch_gone "$L" 24
else log "   REGISTRATION FAILED"; fi

log ""; log "== registry =="; log "$(adm vhost | jq -c '[.[]|{subdomain,active,uptime_secs}]')"
log "DONE"
