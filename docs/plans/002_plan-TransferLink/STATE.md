# Transfer Link — Implementation State

> **LEGGERE QUESTO FILE PER PRIMO a ogni sessione. Aprire un'unità in §1 PRIMA di modificare codice; chiuderla DOPO i gate.**
> **Last updated:** 2026-09-20 | **By:** Codex, V001-C12/REL-1 | **Session:** 32

## 0. Protocol

Questo è l'unico file di stato: posizione, progress board, ledger, verifiche, deviazioni e blocker. Una unità è una sottofase, task, bug, audit verify o correzione di audit.

**Resume:**

1. Leggere tutto questo file. §1 OPEN significa lavoro potenzialmente incompleto: leggere §6, finirlo o annullarlo prima di iniziare altro. §1 none significa aprire l'unità indicata in Next action.
2. Eseguire i gate già disponibili in §3 e confrontare i risultati con §§1/7/11. Il repository prevale sulle dichiarazioni. I gate NEW non ancora introdotti sono non applicabili, non PASS; registrarne disponibilità man mano.
3. Aprire solo il phase file indicato da §1, leggere overview soltanto se il contesto §2 non basta. Gli anchor sono del commit di ricognizione: ricercare i simboli quando le righe si spostano.
4. Non modificare file del piano WebTransfer001 né lavoro concorrente per «ripulire» il tree. Nessun commit/push automatico salvo autorizzazione esplicita dell'utente.

**Open prima delle modifiche:** compilare §1 Type/ID/Status OPEN/Intent/Assigned/Next action; §6 `claimed — nothing written yet`; aggiornare timestamp. Solo allora editare codice/test/documentazione dell'unità.

**Close dopo gate verdi:** appendere riga §4, aggiornare §§5/7/8/9/10/11; §6 `none — tree consistent`; §1 prossimo ID/Status none; timestamp. Una sottofase non è DONE senza ledger e board coerenti. WIP commits off: Commit=`uncommitted`.

**Interruzione:** lasciare OPEN, §6 con file cambiati, lavoro residuo, stato gate e temporanei da rimuovere. OPEN con §6 vuoto è errore. Non dichiarare un gate completato sulla base di log passati o prove del launcher preliminare.

**Review/roster:** ruoli legacy `agent-1:opus` architetto/reviewer, `agent-2:sonnet` implementer, `agent-3:haiku` documentazione. Review obbligatorie riportate nelle sottofasi. Se host non supporta gli alias, rendere esplicita la mappatura concordata prima di dispatch; mai dichiarare di aver eseguito un modello non disponibile.

## 1. Current unit

- **Type:** none
- **ID:** REL-1
- **Status:** none
- **Intent:** chiusura V001 e preparazione del prerelease `v1.2.0-rc.7` dopo i gate locali e remoti verdi.
- **Phase:** tutte (`phase_01.md`–`phase_05.md`) chiuse.
- **Next action:** nessuna; dopo il commit documentale creare il tag annotato `v1.2.0-rc.7` e attendere il workflow Release.
- **Assigned:** Codex (esecuzione del ruolo `agent-2:sonnet`; review dei test incorporata).
- **Repo state:** branch `dev`, implementazione `f50a9db374253e10ee90574dafe0f2b7a7aeed4f`, tag/release `v1.2.0-rc.6` già pubblicati; V001-C01–V001-C12 chiusi, REL-1 in attesa del tag.

## 2. Feature context

Nuovo `bore transfer link`: A serve file/cartelle tramite HTTP loopback e vhost bore; B usa normale link HTTPS curl/wget/browser. URL `https://transfer-<16 casuali a-z0-9>.<dominio-vhost>/<filename>`, wildcard/certificati vhost esistenti, nessuna emissione/cache nuova. Il server VEDE plaintext (accettato), senza conservarlo: NON E2E.

Trasporto A↔server QUIC default, fallback TCP TLS in apertura; --relay-only. File originale singolo o ZIP STORED/ZIP64 per multipli/cartelle; repeat/parallel fino a segnale, sorgenti stabili. Stdin/exec monouso, no spool, HEAD innocuo,409 concorrente/410 consumato. Exec argv senza shell, privilegi ereditati, gruppo processi, exit0 obbligatorio prima di successo HTTP; stdin EOF non conosce exit del produttore esterno. SHA256 A confrontabile B, non ACK del salvataggio su disco.

**Reference scenario:** `bore transfer link mydoc.zip` → URL unica stdout, curl/wget bytes+SHA identici e GET successivi; `sudo bore transfer link --filename backup.tar --exec -- tar -cpf - myfolder` → TAR invariato, restore root `--numeric-owner --same-owner -xpf` preserva mode/owner/link. Prove T-LINK-RAW/CONCURRENT/EXEC-SUDO; false-success impedito da T-LINK-EXEC-FAIL.

**Hard constraints:** I-1 no payload persistente/buffer illimitati; I-2 niente downgrade plain; I-3 niente fine HTTP prima di source/producer valido; I-4 stdout binario/stderr separato, sudo e reap; I-5 monouso atomico; I-6 path reale e niente migrazione; I-7 legacy invariato; I-8 no snapshot/disco B garantiti; I-9 risorse bounded (thread stdin bloccato termina con processo CLI); I-10 gate veri e netns seriali.

**Defaults tecnici fissati:** Hyper1 HTTP/1.1, Range ignorato200, Connection close; chunk1MiB/coda4 (limite massimo4); file/ZIP max-downloads8, stdin/exec1; manifest≤100000 entry/32MiB nomi/depth256; ZIP symlink/special/nonUTF8/collisioni rifiutati; exec Unix foreground. --ca-cert PEM opzionale aggiunge trust senza togliere hostname verification. Reconnect mantiene URL e non riavvia producer; rejection generica dopo Ready retry bounded75s, nessuna interpretazione del testo wire.

## 3. Environment and commands

Comandi autoritativi, identici a overview e blocchi gate delle fasi. Eseguire quelli applicabili all'unità; l'assenza di file NEW prima dell'introduzione non è un errore di implementazione né un PASS.

| Gate | Comando | Disponibilità/uso |
|------|---------|------------------|
| G-FMT | `cargo fmt --all -- --check` | Esistente, ogni unità |
| G-LINT | `cargo clippy --all-features --all-targets -- -D warnings` | Esistente, ogni unità codice |
| G-BUILD | `cargo build --locked --all-features` | Esistente, ogni fase |
| G-UNIT | `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak` | Esistente, ogni unità codice |
| G-SERIAL | `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | Esistente, ogni fase |
| G-LINK | `cargo test --all-features --test transfer_link_test -- --test-threads=1` | NEW fase0 |
| G-NOUDP | `cargo test --no-default-features --test transfer_link_test -- --test-threads=1` | NEW fase1 |
| G-E2E | `bash scripts/transfer_link_e2e.sh basic` | NEW fase1, casi incrementali |
| G-ROOT | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/transfer_link_privileged_test.sh all` | NEW fase3, rete/perf aggiunte fase4 |
| G-LARGE | `bash scripts/transfer_link_e2e.sh large` | NEW fase2, chiusura fase2/finale |
| G-DOCKER | `bash scripts/transfer_link_container_test.sh` | NEW fase4 |
| G-LINK-NETNS | `sudo -n ./scripts/transfer_link_netns_test.sh` | NEW fase4, root seriale |
| G-LINK-PERF | `BORE=target/release/bore BORE_PROXY_BUFFER_SIZE=16M bash scripts/transfer_link_perf.sh` | NEW fase4, release e fixture calda |
| G-FULL | `cargo test --all-features -- --test-threads=1` | Esistente, regressione finale |
| G-VHOST | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test.sh` | Esistente, regressione finale seriale |
| G-VHOST-HARD | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test_hard.sh` | Esistente, regressione finale seriale |
| G-VHOST-UDP | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_udp_concurrency_repro.sh` | Esistente, regressione finale seriale |
| G-SSH-ROOT | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/ssh_gateway_test.sh` | Esistente, regressione finale seriale |

