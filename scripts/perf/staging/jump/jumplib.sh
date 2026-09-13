#!/usr/bin/env bash
# Shared plumbing for the SSH JUMP HOST campaign.
#
# WHY THIS CAMPAIGN MEASURES LATENCY AND NOT BANDWIDTH
# ----------------------------------------------------
# Every other campaign in this repository asks how many bits per second a tunnel
# moves. The jump host is the one mode where that is the wrong question. What
# rides it is an interactive SSH session and the occasional `direct-tcpip`, so
# what a user feels is the ROUND TRIP: how long until the prompt appears, and
# how long a keystroke takes once it has. A jump host that moves a gigabit but
# adds 200 ms to every character is a bad jump host.
#
# It is also the mode with the least headroom for error: an SSH channel uses
# exactly ONE bidi stream (I-SSH jump invariant), so carriers here buy isolation
# rather than throughput, and any latency they add is pure cost. That is a claim
# the campaign must test rather than repeat.
#
# WHAT IS MEASURED, AND WHY EACH ONE IS SEPARATE
# ----------------------------------------------
#   open_ms   Full `ssh -J` to a trivial command. This is "how long to get in",
#             the number a user times with their own patience. It contains the
#             TCP connect, the OUTER SSH handshake to the gateway, the
#             `direct-tcpip` channel open across the tunnel to the provider, and
#             the INNER SSH handshake with the real sshd. Quoted alone it hides
#             which of those four moved, so it is always reported beside:
#   tcp_ms    Raw TCP connect to the gateway. The floor: no configuration can
#             beat it, and it is the term that follows the network rather than
#             the product.
#   wchan_ms  `ssh -W` — the ProxyJump transport primitive, i.e. everything
#             except the inner SSH handshake. `open_ms - wchan_ms` is therefore
#             the inner handshake and `wchan_ms - tcp_ms` is the gateway's own
#             cost, which is the part this project can actually change.
#   chan_ms   A NEW channel on an ALREADY ESTABLISHED session (ControlMaster).
#             This is the dominant interactive cost in real use -- an editor
#             opening a second connection, an rsync, a port forward -- and it is
#             the one number that excludes every handshake.
#   echo_ms   A byte written into a live session and read back. As close to
#             keystroke latency as a script can get, and the only measurement
#             here that contains no connection setup at all.
#
# MEASUREMENT DISCIPLINE (inherited, and not negotiable)
#   * Arms interleave inside each repetition; a whole sweep per arm would
#     compare two transports across the drift between them.
#   * The direct (`--udp`) arm is WAITED FOR and VERIFIED, never assumed: a
#     provider always starts on the warm TCP relay, so an arm that never
#     upgraded would report relay latency under the label "direct". An arm whose
#     path does not match its label is printed FAILED and excluded, never
#     averaged in.
#   * Raw samples are printed beside every median. A median-only table hides the
#     locale bug that once reported 264 as the middle of {397, 264, 408}, and
#     hides bimodality -- which is exactly the shape the secret campaign's
#     `direct_ready_ms` turned out to have.
#   * `LC_ALL=C` for every numeric sort, pinned here so a helper defined inside
#     a stage cannot reintroduce the bug.
set -uo pipefail
export LC_ALL=C

HERE_JUMP="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
. "$HERE_JUMP/../lib.sh"

