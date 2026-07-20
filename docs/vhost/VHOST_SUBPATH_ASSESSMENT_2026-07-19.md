# Analisi vhost con mount su subpath

**Data:** 2026-07-19  
**Modello:** GPT-5 Codex  
**Stato:** analisi e proposta; nessuna modifica di comportamento implementata

## 0. Verdetto

Sono due requisiti distinti:

1. **Routing per path sullo stesso host**, per esempio
   `dev.bore.example.com/site1/` e `dev.bore.example.com/site2/`: **fattibile**.
2. **Far credere a qualunque applicazione non predisposta ai subpath di essere ancora
   pubblicata su `/`, riscrivendo automaticamente ogni URL e ogni asset**: **non è
   garantibile in modo generale da un reverse proxy**.

La feature è quindi consigliabile solo con un contratto esplicito:

- bore può registrare e instradare mount distinti sullo stesso subdomain;
- bore può togliere il prefisso in ingresso, aggiungere `X-Forwarded-Prefix` e
  riscrivere in modo affidabile header come `Location` e `Set-Cookie Path`;
- l'applicazione, un middleware applicativo o un adapter dedicato deve generare URL
  pubblici contenenti il mount;
- un eventuale riscrittore HTML/CSS può essere offerto soltanto come modalità
  **best-effort**, mai come compatibilità universale.

Per Odoo root-only, la raccomandazione è continuare a usare un origin/subdomain distinto,
oppure sviluppare e versionare un adapter Odoo dedicato. Il solo core di bore non può
promettere che `/odoo1/` sia equivalente alla root in tutti i flussi.

## 1. Requisito preciso

Per il mount `/site1/`, la traduzione desiderata è:

| Direzione | URL pubblico | URL visto/generato dal backend |
|---|---|---|
| richiesta | `/site1/` | `/` |
| richiesta | `/site1/assets/app.js?v=4` | `/assets/app.js?v=4` |
| redirect | `/site1/login` | `Location: /login` |
| cookie | valido sotto `/site1/` | `Set-Cookie: ...; Path=/` |
| WebSocket | `/site1/websocket` | `/websocket` |

Inoltre, ogni URL emesso nell'HTML, CSS, JavaScript, JSON, manifest, redirect, cookie,
WebSocket, worker o integrazione esterna dovrebbe tornare al browser con `/site1/`.
Quest'ultima parte è il confine tra un normale path reverse proxy e l'emulazione
trasparente della root.

## 2. Stato attuale del codice

### 2.1 Registrazione e routing

- `VhostRegistry` è un `DashMap<String, Arc<VhostEntry>>` indicizzato soltanto dal
  label DNS (`src/vhost.rs`, `VhostRegistry`). Non può contenere due provider con lo
  stesso subdomain.
- `HelloVhost` trasporta `subdomain`, metadata e opzioni, ma nessun mount
  (`src/shared.rs`, `ClientMessage::HelloVhost`).
- `serve_vhost_provider` rifiuta atomicamente un secondo provider sullo stesso label e
  registra anche carrier pool, metriche, header e direct pool per quel label.
- HTTP dedicato, HTTPS dedicato e HTTP sulla porta di controllo eseguono tutti una sola
  lookup per `Host -> subdomain` (`src/vhost.rs::handle_http`,
  `src/vhost.rs::handle_https`, `src/server.rs::serve_control_http`).

### 2.2 Il routing è per connessione, non per richiesta

Il frontend legge soltanto il primo request head, sceglie il provider, apre un substream
e poi esegue uno splice bidirezionale dell'intera connessione. Di conseguenza:

- i successivi request HTTP/1.1 keep-alive restano sul provider scelto dal primo;
- il codice documenta esplicitamente che request/response header injection riguarda
  soltanto il primo scambio;
- una connessione che richieda prima `/site1/...` e poi `/site2/...` non può essere
  instradata correttamente con una semplice mappa path -> provider;
