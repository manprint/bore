#!/usr/bin/env bash
# Phase 03.5 of docs/plans/plan_VhostEnhancements/: does phase 03 actually
# isolate small requests from bulk, and what do the carrier settings buy?
#
# F-15 measured small-request p50 collapsing while a bulk transfer was in
# flight. Phase 03 answered it three ways — bulk-aware carrier selection
# (03.2), an adaptive carrier pool (`--carriers 0`, 03.3) and QUIC per-stream
# demotion on the direct path (03.4) — and DEC-VE7 set the target at a 7-15 ms
# p50 under bulk rather than the unloaded 2.5 ms.
#
# Apparatus. This mirrors the campaign's own topology instead of loopback,
# because the leg that queues is the CARRIER leg (server<->provider): bulk bytes
# fill the yamux carrier and a small request's substream waits behind them. So
# the provider, the origin AND the measuring client all live in one network
# namespace and the bore server lives on the host, with netem on the veth
# standing in for the WAN. That is exactly the campaign's shape (VM ran client +
# provider + origin, the server was remote), with the 29 % control drift of a
# shared staging box removed and the RTT under our control.
#
#   netns bore_iso                        host
#   +-------------------------------+     +--------------------+
#   | bench_origin.py (netns-local) |     | bore server        |
#   | bore vhost  --carriers N      |====>|  vhost TLS edge    |
#   | oha / curl (the measurement)  | WAN |  admin API on lo   |
#   +-------------------------------+netem+--------------------+
#
# Reported per case: small-request p50/p95 with 0, 1 and 2 bulk transfers in
# flight, the live carrier count and the published carrier_target (so adaptive
# growth is observed, not assumed), and direct_stream_opens (so a `--udp` case
# that silently served over the relay cannot be read as a direct-path result —
# a campaign pitfall that cost a retracted finding).
#
# Usage: sudo -n /abs/path/scripts/vhost_bulk_isolation.sh [rtt-list] [secs]
# NOPASSWD sudo is per EXACT path; `sudo bash scripts/...` prompts.
set -uo pipefail

RTTS="${1:-2 21}"
DUR="${2:-6}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BORE="$ROOT/target/release/bore"
NS=bore_iso
HOST_IP=10.79.0.1
NS_IP=10.79.0.2
VH=vh_isohost
VN=vh_isons
CP=17837
VHSP=19453
QP=19454
OP=15063
SEC=isospikesecret
ADMTOK=isospikeadmintoken0123456789abcdef
DOM=iso.test
F="${BORE_PERF_OUT:-/tmp/bore-iso-spike}"; mkdir -p "$F"
OUT="$F/results.txt"; : > "$OUT"

[ "$(id -u)" = 0 ] || { echo "must run as root (sudo -n <abs path>)" >&2; exit 1; }
[ -x "$BORE" ] || { echo "missing $BORE; build as your user: cargo build --release" >&2; exit 1; }
if [ -n "$(find "$ROOT/src" -newer "$BORE" -name '*.rs' -print -quit 2>/dev/null)" ]; then
    echo "$BORE is older than src/; rebuild as your user (NOT root): cargo build --release" >&2
    exit 1
fi
OHA="${OHA:-$(command -v oha 2>/dev/null || true)}"
if [ -z "$OHA" ] && [ -n "${SUDO_USER:-}" ]; then
    for c in "/home/$SUDO_USER/.cargo/bin/oha" "/home/$SUDO_USER/oha"; do
        [ -x "$c" ] && OHA=$c && break
    done
fi
[ -x "${OHA:-}" ] || { echo "need oha (pass OHA=/path/to/oha)" >&2; exit 1; }

PIDS=(); BULK=()
cleanup(){
    for p in "${BULK[@]:-}"; do kill -9 "$p" 2>/dev/null; done
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
    ip netns del $NS 2>/dev/null
    ip link del $VH 2>/dev/null
}
trap 'cleanup; exit 130' INT TERM
trap cleanup EXIT
log(){ echo "$*" | tee -a "$OUT"; }
nsx(){ ip netns exec $NS "$@"; }
adm(){ curl -fsS -m 10 -H "Authorization: Bearer $ADMTOK" "http://127.0.0.1:$CP/admin/api/v1/$1" 2>/dev/null; }

[ -f "$F/cert.pem" ] || openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes \
  -keyout "$F/key.pem" -out "$F/cert.pem" -subj "/CN=*.$DOM" \
  -addext "subjectAltName=DNS:*.$DOM,DNS:$DOM" 2>/dev/null

ip netns del $NS 2>/dev/null; ip link del $VH 2>/dev/null
ip netns add $NS
ip link add $VH type veth peer name $VN
ip link set $VN netns $NS
ip addr add $HOST_IP/24 dev $VH; ip link set $VH up
nsx ip addr add $NS_IP/24 dev $VN
nsx ip link set $VN up
nsx ip link set lo up

impair(){ # <rtt-ms>  — RTT/2 each side, so the round trip is symmetric
    local half; half=$(awk -v r="$1" 'BEGIN{printf "%.3f", r/2}')
    tc qdisc del dev $VH root 2>/dev/null
    nsx tc qdisc del dev $VN root 2>/dev/null
    [ "$1" = 0 ] && return 0
    tc qdisc add dev $VH root netem delay "${half}ms"
    nsx tc qdisc add dev $VN root netem delay "${half}ms"
}

cat > "$F/vhost.yml" <<YML
base_domain: $DOM
mode: https
reservations: []
YML

