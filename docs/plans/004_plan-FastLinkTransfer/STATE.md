# Fast Link Transfer — Implementation state

Read first at every session. Updated 2026-09-25 by agent-1:opus (planning).
Plan ID: 004_plan-FastLinkTransfer. Revision: 3. Repo root: /mnt/fabio/dati/Git/Github-manprint/bore-forked.
Plan baseline: 8d2032d4 (`dev`, "chore: bump release to v1.2.1-rc.1"); pre-existing changes: none (clean tree).
Roster: agent-1:opus, agent-2:sonnet, agent-3:haiku. Active execution style: delegated (coordinator = agent-1:opus session; workers = agent-2:sonnet via Agent tool).
State writer: agent-1:opus coordinator session; ownership: active.

## 0. Resume and completion protocol

1. Leggere questo file: readiness, scope, ruolo, unità/attempt, step, checkpoint, branch, baseline. Una nuova sessione riprende solo se il writer precedente ha rilasciato. Un worker delegato resta nella sua unità OPEN assegnata e non possiede mai stato condiviso né commit.
2. Ispezionare HEAD, staged e working tree; preservare modifiche non correlate. Riconciliare riferimenti di commit pendenti prima di nuove unità. Un'unità OPEN riprende dal primo step non verificato: ispezionare la realtà anche se il checkpoint è vecchio.
3. Leggere il phase file indicato e il suo "Local design context". Controllare prerequisiti in §11 e artefatti; cercare i simboli (le righe sono indizi). Contratto cambiato o design mancante → agent-1:opus. Ricerca: riusare R1–R5 di overview.md; mai inviare codice privato o segreti in query web.
4. Aprire prima di editare: §1 type/ID/attempt/OPEN, base, path posseduti, step S1; sottofase e fase IN_PROGRESS in §11. Checkpoint §6 prima di delega, operazioni lunghe/rischiose, blocker o probabile pausa.
5. Chiudere una sottofase coi suoi gate mirati (§3); P<N> esegue i gate completi. Test mancanti, zero-discovery o falliti impediscono DONE. Registrare revisione/diff testati e review reale.
6. Finire documentazione e riconciliazione, riga in §4, aggiornare §11, puntare alla prossima unità. Commit (full-autonomous): salvare `unit:<id>:<attempt>` come riferimento, stage SOLO dei file posseduti, commit sul branch corrente con trailer `PEV-Plan: 004`, `PEV-Unit: <id>`, `PEV-Attempt: <n>`, `PEV-Result: complete` e la riga `Co-Authored-By: Claude Opus 5.5 (1M context) <noreply@anthropic.com>`. Non inserire lo SHA del commit dentro sé stesso. Il primo commit (unità 0.1) include anche i file del piano 004.
7. Interruzione prima del commit: un riferimento non risolto = chiusura pendente, finalizzarla prima di nuovo lavoro. Già committato → risolvere il riferimento senza duplicare. WIP off.
8. Dopo le sottofasi aprire P<N> (gate completi + review agent-1:opus), fase DONE, commit di chiusura. Continuare con le unità eleggibili senza chiedere, dentro lo scope. Completamento finale = scenario di riferimento + tutti i gate + review + push su `dev` + CI verde (§3 G-FINAL).
9. Un worker escala decisioni mancanti o due fix falliti sullo stesso problema. agent-1:opus può rivedere passi tecnici entro requisiti invariati (registrare revisione in §8, aggiornare i phase file dipendenti). Mai indebolire l'accettazione per far passare un'implementazione. Nuove scelte prodotto → utente; supervisore non disponibile → BLOCKED handoff.
10. Su handoff salvare azione esatta, modifiche pendenti, evidenze, rilascio ownership.

## 1. Current unit and scope

- Active scope: plan 004 (tutte le fasi, fino a G-FINAL)
- Scope result: RUNNING
- Type: sub-phase
- ID / attempt: 0.4 / 1
- Status: OPEN
- Intent: sessione (slot, dispatch, upload, download, handoff, re-arm, scadenza, metriche)
- Assigned: agent-2:sonnet (nuovo worker delegato); supervisor: agent-1:opus
- File / unit heading: phase_01.md § 0.4
- Current step: S1 (delegato)
- Next action: attendere il worker, review (D13, I-3, I-4, I-7, I-8), gate
- Next eligible plan unit: 0.4 (phase_01.md)
- Unit base: (dopo commit 0.2); branch: dev; owned changes: none
- Repo state: HEAD 8d2032d4; tree pulito salvo `docs/plans/004_plan-FastLinkTransfer/` (artefatti di piano, da includere nel commit di 0.1)

## 2. Feature context and readiness

