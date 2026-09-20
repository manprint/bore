# Transfer Link verification register

| Audit | Date | Scope | Verdict | Findings | Report |
|-------|------|-------|---------|----------|--------|
| V001 | 2026-09-20 | phases 0–4, `7ea6b9f..1446f2a` | FAIL pending C10–C12 | 2 BLOCKER, 8 MAJOR, 0 MINOR open after C09 | [verify_001_2026-09-20.md](verify_001_2026-09-20.md) |

## Findings

| ID | Severity | Category | Title | Status | Evidence |
|----|----------|----------|-------|--------|----------|
| V001-F01 | BLOCKER | divergent | Source completion wins before the HTTP connection outcome | FIXED | C01 narrow HTTP unit and Link/no-default suites |
| V001-F02 | BLOCKER | divergent | Body drop can abort the owner of exec process-group cleanup | FIXED | C02 privileged GET-disconnect gate; `ProducerTaskOwner` join/cancel |
| V001-F03 | MAJOR | divergent | A failed stdin/exec transfer does not terminate the CLI | FIXED | C03 watch signal and real GET/FIFO cancellation e2e |
| V001-F04 | MAJOR | resource-bound | Scoped task bookkeeping grows for the lifetime of the link | FIXED | C04 RAII abort-handle reclamation and repeated-task unit |
| V001-F05 | MAJOR | bounds | Manifest caps are applied after unbounded directory collection | FIXED | C05 manifest budget unit tests and Link suite |
| V001-F06 | MAJOR | divergent | Transient TLS handshake failures are classified as permanent verification failures | FIXED | C06 typed transport/CLI classification; Link 22/22 with and without default features |
| V001-F07 | MAJOR | rule-violation | Logs disclose the bearer components of the public URL | FIXED | C07 scoped redaction, session correlation id, `-v`/`-vv` e2e scan |
| V001-F08 | MAJOR | missing/stale-state | Phase 4.1 was closed without its mandatory transport and cleanup gates | OPEN | C10 |
| V001-F09 | MAJOR | missing/untested | Phase 4.2 was closed without throughput, 15 GiB or no-spool evidence | OPEN | C11 |
| V001-F10 | MAJOR | resource-bound | HTTP supervisor can detach its task on shutdown timeout | FIXED | C04 abort-and-join unit and retained JoinHandle path |
| V001-F11 | MINOR | divergent | Generated label omits the approved `transfer-` prefix | FIXED | C08 prefix + suffix test and basic e2e |
| V001-F12 | MINOR | divergent | `--carriers 0` auto mode is rejected by the Link CLI | FIXED | C08 parser accepts 0 and retains adaptive client path |
| V001-F13 | MINOR | missing/divergent | Progress is data-triggered and omits the active count | FIXED | C08 timer/select body and no-data progress unit |
| V001-F14 | MAJOR | invalid-gate | Docker acceptance may test a stale binary | FIXED | C09 forced locked rebuild + aged valid static-artifact oracle |
| V001-F15 | MINOR | docs | README contains trust and transfer-outcome inaccuracies | FIXED | C08 README trust/outcome/progress corrections |

## Open blockers

- none
