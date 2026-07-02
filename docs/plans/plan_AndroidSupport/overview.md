# Android Support (Termux, root + non-root) — Plan Overview

> **Status:** planning | **Opus authored:** 2026-07-02
> **Folder:** `docs/plans/plan_AndroidSupport/`
> **Branch:** `android`

## Goal

Complete bore for Android as a Termux-launched CLI binary: every subcommand
(`local`, `proxy`, `vhost`, `transfer`, `test-udp`, `server`) works non-root;
`bore vpn` works root-only in host-only mode (no gateway). Anything Android
makes impossible is documented explicitly, not silently broken. Zero
regressions on Linux/macOS/Windows: every existing gate and netns suite stays
green throughout.

```
Reference scenario (acceptance):
1. Termux, NON-root, real aarch64 device:
   bore local 8080 --to <server> --udp        -> public URL serves local :8080
   bore transfer sender/listener over tunnel   -> BLAKE3-verified file arrives
2. Termux, ROOT (tsu), same device:
   bore vpn connect <server> --secret-id X --accept-all-routes
   -> link up (relay; direct best-effort), ping <listener overlay IP> replies
3. Non-root `bore vpn connect` -> clear error explaining root requirement.
4. All Linux/macOS/Windows CI jobs + `vpn_netns_test.sh` unchanged and green.
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D-A1** | VPN non-root on Android = impossible for a CLI binary (TUN needs root; `VpnService` API is app-only). Documented, not worked around. No APK in scope. | `check_root` on Android errors with a message naming `tsu`/Magisk and pointing at the limits doc |
| **D-A2** | Targets: `aarch64-linux-android` (devices) + `x86_64-linux-android` (emulator CI). API level 24 (Termux minimum), NDK via `cargo-ndk` (pattern already in `ci.yml` job `vpn-cross-build`) | CI builds+clippy both targets; release ships aarch64 only (unchanged) |
| **D-A3** | Test oracle = GitHub Actions Android emulator (AOSP `default` x86_64 image, `adb root` capable) + manual Termux acceptance on a real device. Dev box cannot run Android. | Two new CI jobs (non-VPN e2e, VPN e2e); acceptance doc mirrors `VPN_MACOS_ACCEPTANCE.md` |
| **D-A4** | Android VPN v1 = **host-only** (`listen`+`connect`, empty advertise). `--advertise`, `--nat-masquerade`, `--forward-accept`, `--max-clients >1`, `--tun-queues >1` are hard CLI errors on Android. Gateway/hub is *deferred* (possible with root+iptables), not impossible. | Small `NetConfig::apply` android twin: `ip addr/link/route` only — no nft/iptables/ip_forward/MSS-clamp/fwdref-refcount machinery |
| **D-A5** | cfg strategy: join the Linux branch via `any(target_os="linux", target_os="android")` ONLY where the body is byte-identical and valid on Android (`check_root` list, `check_binary_exists`, offload-`unreachable!` twin lists). Everything else gets an `#[cfg(target_os="android")]` twin (create_tun, run_dir, apply, stale_reclaim). Linux bodies stay **byte-for-byte** (same contract as macOS DEC-M1). | `git diff` must show no semantic edit inside any `cfg(linux)` body; netns suite is the proof |
| **D-A6** | Android is NOT `target_os="linux"` in Rust: nothing currently `cfg(linux)` compiles for it. Today's android CI check passes only because the whole `vpn` module is cfg'd out even with `--features vpn`. | Phase 3 flips the gates; until then `--features vpn` on android = feature-on, code-absent (status quo) |
| **D-A7** | UDP socket buffers on Android keep the existing `cfg(all(udp, unix, not(linux)))` socket2 branch (`holepunch.rs:254`) — already compiling today. No `SO_*BUFFORCE` port in v1; rooted users get a documented `sysctl -w net.core.rmem_max` remediation | Zero code change; ANDROID guide documents the clamp |
| **D-A8** | `run_dir()` android twin returns `"/data/local/tmp"` (static str, root-only VPN so perms fine). NOT boot-cleared like `/run` — `stale_reclaim` already handles stale files by design; android reclaim is state-file-delete only (no ip_forward to restore) | Keeps `&'static str` signature; no fwdref/netns-inode logic on Android |
| **D-A9** | nft never used on Android (kernels lack nft modules; netd is iptables/eBPF). Host-only needs no firewall commands anyway | No nft probing on android path |
| **D-A10** | Emulator e2e drive the compiled binary via `adb shell` scripts using `/data/local/tmp`; `cargo test` binaries are NOT run on-device (test code hardcodes `/tmp`). Unit tests run on the Linux host as always | e2e scripts follow `scripts/` conventions; host runner is the network peer at `10.0.2.2` |

## Architecture summary

