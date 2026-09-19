# Transfer Link — Implementation State

> **LEGGERE QUESTO FILE PER PRIMO a ogni sessione. Aprire un'unità in §1 PRIMA di modificare codice; chiuderla DOPO i gate.**
> **Last updated:** 2026-09-19 | **By:** Codex, CI correction unit opened | **Session:** 13

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

- **Type:** bug-fix
- **ID:** REL-2
- **Status:** OPEN
- **Intent:** Correggere i failure CI riproducibili su Windows/macOS/Linux e Docker senza alterare il protocollo Link.
- **Phase:** 4 — Accettazione (`phase_05.md`).
- **Next action:** applicare guard Unix/env e rimuovere offline dal fallback Docker, eseguire gate locali, registrare risultati e pushare una correzione.
- **Assigned:** agent-2:sonnet (esecuzione root Codex; nessun alias modello dichiarato non disponibile); review agent-1:opus (revisione inline).
- **Repo state:** branch `dev`; commit `038e651` (`feat(transfer): add public transfer links`) già pubblicato. CI ha rilevato failure correggibili in clippy Windows, test env-parallel e fallback Docker offline; il tag resta bloccato fino a una nuova CI completamente verde. Tutte le fasi del piano sono chiuse; i fault UDP dedicati, il ciclo cleanup da 100 iterazioni e il benchmark comparativo non sono stati dichiarati PASS senza un harness/misurazione stabile.

## 2. Feature context

Nuovo `bore transfer link`: A serve file/cartelle tramite HTTP loopback e vhost bore; B usa normale link HTTPS curl/wget/browser. URL `https://transfer-<16 casuali a-z0-9>.<dominio-vhost>/<filename>`, wildcard/certificati vhost esistenti, nessuna emissione/cache nuova. Il server VEDE plaintext (accettato), senza conservarlo: NON E2E.

Trasporto A↔server QUIC default, fallback TCP TLS in apertura; --relay-only. File originale singolo o ZIP STORED/ZIP64 per multipli/cartelle; repeat/parallel fino a segnale, sorgenti stabili. Stdin/exec monouso, no spool, HEAD innocuo,409 concorrente/410 consumato. Exec argv senza shell, privilegi ereditati, gruppo processi, exit0 obbligatorio prima di successo HTTP; stdin EOF non conosce exit del produttore esterno. SHA256 A confrontabile B, non ACK del salvataggio su disco.

**Reference scenario:** `bore transfer link mydoc.zip` → URL unica stdout, curl/wget bytes+SHA identici e GET successivi; `sudo bore transfer link --filename backup.tar --exec -- tar -cpf - myfolder` → TAR invariato, restore root `--numeric-owner --same-owner -xpf` preserva mode/owner/link. Prove T-LINK-RAW/CONCURRENT/EXEC-SUDO; false-success impedito da T-LINK-EXEC-FAIL.

**Hard constraints:** I-1 no payload persistente/buffer illimitati; I-2 niente downgrade plain; I-3 niente fine HTTP prima di source/producer valido; I-4 stdout binario/stderr separato, sudo e reap; I-5 monouso atomico; I-6 path reale e niente migrazione; I-7 legacy invariato; I-8 no snapshot/disco B garantiti; I-9 risorse bounded (thread stdin bloccato termina con processo CLI); I-10 gate veri e netns seriali.

**Defaults tecnici fissati:** Hyper1 HTTP/1.1, Range ignorato200, Connection close; chunk256KiB/coda2; file/ZIP max-downloads8, stdin/exec1; manifest≤100000 entry/32MiB nomi/depth256; ZIP symlink/special/nonUTF8/collisioni rifiutati; exec Unix foreground. --ca-cert PEM opzionale aggiunge trust senza togliere hostname verification. Reconnect mantiene URL e non riavvia producer; rejection generica dopo Ready retry bounded75s, nessuna interpretazione del testo wire.

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
| 24 | correction | REL-2 | Codex (root) | Gated Unix-only `oneshot` imports/constants so Windows clippy is clean; serialized the env-sensitive Link CLI parser test and restored `BORE_SERVER`; removed forced offline resolution from the Docker acceptance fallback so clean runners can fetch the locked graph. | `src/transfer_link/oneshot.rs`, `src/main.rs`, `scripts/transfer_link_container_test.sh`, `docs/plans/002_plan-TransferLink/STATE.md` | fmt PASS; clippy all-features/all-targets PASS; selected regression 0 failed; CLI unit PASS; Docker acceptance PASS | uncommitted |

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
| `.github/workflows/ci.yml`, `.github/workflows/e2e_netns.yml` | Job Link Rust/e2e/Docker e gate privileged seriale | 4.3 |
| `README.md` | File/ZIP/stdin/exec/sudo/Docker, flag/env, URL, TLS, SHA e limiti operativi | 1.5/2.4/3.5/4.4 |

