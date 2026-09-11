#!/usr/bin/env bash
# Shared plumbing for the SECRET-tunnel staging campaign.
#
# A secret tunnel is the only one of bore's four registries whose fast path
# does not touch the server: the relay arm is consumer -> server -> provider,
# and the direct arm is consumer <-> provider over a hole-punched QUIC
# connection the server only brokered. Three consequences shape every script
# in this directory, and they are why `pub/publib.sh` could not simply be
# copied:
#
#  1. TWO clients, not one. Every measurement starts a provider AND a
#     consumer, on hosts chosen per topology (WS<->VM, VM<->WS, VM<->VM), and
#     the traffic driver runs on the CONSUMER's host, against the consumer's
#     own `--local-proxy-port` on loopback.
#
#  2. The path is reported by the CONSUMER row (S-1). The server is not an
#     endpoint of the direct path, so it cannot observe it; the consumer
#     reports what it chose through `ClientMessage::SecretPathReport` and the
#     admin API republishes it as `current_path` / `direct_fallbacks` /
#     `path_reason`. A provider row's `current_path` is "unknown" for a --udp
#     tunnel BY DESIGN — never read the path off the provider.
#
#  3. The relay arm crosses the server TWICE (two transits, two legs of WAN),
#     while the direct arm crosses the Internet once. Unlike the vhost and
#     public campaigns — where the relay is a single hop and measured FASTER
#     than QUIC direct on a clean path — the secret direct path is expected to
#     win, and by a margin that depends on where the two peers sit. That is
#     what the topology matrix is for.
#
# NEVER `pkill bore`: this deployment carries the operator's own live tunnels
# (project rule). Everything started here is killed by the PID it was started
# with; remote processes are matched by their per-run unique secret id, which
# no other process on the box can carry.
set -uo pipefail
HERE_SEC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE_SEC/../lib.sh"

# Byte source on the PROVIDER host's loopback. `raw_origin.py` is the same
# program the public campaign uses, so a MB/s here and a MB/s there are the
# same measurement of the same server-side work.
RP="${RAW_ORIGIN_PORT:-5053}"
OP="${ORIGIN_PORT:-5052}"
# Where the consumer publishes its local end. Chosen high and per-run so two
# stages cannot collide on a box that also runs the operator's tunnels.
SEC_PROXY_PORT="${SEC_PROXY_PORT:-15300}"

# Binaries. The workstation runs the release build from this repository (the
# same bytes the netns gates ran against); the VM runs the provisioned one.
WS_BORE="${WS_BORE:-$HERE_SEC/../../../../target/release/bore}"
VM_BORE="${VM_BORE:-$VM_HOME/bore}"
WS_RAWCLI="${WS_RAWCLI:-$HERE_SEC/../../raw_client.py}"
VM_RAWCLI="$VM_HOME/raw_client.py"

SEC_KIDS=()
sec_cleanup() {
    local p
    for p in "${SEC_KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
}
trap 'sec_cleanup; exit 130' INT TERM
trap sec_cleanup EXIT

# --- admin API, SECRET registry --------------------------------------------
# One fetch per question is deliberate: /admin/api/v1/secret is small and the
# alternative (cache a snapshot) turns a two-field read into two samples of
# different moments, which is exactly how a path flip gets attributed to the
# wrong arm.
sec_json() { adm secret 2>/dev/null; }

# sec_field <role> <secret-id> <jq-field> [default]
#   role is `secretprovider` or `secretconsumer`.
sec_field() {
    local role="$1" id="$2" field="$3" dflt="${4:-}"
    sec_json | jq -r --arg r "$role" --arg i "$id" --arg d "$dflt" \
        "[.[] | select(.role==\$r and .secret_id==\$i)][0].$field // \$d" \
        2>/dev/null || printf '%s' "$dflt"
}

sec_present() {
    sec_json | jq -e --arg r "$1" --arg i "$2" \
        'any(.[]; .role==$r and .secret_id==$i)' >/dev/null 2>&1
}

