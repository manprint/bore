# SSH Jump Host — Resume

> **Last update:** 2026-08-08
> **Planning model:** GPT-5/Codex

## Status

- Owner requirements refined and locked.
- Preliminary repository/documentation/code-path audit complete.
- Overview and Phases 1–5 written.
- `examples_usage.md` written as the Compose/credential/command and E2E
  acceptance contract for native and pure-OpenSSH providers.
- Planning documents validated for balanced code fences, trailing whitespace,
  required files and port-topology consistency.
- Phases 1–4 implemented and green. Phase 5 has not started.
- `bore sshjhost`, `--ssh-jump-base-domain`, native warm-TCP plus direct-QUIC
  carriers, pure-OpenSSH `jump/` registration and real ProxyJump dispatch are available.
- Phase 3 is commit `b330fd8` (`phase 3`). Phase 4 is implemented in the current
  uncommitted worktree for owner review.

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

## Phase 3 checkpoint

- Added exact per-alias active/relay counters shared by the routing registry and
  its single `Role::SshJumpHost` admin row. The existing per-entry semaphore,
  15 s provider-open bound, carrier failover and RAII cleanup now have focused
  cancellation/failover/counter coverage.
- Hardened collision/reconnect behavior: native remains first-wins; pure SSH
  takeover remains same-username-only; concurrent native reconnect storms cannot
  replace the owner or create extra admin rows.
- Added dedicated token-guarded `/admin/api/v1/ssh-jump`, `SshJumpView`, summary
  and metrics counts, config parity and the **Jump Hosts** SPA panel. The view is
  operational-only and omits usernames, identities, secrets, passwords and keys.
- Added structured `allow`/`deny`/`open`/`close` audit events with peer,
  principal, alias, port, provider type/owner class and selected path. Client
  errors stay generic; repeated username mismatches are logarithmically sampled.
- Updated README, SSH gateway guides, admin architecture/section docs and the
  root invariant ledger for the new behavior.

## Phase 3 gates

Run on 2026-08-08:

- `cargo fmt --all -- --check` — pass.
- `cargo clippy --all-targets --all-features -- -D warnings` — pass.
- `cargo test --all-features admin` — pass: 38 focused tests, 0 failed.
- `cargo test --all-features --test ssh_jump_test` — pass: 4 passed, 0 failed.
- `npm test` — pass: 92 passed, 0 failed.
- `cargo test --all-features -- --test-threads=1` — pass: 896 passed, 0 failed,
  2 existing ignored (898 listed). Serial execution is required because the SSH
  integration cases share process/listener state.
- `cargo test --no-default-features` — pass: 544 tests listed, 0 failed.
- `cargo test --no-default-features --features ssh-gateway -- --test-threads=1`
  — pass: 656 tests listed, 0 failed.
- `cargo build --release --all-features` — pass.
- `sudo -n /abs/path/scripts/ssh_gateway_test.sh` — pass: 16, fail: 0.

## Phase 4 checkpoint

- Reused the one server-direct QUIC endpoint and its existing
  `--vhost-quic-port`; `jump:<alias>` joins bare vhost and `port:<N>` without a
  second bind or new flag. Authentication keys are length-bounded and jump
  nonces are tied to exact registration ids across replacement races.
- Native `sshjhost --udp` opens the clamped `--carriers N` QUIC pool, processes
  each bidi stream through the existing local splice, renews only the shortfall
  with bounded backoff and closes all direct carriers on deregistration.
- Each authorized ProxyJump channel selects one QUIC carrier/stream. Missing,
  closed, timed-out or readiness-failed direct opens increment fallback state and
  use the warm TCP carrier pool for that same channel. Pure OpenSSH stays TCP-only.
- Added live direct carrier/open/fallback state and separate direct bytes to the
  sanitized API/UI; updated README, both gateway guides, frontend docs and root
  invariant ledger.
- Added real OpenSSH E2E for N=2 direct use, carrier loss, TCP fallback, renewal,
  resumed direct use and UDP-disabled startup fallback. Shared endpoint bounds,
  ownership, admin and UI have focused unit tests.

## Phase 4 gates

Run on 2026-08-08:

- `cargo fmt --all -- --check` — pass.
- `cargo clippy --all-targets --all-features -- -D warnings` — pass.
- `cargo test --all-features --test ssh_jump_test -- --test-threads=1` — pass:
  6 passed, 0 failed.
- `cargo test --all-features --test vhost_test -- --test-threads=1` — pass:
  42 passed, 0 failed.
- `cargo test --all-features --test public_udp_test -- --test-threads=1` — pass:
  6 passed, 0 failed.
- `cargo test --all-features --test ssh_gateway_test -- --test-threads=1` —
  pass: 42 passed, 0 failed.
- `cargo test --all-features -- --test-threads=1` — pass: 900 passed, 0 failed,
  2 existing ignored (902 listed). The Compose/STUN collision diagnostic added
  after that run also passes as a focused test: 1 passed, 0 failed.
- `npm test` — pass: 92 passed, 0 failed.
- `cargo check --no-default-features --all-targets` — pass with no warnings.
- `cargo check --no-default-features --features ssh-gateway --all-targets` —
  pass with no warnings.
- `cargo build --release --all-features` — pass.
- `sudo -n /abs/path/scripts/ssh_gateway_test.sh` — pass: 16, fail: 0.
- `git diff --check` — pass.

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

Stop at the green Phase 4 boundary. Await the owner's review/commit and an
explicit instruction before beginning `phase_05.md`; do not begin Phase 5
automatically.
