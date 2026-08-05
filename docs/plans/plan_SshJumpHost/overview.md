# SSH Jump Host — Plan Overview

> **Status:** decisions locked; implementation complete through Phase 2 on 2026-08-05
> **Folder:** `docs/plans/plan_SshJumpHost/`
> **Planning model:** GPT-5/Codex
> **Implementation status:** Phase 2 green; Phase 3 not started

## Goal

Publish an SSH daemon behind NAT under a virtual hostname consumed by stock
OpenSSH `ProxyJump`. The provider may use either the native bore client or stock
OpenSSH:

```bash
# VM/provider
BORE_SERVER=https://bore.tld BORE_SECRET=... \
  bore sshjhost localhost:22 \
    --subdomain vm-test-01 \
    --notes "vm test AWS su zona eu-south-1" \
    --auto-reconnect --udp

# Operator (`Host bore.tld` in SSH config pins port 443)
ssh -J bore.tld ubuntu@vm-test-01.ssh.bore.tld

# VM/provider without a bore binary (TCP relay only)
ssh -T -p 443 \
  -R jump/vm-test-01:22:localhost:22 \
  vm-provider@bore.tld -- 'notes="vm test AWS su zona eu-south-1"'
```

The public SSH gateway remains on TCP 443. The client→gateway leg is necessarily
SSH/TCP. `--udp` upgrades only the bore server→VM/provider leg to QUIC, with a
warm TCP carrier fallback per connection. The final SSH handshake remains
end-to-end between the operator's OpenSSH client and the VM's `sshd`.

## Locked owner decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D-JH1** | SSH gateway stays on TCP 443. | A `~/.ssh/config` alias is the canonical UX; without it use `-J bore.tld:443`. This does not require QUIC to share UDP 443. |
| **D-JH2** | Jump targets use a separate namespace. | Server config uses `--ssh-jump-base-domain ssh.bore.tld`; targets are `<label>.ssh.bore.tld`, separate from HTTP vhosts. |
| **D-JH3** | `sshjhost` alone uses classic username-bound SSH authentication inside the SSH gateway; every existing SSH-gateway mode keeps today's username-ignored behavior. | OpenSSH jump publish/connect requires the presented username to match the account owning the successful key/password. Native provider registration has no SSH username and follows D-JH4. No separate ACL or per-host policy file exists. |
| **D-JH4** | Native provider registration uses the existing bore `--secret`; a pure-OpenSSH provider uses normal gateway key/password authentication. | No new provider PKI in v1. All holders of the shared bore secret can claim any free alias through the native path; every classic-bound gateway account can publish/connect through the SSH path. |
| **D-JH5** | QUIC is required only on server→VM. | Reuse the already-shared public/vhost endpoint configured by `--vhost-quic-port` (`443/udp` in the real Compose): no new UDP port, no STUN/hole-punch, provider dials server, TCP stays warm. |
| **D-JH6** | Non-standard SSH ports are supported. | The registered public/virtual SSH port equals the TARGET port in v1. `direct-tcpip` must request that exact port. No port remapping flag in v1. |

## Architecture

```text
OpenSSH client
  │ outer SSH/TCP 443; gateway host-key + gateway user auth
  ▼
bore server / russh gateway
  │ direct-tcpip("vm-test-01.ssh.bore.tld", 22)
  │ normalize hostname → alias; require classic principal; lookup registry
  │ try QUIC bidi stream → on failure open warm TCP/yamux carrier
  ▼
bore sshjhost provider (native mode)
  │ connect localhost:22
  ▼
target sshd
  │ inner SSH host-key + target account auth (opaque to bore)
```

The pure-OpenSSH provider replaces the native-provider leg with a reverse SSH
forward registered as `jump/<alias>:<target-port>`. Its outer SSH session is the
provider transport, so it uses TCP only, has no bore secret, no QUIC and no
`--carriers`. Both provider types install the same logical entry in one jump
registry and are consumed by the same `direct-tcpip` hostname dispatch.

