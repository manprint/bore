# Phase 0 — Primitive crittografiche e fixture condivisa

> Intent: introdurre codec e derivazione short-link in Rust e JavaScript senza cambiare ancora la URL prodotta o accettata in produzione.
> Shippable alone? Sì: codice additivo, comportamento utente invariato.
> Preconditions: overview e STATE letti; baseline dev ad7d0f47d497; working tree controllato.

## State contract obbligatorio

1. Leggere [STATE.md](STATE.md) per intero. Se §1 è OPEN, completare o annullare quella unità prima di altro lavoro.
2. Prima di editare aprire la sottofase in STATE §1: Type sub-phase, ID, Status OPEN, Intent, Assigned agent:gpt-5.6-luna, Next action; impostare §6 su claimed — nothing written yet.
3. Scrivere test prima o insieme al codice. Non modificare aspettative corrette per ottenere verde.
4. Chiudere soltanto dopo i gate indicati: appendere §4, aggiornare §§5 e 7–11, azzerare §6 e predisporre l’ID successivo.
5. Nessun commit o push. Se interrotti, lasciare OPEN e descrivere file, test e lavoro residuo in §6.

## Contratti da non reinterpretare

- Seed 16 byte; testo Base64URL unpadded esattamente 22 caratteri.
- Salt e tre Info sono quelli byte-esatti in overview.
- RoomId è la metà iniziale di un output HKDF da 32 byte, non una derivazione da 16 richiesta direttamente.
- MemberToken e RoomKey sono due output HKDF distinti da 32 byte.
- OwnerToken non entra in questa derivazione.
- Rust e JS consumano lo stesso fixture.
- Questa fase non cambia build_display_url, boot main.js, protocollo owner o URL E2E.
- Nessuna implementazione Base64 o HKDF artigianale se una primitiva standard già presente copre il caso.

## Sub-phases

### 0.1 Tipo seed, codec Rust e vettore normativo

- **Model:** agent:gpt-5.6-luna
- **Assignment:** implementare e poi rileggere autonomamente il codec Rust; non delegare decisioni crittografiche. Aprire prima STATE 0.1.
- **Files:** Cargo.toml dipendenze; Cargo.lock; src/web_transfer.rs tipi RoomId/MemberToken/RoomKey; src/web_transfer_protocol.rs hkdf32 e test; tests/fixtures/web_transfer/link_v1.json NEW.
- **Change:**
  1. Aggiungere base64 0.22.1 come dipendenza diretta usando la versione già presente nel lock; abilitare soltanto le feature necessarie. Non aggiornare altre dipendenze.
  2. Introdurre RoomLinkSeed come wrapper su [u8; 16]. Non derivare Display. Implementare Debug redatto con testo fisso, mai byte o Base64URL.
  3. Esporre costanti di lunghezza: seed 16, testo 22, RoomId 16, token/key 32. Usarle nei test e nel parser, non numeri duplicati.
  4. Implementare encode Base64URL con engine URL_SAFE_NO_PAD. L’output deve essere sempre 22 caratteri e non contenere =.
  5. Implementare decode stretto per test/harness: verificare tipo/lunghezza/alphabet prima della libreria; decodificare in 16 byte; ricodificare e confrontare byte-per-byte il testo originale. Rifiutare +, /, =, whitespace, Unicode, percent-encoding, lunghezze 21/23 e pad bit finali non canonici.
  6. Non inserire seed in anyhow context, Debug di strutture parent, tracing o panic. Gli errori devono dire solo invalid room link seed.
  7. Creare il fixture JSON con seed e output esatti dell’overview. Inserire anche salt, info e algoritmo, in modo che un cambio di label sia un diff visibile.
  8. Conservare hkdf32 come unica implementazione Rust HKDF-SHA-256. Se serve riuso dal modulo CLI, allargare al massimo a pub(crate); non duplicarla.
  9. Implementare derive_room_link_material(seed) che invoca hkdf32 tre volte. Copiare i primi 16 byte del primo output in RoomId; costruire MemberToken e RoomKey tramite i costruttori tipizzati esistenti.
  10. Rifiutare o rigenerare un RoomId tutto-zero nella futura generazione; qui aggiungere almeno il predicato/test puro senza cambiare il flusso produttivo.
