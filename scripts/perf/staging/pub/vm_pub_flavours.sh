#!/usr/bin/env bash
# P5: the three ways an operator can expose a public port, compared
# budget-neutrally — native binary, dockerized binary, OpenSSH `-R`.
#
# A sequential ladder cannot do this on a burstable instance: the first flavour
# measured spends the allowance and every later one reads the shaped baseline
# (the vhost campaign measured exactly that — docker's x4 rung collapsed to
# 9.90 MB/s with allowance +597 390 while its own x1/x2 rungs were clean). So
# all three forwarders are registered AT ONCE against the same origin, each on
# its own public port, and every round hits them in ROTATION with a cooldown.
# A drifting budget then hits all three arms of a round roughly equally.
#
# The SSH leg is TCP-relay-only by design (I-SSH2: no --udp, no --carriers>1),
# so it is compared against the native/docker RELAY arms and never against
# their QUIC arms. That is a property of the transport, not a defect.
#
# TWO MODES, because the campaign has to answer the flavour question on BOTH
# transports and the answer cannot be assumed to carry over:
#
#   relay (default)  native vs docker vs OpenSSH -R, all on the TCP relay
#   udp              native vs docker, both on the QUIC direct path
#
# The `udp` mode exists because the dockerized client is NOT equivalent to the
# native one there: Docker clears every capability for a non-root UID, so the
# default uid-1000 image cannot call SO_*BUFFORCE and its direct UDP socket
# stays clamped to net.core.{r,w}mem_max. That is why this harness runs the
# ROOT image (`ghcr.io/manprint/bore:client`) — and why "docker and native are
# the same" has to be MEASURED on the direct path, not inherited from the
# relay result.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

MODE="${1:-relay}"
ROUNDS="${ROUNDS:-3}"
CONNS="${CONNS:-4}"
MB="${MB:-96}"
CARR="${CARR:---carriers 8}"
PER=$(( MB * 1048576 / CONNS ))

case "$MODE" in
relay)
    FLAVOURS=(native docker ssh)
    UDPFLAG=""
    declare -A PORTS=( [native]="${PUB_NATIVE:-9001}" [docker]="${PUB_DOCKER:-9002}" [ssh]="${PUB_SSH:-9003}" )
    ;;
udp)
    # Distinct ports so a udp run can never be confused with, or collide
    # against, a relay arm that is still registered.
    FLAVOURS=(native docker)
    UDPFLAG="--udp"
    declare -A PORTS=( [native]="${PUB_NATIVE_UDP:-9004}" [docker]="${PUB_DOCKER_UDP:-9005}" )
    ;;
*)  echo "usage: $0 [relay|udp]" >&2; exit 2 ;;
esac
declare -A LIVE=()

start_flavour() {
    local f="$1" p="${PORTS[$1]}"
    case "$f" in
    native)
        setsid nohup "$BORE" local "$RP" --port "$p" --to "$BORE_TO" --secret "$BORE_SECRET" $CARR $UDPFLAG \
            >"$OUT/fl-native-$MODE.log" 2>&1 </dev/null & ;;
    docker)
        # --network host so the container reaches the origin on the VM's own
        # loopback; without it the forwarder would have to traverse the bridge
        # and the measurement would include a NAT hop the native arm never pays.
        setsid nohup sudo -n docker run --rm --name "bore-pub-$p" --network host \
            ghcr.io/manprint/bore:client local "$RP" --port "$p" \
            --to "$BORE_TO" --secret "$BORE_SECRET" $CARR $UDPFLAG \
            >"$OUT/fl-docker-$MODE.log" 2>&1 </dev/null & ;;
    ssh)
        # D1 naming heuristic: a BARE NUMERIC port on -R means a public tunnel.
        setsid nohup sshpass -p "$SSHGW_PASS" ssh -T -o StrictHostKeyChecking=no \
            -o UserKnownHostsFile=/dev/null -o PubkeyAuthentication=no \
            -o PreferredAuthentications=password -o ExitOnForwardFailure=yes \
            -o ServerAliveInterval=30 -o LogLevel=ERROR \
            -R "$p:127.0.0.1:$RP" -p 443 "$SSHGW_USER@$GW" \
            >"$OUT/fl-ssh.log" 2>&1 </dev/null & ;;
    esac
    local i
    for i in $(seq 80); do
        present "$p" && { LIVE[$f]="$p"; echo "  $f live on public port $p"; return 0; }
        sleep 0.5
    done
    echo "  $f FAILED to register on $p"
    return 1
}

