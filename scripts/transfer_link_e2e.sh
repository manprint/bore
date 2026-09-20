#!/usr/bin/env bash
set -Eeuo pipefail

# Small, self-contained public-link smoke harness.  It creates a private CA and
# wildcard certificate, runs the real server and client binaries, and uses a
# normal HTTPS client against the announced vhost URL.  No public DNS or
# certificate bypass is used.

mode=${1:-basic}
if [[ "$mode" != basic && "$mode" != large ]]; then
    printf 'unsupported transfer-link e2e mode: %s\n' "$mode" >&2
    exit 2
fi

for tool in cargo openssl curl wget python3; do
    command -v "$tool" >/dev/null || {
        printf 'missing prerequisite: %s\n' "$tool" >&2
        exit 2
    }
done

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin="$repo/target/debug/bore"
cargo build --offline --locked --all-features --quiet
[[ -x "$bin" ]] || {
    printf 'build did not produce %s\n' "$bin" >&2
    exit 2
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-transfer-link.XXXXXX")
server_pid=''
link_pid=''
stdin_pipeline_pid=''
stdin_blocker_pid=''
stdin_cancel_link_pid=''
stdin_cancel_curl_pid=''
stdin_cancel_producer_pid=''
rss_monitor_pid=''
cleanup() {
    local status=$?
    if [[ -n "$link_pid" ]] && kill -0 "$link_pid" 2>/dev/null; then
        kill -TERM "$link_pid" 2>/dev/null || true
        wait "$link_pid" 2>/dev/null || true
    fi
    if [[ -n "$stdin_pipeline_pid" ]] && kill -0 "$stdin_pipeline_pid" 2>/dev/null; then
        kill -TERM "$stdin_pipeline_pid" 2>/dev/null || true
        wait "$stdin_pipeline_pid" 2>/dev/null || true
    fi
    if [[ -n "$stdin_blocker_pid" ]] && kill -0 "$stdin_blocker_pid" 2>/dev/null; then
        kill -TERM "$stdin_blocker_pid" 2>/dev/null || true
        wait "$stdin_blocker_pid" 2>/dev/null || true
    fi
    for pid in "$stdin_cancel_curl_pid" "$stdin_cancel_link_pid" "$stdin_cancel_producer_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [[ -n "$rss_monitor_pid" ]] && kill -0 "$rss_monitor_pid" 2>/dev/null; then
        kill -TERM "$rss_monitor_pid" 2>/dev/null || true
        wait "$rss_monitor_pid" 2>/dev/null || true
    fi
    if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
        kill -TERM "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    if [[ "${BORE_TRANSFER_LINK_KEEP_ARTIFACTS:-0}" == 1 ]]; then
        printf 'harness artifacts retained at %s\n' "$tmp" >&2
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
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_tcp() {
    local port=$1
    local deadline=$((SECONDS + 15))
    while (( SECONDS < deadline )); do
        if python3 - "$port" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(0.2)
try:
    s.connect(('127.0.0.1', int(sys.argv[1])))
except OSError:
    raise SystemExit(1)
finally:
    s.close()
PY
        then
            return 0
        fi
        sleep 0.05
    done
    printf 'timed out waiting for TCP port %s\n' "$port" >&2
    return 1
}

control_port=$(free_port)
http_port=$(free_port)
https_port=$(free_port)
quic_port=$(free_port)

cat >"$tmp/ca.cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bore transfer-link test
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
CN = bore transfer-link test server
[req_ext]
subjectAltName = DNS:localhost,DNS:bore.local,DNS:*.bore.local
EOF
openssl req -newkey rsa:2048 -nodes \
    -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" \
    -config "$tmp/leaf.cnf" >/dev/null 2>&1
cat >>"$tmp/leaf.cnf" <<'EOF'
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:localhost,DNS:bore.local,DNS:*.bore.local
authorityKeyIdentifier = keyid,issuer
subjectKeyIdentifier = hash
EOF
openssl x509 -req -days 1 -sha256 \
    -in "$tmp/leaf.csr" -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" \
    -CAcreateserial -out "$tmp/leaf.pem" \
    -extfile "$tmp/leaf.cnf" -extensions leaf_ext >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj '/CN=untrusted transfer-link test' \
    -keyout "$tmp/wrong-ca.key" -out "$tmp/wrong-ca.pem" >/dev/null 2>&1

if [[ "$mode" == basic ]]; then
    printf 'fixture transfer-link payload\n' >"$tmp/source.bin"
    printf 'fixture transfer-link payload\n' >"$tmp/expected.bin"
    source_arg="$tmp/source.bin"
else
    mkdir "$tmp/large-root"
    # A true ZIP64 payload: the source file crosses 2^32 bytes while remaining
    # sparse on the fixture filesystem.  The final marker makes the tail check
    # independent of the filesystem's zero-fill implementation.
    truncate -s 4294967297 "$tmp/large-root/sparse.bin"
    python3 - "$tmp/large-root/sparse.bin" <<'PY'
import sys
with open(sys.argv[1], "r+b") as stream:
    stream.seek(4294967296)
    stream.write(b"Z")
PY
    printf 'small archive entry\n' >"$tmp/large-root/small.txt"
    source_arg="$tmp/large-root"
fi

start_server() {
    "$bin" server \
        --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
        --control-port "$control_port" \
        --cert-file "$tmp/leaf.pem" --key-file "$tmp/leaf.key" \
        --vhost-base-domain bore.local --vhost-mode https \
        --vhost-http-port "$http_port" --vhost-https-port "$https_port" \
        --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
        --vhost-quic-port "$quic_port" --udp \
        >>"$tmp/server.log" 2>&1 &
    server_pid=$!
    wait_tcp "$control_port"
}

start_server

"$bin" -vv transfer link "$source_arg" \
    --to "https://localhost:$control_port" \
    --ca-cert "$tmp/ca.pem" --max-downloads 4 \
    >"$tmp/url.txt" 2>"$tmp/link.log" &
link_pid=$!

deadline=$((SECONDS + 20))
while [[ ! -s "$tmp/url.txt" ]] && (( SECONDS < deadline )); do
    if ! kill -0 "$link_pid" 2>/dev/null; then
        cat "$tmp/link.log" >&2 || true
        printf 'transfer-link client exited before URL publication\n' >&2
        exit 1
    fi
    sleep 0.05
done
[[ -s "$tmp/url.txt" ]] || {
    cat "$tmp/link.log" >&2 || true
    printf 'timed out waiting for transfer-link URL\n' >&2
    exit 1
}
url=$(head -n 1 "$tmp/url.txt")
[[ "$url" == https://* ]] || {
    printf 'unexpected URL: %s\n' "$url" >&2
    exit 1
}
read -r host port <<<"$(python3 - "$url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != 'https' or not u.hostname or not u.port:
    raise SystemExit('URL has no explicit HTTPS host/port')
print(u.hostname, u.port)
PY
)"
resolve="$host:$port:127.0.0.1"

# The vhost label and the URL path are bearer credentials.  At trace logging
# the only Link correlation field allowed is the process-local session id;
# neither the host/URL nor the advertised filename may appear in stderr.
if grep -F -e "$host" -e "$url" -e 'source.bin' "$tmp/link.log" >/dev/null; then
    printf 'transfer-link verbose log leaked a bearer URL component\n' >&2
    cat "$tmp/link.log" >&2
    exit 1
fi
grep -q 'session_id=' "$tmp/link.log"

curl_args=(--fail --silent --show-error --cacert "$tmp/ca.pem" --resolve "$resolve")

if [[ "$mode" == large ]]; then
    # Keep the 4+ GiB archive out of a dense fixture file.  The sink preserves
    # every byte, seeks across zero-only blocks, and reports an independent
    # SHA-256/length summary.  PIPESTATUS keeps a successful sink from hiding a
    # failed curl.
    set +e
    curl_args+=(--max-time 300)
    python3 - "$link_pid" "$server_pid" "$tmp/rss.json" "$tmp/rss.stop" <<'PY' &
import json
import os
import sys
import time

sender_pid = int(sys.argv[1])
server_pid = int(sys.argv[2])
output = sys.argv[3]
stop = sys.argv[4]
pids = [sender_pid, server_pid]
baseline = [None, None]
peak_delta = [0, 0]

def rss_kib(pid):
    try:
        with open(f"/proc/{pid}/status", encoding="ascii") as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError, ValueError):
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
    if not alive:
        break
    time.sleep(0.2)

with open(output, "w", encoding="ascii") as stream:
    json.dump({"baseline_kib": baseline, "peak_delta_kib": peak_delta}, stream)
PY
    rss_monitor_pid=$!
    curl "${curl_args[@]}" "$url" | python3 "$repo/scripts/transfer_link_sparse_sink.py" \
        "$tmp/large.zip" >"$tmp/large-sink.json"
    pipeline_status=("${PIPESTATUS[@]}")
    set -e
    touch "$tmp/rss.stop"
    wait "$rss_monitor_pid" 2>/dev/null || true
    rss_monitor_pid=''
    if (( pipeline_status[0] != 0 || pipeline_status[1] != 0 )); then
        printf 'ZIP64 curl/sink failed: curl=%s sink=%s\n' \
            "${pipeline_status[0]}" "${pipeline_status[1]}" >&2
        exit 1
    fi
    python3 - "$tmp/rss.json" <<'PY'
import json
import sys
summary = json.load(open(sys.argv[1], encoding="ascii"))
sender, server = summary["peak_delta_kib"]
print(f"transfer-link large RSS delta: sender={sender}KiB server={server}KiB")
assert sender <= 128 * 1024, sender
assert server <= 64 * 1024, server
PY
    python3 - "$tmp/large.zip" "$tmp/large-sink.json" <<'PY'
import hashlib
import json
import sys
import zipfile

archive_path, summary_path = sys.argv[1:]
summary = json.load(open(summary_path))
assert summary["bytes"] > 2**32
with zipfile.ZipFile(archive_path) as archive:
    names = archive.namelist()
    assert "large-root/" in names
    assert "large-root/sparse.bin" in names
    assert "large-root/small.txt" in names
    sparse = archive.getinfo("large-root/sparse.bin")
    assert sparse.compress_type == zipfile.ZIP_STORED
    assert sparse.file_size == 2**32 + 1
    digest = hashlib.sha256()
    with archive.open(sparse) as stream:
        while True:
            block = stream.read(1024 * 1024)
            if not block:
                break
            digest.update(block)
    expected = hashlib.sha256()
    zero_block = b"\0" * (1024 * 1024)
    for _ in range(4096):
        expected.update(zero_block)
    expected.update(b"Z")
    assert digest.digest() == expected.digest()
PY

    # Exercise the independent ZIP64 entry-count boundary (> 65535 entries).
    mkdir "$tmp/many-root"
    python3 - "$tmp/many-root" <<'PY'
import os
import sys
root = sys.argv[1]
for index in range(65536):
    open(os.path.join(root, f"entry-{index:05d}"), "wb").close()
PY
    kill -INT "$link_pid"
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
    "$bin" transfer link "$tmp/many-root" --filename many.zip \
        --to "https://localhost:$control_port" --ca-cert "$tmp/ca.pem" \
        --max-downloads 1 >"$tmp/many-url.txt" 2>"$tmp/many-link.log" &
    link_pid=$!
    deadline=$((SECONDS + 30))
    while [[ ! -s "$tmp/many-url.txt" ]] && (( SECONDS < deadline )); do
        if ! kill -0 "$link_pid" 2>/dev/null; then
            cat "$tmp/many-link.log" >&2 || true
            printf 'entry-count transfer-link client exited before URL publication\n' >&2
            exit 1
        fi
        sleep 0.05
    done
    [[ -s "$tmp/many-url.txt" ]] || {
        cat "$tmp/many-link.log" >&2 || true
        printf 'timed out waiting for entry-count URL\n' >&2
        exit 1
    }
    many_url=$(head -n 1 "$tmp/many-url.txt")
    read -r many_host many_port <<<"$(python3 - "$many_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
print(u.hostname, u.port)
PY
)"
    many_resolve="$many_host:$many_port:127.0.0.1"
    curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
        --resolve "$many_resolve" "$many_url" -o "$tmp/many.zip"
    python3 - "$tmp/many.zip" <<'PY'
