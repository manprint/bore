#!/usr/bin/env bash
# Transfer-link performance gate.
#
# The baseline is the existing bore vhost serving the same hot origin and
# using the same server, TLS frontend and transport mode.  We measure three
# warm runs for each mode, compare medians, and also exercise three concurrent
# GETs.  This is a repeatable smoke threshold, not a claim of universal
# network speed.
set -Eeuo pipefail

for tool in cargo curl openssl python3 sha256sum awk; do
    command -v "$tool" >/dev/null || { printf 'missing prerequisite: %s\n' "$tool" >&2; exit 2; }
done

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BORE="${BORE:-$repo/target/release/bore}"
if [[ ! -x "$BORE" ]] ||
    find "$repo/src" "$repo/Cargo.toml" "$repo/Cargo.lock" -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    cargo build --locked --all-features --release --quiet
fi
[[ -x "$BORE" ]] || { printf 'missing bore binary: %s\n' "$BORE" >&2; exit 2; }
# Keep the measured transport from being dominated by the generic 256 KiB
# proxy-buffer default.  Both baseline and Link use this same value.
export BORE_PROXY_BUFFER_SIZE="${BORE_PROXY_BUFFER_SIZE:-16M}"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-transfer-link-perf.XXXXXX")
server_pid=''
origin_pid=''
provider_pid=''
link_pid=''
memory_producer_pid=''
memory_curl_pid=''
cleanup() {
    local status=$?
    set +e
    for pid in "$memory_curl_pid" "$memory_producer_pid" "$link_pid" "$provider_pid" "$origin_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            sleep 0.1
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [[ "${KEEP_TMP:-0}" == 1 ]]; then
        printf 'T-LINK-PERF temporary evidence: %s\n' "$tmp" >&2
    else
        rm -rf -- "$tmp"
    fi
    exit "$status"
}
trap cleanup EXIT INT TERM

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_tcp() {
    local port=$1 deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if python3 - "$port" <<'PY'
import socket, sys
s = socket.socket(); s.settimeout(.2)
try: s.connect(("127.0.0.1", int(sys.argv[1])))
except OSError: raise SystemExit(1)
finally: s.close()
PY
        then return 0; fi
        sleep .05
    done
    return 1
}

wait_url() {
    local file=$1 pid=$2 deadline=$((SECONDS + 25))
    while [[ ! -s "$file" ]] && (( SECONDS < deadline )); do
        kill -0 "$pid" 2>/dev/null || return 1
        sleep .05
    done
    [[ -s "$file" ]]
}

url_parts() {
    local url=$1
    read -r url_host url_port <<<"$(python3 - "$url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != "https" or not u.hostname or not u.port: raise SystemExit(1)
print(u.hostname, u.port)
PY
)"
    url_resolve="$url_host:$url_port:127.0.0.1"
}

cat >"$tmp/ca.cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bore transfer-link performance CA
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$tmp/ca.key" -out "$tmp/ca.pem" \
    -config "$tmp/ca.cnf" -extensions ca_ext >/dev/null 2>&1
cat >"$tmp/leaf.cnf" <<'EOF'
[req]
distinguished_name = dn
req_extensions = req_ext
prompt = no
[dn]
CN = bore transfer-link performance server
[req_ext]
subjectAltName = DNS:localhost,DNS:bore.local,DNS:*.bore.local
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost,DNS:bore.local,DNS:*.bore.local
authorityKeyIdentifier = keyid,issuer
subjectKeyIdentifier = hash
EOF
openssl req -newkey rsa:2048 -nodes -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" \
    -config "$tmp/leaf.cnf" >/dev/null 2>&1
openssl x509 -req -days 1 -sha256 -in "$tmp/leaf.csr" \
    -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" -CAcreateserial \
    -out "$tmp/leaf.pem" -extfile "$tmp/leaf.cnf" -extensions leaf_ext >/dev/null 2>&1

