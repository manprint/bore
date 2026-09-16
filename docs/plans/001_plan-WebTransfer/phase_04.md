# Phase 3 — Relay cifrato e prima vertical slice utilizzabile

> **Intent:** consegnare `bore transfer web` con download manuale di singoli file, cifratura end-to-end, relay a memoria limitata, cancellazione e ripresa OPFS.
> **Shippable alone?** yes — funziona end-to-end via relay; il percorso diretto viene aggiunto nella fase successiva e la limitazione è documentata esplicitamente.
> **Preconditions:** Phase 2 DONE

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`, finish or revert that unit first (§6 says how far it got). Run the gate commands in STATE.md **§3** and check the result against what §1, §7, and §11 claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type: sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to `claimed — nothing written yet`. Write or update the listed tests first or alongside production edits; do not defer them to a later unit.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1 at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP commits are on.

---

## Fixed contracts for this phase

- La slice supporta download grezzo quando la selezione contiene esattamente un file. Offerte multiple/cartelle sono visibili ma il relativo pulsante download resta disabilitato fino alla Phase 5.
- Un annuncio implica consenso della fonte a servire il file: `transfer.incoming` viene accettato automaticamente se il `File` locale e il suo manifest sono ancora validi e i limiti locali lo consentono. Non compare un secondo prompt della fonte.
- Il destinatario è l'unico attore che può iniziare o riprendere. Pubblicazione, join, reconnect e heartbeat non inviano mai `transfer.request`.
- Stati server prescritti: `Requested`, `WaitingSource`, `WaitingRelay {attempt}`, `Active {attempt,Relay}`, quindi uno solo fra `Completed`, `Cancelled`, `Failed`. Ogni transizione verifica room, partecipanti, offerta e attempt corrente sotto un'unica breve lock.
- `attempt_number` parte da 1 e cresce con checked add. Ogni tentativo riceve `AttemptId` casuale nuovo; non riutilizzare attempt ID, chiave AES o sequence.
- Il server non riceve mai room key, chiave derivata, nonce, plaintext, digest chunk in chiaro o contenuto ZIP. Digest e metadati necessari alla ripresa restano tra browser dentro payload cifrato o nel tab/OPFS.
- Il relay usa un solo task di copia per coppia: legge un messaggio sorgente, applica rate/backpressure, lo scrive al destinatario con deadline 10 secondi e poi legge il successivo. Non crea canali payload intermedi.
- Ogni cancellazione è terminale e idempotente sul server. Il browser usa un AbortController per fermare letture, hashing, cifratura e socket.

## Sub-phases

### 3.1 Implementare la macchina di stato del trasferimento e i ticket relay

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** implementare stati, autorizzazione, tentativi, ticket e cleanup esattamente come descritti; fare self-review con una tabella di tutte le transizioni e dei permit rilasciati.
- **Files:** `src/web_transfer.rs:new — TransferRecord, TransferState, AttemptState, RelayTicketRecord, transition methods`; `src/web_transfer_protocol.rs:new — transfer control bodies`; `tests/web_transfer_test.rs:new — transfer state group`.
- **Change:** `transfer.request` contiene OfferId, sorted unique selected entry IDs, `selectionDigest=SHA256(canonical {offerId,manifestMac,entryIds,mode})`, mode `raw`, e resume descriptor opzionale `{verifiedRanges:[[start,endExclusive]...], outputLength}`. In questa fase accettare soltanto un entry ID che identifica un file e mode raw. Range ordinati, non sovrapposti, entro chunkCount, massimo 4096; oltre il limite il browser deve ridurre allo stabile prefisso contiguo e ritrasmettere il resto. Il server verifica richiesta, fonte online, manifest invariato, cap trasferimenti per entrambi e idempotency requestId; alloca TransferId, riserva un permit attivo per fonte e destinatario, inserisce `Requested`, poi invia `transfer.incoming` alla fonte senza bloccare la lock. La fonte risponde `source_ready` o `reject`; ready deve riportare lo stesso selectionDigest e porta a `WaitingRelay`. Acquisire il semaforo globale relay con timeout 30 secondi; se pieno inviare `RELAY_BUSY`, mantenere il trasferimento in uno stato riprendibile senza permit relay e richiedere un nuovo click/requestId per riprovare. Quando ammesso, creare due ticket casuali distinti da 128 bit, ruolo-bound, one-use, scadenza 30 secondi; conservare soltanto SHA-256, TransferId, AttemptId, PeerId e ruolo. Inviare a ciascun peer soltanto il proprio ticket in `transfer.relay_ticket`. Il ticket scaduto o usato non torna valido. Solo fonte/destinatario possono `transfer.cancel`; terzi ricevono `NOT_PARTICIPANT`. L'actor risponde sempre l'ack prima di drenare gli eventi prodotti dalla stessa richiesta (stessa coda, FIFO: ack-before-event sullo stesso peer; le notifiche cross-peer restano sincrone). Withdraw offerta e PeerGuard cancellano tutti i trasferimenti interessati. Room destroy cancella tutto. `transfer.complete` è accettato soltanto dal destinatario per attempt corrente dopo verifica locale e porta a Completed. Le risposte duplicate di request/cancel/complete sono terminali idempotenti. Conservare record terminali al massimo 5 minuti in FIFO per idempotency, poi liberare metadata; i permit attivi/relay vengono rilasciati al momento della transizione terminale, non alla GC. Eventi e log contengono ID opachi, ruoli, path, byte aggregati e codice, mai file/path/token.
- **Unit tests:** `request_requires_recipient_click_source_online_and_single_file`; `duplicate_request_id_returns_same_transfer`; `source_ready_must_match_source_and_selection_digest`; `active_caps_reserve_both_peers_and_roll_back`; `relay_busy_is_bounded_and_retryable`; `tickets_are_distinct_role_bound_hashed_one_use_and_expire`; `attempt_ids_never_repeat_and_increment_checked`; `only_participants_cancel`; `withdraw_disconnect_and_room_close_cancel_related_transfers`; `terminal_transitions_release_permits_exactly_once`; `stale_attempt_messages_do_not_change_current_state`; `terminal_cache_is_bounded_and_idempotent`.
- **e2e tests:** `T-WEB-TRANSFER-STATE` — A pubblica, B richiede esplicitamente, A auto-ready, entrambi ricevono ticket diversi, un terzo peer non può usarli/cancellare, timeout/duplicate/cancel lasciano contatori e stato coerenti.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-TRANSFER-STATE` passa + test concorrenti request/disconnect/withdraw/close ritornano a zero permit + self-review documenta una sola uscita terminale per ogni interleaving + closed in `STATE.md` (§1 → 3.2, §4 ledger row, §6 `none`, §11 board).

