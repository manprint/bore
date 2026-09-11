#!/usr/bin/env bash
# Secret-tunnel RESOURCE hunt: descriptors, resident memory, wedges, reaping.
#
# WHAT THIS IS FOR
#   `scripts/secret_netns_test.sh` proves the secret path is CORRECT (echo
#   round-trips, no zombie rows, relay/direct labelling, carrier accounting).
#   It does not prove the path is BOUNDED. A tunnel that answers every request
#   while leaking one descriptor or one allocation per proxied connection is
#   indistinguishable from a healthy one for the length of a test — and
#   distinguishable from it only in production, hours later, as `EMFILE` on
#   every listener of the process (P-12) or as an OOM kill.
#
#   Every arm here is therefore a RATE measurement, not a level: the same churn
#   is run several times and the LAST phase's growth is what is asserted. That is
#   what makes it robust against the two things that make a naive
#   before/after leak test lie — allocator warm-up (a heap that grows once and
#   then never again reads as a leak) and steady-state caches (the winning-pair
#   cache, the carrier pool, quinn's own pools).
#
# THE ARMS
#   T-SECLEAK-CHURN-RELAY / T-SECLEAK-CHURN-DIRECT
#       Four identical phases of 200 proxied connections at bounded
#       concurrency, through a secret tunnel on the relay and then on the QUIC
#       direct path. Asserts the LAST phase adds no descriptors and no
#       measurable resident memory to the server, the provider or the consumer
#       — a leak is linear in connections and so keeps the per-phase delta
#       CONSTANT, while warm-up and saturating caches decay toward zero. The
#       whole series is printed, so the decay is visible and not assumed.
#       Both transports are run because they leak differently by construction:
#       the relay path allocates a yamux substream per connection on three
#       processes, the direct path a QUIC bidi stream on two of them plus a
#       punch socket that is re-bound periodically.
#
#   T-SECLEAK-STALL / T-SECLEAK-STALL-DIRECT
#       The deadlock arm, run on both transports because they wedge
#       differently: the relay carries every connection as a yamux substream
#       through the server, the direct path as a QUIC bidi stream on ONE
#       connection between the peers, where an abrupt abort leaves a half-open
#       stream on a shared congestion controller. 16 connections held open with no bytes moving, six
#       overlapping bulk waves, and every wave killed mid-flight with SIGKILL
#       so the tunnel sees abrupt client aborts rather than clean closes. Then
#       ONE fresh round-trip, timed. A wedge — a yamux waker lost to a split
#       stream, a heartbeat write parked on a full window (P-9), a carrier
#       picked and never opened — shows up here and nowhere else: the tunnel
#       keeps looking registered while a new connection never completes. The
#       admin API is polled in the same breath, because the failure that
#       matters most is one tunnel's concurrency taking the whole process down
#       with it. On a stall the arm dumps every thread's backtrace with `gdb`
#       into the run directory instead of only reporting a number.
#
#   T-SECLEAK-REAP
#       The secret control-liveness reaper (see CLAUDE.md) releases an admin
#       row when a peer stops answering. This arm SIGSTOPs the consumer — the
#       wedged-but-TCP-alive shape that `send`/`recv` cannot see, not a process
#       kill, which any implementation survives — and asserts the row goes away
#       AND the server's descriptor count comes back down. Releasing the row
#       while holding the connection's descriptors would be a leak wearing a
#       clean admin page. Takes ~70 s: the shipped timeout is 60 s and there is
#       no env override for it on the secret registry, on purpose.
#
#   T-SECLEAK-UPGRADE
#       The observability arm, and the one that found a real defect. A secret
#       tunnel's transport reaches the admin API through exactly one channel —
#       the consumer's own `SecretPathReport` (S-1) — and that report used to be
#       sent ONCE, at registration. This arm registers the consumer with NO
#       provider present, so the first negotiation necessarily fails and the
#       tunnel starts on the relay; then it starts the provider and waits for
#       the live session to upgrade itself. Before the fix the consumer logged
#       `path=direct-udp` on every subsequent connection while `current_path`
#       answered `relay` with a `path_reason` that no longer applied, for the
#       rest of the session. Read from the server (P-12: the log proves the
#       client TALKED about a path, the API proves the server BELIEVES it).
#
#   T-SECLEAK-UPGRADE-CAP
#       The same shape, asking the other question: not "does the upgrade
#       happen" but "how long does the WORST case last". The provider is held
#       back for 90 s (`SECLEAK_LATE`), which is past several steps of the
#       upgrade's exponential backoff, and the arm then measures the seconds
#       from "the direct path became possible" to "the admin API says direct".
#       That number is the backoff CAP by construction, which is why the cap is
#       what the arm asserts (<= 75 s). It red-checks the S-8 change: with the
#       previous 256 s cap the retry grid ran 2, 4, 8, 16, 32, 64, 128, so a
#       provider appearing at t=90 s went unnoticed until t=190 s — four
#       minutes of relay on a network that had been fine for three of them.
#
#   T-SECLEAK-UDPBUF
#       P-13 says every QUIC endpoint is built over a socket whose buffers were
#       configured, and that the CONSTRUCTORS are what guarantee it. On the
#       secret path both endpoints are hole-punched sockets built by
#       `client_endpoint`, i.e. the one funnel — so this reads `rb`/`tb` out of
#       `ss -uapm` for the live punch socket, which is the KERNEL's view and
#       not the log's (P-12's rule). The host's `net.core.rmem_max` is a global
#       ceiling and not per-netns, so the assertion is "it asked for more than
#       `rmem_default`", never a specific number.
#
# WHY IT IS SOUND WITHOUT A REAL NETWORK
#   Throughput is not measured here and must not be: it false-passes on
#   loopback. Descriptors, resident memory, a wedge and a socket option are
#   properties of the PROCESS and the KERNEL, and a namespace on one host
#   measures them exactly.
#
#   The direct arm still needs candidates that are not loopback, because
#   loopback is not a routable hole-punch candidate. It gets them from a
#   `dummy` interface inside the namespace: both peers advertise an explicit
#   `--udp-candidate` on a fixed `--nat-udp-preferred-port`, with STUN off, so
#   the direct path is established by construction rather than discovered. That
#   is deliberate — this harness is not testing traversal (the NAT matrix in
#   `scripts/udp_nat_netns_test.sh` is), it is testing what the direct path
#   COSTS once it exists.
#
# EVERYTHING RUNS INSIDE A ROOTLESS NETWORK NAMESPACE (`unshare -rn`), so no
# sudo is needed and no host port, process or sysctl is touched.
#
# USAGE
#   scripts/perf/secret_leak_hunt.sh                 # every arm
#   scripts/perf/secret_leak_hunt.sh churn           # both churn arms
#   scripts/perf/secret_leak_hunt.sh churn-relay
#   scripts/perf/secret_leak_hunt.sh churn-direct
#   scripts/perf/secret_leak_hunt.sh stall
#   scripts/perf/secret_leak_hunt.sh stall-direct
#   scripts/perf/secret_leak_hunt.sh reap            # ~70 s
#   scripts/perf/secret_leak_hunt.sh udpbuf
#   scripts/perf/secret_leak_hunt.sh upgrade
#   scripts/perf/secret_leak_hunt.sh upgrade-late   # ~3 min
#
# Env:
#   BORE_BIN              binary under test (default target/release/bore)
#   SECLEAK_CONNS         connections per phase (default 200)
#   SECLEAK_PHASES        churn phases; the LAST one is the verdict (default 4)
#   SECLEAK_KEEP=1        keep the run directories for inspection
#   SECLEAK_LATE          seconds the upgrade-late arm withholds the provider
#                         (default 90 — must exceed several backoff steps)
#
# Requires: a release build (`cargo build --release`), `jq`, `python3`, `ss`,
# `unshare` with unprivileged user namespaces, and `gdb` for the stall dump
# (optional — the arm degrades to reporting the number alone).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="${BORE_BIN:-$ROOT/target/release/bore}"
CONNS="${SECLEAK_CONNS:-200}"
PHASES="${SECLEAK_PHASES:-4}"

