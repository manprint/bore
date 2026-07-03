# SSH Access Gateway — Resume

> **Next:** phase_01.md § 1.1 — Feature gate, dependencies, module stubs
> **Last updated:** 2026-07-03 (plan initialized)

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 1 — Scaffolding + russh spike | phase_01.md | `TODO` | — |
| 2 — LinkOpener / STREAM_READY confinement | phase_02.md | `TODO` | independent of Phase 1 |
| 3 — Auth stores + hash-password | phase_03.md | `TODO` | needs Phase 1 |
| 4 — Gateway core + public tunnels | phase_04.md | `TODO` | needs 1, 2, 3; read SPIKE_FINDINGS.md first |
| 5 — Vhost + secret + takeover | phase_05.md | `TODO` | needs 2, 4 |
| 6 — Demux control port + SSH-over-TLS | phase_06.md | `TODO` | needs 4 (5 recommended) |
| 7 — FE, netns, CI, docs, final gate | phase_07.md | `TODO` | needs 4-6 |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-SSH-SPIKE1..5 | cargo e2e (feature) | `TODO` | russh primitives vs real ssh CLI (pubkey, -R, -L, exec/env, keepalive) |
| link_open_ready_writes_single_zero_byte | unit | `TODO` | yamux open_ready marker |
| link_open_ready_ssh_writes_no_marker | unit | `TODO` | I-4 on SSH variant |
| KeyStore/PasswordStore unit matrix | unit | `TODO` | parse, options, hot-reload, mtime cache, argon2 cap |
| hash_password_roundtrip | unit | `TODO` | subcommand output accepted by store |
| spec_matrix / params_* / direct_tcpip_dest | unit | `TODO` | D1 heuristic + prefix + param grammar |
| should_reap_logic / demux_classify_first_byte / takeover_decision_table | unit | `TODO` | pure-fn tables |
| T-SSH-PUB1..3 | cargo e2e | `TODO` | public via ssh: roundtrip, port 0, params+max-conns |
| T-SSH-WARN1 | cargo e2e | `TODO` | I-2 warning on udp=on |
| T-SSH-CANCEL1 | cargo e2e | `TODO` | forward teardown frees listener |
| T-SSH-PREAUTH1 / T-SSH-KEEP1 | cargo e2e | `TODO` | pre-auth timeout; idle session survives 90 s |
| T-SSH-VH1/VH2 / T-SSH-PFX1 | cargo e2e | `TODO` | vhost via ssh, gateway basic-auth 401/200, prefixes |
| T-SSH-SEC1..3 | cargo e2e | `TODO` | ssh provider / ssh consumer / both; one admin row per session |
| T-SSH-TAKE1/2 | cargo e2e | `TODO` | same-key takeover; different-key reject |
| T-SSH-DMX1/DMX2 / T-SSH-TLS1 / T-DMX-OFF | cargo e2e | `TODO` | 4-protocol port, banner timeout, SSH-over-TLS, off-path identical |
| admin_entry_transport_serialized | unit | `TODO` | additive JSON fields |
| npm: flagBadges ssh badge (2 cases) | FE unit | `TODO` | — |
| T-SSH-N1..N6 | netns (sudo) | `TODO` | half-open reap, autossh recovery, takeover under partition, mixed native+ssh, throughput (informative), password auth |

## Docs

| File | Status | Notes |
|------|--------|-------|
| docs/plans/plan_SshGateway/SPIKE_FINDINGS.md | `TODO` | written in phase 1.2 |
| docs/SSH_GATEWAY.md (operational guide + status flip) | `TODO` | phase 7.4 |
| CLAUDE.md (I-SSH block) | `TODO` | phase 7.4 |
| README.md (feature bullet) | `TODO` | phase 7.4 |

## Open blockers
- none

## Decisions changed at runtime
- none
