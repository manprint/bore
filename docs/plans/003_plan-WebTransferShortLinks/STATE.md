# Web Transfer Short Links — Implementation State

Last updated: 2026-09-22 (sub-phase 2.1 closed; sub-phase 2.2 opened)

## 0. Resume protocol

Read this file first, then `overview.md`, then the phase file named by **Next action**.
Implement exactly one sub-phase at a time. After each sub-phase:

1. run every gate listed in that sub-phase;
2. record the commands and outcomes here;
3. update the work ledger and progress board;
4. leave the tree in a zero-regression state;
5. move **Next action** to the following sub-phase.

Do not create additional state files. Do not silently weaken a gate. If a gate cannot run,
record the exact reason and leave the sub-phase incomplete. Do not make commits unless the
operator explicitly requests them. This execution has explicit commit authorization; the setting is recorded in §3.

All implementation and verification units are assigned to `agent:gpt-5.6-luna` as requested.

## 1. Current unit

| Field | Value |
|---|---|
| Mode | Execute; WIP commits enabled |
| Current sub-phase | `2.2` |
| Status | `OPEN` |
| Intent | Chrome/Edge/Brave branded smoke |
| Phase file | `phase_03.md` |
| Next action | Execute sub-phase `2.2` in `phase_03.md` |
| Assigned model | `agent:gpt-5.6-luna` |
| Baseline branch | `dev` |
| Baseline commit | `ad7d0f47d497` |
| Baseline worktree | Clean before plan creation; only this plan folder is expected to be new |

## 2. Frozen feature contract

- Canonical room URL: `https://HOST/transfer/#TOKEN`.
- `TOKEN` is exactly 22 unpadded Base64URL characters encoding exactly 16 random bytes.
- The fragment is persistent. Never call `history.replaceState` or otherwise remove/rewrite it.
- No server-side shortener, seed database, seed persistence, or lookup service.
- Derive `RoomId`, `MemberToken`, and `RoomKey` independently with HKDF-SHA256 and the
  exact salt/info labels in `overview.md`.
- Truncate only `RoomId`, to 16 bytes. Keep the other two outputs at 32 bytes.
- Owner capability/token remains independently random and is not derived from the URL seed.
- Browser derivation uses native WebCrypto HKDF. Missing/failed support is a clear pre-WebSocket
  error; there is no JavaScript crypto fallback.
- Native owner protocol hard-cuts to version 2 and sends the client-derived room ID. No old/new
  fallback is required.
- Legacy `32hex + 64hex + 64hex` fragments are rejected. There are no old links to preserve.
- Canonical decoding is strict: reject padding, classic Base64 symbols, whitespace, percent
  escapes, wrong decoded length, and alternate encodings caused by non-zero trailing pad bits.
- The deterministic fixture and all expected outputs in `overview.md` are normative.
- Browser support means current ordinary Chromium, Firefox, and WebKit automation on every
  relevant change, plus periodic/manual real Chrome, Edge, and Brave smoke coverage.

Any implementation choice that contradicts one of these bullets is a plan deviation and must
be stopped, documented below, and escalated to the operator before proceeding.

## 3. Execution order and ownership

Commit mode: WIP commits ON — explicit operator request on 2026-09-21; commit every closed sub-phase and phase.

