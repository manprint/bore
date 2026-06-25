# VPN macOS Completion — Plan Overview

> **Status:** planning | **Opus authored:** 2026-06-25
> **Folder:** `docs/plans/plan_VpnMacosCompletion/`

## Goal

Bring the `bore vpn` L3 tunnel to **full functional parity on macOS** (Apple
Silicon, macOS 13 Ventura+), with **zero Linux regression**. Today only the
*pure* macOS host-config groundwork has landed (`hostcfg_cmd::macos` argv
builders + `pf_ruleset` composer + `parse_lan_iface`, snapshot-tested on the
Linux CI); the VPN module is still compiled out on macOS
(`src/vpn.rs:3` is gated `#[cfg(all(feature = "vpn", target_os = "linux"))]`),
so there is no macOS runtime, no `bore vpn` subcommand, and no macOS tests. This
plan completes the runtime: utun device creation, the macOS `NetConfig`
apply/Drop/stale_reclaim twin (PF anchor + `sysctl` forwarding), the cfg-gate
flip, platform flag warnings, a macOS CI build/smoke gate, and docs.

```
Reference scenario (acceptance):
On Apple Silicon, macOS 13+:
  1. CI macOS runner: `cargo build --features vpn` + `cargo clippy --features vpn
     -- -D warnings` + `cargo test --features vpn` all green (T-MAC-BUILD).
  2. CI macOS runner, sudo, single host (T-MAC-SMOKE):
       sudo target/debug/bore vpn connect --to <srv> --secret s --id m0 \
            --tun-name auto --relay-only --no-route-manage
     creates a utunN device, assigns the /30, then RAII-reverts on SIGTERM and
     stale_reclaim cleans a SIGKILLed run — utun gone, sysctl restored, PF anchor
     `bore_vpn/m0` flushed.
  3. Manual two-host (T-MAC-MANUAL, human-run on a Mac + Linux peer):
     macOS connector advertises `192.168.7.0/24@10.77.0.0/24`; a Linux peer pings
     a host in 10.77.0.0/24 → reaches 192.168.7.x via PF `binat`; link comes up
     on relay, upgrades to direct QUIC; `sysctl net.inet.ip.forwarding` and the PF
     anchor are applied on start and reverted on exit.
  4. Linux unchanged: `scripts/vpn_netns_test.sh` + `_hard` 100% green (DEC-M1).
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Compile-time `cfg` twin (a `#[cfg(target_os="linux")]` body and a `#[cfg(target_os="macos")]` body per runtime fn), **not** a runtime trait. Reaffirms DEC-M1. | Linux runtime stays byte-for-byte; macOS is additive. Shared helpers (`pick_tun_name`, `check_root`, `CommandRunner`/`RealRunner`, `NetConfig` fields, `Drop`'s `revert_cmds` loop) stay un-gated. |
| **D2** | Flip the cfg gates **early** (Phase 2) and provide macOS runtime **stubs** that `bail!("macOS VPN runtime pending Phase N")`; fill stubs in over Phases 3–4. | Every phase is CI-green on the macOS runner (incremental compile feedback). A mid-port `bore vpn` on macOS bails cleanly, never panics or misbehaves. |
| **D3** | macOS CI runner = `macos-14` (Apple Silicon). CI proves **build + clippy + unit/snapshot + single-host smoke** (utun create/teardown under sudo). Full two-host gateway e2e is **manual** (no netns on macOS; CI has no second host / real LAN). | Acceptance = CI (T-MAC-BUILD, T-MAC-SMOKE) + a documented manual checklist (T-MAC-MANUAL). No false claim of full-LAN CI coverage. |
| **D4** | macOS forwarding via `sysctl net.inet.ip.forwarding`; NAT/filter via ONE per-link PF anchor `bore_vpn/<id>` loaded from a temp file built by `pf_ruleset`. | Apply = `sysctl` enable + `pfctl -e` + `pfctl -a bore_vpn/<id> -f <tmp>`. Revert = `pfctl -a bore_vpn/<id> -F all` + `sysctl` restore. `stale_reclaim` flushes the anchor by id + restores sysctl. |
| **D5** | macOS state files under `/var/run/bore_vpn_*` **without** the Linux `/proc/self/ns/net` inode scoping (macOS has no netns). Keep the first-wins-orig + per-link refcount markers for concurrent same-host links. | Simpler than the Linux B3 path; still last-out restore of `net.inet.ip.forwarding`. |
| **D6** | PF grammar in `pf_ruleset` is **PROVISIONAL** until the Phase 1 spike validates it on real macOS 13+. The spike may patch `pf_ruleset`/builders + their snapshots. | Phase 1 is a hard gate for Phase 4. Hardware-gated (needs a Mac + sudo). |
| **D7** | macOS `create_tun` ignores Linux-style names (`boreN`), requests a kernel-assigned `utun`, and reads the resolved name back via the device handle. No GSO/GRO offload, queues forced to 1. | `--tun-name` is **advisory** on macOS (kernel assigns `utunN`); `--tun-queues > 1` warns and is clamped to 1; offload path is never taken. |
| **D8** | No `check_binary_exists("<tool>")` (`<tool> --version`) probe for the BSD tools `route`/`ifconfig`/`pfctl`/`sysctl`. | Those tools do not support `--version` and would false-negative. macOS path assumes they exist (always present on macOS) and reports errors from the actual command. |
| **D9** | No `Cargo.toml` change for `tun-rs` (already `cfg(any(linux, macos))`, line 77-78); `procfs` stays Linux-only (not used by the vpn module). Windows groundwork untouched (deferred). | Dependency surface unchanged. |