stop_flavours() {
    local f p
    for f in "${!PORTS[@]}"; do
        p="${PORTS[$f]}"
        case "$f" in
            # NEVER a blanket `pkill bore`: this deployment carries unrelated
            # live tunnels that belong to the operator. Match the exact port.
            native) pkill -9 -f "local $RP --port $p" 2>/dev/null ;;
            docker) sudo -n docker rm -f "bore-pub-$p" >/dev/null 2>&1 ;;
            ssh)    pkill -9 -f "\-R $p:127.0.0.1:$RP" 2>/dev/null ;;
        esac
    done
}
trap 'stop_flavours; cleanup; exit 130' INT TERM
trap 'stop_flavours; cleanup' EXIT

start_origins || exit 1
say "flavour rotation [$MODE]: $ROUNDS rounds, ${MB} MiB over $CONNS conns per burst, ${COOL}s cooldown"
echo "    carriers flag for native/docker: '$CARR'${UDPFLAG:+ plus $UDPFLAG}"
if [ "$MODE" = udp ]; then
    echo "    the SSH leg is ABSENT from this mode on purpose: an SSH forward is"
    echo "    TCP-relay-only by design (I-SSH2), so there is no QUIC arm to compare."
else
    echo "    (the SSH leg is TCP-relay-only by design)"
fi
for f in "${FLAVOURS[@]}"; do start_flavour "$f"; done
[ "${#LIVE[@]}" -gt 0 ] || { echo "no flavour registered"; exit 1; }

declare -A DL UP
for f in "${!LIVE[@]}"; do DL[$f]=""; UP[$f]=""; done

for dir in get put; do
    echo
    echo "  === $( [ "$dir" = get ] && echo download || echo upload ) ==="
    for r in $(seq "$ROUNDS"); do
        line="    round $r: "
        # Rotate which flavour goes first so no arm is always measured into a
        # freshly-drained budget.
        order=("${FLAVOURS[@]}")
        shift_by=$(( (r - 1) % ${#FLAVOURS[@]} ))
        order=("${order[@]:$shift_by}" "${order[@]:0:$shift_by}")
        for f in "${order[@]}"; do
            [ -n "${LIVE[$f]:-}" ] || continue
            p="${LIVE[$f]}"
            if [ "$dir" = get ]; then v=$(raw_get "$p" "$PER" "$CONNS"); DL[$f]="${DL[$f]} ${v:-0}"
            else v=$(raw_put "$p" "$PER" "$CONNS"); UP[$f]="${UP[$f]} ${v:-0}"; fi
            # In udp mode the transport is part of the result, not an
            # assumption: an arm that fell back is a RELAY number and must
            # never be quoted as a QUIC one.
            if [ "$MODE" = udp ]; then
                line="$line $f=${v:-0}($(tfld "$p" current_path))"
            else
                line="$line $f=${v:-0}"
            fi
            cool
        done
        echo "$line"
    done
    for f in "${FLAVOURS[@]}"; do
        [ -n "${LIVE[$f]:-}" ] || continue
        if [ "$dir" = get ]; then vals="${DL[$f]}"; else vals="${UP[$f]}"; fi
        echo "    median $dir $f: $(printf '%s\n' $vals | med) MB/s"
    done
done

echo
echo "  === latency, one new connection per probe ==="
for f in "${FLAVOURS[@]}"; do
    [ -n "${LIVE[$f]:-}" ] || continue
    echo "    $f: $(raw_ping "${LIVE[$f]}" 60)"
done

echo
echo "  === server view at the end ==="
for f in "${FLAVOURS[@]}"; do
    [ -n "${LIVE[$f]:-}" ] || continue
    p="${LIVE[$f]}"
    echo "    $f port=$p $(tsnap "$p" | jq -r '"path=\(.current_path) carriers=\(.carriers) opens=\(.direct_stream_opens) fb=\(.direct_fallbacks) pool=\(.direct_pool) active=\(.active)"' 2>/dev/null)"
done
echo DONE
