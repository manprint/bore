# Web Transfer — link room corti

> Piano di implementazione per agent:gpt-5.6-luna. Leggere [STATE.md](STATE.md) per intero prima di aprire una fase. Questo piano non autorizza commit, push o release.

## Stato

- Stato del piano: PLANNED, nessuna implementazione iniziata.
- Branch ricognizione: dev.
- Commit ricognizione: ad7d0f47d497.
- Working tree alla pianificazione: pulito.
- Modalità commit: WIP commits OFF.
- Implementazione e review: sempre agent:gpt-5.6-luna; la review finale è una self-review esplicita e separata.

## Obiettivo

Sostituire il link Web Transfer lungo:

    https://bore.example/transfer/<32hex>#m=<64hex>&k=<64hex>

con un’unica capability da 128 bit codificata Base64URL senza padding:

    https://bore.example/transfer/#YVCYDRDYkjNIIyoIIHIH_w

Il token dopo # deve avere esattamente 22 caratteri. Dal seed di 16 byte CLI e browser derivano deterministicamente, con tre HKDF-SHA-256 separate e domain separation, il RoomId, il MemberToken e il RoomKey. L’OwnerToken resta casuale e indipendente. Il fragment resta visibile nella barra per tutta la vita della pagina: refresh, copia dalla barra e pulsante Copia link room devono restituire lo stesso link corto.

## Scenario di riferimento

1. Un server aggiornato viene avviato con Web Transfer abilitato.
2. bore transfer web stampa una sola URL nella forma origin/transfer/#seed22.
3. La richiesta HTTP del browser è GET /transfer/ e non contiene il seed.
4. Chrome, Edge, Brave, Firefox e Safari/WebKit decodificano lo stesso seed e derivano gli stessi RoomId, MemberToken e RoomKey della CLI.
5. Il browser apre il WebSocket di controllo sul RoomId derivato e presenta il MemberToken derivato; il RoomKey non lascia il browser.
6. Il fragment resta nella barra dopo la connessione e dopo un reload. Copiare la barra o usare Copia link room produce la stessa URL canonica.
7. Un link lungo precedente viene rifiutato come Link incompleto e non apre alcun WebSocket.
8. Client e server nativi di generazioni diverse si rifiutano chiaramente: non esiste downgrade silenzioso.

## Risultato consegnabile

- Link room esattamente origin/transfer/#seed22.
- Seed CSPRNG di 16 byte, 128 bit effettivi.
- Base64URL RFC 4648 URL-safe, senza =, forma canonica obbligatoria.
- Tre derivazioni HKDF-SHA-256 indipendenti per RoomId, MemberToken e RoomKey.
- OwnerToken CSPRNG da 32 byte, non derivato dal seed.
- RoomId scelto dal client e verificato dal server, senza overwrite in caso di collisione.
- Hard cutover del controllo owner nativo; nessun supporto ai vecchi link browser.
- Nessun servizio shortener, database di mapping o persistenza server-side del seed.
- Matrice obbligatoria Chromium, Firefox e WebKit; smoke periodico/manuale su Chrome, Edge e Brave.
- Fixture unica Rust/JavaScript con vettori byte-per-byte.
- README e protocollo normativo aggiornati nello stesso lavoro.

## Fuori ambito

- Link revocabili senza chiudere la room.
- Alias leggibili, shortener HTTP, redirect o lookup server-side.
- Recupero di un seed perso.
- Compatibilità con URL 32hex + m/k o con owner control precedente.
- Fallback HKDF scritto a mano in JavaScript.
- Modifica del protocollo payload, dei frame cifrati, di WebRTC, relay o direct path.
- Aumento della forza oltre 128 bit: il requisito accetta esplicitamente 128 bit pieni.
- Automazione del Safari installato: WebKit è il gate continuo; Safari reale resta smoke di release quando disponibile.

## Contratto crittografico congelato

### Seed e codifica

- Input casuale: 16 byte da CSPRNG del sistema.
- Testo: Base64URL RFC 4648, alphabet A–Z a–z 0–9 _ -, senza padding.
- Lunghezza testuale: esattamente 22 caratteri.
- Decodifica: deve produrre esattamente 16 byte.
- Canonicalità: decode seguito da encode deve restituire lo stesso testo. Questo rifiuta padding, alfabeti standard + e /, whitespace, percent-encoding e bit di padding finali non canonici.
- Path canonico: /transfer/; query vuota; hash formato da # seguito soltanto dal seed.

### HKDF

Tutte le stringhe seguenti sono byte ASCII esatti, senza NUL e senza newline.