## Architecture summary

The data plane (bridge, AEAD seal/open, carriers, relay substreams, direct QUIC
`DirectConn`, PMTU monitor, reconnect) is pure Rust + tokio + quinn + ring with
**no platform syscalls** — it is reused unchanged on macOS. Only the *host edge*
is platform-specific: TUN creation (`create_tun`) and host network config
(`NetConfig::apply`/`Drop` + `stale_reclaim`). Each gets a `#[cfg(macos)]` twin
that drives the already-landed `hostcfg_cmd::macos` argv builders + `pf_ruleset`
(PF anchor instead of nft/iptables, `sysctl` instead of `/proc`). The cfg gates
that hide the whole module from macOS are flipped to `any(linux, macos)`.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|-----------------|
| 1 — De-risk spike + macOS CI build job | [phase_01.md](phase_01.md) | Haiku + Sonnet (Opus gate on spike findings) | yes (additive) |
| 2 — Flip cfg gates + macOS runtime stubs | [phase_02.md](phase_02.md) | Opus design review → Sonnet; Haiku for flag warnings | yes |
| 3 — macOS TUN runtime (`create_tun` twin) | [phase_03.md](phase_03.md) | Opus design review → Sonnet | yes |
| 4 — macOS host-config runtime (NetConfig/Drop/stale_reclaim twin) | [phase_04.md](phase_04.md) | Opus design review → Sonnet | yes |
| 5 — Integration, e2e, acceptance | [phase_05.md](phase_05.md) | Sonnet (Opus gate on acceptance asserts) | yes |
| 6 — Docs | [phase_06.md](phase_06.md) | Haiku (Opus final read gate) | yes |

