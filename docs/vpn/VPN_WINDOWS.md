# bore VPN on Windows

> Status: implementation in progress. This document tracks the Windows backend decisions and operator requirements while `docs/plans/plan_WindowsSupport/` is executed.

## D-WT1: WinTun backend

Windows VPN uses WinTun as the L3 packet device backend.

Rationale:
- bore VPN is an L3 IPv4 packet tunnel; WinTun is a Windows L3 TUN driver.
- TAP/L2 drivers are not the default path because they add Ethernet framing and a heavier driver/install model.
- The upstream WinTun distribution model is `wintun.dll`; bore loads it at runtime.
- The Rust bindings expose safe adapter/session operations after a required unsafe DLL load boundary.
- bore's main crate uses `#![forbid(unsafe_code)]`; therefore unsafe DLL loading is isolated in the local `bore-wintun` wrapper crate.

Chosen Rust binding:
- Crate: `wintun-bindings` 0.7.39.
- License: MIT.
- Bore-facing wrapper: `crates/bore-wintun`.

API facts used by the implementation:
- `wintun_bindings::load()` loads `wintun.dll` from the default DLL search path.
- `wintun_bindings::load_from_path(...)` loads an explicit DLL path.
- `Adapter::create(...)` and `Adapter::open(...)` create/open adapters.
- `Adapter::start_session(...)` starts a packet session.
- `Session::recv(...)`, `Session::send(...)`, and `Session::shutdown(...)` cover packet I/O and shutdown.

Operational policy:
- `wintun.dll` is **bundled in the official release artifacts** (zip and a
  separate `wintun-<target>.dll` asset), placed next to `bore.exe` so the
  default DLL search path finds it — no separate download step for users.
  Building from source still needs it placed next to the binary or provided
  through `BORE_WINTUN_DLL`. See "WinTun DLL distribution" below.
- `BORE_WINTUN_DLL` must point to a trusted DLL path.
- Missing DLL must fail before host networking side effects (verified:
  `windows_vpn_spike missing-dll`, T-WIN-TUN5).
- Windows VPN requires an elevated shell for adapter creation and host networking changes.

## WinTun DLL distribution (2026-07-01 decision)

Redistributing the official signed `wintun.dll` was chosen over requiring a
separate user download — WinTun's own site states "the below signed DLLs are
the only supported way of distributing Wintun", i.e. redistribution of the
unmodified signed binary is the *expected* path, not merely tolerated.

- Pinned version + zip SHA256 live in `scripts/fetch_wintun.ps1` (currently
  WinTun `0.14.1`). Bumping the WinTun version means updating both the
  version string and the hash in that one script.
- The same script is reused by two callers: the `windows-vpn-e2e` CI job
  (places `wintun.dll` next to the compiled example so the elevated spike can
  actually create adapters) and `mean_bean_deploy.yml`'s Windows release job
  (bundles the correct arch — `amd64` for `x86_64-pc-windows-msvc`, `x86` for
  `i686-pc-windows-msvc` — into the release zip, and uploads it again as a
  standalone `wintun-<target>.dll` asset for users who fetch the raw `.exe`).
- Integrity is checked by comparing the downloaded zip's SHA256 against the
  pinned value before extracting anything — a mismatch aborts the build/CI
  step rather than silently shipping an unverified DLL.

## Implementation status (2026-06-30)

WinTun adapter creation/read/write (Phase 1) and the bulk of Windows host
networking (Phase 2) are implemented and unit-tested. None of it has run on
real Windows hardware yet — the dev box is Linux and has neither MSVC
(`ml64.exe`/`lib.exe`) nor a mingw toolchain, so `cfg(target_os = "windows")`
code in this repo can be statically reviewed but not compiled or executed
locally. Every item below is implemented but **CI/elevated-hardware verified
only**, same posture as the rest of this plan (see
`docs/plans/plan_WindowsSupport/resume.md`).

Implemented:
- `hostcfg::create_tun` Windows twin (WinTun adapter create/open, IPv4/MTU
  config, single-queue/no-offload).
- `hostcfg::check_root` Windows twin: queries process token elevation via
  PowerShell (`[Security.Principal.WindowsPrincipal]...IsInRole(...
  Administrator)`) and fails BEFORE any adapter/host mutation. (Previously a
  stub that always returned `Ok(())` — found and fixed; this was the most
  severe gap, since it meant the non-admin preflight did nothing on Windows.)
