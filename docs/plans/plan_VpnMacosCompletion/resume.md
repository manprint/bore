# Resume — VPN macOS Completion

> Updated 2026-06-29. Implemented on a **Linux** dev box. macOS cannot be compiled
> locally (blake3 NEON C rejects cross-compile to `aarch64-apple-darwin`), so the
> `macos-14` CI job is the macOS compile/test oracle and a Mac is needed for the
> spike + manual acceptance. Linux is the hard local gate (I-M1).

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
| 1.2 De-risk spike | **HARDWARE-PENDING** | Harness `examples/macos_vpn_spike.rs` (mode `spike`) + findings template `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md`. Human runs on a Mac; Opus gates findings. PF grammar PROVISIONAL until then (D6). |
| 2.1 Flip cfg gates | **DONE** | 6 anchors + grep hits widened to `any(linux,macos)` (vpn.rs:1, lib.rs, main.rs ×11). holepunch.rs VPN data-path **tests** left `cfg(linux)` (conservative; not CLI/enum wiring). |
| 2.2 cfg-split runtime | **DONE** | `create_tun`, `NetConfig::apply`, `Drop`→`restore_ip_forward_op`, `stale_reclaim`, offload pumps ×3 cfg-twinned. macOS test `macos_apply_stub_bails` kept for apply-error wording until 4 landed; now apply is real — test asserts tun-request + `/var/run` paths instead. |
| 2.3 Platform flag warnings | **DONE** | `macos_flag_warnings` (pure, tested) + `emit_macos_flag_warnings` once in `run_listen`/`run_connect` (hub via `run_listen`). |
| 3 macOS `create_tun` | **DONE (CI/spike-validate)** | Single-queue, no-offload utun; kernel assigns + reads back name (`dev.name()`); `macos_tun_request` maps auto/boreN→kernel, utunN→passthrough. tun-rs 2.8.5 API confirmed via docs. |
| 4 macOS NetConfig/Drop/stale_reclaim | **DONE (CI/spike-validate)** | `apply` = `route -n` + `sysctl net.inet.ip.forwarding` + PF anchor `bore_vpn/<id>` from `pf_ruleset` (MSS 1310). `/var/run` state files (D5) via `run_dir()` cfg-twin. PF PROVISIONAL until 1.2. |
| 5.1 single-host smoke (CI) | **DONE** | example modes `create-teardown`/`apply-revert`/`leak-then-reclaim`; CI step `continue-on-error` (GitHub runner may forbid utun under sudo — plan-sanctioned, warns). |
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

## Next (Mac operator)

1. `sudo target/debug/examples/macos_vpn_spike spike` → fill
   `VPN_MACOS_SPIKE_FINDINGS.md`; patch `pf_ruleset`/`cmd_pf_*` + snapshots if any
   PF line is rejected.
2. Run `VPN_MACOS_ACCEPTANCE.md` (T-MAC-MANUAL) on a Mac+Linux pair.
3. After PF validated, finish Phase 6 doc wording.
