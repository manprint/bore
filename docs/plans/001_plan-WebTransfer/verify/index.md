# Web Transfer multipeer — Audit register

> Findings never disappear: they move to `FIXED`, `ACCEPTED`, or `OBSOLETE`,
> always with evidence. Statuses here and in the reports must agree.

## Reports

| # | File | Date | Verdict | Blocker | Major | Minor | Auditor |
|---|------|------|---------|---------|-------|-------|---------|
| V001 | [verify_001_2026-09-15.md](verify_001_2026-09-15.md) | 2026-09-15 | `PASS WITH FINDINGS` | 0 | 5 | 1 | `agent-1:Codex-GPT-5` |
| V002 | [verify_002_2026-09-15.md](verify_002_2026-09-15.md) | 2026-09-15 | `FAIL` | 1 | 5 | 2 | `agent-1:Claude-Opus-5` |

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

Status values: `OPEN` · `FIXED` · `ACCEPTED` (user decided to live with it, with
the reason) · `OBSOLETE` (no longer applies, with the reason)

## Open blockers

- none — every `V002` finding is `FIXED`; the register and `STATE.md` agree.
