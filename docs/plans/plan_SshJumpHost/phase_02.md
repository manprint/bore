# Phase 2 — Complete TCP `ssh -J` data path

> **Planning model:** GPT-5/Codex
> **Intent:** deliver a production-safe TCP-only vertical slice from either a
> native or pure-OpenSSH provider through an actual OpenSSH ProxyJump session.
> **Precondition:** Phase 1 gates green and owner approval.

## 2.1 Expose CLI/server configuration and provider registration

- Add ungated `Command::SshJHost` in `src/main.rs`, reusing
  `resolve_local_target`, `clamp_notes`, endpoint parsing and reconnect options.
- Add server flag `--ssh-jump-base-domain`; fail fast unless it is used with
  `--ssh-gateway`. The existing gateway rule already requires at least one key
  directory/password file. Populate the sanitized config view from Phase 1.
- Add Clap tests for the exact standard/nonstandard commands, env parity,
  invalid aliases and feature combinations. The command must compile without
  the local `ssh-gateway` feature; only the remote server needs russh support.
- Implement `Client::new_ssh_jump_provider` using the existing control transport:
  open yamux, send `HelloSshJump` before auth, perform existing HMAC auth, receive
  `SshJumpReady`, then establish requested extra carriers through the existing
  `CarrierToken`/redial machinery.
- Route accepted data substreams through the same generic
  `Client::handle_connection` used by public/vhost so `STREAM_READY`, TCP tuning,
  Basic transport errors and half-close behavior stay shared.
- Provider logs the exact hostname, port and copyable command. Because gateway
  is on 443, the command must rely on a documented `Host bore.tld` config or use explicit
  `bore.tld:443`; never print a misleading port-22 command.
- Update README command overview, complete TCP flags/env reference and a minimal
  working 443 deployment in this same functional phase.

## 2.2 Server registration, liveness and teardown

- Add a dedicated `serve_ssh_jump_provider` branch in server routing; reject when
  SSH jump service is not configured or the server lacks SSH gateway support.
- Atomically insert one alias entry; duplicates are first-wins/reject.
- Seed `CarrierPool`, cap carriers by `--max-carriers`, support leak-free joined
  carriers and carrier top-up exactly like current provider pools.
- Provider sends heartbeat every 20 s. Server tracks `last_recv` and reaps at
  60 s on the 500 ms heartbeat tick. Do not use `timeout(recv)`.
- Deregistration guard removes registry/pending state only if it still owns the
  installed token; no stale guard may delete a newer registration.

## 2.3 Pure-OpenSSH provider registration

- Extend `ForwardSpec` with the explicit and unambiguous grammar
  `jump/<alias>:<ssh-port>` for `tcpip-forward`; never reinterpret the existing
  numeric `-R <port>:host:port` public mode or bare-label heuristics.
- Require `jump_principal` before parsing/registry errors are exposed. A legacy
  accepted-but-username-mismatched session may still use all existing forward
  modes, but its jump remote forward is generically rejected.
- Register the reverse SSH channel as an `SshOpener` variant in the same
  `SshJumpRegistry` used by native providers. Its owner is the exact,
  case-sensitive classic username; same-username reconnect may take over,
  different-username and cross-native collisions reject.
- Reuse the existing connection/session RAII so disconnect/cancel removes only
  entries still owned by that forward. Do not add a second heartbeat or a second
  registry for this transport.
- Accept `notes=` through the existing exec parameter channel. Report
  `udp=`, `carriers=` and native-only parameters as inapplicable for this mode;
  pure OpenSSH remains TCP-only.
- Add parser, classic-binding, ownership/takeover, collision, cancel and
  multi-forward tests. Prove `-R 22:localhost:22` remains the current public
  forward and is not a jump registration.

## 2.4 Gateway FQDN dispatch, classic auth and TCP splice

- In `channel_open_direct_tcpip`:
  1. classify exact jump FQDN without changing unmatched legacy secret behavior;
  2. normalize alias/port;
  3. require `jump_principal` before registry lookup or existence-specific errors;
  4. verify the requested port equals the registered SSH port;
  5. acquire the per-tunnel connection permit;
  6. accept/spawn a bounded provider open and one-task bidirectional splice.
- Native entries use `open_with_failover` semantics across live TCP carriers. A
  carrier dying between pick and open must retry up to pool size. Pure-SSH
  entries call the existing `SshOpener` without introducing `STREAM_READY`.
- Never write `STREAM_READY` to the OpenSSH channel. It belongs only on the
  native provider stream (`open_ready`).
- Preserve originator and outer peer metadata for logs; parsing failures degrade
  safely and never authorize access.

## 2.5 TCP tests

- Unit tests for suffix-vs-secret routing and classic-auth-before-existence behavior.
- In-process integration: mock SSH channel ↔ provider ↔ local TCP echo, including
  banner-first target and half-close.
- Real OpenSSH e2e:
  - gateway on an ephemeral local port represented as 443-equivalent config;
  - `ssh -J` reaches a real test `sshd`/SSH server through the alias;
  - target key auth succeeds without agent forwarding;
  - target password path (when tooling available) remains inner and functional;
  - nonstandard target port succeeds only when requested exactly;
  - wrong username binding and wrong port are rejected without alias-existence leakage;
  - existing SSH secret consumer tests stay byte-for-byte green.
- Run the real jump session once with a native provider and once with
  `ssh -R jump/<alias>:<port>:localhost:<port>`; add key- and password-auth
  variants for the pure-SSH provider.
- Liveness regression: half-open provider disappears from registry/admin within
  the configured test timeout and auto-reconnect can reclaim it.

## Phase 2 gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --test ssh_jump_test
cargo test --all-features --test ssh_gateway_test
cargo test --all-features --test secret_test
cargo test --all-features
```

The phase is done only when a stock OpenSSH `ssh -J` command executes a command
against the target `sshd` through both provider transports, classic-auth
mismatch is jump-only/fail-closed, zombie/collision tests pass, and all existing
SSH/secret regressions remain green.
