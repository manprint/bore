#!/usr/bin/env bash
set -Eeuo pipefail

# Real Docker acceptance test.  The client image is built from the repository's
# docker/Dockerfile.client; its base image is a locally tagged scratch image
# containing the current static binary, so the test never silently exercises an
# older registry tag.

for tool in docker openssl curl python3; do
    command -v "$tool" >/dev/null || {
        printf 'missing prerequisite: %s\n' "$tool" >&2
        exit 2
    }
done
docker info >/dev/null 2>&1 || {
    printf 'Docker daemon is unavailable\n' >&2
    exit 2
}

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
static_bin="$repo/target/x86_64-unknown-linux-gnu/debug/bore"
if [[ ! -x "$static_bin" ]] || ! file "$static_bin" 2>/dev/null | grep -q 'static'; then
    RUSTFLAGS='-C target-feature=+crt-static' \
        cargo build --offline --locked --all-features \
            --target x86_64-unknown-linux-gnu --quiet
fi
[[ -x "$static_bin" ]] || {
    printf 'static binary was not built: %s\n' "$static_bin" >&2
    exit 2
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-transfer-link-docker.XXXXXX")
server_pid=''
raw_pid=''
stdin_pid=''
exec_pid=''
base_image="bore-transfer-link-base-$$"
client_image="bore-transfer-link-client-$$"
cleanup() {
    local status=$?
    for pid in "$raw_pid" "$stdin_pid" "$exec_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    docker image rm -f "$client_image" "$base_image" >/dev/null 2>&1 || true
    rm -rf -- "$tmp"
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
    local deadline=$((SECONDS + 20))
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

wait_url() {
    local output=$1 log=$2 pid=$3
    local deadline=$((SECONDS + 25))
    while [[ ! -s "$output" ]] && (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            cat "$log" >&2 || true
            return 1
        fi
        sleep 0.05
    done
    [[ -s "$output" ]] || {
        cat "$log" >&2 || true
        printf 'timed out waiting for container URL\n' >&2
        return 1
    }
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
CN = bore Docker transfer-link test
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
CN = bore Docker transfer-link test server
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
openssl req -newkey rsa:2048 -nodes \
    -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" \
    -config "$tmp/leaf.cnf" >/dev/null 2>&1
openssl x509 -req -days 1 -sha256 \
    -in "$tmp/leaf.csr" -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" \
    -CAcreateserial -out "$tmp/leaf.pem" \
    -extfile "$tmp/leaf.cnf" -extensions leaf_ext >/dev/null 2>&1

mkdir "$tmp/base-context"
cp "$static_bin" "$tmp/base-context/bore"
cat >"$tmp/base.Dockerfile" <<'EOF'
FROM scratch
COPY bore /bore
ENTRYPOINT ["/bore"]
EOF
docker build --quiet -f "$tmp/base.Dockerfile" -t "$base_image" "$tmp/base-context" >/dev/null
docker build --quiet -f "$repo/docker/Dockerfile.client" \
    --build-arg "BORE_IMAGE=$base_image" -t "$client_image" "$repo" >/dev/null

printf 'docker raw payload\n' >"$tmp/source.bin"
mkdir "$tmp/mixed-root"
printf 'nested Docker payload\0\xff\n' >"$tmp/mixed-root/nested.bin"

"$repo/target/x86_64-unknown-linux-gnu/debug/bore" server \
    --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port "$control_port" \
    --cert-file "$tmp/leaf.pem" --key-file "$tmp/leaf.key" \
    --vhost-base-domain bore.local --vhost-mode https \
    --vhost-http-port "$http_port" --vhost-https-port "$https_port" \
    --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
    --vhost-quic-port "$quic_port" --udp \
    >"$tmp/server.log" 2>&1 &
server_pid=$!
wait_tcp "$control_port"

# The scratch client needs no shell, tar or helper binary.  --workdir/-w and a
# readonly bind mount are the same shape users get for ordinary source files.
docker run --rm --network host --workdir /dir \
    -v "$tmp:/dir:ro" "$client_image" \
    transfer link /dir/source.bin --to "https://localhost:$control_port" \
    --ca-cert /dir/ca.pem --max-downloads 1 \
    >"$tmp/raw-url.txt" 2>"$tmp/raw.log" &
raw_pid=$!
wait_url "$tmp/raw-url.txt" "$tmp/raw.log" "$raw_pid"
raw_url=$(head -n 1 "$tmp/raw-url.txt")
read -r raw_host raw_port <<<"$(python3 - "$raw_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
print(u.hostname, u.port)
PY
)"
curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$raw_host:$raw_port:127.0.0.1" "$raw_url" \
    -o "$tmp/raw.bin"
cmp "$tmp/source.bin" "$tmp/raw.bin"
kill -INT "$raw_pid"
wait "$raw_pid" 2>/dev/null || true
raw_pid=''

# A host producer is piped through docker -i.  There is intentionally no -t:
# a pseudo-TTY would translate/control bytes and corrupt a binary tar stream.
python3 - "$tmp/stdin.expected" <<'PY'
import sys
payload = b"\x00\x0a\x0d\xff" * 65536
open(sys.argv[1], "wb").write(payload)
PY
cat "$tmp/stdin.expected" | docker run --rm -i --network host --workdir /dir \
    -v "$tmp:/dir:ro" "$client_image" \
    transfer link --stdin --filename stdin.bin --to "https://localhost:$control_port" \
    --ca-cert /dir/ca.pem --max-downloads 1 \
    >"$tmp/stdin-url.txt" 2>"$tmp/stdin.log" &
stdin_pid=$!
wait_url "$tmp/stdin-url.txt" "$tmp/stdin.log" "$stdin_pid"
stdin_url=$(head -n 1 "$tmp/stdin-url.txt")
read -r stdin_host stdin_port <<<"$(python3 - "$stdin_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
print(u.hostname, u.port)
PY
)"
curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
    --resolve "$stdin_host:$stdin_port:127.0.0.1" "$stdin_url" \
    -o "$tmp/stdin.bin"
cmp "$tmp/stdin.expected" "$tmp/stdin.bin"
kill -INT "$stdin_pid"
wait "$stdin_pid" 2>/dev/null || true
stdin_pid=''

# The scratch image has no /bin/sh or tar.  Exec must fail as a producer error,
# never emit a successful terminator, and never hang the container.
docker run --rm --network host --workdir /dir \
    -v "$tmp:/dir:ro" "$client_image" \
    transfer link --exec --filename failed.tar --to "https://localhost:$control_port" \
    --ca-cert /dir/ca.pem --max-downloads 1 -- tar -cpf - /dir/source.bin \
    >"$tmp/exec-url.txt" 2>"$tmp/exec.log" &
exec_pid=$!
wait_url "$tmp/exec-url.txt" "$tmp/exec.log" "$exec_pid"
exec_url=$(head -n 1 "$tmp/exec-url.txt")
read -r exec_host exec_port <<<"$(python3 - "$exec_url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
print(u.hostname, u.port)
PY
)"
set +e
curl --fail --silent --show-error --max-time 15 --cacert "$tmp/ca.pem" \
    --resolve "$exec_host:$exec_port:127.0.0.1" "$exec_url" -o "$tmp/failed.tar"
exec_status=$?
set -e
(( exec_status != 0 )) || {
    printf 'scratch --exec tar unexpectedly succeeded\n' >&2
    exit 1
}
if grep -q 'transfer-link download completed' "$tmp/exec.log"; then
    printf 'scratch --exec failure was logged as completed\n' >&2
    exit 1
fi
kill -INT "$exec_pid"
wait "$exec_pid" 2>/dev/null || true
exec_pid=''

printf 'transfer-link Docker: PASS (raw, stdin -i, scratch exec failure)\n'
