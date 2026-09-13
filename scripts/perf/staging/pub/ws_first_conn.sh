#!/usr/bin/env bash
# W1e: what does the FIRST proxied connection of a tunnel cost, and why?
#
# THE OBSERVATION THIS EXISTS TO EXPLAIN
# ---------------------------------------
# `ws_dl1.sh` measures six paired single-connection downloads, relay against
# QUIC direct, and the first pair is low on the QUIC arm in BOTH runs of this
# campaign: 18.6 % below the median of the later pairs at 384 MiB and 21.1 %
# below at 96 MiB. It is not a position effect -- the arm order alternates and
# the QUIC arm runs FIRST in the even pairs, where it reads full rate. It is the
# first QUIC transfer of the stage, and only that one.
#
# It is also a RATE effect and not a fixed setup cost: the penalty in time grows
# with the transfer, 0.303 s at 96 MiB and 0.871 s at 384 MiB. A handshake or a
# stream open would cost the same in both (one RTT here is 19 ms).
#
# WHAT SEPARATES THE CANDIDATE EXPLANATIONS
# ------------------------------------------
# Three explanations fit that shape and the admin API tells them apart, which is
# the whole design of this stage -- measure the rate AND read the server's own
# view of the transport around the same transfer:
#
#   cold congestion control   path=direct, direct_stream_opens +1, fallbacks +0
#   served on the warm relay  path=relay,  opens +0,              fallbacks +1
#   carriers dialled lazily   direct_pool rises from 0 across the first transfer
#
# THE CONTROL IS A RELAY TUNNEL, AND IT IS NOT OPTIONAL
# -----------------------------------------------------
# "The first transfer is slow" is only interesting if it is NOT true of every
# tunnel. A plain relay tunnel registered at the same moment and driven with the
# same sequence answers that. Its own first transfer carries the server's TCP
# carrier warm-up and this workstation's, so whatever it shows is the part that
# belongs to neither transport in particular.
#
# The two tunnels are registered up front and then driven ALTERNATELY, so line
# drift is common to both. They cannot be interleaved within a transfer -- a
# tunnel has exactly one first connection, which is the quantity under test.
#
# COST: 6 arms x XFER_MB. Every arm is a download, so every arm is AWS egress:
# 2.25 GiB at the default. Declared, because a stage whose cost is invisible
# gets re-run without thinking.
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C
RAWCLI="$(cd "$(dirname "$0")/../.." && pwd)/raw_client.py"
XFER_MB="${XFER_MB:-384}"
XFERS="${XFERS:-3}"
RP=5053; R=9047; Q=9048; PER=$(( XFER_MB*1048576 ))

UP=()
up() { # <port> <extra flags>
    vm "setsid nohup \$HOME/bore local $RP --port $1 --to '$BORE_TO' --secret '$BORE_SECRET' --carriers 1 $2 \
        > \$HOME/out/wsfirst-$1.log 2>&1 </dev/null & true" >/dev/null 2>&1
    local i; for i in $(seq 80); do
        adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
            && { UP+=("$1"); return 0; }
        sleep 0.5
    done
    return 1
}
down() { local p; for p in "${UP[@]:-}"; do vm "pkill -9 -f \"local $RP --port $p\" 2>/dev/null; true" >/dev/null 2>&1; done; }
trap 'down' EXIT

# The server's own view of one tunnel, as four fields on one line. An empty
# answer is printed as `?` and NEVER as a zero: "the API did not answer" and
# "the counter did not move" are opposite readings and used to look identical
# (the standing rule this campaign has paid for six times).
view() { # <port> -> "path opens fallbacks pool"
    # MEASURED: on empty input `jq` prints NOTHING and exits 0, so a `|| echo`
    # tail never fires and the caller gets an empty string -- which printf then
    # renders as a short, silent row. An unanswered API must LOOK unanswered.
    local v
    v="$(adm tunnels 2>/dev/null | jq -r --argjson p "$1" '
        [ .[] | select(.public_port == $p) ] as $t
        | if ($t|length) == 0 then "? ? ? ?"
          else "\($t[0].current_path // "?") \($t[0].direct_stream_opens // "?") \($t[0].direct_fallbacks // "?") \($t[0].direct_pool // "?")"
          end' 2>/dev/null)"
    [ -n "$v" ] || v="? ? ? ?"
    printf '%s' "$v"
}

g() { python3 "$RAWCLI" get "$BORE_GW" "$1" "$PER" 1 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }

echo "=== first-connection cost, ${XFER_MB} MiB per transfer, ONE connection, $XFERS transfers per arm ==="
echo "  relay tunnel on $R (control), --udp tunnel on $Q; registered up front, driven alternately"
echo

up "$R" ""      || { echo "relay arm failed to register"; exit 1; }
up "$Q" "--udp" || { echo "quic arm failed to register";  exit 1; }

# BEFORE ANY TRAFFIC. This row is the one that can answer the lazy-carrier
# question on its own: a direct pool that is already populated here cannot be
# what the first transfer is paying for.
printf '  %-7s %-4s %-9s %-12s %-7s %-7s %-6s\n' arm xfer MBs path opens fallb pool
printf '  %-7s %-4s %-9s %s\n' relay  0 '(no traffic)' "$(view "$R")"
printf '  %-7s %-4s %-9s %s\n' quic   0 '(no traffic)' "$(view "$Q")"

declare -A SAMP
for n in $(seq 1 "$XFERS"); do
    for arm in relay quic; do
        case "$arm" in relay) port=$R ;; quic) port=$Q ;; esac
        pre="$(view "$port")"
        mbs=$(g "$port")
        post="$(view "$port")"
        case "${mbs:-}" in ''|*[!0-9.]*) mbs=FAILED ;; *) SAMP["$arm"]+=" $mbs" ;; esac
        printf '  %-7s %-4s %-9s %s\n' "$arm" "$n" "$mbs" "$post"
        printf '  %-7s %-4s %-9s %s\n' "" "" '  (before)' "$pre"
        cool 75
    done
done

echo
echo "=== rate by transfer number (MB/s) ==="
for arm in relay quic; do printf '  %-7s%s\n' "$arm" "${SAMP["$arm"]:-  (none)}"; done

echo
echo "=== reading ==="
echo "  If transfer 1 on the quic arm is slow while the relay control is flat,"
echo "  the cost belongs to the DIRECT path. Then the counters say which of the"
echo "  three it is: path=relay with fallbacks +1 means it was never direct;"
echo "  path=direct with opens +1 and a pool already populated before any"
echo "  traffic means a cold congestion controller and nothing else; a pool that"
echo "  only fills across transfer 1 means the carriers are dialled lazily."
echo "  If BOTH arms are slow on transfer 1 the cost is not the transport's --"
echo "  it is the path, this workstation, or the origin waking up."
echo
echo "DONE"
