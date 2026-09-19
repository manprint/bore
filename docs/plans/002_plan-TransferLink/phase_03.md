# Phase 2 — ZIP STORED/ZIP64, directory e selezioni multiple

> **Intent:** estendere Link a file/cartelle misti, generando ZIP durante ciascun download senza archivio temporaneo.
> **Shippable alone?** sì, estensione del comando file già operativo.
> **Preconditions:** fase 1 (`phase_02.md`) DONE; HTTP fallibile, TLS, scope task, statistiche e harness basic presenti.

## State contract (mandatory)

1. Leggere [STATE.md](STATE.md); con §1 OPEN finire/annullare quell'unità usando §6. Eseguire gate disponibili §3 e confrontare §§1/7/11; repository prevale sulle dichiarazioni.
2. Aprire sottofase in §1 prima di editare (Type sub-phase, ID, OPEN, Intent, Assigned, Next action); §6 `claimed — nothing written yet`.
3. Gate verdi → ledger §4 append-only, §§5,7–11 aggiornati, §6 `none — tree consistent`, prossimo ID/none in §1 e timestamp. WIP commits off, nessun commit/push.
4. Interruzione → lasciare OPEN, elenco preciso file/resti/gate in §6.

## Contratti filesystem/archivio

Singolo file regolare: body originale (fase1). Qualunque directory oppure più elementi: ZIP con metodo STORED, zero compressione, filename default `download.zip`, override --filename consentito. Ciascun GET ha writer/CRC/offset indipendenti. Nessun TAR automatico, nessun archivio/cache sul server o su A. Il TAR di un futuro --exec attraverserà invariato, senza essere inserito in ZIP.

Preparare un manifest metadata prima della pubblicazione: radici nominate per basename, directory incluse (anche vuote), entry in ordine deterministico. Massimo100.000 entry e32MiB cumulativi di path sorgente/nomi codificati; superamento → errore prima dell'URL. Il manifest non contiene payload. Una volta pubblicato il link il set resta quello; cambiare sorgenti richiede nuova invocazione. I controlli rilevano cambi osservabili, non offrono snapshot atomico.

Policy deliberata: rifiutare symlink (anche radici), FIFO/socket/device e nomi entry non UTF-8; rifiutare path ZIP assoluti, segmenti vuoti/`.`/`..`, backslash, controlli, collisioni e radici con basename identico anche senza distinzione ASCII maiuscole/minuscole. Rifiutare selezioni sovrapposte che duplicano il medesimo path relativo. Hardlink di file regolari: contenuto duplicato in due entry, non preservation del collegamento. Non promettere restore di owner/ACL/xattrs tramite ZIP.

## Sub-phases

### 2.1 Manifest stabile e selezione sicura

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review filesystem/memoria; agent-2 implementa.
- **Files:** `src/transfer_link/source.rs`, `src/transfer_link/manifest.rs` NEW, `src/transfer_link/mod.rs`, `src/main.rs:1104` Link introdotto fase1, `tests/transfer_link_test.rs`.
- **Change:**
  1. Estendere selezione a Vec<PathBuf> non vuoto; regola singolo file originale, altrimenti PreparedZip. Non cambiare comportamento/byte del file singolo. Directory nuova ulteriore non necessaria: restare in src/transfer_link.
  2. Traversal iterativo, niente ricorsione Rust illimitata; depth massimo256 e check overflow su conteggi/dimensioni. Usare symlink_metadata, non canonicalize per nascondere symlink. Root path può essere assoluto/localmente annidato ma nome archivio è solo basename + discendenti.
  3. Validare nomi e collisioni prima della pubblicazione; nessuna deduplica silenziosa, nessun suffisso inventato. Per elementi con basename uguali dire esattamente quali collidono. Entry directory terminano `/`; nomi validi percent-encoded soltanto nell'URL finale, non dentro i nomi ZIP.
  4. Registrare fingerprint file e directory, directory membership ordinata o digest metadata dei figli; verificare identità prima/dopo traversal per cogliere sostituzioni. Non seguire link apparsi in corso d'opera. Se i filesystem mutano mentre si prepara, interrompere e chiedere sorgenti stabili.
  5. Imporre limiti prima di push/allocazione della prossima entry; conteggiare anche byte dei path OS e nome ZIP, non solo numero entry. Directory centrale della libreria sarà O(entry), documentare/testare questo costo; non descrivere ZIP come memoria O(1).
  6. Per ogni GET validare manifest contro sorgenti prima degli header per errori già presenti; durante letture usare fingerprint dell'handle (fase0), e prima di finalizzare ZIP ricontrollare file/directory/membership. Non ricostruire automaticamente un manifest diverso nello stesso link.
  7. Un errore di stabilità invalida la disponibilità della sorgente della sessione: nuovi GET503 con istruzione rilanciare A; gli attivi devono fallire al proprio controllo, mai fingere snapshot. Non toccare fd di altri task per forzarne la lettura.
