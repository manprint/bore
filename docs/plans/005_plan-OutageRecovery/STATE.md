# Outage recovery — Implementation state

Read this first in every session. Updated 2026-10-02 by claude-opus-5-5.

- Plan ID: 005_plan-OutageRecovery. Revision: 1. Repo root: /mnt/fabio/dati/Git/Github-manprint/bore-forked.
- Plan baseline: f7b0745 (branch dev, clean tree); pre-existing changes: none.
- Roster: agent-1 = claude-opus-5-5. With a single-agent roster, agent-1 implements and explicitly self-reviews as strong supervisor.
- Active execution style: handoff (single session).
- State writer: claude-opus-5-5 session 7e4be364; ownership: active.

## 0. Resume and completion protocol

1. Read this state. Identify readiness, scope, unit/attempt, step, checkpoint, branch and baseline. A new session resumes only after the previous writer has released ownership.
2. Inspect HEAD and working changes against the §6 checkpoint, and preserve unrelated changes. Before new work, reconcile any unresolved completion reference by searching `git log` for the exact trailers `PEV-Plan: 005_plan-OutageRecovery`, `PEV-Unit: <id>` and `PEV-Attempt: <n>`.
3. Read the phase file for the unit. Locate symbols by name; line numbers are hints only. Changed contracts go to the supervisor (agent-1).
4. Before editing, open the unit in §1 and mark its sub-phase IN_PROGRESS in §11. Checkpoint §6 after meaningful batches.
5. Run the unit gates and confirm the intended tests ran (test names/counts are in the output). Record the results in §7.
6. Docs, ledger, progress, next unit. Then commit on `dev`, staging only owned paths, with trailers `PEV-Plan`, `PEV-Unit`, `PEV-Attempt` and `PEV-Result: complete`, plus the session attribution lines.
7. If interrupted before the commit, finish the closure first.
8. After a phase's sub-phases, open P<N>: run the phase gates, self-review, then commit the closure.
9. Escalate after two failed fixes of the same issue. Never weaken acceptance.
10. On handoff, save the exact next action here.

## 1. Current unit and scope

- Active scope: plan (all phases) — user: "implementa il piano, testa tutto, committa su Dev e segui la ci".
- Scope result: RUNNING.
- Type / ID / attempt: none.
- Next action: open 0.2 (`src/liveness.rs`).
- Next eligible plan unit: 0.2.
- Unit base: f7b0745; branch: dev.
- Repo state: HEAD f7b0745 plus the untracked plan folder.

## 2. Feature context and readiness

**Readiness:** READY. Supervisor validated revision 1 on 2026-10-02: every unit has a contract, steps and tests, and there are no open questions.

**Goal:** a ≈ 20 s outage, including an ISP IP change, never causes minutes of downtime in any mode, and brief flicks never disconnect (see overview.md).

**Current decisions:** D1–D10 and D-R1–D-R4 (overview.md).

**Research:** R1–R5 CONFIRMED; no required unverified claims.

## 3. Environment, settings, and gate registry

- Full autonomous: **true**. Sources: the invocation (`full-autonomous:true`) and the user memory "plans default full-autonomous".
- WIP commits: off.
- Completion policy: each completed sub-phase and phase closure is committed locally on `dev`.
- Push: the user explicitly authorized it on 2026-10-02 ("committa su Dev e segui la ci"). Push `dev` after G-FINAL, then watch CI until green.
- Setup:
  - cwd is the repo root.
  - Run sudo netns harnesses via their exact path: `sudo -n /abs/scripts/<x>.sh`. NOPASSWD covers `scripts/*`.
  - Never run two netns harnesses concurrently.
  - Release binary for the harnesses: `cargo build --release --features vpn,ssh-gateway`, built as the user, not root.

| Gate | Stage | Exact command (cwd repo root) | Required assertions |
|------|-------|-------------------------------|---------------------|
| G-FMT | every unit | `cargo fmt --all -- --check` | clean |
| G-CLIPPY | every unit | `cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features vpn,ssh-gateway -- -D warnings` | clean |
| G-U01 | 0.1 | `cargo test --lib mux::tests` | new 5 tests pass + existing |
| G-U02 | 0.2 | `cargo test --lib liveness` | tables pass |
| G-U03 | 0.3 | `cargo test --lib reconnect` | sequence test |
| G-U11 | 1.1 | `cargo test --lib shared` | serde defaults |
| G-U12 | 1.2–1.5 | `cargo test --test outage_liveness_test` | ≥ 10 tests, 0 failed |
| G-U2 | 2.x | `cargo test --features vpn --lib vpn` + `cargo test --features vpn --test vpn_liveness_test` | new tests pass |
| G-U3 | 3.x | `cargo test --lib web_transfer` + `cargo test --features ssh-gateway --lib sshgw` | new tests pass |
| G-PHASE | P0–P3 | `cargo test` + `cargo test --features vpn` + `cargo test --features ssh-gateway` | 0 failed |
| G-NETNS-OUT | 4.1 | `sudo -n $PWD/scripts/outage_netns_test.sh` | FAIL: 0; red on baseline |
| G-NETNS-REG | 4.2 | the `local_proxy_netns_test`, `secret_netns_test`, `vhost_netns_test`, `vpn_netns_test` and `ssh_gateway_test` harnesses, serially via `sudo -n $PWD/scripts/<x>.sh` | FAIL: 0 (or proven pre-existing) |
| G-CI | final | `git push origin dev`, then `gh run list --branch dev` / `gh run watch` | all workflows success |

