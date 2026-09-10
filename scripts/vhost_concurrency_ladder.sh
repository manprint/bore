#!/usr/bin/env bash
# Phase 04 of docs/plans/plan_VhostEnhancements/: the relay's concurrency tail
# (open question 7, §2.13 G8).
#
# Staging measured a fresh request taking 966 ms at 256 concurrent connections
# and 1436 ms at 512, on the TCP relay only — QUIC direct stayed at 14 ms. That
# is the relay's ONLY measured weakness, and it is a SCALE effect: S4 showed no
# head-of-line blocking with 6 slow readers, so do not conflate it with F-15.
#
# Phase 04 is diagnosis-first because three candidates have materially different
# fixes and the evidence does not distinguish them:
#   (a) --max-conns permit queueing        -> tail scales with the limit
#   (b) yamux substream open serialization -> tail divides by the carrier count
#   (c) carrier byte-stream head-of-line   -> tail only when connections carry data
#
# This harness runs all four experiments against a PRIVATE server, which is what
# lets --max-conns be varied at all (staging is frozen and its limit is 1024).
# The client, provider and origin share one network namespace with netem on the
# veth, so the impaired leg is the WAN and the RTT is ours to set — the same
# apparatus as scripts/vhost_bulk_isolation.sh, and the same reason.
#
# The metric is G8's own: N connections held open, then ONE fresh request on a
# NEW connection, timed end to end (TLS handshake included), median of three.
#
# Usage: sudo -n /abs/path/scripts/vhost_concurrency_ladder.sh [rtt-ms] [a|b|c|ladder|all]
# NOPASSWD sudo is per EXACT path; `sudo bash scripts/...` prompts.
set -uo pipefail

RTT="${1:-2}"
WHICH="${2:-all}"
# CPU list for the SERVER process (taskset). Staging is a 2-vCPU burstable
# t4g.micro; this workstation has 16 cores, so an unconstrained run here cannot
# tell "phase 03 fixed the tail" from "this box has eight times the headroom".
# Pass e.g. 0,1 to reproduce the staging core count.
CPUS="${3:-}"
# CPU list for the CLIENT side (the load generator and the fresh request), used
# only by experiment `e`. G8 ran 512 concurrent `curl` PROCESSES on a 2-vCPU VM
# and timed a newly forked 513th one; on a 16-core workstation that costs
# nothing, so reconstructing it faithfully means constraining the client too.
CLIENT_CPUS="${4:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BORE="$ROOT/target/release/bore"
NS=bore_cl
HOST_IP=10.80.0.1
NS_IP=10.80.0.2
VH=vh_clhost
VN=vh_clns
CP=17838
VHSP=19463
UP=19473        # unified: control port AND vhost HTTPS frontend on one port
QP=19464
OP=15064
SEC=ladderspikesecret
ADMTOK=ladderspikeadmintoken0123456789ab
DOM=lad.test
F="${BORE_PERF_OUT:-/tmp/bore-ladder-spike}"; mkdir -p "$F"
OUT="$F/results.txt"; : > "$OUT"

[ "$(id -u)" = 0 ] || { echo "must run as root (sudo -n <abs path>)" >&2; exit 1; }
[ -x "$BORE" ] || { echo "missing $BORE; build as your user: cargo build --release" >&2; exit 1; }
if [ -n "$(find "$ROOT/src" -newer "$BORE" -name '*.rs' -print -quit 2>/dev/null)" ]; then
    echo "$BORE is older than src/; rebuild as your user (NOT root): cargo build --release" >&2
    exit 1
fi

