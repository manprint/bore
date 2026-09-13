#!/usr/bin/env bash
# Bring up ONE local `bore vpn` endpoint as root, in the background, and tear it
# down again — nothing else.
#
# WHY THIS EXISTS AS A SEPARATE, TINY SCRIPT
# ------------------------------------------
# A VPN endpoint needs root (it creates a TUN and edits routes), but a benchmark
# does not: the iperf3 client, the ssh calls that drive the far end, the parsing
# and the tables are all ordinary user work. Running the WHOLE harness as root
# would mean every one of those steps runs as root too, and on this workstation
# `sudo` is granted per exact path — so the harness would also have to live at a
# path that is permanently root-executable. This script is the only part that
# genuinely needs the privilege, so it is the only part that gets it. The driver
# (`scripts/perf/staging/vpn/vpn_ab.sh`) runs unprivileged and calls in here.
#
# It refuses to run anything but this repository's own release binary, and only
# with `vpn listen` or `vpn connect` as the first two arguments. That is not
# because an attacker who can already write to this directory would be stopped
# by it — they would not — but because a root entry point that takes a command
# and runs it is a different kind of object from one that takes arguments for a
# fixed command, and this is meant to be the second kind.
#
# sudo runs with `env_reset`, so NOTHING may be passed in the environment: every
# input is a positional argument. That trap has bitten this project before — a
# ladder harness silently measured the defaults because its knobs were env vars
# and sudo dropped them.
#
# Usage (always through the absolute path, which is what sudoers matches):
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh start <tag> <vpn-args...>
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh wait  <tag> [timeout_s]
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh addr  <tag>
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh log   <tag> [lines]
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh stop  <tag>
#   sudo -n /abs/.../scripts/vpn_tun_endpoint.sh blackhole on|off|status [ipv4]
#
# `<tag>` names the run: it is the pid/log file name and the only thing `stop`
# needs. Tags are constrained to [A-Za-z0-9_-] so they cannot escape the run
# directory.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BORE="$ROOT/target/release/bore"
RUNDIR=/run/bore-vpn-bench

die() { echo "vpn_tun_endpoint: $*" >&2; exit 2; }

ACTION="${1:-}"; shift || true
TAG="${1:-}"; shift || true
[ -n "$ACTION" ] || die "no action"
[ -n "$TAG" ] || die "no tag"
case "$TAG" in
    *[!A-Za-z0-9_-]*) die "tag '$TAG' has characters outside [A-Za-z0-9_-]" ;;
esac

mkdir -p "$RUNDIR"
PIDF="$RUNDIR/$TAG.pid"
LOGF="$RUNDIR/$TAG.log"
IFF="$RUNDIR/$TAG.ifname"

case "$ACTION" in
start)
    [ -x "$BORE" ] || die "no release binary at $BORE — cargo build --release --features vpn"
    # Leading `BORE_*=value` tokens become the child's environment.
    #
    # This is not a convenience: `sudo` runs with `env_reset`, so a tunable
    # exported by the caller NEVER reaches this script, let alone the process it
    # starts. Without an explicit channel a ladder over `BORE_DIRECT_DGRAM_SEND_BUF`
    # would run every rung with the compiled-in default and report the resulting
    # flat line as "the knob does nothing" — the exact failure a previous ladder
    # in this repository shipped before the cause was found.
    #
    # Restricted to the `BORE_` prefix so this stays an argument list for a fixed
    # command rather than a way to set arbitrary environment for a root process.
    CHILD_ENV=()
    while [ $# -gt 0 ]; do
        case "${1:-}" in
            BORE_*=*) CHILD_ENV+=("$1"); shift ;;
            *) break ;;
        esac
    done
    # Only this one command, with arguments. See the header.
    [ "${1:-}" = vpn ] || die "first argument must be 'vpn' (got '${1:-}')"
    case "${2:-}" in
        listen|connect) ;;
        *) die "second argument must be 'listen' or 'connect' (got '${2:-}')" ;;
    esac
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
        die "tag '$TAG' is already running as pid $(cat "$PIDF")"
    fi
    : > "$LOGF"; rm -f "$IFF"
    # `debug` is what carries the path switch, the PMTU moves and the direct
    # diagnostics this campaign reads; it is also what makes the log the record
    # of the run rather than a summary of it.
    env RUST_LOG="${RUST_LOG:-bore_cli=debug,bore=debug,info}" "${CHILD_ENV[@]}" \
        setsid "$BORE" "$@" >> "$LOGF" 2>&1 &
    echo $! > "$PIDF"
    # Echo the tunables back so a run's log records what it actually ran with,
    # rather than what the caller believed it had set.
    echo "started $TAG pid=$(cat "$PIDF") log=$LOGF env=${CHILD_ENV[*]:-none}"
    ;;