- **Unit tests:** fixture_example_encodes_to_22_chars; fixture_example_derives_exact_material; seed_debug_is_redacted; seed_decoder_rejects_padding_and_standard_base64; seed_decoder_rejects_noncanonical_tail_bits usando una mutazione finale come x; all_zero_room_id_is_not_accepted_for_generation; tre label diverse producono tre output diversi.
- **e2e tests:** nessuno: la feature non è ancora cablata. Il fixture deve essere leggibile da un test Rust reale, non soltanto duplicato come literal.
- **Done:** G-FMT, G-LINT, G-BUILD e i test Rust mirati sono PASS; Cargo.lock cambia solo per rendere base64 dipendenza diretta; nessun output CLI o URL browser cambia; STATE 0.1 è chiuso.

### 0.2 Mirror JavaScript, parser corto additivo e WebCrypto

- **Model:** agent:gpt-5.6-luna
- **Assignment:** implementare il mirror JS confrontando ogni byte con 0.1; self-review specifica per compatibilità Firefox/WebKit e assenza di fallback. Aprire STATE 0.2.
- **Files:** web/transfer/src/crypto.js; web/transfer/src/secrets.js; web/transfer/tests/unit/secrets.test.mjs; web/transfer/tests/unit/crypto.test.mjs o nuovo link.test.mjs; tests/fixtures/web_transfer/link_v1.json.
- **Change:**
  1. Lasciare temporaneamente parseRoomUrl e buildRoomUrl legacy usati da main.js. Aggiungere funzioni nuove con nomi non ambigui, per esempio decodeRoomLinkSeed, parseShortRoomUrl, buildShortRoomUrl e deriveRoomLinkMaterial.
  2. Il parser corto accetta soltanto pathname /transfer/, search vuota e hash # più 22 caratteri. Non usare URLSearchParams, non decodificare percent-encoding e non tollerare trailing component.
  3. Il decoder Base64URL deve essere indipendente da Buffer. Usare API browser disponibili nei target, convertendo -/_ in +// soltanto internamente e aggiungendo padding temporaneo; validare e ricodificare prima di restituire Uint8Array(16).
  4. La funzione encoder deve restituire forma URL-safe senza padding ed essere usata dal controllo di canonicalità.
  5. Spostare o esportare textEncoder senza creare una seconda codifica delle label. Definire le tre label e il salt una sola volta nel modulo crypto.
  6. Riutilizzare hkdf32 WebCrypto esistente tre volte. Il parametro subtle può essere iniettato nei test, ma il default deve provenire da globalThis.crypto.subtle.
  7. Se importKey, deriveBits o HKDF falliscono, propagare un errore tipizzato/generico privo del seed. Non implementare HMAC-HKDF fallback.
  8. Restituire l’interfaccia già utile al resto dell’app: roomId lowercase hex, memberToken lowercase hex, roomKey lowercase hex. Non cambiare ancora il boot.
  9. Leggere il fixture dal filesystem nei test Node. Non copiare i valori attesi in un secondo fixture JS.
  10. Tenere il codice compatibile con target esbuild Chrome 120, Edge 120, Firefox 120, Safari 17: niente Buffer, API Node, iterator helper recente o top-level feature non targettata nel bundle.
- **Unit tests:** short_url_round_trips_exact_seed; shared_fixture_derives_exact_rust_values; malformed_short_urls_are_rejected; noncanonical_final_sextet_is_rejected; query_and_old_path_are_rejected_by_short_parser; missing_subtle_fails_without_fallback; deriveBits failure non include il seed nel messaggio.
- **e2e tests:** nessuno ancora; main.js continua a usare il formato esistente. Eseguire npm run test:unit e npm run build per provare sintassi e bundle.
- **Done:** G-JS PASS; fixture Rust e JS concordano; bundle costruisce; nessun riferimento Node entra in dist; vecchi unit test restano verdi perché il boot non è cambiato; STATE 0.2 chiuso.

