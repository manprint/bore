# Phase 1 — Scaffolding + russh spike

> **Intent:** land the `ssh-gateway` feature skeleton (pure additive, feature off by default) and prove every russh server primitive the design depends on, against a real OpenSSH client, before any wiring.
> **Shippable alone?** yes — no behavior change; default build unchanged.
> **Preconditions:** none.

Context (self-contained): bore is an async Rust tunnel server (`#![forbid(unsafe_code)]`,
edition 2021). The plan adds an embedded SSH ingress. This phase only adds the feature
gate, empty modules, and a spike test validating the russh API. Design reference:
`docs/SSH_GATEWAY.md`.

---

## Sub-phases

### 1.1 Feature gate, dependencies, module stubs
- **Model:** Haiku
- **Files:** `Cargo.toml:35-42` ([features]), `Cargo.toml:78-92` (optional-deps block), `src/lib.rs:19-52` (module list)
- **Change:**
  1. `Cargo.toml` [features]: add `ssh-gateway = ["dep:russh", "dep:argon2"]`. Do NOT add it to `default` (stays `["udp"]`).
  2. `[dependencies]`: add `russh = { version = "<latest stable>", optional = true }` and `argon2 = { version = "0.5", optional = true }`. Note: recent russh includes key parsing under `russh::keys` (the separate `russh-keys` crate was merged); if the chosen version still needs `russh-keys`, add it as a second optional dep inside the same feature. Run `cargo tree -e features -i russh` and record the tree in the commit message if any duplicate `ring`/`aws-lc` version appears.
  3. `src/lib.rs`: after the `pub mod server;` line add, in alphabetical position among modules:
     `#[cfg(feature = "ssh-gateway")] pub mod sshgw;` and `#[cfg(feature = "ssh-gateway")] pub mod sshgw_auth;` (pattern: the `vpn` gate at `src/lib.rs:40-51`, but no target_os condition — SSH gateway is OS-independent).
  4. Create `src/sshgw.rs` and `src/sshgw_auth.rs` stubs: module doc comment (one paragraph each, English, professional), and in `sshgw.rs` the two constants with comments citing parity: `pub const SSH_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);` (parity `CTRL_CLIENT_HEARTBEAT`, `src/secret.rs:59`) and `pub const SSH_CTRL_TIMEOUT: Duration = Duration::from_secs(60);` (parity `SECRET_CTRL_TIMEOUT`, `src/secret.rs:54`). No other code.
  Follow existing flat-module layout (D5); create no directories.
- **Unit tests:** none (stubs).
- **e2e tests:** none (no behavior change).
- **Done:** `cargo build` (default features) byte-identical behavior and green; `cargo build --features ssh-gateway` green; `cargo clippy --all-targets --features ssh-gateway -- -D warnings` green; `cargo clippy --all-targets --all-features -- -D warnings` green.

### 1.2 russh API spike test (design-gating)
- **Model:** Sonnet (Opus reviews the findings file before Phase 4 starts)
- **Files:** new `tests/ssh_gateway_spike_test.rs`; new `docs/plans/plan_SshGateway/SPIKE_FINDINGS.md`
- **Change:** Feature-gated integration test (`#![cfg(feature = "ssh-gateway")]`) that embeds a minimal russh server on an ephemeral 127.0.0.1 port and drives it with the real OpenSSH CLI. Test-file conventions: `#[tokio::test]`, sequential guard if needed (pattern: `SERIAL_GUARD` in `tests/transfer_test.rs`). Skip guard: if `ssh` or `ssh-keygen` is not on PATH, print a warning and return early (test passes) — CI installs them in phase 7.3.
  Harness helpers inside the test file:
  - generate a throwaway ed25519 client key: `ssh-keygen -t ed25519 -N "" -f <tempdir>/id`;
  - generate/load a server host key via russh's key API;
  - standard client opts used everywhere: `-o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ExitOnForwardFailure=yes -i <tempdir>/id -p <port> user@127.0.0.1`.
  The minimal `russh::server::Handler` must exercise and assert, each as its own test:
  - **T-SSH-SPIKE1 (pubkey auth):** `auth_publickey` receives the offered key; accept only the generated key; `ssh ... true`-style session succeeds; a different key is rejected.
  - **T-SSH-SPIKE2 (tcpip-forward + forwarded-tcpip):** client runs `-N -R testname:0:127.0.0.1:<echo>` where `<echo>` is a local TCP echo listener spawned by the test. Assert the handler receives `tcpip_forward` with address `"testname"` port `0`; reply granting port 45001 (any fixed test value); then from the server side open a `forwarded-tcpip` channel toward the client (originator dummy), convert with `channel.into_stream()`, write `b"ping"`, read back `b"ping"` (echoed by the client-side service). This proves the whole `-R` data path.
  - **T-SSH-SPIKE3 (direct-tcpip):** client runs `-N -L <lport>:testname:0 ...`; test connects to `127.0.0.1:<lport>`, writes `b"ping"`; assert handler's `channel_open_direct_tcpip` sees host `"testname"` port `0`, echo back over the channel stream, client socket reads `b"ping"`.
  - **T-SSH-SPIKE4 (exec + env):** client runs `-o SetEnv=BORE_NOTES=spike ... 'notes=cli'`; assert handler receives env `BORE_NOTES=spike` (requires `-o SendEnv=BORE_*`? record actual behavior) and exec string `notes=cli`; server writes a line to the channel and asserts the client prints it (capture ssh stdout).
  - **T-SSH-SPIKE5 (keepalive):** client runs with `-o ServerAliveInterval=1 -o ServerAliveCountMax=2 -N -R ...`; server answers global requests (record which russh hook fires for `keepalive@openssh.com`); assert the session is still alive after 5 s. Also record how the server can SEND a global request to the client (needed for I-3) — if russh cannot send `keepalive@openssh.com` from the server side, record the fallback (e.g. `SSH_MSG_IGNORE`/channel window probe) in the findings file.
  Write `SPIKE_FINDINGS.md`: one bullet per primitive — exact russh version pinned, handler method names/signatures actually used, any deviation from the design assumptions in `docs/SSH_GATEWAY.md` §1, and the server-initiated-keepalive answer. Phases 4-6 implementers read this file INSTEAD of re-discovering the API.
- **Unit tests:** the five tests above are the tests.
- **e2e tests:** T-SSH-SPIKE1..5 (cargo, feature-gated).
- **Done:** `cargo test --features ssh-gateway --test ssh_gateway_spike_test` green locally; `SPIKE_FINDINGS.md` written; default-features `cargo test` untouched and green.

> Loud note: if any spike assertion is impossible with russh, STOP — do not work around it silently. Update `SPIKE_FINDINGS.md`, flag the owner, and let Opus amend phases 4-6 before proceeding.

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test` (default) AND `cargo test --features ssh-gateway`
- **Regression guard:** default-feature build/test identical to pre-phase (no source file outside the new stubs/tests may change except Cargo.toml/lib.rs additions).

## Phase done criterion

`ssh-gateway` feature compiles on stable, T-SSH-SPIKE1..5 pass against a real OpenSSH client, `SPIKE_FINDINGS.md` exists and answers the five primitives, and the default build is provably unchanged (full default `cargo test` green).
