# bore on Android

Companion to the [operational plan](plans/plan_AndroidSupport/). Covers running any `bore`
subcommand on Android (Termux or a raw shell), and documents the **host-only VPN client**
implementation in depth, now SHIPPED AND VALIDATED on CI (2026-07-03).

> **Status (2026-07-03):** RUNTIME SHIPPED. `bore vpn` runs on **Android** (API 24+, rooted device
> or emulator, root or equivalent privilege). Module gate: `cfg(all(feature="vpn",
> any(target_os="linux", target_os="macos", target_os="android")))`. CI validation: rooted
> x86_64 emulator (API 30, `android-vpn-e2e` job, 2026-07-03, 8/8 tests passing). Linux byte-identical,
> zero regression (netns 161/0). Non-VPN subcommands need no root and are covered by the
> separate `android-emu-e2e` CI job.

---

## Install (Termux)

**Prebuilt binary** (ARM64, matches most phones/tablets):

```bash
pkg install curl   # if not already present
curl -fL https://github.com/manprint/bore/releases/latest/download/bore-aarch64-linux-android \
  -o bore
chmod +x bore
./bore --version
```

No `pkg install` dependencies are required to *run* `bore` for any non-VPN subcommand — the
binary is statically-ish linked against the Android NDK's libc and needs nothing else installed.
VPN mode additionally needs root: install `tsu` (`pkg install tsu`) to bridge Termux to a
Magisk/KernelSU root grant — see "Root VPN quickstart" below.

The generic install script (`docs/INSTALL_BORE.md`) also auto-detects Android/arm64 and works
unmodified under Termux, since it's bash-only:

```bash
curl -fsSL https://raw.githubusercontent.com/manprint/bore/main/install.sh | bash
```

---

## Feature matrix

| Subcommand | Non-root | Root |
|---|---|---|
| `local` | works (unless the local target port is `<1024`) | works (no benefit over non-root) |
| `proxy` | works (same `<1024` caveat on the local forward port) | works (no benefit) |
| `vhost` | works (same `<1024` caveat) | works (no benefit) |
| `transfer listener\|sender` | works (file transfer, no privileged ports) | works (no benefit) |
| `test-udp` | works, but bandwidth numbers are limited by the UDP buffer clamp | works — can raise `net.core.*mem_max` first for accurate throughput |
| `server` | works (default control port 7835 is unprivileged) | works — only needed to bind a port `<1024` |
| `vpn` | **no — impossible** (Android's non-root VPN path is the `VpnService` Java API, which only a signed, permission-granted app can use; `bore` is a native CLI binary and can't hook into it) | works (host-only, see below) |

None of the non-VPN subcommands touch a TUN device, `ip_forward`, or any routing table — they're
plain userspace TCP/UDP sockets, so they behave like any other Linux CLI network tool under
Termux. See [Limits and unsupported features](vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md) for
the full non-root-VPN rationale.

---

## Non-root notes

- **Ports `<1024` need root** (`CAP_NET_BIND_SERVICE` or uid 0) — same as any Linux kernel.
  Only matters if you point `local`/`proxy`/`vhost`/`server` at a privileged port; the bore
  server's own control port (7835) and all data substreams are unprivileged.
