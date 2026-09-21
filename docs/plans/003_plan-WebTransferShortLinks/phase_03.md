# Phase 2 — Compatibilità browser, leak audit e accettazione

> Intent: dimostrare stabilità del link corto su Chrome, Edge, Brave, Firefox e Safari/WebKit e chiudere tutti i confini di sicurezza/package.
> Shippable alone? Sì: è la chiusura release-ready.
> Preconditions: fase 1 DONE; URL corto unico attivo; source/dist sincronizzati.

## State contract obbligatorio

1. Leggere STATE e aprire una sola unità. Non marcare browser PASS se è stato eseguito soltanto Chromium.
2. Ogni risultato deve indicare engine/progetto e comando. Browser non installato = NOT RUN.
3. Le modifiche CI vanno validate con actionlint se disponibile e con test unit ci.test.mjs.
4. Nessun commit/push/release. Gli artefatti di failure non devono essere allegati al piano.
5. La review 2.4 è lavoro distinto: riaprire il diff dall’inizio e non limitarsi ai test verdi.

## Compatibilità richiesta

- API comuni: URL, TextEncoder, Uint8Array, atob/btoa, crypto.subtle.importKey e deriveBits HKDF.
- Nessun Buffer o node:crypto nel bundle; node:crypto è ammesso solo negli helper test.
- Nessun local/session storage richiesto per reload.
- WebCrypto deve essere disponibile in secure context; loopback HTTP dei test è potentially trustworthy.
- Safari è rappresentato continuamente da Playwright WebKit e dal target esbuild Safari 17. Non chiamare WebKit branded Safari.
- Brave viene pilotato dal backend Chromium Playwright tramite executablePath; non inventare un channel brave.

## Sub-phases

### 2.1 Matrice engine ordinaria e failure WebCrypto

- **Model:** agent:gpt-5.6-luna
- **Assignment:** rendere gli stessi test portabili senza branch per engine; diagnosticare ogni differenza, non saltarla. Aprire STATE 2.1.
- **Files:** web/transfer/tests/e2e/room.spec.mjs; web/transfer/tests/e2e/security.spec.mjs; web/transfer/tests/e2e/helpers.mjs; web/transfer/playwright.config.mjs; web/transfer/esbuild.mjs; web/transfer/tests/unit/ci.test.mjs se controlla i project.
- **Change:**
  1. Parametrizzare i test short-link critici nel normale progetto Playwright, così vengono eseguiti invariati da chromium, firefox e webkit.
  2. Il test principale deve provare: URL esatta 22-char; hash persistente dopo connected; reload; nuova tab; copia clipboard tramite permission/helper portabile; due peer e piccolo trasferimento.
  3. Se clipboard permission differisce, testare il click e intercettare navigator.clipboard con init script comune, senza cambiare il codice prodotto né saltare WebKit/Firefox.
  4. Testare no-HKDF su ogni engine patchando deriveBits prima del bundle. Se un engine non consente patch del prototype, iniettare crypto/subtle tramite la dipendenza testabile della funzione, ma il test E2E deve comunque provare zero WebSocket.
  5. Asserire che page.url conserva esattamente il seed originale, non soltanto che contiene #.
  6. Asserire sessionStorage.length e localStorage.length uguali a zero prima e dopo reload.
  7. Catturare request URL e WebSocket URL: HTTP shell è /transfer/; WS usa RoomId derivato; nessuno contiene seed/member/key.
  8. Non usare timeout diversi per mascherare errori di derivazione. Solo i timeout di connessione già giustificati dal repository.
  9. Confermare esbuild targets Chrome 120, Edge 120, Firefox 120, Safari 17; Brave eredita il target Chromium e non richiede target separato.
  10. Correggere commenti Playwright che dicono fragment scrubbed. I video Playwright catturano il viewport, non autorizzano comunque trace/screenshot; mantenere artifact policy corrente salvo evidenza.
