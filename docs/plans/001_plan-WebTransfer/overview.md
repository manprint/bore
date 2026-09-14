# Web Transfer multippeer — Plan Overview

> **Status:** planning | **Authored:** 2026-09-14 by GPT-6 Astra
> **Folder:** `docs/plans/001_plan-WebTransfer/`
> **Executing this plan? Read [STATE.md](STATE.md) FIRST** — it is the only execution-state file: live position, progress board, environment, in-flight work and next action. Finish or revert any `OPEN` unit, run its §3 gates against §1/§7/§11, then open the named sub-phase before editing and close it only after all gates pass.

## Goal

Implementare `bore transfer web`: il comando crea una room effimera, stampa un link e mantiene soltanto il lease proprietario. Ogni browser che apre il link può annunciare file/cartelle locali, vedere le offerte degli altri e avviare manualmente un download. I payload passano prima su WebRTC DataChannel diretto e, quando ICE fallisce, su relay WebSocket cifrato end-to-end; il server coordina e conserva metadati, mai file o payload. La chiusura pulita del CLI invalida subito room, URL e trasferimenti; una caduta anomala consente resume proprietario per 60 secondi di default.

```text
A: bore transfer web → apre il link → seleziona una cartella nel browser.
B: apre lo stesso link → vede l'offerta → clicca download → riceve via WebRTC diretto.
C: apre il link con ICE/UDP indisponibile → clicca download → riceve via relay.
B e C possono pubblicare proprie offerte; B può annullare un transfer di cui è parte.
Prima di ogni click non parte alcun download. Chiudendo il CLI di A, tutto diventa invalido.
```

## Design decisions

Il messaggio dell'utente “tutti default” approva tutte le opzioni raccomandate presentate nel clarification gate; per questo non restano default impliciti.

| # | Decision | Consequence |
|---|----------|-------------|
| **D1 (user, all defaults)** | Il CLI possiede solo il lease; file e cartelle vengono scelti esclusivamente nei browser. | Nessun path/file API nel comando; il browser del creatore è un peer ordinario. |
| **D2 (user, all defaults)** | Tutti i browser della room hanno gli stessi permessi. | Ogni peer pubblica/ritira offerte proprie e scarica; solo owner CLI chiude la room. |
| **D3 (user, all defaults)** | Nessun download o resume automatico. | Soltanto i click **Scarica**, **Scarica tutto come ZIP** o **Riprendi** inviano `transfer.request`. |
| **D4 (user, all defaults)** | Fonte unica per offerta, nessuno swarm automatico. | Un recipient diventa fonte solo ripubblicando esplicitamente un proprio file locale. |
| **D5 (user, all defaults)** | Direct-first via WebRTC, relay WebSocket automatico. | Una RTCPeerConnection per transfer; nessun riuso di QUIC/holepunch nativo o nuova socket UDP. |
| **D6 (user, all defaults)** | Relay senza storage e con buffer limitati. | Il server accoppia due socket con ticket monouso e inoltra un frame alla volta. |
| **D7 (user, all defaults)** | E2EE applicativa su direct e relay; metadati visibili al server. | Room key nel fragment/sessionStorage; payload AES-256-GCM; niente chiavi in server/log/admin. |
| **D8 (user, all defaults)** | URL capability con member token e room key nel fragment. | Forma `/transfer/<room>#m=<member>&k=<key>`; fragment scrub immediato e Copy Link esplicito. |
| **D9 (user, all defaults)** | Owner token separato e grace anomalo 60 s. | Clean close distrugge subito; reconnect riprende la stessa room, mai una nuova silenziosa. |
| **D10 (user, all defaults)** | Manifest immutabile, firmato e con hash per chunk. | Cambio del file produce SOURCE_CHANGED e richiede ripubblicazione. |
| **D11 (user, all defaults)** | OPFS staging obbligatorio per il download. | Resume verificato; senza OPFS catalogo/upload funzionano ma download è disabilitato. |
| **D12 (user, all defaults)** | ZIP per singola offerta/fonte, mai room-wide. | zip.js streaming store/Zip64; file singolo grezzo; cartella/multifile con download ZIP esplicito. |
| **D13 (user, all defaults)** | Feature opt-in sul listener/host di controllo esistente. | `--web-transfer-base-url` abilita route same-origin; HTTPS salvo loopback HTTP. |
| **D14 (user, all defaults)** | Default ICE senza TURN. | STUN server derivato quando `--udp`, poi catena esistente; relay applicativo è fallback. |
| **D15 (user, all defaults)** | Limiti stretti e configurabili, rate relay per room. | Ogni allocazione ha cap e rollback; config totals separati dai gauge live. |
| **D16 (user, all defaults)** | Browser target Chrome, Edge, Firefox, Safari. | CI Chromium/Firefox/WebKit; Chrome/Edge branded e Safari reale in smoke release. |
| **D17 (user, all defaults)** | Dipendenze/pipeline frontend pin e dist committato. | Cargo incorpora `web/transfer/dist` e non richiede Node al build consumer. |
| **D18 (user, all defaults)** | Prima slice pubblica relay-only, poi cambio dichiarato a direct-first. | README segue solo ciò che ciascuna fase rende realmente usabile. |
| **D19 (user, all defaults)** | Nuove varianti native additive e server aggiornato richiesto. | Vecchi listener/sender restano byte-identici; client nuovo mostra errore upgrade/config chiaro. |
| **D20 (user, all defaults)** | Tutte le subfasi sono eseguite da GPT-5.6 Luna con self-review. | Il piano prescrive simboli, algoritmi, limiti, test e criteri Done senza scelte lasciate a Luna. |

