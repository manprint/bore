#!/usr/bin/env bash
# Entrypoint for the bore SSH-gateway tunnel client container (docker/Dockerfile.ssh).
#
# Builds and execs an `autossh` command entirely from environment variables.
# All modes of the bore SSH gateway are supported:
#   vhost | public | secret-provider | secret-consumer | raw
#
# See compose.ssh.yml for the full variable reference and per-mode examples.
#
# Reliability model (three independent layers):
#   1. OpenSSH keepalives (ServerAliveInterval/CountMax + TCPKeepAlive) detect a
#      dead/wedged server and make ssh exit instead of hanging forever.
#   2. autossh -M0 restarts ssh immediately on any exit (AUTOSSH_GATETIME=0:
#      failures during the first seconds also trigger a restart; the gateway's
#      same-identity takeover, I-SSH5, makes fast reconnection safe).
#   3. Docker `restart: always` restarts the whole container if autossh itself dies.
#
# NOTE: never use -N (the gateway's status banner and all warnings arrive on the
# session channel, which -N suppresses entirely — see docs/SSH_GATEWAY.md §6.4a).
# -T is used instead: it only skips the PTY, keeping the banner visible in logs.
set -euo pipefail

log() { echo "[bore-ssh-tunnel] $*" >&2; }
die() { log "ERROR: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Required / mode selection
# ---------------------------------------------------------------------------
: "${BORE_SSH_HOST:?BORE_SSH_HOST is required (bore server hostname/IP)}"
BORE_SSH_PORT="${BORE_SSH_PORT:-7835}"
BORE_SSH_USER="${BORE_SSH_USER:-tunnel}"   # ignored by the gateway (label only)
TUNNEL_MODE="${TUNNEL_MODE:?TUNNEL_MODE is required: vhost | public | secret-provider | secret-consumer | raw}"

# ---------------------------------------------------------------------------
# Tunnel target / naming
# ---------------------------------------------------------------------------
LOCAL_HOST="${LOCAL_HOST:-host.docker.internal}"
LOCAL_PORT="${LOCAL_PORT:-}"
VHOST_LABEL="${VHOST_LABEL:-}"
PUBLIC_PORT="${PUBLIC_PORT:-0}"            # 0 = server-assigned port
SECRET_ID="${SECRET_ID:-}"
LISTEN_ADDRESS="${LISTEN_ADDRESS:-0.0.0.0}"
LISTEN_PORT="${LISTEN_PORT:-}"
FORWARD_SPEC="${FORWARD_SPEC:-}"           # raw mode only
EXTRA_FORWARDS="${EXTRA_FORWARDS:-}"       # extra "-R spec"/"-L spec" tokens
EXEC_PARAMS="${EXEC_PARAMS:-}"             # exec string after `--`

# ---------------------------------------------------------------------------
# SSH stability options (defaults tuned for long-lived unattended tunnels)
# ---------------------------------------------------------------------------
SERVER_ALIVE_INTERVAL="${SERVER_ALIVE_INTERVAL:-15}"
SERVER_ALIVE_COUNT_MAX="${SERVER_ALIVE_COUNT_MAX:-3}"
CONNECT_TIMEOUT="${CONNECT_TIMEOUT:-10}"
EXIT_ON_FORWARD_FAILURE="${EXIT_ON_FORWARD_FAILURE:-yes}"
TCP_KEEPALIVE="${TCP_KEEPALIVE:-yes}"
STRICT_HOST_KEY_CHECKING="${STRICT_HOST_KEY_CHECKING:-accept-new}"
KNOWN_HOSTS_FILE="${KNOWN_HOSTS_FILE:-/ssh/known_hosts}"
SSH_LOG_LEVEL="${SSH_LOG_LEVEL:-INFO}"
EXTRA_SSH_OPTS="${EXTRA_SSH_OPTS:-}"
SSH_OVER_TLS="${SSH_OVER_TLS:-off}"

# autossh reliability knobs (exported: read by autossh itself)
export AUTOSSH_GATETIME="${AUTOSSH_GATETIME:-0}"
export AUTOSSH_POLL="${AUTOSSH_POLL:-30}"

# ---------------------------------------------------------------------------
# Auth material
# ---------------------------------------------------------------------------
SSH_KEY_FILE="${SSH_KEY_FILE:-}"
SSH_PASSWORD="${SSH_PASSWORD:-}"
SSH_PASSWORD_FILE="${SSH_PASSWORD_FILE:-}"

RUNTIME_DIR=/tmp/bore-ssh
mkdir -p "$RUNTIME_DIR"
chmod 700 "$RUNTIME_DIR"

AUTH_ARGS=()
HAVE_KEY=0
HAVE_PASSWORD=0

if [[ -n "$SSH_KEY_FILE" ]]; then
    [[ -r "$SSH_KEY_FILE" ]] || die "SSH_KEY_FILE=$SSH_KEY_FILE is not readable"
    # Copy so mounted-volume ownership/permissions can't make ssh reject the key.
    cp "$SSH_KEY_FILE" "$RUNTIME_DIR/id_key"
    chmod 600 "$RUNTIME_DIR/id_key"
    AUTH_ARGS+=(-i "$RUNTIME_DIR/id_key" -o IdentitiesOnly=yes)
    HAVE_KEY=1
fi

if [[ -n "$SSH_PASSWORD_FILE" ]]; then
    [[ -r "$SSH_PASSWORD_FILE" ]] || die "SSH_PASSWORD_FILE=$SSH_PASSWORD_FILE is not readable"
    SSHPASS="$(cat "$SSH_PASSWORD_FILE")"
    export SSHPASS
    HAVE_PASSWORD=1
elif [[ -n "$SSH_PASSWORD" ]]; then
    export SSHPASS="$SSH_PASSWORD"
    HAVE_PASSWORD=1
fi

if (( HAVE_PASSWORD )); then
    # autossh restarts ssh internally on every reconnect, so wrapping autossh in
    # sshpass would only feed the FIRST prompt. AUTOSSH_PATH points autossh at a
    # wrapper that re-applies sshpass on every ssh (re)spawn.
    cat > "$RUNTIME_DIR/ssh-with-pass" <<'WRAP'
#!/bin/sh
exec sshpass -e /usr/bin/ssh "$@"
WRAP
    chmod 700 "$RUNTIME_DIR/ssh-with-pass"
    export AUTOSSH_PATH="$RUNTIME_DIR/ssh-with-pass"
elif (( HAVE_KEY )); then
    # Key-only: never hang on an interactive prompt.
    AUTH_ARGS+=(-o BatchMode=yes)
else
    die "no auth configured: set SSH_KEY_FILE, or SSH_PASSWORD / SSH_PASSWORD_FILE"
fi

# ---------------------------------------------------------------------------
# Host key verification
# ---------------------------------------------------------------------------
HOSTKEY_ARGS=()
if [[ -r "$KNOWN_HOSTS_FILE" ]]; then
    HOSTKEY_ARGS+=(-o "UserKnownHostsFile=$KNOWN_HOSTS_FILE" \
                   -o "StrictHostKeyChecking=$STRICT_HOST_KEY_CHECKING")
else
    if [[ "$STRICT_HOST_KEY_CHECKING" == "yes" ]]; then
        die "STRICT_HOST_KEY_CHECKING=yes but KNOWN_HOSTS_FILE=$KNOWN_HOSTS_FILE is missing/unreadable"
    fi
    # No pinned known_hosts mounted: persist first-seen key for the container's
    # lifetime so a MITM appearing AFTER first connect is still detected.
    HOSTKEY_ARGS+=(-o "UserKnownHostsFile=$RUNTIME_DIR/known_hosts" \
                   -o "StrictHostKeyChecking=$STRICT_HOST_KEY_CHECKING")
    log "no known_hosts mounted — using StrictHostKeyChecking=$STRICT_HOST_KEY_CHECKING (pin the host key for production, see compose.ssh.yml)"
fi

# ---------------------------------------------------------------------------
# SSH-over-TLS (gateway demux: ALPN "ssh" routes straight to the SSH gateway)
# ---------------------------------------------------------------------------
TLS_ARGS=()
if [[ "$SSH_OVER_TLS" == "on" ]]; then
    TLS_ARGS+=(-o "ProxyCommand=openssl s_client -quiet -verify_quiet -servername %h -alpn ssh -connect %h:%p")
fi

# ---------------------------------------------------------------------------
# Forward spec per mode
# ---------------------------------------------------------------------------
FORWARD_ARGS=()
case "$TUNNEL_MODE" in
    vhost)
        [[ -n "$VHOST_LABEL" ]] || die "vhost mode requires VHOST_LABEL"
        [[ -n "$LOCAL_PORT"  ]] || die "vhost mode requires LOCAL_PORT"
        FORWARD_ARGS+=(-R "vhost/${VHOST_LABEL}:0:${LOCAL_HOST}:${LOCAL_PORT}")
        ;;
    public)
        [[ -n "$LOCAL_PORT" ]] || die "public mode requires LOCAL_PORT"
        FORWARD_ARGS+=(-R "${PUBLIC_PORT}:${LOCAL_HOST}:${LOCAL_PORT}")
        ;;
    secret-provider)
        [[ -n "$SECRET_ID"  ]] || die "secret-provider mode requires SECRET_ID"
        [[ -n "$LOCAL_PORT" ]] || die "secret-provider mode requires LOCAL_PORT"
        FORWARD_ARGS+=(-R "secret/${SECRET_ID}:0:${LOCAL_HOST}:${LOCAL_PORT}")
        ;;
    secret-consumer)
        [[ -n "$SECRET_ID"   ]] || die "secret-consumer mode requires SECRET_ID"
        [[ -n "$LISTEN_PORT" ]] || die "secret-consumer mode requires LISTEN_PORT"
        # Final port is an ignored nonzero placeholder (OpenSSH -L rejects 0).
        FORWARD_ARGS+=(-L "${LISTEN_ADDRESS}:${LISTEN_PORT}:secret/${SECRET_ID}:1")
        ;;
    raw)
        [[ -n "$FORWARD_SPEC" || -n "$EXTRA_FORWARDS" ]] || die "raw mode requires FORWARD_SPEC and/or EXTRA_FORWARDS"
        if [[ -n "$FORWARD_SPEC" ]]; then
            # shellcheck disable=SC2206
            FORWARD_ARGS+=($FORWARD_SPEC)
        fi
        ;;
    *)
        die "unknown TUNNEL_MODE=$TUNNEL_MODE (expected vhost | public | secret-provider | secret-consumer | raw)"
        ;;