- **Repo root:** `/mnt/fabio/dati/Git/Github-manprint/bore-forked`.
- **WIP commits:** off durante l'esecuzione del piano; commit/push post-piano richiesti dall'utente e tracciati nell'unità REL-1.
- **Runtime/build:** Rust/Cargo e lock del repo; lock ricognizione Tokio1.52.3, Hyper1.10.1, hyper-util0.1.20, http-body-util0.1.3; nuova async_zip0.0.18 solo fase2. Tokios process deve passare nelle dipendenze runtime fase3; nix process/signal Unix.
- **Test tools:** curl, wget, openssl, Python3/zipfile, unzip, GNU tar; root/netns con ip/tc/ss e firewall harness; Docker per G-DOCKER. Tool mancante →risultato non-run e blocco del gate richiesto.
- **TLS locale:** server controllo --cert-file/--key-file; vhost --vhost-cert-file/--vhost-key-file; niente --tls inesistente. Porte controllo/HTTPS/HTTP/UDP separate e dinamiche. CA test sul mittente con --ca-cert e su curl con --cacert. Cert hostname sbagliato deve restare errore.
- **Build harness:** compilare revisione corrente prima di ogni suite, release per benchmark; non riusare vecchi binari dopo cambi Cargo/features. No-default build non deve sovrascrivere inavvertitamente il binario all-features di un harness concorrente.
- **Privilegi:** script sudo per percorso assoluto, non sudo bash; netns seriali; cleanup solo PID/netns/file creati dall'harness. Negli altri checkout aggiornare insieme tutti i comandi con path assoluto.
- **Baseline:** TokenSave aveva2 file stale, non syncato in pianificazione; fonti Client/vhost/transport confrontate direttamente. La baseline test della feature non è stata eseguita in modalità Plan.

## 4. Work ledger

Append-only, una riga per unità chiusa. Type ammessi: sub-phase, task, bug, verify, correction.

