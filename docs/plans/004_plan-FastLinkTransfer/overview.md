# Fast Link Transfer — Plan overview

Authored 2026-09-25 by agent-1:opus (supervisor). Folder: `docs/plans/004_plan-FastLinkTransfer/`.
Execution starts at [STATE.md](STATE.md); all live readiness and progress are there.

## Goal and reference scenario

Il server bore, con due variabili d'ambiente (più la credenziale di upload), espone un
vhost permanente `fast.<base-domain>` che fa da **relay HTTP in streaming puro**: un
client HTTP standard carica con `curl -T`, riceve subito il link di download e i byte
passano dal socket dell'uploader al socket dell'unico downloader. Nessun file su
disco, nessun client bore. Stesso concetto di `bore transfer link`, lato server.

```sh
# server
BORE_FAST_LINK_TRANSFER_ENABLED=true \
BORE_FAST_LINK_TRANSFER_VHOST=fast.bore.tld \
BORE_FAST_LINK_TRANSFER_AUTH='alice:s3cret' \
bore server --vhost-base-domain bore.tld --vhost-cert-file wild.pem --vhost-key-file wild.key ...

# A: file singolo (Content-Length) — stdout TTY: la riga 1 appare subito
curl -u alice:s3cret -T miofile.tar https://fast.bore.tld
# https://fast.bore.tld/k3v9q0x7m2a8d1zp/miofile.tar
# # waiting for the download (expires in 60 min); nothing is stored on the server
# # download started
# # done: 1073741824 bytes in 9.8 s (104.5 MiB/s)       ← curl exit 0

# A: streaming (chunked), stdout non-TTY → serve -N
sudo tar -cpf - myfolder | curl -N -u alice:s3cret -T - https://fast.bore.tld/myfolder.tar

# B: una sola volta, con qualunque client
curl -fO 'https://fast.bore.tld/k3v9q0x7m2a8d1zp/miofile.tar'   # oppure wget / browser
```

Esclusioni: più ricevitori per upload (fan-out), resume/Range, persistenza, upload da
browser (form), HTTP/2, compressione, autenticazione del download, UI admin dedicata.

## Decisions

