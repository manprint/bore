# VPN macOS — Actual Limits vs Linux

Purpose: know what NOT to test on macOS. Linux = reference full impl. This doc lists
gaps only — anything not listed here works same as Linux.

Sources: `src/vpn.rs`, `src/holepunch.rs`, `docs/vpn/VPN_MACOS.md`,
`VPN_MACOS_SPIKE_FINDINGS.md`, `VPN_MACOS_ACCEPTANCE.md`,
`docs/plans/plan_VpnMacosCompletion/resume.md`.

## Do NOT test on macOS (not implemented / not verified)

1. **`--tun-queues N>1`** — clamped to 1, `warn!`. utun has no multi-queue in `tun-rs`.
   (`vpn.rs:569-570`, `vpn.rs:4900-4909`)

2. **UDP hole-punch helper flags** (`--upnp`, `--stun-server`, `--try-port-prediction`,
   `--nat-udp-preferred-port`, `--nat-udp-release-timeout`) — parsed + `warn!`'d as
   advisory/best-effort. Behavior NOT validated on real macOS hardware (CI is
   single-host). Don't rely on results. (`vpn.rs:572-576`)

3. **GSO/GRO offload pumps** (`recv_multiple`/`send_multiple`/`GROTable`/
   `VIRTIO_NET_HDR_LEN`) — not compiled for macOS. `create_tun` forces
   `offload=false`; bridge always takes single-packet path. Don't test
   offload-dependent throughput claims. (`vpn.rs:4900-4922`, `vpn.rs:8311-8317`,
   `vpn.rs:8399-8407`, unreachable twins)

4. **`SO_RCVBUFFORCE`/`SO_SNDBUFFORCE` socket tuning** — not available. macOS uses
   plain `socket2` setters (no FORCE variant), so a low `net.inet.udp.*` limit can't
   be forced past like on Linux CAP_NET_ADMIN. Don't test the "forced 32MB buffer"
   behavior — only the plain best-effort setter runs. (`holepunch.rs:253-270`)

5. **Two-host LAN gateway acceptance (T-MAC-MANUAL)** — MANUAL ONLY, not CI-covered.
   CI (`macos-14`) is single-host: validates PF ruleset grammar + create/apply/revert/
   reclaim, NOT actual cross-host spoke traffic. Treat any 2-host macOS gateway
   scenario as unverified until run by hand per `VPN_MACOS_ACCEPTANCE.md`.

6. **Host-only hub (`--max-clients N>1`, no `--advertise`) spoke isolation** — relies
   on host `ip_forward=0`; NO PF/nft table created for host-only case (known v1 gap,
   same limitation documented for Linux too — not macOS-specific, but flag if testing
   isolation).

## Works same as Linux (no gap)

- Single-queue TUN creation/teardown, routes, `sysctl net.inet.ip.forwarding`
  (refcounted like Linux `ip_forward`, `/var/run` instead of `/run`)
- PF-based NAT: blanket masquerade (`nat`), overlapping-subnet netmap (`binat`,
  1:1 real@exposed), MSS clamp (`scrub max-mss`)
- Hub spoke isolation via PF `block` (when `--advertise`/routed hub, not host-only)
- `--forward-accept` via PF `pass`
- `--pin-mtu` / PMTU monitor
- SIGKILL recovery (`stale_reclaim`): PF anchor flush + forwarding restore from
  `/var/run` state
- 1:1 relay/direct VPN data plane (AEAD, carriers, QUIC) — fully shared code with
  Linux, no gap

## Status snapshot

Validated on `macos-14` CI (2026-06-29, branch `macos`): build + clippy + unit tests
green, `macos-vpn-e2e` spike (create/teardown, apply/revert, leak/reclaim) green under
sudo. Real 2-host manual acceptance (`T-MAC-MANUAL`) is the only remaining gap —
everything else above is either implemented-and-verified or explicitly
unsupported/warned.
