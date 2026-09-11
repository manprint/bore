#!/usr/bin/env bash
# Public-tunnel direct-path deadlines, loss window and recovery.
#
# The public-tunnel twin of `vhost_idle_window.sh`, and it exists because the
# two paths were NOT symmetric. Three separate asymmetries were measured with
# this harness and then fixed; each arm below is the gate for one of them.
#
#   T-PUB-DEADLINE / T-PUB-LADDER
#       `Server::serve_tunnel` opened its direct QUIC stream with NO deadline,
#       while the vhost relay bounds the same operation with
#       `vhost::direct_open_timeout()` (3 s, `BORE_DIRECT_OPEN_TIMEOUT_MS`) and
#       the SSH jump host with `SSH_DIRECT_OPEN_TIMEOUT`. The deadline does NOT
#       shorten a total-blackout loss window (see T-PUB-IDLE for why) — it
#       bounds an open that genuinely BLOCKS, which is what happens when the
#       peer's concurrent-stream limit is already taken. Unbounded, such a
#       connection waited for whoever held the stream; bounded, it is on the
#       warm relay at the deadline. Measured 1.00 / 3.00 / 20.00 s against
#       deadlines of 1000 / 3000 / 20000 ms.
#
#   T-PUB-IDLE
#       The loss window of a TOTAL UDP blackout is the QUIC `max_idle_timeout`,
#       exactly, and no open deadline can change that: on a silent peer both
#       `open_bi` and the `STREAM_READY` write succeed LOCALLY (neither needs a
#       round trip once stream credit exists), so the connection is already
#       committed to that stream and can only wait for the connection itself to
#       die. The lever is therefore `BORE_DIRECT_QUIC_IDLE_MS`, and this arm is
#       what turns that claim into a number on the public path.
#
#   T-PUB-RECOVER
#       After the blackout cleared, the direct path never came back: the client
#       sent one `PublicUdpRenew` and `Server::serve_tunnel` had no arm reading
#       the control substream at all, so the request was never read and the
#       tunnel stayed on the relay for the rest of its life (observed still
#       degraded 100 s later). The vhost and SSH-jump control loops both
#       answered their own renewal already.
#
#   T-PUB-QUICPORT
#       `--vhost-quic-port` was applied only inside the vhost configuration
#       block, so a public-tunnels-only server ignored it and tried the 443
#       default — root-only, and taken by any real HTTPS service on the host.
#
#   T-PUB-RELAYPATH
#       `current_path` is derived from the tunnel's `PublicDirectEntry`, which
#       is created only for a `--udp` tunnel, so every ORDINARY public tunnel
#       — the overwhelming majority — reported `current_path: "unknown"`
#       (P-10). "Unknown" has to mean one thing, "a --udp tunnel has not
#       proxied a connection yet", or the field cannot be used to spot a
#       tunnel that negotiated the direct path and has since fallen back. The
#       unit test pins the derivation; this arm proves the whole server →
#       admin-API path reports `relay` for a tunnel that has served five real
#       connections, which is the form the defect was actually found in.
#
#   T-PUB-FDBUDGET
#   T-PUB-UDPBUF
#       `--max-conns` promises a GRACEFUL bound: at capacity one connection is
#       refused, `conn_rejections` counts it and everything else keeps serving.
#       That promise only holds while the process can still open a descriptor
#       per admitted connection. Measured in the field (campaign 2026-09-11,
#       §11): the staging container ran `--max-conns 1024` with a soft
#       `RLIMIT_NOFILE` of 1024, and at ~976 held connections through ONE
#       public tunnel the server logged `failed to accept tunnel connection
#       err=No file descriptors available (os error 24)` every 100 ms and
#       answered nothing on its control port for about half a minute — with
#       `conn_rejections` still at 0, because the configured bound was
#       unreachable by construction. EMFILE lands on `accept()` for every
#       listener, so one tunnel's concurrency took the admin API down with it.
#       A process may raise its own soft limit up to its hard limit without
#       privilege, so the server now does that at startup and says what it did;
#       when the hard limit is itself too low it warns with both remedies. This
#       arm runs a real server under a deliberately tiny limit and reads back
#       `/proc/<pid>/limits`, because the only claim worth gating is that the
#       process' limit actually MOVED.
#
# WHY IT IS SOUND ON LOOPBACK
#   Throughput and the injected-flush class false-pass on loopback and must be
#   measured on a real network. DEADLINES and RECOVERY do not: the mechanisms
#   are quinn timers, a tokio timeout and a control-plane message. Everything
#   runs inside a ROOTLESS network namespace (`unshare -rn`), so the blackhole
#   is a real kernel drop and NO SUDO IS NEEDED.
#
# USAGE
#   scripts/perf/public_idle_window.sh              # the full matrix
#   scripts/perf/public_idle_window.sh deadline     # blocked open -> bounded fallback
#   scripts/perf/public_idle_window.sh ladder       # fallback tracks the deadline
#   scripts/perf/public_idle_window.sh idle         # blackout loss window == idle timeout
#   scripts/perf/public_idle_window.sh recover      # the direct path comes back
#   scripts/perf/public_idle_window.sh quicport     # --vhost-quic-port with no vhost
#   scripts/perf/public_idle_window.sh healthy      # a short deadline must not break a lossy path
#   scripts/perf/public_idle_window.sh relaypath    # a relay-only tunnel reports "relay", not "unknown"
#   scripts/perf/public_idle_window.sh fdbudget     # --max-conns is reconciled with RLIMIT_NOFILE
#   scripts/perf/public_idle_window.sh udpbuf       # the shared QUIC endpoint runs on a tuned socket
#
# Requires: a release build (`cargo build --release`), `jq`, `python3`,
# `iptables` and `unshare` with unprivileged user namespaces enabled.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="${BORE_BIN:-$ROOT/target/release/bore}"