# --- coordinates ------------------------------------------------------------
# The gateway runs on the TEST VM, never on staging: a staging redeploy restarts
# the server and drops the operator's live tunnels, which needs explicit
# approval and is not part of this campaign.
JUMP_CTRL_PORT="${JUMP_CTRL_PORT:-7845}"   # bore control port on the test VM
JUMP_SSH_PORT="${JUMP_SSH_PORT:-7846}"     # dedicated SSH port (keeps us off 22/443, no root)
JUMP_BASE_DOMAIN="${JUMP_BASE_DOMAIN:-j.test}"
JUMP_ALIAS="${JUMP_ALIAS:-wsjump}"
# The gateway principal: the authorized-keys FILE NAME and the SSH username
# the client connects with are the same string, by construction (see
# `jump_start_gateway`). Not `$USER` and not `$BORE_VM_USER` -- neither of
# those is bound to the alias.
#
# TWO HOPS, TWO IDENTITIES, and confusing them costs an afternoon:
#
#   ssh -J $JUMP_SSH_USER@<vm>:$JUMP_SSH_PORT  $JUMP_INNER_USER@$JUMP_TARGET
#          ^^^^^^^^^^^^^^ outer: the GATEWAY     ^^^^^^^^^^^^^^^ inner: an
#          authenticated against `ak/$JUMP_SSH_USER`   account on the sshd the
#          and bound to the alias                      provider splices to
#
# `$JUMP_SSH_USER` is a principal that exists only inside bore's gateway; it is
# not a Unix account anywhere. `$JUMP_INNER_USER` is a real account on the
# INNER target (see `jump_inner_target.sh`), which is what the inner sshd
# authenticates. Using the gateway principal for the inner hop fails at the
# inner sshd (no such user); using an inner account for the outer hop fails at
# the gateway with `classic_auth_required`. `$USER` is neither of them and
# belongs in neither position.
JUMP_SSH_USER="${JUMP_SSH_USER:-bench}"
# The alias hostname is resolved by the GATEWAY, not by the client: `ssh -J`
# hands the target name to the jump server, which parses `<alias>.<base>`
# itself. So this campaign needs no DNS for JUMP_BASE_DOMAIN.
JUMP_TARGET="$JUMP_ALIAS.$JUMP_BASE_DOMAIN"

# THE INNER TARGET. Not port 22 of this workstation: there is no sshd on it
# (`openssh-server` is not installed and 22 answers `Connection refused`), and
# pointing the provider there is what made every `open` sample read FAILED and
# every `wchan` sample read a plausible NUMBER for the whole first bring-up.
# `jump_inner_target.sh` stands up a real OpenSSH in a container bound to
# 127.0.0.1 and explains why it is a container; `jump_require_inner_target`
# below refuses to let a stage measure without it.
#
# THE INNER PORT IS A CLIENT-SIDE PARAMETER, not just a provider-side one.
# `sshjhost 127.0.0.1:2222` registers the alias AT PORT 2222 -- the provider's
# own banner says so (`SSH jump host ready hostname=wsjump.j.test port=2222`,
# `connect with: ssh -J <vm>:7845 -p 2222 wsjump.j.test`) -- and the gateway
# matches the port the client asks for against the registered one. A client
# requesting `:22` is answered `channel 0: open failed: connect failed`, which
# reads exactly like an unreachable origin and is not one. So every client-side
# spelling carries it: `-W $JUMP_TARGET:$JUMP_INNER_PORT` for the transport
# primitive, `-p $JUMP_INNER_PORT` for each `-J` form (where `-p` is the FINAL
# destination's port, the jump's own port living inside the `-J` argument).
JUMP_INNER_PORT="${JUMP_INNER_PORT:-2222}"
JUMP_INNER_USER="${JUMP_INNER_USER:-bench}"

