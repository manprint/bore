# Windows Support — Plan Overview

> **Status:** planning | **Opus authored:** 2026-06-30
> **Folder:** `docs/plans/plan_WindowsSupport/`

## Goal

Complete bore Windows support across all user-visible modes: public tunnels (`bore local`), secret tunnels (`bore proxy`, relay and UDP/QUIC direct), vhost tunnels, server mode, transfer/test utilities, and full VPN (`bore vpn listen|connect`). Recon shows non-VPN transports are already cross-platform in the shared async TCP/QUIC data plane; Windows work is concentrated in VPN compile gates, TUN creation, Windows host networking, privilege/state handling, Windows CI, and end-to-end acceptance.

```
Reference scenario:
1. Windows 11 host W runs bore binaries built from this branch.
2. W exposes a local HTTP server through `bore local` over TCP relay and public `--udp`; a remote client reaches both paths.
3. W acts as secret provider and secret consumer with `bore proxy --udp --carriers 4`; relay fallback, direct QUIC, admin rows, and carrier failover match Linux/macOS behavior.
4. W registers a vhost with and without `--udp`; remote HTTP requests complete and `STREAM_READY` semantics are preserved.
5. W runs `bore server --udp` and accepts Linux/macOS/Windows clients for public, secret, vhost, and VPN modes.
6. W runs `bore vpn listen|connect` against Linux/macOS/Windows peers: relay-only works, direct QUIC upgrades in place, direct fallback works, multi-client hub works, gateway routes work, overlapping-subnet NAT (`real@virtual`) works, `--forward-accept` equivalent works, stale reclaim restores host networking after process kill.
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Treat non-VPN transports as already Windows-capable unless tests prove otherwise. | Implementation must not edit public/secret/vhost/server/transfer data-plane code except for test/CI fixes. Parity is proven by Windows CI and e2e scripts. |
| **D2** | Add Windows VPN as an additive cfg twin: `#[cfg(target_os = "windows")]`, mirroring Linux/macOS. | `src/vpn.rs:3`, `src/lib.rs:1`, and `src/main.rs:5`/`:931` expand to include Windows; Linux and macOS bodies stay semantically unchanged. |
| **D3** | Use WinTun for Windows L3 TUN. | Bore VPN remains L3 IP packet tunnel. TAP/L2 drivers are rejected for default path. Windows packaging must handle `wintun.dll` and driver install/update policy. |
| **D4** | Windows VPN starts single-queue/no-offload. | `--tun-queues > 1` warns and clamps on Windows like macOS; bridge uses single-packet path. Multi-queue/offload can be future work after correctness. |
| **D5** | Use `%ProgramData%\bore\vpn\state\` for machine-wide Windows stale-reclaim markers. | SIGKILL recovery can run from elevated CLI without per-user ambiguity. Markers use same `(id, role)` model as Linux/macOS. |
| **D6** | Use Windows-native networking commands first: PowerShell/NetTCPIP/NetNat/NetSecurity plus `netsh` fallback builders where needed. | Unit tests snapshot argv/script builders; `CommandRunner` stays reusable. Locale-sensitive parsing must be avoided or constrained. |
| **D7** | Plain gateway masquerade uses WinNAT; overlapping-subnet `real@virtual` requires a Windows prefix-translation backend before VPN can be declared complete. | Phase 2 contains an Opus feasibility gate. Acceptable backend must preserve host-bit 1:1 semantics and pass T-WIN-VPN-NAT. If Windows built-ins cannot do prefix netmap, implement a supported WFP/WinDivert-based hostcfg helper or do not ship Windows VPN as complete. |
| **D8** | Windows firewall/forward-accept uses named per-link rules, not global policy changes. | Rule names include sanitized bore id/role; `Drop` and stale reclaim delete by exact rule group/name. |
| **D9** | Windows direct QUIC data plane must reuse existing holepunch/Quinn/AEAD logic. | No edits to token derivation, nonce counters, carrier steering, PMTU decisions, or yamux stream ownership except Windows socket-buffer verification if needed. |
| **D10** | Windows e2e has two tiers: GitHub-hosted non-admin CI and elevated manual/self-hosted CI. | Public/secret/vhost/server/transfer run on hosted Windows CI. VPN host-networking e2e runs on elevated self-hosted Windows or documented manual acceptance until hosted runners provide needed privileges. |

## Architecture summary

Windows support is an additive platform backend under existing VPN isolation points: cfg gates, `hostcfg::create_tun`, `hostcfg::NetConfig::apply`, `hostcfg::stale_reclaim`, and `hostcfg_cmd::windows`. The VPN bridge, relay/direct path, carriers, AEAD, PMTU monitor, hub router, and admin data model remain shared. Non-VPN modes are validated on Windows through CI/e2e without data-plane changes.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|-----------------|
| 0 — Compile gates and dependency scaffold | [phase_01.md](phase_01.md) | Haiku/Sonnet | yes |
| 1 — WinTun adapter backend | [phase_02.md](phase_02.md) | Sonnet + Opus gate | yes |
| 2 — Windows host networking backend | [phase_03.md](phase_03.md) | Sonnet + Opus gate | yes |
| 3 — VPN runtime integration | [phase_04.md](phase_04.md) | Sonnet + Opus gate | yes |
| 4 — Non-VPN Windows parity | [phase_05.md](phase_05.md) | Haiku/Sonnet | yes |
| 5 — Windows e2e and CI | [phase_06.md](phase_06.md) | Sonnet + Opus gate | yes |
| 6 — Documentation, packaging, release hardening | [phase_07.md](phase_07.md) | Haiku/Sonnet + Opus final read | yes |

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| VPN cfg gates | Existing Linux/macOS gates | `src/vpn.rs:3`, `src/lib.rs:1`, `src/main.rs:5`, `src/main.rs:931` |
| Linux TUN semantics | `hostcfg::create_tun` Linux twin | `src/vpn.rs:4091` |
| macOS no-offload/single-queue precedent | `hostcfg::create_tun` macOS twin | `src/vpn.rs:4203` |
| Windows command-builder insertion | `hostcfg_cmd::windows` module | `src/vpn.rs:3261` |
| Shared host config state | `hostcfg::NetConfig` fields | `src/vpn.rs:4384` |
| macOS additive `NetConfig::apply` pattern | `hostcfg::NetConfig::apply` macOS twin | `src/vpn.rs:4406` |
| Linux full host config behavior | `hostcfg::NetConfig::apply` Linux twin | `src/vpn.rs:4563` |
| Windows route/MTU builder snapshots | Existing Windows tests | `src/vpn.rs:3451` |
| Stale reclaim shape | `hostcfg::stale_reclaim` macOS twin | `src/vpn.rs:4042` |
| Bridge single-packet/offload split | `bridge::run_uplink_offload`, `bridge::run_downlink_offload` | `src/vpn.rs:7211`, `src/vpn.rs:7304` |
| Existing portable VPN tests | VPN unit test module | `src/vpn.rs:7476` |
| Transfer Windows path tests | stdin/path codec tests | `tests/transfer_stdin_cli_test.rs:56` |
| Windows cross-build CI | `vpn-cross-build` matrix | `.github/workflows/ci.yml:60` |
| macOS VPN CI precedent | `macos-vpn-build`, `macos-vpn-e2e` | `.github/workflows/ci.yml:102`, `.github/workflows/ci.yml:129` |
| macOS docs precedent | Runtime/acceptance docs | `docs/vpn/VPN_MACOS.md`, `docs/vpn/VPN_MACOS_ACCEPTANCE.md` |

## Invariants

- **I-WIN1:** Linux runtime function bodies remain semantically unchanged: `create_tun`, Linux `NetConfig::apply`, Linux `Drop`, Linux `stale_reclaim`, nft/iptables builders, offload pumps.
- **I-WIN2:** macOS runtime function bodies remain semantically unchanged; Windows is a third cfg twin, not a refactor of Linux/macOS.
- **I-WIN3:** Non-VPN data planes remain byte-identical unless a Windows-only test exposes an actual portability bug.
- **I-WIN4:** VPN shared data plane remains unchanged: AEAD nonce counters, direct QUIC, carrier steering, PMTU logic, relay queues, hub router, `STREAM_READY`, yamux stream ownership.
- **I-WIN5:** No `tokio::io::split` on `mux::Stream`.
- **I-WIN6:** `carriers == 1` and default VPN behavior stay byte/path-identical on Linux/macOS.
- **I-WIN7:** Windows single-queue/no-offload path must not call Linux-only tun-rs offload APIs.
- **I-WIN8:** Stale reclaim must be idempotent: process kill followed by next run restores IP forwarding/rules only for dead bore link, never breaks another live link.
- **I-WIN9:** Overlapping-subnet NAT must preserve host bits exactly (`real@virtual`) or the feature is not complete on Windows.
- **I-WIN10:** Every generated doc/script/test uses existing repo structure and professional technical prose only.

## Risk register

| Risk | Mitigation |
|------|-----------|
| WinTun crate/API mismatch or unsafe wrapper constraints | Phase 1 isolates adapter wrapper and requires compile+mock tests before runtime wiring. External refs: [WireGuard WinTun](https://github.com/WireGuard/wintun), [wintun-bindings Session](https://docs.rs/wintun-bindings/latest/wintun_bindings/struct.Session.html). |
| Windows NAT lacks Linux/macOS 1:1 prefix netmap equivalent | Phase 2 has explicit Opus feasibility gate. Do not mark VPN complete until T-WIN-VPN-NAT passes. |
| Elevated networking unavailable on hosted Windows CI | Split hosted non-admin CI from manual/self-hosted elevated VPN e2e. `VPN_WINDOWS_ACCEPTANCE.md` records manual path. |
| Locale-sensitive `netsh`/PowerShell output parsing | Prefer structured PowerShell output (`ConvertTo-Json`) and snapshot exact commands. Avoid parsing localized prose where possible. |
| Firewall rule leaks/collisions | Use per-link rule group/name with sanitized id/role. Stale reclaim deletes by exact group/name and tests collision cases. |
| UDP throughput lower on Windows due socket-buffer clamp | Add buffer set/verify tests and benchmark notes; document limitation if clamp remains without supported override. |
| Non-VPN regression while editing VPN cfg | Phase 4 Windows parity tests plus Opus diff audit: no data-plane edits outside approved files. |

## Model-assignment summary

| Work type | Assigned model | Reason |
|-----------|----------------|--------|
| Compile gates, cfg expansion, docs stubs, command snapshots | Haiku 4.5 | Mechanical mirror of existing patterns. |
| WinTun wrapper, host networking backend, VPN runtime integration, e2e scripts | Sonnet 4.6 | Non-trivial Rust + tests. |
| TUN driver decision, NAT/netmap feasibility, concurrency/lifecycle reviews, acceptance definition, final docs read | Opus 4.8 review gate | Architecture/correctness-critical decisions. |
| Current plan authoring | gpt-5.5 architect slot | Session model acting as Opus-equivalent planner. |