[ -x "$BIN" ] || { echo "no bore binary at $BIN — run: cargo build --release" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
unshare -rn true 2>/dev/null || {
    echo "unprivileged network namespaces are unavailable; this harness needs them" >&2
    exit 2
}

PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "PASS: $*"; }
fail(){ FAIL=$((FAIL+1)); echo "FAIL: $*"; }

# ---------------------------------------------------------------------------
# One scenario, entirely inside its own netns.
#   $1 mode     : blackhole | recover | blocked | healthy
#   $2 label
#   $3 open deadline ms for the SERVER ("" = shipped default)
#   $4 idle ms for the SERVER          ("" = shipped default)
#   $5 loss %   (mode=healthy only)
# Prints a human transcript; the caller greps it.
# ---------------------------------------------------------------------------
scenario() {
    local mode=$1 label=$2 deadline=$3 idle=$4 loss=${5:-0}
    local run; run=$(mktemp -d -p "${TMPDIR:-/tmp}" borepub.XXXXXX)
    unshare -rn bash -s "$BIN" "$run" "$mode" "$label" "$deadline" "$idle" "$loss" <<'INNER'
set -uo pipefail
BIN=$1; RUN=$2; MODE=$3; LABEL=$4; DEADLINE=$5; IDLE=$6; LOSS=$7
ip link set lo up

ORIGIN=18090; CTRL=17845; QUIC=17846; PUB=19090
SEC=pubwindow; TOK=perf-public-window-token-0123456789abcdef

# A two-route origin. `/` answers immediately; `/slow` holds the connection
# open for 25 s, which is how the "peer's stream limit is taken" case is
# produced without any packet manipulation at all.
python3 - "$ORIGIN" >"$RUN/origin.log" 2>&1 <<'PY' &
import socket, sys, threading, time
port = int(sys.argv[1])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port))
srv.listen(128)
OK = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
def serve(conn, slow):
    try:
        conn.recv(65536)
        if slow:
            time.sleep(25)
        conn.sendall(OK)
    except Exception:
        pass
    finally:
        try: conn.close()
        except Exception: pass
while True:
    conn, _ = srv.accept()
    try:
        head = conn.recv(4096, socket.MSG_PEEK)
    except Exception:
        conn.close(); continue
    threading.Thread(target=serve, args=(conn, b"/slow" in head), daemon=True).start()
PY
for _ in $(seq 40); do curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$ORIGIN/" 2>/dev/null && break; sleep 0.25; done

# `--udp-max-streams 1` is what makes the `blocked` mode deterministic: the
# client then advertises room for exactly ONE concurrent bidi stream, so while
# the slow connection holds it the server's next `open_bi` genuinely blocks.
STREAMS=""
[ "$MODE" = blocked ] && STREAMS="--udp-max-streams 1"

# Both overrides go into the SERVER's environment only: the server is the side
# that opens the direct stream for a public tunnel, and QUIC negotiates
# max_idle_timeout as the minimum of the two advertised values. That is also
# the realistic deployment shape — an operator controls the server, not every
# client that connects to it.
#
# --vhost-base-domain is passed ONLY so --vhost-quic-port is honoured on a
# build from BEFORE that flag was made unconditional; T-PUB-QUICPORT is the arm
# that tests the public-only shape on purpose.
env ${DEADLINE:+BORE_DIRECT_OPEN_TIMEOUT_MS=$DEADLINE} ${IDLE:+BORE_DIRECT_QUIC_IDLE_MS=$IDLE} \
  "$BIN" server --secret "$SEC" --udp --min-port "$PUB" --max-port "$PUB" \
    --vhost-base-domain t.local --vhost-http-port 18091 \
    --vhost-quic-port "$QUIC" --control-port "$CTRL" \
    --admin-token "$TOK" $STREAMS >"$RUN/server.log" 2>&1 &
for _ in $(seq 40); do
  curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$CTRL/admin/api/v1/config" \
      -H "Authorization: Bearer $TOK" 2>/dev/null && break
  sleep 0.25
done

# `relaypath` is the ONE mode whose client does NOT ask for --udp: it is the
# ordinary relay-only public tunnel, and what it gates is the REPORTING of
# that tunnel, not its transport (P-10). Every other mode needs the direct
# path and therefore needs the flag.
UDPFLAG="--udp"
[ "$MODE" = relaypath ] && UDPFLAG=""