- anche due richieste successive allo stesso mount avrebbero bisogno di rimuovere il
  prefisso da **ogni** request target, non soltanto dal primo.

Questo è il principale cambiamento architetturale richiesto: un host con più mount deve
usare un proxy HTTP per-request, non il raw splice attuale.

### 2.3 HTTPS e protocolli

- bore termina TLS e poi analizza HTTP/1.1.
- Il TLS acceptor vhost non configura ALPN HTTP/2; la documentazione corrente dichiara
  WebSocket classico `HTTP/1.1 Upgrade`, non RFC 8441.
- Dopo un `101 Switching Protocols`, il raw splice è corretto e deve rimanere tale: il
  routing è ormai fissato dal path dell'handshake.

### 2.4 UDP/QUIC direct

La chiave del direct handshake vhost è oggi il solo subdomain:

- pending nonce indicizzato per subdomain;
- client `direct_key = subdomain`;
- server installa la connessione nel `VhostEntry` cercato con quella chiave.

Più mount sullo stesso host richiedono una chiave namespaced per route; modificare la
sola registry HTTP lascerebbe collisioni nel direct path.

### 2.5 SSH gateway, admin e configurazione

Anche questi componenti assumono `1 subdomain = 1 provider`:

- SSH accetta `vhost/<label>` e valida tutto il resto come singolo label DNS;
- takeover/ownership SSH è indicizzato dal label;
- `vhost.yml` riserva un intero subdomain a un client;
- dashboard e API mostrano/countano subdomain, non route/mount;
- `VhostReady` restituisce URL senza path.

La feature deve quindi essere end-to-end; limitarla a `handle_http` produrrebbe
registrazioni, direct QUIC, takeover e osservabilità incoerenti.

## 3. Perché la root emulation universale è impossibile

### 3.1 Perdita irreversibile dell'identità del mount

Si considerino due pagine:

```javascript
// Applicazione site1
fetch('/api/session');

// Applicazione site2
fetch('/api/session');
```

Il browser invia in entrambi i casi:

```http
GET /api/session HTTP/1.1
Host: dev.bore.example.com
```

Le richieste sono indistinguibili. Il browser può anche usare lo stesso pool di
connessioni perché scheme, host e porta sono identici. Quando `/site1/` è scomparso
dall'URL, il proxy non possiede informazione sufficiente per ricostruire in modo
deterministico il provider corretto.

Possibili euristiche non risolvono il problema:

- `Referer` può essere assente per policy/privacy o non identificare una fetch;
- un cookie globale di routing impedisce l'uso contemporaneo di site1 e site2 in tab
  distinti;
- cookie con `Path=/site1/` non vengono inviate a `/api/session`;
- il pinning per connessione è invalido perché le connessioni HTTP sono condivise per
  origin;
- un service worker scoped a `/site1/` non controlla normalmente richieste a `/api` e
  due worker non possono entrambi possedere la root dello stesso origin.

Per evitare la perdita di informazione, l'app o il contenuto consegnato al browser deve
produrre `/site1/api/session`. Non esiste una correzione affidabile da applicare dopo
che è arrivato al proxy `GET /api/session`.

Con **un solo** provider mounted su un host si potrebbe usare quel provider come
fallback anche per `/api/session`; in pratica, però, esso tornerebbe a possedere la root
dell'host. La strategia diventa ambigua non appena si aggiunge `/site2/`, quindi non
soddisfa il caso richiesto e non è una base estendibile.

### 3.2 Superfici URL da trasformare