import sys
import zipfile
with zipfile.ZipFile(sys.argv[1]) as archive:
    entries = archive.infolist()
    assert len(entries) == 65537, len(entries)
    assert all(item.compress_type == zipfile.ZIP_STORED for item in entries)
PY
    kill -INT "$link_pid"
    deadline=$((SECONDS + 10))
    while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < deadline )); do
        sleep 0.05
    done
    if kill -0 "$link_pid" 2>/dev/null; then
        printf 'large transfer-link client did not stop after SIGINT\n' >&2
        exit 1
    fi
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
    printf 'transfer-link large: PASS\n'
    exit 0
fi

if curl --fail --silent --show-error --cacert "$tmp/wrong-ca.pem" --resolve "$resolve" "$url" \
    -o "$tmp/wrong.bin" 2>"$tmp/wrong-ca.log"; then
    printf 'download unexpectedly trusted an unrelated CA\n' >&2
    exit 1
fi
curl "${curl_args[@]}" "$url" -o "$tmp/download.bin"
cmp "$tmp/expected.bin" "$tmp/download.bin"
sha256sum "$tmp/expected.bin" "$tmp/download.bin" >/dev/null

# HEAD is metadata-only and a Range request is deliberately served as the
# complete object.  Both checks use the same normal HTTPS URL.
head -n 1 <(curl "${curl_args[@]}" -I "$url") | grep -q '200'
curl "${curl_args[@]}" -H 'Range: bytes=0-1' "$url" -o "$tmp/range.bin"
cmp "$tmp/expected.bin" "$tmp/range.bin"