| Order | Sub-phase | Owner | Purpose | Status |
|---:|---|---|---|---|
| 1 | `0.1` | `agent:gpt-5.6-luna` | Rust seed codec, HKDF derivation, fixture | DONE |
| 2 | `0.2` | `agent:gpt-5.6-luna` | Browser codec/KDF mirror and failure behavior | DONE |
| 3 | `0.3` | `agent:gpt-5.6-luna` | Cross-language contract and red-check audit | DONE |
| 4 | `0.4` | `agent:gpt-5.6-luna` | Phase 0 README synchronization | DONE |
| 5 | `1.1` | `agent:gpt-5.6-luna` | Native owner protocol v2 and client-selected room ID | DONE |
| 6 | `1.2` | `agent:gpt-5.6-luna` | CLI/browser switch to persistent short fragment | DONE |
| 7 | `1.3` | `agent:gpt-5.6-luna` | Negative, legacy-removal, and secrecy tests | DONE |
| 8 | `1.4` | `agent:gpt-5.6-luna` | Protocol documentation | DONE |
| 9 | `1.5` | `agent:gpt-5.6-luna` | Rebuild assets/package and run regressions | DONE |
| 10 | `1.6` | `agent:gpt-5.6-luna` | Phase 1 README synchronization | DONE |
| 11 | `2.1` | `agent:gpt-5.6-luna` | Chromium/Firefox/WebKit compatibility gates | DONE |
| 12 | `2.2` | `agent:gpt-5.6-luna` | Chrome/Edge/Brave branded smoke coverage | OPEN |
| 13 | `2.3` | `agent:gpt-5.6-luna` | Fragment persistence and leak audit | TODO |
| 14 | `2.4` | `agent:gpt-5.6-luna` | Full regression and requirement self-review | TODO |
| 15 | `2.5` | `agent:gpt-5.6-luna` | Final README synchronization | TODO |

Never overlap these units. Later units depend on the frozen outputs and protocol choices made by
earlier units.

## 4. Work ledger

Append one row after every completed or attempted sub-phase. Use exact commands, not summaries.

