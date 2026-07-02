# Phase 4 — VPN runtime validation (spike + host-only link e2e, rooted emulator)

> Precondition: phase 3 done (android vpn compiles; guards in place).
> Postcondition: TUN creation, apply/revert, stale reclaim, and a live
> host-only relay link PROVEN on a rooted Android emulator in CI. tun-rs
> android assumptions from 3.2 verified or corrected.

Context for the implementer (do not re-explore):

- Mirror `examples/macos_vpn_spike.rs` (structure: spike / create-teardown /
  apply-revert / leak-then-reclaim subcommands, driven by the e2e script) and
  the `macos-vpn-e2e` CI job shape.
- `adb root` then `adb shell` = uid 0 on the `target: default` image. After
  `adb root` the adb daemon restarts — script must `adb wait-for-device`.
- Rooted-emulator unknowns this phase resolves (risk register): tun-rs android
  device node (`/dev/net/tun` vs `/dev/tun`), builder-vs-fd API behavior,
  toybox `ip` acceptance of each argv from 3.2.
- Fallback recipe if tun-rs builder fails on android: open the device node
  `O_RDWR`, `ioctl(TUNSETIFF, IFF_TUN|IFF_NO_PI)` (tun-rs exposes an fd-based
  constructor — search its docs for `from_fd`/`FromRawFd` support in 2.8.5),
  then configure addr/mtu via `ip` argv through `CommandRunner`. If the
  fallback is needed, 4.1 implements it inside the android `create_tun` twin
  and updates the `// verified by` comments.
- Host side of the link: GitHub ubuntu runners allow `sudo` and have
  `/dev/net/tun` (the netns e2e job already relies on root netns). The Linux
  listener runs on the runner under sudo; `bore server` runs on the runner
  unprivileged; the emulator connector reaches both at `10.0.2.2`.

---

### 4.1 — `examples/android_vpn_spike.rs`

**Model:** Sonnet
**Files:** new `examples/android_vpn_spike.rs` (same dir as the macOS/windows
spikes — no new directory); possibly `src/vpn.rs` android twin corrections if
the spike falsifies a 3.2 assumption
**Change:** Clone the macOS spike's subcommand structure, android flavor:
1. `spike` — create TUN via the android `create_tun` twin, print the actual
   device name, read/write one packet against it (self-ping via the kernel:
   assign 10.199.0.1/30, `ping -c 1 10.199.0.2` in a spawned shell while
   reading the TUN fd; a received ICMP echo on the fd = pass), teardown.
2. `create-teardown` — create, assert `ip link` shows the device, drop, assert
   gone.
3. `apply-revert` — run `NetConfig::apply` with two fake peer routes, assert
   `ip route` shows them, drop NetConfig, assert routes gone.
4. `leak-then-reclaim` — write a state file as a crashed link would, run
   `stale_reclaim`, assert file removed.
Exit non-zero on any failed assertion; plain stdout `PASS <step>` lines.
**Unit tests:** none (example binary).
**e2e tests:** executed by 4.2.
**Done-criteria:** example compiles for both android targets under
`cargo ndk ... --features vpn --example android_vpn_spike`; Linux build
unaffected (example is `#[cfg(target_os="android")]`-guarded in main with a
stub main for other OS — copy how the macOS spike handles non-native
compilation, it has the same problem on Linux).

---

### 4.2 — CI job `android-vpn-e2e` + `scripts/android_vpn_test.sh`

