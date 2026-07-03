# VPN Windows — Actual Limits vs Linux

Purpose: know what NOT to test on Windows. Linux = reference full impl. This doc
lists gaps only — anything not listed here works same as Linux.

Sources: `src/vpn.rs`, `src/holepunch.rs`, `docs/vpn/VPN_WINDOWS.md`,
`VPN_WINDOWS_ACCEPTANCE.md`, `docs/plans/plan_WindowsSupport/resume.md`.

## Do NOT test on Windows (not implemented / not enforced / unverified)

1. **`--advertise R@V` (overlapping-subnet netmap)** — NOT IMPLEMENTED. Windows has
   no stateless 1:1 prefix DNAT/SNAT backend (no WFP/WinDivert impl yet, `New-NetNat`
   only does plain masquerade). netmap'd real subnets are silently skipped; only plain
   subnets get masqueraded. Do NOT test overlapping-LAN scenarios on Windows.
   (`vpn.rs:6029-6032`, `VPN_WINDOWS.md` D7 §2.6)

2. **`--max-clients N>1` hub mode spoke isolation** — hub mode ITSELF works (compiles,
   relays, per-peer routing), but spoke→spoke isolation is NOT enforced. Windows
   Firewall (`New-NetFirewallRule`) has no combined ingress+egress interface predicate
   for routed/transit traffic; a naive rule would also block spoke→LAN. Runtime
   `warn!`s instead of silently claiming isolation. Do NOT trust hub isolation on
   Windows — any spoke can currently reach any other spoke. (`vpn.rs:5923-5928`,
   `VPN_WINDOWS.md` D2)

3. **`--tun-queues N>1`** — clamped to 1, `warn!`. WinTun has no multi-queue support.
   (`vpn.rs:4961-4965`, `vpn.rs:596-602`)

4. **GSO/GRO offload pumps** (`recv_multiple`/`send_multiple`/`GROTable`/
   `VIRTIO_NET_HDR_LEN`) — not compiled for Windows. `create_tun` forces
   `offload=false`; bridge always takes single-packet path. Don't test
   offload-dependent throughput claims. (`vpn.rs:4992-4993`, `vpn.rs:8311-8317`,
   `vpn.rs:8399-8407`, unreachable twins)

5. **`SO_RCVBUFFORCE`/`SO_SNDBUFFORCE` socket tuning** — not available. Windows uses
   generic `socket2` setters (no FORCE semantics on this platform). Don't test the
   "forced large UDP buffer past OS default" behavior. (`holepunch.rs:168-188`)

6. **`--forward-accept` on routed (non-host-destined) traffic** — UNVERIFIED. Windows
   Defender Firewall's `New-NetFirewallRule` is primarily WFP ALE-layer (host-bound
   traffic); whether it actually affects merely-forwarded packets through the gateway
   is not yet validated on real hardware. Treat any forward-accept-through-gateway
   test result as unconfirmed until T-WIN-FWD1/T-WIN-FWD2 run manually.
   (`VPN_WINDOWS.md` lines 108-118)

## Works, but only CI-verified (no local dev-box verification — flag as lower confidence)

- WinTun adapter create/teardown, `netsh` routes, `IPEnableRouter` forwarding
  (refcounted, state files), plain-subnet `New-NetNat` masquerade — implemented,
  unit-tested, verified on hosted `windows-latest` CI (`windows-vpn-e2e`), NOT run on
  a local Windows box during dev (Linux dev machine, no MSVC toolchain).
- UDP hole-punch helper flags (`--upnp`, `--stun-server`, etc.) — accepted, NOT
  `warn!`'d as unsupported (shares cross-platform `socket2` UDP code with Linux), but
  only exercised via CI, not manual hardware acceptance yet.

## Works same as Linux (no gap)

- 1:1 relay/direct VPN data plane (AEAD, carriers, QUIC) — fully shared code, no gap
- `--pin-mtu` / PMTU monitor
- SIGKILL recovery (`stale_reclaim`): forwarding + route state restore
- Adapter auto-naming (fixed 2026-07-01, commit `90e59f3`: real existing-adapter check
  via PowerShell, was previously a hardcoded `|_| false` that let two concurrent
  `bore vpn` links silently share one WinTun adapter)

## Status snapshot

Phases 0-4 done, Phase 5 elevated single-host e2e done 2026-07-01 (hosted
`windows-latest`, no self-hosted runner needed). Phase 6 mostly done (DLL bundled,
acceptance doc written). Two items above (netmap, hub isolation) are explicit
deferrals — not "not yet gotten to," but decided out-of-scope for this pass pending a
WFP/WinDivert backend design.