- **Unit tests:** `single_file_remains_original`; `mixed_roots_have_expected_archive_paths`; `empty_dirs_survive_manifest`; `symlink_fifo_and_device_are_rejected`; `duplicate_basename_is_explicit_error`; `path_escape_and_non_utf8_entry_rejected`; `manifest_limits_checked_before_growth`; `directory_add_remove_rename_invalidates_manifest`.
- **e2e tests:** T-LINK-ZIP (manifest) — input misto e cartella vuota danno nomi attesi; input invalido fallisce prima dell'URL. T-LINK-MUTATION — variazione membership durante invio rilevata come errore, non ZIP parziale riuscito.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK verdi; limiti/policy coperti; unità chiusa in STATE §§1/4/6/11.

### 2.2 Writer ZIP streaming e finalizzazione subordinata al successo

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review API/ZIP64/hot path; agent-2 implementa.
- **Files:** `Cargo.toml:66`, `Cargo.lock`, `src/transfer_link/zip.rs` NEW, `src/transfer_link/source.rs`, `src/transfer_link/http.rs`, `tests/transfer_link_test.rs`.
- **Change:**
  1. Aggiungere `async_zip = 0.0.18`, default-features=false, feature tokio soltanto. Non abilitare deflate/bzip2/zstd né usare librerie ZIP sincrone con seek finto. Prima di procedere verificare crate risolta/API e compatibilità MSRV/build; nessun upgrade indiscriminato del lock.
  2. R6: https://docs.rs/async_zip/0.0.18/async_zip/base/write/struct.ZipFileWriter.html — `with_tokio`, `force_zip64`, `write_entry_stream`, close entry e close archive. R7: https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT — data descriptors e directory centrale sono parte della rappresentazione, non metadati opzionali.
  3. Writer riceve un AsyncWrite bounded verso il body pipeline, oppure un tokio duplex di capacità256KiB con pump posseduto verso la coda HTTP. Scegliere UNA sola pipeline, enumerarne tutti i buffer nel test. Nessun vec completo in write_entry_whole per i file. Entry vuote possono essere whole con slice vuota.
  4. ZIP STORED per ogni entry; `ZipEntryBuilder::new(name, Compression::Stored)` e size nota u64 quando appropriato. `force_zip64()` garantisce strutture finali ZIP64, ma da solo non prova correttezza della local header dei file grandi: verificare writer stream della versione0.0.18 e testare realmente >4GiB. Non implementare CRC/descriptor manualmente per aggirare errori della libreria.
  5. Copiare file con routine di lettura stabile bounded della fase0. Chiudere entry SOLO dopo controllo lettura/size/fingerprint; propagare ogni errore di close. Directory entries zero byte, nomi con slash finale. Metadata ZIP opzionali solo se API verificata; non promettere ownership/ACL. Le date non necessarie possono usare default deterministico.
  6. Prima di writer.close eseguire controllo finale manifest; poi attendere close e pump. Solo dopo tutti i successi emettere Complete verso body HTTP. Drop/panic/EOF del writer senza segnale Complete deve produrre Error, anche se il pump ha già visto EOF; un risultato separato della produzione governa la finalizzazione.
  7. HTTP: application/zip, attachment, Content-Length omesso, size_hint sconosciuto (niente stima ZIP dai size file); Hyper gestisce chunked. Rifiutare GET ZIP HTTP/1.0 con505 prima di avviare writer: un body close-delimited non distinguerebbe errore da EOF riuscito. SHA-256 include TUTTI i bytes ZIP, header/descriptor/directory finale compresi. Dimensione finale nota solo a fine produzione; progress non usa somma file come totale ZIP esatto.
  8. Download paralleli non condividono writer/offset/hasher; condividono soltanto manifest immutabile e semaphore. Drop downloader cancella il proprio writer/pump e libera permit/FD senza influenzare gli altri.
- **Unit tests:** `stored_entries_have_no_compression`; `zip_error_never_emits_complete`; `archive_close_failure_reaches_http_body`; `zip_digest_includes_central_directory`; `parallel_zip_writers_are_independent`; `zip_pipeline_buffers_are_bounded`.
- **e2e tests:** T-LINK-ZIP — curl scarica archivio; decoder indipendente Python zipfile e unzip verificano nomi, metodo0, contenuto/hash, directory vuota, secondo GET e3 GET concorrenti. T-LINK-INCOMPLETE — file rimosso/read error/close error producono curl nonzero e nessun completed log.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK/G-E2E verdi; niente spool o Compression diversa da Stored; unità chiusa in STATE §§1/4/6/11.