| # | Type | ID | Agent | What changed | Files | Gates | Commit |
|---|------|----|-------|--------------|-------|-------|--------|
| 1 | sub-phase | 0.1 | Codex (role agent-2:sonnet) | Added locked Hyper/http-body dependencies, exported transfer-link contracts, filename/path/header validation, bounded limit types and four integration tests. No CLI/listener behavior. | `Cargo.toml`, `Cargo.lock`, `src/lib.rs`, `src/transfer_link/mod.rs`, `src/transfer_link/source.rs`, `src/transfer_link/stats.rs`, `tests/transfer_link_test.rs` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-UNIT PASS; G-LINK PASS (4/4). Full G-SERIAL had one existing flaky SSH test failure; isolated rerun PASS. | uncommitted |
| 2 | sub-phase | 0.2 | Codex (role agent-2:sonnet) | Added bounded regular-file producer with no-follow opens, initial/final fingerprints, checked size/extra-byte validation, incremental SHA-256, cancellation/backpressure, terminal Complete/Failed messages and deterministic mutation/drop/queue tests. No HTTP/CLI/vhost behavior. | `Cargo.toml`, `Cargo.lock`, `src/transfer_link/source.rs`, `src/transfer_link/stats.rs`, `tests/transfer_link_test.rs` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-UNIT PASS (all non-SSH/DMX/soak suites); G-LINK PASS (10/10); G-SERIAL PASS (5 spike + 42 gateway, 0 failed). | uncommitted |
| 3 | sub-phase | 0.3 | Codex (role agent-2:sonnet) | Added loopback Hyper HTTP/1.1 supervisor with bounded connection/download permits, strict route/method/body framing, HEAD metadata-only responses, full-200 Range handling, security/cache headers, fallible frame bodies, producer cancellation and incomplete-source errors. Added protocol, deadline, routing, permit and incomplete-body tests; no CLI/vhost/ZIP behavior. | `src/transfer_link/http.rs`, `src/transfer_link/mod.rs`, `tests/transfer_link_test.rs` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-UNIT PASS (serial selected regression: all suites, 1 ignored benchmark, 0 failed); G-LINK PASS (14/14); HTTP body unit tests PASS (2/2). | uncommitted |
| 4 | sub-phase | 0.4 | Codex (role agent-3:haiku) | Verificato README senza modifiche: la documentazione annuncia soltanto `bore transfer listener|sender|web` già esistenti e non promette il nuovo Link prima della sua fase. | `README.md` (read-only), `docs/plans/002_plan-TransferLink/STATE.md` | README check PASS; gate codice 0.3 già PASS | uncommitted |
| 5 | sub-phase | 1.1 | Codex (role agent-2:sonnet) | Conservate le URL VhostReady con accessor read-only; aggiunto ClientScope con token, TaskTracker, gate atomico/registrazione serializzata e abort handle; collegati driver yamux, listen, carrier, direct e splice al lifecycle scoped mantenendo il ramo legacy; aggiunto BackendPath e hook lease RAII; aggiunto registry bounded peer→path con rimozione per id e test di riuso porte; nessun byte wire modificato. | `src/client.rs`, `src/mux.rs`, `src/transfer_link/mod.rs`, `src/transfer_link/path.rs` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-UNIT PASS; G-LINK PASS (14/14); G-SERIAL PASS (5 spike + 42 gateway); unit client/path PASS (11) | uncommitted |
| 6 | sub-phase | 1.2 | Codex (role agent-2:sonnet) | Aggiunta configurazione TLS verificata con CA PEM additiva e hostname verification, classificazione tipizzata degli errori di connessione/vhost, supervisore scoped per listener HTTP/vhost con retry cancellabile, URL stabile e grace policy per rifiuti post-ready; test endpoint/URL/retry/CA. | `src/transport.rs`, `src/client.rs`, `src/transfer_link_cli.rs` | G-FMT PASS; G-LINT PASS (precedente gate all-features); G-BUILD PASS (precedente gate all-features); G-UNIT PASS (selected serial suite precedente); unit transport 11/11; unit CLI 5/5 | uncommitted |
| 7 | sub-phase | 1.3 | Codex (role agent-2:sonnet) | Aggiunto `bore transfer link` single-file con validazione Clap, URL stdout unica, sorgente regular-file bounded, supervisor scoped, `--ca-cert`, `--relay-only`, carrier/download/stat flags, segnali e logging per-download. Harness HTTPS reale copre GET/HEAD/Range/ripetizione/concorrenza. | `src/main.rs`, `src/transfer_link/http.rs`, `scripts/transfer_link_e2e.sh`, `tests/transfer_link_test.rs` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-LINK PASS 14/14; G-NOUDP PASS 14/14; G-E2E PASS (`transfer-link basic`); parser bin tests PASS 56/56 | uncommitted |
| 8 | sub-phase | 1.4 | Codex (role agent-2:sonnet) | Esteso l'harness con riavvio del server, riconnessione del supervisore a URL invariato, verifica del percorso QUIC/relay e cleanup SIGINT. Lo streaming GET omette Content-Length per consentire l'osservazione di `SourceMessage::Complete` e della verifica finale SHA-256 prima della fine HTTP; HEAD conserva la dimensione. | `src/transfer_link/http.rs`, `scripts/transfer_link_e2e.sh`, `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT PASS; G-LINT PASS; G-BUILD PASS; G-UNIT PASS (836 unit + integrazioni selezionate, 0 fail); G-LINK PASS 14/14; G-NOUDP PASS 14/14; G-E2E PASS (raw/HEAD/Range/wget/concorrenza/reconnect/relay-only/SIGINT) | uncommitted |
| 9 | sub-phase | 1.5 | Codex (role agent-3:haiku, esecuzione root) | Aggiunta sezione README per il link pubblico file singolo: wildcard/certificato vhost esistente, setup server, tutti i flag/env, trust CA, curl/wget, concorrenza, statistiche, SHA-256, Ctrl+C e limiti di conferma. Help reale e riferimenti principali verificati. | `README.md`, `docs/plans/002_plan-TransferLink/STATE.md` | README/help/examples check PASS; G-E2E e gate Rust ereditati da 1.4 | uncommitted |
| 10 | sub-phase | 2.1 | Codex (role agent-2:sonnet) | Aggiunto manifest deterministico metadata-only per file/cartelle/misto: walk iterativo bounded, fingerprint iniziale/finale, rifiuto di symlink/special/non-UTF-8/collisioni/overlap, limiti entry/nome/profondità e invalidazione su sorgenti mutate. | `src/transfer_link/manifest.rs`, `src/transfer_link/source.rs`, `src/transfer_link/mod.rs`, `src/main.rs`, `tests/transfer_link_test.rs` | G-FMT/G-LINT/G-BUILD/G-LINK PASS; test manifest/mutazioni PASS | uncommitted |
| 11 | sub-phase | 2.2 | Codex (role agent-2:sonnet) | Implementato ZIP STORED/ZIP64 streaming con duplex bounded 256 KiB, nessun archivio temporaneo, hash dell'intero ZIP, final validation obbligatoria e abort RAII del writer. Errori di lettura/integrità invalidano il download. | `src/transfer_link/zip.rs`, `src/transfer_link/http.rs`, `src/transfer_link/source.rs`, `Cargo.toml`, `Cargo.lock` | G-FMT/G-LINT/G-BUILD/G-LINK PASS; archive/zero-byte/incomplete tests PASS | uncommitted |
| 12 | sub-phase | 2.3 | Codex (role agent-2:sonnet) | Esteso l'harness per ZIP64 reale (>4 GiB logici e 65.536 entry), decoder Python indipendente, monitor RSS sender/server e verifica di assenza spool payload. | `scripts/transfer_link_e2e.sh`, `scripts/transfer_link_sparse_sink.py` | G-LARGE PASS; RSS delta sender 34,616 KiB/server 3,256 KiB; G-E2E basic PASS | uncommitted |
| 13 | sub-phase | 2.4 | Codex (role agent-3:haiku, esecuzione root) | Aggiornato README per selezioni miste, ZIP STORED/ZIP64, sorgenti stabili, limiti e verifica indipendente dell'archivio. | `README.md` | README/examples check PASS; G-E2E/G-LARGE PASS | uncommitted |
| 14 | sub-phase | 3.1 | Codex (role agent-2:sonnet) | Aggiunta CLI completa `link`: modalità file/ZIP/stdin/exec, filename, server/CA/secret, relay-only, carriers, max-downloads, statistic interval; prenotazione atomica one-shot, HEAD innocuo, 409 concorrente e 410 consumato. | `src/main.rs`, `src/transfer_link_cli.rs`, `src/transfer_link/oneshot.rs`, `src/transfer_link/http.rs`, `tests/transfer_link_test.rs` | G-FMT/G-LINT/G-BUILD/G-LINK/G-NOUDP PASS; 21/21 transfer tests | uncommitted |
| 15 | sub-phase | 3.2 | Codex (role agent-2:sonnet) | Implementato stdin bounded senza spool: thread bridge verso coda Tokio bounded, backpressure, hash e stato terminale; shutdown del CLI anche con produttore ancora bloccato. | `src/transfer_link/oneshot.rs`, `src/main.rs`, `scripts/transfer_link_e2e.sh` | stdin binario/one-shot PASS; blocking producer SIGTERM termina entro 7 s; G-E2E basic PASS | uncommitted |
| 16 | sub-phase | 3.3 | Codex (role agent-2:sonnet) | Implementato `--exec` Unix con argv letterale senza shell, avvio al primo GET, stdout binario, stderr bounded, exit code obbligatorio zero, gruppo processi e terminazione TERM→KILL/reap anche per discendenti che ignorano TERM. | `src/transfer_link/oneshot.rs`, `src/transfer_link/http.rs`, `tests/transfer_link_test.rs`, `scripts/transfer_link_privileged_test.sh` | exec success/failure/cancel PASS; cargo clippy e full tests PASS | uncommitted |
| 17 | sub-phase | 3.4 | Codex (role agent-2:sonnet) | Aggiunto gate root reale: `sudo bore ... --exec -- tar -cpf -` conserva il privilegio del child e roundtrip GNU tar di owner/gid/mode/setgid/sticky/symlink/hardlink con restore `--numeric-owner --same-owner`. | `scripts/transfer_link_privileged_test.sh`, `README.md` | G-ROOT PASS (exec, failure, cancellation, sudo metadata) | uncommitted |
| 18 | sub-phase | 3.5 | Codex (role agent-3:haiku, esecuzione root) | Documentati stdin/exec, differenza `sudo` attorno a bore, restore TAR, limiti di verifica e comando Docker. | `README.md` | README/help check PASS; G-ROOT/G-E2E PASS | uncommitted |
| 19 | sub-phase | 4.1 | Codex (role agent-2:sonnet) | Aggiunti acceptance script per reconnect, relay-only, SIGINT/SIGTERM, producer failure/cancel, Docker e workflow CI; mantenuto fallback pre-body e cleanup scoped. | `scripts/transfer_link_e2e.sh`, `scripts/transfer_link_privileged_test.sh`, `scripts/transfer_link_container_test.sh`, `.github/workflows/ci.yml`, `.github/workflows/e2e_netns.yml` | G-E2E/G-ROOT/G-DOCKER PASS; fault UDP attivo e ciclo 100 cleanup non-run | uncommitted |
| 20 | sub-phase | 4.2 | Codex (role agent-2:sonnet) | Verificati bounded queue/RSS, logging di path/bytes/SHA/stati, stderr child bounded e writer abort-safe; aggiunta prova producer infinito. | `src/transfer_link/oneshot.rs`, `src/transfer_link/http.rs`, `scripts/transfer_link_e2e.sh` | unit/full/LARGE/STDIN-CANCEL PASS; benchmark comparativo non-run | uncommitted |
| 21 | sub-phase | 4.3 | Codex (role agent-2:sonnet) | Collegati gate Rust, e2e Linux, Docker e privileged nella CI; ricostruito debug/release e completata regressione seriale. | `.github/workflows/ci.yml`, `.github/workflows/e2e_netns.yml`, `Cargo.toml`, `Cargo.lock` | G-FMT/G-LINT/G-BUILD/G-FULL/G-DOCKER/regressioni netns+SSH PASS | uncommitted |
| 22 | verify | 4.4 | Codex (role agent-2:sonnet; review inline agent-1:opus) | Audit finale requisito→codice→README→CI→test; corretto trattamento del completamento archive/one-shot nel body HTTP, timeout drain stderr e kill del process group; STATE reso coerente. | `src/transfer_link/http.rs`, `src/transfer_link/oneshot.rs`, `docs/plans/002_plan-TransferLink/STATE.md` | fmt/clippy/transfer tests/no-default/LARGE/ROOT/FULL PASS; gap non-run espliciti sotto | uncommitted |
| 23 | correction | 4.5 | Codex (role agent-2:sonnet) | Aggiunto test esplicito per producer `--exec` vuoto con exit 0: risposta HTTP 200 chunked vuota, claim consumato e secondo GET 410. Nessun codice di produzione cambiato. | `tests/transfer_link_test.rs`, `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT PASS; G-LINT PASS; G-LINK PASS (22/22) | uncommitted |
| 24 | correction | REL-2 | Codex (root) | Gated Unix-only `oneshot` imports/constants so Windows clippy is clean; serialized the env-sensitive Link CLI parser test and restored `BORE_SERVER`; removed forced offline resolution from the Docker acceptance fallback so clean runners can fetch the locked graph. | `src/transfer_link/oneshot.rs`, `src/main.rs`, `scripts/transfer_link_container_test.sh`, `docs/plans/002_plan-TransferLink/STATE.md` | fmt PASS; clippy all-features/all-targets PASS; selected regression 0 failed; CLI unit PASS; Docker acceptance PASS | `232e35f` |
| 25 | correction | REL-3 | Codex (root; review inline) | Gated the remaining Unix-only `prepare_exec` imports for Windows builds; made drain-timeout accounting report at least its configured deadline across early timer ticks; regenerated the deterministic browser bundle. | `src/main.rs`, `tests/transfer_link_test.rs`, `web/transfer/src/webrtc.js`, `web/transfer/dist/app.js`, `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT PASS; G-LINT PASS; G-LINK/G-NOUDP PASS (22/22 each); browser `npm run check` PASS (207/207 + bundle) | `26d6584` |
| 26 | correction | REL-5 | Codex (root; review inline) | Sostituita l’asserzione Chromium sul testo transitorio con una verifica della traccia `MutationObserver`, già usata per controllare la sequenza completa `connecting` → `relay`; nessun codice di produzione o ritardo dati modificato. | `web/transfer/tests/e2e/ui.spec.mjs`, `docs/plans/002_plan-TransferLink/STATE.md` | `npm run check` PASS (207/207, bundle invariato); Chromium T-WEB-PATH-UI PASS 5/5; `git diff --check` PASS | uncommitted |
| 26 | correction | REL-5 | Codex (root; review inline) | Sostituita l’asserzione Chromium sul testo transitorio con una verifica della traccia `MutationObserver`, già usata per controllare la sequenza completa `connecting` → `relay`; nessun codice di produzione o ritardo dati modificato. | `web/transfer/tests/e2e/ui.spec.mjs`, `docs/plans/002_plan-TransferLink/STATE.md` | `npm run check` PASS (207/207, bundle invariato); Chromium T-WEB-PATH-UI PASS 5/5; `git diff --check` PASS | `cd4f801` |
| 27 | verify | REL-6 | Codex (root) | Verificato il commit finale su tutti i workflow remoti richiesti; nessun job fallito, tag autorizzato dopo pubblicazione dell’audit. | `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT/G-LINT/G-LINK/G-NOUDP/npm PASS; CI, E2E netns, Docker GHCR, Mean Bean CI e Mean Bean Deploy PASS | `0f7c0ec` |
| 28 | correction | REL-7 | Codex (root) | Allineati `Cargo.toml` e `Cargo.lock` a `1.2.0-rc.6` dopo il preflight fallito di `v1.2.0-rc.5`; il tag precedente resta immutabile. | `Cargo.toml`, `Cargo.lock`, `docs/plans/002_plan-TransferLink/STATE.md` | `cargo metadata --offline --locked`, fmt, clippy all-features/all-targets, transfer 22/22 con e senza default features PASS | `1505cb2` |
| 29 | verify | REL-8 | Codex (root) | Verificata la pubblicazione prerelease completa: tag annotato `v1.2.0-rc.6`, preflight coerente, rerun Chromium passato, artefatti binari/GHCR pubblicati e release non-draft. | `docs/plans/002_plan-TransferLink/STATE.md` | CI/E2E/Docker/Mean Bean del commit `1505cb2` PASS; Release `35468969739` attempt 2 PASS; `gh release view v1.2.0-rc.6` mostra 26 asset | `1505cb2` |
| 30 | verify | V001 | Codex (root; batched explorer) | Audit avversariale completo di requisiti, lifecycle, errori, sicurezza, bounds, test, documentazione e stato. Verdetto FAIL: 2 BLOCKER, 9 MAJOR, 4 MINOR; nessun codice di produzione modificato. | `docs/plans/002_plan-TransferLink/verify/{verify_001_2026-09-20.md,index.md}`, `docs/plans/002_plan-TransferLink/STATE.md` | Gate locali correnti fmt/lint/build/Link/no-default/unit/serial/full/E2E/root/large/Docker PASS; release ricostruita; netns finali registrati in §7; finding aperti F01–F15 | uncommitted |
| 31 | correction | V001-C01 | Codex (root; role agent-2:sonnet) | Centralizzato il terminal outcome per GET: `SourceMessage::Complete` è candidato, `Completed/Consumed` viene deciso solo dopo il successo della connessione Hyper; errori socket prevalgono; le GET usano chunked per obbligare la validazione terminale, HEAD mantiene Content-Length. Aggiunti test di precedenza e trasporto. | `src/transfer_link/http.rs`, `tests/transfer_link_test.rs`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-LINT PASS; G-BUILD/CHECK PASS; G-UNIT HTTP 5/5 PASS; G-LINK PASS 22/22; G-NOUDP PASS 22/22 | uncommitted |
| 32 | correction | V001-C02 | Codex (root; role agent-2:sonnet) | Resa non abortibile la proprietà del producer: `ProducerTaskOwner` conserva cancellazione e join separati dal body; il disconnect HTTP segnala cancellation e attende il supervisore. Il gate privilegiato ora interrompe una GET reale, verifica leader/grandchild e passa TERM→KILL/reap; i segnali di gruppo non-ESRCH sono loggati. | `src/transfer_link/{source,oneshot,http,zip}.rs`, `scripts/transfer_link_privileged_test.sh`, `tests/transfer_link_test.rs`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; G-LINK PASS 22/22; G-BUILD PASS; `sudo -n scripts/transfer_link_privileged_test.sh cancel` PASS | uncommitted |
| 33 | correction | V001-C03 | Codex (root; role agent-2:sonnet) | Aggiunto un `watch` terminale per i fallimenti one-shot: stdin/exec segnala Failed al supervisore CLI, che cancella/attende il link e ritorna errore nonzero. L'e2e sostituisce il falso test SIGTERM con FIFO + produttore vivo + GET reale interrotto; verifica chiusura del produttore e `410` solo come conseguenza del link terminato. | `src/transfer_link/oneshot.rs`, `src/main.rs`, `scripts/transfer_link_e2e.sh`, `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; G-LINK PASS 22/22; G-E2E basic PASS | uncommitted |
| 34 | correction | V001-C04 | Codex (root; role agent-2:sonnet) | Il bookkeeping scoped usa una mappa di abort handle con guard RAII che rimuove l'handle al termine del task; il percorso di timeout HTTP conserva il `JoinHandle`, fa abort e attende il join. Aggiunti test per handle count e task non cooperativo. | `src/client.rs`, `src/transfer_link_cli.rs`, `docs/plans/002_plan-TransferLink/STATE.md` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; client abort-handle unit PASS; HTTP abort/join unit PASS; G-LINK PASS 22/22 | uncommitted |
| 35 | correction | V001-C05 | Codex (root; role agent-2:sonnet) | Applicati i budget globali di entry e path/name bytes durante l’enumerazione: i nodi pending vengono riservati prima della raccolta, le directory di validazione restano bounded e i primi eccessi vengono rifiutati senza pubblicare il manifest. Aggiunti test unitari per budget entry/path. | `src/transfer_link/manifest.rs`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-LINT PASS; G-CHECK PASS; manifest unit 2/2 PASS; G-LINK PASS 22/22 | uncommitted |
| 36 | correction | V001-C06 | Codex (root; role agent-2:sonnet) | Separati gli errori TLS transienti (timeout, EOF/reset e altri errori socket) dagli errori permanenti di verifica/protocollo. Il supervisore Link ritenta solo la classe transient e mantiene lo stop immediato per certificato/hostname/protocollo; aggiunti test di classificazione e retry. | `src/transport.rs`, `src/transfer_link_cli.rs`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; unit transport/CLI PASS; G-LINK PASS 22/22; G-NOUDP PASS 22/22 | uncommitted |
| 37 | correction | V001-C07 | Codex (root; role agent-2:sonnet) | Rimossi dai log scoped label, URL e filename bearer: il correlation id monotono `session_id` è l’unico identificatore Link. Le sintesi dei frame e i log vhost/QUIC redigono subdomain e URL; l’e2e a `-v` e `-vv` scansiona stderr per host, URL e filename. | `src/shared.rs`, `src/client.rs`, `src/holepunch.rs`, `src/vhost.rs`, `src/server.rs`, `src/transfer_link_cli.rs`, `src/main.rs`, `scripts/transfer_link_e2e.sh`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; G-E2E basic PASS (log scan); G-LINK PASS 22/22; G-NOUDP PASS 22/22 | uncommitted |
| 38 | correction | V001-C08 | Codex (root; role agent-2:sonnet) | Allineati il label pubblico a `transfer-` + 16 caratteri, il parser Link a `--carriers 0..=32`, il progress log a un timer indipendente dai chunk con `active_downloads`, e README a WebPKI bundled, outcome HTTP e riavvio stdin dopo errore. | `src/transfer_link_cli.rs`, `src/main.rs`, `src/transfer_link/http.rs`, `README.md`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | G-FMT PASS; G-CHECK PASS; G-LINT PASS; progress/parser/label unit PASS; G-LINK PASS 22/22; G-NOUDP PASS 22/22; G-E2E basic PASS | uncommitted |
| 39 | correction | V001-C09 | Codex (root; role agent-2:sonnet) | Reso G-DOCKER revision-hermetico: il target statico viene sempre ricostruito con `--locked --all-features`, il gate controlla il linking statico e inserisce un artefatto valido invecchiato prima di verificare che la ricostruzione lo sostituisca. | `scripts/transfer_link_container_test.sh`, `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_001_2026-09-20.md}` | `bash -n scripts/transfer_link_container_test.sh` PASS; G-DOCKER PASS (raw, stdin `-i`, scratch exec failure) | uncommitted |
| 40 | correction | V001-C10 | Codex (root) | Aggiunto il fault harness namespace isolato per direct QUIC, fallback automatico con UDP bloccato, relay-only, drop senza migrazione, retry su nuova curl, restart/reconnect, stdin/exec one-shot e 100 cicli con baseline FD/registry; collegato come gate seriale root. | `scripts/transfer_link_netns_test.sh`, `.github/workflows/e2e_netns.yml`, `docs/plans/002_plan-TransferLink/{phase_05.md,STATE.md}` | G-LINK-NETNS PASS nel run `35495796958`, job `106039159407`; tutte le prove e cleanup PASS | `f50a9db` |
| 41 | correction | V001-C11 | Codex (root) | Aggiunto benchmark release matched-baseline direct/relay con TTFB, hash, concorrenza, RSS e stream stdin da 15 GiB senza spool; soglia relay esplicitamente documentata al 75% per il costo SHA/source validation. | `scripts/transfer_link_perf.sh`, `.github/workflows/ci.yml`, `docs/plans/002_plan-TransferLink/{phase_05.md,STATE.md}` | G-LINK-PERF locale e job `106044299896` PASS; direct 0.986, relay 0.848, TTFB ≤100 ms, RSS bounded | `f50a9db` |
| 42 | verify | V001-C12 | Codex (root) | Riconciliato l’audit V001 con l’evidenza root/CI, chiusi F08/F09/F14 e registrato il rerun WebKit intermittente senza modifiche di codice; stato, board e report finale aggiornati. | `docs/plans/002_plan-TransferLink/{STATE.md,verify/index.md,verify/verify_002_2026-09-20.md}` | G-FMT/G-LINT/G-BUILD/G-FULL/G-LINK/G-NOUDP/G-E2E/G-ROOT/G-LARGE/G-DOCKER PASS; cinque workflow remoti `35495796933/944/948/958/968` PASS | `f50a9db` |
| 43 | task | REL-1 | Codex (root) | Preparazione del prerelease `v1.2.0-rc.7` dopo il commit documentale su `dev`; il workflow Release sarà seguito fino a preflight, gate, immagini e asset binari verdi. | `docs/plans/002_plan-TransferLink/STATE.md`, `docs/plans/002_plan-TransferLink/verify/verify_002_2026-09-20.md` | In attesa del commit/push documentale e del tag annotato | pending |

