# Phase 1 — Cutover atomico server, CLI e browser

> Intent: rendere il link corto l’unico formato operativo, con RoomId scelto dalla CLI e fragment persistente nel browser.
> Shippable alone? Sì: al termine server, CLI, browser, bundle, test e documentazione sono coerenti.
> Preconditions: fase 0 DONE; fixture e primitive verdi; nessuna unità OPEN in STATE.

## State contract obbligatorio

1. Leggere STATE per intero e aprire una sola unità prima di ogni modifica.
2. Ogni unità deve lasciare build e test esistenti verdi. Quando serve una transizione, mantenere temporaneamente il comportamento precedente soltanto all’interno della sottofase e rimuoverlo prima della chiusura.
3. Non fare commit/push. Non aggiornare dist prima dei sorgenti e dei test.
4. Registrare in §6 ogni file modificato se il lavoro viene interrotto.
5. Chiudere l’unità solo dopo gate e ledger; un errore browser/server non osservato è NOT RUN, non PASS.

## Contratti da non reinterpretare

- Browser control e payload restano v1 e continuano a usare bore-transfer-v1.
- Il native owner control passa a versione 2; non cambiare globalmente PROTOCOL_VERSION del browser.
- CreateWebTransferRoom contiene RoomId obbligatorio e derivato dal seed. Vietato serde(default).
- New client + old server e old client + new server devono fallire, non degradare.
- Il browser accetta soltanto /transfer/#seed22.
- Il fragment resta nella barra; niente replaceState e niente sessionStorage come recovery.
- Il pulsante copia ricostruisce il link dal seed canonico in memoria; non usa MemberToken/RoomKey.
- Il server non riceve mai il seed o il RoomKey.

## Sequenza che mantiene il repository verde

1. Rendere il protocollo owner v2 e far derivare alla CLI una room coerente, ma mantenere per la durata di 1.1 la vecchia URL lunga derivata: il browser corrente continua a funzionare.
2. Nella stessa unità 1.2 cambiare CLI, boot browser, helper E2E e test essenziali; non chiudere finché l’intero slice Web Transfer non è verde.
3. In 1.3 aggiungere i negativi e rimuovere ogni ramo legacy residuo.

## Sub-phases

### 1.1 Owner protocol v2 e RoomId client-selected

- **Model:** agent:gpt-5.6-luna
- **Assignment:** implementare wire e lifecycle con particolare attenzione a nessuna allocazione su mismatch; self-review dei match enum e di tutti i costruttori. Aprire STATE 1.1.
- **Files:** src/shared.rs CreateWebTransferRoom/WebTransferRoomCreated e test serde; src/web_transfer.rs OwnerLease, create_room_with_id, serve_owner_first_message e test; src/web_transfer_cli.rs OwnerSecrets, create/resume e fake connector; tests/web_transfer_test.rs costruttori owner; eventuali costanti pubbliche nel modulo appropriato.
- **Change:**
  1. Introdurre WEB_TRANSFER_OWNER_PROTOCOL_VERSION = 2 separata da web_transfer_protocol::PROTOCOL_VERSION = 1. Documentare che la prima governa create/resume/created/resumed nativi, la seconda browser control e payload.
  2. Aggiungere room_id: RoomId a ClientMessage::CreateWebTransferRoom come campo obbligatorio, senza serde(default). Aggiornare control_frame_summary senza includere seed/token/hash.
  3. Fare usare versione owner 2 a CreateWebTransferRoom, ResumeWebTransferRoom, WebTransferRoomCreated e WebTransferRoomResumed. Non cambiare gli envelope browser v:1.
  4. Estendere OwnerSecrets affinché generi una volta sola RoomLinkSeed e OwnerToken; derivare RoomId, MemberToken e RoomKey dalle primitive fase 0. Seed e derivati devono restare invariati durante tutti i reconnect.
  5. Generare nuovamente il seed se il RoomId derivato è all-zero. Non usare loop illimitato non testabile: estrarre un helper con RNG iniettabile o limite ragionevole e errore impossibile esplicito.
  6. L’hash member deriva dal MemberToken; owner hash deriva dall’OwnerToken indipendente. Verificare che owner non cambi modificando il seed in un test deterministico.
  7. Aggiungere OwnerLease::create_with_id o equivalente stretto che usa create_room_with_id. Non sovrascrivere mai una room esistente; il permit globale deve essere rilasciato su errore.
  8. Nel server validare versione owner prima di registry lookup/allocazione. Validare RoomId nonzero. Creare esattamente l’ID richiesto e rispondere con quello effettivamente installato.
  9. Nel client verificare sempre response version e room_id. In caso di risposta con ID diverso, chiudere bounded la room restituita se possibile e fallire con istruzione di aggiornare client e server; non stampare alcuna URL.
  10. Un vecchio server riceve version 2 e deve rifiutare prima di creare. Un vecchio client manca room_id/versione 2 e il nuovo server deve chiudere/rifiutare prima di creare. Scrivere test per entrambe le direzioni, senza simulare successo.
  11. Aggiornare tutti i literal enum in src e tests. Non aggiungere default per far compilare fixture vecchi: il cutover è intenzionale.
  12. Per mantenere il runtime verde durante questa unità, build_display_url può ancora emettere temporaneamente il formato lungo usando i derivati; annotare in STATE che è transitorio e non chiudere la fase.
