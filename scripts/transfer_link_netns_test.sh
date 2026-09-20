#!/usr/bin/env bash
# Transfer-link transport, fault and lifecycle gate.
#
# This is deliberately a real three-party test.  The sender and downloader are
# in separate namespaces, the server is in a third namespace, and UDP faults
# are installed only in the sender namespace.  Every object created by this
# script is named with the shell PID and is removed by the EXIT trap.
set -Eeuo pipefail

if (( EUID != 0 )); then
    printf 'transfer-link netns gate must run as root (use sudo -n)\n' >&2
    exit 2
fi

for tool in cargo curl file ip nft openssl python3 sha256sum ss timeout; do
    command -v "$tool" >/dev/null || {
        printf 'missing prerequisite: %s\n' "$tool" >&2
        exit 2
    }
done

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BORE="${BORE:-$repo/target/release/bore}"
if [[ ! -x "$BORE" ]]; then
    cargo build --offline --locked --all-features --release --quiet
fi
[[ -x "$BORE" ]] || { printf 'missing bore binary: %s\n' "$BORE" >&2; exit 2; }
if [[ "${BORE_TRANSFER_LINK_ALLOW_STALE:-0}" != 1 ]] &&
    find "$repo/src" "$repo/Cargo.toml" -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    printf 'bore binary is older than the sources: %s\n' "$BORE" >&2
    printf 'build the current release binary or set BORE_TRANSFER_LINK_ALLOW_STALE=1 only for local diagnosis\n' >&2
    exit 2
fi

run_id=$$
ns_server="btl-s-$run_id"
ns_sender="btl-a-$run_id"
ns_receiver="btl-b-$run_id"
table="btl$run_id"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-transfer-link-netns.XXXXXX")
server_pid=''
link_pid=''
curl_pid=''
producer_pid=''
fifo=''
current_log=''
current_url_file=''
current_url=''
current_resolve=''
server_log="$tmp/server.log"

pass_count=0
fail_count=0
pass() { printf 'PASS %s\n' "$*"; pass_count=$((pass_count + 1)); }
fail() { printf 'FAIL %s\n' "$*" >&2; fail_count=$((fail_count + 1)); }

kill_wait() {
    local pid=${1:-} deadline
    [[ -n "$pid" ]] || return 0
    if kill -0 "$pid" 2>/dev/null; then
        kill -TERM "$pid" 2>/dev/null || true
        deadline=$((SECONDS + 8))
        while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.05; done
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
}

clear_fault() {
    ip netns exec "$ns_sender" nft delete table inet "$table" 2>/dev/null || true
}

install_udp_fault() {
    clear_fault
    ip netns exec "$ns_sender" nft add table inet "$table"
    ip netns exec "$ns_sender" nft add chain inet "$table" output \
        '{ type filter hook output priority 0; policy accept; }'
    ip netns exec "$ns_sender" nft add rule inet "$table" output \
        ip daddr "$server_ip" udp dport "$quic_port" counter drop
    ip netns exec "$ns_sender" nft add rule inet "$table" output \
        ip daddr "$receiver_server_ip" udp dport "$quic_port" counter drop
}

cleanup() {
    local status=$?
    set +e
    [[ -n "$curl_pid" ]] && kill_wait "$curl_pid"
    [[ -n "$link_pid" ]] && kill_wait "$link_pid"
    [[ -n "$producer_pid" ]] && kill_wait "$producer_pid"
    clear_fault
    for ns in "$ns_sender" "$ns_receiver" "$ns_server"; do
        ip netns pids "$ns" 2>/dev/null | while read -r pid; do
            if [[ -n "$pid" ]]; then
                kill -TERM "$pid" 2>/dev/null || true
            fi
        done
        ip netns del "$ns" 2>/dev/null || true
    done
    if [[ "${BORE_TRANSFER_LINK_KEEP_ARTIFACTS:-0}" == 1 ]]; then
        printf 'transfer-link netns artifacts: %s\n' "$tmp" >&2
    else
        rm -rf -- "$tmp"
    fi
    if (( status == 0 && fail_count != 0 )); then status=1; fi
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
    local ip=$1 port=$2 deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if ip netns exec "$ns_sender" python3 - "$ip" "$port" <<'PY'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.settimeout(0.25)
try:
    s.connect((sys.argv[1], int(sys.argv[2])))
except OSError:
    raise SystemExit(1)
finally:
    s.close()
PY
        then return 0; fi
        sleep 0.05
    done
    return 1
}