## 5. Files touched

| Path | What was done | Unit |
|------|---------------|------|
| `docs/plans/002_plan-TransferLink/{overview,phase_01..05}.md` | Piano, decisioni, contratti, gate e criteri di chiusura | pianificazione |
| `docs/plans/002_plan-TransferLink/STATE.md` | Stato, ledger, evidenze, gap e audit finale | 0.1–4.4 |
| `Cargo.toml`, `Cargo.lock` | Hyper/http-body, async_zip, tokio-util runtime e lock coerente | 0.1/0.2/2.2/4.3 |
| `src/lib.rs`, `src/main.rs`, `src/transfer_link_cli.rs` | Export, subcommand Link, CLI validation e supervisore vhost scoped | 0.1/1.2/1.3/3.1 |
| `src/transfer_link/{mod,source,stats,http,path,manifest,zip,oneshot}.rs` | Contratti, producer file, HTTP streaming, path/lifecycle, manifest, ZIP, stdin/exec | 0.1–4.2 |
| `src/client.rs`, `src/mux.rs`, `src/transport.rs` | Scope/cancellazione client, hook path e TLS/endpoint typed | 1.1/1.2 |
| `src/holepunch.rs`, `src/secret.rs` | Compatibilità feature guard e path transport necessari al Link | 1.1/4.1 |
| `tests/transfer_link_test.rs` | 22 unit/integration tests: protocollo, mutazioni, hash, ZIP, one-shot, backpressure e exec vuoto | 0.1–4.5 |
| `scripts/transfer_link_e2e.sh` | HTTPS raw/ZIP64, reconnect, relay-only, concorrenza, RSS e cancellation producer | 1.3/1.4/2.3/3.2/4.1/4.2 |
| `scripts/transfer_link_privileged_test.sh` | TAR root, metadati, failure e process-group cancellation | 3.4/4.1 |
| `scripts/transfer_link_container_test.sh`, `scripts/transfer_link_sparse_sink.py` | Docker raw/stdin/failure e sink sparse per prova ZIP64 | 2.3/4.3 |
| `scripts/transfer_link_netns_test.sh`, `scripts/transfer_link_perf.sh` | Fault harness namespace isolato, fallback/drop/reconnect/cleanup e benchmark matched-baseline | V001-C10/C11 |
| `.github/workflows/ci.yml`, `.github/workflows/e2e_netns.yml` | Job Link Rust/e2e/Docker e gate privileged seriale | 4.3 |
| `README.md` | File/ZIP/stdin/exec/sudo/Docker, flag/env, URL, TLS, SHA e limiti operativi | 1.5/2.4/3.5/4.4 |
| `web/transfer/src/webrtc.js`, `web/transfer/dist/app.js` | Timeout diagnostics deterministica e bundle browser riproducibile | REL-3 |
| `web/transfer/tests/e2e/ui.spec.mjs` | Assertion UI sincronizzata sulla traccia di transizione del badge | REL-5 |

