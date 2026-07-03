# bore vpn on Android — backend reference

Companion to the [operational plan](plans/plan_AndroidSupport/). This documents the **Android
host-only VPN client** implementation, now SHIPPED AND VALIDATED on CI (2026-07-03).

> **Status (2026-07-03):** RUNTIME SHIPPED. `bore vpn` runs on **Android** (API 24+, rooted device
> or emulator, root or equivalent privilege). Module gate: `cfg(all(feature="vpn",
> any(target_os="linux", target_os="macos", target_os="android")))`. CI validation: rooted
> x86_64 emulator (API 30, `android-vpn-e2e` job, 2026-07-03, 8/8 tests passing). Linux byte-identical,
> zero regression (netns 161/0).

---

## Design scope: host-only, never a gateway

Android VPN is **permanently host-only by hard invariant** (D-A4/D-A6/D-A9 in CLAUDE.md):

- ✅ **Supported:** VPN client (listen + connect), relay uplink, optional direct QUIC upgrade,
  same AEAD encryption as Linux/macOS/Windows
- ❌ **NOT supported (and never will be):** gateway mode (`--advertise`), NAT/masquerade
  (`--nat-masquerade`), firewall accept rules (`--forward-accept`), hub mode (`--max-clients > 1`),
  multi-queue TUN (`--tun-queues > 1`)

Any attempt to pass an unsupported flag fails at CLI validation, with examples:

- `--advertise <any>` → `Android VPN is host-only: --advertise is not supported`
- `--nat-masquerade` → `--nat-masquerade is not supported on Android (host-only)`
- `--forward-accept` → `--forward-accept is not supported on Android (host-only)`
- `--max-clients N` (N > 1) → `hub mode is not supported on Android`
- `--tun-queues N` (N > 1) → `multi-queue TUN is not supported on Android`

This is enforced twice: once at the top of `run_listen`/`run_connect` (before any network
setup), and again defensively in `NetConfig::apply` at the host-config layer.

---

## Rooting and privilege

`bore vpn` requires **root access** to create and manage the TUN device. On a rooted device,
this means:

```bash
adb shell "su -c 'bore vpn listen …'"
```

On an unrooted device, `bore vpn` exits immediately with:

```
error: not running as root (uid=<uid>). bore vpn requires root or CAP_NET_ADMIN
```

### Rooting options

**Rooted Android device (physical or emulator):**
- Physical: use your device's standard rooting method (Magisk, custom ROM, etc.)
- Emulator: start with `-selinux off -writable-system` and the `default` (non-`google_apis`) image.
  The `android-vpn-e2e` CI job uses `system-images;android-30;default;x86_64`.

**Termux + su fallback:**
Some users run bore under Termux (Linux userspace on Android) with Magisk `su` bridging to
the system. This is out of scope for this doc — Termux's model requires its own network
namespace + magic (not directly supported by bore), and the Termux-to-root bridge is
device-specific. If you need this path, consult Termux documentation + your root solution.

---

## TUN device creation

### Device node: `/dev/tun` (not desktop Linux'''s `/dev/net/tun`)

Android'''s minimal `/dev` has no `net` subdirectory on stock ROMs, so the TUN clone device
lives at `/dev/tun` — unlike desktop Linux, where it'''s `/dev/net/tun`. `bore-android-tun`
tries `/dev/tun` first and falls back to `/dev/net/tun` for kernels/ROMs that still provide it.

### bore-android-tun crate

The workspace declares `#![forbid(unsafe_code)]` at the root, so unsafe syscalls live
in a separate crate, `crates/bore-android-tun`, which safely wraps:

1. Opens the TUN clone device — `/dev/tun` if present, else `/dev/net/tun` (some ROMs/kernels
   lack the minimal `/dev`'s `net` subdirectory) — a safe `std::fs::OpenOptions::open`.
2. `TUNSETIFF` ioctl with `IFF_TUN | IFF_NO_PI` (unsafe raw ioctl FFI) to bind the fd to `name`.
3. Wraps the fd in `tun_rs::AsyncDevice::from_fd` (unsafe: takes ownership of the raw fd) and
   returns the safe `AsyncDevice` plus the kernel-resolved name to `src/vpn.rs`.

This mirrors the `crates/bore-wintun` model on Windows — isolate the FFI boundary, return
safe types to the main crate.

### Device naming

Unlike macOS (kernel-assigned `utunN`, read back after creation), Android's TUN device is
created with an explicit name, same as Linux: `pick_tun_name` resolves `--tun-name` (default
`auto`) against `/sys/class/net/<name>` and picks the first free `bore0`, `bore1`, etc. if the
requested name is taken. An explicit `--tun-name boreX` is honored as long as it's free.

---

## Host-config backend (NetConfig::apply)

### Route setup (no ip_forward, no firewall)

Android's `NetConfig::apply` twin for Android:

1. **TUN device creation** via `bore_android_tun::create` (unsafe wrapper).
2. **Address assignment** via `ip addr add <overlay-cidr> dev <tun-name>` (toybox `ip` applet).
3. **MTU tuning** via `ip link set dev <tun-name> mtu <n>` (toybox `ip` applet).
4. **Bring up interface** via `ip link set dev <tun-name> up` (toybox `ip` applet).
5. **Relax reverse-path filtering** — writes `0\n` directly to
   `/proc/sys/net/ipv4/conf/all/rp_filter` and `/proc/sys/net/ipv4/conf/<tun-name>/rp_filter`
   (best-effort; AOSP defaults strict, effective mode is `max(all, <if>)`). Defense in depth,
   not the actual netd fix below.
6. **Add critical routing rule** via `ip rule add to <subnet> lookup main priority 100`
   (see the netd finding below). Tolerates an exact-duplicate `File exists` error as success.
7. **Accepted peer routes** (if connector, applied later via `NetConfig::apply`) via
   `ip route add <cidr> dev <tun-name>` (toybox `ip` — note `add`, not the Linux twin's
   idempotent `replace`; toybox doesn't support `ip route replace`).

No changes to:
- `ip_forward` (Android does not fork this from `/proc/sys/net/ipv4/ip_forward`)
- `nftables` (not available on stock Android)
- `iptables` (available but not used for VPN routing on Android)
- PF, `netsh`, WinTun (platform-specific, not Android)

### The netd routing-policy database quirk

**Symptom:** Host-initiated ping to a peer-tunneled subnet hangs; guest replies work.

**Root cause:** Android's `netd` manages all routing policy rules via explicit `ip rule` entries
and does NOT preserve the kernel default `32766: from all lookup main` fallback rule. A
reply packet (generated by the kernel, not attached to any user socket) with mark=0 hits
netd's `15000: from all fwmark 0x0/0x10000 lookup legacy_system` rule first, which matches
any default packet and never reaches the TUN's connected route in the "main" table.

**Fix:** On Android TUN creation, add:

```bash
ip rule add to <peer-subnet> lookup main priority 100
```

This rule is scoped to the peer subnet and beats every netd rule (netd uses priority 10000+),
so the TUN's return-path packets reach "main" while normal app traffic routing is untouched.
Rp_filter was also relaxed as defense-in-depth, though the rule is the actual fix.

### Duplicate rule handling

Unlike `ip addr add` (which silently no-ops on an exact duplicate), `ip rule add` errors
with `RTNETLINK answers: File exists` if the rule already exists. The rule lives in the
kernel's routing-policy database, not on the device, so it survives TUN teardown. If a
link tears down and restarts (or two links reuse the same pool address), the second `ip rule add`
hits the existing rule.

**Fix:** Issue the `ip rule add` via `std::process::Command` directly and tolerate stderr
containing "File exists" as success — the rule is idempotent by construction.

### No RAII state files on Android

Linux uses `/run/bore-vpn-*` state files to track refcounted `ip_forward` and leaked rules
during SIGKILL recovery. Android has no writable `/run` under SELinux + app sandbox (Magisk/
Termux environments may differ, but unrooted Android has no `/run`). Instead:

- **State path:** `/data/local/tmp` (readable/writable even in typical SELinux policies).
- **Stored on apply:** only marker files for SIGKILL reclaim (routes and rules are expected to
  persist across crashes until manual cleanup or the next successful `connect`).
- **Restored on SIGKILL:** `stale_reclaim` flushes leaked state files but has no `ip_forward`
  to restore (it was never touched).

---

## Supported and unsupported flags

### Supported (work identically to Linux)

- `--relay-only` — skip direct QUIC upgrade, stay on relay (optional, default allows both)
- `--auto-reconnect` — automatically reconnect on link death (optional, default off)
- `--carriers N` — parallel relay carriers (default 1; rarely helps a VPN with a single inner flow)
- `--pin-mtu` — lock TUN MTU (avoid dynamic PMTU monitor; for tests)
- UDP hole-punch discovery flags (`--upnp`, `--stun-server`, `--try-port-prediction`,
  `--nat-udp-preferred-port`, `--nat-udp-release-timeout`) — accepted, best-effort;
  behavior on Android's NAT/firewall untested (CI is single-host; manual testing deferred)

### NOT supported (rejected at CLI)

| Flag | Why | Error message |
|------|-----|---|
| `--advertise <cidr>` (any non-empty) | Gateway mode forbidden | `Android VPN is host-only: --advertise is not supported` |
| `--nat-masquerade` | Gateway feature | `--nat-masquerade is not supported on Android (host-only)` |
| `--forward-accept` | Firewall feature | `--forward-accept is not supported on Android (host-only)` |
| `--max-clients N` where `N > 1` | Hub mode forbidden | `hub mode is not supported on Android` |
| `--tun-queues N` where `N > 1` | Multi-queue not supported | `multi-queue TUN is not supported on Android` |

All five are rejected at the CLI (`validate_android_host_only`, `src/vpn.rs`), before any TUN
is created — none of these is a warning-only/best-effort check.

Attempting any of these fails before the TUN is created.

---

## Build and install

### From prebuilt binary

The release page includes `bore-aarch64-linux-android` for most Android devices (ARM64).
Download and place on the device:

```bash
# On your dev machine
curl -fL https://github.com/manprint/bore/releases/latest/download/bore-aarch64-linux-android \
  -o bore
adb push bore /data/local/tmp/
adb shell chmod +x /data/local/tmp/bore

# On the device (or via adb shell)
su -c '/data/local/tmp/bore vpn listen …'
```

For emulator (x86_64), the prebuilt binary is not yet in releases; build from source (see below).

### From source

**Prerequisites:**

- Rust toolchain with Android NDK integration: `rustup target add aarch64-linux-android`
- Android NDK (via Android Studio, or standalone)
- `cargo-ndk` (optional, bore's `Justfile` uses it): `cargo install cargo-ndk`

**Build for ARM64 (most real devices):**

```bash
just android-arm64  # Builds into ./bin/bore-android-arm64
```

This uses Docker + the NDK to cross-compile. If you don't have Docker, or prefer a local build
with an installed NDK:

```bash
export ANDROID_NDK_HOME=/path/to/ndk  # typically ~/Android/Sdk/ndk/<version>
cargo ndk -t aarch64 build --release --features vpn
```

Binary lands in `target/aarch64-linux-android/release/bore`.

**Build for x86_64 (emulator):**

```bash
just android-x86_64
```

Builds into `target/x86_64-linux-android/release/bore`. Used by CI for emulator validation.

**Transfer to device:**

```bash
adb push target/aarch64-linux-android/release/bore /data/local/tmp/
adb shell chmod +x /data/local/tmp/bore
```

---

## Runtime usage

The CI e2e (`scripts/android_vpn_test.sh`) validates Android as the **connector** side, dialing
into a `bore vpn listen` running on another host — this is the primary supported pattern. Android
can also run `bore vpn listen`, as long as `--advertise` (and the other gateway-only flags) are
never passed — host-only applies to both subcommands identically.

**Host side** (any platform, e.g. the Linux machine the phone/emulator reaches):

```bash
bore server &
bore vpn listen --to <bore-server-host:port> --secret <shared-secret> --id <link-id>
```

**Android side** (connector, as root):

```bash
adb shell "su -c '/data/local/tmp/bore vpn connect --to <bore-server-host:port> --secret <shared-secret> --id <link-id> --accept-all-routes'"
```

or inside Termux with Magisk `su`:

```bash
su -c '/data/local/tmp/bore vpn connect --to <bore-server-host:port> --secret <shared-secret> --id <link-id>'
```

`--accept-all-routes` is only needed if the far side advertises routes (it never does when the
far side is also Android — host-only means no side advertises anything). Never pass `--advertise`,
`--nat-masquerade`, `--forward-accept`, `--max-clients > 1`, or `--tun-queues > 1` on Android —
see the rejected-flags table above.

---

## Validation status

**CI (rooted x86_64 emulator, API 30):**
- ✅ TUN creation + interface config + relay uplink (`T-AND-S1`, `T-AND-L1`)
- ✅ Direct QUIC upgrade + bidirectional ping (`T-AND-L2`, informational best-effort)
- ✅ CLI guard matrix: non-root, `--advertise`, `--tun-queues > 1` all rejected (`T-AND-L3..L5`)
- ✅ Host-only invariants enforced at apply time
- ✅ SIGKILL reclaim (leak cleanup) functional
- ✅ Zero regressions on Linux (netns 161/0 pass)

**Manual device acceptance (physical hardware, Phase 5.2):**
- TODO — `VPN_ANDROID_ACCEPTANCE.md` to be filled in with real physical-device testing
  (multi-carrier relay, direct path on real networks, carrier switching, failover)

**Known open questions (Phase 5.2+):**
- Real carrier UDP buffer clamps on Android (similar to Linux 10 MB/s @ 20 ms clamp)
- Direct-path throughput + uptime on real device networks
- Magisk/Termux bridge behavior
- SELinux policy interactions with `/data/local/tmp` state files

---

## Troubleshooting

### Not running as root

```
error: not running as root (uid=<uid>). bore vpn requires root or CAP_NET_ADMIN
```

**Fix:** Run with `su -c` or ensure the shell is root.

### TUN device not found

```
error: bore-android-tun: failed to open /dev/tun: permission denied
```

**Fix:** Confirm you are running as root, and the device has `/dev/tun` (should be present on any stock
Android with Linux kernel TUN support).

### Stale `ip rule` entries accumulate across links

You will never see `RTNETLINK answers: File exists` as a bore-level error — bore itself
tolerates an exact-duplicate `ip rule add to <subnet> lookup main priority 100` internally
(treats it as success, silently, no log line) since this rule is idempotent by construction.

What IS a real, permanent gap: this rule lives in the kernel's routing-policy database, not
attached to the TUN device, so **nothing ever removes it** — not `NetConfig`'s RAII teardown,
not `stale_reclaim`. Every distinct subnet a link has ever used stays in `ip rule show` until
the device reboots. Harmless in practice (each rule is scoped to one small `/link` subnet and
low-priority), but if you reuse many overlay subnets over a long-running device's uptime, list
and manually prune old entries:

```bash
adb shell "su -c 'ip rule show'"
adb shell "su -c 'ip rule del to <stale-subnet> lookup main priority 100'"
```

### Direct path not upgrading

If the link stays on relay and never tries to upgrade to direct:

- Check `--relay-only` (forces relay only).
- On a UDP-hostile network, direct upgrade may never succeed but the link stays stable on relay
  (this is correct behavior, not a failure — see `direct_upgrade_task`'''s 30s retry grid in
  CLAUDE.md, which applies unchanged on Android).

---

## Testing

### Unit tests (cross-platform, on Linux CI)

```bash
cargo test --features vpn --lib vpn::
```

All Android host-config logic is pure functions or gated by a `target_is_android()` parameter,
so unit tests run on Linux CI without an actual Android device.

### Emulator e2e (`android-vpn-e2e` CI job)

Runs on every push to CI: rooted x86_64 emulator, API 30, covers TUN creation, relay/direct
paths, CLI guards, and SIGKILL reclaim.

### Manual device acceptance (Phase 5.2)

See `VPN_ANDROID_ACCEPTANCE.md` (not yet written; will cover real 2+ device scenarios,
carrier failover, and network switching).

---

## See also

- [CLAUDE.md — VPN Android port](../CLAUDE.md) — terse invariant list (D-A4/D-A6/D-A9) and status
- [plan_AndroidSupport](plans/plan_AndroidSupport/) — full project plan + status + resume
- [SPIKE_FINDINGS.md](plans/plan_AndroidSupport/SPIKE_FINDINGS.md) — CI findings (netd quirk, ip rule duplicate handling)
- [Limits and unsupported features](vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md)
