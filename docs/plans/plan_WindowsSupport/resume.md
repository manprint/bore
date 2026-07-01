# Windows Support — Resume

> **Next:** Phase 4 is now `DONE` — its single-host correctness was already proven
> (`windows-vpn-build` green, run 28484152255) and the remaining cross-OS two-machine
> rows are, as of today, a permanent manual-acceptance decision rather than an open gap
> (see Decisions below). This session added Phase 5's elevated single-host e2e tier: a
> new `windows-vpn-e2e` CI job on hosted `windows-latest` runs `examples/windows_vpn_spike.rs`
> (WinTun adapter create/teardown, `NetConfig` apply/revert with firewall+WinNAT+
> `IPEnableRouter`, SIGKILL stale-reclaim, two-link `ip_forward` refcount) with ZERO
> elevation ceremony — the hosted runner is already Administrator, resolving the plan's
> original "elevated Windows runner availability" open question for everything that
> doesn't need a second physical machine. Confirmed fully green run:
> [28503204189](https://github.com/manprint/bore/actions/runs/28503204189) (commit
> `90e59f3`), zero revert warnings. Getting there took a real bug hunt — 7 CI iterations,
> 3 confirmed bugs fixed (see Decisions) — because two of the fixes were educated guesses
> that turned out wrong before stderr capture gave the actual root cause. Also bundled the
> official signed `wintun.dll` into every Windows release artifact (pinned version+hash,
> `scripts/fetch_wintun.ps1`) and wrote `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`, the manual
> two-host checklist for everything that genuinely needs a second machine. Phase 3's two
> open items (netmap §2.6, hub spoke isolation D2) are unchanged — still explicitly
> deferred, not touched this session.
> **Last updated:** 2026-07-01

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Compile gates and dependency scaffold | phase_01.md | `DONE` | Windows VPN CLI cfg visible; Windows stub API exports explicit unsupported runtime; local Windows cross-check blocked by missing MSVC tools. |
| 1 — WinTun adapter backend | phase_02.md | `DONE` | `bore-wintun` wrapper crate, `TunDevice` Windows twin, `create_tun` Windows twin (single-queue/no-offload, `BORE_WINTUN_DLL` override), bridge wiring reuses the shared macOS/Windows offload-stub twin. All §1.2 pure-logic unit tests added and green on Linux (ungated `validate_windows_adapter_name`/`prefix_to_netmask` — no Windows API dependency, no reason to block on a Windows runner). §1.5 (DLL packaging/CI download) `DONE` 2026-07-01 via `scripts/fetch_wintun.ps1` (see Phase 6). Elevated adapter e2e: T-WIN-TUN1/TUN4/TUN5 `DONE` 2026-07-01 (see Tests table); T-WIN-TUN2/TUN3 (packet-level TUN I/O) still TODO. |
| 2 — Windows host networking backend | phase_03.md | `IN_PROGRESS` | §2.1 (admin check), §2.2 (routes), §2.3 (ip_forward refcount), §2.4 (forward-accept), §2.5 (plain NAT masquerade), §2.7 (MTU dispatch) implemented + unit-tested. §2.6 (netmap) and the hub-isolation half of §2.8 explicitly DEFERRED (see Open blockers) — not coded, documented as gaps in `docs/vpn/VPN_WINDOWS.md` rather than guessed at. `stale_reclaim` now actually cleans up firewall/NAT leaks (previously a no-op) — and, as of 2026-07-01, actually WORKS on real Windows (the wildcard-match bug that silently defeated it is fixed, see Decisions). Elevated e2e: T-WIN-HOST1/HOST2/HOST3, T-WIN-FWD1/FWD2, T-WIN-NAT1 (partial) `DONE` 2026-07-01; T-WIN-HOST0/HOST4, T-WIN-MTU1 still TODO (need real hardware or a contrived failure, see Tests table). |
| 3 — VPN runtime integration | phase_04.md | `IN_PROGRESS` | Far more was already done than `TODO` suggested: relay/direct/carriers/admin/stale-reclaim-integration/signal-handling are OS-agnostic code with ZERO `target_os` gates (verified by grep across `vpn.rs`/`holepunch.rs`/`link.rs`/`crypto.rs`/`hub.rs`/`routes.rs`/`admin_api.rs`) — they already work on Windows as a side effect of the shared design, not because anyone wired Windows in specifically. `holepunch.rs` already has a Windows-specific UDP socket-buffer variant (`configure_udp_socket_buffers`, `cfg(all(feature="udp", windows))`, pre-existing). `main.rs`'s VPN CLI dispatch was already `any(linux, macos, windows)`-gated. `shutdown_signal` (main.rs) already has a generic `cfg(not(unix))` branch so Ctrl-C works on Windows without SIGTERM. Real gap closed this session: §3.1's CLI-level advisory-warning parity — added `windows_flag_warnings`/`emit_windows_flag_warnings` (only `tun-queues`; deliberately did NOT copy macOS's `holepunch-helpers` warning — no evidence Windows can't do UPnP/STUN/port-prediction, they ride the same cross-platform socket2 code as Linux, see code comment). §3.5 (hub spoke isolation) and the netmap part of §3.6 inherit the Phase 2 deferred gaps — hub mode and gateway mode both RUN on Windows, just without those two specific guarantees, same caveat as before. Nothing else in Phase 3 needed new code. |
| 4 — Non-VPN Windows parity | phase_05.md | `DONE` | Added `windows-vpn-build` job to `.github/workflows/ci.yml` (`runs-on: windows-latest`, mirrors `macos-vpn-build`): `cargo build --features vpn`, `cargo clippy --features vpn --all-targets -- -D warnings`, `cargo test --features vpn` on a real Windows host. This immediately found 3 real, previously-unknown bugs — exactly what this job is for. **1)** `bore-wintun`: unconditional `use anyhow::bail` was genuinely unused on a real Windows build (only used in the `cfg(not(windows))` stub branches) → fixed, gated `cfg(not(windows))`. **2)** 5 pre-existing (predate this session) Windows-only clippy lints in `transfer.rs`/`vpn.rs`, never caught because this was the first time that code ever compiled on real Windows: `only_used_in_recursion` on `scan_entry`'s `devices` param (real on Windows, false positive — genuinely used in the `cfg(unix)` device-file branch — `#[allow]`, not removed), 2× `needless_return`, 2× `Error::new(ErrorKind::Other,_)` → `Error::other(_)`. **3) The big one:** `bore transfer sender --stdin` (and several other transfer CLI paths) crashed the spawned `bore.exe` with `STATUS_STACK_OVERFLOW` (`0xc00000fd`) on Windows — a genuine, previously-unknown, cross-platform correctness bug, not a Windows-only code issue. Root cause: `#[tokio::main] async fn run(...)` is invoked directly from `fn main()` on the OS-provided main thread — Windows' default main-thread stack is 1 MiB vs ~8 MiB on Linux/macOS, and some transfer/stdin async call chain holds enough state to exceed 1 MiB but not 8 MiB. Fixed by running `main()`'s real logic on an explicitly-spawned thread with an 8 MiB stack (no-op on Linux/macOS, the actual fix on Windows). **CONFIRMED FIXED 2026-07-01**: run [28484152255](https://github.com/manprint/bore/actions/runs/28484152255) (commit `0cb5641`) — `windows-vpn-build` passed clean (build + clippy + the FULL `cargo test --features vpn` suite, all `tests/*.rs` files, real `windows-latest`, 33m50s wall time). This is the first time the complete non-VPN integration suite (public/secret/vhost/server/transfer/test-udp-adjacent protocol tests) has ever been proven to pass on real Windows in one process. **Marked `DONE` 2026-07-01**: the only remaining item — literal cross-OS two-machine interop (a real Windows binary talking to a separate real Linux/macOS binary) — is a permanent manual-acceptance decision (see Decisions below), not an open gap; every single-host-provable piece of Phase 4 is proven. |
| 5 — Windows e2e and CI | phase_06.md | `IN_PROGRESS` | **Elevated single-host tier DONE this session**: new `windows-vpn-e2e` job (`.github/workflows/ci.yml`) runs `examples/windows_vpn_spike.rs` on hosted `windows-latest` — WinTun adapter create/teardown, missing-DLL clean failure, route add/del, `NetConfig` apply/revert (firewall + WinNAT + `IPEnableRouter`), gateway-without-`--forward-accept` warn-only path, two-link `ip_forward` refcount, SIGKILL leak-then-reclaim. Answers the plan's original open question empirically: **the hosted runner is already Administrator, no self-hosted runner needed** for this tier — zero elevation ceremony, no `runas`/service-account workaround. Found and fixed 3 real bugs along the way (see Decisions) across 7 CI iterations before landing fully clean: [28503204189](https://github.com/manprint/bore/actions/runs/28503204189) (commit `90e59f3`), zero revert warnings, every firewall-rule/NAT/route/ip_forward assertion exact. **Still `IN_PROGRESS`, not `DONE`**: every T-WIN-VPN-RELAY*/DIRECT*/CARR*/HUB*/HUBD*/GW*/ADMIN1 row (needs a second real machine) and the hosted-CI non-elevated `--forward-accept`-vs-default-Windows-Firewall-policy question (T-WIN-HOST0, T-WIN-HOST4, T-WIN-MTU1) remain — all now tracked as manual rows in `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` rather than left as bare `TODO` with no repro path. |
| 6 — Documentation, packaging, and release hardening | phase_07.md | `IN_PROGRESS` | **WinTun DLL bundling DONE**: `scripts/fetch_wintun.ps1` downloads the official signed WinTun release (pinned `0.14.1`, SHA256-verified), reused by both `windows-vpn-e2e` (places `wintun.dll` next to the compiled example) and `mean_bean_deploy.yml`'s Windows release job (bundles the correct arch — `amd64`/`x86` — into the release zip AND uploads a standalone `wintun-<target>.dll` asset). **Verified in a real release**: tag `windows-ac267d9` — `bore-windows-ac267d9-x86_64-pc-windows-msvc.zip` contains exactly `bore.exe` + `wintun.dll`; `wintun-x86_64-pc-windows-msvc.dll`/`wintun-i686-pc-windows-msvc.dll` present as standalone assets. **`docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` written** (manual two-host checklist, 20 VPN rows + 20 non-VPN cross-OS rows + perf/soak/install/security rows, per the 2026-07-01 manual-acceptance decision). **Stale doc references fixed**: `README.md`/`docs/vpn/VPN.md` said "Windows is deferred"/"Linux only" — both wrong since macOS and now Windows shipped; `docs/INSTALL_BORE.md` had no Windows section at all (the bash installer is Linux/macOS/Android-only). **Still `TODO`**: `T-WIN-INSTALL1`/`T-WIN-PKG1` (fresh-VM manual verification), `T-WIN-SEC1`/`T-WIN-SEC2` (manual security checks), and the final `T-WIN-ACCEPTANCE` sign-off — all need either manual hands-on-a-VM time or the netmap/hub-isolation decisions to close first. |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

