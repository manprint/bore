#!/usr/bin/env bash
# Shared plumbing for the REAL-PATH VPN campaign.
#
# WHY THIS DIRECTORY EXISTS AT ALL
# --------------------------------
# `scripts/vpn_bench.sh` already measures the VPN thoroughly — over netns with
# `netem` shaping. Its own assessment states the limit this campaign exists to
# remove, and it is quoted here because it is the whole mandate:
#
#     "netns + netem != a real Internet path (single FIFO, synthetic loss).
#      Numbers are directional, not absolute. Real-path validation still
#      required."   -- docs/vpn/VPN_BANDWIDTH_ASSESSMENT.md
#
# So every script here runs the SAME measurements over a real WAN: a home NAT
# on one end, an AWS instance in the same region on the other, a real router
# doing real NAT, real queueing, real loss. The measurement vocabulary is kept
# deliberately identical to `vpn_bench.sh` (iperf3 TCP single/parallel, UDP with
# loss, fixed volume, ping RTT) so a real-path number and a netns number are the
# same quantity measured on two different paths, and the gap between them is
# itself a result.
#
# WHAT A VPN CAMPAIGN HAS THAT THE OTHERS DID NOT
# -----------------------------------------------
#  1. BOTH ends need root and a TUN. The secret/public/vhost campaigns drove
#     unprivileged clients; here the workstation half runs through
#     `scripts/vpn_tun_endpoint.sh` (the one root entry point) and the VM half
#     through `sudo -n` over ssh.
#  2. The path is NOT chosen at registration. A VPN link ALWAYS starts on the
#     relay and upgrades to direct on a background 30 s grid, so "the direct
#     arm" is not a flag — it is a state that must be WAITED for and then
#     VERIFIED. `wait_path direct` exists for exactly that, and every arm that
#     could not reach its path is reported as a failure, never averaged in.
#     This is the secret campaign's hardest-won rule: a direct number that was
#     silently a relay number is the one mistake the whole campaign avoids.
#  3. The interesting traffic is INNER TCP. The tunnel carries IP packets, so
#     what a user experiences is a TCP flow inside a tunnel, subject to the
#     Mathis bound on whatever loss the outer path shows. That is why the flow
#     count is a first-class axis here and was not in the other campaigns.
#
# LEAVING THE WORKSTATION CLEAN
# -----------------------------
# A VPN endpoint edits the HOST: it creates an interface, adds routes, may set
# ip_forward and may install nft/iptables rules. A benchmark that leaves any of
# that behind has changed the machine it is measuring. Every script traps EXIT
# through `vpn_cleanup`, which stops both endpoints by PID/id (NEVER a blanket
# `pkill bore` -- project rule: this host and the server carry the operator's
# own live tunnels) and then `vpn_assert_clean` re-reads the host and reports
# any interface, route or ip_forward change that survived. The teardown is
# SIGTERM so the RAII revert actually runs; SIGKILL would leave the revert to
# the next run's stale-reclaim path, which is a different code path and not one
# a benchmark should exercise by accident.
set -uo pipefail
# Numeric sorting MUST NOT depend on the operator's locale. Under it_IT (and
# every other comma-decimal locale) `sort -n` reads "397.46" and "264.01" with
# no decimal separator it recognises and orders them wrongly, so a median picks
# the wrong sample and reports it with full confidence. The shared lib.sh `med`
# already pins LC_ALL=C per call; this pins it for the whole VPN stage set so a
# helper defined locally in a stage cannot reintroduce the bug. Found 2026-09-12
# when a median of {397.46, 264.01, 408} reported 264.01.
export LC_ALL=C

HERE_VPN="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE_VPN/../lib.sh"

# --- coordinates ------------------------------------------------------------
ROOTSH="$HERE_VPN/../../../vpn_tun_endpoint.sh"
[ -x "$ROOTSH" ] || { echo "missing root helper $ROOTSH" >&2; exit 2; }
WS_BORE="${WS_BORE:-$HERE_VPN/../../../../target/release/bore}"
VM_BORE="${VM_BORE:-\$HOME/bore-vpn}"