The new provider has its own registry and protocol type. It must not masquerade
as a vhost (which would expose an HTTP route) or as a secret provider (whose UDP
path is peer-to-peer and requires a native consumer). It reuses their primitives:

- `CarrierPool` / `LinkOpener` for TCP carriers and failover;
- `DirectPool` and the unified server QUIC endpoint for server-direct streams;
- `Client::handle_connection` for `STREAM_READY` + local TCP splice;
- secret-provider heartbeat/reaper discipline for zombie-free registration;
- SSH gateway `direct-tcpip` dispatch and originator metadata;
- `copy_bidirectional_with_sizes` for half-close-safe, single-task splice.

## CLI contract

### Provider

```text
bore sshjhost <TARGET> --subdomain <LABEL> [OPTIONS]
```

`TARGET` accepts the existing `HOST:PORT` / `[IPv6]:PORT` syntax. The port is
both the provider's local target port and the virtual SSH destination port.

Initial option set (reuse current meanings/env names where possible):

- `--subdomain LABEL` / `BORE_SSH_JUMP_SUBDOMAIN` (required);
- `--to ADDR` / `BORE_SERVER`;
- `--secret` / `BORE_SECRET`;
- `--insecure`;
- `--notes` / `BORE_NOTES`;
- `--carriers N` / `BORE_CARRIERS` (default 1);
- `--udp` / `BORE_PREFER_UDP`;
- `--auto-reconnect` / `BORE_AUTO_RECONNECT`.

Secret P2P-only flags (`--stun-server`, `--upnp`, prediction/manual-candidate
flags) are deliberately absent: jump QUIC dials the public server directly.

Example for non-standard SSH:

```bash
bore sshjhost localhost:2222 --subdomain legacy-01 --auto-reconnect
ssh -p 2222 -J bore.tld admin@legacy-01.ssh.bore.tld
```

### Pure-OpenSSH provider

The zero-install provider grammar extends the existing SSH-gateway remote
forward namespace:

```text
ssh -T -p 443 \
  -R jump/<LABEL>:<TARGET_PORT>:<TARGET_HOST>:<TARGET_PORT> \
  <PROVIDER_USER>@bore.tld -- 'notes="..."'
```

For example:

```bash
ssh -T -p 443 \
  -R jump/vm-test-01:22:localhost:22 \
  vm-provider@bore.tld -- 'notes="vm test AWS su zona eu-south-1"'
```

`jump/` is mandatory: the superficially similar
`ssh -R 22:localhost:22 bore.tld` is, and remains, the existing anonymous public
TCP-forward syntax. It opens/requests public port 22 and does not create a named
ProxyJump target. The pure-SSH provider authenticates with its username-bound
gateway key/password and stays TCP-only; `udp=on` and `carriers=` are rejected or
reported as inapplicable, never silently claimed as active.

### Server

New flags:

- `--ssh-jump-base-domain ssh.bore.tld` / `BORE_SSH_JUMP_BASE_DOMAIN`.

No new QUIC-port flag is introduced. The endpoint currently named
`--vhost-quic-port` / `BORE_VHOST_QUIC_PORT` is already shared by vhost and
public direct paths; jump adds the collision-proof `jump:<alias>` namespace to
that same accept loop. Renaming this established option is outside the feature
scope and would create unnecessary deployment churn.

Enabling `--ssh-jump-base-domain` requires `--ssh-gateway`; startup fails fast
otherwise. The gateway's existing requirement for at least one credential source
(`--ssh-authorized-keys-dir` and/or `--ssh-passwords-file`) remains unchanged.
The server must be built with `ssh-gateway`; the provider command itself stays
available without the local `ssh-gateway` feature so a small client binary can
reach a fully-featured remote server.

## Hostname routing contract

- Store only the single ASCII DNS label (`[a-z0-9-]`, max 63, no leading or
  trailing hyphen) in the registry.
