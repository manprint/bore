# Phase 6 — Hardening, osservabilità, compatibilità e rilascio

> **Intent:** dimostrare sotto carico e input ostile che il servizio finale resta limitato, privato, compatibile e distribuibile.
> **Shippable alone?** yes — porta la funzione completa alla qualità di rilascio richiesta dal repository.
> **Preconditions:** Phase 5 DONE

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`, finish or revert that unit first (§6 says how far it got). Run the gate commands in STATE.md **§3** and check the result against what §1, §7, and §11 claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type: sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to `claimed — nothing written yet`. Write or update the listed tests first or alongside production edits; do not defer them to a later unit.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1 at the next unit with `Status: none`, bump the timestamp. When STATE.md §3 has WIP commits on, commit the closed sub-phase and put its sha in the §4 row. A sub-phase is not done until this is written.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is half-finished into §6 before stopping — plus a `wip(<N.Y>)` commit when WIP commits are on.

---

## Fixed contracts for this phase

- Questa fase non cambia il flusso o i default pubblici già consegnati. Corregge soltanto difetti dimostrati dai gate, aggiunge osservabilità aggregata e integra deploy/CI.
- Nessuna metrica, log o admin view contiene nome peer scelto dall'utente, label offerta, path, filename, manifest, SDP, ICE candidate, token, chiave, ticket, digest o payload marker.
- I valori `max_*` e rate configurati sono config immutabile. `current`, `active`, `available` e contatori cumulativi sono metriche separate; zero disponibile deve restare visibile come zero.
- Budget file descriptor web: `max_rooms + max_peers + 2*max_relays`, somma checked/saturating soltanto al confine syscall; si aggiunge al `max_conns` esistente prima di `fdlimit::reconcile_fd_limit`, che continua ad aggiungere il proprio headroom 256.
- Handshake HTTP→WebSocket non autenticati: semaforo globale 256, timeout complessivo 10 secondi, request head 8 KiB. Il permit si rilascia dopo auth/attach o errore.
- Target browser: release correnti Chrome, Edge, Firefox, Safari. CI obbligatoria usa Playwright Chromium, Firefox e WebKit; Chrome/Edge branded sono smoke schedulati quando installati; Safari reale è una checklist release su macOS, perché WebKit Playwright è un'approssimazione esplicitamente documentata.
- Tutti i test native SSH/netns restano seriali. Nessuna nuova socket UDP compare nel processo server per web transfer.

## Sub-phases

### 6.1 Auditare limiti, memoria, file descriptor e fairness

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** applicare una matrice di risorse prescritta a ogni ingresso/uscita, aggiungere soak e correggere leak/starvation; fare self-review quantitativa dei budget.
- **Files:** `src/web_transfer.rs:new admission, semaphores, token buckets and counters`; `src/web_transfer_http.rs:new pending handshake guard and timeouts`; `src/main.rs:server startup before listeners — fd budget`; `src/fdlimit.rs:fd_budget/reconcile_fd_limit`; `tests/web_transfer_test.rs:new limits/load/fairness groups`; `scripts/web_transfer_e2e.sh:new soak mode`; `docs/transfer/WEB_TRANSFER_PROTOCOL.md:Resource limits`.
- **Change:** aggiungere semaforo pending handshake 256 prima di upgrade; rifiuto `503` generico, timeout 10 s e rilascio RAII. Rendere esatto il pre-auth IP limiter: LRU massima 8192 chiavi, idle TTL 10 minuti, 10 tentativi/minuto burst 20; oltre 8192 nuove IP condividono un overflow bucket con gli stessi parametri, evitando crescita map. Verificare che ogni allocazione attacker-controlled sia preceduta da limite: URL/head, JSON/frame, stringhe, peer, offer, entry, metadata, request cache, candidate, SDP, events, terminal cache, partial ranges, relay/frame e worker concurrency. Calcolare al bootstrap `web_fd = max_rooms + max_peers + 2*max_relays` con saturazione esplicita e passare `max_conns + web_fd` alla logica fdlimit; preservare widen/narrow portabili e warning rimedi P-12. Non modificare SO_RCVBUF/SO_SNDBUF TCP. Fairness relay: token bucket per room già fissato, round-robin non richiesto perché ogni relay ha task indipendente; verificare che una room throttled non trattenga lock globale e non rallenti altre room. Soak esatto: 32 peer control, 64 offerte piccole per peer, 32 relay concorrenti per 5 minuti, metà cancellati/ripresi; RSS server <= baseline + 32 MiB + 2 MiB per relay + 1 MiB per peer e nessuna crescita monotona negli ultimi 3 campioni; a chiusura room contatori/permit tornano a baseline entro 10 s. Un payload da 4 GiB sintetico sul relay counting harness deve mostrare memoria indipendente dalla dimensione senza materializzare 4 GiB. Preservare I-WEB8 e P-12.
- **Unit tests:** `web_fd_budget_adds_rooms_peers_and_two_relay_sockets_without_wrap`; `fd_reconcile_existing_behavior_is_unchanged_when_web_disabled`; `pending_handshake_semaphore_and_timeout_release_exactly_once`; `preauth_lru_caps_at_8192_and_uses_overflow_bucket`; `every_protocol_length_is_checked_before_allocation`; `throttled_room_does_not_hold_registry_or_other_room_lock`; `all_guards_release_on_panic_free_error_paths`.
- **e2e tests:** `T-WEB-SOAK` — carico esatto e bound RSS/counter sopra; `T-WEB-FDBUDGET` — server reale sotto soft/hard rlimit scelti alza soft quando possibile e avverte quando hard insufficiente; `T-WEB-FAIRNESS` — room rate-limited non riduce throughput/cadenza controllo di seconda room; `T-WEB-NOSTORE` resta verde con 64 MiB.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + T-WEB-SOAK/FDBUDGET/FAIRNESS/NOSTORE passano + profiler/contatori non mostrano crescita non limitata + self-review elenca formula e massimo di ogni collection/channel/buffer/task + closed in `STATE.md` (§1 → 6.2, §4 ledger row, §6 `none`, §11 board).

**Esito 6.1 (2026-09-17, `agent-1:Claude-Opus-5`).** La superficie web ora dichiara le
proprie risorse e le prova su un processo vero. Quattro cose implementate e quattro
imparate.

**Implementato.**

1. **Ammissione dell'handshake.** `WEB_TRANSFER_PENDING_HANDSHAKES = 256` è un semaforo
   sul REGISTRY, preso in cima a entrambi i rami di upgrade (`ControlWs` e `RelayWs`) e
   rilasciato appena l'accept finisce: limita l'handshake e mai la sessione che ne segue.
   Un rifiuto è un `503` generico che non nomina la room — un 503 che dicesse «room
   sconosciuta» sarebbe un oracolo di esistenza per chi tira a indovinare gli URL. L'accept
   è avvolto in `WEB_TRANSFER_HANDSHAKE_TIMEOUT` (10 s), quindi un client che apre la
   connessione e non parla occupa uno slot per dieci secondi e non per sempre.
2. **Budget di file descriptor.** `fdlimit::web_transfer_fds(max_rooms, max_peers,
   max_relays) = rooms + peers + 2*relays` (2 per relay: la coppia è due socket) sommato
   con saturazione a `--max-conns` da `conn_bound_with_web` prima di
   `reconcile_fd_limit`, che continua ad aggiungere il proprio headroom di 256. Il
   messaggio di rimedio di P-12 diceva «lower --max-conns to about N» su un numero che
   l'operatore non ha mai scritto: con la superficie web inclusa, `--max-conns 64` veniva
   riportato come 5696. `reconcile_fd_limit_with_web` nomina ora le due quote separate e
   i flag che abbassano quella web. `web` assente ⇒ percorso storico identico.
3. **Limiter pre-auth esatto.** LRU di 8192 IP, TTL di inattività 10 minuti, 10/minuto con
   burst 20, e oltre le 8192 chiavi un bucket di overflow condiviso con gli stessi
   parametri: la mappa non cresce e nessuna IP nuova entra gratis.
4. **Tripwire di allocazione.** `every_protocol_length_is_checked_before_allocation` legge
   i tre file della superficie con `include_str!` e rifiuta ogni `with_capacity` il cui
   argomento non sia clampato (`.min(`), derivato da qualcosa già in memoria (`.len()`) o
   letterale. L'ago è composto da due pezzi (`concat!("with_", "capacity(")`) apposta:
   scritto per intero comparirebbe nel file e il test segnalerebbe la propria riga.

**Gate nuovi.** `T-WEB-FDBUDGET` (`t_web_fdbudget`) avvia il binario vero sotto una coppia
`ulimit` scelta e rilegge `/proc/<pid>/limits` — il log proverebbe soltanto che il server
ne ha PARLATO (P-12) — in tre bracci: limite hard capiente ⇒ soft esattamente
`64 + 5632 + 256 = 5952`; limite hard corto (2048) ⇒ soft alzato al soffitto e advisory che
nomina la quota web e i flag; nessuna superficie web ⇒ `64 + 256 = 320`, cioè zero
regressione. `T-WEB-FAIRNESS` (`t_web_fairness`) fa relayare due room insieme a 2 MiB/s con
12 MiB ciascuna: pagare solo per sé costa `(12-4)/2 = 4 s`, condividere un bucket ne
costerebbe `(24-4)/2 = 10 s`. Misurato: 4,01 s e 4,01 s, RTT di controllo peggiore 1 ms.
Red-check: la stessa room con 24 MiB legge 10,01 s e fa fallire la soglia. `T-WEB-SOAK`
(`t_web_soak`) è il carico esatto del piano — 32 peer di controllo, 64 offerte ciascuno
(2048), 32 relay concorrenti, metà annullati e ripresi a metà finestra — contro un processo
vero di cui si legge la RSS dal kernel.

**Imparato.**

1. **Un soak ha senso solo se il server è della misura del carico.** Ai limiti di default
   il finale del test («dopo la chiusura tutto torna») non proverebbe niente: ci sarebbero
   migliaia di permit liberi in cui nascondere una perdita. Il server del gate parte con
   `--web-transfer-max-peers 32 --web-transfer-max-relays 32 --web-transfer-max-rooms 2`,
   esattamente il carico, quindi la room fresca che ammette di nuovo 32 peer e una coppia
   relay può riuscire SOLO se ogni permit è tornato. Per lo stesso motivo l'annullo e la
   ripresa di metà dei trasferimenti sono un'asserzione e non un contorno: con 32 permit
   relay in tutto, un permit non restituito dall'annullo fa fallire l'attach della ripresa.
2. **Il limiter pre-auth è per IP, e un test di carico è molte IP.** 32 peer da
   127.0.0.1 esauriscono il burst di 20: è il prodotto che si comporta bene. Ogni peer del
   soak parte quindi dal proprio indirizzo di 127.0.0.0/8, che è anche il modo in cui 32
   browser veri arrivano.
3. **Un peer «lasciato cadere» non chiude il socket se la metà in lettura vive in un
   task.** La prima versione di `PumpedPeer` teneva lo stream in un task e il sink nel
   test: rilasciare il peer non chiudeva niente, il server continuava a contarlo e il
   finale leggeva «permit perso» — un difetto dell'harness travestito da difetto di
   prodotto. Ora `Drop for PumpedPeer` fa `abort()` sul task.
4. **La coda di uscita per peer è 64 messaggi.** Un soak che pubblica 2048 offerte fa
   arrivare a ogni peer ~2000 eventi: un client che legge solo ciò che aspetta riempie
   quella coda e viene scartato per lentezza. `PumpedPeer` legge tutto e RICORDA solo i
   tipi che un'asserzione guarda (un `error` non è mai scartato: un rifiuto sotto carico È
   il risultato).

**Self-review — formula e massimo di ogni risorsa.**

| Risorsa | Formula | Massimo (default) |
| --- | --- | --- |
| `rooms` (DashMap) | ≤ `max_rooms`, permit RAII sulla room | 1024 |
| `peers` per room (HashMap) | ≤ `max_peers_per_room`, e la somma ≤ `max_peers_global` | 32 / 4096 |
| `offers` per peer | ≤ `max_offers_per_peer`, manifest ≤ 256 KiB ciascuno | 64 |
| `entries` per offerta | ≤ `max_entries_per_offer` | 10000 |
| metadata per room / totale | contatori esatti, rifiuto oltre il cap | 16 MiB / 256 MiB |
| `transfers` per peer | ≤ `max_transfers_per_peer` | 8 |
| coppie relay | semaforo globale `max_relays_global`, 2 socket ciascuna | 256 |
| handshake in volo | semaforo `WEB_TRANSFER_PENDING_HANDSHAKES`, timeout 10 s | 256 |
| coda uscita per peer | `mpsc::channel(WEB_TRANSFER_OUTGOING_CAP)` | 64 messaggi |
| eventi per room | `broadcast::channel(256)`, il lag risincronizza | 256 |
| cache richieste per peer | `WEB_TRANSFER_REQUEST_CACHE_CAP`, TTL 5 min | 256 |
| limiter pre-auth | LRU `WEB_TRANSFER_PRE_AUTH_MAX_IPS` + overflow, TTL 10 min | 8192 IP |
| frame relay | `WEB_TRANSFER_RELAY_MAX_FRAME_LEN` letto a uno a uno, mai bufferizzato | 32 KiB |
| messaggio di controllo | `WEB_TRANSFER_MAX_CONTROL_BYTES` | 320 KiB |
| SDP / candidati ICE | `MAX_SDP_BYTES`, `MAX_ICE_CANDIDATE_BYTES` x `MAX_ICE_CANDIDATES_PER_SIDE` | 64 KiB / 4 KiB x 128 |
| task spawnati | uno per attempt diretto (deadline, si estingue), uno per attempt relay in coda, uno per room staccata (grace) | ≤ transfers + rooms |
| descrittori | `max_conns + rooms + peers + 2*relays + 256` | riconciliato all'avvio |

**Memoria indipendente dalla dimensione del payload (il requisito dei 4 GiB).** Misurato
sullo stesso gate con due finestre: 15 s muovono **740,8 MiB** con RSS 38,1 → 57,1 MiB;
90 s ne muovono **4510,8 MiB** (4,4 GiB) con RSS 37,8 → **56,7 MiB**. Sei volte i byte,
la stessa RSS di picco: il relay inoltra un frame alla volta (≤ 32 KiB) e non materializza
mai il payload. Nello stesso run, 66080 eventi di catalogo consegnati e scartati dai peer,
descrittori del server 11 → 43 (2 per coppia relay + i listener), crescita nelle ultime tre
campionature sotto la soglia di 8 MiB.

**Gate eseguiti.** `cargo fmt` 0, `cargo clippy --all-features --all-targets -D warnings` 0,
Rust lib 798/0/2 (+7), suite e2e Rust seriale 26 passed / 1 ignored (166 s), stage
`resources` di `scripts/web_transfer_e2e.sh` con la finestra da 300 s del piano.

### 6.2 Pubblicare config e metriche aggregate senza dati sensibili

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiungere view/API/pannello admin con nomi e semantica fissati; fare self-review privacy e config-versus-gauge.
- **Files:** `src/admin_views.rs:447 — ConfigView`; `src/admin_views.rs:596 — MetricsView`; `src/admin_api.rs:855 — metrics response`; `src/admin_ui/panels/metrics.js:206 — metrics rows`; `src/web_transfer.rs:new atomic counter accessors`; `tests/admin_test.rs:config/metrics serialization groups`; `scripts/admin_dashboard_test.sh:Web Transfer metrics UI group`; `tests/web_transfer_test.rs:new privacy/metrics group`; `README.md:admin section` only in final README subphase, not here.
- **Change:** appendere campi serde additive con `#[serde(default)]`. Config names esatti: `web_transfer_enabled: bool`, e quando enabled `web_transfer_base_origin`, `web_transfer_max_rooms`, `web_transfer_max_peers`, `web_transfer_max_peers_per_room`, `web_transfer_max_offers_per_peer`, `web_transfer_max_entries_per_offer`, `web_transfer_max_offer_bytes`, `web_transfer_max_metadata_per_room`, `web_transfer_max_metadata_total`, `web_transfer_max_transfers_per_peer`, `web_transfer_max_relays`, `web_transfer_relay_rate_bytes_per_second`, `web_transfer_owner_grace_seconds`, `web_transfer_stun_count`; gli optional sono null quando disabled. Non pubblicare lista STUN. Metric names esatti e optional quando disabled: `web_transfer_rooms_current`, `web_transfer_peers_current`, `web_transfer_offers_current`, `web_transfer_metadata_bytes_current`, `web_transfer_transfers_active`, `web_transfer_relays_active`, `web_transfer_relay_slots_available`, `web_transfer_relay_ciphertext_bytes_total`, `web_transfer_direct_commits_total`, `web_transfer_relay_commits_total`, `web_transfer_completed_total`, `web_transfer_cancelled_total`, `web_transfer_rejected_total`. Usare atomiche saturating per telemetria senza influenzare il data path; decrementi current devono essere exact e debug-assert nonnegative. Il server registra commit direct/relay soltanto dal primo verified progress recipient, non dalla negoziazione. Il pannello mostra sezione Web Transfer soltanto enabled, usa controllo null esplicito per available così 0 è visibile/allarme, formatta bytes/rate con helper esistenti e non crea righe per peer/offer. Log strutturati ammessi: evento allow/deny/open/close, outer peer IP già previsto dalla policy, RoomId/PeerId/OfferId/TransferId opachi, owner class, path, counts/bytes/duration, error code. Vietati tutti i dati elencati nel contratto fase. Applicare sampling logaritmico alle ripetute auth failure dallo stesso IP, riusando pattern esistente. Preservare P-11.
- **Unit tests:** `web_config_reports_totals_and_null_when_disabled`; `web_config_totals_do_not_move_under_load`; `web_metrics_report_live_zero_as_zero_not_null`; `web_metrics_counters_follow_guard_lifecycle_exactly`; `path_counter_changes_only_after_recipient_verified_report`; `admin_json_never_contains_canary_names_paths_tokens_sdp_candidates_or_manifest`; frontend `web_metrics_panel_preserves_zero_available_and_hides_when_disabled`; serialization fixture prova additive defaults.
- **e2e tests:** `T-WEB-ADMIN` — sotto direct, relay, saturation, cancel e cleanup leggere config/metrics reali e verificare totali stabili/gauge mobili/zero visibile; `T-WEB-LOG-PRIVACY` — usare canary in ogni campo vietato e provarne assenza da log/admin, mantenendo ID/conteggi utili.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + T-WEB-ADMIN e T-WEB-LOG-PRIVACY passano + vecchie fixture admin decodificano con default + self-review privacy campo-per-campo e distinzione total/current/available + closed in `STATE.md` (§1 → 6.3, §4 ledger row, §6 `none`, §11 board).

**Esito 6.2 (2026-09-17, `agent-1:Claude-Opus-5`).** La superficie admin risponde ora alle
due domande di un operatore — «cosa ho configurato?» e «cosa sta succedendo adesso?» —
senza mai rispondere alla terza, «chi sta trasferendo cosa», che il contratto vieta.

**Implementato.**

1. **Tredici totali configurati e tredici valori vivi, mai confusi.** `ConfigView` porta
   `web_transfer_enabled` più i tredici totali (nomi esatti del piano) e `MetricsView` i
   tredici gauge/totali. I totali vengono catturati da `Server::set_web_transfer` PRIMA
   che la config venga spostata nel registry (`registry_limits` + `stun_count`), quindi
   sono uno snapshot dell'avvio e non possono diventare un gauge: è esattamente il difetto
   P-11, dove `/config` pubblicava `Semaphore::available_permits()` e leggeva 0 su un
   server saturo, cioè la stessa cosa che «non configurato».
2. **Contatori che seguono la guardia, non l'intenzione.** `offers_current` si muove
   accanto a `state.offers.insert/remove` e nella pulizia del peer che esce;
   `relay_bytes_total` accanto a `stats.bytes` dentro `run_relay_pair`;
   `completed_total`/`cancelled_total` in `terminate_locked` (il terminale `Failed` resta
   deliberatamente non contato: non è un esito dell'utente). `rejected_total` passa da
   `refused()`, un unico punto cablato sui dieci siti di capacità più `try_acquire_handshake`
   e `check_pre_auth` — un rifiuto contato in dieci posti diverse è un rifiuto contato male.
3. **La lista STUN non esce, il suo numero sì.** `web_transfer_stun_count` è un conteggio
   per scelta: la lista dei server è un'impronta del deployment, il numero è ciò che serve
   per sapere se ICE ha dove andare. Il gate lo verifica cercando `stun:` nel JSON.
4. **Sezione Web Transfer nel pannello metriche.** Compare solo quando
   `web_transfer_rooms_current` non è `undefined` né `null`, e ogni riga usa un controllo
   null esplicito (`webRow`) e mai la verità booleana: `0` slot relay liberi è il valore
   ALLARMANTE e un guard per truthiness nasconde proprio quello (la lezione di P-11 lato
   frontend, già pagata con `udp_direct_slots_available`).
5. **Sampling logaritmico dei rifiuti pre-auth.** `AuthFailureSampler` conta i fallimenti
   per IP nello stesso bound del limiter (8192 IP, TTL 10 minuti) e riporta l'1°, il 2°,
   il 4°, l'8°… La riga porta IP, conteggio e una classe grossolana; non porta la room né
   il token, perché il rifiuto è costruito apposta per rendere indistinguibili room
   assente, token malformato e token sbagliato, e una riga di log non è un'eccezione a
   quella proprietà. Le due alternative sono entrambe difetti: una riga per rifiuto rende
   uno scanner un attacco al disco dell'operatore, zero righe nasconde un indirizzo che
   fallisce diecimila volte.

**Imparato.**

- **Un gate sulla privacy deve provare anche la metà utile.** La prima versione di
  `T-WEB-LOG-PRIVACY` provava solo l'assenza dei canary e sarebbe passata su un server che
  non logga NIENTE — cioè sul peggior server possibile per un operatore. Il gate ora guida
  anche un braccio relay e pretende che la riga di chiusura porti transfer id, byte e
  frame: è una garanzia di privacy, non di silenzio.
- **Il percorso diretto non logga: l'unico log del web transfer è la chiusura del relay.**
  La prima stesura asseriva che il log nominasse la room dopo un flusso diretto e falliva
  legittimamente. Il server non scrive nulla lungo la segnalazione diretta, ed è corretto
  così (i frame che passa sono SDP e candidati, cioè proprio ciò che non va scritto).
- **`tracing_subscriber` colora i nomi dei campi.** Senza `.with_ansi(false)` la stringa
  `bytes=` non compare mai letteralmente e un'asserzione su di essa fallisce per un motivo
  che non c'entra con ciò che viene loggato.
- **Il ciclo `while let Ok(permit) = try_acquire...` conta già un rifiuto** — quello che lo
  fa uscire. Una baseline presa prima del ciclo produce un off-by-one che sembra un bug del
  contatore e non lo è.

**Self-review privacy, campo per campo.** Ogni campo pubblicato è un conteggio, un byte
count, un totale configurato o un booleano; nessuno è una stringa di origine utente.
L'unica stringa è `web_transfer_base_origin`, che è un parametro dell'operatore
(`--web-transfer-base-url`) e non un dato di un utente. Nomi peer, label/percorsi/manifest
delle offerte, token, MAC, SDP e candidati ICE sono provati assenti da `/config`,
`/metrics`, `/admin/status/data` e dai log (canary distinti per campo, così un fallimento
NOMINA il campo che perde).

**Total vs current vs available.** `max_*` = totale configurato, immobile sotto carico
(gate `web_config_totals_do_not_move_under_load`, che satura rooms e relay e riconfronta);
`*_current`/`*_active` = gauge, tornano a zero a room chiusa; `*_total` = cumulativi, non
tornano mai (asserito esplicitamente dopo la pulizia in `t_web_admin`);
`relay_slots_available` = l'unico «quanto ne resta», ed è l'unico il cui ZERO è la notizia.

**Test aggiunti.** Rust unit: `web_metrics_counters_follow_guard_lifecycle_exactly`,
`repeated_pre_auth_failures_are_logged_logarithmically`. Rust e2e:
`web_config_reports_totals_and_null_when_disabled`, `web_config_totals_do_not_move_under_load`,
`web_metrics_report_live_zero_as_zero_not_null`,
`admin_json_never_contains_canary_names_paths_tokens_sdp_candidates_or_manifest`,
`admin_web_transfer_fields_are_additive_and_match_the_fixture` (fixture
`tests/fixtures/web_transfer/v1/admin-fields.json`), `web_pre_auth_failures_are_sampled_in_the_log`,
`t_web_admin` (T-WEB-ADMIN), `t_web_log_privacy` (T-WEB-LOG-PRIVACY). Frontend:
`test/admin_ui/metrics-web-transfer.test.js` (4 casi). Harness: gruppo `T-WEBUI-E2E` in
`scripts/admin_dashboard_test.sh`, che ora avvia il server di riferimento con
`--web-transfer-base-url` e verifica totali configurati, assenza della lista STUN, gauge a
zero VISIBILE, `/transfer/` servito sull'origine annunciata e la sezione presente nel
bundle servito.

**Red-check.** `t_web_log_privacy` è stato reso rosso aggiungendo un `debug!` che logga il
nome del peer: fallisce con «the peer display name reached the server log:
CANARY-NAME-pangolin». Il pannello frontend era già stato red-checked in 6.2 sostituendo il
controllo null con `if (!value) return;` (2 test su 4 falliscono, quelli sullo zero).

### 6.3 Eseguire hardening protocollo, sicurezza browser e compatibilità legacy

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiungere test malformed/fuzz/security per tutti i trust boundary e correggere soltanto i difetti dimostrati; fare self-review degli invarianti storici.
- **Files:** `src/web_transfer_protocol.rs:new bounded decoders`; `src/web_transfer_http.rs:new origin/host/CSP/error hardening`; `src/web_transfer.rs:new authorization/race fixes`; `tests/web_transfer_test.rs:new malformed/security/compat groups`; `web/transfer/tests/e2e/security.spec.mjs:new`; `web/transfer/tests/unit/*.test.mjs:new security cases`; `tests/local_proxy_hardening_test.rs:public control/direct regressions`; `tests/vhost_test.rs:control-liveness group`; `tests/ssh_gateway_test.rs:existing SSH gateway regressions`; `tests/transfer_test.rs:existing native transfer regressions`; `docs/transfer/WEB_TRANSFER_PROTOCOL.md:Security and compatibility`.
- **Change:** creare corpus deterministico e property tests per request head, path, hello/control JSON, manifest, resume ranges, SDP/candidate envelope, relay attach e binary frame. Ogni input truncated/oversize/unknown/duplicate field, invalid Unicode/number/hex, integer boundary e reordered lifecycle deve finire in errore stabile senza panic, allocation oltre cap o stato parziale. Verificare CSRF/cross-origin: Host/Origin/subprotocol mismatch, null Origin, scheme/port mismatch e DNS suffix trick falliscono; CSP finale usa `connect-src 'self'` soltanto, nessun inline/eval/blob worker, e funziona nei tre motori. Verificare XSS con nomi/path contenenti markup e bidi/control: control è rifiutato dove vietato, testo ammesso resta text node. Verificare fixation/replay dei token, ticket e requestId; timing tests controllano stessa classe risposta auth, senza pretendere costanza di rete. Verificare crypto: nonce mai ripetuto nello stesso key, wrong key/AAD/tag, stale attempt e replay non scrivono OPFS. Aggiungere una scansione delle socket server prima/dopo web transfer: nessuna socket UDP nuova rispetto allo stesso server config; native public/vhost/ssh condividono ancora l'unico endpoint previsto. Pin delle vecchie varianti serde e dei comandi `transfer listener|sender`. Eseguire esplicitamente tutti i gate liveness P-4/P-7/P-9/P-14, secret carrier/path report, vhost e SSH jump citati in `AGENTS.md`; non cambiare i loro timeout/registries. Aggiornare il protocollo soltanto per chiarimenti osservati, senza cambiare v1 silenziosamente.
- **Unit tests:** `all_web_decoders_are_panic_free_and_allocation_bounded`; `duplicate_json_keys_are_rejected`; `origin_host_and_subprotocol_matrix_is_closed`; `csp_contains_self_only_and_no_inline_escape`; `remote_markup_and_bidi_do_not_execute_or_spoof_controls`; `token_ticket_request_replays_are_idempotent_or_rejected`; `crypto_failure_writes_zero_bytes`; `web_transfer_opens_no_udp_socket`; all legacy serialization fixtures byte-identical.
- **e2e tests:** `T-WEB-MALFORMED` — corpus su server reale lascia room/peer sano operativo; `T-WEB-XSS-CSRF` — browser prova origin/CSP/markup senza esecuzione o secret referrer; `T-WEB-LEGACY` — listener/sender/public/secret/vhost/ssh regression complete; `T-WEB-UDP-ENDPOINT` — lista socket prima/dopo prova nessun nuovo bind UDP.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + quattro gate passano + fuzz/property run minimo 60 s per decoder in CI-safe harness senza crash/hang + self-review mappa I-WEB1..15 e ogni invariant AGENTS to test evidence + closed in `STATE.md` (§1 → 6.4, §4 ledger row, §6 `none`, §11 board).

**Esito 6.3 (2026-09-16, `agent-1:Claude-Opus-5`).** L'hardening ha trovato un difetto
vero nella parte piu' esposta della superficie — quella che un altro peer sceglie — e ha
chiuso le tre matrici che restavano aperte: decoder, origine, rifiuto.

**Implementato.**

1. **Le stringhe remote non possono piu' mentire su se stesse.** Il gate browser
   `T-WEB-XSS-CSRF` ha misurato che i caratteri di controllo bidirezionale
   (`U+202A`-`U+202E`, `U+2066`-`U+2069`, `U+061C`, `U+200E`/`U+200F`) arrivavano nel
   documento: sopravvivono all'escaping HTML perche' non sono markup, e riordinano il
   testo al momento del rendering — `fattura\u202Efdp.exe` si legge `fattura exe.pdf`
   esattamente nella scheda in cui il destinatario decide se scaricare. Non e' un XSS
   (il testo resta un text node, cosa che `remote_strings_use_text_nodes` gia' fissava)
   ma e' una bugia, ed e' la stessa classe. Il fix e' strutturale: `inertText` in
   `view.js`, applicato dentro `el()` e sulle poche assegnazioni dirette di
   `textContent`, cioe' l'UNICO imbuto per cui passa ogni stringa remota (`view.js` e'
   l'unico modulo che scrive nel DOM: verificato, non assunto). Strippare e non
   rifiutare, perche' il server vede un nome come byte opachi e rifiutare renderebbe
   inusabile un nome legittimamente RTL: e' un obbligo del client, ed e' scritto come
   tale nel protocollo (§7.2).
2. **La matrice origine e' chiusa da una pagina VERA di un'altra origine.** L'upgrade
   WebSocket non e' soggetto alla same-origin policy come `fetch`: qualunque pagina puo'
   aprirne uno. Cio' che non puo' fare e' falsificare `Origin`, quindi il controllo
   esatto del server e' l'intero confine — e va provato da una pagina reale, altrimenti
   si sta testando il test. Il gate serve la pagina attaccante da un server HTTP
   effimero suo (`node:http` su una porta propria) e non da una seconda porta di bore:
   bore non risponde su un `Host` che non ha annunciato, quindi la scorciatoia
   `localhost` vs `127.0.0.1` non mette in scena l'attacco, lo fa fallire prima
   (misurato: `NS_ERROR_NET_EMPTY_RESPONSE`).
3. **Ogni rifiuto pre-auth e' una sola classe.** Token non esadecimale, token esadecimale
   sbagliato e token valido per una stanza inesistente sono tre fatti diversi; chi
   riesce a distinguerli enumera le stanze con un token che ha gia'. Il server rispondeva
   gia' allo stesso modo — `t_web_auth_refusals_are_one_class` lo FISSA, sullo stesso
   close code e con il ritardo uniforme come PAVIMENTO e mai come uguaglianza: una rete
   non e' costante e un gate che pretende tempi costanti e' un gate instabile, non uno
   piu' forte. Il controllo positivo nello stesso test (il token giusto entra) impedisce
   che la proprieta' sia soddisfatta da un server che rifiuta tutto.
4. **Fuzzing con budget, seme e deadline per chiamata.** `tests/web_transfer_fuzz.rs`
   muta corpus validi (bit flip, splice di byte su cui un parser JSON ramifica,
   troncamento, cancellazione, ripetizione di slice) entro il cap che il chiamante vero
   applica, e verifica tre proprieta' che non dipendono dalla validita' dell'input:
   nessun panic, nessuna chiamata oltre `MAX_CALL` (una decodifica quadratica si vede
   come fallimento e non come timeout di CI senza colpevole) e nessun valore restituito
   oltre i limiti con cui e' stato parsato. Deterministico: `BORE_WEB_FUZZ_SEED` e
   `BORE_WEB_FUZZ_SECS`, default 1 s per decoder perche' un `cargo test` ordinario deve
   restare veloce, ed e' lo stesso test che in CI gira con un budget maggiore.

**Lezioni.**

- **Un gate di sicurezza deve poter fallire per il motivo giusto.** Il primo tentativo
  del gate CSRF passava per un errore d'ambiente (nessuna risposta dal server su un
  `Host` sconosciuto), cioe' avrebbe continuato a passare anche con il controllo
  d'origine rimosso. La versione che conta e' quella che monta una vera origine
  straniera.
- **Escaping e riordino sono due problemi diversi.** L'HTML escaping neutralizza il
  markup e lascia intatti i controlli bidi; `textContent` non li tocca per definizione.
  Il primo gate copriva solo il primo dei due, ed e' passato per anni.

**Test aggiunti.**

| Test | Livello | Cosa fissa |
|------|---------|-----------|
| `T-WEB-XSS-CSRF` (`security.spec.mjs`) | browser, 3 motori | origine straniera rifiutata; markup inerte; nessun controllo bidi nel documento |
| `remote_markup_and_bidi_do_not_execute_or_spoof_controls` | unit JS | stesso invariante al livello del renderer, red-checked |
| `t_web_auth_refusals_are_one_class` | e2e Rust | un solo close code e un pavimento di ritardo per tre rifiuti diversi |
| `envelope_decoder_…`, `value_decoders_…`, `relay_attach_decoder_…`, `sealed_frame_decoder_…` | fuzz Rust | nessun panic, nessun hang, nessun risultato fuori limite |
| `T-WEB-MALFORMED`, `T-WEB-UDP-ENDPOINT`, `duplicate_json_keys_are_rejected`, `all_web_decoders_are_panic_free_and_allocation_bounded`, `origin_host_and_subprotocol_matrix_is_closed`, `csp_contains_self_only_and_no_inline_escape`, `token_ticket_request_replays_are_idempotent_or_rejected`, `crypto_failure_writes_zero_bytes` | vari | i gate 6.3 gia' verdi prima di questa tranche |

**Red-check.** Sostituendo il corpo di `inertText` con `return value;` il gate unit
fallisce con «a bidi control reached the document: "\u202E"» — il difetto in produzione,
non una variante di laboratorio.

**Invarianti I-WEB1..15, evidenza.**

| Invariante | Evidenza |
|-----------|----------|
| I-WEB1 no payload sul server | `t_web_nostore`, `t_web_relay_opaque` |
| I-WEB2 solo un click crea un transfer | `T-WEB-DOWNLOAD` + `t_web_transfer_state` |
| I-WEB3 permessi uguali, owner separato | `T-WEB-OWNER-SEPARATION`, `t_web_owner_lease` |
| I-WEB4 chiusura pulita immediata, grace autenticata | `t_web_room_life` |
| I-WEB5 direct poi relay nello stesso TransferId | `T-WEB-DIRECT-FALLBACK`, `t_web_signaling` |
| I-WEB6 AES-GCM su entrambi i percorsi, chiave mai al server | `crypto.test.mjs`, `t_web_relay_opaque`, `t_web_log_privacy` |
| I-WEB7 solo la sorgente originale | `T-WEB-SOURCE-ONLY` |
| I-WEB8 tutto limitato | `t_web_limits`, `t_web_fdbudget`, `t_web_fairness`, fuzz |
| I-WEB9 vecchio wire invariato | `t_web_legacy`, `t_web_native_wire`, fixture v1 |
| I-WEB10 direct e' WebRTC, nessun endpoint UDP | `web_transfer_opens_no_udp_socket` |
| I-WEB11 autorizzazioni di withdraw/cancel/close | `t_web_offer_races`, `t_web_room_life` |
| I-WEB12 cleanup su Arc/Weak catturati | `t_web_registry_life` (P-14 in `a_reregistered_tunnel_keeps_its_carrier_when_the_previous_one_closes`) |
| I-WEB13 heartbeat bounded, reaper su `last_recv` | `t_web_peers`, gate storici P-4/P-7/P-9 rieseguiti |
| I-WEB14 totali e gauge separati | `web_config_totals_do_not_move_under_load`, `web_metrics_report_live_zero_as_zero_not_null` |
| I-WEB15 log/admin senza segreti | `t_web_log_privacy`, `admin_json_never_contains_canary_…` |

**Gate storici rieseguiti (AGENTS.md).** Nella regressione completa: P-4
`public_real_client_survives_past_the_reap_deadline`; P-7/P-9
`beat_once_sends_immediately_to_a_reading_peer` e
`beat_once_stands_down_when_the_peer_stops_reading`, `direct_renewal_*`; P-14
`a_reregistered_tunnel_keeps_its_carrier_when_the_previous_one_closes`; vhost
`vhost_real_client_survives_past_the_reap_deadline`; secret carrier/path report
`secret_path_report_is_recorded_and_rendered`, `set_carriers_updates_snapshot`; SSH jump
`ssh_jump_open_fails_over_when_picked_carrier_dies` piu' la suite
`ssh_gateway_test`. Nessun timeout o registry toccato. Gli harness netns/sudo
(`secret_leak_hunt.sh`, `public_idle_window.sh`, `vpn_netns_test.sh`) restano fuori da
questa sotto-fase: misurano percorsi che il web transfer non usa e richiedono uno
staging, e la 6.5 li rieseguira' serialmente registrando ogni N/A ambientale.

### 6.4 Rendere riproducibili CI e matrice browser

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** integrare build/test frontend e matrice browser nella CI esistente senza duplicare gate; fare self-review pin, cache e artefatti.
- **Files:** `.github/workflows/ci.yml:existing Rust jobs plus new web-transfer job`; `web/transfer/playwright.config.mjs:new projects/reporters/webServer`; `web/transfer/package.json:new CI scripts`; `web/transfer/package-lock.json:generated lock`; `scripts/web_transfer_e2e.sh:new CI mode`; `web/transfer/tests/e2e/*.spec.mjs:new/extended specs`; `web/transfer/dist/**:generated committed assets`; `Cargo.toml:[package] include/exclude rules`; `build.rs:main asset consistency check`.
- **Change:** usare Node 20 e `npm ci`; installare browser Playwright alla versione lockata. Job obbligatorio esegue unit/check, build, `git diff --exit-code -- web/transfer/dist`, quindi progetti Chromium, Firefox, WebKit contro un server Rust release/debug selezionato esplicitamente. Non scaricare dipendenze al runtime Cargo. Shard soltanto per file spec, mai separare i tre peer dello stesso scenario. Conservare trace/screenshot/video solo al failure, redigendo URL fragment nelle label/artifact names; i trace stessi sono artefatti CI riservati e il test deve scrubbare fragment prima della prima screenshot. Aggiungere job scheduled/manual `chrome` e `msedge` soltanto quando i canali sono installati, senza rendere verde un loro skip nascosto: output deve dire `not installed` e la release checklist richiede esecuzione. Playwright WebKit resta gate Safari-compat; aggiungere checklist reale macOS Safari con versione/data/esito in un report release, non una falsa automazione Linux. Test cross-engine pair matrix: Chromium↔Chromium, Firefox↔Firefox, WebKit↔WebKit sempre; Chromium↔Firefox e Chromium↔WebKit per direct/signaling; tutti e tre contro relay. Fissare porte test dinamiche/riservate e serializzare `web_transfer_test` per evitare race listener. Cargo packaging test estrae crate/source artifact, elimina node_modules, compila e serve asset incorporati. R14 — Playwright fornisce engine projects e canali branded ([docs](https://playwright.dev/docs/browsers)).
- **Unit tests:** `playwright_config_defines_required_projects_and_secret_safe_artifacts`; `package_lock_versions_equal_plan_pins`; `cargo_package_contains_dist_but_not_node_modules`; CI syntax/lint già adottato dal repository.
- **e2e tests:** `T-WEB-CROSS` — matrix obbligatoria sopra; `T-WEB-ASSET-DRIFT` — rebuild pulita produce dist identico; `T-WEB-PACKAGE` — artefatto Cargo senza Node serve shell e completa relay; `T-WEB-BRANDED` — Chrome/Edge scheduled/manual con esito esplicito.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + workflow locale/CI passa per tre engine e package test + nessun artifact name/log CI contiene fragment + self-review controlla exact pins e che nessun test critico sia mascherato da conditional/skip + closed in `STATE.md` (§1 → 6.5, §4 ledger row, §6 `none`, §11 board).

**Esito 6.4 (2026-09-16, `agent-1:Claude-Opus-5`).** La matrice browser esisteva gia' su
tre motori; mancava la cosa che un trasferimento e' davvero — una COPPIA — e mancava la
prova che l'artefatto distribuibile contenga cio' che serve.

**Implementato.**

1. **`T-WEB-CROSS`, la matrice delle coppie.** Ogni altra spec mette lo stesso motore sui
   due lati, una volta per progetto: prova che ciascun motore parla con SE STESSO. La
   promessa del prodotto e' un'altra — «manda un file dal tuo Firefox al suo Safari» — e i
   due lati NEGOZIANO (SDP, ICE, i limiti del DataChannel e le dimensioni dei frammenti che
   ne derivano). `cross.spec.mjs` apre quindi coppie nominando i motori da dentro il test:
   cinque coppie sul percorso diretto (chromium, firefox, webkit con se stessi, piu'
   chromium↔firefox e chromium↔webkit) e le stesse cinque sul relay, dove il server deve
   restare cieco al motore. Gira sotto UN solo progetto, altrimenti la stessa matrice
   verrebbe ripetuta tre volte. Ogni braccio verifica anche i BYTE: una prova di interop
   che si ferma a «si sono connessi» e' la prova a cui un bug di frammentazione
   sopravvive. Risultato: 10/10.
2. **`T-WEB-PACKAGE`, l'artefatto.** Il binario incorpora `web/transfer/dist` a tempo di
   compilazione e il bundle e' committato proprio perche' `cargo install bore-cli` non
   debba avere Node. Quella promessa riguarda l'INSIEME DI FILE che `cargo package`
   spedisce, non questo checkout: un `dist` escluso compila qui e restituisce 404 li'.
   Lo script impacchetta, controlla la lista (i cinque asset presenti; `node_modules`,
   `test-results`, `playwright-report` assenti), ricostruisce l'albero dai SOLI file
   elencati, compila senza alcun albero Node e poi CHIEDE l'asset al binario via HTTP
   confrontandolo byte a byte con quello committato. `Cargo.toml` dichiara ora l'`exclude`
   esplicito, e il gate `cargo_package_contains_dist_but_not_node_modules` lo fissa anche
   nella suite ordinaria, cioe' dove si modifica `Cargo.toml`.
3. **`build.rs` confronta la pagina con il bundle.** La lista degli asset richiesti provava
   solo che il bundler fosse girato; non che pagina e bundle fossero d'accordo. Ora ogni
   riferimento `/transfer/assets/...` dentro `index.html` deve risolvere in un asset
   incorporato, a tempo di compilazione — red-checked rinominando `app.js` nella pagina:
   il build fallisce nominando il file mancante invece di produrre un server che serve una
   404 nella pagina il cui unico compito e' caricare uno script.
4. **CI e canali branded.** Il job esistente esegue ora anche la matrice delle coppie, i
   fuzzer con un budget CI e il gate del pacchetto. Chrome ed Edge stanno in un job
   separato `workflow_dispatch`/schedule che li INSTALLA e fallisce se non puo': uno skip
   silenzioso e' esattamente il modo in cui una voce di checklist diventa verde senza
   essere mai stata eseguita. Lo stage `branded` dello script stampa `NOT-INSTALLED` per
   canale invece di saltare in silenzio (su questa macchina: Chrome presente e verde, Edge
   assente e dichiarato tale).
5. **Contratto CI verificato come dato.** `ci.test.mjs` pinna i tre motori piu' i due
   canali branded nella config, il lock esatto rispetto a `package.json` (nessun range: un
   range e' un'installazione diversa su ogni macchina, e la matrice browser e' il posto
   dove questo costa ore) e il fatto che `test:e2e` NOMINI i tre progetti — un progetto
   sparito dalla config altrimenti smetterebbe di girare, in silenzio e in verde.

**Lezione (misurata, non supposta): un aiuto al debug non deve cambiare cio' che osserva.**
Attivare trace e screenshot «solo al fallimento» ha reso `three peers appear, rename and
leave` ROSSO su WebKit in modo deterministico: entrambi STRUMENTANO la pagina — le
snapshot del trace iniettano markup, lo screenshot WebKit inietta un foglio di stile per
nascondere il caret — e questa pagina vive sotto `style-src 'self'` senza `unsafe-inline`.
WebKit rifiuta, logga, e le spec trattano un errore di console come un fallimento: il gate
falliva sulla CSP che funziona. Verificato uno alla volta (tutto spento: verde; solo video:
verde; solo screenshot: rosso), quindi restano il video al fallimento e nient'altro. E' la
stessa famiglia di §8: un harness che fabbrica il proprio difetto.

**Test aggiunti.** `T-WEB-CROSS` (10 casi), `T-WEB-PACKAGE`
(`scripts/web_transfer_package_test.sh` + `cargo_package_contains_dist_but_not_node_modules`),
`playwright_config_defines_required_projects_and_secret_safe_artifacts`,
`package_lock_versions_equal_plan_pins`, il controllo dei riferimenti in `build.rs`
(red-checked) e gli stage `cross`, `fuzz`, `branded`, `package` nel driver.

### 6.5 Validare deployment, protocollo operativo e accettazione completa

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** aggiornare configurazioni di deploy e produrre un gate end-to-end dal binario distribuito; fare self-review delle istruzioni con reverse proxy/TLS reale.
- **Files:** `docker/docker-compose.server.yml:server command and environment`; `docker/docker-compose.server.prod.yml:production command and environment`; `docker/docker-compose-full-yml.yml:server example`; `Dockerfile:asset/package verification stage`; `docs/transfer/WEB_TRANSFER_PROTOCOL.md:final normative and operations sections`; `scripts/web_transfer_e2e.sh:new release mode`; `tests/web_transfer_test.rs:new shutdown/deploy integration`.
- **Change:** aggiungere variabili `BORE_WEB_TRANSFER_*` ai compose come valori commentati/default sicuri; il servizio resta disabled senza base URL. Per direct, esporre soltanto le porte già richieste da `--udp`/STUN esistente; non aggiungere port mapping UDP web. Per same-origin TLS, documentare due deployment supportati nei file pertinenti: TLS nativo del control listener oppure reverse proxy che preserva `Host`, `Origin`, HTTP/1.1 Upgrade e connessioni lunghe su `/transfer/ws/*`; non fidarsi di `X-Forwarded-*`. L'URL base può essere l'origine HTTPS esterna anche se il hop proxy→bore è HTTP loopback. Configurare timeout proxy maggiori di 70 s e disabilitare buffering/compression per relay. Il container deve avere filesystem root read-only compatibile col server web transfer; soltanto configurazioni/cert già previste sono mount, nessun volume upload/temp. Eseguire binario release/container con room, A/B/C, direct, forced relay, cancel/resume, folder ZIP e owner close. Verificare dall'host: socket TCP/UDP attese, fd, filesystem diff, RSS, admin metriche e log privacy. Finalizzare protocol doc con state diagrams testuali, error table, limits, cryptographic byte tables, route/header contract, cleanup matrix, versioning rule: qualsiasi breaking change richiede v2/subprotocol nuovo, mai reinterpretazione silenziosa di v1. Conservare riferimenti normativi con versione/data.
- **Unit tests:** compose/config parser test verifica nomi env e assenza volumi payload/porte UDP nuove; protocol fixture/link checker; release binary `--help` snapshot coincide con docs.
- **e2e tests:** `T-WEB-DEPLOY` — compose/release binary serve same-origin WebSocket dietro TLS/reverse-proxy fixture e completa direct+relay; `T-WEB-ACCEPTANCE` — scenario finale A/B/C con cartella ZIP, republish peer, cancel/resume e owner close; `T-WEB-NOSTORE-CONTAINER` — root read-only e filesystem/fd/log invariati; eseguire i netns/SSH gate esistenti serialmente quando prerequisiti host disponibili e registrare esplicitamente ogni N/A ambientale.
- **Done:** gates green (all authoritative commands in `STATE.md` §3) + T-WEB-DEPLOY/ACCEPTANCE/NOSTORE-CONTAINER passano dal binario/immagine distribuibile + protocol doc e compose coincidono con flag/help + self-review operativo completa su TLS, reverse proxy, firewall, fd, memory e shutdown + closed in `STATE.md` (§1 → 6.6, §4 ledger row, §6 `none`, §11 board).

**Esito 6.5 (2026-09-16, `agent-1:Claude-Opus-5`).** Il deployment e' la parte che
nessun test unitario raggiunge: gli esempi che un operatore copia, il proxy davanti,
il binario che spedisce e l'immagine che gira. Quattro cose sono state PROVATE invece
che dichiarate.

1. **Gli esempi di deploy sono ora verificati dal compilatore, non riletti.** I tre
   compose portano un blocco `--- Web transfer ---` commentato: spento senza
   `BORE_WEB_TRANSFER_BASE_URL`, NESSUNA porta nuova (la superficie vive sulla porta di
   controllo), nessun volume (il server non conserva payload, quindi non ha niente da
   scrivere), i requisiti del reverse proxy (`Host` e `Origin` passati intatti,
   `Upgrade` permesso, buffering spento, timeout oltre i 70 s) e i dodici valori di
   capacita'. `tests/web_transfer_deploy_test.rs` legge quei file: ogni variabile
   nominata deve essere una che la CLI legge davvero, nessun esempio puo' aggiungere
   una porta UDP o un volume, e ogni default citato e' confrontato con
   `WebTransferLimits::default()`. Quest'ultimo gate ha trovato **cinque default
   sbagliati** che avevo scritto a mano nei commenti — esattamente la deriva che rende
   un esempio peggiore dell'assenza di un esempio.
2. **`T-WEB-DEPLOY`: il proxy davvero in mezzo.** Il server ascolta su una porta e la
   base URL e' quella del PROXY; la shell e il `welcome` arrivano attraverso il proxy,
   e un client che scavalca il proxy presentando l'authority del listener viene
   rifiutato — che e' il punto: `X-Forwarded-*` non e' letto e l'origine e'
   confrontata esattamente. Il test prima scadeva invece di fallire, perche' il proxy
   di supporto non propagava il FIN: `spawn_proxy` ora chiude la meta' di scrittura
   opposta a EOF. Un harness che scade nasconde la differenza tra «rifiutato» e
   «mai risposto», che e' la sola cosa che questo gate misura.
3. **Il binario di RELEASE, non quello di debug.** Lo stage `release` costruisce
   `--release` e ci fa girare l'accettazione (`folder-zip` + `multipeer`), il flusso
   del README (`T-WEB-README-RELEASE`) e un confronto fra l'`--help` del binario che
   spedisce e i flag documentati: un profilo diverso che accendesse o spegnesse una
   feature lascerebbe la guida a descrivere bandierine che il binario scaricato non ha.
4. **`T-WEB-NOSTORE-CONTAINER`: l'immagine spedita, root in sola lettura, nessun
   volume.** Serve il bundle committato byte per byte, non apre alcun socket UDP,
   ospita un trasferimento vero fatto da browser veri (1 MiB, sha256 confrontato, path
   `relay`), esce con `docker diff` VUOTO e pubblica i gauge admin. Una lezione l'ha
   insegnata il gate stesso: l'immagine e' `FROM scratch` — un solo binario statico,
   niente shell e niente `cat` — quindi `docker exec ... cat /proc/net/udp` risponde
   127 e sotto `set -e` il gate finiva in silenzio dopo due PASS, con exit 0. Ora la
   tabella del kernel e' letta da un SIDECAR che condivide il network namespace del
   container (`docker run --network container:<name>`), e se quell'immagine non c'e' il
   gate stampa `N/A` invece di passare: un controllo che non ha potuto girare deve
   dirlo.

### 6.6 Update README.md

- **Model:** `agent:gpt-5.6-luna`
- **Assignment:** eseguire la revisione finale della documentazione come unico agente, rileggendo tutti i comportamenti pubblici e riprovando i comandi; self-review obbligatoria dell'intero README.
- **Files:** `README.md:Table of contents`; `README.md:290 — command overview`; `README.md:881 — self-hosting`; `README.md:900 — all server flags`; `README.md:996 — HTTPS/reverse proxy`; `README.md:1027 — admin`; `README.md:2035 — secure file transfer`; `README.md:3200 — troubleshooting`.
- **Change:** modificare il README esistente senza riscriverlo, preservandone struttura, tono e lingua; renderlo la fonte unica completa per installazione, deploy e uso finale. Includere: flusso CLI→browser senza selezione CLI; formato e sensibilità link; uguaglianza dei peer; annuncio versus trasferimento; no-auto-download; direct WebRTC preferito e relay automatico; E2EE e metadati visibili al server; file, folder, singolo download e ZIP per offerta; OPFS/quota/save/resume; ownership cancel/withdraw; lifecycle clean/grace; tutti i flag client/server, env, default e validazioni; server disabilitato senza base URL; TLS/same-origin/reverse proxy; STUN/no TURN; limiti/rate/fd; metriche admin aggregate; browser support e Safari smoke; container/binary/SSH gateway con comandi realistici e `sudo` dove il repository lo richiede; troubleshooting per old server, origin/host, proxy upgrade, WebRTC blocked, relay busy/rate, quota/OPFS, source changed, room expired, asset mismatch. Inserire uno scenario A/B/C numerato identico al phase criterion e chiarire che chi chiude la shell proprietaria invalida URL/transfer. Non includere struct, file, algoritmi interni, nomi fase o roadmap. Controllare che nessuna sezione precedente contraddica i sei subcommand descritti o il transfer nativo.
- **Unit tests:** docs/help/env/default/link consistency; verifica automatica che ogni `--web-transfer-*` appaia una sola volta nella tabella autorevole e che esempi non contengano secret reali.
- **e2e tests:** `T-WEB-README-RELEASE` — da checkout/build puliti seguire soltanto README per binary, reverse proxy e container, completare scenario A/B/C e troubleshooting principali con output atteso.
- **Done:** un nuovo utente e un operatore possono installare, configurare, usare, osservare e diagnosticare il servizio dalla sola README; tutti gli esempi sono stati eseguiti; tutti i gate fase/repository sono verdi; self-review finale non trova dettagli interni o contraddizioni; closed in `STATE.md` con §1 `Status: none`, §6 `none`, tutte le righe §11 e stato piano `COMPLETE`.

**Esito 6.6 (2026-09-16, `agent-1:Claude-Opus-5`).** La revisione finale del README ha
trovato quello che una revisione finale deve trovare: prosa vera al momento in cui fu
scritta e falsa adesso, e prosa che nessun gate teneva ancorata al prodotto.

1. **Una riga di troubleshooting prometteva il contrario di cio' che il prodotto fa.**
   `Cartelle e selezioni multiple non ancora supportate` era il testo di `MULTI_ENTRY`
   dalla 3.8 — e la 5.x ha spedito cartelle, ZIP e la scelta di un singolo file
   dall'albero, trecento righe piu' in su nella stessa pagina. Il rifiuto pero' NON e'
   morto: e' difensivo, e si raggiunge solo quando arriva una richiesta `raw` per
   un'offerta con piu' di un file senza nominarne uno (il pulsante della card chiede lo
   ZIP, quindi in pratica significa che l'offerta e' cambiata fra il render e il click).
   Il codice resta, il TESTO ora descrive il caso reale: «Offerta con piu' file: scegli
   un file o scarica lo ZIP». Corretti insieme `state.js`, il bundle e il README.
2. **Mancavano le righe che il piano chiedeva** — file cambiato alla sorgente, sorgente
   irraggiungibile, offerta cambiata, bundle servito da una cache (il server manda
   `Cache-Control: no-cache` e il bundle e' compilato DENTRO il binario, quindi un
   bundle vecchio e' sempre un proxy che riscrive quell'header) e versione non
   supportata. La sezione admin ora documenta anche le metriche della superficie web,
   dicendo esplicitamente che sono AGGREGATE: rispondono a «cosa ho configurato» e «cosa
   sta succedendo», mai a «chi trasferisce cosa».
3. **Quattro nuovi ancoraggi in `t_web_readme`, tutti red-checked.** Una riga di flag
   duplicata (la tabella era letta in una mappa, quindi un doppione diventava
   silenziosamente l'ultima riga); una stringa esadecimale lunga nella guida (un id, una
   chiave o un token VERO incollato in un esempio consegna una room a ogni lettore); un
   messaggio di troubleshooting che la pagina non mostra piu', letto direttamente dalla
   mappa `ERROR_TEXT` di `state.js`; e un campo admin citato che nessuna view pubblica.
   Il tema e' sempre lo stesso: la documentazione che cita una COSTANTE del prodotto
   deve derivarla dal prodotto, altrimenti divergono in silenzio.

---

## Phase gates

- **Build:** `cargo build --all-features`
- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-features --all-targets -- -D warnings`
- **Test subset:** `cargo test --all-features --lib && cargo test --all-features --test web_transfer_test -- --test-threads=1 && npm ci --prefix web/transfer && npm run check --prefix web/transfer && npm run test:e2e --prefix web/transfer`
- **Asset drift:** `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist`
- **Regression guard:** `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_ && cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1`; eseguire inoltre i gate sudo/netns prescritti da `AGENTS.md` serialmente quando disponibili.
- **Acceptance:** T-WEB-SOAK, FDBUDGET, ADMIN, LOG-PRIVACY, MALFORMED, XSS-CSRF, LEGACY, UDP-ENDPOINT, CROSS, PACKAGE, DEPLOY, ACCEPTANCE, NOSTORE-CONTAINER e README-RELEASE tutti verdi o con N/A esclusivamente ambientale documentato e non relativo a un target CI obbligatorio.
- **README:** revisione finale completa, esempi realmente eseguiti, nessun comportamento/flag/deploy mancante.

## Phase done criterion

Il binario e il container distribuibili superano lo scenario A/B/C, carico, limiti, privacy, no-storage, lifecycle, malformed input, matrice browser e regressione completa. Config e metriche conservano semantica distinta, nessuna nuova socket UDP esiste e i vecchi modi restano invariati. README.md è la fonte utente completa e `STATE.md` §11 mostra tutte le fasi e i gate `DONE`, con nessuna unità aperta.

## Esito — primo giro di CI su `dev` (`84cd16d`, 2026-09-16)

Tre workflow verdi al primo colpo — `E2E (netns)`, `Docker (GHCR)`, `Mean Bean
Deploy` — e due rossi, `CI` e `Mean Bean CI`. Nessuno dei rossi era un difetto
del prodotto: **otto** difetti in tutto, uno di dipendenza e sette
dell'harness, tutti di una classe che una workstation Linux non può vedere per
costruzione.

1. **Una dipendenza vulnerabile** (T-A002). `cargo audit` segnala
   RUSTSEC-2026-0285 su rustls 0.23.40, pubblicato il giorno prima del push:
   aggiornato a 0.23.45. È l'unico dei otto che riguarda ciò che viene
   spedito.
2. **Un errore terminale che correva contro il teardown** (B-A002). L'unico
   difetto di prodotto del giro, e l'unico trovato da `windows-latest`: su uno
   stream muxato `send` accoda soltanto, quindi il ritorno immediato lasciava
   che M-1 chiudesse la connessione prima che il frame partisse. Il client
   leggeva un EOF pulito al posto di «versione non supportata». Corretto con
   `linger_after_error` e red-checked.
3. **Un job che non aveva mai eseguito i propri test** (B-A003). `node --test`
   espande i glob solo da Node 22; la CI usa Node 20 e la slice browser era
   rossa fin dal primo giorno per un file inesistente. Ora il glob lo espande
   la shell.
4. **Cinque gate che descrivevano la macchina invece del server**
   (B-A004..B-A008): tre leggevano `/proc` e fallivano duro su macOS (e uno di
   essi sarebbe passato A VUOTO, che è peggio); uno osservava `--open`
   attraverso `$BROWSER`, che `webbrowser` non consulta su macOS; uno aspettava
   400 ms fissi l'uscita di un processo; uno confrontava l'RTT del control
   plane con un millisecondaggio assoluto invece che con un rapporto (V-9);
   l'ultimo, il soak, buttava via i peer già ammessi a ogni ritentativo e
   competeva con il proprio rilascio.

La lezione del giro, ed è la ragione per cui questa sezione esiste: **un gate
che non può girare deve dirlo.** Quattro dei sette difetti dell'harness erano
un numero o un file di sistema presi per universali; uno di essi — le
asserzioni sui descrittori su una lista vuota — sarebbe passato in silenzio per
sempre. Ogni ramo che ora non può misurare stampa `N/A` con la ragione, e ogni
budget che restava è diventato un rapporto o una costante DERIVATA da quella
che il server pubblica (la scadenza del soak segue ora
`WEB_TRANSFER_CTRL_SEND_TIMEOUT`, che è il vero limite superiore al ritorno di
un permesso).

## Esito — secondo giro di CI su `dev` (`111a98c`, 2026-09-16)

Quattro workflow su cinque verdi: `E2E (netns)`, `Docker (GHCR)`,
`Mean Bean Deploy` e — per la prima volta — nessun fallimento di `Security
audit`, `macOS VPN build` o `Windows`. Restano rosse `CI` e `Mean Bean CI`, e
il giro ha insegnato la cosa più utile di tutte: **aggiustare B-A003 ha fatto
GIRARE la slice browser**, che ha subito riportato 145 passati e 2 falliti. Un
job che non era mai partito non stava dando alcuna garanzia; ora ne dà una.

Cinque difetti nuovi, tutti nell'harness, e tre di essi della stessa famiglia
dei precedenti:

- **B-A009**: il conteggio esatto del burst di controllo. Un token bucket si
  ricarica mentre il burst viene servito, quindi `60` misurava quanto in fretta
  la macchina drena 200 messaggi. Pavimento e soffitto vengono ora dalle
  costanti del server.
- **B-A010**, il più serio: `free_port()` faceva bind su `:0` e rilasciava. Il
  job `Build, test & lint` gira l'intero workspace con il parallelismo di
  DEFAULT, quindi due test ricevevano la stessa porta, il secondo
  `Server::listen` falliva il bind dentro uno `spawn` che scarta l'errore, e il
  test che si credeva padrone della porta parlava con il server di un altro.
  `wait_port` completava l'inganno tornando in silenzio dopo cinque secondi.
  Ora le porte vengono da un cursore atomico sotto l'intervallo effimero e
  `wait_port` fa `panic!` con porta e direzione.
- **B-A011**, il più istruttivo: `t_web_log_privacy` accusava il prodotto di
  aver stampato il payload. La riga era di `tungstenite::protocol`, cioè del
  client WebSocket DEL TEST, catturata perché il buffer prendeva tutto il
  processo e il ponte `log` era a `TRACE`. Un gate di privacy che legge il log
  di chiunque non sta misurando il server.
- **B-A012**: i due tentativi di `T-WEB-PATH-UI` erano decorativi, perché
  l'attesa che può legittimamente fallire era una `expect` dura dentro il
  ciclo.
- **B-A013**: 300 s di budget per un test che su due core ne vuole di più.

Il filo conduttore dei due giri, in una riga: **ogni numero assoluto in un
gate è un'ipotesi sulla macchina.** Di tredici difetti trovati dalla CI, uno
solo era nel prodotto (B-A002); gli altri erano un harness che descriveva la
workstation su cui era stato scritto, o che leggeva più di quanto affermasse.

### Esito — terzo giro di CI su `dev` (`6356e10`, 2026-09-16)

Quattro workflow su cinque verdi: `E2E (netns)`, `Docker (GHCR)`,
`Mean Bean Deploy` e — per la prima volta — `Mean Bean CI`. Rosso il solo
workflow `CI`, per due job e due difetti, entrambi dell'harness.

- **B-A014** (`windows-latest`): `cargo clippy --features vpn --all-targets --
  -D warnings` rifiuta `unused variable: pid` due volte e il crate di test non
  compila. Il warning è però la parte piccola: `t_web_cli` usa quel pid solo
  dentro `#[cfg(unix)] signal_pid(..)`, quindi su windows sarebbe stato
  compilato ed ESEGUITO senza mai chiudere i processi che avvia — tre join da
  dieci secondi, poi l'accusa al prodotto di non essere uscito. Tre delle sue
  quattro gambe sono segnali, e `t_web_room_life` aveva già la forma giusta:
  il test intero è ora `#[cfg(unix)]`. Un test che non può eseguire il proprio
  soggetto va saltato, non indebolito.
- **B-A015** (`macos-14`): `t_web_fairness` di nuovo rosso, e stavolta contro
  il rimedio di B-A007 — «answered in 1.747280833s (budget 1.346883625s, relay
  4.040650875s, idle 279.709µs)». Cioè il control plane è stato **2,3 volte
  più veloce** della cosa che era accusato di aspettare. `elapsed_b / 3`
  *sembra* un rapporto e non lo è: il relay è limitato dal token bucket del
  server, quindi `elapsed_b` è una costante di configurazione e dividerla
  produce un budget assoluto travestito — V-9 al secondo giro, sulla stessa
  asserzione. Il verdetto stava per giunta sul PEGGIORE di dieci campioni,
  cioè sulla singola pausa di scheduler di un runner condiviso.

  Il rimedio viene dall'aritmetica del difetto invece che da un numero: un
  control plane bloccato risponde a un ping mandato a `t` solo quando il relay
  finisce a `T`, quindi ogni campione costa `T - t` e dieci campioni a 200 ms
  danno mediana ≈ 0,55·`T` e peggiore ≈ `T`. Un control plane sano risponde in
  microsecondi, e lo scheduler allunga i CAMPIONI, mai la mediana di dieci. Il
  gate giudica ora `mediana < elapsed_b / 4` — tre ordini di grandezza di
  margine — e tiene `peggiore < elapsed_b` come forma letterale del difetto.
  Misurato in locale dopo il cambio: `FAIRNESS a=4.01s b=4.01s control-rtt
  median=0ms worst=1ms idle=0.2ms samples=[0,0,0,0,0,0,0,0,0,0]`.

Il conto dopo tre giri: **quindici difetti trovati dalla CI, uno solo nel
prodotto** (B-A002). E una lezione che il secondo giro non aveva ancora
imparato: non basta che un limite *abbia la forma* di un rapporto — il
denominatore deve misurare la stessa cosa che il numeratore rischia di
misurare. Un denominatore che è una costante di configurazione riporta
l'asserzione esattamente da dove veniva.

### Esito — quarto giro di CI su `dev` (`e612b19`, 2026-09-16)

Due rossi, nessuno dei due nel prodotto.

- **`Security audit`** non ha trovato nulla: è morto prima di poter auditare,
  con `error: couldn't fetch advisory database: git operation failed: An IO
  error occurred when talking to the server`. Guasto di rete del runner
  mentre clonava il database RustSec. In locale, con il DB presente, lo stesso
  comando esce 0 con zero vulnerabilità e le sette warning già ammesse. Il job
  è stato rilanciato.
- **B-A014, secondo giro**: marcare `t_web_cli` come `#[cfg(unix)]` ha reso
  ORFANO `room_alive`, i cui unici due chiamanti erano quel test e
  `t_web_room_life` (già `#[cfg(unix)]`). Su windows `-D warnings` lo respinge
  come `never used` e il crate di test non compila — lo stesso job di prima,
  un difetto più in là. `split_room_url` e `next_line` invece restano
  disponibili ovunque, perché `t_web_nostore` e `t_web_soak` li usano.

  La lezione vera è sull'**oracolo**. Gating condizionale si verifica solo
  compilando il ramo che si è disabilitato, e questa workstation non ha mingw,
  quindi `--target x86_64-pc-windows-gnu` non parte (`cc-rs: failed to find
  tool "x86_64-w64-mingw32-gcc"`). Ma ciò che conta non è Windows: è
  «gli item `cfg(unix)` non esistono», e quello si riproduce in locale con un
  cfg sempre falso —

  ```sh
  sed -i 's/#\[cfg(unix)\]/#[cfg(any())]/g' tests/web_transfer_test.rs
  cargo clippy --all-features --test web_transfer_test -- -D warnings   # exit 0
  ```

  Dopo la correzione esce 0, e con `room_alive` ancora pubblico avrebbe
  mostrato esattamente il `never used` della CI. Questo comando è ora in
  `STATE.md` §7 come gate del ramo non-unix: una modifica `cfg` non va spedita
  alla CI per sapere se compila.