# Overlay addressing is STATIC (`--vpn-addr`/`--vpn-peer-addr`) rather than the
# server pool, because a benchmark that has to discover its own peer address
# from a log line has one more way to fail for a reason that is not the thing
# being measured. 10.77/16 is chosen to collide with nothing on either host.
A_ADDR="${A_ADDR:-10.77.0.1/30}"   # listener side
A_PEER="${A_PEER:-10.77.0.2}"
B_ADDR="${B_ADDR:-10.77.0.2/30}"   # connector side
B_PEER="${B_PEER:-10.77.0.1}"

# MTU policy: measure SHIPPED behaviour, and never pin below what the path
# actually reaches.
#
# This was got wrong once and the error is recorded here so it is not repeated.
# The first pass pinned 1280, reasoning from a single early log line that read
# `max_datagram=Some(1288)`. That line is the value FIFTEEN SECONDS INTO THE
# LINK: quinn starts MTU discovery low and probes upward, and on this path it
# climbs to a QUIC MTU of 1452, after which the VPN's PMTU monitor raises the
# TUN from 1288 to 1414 and it stays there. Pinning 1280 therefore handed the
# tunnel an MSS of 1240 against the 1460 the bare path was using -- a 10.8 %
# handicap applied to the tunnel arm only, and silently, since a pinned MTU
# produces no churn to notice. Throughput on a single TCP flow is proportional
# to MSS (Mathis), so a large part of the "tunnel is 30 % below bare" result
# that pin produced was the pin itself.
#
# So: DO NOT pin by default. Let the link auto-tune exactly as it does in
# production, wait for it to settle, and record the MTU each arm actually ran
# at so the number is attributable. `BENCH_PIN=1` restores pinning for the one
# stage that deliberately holds the MTU still.
BENCH_MTU="${BENCH_MTU:-1350}"
BENCH_PIN="${BENCH_PIN:-0}"
PIN_FLAG=""
[ "$BENCH_PIN" = 1 ] && PIN_FLAG="--pin-mtu"

IPERF_PORT="${IPERF_PORT:-5299}"
VPN_RUN_ID="${VPN_RUN_ID:-vb$$}"

# `NAME=value` pairs applied to the bore process on BOTH ends. Word-split on
# purpose -- these are argv words, never a quoted string.
#
# Both ends, always: a datagram send buffer is a property of the SENDER, so a
# tunable set on one host alone would be measured in one direction and silently
# absent in the other, and the run would report the average of two different
# configurations as one. Setting both is what makes a rung of the ladder a
# single configuration.
VPN_ENV="${VPN_ENV:-}"

# --- teardown ---------------------------------------------------------------
# Tags started on this workstation, and link ids started on the VM. Both are
# minted per run so nothing here can match another process on either box.
VPN_WS_TAGS=()
VPN_VM_IDS=()

vpn_cleanup() {
    local t id
    for t in "${VPN_WS_TAGS[@]:-}"; do
        [ -n "$t" ] && sudo -n "$ROOTSH" stop "$t" >/dev/null 2>&1
    done
    for id in "${VPN_VM_IDS[@]:-}"; do
        # Matched by the per-run link id, which no other process can carry.
        [ -n "$id" ] && vm "sudo -n pkill -TERM -f 'vpn (listen|connect) --id $id' 2>/dev/null; true" >/dev/null 2>&1
    done
    # Killed by RECORDED PID, never by a pattern. A pattern is not merely
    # against the project rule here, it is broken: `ssh host "pkill -f 'iperf3
    # -s -p 5299'; ... iperf3 -s -p 5299 ..."` puts that exact string in the
    # remote shell's OWN argv, so `pkill -f` matches the launcher and kills the
    # session that was about to start the server. The symptom is an iperf3 that
    # is never listening and a benchmark that reads "connection refused" as a
    # tunnel fault.
    vm 'for f in ~/.iperf3-'"$IPERF_PORT"'.pid; do [ -f "$f" ] && kill -TERM "$(cat "$f")" 2>/dev/null; rm -f "$f"; done; true' >/dev/null 2>&1
    # Unconditional, and idempotent on purpose: a stage that dies between
    # `blackhole on` and its own `off` would otherwise leave this workstation
    # dropping UDP to the far end -- a lasting change to the machine the
    # campaign is measuring, and the exact failure "leave the host clean" is
    # about. Cheap enough to run even for the stages that never blackhole.
    sudo -n "$ROOTSH" blackhole off >/dev/null 2>&1
    [ -f "$WORK/iperf3-$IPERF_PORT.pid" ] && { kill -TERM "$(cat "$WORK/iperf3-$IPERF_PORT.pid")" 2>/dev/null; rm -f "$WORK/iperf3-$IPERF_PORT.pid"; }
}

