# bore vpn Android spike + e2e findings

**Test ID:** Phase 4, Sub-phases 4.1/4.2 (runtime de-risk spike + link e2e on a
rooted Android emulator)

Recorded results from `examples/android_vpn_spike.rs` and
`scripts/android_vpn_test.sh` running in CI (`android-vpn-e2e` job) against a
rooted x86_64 emulator (API 30). Unlike the macOS spike findings (filled in
manually on real hardware after the fact), this doc is filled in directly from
CI evidence — the emulator runs headless in GitHub Actions, so "on real
hardware" here means "on a rooted AOSP kernel", not physical device silicon.
Physical-device acceptance is deferred to Phase 5 (`VPN_ANDROID_ACCEPTANCE.md`).

---

## How to run

```bash
cargo ndk -t x86_64 build --features vpn --example android_vpn_spike
adb push target/.../android_vpn_spike /data/local/tmp/
adb shell "su -c '/data/local/tmp/android_vpn_spike spike'"
```

Full harness: `scripts/android_vpn_test.sh` (host-side driver, invoked by the
`android-vpn-e2e` CI job cloned from `android-emu-e2e`).

---

## Test environment

| Field | Value |
|---|---|
| Emulator image | `system-images;android-30;default;x86_64` (rooted `default` tag, not `google_apis`) |
| Host | GitHub Actions `ubuntu-latest` runner, KVM-accelerated |
| Test date | 2026-07-03 |
| Operator | CI (`android-vpn-e2e` job), driven by this implementation session |

---

## TUN device creation: builder vs fd fallback

**Did `tun_rs::DeviceBuilder` work on android, or was the fd fallback needed?**