- **Unit tests:** ci.test.mjs continua a imporre chromium/firefox/webkit e artifact policy; fixture/KDF unit tests su Node PASS.
- **e2e tests:** T-WEB-SHORT-URL, T-WEB-HASH-PERSIST, T-WEB-NOHKDF, T-WEB-NOSTORAGE e piccolo transfer PASS separatamente su chromium, firefox, webkit.
- **Done:** G-JS e G-E2E PASS con tre report espliciti; nessuno skip engine-specific; bundle target invariato o giustificato; STATE 2.1 chiuso.

### 2.2 Chrome, Edge e Brave branded

- **Model:** agent:gpt-5.6-luna
- **Assignment:** estendere lo smoke branded esistente senza indebolire Chrome/Edge; verificare config, install CI e failure mode. Aprire STATE 2.2.
- **Files:** web/transfer/playwright.config.mjs; web/transfer/package.json; web/transfer/tests/unit/ci.test.mjs; .github/workflows/ci.yml job web-transfer-branded; README.md soltanto nella sottofase finale 2.5.
- **Change:**
  1. Aggiungere progetto branded-brave con Desktop Chrome device e launchOptions.executablePath.
  2. Risolvere executable path da process.env.BORE_BRAVE_EXECUTABLE_PATH se non vuota, altrimenti /usr/bin/brave-browser su Linux. Non cercare euristicamente molte directory e non saltare il progetto.
  3. Estendere test:e2e:branded affinché selezioni branded-chrome, branded-edge e branded-brave.
  4. Aggiornare ci.test.mjs per rendere obbligatori tutti e tre i branded project e il comando package.
  5. Nel job web-transfer-branded mantenere schedule/manuale come D7. Installare Chrome/Edge con Playwright come oggi.
  6. Installare Brave usando il repository/pacchetto ufficiale descritto da R6: keyring e .sources ufficiali, apt update, apt install brave-browser. Vietato curl pipe shell.
  7. Aggiungere preflight command -v brave-browser e brave-browser --version; la mancanza deve fallire.
  8. Passare BORE_BRAVE_EXECUTABLE_PATH soltanto se il path CI differisce dal default. Non hardcodare path del runner in JS oltre al default Linux documentato.
  9. Rinominare job/artifact in Chrome/Edge/Brave e conservare failure-only retention. Non includere URL room nei nomi.
  10. Verificare il filtro workflow_dispatch: un run manuale richiesto per Web Transfer deve poter eseguire il branded smoke; se la condizione corrente inputs.only != web-transfer lo impedisce, correggerla e fissarla in ci.test.mjs senza far partire branded a ogni push.
  11. Il branded smoke può usare un sottoinsieme stabile ma deve includere bootstrap corto, hash persistente, reload, due peer e transfer minimo; non solo apertura shell.
  12. Non affermare supporto a estensioni Brave o Shields custom: si testa il browser stock.
- **Unit tests:** config contiene branded-brave con executablePath; script branded include tre progetti; workflow ha install/preflight Brave e trigger schedule/manuale; nessun silent skip.
- **e2e tests:** G-BRANDED PASS su macchina/CI con tutti e tre. In ambiente locale privo di browser registrare NOT RUN e usare il workflow come gate richiesto, mai PASS simulato.
- **Done:** Chrome, Edge e Brave hanno risultati distinti; config/CI lint verdi; job fallisce se Brave manca; STATE 2.2 chiuso.

### 2.3 Leak audit, history e artefatti

