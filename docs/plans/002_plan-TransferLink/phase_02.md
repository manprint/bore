# Phase 1 — Link pubblico per file singolo, TLS e lifecycle

> **Intent:** rendere utilizzabile `bore transfer link file`, con URL HTTPS, download concorrenti/ripetibili e shutdown supervisionato.
> **Shippable alone?** sì: file singolo; directory/multipli e stdin/exec arrivano nelle fasi successive e qui sono rifiutati esplicitamente.
> **Preconditions:** fase 0 (`phase_01.md`) DONE; moduli HTTP/source bounded e relativo target test presenti.

## State contract (mandatory)

1. Leggere [STATE.md](STATE.md); risolvere unità OPEN usando §6. Eseguire gate disponibili §3 contro §1/§7/§11; correggere lo stato quando il repo lo smentisce.
2. Aprire ogni sottofase PRIMA di editare: §1 Type sub-phase, ID, OPEN, Intent, Assigned, Next action; §6 `claimed — nothing written yet`.
3. Dopo i gate appendere ledger §4, aggiornare §§5,7–11, §6 `none — tree consistent`, §1 prossimo ID/none, timestamp. WIP commits off: nessun commit/push.
4. Se interrotti lasciare OPEN con file, modifiche residue e test mancanti in §6.

## Contratto della fase

Riutilizzare vhost, non costruire un server/registro/protocollo transfer nuovo. `Client::new_vhost_provider_with_udp` (`src/client.rs:609`) prende local_host, local_port, to, subdomain, client_id, secret, insecure, carriers, udp, ProviderMeta, access_logger. Il costruttore riceve `VhostReady { http_url, https_url }` a `:683` ma oggi scarta le URL. `listen(self)` a `:984` avvia il servizio. Il loopback HTTP deve essere già in ascolto prima della registrazione.

URL `https://transfer-<16 a-z0-9>.<dominio-vhost>/<filename>`; dominio/porta ricavati dalla HTTPS URL annunciata dal server, NON da `--to`. Il wildcard vhost già configurato copre quel label. Il server termina TLS (`src/vhost.rs:1958`) e può leggere i bytes; nessuna promessa E2E. La tratta A→server, compresi tutti i carrier TCP di fallback, deve restare cifrata e verificata.

## Sub-phases

### 1.1 API client additive, URL conservate e scope di sessione

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review obbligatoria di lifecycle/compatibilità; agent-2 implementa.
- **Files:** `src/client.rs:35`, `:201`, `:374`, `:609`, `:683`, `:773`, `:822`, `:984`, `:1616`, `:1670`, `:1745`, `:2130`; `src/transport.rs:145`; `Cargo.toml:87`; `tests/vhost_test.rs:319`; `tests/transfer_link_test.rs`.
- **Change:**
  1. Conservare URL ricevute da VhostReady in campo opzionale del Client con accessor read-only. Aggiornare TUTTI gli inizializzatori Client; non cambiare il wire. Costruttori non-vhost hanno None; logging legacy resta compatibile. Sopprimere SOLO per scope Link i log delle URL a client.rs:688–692: la stampa autorizzata sarà una sola su stdout dopo la validazione HTTPS.
  2. Introdurre scope opzionale per il solo nuovo chiamante Link, creato PRIMA del costruttore: CancellationToken, TaskTracker (aggiungere feature `rt` a tokio-util), raccolta abort handle, callback backend/path. I costruttori pubblici esistenti delegano a un inner con scope None; aggiungere factory scoped per Link senza cambiare le loro firme.
  3. Inventariare ogni spawn raggiungibile dal costruttore, carrier redial, yamux driver, listen, spawn_handle, spawn_direct e relativo task figlio. Con scope Some, tutti i task/handle o risorse che li mantengono vivi devono avere owner e cancellazione; un hook applicato soltanto DOPO il costruttore non basta. Con None mantenere la semantica legacy, inclusi direct pool, carrier e heartbeat.
  4. TaskTracker traccia, NON cancella e close NON vieta nuovi spawn: aggiungere gate esplicito atomico di chiusura allo scope, coordinato con registrazione task, così nessuno spawn sfugge al drain. Token interrompe i select/accept/splice, chiusura carrier fa terminare driver, poi tracker.close, attesa limitata a 5 s, abort dei soli task rimasti e join. Non usare abort_all su task di altre sessioni. Conservare half-close/flush normali finché non c'è cancellation.
  5. Introdurre enum in-process `BackendPath::{RelayTcp,DirectQuic}`. Passarlo ai TRE caller di handle_connection: relay :1670, vhost/public direct :1785, secret direct :2130. Per caller senza hook è solo un valore interno, nessun byte nuovo.
  6. Subito dopo connect backend :1635 e prima di prefix/request forwarding, hook riceve `local_conn.local_addr()` e path osservato. Restituisce lease RAII che vive fino al termine splice. Link mantiene mappa bounded peer-loopback→(id lease,path); handler cerca dopo lettura header, quando l'inserimento è già avvenuto. Cleanup per id/owner, non rimozione cieca di un socket eventualmente riusato. Non abilitare AccessLogger su disco per ottenere questa informazione.
  7. Nessun cambio alla negoziazione/fallback `src/vhost.rs:1410`: qui il path viene osservato, non scelto. Non toccare shared endpoint, buffer UDP, heartbeat bounded o SNI TLS server.