| Date/time | Sub-phase | Files changed | Gates run | Result | Notes | Commit |
|---|---|---|---|---|---|---|
| 2026-09-21 | Planning | `docs/plans/003_plan-WebTransferShortLinks/*` | Plan structure checks only | Plan authored | No production code or tests changed/run | — |
| 2026-09-21 | `0.1` | `Cargo.toml`, `Cargo.lock`, `src/web_transfer.rs`, `src/web_transfer_protocol.rs`, `tests/fixtures/web_transfer/link_v1.json`, `STATE.md` | `cargo fmt --all -- --check`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo test --locked --all-features --lib web_transfer`; `git diff --check` | PASS | 189 web_transfer tests passed | `09ed559` |
| 2026-09-21 | `0.2` | `web/transfer/src/crypto.js`, `web/transfer/src/secrets.js`, `web/transfer/tests/unit/crypto.test.mjs`, `web/transfer/dist/app.js`, `web/transfer/dist/offer-worker.js`, `STATE.md` | `npm run check --prefix web/transfer`; `rg -n '\\bBuffer\\b' web/transfer/src web/transfer/tests/unit/crypto.test.mjs web/transfer/dist`; `git diff --check` | PASS | 217 browser unit tests passed; bundle rebuild reproduced tracked assets; no Buffer references; offer-worker changed because the normal build emitted the shared browser update | `240dc5e` |
| 2026-09-21 | `0.3` | `web/transfer/tests/unit/crypto.test.mjs`, `STATE.md` | `node --test tests/unit/crypto.test.mjs`; independent `node:crypto.hkdfSync` oracle; controlled label/seed/truncation red-checks; label duplicate audit; no test stdout/stderr audit; `cargo fmt --all -- --check`; `cargo clippy --locked --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo test --locked --all-features --lib web_transfer`; `npm run check --prefix web/transfer`; `git diff --check`; Cargo.lock diff inspection | PASS | 16 crypto tests; 218 browser tests; 189 Rust tests; all red-checks failed as expected and tree was restored/untouched | `6b6c002` |
| 2026-09-21 | `0.4` | `README.md` (read-only audit), `docs/transfer/WEB_TRANSFER_PROTOCOL.md` (read-only audit), `STATE.md` | `rg -n -i 'web.?transfer|browser.?to.?browser|room|owner' README.md`; protocol section audit; `npx playwright test tests/e2e/readme.spec.mjs --project=chromium --project=firefox --project=webkit`; inherited phase gates G-FMT/G-LINT/G-BUILD/G-RUST-UNIT/G-JS/G-DIFF | PASS | README remains truthful for the active long URL; no premature short-link promise; README flow 3/3 engines passed; no README/protocol edit needed | `a249929` |
| 2026-09-21 | `1.1` | `src/shared.rs`, `src/web_transfer.rs`, `src/web_transfer_cli.rs`, `tests/web_transfer_test.rs`, `STATE.md` | `cargo fmt --all -- --check`; `cargo clippy --locked --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo test --locked --all-features --lib web_transfer -- --test-threads=1`; `cargo test --locked --all-features --test web_transfer_test -- --test-threads=1`; `npm run check --prefix web/transfer`; `git diff --check` | PASS | 194 Rust unit tests; web-transfer integration 40 passed, 1 ignored benchmark; 218 browser tests; v2 owner wire, exact requested IDs, duplicate protection, old/missing-ID rejection, derived reconnect invariants and response mismatch secrecy are covered; URL remains intentionally long for 1.2 cutover | `758e1f2` |
| 2026-09-21 | `1.2` | `src/web_transfer_cli.rs`, `src/web_transfer_http.rs`, `tests/web_transfer_test.rs`, `examples/web_transfer_e2e_owner.rs`, `web/transfer/src/{main.js,secrets.js}`, `web/transfer/dist/app.js`, `web/transfer/tests/{unit,e2e}`, `STATE.md` | `cargo fmt --all -- --check`; `cargo clippy --locked --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo build --locked --all-features --example web_transfer_e2e_owner`; `cargo test --locked --all-features --lib web_transfer -- --test-threads=1`; `cargo test --locked --all-features --test web_transfer_test -- --test-threads=1`; `npm run check --prefix web/transfer`; `npm run test:e2e -- tests/e2e/room.spec.mjs`; `git diff --check` | PASS | 194 Rust unit tests; web-transfer integration 40 passed, 1 ignored benchmark; 216 browser unit tests; room E2E 27 passed across Chromium/Firefox/WebKit; HTTP shell 14 unit tests plus `t_web_http`; canonical short fragment persists, storage stays empty, copy/reload/two-peer flow passes, and bare `/transfer/` serves the shell | `15477f3` |
| 2026-09-22 | `1.3` | `src/web_transfer.rs`, `src/web_transfer_cli.rs`, `web/transfer/tests/unit/secrets.test.mjs`, `web/transfer/tests/e2e/{helpers.mjs,room.spec.mjs,security.spec.mjs}`, `STATE.md` | `cargo fmt --all -- --check`; `cargo clippy --locked --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo test --locked --all-features --lib web_transfer -- --test-threads=1`; `cargo test --locked --all-features --test web_transfer_test -- --test-threads=1`; `npm run check --prefix web/transfer`; `npm run test:e2e -- tests/e2e/room.spec.mjs`; `npm run test:e2e -- tests/e2e/security.spec.mjs`; `git diff --check` | PASS | 195 Rust unit tests; web-transfer integration 40 passed, 1 ignored benchmark; 216 browser unit tests; room E2E 30/30 and security E2E 15/15 across Chromium/Firefox/WebKit; collision returns generic error and preserves the existing room; three red-checks (legacy parser, replaceState, early startSession) failed as expected and were restored | `d05346d` |
| 2026-09-22 | `1.4` | `docs/transfer/WEB_TRANSFER_PROTOCOL.md`, `STATE.md` (fixture read-only verification) | `node --test tests/unit/crypto.test.mjs` from `web/transfer`; `cargo test --locked --all-features --lib web_transfer_protocol -- --test-threads=1`; deterministic doc/fixture literal audit; `git diff --check` | PASS | 16 browser crypto tests; 35 Rust protocol tests; canonical short URL, strict rejection, three HKDF labels/outputs, owner v2 separation, 128-bit security, collision behavior, key hygiene and fragment threat model verified against `link_v1.json`; no legacy session-storage/scrub wording remains | `4d25de9` |
| 2026-09-22 | `1.5` | `scripts/web_transfer_e2e.sh`, `web/transfer/playwright.config.mjs`, `web/transfer/tests/e2e/{catalog.spec.mjs,download.spec.mjs,sender-relay.spec.mjs}`, `STATE.md` | `npm run build --prefix web/transfer`; `cargo fmt --all -- --check`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo build --locked --all-features`; `cargo test --locked --all-features --lib web_transfer -- --test-threads=1`; `cargo test --locked --all-features --test web_transfer_test -- --test-threads=1`; `npm run check --prefix web/transfer`; `npm run test:e2e --prefix web/transfer`; `bash scripts/web_transfer_package_test.sh`; `bash scripts/web_transfer_container_test.sh`; `git diff --check` | PASS | 195 Rust unit tests; web-transfer integration 40 passed, 1 ignored benchmark; 216 browser checks; full Playwright matrix 172 passed, 23 skipped; package and container gates passed; browser harness was started but deferred on operator instruction so the repository-level full harness runs once at final verification; targeted browser runs remained green | `10130b0` |
| 2026-09-22 | `1.6` | `README.md`, `STATE.md` | README legacy-format audit; `npx playwright test tests/e2e/readme.spec.mjs tests/e2e/readme-direct.spec.mjs --project=chromium --project=firefox --project=webkit --workers=1 --reporter=line`; `git diff --check` | PASS | Six README flow tests passed across Chromium, Firefox and WebKit; canonical 22-character short-link example, persistent fragment, copy/refresh behavior, exposure warning and coordinated hard cutover are documented; no active legacy URL/storage wording remains | `a0fc672` |
| 2026-09-22 | `2.1` | `web/transfer/tests/e2e/room.spec.mjs`, `web/transfer/tests/unit/ci.test.mjs`, `STATE.md` | `node --test tests/unit/ci.test.mjs`; `npm run check --prefix web/transfer`; `npx playwright test tests/e2e/room.spec.mjs --project=chromium --project=firefox --project=webkit --workers=1 --reporter=line`; `git diff --check` | PASS | CI-contract 2/2; JavaScript unit/build check 216/216; room matrix 30/30 on Chromium, Firefox and WebKit; exact short hash, reload/copy, no-storage, no-HKDF/no-WebSocket and shell/WS URL boundaries verified | `pending` |