# Baseline of the host state a VPN endpoint is able to change, captured before
# anything starts so the check at the end compares against THIS machine rather
# than against an assumption about it.
VPN_BASE_FWD="$(cat /proc/sys/net/ipv4/ip_forward 2>/dev/null)"
VPN_BASE_IFS="$(ip -o link show 2>/dev/null | grep -coE 'bore[0-9]+')"

vpn_assert_clean() {
    sleep 1
    local ifs fwd rt bad=0
    ifs="$(ip -o link show 2>/dev/null | grep -coE 'bore[0-9]+')"
    fwd="$(cat /proc/sys/net/ipv4/ip_forward 2>/dev/null)"
    rt="$(ip route show 2>/dev/null | grep -c '10\.77\.')"
    [ "$ifs" = "$VPN_BASE_IFS" ] || { echo "DIRTY: bore interfaces $VPN_BASE_IFS -> $ifs" >&2; bad=1; }
    [ "$fwd" = "$VPN_BASE_FWD" ] || { echo "DIRTY: ip_forward $VPN_BASE_FWD -> $fwd" >&2; bad=1; }
    [ "$rt" = 0 ] || { echo "DIRTY: $rt leftover 10.77 route(s)" >&2; bad=1; }
    # A leftover blackhole is invisible to `ip`/`ip route` and would silently
    # ruin every later measurement by dropping the direct path's UDP, so it is
    # checked here rather than trusted to the cleanup that may not have run.
    local bh; bh="$(sudo -n "$ROOTSH" blackhole status 2>/dev/null | tr -dc '0-9')"
    [ "${bh:-0}" = 0 ] || { echo "DIRTY: $bh blackhole rule(s) still installed" >&2; bad=1; }
    if [ "$bad" = 0 ]; then
        echo "host clean: bore ifaces=$ifs ip_forward=$fwd overlay routes=0"
    else
        echo "HOST NOT CLEAN -- inspect before the next run" >&2
    fi
}

# INSTALLED AFTER BOTH FUNCTIONS EXIST, not before.
# The trap body is evaluated when the trap FIRES, so installing it earlier is
# usually harmless -- but "usually" is doing work there: a signal arriving
# between the install and the definition would run a handler in which
# `vpn_assert_clean` is not a command yet. The cleanup would still happen (it is
# defined above), the CHECK would silently not. Nothing is allocated between the
# two points, so installing here costs nothing and removes the window.
trap 'vpn_cleanup; vpn_assert_clean; exit 130' INT TERM
trap 'vpn_cleanup; vpn_assert_clean' EXIT

# --- endpoint control -------------------------------------------------------
# Every stage mints a FRESH id per link (`VPN_LINK_ID="${VPN_RUN_ID}..."`) so
# two links of the same stage never collide on the server. The library only
# reads it -- and until now it read it BARE, so a stage that forgot died with
# `vpnlib.sh: line 188: VPN_LINK_ID: unbound variable` from inside a helper,
# which names the library rather than the mistake. Declared here, checked at
# the point of use, and the check says what to do.
VPN_LINK_ID="${VPN_LINK_ID:-}"

vpn_need_link_id() { # <caller>
    [ -n "$VPN_LINK_ID" ] && return 0
    echo "  $1: VPN_LINK_ID is empty -- the STAGE must mint a fresh id per link," >&2
    echo "      e.g. VPN_LINK_ID=\"\${VPN_RUN_ID}x\$(date +%s%N | tail -c 5)\"" >&2
    return 1
}

# ws_up <tag> <listen|connect> <extra flags...>
ws_up() {
    local tag="$1" verb="$2"; shift 2
    vpn_need_link_id ws_up || return 1
    VPN_WS_TAGS+=("$tag")
    local addr peer
    if [ "$verb" = listen ]; then addr="$A_ADDR"; peer="$A_PEER"; else addr="$B_ADDR"; peer="$B_PEER"; fi
    # shellcheck disable=SC2086
    sudo -n "$ROOTSH" start "$tag" $VPN_ENV vpn "$verb" \
        --id "$VPN_LINK_ID" --to "$BORE_TO" --secret "$BORE_SECRET" \
        --vpn-addr "$addr" --vpn-peer-addr "$peer" \
        --mtu "$BENCH_MTU" $PIN_FLAG "$@" >/dev/null
}

