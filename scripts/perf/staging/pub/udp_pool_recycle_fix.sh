#!/usr/bin/env bash
# Does the P-14 correction hold on a REAL path, against a REAL server?
#
# WHY THIS EXISTS BESIDE THE UNIT TEST AND THE NETNS GATE
# -------------------------------------------------------
# The defect was found in the field (§48) and the correction is gated twice in
# process: a red-checked unit that holds the old entry and closes its pool by
# hand, and `T-PUB-POOLRECYCLE` in `scripts/perf/public_idle_window.sh`, which
# runs a real server and a real client inside a rootless netns with the QUIC
# idle timeout cut to 2 s so the eviction, if it happened, would happen in
# seconds. Both are necessary. Neither is the shape the defect was MEASURED in:
# a client on one host, a server on another, the shipped 10 s idle timeout, and
# a WAN between them.
#
# So this stage repeats `udp_pool_recycle.sh`'s experiment — the same two arms,
# one variable wide — with ONE thing changed: the server is the CORRECTED
# binary. The old stage's result is the red-check and is already recorded
# (FRESH survived 45 s 2/2, RECYCLED died at t=12 s 2/2, §48). A "after" with
# no "before" beside it does not demonstrate a correction; §43 is that rule and
# this pair is how it is honoured here.
#
# WHY NOT STAGING. The server this workstation's campaigns normally interrogate
# is the operator's own, carrying live tunnels; redeploying it restarts it and
# drops them, which needs explicit approval and is not in this plan. The TEST
# VM is the right host and already proven reachable on these ports: the jump
# campaign runs a `bore server` there on 7845 with `--vhost-quic-port 7847` and
# dials it from this workstation. Those two ports are therefore the ones used
# here, and it is also why this stage must NEVER run beside a jump stage.
#
# DIRECTION IS DELIBERATE. In §48 the client sat on the VM and the server was
# staging. Here it is the other way round, because the corrected binary is the
# SERVER's and the server is the party that owns the defect. The client is this
# workstation, so the QUIC carrier crosses the real WAN exactly as a user's
# would.
#
# COST: ZERO bandwidth. Not one byte is proxied — nobody ever connects to the
# public port. The whole stage is registration plus admin-API polling.
set -uo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib.sh"
export LC_ALL=C

CTRL="${CTRL:-7845}"          # known-open on the test VM (jump campaign)
QUIC="${QUIC:-7847}"          # ditto
P="${P:-9081}"                # public port; never connected to
LOCAL="${LOCAL:-5055}"        # a local port on THIS host; never connected to
REPS="${REPS:-2}"
WATCH="${WATCH:-45}"
POLL="${POLL:-2}"
WS_BORE="${WS_BORE:-$PWD/target/release/bore}"
REMOTE="\$HOME/bore-p14"

RUN="p14$(date +%s%N | tail -c 6)"
WS_PIDS=()

cleanup() {
    local p
    for p in "${WS_PIDS[@]:-}"; do [ -n "$p" ] && kill -TERM "$p" 2>/dev/null; done
    # Narrow, per-run patterns only. A blanket `pkill bore` would kill the
    # operator's own live tunnels on either host -- project rule.
    vm "pkill -TERM -f -- 'server --udp --control-port $CTRL' 2>/dev/null; true" >/dev/null 2>&1
}
trap cleanup EXIT

adm_vm() { # <path> -- the VM server's admin API, read over the WAN
    curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" \
        "http://$BORE_VM:$CTRL/admin/api/v1/$1" 2>/dev/null
}

echo "### does the P-14 correction hold on the real path? -- $(date -Is)"
echo "  server: the TEST VM, CORRECTED binary   client: this workstation"
echo "  $REPS reps, watch ${WATCH}s on a ${POLL}s grid, port $P, ZERO bytes transferred"
echo

# --- provisioning -----------------------------------------------------------
# THE BINARY MUST BE THE CORRECTED ONE, AND THAT IS ASSERTED, NOT ASSUMED.
# A stage that ships whatever happens to sit in `target/release` and then
# reports "the defect is gone" has measured the deployment, not the fix. The
# version string carries the branch and commit (build.rs), and the checksums of
# the two ends must agree.
[ -x "$WS_BORE" ] || { echo "INSTRUMENT FAILURE: no binary at $WS_BORE"; exit 2; }
sha_local=$(sha256sum "$WS_BORE" | cut -c1-16)
ver_local=$("$WS_BORE" --version 2>/dev/null | head -1)
echo "  local  : $sha_local  $ver_local"

vm "mkdir -p \$HOME/out" >/dev/null 2>&1
# Copy BESIDE then rename: writing onto a file a previous run may still be
# executing fails with ETXTBSY, and `mv` unlinks the old inode instead.
if ! scp -q "${SSH_OPTS[@]}" "$WS_BORE" "$BORE_VM_USER@$BORE_VM:$REMOTE.new" 2>/dev/null; then
    echo "INSTRUMENT FAILURE: could not ship the binary to the test VM"; exit 2
fi
vm "mv -f $REMOTE.new $REMOTE && chmod +x $REMOTE" >/dev/null 2>&1
sha_vm=$(vm "sha256sum $REMOTE | cut -c1-16" 2>/dev/null | tr -d '\r\n ')
ver_vm=$(vm "$REMOTE --version 2>/dev/null | head -1" 2>/dev/null | tr -d '\r')
echo "  on VM  : $sha_vm  $ver_vm"
if [ "$sha_local" != "$sha_vm" ]; then
    echo "INSTRUMENT FAILURE: the VM is running a DIFFERENT binary ($sha_local vs $sha_vm)"
    exit 2
