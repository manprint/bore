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
- Next action: 4.1 — run `sudo -n $PWD/scripts/outage_netns_test.sh` on the release build (`--features vpn,ssh-gateway`), iterate to green, then red-check on a baseline f7b0745 release build (`BORE=` override); record both runs in §7.
- Next eligible plan unit: 4.1.
- Unit base: f7b0745; branch: dev.
- Repo state: plan commits on dev after f7b0745 (latest: P2, matched by `PEV-Unit: P2`).

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
| G-PHASE P3 (G-P3) | main tree at 91243d6 (uncommitted: only the 4.1/4.2 harness + workflow, not compiled): fmt; clippy default, `vpn,ssh-gateway`, all-features; all-features `--no-fail-fast` lib/bins/examples/tests (skip `t_ssh_`/`t_dmx_`); doc; ssh serial; web serial; no-default `transfer_link_test`; `cargo test --features ssh-gateway --no-fail-fast`; root `npm test`; `web/transfer` `npm run test:unit` | PASS (env-only red) | all-features 1415 passed, 1 failed = `vhost_entry_redirect_overrides_both` (port 19000, §9); doc 1/0; ssh serial 48/0; web serial 41/0; no-default 22/0; `--features ssh-gateway` 1294 passed, the same 1 env-only failure; npm 125/0; web unit 227/0; fmt + 3 clippy clean. (A first run was void: the root filesystem that holds /tmp hit ENOSPC mid-run (what filled it was not identified; ~36 GB of /tmp belongs to other projects' sessions) and every log came back empty; rerun after freeing this plan's own 14 GB isolated target) | HEAD 91243d6 | 2026-10-02 |
| G-NETNS-SSH (P3) | `sudo -n $PWD/scripts/ssh_gateway_test.sh` (release `--features vpn,ssh-gateway` at 91243d6, serial) | PASS | 21 passed, 0 failed; T-SSH-N1 cleared the half-open session's admin row after 15 s (bound 20 s; the pre-plan 60 s reaper cannot pass it) | HEAD 91243d6 | 2026-10-02 |
| G-U3 (3.4) | isolated tree holding exactly the commit: fmt; clippy default + all-features; `--lib web_transfer`; web_transfer_deploy_test + web_transfer_fuzz; web_transfer_test `--test-threads=1`; `npm run test:unit`; `npx playwright test --project=chromium` with `BORE_E2E_BIN`/`BORE_E2E_OWNER_BIN` = that tree's all-features debug build | PASS | lib 208/0; deploy 3/0; fuzz 4/0; web serial 41/0 (1 ignored = bench); npm unit 227/0; Playwright chromium 67 passed, 1 skipped (engine-conditional `test.skip`), 0 failed; dist rebuilt and byte-identical to `npm run build` | 3.4 diff on 53f8e31 | 2026-10-02 |
| G-PHASE P2 | `cargo fmt --check`; clippy default + `vpn,ssh-gateway`; `cargo test --features vpn --no-fail-fast` — on an isolated worktree holding exactly the P2 commit | PASS (env-only red) | 1375 passed, 1 failed = `vhost_entry_redirect_overrides_both` (port 19000, §9); `--lib vpn` 142/0 (2 new `PumpGuard` tests), vpn_liveness_test 12/0 | HEAD 8454a73 + revision 1g | 2026-10-02 |
| G-NETNS-VPN (P2) | `sudo -n $PWD/scripts/vpn_netns_test.sh` (release `--features vpn`, serial) | PASS | 169 passed, 0 failed, 1 SKIP = T-PINMTU (its stimulus — shrinking the path — did not move the UNPINNED control TUN, 1414 -> 1414, so the pinned arm cannot discriminate and is not run; on the CI baseline the same test was already red, 171/1, before this plan). Before revision 1g the same run was red on T-new-2/T-new-3 (a second `bore1` beside the still-open `bore0`) | HEAD 8454a73 + revision 1g | 2026-10-02 |
| G-PHASE P1 (G-P1) | all-features `--no-fail-fast` lib/bins/examples/tests (skip `t_ssh_`/`t_dmx_`), doc, ssh serial, web serial, no-default `transfer_link_test`, `npm test`; `cargo fmt --check`; both G-CLIPPY | PASS (env-only red) | 1489 passed, 1 failed = `vhost_entry_redirect_overrides_both` (port 19000 held by the foreign `node build/new_configurator/index.js`, pid 3179752); the same test passes in a private network namespace (`unshare -rn`, 1/0); doc 1/0, ssh 48/0, web 40/0 (1 ignored), no-default 22/0, npm 0 fail; fmt + clippy (default and `vpn,ssh-gateway`) clean | HEAD 52e0428 | 2026-10-02 |
| G-PHASE P0 | `cargo test` / `--features vpn` / `--features ssh-gateway` | PASS (env-only reds) | default 1010 pass + `transfer_ask_confirm_returns_err_when_no_tty_available` flaky (passes on rerun 43/0; dials default port while other tests run servers); vpn 1204 pass + `vhost_entry_redirect_overrides_both` red = port 19000 held by a foreign `node build/new_configurator/index.js` (pid 3179752) on this workstation; ssh 991 pass 0 fail | HEAD after 0.4 | 2026-10-02 |

| Review | Reviewer | Plan revision / reviewed change | Invariants/assertions checked | Verdict |
|--------|----------|---------------------------------|------------------------------|---------|
| P3 self-review | claude-opus-5-5 (agent-1, supervisor) | revisions 1f, 1h, 1i, 1j; 3.1–3.4 | owner: a legacy server (no heartbeat) never arms the owner's deadline and a legacy owner (declares 0) is never transport-reaped — the pair is byte-identical (DEC-VE2); the owner's explicit close path is unchanged; SSH: russh resets the keepalive counter on ANY received byte, so 15 s counts from the last byte and a busy session is never reaped, and KEEP1 holds an idle session for 2× the deadline on keepalives alone; browser: wire unchanged (RFC 6455 Ping/Pong), a client that never answers a Ping keeps the legacy 60 s, every new write is bounded (P-9), the first Ping never races the welcome/snapshot frames; the bore-ssh-client image defaults now match the documented 2 x 7 | PASS |
| P2 self-review | claude-opus-5-5 (agent-1, supervisor) | revisions 1d(a), 1g; 2.1–2.5 + bridge/hub teardown | I-MC1: hub stays a separate branch (`run_listen_hub`), the 1:1 path only gained the cancel/await teardown; legacy waiting path sends nothing until the server's first `Heartbeat` (I-9, `await_vpn_ready_legacy_server`); 1:1 on DIRECT warns instead of tripping, explicit close still ends it; pairing teardown leaves no admin row; every pump of a bridge is aborted AND awaited before `run` returns (no second `boreN` beside an open one, T-new-2/3 netns), and an unorderly drop is covered by `PumpGuard` (red-checked: no-op `Drop` → FAIL) | PASS |
| P1 self-review | claude-opus-5-5 (agent-1, supervisor) | revisions 1b–1e; f7b0745..52e0428 | every reap path returns/breaks so the RAII registration drops (no zombie row); reap checked on the heartbeat tick, never `timeout(recv)` (DEC-VE3); undeclared clients get `None` (DEC-VE2, `undeclared_client_is_never_transport_reaped`); consumer carriers get no reaper (BUG-S2) and provider carriers go through `serve_carrier`; a client never declares `ctrl_heartbeat_ms` without beating (`Client::new`/`Proxy` set both); beats use `beat_once` (P-9); explicit server close always ends the client; transfer one-shot in-progress failure path unchanged (resume state kept) | PASS |

## 8. Technical revisions and deviations

| Revision | Previous decision/step | Approved replacement and reason | Supervisor | Dependents/revalidation |
|----------|------------------------|---------------------------------|------------|------------------------|
| 1j | 3.3 covered the README/SSH guides only | The project ships an unattended OpenSSH client image (`ghcr.io/manprint/bore-ssh-client`, `docker/ssh-entrypoint.sh`) whose DEFAULT `ServerAliveInterval=15`/`ServerAliveCountMax=3` meant a client noticed a dead server only after 45-60 s — the SSH-gateway mode would have stayed down for most of a minute after an IP change even with the gateway reaping in 15 s. Defaults changed to 2 x 7 (the 1f recommendation) in the same unit, documented in `compose.ssh.yml`, gated by `tests/ssh_client_image_test.rs` (Linux only: the image is Linux, and macOS's bash 3.2 rejects empty arrays under `set -u`). Test seam: `BORE_SSH_RUNTIME_DIR` (default `/tmp/bore-ssh`, unchanged) so the test never writes a shared path. | claude-opus-5-5 | 3.3; README already recommends 2 x 7 |
| 1i | 3.1, 3.2 and 3.4 each own their README/doc text | The user-facing text for all three lands in **3.3** as one unit: the three changes share one README section ("Connection liveness and outage recovery") and the SSH guides quote the same 15 s floor the native clients use, so writing them separately would have produced three overlapping edits of the same paragraphs. 3.1/3.2/3.4 committed code + tests only; 3.3 verifies every number against the shipped constants (`CTRL_CLIENT_HEARTBEAT` 2 s, `TRANSPORT_REAP_FLOOR` 15 s, `SSH_KEEPALIVE_INTERVAL` 1 s, `SSH_CTRL_TIMEOUT` 15 s, `WEB_TRANSFER_PEER_WS_PING` 1 s, `WEB_TRANSFER_PEER_TRANSPORT_TIMEOUT` 20 s, `PING_INTERVAL_MS` 5 s, `PONG_DEADLINE_MS` 20 s, `RECONNECT_MIN_MS`/`MAX_MS` 250 ms/5 s, `RESUME_BACKOFF_MAX_MS` 5 s). | claude-opus-5-5 | 3.3; P3 cannot close before 3.3 |
| 1h | plan had no unit for the BROWSER peers of a web-transfer room (3.1 covers the owner CLI only) | New sub-phase **3.4** (phase_04.md): the server sends a WebSocket Ping every 1 s on every browser control session; a session that has answered one is reaped after 20 s of silence (`WEB_TRANSFER_PEER_TRANSPORT_TIMEOUT`), one that never has keeps 60 s (DEC-VE2). The page pings every 5 s and abandons a socket with nothing inbound for 20 s, redialling at once instead of waiting for a `close` event a dead socket fires only after the closing-handshake timeout. Wire unchanged (Ping/Pong are RFC 6455 control frames every browser answers below page script — immune to the one-a-minute throttling of a hidden tab's timers, which is why the server-side deadline does not ride the app-level `ping`). | claude-opus-5-5 | 3.4; P3 gate adds `npm run test:unit` + the dist rebuild; 4.1 web-transfer scenario |
| 1g | 2.3/2.4 assumed `run_bridge_with_ctrl` tears the bridge down on control loss | **P2 gate finding (VPN netns T-new-2/T-new-3 red):** `run_bridge_with_ctrl` `select!`ed the bridge future against the control actor and DROPPED the bridge when the actor ended. Dropping a future does not abort the tasks it spawned, so every pump kept its `Arc<TunDevice>` and the old `boreN` stayed up; the reconnect then created `boreN+1` with the SAME address and the two links fought over the route. Latent before 2.4 (a dead control connection took ~15 min to surface); 2.4's 15 s deadline made it the common path. Fix (`src/vpn.rs`): `bridge::run` takes a `CancellationToken` and leaves through its existing tail, which now aborts AND awaits every pump (stats, relay and direct downlinks, uplinks); `run_bridge_with_ctrl` cancels and AWAITS the bridge; `bridge::PumpGuard` (abort-on-drop of every pump, pruned on track) covers an unorderly drop. Hub: `LivePeerEntry` holds each peer downlink in `AbortOnDrop`, and `run_listen_hub` aborts and awaits its router uplinks and coordinator before returning. | claude-opus-5-5 | P2 closure re-runs `vpn_netns_test.sh`; CLAUDE.md note in 4.3 |
| 1f | D9: `SSH_KEEPALIVE_INTERVAL` 5 s; 3.3 recommends `ServerAliveInterval=5 ServerAliveCountMax=4` | **Keepalive 1 s, reap 15 s** (`ssh_reaper_values_cover_a_probe_plus_the_retransmit_ladder` pins probe + 12.6 s ladder < 15 s, the 1b reasoning applied to the gateway); `SSH_KEEPALIVE_MAX_MISSES` stays derived. Client recommendation **`ServerAliveInterval=2 ServerAliveCountMax=7`** (≈ 14 s, so the client gives up just before the gateway frees its names and the reconnect finds them free). `SessionType none` removed from every example: it is `-N`, which hides the banner and every warning (I-SSH7 corollary). | claude-opus-5-5 | 3.2, 3.3; CLAUDE.md I-SSH3 numbers in 4.3 |
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
| 2.1 | phase_03.md | P1 | DONE | 1 | `HelloVpn`/`ConnectVpn.ctrl_heartbeat_ms` + `VpnReady.ctrl_heartbeat`, all `#[serde(default)]`; clients still declare 0 and the server still answers `false` (wired in 2.2/2.4). `shared::vpn_liveness_fields_default_and_roundtrip` pass (shared 72/0); red-check: dropping the `#[serde(default)]` on `VpnReady.ctrl_heartbeat` → FAIL. vpn_server_test 57/0, vpn_relay_link_test 3/0; fmt + both clippy clean |
| 2.2 | phase_03.md | 2.1 | DONE | 1 | NEW `tests/vpn_liveness_test.rs` 9/0: the 4 contract tests (`waiting_listener_gets_heartbeats_only_when_declared` ≥ 2 beats in 1.5 s vs none for legacy, `waiting_listener_accepts_client_heartbeats_when_declared` incl. a duplicate refused `already in use`, `declared_waiting_listener_is_transport_reaped`, `vpn_ready_carries_ctrl_heartbeat_flag` both directions) + `legacy_waiting_listener_is_never_transport_reaped` (DEC-VE2) + reaps of a paired listener, paired connector, hub listener and hub spoke (the hub outlives its reaped spoke). Red-checks: every `reap_if_due` → `None` = 5 FAIL; `declared = false` = 5 FAIL; listener flag patch removed = flag test FAIL; waiting `Heartbeat` arm returns = 2 FAIL. vpn_server_test 57/0, vpn_relay_link_test 3/0, `--lib vpn` 128/0; fmt + both clippy clean. Note: a declared fake client must beat, or the server reaps it after the floor — that is the contract, not a test flake |
| 2.3 | phase_03.md | 2.1 | DONE | 1 | `VpnPairMsg.cancel` + `HubShared.cancel` (`tokio_util` `CancellationToken`); the connector's drop guard exists before its first await after building the pairing, the listener's right after receiving it, the hub's at `HubShared` creation (held for the handler's life); cancel arms in the paired listener, 1:1 connector and spoke loops. Also: a connector whose listener left between lookup and pairing (`remove` → `None` / `pair_tx.send` → `Err`) now closes instead of idling paired to nothing. vpn_liveness_test 12/0 (3 new: `vpn_pair_teardown_listener_exit_closes_connector`, `vpn_pair_teardown_connector_exit_closes_listener` incl. the id free again, `vpn_hub_exit_closes_spokes`; each also asserts the link stays open while both sides live and no admin row is left). Red-check: each cancel arm replaced by `pending()` → exactly its test FAIL. vpn_server_test 57/0, vpn_relay_link_test 3/0, `--lib vpn` 128/0; fmt + both clippy clean |
| 2.4 | phase_03.md | 2.2 | DONE | 1 | `CTRL_HEARTBEAT_TIMEOUT` (60 s `timeout(recv)`) removed; `CtrlLiveness` (activity below yamux + `client_silence_deadline()` + `ctrl_client_heartbeat()`); `await_vpn_ready` (deadline + beats armed only after the server's first `Heartbeat`, so a legacy server's waiting path sends nothing — I-9); `spawn_ctrl_actor(ctrl, live, beats, on_direct)` with `silence_verdict` (revision 1d(a): 1:1 on DIRECT warns once instead of tripping; an explicit server close still ends it); hub actor extracted to `spawn_hub_ctrl_actor` and trips regardless of path (spokes ride the hub's server); both clients declare `ctrl_heartbeat_ms` and beat only when `VpnReady.ctrl_heartbeat` (or the server's waiting heartbeat) proves the server decodes them; `direct_upgrade_task` publishes `on_direct`. Contract deviation: `await_vpn_ready` returns `Result<Option<ServerMessage>>` (`None` = server closed while waiting, kept distinct from an error so the caller's existing close handling stays). Tests 12 new: `silence_verdict_table`, `ctrl_actor_trips_when_the_server_goes_silent`, `ctrl_actor_survives_while_server_heartbeats`, `ctrl_actor_never_beats_when_not_allowed`, `ctrl_actor_beats_when_allowed`, `ctrl_actor_on_direct_survives_server_silence`, `ctrl_actor_ends_on_explicit_close_even_on_direct`, `await_vpn_ready_legacy_server`, `await_vpn_ready_new_server`, `await_vpn_ready_trips_when_a_heartbeating_server_goes_silent`, `hub_ctrl_actor_trips_when_the_server_goes_silent`, `hub_ctrl_actor_beats_only_when_allowed`. Red-checks (each mutation bites its test): M1 actor deadline off, M2 actor ignores `on_direct` (trips on direct), M3 actor never beats, M4 `await_vpn_ready` watches before the first heartbeat (legacy server tripped), M5 `await_vpn_ready` never beats, M6 `await_vpn_ready` deadline off; H1 hub deadline off, H2 hub never beats, H3 hub always beats. `--lib vpn` 140/0, vpn_liveness_test 12/0, vpn_server_test 57/0, vpn_relay_link_test 3/0; fmt + both clippy clean |
| 2.5 | phase_03.md | 2.2–2.4 | DONE | 1 | README + docs/vpn updated, each claim checked against `CtrlLiveness::new`, `silence_verdict`, `await_vpn_ready`, the 2.3 cancel arms and `reconnect::Backoff`; found and fixed stale VPN backoff tables (1..32 s, the 0.3 cap made it 8 s). fmt + both clippy clean; `cargo doc` warnings unchanged (35 before/after, pre-existing) |
| 3.1 | phase_04.md | P1 | DONE | 1 | G-U3 (owner) on an isolated tree holding exactly the commit: `--lib web_transfer` 207/0, `--lib shared` 73/0, outage_liveness_test 22/0 (new `web_transfer_owner_resumes_after_an_ip_change`), web_transfer_test serial 40/0; fmt + clippy default/all-features clean. Red-checks (6/6 bite): server never beats → `owner_control_sends_heartbeats_only_when_declared` FAIL; server always beats (legacy owner) → same test FAIL; `reap_if_due(&None)` → `owner_control_transport_reaps_silent_declared_owner` FAIL; CLI deadline off → `heartbeat_phase_trips_after_server_beats_stop` FAIL AND the e2e IP-change test FAIL; CLI trips without having seen a server beat → `heartbeat_phase_never_trips_without_server_beats` FAIL. Added `owner control resumed` info line (the counterpart of the `, resuming` warn) |
| 3.2 | phase_04.md | P1 | DONE | 1 | Revision 1f: `SSH_KEEPALIVE_INTERVAL` 1 s, `SSH_CTRL_TIMEOUT` 15 s (= `TRANSPORT_REAP_FLOOR`), `SSH_KEEPALIVE_MAX_MISSES` 14 (derived). Verified in the vendored russh (`server/session.rs`): the keepalive timer is reset by ANY received data and `alive_timeouts` zeroed, so the 15 s counts from the last byte received and a busy session is never reaped. On an isolated tree holding exactly the commit: `--lib sshgw` 63/0 (new `ssh_reaper_values_cover_a_probe_plus_the_retransmit_ladder`; red-check: a 5 s interval → FAIL), ssh_gateway_test serial 43/0 (T-SSH-KEEP1 idle 30 s = 2× the deadline on keepalives alone) + spike 5/0; fmt + clippy default/all-features clean. Netns T-SSH-N1 now bounds the half-open reap at 20 s (was 75 s) — the old 60 s reaper cannot pass it; run in P3 |
| 3.3 | phase_04.md | 3.1, 3.2, 3.4 (rev 1i) | DONE | 1 | README ("Connection liveness and outage recovery": browser rooms, browser tabs, SSH gateway sessions; SSH examples + troubleshooting row), `docs/ssh-gateway/{SSH_GATEWAY.md,README-SSH-GATEWAY.md,README-JUMP-HOST.md}`, `docs/transfer/WEB_TRANSFER_PROTOCOL.md` §8/§12: every number checked against the shipped constant (list in revision 1i); no `ServerAliveInterval` other than 2 left in any user-facing doc (perf harnesses and the KEEP1 test keep longer intervals on purpose). Revision 1j: the `bore-ssh-client` image defaults 15 x 3 -> **2 x 7** (`docker/ssh-entrypoint.sh`, `compose.ssh.yml`), gated by NEW `tests/ssh_client_image_test.rs` 2/0 (runs the real entrypoint with an `autossh` shim; red-checks: 15 x 3 -> FAIL on the defaults, 5 x 7 with that assert neutralised -> FAIL on the 15 s arithmetic); fmt + clippy clean |
| 3.4 | phase_04.md (rev 1h) | P1 | DONE | 1 | On an isolated tree holding exactly the commit: fmt + clippy default/all-features clean; `--lib web_transfer` 208/0 (new `peer_session_expiry_is_gated_on_an_answered_ping`); web_transfer_deploy_test 3/0, web_transfer_fuzz 4/0; web_transfer_test serial 41/0 (1 ignored = bench; new `a_browser_that_stops_answering_pings_leaves_on_the_transport_deadline`); `npm run test:unit` 227/0; Playwright chromium (see §7). Rust red-checks 4/4 bite: R1 server never pings (`WEB_TRANSFER_PEER_WS_PING` 3600 s) → e2e FAIL; R2 Pong never recorded → e2e FAIL (B stays on the 60 s window); R3 expiry not gated on an answered Ping → unit FAIL; R4 Pong does not refresh `last_recv` → e2e FAIL (the answering peer A is reaped). JS red-checks J1–J6 all FAIL fast (no hang: afterEach stops every session, each async test has a 5 s timeout). Found by the gate and fixed in the same unit: (a) the first Ping now waits one period (`interval_at`) — an immediate tick could race the welcome/snapshot frames and make `pong_seen` depend on scheduling; (b) the test helpers `next_text`/`close_code`/`next_msg`/`answer_one_ping_then_go_silent` bound the WHOLE call by a deadline — a per-frame timeout longer than the 1 s Ping never expires (hung the new test); (c) `t_web_peers` quiet reap: S is no longer READ while waiting (reading answers Pings, and a session that answers is alive by design) and A/B ping every 10 s instead of sleeping 20 s against the 20 s transport deadline |
| 4.1 | phase_05.md | P2, P3 | TODO | 1 | — |
| 4.2 | phase_05.md | 4.1 | TODO | 1 | — |
| 4.3 | phase_05.md | 4.2 | TODO | 1 | — |

### Phases
| ID | File | Closure unit | Status | Review / commit reference |
|----|------|--------------|--------|---------------------------|
| 0 | phase_01.md | P0 | DONE | self-review: no callers of terminate yet; heartbeat 5 s only speeds beats; PEV-Unit P0 |
| 1 | phase_02.md | P1 | DONE | G-P1 + self-review (§7); PEV-Unit P1 |
| 2 | phase_03.md | P2 | DONE | G-PHASE P2 + G-NETNS-VPN + self-review (§7); revision 1g; PEV-Unit P2 |
| 3 | phase_04.md | P3 | DONE | G-P3 + G-NETNS-SSH + self-review (§7); revisions 1f, 1h, 1i, 1j; PEV-Unit P3 |
| 4 | phase_05.md | P4 | TODO | — |

### Tests
| ID/name | Owning unit | Gate | Status | Evidence |
|---------|-------------|------|--------|----------|
| mux activity/terminate (5) | 0.1 | G-U01 | PASS | 15 passed; red-check terminate arm → timeout |
| liveness tables | 0.2 | G-U02 | PASS | 6 tests incl. LivenessTicker |
| backoff cap | 0.3 | G-U03 | PASS | 8 passed |
| serde defaults | 1.1/2.1/3.1 | G-U11 | PASS (1.1, 2.1) | 7 new in shared::tests |
| outage_liveness_test | 1.2–1.5 | G-U12 | PASS (1.7: 21/0) | server reapers vhost/public/secret provider/consumer + I-5; client trips public/vhost/secret provider+consumer, flick survival, carrier termination, consumer direct survival, provider linger, sticky public port |
| vpn liveness/teardown | 2.2–2.4 | G-U2 | PASS (2.4: lib vpn 140/0, vpn_liveness_test 12/0) | vpn_liveness_test server side + pairing teardown; `vpn::tests` ctrl actor / `await_vpn_ready` / `silence_verdict`; `vpn::hub::tests` hub ctrl actor |
| owner + sshgw + browser + ssh image | 3.1/3.2/3.3/3.4 | G-U3 | PASS | 3.1: outage_liveness_test 22/0 + web lib; 3.2: `--lib sshgw` 63/0 + ssh serial 43/0; 3.4: web lib 208/0 + web serial 41/0 + npm unit 227/0 |
| T-OUT-* | 4.1 | G-NETNS-OUT | TODO | — |

### Documentation
| Document/sections | Owning unit | Status | Evidence |
|-------------------|-------------|--------|----------|
| README auto-reconnect backoff | 0.4 | DONE | README §Automatic reconnection |
| README liveness section | 1.6 | DONE | README "Connection liveness and outage recovery" + transfer paragraph (1.7) |
| README VPN | 2.5 | DONE | README liveness section "VPN links" paragraph + VPN Cleanup paragraph; docs/vpn/VPN.md + VPN_USER_FULL_GUIDE.md (liveness, pairing teardown, direct tolerance; stale 32 s backoff → 8 s); superseded notes in the two historical VPN assessments; CLAUDE.md B5 line; stale `secret.rs` doc comments (60 s VPN parity, 5 s heartbeat) |
| README web-transfer + SSH, SSH_GATEWAY.md | 3.3 | DONE | README liveness section (owner, browser tabs, SSH sessions) + SSH examples/troubleshooting; SSH_GATEWAY.md, README-SSH-GATEWAY.md, README-JUMP-HOST.md, WEB_TRANSFER_PROTOCOL.md; `compose.ssh.yml` stability knobs |
| CLAUDE.md invariant + README final | 4.3 | TODO | — |

### Audits
| Report | Verdict | Current unresolved findings | Evidence |
|--------|---------|-----------------------------|----------|
| none | — | — | — |

## 12. Suspended implementation

Snapshot state: none.
