# Phase 4 — Non-VPN Windows parity

> **Intent:** Prove public tunnels, secret tunnels, vhost, server, transfer, and test utilities work on Windows without data-plane refactors.
> **Shippable alone?** yes — validates existing cross-platform transports and fixes only real Windows portability bugs.
> **Preconditions:** phase_01 DONE; phase_02/03 not required except for shared CI setup

---

## Sub-phases

### 4.1 Public tunnel parity: `bore local`
- **Model:** Haiku 4.5 for test wiring; Sonnet 4.6 only if bug found
- **Files:** `src/client.rs:1`, `src/server.rs:1`, `src/shared.rs:1`, `tests/` existing integration test folder, `.github/workflows/ci.yml:1`
- **Change:** Add Windows e2e coverage for `bore local <port>` TCP relay and public `--udp` direct path. Implementation scope is tests first; do not edit public tunnel data plane unless a Windows failure is reproduced. Test harness should follow existing `tests/` and script conventions; if no existing Windows e2e script folder exists, add under the closest existing test/script directory rather than creating a new top-level directory. Required assertions: server assigns public port; remote client receives local HTTP response; `--udp` direct path works when server `--udp` enabled; TCP fallback works if direct unavailable; `STREAM_READY` preserved.
- **Unit tests:** none unless bug fix needed; then name focused test after failing component.
- **e2e tests:** T-WIN-LOCAL1 — Windows client exposes local HTTP over TCP relay; Linux runner/client fetches expected body; T-WIN-LOCAL2 — Windows client public `--udp` direct path serves response; T-WIN-LOCAL3 — block UDP, same tunnel falls back to TCP relay per connection.
- **Done:** Windows public tunnel e2e passes; no changes to `client.rs`/`server.rs`/`shared.rs` unless tied to failing Windows test.

### 4.2 Secret tunnel parity: relay, carriers, UDP direct
- **Model:** Sonnet 4.6
- **Files:** `src/secret.rs:1`, `src/holepunch.rs:84`, `src/server.rs:1`, `tests/` existing integration test folder, `.github/workflows/ci.yml:1`
- **Change:** Add Windows e2e for secret provider/consumer. Preserve existing invariants: exactly one admin row for consumer regardless of carriers; carrier controls do not register/reap; heartbeats only main control; direct-path benign strays logged debug; relay failover across carriers. Do not edit secret code unless Windows-specific bug reproduced. Tests should run combinations: Windows provider + Linux consumer, Linux provider + Windows consumer, Windows provider + Windows consumer where runner topology permits.
- **Unit tests:** reuse existing secret carrier/admin tests; add `test_secret_windows_carrier_flag_wire_compat` only if cfg/serialization issue found.
- **e2e tests:** T-WIN-SECRET1 — Windows provider, Linux consumer, relay path succeeds; T-WIN-SECRET2 — Linux provider, Windows consumer, relay path succeeds; T-WIN-SECRET3 — Windows side with `--udp --carriers 4` goes direct or falls back cleanly; T-WIN-SECRET4 — admin `/status` shows one logical row, no N/A carrier rows; T-WIN-SECRET5 — kill one provider carrier, connection retries another live carrier.
- **Done:** Secret parity passes on Windows; no WARN spam from benign QUIC strays; admin row invariant preserved.

### 4.3 Vhost parity with and without `--udp`
- **Model:** Haiku 4.5 for test wiring; Sonnet 4.6 only if bug found
- **Files:** `src/vhost.rs:1`, `src/server.rs:1`, `src/holepunch.rs:84`, frontend/admin vhost files if tests cover admin, `.github/workflows/ci.yml:1`
- **Change:** Add Windows vhost e2e. Required assertions: vhost registration succeeds; Host header routing works; `--udp` direct path mirrors non-Windows behavior; carrier count behavior and fallback unchanged; admin vhost section shows same columns/flags as Linux/macOS. Do not refactor vhost data plane.
- **Unit tests:** none unless a Windows-specific bug fix is needed.
- **e2e tests:** T-WIN-VHOST1 — Windows client registers vhost and serves HTTP via TCP relay; T-WIN-VHOST2 — Windows client vhost `--udp` direct path serves response; T-WIN-VHOST3 — UDP blocked -> fallback; T-WIN-VHOST4 — admin vhost flags visible.
- **Done:** Vhost parity green; no non-test data-plane edits unless required by a failing Windows test.

