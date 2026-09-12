#!/usr/bin/env bash
# Shared plumbing for the FILE-TRANSFER staging campaign.
#
# `bore transfer` is not a fifth transport: it is a secret tunnel with a file
# protocol on top. `transfer listener` registers as the secret PROVIDER and
# RECEIVES; `transfer sender` is the secret CONSUMER (`secret::Proxy`) and
# SENDS. Three consequences, and they are why this directory exists beside
# `sec/` instead of inside it:
#
#  1. Every result of the secret campaign applies unchanged — the direct arm
#     runs sender <-> listener with the server off the path (S-1), the window
#     floor is decided by `--udp-memory-budget / --max-carriers`, and the
#     dialer/listener check-round asymmetry (S-5) governs how long the direct
#     path takes to come up.
#
#  2. The DIRECTION is not symmetric and is not the topology name. `ws-vm`
#     here means the SENDER runs on the workstation and the LISTENER on the
#     VM, i.e. bytes flow ws -> vm. Reversing it measures the other half of a
#     home uplink, which on a consumer line is nothing like the downlink.
#
#  3. `--parallel N` is the only bandwidth knob that matters, and it means a
#     different thing per arm: N QUIC bidi streams on one connection when the
#     path is direct, N yamux substreams over the relay carrier pool when it
#     is not. A single stream is bounded by `window / RTT` on the direct arm,
#     so a `--parallel 1` number is a window measurement, not a link one.
#
# NEVER `pkill bore`: this deployment carries the operator's own live tunnels
# (project rule). Local processes are killed by the PID they were started
# with; remote ones by their per-run `--transfer-id`, which is minted here and
# exists nowhere else on the box.
set -uo pipefail
HERE_X="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE_X/../lib.sh"

WS_BORE="${WS_BORE:-$HERE_X/../../../../target/release/bore}"
VM_BORE="${VM_BORE:-$VM_HOME/bore}"
# Where fixtures and destinations live on each host. Kept off $HOME/out so a
# multi-GiB fixture is never mistaken for a result file.
WS_WORK="${WS_WORK:-$WORK/xfer}"
VM_WORK="${VM_WORK:-$VM_HOME/xferwork}"