"$BORE" server --secret $SEC --control-port $CP --bind-tunnels 0.0.0.0 \
  --admin-token $ADMTOK --max-carriers 8 --udp \
  --vhost-config "$F/vhost.yml" --vhost-https-port $VHSP --vhost-quic-port $QP \
  --vhost-cert-file "$F/cert.pem" --vhost-key-file "$F/key.pem" \
  > "$F/server.log" 2>&1 &
PIDS+=($!); disown
for i in $(seq 60); do ss -ltn 2>/dev/null | grep -q ":$CP " && break; sleep 0.25; done

nsx python3 "$ROOT/scripts/bench_origin.py" $OP > "$F/origin.log" 2>&1 &
PIDS+=($!); disown
for i in $(seq 40); do nsx curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" 2>/dev/null && break; sleep 0.25; done

CT=(--connect-to)   # filled per label

provider_up(){ # <label> <carriers> <udp:0|1>
    local l=$1 c=$2 udp=$3 extra=()
    [ "$udp" = 1 ] && extra+=(--udp)
    nsx "$BORE" vhost "127.0.0.1:$OP" --subdomain "$l" --id "$l" \
        --to "http://$HOST_IP:$CP" --secret $SEC --carriers "$c" "${extra[@]}" \
        > "$F/prov-$l.log" 2>&1 &
    PROV=$!; PIDS+=($PROV); disown
    local i
    for i in $(seq 80); do
        adm vhost | grep -q "\"$l\"" && return 0
        kill -0 $PROV 2>/dev/null || return 1
        sleep 0.25
    done
    return 1
}

entry(){ # <label> <field>
    adm vhost | python3 -c "
import json,sys
label,field=sys.argv[1],sys.argv[2]
try: rows=json.load(sys.stdin)
except Exception: print('?'); raise SystemExit
for r in rows:
    if r.get('subdomain')==label: print(r.get(field,'?')); raise SystemExit
print('?')" "$1" "$2"
}

small_lat(){ # <label> -> "p50 p95 rps"
    local l=$1
    nsx timeout $((DUR+20)) "$OHA" -z "${DUR}s" -c 1 --no-tui --insecure \
        --connect-to "$l.$DOM:$VHSP:$HOST_IP:$VHSP" --output-format json \
        "https://$l.$DOM:$VHSP/b/1024" 2>>"$F/oha.err" \
      | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print('? ? ?'); raise SystemExit
m=d['metrics']['latency_ms']; print('%.2f %.2f %.0f'%(m['p50'],m['p95'],d['summary']['requestsPerSec']))"
}

start_bulk(){ # <label> <n>
    local l=$1 n=$2 i
    for i in $(seq "$n"); do
        nsx bash -c "while :; do curl -sk -o /dev/null -m 40 --connect-to '$l.$DOM:$VHSP:$HOST_IP:$VHSP' 'https://$l.$DOM:$VHSP/stream/1073741824' || sleep 0.2; done" \
            > /dev/null 2>&1 &
        BULK+=($!); disown
    done
    [ "$n" -gt 0 ] && sleep 3
    return 0
}
stop_bulk(){
    local p
    for p in "${BULK[@]:-}"; do
        pkill -9 -P "$p" 2>/dev/null
        kill -9 "$p" 2>/dev/null
    done
    BULK=()
    sleep 2
}

case_run(){ # <transport-label> <carriers-arg> <udp:0|1>
    local tl=$1 c=$2 udp=$3 l base
    l="iso$(date +%s%N | cut -c10-16)"
    provider_up "$l" "$c" "$udp" || { log "    $tl c=$c REGISTRATION FAILED: $(tail -2 "$F/prov-$l.log")"; return 1; }
    # warm the tunnel and, for --udp, let the direct pool establish
    nsx curl -sk -o /dev/null -m 20 --connect-to "$l.$DOM:$VHSP:$HOST_IP:$VHSP" "https://$l.$DOM:$VHSP/ping"
    sleep 2
    local d0 d1 path
    d0=$(entry "$l" direct_stream_opens)
    nsx curl -sk -o /dev/null -m 20 --connect-to "$l.$DOM:$VHSP:$HOST_IP:$VHSP" "https://$l.$DOM:$VHSP/b/102400"
    d1=$(entry "$l" direct_stream_opens)
    path=$(entry "$l" current_path)
    local n r
    for n in 0 1 2; do
        start_bulk "$l" "$n"
        r=$(small_lat "$l")
        log "$(printf "    %-12s carriers=%-4s bulk=%d  p50=%-8s p95=%-8s rps=%-6s live=%s target=%s path=%s" \
            "$tl" "$c" "$n" $(echo "$r" | awk '{print $1, $2, $3}') \
            "$(entry "$l" carriers)" "$(entry "$l" carrier_target)" "$(entry "$l" current_path)")"
        stop_bulk
    done
    log "        direct_stream_opens ${d0:-?} -> ${d1:-?}, path after a 100 KiB request: ${path:-?}"
    kill -9 $PROV 2>/dev/null
    sleep 2
}

log "phase 03.5 — small-request latency with bulk in flight, private server, netns client+provider"
log "window ${DUR}s per point, oha -c 1 keep-alive, bulk = looped /stream over the same tunnel"
log ""
for rtt in $RTTS; do
    impair "$rtt"
    nsx ping -c 1 -W 2 -q $HOST_IP >/dev/null 2>&1
    real=$(nsx ping -c 5 -i 0.2 -q $HOST_IP 2>/dev/null | awk -F/ '/rtt|round-trip/{printf "%.2f", $5}')
    log "===== RTT target ${rtt} ms (measured ${real:-?} ms) ====="
    case_run relay-tcp   1 0
    case_run relay-tcp   4 0
    case_run relay-auto  0 0
    case_run direct-quic 1 1
    case_run direct-quic 4 1
    log ""
done
impair 0
log "DONE — full output in $OUT"
