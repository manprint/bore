# Transfer Link — Plan Overview

> **Authored:** 2026-09-19. Skill: `plan-execute-verify`, modalità Plan.
> **Folder:** `docs/plans/002_plan-TransferLink/`.
> **Esecuzione: leggere [STATE.md](STATE.md) PRIMA di questo file.** È l'unico stato di avanzamento. Con unità OPEN, finirla o annullarla prima di aprirne un'altra; verificare i gate §3 contro le dichiarazioni §1/§7/§11. Aprire ogni unità prima delle modifiche, chiuderla dopo i gate.

## Goal

Implementare `bore transfer link`: download HTTPS standard da curl, wget e browser, con sorgente sul mittente e relay senza persistenza dei payload. File riutilizzabili e download concorrenti fino a Ctrl+C; cartelle/multipli in ZIP STORED/ZIP64; stdin ed exec monouso senza spool. QUIC mittente↔server di default, fallback TCP cifrato prima dell'apertura del flusso, osservabilità sul mittente.

```sh
# A: pubblica e resta attivo; stdout contiene soltanto il link.
bore transfer link mydoc.zip
# https://transfer-<16 caratteri a-z0-9>.bore.tld/mydoc.zip
# B, poi anche C in parallelo:
curl --fail --output mydoc.zip 'https://transfer-<id>.bore.tld/mydoc.zip'
# A, backup supervisionato (funzionalità da implementare):
sudo bore transfer link --filename backup.tar --exec -- tar -cpf - myfolder
# B, ripristino su directory predisposta:
sudo tar --numeric-owner --same-owner -xpf backup.tar -C restore
```

## Design decisions

| # | Decisione e origine | Conseguenza |
|---|--------------------|-------------|
| **D1** | Utente, ultima risposta sui certificati: accetta visibilità del server; riuso vhost e certificati esistenti. | HTTPS termina sul server; NON dichiarare E2E. Nessuna CA/provisioning/cache certificati nuova. |
| **D2** | Utente, formato sottodominio + ID16: `transfer-<id>.<dominio-vhost>`. | 16 caratteri CSPRNG uniformi a-z0-9; un solo label coperto dal wildcard esistente. Nessuno slug aggiuntivo obbligatorio nel filename. |
| **D3** | Utente, Q2 trasporto e Q6 fallback. | QUIC A↔server; TCP TLS automatico solo in apertura; `--relay-only`; niente migrazione di download attivi, niente P2P A↔B. |
| **D4** | Utente, flusso operativo 5–6. | File/ZIP ripetibili, paralleli; link fino a segnale o errore fatale. Byte consegnati al trasporto non provano salvataggio su B. |
| **D5** | Utente, stdin senza spazio disco + conferma monouso. | Buffer limitati, singolo GET consumante; HEAD innocuo; niente replay/resume dello stream; dopo errore rilanciare anche A. |
| **D6** | Utente, exec/sudo e integrità. | Argomenti OS senza shell; eredita privilegi; stdout binario separato da stderr; successo soltanto dopo exit 0. `-p` di GNU tar agisce al ripristino; ownership con `--same-owner`. |
| **D7** | Utente, SHA-256 e pipe esterna. | Digest della rappresentazione HTTP completa su A; stdin EOF non certifica successo di tar esterno. |
| **D8** | Utente, ZIP senza compressione e sorgenti stabili. | ZIP STORED, ZIP64, nessun archivio temporaneo; errori/modifiche rilevate interrompono il download; nessuna promessa di snapshot. |
| **D9** | Scelta implementativa esplicita: HTTP/1.1 Hyper, un GET per connessione; Range non implementato. | `Connection: close`, Range ignorato con risposta integrale 200, `Accept-Ranges: none`; niente resume implicito, niente ETag/304. |
| **D10** | Scelta implementativa: limiti locali e manifest per sessione. | 8 download file/ZIP di default, 1 stdin/exec; 256 KiB/chunk, coda 2 chunk; massimo 100.000 entry e 32 MiB di nomi ZIP. Rifiutare superamento prima di pubblicare. |
| **D11** | Scelta implementativa: policy filesystem conservativa. | ZIP rifiuta symlink, file speciali, nomi non UTF-8/ambigui, radici con stesso basename; hardlink regolari duplicati come file. Il TAR exec resta la via per fedeltà Unix. |
| **D12** | Scelta implementativa: errori HTTP espliciti. | 409 stream occupato, 410 consumato/fallito, 503 capacità esaurita; nessun riavvio automatico del produttore; URL trattato come credenziale bearer. |
| **D13** | Scelta implementativa: lifecycle supervisionato e rinnovo controllo. | Riutilizzare label durante reconnect, nessun retry dei corpi. Chiudere task/carrier di ogni tentativo prima del successivo. Sessione stdin interrotta durante GET termina con errore per liberare la pipe. |
| **D14** | Scelta implementativa di portabilità. | File/ZIP multipiattaforma; `--exec` inizialmente Unix con gruppi di processi; su altri target errore anticipato esplicito. Nessuna rottura della matrice di build. |
| **D15** | Utente: piano per esecutore meno capace, autorizzazione finale «scrivi il piano». | Fasi prescrittive, prove nominate, review dell'architetto sui punti delicati; nessuna implementazione in questa consegna. |
| **D16** | Scelta implementativa: CA aggiuntiva esplicita per installazioni private e test TLS reali. | `--ca-cert` estende il trust del client mantenendo verifica hostname; non emette certificati e non distribuisce chiavi private. Default webpki invariato. |