wait_file() {
    local path=$1 pid=$2 description=$3 deadline=$((SECONDS + 25))
    while [[ ! -s "$path" ]] && (( SECONDS < deadline )); do
        if ! kill -0 "$pid" 2>/dev/null; then
            printf '%s exited before publication\n' "$description" >&2
            if [[ -f "$current_log" ]]; then
                tail -n 80 "$current_log" >&2 || true
            fi
            return 1
        fi
        sleep 0.05
    done
    [[ -s "$path" ]]
}

wait_log() {
    local path=$1 pattern=$2
    local seconds=${3:-25}
    local deadline=$((SECONDS + seconds))
    while (( SECONDS < deadline )); do
        grep -E -q "$pattern" "$path" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

parse_url() {
    local url=$1
    read -r current_host current_port <<<"$(python3 - "$url" <<'PY'
from urllib.parse import urlparse
import sys
u = urlparse(sys.argv[1])
if u.scheme != "https" or not u.hostname or not u.port:
    raise SystemExit("URL is not an explicit HTTPS URL")
print(u.hostname, u.port)
PY
)"
    # B is on its own point-to-point link, so it reaches the server through
    # the server-side address of that link.  A uses server_ip for control and
    # the direct QUIC path.
    current_resolve="$current_host:$current_port:$receiver_server_ip"
}

start_server() {
    RUST_LOG="${RUST_LOG:-info}" ip netns exec "$ns_server" "$BORE" server \
        --bind-addr 0.0.0.0 --bind-tunnels 0.0.0.0 \
        --control-port "$control_port" --secret "$secret" \
        --cert-file "$tmp/leaf.pem" --key-file "$tmp/leaf.key" \
        --vhost-base-domain bore.local --vhost-mode https \
        --vhost-http-port "$http_port" --vhost-https-port "$https_port" \
        --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
        --vhost-quic-port "$quic_port" --udp \
        >>"$server_log" 2>&1 &
    server_pid=$!
    wait_tcp "$server_ip" "$control_port" || {
        printf 'server did not open control port\n' >&2
        tail -n 100 "$server_log" >&2 || true
        return 1
    }
}

start_link() {
    local name=$1 source=$2; shift 2
    current_url_file="$tmp/$name.url"
    current_log="$tmp/$name.log"
    : >"$current_url_file"
    : >"$current_log"
    RUST_LOG="${RUST_LOG:-info}" \
        BORE_DIRECT_QUIC_IDLE_MS=2000 BORE_DIRECT_QUIC_KEEPALIVE_MS=200 \
        ip netns exec "$ns_sender" "$BORE" -v transfer link "$source" \
        --to "https://$server_ip:$control_port" --secret "$secret" \
        --ca-cert "$tmp/ca.pem" "$@" \
        >"$current_url_file" 2>"$current_log" &
    link_pid=$!
    wait_file "$current_url_file" "$link_pid" "$name" || return 1
    current_url=$(head -n 1 "$current_url_file")
    parse_url "$current_url"
}

