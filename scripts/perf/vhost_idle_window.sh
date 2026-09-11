#!/usr/bin/env bash
# Measure — and bound — the F-14 loss window: how long requests are lost after
# the UDP path of a live `--udp` vhost tunnel stops working.
#
# WHY THIS EXISTS
# ---------------
# The fallback from QUIC direct to the warm TCP relay is transparent and costs
# about one round trip, but only from the SECOND request onward. The first
# request issued after UDP dies is lost, because with the peer silent both
# `open_bi` and the `STREAM_READY` write succeed locally (neither needs a round
# trip once stream credit exists): the request is already committed to that
# stream and can only wait for the connection itself to time out. That wait is
# exactly `max_idle_timeout`, so the loss window is a tunable, not a mystery —
# and this script is what turns that claim into a number.
#
# WHY IT IS SOUND ON LOOPBACK
# ---------------------------
# Throughput and the injected-flush class false-pass on loopback and must be
# measured on a real network (see docs/VHOST_INJECTED_FLUSH_FIX.md). A DEADLINE
# does not: the mechanism under test is a quinn timer, and the staging
# measurement in docs/performance/ is reproduced here to the millisecond,
# counters included. Everything runs inside a ROOTLESS network namespace
# (`unshare -rn`), so the blackhole is a real kernel drop and NO SUDO IS NEEDED.
#
# USAGE
#   scripts/perf/vhost_idle_window.sh                 # the full matrix
#   scripts/perf/vhost_idle_window.sh ladder          # idle timeout vs loss window
#   scripts/perf/vhost_idle_window.sh server-only     # is a server-side override enough?
#   scripts/perf/vhost_idle_window.sh loss            # does a short deadline kill a lossy-but-healthy path?
#   scripts/perf/vhost_idle_window.sh one <label> [VAR=VALUE ...]
#
# Requires: a release build (`cargo build --release`), `jq`, `python3`,
# `iptables` and `unshare` with unprivileged user namespaces enabled
# (`sysctl kernel.unprivileged_userns_clone=1` on distributions that gate it).
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
#   $1 mode : blackhole | server-only | loss
#   $2 label
#   $3 idle ms for the SERVER ("" = shipped default)
#   $4 keepalive ms for the SERVER ("" = shipped default)
#   $5 loss % (mode=loss only)
# Prints a human transcript; the caller greps it.
# ---------------------------------------------------------------------------
scenario() {
    local mode=$1 label=$2 idle=$3 ka=$4 loss=${5:-0}
    local run; run=$(mktemp -d -p "${TMPDIR:-/tmp}" boreidle.XXXXXX)
    unshare -rn bash -s "$BIN" "$run" "$mode" "$label" "$idle" "$ka" "$loss" <<'INNER'
set -uo pipefail
BIN=$1; RUN=$2; MODE=$3; LABEL=$4; IDLE=$5; KA=$6; LOSS=$7
ip link set lo up

ORIGIN=18080; CTRL=17835; HTTP=18081; QUIC=17836
SEC=idlewindow; TOK=perf-idle-window-token-0123456789abcdef

mkdir -p "$RUN/www"; echo hello > "$RUN/www/index.html"
python3 -m http.server "$ORIGIN" --bind 127.0.0.1 --directory "$RUN/www" >"$RUN/origin.log" 2>&1 &
for _ in $(seq 40); do curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$ORIGIN/" 2>/dev/null && break; sleep 0.25; done

# The overrides go into the SERVER's environment only. QUIC negotiates
# max_idle_timeout as the minimum of the two advertised values, so this is also
# the realistic deployment shape: an operator controls the server, not every
# provider that connects to it.
env ${IDLE:+BORE_DIRECT_QUIC_IDLE_MS=$IDLE} ${KA:+BORE_DIRECT_QUIC_KEEPALIVE_MS=$KA} \
  "$BIN" server --secret "$SEC" --udp --vhost-base-domain t.local \
    --vhost-http-port "$HTTP" --vhost-quic-port "$QUIC" --control-port "$CTRL" \
    --admin-token "$TOK" >"$RUN/server.log" 2>&1 &
for _ in $(seq 40); do
  curl -fsS -m 1 -o /dev/null "http://127.0.0.1:$CTRL/admin/api/v1/config" \
      -H "Authorization: Bearer $TOK" 2>/dev/null && break
  sleep 0.25
done

# The provider always runs with a CLEAN environment: it keeps the shipped 10 s.
env -u BORE_DIRECT_QUIC_IDLE_MS -u BORE_DIRECT_QUIC_KEEPALIVE_MS \
  "$BIN" vhost "127.0.0.1:$ORIGIN" --subdomain a --id a --to "127.0.0.1:$CTRL" \
    --secret "$SEC" --udp >"$RUN/prov.log" 2>&1 &
for _ in $(seq 60); do
  curl -fsS -m 1 "http://127.0.0.1:$CTRL/admin/api/v1/vhost" -H "Authorization: Bearer $TOK" 2>/dev/null \
    | grep -q '"subdomain":"a"' && break
  sleep 0.25
done

adm(){ curl -fsS -m 3 "http://127.0.0.1:$CTRL/admin/api/v1/$1" -H "Authorization: Bearer $TOK" 2>/dev/null; }
ent(){ adm vhost | jq -r '.[]|select(.subdomain=="a")|"path=\(.current_path) opens=\(.direct_stream_opens) fb=\(.direct_fallbacks)"' 2>/dev/null; }
req(){ curl -s -o /dev/null -m 40 -H 'Host: a.t.local' -w '%{http_code} %{time_total}' "http://127.0.0.1:$HTTP/" 2>/dev/null; }
blackhole(){ local p; for p in dport sport; do
    iptables -A INPUT  -p udp --$p "$QUIC" -j DROP 2>/dev/null
    iptables -A OUTPUT -p udp --$p "$QUIC" -j DROP 2>/dev/null
  done; }

echo "  == $LABEL (mode=$MODE idle=${IDLE:-default} ka=${KA:-default} loss=${LOSS}%) =="
echo "  config: $(adm config | jq -r '"keepalive_ms=\(.direct_quic_keepalive_ms) idle_ms=\(.direct_quic_idle_ms)"')"
w1=$(req); w2=$(req)
echo "  warm-up: $w1 | $w2 | $(ent)"
case "$(ent)" in *path=direct*) ;; *) echo "  ABORT: the direct path never came up"; exit 3;; esac