# vm_up <listen|connect> <extra flags...>
vm_up() {
    local verb="$1"; shift
    vpn_need_link_id vm_up || return 1
    VPN_VM_IDS+=("$VPN_LINK_ID")
    local addr peer
    if [ "$verb" = listen ]; then addr="$A_ADDR"; peer="$A_PEER"; else addr="$B_ADDR"; peer="$B_PEER"; fi
    vm "mkdir -p ~/out; setsid nohup sudo -n env RUST_LOG=bore_cli=debug,bore=debug,info $VPN_ENV \
        $VM_BORE vpn $verb --id '$VPN_LINK_ID' --to '$BORE_TO' --secret '$BORE_SECRET' \
        --vpn-addr $addr --vpn-peer-addr $peer --mtu $BENCH_MTU $PIN_FLAG $* \
        > ~/out/$VPN_LINK_ID.log 2>&1 < /dev/null & disown" >/dev/null 2>&1
}

ws_log() { sudo -n "$ROOTSH" log "$1" "${2:-400}"; }
vm_log() { vm "tail -n ${1:-400} ~/out/$VPN_LINK_ID.log" 2>/dev/null; }

# ws_quic_since <tag> <iso8601-start> -- what the DIRECT carrier saw since a
# moment, read out of the connector's own 5 s stats lines.
#
# WHY EVERY THROUGHPUT STAGE SHOULD PRINT THIS
# --------------------------------------------
# A throughput number alone cannot say whose fault a low one is. Loss can:
# the direct path carries inner IP packets as QUIC DATAGRAMS, which are
# unreliable by design (adding retransmission under a tunnelled TCP flow is the
# TCP-over-TCP meltdown), so a dropped datagram IS a dropped inner segment and
# the inner flow pays Mathis -- throughput ~ MSS / (RTT * sqrt(p)). At 19 ms and
# a 1350 B MSS, **0.01 % loss caps a single inner TCP flow at ~70 Mbit/s**,
# which is a sixfold collapse produced entirely outside this process.
#
# MEASURED, and it is why this helper exists: `vpn_stability` cycle 3 on
# 2026-09-12 read 95-160 Mbit/s where cycles 1 and 2 read 712, and the diagnosis
# was read off these lines -- cycles 1-2 had `lost_pkts_d=0` in EVERY sample at
# 762 Mbit/s carrier rate, cycle 3 had 2-8 lost per 5 s at a fifth of the rate,
# with our own `buffer_drop_est` and `tx_drops_total` both at 0 throughout. A
# leak in this process cannot produce path loss, so the fd/RSS growth the stage
# was built to find was NOT the cause of the collapse it happened to coincide
# with. Without this column the correlation reads as causation.
#
# ISO-8601 timestamps compare correctly as strings, which is the whole reason
# the log prints them that way.
ws_quic_since() {
    local tag="$1" since="$2"
    ws_log "$tag" 20000 | awk -v since="$since" '
        $0 ~ /direct carrier quic stats/ && $1 >= since {
            for (i = 1; i <= NF; i++) {
                split($i, kv, "=")
                if (kv[1] == "lost_pkts_d")  lost += kv[2]
                if (kv[1] == "sent_pkts_d")  sent += kv[2]
                if (kv[1] == "cong_events_d") cong += kv[2]
                if (kv[1] == "rtt_ms") { rtt[n++] = kv[2]; if (mn == "" || kv[2] < mn) mn = kv[2] }
            }
        }
        END {
            if (n == 0) { print "lost=n/a sent=n/a cong=n/a rtt_min=n/a rtt_max=n/a samples=0"; exit }
            mx = 0; for (i = 0; i < n; i++) if (rtt[i] > mx) mx = rtt[i]
            printf "lost=%d sent=%d cong=%d rtt_min=%s rtt_max=%s samples=%d\n",
                   lost, sent, cong, mn, mx, n
        }'
}

