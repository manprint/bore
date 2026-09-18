#!/usr/bin/env bash
# T-WEB-PERF-WAN (plan 001, V003-C1) — web transfer measured across two REAL
# hosts over a WAN, both legs scripted.
#
# Topology, and it is chosen rather than convenient:
#   - the SERVER runs on the REMOTE host, because that is the side with a
#     public address; a server behind the local NAT could not be reached;
#   - the SOURCE is the LOCAL browser and the RECIPIENT is a browser on the
#     remote host. Bytes therefore travel local -> remote, which on a cloud
#     host is INGRESS and is not billed. The reverse direction is measured
#     once, small, on purpose;
#   - the relay arm's second leg is loopback on the remote. Both arms carry
#     the same WAN leg, which is what makes the pair comparable; the relay
#     arm is flattered by exactly one loopback hop and the report says so.
#
# Rules, each paid for elsewhere in this repository: raw samples always
# printed (V-11), LC_ALL=C, a failed arm is FAILED and never a number (V-9),
# arms alternate INSIDE a repetition (V-13), and the payload is sized against
# the measured line rather than chosen round (V-19).
set -euo pipefail
export LC_ALL=C

REMOTE="${REMOTE:-awstest}"
SSH_OPTS="${SSH_OPTS:--o LogLevel=ERROR}"
REMOTE_ADDR="${REMOTE_ADDR:-}"
PORT="${PORT:-7835}"
SIZE_MB="${SIZE_MB:-256}"
REPS="${REPS:-3}"
ARMS="${ARMS:-direct,relay}"
# Carriers are a SERVER decision (phase 7.3), so sweeping them means restarting
# the server — which is why SWEEP re-execs this script once per value instead
# of alternating carrier counts inside a repetition. Arms still alternate
# inside every repetition, at a fixed carrier count, which is the comparison
# that has to be drift-free (V-13).
CARRIERS="${CARRIERS:-}"
SWEEP="${SWEEP:-}"
REMOTE_DIR="${REMOTE_DIR:-/home/ubuntu/wt}"
OUT="${OUT:-$(mktemp -d)}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

if [[ -n "$SWEEP" ]]; then
  # One report over every carrier count, and one server lifetime per value.
  sweep_out="$OUT"
  mkdir -p "$sweep_out"
  : > "${sweep_out}/samples.ndjson"
  IFS=',' read -r -a sweep_values <<< "$SWEEP"
  for n in "${sweep_values[@]}"; do
    echo "############ carriers=${n} ############"
    SWEEP="" CARRIERS="$n" OUT="${sweep_out}/c${n}" "${BASH_SOURCE[0]}" ||       echo "FAIL: the carriers=${n} run did not complete" >&2
    cat "${sweep_out}/c${n}/samples.ndjson" >> "${sweep_out}/samples.ndjson" 2>/dev/null || true
  done
  echo
  echo "== sweep summary =="
  python3 scripts/perf/webwan/report.py "${sweep_out}/samples.ndjson"
  echo
  echo "raw: ${sweep_out}/samples.ndjson"
  exit 0
fi


if [[ -z "$REMOTE_ADDR" ]]; then
  REMOTE_ADDR="$(ssh -n "$REMOTE" 'curl -s --max-time 5 ifconfig.me' 2>/dev/null || true)"
fi
[[ -n "$REMOTE_ADDR" ]] || { echo "FAIL: set REMOTE_ADDR=<public ip of $REMOTE>" >&2; exit 2; }

base="https://${REMOTE_ADDR}:${PORT}/"
# Empty means "whatever the binary ships", which is itself one of the arms
# worth measuring: the default is the number most users will run.
CARRIER_FLAG=""
[[ -n "$CARRIERS" ]] && CARRIER_FLAG="--web-transfer-direct-carriers ${CARRIERS}"
mkdir -p "$OUT"

# Stopped by PID FILE, never by `pkill -f`: the pattern that matches the
# server also matches the ssh command line that starts it, so a pkill run
# through ssh kills its own session and the driver hangs before the first
# measurement. It did, twice.
remote_stop() {
  ssh -n "$REMOTE" "cd ${REMOTE_DIR} 2>/dev/null || exit 0;     for f in server.pid owner.pid; do [ -f \$f ] && kill \$(cat \$f) 2>/dev/null; rm -f \$f; done; exit 0" >/dev/null 2>&1 || true
}
cleanup() { remote_stop; }
trap cleanup EXIT

echo "== bore web transfer, two hosts, WAN =="
echo "local  : $(uname -sr), $(nproc) threads"
echo "remote : ${REMOTE} (${REMOTE_ADDR}), server + recipient"
echo "server : ${base}  (relay unthrottled)"
echo "size   : ${SIZE_MB} MiB   reps=${REPS}   arms=${ARMS}   carriers=${CARRIERS:-default}"
echo "date   : $(date -Is)"
echo