- **Unit tests:** create_v2_round_trips_with_required_room_id; create_v1_is_rejected_before_allocation; missing_room_id_does_not_deserialize; requested_room_id_is_installed_exactly; duplicate_room_id_never_overwrites_existing_room; response_room_mismatch_closes_and_prints_nothing; owner_token_is_independent_from_seed; reconnect_reuses_seed_room_and_owner.
- **e2e tests:** t_web_cli continua a passare con la URL transitoria; aggiungere controllo che room server e derivazione CLI coincidano. Nessun test può leggere segreti da log.
- **Done:** G-FMT, G-LINT, G-BUILD, G-RUST-UNIT e G-WEB-RUST PASS; browser protocol v1 invariato; zero room allocate nei mismatch; STATE 1.1 chiuso.

### 1.2 Switch URL CLI e bootstrap browser con fragment persistente

- **Model:** agent:gpt-5.6-luna
- **Assignment:** eseguire il cutover come un’unica unità coerente; non fermarsi con CLI e bundle incompatibili. Aprire STATE 1.2.
- **Files:** src/web_transfer_cli.rs build_display_url/run_owner_lease_with e test; web/transfer/src/secrets.js; web/transfer/src/crypto.js; web/transfer/src/main.js import, onCopyLink, teardown e boot; examples/web_transfer_e2e_owner.rs commenti; web/transfer/tests/e2e/helpers.mjs; web/transfer/tests/e2e/room.spec.mjs; web/transfer/tests/e2e/readme.spec.mjs; web/transfer/tests/e2e/readme-direct.spec.mjs; tests/web_transfer_test.rs split_room_url/t_web_cli; test unit fase 0.
- **Change:**
  1. Cambiare build_display_url affinché accetti origin e RoomLinkSeed e produca esattamente origin normalizzata + /transfer/# + seed22. Non includere RoomId, m=, k=, query o slash aggiuntivi.
  2. CreatedRoom continua a esporre room_id per log sicuro e test, ma Debug non deve contenere display_url o seed. Aggiornare i canary test per cercare il seed, non soltanto m/k.
  3. Sostituire nel browser il parser legacy con parseShortRoomUrl. Rimuovere dal boot il fallback /transfer/<room>, loadSecrets, saveSecrets e scrubFragment.
  4. Conservare roomSeed canonico in memoria di modulo insieme a roomId e secrets derivati. Non scriverlo in sessionStorage, localStorage, IndexedDB, DOM, dataset o test hook.
  5. Trasformare il boot in una funzione async esplicita. Ordine obbligatorio: installare solo l’hook diagnostico innocuo; parse URL; decode; await tre HKDF; assegnare roomId/secrets; poi e soltanto poi startSession. Evitare promise non awaited.
  6. Distinguere errore link da WebCrypto non disponibile. Link malformato mostra Link incompleto; HKDF/subtle non disponibile mostra un messaggio stabile come Browser non supportato: WebCrypto HKDF non disponibile. Entrambi non aprono WebSocket.
  7. Non invocare history.replaceState. Dopo connessione, errore, room_closed e reload, window.location.hash deve rimanere identico.
  8. onCopyLink usa buildShortRoomUrl(window.location.origin, roomSeed), non roomId/member/key e non una URL presa da storage. Il testo copiato deve uguagliare la URL canonica nella barra.
  9. teardownUnavailable deve interrompere sessione e rimuovere secrets dalla memoria quanto possibile, ma non alterare la barra. Non chiamare clearSecrets; eliminare le API storage/scrub obsolete da secrets.js quando non hanno più caller.
  10. Aggiornare helper E2E per leggere seed22 e derivare roomId/member/key con node:crypto hkdfSync come oracolo indipendente. Errori helper non devono interpolare roomUrl.
  11. Aggiornare split_room_url nel test Rust: estrarre il seed, decodificarlo strettamente e derivare RoomId/MemberToken con un oracolo di test indipendente o fixture, così room_alive continua a interrogare la room reale.
  12. Aggiornare regex degli E2E README e commento dell’example. La riga machine-readable resta WEB_TRANSFER_ROOM_URL= ma il valore è corto.
  13. Aggiornare il test room principale: connesso; page.url uguale alla URL corta; hash ancora presente; sessionStorage/localStorage vuoti; reload della stessa pagina riconnette; una seconda context/tab con la stessa URL entra.
  14. Non rigenerare ancora dist manualmente: npm run build a fine unità deve produrlo dai sorgenti.
