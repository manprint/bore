# Web Transfer multipeer — Audit register

> Findings never disappear: they move to `FIXED`, `ACCEPTED`, or `OBSOLETE`,
> always with evidence. Statuses here and in the reports must agree.

## Reports

| # | File | Date | Verdict | Blocker | Major | Minor | Auditor |
|---|------|------|---------|---------|-------|-------|---------|
| V001 | [verify_001_2026-09-15.md](verify_001_2026-09-15.md) | 2026-09-15 | `PASS WITH FINDINGS` | 0 | 5 | 1 | `agent-1:Codex-GPT-5` |

## Findings

| ID | Severity | Category | Title | Status | Closed by | Evidence |
|----|----------|----------|-------|--------|-----------|----------|
| V001-F01 | `MAJOR` | divergent | Identical offer replay loses idempotency at saturated caps | `FIXED` | `execute verify V001-C1` 2026-09-15 | `identical_publish_is_idempotent_at_saturated_caps` passes; fmt/clippy/lib 735 green |
| V001-F02 | `MAJOR` | divergent | Room revisions wrap instead of failing checked | `FIXED` | `execute verify V001-C2` + C5 review 2026-09-15 | overflow rollback/RAII lifecycle test proves no wrap/leak plus destroy/cancel/registry removal; final matrix green |
| V001-F03 | `MAJOR` | rule-violation | Malformed bracketed Host authorities can match | `FIXED` | `execute verify V001-C3` 2026-09-15 | IPv6 exact-authority matrix passes; fmt/clippy/lib 736 green |
| V001-F04 | `MAJOR` | divergent | Bracketed STUN targets bypass validation | `FIXED` | `execute verify V001-C4` 2026-09-15 | custom/parser STUN matrix 16 pass; fmt/clippy/lib 736 green |
| V001-F05 | `MAJOR` | failing-gate | Full-regression gate ignores required serial execution | `FIXED` | `execute verify V001-C5` 2026-09-15 | exact aggregate command with `--test-threads=1` passes; coverage retained |
| V001-F06 | `MINOR` | stale-state | Completed artifacts and deviation rows were stale | `FIXED` | `verify V001` 2026-09-15 | `STATE.md` §8/§11 re-synced |

Status values: `OPEN` · `FIXED` · `ACCEPTED` (user decided to live with it, with
the reason) · `OBSOLETE` (no longer applies, with the reason)

## Open blockers

- none