## 5. Decision and deviation ledger

The authoritative rationale is in `overview.md` decisions D1–D10. This table tracks execution-time
changes only.

| ID | Status | Decision or deviation | Required action |
|---|---|---|---|
| D1–D10 | FROZEN | Use the exact contract and compatibility policy from `overview.md` | Implement without reinterpretation |
| E1 | RECORDED | Operator explicitly authorized commits at every closed sub-phase and phase, overriding the plan's planning-time commit-off note | Stage only touched files and plan state; record each commit SHA in §4 |

If a deviation becomes necessary, add a new row with: observed evidence, affected tests/files,
security and compatibility impact, operator decision, and the phase updates required. Never edit
the original decision away.

## 6. Gate ledger

`NOT RUN` is intentional at planning time. Replace it only with an exact command plus pass/fail
evidence during execution.

| Gate | Current result | Last command/evidence |
|---|---|---|
| Rust formatting | PASS | `cargo fmt --all -- --check` |
| Rust clippy, all features, warnings denied | PASS | `cargo clippy --all-features --all-targets -- -D warnings` |
| Rust unit/integration tests, all features | PASS | `cargo test --locked --all-features --lib web_transfer -- --test-threads=1`: 195 passed, 0 failed; `cargo test --locked --all-features --test web_transfer_test -- --test-threads=1`: 40 passed, 1 ignored |
| Rust build, all features | PASS | `cargo build --locked --all-features` |
| Browser unit tests | PASS | `npm run check --prefix web/transfer`: 216 passed, 0 failed |
| Browser Chromium E2E | PASS | `npm run test:e2e --prefix web/transfer`: full matrix completed with no failures |
| Browser Firefox E2E | PASS | `npm run test:e2e --prefix web/transfer`: full matrix completed with no failures |
| Browser WebKit E2E | PASS | `npm run test:e2e --prefix web/transfer`: full matrix completed with no failures |
| Branded Chrome smoke | NOT RUN | Implementation not started |
| Branded Edge smoke | NOT RUN | Implementation not started |
| Branded Brave smoke | NOT RUN | Implementation not started |
| Web Transfer shell/E2E harness | DEFERRED | Targeted Rust HTTP tests and full package Playwright matrix passed; repository-level `scripts/web_transfer_e2e.sh` is intentionally deferred to final verification per operator instruction |
| Asset rebuild cleanliness | PASS | `npm run check --prefix web/transfer`: unit asset reproducibility assertion passed; normal build completed |
| Independent Node HKDF oracle and primitive red-check audit | PASS | `node:crypto.hkdfSync` matched fixture; label, final-seed-character, and RoomId-width mutations failed as expected; no source mutation remained |
| README contract audit | PASS | Active README audit found 0 legacy URL/storage hits; canonical short-link and persistent-fragment behavior are covered by the six README E2E tests |
| Secret/fragment leak audit | PASS | Runtime/source audit plus unit/E2E assertions: no storage recovery, no legacy parser, seed/member/key absent from error/URL paths; negative literals remain only in dedicated tests/docs |
| `git diff --check` | PASS | `git diff --check` |