### 3.2 Implementare il relay WebSocket opaco e sottoposto a backpressure

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** implementare l'attach e il pump relay senza storage né code payload, poi self-review heap, fd, timeout e ogni ramo di chiusura.
- **Files:** `src/web_transfer_http.rs:new — relay route/upgrade/attach`; `src/web_transfer.rs:new — RelayPair and run_relay_pair`; `src/web_transfer_protocol.rs:new — RelayAttach`; `tests/web_transfer_test.rs:new — relay transport group`; `tests/support/web_transfer.rs:new — relay pair helpers`.
- **Change:** sulla route relay richiedere stesso Host, Origin e sottoprotocollo controllo, ma WebSocketConfig separata: read/write buffer 4 KiB, max write 256 KiB, max frame e message 32 KiB. Il primo e unico messaggio text ammesso entro 10 secondi è `relay.attach {v,peerId,transferId,attemptId,role,ticket}`; validare formato prima dell'hash, consumare atomicamente il ticket corrispondente e scartare il raw. Prima del pairing accettare soltanto ping/pong/close; qualunque binary prematuro chiude. Le due connessioni devono arrivare entro la scadenza ticket/30 secondi, altrimenti chiudere entrambe e riportare `DIRECT_FAILED`/retryable relay attach failure sul controllo. Dopo entrambi gli attach, inviare `transfer.path_commit {path:"relay",attemptId}` sui control socket e attendere ack `source_ready` per quel commit prima di leggere payload. Nel pump, la fonte può inviare solo binary 17..32768 byte e ping/close; il destinatario non può inviare binary. Controllare magic/header e sequence monotona senza decifrare il body; non ispezionare tipo cifrato. Applicare token bucket per room con rate configurato, burst esatto 2×rate limitato a 200 MiB, oppure nessun delay se rate=0. Usare `select!` tra cancellation token, source read, recipient protocol violation e 10-second send timeout. Inviare il `Message` direttamente al sink destinatario prima di leggere il successivo: nessun `mpsc`, `Vec` cumulativo, temp file o clone del payload. Disabilitare estensioni compression WebSocket. Alla fine notificare il transfer actor, chiudere entrambe le socket e rilasciare il permit relay una volta. Il server può contare ciphertext bytes/frame e durata, non contenuto. R5 — i frame binari e le close semantics seguono RFC 6455 ([RFC 6455](https://www.rfc-editor.org/rfc/rfc6455.html)).
- **Unit tests:** `relay_attach_requires_first_text_message_and_exact_identity`; `relay_ticket_is_consumed_atomically`; `relay_rejects_oversize_text_binary_compression_and_recipient_binary`; `relay_checks_magic_and_sequence_without_decrypting`; `relay_holds_at_most_one_application_frame`; `blocked_recipient_times_out_and_releases_permit`; `room_token_bucket_rate_and_burst_are_exact`; `zero_rate_disables_throttling`; `relay_cancel_closes_both_sides`; `relay_logs_only_opaque_aggregates`.
- **e2e tests:** `T-WEB-RELAY-OPAQUE` — due client WebSocket trasferiscono 64 MiB di ciphertext con hash finale esatto, rate osservato entro tolleranza 10%, cancellazione immediata, tentativi di attach/replay/oversize respinti e nessuna crescita lineare dell'RSS.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-RELAY-OPAQUE` passa + RSS delta resta entro 8 MiB più buffer TLS/WebSocket misurati e non cresce con la dimensione payload + self-review trova zero scritture filesystem nel modulo relay e zero queue payload + closed in `STATE.md` (§1 → 3.3, §4 ledger row, §6 `none`, §11 board).