control_port=$(free_port)
http_port=$(free_port)
https_port=$(free_port)
quic_port=$(free_port)
origin_port=$(free_port)
secret="perf-$RANDOM-$$"
mkdir "$tmp/origin"
dd if=/dev/zero of="$tmp/origin/payload.bin" bs=1M count=1024 status=none
sha256sum "$tmp/origin/payload.bin" >"$tmp/expected.sha"

# The baseline origin performs the same streaming SHA-256 work as Link while
# serving the same file.  This keeps the comparison focused on the vhost/Link
# orchestration instead of rewarding a baseline that omits integrity work.
python3 - "$origin_port" "$tmp/origin/payload.bin" >"$tmp/origin.log" 2>&1 <<'PY' &
import hashlib
import http.server
import os
import sys

port = int(sys.argv[1])
source = sys.argv[2]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/payload.bin":
            self.send_error(404)
            return
        size = os.stat(source).st_size
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        digest = hashlib.sha256()
        with open(source, "rb", buffering=1024 * 1024) as inp:
            while True:
                block = inp.read(1024 * 1024)
                if not block:
                    break
                digest.update(block)
                self.wfile.write(block)
        digest.digest()

    def log_message(self, *_args):
        pass

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
origin_pid=$!
"$BORE" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port "$control_port" --secret "$secret" \
    --cert-file "$tmp/leaf.pem" --key-file "$tmp/leaf.key" \
    --vhost-base-domain bore.local --vhost-mode https \
    --vhost-http-port "$http_port" --vhost-https-port "$https_port" \
    --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
    --vhost-quic-port "$quic_port" --udp \
    >"$tmp/server.log" 2>&1 &
server_pid=$!
wait_tcp "$control_port"

measure_url() {
    local label=$1 url=$2 resolve=$3 out="$tmp/$1.speeds"
    : >"$out"
    # Warm the origin/page cache and the carrier before recording values.
    curl --fail --silent --show-error --cacert "$tmp/ca.pem" --resolve "$resolve" \
        "$url" -o /dev/null >/dev/null
    for _ in 1 2 3; do
        curl --fail --silent --show-error --cacert "$tmp/ca.pem" --resolve "$resolve" \
            -w '%{speed_download} %{time_starttransfer}\n' "$url" -o /dev/null >>"$out"
    done
    read -r median_speed median_ttfb <<<"$(python3 - "$out" <<'PY'
import statistics, sys
rows = [tuple(map(float, line.split())) for line in open(sys.argv[1]) if line.strip()]
if len(rows) != 3: raise SystemExit("expected three measurements")
print(int(statistics.median(x[0] for x in rows)), f"{statistics.median(x[1] for x in rows):.6f}")
PY
)"
    printf '%s speed=%sB/s ttfb=%ss\n' "$label" "$median_speed" "$median_ttfb"
}

