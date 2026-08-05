# SSH Jump Host — Resume

> **Last update:** 2026-08-05
> **Planning model:** GPT-5/Codex

## Status

- Owner requirements refined and locked.
- Preliminary repository/documentation/code-path audit complete.
- Overview and Phases 1–5 written.
- `examples_usage.md` written as the Compose/credential/command and E2E
  acceptance contract for native and pure-OpenSSH providers.
- Planning documents validated for balanced code fences, trailing whitespace,
  required files and port-topology consistency.
- Phases 1 and 2 implemented and green. Phase 3 has not started.
- `bore sshjhost`, `--ssh-jump-base-domain`, native TCP carriers, pure-OpenSSH
  `jump/` registration and real ProxyJump dispatch are available.
- No commit was created by the implementation agent; the owner will review and
  commit this phase.

## Phase 1 checkpoint

- Added bounded, validated SSH-jump protocol messages before the load-bearing
  final client `Heartbeat` variant, with round-trip, frame-size, redaction and
  old-peer rejection coverage.
- Added internal alias/hostname/port contracts, registration metadata, jump
  registry, namespaced pending-QUIC nonce state and replacement-safe RAII
  teardown scaffolding. All production registration/routing remains disabled.
- Added disabled/sanitized config-view fields. Existing server constructors use
  `None`/`false`, registries start empty and the bore secret is never exposed.
- Added jump-only classic credential metadata:
  - public-key binding comes only from an exact `<user>` or `<user>.pub`
    directory-entry filename;
  - password binding comes only from an exact password-file label;
  - existing username-agnostic SSH gateway Accept/Reject and grant identity
    semantics remain unchanged for every legacy operation;
  - key comments never become jump principals;
  - both stores retain their current per-attempt hot reload behavior.
- Added target-resolution coverage for standard/nonstandard ports and IPv6,
  alias/routing grammar tables, stale-guard ownership tests and handler-level
  separation between legacy grant identity and jump principal.
- README remains unchanged in Phase 1 because no command, flag or usable
  behavior/API is exposed yet, as required by `phase_01.md`.

## Phase 1 gates

Run on 2026-08-05 with all features:

- `cargo fmt --all -- --check` — pass.
- `cargo clippy --all-targets --all-features -- -D warnings` — pass.
- `cargo test --all-features ssh_jump` — pass: 17 focused tests, 0 failed.
- `cargo test --all-features` — pass: 884 passed, 0 failed, 2 ignored
  (the existing root/CAP_NET_ADMIN-only tests).
- Extra compatibility check:
  `cargo test --no-default-features ssh_jump --no-fail-fast` — pass: 10
  focused tests, 0 failed.

## Phase 2 checkpoint

- Added the ungated `bore sshjhost <HOST:PORT>` command with alias validation,
  bore-secret auth, notes, carrier pool/top-up, reconnect, TCP fallback warning
  for the future `--udp` path, and exact ProxyJump connection logging.
- Added `--ssh-jump-base-domain` / `BORE_SSH_JUMP_BASE_DOMAIN`; it normalizes
  the namespace, requires a feature-enabled `--ssh-gateway`, and populates the
  sanitized server config view without exposing secrets.
- Implemented native-provider registration with first-wins aliases, capped
  `CarrierPool`, per-tunnel permits, 20 s client heartbeat, the required
  500 ms-tick/60 s receive reaper and replacement-safe registry/admin teardown.
- Added pure-OpenSSH provider grammar
  `-R jump/<alias>:<port>:host:<port>` in the shared jump registry. Classic
  username binding is checked before parsing; same-username reconnect takeover,
  different-user/native collisions, cancel and multi-forward RAII are covered.
  Numeric `-R 22:localhost:22` remains the existing public forward.
- Added exact jump-FQDN `direct-tcpip` dispatch with generic fail-closed auth,
  exact nonstandard-port matching, bounded provider opens, carrier failover,
  no SSH-side `STREAM_READY`, and one-task half-close-safe splice.
- Added real stock-OpenSSH E2E coverage in `tests/ssh_jump_test.rs`: native and
  pure providers, provider gateway auth by key and password, inner target key
  and password auth, two forwards in one session, takeover/collisions, wrong
  username/port, carrier pool, liveness reaper and alias reclaim.
- Updated README as the operational source of truth, `docs/SSH_GATEWAY.md`, and
  `examples_usage.md`: Compose adds only
  `BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.0912345.xyz`; no 8443 mapping is needed;
  Phase 2 is explicitly TCP-only while existing `443/udp` remains unchanged.

## Phase 2 gates

Run on 2026-08-05 with all features:

- `cargo fmt --all -- --check` — pass.
- `cargo clippy --all-targets --all-features -- -D warnings` — pass.
- `cargo test --all-features --test ssh_jump_test` — pass: 4 passed, 0 failed.
- `cargo test --all-features --test ssh_gateway_test` — pass: 42 passed, 0 failed.
- `cargo test --all-features --test secret_test` — pass: 17 passed, 0 failed.
- `cargo test --all-features` — pass: 893 passed, 0 failed, 2 ignored
  (existing root/CAP_NET_ADMIN-only tests).
- Extra small-client compatibility check:
  `cargo check --no-default-features --all-targets` — pass with no warnings.

## Locked configuration

- SSH gateway: TCP 443 through OpenSSH config alias.
- Jump namespace: `<label>.ssh.bore.tld` (separate from HTTP vhost).
- Access: no separate ACL. Only jump publish/connect requires username-bound
  public-key/password authentication; all existing gateway modes keep today's
  username-ignored behavior.
- Provider auth: existing bore shared secret over TLS control transport.
- Provider modes: native `bore sshjhost` uses that secret and may use QUIC;
  pure OpenSSH `-R jump/...` uses classic username-bound gateway auth and is
  TCP-only. Both share one registry.
- UDP scope: QUIC only on server→provider; warm TCP fallback retained.
- Ports in the owner's real Compose: public SSH/control 443/tcp maps to internal
  7835; STUN remains 7835/udp; sshjhost reuses the existing vhost/public direct
  endpoint on 443/udp. No 8443 mapping or new QUIC-port variable is needed.
- Compose delta: add only
  `BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.0912345.xyz`; every current port, vhost,
  VPN, SSH-gateway and volume setting remains unchanged.
- Ports: standard and nonstandard supported; virtual port equals TARGET port in v1.

## Next

Stop at the green Phase 2 boundary. Await the owner's review/commit and an
explicit instruction before beginning `phase_03.md`; do not begin Phase 3
automatically.