# Repeat and concurrent downloads each open an independent source handle.
curl "${curl_args[@]}" "$url" -o "$tmp/repeat.bin"
cmp "$tmp/expected.bin" "$tmp/repeat.bin"
wget --quiet --ca-certificate "$tmp/ca.pem" \
    --header "Host: $host" \
    --output-document "$tmp/wget.bin" "https://localhost:$port/source.bin"
cmp "$tmp/expected.bin" "$tmp/wget.bin"
download_pids=()
for n in 1 2 3; do
    curl "${curl_args[@]}" "$url" -o "$tmp/concurrent-$n.bin" &
    download_pids+=("$!")
done
for pid in "${download_pids[@]}"; do
    wait "$pid"
done
for n in 1 2 3; do
    cmp "$tmp/expected.bin" "$tmp/concurrent-$n.bin"
done

# A server restart must preserve the public label. The supervisor reconnects
# the vhost and a fresh HTTP request then uses the same URL.
old_url=$url
kill -TERM "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=''
sleep 0.2
start_server
[[ "$(head -n 1 "$tmp/url.txt")" == "$old_url" ]]
reconnect_ok=0
deadline=$((SECONDS + 25))
while (( SECONDS < deadline )); do
    if curl --connect-timeout 1 --max-time 3 "${curl_args[@]}" "$old_url" -o "$tmp/reconnect.bin" 2>/dev/null; then
        reconnect_ok=1
        break
    fi
    sleep 0.2