### 2.3 ZIP64 e memoria con prove effettive

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus valida che l'oracolo sia indipendente e attraversi la vera soglia; agent-2 implementa.
- **Files:** `scripts/transfer_link_e2e.sh` da fase1, `scripts/transfer_link_sparse_sink.py` NEW (test utility soltanto), `tests/transfer_link_test.rs`.
- **Change:**
  1. Aggiungere modalità `large` al harness: file sorgente sparse di 4GiB+1 byte e almeno un'altra entry; trasmettere REALMENTE tutti i byte via Link/curl. Una struct con size finta o seek oltre4GiB non sostituisce il test del writer.
  2. Per non richiedere spazio disco equivalente, sink test trasforma blocchi tutti-zero in seek/hole e scrive i blocchi nonzero, facendo truncate alla lunghezza finale. Questa utility riguarda SOLO test, mai produzione. Verificare prima sink con fixture piccola contro copia ordinaria, anche blocchi misti e coda zero.
  3. Conservare codici d'uscita di curl e sink separatamente (pipefail/PIPESTATUS), poi aprire ZIP risultante con decoder indipendente; verificare size64 esatta, CRC leggendo chunk bounded, hash del contenuto, offset/directory ZIP64 e numero entry. Nessun read() senza size su file grande.
  4. Secondo caso >65535 entry vuote ma <100000 limite manifest: directory finale ZIP64 valida e count esatto. Questo copre soglia numero entry distinta dalla soglia dimensione.
  5. Misurare RSS A/server durante payload grande e destinatario rallentato; l'aumento dopo warmup non cresce con i GiB trasferiti. Con manifest piccolo e1 download: delta RSS massimo128MiB sul mittente e64MiB sul server; se superato diagnosticare buffer/transport, non allargare soglia senza review/evidenza. Con più download crescita proporzionale al limite configurato, non alla durata.
  6. Rilevare file payload creati nella directory temporanea controllata di A/server e fd di file regolari aperti durante il test; soltanto fixture sorgente, certificati/log harness e output destinatario consentiti. Test deve fallire se compare spool anche se eliminato alla fine (monitorare durante, non solo snapshot finale).
  7. Prerequisiti sparse filesystem/spazio logico e decoder espliciti; strumenti mancanti = non-run/error, non PASS. Il gate large può essere job distinto ma è obbligatorio per chiudere questa fase.
- **Unit tests:** `sparse_sink_preserves_offsets_and_trailing_zeros`; `zip_size_counters_do_not_truncate_u64`; boundary 0xffffffff/0x100000000 e65535/65536 con encoder reale dove sostenibile.
- **e2e tests:** T-LINK-ZIP64 — file >4GiB e >65535 entry decodificati indipendentemente; T-LINK-MEMORY — RSS bounded con payload grande; T-LINK-NOSPOOL — nessun payload temporaneo A/server.
- **Done:** G-LARGE verde con byte realmente trasferiti e decoder indipendente; G-E2E e gate Rust verdi; unità chiusa in STATE §§1/4/6/11.

### 2.4 Update README.md

- **Model:** agent-3:haiku
- **Assignment:** documentazione; agent-1 verifica limiti e affermazioni di integrità.
- **Files:** `README.md:2049` Secure file transfer, nuova sezione Transfer link da fase1, Vhost `:3165` solo se serve cross-link.
- **Change:** aggiungere comandi file/cartelle misti, default download.zip/--filename, ZIP senza compressione e >4GiB, parallelismo con consumo banda/disco/RAM per manifest, limiti100000 entry/32MiB nomi/depth256. Sorgenti stabili richieste; descrivere errori cambiamento e nuovo comando necessario, policy symlink/special/non-UTF8/collisioni, assenza di snapshot e fedeltà Unix ZIP. Distinguere singolo file originale da archivio multiplo. Conservare lingua/struttura/tono, niente dettagli di libreria/moduli/algoritmi/piano.
- **Unit tests:** nessuno, documentazione.
- **e2e tests:** eseguire esempio misto e apertura con unzip; flag/nomi/output allineati all'help reale.
- **Done:** utente sa scaricare e aprire selezione mista dal README; G-E2E/G-LARGE e gate fase verdi; unità chiusa in STATE §§1/4/6/11 e Docs fase2 aggiornata.

## Phase gates

- G-FMT: `cargo fmt --all -- --check`
- G-LINT: `cargo clippy --all-features --all-targets -- -D warnings`
- G-BUILD: `cargo build --locked --all-features`
- G-UNIT: `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak`
- G-SERIAL: `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`
- G-LINK: `cargo test --all-features --test transfer_link_test -- --test-threads=1`
- G-NOUDP: `cargo test --no-default-features --test transfer_link_test -- --test-threads=1`
- G-E2E: `bash scripts/transfer_link_e2e.sh basic`
- G-LARGE: `bash scripts/transfer_link_e2e.sh large`
- Regression guard: file singolo resta byte-identico, vhost/transfer/web e scope legacy invariati.
- README: formati/limiti reali, esempi eseguiti, niente promessa backup Unix attraverso ZIP.

## Phase done criterion

File/cartelle misti scaricabili come ZIP STORED valido anche oltre4GiB e65535 entry, senza spool; mutazioni/letture fallite interrompono HTTP; download paralleli indipendenti. STATE §11 fase2 DONE, tutte le unità chiuse e README aggiornato.