SRV=""; PROV=""; HOLDER=""
cleanup(){
    [ -n "$HOLDER" ] && { pkill -9 -P "$HOLDER" 2>/dev/null; kill -9 "$HOLDER" 2>/dev/null; }
    [ -n "$PROV" ] && kill -9 "$PROV" 2>/dev/null
    [ -n "$SRV" ] && kill -9 "$SRV" 2>/dev/null
    ip netns del $NS 2>/dev/null
    ip link del $VH 2>/dev/null
}
trap 'cleanup; exit 130' INT TERM
trap cleanup EXIT
log(){ echo "$*" | tee -a "$OUT"; }
nsx(){ ip netns exec $NS "$@"; }
adm(){
    if [ "${ADMPORT:-$CP}" = "$UP" ]; then
        curl -fsSk -m 10 -H "Authorization: Bearer $ADMTOK" "https://127.0.0.1:$UP/admin/api/v1/$1" 2>/dev/null
    else
        curl -fsS -m 10 -H "Authorization: Bearer $ADMTOK" "http://127.0.0.1:$CP/admin/api/v1/$1" 2>/dev/null
    fi
}

[ -f "$F/cert.pem" ] || openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
  -keyout "$F/key.pem" -out "$F/cert.pem" -subj "/CN=*.$DOM" \
  -addext "subjectAltName=DNS:*.$DOM,DNS:$DOM" 2>/dev/null

cat > "$F/vhost.yml" <<YML
base_domain: $DOM
mode: https
reservations: []
YML

# Holds N keep-alive connections open, each having completed one request, so a
# proxied connection (a yamux substream plus a --max-conns permit) exists per
# held connection. `slow` keeps reading a large body at ~20 KB/s, which is G8's
# load; `idle` finishes its small body and then holds — that pair is candidate
# (c)'s discriminator, since only `slow` puts bytes on the carrier.
cat > "$F/hold.py" <<'PY'
import asyncio, ssl, sys

host, port, ip, n, mode = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]), sys.argv[5]
path = "/stream/1048576" if mode == "slow" else "/b/1024"
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
established = 0


async def one():
    global established
    reader, writer = await asyncio.open_connection(ip, port, ssl=ctx, server_hostname=host)
    writer.write(
        f"GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n\r\n".encode()
    )
    await writer.drain()
    head = await reader.readuntil(b"\r\n\r\n")
    clen = 0
    for line in head.split(b"\r\n")[1:]:
        if line[:15].lower() == b"content-length:":
            clen = int(line.split(b":", 1)[1].strip())
    established += 1
    left = clen
    if mode == "slow":
        while left > 0:
            chunk = await reader.read(min(left, 20480))
            if not chunk:
                break
            left -= len(chunk)
            await asyncio.sleep(1.0)
    else:
        while left > 0:
            chunk = await reader.read(min(left, 65536))
            if not chunk:
                break
            left -= len(chunk)
    await asyncio.sleep(3600)


async def main():
    tasks = [asyncio.create_task(one()) for _ in range(n)]
    for _ in range(600):
        if established >= n:
            break
        await asyncio.sleep(0.1)
    print(f"established={established}", flush=True)
    await asyncio.gather(*tasks, return_exceptions=True)


asyncio.run(main())
PY

ip netns del $NS 2>/dev/null; ip link del $VH 2>/dev/null
ip netns add $NS
ip link add $VH type veth peer name $VN
ip link set $VN netns $NS
ip addr add $HOST_IP/24 dev $VH; ip link set $VH up
nsx ip addr add $NS_IP/24 dev $VN
nsx ip link set $VN up
nsx ip link set lo up
# 512 concurrent connections need more than the default backlog and conntrack
# is not in play here, but the namespace's own port range and file limits are.
nsx sysctl -q -w net.ipv4.ip_local_port_range="10000 60999" 2>/dev/null
ulimit -n 65536 2>/dev/null

half=$(awk -v r="$RTT" 'BEGIN{printf "%.3f", r/2}')
tc qdisc del dev $VH root 2>/dev/null
nsx tc qdisc del dev $VN root 2>/dev/null
if [ "$RTT" != 0 ]; then
    tc qdisc add dev $VH root netem delay "${half}ms"
    nsx tc qdisc add dev $VN root netem delay "${half}ms"
fi
nsx ping -c 1 -W 2 -q $HOST_IP >/dev/null 2>&1
REAL=$(nsx ping -c 5 -i 0.2 -q $HOST_IP 2>/dev/null | awk -F/ '/rtt|round-trip/{printf "%.2f", $5}')

