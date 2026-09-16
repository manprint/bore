# Phase 5 — Cartelle, ZIP streaming e UX multippeer completa

> **Intent:** supportare offerte di cartelle/selezioni multiple e download per file o per intera offerta come ZIP streaming, mantenendo direct-first, resume e cancellazione.
> **Shippable alone?** yes — completa il flusso funzionale richiesto per tutti i peer della room.
> **Preconditions:** Phase 4 DONE

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`, finish or revert that unit first (§6 says how far it got). Run the gate commands in STATE.md **§3** and check the result against what §1, §7, and §11 claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type: sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to `claimed — nothing written yet`. Write or update the listed tests first or alongside production edits; do not defer them to a later unit.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1 at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP commits are on.

---

## Fixed contracts for this phase

- Un'azione di selezione produce una sola offerta. **Aggiungi file** può produrre `file` o `files`; **Aggiungi cartella** produce `folder`.
- Ogni file di un'offerta può essere scaricato singolarmente in formato grezzo. **Scarica tutto come ZIP** scarica l'intera offerta e compare soltanto per `files` o `folder`. Non esiste ZIP dell'intera room né ZIP che attraversa offerte/fonti diverse.
- Per `mode=raw`, selected entry IDs contengono esattamente un file. Per `mode=zip`, contengono esattamente tutti gli entry ID dell'offerta, incluse directory, nell'ordine manifest.
- Il payload ZIP usa l'entry ID riservato `0xffff_ffff`; gli ID manifest validi sono `0..0xffff_fffe`.
- Il server valida soltanto selection/mode contro manifest. La sorgente genera lo ZIP; server e destinatario non chiedono né ricevono un archivio intermedio dalla sorgente.
- ZIP options obbligatorie: `level:0`, `zip64:true`, `useWebWorkers:false`, nomi ordinati dal manifest, timestamp derivati dal manifest, nessun commento o extra variabile. Lo stesso offer/selection deve produrre byte identici su rigenerazione nello stesso set di browser supportati.
- Il writer ZIP alimenta direttamente il medesimo pipeline chunk→digest→frammento→AES-GCM usato dal file grezzo. Non usa Blob/ArrayBuffer dell'archivio completo.

## Sub-phases

### 5.1 Acquisire cartelle e costruire manifest ad albero portabili

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** implementare i due percorsi browser di selezione cartella con identico manifest finale; fare self-review traversal, symlink, abort e collisioni.
- **Files:** `web/transfer/src/index.html:selection controls — file/folder inputs`; `web/transfer/src/offers.js:selection adapters — traversal`; `web/transfer/src/offer-worker.js:manifest builder — directory entries`; `web/transfer/src/view.js:catalog tree — accessible render`; `web/transfer/tests/unit/folders.test.mjs:new`; `web/transfer/tests/e2e/catalog.spec.mjs:folder cases`; `web/transfer/dist/:generated — rebuilt assets`; `src/web_transfer_protocol.rs:ManifestEntry validation — reserved entry ID`; `tests/web_transfer_test.rs:folder manifest group — server validation`.
- **Change:** **Aggiungi file** usa input multiple e conserva nomi locali; **Aggiungi cartella** preferisce `showDirectoryPicker()` quando disponibile, chiamato direttamente nel click per rispettare user activation, e altrimenti usa `<input type=file webkitdirectory multiple>`. Traversal File System Access è depth-first ma raccoglie prima metadata e poi ordina; massimo 10.000 entry, profondità massima 128 aggiuntiva, AbortSignal controllato ad ogni iterazione. Accettare soltanto handle `file` e `directory`; ignorare/rifiutare symlink o tipi sconosciuti senza seguirli. Nel fallback `webkitRelativePath`, validare ogni componente; empty directory non esposte dall'API non possono essere inventate e la UI lo spiega soltanto quando quel fallback è in uso. Drag/drop directory usa `DataTransferItem.getAsFileSystemHandle` quando disponibile, altrimenti tratta solo file piatti. Il root scelto diventa label ma i path entry iniziano sotto il nome root per folder; file multipli hanno path relativi ai nomi selezionati. Aggiungere entry directory con size 0/root null, comprese directory vuote quando l'API le espone. Normalizzare e rifiutare path usando la stessa funzione pura già testata: niente sostituzioni silenziose. Ordinare con confronto su code point della stringa NFC; ID sequenziali non usano `0xffff_ffff`. Hashare file con concorrenza due e aggregare total bytes checked. Se traversal/hash viene annullato o supera cap, non pubblicare offerta parziale. Il tree UI usa nodi DOM, espansione accessibile, conteggi/dimensioni e preserva grouping per fonte; non legge file per renderizzare. Il server estende validazione selection: raw un file; zip exact full manifest.
- **Unit tests:** `directory_handle_and_webkitrelativepath_produce_same_manifest`; `empty_directories_are_preserved_when_exposed`; `unknown_handle_and_symlink_are_never_followed`; `depth_entry_reserved_id_and_byte_caps_fail_before_publish`; `folder_root_paths_are_canonical_and_sorted`; `cancel_mid_traversal_publishes_nothing`; `drag_drop_fallback_is_flat_and_explicit`; Rust `reserved_zip_entry_id_is_rejected_in_manifest`; `raw_and_zip_selection_sets_are_exact`.
- **e2e tests:** `T-WEB-FOLDER-OFFER` — A seleziona un albero con nested/empty/Unicode/zero-byte file usando handle path in Chromium e directory-input path negli altri motori; B/C vedono lo stesso albero verificato, nessun transfer parte e withdraw rimuove tutta l'offerta.
**Esito 5.1 (2026-09-16, `agent-1:Claude-Opus-5`).** Cartelle acquisite per entrambi i
percorsi browser, con un solo `folders.js` a possedere la API della directory (il tripwire
statico in `state.test.mjs` non la vieta più: la ASSEGNA, come `main.js` possiede
`showSaveFilePicker`). Fatti che valgono la pena di essere scritti:

1. **Una directory guadagna un entry solo se il sottoalbero non ha prodotto NULLA.** La
   prima versione ne emetteva uno per ogni sottoalbero senza file, così una cartella che
   contiene solo una cartella vuota veniva descritta due volte e la seconda descrizione
   era falsa (`root: null` significa "questa directory è vuota", e quella non lo era).
2. **`webkitGetAsEntry` va chiesto DENTRO il gestore del drop.** Una `DataTransferItemList`
   vive quanto l'evento: la domanda posta dopo il primo `await` risponde su una lista che
   il motore ha già svuotato, quindi il messaggio "questo browser non consente di
   trascinare cartelle" non sarebbe mai comparso proprio sui motori che ne hanno bisogno.
3. **L'ordine dell'array `entryIds` NON è l'ordine del manifest.** `parse_entry_ids` impone
   da sempre l'ordine lessicografico canonico (e con 11 entry `"10" < "2"`), quindi
   `validate_selection` verifica l'INSIEME — lunghezza, appartenenza, nessuna ripetizione —
   mentre l'ordine manifest resta quello con cui l'archivio scrive le sue voci. Il contratto
   fisso della fase va letto così.
4. **L'ID riservato `0xffff_ffff` è rifiutato PRIMA della regola di sequenzialità**, così un
   manifest che lo nomina viene respinto per quello che è e non come una numerazione
   sbagliata (red-check: senza il controllo il messaggio parla di "0-based sequential").
5. **Il `zip` è validato ma non ancora trasportabile.** `request_transfer` rifiuta
   esplicitamente `mode: "zip"`: un `TransferRecord` porta `entry_root`/`entry_size` presi
   dal manifest e un archivio non ha né l'uno né l'altro (sono dinamici e autenticati dal
   frame finale). La forma del record è stata assegnata a 5.3, che è la sotto-fase che
   definisce quella tupla.
6. L'albero del catalogo è renderizzato dal solo manifest, con `<details>` nativi (non un
   `role="tree"` fatto a mano: l'elemento nativo è già operabile da tastiera e annunciato
   correttamente sui tre motori) e aperto di default fino a 50 file per directory.

Gate: `npm run check` 137/137; Rust lib 787/0/2; Rust web e2e 22 passed / 1 ignored seriale;
`T-WEB-FOLDER-OFFER` verde su chromium (handle reali presi da OPFS: solo il dialogo del
picker è sostituito), firefox e webkit (percorso `webkitdirectory`); README aggiornato
(pubblicare una cartella è possibile, scaricare un'offerta multi-entry no, e la directory
vuota sopravvive solo dove il motore espone il picker).

- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-FOLDER-OFFER` passa per i percorsi supportati + manifest equivalence fixture è byte-identica escluse empty dirs non osservabili documentate + self-review conferma zero traversal di symlink e zero offerta parziale + closed in `STATE.md` (§1 → 5.2, §4 ledger row, §6 `none`, §11 board).