**Model:** Sonnet — **Opus review gate on acceptance assertions**
**Files:** new `scripts/android_vpn_test.sh`; `.github/workflows/ci.yml` (new
job cloned from `android-emu-e2e`, phase 2)
**Change:**
1. Script (host-side, same env contract as `android_emu_test.sh` plus
   `$BORE_SPIKE_BIN` = android build of the spike example):
   - `adb root && adb wait-for-device`.
   - Push spike + bore binaries; chmod.
   - **T-AND-S1:** run spike `spike` in guest as root → PASS lines, exit 0.
   - **T-AND-S2:** spike `create-teardown` then `apply-revert` → exit 0.
   - **T-AND-S3:** spike `leak-then-reclaim` → exit 0.
   - **T-AND-L1 (relay link):** host: start `bore server` (unprivileged) and
     `sudo -n bore vpn listen --relay-only ...` (host-only listener, overlay
     e.g. 10.99.0.1; consult `scripts/vpn_netns_test.sh` for the exact
     current listen/connect flag set). Guest (root):
     `/data/local/tmp/bore vpn connect 10.0.2.2 --relay-only --accept-all-routes ...`.
     Assert: guest `ping -c 3 <listener overlay IP>` all replies AND host
     `ping -c 3 <connector overlay IP>` all replies (bidirectional).
   - **T-AND-L2 (direct best-effort, informational):** repeat L1 WITHOUT
     `--relay-only`; assert the link works regardless of path; grep logs for
     `direct` vs `relay` and echo the result. NEVER fail the job on
     relay-fallback (emulator NAT).
   - **T-AND-L3 (non-root negative):** `adb unroot`, wait, run
     `bore vpn connect ...` as shell uid → non-zero exit, stderr contains the
     3.1 root-hint message (tsu/Magisk + limits doc path). Re-`adb root` after.
   - **T-AND-L4 (guard e2e):** as root, `bore vpn connect ... --advertise 192.168.1.0/24`
     → non-zero, stderr contains "host-only".
   - **T-AND-L5 (guard e2e):** as root, `... --tun-queues 2` → non-zero,
     stderr contains "multi-queue".
   - Cleanup trap: pkill guest bore, kill host listener/server, `sudo -n`
     teardown of host TUN if leaked.
2. CI job: clone `android-emu-e2e`, additionally build the spike example and
   the host binary `--features vpn`; step order: KVM, builds, emulator-runner
   with `script: scripts/android_vpn_test.sh`. Host listener sudo: hosted
   runners have passwordless sudo — plain `sudo` (the `-n` exact-path rule is
   a dev-box constraint, harmless in CI).
**Unit tests:** none.
**e2e tests:** the script IS T-AND-S1..S3 + T-AND-L1..L5.
**Done-criteria:** job green 2 consecutive runs; Opus reviews the assertion
set (bidirectional ping, guard messages, negative tests) before the job is
marked required.

---

### 4.3 — Findings write-back + twin corrections

**Model:** Sonnet — **Opus review gate if any 3.2 twin body changes**
**Files:** `src/vpn.rs` (android twins only), `docs/plans/plan_AndroidSupport/`
(findings file `SPIKE_FINDINGS.md`, mirrors `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md`
placement style but lives in this plan folder), `CLAUDE.md` (spike-pending →
validated)
**Change:**
1. Record: actual device node used, tun-rs android API path taken (builder or
   fd fallback), toybox `ip` spellings accepted, emulator direct-path outcome
   (L2), any deviation from 3.2 assumptions — one findings doc, terse.
2. Apply any twin corrections the spike forced; remove the
   `// verified by android_vpn_spike` provisional comments (now proven).
3. Rerun phase 3 regression sweep (Linux gates + netns suite) if `src/`
   changed at all.
**Unit tests:** update the 3.2 argv unit tests if spellings changed.
**e2e tests:** rerun android-vpn-e2e green after any correction.
**Done-criteria:** findings doc written; twins final; all gates + both android
CI jobs green; zero Linux diff outside android twins.

---

## Phase gates

- Everything from phase 3 gates, PLUS `android-emu-e2e` and `android-vpn-e2e`
  green.
- `vpn_netns_test.sh` full pass if any `src/` file changed in 4.3.

**Phase done when:** T-AND-S1..S3, T-AND-L1..L5 green in CI + Opus assertion
review approved. Update `resume.md`.
