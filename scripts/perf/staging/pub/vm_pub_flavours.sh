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
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/publib.sh"

ROUNDS="${ROUNDS:-3}"
CONNS="${CONNS:-4}"
MB="${MB:-96}"
CARR="${CARR:---carriers 8}"
PER=$(( MB * 1048576 / CONNS ))

# One fixed public port per flavour so all three can be live simultaneously.
declare -A PORTS=( [native]="${PUB_NATIVE:-9001}" [docker]="${PUB_DOCKER:-9002}" [ssh]="${PUB_SSH:-9003}" )
declare -A LIVE=()

start_flavour() {
    local f="$1" p="${PORTS[$1]}"
    case "$f" in
    native)
        setsid nohup "$BORE" local "$RP" --port "$p" --to "$BORE_TO" --secret "$BORE_SECRET" $CARR \
            >"$OUT/fl-native.log" 2>&1 </dev/null & ;;
    docker)
        # --network host so the container reaches the origin on the VM's own
        # loopback; without it the forwarder would have to traverse the bridge
        # and the measurement would include a NAT hop the native arm never pays.
        setsid nohup sudo -n docker run --rm --name "bore-pub-$p" --network host \
            ghcr.io/manprint/bore:client local "$RP" --port "$p" \
            --to "$BORE_TO" --secret "$BORE_SECRET" $CARR \
            >"$OUT/fl-docker.log" 2>&1 </dev/null & ;;
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
say "flavour rotation: $ROUNDS rounds, ${MB} MiB over $CONNS conns per burst, ${COOL}s cooldown"
echo "    carriers flag for native/docker: '$CARR' (the SSH leg is TCP-relay-only by design)"
for f in native docker ssh; do start_flavour "$f"; done
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
        order=(native docker ssh)
        shift_by=$(( (r - 1) % 3 ))
        order=("${order[@]:$shift_by}" "${order[@]:0:$shift_by}")
        for f in "${order[@]}"; do
            [ -n "${LIVE[$f]:-}" ] || continue
            p="${LIVE[$f]}"
            if [ "$dir" = get ]; then v=$(raw_get "$p" "$PER" "$CONNS"); DL[$f]="${DL[$f]} ${v:-0}"
            else v=$(raw_put "$p" "$PER" "$CONNS"); UP[$f]="${UP[$f]} ${v:-0}"; fi
            line="$line $f=${v:-0}"
            cool
        done
        echo "$line"
    done
    for f in native docker ssh; do
        [ -n "${LIVE[$f]:-}" ] || continue
        if [ "$dir" = get ]; then vals="${DL[$f]}"; else vals="${UP[$f]}"; fi
        echo "    median $dir $f: $(printf '%s\n' $vals | med) MB/s"
    done
done

echo
echo "  === latency, one new connection per probe ==="
for f in native docker ssh; do
    [ -n "${LIVE[$f]:-}" ] || continue
    echo "    $f: $(raw_ping "${LIVE[$f]}" 60)"
done

echo
echo "  === server view at the end ==="
for f in native docker ssh; do
    [ -n "${LIVE[$f]:-}" ] || continue
    p="${LIVE[$f]}"
    echo "    $f port=$p $(tsnap "$p" | jq -r '"path=\(.current_path) carriers=\(.carriers) opens=\(.direct_stream_opens) fb=\(.direct_fallbacks) pool=\(.direct_pool) active=\(.active_conns)"' 2>/dev/null)"
done
echo DONE