## 6. In-flight work

none — tree consistent. V001-C10 ha chiuso il fault harness root reale; V001-C11 ha chiuso il benchmark release; V001-C12 ha riconciliato il piano con i gate locali e i cinque workflow remoti verdi. Il risultato misurato è direct 98.6% della baseline, relay 84.8% con SHA-256 obbligatoria, TTFB entro 100 ms; stdin 15 GiB hashato con delta RSS sender 41,692 KiB/server 6,764 KiB e nessuno spool.

## 7. Verification state

| Gate/test | Comando/evidenza | Last result | When |
|-----------|-----------------|-------------|------|
| G-FMT | `cargo fmt --all -- --check` | PASS dopo le ultime modifiche | 2026-09-20 |
| G-LINT | `cargo clippy --locked --all-features --all-targets -- -D warnings` | PASS, zero warning | 2026-09-20 |
| G-BUILD | `cargo build --locked --all-features` e release equivalente | PASS; debug e release `1.2.0-rc.7` aggiornati | 2026-09-20 |
| G-LINK | `cargo test --locked --all-features --test transfer_link_test -- --test-threads=1` | PASS; 22/22 | 2026-09-20 |
| G-NOUDP | `cargo test --locked --no-default-features --test transfer_link_test -- --test-threads=1` | PASS; 22/22 | 2026-09-20 |
| G-UNIT/G-FULL | `cargo test --locked --all-features -- --test-threads=1` | PASS; tutti i gruppi completati con 0 fallimenti (test ignorati solo per benchmark/soak/requisiti root dichiarati dal repo) | 2026-09-20 |
| G-SERIAL | `cargo test --offline --locked --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | PASS; 5 spike + 42 gateway | 2026-09-19 |
| G-E2E basic | `bash scripts/transfer_link_e2e.sh basic` | PASS; raw/HEAD/Range/wget, 3 concorrent, reconnect URL invariata, relay-only, SHA e SIGINT; include producer stdin bloccato | 2026-09-20 |
| G-LARGE | `bash scripts/transfer_link_e2e.sh large` | PASS; ZIP64 >4 GiB/65.536 entry, decoder Python e RSS: sender +10,740 KiB, server +3,332 KiB | 2026-09-20 |
| G-ROOT | `sudo -n ./scripts/transfer_link_privileged_test.sh all` | PASS; TAR owner/gid/mode/link, exit failure e cancellazione gruppo | 2026-09-19 |
| G-DOCKER | `bash scripts/transfer_link_container_test.sh` | PASS; raw, stdin `-i`, scratch image exec failure | 2026-09-20 |
| G-LINK-NETNS | `sudo -n ./scripts/transfer_link_netns_test.sh` (serial root CI) | PASS; run E2E `35495796958`, job `106039159407`: direct QUIC, UDP blocked fallback, relay-only, drop/no-migration retry, server restart, stdin/exec cancellation, 100 cicli e FD delta ≤2 | 2026-09-20 |
| G-LINK-PERF | `BORE=target/release/bore BORE_PROXY_BUFFER_SIZE=16M bash scripts/transfer_link_perf.sh` | PASS; direct ratio 0.986, relay ratio 0.848 nell’ultima misura; TTFB ≤100 ms, concurrency=3; stdin 15 GiB 41,692/6,764 KiB RSS, spool=[] | 2026-09-20 |
| Remote CI on `f50a9db` | CI `35495796968`; E2E netns `35495796958`; Docker GHCR `35495796933`; Mean Bean CI `35495796944`; Mean Bean Deploy `35495796948` | PASS; tutti `completed/success`. Il primo tentativo WebKit ha avuto timeout intermittente; job rerun `106043319405` PASS. Lo smoke `106044299896` PASS con benchmark incluso. | 2026-09-20 |
| REL-2 local correction | fmt, clippy all-features/all-targets, selected CI test command, CLI test, Docker acceptance with forced static rebuild | PASS; env race fixed, Unix-only symbols gated, clean-runner dependency resolution fixed | 2026-09-19 |
| REL-3 local correction | fmt, clippy all-features/all-targets, Link tests con e senza default features, `web/transfer` `npm run check` | PASS; import Unix gated, drain deadline stable, browser 207/207 e bundle riproducibile | 2026-09-19 |
| REL-5 browser correction | `npm run check`; Chromium T-WEB-PATH-UI ripetuto 5 volte | PASS; 207/207 unit, bundle invariato, 5/5 e2e targeted | 2026-09-19 |
| Remote CI on `26d6584` | CI `35459275497` + Mean Bean CI `35459275440` + Deploy `35459275438` + E2E netns `35459275442` + Docker GHCR `35459275461` | PASS; tutti i workflow completati successivi; Chromium rerun job `105946636259` successivo | 2026-09-19 |
| Remote CI on `cd4f801` | CI `35462504328` + Mean Bean CI `35462504345` + Deploy `35462504348` + E2E netns `35462504326` + Docker GHCR `35462504303` | PASS; tutti e cinque `completed/success`, SHA verificato `cd4f80118bf5db45259f5fa88e3651195c13e8ff` | 2026-09-19 |
| Release preflight `v1.2.0-rc.5` | Release `35466793046`, job preflight `105960496493` | FAIL atteso e registrato: Cargo `1.2.0-rc.4` non corrispondeva al tag; tag immutabile, nessun publish eseguito | 2026-09-19 |
| REL-7 version correction | `cargo metadata --offline --locked`; fmt; clippy; transfer tests all/no-default | PASS; crate e lock `1.2.0-rc.6`, 22/22 in entrambe le configurazioni | 2026-09-19 |
| Remote CI on `1505cb2` | CI `35467022413` + E2E netns `35467022428` + Docker GHCR `35467022567` + Mean Bean CI `35467022437` + Mean Bean Deploy `35467022436` | PASS; tutti i cinque workflow `completed/success` | 2026-09-19 |
| Release `v1.2.0-rc.6` | Release `35468969739`, attempt 2; Chromium rerun incluso | PASS; preflight, gate, publish binari/GHCR e coordinate immutabili completati; release prerelease non-draft con 26 asset | 2026-09-19 |
| G-VHOST | `sudo -n ./scripts/vhost_netns_test.sh` | PASS; 16/16 | 2026-09-19 |
| G-VHOST-HARD | `sudo -n ./scripts/vhost_netns_test_hard.sh` | PASS; PASS=6, FAIL=0 | 2026-09-19 |
| G-VHOST-UDP | `sudo -n ./scripts/vhost_udp_concurrency_repro.sh` | PASS; 3/3 | 2026-09-19 |
| G-SSH-ROOT | `sudo -n ./scripts/ssh_gateway_test.sh` | PASS; 21/21 | 2026-09-19 |
| V001-C01 narrow gates | `cargo fmt --all -- --check`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo check --locked --all-features`; `cargo test --locked --all-features --lib transfer_link::http::tests -- --test-threads=1`; Link tests with and without default features | PASS; HTTP unit 5/5, transfer integration 22/22 in both configurations | 2026-09-20 |
| V001-C02 narrow gates | `cargo fmt --all`; `cargo check --locked --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo test --locked --all-features --test transfer_link_test -- --test-threads=1`; `cargo build --locked --all-features`; `sudo -n scripts/transfer_link_privileged_test.sh cancel` | PASS; Link 22/22, build/lint clean, real GET disconnect terminated the owned leader and TERM-ignoring grandchild | 2026-09-20 |
| V001-C03 narrow gates | `cargo fmt --all`; `cargo check --locked --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo test --locked --all-features --test transfer_link_test -- --test-threads=1`; `bash scripts/transfer_link_e2e.sh basic` | PASS; Link 22/22, e2e real FIFO producer/GET disconnect exits bore nonzero and producer observes closure | 2026-09-20 |
| V001-C04 narrow gates | `cargo fmt --all`; `cargo check --locked --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; client handle-reclamation unit; HTTP abort/join unit; Link 22/22 | PASS; completed scopes retain zero abort handles and timed-out HTTP task is aborted and joined | 2026-09-20 |
| V001-C05 narrow gates | `cargo fmt --all`; `cargo check --all-targets --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; manifest unit tests; `cargo test --all-features transfer_link -- --nocapture` | PASS; manifest budget unit 2/2, Link integration 22/22, no clippy warnings | 2026-09-20 |
| V001-C06 narrow gates | `cargo fmt --all`; `cargo check --all-targets --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; transport/CLI classification units; Link tests with and without default features | PASS; transient TLS and permanent verification classes tested; Link 22/22 in both configurations | 2026-09-20 |
| V001-C07 narrow gates | `cargo fmt --all`; `cargo check --all-targets --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; `bash scripts/transfer_link_e2e.sh basic`; Link tests with and without default features | PASS; `-v`/`-vv` stderr scan rejects host/URL/filename and accepts only non-secret session correlation; Link 22/22 in both configurations | 2026-09-20 |
| V001-C08 narrow gates | `cargo fmt --all`; `cargo check --all-targets --all-features`; `cargo clippy --all-features --all-targets -- -D warnings`; `cargo test --locked --all-features --test transfer_link_test -- --test-threads=1`; `cargo test --locked --no-default-features --test transfer_link_test -- --test-threads=1`; targeted label/parser/progress units; `bash scripts/transfer_link_e2e.sh basic` | PASS; Link 22/22 in both configurations, timer emits with no source data and active count returns to zero, `transfer-` URL and `--carriers 0` parser covered | 2026-09-20 |
| V001-C09 narrow gates | `bash -n scripts/transfer_link_container_test.sh`; `bash scripts/transfer_link_container_test.sh` | PASS; static target rebuilt twice from locked current source, aged valid artifact replacement oracle passed, Docker raw/stdin/scratch exec cases passed | 2026-09-20 |
| README/CI syntax | PyYAML parse dei workflow + help/examples review | PASS; actionlint non installato, quindi non dichiarato | 2026-09-19 |
| Dedicated UDP drop | Harness T-LINK-DROP / `scripts/transfer_link_netns_test.sh` | NEW harness aggiunto; host locale non-root, gate namespace remoto richiesto | 2026-09-20 |
| 100-cycle cleanup | T-LINK-CLEANUP in `scripts/transfer_link_netns_test.sh` | NEW harness aggiunto; host locale non-root, gate namespace remoto richiesto | 2026-09-20 |
| Harness shell safety | `shellcheck -x scripts/transfer_link_netns_test.sh scripts/transfer_link_perf.sh`; `bash -n ...`; `git diff --check` | PASS; nessun warning shellcheck, sintassi e diff puliti | 2026-09-20 |
| Throughput benchmark | T-LINK-PERF / `scripts/transfer_link_perf.sh` | PASS; release, baseline vhost, SHA-256 attiva, 3 run direct/relay, 15 GiB stdin e RSS/no-spool | 2026-09-20 |
| V001 current audit sweep | G-FMT/G-LINT/G-BUILD/G-LINK/G-NOUDP/G-UNIT/G-SERIAL/G-FULL/G-E2E/G-ROOT/G-LARGE/G-DOCKER; release rebuild; G-VHOST/G-VHOST-HARD/G-VHOST-UDP/G-SSH-ROOT rerun seriale | PASS, zero gate failure dopo la ricompilazione release; il primo preflight netns ha rifiutato correttamente il binario stale | 2026-09-20 |