Readiness: READY — agent-1:opus, revision 1, 2026-09-25 (decisioni utente Q1–Q4 + round 2; D7–D22 del supervisore; R1–R3 CONFIRMED, R4 Slack CONFIRMED e altri UA optional con backstop, R5 CONFIRMED).
Goal: `bore server` con `BORE_FAST_LINK_TRANSFER_ENABLED=true`, `BORE_FAST_LINK_TRANSFER_VHOST=fast.<base>`, `BORE_FAST_LINK_TRANSFER_AUTH=USER:PASS` accetta `curl -u USER:PASS -T file https://fast.<base>` e `tar -cpf - dir | curl -N -u USER:PASS -T - https://fast.<base>/dir.tar`, risponde subito col link, e trasferisce in streaming puro (niente disco) all'unico downloader (curl/wget/browser). Banda massima (utente, ribadito). Sempre relay (nessun client = nessun P2P).
Exclusions: fan-out, resume/Range, persistenza, upload da browser, HTTP/2, UI admin con lista.
Current decisions: D1–D22 (overview.md).
Unresolved design/acceptance questions: none.
Research evidence: overview.md §Research R1–R5.
Required unverified external claims: none (R4 non-Slack è optional, backstop D2c).
Required supervisor reviews cannot be silently replaced by weaker self-review.

## 3. Environment, settings, and gate registry

- Full autonomous: **true** (utente, 2026-09-25: "setta la modalità full autonomous di default, deve essere sempre autonoma"; "eseguilo, in modalità autonoma")
- WIP commits: off
- Push: autorizzato SOLO a fine piano su `dev` (utente 2026-09-25: "alla fine push della funzionalità completa su dev, con ci verde"). Nessun altro push, nessun tag, nessun deploy.
- Completion policy: full autonomy committa sottofasi e chiusure di fase. `--no-wip-commit` non disabilita i commit autonomi.
- Scope/roster survive handoff. Explicit invocation overrides stored settings.
- Setup: Rust stable, Node ≥ 20, `curl`, `wget`, `openssl`, `python3`, `tar`, `sha256sum`; OpenSSH client per i test `t_ssh_*`; Playwright Chromium installato in `web/transfer` (`npx playwright install chromium`). cwd = repo root salvo indicazione.
- Supervisor access: il coordinatore è agent-1:opus; i worker agent-2:sonnet sono lanciati con l'Agent tool (`model: "sonnet"`) con il bundle di dispatch; review eseguite dal coordinatore sul diff reale.

| Gate | Stage / active from | Exact command and cwd | Required assertions/discovery | Setup |
|------|---------------------|-----------------------|-------------------------------|-------|
| G-BASE | baseline, prima di 0.1 (PASS 2026-09-25: lib 871/0, npm 120/0) | `cargo build --all-features && cargo test --all-features --lib && npm test` | 0 fail; npm 120 pass | — |
| G-FMT | ogni unità | `cargo fmt --all -- --check` | nessun diff | — |
| G-CLIPPY | ogni unità | `cargo clippy --all-features --all-targets -- -D warnings` | 0 warning | — |
| G-NODEF | da 0.1 | `cargo check --no-default-features --lib --bins` | compila | — |
| G-U0 | 0.1–0.4 | `cargo test --all-features --lib fast_link::` | tutti i test nominati delle unità chiuse eseguiti e verdi | — |
| G-U1 | 1.1–1.2 | `cargo test --all-features --lib -- reserved_label_reason conn_security config_and_metrics_publish_fast_link set_fast_link && cargo test --all-features --bins server_fast_link_flags` | ogni filtro scopre ≥ 1 test | — |
| G-I1 | 1.3 | `cargo test --all-features --test fast_link_test -- --test-threads=1 && cargo test --all-features --test ssh_gateway_test t_ssh_fast_link -- --test-threads=1` | ≥ 7 test fast_link + 1 ssh | OpenSSH client |
| G-NPM | 1.3 | `npm test` | ≥ 121 pass, 0 fail | Node |
| G-FULL | P1, P2 | i passi del job CI `test`: fmt; clippy; `cargo build --all-features`; `targets=(); for f in tests/*.rs; do n=$(basename "$f" .rs); [ "$n" = web_transfer_test ] && continue; targets+=(--test "$n"); done; cargo test --all-features --lib --bins --examples "${targets[@]}" -- --skip t_ssh_ --skip t_dmx_`; `cargo test --all-features --doc`; `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`; `cargo test --all-features --test web_transfer_test -- --test-threads=1`; `npm test` | 0 fail in ogni passo | OpenSSH |
| G-E2E | 2.1 | `bash scripts/fast_link_e2e.sh` | 12 righe `T-FL-E<n> PASS` | curl, wget, openssl |
| G-PERF | 2.2 | `bash scripts/fast_link_perf.sh` | `T-FL-PERF PASS` (mediana fast ≥ 1.0× vhost) e `T-FL-TRANSIT PASS`; campioni grezzi in §7 | Linux |
| G-PW | 2.3 | `npm run build --prefix web/transfer && cargo build --all-features --bin bore --example web_transfer_e2e_owner && (cd web/transfer && npx playwright test --project=chromium fast-link)` | 1 passed; `git diff --exit-code -- web/transfer/dist` | Chromium Playwright |
| G-FINAL | dopo P2 | `git push origin dev`, poi tutti i workflow del commit (`CI`, `E2E (netns)`, `Mean Bean CI`, `Mean Bean Deploy`, `Docker (GHCR)`) `completed success` (`gh run list --commit <sha>`) | nessun job `failure`; flake noti preesistenti rilanciati con `gh run rerun --failed` e registrati | gh auth |