[ -x "$BIN" ] || { echo "no bore binary at $BIN — run: cargo build --release" >&2; exit 2; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 2; }
command -v ss >/dev/null || { echo "ss (iproute2) is required" >&2; exit 2; }
unshare -rn true 2>/dev/null || {
    echo "unprivileged network namespaces are unavailable; this harness needs them" >&2
    exit 2
}

PASS=0; FAIL=0
pass(){ PASS=$((PASS+1)); echo "PASS: $*"; }
fail(){ FAIL=$((FAIL+1)); echo "FAIL: $*"; }

# ---------------------------------------------------------------------------
# One arm, entirely inside its own namespace.
#   $1 arm  : churn-relay | churn-direct | stall | reap | udpbuf
# Prints a transcript whose `RESULT <name> <ok|bad> <detail>` lines the caller
# turns into pass/fail. Keeping the verdicts in the transcript (and not in the
# namespace's exit code) is what lets a failing run be read after the fact from
# the log alone.
# ---------------------------------------------------------------------------
arm() {
    local a=$1 run
    run=$(mktemp -d -p "${TMPDIR:-/tmp}" borelk.XXXXXX)
    echo "# run dir: $run"
    unshare -rn bash -s "$BIN" "$ROOT" "$run" "$a" "$CONNS" "$PHASES" <<'INNER'
set -uo pipefail
BIN=$1; ROOT=$2; RUN=$3; ARM=$4; CONNS=$5; PHASES=$6

CTRL=17945; ORIGIN=18995; PROXY=19995
PPORT=41001; CPORT=41002
SEC=secleakhunt
TOK=0123456789abcdef0123456789abcdef01234567
ID=leakhunt
DUMMY_IP=10.77.0.1

ip link set lo up
# A routable candidate that is not loopback, in this namespace only. The peers
# are both on this host, so punching to it is a local send — which is exactly
# what we want: the traversal is not under test here, the cost of the
# established path is.
ip link add sec0 type dummy 2>/dev/null
ip addr add "$DUMMY_IP/24" dev sec0 2>/dev/null
ip link set sec0 up 2>/dev/null

RC="$ROOT/scripts/perf/raw_client.py"
RO="$ROOT/scripts/perf/raw_origin.py"

fds(){ ls "/proc/$1/fd" 2>/dev/null | wc -l; }
rss(){ awk '/^VmRSS/{print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }
alive(){ kill -0 "$1" 2>/dev/null; }
adm(){ curl -fsS -m 3 -H "Authorization: Bearer $TOK" \
       "http://127.0.0.1:$CTRL/admin/api/v1/$1" 2>/dev/null; }
consumers(){ adm secret | jq '[.[]|select(.role=="secretconsumer")]|length' 2>/dev/null || echo 0; }
cpath(){ adm secret | jq -r '[.[]|select(.role=="secretconsumer")][0].current_path // "unknown"' 2>/dev/null || echo unknown; }
# One round-trip, in milliseconds. `ping` moves two bytes, so what it times is
# the tunnel accepting a connection and coming back — the thing a wedge takes
# away.
rtt_ms(){
    local t0 t1
    t0=$(date +%s%3N)
    python3 "$RC" ping 127.0.0.1 "$PROXY" 1 1 8 >/dev/null 2>&1 || { echo -1; return; }
    t1=$(date +%s%3N)
    echo $((t1 - t0))
}
wave(){ python3 "$RC" get 127.0.0.1 "$PROXY" "$1" "$2" 25 0 2>/dev/null; }

python3 "$RO" "$ORIGIN" >"$RUN/origin.log" 2>&1 &
ORIGIN_PID=$!; disown

"$BIN" server --secret "$SEC" --control-port "$CTRL" \
    --bind-addr 127.0.0.1 --bind-tunnels 127.0.0.1 \
    --admin-token "$TOK" --udp >"$RUN/server.log" 2>&1 &
SRV=$!; disown

for _ in $(seq 1 40); do
    adm metrics >/dev/null 2>&1 && break
    sleep 0.25
done
adm metrics >/dev/null 2>&1 || { echo "RESULT server-up bad the control port never answered"; exit 0; }

# Per-arm transport flags. The direct arms pin BOTH ends: an explicit candidate
# on a fixed port with STUN off leaves nothing to discover, so a relay reading
# here means the direct path broke, never that discovery was slow.
PFLAGS=(); CFLAGS=()
case "$ARM" in
    churn-direct|udpbuf|upgrade|upgrade-late|stall-direct)
        PFLAGS=(--udp --udp-no-stun --nat-udp-preferred-port "$PPORT" --udp-candidate "$DUMMY_IP:$PPORT")
        CFLAGS=(--udp --udp-no-stun --nat-udp-preferred-port "$CPORT" --udp-candidate "$DUMMY_IP:$CPORT")
        ;;
esac

start_provider(){
    "$BIN" local "$ORIGIN" --to "127.0.0.1:$CTRL" --secret "$SEC" \
        --tcp-secret-id "$ID" "${PFLAGS[@]+"${PFLAGS[@]}"}" \
        >"$RUN/provider.log" 2>&1 &
    PROV=$!; disown
}
start_consumer(){
    "$BIN" proxy --to "127.0.0.1:$CTRL" --secret "$SEC" \
        --tcp-secret-id "$ID" --local-proxy-port "127.0.0.1:$PROXY" \
        "${CFLAGS[@]+"${CFLAGS[@]}"}" \
        >"$RUN/consumer.log" 2>&1 &
    CONS=$!; disown
}

# How long the consumer is left with no provider before the upgrade becomes
# possible. `upgrade` keeps it at zero (the fastest deterministic late upgrade);
# `upgrade-late` pushes it past several backoff steps so the arm measures the
# CAP rather than the first retry.
LATE_SECS=0
[ "$ARM" = upgrade-late ] && LATE_SECS="${SECLEAK_LATE:-90}"

if [ "$ARM" = upgrade ] || [ "$ARM" = upgrade-late ]; then
    # Deterministic late upgrade: with no provider registered, the consumer's
    # FIRST negotiation cannot succeed, so it registers on the relay and reports
    # that. Whether the initial negotiation wins its race is otherwise a matter
    # of scheduling, and a gate that depends on losing a race is not a gate.
    start_consumer
    for _ in $(seq 1 60); do
        grep -q "udp unavailable, using relay" "$RUN/consumer.log" 2>/dev/null && break
        sleep 0.5
    done
    if grep -q "udp unavailable, using relay" "$RUN/consumer.log" 2>/dev/null; then
        echo "RESULT upgrade-starts-on-relay ok the consumer registered on the relay with no provider present"
    else
        echo "RESULT upgrade-starts-on-relay bad the consumer never reported the relay verdict"
    fi
    # Let the exponential backoff climb before the path becomes possible. This
    # is what turns the arm from "does an upgrade happen at all" into "how long
    # does the WORST case last", which is the only question the cap answers.
    [ "$LATE_SECS" -gt 0 ] && sleep "$LATE_SECS"
    UPGRADE_POSSIBLE_AT=$(date +%s)
    start_provider
else
    start_provider
    start_consumer
fi

# Ready = the consumer's listener answers a real round-trip. Polling the admin
# API alone would pass before the data path exists.
ready=0
for _ in $(seq 1 60); do
    if [ "$(rtt_ms)" -ge 0 ] 2>/dev/null; then ready=1; break; fi
    sleep 0.5
done
[ "$ready" = 1 ] || { echo "RESULT tunnel-up bad no round-trip in 30 s"; exit 0; }
echo "RESULT tunnel-up ok round-trip served"

# The direct arms need the transport they claim to measure. The consumer is the
# only side that can report it (S-1) and the server is what we read back.
if [ "$ARM" = "churn-direct" ] || [ "$ARM" = "udpbuf" ] || [ "$ARM" = "stall-direct" ]; then
    path=relay
    for _ in $(seq 1 40); do
        path=$(cpath); [ "$path" = "direct" ] && break
        # A path report is written on a proxied connection, so keep offering one.
        python3 "$RC" ping 127.0.0.1 "$PROXY" 1 1 8 >/dev/null 2>&1
        sleep 0.5
    done
    if [ "$path" = "direct" ]; then
        echo "RESULT direct-path ok the consumer reports direct"
    else
        echo "RESULT direct-path bad the consumer reports $path"
    fi
fi

case "$ARM" in

# ---------------------------------------------------------------------------
upgrade|upgrade-late)
    # The upgrade retries on a capped backoff (2, 4, 8, … seconds), so 60 s of
    # patience covers several attempts without the gate becoming a stopwatch.
    t0=${UPGRADE_POSSIBLE_AT:-$(date +%s)}; got=relay
    for _ in $(seq 1 400); do
        got=$(cpath); [ "$got" = direct ] && break
        python3 "$RC" ping 127.0.0.1 "$PROXY" 1 1 8 >/dev/null 2>&1
        sleep 0.5
    done
    took=$(( $(date +%s) - t0 ))
    echo "MEASURE upgrade current_path=$got after=${took}s"
    if grep -q "upgraded relay → direct udp path" "$RUN/consumer.log" 2>/dev/null; then
        echo "RESULT upgrade-happened ok the consumer upgraded the live session to the direct path"
    else
        echo "RESULT upgrade-happened bad the consumer never upgraded, so there is nothing to report"
    fi
    # The whole point: the server must be TOLD. Before this was fixed the
    # consumer logged `path=direct-udp` for every connection while the admin
    # API answered `relay` for the rest of the session.
    if [ "$got" = direct ]; then
        echo "RESULT upgrade-reported ok the admin API reports direct ${took}s after the upgrade became possible"
    else
        echo "RESULT upgrade-reported bad the admin API still reports $got while the data path moved"
    fi
    # S-8: the cap is the worst case, so the arm that provoked the worst case
    # asserts it. With the previous 256 s cap the retry grid was 2, 4, 8, 16,
    # 32, 64, 128, … — a provider appearing at t=90 s would not have been
    # noticed before t=190 s, which is what this bound red-checks.
    if [ "$ARM" = upgrade-late ]; then
        if [ "$got" = direct ] && [ "$took" -le 75 ]; then
            echo "RESULT upgrade-cap ok the worst-case relay dwell was ${took}s"
        else
            echo "RESULT upgrade-cap bad the tunnel needed ${took}s to leave the relay (path=$got)"
        fi
    fi
    # And the stale explanation must go with it: `path_reason` describes why the
    # path is what it IS, not what it once was.
    why=$(adm secret | jq -r '[.[]|select(.role=="secretconsumer")][0].path_reason // ""' 2>/dev/null)
    echo "MEASURE upgrade path_reason=\"${why}\""
    if [ -z "$why" ]; then
        echo "RESULT upgrade-reason-cleared ok no stale path_reason survives the upgrade"
    else
        echo "RESULT upgrade-reason-cleared bad the row still explains the path with: $why"
    fi
    # The data path itself has to keep working across the swap — an upgrade that
    # reports beautifully and drops connections is worse than no upgrade.
    ms=$(rtt_ms)
    if [ "$ms" -ge 0 ] && [ "$ms" -le 3000 ]; then
        echo "RESULT upgrade-serves ok a round-trip after the swap took ${ms}ms"
    else
        echo "RESULT upgrade-serves bad a round-trip after the swap took ${ms}ms"
    fi
    ;;