## Open questions

Nessuna domanda prodotto rinviata. L'intervista precedente ha risolto certificati, fiducia, trasporto, stdin, sudo e formato. D9–D14 sono scelte tecniche dichiarate del piano, non nuove risposte attribuite all'utente. Una sostituzione di libreria/architettura richiede review e aggiornamento delle decisioni, non un'implementazione improvvisata.

## Architecture summary

`curl/browser → HTTPS vhost esistente → QUIC oppure TCP TLS → client bore → HTTP loopback 127.0.0.1:0 → sorgente`. Nessun nuovo registro o messaggio server; nessun riuso del protocollo applicativo `transfer web`. HTTP, sorgenti e supervisore vivono in nuovi moduli `src/transfer_link/`; CLI orchestration in `src/transfer_link_cli.rs`. Hook opzionali in `Client` conservano URL, espongono il percorso realmente usato e supervisionano i task della sola sessione Link.

## Interface

| Superficie | Nome | Tipo/valori | Default | Regole |
|------------|------|-------------|---------|--------|
| CLI | `bore transfer link [PATHS...]` | `Vec<PathBuf>` | nessuno | ≥1 path; conflitto stdin/exec; singolo file originale, altrimenti ZIP. File che iniziano con `-`: usare `./-nome`. |
| CLI/env | `--to`, `BORE_SERVER` | endpoint | `DEFAULT_SERVER` di main.rs | Solo schema TLS supportato da Endpoint, es. `https://host[:port]`; rifiutare plain prima del connect. |
| CLI/env | `--secret`, `BORE_SECRET` | stringa opzionale | nessuno | Riusa autenticazione bore; env nascosta nell'help; mai loggare. |
| CLI | `--ca-cert` | percorso PEM opzionale | trust webpki esistente | CA pubblica aggiuntiva del controllo/carrier TLS; stesso hostname verificato; nessuna chiave privata. |
| CLI | `--relay-only` | bool | false | Disabilita QUIC, conserva TLS sul controllo e tutti i carrier TCP. |
| CLI | `--carriers` | u16, 0..32 | 1 | 0 = auto esistente; rispettare anche clamp server. Nessun striping di un singolo HTTP download. |
| CLI | `--filename` | nome UTF-8 singolo componente | basename file / `download.zip` | Obbligatorio stdin/exec; niente slash/backslash, CR/LF, NUL, `.`/`..`; ≤255 byte UTF-8. |
| CLI | `--stdin` | bool | false | Nessun path/exec; stream originale, nessun file temporaneo. |
| CLI | `--exec -- COMMAND [ARGS...]` | bool + `Vec<OsString>` dopo `--` | false | Nessun path/stdin; comando non vuoto; nessuna shell; Unix; stdin child nullo. |
| CLI | `--max-downloads` | 1..256, opzionale | risolto a 8 oppure 1 | Stdin/exec accettano solo 1; valore di default condizionale, non Clap default 8. |
| CLI | `--stats-interval` | secondi interi 1..60 | 1 | Aggiornamento stderr; non condiziona heartbeat/rete. |
| CLI esistente | `-v`, `-vv`, `RUST_LOG` | tracing globale | esistente | Nessun secondo sistema debug; stdout solo URL, newline e flush dopo readiness. |
| HTTP | GET/HEAD sul path pubblicato | percent-encoding canonico | — | Nessun listing; query rifiutata, path diverso 404, altri metodi 405. HEAD non riserva stream. |