nsx python3 "$ROOT/scripts/bench_origin.py" $OP > "$F/origin.log" 2>&1 &
ORIGIN=$!; disown
for i in $(seq 40); do nsx curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" 2>/dev/null && break; sleep 0.25; done

# The UNIFIED topology, which is what staging actually runs: the control port
# doubles as the vhost HTTPS frontend (443 = control + vhost + SSH gateway).
# Every inbound connection then takes a materially heavier accept path —
# `sshgw::demux_pre_tls`'s first-byte peek, `accept_tls_with_alpn`'s
# LazyConfigAcceptor, then `route_connection_known_http`'s own one-byte read —
# none of which exists on a standalone vhost frontend. A tail that only appears
# there is a property of the demux, not of the relay.
unified_up(){ # <sshgw:0|1>
    [ -n "$SRV" ] && { kill -9 "$SRV" 2>/dev/null; SRV=""; sleep 1; }
    local sshgw=$1 extra=() pin=()
    [ -n "$CPUS" ] && pin=(taskset -c "$CPUS")
    if [ "$sshgw" = 1 ]; then
        : > "$F/passwords"
        extra+=(--ssh-gateway --ssh-host-key-file "$F/ssh_host_key.pem" --ssh-passwords-file "$F/passwords")
    fi
    "${pin[@]}" "$BORE" server --secret $SEC --control-port $UP --bind-tunnels 0.0.0.0 \
      --admin-token $ADMTOK --max-carriers 16 --max-conns 1024 --udp \
      --cert-file "$F/cert.pem" --key-file "$F/key.pem" \
      --vhost-config "$F/vhost.yml" --vhost-https-port $UP --vhost-quic-port $QP \
      --vhost-cert-file "$F/cert.pem" --vhost-key-file "$F/key.pem" "${extra[@]}" \
      > "$F/server-unified-ssh$sshgw.log" 2>&1 &
    SRV=$!; disown
    local i
    for i in $(seq 80); do ss -ltn 2>/dev/null | grep -q ":$UP " && return 0; sleep 0.25; done
    return 1
}

server_up(){ # <max-conns>
    [ -n "$SRV" ] && { kill -9 "$SRV" 2>/dev/null; SRV=""; sleep 1; }
    local pin=()
    [ -n "$CPUS" ] && pin=(taskset -c "$CPUS")
    "${pin[@]}" "$BORE" server --secret $SEC --control-port $CP --bind-tunnels 0.0.0.0 \
      --admin-token $ADMTOK --max-carriers 16 --max-conns "$1" --udp \
      --vhost-config "$F/vhost.yml" --vhost-https-port $VHSP --vhost-quic-port $QP \
      --vhost-cert-file "$F/cert.pem" --vhost-key-file "$F/key.pem" \
      > "$F/server-mc$1-cpu${CPUS:-all}.log" 2>&1 &
    SRV=$!; disown
    local i
    for i in $(seq 80); do ss -ltn 2>/dev/null | grep -q ":$CP " && return 0; sleep 0.25; done
    return 1
}

LABEL=""
PORT=$VHSP      # the port the CLIENT talks to; differs in the unified topology
ADMPORT=$CP     # the port the admin API lives on
provider_up(){ # <carriers> <udp:0|1> [unified:0|1]
    [ -n "$PROV" ] && { kill -9 "$PROV" 2>/dev/null; PROV=""; sleep 2; }
    local c=$1 udp=$2 uni=${3:-0} extra=() to
    [ "$udp" = 1 ] && extra+=(--udp)
    if [ "$uni" = 1 ]; then
        PORT=$UP; ADMPORT=$UP
        # TLS control port with a self-signed cert: --insecure is exactly what it
        # is for. Nothing here reaches a real deployment.
        to="https://$HOST_IP:$UP"; extra+=(--insecure)
    else
        PORT=$VHSP; ADMPORT=$CP
        to="http://$HOST_IP:$CP"
    fi
    LABEL="lad$(date +%s%N | cut -c11-16)"
    nsx "$BORE" vhost "127.0.0.1:$OP" --subdomain "$LABEL" --id "$LABEL" \
        --to "$to" --secret $SEC --carriers "$c" "${extra[@]}" \
        > "$F/prov-$LABEL.log" 2>&1 &
    PROV=$!; disown
    local i
    for i in $(seq 80); do
        adm vhost | grep -q "\"$LABEL\"" && {
            nsx curl -sk -o /dev/null -m 20 --connect-to "$LABEL.$DOM:$PORT:$HOST_IP:$PORT" \
                "https://$LABEL.$DOM:$PORT/ping"
            return 0
        }
        kill -0 $PROV 2>/dev/null || return 1
        sleep 0.25
    done
    return 1
}