## Open questions

none — all clarifications resolved; the user explicitly selected all recommended defaults.

## Architecture summary

Un registry `Arc<DashMap<RoomId, Arc<WebTransferRoom>>>` governa lease, peer, offerte e transfer con lock brevi e guard RAII. Il browser mantiene i `File`, firma i manifest e cifra i frame. Il server inoltra controllo/signaling e, nel fallback, ciphertext bounded. Owner/peer reaper usano tick più `last_recv`; ogni cleanup async cattura la room reale con `Weak` ed epoch.

## Interface

| Surface | Name | Type / values | Default | Notes |
|---------|------|---------------|---------|-------|
| CLI | `bore transfer web` | subcommand | — | crea lease, non seleziona file |
| CLI flag | `--to <HOST>` | endpoint | existing public server | stesso parsing/default degli altri transfer |
| CLI flag | `--secret <SECRET>` | string | none | auth server esistente |
| CLI flag | `--insecure` | bool | false | semantica TLS esistente |
| CLI flag | `--open` | bool | false | apre browser dopo stdout flush; failure non fatale |
| Server | `--web-transfer-base-url` | HTTPS URL; HTTP loopback | unset/disabled | niente path/query/fragment/userinfo |
| Server | `--web-transfer-stun` | repeat/comma `stun:HOST[:PORT]` | derived + existing chain | custom sostituisce default |
| Server | `--web-transfer-no-stun` | bool | false | conflitto con custom; host candidates restano |
| Server | `--web-transfer-max-rooms` | positive integer | 1024 | globale |
| Server | `--web-transfer-max-peers` | positive integer | 4096 | globale |
| Server | `--web-transfer-max-peers-per-room` | positive integer | 32 | per room |
| Server | `--web-transfer-max-offers-per-peer` | positive integer | 64 | per peer |
| Server | `--web-transfer-max-entries-per-offer` | positive integer | 10000 | manifest |
| Server | `--web-transfer-max-offer-bytes` | positive bytes | 1099511627776 | 1 TiB |
| Server | `--web-transfer-max-metadata-per-room` | positive bytes | 16777216 | 16 MiB |
| Server | `--web-transfer-max-metadata-total` | positive bytes | 268435456 | >= per-room |
| Server | `--web-transfer-max-transfers-per-peer` | positive integer | 8 | source e recipient |
| Server | `--web-transfer-max-relays` | positive integer | 256 | globale |
| Server | `--web-transfer-relay-rate` | bytes/s | 104857600 | 0 disabilita |
| Server | `--web-transfer-owner-grace` | seconds 5..600 | 60 | abnormal owner loss |
| HTTP | `/transfer/<room>` | GET/HEAD | enabled only | generic shell, existence hidden until WS auth |
| HTTP | `/transfer/assets/*` | GET/HEAD | enabled only | embedded self-only assets |
| WebSocket | `/transfer/ws/control/<room>` | subprotocol `bore-transfer-v1` | enabled only | exact Host/Origin |
| WebSocket | `/transfer/ws/relay/<room>/<transfer>` | binary relay | fallback only | role ticket, one-use, opaque |

Ogni server flag ha variabile omonima `BORE_WEB_TRANSFER_*`. Tutti i cap sono >0 salvo il rate con zero esplicitamente ammesso; usare checked conversions e fallire prima del bind.