- `hostcfg::NetConfig::apply`/`Drop`/`stale_reclaim` Windows twins: peer
  routes (`netsh`), IP forwarding refcount (`IPEnableRouter`, first-wins
  original-value tracking matching the Linux/macOS B3 refcount model),
  `--forward-accept` firewall rules, and plain-subnet NAT masquerade
  (`New-NetNat`). `Drop` itself is NOT Windows-specific — every Windows
  command here is a plain argv runnable by the same generic
  `std::process::Command::new(argv[0])` revert loop Linux/macOS already use.

Explicitly NOT implemented (documented gaps, not unverified guesses):
- **Overlapping-subnet `real@virtual` 1:1 prefix netmap (D7 §2.6).** Windows
  has no built-in equivalent to nft's stateless prefix DNAT/SNAT or PF
  `binat`; `New-NetNat`/WinNAT only does basic masquerade/port mapping, not
  host-bit-preserving 1:1 prefix translation. A real backend would need a
  WFP callout driver or a WinDivert-based helper — both are new signed-driver
  dependencies with their own licensing/distribution/maintenance cost, and
  neither can be validated without Windows hardware. Deferred rather than
  shipped as an unverified guess (2026-06-30 decision: ship everything else
  now, revisit this with a dedicated Opus feasibility pass + real hardware).
- **Hub mode (`--max-clients > 1`) spoke isolation (D2).** The Linux/macOS
  equivalents (nft `iifname tun oifname tun drop`, PF `block in on utun from
  (utun:network) to (utun:network)`) both match "entered AND will leave via
  the same interface." `New-NetFirewallRule` has no combined ingress+egress
  interface predicate for routed/transit traffic, so a naive
  `-InterfaceAlias tun -Direction Inbound -Action Block` rule would also
  block legitimate spoke→LAN traffic (which also enters via the TUN
  interface). Rather than ship a block rule that either does nothing or
  breaks the gateway, Windows hub mode currently logs a `WARN` that spoke
  isolation is not enforced (fail-visible, not fail-open). A starting point
  (`hostcfg_cmd::windows::cmd_firewall_block_spoke_isolation`) is in the
  tree, unused, pending a real WFP-based design.
- **`--forward-accept`'s actual effectiveness against ROUTED (not
  host-bound) traffic is unverified.** Windows Defender Firewall's standard
  `New-NetFirewallRule` model is primarily host-bound-traffic-oriented (WFP
  ALE layer); whether it filters merely-forwarded packets the way Linux's
  iptables FORWARD chain or macOS PF does is a real platform question that
  needs T-WIN-FWD1/T-WIN-FWD2 on real hardware to answer. The rule shape
  implemented here (`cmd_firewall_allow_tun_to_lan` +
  `cmd_firewall_allow_lan_to_tun`, mirroring macOS's two-rule "allow
  everything on tun" + "allow tun-network-sourced traffic out the LAN
  interface" pattern) is the best structural analogy available without a
  combined in/out-interface match, not a confirmed-working design.

## Implementation status update (2026-07-01)

Added `examples/windows_vpn_spike.rs` (mirrors `macos_vpn_spike.rs`) and a
`windows-vpn-e2e` CI job on hosted `windows-latest` — the same de-risking
approach already validated for macOS, applied to determine empirically
whether GitHub's hosted Windows runner is privileged enough to exercise real
WinTun/host-config mutation without a self-hosted runner. See
`docs/plans/plan_WindowsSupport/resume.md` for the current pass/fail result
of that job (CI-verified only, updated per run).

Building the spike surfaced a real, previously-unknown bug (now fixed):
`create_tun`'s `"auto"` adapter-name resolution passed a hardcoded
`|_| false` existence predicate, so it always resolved to `bore0` regardless
of what adapters already existed on the host. Two concurrent `bore vpn`
links on one Windows machine would silently share/reconfigure the SAME
WinTun adapter instead of getting independent ones (`open_or_create`'s "open"
half masked the collision — no error, just a wrong shared adapter). Fixed by
querying existing adapter names once via PowerShell before resolving,
mirroring Linux's real `/sys/class/net` check.

Still required before Windows VPN can be declared complete:
- A real netmap backend decision + implementation (D7 §2.6), or an explicit
  permanent decision to ship Windows VPN without overlapping-subnet support.
- A real hub spoke-isolation backend (D2), or an explicit decision to ship
  Windows hub mode as "isolation not enforced, use at your own risk."
- Cross-OS VPN relay/direct/hub/NAT acceptance on real hardware — **decided
  2026-07-01: manual acceptance only** (no self-hosted runner, no
  cross-runner tunnel infra exists). See
  `docs/vpn/VPN_WINDOWS_ACCEPTANCE.md` for the exact repro commands and
  result log for every cross-OS row.
