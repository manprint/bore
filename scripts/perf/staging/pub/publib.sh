#!/usr/bin/env bash
# Shared plumbing for the VM-side PUBLIC-tunnel harnesses.
#
# Sourced by every `vm_pub_*.sh`. It runs ON THE TEST VM, so it reads ~/env.sh
# (written by provision.sh, mode 600) rather than the workstation environment,
# and it hardcodes no host, port or credential.
#
# Deliberately NOT `set -e`: a benchmark that aborts halfway leaves registered
# tunnels behind and prints a partial result that looks complete.
set -uo pipefail
H="${BORE_PERF_HOME:-$HOME}"
# shellcheck disable=SC1091
. "$H/env.sh"

BORE="$H/bore"
OHA="$H/oha"
OUT="$H/out"; mkdir -p "$OUT"
# HTTP byte source (bench_origin.py) and RAW TCP byte source (raw_origin.py).
OP="${ORIGIN_PORT:-5052}"
RP="${RAW_ORIGIN_PORT:-5053}"
RAWCLI="$H/raw_client.py"

# Every process this harness starts, killed on EVERY exit path. NEVER
# `pkill bore`: this deployment carries unrelated live tunnels that belong to
# the operator, and a blanket kill takes them down (project rule).
KIDS=()
cleanup() { for p in "${KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'cleanup; exit 130' INT TERM
trap cleanup EXIT

# --- admin API, PUBLIC tunnels ---------------------------------------------
adm()  { curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
# One fetch, many fields: /tunnels is polled in tight loops and a field-per-call
# helper turns one sample into five inconsistent ones.
# NOTE the field is `public_port`, not `port`, and the live connection count is
# `active`, not `active_conns` — TunnelView's names, verified against
# src/admin_views.rs. Guessing them cost a whole smoke run: the tunnel was
# registered and `present` still said no.
tsnap() { adm tunnels | jq -c --argjson p "$1" '.[]|select(.public_port==$p)' 2>/dev/null; }
tfld()  { tsnap "$1" | jq -r --arg f "$2" '.[$f] // empty' 2>/dev/null; }
pport() { adm tunnels | jq -r '.[].public_port' 2>/dev/null; }
present() { adm tunnels | jq -e --argjson p "$1" 'any(.[]; .public_port==$p)' >/dev/null 2>&1; }

# --- origins ----------------------------------------------------------------
# reap_stale_origin <script_basename> <port>
#   An origin is reused across stages on purpose (restarting it between stages
#   would re-warm the page cache and change the numbers). But a LONG-LIVED
#   origin can outlive an EDIT to its own source: H-7 in the public campaign
#   had the P6 ladder report `held=N up=0 errs=N` on every rung for exactly
#   this reason — the running `raw_origin.py` had been started 40 minutes
#   before the `HOLD` verb was added to the file, and `pgrep` happily called
#   that "already running". A benchmark that silently measures an older
#   program than the one in the tree is worse than one that refuses to run.
#   So: if the process is OLDER than its own script file, kill THAT PID (never
#   a pattern-wide `pkill`, project rule) and let the caller start it fresh.
reap_stale_origin() {
    local script="$1" port="$2" pid age fage
    pid=$(pgrep -f "$script $port" | head -1) || return 0
    [ -n "$pid" ] || return 0
    age=$(ps -o etimes= -p "$pid" 2>/dev/null | tr -d ' ')
    [ -n "$age" ] || return 0
    fage=$(( $(date +%s) - $(stat -c %Y "$H/$script" 2>/dev/null || echo 0) ))
    if [ "$age" -gt "$fage" ]; then
        echo "reaping stale $script (pid $pid, ${age}s old, script ${fage}s old)" >&2
        kill -9 "$pid" 2>/dev/null
        for _ in $(seq 20); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    fi
}

start_origins() {
    reap_stale_origin bench_origin.py "$OP"
    reap_stale_origin raw_origin.py "$RP"
    pgrep -f "bench_origin.py $OP" >/dev/null 2>&1 || {
        python3 "$H/bench_origin.py" "$OP" >"$OUT/origin.log" 2>&1 &
        KIDS+=("$!")
    }
    pgrep -f "raw_origin.py $RP" >/dev/null 2>&1 || {
        python3 "$H/raw_origin.py" "$RP" >"$OUT/raworigin.log" 2>&1 &
        KIDS+=("$!")
    }
    for _ in $(seq 40); do
        curl -fsS -m 2 -o /dev/null "http://127.0.0.1:$OP/ping" 2>/dev/null &&
            python3 "$RAWCLI" ping 127.0.0.1 "$RP" 1 >/dev/null 2>&1 && return 0
        sleep 0.25
    done
    echo "origins failed to start" >&2
    return 1
}

# --- tunnel lifecycle -------------------------------------------------------
# up_native <local_port> <requested_public_port> [flags...]
#   Starts `bore local` and waits until the server publishes the tunnel.
#   Sets LASTPID and LASTPORT. Requesting port 0 lets the server choose, which
#   is what a real user does; the chosen port is discovered from the client log.
up_native() {
    local lp="$1" want="$2"; shift 2
    local log="$OUT/pub-$$-$RANDOM.log"
    "$BORE" local "$lp" --to "$BORE_TO" --secret "$BORE_SECRET" --port "$want" "$@" \
        >"$log" 2>&1 &
    LASTPID=$!
    LASTLOG="$log"
    LASTPORT=""
    local i
    for i in $(seq 120); do
        kill -0 "$LASTPID" 2>/dev/null || { echo "client died: $(tail -3 "$log")" >&2; return 1; }
        LASTPORT=$(grep -oE 'listening at [^:]+:[0-9]+' "$log" | tail -1 | grep -oE '[0-9]+$')
        if [ -n "$LASTPORT" ] && present "$LASTPORT"; then
            KIDS+=("$LASTPID")
            return 0
        fi
        sleep 0.5
    done
    echo "tunnel never registered: $(tail -3 "$log")" >&2
    kill -9 "$LASTPID" 2>/dev/null
    return 1
}

# down <pid> <port> — kill the client and wait for the server to release the port.
down() {
    kill -9 "$1" 2>/dev/null
    # Reap the job before returning. Without this, bash notifies the death of
    # its own background job asynchronously and prints a `... Killed  "$BORE"
    # local ...` line into the MIDDLE of the stage log, right after whatever
    # measurement happened to be printing next. The kill is the harness's own
    # deliberate teardown, so the notice is pure noise in a transcript that is
    # read as evidence.
    wait "$1" 2>/dev/null
    local i
    for i in $(seq 30); do present "$2" || return 0; sleep 0.5; done
    return 1
}

# --- measurement primitives -------------------------------------------------
# Rates are computed from the SERVER's own relay_tx_bytes where a relay is
# involved, and from the client-observed byte count otherwise. On the QUIC
# direct path the bytes never touch the relay counter, so a single metric
# cannot cover both transports: the raw driver's own count is used throughout
# and the server counter is reported alongside as a cross-check.
now() { date +%s.%N; }
mbs() { LC_ALL=C awk -v b="$1" -v s="$2" -v e="$3" 'BEGIN{d=e-s; if(d<=0){print "0.00";exit} printf "%.2f", b/1048576/d}'; }
med() { LC_ALL=C sort -g | awk '{v[NR]=$1} END{if(NR==0){print "nan";exit} print (NR%2)?v[(NR+1)/2]:(v[NR/2]+v[NR/2+1])/2}'; }
ratio() { LC_ALL=C awk -v a="$1" -v b="$2" 'BEGIN{if(b>0) printf "%.3f", a/b; else print "nan"}'; }

# raw_get <public_port> <bytes-per-conn> <conns> -> "MBs"
raw_get() { python3 "$RAWCLI" get "$GW" "$1" "$2" "$3" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
raw_put() { python3 "$RAWCLI" put "$GW" "$1" "$2" "$3" 2>/dev/null | grep -oE 'MBs=[0-9.]+' | cut -d= -f2; }
raw_ping() { python3 "$RAWCLI" ping "$GW" "$1" "$2" 2>/dev/null; }

# http_get <public_port> <path> <seconds> -> MB/s, from the client's own count.
# Public tunnels are plain TCP: there is no TLS on the tunnel port unless the
# tunnel asked for it, so this is http:// and NOT https://.
http_get() {
    local port="$1" path="$2" secs="$3" t0 t1 by
    t0=$(now)
    by=$(curl -fsS -o /dev/null --max-time "$secs" -w '%{size_download}' \
        "http://$GW:$port$path" 2>/dev/null || echo 0)
    t1=$(now)
    mbs "${by:-0}" "$t0" "$t1"
}

# --- instance allowance -----------------------------------------------------
# The VM has no SSH access to the server host, so the allowance counters are
# NOT read here. The workstation samples them on a timeline for the whole run
# (`res/ena_timeline.sh`) and every stage's wall-clock window is stamped into
# its own output, which is what lets a shaped burst be identified afterwards
# instead of silently averaged in.
#
# Cooldown between bursts. One 4-stream 10 s download is roughly a whole
# inbound burst budget on this instance class, so back-to-back arms measure the
# token bucket instead of the tunnel.
COOL="${COOL:-75}"
cool() { sleep "${1:-$COOL}"; }

say() { echo "### $* — $(date -Is)"; }
lab() { printf 'p%s' "$(date +%s%N | cut -c9-13)"; }