### 3.3 Implementare cifratura, framing e sender browser per file singolo

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** collegare i codec fissati al File reader e al relay con controllo backpressure; self-review nonce uniqueness, source-change detection e buffer lifetime.
- **Files:** `web/transfer/src/crypto.js:attempt helpers — per-attempt derivation and AES-GCM`; `web/transfer/src/framing.js:new — encrypted frame stream`; `web/transfer/src/sender.js:new — source transfer actor`; `web/transfer/src/control.js:transfer dispatch — incoming/ready/cancel messages`; `web/transfer/tests/unit/framing.test.mjs:new`; `web/transfer/tests/unit/sender.test.mjs:new`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** quando arriva `transfer.incoming`, cercare l'OfferId/entry nel Map locale, verificare File.size e lastModified contro manifest e rifiutare `OFFER_NOT_FOUND`/`SOURCE_CHANGED` se assente o mutato. L'annuncio valido implica accettazione automatica, soggetta al limite locale di 8 transfer concorrenti. Derivare la chiave per ogni attempt esattamente dalla fixture Phase 0, mantenendola non-estraibile in WebCrypto e abbandonando i riferimenti al termine. Dopo `path_commit relay`, aprire/attivare il sender. Leggere slice logiche da 1 MiB in ordine, compresi i chunk saltati dal resume: ricalcolare SHA-256 di ogni chunk e confrontarlo con i digest conservati alla pubblicazione prima di inviare/scartare. Su divergenza inviare `SOURCE_CHANGED`, abortire e ritirare l'offerta. Per chunk necessario, dividerlo in frammenti plaintext <=24 KiB, numerare frame sequence da zero, costruire header 16 byte, cifrare body con AES-256-GCM/header AAD e inviare ogni ciphertext in un singolo messaggio WebSocket <32 KiB. Seguire con `CHUNK_DIGEST`, `ENTRY_DONE` e `TRANSFER_DONE`; il root finale deve coincidere col manifest. Non riusare sequence o ritentare plaintext diverso: qualunque errore termina l'attempt; il retry riceve nuova key/attempt. Applicare backpressure del browser: prima di leggere la slice successiva, se `WebSocket.bufferedAmount > 4 MiB`, attendere polling/evento fino a <1 MiB con cancellazione; non tenere più di un chunk plaintext e un frame ciphertext. Emettere progress control al massimo ogni 500 ms o ogni MiB, scegliendo l'evento meno frequente, con soli conteggi. AbortController condiviso interrompe File reads, crypto promises ignorandone l'esito tardivo, backpressure waits e WebSocket.
- **Unit tests:** `attempt_key_nonce_and_ciphertext_match_fixture`; `sender_waits_for_path_commit_before_first_file_read`; `sender_auto_accepts_only_valid_local_offer`; `resume_ranges_are_rehashed_but_not_sent`; `source_change_aborts_and_withdraws`; `one_mib_chunk_fragments_stay_under_all_limits`; `sequence_is_monotonic_and_new_attempt_resets_with_new_key`; `sender_high_low_water_blocks_future_file_reads`; `abort_stops_reads_crypto_and_socket`; `sender_peak_live_buffers_are_one_chunk_plus_one_frame`.
- **e2e tests:** `T-WEB-SENDER-RELAY` — dopo il click di B, A auto-accetta e invia un file multi-chunk; prima del click e prima del path_commit non legge payload; alterazione File tra publish/send produce SOURCE_CHANGED senza byte utile.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-SENDER-RELAY` e fixture E2EE passano su tutti i motori + instrumentation prova zero read prima di commit e buffer limitati + self-review verifica ogni incremento sequence e ogni creazione attempt + closed in `STATE.md` (§1 → 3.4, §4 ledger row, §6 `none`, §11 board).

### 3.4 Implementare sink OPFS, verifica, resume e salvataggio esplicito

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** implementare la ricezione transazionale e persistente senza conservare segreti, poi self-review crash consistency, quota e verifica completa.
- **Files:** `web/transfer/src/storage.js:new — OPFS and IndexedDB repository`; `web/transfer/src/receiver.js:new — decrypt/verify/write actor`; `web/transfer/src/framing.js:decodeFrame — strict decoder`; `web/transfer/src/view.js:download controls — save/discard UI`; `web/transfer/tests/unit/storage.test.mjs:new`; `web/transfer/tests/unit/receiver.test.mjs:new`; `web/transfer/tests/e2e/download.spec.mjs:new`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** prima di mostrare pulsante attivo verificare supporto `navigator.storage.getDirectory`, `crypto.subtle` e IndexedDB; se OPFS manca lasciare join/catalog/upload disponibili e mostrare download non supportato. Al click, calcolare spazio richiesto con checked BigInt e richiedere `storage.estimate().quota-usage >= size + min(64 MiB, 5% size)`; altrimenti errore `STORAGE_QUOTA` prima di `transfer.request`. Aprire DB `bore-transfer-v1`, schema version 1, store `partials`; chiave `(roomId,sourcePeerId,offerId,selectionDigest)`. Record: manifestMac, output kind/path, expected length/root, RLE ranges verificati, digest SHA-256 per chunk completato, updatedAt; mai member token, room key, attempt key o URL completo. Per file grezzo usare OPFS `bore-transfer-v1/<room>/<offer>/<selectionDigest>/<entryId>.part`, creando solo segmenti interni generati, mai path manifest come path filesystem. Al resume riaprire e ricalcolare digest di ogni chunk segnato; eliminare range corrotti/troncati e inviare massimo 4096 range, riducendo al prefisso contiguo se necessario. Ricevere frame, verificare magic/sequence prima di AES-GCM, decrypt con header AAD, validare offset/entry/chunk/fragment e scrivere soltanto chunk completo dopo digest match; commit di bytes+digest+RLE in ordine crash-safe (flush OPFS, poi transazione IDB). Il final root e lunghezza devono coincidere col manifest prima di `transfer.complete`. Su cancel mantenere partial; su source change/withdraw/room close eliminare partial e record. Dopo successo ottenere OPFS File, creare Object URL e mostrare **Salva file verificato** con nome manifest sanitizzato soltanto per attributo download; revocare URL e pulire staging dopo click salvato confermato dall'app o **Scarta**, non automaticamente alla verifica. `showSaveFilePicker` può apparire solo come enhancement rilevato a runtime e sempre dentro user activation; il fallback anchor è obbligatorio. R8/R9 — OPFS è disponibile in secure context e via `navigator.storage.getDirectory()` ([standard](https://fs.spec.whatwg.org/), [MDN](https://developer.mozilla.org/en-US/docs/Web/API/StorageManager/getDirectory)); R10 — `showSaveFilePicker` non è universalmente disponibile e richiede attivazione ([MDN](https://developer.mozilla.org/en-US/docs/Web/API/Window/showSaveFilePicker)).
- **Unit tests:** `storage_schema_contains_no_secret_fields`; `quota_is_checked_with_bigint_and_headroom`; `generated_opfs_paths_ignore_manifest_paths`; `resume_rehashes_and_drops_corrupt_or_truncated_chunks`; `resume_ranges_are_sorted_bounded_and_coalesced_to_prefix`; `receiver_rejects_gap_duplicate_old_attempt_wrong_tag_digest_offset_and_root`; `chunk_is_flushed_before_idb_commit`; `cancel_retains_partial_but_withdraw_room_close_purge`; `verified_file_requires_explicit_save_or_discard_to_purge`; `object_urls_are_revoked`; `no_opfs_disables_only_downloads`.
- **e2e tests:** `T-WEB-DOWNLOAD-RELAY` — B clicca, riceve e verifica bytes esatti via relay, poi salva esplicitamente; quota insufficiente non crea richiesta; browser senza OPFS continua a pubblicare ma non scarica.
- **Deviation (applied by `execute verify V002`, see `STATE.md` §8.58–60):** lo staging non usa un unico `<entryId>.part` riaperto con `keepExistingData` — quella forma copia l'intero output a ogni chunk (quadratica: 128 MiB a 8.43 MiB/s contro 22.81 a 32 MiB). Ogni chunk verificato è un file a sé, `<entryId>.<chunkIndex>.part`, e il file finito è un `Blob` composto dalle parti in ordine (`repository.stagedBlob`), senza pass di assemblaggio né copia su disco; 128 MiB scende a 2580 ms (49.61 MiB/s). Il salvataggio scrive quel `Blob`: la pagina non può fare `fetch()` del proprio object URL (CSP `connect-src 'self'`). L'indice del chunk in arrivo viene dal piano di invio, non dall'ordine di arrivo, perché un resume salta i range già verificati. Le fasi successive (3.5 resume, 5.x ZIP) costruiscono su `stagedBlob` e sul cursore di piano.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-DOWNLOAD-RELAY` passa su Chromium/Firefox/WebKit con adapter OPFS reale o fixture esplicitamente separata dove WebKit headless manca supporto + scansione IndexedDB/OPFS prova assenza segreti + self-review simula crash tra flush e commit + closed in `STATE.md` (§1 → 3.5, §4 ledger row, §6 `none`, §11 board).