## 4. Work ledger

| Type | ID / attempt | Plan revision | Agent | Changes | Evidence/review | Commit |
|------|--------------|---------------|-------|---------|-----------------|--------|
| plan | 005 / 1 | 1 | claude-opus-5-5 | plan folder | self-validated | committed with 0.1 |
| unit | 0.1 / 1 | 1 | claude-opus-5-5 | src/mux.rs ConnActivity/ActivityIo/terminate | self-review: drop path = yamux drop_all_streams | PEV-Unit 0.1 |

## 5. Files and ownership

| Path/hunks | Existing changes to preserve | Unit changes | Owning unit |
|------------|------------------------------|--------------|-------------|
| docs/plans/005_plan-OutageRecovery/* | none | NEW | plan |

## 6. In-flight checkpoint

None.

## 7. Verification and reviews

| Gate/test | Command | Result | Test count/named evidence | Tested revision/diff | When |
|-----------|---------|--------|--------------------------|---------------------|------|
| G-U01 | `cargo test --lib mux::tests` | PASS | 15 passed, 5 new | 0.1 diff | 2026-10-02 |
| G-FMT/G-CLIPPY | per §3 | PASS | clean | 0.1 diff | 2026-10-02 |

| Review | Reviewer | Plan revision / reviewed change | Invariants/assertions checked | Verdict |
|--------|----------|---------------------------------|------------------------------|---------|

## 8. Technical revisions and deviations

| Revision | Previous decision/step | Approved replacement and reason | Supervisor | Dependents/revalidation |
|----------|------------------------|---------------------------------|------------|------------------------|

## 9. Blockers

None.

## 10. Do-not-repeat

- Do not measure liveness on control frames: they queue behind bulk data (D-R4).
- Do not use `timeout(recv)` inside a `select!` (DEC-VE3).

## 11. Progress board

### Sub-phases
| ID | Phase file | Depends on | Status | Attempt | Evidence / reason |
|----|------------|------------|--------|---------|-------------------|
| 0.1 | phase_01.md | none | DONE | 1 | G-U01 15/0 (5 new); terminate red-checked |
| 0.2 | phase_01.md | none | TODO | 1 | — |
| 0.3 | phase_01.md | none | TODO | 1 | — |
| 0.4 | phase_01.md | 0.3 | TODO | 1 | — |
| 1.1 | phase_02.md | P0 | TODO | 1 | — |
| 1.2 | phase_02.md | 1.1 | TODO | 1 | — |
| 1.3 | phase_02.md | 1.1 | TODO | 1 | — |
| 1.4 | phase_02.md | 1.1 | TODO | 1 | — |
| 1.5 | phase_02.md | 1.1 | TODO | 1 | — |
| 1.6 | phase_02.md | 1.2–1.5 | TODO | 1 | — |
| 2.1 | phase_03.md | P1 | TODO | 1 | — |
| 2.2 | phase_03.md | 2.1 | TODO | 1 | — |
| 2.3 | phase_03.md | 2.1 | TODO | 1 | — |
| 2.4 | phase_03.md | 2.2 | TODO | 1 | — |
| 2.5 | phase_03.md | 2.2–2.4 | TODO | 1 | — |
| 3.1 | phase_04.md | P1 | TODO | 1 | — |
| 3.2 | phase_04.md | P1 | TODO | 1 | — |
| 3.3 | phase_04.md | 3.1, 3.2 | TODO | 1 | — |
| 4.1 | phase_05.md | P2, P3 | TODO | 1 | — |
| 4.2 | phase_05.md | 4.1 | TODO | 1 | — |
| 4.3 | phase_05.md | 4.2 | TODO | 1 | — |

### Phases
| ID | File | Closure unit | Status | Review / commit reference |
|----|------|--------------|--------|---------------------------|
| 0 | phase_01.md | P0 | TODO | — |
| 1 | phase_02.md | P1 | TODO | — |
| 2 | phase_03.md | P2 | TODO | — |
| 3 | phase_04.md | P3 | TODO | — |
| 4 | phase_05.md | P4 | TODO | — |

### Tests
| ID/name | Owning unit | Gate | Status | Evidence |
|---------|-------------|------|--------|----------|
| mux activity/terminate (5) | 0.1 | G-U01 | PASS | 15 passed; red-check terminate arm → timeout |
| liveness tables | 0.2 | G-U02 | TODO | — |
| backoff cap | 0.3 | G-U03 | TODO | — |
| serde defaults | 1.1/2.1/3.1 | G-U11 | TODO | — |
| outage_liveness_test | 1.2–1.5 | G-U12 | TODO | — |
| vpn liveness/teardown | 2.2–2.4 | G-U2 | TODO | — |
| owner + sshgw | 3.1/3.2 | G-U3 | TODO | — |
| T-OUT-* | 4.1 | G-NETNS-OUT | TODO | — |

### Documentation
| Document/sections | Owning unit | Status | Evidence |
|-------------------|-------------|--------|----------|
| README auto-reconnect backoff | 0.4 | TODO | — |
| README liveness section | 1.6 | TODO | — |
| README VPN | 2.5 | TODO | — |
| README web-transfer + SSH, SSH_GATEWAY.md | 3.3 | TODO | — |
| CLAUDE.md invariant + README final | 4.3 | TODO | — |

### Audits
| Report | Verdict | Current unresolved findings | Evidence |
|--------|---------|-----------------------------|----------|
| none | — | — | — |

## 12. Suspended implementation

Snapshot state: none.
