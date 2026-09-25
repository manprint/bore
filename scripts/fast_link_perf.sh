#!/usr/bin/env bash
# Fast link transfer bandwidth gate + transit proof (plan 004, T-FL-PERF,
# T-FL-TRANSIT).
#
# One server, one host, one tmpfs payload. Two arms, interleaved so a drift
# of the machine lands on both:
#   VHOST  the existing relay baseline: a local HTTP origin behind `bore
#          vhost` (TCP relay, no --udp), downloaded over the vhost HTTPS
#          frontend.
#   FAST   `curl -T` into the fast host, the printed link downloaded by a
#          second curl.
# The measure is the DOWNLOADER's speed in both arms. Raw samples are always
# printed (a median alone hides a corrupt sort, V-11). PASS when
# median(FAST) >= 1.0 x median(VHOST): the fast path has one hop and no mux
# where the relay has two hops and yamux, so measuring less is a defect,
# not noise to be tuned away.
#
# During the second measured FAST run the server's /proc/<pid>/io
# write_bytes and a 10 Hz VmRSS sample prove the bytes only transit: the
# server writes (almost) nothing to disk and holds no copy of the payload
# (RSS growth bounded by I-2's formula for the buffer size in force).
set -Eeuo pipefail
export LC_ALL=C

if [[ "$(uname -s)" != Linux ]]; then
    printf 'T-FL-PERF SKIP (needs /proc; Linux only)\n'
    exit 0
fi
for tool in cargo curl openssl python3 awk; do
    command -v "$tool" >/dev/null || { printf 'missing prerequisite: %s\n' "$tool" >&2; exit 2; }
done

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BORE="${BORE:-$repo/target/release/bore}"
if [[ ! -x "$BORE" ]] ||
    find "$repo/src" "$repo/Cargo.toml" "$repo/Cargo.lock" -newer "$BORE" -print -quit 2>/dev/null | grep -q .; then
    cargo build --locked --all-features --release --quiet
fi
[[ -x "$BORE" ]] || { printf 'missing bore binary: %s\n' "$BORE" >&2; exit 2; }
# The same proxy buffer for both arms (the existing vhost baseline's value).
export BORE_PROXY_BUFFER_SIZE="${BORE_PROXY_BUFFER_SIZE:-16M}"
SIZE_MIB="${FAST_LINK_PERF_MIB:-1024}"

# The payload lives in tmpfs so no arm reads a disk.
shm=/dev/shm
[[ -d "$shm" && -w "$shm" ]] || shm="${TMPDIR:-/tmp}"
tmp=$(mktemp -d "$shm/bore-fast-link-perf.XXXXXX")
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
        rm -f -- "$tmp/payload.bin"
        printf 'T-FL-PERF temporary evidence: %s\n' "$tmp" >&2
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

die() {
    printf 'T-FL-PERF FAIL %s\n' "$1"
    [[ -f "$tmp/server.log" ]] && tail -n 40 "$tmp/server.log" | sed 's/\x1b\[[0-9;]*m//g'
    exit 1
}

# ── certificates ────────────────────────────────────────────────────────────
cat >"$tmp/ca.cnf" <<'EOF'
[req]
distinguished_name = dn
prompt = no
[dn]
CN = bore fast link perf CA
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
CN = bore fast link perf server
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

dd if=/dev/zero of="$tmp/payload.bin" bs=1M count="$SIZE_MIB" status=none
SIZE=$((SIZE_MIB * 1024 * 1024))
PASS="perf-$RANDOM-$$"
CP=$(free_port)
HP=$(free_port)
SP=$(free_port)
OP=$(free_port)

# The baseline origin: a plain threaded HTTP server streaming the tmpfs file
# in 1 MiB blocks, no hashing, so neither arm does extra work.
python3 - "$OP" "$tmp/payload.bin" >"$tmp/origin.log" 2>&1 <<'PY' &
import http.server, os, sys
port, source = int(sys.argv[1]), sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        size = os.stat(source).st_size
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Content-Type", "application/octet-stream")
        self.end_headers()
        with open(source, "rb", buffering=0) as inp:
            while True:
                block = inp.read(1024 * 1024)
                if not block:
                    break
                self.wfile.write(block)
    def log_message(self, *_args):
        pass
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
pids+=($!)

