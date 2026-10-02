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
- Next action: P1 closure (G-P1 gates + self-review).
- Next eligible plan unit: P1.
- Unit base: f7b0745; branch: dev.
- Repo state: plan commits on dev after f7b0745 (latest: the 1.4 commit, matched by `PEV-Unit: 1.7`).

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
| G-U02 | `cargo test --lib liveness` | PASS | 6 new | 0.2 diff | 2026-10-02 |
| G-U12 (1.3) | `cargo test --test outage_liveness_test` | PASS | 10 passed (5 new client-side) | 1.3 diff | 2026-10-02 |
| full `cargo test` (1.3) | `cargo test` | PASS (env-only red) | 1095 pass; only `vhost_entry_redirect_overrides_both` (port 19000 foreign pid 3179752, §9) | 1.3 diff | 2026-10-02 |
| G-PHASE P0 | `cargo test` / `--features vpn` / `--features ssh-gateway` | PASS (env-only reds) | default 1010 pass + `transfer_ask_confirm_returns_err_when_no_tty_available` flaky (passes on rerun 43/0; dials default port while other tests run servers); vpn 1204 pass + `vhost_entry_redirect_overrides_both` red = port 19000 held by a foreign `node build/new_configurator/index.js` (pid 3179752) on this workstation; ssh 991 pass 0 fail | HEAD after 0.4 | 2026-10-02 |

| Review | Reviewer | Plan revision / reviewed change | Invariants/assertions checked | Verdict |
|--------|----------|---------------------------------|------------------------------|---------|

## 8. Technical revisions and deviations