# The client always runs with a CLEAN environment: it keeps the shipped values.
env -u BORE_DIRECT_OPEN_TIMEOUT_MS -u BORE_DIRECT_QUIC_IDLE_MS \
  "$BIN" local "$ORIGIN" --to "127.0.0.1:$CTRL" --port "$PUB" \
    --secret "$SEC" $UDPFLAG >"$RUN/client.log" 2>&1 &
if [ "$MODE" = relaypath ]; then
  for _ in $(seq 80); do
    curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$PUB/" 2>/dev/null && break
    sleep 0.25
  done
else
  for _ in $(seq 80); do
    grep -q 'public QUIC direct carrier established' "$RUN/server.log" && break
    sleep 0.25
  done
fi

adm(){ curl -fsS -m 3 "http://127.0.0.1:$CTRL/admin/api/v1/$1" -H "Authorization: Bearer $TOK" 2>/dev/null; }
ent(){ adm tunnels | jq -r --argjson p "$PUB" \
        '.[]|select(.public_port==$p)|"path=\(.current_path // "-") opens=\(.direct_stream_opens // "-") fb=\(.direct_fallbacks // "-") pool=\(.direct_pool // "-")"' 2>/dev/null; }
req(){ curl -s -o /dev/null -m 40 -w '%{http_code} %{time_total}' "http://127.0.0.1:$PUB/" 2>/dev/null; }
blackhole(){ local p; for p in dport sport; do
    iptables -A INPUT  -p udp --$p "$QUIC" -j DROP 2>/dev/null
    iptables -A OUTPUT -p udp --$p "$QUIC" -j DROP 2>/dev/null
  done; }

echo "  == $LABEL (mode=$MODE open_deadline=${DEADLINE:-default} idle=${IDLE:-default} loss=${LOSS}%) =="
echo "  config: $(adm config | jq -r '"keepalive_ms=\(.direct_quic_keepalive_ms) idle_ms=\(.direct_quic_idle_ms) max_streams=\(.udp_max_streams)"')"
w1=$(req); w2=$(req)
echo "  warm-up: $w1 | $w2 | $(ent)"
if [ "$MODE" != relaypath ] \
   && ! grep -q 'public QUIC direct carrier established' "$RUN/server.log"; then
  echo "  ABORT: the direct path never came up"; exit 3
fi

case "$MODE" in
blocked)
  # Take the peer's only stream slot, then ask for a second connection.
  curl -s -o /dev/null -m 40 "http://127.0.0.1:$PUB/slow" & slow=$!
  sleep 2
  echo "    blocked#1: $(req)   $(ent)"
  kill "$slow" 2>/dev/null
  ;;
blackhole)
  blackhole
  echo "  --- UDP blackholed in both directions ---"
  for i in 1 2 3 4 5; do echo "    req#$i: $(req)   $(ent)"; done
  iptables -F 2>/dev/null
  echo "  --- blackhole cleared ---"
  ;;
recover)
  blackhole
  echo "  --- UDP blackholed in both directions ---"
  echo "    req#1: $(req)   $(ent)"
  echo "    req#2: $(req)   $(ent)"
  iptables -F 2>/dev/null
  echo "  --- blackhole cleared at t=0; watching for the direct path to return ---"
  t0=$(date +%s); back=""
  while :; do
    n=$(( $(date +%s) - t0 )); [ "$n" -ge 120 ] && break
    e=$(ent); echo "    t+${n}s $e"
    case "$e" in *pool=1*) back=$n; break;; esac
    sleep 5
  done
  if [ -n "$back" ]; then
    echo "    RECOVERED after ${back}s"
    echo "    post-recovery request: $(req)   $(ent)"
  else
    echo "    NOT RECOVERED within 120s"
  fi
  ;;
relaypath)
  # A public tunnel that never asked for --udp has exactly ONE possible
  # transport and the server knows it with certainty. Reporting "unknown"
  # for it (P-10) read as "the server cannot tell", which is false, and it
  # made the one field that CAN express a degraded --udp tunnel useless.
  echo "  --- relay-only public tunnel: 5 requests, reading the reported path ---"
  served=0
  for i in $(seq 5); do
    r=$(req); code=${r%% *}
    [ "$code" = "200" ] && served=$((served+1))
    echo "    #$i: $r   $(ent)"
    sleep 1
  done
  echo "  --- served: $served of 5 ---"
  ;;