### 3.5 Integrare UX download, cancellazione e reconnessione senza auto-resume

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** collegare sender/receiver/control alla UI con ownership e azioni esplicite; self-review ogni evento che può iniziare I/O.
- **Files:** `web/transfer/src/main.js:TransferController wiring — transfer orchestration`; `web/transfer/src/control.js:transfer reducer — reconnect state`; `web/transfer/src/view.js:transfer rows — buttons, path/progress/cancel/save`; `web/transfer/src/styles.css:transfer states — accessible progress`; `web/transfer/tests/unit/state.test.mjs:transfer cases`; `web/transfer/tests/e2e/download.spec.mjs:cancel/resume cases`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** per ogni offerta altrui con un solo file mostrare **Scarica**; se esiste partial valido mostrare **Riprendi** ma entrambe chiamano `transfer.request` soltanto nel click handler. Non mostrare download sulla propria offerta. La riga transfer presenta fonte, destinatario, dimensione, percentuale verificata, stato, path `relay`, velocità locale e pulsante **Annulla** soltanto ai due partecipanti. Non visualizzare filename in eventi/global logs; nella UI locale è ammesso. `transfer.incoming` alla fonte avvia l'auto-ready senza prompt solo per un'offerta locale valida. Cancel utente chiama prima AbortController, poi invia cancel idempotente; cancel remoto abortisce allo stesso modo. Disabilitare pulsanti duplicati mentre requestId è pendente. Se control WS cade, PeerGuard server cancella trasferimenti e offerte; il tab riconnesso ripubblica offerte locali, ma mostra i partial come “Disponibile per ripresa” e non invia request fino a nuovo click. Room closed purga partial, secret session e tutte le righe attive. Non usare unload per promettere affidabilità; best-effort cancel è ammesso ma cleanup server è autoritativo. Toast e aria-live annunciano offerta, disponibilità, completamento/errore; nessuna Notification API. Mappare ogni codice protocollo a testo italiano stabile senza includere dati server grezzi.
- **Unit tests:** `only_download_or_resume_click_dispatches_request`; `source_incoming_auto_ready_requires_local_file`; `only_participants_see_cancel`; `cancel_aborts_before_control_send`; `reconnect_marks_partial_resume_available_without_request`; `room_close_purges_and_disables_all`; `error_codes_map_to_safe_user_text`; `own_offer_has_no_download_button`.
- **e2e tests:** `T-WEB-NOAUTO` — preserva zero transfer/RTC/relay prima del click anche con partial presente; `T-WEB-CANCEL-RESUME` — cancella al 25%, byte cessano, chunk OPFS restano, reconnect non riparte, nuovo click salta i chunk verificati e produce file esatto; `T-WEB-CANCEL-AUTH` — fonte/destinatario annullano, terzo peer non può.
- **Deviation (3.5, vedi `STATE.md` §8.63–65):** il descrittore di resume viaggia su `transfer.request` e la sorgente non lo vede mai, quindi `transfer.path_commit` porta un campo additivo `resumeRanges` (presente solo se il destinatario ha un parziale) — è l'unico modo perché la sorgente salti i chunk già verificati. Di conseguenza `FINAL` conta il plaintext di QUESTO tentativo, non l'intera entry. `outputLength` viaggia come NUMERO (il server lo legge con `as_u64`: la stringa decimale del manifest fa rifiutare tutta la richiesta con `INVALID_MESSAGE`). La view non ricostruisce più il DOM a ogni evento di progresso: le righe transfer si aggiornano in place e catalogo/peer si ridisegnano solo quando cambiano, altrimenti il pulsante **Annulla** viene staccato sotto il cursore (misurato: il click non arrivava mai su firefox e webkit).
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + tutti e tre i gate passano sui motori default + instrumentation dimostra che soltanto i due click handler inviano request + self-review elenca ogni chiamata a `transfer.request`, `File.slice` post-publish e relay open + closed in `STATE.md` (§1 → 3.6, §4 ledger row, §6 `none`, §11 board).

### 3.6 Esporre `bore transfer web` e completare il lifecycle CLI

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiungere la superficie CLI esatta e collegarla all'owner loop esistente, poi self-review output, signal handling e compatibilità dei sottocomandi transfer.
- **Files:** `src/main.rs:557-560 — TransferCommand`; `src/main.rs:1910-2027 — transfer dispatch`; `src/web_transfer_cli.rs:run_web_transfer — public run path, output and OS signals`; `Cargo.toml:dependencies — webbrowser pin`; `Cargo.lock:package entries — resolved pin`; `src/main.rs:CLI tests — parsing/help snapshots`; `tests/web_transfer_test.rs:CLI lifecycle group — real process cases`.
- **Change:** aggiungere variante `Web` senza modificare `Listener`/`Sender`. Sintassi e soli flag: `bore transfer web`, `--to <HOST>` con lo stesso default pubblico e parsing endpoint già usato dal progetto (quindi il comando senza flag è valido), `--secret <SECRET>`, `--insecure`, `--open`. Non aggiungere selezione file, direct/relay, room-name o storage flag. Collegare a `OwnerClient`: dopo creazione riuscita scrivere su stdout esattamente due righe `room: <full-url>` e `room active; press Ctrl+C to close`, flushare e soltanto allora mantenere il lease. Se write/flush fallisce, chiudere room entro un secondo e uscire nonzero. `--open` chiama `webbrowser::open` dopo il flush; fallimento produce warning redatto su stderr ma non chiude la room. Installare Ctrl+C e SIGTERM su tutte le piattaforme disponibili e SIGHUP su Unix; il primo segnale invia close bounded 1 s, il secondo forza l'uscita. EOF della shell tramite SIGHUP segue close pulita. Caduta rete segue resume/backoff Phase 1 per massimo owner grace; non stampa nuovo URL e non crea room nuova. Se il server è vecchio/disabilitato, uscire con `web transfer requires an upgraded server configured with --web-transfer-base-url` senza dump del messaggio wire. Tracing va su stderr; nessun log contiene full URL. Preserve exit codes and help ordering of old transfer modes.
- **Unit tests:** `transfer_web_cli_accepts_only_documented_flags`; `transfer_listener_sender_cli_snapshots_unchanged`; `web_stdout_is_exactly_two_lines_and_flush_precedes_open`; `stdout_failure_closes_room`; `browser_open_failure_is_nonfatal_and_redacted`; `first_signal_closes_second_forces`; `old_server_error_is_actionable`; `cli_logs_never_contain_fragment_tokens`.
- **e2e tests:** `T-WEB-CLI` — spawn real `bore server` and `bore transfer web`, parse the exact URL, open browser flow, test `--open` through injectable opener, then Ctrl+C/SIGTERM/SIGHUP and assert immediate invalidation; force native control reset and assert same URL resumes within grace.
- **Deviation (3.6, vedi `STATE.md` §8.68–70):** l'apertura del browser esce da `run_owner_lease` (dove la Fase 1.4 l'aveva messa) e vive nel run path DOPO il flush, perché dentro il lease correva in parallelo alla stampa. `run()` in `main.rs` esclude `transfer web` dalla race generica con `shutdown_signal()`: quella race ritorna e basta, e lascerebbe la room viva per tutta la grace. `close_bounded` adesso ATTENDE che il server chiuda il controllo (stesso bound di 1 s): il frame di close viaggia su una substream yamux il cui driver è un task staccato, quindi un processo che esce appena `send` risolve può uscire con il close ancora in coda — misurato, la room sopravviveva a Ctrl+C.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-CLI` passes on supported signal platforms + old `bore transfer listener|sender` help and e2e remain green + self-review confirms no CLI path reads a local file or prints a secret outside the intended URL line + closed in `STATE.md` (§1 → 3.7, §4 ledger row, §6 `none`, §11 board).

### 3.7 Eseguire gate di sicurezza, no-storage e lifecycle della slice relay

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** costruire i test di accettazione avversariali della slice pubblica, correggere ogni difetto trovato entro questa subfase e self-review delle evidenze prodotte.
- **Files:** `tests/web_transfer_test.rs:acceptance groups — no-store, lifecycle, limits, legacy`; `web/transfer/tests/e2e/download.spec.mjs:acceptance cases`; `web/transfer/tests/e2e/security.spec.mjs:new`; `scripts/web_transfer_e2e.sh:new — local orchestration without sudo`; `.github/workflows/ci.yml:web-transfer job — add frontend checks only if this is the repository's active CI file`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** aggiungere un harness che esegue server con cwd e TMPDIR vuoti dedicati, cattura `/proc/<pid>/fd`, RSS e log, trasferisce 64 MiB contenenti un marker canary, annulla un secondo trasferimento e chiude la room. Prima/dopo confrontare directory e fd: nessun file regolare nuovo, memfd payload o descriptor cancellato; il solo executable/config/cert già esistente è escluso esplicitamente. RSS non cresce linearmente e resta entro baseline + 8 MiB + buffer TLS/WebSocket misurati per relay concorrente. Cercare marker in log e output server: assente. Intercettare control JSON e URL HTTP: `k` e room key assenti; il member token appare una volta soltanto nel primo hello WSS e mai in log. Testare wrong key, modified AAD, stale attempt, replay sequence, stolen ticket, ticket role swap, oversized frame, recipient binary, slow receiver, rate cap, peer/offer/transfer limits e chiusura room durante active relay. Per lifecycle reale: Ctrl+C/SIGTERM/SIGHUP invalidano subito pagina/WS/transfer; SIGKILL o firewall sul native owner mantiene 60 secondi, resume con token mantiene room, assenza resume la distrugge e abortisce payload. I test con grace reale possono usare clock Tokio controllato per unità, ma almeno un e2e usa `--web-transfer-owner-grace 5` e wall clock. CI esegue npm ci/check/build/drift, Playwright default e Rust test seriale web. Non aggiungere job branded browser obbligatori in questa fase.
- **Unit tests:** regression aggregate dei test `T-WEB-E2EE` e `T-WEB-LIMITS`; fuzz seeds per control/relay parser; `server_source_contains_no_payload_filesystem_api` come audit mirato solo ai moduli web transfer, senza fragile grep globale.
- **e2e tests:** `T-WEB-NOSTORE` — criterio 64 MiB/TMPDIR/fd/RSS/log; `T-WEB-E2EE` — vettori, wrong key/AAD, stale/replay e chiave mai server; `T-WEB-ROOM-LIFE` — clean close, abnormal grace/resume/expiry e active abort; `T-WEB-LIMITS` — ogni cap/rate/input malformato fallisce senza corrompere peer sani; `T-WEB-LEGACY` — suite sender/listener e wire precedente invariata.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + tutti i T-WEB nominati producono evidenze pass/fail ripetibili + nessun test è skipped sui prerequisiti disponibili + self-review verifica che gli strumenti misurino stato OS/server reale e non soltanto log dichiarativi + closed in `STATE.md` (§1 → 3.8, §4 ledger row, §6 `none`, §11 board).

