#!/usr/bin/env bash
# Phase 07.1 of docs/plans/plan_VhostEnhancements/: what is the real prize of an
# HTTP/2 vhost edge?
#
# The campaign measured +45 ms per NEW connection from a 21 ms-RTT client but
# only +4.9 ms at 1.84 ms RTT, so ~4.9 ms is server time and ~40 ms is the
# client's TLS round trips. A browser opens ~6 connections per origin, so a
# 30-asset page pays several waves of that. h2 collapses the waves onto one
# connection. That arithmetic is an ESTIMATE from a microbenchmark, and phase 07
# exists so the largest item in the plan is not committed on an estimate.
#
# Apparatus. The impaired leg must be the CLIENT leg only — the provider->server
# and server->origin legs are localhost in a real deployment and must stay
# undelayed. Loopback cannot express that: the kernel picks src 127.0.0.1 for
# every loopback destination, so a `tc` filter on dst 127.0.0.1/32 delays every
# hop, not one. So the client runs in its own network namespace behind a veth
# pair and netem sits on the veth, which is the only leg it can touch.
#
#   netns bore_h2                      host
#   +--------------+   veth   +-----------------------------------+
#   | curl         |=========>| bore server (vhost edge, TLS)     |
#   | 10.77.0.2    | netem    |   -> yamux over lo -> bore vhost  |
#   +--------------+ RTT/2 x2 |        -> bench_origin.py on lo   |
#                             | node h2 server (TLS, ALPN h2)     |
#                             | node h1 server (TLS, http/1.1)    |
#                             +-----------------------------------+
#
# Three arms per RTT, same 31-asset page, same cert, same bytes:
#   tunnel-h1  the product as it ships today: h1 through the vhost edge, 6 conns
#   direct-h1  the same protocol with the tunnel removed (the tunnel's own cost)
#   direct-h2  one multiplexed connection (the protocol's own prize)
# direct-h2 versus tunnel-h1 is the UPPER BOUND on what an h2 vhost edge could
# ever deliver, which is what phase 07.3 has to decide on.
#
# No production code is touched and no deployment access is needed.
#
# Usage: sudo -n /abs/path/scripts/vhost_h2_page_load.sh [rtt-list] [runs]
#   sudo -n .../vhost_h2_page_load.sh                 # 2 21 60 100 ms, 5 runs
#   sudo -n .../vhost_h2_page_load.sh "21 100" 3
# NOPASSWD sudo is per EXACT path; `sudo bash scripts/...` prompts.
set -uo pipefail

RTTS="${1:-2 21 60 100}"
RUNS="${2:-5}"
# Size of the ONE large asset, in bytes. Pass 0 for a small-assets-only page:
# that separates the wave structure h2 collapses from the single-connection bulk
# penalty h2 pays, which the mixed page conflates.
LARGE="${3:-2097152}"
# Number of small assets. Pass 0 with a nonzero LARGE for a bulk-only page: that
# isolates what h2 COSTS on one multiplexed connection, which is the other half
# of the same trade.
SMALL_N="${4:-30}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BORE="$ROOT/target/release/bore"
NS=bore_h2
HOST_IP=10.77.0.1
NS_IP=10.77.0.2
VH=vh_h2host
VN=vh_h2ns
CP=17836        # bore control port
VHSP=19443      # vhost HTTPS frontend
H2P=18443       # node h2 server
H1P=18444       # node h1 server
OP=15062        # origin
SEC=h2spikesecret
DOM=h2.test
LABEL=page
F="${BORE_PERF_OUT:-/tmp/bore-h2-spike}"; mkdir -p "$F"
OUT="$F/results.txt"; : > "$OUT"

[ "$(id -u)" = 0 ] || { echo "must run as root (sudo -n <abs path>)" >&2; exit 1; }
[ -x "$BORE" ] || { echo "missing $BORE; build it as your user: cargo build --release" >&2; exit 1; }
if [ -n "$(find "$ROOT/src" -newer "$BORE" -name '*.rs' -print -quit 2>/dev/null)" ]; then
    echo "$BORE is older than src/; rebuild as your user (NOT root): cargo build --release" >&2
    exit 1