## Protocol and data-structure changes

| Change | Shape | Backward-compat strategy |
|--------|-------|--------------------------|
| Native owner wire | `CreateWebTransferRoom`, `ResumeWebTransferRoom`, `CloseWebTransferRoom`; created/resumed replies | varianti aggiunte in coda; vecchie fixture byte-identiche; upgrade server richiesto |
| Browser control v1 | bounded JSON envelope `{v,type,requestId?,body}` over WebSocket | exact subprotocol/version; breaking change richiede v2 |
| Browser payload v1 | 16-byte AAD header + AES-256-GCM body, <=32 KiB message | version/magic/attempt strict; late attempt discarded |
| Manifest v1 | canonical JSON, HMAC-SHA-256, 1 MiB chunk roots | immutable offer; changed source invalidates |
| Room state | metadata-only registry + RAII permits/guards | opt-in `Option`; disabled server retains old paths |
| Persisted browser partial | IndexedDB `bore-transfer-v1` + OPFS generated paths | schema v1; no token/key; invalid room/offer purges |
| Admin JSON | additive optional config/metric fields | `#[serde(default)]`; null means feature disabled, zero gauge remains zero |
| Static assets | committed `web/transfer/dist` embedded by `build.rs` | Cargo consumer needs no Node; drift gate pins output |

## Phases

| Phase | File | Primary assignment | Shippable alone? |
|-------|------|--------------------|------------------|
| 0 — Fondazioni e contratti | [phase_01.md](phase_01.md) | `agent:gpt-5.6-luna` | no, additive scaffold |
| 1 — Registry e owner lease | [phase_02.md](phase_02.md) | `agent:gpt-5.6-luna` | no, internal lifecycle |
| 2 — HTTP/control/catalogo | [phase_03.md](phase_03.md) | `agent:gpt-5.6-luna` | no, internal browser harness |
| 3 — Relay E2EE e CLI | [phase_04.md](phase_04.md) | `agent:gpt-5.6-luna` | yes, relay-only single file |
| 4 — WebRTC e fallback | [phase_05.md](phase_05.md) | `agent:gpt-5.6-luna` | yes, direct-first single file |
| 5 — Cartelle e ZIP | [phase_06.md](phase_06.md) | `agent:gpt-5.6-luna` | yes, complete functional flow |
| 6 — Hardening e rilascio | [phase_07.md](phase_07.md) | `agent:gpt-5.6-luna` | yes, production quality |

Live status of every phase is in `STATE.md` §11, never duplicated here.

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| CLI transfer dispatch | `TransferCommand` and match | `src/main.rs:557-560`, `src/main.rs:1910-2027` |
| Native transfer behavior to preserve | listener/sender entry points | `src/transfer.rs:782-1048` |
| Existing manifest/frame lessons | transfer types and bounded frame constants | `src/transfer.rs:30-44`, `src/transfer.rs:254-344` |
| Existing resumable transfer | preflight/workers/verify/commit | `src/transfer.rs:1400-2760` |
| Native control connect | `open_carrier` sequence | `src/client.rs:1787-1818` |
| Bounded heartbeat | `CtrlBeat`, `beat_once` | `src/client.rs:2218-2260` |
| Endpoint/TLS parsing | `Endpoint::parse/connect` | `src/transport.rs:102-159` |
| Auth handshake | client/server authenticator | `src/auth.rs:50-81` |
| Wire enum extension | `ClientMessage`, `ServerMessage` | `src/shared.rs:1371`, `src/shared.rs:1773` |
| RAII cleanup | `Registration` patterns | `src/admin.rs:364-388`, `src/admin.rs:524-607` |
| HTTP parsing/security | one-request HTTP handler | `src/admin_http.rs:1-79`, `src/admin_http.rs:310-381` |
| HTTP/yamux demux | listener first-byte branch | `src/server.rs:1983-2001` |
| vhost/admin routing | `serve_control_http` | `src/server.rs:2048-2142` |
| Server ownership/defaults | `Server` fields/constructor | `src/server.rs:359-455`, `src/server.rs:590-690` |
| Asset embedding | recursive admin embed | `build.rs:35-89` |
| Admin config/metrics | `ConfigView`, `MetricsView`, API/UI | `src/admin_views.rs:447`, `src/admin_views.rs:596`, `src/admin_api.rs:855`, `src/admin_ui/panels/metrics.js:206` |
| WebSocket test utilities | existing support helpers | `tests/support/websocket.rs` |
| Native direct path to keep separate | `DirectConn`/`DirectListener` | `src/holepunch.rs:3198-3249`, `src/holepunch.rs:3875-3945` |

