#!/usr/bin/env bash
# Does a public `--udp` tunnel lose its direct carrier because the PREVIOUS
# tunnel on the same port died?
#
# THE HYPOTHESIS, AND WHY IT NEEDS A FALSIFYING TEST RATHER THAN AGREEMENT
# -----------------------------------------------------------------------
# `pub/ws_first_conn.sh`'s delay axis showed a fresh `--udp` tunnel holding
# `direct_pool=1` immediately and `0` twenty seconds later, after which every
# connection fell back to the relay for the life of the tunnel. The obvious
# reading -- an idle timeout -- is WRONG, and a packet capture on the client's
# own QUIC socket is what killed it: keep-alives flow every 3 s in BOTH
# directions and are answered in about a millisecond, so the connection is alive
# while the server's pool says it is gone.
#
# Reading the server instead: on every registration `Server::serve_tunnel`
# builds a FRESH `PublicDirectEntry` with a `DirectPool::default()`, whose ids
# restart at 0, and inserts it under the SAME key `port:<N>`. The close monitor
# re-resolves that key AT CLOSE TIME:
#
#     direct.closed().await;
#     if let Some(entry) = public_reg.get(&key) { entry.direct.remove(id); }
#
# So when the PREVIOUS tunnel's connection finally closes, the monitor resolves
# `port:<N>` to the tunnel that exists NOW and removes id 0 -- which is the new,
# live carrier. The code comment says "keyed by a monotonic id so a stale
# close-monitor never evicts a newer member", and that is true only WITHIN one
# pool. The server's own log agrees: four re-registrations of the same port each
# logged `id=0`, where a global id would have read 0, 1, 2, 3.
#
# That is a tidy story, which is exactly the kind this campaign has been wrong
# about before (trap 40). So it is put at risk of falsification with the one
# variable that separates it from every timeout explanation: whether a tunnel
# died on this port just before.
#
#   arm FRESH     the first tunnel this stage puts on the port. Nothing died
#                 here recently, so no stale monitor exists.
#                 PREDICTION: the pool stays 1 through the whole watch.
#
#   arm RECYCLED  the same port, killed and immediately re-registered, so a
#                 stale monitor from the arm above is pending.
#                 PREDICTION: the pool reaches 1 and drops to 0 within roughly
#                 the QUIC idle timeout of the connection that just died.
#
# If FRESH also drops, the hypothesis is dead and the stage says so. An idle
# timeout cannot tell the two arms apart -- both are idle, for the same time, on
# the same port, with the same client and server -- so the arms differ in
# exactly one thing.
#
# COST: ZERO bandwidth. Not one byte is transferred; the whole stage is admin
# API polling. It is safe to run beside a measurement that owns the line.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C

RP=5053; P="${P:-9071}"
REPS="${REPS:-3}"
WATCH="${WATCH:-45}"
POLL="${POLL:-2}"

UP=()
up() { # <port> <extra>
    vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/poolrecycle-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
    local i; for i in $(seq 80); do
        adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
            && { UP+=("$1"); return 0; }
        sleep 0.5
    done
    return 1
}
kill_one() { vm "pkill -9 -f \"local $RP --port $1\" 2>/dev/null; true" >/dev/null 2>&1; }
down_one() { # <port> -- kill and WAIT for the server to forget the entry
    kill_one "$1"
    local i js
    for i in $(seq 60); do
        js="$(adm tunnels 2>/dev/null)"
        if [ -z "$js" ]; then echo "    WARNING: admin API silent while releasing $1"; sleep 1; continue; fi
        printf '%s' "$js" | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 || return 0
        sleep 1
    done
    return 1
}
down() { local p; for p in "${UP[@]:-}"; do down_one "$p" >/dev/null 2>&1; done; }
trap 'down' EXIT