done
(( reconnect_ok == 1 )) || {
    printf 'transfer-link supervisor did not reconnect before deadline\n' >&2
    exit 1
}
cmp "$tmp/expected.bin" "$tmp/reconnect.bin"

kill -INT "$link_pid"
deadline=$((SECONDS + 10))
while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < deadline )); do
    sleep 0.05
done
if kill -0 "$link_pid" 2>/dev/null; then
    printf 'transfer-link client did not stop after SIGINT\n' >&2
    exit 1
fi
wait "$link_pid" 2>/dev/null || true
link_pid=''
grep -q 'transfer-link download completed' "$tmp/link.log"
grep -q 'sha256=' "$tmp/link.log"

# Reuse the server for a second scoped registration with explicit TCP relay.
"$bin" -v transfer link "$tmp/source.bin" \
    --to "https://localhost:$control_port" \
    --ca-cert "$tmp/ca.pem" --relay-only --max-downloads 4 \
    >"$tmp/relay-url.txt" 2>"$tmp/relay-link.log" &
link_pid=$!
deadline=$((SECONDS + 20))
while [[ ! -s "$tmp/relay-url.txt" ]] && (( SECONDS < deadline )); do
    if ! kill -0 "$link_pid" 2>/dev/null; then
        cat "$tmp/relay-link.log" >&2 || true
        printf 'relay-only transfer-link client exited before URL publication\n' >&2
        exit 1
    fi
    sleep 0.05