| ID | Decision and consequence | Authority/source | Supersedes |
|----|--------------------------|------------------|------------|
| D1 | Upload protetto da HTTP Basic (`--fast-link-transfer-auth USER:PASS`, obbligatoria se abilitato; riuso `basicauth::BasicAuth`). Download libero: il link è una credenziale bearer (ID 16 char `[a-z0-9]` CSPRNG, ~82 bit). | Utente Q1 2026-09-25 | — |
| D2 | Anti-anteprima: (a) User-Agent di bot noti → 200 HTML generico, non consuma; (b) `Range` diverso da `bytes=0-` → 416, non consuma; (c) finestra replay RAM 4 MiB: un download interrotto prima che il server abbia prelevato più di 4 MiB dall'upload rimette il link in attesa. | Utente Q2 2026-09-25; R4 | — |
| D3 | Un solo ricevitore per upload; link monouso (consumato da un GET che supera la finestra o completa). | Utente Q3 | — |
| D4 | Nomi: env `BORE_FAST_LINK_TRANSFER_ENABLED`, `_VHOST`, `_AUTH`, `_WAIT_TIMEOUT`, `_MAX_ACTIVE`; flag `bore server --fast-link-transfer`, `--fast-link-transfer-vhost`, `--fast-link-transfer-auth`, `--fast-link-transfer-wait-timeout`, `--fast-link-transfer-max-active`. | Utente Q4 (grafia TRANSFER corretta) | — |
| D5 | Attesa max di un download: default 3600 s, configurabile 1..=604800. Scaduta → riga `# expired`, chiusura senza terminatore (curl exit 18), link 404. | Utente round 2 | — |
| D6 | Admin: oggetto `fast_link` in `/admin/api/v1/config` e `/admin/api/v1/metrics` (null se disabilitato) + card "Fast Link" nel pannello **Metrics**, stesso pattern della card Web Transfer. L'opzione approvata diceva "card in Overview": il supervisore sceglie Metrics per coerenza con Web Transfer; segnalato all'utente. Nessuna sezione UI con lista. | Utente round 2 + supervisore | — |
| D7 | Solo transito: nessun byte su disco; RAM per trasferimento ≤ finestra replay (4 MiB) + `PUMP_DEPTH`×`proxy_buffer_size()` (1 MiB default) + buffer di attesa. | Utente (richiesta iniziale) | — |
| D8 | Banda massima: in streaming una **pompa a due task** — R possiede l'uploader (read + decifratura TLS + framing), W il downloader (cifratura TLS + write + flush) — così le due metà crittografiche girano su core diversi; tra loro `PUMP_DEPTH = 4` buffer fissi da `proxy_buffer_size()` (256 KiB default, `BORE_PROXY_BUFFER_SIZE`) riciclati: zero allocazioni a regime (evita la soglia mmap di glibc, H-18). Backpressure end-to-end via canale limitato + TCP. Chunked in **passthrough** verbatim (validato, mai ri-codificato); nessun hash sul server; mai `tokio::io::copy` (buffer 8 KiB). Rifiutata: una task sola read→write (una sola CPU per decifrare+cifrare = tetto dimezzato). | Utente ("massime performance di banda", ribadito due volte) + supervisore | — |
| D9 | Fast host solo HTTPS: su connessione non TLS `GET`/`HEAD` → 308 verso `https://`, altri metodi → 403. All'avvio serve un percorso HTTPS (cert vhost o TLS del control port), altrimenti errore. | Supervisore (Basic in chiaro = credenziale esposta) | — |
| D10 | Risposta anticipata all'uploader: `100 Continue` se `Expect: 100-continue` (dopo auth), poi `200` `text/plain` chunked. Riga 1 = URL da solo; righe successive `# ...` di stato. Successo = chunk terminatore → curl exit 0. Fallimento/scadenza = chiusura senza terminatore → curl exit 18. | Supervisore; R1, R2 | — |
| D11 | Framing upload: `Content-Length` → CL identico al downloader, copia esatta; `Transfer-Encoding: chunked` → passthrough verbatim fino al last-chunk incluso, trailer non vuoti rifiutati; CL+TE → 400; TE diverso da `chunked` → 501; nessuno dei due → 411. | Supervisore; R3 | — |
| D12 | Path: `PUT /` → nome `upload.bin`; `PUT /<segmento>` percent-decoded + `transfer_link::validate_filename`; più segmenti o query → 400. Download `GET/HEAD /<id>[/<qualsiasi>]` (query ignorata). `GET/HEAD /` → testo d'uso. Link = `https://<Host ricevuto, lowercase>/<id>/<encode_path_segment(nome)>`. | Supervisore; R1 | — |
| D13 | Stato slot `Waiting → Streaming → (Waiting per re-arm) → Closed`. Solo il downloader fa `Waiting→Streaming` (sotto mutex), poi `try_send` del proprio stream nel canale (cap. 1) dello slot. Solo l'uploader fa `Streaming→Waiting/Closed`. Scadenza o abort dell'uploader con stato `Streaming` → ricevere l'handoff in volo, mai perderlo. | Supervisore | — |
| D14 | Risposte downloader: 404 id sconosciuto/chiuso, 409 download già in corso, 200 HTML generico per bot, 416 per Range, HEAD mai consumante (header come il GET). | Supervisore | — |
| D15 | Limiti: `max_active` 32 upload (attesa + streaming; RAM peggiore ≈ 32×4 MiB + 32×buffer), stall 600 s per read/write, head ≤ 16 KiB, riga chunk ≤ 4096 B, handoff in volo ≤ 5 s, linger close 2 s / 1 MiB. | Supervisore | — |
| D16 | Label riservato con `vhost::ReservedVhostLabel = Arc<OnceLock<String>>` posseduto da `Server`, passato a `SshGateway::new`, letto dalla registrazione nativa (prima di `serve_vhost_provider`) e SSH (prima di `resolve_route`). **Nessun campo nuovo in `VhostConfig`**: 31 struct literal e l'hot-reload lo perderebbero. | Supervisore | — |
| D17 | Routing: hook nei 3 punti d'ingresso HTTP (`vhost::handle_http`, `vhost::handle_https`, `Server::serve_control_http_after_web`) PRIMA del lookup sottodominio, match esatto sull'Host (case-insensitive, porta ignorata), indipendente dall'hot-reload di `base_domain`. Trait `ConnSecurity` (`const TLS: bool`) sul tipo di stream per sapere se la connessione è TLS senza toccare il loop di accept legacy (I-SSH1). Sul path unificato si acquisisce il permit `--max-conns` come fa il ramo vhost. | Supervisore | — |
| D18 | Log: mai l'ID completo (solo i primi 4 caratteri), mai Authorization né credenziali. Contatori in metriche. | Supervisore | — |
| D19 | `--fast-link-transfer` spento ma altre flag fast-link impostate → ignorate con un `warn!` che le elenca (kill-switch da env, mai silenzioso), non errore. | Supervisore | — |
| D20 | Errori di avvio: fast link senza vhost, host non `<label>.<base_domain>` (il wildcard esistente non lo coprirebbe), auth mancante/malformata, nessun HTTPS disponibile, host uguale all'autorità di web transfer. | Supervisore | — |
| D21 | Brute-force Basic: parità con il basic-auth vhost esistente (nessun ritardo), contatore `auth_failures_total`; risposte d'errore con linger close (evita RST che cancella il 401). | Supervisore; R3 | — |
| D22 | Test browser: Playwright **solo Chromium** con `--host-resolver-rules` e `ignoreHTTPSErrors`; Firefox/WebKit non coperti (skip motivato), il comportamento testato è header HTTP standard. | Supervisore | — |