| Superficie | Trasformazione | Affidabilità generica |
|---|---|---|
| request target | togliere `/site1` | alta |
| `Location`, `Content-Location`, `Refresh` | aggiungere mount agli URI locali | alta, con parser corretto |
| `Set-Cookie Path=/` | trasformare in `Path=/site1/` | alta |
| HTML (`href`, `src`, `action`, `srcset`, SVG, meta refresh) | riscrittura strutturata | media |
| CSS (`url()`, `@import`) | parser CSS | media |
| JavaScript statico | AST più analisi semantica | bassa |
| URL costruiti a runtime, `eval`, WASM | intercettazione runtime | non generale |
| JSON/XML con URL applicativi | richiede conoscenza dello schema | non generale |
| email, webhook, OAuth callback generati fuori dalla risposta corrente | configurazione applicativa | impossibile al proxy |
| service worker, manifest, `start_url`, worker e moduli | riscritture coordinate e scope | bassa |

Una sostituzione testuale di `"/"` corromperebbe divisioni, regex, JSON, firme, path di
terze parti e dati ordinari. Anche un AST JavaScript non può sapere se una stringa
costruita dinamicamente rappresenta un URL.

### 3.3 Compressione, cache e integrità

Modificare i body richiede inoltre:

- decompressione/ricompressione per gzip, Brotli e deflate;
- aggiornare `Content-Length` o cambiare framing;
- invalidare/ricalcolare `ETag`, `Content-MD5`, `Digest` e cache validator;
- gestire range requests su rappresentazioni trasformate;
- preservare streaming, SSE e grandi download senza buffering illimitato;
- rispettare CSP, nonce e hash;
- ricalcolare o rimuovere SRI (`integrity=`) se si modifica CSS/JavaScript.

Un `<base href="/site1/">` aiuta soltanto gli URL relativi; non cambia `/api`,
`location.href = '/x'`, WebSocket root-absolute o callback esterne.

### 3.4 Stesso origin non significa isolamento

`/site1/` e `/site2/` condividono scheme/host/port, quindi condividono lo stesso origin.
Questo comporta:

- `localStorage` e IndexedDB condivisi;
- possibili collisioni di service worker e Cache Storage;
- cookie omonimi da gestire attentamente con `Path`;
- nessuna separazione Same-Origin Policy tra applicazioni;
- una compromissione XSS in un'app può interagire con risorse same-origin dell'altra.

Un path non deve quindi essere presentato come boundary di sicurezza equivalente a due
subdomain.

## 4. Caso Odoo

### 4.1 Evidenza ufficiale

La documentazione Odoo 19 descrive `--proxy-mode` in termini di
`X-Forwarded-Host`, `X-Forwarded-Proto` e `X-Forwarded-For`; non documenta il supporto
di `X-Forwarded-Prefix`. Anche il core Odoo configura Werkzeug `ProxyFix` con
`x_for`, `x_proto` e `x_host`, non `x_prefix`.

Inoltre il deployment Odoo multiprocess richiede routing dedicato del path
`/websocket/` verso il gevent port. L'esempio ufficiale usa un upstream per HTTP e uno
per websocket. Il vhost bore attuale ha un solo target locale, quindi una produzione
Odoo tipica necessita già di un sidecar locale (per esempio nginx) o di un'estensione
multi-target.

Riferimenti:

- <https://www.odoo.com/documentation/19.0/developer/reference/cli.html>
- <https://www.odoo.com/documentation/19.0/administration/on_premise/deploy.html>
- <https://github.com/odoo/odoo/blob/19.0/odoo/http.py>

### 4.2 Implicazioni

Impostare `proxy_mode = True` e `web.base.url` è utile per host e scheme, ma non è una
prova di compatibilità con un mount arbitrario. Gli endpoint del client web, asset,
WebSocket, redirect, URL generati da moduli, report, OAuth e link inviati fuori banda
devono tutti conoscere `/odoo1/`.

Un profilo built-in `--compat odoo` in bore sarebbe:

- dipendente dalla versione Odoo;
- difficile da testare soltanto con unit test HTTP;
- soggetto a regressioni a ogni cambiamento del frontend o dei moduli installati;
- incompleto per moduli custom ed Enterprise non disponibili nel repository bore.

Per rendere davvero supportato `dev.../odoo1/`, servirebbe un adapter/middleware Odoo
versionato che renda il prefisso parte del suo URL model, più un sidecar per il routing
HTTP/WebSocket. Bore dovrebbe occuparsi del mount e del tunnel, non interpretare il
contenuto applicativo Odoo.