# THE REMOTE HOME IS RESOLVED, NOT SPELLED `$HOME`.
#
# `\$HOME/bore-jump` works wherever the path reaches a remote SHELL (`ssh host
# "sha256sum \$HOME/..."` expands it), and FAILS wherever it reaches SFTP.
# OpenSSH 9 made `scp` use the SFTP protocol by default, and SFTP does not run a
# shell: the remote path is taken literally. MEASURED on the first P6 run --
# `scp: dest open "$HOME/bore-jump": No such file or directory`, i.e. it looked
# for a DIRECTORY named `$HOME`. All three stages failed at provisioning.
#
# The fix is one spelling that works in both places, which means an ABSOLUTE
# path, which means asking the far end where its home is. Done once here, and
# loudly: an unresolved home would otherwise silently produce `/bore-jump`,
# which the VM's user cannot write -- a coordinate defect turning into a
# permission error three stages later.
JUMP_HOME="${JUMP_HOME:-$(vm 'printf %s "$HOME"' 2>/dev/null | tr -d '\r')}"
case "$JUMP_HOME" in
    /*) ;;
    *)  echo "jumplib: cannot resolve the test VM's home directory (got '$JUMP_HOME')" >&2
        echo "         every jump stage would deploy to an unwritable path; refusing." >&2
        return 1 2>/dev/null || exit 2 ;;
esac
JUMP_REMOTE_DIR="${JUMP_REMOTE_DIR:-$JUMP_HOME/jump}"
JUMP_BORE_VM="${JUMP_BORE_VM:-$JUMP_HOME/bore-jump}"
# The jump-host campaign needs a binary built with an EXTRA feature
# (`ssh-gateway`), and it must not be `target/release/bore`. That path is the
# artefact every VPN stage in this window is being measured with, and its
# checksum is recorded in the sweep driver's provenance header. Building a
# different feature set into it would (a) silently change the binary under a
# campaign that has already published numbers against the old one, and (b) make
# cargo rebuild the whole workspace again the next time a VPN stage runs, since
# the feature set would have to change back. So the jump build gets its OWN
# CARGO_TARGET_DIR and its own binary path; nothing the other stages touch moves.
JUMP_TARGET_DIR="${JUMP_TARGET_DIR:-$HERE_JUMP/../../../../target/jump}"
WS_BORE="${WS_BORE:-$JUMP_TARGET_DIR/release/bore}"

# Extra flags for the gateway. The direct (`--udp`) arms of every jump stage
# need the server to HAVE a QUIC endpoint, so this is not optional decoration:
# without it a `--udp` provider stays on the warm relay for ever and the arm is
# reported FAILED after a 90 s wait. It lived only in the stages, read bare
# here, so a new stage that forgot it would abort inside `jump_start_gateway`.
# The default is the library's; a stage may still override it.
JUMP_SERVER_EXTRA="${JUMP_SERVER_EXTRA:---udp --vhost-quic-port 7847}"

JUMP_RUN_ID="j$(date +%s%N | tail -c 6)"
JUMP_WS_PIDS=()

# Non-interactive ssh everywhere: a campaign must never block on a prompt.
JSSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
           -o LogLevel=ERROR -o ConnectTimeout=10)

jump_cleanup() {
    local p
    for p in "${JUMP_WS_PIDS[@]:-}"; do
        [ -n "$p" ] && kill -TERM "$p" 2>/dev/null
    done
    # Narrow, per-run pattern only. A blanket `pkill bore` would kill the
    # operator's own live tunnels on this host -- project rule.
    pkill -TERM -f -- "--subdomain $JUMP_ALIAS" 2>/dev/null
    rm -f "${JUMP_CTL_SOCK:-/nonexistent}" 2>/dev/null
    vm "pkill -TERM -f -- '--ssh-jump-base-domain $JUMP_BASE_DOMAIN'" >/dev/null 2>&1
}

# --- gateway provisioning ---------------------------------------------------

# Build and ship a binary that actually HAS the feature. The shipped release
# binary is built `--features vpn` only: `sshjhost` is present (it is a client
# subcommand) but `--ssh-gateway` is not, so a campaign that skipped this step
# would fail at server start with an unhelpful clap error.
jump_build_and_deploy() {
    local sha_local sha_vm
    echo "  building --features vpn,ssh-gateway (nice, -j 3)"
    ( cd "$HERE_JUMP/../../../.." && CARGO_TARGET_DIR="$JUMP_TARGET_DIR" \
        nice -n 19 cargo build --release -j 3 \
        --features vpn,ssh-gateway ) || return 1
    [ -x "$WS_BORE" ] || { echo "  FAILED: no binary at $WS_BORE after the build"; return 1; }
    # The feature must be IN the binary, not merely on the command line: a
    # stale artefact or a silently-skipped rebuild would otherwise be measured
    # as a jump host that behaves oddly rather than as one that is not there.
    if ! "$WS_BORE" server --help 2>/dev/null | grep -q -- '--ssh-gateway'; then
        echo "  FAILED: $WS_BORE has no --ssh-gateway; the feature did not build in"
        return 1
    fi
    sha_local=$(sha256sum "$WS_BORE" | cut -c1-16)
    # COPY BESIDE, THEN RENAME OVER. Writing straight onto the destination fails
    # with ETXTBSY (`scp: dest open ...: Failure`) whenever a gateway from an
    # earlier run is still executing that exact file -- which is the normal
    # state between two stages, since each stage starts its own gateway and the
    # previous one may still be shutting down. `mv` unlinks the old inode
    # instead of writing into it, so the running process keeps the image it
    # already mapped and the next start gets the new one.
    vmcp "$WS_BORE" "$BORE_VM_USER@$BORE_VM:$JUMP_BORE_VM.new" || return 1
    vm "chmod +x '$JUMP_BORE_VM.new' && mv -f '$JUMP_BORE_VM.new' '$JUMP_BORE_VM'" \
        >/dev/null 2>&1 || { echo "  FAILED: could not install the binary on the VM"; return 1; }
    sha_vm=$(vm "sha256sum $JUMP_BORE_VM | cut -c1-16" 2>/dev/null | tr -d '\r')
    # P-12's rule applied to a file: the copy proves we sent bytes, the checksum
    # proves both ends run the same ones. A silent truncation here would be
    # reported as a performance difference.
    if [ "$sha_local" != "$sha_vm" ]; then
        echo "  FAILED: binary checksum mismatch local=$sha_local vm=$sha_vm"
        return 1
    fi
    echo "  binary $sha_local on both ends"
}

# The gateway needs at least one credential source or it refuses to start. The
# workstation's existing public key is used, so no new secret is minted and
# nothing has to be written down anywhere.
jump_start_gateway() {
    local pub
    pub=$(cat ~/.ssh/id_ed25519.pub 2>/dev/null || cat ~/.ssh/id_rsa.pub 2>/dev/null)
    [ -n "$pub" ] || { echo "  FAILED: no local ssh public key to authorize"; return 1; }

    # THE FILENAME IS THE USERNAME, AND THE CLIENT MUST USE IT.
    #
    # `--ssh-authorized-keys-dir` holds ONE FILE PER USERNAME, and the file name
    # is what binds a key to a jump principal: a key in `ak/bench` grants
    # `jump_principal = Some("bench")` only when the SSH client connects AS
    # `bench`. Connect with any other username and the key still AUTHENTICATES
    # -- legacy compatibility is deliberate -- but the principal is `None`, and
    # the gateway then refuses the jump with `reason="classic_auth_required"`
    # (`--ssh-jump-base-domain` is what turns that requirement on,
    # `src/main.rs`: `ssh_jump_classic_auth_required: ssh_jump_base_domain.is_some()`).
    #
    # MEASURED: the key went into `ak/bench` while every client connected as
    # `$BORE_VM_USER` (`ubuntu`), so `ssh -v` showed `Authenticated ... using
    # "publickey"` immediately followed by `channel 0: open failed: connect
    # failed`, and the gateway logged the deny. An auth that succeeds and a
    # jump that is refused is a confusing pair to read, which is why the two
    # names now come from ONE variable instead of being spelled twice.
    vm "mkdir -p $JUMP_REMOTE_DIR/ak && printf '%s\n' '$pub' > $JUMP_REMOTE_DIR/ak/$JUMP_SSH_USER" \
        >/dev/null 2>&1 || return 1

    vm "pkill -TERM -f -- '--ssh-jump-base-domain $JUMP_BASE_DOMAIN'" >/dev/null 2>&1
    sleep 1
    # setsid + nohup: the server must outlive the ssh session that started it.
    #
    # `cd X; ... & true` and NOT `cd X && ... &`, and that is the whole
    # difference between this returning and this hanging.
    #
    # `A && B &` backgrounds the ENTIRE list: bash forks one subshell for
    # `cd X && setsid ...`, and that subshell inherits ssh's stdout, because the
    # `> log 2>&1` redirections belong to the `setsid` command inside it. ssh
    # closes a session only when every process holding the channel is gone, so
    # it waited on a shell that was not going anywhere.
    #
    # MEASURED on the second P6 run: the gateway came up correctly (listening on
    # :7845 and :7846, log clean) and the ssh that started it sat for 417 s
    # until it was killed -- the stage never got past provisioning, with no
    # output and no error, because nothing had failed. A hang whose subject
    # SUCCEEDED is the worst shape to debug, so it is pinned here.
    #
    # With `;` the `&` binds to the `setsid` command alone, and `true` gives the
    # shell something to exit on. This is the exact shape of `up()` in
    # `pub/ws_conns.sh`, which has always returned promptly -- it never had the
    # `cd &&` prefix.
    vm "cd $JUMP_REMOTE_DIR; setsid nohup $JUMP_BORE_VM server \
          --control-port $JUMP_CTRL_PORT \
          --ssh-gateway \
          --ssh-port $JUMP_SSH_PORT \
          --ssh-jump-base-domain $JUMP_BASE_DOMAIN \
          --ssh-host-key-file $JUMP_REMOTE_DIR/host_key.pem \
          --ssh-authorized-keys-dir $JUMP_REMOTE_DIR/ak \
          --secret '$BORE_SECRET' \
          --admin-token '$ADMIN_TOKEN' \
          $JUMP_SERVER_EXTRA \
          > $JUMP_REMOTE_DIR/server.log 2>&1 < /dev/null & true" >/dev/null 2>&1

    local i
    for i in $(seq 1 30); do
        if timeout 3 bash -c "exec 3<>/dev/tcp/$BORE_VM/$JUMP_SSH_PORT" 2>/dev/null; then
            echo "  gateway up on :$JUMP_SSH_PORT after ${i}s"; return 0
        fi
        sleep 1
    done
    echo "  FAILED: gateway never accepted on :$JUMP_SSH_PORT"
    vm "tail -20 $JUMP_REMOTE_DIR/server.log" 2>/dev/null | sed 's/^/    /'
    return 1
}

# The provider publishes THIS workstation's sshd, so the measured path crosses
# the real access link exactly as a NAT-bound provider would in production.
# READINESS IS READ FROM THE SERVER, NOT FROM THE CLIENT'S LOG (P-12).
#
# This used to wait for `registered|ready|jump host` to appear in the provider's
# own output. MEASURED: `sshjhost` prints NOTHING on a successful registration,
# so that grep could not match on success -- only the 40-iteration timeout could
# fire, and every arm of every repetition reported `provider FAILED` even when
# the provider was healthy. A readiness check that cannot observe readiness is
# worse than none: it converts every run into a uniform failure that looks like
# a product problem.
#
# The server's own admin API answers the question directly: the alias either
# owns a row or it does not. That is also the same source `jump_path` reads, so
# readiness and path can never disagree about whether the provider exists.
#
# DIAGNOSTICS GO TO STDERR because this function is called as `pid=$(...)`:
# anything it writes to stdout is captured into the variable instead of shown
# (trap 26). The old spelling printed a five-line log tail on failure that
# nobody ever saw -- it was being assigned to `$pid`.
jump_start_provider() {
    local extra="${1:-}" log="$WORK/jump-prov-$JUMP_RUN_ID.log"
    "$WS_BORE" sshjhost "127.0.0.1:$JUMP_INNER_PORT" \
        --subdomain "$JUMP_ALIAS" \
        --to "$BORE_VM:$JUMP_CTRL_PORT" \
        --secret "$BORE_SECRET" \
        $extra > "$log" 2>&1 &
    local pid=$!
    JUMP_WS_PIDS+=("$pid")
    local i rows
    for i in $(seq 1 40); do
        rows=$(jump_rows)
        case "$rows" in
            ''|*[!0-9]*) : ;;
            0) : ;;
            *) echo "$pid"; return 0 ;;
        esac
        kill -0 "$pid" 2>/dev/null || {
            echo "  provider died:" >&2; tail -5 "$log" | sed 's/^/    /' >&2; return 1; }
        sleep 0.5
    done
    echo "  FAILED: provider never registered (server reports rows='$rows')" >&2
    tail -5 "$log" | sed 's/^/    /' >&2
    return 1
}

# Which path is the provider actually on? Read it from the SERVER's admin API,
# never from the client's log: the log proves the client talked about a path,
# the API proves the server believes it (P-12).
#
# THE FIELDS HERE ARE NOT THE ONES THE OTHER REGISTRIES USE, AND ASSUMING THEY
# WERE COST THIS CAMPAIGN NOTHING ONLY BECAUSE IT WAS CAUGHT BEFORE THE RUN.
# vhost and public publish `current_path`; `SshJumpView` (src/admin_views.rs)
# publishes NEITHER `current_path` NOR `alias`. The first version of this helper
# asked for `select(.alias==$a)|.current_path`, which selects no row and prints
# NOTHING -- so every `--udp` arm would have been waited out for 90 s and then
# reported FAILED, and the jump campaign would have produced zero direct data
# while looking like it had merely been unlucky. Exactly the class this campaign
# keeps paying for: an instrument that fails as a blank rather than as an error.
#
# So the path is DERIVED from the fields that do exist:
#   hostname          the full ProxyJump name, whose first label is the alias
#   udp_active        at least one direct QUIC carrier is LIVE
# `udp_active` means the direct path is AVAILABLE. What proves a channel really
# used it is `direct_stream_opens` climbing, and what proves one did not is
# `direct_fallbacks` -- both exposed by `jump_counters` below, so a stage can
# corroborate rather than trust a single boolean.
#
# `absent` is returned when the alias owns no row at all, and is deliberately
# NOT the same token as `relay`: a provider that never registered and one that
# registered on the relay are opposite diagnoses.
JUMP_API="admin/api/v1/ssh-jump"

# THE SCHEME IS `http`, AND THAT IS NOT AN OVERSIGHT.
#
# The staging server serves its control port over TLS, so every other stage in
# this harness reaches an admin API with `https://`. This gateway is started by
# `jump_start_gateway` WITHOUT a certificate -- its own log says
# `server listening ... tls=false` -- so `https://` here talks TLS to a
# plaintext socket. Both the provider's `--to` and this call had inherited the
# staging spelling, and both failed: the provider with
# `TLS handshake failed: received corrupt message of type InvalidContentType`,
# and this one with an empty body that `jq` turned into `absent` -- i.e. into
# "the provider never registered", which is the wrong diagnosis and the
# expensive one.
#
# Plaintext costs nothing that is measured here: the control port carries only
# registration, while every latency this campaign publishes is measured on the
# SSH port.
jump_api() {
    curl -s --max-time 5 -H "Authorization: Bearer $ADMIN_TOKEN" \
        "http://$BORE_VM:$JUMP_CTRL_PORT/$JUMP_API" 2>/dev/null
}

jump_path() {
    jump_api | jq -r --arg a "$JUMP_ALIAS" '
        [ .[] | select((.hostname | split(".")[0]) == $a) ] as $r
        | if   ($r | length) == 0 then "absent"
          elif ($r[0].udp_active)  then "direct"
          else "relay" end' 2>/dev/null
}

# How many admin rows does this ONE alias own? The invariant is exactly one,
# across carriers and reconnect storms, and the server's own API is the only
# place it can be checked.
jump_rows() {
    jump_api | jq -r --arg a "$JUMP_ALIAS" \
        '[ .[] | select((.hostname | split(".")[0]) == $a) ] | length' 2>/dev/null
}

# opens<TAB>fallbacks<TAB>carriers -- the corroboration for `jump_path`.
#
# AN ABSENT ROW PRINTS `?`, NEVER `0`, AND THAT IS THE WHOLE POINT.
# This used to answer "0\t0\t0" when the alias owned no row. A caller then
# compares before against after, reads 0 -> 0, and convicts the PRODUCT of a
# counter that did not move -- when what actually happened is that the admin
# API had nothing to say. It is the campaign's most expensive defect class
# ("a zero that means the instrument failed"), sitting in the helper that feeds
# the one check in `jump_stab.sh` carrying the fallback promise.
# `?` is not an integer, so a caller that forgets to guard gets a loud shell
# error instead of a quiet wrong verdict.
jump_counters() {
    jump_api | jq -r --arg a "$JUMP_ALIAS" '
        [ .[] | select((.hostname | split(".")[0]) == $a) ][0]
        | if . == null then "? ? ?"
          else "\(.direct_stream_opens)\t\(.direct_fallbacks)\t\(.direct_carriers)" end' 2>/dev/null
}

jump_wait_path() {
    local want="$1" secs="${2:-90}" i p
    for i in $(seq 1 "$secs"); do
        p=$(jump_path)
        [ "$p" = "$want" ] && { echo "$want"; return 0; }
        sleep 1
    done
    echo "${p:-unknown}"
}

# --- the five measurements --------------------------------------------------

ms_since() { LC_ALL=C awk -v a="$1" -v b="$2" 'BEGIN{printf "%.1f", (b-a)*1000}'; }
now()      { date +%s.%N; }

# TCP connect to the gateway. The floor.
jump_tcp_ms() {
    local t0 t1
    t0=$(now)
    timeout 5 bash -c "exec 3<>/dev/tcp/$BORE_VM/$JUMP_SSH_PORT; exec 3<&-" 2>/dev/null || { echo FAILED; return; }
    t1=$(now); ms_since "$t0" "$t1"
}

# `ssh -W` is the ProxyJump transport primitive itself: outer handshake plus the
# direct-tcpip channel across the tunnel, and NOT the inner handshake.
#
# IT MUST INSPECT THE BYTES. This helper used to end in
# `... | head -c 1 >/dev/null || { echo FAILED; return; }`, whose comment
# claimed that reading one byte proved the channel carried data. It proved
# nothing: a pipeline that produced ZERO bytes still exits 0, so with no sshd
# behind the provider at all the helper published 1270.3 ms -- the time it took
# to fail -- as a latency. Same defect class as every other "a zero that means
# the instrument failed" in this campaign, hiding this time inside a pipe.
#
# `SSH-` is the discriminator because RFC 4253 requires the identification
# string to begin with it; nothing else this channel can carry does.
jump_wchan_ms() {
    local t0 t1 banner
    t0=$(now)
    banner=$(timeout 25 ssh "${JSSH_OPTS[@]}" -p "$JUMP_SSH_PORT" \
        -W "$JUMP_TARGET:$JUMP_INNER_PORT" "$JUMP_SSH_USER@$BORE_VM" </dev/null 2>/dev/null | head -c 4)
    t1=$(now)
    case "$banner" in SSH-*) ;; *) echo FAILED; return ;; esac
    ms_since "$t0" "$t1"
}

# THE PREMISE OF THE MEASUREMENT, checked before the measurement.
#
# `open_ms - wchan_ms` is the inner sshd's handshake and `wchan_ms - tcp_ms` is
# the gateway's own cost -- the second is this campaign's deliverable and it is
# obtained by SUBTRACTION, so an absent inner sshd does not merely lose one
# column, it silently corrupts the other. A stage must therefore prove the
# inner target answers before it is allowed to publish anything.
jump_require_inner_target() {
    local banner
    banner=$(timeout 5 bash -c "exec 3<>/dev/tcp/127.0.0.1/$JUMP_INNER_PORT; head -c 4 <&3" 2>/dev/null)
    case "$banner" in
        SSH-*) return 0 ;;
    esac
    echo "INSTRUMENT FAILURE: no SSH server on 127.0.0.1:$JUMP_INNER_PORT (read '$banner')." >&2
    echo "  The inner hop of \`ssh -J\` does not exist, so neither the inner sshd's" >&2
    echo "  handshake nor the gateway's own cost can be measured. Start it with:" >&2
    echo "    scripts/perf/staging/jump/jump_inner_target.sh up" >&2
    return 1
}

# The same thing, but it will START the inner target rather than only complain.
# This is what a STAGE calls: the campaign has to be re-runnable from one
# command months later (`rerun_jump.sh`), and a stage that aborts because a
# helper container is not running is a stage nobody re-runs. It still refuses
# to measure if the target cannot be brought up -- bringing it up and checking
# it are separate steps on purpose, so "started" is never mistaken for "ready".
#
# It deliberately does NOT stop the container afterwards: the three jump stages
# run as separate processes and would each pay the start-up cost. `rerun_jump.sh`
# takes it down when the campaign ends, which is what leaves the workstation
# clean.
jump_ensure_inner_target() {
    jump_require_inner_target 2>/dev/null && return 0
    echo "  inner target not up; starting it" >&2
    "$HERE_JUMP/jump_inner_target.sh" up >&2 || return 1
    jump_require_inner_target
}

# The whole thing, to a trivial command.
jump_open_ms() {
    local t0 t1
    t0=$(now)
    timeout 30 ssh "${JSSH_OPTS[@]}" -p "$JUMP_INNER_PORT" \
        -J "$JUMP_SSH_USER@$BORE_VM:$JUMP_SSH_PORT" \
        "$JUMP_INNER_USER@$JUMP_TARGET" true </dev/null >/dev/null 2>&1 || { echo FAILED; return; }
    t1=$(now); ms_since "$t0" "$t1"
}

JUMP_CTL_SOCK="$WORK/jumpctl.$JUMP_RUN_ID"

jump_master_open() {
    rm -f "$JUMP_CTL_SOCK"
    timeout 40 ssh "${JSSH_OPTS[@]}" -M -N -f -p "$JUMP_INNER_PORT" \
        -o ControlPath="$JUMP_CTL_SOCK" -o ControlPersist=600 \
        -J "$JUMP_SSH_USER@$BORE_VM:$JUMP_SSH_PORT" \
        "$JUMP_INNER_USER@$JUMP_TARGET" >/dev/null 2>&1 || return 1
    [ -S "$JUMP_CTL_SOCK" ]
}
jump_master_close() {
    ssh -O exit -o ControlPath="$JUMP_CTL_SOCK" "$JUMP_INNER_USER@$JUMP_TARGET" >/dev/null 2>&1
    rm -f "$JUMP_CTL_SOCK"
}

# A new CHANNEL on an already established session: no handshake of any kind.
jump_chan_ms() {
    local t0 t1
    [ -S "$JUMP_CTL_SOCK" ] || { echo FAILED; return; }
    t0=$(now)
    timeout 15 ssh -o ControlPath="$JUMP_CTL_SOCK" "$JUMP_INNER_USER@$JUMP_TARGET" true \
        </dev/null >/dev/null 2>&1 || { echo FAILED; return; }
    t1=$(now); ms_since "$t0" "$t1"
}

# A byte in, the same byte out, over a live session. The closest a script gets
# to keystroke latency.
jump_echo_ms() {
    local t0 t1 out
    [ -S "$JUMP_CTL_SOCK" ] || { echo FAILED; return; }
    t0=$(now)
    out=$(printf 'x\n' | timeout 15 ssh -o ControlPath="$JUMP_CTL_SOCK" \
          "$JUMP_INNER_USER@$JUMP_TARGET" "head -c 2" 2>/dev/null)
    t1=$(now)
    [ -n "$out" ] || { echo FAILED; return; }
    ms_since "$t0" "$t1"
}