done
[[ -s "$tmp/relay-url.txt" ]] || {
    cat "$tmp/relay-link.log" >&2 || true
    printf 'timed out waiting for relay-only URL\n' >&2
    exit 1
}
relay_url=$(head -n 1 "$tmp/relay-url.txt")
read -r relay_host relay_port <<<"$(python3 - "$relay_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != 'https' or not u.hostname or not u.port:
    raise SystemExit('relay URL has no explicit HTTPS host/port')
print(u.hostname, u.port)
PY
)"
relay_resolve="$relay_host:$relay_port:127.0.0.1"
if grep -F -e "$relay_host" -e "$relay_url" -e 'source.bin' "$tmp/relay-link.log" >/dev/null; then
    printf 'transfer-link info log leaked a bearer URL component\n' >&2
    cat "$tmp/relay-link.log" >&2
    exit 1
fi
grep -q 'session_id=' "$tmp/relay-link.log"
curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$relay_resolve" "$relay_url" -o "$tmp/relay.bin"
cmp "$tmp/expected.bin" "$tmp/relay.bin"
grep -q 'relay_only=true' "$tmp/relay-link.log"
kill -INT "$link_pid"
deadline=$((SECONDS + 10))
while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < deadline )); do
    sleep 0.05
done
if kill -0 "$link_pid" 2>/dev/null; then
    printf 'relay-only transfer-link client did not stop after SIGINT\n' >&2
    exit 1
fi
wait "$link_pid" 2>/dev/null || true
link_pid=''

# Stdin is deliberately a one-shot stream.  The external producer starts
# before the GET and naturally blocks once the bounded pipe is full; bore starts
# consuming it only after it has published the URL and a client claims it.
python3 - "$tmp/stdin-expected.bin" <<'PY'
import sys
payload = (b"stdin transfer-link payload\n" * 32768) + b"tail\n"
open(sys.argv[1], "wb").write(payload)
PY
cat "$tmp/stdin-expected.bin" | "$bin" transfer link --stdin \
    --filename stdin.bin --to "https://localhost:$control_port" \
    --ca-cert "$tmp/ca.pem" --max-downloads 1 \
    >"$tmp/stdin-url.txt" 2>"$tmp/stdin-link.log" &
stdin_pipeline_pid=$!
deadline=$((SECONDS + 20))
while [[ ! -s "$tmp/stdin-url.txt" ]] && (( SECONDS < deadline )); do
    if ! kill -0 "$stdin_pipeline_pid" 2>/dev/null; then
        cat "$tmp/stdin-link.log" >&2 || true
        printf 'stdin transfer-link exited before URL publication\n' >&2
        exit 1
    fi
    sleep 0.05
done
[[ -s "$tmp/stdin-url.txt" ]] || {
    cat "$tmp/stdin-link.log" >&2 || true
    printf 'timed out waiting for stdin URL\n' >&2
    exit 1
}
stdin_url=$(head -n 1 "$tmp/stdin-url.txt")
read -r stdin_host stdin_port <<<"$(python3 - "$stdin_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != 'https' or not u.hostname or not u.port:
    raise SystemExit('stdin URL has no explicit HTTPS host/port')
print(u.hostname, u.port)
PY
)"
stdin_resolve="$stdin_host:$stdin_port:127.0.0.1"
curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$stdin_resolve" "$stdin_url" -o "$tmp/stdin.bin"
cmp "$tmp/stdin-expected.bin" "$tmp/stdin.bin"
grep -q 'transfer-link download completed' "$tmp/stdin-link.log"
if curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$stdin_resolve" "$stdin_url" -o "$tmp/stdin-retry.bin"; then
    printf 'stdin transfer-link unexpectedly allowed a second GET\n' >&2
    exit 1
fi
kill -INT "$stdin_pipeline_pid"
wait "$stdin_pipeline_pid" 2>/dev/null || true
stdin_pipeline_pid=''

# A real consuming GET is interrupted while the external writer keeps stdin
# open.  The one-shot failure must terminate bore nonzero and close its stdin
# so the writer observes EPIPE/EOF; a URL-only SIGTERM would not exercise this
# contract because the producer starts only after GET claims the stream.
mkfifo "$tmp/stdin-cancel.fifo"
cat >"$tmp/stdin-cancel-producer.py" <<'PY'
import os
import sys
import time