- **Unit tests:** build_display_url_exact_short_shape; created_room_debug_redacts_seed; boot_order helper se estratto; copy builder equals parser canonical URL; obsolete storage exports assenti.
- **e2e tests:** T-WEB-SHORT-URL — CLI URL regex esatta e join; T-WEB-HASH-PERSIST — hash identico dopo connected/reload; T-WEB-COPY — clipboard uguale a page.url; T-WEB-NOSTORAGE — zero chiavi local/session; due peer reali con seed unico.
- **Done:** G-FMT, G-LINT, G-WEB-RUST, G-JS e il slice G-E2E PASS; nessun formato lungo rimane nel percorso produttivo; dist generato e coerente; STATE 1.2 chiuso.

### 1.3 Negativi, no-legacy e sicurezza della derivazione

- **Model:** agent:gpt-5.6-luna
- **Assignment:** aggiungere test avversariali che dimostrino il cutover; eseguire red-check senza lasciare patch temporanee. Aprire STATE 1.3.
- **Files:** web/transfer/tests/unit/secrets.test.mjs; web/transfer/tests/e2e/room.spec.mjs; web/transfer/tests/e2e/security.spec.mjs; web/transfer/tests/e2e/helpers.mjs; tests/web_transfer_test.rs; src/web_transfer_cli.rs test; src/web_transfer.rs test.
- **Change:**
  1. Aggiungere tabella di input invalidi: seed 21/23 char, =, +, /, spazio, percent escape, Unicode, hash vuoto, query, path room legacy, m/k legacy, carattere finale con pad bits nonzero.
  2. Per ogni input browser invalido catturare window.WebSocket e provare che nessuna istanza è costruita; non limitarsi al testo UI.
  3. Aggiungere test esplicito che una vecchia URL 32hex#m=...&k=... mostra Link incompleto e non usa sessionStorage come fallback.
  4. Aggiungere test WebCrypto failure con page.addInitScript che fa fallire deriveBits HKDF; provare messaggio non supportato, hash invariato e zero WebSocket.
  5. Riscrivere il test wrong room key: non è più possibile cambiare k nella URL conservando room/member. Prima del goto del peer avversario, usare addInitScript per patchare SubtleCrypto.prototype.deriveBits soltanto quando info è bore-web-transfer-room-key-v1 e flipparne un bit. RoomId e MemberToken restano corretti; il peer entra nella stessa room ma rifiuta manifest e snapshot.
  6. La patch test-only deve stare nel test, non in app.js o in __BORE_TEST__. Ripristino automatico con chiusura context.
  7. Aggiungere test server collisione: preinserire RoomId derivato, inviare create e verificare existing room intatta, nessun lease rubato, risposta generica senza seed.
  8. Aggiungere test old/new fake server per errore operatore stabile e nessuna URL stdout.
  9. Verificare che origin con slash finale non produca //transfer e che username/query non possano entrare nell’origin configurata.
  10. Red-check: reintrodurre temporaneamente parser legacy, replaceState e startSession prima di await; ciascun test dedicato deve fallire. Ripristinare.
