# Phase 5 — Full regression, documentation and production acceptance

> **Planning model:** GPT-5/Codex
> **Intent:** prove the complete feature and make README the operational source
> of truth.
> **Precondition:** Phases 1–4 complete and owner approval.

## 5.1 README and focused documentation

Update `README.md` in the same change with:

- command overview and complete `bore sshjhost` flag/env reference;
- server flag, feature/build requirements and reuse of the existing
  `BORE_VHOST_QUIC_PORT` endpoint without a new port/alias;
- full deployment commands for gateway-on-443 TCP and UDP;
- firewall/socket table for canonical Compose (`443/tcp` published to internal
  7835, `7835/udp` STUN, existing `443/udp` shared direct QUIC), plus the caveat
  that a bare process whose actual control port is 443 cannot also bind direct
  QUIC to the same UDP socket;
- `~/.ssh/config` blocks for `bore.tld` on port 443 and `*.ssh.bore.tld`,
  preserving the requested `ssh -J bore.tld ...` UX;
- standard and nonstandard target-port examples;
- three independent authentication layers and exact key locations;
- jump-only username binding for public keys/passwords, plus proof that every
  existing gateway mode still ignores username exactly as before;
- warning that agent forwarding is unnecessary and discouraged;
- DNS behavior, `known_hosts` entries and target host-key rotation;
- shared-secret provider trust boundary/alias-squatting caveat;
- TCP/QUIC topology, proof/observability and fallback troubleshooting;
- systemd provider service using environment/credential files, never secrets in
  process arguments.
- both provider modes from `examples_usage.md`: native bore (TCP/QUIC) and pure
  OpenSSH `-R jump/...` (TCP-only), including key/password operation.

Update focused docs: `docs/SSH_GATEWAY.md`, `docs/README-SSH-GATEWAY.md`, UDP/direct
transport docs, admin architecture/sections and `CLAUDE.md` invariants. Add a
short standalone operator guide only if README becomes unwieldy; README remains
the single source of truth.

## 5.2 Acceptance matrix

Run serially:

1. TCP alias port 22, public-key gateway auth, public-key target auth.
2. TCP nonstandard target port, password gateway identity (Argon2 file), target
   auth independent.
3. Public key/password with matching username succeeds; the same credential with
   a mismatched username fails only for jump while a legacy forward still works.
4. Multiple concurrent operators to one host and multiple hosts/providers.
5. Carrier count 1 and >1; carrier death between pick/open.
6. UDP direct active, UDP blocked fallback, live direct loss, renewal.
7. Provider kill/half-open/server restart/reconnect; zero zombie rows and permits.
8. Existing secret `ssh -L`, vhost/public SSH ingress, native public/secret/vhost
   TCP+UDP, transfer and VPN regression suites.
9. Execute the command scenarios identified in
   [examples_usage.md](examples_usage.md); keep their IDs stable so this file can
   be promoted into the e2e harness without rewriting the behavior contract.

Never run network-namespace harnesses concurrently. Rebuild the relevant release
binary before sudo harnesses; use the exact sudoers-approved script path.

## 5.3 Final gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo test --no-default-features --features ssh-gateway
cargo build --release --all-features
```

Then run the existing SSH gateway and relevant UDP/netns scripts serially plus
the new jump-host scenario. Record exact PASS/FAIL totals and environment in
`resume.md`; no ignored failure counts as acceptance.

## Definition of done

- Both exact provider commands register a namespaced jump target with notes:
  native `bore sshjhost` and pure OpenSSH `-R jump/...`.
- `ssh -J bore.tld user@alias.ssh.bore.tld` works with gateway on 443 through
  the documented SSH config.
- Nonstandard ports work and mismatches are denied.
- Jump publish/connect requires classic username binding for both gateway auth
  types; no separate ACL exists and all bound jump accounts have full jump access.
- Target private keys/passwords never reside on or become visible to bore.
- QUIC is proven on server→provider and transparently falls back to warm TCP.
- One alias produces one accurate admin row; no zombie rows or leaked permits.
- README covers build, deploy, modes, every flag/env, auth, firewall, examples and
  troubleshooting.
- All internal, unit, integration, e2e and full regression gates are green.

## Completion checkpoint — 2026-08-08

- Added production netns gate `T-SSH-JUMP` to the existing SSH-gateway harness.
  It uses stock OpenSSH `ssh -W` (the `direct-tcpip` transport primitive behind
  ProxyJump) and proves UDP-blocked warm-TCP fallback, N=2 QUIC renewal/direct
  use, simultaneous bare-vhost/`port:`/`jump:` direct pools on one UDP endpoint,
  jump-only username binding with legacy public-forward compatibility, and pure-
  OpenSSH exact nonstandard-port/TCP-only behavior.
- The six real-OpenSSH Rust E2E cases cover native key/password target auth and
  rejection paths, native direct loss/fallback/renewal, UDP-disabled startup,
  pure provider key/password operation, cancellation and native liveness reap.
- README is the complete operational source for source builds, canonical
  443/TCP+UDP Compose/firewall topology, both provider modes, all flags/env,
  three independent authentication layers, DNS/`known_hosts` rotation, agent
  forwarding policy, systemd credentials, trust boundary, observability and
  troubleshooting. Focused SSH, direct-transport, NAT and admin docs agree.
- Full all-feature and both no-default matrices, frontend tests, release build,
  SSH gateway and four relevant UDP/netns harnesses are green. Exact environment
  and PASS/FAIL totals are recorded in `resume.md`.