fi

# Free the ports a previous run may still hold, then start the server.
vm "pkill -TERM -f -- 'server --udp --control-port $CTRL' 2>/dev/null; true" >/dev/null 2>&1
vm "setsid nohup $REMOTE server --udp --control-port $CTRL --vhost-quic-port $QUIC \
      --min-port $P --max-port $P --secret '$BORE_SECRET' \
      --admin-token '$ADMIN_TOKEN' \
      > \$HOME/out/$RUN-server.log 2>&1 </dev/null & true" >/dev/null 2>&1

srv_up=""
for i in $(seq 60); do
    if adm_vm config >/dev/null 2>&1; then srv_up=1; break; fi
    sleep 1
done
[ -n "$srv_up" ] || { echo "INSTRUMENT FAILURE: the VM server never answered its admin API"; exit 2; }
echo "  server : $(adm_vm config | jq -r '"\(.server_version)  keepalive=\(.direct_quic_keepalive_ms)ms idle=\(.direct_quic_idle_ms)ms"' 2>/dev/null)"
echo

# --- the two arms -----------------------------------------------------------
up() { # -> prints the client pid, or nothing
    local pid
    # The control port rides in --to; `bore local` has no --control-port flag.
    "$WS_BORE" local "$LOCAL" --to "$BORE_VM:$CTRL" --port "$P" --udp --carriers 1 \
        --secret "$BORE_SECRET" \
        > "$WORK/$RUN-client.log" 2>&1 </dev/null &
    pid=$!
    WS_PIDS+=("$pid")
    local i
    for i in $(seq 80); do
        adm_vm tunnels 2>/dev/null | jq -e --argjson p "$P" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 \
            && { printf '%s' "$pid"; return 0; }
        kill -0 "$pid" 2>/dev/null || return 1
        sleep 0.5
    done
    return 1
}

down() { # <pid> -- kill and WAIT for the server to forget the entry
    kill -KILL "$1" 2>/dev/null
    local i
    for i in $(seq 60); do
        adm_vm tunnels 2>/dev/null | jq -e --argjson p "$P" 'any(.[]; .public_port==$p)' >/dev/null 2>&1 || return 0
        sleep 1
    done
    return 1
}

pool_of() {
    local v
    v="$(adm_vm tunnels 2>/dev/null | jq -r --argjson p "$P" \
        '[.[]|select(.public_port==$p)] as $t | if ($t|length)==0 then "?" else "\($t[0].direct_pool // "?")" end' 2>/dev/null)"
    [ -n "$v" ] || v="?"
    printf '%s' "$v"
}

watch_arm() { # -> "died=<s>" | "survived=<peak>" | "never=1"
    local t=0 peak=0 died="" p series=""
    while [ "$t" -lt "$WATCH" ]; do
        p="$(pool_of)"
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

FRESH=""; RECYC=""; scored=0
for rep in $(seq 1 "$REPS"); do
    echo "  --- rep $rep"
    echo "    FRESH (nothing died on this port recently)"
    pid=$(up) || { echo "      FAILED: the tunnel never registered"; continue; }
    FRESH+=" $(watch_arm)"
    down "$pid" || echo "      WARNING: the server still lists port $P"

    echo "    RECYCLED (the previous tunnel on this port died moments ago)"
    pid=$(up) || { echo "      FAILED: the tunnel never re-registered"; continue; }
    RECYC+=" $(watch_arm)"
    down "$pid" || echo "      WARNING: the server still lists port $P"
    scored=$((scored + 1))
done

echo
echo "=== the client's account ==="
TOKENSAVE_DISABLE_GREP_HOOK=1 grep -a 'direct udp carrier ready\|direct udp carrier renewed' "$WORK/$RUN-client.log" 2>/dev/null | tail -n 6 | sed 's/^/    /'
echo
echo "=== the server's account: which id each install minted on port $P ==="
vm "grep -a 'port:$P' \$HOME/out/$RUN-server.log 2>/dev/null | grep -a 'direct carrier' | tail -n 8" 2>/dev/null | sed 's/^/    /'
echo "    (per-pool ids restart at 0 by design; what the correction changes is"
echo "     WHICH pool the close monitor removes from, which no log line shows.)"

echo
echo "=== verdict ==="
printf '  FRESH   :%s\n' "${FRESH:- (none)}"
printf '  RECYCLED:%s\n' "${RECYC:- (none)}"
echo
if [ "$scored" -eq 0 ]; then
    echo "  INSTRUMENT FAILURE: no repetition completed both arms."
    exit 2
fi
fresh_died=$(printf '%s' "$FRESH" | tr ' ' '\n' | grep -c '^died=' || true)
recyc_died=$(printf '%s' "$RECYC" | tr ' ' '\n' | grep -c '^died=' || true)
echo "  died: FRESH $fresh_died/$scored, RECYCLED $recyc_died/$scored"
echo "  (before the correction, on the same experiment: FRESH 0/2, RECYCLED 2/2 -- §48)"
if [ "$recyc_died" -eq 0 ] && [ "$fresh_died" -eq 0 ]; then
    echo "  PASS: a re-registered tunnel keeps its own direct carrier on the real path."
    echo
    echo "DONE"
    exit 0
fi
echo "  FAIL: a carrier was still lost. The correction does not hold here, or"
echo "  something else removes it -- read the series above before concluding."
exit 1
