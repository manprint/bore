# Phase 1 — Wiring server, CLI, admin, integrazione

Intent: `bore server` con le variabili/flag fast link serve il vhost `fast.<base>` su
tutti gli ingressi HTTPS (frontend vhost dedicato e control port unificato), riserva il
label contro registrazioni native e SSH, pubblica config e metriche admin; provato da
test d'integrazione su un server reale con TLS.
Prerequisites: P0 DONE (`src/fast_link/` completo).
Phase closure: P1; review by agent-1:opus.

## State and ownership contract
Read STATE.md §0–1 first for scope, ownership, recovery, checks, and commit rules.
Open before edits; checkpoint at a recovery boundary; close with evidence. A
delegated worker follows its assigned OPEN unit. Missing design goes to agent-1:opus.

## Local design context
Plan revision: 1.

- **D4** Flag/env: `--fast-link-transfer` (`BORE_FAST_LINK_TRANSFER_ENABLED`, bool), `--fast-link-transfer-vhost <HOST>` (`_VHOST`), `--fast-link-transfer-auth <USER:PASS>` (`_AUTH`, `hide_env_values = true`), `--fast-link-transfer-wait-timeout <SECS>` (`_WAIT_TIMEOUT`, default `fast_link::DEFAULT_WAIT_TIMEOUT_SECS`), `--fast-link-transfer-max-active <N>` (`_MAX_ACTIVE`, default `fast_link::DEFAULT_MAX_ACTIVE`). Validazione in `fast_link::resolve_server_config` (0.1), mai duplicata in clap.
- **D6** Admin: `ConfigView.fast_link: Option<FastLinkConfigView>` e `MetricsView.fast_link: Option<FastLinkMetricsView>`, `None` → JSON `null` = servizio spento. Card "Fast Link" nel pannello Metrics con lo stesso pattern della card Web Transfer (`!= null`, mai truthiness).
- **D9** Solo HTTPS: una connessione non TLS al fast host riceve 308 (GET/HEAD) o 403 (altri) da `FastLink::serve(secure=false)`.
- **D16** `vhost::ReservedVhostLabel = Arc<std::sync::OnceLock<String>>`, posseduto da `Server`, passato a `SshGateway::new`; `vhost::reserved_label_reason(label, &reserved) -> Option<String>` (case-insensitive) = `"subdomain '<label>' is reserved for the fast link transfer service"`. Nessun campo in `VhostConfig`.
- **D17** Hook: se `server.fast_link` è `Some` e `fast.matches_host(host_header)` → `fast.serve(stream, buffered, peer, secure, permit)` e return; altrimenti il codice esistente prosegue IDENTICO. `secure` viene da `ConnSecurity::TLS` (trait su tipo). Il path unificato acquisisce un permit `--max-conns` (`try_acquire_owned`; esaurito → `conn_rejections += 1` + `vhost::send_service_unavailable`).
- **D19** Spento + altre flag → `warn!` per ogni flag ignorata, nessun errore.
- **D20** `Server::set_fast_link` rifiuta: vhost assente, nessun HTTPS (`vhost_tls` vuoto e `tls` None), host uguale all'autorità web transfer (`web_transfer_http::host_matches_authority`), doppia configurazione.
- **I-1** Spento ⇒ nessuna lettura/hook in più: ogni hook è dentro `if let Some(fast) = &self.fast_link` (o parametro `Option`) valutato DOPO che la head era già stata letta dal codice esistente.
- **I-SSH1** (CLAUDE.md) Il loop di accept del control port NON cambia: `git diff` non deve mostrare righe rimosse o modificate dentro `Server::listen` tra `loop {` e la fine del blocco `tokio::spawn` del loop di accept.
- **I-9** fast host solo su TLS. **I-10** label riservato nativo+SSH.

