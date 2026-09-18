#!/usr/bin/env bash
# T-WEB-PERF-LAN (plan 001, V003-C1) — two-host web-transfer measurement.
#
# WHY THIS EXISTS. Every other web-transfer benchmark runs both peers in one
# process on one machine. That is the one link the direct path does not exist
# for: a loopback pair reaches itself on a host candidate, the "relay" arm is
# a localhost TCP hop through a server on the same CPU, and V-9's rule says
# absolute numbers belong to the machine that produced them. V003-F01 asks a
# question loopback cannot answer — does the direct path carry a 400 MB file
# between two REAL hosts faster than the relay, and does it meet 50 MB/s.
#
# WHAT IT DRIVES. The SOURCE only. The recipient is a browser somebody opens
# on the other device, which is the whole point: an Android phone cannot be
# driven by Playwright, and a harness that insisted would measure the two
# machines that happen to have node on them. The source's own view is
# sufficient because the recipient acknowledges only VERIFIED ranges — a
# transfer this side sees complete is a transfer the other side hashed.
#
# Rules this script obeys, each paid for elsewhere in this repository:
#   - every raw sample is printed beside the median (V-11: a file that prints
#     only medians hides the bug that corrupts them);
#   - LC_ALL=C, so a comma-decimal locale cannot reorder a numeric sort;
#   - an arm that produced nothing prints FAILED and the script exits nonzero
#     (V-9: a failed arm must never enter a table as a number);
#   - the arms run in ONE session, alternating, because the line under a
#     benchmark moves and a split run would attribute that move to the change
#     (V-13);
#   - 400 MiB by default, because a transfer shorter than the ramp measures
#     the ramp (V-19).
set -euo pipefail
export LC_ALL=C

SIZE_MB="${SIZE_MB:-400}"
REPS="${REPS:-3}"
ENGINE="${ENGINE:-chromium}"
ARMS="${ARMS:-direct,relay}"
PORT="${PORT:-7840}"
WAIT_MS="${WAIT_MS:-600000}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

bin="${BORE_BIN:-target/debug/bore}"
if [[ ! -x "$bin" ]]; then
  echo "FAIL: $bin missing — run: npm --prefix web/transfer run build && cargo build --all-features" >&2
  exit 2
fi

# The LAN address the OTHER host must be able to reach. Derived from the
# route the kernel would actually take, never from `hostname -I`, whose first
# entry on a docker host is a bridge nobody outside the host can reach.
addr="${LAN_ADDR:-$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -1)}"
if [[ -z "$addr" ]]; then
  echo "FAIL: cannot derive a LAN address — set LAN_ADDR=<ip of this host>" >&2
  exit 2
fi

# HTTPS IS NOT OPTIONAL HERE, and not because of a policy this script could
# waive: the server refuses a plaintext base URL off loopback, and it is right
# to — `crypto.subtle`, OPFS and `RTCPeerConnection` all need a SECURE
# CONTEXT, so a phone pointed at `http://192.168.x.y` would not have the APIs
# this product is built from. The two supported shapes:
#
#   CERT=/path/fullchain.pem KEY=/path/key.pem BASE_URL=https://name.example/
#       a real name and a real certificate — the deployment case, and the one
#       to use for an acceptance claim;
#   (nothing)
#       a self-signed certificate generated here for ${addr}. The recipient
#       device must accept the warning ONCE; after that the page is a secure
#       context and everything works. Never use this shape for a claim about
#       TLS overhead — it is a measurement convenience.
cert="${CERT:-}"
key="${KEY:-}"
if [[ -z "$cert" || -z "$key" ]]; then
  command -v openssl >/dev/null || {
    echo "FAIL: no CERT/KEY given and openssl is missing — cannot serve HTTPS" >&2
    exit 2
  }
  tlsdir="$(mktemp -d)"
  cert="$tlsdir/cert.pem"
  key="$tlsdir/key.pem"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$key" -out "$cert" -days 2 \
    -subj "/CN=${addr}" -addext "subjectAltName=IP:${addr}" >/dev/null 2>&1 || {
    echo "FAIL: openssl could not generate a self-signed certificate for ${addr}" >&2
    exit 2
  }
  echo "note: self-signed certificate for ${addr} (accept the warning on the recipient)"
fi
base="${BASE_URL:-https://${addr}:${PORT}/}"