# `$!` of a process-substitution redirect is the bore process itself, so
# /proc/$server_pid is the server and never the log pipe.
BORE_FAST_LINK_TRANSFER_ENABLED=true BORE_FAST_LINK_TRANSFER_VHOST=fast.bore.local \
    BORE_FAST_LINK_TRANSFER_AUTH="u:$PASS" \
    "$BORE" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port "$CP" --vhost-base-domain bore.local --vhost-mode both \
    --vhost-http-port "$HP" --vhost-https-port "$SP" \
    --vhost-cert-file "$tmp/leaf.pem" --vhost-key-file "$tmp/leaf.key" \
    > >(cat >"$tmp/server.log") 2>&1 &
server_pid=$!
pids+=("$server_pid")
wait_tcp "$SP" || die "server did not open its HTTPS port"
wait_tcp "$OP" || die "origin did not start"

"$BORE" vhost "127.0.0.1:$OP" --subdomain perf --id perf --to "http://127.0.0.1:$CP" \
    >"$tmp/provider.log" 2>&1 &
pids+=($!)
deadline=$((SECONDS + 20))
until curl -fsS -o /dev/null --cacert "$tmp/ca.pem" --resolve "perf.bore.local:$SP:127.0.0.1" \
    -r 0-0 "https://perf.bore.local:$SP/payload.bin" 2>/dev/null; do
    (( SECONDS < deadline )) || die "vhost provider never became routable"
    sleep .1
done

cpu_ticks() { awk '{print $14 + $15}' "/proc/$server_pid/stat"; }
clk_tck=$(getconf CLK_TCK)

# download URL RESOLVE: one downloader; prints "speed size".
download() {
    curl -sS -o /dev/null --cacert "$tmp/ca.pem" --resolve "$2" \
        -w '%{speed_download} %{size_download}\n' "$1"
}

arm_vhost() {
    download "https://perf.bore.local:$SP/payload.bin" "perf.bore.local:$SP:127.0.0.1"
}

# arm_fast [transit]: upload, download, and (with `transit`) sample the
# server's disk writes and RSS for the whole transfer.
arm_fast() {
    local transit=${1:-} out="$tmp/up.out" rc_file="$tmp/up.rc"
    : >"$out"
    rm -f "$rc_file" "$tmp/rss.stop"
    local wb0 rss0 sampler=''
    if [[ "$transit" == transit ]]; then
        wb0=$(awk '/^write_bytes:/{print $2}' "/proc/$server_pid/io")
        rss0=$(awk '/^VmRSS:/{print $2}' "/proc/$server_pid/status")
        : >"$tmp/rss.samples"
        (
            while [[ ! -e "$tmp/rss.stop" ]]; do
                awk '/^VmRSS:/{print $2}' "/proc/$server_pid/status" >>"$tmp/rss.samples"
                sleep 0.1
            done
        ) &
        sampler=$!
    fi
    (
        rc=0
        curl -sS -N --cacert "$tmp/ca.pem" --resolve "fast.bore.local:$SP:127.0.0.1" \
            -u "u:$PASS" -T "$tmp/payload.bin" "https://fast.bore.local:$SP/payload.bin" \
            >"$out" 2>"$tmp/up.err" || rc=$?
        echo "$rc" >"$rc_file"
    ) &
    local up=$!
    local deadline=$((SECONDS + 20))
    until head -n 1 "$out" 2>/dev/null | grep -q '^https://'; do
        (( SECONDS < deadline )) || die "no link printed"
        sleep .01
    done
    local link
    link=$(head -n 1 "$out")
    download "$link" "fast.bore.local:$SP:127.0.0.1"
    wait "$up" 2>/dev/null || true
    [[ "$(cat "$rc_file" 2>/dev/null)" == 0 ]] || die "uploader exit $(cat "$rc_file" 2>/dev/null): $(cat "$tmp/up.err")"
    grep -q "^# done: $SIZE bytes" "$out" || die "uploader did not report '# done: $SIZE bytes'"
    if [[ -n "$sampler" ]]; then
        touch "$tmp/rss.stop"
        wait "$sampler" 2>/dev/null || true
        local wb1 rss_max
        wb1=$(awk '/^write_bytes:/{print $2}' "/proc/$server_pid/io")
        rss_max=$(sort -n "$tmp/rss.samples" | tail -n 1)
        printf '%s %s %s %s %s\n' "$wb0" "$wb1" "$rss0" "$rss_max" "$(wc -l <"$tmp/rss.samples")" >"$tmp/transit"
    fi
}