wait)
    # Wait for the interface to exist AND carry an address: a TUN that is up
    # with no address is not a link a benchmark can use, and reporting "ready"
    # on the device alone would hand the driver a half-built link.
    TMO="${1:-60}"
    END=$(( $(date +%s) + TMO ))
    while [ "$(date +%s)" -lt "$END" ]; do
        if [ -f "$PIDF" ] && ! kill -0 "$(cat "$PIDF")" 2>/dev/null; then
            echo "DIED"; tail -5 "$LOGF" >&2; exit 1
        fi
        # The TUN name is chosen by the client ("auto" picks the first free
        # boreN), so it is read back from THIS endpoint's own log. The patterns
        # are the ones bore actually emits:
        #   "created tun device ... iface=bore0"
        #   "TUN created with GSO/GRO offload ... resolved_name=bore0"
        #
        # There is deliberately NO "first bore* interface on the host" fallback.
        # It was tried and it is wrong whenever more than one endpoint runs here
        # — a hub test with two spokes had both tags resolve to bore0, so both
        # reported the same overlay address, and a spoke-isolation check then
        # "proved" isolation was broken by pinging the local interface. A tag
        # that cannot find its OWN interface must time out, not guess.
        name=$(grep -oE '(iface|resolved_name|tun)=[A-Za-z0-9]+' "$LOGF" 2>/dev/null \
               | grep -oE 'bore[0-9]+|utun[0-9]+' | tail -1)
        if [ -n "$name" ]; then
            addr=$(ip -4 -o addr show dev "$name" 2>/dev/null | awk '{print $4}' | head -1)
            if [ -n "$addr" ]; then
                echo "$name" > "$IFF"
                echo "READY $name $addr"
                exit 0
            fi
        fi
        sleep 0.3
    done
    echo "TIMEOUT"; tail -5 "$LOGF" >&2; exit 1
    ;;
addr)
    name=$(cat "$IFF" 2>/dev/null)
    [ -n "$name" ] || die "no interface recorded for '$TAG' (run 'wait' first)"
    ip -4 -o addr show dev "$name" 2>/dev/null | awk -v n="$name" '{print n, $4}'
    ;;
log)
    tail -n "${1:-40}" "$LOGF" 2>/dev/null
    ;;
stat)
    # /proc/<pid>/stat fields 14/15 (utime/stime) in ticks, plus RSS — the same
    # attribution every other campaign in this repo uses, because `perf` is not
    # available here (perf_event_paranoid=4).
    p=$(cat "$PIDF" 2>/dev/null)
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null || { echo "0 0 0"; exit 0; }
    awk '{print $14, $15}' "/proc/$p/stat" 2>/dev/null | tr '\n' ' '
    awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null
    ;;
res)
    # RSS (KiB), thread count and open file descriptors -- the three quantities
    # that separate a leak from an allocator artefact. RSS and Threads come from
    # the world-readable /proc/<pid>/status; the fd count does NOT, because
    # /proc/<pid>/fd on a root process is mode 0500, which is exactly why this
    # verb exists rather than a `sudo ls` at the call site (sudo here is
    # NOPASSWD per EXACT path, so a bare `sudo ls` would prompt and the stage
    # would silently report zero descriptors forever).
    p=$(cat "$PIDF" 2>/dev/null)
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null || { echo "0 0 0"; exit 0; }
    echo "$(awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null || echo 0)" \
         "$(awk '/Threads/{print $2}' "/proc/$p/status" 2>/dev/null || echo 0)" \
         "$(ls "/proc/$p/fd" 2>/dev/null | wc -l)"
    ;;