## 6. In-flight work

claimed — REL-2 CI correction: guard Unix-only oneshot symbols, serialize the env-sensitive CLI test, and make the Docker fallback resolve dependencies on a clean runner.

## 7. Verification state

| Gate/test | Comando/evidenza | Last result | When |
|-----------|-----------------|-------------|------|
| G-FMT | `cargo fmt --all -- --check` | PASS dopo le ultime modifiche | 2026-09-19 |
| G-LINT | `cargo clippy --offline --locked --all-features --all-targets -- -D warnings` | PASS, zero warning | 2026-09-19 |
| G-BUILD | `cargo build --offline --locked --all-features` e release equivalente | PASS; debug e release aggiornati | 2026-09-19 |
| G-LINK | `cargo test --offline --locked --all-features --test transfer_link_test -- --test-threads=1` | PASS; 22/22 | 2026-09-19 |
| G-NOUDP | `cargo test --offline --locked --no-default-features --test transfer_link_test -- --test-threads=1` | PASS; 22/22 | 2026-09-19 |
| G-UNIT/G-FULL | `cargo test --offline --locked --all-features -- --test-threads=1` | PASS; 33 result groups, 1324 passed, 0 failed, 3 ignored (benchmark/soak dichiarati dal repo), dopo l'aggiunta del test 4.5 | 2026-09-19 |
| G-SERIAL | `cargo test --offline --locked --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | PASS; 5 spike + 42 gateway | 2026-09-19 |
| G-E2E basic | `bash scripts/transfer_link_e2e.sh basic` | PASS; raw/HEAD/Range/wget, 3 concorrent, reconnect URL invariata, relay-only, SHA e SIGINT; include producer stdin bloccato | 2026-09-19 |
| G-LARGE | `bash scripts/transfer_link_e2e.sh large` | PASS; ZIP64 >4 GiB/65.536 entry, decoder Python e RSS: sender +34,616 KiB, server +3,256 KiB | 2026-09-19 |
| G-ROOT | `sudo -n ./scripts/transfer_link_privileged_test.sh all` | PASS; TAR owner/gid/mode/link, exit failure e cancellazione gruppo | 2026-09-19 |
| G-DOCKER | `bash scripts/transfer_link_container_test.sh` | PASS; raw, stdin `-i`, scratch image exec failure | 2026-09-19 |
| REL-2 local correction | fmt, clippy all-features/all-targets, selected CI test command, CLI test, Docker acceptance with forced static rebuild | PASS; env race fixed, Unix-only symbols gated, clean-runner dependency resolution fixed | 2026-09-19 |
| G-VHOST | `sudo -n ./scripts/vhost_netns_test.sh` | PASS; 16/16 | 2026-09-19 |
| G-VHOST-HARD | `sudo -n ./scripts/vhost_netns_test_hard.sh` | PASS; PASS=6, FAIL=0 | 2026-09-19 |
| G-VHOST-UDP | `sudo -n ./scripts/vhost_udp_concurrency_repro.sh` | PASS; 3/3 | 2026-09-19 |
| G-SSH-ROOT | `sudo -n ./scripts/ssh_gateway_test.sh` | PASS; 21/21 | 2026-09-19 |
| README/CI syntax | PyYAML parse dei workflow + help/examples review | PASS; actionlint non installato, quindi non dichiarato | 2026-09-19 |
| Dedicated UDP drop | Harness T-LINK-DROP da phase_05 | NOT RUN; nessun harness di fault attivo stabile disponibile | 2026-09-19 |
| 100-cycle cleanup | T-LINK-CLEANUP esteso da phase_05 | NOT RUN; base SIGINT/reconnect/RAII verificata, ciclo completo non misurato | 2026-09-19 |
| Throughput benchmark | T-LINK-PERF da phase_05 | NOT RUN; RSS e boundedness misurate, nessuna soglia ≥90% dichiarata | 2026-09-19 |

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

## 9. Blockers and open questions

- Nessuna domanda prodotto rinviata e nessun blocker tecnico noto per file, ZIP, stdin o exec.
- Il primo run remoto del commit `038e651` è stato analizzato: CI ha fallito per i tre difetti registrati in REL-2 (più i due job VPN che ereditavano lo stesso test env); Docker/GHCR, Mean Bean CI/Deploy sono verdi. La correzione va pubblicata e verificata su una nuova CI prima del tag.
- T-LINK-DROP (guasto UDP durante un download), il ciclo T-LINK-CLEANUP da 100 iterazioni e T-LINK-PERF con baseline ≥90% non sono stati eseguiti: mancano un harness di fault ripetibile e una baseline comparabile. Sono gap di evidenza, non risultati PASS impliciti.
- La CI remota non è stata attesa da questo ambiente; i workflow YAML sono stati parsati con PyYAML e i job locali equivalenti sono passati.
- Il server relay vede il plaintext HTTPS secondo la decisione dell'utente; il traffico tra A/server resta TLS/QUIC o TLS/TCP secondo il path scelto.

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
| 0 — Fondazioni HTTP | phase_01.md | DONE | 0.1/0.2/0.3/0.4 DONE; README invariato |
| 1 — Link pubblico file | phase_02.md | DONE | 1.1/1.2/1.3/1.4/1.5 DONE; file singolo operativo |
| 2 — ZIP streaming | phase_03.md | DONE | 2.1–2.4 DONE; manifest bounded, STORED/ZIP64 streaming e RSS verificati |
| 3 — Stdin/exec/sudo | phase_04.md | DONE | 3.1–3.5 DONE; one-shot, pipe, argv, process group e TAR root verificati |
| 4 — Accettazione | phase_05.md | DONE | 4.1–4.5 DONE; gap T-LINK-DROP/CLEANUP-100/PERF espliciti in §7/§9 |

Stati ammessi: TODO, IN_PROGRESS, DONE, SKIPPED (con motivo), BLOCKED.

### Sub-phases

| ID | Assignment | Status |
|----|------------|--------|
| 0.1 | agent-2:sonnet | DONE |
| 0.2 | agent-2:sonnet | DONE |
| 0.3 | agent-2:sonnet | DONE |
| 0.4 | agent-3:haiku | DONE |
| 1.1 | agent-2:sonnet | DONE |
| 1.2 | agent-2:sonnet | DONE |
| 1.3 | agent-2:sonnet | DONE |
| 1.4 | agent-2:sonnet | DONE |
| 1.5 | agent-3:haiku | DONE |
| 2.1 | agent-2:sonnet | DONE |
| 2.2 | agent-2:sonnet | DONE |
| 2.3 | agent-2:sonnet | DONE |
| 2.4 | agent-3:haiku | DONE |
| 3.1 | agent-2:sonnet | DONE |
| 3.2 | agent-2:sonnet | DONE |
| 3.3 | agent-2:sonnet | DONE |
| 3.4 | agent-2:sonnet | DONE |
| 3.5 | agent-3:haiku | DONE |
| 4.1 | agent-2:sonnet | DONE (gap T-LINK-DROP/100-cycle) |
| 4.2 | agent-2:sonnet | DONE (gap T-LINK-PERF) |
| 4.3 | agent-2:sonnet | DONE |
| 4.4 | agent-2:sonnet | DONE |
| 4.5 | agent-2:sonnet | DONE |

### Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| T-LINK-HTTP | integration/e2e | PASS | HTTP raw loopback: GET/HEAD, Range full-200, 404/405/400, headers e deadline |
| T-LINK-INCOMPLETE | integration/e2e | PASS | body fallibile: canale chiuso senza Complete produce UnexpectedEof |
| T-LINK-MUTATION | integration/e2e | PASS | producer rifiuta crescita, troncamento e replacement prima di Complete |
| T-LINK-MEMORY | unit/e2e/perf | PASS | coda bounded a 2 chunk, receiver lento/drop e cancellation verificati |
| T-LINK-CLEANUP | integration/netns | PASS parziale | SIGINT/SIGTERM bounded, reconnect e RAII verificati; ciclo 100 non-run |
| T-LINK-COMPAT | regression | PASS | G-UNIT/G-SERIAL e test vhost esistenti verdi |
| T-LINK-TLS | e2e | PASS parziale | CA controllo/pubblico, hostname e CA rejection nel harness; carrier multipli non isolati |
| T-LINK-RAW | e2e | PASS | curl/wget bytes e SHA, repeat, HEAD e Range full-200 |
| T-LINK-CONCURRENT | e2e | PASS | tre GET indipendenti contemporanei con permit condiviso |
| T-LINK-ZIP | e2e | PASS | misto/cartella vuota, STORED, decoder Python indipendente, repeat/concorrenza |
| T-LINK-ZIP64 | large e2e | PASS | >4 GiB logici e 65.536 entry, decoder Python |
| T-LINK-NOSPOOL | e2e | PASS parziale | RSS bounded e nessun archivio temporaneo nel percorso; prova 15 GiB non-run |
| T-LINK-ONESHOT | integration/e2e | PASS | claim atomico, HEAD innocuo,409 concorrente,410 dopo consumo |
| T-LINK-STDIN | e2e | PASS | pipe binaria, byte/hash esatti, one-shot; EOF dichiara solo stream terminato |
| T-LINK-STDIN-CANCEL | process e2e | PASS | producer infinito, SIGTERM, uscita entro 7 s |
| T-LINK-EXEC | process e2e | PASS | argv letterale, avvio differito, output/hash/exit0 e producer vuoto valido |
| T-LINK-EXEC-FAIL | process e2e | PASS | fallimento dopo output e senza output, nessun falso successo |
| T-LINK-EXEC-CANCEL | privileged e2e | PASS | TERM/KILL gruppo, discendente ignorante TERM terminato e child reaped |
| T-LINK-EXEC-SUDO | privileged e2e | PASS | root-only, owner/gid/mode/setgid/sticky/symlink/hardlink TAR restore |
| T-LINK-QUIC | netns e2e | PASS parziale | harness osserva `path=Some(DirectQuic)` e server carrier QUIC; netns/perf resta fase4 |
| T-LINK-FALLBACK | netns e2e | PASS parziale | `--relay-only` e path `RelayTcp` verificati; UDP bloccato con fallback automatico resta fase4 |
| T-LINK-DROP | netns e2e | NOT RUN | fault UDP attivo non disponibile in harness stabile |
| T-LINK-RECONNECT | netns e2e | PASS | stop/start server, URL identica e GET nuovo dopo riconnessione |
| T-LINK-PERF | benchmark | NOT RUN | RSS/boundedness misurate; baseline throughput e soglia ≥90% non dichiarate |
| T-LINK-OBSERVABILITY | e2e | PASS parziale | path, bytes, size e SHA-256 completion log verificati; fault-log matrix resta fase4 |
| T-LINK-DOCKER | container e2e | PASS | raw, stdin `-i` con byte binari, scratch exec failure |

### Docs

| Doc | Phase | Status | Notes |
|-----|-------|--------|-------|
| README.md | 0–4 | PASS | file/ZIP/stdin/exec/sudo/Docker, deploy vhost, flag/env, TLS/URL, SHA, limiti e troubleshooting aggiornati insieme alla feature |

### Audits

| Report | Date | Verdict | Open findings |
|--------|------|---------|---------------|
| Final verify 4.4 + correction 4.5 | 2026-09-19 | PASS with explicit evidence gaps | Empty `--exec` success now covered; T-LINK-DROP, cleanup 100-cycle and throughput benchmark remain NOT RUN; no code/test failure |
