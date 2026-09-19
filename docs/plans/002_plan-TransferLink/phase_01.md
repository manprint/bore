# Phase 0 — Fondazioni HTTP e sorgente file

> **Intent:** costruire un motore HTTP con completamento verificabile e memoria limitata, senza esporre ancora un comando nuovo.
> **Shippable alone?** sì: moduli additivi, nessun comportamento CLI esistente cambia.
> **Preconditions:** nessuna fase precedente; baseline `dev` a `f74a14d`. Le righe sono anchor di ricognizione, ricercare i simboli se cambiano.

## State contract (mandatory)

1. Leggere [STATE.md](STATE.md) per intero. Se §1 è OPEN, completare o annullare quell'unità prima di procedere; §6 deve descriverla. Eseguire i gate disponibili §3 e confrontarli con §1/§7/§11; correggere dichiarazioni smentite dal repository.
2. Prima di editare, aprire la sottofase in STATE §1: Type sub-phase, ID, Status OPEN, Intent, Assigned, Next action; §6 `claimed — nothing written yet`.
3. Chiudere solo dopo i gate: appendere ledger §4, aggiornare §§5,7–11, resettare §6 a `none — tree consistent`, §1 al prossimo ID con Status none e timestamp aggiornato. WIP commits off: nessun commit/push.
4. Se interrotti, lasciare OPEN e dettagliare in §6 file scritti, lavoro residuo, test mancanti; non inventare PASS.

## Contesto vincolante

Il prodotto finale sarà un normale link HTTPS via vhost esistente. Il server può leggere il contenuto, ma non lo salva. Sorgenti file/ZIP ripetibili; stdin/exec monouso senza spool. Qui si costruiscono solamente HTTP locale e file singolo. Non copiare il protocollo `web_transfer_protocol.rs:12`: richiede un destinatario speciale. Riusare `sha2` diretto (`Cargo.toml:81`), `bytes` (`:62`), Tokio e URL (`:100`). Non implementare ZIP, vhost o processi in questa fase.

### Contratti da fissare prima del codice

- Directory nuova `src/transfer_link/` giustificata dai moduli di una sola feature; rispettare convenzioni Rust del repository, niente framework parallelo. File: `mod.rs`, `source.rs`, `http.rs`, `stats.rs`; test integrazione `tests/transfer_link_test.rs`.
- `LinkOptions`: filename validato, path HTTP codificato una sola volta, max_downloads risolto, stats_interval; separato dagli argomenti Clap. `SourceKind` distingue File e future Zip/Stdin/Exec; non aggiungere varianti inutilizzate che generano warning: estendere nelle fasi successive.
- `PreparedFile`: percorso selezionato e identità/metadati iniziali, dimensione u64; nessun contenuto in RAM o su disco aggiuntivo. Un handle nuovo per GET, offset indipendenti. `SourceFingerprint`: tipo, size, modified e, Unix, dev/ino/ctime con precisione disponibile; niente confronto basato solo sul basename.
- Limiti: chunk payload 256 KiB, massimo 2 chunk in coda, un chunk di coda trattenuto per validazione finale; 16 KiB buffer parser HTTP, 64 header, header timeout 3 s. Il limite buffer di Hyper NON è il chunk payload. Nessun timeout totale del download.
- `DownloadId` monotono per sessione. `DownloadOutcome`: Completed/Failed/Cancelled, byte di rappresentazione forniti al body HTTP, elapsed, SHA-256 soltanto se sorgente completa valida. È vietato chiamare questi byte «salvati dal destinatario».
- Canale producer→body bounded con messaggi distinti Data(Bytes), Complete(summary), Failed(error). Chiusura del canale senza Complete è errore, mai EOF di successo. Alternativa di tipo privata ammessa solo se conserva questa proprietà e relativa prova.
- Producer e consumer hanno cancellation condivisa e handle attendibili; drop del body sveglia producer bloccato sulla coda. Nessun `read_to_end`, `collect`, payload `Vec` crescente o canale unbounded.

## Sub-phases

### 0.1 Dipendenze, tipi e validazione

