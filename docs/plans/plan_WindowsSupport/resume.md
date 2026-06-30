# Windows Support — Resume

> **Next:** phase_04.md § 3.x — VPN runtime integration (relay/direct/carriers/hub/gateway/admin); Phase 2 remainder is the netmap (§2.6) and hub-isolation backend decisions, both explicitly deferred — see Open blockers.
> **Last updated:** 2026-06-30

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Compile gates and dependency scaffold | phase_01.md | `DONE` | Windows VPN CLI cfg visible; Windows stub API exports explicit unsupported runtime; local Windows cross-check blocked by missing MSVC tools. |
| 1 — WinTun adapter backend | phase_02.md | `DONE` | `bore-wintun` wrapper crate, `TunDevice` Windows twin, `create_tun` Windows twin (single-queue/no-offload, `BORE_WINTUN_DLL` override), bridge wiring reuses the shared macOS/Windows offload-stub twin. All §1.2 pure-logic unit tests added and green on Linux (ungated `validate_windows_adapter_name`/`prefix_to_netmask` — no Windows API dependency, no reason to block on a Windows runner). §1.5 (DLL packaging/CI download) still TODO. Elevated adapter e2e (T-WIN-TUN1-5) still TODO — needs real hardware. |
| 2 — Windows host networking backend | phase_03.md | `IN_PROGRESS` | §2.1 (admin check), §2.2 (routes), §2.3 (ip_forward refcount), §2.4 (forward-accept), §2.5 (plain NAT masquerade), §2.7 (MTU dispatch) implemented + unit-tested. §2.6 (netmap) and the hub-isolation half of §2.8 explicitly DEFERRED (see Open blockers) — not coded, documented as gaps in `docs/vpn/VPN_WINDOWS.md` rather than guessed at. `stale_reclaim` now actually cleans up firewall/NAT leaks (previously a no-op). Elevated e2e (T-WIN-HOST*, T-WIN-FWD*, T-WIN-NAT1, T-WIN-MTU*) still TODO. |
| 3 — VPN runtime integration | phase_04.md | `TODO` | Relay/direct/carriers/hub/gateway/admin. |
| 4 — Non-VPN Windows parity | phase_05.md | `TODO` | Public, secret, vhost, server, transfer, test-udp. |
| 5 — Windows e2e and CI | phase_06.md | `TODO` | Hosted + elevated/manual matrix. |
| 6 — Documentation, packaging, and release hardening | phase_07.md | `TODO` | Docs, artifacts, security, release sign-off. |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

**Caveat on every `DONE`/`IN_PROGRESS` Windows item above:** none of it has run on real
Windows hardware. The dev environment is Linux with neither MSVC (`ml64.exe`/`lib.exe`)
nor a mingw-w64 toolchain installed (no passwordless sudo for `apt-get install
mingw-w64`), so `target_os = "windows"` code can only be statically reviewed here, not
compiled or executed. Code that has NO Windows API dependency (pure string/argv
builders, validation, parsing — the bulk of `hostcfg_cmd::windows` plus
`validate_windows_adapter_name`/`prefix_to_netmask`) was deliberately left un-gated by
`target_os` so it compiles and unit-tests on every CI runner today; only the bodies that
actually call WinTun/PowerShell/registry APIs remain CI/hardware-verified-only, same as
before this session. If real local cross-compilation is wanted, install mingw-w64
(`sudo apt-get install -y mingw-w64`) — `cargo check --target x86_64-pc-windows-gnu
--features vpn` then exercises the `cfg(target_os = "windows")` bodies without needing a
linker.

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| `cargo_check_windows_vpn_cfg` | compile | `BLOCKED` | Local Linux host lacks MSVC AND mingw-w64; Windows CI runner must verify. `cargo check --target x86_64-pc-windows-gnu --features vpn` would unblock locally if mingw-w64 is installed (no passwordless sudo here). |
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