- **Unit tests:** `vhost_ready_urls_are_retained`; `non_vhost_client_has_no_urls`; `backend_path_lease_precedes_forwarded_bytes`; `stale_backend_lease_cannot_remove_new_entry`; `scope_cancel_joins_nested_tasks`; `legacy_scope_none_keeps_stream_bytes_identical`.
- **e2e tests:** T-LINK-CLEANUP (base) — chiudere scope con download bloccato e osservare deregistrazione, porte/FD tornati al baseline e nessun task attivo dello scope; T-LINK-COMPAT — registrazione vhost legacy e percorso TCP/direct già esistenti invariati.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK/G-SERIAL verdi; review ownership approvata; unità chiusa in STATE §§1/4/6/11.

### 1.2 Validazione TLS, trust esplicito e supervisore Link

- **Model:** agent-2:sonnet
- **Assignment:** agent-1:opus review di trust/reconnect; agent-2 implementa.
- **Files:** `src/transfer_link_cli.rs` NEW, `src/lib.rs:15`, `src/transfer_link/mod.rs`, `src/transport.rs:115`, `:145`, `:171`, `src/client.rs:609`, `src/reconnect.rs:79`, `tests/transfer_link_test.rs`.
- **Change:**
  1. Validare --to con `url::Url` PRIMA di `Endpoint::parse`: schema https, host non vuoto, porta valida, niente userinfo/query/fragment e path solo vuoto o `/`. Endpoint attuale è permissivo; non usare il suo successo come validazione. Testare IPv6 bracket o rifiutarlo esplicitamente prima del connect finché l'adapter non lo gestisce correttamente; preferire correggere conversione scoped con host/port strutturati mantenendo legacy.
  2. `--ca-cert PATH` opzionale carica certificati CA PEM aggiuntivi nel trust store webpki, senza disabilitare hostname verification. Config TLS posseduta dallo scope Link e passata a OGNI connect TCP, inclusi carrier extra, redial e reconnect. Nessuna variabile globale, nessuna chiave privata, nessun --insecure. Default scope None mantiene configurazione esistente. PEM vuoto/invalido o file illeggibile = errore anticipato.
  3. Preparare file e bind listener 127.0.0.1:0, generare label con `ring::rand::SystemRandom`: rejection sampling su byte <252 e modulo36, esattamente 16 caratteri. client_id distinto CSPRNG, riutilizzato nei tentativi della stessa sessione; mai stamparlo. Nessun ID basato su tempo/PID.
  4. Registrare vhost scoped con local port effettivo, insecure=false, udp=!relay_only (false se feature UDP assente), carriers impostati, ProviderMeta https_policy=Some(Redirect), basic_auth=None, backend_tls=false, backend_tls_sni=None, access_logger=None. I dati loopback non richiedono certificati locali.
  5. Esigere https_url Some, parse valido https, host con label esatto richiesto, nessun userinfo/query/fragment, base path `/`; preservare dominio/porta del server. Assenza HTTPS, Redirect degradato a HTTP, certificato del CONTROLLO errato o auth fallita sono errori permanenti: cancellare/deregistrare e non stampare URL. VhostReady attesta configurazione HTTPS, non verifica il certificato del frontend pubblico: quello viene verificato da curl/wget/browser al download. Non dichiarare un self-probe pubblico che non è implementato.
  6. Avviare listen sotto scope e solo dopo readiness pubblicare URL su stdout una volta, newline+flush; tutte le statistiche/log su stderr. TCP è caldo anche quando QUIC non è ancora pronto; non attendere un hole punch inesistente. Negoziazione QUIC e uso effettivo sono log distinti.
  7. Supervisore Link: Initializing→Ready→Reconnecting→Ready oppure Stopping→Stopped. Non chiamare ciecamente `reconnect::run(true,...)`: ritenta ogni errore. Riutilizzare tempi/backoff della libreria ma classificare localmente per stadio/tipo: Transport, TlsVerification, Authentication, RegistrationRejected, Protocol, InvalidUrl. `ServerMessage::Error(String)` rimane RegistrationRejected senza inferirne la causa dal testo; nessun nuovo messaggio wire né substring matching.
  8. Config/cert/hostname/auth/HTTP-only/source invalid →errore permanente. IO/timeout/disconnessione controllo →retry transitorio con backoff cancellabile. RegistrationRejected prima di aver pubblicato URL →errore permanente, inclusa rarissima collisione ID (rilanciare comando). Dopo precedente Ready →ritentare RegistrationRejected per al massimo75s dalla prima rejection, con causa generica nei log, per consentire il reaper server60s; allo scadere errore permanente. Non affermare di distinguere collisione/ownership/autorizzazione dal wire generico. Mai cambiare label dopo URL pubblicato. Auth e TLS esplicitamente falliti non entrano nella grace registration.
  9. Prima di ogni reconnect cancellare e attendere interamente il vecchio scope; HTTP listener/source/sessione restano posseduti dal supervisore. Download attivi del vecchio tentativo falliscono, non si migrano. Dopo nuova registrazione verificare URL identica; se cambiata, errore e istruzione rilanciare. Stream monouso futuri mantengono stato, mai produttore riavviato.
  10. In shutdown smettere di accettare GET, cancellare body/producer, chiudere vhost e carrier, attendere task entro budget. Esportare futuro di shutdown iniettabile per test; i segnali OS sono wiring CLI. Errore di una richiesta non deve chiudere gli altri file download.
