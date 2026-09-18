# Web Transfer multipeer — Audit register

> Findings never disappear: they move to `FIXED`, `ACCEPTED`, or `OBSOLETE`,
> always with evidence. Statuses here and in the reports must agree.

## Reports

| # | File | Date | Verdict | Blocker | Major | Minor | Auditor |
|---|------|------|---------|---------|-------|-------|---------|
| V001 | [verify_001_2026-09-15.md](verify_001_2026-09-15.md) | 2026-09-15 | `PASS WITH FINDINGS` | 0 | 5 | 1 | `agent-1:Codex-GPT-5` |
| V002 | [verify_002_2026-09-15.md](verify_002_2026-09-15.md) | 2026-09-15 | `FAIL` | 1 | 5 | 2 | `agent-1:Claude-Opus-5` |
| V003 | [verify_003_2026-09-16.md](verify_003_2026-09-16.md) | 2026-09-16 | `PASS WITH FINDINGS` | 0 | 4 | 1 | `agent-1:Codex-GPT-5` |

## Findings

| ID | Severity | Category | Title | Status | Closed by | Evidence |
|----|----------|----------|-------|--------|-----------|----------|
| V001-F01 | `MAJOR` | divergent | Identical offer replay loses idempotency at saturated caps | `FIXED` | `execute verify V001-C1` 2026-09-15 | `identical_publish_is_idempotent_at_saturated_caps` passes; fmt/clippy/lib 735 green |
| V001-F02 | `MAJOR` | divergent | Room revisions wrap instead of failing checked | `FIXED` | `execute verify V001-C2` + C5 review 2026-09-15 | overflow rollback/RAII lifecycle test proves no wrap/leak plus destroy/cancel/registry removal; final matrix green |
| V001-F03 | `MAJOR` | rule-violation | Malformed bracketed Host authorities can match | `FIXED` | `execute verify V001-C3` 2026-09-15 | IPv6 exact-authority matrix passes; fmt/clippy/lib 736 green |
| V001-F04 | `MAJOR` | divergent | Bracketed STUN targets bypass validation | `FIXED` | `execute verify V001-C4` 2026-09-15 | custom/parser STUN matrix 16 pass; fmt/clippy/lib 736 green |
| V001-F05 | `MAJOR` | failing-gate | Full-regression gate ignores required serial execution | `FIXED` | `execute verify V001-C5` 2026-09-15 | exact aggregate command with `--test-threads=1` passes; coverage retained |
| V001-F06 | `MINOR` | stale-state | Completed artifacts and deviation rows were stale | `FIXED` | `verify V001` 2026-09-15 | `STATE.md` §8/§11 re-synced |
| V002-F01 | `MAJOR` | stale-state | 3.4 implemented without being opened, gated or closed | `FIXED` | `execute verify V002-C6` 2026-09-15 | `STATE.md` §1 → 3.5, §4 rows 27–33, §5 file rows, §6 `none`, §7 final matrix, §8.58–62, §11 board with `T-WEB-DOWNLOAD-RELAY` `DONE` |
| V002-F02 | `BLOCKER` | failing-gate | `T-WEB-DOWNLOAD-RELAY` fails on WebKit (ephemeral-context OPFS) | `FIXED` | `execute verify V002-C1` 2026-09-15 | persistent profiles for every download leg; `npm run test:e2e` 54 passed / 0 failed on chromium+firefox+webkit |
| V002-F03 | `MAJOR` | efficiency | OPFS sink is quadratic (`keepExistingData` copies the output per chunk) | `FIXED` | `execute verify V002-C2` 2026-09-15 | part-file staging + Blob composition; 128 MiB 8.43 -> 49.61 MiB/s, 32 MiB 22.81 -> 35.63 MiB/s, no quadratic term left |
| V002-F04 | `MAJOR` | divergent | Receiver cannot consume a stream that skips resumed chunks | `FIXED` | `execute verify V002-C3` 2026-09-15 | `receiver_places_chunks_by_plan_when_the_source_skips_verified_ranges` (red-checked: reverting the plan index fails it) |
| V002-F05 | `MAJOR` | untested | Three named unit tests missing or silently renamed | `FIXED` | `execute verify V002-C4` 2026-09-15 | `resume_rehashes_and_drops_corrupt_or_truncated_chunks`, `relay_rejects_oversize_text_binary_compression_and_recipient_binary`, attach test renamed and extended |
| V002-F06 | `MINOR` | divergent | `DIRECT_FAILED` branch in the receiver is unreachable | `FIXED` | `execute verify V002-C5` 2026-09-15 | `direct_failed_notice_fails_the_matching_transfer` |
| V002-F07 | `MINOR` | efficiency | Resume record re-read and database re-opened per chunk | `FIXED` | `execute verify V002-C5` 2026-09-15 | `storage_opens_the_database_once_for_many_operations`; record carried in memory |
| V002-F08 | `MAJOR` | divergent | File-picker save fetches a `blob:` URL the page CSP forbids | `FIXED` | `execute verify V002-C2` 2026-09-15 | picker path writes the staged Blob; no code fetches an object URL |
| V003-F01 | `MAJOR` | untested | Direct goodput is bimodal; no two-host 400 MB acceptance gate | `OPEN` (characterised; field run outstanding) | — | `execute verify V003-C1` 2026-09-17 delivered the instrumentation, the falsification and the gate, and the field run is what is left. MEASURED: the collapse is ONE `waitLow` of 1.3–1.7 s, independent of queue depth (peak 4.5 MB vs 1.17 MB), of the selected pair (`host/host` always) and of `packetsDiscardedOnSend` (1209 on a fast repetition, 38 on the slowest), and **absent on firefox over the same loopback** (0.882x, longest wait 155 ms) — which falsifies the path-loss/RTO claim the document carried; it is removed. `T-WEB-PERF-LAN` (`scripts/perf/web_transfer_lan.sh` + `lan.perf.mjs`) is proved end to end on both arms; the two-host 400 MB run on the reporter's network cannot be executed from a single-host machine and keeps this finding open |
| V003-F02 | `MAJOR` | divergent | DataChannel drain timeout resolves while the queue is still high | `FIXED` | `execute verify V003-C2` 2026-09-17 | the deadline now fails the attempt with the fixed reason `timeout` and REJECTS the wait, so the sender stops reading and the relay attempt resumes from the verified ranges; `stalled_channel_never_queues_after_drain_deadline` red-checked (a deadline that resolves unconditionally fails it) |
| V003-F03 | `MAJOR` | untested | No selected ICE pair or failure trace for a direct-to-relay transition | `FIXED` | `execute verify V003-C3` 2026-09-17 | bounded, allow-list-redacted per-attempt trace (ICE/channel timeline, selected pair TYPE and `getStats()` counters, drain accounting) + `Copia diagnostica percorso` + server `info!` with fixed reason and opaque ids; `T-WEB-DIRECT-DIAG` green on chromium/firefox/webkit, 4 units in `diagnostics.test.mjs`, `t_web_log_privacy` extended — red-checked ×4. The Android FIELD RUN is measurement, not implementation, and stays with F01/C1 |
| V003-F04 | `MINOR` | divergent | README LAN/STUN and speed troubleshooting is inaccurate | `FIXED` | `execute verify V003-C4` 2026-09-17 | the LAN row now separates host reachability from STUN reachability (on one LAN the browsers pair on host candidates and `--web-transfer-no-stun` is not what forces the relay — client isolation, a host firewall or different subnets are) and sends the reader to `Copia diagnostica percorso` first; the speed row is split into `relay` (default already 100 MiB/s, both legs cross the server) and `diretto` (the measured 1.3–1.7 s freeze, read off `drain_longest`). `t_web_readme` extended with seven needles on the new wording and two on the old wording that must be GONE — red-checked |
| V003-F05 | `MAJOR` | divergent | Remote ICE end-of-candidates marker is discarded | `FIXED` | `execute verify V003-C5` 2026-09-17 | `pc.addIceCandidate(null)` is called exactly once, after the queued candidates, with one bounded pending slot that does not consume the 128 budget; `remote_end_of_candidates_is_applied_after_queued_candidates` red-checked, plus a host-only ICE case on chromium/firefox/webkit reading the delivery out of the C3 trace |

Status values: `OPEN` · `FIXED` · `ACCEPTED` (user decided to live with it, with
the reason) · `OBSOLETE` (no longer applies, with the reason)

## Open blockers

- No blocker. V003: F02, F03 and F05 are `FIXED`; F01 is characterised with its gate
  delivered and stays open on the two-host field run alone; F04 is `FIXED`. V001/V002 are fixed.
