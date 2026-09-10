#!/usr/bin/env bash
# Sample the bore server host from outside: CPU busy split, and STEAL time,
# which is the direct evidence of burstable-credit throttling on a t-family
# instance (CPUCreditBalance itself is CloudWatch-only, not readable in-guest).
# One persistent ssh session runs the loop, so per-sample cost is a newline.
# Requires, from the environment (see docs/performance/ §9 runbook):
#   BORE_SERVER_IP   address of the bore server host
#   BORE_SERVER_USER ssh user on it (default: ubuntu)
#   BORE_SERVER_KEY  ssh private key for it
: "${BORE_SERVER_IP:?set BORE_SERVER_IP}"
: "${BORE_SERVER_KEY:?set BORE_SERVER_KEY}"
SRVUSER="${BORE_SERVER_USER:-ubuntu}"
N="${1:-150}"; IV="${2:-2}"
timeout $((N * IV + 60)) ssh -o BatchMode=yes -o ServerAliveInterval=30 \
  -i "$BORE_SERVER_KEY" "$SRVUSER@$BORE_SERVER_IP" "
for i in \$(seq $N); do
  read -r _ u ni sy id io irq sirq st rest < /proc/stat
  printf '%s %s %s %s %s %s %s %s %s %s\n' \"\$(date +%s)\" \"\$u\" \"\$ni\" \"\$sy\" \"\$id\" \"\$io\" \"\$irq\" \"\$sirq\" \"\$st\" \"\$(cut -d' ' -f1 /proc/loadavg)\"
  sleep $IV
done"
