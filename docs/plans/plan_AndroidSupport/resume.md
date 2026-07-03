# plan_AndroidSupport — Progress tracker

> Machine-readable. Implementer: update after EVERY sub-phase. Keep terse.

**Next:** phase_05.md sub-phase 5.2 (VPN_ANDROID_ACCEPTANCE.md)

> **Model note:** per explicit user instruction, every "Opus" role in the
> original plan (architect / review-gate / final-read) is executed by
> **Sonnet 5** instead. Table below reflects that substitution.

## Phase status

| Phase | Sub-phase | Model | Status | Notes |
|-------|-----------|-------|--------|-------|
| 1 | 1.1 CI x86_64-android target | Haiku | DONE | commit 7e3be69 |
| 1 | 1.2 check → clippy -D warnings | Haiku | DONE | commit b954935 |
| 1 | 1.3 Justfile android-x86_64 + API pin | Haiku | DONE | commit 040f920; API 24 already consistent |
| 2 | 2.1 scripts/android_emu_test.sh | Sonnet | DONE | commit 762f383; shellcheck-clean via docker koalaman/shellcheck; deviated from literal spec on T-AND-E2 (transfer uses built-in --to/--transfer-id rendezvous, not wrapped in bore local — matches actual Sender/Listener CLI) |
| 2 | 2.2 CI job android-emu-e2e | Sonnet | DONE | commit de782e9 |
| 2 | 2.3 portability fixes (contingency) | Sonnet | DONE | round 1 commit cc05bda: cargo-ndk `-p`→`-P` flag fix (run 28615734964 failed, "unknown package: 24"). round 2 commit 2f2c949: build succeeded this time (E1/E4/E5 passed) but T-AND-E2 (transfer sender) and T-AND-E3 (proxy) failed — both run on the HOST but dialed `--to 10.0.2.2`, the emulator's guest-only NAT alias for the host; meaningless from the host's own netns → connect timeout (E2) / empty body (E3). Fixed: host-launched processes now dial 127.0.0.1. round 3 commit b1854aa: E1-E5 all passed; script then died with generic exit-1 before ever printing an E6 result — `E6_OUT="$(...)"; E6_STATUS=$?` is the classic `set -e` gotcha (bore server's expected nonzero exit aborted the script on the assignment line, before `$?` was ever read). Fixed via `|| E6_STATUS=$?` form. CONFIRMED GREEN 2x consecutive: run 28617339653 initial pass + `gh run rerun` pass — flake-check satisfied. Phase 2 DONE |
| 3 | 3.1 gate flips + shared joins | Sonnet + Sonnet 5 review gate | DONE | commit 0997c09; extended all vpn any(linux,macos,windows) gates to +android (Cargo.toml tun-rs, lib.rs/main.rs module+subcommand, both vpn test files, check_root, 2x `ip --version` probe cfg, 3x offload unreachable twins); check_root gained android hint message (body unchanged); reviewed — diff is cfg-list-only + 1 message branch, zero semantic change to existing platforms (I-A1); Linux fmt/clippy/test (default+vpn) green. Expected: does NOT compile for android yet (no twins) — that's phase 3.2 |
| 3 | 3.2 android twins (tun/run_dir/apply/reclaim) | Sonnet | DONE | commit 6c32726; NetConfig::apply (host-only route table, toybox `ip route add`, no ip_forward/nft/iptables/PF, check_host_only guard D-A4/D-A6/D-A9), stale_reclaim (no fwdref/ip_forward state to restore), restore_ip_forward_op (unreachable!, never pushed). Plus 3 self-found compile-blocking gaps not in plan text: TunDevice cfg(any) missing android, pmtu_link_set_mtu_argv missing android arm (reuses Linux builder — toybox ip supports same `link set mtu` grammar), restore_ip_forward_op missing android arm. Guard/argv logic pulled into un-gated hostcfg_cmd::android so Linux CI can unit-test without cross-compile. fmt/clippy(-D warnings, default+vpn)/test(default 490, vpn 326, both 0 failed) all green |
| 3 | 3.3 host-only CLI guard matrix | Sonnet + Sonnet 5 review gate | DONE | commit 607355f; validate_android_host_only (pure, target_is_android bool param) called at top of run_listen/run_connect before run_with_reconnect; rejects --advertise non-empty, --nat-masquerade, --forward-accept, --max-clients>1, --tun-queues>1 with exact D-A4 message text; UDP hole-punch flags NOT special-cased (unchanged). Sonnet-5 review gate: rejected set == D-A4/D-A6/D-A9 exactly, confirmed. fmt/clippy(-D warnings, default+vpn)/test(default+vpn) all green |
| 3 | 3.4 regression sweep + CLAUDE.md | Haiku | DONE | commit 1f9a52d; full `vpn_netns_test.sh` 161/0 (SIGKILL reclaim, hub, NAT, forward-accept, stress/flap suites all pass — zero regression from Phase 3 twins/guards); fmt/clippy(-D warnings, default+vpn)/test(default+vpn) all green; CLAUDE.md VPN Android port block added. Phase 3 DONE |
| 4 | 4.1 examples/android_vpn_spike.rs | Sonnet | DONE | commit 9ce7718; 4 modes (spike/create-teardown/apply-revert/leak-then-reclaim) mirroring macos/windows spike structure; every fn individually cfg(android)-gated, stub main for Linux; hardcodes deterministic "bore-vpn-ns0-" fwdref prefix (private helpers unreachable from example crate); fmt/clippy(-D warnings, default+vpn incl. this example)/test(default+vpn) all green on Linux. Actual android compile/run is CI/device-only, verified in 4.2 |
| 4 | 4.2 android-vpn-e2e job + script | Sonnet + Sonnet 5 review gate | DONE | commit cdac0c8 (script+job) then a bug chain to reach green, all CI-diagnosed not guessed: bdb7494 (rp_filter relax, insufficient alone) → fbe5edd (android netd policy routing eats implicit `lookup main` fallback rule, fixed with `ip rule add to <subnet> lookup main priority 100`; T-AND-L1 PASS after this) → 6301c3c (hypothesized T-AND-L2 teardown race, added `wait_for_guest_iface_gone` poll — kept as valid hardening but CI disproved the hypothesis, same failure recurred) → ee09770 (diagnostics-only: the "could not discover overlay addrs" branch had zero diagnostics, added a dump) → **aa0dfb3** (real root cause from the ee09770 CI log: `ip rule add` errors `RTNETLINK answers: File exists` on an exact duplicate — unlike `ip addr add` — and the rule persists across TUN teardown since it's routing-policy-DB state, not device state; T-AND-L1 and T-AND-L2 reuse the same pool address so L2 hit L1's leftover rule and `run_ip`'s `bail!` killed `connect` before the TUN came up; fixed by adding this one rule via `std::process::Command` directly, tolerating `File exists` as success). **CI CONFIRMED GREEN on aa0dfb3**: `Android VPN e2e (api 30, x86_64)` → `PASS: 8 FAIL: 0` (T-AND-L2 path=DIRECT), `VPN cross check` both aarch64/x86_64-linux-android green, Mean Bean Deploy + Docker(GHCR) green — zero regressions |
| 4 | 4.3 findings write-back | Sonnet (Sonnet 5 review, no twin body change beyond 4.2's) | DONE | `SPIKE_FINDINGS.md` written (mirrors VPN_MACOS_SPIKE_FINDINGS.md structure, filled from CI evidence not TODO placeholders); CLAUDE.md Android block refreshed (bore-android-tun crate rationale, netd/`ip rule` finding, duplicate-tolerance correction, status flipped to Phase 4 DONE+CI-GREEN). No further `src/` twin changes needed beyond aa0dfb3 (4.2) — the twins were already final. Phase 4 DONE |
| 5 | 5.1 docs/ANDROID.md + limits refresh | Haiku | DONE | Two-pass: (1) Haiku draft was VPN-only backend reference w/ 8 factual errors (TUN naming, rp_filter mechanism, self-contradicting `--advertise` example w/ hallucinated `--server` flag, placeholder error table, backwards `/dev/tun` vs `/dev/net/tun` claim, mislabeled unsafe boundary, fabricated Phase 5.2 claim, stale troubleshooting wording), all found+fixed against src/vpn.rs/main.rs/bore-android-tun source directly (commit 2b8527c). (2) Cross-check vs phase_05.md spec found the draft didn't match the required section order/content at all (missing Install/Termux, feature matrix across ALL subcommands, non-root notes, root quickstart, emulator pointers) — rewrote docs/ANDROID.md to spec order, kept prior detail as a "VPN backend reference" section; corrected VPN_ANDROID_ACTUAL_LIMIT.md to add the missing "non-root VPN impossible (VpnService app-only)" limit and separate it from the `--advertise`/gateway limits, which are v1-scoped-deferred (D-A4) not technically impossible (commit fa796cd). INSTALL_BORE.md/DOWNLOAD_URLS.md verified already correct, no edit needed. Gates green (fmt/clippy default+vpn/test); done-criteria link check passes (`grep -o '](\S*\.md' docs/ANDROID.md` all targets exist). |
| 5 | 5.2 VPN_ANDROID_ACCEPTANCE.md | Haiku | TODO | |
| 5 | 5.3 release verify + final read | Sonnet 5 | TODO | |

## Test status

| ID | What | Phase | Status |
|----|------|-------|--------|
| T-AND-B1, B2 | build matrix (targets, clippy) | 1 | PENDING CI (pushed, awaiting run) |
| T-AND-B3 | `just android-x86_64` local build | 1 | BLOCKED — dev box has no NDK/cargo-ndk installed; covered indirectly by CI's cargo-ndk legs (B1/B2) instead |
| T-AND-E1..E6 | non-VPN emulator e2e | 2 | PASS (run 28617339653 x2) |
| T-AND-E-CI | android-emu-e2e job green | 2 | PASS 2x consecutive (28617339653 initial + rerun) |
| unit: android_apply_builds_expected_argv | apply argv + revert LIFO | 3 | PASS |
| unit: android_apply_rejects_gateway_inputs | defense in depth | 3 | PASS |
| unit: android_stale_reclaim_removes_leaked_state | reclaim | 3 | PASS |
| unit: android_guard_matrix | CLI guard table | 3 | PASS |
| regression: vpn_netns_test.sh full | Linux zero-regression proof | 3,4,5 | PASS 161/0 |
| T-AND-S1..S3 | spike (tun, apply/revert, reclaim) | 4 | PASS (run 28637749882, commit aa0dfb3) |
| T-AND-L1 | relay link bidirectional ping | 4 | PASS (run 28637749882, commit aa0dfb3) |
| T-AND-L2 | direct best-effort (informational) | 4 | PASS, path=DIRECT (run 28637749882, commit aa0dfb3) |
| T-AND-L3..L5 | negatives (non-root, advertise, queues) | 4 | PASS (run 28637749882, commit aa0dfb3) |
| T-AND-M1..M5 | manual acceptance, real device | 5 | TODO (hardware) |

## Docs status

| Doc | Phase | Status |
|-----|-------|--------|
| CLAUDE.md Android block | 3.4, 5.3 | DONE (3.4, will refresh at 5.3) |
| SPIKE_FINDINGS.md (plan folder) | 4.3 | DONE |
| docs/ANDROID.md | 5.1 | DONE |
| limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md rewrite | 5.1 | DONE |
| docs/vpn/VPN_ANDROID_ACCEPTANCE.md | 5.2 | TODO |
| INSTALL_BORE.md / DOWNLOAD_URLS.md android rows | 5.1 | DONE (already correct pre-existing, verified, no edit needed) |
