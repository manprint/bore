#!/usr/bin/env bash
# How long does a PUBLIC `--udp` tunnel's direct QUIC pool survive with no
# traffic at all, and does it ever come back?
#
# THE OBSERVATION THAT MADE THIS NECESSARY
# ----------------------------------------
# `pub/ws_first_conn.sh`'s DELAYS axis registers a fresh `--udp` tunnel, waits,
# and only then moves a byte. Reading the SERVER's own admin API (never the
# client's log -- P-12) it reported, in one repetition:
#
#     delay=0   before: unknown 0 0 1   -> every transfer  direct
#     delay=20  before: unknown 0 0 0   -> every transfer  relay, fallbacks 1,2,3
#     delay=60  before: unknown 0 0 0   -> every transfer  relay, fallbacks 1,2,3
#
# (the four fields are `current_path direct_stream_opens direct_fallbacks
# direct_pool`.) The pool holds a connection immediately after registration and
# is EMPTY twenty seconds later, with nothing having used it in between.
#
# That should not happen. The server publishes `direct_quic_keepalive_ms=3000`
# and `direct_quic_idle_ms=10000`, and BOTH ends build their transport config
# through the same `holepunch::transport_config`, so a keep-alive every 3 s sits
# well inside a 10 s idle timeout. `DirectPool::len` is the raw vector length
# and `pick` never prunes, so a pool reading 0 means something called
# `DirectPool::remove` -- which only the close monitor does, when the QUIC
# connection actually closed.
#
# WHAT THIS STAGE SEPARATES
# -------------------------
# Two readings of "pool 0" are opposite diagnoses and look identical in a single
# cell taken after the fact:
#
#   (a) the connection came up and DIED   -> a liveness defect
#   (b) the connection never came up      -> a flaky establishment
#
# So this stage polls from BEFORE the pool can be full, on a fine grid, and
# prints the whole series. A transition 1 -> 0 at a recorded second is (a); a
# series that is 0 throughout is (b). Nothing is inferred from an endpoint.
#
# THE CONTROL, AND WHY IT IS NOT OPTIONAL
# ---------------------------------------
# "The direct pool died" and "the tunnel died" produce the same empty pool. A
# plain relay tunnel is therefore raised alongside and polled on the same grid:
# it has no direct pool to lose, so it can only report whether the CONTROL
# channel is still up. If the control channel is also gone, this stage has
# measured a reconnect and not a pool lifetime, and it says so instead of
# publishing a number.
#
# COST: REPS transfers of XFER_MB, and nothing else -- the polling is admin-API
# requests. At the defaults that is well under a gigabyte, which is why the
# grid can be fine and the watch long.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"

RP=5053; R=9067; Q=9068
XFER_MB="${XFER_MB:-384}"; PER=$(( XFER_MB*1048576 ))
REPS="${REPS:-3}"
WATCH="${WATCH:-90}"       # seconds of pure idle to watch
POLL="${POLL:-2}"          # seconds between polls
AFTER="${AFTER:-40}"       # seconds to keep watching AFTER the transfer

UP=()
up() { # <port> <extra flags>
    vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/poollife-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
    local i; for i in $(seq 80); do
        adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
            && { UP+=("$1"); return 0; }
        sleep 0.5
    done
    return 1
}
down_one() { # <port>
    vm "pkill -9 -f \"local $RP --port $1\" 2>/dev/null; true" >/dev/null 2>&1
    local i js
    for i in $(seq 60); do
        js="$(adm tunnels 2>/dev/null)"
        if [ -z "$js" ]; then
            echo "    WARNING: the admin API did not answer while releasing port $1"
            sleep 1; continue
        fi
        printf '%s' "$js" | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 || return 0
        sleep 1
    done
    echo "    WARNING: the server still holds port $1"
    return 1
}
down() { local p; for p in "${UP[@]:-}"; do down_one "$p" >/dev/null 2>&1; done; }
trap 'down' EXIT

# The server's own four fields for one tunnel. An unanswered API prints `?`,
# never a zero: "the API did not answer" and "the counter did not move" are
# opposite readings and this campaign has paid for confusing them.
view() { # <port> -> "path opens fallbacks pool"
    local v
    v="$(adm tunnels 2>/dev/null | jq -r --argjson p "$1" '
        [ .[] | select(.public_port == $p) ] as $t
        | if ($t|length) == 0 then "? ? ? ?"
          else "\($t[0].current_path // "?") \($t[0].direct_stream_opens // "?") \($t[0].direct_fallbacks // "?") \($t[0].direct_pool // "?")"
          end' 2>/dev/null)"
    [ -n "$v" ] || v="? ? ? ?"
    printf '%s' "$v"
}
pool_of()  { printf '%s' "$1" | awk '{print $4}'; }
path_of()  { printf '%s' "$1" | awk '{print $1}'; }
g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 1 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