Non aggiungere `--insecure`, `--finename`, `--relay only`, compressione, spool, password del link, Range o comando shell implicito. Flag aggiunti soltanto nella fase che li rende operativi.

## Protocol and data-structure changes

| Cambiamento | Forma | Compatibilità |
|-------------|-------|---------------|
| Wire bore | Nessuno: `HelloVhost`, `VhostReady`, carrier/direct esistenti | Server vhost compatibile; HTTPS e autenticazione già presenti. Server senza HTTPS rifiutato dal nuovo comando. |
| `Client` | URL vhost conservate/accessor; hook opzionali per scope task e backend peer→path | Costruttori legacy inizializzano None; nessun cambiamento dei marker o bytes sul wire. |
| HTTP | Hyper HTTP/1.1 + body fallibile; ZIP/STORED64 o stream originale | Client HTTP standard; fallimento tardivo chiude corpo senza successo HTTP completo. |
| Persistenza | Nessuna nuova persistenza payload/identità | Solo eventuali artefatti dei test; nessuno spool in produzione. |

## Phases

| Fase | File | Primary assignment | Shippable alone? |
|------|------|--------------------|------------------|
| 0 — Fondazioni HTTP e sorgente file | [phase_01.md](phase_01.md) | `agent-2:sonnet` | Sì, additiva senza CLI nuova |
| 1 — Link pubblico, TLS e lifecycle | [phase_02.md](phase_02.md) | `agent-2:sonnet` | Sì, file singolo completo |
| 2 — ZIP streaming e file multipli | [phase_03.md](phase_03.md) | `agent-2:sonnet` | Sì, aggiunge directory/multipli |
| 3 — Stdin, exec e backup con sudo | [phase_04.md](phase_04.md) | `agent-2:sonnet` | Sì, aggiunge stream monouso |
| 4 — Fault, banda, Docker e accettazione | [phase_05.md](phase_05.md) | `agent-2:sonnet` | Sì, chiude accettazione completa |

## Reuse map

| Necessità | Riuso | Anchor al commit di ricognizione |
|-----------|-------|----------------------------------|
| Registrazione/transport | `Client::new_vhost_provider_with_udp`, `listen` | `src/client.rs:609`, `:984` |
| URL effettiva | ricezione `ServerMessage::VhostReady` | `src/client.rs:683` |
| Path reale e task | `handle_connection`, `spawn_handle`, `spawn_direct` | `src/client.rs:1616`, `:1670`, `:1745`; terzo caller `:2130` |
| TLS e redirect | `Endpoint::parse`, `transport::connect`, `ProviderMeta` | `src/transport.rs:145`, `src/client.rs:35`, `src/vhost.rs:932` |
| Fallback e forwarding | `relay_vhost`, splice | `src/vhost.rs:1410`, `:1579` |
| CLI e shutdown | `TransferCommand`, dispatch, shutdown_signal | `src/main.rs:1104`, `:2153`, `:1728`, `:1746` |
| Reconnect/log | `reconnect::run`, tracing stderr | `src/reconnect.rs:79`, `src/main.rs:3339` |
| Test | TLS vhost, CLI stdin, netns | `tests/vhost_test.rs:217`, `tests/transfer_stdin_cli_test.rs:184`, `.github/workflows/e2e_netns.yml:80` |
| Docker reale | immagine client scratch | `docker/Dockerfile.client:32` — nessun tar/shell/sudo |

Anchor verificati su `dev`, `f74a14d`; risolvere per simbolo se le righe cambiano. TokenSave segnalava due file stale: ricognizione confrontata con sorgenti correnti, indice non risincronizzato.

## References