field(){ # <field>
    adm vhost | python3 -c "
import json,sys
label,field=sys.argv[1],sys.argv[2]
try: rows=json.load(sys.stdin)
except Exception: print('?'); raise SystemExit
for r in rows:
    if r.get('subdomain')==label: print(r.get(field,'?')); raise SystemExit
print('?')" "$LABEL" "$1"
}
metric(){ adm metrics | python3 -c "
import json,sys
try: print(json.load(sys.stdin).get(sys.argv[1],'?'))
except Exception: print('?')" "$1"; }

hold_start(){ # <n> <mode>
    [ -n "$HOLDER" ] && hold_stop
    nsx python3 "$F/hold.py" "$LABEL.$DOM" $PORT $HOST_IP "$1" "$2" > "$F/hold.out" 2>"$F/hold.err" &
    HOLDER=$!; disown
    local i
    for i in $(seq 200); do grep -q established "$F/hold.out" 2>/dev/null && break; sleep 0.25; done
    sleep 3
}
hold_stop(){
    [ -n "$HOLDER" ] && { pkill -9 -P "$HOLDER" 2>/dev/null; kill -9 "$HOLDER" 2>/dev/null; }
    HOLDER=""
    sleep 3
}

CLIENT_PIN=()   # set by experiment `e`
FRESH_CODES=""  # HTTP status of each fresh probe; "000" = curl never got a response.
                # A saturated --max-conns semaphore must be distinguishable from a
                # served request: both can be FAST, and only the code tells them apart.
fresh(){ # median of three fresh requests, each on a NEW connection (TLS included)
    local i t=() r
    FRESH_CODES=""
    for i in 1 2 3; do
        r="$(nsx "${CLIENT_PIN[@]}" curl -sk -o /dev/null -m 30 \
            --connect-to "$LABEL.$DOM:$PORT:$HOST_IP:$PORT" \
            -w '%{time_total} %{http_code}' "https://$LABEL.$DOM:$PORT/b/1024" 2>/dev/null || echo "99 000")"
        t+=("${r%% *}"); FRESH_CODES="$FRESH_CODES${r##* },"
    done
    printf '%s' "$FRESH_CODES" > "$F/fresh.codes"
    printf '%s\n' "${t[@]}" | sort -n | sed -n 2p | awk '{printf "%.0f", $1*1000}'
}

rung(){ # <n> <mode> <tag>
    local n=$1 mode=$2 tag=$3 held act fm codes rej
    hold_start "$n" "$mode"
    # Deliberate ordering: the rejection counter is read AFTER the fresh probes,
    # otherwise a probe the server drops for max-conns exhaustion is invisible
    # (bash expands command substitutions left to right).
    held=$(sed -n 's/established=//p' "$F/hold.out" | tail -1)
    act=$(field active)
    fm=$(fresh); codes=$(cat "$F/fresh.codes")
    rej=$(metric conn_rejections)
    log "$(printf "    %-26s n=%-4s held=%-4s active=%-4s rej=%-4s fresh=%s ms code=%s" \
        "$tag" "$n" "$held" "$act" "$rej" "$fm" "$codes")"
    hold_stop
}

log "phase 04 — the relay's concurrency tail, private server, netns client+provider"
log "RTT target ${RTT} ms (measured ${REAL:-?} ms); fresh request = new connection, TLS included, median of 3"
log "server CPU affinity: ${CPUS:-all $(nproc) cores}"
log ""

case "$WHICH" in
ladder|all)
    log "=== 4.1 the ladder, --carriers 1, G8's own load (slow readers) ==="
    server_up 1024 || { log "server failed"; exit 1; }
    for udp in 0 1; do
        tag=$([ $udp = 1 ] && echo direct-quic || echo relay-tcp)
        provider_up 1 $udp || { log "    $tag REGISTRATION FAILED"; continue; }
        log "  $tag (path after warm-up: $(field current_path))"
        for n in 16 64 256 512; do rung "$n" slow "$tag c=1"; done
    done
    log ""
    ;;&