case "$MODE" in
blackhole|server-only)
  blackhole
  echo "  --- UDP blackholed in both directions ---"
  for i in 1 2 3 4 5; do echo "    req#$i: $(req)   $(ent)"; done
  iptables -F 2>/dev/null
  echo "  --- blackhole cleared ---"
  for i in 1 2 3; do sleep 4; echo "    recovery#$i: $(req)   $(ent)"; done
  ;;
loss)
  tc qdisc add dev lo root netem loss "${LOSS}%" 2>/dev/null || echo "    (netem unavailable — result not meaningful)"
  echo "  --- ${LOSS}% loss on the whole loopback, 60 s of traffic ---"
  relayed=0
  for i in $(seq 12); do
    r=$(req); e=$(ent)
    case "$e" in *path=relay*) relayed=$((relayed+1));; esac
    echo "    t+$((i*5))s: $r   $e"
    sleep 5
  done
  tc qdisc del dev lo root 2>/dev/null
  echo "  --- relay samples: $relayed of 12 ---"
  ;;
esac
echo "  final: $(ent)"
pkill -P $$ 2>/dev/null
INNER
    local rc=$?
    rm -rf "$run"
    return $rc
}

# first-request loss time, in seconds, out of a transcript
window_of(){ grep -m1 'req#1:' | awk '{print $3}'; }