| # | Contratto verificato | Fonte | Versione/data |
|---|----------------------|-------|---------------|
| R1 | GET/HEAD, Range ignorabile, representation metadata | https://www.rfc-editor.org/rfc/rfc9110.html | RFC 9110, consultato 2026-09-19 |
| R2 | Body incompleto: Content-Length corto / chunk finale assente | https://www.rfc-editor.org/rfc/rfc9112.html | RFC 9112, consultato 2026-09-19 |
| R3 | Wildcard TLS copre un label | https://www.rfc-editor.org/rfc/rfc9525.html | RFC 9525 |
| R4 | Gruppi processi, kill_on_drop non sostituisce wait/reap | https://docs.rs/tokio/1.52.3/tokio/process/struct.Command.html | Tokio lock 1.52.3 |
| R5 | HTTP builder timeout richiede timer; buffer minimo 8192 | https://docs.rs/hyper/1.10.1/hyper/server/conn/http1/struct.Builder.html | Hyper lock 1.10.1 |
| R6 | Stream ZIP, close entry/archive, force_zip64, adapter Tokio | https://docs.rs/async_zip/0.0.18/async_zip/base/write/struct.ZipFileWriter.html | Nuova dipendenza proposta 0.0.18, API consultata |
| R7 | ZIP descriptors, directory centrale, ZIP64 | https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT | APPNOTE consultata 2026-09-19 |
| R8 | GNU tar: -p estrazione, ownership, ACL/xattrs | https://www.gnu.org/software/tar/manual/tar.html | GNU tar 1.35 anche provato localmente |
| R9 | Argomenti esatti e processo figlio | https://doc.rust-lang.org/std/process/struct.Command.html | Rust std, consultato 2026-09-19 |
| R10 | Segnali a gruppo Unix | https://docs.rs/nix/0.29.0/nix/sys/signal/fn.killpg.html | nix diretto 0.29.0, richiede feature signal/process |
| R11 | Body da stream di frame fallibili | https://docs.rs/http-body-util/0.1.3/http_body_util/struct.StreamBody.html | http-body-util lock 0.1.3 |

## Invariants

- **I-1:** nessun payload su disco server o spool locale; memoria limitata anche con stdin infinito e downloader lento.
- **I-2:** ogni tratta esterna cifrata; nessun downgrade HTTP/plain TCP; server vede plaintext, documentarlo.
- **I-3:** successo del body soltanto dopo sorgente valida; exec richiede exit 0; errore tardivo deve risultare errore anche a curl/wget.
- **I-4:** stdout del produttore binario invariato; stderr separato; nessuna shell né perdita privilegi; cleanup e reap obbligatori.
- **I-5:** monouso è una transizione atomica; HEAD non consuma, nessun replay/restart automatico.
- **I-6:** perdita QUIC non migra un body già iniziato; fallback esistente prima dello splice; statistiche mostrano path osservato, non dedotto dalla sola registrazione.
- **I-7:** nessuna regressione vhost/public/secret/SSH/web: protocollo, heartbeat bounded, shared UDP endpoint, pool/Weak ownership, autotuning TCP, half-close e flush TLS conservati.
- **I-8:** nessuna promessa di snapshot, preservazione completa ZIP Unix o salvataggio su B; hash confrontabile sul destinatario.
- **I-9:** limiti di task/FD/buffer/manifest; registrazione pronta prima dell'URL; child/task asincroni ripuliti alla chiusura. Un thread stdin std bloccato termina con l'uscita OS del CLI, mai promessa di join cancellabile né uso come API in-process riutilizzabile.
- **I-10:** tutti i gate realmente eseguiti; test privilegiati/netns seriali; mai dichiarare PASS una prova non eseguibile.

## Risk register

| Rischio | Mitigazione/prova |
|---------|------------------|
| HTTP 200 apparentemente riuscito su tar exit 2 | Body fallibile + niente terminatore prima di wait; T-LINK-EXEC-FAIL |
| Ultimi bytes inviati prima del controllo file | Trattenere ultimo chunk fino a validazione finale; T-LINK-MUTATION |
| Task detached trattengono Client/label | Scope opzionale, join/abort bounded, T-LINK-CLEANUP |
| stdin blocca shutdown runtime | Thread std dedicato bounded, mai tokio stdin/spawn_blocking non cancellabile; T-LINK-STDIN-CANCEL |
| ZIP64/directory centrale consumano RAM | Libreria streaming, limiti manifest, T-LINK-ZIP64/T-LINK-MEMORY |
| Degrado throughput per copie/log/UDP buffer | Chunk bounded, log periodico, riuso endpoint tarati; confronto vhost T-LINK-PERF |
| Docker scratch senza tar | Pipe host `-i` senza TTY; exec nativo o immagine derivata esplicita; T-LINK-DOCKER |