pool_of() { # <port> -- the server's own count, `?` when it did not answer
    local v
    v="$(adm tunnels 2>/dev/null | jq -r --argjson p "$1" \
        '[.[]|select(.public_port==$p)] as $t | if ($t|length)==0 then "?" else "\($t[0].direct_pool // "?")" end' 2>/dev/null)"
    [ -n "$v" ] || v="?"
    printf '%s' "$v"
}

# Watch one already-registered tunnel and report its series plus the second at
# which the pool first read 0 after having been positive.
watch_arm() { # <port> -> prints series; echoes "died=<s>" or "survived=<peak>"
    local port="$1" t=0 peak=0 died="" p series=""
    while [ "$t" -lt "$WATCH" ]; do
        p="$(pool_of "$port")"
        series+=" ${t}s:$p"
        case "$p" in ''|*[!0-9]*) : ;; *)
            [ "$p" -gt "$peak" ] && peak="$p"
            [ "$peak" -gt 0 ] && [ "$p" -eq 0 ] && [ -z "$died" ] && died="$t" ;;
        esac
        sleep "$POLL"; t=$((t + POLL))
    done
    printf '      series:%s\n' "$series" >&2
    if [ "$peak" -eq 0 ]; then printf 'never=1'
    elif [ -n "$died" ]; then printf 'died=%s' "$died"
    else printf 'survived=%s' "$peak"; fi
}

echo "### does a recycled port lose its direct carrier? -- $(date -Is)"
echo "  $REPS reps, watch ${WATCH}s on a ${POLL}s grid, port $P, ZERO bytes transferred"
echo "  server: $(adm config 2>/dev/null | jq -r '"\(.server_version)  keepalive=\(.direct_quic_keepalive_ms)ms idle=\(.direct_quic_idle_ms)ms"' 2>/dev/null)"
echo
echo "  FRESH    = first tunnel on this port; no stale close-monitor pending"
echo "  RECYCLED = same port, previous tunnel killed moments earlier"
echo "  An idle timeout cannot distinguish these two. The hypothesis can."
echo

FRESH=""; RECYC=""; scored=0
for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    # Make the port genuinely quiet first, so FRESH earns its name.
    down_one "$P" >/dev/null 2>&1
    sleep 15

    if ! up "$P" "--udp"; then echo "    FRESH: failed to register"; continue; fi
    echo "    FRESH (nothing died on this port recently)"
    r1="$(watch_arm "$P")"
    printf '      verdict: %s\n' "$r1"
    FRESH+=" $r1"

    # Now recycle: kill and re-register IMMEDIATELY, so the monitor of the
    # connection just killed is still pending when the new carrier installs.
    kill_one "$P"
    local_i=0
    while [ "$local_i" -lt 30 ]; do
        adm tunnels 2>/dev/null | jq -e --argjson p "$P" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 || break
        sleep 1; local_i=$((local_i + 1))
    done
    if ! up "$P" "--udp"; then echo "    RECYCLED: failed to re-register"; continue; fi
    echo "    RECYCLED (the previous tunnel on this port died moments ago)"
    r2="$(watch_arm "$P")"
    printf '      verdict: %s\n' "$r2"
    RECYC+=" $r2"

    scored=$((scored + 1))
    down_one "$P" >/dev/null 2>&1
done

echo
echo "=== the server's own account of each carrier install (id restarts = fresh pool) ==="
vm "grep -a 'direct udp carrier ready' \$HOME/out/poolrecycle-$P.log 2>/dev/null | tail -n 8" 2>/dev/null | sed 's/^/    /'

echo
echo "=== verdict ==="
printf '  FRESH   :%s\n' "${FRESH:- (none)}"
printf '  RECYCLED:%s\n' "${RECYC:- (none)}"
echo
if [ "$scored" -eq 0 ]; then
    echo "  INSTRUMENT FAILURE: no repetition completed both arms."
    exit 2
fi
fresh_died=$(printf '%s' "$FRESH"  | tr ' ' '\n' | grep -c '^died=' || true)
recyc_died=$(printf '%s' "$RECYC" | tr ' ' '\n' | grep -c '^died=' || true)
echo "  died: FRESH $fresh_died/$scored, RECYCLED $recyc_died/$scored"
if [ "$fresh_died" -eq 0 ] && [ "$recyc_died" -gt 0 ]; then
    echo "  CONFIRMED: only the recycled port loses its carrier. A stale close monitor"
    echo "  from the previous tunnel resolves the key to the CURRENT entry and removes"
    echo "  id 0 from it -- the new pool's ids restart at 0, so the id guard cannot"
    echo "  tell the two apart. The QUIC connection itself stays up, which is why the"
    echo "  client never notices and never renews."
elif [ "$fresh_died" -gt 0 ]; then
    echo "  HYPOTHESIS FALSIFIED: the fresh port lost its carrier too, so a stale"
    echo "  monitor cannot be the whole story. Do not publish the eviction mechanism."
else
    echo "  INCONCLUSIVE: neither arm lost its carrier in this run."
fi
echo
echo "DONE"
