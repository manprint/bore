# Phase 3 — VPN runtime integration

> **Intent:** Enable full `bore vpn listen|connect` runtime on Windows using shared VPN data plane and Windows host backend.
> **Shippable alone?** yes — Windows VPN works for relay/direct/core modes; non-VPN parity is separate Phase 4.
> **Preconditions:** phase_03 DONE

---

## Sub-phases

### 3.1 Wire Windows VPN CLI/runtime preflight
- **Model:** Sonnet 4.6
- **Files:** `src/main.rs:931`, `src/main.rs:1669`, `src/main.rs:1737`, `src/vpn.rs:487`, `src/vpn.rs:513`, `src/vpn.rs:544`, `src/vpn.rs:588`, `src/vpn.rs:608`
- **Change:** Make Windows VPN CLI behave like Linux/macOS with Windows-specific warnings. Required warnings: `--tun-queues > 1` unsupported -> clamp to 1; Linux/macOS-only UDP helper flags warn if not meaningful on Windows; missing elevation error before side effects; missing `wintun.dll` error before side effects. Keep CLI argument names/defaults unchanged. Do not add Windows-only flags unless required for WinTun DLL path; prefer env var `BORE_WINTUN_DLL` plus docs.
- **Unit tests:** `test_windows_vpn_cli_tun_queues_warns`; `test_windows_vpn_cli_requires_admin_before_wintun`; `test_windows_vpn_cli_missing_dll_error`; `test_windows_nat_udp_flags_warn_not_silent`.
- **e2e tests:** T-WIN-VPN0 — non-admin Windows CLI exits before side effects; T-WIN-VPN1 — admin CLI with `--help` and dry run/parsing path shows VPN subcommands.
- **Done:** Windows CLI exposes VPN subcommands under `--features vpn`; Linux/macOS help output unchanged except accepted cfg expansion where expected.

### 3.2 Relay-only 1:1 Windows VPN
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:6096`, `src/vpn.rs:6117`, `src/vpn.rs:4384`, `src/vpn.rs:4406`, `tests/vpn_relay_link_test.rs:1`, `tests/vpn_server_test.rs:1`
- **Change:** Run existing 1:1 relay bridge on Windows with WinTun single-packet path. Preserve relay warm-path semantics and shared AEAD nonce counter. Do not split yamux streams; use existing two unidirectional substream tags. Host config applies Windows route/forwarding/rules from Phase 2. Start with `--relay-only` to exclude direct path variables.
- **Unit tests:** `test_windows_vpn_relay_nonce_counter_shared`; `test_windows_vpn_relay_uses_single_packet_bridge`; existing `vpn_relay_link_test` remains platform-portable.
- **e2e tests:** T-WIN-VPN-RELAY1 — Windows connector to Linux listener, `--relay-only`, ping overlay address both directions; T-WIN-VPN-RELAY2 — Linux connector to Windows listener; T-WIN-VPN-RELAY3 — Windows listener and Windows connector via Linux server.
- **Done:** Relay-only works across Windows/Linux/macOS peer combinations; `carriers=1` path unchanged on Linux/macOS.

### 3.3 Direct QUIC upgrade and fallback on Windows
- **Model:** Opus 4.8 design review → Sonnet implements
- **Files:** `src/holepunch.rs:84`, `src/holepunch.rs:168`, `src/vpn.rs:3825`, `src/vpn.rs:3956`, `src/vpn.rs:4046`, `src/vpn.rs:7562`
- **Change:** Enable Windows in existing direct upgrade path without changing token derivation, Quinn config, AEAD, carrier steering, or PMTU decisions. Add Windows UDP socket buffer set/verify behavior if missing at `holepunch.rs:168`; if Windows clamps buffers, warn with actionable registry/admin remediation only if tested and valid. Direct upgrade retry grid, relay warm fallback, and `TooLarge` handling remain shared. Opus review must inspect diff for no direct-path behavior change outside Windows cfg.
- **Unit tests:** `test_windows_udp_socket_buffer_set_verify`; `test_windows_direct_retry_grid_unchanged`; existing `test_should_retry_direct_cases`; existing PMTU tests `test_pmtu_*`.
- **e2e tests:** T-WIN-VPN-DIRECT1 — Windows connector and Linux listener upgrade from relay to direct; T-WIN-VPN-DIRECT2 — direct path killed/blocked, bridge falls back to warm relay without TUN reconnect; T-WIN-VPN-DIRECT3 — direct retry succeeds after Windows firewall opens UDP later.
- **Done:** Direct path works/falls back on Windows; no extra server protocol fields; relay remains warm.

### 3.4 VPN carriers and flow-pinned direct steering on Windows
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:6096`, `src/vpn.rs:6117`, `src/vpn.rs:7211`, `src/vpn.rs:7304`, `src/vpn.rs:7562`
- **Change:** Validate Windows relay carriers and direct carriers. Relay keeps per-datagram round-robin over reliable substreams. Direct keeps flow-pinned carrier hashing, not per-datagram RR. Windows WinTun single-queue must not become bottleneck due unbounded buffering; bounded bridge queues backpressure inner flows.
- **Unit tests:** `test_windows_direct_flow_carrier_hash_stable`; existing `flow_carrier` tests; `test_windows_relay_carriers_nonce_counter_shared`; `test_windows_tun_queue_backpressure_with_carriers`.
- **e2e tests:** T-WIN-VPN-CARR1 — Windows VPN `--carriers 4 --relay-only` moves traffic; killing one carrier cleanly reconnects/tears down per existing invariant, no silent degradation; T-WIN-VPN-CARR2 — direct `--carriers 4` establishes all sibling QUIC conns or stays on relay; T-WIN-VPN-CARR3 — single TCP flow remains ordered on direct carriers.
- **Done:** Carrier semantics match Linux/macOS; `--carriers 1` default byte/path-identical; no per-datagram direct RR.