Android is a fourth `cfg` platform alongside linux/macos/windows, reusing the
Linux kernel semantics wherever bodies are identical and adding minimal twins
where Termux/Android userland differs. Data plane (yamux/QUIC/AEAD/carriers)
is already OS-neutral and compiles today. The only new runtime code is the VPN
host-config subset; everything else is build matrix, e2e harness, and docs.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|-----------------|
| 1 — Build matrix completion | [phase_01.md](phase_01.md) | Haiku | yes |
| 2 — Non-VPN emulator e2e | [phase_02.md](phase_02.md) | Sonnet | yes |
| 3 — VPN compile port (cfg + twins + guards) | [phase_03.md](phase_03.md) | Sonnet + Opus gate | yes |
| 4 — VPN runtime validation (spike + link e2e) | [phase_04.md](phase_04.md) | Sonnet + Opus gate | yes |
| 5 — Docs, packaging, manual acceptance | [phase_05.md](phase_05.md) | Haiku + Opus final read | yes |

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| NDK build pattern | `vpn-cross-build` job (cargo-ndk, `CC_aarch64_linux_android`, `ANDROID_API`) | `.github/workflows/ci.yml:~60-110` |
| Release android build | `android-arm64` recipe, `android_api` var | `Justfile:82`, `Justfile:19`; `docker/Dockerfile.android` |
| create_tun twin shape | macOS twin (single-queue, no offload, name read-back) | `src/vpn.rs:886` |
| Offload `unreachable!` twins | macOS/windows pump twins | `src/vpn.rs` (search `run_uplink_offload`) |
| check_root | `nix::unistd::getuid().is_root()` | `src/vpn.rs:4521` |
| run_dir per-OS | linux/macos/windows twins | `src/vpn.rs:5112-5120` |
| NetConfig::apply twin shape | macOS apply (routes + revert stack) | `src/vpn.rs:5419` |
| stale_reclaim twin shape | macOS reclaim | `src/vpn.rs:4661` |
| Spike example shape | `examples/macos_vpn_spike.rs`, `examples/windows_vpn_spike.rs` | `examples/` |
| e2e script conventions | `scripts/secret_netns_test.sh`, `scripts/vpn_netns_test.sh` | `scripts/` |
| Acceptance doc shape | `docs/vpn/VPN_MACOS_ACCEPTANCE.md` | `docs/vpn/` |
| Limits doc to update | `VPN_ANDROID_ACTUAL_LIMIT.md` | `docs/vpn/limits_win_mac/` |
| Vpn subcommand gate | `any(linux, macos, windows)` lists | `src/main.rs:410-412`, `src/lib.rs` (search `pub mod vpn`), `Cargo.toml:79` |

## Invariants

- **I-A1:** Linux/macOS/Windows behavior byte-identical — no semantic edit
  inside any existing `cfg` body; all existing CI jobs and netns suites green.
- **I-A2:** Non-VPN android binary (today's shipped artifact) keeps building
  from phase 1 onward — no phase may break `cargo ndk ... build` default features.
- **I-A3:** Android VPN guards fail fast at CLI/startup with explicit errors —
  never a silent clamp or silent ignore (house rule).
- **I-A4:** All CLAUDE.md "Key invariants" (yamux single-task streams, AEAD
  nonce sharing, STREAM_READY-before-splice, half-close, tune_tcp) untouched —
  the port adds no new stream/task topology.

## Risk register

| Risk | Mitigation |
|------|-----------|
| `tun-rs` 2.8.5 android backend shape unknown (builder vs fd-based; `/dev/net/tun` vs `/dev/tun`) | Phase 4 spike runs FIRST in that phase, on rooted emulator CI; phase_04 carries a fallback recipe (manual open + `from_fd`) |
| Toybox `ip` subset may lack a flag the android apply twin uses | Twin restricted to `ip addr add / link set up mtu / route add|del` (toybox-supported); spike asserts each command |
| Emulator NAT breaks UDP direct path determinism | Direct-path e2e is best-effort/informational (T-AND-L2); deterministic gates use `--relay-only` |
| `adb root` unavailable on chosen image | Pin `target: default` (AOSP userdebug) emulator image, not google_play |
| Phantom process killer kills Termux background tunnels on Android 12+ | Documentation (wake-lock + device_config), phase 5; manual acceptance verifies |
| nix crate gates a joined symbol linux-only (compile break on android) | D-A7 avoids nix entirely on android paths; joins limited to `getuid` (nix `unistd` is unix-wide) |

## Model-assignment summary

| Phase | Sub-phases | Haiku | Sonnet | Opus |
|-------|-----------|-------|--------|------|
| 1 | 3 | 1.1, 1.2, 1.3 | — | — |
| 2 | 3 | — | 2.1, 2.2, 2.3 | — |
| 3 | 4 | 3.4 | 3.1, 3.2, 3.3 | review gate on 3.1/3.3 |
| 4 | 3 | — | 4.1, 4.2, 4.3 | review gate on 4.2/4.3 |
| 5 | 3 | 5.1, 5.2 | — | 5.3 final read |