## 5. Disegno raccomandato: mount robusto per app prefix-aware/adattate

### Fase 1 — Modello route e protocollo

#### 1.1 CLI nativa

Proposta:

```bash
bore vhost 127.0.0.1:8081 \
  --subdomain dev \
  --mount /site1/ \
  --id client-site1

bore vhost 127.0.0.1:8082 \
  --subdomain dev \
  --mount /site2/ \
  --id client-site2
```

Contratto v1:

- `--mount` assente equivale al legacy root-vhost;
- mount canonico: inizia e finisce con `/`, eccetto `/` stesso;
- query string esclusa dal matching e preservata nella riscrittura;
- matching su boundary di segmento: `/site1/` non cattura `/site10/`;
- `/site1` riceve `308` verso `/site1/`;
- nessuna percent-decoding durante la selezione della route (`%2f` non diventa `/`);
- mount con dot-segment, controllo, backslash, `?`, `#` o encoding ambiguo rifiutato;
- niente route sovrapposte nella v1; evita shadowing/hijack tra client;
- una route root `/` è esclusiva e non coesiste con mount sullo stesso subdomain.

L'esclusività della root permette di mantenere il path attuale byte-identico per tutti
i vhost legacy.

#### 1.2 Registry

Separare esplicitamente:

```text
HostRoutes::LegacyRoot(VhostEntry)
HostRoutes::Mounted(RouteTable<mount, VhostEntry>)
```

Per i mounted host, il lookup deve essere per-request. Non usare una chiave stringa
concatenata senza un tipo/normalizzazione centrale; registry, pending UDP, ownership,
admin e log devono condividere lo stesso `RouteId`.

#### 1.3 Wire compatibility

Estendere `HelloVhost` con un campo additivo `mount_path` con `#[serde(default)]` è
necessario ma non sufficiente. Un server vecchio ignora campi JSON sconosciuti e
registrerebbe accidentalmente `dev` come root.

Serve un ack esplicito:

- il nuovo server restituisce nel `VhostReady` il mount normalizzato e/o un `route_id`;
- un nuovo client che ha richiesto `--mount` rifiuta un ack privo del campo o diverso;
- client vecchio -> server nuovo resta root e mantiene il comportamento attuale;
- server vecchio -> client mounted fallisce rumorosamente e si deregistra, mai silent
  downgrade a root.

#### 1.4 Config e ownership

Una reservation legacy senza `mount` deve continuare a riservare **l'intero host** al
client, così un upgrade non indebolisce una policy esistente. Le reservation path-based
devono essere esplicite:

```yaml
reservations:
  - client_id: client-site1
    subdomain: dev
    mount: /site1/
  - client_id: client-site2
    subdomain: dev
    mount: /site2/
```

Takeover SSH e collision detection devono operare sul `RouteId`, non sul solo label.

#### 1.5 URL, QUIC e SSH

- `VhostReady` deve mostrare `https://dev.../site1/`.
- Pending nonce e direct registry devono usare una chiave namespaced non ambigua, per
  esempio una serializzazione canonica di `RouteId`; il legacy root conserva la vecchia
  chiave per compatibilità.
- `VhostUdpRenew` deve identificare la route completa.
- SSH può usare una grammatica additiva come `vhost/dev/site1`; il bare label 80/443
  resta legacy root. Permit, banner, cancel e takeover vanno aggiornati insieme.

### Fase 2 — Frontend HTTP per-request, separato dal legacy fast path

Per un host `Mounted`, bore deve diventare un HTTP/1.1 reverse proxy:

1. leggere e validare ogni richiesta;
2. scegliere il mount;
3. aprire un nuovo provider substream per la richiesta;
4. togliere il mount dal request target;
5. trasmettere il body in streaming con backpressure;
6. leggere e restituire la risposta preservando framing e half-close;
7. ripetere sulla stessa connessione pubblica;
8. dopo `101`, passare a un unico raw splice task per WebSocket.

