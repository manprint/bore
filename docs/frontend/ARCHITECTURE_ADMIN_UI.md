# Analisi architettura — Admin UI (src/admin_ui)

## Sintesi

L'Admin UI è una singola SPA minimale (vanilla JS, ES modules) embedded come risorsa statica dal build script Rust (build.rs → admin_assets.rs) e servita dal server Rust. Architettura modulare: bootstrap (app.js), registro dei pannelli (registry.js), router hash-based (router.js), poller centralizzato (poller.js), helper UI (ui.js, modal.js), storage token (store.js) e pannelli in `src/admin_ui/panels/*.js` che consumano le API `/admin/api/v1/*` tramite `api.js`. Il progetto include test Node.js in `test/admin_ui` che usano `dom-stub.js` per emulare il DOM.

## Mappa dei file essenziali

- src/admin_ui/
  - index.html           — DOM root (#menu, #view, #login-overlay)
  - app.js               — bootstrap: buildSidebar, setupLoginOverlay, setupRouter, polling
  - registry.js          — elenco ordinato dei pannelli (single import point)
  - router.js            — hash router; expose refreshCurrent()
  - api.js               — apiGet(endpoint) (fetch + Authorization + 401 handling)
  - poller.js            — createPoller(refreshFn), DEFAULT_REFRESH_MS (testable)
  - store.js             — sessionStorage token & refresh interval helpers
  - ui.js                — helper puri e testabili (escapeHtml, badge, table, flagBadges, format)
  - modal.js             — openModal/closeModal, detailRows
  - style.css            — stili dell'interfaccia
  - panels/*.js          — pannelli: overview, tunnels, secret, vhost, vpn, certs, config, metrics
- build.rs               — bundle static assets (include_bytes!) → OUT_DIR/admin_assets.rs
- package.json           — `npm test` script per i test frontend (node --test ...)
- test/admin_ui/*        — test unitari (dom-stub.js + molte suite .test.js)

## Contract / pattern

Ogni pannello esporta un default object che soddisfa il seguente contratto:

{
  id: string,
  title: string,
  route: string,
  endpoint: string | null,
  refreshMs: number,      // 0 = no polling
  render(el, data, ctx): Promise<void>
}

Nota: `registry.js` è l'unico file che importa i pannelli — punto centrale per estendere l'interfaccia.

## Responsabilità (file → cosa fa)

- index.html
  - struttura DOM base: `<aside id="menu">`, `<main id="view">`, `#login-overlay`
  - carica `app.js` come module
- app.js
  - popula la sidebar (registry)
  - gestisce overlay login e evento global `bore:unauthorized`
  - inizializza router e poller
- registry.js
  - array ordinato di pannelli importati (il punto unico che accede ai pannelli)
- router.js
  - renderPanel(route): trova panel via registry, importa dinamicamente `api.js` se serve, chiama panel.render(view, data)
  - espone `refreshCurrent()` per il poller
- api.js
  - `apiGet(endpoint)`: fetch con header Authorization: Bearer <token>, `cache: 'no-store'`
  - su 401: `clearToken()` + dispatch `bore:unauthorized` + throw
- poller.js
  - `createPoller(refreshFn, timers = { setInterval, clearInterval })` — test-friendly
- store.js
  - gestione token in sessionStorage e refresh interval
- ui.js
  - funzioni pure: `escapeHtml`, `fmtBytes`, `fmtDuration`, `flagBadges`, `table`, `notesCell`, `badgeCell` — centralizza escaping e formati
- modal.js
  - modale di dettaglio; `detailRows()` lavora i campi con formatting smart
- panels/*.js
  - ogni pannello implementa `render(el, data)` con logiche di table/card/modal

## Diagrammi ASCII

### 1) DOM iniziale (index.html)

<body>
├─ <aside id="menu">         ← sidebar popolata da app.buildSidebar()
├─ <main id="view">          ← pannello renderizzato qui
├─ <div id="login-overlay">  ← login form (sessionStorage token)
└─ <script type="module" src="/admin/ui/app.js">

### 2) Diagramma ad alto livello (moduli & runtime)

Browser (index.html)
    |
    v
[app.js] ---------------------------+
 | buildSidebar()                    |
 | setupLoginOverlay()               |
 | setupRouter(registry)             |
 | setupPolling()                    |
 +-> registry.js (panels list)       |
 +-> poller.js (createPoller)        |
 +-> store.js (sessionStorage token) |
    |                                |
    v                                v
[router.js] --(renderPanel)-> [panel X]
    |                                |
    | (if panel.endpoint)             | imports ui.js, modal.js, usa panel.render()
    | dynamic import('./api.js')      |
    v                                |
[api.js] --fetch /admin/api/v1/...--> Server (Rust)
   (on 401 -> clearToken, dispatch 'bore:unauthorized')

### 3) Dipendenze tra file (grafo semplificato)

app.js
 ├─> registry.js
 ├─> poller.js
 ├─> router.js
 └─> store.js
router.js
 ├─> (dynamic) api.js
 └─> panels/*  (i pannelli importano ui.js/modal.js)
panels/*
 ├─> ui.js
 ├─> modal.js
 └─> poller.js (DEFAULT_REFRESH_MS)

### 4) Sequenza (bootstrap + primo render)

index.html -> carica app.js
app.js:
  -> buildSidebar()  (usa registry.js)
  -> setupLoginOverlay()  (check getToken())
  -> setupRouter(registry)  (-> router.renderPanel(initialRoute))
  -> setupPolling()  (arma poller per pannello attivo)
router.renderPanel(route):
  -> trova panel (registry)
  -> se panel.endpoint:
       await import('./api.js')
       data = await apiGet(panel.endpoint)
  -> await panel.render(viewEl, data, ctx)
  -> DOM update

Poller:
  createPoller(refreshCurrent) -> poller.start(ms)
  timer -> refreshCurrent() -> router.renderPanel(currentRoute) -> repeat

### 5) Flusso login / 401

1. apiGet() effettua fetch con Authorization header
2. server risponde 401
3. apiGet(): clearToken(); document.dispatchEvent('bore:unauthorized'); throw Error
4. app.js listener su `bore:unauthorized` mostra `#login-overlay`
5. user inserisce token -> setToken(token); overlay nascosto; router ricarica il pannello
6. richieste successive includono Authorization header

## Note su sicurezza e robustezza

- Escaping/XSS: `ui.js` fornisce `escapeHtml()` e il codice lo usa ampiamente. Tuttavia ci sono punti che usano `innerHTML` (con contenuti costruiti via `escapeHtml` o tramite elementi creati dal codice). Quando si aggiungono nuove concatenazioni `innerHTML`, preferire `textContent` o creare nodi DOM per ridurre il rischio XSS.
- Token in sessionStorage: comodo per SPA, ma esposto a XSS. Per massima sicurezza considerare cookie HttpOnly/secure con CSRF mitigations (richiede modifiche server-side).
- apiGet usa `cache: 'no-store'` (corretto: ogni poll colpisce il server, non cache intermedi)
- `poller.createPoller` è progettato testabile (timers iniettati) — buona pratica per unit test
- Modal overlay è appeso a `document.body` (così il polling che rigenera `#view` non lo distrugge)

## Test e harness

- Test folder: `test/admin_ui/` (unit tests Node). Contiene `dom-stub.js` per emulare DOM nel test environment.
- Test principali: `badges.test.js`, `config.test.js`, `detail.test.js`, `metrics-rate.test.js`, `modal.test.js`, `poller.test.js`, `vhost-parity.test.js`, `vpn-render.test.js`, ecc.
- Eseguire i test (repo root):

```bash
npm test
# equivale a: node --test "test/admin_ui/**/*.test.js"
```

Assicurarsi di avere una versione di Node compatibile con `node --test`.

## Integrazione build/Deploy con Rust

- `build.rs` cammina `src/admin_ui` e genera `OUT_DIR/admin_assets.rs` con `include_bytes!()` per ogni asset. Il binario Rust include questi bytes e serve `/admin/ui/*` come risorsa statica.
- Per provare l'UI servita dal server Rust: eseguire il server Rust (vedere README per argomenti):

```bash
cargo run --bin bore -- server <opzioni>
```

Per sviluppo rapido puoi anche servire `src/admin_ui` con un webserver statico (attenzione CORS e percorsi API).

## Raccomandazioni e punti di miglioramento

- Ridurre l'uso di `innerHTML`: preferire la creazione di nodi DOM e `textContent` quando possibile.
- Aggiungere un test smoke che verifichi che ogni pannello esporti `id/title/route/render` (smoke.test.js già esiste in parte).
- Integrare `npm test` nella CI per reggere regressioni frontend.
- Documentare formalmente il contract dei pannelli e fornire uno snippet di esempio per nuovi pannelli.
- Valutare storage token HttpOnly lato server se si desidera aumentare la protezione contro XSS (implica cambi server).

## FAQ rapide / ancore di codice

- Punto di ingresso UI: `src/admin_ui/index.html` → `src/admin_ui/app.js`
- Registro pannelli (ordine & punti di estensione): `src/admin_ui/registry.js`
- Router + refresh hook: `src/admin_ui/router.js` (`setupRouter`, `refreshCurrent`)
- API fetch + 401 handling: `src/admin_ui/api.js` (`apiGet`)
- Poller e default polling: `src/admin_ui/poller.js` (`DEFAULT_REFRESH_MS`)
- Helpers UI: `src/admin_ui/ui.js`
- Modal & formatting: `src/admin_ui/modal.js`
- Pannelli: `src/admin_ui/panels/*.js`
- Test: `test/admin_ui/` (es. `dom-stub.js`)

---

Se vuoi, posso:
- aggiungere un diagramma SVG o PNG generato automaticamente,
- estrarre un template `NEW_PANEL.md` per creare nuovi pannelli conformi al contract,
- aprire una PR con le modifiche consigliate (es. convertire punti `innerHTML` rischiosi).

