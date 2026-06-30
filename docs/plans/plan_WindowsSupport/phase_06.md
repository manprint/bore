# Phase 5 — Windows e2e and CI

> **Intent:** Make Windows support continuously verifiable across hosted CI, elevated CI/manual acceptance, and cross-platform interop scenarios.
> **Shippable alone?** yes — tests and CI can land progressively and expose unsupported VPN e2e as documented manual gate until self-hosted runner exists.
> **Preconditions:** phase_04 DONE for VPN e2e; phase_05 can run in parallel for non-VPN e2e

---

## Sub-phases

### 5.1 Define Windows test taxonomy and IDs
- **Model:** Haiku 4.5
- **Files:** `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new), `docs/plans/plan_WindowsSupport/resume.md`, existing `scripts/` or `tests/` e2e location
- **Change:** Create Windows test matrix doc before adding scripts. Group tests by privilege: hosted non-admin, hosted admin-if-available, self-hosted elevated, manual two/three-host. Include every T-ID from this plan: T-WIN-LOCAL*, T-WIN-SECRET*, T-WIN-VHOST*, T-WIN-SERVER*, T-WIN-TRANSFER*, T-WIN-UDPTEST*, T-WIN-TUN*, T-WIN-HOST*, T-WIN-FWD*, T-WIN-NAT*, T-WIN-VPN*, T-WIN-HUB*, T-WIN-GW*, T-WIN-STALE*, T-WIN-ADMIN*. Follow existing docs/vpn acceptance style; do not create decorative docs.
- **Unit tests:** none.
- **e2e tests:** none; this defines registry.
- **Done:** Each T-ID has command, topology, privilege requirement, expected pass/fail signal, cleanup check, and log location.

### 5.2 Hosted Windows CI: build, lint, unit, non-admin behavior
- **Model:** Sonnet 4.6
- **Files:** `.github/workflows/ci.yml:24`, `.github/workflows/ci.yml:45`, `.github/workflows/ci.yml:60`, `.github/workflows/ci.yml:102`
- **Change:** Add/strengthen Windows hosted jobs. Required commands: `cargo fmt --all` stays global; Windows job runs `cargo build --features vpn`, `cargo clippy --features vpn --all-targets -- -D warnings`, `cargo test --features vpn`, and non-admin tests that must not mutate host networking. Keep Linux all-features and macOS VPN jobs. Add artifact upload for Windows logs if e2e fails. If WinTun DLL cannot be downloaded on hosted runner, skip elevated TUN tests but still check missing-DLL error path.
- **Unit tests:** all Windows unit tests except elevated-only; `windows_admin_check_error_message`; `wintun_dll_missing_error_mentions_path`; hostcfg command snapshots.
- **e2e tests:** T-WIN-HOST0; non-admin parse/help; transfer path codec; small local loopback tests that do not require elevation.
- **Done:** Hosted Windows CI catches compile/clippy/test regressions; no flaky elevated operations in hosted job.

### 5.3 Hosted Windows non-VPN e2e
- **Model:** Sonnet 4.6
- **Files:** existing `scripts/` or `tests/` e2e location, `.github/workflows/ci.yml:1`
- **Change:** Add Windows non-VPN e2e scripts/tests using loopback where possible. Start bore server and clients as child processes; allocate random free ports; kill process tree on failure; collect logs. Avoid hard-coded privileged ports. Use PowerShell scripts only if repo already has script convention for Windows; otherwise Rust integration tests are preferred. New files must follow existing test/script folder conventions.
- **Unit tests:** helper tests for free-port allocation/process cleanup if helper code added.
- **e2e tests:** T-WIN-LOCAL1, T-WIN-SECRET1/2 where topology can run on one host, T-WIN-VHOST1, T-WIN-SERVER1/2/3, T-WIN-TRANSFER1/2 on loopback, T-WIN-UDPTEST1.
- **Done:** Hosted Windows e2e proves non-VPN modes work without admin.

### 5.4 Elevated Windows VPN CI path
- **Model:** Opus 4.8 design review → Sonnet implements
- **Files:** `.github/workflows/ci.yml:129`, new workflow if self-hosted runner needed, `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new)
- **Change:** Decide elevated CI strategy. Preferred: self-hosted Windows 11 runner with admin service account and WinTun DLL installed/available. Alternative until runner exists: manual acceptance doc is required gate, not CI. CI job must check runner labels, refuse to run destructive host-networking tests on unapproved runner, and always execute cleanup. Add preflight that records Windows version, admin status, WinTun driver version, firewall profiles, IP forwarding original state, routes/rules before test. Postflight must compare and fail if leaked routes/firewall/NAT/adapters/state files remain.
- **Unit tests:** none.
- **e2e tests:** T-WIN-TUN1/4/5, T-WIN-HOST1/2/3/4, T-WIN-FWD1/2, T-WIN-NAT1, T-WIN-VPN-NAT, T-WIN-MTU1/2, T-WIN-VPN-RELAY*, T-WIN-VPN-DIRECT*, T-WIN-VPN-CARR*, T-WIN-HUB*, T-WIN-GW*, T-WIN-STALE*, T-WIN-ADMIN1.
- **Done:** Elevated CI exists and is green, or manual acceptance is documented as remaining external blocker with exact commands and cleanup checks.