| Campo | Valore |
|---|---|
| Hash | SHA-256 |
| IKM | i 16 byte del seed |
| Salt | bore-web-transfer-link-v1 |
| Info RoomId | bore-web-transfer-room-id-v1 |
| Info MemberToken | bore-web-transfer-member-token-v1 |
| Info RoomKey | bore-web-transfer-room-key-v1 |

Eseguire tre chiamate HKDF separate:

- RoomId = primi 16 byte dell’output HKDF da 32 byte con Info RoomId.
- MemberToken = tutti i 32 byte con Info MemberToken.
- RoomKey = tutti i 32 byte con Info RoomKey.

Non sostituire le tre chiamate con un’unica espansione da 80 byte e non riusare una label.

### Vettore normativo minimo

| Campo | Valore |
|---|---|
| seed Base64URL | YVCYDRDYkjNIIyoIIHIH_w |
| seed hex | 6150980d10d8923348232a08207207ff |
| RoomId hex | c5e230000f48c492799fe9ea32d18d8c |
| MemberToken hex | cfa20bcf70b2a0fd23ba06651bdfb9c6291f66df8e9deb5b342974fc2e3d45d6 |
| RoomKey hex | 585579f6a4e991c4b8e632e9d79fb73d89fe02e60ec43a8a69158f704cbc61d2 |

Questi valori devono essere controllati da Rust, JavaScript e da almeno un oracolo indipendente Node crypto durante i test. Il fixture previsto è tests/fixtures/web_transfer/link_v1.json.

## Flusso finale

    CLI CSPRNG
      ├─ seed 16 B ─ Base64URL ────────────────> fragment visibile #seed22
      ├─ HKDF room-id ─ first 16 B ───────────> RoomId inviato al server
      ├─ HKDF member-token ─ 32 B ─ SHA-256 ──> hash inviato al server
      ├─ HKDF room-key ─ 32 B ────────────────> mai inviato al server
      └─ OwnerToken random 32 B ─ SHA-256 ────> lease owner indipendente

    Browser #seed22
      ├─ validazione Base64URL canonica
      ├─ le stesse tre HKDF WebCrypto
      ├─ WebSocket /transfer/ws/control/<RoomId>
      └─ hello con MemberToken; cifratura con RoomKey

## Decision log

| ID | Decisione | Fonte |
|---|---|---|
| D1 | URL finale esatta /transfer/#<22 Base64URL>; il cancelletto resta. | user, richiesta iniziale |
| D2 | Tre HKDF-SHA-256 separate con label distinte; RoomId troncato a 16 byte. | user, Q1:a |
| D3 | Se WebCrypto HKDF non è disponibile il browser fallisce prima di qualsiasi WebSocket; nessun fallback JS. | user, Q2:a |
| D4 | Hard cutover client/server: serve un server aggiornato; niente fallback lungo. | user, Q3:b |
| D5 | Il fragment non viene riscritto o rimosso. Deve restare copiabile dalla barra, anche dopo connessione e reload. | user, Q4; risoluzione planner |
| D6 | Nessuna retrocompatibilità con vecchi link perché non esistono link da preservare. | user, Q5 |
| D7 | Chromium/Firefox/WebKit restano gate ordinari; Chrome/Edge/Brave sono smoke periodici/manuali. | user, Q6:a |
| D8 | Nessun mapping server-side: il seed contiene tutta la capability e resta nel fragment. | conversazione, vincolo accettato |
| D9 | OwnerToken indipendente e casuale; il seed condiviso non autorizza il controllo owner. | requisito sicurezza del piano |
| D10 | Il protocollo browser e i frame restano v1; solo il protocollo nativo owner passa a versione 2. | planner, compatibilità esplicita |

## Conseguenze di sicurezza accettate

- Il fragment non viene incluso nella richiesta HTTP e non arriva a reverse proxy o server.
- Mantenendolo visibile, il seed resta nella cronologia del browser ed è osservabile tramite barra, screenshot o condivisione schermo. È una scelta esplicita necessaria alla copia diretta.
- Il seed è una bearer capability: chi lo possiede deriva autenticazione membro e chiave payload.
- La sicurezza effettiva di RoomId, MemberToken e RoomKey derivati è limitata ai 128 bit del seed, anche se gli output sono più lunghi.
- Il server continua a conoscere RoomId e hash del MemberToken, ma non seed e RoomKey.
- Il RoomKey non deve apparire in frame, URL di rete, log, errori, storage o artefatti.
- Il seed deve apparire soltanto nella URL room intenzionale e nella memoria necessaria al runtime/test; mai in log, nomi artifact o messaggi di errore.