- **Unit tests:** `plain_or_malformed_endpoint_rejected_before_dial`; `ca_keeps_hostname_verification`; `all_carrier_dials_use_link_tls_config`; `https_absent_never_prints_link`; `label_is_uniform_alphabet_and_fixed_length`; `transient_reconnect_keeps_url`; `permanent_auth_error_is_not_retried`; `url_change_during_reconnect_is_fatal`.
- **e2e tests:** T-LINK-TLS — CA locale esplicita accettata, sconosciuta/hostname errato rifiutati, controllo plain e server HTTP-only rifiutati, stdout vuoto sugli errori; verificare anche carrier TCP2 e redial, non solo controllo.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK verdi; ogni connect del Link verificato TLS, niente task residui; unità chiusa in STATE §§1/4/6/11.

### 1.3 CLI file e statistiche veritiere

- **Model:** agent-2:sonnet
- **Assignment:** agent-2 implementa, agent-1 review della superficie CLI e integrazione shutdown.
- **Files:** `src/main.rs:67`, `:72`, `:1104`, `:1728`, `:1746`, `:2153`, `:3339`; `src/transfer_link_cli.rs`; `src/transfer_link/stats.rs`; `tests/transfer_link_test.rs`.
- **Change:**
  1. Aggiungere TransferCommand::Link e dispatch. Fase attuale: esattamente un file; --to con BORE_SERVER/DEFAULT_SERVER, --secret con BORE_SECRET hide_env_values, --ca-cert, --relay-only, --carriers 0..32 default1, --filename opzionale, --max-downloads 1..256 default effettivo8, --stats-interval secondi1..60 default1. Usare Option per max-downloads in vista dei default monouso, non hardcodare8 nel parser.
  2. Directory/multipli: errore chiaro prima di bind; non nascondere un'implementazione incompleta dietro un link che fallirà al primo GET. --stdin/--exec non ancora aggiunti.
  3. Esentare Link dal select di shutdown generico di main.rs:1728 come già Web, altrimenti drop improvviso salta cleanup. Riutilizzare segnali Ctrl+C/SIGTERM di :1746 forniti al supervisore. Testare il comportamento del vero processo, non soltanto cancellare un token in unit test.
  4. Log stderr: session_ready (filename, size se nota, limite, QUIC richiesto/TCP-only), transport_ready/lost/fallback/reconnect, download_started con id/path reale, progress con bytes_for_http/size/elapsed/rate/media e active count, download_completed con SHA-256/bytes/elapsed, download_failed con fase/causa e bytes parziali. Se mapping path manca, log unknown con causa; mai dedurre direct dal flag --udp.
  5. Frequenza progress = stats-interval, zero log per chunk a livello info; debug -v/-vv per canale/errori. Metriche atomiche o aggiornamenti aggregati, niente lock conteso ad ogni byte. Su non-TTY righe strutturate leggibili; niente escape terminale.
  6. Non ristampare URL in log di progress, header, debug o command args; token link e secret sensibili. Stampare una sola URL su stdout; non catturare stdout del futuro produttore nel logging.
  7. Build --no-default-features: funzione operativa via TCP TLS; messaggio chiaro UDP non compilato, --relay-only ancora valido. Non introdurre riferimenti Quinn non protetti da cfg.
