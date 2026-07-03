# VPN Android — Actual Limits vs Linux

Purpose: know what NOT to test on Android. Linux = reference full impl. This doc lists
gaps only — anything not listed here works same as Linux.

Sources: `src/vpn.rs`, `crates/bore-android-tun`, `docs/ANDROID.md`,
`SPIKE_FINDINGS.md`, `docs/plans/plan_AndroidSupport/resume.md`.

## Do NOT test on Android (not implemented / permanently out of scope)

1. **`--advertise R@V` (overlapping-subnet netmap)** — PERMANENTLY NOT SUPPORTED. Android
   VPN is host-only by hard invariant; gateway mode is forbidden at CLI.
   (`vpn.rs:628-660`, `vpn.rs:3554-3560`, `docs/ANDROID.md` design scope)

2. **`--nat-masquerade`** — PERMANENTLY NOT SUPPORTED. No NAT backend on Android.
   Gateway features forbidden at CLI. (`vpn.rs:628-660`)

3. **`--forward-accept`** — PERMANENTLY NOT SUPPORTED. Android has no firewall rule
   API (nftables/iptables not used for VPN). Gateway features forbidden at CLI.
   (`vpn.rs:628-660`)

4. **`--max-clients N>1` hub mode** — PERMANENTLY NOT SUPPORTED. Android is single-client
   only. Hub mode forbidden at CLI. (`vpn.rs:628-660`)

5. **`--tun-queues N>1`** — clamped to 1, `warn!`. toybox `ip` does not support
   multi-queue configuration, and single-queue is the only Android TUN mode.
   (`vpn.rs:4966`, `vpn.rs:5045-5051`)

6. **GSO/GRO offload pumps** (`recv_multiple`/`send_multiple`/`GROTable`/
   `VIRTIO_NET_HDR_LEN`) — not compiled for Android. `create_tun` forces
   `offload=false`; bridge always takes single-packet path. Don't test
   offload-dependent throughput claims. (`vpn.rs:4966-4968`, `vpn.rs:8311-8317`,
   `vpn.rs:8399-8407`, unreachable twins)

7. **`SO_RCVBUFFORCE`/`SO_SNDBUFFORCE` socket tuning** — not available. Android uses
   plain `socket2` setters (no FORCE variant). Don't test the "forced large UDP buffer
   past OS default" behavior. (`holepunch.rs:253-270`, platform-specific socket API)

8. **Multi-host LAN gateway acceptance (T-AND-M1..M5)** — MANUAL ONLY, not CI-covered.
   CI (rooted x86_64 emulator) is single-host: validates TUN creation/relay/direct
   paths, NOT cross-host spoke traffic or real network conditions. Treat any 2+host
   Android gateway scenario as unverified until run by hand per `VPN_ANDROID_ACCEPTANCE.md`.

9. **Real carrier UDP buffer clamps** — unknown. Android kernel may clamp UDP buffers
   like Linux (stock Linux: 208 KiB → ~10 MB/s at 20 ms RTT). Mitigation via
   `--carriers N` (relay only) or raised sysctl is untested on real Android hardware.

10. **Direct-path throughput + uptime on real networks** — not CI-validated. Emulator
    NAT allowed one DIRECT upgrade but provides no signal for real-world NAT types
    (CGNAT, symmetric, etc.). Treat direct path as best-effort; relay is the guarantee.

11. **`ip rule add to <subnet> lookup main priority 100` is never removed** — Android-only
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
- SIGKILL recovery (`stale_reclaim`): state file cleanup (no ip_forward to restore; does NOT
  cover the `ip rule` leak in gap #11 above — that state was never registered for revert)
- UDP hole-punch helper flags (`--upnp`, `--stun-server`, etc.) — accepted, best-effort
  (cross-platform socket code, behavior untested on real Android networks)

## Works differently than Linux (Android-specific, required for correctness)

- `ip rule add to <subnet> lookup main priority 100` — Android-ONLY. Linux needs no such
  rule: the kernel's implicit `32766: from all lookup main` fallback already covers it.
  Android's `netd` deletes that fallback and replaces it with per-UID/fwmark policy rules,
  so bore adds this rule itself as the fix (with exact-duplicate tolerance, see gap #11
  above for the accompanying cleanup gap and SPIKE_FINDINGS.md for the full finding).

## Status snapshot

Validated on `android-30` x86_64 emulator CI (2026-07-03, branch `android`): build +
clippy + unit tests green, `android-vpn-e2e` job (TUN/relay/direct/guards/reclaim) 8/8
PASS under sudo. Physical device acceptance (multi-host scenarios, real NAT/networks) is
the remaining gap — everything else above is either implemented-and-verified (relay,
guards) or permanently out-of-scope (gateway mode).
