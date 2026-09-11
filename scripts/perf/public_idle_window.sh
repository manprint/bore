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

# The client always runs with a CLEAN environment: it keeps the shipped values.
env -u BORE_DIRECT_OPEN_TIMEOUT_MS -u BORE_DIRECT_QUIC_IDLE_MS \
  "$BIN" local "$ORIGIN" --to "127.0.0.1:$CTRL" --port "$PUB" \
    --secret "$SEC" --udp >"$RUN/client.log" 2>&1 &
for _ in $(seq 80); do
  grep -q 'public QUIC direct carrier established' "$RUN/server.log" && break
  sleep 0.25
done

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
if ! grep -q 'public QUIC direct carrier established' "$RUN/server.log"; then
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
healthy)
  tc qdisc add dev lo root netem loss "${LOSS}%" 2>/dev/null || echo "    (netem unavailable — result not meaningful)"
  echo "  --- ${LOSS}% loss on the whole loopback, 12 connections ---"
  served=0
  for i in $(seq 12); do
    r=$(req); code=${r%% *}
    [ "$code" = "200" ] && served=$((served+1))
    echo "    t+$((i*2))s: $r   $(ent)"
    sleep 2
  done
  tc qdisc del dev lo root 2>/dev/null
  echo "  --- served: $served of 12 ---"
  ;;
esac
echo "  final: $(ent)"
echo "  open timeouts logged: $(grep -c 'public QUIC direct open timed out' "$RUN/server.log")"
pkill -P $$ 2>/dev/null
INNER
    local rc=$?
    rm -rf "$run"
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
    local loss out served
    for loss in 0 10 30; do
        out=$(scenario healthy "healthy_loss${loss}" 3000 "" "$loss") || {
            fail "T-PUB-HEALTHY loss=$loss did not run"; continue; }
        echo "$out"
        served=$(printf '%s\n' "$out" | sed -n 's/.*served: \([0-9]*\) of.*/\1/p')
        if [ "${served:-0}" -eq 12 ]; then
            pass "T-PUB-HEALTHY loss=${loss}%: all 12 connections served"
        else
            fail "T-PUB-HEALTHY loss=${loss}%: only ${served:-0} of 12 served"
        fi
    done
}

case "${1:-all}" in
    deadline) run_deadline ;;
    ladder)   run_ladder ;;
    idle)     run_idle ;;
    recover)  run_recover ;;
    quicport) run_quicport ;;
    healthy)  run_healthy ;;
    all)      run_deadline; echo; run_ladder; echo; run_idle; echo
              run_recover; echo; run_quicport; echo; run_healthy ;;
    *)        echo "unknown mode: $1" >&2; exit 2 ;;
esac

echo
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
