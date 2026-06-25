# Phase 5 — Integration, e2e, acceptance

**Goal:** prove the macOS port end to end at the level CI can reach (single-host
smoke under sudo on `macos-14`), document the manual two-host acceptance that CI
cannot reach, and re-prove zero Linux regression. This phase establishes "done"
for the whole feature.

Invariants in play: I-M6 (RAII + SIGKILL parity), I-M1 (Linux unchanged).
Decisions: D3 (CI = build + single-host smoke; two-host is manual). Depends on:
Phases 3 + 4 complete.

> Opus gate on 5.2 acceptance assertions — they define "done" for the feature.

---

## Sub-phase 5.1 — macOS single-host smoke e2e in CI

**Model:** Sonnet

**Files:**
- `.github/workflows/ci.yml` — extend the `macos-14` job (from Phase 1.1) with a
  sudo smoke step.
- `examples/macos_vpn_spike.rs` — the `create-teardown` mode from Phase 3.2,
  extended to also exercise `NetConfig::apply` + Drop + `stale_reclaim` on a
  single host with `--no-route-manage` off in a self-contained way (see below).
  Reuse this example; do not add a new top-level dir.

**Change:**
1. Add a smoke mode `apply-revert` to `examples/macos_vpn_spike.rs` that, as root
   on macOS:
   a. calls the real `create_tun("auto", 10.255.255.1/30, 1350, 1)` → utunN;
   b. calls `NetConfig::apply(RealRunner, "smoke0", "listen", utunN,
      10.255.255.1, 30, peer_routes=[], advertised=[192.0.2.0/24], nat_maps=[],
      no_route_manage=false, hub=false, nat_masquerade=false,
      forward_accept=false)` — gateway mode so it enables forwarding + loads the
      PF anchor `bore_vpn/smoke0`. (192.0.2.0/24 is TEST-NET-1, safe.)
   c. asserts `sysctl -n net.inet.ip.forwarding` == 1 and
      `pfctl -a bore_vpn/smoke0 -sa` shows the loaded rules;
   d. drops the `NetConfig` → asserts the anchor is empty/flushed and forwarding
      restored to its pre-run value;
   e. drops the device → asserts the utun is gone.
   f. SIGKILL-recovery check: re-run a fresh `apply` then `std::process::exit`
      WITHOUT running Drop (simulate SIGKILL is not possible in-process; instead
      provide a second invocation mode `leak-then-reclaim`: first call applies and
      leaks the state file + anchor by `std::mem::forget(netcfg)` then exits;
      second call runs `stale_reclaim("smoke0", "listen")` and asserts the anchor
      is flushed + forwarding restored). The CI step runs both invocations in
      sequence.
2. CI step (in the `macos-14` job, after the test step):
   `sudo target/debug/examples/macos_vpn_spike apply-revert` then
   `sudo target/debug/examples/macos_vpn_spike leak-then-reclaim`. Build the
   example first (`cargo build --features vpn --example macos_vpn_spike`). Mark
   the step `continue-on-error: false`. If GitHub `macos-14` runners forbid utun
   creation even under sudo (verify during implementation), downgrade this step to
   the `apply-revert` rule-plane portion using a capturing runner and move the
   real-utun assertion to the Phase 5.2 manual checklist — and `log`/note the
   downgrade explicitly in the job and in `resume.md` (no silent scope cut).

**Test IDs:**
- `T-MAC-BUILD` — the `macos-14` build+clippy+test job (Phases 1–4) green.
- `T-MAC-SMOKE` — the `apply-revert` + `leak-then-reclaim` CI steps green.

**Unit tests:** none new (this is e2e).

**Done-criteria:**
- `T-MAC-SMOKE` green on `macos-14`: utun created, gateway apply enables
  forwarding + loads anchor, Drop reverts both, device torn down, and
  `stale_reclaim` cleans a leaked run.
- If the runner forbids utun, the downgrade is documented and the rule-plane
  portion still runs.

---

## Sub-phase 5.2 — Manual two-host acceptance checklist (Opus gate → Sonnet)