## Open questions

Nessuna. Le domande prodotto (auth, anteprime, ricevitori, nomi, attesa, admin) sono state
risposte dall'utente il 2026-09-25; il resto è scelta tecnica del supervisore (D7–D22).

## Architecture

```
curl -T ──TLS──▶ [vhost HTTPS | control port unificato] ─┐
                                                          ├─ Host == fast host? ──▶ FastLink::serve
wget/browser ─TLS─▶ (stesso ingresso) ────────────────────┘                          │
                                                   PUT ─▶ upload task U (possiede uploader stream)
                                                   GET ─▶ download task D ─ handoff(stream) ─▶ U
U: 100/200 + link ─▶ attesa (prefill replay ≤4 MiB) ─▶ streaming uploader→downloader ─▶ esito
```

Nuovo modulo `src/fast_link/` (compilato sempre, nessuna feature): `mod.rs` (config,
`FastLink`, metriche, viste admin), `request.rs` (parse head/target, preview, ID),
`framing.rs` (`BodyFramer`), `response.rs` (byte delle risposte, linger/abort),
`pump.rs` (pompa di streaming a due task), `session.rs` (slot, upload, download, handoff). Nessun messaggio wire bore nuovo.

## Interfaces and compatibility

| Surface | Exact name/type | Default | Errors/conflicts | Compatibility |
|---------|-----------------|---------|------------------|---------------|
| CLI/env | `--fast-link-transfer` / `BORE_FAST_LINK_TRANSFER_ENABLED` bool | false | — | nuovo, additivo |
| CLI/env | `--fast-link-transfer-vhost <FQDN>` / `BORE_FAST_LINK_TRANSFER_VHOST` | nessuno | obbligatorio se abilitato; deve essere `<label>.<vhost base>` | nuovo |
| CLI/env | `--fast-link-transfer-auth <USER:PASS>` / `BORE_FAST_LINK_TRANSFER_AUTH` (valore nascosto in help) | nessuno | obbligatorio se abilitato; user e pass non vuoti | nuovo |
| CLI/env | `--fast-link-transfer-wait-timeout <SECS>` / `BORE_FAST_LINK_TRANSFER_WAIT_TIMEOUT` | 3600 | 1..=604800 | nuovo |
| CLI/env | `--fast-link-transfer-max-active <N>` / `BORE_FAST_LINK_TRANSFER_MAX_ACTIVE` | 32 | 1..=4096 | nuovo |
| HTTP | `PUT /[nome]` (Basic), `GET/HEAD /<id>[/nome]`, `GET/HEAD /` | — | vedi D10–D14 | client HTTP/1.1 standard |
| Admin API | `ConfigView.fast_link: Option<FastLinkConfigView>`, `MetricsView.fast_link: Option<FastLinkMetricsView>` | null | mai credenziali | additivo |
| Vhost | label del fast host riservato (nativo + SSH) | — | registrazione rifiutata con motivo | solo se abilitato |
| Wire bore | nessuna modifica | — | — | — |