- **UDP socket buffers are clamped** by `net.core.{r,w}mem_max` (same mechanism as desktop
  Linux, see `CLAUDE.md`'s VPN throughput notes) — non-root can't raise this, so `test-udp`
  bandwidth numbers and VPN relay throughput are capped at the stock clamp. Rooted remediation:
  `sysctl -w net.core.rmem_max=16777216` (and the matching `wmem_max`).
- **Android 12+ phantom process killer** terminates long-running Termux child processes that
  aren't tied to a foreground activity — a `bore local`/`proxy`/`vpn connect` left running in the
  background can be killed without warning. Two remediations:
  - `termux-wake-lock` (keeps Termux's own process alive; install the Termux:API add-on if not
    already present)
  - `adb shell device_config put activity_manager max_phantom_processes 2147483647` (effectively
    disables the killer), or the blunter `adb shell settings put global
    settings_enable_monitor_phantom_procs false`
- **No `/tmp`** — Termux has no writable `/tmp`; it sets `$TMPDIR` to `$PREFIX/tmp` instead. Not
  relevant to `bore` directly (it doesn't use `/tmp`), but relevant if you're scripting around it.

---

## Root VPN quickstart

```bash
pkg install tsu
tsu   # or: su -c '...'  if you're not using tsu
```

**Host side** (any platform reachable from the phone, e.g. a Linux box running the bore server):

```bash
bore server &
bore vpn listen --to <bore-server-host:port> --secret <shared-secret> --id <link-id>
```

**Android side** (connector, as root):

```bash
tsu -c '/data/data/com.termux/files/home/bore vpn connect --to <bore-server-host:port> \
  --secret <shared-secret> --id <link-id> --accept-all-routes'
```

`--accept-all-routes` is only needed if the far side advertises routes — it never does when the
far side is also Android (host-only means no side ever advertises anything).

Android VPN is **host-only by hard invariant** (D-A4/D-A6/D-A9 in `CLAUDE.md`) — gateway mode is
out of scope for this release (a scoping decision, not a technical impossibility, unlike non-root
VPN above; see D-A4). Five flags are rejected at the CLI before any TUN is created:

| Flag | Why | Error message |
|------|-----|---|
| `--advertise <cidr>` (any non-empty) | Gateway mode forbidden | `Android VPN is host-only: --advertise is not supported` |
| `--nat-masquerade` | Gateway feature | `--nat-masquerade is not supported on Android (host-only)` |
| `--forward-accept` | Firewall feature | `--forward-accept is not supported on Android (host-only)` |
| `--max-clients N` where `N > 1` | Hub mode forbidden | `hub mode is not supported on Android` |
| `--tun-queues N` where `N > 1` | Multi-queue not supported | `multi-queue TUN is not supported on Android` |

All five are enforced twice — `validate_android_host_only` at the top of `run_listen`/
`run_connect` (before any network setup), and again defensively in `NetConfig::apply` at the
host-config layer — none of these is a warning-only/best-effort check.

Everything else (`--relay-only`, `--auto-reconnect`, `--carriers N`, `--pin-mtu`, the UDP
hole-punch discovery flags) works identically to Linux.

---

## Emulator / dev notes

Two CI jobs exercise Android without a physical device:

- **`android-emu-e2e`** (Phase 2.2, `scripts/android_emu_test.sh`) — non-VPN subcommands
  (`local`/`proxy`/`transfer`/`test-udp`/`server`) on a plain (non-rooted) emulator.
- **`android-vpn-e2e`** (Phase 4.2, `scripts/android_vpn_test.sh`) — the VPN path, needs a
  **rooted** emulator: `system-images;android-30;default;x86_64` started with
  `-selinux off -writable-system` (the `default`, non-`google_apis`, image is the one that
  allows root).

Building for the emulator (x86_64) instead of a real device (ARM64) is covered under
"Build and install" below.

---

## VPN backend reference

The sections below are implementation detail for anyone modifying or debugging the Android VPN
backend itself — not needed to just run `bore vpn` on a device.

### TUN device creation

#### Device node: `/dev/tun` (not desktop Linux's `/dev/net/tun`)

Android's minimal `/dev` has no `net` subdirectory on stock ROMs, so the TUN clone device
lives at `/dev/tun` — unlike desktop Linux, where it's `/dev/net/tun`. `bore-android-tun`
tries `/dev/tun` first and falls back to `/dev/net/tun` for kernels/ROMs that still provide it.

#### bore-android-tun crate

The workspace declares `#![forbid(unsafe_code)]` at the root, so unsafe syscalls live
in a separate crate, `crates/bore-android-tun`, which safely wraps:

1. Opens the TUN clone device — `/dev/tun` if present, else `/dev/net/tun` (some ROMs/kernels
   lack the minimal `/dev`'s `net` subdirectory) — a safe `std::fs::OpenOptions::open`.
2. `TUNSETIFF` ioctl with `IFF_TUN | IFF_NO_PI` (unsafe raw ioctl FFI) to bind the fd to `name`.
3. Wraps the fd in `tun_rs::AsyncDevice::from_fd` (unsafe: takes ownership of the raw fd) and
   returns the safe `AsyncDevice` plus the kernel-resolved name to `src/vpn.rs`.

This mirrors the `crates/bore-wintun` model on Windows — isolate the FFI boundary, return
safe types to the main crate.

#### Device naming

Unlike macOS (kernel-assigned `utunN`, read back after creation), Android's TUN device is
created with an explicit name, same as Linux: `pick_tun_name` resolves `--tun-name` (default
`auto`) against `/sys/class/net/<name>` and picks the first free `bore0`, `bore1`, etc. if the
requested name is taken. An explicit `--tun-name boreX` is honored as long as it's free.

### Host-config backend (NetConfig::apply)

#### Route setup (no ip_forward, no firewall)

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

#### The netd routing-policy database quirk

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

#### Duplicate rule handling

Unlike `ip addr add` (which silently no-ops on an exact duplicate), `ip rule add` errors
with `RTNETLINK answers: File exists` if the rule already exists. The rule lives in the
kernel's routing-policy database, not on the device, so it survives TUN teardown. If a
link tears down and restarts (or two links reuse the same pool address), the second `ip rule add`
hits the existing rule.

**Fix:** Issue the `ip rule add` via `std::process::Command` directly and tolerate stderr
containing "File exists" as success — the rule is idempotent by construction.

#### No RAII state files on Android

Linux uses `/run/bore-vpn-*` state files to track refcounted `ip_forward` and leaked rules
during SIGKILL recovery. Android has no writable `/run` under SELinux + app sandbox (Magisk/
Termux environments may differ, but unrooted Android has no `/run`). Instead:

- **State path:** `/data/local/tmp` (readable/writable even in typical SELinux policies).
- **Stored on apply:** only marker files for SIGKILL reclaim (routes and rules are expected to
  persist across crashes until manual cleanup or the next successful `connect`).
- **Restored on SIGKILL:** `stale_reclaim` flushes leaked state files but has no `ip_forward`
  to restore (it was never touched).

### Build and install

#### From prebuilt binary

The release page includes `bore-aarch64-linux-android` for most Android devices (ARM64) — see
"Install (Termux)" above for the download command.

For emulator (x86_64), the prebuilt binary is not yet in releases; build from source (see below).

#### From source

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

### Validation status

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

### Troubleshooting

#### Not running as root

```
error: not running as root (uid=<uid>). bore vpn requires root or CAP_NET_ADMIN
```

**Fix:** Run with `su -c`/`tsu` or ensure the shell is root.

#### TUN device not found

```
error: bore-android-tun: failed to open /dev/tun: permission denied
```

**Fix:** Confirm you are running as root, and the device has `/dev/tun` (should be present on any stock
Android with Linux kernel TUN support).

#### Stale `ip rule` entries accumulate across links

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

#### Direct path not upgrading

If the link stays on relay and never tries to upgrade to direct:

- Check `--relay-only` (forces relay only).
- On a UDP-hostile network, direct upgrade may never succeed but the link stays stable on relay
  (this is correct behavior, not a failure — see `direct_upgrade_task`'s 30s retry grid in
  CLAUDE.md, which applies unchanged on Android).

### Testing

#### Unit tests (cross-platform, on Linux CI)

```bash
cargo test --features vpn --lib vpn::
```

All Android host-config logic is pure functions or gated by a `target_is_android()` parameter,
so unit tests run on Linux CI without an actual Android device.

#### Emulator e2e (`android-vpn-e2e` CI job)

Runs on every push to CI: rooted x86_64 emulator, API 30, covers TUN creation, relay/direct
paths, CLI guards, and SIGKILL reclaim.

#### Manual device acceptance (Phase 5.2)

See `VPN_ANDROID_ACCEPTANCE.md` (not yet written; will cover real 2+ device scenarios,
carrier failover, and network switching).

---

## See also

- [CLAUDE.md — VPN Android port](../CLAUDE.md) — terse invariant list (D-A4/D-A6/D-A9) and status
- [plan_AndroidSupport](plans/plan_AndroidSupport/) — full project plan + status + resume
- [SPIKE_FINDINGS.md](plans/plan_AndroidSupport/SPIKE_FINDINGS.md) — CI findings (netd quirk, ip rule duplicate handling)
- [Limits and unsupported features](vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md)