XFER_KIDS=()
xfer_cleanup() { local p; for p in "${XFER_KIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done; }
trap 'xfer_cleanup; exit 130' INT TERM
trap xfer_cleanup EXIT

xfer_id() { printf 'xf%s' "$(date +%s%N | cut -c9-14)"; }

# --- fixtures ---------------------------------------------------------------
# Both hosts need the same bytes so a direction flip is the only variable.
# Zeros would be wrong here in a way they are not for the vhost campaign:
# nothing on bore's path compresses, but the FILESYSTEM underneath can (and a
# sparse file makes the sender's read free), so fixtures are incompressible.
#
# ws_fixture <MiB> <files> -> prints the directory
ws_fixture() {
    # Split: `local` expands every word BEFORE it assigns any of them, so a
    # one-liner referring to $mb in the same statement reads it unbound.
    local mb="$1" n="$2"
    local d="$WS_WORK/fx-${mb}m-${n}f"
    [ -d "$d" ] && [ "$(find "$d" -type f | wc -l)" = "$n" ] && { printf '%s' "$d"; return 0; }
    rm -rf "$d"; mkdir -p "$d"
    python3 "$HERE_X/mkfixture.py" "$d" "$n" "$mb" >&2 || return 1
    printf '%s' "$d"
}
vm_fixture() {
    local mb="$1" n="$2"
    local d="$VM_WORK/fx-${mb}m-${n}f"
    vmcp "$HERE_X/mkfixture.py" "$VMU@$V:/tmp/mkfixture.py" >/dev/null 2>&1
    vm "mkdir -p $VM_WORK
        if [ -d '$d' ] && [ \"\$(find '$d' -type f | wc -l)\" = '$n' ]; then exit 0; fi
        rm -rf '$d'; mkdir -p '$d'; python3 /tmp/mkfixture.py '$d' $n $mb" >&2 || return 1
    printf '%s' "$d"
}

# --- role placement ---------------------------------------------------------
# xfer_listener_ws <id> <dest> [flags...]
xfer_listener_ws() {
    local id="$1" dest="$2"; shift 2
    rm -rf "$dest"; mkdir -p "$dest"
    RUST_LOG=${XFER_LOG:-warn,bore_cli::secret=info} "$WS_BORE" transfer listener --to "$BORE_TO" --secret "$BORE_SECRET" \
        --transfer-id "$id" --dest-path "$dest" "$@" >"$OUT/lis-$id.log" 2>&1 &
    LASTPID=$!; XFER_KIDS+=("$LASTPID")
}
xfer_listener_vm() {
    local id="$1" dest="$2"; shift 2
    vm "rm -rf '$dest'; mkdir -p '$dest' ~/out
        setsid nohup env RUST_LOG=${XFER_LOG:-warn,bore_cli::secret=info} $VM_BORE transfer listener --to '$BORE_TO' \
            --secret '$BORE_SECRET' --transfer-id $id --dest-path '$dest' $* \
            > ~/out/lis-$id.log 2>&1 </dev/null & true" >/dev/null 2>&1
}
# xfer_sender_ws <id> <src> [flags...] -> sets XFER_RC / XFER_WALL
xfer_sender_ws() {
    local id="$1" src="$2"; shift 2
    local t0 t1
    t0=$(date +%s.%N)
    RUST_LOG=${XFER_LOG:-warn,bore_cli::secret=info} "$WS_BORE" transfer sender --to "$BORE_TO" --secret "$BORE_SECRET" \
        --transfer-id "$id" --sources "$src" "$@" >"$OUT/snd-$id.log" 2>&1
    XFER_RC=$?
    t1=$(date +%s.%N)
    XFER_WALL=$(LC_ALL=C awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
}
xfer_sender_vm() {
    local id="$1" src="$2"; shift 2
    local t0 t1
    t0=$(date +%s.%N)
    vm "env RUST_LOG=${XFER_LOG:-warn,bore_cli::secret=info} $VM_BORE transfer sender --to '$BORE_TO' --secret '$BORE_SECRET' \
        --transfer-id $id --sources '$src' $* > ~/out/snd-$id.log 2>&1"
    XFER_RC=$?
    t1=$(date +%s.%N)
    XFER_WALL=$(LC_ALL=C awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
}

# --- teardown ---------------------------------------------------------------
# The transfer id is minted per run, so `-f -- "--transfer-id <id>"` is a
# narrowly matched pattern and never touches the operator's tunnels.
xfer_down() {
    local id="$1"; shift
    local p
    for p in "$@"; do kill -9 "$p" 2>/dev/null; wait "$p" 2>/dev/null; done
    vm "pkill -9 -f -- '--transfer-id $id' 2>/dev/null; true" >/dev/null 2>&1
}

# --- reading the result -----------------------------------------------------
# The path a transfer actually used is printed by the SENDER (it is the secret
# consumer, S-1). `direct` vs `relay` is read from its log, never assumed from
# the flags: a `--udp` run that failed to punch reports relay and is a
# different measurement.
xfer_path() { # <id> <ws|vm>
    local id="$1" where="$2" log
    if [ "$where" = ws ]; then log=$(cat "$OUT/snd-$id.log" 2>/dev/null)
    else log=$(vm "cat ~/out/snd-$id.log 2>/dev/null"); fi
    case "$log" in
        *"path=direct"*) echo direct ;;
        *"path=relay"*)  echo relay ;;
        *) echo unknown ;;
    esac
}
# What bore itself says it moved, and in how long.
#
# `wall_s` is end-to-end and INCLUDES the rendezvous — on the direct arm that is
# the hole punch, which the secret campaign measured at 37..53 ms when the check
# round ends cleanly and ~1.1 s when it does not (S-5). Quoting only the wall
# would charge the transport for the handshake; quoting only bore's own figure
# would hide a slow path from the operator. Report both.
xfer_reported() { # <id> <ws|vm> -> "<secs> <MiB/s>" from the sender's completion line
    local id="$1" where="$2" line
    if [ "$where" = ws ]; then line=$(grep -h "complete:" "$OUT/snd-$id.log" 2>/dev/null | tail -1)
    else line=$(vm "grep -h 'complete:' ~/out/snd-$id.log 2>/dev/null | tail -1"); fi
    LC_ALL=C awk -v l="$line" 'BEGIN{
        s="-"; r="-";
        if (match(l, /in [0-9.]+s/))      s=substr(l, RSTART+3, RLENGTH-4);
        if (match(l, /[0-9.]+MiB\/s avg/)) r=substr(l, RSTART, RLENGTH-9);
        print s, r
    }'
}

xfer_mbs() { # <MiB> <wall_s>
    LC_ALL=C awk -v m="$1" -v s="$2" 'BEGIN{if(s<=0){print "n/a";exit} printf "%.2f", m/s}'
}