## Phase map

| Logical phase | File | Sub-phase IDs | Depends on | Assignment |
|---------------|------|---------------|------------|------------|
| 0 — Motore fast link in-process | [phase_01.md](phase_01.md) | 0.1, 0.2, 0.3, 0.4 | none | agent-2:sonnet |
| 1 — Wiring server, CLI, admin, integrazione | [phase_02.md](phase_02.md) | 1.1, 1.2, 1.3 | P0 | agent-2:sonnet |
| 2 — Client reali, banda, browser, documentazione | [phase_03.md](phase_03.md) | 2.1, 2.2, 2.3, 2.4 | P1 | agent-2:sonnet |

## Reuse map

| Need | Path and symbol | Contract to preserve |
|------|-----------------|----------------------|
| Basic auth | `src/basicauth.rs` — `BasicAuth::parse`, `BasicAuth::authorized`, `UNAUTHORIZED` | confronto constant-time; passare SOLO i byte della head |
| Filename | `src/transfer_link/source.rs` — `validate_filename`, `content_disposition`, `encode_path_segment`, `MIME_OCTET_STREAM` (re-export `crate::transfer_link::*`) | stesse regole di `transfer link` |
| ID CSPRNG | `src/transfer_link_cli.rs:430` — `generate_link_label` (rejection sampling `<252`) | copiare l'algoritmo, non la funzione (ha prefisso) |
| Head reading | `src/vhost.rs:2008` `read_head_async`, `src/edge.rs` `read_request_head` | possono leggere oltre `\r\n\r\n`: il residuo è body |
| Routing | `src/vhost.rs:1863` `handle_http`, `:1941` `handle_https`; `src/server.rs:2216` `serve_control_http_after_web` | lookup subdomain invariato dopo l'hook |
| Max-conns | `src/server.rs:2240` permit nel ramo vhost unificato | `try_acquire_owned`, 503 su esaurimento |
| Flush discipline | `src/vhost.rs:1718` `copy_one_direction_with_shutdown`; mock `FlushGatedWriter` in `vhost.rs mod tests` | write→flush prima di ogni read |
| Reservation | `src/vhost.rs:920` rifiuto `resolve_route`; `src/sshgw.rs:1187` | stesso shape di errore |
| Admin | `src/admin_api.rs:749` `config`, `:781` `metrics`; `src/admin_ui/panels/metrics.js:254` card Web Transfer; `test/admin_ui/metrics-web-transfer.test.js` | null = disabilitato, `!= null` mai truthiness |
| Test TLS | `tests/vhost_test.rs:217` `self_signed_for`, `write_pem_files`; `scripts/transfer_link_perf.sh` CA+leaf openssl | — |

## Research and evidence

| ID | Question / required or optional | Evidence state | Affected decisions / units / tests |
|----|---------------------------------|----------------|------------------------------------|
| R1 | Come si comporta curl `-T` (path, framing, Expect, risposta anticipata, output)? Required. | CONFIRMED (local) | D10, D12, 2.1, README |
| R2 | Il server può rispondere finale prima di leggere il body e deve rispondere subito a 100-continue? Required. | CONFIRMED | D10, 0.3 |
| R3 | Regole framing HTTP/1.1 (CL+TE, chunked, chiusura). Required. | CONFIRMED | D11, D21, 0.2 |
| R4 | Come si identificano i bot di anteprima? Required per Slack, optional per gli altri. | CONFIRMED (Slack) / UNVERIFIED-optional (altri) | D2, 0.1 |
| R5 | Prior art del modello PUT→GET streaming. Optional. | CONFIRMED | architettura |