# --- remote: certificate, server, room --------------------------------------
remote_stop
# `(setsid cmd … &)` — a SUBSHELL around the launch, and it is not cosmetic.
# `cmd > log 2>&1 < /dev/null &` over ssh still holds the session open until
# the child exits: measured here with a plain `setsid sleep 40`, which kept
# ssh for the full timeout, while the same command inside `( … &)` returned
# in 1.0 s. The driver hung at this exact line for three full timeouts before
# the control experiment said the shape was at fault and `bore` was not.
ssh -n "$REMOTE" "cd ${REMOTE_DIR} && \
  ([ -f cert.pem ] || openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 2 \
     -subj '/CN=${REMOTE_ADDR}' -addext 'subjectAltName=IP:${REMOTE_ADDR}' >/dev/null 2>&1) && \
  (setsid ./bore server --control-port ${PORT} --cert-file cert.pem --key-file key.pem \
     --web-transfer-base-url '${base}' --web-transfer-relay-rate 0 ${CARRIER_FLAG} > server.log 2>&1 < /dev/null & \
     echo \$! > server.pid) ; sleep 2; echo remote-server-started" < /dev/null

for _ in $(seq 1 50); do
  curl -fsSk -o /dev/null --max-time 3 "${base}transfer/assets/app.js" && break
  sleep 0.3
done
curl -fsSk -o /dev/null --max-time 5 "${base}transfer/assets/app.js" || {
  echo "FAIL: server not answering on ${base}" >&2
  ssh -n "$REMOTE" "tail -20 ${REMOTE_DIR}/server.log" >&2 || true
  exit 2
}

ssh -n "$REMOTE" "cd ${REMOTE_DIR} && rm -f owner.log && \
  (setsid ./bore transfer web --to '${base}' --insecure ${ROOM_FLAGS:-} > owner.log 2>&1 < /dev/null & \
     echo \$! > owner.pid) ; sleep 2; echo room-started" < /dev/null
room=""
for _ in $(seq 1 50); do
  room="$(ssh -n "$REMOTE" "sed -n 's/^room: //p' ${REMOTE_DIR}/owner.log | head -1" 2>/dev/null || true)"
  [[ -n "$room" ]] && break
  sleep 0.4
done
[[ -n "$room" ]] || { echo "FAIL: no room URL" >&2; ssh -n "$REMOTE" "cat ${REMOTE_DIR}/owner.log" >&2; exit 2; }
echo "room   : ${room:0:48}…"
echo

scp -q web/transfer/tests/perf/wan-recipient.mjs "${REMOTE}:${REMOTE_DIR}/recipient.mjs"

# --- one measurement: one repetition of one arm -----------------------------
measure() {
  local arm="$1" rep="$2"
  local rlog="${OUT}/recipient-${arm}-${rep}.log"
  local slog="${OUT}/source-${arm}-${rep}.log"
  ssh -n "$REMOTE" "cd ${REMOTE_DIR} && node recipient.mjs '${room}' 1 '${arm}' /dev/shm" > "$rlog" 2>&1 &
  local rpid=$!
  for _ in $(seq 1 200); do
    grep -q WTREADY "$rlog" && break
    kill -0 $rpid 2>/dev/null || break
    sleep 0.3
  done
  grep -q WTREADY "$rlog" || { echo "FAIL: recipient never joined (${arm} rep ${rep})"; tail -5 "$rlog"; wait $rpid 2>/dev/null || true; return 1; }
  # Both runners live under `web/transfer/tests/perf/` because an ESM bare
  # specifier resolves from the IMPORTING FILE upwards, not from the CWD:
  # under `scripts/` node never finds `@playwright/test`, whatever the cwd.
  node web/transfer/tests/perf/wan-source.mjs "$room" "$SIZE_MB" 1 "$arm" "${PAYLOAD_DIR:-/tmp}" > "$slog" 2>&1 || true
  # A dead source must not leave the recipient waiting out its own 30-minute
  # deadline: one failed leg would otherwise cost the whole run.
  if ! grep -q '^WTJSON ' "$slog"; then
    kill $rpid 2>/dev/null || true
    ssh -n "$REMOTE" "pkill -f '[r]ecipient.mjs'" >/dev/null 2>&1 || true
  fi
  wait $rpid 2>/dev/null || true
  local cj="\"carriers\":${CARRIERS:-null},"
  grep -h '^WTJSON ' "$slog" | sed "s/^WTJSON /{\"side\":\"src\",\"arm\":\"${arm}\",${cj}\"rep\":${rep},\"d\":/;s/$/}/" >> "${OUT}/samples.ndjson"
  grep -h '^WTJSON ' "$rlog" | sed "s/^WTJSON /{\"side\":\"dst\",\"arm\":\"${arm}\",${cj}\"rep\":${rep},\"d\":/;s/$/}/" >> "${OUT}/samples.ndjson"
  grep -q '^WTJSON ' "$slog" || echo "  (${arm} rep ${rep}): source produced no sample — see $slog"
}

: > "${OUT}/samples.ndjson"
IFS=',' read -r -a arms <<< "$ARMS"
for rep in $(seq 0 $((REPS - 1))); do
  echo "-- repetition $((rep + 1))/${REPS} --"
  order=("${arms[@]}")
  if (( rep % 2 == 1 )); then
    order=()
    for (( i=${#arms[@]}-1 ; i>=0 ; i-- )); do order+=("${arms[i]}"); done
  fi
  for arm in "${order[@]}"; do
    printf "   %-7s " "$arm"
    if measure "$arm" "$rep"; then
      tail -2 "${OUT}/samples.ndjson" | python3 scripts/perf/webwan/tick.py
    fi
  done
done

echo
echo "== summary =="
python3 scripts/perf/webwan/report.py "${OUT}/samples.ndjson"
echo
echo "raw: ${OUT}/samples.ndjson"