healthy)
  # The loss goes on the QUIC PORT ONLY, not on the whole loopback.
  #
  # What this arm claims is that a short DIRECT-OPEN deadline (3 s here) must
  # not break a path that is merely lossy: the open either succeeds or the
  # connection falls back to the warm TCP relay, and either way it is served.
  # The lossy thing in that claim is the UDP path between server and client.
  #
  # The first version put netem on `dev lo root`, which also degraded the
  # CLIENT'S DIAL OF THE LOCAL ORIGIN. That dial is bounded by
  # `connect_with_timeout` at `NETWORK_TIMEOUT` (3 s), so at 30% loss a dial
  # whose SYN is dropped past the third retry exceeds the bound, the client
  # closes the substream and the public connection ends with no response.
  # MEASURED, not reasoned: 2 failures in 4 cells as `000 3.205574` /
  # `000 4.084193`, then 3 in one cell with the client log kept —
  # `WARN could not connect to localhost:18090` at exactly the three failing
  # timestamps, with `direct_stream_opens` incrementing and `fb=0` throughout.
  # The direct path was healthy every time and the ORIGIN dial was not: the
  # client was working as designed and the ARM was flaking on something it
  # does not claim. Whole-path loss belongs to the netem matrix stage
  # (`staging/pub/vm_pub_netem.sh`) against a real server, not here.
  #
  # `prio` with an all-2 priomap sends every packet to band 1:3 (plain pfifo);
  # two u32 filters divert UDP carrying the QUIC port as source or destination
  # into band 1:1, the only band that has netem on it.
  tcok=1
  tc qdisc add dev lo root handle 1: prio bands 3 \
     priomap 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2>/dev/null || tcok=0
  tc qdisc add dev lo parent 1:1 handle 10: netem loss "${LOSS}%" 2>/dev/null || tcok=0
  for d in dport sport; do
    tc filter add dev lo parent 1: protocol ip prio 1 u32 \
       match ip protocol 17 0xff match ip $d "$QUIC" 0xffff flowid 1:1 2>/dev/null || tcok=0
  done
  [ "$tcok" = 1 ] || echo "    (tc unavailable — result not meaningful)"
  echo "  --- ${LOSS}% loss on the QUIC port only, 12 connections ---"
  served=0
  for i in $(seq 12); do
    r=$(req); code=${r%% *}
    [ "$code" = "200" ] && served=$((served+1))
    echo "    t+$((i*2))s: $r   $(ent)"
    sleep 2
  done
  # A silently-empty netem class would make the whole arm vacuous — the same
  # shape as H-7, where a harness measured nothing and said nothing. So report
  # what the class actually carried and let the caller assert on it.
  echo "  netem class: $(tc -s qdisc show dev lo | tr '\n' ' ' \
       | sed -n 's/.*netem.*Sent [0-9]* bytes \([0-9]*\) pkt (dropped \([0-9]*\).*/sent_pkt=\1 dropped=\2/p')"
  tc qdisc del dev lo root 2>/dev/null
  echo "  --- served: $served of 12 ---"
  ;;
esac
echo "  final: $(ent)"
echo "  open timeouts logged: $(grep -c 'public QUIC direct open timed out' "$RUN/server.log")"
pkill -P $$ 2>/dev/null
INNER
    local rc=$?
    # `BORE_PUB_KEEP_RUN=1` keeps the server and client logs of this arm. The
    # netns is gone by now but the run directory is a HOST path, so the logs
    # survive it — which is the only way to diagnose an arm that fails once in
    # forty-eight requests under 30% loss.
    if [ -n "${BORE_PUB_KEEP_RUN:-}" ]; then
        echo "  logs kept in $run"
    else
        rm -rf "$run"
    fi
    return $rc
}

secs_of(){ grep -m1 "$1" | awk '{print $3}'; }

run_deadline() {
    echo "########## T-PUB-DEADLINE: a BLOCKED direct open falls back at the deadline"
    local out w
    out=$(scenario blocked blocked_dl3000 3000 "" 0) || { fail "T-PUB-DEADLINE did not run"; return; }
    echo "$out"
    w=$(printf '%s\n' "$out" | secs_of 'blocked#1:')
    if LC_ALL=C awk -v w="$w" 'BEGIN{exit !(w > 2.5 && w < 5.0)}'; then
        pass "T-PUB-DEADLINE: served on the warm relay after ${w}s, bounded by the 3 s deadline"
    else
        fail "T-PUB-DEADLINE: ${w}s — the open is not bounded by the deadline"
    fi
    if printf '%s\n' "$out" | grep -q 'blocked#1: 200'; then
        pass "T-PUB-DEADLINE: the connection was SERVED, not dropped"
    else
        fail "T-PUB-DEADLINE: the connection was not served"
    fi
    if printf '%s\n' "$out" | grep -q 'open timeouts logged: [1-9]'; then
        pass "T-PUB-DEADLINE: the fallback is logged, never silent"
    else
        fail "T-PUB-DEADLINE: no timeout line in the server log"
    fi
}

run_ladder() {
    echo "########## T-PUB-LADDER: the fallback tracks the open deadline"
    local dl out w
    for dl in 1000 3000 8000; do
        out=$(scenario blocked "blocked_dl${dl}" "$dl" "" 0) || { fail "T-PUB-LADDER $dl did not run"; continue; }
        echo "$out"
        w=$(printf '%s\n' "$out" | secs_of 'blocked#1:')
        if LC_ALL=C awk -v w="$w" -v want="$dl" 'BEGIN{exit !(w*1000 > want-400 && w*1000 < want+2000)}'; then
            pass "T-PUB-LADDER dl=${dl}ms: fell back after ${w}s"
        else
            fail "T-PUB-LADDER dl=${dl}ms: fell back after ${w}s, which does not track the deadline"
        fi
    done
}