- **Esito (2026-09-16):** i cinque gate T-WEB nominati esistono e girano.
  `t_web_nostore` muove 64 MiB di frame opachi fra un server con cwd e `TMPDIR`
  propri e vuoti, e confronta prima/dopo l'albero del sandbox, `/proc/<pid>/fd`
  e la RSS: zero file nuovi, nessun descriptor cancellato o `memfd:`, crescita
  RSS < 16 MiB, e il log non contiene né il canary, né l'etichetta, né la room
  key, né il member token. `t_web_room_life` usa processi reali e orologio da
  parete con `--web-transfer-owner-grace 5`: SIGKILL sull'owner tiene la room
  (e il relay continua a passare byte a +2 s), alla scadenza la room muore
  portandosi via il trasferimento attivo, mentre un SIGTERM la chiude subito.
  `t_web_legacy` prova la coesistenza su UN solo server con web transfer
  attivo: public tunnel in entrambe le direzioni, `bore transfer` nativo
  listener/sender su 512 KiB verificati, superficie web ancora viva.
  `t_web_limits` (3.7, già verde) e la suite fixture coprono E2EE e cap.
  `web/transfer/tests/e2e/security.spec.mjs` aggiunge la metà browser:
  la room key non lascia mai il tab e il member token esce una volta sola nel
  primo hello, un peer con la chiave sbagliata non vede l'offerta, un ticket
  speso e lo stesso ticket con il ruolo invertito vengono entrambi rifiutati
  senza un frame, e la perdita della room durante un download attivo fa
  fallire il download invece di consegnare un file parziale.
  `scripts/web_transfer_e2e.sh` orchestra tutto senza sudo (stage
  assets/build/rust/browser) e il job `web-transfer` in `.github/workflows/ci.yml`
  esegue npm ci/check/drift, il build, la suite Rust seriale e Playwright sui
  tre motori.