start_stdin_link() {
    local name=$1; shift
    current_url_file="$tmp/$name.url"
    current_log="$tmp/$name.log"
    : >"$current_url_file"
    : >"$current_log"
    fifo="$tmp/$name.fifo"
    mkfifo "$fifo"
    python3 - "$fifo" "$tmp/stream.bin" <<'PY' &
import os, sys, time
fifo, source = sys.argv[1:]
with open(fifo, "wb", buffering=0) as out, open(source, "rb") as inp:
    while True:
        block = inp.read(64 * 1024)
        if not block:
            break
        out.write(block)
        time.sleep(0.01)
PY
    producer_pid=$!
    RUST_LOG="${RUST_LOG:-info}" \
        BORE_DIRECT_QUIC_IDLE_MS=2000 BORE_DIRECT_QUIC_KEEPALIVE_MS=200 \
        ip netns exec "$ns_sender" "$BORE" -v transfer link --stdin "$@" \
        --to "https://$server_ip:$control_port" --secret "$secret" \
        --ca-cert "$tmp/ca.pem" \
        <"$fifo" >"$current_url_file" 2>"$current_log" &
    link_pid=$!
    wait_file "$current_url_file" "$link_pid" "$name" || return 1
    current_url=$(head -n 1 "$current_url_file")
    parse_url "$current_url"
}

