# Phase 5 — Documentation, packaging, manual acceptance

> Precondition: phases 1-4 done (android proven in CI).
> Postcondition: complete user-facing Android documentation (root + non-root,
> Termux), limits stated explicitly, release artifacts confirmed, manual
> acceptance procedure ready for the real device.

Context for the implementer (do not re-explore):

- Doc conventions: top-level guides live in `docs/` (`INSTALL_BORE.md`,
  `DOWNLOAD_URLS.md`); VPN platform docs in `docs/vpn/` (`VPN_MACOS.md`,
  `VPN_MACOS_ACCEPTANCE.md`); hard-limit docs in `docs/vpn/limits_win_mac/`.
- Release already ships aarch64 android (Justfile `android-arm64`,
  `docker/Dockerfile.android`, `mean_bean_deploy.yml` android entries) — this
  phase VERIFIES and documents, it does not rebuild the pipeline.
- Android facts to document (established during planning — do not re-research):
  Termux min API 24; `pkg install` for deps; root via Magisk/KernelSU + `tsu`;
  no `/tmp` (Termux `$TMPDIR`); ports <1024 root-only; `VpnService` is
  app-only hence CLI non-root VPN impossible; Android 12+ phantom process
  killer kills long-running Termux children — remediations:
  `termux-wake-lock` and
  `adb shell device_config put activity_manager max_phantom_processes 2147483647`
  (or `settings put global settings_enable_monitor_phantom_procs false`);
  UDP socket buffers clamped by `net.core.{r,w}mem_max` — rooted remediation
  `sysctl -w net.core.rmem_max=16777216` (D-A7); nft absent (D-A9); toybox
  `ip` subset only.

---

### 5.1 — `docs/ANDROID.md` user guide + limits refresh

**Model:** Haiku
**Files:** new `docs/ANDROID.md`; update
`docs/vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md`; touch
`docs/INSTALL_BORE.md` + `docs/DOWNLOAD_URLS.md` (add android rows/links only)
**Change:**
1. `docs/ANDROID.md`, sections in this order:
   - Install (Termux): download aarch64 binary (link pattern from
     DOWNLOAD_URLS.md), `chmod +x`, optional `pkg install` deps (none required
     for non-VPN; `tsu` + root for VPN).
   - Feature matrix table: rows = local / proxy / vhost / transfer / test-udp
     / server / vpn; columns = non-root / root; each cell works|limited|no
     with one-line reason. VPN non-root cell = "impossible (VpnService is
     app-only)" linking the limits doc.
   - Non-root notes: port <1024, UDP buffer clamp warn, phantom process
     killer + both remediations, keep-alive via `termux-wake-lock`.
   - Root VPN quickstart: `tsu`, `bore vpn connect` host-only example, guard
     matrix table (the 5 rejected flags from phase 3.3 with their exact error
     strings).
   - Emulator/dev notes: pointer to the two CI jobs and scripts.
   Plain technical prose, no emojis (professionalism standard).
2. Rewrite `VPN_ANDROID_ACTUAL_LIMIT.md`: status NOT IMPLEMENTED → IMPLEMENTED
   (host-only); keep the "do not test" framing but invert it: list what IS
   testable now (host-only relay/direct connect+listen) and what remains a
   hard limit (non-root, gateway/NAT/MSS-clamp, hub, multi-queue, offload).
   Note gateway = deferred-not-impossible (D-A4).
**Unit tests:** n/a.
**e2e tests:** n/a.
**Done-criteria:** docs build no broken relative links
(`grep -o '](\S*\.md' docs/ANDROID.md` targets all exist); feature matrix
consistent with the phase 3.3 guard matrix and phase 2/4 test results.

---

### 5.2 — `docs/vpn/VPN_ANDROID_ACCEPTANCE.md` (manual, real device)

**Model:** Haiku
**Files:** new `docs/vpn/VPN_ANDROID_ACCEPTANCE.md` (mirror
`VPN_MACOS_ACCEPTANCE.md` structure)
**Change:** Step-by-step operator procedure, IDs **T-AND-M1..M5**:
- **T-AND-M1** (non-root, Termux): public tunnel `bore local 8080 --to <server>`
  → phone-hosted page reachable from a second machine. Include the exact
  commands and expected output lines.
- **T-AND-M2** (non-root): `bore transfer` phone→PC and PC→phone over the
  tunnel, BLAKE3 verified (exit 0), resume test (kill mid-transfer, rerun,
  completes).
- **T-AND-M3** (root, tsu): `bore vpn connect` host-only to a Linux listener;
  bidirectional overlay ping; then kill with SIGINT and assert routes/TUN
  reverted (`ip route`, `ip link`).
- **T-AND-M4** (root): SIGKILL the connector; relaunch; assert `stale_reclaim`
  log line and clean state dir.
- **T-AND-M5** (longevity): tunnel with `termux-wake-lock`, screen off 30 min,
  assert still connected (phantom-killer remediation proof); document the
  observed Android version + whether the device_config step was needed.
Each step: preconditions, commands, expected observation, PASS checkbox.
**Unit tests / e2e tests:** the doc IS the manual test suite.
**Done-criteria:** procedure executable top-to-bottom by an operator with no
repo context; flag names verified against `--help` output of the built binary.

---

### 5.3 — Release verification + final read (Opus)

**Model:** Opus
**Files:** none (verification) + `CLAUDE.md` final Android block +
`docs/plans/plan_AndroidSupport/resume.md`
**Change:**
1. Verify release pipeline: `Justfile android-arm64` builds with
   `--features vpn`? Decide and enforce: released android binary SHIPS vpn
   feature (consistent with linux/macos releases — check what
   `Dockerfile.android` passes today; `--all-features` already implies vpn
   post-phase-3, so verify the NDK image build still succeeds with the vpn
   module now compiled in: run the Dockerfile build locally or in CI once).
2. Cross-check every claim in ANDROID.md and the acceptance doc against the
   implemented behavior (guard strings, run_dir path, CI job names).
3. Finalize the CLAUDE.md Android block (scope, invariants D-A4/D-A5/D-A8,
   test oracle, pointers to plan + limits + acceptance docs).
4. Mark resume.md: phases complete; remaining = T-AND-M1..M5 on hardware.
**Unit tests / e2e:** full regression sweep one last time: Linux gates
(default + vpn), netns suite, all CI jobs including the two android jobs.
**Done-criteria:** everything green; docs consistent; plan closed except
manual hardware acceptance.

---

## Phase gates

- Full CI matrix green (all platforms, all android jobs).
- `vpn_netns_test.sh` green.
- Docs link-check clean.

**Phase done when:** 5.1-5.3 complete. Manual T-AND-M1..M5 tracked in
resume.md as the only open items (hardware-dependent).