- **Unit tests:** tutti gli invalidi sopra; owner mismatch/collision; URL normalization; nessun errore contiene seed, member o key.
- **e2e tests:** T-WEB-NOLEGACY; T-WEB-NOHKDF; T-WEB-WRONGKEY; T-WEB-NONET-BADLINK; T-WEB-HASH-PERSIST.
- **Done:** G-RUST-UNIT, G-WEB-RUST, G-JS e G-E2E PASS; tre red-check osservati; nessun ramo legacy o storage recovery trovato con rg; STATE 1.3 chiuso.

### 1.4 Protocollo normativo e threat model

- **Model:** agent:gpt-5.6-luna
- **Assignment:** aggiornare la specifica dopo che i test fissano il comportamento; confrontare ogni valore con il fixture. Aprire STATE 1.4.
- **Files:** docs/transfer/WEB_TRANSFER_PROTOCOL.md §1 e §7; tests/fixtures/web_transfer/link_v1.json; eventuali link docs; STATE.md.
- **Change:**
  1. Riscrivere la forma URL come https://authority/transfer/#seed22 e dichiarare che il formato lungo non è accettato.
  2. Separare chiaramente browser protocol v1 da owner control v2; non rinominare l’intero protocollo payload in v2.
  3. Aggiungere tabella normativa seed, Base64URL, salt, tre info, output e troncamento RoomId.
  4. Documentare owner token indipendente, server view, 128-bit effective security e collision handling.
  5. Sostituire la frase che il browser sposta segreti in sessionStorage e scrubs il fragment: il fragment resta intenzionalmente visibile e nella history per essere copiabile.
  6. Documentare che fragment non va nella request secondo R3, ma può comparire in barra/screenshot/history. Non promettere segretezza contro estensioni o codice same-origin compromesso.
  7. Aggiornare Key hygiene: seed e RoomKey mai loggati; MemberToken soltanto hello; seed non in frame.
  8. Aggiornare fixture generation con il comando/oracolo effettivo usato e non includere materiale casuale nuovo a ogni run.
- **Unit tests:** test doc/fixture se presenti; grep automatico che i literal del fixture corrispondano alla tabella.
- **e2e tests:** nessuno nuovo; i test 1.2/1.3 sono l’oracolo eseguibile della prosa.
- **Done:** protocol doc e fixture non divergono; nessun testo sessionStorage/scrub legacy resta nelle sezioni normative; link validi; STATE 1.4 chiuso.

### 1.5 Bundle, package e regressione della fase

- **Model:** agent:gpt-5.6-luna
- **Assignment:** rigenerare artefatti in ordine corretto, eseguire gate completi e fare prima self-review di fase. Aprire STATE 1.5.
- **Files:** web/transfer/dist/app.js; eventuale dist/app.css solo se il build deterministico lo modifica; scripts/web_transfer_e2e.sh; scripts/web_transfer_package_test.sh; scripts/web_transfer_container_test.sh; file modificati 1.1–1.4; STATE.md.
- **Change:**
  1. Eseguire npm run build prima di cargo build: il binary incorpora dist.
  2. Controllare il diff dist: deve contenere il parser/derivazione nuovi e non storage/scrub/URL lunga. Non editare bundle minificato.
  3. Aggiornare assertion script che cercano la vecchia URL. Gli harness devono derivare dal seed, non chiedere alla produzione di stampare segreti aggiuntivi.
  4. Eseguire package test per provare che il tar/crate contiene il bundle nuovo e nessun node_modules/test-results.
  5. Eseguire container smoke con server e owner della stessa build. Hard cutover significa che immagini miste falliscono con messaggio di upgrade; documentare, non aggiungere fallback.
  6. Eseguire suite Rust seriale Web Transfer e Playwright sui tre engine ordinari.
  7. Cercare nel diff e nei file runtime: #m=, &k=, SESSION_KEY_PREFIX, scrubFragment, saveSecrets, loadSecrets, /transfer/<room> come URL shell. Occorrenze in piani storici 001 sono escluse e non vanno editate.
  8. Self-review: response mismatch, reconnect, Ctrl+C/close, relay-only echo, old-server error e URL stdout unica.