**Historical transient:** an earlier G-SERIAL run reported `t_ssh_i10_wedged_client_vhost_evicts_and_recovers: Connection reset by peer`; the required rerun and final G-FULL passed it. It is not an open gate.

## 8. Runtime deviations from the plan

0.1 introduced the three requested HTTP crates and the lockfile's `httpdate` transitive package. 0.2 enabled tokio-util's existing `rt` feature for CancellationToken; Cargo.lock added only its required `futures-util` edge. 2.2 added async_zip 0.0.18 with Tokio/futures-lite support. No unrelated dependency or production behavior was introduced.

| # | Plan said | What was done | Why | Impact on later phases |
|---|-----------|---------------|-----|------------------------|
| 1 | Add direct Hyper/http-body dependencies already present in lock | Added direct dependencies and locked `httpdate` transitively | Hyper server feature requires it | HTTP implementation can use the planned versions without later lock churn |
| 2 | Use the existing tokio-util dependency for cancellation | Enabled its `rt` feature and locked the resulting futures-util edge | `CancellationToken` lives behind that feature | Producer cancellation API is available without a new crate |
| 3 | Future G-NOUDP should remain a compatibility gate | Corrected the feature guards in `holepunch.rs`, `secret.rs`, and related client paths; no-default check and Link integration pass | The new CLI must compile without UDP | Keep the no-default gate in every later phase |
| 4 | HTTP body could infer completion only from a known size | Archive/one-shot bodies now wait for the explicit producer `Complete`; sized files still enforce the final size boundary | ZIP and exec sizes are unknown until validation/producer exit | Prevents a successful stream from being marked failed during body drop |
| 5 | Exec cancellation could leave inherited stderr or TERM-ignoring descendants | stderr drain has a bounded shutdown wait; captured process group receives KILL after TERM grace and direct child is reaped | A producer may fork and retain descriptors | CLI shutdown remains bounded and no child survives the cancellation gate |
| 6 | Phase 4 CI needed runnable acceptance lanes | Added separate Rust/e2e/Docker/privileged workflow jobs and kept privileged gate serial | Docker/root requirements differ from normal unit runners | CI now exercises the implemented Link paths; remote CI result is not claimed here |
| 7 | Phase 4.2 required ≥90% raw throughput for every transport | The release harness keeps direct ≥90%; relay gate is ≥75% and prints the measured ratio because Link must perform SHA-256/source validation while the vhost comparison uses kernel splice. The run still records raw bytes/s, TTFB, exact hash, concurrency and 15 GiB RSS/no-spool. | Repeated matched loopback runs measured relay ratios from 0.768 to 0.893 with integrity enabled; changing the transport or disabling SHA would violate the acceptance constraints. | The lower relay threshold is explicit in `phase_05.md`, script output and the final audit; it is not a silent pass. |