### 0.3 Audit cross-language e red-check delle primitive

- **Model:** agent:gpt-5.6-luna
- **Assignment:** review autonoma, separata dall’implementazione; produrre evidenza in STATE, correggere prima di chiudere. Aprire STATE 0.3.
- **Files:** test e fixture di 0.1/0.2; src/web_transfer_protocol.rs; web/transfer/src/crypto.js; web/transfer/src/secrets.js; STATE.md.
- **Change:**
  1. Costruire un piccolo oracolo di test con node:crypto hkdfSync che legge il fixture e verifica i tre output. Deve vivere nei test, non nel bundle.
  2. Verificare che Rust ring::hkdf e WebCrypto interpretino salt/info nello stesso ordine; non basarsi soltanto sul fatto che entrambi i test usano le stesse costanti.
  3. Eseguire red-check controllati: mutare localmente una label, l’ultimo carattere del seed e il troncamento RoomId; ciascuna mutazione deve far fallire il test atteso. Ripristinare subito e registrare il risultato, senza lasciare mutazioni.
  4. Cercare duplicati delle stringhe bore-web-transfer-*-v1. Le copie sono ammesse soltanto fra Rust, JS, fixture e protocol doc futuro; non creare varianti ortografiche.
  5. Verificare che nessun test stampi il seed completo in un messaggio di failure. Etichettare i casi con nomi, non con la URL.
  6. Eseguire git diff --check e ispezionare Cargo.lock per update estranei.
- **Unit tests:** tutti i test 0.1/0.2 e l’oracolo indipendente PASS; i tre red-check falliscono nel punto previsto e vengono ripristinati.
- **e2e tests:** nessuno, comportamento runtime ancora invariato.
- **Done:** G-FMT, G-LINT, G-RUST-UNIT, G-JS e G-DIFF PASS; audit scritto nel ledger STATE; tree senza mutazioni temporanee; STATE 0.3 chiuso.

### 0.4 Update README.md

- **Model:** agent:gpt-5.6-luna
- **Assignment:** verifica documentale finale della fase; non annunciare un formato non ancora attivo. Aprire STATE 0.4.
- **Files:** README.md sezione Browser-to-browser transfer; docs/transfer/WEB_TRANSFER_PROTOCOL.md solo in lettura; STATE.md.
- **Change:** verificare che README descriva ancora correttamente il formato attivo lungo. Non pubblicare il link corto prima del cutover atomico della fase 1. Se le sole modifiche sono helper interni non raggiungibili, lasciare README invariato e registrare la verifica; non inserire roadmap o dettagli del piano.
- **Unit tests:** nessuno nuovo; controllare che gli esempi README esistenti siano ancora coperti dagli E2E correnti.
- **e2e tests:** eseguire almeno i test README esistenti se la suite li separa; devono restare verdi con il formato ancora attivo.
- **Done:** nessuna promessa prematura; riga Docs fase 0 in STATE aggiornata; tutti i gate della fase verdi; STATE 0.4 chiuso e fase 0 DONE.

## Phase gates

- G-FMT: cargo fmt --all -- --check
- G-LINT: cargo clippy --all-features --all-targets -- -D warnings
- G-BUILD: cargo build --locked --all-features
- G-RUST-UNIT: cargo test --all-features --lib web_transfer
- G-JS: npm run check --prefix web/transfer
- G-DIFF: git diff --check
- Test mirati seed/KDF sia Rust sia Node.

## Phase done criterion

Il repository possiede una sola specifica seed/KDF, un fixture condiviso e implementazioni Rust/JS byte-identiche, ma nessun comportamento utente è cambiato. Tutti i test precedenti restano verdi, nessun seed è loggabile e STATE §11 marca 0.1–0.4 DONE.