run_ladder() {
    echo "########## T-IDLE-LADDER: loss window as a function of the idle timeout"
    local spec lbl idle ka out w
    for spec in "default::" "idle6000:6000:" "idle4000:4000:" "idle2000:2000:" "idle4000ka1000:4000:1000"; do
        lbl=${spec%%:*}; rest=${spec#*:}; idle=${rest%%:*}; ka=${rest#*:}
        out=$(scenario blackhole "$lbl" "$idle" "$ka" 0) || { fail "T-IDLE-LADDER $lbl did not run"; continue; }
        echo "$out"
        w=$(printf '%s\n' "$out" | window_of)
        # The claim under test: the loss window IS the idle timeout.
        local want=${idle:-10000}
        if LC_ALL=C awk -v w="$w" -v want="$want" 'BEGIN{exit !(w*1000 > want-500 && w*1000 < want+1500)}'; then
            pass "T-IDLE-LADDER $lbl: first-request loss window ${w}s matches idle ${want}ms"
        else
            fail "T-IDLE-LADDER $lbl: window ${w}s does not match idle ${want}ms"
        fi
        # And the fallback itself must stay transparent from request #2.
        if printf '%s\n' "$out" | grep -q 'req#2: 200'; then
            pass "T-IDLE-LADDER $lbl: request #2 served on the warm relay"
        else
            fail "T-IDLE-LADDER $lbl: request #2 was not served"
        fi
    done
}

run_server_only() {
    echo "########## T-IDLE-SRVONLY: a server-side override is enough"
    local out w
    out=$(scenario server-only srvonly4000 4000 "" 0) || { fail "T-IDLE-SRVONLY did not run"; return; }
    echo "$out"
    w=$(printf '%s\n' "$out" | window_of)
    if LC_ALL=C awk -v w="$w" 'BEGIN{exit !(w > 3.5 && w < 5.5)}'; then
        pass "T-IDLE-SRVONLY: ${w}s window with a stock provider — the minimum is negotiated"
    else
        fail "T-IDLE-SRVONLY: window ${w}s, expected ~4s"
    fi
}

run_loss() {
    echo "########## T-IDLE-LOSS: a short deadline must not kill a healthy lossy path"
    local loss idle out relayed
    for loss in 10 30 50; do
        for idle in "" 4000; do
            out=$(scenario loss "loss${loss}_idle${idle:-default}" "$idle" "" "$loss") || {
                fail "T-IDLE-LOSS loss=$loss idle=${idle:-default} did not run"; continue; }
            echo "$out"
            relayed=$(printf '%s\n' "$out" | sed -n 's/.*relay samples: \([0-9]*\) of.*/\1/p')
            echo "    -> loss=${loss}% idle=${idle:-10000}ms relay samples=${relayed}/12"
        done
    done
    echo "  (this arm is a MEASUREMENT, not an assertion: the acceptable number of"
    echo "   fallbacks at a given loss level is a deployment decision, and a fallback"
    echo "   is not a failure — it is served on the warm relay in ~1 RTT.)"
}

case "${1:-all}" in
    ladder)      run_ladder ;;
    server-only) run_server_only ;;
    loss)        run_loss ;;
    one)         shift; lbl=${1:-adhoc}; shift || true
                 idle=""; ka=""
                 for kv in "$@"; do
                     case "$kv" in
                         BORE_DIRECT_QUIC_IDLE_MS=*) idle=${kv#*=} ;;
                         BORE_DIRECT_QUIC_KEEPALIVE_MS=*) ka=${kv#*=} ;;
                     esac
                 done
                 scenario blackhole "$lbl" "$idle" "$ka" 0 ;;
    all)         run_ladder; echo; run_server_only; echo; run_loss ;;
    *)           echo "unknown mode: $1" >&2; exit 2 ;;
esac

echo
echo "PASS: $PASS  FAIL: $FAIL"
[ "$FAIL" -eq 0 ]