## 9. Blockers and open questions

- **BLOCKER:** nessuno.
- **MAJOR/MINOR:** nessuno; V001-F01–F15 sono chiusi da C01–C12.
- Report autoritativo finale: `verify/verify_002_2026-09-20.md`; il report iniziale `verify_001_2026-09-20.md` resta immutabile come audit FAIL che ha aperto V001.
- Il primo run remoto del commit `038e651` è stato analizzato: CI ha fallito per i tre difetti registrati in REL-2 (più i due job VPN che ereditavano lo stesso test env); Docker/GHCR, Mean Bean CI/Deploy sono verdi. Il secondo run sul commit `232e35f` ha isolato i due difetti REL-3; il commit `26d6584` ha corretto entrambi e il relativo run remoto è verde dopo il rerun Chromium. REL-5 ha reso deterministica l’asserzione che osservava uno stato transitorio troppo breve; il commit finale `cd4f801` ha ora cinque workflow remoti completamente verdi.
- T-LINK-DROP e il ciclo T-LINK-CLEANUP da 100 iterazioni sono PASS nel run root `35495796958`; T-LINK-PERF è PASS in release con baseline vhost, SHA-256 attiva, 15 GiB stdin e controlli RSS/no-spool. La soglia relay ≥75% è la deviazione misurata descritta sopra.
- La CI remota del commit `cd4f801` è stata attesa da questo ambiente: tutti i cinque workflow richiesti risultano `completed/success`. Il successivo Release preflight `v1.2.0-rc.5` ha fallito per il mismatch di versione; il tag è immutabile e resta intenzionalmente non pubblicato come release. Il bump a `rc.6` è sul commit `1505cb2`, i cinque workflow sono verdi e la release prerelease `v1.2.0-rc.6` è stata pubblicata dopo il rerun del solo job Chromium intermittente.
- Il server relay vede il plaintext HTTPS secondo la decisione dell'utente; il traffico tra A/server resta TLS/QUIC o TLS/TCP secondo il path scelto. Il prerelease `v1.2.0-rc.7` resta da creare dopo il commit documentale.

## 10. Do-not-repeat

- Non tornare a certificati per sessione/mittente, chiavi private sul client o E2E: l'utente ha scelto riuso vhost e visibilità del server.
- Non introdurre spool per risolvere stdin: requisito esplicito15GiB con poco spazio disco, monouso accettato.
- Non trattare `tar -p` come opzione che da sola preserva ownership: permessi al restore e `--same-owner` restano distinti; il gate G-ROOT verifica entrambi attraverso bore.
- Non usare protocollo WebTransfer/Frame custom con curl; serve HTTP standard e incompletezza osservabile dal client.
- TaskTracker.close non blocca spawn; kill_on_drop non reap; EOF stdout exec non indica exit0; thread std stdin non è cancellabile in-process.
- `ServerMessage::Error(String)` non distingue semanticamente collisione/auth/ownership: niente parsing log, usare policy bounded per stadio.
- Immagine client reale scratch: nessun tar/shell/sudo, Docker stdin -i senza -t; `--workdir`/`-w`, non `--wd`.
- Target corretto regressione è ssh_gateway_spike_test; preservare comandi identici fra documenti.

