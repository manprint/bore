# Phase 0 — Compile gates and dependency scaffold

> **Intent:** Make `--features vpn` compile-checkable for Windows without enabling runtime behavior yet.
> **Shippable alone?** yes — no behavior change; Windows VPN may still return explicit unsupported errors behind stubs.
> **Preconditions:** none

---

## Sub-phases

### 0.1 Expand VPN cfg gates to include Windows
- **Model:** Haiku 4.5
- **Files:** `src/vpn.rs:3`, `src/lib.rs:1`, `src/main.rs:5`, `src/main.rs:931`, `src/main.rs:1669`, `src/main.rs:1737`, `Cargo.toml:27`, `Cargo.toml:77`
- **Change:** Mirror existing Linux/macOS gate pattern and include Windows in feature-visible VPN code. Replace `any(target_os = "linux", target_os = "macos")` with `any(target_os = "linux", target_os = "macos", target_os = "windows")` only where the whole VPN module/CLI should exist. Do not change Linux-only or macOS-only runtime twins. In `Cargo.toml`, keep `vpn = ["udp", "dep:tun-rs"]` only until Phase 1 chooses final Windows dependency shape; if `tun-rs` cannot compile on Windows, split VPN deps so `tun-rs` remains in Unix target deps and Windows gets placeholder optional dep. Follow existing `Cargo.toml` target dependency conventions; do not create new dependency sections if an existing target section serves the same purpose.
- **Unit tests:** `cargo_check_windows_vpn_cfg` — represented by CI command `cargo check --features vpn --target x86_64-pc-windows-msvc`; asserts the VPN module and CLI parse compile on Windows target.
- **e2e tests:** none (compile-only, no behavior change).
- **Done:** `cargo fmt --all`; Linux `cargo check --features vpn`; Windows target `cargo check --features vpn --target x86_64-pc-windows-msvc` reaches only missing Windows runtime stubs, not cfg-hidden module errors. No changes inside Linux/macOS function bodies.

### 0.2 Add explicit Windows unsupported runtime stubs
- **Model:** Haiku 4.5
- **Files:** `src/vpn.rs:3261`, `src/vpn.rs:4091`, `src/vpn.rs:4203`, `src/vpn.rs:4384`, `src/vpn.rs:4406`, `src/vpn.rs:4563`, `src/vpn.rs:5081`
- **Change:** Add Windows cfg twins at existing isolation points with explicit `anyhow::bail!("Windows VPN backend not implemented yet")` or equivalent typed error. Required stubs: `hostcfg::create_tun` Windows twin with same signature as Linux/macOS; `hostcfg::NetConfig::apply` Windows twin with same public call shape; `hostcfg::stale_reclaim` Windows twin; Windows `Drop` behavior that only runs if `applied_ops` contains Windows ops. Keep stubs additive. Do not introduce trait abstraction. Follow existing `hostcfg` module placement; no new module file unless `vpn.rs` already has a platform module serving that role.
- **Unit tests:** `windows_vpn_backend_unsupported_error` — target-gated test asserts Windows stub error string is explicit and not a panic; `vpn_cfg_windows_symbols_exist` — compile-only test via `cargo check --features vpn --target x86_64-pc-windows-msvc`.
- **e2e tests:** none (runtime intentionally unsupported in Phase 0).
- **Done:** Linux/macOS tests unchanged; Windows `cargo check --features vpn --target x86_64-pc-windows-msvc` passes through all symbols except dependency availability issues resolved in 0.3.

### 0.3 Split platform-specific VPN dependencies
- **Model:** Sonnet 4.6
- **Files:** `Cargo.toml:27`, `Cargo.toml:77`, `Cargo.toml:82`, `.github/workflows/ci.yml:60`
- **Change:** Make dependency graph compile on Linux, macOS, and Windows. Keep `tun-rs` only for `cfg(any(target_os = "linux", target_os = "macos"))`. Add Windows-only WinTun dependency placeholder under `target.'cfg(target_os = "windows")'.dependencies` only if Phase 1 crate selected; until then, gate Windows backend stubs without a WinTun crate. The `vpn` feature remains one feature flag. Do not create `vpn-windows` unless Opus review approves a packaging reason. Update `vpn-cross-build` CI matrix to make Windows target check required, not best-effort.
- **Unit tests:** `cargo_metadata_vpn_windows_deps` — CI command verifies dependency resolution for `x86_64-pc-windows-msvc`; `cargo_check_linux_vpn_deps` — verifies Linux still resolves `tun-rs`.
- **e2e tests:** none.
- **Done:** `cargo check --features vpn` on Linux; `cargo check --features vpn --target x86_64-pc-windows-msvc`; no `tun-rs` attempted for Windows; no Windows-only dependency attempted for Linux/macOS.

### 0.4 Snapshot Windows command builder baseline
- **Model:** Haiku 4.5
- **Files:** `src/vpn.rs:3261`, `src/vpn.rs:3304`, `src/vpn.rs:3451`
- **Change:** Extend existing `hostcfg_cmd::windows` tests without changing behavior. Snapshot existing `cmd_route_add`, `cmd_route_del`, and `cmd_link_set_mtu` output. Add TODO-marked test names for missing builders: IP forwarding, firewall rule add/delete, NAT add/delete, prefix-translation add/delete, stale marker path. Keep tests platform-independent where possible by testing vector/string builders only.
- **Unit tests:** `test_cmd_windows_route_add_snapshot`; `test_cmd_windows_route_del_snapshot`; `test_cmd_windows_link_set_mtu_snapshot`; `test_windows_hostcfg_missing_builders_are_tracked`.
- **e2e tests:** none.
- **Done:** `cargo test --features vpn hostcfg_cmd::tests::test_cmd_windows -- --nocapture` passes on Linux host; Windows target compile still passes.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test subset:** `cargo test --features vpn hostcfg_cmd::tests::test_cmd_windows -- --nocapture`
- **Cross-check:** `cargo check --features vpn --target x86_64-pc-windows-msvc`
- **Regression guard:** Linux/macOS cfg bodies unchanged by diff review; `cargo test --features vpn test_pick_tun_name -- --nocapture`

## Phase done criterion

Phase 0 is done when `--features vpn` is visible to Windows builds, all missing runtime pieces fail with explicit unsupported errors or compile-time stubs, and Linux/macOS behavior remains unchanged.