- **Unit tests:** `link_cli_defaults_and_conflicts`; `filename_override_keeps_raw_bytes`; `stdout_is_only_one_url`; `progress_reports_observed_path_not_requested_path`; `no_udp_build_resolves_to_tls_relay`.
- **e2e tests:** T-LINK-RAW — file normale/vuoto/Unicode, curl con output scelto e wget leggono gli stessi byte/SHA; secondo download stesso URL riesce. T-LINK-CONCURRENT — 3 downloader indipendenti, abort di uno non interrompe altri, capacità esaurita503, slot riusabile. T-LINK-CLEANUP — SIGINT/SIGTERM su processo reale escono bounded, registry label e task/FD liberati.
- **Done:** G-FMT/G-LINT/G-UNIT/G-LINK/G-NOUDP verdi; regressione segnali Web preservata; unità chiusa in STATE §§1/4/6/11.

### 1.4 Harness HTTP pubblico e regressioni

- **Model:** agent-2:sonnet
- **Assignment:** agent-1 definisce/revisiona oracoli; agent-2 implementa harness.
- **Files:** `scripts/transfer_link_e2e.sh` NEW; `tests/transfer_link_test.rs`; `tests/vhost_test.rs:217`; `.github/workflows/ci.yml:44`; sorgenti fase corrente solo per correzioni comprovate.
- **Change:**
  1. Harness `basic`: build binario corrente `cargo build --locked --all-features`; directory mktemp, trap PID posseduti, porte dinamiche e readiness condizionata, timeout per ogni attesa. Nessun pkill globale e nessun processo daemon residuo.
  2. Generare CA/cert test con SAN localhost/control hostname e wildcard del dominio finto; usare --ca-cert su Link e --cacert curl/--ca-certificate wget. Per download usare --resolve curl; hostname ottenuto da stdout, nessun DNS pubblico reale richiesto. Non usare -k per le prove di TLS.
  3. Avviare vero bore server con vhost HTTPS/cert/UDP e porte DISTINTE per controllo TLS, vhost HTTP/HTTPS e shared QUIC. Controllo TLS usa --cert-file/--key-file, NON esiste --tls; frontend usa --vhost-cert-file/--vhost-key-file. Riutilizzare ricette `tests/vhost_test.rs:217`/README Vhost. Accertare che sia bore a terminare HTTPS. Caso cert pubblico errato: il downloader deve rifiutarlo; distinto dal cert controllo errato che impedisce la pubblicazione.
  4. Eseguire T-LINK-RAW/HTTP/TLS/CONCURRENT/CLEANUP/INCOMPLETE/MUTATION. Per mutazioni usare coordinazione test (reader barrier nel library harness o file grande e blocco destinatario), senza accettare un test che modifica il file solo dopo il completamento.
  5. Distinguere strumenti mancanti da PASS: prerequisite check con errore chiaro. Non aggiungere alla CI uno script che passa se nessun caso è stato eseguito. Testare no-default-features via target dedicato; non sovrascrivere involontariamente il binario all-features usato in altro gate.