Il legacy `HostRoutes::LegacyRoot` deve continuare a usare l'attuale
`relay_vhost`/`copy_bidirectional_with_sizes`, senza regressioni di throughput.

È sconsigliato estendere il piccolo parser hand-written soltanto per i casi felici. Un
proxy corretto deve gestire almeno:

- `Content-Length`, chunked e trailer;
- `HEAD`, `1xx`/`100 Continue`/`103`, `204`, `304`;
- request body in streaming e upload grandi;
- disconnect e half-close;
- keep-alive e richieste sequenziali sullo stesso socket;
- upgrade WebSocket;
- limiti su head, count/size header e timeout delle sole fasi appropriati;
- hop-by-hop header (`Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade`).

La scelta raccomandata è una libreria HTTP mantenuta e streaming, confinata al nuovo
path `Mounted`. Riutilizzare i parser di `weblog.rs` è possibile, ma essi sono tap
best-effort che degradano a raw; un router non può degradare a raw senza perdere il
tenant. Qualunque soluzione deve rispettare l'invariante: un `mux::Stream` non va mai
split tra due task; le due direzioni possono essere guidate nello stesso task/future,
come l'attuale `try_join!`.

### Fase 3 — Semantica prefix-aware affidabile

Ingress obbligatorio:

- `/site1/x?q=1` -> `/x?q=1`;
- `X-Forwarded-Prefix: /site1`;
- `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Forwarded-Port` e forwarding IP con una
  policy trust chiara;
- `Host` pubblico preservato di default;
- eventuale rewrite del `Referer` same-origin verso la vista root del backend.

Egress affidabile:

- `Location` e `Content-Location` relativi/root-absolute/same-origin;
- `Set-Cookie Path=/` -> `Path=/site1/` e corretta gestione di Path assente;
- `Refresh` e `Link` soltanto con parser dedicato;
- nessun rewrite di URL esterni;
- protezione da doppio prefisso;
- risposta 404 dedicata per subdomain noto ma mount sconosciuto; non cadere nella pagina
  admin della porta unificata.

Questa modalità è production-grade soltanto se l'app/adapter genera già URL browser
corretti usando il prefisso.

### Fase 4 — Eventuale compatibility rewriter sperimentale

Se desiderato, può essere aggiunto separatamente e opt-in:

- allowlist di `Content-Type`;
- tokenizer HTML streaming, non regex;
- parser CSS;
- gestione esplicita di encoding, cache validator, CSP e SRI;
- limite di dimensione/tempo e comportamento fail-closed;
- header/metriche che segnalano quando una risposta non è stata trasformata.

Non deve dichiarare compatibilità universale, non deve riscrivere JavaScript/JSON alla
cieca e non deve essere il default. Un nome come `--content-rewrite=experimental` è più
corretto di `--transparent-root`.

## 6. Alternative

| Soluzione | App root-only | Affidabilità | Isolamento | Giudizio |
|---|---:|---:|---:|---|
| subdomain distinto per app | sì | alta | origin distinto | **raccomandata** |
| mount bore + app/adapter prefix-aware | sì, dopo adapter | alta | origin condiviso | valida se stesso host obbligatorio |
| solo strip prefix + header | no per app ignare | alta nel suo perimetro | origin condiviso | utile come feature core |
| rewrite HTML/CSS/JS generico | parziale | bassa | origin condiviso | sperimentale, non promessa |
| cookie/referer/connection routing | apparentemente | intrinsecamente ambiguo | origin condiviso | da rifiutare |
| iframe verso subdomain | spesso rotto da CSP/frame policy, OAuth e navigation | bassa | origin separato interno | non equivalente |

## 7. Matrice di test minima

### 7.1 Unit e protocollo