- **Unit tests:** tutte le suite unit Rust/JS PASS; bundle drift check PASS.
- **e2e tests:** G-E2E, G-SCRIPT-E2E, G-PACKAGE e G-CONTAINER quando disponibili; ogni skip è registrato NOT RUN.
- **Done:** artefatti source/dist sincronizzati; gate fase verdi; zero occorrenze legacy in runtime/test attivo salvo test negativi; STATE 1.5 chiuso.

### 1.6 Update README.md

- **Model:** agent:gpt-5.6-luna
- **Assignment:** aggiornare README come single source of truth e verificare esempi con i binari correnti. Aprire STATE 1.6.
- **Files:** README.md sezione Browser-to-browser transfer, opening a room, security, troubleshooting, browsers e test; web/transfer/tests/e2e/readme.spec.mjs; web/transfer/tests/e2e/readme-direct.spec.mjs; STATE.md.
- **Change:**
  1. Sostituire ogni esempio lungo con room: https://files.example.com/transfer/#YVCYDRDYkjNIIyoIIHIH_w o placeholder chiaramente 22-char.
  2. Spiegare in linguaggio utente che # è necessario per non inviare la capability al server.
  3. Dire esplicitamente che il link resta nella barra ed è copiabile direttamente o con Copia link room; refresh usa lo stesso fragment.
  4. Rimuovere istruzioni su sessionStorage, fragment scrubbed e riapertura /transfer/<id>.
  5. Avvertire che cronologia, screenshot e condivisione schermo possono mostrare il link; chi possiede il link entra nella room.
  6. Dichiarare hard cutover: aggiornare server e CLI insieme; link precedenti non sono supportati.
  7. Conservare tutti i flag, setup reverse proxy, SSH gateway/binary e comandi end-to-end già richiesti dal README; il cambio URL non deve cancellare documentazione esistente.
  8. Aggiornare sezione browser senza anticipare Brave finché fase 2 non aggiunge il gate; qui mantenere Chrome/Edge/Firefox/Safari conformi ai test reali disponibili.
  9. Far eseguire i comandi README dagli E2E readme.
- **Unit tests:** verifica link Markdown e grep assenza esempi legacy nelle sezioni attive.
- **e2e tests:** readme.spec e readme-direct.spec PASS con output CLI esatto.
- **Done:** README descrive l’intero flusso corto e nessun vecchio recupero; G-E2E README verde; riga Docs aggiornata; STATE 1.6 chiuso e fase 1 DONE.

## Phase gates

- G-FMT: cargo fmt --all -- --check
- G-LINT: cargo clippy --all-features --all-targets -- -D warnings
- G-BUILD: cargo build --locked --all-features
- G-RUST-UNIT: cargo test --all-features --lib web_transfer
- G-WEB-RUST: cargo test --all-features --test web_transfer_test -- --test-threads=1
- G-JS: npm run check --prefix web/transfer
- G-E2E: npm run test:e2e --prefix web/transfer
- G-SCRIPT-E2E: bash scripts/web_transfer_e2e.sh
- G-PACKAGE: bash scripts/web_transfer_package_test.sh
- G-CONTAINER: bash scripts/web_transfer_container_test.sh
- G-DIFF: git diff --check

## Phase done criterion

Un server e una CLI aggiornati creano una room dal seed, stampano solo il link corto e ogni engine ordinario entra dalla stessa capability. Fragment persistente, reload e copia barra sono provati. Vecchi link e native owner incompatibili falliscono senza room fantasma o rete browser. Protocol doc, README, sorgenti e bundle sono coerenti; STATE §11 marca 1.1–1.6 DONE.