- **Difetti trovati e corretti in questa sub-fase:**
  1. **Il destinatario non verificava il MAC del manifest.** Il manifest
     arriva ATTRAVERSO il server, che non possiede la room key e quindi non
     può calcolare il tag: senza il controllo, un manifest forgiato sostituiva
     i propri root e ogni verifica per-chunk successiva riusciva contro la
     forgiatura — un download che riporta successo consegnando byte scelti
     dall'attaccante. Ora il tag è verificato all'ingresso nel catalogo
     (l'offerta non appare) e di nuovo in `startDownload` prima che parta
     qualsiasi messaggio (`MANIFEST_MAC`). Gate:
     `a_manifest_that_does_not_authenticate_is_refused_before_anything_leaves`
     più il caso e2e con la chiave sbagliata.
  2. **Una room morta lasciava la pagina in "Riconnessione…" per sempre.** Il
     server risponde all'hello di una room inesistente esattamente come a un
     token sbagliato (4001, l'esistenza non viene mai rivelata), e il client
     trattava ogni 4001 post-ack come sanabile. Ora la guarigione è limitata:
     il primo 4001 riprova (idle reap), il secondo consecutivo è terminale e
     la pagina dichiara la room non disponibile. Il timeout locale dell'hello
     usa un codice proprio (4002) così un server lento non viene mai contato
     come rifiuto. Gate: `a_room_that_stays_gone_stops_reconnecting_and_says_so`
     e `a_hello_timeout_is_not_a_refusal`.
  3. **Due difetti server trovati da `t_web_limits`** (già corretti in questa
     sub-fase): JSON malformato rispondeva `UNSUPPORTED_VERSION`, e le
     risposte non potevano uscire mentre un peer inondava il socket, quindi
     `RATE_LIMITED` era inosservabile e la sessione si auto-bloccava fino al
     timeout di invio (prima: pong 32, limited false; dopo: pong 60, limited true).

### 3.8 Update README.md

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** documentare la prima funzione pubblica relay-only con guida completa e fare self-review da utente nuovo.
- **Files:** `README.md:290 — command overview`; `README.md:881 — self-hosting`; `README.md:900 — complete server flags`; `README.md:996 — HTTPS`; `README.md:2035 — secure file transfer`; `README.md:3200 — troubleshooting`.
- **Change:** preservare la struttura, il tono e la lingua esistenti del README, modificandolo senza riscriverlo. Aggiungere `bore transfer web` alla panoramica; spiegare con il flusso esplicito che il comando crea/mantiene la room e la selezione avviene nel browser, che tutti i browser possono annunciare e scaricare, che “upload” significa annuncio e i byte restano nel tab, e che nessun download parte senza click. Documentare deploy server con `--web-transfer-base-url`, HTTPS pubblico o HTTP loopback, tutte le opzioni `--web-transfer-*`, default, env e validazioni; documentare i quattro flag client, output esatto, `--open`, segnali e grace 60 s. Fornire comandi binary e SSH-gateway/deploy coerenti con le sezioni esistenti, inclusi esempi `sudo` dove già richiesti dal README. Dichiarare esplicitamente lo stato di questa release: trasferimento di un singolo file via relay WebSocket cifrato end-to-end; cartelle/ZIP e direct WebRTC non sono ancora inclusi. Spiegare OPFS, quota, salvataggio esplicito, cancellazione/ripresa e limiti browser. Troubleshooting deve coprire server non configurato/vecchio, link incompleto dopo apertura in altro tab, OPFS/quota, relay busy, room scaduta. Non citare moduli, struct, fasi o dettagli di implementazione.
- **Unit tests:** docs command/help snapshot — ogni flag e default README coincide con `bore server --help` e `bore transfer web --help`; link checker interno se presente.
- **e2e tests:** `T-WEB-README-RELAY` — eseguire da build pulita i comandi server/client copiati dal README, aprire il link, pubblicare e scaricare un file via relay, annullare/riprendere e chiudere la room con l'output documentato.
- **Done:** un nuovo utente può installare, configurare e usare la slice relay-only dal README senza leggere il sorgente; limitazioni direct/cartelle sono evidenti; tutti i gate verdi; self-review README completa; closed in `STATE.md` con §1 → 4.1 e la §11 docs row per Phase 3 `DONE`.

- **Esito (2026-09-16):** la sezione README è riscritta senza toccarne tono, lingua o
  struttura: chi fa cosa (il CLI non legge un byte, "caricare" è solo annunciare, nessun
  download parte senza click), il deploy con tutte le opzioni `--web-transfer-*` e le
  validazioni che il server rifiuta all'avvio, i quattro flag client con l'output esatto e
  l'ordine di `--open`, il flusso in pagina (OPFS, quota, salvataggio esplicito,
  annulla/riprendi, ricarica della stessa scheda), i tre motori, le performance e una
  tabella di troubleshooting. Aggiunti: voce nell'indice, rimando dalla sezione
  self-hosting, il gruppo `--web-transfer-*` nel riferimento completo dei flag, la ricetta
  di deploy 12 (binario + Compose) e la riga nel troubleshooting globale. Lo stato della
  release è dichiarato in un blockquote: singolo file, relay WebSocket cifrato end to end;
  cartelle/ZIP e WebRTC direct non ci sono ancora.
  Tre difetti trovati scrivendo o eseguendo la documentazione, tutti corretti qui:
  1. **un'offerta multi-voce veniva rifiutata come `OFFER_NOT_FOUND`** — "Offerta non più
     disponibile" per qualcosa che è visibilmente in catalogo. Ora c'è `MULTI_ENTRY`
     ("Cartelle e selezioni multiple non ancora supportate"), che è ciò che il README
     promette; gate `a_multi_entry_offer_is_refused_as_unsupported_not_as_missing`.
  2. **`render` sovrascriveva il campo del nome a ogni evento di room** — un peer che
     entra mentre si digita svuotava la casella e faceva rispedire il nome vecchio.
     Era anche la causa del flake intermittente di `room.spec.mjs` su firefox. Ora il
     valore si scrive solo quando il campo non ha il fuoco; gate deterministico
     `a name being typed survives a room event` (red-checked: senza la correzione il
     campo legge `Peer 2a65`).
  3. **404 su `/favicon.ico`** su Chrome/Edge di sistema: un errore in console a ogni
     apertura. Risolto con `<link rel="icon" href="data:,">`, senza nuovi asset.
- **Gate 3.8:** `t_web_readme` (nuovo, Rust) confronta nome, env e default di ogni
  `--web-transfer-*` fra `bore server --help`, il riferimento flag del README e la tabella
  della sezione, più i flag di `bore transfer web --help` e le frasi che dichiarano lo
  scopo della release — red-checked cambiando un default nella tabella.
  `T-WEB-README-RELAY` (`web/transfer/tests/e2e/readme.spec.mjs`) esegue i comandi
  server/room copiati dal README, verifica le due righe di stdout documentate, poi
  pubblica → scarica → annulla → riprende → salva e chiude con SIGINT, pretendendo che
  ogni pagina dichiari la room finita invece di riconnettersi.

### 3.9 Misurare le performance della slice relay (aggiunta in esecuzione, 2026-09-15)

> Sub-fase aggiunta su richiesta esplicita dell'utente durante l'esecuzione di 3.6: *"bore
> si distingue per le performance elevate di trasferimento. Non perdiamo mai di vista questo
> aspetto."* Nessuna sub-fase esistente possiede la misura, e senza un harness la prima
> regressione di throughput si scopre in produzione. Eseguita dopo 3.6 e prima di 3.7 così
> che 3.8 possa citare numeri reali.

- **Model:** `agent-1:Claude-Opus-5`
- **Assignment:** costruire un harness di benchmark ripetibile per il percorso web transfer, pubblicare la prima baseline e rendere visibile una regressione; self-review del metodo di misura (non dei soli numeri).
- **Files:** `tests/web_transfer_bench.rs:new — bench Rust del pump relay (#[ignore], guidato dal driver)`; `web/transfer/tests/perf/{report,crypto-bench,throughput.perf}.mjs:new — helper condiviso, braccio crypto, braccio browser`; `web/transfer/tests/unit/perf.test.mjs:new`; `web/transfer/playwright.perf.config.mjs:new`; `web/transfer/package.json:scripts — test:perf`; `scripts/perf/web_transfer_bench.sh:new — driver, campioni grezzi + mediane`; `docs/transfer/WEB_TRANSFER_PERF.md:new — metodo, baseline, regole`; `README.md:Browser-to-browser transfer — paragrafo performance`.
- **Change:** tre bracci, ognuno a una variabile di distanza dal precedente. (1) **pipe**: N MiB attraverso il relay opaco del server senza crittografia applicativa — il tetto che il server può offrire. (2) **crypto**: gli stessi byte attraverso i moduli `crypto.js`/`framing.js` spediti — seal, open e digest per chunk — sotto Node e senza rete né storage: il tetto della CPU. (Il braccio era stato immaginato con la rete dentro; separarlo è meglio, perché il numero end-to-end è limitato dal minore fra i due tetti e due misure separate dicono QUALE dei due si è mosso.) (3) **browser**: la pagina reale (read → encrypt → relay → decrypt → verify → staging OPFS → save), il numero che l'utente sente. Ogni ripetizione esegue i bracci INTERLEAVED (mai tutto il braccio 1 e poi tutto il 2: la linea cambia sotto i piedi), stampa OGNI campione grezzo accanto alla mediana e forza `LC_ALL=C` (V-11: `sort -n` in locale a virgola decimale corrompe la mediana in silenzio). Un braccio che fallisce stampa `FAILED`, mai uno zero che entra in una mediana (V-9). I confronti pubblicati sono RAPPORTI misurati nella stessa ripetizione; i valori assoluti valgono solo per la macchina che li ha prodotti. Il throttle di room (`--web-transfer-relay-rate`) va azzerato nel bench del pipe, altrimenti si misura il bucket.
- **Unit tests:** `throughput_is_bytes_over_elapsed_and_refuses_zero_time` (Rust, puro); `bench_reports_every_sample_not_only_the_median` (Rust, sul formatter); `perf_harness_fails_loudly_when_an_arm_produces_no_bytes` (JS).
- **e2e tests:** `T-WEB-PERF` — il driver gira end-to-end su loopback, produce la tabella con i tre bracci su almeno due dimensioni e fallisce rumorosamente se un braccio non produce un numero o se il rapporto `e2ee-node / pipe` scende sotto la soglia registrata nel documento.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + `T-WEB-PERF` produce una tabella completa su loopback + `docs/transfer/WEB_TRANSFER_PERF.md` riporta metodo, baseline con campioni grezzi e la regola dei rapporti + README cita la performance senza numeri assoluti non qualificati + closed in `STATE.md` (§1 → 3.7, §4 ledger row, §6 `none`, §11 board con `T-WEB-PERF`).

### 3.10 Ridurre il divario misurato fra il percorso browser e i due tetti (aggiunta in esecuzione, 2026-09-15)

> Conseguenza diretta della baseline di 3.9, non una nuova richiesta: il relay
> trasporta ~470 MiB/s e la CPU del destinatario ne fa 435–605, ma il percorso browser
> end-to-end ne misura **~40**. Un fattore ~12 non è "il browser è lento": è un collo di
> bottiglia che nessuno aveva ancora guardato. D21 rende questo lavoro parte del prodotto.

- **Model:** `agent-1:Claude-Opus-5`
- **Assignment:** PROFILARE prima di toccare qualunque riga, poi adottare solo ciò che la misura giustifica, con il rapporto prima/dopo registrato in `docs/transfer/WEB_TRANSFER_PERF.md`; self-review sul fatto che nessuna ottimizzazione cambi il formato sul filo o indebolisca una verifica.
- **Files:** `web/transfer/src/sender.js:chunk read + seal loop`; `web/transfer/src/receiver.js:open + verify + stage loop`; `web/transfer/src/storage.js:part file lifecycle`; `web/transfer/src/framing.js:fragment iteration`; `web/transfer/tests/perf/throughput.perf.mjs:stage timings`; `docs/transfer/WEB_TRANSFER_PERF.md:before/after`; `web/transfer/dist/:generated — rebuilt assets`.
- **Change:** prima si misura DOVE va il tempo, aggiungendo al braccio browser una scomposizione per stadio (read, seal, send, receive, open, digest, stage, commit) presa con `performance.now()` dentro la pagina e stampata come righe `PERF stage=...`; nessuna modifica prima di quel numero. Poi, solo sugli stadi che la scomposizione indica, il catalogo — ognuna adottata con misura o scartata con motivo:
  1. **Pipeline invece di sequenza.** Oggi ogni frammento da 24 KiB attende il proprio `await` su WebCrypto su entrambi i lati: le operazioni sono indipendenti e possono essere emesse a gruppi (per chunk) mantenendo l'ORDINE di invio. Questo è il coalescing di V-14a tradotto: raggruppare ciò che è già pronto, mai aspettare che si formi un gruppo.
  2. **Lettura in anticipo di un chunk.** Il mittente legge `File.slice(...).arrayBuffer()` e solo dopo cifra; leggere il chunk successivo mentre cifra il corrente nasconde l'I/O dietro la CPU. Un chunk di anticipo, non una coda: la coda profonda è ciò che V-13 ha misurato come latenza pura.
  3. **Ciclo di vita del part file.** Oggi il destinatario crea, scrive e chiude un file OPFS per ogni chunk da 1 MiB. Misurare il costo di create/close e, se domina, tenere aperto un writable per part file e chiudere solo al confine di verifica — senza MAI tornare al file unico riscritto con `keepExistingData` (§8.59: è quadratico) e senza allargare la granularità del resume oltre il chunk verificato.
  4. **Allocazioni per frame.** Header e ciphertext in un solo buffer, decrypt in place dove la WebCrypto lo consente (V-14b tradotto). Il formato non si muove: il test di non-regressione è il vettore fixture byte per byte.
  5. **Vietato:** cambiare il formato del frame, ridurre o spostare la verifica per chunk, batchare più frame in un solo messaggio WebSocket (il relay inoltra messaggi opachi, un batch cambierebbe il filo), introdurre un timer che attenda compagnia.
- **Unit tests:** `pipelined_seal_preserves_frame_order_and_bytes`; `read_ahead_never_exceeds_one_chunk`; `part_file_lifecycle_keeps_chunk_level_resume`; `frame_bytes_match_the_fixture_after_the_allocation_change`.
- **e2e tests:** `T-WEB-PERF` — stesso harness, ora con la scomposizione per stadio; il gate è il RAPPORTO `browser / pipe` misurato nella stessa esecuzione, registrato prima e dopo; `T-WEB-DOWNLOAD-RELAY` e `T-WEB-CANCEL-RESUME` restano verdi byte per byte (una ottimizzazione che cambia un byte è un difetto).
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + la scomposizione per stadio è pubblicata + ogni voce adottata ha il suo prima/dopo con campioni grezzi in `docs/transfer/WEB_TRANSFER_PERF.md` + nessun test byte-esatto cambiato + closed in `STATE.md` (§1 → 3.7, §4 ledger row, §6 `none`, §11 board).

- **Esito (2026-09-16):** la scomposizione per stadio è in `web/transfer/src/perf.js`
  (inerte senza `window.__borePerf`) e i numeri stanno in
  `docs/transfer/WEB_TRANSFER_PERF.md`. Tre risultati, in ordine di importanza:
  (1) **la misura di 3.9 era quantizzata dall'harness**, non dal prodotto — il braccio
  browser si fermava a un poll di Playwright (griglia 100/250/500 ms) e a 32 MiB
  riportava ~39 MiB/s a ogni taglia; ora il tempo è preso dentro la pagina e il valore
  pollato resta pubblicato accanto come `browser-wall-<engine>`;
  (2) **i frame arrivano come `ArrayBuffer` e non come `Blob`** — un `await
  blob.arrayBuffer()` per messaggio valeva 366 ms su 630 (chromium, 32 MiB): **1.66x**;
  (3) **i frame si aprono a gruppi di 8 e si consumano in ordine stretto** — la sonda
  in pagina dice che webkit passa da 1008 a 2481 MiB/s e firefox da 921 a 1897 con otto
  `decrypt` in volo, chromium è piatto: end to end **webkit 39.60 -> 81.84 MiB/s
  (2.07x)**, firefox 41.13 -> 44.63, chromium 82.77 -> 85.86.
  Declinate con la loro misura: read-ahead e seal pipelined lato mittente (il mittente
  chiude 32 MiB in 222 ms contro i 373 del destinatario), frame in una sola allocazione
  (seal+open sotto il 12%), e l'apertura anticipata del part file (87.03 contro 86.32
  MiB/s su cinque ripetizioni: dentro il rumore, codice **rimosso**).
  Ciò che resta è misurato e dimensionato: `createWritable` + `write` + `close` è
  247 ms su 373 (chromium) e 449 su 717 (firefox) — lo risolve solo
  `createSyncAccessHandle`, che esiste esclusivamente in un worker, e quindi diventa
  la sotto-fase 3.11.

