# Task ledger — 001 Web Transfer

Out-of-plan changes that are not sub-phases, one row per task. IDs are never
reused. A row exists so a later `verify` can tell an intentional diff from an
unexplained one; `STATE.md` §4 carries the one-line version.

| ID | Date | Intent | Files | Verification | Commit |
|----|------|--------|-------|--------------|--------|
| T-A001 | 2026-09-15 | Bring three user directions into the plan itself instead of leaving them in a conversation: performance is a product requirement (D21), the DIRECT WebRTC/UDP path is the main road and the rest of bore's measured optimizations must be carried onto it (D22), and the UI is part of the product — ordered, intuitive, explicit about `direct` vs `relay`, with drag & drop (D23). Each one is owned by a new sub-phase with its own tests in the harness: **3.9** (`T-WEB-PERF`), **4.6** (`T-WEB-PERF-DIRECT`), **5.6** (`T-WEB-DND`, `T-WEB-PATH-UI`, `T-WEB-UI`). | docs/plans/001_plan-WebTransfer/{overview.md,phase_04.md,phase_05.md,phase_06.md,STATE.md,tasks.md} | plan-only change, no production code: the affected phase gates are the ones the new sub-phases declare; `STATE.md` §11 lists the five new test IDs as `TODO` | uncommitted |