# run ARM [transit] -> appends "speed" to $tmp/<arm>.speeds and CPU to .cpu
run() {
    local arm=$1 t0 t1 line speed size
    t0=$(cpu_ticks)
    if [[ "$arm" == vhost ]]; then line=$(arm_vhost); else line=$(arm_fast "${2:-}"); fi
    t1=$(cpu_ticks)
    read -r speed size <<<"$line"
    [[ "$size" == "$SIZE" ]] || die "$arm downloader received $size bytes (want $SIZE)"
    printf '%s\n' "$speed" >>"$tmp/$arm.speeds"
    printf '%s\n' "$((t1 - t0))" >>"$tmp/$arm.cpu"
}

# Warm-up, unmeasured: page cache, TLS session setup, carrier.
arm_vhost >/dev/null
arm_fast >/dev/null
: >"$tmp/vhost.speeds"; : >"$tmp/fast.speeds"; : >"$tmp/vhost.cpu"; : >"$tmp/fast.cpu"

run vhost
run fast
run fast transit
run vhost
run vhost
run fast

python3 - "$tmp" "$SIZE" "$clk_tck" "$BORE_PROXY_BUFFER_SIZE" <<'PY'
import re, statistics, sys
tmp, size, tck, buf_spec = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
def load(name, conv=float):
    return [conv(x) for x in open(f"{tmp}/{name}").read().split()]
mib = 1024 * 1024
vh = [s / mib for s in load("vhost.speeds")]
fa = [s / mib for s in load("fast.speeds")]
assert len(vh) == 3 and len(fa) == 3, (vh, fa)
gib = size / (1024 ** 3)
cpu = {a: [t / tck / gib for t in load(f"{a}.cpu", int)] for a in ("vhost", "fast")}
print("T-FL-PERF raw vhost=%s fast=%s (MiB/s)" % (
    ",".join(f"{x:.1f}" for x in vh), ",".join(f"{x:.1f}" for x in fa)))
print("T-FL-PERF server cpu_s_per_gib vhost=%s fast=%s (informational)" % (
    ",".join(f"{x:.2f}" for x in cpu["vhost"]), ",".join(f"{x:.2f}" for x in cpu["fast"])))
mv, mf = statistics.median(vh), statistics.median(fa)
ratio = mf / mv if mv else 0.0
print(f"T-FL-PERF median vhost={mv:.1f} fast={mf:.1f} MiB/s ratio={ratio:.3f}")
print("T-FL-PERF " + ("PASS" if ratio >= 1.0 else "FAIL") + " (threshold fast >= 1.0 x vhost)")

# The RSS bound is I-2's own formula, never a fixed figure: the replay
# window, the pump's PUMP_DEPTH (4) buffers and the one wait buffer, each of
# proxy_buffer_size() bytes, plus 16 MiB of allocator/TLS slack. A fixed
# 48 MiB sat BELOW that formula for the 16M buffer this gate runs with, and
# read 45.9 MiB on a passing run. Either way the bound stays far below the
# payload, which is what proves the payload is never held whole.
m = re.fullmatch(r"\s*(\d+)\s*([A-Za-z]*)\s*", buf_spec)
mult = {"": 1, "b": 1, "k": 1000, "kb": 1000, "m": 10**6, "mb": 10**6, "g": 10**9,
        "gb": 10**9, "ki": 1024, "kib": 1024, "mi": mib, "mib": mib}
buf = int(m.group(1)) * mult[m.group(2).lower()]
rss_bound = 4 * mib + 5 * buf + 16 * mib
assert rss_bound < size // 2, (rss_bound, size)
wb0, wb1, rss0, rss_max, samples = map(int, open(f"{tmp}/transit").read().split())
dwb, drss = wb1 - wb0, (rss_max - rss0) * 1024
print(f"T-FL-TRANSIT write_bytes_delta={dwb} rss_growth={drss} rss_bound={rss_bound} samples={samples}")
ok = dwb <= 262144 and drss <= rss_bound and samples >= 2
print("T-FL-TRANSIT " + ("PASS" if ok else "FAIL") +
      f" (write <= 256 KiB, RSS growth <= {rss_bound // mib} MiB = replay + 5 x buffer + 16 MiB)")
raise SystemExit(0 if ratio >= 1.0 and ok else 1)
PY