In-flight: 2.2 — opened after 2.1 PASS; extend the branded Chrome/Edge/Brave smoke configuration and CI contract.

Branded-browser rules:

- Chrome and Edge use Playwright's supported branded channels.
- Brave uses an explicit executable path, preferring `BORE_BRAVE_EXECUTABLE_PATH`, otherwise
  `/usr/bin/brave-browser` in the documented CI image/host.
- A requested branded gate that has no executable is a clear failure or setup error, never a
  silent skip reported as success.
- Safari compatibility is represented by Playwright WebKit in routine Linux CI. A real Safari
  smoke on macOS remains a release/manual confidence gate and must be recorded distinctly; do
  not claim that WebKit-on-Linux is literal Safari.

## 7. Build and test discipline

For every implementation sub-phase:

1. add or update the narrowly relevant test first, and red-check it when the phase asks for one;
2. make the smallest production change that satisfies the frozen contract;
3. run the narrow gate before the broad gate;
4. rebuild `web/transfer/dist/app.js` only from the source build command documented in the repo;
5. run Rust tests only after the generated bundle is current when Rust embeds or packages it;
6. run `cargo fmt --all -- --check`, all-feature clippy with warnings denied, and the full relevant
   test suite before calling a phase complete;
7. update `README.md` in the final sub-phase of every phase;
8. inspect the final diff for generated junk, secrets, unrelated edits, and stale old-format text.

Do not hand-edit the generated browser bundle. Do not accept an existing green test as proof until
its assertion is shown to fail under the old behavior or an equivalent red-check described in the
phase has been performed.

## 8. Expected file ownership

This is a guide, not permission to modify unrelated files. The exact per-sub-phase list in each
phase file takes precedence.

| Area | Expected files |
|---|---|
| Rust derivation/URL | `src/web_transfer_cli.rs`, dependency manifests if required |
| Wire messages | `src/shared.rs`, `src/web_transfer.rs` |
| Browser derivation/URL | `web/transfer/src/crypto.js`, `web/transfer/src/secrets.js`, `web/transfer/src/main.js` |
| Browser tests/config | `web/transfer/test/**`, `web/transfer/playwright.config.js`, `web/transfer/package.json` |
| Generated browser asset | `web/transfer/dist/app.js` via the normal build only |
| Rust integration/E2E | `tests/web_transfer_test.rs`, relevant scripts/examples |
| CI | `.github/workflows/ci.yml` |
| User contract | `README.md` |
| Protocol detail | Existing Web Transfer protocol/design documentation identified in `phase_02.md` |

