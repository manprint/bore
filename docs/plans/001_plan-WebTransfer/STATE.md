# Web Transfer multipeer — Implementation State

> **READ THIS FILE FIRST at the start of every session, before any other plan file. OPEN a unit in §1 before touching code; CLOSE it after the gates pass.**
> **Last updated:** 2026-09-14 18:50 CEST | **By:** muse-spark1.3 (executor, single-agent) | **Session:** 2

## 0. Protocol

This is the only execution-state file — position, progress, ledger, and blockers all live here. A **unit of work** is one sub-phase, one `task`, one `bug`, one `verify` audit, or one correction from a verify report.

**Resume (cold start):**
1. Read this file end to end.
2. Read §1 `Status`:
   - `OPEN` — a unit was claimed and may be half-written. Read §6, then finish or revert it before starting anything new. If §3 has WIP commits on and `HEAD` is a `wip:` commit, that commit is the in-flight work: its diff is what got written, §6 says why it stopped. Finish and `amend` into the close commit, or revert it.
   - `none` — nothing in flight. Open the unit named in §1 `Next action:`.
3. Run the gate commands in §3 and compare the result with what §1, §7, and §11 claim. The repo is the truth; correct this file if it drifted.
4. Open only the file §1 points at: the phase file at the named sub-phase, or the verify report for a correction. Read `overview.md` only if §2 is insufficient.

**Open a unit — before touching code, mandatory:** set §1 `Type`, `ID`, `Status: OPEN`, `Intent`, `Next action:`, `Assigned`; set §6 to `claimed — nothing written yet`; bump the header timestamp. Only then edit anything.

**Implementation order — mandatory:** write or update the unit's listed tests first or alongside production edits. Do not defer tests, documentation or cleanup to a later unit unless that later unit explicitly owns them.

**Close a unit — after its gates are green, mandatory:** append a §4 ledger row; reset §6 to `none — tree consistent`; update §5, §7, §8, §9, §10 and the §11 board; set §1 to the next unit with `Status: none`; bump the timestamp. When §3 has WIP commits on, commit the closed unit — code, tests, this file, docs, ledger together — staging only the files in §5 plus the plan files, never `git add -A`, and record the sha in the §4 row. A unit is not `DONE` until this is written.

**Interrupted mid-unit:** leave §1 `OPEN` and write into §6 exactly what is half-finished — files written, edits still pending, temporary code or TODO markers to remove. `OPEN` with an empty §6 is an execution bug. With WIP commits on, also commit that state as `wip(<id>): <what remains>`.

## 1. Current unit

- **Type:** `sub-phase`
- **ID:** 2.1
- **Status:** `none`
- **Intent:** next phase (Phase 2 — HTTP/control/catalogo, `phase_03.md`).
- **Phase:** 2 — HTTP, WebSocket controllo e catalogo (`phase_03.md`)
- **Next action:** Open sub-phase 2.1 in this section, read only `phase_03.md` §2.1, then implement.
- **Assigned:** `agent:muse-spark1.3`
- **Repo state:** branch `dev` | working tree `dirty (phase 0-1 code + plan files, uncommitted)` | last commit `d13ee21 fix(release): only a version tag is "latest"`

## 2. Feature context (self-contained recap)

`bore transfer web` creates no file selection: it prints a secret-fragment URL and keeps the room owner lease alive. Every browser peer with that link can announce local files/folders, see all peers' offers and explicitly download another peer's offer. Browser data uses one WebRTC DataChannel per transfer when possible, then an opaque bounded WebSocket relay fallback; both paths carry application AES-256-GCM ciphertext and the server stores no payload. Clean owner exit destroys immediately; abnormal disconnect allows authenticated resume for 60 seconds by default. Downloads never start or resume without a recipient click.