`ConnSecurity` (NEW, `src/prefixed.rs` o `src/mux.rs` accanto a `Transport`; scegliere `src/prefixed.rs` perché `Prefixed` deve implementarlo):
```rust
/// Whether a stream type carries TLS end to end from the client. Static, per type.
pub trait ConnSecurity { const TLS: bool; }
impl ConnSecurity for tokio::net::TcpStream { const TLS: bool = false; }
impl<S> ConnSecurity for tokio_rustls::server::TlsStream<S> { const TLS: bool = true; }
impl<S: ConnSecurity> ConnSecurity for Prefixed<S> { const TLS: bool = S::TLS; }
```
Aggiungere il bound `S: ConnSecurity` a `Server::route_connection`, `route_connection_known_http`, `serve_control_http`, `serve_control_http_after_web` e implementare il trait per ogni altro tipo concreto che il compilatore segnala (per stream di test in memoria, es. `tokio::io::DuplexStream`: `false`).

## Sub-phases

### 1.1 CLI, `Server::set_fast_link`, riserva del label, API admin
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus a P1.
- **Files:** READ `src/main.rs` (`Command::Server` ~riga 591-1030, dispatch ~2591-3040: `resolve_server_config` web transfer ~2689, `set_vhost` ~2916, `set_config_view` ~3014, `set_ssh_gateway` ~3037), `src/server.rs` (`Server` struct ~400, `Server::new` ~680, `set_web_transfer` ~917, `set_ssh_gateway` ~1384, `HelloVhost` ~2455), `src/sshgw.rs` (`SshGateway` ~317, `SshGateway::new`, forward vhost ~1150), `src/admin_api.rs` (`config` ~749, `metrics` ~781/847), `src/admin_views.rs` (`ConfigView` ~447, `MetricsView` ~645). WRITE quegli stessi file + `src/vhost.rs` (tipo e helper D16). NEW nessuno.
- **Change:**
  Preconditions: P0 DONE.
  Steps:
  1. S1 — `src/vhost.rs`: `pub type ReservedVhostLabel` e `pub fn reserved_label_reason` (doc comment; unit test `reserved_label_reason_is_case_insensitive_and_empty_when_unset`). `src/server.rs`: campi `fast_link: Option<Arc<crate::fast_link::FastLink>>` e `reserved_vhost_label: crate::vhost::ReservedVhostLabel` (inizializzati in `Server::new` a `None` / `Arc::new(OnceLock::new())`); metodi `pub fn vhost_base_domain(&self) -> Option<String>` (dalla config vhost live), `pub fn set_fast_link(&mut self, config: crate::fast_link::FastLinkConfig) -> Result<()>` con i controlli D20 in quest'ordine (vhost, HTTPS, web transfer, `reserved_vhost_label.set`), poi `self.fast_link = Some(Arc::new(FastLink::new(config, Arc::clone(&self.total_rx_bytes), Arc::clone(&self.total_tx_bytes))))`; `pub fn fast_link(&self) -> Option<Arc<crate::fast_link::FastLink>>`. Messaggi d'errore esatti: `"fast link transfer requires the vhost frontend (--vhost-config or --vhost-base-domain)"`, `"fast link transfer requires HTTPS: configure --vhost-cert-file/--vhost-key-file or the control port --cert-file/--key-file"`, `"fast link transfer host '<host>' is also the web transfer origin; use a different label"`, `"fast link transfer is already configured"`. Expected: compila.
  2. S2 — riserva: in `server.rs` ramo `ClientMessage::HelloVhost`, subito dopo il `let Some(cfg) = self.vhost_config.clone() else {...}`: `if let Some(reason) = vhost::reserved_label_reason(&subdomain, &self.reserved_vhost_label) { warn!(%reason, "vhost registration rejected"); let _ = control.send(ServerMessage::Error(reason)).await; return Ok(()); }`. `SshGateway`: nuovo campo `reserved_vhost_label: crate::vhost::ReservedVhostLabel`, nuovo parametro in `SshGateway::new` (passare `Arc::clone(&self.reserved_vhost_label)` da `set_ssh_gateway`; aggiornare anche la chiamata di test in `sshgw.rs` ~4184 con `Arc::new(OnceLock::new())`); nel gestore della forward vhost, accanto al controllo `permit_allows`, PRIMA di `peek_takeover`: `if let Some(reason) = crate::vhost::reserved_label_reason(&label, &self.gateway.reserved_vhost_label) { self.state.queue_message(format!("bore ssh-gateway: {reason}")); return Ok(false); }`. Expected: compila; comportamento con label non riservato invariato.
  3. S3 — `src/main.rs`: 5 argomenti clap nella variante `Server` subito dopo gli argomenti `web_transfer_*` (doc comment in inglese di una riga ciascuno; nessun `requires`/`conflicts` clap: la validazione è in `resolve_server_config`); destrutturazione in `Command::Server { .. }`; subito dopo il blocco che chiama `server.set_vhost(cfg)?` (e quindi dopo `set_tls`, ~2802): costruire `bore_cli::fast_link::FastLinkServerArgs`, `let resolution = bore_cli::fast_link::resolve_server_config(&args, server.vhost_base_domain().as_deref())?;`, `for flag in &resolution.ignored { warn!(flag, "fast link transfer is disabled (--fast-link-transfer / BORE_FAST_LINK_TRANSFER_ENABLED=true); ignoring this setting"); }`, `if let Some(config) = resolution.config { let host = config.host.clone(); server.set_fast_link(config)?; info!(%host, "fast link transfer enabled"); }`. Deve precedere `set_ssh_gateway` e `listen`. Expected: `bore server --help` mostra le 5 flag; `BORE_FAST_LINK_TRANSFER_AUTH` non stampa il valore.
  4. S4 — admin: `admin_views.rs` aggiungere `#[serde(default)] pub fast_link: Option<crate::fast_link::FastLinkConfigView>` a `ConfigView` e `pub fast_link: Option<crate::fast_link::FastLinkMetricsView>` a `MetricsView` (doc comment: null = spento; mai credenziali); aggiungere `fast_link: None` a OGNI struct literal che il compilatore segnala (main.rs, server.rs ~715, admin_views tests, admin_api tests, tests/admin_test.rs). `admin_api::config`: in entrambi i rami cfg `view.fast_link = server.fast_link().map(|f| f.config_view());` dopo gli overlay esistenti. `admin_api::metrics`: `fast_link: server.fast_link().map(|f| f.metrics_view()),`. Expected: compila; JSON invariato a parte la chiave `fast_link`.
  Recovery boundary: dopo S2 (checkpoint §6 se si prevede un'interruzione).
  Failure handling: ordine delle chiamate in `main.rs` diverso da quanto indicato → rispettare "dopo set_vhost e set_tls, prima di set_ssh_gateway e listen"; conflitti non previsti → agent-1:opus.
- **Unit tests:** (G-U1)
  - `src/vhost.rs` `reserved_label_reason_is_case_insensitive_and_empty_when_unset`.
  - `src/server.rs` `mod tests` (o `tests/fast_link_test.rs` se `Server` interno non è costruibile nei test unitari): `set_fast_link_requires_vhost_https_and_a_distinct_web_origin` — quattro casi d'errore con messaggio esatto + caso ok.
  - `src/admin_api.rs` `config_and_metrics_publish_fast_link_only_when_enabled` — server senza fast link → `fast_link` null in entrambi; con fast link → oggetto con `host`, `wait_timeout_seconds`, `max_active`, `replay_window_bytes`; la serializzazione JSON NON contiene la password di test.
  - `src/main.rs` test degli argomenti (accanto a quelli esistenti ~5238): `server_fast_link_flags_parse_and_default` — `try_parse_from` con le 5 flag → valori; senza flag → `false`, `None`, `None`, 3600, 32.
- **e2e tests:** N/A qui — T-FL-I4/I6/I8 in 1.3.
- **Done:** gate G-U1 verdi coi test sopra; unità chiusa; commit di completamento.

### 1.2 Routing sui tre ingressi, `ConnSecurity`, permit sul path unificato
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus DOPO il diff (focus I-1 e I-SSH1: `git diff src/server.rs` senza righe cambiate nel loop di accept; nessuna lettura aggiunta quando `fast_link` è None).
- **Files:** READ `src/server.rs` (`listen` accept loop ~1965-2083, frontend vhost ~1490-1600, `route_connection` ~2099, `route_connection_known_http` ~2145, `serve_control_http` ~2187, `serve_control_http_after_web` ~2216), `src/vhost.rs` (`handle_http` ~1863, `handle_https` ~1941, `send_service_unavailable`), `src/prefixed.rs`. WRITE `src/prefixed.rs` (trait `ConnSecurity`), `src/vhost.rs`, `src/server.rs`.
- **Change:**
  Preconditions: 1.1 DONE.
  Steps:
  1. S1 — `ConnSecurity` come nel contesto; bound `S: ConnSecurity` sui 4 metodi del server; impl per ogni tipo segnalato dal compilatore. Expected: `cargo build --all-features` e `--no-default-features` ok, nessuna riga del loop di accept toccata.
  2. S2 — `vhost::handle_http` e `vhost::handle_https`: due nuovi ultimi parametri `fast_link: Option<Arc<crate::fast_link::FastLink>>` e `permit: Option<tokio::sync::OwnedSemaphorePermit>`. Prima riga del corpo: nessuna; il permit resta vivo per tutta la funzione (`let permit = permit;` e, nei rami non fast, lasciarlo cadere a fine funzione come oggi). Subito dopo `extract_host_from_head` e PRIMA di `extract_subdomain`: `if let (Some(fast), Some(h)) = (fast_link.as_ref(), host) { if fast.matches_host(h) { fast.serve(stream, head, Some(addr), <false in handle_http | true in handle_https>, permit).await; return Ok(()); } }` (in `handle_https` lo stream è `tls_stream`). Così il permit `--max-conns` segue lo stream anche quando il downloader viene consegnato alla task dell'uploader. Chiamanti in `server.rs` (task dei frontend dedicati ~1520 e ~1580): sostituire `let _permit = permit;` con il passaggio `Some(permit)` e `this3.fast_link.clone()` come argomenti (queste righe sono nei listener vhost, NON nel loop di accept del control port).
  3. S3 — `serve_control_http_after_web`: dentro `if let Some(cfg_lock) = &self.vhost_config`, subito dopo aver ottenuto `head` e PRIMA di calcolare `sub`: `if let Some(fast) = &self.fast_link { if vhost::extract_host_from_head(&head).is_some_and(|h| fast.matches_host(h)) { let permit = match Arc::clone(&self.conn_permits).try_acquire_owned() { Ok(p) => p, Err(_) => { self.conn_rejections.fetch_add(1, Relaxed); debug!("fast link connection on control port dropped: max-conns reached"); return vhost::send_service_unavailable(stream).await; } }; Arc::clone(fast).serve(stream, head, None, S::TLS, Some(permit)).await; return Ok(()); } }`. Nota: `stream` è `Prefixed<S>` → `Prefixed::<S>::TLS == S::TLS`. Il peer non è disponibile qui (il ramo vhost usa un indirizzo fittizio): passare `None`.
  Recovery boundary: none.
  Failure handling: se `FastLink::serve` richiede `self: &Arc<Self>`, chiamarlo su `&fast` (Arc) — adattare senza cambiare semantica; qualsiasi modifica al loop di accept → STOP e agent-1:opus.
- **Unit tests:** (G-U1) `src/prefixed.rs` `conn_security_is_static_per_type` (`TcpStream::TLS == false`, `TlsStream<TcpStream>::TLS == true`, `Prefixed<TlsStream<TcpStream>>::TLS == true`, `Prefixed<TcpStream>::TLS == false`).
- **e2e tests:** T-FL-I1..I3, I5, I7 in 1.3 esercitano questi hook.
- **Done:** gate G-U1 verdi; review agent-1:opus registrata con l'esito del controllo `git diff` I-SSH1; unità chiusa; commit di completamento.

### 1.3 Test d'integrazione, card admin, CI, README
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus a P1.
- **Files:** READ `tests/vhost_test.rs` (spawn server, `self_signed_for`, `write_pem_files`, `wait_port`, pattern `SERIAL_GUARD`), `tests/ssh_gateway_test.rs` (harness OpenSSH, test `t_ssh_*` che registrano `vhost/<label>`), `src/admin_ui/panels/metrics.js` (card Web Transfer ~254), `test/admin_ui/metrics-web-transfer.test.js`, `.github/workflows/ci.yml` job `test`. WRITE `src/admin_ui/panels/metrics.js`, `tests/ssh_gateway_test.rs`, `.github/workflows/ci.yml`, `README.md`. NEW `tests/fast_link_test.rs`, `test/admin_ui/metrics-fast-link.test.js`.
- **Change:**
  Preconditions: 1.1, 1.2 DONE.
  Contract del file di test `tests/fast_link_test.rs`: una `static SERIAL: tokio::sync::Mutex<()>` presa da ogni test; blocco di porte fisso 18400–18499 (ogni test le proprie, nessun `free_port`); certificato `rcgen` con SAN `*.bore.local` e `bore.local` (file `#![cfg(feature = "udp")]` perché `rcgen` è sotto `udp`, come in `vhost_test.rs`); client HTTPS di test = `tokio_rustls::TlsConnector` con root = il certificato generato, SNI `fast.bore.local`, connessione TCP a `127.0.0.1:<porta>`, richieste HTTP/1.1 scritte a mano. Helper: `spawn_fast_server(ports, topology) -> ...` con `topology ∈ {Dedicated, Unified}`: Dedicated = vhost HTTPS su porta propria (`https_port`) con cert vhost; Unified = `https_port == control_port` e TLS del control port (`set_tls`) con lo stesso cert; entrambi `set_fast_link(resolve_server_config(...))` con host `fast.bore.local`, auth `u:p`.
  Steps:
  1. S1 — `tests/fast_link_test.rs` con i test sotto.
  2. S2 — SSH: `t_ssh_fast_link_label_is_reserved` in `tests/ssh_gateway_test.rs` (server con fast link su `fast.<base del test>`; `ssh -R vhost/fast:80:localhost:<porta>` deve ricevere il messaggio `reserved for the fast link transfer service` e nessuna entry `fast` nel registry vhost; un secondo label `other` resta registrabile). Seguire esattamente la struttura e le utility dei `t_ssh_*` esistenti (girano nel passo seriale della CI).
  3. S3 — `metrics.js`: card "Fast Link" dopo la card Web Transfer, visibile solo se `data.fast_link !== undefined && data.fast_link !== null`; righe: Waiting, Streaming, Uploads, Completed, Failed, Expired, Re-armed, Previews Blocked, Auth Failures, Rejected (Busy), Bytes (con `fmtBytes`); stesso helper di riga, `escapeHtml`, test `!= null` per ogni valore. `test/admin_ui/metrics-fast-link.test.js` ricalca `metrics-web-transfer.test.js`: `fast_link: null` → nessuna card; oggetto con zeri → card con "0" visibili (P-11).
  4. S4 — CI: nel job `test` di `.github/workflows/ci.yml` aggiungere dopo il passo cargo test parallelo `- name: Admin UI unit tests` / `run: npm test` (Node preinstallato su `ubuntu-latest`, `package.json` radice senza dipendenze). `fast_link_test` resta nel passo parallelo (serializzato internamente da `SERIAL`).
  5. S5 — README: nuova sezione "Fast link transfer (`curl -T` → one-shot link)" nella parte server/transfer, con: cosa fa (streaming puro, nulla su disco, un download); abilitazione con la tabella env/flag D4 e un esempio `docker run -e ...`; requisiti (vhost base domain + certificato wildcard già esistente, HTTPS obbligatorio); esempi `curl -u USER:PASS -T file https://fast.<base>`, `sudo tar -cpf - myfolder | curl -N -u USER:PASS -T - https://fast.<base>/myfolder.tar`, download `curl -fO`/`wget`/browser e ripristino `sudo tar --numeric-owner --same-owner -xpf myfolder.tar`; semantica (link monouso, HEAD innocuo, anteprime chat, finestra 4 MiB, scadenza, codici di uscita curl 0/18, `-N` quando stdout non è un terminale); limiti e sicurezza (link = credenziale, Basic su upload, label riservato, log senza ID); metriche admin. La sezione sarà completata in 2.4 con le evidenze e2e.
  Recovery boundary: dopo S1.
  Failure handling: porte occupate da test paralleli di altri binari → le porte del blocco sono uniche nel repo (verificare con `grep -rn "184[0-9][0-9]" tests`); flakiness → agent-1:opus, mai `sleep` lunghi.
- **Unit tests:** `test/admin_ui/metrics-fast-link.test.js` (G-NPM).
- **e2e tests:** `tests/fast_link_test.rs` (G-I1), ciascuno verifica anche `fast.slots_len() == 0` a fine scenario:
  - T-FL-I1 `dedicated_https_frontend_streams_a_cl_upload` — PUT CL 8 MiB, link letto dalla risposta, GET dal link, SHA-256 identico, risposta uploader con `# done:` e terminatore.
  - T-FL-I2 `unified_control_port_streams_a_chunked_upload` — topologia Unified, PUT chunked, download chunked identico.
  - T-FL-I3 `plain_http_frontend_refuses_uploads_and_redirects_downloads` — frontend vhost HTTP: PUT → 403, GET `/x` → 308 `https://`.
  - T-FL-I4 `native_vhost_registration_of_the_fast_label_is_rejected` — `bore vhost` nativo (API client come in `vhost_test.rs`) con sottodominio `fast` → errore contenente `reserved for the fast link transfer service`; sottodominio `app` → ok.
  - T-FL-I5 `a_disabled_server_routes_the_fast_host_like_any_vhost` — server senza fast link: GET `https://fast.bore.local/...` → 502 (nessun provider), comportamento pre-feature (I-1).
  - T-FL-I6 `admin_reports_fast_link_config_and_metrics_without_secrets` — con admin token: `/admin/api/v1/config` → `fast_link.host == "fast.bore.local"`; `/admin/api/v1/metrics` dopo un trasferimento → `completed_total == 1`, `bytes_total` corretto; nessuna delle due risposte contiene `u:p` né la sua base64.
  - T-FL-I7 `unified_path_honours_max_conns` — `set_max_conns(1)`, Unified: una connessione fast link aperta (upload in attesa), una seconda → 503.
  - T-FL-I8 = `t_ssh_fast_link_label_is_reserved` (S2).
- **Done:** G-I1, G-NPM, test SSH verdi; README aggiornato; CI modificata; unità chiusa; commit di completamento.

## Phase gates and closure
- Required gates: G-FMT, G-CLIPPY, G-FULL (tutti i passi del job CI `test`, incluso il passo seriale SSH e `npm test`), G-NODEF, G-I1, G-NPM. Assertions: zero fallimenti; `fast_link_test` scopre ≥ 7 test; `t_ssh_fast_link_label_is_reserved` eseguito.
- README obligation: sezione "Fast link transfer" creata in 1.3 S5 e verificata contro il comportamento effettivo (flag, default, esempi) a P1.
- Aprire P1: gate completi, review agent-1:opus (I-1, I-SSH1, I-9, I-10, D20), fase DONE, commit di chiusura.