| Revision | Previous decision/step | Approved replacement and reason | Supervisor | Dependents/revalidation |
|----------|------------------------|---------------------------------|------------|------------------------|
| 1e | 1.2 gate list; plan had no transfer-listener unit | (a) 1.2's G-U12 covered the vhost/public/secret reapers but not ssh-jump's, although 1.2 wired it: `declared_silent_native_provider_is_transport_reaped` in `tests/ssh_jump_test.rs` closes the gap (helper `spawn_bore_server_full` adds the transport floor). (b) New sub-phase **1.7**: a persistent `bore transfer listener` (`--persistent`) EXITS when its secret-provider task returns `Err` (`run_listener`: "transfer listener transport failed") — after 1.3 that now happens 15 s into an outage instead of after ~15 min, which would turn a slow failure into a fast permanent one. In persistent mode the listener must re-create its provider with the reconnect backoff and keep its loopback listener; one-shot mode keeps failing fast (the sender's resume state survives). | claude-opus-5-5 | 1.7; P1 closure; 4.1 transfer scenario |
| 1d | 1.4: `Proxy::listen` trips on server silence unconditionally (D2) | Three availability refinements found while testing, all within the requirement "a server outage must not cost more than it must": (a) **consumer on DIRECT does not trip** — the direct path runs consumer↔provider without the server, so a server outage must not end a working tunnel; it warns once and reconnects when the direct path itself closes (already an existing arm). In the field case (IP change) the direct path dies within the QUIC idle timeout (10 s) and the trip follows, so detection is unchanged. (b) **provider lingers its direct path on a TRIP** — `provider_direct` used to close the QUIC listener when the control connection went away (correct for a clean exit: consumers re-negotiate at once). On a trip (server LOST, not left) it now keeps serving the consumers already on it until their connections end (`UdpProviderCfg.linger_direct`, live counter + `LiveGuard`), while the reconnected provider registers afresh for new consumers. Deferring the provider's trip instead was rejected: it would hide the provider from NEW consumers for as long as an old direct session lasted. (c) **per-carrier watch for consumer relay carriers** — the server heartbeats every consumer carrier, so each carrier's drain task checks its own silence and leaves the pool (connections routed to a dead carrier would otherwise hang); public/vhost carriers have no server beat (`serve_carrier` only reads), so they rely on the main trip. VPN (2.4) must apply rule (a) to its 1:1 direct path. | claude-opus-5-5 | 2.4: direct-alive VPN link must not be torn down by server silence; 1.6 README documents (a)/(b) |
| 1c | 1.3 S3: carrier activities in a local `Vec` | `liveness::ConnActivities` (track/forget/terminate_all) shared with 1.4 and 2.x, plus `mux::ConnActivity::same_connection`. Each carrier pump tracks on start and forgets on exit, so the set never holds a finished carrier and stays bounded across re-dials; only a trip terminates (clean exits keep the graceful close and in-flight streams). The flick test (I-3) uses 2.5 s of a 4 s deadline: a 1 s flick of 3 s let a `deadline/4` mutation pass by tick-phase luck. | claude-opus-5-5 | 1.4 uses ConnActivities |
| 1b | D3: `CTRL_CLIENT_HEARTBEAT` 5 s | **2 s**. Reason (arithmetic, pinned by `server_deadline_covers_a_beat_plus_the_retransmit_ladder`): on an idle tunnel the server's silence after a flick = one beat + Linux's RTO ladder (next retransmit 12.6 s after first loss for flicks 6.2–12.6 s at RTO 200 ms); 5 + 12.6 > 15 would let the server reap a client that itself survives; 2 + 12.6 < 15 restores I-3 on the server side. Server deadline formula unchanged (max(3×2 s, 15 s) = 15 s). Cost: one ~15 B frame / 2 s / tunnel. Also added `liveness::TransportReaper` + `reap_if_due` used by every server loop. | claude-opus-5-5 | 3.1 OWNER_HEARTBEAT follows the same reasoning (2 s); docs (1.6/3.3/4.3) quote 2 s |
| 1a | 0.2 contract: `secret::CTRL_CLIENT_HEARTBEAT` kept as a delegating const | the const became unused (only `ctrl_client_heartbeat()` read it), so it is removed and doc links point at `liveness::CTRL_CLIENT_HEARTBEAT`; `ctrl_client_heartbeat()` keeps its name. Added `liveness::LivenessTicker` (disarmable tick) + `declared_ms_for(Duration)` used by 1.x/2.x/3.x. `CTRL_HEARTBEAT_SEND_TIMEOUT` stays 10 s; its doc no longer claims "≤ heartbeat" (beats are awaited in place, cannot queue). | claude-opus-5-5 | none |

## 9. Blockers

None. Environment note: ports 19000/19001 are held by a foreign node process on this workstation, so `vhost_entry_redirect_overrides_both` cannot pass locally; CI is its oracle.

## 10. Do-not-repeat

- Do not measure liveness on control frames: they queue behind bulk data (D-R4).
- Do not use `timeout(recv)` inside a `select!` (DEC-VE3).

## 11. Progress board

### Sub-phases
| ID | Phase file | Depends on | Status | Attempt | Evidence / reason |
|----|------------|------------|--------|---------|-------------------|
| 0.1 | phase_01.md | none | DONE | 1 | G-U01 15/0 (5 new); terminate red-checked |
| 0.2 | phase_01.md | none | DONE | 1 | G-U02 6 liveness tests pass |
| 0.3 | phase_01.md | none | DONE | 1 | G-U03 8/0 (sequence [1,2,4,8,8,8] + default_cap_is_eight_seconds) |
| 0.4 | phase_01.md | 0.3 | DONE | 1 | README backoff text 1,2,4,8; anchor to liveness section (added in 1.6) |
| 1.1 | phase_02.md | P0 | DONE | 1 | G-U11 70/0 (7 new serde tests) |
| 1.2 | phase_02.md | 1.1 | DONE | 1 | G-U12 5/0; red-check: neutered reap_if_due → 4 FAIL; reap-undeclared → I-5 test FAIL |
| 1.3 | phase_02.md | 1.1 | DONE | 1 | G-U12 10/0 (5 new); red-checks: liveness arm off → 3 trip tests time out; `terminate_all` off → carrier test FAIL; `deadline/4` → flick test FAIL 3/3 |
| 1.4 | phase_02.md | 1.1 | DONE | 1 | G-U12 16/0 (6 new); red-checks: Proxy liveness arm off → trip test times out; per-carrier watch off → dead-carrier test FAIL; watch + terminate_all off → carrier-close test FAIL; no beats → heartbeat test FAIL; trip-regardless-of-path → direct-survival test FAIL; linger off → provider direct test FAIL; full `cargo test` 1100+ passed, only failure = env `vhost_entry_redirect_overrides_both` (foreign port 19000) |
| 1.5 | phase_02.md | 1.1 | DONE | 1 | G-U12 19/0 (3 new) + `sticky_preferred_port_table`; red-checks: server ignores `preferred_port` → `public_port_zero_reconnect_gets_the_same_port` FAIL; no fallback on a taken preferred port → `preferred_port_taken_falls_back_to_random` FAIL. The `bore local` closure wiring (store after connect) is gated end-to-end by T-OUT-IPCHANGE0 (4.1) |
| 1.6 | phase_02.md | 1.2–1.5 | DONE | 1 | README "Connection liveness and outage recovery" checked claim by claim against the code (attempt bounds = `NETWORK_TIMEOUT` 3 s per step; server heartbeat = control connections only, carriers are not beaten). Revision 1e gate added: `declared_silent_native_provider_is_transport_reaped` (ssh_jump_test) — red-checked by replacing the `reap_if_due` call with `None` (a `.filter` after the call is NOT a mutation: `reap_if_due` terminates before returning) |
| 1.7 | phase_02.md (rev 1e) | 1.3 | DONE | 1 | G-U12 21/0 (2 new: persistent + one-shot-waiting listener register again after a 3 s blackhole); red-check: old `bail!`/`return Err` on a finished provider task → both FAIL; transfer_test 43/0, transfer_stdin_cli_test 14/0, transfer_link_test 22/0. Scope: re-registration applies to BOTH modes while waiting (a one-shot listener that has not received its sender has not done its job); a transfer in progress keeps failing as before (resume state kept). README transfer flags + liveness section updated |
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
| 0 | phase_01.md | P0 | DONE | self-review: no callers of terminate yet; heartbeat 5 s only speeds beats; PEV-Unit P0 |
| 1 | phase_02.md | P1 | TODO | — |
| 2 | phase_03.md | P2 | TODO | — |
| 3 | phase_04.md | P3 | TODO | — |
| 4 | phase_05.md | P4 | TODO | — |

### Tests
| ID/name | Owning unit | Gate | Status | Evidence |
|---------|-------------|------|--------|----------|
| mux activity/terminate (5) | 0.1 | G-U01 | PASS | 15 passed; red-check terminate arm → timeout |
| liveness tables | 0.2 | G-U02 | PASS | 6 tests incl. LivenessTicker |
| backoff cap | 0.3 | G-U03 | PASS | 8 passed |
| serde defaults | 1.1/2.1/3.1 | G-U11 | PASS (1.1) | 7 new in shared::tests |
| outage_liveness_test | 1.2–1.5 | G-U12 | PASS (1.7: 21/0) | server reapers vhost/public/secret provider/consumer + I-5; client trips public/vhost/secret provider+consumer, flick survival, carrier termination, consumer direct survival, provider linger, sticky public port |
| vpn liveness/teardown | 2.2–2.4 | G-U2 | TODO | — |
| owner + sshgw | 3.1/3.2 | G-U3 | TODO | — |
| T-OUT-* | 4.1 | G-NETNS-OUT | TODO | — |

### Documentation
| Document/sections | Owning unit | Status | Evidence |
|-------------------|-------------|--------|----------|
| README auto-reconnect backoff | 0.4 | DONE | README §Automatic reconnection |
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