echo "### public --udp direct-pool lifetime under pure idle -- $(date -Is)"
echo "  $REPS reps: watch ${WATCH}s idle on a ${POLL}s grid, one ${XFER_MB} MiB transfer, then ${AFTER}s more"
echo "  the relay tunnel on the control port has no direct pool: it reports only"
echo "  whether the CONTROL channel is still up, which is what separates"
echo "  'the pool died' from 'the tunnel died'."
echo "  server: $(adm config 2>/dev/null | jq -r '"\(.server_version)  keepalive=\(.direct_quic_keepalive_ms)ms idle=\(.direct_quic_idle_ms)ms"' 2>/dev/null)"
echo

declare -A DIED   # DIED[rep] = second at which the pool first read 0 after being >0
RECOVER=""        # per-rep: did the pool come back after the transfer?
scored=0

for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    if ! up "$R" "" || ! up "$Q" "--udp"; then
        echo "    FAILED to register the pair (r=$R q=$Q)"
        down_one "$Q"; down_one "$R"; continue
    fi

    series=""; ctrl_series=""; t=0; peak=0; died=""
    while [ "$t" -lt "$WATCH" ]; do
        vq="$(view "$Q")"; vr="$(view "$R")"
        p="$(pool_of "$vq")"
        series+=" ${t}s:$p"
        ctrl_series+=" ${t}s:$(path_of "$vr")"
        case "$p" in ''|*[!0-9]*) : ;; *)
            [ "$p" -gt "$peak" ] && peak="$p"
            if [ "$peak" -gt 0 ] && [ "$p" -eq 0 ] && [ -z "$died" ]; then died="$t"; fi ;;
        esac
        sleep "$POLL"; t=$((t + POLL))
    done

    printf '    pool over idle:%s\n' "$series"
    printf '    control path  :%s\n' "$ctrl_series"

    # THE CONTROL DECIDES WHETHER THE REST IS READABLE.
    if printf '%s' "$ctrl_series" | grep -q '?'; then
        echo "    INSTRUMENT: the control tunnel went missing during the watch --"
        echo "    this repetition measured a reconnect, not a pool lifetime. Not scored."
        down_one "$Q"; down_one "$R"; continue
    fi

    if [ "$peak" -eq 0 ]; then
        echo "    the pool was NEVER populated: this is 'never came up', not 'came up and died'."
    elif [ -n "$died" ]; then
        echo "    the pool reached $peak and was EMPTY by t=${died}s, with no traffic at all."
        DIED["$rep"]="$died"
    else
        echo "    the pool reached $peak and was still $peak at t=${WATCH}s -- it SURVIVED the idle."
    fi

    # Now move a byte and see which path it takes, then watch for recovery.
    mbs=$(g "$Q"); case "${mbs:-}" in ''|*[!0-9.]*) mbs=FAILED ;; esac
    after_v="$(view "$Q")"
    printf '    transfer: %-9s -> %s\n' "$mbs" "$after_v"

    rec=""; t=0
    while [ "$t" -lt "$AFTER" ]; do
        sleep "$POLL"; t=$((t + POLL))
        rec+=" ${t}s:$(pool_of "$(view "$Q")")"
    done
    printf '    pool after the transfer:%s\n' "$rec"
    RECOVER+="  rep $rep:$rec"$'\n'
    scored=$((scored + 1))

    down_one "$Q"; down_one "$R"
    cool 30
done

echo
echo "=== what the client itself said (complementary -- the SERVER's pool is the state) ==="
vm "tail -n 25 \$HOME/out/poollife-$Q.log 2>/dev/null" 2>/dev/null | sed 's/^/    /'

echo
echo "=== verdict ==="
if [ "$scored" -eq 0 ]; then
    echo "  INSTRUMENT FAILURE: no repetition produced a readable series."
    echo "  Nothing above is a measurement; exiting non-zero so no resume marker is"
    echo "  written and the next run repeats this stage instead of skipping it."
    exit 2
fi
if [ "${#DIED[@]}" -eq 0 ]; then
    echo "  The idle direct pool SURVIVED in every scored repetition."
else
    printf '  The idle direct pool died in %s of %s scored repetitions, at:' "${#DIED[@]}" "$scored"
    for rep in $(seq 1 "$REPS"); do [ -n "${DIED[$rep]:-}" ] && printf ' rep%s=%ss' "$rep" "${DIED[$rep]}"; done
    echo
    echo "  With keepalive 3 s inside a 10 s idle timeout on BOTH ends, a pool that"
    echo "  empties under pure idle is a liveness defect, not a timeout working."
fi
echo
echo "  recovery after the transfer (a pool that comes back is a different defect"
echo "  from one that does not -- P-7 exists to make it come back):"
printf '%s' "$RECOVER"
echo
echo "DONE"