> Landed groundwork (2026-06-16) is the implicit "Phase 0", DONE: `tun-rs` on
> the macOS target (`Cargo.toml:77-78`), the full `hostcfg_cmd::macos` builder set
> + `pf_ruleset` + `parse_lan_iface` with 5 snapshot tests. This plan does not
> re-do it; it consumes it.

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| macOS route add/get argv | `macos::cmd_route_add`, `macos::cmd_route_get` | `src/vpn.rs:2949`, `src/vpn.rs:2952` |
| macOS LAN-iface parse | `macos::parse_lan_iface` | `src/vpn.rs:2957` |
| macOS addr/mtu/link argv | `macos::cmd_addr_add`, `cmd_link_set_up`, `cmd_link_set_mtu` | `src/vpn.rs:2962`, `:2970`, `:2975` |
| macOS forwarding sysctl argv | `macos::cmd_sysctl_ip_forward`, `cmd_sysctl_get_ip_forward` | `src/vpn.rs:2995`, `:3005` |
| macOS PF enable/load/flush argv | `macos::cmd_pf_enable`, `cmd_pf_load_anchor`, `cmd_pf_flush_anchor`, `cmd_pf_show_anchor` | `src/vpn.rs:2991`, `:2996`, `:3006`, `:3011` |
| macOS PF ruleset composer | `macos::pf_ruleset(tun, lan_if, advertised, nat_maps, hub, nat_masquerade, forward_accept, mss)` | `src/vpn.rs:3071` |
| Existing macOS snapshot tests | `cmd_macos_builders_snapshot`, `macos_pf_ruleset_*`, `macos_parse_lan_iface_from_route_get` | `src/vpn.rs:3179`, `:3260`, `:3280`, `:3298`, `:3245` |
| Linux apply (structural template for the macOS twin) | `NetConfig::apply` | `src/vpn.rs:4145` |
| Linux Drop (ip_forward restore branch to twin) | `impl Drop for NetConfig` | `src/vpn.rs:4641` |
| Linux stale_reclaim (twin target) | `hostcfg::stale_reclaim` | `src/vpn.rs:3823` |
| TUN creation (twin target) | `hostcfg::create_tun` | `src/vpn.rs:3916` |
| State-file helpers (macOS variant) | `ipforward_state_path`, `fwd_refcount_path`, `ipforward_orig_path`, `other_fwdref_present` | `src/vpn.rs:4020`, `:4055`, `:4068`, `:4098` |
| Privilege + tool probe (reusable) | `hostcfg::check_root`, `hostcfg::check_binary_exists` | `src/vpn.rs:3798`, `:3810` |
| Command runner abstraction (reusable) | `CommandRunner` trait, `RealRunner` | `src/vpn.rs` (`hostcfg` mod) |
| cfg gates to flip | module + CLI gates | `src/vpn.rs:3`, `src/lib.rs:40-42`, `src/main.rs:5-8`, `:937-938`, `:1092-1093`, `:1643-1644` |
| Call sites (listen/connect/hub) | `stale_reclaim`/`create_tun`/`NetConfig::apply` | `src/vpn.rs:585-628`, `:1651-1703`, `:8061-8085` |
| TUN-into-bridge offload flow | `bridge::run(..., offload, ...)` | `src/vpn.rs:1538`, `:691-703` |

## Interface & protocol

- **Interface (§4): no new CLI flags, no new public API.** The existing `bore vpn
  listen|connect` flags gain macOS-specific *semantics*: `--tun-name` becomes
  advisory (kernel assigns `utunN`, D7/I-M8); `--tun-queues > 1` and the UDP
  hole-punch helper flags (`--upnp`, `--stun-server`, `--try-port-prediction`,
  `--nat-udp-*`) warn and are advisory/ignored on macOS (D8/I-M4, Phase 2.3). No
  conflict rules change.
- **Protocol/schema (§5): N/A — no wire change.** The control/data protocol,
  AEAD, carriers, relay framing, and direct QUIC are platform-agnostic and
  untouched (I-M3). A macOS peer and a Linux peer interoperate over the unchanged
  wire. No serde/wire field added or modified by this plan.

## Invariants

- **I-M1:** The Linux runtime is **byte-for-byte unchanged** (DEC-M1). All Linux
  `cmd_nft_*`/`cmd_iptables_*` builders, the Linux `apply` body, the Linux Drop
  ip_forward branch, and Linux state-file helpers stay under
  `#[cfg(target_os="linux")]`. Proven by `scripts/vpn_netns_test.sh` + `_hard`
  staying 100% green and by `git diff` showing no semantic edit inside a
  `cfg(linux)` block.
- **I-M2:** macOS support is a **compile-time twin** (`cfg`), never a runtime
  trait/branch in shared code.
- **I-M3:** The **data plane is untouched**: bridge, AEAD, carriers, relay,
  direct QUIC, PMTU monitor, reconnect carry **no** platform gates.
