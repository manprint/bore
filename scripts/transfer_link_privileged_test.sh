#!/usr/bin/env bash
set -Eeuo pipefail

# Root-only acceptance harness for transfer-link --exec.  It proves that the
# producer is actually started with the privileges of bore and that a GNU tar
# stream can be restored with its recorded ownership and permissions.

mode=${1:-all}
if [[ "$mode" != all && "$mode" != exec && "$mode" != cancel ]]; then
    printf 'usage: %s [all|exec|cancel]\n' "$0" >&2
    exit 2
fi
if (( EUID != 0 )); then
    printf 'this gate must be run as root (use sudo -n %s %s)\n' "$0" "$mode" >&2
    exit 2
fi

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
bin="$repo/target/debug/bore"
[[ -x "$bin" ]] || {
    printf 'missing debug binary: %s\n' "$bin" >&2
    exit 2
}
for tool in openssl curl python3 tar sha256sum pgrep; do
    command -v "$tool" >/dev/null || {
        printf 'missing prerequisite: %s\n' "$tool" >&2
        exit 2
    }
done

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-transfer-link-root.XXXXXX")
server_pid=''
link_pid=''
cancel_pid=''
curl_pid=''
cleanup() {
    local status=$?
    for pid in "$curl_pid" "$cancel_pid" "$link_pid" "$server_pid"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
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

wait_url() {
    local output=$1 log=$2 pid=$3
    local deadline=$((SECONDS + 20))
    while [[ ! -s "$output" ]] && (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            cat "$log" >&2 || true
            printf 'transfer-link exited before URL publication\n' >&2
            return 1
        fi
        sleep 0.05
    done
    [[ -s "$output" ]] || {
        cat "$log" >&2 || true
        printf 'timed out waiting for transfer-link URL\n' >&2
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
CN = bore privileged transfer-link test
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
CN = bore privileged transfer-link test server
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

"$bin" server \
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

fixture="$tmp/fixture"
mkdir -p "$fixture/private-dir/nested" "$fixture/sticky-dir"
printf 'root-only\n' >"$fixture/root-only"
printf 'metadata\n' >"$fixture/metadata.bin"
printf '#!/bin/sh\nprintf executable\\n\n' >"$fixture/executable.sh"
printf 'setgid and mode\n' >"$fixture/setgid.txt"
printf 'nested\n' >"$fixture/private-dir/nested/file.txt"
printf 'hardlink\n' >"$fixture/hardlink-source"
ln "$fixture/hardlink-source" "$fixture/hardlink-copy"
ln -s 'metadata.bin' "$fixture/metadata-link"
chmod 0600 "$fixture/root-only"
chmod 0640 "$fixture/metadata.bin"
chmod 0755 "$fixture/executable.sh"
chmod 06750 "$fixture/setgid.txt"
chmod 02750 "$fixture/private-dir"
chmod 01777 "$fixture/sticky-dir"
chown 12345:23456 "$fixture/metadata.bin" "$fixture/setgid.txt"
chown 12345:23456 "$fixture/private-dir"
chown 12345:23456 "$fixture/sticky-dir"

if [[ "$mode" == all || "$mode" == exec ]]; then
    "$bin" transfer link --filename backup.tar --max-downloads 1 \
        --to "https://localhost:$control_port" --ca-cert "$tmp/ca.pem" \
        --exec -- tar -cpf - -C "$tmp" "$(basename "$fixture")" \
        >"$tmp/url.txt" 2>"$tmp/link.log" &
    link_pid=$!
    wait_url "$tmp/url.txt" "$tmp/link.log" "$link_pid"
    url=$(head -n 1 "$tmp/url.txt")
    read -r host port <<<"$(python3 - "$url" <<'PY'
from urllib.parse import urlparse
import sys
parsed = urlparse(sys.argv[1])
if parsed.scheme != 'https' or not parsed.hostname or not parsed.port:
    raise SystemExit('invalid announced URL')
print(parsed.hostname, parsed.port)
PY
)"
    resolve="$host:$port:127.0.0.1"
    set +e
    curl --fail --silent --show-error --cacert "$tmp/ca.pem" \
        --resolve "$resolve" "$url" -o "$tmp/backup.tar"
    curl_status=$?
    set -e
    (( curl_status == 0 )) || {
        cat "$tmp/link.log" >&2 || true
        printf 'root tar download failed: curl=%s\n' "$curl_status" >&2
        exit 1
    }
    grep -q 'transfer-link download completed' "$tmp/link.log" || {
        cat "$tmp/link.log" >&2
        printf 'root tar download had no completion record\n' >&2
        exit 1
    }
    kill -INT "$link_pid"
    deadline=$((SECONDS + 10))
    while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < deadline )); do
        sleep 0.05
    done
    if kill -0 "$link_pid" 2>/dev/null; then
        printf 'root tar transfer-link client did not stop after SIGINT\n' >&2
        exit 1
    fi
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
    sha256sum "$tmp/backup.tar" >"$tmp/backup.sha256"
    mkdir "$tmp/restore"
    (umask 0077; tar --numeric-owner --same-owner -xpf "$tmp/backup.tar" -C "$tmp/restore")
    python3 - "$fixture" "$tmp/restore/$(basename "$fixture")" <<'PY'
import os, stat, sys
source, restored = sys.argv[1:]
paths = [
    'root-only', 'metadata.bin', 'executable.sh', 'setgid.txt',
    'private-dir', 'private-dir/nested/file.txt', 'sticky-dir',
    'hardlink-source', 'hardlink-copy', 'metadata-link',
]
for rel in paths:
    left = os.path.join(source, rel)
    right = os.path.join(restored, rel)
    ls, rs = os.lstat(left), os.lstat(right)
    assert stat.S_IFMT(ls.st_mode) == stat.S_IFMT(rs.st_mode), rel
    assert stat.S_IMODE(ls.st_mode) == stat.S_IMODE(rs.st_mode), (rel, oct(ls.st_mode), oct(rs.st_mode))
    assert ls.st_uid == rs.st_uid and ls.st_gid == rs.st_gid, (rel, ls.st_uid, ls.st_gid, rs.st_uid, rs.st_gid)
    if stat.S_ISLNK(ls.st_mode):
        assert os.readlink(left) == os.readlink(right), rel
    elif stat.S_ISREG(ls.st_mode):
        assert open(left, 'rb').read() == open(right, 'rb').read(), rel
assert os.stat(os.path.join(restored, 'hardlink-source')).st_ino == os.stat(os.path.join(restored, 'hardlink-copy')).st_ino
PY
    printf 'transfer-link privileged exec: PASS\n'

    "$bin" transfer link --filename failed.tar --max-downloads 1 \
        --to "https://localhost:$control_port" --ca-cert "$tmp/ca.pem" \
        --exec -- sh -c 'printf partial; exit 2' \
        >"$tmp/failure-url.txt" 2>"$tmp/failure.log" &
    link_pid=$!
    wait_url "$tmp/failure-url.txt" "$tmp/failure.log" "$link_pid"
    failure_url=$(head -n 1 "$tmp/failure-url.txt")
    read -r failure_host failure_port <<<"$(python3 - "$failure_url" <<'PY'
from urllib.parse import urlparse
import sys
parsed = urlparse(sys.argv[1])
print(parsed.hostname, parsed.port)
PY
)"
    set +e
    curl --fail --silent --show-error --max-time 15 --cacert "$tmp/ca.pem" \
        --resolve "$failure_host:$failure_port:127.0.0.1" "$failure_url" \
        -o "$tmp/failed.tar"
    failure_status=$?
    set -e
    (( failure_status != 0 )) || {
        printf 'failed exec unexpectedly produced a successful HTTP body\n' >&2
        exit 1
    }
    if grep -q 'transfer-link download completed' "$tmp/failure.log"; then
        printf 'failed exec was logged as completed\n' >&2
        exit 1
    fi
    kill -INT "$link_pid"
    deadline=$((SECONDS + 10))
    while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < deadline )); do
        sleep 0.05
    done
    wait "$link_pid" 2>/dev/null || true
    link_pid=''
    printf 'transfer-link privileged exec failure: PASS\n'