srvlog="$(mktemp -d)/server.log"
ownerlog="$(dirname "$srvlog")/owner.log"
cleanup() {
  [[ -n "${owner_pid:-}" ]] && kill "$owner_pid" 2>/dev/null || true
  [[ -n "${server_pid:-}" ]] && kill "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT

# `--web-transfer-relay-rate 0` removes the 100 MiB/s bucket: measured
# through it, the relay arm would report the bucket and not the path.
"$bin" server \
  --control-port "$PORT" \
  --cert-file "$cert" \
  --key-file "$key" \
  --web-transfer-base-url "$base" \
  --web-transfer-relay-rate 0 \
  >"$srvlog" 2>&1 &
server_pid=$!
probe="${base%/}/transfer/assets/app.js"
for _ in $(seq 1 100); do
  curl -fsSk -o /dev/null "$probe" && break
  sleep 0.2
done
if ! curl -fsSk -o /dev/null "$probe"; then
  echo "FAIL: the server did not answer on ${base} — see $srvlog" >&2
  exit 2
fi

# The room lease is the SHIPPED command, not a test fixture: what an operator
# repeating this run would type.
"$bin" transfer web --to "$base" --insecure >"$ownerlog" 2>&1 &
owner_pid=$!
room=""
for _ in $(seq 1 150); do
  room="$(sed -n 's/^room: //p' "$ownerlog" | head -1)"
  [[ -n "$room" ]] && break
  sleep 0.2
done
if [[ -z "$room" ]]; then
  echo "FAIL: 'bore transfer web' printed no 'room: ' line — see $ownerlog" >&2
  exit 2
fi

echo "== bore web transfer, two hosts =="
echo "source : $(uname -sr), $(nproc) threads, engine=${ENGINE}"
echo "server : ${base}  (relay rate: unthrottled)"
echo "size   : ${SIZE_MB} MiB   reps=${REPS}   arms=${ARMS}"
echo "date   : $(date -Is)"
echo
echo "OPEN THIS ON THE OTHER HOST (phone, laptop — any browser):"
echo
echo "  $room"
echo
echo "Then, for every 'offered' line this script prints, press Scarica on that"
echo "device. The run waits up to $((WAIT_MS / 1000))s for each step."
echo
echo "Qualify the link FIRST, exactly as V-9 requires — a rate you cannot"
echo "compare to the line is not a result:"
echo "  iperf3 -c <the other host> -t 20        # and -P 8, the policer test"
echo "  ${bin} transfer listener / sender       # the native baseline, same route"
echo

out="$(mktemp -d)/lan.raw"
: > "$out"
status=0
IFS=',' read -r -a arms <<< "$ARMS"
for arm in "${arms[@]}"; do
  echo "-- arm ${arm} --"
  if BORE_LAN_ROOM_URL="$room" BORE_LAN_SIZE_MB="$SIZE_MB" BORE_LAN_REPS="$REPS" \
     BORE_LAN_ARM="$arm" BORE_LAN_WAIT_MS="$WAIT_MS" \
     npx --prefix web/transfer playwright test \
       --config web/transfer/playwright.perf.config.mjs \
       --project="$ENGINE" lan.perf.mjs 2>&1 | tee -a "$out" | grep -E "^PERF"; then
    :
  else
    echo "PERF lan-${arm} size=${SIZE_MB}MiB median=FAILED samples=[]" | tee -a "$out"
    status=1
  fi
  echo
done

echo "== summary (raw samples included on purpose) =="
grep -E "^PERF lan-(direct|relay) size=" "$out" || true
grep -E "^PERF lan-path-" "$out" || true
direct="$(sed -n 's/^PERF lan-direct size=[0-9]*MiB median=\([0-9.]*\)MiB.*/\1/p' "$out" | head -1)"
relay="$(sed -n 's/^PERF lan-relay size=[0-9]*MiB median=\([0-9.]*\)MiB.*/\1/p' "$out" | head -1)"
if [[ -n "$direct" && -n "$relay" ]]; then
  awk -v d="$direct" -v r="$relay" 'BEGIN{ if (r > 0) printf "PERF lan-direct-over-relay ratio=%.3fx\n", d / r }'
fi
if [[ -n "$direct" ]]; then
  awk -v d="$direct" 'BEGIN{ printf "PERF lan-direct-vs-goal goal=50.00MiB/s measured=%.2fMiB/s verdict=%s\n", d, (d >= 50 ? "MET" : "NOT-MET") }'
fi
echo
echo "raw: $out    server: $srvlog    room: $ownerlog"
exit "$status"
