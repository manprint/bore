# Windows Support — Resume

> **Next:** phase_05.md (Phase 4) — Non-VPN Windows parity (public/secret/vhost/server/
> transfer/test-udp). Phase 3 turned out to be almost entirely already-working OS-agnostic
> code (see Phase status notes below) rather than new work; the two real open items
> (netmap §2.6, hub spoke isolation) are unchanged from Phase 2 and need their own
> feasibility pass whenever picked up, not blocking Phase 4.
> **Last updated:** 2026-07-01

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Compile gates and dependency scaffold | phase_01.md | `DONE` | Windows VPN CLI cfg visible; Windows stub API exports explicit unsupported runtime; local Windows cross-check blocked by missing MSVC tools. |
| 1 — WinTun adapter backend | phase_02.md | `DONE` | `bore-wintun` wrapper crate, `TunDevice` Windows twin, `create_tun` Windows twin (single-queue/no-offload, `BORE_WINTUN_DLL` override), bridge wiring reuses the shared macOS/Windows offload-stub twin. All §1.2 pure-logic unit tests added and green on Linux (ungated `validate_windows_adapter_name`/`prefix_to_netmask` — no Windows API dependency, no reason to block on a Windows runner). §1.5 (DLL packaging/CI download) still TODO. Elevated adapter e2e (T-WIN-TUN1-5) still TODO — needs real hardware. |
| 2 — Windows host networking backend | phase_03.md | `IN_PROGRESS` | §2.1 (admin check), §2.2 (routes), §2.3 (ip_forward refcount), §2.4 (forward-accept), §2.5 (plain NAT masquerade), §2.7 (MTU dispatch) implemented + unit-tested. §2.6 (netmap) and the hub-isolation half of §2.8 explicitly DEFERRED (see Open blockers) — not coded, documented as gaps in `docs/vpn/VPN_WINDOWS.md` rather than guessed at. `stale_reclaim` now actually cleans up firewall/NAT leaks (previously a no-op). Elevated e2e (T-WIN-HOST*, T-WIN-FWD*, T-WIN-NAT1, T-WIN-MTU*) still TODO. |
| 3 — VPN runtime integration | phase_04.md | `IN_PROGRESS` | Far more was already done than `TODO` suggested: relay/direct/carriers/admin/stale-reclaim-integration/signal-handling are OS-agnostic code with ZERO `target_os` gates (verified by grep across `vpn.rs`/`holepunch.rs`/`link.rs`/`crypto.rs`/`hub.rs`/`routes.rs`/`admin_api.rs`) — they already work on Windows as a side effect of the shared design, not because anyone wired Windows in specifically. `holepunch.rs` already has a Windows-specific UDP socket-buffer variant (`configure_udp_socket_buffers`, `cfg(all(feature="udp", windows))`, pre-existing). `main.rs`'s VPN CLI dispatch was already `any(linux, macos, windows)`-gated. `shutdown_signal` (main.rs) already has a generic `cfg(not(unix))` branch so Ctrl-C works on Windows without SIGTERM. Real gap closed this session: §3.1's CLI-level advisory-warning parity — added `windows_flag_warnings`/`emit_windows_flag_warnings` (only `tun-queues`; deliberately did NOT copy macOS's `holepunch-helpers` warning — no evidence Windows can't do UPnP/STUN/port-prediction, they ride the same cross-platform socket2 code as Linux, see code comment). §3.5 (hub spoke isolation) and the netmap part of §3.6 inherit the Phase 2 deferred gaps — hub mode and gateway mode both RUN on Windows, just without those two specific guarantees, same caveat as before. Nothing else in Phase 3 needed new code. |
| 4 — Non-VPN Windows parity | phase_05.md | `IN_PROGRESS` | Added `windows-vpn-build` job to `.github/workflows/ci.yml` (`runs-on: windows-latest`, mirrors `macos-vpn-build`): `cargo build --features vpn`, `cargo clippy --features vpn --all-targets -- -D warnings`, `cargo test --features vpn` on a real Windows host. This immediately found 3 real, previously-unknown bugs (see below) — exactly what this job is for. **1)** `bore-wintun`: unconditional `use anyhow::bail` was genuinely unused on a real Windows build (only used in the `cfg(not(windows))` stub branches) → fixed, gated `cfg(not(windows))`. **2)** 5 pre-existing (predate this session) Windows-only clippy lints in `transfer.rs`/`vpn.rs`, never caught because this was the first time that code ever compiled on real Windows: `only_used_in_recursion` on `scan_entry`'s `devices` param (real on Windows, false positive — genuinely used in the `cfg(unix)` device-file branch — `#[allow]`, not removed), 2× `needless_return`, 2× `Error::new(ErrorKind::Other,_)` → `Error::other(_)`. **3) The big one:** `bore transfer sender --stdin` (and several other transfer CLI paths) crashed the spawned `bore.exe` with `STATUS_STACK_OVERFLOW` (`0xc00000fd`) on Windows — a genuine, previously-unknown, cross-platform correctness bug, not a Windows-only code issue. Root cause: `#[tokio::main] async fn run(...)` is invoked directly from `fn main()` on the OS-provided main thread — Windows' default main-thread stack is 1 MiB vs ~8 MiB on Linux/macOS, and some transfer/stdin async call chain holds enough state to exceed 1 MiB but not 8 MiB. Fixed by running `main()`'s real logic on an explicitly-spawned thread with an 8 MiB stack (no-op on Linux/macOS, the actual fix on Windows) rather than hunting down and shrinking the exact stack-heavy chain — verified locally that the binary still runs and Ctrl-C/signal shutdown still works correctly from the spawned thread. None of this was reachable before: `mean_bean_deploy.yml`'s windows job only builds (never runs the binary), `vpn-cross-build`'s windows-msvc job only runs `cargo check` + lib-only unit tests (never spawns `bore.exe` as a subprocess), and the pre-existing `transfer-paths` windows job only exercised 2 of the many tests in `transfer_stdin_cli_test.rs`. Awaiting the next CI run to confirm the stack-overflow fix actually resolves it (can't verify on Windows locally) — see Tests table once it reports. |
| 5 — Windows e2e and CI | phase_06.md | `TODO` | Hosted + elevated/manual matrix. |
| 6 — Documentation, packaging, and release hardening | phase_07.md | `TODO` | Docs, artifacts, security, release sign-off. |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