## Compatibilità

| Target | Gate | Aspettativa |
|---|---|---|
| Chromium Playwright | ogni PR/push | parsing, HKDF, reload, copia, WebSocket e flussi completi |
| Firefox Playwright | ogni PR/push | stesso contratto, nessun uso di API Chromium-only |
| WebKit Playwright | ogni PR/push | proxy continuo per Safari, target bundle Safari 17 |
| Google Chrome branded | schedule/manuale | smoke completo con channel chrome |
| Microsoft Edge branded | schedule/manuale | smoke completo con channel msedge |
| Brave branded | schedule/manuale | Chromium Playwright con executablePath esplicito |
| Safari reale | checklist release quando disponibile | apertura, reload, copia barra, due peer e piccolo transfer |

Brave non è un channel Playwright supportato. Il progetto deve usare un executablePath esplicito, sovrascrivibile da BORE_BRAVE_EXECUTABLE_PATH e con default Linux /usr/bin/brave-browser. Il job CI deve installare il pacchetto ufficiale e fallire se l’eseguibile manca; vietato saltare silenziosamente il progetto.

## Reuse map e anchor

Gli anchor appartengono al commit di ricognizione; cercare sempre il simbolo prima di editare.

| Area | Simboli/file |
|---|---|
| Generazione owner e URL | src/web_transfer_cli.rs OwnerSecrets:128, build_display_url:153, run_owner_lease_with:391 |
| Tipi RoomId/MemberToken/RoomKey | src/web_transfer.rs RoomKey:276 e tipi adiacenti |
| HKDF Rust esistente | src/web_transfer_protocol.rs hkdf32:2716 |
| Create/resume wire | src/shared.rs CreateWebTransferRoom:1774, WebTransferRoomCreated:2069 |
| Handler server owner | src/web_transfer.rs serve_owner_first_message:6532 |
| Registry ID scelto | src/web_transfer.rs create_room_with_id:6429, OwnerLease::create:6247 |
| Parser/storage URL browser | web/transfer/src/secrets.js parseRoomUrl:19 e funzioni storage/scrub |
| HKDF WebCrypto | web/transfer/src/crypto.js hkdf32:34 |
| Boot/copy browser | web/transfer/src/main.js onCopyLink:226, boot:1407 |
| Shell HTTP | src/web_transfer_http.rs route GET/HEAD /transfer/:268 |
| Test CLI reale | tests/web_transfer_test.rs split_room_url:2343, t_web_cli:2396 |
| Unit URL browser | web/transfer/tests/unit/secrets.test.mjs |
| E2E bootstrap | web/transfer/tests/e2e/room.spec.mjs |
| E2E sicurezza | web/transfer/tests/e2e/security.spec.mjs |
| Harness E2E | web/transfer/tests/e2e/helpers.mjs |
| Browser config | web/transfer/playwright.config.mjs, package.json |
| CI | .github/workflows/ci.yml web-transfer, web-transfer-e2e, web-transfer-branded |
| Bundle | web/transfer/esbuild.mjs e web/transfer/dist/app.js committato |
| Docs | README.md Browser-to-browser transfer; docs/transfer/WEB_TRANSFER_PROTOCOL.md §1 |

## Fasi

| Fase | File | Obiettivo | Shippable alone? |
|---|---|---|---|
| 0 — Primitive e fixture | [phase_01.md](phase_01.md) | codec seed e HKDF identici Rust/JS, ancora non attivi | sì, additiva |
| 1 — Cutover completo | [phase_02.md](phase_02.md) | server, CLI e browser usano solo il link corto | sì, feature completa |
| 2 — Browser/security/release | [phase_03.md](phase_03.md) | matrice cinque browser, leak audit, package e regressione | sì, chiusura release-ready |

Non iniziare una fase se quella precedente non è DONE in STATE §11. Non chiudere una sottofase con test esistenti rossi.

## Gate globali

| Gate | Comando |
|---|---|
| G-FMT | cargo fmt --all -- --check |
| G-LINT | cargo clippy --all-features --all-targets -- -D warnings |
| G-BUILD | cargo build --locked --all-features |
| G-RUST-UNIT | cargo test --all-features --lib web_transfer |
| G-WEB-RUST | cargo test --all-features --test web_transfer_test -- --test-threads=1 |
| G-JS | npm run check --prefix web/transfer |
| G-E2E | npm run test:e2e --prefix web/transfer |
| G-BRANDED | npm run test:e2e:branded --prefix web/transfer |
| G-SCRIPT-E2E | bash scripts/web_transfer_e2e.sh |
| G-PACKAGE | bash scripts/web_transfer_package_test.sh |
| G-CONTAINER | bash scripts/web_transfer_container_test.sh |
| G-FULL | cargo test --all-features -- --test-threads=1 |
| G-DIFF | git diff --check |

