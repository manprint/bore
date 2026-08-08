# Phase 4 — QUIC on server→provider with warm TCP fallback

> **Planning model:** GPT-5/Codex
> **Intent:** implement `bore sshjhost --udp` without changing OpenSSH or making
> UDP a liveness dependency. Pure-OpenSSH providers remain TCP-only.
> **Precondition:** hardened TCP path and admin surface green; owner approval.
> **Completion:** implemented and green on 2026-08-08.

## 4.1 Extend the existing shared server-direct endpoint

- Keep `--vhost-quic-port` / `BORE_VHOST_QUIC_PORT` unchanged. Despite the
  historical name, its one endpoint already serves both bare vhost keys and
  `port:<N>` public keys whenever `bore server --udp`; do not add or rename a
  port option as part of `sshjhost`.
- Extend that same handshake lookup/classification with collision-free
  `jump:<alias>`; retain bare vhost and `port:<N>` behavior unchanged.
- Install a successful `jump:` connection into the matching jump entry's
  `DirectPool`. Do not bind a second UDP socket or fork the accept loop.
- Preserve current bind/error behavior. Add a configuration test for the real
  Compose topology: control/STUN 7835, shared direct endpoint 443. Also retain a
  diagnostic test for the different bare-binary topology where actual
  `control_port == vhost_quic_port` and STUN already owns that UDP socket.
- Add worst-case auth-key/frame tests and ensure no attacker-controlled key can
  allocate unbounded memory.

## 4.2 Provider direct carriers

- When jump provider requested UDP, issue `SshJumpUdp { port, nonce, tuning }`
  only after authenticated TCP registration succeeds.
- Reuse endpoint resolution, token derivation, `vhost_connect`/generalized
  `direct_connect`, `spawn_direct`, carrier cap and renewal backoff.
- Provider opens N independent QUIC connections (clamped to direct-pool cap),
  accepts bidi streams, and feeds each through `Client::handle_connection`.
- Add `SshJumpUdpRenew` for carrier loss; renew only the shortfall and prevent
  open/close churn above the cap.

## 4.3 Gateway selection and fallback

- For each authorized channel, pick one live direct connection and open one bidi
  stream. Write native `STREAM_READY`, then splice the SSH channel and provider
  stream in one task.
- If no direct connection exists or open fails, immediately use the warm TCP
  carrier pool for that same inbound channel. Do not fail the SSH session solely
  because QUIC is down.
- Round-robin per proxied SSH connection across QUIC connections; never stripe a
  single stream or datagrams across carriers.
- Increment direct-open/fallback counters and surface current direct carrier
  count in admin/logs.

## 4.4 QUIC/fallback tests

- Red-check that disabling the jump direct selection makes the direct-use counter
  remain zero and fails the direct-specific assertion.
- E2E cases:
  - UDP available → actual `ssh -J` traffic increments direct stream opens;
  - UDP blocked at startup → TCP works immediately;
  - QUIC carrier closed between pick/open → same connection falls back to TCP;
  - all QUIC carriers closed during an established environment → new sessions
    use TCP, renew restores future direct sessions;
  - `--carriers N` yields N independent QUIC conns, but one SSH session stays on
    one bidi stream;
  - UDP socket buffer clamp warns without breaking relay;
  - real Compose reuses 443/udp and never attempts an 8443/second bind;
  - simultaneous vhost, public and jump direct providers authenticate into their
    own pools through the same UDP 443 endpoint; no namespace can steal another;
  - a real same-socket control/STUN collision remains diagnosed;
  - vhost/public direct-path regressions remain green.
- Pure-OpenSSH provider entries never advertise direct carriers and continue to
  serve over their SSH reverse-forward path while native QUIC tests run.

## Phase 4 gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --test ssh_jump_test
cargo test --all-features --test vhost_test
cargo test --all-features --test public_udp_test
cargo test --all-features --test ssh_gateway_test
cargo test --all-features
```

Phase done means QUIC usage is proven by counters/logs, every forced failure falls
back to warm TCP, and vhost/public direct paths are unchanged.

## Completion checkpoint — 2026-08-08

- Shared endpoint now authenticates bounded `jump:<alias>` keys and binds each
  nonce to the exact registration id; stale handshakes cannot enter a replacement
  entry. Bare-vhost and `port:<N>` routing remain unchanged.
- Native providers receive `SshJumpUdp` only after authenticated registration,
  open a capped N-connection QUIC pool, accept native bidi streams through the
  existing `Client::handle_connection`, and renew only lost capacity.
- Gateway selection writes `STREAM_READY`, counts direct opens/bytes, and falls
  back on the same authorized channel to warm TCP when no direct carrier exists
  or open/readiness fails. One task retains half-close semantics.
- Deregistration closes every direct carrier, removes its owned nonce and keeps
  replacement cleanup race-safe. Pure-OpenSSH providers remain TCP-only.
- Admin/API/UI expose live direct carrier count, opens, fallbacks and separate
  direct/relay bytes without usernames or credential material.
- Real OpenSSH E2E proves two QUIC carriers, direct traffic, forced all-carrier
  failure, immediate TCP fallback, renewal, resumed direct traffic and server-UDP
  disabled fallback. Vhost/public direct suites remain green.