fdlist)
    # The descriptor TABLE, not its size. `res` answers "is the count rising";
    # this answers "rising with WHAT", which is the only form of the question a
    # leak can be fixed from. Same privilege reason as `res`: /proc/<pid>/fd on
    # a root process is mode 0500, so this cannot be a `readlink` at the call
    # site. Output is one line per descriptor: the target, with socket inodes
    # resolved through `ss` where it can name them.
    p=$(cat "$PIDF" 2>/dev/null)
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null || { echo "(no process)"; exit 0; }
    for f in /proc/"$p"/fd/*; do
        [ -e "$f" ] || continue
        printf '%s -> %s\n' "${f##*/}" "$(readlink "$f" 2>/dev/null)"
    done
    echo "--- sockets this pid owns ---"
    ss -anp 2>/dev/null | grep "pid=$p," || true
    ;;
stop)
    p=$(cat "$PIDF" 2>/dev/null)
    if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then
        # SIGTERM, not SIGKILL: the whole point of the RAII teardown is that it
        # reverts routes, nft rules and ip_forward. SIGKILL leaves that to the
        # next run's stale reclaim, which is a different code path and not the
        # one a benchmark should be exercising by accident.
        kill -TERM "$p" 2>/dev/null
        for _ in $(seq 60); do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done
        kill -0 "$p" 2>/dev/null && { echo "did not exit on TERM; KILL" >&2; kill -KILL "$p" 2>/dev/null; }
    fi
    rm -f "$PIDF" "$IFF"
    echo "stopped $TAG"
    ;;
blackhole)
    # Drop UDP to and from ONE address, so a live direct path dies while the
    # TCP relay to the server -- a different host -- keeps working. That is the
    # only way to measure, on the real path, how long bore takes to notice a
    # dead direct path and fall back to the warm relay: the product's own
    # promise (DEC-2, seamless in-place fallback) and the thing
    # BORE_DIRECT_QUIC_IDLE_MS / _KEEPALIVE_MS are supposed to move.
    #
    # `$TAG` carries on|off here (it is already constrained to [A-Za-z0-9_-]);
    # the address is the next argument and is validated to be a dotted quad
    # with every octet in range, because this builds a root firewall rule.
    #
    # The rule lives in its OWN table, named once, so teardown is a single
    # `delete table` that cannot touch anything else on this host -- the same
    # discipline bore's own NetConfig uses. `off` is idempotent, so a caller
    # may run it unconditionally in a cleanup trap.
    BH_TABLE=bore_bench_blackhole
    case "$TAG" in
        on)
            ip4="${1:-}"
            case "$ip4" in
                *[!0-9.]*|"") die "blackhole on: '$ip4' is not an IPv4 address" ;;
            esac
            echo "$ip4" | awk -F. 'NF!=4 { exit 1 }
                { for (i=1;i<=4;i++) if ($i=="" || $i+0>255 || $i ~ /[^0-9]/) exit 1 }' \
                || die "blackhole on: '$ip4' is not an IPv4 address"
            nft delete table inet "$BH_TABLE" 2>/dev/null
            nft add table inet "$BH_TABLE" || die "nft add table failed"
            nft add chain inet "$BH_TABLE" out \
                '{ type filter hook output priority -100 ; }' || die "nft add chain out failed"
            nft add chain inet "$BH_TABLE" in \
                '{ type filter hook input priority -100 ; }'  || die "nft add chain in failed"
            nft add rule inet "$BH_TABLE" out ip daddr "$ip4" udp dport 1-65535 drop \
                || die "nft add rule out failed"
            nft add rule inet "$BH_TABLE" in  ip saddr "$ip4" udp sport 1-65535 drop \
                || die "nft add rule in failed"
            echo "blackhole ON udp<->$ip4 (table inet $BH_TABLE)"
            ;;
        off)
            nft delete table inet "$BH_TABLE" 2>/dev/null
            echo "blackhole OFF (table inet $BH_TABLE removed if present)"
            ;;
        status)
            nft list table inet "$BH_TABLE" 2>/dev/null | grep -cE '\bdrop\b' || true
            ;;
        *) die "blackhole: expected on|off|status, got '$TAG'" ;;
    esac
    ;;
*)
    die "unknown action '$ACTION'"
    ;;
esac