start_exec_link() {
    local name=$1; shift
    local separator=0 arg
    local -a options=() command=()
    for arg in "$@"; do
        if [[ "$arg" == "--" && $separator == 0 ]]; then
            separator=1
            continue
        fi
        if (( separator == 0 )); then options+=("$arg"); else command+=("$arg"); fi
    done
    (( separator == 1 && ${#command[@]} > 0 )) || {
        printf 'exec test helper requires -- followed by a producer command\n' >&2
        return 2
    }
    current_url_file="$tmp/$name.url"
    current_log="$tmp/$name.log"
    : >"$current_url_file"
    : >"$current_log"
    RUST_LOG="${RUST_LOG:-info}" \
        BORE_DIRECT_QUIC_IDLE_MS=2000 BORE_DIRECT_QUIC_KEEPALIVE_MS=200 \
        ip netns exec "$ns_sender" "$BORE" -v transfer link --exec "${options[@]}" \
        --to "https://$server_ip:$control_port" --secret "$secret" \
        --ca-cert "$tmp/ca.pem" -- "${command[@]}" \
        >"$current_url_file" 2>"$current_log" &
    link_pid=$!
    wait_file "$current_url_file" "$link_pid" "$name" || return 1
    current_url=$(head -n 1 "$current_url_file")
    parse_url "$current_url"
}

curl_download() {
    local output=$1; shift
    ip netns exec "$ns_receiver" curl --fail --silent --show-error \
        --connect-timeout 5 --max-time 90 --cacert "$tmp/ca.pem" \
        --resolve "$current_resolve" "$current_url" -o "$output" "$@"
}

assert_hash() {
    local expected=$1 actual=$2
    [[ "$(sha256sum "$expected" | awk '{print $1}')" == "$(sha256sum "$actual" | awk '{print $1}')" ]]
}

stop_link() {
    local pid=$link_pid deadline
    [[ -n "$pid" ]] || return 0
    if kill -0 "$pid" 2>/dev/null; then
        kill -INT "$pid" 2>/dev/null || true
        deadline=$((SECONDS + 15))
        while kill -0 "$pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.05; done
        kill -TERM "$pid" 2>/dev/null || true
        kill -KILL "$pid" 2>/dev/null || true
    fi
    wait "$pid" 2>/dev/null || true
    link_pid=''
}

printf 'T-LINK netns: setup\n'
server_ip=10.250.0.1
sender_ip=10.250.0.2
receiver_server_ip=10.250.0.5
receiver_ip=10.250.0.6
secret="btl-$run_id"
control_port=$(free_port)
http_port=$(free_port)
https_port=$(free_port)
quic_port=$(free_port)

ip netns add "$ns_server"
ip netns add "$ns_sender"
ip netns add "$ns_receiver"
ip link add "vsa$run_id" type veth peer name "vas$run_id"
ip link set "vsa$run_id" netns "$ns_server"
ip link set "vas$run_id" netns "$ns_sender"
ip link add "vsb$run_id" type veth peer name "vbs$run_id"
ip link set "vsb$run_id" netns "$ns_server"
ip link set "vbs$run_id" netns "$ns_receiver"
ip netns exec "$ns_server" ip addr add "$server_ip/30" dev "vsa$run_id"
ip netns exec "$ns_sender" ip addr add "$sender_ip/30" dev "vas$run_id"
ip netns exec "$ns_server" ip addr add "$receiver_server_ip/30" dev "vsb$run_id"
ip netns exec "$ns_receiver" ip addr add "$receiver_ip/30" dev "vbs$run_id"
for ns in "$ns_server" "$ns_sender" "$ns_receiver"; do ip netns exec "$ns" ip link set lo up; done
ip netns exec "$ns_server" ip link set "vsa$run_id" up
ip netns exec "$ns_sender" ip link set "vas$run_id" up
ip netns exec "$ns_server" ip link set "vsb$run_id" up
ip netns exec "$ns_receiver" ip link set "vbs$run_id" up

cat >"$tmp/ca.cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bore transfer-link netns CA
[ca_ext]
basicConstraints = critical,CA:TRUE
keyUsage = critical,keyCertSign,cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
EOF
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$tmp/ca.key" -out "$tmp/ca.pem" \
    -config "$tmp/ca.cnf" -extensions ca_ext >/dev/null 2>&1
cat >"$tmp/leaf.cnf" <<EOF
[req]
distinguished_name = dn
req_extensions = req_ext
prompt = no
[dn]
CN = bore transfer-link netns server
[req_ext]
subjectAltName = DNS:bore.local,DNS:*.bore.local,IP:$server_ip,IP:$receiver_server_ip
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:bore.local,DNS:*.bore.local,IP:$server_ip,IP:$receiver_server_ip
authorityKeyIdentifier = keyid,issuer
subjectKeyIdentifier = hash
EOF
openssl req -newkey rsa:2048 -nodes -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" \
    -config "$tmp/leaf.cnf" >/dev/null 2>&1
openssl x509 -req -days 1 -sha256 -in "$tmp/leaf.csr" \
    -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" -CAcreateserial \
    -out "$tmp/leaf.pem" -extfile "$tmp/leaf.cnf" -extensions leaf_ext >/dev/null 2>&1

printf 'T-LINK netns: start server\n'
start_server
udp_view=$(ip netns exec "$ns_server" ss -uapm 2>/dev/null | grep -E ":$quic_port([[:space:]]|$)" || true)
[[ -n "$udp_view" ]] || { printf 'no server UDP socket visible in ss\n' >&2; exit 1; }
printf 'T-LINK-UDP-BUFFER %s\n' "$udp_view"

printf 'T-LINK-QUIC direct\n'
printf 'direct payload\n' >"$tmp/direct.bin"
start_link direct "$tmp/direct.bin" --filename direct.bin --max-downloads 2
curl_download "$tmp/direct.out"
assert_hash "$tmp/direct.bin" "$tmp/direct.out"
wait_log "$current_log" 'DirectQuic' 10 || { tail -n 100 "$current_log" >&2; exit 1; }
pass 'T-LINK-QUIC direct path and SHA-256'
stop_link

printf 'T-LINK-FALLBACK UDP blocked before connect (carriers=1)\n'
install_udp_fault
start_link fallback1 "$tmp/direct.bin" --filename fallback.bin --carriers 1 --max-downloads 2
curl_download "$tmp/fallback1.out"
assert_hash "$tmp/direct.bin" "$tmp/fallback1.out"
wait_log "$current_log" 'RelayTcp' 10 || { tail -n 100 "$current_log" >&2; exit 1; }
pass 'T-LINK-FALLBACK automatic relay carriers=1'
stop_link

printf 'T-LINK-FALLBACK UDP blocked before connect (carriers=2)\n'
start_link fallback2 "$tmp/direct.bin" --filename fallback2.bin --carriers 2 --max-downloads 2
curl_download "$tmp/fallback2.out"
assert_hash "$tmp/direct.bin" "$tmp/fallback2.out"
wait_log "$current_log" 'RelayTcp' 10 || { tail -n 100 "$current_log" >&2; exit 1; }
pass 'T-LINK-FALLBACK automatic relay carriers=2'
stop_link
clear_fault

printf 'T-LINK-FALLBACK explicit relay-only\n'
start_link relay_only "$tmp/direct.bin" --filename relay.bin --relay-only --max-downloads 2
curl_download "$tmp/relay.out"
assert_hash "$tmp/direct.bin" "$tmp/relay.out"
wait_log "$current_log" 'RelayTcp' 10 || { tail -n 100 "$current_log" >&2; exit 1; }
pass 'T-LINK-FALLBACK relay-only path'
stop_link

printf 'T-LINK-DROP QUIC body has no migration\n'
dd if=/dev/zero of="$tmp/drop.bin" bs=1M count=16 status=none
start_link drop "$tmp/drop.bin" --filename drop.bin --max-downloads 2
ip netns exec "$ns_receiver" curl --fail --silent --show-error --limit-rate 64k \
    --connect-timeout 5 --max-time 90 --cacert "$tmp/ca.pem" \
    --resolve "$current_resolve" "$current_url" -o "$tmp/drop.out" &
curl_pid=$!
wait_log "$current_log" 'DirectQuic' 15 || { tail -n 100 "$current_log" >&2; exit 1; }
install_udp_fault
drop_deadline=$((SECONDS + 45))
while kill -0 "$curl_pid" 2>/dev/null && (( SECONDS < drop_deadline )); do sleep 0.1; done
if kill -0 "$curl_pid" 2>/dev/null; then
    kill_wait "$curl_pid"
    printf 'QUIC download did not fail within 45 seconds\n' >&2
    exit 1
fi
set +e
wait "$curl_pid"
drop_rc=$?
set -e
curl_pid=''
(( drop_rc != 0 )) || { printf 'QUIC download unexpectedly completed after UDP drop\n' >&2; exit 1; }
if grep -q 'transfer-link download completed' "$current_log"; then
    printf 'dropped download was falsely marked completed\n' >&2
    exit 1
fi
pass 'T-LINK-DROP active HTTP stream fails without QUIC-to-TCP migration'

curl_download "$tmp/drop-retry.out"
assert_hash "$tmp/drop.bin" "$tmp/drop-retry.out"
wait_log "$current_log" 'RelayTcp' 15 || { tail -n 120 "$current_log" >&2; exit 1; }
pass 'T-LINK-DROP subsequent GET succeeds through relay with exact hash'
stop_link
clear_fault

printf 'T-LINK-RECONNECT same URL after server restart\n'
printf 'reconnect payload\n' >"$tmp/reconnect.bin"
start_link reconnect "$tmp/reconnect.bin" --filename reconnect.bin --max-downloads 2
old_url=$current_url
kill_wait "$server_pid"
server_pid=''
if curl_download "$tmp/reconnect-down.out" 2>/dev/null; then
    printf 'download unexpectedly succeeded while server was stopped\n' >&2
    exit 1
fi
start_server
reconnect_deadline=$((SECONDS + 35))
while (( SECONDS < reconnect_deadline )); do
    ready_count=$(grep -c 'transfer-link vhost ready' "$current_log" 2>/dev/null || true)
    (( ready_count >= 2 )) && break
    sleep 0.1
done
[[ "$current_url" == "$old_url" ]] || { printf 'URL changed across reconnect\n' >&2; exit 1; }
(( ready_count >= 2 )) || { tail -n 120 "$current_log" >&2; exit 1; }
curl_download "$tmp/reconnect.out"
assert_hash "$tmp/reconnect.bin" "$tmp/reconnect.out"
pass 'T-LINK-RECONNECT same URL and successful post-reconnect download'
stop_link

printf 'T-LINK-EXEC-CANCEL and stdin one-shot after UDP fault\n'
dd if=/dev/zero of="$tmp/stream.bin" bs=1M count=8 status=none
install_udp_fault
start_stdin_link stdin_cancel --filename stdin.bin --max-downloads 1
ip netns exec "$ns_receiver" curl --fail --silent --show-error --limit-rate 64k \
    --connect-timeout 5 --max-time 90 --cacert "$tmp/ca.pem" \
    --resolve "$current_resolve" "$current_url" -o "$tmp/stdin.out" &
curl_pid=$!
wait_log "$current_log" 'DirectQuic' 15 || { tail -n 100 "$current_log" >&2; exit 1; }
install_udp_fault
stdin_deadline=$((SECONDS + 45))
while kill -0 "$curl_pid" 2>/dev/null && (( SECONDS < stdin_deadline )); do sleep 0.1; done
if kill -0 "$curl_pid" 2>/dev/null; then
    kill_wait "$curl_pid"
    printf 'stdin download did not fail within 45 seconds\n' >&2
    exit 1
fi
set +e; wait "$curl_pid"; stdin_curl_rc=$?; set -e
curl_pid=''
(( stdin_curl_rc != 0 )) || { printf 'stdin download unexpectedly completed after UDP drop\n' >&2; exit 1; }
stdin_wait=$((SECONDS + 20))
while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < stdin_wait )); do sleep 0.1; done
if kill -0 "$link_pid" 2>/dev/null; then
    printf 'stdin one-shot client remained alive after failed GET\n' >&2
    exit 1
