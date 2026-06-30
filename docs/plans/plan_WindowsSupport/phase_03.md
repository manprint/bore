# Phase 2 — Windows host networking backend

> **Intent:** Implement Windows route, IP forwarding, firewall, NAT/masquerade, overlapping-subnet netmap, MTU/MSS, and stale-reclaim behavior behind `NetConfig`.
> **Shippable alone?** yes — Windows VPN host configuration can be applied/reverted independently; data plane integration comes next.
> **Preconditions:** phase_02 DONE

---

## Sub-phases

### 2.1 Add Windows privilege and command runner probes
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:2361`, `src/vpn.rs:3261`, `src/vpn.rs:4384`, `src/main.rs:1669`, `src/main.rs:1737`
- **Change:** Add Windows admin check near existing VPN preflight. Required behavior: non-admin `bore vpn listen|connect` fails before creating WinTun adapter or modifying host config; error names exact remediation: run elevated PowerShell/CMD. Add Windows command builders for structured PowerShell invocation and `netsh` fallback inside `hostcfg_cmd::windows`. Keep `CommandRunner`/`RealRunner` reused; do not introduce shell-string concatenation. All commands must be argv arrays or PowerShell `-NoProfile -NonInteractive -Command <script>` with escaped values from sanitized inputs only.
- **Unit tests:** `test_windows_admin_check_error_message`; `test_windows_powershell_argv_no_shell_concat`; `test_windows_sanitize_rule_name_rejects_control_chars`; `test_windows_sanitize_adapter_name_rejects_metacharacters`.
- **e2e tests:** T-WIN-HOST0 — non-admin Windows run of `bore vpn listen` fails before adapter creation; verify no WinTun adapter, routes, firewall rules, or state files exist after failure.
- **Done:** Non-admin error deterministic; every Windows command builder snapshot contains no unescaped user-provided shell fragments.

### 2.2 Implement Windows route and MTU configuration
- **Model:** Haiku 4.5
- **Files:** `src/vpn.rs:3261`, `src/vpn.rs:3451`, `src/vpn.rs:4406`
- **Change:** Complete Windows route/MTU builders already stubbed at `hostcfg_cmd::windows`. Required commands: add route for each accepted peer route to TUN interface, delete route, set interface MTU, optional set interface metric if needed to prefer TUN routes. Use interface name or interface index/LUID consistently from Phase 1. Existing `cmd_route_add`, `cmd_route_del`, `cmd_link_set_mtu` snapshots are baseline; fix them if Windows syntax requires interface index instead of name. Keep route management skipped when `--no-route-manage` is set.
- **Unit tests:** `test_cmd_windows_route_add_snapshot`; `test_cmd_windows_route_del_snapshot`; `test_cmd_windows_set_mtu_snapshot`; `test_windows_no_route_manage_skips_routes`; `test_windows_routes_use_resolved_interface_name`.
- **e2e tests:** T-WIN-HOST1 — apply config for route `10.99.0.0/24`, verify `Get-NetRoute` or `route print` shows route through WinTun adapter; Drop removes route.
- **Done:** Route add/delete idempotent; Drop reverses in LIFO order using `revert_cmds`; Linux/macOS route builders unchanged.

### 2.3 Implement Windows IP forwarding save/enable/restore with refcount
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4296`, `src/vpn.rs:4332`, `src/vpn.rs:4346`, `src/vpn.rs:4384`, `src/vpn.rs:4406`, `src/vpn.rs:4042`
- **Change:** Add Windows equivalents for forwarding state. Use `%ProgramData%\bore\vpn\state\` via a Windows `run_dir()` twin or dedicated helper; create directory with restrictive permissions if possible. Save original forwarding state once, create per-`(id,role)` refcount marker, enable forwarding for gateway modes, restore only when no other bore Windows gateway marker remains. Preferred state source: PowerShell/NetTCPIP or registry value `IPEnableRouter`; pick one and document in `VPN_WINDOWS.md`. Reuse Linux/macOS refcount model; Windows has one host namespace, so no netns inode. Atomic write state files.
- **Unit tests:** `test_windows_run_dir_programdata`; `test_windows_ipforward_state_path_sanitized`; `test_windows_ipforward_refcount_keeps_forwarding_for_other_link`; `test_windows_ipforward_restore_last_link`; `test_windows_ipforward_state_atomic_write`.
- **e2e tests:** T-WIN-HOST2 — two gateway links in one Windows host: stopping first keeps forwarding enabled; stopping second restores original value. Repeat with killed process and stale reclaim.
- **Done:** Refcount semantics match Linux/macOS; no global forwarding disable while another bore link is live.

### 2.4 Implement Windows firewall and `--forward-accept`
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:2651`, `src/vpn.rs:2714`, `src/vpn.rs:3261`, `src/vpn.rs:4406`, `src/vpn.rs:4042`
- **Change:** Add Windows firewall builders under `hostcfg_cmd::windows`. Default behavior: if gateway mode needs forwarding and `--forward-accept` is not set, probe Windows firewall/profile state and warn with exact remediation when traffic may be blocked. With `--forward-accept`, add per-link allow rules for TUN↔LAN directions using a unique group/name such as `bore-vpn/<sanitized-id>/<role>`. Drop and stale reclaim remove exact rules only. Rules must match interface aliases and remote/local CIDRs as tightly as Windows firewall supports. Do not disable firewall globally.
- **Unit tests:** `test_cmd_windows_firewall_allow_tun_to_lan_snapshot`; `test_cmd_windows_firewall_allow_lan_to_tun_snapshot`; `test_cmd_windows_firewall_delete_group_snapshot`; `test_windows_forward_accept_rule_names_unique`; `test_windows_forward_accept_off_warns_only`.
- **e2e tests:** T-WIN-FWD1 — Windows gateway with default-deny firewall blocks LAN host behind gateway without `--forward-accept` and logs warning; T-WIN-FWD2 — same setup with `--forward-accept` allows remote peer to reach LAN host; Drop removes rules.
- **Done:** `--forward-accept` parity with Linux/macOS behavior; firewall rules scoped and cleaned.