**Reference scenario:** A runs the CLI, opens its browser and announces a folder; B clicks and downloads it direct; C has ICE forced to fail and downloads through relay; B/C can publish too; B cancels a transfer; closing A's CLI invalidates room, URL and active transfers.
**Hard constraints:** preserve every `AGENTS.md` invariant and old wire fixture; append enum variants last; no new browser-data UDP socket/native QUIC; bounded buffers/maps/tasks/rates; no server payload storage; README updated at the end of every phase.
**Key decisions in force:**
- **D1:** CLI owns only the lease; all data sources/recipients are equal browser peers.
- **D2:** URL is `/transfer/<32hex>#m=<64hex>&k=<64hex>`; fragment secrets move to sessionStorage then are scrubbed.
- **D3:** recipient is fixed WebRTC SDP offerer, source answerer; one peer connection/data channel per logical transfer.
- **D4:** relay pairs two role-bound, one-use-ticket WebSockets and forwards one ciphertext frame at a time without storage.
- **D5:** server authority plus fresh AttemptId/derived key resolves direct/fallback/cancel races; source reads only after path_commit.
- **D6:** source-only serving; a recipient never swarms or republishes unless the user selects the local output explicitly.
- **D7:** manifest is immutable, canonical and HMAC-authenticated; 1 MiB SHA-256 chunks use the fixed rolling-root formula.
- **D8:** OPFS/IndexedDB hold verified partials but no token/key; cancel retains valid chunks, withdraw/room close purge them.
- **D9:** ZIP is per offer, store-mode streaming Zip64 via zip.js 2.15.0; no room-wide archive or full archive buffer.
- **D10:** service is enabled only by HTTPS `--web-transfer-base-url` (HTTP loopback allowed), on the existing control listener and exact Host.
- **D11:** all public defaults, caps, timeouts, error codes, flags and protocol bytes are fixed in `overview.md` and phase files; do not substitute alternatives.
- **D12:** implementation agent for every unit is `agent:gpt-5.6-luna`, with self-review before close.

## 3. Environment and commands

The authoritative gate commands. Identical to the phase gates and to `overview.md`'s verification summary — no drift.

- **Repo root:** `/mnt/fabio/dati/Git/Github-manprint/bore-forked`
- **Build:** `cargo build --all-features` · **Fmt:** `cargo fmt --all -- --check` · **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Unit tests:** `cargo test --all-features --lib && npm ci --prefix web/transfer && npm run check --prefix web/transfer` · **E2E:** `cargo test --all-features --test web_transfer_test -- --test-threads=1 && npm run test:e2e --prefix web/transfer`
- **Asset/regression:** `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist && cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ && cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`
- **Setup / caveats:** Phase 0.1 marks npm commands N/A until 0.2 creates `web/transfer`; use Node >=20 thereafter. Frontend dist is committed and Cargo must build without node_modules. Bind tests use dynamic ports and `web_transfer_test` runs serially. Existing sudo/netns harnesses authorized in `AGENTS.md` run serially when host prerequisites exist. TokenSave was synced for branch `dev`, but the active MCP process still served `main` at plan time; use branch `dev` after MCP restart or inspect source directly until then.
- **WIP commits:** `off`

## 4. Work ledger (append-only, one row per closed unit)

Every unit type shares this ledger, in the order it closed. `Commit` is `uncommitted` while WIP commits are off.

