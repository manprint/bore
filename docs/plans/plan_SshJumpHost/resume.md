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
- Phase 1 implemented and green. Phase 2 has not started.
- No `sshjhost` command or server flag is exposed yet; a Phase 1 regression
  test pins that intentional boundary.
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

Stop at the green Phase 1 boundary. Await the owner's review/commit and an
explicit instruction before beginning `phase_02.md`; do not begin Phase 2
automatically.