fi
set +e; wait "$link_pid"; stdin_rc=$?; set -e
link_pid=''
(( stdin_rc != 0 )) || { printf 'stdin one-shot client returned success after fault\n' >&2; exit 1; }
set +e
stdin_retry_code=$(ip netns exec "$ns_receiver" curl --silent --show-error --max-time 10 \
    --cacert "$tmp/ca.pem" --resolve "$current_resolve" \
    -o /dev/null -w '%{http_code}' "$current_url")
stdin_retry_rc=$?
set -e
if (( stdin_retry_rc == 0 )) || [[ "$stdin_retry_code" != 410 ]]; then
    printf 'stdin retry was not rejected as one-shot (rc=%s http=%s)\n' "$stdin_retry_rc" "$stdin_retry_code" >&2
    exit 1
fi
wait "$producer_pid" 2>/dev/null || true
producer_pid=''
pass 'T-LINK-STDIN failed stream exits nonzero and is not replayable'

cat >"$tmp/slow-producer.py" <<'PY'
import os, sys, time
for _ in range(512):
    os.write(1, b"tar-stream-block\0\n\r\xff" * 4096)
    time.sleep(0.01)
PY
chmod +x "$tmp/slow-producer.py"
clear_fault
start_exec_link exec_cancel --filename failed.tar --max-downloads 1 -- \
    /usr/bin/python3 "$tmp/slow-producer.py"
