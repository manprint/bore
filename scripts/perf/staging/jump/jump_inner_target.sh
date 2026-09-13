#!/usr/bin/env bash
# The INNER SSH TARGET of the jump-host campaign, and why it is a container.
#
# WHAT THIS EXISTS TO FIX
# -----------------------
# `ssh -J gateway inner` has TWO hops. The campaign measures the OUTER one --
# the gateway is the only part of the path this project writes -- but it can
# only measure it by SUBTRACTION, and subtraction needs the inner hop to exist:
#
#     wchan_ms - tcp_ms   the gateway's own cost      <- the deliverable
#     open_ms  - wchan_ms the inner sshd's handshake  <- must be REAL, or the
#                                                        first term is a guess
#
# This workstation has **no sshd**: `openssh-server` is not installed, port 22
# answers `Connection refused`, and the campaign had been pointing the provider
# at `127.0.0.1:22` for its whole life. Every `open` sample read FAILED and
# every `wchan` sample read a NUMBER -- because `jump_wchan_ms` piped ssh into
# `head -c 1`, and a pipeline that produced zero bytes still exits 0. 1270.3 ms
# was the time it took to fail. That is this campaign's signature defect, "a
# zero that means the instrument failed", wearing a pipeline as a disguise.
#
# WHY A CONTAINER AND NOT `apt install openssh-server`
# ----------------------------------------------------
# Installing a system sshd needs root, and -- more to the point -- it would
# leave a listening sshd on the operator's workstation after a benchmark. The
# standing rule for this campaign is that a stage leaves the machine as it
# found it. A container is removable in one command, publishes ONLY on
# 127.0.0.1, and runs the same OpenSSH the measurement is meant to be about.
#
# WHY LOOPBACK IS THE RIGHT PLACE FOR IT
# ---------------------------------------
# The provider runs on this workstation, so provider -> inner target must not
# cross the WAN: if it did, `open_ms - wchan_ms` would contain a round trip
# that belongs to the topology and not to the sshd, and the subtraction above
# would silently attribute it to the gateway. The alternative topologies with
# only two hosts (inner sshd on the VM) cost three WAN crossings instead of
# one. D6/D7 already record that a third host is what this campaign lacks.
#
# THREE USERNAMES LIVE IN THIS CAMPAIGN AND THEY ARE NOT INTERCHANGEABLE:
#   JUMP_SSH_USER    the gateway PRINCIPAL (outer hop; a name inside bore,
#                    not a Unix account anywhere) -- see jumplib.sh
#   JUMP_INNER_USER  a real account inside THIS container (inner hop)
#   BORE_VM_USER     the VM's own login, for deploying the gateway binary
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

JUMP_INNER_NAME="${JUMP_INNER_NAME:-bore-jump-inner}"
JUMP_INNER_PORT="${JUMP_INNER_PORT:-2222}"
JUMP_INNER_USER="${JUMP_INNER_USER:-bench}"
JUMP_INNER_IMAGE="${JUMP_INNER_IMAGE:-bore-jump-inner:local}"
JUMP_INNER_PUBKEY="${JUMP_INNER_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"

build() {
    # NOT a `trap ... RETURN`: that trap outlives this function's frame and
    # then fires in the CALLER's, where `$ctx` does not exist -- under `set -u`
    # that is an error message on a build that actually succeeded.
    local ctx rc=0
    [ -r "$JUMP_INNER_PUBKEY" ] || { echo "no public key at $JUMP_INNER_PUBKEY" >&2; return 1; }
    ctx="$(mktemp -d)" || return 1
    cp "$JUMP_INNER_PUBKEY" "$ctx/authorized_keys"
    # UseDNS no is not cosmetic: a reverse lookup on the connecting address is
    # charged to `open_ms`, which is the number this stage publishes.
    cat > "$ctx/Dockerfile" <<EOF
# Two alpine-specific facts, both of which cost a debugging round:
#   * \`adduser -D\` leaves the account LOCKED (\`!\` in /etc/shadow) and sshd
#     refuses a locked account even for PUBLIC KEY auth -- it logs
#     "User <u> not allowed because account is locked" and answers the client
#     the generic "Permission denied (publickey)". Unlocking is \`*\`, which is
#     "no password login", not \`\` which would be "no password REQUIRED".
#   * this OpenSSH build has no GSSAPI support at all, so naming the option is
#     a config error ("Unsupported option GSSAPIAuthentication"), not a no-op.
FROM alpine:3.20
RUN apk add --no-cache openssh-server && ssh-keygen -A \\
 && adduser -D -s /bin/sh $JUMP_INNER_USER \\
 && sed -i 's/^$JUMP_INNER_USER:!:/$JUMP_INNER_USER:*:/' /etc/shadow \\
 && mkdir -p /home/$JUMP_INNER_USER/.ssh
COPY authorized_keys /home/$JUMP_INNER_USER/.ssh/authorized_keys
RUN chown -R $JUMP_INNER_USER:$JUMP_INNER_USER /home/$JUMP_INNER_USER/.ssh \\
 && chmod 700 /home/$JUMP_INNER_USER/.ssh \\
 && chmod 600 /home/$JUMP_INNER_USER/.ssh/authorized_keys \\
 && printf '%s\\n' 'PermitRootLogin no' 'PasswordAuthentication no' \\
      'UseDNS no' \\
      'MaxSessions 100' 'MaxStartups 100:30:200' >> /etc/ssh/sshd_config
CMD ["/usr/sbin/sshd","-D","-e"]
EOF
    docker build -q -t "$JUMP_INNER_IMAGE" "$ctx" >/dev/null || rc=1
    rm -rf "$ctx"
    return "$rc"
}

up() {
    docker rm -f "$JUMP_INNER_NAME" >/dev/null 2>&1
    build || { echo "inner target: build failed" >&2; return 1; }
    # 127.0.0.1 ONLY. This is an sshd with a live key on an operator's machine.
    docker run -d --name "$JUMP_INNER_NAME" \
        -p "127.0.0.1:$JUMP_INNER_PORT:22" "$JUMP_INNER_IMAGE" >/dev/null || return 1
    check 30
}

down() { docker rm -f "$JUMP_INNER_NAME" >/dev/null 2>&1; }

# THE PREMISE CHECK. It reads the BANNER, not an exit code -- the defect this
# file documents is precisely a check that an empty stream satisfied.
check() {
    local tries="${1:-1}" i banner
    for i in $(seq 1 "$tries"); do
        banner=$(timeout 5 bash -c "exec 3<>/dev/tcp/127.0.0.1/$JUMP_INNER_PORT; head -c 4 <&3" 2>/dev/null)
        case "$banner" in SSH-*) echo "inner target ready on 127.0.0.1:$JUMP_INNER_PORT ($banner)"; return 0;; esac
        sleep 1
    done
    echo "inner target NOT ready on 127.0.0.1:$JUMP_INNER_PORT (banner='$banner')" >&2
    return 1
}

case "${1:-up}" in
    up) up ;;
    down) down ;;
    check) check "${2:-1}" ;;
    *) echo "usage: $0 {up|down|check}" >&2; exit 2 ;;
esac