If an expected file named by the plan has moved, locate its current equivalent and record that
mapping here before editing. Do not revive a stale path or duplicate an existing module.

## 9. Documentation ledger

| Document | Required content | Status |
|---|---|---|
| `README.md` phase 0 update | Link shape, 128-bit seed, derivation/error contract as user-visible | PASS — verified active long format remains the only documented user format; no premature short-link text added |
| Protocol/design documentation | Owner protocol v2, client-selected room ID, hard cutover, vectors | PASS — `docs/transfer/WEB_TRANSFER_PROTOCOL.md` synchronized in sub-phase 1.4 |
| `README.md` phase 1 update | Complete CLI/browser flow, copy/address-bar behavior, no legacy links | PASS — synchronized in sub-phase 1.6 and verified by README E2E |
| `README.md` phase 2 update | Browser support matrix, branded/manual gates, security consequences | TODO |

README is the user-facing single source of truth. A phase is not complete while its README
sub-phase remains TODO.

## 10. Open questions, blockers, and assumptions

### Open questions

None. The operator answered Q1–Q6 and delegated Q4 to the stability requirement. Q4 is resolved as
a persistent, canonical fragment: it remains visible and copyable from the address bar for the
entire room session.

### Known operational assumptions

- Current target browsers expose native HKDF through WebCrypto. Unsupported environments fail
  clearly before network session establishment.
- The repository intentionally accepts a coordinated hard cutover of the native owner protocol.
- There are no legacy room URLs that require migration.
- Brave is installed explicitly in its periodic/manual CI environment.
- Real Safari execution needs a macOS runner or documented release workstation; routine Linux CI
  uses WebKit as the engine-level regression gate.

### Security consequence requiring preservation

Because the fragment remains visible, it may be retained in local browser history and exposed in
screenshots, screen sharing, copied address bars, or local session inspection. It is still excluded
from the HTTP request target under normal URL semantics. Documentation and tests must state both
facts without claiming that a fragment is globally secret.

## 11. Progress board

### Phase status

- Phase 0 — deterministic primitives and parity: DONE
- Phase 1 — protocol and URL hard cutover: DONE
- Phase 2 — browser matrix, security audit, final verification: IN PROGRESS

### Completion conditions

The plan is complete only when all of the following are true:

- every sub-phase is `DONE` in the execution-order table;
- all deterministic vectors match in Rust and browser JavaScript;
- the address bar always retains exactly `#` plus the 22-character seed;
- malformed and legacy fragments fail before WebSocket use;
- the server never receives the seed or room key;
- owner protocol v2 rejects version mismatch before room allocation;
- Chromium, Firefox, and WebKit automated gates pass;
- Chrome, Edge, and Brave branded smoke results are recorded without silent skips;
- real Safari manual/release status is honestly recorded;
- all Rust and Web Transfer regressions pass;
- generated assets are reproducible and current;
- README and protocol docs match the implementation;
- final self-review maps every frozen requirement to code and a passing gate;
- `git diff --check` passes and no unrelated changes are included.

## 12. Final handoff template

When implementation is genuinely complete, replace this section with:

- final branch and commit/worktree identity;
- concise list of changed behavior;
- exact short-link example from the fixture;
- exact commands and outcomes for every gate in section 6;
- browser matrix with engine/browser version and environment;
- any remaining release-only Safari result clearly marked;
- confirmation that the fragment remained persistent and no secret crossed the HTTP/WebSocket
  boundary except the derived room identifier and intended authorization material;
- confirmation that README and protocol docs were updated;
- explicit statement of zero known regressions, or a concrete unresolved-blocker report.
