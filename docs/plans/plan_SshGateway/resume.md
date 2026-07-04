# SSH Access Gateway — Resume

> **Next:** none — plan closed, all 7 phases DONE.
> **Last updated:** 2026-07-04 (Phase 7.5 final gate closed)

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 1 — Scaffolding + russh spike | phase_01.md | `DONE` | — |
| 2 — LinkOpener / STREAM_READY confinement | phase_02.md | `DONE` | I-4 confirmed via `git grep STREAM_READY` in the final gate |
| 3 — Auth stores + hash-password | phase_03.md | `DONE` | — |
| 4 — Gateway core + public tunnels | phase_04.md | `DONE` | — |
| 5 — Vhost + secret + takeover | phase_05.md | `DONE` | Opus-reviewed (5.4 registry race semantics) |
| 6 — Demux control port + SSH-over-TLS | phase_06.md | `DONE` | Opus-reviewed (6.1 hot accept path + I-1); I-1 re-confirmed in the final gate via a clean `git show`/diff of the exact commit |
| 7 — FE, netns, CI, docs, final gate | phase_07.md | `DONE` | Found + fixed a real `ConnState` reference-cycle zombie-entry bug via the new netns harness (T-SSH-N1) — see phase_07.md and CLAUDE.md's I-SSH3 entry |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-SSH-SPIKE1..5 | cargo e2e (feature) | `DONE` | `tests/ssh_gateway_spike_test.rs` |
| link_open_ready_writes_single_zero_byte | unit | `DONE` | `src/mux.rs` |
| link_open_ready_ssh_writes_no_marker | unit | `DONE` | covered by `SshOpener::open` never calling `open_ready`/`write_stream_ready` (I-4) |
| KeyStore/PasswordStore unit matrix | unit | `DONE` | `src/sshgw_auth.rs` |
| hash_password_roundtrip | unit | `DONE` | `src/sshgw_auth.rs` |
| spec_matrix / params_* / direct_tcpip_dest | unit | `DONE` | `src/sshgw.rs` |
| demux_classify_first_byte_table / demux_classify_prefix_table / takeover_decision_table | unit | `DONE` | `src/sshgw.rs` (`should_reap_logic` folded into the keepalive-config unit tests, no separate pure fn was needed) |
| T-SSH-PUB1..3 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-WARN1 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-CANCEL1 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-PREAUTH1 / T-SSH-KEEP1 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-VH1/VH2 / T-SSH-PFX1 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-SEC1..3 | cargo e2e | `DONE` | `tests/ssh_gateway_test.rs` |
| T-SSH-TAKE1/2 | cargo e2e | `DONE` | `t_ssh_take1_same_identity_vhost_takeover` / `t_ssh_take2_different_identity_rejected` |
| T-SSH-DMX1/DMX2 / T-SSH-TLS1 / T-DMX-OFF | cargo e2e | `DONE` | `t_ssh_dmx1_...` / `t_ssh_dmx2_...` / `t_ssh_tls1_...` / `t_dmx_off_...` in `tests/ssh_gateway_test.rs` |
| admin_entry_transport_serialized | unit | `DONE` | `tests/admin_test.rs` |
| npm: flagBadges ssh badge (2 cases) | FE unit | `DONE` | `test/admin_ui/badges.test.js` |
| T-SSH-N1..N6 | netns (sudo) | `DONE` | `scripts/ssh_gateway_test.sh`, PASS: 10 FAIL: 0 on the dev box (2026-07-04); T-SSH-N1's real netfilter half-open found the `ConnState` reference-cycle bug (now fixed) |

Full cargo test suites (`cargo test`, `cargo test --all-features`) and `npm test` all green on
2026-07-04 against the final committed state. All five mandatory netns suites re-verified green
against the same `--release --features vpn,ssh-gateway` binary on 2026-07-04: `secret_netns_test.sh`
29/0, `vhost_netns_test.sh` 13/0, `local_proxy_netns_test.sh` 16/0, `ssh_gateway_test.sh` 10/0,
`vpn_netns_test.sh` 161/0.

## Docs

| File | Status | Notes |
|------|--------|-------|
| docs/plans/plan_SshGateway/SPIKE_FINDINGS.md | `DONE` | written in phase 1.2 |
| docs/SSH_GATEWAY.md (operational guide + status flip) | `DONE` | status header flipped to implemented; new §6 Guida operativa, every command verified against a real local server (2026-07-04) |
| CLAUDE.md (I-SSH block) | `DONE` | compact I-SSH1..5 + D1 + demux block added, includes the reference-cycle bug and fix |
| README.md (feature bullet) | `DONE` | new `#### SSH ingress gateway` subsection (README has no single bulleted feature list; matched the file's actual `####`-per-capability structure instead) |

## Open blockers
- none

## Decisions changed at runtime
- **T-SSH-DMX1's clause (c)** ("plain-HTTP admin/vhost request") was implemented as a plain-HTTP
  admin request specifically, not a vhost request — vhost wiring was out of scope for that
  harness's setup. T-DMX-OFF's clause (a) is covered by the existing unmodified
  `tests/tls_test.rs`/`tests/admin_test.rs` suites (run as part of the same `cargo test
  --all-features` gate) rather than duplicated inline, per the plan's own "reuse/extend an
  existing test only by RUNNING it" instruction.
- **`demux_pre_tls`'s return type is a 3-way `PreTlsRoute::{Ssh,Tls,Direct}`**, not the binary
  SSH/not-SSH split phase_06.md's prose first suggested — required so a plain HTTP/bore client
  keeps working on a port that also serves TLS (bypasses the TLS acceptor entirely for `Direct`).
  Caught before commit by re-reading the phase file's own more detailed wording, which already
  specified this; documented as an implementation note in phase_06.md.
- **`scripts/ssh_gateway_test.sh` uses a single client namespace** (`nscli` runs the ssh client,
  local services, autossh, and the native secret provider/consumer pair for T-SSH-N4) rather than
  the 3-namespace provider/consumer split other netns scripts use — sufficient for this harness's
  purpose (concurrent-usage safety, not topology realism).
- **The `ConnState` reference-cycle fix** (`drop(state)` before each `pending()` tail in
  `src/sshgw.rs`) was not anticipated by any phase document — found via T-SSH-N1's real netfilter
  half-open repro during Phase 7.2 and fixed in the same commit. Documented in phase_07.md,
  CLAUDE.md's I-SSH3 entry, and re-verified via a full re-run of all five netns suites.