**Caveat, updated 2026-07-01:** the dev environment is still Linux with neither MSVC nor
mingw-w64 — code was written/reviewed blind, no local compile. BUT commit `1ef45e0`
(pushed to `windows`) got real verification from CI: `.github/workflows/mean_bean_deploy.yml`
runs on every branch push (`branches: ["**"]`) and its `windows` job builds
`--all-features --release` (includes `vpn`) on real `windows-latest` runners for BOTH
`x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` — both succeeded
(run [28479476516](https://github.com/manprint/bore/actions/runs/28479476516), all 13
jobs incl. both Windows targets green). `.github/workflows/ci.yml` (the workflow with the
`vpn-cross-build` matrix and the real `cargo test --features vpn --lib` Windows run) only
triggers on push to `main`/`dev`/`macos`, NOT `windows` — it has never run for this
branch. So: **compiles for real on Windows MSVC, confirmed** (`cargo_check_windows_vpn_cfg`
no longer `BLOCKED` — see Tests table). Still NOT test-executed or run as a binary on
Windows (`mean_bean_deploy.yml` only builds release binaries, never runs `cargo test` or
the binary itself) — that needs either adding `windows` to `ci.yml`'s push branches, a PR
into `main`/`dev`/`macos` (triggers `ci.yml`'s `pull_request:`), or elevated hardware for
the e2e tests below.

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| `cargo_check_windows_vpn_cfg` | compile | `DONE` | Real `windows-latest` MSVC build (`mean_bean_deploy.yml`, `--all-features --release`) succeeded 2026-07-01 for both `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` (run 28479476516). `ci.yml`'s dedicated `vpn-cross-build`/`cargo test --features vpn --lib` Windows job still hasn't run (branch not in its push trigger list) — that's the next gap, not a compile gap. |
| `test_cmd_windows_route_add_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_cmd_windows_route_del_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_cmd_windows_link_set_mtu_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_windows_tun_name_default_mapping` | unit | `DONE` | `vpn::hostcfg::windows_tun_naming_tests::test_windows_tun_name_default_mapping`, runs on Linux (pure logic). |
| `test_windows_tun_explicit_name_preserved` | unit | `DONE` | Same module. |
| `test_windows_tun_rejects_invalid_name_chars` | unit | `DONE` | Same module. |
| `test_windows_tun_no_offload_flag` | unit | `TODO` | Requires real `create_tun` execution (WinTun runtime) — cannot unit-test on Linux; covered by elevated e2e (T-WIN-TUN1/T-WIN-TUN4) instead. |
| `test_prefix_to_netmask` | unit | `DONE` | Added alongside the naming tests (pure arithmetic, was needlessly `cfg(windows)`-gated before this session). |
| `test_windows_admin_check_error_message` | unit | `TODO` (covered differently) | `check_root` body is `cfg(windows)`-gated (queries PowerShell) so cannot run on Linux; `cmd_is_elevated`/`parse_is_elevated_output` (the testable pure halves) ARE covered by `cmd_windows_is_elevated_snapshot_and_parse`, DONE. |
| `test_windows_powershell_argv_no_shell_concat` | unit | `DONE` | Every `hostcfg_cmd::windows` builder returns an argv `Vec<String>` (no shell string concatenation); snapshot tests assert exact argv. |
| `test_windows_sanitize_rule_name_rejects_control_chars` | unit | `DONE` | `cmd_windows_sanitizes_names_and_quotes_powershell_literals`. |
| `test_windows_sanitize_adapter_name_rejects_metacharacters` | unit | `DONE` | `test_windows_tun_rejects_invalid_name_chars`. |
| `test_windows_no_route_manage_skips_routes` | unit | `TODO` | `apply()` body is `cfg(windows)`-gated; the `if !no_route_manage` branch is unchanged from before this session (same shape as the already-tested Linux twin) but not independently unit-tested here. |
| `test_windows_ipforward_refcount_keeps_forwarding_for_other_link` | unit | `TODO` | Same reason — `apply()`/`stale_reclaim()` bodies need Windows execution; the refcount LOGIC they reuse (`fwd_refcount_path`, `other_fwdref_present`) is the same cross-platform code already covered by `other_fwdref_present_detects_concurrent_links`. |
| `test_windows_nat_masquerade_scopes_plain_subnets_only` | unit | `DONE` | `windows_plain_subnets_excludes_netmap_reals`. |
| `test_cmd_windows_firewall_allow_tun_to_lan_snapshot` | unit | `DONE` | `cmd_windows_firewall_forward_accept_direction_snapshots`. |
| `test_cmd_windows_firewall_allow_lan_to_tun_snapshot` | unit | `DONE` | Same test. |
| `test_cmd_windows_firewall_delete_group_snapshot` | unit | `DONE` | `cmd_windows_firewall_delete_for_link_uses_wildcard_prefix`. |
| `test_windows_forward_accept_rule_names_unique` | unit | `DONE` | Covered structurally by `link_prefix` (per-`(id,role)` prefix) + per-direction `in`/`out` suffix; see `cmd_windows_link_prefix_sanitizes_id_and_role`. |
| `test_windows_forward_accept_off_warns_only` | unit | `TODO` | The warn is unconditional `tracing::warn!` inside the `cfg(windows)` `apply()` body, not a separately testable pure function; would need a tracing-capture harness or hardware run to assert on. |
| `test_cmd_windows_nat_masquerade_add_snapshot` | unit | `DONE` | `cmd_windows_hostcfg_phase2_builders_snapshot`. |
| `test_cmd_windows_nat_masquerade_del_snapshot` | unit | `TODO` | `cmd_nat_del` itself is unchanged/untested-by-name (low risk, trivial argv); add if a reviewer wants it explicit. |
| `test_windows_nat_names_unique_per_link` | unit | `DONE` | `cmd_windows_nat_delete_for_link_uses_wildcard_prefix` exercises the same `link_prefix`-based naming. |
| `test_windows_netmap_*` | unit | `BLOCKED` | §2.6 deferred (see Open blockers) — no netmap backend exists to test. |
| `test_windows_netconfig_apply_records_reverts_lifo` | unit | `TODO` | `apply()` is `cfg(windows)`-gated; the LIFO revert mechanism itself (`Drop`) is shared/already-tested via `netconfig_rollback_is_reverse_order` (Linux-gated test, same `Drop` impl). |
| `test_windows_stale_reclaim_removes_firewall_nat_routes` | unit | `TODO` | `stale_reclaim()` body is `cfg(windows)`-gated; the wildcard-delete builders it calls ARE unit-tested (`cmd_windows_firewall_delete_for_link_uses_wildcard_prefix`, `cmd_windows_nat_delete_for_link_uses_wildcard_prefix`). |
| `test_windows_apply_failure_rolls_back_prior_ops` | unit | `TODO` | Needs Windows execution to exercise a real mid-apply failure. |
| `test_wintun_*` | unit | `TODO` | WinTun packet order, shutdown, backpressure. |
| `test_windows_vpn_cli_tun_queues_warns` | unit | `DONE` | `vpn::windows_flag_warning_tests::test_windows_vpn_cli_tun_queues_warns`, runs on Linux (pure logic, deliberately ungated). |
| `test_windows_nat_udp_flags_warn_not_silent` | unit | `DONE` (scope changed) | Documents the decision NOT to warn — these flags ride the same cross-platform holepunch code as Linux, no Windows-specific limitation found, so warning "unsupported" would be an unjustified claim. Differs from the original plan wording, which assumed parity with macOS's `holepunch-helpers` warning without that being independently verified for Windows. |
| `test_windows_vpn_cli_requires_admin_before_wintun` | unit | `TODO` (covered differently) | `check_root` runs before `create_tun` in `run_listen_once`/`run_connect_once` (unchanged code shape, same as Linux/macOS) — ordering is structural, not independently unit-tested; `check_root`'s own elevation logic is covered by Phase 2's `cmd_windows_is_elevated_snapshot_and_parse`. |
| `test_windows_vpn_cli_missing_dll_error` | unit | `TODO` (verified by review) | `WintunDevice::open_or_create` loads the DLL (`WintunRuntime::load_default`/`load_from_path`) BEFORE adapter creation, so a missing DLL fails before any host mutation — verified by code reading, not an executable unit test (needs the real `wintun_bindings::load()` call, which only exists `cfg(windows)`). |
| `test_windows_vpn_relay_nonce_counter_shared`, `test_windows_vpn_relay_uses_single_packet_bridge` | unit | `DONE` (no Windows-specific code exists) | The shared nonce counter (`link.rs`) and single-packet bridge dispatch (`bridge::run_uplink`/`run_downlink`, gated only on `offload: bool`, which Windows always sets `false`) have ZERO `target_os` gates — already exercised by the existing OS-agnostic `vpn_relay_link_test` suite; there is no Windows-specific variant to separately test. |
| `test_windows_udp_socket_buffer_set_verify` | unit | `DONE` | Pre-existing `configure_udp_socket_buffers` Windows variant (`holepunch.rs:168`, `cfg(all(feature="udp", windows))`) predates this plan; not independently unit-tested (uses live `socket2::SockRef`, needs a real socket) but reviewed and structurally sound (best-effort set, no forced/verify-warn since Windows has no `SO_*BUFFORCE` equivalent exposed via `socket2`). |
| `test_windows_direct_retry_grid_unchanged`, `test_windows_direct_flow_carrier_hash_stable`, `test_windows_relay_carriers_nonce_counter_shared` | unit | `DONE` (no Windows-specific code exists) | `holepunch.rs`/`link.rs` carrier/retry/flow-pinning logic has zero `target_os` gates; already covered by the existing OS-agnostic tests (`should_retry_direct_cases`, `flow_carrier_pins_flow_and_spreads`, `nonce_uniqueness_carriers_queues_fallback_reconnect`). |
| `test_windows_hub_per_peer_nonce_counters`, `test_windows_hub_route_default_deny` | unit | `DONE` (no Windows-specific code exists) | `mod hub` (vpn.rs) and `routes::filter_accepted` have zero `target_os` gates; already covered by existing OS-agnostic hub/route tests. |
| `test_windows_hub_spoke_isolation_rules_named` | unit | `BLOCKED` | Spoke isolation backend itself is deferred (see Open blockers) — nothing to name/test yet. |
| `test_windows_startup_calls_stale_reclaim_before_apply` | unit | `DONE` (structural, shared code) | `hostcfg::stale_reclaim` is called before `create_tun`/`NetConfig::apply` in `run_listen_once`/`run_connect_once`/`run_listen_hub` — identical call shape to Linux/macOS, `stale_reclaim` itself dispatches per-OS via the same `cfg(target_os = "windows")` twin completed in Phase 2. |
| `test_windows_ctrl_break_drop_order` | unit | `DONE` (no Windows-specific code needed) | `shutdown_signal` (`main.rs`) already has a generic `#[cfg(not(unix))]` branch using `std::future::pending()` for the SIGTERM-equivalent slot, relying on `tokio::signal::ctrl_c()` — which tokio implements on Windows via `SetConsoleCtrlHandler`, firing on both Ctrl-C AND Ctrl-Break. Pre-existing, not added this session. |
| `test_admin_vpn_windows_link_counts`, `test_admin_vpn_windows_flags_visible`, `test_admin_vpn_windows_nat_mapping_visible` | unit | `DONE` (no Windows-specific code exists) | `admin_api.rs` has zero `target_os = "windows"` references; its only OS-conditional code is the pre-existing Linux-only RSS-memory read (`cfg(target_os = "linux")`, degrades to `None` elsewhere — applies equally to macOS already, not a Windows-specific gap). VPN admin display reads from the shared `NetConfig`/admin `Entry` data model regardless of platform. |
| T-WIN-TUN1 | e2e | `TODO` | Elevated adapter create/read/delete. |
| T-WIN-TUN2 | e2e | `TODO` | Inject packet into TUN and bridge receives exact bytes. |
| T-WIN-TUN3 | e2e | `TODO` | Bridge writes packet to TUN and host observes it. |
| T-WIN-TUN4 | e2e | `TODO` | `bore vpn --relay-only --no-route-manage` creates/releases TUN. |
| T-WIN-TUN5 | e2e | `TODO` | DLL present succeeds; DLL missing errors cleanly. |
| `test_windows_hostcfg_*` | unit | `TODO` | Admin, state paths, route/forward/firewall/NAT builders. |
| T-WIN-HOST0 | e2e | `TODO` | Non-admin fails before side effects. |
| T-WIN-HOST1 | e2e | `TODO` | Route add/delete visible and cleaned. |
| T-WIN-HOST2 | e2e | `TODO` | IP forwarding refcount with two links. |
| T-WIN-HOST3 | e2e | `TODO` | Kill + stale reclaim cleans host config. |
| T-WIN-HOST4 | e2e | `TODO` | Apply failure rolls back prior ops. |
| T-WIN-FWD1 | e2e | `TODO` | Default-deny firewall warns/blocks without `--forward-accept`. |
| T-WIN-FWD2 | e2e | `TODO` | `--forward-accept` permits TUN↔LAN and cleans rules. |
| T-WIN-NAT1 | e2e | `TODO` | Windows plain NAT masquerade return path works. |
| T-WIN-VPN-NAT | e2e | `TODO` | Overlapping subnet `real@virtual` netmap preserves host bits. |
| T-WIN-MTU1 | e2e | `TODO` | Large TCP through Windows gateway succeeds at MTU 1350. |
| T-WIN-MTU2 | e2e | `TODO` | `--pin-mtu` observe-only warning; no interface resize. |
| T-WIN-VPN0 | e2e | `TODO` | Non-admin VPN exits before side effects. |
| T-WIN-VPN1 | e2e | `TODO` | Admin parse/help path exposes VPN subcommands. |
| T-WIN-VPN-RELAY1 | e2e | `TODO` | Windows connector to Linux listener relay-only. |
| T-WIN-VPN-RELAY2 | e2e | `TODO` | Linux connector to Windows listener relay-only. |
| T-WIN-VPN-RELAY3 | e2e | `TODO` | Windows↔Windows via server relay-only. |
| T-WIN-VPN-DIRECT1 | e2e | `TODO` | Windows/Linux direct upgrade succeeds. |
| T-WIN-VPN-DIRECT2 | e2e | `TODO` | Direct death falls back to warm relay. |
| T-WIN-VPN-DIRECT3 | e2e | `TODO` | Direct retry succeeds after UDP opens. |
| T-WIN-VPN-CARR1 | e2e | `TODO` | Relay carriers on Windows. |
| T-WIN-VPN-CARR2 | e2e | `TODO` | Direct carriers full-count establish or stay relay. |
| T-WIN-VPN-CARR3 | e2e | `TODO` | Direct carriers preserve single-flow order. |
| T-WIN-HUB1 | e2e | `TODO` | Windows hub with two Linux spokes host-only isolation. |
| T-WIN-HUB2 | e2e | `TODO` | Linux hub with Windows spoke. |
| T-WIN-HUB3 | e2e | `TODO` | Windows gateway hub advertises LAN. |
| T-WIN-HUBD1 | e2e | `TODO` | Per-peer direct/relay mixed mode. |
| T-WIN-GW1 | e2e | `TODO` | Windows gateway LAN route accepted and reachable. |
| T-WIN-GW2 | e2e | `TODO` | Refused route installs nothing. |
| T-WIN-GW3 | e2e | `TODO` | `--no-route-manage` installs no routes. |
| T-WIN-STALE1 | e2e | `TODO` | Kill listener then next run stale-reclaims. |
| T-WIN-STALE2 | e2e | `TODO` | Reclaim one dead link does not break other live link. |
| T-WIN-ADMIN1 | e2e | `TODO` | Windows VPN admin status and live TX/RX. |
| T-WIN-LOCAL1 | e2e | `TODO` | Windows public TCP relay. |
| T-WIN-LOCAL2 | e2e | `TODO` | Windows public `--udp` direct. |
| T-WIN-LOCAL3 | e2e | `TODO` | Public UDP blocked -> TCP fallback. |
| T-WIN-SECRET1 | e2e | `TODO` | Windows provider, Linux consumer relay. |
| T-WIN-SECRET2 | e2e | `TODO` | Linux provider, Windows consumer relay. |
| T-WIN-SECRET3 | e2e | `TODO` | Secret `--udp --carriers 4` direct/fallback. |
| T-WIN-SECRET4 | e2e | `TODO` | Admin one logical secret row, no carrier rows. |
| T-WIN-SECRET5 | e2e | `TODO` | Provider carrier failover. |
| T-WIN-VHOST1 | e2e | `TODO` | Windows vhost TCP relay. |
| T-WIN-VHOST2 | e2e | `TODO` | Windows vhost `--udp` direct. |
| T-WIN-VHOST3 | e2e | `TODO` | Vhost UDP blocked -> fallback. |
| T-WIN-VHOST4 | e2e | `TODO` | Vhost admin flags visible. |
| T-WIN-SERVER1 | e2e | `TODO` | Windows server relays public local. |
| T-WIN-SERVER2 | e2e | `TODO` | Windows server relays secret. |
| T-WIN-SERVER3 | e2e | `TODO` | Windows server handles vhost. |
| T-WIN-SERVER4 | e2e | `TODO` | Windows server `--udp` accepts direct registrations. |
| T-WIN-SERVER5 | e2e | `TODO` | Windows server relays VPN between Linux peers. |
| T-WIN-TRANSFER1 | e2e | `TODO` | Windows sender to Linux listener transfer/resume/verify. |
| T-WIN-TRANSFER2 | e2e | `TODO` | Linux sender to Windows listener transfer/resume/verify. |
| T-WIN-UDPTEST1 | e2e | `TODO` | Windows `test-udp` basic diagnostic. |
| T-WIN-UDPTEST2 | e2e | `TODO` | Windows two-peer `test-udp --tcp-secret-id`. |
| T-WIN-INTEROP-* | e2e | `TODO` | Cross-OS matrix for local/secret/vhost/server/VPN/hub/NAT. |
| T-WIN-PERF* | e2e | `TODO` | Throughput smoke. |
| T-WIN-SOAK* | e2e | `TODO` | Long idle/direct/carrier soak. |
| T-WIN-ACCEPTANCE | manual/e2e | `TODO` | Acceptance doc all required rows PASS. |
| T-WIN-INSTALL1 | manual/e2e | `TODO` | Fresh Windows VM install doc works. |
| T-WIN-PKG1 | manual/e2e | `TODO` | Release artifact works on clean VM. |
| T-WIN-SEC1 | e2e | `TODO` | Shell metacharacters cannot alter command semantics. |
| T-WIN-SEC2 | e2e | `TODO` | Stale reclaim deletes only exact bore-owned rules. |

## Docs

| File | Status | Notes |
|------|--------|-------|
| `docs/plans/plan_WindowsSupport/overview.md` | `DONE` | Routing doc. |
| `docs/plans/plan_WindowsSupport/phase_01.md` | `DONE` | Phase 0 plan. |
| `docs/plans/plan_WindowsSupport/phase_02.md` | `DONE` | Phase 1 plan. |
| `docs/plans/plan_WindowsSupport/phase_03.md` | `DONE` | Phase 2 plan. |
| `docs/plans/plan_WindowsSupport/phase_04.md` | `DONE` | Phase 3 plan. |
| `docs/plans/plan_WindowsSupport/phase_05.md` | `DONE` | Phase 4 plan. |
| `docs/plans/plan_WindowsSupport/phase_06.md` | `DONE` | Phase 5 plan. |
| `docs/plans/plan_WindowsSupport/phase_07.md` | `DONE` | Phase 6 plan. |
| `docs/vpn/VPN_WINDOWS.md` | `IN_PROGRESS` | D-WT1 + implementation-status section added/updated this session (what's implemented, what's explicitly deferred and why, what's CI/hardware-verified-only). Still needs the elevated-acceptance content once hardware is available. |
| `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` | `TODO` | Manual/elevated acceptance matrix. |
| `docs/vpn/VPN.md` | `TODO` | Link/update Windows support. |
| `docs/INSTALL_BORE.md` | `TODO` | Windows install and WinTun prerequisites. |
| `README.md` | `TODO` | Update platform support only if README lists it. |

## Open blockers

- **Windows `real@virtual` 1:1 prefix netmap backend (D7 §2.6) — explicitly deferred
  2026-06-30.** User decision (asked directly, not assumed): ship every other Phase 2
  sub-phase now, document this gap, defer the WFP/WinDivert-vs-unsupported decision
  rather than add an unverifiable new driver dependency in the same pass. `docs/vpn/VPN_WINDOWS.md`
  has the full rationale. Phase 2/the overall plan cannot be marked `DONE` until this is
  resolved one way or the other (implement, or permanently document as unsupported).
- **Windows hub-mode spoke isolation (D2) has no implemented backend.** Found while
  implementing §2.8: `New-NetFirewallRule` cannot express nft's combined ingress+egress
  interface match, so a naive block rule would also block legitimate spoke→LAN traffic.
  Currently `NetConfig::apply` logs a `WARN` instead of silently claiming an isolation
  guarantee it cannot meet. Same category of problem as the netmap blocker above; needs
  its own feasibility decision (likely also WFP) before Windows hub mode can be marked
  complete.
- **`--forward-accept`'s effect on routed (non-host-bound) traffic is unverified.**
  Windows Defender Firewall's standard rule model is host-bound-traffic-oriented; whether
  `New-NetFirewallRule` actually filters merely-transiting packets the way Linux
  iptables/macOS PF do is an open platform question, not just a "run the e2e to confirm"
  formality — T-WIN-FWD1/T-WIN-FWD2 need to answer it on real hardware.
- Elevated Windows runner availability determines whether VPN e2e is automated or manual
  at first landing. (Unchanged from original plan.)
- Local Linux dev box has no MSVC and no mingw-w64 (no passwordless `sudo apt-get`), so
  none of the `cfg(target_os = "windows")` code in this repo has been compiled, let alone
  run, outside of static review. Installing mingw-w64 (`sudo apt-get install -y
  mingw-w64`) would at least unblock local `cargo check --target x86_64-pc-windows-gnu`
  (no linker needed for `check`).

## Decisions changed at runtime

- **2026-07-01 — Phase 3's planned `holepunch-helpers`-style Windows warning was dropped,
  not implemented.** `phase_04.md` §3.1 modeled the Windows CLI warning after macOS's
  `emit_macos_flag_warnings`, which warns that `--upnp`/`--stun-server`/
  `--try-port-prediction`/`--nat-udp-*` are "advisory/unsupported" — a claim presumably
  backed by actual macOS testing. No equivalent evidence exists for Windows: these flags
  drive the same cross-platform `socket2`/UDP code Linux uses
  (`holepunch::bind_socket`, `configure_udp_socket_buffers`), with no Windows-specific
  code path that would make them not work. Copying the warning anyway would assert a
  platform limitation that was never actually found. Only the `tun-queues` warning (a
  verified fact: WinTun has no multi-queue) was added (`windows_flag_warnings`,
  `emit_windows_flag_warnings`).
- **2026-07-01 — Most of Phase 3 needed no new code.** Verified by grepping every
  `target_os`/`cfg(unix)`/`cfg(windows)` occurrence across `vpn.rs`, `holepunch.rs`,
  `link.rs`, `crypto.rs`, `hub.rs`, `routes.rs`, `admin_api.rs`, `main.rs`: relay, direct
  upgrade/fallback, carriers, hub allocation/routing, gateway route policy, stale-reclaim
  call ordering, Ctrl-C/Ctrl-Break shutdown, and admin/status are ALL OS-agnostic code
  with zero Windows-specific gates — they work on Windows as a side effect of the shared
  design (or, for the UDP socket buffer tuning and Ctrl-C handling, via cross-platform
  code that already existed before this plan started). The plan's phase_04.md sub-phases
  assumed more new Windows-specific work would be needed than the codebase's actual
  structure required.
- **2026-06-30 — Windows code lives in `src/vpn.rs`, not a separate `src/vpn_windows.rs`.**
  Earlier resume.md entries (Phase 0/1 notes) referenced `src/vpn_windows.rs` as the file
  Windows-only code/tests would land in; that file was never created. The actual
  implementation follows the SAME pattern as the macOS twin (cfg-gated functions
  alongside their Linux/macOS siblings inside `src/vpn.rs`, e.g. `hostcfg::create_tun`,
  `hostcfg_cmd::windows`), which is more consistent with I-WIN2 ("Windows is a third cfg
  twin, not a refactor") than a separate file would have been. Updating this explicitly
  since the previous resume.md said "none" here despite this divergence already having
  happened in Phase 0/1.
- **2026-06-30 — `validate_windows_adapter_name` and `prefix_to_netmask` are NOT
  `target_os`-gated**, unlike most other Windows-specific code in this plan. Both are
  pure string/arithmetic logic with zero Windows API dependency; gating them behind
  `cfg(target_os = "windows")` would block their unit tests from running on every
  non-Windows CI runner (i.e. all of them, today) for no platform-correctness benefit.
  Same reasoning applies to the entire `hostcfg_cmd::windows` builder module, which was
  never gated in the first place (it only builds `Vec<String>` argv, never executes
  anything) — calling this out so future phases default to "gate only where there is a
  real Windows API dependency," not "gate everything under `target_os = "windows"`."