---

### 3.11 Staging OPFS in un worker (`createSyncAccessHandle`) — aggiunta in esecuzione, 2026-09-16

> Conseguenza diretta del profilo di 3.10, con il numero già in mano: dopo le due
> ottimizzazioni adottate il costo dominante del destinatario è il protocollo di
> scrittura OPFS, non la rete e non la cifratura.

- **Model:** `agent-1:Claude-Opus-5`
- **Assignment:** portare la scrittura dei part file in un worker dedicato che usa
  `createSyncAccessHandle`, con fallback automatico a `createWritable` sul thread
  principale quando il motore non lo espone; self-review sul fatto che l'ordine
  scrittura-poi-commit IDB e la granularità per chunk del resume non cambino.
- **Files:** `web/transfer/src/stage-worker.js` (nuovo); `web/transfer/src/storage.js`;
  `web/transfer/esbuild.mjs`; `build.rs:REQUIRED`; `src/web_transfer_http.rs:asset route`;
  `README.md`; `docs/transfer/WEB_TRANSFER_PERF.md:before/after`.
- **Change:** un worker per stanza (non per trasferimento) riceve `(dirSegments, name,
  bytes)` con l'`ArrayBuffer` trasferito, scrive con `createSyncAccessHandle`, fa
  `flush()` e `close()` e risponde; il record IDB resta committato dal thread principale
  DOPO la risposta, così l'ordine crash-safe è identico. Un motore senza sync access
  handle (o un worker che non parte) ricade sul percorso attuale, che resta la
  definizione di correttezza. Vietato: un part file per più chunk, `keepExistingData`,
  e qualunque allargamento della granularità del resume.
