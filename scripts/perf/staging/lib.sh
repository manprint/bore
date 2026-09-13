#!/usr/bin/env bash
# Shared plumbing for every script in scripts/perf/staging.
#
# Sourcing this file is the ONLY thing a harness script needs to do to obtain
# the deployment coordinates, the ssh commands, the admin-API helpers and the
# statistics helpers. Nothing below hardcodes a host, a key or a domain: the
# campaign is re-pointed at a different deployment by editing `env.sh` alone.
#
# Deliberately NOT `set -e`: a benchmark that aborts halfway through leaves the
# deployment holding registered tunnels and produces a partial result file that
# looks like a complete one. Every script checks its own return codes and each
# one traps EXIT to stop whatever it started.
set -uo pipefail

# --- locate and load the environment ---------------------------------------
_perf_env() {
    local c
    for c in "${BORE_PERF_ENV:-}" "$HOME/.config/bore-perf/env.sh" "$PWD/env.sh"; do
        [ -n "$c" ] && [ -f "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}
PERF_ENV="$(_perf_env)" || {
    echo "no env.sh found. Copy scripts/perf/staging/env.sh.example to" >&2
    echo "~/.config/bore-perf/env.sh (chmod 600), fill it in, and re-run." >&2
    exit 2
}
# shellcheck disable=SC1090
. "$PERF_ENV"

: "${BORE_GW:?BORE_GW must be set in env.sh}"
: "${BORE_VM:?BORE_VM must be set in env.sh}"
: "${BORE_SSH_KEY:?BORE_SSH_KEY must be set in env.sh}"
BORE_SRV="${BORE_SRV:-}"
BORE_SRV_USER="${BORE_SRV_USER:-ubuntu}"
BORE_VM_USER="${BORE_VM_USER:-ubuntu}"
BORE_SRV_IFACE="${BORE_SRV_IFACE:-ens5}"
BORE_SRV_CONTAINER="${BORE_SRV_CONTAINER:-bore-server}"
WORK="${BORE_PERF_WORK:-$HOME/.cache/bore-perf}"
OUT="${BORE_PERF_OUT:-$PWD/out}"
mkdir -p "$WORK" "$OUT"

# `-o LogLevel=ERROR` is not cosmetic: without it ssh writes
# "Warning: Permanently added '<address>' (ED25519) to the list of known hosts."
# to STDERR, and a stage that captures stderr puts a COORDINATE into its own
# result file. MEASURED: `secret_scan.sh --out out/eth` reported 48 hits and the
# large majority were that one line, in `xfer_bw`, `vpn_hub`, `ws_ref` and
# `sec_ack`. The address never came from the harness's prose or its code -- it
# came from a tool being helpful on a channel nobody had thought about.
# Errors still print; only the advisory is silenced.
SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=no -o LogLevel=ERROR -o ConnectTimeout=10 -i "$BORE_SSH_KEY")
# shellcheck disable=SC2086
vm()  { ssh "${SSH_OPTS[@]}" "$BORE_VM_USER@$BORE_VM" "$@"; }
srv() { [ -n "$BORE_SRV" ] || return 1; ssh "${SSH_OPTS[@]}" "$BORE_SRV_USER@$BORE_SRV" "$@"; }
vmcp(){ scp -q "${SSH_OPTS[@]}" "$@"; }

# --- admin API --------------------------------------------------------------
adm()     { curl -fsS -m 10 -H "Authorization: Bearer $ADMIN_TOKEN" "$ADMIN_URL/$1"; }
present() { adm vhost | jq -e --arg l "$1" 'any(.[]; .subdomain==$l)' >/dev/null 2>&1; }
fld()     { adm vhost | jq -r --arg l "$1" --arg f "$2" '.[]|select(.subdomain==$l)|.[$f]'; }

# --- instance network allowance --------------------------------------------
# The single most important confounder on a burstable instance: one 4-stream
# 10 s download is roughly a whole inbound burst budget, so every measurement
# reports the delta across itself and a shaped burst is discarded rather than
# averaged in. Returns 0 when no server host is configured.
ena() {
    srv "sudo -n ethtool -S $BORE_SRV_IFACE | grep bw_in_allowance_exceeded | tr -dc '0-9'" 2>/dev/null || echo 0
}
ena_out() {
    srv "sudo -n ethtool -S $BORE_SRV_IFACE | grep bw_out_allowance_exceeded | tr -dc '0-9'" 2>/dev/null || echo 0
}

# --- statistics -------------------------------------------------------------
# A MEDIAN MUST REFUSE WHAT IT CANNOT MEASURE, AND SAY SO.
#
# Two separate traps meet in this one helper.
#
# (1) `sort -n` is locale-dependent (V-11): under a comma-decimal locale
#     {397.46, 264.01, 408} sorts to {408, 264.01, 397.46}. `LC_ALL=C` fixed
#     that, but the sort is gone now anyway -- awk compares the values as
#     NUMBERS, which no locale can reinterpret.
#
# (2) A sample that is not a number is not a slow measurement, it is the
#     ABSENCE of one, and the harness has now been bitten by that three times:
#     `cf()` published `0` for a Cloudflare download that fetched nothing,
#     `vpn_hub` exited 0 having measured nothing, and `vpn_relay_attrib`
#     printed `0.00` for endpoints no packet could reach. Every one of those
#     was a zero that meant "the instrument failed". Individual stages grew
#     their own `add()`/`keep()` guards one at a time; this is the chokepoint
#     every one of them ends at, so the guard belongs here as well.
#
# A refused sample goes to STDERR, not stdout: the value is consumed inline by
# `printf`/`awk` at the call sites, so a note on stdout would corrupt the very
# table it is warning about -- while stderr is captured into the stage's `.out`
# by the driver and so is read by whoever reads the result.
#
# Note what is NOT refused: a genuine measured `0`. A blackholed arm really
# does deliver 0 Mbit/s, and that is a result. Distinguishing the two is the
# job of the producer (`tcp_mbps` now prints FAILED when iperf3 produced no
# usable JSON at all), not of this function.
med()  {
    LC_ALL=C awk '
        /^[0-9]+(\.[0-9]+)?$/ { v[++n] = $1 + 0; next }
        NF { bad++ }
        END {
            if (bad) printf "  med(): refused %d non-numeric sample(s)\n", bad > "/dev/stderr"
            if (n == 0) { print "n/a"; exit }
            for (i = 1; i <= n; i++)
                for (j = i + 1; j <= n; j++)
                    if (v[j] < v[i]) { t = v[i]; v[i] = v[j]; v[j] = t }
            print (n % 2) ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
        }'
}
rate() { LC_ALL=C awk -v by="$1" -v s="$2" -v e="$3" 'BEGIN{printf "%7.2f MB/s (%4.0f Mbit/s)", by/1048576/(e-s), by*8/1000000/(e-s)}'; }
mbps() { LC_ALL=C awk -v m="$1" 'BEGIN{printf "%.0f", m*8.388608}'; }

# --- fixtures ---------------------------------------------------------------
# A 1 GiB zero file for uploads. Zeroes are fine: nothing on the path
# compresses, and /dev/urandom at this size is slower than the link.
bigup() {
    local f="$WORK/up1g.bin"
    [ -f "$f" ] || head -c 1073741824 /dev/zero > "$f"
    printf '%s' "$f"
}

# --- cooldown ---------------------------------------------------------------
# One 4-stream 10 s burst is roughly a whole inbound allowance budget on this
# instance class, so back-to-back arms measure the token bucket instead of the
# tunnel. Every harness waits between bursts; the default is deliberately long.
COOL="${COOL:-75}"
cool() { sleep "${1:-$COOL}"; }

# --- misc -------------------------------------------------------------------
label() { printf '%s%s' "${1:-p}" "$(date +%s%N | cut -c8-13)"; }
say()   { echo "### $* — $(date -Is)"; }

# --- short aliases used throughout the harness ------------------------------
# The scripts were written against a specific deployment and read better with
# short names; these are the ONLY place the deployment leaks into them.
V="$BORE_VM"; S="$BORE_SRV"; SRV="$BORE_SRV"; GW="$BORE_GW"
VM_HOME="/home/$BORE_VM_USER"
SRV_HOME="/home/$BORE_SRV_USER"
SSH="ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 -i $BORE_SSH_KEY"
VMU="$BORE_VM_USER"; SRVU="$BORE_SRV_USER"
IFACE="$BORE_SRV_IFACE"
# Port the dufs real-world origin listens on, on the test VM's loopback.
DUFS_PORT="${BORE_DUFS_PORT:-5080}"
# Port the plain byte-source origin listens on, on the test VM's loopback.
ORIGIN_PORT="${BORE_ORIGIN_PORT:-5052}"
# WiFi interface of the consumer workstation, for the radio-link reference.
WIFI_IFACE="${BORE_WIFI_IFACE:-}"