run_idle() {
    echo "########## T-PUB-IDLE: a TOTAL blackout's loss window is the idle timeout"
    local spec lbl idle out w want
    for spec in "default:" "idle6000:6000" "idle4000:4000" "idle2000:2000"; do
        lbl=${spec%%:*}; idle=${spec#*:}
        out=$(scenario blackhole "$lbl" "" "$idle" 0) || { fail "T-PUB-IDLE $lbl did not run"; continue; }
        echo "$out"
        w=$(printf '%s\n' "$out" | secs_of 'req#1:')
        want=${idle:-10000}
        if LC_ALL=C awk -v w="$w" -v want="$want" 'BEGIN{exit !(w*1000 > want-500 && w*1000 < want+1500)}'; then
            pass "T-PUB-IDLE $lbl: first-connection loss window ${w}s matches idle ${want}ms"
        else
            fail "T-PUB-IDLE $lbl: window ${w}s does not match idle ${want}ms"
        fi
        if printf '%s\n' "$out" | grep -q 'req#2: 200'; then
            pass "T-PUB-IDLE $lbl: connection #2 served on the warm relay"
        else
            fail "T-PUB-IDLE $lbl: connection #2 was not served"
        fi
    done
}

run_recover() {
    echo "########## T-PUB-RECOVER: the direct path returns after the blackout clears"
    local out back
    out=$(scenario recover recover_idle4000 "" 4000 0) || { fail "T-PUB-RECOVER did not run"; return; }
    echo "$out"
    back=$(printf '%s\n' "$out" | sed -n 's/.*RECOVERED after \([0-9]*\)s.*/\1/p')
    if [ -n "$back" ]; then
        pass "T-PUB-RECOVER: the QUIC direct carrier came back ${back}s after the path healed"
    else
        fail "T-PUB-RECOVER: the tunnel stayed on the relay for the whole 120 s window"
    fi
    if printf '%s\n' "$out" | grep -q 'post-recovery request: 200'; then
        pass "T-PUB-RECOVER: it serves on the recovered path"
    else
        fail "T-PUB-RECOVER: no served request after recovery"
    fi
}

# ---------------------------------------------------------------------------
# T-PUB-QUICPORT: `--vhost-quic-port` must apply to a server with NO vhost.
# ---------------------------------------------------------------------------
run_quicport() {
    echo "########## T-PUB-QUICPORT: the shared QUIC port applies without a vhost config"
    local run; run=$(mktemp -d -p "${TMPDIR:-/tmp}" borepubqp.XXXXXX)
    local out
    out=$(unshare -rn bash -s "$BIN" "$run" <<'INNER'
set -uo pipefail
BIN=$1; RUN=$2
ip link set lo up
ORIGIN=18092; CTRL=17847; QUIC=17848; PUB=19092
SEC=pubqp; TOK=perf-public-quicport-token-0123456789abcdef
mkdir -p "$RUN/www"; echo hello > "$RUN/www/index.html"
python3 -m http.server "$ORIGIN" --bind 127.0.0.1 --directory "$RUN/www" >"$RUN/origin.log" 2>&1 &
for _ in $(seq 40); do curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$ORIGIN/" 2>/dev/null && break; sleep 0.25; done
# NO --vhost-base-domain, NO --vhost-config: a public-tunnels-only server.
"$BIN" server --secret "$SEC" --udp --min-port "$PUB" --max-port "$PUB" \
    --vhost-quic-port "$QUIC" --control-port "$CTRL" \
    --admin-token "$TOK" >"$RUN/server.log" 2>&1 &
for _ in $(seq 40); do
  curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$CTRL/admin/api/v1/config" \
      -H "Authorization: Bearer $TOK" 2>/dev/null && break
  sleep 0.25
done
"$BIN" local "$ORIGIN" --to "127.0.0.1:$CTRL" --port "$PUB" --secret "$SEC" --udp \
    >"$RUN/client.log" 2>&1 &
for _ in $(seq 60); do
  grep -q 'public QUIC direct carrier established' "$RUN/server.log" && break
  sleep 0.25
done
echo "LISTEN: $(grep -o 'shared QUIC direct endpoint listening port=[0-9]*' "$RUN/server.log" | head -1)"
echo "WANT: shared QUIC direct endpoint listening port=$QUIC"
echo "CARRIERS: $(grep -c 'public QUIC direct carrier established' "$RUN/server.log")"
echo "REQ: $(curl -s -o /dev/null -m 10 -w '%{http_code}' "http://127.0.0.1:$PUB/")"
pkill -P $$ 2>/dev/null
INNER
    )
    rm -rf "$run"
    echo "$out" | sed 's/^/    /'
    local got want
    got=$(printf '%s\n' "$out" | sed -n 's/^LISTEN: //p')
    want=$(printf '%s\n' "$out" | sed -n 's/^WANT: //p')
    if [ -n "$got" ] && [ "$got" = "$want" ]; then
        pass "T-PUB-QUICPORT: the endpoint bound the requested port with no vhost configured"
    else
        fail "T-PUB-QUICPORT: got '${got:-<nothing>}', wanted '$want' — the flag was ignored"
    fi
    if [ "$(printf '%s\n' "$out" | sed -n 's/^CARRIERS: //p')" -ge 1 ] 2>/dev/null; then
        pass "T-PUB-QUICPORT: a public --udp tunnel established a direct carrier on it"
    else
        fail "T-PUB-QUICPORT: no direct carrier came up"
    fi
    if [ "$(printf '%s\n' "$out" | sed -n 's/^REQ: //p')" = "200" ]; then
        pass "T-PUB-QUICPORT: the tunnel serves"
    else
        fail "T-PUB-QUICPORT: the tunnel does not serve"
    fi
}

run_healthy() {
    echo "########## T-PUB-HEALTHY: a short deadline must not break a healthy (lossy) path"
    local loss out served sent drop
    # `BORE_PUB_HEALTHY_LOSS` narrows the cells, which is what makes a
    # one-in-four failure investigable: the interesting cell can be run on its
    # own, repeatedly, with `BORE_PUB_KEEP_RUN=1` keeping its logs.
    # `BORE_PUB_HEALTHY_DEADLINE` varies the open deadline for this arm.
    # NOTE what it does NOT do: it does not red-check the two clean-path
    # assertions below. Tried and measured — at `BORE_PUB_HEALTHY_DEADLINE=1`
    # the clean cell still reports `open timeouts logged: 0` and `fb=0`,
    # because on loopback with a healthy peer `open_stream` plus
    # `write_stream_ready` complete in well under a millisecond (and 0 is
    # rejected by `direct_open_timeout`, which requires ms > 0). That is worth
    # knowing on its own: the 3 s default is three thousand times the local
    # cost of the operation it bounds. The positive control for those two
    # greps is T-PUB-DEADLINE and T-PUB-LADDER above, which run the SAME
    # server and the SAME log format and do produce
    # `open timeouts logged: [1-9]` plus a relay fallback — so the predicates
    # are known to be able to see a nonzero value.
    local dl="${BORE_PUB_HEALTHY_DEADLINE:-3000}"
    for loss in ${BORE_PUB_HEALTHY_LOSS:-0 10 30}; do
        out=$(scenario healthy "healthy_loss${loss}" "$dl" "" "$loss") || {
            fail "T-PUB-HEALTHY loss=$loss did not run"; continue; }
        echo "$out"
        served=$(printf '%s\n' "$out" | sed -n 's/.*served: \([0-9]*\) of.*/\1/p')
        if [ "${served:-0}" -eq 12 ]; then
            pass "T-PUB-HEALTHY loss=${loss}%: all 12 connections served"
        else
            fail "T-PUB-HEALTHY loss=${loss}%: only ${served:-0} of 12 served"
        fi
        # The loss must be PROVEN to have reached the path, or a green arm
        # means nothing. netem's own counters are the only witness: packets
        # through the class, and drops above zero on the two lossy cells.
        sent=$(printf '%s\n' "$out" | sed -n 's/.*sent_pkt=\([0-9]*\).*/\1/p')
        drop=$(printf '%s\n' "$out" | sed -n 's/.*dropped=\([0-9]*\).*/\1/p')
        if [ "${sent:-0}" -gt 0 ]; then
            pass "T-PUB-HEALTHY loss=${loss}%: the QUIC path really went through netem (${sent} pkt)"
        else
            fail "T-PUB-HEALTHY loss=${loss}%: netem carried no packets — the arm measured nothing"
        fi
        if [ "$loss" != 0 ] && [ "${drop:-0}" -lt 1 ]; then
            fail "T-PUB-HEALTHY loss=${loss}%: netem dropped nothing at ${loss}% loss"
        fi
        # The complement of T-PUB-DEADLINE, and the assertion with real
        # discriminating power here: that arm proves the deadline FIRES and is
        # logged when the peer cannot answer an open, this one proves it does
        # NOT fire when nothing is wrong. A deadline that fired on a clean path
        # would put every connection on the relay while reporting success, so
        # the served count alone cannot see it.
        if [ "$loss" = 0 ]; then
            if printf '%s\n' "$out" | grep -q 'open timeouts logged: 0'; then
                pass "T-PUB-HEALTHY loss=0%: the open deadline never fired on a clean path"
            else
                fail "T-PUB-HEALTHY loss=0%: the open deadline fired with nothing wrong"
            fi
            if printf '%s\n' "$out" | grep -q 'final: path=direct .* fb=0'; then
                pass "T-PUB-HEALTHY loss=0%: no connection fell back to the relay"
            else
                fail "T-PUB-HEALTHY loss=0%: something fell back on a clean path"
            fi
        fi
    done
}

run_relaypath() {
    echo "########## T-PUB-RELAYPATH: a relay-only tunnel reports the relay, not \"unknown\""
    local out
    out=$(scenario relaypath relay_only "" "" 0) || { fail "T-PUB-RELAYPATH did not run"; return; }
    echo "$out"
    if printf '%s\n' "$out" | grep -q '^  --- served: 5 of 5'; then
        pass "T-PUB-RELAYPATH: every request was served over the relay"
    else
        fail "T-PUB-RELAYPATH: not every request was served"
    fi
    # The FINAL entry is the one that matters: it is read after five proxied
    # connections, so "unknown" there cannot be excused as "nothing has been
    # proxied yet".
    local final
    final=$(printf '%s\n' "$out" | grep '^  final: ' | tail -1)
    case "$final" in
      *path=relay*) pass "T-PUB-RELAYPATH: $final" ;;
      *)            fail "T-PUB-RELAYPATH: $final (expected path=relay)" ;;
    esac
    if printf '%s\n' "$out" | grep -q '^  final: .*opens=0'; then
        pass "T-PUB-RELAYPATH: no direct stream was ever opened"
    else
        fail "T-PUB-RELAYPATH: the tunnel opened a direct stream it never asked for"
    fi
}