- **Model:** agent:gpt-5.6-luna
- **Assignment:** audit avversariale dell’intero diff; correggere leak anche se i test funzionali sono verdi. Aprire STATE 2.3.
- **Files:** src/web_transfer_cli.rs; src/shared.rs; src/web_transfer.rs; web/transfer/src/secrets.js; web/transfer/src/crypto.js; web/transfer/src/main.js; web/transfer/tests/e2e/security.spec.mjs; web/transfer/tests/e2e/helpers.mjs; web/transfer/playwright.config.mjs; .github/workflows/ci.yml; web/transfer/dist/app.js.
- **Change:**
  1. Classificare i valori: seed segreto condiviso; RoomKey segreto; MemberToken bearer inviato una volta; OwnerToken owner-only; RoomId loggabile.
  2. Audit Rust Debug/Display/error/tracing. CreatedRoom mostra soltanto RoomId; RoomLinkSeed è redatto; mismatch e collisioni non interpolano request/URL/hash.
  3. Audit browser: seed resta inevitabilmente location.hash e memoria; non deve entrare in DOM, aria labels, title, console, exceptions, WebSocket URL, fetch URL, session/local storage, IndexedDB o diagnostics.
  4. Audit helper/test: rimuovere errori come unparseable room URL <full URL>. Usare messaggi generici e identificatori di caso.
  5. Intercettare tutte le request HTTP e WebSocket durante un transfer. Verificare assenza del seed testuale, RoomKey e MemberToken negli URL; MemberToken appare in un solo primo frame hello; RoomKey e seed in zero frame.
  6. Verificare che Referer, se presente su asset same-origin, non contenga il fragment. Non richiedere header impossibili al browser; provare il valore osservato.
  7. Verificare che hash rimanga anche dopo room_closed/unavailable perché D5 vieta la riscrittura. Il testo UI può avvertire che la room non è più disponibile.
  8. Verificare failure artifacts: trace e screenshot restano off; video failure non include browser chrome; nomi artifact/test non derivano dalla URL. Se reporter stampa page.url in failure custom, eliminare il valore segreto.
  9. Eseguire ricerca sul bundle e sorgenti per seed in log/format. La presenza delle label KDF e del parser è attesa; la stampa runtime no.
  10. Verificare che server HTTP access log osservi /transfer/ e i WS derivati, mai #seed. Aggiungere test lato server/proxy se il solo browser capture non copre il log.
  11. Documentare in threat model che browser history conserva la capability; non tentare di cancellarla in teardown.
  12. Red-check: aggiungere temporaneamente seed a un error message e dimostrare che il test leak fallisce; ripristinare.
- **Unit tests:** Debug/error redaction Rust; JS error redaction; ci artifact policy.
- **e2e tests:** T-WEB-SEED-BOUNDARY; T-WEB-HELLO-ONCE; T-WEB-NO-STORAGE; T-WEB-HISTORY; T-WEB-ARTIFACT-NAMES.
- **Done:** audit senza finding aperti; red-check osservato; G-RUST-UNIT, G-JS, security.spec sui tre engine e G-DIFF PASS; STATE 2.3 chiuso.

### 2.4 Full regression e self-review finale

- **Model:** agent:gpt-5.6-luna
- **Assignment:** review finale indipendente della propria implementazione, poi tutti i gate. Non correggere oltre scope senza registrare deviazione. Aprire STATE 2.4.
- **Files:** tutti i file del diff; STATE.md; nessuna modifica a piani storici docs/plans/001_plan-WebTransfer o 002_plan-TransferLink.
- **Change:**
  1. Rileggere overview D1–D10 e marcare ciascuna con file/test che la prova.
  2. Confrontare la URL stdout reale con regex ^https?://[^/]+/transfer/#[A-Za-z0-9_-]{22}$ e con il fixture.
  3. Ricontrollare che browser PROTOCOL_VERSION e bore-transfer-v1 non siano diventati 2; solo owner control è 2.
  4. Ricontrollare che CreateWebTransferRoom room_id sia mandatory e che server non chiami il generatore casuale legacy su questo percorso.
  5. Ricontrollare reconnect/resume: stesso RoomId, OwnerToken, seed e URL; nessuna seconda room.
  6. Ricontrollare --relay-only echo, Ctrl+C/SIGTERM close e owner grace: il refactor secrets non deve cambiare lifecycle.
  7. Ricontrollare no-legacy: nessun parser/fallback/storage; vecchi literal ammessi solo in test negativo e piani storici.
  8. Ricontrollare browser matrix: Chromium/Firefox/WebKit PASS; Chrome/Edge/Brave PASS o workflow richiesto con evidenza. Safari reale manuale può essere NOT RUN ma non sostituisce WebKit.
  9. Rigenerare bundle, poi ricostruire Rust, poi package/container; non invertire.
  10. Eseguire full Rust seriale per evitare port contention e tutte le suite Web Transfer.
  11. Eseguire git diff --check e controllare git status: solo file attesi, niente test-results, node_modules, video, cert o temp.
  12. Annotare finding con severity. Ogni blocker/major va corretto e i gate interessati rieseguiti prima di chiudere.