### 4.4 Server mode parity on Windows
- **Model:** Sonnet 4.6
- **Files:** `src/server.rs:1`, `src/admin_api.rs:22`, `src/main.rs:1`, `.github/workflows/ci.yml:1`
- **Change:** Validate `bore server` runs on Windows for all non-VPN modes and acts as VPN relay server once Phase 3 is complete. Required: control port bind; data port allocation; TLS URL if existing tests cover it; `--udp` binds QUIC endpoint; admin API works; public/secret/vhost clients from Linux/macOS/Windows can connect. Windows firewall prompts are out of process; docs cover them.
- **Unit tests:** existing server tests; add `test_server_windows_default_bind_addresses` if Windows bind behavior differs.
- **e2e tests:** T-WIN-SERVER1 — Windows server relays Linux public local tunnel; T-WIN-SERVER2 — Windows server relays secret provider/consumer; T-WIN-SERVER3 — Windows server handles vhost; T-WIN-SERVER4 — Windows server `--udp` accepts public/secret/vhost direct registrations; T-WIN-SERVER5 — Windows server relays VPN control/data between Linux peers.
- **Done:** Windows server mode usable for all modes; admin endpoint stable.

### 4.5 Transfer and `test-udp` parity
- **Model:** Haiku 4.5
- **Files:** `src/transfer.rs:1`, `tests/transfer_stdin_cli_test.rs:56`, `src/main.rs:1`, `.github/workflows/ci.yml:45`
- **Change:** Expand existing Windows transfer CI coverage. Existing path codec tests cover reserved names/invalid chars. Add actual transfer sender/listener small-file e2e on Windows if absent. Add `bore test-udp` Windows smoke: UDP socket bind, STUN/direct diagnostics if no privileged network needed, `--tcp-secret-id` two-peer test where topology permits. Keep transfer carrier auto-scaling behavior unchanged.
- **Unit tests:** existing transfer path tests; `test_transfer_windows_reserved_names`; `test_transfer_windows_invalid_chars`; add `test_transfer_windows_carriers_auto_resolve` if not already portable.
- **e2e tests:** T-WIN-TRANSFER1 — Windows sender to Linux listener transfers file, resume works after interruption, BLAKE3 verifies; T-WIN-TRANSFER2 — Linux sender to Windows listener; T-WIN-UDPTEST1 — Windows `test-udp` basic diagnostic completes; T-WIN-UDPTEST2 — two-peer latency/bandwidth test through `--tcp-secret-id` completes.
- **Done:** Transfer/test-udp parity green on Windows; no regression to Linux/macOS transfer suite.

### 4.6 Opus non-VPN diff audit
- **Model:** Opus 4.8 review gate
- **Files:** `src/client.rs:1`, `src/server.rs:1`, `src/shared.rs:1`, `src/secret.rs:1`, `src/vhost.rs:1`, `src/holepunch.rs:84`, `src/transfer.rs:1`
- **Change:** Review final diff for Phase 4. Acceptable production changes outside VPN/hostcfg: Windows-specific portability fixes only, each backed by a failing Windows test. Reject broad refactors, protocol changes, or behavior changes. Confirm `shared::tune_tcp`, `STREAM_READY`, secret heartbeats/carrier invariants, vhost/public direct fallback, and transfer carrier defaults unchanged.
- **Unit tests:** none (review gate).
- **e2e tests:** Re-run T-WIN-LOCAL*, T-WIN-SECRET*, T-WIN-VHOST*, T-WIN-SERVER*, T-WIN-TRANSFER*, T-WIN-UDPTEST*.
- **Done:** Diff audit recorded in `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` or CI summary; any non-VPN code change has test ID and rationale.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** `cargo test --all-features transfer windows -- --nocapture` plus existing integration tests
- **Windows hosted e2e:** T-WIN-LOCAL*, T-WIN-SECRET*, T-WIN-VHOST*, T-WIN-SERVER*, T-WIN-TRANSFER*, T-WIN-UDPTEST*
- **Regression guard:** Linux/macOS non-VPN e2e scripts remain green; no production data-plane edits without failing Windows test

## Phase done criterion

Phase 4 is done when every non-VPN bore mode works on Windows with tests, and any production code changes are limited to proven Windows portability fixes.