### 3.5 Multi-client hub mode on Windows
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:1672`, `src/vpn.rs:1690`, `src/vpn.rs:6096`, `src/vpn.rs:7211`, `src/vpn.rs:7304`, `tests/vpn_server_test.rs:1`
- **Change:** Enable Windows listener hub mode (`--max-clients N>1`) and Windows connector spokes. Preserve separate hub branch; do not edit 1:1 path to add hub behavior. One Windows TUN for hub; per-peer `LinkSender` swaps relay/direct in place; per-peer keys and nonce counters remain isolated. Windows firewall/spoke isolation from Phase 2 must block spoke-to-spoke when configured.
- **Unit tests:** existing hub allocation/server tests; `test_windows_hub_per_peer_nonce_counters`; `test_windows_hub_spoke_isolation_rules_named`; `test_windows_hub_route_default_deny`.
- **e2e tests:** T-WIN-HUB1 — Windows hub with two Linux spokes, host-only mode; spokes reach hub, not each other; T-WIN-HUB2 — Linux hub with Windows spoke; T-WIN-HUB3 — Windows hub gateway advertises LAN to two spokes; T-WIN-HUBD1 — one peer upgrades direct while another remains relay.
- **Done:** Hub mode parity with Linux/macOS, including per-peer direct/fallback and route policy default-deny.

### 3.6 Gateway mode, route policy, NAT, and forward-accept runtime
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4384`, `src/vpn.rs:4406`, `src/vpn.rs:2924`, `src/vpn.rs:3007`, `src/vpn.rs:3261`
- **Change:** Wire Windows host backend into runtime option matrix: `--advertise`, `--accept-all-routes`, `--accept-route`, `--refuse-route`, `--nat-masquerade`, `real@virtual`, `--forward-accept`, `--no-route-manage`. Preserve connector route policy default-deny. Server overlap check uses virtuals only for netmap. Admin/status should show Windows flags and NAT mappings same as Linux/macOS because data model is shared.
- **Unit tests:** existing `routes::filter_accepted` tests; `test_windows_vpn_ready_serializes_virtual_only`; `test_windows_route_policy_default_deny`; `test_windows_gateway_flags_admin_display_bundle`.
- **e2e tests:** T-WIN-GW1 — Windows gateway advertises LAN, Linux peer accepts route and reaches LAN host; T-WIN-GW2 — Windows connector refuses advertised route, no route installed; T-WIN-GW3 — `--no-route-manage` installs no Windows routes; T-WIN-NAT1 and T-WIN-VPN-NAT from Phase 2 run through real VPN.
- **Done:** Gateway option matrix works on Windows; admin/status matches shared data.

### 3.7 Stale reclaim under real VPN runtime
- **Model:** Sonnet 4.6
- **Files:** `src/vpn.rs:4042`, `src/vpn.rs:5081`, `src/vpn.rs:4384`, `src/main.rs:1669`, `src/main.rs:1737`
- **Change:** Validate stale reclaim integrated with real runtime. On startup, Windows VPN must reclaim leaked resources for same `(id, role)` before applying new config. Ctrl-C/Ctrl-Break triggers normal Drop. Process kill leaves markers; next run removes leaked routes/firewall/NAT/netmap/forwarding when safe. Do not restore forwarding if another live bore marker remains.
- **Unit tests:** `test_windows_startup_calls_stale_reclaim_before_apply`; `test_windows_ctrl_break_drop_order`; `test_windows_stale_reclaim_ignores_other_live_marker`.
- **e2e tests:** T-WIN-STALE1 — kill Windows listener process after gateway apply; next run logs reclaim and host returns clean after exit; T-WIN-STALE2 — two live gateway links, kill one, reclaim does not break other.
- **Done:** Runtime stale reclaim robust and idempotent.

### 3.8 Admin/status Windows VPN parity
- **Model:** Haiku 4.5
- **Files:** `src/admin_api.rs:22`, `src/vpn.rs:1672`, `src/vpn.rs:1690`, existing frontend/admin files if they already display VPN flags
- **Change:** Verify no OS-specific admin omissions for Windows. If UI/backend filters VPN info by Linux/macOS cfg, expand to `feature="vpn"` or include Windows. Show Windows-specific warnings/limitations as notes only if shared display model already supports notes. Do not create a Windows-only admin section.
- **Unit tests:** existing admin VPN tests; `test_admin_vpn_windows_link_counts`; `test_admin_vpn_windows_flags_visible`; `test_admin_vpn_windows_nat_mapping_visible`.
- **e2e tests:** T-WIN-ADMIN1 — Windows VPN link visible in `/admin/api/v1/status`; TX/RX counters increment live; flags and NAT mappings shown.
- **Done:** Admin status parity for Windows VPN; non-VPN admin unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --all`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test subset:** `cargo test --features vpn vpn_relay_link_test vpn_server_test windows_vpn -- --nocapture`
- **Cross-check:** `cargo check --features vpn --target x86_64-pc-windows-msvc`
- **Elevated Windows check:** T-WIN-VPN-RELAY*, T-WIN-VPN-DIRECT*, T-WIN-VPN-CARR*, T-WIN-HUB*, T-WIN-GW*, T-WIN-STALE*, T-WIN-ADMIN1
- **Regression guard:** Existing Linux netns VPN suite and macOS CI/e2e remain green; direct QUIC/AEAD/holepunch shared code reviewed by Opus if touched

## Phase done criterion

Phase 3 is done when Windows VPN supports relay, direct upgrade/fallback, carriers, hub-and-spoke, gateway routing, NAT/netmap, forward-accept, stale reclaim, and admin visibility with parity to Linux/macOS.