- Normalize the OpenSSH destination to lowercase and strip one terminal dot.
- Accept exactly `<label>.<configured-base-domain>`; reject nested labels,
  suffix confusion (`evilssh.bore.tld`) and other domains.
- If the host does not match the jump suffix, run the existing secret-consumer
  parser unchanged. This preserves all current `ssh -L` behavior.
- Require `port_to_connect == entry.ssh_port`; reject a mismatched port before
  accepting the channel.
- `*.ssh.bore.tld` does not need public DNS for OpenSSH ProxyJump: the hostname
  is carried in `direct-tcpip`. Only `bore.tld` must resolve. Document that a
  wildcard DNS record is optional and does not enable direct SSH routing.

## Classic authentication contract (scoped only to `sshjhost`)

The existing gateway authentication callbacks remain byte/behavior-compatible:
they may still accept a valid key or password regardless of the username supplied
to OpenSSH. Alongside that legacy result, authentication records an optional
`jump_principal` used only by the new SSH-gateway jump publish/connect paths.
The native `bore sshjhost` provider does not traverse the SSH gateway and is
authenticated solely by the existing bore secret.

Public-key binding:

- files remain standard `authorized_keys` files in the existing directory;
- for jump use, the filename stem is the account name:
  `/etc/bore/ssh/authorized_keys.d/fabio` or `fabio.pub` binds every key in that
  file to SSH username `fabio`;
- a key may still live in an arbitrary legacy file and authenticate existing
  vhost/public/secret modes exactly as today, but it has no jump principal unless
  a matching username-named file grants it;
- comments/fingerprints remain the legacy identity; jump ownership/audit uses the
  matched username. Multiple keys in the same user file represent one account.

Password binding:

- the existing line format stays unchanged: `fabio:$argon2id$...`;
- existing modes still accept the password using today's label scan, regardless
  of the presented username;
- jump use additionally requires presented username `fabio` and verification of
  the hash on the `fabio:` line.

If legacy authentication succeeds but classic binding does not, the SSH session
is not disconnected: existing forwards remain usable, while a jump publish/open
gets a generic rejection before alias existence is disclosed. This is the seam
that satisfies “classic only for sshjhost” without a regression elsewhere.

There is deliberately no alias ACL. Every successfully bound jump account may
publish through pure OpenSSH under any free alias and connect to any registered
jump host/port. Existing
per-key `permit=` behavior remains unchanged for existing modes and is not
silently repurposed as a jump ACL.

## Authentication and key placement

Three independent trust layers remain explicit:

1. **Native provider→bore server:** existing HMAC challenge using `BORE_SECRET`,
   over `https://bore.tld` in production. No SSH key is stored by
   `bore sshjhost`.
2. **Pure-SSH provider/operator→jump gateway:** gateway host key persisted at
   `/etc/bore/ssh/host_key.pem`; public keys live in username-named files under
   `/etc/bore/ssh/authorized_keys.d/`, or Argon2id password hashes use a matching
   username label in the existing passwords file. Private keys stay on the VM or
   operator host that owns them.
3. **Operator→target sshd:** target host keys remain `/etc/ssh/ssh_host_*`; target
   account public keys remain `~/.ssh/authorized_keys`. Target private keys and
   passwords stay on the operator/target and are never available to bore.

ProxyJump does not require `ForwardAgent`; documentation must recommend leaving
agent forwarding off. Bore terminates the outer SSH layer but carries an inner,
independently encrypted SSH handshake, so it can observe alias/port/timing/byte
counts but not the target password or inner SSH plaintext.

## Wire/data structures

Add new client messages **before the load-bearing final `ClientMessage::Heartbeat`**:

- `HelloSshJump { alias, ssh_port, notes, carriers, udp, auto_reconnect,
  local_host, local_port }`;
- `SshJumpUdpRenew { alias }`.

Add server messages:

- `SshJumpReady { hostname, port }`;
- `SshJumpUdp { port, nonce, tuning }`.

New server entry (indicative):

```text
SshJumpEntry {
  provider: Native { pool: Arc<CarrierPool>, direct: DirectPool }
          | Ssh { opener: SshOpener, owner_username },
  ssh_port: u16,
  permits: Arc<Semaphore>,
  peer/notes/carriers/udp/auto_reconnect/local target metadata,
  active/relay/direct counters
}
```

QUIC auth keys use the collision-proof namespace `jump:<alias>` alongside
existing bare vhost labels and `port:<N>` public keys. One server QUIC endpoint
serves all three namespaces. The token remains derived from the control-session
nonce plus the existing bore secret.

## Compatibility and lifecycle invariants

- **I-JH1:** no jump configuration ⇒ existing public/secret/vhost/SSH gateway
  paths are byte-identical; unmatched `direct-tcpip` destinations take the old
  secret path unchanged.
- **I-JH2:** one live alias = exactly one provider/admin row, independent of TCP
  carriers or QUIC carrier count.
- **I-JH3:** client sends `HelloSshJump` before auth (yamux lazy invariant).
- **I-JH4:** a native provider sends `ClientMessage::Heartbeat` every 20 s;
  server checks `last_recv` on the 500 ms heartbeat tick and reaps at 60 s.
  Never implement this as `timeout(recv)`. Pure-SSH provider teardown stays
  owned by the existing SSH-session/forward RAII lifecycle.
- **I-JH5:** jump classic binding is required before an SSH-gateway registry
  existence is disclosed or a data stream is opened. Missing/mismatched username
  binding is fail-closed for jump only and never changes existing forward
  authorization. Native registration remains bore-secret authenticated.
- **I-JH6:** `STREAM_READY` is consumed only by the native provider link; it is
  never written onto the OpenSSH channel.
- **I-JH7:** every provider `mux::Stream` stays in one task. Never split it across
  spawned tasks; preserve half-close with `copy_bidirectional_with_sizes`.
- **I-JH8:** native `--udp` never gates tunnel liveness. TCP carrier(s) stay
  warm; direct open failure falls back per connection. Pure OpenSSH is explicitly
  TCP-only.
- **I-JH9:** direct pool chooses one QUIC connection per proxied SSH connection;
  no intra-stream striping.
- **I-JH10:** duplicate native registrations are first-wins/reject because the
  shared bore secret cannot prove same-provider identity. Pure-SSH jump providers
  use the classic username as owner: same-username reconnect may take over;
  different username or native-vs-SSH collision rejects. Existing takeover
  identity semantics for all other modes remain unchanged.
- **I-JH11:** server and provider enforce bounded alias length, exact suffix,
  nonzero port, carrier caps, per-tunnel max-conns, and bounded open timeout.
- **I-JH12:** gateway port 443 is represented in operator SSH config; generated
  commands/banners never imply default port 22.

## Deployment target

The canonical deployment is the real Compose topology: control remains 7835 in
the container, TCP 443 is published to it, STUN stays on UDP 7835, and the
existing direct QUIC endpoint stays on UDP 443:

```yaml
ports:
  - "443:7835"       # SSH/TLS/native bore over TCP
  - "7835:7835/udp"  # existing STUN responder
  - "443:443/udp"    # existing shared vhost/public/jump direct QUIC
environment:
  - BORE_CONTROL_PORT=7835
  - BORE_UDP=true
  - BORE_VHOST_QUIC_PORT=443
  - BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.tld
```

TCP 443 and UDP 443 are independent sockets and may use different container
targets. There is no conflict because the STUN responder binds container
`7835/udp`, not `443/udp`. No public port is opened per VM. A bare-binary server
that instead sets the actual `--control-port 443` would also bind STUN on
`443/udp` and could not bind direct QUIC there simultaneously; that is a
different topology and would need a distinct existing `--vhost-quic-port`.
The exact real-Compose delta and operational examples are the acceptance
contract in [examples_usage.md](examples_usage.md).