# sec_wait <role> <secret-id> [tries]
sec_wait() {
    local role="$1" id="$2" tries="${3:-120}" i
    for i in $(seq "$tries"); do
        sec_present "$role" "$id" && return 0
        sleep 0.5
    done
    return 1
}

# --- process lifecycle ------------------------------------------------------
# Each start helper returns the PID it started (local) or prints the remote
# PID (remote). A run's secret id is unique, so the remote pattern match is as
# narrow as a PID and survives a box that runs other bore processes.
sec_id() { printf 'sec%s' "$(date +%s%N | cut -c9-14)"; }

# ws_provider <local-port> <secret-id> [flags...]
ws_provider() {
    local lp="$1" id="$2"; shift 2
    "$WS_BORE" local "$lp" --to "$BORE_TO" --secret "$BORE_SECRET" \
        --tcp-secret-id "$id" "$@" >"$OUT/prov-$id.log" 2>&1 &
    LASTPID=$!
    SEC_KIDS+=("$LASTPID")
}

# ws_consumer <proxy-port> <secret-id> [flags...]
ws_consumer() {
    local pp="$1" id="$2"; shift 2
    "$WS_BORE" proxy --to "$BORE_TO" --secret "$BORE_SECRET" \
        --tcp-secret-id "$id" --local-proxy-port "127.0.0.1:$pp" "$@" \
        >"$OUT/cons-$id.log" 2>&1 &
    LASTPID=$!
    SEC_KIDS+=("$LASTPID")
}

# vm_provider / vm_consumer — same, started detached on the test VM. The log
# name carries the id so a post-mortem can pair it with the admin row.
vm_provider() {
    local lp="$1" id="$2"; shift 2
    vm "mkdir -p ~/out; setsid nohup $VM_BORE local $lp --to '$BORE_TO' --secret '$BORE_SECRET' \
        --tcp-secret-id $id $* > ~/out/prov-$id.log 2>&1 </dev/null & true" >/dev/null 2>&1
}
vm_consumer() {
    local pp="$1" id="$2"; shift 2
    vm "mkdir -p ~/out; setsid nohup $VM_BORE proxy --to '$BORE_TO' --secret '$BORE_SECRET' \
        --tcp-secret-id $id --local-proxy-port 127.0.0.1:$pp $* > ~/out/cons-$id.log 2>&1 </dev/null & true" >/dev/null 2>&1
}

# sec_down <secret-id> [local-pids...]
#   Kills the local PIDs explicitly and the remote ones by the run's own id.
#   `pkill -f "--tcp-secret-id <id>"` is a narrowly-matched pattern, not a
#   blanket kill: the id is minted per run and exists nowhere else.
sec_down() {
    local id="$1"; shift
    local p
    for p in "$@"; do kill -9 "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
    vm "pkill -9 -f -- '--tcp-secret-id $id' 2>/dev/null; true" >/dev/null 2>&1
    local i
    for i in $(seq 30); do
        sec_present secretconsumer "$id" || sec_present secretprovider "$id" || return 0
        sleep 0.5
    done
    return 1
}

# --- path observation -------------------------------------------------------
# sec_path <secret-id> -> direct|relay|unknown, ALWAYS from the consumer row.
sec_path()      { sec_field secretconsumer "$1" current_path unknown; }
sec_fallbacks() { sec_field secretconsumer "$1" direct_fallbacks 0; }
sec_reason()    { sec_field secretconsumer "$1" path_reason ""; }

# sec_time_to_direct <secret-id> [timeout-s] -> milliseconds, or "never"
#   The single number an operator asks for first: after the tunnel is up and
#   the first connection has been proxied, how long until traffic actually
#   leaves the relay? Polled at 250 ms because the punch itself is budgeted at
#   3 s (CHECK_TOTAL_CAP) and a coarser poll would quantise the answer into
#   uselessness.
sec_time_to_direct() {
    local id="$1" limit="${2:-30}" t0 now path
    t0=$(date +%s.%N)
    while :; do
        path=$(sec_path "$id")
        now=$(date +%s.%N)
        if [ "$path" = direct ]; then
            LC_ALL=C awk -v a="$t0" -v b="$now" 'BEGIN{printf "%.0f", (b-a)*1000}'
            return 0
        fi
        if LC_ALL=C awk -v a="$t0" -v b="$now" -v l="$limit" 'BEGIN{exit !((b-a)>l)}'; then
            printf 'never'
            return 1
        fi
        sleep 0.25
    done
}