- **Unit tests:** `stage_worker_falls_back_when_sync_handles_are_missing`;
  `staged_bytes_are_identical_on_both_paths`; `record_commits_only_after_the_part_closes`.
- **e2e tests:** `T-WEB-PERF` (rapporto prima/dopo per motore, stessi campioni grezzi) +
  `T-WEB-DOWNLOAD-RELAY` e `T-WEB-CANCEL-RESUME` verdi byte per byte su tre motori.
- **Done:** gates green + `docs/transfer/WEB_TRANSFER_PERF.md` riporta il prima/dopo per
  motore con i campioni + nessun test byte-esatto cambiato + closed in `STATE.md`.

- **Esito (2026-09-16):** il worker esiste (`web/transfer/src/stage-worker.js`, un bundle
  ESM servito come gli altri), `storage.js` lo usa quando c'è e ricade sul percorso
  principale quando manca, rifiuta la capability, non risponde entro
  `STAGE_WORKER_HELLO_MS` (3 s) o fallisce una scrittura. Il record IDB resta committato
  dal thread principale DOPO la risposta del worker, quindi l'ordine crash-safe e la
  granularità per chunk del resume non si muovono.
  **La misura ha corretto la previsione di 3.10, che era giusta a metà.** Prima e dopo
  girano nella STESSA pagina, nello STESSO run, alternando l'ordine dei due bracci
  (senza alternanza il primo braccio di ogni ripetizione vince un vantaggio sistematico:
  la prima passata leggeva +6/+10/+14% e non è sopravvissuta). Su 32 MiB, otto
  ripetizioni: lo staging cala del 9.7% su chromium (277.6 → 250.6 ms) e del **20.9% su
  firefox** (485.0 → 383.5), ma **CRESCE del 16% su webkit** (130.5 → 151.5), dove il
  percorso asincrono è già economico (`close` 0–2 ms contro 183 su firefox) e il salto nel
  worker è solo overhead. End to end: chromium 78.40 vs 74.69 (**1.050**), firefox 42.36
  vs 40.56 (**1.044**), webkit 67.80 vs 67.38 (1.006, dentro il rumore). Lo stallo del
  thread principale, misurato con un timer a 16 ms dentro la pagina, **non cambia** su
  nessun motore: lo staging non bloccava il thread principale, `createWritable` è
  asincrono.
  **Trasferire il buffer non è un'ottimizzazione, è la differenza fra un guadagno e una
  perdita.** La prima implementazione COPIAVA il chunk nel worker (structured clone) per
  poter riprovare sul thread principale: misurata, costava il 4.6% end to end su webkit e
  annullava il guadagno di chromium. La versione spedita trasferisce l'`ArrayBuffer`
  quando la view possiede tutto il buffer, e il worker **restituisce** il buffer insieme a
  un errore gestito, così il fallback ha ancora i byte da scrivere. Solo un worker che
  muore del tutto li perde, e quel caso è il normale errore di trasferimento da cui il
  destinatario sa già riprendere.
  Declinati con la loro misura: lo staging per motore (main thread su webkit, worker
  altrove — 0.6% end to end, dentro il rumore, e sarebbe un ramo permanente su un motore
  libero di cambiare) e una coda più profonda nel worker (un chunk per volta è già la
  costruzione del destinatario).
- **Gate 3.11:** `staged_bytes_are_identical_on_both_paths`,
  `stage_worker_falls_back_when_sync_handles_are_missing` (quattro forme: niente sync
  handle, nessun worker, handshake che non arriva, scrittura che fallisce),
  `record_commits_only_after_the_part_closes` (ordine `flush` → `close` → `idb`) e
  `a_handshake_that_never_lands_does_not_delay_the_first_chunk_forever`; `T-WEB-PERF` con
  i due bracci alternati e la riga `mainthread-stall-ms`; `T-WEB-DOWNLOAD-RELAY` e
  `T-WEB-CANCEL-RESUME` verdi byte per byte su tre motori.

---

## Phase gates

- **Build:** `cargo build --all-features`
- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** `cargo test --all-features --lib && cargo test --all-features --test web_transfer_test -- --test-threads=1 && npm ci --prefix web/transfer && npm run check --prefix web/transfer && npm run test:e2e --prefix web/transfer`
- **Asset drift:** `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist`
- **Regression guard:** `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ && cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`; T-WEB-NOAUTO, T-WEB-NOSTORE, T-WEB-E2EE, T-WEB-ROOM-LIFE, T-WEB-LIMITS e T-WEB-LEGACY passano.
- **README:** aggiornato per la slice relay-only realmente usabile, con installazione, deploy, tutti i flag/default, esempi, sicurezza, limiti e troubleshooting.

## Phase done criterion

Un utente può creare una room con il comando pubblico, selezionare nel browser un file singolo, farlo scaricare manualmente a un altro peer via relay cifrato, annullare/riprendere e distruggere la room chiudendo il CLI. Il server non conserva payload e ogni limite è applicato. README.md descrive esattamente questa slice relay-only e `STATE.md` §11 mostra Phase 3 `DONE` con ogni subfase chiusa.