ip netns exec "$ns_receiver" curl --fail --silent --show-error --limit-rate 64k \
    --connect-timeout 5 --max-time 90 --cacert "$tmp/ca.pem" \
    --resolve "$current_resolve" "$current_url" -o "$tmp/exec.out" &
curl_pid=$!
wait_log "$current_log" 'DirectQuic' 15 || { tail -n 100 "$current_log" >&2; exit 1; }
install_udp_fault
exec_deadline=$((SECONDS + 45))
while kill -0 "$curl_pid" 2>/dev/null && (( SECONDS < exec_deadline )); do sleep 0.1; done
if kill -0 "$curl_pid" 2>/dev/null; then
    kill_wait "$curl_pid"
    printf 'exec download did not fail within 45 seconds\n' >&2
    exit 1
fi
set +e; wait "$curl_pid"; exec_curl_rc=$?; set -e
curl_pid=''
(( exec_curl_rc != 0 )) || { printf 'exec download unexpectedly completed after UDP drop\n' >&2; exit 1; }
exec_wait=$((SECONDS + 25))
while kill -0 "$link_pid" 2>/dev/null && (( SECONDS < exec_wait )); do sleep 0.1; done
if kill -0 "$link_pid" 2>/dev/null; then
    printf 'exec producer client remained alive after failed GET\n' >&2
    exit 1