## Verification summary

Comandi canonici, identici a STATE §3 e richiamati dalle fasi; comandi NEW attivi solo dopo la relativa introduzione.

| Gate | Comando | Quando |
|------|---------|--------|
| G-FMT | `cargo fmt --all -- --check` | Ogni unità |
| G-LINT | `cargo clippy --all-features --all-targets -- -D warnings` | Ogni unità codice |
| G-BUILD | `cargo build --locked --all-features` | Ogni fase |
| G-UNIT | `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak` | Ogni unità codice; tutti i test non separati |
| G-SERIAL | `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | Ogni fase |
| G-LINK | `cargo test --all-features --test transfer_link_test -- --test-threads=1` | NEW da fase 0 |
| G-NOUDP | `cargo test --no-default-features --test transfer_link_test -- --test-threads=1` | NEW da fase 1 |
| G-E2E | `bash scripts/transfer_link_e2e.sh basic` | NEW da fase 1; include solo casi introdotti |
| G-ROOT | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/transfer_link_privileged_test.sh all` | NEW fase 3, completato fase 4 |
| G-LARGE | `bash scripts/transfer_link_e2e.sh large` | NEW fase 2; chiusura fase 2 e finale |
| G-DOCKER | `bash scripts/transfer_link_container_test.sh` | NEW fase 4 |
| G-FULL | `cargo test --all-features -- --test-threads=1` | Regressione finale fase 4 |
| G-VHOST | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test.sh` | Regressione netns finale, seriale |
| G-VHOST-HARD | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_netns_test_hard.sh` | Regressione netns finale, seriale |
| G-VHOST-UDP | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/vhost_udp_concurrency_repro.sh` | Regressione netns finale, seriale |
| G-SSH-ROOT | `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/ssh_gateway_test.sh` | Regressione Client condiviso finale, seriale |

**Acceptance:** T-LINK-RAW/T-LINK-CONCURRENT provano il riferimento file ripetibile; T-LINK-ZIP/T-LINK-ZIP64 cartelle; T-LINK-STDIN e T-LINK-EXEC-SUDO il riferimento stream e TAR; T-LINK-EXEC-FAIL impedisce falso successo; T-LINK-QUIC/T-LINK-FALLBACK/T-LINK-DROP trasporto; T-LINK-MEMORY/T-LINK-PERF limiti e banda. Elenco completo in STATE §11; istruzioni/oracoli nelle fasi.

**Run caveats:** costruire sempre il binario della revisione corrente; test CA locale via `--ca-cert` sul mittente e `--cacert` su curl, porte assegnate dinamicamente, root/netns seriali, nessun `pkill` globale. I test di questo piano non sono stati eseguiti: si sta consegnando un piano.

## Model-assignment summary

Assegnazioni legacy del progetto/skill: `agent-1:opus` architettura/review; `agent-2:sonnet` implementazione; `agent-3:haiku` documentazione/meccanica. Sono ruoli per l'esecuzione futura, non dichiarazioni di modelli invocati. Questa pianificazione e la ricognizione delegata sono state svolte da Codex con il modello della sessione; se un host non offre gli alias configurati, registrarne l'errore e concordare/mappare esplicitamente il roster, senza sostituzioni silenziose.

| Fase | Sottofasi per assegnazione | Primary | Gate agent-1 |
|------|----------------------------|---------|--------------|
| 0 | 0.1–0.3 sonnet; 0.4 haiku | agent-2:sonnet | HTTP framing, bounded producer, asserzioni |
| 1 | 1.1–1.4 sonnet; 1.5 haiku | agent-2:sonnet | hook lifecycle, TLS, CLI, fault |
| 2 | 2.1–2.3 sonnet; 2.4 haiku | agent-2:sonnet | filesystem, ZIP64, memoria |
| 3 | 3.1–3.4 sonnet; 3.5 haiku | agent-2:sonnet | monouso, process groups, sudo, false-success |
| 4 | 4.1–4.3 sonnet; 4.4 haiku | agent-2:sonnet | fault, perf, accettazione e README finale |