b|all)
    log "=== 4.2 (b) yamux open serialization: 512 held, carriers 1 / 4 / 8 ==="
    server_up 1024 || { log "server failed"; exit 1; }
    for c in 1 4 8; do
        provider_up "$c" 0 || { log "    carriers=$c REGISTRATION FAILED"; continue; }
        rung 512 slow "relay-tcp c=$c live=$(field carriers)"
    done
    log ""
    ;;&
a|all)
    log "=== 4.2 (a) --max-conns permit queueing: 512 held, limit 512 / 1024 / 4096 ==="
    for mc in 512 1024 4096; do
        server_up "$mc" || { log "server failed"; continue; }
        provider_up 1 0 || { log "    max-conns=$mc REGISTRATION FAILED"; continue; }
        rung 512 slow "relay-tcp c=1 max-conns=$mc"
    done
    log ""
    ;;&
c|all)
    log "=== 4.2 (c) carrier byte-stream HOL: 512 held, carrying data versus idle ==="
    server_up 1024 || { log "server failed"; exit 1; }
    provider_up 1 0 || { log "    REGISTRATION FAILED"; exit 1; }
    rung 512 slow "relay-tcp c=1 ACTIVE"
    rung 512 idle "relay-tcp c=1 IDLE  "
    log ""
    ;;&
e|all)
    log "=== 4.2 (e) G8's literal client shape: N concurrent curl PROCESSES, client CPU-bounded ==="
    log "    client pinned to ${CLIENT_CPUS:-all cores}; the load is 512 forked curls, not one event loop"
    server_up 1024 || { log "server failed"; exit 1; }
    provider_up 1 0 || { log "    REGISTRATION FAILED"; exit 1; }
    [ -n "$CLIENT_CPUS" ] && CLIENT_PIN=(taskset -c "$CLIENT_CPUS")
    for n in 64 256 512; do
        CURLS=()
        for i in $(seq "$n"); do
            nsx "${CLIENT_PIN[@]}" curl -sk -o /dev/null --limit-rate 20k -m 60 \
                --connect-to "$LABEL.$DOM:$PORT:$HOST_IP:$PORT" \
                "https://$LABEL.$DOM:$PORT/stream/1048576" >/dev/null 2>&1 &
            CURLS+=($!); disown
        done
        sleep 8
        act=$(field active); fm=$(fresh); codes=$(cat "$F/fresh.codes")
        log "$(printf "    %-26s n=%-4s active=%-4s rej=%-4s fresh=%s ms code=%s" \
            "curl-processes" "$n" "$act" "$(metric conn_rejections)" "$fm" "$codes")"
        for p in "${CURLS[@]}"; do kill -9 "$p" 2>/dev/null; done
        CURLS=()
        sleep 4
    done
    CLIENT_PIN=()
    log ""
    ;;&
u|all)
    log "=== 4.2 (d) the UNIFIED control-port topology, which is what staging runs ==="
    for sshgw in 0 1; do
        unified_up $sshgw || { log "    unified sshgw=$sshgw server failed"; continue; }
        provider_up 1 0 1 || { log "    unified sshgw=$sshgw REGISTRATION FAILED: $(tail -2 "$F/prov-$LABEL.log")"; continue; }
        for n in 64 256 512; do rung "$n" slow "unified ssh-gateway=$sshgw"; done
    done
    PORT=$VHSP; ADMPORT=$CP
    log ""
    ;;&
esac

tc qdisc del dev $VH root 2>/dev/null
nsx tc qdisc del dev $VN root 2>/dev/null
kill -9 $ORIGIN 2>/dev/null
log "DONE — full output in $OUT"