fi
set +e; wait "$link_pid"; exec_rc=$?; set -e
link_pid=''
(( exec_rc != 0 )) || { printf 'exec client returned success after fault\n' >&2; exit 1; }
set +e
exec_retry_code=$(ip netns exec "$ns_receiver" curl --silent --show-error --max-time 10 \
    --cacert "$tmp/ca.pem" --resolve "$current_resolve" \
    -o /dev/null -w '%{http_code}' "$current_url")
exec_retry_rc=$?
set -e
if (( exec_retry_rc == 0 )) || [[ "$exec_retry_code" != 410 ]]; then
    printf 'exec retry was not rejected as one-shot (rc=%s http=%s)\n' "$exec_retry_rc" "$exec_retry_code" >&2
    exit 1
fi
producer_left=0
for _ in $(seq 1 50); do
    if ! pgrep -f -- "$tmp/slow-producer.py" >/dev/null 2>&1; then
        producer_left=1
        break
    fi
    sleep 0.1
done
(( producer_left == 1 )) || { printf 'exec producer process remained after cancellation\n' >&2; exit 1; }
pass 'T-LINK-EXEC-CANCEL producer is not replayed after transport fault'
clear_fault

printf 'T-LINK-CLEANUP 100 registrations, half relay and half QUIC\n'
printf 'cycle payload\n' >"$tmp/cycle.bin"
fd_before=$(find "/proc/$server_pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
for cycle in $(seq 1 100); do
    if (( cycle % 2 == 0 )); then
        start_link "cycle-$cycle" "$tmp/cycle.bin" --filename cycle.bin --max-downloads 1
    else
        start_link "cycle-$cycle" "$tmp/cycle.bin" --filename cycle.bin --relay-only --max-downloads 1
    fi
    curl_download "$tmp/cycle-$cycle.out"
    assert_hash "$tmp/cycle.bin" "$tmp/cycle-$cycle.out"
    if (( cycle % 2 == 0 )); then
        wait_log "$current_log" 'DirectQuic' 10 || { tail -n 80 "$current_log" >&2; exit 1; }
    else
        wait_log "$current_log" 'RelayTcp' 10 || { tail -n 80 "$current_log" >&2; exit 1; }
    fi
    stop_link
done
sleep 1
fd_after=$(find "/proc/$server_pid/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
fd_delta=$((fd_after - fd_before))
(( fd_delta <= 2 )) || { printf 'server FD leak after cleanup cycles: before=%s after=%s delta=%s\n' "$fd_before" "$fd_after" "$fd_delta" >&2; exit 1; }
pass "T-LINK-CLEANUP 100 cycles released resources (fd delta $fd_delta, tolerance 2)"

printf 'T-LINK-ORACLE direct path assertion\n'
if grep -q 'DirectQuic' "$tmp/direct.log" && ! grep -q 'RelayTcp' "$tmp/direct.log"; then
    pass 'T-LINK-ORACLE direct case cannot silently pass through relay'
else
    printf 'direct-path oracle failed: expected DirectQuic and no RelayTcp\n' >&2
    exit 1
fi

printf '================ %s passed, %s failed ================\n' "$pass_count" "$fail_count"
(( fail_count == 0 ))