# ---------------------------------------------------------------------------
# T-PUB-FDBUDGET: the connection bound must be reconciled with the process
# descriptor limit, and the reconciliation must be VISIBLE.
#
# Each case starts a real server inside its own netns under a chosen
# `ulimit -n`, then reads the limit back out of `/proc/<pid>/limits`: the log
# line alone would only prove the server talked about it.
# ---------------------------------------------------------------------------
fdcase() { # <soft> <hard|-> <max-conns>  -> prints "soft=<n> log=<matched line>"
    local soft=$1 hard=$2 conns=$3
    unshare -rn bash -s "$BIN" "$soft" "$hard" "$conns" <<'INNER'
set -uo pipefail
BIN=$1; SOFT=$2; HARD=$3; CONNS=$4
ip link set lo up
RUN=$(mktemp -d)
# `ulimit -n` with neither -S nor -H sets BOTH limits, which would make every
# case here look hard-capped (it did, the first time). Set them explicitly.
# The SOFT limit goes first: `setrlimit` refuses a hard limit below the
# current soft limit (EINVAL), so lowering the ceiling before the floor
# silently leaves the ceiling where it was — which made the hard-capped case
# read as a successful raise the first time.
ulimit -Sn "$SOFT" 2>/dev/null
[ "$HARD" = "-" ] || ulimit -Hn "$HARD" 2>/dev/null
"$BIN" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port 17851 --min-port 19100 --max-port 19110 \
    --max-conns "$CONNS" --secret fdbudget >"$RUN/server.log" 2>&1 &
PID=$!
for _ in $(seq 60); do
    grep -q 'server listening' "$RUN/server.log" 2>/dev/null && break
    sleep 0.2
done
echo "soft=$(awk '/Max open files/{print $4}' /proc/$PID/limits)"
echo "log=$(grep -oE '(raised the file-descriptor limit[^ ]*|could not raise the file-descriptor limit|still short)' "$RUN/server.log" | head -1)"
kill -9 $PID 2>/dev/null
INNER
}