### 5.2 Generare ZIP64 deterministico direttamente nello stream cifrato

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** integrare ZipWriter con un sink streaming bounded e opzioni deterministiche fissate; fare self-review che nessun ramo materializzi l'archivio completo.
- **Files:** `web/transfer/src/zip-stream.js:new — ZipWriter adapter`; `web/transfer/src/sender.js:source abstraction — raw/zip`; `web/transfer/src/framing.js:archive entry — reserved ID`; `web/transfer/tests/unit/zip.test.mjs:new`; `web/transfer/tests/e2e/download.spec.mjs:zip cases`; `web/transfer/dist/:generated — rebuilt assets`; `web/transfer/package.json:dependencies — zip.js exact pin`; `web/transfer/package-lock.json:zip.js package — resolved pin`.
- **Change:** importare soltanto l'API necessaria da `@zip.js/zip.js` 2.15.0 e costruire `ZipWriter` sopra un `WritableStream` custom. Il sink accumula al massimo un chunk logico da 1 MiB, calcola SHA-256/root con la formula file già fissata usando entry riservato, lo passa al frame sender e rispetta il suo backpressure prima di accettare altro output. Configurare writer `level:0`, `zip64:true`, `useWebWorkers:false`; per ogni entry manifest in ordine aggiungere directory con slash finale o file stream. Impostare `lastModDate` dal `lastModifiedMs` manifest normalizzato alla precisione ZIP, disabilitare timestamp estesi/commenti/extra platform-dependent mediante le opzioni disponibili della versione pin; fissare nome UTF-8 e niente password ZIP. Prima di generare, verificare ancora File.size/lastModified e durante lettura ricalcolare root originale: qualsiasi divergenza annulla con SOURCE_CHANGED e ritira offerta. Il nome output è `<label-sanitized>.zip`: sanificazione solo locale rimuove slash/control, trimma, fallback `bore-transfer`, massimo 120 caratteri; non altera i path interni già validi. `ZipWriter.close()` deve completare central directory nel sink prima di `TRANSFER_DONE`; abort non chiama finalizzazione riuscita. Il root/length dell'archivio è dinamico e autenticato dal frame finale, non aggiunto al manifest server. Per test >4 GiB usare una source sintetica lazy e un sink counting che verifichi record Zip64 senza allocare/scrivere 4 GiB; non ridurre la soglia nella produzione. R13 — ZipWriter accetta WritableStream e supporta Zip64 ([API](https://gildas-lormeau.github.io/zip.js/api/classes/ZipWriter.html)).
- **Unit tests:** `zip_options_are_store_zip64_no_worker_and_no_variable_extras`; `zip_entries_follow_manifest_order_with_safe_paths`; `same_manifest_and_files_generate_identical_bytes_twice`; `zip_sink_never_buffers_more_than_one_logical_chunk`; `zip_root_and_length_cover_full_archive`; `zip64_records_are_used_for_synthetic_large_source_without_large_allocation`; `source_change_aborts_before_successful_central_directory`; `archive_name_sanitization_is_local_and_bounded`; `empty_directory_and_empty_file_round_trip`.
- **e2e tests:** `T-WEB-ZIP` — B scarica l'intera folder di A via direct, apre ZIP, confronta tree/bytes/timestamp normalizzati; seconda generazione ha hash archivio identico; C ripete via relay; nessun temp/blob archivio appare su A o server.
**Esito 5.2 (2026-09-16, `agent-1:Claude-Opus-5`).** L'archivio si genera dentro lo stream
cifrato e non esiste da nessun'altra parte: il mittente non costruisce mai un `Blob`, non
crea mai un object URL e non scrive mai un file temporaneo (`T-WEB-ZIP` lo verifica
strumentando `URL.createObjectURL` e `Blob` sulla pagina che pubblica). Fatti che valgono
la pena di essere scritti:

1. **`dataDescriptor: false` — l'opzione che il piano fissava — materializza l'intero
   entry in memoria.** zip.js ha bisogno del CRC32 per completare un local header, e
   l'unico modo di averlo prima del payload è leggere prima il payload: con il descriptor
   spento il writer prende il ramo BUFFERED (`zip-writer.js`, `(!dataDescriptor &&
   !emptyEntry)`) attraverso un `TransformStream` con `highWaterMark` INFINITY, e il
   backpressure del sink non raggiunge mai il lettore. MISURATO su un entry da 64 MiB con
   un sink da 20 ms per chunk: il lettore correva **63,0 MiB avanti** al sink a 174 MiB di
   RSS con il descriptor spento, e **0,8 MiB** avanti a 89 MiB di RSS con il descriptor
   acceso — meno di un chunk logico, che è esattamente la promessa della sotto-fase. Lo
   stesso cambio è ciò che rende un abort un abort: abbandonare l'attempt su una sorgente
   da 5 GiB legge 458 KiB in 8 ms invece di 5 GiB in 7 s, perché un entry bufferizzato
   viene letto fino in fondo qualunque cosa dicano il sink o l'`AbortSignal`. Il piano
   diceva "le size sono note prima dei dati, quindi l'header è completo": vero per il
   FORMATO, falso per questa libreria, e il costo non è il formato ma la memoria. Niente
   nel data descriptor dipende dall'host, quindi l'output resta deterministico —
   `same_manifest_and_files_generate_identical_bytes_twice` passa in ENTRAMBE le
   configurazioni, ed è per questo che il difetto è invisibile a un test di determinismo e
   serve `the_source_is_read_no_further_ahead_than_one_logical_chunk` (red-check: con il
   descriptor spento falliscono 3 test e nessuno dei tre parla di byte diversi).
2. **Il frame FINAL ha due forme, non una.** Un archivio è GENERATO: la sua lunghezza, il
   suo numero di chunk e la sua root non sono in nessun manifest e non possono essere
   confrontati con nulla. Il FINAL di un transfer `zip` porta quindi 48 byte
   (`u64be(total) || u64be(chunkCount) || root[32]`) invece degli 8 del `raw`, ed è il
   sigillo AEAD a rendere quella dichiarazione quella della SORGENTE. La regola sta nel
   codec — `FINAL_RAW_LEN`/`FINAL_ARCHIVE_LEN` in Rust, `FINAL_RAW_BYTES`/
   `FINAL_ARCHIVE_BYTES` in `crypto.js` — e i due decoder accettano quelle due lunghezze e
   nessuna terza (unit gemelli nei due linguaggi, un byte in più e uno in meno intorno a
   ciascuna forma). `WEB_TRANSFER_PROTOCOL.md` §6 lo documenta.
3. **`mode` va inoltrato dal server ALLA SORGENTE, e il campo da solo non basta.** Il
   server appende `mode` a `transfer.incoming` (additivo, ultimo), ma il mittente lo
   ignorava e leggeva l'offerta come `raw`: una cartella ha più di un entry servibile,
   quindi `startIncoming` rispondeva `transfer.reject OFFER_NOT_FOUND` e il download
   moriva prima del primo byte. Ora la sorgente ricalcola il selection digest sul proprio
   `mode`, che è ciò che fa fallire una richiesta contraffatta ALLA SORGENTE e non solo al
   server.
4. **`entryIds` viaggia in ordine lessicografico anche per lo `zip`.** `parse_entry_ids`
   rifiuta una lista non ordinata, e il digest copre la lista COSÌ COM'È: entrambi i capi
   ordinano con lo stesso confronto (`"10"` prima di `"2"`), altrimenti il digest del
   destinatario e quello della sorgente non coincidono e il transfer muore come
   `SOURCE_CHANGED`.
5. **La carta dell'offerta mostra un solo bottone, e quale sia lo decide l'offerta.** Un
   file singolo servibile raw mostra `Scarica`; qualsiasi altra cosa — cartella, selezione
   multipla, file vuoto — mostra `Scarica ZIP`. Non è una preferenza: il percorso raw non
   può servire un file vuoto (nessun chunk da verificare) né una directory, e il bottone
   che prima li offriva rispondeva `MULTI_ENTRY`/`OFFER_NOT_FOUND` a chi lo premeva.
6. **Il totale di un archivio è una stima fino al FINAL.** Prima che l'archivio esista non
   esiste la sua dimensione: la barra usa la somma dei byte impacchettati (limite
   inferiore, perché lo store non comprime) e riporta `max(stima, ricevuto)` così non
   torna indietro né supera il proprio fondo; al FINAL la lunghezza autenticata la
   sostituisce. Documentato nel README, perché è visibile all'utente.
7. **Nessun resume per l'archivio in questa sotto-fase.** `adoptAttempt`/`applyCommitPlan`
   ripartono da chunk zero e `resetArchive` azzera leaves e contatori; costa poco proprio
   perché l'output è deterministico. Il resume vero è 5.3.
8. **Self-review memoria (richiesta dal Done).** Nel path ZIP non esiste un `Blob`: gli
   unici `arrayBuffer()` sono `fileReader` (un blocco che zip.js ha chiesto, ≤512 KiB, mai
   il file intero) e `buffer.slice(0, filled)` in `createChunkSink` (una copia VOLUTA:
   consegnare una vista su un buffer riusato sarebbe sbagliato dal primo `await` del
   consumatore). L'unico accumulatore è l'array delle leaves, 32 byte per MiB su entrambi
   i capi — 1,3 MiB per un archivio da 40 GiB — ed è la stessa forma che il manifest ha
   sempre avuto per un file. Il `Blob` del destinatario è quello della staging OPFS, che
   legge le parti su richiesta e non copia nulla.

Gate: `npm run check` 150/150; Rust lib 788/0/2; Rust web e2e 22 passed / 1 ignored seriale;
`T-WEB-ZIP` verde su chromium, firefox e webkit, sia relay sia direct, con hash
dell'archivio identico fra i due percorsi e fra due peer diversi; README aggiornato (il
bottone ZIP, l'archivio costruito mentre viaggia, la stima della barra, e le parole che
rifiutavano il download multi-entry sono state RIMOSSE — `t_web_readme` ora lo verifica in
negativo, non solo in positivo).

- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-ZIP` passa direct e relay + memory instrumentation resta entro ZipWriter state + un chunk plaintext + un frame + self-review cerca `Blob`, `arrayBuffer()` e accumulatori nel path ZIP e giustifica/elimita ogni uso + closed in `STATE.md` (§1 → 5.3, §4 ledger row, §6 `none`, §11 board).

### 5.3 Estendere OPFS, resume e cancellazione agli archivi e alle selezioni

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** estendere il transfer controller ai due mode senza duplicare il protocollo; fare self-review di selection identity, rigenerazione e partial cleanup.
- **Files:** ~~`src/web_transfer.rs:transfer record — zip mode and the dynamic final tuple`~~ (LANDED IN 5.2, 2026-09-16: `T-WEB-ZIP` cannot run at all unless `request_transfer` accepts `mode: "zip"`, so `entry_root`/`entry_size` are now `Option` and a `zip` record carries `None` for both, the expectation having moved to the recipient and the sealed FINAL tuple. See `STATE.md` §8. Nothing about the record shape remains for this sub-phase); `web/transfer/src/storage.js:partial records — archive state`; `web/transfer/src/receiver.js:zip validation — dynamic expected root`; `web/transfer/src/sender.js:zip resume — regeneration and range skip`; `web/transfer/src/main.js:download actions — selection modes`; `web/transfer/src/view.js:offer controls — per-file and per-offer`; `web/transfer/tests/unit/storage.test.mjs:zip cases`; `web/transfer/tests/unit/state.test.mjs:selection cases`; `web/transfer/tests/e2e/download.spec.mjs:zip resume cases`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** per raw mantenere `<entryId>.part`; per zip usare `archive.part` sotto lo stesso path OPFS dell'offerta. `selectionDigest` include mode e lista completa entry e impedisce che partial di file/ZIP o subset diversi collidano. Il pulsante su un file invia raw con un solo ID; il pulsante offer-level invia zip con tutti gli ID esatti. Il receiver ZIP non ha expected size/root nel manifest: persiste i chunk verificati e accetta `TRANSFER_DONE(totalLength,chunkCount,root)` autenticato soltanto dopo sequence/digest coerenti; al primo tentativo salva quei valori come expected, e nei resume successivi richiede identico final tuple oppure SOURCE_CHANGED. Prima del resume rilegge tutti i range marcati e ricostruisce root; se più di 4096 range conserva il prefisso contiguo e tronca/azzera metadata successivo. La sorgente rigenera ZIP dall'inizio con le stesse opzioni, ricalcola ogni output chunk e scarta quelli nei range verificati; continua a includere i loro digest nel root finale. Se bytes/root/length differiscono dal precedente expected, il destinatario scarta il nuovo tentativo e offre ripartenza da zero dopo conferma esplicita, non sovrascrive il partial verificato automaticamente. Cancel interrompe traversal, ZipWriter, stream reader, crypto e transport tramite lo stesso AbortController; conserva solo chunk completi. Withdraw/room close/source changed purgano raw e zip partial. Un peer può avere max 8 trasferimenti complessivi, indipendentemente dal mode.
- **Unit tests:** `raw_and_zip_partials_have_disjoint_selection_keys`; `zip_resume_rehashes_ranges_and_regenerates_from_byte_zero`; `zip_resume_skips_verified_output_without_skipping_source_reads`; `dynamic_final_tuple_is_persisted_then_must_match`; `too_many_sparse_ranges_reduce_to_verified_prefix`; `zip_cancel_keeps_only_complete_chunks`; `withdraw_room_close_source_change_purge_all_selection_partials`; `per_peer_transfer_cap_counts_raw_and_zip_together`.
- **e2e tests:** `T-WEB-ZIP-RESUME` — annullare ZIP al 25%, verificare partial, nessun auto-resume, nuovo click rigenera dall'inizio ma non ritrasmette chunk validi e produce ZIP identico; `T-WEB-ZIP-SOURCE-CHANGE` — mutare un file dopo partial, resume fallisce senza corrompere bytes già verificati e richiede ripartenza esplicita.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + entrambi i gate passano direct e relay + contatori byte dimostrano source reread e network skip distinti + self-review verifica cleanup matrix raw/zip × cancel/withdraw/close/change/complete + closed in `STATE.md` (§1 → 5.4, §4 ledger row, §6 `none`, §11 board).

**Esito 5.3 (2026-09-16, `agent-1:Claude-Opus-5`).** Resume e cancellazione valgono ora
anche per gli archivi e per la selezione di un singolo file dentro una cartella. Quattro
cose che meritano di stare scritte:

1. **Un archivio riprende SOLO per prefisso contiguo, e la regola vive su entrambi i
   lati.** Un chunk di archivio non ha digest nel manifest e non ha identità oltre alla
   propria POSIZIONE in uno stream che la sorgente deve rigenerare: un insieme sparso di
   range non è quindi un'affermazione verificabile. `storage.js:verifiedPrefix` riduce i
   range a `[0, n)` o a niente, e lo applicano **sia il destinatario** (prima di chiedere)
   **sia la sorgente** (prima di saltare) — non uno solo dei due, perché un mittente che si
   fidasse di range sparsi produrrebbe un archivio con buchi il cui root tornerebbe
   comunque corretto. Il piano prevedeva "se più di 4096 range conserva il prefisso
   contiguo": la riduzione è incondizionata, non a soglia, per la stessa ragione.
2. **La sorgente rigenera da byte zero e HASHA tutto, salta solo l'invio.** `sendArchive`
   calcola `sha256` di ogni chunk e lo mette in `leaves` SEMPRE; il ramo `skip` scarta solo
   il fragment + seal + send e contabilizza i byte come `baseBytes`. Così il rolling root
   finale copre anche i chunk che non hanno viaggiato, e i due contatori restano distinti:
   la rilettura della sorgente non cala, il traffico sì. `zip_resume_skips_verified_output_without_skipping_source_reads`
   misura esattamente questa differenza, e `T-WEB-ZIP-RESUME` la ri-misura sul filo
   (`received <= saved.length - n*CHUNK_BYTES`).
3. **La tupla FINAL si confronta con il DISCO, non con il filo.** `acceptArchiveFinal`
   verifica `transfer.archiveBytes !== totalBytes`, non `receivedBytes`: un tentativo
   ripreso trasporta legittimamente meno byte di quanti l'archivio ne abbia. Al primo
   completamento la tupla `{totalBytes, chunkCount, root}` viene persistita nel record; da
   lì in poi una tupla diversa è `SOURCE_CHANGED`.
4. **`SOURCE_CHANGED` è un esito di prima classe e NON distrugge nulla.** Il partial
   verificato resta su OPFS, la pagina scrive "La sorgente è cambiata: la ripresa non è più
   valida", nasconde `Salva file verificato` e mostra `Riparti da zero`: solo quel secondo
   gesto esplicito cancella i byte. Il piano diceva "offre ripartenza da zero dopo conferma
   esplicita, non sovrascrive il partial verificato automaticamente" — è quello, con il
   flag `sourceChanged` in `state.js` pulito da `room.closed`, `offer.removed` e da un
   avvio `fresh`.

**Due difetti reali trovati dai gate, non dalla lettura del codice** (entrambi avrebbero
prodotto trasferimenti silenziosamente bloccati in produzione):

- `commitArchiveChunk` aggiornava solo il record su OPFS e **mai `transfer.verifiedRanges`
  in memoria**. Conseguenze oltre al resume: il fallback mid-transfer verso il relay
  ripartiva da zero (i range riportati erano vuoti) e la riga di progresso non avanzava.
  Trovato da `T-WEB-ZIP-RESUME`, che andava in timeout aspettando `verifiedRanges > 0`.
- Un fallimento terminale **lato destinatario non arrivava mai al server**: il record del
  transfer restava vivo e la richiesta di restart veniva risolta con
  `RequestOutcome::Existing`, cioè con lo STESSO transfer id, quindi la sorgente non
  ripartiva. `failTransfer` ora invia `transfer.cancel` (`notifyServer = false` solo dove è
  il server ad aver annunciato il fallimento, cioè `DIRECT_FAILED`). Trovato da
  `T-WEB-ZIP-SOURCE-CHANGE`, dove il restart non andava mai in staging.

Red-check eseguiti su tre gate nuovi (`dynamic_final_tuple_is_persisted_then_must_match` —
reso discriminante ancorando `expected.totalBytes` a un valore falso con root VERO, perché
altrimenti a rifiutare era il controllo del root; `zip_cancel_keeps_only_complete_chunks`;
`zip_resume_skips_verified_output_without_skipping_source_reads`). Trappola del test
harness da non ripetere: un fragment non può mai scavalcare un confine di chunk (il
destinatario alza "fragment overrun"), quindi il driver di test spezza prima sui confini di
chunk e poi in fragment.

### 5.4 Validare il flusso completo A/B/C e l'isolamento per fonte

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** trasformare lo scenario utente definitivo in test di accettazione multippeer, correggere ogni failure e fare self-review dell'autenticità delle evidenze.
- **Files:** `web/transfer/tests/e2e/multipeer.spec.mjs:final scenario — A/B/C`; `web/transfer/tests/e2e/folder-zip.spec.mjs:new`; `web/transfer/tests/e2e/fixtures.js:peer/file helpers — acceptance fixtures`; `tests/web_transfer_test.rs:concurrent accounting group — transfer isolation`; `scripts/web_transfer_e2e.sh:acceptance mode — process orchestration`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** test finale, in questo ordine osservabile: (1) avviare il CLI owner senza file; (2) aprire il link per A e verificare catalogo vuoto; (3) A seleziona una cartella e soltanto allora compare l'offerta; (4) B apre link, vede albero e non parte alcun download; (5) B clicca **Scarica tutto come ZIP**, riceve direct e verifica contenuto; (6) C apre link con RTC relay-only, clicca lo stesso ZIP e usa relay; (7) B annuncia un file proprio, C annuncia una cartella propria, tutti vedono tutte le offerte; (8) A scarica da B e C, con source IDs corretti; (9) B inizia un secondo download e annulla, mentre uno concorrente continua; (10) il file ottenuto da A non compare come offerta di B finché B non lo seleziona esplicitamente; (11) nessun controllo room-wide “download ZIP” esiste; (12) chiudere owner abortisce tutti e rende URL non disponibile. Verificare output tramite hash file/ZIP, eventi network DataChannel/relay, UI path dopo first chunk, server counters e assenza marker nei log. Eseguire due trasferimenti simultanei dalla stessa fonte e da fonti diverse per convalidare cap/isolation. Ripetere lo scenario con A/B ruoli browser scambiati: chi crea link non ha privilegi browser. Conservare test deterministico con fixture piccole; il test no-store 64 MiB resta separato.
- **Unit tests:** `offer_source_is_original_peer_only`; `cli_owner_has_no_browser_privileges_or_peer_id`; `zip_action_is_scoped_to_one_offer`; `concurrent_transfer_cleanup_isolated_by_transfer_id`; `download_completion_never_publishes_output`.
- **e2e tests:** `T-WEB-MULTIPEER-FINAL` — tutti i dodici passi sopra; `T-WEB-OWNER-SEPARATION` — browser di A può essere assente o entrare dopo B e resta peer ordinario; `T-WEB-SOURCE-ONLY` — recipient non serve/annuncia automaticamente e una ripubblicazione esplicita crea nuova OfferId/source.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + tre gate passano su Chromium e lo scenario principale passa su Firefox/WebKit + ogni asserzione path/payload usa evidenza reale, non solo testo UI + self-review confronta il flusso con l'obiettivo di `overview.md` punto per punto + closed in `STATE.md` (§1 → 5.5, §4 ledger row, §6 `none`, §11 board).

**Esito 5.4 (2026-09-16, `agent-1:Claude-Opus-5`).** I dodici passi vivono in
`tests/e2e/folder-zip.spec.mjs` come un unico test osservabile, più i due gate che dicono
cosa la stanza NON è. Quattro cose da registrare:

1. **Ogni asserzione legge evidenza, non testo.** I byte escono dall'archivio salvato e
   vengono confrontati path per path; il percorso è quello che il destinatario ha
   COMMESSO (`pathCommits`), non l'etichetta della riga; «nessun relay» è la lista degli
   URL WebSocket che la pagina ha davvero aperto; l'attribuzione di una sorgente è il
   `sourcePeerId` del trasferimento (per la gamba lenta) e la notifica di progresso che il
   server inoltra alla SOLA sorgente (per quella veloce). Il testo dell'interfaccia è letto
   in un punto solo, «Room non disponibile», dove il testo È il prodotto.
2. **Un difetto reale nell'harness, causato da questa stessa sotto-fase.** Lo stage `soak`
   di `scripts/web_transfer_e2e.sh` selezionava con `-g T-WEB-MULTIPEER`, e il nuovo
   `T-WEB-MULTIPEER-FINAL` corrisponde a quel prefisso: il criterio anti-flake della 4.4
   avrebbe smesso silenziosamente di misurare lo scenario per cui esiste e avrebbe soakato
   due scenari diversi. Il pattern è ora il titolo completo. Lezione generale: un `-g` per
   prefisso è un accoppiamento fra il nome di un test e il significato di uno stage.
3. **`withdraw_offer` di un'offerta altrui risponde `NOT_PARTICIPANT`, non
   `OFFER_NOT_FOUND`.** Il test è stato scritto aspettandosi il secondo; il prodotto ha
   ragione e il test è stato corretto — l'offerta esiste e ogni peer della stanza la vede
   già nel catalogo, quindi nominarla non rivela nulla, mentre `OFFER_NOT_FOUND` direbbe
   una cosa falsa a chi la sta guardando.
4. **Lo scenario gira su tutti e tre i motori**, non solo su Chromium come il criterio di
   questa sotto-fase avrebbe concesso: costa ~50 s per motore ed è la promessa del
   prodotto. Il `receiverState()` del hook di test porta ora anche `offerId` e
   `sourcePeerId`, perché l'attribuzione va letta dal trasferimento e non da un'etichetta
   che gli sta accanto.

### 5.5 Update README.md

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** documentare cartelle, ZIP per offerta e flusso multippeer definitivo; fare self-review completa come utente e amministratore.
- **Files:** `README.md:290 — command overview`; `README.md:881 — self-hosting`; `README.md:2035 — secure file transfer`; `README.md:3200 — troubleshooting`.
- **Change:** preservare la struttura, il tono e la lingua esistenti del README, modificandolo senza riscriverlo. Rimuovere la limitazione file singolo e descrivere **Aggiungi file**, **Aggiungi cartella**, drag/drop, download del singolo file e **Scarica tutto come ZIP** limitato a una singola offerta/fonte. Spiegare chiaramente che il CLI non seleziona file, che ogni browser può annunciare/scaricare, che gli annunci appaiono progressivamente, che una fonte serve soltanto le proprie offerte e che un download non viene ripubblicato. Fornire lo scenario A/B/C passo per passo: A avvia room e seleziona cartella nel browser; B clicca e usa direct; C con WebRTC/UDP bloccato clicca e usa relay; B/C possono pubblicare; un partecipante annulla il proprio transfer; chiusura A invalida tutto. Documentare ZIP store/Zip64, OPFS/quota/resume, comportamento empty-dir del fallback browser, supporto Chrome/Edge/Firefox/Safari e nessun download automatico. Evitare dettagli di libreria, frame, classi o fasi.
- **Unit tests:** docs/help/link checks più verifica automatica che esempi non suggeriscano path CLI o ZIP room-wide.
- **e2e tests:** `T-WEB-README-FINAL-FLOW` — un operatore segue soltanto README per deploy e scenario A/B/C, inclusi cartella, ZIP, direct, relay, cancel e chiusura.
- **Done:** README consente a un nuovo utente di comprendere ed eseguire il flusso completo senza ambiguità; esempi verificati; tutti i gate verdi; self-review finale della fase; closed in `STATE.md` con §1 → 6.1 e la §11 docs row per Phase 5 `DONE`.

**Esito 5.5 (2026-09-16, `agent-1:Claude-Opus-5`).** Il README descrive ora la UI
definitiva e l'esecuzione completa, e il gate che lo tiene onesto e' cresciuto con lui.
Quattro cose da registrare:

1. **Quello che mancava non era la funzione, era il percorso.** Cartelle, ZIP per offerta,
   resume dell'archivio, prelievo del singolo file e badge del percorso erano gia'
   documentati sotto-fase per sotto-fase; nessuna pagina diceva come si svolge una
   sessione dall'inizio alla fine. La nuova sezione «A complete run, three browsers» sono
   sette passi che un lettore esegue senza altri strumenti — server, room, A pubblica una
   cartella, B la scarica in diretto, C la scarica sul relay, B pubblica a sua volta, un
   annullo e una ripresa, Ctrl+C — e `T-WEB-README-FINAL-FLOW` li esegue tutti e sette
   sui tre motori, con i comandi presi dal README e non da un helper.
2. **Tre fatti dell'interfaccia sono ora promesse scritte.** Le tre zone e il loro ordine,
   il fatto che nulla si sposta durante un trasferimento, e il badge che porta una FORMA
   oltre alla parola (`◌`, `◆`, `▲`). Sono esattamente il genere di dettaglio che una
   modifica successiva sposta senza accorgersi della guida, quindi il gate li pinna come
   gia' pinna la catena STUN di default.
3. **Due regole che il prodotto ha sempre avuto e la guida non diceva:** un browser serve
   soltanto cio' che ha pubblicato lui (ricevere una cartella non ti rende sorgente: appare
   di nuovo nella stanza solo se la pubblichi tu, come offerta tua) e non esiste alcun
   controllo di download «di tutta la stanza» — ogni pulsante appartiene a una offerta e a
   un editore. `T-WEB-README-FINAL-FLOW` verifica la prima contando gli `offer.publish` di
   A dopo che A ha scaricato il file di B.
4. **Due nuovi controlli automatici sul testo.** Ogni link della sezione deve risolvere —
   ancora interna o file su disco — e nessun esempio puo' passare un percorso a
   `bore transfer web`: il comando non seleziona file, e un esempio che ne mostrasse uno
   insinuerebbe che il CLI legge i file dell'utente. Entrambi red-checked (un link rotto e
   un argomento in piu' fanno fallire il gate).

Due difetti nell'harness, stessa famiglia, trovati eseguendo la suite completa sui tre
motori:

- **Il killer del canale diretto sparava troppo presto.** Aspettava «un chunk verificato»,
  ma il chunk e' verificato un istante PRIMA che il percorso venga committato (e per un
  archivio il commit aspetta la scrittura del record). Uccidendo dentro quella finestra il
  prodotto si comporta correttamente — il badge non ha mai nominato il diretto, quindi non
  c'e' niente da riportare indietro — ma lo scenario che il gate esiste per provare non
  avviene: `T-WEB-PATH-UI` leggeva `["connecting", "relay"]` e `T-WEB-DIRECT-FALLBACK`
  leggeva un solo commit invece di due. Ora il killer aspetta che il BADGE nomini il
  diretto, cioe' lo stesso fatto osservato dopo che e' diventato tale. Corretto in
  entrambi i file, perche' era lo stesso killer copiato.
- **Una gamba diretta che non si alza e' un fallimento?** No: e' la preferenza del prodotto
  (§8.55), e sotto il carico della suite completa un peer atterra legittimamente sul relay.
  `T-WEB-PATH-UI` prova ora fino a due volte e usa `killedAt` per sapere se lo scenario e'
  avvenuto davvero; le asserzioni sempre vere (il badge non apre mai su un trasporto non
  verificato) valgono su ogni tentativo, e due gambe dirette mancate di fila restano un
  fallimento vero.

---

### 5.6 Interfaccia: layout ordinato, path direct/relay evidente, drag & drop completo (aggiunta in esecuzione, 2026-09-15)

> Sub-fase aggiunta su richiesta esplicita dell'utente (D23): *"cura poi la parte di frontend
> a cui accede l'utente. Un'interfaccia pulita e funzionale, con chiara evidenza anche se la
> connessione è diretta o relay, ordinata, intuitiva e funzionale. È gradita anche la
> funzionalità di trasferimento dei file tramite drag and drop."* Eseguita DOPO 5.3 (esistono
> cartelle e ZIP da mostrare) e PRIMA di 5.4, così lo scenario finale valida la UI definitiva.
> Stato di partenza: il drop di FILE esiste già dalla fase 2 (`dropzone`, `onSelectFiles(…, "drop")`),
> senza però evidenza visiva di drag, senza cartelle e senza copertura e2e.

- **Model:** `agent-1:Claude-Opus-5`
- **Assignment:** portare la UI da funzionante a curata senza introdurre alcun automatismo di trasferimento; self-review su accessibilità, stati vuoti e su ogni punto in cui la UI potrebbe DICHIARARE qualcosa che non ha verificato.
- **Files:** `web/transfer/src/view.js:layout, badge di path, dropzone`; `web/transfer/src/state.js:derivazioni di stato e testi`; `web/transfer/src/styles.css:sistema visivo`; `web/transfer/src/main.js:wiring drop e annunci`; `web/transfer/tests/unit/state.test.mjs:derivazioni`; `web/transfer/tests/e2e/ui.spec.mjs:new`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:**
  1. **Badge di path per trasferimento**: `connecting` → `diretto` o `relay`, con testo E forma distinguibili (mai il solo colore), tooltip che spiega la differenza in una riga. Il badge NON può anticipare la verità: resta `connecting` finché il destinatario non ha verificato il primo chunk (contratto Fase 4), e un fallback lo riporta a `connecting` prima di mostrare `relay`.
  2. **Layout ordinato**: tre zone stabili — *la mia room* (peer, nome, link), *offerte* (proprie e altrui, raggruppate per peer), *trasferimenti* (righe con progresso, velocità, path, azioni). Nessun elemento cambia posizione durante un trasferimento; il rendering resta incrementale (§8.66: un rebuild sotto evento stacca i pulsanti).
  3. **Drag & drop completo**: `dragenter`/`dragleave`/`dragover` producono uno stato visivo esplicito; il drop accetta file E cartelle via `webkitGetAsEntry`/`getAsFileSystemHandle` quando disponibile, ricadendo sui soli file altrimenti; un drop mentre la room è bloccata è rifiutato con un messaggio, mai silenziosamente ignorato. Il drop pubblica un'offerta: non trasferisce nulla da solo (D3).
  4. **Stati vuoti, errori e attese** hanno testo proprio in italiano stabile; ogni azione distruttiva (ritira, annulla, scarta) dice cosa perde.
  5. **Accessibilità**: focus visibile, ordine di tabulazione uguale all'ordine visivo, `aria-live` per gli annunci già esistenti, progresso come `role="progressbar"` con valori, contrasto AA, nessuna dipendenza dal solo hover.
- **Unit tests:** `path_badge_stays_connecting_until_a_chunk_is_verified`; `fallback_resets_the_badge_before_it_says_relay`; `drop_of_files_publishes_one_offer_and_no_transfer`; `drop_while_locked_is_refused_with_a_message`; `empty_states_have_their_own_text`; `progressbar_exposes_value_min_max`.
- **e2e tests:** `T-WEB-DND` — drop reale di uno e più file (e di una cartella dove il motore lo consente) su tutti e tre i motori: l'offerta compare, nessun `transfer.request` parte; `T-WEB-PATH-UI` — un trasferimento relay mostra `relay` e uno diretto mostra `diretto`, entrambi solo dopo il primo chunk verificato, e un fallback direct→relay mostra la transizione; `T-WEB-UI` — layout stabile durante un trasferimento (nessun elemento si sposta, il pulsante Annulla resta cliccabile), tab order e focus visibile.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + i tre gate passano sui tre motori + self-review conferma che nessun badge dichiara un path non verificato e che nessun gesto di drop avvia un trasferimento + closed in `STATE.md` (§1 → 5.5, §4 ledger row, §6 `none`, §11 board). NOTA (2026-09-16): 5.6 ha girato DOPO 5.4 e non prima; 5.5 (README) è stata spostata dopo 5.6 di proposito, perché documenti la UI definitiva. Vedi `STATE.md` §8.60.

**Esito 5.6 (2026-09-16, `agent-1:Claude-Opus-5`).** La UI è passata da funzionante a
curata e i tre gate girano sui tre motori (`ui.spec.mjs`, 9/9). Quattro cose da registrare,
tre delle quali sono difetti veri trovati proprio perché il gate guarda l'interfaccia e non
il protocollo:

1. **`onSelectionError` non era collegato.** `view.js` chiamava
   `callbacks.onSelectionError?.(…)` da anni e `main.js` non forniva quella callback: ogni
   fallimento di selezione — una cartella rifiutata, un drop non supportato, una room
   bloccata — finiva nel nulla, con l'utente davanti a un'interfaccia che non diceva niente.
   Il `?.` che rende il codice robusto è esattamente ciò che ha reso il difetto invisibile.
2. **L'elemento degli annunci era `visually-hidden` sempre.** `announce()` scriveva in un
   nodo `aria-live` che nessun utente vedente poteva leggere: ogni messaggio del prodotto
   esisteva solo per uno screen reader. Ora il nodo prende `.app-toast-live` quando ha un
   testo e torna nascosto quando è vuoto.
3. **Il badge continuava a nominare un trasporto morto.** Dopo un fallback a metà
   trasferimento la riga restava `direct` fino al primo chunk verificato sul relay: una
   dichiarazione su un canale che non esisteva più. `state.js` ha ora l'unico evento che
   riporta indietro una riga (`transfer.path_reset`, locale per costruzione — nessun
   messaggio remoto vi si mappa) e `main.js` lo emette su `abandonDirect` e su entrambi i
   rami di `failDirect`.
4. **Un difetto Chromium-specifico nel drop.** Un `DataTransferItem` sintetico in Chromium
   ESPONE `getAsFileSystemHandle` e la promessa risolve a `undefined`, non a `null`: il
   filtro passava, `handle.kind` era `undefined` e `entriesFromDataTransfer` sollevava
   «Voce di cartella non supportata». `folders.js` scarta ora anche `undefined` e, se nessun
   handle si risolve, ricade sui `files` del `DataTransfer` — pubblicare ciò che è stato
   lasciato cadere è l'unico esito accettabile per un drop.

Tre correzioni al gate stesso, ognuna perché misurava la cosa sbagliata:

- **Handle di locator stantii.** Il ritiro delle offerte raccoglieva tutti i pulsanti e poi
  li cliccava: il primo click ridisegna il catalogo, il secondo handle è staccato e
  `click()` — che non ha timeout di default — aspettava per sempre. Il ciclo ri-risolve a
  ogni passata. È il gemello UI di §8.66: un rendering incrementale non salva un test che
  tiene in mano nodi vecchi.
- **«Stessa riga» è una SOVRAPPOSIZIONE verticale, non un `top` uguale.** In una riga
  centrata il controllo più alto ha il `top` più piccolo (input 206 px, pulsante 202 px), e
  confrontare i soli `top` leggeva quel layout corretto come un salto all'indietro.
- **La stabilità del layout si misura in coordinate del DOCUMENTO.** `getBoundingClientRect`
  è relativo al viewport, e lo scroll che Playwright stesso fa prima di un click spostava
  ogni riquadro di 388 px. I riquadri ora portano `scrollY`/`scrollX`; che la pagina non
  scorra da sola durante il trasferimento è diventata un'asserzione separata, che è poi la
  promessa vera.

Due note oneste sul resto:

- **WebKit riporta come errore di pagina la scrittura su un canale appena ucciso**
  («Error sending binary data through RTCDataChannel.»). Non è catturabile da JS: al momento
  della `send` il canale legge ancora `open` e il trasporto è già sparito, quindi né la
  guardia del sink né il suo `try/catch` vedono nulla. Il gate filtra quella sola riga e
  conta tutto il resto.
- **`T-WEB-PATH-UI` non aspetta più la fine del file.** Una volta si è bloccato 300 s sotto
  il carico dell'intera suite sui tre motori (stessa classe di §8.55/§8.59) aspettando
  `#save-file` per una coda di 6 MiB che non fa parte della sua tesi: il badge arriva a
  `relay` molto prima. Il completamento dopo un canale ucciso a metà resta coperto da
  `T-WEB-DIRECT-FALLBACK`, che è il gate che lo possiede. Il trasferimento viene annullato
  esplicitamente, non abbandonato.

---

## Phase gates

- **Build:** `cargo build --all-features`
- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** `cargo test --all-features --lib && cargo test --all-features --test web_transfer_test -- --test-threads=1 && npm ci --prefix web/transfer && npm run check --prefix web/transfer && npm run test:e2e --prefix web/transfer`
- **Asset drift:** `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist`
- **Regression guard:** `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ && cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`; T-WEB-MULTIPEER-FINAL, T-WEB-ZIP, T-WEB-ZIP-RESUME, T-WEB-NOAUTO, T-WEB-DND, T-WEB-PATH-UI, T-WEB-UI e tutti i security/lifecycle gate precedenti passano.
- **README:** aggiornato al comportamento finale cartelle/ZIP/multipeer, con scenario esplicito e limiti browser.

## Phase done criterion

Qualunque browser peer può pubblicare file/cartelle, scaricare manualmente un file o l'intera offerta come ZIP, annullare i transfer di cui è parte e riprenderli. Direct resta preferito, relay funziona quando ICE fallisce, e nessun destinatario diventa seed automaticamente. README.md descrive senza ambiguità lo scenario finale e `STATE.md` §11 mostra Phase 5 `DONE` con ogni subfase chiusa.