### 5.5 Cross-OS interop matrix
- **Model:** Sonnet 4.6
- **Files:** `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new), e2e scripts/tests from 5.3/5.4
- **Change:** Add cross-OS matrix, not just Windows↔Windows. Required peer combinations: Windows client/server against Linux; Windows against macOS if macOS runner/host available; Linux/macOS clients against Windows server; Windows VPN connector/listener with Linux/macOS peer; Windows hub with Linux/macOS spokes and vice versa. Keep matrix small but complete by mode. For modes that cannot run in hosted CI, put manual/self-hosted command in acceptance doc.
- **Unit tests:** none.
- **e2e tests:** T-WIN-INTEROP-LOCAL; T-WIN-INTEROP-SECRET; T-WIN-INTEROP-VHOST; T-WIN-INTEROP-SERVER; T-WIN-INTEROP-VPN-RELAY; T-WIN-INTEROP-VPN-DIRECT; T-WIN-INTEROP-HUB; T-WIN-INTEROP-NAT.
- **Done:** At least Windows↔Linux interop automated; Windows↔macOS documented or automated depending runner availability.

### 5.6 Performance and soak tests
- **Model:** Sonnet 4.6
- **Files:** existing benchmark/script locations, `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new), `docs/vpn/VPN_WINDOWS.md` (new)
- **Change:** Add bounded performance tests so Windows support is not just functional. Required checks: public/secret/vhost throughput smoke; VPN relay and direct throughput smoke; UDP buffer warning if throughput capped; long idle direct VPN soak over 10+ minutes; carrier direct ordering test under load. Do not set unrealistic Linux parity thresholds; use minimum thresholds based on runner class and document expected lower Windows throughput if socket buffers/offload limit performance.
- **Unit tests:** none.
- **e2e tests:** T-WIN-PERF1 — public tunnel throughput smoke; T-WIN-PERF2 — secret direct throughput smoke; T-WIN-PERF3 — VPN relay/direct throughput smoke; T-WIN-SOAK1 — VPN direct idle + traffic survives 10 minutes; T-WIN-SOAK2 — carrier direct no reordering/loss beyond threshold.
- **Done:** Performance smoke stable; docs include known limits.

### 5.7 Final regression matrix gate
- **Model:** Opus 4.8 review gate
- **Files:** `.github/workflows/ci.yml:1`, `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` (new), `docs/plans/plan_WindowsSupport/resume.md`
- **Change:** Review complete test matrix before declaring implementation done. Required full gates: Linux `cargo fmt`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo test --all-features`; macOS VPN CI/e2e; Windows hosted CI; Windows elevated CI/manual; existing netns scripts where available. Map every reference-scenario bullet from `overview.md` to at least one green T-ID.
- **Unit tests:** none.
- **e2e tests:** full matrix.
- **Done:** Acceptance table has no TODO for required features; only explicitly deferred items may remain, and user must approve any deferral.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** Linux `cargo test --all-features`; Windows hosted `cargo test --features vpn`; macOS `cargo test --features vpn`
- **Windows hosted e2e:** non-admin + non-VPN matrix
- **Windows elevated e2e/manual:** full VPN matrix
- **Regression guard:** Existing Linux VPN netns and macOS VPN e2e remain green

## Phase done criterion

Phase 5 is done when Windows support has continuous hosted CI for build/unit/non-VPN, elevated CI or fully specified manual acceptance for VPN host networking, and a cross-OS matrix proving every feature named in the reference scenario.