fi

if [[ "$mode" == all || "$mode" == cancel ]]; then
    "$bin" transfer link --filename cancelled.tar --max-downloads 1 \
        --to "https://localhost:$control_port" --ca-cert "$tmp/ca.pem" \
        --exec -- sh -c 'trap "" TERM; sleep 30 & wait' \
        >"$tmp/cancel-url.txt" 2>"$tmp/cancel.log" &
    cancel_pid=$!
    wait_url "$tmp/cancel-url.txt" "$tmp/cancel.log" "$cancel_pid"
    cancel_url=$(head -n 1 "$tmp/cancel-url.txt")
    read -r cancel_host cancel_port <<<"$(python3 - "$cancel_url" <<'PY'
from urllib.parse import urlparse
import sys
parsed = urlparse(sys.argv[1])
print(parsed.hostname, parsed.port)
PY
)"
    set +e
    curl --silent --show-error --cacert "$tmp/ca.pem" \
        --resolve "$cancel_host:$cancel_port:127.0.0.1" "$cancel_url" \
        -o "$tmp/cancelled.tar" &
    curl_pid=$!
    set -e
    child_pid=''
    deadline=$((SECONDS + 10))
    while (( SECONDS < deadline )); do
        child_pid=$(pgrep -P "$cancel_pid" | head -n 1 || true)
        [[ -n "$child_pid" ]] && break
        sleep 0.05
    done
    [[ -n "$child_pid" ]] || {
        printf 'exec cancellation test did not observe a child\n' >&2
        exit 1
    }
    kill -TERM "$cancel_pid"
    wait "$cancel_pid" 2>/dev/null || true
    cancel_pid=''
    wait "$curl_pid" 2>/dev/null || true
    curl_pid=''
    deadline=$((SECONDS + 7))
    while kill -0 "$child_pid" 2>/dev/null && (( SECONDS < deadline )); do
        sleep 0.05
    done
    if kill -0 "$child_pid" 2>/dev/null; then
        printf 'exec cancellation left child pid %s alive\n' "$child_pid" >&2
        exit 1
    fi
    printf 'transfer-link privileged exec cancellation: PASS\n'
fi

printf 'transfer-link privileged: PASS (%s)\n' "$mode"