Usare i comandi canonici realmente presenti al momento dell’esecuzione; se uno script accetta filtri ENGINES/STAGES, registrarli in STATE ma non sostituire il run completo finale. Un browser non installato è NOT RUN, non PASS.

## Strategia di test

- Unit Rust: codec canonico, KDF, redazione Debug, nonzero seed-derived RoomId, owner indipendente.
- Unit JS: parser URL stretto, Base64URL canonico, vettore condiviso, errore WebCrypto.
- Wire Rust: owner version 2, RoomId obbligatorio, mismatch old/new, collisione senza overwrite e nessuna allocazione su versione errata.
- Integrazione CLI: stdout esatto, 22 caratteri, seed derivato uguale al room realmente servito, resume invariato.
- E2E browser: fragment persistente, reload, copia barra, pulsante copia, due peer, no storage, link vecchio/malformato senza WebSocket.
- Security: seed assente dalle richieste HTTP/WS URL/frame/log/artifact; MemberToken solo nel primo hello; RoomKey mai trasmesso.
- Cross-browser: stessa fixture e gli stessi flussi in Chromium, Firefox e WebKit; branded smoke Chrome/Edge/Brave.
- Package: bundle rigenerato prima della build Rust perché il binary lo incorpora.

## Failure injection obbligatoria

- Togliere una label HKDF o riusarne una deve rompere il vettore.
- Accettare l’ultimo carattere Base64URL con pad bit non canonici deve rompere un test.
- Ripristinare history.replaceState deve rompere il test del fragment persistente.
- Aprire il WebSocket prima della fine HKDF deve rompere il test WebCrypto-unavailable.
- Accettare il vecchio URL lungo deve rompere il test no-legacy.
- Far generare il RoomId al server deve rompere il test client/server derivation match.
- Saltare Brave perché l’eseguibile manca deve rompere il job branded.

## Rischi e contromisure

| Rischio | Contromisura |
|---|---|
| Rust e JS divergono | fixture unica, oracolo indipendente, costanti byte esatte documentate |
| Decoder permissivo accetta alias | lunghezza/alphabet/decode/re-encode stretti |
| Nuovo client contro vecchio server crea room diversa | owner protocol v2 e controllo RoomId della risposta |
| Vecchio client contro nuovo server crea room legacy | RoomId mandatory e versione owner 2; rifiuto prima dell’allocazione |
| Seed compare in errori/artifact | errori generici, no URL interpolata, audit grep e test artifact |
| Browser senza HKDF degrada | errore UI prima della rete, nessun fallback |
| Test wrong-key non più costruibile modificando k | patch WebCrypto test-only via addInitScript soltanto per la label RoomKey |
| Brave non è channel Playwright | executablePath esplicito e install ufficiale CI |
| WebKit non equivale a Safari branded | dichiarazione trasparente e smoke release reale |
| Collisione RoomId | insert only-if-vacant, nessun overwrite; errore chiaro e comando ripetibile |

## Riferimenti

| ID | Uso | Fonte |
|---|---|---|
| R1 | HKDF extract/expand e info per domain separation | https://www.rfc-editor.org/rfc/rfc5869.html |
| R2 | Base64URL alphabet e padding omissibile | https://www.rfc-editor.org/rfc/rfc4648.html |
| R3 | Fragment separato prima della dereference HTTP | https://www.rfc-editor.org/rfc/rfc3986.html |
| R4 | WebCrypto HKDF deriveBits/deriveKey | https://www.w3.org/TR/webcrypto-2/ |
| R5 | channel Chrome/Edge ed executablePath Chromium | https://playwright.dev/docs/api/class-browsertype |
| R6 | Installazione ufficiale Brave Linux | https://brave.com/linux/ |

## Regola finale di review

agent:gpt-5.6-luna deve eseguire una self-review separata dopo tutti i gate:

1. rileggere ogni Decisione D1–D10;
2. confrontare diff e file plan uno per uno;
3. cercare supporto legacy accidentale, fallback HKDF, logging seed e riscrittura hash;
4. verificare che ogni test negativo fallisca rimuovendo mentalmente o tramite red-check il guard corrispondente;
5. registrare finding e correzioni in STATE prima di dichiarare DONE.