### 2.5 Implement Windows NAT masquerade for plain advertised subnets
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:2924`, `src/vpn.rs:3007`, `src/vpn.rs:3261`, `src/vpn.rs:4406`, `docs/vpn/VPN_NAT_ASSESSMENT.md`, `docs/vpn/VPN_WINDOWS.md` (new)
- **Change:** Add Windows NAT masquerade backend for plain `--advertise CIDR` when `--nat-masquerade` is set and gateway is not LAN router. Preferred built-in path: WinNAT/PowerShell `New-NetNat` with per-link names. Required semantics: NAT only plain subnets, not `real@virtual` netmap subnets; remove exact NAT instance/rule on Drop; stale reclaim removes leaked per-link NAT. If Windows built-in NAT cannot scope exactly, Opus review must choose safe alternative before coding.
- **Unit tests:** `test_cmd_windows_nat_masquerade_add_snapshot`; `test_cmd_windows_nat_masquerade_del_snapshot`; `test_windows_nat_masquerade_scopes_plain_subnets_only`; `test_windows_nat_names_unique_per_link`.
- **e2e tests:** T-WIN-NAT1 — Windows gateway not default LAN router advertises plain LAN with `--nat-masquerade`; remote peer reaches LAN host and return path works; Drop removes NAT.
- **Done:** Plain gateway NAT parity works; no NAT applies to virtual/netmap subnets.

### 2.6 Opus netmap feasibility gate and implementation path
- **Model:** Opus 4.8 design review → Sonnet implements
- **Files:** `src/vpn.rs:2857`, `src/vpn.rs:2871`, `src/vpn.rs:2900`, `src/vpn.rs:2952`, `src/vpn.rs:2979`, `src/vpn.rs:3261`, `src/vpn.rs:4406`, `docs/vpn/VPN_NAT_ASSESSMENT.md`, `docs/vpn/VPN_WINDOWS.md` (new)
- **Change:** Decide and implement Windows `real@virtual` overlapping-subnet NAT. Required semantics from existing invariant: stateless 1:1 prefix translation, host bits preserved, no Rust packet rewrite, identical relay/direct behavior, virtual CIDRs only on wire. Evaluate Windows built-in options first. If built-ins cannot express prefix netmap, choose one supported hostcfg backend: Windows Filtering Platform callout/helper, WinDivert-based helper, or explicit documented unsupported status. The goal says all features supported; therefore Phase 2 cannot be complete until a supported backend passes tests. Implementation must remain kernel/host-networking side, not bore data-plane packet rewriting, unless Opus explicitly revises I-WIN9 and documents why.
- **Unit tests:** `test_windows_netmap_dnat_builder_preserves_prefix`; `test_windows_netmap_snat_builder_preserves_prefix`; `test_windows_netmap_rejects_prefix_mismatch`; `test_windows_netmap_virtuals_only_overlap_check`; `test_windows_masquerade_excludes_netmap_subnets`.
- **e2e tests:** T-WIN-VPN-NAT — two Windows/Linux LANs both use `192.168.1.0/24`, advertise as `192.168.1.0/24@10.201.1.0/24` and `192.168.1.0/24@10.202.1.0/24`; peers reach virtual addresses; host bits preserved; real subnets never appear on wire/admin protocol; Drop removes rules.
- **Done:** T-WIN-VPN-NAT passes, or plan is explicitly blocked. Do not mark Windows VPN complete without this test.

### 2.7 Implement Windows MSS/MTU handling
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:2522`, `src/vpn.rs:2754`, `src/vpn.rs:3261`, `src/vpn.rs:4406`, `src/vpn.rs:7562`
- **Change:** Provide Windows equivalent for gateway MSS clamp or prove Windows path does not require explicit clamp with TUN MTU 1350. If using firewall/WFP/netsh supports rule, add per-link rule and cleanup. If not supported, document limitation and rely on TUN MTU plus route MTU, but add e2e that large TCP transfer behind gateway does not black-hole. PMTU monitor remains shared and unchanged except Windows route/MTU command calls.
- **Unit tests:** `test_cmd_windows_mtu_set_snapshot`; `test_windows_mss_clamp_rule_snapshot` if backend exists; `test_windows_pmtu_pin_warns_without_resize`.
- **e2e tests:** T-WIN-MTU1 — large TCP transfer through Windows gateway succeeds with default MTU 1350; T-WIN-MTU2 — `--pin-mtu` logs observe-only warning on TooLarge path and never changes interface MTU.
- **Done:** Large TCP through Windows VPN does not stall/black-hole; PMTU tests still pass on Linux/macOS.