### R1 — curl `-T`
- Sources: probe locale con server Python che risponde `100` + `200` chunked prima di leggere il body.
- Applicability: curl 8.5.0 (Ubuntu 24.04, OpenSSL 3.0.13), HTTP/1.1 in chiaro; TLS non cambia la logica di trasferimento.
- Dates: eseguito 2026-09-25.
- Documented fact: nessuno (osservazione locale).
- Local check: `curl -T miofile.tar http://h` → `PUT /miofile.tar`, `Content-Length`, `Expect: 100-continue`; `curl -T - http://h` → `PUT /`, `Transfer-Encoding: chunked`, `Expect`; `-T - http://h/backup.tar` → `PUT /backup.tar`. Dopo il `200` anticipato curl CONTINUA a inviare tutto il body. Con stdout TTY (pty) o `-N`, la riga del link è stampata subito (6 ms dopo l'invio mentre il server non leggeva il body); con stdout pipe senza `-N` compare solo a fine trasferimento.
- Supervisor inference: il link va stampato come prima riga da sola; README deve dire di usare `-N` quando stdout non è un terminale.
- Decision impact: D10, D12; test E1/E2 in 2.1 usano `-N`.

### R2 — Expect / risposta anticipata
- Sources: RFC 9110 §10.1.1 (https://www.rfc-editor.org/rfc/rfc9110.html#section-10.1.1).
- Applicability: HTTP/1.1, giugno 2022; consultato 2026-09-25.
- Documented fact: su `100-continue` l'origin DEVE inviare subito `100` o un finale; un server che risponde finale prima di leggere tutto il body DOVREBBE indicare se chiuderà o continuerà a leggere.
- Decision impact: D10 (`100` poi `200` che continua a leggere, `Connection: close` alla fine); auth prima del `100` (I-7).

### R3 — Framing HTTP/1.1
- Sources: RFC 9112 §6.3, §7.1, §9.5, §9.6 (https://www.rfc-editor.org/rfc/rfc9112.html).
- Applicability: HTTP/1.1, consultato 2026-09-25.
- Documented fact: CL insieme a TE è indizio di smuggling e va trattato come errore; chunked = size hex + estensioni opzionali + CRLF, last-chunk `0`, trailer section, CRLF; implementazioni devono monitorare la chiusura; la chiusura va fatta in modo graceful.
- Supervisor inference: passthrough verbatim dei byte chunked validati è corretto verso un downloader HTTP/1.1; trailer non vuoti rifiutati (curl non li invia). Linger close dopo errori con body non letto.
- Decision impact: D11, D21, 0.2.

### R4 — Bot di anteprima
- Sources: https://api.slack.com/robots (consultato 2026-09-25).
- Documented fact: `Slackbot-LinkExpanding 1.0 (+https://api.slack.com/robots)` "fetches as little of the page as it can (using HTTP Range headers)"; anche `Slack-ImgProxy`, `Slackbot`.
- Supervisor inference: il segnale `Range` è un secondo discriminatore; gli UA degli altri servizi (Discord, Telegram, WhatsApp, Facebook, Twitter, LinkedIn, Teams/Skype, Mattermost, crawler) sono noti per convenzione ma non verificati qui — OPTIONAL perché la finestra replay (D2c) è il backstop che non dipende dalla lista.
- Decision impact: D2; lista in 0.1; limite documentato (scanner tipo Safe Links che scaricano tutto consumano il link).

### R5 — Prior art
- Sources: https://github.com/nwtgck/piping-server README (branch develop, consultato 2026-09-25).
- Documented fact: sender `curl -T - https://ppng.io/<path>`, receiver GET sullo stesso path, trasferimento in streaming senza storage.
- Decision impact: conferma fattibilità con client standard; bore genera il path (ID) lato server (D12) invece di farlo scegliere.

## Invariants

| ID | Meaning | Guarding test |
|----|---------|---------------|
| I-1 | Feature spenta ⇒ ogni percorso HTTP/vhost/control è identico a oggi (nessuna lettura, nessun hook). | suite esistenti + T-FL-I5 |
| I-2 | Nessun byte di payload su disco; RAM per trasferimento ≤ finestra replay + `PUMP_DEPTH` buffer di copia + buffer di attesa. | T-FL-S6/S7 (cap replay), T-FL-TRANSIT |
| I-3 | Un upload ha al più un download completato. Re-arm solo se TUTTI i byte prelevati dall'uploader sono ancora nella finestra replay. | T-FL-S6, T-FL-S7 |
| I-4 | curl uploader esce 0 ⟺ il downloader ha ricevuto il body intero (ultimo byte scritto e flushato). Ogni altro esito chiude senza terminatore. | T-FL-S1, S7, S8, E1, E6, E8 |
| I-5 | Un body troncato non è mai presentato come completo al downloader (CL corto o chunked senza last-chunk). | T-FL-S9, E9 |
| I-6 | Ogni scrittura su uno stream (TLS) è seguita da `flush` prima di attendere una read. | `pump_writes_are_flushed_before_waiting` (0.3) |
| I-7 | Auth verificata prima di `100 Continue`, prima di allocare slot/permit, e SOLO sui byte della head. | T-FL-S3, T-FL-S16 |
| I-8 | Lo slot è rimosso su ogni uscita (RAII) e i gauge tornano a 0. | T-FL-S* (assert `slots_len()==0`) |
| I-9 | Il fast host è servito solo su TLS. | T-FL-S11, T-FL-I3, E12 |
| I-10 | Nessun provider (nativo o SSH) può registrare il label del fast host. | T-FL-I4, T-FL-I8 |
| I-11 | Pompa a due task con buffer riciclati `proxy_buffer_size()`, passthrough chunked, nessun `tokio::io::copy`, nessun hash: banda ≥ 1.0× la baseline vhost relay sullo stesso host (un salto e un mux in meno). | T-FL-PERF |

## Risks

| Risk | Design mitigation | Verification |
|------|-------------------|--------------|
| Link invisibile se stdout di curl è una pipe | riga 1 = URL; README impone `-N` fuori da un TTY | R1, E2 |
| Anteprima chat consuma il link | UA + Range + finestra replay 4 MiB | S4, S6, E5, E7 |
| Scanner che scaricano tutto (Safe Links) | limite documentato; nessuna euristica possibile | README |
| Stall TLS per record non flushati | flush dopo ogni write (lezione vhost 36cd70d) | `pump_writes_are_flushed_before_waiting` (mock) |
| RST che cancella il 401/403 | linger close | S3, E4 |
| Over-read della head | `buffered` completo passato a `serve`, residuo = body | S1, S16 |
| Race scadenza/claim | protocollo D13 (handoff in volo ricevuto) | S12 |
| Memoria con molti upload in attesa | `max_active` + finestra fissa | S10, TRANSIT |
| Regressione del loop di accept legacy | trait `ConnSecurity` sul tipo, nessuna riga del loop cambiata | review 1.2 (`git diff`) |

## Verification strategy

G-BASE prima di 0.1. Ogni sottofase esegue i suoi gate mirati (STATE.md §3). P0 = fmt,
clippy e lib test. P1 = gate CI completo (`G-FULL`), più integrazione (`G-I1`) e npm.
P2 = `G-FULL` + e2e reali (`G-E2E`) + banda/transito (`G-PERF`) + browser (`G-PW`).
Lo scenario di riferimento è provato da E1 (file), E2 (tar streaming) e T-FL-PW (browser).

## Model-assignment summary

| Unit(s) | Implementer | Required supervisor review |
|---------|-------------|----------------------------|
| 0.1, 0.2 | agent-2:sonnet | agent-1:opus dopo il diff (auth solo head, parser) |
| 0.3 | agent-2:sonnet | agent-1:opus dopo il diff (pompa: parallelismo, riciclo buffer, cancellazione, I-6/I-11) |
| 0.4 | agent-2:sonnet | agent-1:opus dopo il diff (concorrenza, D13, I-3/I-4/I-7/I-8) |
| 1.1, 1.3 | agent-2:sonnet | agent-1:opus a P1 |
| 1.2 | agent-2:sonnet | agent-1:opus dopo il diff (I-1, I-SSH1: nessuna riga del loop di accept cambiata) |
| 2.1, 2.3, 2.4 | agent-2:sonnet | agent-1:opus a P2 |
| 2.2 | agent-2:sonnet | agent-1:opus dopo il diff (validità della misura) |
| P0, P1, P2 | agent-1:opus | — |