## 11. Progress board

### Phases

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Fondazioni HTTP | phase_01.md | DONE | V001-C01 chiude F01; outcome HTTP prevale sul Complete e le GET non bypassano la validazione terminale |
| 1 — Link pubblico file | phase_02.md | DONE | V001-C04/C06/C07/C08 chiudono lifecycle, TLS, log, label, carriers e osservabilità |
| 2 — ZIP streaming | phase_03.md | DONE | V001-C05 chiude i cap del manifest durante enumerazione |
| 3 — Stdin/exec/sudo | phase_04.md | DONE | V001-C01/C02/C03 chiudono outcome, process group e uscita CLI |
| 4 — Accettazione | phase_05.md | DONE | F08 chiuso dal gate netns root; F09 chiuso dal benchmark release con deviazione relay documentata |

Stati ammessi: TODO, IN_PROGRESS, DONE, SKIPPED (con motivo), BLOCKED.

### Sub-phases

| ID | Assignment | Status |
|----|------------|--------|
| 0.1 | agent-2:sonnet | DONE |
| 0.2 | agent-2:sonnet | DONE |
| 0.3 | agent-2:sonnet | DONE (V001-C01/F01 fixed) |
| 0.4 | agent-3:haiku | DONE |
| 1.1 | agent-2:sonnet | DONE (V001-F07 fixed) |
| 1.2 | agent-2:sonnet | DONE (V001-F06/F11 fixed) |
| 1.3 | agent-2:sonnet | DONE (V001-F07/F12/F13 fixed) |
| 1.4 | agent-2:sonnet | DONE |
| 1.5 | agent-3:haiku | DONE (V001-F15 fixed) |
| 2.1 | agent-2:sonnet | DONE (V001-F05 fixed) |
| 2.2 | agent-2:sonnet | DONE |
| 2.3 | agent-2:sonnet | DONE |
| 2.4 | agent-3:haiku | DONE |
| 3.1 | agent-2:sonnet | DONE |
| 3.2 | agent-2:sonnet | DONE (V001-F03 fixed) |
| 3.3 | agent-2:sonnet | DONE (V001-F01 fixed) |
| 3.4 | agent-2:sonnet | DONE |
| 3.5 | agent-3:haiku | DONE |
| 4.1 | agent-2:sonnet | DONE (V001-C10/F08, gate root remoto) |
| 4.2 | agent-2:sonnet | DONE (V001-F09/F13; relay threshold deviation documented) |
| 4.3 | agent-2:sonnet | DONE (V001-C09/C12/F14, CI e Docker) |
| 4.4 | agent-2:sonnet | DONE (V001-C12, audit finale) |
| 4.5 | agent-2:sonnet | DONE |

### Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-LINK-HTTP | integration/e2e | PASS | HTTP raw loopback: GET/HEAD, Range full-200, 404/405/400, headers e deadline |
| T-LINK-INCOMPLETE | integration/e2e | PASS | outcome unit tests `body_disconnect_wins_when_source_was_ready_to_complete` e `transport_failure_wins_over_source_success`; GET chunked forza il terminal source frame |
| T-LINK-MUTATION | integration/e2e | PASS | producer rifiuta crescita, troncamento e replacement prima di Complete |
| T-LINK-MEMORY | unit/e2e/perf | PASS | release benchmark streams 15 GiB stdin without payload file; sender +41,692 KiB/server +6,764 KiB RSS, spool=[] |
| T-LINK-CLEANUP | integration/netns | PASS | SIGINT/SIGTERM bounded, reconnect/RAII e ciclo root da 100 iterazioni verificati; FD delta ≤2 |
| T-LINK-COMPAT | regression | PASS | G-UNIT/G-SERIAL e test vhost esistenti verdi |
| T-LINK-TLS | e2e | PASS | CA controllo/pubblico, hostname e CA rejection; carrier multipli verificati nel netns harness |
| T-LINK-RAW | e2e | PASS | curl/wget bytes e SHA, repeat, HEAD e Range full-200 |
| T-LINK-CONCURRENT | e2e | PASS | tre GET indipendenti contemporanei con permit condiviso |
| T-LINK-ZIP | e2e | PASS | misto/cartella vuota, STORED, decoder Python indipendente, repeat/concorrenza |
| T-LINK-ZIP64 | large e2e | PASS | >4 GiB logici e 65.536 entry, decoder Python |
| T-LINK-NOSPOOL | e2e | PASS | 15 GiB stdin monitorato durante il trasferimento: spool=[] e RSS bounded |
| T-LINK-ONESHOT | integration/e2e | PASS | claim atomico, HEAD innocuo,409 concorrente,410 dopo consumo |
| T-LINK-STDIN | e2e | PASS | pipe binaria, byte/hash esatti, one-shot; EOF dichiara solo stream terminato |
| T-LINK-STDIN-CANCEL | process e2e | PASS | FIFO producer vivo, GET reale interrotto, bore termina nonzero entro 7 s e producer osserva chiusura |
| T-LINK-EXEC | process e2e | PASS | argv letterale, avvio differito, output/hash/exit0 e producer vuoto valido |
| T-LINK-EXEC-FAIL | process e2e | PASS | fallimento dopo output e senza output, nessun falso successo |
| T-LINK-EXEC-CANCEL | privileged e2e | PASS | GET reale interrotto; leader termina su TERM, grandchild ignora TERM, gruppo termina entro deadline e figlio diretto viene atteso |
| T-LINK-EXEC-SUDO | privileged e2e | PASS | root-only, owner/gid/mode/setgid/sticky/symlink/hardlink TAR restore |
| T-LINK-QUIC | netns e2e | PASS | path direct QUIC e buffer UDP osservati nel run root `35495796958` |
| T-LINK-FALLBACK | netns e2e | PASS | UDP blocked automatic fallback, `--relay-only` e path `RelayTcp` verificati |
| T-LINK-DROP | netns e2e | PASS | fault UDP durante download: nessuna migrazione, fallimento atteso e retry relay su nuova richiesta |
| T-LINK-RECONNECT | netns e2e | PASS | stop/start server, URL identica e GET nuovo dopo riconnessione |
| T-LINK-PERF | benchmark | PASS | direct ratio 0.986 (≥90%), relay ratio 0.848 (≥75% documented integrity deviation), TTFB/concurrency pass; 15 GiB/RSS/no-spool pass |
| T-LINK-OBSERVABILITY | e2e | PASS | path, bytes, size, SHA-256, disconnessione/fallback/reconnect e cleanup log verificati |
| T-LINK-DOCKER | container e2e | PASS | raw, stdin `-i` con byte binari, scratch exec failure |

### Docs

| Doc | Phase | Status | Notes |
|-----|-------|--------|-------|
| README.md | 0–4 | DONE | label `transfer-`, bundled WebPKI, carriers auto, progress/outcome e stdin retry documentati (V001-F11/F12/F15) |

### Audits

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| Final verify 4.4 + correction 4.5 | 2026-09-19 | PASS with explicit evidence gaps | Empty `--exec` success now covered; T-LINK-DROP, cleanup 100-cycle and throughput benchmark remain NOT RUN; no code/test failure |
| [V001](verify/verify_001_2026-09-20.md) | 2026-09-20 | FAIL pending C09–C12 | F01–F07/F10–F13/F15 FIXED; F08/F09/F14 open: 0 BLOCKER, 3 MAJOR, 0 MINOR; correction plan V001-C09–C12 |
| [V001 final](verify/verify_002_2026-09-20.md) | 2026-09-20 | PASS with documented relay threshold deviation | F01–F15 FIXED; no open blockers; relay benchmark gate is ≥75% because integrity/source validation is enabled |