## 4. Work ledger

| Type | ID / attempt | Plan revision | Agent | Changes | Evidence/review | Commit |
|------|--------------|---------------|-------|---------|-----------------|--------|
| plan | 004 / 1 | 1 | agent-1:opus | docs/plans/004_plan-FastLinkTransfer/* | recon + R1 probe locale + R2–R5 | unit:0.1:1 (incluso) |
| sub-phase | 0.3 / 1 | 3 | agent-2:sonnet (worker), review agent-1:opus | src/fast_link/pump.rs NEW, mod.rs | G-U0 36/36; red-check flush (fallisce senza flush) e coalescenza (4096 msg senza); 3 deviazioni accettate (written sempre restituito, free_rx None→Cancelled, assert prefisso su abort) | unit:0.3:1 |
| sub-phase | 0.2 / 1 | 2 | agent-2:sonnet (worker), review agent-1:opus | src/fast_link/framing.rs NEW, mod.rs | G-U0 26/26; review: grammatica chunked conforme, skip dati in blocco, errori sticky; derive additive accettate | unit:0.2:1 |
| sub-phase | 0.1 / 1 | 2 | agent-2:sonnet (worker), review agent-1:opus | src/lib.rs, src/fast_link/{mod,request,response}.rs NEW | G-FMT/G-CLIPPY/G-NODEF ok; G-U0 19/19; review: contratto ok, 3 deviazioni additive accettate (Debug su RequestHead, helper expect_err nei test, input CL 21 cifre) | unit:0.1:1 |

Commit references match trailers PEV-Plan, PEV-Unit, PEV-Attempt, PEV-Result.
Compare exact trailer values in current HEAD's ancestry, not subject/substrings.
No match is pending closure; multiple matches are a conflict requiring reconciliation.

## 5. Files and ownership

| Path/hunks | Existing changes to preserve | Unit changes | Owning unit |
|------------|------------------------------|--------------|-------------|
| docs/plans/004_plan-FastLinkTransfer/* | — | piano | plan → commit con 0.1 |

## 6. In-flight checkpoint

none

## 7. Verification and reviews

| Gate/test | Command | Result | Test count/named evidence | Tested revision/diff | When |
|-----------|---------|--------|--------------------------|---------------------|------|
| R1 probe | server Python + curl 8.5.0 (overview R1) | pass | 5 casi A–I | n/a | 2026-09-25 |

| Review | Reviewer | Plan revision / reviewed change | Invariants/assertions checked | Verdict |
|--------|----------|---------------------------------|------------------------------|---------|
| plan readiness | agent-1:opus | rev 1 | checklist Plan readiness; cold read di 0.4 | READY |
| 0.3 | agent-1:opus | rev 3, diff src/fast_link/pump.rs | D8 due task, zero alloc, cancel prima di drop full_tx, flush dopo write, resume_unwind, coalescenza now_or_never | APPROVED (dopo rev 3) |
| 0.2 | agent-1:opus | rev 2, diff src/fast_link/framing.rs | stati SizeDigits..Done, forward prefisso, overflow, LF nudo, trailer | APPROVED |
| 0.1 | agent-1:opus | rev 1, diff src/fast_link/{mod,request,response}.rs | D19/D20 messaggi e ordine, nessuna credenziale in Debug/errori, regole parse/target/preview | APPROVED (nota authority → rev 2) |

## 8. Technical revisions and deviations

| Revision | Previous decision/step | Approved replacement and reason | Supervisor | Dependents/revalidation |
|----------|------------------------|---------------------------------|------------|------------------------|
| 3 | 0.3 R: una read = un messaggio | R coalesce le read pronte (`now_or_never`) nello stesso buffer prima di inviarlo a W: tokio-rustls rende ~1 record (≤16 KiB) per read → senza coalescenza ~64k messaggi/wakeup/flush al s a 1 GB/s (review 0.3); nuovo test `pump_coalesces_small_reads` red-checked | agent-1:opus | solo 0.3 |
| 2 | 0.4 serve step 2: `authority` = header Host lowercase | `authority` = `config.host` + porta numerica dell'header se presente: nessun byte dell'header nel link stampato (review 0.1) | agent-1:opus | 0.4 (non ancora iniziata) |
| 1 (pre-READY) | pompa a una task sola | D8: pompa a due task con buffer riciclati — la versione a una task mette decifratura e cifratura TLS sullo stesso core (utente: priorità banda) | agent-1:opus | 0.3 nuova, 0.4 la usa; I-2/I-11 aggiornati |

## 9. Blockers

none

## 10. Do-not-repeat

- Non aggiungere campi a `VhostConfig` (31 struct literal; l'hot-reload li perderebbe): usare `ReservedVhostLabel`.
- Non usare `tokio::io::copy` (buffer 8 KiB) né allocare un buffer per chunk (soglia mmap glibc, H-18).
- Non toccare il loop di accept del control port (I-SSH1): `ConnSecurity` sul tipo.
- Mai due harness netns in parallelo; mai `sleep` fissi lunghi nei test.

## 11. Progress board

### Sub-phases
| ID | Phase file | Depends on | Status | Attempt | Evidence / reason |
|----|------------|------------|--------|---------|-------------------|
| 0.1 | phase_01.md | none | DONE | 1 | G-U0 19/19, review ok |
| 0.2 | phase_01.md | 0.1 | DONE | 1 | G-U0 26/26, review ok |
| 0.3 | phase_01.md | 0.2 | DONE | 1 | G-U0 36/36, 2 red-check, review ok |
| 0.4 | phase_01.md | 0.1, 0.2, 0.3 | IN_PROGRESS | 1 | — |
| 1.1 | phase_02.md | P0 | TODO | 1 | — |
| 1.2 | phase_02.md | 1.1 | TODO | 1 | — |
| 1.3 | phase_02.md | 1.1, 1.2 | TODO | 1 | — |
| 2.1 | phase_03.md | P1 | TODO | 1 | — |
| 2.2 | phase_03.md | 2.1 | TODO | 1 | — |
| 2.3 | phase_03.md | P1 | TODO | 1 | — |
| 2.4 | phase_03.md | 2.1, 2.2, 2.3 | TODO | 1 | — |

README obligation per phase: 0 → nessuna sezione cambia (verifica a P0); 1 → 1.3 S5 crea "Fast link transfer"; 2 → 2.4 S2 completa.

### Phases
| ID | File | Closure unit | Status | Review / commit reference |
|----|------|--------------|--------|---------------------------|
| 0 | phase_01.md | P0 | IN_PROGRESS | — |
| 1 | phase_02.md | P1 | TODO | — |
| 2 | phase_03.md | P2 | TODO | — |

Statuses: TODO, IN_PROGRESS, IN_REVIEW, DONE, SKIPPED, BLOCKED.

### Tests
| ID/name | Owning unit | Gate | Status | Evidence |
|---------|-------------|------|--------|----------|
| resolve_* / generate_id / parse_* / upload_* / download_target / preview / host_matches / response_bytes_exact | 0.1 | G-U0 | PASS | 19/19 |
| cl_* / chunked_* / error_is_sticky | 0.2 | G-U0 | PASS | 7/7 (26 totali) |
| pump_* (10) incl. red-check `pump_writes_are_flushed_before_waiting`, `pump_coalesces_small_reads` | 0.3 | G-U0 | PASS | 10/10, loop 5x senza flake |
| T-FL-S1..S12, S14..S16 | 0.4 | G-U0 | TODO | — |
| reserved_label_reason_*, set_fast_link_*, config_and_metrics_publish_fast_link_*, server_fast_link_flags_* | 1.1 | G-U1 | TODO | — |
| conn_security_is_static_per_type | 1.2 | G-U1 | TODO | — |
| T-FL-I1..I7, t_ssh_fast_link_label_is_reserved, metrics-fast-link.test.js | 1.3 | G-I1, G-NPM | TODO | — |
| T-FL-E1..E12 | 2.1 | G-E2E | TODO | — |
| T-FL-PERF, T-FL-TRANSIT | 2.2 | G-PERF | TODO | — |
| T-FL-PW | 2.3 | G-PW | TODO | — |

### Documentation
| Document/sections | Owning unit | Status | Evidence |
|-------------------|-------------|--------|----------|
| README "Fast link transfer" | 1.3 (crea), 2.4 (completa) | TODO | — |
| docs/transfer/FAST_LINK.md | 2.4 | TODO | — |
| docs/README.md indice | 2.4 | TODO | — |
| CLAUDE.md Key invariants | 2.4 | TODO | — |

### Audits
| Report | Verdict | Current unresolved findings | Evidence |
|--------|---------|-----------------------------|----------|
| — | — | — | — |

## 12. Suspended implementation (only while auditing unfinished work)

Snapshot state: none.