## References (external documentation consulted)

| # | What it settled | Source | Version / date |
|---|-----------------|--------|----------------|
| R1 | RTCPeerConnection/DataChannel and signaling roles | [W3C WebRTC 1.0](https://www.w3.org/TR/webrtc/) | Recommendation, 2025-03-13 |
| R2 | ordered/reliable channels preserve message boundaries | [RFC 8831](https://www.rfc-editor.org/rfc/rfc8831.html) | January 2021 |
| R3 | absent SDP max-message-size defaults to 64 KiB; obey peer max | [RFC 8841](https://www.rfc-editor.org/rfc/rfc8841.html) | January 2021 |
| R4 | ICE NAT traversal/check model | [RFC 8445](https://www.rfc-editor.org/rfc/rfc8445.html) | July 2018 |
| R5 | WebSocket upgrade, binary frame and private close-code rules | [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455.html) | December 2011 |
| R6 | HKDF SHA-256 and AES-GCM `additionalData`/IV semantics | [WebCrypto Level 2](https://www.w3.org/TR/WebCryptoAPI/) | Working Draft, 2025-04-22 |
| R7 | URL fragment is removed before dereference | [RFC 3986](https://www.rfc-editor.org/rfc/rfc3986.html) | January 2005 |
| R8 | OPFS `navigator.storage.getDirectory()` and writable files | [File System Standard](https://fs.spec.whatwg.org/) | Living Standard, 2026-03-15 snapshot |
| R9 | OPFS availability and secure-context requirement | [MDN StorageManager.getDirectory](https://developer.mozilla.org/en-US/docs/Web/API/StorageManager/getDirectory) | checked 2026-09-14 |
| R10 | save picker is limited and user-activation-bound | [MDN showSaveFilePicker](https://developer.mozilla.org/en-US/docs/Web/API/Window/showSaveFilePicker) | checked 2026-09-14 |
| R11 | DataChannel high/low-water backpressure | [MDN RTCDataChannel.bufferedAmount](https://developer.mozilla.org/en-US/docs/Web/API/RTCDataChannel/bufferedAmount) | checked 2026-09-14 |
| R12 | async header callback handshake and buffer config | [tokio-tungstenite 0.28.0](https://docs.rs/tokio-tungstenite/0.28.0/tokio_tungstenite/fn.accept_hdr_async_with_config.html) | 0.28.0 |
| R13 | streaming WritableStream ZipWriter and Zip64 | [zip.js API](https://gildas-lormeau.github.io/zip.js/api/classes/ZipWriter.html), [npm metadata](https://registry.npmjs.org/@zip.js/zip.js/latest) | 2.15.0, checked 2026-09-14 |
| R14 | Chromium/Firefox/WebKit projects and Chrome/Edge channels | [Playwright browsers](https://playwright.dev/docs/browsers), [npm metadata](https://registry.npmjs.org/@playwright/test/latest) | 1.63.0, checked 2026-09-14 |
| R15 | pinned deterministic bundler package | [esbuild npm metadata](https://registry.npmjs.org/esbuild/latest) | 0.28.2, checked 2026-09-14 |
| R16 | cross-platform default-browser opener | [webbrowser crate API](https://crates.io/api/v1/crates/webbrowser) | 1.1.0, published 2026-02-07 |
| R17 | sessionStorage tab/session lifecycle | [MDN sessionStorage](https://developer.mozilla.org/en-US/docs/Web/API/Window/sessionStorage) | checked 2026-09-14 |

No external point used by the plan remains `UNVERIFIED`.

## Invariants

- **I-WEB1:** server never persists/caches payload, plaintext, ciphertext or ZIP.
- **I-WEB2:** only an explicit recipient click creates/resumes a transfer.
- **I-WEB3:** browser permissions are equal; CLI owner is a separate lease role.
- **I-WEB4:** clean owner close is immediate; abnormal loss expires after authenticated grace/resume.
- **I-WEB5:** direct first, relay fallback within one TransferId and fresh attempt/key.
- **I-WEB6:** both paths carry AES-GCM payload; room key never reaches server.
- **I-WEB7:** original source only; no automatic swarm/republish.
- **I-WEB8:** memory, queues, objects, attempts, rates and descriptors are bounded.
- **I-WEB9:** old listener/sender and wire remain unchanged; variants are appended last.
- **I-WEB10:** browser direct is WebRTC, never native QUIC or another UDP endpoint.
- **I-WEB11:** only offer owner withdraws; only source/recipient cancels; members cannot close room.
- **I-WEB12:** async cleanup captures actual Arc/Weak plus epoch, never re-resolves a registry key.
- **I-WEB13:** bounded heartbeat send; server reaper checks `last_recv` on tick, not `timeout(recv)`.
- **I-WEB14:** configured totals and live metrics remain separate; zero available is visible.
- **I-WEB15:** log/admin omit secrets and private filenames/path/manifest/signaling/payload.
- Existing SSH jump, secret/public/vhost liveness, UDP buffer, fdlimit and path-report invariants in `AGENTS.md` remain green.

## Risk register

| Risk | Mitigation |
|------|------------|
| stale cleanup destroys a resumed/reused room | captured Weak + epoch + pointer-conditional removal; Phase 1 race tests |
| WebSocket/manifest DoS | pre-allocation limits, semaphores, token buckets and fuzz; Phases 2/6 |
| server learns/stores content | uniform E2EE frames, one-frame relay, filesystem/fd/log canaries; Phases 3/6 |
| nonce reuse across retry | fresh random AttemptId/key and attempt-bound callbacks; Phases 3/4 vectors |
| transfer starts before consent | instrumentation asserts zero request/RTC/relay/read before click; Phases 2–6 |
| WebRTC race sends before recipient ready | server requires both ready and attempt-bound path_commit; Phase 4 |
| mid-direct loss corrupts output | flush only verified chunks, stale-attempt rejection, fresh relay attempt; Phase 4 |
| ZIP consumes source/server memory | ZipWriter to bounded stream, store mode, no archive Blob; Phase 5 memory tests |
| browser API inconsistency | OPFS capability gate, folder fallback, three-engine CI plus release smoke; Phases 3/5/6 |
| fd exhaustion across services | web admitted-FD budget included in pre-bind reconcile; Phase 6 OS gate |
| old modes regress through shared listener/wire | narrow exact-host routing, additive variants, complete legacy/SSH regression each phase |
| docs describe future behavior | mandatory README subphase per phase and executable README acceptance gates |

## Verification summary

| Gate | Command | Where it runs |
|------|---------|---------------|
| build | `cargo build --all-features` | every phase |
| fmt | `cargo fmt --all -- --check` | every phase |
| lint | `cargo clippy --all-features --all-targets -- -D warnings` | every phase |
| Rust unit | `cargo test --all-features --lib` | every phase |
| frontend setup/check | `npm ci --prefix web/transfer && npm run check --prefix web/transfer` | Phase 0.2 onward |
| Rust web e2e | `cargo test --all-features --test web_transfer_test -- --test-threads=1` | Phase 1 onward |
| browser e2e | `npm run test:e2e --prefix web/transfer` | Phase 0.2 onward |
| asset drift | `npm run build --prefix web/transfer && git diff --exit-code -- web/transfer/dist` | Phase 0.2 onward |
| full regression | `cargo test --all-features -- --skip t_ssh_ --skip t_dmx_` | every phase |
| serial SSH regression | `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` | every phase |

**Acceptance:** T-WEB-MULTIPEER-FINAL proves A/B/C publish and explicit direct/relay downloads; T-WEB-NOAUTO proves no pre-click transfer; T-WEB-CANCEL-RESUME proves cancellation and explicit verified resume; T-WEB-ROOM-LIFE proves owner-close invalidation; T-WEB-NOSTORE proves relay persistence absence; T-WEB-E2EE proves payload confidentiality; T-WEB-ZIP proves per-offer streaming archive; T-WEB-LEGACY proves old modes unchanged.

**Run caveats:** Phase 0.1 has no npm workspace yet. Thereafter Node >=20 is required only for contributor frontend gates. Browser ports are dynamic and Rust web e2e is serial. Existing sudo/netns gates run serially when their host prerequisites exist. Commands match `STATE.md` §3.

## Model-assignment summary

| Phase | Sub-phases by assignment | Primary | Review gate |
|-------|--------------------------|---------|-------------|
| 0 | 0.1–0.4 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 1 | 1.1–1.5 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 2 | 2.1–2.7 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 3 | 3.1–3.8 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 4 | 4.1–4.5 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 5 | 5.1–5.5 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit |
| 6 | 6.1–6.6 → `agent:gpt-5.6-luna` | `agent:gpt-5.6-luna` | self-review every unit and final README |
