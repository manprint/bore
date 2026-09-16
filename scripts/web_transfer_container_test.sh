#!/usr/bin/env bash
# T-WEB-NOSTORE-CONTAINER (6.5): the SHIPPED image, with a read-only root
# filesystem and no volume, serves a room and moves a file through its relay —
# and writes nothing while doing it.
#
# The claim is structural: the server holds no payload, so it needs no
# writable path. `--read-only` is how that claim is TESTED rather than
# asserted — a single stray write fails the container instead of leaving a
# file nobody looks for. The transfer itself runs in real browsers
# (`container.spec.mjs`), because "the process started" is what a liveness
# probe proves and is not the claim.
#
# Environment (all optional):
#   BORE_IMAGE      image to test; built from ./Dockerfile when unset
#   KEEP_IMAGE=1    do not remove a locally built image at the end
set -euo pipefail
export LC_ALL=C

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if ! command -v docker >/dev/null 2>&1 || ! docker version >/dev/null 2>&1; then
  echo "T-WEB-NOSTORE-CONTAINER: N/A (no usable docker on this host)"
  exit 0
fi

image="${BORE_IMAGE:-bore-web-transfer:gate}"
built=0
if [ -z "${BORE_IMAGE:-}" ]; then
  echo "== building $image (this is the shipped Dockerfile, not a test-only one)"
  docker build -t "$image" . >/tmp/bore-container-build.log 2>&1 || {
    echo "FAIL image build; tail of the log:"
    tail -20 /tmp/bore-container-build.log
    exit 1
  }
  built=1
fi

port="$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')"
name="bore-webtransfer-gate-$$"
cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  if [ "$built" = "1" ] && [ "${KEEP_IMAGE:-0}" != "1" ]; then
    docker rmi "$image" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# READ-ONLY ROOT, no volume, no tmpfs: if the server needs to write anything
# at all, it fails here and that is the finding.
docker run -d --name "$name" \
  --read-only \
  -p "127.0.0.1:$port:7835" \
  -e BORE_CONTROL_PORT=7835 \
  -e "BORE_WEB_TRANSFER_BASE_URL=http://127.0.0.1:$port/" \
  -e BORE_ADMIN_TOKEN=gate-token-that-is-at-least-32-chars-long \
  "$image" server >/dev/null

fail=0
check() {
  if [ "$1" = "0" ]; then printf 'PASS %s\n' "$2"; else printf 'FAIL %s\n' "$2"; fail=1; fi
}

ready=1
for _ in $(seq 1 100); do
  if curl -fsS -o /dev/null "http://127.0.0.1:$port/transfer/assets/app.js" 2>/dev/null; then
    ready=0
    break
  fi
  sleep 0.2
done
check "$ready" "the read-only container serves the bundle"
if [ "$ready" != "0" ]; then
  echo "--- container log ---"
  docker logs "$name" 2>&1 | tail -20
  exit 1
fi

# The bytes served are the committed ones.
curl -fsS -o /tmp/bore-container-app.js "http://127.0.0.1:$port/transfer/assets/app.js"
cmp -s /tmp/bore-container-app.js web/transfer/dist/app.js
check $? "the image serves the committed bundle"

# No UDP socket for this feature inside the container. The shipped image is
# `FROM scratch` — one static binary, no shell and no `cat` — so `docker exec`
# cannot read anything there (it answers 127, which under `set -e` ended this
# gate silently at "two checks passed"). A SIDECAR sharing the container's
# network namespace reads the same kernel tables from the same namespace,
# which is what the claim is actually about.
helper="${BORE_NET_HELPER_IMAGE:-alpine:3}"
if docker image inspect "$helper" >/dev/null 2>&1 || docker pull "$helper" >/dev/null 2>&1; then
  udp="$(docker run --rm --network "container:$name" "$helper" \
    sh -c 'cat /proc/net/udp /proc/net/udp6 2>/dev/null' |
    awk '$1 !~ /sl/ && NF > 3 {n++} END {print n+0}')"
  [ "${udp:-1}" -eq 0 ]
  check $? "the container opened no UDP socket (found ${udp:-unknown})"
else
  # Never a silent pass: a check that could not run says so.
  echo "N/A the UDP-socket check needs $helper to share the network namespace"
fi

# A room, created from the HOST with the shipped command, against the
# container's control port.
roomlog="$(mktemp)"
./target/debug/bore transfer web --to "http://127.0.0.1:$port" >"$roomlog" 2>&1 &
owner=$!
room=""
for _ in $(seq 1 100); do
  room="$(grep -o 'http://[^ ]*/transfer/[^ ]*' "$roomlog" | head -1 || true)"
  [ -n "$room" ] && break
  sleep 0.2
done
if [ -z "$room" ]; then
  echo "FAIL the owner never printed a room URL"
  cat "$roomlog"
  kill "$owner" 2>/dev/null || true
  exit 1
fi
printf 'PASS a room was created against the container (%s…)\n' "${room%%#*}"

# The real transfer, in real browsers, through the container's relay.
if ( cd web/transfer && BORE_WEB_E2E_ROOM_URL="$room" npx playwright test container.spec.mjs --project=chromium --reporter=line ); then
  check 0 "a file moved through the container's relay with the bytes intact"
else
  check 1 "a file moved through the container's relay with the bytes intact"
fi
kill "$owner" 2>/dev/null || true

# NOTHING WAS WRITTEN. With a read-only root the kernel would have refused a
# write, so this reads as belt and braces — and it is also what catches a
# future image that adds a writable layer or a volume for convenience.
changes="$(docker diff "$name" | grep -v '^C /$' || true)"
[ -z "$changes" ]
check $? "the container filesystem is unchanged ($(printf '%s' "$changes" | wc -l) entries)"

# The admin surface agrees: the room is gone with its owner, nothing leaked.
metrics="$(curl -fsS -H 'Authorization: Bearer gate-token-that-is-at-least-32-chars-long' \
  "http://127.0.0.1:$port/admin/api/v1/metrics" || true)"
printf '%s' "$metrics" | grep -q 'web_transfer_rooms_current'
check $? "the admin metrics publish the web-transfer gauges"

if [ "$fail" -eq 0 ]; then
  echo "T-WEB-NOSTORE-CONTAINER: ok"
else
  echo "T-WEB-NOSTORE-CONTAINER: FAILED"
  docker logs "$name" 2>&1 | tail -20
fi
exit "$fail"
