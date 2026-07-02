# plan_AndroidSupport — Progress tracker

> Machine-readable. Implementer: update after EVERY sub-phase. Keep terse.

**Next:** phase_02.md sub-phase 2.1

> **Model note:** per explicit user instruction, every "Opus" role in the
> original plan (architect / review-gate / final-read) is executed by
> **Sonnet 5** instead. Table below reflects that substitution.

## Phase status

| Phase | Sub-phase | Model | Status | Notes |
|-------|-----------|-------|--------|-------|
| 1 | 1.1 CI x86_64-android target | Haiku | DONE | commit 7e3be69 |
| 1 | 1.2 check → clippy -D warnings | Haiku | DONE | commit b954935 |
| 1 | 1.3 Justfile android-x86_64 + API pin | Haiku | DONE | commit 040f920; API 24 already consistent |
| 2 | 2.1 scripts/android_emu_test.sh | Sonnet | TODO | |
| 2 | 2.2 CI job android-emu-e2e | Sonnet | TODO | |
| 2 | 2.3 portability fixes (contingency) | Sonnet | TODO | |
| 3 | 3.1 gate flips + shared joins | Sonnet + Sonnet 5 review gate | TODO | |
| 3 | 3.2 android twins (tun/run_dir/apply/reclaim) | Sonnet | TODO | |
| 3 | 3.3 host-only CLI guard matrix | Sonnet + Sonnet 5 review gate | TODO | |
| 3 | 3.4 regression sweep + CLAUDE.md | Haiku | TODO | |
| 4 | 4.1 examples/android_vpn_spike.rs | Sonnet | TODO | |
| 4 | 4.2 android-vpn-e2e job + script | Sonnet + Sonnet 5 review gate | TODO | |
| 4 | 4.3 findings write-back | Sonnet (+ Sonnet 5 review if twins change) | TODO | |
| 5 | 5.1 docs/ANDROID.md + limits refresh | Haiku | TODO | |
| 5 | 5.2 VPN_ANDROID_ACCEPTANCE.md | Haiku | TODO | |
| 5 | 5.3 release verify + final read | Sonnet 5 | TODO | |

## Test status

| ID | What | Phase | Status |
|----|------|-------|--------|
| T-AND-B1, B2 | build matrix (targets, clippy) | 1 | PENDING CI (pushed, awaiting run) |
| T-AND-B3 | `just android-x86_64` local build | 1 | BLOCKED — dev box has no NDK/cargo-ndk installed; covered indirectly by CI's cargo-ndk legs (B1/B2) instead |
| T-AND-E1..E6 | non-VPN emulator e2e | 2 | TODO |
| T-AND-E-CI | android-emu-e2e job green | 2 | TODO |
| unit: android_apply_builds_expected_argv | apply argv + revert LIFO | 3 | TODO |
| unit: android_apply_rejects_gateway_inputs | defense in depth | 3 | TODO |
| unit: android_stale_reclaim_removes_leaked_state | reclaim | 3 | TODO |
| unit: android_guard_matrix | CLI guard table | 3 | TODO |
| regression: vpn_netns_test.sh full | Linux zero-regression proof | 3,4,5 | TODO |
| T-AND-S1..S3 | spike (tun, apply/revert, reclaim) | 4 | TODO |
| T-AND-L1 | relay link bidirectional ping | 4 | TODO |
| T-AND-L2 | direct best-effort (informational) | 4 | TODO |
| T-AND-L3..L5 | negatives (non-root, advertise, queues) | 4 | TODO |
| T-AND-M1..M5 | manual acceptance, real device | 5 | TODO (hardware) |

## Docs status

| Doc | Phase | Status |
|-----|-------|--------|
| CLAUDE.md Android block | 3.4, 5.3 | TODO |
| SPIKE_FINDINGS.md (plan folder) | 4.3 | TODO |
| docs/ANDROID.md | 5.1 | TODO |
| limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md rewrite | 5.1 | TODO |
| docs/vpn/VPN_ANDROID_ACCEPTANCE.md | 5.2 | TODO |
| INSTALL_BORE.md / DOWNLOAD_URLS.md android rows | 5.1 | TODO |