Canonical operator config:

```sshconfig
Host bore.tld
    HostName bore.tld
    Port 443
    User tunnel
    IdentityFile ~/.ssh/id_ed25519_bore_gateway
    IdentitiesOnly yes
    StrictHostKeyChecking yes
    ServerAliveInterval 15
    ServerAliveCountMax 3

Host *.ssh.bore.tld
    ProxyJump bore.tld
    IdentityFile ~/.ssh/id_ed25519_vm
    IdentitiesOnly yes
```

## Phases

| Phase | File | Outcome |
|-------|------|---------|
| 1 — Contracts, protocol and classic-auth binding | [phase_01.md](phase_01.md) | **Complete/green.** Internal additive scaffolding and username/credential binding tests |
| 2 — Complete TCP path | [phase_02.md](phase_02.md) | **Complete/green.** Native or pure-OpenSSH provider → real `ssh -J` over TCP, lifecycle-safe |
| 3 — Hardening, observability and admin | [phase_03.md](phase_03.md) | Limits, reconnect/chaos coverage, dedicated dashboard/API surface |
| 4 — Server→provider QUIC + fallback | [phase_04.md](phase_04.md) | Direct pool, warm TCP fallback, churn/recovery tests |
| 5 — Full regression, deployment docs and acceptance | [phase_05.md](phase_05.md) | README source-of-truth, netns/e2e matrix, release-ready evidence |

## Risk register

| Risk | Mitigation |
|------|------------|
| ProxyJump hostname parsing accidentally captures legacy secret destinations. | Match exact configured suffix first; unmatched input executes the existing parser untouched; table-driven regression matrix. |
| Jump-only username checks regress existing gateway behavior. | Keep legacy auth result and jump principal as separate fields; exhaustive mismatch tests prove old vhost/public/secret modes still accept exactly as before while jump rejects. |
| No ACL means one gateway account can reach/publish every jump alias. | Explicit, documented trust model; operators provision gateway accounts only to trusted users/VMs. Native providers are separately trusted through `BORE_SECRET`. Future optional restrictions are out of scope and must not be smuggled into v1. |
| Shared `BORE_SECRET` permits provider alias squatting. | Explicitly documented v1 trust boundary; first-wins duplicate reject; future per-provider credentials remain out of scope. |
| Direct QUIC classifier collides with vhost/public. | Namespaced `jump:` key and worst-case auth-frame bound test. |
| Gateway dispatch loop blocks on provider open. | Validate/accept quickly, perform bounded open/splice in a spawned per-channel task, matching existing secret consumer discipline. |
| QUIC failure silently kills SSH sessions. | Warm relay, per-open fallback, counters/logs, forced-close e2e while an SSH connection is active. |
| A deployment confuses external TCP 443 with the control UDP port. | Canonical Compose explicitly proves the sockets: `443/tcp→7835`, `7835/udp` STUN, existing `443/udp` direct QUIC. Add `jump:` to the current endpoint; never bind a second listener. A bare process with actual control port 443 must keep direct QUIC on a different UDP port. |
| Inner and outer SSH credentials are confused operationally. | Three-layer key-placement table and separate `Host` blocks in README. |

## Implementer protocol

1. Execute phases and sub-phases in order; tests first or alongside each unit.
2. At the end of every phase run fmt, Clippy `-D warnings`, the phase subset,
   and the full relevant regression suite. Zero existing failures are allowed.
3. Update README and matching SSH/UDP/admin docs in the same phase as behavior.
4. Preserve unrelated worktree changes. Never run two netns harnesses together.
5. Update `resume.md` after each sub-phase.
6. Stop after each completed phase and request owner approval before the next.

Phase 1 deliberately exposes no incomplete CLI/flag surface; user-visible command
and server configuration land together with the functional TCP path in Phase 2.