- **Unit tests:** G-RUST-UNIT e G-JS completi; nessun test ignorato nuovo.
- **e2e tests:** G-WEB-RUST, G-E2E, G-BRANDED, G-SCRIPT-E2E, G-PACKAGE, G-CONTAINER e G-FULL; gate non disponibili marcati NOT RUN con ragione e owner.
- **Done:** zero blocker/major aperti; tutti i gate obbligatori disponibili PASS; mapping D1–D10 registrato; tree pulito da artifact; STATE 2.4 chiuso.

### 2.5 Update README.md

- **Model:** agent:gpt-5.6-luna
- **Assignment:** chiusura documentale finale; README è single source of truth. Aprire STATE 2.5.
- **Files:** README.md sezioni Browser-to-browser transfer, security, browsers, testing e troubleshooting; docs/transfer/WEB_TRANSFER_PROTOCOL.md per cross-check; STATE.md.
- **Change:**
  1. Elencare supporto e gate reali: Chrome, Edge, Brave, Firefox, Safari; chiarire WebKit CI e smoke Safari reale.
  2. Aggiungere Brave allo smoke branded periodico/manuale e il comando npm run test:e2e:branded.
  3. Documentare prerequisito WebCrypto HKDF e messaggio utente quando manca.
  4. Conservare il link corto con esempio esattamente 22 caratteri e fragment persistente.
  5. Documentare cronologia/screen sharing e bearer capability senza allarmismo né promessa falsa.
  6. Documentare upgrade lockstep server/CLI e assenza compat vecchi link nella sezione troubleshooting.
  7. Aggiornare comandi di test completi, ordine bundle-before-Rust e modalità branded.
  8. Verificare tutti i flag/env e le modalità binary/SSH gateway del Web Transfer già documentate; non eliminare dettagli non collegati.
  9. Eseguire test README e link checker esistente. Ogni comando copiato nel README deve essere valido.
- **Unit tests:** grep assenza di sessionStorage/scrub e vecchio formato nelle sezioni attive; link Markdown validi.
- **e2e tests:** readme.spec e readme-direct.spec PASS sui progetti ordinari; branded smoke usa gli stessi esempi quando applicabile.
- **Done:** README e protocol doc concordano con codice/test/CI; matrice cinque browser dichiarata con precisione; riga Docs DONE; STATE 2.5 chiuso e piano completo.

## Phase gates

- G-FMT: cargo fmt --all -- --check
- G-LINT: cargo clippy --all-features --all-targets -- -D warnings
- G-BUILD: cargo build --locked --all-features
- G-RUST-UNIT: cargo test --all-features --lib web_transfer
- G-WEB-RUST: cargo test --all-features --test web_transfer_test -- --test-threads=1
- G-JS: npm run check --prefix web/transfer
- G-E2E: npm run test:e2e --prefix web/transfer
- G-BRANDED: npm run test:e2e:branded --prefix web/transfer
- G-SCRIPT-E2E: bash scripts/web_transfer_e2e.sh
- G-PACKAGE: bash scripts/web_transfer_package_test.sh
- G-CONTAINER: bash scripts/web_transfer_container_test.sh
- G-FULL: cargo test --all-features -- --test-threads=1
- G-DIFF: git diff --check
- Workflow lint: actionlint -no-color -oneline se installato/CI.

## Phase done criterion

Il link corto è provato sui tre engine ordinari e sui tre browser Chromium branded, con Brave non skippabile. Leak audit dimostra il confine del fragment, full regression è verde, package/container incorporano il bundle corretto, README/protocollo sono coerenti e agent:gpt-5.6-luna ha chiuso la self-review senza finding blocker/major. STATE §11 è interamente DONE.