fifo, marker = sys.argv[1:]
fd = os.open(fifo, os.O_WRONLY)
try:
    while True:
        os.write(fd, b"cancel-payload-" * 4096)
except (BrokenPipeError, OSError):
    with open(marker, "w", encoding="ascii") as output:
        output.write("closed\n")
finally:
    os.close(fd)
PY
python3 "$tmp/stdin-cancel-producer.py" "$tmp/stdin-cancel.fifo" \
    "$tmp/stdin-cancel-producer.status" &
stdin_cancel_producer_pid=$!
"$bin" transfer link --stdin \
    --filename cancelled.bin --to "https://localhost:$control_port" \
    --ca-cert "$tmp/ca.pem" --max-downloads 1 \
    <"$tmp/stdin-cancel.fifo" \
    >"$tmp/stdin-cancel-url.txt" 2>"$tmp/stdin-cancel.log" &
stdin_cancel_link_pid=$!
deadline=$((SECONDS + 20))
while [[ ! -s "$tmp/stdin-cancel-url.txt" ]] && (( SECONDS < deadline )); do
    if ! kill -0 "$stdin_cancel_link_pid" 2>/dev/null; then
        cat "$tmp/stdin-cancel.log" >&2 || true
        printf 'stdin cancellation transfer-link exited before URL publication\n' >&2
        exit 1
    fi
    sleep 0.05
done
[[ -s "$tmp/stdin-cancel-url.txt" ]] || {
    cat "$tmp/stdin-cancel.log" >&2 || true
    printf 'timed out waiting for stdin cancellation URL\n' >&2
    exit 1
}
stdin_cancel_url=$(head -n 1 "$tmp/stdin-cancel-url.txt")
read -r cancel_host cancel_port <<<"$(python3 - "$stdin_cancel_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != 'https' or not u.hostname or not u.port:
    raise SystemExit('cancel URL has no explicit HTTPS host/port')
print(u.hostname, u.port)
PY
)"
cancel_resolve="$cancel_host:$cancel_port:127.0.0.1"
curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$cancel_resolve" "$stdin_cancel_url" \
    -o "$tmp/stdin-cancel.bin" &
stdin_cancel_curl_pid=$!
deadline=$((SECONDS + 10))
while [[ ! -s "$tmp/stdin-cancel.bin" ]] && kill -0 "$stdin_cancel_curl_pid" 2>/dev/null && (( SECONDS < deadline )); do
    sleep 0.05
done
[[ -s "$tmp/stdin-cancel.bin" ]] || {
    printf 'cancel GET did not receive any payload before interruption\n' >&2
    exit 1
}
kill -TERM "$stdin_cancel_curl_pid" 2>/dev/null || true
wait "$stdin_cancel_curl_pid" 2>/dev/null || true
stdin_cancel_curl_pid=''
deadline=$((SECONDS + 7))
while kill -0 "$stdin_cancel_link_pid" 2>/dev/null && (( SECONDS < deadline )); do
    sleep 0.05
done
if kill -0 "$stdin_cancel_link_pid" 2>/dev/null; then
    printf 'stdin cancellation did not stop bore within 7 seconds\n' >&2
    exit 1
fi
set +e
wait "$stdin_cancel_link_pid"
stdin_cancel_status=$?
set -e
stdin_cancel_link_pid=''
(( stdin_cancel_status != 0 )) || {
    cat "$tmp/stdin-cancel.log" >&2 || true
    printf 'stdin cancellation unexpectedly returned success\n' >&2
    exit 1
}
deadline=$((SECONDS + 7))
while [[ ! -s "$tmp/stdin-cancel-producer.status" ]] && (( SECONDS < deadline )); do
    sleep 0.05
done
grep -qx 'closed' "$tmp/stdin-cancel-producer.status" || {
    printf 'external stdin producer did not observe closure\n' >&2
    exit 1
}
wait "$stdin_cancel_producer_pid" 2>/dev/null || true
stdin_cancel_producer_pid=''

printf 'transfer-link basic: PASS\n'