compare_medians() {
    local label=$1 baseline=$2 link=$3 minimum=$4
    python3 - "$label" "$baseline" "$link" "$minimum" <<'PY'
import sys
label, baseline, link, minimum = sys.argv[1], float(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
ratio = link / baseline if baseline else 0.0
print(f"{label} median ratio={ratio:.3f}")
assert ratio >= minimum, (label, ratio, minimum)
PY
}

run_mode() {
    local mode=$1 provider_args=() link_args=() baseline_url baseline_resolve
    if [[ "$mode" == direct ]]; then
        provider_args=(--udp)
        link_args=()
    else
        link_args=(--relay-only)
    fi

    "$BORE" vhost "127.0.0.1:$origin_port" --subdomain "perf-$mode" \
        --id "perf-$mode" --to "https://localhost:$control_port" \
        --secret "$secret" --insecure "${provider_args[@]}" \
        >"$tmp/provider-$mode.log" 2>&1 &
    provider_pid=$!
    sleep 1
    baseline_url="https://perf-$mode.bore.local:$https_port/payload.bin"
    baseline_resolve="perf-$mode.bore.local:$https_port:127.0.0.1"
    baseline_speed=$(measure_url "baseline-$mode" "$baseline_url" "$baseline_resolve" |
        awk '/speed=/{sub(/^speed=/,"",$2); sub(/B\/s$/,"",$2); print $2}')
    kill "$provider_pid" 2>/dev/null || true
    wait "$provider_pid" 2>/dev/null || true
    provider_pid=''

    "$BORE" -v transfer link "$tmp/origin/payload.bin" --filename payload.bin \
        --to "https://localhost:$control_port" --secret "$secret" \
        --ca-cert "$tmp/ca.pem" "${link_args[@]}" \
        >"$tmp/link-$mode.url" 2>"$tmp/link-$mode.log" &
    link_pid=$!
    wait_url "$tmp/link-$mode.url" "$link_pid" || { tail -n 80 "$tmp/link-$mode.log" >&2; return 1; }
    link_url=$(head -n 1 "$tmp/link-$mode.url")
    url_parts "$link_url"
    link_speed=$(measure_url "link-$mode" "$link_url" "$url_resolve" |
        awk '/speed=/{sub(/^speed=/,"",$2); sub(/B\/s$/,"",$2); print $2}')
    link_ttfb=$(awk '{print $2}' "$tmp/link-$mode.speeds" | sort -n | sed -n '2p')
    if [[ "$mode" == direct ]]; then
        compare_medians "T-LINK-PERF $mode" "$baseline_speed" "$link_speed" 0.90
    else
        # Relay baseline uses kernel splice. Link must additionally run the
        # mandatory SHA-256/source validation pipeline, so this gate records
        # the measured integrity cost instead of allowing it to be hidden.
        compare_medians "T-LINK-PERF $mode" "$baseline_speed" "$link_speed" 0.75
    fi
    python3 - "$link_ttfb" <<'PY'
import sys
assert float(sys.argv[1]) <= .100, sys.argv[1]
PY
    # Three simultaneous GETs exercise independent downloads without claiming
    # that one HTTP stream is striped across carriers.
    for n in 1 2 3; do
        curl --fail --silent --show-error --cacert "$tmp/ca.pem" --resolve "$url_resolve" \
            "$link_url" -o "$tmp/link-$mode-concurrent-$n.out" &
        concurrent_pids[n]=$!
    done
    for n in 1 2 3; do wait "${concurrent_pids[$n]}"; done
    for n in 1 2 3; do cmp "$tmp/origin/payload.bin" "$tmp/link-$mode-concurrent-$n.out"; done
    printf 'T-LINK-PERF %s concurrent=3 PASS\n' "$mode"
    kill -INT "$link_pid" 2>/dev/null || true
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
}

run_stdin_memory() {
    local fifo="$tmp/memory.fifo" stop="$tmp/memory-rss.stop"
    local url_file="$tmp/memory.url" log_file="$tmp/memory.log"
    local result="$tmp/memory.result.json" rss="$tmp/memory.rss.json"
    local total=$((15 * 1024 * 1024 * 1024))
    local expected_sha='55a20aa6ffa6c4c931daa2fcf823213783ab13a8bd9e15e05fc93143b038c5c4'
    mkfifo "$fifo"
    cat >"$tmp/memory-sink.py" <<'PY'
import hashlib
import json
import sys

output, expected_size, expected_sha = sys.argv[1:]
expected_size = int(expected_size)
digest = hashlib.sha256()
size = 0
while True:
    block = sys.stdin.buffer.read(1024 * 1024)
    if not block:
        break
    size += len(block)
    digest.update(block)
with open(output, "w", encoding="ascii") as stream:
    json.dump({"bytes": size, "sha256": digest.hexdigest()}, stream)
if size != expected_size or digest.hexdigest() != expected_sha:
    raise SystemExit(f"stdin integrity mismatch: bytes={size} sha256={digest.hexdigest()}")
PY
    cat >"$tmp/memory-producer.py" <<'PY'
import os
import sys

fifo, total = sys.argv[1:]
total = int(total)
block = b"\0" * (1024 * 1024)
fd = os.open(fifo, os.O_WRONLY)
try:
    while total:
        view = memoryview(block)[:min(len(block), total)]
        while view:
            written = os.write(fd, view)
            view = view[written:]
        total -= min(len(block), total)
finally:
    os.close(fd)
PY
    python3 "$tmp/memory-producer.py" "$fifo" "$total" >"$tmp/memory-producer.log" 2>&1 &
    memory_producer_pid=$!
    : >"$url_file"
    : >"$log_file"
    "$BORE" transfer link --stdin --filename memory.bin --max-downloads 1 \
        --stats-interval 60 --to "https://localhost:$control_port" \
        --secret "$secret" --ca-cert "$tmp/ca.pem" \
        <"$fifo" >"$url_file" 2>"$log_file" &
    link_pid=$!
    wait_url "$url_file" "$link_pid" || { tail -n 80 "$log_file" >&2; return 1; }
    local memory_url
    memory_url=$(head -n 1 "$url_file")
    url_parts "$memory_url"
    cat >"$tmp/memory-rss.py" <<'PY'
import json
import os
import stat
import sys
import time

sender, server, output, stop, tmpdir = sys.argv[1:]
pids = [int(sender), int(server)]
baseline = [None, None]
peak_delta = [0, 0]
spool = []

def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status", encoding="ascii") as stream:
            for line in stream:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError, ValueError):
        return None
    return None

def payload_fd(pid):
    root = f"/proc/{pid}/fd"
    try:
        for name in os.listdir(root):
            fd = os.path.join(root, name)
            try:
                target = os.readlink(fd)
                mode = os.stat(fd).st_mode
            except (FileNotFoundError, OSError):
                continue
            if not stat.S_ISREG(mode):
                continue
            # Certificates, URL/log files and the shell's descriptors are
            # expected.  A payload extension or a deleted regular file is a
            # source spool and fails the gate even while the stream is live.
            if target.endswith((".bin", ".zip", ".tar", ".part")) or " (deleted)" in target:
                return target
    except (FileNotFoundError, OSError):
        return None
    return None

while not os.path.exists(stop):
    alive = False
    for index, pid in enumerate(pids):
        value = rss_kib(pid)
        if value is not None:
            alive = True
            if baseline[index] is None:
                baseline[index] = value
            peak_delta[index] = max(peak_delta[index], value - baseline[index])
        target = payload_fd(pid)
        if target is not None:
            spool.append(target)
    if not alive:
        break
    time.sleep(0.2)

with open(output, "w", encoding="ascii") as stream:
    json.dump({"baseline_kib": baseline, "peak_delta_kib": peak_delta, "spool": spool[:4]}, stream)
PY
    python3 "$tmp/memory-rss.py" "$link_pid" "$server_pid" "$rss" "$stop" "$tmp" &
    local rss_pid=$!
    set +e
    curl --fail --silent --show-error --max-time 900 --cacert "$tmp/ca.pem" \
        --resolve "$url_resolve" "$memory_url" |
        python3 "$tmp/memory-sink.py" "$result" "$total" "$expected_sha"
    local -a statuses=("${PIPESTATUS[@]}")
    set -e
    touch "$stop"
    wait "$rss_pid" 2>/dev/null || true
    if (( statuses[0] != 0 || statuses[1] != 0 )); then
        printf '15 GiB stdin pipeline failed: curl=%s sink=%s\n' "${statuses[0]}" "${statuses[1]}" >&2
        return 1
    fi
    python3 - "$rss" <<'PY'
import json
import sys
summary = json.load(open(sys.argv[1], encoding="ascii"))
sender, server = summary["peak_delta_kib"]
print(f"T-LINK-MEMORY stdin bytes=15GiB sender_rss_delta={sender}KiB server_rss_delta={server}KiB spool={summary['spool']}")
assert sender <= 128 * 1024, sender
assert server <= 64 * 1024, server
assert not summary["spool"], summary["spool"]
PY
    wait "$memory_producer_pid"
    memory_producer_pid=''
    kill -INT "$link_pid" 2>/dev/null || true
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
    printf 'T-LINK-MEMORY PASS: 15 GiB stdin streamed with bounded RSS and no payload spool\n'
}

run_mode direct
run_mode relay
run_stdin_memory
printf 'T-LINK-PERF PASS: direct >=90%% and relay >=75%% of matched vhost baseline (relay includes mandatory SHA-256), TTFB <=100ms, concurrency=3\n'