fi
# `sudo` resets the environment, so a node installed under nvm is not on root's
# PATH. Look it up in the invoking user's nvm tree rather than making the
# operator install a second node as root.
NODE="${NODE:-$(command -v node 2>/dev/null || true)}"
if [ -z "$NODE" ] && [ -n "${SUDO_USER:-}" ]; then
    NODE=$(ls -1 "/home/$SUDO_USER"/.nvm/versions/node/*/bin/node 2>/dev/null | tail -1)
fi
[ -x "${NODE:-}" ] || { echo "need node for the h2/h1 reference servers (pass NODE=/path/to/node)" >&2; exit 1; }
command -v openssl >/dev/null || { echo "need openssl" >&2; exit 1; }

PIDS=()
cleanup(){
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
    ip netns del $NS 2>/dev/null
    ip link del $VH 2>/dev/null
}
trap 'cleanup; exit 130' INT TERM
trap cleanup EXIT
log(){ echo "$*" | tee -a "$OUT"; }
nsx(){ ip netns exec $NS "$@"; }

# --- the page ---------------------------------------------------------------
# 30 small assets plus one 2 MiB asset: the F-15 shape, and the shape whose wave
# structure per-request percentiles hide. Distinct sizes rather than one URL
# repeated, so nothing can be served from a cache anywhere in the path.
paths(){
    local i
    [ "$SMALL_N" != 0 ] && for i in $(seq 0 $((SMALL_N-1))); do echo "/b/$((1024+i))"; done
    [ "$LARGE" != 0 ] && echo "/b/$LARGE"
    return 0
}
ASSETS=$(( SMALL_N + (LARGE != 0 ? 1 : 0) ))

# --- cert (one cert for all three arms) -------------------------------------
[ -f "$F/cert.pem" ] || openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
  -keyout "$F/key.pem" -out "$F/cert.pem" -subj "/CN=*.$DOM" \
  -addext "subjectAltName=DNS:*.$DOM,DNS:$DOM" 2>/dev/null

# --- the two reference servers ---------------------------------------------
# Node's built-in http2/https modules: no dependency to install, and the same
# request handler serves both arms, so the only difference between them is the
# protocol.
cat > "$F/pageserver.js" <<'JS'
const fs = require('fs');
const mode = process.argv[2];               // 'h2' | 'h1'
const port = parseInt(process.argv[3], 10);
const host = process.argv[4];
const opts = { key: fs.readFileSync(process.argv[5]), cert: fs.readFileSync(process.argv[6]) };
const RE = /^\/b\/(\d+)$/;
const cache = new Map();
function body(n) {
    let b = cache.get(n);
    if (!b) { b = Buffer.alloc(n); cache.set(n, b); }
    return b;
}
function serve(path, respond, notFound) {
    const m = RE.exec(path);
    if (!m) { notFound(); return; }
    const n = parseInt(m[1], 10);
    if (n > 64 * 1024 * 1024) { notFound(); return; }
    respond(body(n));
}
if (mode === 'h2') {
    opts.allowHTTP1 = false;
    const server = require('http2').createSecureServer(opts);
    server.on('stream', (stream, headers) => {
        serve(headers[':path'] || '', (b) => {
            stream.respond({ ':status': 200, 'content-length': b.length,
                             'content-type': 'application/octet-stream',
                             'cache-control': 'no-store' });
            stream.end(b);
        }, () => { stream.respond({ ':status': 404, 'content-length': 0 }); stream.end(); });
    });
    server.on('error', (e) => { console.error(e.message); });
    server.listen(port, host, () => console.log('h2 listening'));
} else {
    const server = require('https').createServer(opts, (req, res) => {
        serve(req.url, (b) => {
            res.writeHead(200, { 'content-length': b.length,
                                 'content-type': 'application/octet-stream',
                                 'cache-control': 'no-store' });
            res.end(b);
        }, () => { res.writeHead(404, { 'content-length': 0 }); res.end(); });
    });
    server.on('error', (e) => { console.error(e.message); });
    server.listen(port, host, () => console.log('h1 listening'));
}
JS

# --- netns + veth ----------------------------------------------------------
ip netns del $NS 2>/dev/null
ip link del $VH 2>/dev/null
ip netns add $NS
ip link add $VH type veth peer name $VN
ip link set $VN netns $NS
ip addr add $HOST_IP/24 dev $VH
ip link set $VH up
nsx ip addr add $NS_IP/24 dev $VN
nsx ip link set $VN up
nsx ip link set lo up

# netem goes on BOTH ends, delay RTT/2 each, so the round trip is symmetric and
# equal to the target. One-sided delay would halve the setup cost, which is
# exactly the quantity under measurement.
impair(){ # <rtt-ms>
    local half
    half=$(awk -v r="$1" 'BEGIN{printf "%.3f", r/2}')
    tc qdisc del dev $VH root 2>/dev/null
    nsx tc qdisc del dev $VN root 2>/dev/null
    if [ "$1" != "0" ]; then
        tc qdisc add dev $VH root netem delay "${half}ms"
        nsx tc qdisc add dev $VN root netem delay "${half}ms"
    fi
}

# --- origin, bore server, provider, reference servers ----------------------
python3 "$ROOT/scripts/bench_origin.py" $OP > "$F/origin.log" 2>&1 &
PIDS+=($!); disown
for i in $(seq 40); do curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" 2>/dev/null && break; sleep 0.25; done

cat > "$F/vhost.yml" <<YML
base_domain: $DOM
mode: https
reservations: []
YML

"$BORE" server --secret $SEC --control-port $CP --bind-tunnels 0.0.0.0 \
  --vhost-config "$F/vhost.yml" --vhost-https-port $VHSP \
  --vhost-cert-file "$F/cert.pem" --vhost-key-file "$F/key.pem" \
  > "$F/server.log" 2>&1 &
PIDS+=($!); disown
for i in $(seq 60); do ss -ltn 2>/dev/null | grep -q ":$CP " && break; sleep 0.25; done

"$BORE" vhost "127.0.0.1:$OP" --subdomain $LABEL --id $LABEL \
  --to "127.0.0.1:$CP" --secret $SEC > "$F/provider.log" 2>&1 &
PIDS+=($!); disown

"$NODE" "$F/pageserver.js" h2 $H2P $HOST_IP "$F/key.pem" "$F/cert.pem" > "$F/h2.log" 2>&1 &
PIDS+=($!); disown
"$NODE" "$F/pageserver.js" h1 $H1P $HOST_IP "$F/key.pem" "$F/cert.pem" > "$F/h1.log" 2>&1 &
PIDS+=($!); disown
sleep 3

# --- one page load ---------------------------------------------------------
# curl -Z runs the transfers in parallel: with --http1.1 and --parallel-max 6 it
# reproduces a browser's 6-connection wave structure, and with --http2 it
# multiplexes them onto one connection. num_connects is summed and reported so
# the multiplexing is proved rather than assumed.
page(){ # <host:port> <ip:port> <curl-proto-flag> <parallel-max> -> "ms conns ok"
    local hp=$1 target=$2 proto=$3 pmax=$4 t0 t1 p
    # Every URL needs its OWN `output` entry. A single `-o` applies to the FIRST
    # url only and the rest go to stdout, which curl will not do in parallel —
    # measured: 31 assets served over ONE connection, i.e. the wave structure
    # under test silently disappeared, and 2 MiB of body landed in the -w file.
    local cfg="$F/urls.cfg"; : > "$cfg"
    while read -r p; do
        printf 'url = "https://%s%s"\noutput = "/dev/null"\n' "$hp" "$p" >> "$cfg"
    done < <(paths)
    local wf="$F/w.txt"; : > "$wf"
    t0=$(date +%s%N)
    nsx curl -sS -k "$proto" -Z --parallel-max "$pmax" \
        --connect-to "$hp:$target" \
        -w '%{num_connects} %{http_code}\n' \
        -K "$cfg" >> "$wf" 2>>"$F/curl.err"
    t1=$(date +%s%N)
    awk -v t0="$t0" -v t1="$t1" '{c+=$1; if($2=="200")ok++} END{
        printf "%.1f %d %d", (t1-t0)/1000000, c, ok+0}' "$wf"
}

median(){ printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END{print (NR%2)?a[(NR+1)/2]:(a[NR/2]+a[NR/2+1])/2}'; }

# Sets ARM_MED (median page-load ms) and logs its own line, so no caller has to
# separate the two from one stdout.
ARM_MED=0
arm(){ # <label> <host:port> <ip:port> <proto> <pmax>
    local label=$1 hp=$2 target=$3 proto=$4 pmax=$5 i r ms=() conns=? ok=?
    # One warm-up page, discarded: the tunnel's carrier pool, node's JIT and the
    # TLS session cache all settle on the first page and would otherwise be
    # charged to whichever arm happened to run first.
    page "$hp" "$target" "$proto" "$pmax" >/dev/null
    for i in $(seq "$RUNS"); do
        r=$(page "$hp" "$target" "$proto" "$pmax")
        ms+=("$(echo "$r" | awk '{print $1}')")
        conns=$(echo "$r" | awk '{print $2}')
        ok=$(echo "$r" | awk '{print $3}')
    done
    ARM_MED=$(median "${ms[@]}")
    log "$(printf "  %-10s page=%8s ms   conns=%-3s  ok=%s/%s" \
        "$label" "$ARM_MED" "$conns" "$ok" "$ASSETS")"
}

if [ "$SMALL_N" = 0 ]; then
    log "phase 07.1 — page load versus RTT, bulk only (1x $((LARGE/1024)) KiB, no small assets)"
elif [ "$LARGE" = 0 ]; then
    log "phase 07.1 — page load versus RTT, $ASSETS assets (${SMALL_N}x ~1 KiB, no large asset)"
else
    log "phase 07.1 — page load versus RTT, $ASSETS assets (${SMALL_N}x ~1 KiB + 1x $((LARGE/1024)) KiB)"
fi
log "runs per arm: $RUNS (median reported); client in netns $NS behind netem"
log ""
for rtt in $RTTS; do
    impair "$rtt"
    # Warm ARP first: the FIRST packet of a cold ping pays an ARP round trip at
    # the impaired delay too, and averaging it in reported 28 ms for a 21 ms
    # path (netem itself is accurate to ~0.15 ms, calibrated on a bare veth).
    nsx ping -c 1 -W 2 -q $HOST_IP >/dev/null 2>&1
    real=$(nsx ping -c 5 -i 0.2 -q $HOST_IP 2>/dev/null | awk -F/ '/rtt|round-trip/{printf "%.2f", $5}')
    log "===== RTT target ${rtt} ms (measured ${real:-?} ms) ====="
    arm tunnel-h1 "$LABEL.$DOM:$VHSP" "$HOST_IP:$VHSP" --http1.1 6; t=$ARM_MED
    arm direct-h1  "$LABEL.$DOM:$H1P"  "$HOST_IP:$H1P"  --http1.1 6; d1=$ARM_MED
    arm direct-h2  "$LABEL.$DOM:$H2P"  "$HOST_IP:$H2P"  --http2  100; d2=$ARM_MED
    log "$(awk -v t="$t" -v a="$d1" -v b="$d2" 'BEGIN{
        printf "  headroom: direct-h2 vs tunnel-h1 = %.2fx (%.1f ms saved);  protocol alone (h2 vs h1, no tunnel) = %.2fx;  tunnel cost = %.1f ms",
        (b>0? t/b : 0), t-b, (b>0? a/b : 0), t-a}')"
    log ""
done
impair 0
log "DONE — full output in $OUT"