| # | Type | ID | Agent | What changed | Files | Gates | Commit |
|---|------|----|-------|--------------|-------|-------|--------|
| 1 | sub-phase | 0.1 | agent:muse-spark1.3 | domain model + constants + validators + pinned deps, no runtime wiring | Cargo.toml, Cargo.lock, src/lib.rs, src/web_transfer.rs, src/web_transfer_protocol.rs | fmt+clippy+build+lib 649+12 green | uncommitted |
| 2 | sub-phase | 0.2 | agent:muse-spark1.3 | npm/esbuild/playwright scaffold + committed dist + build.rs embed | web/transfer/*, build.rs, .gitignore, src/web_transfer.rs | npm ci+check+e2e(3 engines)+drift-idempotent+Rust fmt/clippy/lib green | uncommitted |
| 3 | sub-phase | 0.3 | agent:muse-spark1.3 | protocol v1 doc + Rust/JS codecs + shared fixtures + E2EE cross-check | docs/transfer/WEB_TRANSFER_PROTOCOL.md, src/web_transfer_protocol.rs, web/transfer/src/protocol.js, web/transfer/src/crypto.js, tests/fixtures/web_transfer/v1/*, web/transfer/tests/unit/{protocol,crypto}.test.mjs, web/transfer/tests/unit/vectors.mjs, Cargo.toml/lock | fmt+clippy+lib 660 green, JS 11 green, e2e 3 green, T-WEB-E2EE-FIXTURE green | uncommitted |
| 4 | sub-phase | 0.4 | agent:muse-spark1.3 | README verified accurate, byte-identical (no user-visible change) | — | existing transfer docs intact, no web-transfer mention | uncommitted |
| 5 | sub-phase | 1.1 | agent:muse-spark1.3 | 15 server flags + resolve + registry/admission + ConfigView + T-WEB-CONFIG | src/main.rs, src/server.rs, src/admin_views.rs, src/admin_api.rs(test), tests/admin_test.rs, src/web_transfer.rs, tests/web_transfer_test.rs, tests/support/web_transfer.rs | fmt+clippy+lib 668+T-WEB-CONFIG+JS11+e2e3+full-regression(30 suites) green | uncommitted |
| 6 | sub-phase | 1.2 | agent:muse-spark1.3 | OwnerLease/resume/expiry/destroy + lifecycle tests + T-WEB-REGISTRY-LIFE | src/web_transfer.rs, tests/web_transfer_test.rs | fmt+clippy+lib 680+T-WEB-REGISTRY-LIFE(x3)+lifecycle(x5) green | uncommitted |
| 7 | sub-phase | 1.3 | agent:muse-spark1.3 | native owner wire + serve_owner_control + fixtures + T-WEB-NATIVE-WIRE | src/shared.rs, src/web_transfer.rs, src/server.rs, src/client.rs, src/secret.rs, tests/web_transfer_test.rs | fmt+clippy+lib 687+regression 30 suites+SSH serial(5+42)+JS11+e2e3 green | uncommitted |
| 8 | sub-phase | 1.4 | agent:muse-spark1.3 | owner client loop + 9 unit tests + T-WEB-OWNER-LEASE | src/web_transfer_cli.rs, src/lib.rs, src/server.rs, tests/web_transfer_test.rs, tests/support/web_transfer.rs | fmt+clippy+lib 696+T-WEB-OWNER-LEASE(x2)+JS11+e2e3+regression+SSH(42/42 rerun) green | uncommitted |
| 9 | sub-phase | 1.5 | agent:muse-spark1.3 | README verified accurate, byte-identical (no user-visible change) | — | no web-transfer mention, legacy transfer docs intact | uncommitted |

## 5. Files touched

| Path | What was done | Unit |
|------|---------------|------|
| Cargo.toml | +url 2.5, +tokio-tungstenite 0.28.0 (connect/handshake/stream, no default), +webbrowser 1.1.0, +subtle 2.6 (lockfile reuse) | 0.1 |
| Cargo.lock | resolved pins regenerated by build | 0.1 |
| src/lib.rs | +pub mod web_transfer, web_transfer_protocol | 0.1 |
| src/web_transfer.rs | new: consts, 9 hex IDs, limits+validate, base-url parse, ICE resolve, config, 10 unit tests | 0.1 |
| src/web_transfer.rs | 1.1: error type, RoomState/OwnerState/records, registry+admission+STUN/flag resolve, 6 tests | 1.1 |
| src/web_transfer_protocol.rs | new: PROTOCOL_VERSION re-export, CONTROL_SUBPROTOCOL, RequestId, 2 unit tests | 0.1 |
| web/transfer/dist/app.js, app.css, index.html | generated committed shell (esbuild, deterministic) | 0.2 |
| web/transfer/tests/unit/scaffold.test.mjs | inline-content + reproducibility unit tests | 0.2 |
| web/transfer/tests/e2e/scaffold.spec.mjs | T-WEB-SCAFFOLD harness (static dist, 3 engines) | 0.2 |
| build.rs | walk_asset_dir/emit_asset_table helpers + strict web bundler | 0.2 |
| .gitignore | ignore node_modules/test-results/playwright-report/blob-report | 0.2 |
| docs/transfer/WEB_TRANSFER_PROTOCOL.md | new: normative v1 (§1-9 + byte-offset table) | 0.3 |
| tests/fixtures/web_transfer/v1/manifest.json | new: 2-entry offer fixture | 0.3 |
| tests/fixtures/web_transfer/v1/manifest.canonical.json | new: exact canonical bytes | 0.3 |
| tests/fixtures/web_transfer/v1/crypto-vectors.json | new: inputs + JS-generated expected | 0.3 |
| tests/fixtures/web_transfer/v1/control-messages.json | new: 38 control + relay.attach envelopes | 0.3 |
| web/transfer/src/protocol.js | new: canonical/manifest/envelope/attach mirror | 0.3 |
| web/transfer/src/crypto.js | new: HKDF/HMAC/root/frame WebCrypto mirror | 0.3 |
| web/transfer/tests/unit/protocol.test.mjs | new: canonical/manifest/control mirror tests | 0.3 |
| web/transfer/tests/unit/crypto.test.mjs | new: HMAC/root/key/frame mirror tests | 0.3 |
| web/transfer/tests/unit/vectors.mjs | new: T-WEB-E2EE-FIXTURE JS vector runner | 0.3 |
| src/web_transfer_protocol.rs | extended: envelopes/canonical/manifest/HKDF/frames/attach + 11 tests | 0.3 |
| Cargo.toml / Cargo.lock | +unicode-normalization 0.1 (NFC both sides) | 0.3 |

## 6. In-flight work

none — tree consistent (Phase 1 DONE through 1.5; next 2.1)

## 7. Verification state

| Gate / test | Command | Last result | When |
|-------------|---------|-------------|------|
| build | `cargo build --all-features` | `pass` | 0.1, 14:54 CEST |
| fmt | `cargo fmt --all -- --check` | `pass` | 0.1, 14:54 CEST |
| lint | `cargo clippy --all-features --all-targets -- -D warnings` | `pass` | 0.1, 14:54 CEST |
| Rust unit | `cargo test --all-features --lib` | `pass 696 (48 web_transfer + 9 cli)` | 1.4, 18:45 CEST |
| frontend install/check | `npm ci --prefix web/transfer && npm run check --prefix web/transfer` | `pass` | 0.2, 15:08 CEST |
| web-transfer Rust e2e | `cargo test --all-features --test web_transfer_test -- --test-threads=1` | `pass (CONFIG/LIFE/NATIVE-WIRE/OWNER-LEASE)` | 1.4, 18:45 CEST |
| browser e2e | `npm run test:e2e --prefix web/transfer` | `pass (T-WEB-SCAFFOLD, 3 engines)` | 0.2, 15:08 CEST |
| asset drift | `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist` | `pass-idempotent (hashes stable; dist new-uncommitted)` | 0.2, 15:08 CEST |
| full non-netns regression | `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_` | `pass (all targets 0 failed)` | 0.1, 14:54 CEST |
| serial SSH regression | `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | `not-run` | — |

**Failing output (verbatim, trimmed to the error):**
```text
<none>
```

## 8. Runtime deviations from the plan

| # | Plan said | What was done | Why | Impact on later phases |
|---|-----------|---------------|-----|------------------------|
| 1 | assignment `agent:gpt-5.6-luna` | executed as `agent:muse-spark1.3` | invocation override; same single-agent self-review role | none; later units keep invocation assignment |
| 2 | constant-time compare "using the existing cryptographic dependency" | `subtle = "2.6"` named directly (lockfile reuse, url-pattern) + `ConstantTimeEq` | `ring::constant_time` deprecated in ring 0.17 (clippy deny); subtle already transitive | none; no new compiled dep |
| 3 | tokio-tungstenite "only the handshake/runtime features required" | features `connect,handshake,stream`, no default, no TLS | server accept + client connect + split streams; TLS stays on existing rustls listener | 0.2+/Phase 2 use this set; widen only with reason |
| 4 | Files line names `WebTransferConfig` with no Change fields | minimal aggregate `{base_url, limits, ice}` + validating `new()` | Change authoritative; server-flag wiring deferred to Phase 1 | Phase 1 extends, never redefines |
| 5 | Files line names `WebTransferConfig` with no Change fields | minimal aggregate `{base_url, limits, ice}` + validating `new()` | Change authoritative; server-flag wiring deferred to Phase 1 | Phase 1 extends, never redefines |
| 5 | raw-byte accessors with no consumers yet | `pub(crate) as_bytes` + `#[allow(dead_code)]` (first consumers Phase 1 / 0.3) | `-D warnings` denies dead code on the non-test target | remove allows only if a phase leaves them truly unused |
| 6 | 0.2 node --test directory arg | scripts use quoted `tests/unit/**/*.test.mjs` glob | node 24 loads a directory arg as CJS module | none |
| 7 | 0.2 ESM ignores NODE_PATH | repro test symlinks real node_modules into tmp copy | ESM resolution is path-based | none |
| 8 | 0.2 e2e harness 404s | favicon→204; `/transfer/assets/*` stripped to dist names | mirrors Phase 2 routing; browsers always probe favicon | harness replaced by real server in Phase 2 |
| 9 | 0.2 build.rs generalization | shared `walk_asset_dir` (sorted) + strict web bundler; admin traversal/lookup semantics preserved | sorted output = reproducible generated source | Phase 2 routes consume WEB_TRANSFER_ASSETS |
| 10 | 0.3 unicode-normalization dep | `unicode-normalization 0.1` (pure Rust) for NFC both sides | 0.1 could not foresee; manifest NFC rule needs tables on both ends | none |
| 11 | 0.3 frame layout undefined in overview | defined in protocol doc §6 (magic/version/type/flags/seq/body_len + HKDF formulas); fixtures pin bytes | overview only said "16-byte header"; 0.3 is authoritative per plan | later phases implement against doc + fixtures |
| 12 | 0.3 JS AAD omission (caught pre-gate) | `additionalData: header` in seal/open; T-WEB-E2EE-FIXTURE proves interop | WebCrypto binds AAD only when passed; omission = cross-impl failure | regression covered by E2EE fixture |
| 13 | 1.1 semaphore receivers | `Arc<Semaphore>` fields (`try_acquire_owned` needs `Arc` self) | mirrors existing `conn_permits` shape | none |
| 14 | 1.1 ConfigView surface | only `web_transfer_enabled` + `web_transfer_base_origin` now; full totals/metrics stay Phase 6 | admin JSON null = disabled is safe; P-11 forbids gauge/total conflation | Phase 6 appends remaining fields |
| 15 | 1.1 peer duplicate policy | duplicate `join_peer` rejected (`INVALID_MESSAGE`) | placeholder; Phase 2 join semantics may refine | none |
| 16 | 1.2 expiry monitor holds Weak | monitor captures `Weak` + epoch + id, never strong `Arc` | strong hold pinned room memory + global slot for whole grace | pairing rule documented on `remove_room_if_current` |
| 17 | 1.3 hex serde on IDs/secrets | manual hex-string `Serialize/Deserialize` for all nine newtypes | native yamux wire needs them; deliberate, admin/log must never serialize rooms | none |
| 18 | 1.3 Copy tokens | explicit `drop()` removed (no-op lint); raw value only hashed, never stored/logged | `OwnerToken` is `Copy`; scope end is the only boundary | test asserts no token hex in state/reply |
| 19 | 1.4 public-but-hidden cli module | `#[doc(hidden)] pub mod web_transfer_cli` instead of crate-private | T-WEB-OWNER-LEASE (integration) must drive it; Phase 3 command reuses it | none |
| 20 | 1.4 reset via proxy, not abort | killable loopback TCP proxy for reset/expiry phases | aborting listen leaves accepted conns alive (one-sided, proves nothing) | proxy helper in tests/support |
| 21 | 1.4 SSH serial single flake | 1 failure in ssh_gateway_test, 42/42 green on rerun | timing-flaky SSH suite; change touches no SSH path | none |

## 9. Blockers and open questions

- none — the user accepted every proposed default; implementation must use the decisions in the plan without reopening them.

## 10. Do-not-repeat

- Do not ask the CLI to select or read paths; selection belongs to every browser peer.
- Do not reuse native QUIC/holepunch types for browser data or bind another UDP socket.
- Do not add HTTP upload, server file/temp storage, TURN, CDN assets, service workers, OS notifications or telemetry.
- Do not auto-download, auto-resume, auto-seed or create a room-wide ZIP.
- Do not use fflate or another ZIP library; the pinned implementation is `@zip.js/zip.js` 2.15.0.
- Do not move secret material from the URL fragment/sessionStorage into query, localStorage, IndexedDB, log or admin state.
- Do not use `timeout(recv)` for liveness or an unbounded heartbeat send; follow the tick/last_recv and `beat_once` invariants.
- Do not let async cleanup look up a current room by ID; capture the actual room through Arc/Weak plus epoch.
- Do not "fix" `cargo check --no-default-features`: broken pre-existing on clean tree (holepunch `Arc` import, verified via stash 14:54 CEST); plan gates use `--all-features`.
- Do not reintroduce `ring::constant_time`: deprecated in ring 0.17; digest comparison uses `subtle::ConstantTimeEq`.

## 11. Progress board

### Phases

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Fondazioni, contratti e pipeline asset | phase_01.md | `DONE` | 0.1–0.4 closed; fixtures + embed + README verified |
| 1 — Registry room e lease proprietario | phase_02.md | `DONE` | 1.1–1.5 closed; native owner lifecycle proved |
| 2 — HTTP, WebSocket controllo e catalogo | phase_03.md | `TODO` | internal browser harness |
| 3 — Relay cifrato e prima vertical slice | phase_04.md | `TODO` | first public relay-only release |
| 4 — WebRTC diretto con fallback | phase_05.md | `TODO` | direct-first behavior |
| 5 — Cartelle, ZIP e UX multippeer | phase_06.md | `TODO` | final functional flow |
| 6 — Hardening, osservabilità e rilascio | phase_07.md | `TODO` | production gates and docs |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

### Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-WEB-SCAFFOLD | browser e2e | `TODO` | inert embedded shell on three engines |
| T-WEB-E2EE-FIXTURE | cross-language | `TODO` | Rust/JS canonical and crypto bytes match |
| T-WEB-CONFIG | Rust e2e | `DONE` | 1.1: valid binds, invalid exits nonzero, disabled absent |
| T-WEB-REGISTRY-LIFE | Rust e2e | `DONE` | 1.2: resume-same-room, expiry, stale-monitor vs reused ID (x3) |
| T-WEB-NATIVE-WIRE | Rust e2e | `DONE` | 1.3: create/heartbeat/drop/resume/close + disabled/version errors |
| T-WEB-OWNER-LEASE | Rust e2e | `DONE` | 1.4: reset-resume/3x clean close/grace expiry/log privacy (x2) |
| T-WEB-HTTP | Rust e2e | `TODO` | same-origin routing/security and old routes |
| T-WEB-PEERS | Rust e2e | `TODO` | auth, snapshot, lag, join/leave and slow peer |
| T-WEB-CATALOG-SERVER | Rust e2e | `TODO` | offer auth, caps, convergence and cleanup |
| T-WEB-BROWSER-ROOM | browser e2e | `TODO` | fragment scrub, peer UI and reconnect |
| T-WEB-OFFER | browser e2e | `TODO` | deterministic browser manifest/HMAC |
| T-WEB-NOAUTO | browser e2e | `TODO` | zero transfer/RTC/relay before click |
| T-WEB-EQUAL-PEERS | browser e2e | `TODO` | A/B/C symmetric publish/withdraw rights |
| T-WEB-REPUBLISH | browser e2e | `TODO` | reconnect republishes offers only |
| T-WEB-TRANSFER-STATE | Rust e2e | `TODO` | request, ready, ticket, auth and cleanup |
| T-WEB-RELAY-OPAQUE | Rust e2e | `TODO` | bounded opaque relay and rate |
| T-WEB-SENDER-RELAY | browser e2e | `TODO` | source starts after commit and detects changes |
| T-WEB-DOWNLOAD-RELAY | browser e2e | `TODO` | OPFS receive, verify and explicit save |
| T-WEB-CANCEL-RESUME | browser e2e | `TODO` | cancel at 25%, click-to-resume and exact bytes |
| T-WEB-CANCEL-AUTH | browser/Rust | `TODO` | participants only can cancel |
| T-WEB-CLI | process e2e | `TODO` | exact output, flags, signals and resume |
| T-WEB-NOSTORE | OS/process e2e | `TODO` | 64 MiB relay leaves filesystem/fd/log clean |
| T-WEB-E2EE | security e2e | `TODO` | key/AAD/attempt failures and key absent server-side |
| T-WEB-ROOM-LIFE | process e2e | `TODO` | clean close and abnormal grace lifecycle |
| T-WEB-LIMITS | adversarial e2e | `TODO` | caps/rates/malformed input preserve healthy peers |
| T-WEB-LEGACY | regression | `TODO` | native transfer/public/secret/vhost/SSH unchanged |
| T-WEB-README-RELAY | docs e2e | `TODO` | relay-only README flow works |
| T-WEB-SIGNALING | Rust e2e | `TODO` | roles/order/caps/timeout/fallback |
| T-WEB-DIRECT | browser e2e | `TODO` | real DataChannel, exact bytes, no relay |
| T-WEB-DIRECT-FALLBACK | browser e2e | `TODO` | midstream direct failure resumes on relay |
| T-WEB-DIRECT-TIMEOUT | browser e2e | `TODO` | ICE failure falls back after one click |
| T-WEB-MULTIPEER | browser e2e | `TODO` | B direct, C forced ICE failure relay, symmetric offers |
| T-WEB-PATH-AUTH | browser/Rust | `TODO` | recipient first verified chunk sets path |
| T-WEB-README-DIRECT | docs e2e | `TODO` | documented direct and fallback examples work |
| T-WEB-FOLDER-OFFER | browser e2e | `TODO` | portable tree manifest and no auto transfer |
| T-WEB-ZIP | browser e2e | `TODO` | deterministic per-offer ZIP direct/relay |
| T-WEB-ZIP-RESUME | browser e2e | `TODO` | regenerate, skip verified network chunks, exact ZIP |
| T-WEB-ZIP-SOURCE-CHANGE | browser e2e | `TODO` | changed source cannot corrupt partial |
| T-WEB-MULTIPEER-FINAL | acceptance | `TODO` | complete A/B/C folder flow |
| T-WEB-OWNER-SEPARATION | acceptance | `TODO` | CLI owner has no browser privilege |
| T-WEB-SOURCE-ONLY | acceptance | `TODO` | no automatic swarm/republish |
| T-WEB-README-FINAL-FLOW | docs e2e | `TODO` | documented folder/ZIP scenario works |
| T-WEB-SOAK | load e2e | `TODO` | fixed peer/offer/relay load stays bounded |
| T-WEB-FDBUDGET | OS/process e2e | `TODO` | rlimit includes web admitted descriptors |
| T-WEB-FAIRNESS | load e2e | `TODO` | throttled room does not starve another |
| T-WEB-ADMIN | admin e2e | `TODO` | config totals and live gauges are distinct |
| T-WEB-LOG-PRIVACY | security e2e | `TODO` | forbidden canaries absent from log/admin |
| T-WEB-MALFORMED | adversarial e2e | `TODO` | malformed corpus bounded and non-disruptive |
| T-WEB-XSS-CSRF | browser security | `TODO` | origin/CSP/remote text cannot execute or leak |
| T-WEB-UDP-ENDPOINT | OS/process e2e | `TODO` | no new UDP socket for web transfer |
| T-WEB-CROSS | browser matrix | `TODO` | Chromium/Firefox/WebKit and cross-engine paths |
| T-WEB-ASSET-DRIFT | build | `TODO` | clean frontend rebuild is byte-identical |
| T-WEB-PACKAGE | packaging | `TODO` | source artifact builds without Node and serves UI |
| T-WEB-BRANDED | release smoke | `TODO` | real Chrome/Edge scheduled/manual result |
| T-WEB-DEPLOY | deployment e2e | `TODO` | release/container behind TLS proxy works |
| T-WEB-ACCEPTANCE | acceptance | `TODO` | final distributed-binary A/B/C scenario |
| T-WEB-NOSTORE-CONTAINER | OS/container | `TODO` | read-only container stores no payload |
| T-WEB-README-RELEASE | docs e2e | `TODO` | final README works from clean checkout |

### Docs

| Doc | Phase | Status | Notes |
|-----|-------|--------|-------|
| README.md | 0 | `DONE` | verified byte-identical, no user-visible change (0.4) |
| README.md | 1 | `DONE` | verified byte-identical, no user-visible change (1.5) |
| README.md | 2 | `TODO` | verify no user-visible change |
| README.md | 3 | `TODO` | relay-only command/deploy/limits |
| README.md | 4 | `TODO` | direct-first, fallback and STUN |
| README.md | 5 | `TODO` | folders, ZIP and final peer flow |
| README.md | 6 | `TODO` | full production/deploy/admin/troubleshooting review |
| docs/transfer/WEB_TRANSFER_PROTOCOL.md | 0 | `TODO` | normative protocol v1 and fixtures |
| docs/transfer/WEB_TRANSFER_PROTOCOL.md | 6 | `TODO` | final state/error/operations/versioning audit |
| Docker/compose deployment examples | 6 | `TODO` | disabled-by-default config and no payload volume |

### Audits

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| <none yet> | — | — | — |