# ws_nic_drops -- this end's own egress counters, as a single line. A local
# drop (qdisc or driver) and a path drop are opposite conclusions from the same
# throughput number, and only this separates them.
ws_nic_drops() {
    local nic
    nic="$(ip route show default | awk '/^default/{print $5; exit}')"
    [ -n "$nic" ] || { echo "tx_dropped=n/a tx_errors=n/a"; return; }
    printf 'tx_dropped=%s tx_errors=%s\n' \
        "$(cat /sys/class/net/"$nic"/statistics/tx_dropped 2>/dev/null)" \
        "$(cat /sys/class/net/"$nic"/statistics/tx_errors 2>/dev/null)"
}

# ws_ready <tag> [timeout] -- interface exists AND carries an address
ws_ready() { sudo -n "$ROOTSH" wait "$1" "${2:-60}"; }

# ws_path <tag> -- the LAST path this endpoint reported. Read from the log
# because the VPN publishes its path as an event, not as a pollable field: the
# bridge announces each switch, so the last announcement is the current state.
# THE PATTERNS MUST BE THE STRINGS THE PRODUCT ACTUALLY WRITES.
#
# This grepped for `falling back to relay`. The product writes
# `direct path lost; fell back to relay (link preserved)` (src/vpn.rs, both
# fallback arms) -- "fell", not "falling" -- so the fallback line NEVER matched
# and the last match stayed the earlier "switched to direct". After any
# fallback the path therefore read `direct` FOREVER.
#
# That is the premise of this campaign's cardinal sin, "a direct number that was
# silently a relay number". MEASURED cost 2026-09-13: `vpn_quic_timers` reported
# `back to direct` at ~30 ms against a `DIRECT_RETRY_INTERVAL` of 30 s -- three
# orders of magnitude, and the stage's own note said that column is quantised by
# the grid. Its `dead` column, measured WITH PACKETS rather than from a log
# line, was unaffected: that design choice is what saved the stage's main
# result.
#
# Reading a path from a log is a P-12 violation to begin with (read the state
# from the kernel or the server, not from what the program said about it). It
# survives here because a VPN link has no admin row to ask, unlike the vhost /
# secret / jump registries -- so the mitigation is to pin the strings to the
# source and to keep any liveness verdict on PACKETS.
ws_path() {
    local last
    last="$(ws_log "$1" 4000 | grep -oE 'bridge switched to (direct|relay) path|vpn path upgraded to direct|direct path lost; fell back to relay|falling back to relay' | tail -1)"
    case "$last" in
        *"to direct"*) echo direct ;;
        *relay*)       echo relay ;;
        *)             echo relay ;;   # a link that never announced is still on its start path
    esac
}

# ws_mtu <tag> -- the TUN's CURRENT MTU, read from the kernel rather than the
# log: the log says what bore decided, the kernel says what the packets get.
ws_mtu() {
    local n; n="$(sudo -n "$ROOTSH" addr "$1" 2>/dev/null | awk '{print $1}')"
    [ -n "$n" ] && ip -o link show dev "$n" 2>/dev/null | grep -oE 'mtu [0-9]+' | awk '{print $2}'
}

# wait_mtu_settle <tag> [quiet_s] [timeout_s]
# Returns once the TUN MTU has held still for `quiet_s`. Measuring before this
# means measuring across an MTU change, which moves MSS mid-transfer and makes
# the result a blend of two configurations.
#
# The quiet window MUST exceed the gap between successive PMTU steps, not merely
# be "a few seconds". Measured on this path: the TUN goes 1350 -> 1288 at about
# t+5 s and 1288 -> 1414 at about t+25 s, so a plateau of ~20 s separates two
# real changes. An 8 s window was tried first and returned 1288 every time --
# settling on the intermediate value, reporting it as final, and measuring the
# tunnel at an MSS 126 bytes short of the one it actually runs at.
wait_mtu_settle() {
    local tag="$1" quiet="${2:-24}" tmo="${3:-90}" end last cur stable
    end=$(( $(date +%s) + tmo ))
    last=""; stable=0
    while [ "$(date +%s)" -lt "$end" ]; do
        cur="$(ws_mtu "$tag")"
        if [ -n "$cur" ] && [ "$cur" = "$last" ]; then
            stable=$(( stable + 2 ))
            [ "$stable" -ge "$quiet" ] && { echo "$cur"; return 0; }
        else
            stable=0; last="$cur"
        fi
        sleep 2
    done
    echo "${last:-unknown}"
    return 1
}

