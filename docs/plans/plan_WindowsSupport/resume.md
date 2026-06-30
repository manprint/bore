# Windows Support — Resume

> **Next:** phase_02.md § 1.2 — Add Windows TUN abstraction without touching Unix types
> **Last updated:** 2026-06-30

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Compile gates and dependency scaffold | phase_01.md | `DONE` | Windows VPN CLI cfg visible; Windows stub API exports explicit unsupported runtime; local Windows cross-check blocked by missing MSVC tools. |
| 1 — WinTun adapter backend | phase_02.md | `IN_PROGRESS` | WinTun binding selected via local `bore-wintun` safe wrapper; shared VPN runtime now has Windows `TunDevice` wrapper and `create_tun` twin. |
| 2 — Windows host networking backend | phase_03.md | `IN_PROGRESS` | Pure route/MTU/forwarding/firewall/NAT builder snapshots added; Windows `NetConfig::apply`/Drop/stale_reclaim scaffold added; elevated e2e still TODO. |
| 3 — VPN runtime integration | phase_04.md | `TODO` | Relay/direct/carriers/hub/gateway/admin. |
| 4 — Non-VPN Windows parity | phase_05.md | `TODO` | Public, secret, vhost, server, transfer, test-udp. |
| 5 — Windows e2e and CI | phase_06.md | `TODO` | Hosted + elevated/manual matrix. |
| 6 — Documentation, packaging, and release hardening | phase_07.md | `TODO` | Docs, artifacts, security, release sign-off. |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| `cargo_check_windows_vpn_cfg` | compile | `BLOCKED` | Local Linux host lacks MSVC tools (`ml64.exe`/`lib.exe`); Windows CI runner must verify. |
| `windows_vpn_backend_unsupported_error` | unit | `TODO` | Windows-only stub tests added in `src/vpn_windows.rs`; require Windows target/CI to execute. |
| `test_cmd_windows_route_add_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_cmd_windows_route_del_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_cmd_windows_link_set_mtu_snapshot` | unit | `DONE` | Covered by `cmd_windows_builders_snapshot`. |
| `test_windows_tun_*` | unit | `IN_PROGRESS` | Name validation/env preflight tests added in `src/vpn_windows.rs`; full adapter tests pending WinTun runtime. |
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
| `docs/vpn/VPN_WINDOWS.md` | `TODO` | Architecture/operator doc. |
| `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` | `TODO` | Manual/elevated acceptance matrix. |
| `docs/vpn/VPN.md` | `TODO` | Link/update Windows support. |
| `docs/INSTALL_BORE.md` | `TODO` | Windows install and WinTun prerequisites. |
| `README.md` | `TODO` | Update platform support only if README lists it. |

## Open blockers

- WinTun Rust binding choice must pass license/API review before Phase 1 coding.
- Windows `real@virtual` 1:1 prefix netmap backend must be proven; if no built-in backend exists, a supported WFP/WinDivert/helper approach is required before declaring Windows VPN complete.
- Elevated Windows runner availability determines whether VPN e2e is automated or manual at first landing.

## Decisions changed at runtime

- none