- **I-M4:** On macOS, TUN offload is always **off** and queues are **1**
  (utun has neither). `--tun-queues > 1` warns and clamps; the GSO/GRO branch in
  `create_tun` is never compiled into the macOS path.
- **I-M5:** macOS PF anchor semantics mirror the Linux `gateway_nft_cmds`:
  `binat`=1:1 netmap (host-bit preserving), `nat`=masquerade, `scrub max-mss`=MSS
  clamp, `block`=hub spoke isolation, `pass`=`--forward-accept`.
- **I-M6:** macOS has **RAII revert + SIGKILL `stale_reclaim` parity** with Linux:
  on exit the PF anchor is flushed and `net.inet.ip.forwarding` restored
  (last-link-out), and a SIGKILLed run is reclaimed on the next start by id.
- **I-M7:** The macOS path never probes `route`/`ifconfig`/`pfctl`/`sysctl` with
  `--version` (D8); it assumes presence and surfaces real command errors.
- **I-M8:** `--tun-name` is advisory on macOS (kernel assigns `utunN`); the
  resolved name is read back from the device and used everywhere downstream.

## Risk register

| Risk | Mitigation |
|------|-----------|
| PF grammar in `pf_ruleset` is wrong on real macOS (PROVISIONAL, D6). | Phase 1 spike validates/patches it on a Mac **before** Phase 4 wires it; snapshots updated to the validated form. |
| No Mac available to the implementer (dev on Linux). | macOS-gated code can't compile on Linux; the `macos-14` CI job (Phase 1.1) is the compile/clippy/test oracle for every later phase. Spike + smoke + manual checklist run on a Mac (CI runner / human). |
| utun naming differs from Linux assumptions (`pick_tun_name` uses sysfs). | Phase 3 macOS `create_tun` kernel-auto-assigns and reads back the name (D7); `--tun-name` documented advisory. |
| BSD tools reject `--version` → false "missing" (D8). | macOS path never calls `check_binary_exists` for them; Phase 4 asserts this. |
| `/run` absent on macOS → state files fail → no SIGKILL recovery. | Phase 4 uses `/var/run/bore_vpn_*` (D5); smoke test (Phase 5.1) proves stale_reclaim after a SIGKILL. |
| Mid-port `bore vpn` ships on macOS and misleads users. | Stubs `bail!` with an explicit "pending Phase N" message (D2); docs (Phase 6) state macOS status per release. |

## Model-assignment summary

| Phase | Sub-phase | Model |
|-------|-----------|-------|
| 1 | 1.1 macOS CI build job | Haiku |
| 1 | 1.2 De-risk spike + findings + PF/builder corrections | Sonnet implements; **Opus gate** on findings |
| 2 | 2.1 Flip cfg gates | **Opus design review → Sonnet** |
| 2 | 2.2 cfg-split runtime + macOS stubs | **Opus design review → Sonnet** |
| 2 | 2.3 Platform flag warnings | Haiku |
| 3 | 3.1 macOS `create_tun` twin | **Opus design review → Sonnet** |
| 3 | 3.2 macOS name-resolution unit tests + smoke hook | Sonnet |
| 4 | 4.1 macOS `NetConfig::apply` twin | **Opus design review → Sonnet** |
| 4 | 4.2 macOS Drop + stale_reclaim twin | **Opus design review → Sonnet** |
| 4 | 4.3 macOS state-file helpers | Sonnet |
| 4 | 4.4 macOS rule-plane unit tests | Sonnet |
| 5 | 5.1 macOS single-host smoke e2e (CI) | Sonnet |
| 5 | 5.2 Manual two-host acceptance checklist | **Opus gate** on asserts → Sonnet |
| 5 | 5.3 Linux regression proof | Sonnet |
| 6 | 6.1 Docs update | Haiku; **Opus final read gate** |

## Gate commands (all phases)

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings`
- **Test (unit):** `cargo test --features vpn`
- **macOS CI:** runner `macos-14`, same three commands.
- **Linux e2e (regression guard):** `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vpn_netns_test.sh`
  (rebuild first: `cargo build --release --features vpn` as your user, not root).