# wait_path <tag> <direct|relay> [timeout_s]
# The direct upgrade runs on a fixed 30 s grid, so the timeout must exceed one
# full grid interval plus an attempt, or a slow first round reads as a failure
# to upgrade when it was only a failure to wait.
wait_path() {
    local tag="$1" want="$2" tmo="${3:-75}" end
    end=$(( $(date +%s) + tmo ))
    while [ "$(date +%s)" -lt "$end" ]; do
        [ "$(ws_path "$tag")" = "$want" ] && { echo "$want"; return 0; }
        sleep 2
    done
    echo "$(ws_path "$tag")"
    return 1
}

# --- traffic ----------------------------------------------------------------
# iperf3 server on the VM, reachable over the overlay. `< /dev/null & disown`
# matters: without it the server dies with the ssh session that started it,
# which reads as "connection refused" and looks like a tunnel fault.
# Starts the server, records its PID, and RETURNS ITS LISTENING STATE. A
# benchmark that assumes the server came up reports the failure to start it as
# a property of the tunnel, which is exactly the wrong conclusion.
vm_iperf_server() {
    local pf="~/.iperf3-$IPERF_PORT.pid"
    vm "f=\$HOME/.iperf3-$IPERF_PORT.pid; [ -f \$f ] && kill -TERM \$(cat \$f) 2>/dev/null; \
        mkdir -p ~/out; setsid nohup iperf3 -s -p $IPERF_PORT > ~/out/iperf3.log 2>&1 < /dev/null & \
        echo \$! > \$f; sleep 1; ss -lnt | grep -c ':$IPERF_PORT '" 2>/dev/null | tail -1
}
ws_iperf_server() {
    local pf="$WORK/iperf3-$IPERF_PORT.pid"
    [ -f "$pf" ] && kill -TERM "$(cat "$pf")" 2>/dev/null
    setsid nohup iperf3 -s -p "$IPERF_PORT" >/dev/null 2>&1 < /dev/null &
    echo $! > "$pf"
    sleep 1
    ss -lnt 2>/dev/null | grep -c ":$IPERF_PORT "
}

# tcp_mbps <target-ip> <seconds> <parallel> [extra iperf3 flags]
# Reports the RECEIVER's bits/s, which is goodput actually delivered; the
# sender's own figure counts bytes handed to the kernel, which on a tunnel can
# sit in a TUN queue that has not been drained yet.
# A FAILED RUN PRINTS `FAILED`, NEVER `0`.
#
# `// 0` on a missing field plus `|| echo 0` on a failed pipeline meant that
# "iperf3 could not run", "iperf3 ran and produced no JSON" and "the path
# delivered nothing" were the same three characters. Seven stages then push
# that value straight into a median (`UP[$q]="${UP[$q]} ${u:-0}"` and its
# siblings), so one unreachable arm would have dragged a whole rung's median
# toward zero and looked like a performance finding. It has not happened yet
# only because nothing has failed yet -- which is the definition of a latent
# defect, not of a safe one.
#
# The discriminator is whether iperf3 produced USABLE JSON, not whether the
# number is small: a blackholed arm genuinely delivering 0 Mbit/s is a
# measurement and must survive as `0`. `// empty` is what separates them --
# a missing field yields nothing, and nothing becomes FAILED.
#
# On the success path the jq program is unchanged, so a working arm prints the
# same bytes it printed before: this is a failure-path-only change and cannot
# move a result that was already valid.
tcp_mbps() {
    local ip="$1" secs="$2" par="$3"; shift 3
    local json v
    json=$(iperf3 -c "$ip" -p "$IPERF_PORT" -t "$secs" -P "$par" -J "$@" 2>/dev/null) \
        || { echo FAILED; return; }
    v=$(printf '%s' "$json" \
        | jq -r '(.end.sum_received.bits_per_second // empty) / 1e6 | (.*100|round)/100' 2>/dev/null)
    case "$v" in ''|null) echo FAILED ;; *) echo "$v" ;; esac
}