# --- the provider's byte source ---------------------------------------------
# ONE implementation, here, because four copies in four stages is exactly how
# the public campaign's hard-won lesson failed to reach this directory:
#
#   a REMOTE `pgrep -f 'raw_origin.py 5053'` matches the `bash -c` that is
#   RUNNING it — the pattern sits inside that shell's own command line — so the
#   idempotence guard always reads "already running", the origin is never
#   started, and every arm of the stage measures nothing.
#
# Measured, not reasoned: an S1 smoke returned 0.00 MB/s on all four arms while
# the provider on the VM logged `could not connect to localhost:5053` and the
# consumer's own log showed a perfectly healthy direct QUIC path. The bracket
# in `raw_origin.p[y]` is what breaks the self-match (the literal `[y]` in the
# shell's command line is not what the regex matches), and the readiness probe
# is what makes a failure LOUD instead of a table of zeros — the same harness
# defect class (H-8) the public campaign named.
#
# sec_start_origin <topo> -> 0 when the origin is SERVING, 1 otherwise.
sec_start_origin() {
    local topo="$1" i
    case "$topo" in
        vm-ws|vm-vm)
            # The idiom is the public campaign's, verbatim, and every piece of
            # it is load-bearing (see `pub/ws_pub.sh`, trap H-12):
            #   * the existence check is a real TCP CONNECT, not a `pgrep` —
            #     bracketing the pattern stops it matching its own text but NOT
            #     the plain `raw_origin.py $RP` that the START half of the same
            #     one-liner necessarily contains, so a `pgrep || start` guard
            #     always believes the origin is up and never starts it;
            #   * `sleep 1; true` keeps the ssh session alive past the spawn —
            #     without it the freshly detached process does not survive the
            #     session teardown on this host, measured repeatedly;
            #   * and "a process exists" is the wrong question anyway. What the
            #     stage needs is "something is SERVING on that port".
            vm "mkdir -p \$HOME/out; timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP' 2>/dev/null || \
                (setsid nohup python3 \$HOME/raw_origin.py $RP > \$HOME/out/raworigin.log 2>&1 </dev/null &); \
                sleep 1; true" >/dev/null 2>&1
            for i in $(seq 40); do
                vm "timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP'" >/dev/null 2>&1 && return 0
                sleep 0.25
            done
            echo "raw origin is NOT serving on the VM at 127.0.0.1:$RP — every arm would read 0.00" >&2
            return 1 ;;
        ws-vm)
            pgrep -f "raw_origin.p[y] $RP" >/dev/null 2>&1 || {
                python3 "$HERE_SEC/../../raw_origin.py" "$RP" >"$OUT/raworigin.log" 2>&1 &
                SEC_KIDS+=("$!")
            }
            for i in $(seq 40); do
                python3 "$WS_RAWCLI" ping 127.0.0.1 "$RP" 1 >/dev/null 2>&1 && return 0
                sleep 0.25
            done
            echo "raw origin failed to start on the workstation at 127.0.0.1:$RP" >&2
            return 1 ;;
        *) echo "sec_start_origin: unknown topology '$topo'" >&2; return 2 ;;
    esac
}

# --- measurement primitives -------------------------------------------------
now()   { date +%s.%N; }
mbs()   { LC_ALL=C awk -v b="$1" -v s="$2" -v e="$3" 'BEGIN{d=e-s; if(d<=0){print "0.00";exit} printf "%.2f", b/1048576/d}'; }
ratio() { LC_ALL=C awk -v a="$1" -v b="$2" 'BEGIN{if(b>0) printf "%.3f", a/b; else print "nan"}'; }
