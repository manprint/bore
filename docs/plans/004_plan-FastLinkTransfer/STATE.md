# Fast Link Transfer — Implementation state

Read first at every session. Updated 2026-09-25 by agent-1:opus (planning).
Plan ID: 004_plan-FastLinkTransfer. Revision: 5. Repo root: /mnt/fabio/dati/Git/Github-manprint/bore-forked.
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
- Type: phase closure
- ID / attempt: P2 / 1
- Status: OPEN
- Intent: chiusura finale (gate locali, review, push su dev, CI remota verde)
- Assigned: agent-1:opus
- File / unit heading: phase_03.md § Phase gates and closure
- Current step: S1
- Next action: fmt/clippy, commit P2, `git push origin dev`, seguire tutti i workflow del commit
- Next eligible plan unit: G-FINAL
- Unit base: commit unit:2.4:1; branch: dev; owned changes: none
- Repo state: HEAD = commit 1.3; non tracciati scripts/fast_link_e2e.sh, scripts/fast_link_perf.sh (unità 2.1/2.2, scritti da agent-1:opus in parallelo a 1.3)

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
| sub-phase | 2.4 / 1 | 5 | agent-1:opus | docs/transfer/FAST_LINK.md NEW, README.md (troubleshooting + rimando), docs/README.md (indice), CLAUDE.md (Key invariants) | verifica contro il codice: 11 nomi di test citati esistono (`grep fn`), 5 flag presenti in `bore server --help`, testi delle risposte e limiti letti da `session.rs`/`request.rs`/`mod.rs`, UA list copiata da `BOT_UA_MARKERS`, numeri T-FL-PERF dai tre run di §7 | unit:2.4:1 |
| sub-phase | 2.3 / 1 | 5 | agent-1:opus | web/transfer/tests/e2e/fast-link.spec.mjs NEW | G-PW: `npm run build --prefix web/transfer` (dist invariato, `git diff --exit-code -- web/transfer/dist` pulito), `cargo build --all-features --bin bore --example web_transfer_e2e_owner`, `npx playwright test --project=chromium fast-link` → 1 passed (415 ms): nome suggerito `payload.bin`, SHA-256 identico, uploader exit 0, secondo GET 404; firefox/webkit → 2 skipped con motivo; `page.request` NON usato per il 404 (è un client Node e non vede `--host-resolver-rules`) → seconda navigazione | unit:2.3:1 |
| sub-phase | 2.2 / 1 | 5 | agent-1:opus | scripts/fast_link_perf.sh NEW, ci.yml (passo perf nel job `fast-link`) | G-PERF 3 run su release, tutti PASS (rapporto 1.403 / 1.231 / 1.376; CPU server per GiB inferiore sul fast); T-FL-TRANSIT write 0 in tutti e tre; review validità: bracci interleaved, stessa sorgente tmpfs (/dev/shm), origin senza hash, stesso `BORE_PROXY_BUFFER_SIZE`, byte ricevuti verificati (fallimento rumoroso), `# done: SIZE` verificato, `LC_ALL=C`, campioni grezzi sempre stampati, pid del server = bore (process substitution); rev 5 sulla soglia RSS | unit:2.2:1 |
| sub-phase | 2.1 / 1 | 4 | agent-1:opus | scripts/fast_link_e2e.sh NEW, .github/workflows/ci.yml (job `fast-link`, passo e2e) | G-E2E: 13/13 PASS su release (E1–E12 del contratto + E13 file binario piccolo senza `Expect`). Difetti dello script trovati e corretti in corsa: `$!` di una funzione in background = subshell (kill non raggiungeva curl, E6); `bore | cat &` registrava il pid di cat → bore sopravviveva e `wait` sul job bloccava il cleanup per sempre (timeout 900 s). Difetti del server trovati dall'e2e: E11 (502 su PUT in chiaro → Host dalla sola head, commit fix(vhost)) ed E7 (annuncio dopo il replay → commit fix(fast-link)). Red-check E13 NEGATIVO (passa anche senza il fix: su TLS curl scrive head e body in record separati) → commento e FAST_LINK.md dichiarano che il discriminante è E11 + unit test | unit:2.1:1 |
| phase closure | P1 / 1 | 4 | agent-1:opus | STATE.md | G-FULL completo sul HEAD fd98aa7 (+ solo doc/CI/spec non tracciati, nessun codice): fmt 0, clippy 0, build 0, passo parallelo 0 (lib 928 + tutti i binari di test), doc 0, ssh_gateway 43 + spike 5, web_transfer_test seriale 0, npm 125/0, nodef 0; G-I1 7/7 + 1/1; review I-1 (hook solo con `fast_link` Some, sulla head già letta), I-SSH1 (loop di accept intatto, `git diff`), I-9 (308/403, D20 esteso), I-10 (nativo + SSH con messaggio esatto), D20; README verificato contro `bore server --help` | unit:P1:1 |
| sub-phase | 1.3 / 1 | 4 | agent-2:sonnet (worker), review agent-1:opus | tests/fast_link_test.rs NEW (T-FL-I1..I7), tests/ssh_gateway_test.rs (T-FL-I8), src/admin_ui/panels/metrics.js, test/admin_ui/metrics-fast-link.test.js NEW, .github/workflows/ci.yml (passo `npm test`), README.md (sezione) | fast_link_test 7/0 (loop 4x), t_ssh_fast_link 1/0, ssh_gateway_test 43/0 + spike 5/0, npm 125/0, vhost_test 53/0, lib 929/0, clippy/fmt ok. Review: il worker aveva tolto l'asserzione del messaggio SSH segnalando un bug; diagnosi del supervisore con strumentazione temporanea: NON un problema d'ordine ma lo svuotamento in `channel_open_session` scriveva con `session.data` prima della conferma del canale (accodata da `accept()`) → righe perse → fix nel commit `fix(ssh-gateway)` + helper `reject_line`, asserzione esatta ripristinata e red-checked; README: 4 inesattezze corrette (esempio di ripristino senza `<id>` e con `-u`, re-arm attribuito all'uploader, `hide_env_values` ≠ process list, id nei log = 4 caratteri) + tag docker neutro. Fix trovati dall'e2e (commit separati): Host dalla sola head (vhost, 502 su corpo binario nella stessa lettura, red-checked), annuncio `# download started` prima del replay (red-checked) | unit:1.3:1 |
| fix | smoke 1 / 1 | 4 | agent-1:opus | src/fast_link/session.rs, src/server.rs | primo run reale curl/wget (vhost HTTPS dedicato, 50 MB CL + tar in streaming chunked, SHA/`cmp` identici, exit 0): (a) il replay scritto fuori dalla pompa non era contato → `# done:` diceva 45805696 invece di 50000000, `bytes_total` e TX server sottostimati fino a 4 MiB → contato in `stream_handoff`, test `a_body_delivered_from_the_replay_is_counted` red-checked, asserzione esatta `# done: 10485760 bytes` in S1; (b) il 308 in chiaro usava la porta dell'header Host (porta HTTP) → `FastLink::set_https_port` impostata da `set_fast_link` (frontend vhost HTTPS se serve, altrimenti control port), caso 8443 nel test S11; verificati entrambi sul binario reale; lib 928/0 | commit senza trailer di unità (fix del supervisore) |
| sub-phase | 1.2 / 1 | 3 | agent-1:opus (implementato dal supervisore invece di delegare: unità piccola e critica per I-SSH1) | src/prefixed.rs (`ConnSecurity`), src/vhost.rs (`handle_http`/`handle_https` + hook), src/server.rs (bound, chiamanti frontend con `Some(permit)`, hook su `serve_control_http_after_web`, controllo HTTPS unificato) | clippy/fmt/nodef ok; lib 927/0; `conn_security_is_static_per_type` + `set_fast_link_*` 2/2; vhost_test 53/0; I-SSH1: `git diff src/server.rs` nessuna riga del loop di accept del control port cambiata (solo firme dei 4 metodi e i due listener vhost dedicati, come da contratto); I-1: con `fast_link` None l'unica differenza in `handle_http`/`handle_https` è l'estrazione dell'host spostata prima della lettura della config (pura); ALPN: gli acceptor server non annunciano h2 → curl resta su HTTP/1.1; estensione D20: `https_port == control_port` senza TLS sul control port = nessun HTTPS (topologia unificata) + caso nel test | unit:1.2:1 |
| sub-phase | 1.1 / 1 | 3 | agent-2:sonnet (worker), review agent-1:opus | src/{vhost,server,sshgw,main,admin_api,admin_views}.rs, tests/admin_test.rs | G-U1 (4 filtri ≥1 test ciascuno), lib 926/0, admin_test 20/0, clippy/fmt/nodef ok; prova binario: `ENABLED=false` → warn per flag ignorata, `ENABLED=true` senza vhost → errore; review: ordine main (set_web_transfer → set_tls → set_vhost → set_fast_link → set_ssh_gateway → listen), riserva nativa + SSH prima di `peek_takeover`, nessuna credenziale nelle viste; fix del supervisore: controllo HTTPS di `set_fast_link` richiede un modo vhost che serva HTTPS (cert caricato con `mode: http` passava ma il fast host non era raggiungibile in HTTPS) + caso 2b nel test, red-checked; commento snapshot ConfigView corretto | unit:1.1:1 |
| phase closure | P0 / 1 | 3 | agent-1:opus | STATE.md | G-FMT ok, G-CLIPPY 0 warning, G-NODEF compila, G-U0 52/52, lib completa 923/0 (baseline 871 + 52); README invariato (`git diff 8d2032d4 -- README.md` vuoto); review di tutto `src/fast_link/` | unit:P0:1 |
| sub-phase | 0.4 / 1 | 3 | agent-2:sonnet (worker), review agent-1:opus | src/fast_link/session.rs NEW, mod.rs, pump.rs (tolto `allow(dead_code)`) | G-U0 52/52, loop 3x senza flake; review: stato D13 sotto lock mai attraverso `.await`, gauge idempotenti, re-arm solo con replay intatto (I-3), fallimenti senza terminatore (I-4), authority rev 2; fix del supervisore: scrittura head+replay al downloader e `# download started` limitate da `stall_timeout` (downloader che non legge bloccava l'upload per sempre) + nuovo test `a_downloader_that_never_reads_the_replay_is_bounded` red-checked; `debug_assert!(false)` su handoff chiuso → `warn!` (raggiungibile in una corsa); 4 deviazioni test accettate (S6 finestra 2 MiB/1 MiB, S7 duplex 32 KiB, S12 timeout reali brevi, read_link salta 100 Continue) | unit:0.4:1 |
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
| G-PERF run 1 | `bash scripts/fast_link_perf.sh` (release) | pass | raw vhost=1335.9,1389.4,1435.1 fast=1949.1,1887.4,1960.2 MiB/s; ratio 1.403; cpu s/GiB vhost 0.70,0.67,0.64 fast 0.56,0.59,0.57; transit write 0, RSS +21766144 (soglia allora 48 MiB) | b872eaf + script | 2026-09-25 |
| G-PERF run 2 | idem | pass | raw vhost=1258.0,1530.7,1488.4 fast=1439.0,1831.8,2101.4; ratio 1.231; transit write 0, RSS +45932544 (→ rev 5) | idem | 2026-09-25 |
| G-PERF run 3 | idem, soglia rev 5 | pass | raw vhost=1539.3,1684.7,1189.5 fast=2117.7,2240.0,1838.7; ratio 1.376; cpu vhost 0.59,0.53,0.77 fast 0.50,0.50,0.57; transit write 0, RSS +26480640 ≤ 100971520 | idem | 2026-09-25 |

| Review | Reviewer | Plan revision / reviewed change | Invariants/assertions checked | Verdict |
|--------|----------|---------------------------------|------------------------------|---------|
| plan readiness | agent-1:opus | rev 1 | checklist Plan readiness; cold read di 0.4 | READY |
| 0.4 | agent-1:opus | rev 3, diff src/fast_link/session.rs | D13 (claim solo downloader, uploader unico a uscire da Streaming, InFlight atteso ≤ HANDOFF_RECV_TIMEOUT), I-3, I-4, I-7, I-8, nessun lock attraverso await, gauge | APPROVED (dopo fix scrittura limitata) |
| 0.3 | agent-1:opus | rev 3, diff src/fast_link/pump.rs | D8 due task, zero alloc, cancel prima di drop full_tx, flush dopo write, resume_unwind, coalescenza now_or_never | APPROVED (dopo rev 3) |
| 0.2 | agent-1:opus | rev 2, diff src/fast_link/framing.rs | stati SizeDigits..Done, forward prefisso, overflow, LF nudo, trailer | APPROVED |
| 0.1 | agent-1:opus | rev 1, diff src/fast_link/{mod,request,response}.rs | D19/D20 messaggi e ordine, nessuna credenziale in Debug/errori, regole parse/target/preview | APPROVED (nota authority → rev 2) |

## 8. Technical revisions and deviations

| Revision | Previous decision/step | Approved replacement and reason | Supervisor | Dependents/revalidation |
|----------|------------------------|---------------------------------|------------|------------------------|
| 5 | 2.2 T-FL-TRANSIT: crescita RSS ≤ 48 MiB fissi | soglia = formula di I-2 (finestra replay 4 MiB + (PUMP_DEPTH 4 + 1 buffer d'attesa) × `proxy_buffer_size()` + 16 MiB di margine) = 96 MiB con il 16M imposto dallo stesso gate: 48 MiB stava SOTTO la formula (run passato a 45.9 MiB → flaky per costruzione); resta ≪ payload 1 GiB, che è ciò che dimostra il transito | agent-1:opus | solo 2.2 |
| 4 | D9: redirect 308 verso `https://<authority><target>` con authority dall'header Host | redirect verso `config.host` + porta HTTPS configurata (omessa se 443): l'header Host di una richiesta in chiaro porta la porta HTTP, mai quella HTTPS (smoke test reale) | agent-1:opus | 0.4 (già chiusa, test S11 esteso), 1.1 (`set_fast_link` imposta la porta) |
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
| 0.4 | phase_01.md | 0.1, 0.2, 0.3 | DONE | 1 | G-U0 52/52, red-check replay limitato, review ok |
| 1.1 | phase_02.md | P0 | DONE | 1 | G-U1 verdi, red-check modo vhost, review ok |
| 1.2 | phase_02.md | 1.1 | DONE | 1 | G-U1, vhost_test 53/0, I-SSH1 diff ok |
| 1.3 | phase_02.md | 1.1, 1.2 | DONE | 1 | G-I1 8/8, G-NPM 125/0, review ok |
| 2.1 | phase_03.md | P1 | DONE | 1 | G-E2E 13/13 su release |
| 2.2 | phase_03.md | 2.1 | DONE | 1 | G-PERF 3/3 PASS, ratio 1.23–1.40 |
| 2.3 | phase_03.md | P1 | DONE | 1 | G-PW 1 passed, 2 skipped motivati |
| 2.4 | phase_03.md | 2.1, 2.2, 2.3 | DONE | 1 | doc verificati contro codice e script |

README obligation per phase: 0 → nessuna sezione cambia (verifica a P0); 1 → 1.3 S5 crea "Fast link transfer"; 2 → 2.4 S2 completa.

### Phases
| ID | File | Closure unit | Status | Review / commit reference |
|----|------|--------------|--------|---------------------------|
| 0 | phase_01.md | P0 | DONE | review agent-1:opus; unit:P0:1 |
| 1 | phase_02.md | P1 | DONE | review agent-1:opus; unit:P1:1 |
| 2 | phase_03.md | P2 | IN_PROGRESS | — |

Statuses: TODO, IN_PROGRESS, IN_REVIEW, DONE, SKIPPED, BLOCKED.

### Tests
| ID/name | Owning unit | Gate | Status | Evidence |
|---------|-------------|------|--------|----------|
| resolve_* / generate_id / parse_* / upload_* / download_target / preview / host_matches / response_bytes_exact | 0.1 | G-U0 | PASS | 19/19 |
| cl_* / chunked_* / error_is_sticky | 0.2 | G-U0 | PASS | 7/7 (26 totali) |
| pump_* (10) incl. red-check `pump_writes_are_flushed_before_waiting`, `pump_coalesces_small_reads` | 0.3 | G-U0 | PASS | 10/10, loop 5x senza flake |
| T-FL-S1..S12, S14..S16 + S7b `a_downloader_that_never_reads_the_replay_is_bounded` | 0.4 | G-U0 | PASS | 16/16 (52 totali), loop 3x |
| reserved_label_reason_*, set_fast_link_*, config_and_metrics_publish_fast_link_*, server_fast_link_flags_* | 1.1 | G-U1 | PASS | 4/4 |
| conn_security_is_static_per_type | 1.2 | G-U1 | PASS | 1/1 |
| T-FL-I1..I7, t_ssh_fast_link_label_is_reserved, metrics-fast-link.test.js | 1.3 | G-I1, G-NPM | PASS | 7/7 + 1/1 + 5/5 |
| T-FL-E1..E13 | 2.1 | G-E2E | PASS | 13/13 (release) |
| T-FL-PERF, T-FL-TRANSIT | 2.2 | G-PERF | PASS | 3/3 run, §7 |
| T-FL-PW | 2.3 | G-PW | PASS | chromium 1/1, firefox/webkit skip |

### Documentation
| Document/sections | Owning unit | Status | Evidence |
|-------------------|-------------|--------|----------|
| README "Fast link transfer" | 1.3 (crea), 2.4 (completa) | DONE | sezione + troubleshooting + rimando a FAST_LINK.md |
| docs/transfer/FAST_LINK.md | 2.4 | DONE | 14 sezioni, numeri misurati |
| docs/README.md indice | 2.4 | DONE | voce in `transfer/` |
| CLAUDE.md Key invariants | 2.4 | DONE | bullet "Fast link transfer" |

### Audits
| Report | Verdict | Current unresolved findings | Evidence |
|--------|---------|-----------------------------|----------|
| — | — | — | — |

## 12. Suspended implementation (only while auditing unfinished work)

Snapshot state: none.