- **Model:** agent-2:sonnet
- **Assignment:** implementazione; agent-1:opus approva confini/API e dipendenze prima di consolidarli.
- **Files:** `Cargo.toml:62–100` (anchor dipendenze), `Cargo.lock`, `src/lib.rs:15`, `src/transfer_link/mod.rs` NEW, `src/transfer_link/source.rs` NEW, `src/transfer_link/stats.rs` NEW, `tests/transfer_link_test.rs` NEW.
- **Change:**
  1. Verificare baseline e nomi test separati della CI in `.github/workflows/ci.yml:62`; aggiornare simultaneamente comandi canonici overview/STATE/fasi se un nome è cambiato. Non «sistemare» test esistenti per renderli verdi.
  2. Rendere dirette le dipendenze HTTP già nel lock: `hyper = 1.10.1` con feature server/http1, `hyper-util = 0.1.20` con tokio, `http-body-util = 0.1.3`; `http-body = 1.0.1` soltanto se un adapter implementa direttamente Body. Usare la versione semver dichiarata con lock coerente, senza aggiornamento globale. `hyper::body::Frame`/Bytes e body fallibile secondo R5/R11. Controllare MSRV/build matrix prima di accettare il lock.
  3. Aggiungere module export documentato in lib.rs, rispettando `forbid(unsafe_code)`/`warn(missing_docs)`. Esportare soltanto ciò che serve a CLI/test; helper privati hanno unit test nel modulo.
  4. Validare filename come singolo componente UTF-8 ≤255 byte, non vuoto, non `.`/`..`, nessun separatore/controllo. Input file con basename non rappresentabile: errore con istruzione `--filename`; il path OS rimane PathBuf, non conversione lossy per l'apertura.
  5. Generare path percent-encoded per segmento, non URL-encode dell'intero URL. Formare Content-Disposition attachment con fallback ASCII sicuro e filename* UTF-8 percent-encoded; mai interpolare stringhe non validate in header.
  6. Risolvere MIME conservativamente: file/stream `application/octet-stream` (ZIP fase 2 `application/zip`). Il nome backup.tar non deve causare trasformazioni.
- **Unit tests:** `filename_rejects_controls_and_separators`; `filename_spaces_unicode_percent_roundtrip`; `os_path_is_not_lossily_rewritten`; `limits_reject_zero_and_overflow`; ogni test verifica input e risultato esatto.
- **e2e tests:** nessuno ancora: nessun comando o listener operativo; il target integrazione nuovo deve comunque essere compilabile.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK verdi, lock senza upgrade estranei; contratti approvati; unità chiusa in STATE §§1/4/6/11.

### 0.2 Producer file bounded e completamento

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review del completamento/hot path; agent-2 implementa.
- **Files:** `src/transfer_link/source.rs` NEW da 0.1, `src/transfer_link/stats.rs` NEW da 0.1, `tests/transfer_link_test.rs` NEW da 0.1; confrontare solo concettualmente `src/transfer.rs:1364` (non riusare Frame custom).
- **Change:**
  1. Aprire file regolare, ottenere fingerprint iniziale, rifiutare directory/symlink/file speciali. Usare controlli di tipo prima e dopo open; su Unix apertura no-follow/nonblocking con API sicure già disponibili, così una sostituzione con FIFO non blocca il runtime. La validazione dopo open deve confrontare l'identità del file atteso prima di inviare contenuto.
  2. Leggere al massimo size iniziale, conteggio u64 checked; SHA-256 incrementale dei byte effettivi, ordine originale. Al termine tentare lettura di un byte extra, verificare numero letto, fingerprint dell'handle e del percorso.
  3. Trattenere l'ultimo chunk non vuoto fino al successo di tutti i controlli finali. Se size=0, controlli finali prima di fornire la risposta vuota. Questo impedisce che un Content-Length già soddisfatto mascheri un errore scoperto subito dopo.
  4. Pubblicare Complete soltanto dopo dati finali validati. Su read/stat/size/change error: Failed, cancellare, rilasciare handle. Non seguire/ripreparare automaticamente un file cambiato: richiedere nuovo comando A.
  5. Coda bounded 2×256 KiB; lettura successiva subordinata a capacità downstream. Ricevitore chiuso/cancellation interrompe la produzione; i task sono posseduti, mai lasciati senza join nel test harness.
  6. Distinguere metrica bytes_read/hash da bytes_for_http; digest completo è pubblicabile solo come esito sorgente, successo invio attende anche fine connessione HTTP. Nessun ACK disco inventato.
- **Unit tests:** `file_tail_waits_for_final_validation`; `zero_byte_file_validates_before_success`; `growth_shrink_replacement_are_errors`; `receiver_drop_unblocks_full_queue`; `large_reader_never_queues_more_than_two_chunks`; `sha256_matches_exact_payload`; usare barriere deterministiche per modificare il file fra lettura e validazione, non sleep fortuiti.
- **e2e tests:** T-LINK-MUTATION (porzione locale) — sostituire/troncare file dopo primo chunk e vedere errore terminale, mai Complete; T-LINK-MEMORY (porzione unit) — sorgente logica molto grande con sink lento e high-water mark entro limite, senza allocazione proporzionale ai byte.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK verdi; test negativo fallisce se viene tolto il trattenimento finale; unità chiusa in STATE §§1/4/6/11.