**Model:** Opus gate on the assertions → Sonnet writes the checklist.

**Files (new doc under the existing tree):**
- `docs/vpn/VPN_MACOS_ACCEPTANCE.md`

**Change:** write a precise, copy-pasteable manual checklist a human runs with a
Mac (Apple Silicon, macOS 13+) and a Linux peer. It MUST assert the reference
scenario (overview §Reference scenario item 3). Each step has an exact command
and an exact expected observation:
1. Relay bring-up: macOS `bore vpn connect --to <linux-srv> --secret s --id m1
   --tun-name auto --accept-all-routes`; assert a utunN appears
   (`ifconfig utunN`), the overlay /30 is set, and the Linux peer pings the
   macOS overlay address.
2. Direct upgrade: assert logs show the relay→direct QUIC switch within the retry
   grid (no `--relay-only`); assert ping continues across the switch (seamless,
   I-M3 reuse of the shared direct/relay fallback).
3. Gateway netmap (the binat path): macOS connector
   `--advertise 192.168.7.0/24@10.77.0.0/24 --nat-masquerade`; assert
   `sysctl -n net.inet.ip.forwarding` == 1, `pfctl -a bore_vpn/m1 -sa` shows the
   `binat` + `nat` rules, and the Linux peer reaches a real host at 192.168.7.x
   via the virtual 10.77.0.x.
4. RAII teardown: `Ctrl-C` the macOS side; assert the utun is gone, the PF anchor
   `bore_vpn/m1` is empty, and `net.inet.ip.forwarding` is back to its pre-run
   value.
5. SIGKILL recovery: re-run, `kill -9` the macOS process, restart it; assert
   `stale_reclaim` flushed the stale anchor and restored forwarding (no leaked
   rules in `pfctl -a bore_vpn/m1 -sa`).
6. Flag warnings: start with `--tun-queues 4 --stun-server x`; assert the macOS
   advisory warnings (Phase 2.3) appear and the link still comes up.

**Test ID:** `T-MAC-MANUAL` (human-run; recorded as pass/fail with date + macOS
version in the doc).

**Unit tests:** none.

**e2e tests:** the checklist IS the two-host e2e.

**Done-criteria:**
- `docs/vpn/VPN_MACOS_ACCEPTANCE.md` exists with all six steps, each with an
  exact command + exact expected observation, Opus-approved.
- A human has executed it once on a Mac+Linux pair and recorded the result.

---

## Sub-phase 5.3 — Linux regression proof

**Model:** Sonnet

**Files:** none (verification only) — optionally a one-line note in `resume.md`.

**Change:**
1. Rebuild the release binary as your user (not root):
   `cargo build --release --features vpn`.
2. Run the full Linux e2e netns suite:
   `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vpn_netns_test.sh`
   and `sudo -n .../scripts/vpn_netns_test_hard.sh`.
3. Run `git diff` over `src/vpn.rs` and confirm every change sits inside a
   `#[cfg(target_os="macos")]` block, a newly-added cfg attribute, or a shared
   item that is provably behavior-identical on Linux (I-M1). Any change inside a
   `#[cfg(target_os="linux")]` body is a regression — revert it.

**Test ID:** `T-LINUX-REGRESS` — both netns suites green.

**Unit tests:** the full `cargo test --features vpn` on Linux green.

**Done-criteria:**
- `scripts/vpn_netns_test.sh` and `_hard` 100% green (same counts as before the
  port).
- `git diff` confirms no semantic edit inside any `cfg(linux)` body (I-M1).

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --features vpn --all-targets -- -D warnings` (Linux + `macos-14`).
- **Test:** `cargo test --features vpn` (both targets).
- **macOS e2e:** `T-MAC-SMOKE` green in CI.
- **Linux e2e:** `T-LINUX-REGRESS` green.

## Phase done criterion

`T-MAC-BUILD` + `T-MAC-SMOKE` green in CI; `T-MAC-MANUAL` checklist authored and
executed once on real hardware; `T-LINUX-REGRESS` proves zero Linux regression.
The reference scenario (overview) is satisfied.