run_fdbudget() {
    echo "=== T-PUB-FDBUDGET: --max-conns vs RLIMIT_NOFILE ==="

    # (a) the measured staging shape: soft limit equal to the bound, a high
    #     hard limit. The server must raise itself to bound + headroom.
    local out soft log
    out=$(fdcase 1024 - 1024)
    soft=$(printf '%s\n' "$out" | sed -n 's/^soft=//p')
    log=$(printf '%s\n' "$out" | sed -n 's/^log=//p')
    echo "  soft limit after startup: ${soft:-?}  (log: ${log:-none})"
    if [ "${soft:-0}" -ge 1280 ]; then
        pass "T-PUB-FDBUDGET: --max-conns 1024 raised the soft limit to $soft (>= 1024+256)"
    else
        fail "T-PUB-FDBUDGET: soft limit stayed at ${soft:-?}; --max-conns 1024 cannot be honoured"
    fi
    case "$log" in
        raised*) pass "T-PUB-FDBUDGET: the server said what it did" ;;
        *)       fail "T-PUB-FDBUDGET: the raise was silent (log: ${log:-none})" ;;
    esac

    # (b) a hard limit BELOW the need: the server must raise to the ceiling and
    #     warn, never pretend the bound is honoured.
    out=$(fdcase 200 300 1024)
    soft=$(printf '%s\n' "$out" | sed -n 's/^soft=//p')
    log=$(printf '%s\n' "$out" | sed -n 's/^log=//p')
    echo "  hard-capped case: soft=${soft:-?} (log: ${log:-none})"
    if [ "${soft:-0}" = 300 ]; then
        pass "T-PUB-FDBUDGET: raised to the hard ceiling (300) when that is all there is"
    else
        fail "T-PUB-FDBUDGET: expected the soft limit at the 300 ceiling, got ${soft:-?}"
    fi
    case "$log" in
        *short*) pass "T-PUB-FDBUDGET: an unsatisfiable bound is warned about, not hidden" ;;
        *)       fail "T-PUB-FDBUDGET: an unsatisfiable bound produced no warning (log: ${log:-none})" ;;
    esac

    # (c) a limit that is already sufficient must be left ALONE and must not
    #     produce an advisory: a server that talks about a non-problem trains
    #     its operator to ignore the line that matters.
    out=$(fdcase 4096 - 128)
    soft=$(printf '%s\n' "$out" | sed -n 's/^soft=//p')
    log=$(printf '%s\n' "$out" | sed -n 's/^log=//p')
    echo "  already-sufficient case: soft=${soft:-?} (log: ${log:-none})"
    if [ "${soft:-0}" = 4096 ] && [ -z "$log" ]; then
        pass "T-PUB-FDBUDGET: a sufficient limit is untouched and silent"
    else
        fail "T-PUB-FDBUDGET: a sufficient limit was changed or talked about (soft=${soft:-?} log=${log:-none})"
    fi
}