# ---------------------------------------------------------------------------
churn-relay|churn-direct)
    # Warm: one phase's worth of shape, so allocator growth and pool
    # establishment happen BEFORE the first sample. Without this the first
    # phase always "leaks" and the arm is noise.
    for _ in $(seq 1 5); do wave 65536 4 >/dev/null; done
    sleep 1

    # A SERIES, not a before/after pair. A leak is linear in connections, so
    # its per-phase delta is CONSTANT; warm-up and saturating caches decay
    # toward zero. One pair of samples cannot tell those apart and the first
    # version of this arm duly accused the client of leaking 16 KiB per
    # connection when the real shape was an arena that stopped growing.
    # The verdict is therefore on the LAST phase only, and the whole series is
    # printed so a future reader can see the decay instead of trusting it.
    phase(){
        local waves=$((CONNS / 10)) i
        for i in $(seq 1 "$waves"); do wave 65536 10 >/dev/null; done
        sleep 2
    }

    p_fs=$(fds "$SRV"); p_fp=$(fds "$PROV"); p_fc=$(fds "$CONS")
    p_rs=$(rss "$SRV"); p_rp=$(rss "$PROV"); p_rc=$(rss "$CONS")
    echo "MEASURE warm fd=$p_fs/$p_fp/$p_fc rss=$p_rs/$p_rp/$p_rc"

    d_fs=0; d_fp=0; d_fc=0; d_rs=0; d_rp=0; d_rc=0
    DRS=(); DRP=(); DRC=()
    for k in $(seq 1 "$PHASES"); do
        phase
        n_fs=$(fds "$SRV"); n_fp=$(fds "$PROV"); n_fc=$(fds "$CONS")
        n_rs=$(rss "$SRV"); n_rp=$(rss "$PROV"); n_rc=$(rss "$CONS")
        d_fs=$((n_fs - p_fs)); d_fp=$((n_fp - p_fp)); d_fc=$((n_fc - p_fc))
        d_rs=$((n_rs - p_rs)); d_rp=$((n_rp - p_rp)); d_rc=$((n_rc - p_rc))
        echo "MEASURE phase$k fd=$n_fs/$n_fp/$n_fc rss=$n_rs/$n_rp/$n_rc dfd=$d_fs/$d_fp/$d_fc drss=$d_rs/$d_rp/$d_rc"
        DRS+=("$d_rs"); DRP+=("$d_rp"); DRC+=("$d_rc")
        p_fs=$n_fs; p_fp=$n_fp; p_fc=$n_fc; p_rs=$n_rs; p_rp=$n_rp; p_rc=$n_rc
    done

    # Slack of 4 descriptors absorbs the punch socket being re-bound and a
    # carrier being replaced; a per-connection leak over $CONNS connections
    # cannot hide inside it.
    for pair in "server:$d_fs" "provider:$d_fp" "consumer:$d_fc"; do
        nm=${pair%%:*}; v=${pair##*:}
        if [ "$v" -le 4 ]; then
            echo "RESULT fd-$nm ok +$v descriptors in the last phase of $CONNS connections"
        else
            echo "RESULT fd-$nm bad +$v descriptors in the last phase of $CONNS connections"
        fi
    done
    # RSS is judged on TWO statements, because one of them alone was wrong.
    #
    # The original gate read only the LAST phase's delta against 1024 KiB, on
    # the stated assumption that "by the last phase the arenas have stopped
    # moving". Its own data falsified that on 2026-09-12: the consumer of the
    # DIRECT arm read `drss` -1136 then +1132 KiB on consecutive phases, with
    # the provider flat at -12 and every descriptor count at +0. A leak cannot
    # produce a negative phase. That is glibc's arena taking and returning a
    # ~1.1 MiB chunk, and a single-phase verdict lets whichever phase the swing
    # lands in decide the result — a coin toss dressed as a measurement.
    #
    # So: MAGNITUDE, then TREND.
    #
    # Magnitude — `RSS_PHASE_SLACK` 2048 KiB over $CONNS connections is ~10 KiB
    # each. Still 25x below the smallest retention this path can physically
    # have (one proxy buffer, 256 KiB by default) and ~2x above the arena
    # oscillation measured above, so it cannot be tripped by allocator noise
    # and cannot miss a per-connection leak.
    #
    # Trend — a leak is LINEAR in connections, so it is positive in every
    # phase. Requiring "every phase after the first rose, AND the total rise
    # exceeds one phase's slack" gives back the sensitivity the wider bound
    # gave up: a steady 3.5 KiB/connection drift fails on the trend while
    # every single phase of it sits under the magnitude bound, which is
    # exactly the shape the old gate would have passed four times in a row.
    # The first phase is excluded because it carries the tail of warm-up,
    # which decays and is not a leak (the reason this arm warms up at all).
    #
    # That 3.5 KiB/connection IS the resolution of this instrument at the
    # default 4 phases x 200 connections, and it is a floor set by the data,
    # not by taste: the HEALTHY relay server measured on 2026-09-12 rose in
    # all three trend phases for a total of 1088 KiB, i.e. 1.8 KiB per
    # connection, so any bound below that would fail a process with nothing
    # wrong with it. Resolving finer means more connections, not a tighter
    # number — raise PHASES/CONNS, and the bound scales with neither, so say
    # so in the run rather than quietly tightening it.
    RSS_PHASE_SLACK="${RSS_PHASE_SLACK:-2048}"
    rss_verdict(){ # <name> <per-phase deltas...>
        local nm="$1"; shift
        local -a d=("$@")
        local n=${#d[@]}
        local last="${d[$((n - 1))]}"
        local i v sum=0 rose=1 amp=0 a
        for i in $(seq 1 $((n - 1))); do
            v="${d[$i]}"
            sum=$((sum + v))
            [ "$v" -gt 0 ] || rose=0
            a="${v#-}"; [ "$a" -gt "$amp" ] && amp="$a"
        done
        local why=""
        [ "$last" -gt "$RSS_PHASE_SLACK" ] && why="the last phase alone added ${last} KiB"
        if [ "$rose" = 1 ] && [ "$sum" -gt "$RSS_PHASE_SLACK" ]; then
            why="${why:+$why; }RSS rose in every phase after the first, ${sum} KiB in total"
        fi
        local lastf; lastf=$(printf '%+d' "$last")
        local sumf;  sumf=$(printf '%+d' "$sum")
        if [ -z "$why" ]; then
            echo "RESULT rss-$nm ok last phase $lastf KiB, trend $sumf KiB over $((n - 1)) phases, swing ${amp} KiB (slack $RSS_PHASE_SLACK)"
        else
            echo "RESULT rss-$nm bad $why (slack $RSS_PHASE_SLACK, $CONNS connections per phase)"
        fi
    }
    rss_verdict server   "${DRS[@]}"
    rss_verdict provider "${DRP[@]}"
    rss_verdict consumer "${DRC[@]}"

    # Liveness after the churn: a process that died mid-run would report a
    # beautifully stable descriptor count.
    for pair in "server:$SRV" "provider:$PROV" "consumer:$CONS"; do
        nm=${pair%%:*}; p=${pair##*:}
        if alive "$p"; then echo "RESULT alive-$nm ok still running"
        else echo "RESULT alive-$nm bad exited during the churn"; fi
    done
    ;;

# ---------------------------------------------------------------------------
stall|stall-direct)
    # Held connections move no bytes on purpose: they occupy the tunnel without
    # loading it, which is the state a starved-window or lost-waker bug needs.
    python3 "$RC" hold 127.0.0.1 "$PROXY" 40 16 45 >"$RUN/hold.log" 2>&1 &
    HOLD=$!; disown
    sleep 2

    # Six bulk waves, each killed mid-flight. An abrupt abort is what leaves a
    # half-closed stream behind; a clean close is the easy case.
    for i in $(seq 1 6); do
        python3 "$RC" get 127.0.0.1 "$PROXY" 33554432 4 25 0 >"$RUN/wave$i.log" 2>&1 &
        w=$!; disown
        sleep 1
        kill -9 "$w" 2>/dev/null
        sleep 0.3
    done

    # The discriminator: one fresh connection, while 16 are held open.
    worst=0
    for i in 1 2 3; do
        ms=$(rtt_ms)
        echo "MEASURE fresh-rtt-$i ${ms}ms"
        [ "$ms" -lt 0 ] && { worst=999999; break; }
        [ "$ms" -gt "$worst" ] && worst=$ms
    done
    if [ "$worst" -le 3000 ]; then
        echo "RESULT stall-fresh ok worst fresh round-trip ${worst}ms behind 16 held connections"
    else
        echo "RESULT stall-fresh bad worst fresh round-trip ${worst}ms — the path is wedged"
        # A number alone cannot be acted on a month later; a backtrace can.
        if command -v gdb >/dev/null; then
            for pair in "server:$SRV" "provider:$PROV" "consumer:$CONS"; do
                gdb -p "${pair##*:}" -batch -ex "thread apply all bt" \
                    >"$RUN/stack-${pair%%:*}.txt" 2>&1 || true
            done
            echo "MEASURE stacks dumped to $RUN"
        fi
    fi

    # The other half of the failure that matters: one tunnel's concurrency must
    # not take the process' other listeners down (P-12's shape).
    if adm metrics >/dev/null 2>&1; then
        echo "RESULT stall-admin ok the admin API still answers under the load"
    else
        echo "RESULT stall-admin bad the admin API stopped answering"
    fi

    kill -9 "$HOLD" 2>/dev/null
    sleep 2
    ms=$(rtt_ms)
    if [ "$ms" -ge 0 ] && [ "$ms" -le 3000 ]; then
        echo "RESULT stall-recover ok ${ms}ms after the held connections were aborted"
    else
        echo "RESULT stall-recover bad ${ms}ms after the held connections were aborted"
    fi
    ;;

# ---------------------------------------------------------------------------
reap)
    sleep 2
    n0=$(consumers); f0=$(fds "$SRV")
    echo "MEASURE pre-stop consumers=$n0 server_fd=$f0"
    # SIGSTOP, not SIGKILL: the socket stays open and the kernel keeps ACKing,
    # so neither `send` nor `recv` on the yamux substream can see anything
    # wrong. This is the only shape the heartbeat-tick reaper exists for.
    kill -STOP "$CONS" 2>/dev/null
    t0=$(date +%s); gone=0
    for _ in $(seq 1 150); do
        if [ "$(consumers)" -lt "$n0" ]; then gone=1; break; fi
        sleep 0.5
    done
    t1=$(date +%s); took=$((t1 - t0))
    kill -CONT "$CONS" 2>/dev/null
    if [ "$gone" = 1 ]; then
        echo "RESULT reap-row ok the wedged consumer's row was released after ${took}s"
    else
        echo "RESULT reap-row bad the row survived ${took}s of a wedged consumer"
    fi
    sleep 2
    f1=$(fds "$SRV")
    echo "MEASURE post-reap server_fd=$f1"
    if [ "$f1" -le "$f0" ]; then
        echo "RESULT reap-fd ok the server's descriptors came back down ($f0 -> $f1)"
    else
        echo "RESULT reap-fd bad the row went away but $((f1 - f0)) descriptors did not ($f0 -> $f1)"
    fi
    ;;

# ---------------------------------------------------------------------------
udpbuf)
    # Move some bytes first: the punch socket exists from the handshake, but a
    # reading of a socket that has never carried a flow proves less.
    wave 1048576 4 >/dev/null
    dflt=$(cat /proc/sys/net/core/rmem_default 2>/dev/null || echo 212992)
    want=$((dflt * 2))
    for pair in "provider:$PPORT" "consumer:$CPORT"; do
        nm=${pair%%:*}; port=${pair##*:}
        line=$(ss -uapmH "sport = :$port" 2>/dev/null | tr '\n' ' ')
        rb=$(echo "$line" | grep -oE 'rb[0-9]+' | head -1 | tr -d 'rb')
        tb=$(echo "$line" | grep -oE 'tb[0-9]+' | head -1 | tr -d 'tb')
        echo "MEASURE udpbuf-$nm port=$port rb=${rb:-none} tb=${tb:-none} rmem_default=$dflt"
        if [ -z "${rb:-}" ] || [ -z "${tb:-}" ]; then
            echo "RESULT udpbuf-$nm bad no UDP socket bound on $port"
        elif [ "$rb" -ge "$want" ] && [ "$tb" -ge "$want" ]; then
            echo "RESULT udpbuf-$nm ok rb=$rb tb=$tb, both above the untuned default"
        else
            echo "RESULT udpbuf-$nm bad rb=$rb tb=$tb against a default of $dflt — the socket was never configured"
        fi
    done
    ;;
esac

# No `wait`: a non-interactive bash still reports a SIGKILLed job on stderr,
# which lands in the middle of the transcript and reads like a failure.
kill -9 "$CONS" "$PROV" "$SRV" "$ORIGIN_PID" 2>/dev/null
sleep 0.3
INNER
    echo "$run"
}

