# VPN Android — Actual Limits vs Linux

**Status: IMPLEMENTED (host-only).** `bore vpn` ships on Android as a root-only CLI client —
see `docs/ANDROID.md` for the user guide. This doc lists what remains a hard limit vs what's
now testable, so you know what NOT to test (and why) as opposed to what's simply unverified
pending physical-device access.

Purpose: know what NOT to test on Android. Linux = reference full impl. This doc lists
gaps only — anything not listed here works same as Linux.

Sources: `src/vpn.rs`, `crates/bore-android-tun`, `docs/ANDROID.md`,
`SPIKE_FINDINGS.md`, `docs/plans/plan_AndroidSupport/resume.md`.

## Testable now (host-only relay/direct, listen + connect)

TUN creation, address/MTU/route setup, relay uplink, the direct QUIC upgrade path, carriers,
PMTU pinning, auto-reconnect, and the host-only CLI guard matrix are all implemented and CI
green (`android-vpn-e2e`, 8/8, rooted x86_64 emulator API 30). Both `bore vpn listen` and
`bore vpn connect` work on Android, as long as neither side passes a gateway-only flag.

## Architecturally impossible (not a scoping choice)

1. **Non-root VPN** — Android's only non-root path to a VPN interface is the `VpnService` Java
   API, which is grantable exclusively to a signed, permission-requesting Android app — a shell
   binary like `bore` cannot use it. `bore vpn` therefore always requires root/`tsu`, on every
   Android build, permanently — this is not a v1 scoping gap, there is no non-root path to add
   later short of shipping a companion VpnService app (out of scope for this project).

## Out of scope for v1 (deferred by design, not a technical impossibility — D-A4)

2. **`--advertise R@V` (overlapping-subnet netmap) / gateway mode** — Android VPN is host-only
   by hard invariant (D-A4/D-A6/D-A9); gateway mode is forbidden at CLI. Unlike item 1 above,
   this is a deliberate v1 scoping decision, not a technical wall — Android's `ip`/routing
   primitives could support a gateway role in principle. (`vpn.rs:628-660`, `vpn.rs:3554-3560`,
   `docs/ANDROID.md` design scope)

3. **`--nat-masquerade`** — no NAT backend wired up on Android; gateway feature, forbidden at
   CLI alongside `--advertise`. (`vpn.rs:628-660`)

4. **`--forward-accept`** — Android has no firewall rule API in use for VPN (nftables/iptables
   not wired up); gateway feature, forbidden at CLI. (`vpn.rs:628-660`)

5. **`--max-clients N>1` hub mode** — Android is single-client only; hub mode forbidden at CLI.
   (`vpn.rs:628-660`)

## Not supported due to platform/tooling limits

6. **`--tun-queues N>1`** — rejected at CLI (not just clamped/`warn!`). toybox `ip` does not
   support multi-queue configuration, and single-queue is the only Android TUN mode.
   (`vpn.rs:4966`, `vpn.rs:5045-5051`)

7. **GSO/GRO offload pumps** (`recv_multiple`/`send_multiple`/`GROTable`/
   `VIRTIO_NET_HDR_LEN`) — not compiled for Android. `create_tun` forces
   `offload=false`; bridge always takes single-packet path. Don't test
   offload-dependent throughput claims. (`vpn.rs:4966-4968`, `vpn.rs:8311-8317`,
   `vpn.rs:8399-8407`, unreachable twins)

8. **`SO_RCVBUFFORCE`/`SO_SNDBUFFORCE` socket tuning** — not available. Android uses
   plain `socket2` setters (no FORCE variant). Don't test the "forced large UDP buffer
   past OS default" behavior. (`holepunch.rs:253-270`, platform-specific socket API)

## Unverified pending physical-device access (not a known limit, just untested)

9. **Multi-host LAN gateway acceptance (T-AND-M1..M5)** — MANUAL ONLY, not CI-covered.
   CI (rooted x86_64 emulator) is single-host: validates TUN creation/relay/direct
   paths, NOT cross-host spoke traffic or real network conditions. Treat any 2+host
   Android gateway scenario as unverified until run by hand per `VPN_ANDROID_ACCEPTANCE.md`.

10. **Real carrier UDP buffer clamps** — unknown. Android kernel may clamp UDP buffers
    like Linux (stock Linux: 208 KiB → ~10 MB/s at 20 ms RTT). Mitigation via
    `--carriers N` (relay only) or raised sysctl (`sysctl -w net.core.rmem_max=16777216`,
    root-only) is untested on real Android hardware.

11. **Direct-path throughput + uptime on real networks** — not CI-validated. Emulator
    NAT allowed one DIRECT upgrade but provides no signal for real-world NAT types
    (CGNAT, symmetric, etc.). Treat direct path as best-effort; relay is the guarantee.

## Known permanent gap (implemented, but leaks state)

12. **`ip rule add to <subnet> lookup main priority 100` is never removed** — Android-only
    state (Linux needs no such rule at all — see the "Works differently" note below). This
    rule lives in the kernel's routing-policy database, not attached to the TUN device, so
    neither `NetConfig`'s RAII teardown nor `stale_reclaim` ever deletes it: every distinct
    overlay subnet a device has used stays in `ip rule show` until reboot. Harmless in
    practice (idempotent, low-priority, subnet-scoped) but a real, permanent teardown gap
    vs Linux/macOS/Windows, which fully revert their routing state on link exit. See
    `docs/ANDROID.md`'s troubleshooting section. (`vpn.rs:5260-5285`)

## Works same as Linux (no gap)

- Single-queue TUN creation/teardown via `/dev/tun` (fallback `/dev/net/tun`), `ip addr` +
  `ip link`, `ip route add`
- Relay/direct VPN data plane (AEAD, carriers, QUIC) — fully shared code with Linux, no gap
- `--pin-mtu` / PMTU monitor
- `--relay-only` (force relay-only mode)
- `--auto-reconnect` (auto-reconnect on link death)
- SIGKILL recovery (`stale_reclaim`): always a clean no-op on Android — `apply()` never writes
  an `.ipforward`/`.fwdref` marker (host-only, no `ip_forward` ever touched), so there's nothing
  for it to find. Does NOT cover the `ip rule` leak in gap #12 above (that's separate kernel
  routing-policy state, never registered for revert by any code path)
- UDP hole-punch helper flags (`--upnp`, `--stun-server`, etc.) — accepted, best-effort
  (cross-platform socket code, behavior untested on real Android networks)

## Works differently than Linux (Android-specific, required for correctness)

- `ip rule add to <subnet> lookup main priority 100` — Android-ONLY. Linux needs no such
  rule: the kernel's implicit `32766: from all lookup main` fallback already covers it.
  Android's `netd` deletes that fallback and replaces it with per-UID/fwmark policy rules,
  so bore adds this rule itself as the fix (with exact-duplicate tolerance, see gap #12
  above for the accompanying cleanup gap and SPIKE_FINDINGS.md for the full finding).

## Status snapshot

Validated on `android-30` x86_64 emulator CI (2026-07-03, branch `android`): build +
clippy + unit tests green, `android-vpn-e2e` job (TUN/relay/direct/guards/reclaim) 8/8
PASS under sudo. Physical device acceptance (multi-host scenarios, real NAT/networks) is
the remaining gap — everything else above is either implemented-and-verified (relay,
guards) or a v1 scoping decision deferred, not impossible (gateway mode, D-A4) — the one
true hard wall is non-root VPN (`VpnService` is app-only, item 1 above).