# ---------------------------------------------------------------------------
# T-PUB-UDPBUF: the server's shared QUIC endpoint must run on a TUNED UDP
# socket (P-13).
#
# It did not. `configure_udp_socket_buffers` — whose own doc comment warns that
# an untuned socket caps a QUIC flow at roughly buffer/RTT — was the caller's
# job, and the one caller that builds the server's shared endpoint never called
# it. That endpoint receives the direct-path bytes of every vhost, public and
# ssh-jump tunnel in the process, and it ran on `net.core.rmem_default`
# (212992 bytes) while every other UDP socket in the file asked for 16 MiB.
#
# Read from the KERNEL, never from the log: `ss -uapm` reports the socket's own
# `skmem:(...,rb<N>,...)`. A server that merely SAYS it configured its buffers
# is exactly the evidence P-12 rejected. The assertion is against the kernel
# default rather than a fixed number, because the effective size is whatever
# `net.core.rmem_max` allows on the host running the gate.
# ---------------------------------------------------------------------------
udpbufcase() { # -> prints "rb=<n> tb=<n> default=<n> log=<matched line>"
    unshare -rn bash -s "$BIN" <<'INNER'
set -uo pipefail
BIN=$1
ip link set lo up
RUN=$(mktemp -d)
"$BIN" server --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --control-port 17853 --min-port 19120 --max-port 19130 \
    --udp --vhost-quic-port 17854 --secret udpbuf >"$RUN/server.log" 2>&1 &
PID=$!
for _ in $(seq 60); do
    grep -q 'QUIC direct endpoint listening' "$RUN/server.log" 2>/dev/null && break
    sleep 0.2
done
# The netns is fresh, so this is the only UDP socket on that port.
line=$(ss -uapm 2>/dev/null | grep -A1 ':17854' | tr '\n' ' ')
echo "rb=$(printf '%s' "$line" | sed -n 's/.*rb\([0-9]*\).*/\1/p')"
echo "tb=$(printf '%s' "$line" | sed -n 's/.*,tb\([0-9]*\).*/\1/p')"
echo "default=$(cat /proc/sys/net/core/rmem_default 2>/dev/null)"
echo "log=$(grep -oE '(configured UDP socket buffers|UDP socket buffer clamped below request)' "$RUN/server.log" | head -1)"
kill -9 $PID 2>/dev/null
INNER
}

run_udpbuf() {
    echo "=== T-PUB-UDPBUF: the shared QUIC endpoint's socket buffers ==="
    local out rb tb dflt log
    out=$(udpbufcase)
    rb=$(printf '%s\n' "$out" | sed -n 's/^rb=//p')
    tb=$(printf '%s\n' "$out" | sed -n 's/^tb=//p')
    dflt=$(printf '%s\n' "$out" | sed -n 's/^default=//p')
    log=$(printf '%s\n' "$out" | sed -n 's/^log=//p')
    echo "  kernel view: rb=${rb:-?} tb=${tb:-?}  (net.core.rmem_default=${dflt:-?})"
    echo "  server said: ${log:-nothing}"

    if [ -z "${rb:-}" ]; then
        # `ss` missing, or a kernel that does not report skmem: say so rather
        # than passing. A gate that cannot see its subject has not tested it.
        fail "T-PUB-UDPBUF: could not read the socket's skmem (is ss installed?)"
        return
    fi
    # The kernel reports twice what was asked for, so "tuned" is anything
    # clearly above the untouched default; 4x the default is far outside the
    # noise and does not hardcode this host's rmem_max.
    if [ "${rb:-0}" -gt $(( ${dflt:-212992} * 4 )) ]; then
        pass "T-PUB-UDPBUF: receive buffer raised to $rb (default ${dflt:-?})"
    else
        fail "T-PUB-UDPBUF: receive buffer is $rb, at or near the untouched default ${dflt:-?} — the shared endpoint is the RECEIVING side of every download"
    fi
    if [ "${tb:-0}" -gt $(( ${dflt:-212992} * 4 )) ]; then
        pass "T-PUB-UDPBUF: send buffer raised to $tb"
    else
        fail "T-PUB-UDPBUF: send buffer is $tb, at or near the default ${dflt:-?}"
    fi
    # Whichever way it went, it must be on the record: a clamp the operator
    # cannot see is how this defect survived a whole campaign.
    case "$log" in
        configured*|*clamped*) pass "T-PUB-UDPBUF: the server reported what it got" ;;
        *) fail "T-PUB-UDPBUF: the buffers were configured silently (log: ${log:-none})" ;;
    esac
}

case "${1:-all}" in
    deadline) run_deadline ;;
    relaypath) run_relaypath ;;
    fdbudget) run_fdbudget ;;
    udpbuf)   run_udpbuf ;;
    ladder)   run_ladder ;;
    idle)     run_idle ;;
    recover)  run_recover ;;
    quicport) run_quicport ;;
    healthy)  run_healthy ;;
    all)      run_deadline; echo; run_ladder; echo; run_idle; echo
              run_recover; echo; run_quicport; echo; run_healthy; echo
              run_relaypath; echo; run_fdbudget; echo; run_udpbuf ;;
    *)        echo "unknown mode: $1" >&2; exit 2 ;;
esac

echo
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