**Caveat, updated 2026-07-01 (superseded — kept for history):** earlier revisions of this
doc noted that `ci.yml` (the workflow with the real `cargo test --features vpn` Windows
run) only triggered on push to `main`/`dev`/`macos`, not `windows`, so it had "never run
for this branch." That's no longer true — `ci.yml` has been running (and green) on every
push to `windows` for several sessions now, including every commit in this session's
firewall-revert bug hunt. The dev box itself is still Linux with neither MSVC nor
mingw-w64, so `cfg(target_os = "windows")` code is still written/reviewed blind
locally — but it no longer matters much in practice: `windows-vpn-build` (Phase 4) and
`windows-vpn-e2e` (Phase 5, new this session) both run the real thing on hosted
`windows-latest` on every push, so compile errors and runtime bugs alike get caught
within one CI cycle (~10-20 min), not at merge time.

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| `cargo_check_windows_vpn_cfg` | compile | `DONE` | Real `windows-latest` MSVC build (`mean_bean_deploy.yml`, `--all-features --release`) succeeded 2026-07-01 for both `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` (run 28479476516). `ci.yml`'s `windows-vpn-build`/`windows-vpn-e2e` jobs now run (and pass) on every `windows` branch push — see run 28503204189. |
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
| `test_windows_ipforward_refcount_keeps_forwarding_for_other_link` | e2e (reclassified) | `DONE` | Not unit-testable (needs Windows execution), but now proven end-to-end: `windows_vpn_spike two-link-refcount` creates two real gateway links, drops the first while the second is still active (asserts `IPEnableRouter` stays `1`), then drops the second (asserts restore to the original value). Run [28503204189](https://github.com/manprint/bore/actions/runs/28503204189), clean. |
| `test_windows_nat_masquerade_scopes_plain_subnets_only` | unit | `DONE` | `windows_plain_subnets_excludes_netmap_reals`. |
| `test_cmd_windows_firewall_allow_tun_to_lan_snapshot` | unit | `DONE` | `cmd_windows_firewall_forward_accept_direction_snapshots`. |
| `test_cmd_windows_firewall_allow_lan_to_tun_snapshot` | unit | `DONE` | Same test. |
| `test_cmd_windows_firewall_delete_group_snapshot` | unit | `DONE` | `cmd_windows_firewall_delete_for_link_uses_wildcard_prefix`. |
| `test_windows_forward_accept_rule_names_unique` | unit | `DONE` | Covered structurally by `link_prefix` (per-`(id,role)` prefix) + per-direction `in`/`out` suffix; see `cmd_windows_link_prefix_sanitizes_id_and_role`. |
| `test_windows_forward_accept_off_warns_only` | e2e (reclassified) | `DONE` | Not unit-testable (the warn is inside the `cfg(windows)` `apply()` body), but proven via `windows_vpn_spike forward-accept-off-warn`: applies a gateway link WITHOUT `--forward-accept`, confirms the warn log fires AND zero firewall rules are added (scoped to that link's own id, not just a raw total). Run 28503204189, clean. |
| `test_cmd_windows_nat_masquerade_add_snapshot` | unit | `DONE` | `cmd_windows_hostcfg_phase2_builders_snapshot`. |
| `test_cmd_windows_nat_masquerade_del_snapshot` | unit | `TODO` | `cmd_nat_del` itself is unchanged/untested-by-name (low risk, trivial argv); add if a reviewer wants it explicit. |
| `test_windows_nat_names_unique_per_link` | unit | `DONE` | `cmd_windows_nat_delete_for_link_uses_wildcard_prefix` exercises the same `link_prefix`-based naming. |
| `test_windows_netmap_*` | unit | `BLOCKED` | §2.6 deferred (see Open blockers) — no netmap backend exists to test. |
| `test_windows_netconfig_apply_records_reverts_lifo` | e2e (reclassified) | `DONE` | `apply()`/`Drop()` are `cfg(windows)`-gated (can't unit-test on Linux), but proven for real: `windows_vpn_spike apply-revert` applies a gateway link (forward-accept + nat-masquerade), confirms firewall rules + WinNAT instance + `IPEnableRouter` all present, drops it, confirms all three fully reverted (rules=0, NAT gone, forwarding restored). Took 3 real-bug fixes to get clean (see Decisions) — final clean run 28503204189. |
| `test_windows_stale_reclaim_removes_firewall_nat_routes` | e2e (reclassified) | `DONE` | `stale_reclaim()` body is `cfg(windows)`-gated, but proven via `windows_vpn_spike leak-then-reclaim`: leaks a link (`std::mem::forget`, simulating SIGKILL), then a separate process run calls `stale_reclaim` and confirms 0 firewall rules + `IPEnableRouter` restored. This is what surfaced the confirmed real bug in `cmd_firewall_delete_for_link`'s wildcard matching (see Decisions) — now genuinely fixed, run 28503204189 shows a clean 0. |
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
| T-WIN-TUN1 | e2e | `DONE` | `windows_vpn_spike create-teardown`: real WinTun adapter created, confirmed visible via `Get-NetAdapter`, dropped, confirmed gone. Run 28503204189. |
| T-WIN-TUN2 | e2e | `TODO` | Inject packet into TUN and bridge receives exact bytes — needs packet-level I/O the spike doesn't exercise (adapter create/config only), not covered this session. |
| T-WIN-TUN3 | e2e | `TODO` | Bridge writes packet to TUN and host observes it — same gap as T-WIN-TUN2. |
| T-WIN-TUN4 | e2e | `DONE` | Covered by the same `create-teardown` run as T-WIN-TUN1 (adapter create/release lifecycle); the literal `bore vpn --relay-only --no-route-manage` CLI invocation itself wasn't run (the spike calls `hostcfg::create_tun` directly, mirroring the macOS spike's precedent), but the underlying lifecycle it would exercise is proven. |
| T-WIN-TUN5 | e2e | `DONE` | `windows_vpn_spike missing-dll`: `WintunDevice::open_or_create` with a bogus DLL path fails cleanly before any adapter mutation. Run 28503204189. |
| `test_windows_hostcfg_*` | unit | `TODO` | Admin, state paths, route/forward/firewall/NAT builders. |
| T-WIN-HOST0 | e2e | `TODO` | Non-admin fails before side effects. Cannot be demonstrated from `windows-vpn-e2e`, which runs elevated by design — needs a deliberately non-elevated process on real hardware, see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` §A19. |
| T-WIN-HOST1 | e2e | `DONE` | `windows_vpn_spike route-add-del`: real peer route added to a real adapter, confirmed visible via `Get-NetRoute`, adapter+route dropped together. Run 28503204189. |
| T-WIN-HOST2 | e2e | `DONE` | `windows_vpn_spike two-link-refcount`, see the unit-test-table entry above (same evidence). |
| T-WIN-HOST3 | e2e | `DONE` | `windows_vpn_spike leak-then-reclaim`, see the unit-test-table entry above (same evidence). |
| T-WIN-HOST4 | e2e | `TODO` | Apply failure rolls back prior ops — needs a contrived mid-apply failure (e.g. a permission or interface error partway through), not attempted this session; see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` §A20. |
| T-WIN-FWD1 | e2e | `DONE` | `windows_vpn_spike forward-accept-off-warn`: gateway mode without `--forward-accept` logs the warn and adds zero firewall rules (checked against the link's own id, not a raw host-wide count). The literal "does Windows Defender Firewall actually BLOCK the traffic" half stays a documented open platform question (see Open blockers) — this row proves the warn-only code path, not the blocking claim. Run 28503204189. |
| T-WIN-FWD2 | e2e | `DONE` | `windows_vpn_spike apply-revert`: forward-accept firewall rules created (2, tun→lan + lan→tun) and fully reverted (0) on drop. This took real debugging — 3 confirmed bugs fixed across 7 CI iterations (see Decisions) before landing clean. Final run 28503204189, zero revert warnings. |
| T-WIN-NAT1 | e2e | `DONE` (partial) | `windows_vpn_spike apply-revert` also confirms the WinNAT instance is created (`Get-NetNat`) and removed on revert. What's NOT covered: real cross-host traffic actually flowing through the NAT'd path — that needs a second machine, see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` §A18-adjacent content (masquerade return path). |
| T-WIN-VPN-NAT | e2e | `BLOCKED` | Netmap backend itself is deferred (D7 §2.6, unchanged) — nothing to test yet. |
| T-WIN-MTU1 | e2e | `TODO` | Large TCP through Windows gateway succeeds at MTU 1350 — needs real gateway traffic (a second machine), see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` §A18. |
| T-WIN-MTU2 | e2e | `DONE` (structural, no hardware needed) | `pmtu_monitor`'s `pin` branch (`vpn.rs`) `continue`s immediately after its one-time log line and NEVER calls `pmtu_link_set_mtu_argv` (the per-OS MTU-set dispatch) on ANY platform when `pin=true` — this is a pure code-path fact verifiable by reading the function, not something that needs a live QUIC connection or Windows hardware to confirm. Same reasoning already applied to several macOS/shared-code rows elsewhere in this table. |
| T-WIN-VPN0 | e2e | `TODO` | Non-admin VPN exits before side effects — same gap as T-WIN-HOST0 (needs a real non-elevated process on hardware). |
| T-WIN-VPN1 | e2e | `TODO` | Admin parse/help path exposes VPN subcommands — trivial CLI check, not exercised this session (the spike calls library functions directly, not the `bore` CLI binary). |
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
| T-WIN-STALE1 | e2e | `DONE` | `windows_vpn_spike leak-then-reclaim`: kill-then-reclaim on ONE link, see the unit-test-table entry above (same evidence). |
| T-WIN-STALE2 | e2e | `TODO` | Reclaim one dead link does not break other live link — NOT the same scenario as `two-link-refcount` (both links there die via normal `Drop`, not one killed while the other stays live via `stale_reclaim`); not attempted this session, left honestly TODO rather than claimed via an adjacent-but-different test. |
| T-WIN-ADMIN1 | e2e | `TODO` | Windows VPN admin status and live TX/RX — needs a real link + the admin panel, a second-machine scenario; see `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` §A17. |
| *(note)* | — | — | The single-host correctness these T-WIN-LOCAL/SECRET/VHOST/SERVER/TRANSFER/UDPTEST rows depend on IS now proven: `windows-vpn-build` (run 28484152255, 2026-07-01) runs `cargo test --features vpn` — every `tests/*.rs` file, client+server/provider+consumer both compiled and executing on real `windows-latest` in one process — and it passes. What remains TODO below is specifically the LITERAL cross-OS two-machine scenario (a real Windows binary talking to a separate real Linux/macOS binary over the network) or admin-panel-visual checks, neither of which a single-host CI job can prove even though the wire protocol is byte-identical and already Linux-validated. **2026-07-01 decision**: these stay manual-acceptance forever, not just "TODO until CI catches up" — no self-hosted runner or cross-runner tunnel infra was built (see Decisions below). Every row below has an exact repro command in `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`. |
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
| `docs/vpn/VPN_WINDOWS.md` | `DONE` | D-WT1 + implementation-status section (what's implemented, what's explicitly deferred and why, what's CI/hardware-verified-only). **2026-07-01 update**: added the WinTun DLL redistribution decision + rationale, the Phase 5.4 elevated single-host e2e writeup (spike, CI job, the bug it found and fixed), and pointed the remaining gaps at `VPN_WINDOWS_ACCEPTANCE.md` for the manual cross-OS matrix instead of a vague "needs hardware" note. |
| `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` | `DONE` | Written 2026-07-01: 20 VPN two-host rows (relay/direct/carriers/hub/gateway/admin) + 20 non-VPN cross-OS rows (local/secret/vhost/server/transfer/test-udp) + perf/soak/install/packaging/security rows, each with an exact command and expected observation, plus a blank result-log table to fill in once real hardware is available. |
| `docs/vpn/VPN.md` | `DONE` | Was stale for BOTH macOS and Windows (predated either shipping): title/intro said "Linux Point-to-Point L3 Tunnel", platform table showed macOS/Windows as "📐 Groundwork", Requirements said "Linux only". Fixed to reflect current status (Linux/macOS/Windows all ✅, netmap+hub-isolation gaps called out specifically for Windows) and linked `VPN_WINDOWS_ACCEPTANCE.md`/`VPN_MACOS_ACCEPTANCE.md`. |
| `docs/INSTALL_BORE.md` | `DONE` | Had no Windows section at all — the documented install script is bash-only (Linux/macOS/Android). Added a Windows section: manual release-zip install steps, the `wintun.dll`-must-stay-next-to-`bore.exe` requirement, the elevation requirement for `bore vpn`, and `scripts/fetch_wintun.ps1` for building from source. |
| `README.md` | `DONE` | Fixed two stale spots: the VPN section heading/intro said "(Linux + macOS)" and "Windows is deferred" in the feature bullet list — both wrong since Windows VPN shipped this session. Added the WinTun DLL note and links to both acceptance docs. |

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
- **`--forward-accept`'s effect on ROUTED (non-host-bound, i.e. actually transiting the
  gateway to a different destination) traffic is still unverified.** Windows Defender
  Firewall's standard rule model is host-bound-traffic-oriented; whether
  `New-NetFirewallRule` actually filters merely-transiting packets the way Linux
  iptables/macOS PF do is an open platform question. **Narrowed 2026-07-01**: T-WIN-FWD1/
  T-WIN-FWD2's SINGLE-HOST half is now resolved (the rules are created and revert
  correctly, proven on real `windows-latest` — see Tests table) — what's still open is
  specifically whether they BLOCK anything for real, which needs actual routed traffic
  through the gateway (a second machine). Tracked as `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`
  §A12/§A18.
- ~~Elevated Windows runner availability determines whether VPN e2e is automated or
  manual at first landing.~~ **RESOLVED 2026-07-01**: hosted `windows-latest` IS
  sufficiently privileged for every single-host elevated operation this plan needs
  (WinTun adapter create, firewall rules, WinNAT, `IPEnableRouter` registry write, route
  add/del) — no self-hosted runner, no explicit elevation step, no service-account
  workaround. See the `windows-vpn-e2e` job and Decisions below. Only the LITERAL
  cross-machine two-host scenarios still need real hardware, and that's now a permanent
  manual-acceptance decision, not an availability blocker to solve.
- Local Linux dev box has no MSVC and no mingw-w64 (no passwordless `sudo apt-get`), so
  none of the `cfg(target_os = "windows")` code in this repo has been compiled, let alone
  run, locally — every fix this session was written blind and verified entirely through
  CI round-trips (7 iterations for the firewall-revert bug alone). Installing mingw-w64
  (`sudo apt-get install -y mingw-w64`) would at least unblock local
  `cargo check --target x86_64-pc-windows-gnu` (no linker needed for `check`) — still
  useful for catching Rust-level scope/type errors (like the `HashSet`/`hostcfg_cmd`
  path mistakes this session hit) before spending a CI cycle on them, though it can't
  catch PowerShell-argv-level bugs (the real bulk of this session's bug hunt), which
  need actual Windows execution regardless.
- **Cross-machine two-host access remains the sole blocker for full Windows VPN
  acceptance.** Every row in `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` needs a human with a
  real Windows machine and a real Linux/macOS peer — no CI rig exists or is planned for
  this (see Decisions: the public-tunnel-CI-rig option was explicitly declined).

## Decisions changed at runtime

- **2026-07-01 — WinTun DLL is bundled in release artifacts, not left for users to
  download.** WinTun's own site states "the below signed DLLs are the only supported way
  of distributing Wintun" — redistribution of the unmodified signed binary is the
  expected path. Pinned version `0.14.1` + SHA256 in `scripts/fetch_wintun.ps1`, reused by
  both the `windows-vpn-e2e` CI job and `mean_bean_deploy.yml`'s release packaging.
  Verified in a real release (tag `windows-ac267d9`): the zip contains `bore.exe` +
  `wintun.dll` side by side, plus a standalone `wintun-<target>.dll` asset for raw-`.exe`
  users. User was asked directly (bundle vs. user-downloads) rather than assumed.
- **2026-07-01 — Cross-OS two-machine tests are manual-acceptance only; no CI tunnel rig
  was built.** Considered and explicitly declined: a public-tunnel CI rig (e.g. an
  ephemeral Cloudflare quick tunnel between a `windows-latest` job and an `ubuntu-latest`
  job) could have automated T-WIN-VPN-RELAY*/SECRET*/VHOST*/SERVER*/etc., but it adds a
  new moving part (a live bore server briefly exposed to the public internet each CI run,
  a third-party service dependency, more flake surface) that's disproportionate for a
  first pass. User was asked directly and chose the manual-acceptance path; exact repro
  commands + a blank result log for every affected row live in
  `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md`.
- **2026-07-01 — Hosted `windows-latest` is sufficiently privileged for single-host
  elevated e2e; no self-hosted runner needed for that tier.** The original plan (Open
  blockers, now resolved) assumed elevated Windows CI would need a self-hosted admin
  runner, mirroring how macOS's `sudo`-gated e2e job works. Empirically wrong for
  Windows: the `windows-vpn-e2e` job creates WinTun adapters, firewall rules, WinNAT
  instances, and writes the `IPEnableRouter` registry value directly, with zero
  elevation ceremony (no `runas`, no service account) — the hosted runner's default
  process token is already Administrator. This is a genuinely different answer than
  macOS's (which does need `sudo`), found by just trying it rather than assuming parity.
- **2026-07-01 — Bug hunt: 3 confirmed real bugs found and fixed, one process lesson
  learned the hard way.** Building `examples/windows_vpn_spike.rs` and the
  `windows-vpn-e2e` CI job surfaced real, previously-unverifiable-without-hardware bugs:
  1. **`create_tun`'s `"auto"` adapter-name resolution never checked for existing
     adapters** — a hardcoded `|_| false` existence predicate meant `"auto"` always
     resolved to `bore0` regardless of what already existed, so two concurrent `bore vpn`
     links on one Windows host would silently share/reconfigure the SAME WinTun adapter.
     Fixed by querying `Get-NetAdapter` once per `create_tun` call, mirroring Linux's real
     `/sys/class/net` check.
  2. **`stale_reclaim`'s firewall wildcard-delete never matched anything.**
     `cmd_firewall_delete_for_link` used `-DisplayName '<prefix>*'`, but
     `Get-NetFirewallRule -DisplayName` does exact literal matching only — a `*` is not a
     wildcard there. Every `stale_reclaim` run silently left every rule behind. Fixed by
     switching to `Where-Object -like` on the piped objects, mirroring the pattern
     `cmd_nat_delete_for_link` already used correctly for `Get-NetNat`.
  3. **`cmd_firewall_delete`'s `Get-NetFirewallRule -Group X -DisplayName Y` combination
     is `AmbiguousParameterSet`** — an invalid parameter combination for that cmdlet,
     throwing 100% of the time. This is the one that took real iteration to nail down: two
     earlier fixes (`-Confirm:$false` on `Remove-NetFirewallRule`, `-ErrorAction
     SilentlyContinue` on the lookup) were reasonable, defensible guesses based on how the
     symptom LOOKED (a rule created moments earlier appearing not-yet-visible, which read
     like a propagation-lag race) — both landed, neither fixed it, because the actual cause
     was a parameter-set error that neither guess addressed. A bounded retry loop was added
     as a stopgap (and is a reasonable resilience improvement regardless, kept, but scoped
     to Windows only after it measurably slowed the Linux/macOS unit test suite ~5×). The
     real fix only became obvious after adding a diagnostic mode
     (`windows_vpn_spike diag-firewall`) that printed raw stdout/stderr, and separately
     after capturing stderr on the production retry loop's final failed attempt — which is
     what actually surfaced the `AmbiguousParameterSet` error text. **Process lesson,
     recorded honestly**: two blind guesses at a Windows-specific PowerShell-flag fix is
     the right number to try before adding real diagnostics (capturing stderr, building an
     isolated repro) — a third guess without new data would have been the wrong move, and
     wasn't attempted. Final fix: drop the redundant `-Group` filter (`-DisplayName` alone
     is already exact/unique per `link_prefix`). Confirmed fully clean, zero revert
     warnings, in run [28503204189](https://github.com/manprint/bore/actions/runs/28503204189)
     (commit `90e59f3`), after 7 total CI iterations across this one investigation.
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
