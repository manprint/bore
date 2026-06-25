# Resume — VPN macOS Completion

Machine-readable progress tracker. Implementer updates this after every
sub-phase. All TODO at init.

## Phase status

| Phase | Sub-phase | Model | Status |
|-------|-----------|-------|--------|
| 1 | 1.1 macOS CI build job (`macos-14`) | Haiku | TODO |
| 1 | 1.2 De-risk spike + findings + PF/builder validation | Sonnet / Opus gate | TODO |
| 2 | 2.1 Flip cfg gates to `any(linux, macos)` | Opus→Sonnet | TODO |
| 2 | 2.2 cfg-split runtime + macOS stubs | Opus→Sonnet | TODO |
| 2 | 2.3 Platform flag warnings | Haiku | TODO |
| 3 | 3.1 macOS `create_tun` twin | Opus→Sonnet | TODO |
| 3 | 3.2 macOS name-resolution tests + smoke hook | Sonnet | TODO |
| 4 | 4.1 macOS `NetConfig::apply` twin | Opus→Sonnet | TODO |
| 4 | 4.2 macOS Drop + `stale_reclaim` twin | Opus→Sonnet | TODO |
| 4 | 4.3 macOS state-file helpers | Sonnet | TODO |
| 4 | 4.4 macOS rule-plane unit tests | Sonnet | TODO |
| 5 | 5.1 macOS single-host smoke e2e (CI) | Sonnet | TODO |
| 5 | 5.2 Manual two-host acceptance checklist | Opus gate→Sonnet | TODO |
| 5 | 5.3 Linux regression proof | Sonnet | TODO |
| 6 | 6.1 Docs update | Haiku / Opus read gate | TODO |

## Test status

| Test ID | What it proves | Where | Status |
|---------|----------------|-------|--------|
| `macos_runtime_stubs_bail` | macOS stubs bail "pending" (Phase 2.2) | `src/vpn.rs` (macOS, `macos-14`) | TODO |
| `macos_tun_request_maps_auto_and_bore` | name mapping (Phase 3.1) | `src/vpn.rs` (macOS) | TODO |
| `macos_state_paths_under_var_run` | `/var/run` state paths (Phase 4.3) | `src/vpn.rs` (macOS) | TODO |
| `macos_other_fwdref_present_detects_peer` | refcount detection (Phase 4.3) | `src/vpn.rs` (macOS) | TODO |
| `macos_stale_reclaim_restores_forwarding` | reclaim plan (Phase 4.2) | `src/vpn.rs` (macOS) | TODO |
| `macos_drop_refcount_keeps_forwarding_when_peer_active` | last-out restore (Phase 4.2) | `src/vpn.rs` (macOS) | TODO |
| `macos_apply_plain_advertise_uses_sysctl_and_pf_nat` | apply argv + PF nat (Phase 4.4) | `src/vpn.rs` (macOS) | TODO |
| `macos_apply_netmap_uses_binat` | apply binat netmap (Phase 4.4) | `src/vpn.rs` (macOS) | TODO |
| `macos_apply_nat_masquerade_and_hub_and_forward_accept` | apply F2+hub+forward (Phase 4.4) | `src/vpn.rs` (macOS) | TODO |
| `macos_apply_no_route_manage_runs_nothing` | dry-run safety (Phase 4.4) | `src/vpn.rs` (macOS) | TODO |
| `macos_apply_non_gateway_only_routes` | non-gateway path (Phase 4.4) | `src/vpn.rs` (macOS) | TODO |
| `cmd_macos_builders_snapshot` (+ `macos_pf_ruleset_*`, `macos_parse_lan_iface_*`) | builders match validated grammar (Phase 1.2) | `src/vpn.rs:3179+` (Linux CI) | EXISTING (re-confirm) |
| `T-MAC-BUILD` | build+clippy+test on `macos-14` | CI `ci.yml` | TODO |
| `T-MAC-SMOKE` | single-host utun+apply+revert+reclaim under sudo | CI `ci.yml` | TODO |
| `T-MAC-MANUAL` | two-host relay/direct/gateway/teardown/reclaim | `docs/vpn/VPN_MACOS_ACCEPTANCE.md` (human) | TODO |
| `T-LINUX-REGRESS` | Linux netns suites unchanged | `scripts/vpn_netns_test.sh` + `_hard` | TODO |

## Docs status

| Doc | Action | Status |
|-----|--------|--------|
| `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md` | create (Phase 1.2) | TODO |
| `docs/vpn/VPN_MACOS_ACCEPTANCE.md` | create (Phase 5.2) | TODO |
| `docs/vpn/VPN_MACOS_PORT_PLAN.md` | mark phases landed (Phase 6.1) | TODO |
| `docs/vpn/VPN_MACOS.md` | runtime real, validated PF grammar (Phase 6.1) | TODO |
| `README` | macOS in VPN platform support (Phase 6.1) | TODO |
| `CLAUDE.md` | update macOS port block to "runtime landed" (Phase 6.1) | TODO |

## Invariant guards (must hold at every phase)

- I-M1 Linux byte-for-byte (`git diff` clean inside `cfg(linux)`; netns green)
- I-M2 compile-time twin only
- I-M3 data plane untouched
- I-M4 macOS offload off / queues 1
- I-M5 PF semantics mirror nft
- I-M6 RAII + SIGKILL parity on macOS
- I-M7 no `--version` probe for BSD tools
- I-M8 `--tun-name` advisory, read-back utunN

## Next

**Next:** Phase 1, sub-phase 1.1 — add the `macos-14` CI build job in
`.github/workflows/ci.yml` (Haiku). Phase 1.2 (spike) is hardware-gated and is a
hard prerequisite for Phase 4.