- **Unit tests:** test parser output/statistiche senza assumere ordine di scheduling; helper readiness scade e pulisce PID su startup failure.
- **e2e tests:** T-LINK-RAW/T-LINK-HTTP/T-LINK-TLS/T-LINK-CONCURRENT/T-LINK-CLEANUP/T-LINK-COMPAT; esiti e numero casi registrati in STATE. T-LINK-INCOMPLETE deve verificare exit nonzero curl e wget, non soltanto log A.
- **Done:** G-E2E e gate Rust verdi; harness fallisce togliendo i controlli TLS o il body error; unità chiusa in STATE §§1/4/6/11.

### 1.5 Update README.md

- **Model:** agent-3:haiku
- **Assignment:** documentazione; agent-1 legge esempi e modello di fiducia.
- **Files:** `README.md:2049` Secure file transfer, `:2257` transfer web (aggiungere sezione sorella Transfer link), `:3191` Vhost config, `:938` server flags, `:1154` server Docker Compose, `:3475` access logging.
- **Change:** documentare solo file singolo già operativo: install/requisiti, DNS wildcard e certificato vhost già esistenti, frontend HTTPS, TLS controllo, UDP server e fallback, TUTTI i flag della fase/default/env, URL/stdout, curl/wget, download concorrenti, Ctrl+C, SHA256 e limiti di conferma. Spiegare server vede plaintext senza salvarlo e che URL è accesso al file; --ca-cert riguarda fiducia in CA privata, nessuna emissione certificati. Nessun file di payload nei log. Chiarire integrazione server con --ssh-gateway senza inventare una sintassi SSH per Link. Conservare lingua/struttura/tono del README; niente moduli, algoritmi, piano o roadmap.
- **Unit tests:** nessuno, documentazione.
- **e2e tests:** eseguire esempi native curl/wget con fixture TLS; comandi documentati devono corrispondere all'help reale.
- **Done:** nuovo utente pubblica un file seguendo README senza leggere sorgenti; G-E2E/gate fase verdi; unità chiusa in STATE §§1/4/6/11, Docs fase1 aggiornata.

## Phase gates

- G-FMT: `cargo fmt --all -- --check`
- G-LINT: `cargo clippy --all-features --all-targets -- -D warnings`
- G-BUILD: `cargo build --locked --all-features`
- G-UNIT: `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ --skip t_web_soak`
- G-SERIAL: `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`
- G-LINK: `cargo test --all-features --test transfer_link_test -- --test-threads=1`
- G-NOUDP: `cargo test --no-default-features --test transfer_link_test -- --test-threads=1`
- G-E2E: `bash scripts/transfer_link_e2e.sh basic`
- Regression guard: vhost TLS/redirect, control reaper, carrier/QUIC, Web shutdown e legacy scope None invariati.
- README: tutti i comportamenti/flag introdotti descritti, esempi eseguiti, nessuna promessa ZIP/stdin/exec ancora non spedita.

## Phase done criterion

URL HTTPS vero scaricabile con curl/wget, stesso file ripetibile/concorrente; TLS e fallback warm non degradano; path osservato e completamento onesti; Ctrl+C/SIGTERM liberano risorse. STATE §11 fase1 DONE, tutte le sottofasi chiuse e README aggiornato.
