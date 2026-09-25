#!/usr/bin/env bash
# Fast link transfer end-to-end acceptance (plan 004, T-FL-E1..E13).
#
# Drives a real `bore server`, configured ONLY through the public
# BORE_FAST_LINK_TRANSFER_* environment, with real curl and wget clients:
# the exact commands a user types. Every case prints
# `T-FL-E<n> PASS|FAIL <reason>`; the first FAIL prints the tail of the
# server log and exits non-zero.
#
# wget has no --resolve, so wget cases connect to 127.0.0.1 and send the
# fast host in an explicit Host header; the certificate does not cover the
# IP, hence --no-check-certificate for wget only (curl always verifies
# against the test CA).
set -Eeuo pipefail

for tool in cargo curl wget openssl python3 sha256sum tar awk; do
    command -v "$tool" >/dev/null || { printf 'missing prerequisite: %s\n' "$tool" >&2; exit 2; }
done

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BORE="${BORE:-$repo/target/release/bore}"
if [[ ! -x "$BORE" ]] ||
    find "$repo/src" "$repo/Cargo.toml" "$repo/Cargo.lock" -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    cargo build --locked --all-features --release --quiet
fi
[[ -x "$BORE" ]] || { printf 'missing bore binary: %s\n' "$BORE" >&2; exit 2; }

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bore-fast-link-e2e.XXXXXX")
pids=()
cleanup() {
    local status=$?
    set +e
    for pid in "${pids[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null
            sleep 0.1
            kill -KILL "$pid" 2>/dev/null
        fi
        wait "$pid" 2>/dev/null
    done
    if [[ "${KEEP_TMP:-0}" == 1 ]]; then
        printf 'T-FL-E temporary evidence: %s\n' "$tmp" >&2
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

current_log=''
fail() {
    printf 'T-FL-%s FAIL %s\n' "$1" "$2"
    if [[ -n "$current_log" && -f "$current_log" ]]; then
        printf -- '--- last 40 lines of %s\n' "$current_log"
        tail -n 40 "$current_log" | sed 's/\x1b\[[0-9;]*m//g'
    fi
    exit 1
}
pass() { printf 'T-FL-%s PASS %s\n' "$1" "$2"; }

# ── certificates ────────────────────────────────────────────────────────────
cat >"$tmp/ca.cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bore fast link e2e CA
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
CN = bore fast link e2e server
[req_ext]
subjectAltName = DNS:bore.local,DNS:*.bore.local
[leaf_ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature,keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = DNS:bore.local,DNS:*.bore.local
authorityKeyIdentifier = keyid,issuer
subjectKeyIdentifier = hash
EOF
openssl req -newkey rsa:2048 -nodes -keyout "$tmp/leaf.key" -out "$tmp/leaf.csr" \
    -config "$tmp/leaf.cnf" >/dev/null 2>&1
openssl x509 -req -days 1 -sha256 -in "$tmp/leaf.csr" \
    -CA "$tmp/ca.pem" -CAkey "$tmp/ca.key" -CAcreateserial \
    -out "$tmp/leaf.pem" -extfile "$tmp/leaf.cnf" -extensions leaf_ext >/dev/null 2>&1

PASS="e2e-$RANDOM-$$"

# start_server NAME [ENV=VALUE...]: start one server with the fast link env
# (overridable per call) and set CP/HP/SP to its control, vhost HTTP and
# vhost HTTPS ports. The log goes through a `cat` pipe, like a container,
# via process substitution so `$!` is bore itself: with `bore | cat &` the
# recorded pid would be cat's, bore would outlive the script, and `wait` on
# the job would block cleanup forever.
start_server() {
    local name=$1
    shift
    CP=$(free_port)
    HP=$(free_port)
    SP=$(free_port)
    current_log="$tmp/server-$name.log"
    env BORE_FAST_LINK_TRANSFER_ENABLED=true \
        BORE_FAST_LINK_TRANSFER_VHOST=fast.bore.local \
        BORE_FAST_LINK_TRANSFER_AUTH="u:$PASS" \
        RUST_LOG=info \
        "$@" \
        "$BORE" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
        --control-port "$CP" --vhost-base-domain bore.local --vhost-mode both \
        --vhost-http-port "$HP" --vhost-https-port "$SP" \
        --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
        > >(cat >"$current_log") 2>&1 &
    pids+=($!)
    wait_tcp "$SP" || fail "E0" "server $name did not open its HTTPS port"
    wait_tcp "$HP" || fail "E0" "server $name did not open its HTTP port"
}

base() { printf 'https://fast.bore.local:%s' "$SP"; }
ccurl() { curl --cacert "$tmp/ca.pem" --resolve "fast.bore.local:$SP:127.0.0.1" "$@"; }

# upload NAME URL_PATH SOURCE: run a `curl -N -T` uploader in the
# background; its stdout (the chunked status body) goes to $tmp/NAME.out
# and its exit code to $tmp/NAME.rc. Sets up_pid.
upload() {
    local name=$1 path=$2 source=$3
    : >"$tmp/$name.out"
    rm -f "$tmp/$name.rc"
    (
        rc=0
        ccurl -sS -N -u "u:$PASS" -T "$source" "$(base)$path" >"$tmp/$name.out" 2>"$tmp/$name.err" || rc=$?
        echo "$rc" >"$tmp/$name.rc"
    ) &
    up_pid=$!
    pids+=("$up_pid")
}

# wait_link NAME: wait (bounded) for the first status line and print it.
wait_link() {
    local name=$1 deadline=$((SECONDS + 20))
    while (( SECONDS < deadline )); do
        if [[ -s "$tmp/$name.out" ]] && head -n 1 "$tmp/$name.out" | grep -q '^https://'; then
            head -n 1 "$tmp/$name.out"
            return 0
        fi
        [[ -f "$tmp/$name.rc" ]] && break
        sleep .02
    done
    return 1
}

# wait_rc NAME SECONDS: wait for the uploader's exit code and print it.
wait_rc() {
    local name=$1 deadline=$((SECONDS + $2))
    while (( SECONDS < deadline )); do
        if [[ -s "$tmp/$name.rc" ]]; then
            cat "$tmp/$name.rc"
            return 0
        fi
        sleep .05
    done
    echo timeout
}

# wait_out NAME PATTERN SECONDS: wait until the status body matches.
wait_out() {
    local name=$1 pattern=$2 deadline=$((SECONDS + $3))
    while (( SECONDS < deadline )); do
        grep -q -- "$pattern" "$tmp/$name.out" 2>/dev/null && return 0
        sleep .05
    done
    return 1
}

# wait_size FILE BYTES SECONDS: wait until FILE holds at least BYTES.
wait_size() {
    local file=$1 bytes=$2 deadline=$((SECONDS + $3))
    while (( SECONDS < deadline )); do
        if [[ -f "$file" ]] && (( $(stat -c %s "$file") >= bytes )); then return 0; fi
        sleep .02
    done
    return 1
}

sha() { sha256sum "$1" | awk '{print $1}'; }
link_re='^https://fast\.bore\.local:[0-9]+/[a-z0-9]{16}/'

start_server main

# ── E1: a file with Content-Length ─────────────────────────────────────────
dd if=/dev/urandom of="$tmp/f.bin" bs=1M count=64 status=none
upload e1 / "$tmp/f.bin"
link=$(wait_link e1) || fail E1 "no link printed"
[[ "$link" =~ ${link_re}f\.bin$ ]] || fail E1 "unexpected link shape: $link"
ccurl -fsS -o "$tmp/e1.got" "$link" || fail E1 "download failed"
[[ "$(sha "$tmp/e1.got")" == "$(sha "$tmp/f.bin")" ]] || fail E1 "SHA-256 mismatch"
rc=$(wait_rc e1 20)
[[ "$rc" == 0 ]] || fail E1 "uploader exit $rc (want 0)"
tail -n 1 "$tmp/e1.out" | grep -q '^# done: 67108864 bytes' || fail E1 "last line is not '# done: 67108864 bytes': $(tail -n 1 "$tmp/e1.out")"
pass E1 "64 MiB file, SHA-256 identical, uploader exit 0"

# ── E2: tar streaming (chunked), downloaded with wget ──────────────────────
mkdir -p "$tmp/e2src/sub dir"
for i in $(seq 1 199); do head -c $((RANDOM * 7)) /dev/urandom >"$tmp/e2src/file-$i.bin"; done
: >"$tmp/e2src/sub dir/empty file"
(
    rc=0
    tar -cf - -C "$tmp/e2src" . |
        ccurl -sS -N -u "u:$PASS" -T - "$(base)/dir.tar" >"$tmp/e2.out" 2>"$tmp/e2.err" || rc=${PIPESTATUS[1]}
    echo "$rc" >"$tmp/e2.rc"
) &
pids+=($!)
link=$(wait_link e2) || fail E2 "no link printed"
[[ "$link" =~ ${link_re}dir\.tar$ ]] || fail E2 "unexpected link shape: $link"
path=${link#https://fast.bore.local:$SP}
wrc=0
wget -q --no-check-certificate --header="Host: fast.bore.local:$SP" \
    -O "$tmp/e2.tar" "https://127.0.0.1:$SP$path" || wrc=$?
[[ "$wrc" == 0 ]] || fail E2 "wget exit $wrc"
tar -tf "$tmp/e2.tar" | sort >"$tmp/e2.list"
tar -cf "$tmp/e2.ref.tar" -C "$tmp/e2src" .
tar -tf "$tmp/e2.ref.tar" | sort >"$tmp/e2.ref.list"
cmp -s "$tmp/e2.list" "$tmp/e2.ref.list" || fail E2 "tar listing differs"
mkdir "$tmp/e2dst"
tar -xf "$tmp/e2.tar" -C "$tmp/e2dst"
diff -r "$tmp/e2src" "$tmp/e2dst" >/dev/null || fail E2 "extracted tree differs"
rc=$(wait_rc e2 20)
[[ "$rc" == 0 ]] || fail E2 "uploader exit $rc (want 0)"
pass E2 "tar -cf - | curl -T - streamed 200 files, wget exit 0, trees identical"

# ── E3: the URL name becomes the download filename (-OJ) ───────────────────
upload e3 / "$tmp/f.bin"
link=$(wait_link e3) || fail E3 "no link printed"
mkdir "$tmp/e3dir"
(cd "$tmp/e3dir" && ccurl -fsS -OJ "$link") || fail E3 "download failed"
[[ -f "$tmp/e3dir/f.bin" ]] || fail E3 "expected $tmp/e3dir/f.bin, got: $(ls "$tmp/e3dir")"
[[ "$(sha "$tmp/e3dir/f.bin")" == "$(sha "$tmp/f.bin")" ]] || fail E3 "SHA-256 mismatch"
[[ "$(wait_rc e3 20)" == 0 ]] || fail E3 "uploader did not exit 0"
pass E3 "curl -OJ saved f.bin"

# ── E13: a small binary file (curl sends no Expect below 1 MiB, so the
#    body follows the head at once). Scenario coverage only: over TLS curl
#    writes head and body in separate records, so this does NOT discriminate
#    the Host-parsing regression (a binary body read together with the head
#    used to 502); E11's plain PUT does, and so does the vhost unit test ─
head -c 4096 /dev/urandom >"$tmp/tiny.bin"
upload e13 / "$tmp/tiny.bin"
link=$(wait_link e13) || fail E13 "no link printed: $(cat "$tmp/e13.out" "$tmp/e13.err" 2>/dev/null | tr '\n' '|')"
ccurl -fsS -o "$tmp/e13.got" "$link" || fail E13 "download failed"
[[ "$(sha "$tmp/e13.got")" == "$(sha "$tmp/tiny.bin")" ]] || fail E13 "SHA-256 mismatch"
[[ "$(wait_rc e13 20)" == 0 ]] || fail E13 "uploader did not exit 0"
pass E13 "4 KiB binary without Expect: head and body in one read, SHA-256 identical"

# ── E4: upload authentication ──────────────────────────────────────────────
head -c 1024 /dev/urandom >"$tmp/small.bin"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' -T "$tmp/small.bin" "$(base)/") || true
[[ "$code" == 401 ]] || fail E4 "no credentials: HTTP $code (want 401)"
frc=0
ccurl -sS --fail -o /dev/null -T "$tmp/small.bin" "$(base)/" 2>/dev/null || frc=$?
[[ "$frc" == 22 ]] || fail E4 "curl --fail exit $frc (want 22)"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' -u "u:wrong" -T "$tmp/small.bin" "$(base)/") || true
[[ "$code" == 401 ]] || fail E4 "wrong password: HTTP $code (want 401)"
pass E4 "missing and wrong credentials answer 401 (curl --fail exit 22)"

# ── E5: previews never consume the link ────────────────────────────────────
dd if=/dev/urandom of="$tmp/e5.bin" bs=1M count=8 status=none
upload e5 / "$tmp/e5.bin"
link=$(wait_link e5) || fail E5 "no link printed"
read -r code ctype < <(ccurl -sS -o /dev/null -w '%{http_code} %{content_type}\n' \
    -A 'Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)' "$link")
[[ "$code" == 200 && "$ctype" == text/html* ]] || fail E5 "bot preview: HTTP $code $ctype"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' -r 0-99 "$link") || true
[[ "$code" == 416 ]] || fail E5 "range request: HTTP $code (want 416)"
ccurl -sS -I "$link" >"$tmp/e5.head"
head -n 1 "$tmp/e5.head" | grep -q ' 200' || fail E5 "HEAD: $(head -n 1 "$tmp/e5.head")"
grep -qi '^content-length: 8388608' "$tmp/e5.head" || fail E5 "HEAD without content-length"
grep -qi '^content-disposition: attachment' "$tmp/e5.head" || fail E5 "HEAD without content-disposition"
ccurl -fsS -o "$tmp/e5.got" "$link" || fail E5 "download after previews failed"
[[ "$(sha "$tmp/e5.got")" == "$(sha "$tmp/e5.bin")" ]] || fail E5 "SHA-256 mismatch"
[[ "$(wait_rc e5 20)" == 0 ]] || fail E5 "uploader did not exit 0"
pass E5 "bot UA 200 html, Range 416, HEAD 200, then the full download"

# ── E6: downloader dropped past the replay window ──────────────────────────
dd if=/dev/urandom of="$tmp/e6.bin" bs=1M count=256 status=none
upload e6 / "$tmp/e6.bin"
link=$(wait_link e6) || fail E6 "no link printed"
# curl itself, not the `ccurl` function: `$!` of a backgrounded function is
# its subshell, and killing that leaves the curl child running.
curl --cacert "$tmp/ca.pem" --resolve "fast.bore.local:$SP:127.0.0.1" \
    -sS --limit-rate 20M -o "$tmp/e6.part" "$link" 2>/dev/null &
dl_pid=$!
pids+=("$dl_pid")
wait_size "$tmp/e6.part" $((16 * 1024 * 1024)) 30 || fail E6 "download never passed 16 MiB"
kill -KILL "$dl_pid" 2>/dev/null || true
wait "$dl_pid" 2>/dev/null || true
rc=$(wait_rc e6 30)
[[ "$rc" == 18 ]] || fail E6 "uploader exit $rc (want 18); status: $(tr '\n' '|' <"$tmp/e6.out")"
grep -q '^# failed:' "$tmp/e6.out" || fail E6 "no '# failed:' line"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' "$link") || true
[[ "$code" == 404 ]] || fail E6 "link after failure: HTTP $code (want 404)"
pass E6 "download killed past 16 MiB: uploader exit 18 with '# failed:', link 404"

# ── E7: downloader dropped inside the replay window re-arms ────────────────
upload e7 / "$tmp/f.bin"
link=$(wait_link e7) || fail E7 "no link printed"
curl --cacert "$tmp/ca.pem" --resolve "fast.bore.local:$SP:127.0.0.1" \
    -sS --limit-rate 50k -o /dev/null "$link" 2>/dev/null &
dl_pid=$!
pids+=("$dl_pid")
wait_out e7 '^# download started' 10 || fail E7 "first download never started"
sleep 1
kill -KILL "$dl_pid" 2>/dev/null || true
wait "$dl_pid" 2>/dev/null || true
wait_out e7 '^# download interrupted before the first 4 MiB' 15 || fail E7 "no re-arm line: $(tr '\n' '|' <"$tmp/e7.out")"
ccurl -fsS -o "$tmp/e7.got" "$link" || fail E7 "second download failed"
[[ "$(sha "$tmp/e7.got")" == "$(sha "$tmp/f.bin")" ]] || fail E7 "SHA-256 mismatch"
rc=$(wait_rc e7 20)
[[ "$rc" == 0 ]] || fail E7 "uploader exit $rc (want 0)"
pass E7 "slow download killed inside 4 MiB: link re-armed, second download complete, exit 0"

# ── E9: uploader dropped mid-stream truncates the download ─────────────────
(
    rc=0
    head -c 300M /dev/urandom |
        ccurl -sS -N -u "u:$PASS" -T - "$(base)/s.bin" >"$tmp/e9.out" 2>"$tmp/e9.err" || rc=${PIPESTATUS[1]}
    echo "$rc" >"$tmp/e9.rc"
) &
e9_sub=$!
pids+=("$e9_sub")
link=$(wait_link e9) || fail E9 "no link printed"
(
    rc=0
    ccurl -sS -o "$tmp/e9.part" "$link" 2>/dev/null || rc=$?
    echo "$rc" >"$tmp/e9.dl.rc"
) &
pids+=($!)
wait_size "$tmp/e9.part" $((32 * 1024 * 1024)) 30 || fail E9 "download never passed 32 MiB"
pkill -KILL -f -- "-T - $(base)/s.bin" || fail E9 "uploader curl not found"
deadline=$((SECONDS + 30))
while [[ ! -s "$tmp/e9.dl.rc" ]] && (( SECONDS < deadline )); do sleep .05; done
drc=$(cat "$tmp/e9.dl.rc" 2>/dev/null || echo timeout)
[[ "$drc" != 0 && "$drc" != timeout ]] || fail E9 "downloader exit $drc (want non-zero)"
pass E9 "uploader killed mid-stream: downloader exit $drc (truncated, never complete)"

# ── E10: usage page ────────────────────────────────────────────────────────
ccurl -fsS "$(base)/" >"$tmp/e10.txt" || fail E10 "GET / failed"
grep -q 'curl -u USER:PASS -T' "$tmp/e10.txt" || fail E10 "usage text missing"
pass E10 "GET / prints the usage"

# ── E11: plain HTTP is refused or redirected ───────────────────────────────
code=$(curl -sS -o /dev/null -w '%{http_code}' --resolve "fast.bore.local:$HP:127.0.0.1" \
    -u "u:$PASS" -T "$tmp/small.bin" "http://fast.bore.local:$HP/") || true
[[ "$code" == 403 ]] || fail E11 "plain PUT: HTTP $code (want 403)"
read -r code location < <(curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' \
    --resolve "fast.bore.local:$HP:127.0.0.1" "http://fast.bore.local:$HP/x")
[[ "$code" == 308 && "$location" == "https://fast.bore.local:$SP/x" ]] ||
    fail E11 "plain GET: HTTP $code Location $location (want 308 https://fast.bore.local:$SP/x)"
pass E11 "plain PUT 403, plain GET 308 to https://fast.bore.local:$SP/x"

# ── E8: wait timeout ───────────────────────────────────────────────────────
start_server expiry BORE_FAST_LINK_TRANSFER_WAIT_TIMEOUT=2
upload e8 / "$tmp/small.bin"
link=$(wait_link e8) || fail E8 "no link printed"
rc=$(wait_rc e8 10)
[[ "$rc" == 18 ]] || fail E8 "uploader exit $rc (want 18); status: $(tr '\n' '|' <"$tmp/e8.out")"
grep -q '^# expired:' "$tmp/e8.out" || fail E8 "no '# expired:' line"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' "$link") || true
[[ "$code" == 404 ]] || fail E8 "link after expiry: HTTP $code (want 404)"
pass E8 "no download in 2 s: uploader exit 18 with '# expired:', link 404"

# ── E12: kill switch ───────────────────────────────────────────────────────
start_server disabled BORE_FAST_LINK_TRANSFER_ENABLED=false
deadline=$((SECONDS + 5))
until grep -q 'ignoring this setting' "$current_log" || (( SECONDS >= deadline )); do sleep .05; done
grep -q 'ignoring this setting' "$current_log" || fail E12 "no 'ignoring this setting' warning"
code=$(ccurl -sS -o /dev/null -w '%{http_code}' "$(base)/") || true
[[ "$code" == 502 ]] || fail E12 "disabled fast host: HTTP $code (want 502, an ordinary unknown vhost)"
pass E12 "ENABLED=false: settings ignored with a warning, fast host is an ordinary vhost (502)"

printf 'T-FL-E all 13 cases PASS\n'