# udp_loss <target-ip> <seconds> <rate, e.g. 200M>
# Prints "mbps loss_pct". UDP is how a tunnel's own loss becomes visible: inner
# TCP hides it behind retransmission, and the whole Mathis story is about loss
# the TCP flow cannot see around.
# udp_loss <target-ip> <secs> <total-rate> [streams]
#
# `-b` is PER STREAM in iperf3, so a rung driven with `-P 4 -b 540M` would offer
# 2.16 Gbit/s and the rung measured would not be the rung asked for. The total
# is therefore divided across the streams here, once, rather than at every call
# site.
#
# Streams matter for more than load shape: the kernel hashes flows across a
# multi-queue TUN's queues, so a single UDP flow rides ONE queue however many
# were created. An arm that raises `--tun-queues` and keeps one flow measures
# nothing.
udp_loss() {
    local ip="$1" secs="$2" rate="$3" par="${4:-1}"
    local per="$rate"
    if [ "${par:-1}" -gt 1 ]; then
        per="$(awk -v r="$rate" -v n="$par" 'BEGIN{
            u=""; v=r
            if (r ~ /[MmKkGg]$/) { u=substr(r,length(r),1); v=substr(r,1,length(r)-1) }
            printf "%.0f%s", v/n, u }')"
    fi
    # Same rule as tcp_mbps: no usable JSON is FAILED, not "0 Mbit/s, 0 % lost"
    # -- which would read as a perfect run of a path that never carried a byte.
    local json out
    json=$(iperf3 -c "$ip" -p "$IPERF_PORT" -u -b "$per" -P "$par" -t "$secs" -J 2>/dev/null) \
        || { printf 'FAILED\tFAILED\n'; return; }
    out=$(printf '%s' "$json" | jq -r '
        .end.sum // empty |
        [ ((.bits_per_second // 0)/1e6 | (.*100|round)/100),
          ((.lost_percent // 0)      | (.*100|round)/100) ] | @tsv' 2>/dev/null)
    case "$out" in '') printf 'FAILED\tFAILED\n' ;; *) printf '%s\n' "$out" ;; esac
}

# rtt_ms <target-ip> <count> -- "min avg max mdev"
# Parsed by splitting the summary line on "= " and then on "/", NOT by counting
# whitespace fields: the label itself contains slashes ("rtt min/avg/max/mdev"),
# so a field-position parse is off by the number of label words and silently
# prints mdev under the heading "max". That misparse produced a first set of
# latency numbers in this campaign that had to be thrown away.
rtt_ms() {
    ping -c "$2" -i 0.2 -W 2 -q "$1" 2>/dev/null \
        | awk '/rtt|round-trip/ { sub(/.*= /, ""); sub(/ *ms.*/, ""); gsub("/", " "); print }'
}

# tcp_connect_ms <host> <port> <count>
# The bare-path reference. ICMP to the VM is dropped by its security group, so
# a TCP handshake to a port it does answer is the only honest way to price the
# underlying path against the tunnel's own ICMP RTT. Median of `count`.
tcp_connect_ms() {
    python3 - "$1" "$2" "$3" <<'PY'
import socket, statistics, sys, time
host, port, n = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
v = []
for _ in range(n):
    s = socket.socket()
    s.settimeout(3)
    t = time.perf_counter()
    try:
        s.connect((host, port))
        v.append((time.perf_counter() - t) * 1000)
    except OSError:
        pass
    finally:
        s.close()
    time.sleep(0.1)
print(f"{statistics.median(v):.2f}" if v else "nan")
PY
}

vpn_hdr() { echo "### $* -- $(date -Is)"; }

# --- direct-path blackhole --------------------------------------------------
# Drop UDP to and from the far end so a LIVE direct path dies while the TCP
# relay (to the server, a different host) stays up. This is the only stimulus
# on the real path that exercises DEC-2's promise -- fall back to the warm
# relay IN PLACE, no reconnect, TUN preserved -- and the only way to price
# BORE_DIRECT_QUIC_IDLE_MS / _KEEPALIVE_MS, whose whole job is to decide how
# long the tunnel stays dead before it notices.
#
# It is NOT `--relay-only`: that arm never has a direct path to lose, so it
# measures a different thing entirely.
blackhole_on()  { sudo -n "$ROOTSH" blackhole on "$(getent ahostsv4 "$BORE_VM" 2>/dev/null | awk '{print $1; exit}')" >/dev/null 2>&1; }
blackhole_off() { sudo -n "$ROOTSH" blackhole off >/dev/null 2>&1; }