# ---------------------------------------------------------------------------
# Run one arm and turn its transcript into verdicts.
# ---------------------------------------------------------------------------
run_arm() {
    local a=$1 label=$2 out run
    echo ""
    echo "########## $label"
    out=$(arm "$a")
    echo "$out" | grep -E '^(MEASURE|# run dir)' || true
    local any=0
    while read -r _ name verdict detail; do
        any=1
        if [ "$verdict" = ok ]; then pass "$label/$name: $detail"
        else fail "$label/$name: $detail"; fi
    done < <(echo "$out" | grep '^RESULT ')
    [ "$any" = 1 ] || fail "$label: the arm produced no verdicts (see the transcript above)"
    if [ "${SECLEAK_KEEP:-0}" != 1 ]; then
        run=$(echo "$out" | tail -1)
        case "$run" in /tmp/*borelk.*|"${TMPDIR:-/tmp}"/borelk.*) rm -rf "$run";; esac
    fi
}

MODE="${1:-all}"
echo "=== secret resource hunt: $($BIN --version), $CONNS connections x $PHASES phases ==="
case "$MODE" in
    churn-relay)  run_arm churn-relay  "T-SECLEAK-CHURN-RELAY" ;;
    churn-direct) run_arm churn-direct "T-SECLEAK-CHURN-DIRECT" ;;
    churn)        run_arm churn-relay  "T-SECLEAK-CHURN-RELAY"
                  run_arm churn-direct "T-SECLEAK-CHURN-DIRECT" ;;
    stall)        run_arm stall        "T-SECLEAK-STALL" ;;
    stall-direct) run_arm stall-direct "T-SECLEAK-STALL-DIRECT" ;;
    reap)         run_arm reap         "T-SECLEAK-REAP" ;;
    udpbuf)       run_arm udpbuf       "T-SECLEAK-UDPBUF" ;;
    upgrade)      run_arm upgrade      "T-SECLEAK-UPGRADE" ;;
    upgrade-late) run_arm upgrade-late "T-SECLEAK-UPGRADE-CAP" ;;
    all)
        run_arm churn-relay  "T-SECLEAK-CHURN-RELAY"
        run_arm churn-direct "T-SECLEAK-CHURN-DIRECT"
        run_arm udpbuf       "T-SECLEAK-UDPBUF"
        run_arm upgrade      "T-SECLEAK-UPGRADE"
        run_arm upgrade-late "T-SECLEAK-UPGRADE-CAP"
        run_arm stall        "T-SECLEAK-STALL"
        run_arm stall-direct "T-SECLEAK-STALL-DIRECT"
        run_arm reap         "T-SECLEAK-REAP"
        ;;
    *) echo "unknown mode: $MODE (see the header for the list)" >&2; exit 2 ;;
esac

echo ""
echo "=== PASS: $PASS FAIL: $FAIL ==="
[ "$FAIL" -eq 0 ]