### 2.8 Implement Windows `NetConfig::apply`, `Drop`, and `stale_reclaim`
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4042`, `src/vpn.rs:4384`, `src/vpn.rs:4406`, `src/vpn.rs:5081`
- **Change:** Replace Windows unsupported stubs with real `NetConfig::apply` and Drop twin. Apply order: stale reclaim for same id/role; interface address/MTU if not done by WinTun; route add; forwarding save/enable if gateway; firewall/forward-accept; NAT/netmap; MSS/MTU rule; record revert commands/labels after each successful mutation. Drop order: reverse `revert_cmds`; restore forwarding only if no other refcount marker; remove state files. `stale_reclaim(id, role)` must remove leaked routes/rules/NAT/netmap where possible by id/role, restore forwarding if last marker, and be idempotent. Follow existing `NetConfig` fields; add Windows-specific fields only if unavoidable.
- **Unit tests:** `test_windows_netconfig_apply_records_reverts_lifo`; `test_windows_netconfig_drop_idempotent`; `test_windows_stale_reclaim_removes_firewall_nat_routes`; `test_windows_stale_reclaim_refcount_aware`; `test_windows_apply_failure_rolls_back_prior_ops`.
- **e2e tests:** T-WIN-HOST3 — kill bore after apply; next run stale-reclaims routes/firewall/NAT/forwarding; T-WIN-HOST4 — apply failure midway rolls back prior Windows host mutations.
- **Done:** Host config apply/revert/stale reclaim robust under success, failure, Ctrl-C, and process kill. Linux/macOS hostcfg behavior unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test subset:** `cargo test --features vpn windows_hostcfg windows_netconfig windows_nat windows_firewall -- --nocapture`
- **Cross-check:** `cargo check --features vpn --target x86_64-pc-windows-msvc`
- **Elevated Windows check:** T-WIN-HOST0 through T-WIN-HOST4, T-WIN-FWD1/2, T-WIN-NAT1, T-WIN-VPN-NAT, T-WIN-MTU1/2
- **Regression guard:** Linux netns tests and macOS PF snapshots remain green; no Linux/macOS body changes except shared helper additions reviewed by Opus

## Phase done criterion

Phase 2 is done when Windows can apply, verify, revert, and stale-reclaim all host networking needed for VPN parity, including route management, forwarding, firewall forward accept, plain NAT masquerade, overlapping-subnet 1:1 netmap, and MTU/MSS behavior.
