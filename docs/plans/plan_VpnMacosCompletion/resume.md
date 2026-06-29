# Resume — VPN macOS Completion

> Updated 2026-06-29. Implemented on a **Linux** dev box. macOS cannot be compiled
> locally (blake3 NEON C rejects cross-compile to `aarch64-apple-darwin`), so the
> `macos-14` CI job is the macOS compile/test oracle and a Mac is needed for the
> spike + manual acceptance. Linux is the hard local gate (I-M1).

## macOS CI status (macos-14 runner) — VALIDATED 2026-06-29

Run on branch `macos` (full suite, parity with main/dev) is **100% green**:
- **macOS VPN build (macos-14)**: `cargo build` + `clippy --all-targets -D warnings`
  + `cargo test --features vpn` all pass on real macOS (T-MAC-BUILD ✓).
- **macOS VPN device e2e (macos-14)**: real device tests under sudo all pass
  (T-MAC-SMOKE ✓): runner capability diagnostic (passwordless sudo + `pfctl` +
  `sysctl` available), the **spike** (PF grammar **accepted by real `pfctl`** —
  no longer PROVISIONAL for the validated rules incl. `binat` 192.168.7.0/24@
  10.77.0.0/24 + `nat`), `create_tun("auto")` → kernel-assigned **utun4** read
  back via `dev.name()` and seen in `ifconfig`, gateway apply/RAII-revert, and
  SIGKILL `leak-then-reclaim`.
- Hosted `macos-14` runners DO permit utun creation + PF + sysctl under sudo, so
  the device e2e runs on CI (no self-hosted runner needed for the single-host path).
- Still human-only: **T-MAC-MANUAL** two-host LAN gateway (CI has one host); the
  single-host PF rules are already validated by the spike.

## Local gate status (Linux)

- `cargo build --features vpn` — green.
- `cargo clippy --features vpn --all-targets -- -D warnings` — green.
- `cargo test --features vpn` — **610 passed / 0 failed** (unchanged count vs pre-port).
- `cargo fmt --all -- --check` — clean.
- `scripts/vpn_netns_test.sh` (sudo) — **PASS=161 FAIL=0** (2026-06-29; T-LINUX-REGRESS green, I-M1 proven).

## Phase status

| Phase | Status | Notes |
|-------|--------|-------|
| 1.1 macOS CI build job | **DONE** | `macos-vpn-build` (macos-14): build+clippy+test+example+smoke in `.github/workflows/ci.yml`. |
| 1.2 De-risk spike | **DONE (via CI)** | `examples/macos_vpn_spike.rs spike` runs in the `macos-vpn-e2e` CI job on macos-14 and PASSES — real `pfctl` accepts the `pf_ruleset` grammar, utun creates + reads back. PF no longer PROVISIONAL for the validated rules. Findings template `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md` (fill from CI log). |
| 2.1 Flip cfg gates | **DONE** | 6 anchors + grep hits widened to `any(linux,macos)` (vpn.rs:1, lib.rs, main.rs ×11). holepunch.rs VPN data-path **tests** left `cfg(linux)` (conservative; not CLI/enum wiring). |
| 2.2 cfg-split runtime | **DONE** | `create_tun`, `NetConfig::apply`, `Drop`→`restore_ip_forward_op`, `stale_reclaim`, offload pumps ×3 cfg-twinned. macOS test `macos_apply_stub_bails` kept for apply-error wording until 4 landed; now apply is real — test asserts tun-request + `/var/run` paths instead. |
| 2.3 Platform flag warnings | **DONE** | `macos_flag_warnings` (pure, tested) + `emit_macos_flag_warnings` once in `run_listen`/`run_connect` (hub via `run_listen`). |
| 3 macOS `create_tun` | **DONE (CI/spike-validate)** | Single-queue, no-offload utun; kernel assigns + reads back name (`dev.name()`); `macos_tun_request` maps auto/boreN→kernel, utunN→passthrough. tun-rs 2.8.5 API confirmed via docs. |
| 4 macOS NetConfig/Drop/stale_reclaim | **DONE (CI/spike-validate)** | `apply` = `route -n` + `sysctl net.inet.ip.forwarding` + PF anchor `bore_vpn/<id>` from `pf_ruleset` (MSS 1310). `/var/run` state files (D5) via `run_dir()` cfg-twin. PF PROVISIONAL until 1.2. |
| 5.1 single-host smoke (CI) | **DONE + GREEN** | `macos-vpn-e2e` job (macos-14, gating, NOT continue-on-error) runs spike + create-teardown + apply-revert + leak-then-reclaim — all pass on the hosted runner. |
| 5.2 manual two-host acceptance | **DONE (doc)/PENDING (run)** | `docs/vpn/VPN_MACOS_ACCEPTANCE.md` (T-MAC-MANUAL, 6 steps + result table). |
| 5.3 Linux regression proof | **DONE** | `vpn_netns_test.sh` PASS=161 FAIL=0 (2026-06-29); macOS code is additive `cfg(macos)` + behavior-preserving shared refactors (`run_dir()`). |
| 6 Docs | **PARTIAL** | `CLAUDE.md` macOS block → "runtime LANDED". `VPN_MACOS_PORT_PLAN.md`/`VPN_MACOS.md`/`README` still to drop PROVISIONAL/PENDING (after the spike validates PF). |

## Deviations from the plan (all I-M1-safe, documented)

1. **I-M3 caveat:** TUN offload pumps use Linux-only `tun-rs` APIs
   (`recv_multiple`/`send_multiple`/`GROTable`/`VIRTIO_NET_HDR_LEN`). Fixed by
   cfg-twinning the three `*_offload` fns (`cfg(linux)` real + `cfg(macos)`
   `unreachable!`); offload is always false on macOS so stubs never run.
2. **Linux `cmd_nft_*`/`cmd_iptables_*` left un-gated** — `hostcfg_cmd` has
   `#![allow(dead_code)]`, so unused-on-macOS does not trip clippy; gating ~460
   lines added risk for no benefit.
3. **State paths via `run_dir()` cfg-twin** (`/run` Linux, `/var/run` macOS, D5)
   instead of separate macOS twin helpers. Linux byte-identical.
4. **`run_listen_hub` warning skipped** — `run_listen` (its only caller) already
   warns; warning twice would duplicate.
5. **Phases 3–4 before the 1.2 spike** (D6 gates 4 on 1.2) — done best-effort
   because additive + zero-Linux-risk; PF grammar marked PROVISIONAL.
6. **Attribute-order gotcha:** on a cfg-twinned `pub` method, a doc comment must be
   followed by `#[allow(...)]` THEN `#[cfg(...)]`, else `missing_docs` misfires.

## Next

1. ~~Spike~~ — DONE via the `macos-vpn-e2e` CI job (real `pfctl` accepted the
   grammar). Optionally transcribe the CI spike log into `VPN_MACOS_SPIKE_FINDINGS.md`.
2. **T-MAC-MANUAL** (`VPN_MACOS_ACCEPTANCE.md`) on a Mac+Linux pair — the only
   remaining item CI cannot do (two hosts / real cross-LAN traffic). Single-host PF
   rules already validated by the CI spike.
3. Phase 6 doc rewrites (`VPN_MACOS_PORT_PLAN.md`/`VPN_MACOS.md`/`README`) — PF is
   now CI-validated, so PROVISIONAL/PENDING wording can be dropped.