- normalizzazione mount e boundary match;
- query, percent-encoding, `%2f`, slash doppie, dot-segment, path troppo lunghi;
- collisioni exact/nested/root e cleanup RAII;
- reservation legacy esclusiva e reservation path;
- wire old-client/new-server, new-client/old-server fail-closed;
- URL/ack e chiavi direct route-specific;
- rewrite request target, `Location`, cookie Path e double-prefix.

### 7.2 HTTP integration

- site1 e site2 sullo stesso host, inclusa **una sola connessione keep-alive** che li
  richiede in sequenza;
- GET/HEAD/POST/PUT, body `Content-Length` e chunked, trailer;
- `100 Continue`, `103`, `204`, `304`, redirect relativi e assoluti;
- upload/download grandi senza buffering totale;
- SSE/streaming e client lento con backpressure;
- WebSocket HTTP/HTTPS e upgrade con prefix strip;
- cookie omonimi tra site1/site2;
- mount miss 404, max-conns 503, provider drop e reconnect;
- flush-before-read sul response path trasformato, preservando i gate esistenti.

### 7.3 Topologie bore

- HTTP dedicato, HTTPS dedicato e control-port unificata;
- TCP relay e `--udp`, `--carriers 1` e `N`;
- client nativo e SSH gateway;
- admin API/UI, metriche e access log distinti per route;
- cert/config hot reload;
- regressione completa legacy root-vhost byte/path-identica.

### 7.4 Se si dichiara supporto Odoo

Non basta uno stub. Per ogni versione dichiarata serve un'E2E browser contro Odoo reale:

- login/logout e due istanze contemporanee in tab distinti;
- caricamento asset senza richieste root sfuggite;
- navigation/deep link/refresh;
- RPC, upload, download/report e editor;
- websocket/notifiche su installazione multiprocess;
- service worker/offline se applicabile;
- link email, OAuth/payment callback e moduli custom rappresentativi;
- verifica cookie, storage e cache isolation.

Senza questa suite, la dicitura corretta resta “non supportato/best-effort”.

## 8. Impatto repository e complessità

File/aree certamente coinvolti:

- `src/shared.rs`: protocollo e ack;
- `src/main.rs`, `src/client.rs`: CLI/registrazione/direct context;
- `src/vhost.rs`: route model, dispatcher HTTP, rewrite affidabili;
- `src/server.rs`: dedicated/unified frontend e QUIC install;
- `src/sshgw.rs`: grammatica, permit, takeover, banner;
- `src/admin_api.rs`, `src/admin_views.rs`, `src/admin_ui/panels/vhost.js`;
- test vhost/SSH/UDP e harness;
- `README.md`, `docs/VHOST.md`, guida SSH e test matrix.

Valutazione qualitativa:

- sola struttura registry/CLI: media;
- proxy HTTP per-request production-grade: grande;
- prefix-aware header semantics + tutte le topologie: grande;
- root emulation generica: costo molto grande con affidabilità comunque limitata;
- profilo Odoo supportato: progetto separato e continuativo, version-coupled.

Questa non è una piccola estensione di `extract_subdomain`: cambia il frontend da router
per-connessione a reverse proxy L7 per-request per gli host mounted.

## 9. Decisione raccomandata

**GO** a una feature `--mount` se il suo contratto è:

> routing per-request sullo stesso subdomain, strip del prefisso, forwarded prefix,
> redirect/cookie rewrite e supporto WebSocket; l'applicazione deve essere prefix-aware
> o avere un adapter.

**NO-GO** alla promessa:

> qualunque app che funziona soltanto su `/` funzionerà identica sotto qualsiasi
> subpath grazie alla sola riscrittura bore.

Per il caso concreto Odoo:

1. soluzione più robusta: un subdomain per istanza;
2. se lo stesso host è requisito inderogabile: prima costruire/provare un adapter Odoo
   prefix-aware e un sidecar HTTP/WebSocket;
3. implementare poi il mount bore come trasporto/router generico, senza codificare Odoo
   nel core.