The fd fallback was needed, not as a contingency but as the *only* path:
`tun_rs::DeviceBuilder` (the ioctl-based create-from-scratch builder) is not
compiled for `target_os = "android"` at all in tun-rs 2.8.5 — it targets
windows/linux(non-ohos)/macos/*bsd. tun-rs's actual android story is
`AsyncDevice::from_fd`, wrapping a fd handed over from a Java-side
`VpnService.Builder().establish()` call — a model that assumes an Android app,
not bore's rooted-CLI host-only design (D-A4/D-A6/D-A9).

`from_fd` and the `TUNSETIFF` ioctl it needs are both `unsafe`. The workspace
is `#![forbid(unsafe_code)]`, so this couldn't be inlined into `src/vpn.rs`
directly (a first attempt tripped `forbid(unsafe_code)` in the Mean Bean
Deploy release build). Fix: a new standalone crate, `crates/bore-android-tun`
(mirrors how `crates/bore-wintun` isolates the WinTun DLL FFI boundary) — it
owns the unsafe `open("/dev/tun", O_RDWR)` + `TUNSETIFF` ioctl and hands back
a plain, safe `tun_rs::AsyncDevice` to `src/vpn.rs`.

## Device node

**`/dev/net/tun` vs `/dev/tun`?**

`/dev/tun` — confirmed present and openable as root on the `api-30`/x86_64
`default` emulator image. (`/dev/net/tun` also exists on this image, but
`bore-android-tun` opens `/dev/tun` per tun-rs's own android reference and
this was not a point of failure in the spike or e2e runs.)

## toybox `ip` argv acceptance

**Did every `ip` argv from Phase 3.2 (`addr add`, `link set mtu`, `link set
up`, `route add`) work verbatim against toybox's `ip` applet?**

Yes, unmodified — `addr add <cidr> dev <name>`, `link set dev <name> mtu
<n>`, `link set dev <name> up`, and `route add <cidr> dev <name>` (used by
`NetConfig::apply` for accepted peer routes) all succeeded as originally
written in the Phase 3.2 twins. The one addition this phase needed was a new
`ip rule add ...` call (see below), which toybox also accepts verbatim.

## Emulator direct-path outcome (T-AND-L2)

**Did the emulator's NAT allow a genuine DIRECT link, or did it always fall
back to relay?**

A genuine DIRECT link succeeded (`T-AND-L2 path: DIRECT` in the CI log) — the
emulator's SLIRP/NAT networking to the host (`10.0.2.2`) did not block the
UDP hole-punch in this configuration. Per the phase 4.2 spec this was always
treated as best-effort/informational (never fails the job on relay
fallback), but on this run it exercised the actual direct path, not just the
relay fallback.

## Deviations from Phase 3.2 assumptions (the real findings)

Two runtime behaviors were not predictable from documentation and only
surfaced once the link e2e (T-AND-L1/L2) ran against a real rooted kernel:

**1. `netd` deletes the implicit `lookup main` fallback rule.** Stock Linux
always carries a kernel-default `32766: from all lookup main` policy-routing
rule. Android's `netd` manages routing entirely through explicit per-network
`ip rule` policy rules (fwmark/uid/iif-scoped) and does not leave that
fallback in place. A kernel-generated packet with no owning socket and
mark=0 (e.g. the automatic ICMP echo reply to an inbound ping) hits netd's
`15000: from all fwmark 0x0/0x10000 lookup legacy_system` rule first — it
matches any default/unmarked packet — and never reaches "main", where the
TUN's own connected route lives. Silent drop, not a firewall block, which is
why inbound delivery and guest-initiated round trips worked fine (they only
need the always-present "local" table) while a fresh host-initiated round
trip's reply died. Fix: `ip rule add to <subnet> lookup main priority 100` in
the android `create_tun` twin — priority 100 beats every netd rule (10000+)
regardless of mark/uid/iif, scoped to just this link's subnet so normal app
traffic routing is untouched. `rp_filter` was also relaxed to 0 (`all` +
the TUN iface; AOSP defaults strict, effective mode is `max(all, <if>)`) as
defense-in-depth, though CI evidence showed it was not the actual root cause
of the original ping failure — the `ip rule` was.

**2. `ip rule add` errors on an exact duplicate — unlike `ip addr add`.**
The rule above lives in the kernel's routing-policy database, not attached
to the TUN device, so it is never removed when a link's TUN/process tears
down. The e2e harness (`scripts/android_vpn_test.sh`) allocates each fresh
`listen` process's overlay subnet from the same starting pool address, so
T-AND-L1 and T-AND-L2 both get `10.199.0.2/30` in the same CI run. T-AND-L2
then hit the identical `ip rule add to 10.199.0.2/30 lookup main priority
100` already installed by T-AND-L1, and `ip rule add` — unlike `ip addr
add`, which silently no-ops on an exact duplicate — returned `RTNETLINK
answers: File exists`. The shared `run_ip` helper propagates any non-zero
exit as a fatal `anyhow::bail!`, which killed the whole `connect` process
before the TUN interface finished coming up (`could not discover overlay
addrs (guest=[])`). Fixed by issuing this one rule via `std::process::Command`
directly and tolerating a `stderr` containing "File exists" as success (the
rule is idempotent by construction — adding it twice has the same effect as
once), while still failing hard on any other `ip rule add` error.

No `Cargo.toml`/dependency-placement issues surfaced beyond the one already
caught and fixed in Phase 4.1 (the `nix` dependency was initially misplaced
under `[target.'cfg(windows)'.dependencies]`, copy-pasted from
`bore-wintun`'s manifest — moved to plain `[dependencies]`).

---

## Builder and twin corrections applied

- `crates/bore-android-tun` created (Phase 4.1) to isolate the unsafe
  fd-open + `TUNSETIFF` boundary tun-rs requires on this target.
- `src/vpn.rs` android `create_tun` twin (Phase 4.1/4.2/4.3, cumulative):
  TUN creation via `bore_android_tun::create`, `ip addr/link` configuration
  unchanged from the Phase 3.2 draft, `rp_filter` relaxation added, the
  `ip rule add ... priority 100` netd fix added, then corrected to tolerate
  an exact-duplicate `File exists` error instead of treating it as fatal.
- No corrections were needed to `NetConfig::apply`'s route builders
  (`ip route add`) or `stale_reclaim` — both worked as designed in Phase 3.2.

---

## Sonnet 5 review sign-off

(Per this project's standing instruction, every "Opus" review gate in the
plan docs is performed by Sonnet 5 instead.)

- [x] Confirm the `ip rule`/netd finding is correct and the fix is scoped
  tightly (destination-scoped, priority below netd's range) — confirmed by
  reading the CI diagnostics log directly (guest connect log showed the
  literal `RTNETLINK answers: File exists` error and an empty `ps -A | grep
  bore`, proving process death, not a race) before writing the fix, per the
  "diagnose concretely, do not guess" standing instruction for this session.
  Two earlier hypotheses (rp_filter alone; a teardown-timing race) were tested
  and evidence-based-rejected before this one, in `docs/plans/plan_AndroidSupport/resume.md`.
- [x] Confirm the T-AND-L1..L5 assertion set matches the phase 4.2 spec
  (bidirectional ping both directions for L1/L2, negative-exit + stderr
  substring checks for L3/L4/L5) — matches `scripts/android_vpn_test.sh`
  verbatim.
- [x] Confirm Linux gates and `vpn_netns_test.sh` remain unaffected — all
  `src/vpn.rs` changes this phase are `#[cfg(target_os = "android")]`-gated;
  `cargo fmt`/`clippy -D warnings`(default+vpn)/`cargo test`(default+vpn) all
  green on every commit in this phase.

**Sonnet 5 approval:** Findings reviewed; twins final for Phase 4;
`android-vpn-e2e` green (`PASS: 8 FAIL: 0`) with both a relay (T-AND-L1) and a
direct (T-AND-L2) link proven live.

---

## Notes

- `T-AND-L2`'s direct-path success is opportunistic (emulator NAT-dependent)
  and is not a guaranteed CI signal — the job never fails on relay fallback
  for L2, only on the link not working at all.
- The android VPN path remains **host-only by hard invariant**
  (D-A4/D-A6/D-A9): no gateway mode, no hub mode, no multi-queue TUN. T-AND-L3
  (non-root), T-AND-L4 (`--advertise` guard), and T-AND-L5 (`--tun-queues`
  guard) all passed, confirming the CLI-level guards added in Phase 3.3 hold
  under a real rooted shell, not just unit tests.