### 0.3 Server HTTP e oracolo di download incompleto

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus approva framing e test di falso successo; agent-2 implementa.
- **Files:** `src/transfer_link/http.rs` NEW, `src/transfer_link/mod.rs`, `tests/transfer_link_test.rs`; `src/admin_http.rs:51` solo convenzioni listener, non copiare parser.
- **Change:**
  1. Listener esclusivamente `127.0.0.1:0`; address effettivo restituito al chiamante. Hyper HTTP/1.1 serve_connection con TokioIo/TokioTimer, keep_alive(false), max_headers(64), max_buf_size(16 KiB), header_read_timeout(3 s). R5: https://docs.rs/hyper/1.10.1/hyper/server/conn/http1/struct.Builder.html; timeout senza timer causa panic, quindi testarlo.
  2. Body costruito da stream fallibile di frame; R11: https://docs.rs/http-body-util/0.1.3/http_body_util/struct.StreamBody.html. Non terminare lo stream su EOF del canale privo di Complete. Non raccogliere prima il contenuto in memoria.
  3. Solo path pubblicato: GET scarica, HEAD restituisce gli stessi metadata applicabili senza aprire/leggere producer, altro path/query 404; metodi diversi 405 con Allow GET, HEAD. Rifiutare GET/HEAD con body dichiarato o Transfer-Encoding, Expect e upgrade, chiudendo la connessione; niente drain illimitato del request body. Range ignorato: intero 200, Accept-Ranges none. Nessun 206/304/ETag.
  4. Header: Content-Disposition attachment; Content-Type; Cache-Control no-store; Referrer-Policy no-referrer; Connection close. File: Content-Length iniziale, con tail validato da 0.2; errore prima degli header → status appropriato, errore dopo → body Error e connessione abortita. Stream futuri: size_hint sconosciuto, niente Content-Length né trailers necessari al client.
  5. Una semaphore download (default 8) si prende con try_acquire per GET: piena →503, nessuna coda senza limite. Separare cap connessioni/header: max_downloads+32, così HEAD/errori non consumano slot di download; a cap connessioni chiudere subito nuova connessione. Permit RAII fino a chiusura di body/connessione.
  6. Trackare connection task e producer task. Il supervisore resta unico proprietario della cancellazione; il body non deve auto-segnalare successo solo perché ha letto Complete: osservare anche esito connessione. L'errore del socket prevale su Completed.
  7. Test TLS/public saranno fase 1; qui usare connessione TCP loopback e parser indipendente. Usare body fittizio che emette bytes e poi Failed per fissare l'oracolo HTTP, non solo assert sulle enum interne.
- **Unit tests:** `head_does_not_read_source`; `range_returns_full_200`; `bad_path_and_methods_do_not_start_source`; `header_deadline_has_timer`; `download_permit_released_on_body_drop`; `closed_producer_without_complete_is_error`.
- **e2e tests:** T-LINK-HTTP — GET esatto, HEAD body vuoto e size corretta, 404/405, header bound/deadline; T-LINK-INCOMPLETE — Content-Length corto e chunked senza Complete risultano risposte incomplete a un client indipendente, mentre empty Complete riesce.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK verdi; nessuna nuova CLI; raw HTTP prova fallimento anche se lo status iniziale era 200; unità chiusa in STATE §§1/4/6/11.

### 0.4 Update README.md

- **Model:** agent-3:haiku
- **Assignment:** verifica documentazione; agent-2 controlla assenza di comportamento annunciato prematuramente.
- **Files:** `README.md:2049` Secure file transfer, `:2257` transfer web, `:3165` Vhost.
- **Change:** nessun cambiamento visibile in questa fase: verificare che README descriva ancora correttamente le funzionalità esistenti e lasciarlo invariato. Registrare verifica in STATE. Conservare lingua, struttura e tono; non inserire roadmap, nomi moduli, algoritmi o riferimenti al piano.
- **Unit tests:** nessuno, verifica documentale.
- **e2e tests:** nessuno nuovo: non esistono esempi Link da pubblicare in questa fase.
- **Done:** README non promette Link; gate della fase verdi; unità chiusa in STATE §§1/4/6/11 e riga Docs fase 0 aggiornata.

## Phase gates

- G-FMT: `cargo fmt --all -- --check`
- G-LINT: `cargo clippy --all-features --all-targets -- -D warnings`
- G-BUILD: `cargo build --locked --all-features`
- G-UNIT: `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak`
- G-SERIAL: `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`
- G-LINK: `cargo test --all-features --test transfer_link_test -- --test-threads=1`
- Regression guard: test transfer/web/vhost/public/secret esistenti verdi; nessuna modifica alle loro aspettative.
- README: verificato ancora accurato; nessuna feature non spedita documentata. Comandi NEW di fasi successive non applicabili, non PASS.

## Phase done criterion

Motore locale bounded, errori sorgente diventano HTTP incompleto, HEAD è innocuo. T-LINK-HTTP/T-LINK-INCOMPLETE e prove locali di mutazione/memoria verdi; nessuna CLI nuova; STATE §11 fase 0 DONE con tutte le unità chiuse.