esac

if [[ -n "$EXTRA_FORWARDS" ]]; then
    # Space-separated "-R spec" / "-L spec" tokens; specs contain no spaces.
    # shellcheck disable=SC2206
    FORWARD_ARGS+=($EXTRA_FORWARDS)
fi

EXTRA_OPT_ARGS=()
if [[ -n "$EXTRA_SSH_OPTS" ]]; then
    # shellcheck disable=SC2206
    EXTRA_OPT_ARGS+=($EXTRA_SSH_OPTS)
fi

# ---------------------------------------------------------------------------
# Assemble and exec
# ---------------------------------------------------------------------------
CMD=(autossh -M 0 -T
     -p "$BORE_SSH_PORT"
     -o "ServerAliveInterval=$SERVER_ALIVE_INTERVAL"
     -o "ServerAliveCountMax=$SERVER_ALIVE_COUNT_MAX"
     -o "ConnectTimeout=$CONNECT_TIMEOUT"
     -o "ExitOnForwardFailure=$EXIT_ON_FORWARD_FAILURE"
     -o "TCPKeepAlive=$TCP_KEEPALIVE"
     -o "LogLevel=$SSH_LOG_LEVEL"
     "${HOSTKEY_ARGS[@]}"
     "${TLS_ARGS[@]}"
     "${AUTH_ARGS[@]}"
     "${EXTRA_OPT_ARGS[@]}"
     "${FORWARD_ARGS[@]}"
     "${BORE_SSH_USER}@${BORE_SSH_HOST}")

if [[ -n "$EXEC_PARAMS" ]]; then
    CMD+=(-- "$EXEC_PARAMS")
fi

log "mode=$TUNNEL_MODE target=${BORE_SSH_HOST}:${BORE_SSH_PORT} tls=${SSH_OVER_TLS}"
log "forwards: ${FORWARD_ARGS[*]}"
exec "${CMD[@]}"
