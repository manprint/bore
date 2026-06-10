# VPN_FULL_PLAN_V2 — Piano di sviluppo fase 2 della feature VPN

> Stato di partenza: branch `vpn`, commit `1a5df0b` (post-fix A0 waker yamux).
> Documento progettato per essere eseguito **linearmente** da un agente di coding IA
> (target: Sonnet 4.6), fase per fase, sottofase per sottofase, senza salti nel buio.
>
> **Obiettivo finale:** massima stabilità dell'applicazione, massima banda sfruttata
> sui path dati (direct e relay), minima latenza possibile.

---

## 0. Come usare questo documento

1. Le fasi vanno eseguite **in ordine** (1 → 7). Le sottofasi di una fase vanno
   eseguite in ordine, salvo dove esplicitamente marcate "indipendente".
2. Ogni sottofase termina SOLO quando tutti i punti della sua checklist "Done when"
   sono verificati. **Zero regressioni tollerate**: una sottofase che rompe un test
   esistente non è completa.
3. Gate di qualità da eseguire al termine di OGNI sottofase:
   ```bash
   cargo fmt --check
   cargo clippy --all-features --all-targets -- -D warnings
   cargo test --all-features
   ```
4. I test netns (`scripts/vpn_netns_test.sh`) richiedono `sudo`; vanno eseguiti al
   termine di ogni fase che tocca il data plane o l'host config (fasi 1, 2, 3, 5).
5. Ogni fase che cambia comportamento/API/invarianti aggiorna la documentazione
   indicata nella sottofase. I docs sono parte del deliverable.
6. Modello consigliato per l'implementazione: **Sonnet 4.6** per tutte le sottofasi;
   le sottofasi marcate `[mech]` (meccaniche) possono usare Haiku 4.5.

### 0.1 Invarianti da non violare MAI (estendono CLAUDE.md)

- **I-1** Mai `tokio::io::split` su un `mux::Stream` condiviso fra due task. Un
  `yamux::Stream` ha un solo slot waker: due task lo sovrascrivono e il perdente
  non viene mai svegliato (bug A0). Un substream = un task. Il relay VPN usa due
  substream unidirezionali (tag `0x01`/`0x02`) per questo motivo.
- **I-2** `HelloVpn`/`ConnectVpn` inviati **prima** dell'auth (yamux lazy-init).
- **I-3** Il relay server è AEAD-opaco: il server fa splice di ciphertext, mai
  plaintext IP (`tests/vpn_server_test.rs::vpn_relay_substream_is_opaque`).
- **I-4** La coda relay applica **backpressure** (await su piena), mai drop
  silenziosi (`link::RELAY_QUEUE`, `LinkSender::send_batch`).
- **I-5** Nonce AEAD = counter a 64 bit. **Mai due seal con stesso (key, counter)**.
  Quando più produttori condividono una chiave egress (carriers §4.1, multiqueue
  §4.2) il counter DEVE essere un unico `Arc<AtomicU64>` con `fetch_add`.
- **I-6** `NetConfig` RAII: ogni modifica host (route/nft/ip_forward) registra il
  revert; teardown in ordine inverso su Drop.
- **I-7** I client VPN drenano sempre il control stream dopo `VpnReady`
  (heartbeat ogni 500 ms + rilevamento morte server).
- **I-8** Wire format: i messaggi sono **serde_json** (`shared.rs::Delimited`).
  Nuovi campi su varianti esistenti SOLO con `#[serde(default)]` (vecchio peer →
  default; campo sconosciuto → ignorato). Mai rimuovere/rinominare campi.
- **I-9** Compatibilità: comportamento con `carriers = 1` e `--tun-queues 1` deve
  restare byte-per-byte identico al path attuale.

### 0.2 Stato attuale rilevante (verificato sul codice, 2026-06-10)

| Componente | Stato | Riferimento |
|---|---|---|
| Relay AEAD 2-substream | ✅ funziona, ~200 MB/s in docker | `src/vpn.rs::link` |
| GSO/GRO offload (Phase 6.2) | ✅ con fallback single-packet | `hostcfg::create_tun`, `bridge::run_uplink_offload` |
| Direct QUIC path | ❌ `VpnLink::Direct`/`make_direct` esistono ma mai usati | `src/vpn.rs:1709` |
| Broker UDP server-side | ⚠️ **STUB, non "già pronto"** (vedi sotto) | `src/vpn_server.rs:327-339, 655-697` |
| `--auto-reconnect` | ❌ flag parsato, mai usato | `src/main.rs`, `src/vpn.rs` |
| Route su reconnect | ❌ `ip route add` → EEXIST | `hostcfg_cmd::cmd_route_add` |
| Admin page VPN | ⚠️ riusa `Role::SecretProvider/Consumer` | `src/vpn_server.rs:291-301, 557-567` |
| `carriers` su relay VPN | ❌ campo wire inviato come `1`, ignorato | `HelloVpn.carriers` |
| Piattaforme | Linux only (`#[cfg(all(feature="vpn", target_os="linux"))]`) | `src/lib.rs:37-38` |

**⚠️ Correzione rispetto a VPN_FULL_PLAN_TODO.md §A1:** il TODO afferma che
`broker_vpn_udp()` server-side è "già pronto". **Falso.** Verificato sul codice:

1. `serve_vpn_listener` (`src/vpn_server.rs:334-338`) registra l'entry UDP con un
   canale `to_provider` fittizio: `let (tx, _rx) = mpsc::channel(4); tx` — il
   receiver è droppato subito. Il listener **non può ricevere alcun punch**.
2. `broker_vpn_udp` (`src/vpn_server.rs:689-695`) invia al connector
   `UdpPunch { peer: vec![], .. }` — lista candidati del listener **vuota**
   (commento nel codice: "Listener candidates unknown in Phase 4").
3. Non esiste sincronizzazione fra l'arrivo dell'offer del listener e quello del
   connector (race: il connector può offrire prima che il listener abbia
   pubblicato i propri candidati).

La Fase 1 include quindi il completamento del broker server-side (§1.1), modellato
sul pattern già funzionante dei secret tunnel (`src/secret.rs:123-141` registrazione
con canale reale; `src/secret.rs:260-299` select-arm che inoltra `UdpPunch` al
provider; `src/secret.rs:457-494` broker lato consumer).

### 0.3 Mappa delle decisioni progettuali prese in questo piano

| # | Decisione | Motivazione |
|---|---|---|
| DEC-1 | Path switch relay→direct = **restart controllato del bridge** (abort task uplink/downlink, respawn con halves Direct), TUN e NetConfig intatti | Hot-swap lock-free è complesso e rischioso; la perdita di pochi pacchetti in volo è accettabile (IP è best-effort) |
| DEC-2 | Morte del direct path a runtime = morte del link → gestita da auto-reconnect (Fase 2), che ritenta e ricasca su relay se UDP è bloccato | Evita doppio stato "relay di riserva caldo"; semplifica enormemente il ciclo di vita |
| DEC-3 | Il server invia `UdpPunch` a ENTRAMBI i lati solo quando possiede **entrambe** le offer (stato in `serve_vpn_connector`); timeout 10 s → `UdpUnavailable` | Elimina la race offer-before-offer; comportamento deterministico |
| DEC-4 | Reconnect VPN: loop locale in `vpn.rs` che riusa `reconnect::Backoff`, NON `reconnect::run` | `reconnect::run` ritenta su qualunque errore per sempre; la VPN deve distinguere errori fatali (VpnError: id duplicato, overlap, mismatch) da link persi. Non si tocca il modulo condiviso |
| DEC-5 | Reconnect = teardown completo + rebuild (TUN ricreata, NetConfig riapplicata) ad ogni tentativo | L'addressing Pool può assegnare un /30 diverso alla riconnessione; mantenere la TUN viva richiederebbe riconfigurazione condizionale fragile. `ip route replace` (§1 della Fase 0) rende il re-apply idempotente |
| DEC-6 | Counter nonce condiviso `Arc<AtomicU64>` fra carriers/queues; chiave unica per direzione (niente chiavi per-carrier) | Più semplice, sicuro (I-5), il ricevitore non verifica l'ordine dei counter (nessuna replay window in scope) |
| DEC-7 | Carriers relay: round-robin **per-datagram**, out-of-order accettato | IP è best-effort; il riordino è gestito dagli endpoint (TCP interno) |
| DEC-8 | Cross-platform v1 = **host-only mode** su macOS/Windows/Android (advertise vuoto, no gateway/NAT/MSS-clamp) | Gateway mode richiede NAT engine per-OS (pfctl/ICS); fuori scope. Host↔host copre l'80% dei casi d'uso |
| DEC-9 | Nuovo flag `--relay-only` per disabilitare i tentativi direct | Indispensabile per test deterministici e per ambienti dove UDP outbound è indesiderato |
| DEC-10 | Se in futuro verrà aggiunta replay protection (B1), la sliding window dovrà tenere conto del riordino per-datagram introdotto dai carriers (DEC-7): finestra ≥ 2 × (carriers × RELAY_QUEUE) | Annotato qui per non perderlo; B1 fuori scope di questo piano |

---

## FASE 0 — Fondamenta e fix di robustezza (A3, A4, D1, D4, D5)

Fix piccoli e indipendenti che spianano la strada alle fasi successive. Nessun
cambiamento wire. Tutte le sottofasi sono `[mech]` tranne 0.5.

### 0.1 — A3: `ip route replace` (prerequisito Fase 2)

**File:** `src/vpn.rs` (`hostcfg_cmd::cmd_route_add`, test snapshot a riga ~1032).

**Implementazione:**
- In `cmd_route_add` sostituire il token `"add"` con `"replace"`. `ip route replace`
  è idempotente: crea la route se assente, la sostituisce se presente (niente EEXIST).
- Il revert resta `cmd_route_del` (invariato).
- Aggiornare il test snapshot `cmd_route_add_snapshot` (atteso:
  `["ip","route","replace","10.0.0.0/24","dev","tun0"]`).

**Test nuovi:**
- `hostcfg_cmd::tests::cmd_route_replace_snapshot` (rinomina/aggiorna l'esistente).
- In `netconfig_apply_routes_only`: asserire che le chiamate usino `replace`.

**Done when:** gate ok; `scripts/vpn_netns_test.sh` Test 1–5 passano invariati.

### 0.2 — A4: revert `ip_forward` affidabile senza UID 0

**File:** `src/vpn.rs` (`hostcfg::NetConfig::{apply, Drop}`), `docs/vpn/VPN.md`.

**Implementazione:**
- In `Drop` (riga ~1448-1458): se `std::fs::write("/proc/sys/net/ipv4/ip_forward", ...)`
  fallisce con `EACCES/EPERM`, tentare fallback
  `sh -c "echo <v> | sudo -n tee /proc/sys/net/ipv4/ip_forward"` via
  `std::process::Command` (best-effort, `sudo -n` = non interattivo). Loggare
  `warn!` con istruzione manuale se anche il fallback fallisce.
- Stessa logica in `apply` per l'enable (riga ~1331), usando il `CommandRunner`
  quando possibile per testabilità: aggiungere helper
  `hostcfg_cmd::cmd_sysctl_ip_forward(value: u8) -> Vec<String>` che costruisce
  `["sh","-c","echo <v> | sudo -n tee /proc/sys/net/ipv4/ip_forward"]` e usarlo
  come fallback se la scrittura diretta fallisce.
- Documentare in `docs/vpn/VPN.md` (sezione requisiti): comportamento con
  CAP_NET_ADMIN senza root, riga sudoers consigliata.

**Test nuovi:** snapshot del nuovo builder in `hostcfg_cmd::tests`.

**Done when:** gate ok; netns suite invariata; doc aggiornata.

### 0.3 — D1: warn `TooLarge` persistente dopo 10 s

**File:** `src/vpn.rs` (`bridge::run`).

**Implementazione:**
- Nel task `stats_task` di `bridge::run` (riga ~2083): registrare
  `let start = tokio::time::Instant::now();` e un flag `warned: bool`.
  Ad ogni tick (10 s): se `tx_drops > 0 && start.elapsed() > 10s && !warned`,
  emettere **una sola volta**:
  `warn!(tx_drops, "VPN link is dropping oversized packets; consider lowering --mtu (current path MTU is smaller than the TUN MTU)")`.
- Nessuna modifica a uplink/downlink (i contatori esistono già).

**Test nuovi:** unit `bridge::tests::toolarge_warn_logic` — estrarre la decisione in
una funzione pura `fn should_warn_drops(drops: u64, elapsed: Duration, warned: bool) -> bool`
e testarne la truth table (0 drops → no; <10 s → no; già warned → no; altrimenti sì).

**Done when:** gate ok.

### 0.4 — D4: `VpnLeaseGuard::drop` senza perdita di lease

**File:** `src/vpn_server.rs` (`VpnPool`, `VpnLeaseGuard`, `serve_vpn_connector`),
`tests/vpn_server_test.rs`.

**Implementazione:**
- Cambiare `Arc<tokio::sync::Mutex<VpnPool>>` in `Arc<std::sync::Mutex<VpnPool>>`.
  Le sezioni critiche sono brevissime e mai attraversate da await:
  - `serve_vpn_connector` riga ~450-453: `pool_arc.lock().await` →
    `pool_arc.lock().unwrap()` (lock preso e rilasciato nello stesso statement block,
    nessun await nel mezzo — verificare!).
  - `VpnLeaseGuard::drop` riga ~125-131: `try_lock` → `lock().unwrap()` (un Mutex
    std non può deadlockare qui: nessun altro percorso tiene il lock attraverso await).
- Propagare il tipo in `server.rs` dove il pool viene costruito (cercare
  `Mutex::new(VpnPool` / `vpn_pool`).
- Aggiornare i test esistenti che costruiscono il pool
  (`vpn_lease_guard_drops_cleanly`, `vpn_lease_guard_disarm_prevents_drop`,
  `vpn_pool_exhaustion`, ecc.).

**Test nuovi:**
- `vpn_lease_guard_frees_under_contention`: thread A tiene il lock per 50 ms,
  thread B droppa il guard nel frattempo → al rilascio il blocco DEVE risultare
  libero (con il vecchio `try_lock` questo test fallirebbe).

**Done when:** gate ok; tutti i 32 test di `vpn_server_test.rs` passano.

### 0.5 — D5: deregistrazione UDP protetta da generation token

**Problema:** `VpnDeregister::drop` (`src/vpn_server.rs:222-228`) rimuove
`udp_providers["vpn:{id}"]` incondizionatamente. Se un listener si ripareggia
(reconnect) e il Drop del vecchio handler gira DOPO la registrazione del nuovo,
l'entry UDP del nuovo link viene cancellata → candidati serviti al connector
sbagliato o per niente.

**File:** `src/vpn_server.rs`.

**Implementazione:**
- Aggiungere a `secret::UdpReg` un campo `pub session: u64` (o riusare `nonce` se
  già univoco per sessione — il nonce È univoco per pairing: usare quello, zero
  campi nuovi). Scelta: **confrontare il nonce**.
- `VpnDeregister` guadagna il campo `nonce: Option<[u8; UDP_NONCE_LEN]>`, settato
  quando l'entry UDP viene registrata (post-pairing).
- In `Drop`: `udp_registry.remove_if(&udp_id, |_, reg| Some(reg.nonce) == self.nonce)`
  (DashMap `remove_if`). Se il nonce non coincide, l'entry appartiene a una sessione
  più recente → non toccarla.
- Stessa protezione per `vpn_providers.remove(&self.id)`: il pairing già rimuove
  l'entry (`serve_vpn_connector` riga ~547); per il registry provider usare
  `remove_if` su un token univoco per registrazione (aggiungere a `VpnProviderEntry`
  un campo `session: u64` da un `AtomicU64` globale incrementale).

**Test nuovi (in `tests/vpn_server_test.rs`):**
- `vpn_deregister_does_not_remove_newer_session`: registra listener id=X (sessione 1),
  simula reconnect registrando sessione 2, droppa il deregister della sessione 1 →
  l'entry della sessione 2 DEVE sopravvivere.

**Done when:** gate ok; regressioni zero.

### 0.6 — Chiusura Fase 0

- Eseguire `scripts/vpn_netns_test.sh` completo (richiede sudo): Test 1–5 PASS.
- Aggiornare `docs/vpn/VPN_TEST_MATRIX.md`: nuove righe per i test di fase 0.
- Commit dedicato per la fase (messaggio: `vpn: phase 0 — robustness fixes (route replace, ip_forward fallback, TooLarge warn, lease guard, deregister race)`).

---

## FASE 1 — Direct QUIC path (A1 + D3) — il cuore del piano

Obiettivo: traffico peer-to-peer via QUIC datagrams con hole-punching, fallback
automatico su relay. Sblocca banda (niente colli di bottiglia sul server) e latenza
(un hop in meno). Le sottofasi 1.1→1.5 vanno in ordine stretto.

**Architettura finale (riassunto per orientarsi):**

```
listener                         server                          connector
   |                                |                                |
   |-- HelloVpn ------------------->|<-------------------- ConnectVpn|
   |<-------------------- VpnReady -|- VpnReady -------------------->|
   |                                |                                |
   |== relay bridge attivo (come oggi, 2 substream AEAD) ============|
   |                                |                                |
   |-- UdpCandidateOffer ---------->|<--------- UdpCandidateOffer ---|
   |        (server attende ENTRAMBE le offer, poi:)                 |
   |<-- UdpPunch{peer=cand_conn} ---|--- UdpPunch{peer=cand_list} -->|
   |                                |                                |
   |  DirectListener::new+accept    |        connect_direct          |
   |<================ QUIC diretto (datagrams) =====================>|
   |  bridge restart: Relay → Direct (DEC-1), log path="direct"      |
```

### 1.1 — Server: completare il broker UDP VPN

**File:** `src/vpn_server.rs` (`serve_vpn_listener`, `serve_vpn_connector`,
`broker_vpn_udp`).

**Implementazione (pattern di riferimento: `src/secret.rs:123-141, 260-299, 457-494`):**

1. **Canale reale verso il listener.** In `serve_vpn_listener`, alla registrazione
   dell'entry UDP post-pairing (riga ~327-339):
   ```rust
   let (to_provider_tx, mut to_provider_rx) = tokio::sync::mpsc::channel::<crate::secret::UdpOffer>(4);
   udp_providers.insert(udp_id.clone(), secret::UdpReg {
       candidates: vec![],
       selected_stun: None,
       nonce: pair_msg.nonce,
       to_provider: to_provider_tx,
   });
   ```
   Nel loop heartbeat del listener aggiungere un select-arm:
   ```rust
   Some(offer) = to_provider_rx.recv() => {
       // Il connector ha offerto candidati: inoltra il punch al listener.
       control.send(ServerMessage::UdpPunch {
           nonce: pair_msg.nonce,
           peer: offer.candidates,
           peer_selected_stun: offer.selected_stun,
           tuning: _udp_tuning,
       }).await?;
   }
   ```
   (verificare il tipo esatto di `UdpOffer` in `secret.rs:77-93`; se contiene già
   candidates+selected_stun riusarlo, altrimenti definire un tipo locale).

2. **Punch differito finché mancano le offer (DEC-3).** In `serve_vpn_connector`,
   sostituire la chiamata immediata a `broker_vpn_udp` con una piccola macchina a
   stati nel loop principale:
   - Stato: `connector_offer: Option<UdpCandidateOffer>`, `punched: bool`.
   - All'arrivo di `UdpCandidateOffer` dal connector: salvarla.
   - Ad ogni tick (500 ms) e all'arrivo dell'offer: se `!punched`, leggere l'entry
     `udp_providers["vpn:{id}"]`; se `entry.candidates` non vuoto E
     `connector_offer.is_some()`:
     a. inviare al **connector**: `UdpPunch { nonce, peer: entry.candidates, peer_selected_stun: entry.selected_stun, tuning }`;
     b. inviare al **listener** via `entry.to_provider`: l'offer del connector;
     c. `punched = true`.
   - Timeout: se dopo **10 s** dall'arrivo dell'offer del connector il listener non
     ha ancora candidati → inviare `UdpUnavailable` al connector e `punched = true`
     (il connector resta su relay). Il listener gestisce l'assenza di punch con il
     proprio timeout client-side (§1.3).
   - Mantenere il comportamento attuale (entry mancante → `UdpUnavailable`).

3. **Nessuna modifica wire**: `UdpCandidateOffer`, `UdpPunch`, `UdpUnavailable`
   esistono già (`shared.rs:362-368, 741-765`).

**Test nuovi (in `tests/vpn_server_test.rs`, senza TUN — solo protocollo):**
- `vpn_broker_punches_both_sides_when_both_offers_present`: pairing completo con
  client fittizi sui control stream; listener invia offer con candidato
  `203.0.113.1:1000`, connector invia offer con `203.0.113.2:2000`; asserire che il
  connector riceve `UdpPunch{peer=[203.0.113.1:1000]}` e il listener riceve
  `UdpPunch{peer=[203.0.113.2:2000]}` con lo stesso nonce della `VpnReady`.
- `vpn_broker_waits_for_listener_offer`: connector offre subito, listener offre
  dopo 1 s → punch arriva comunque (entro 2 s).
- `vpn_broker_timeout_sends_unavailable`: il listener non offre mai → il connector
  riceve `UdpUnavailable` entro ~10 s (usare `tokio::time::pause`/`advance` se
  possibile, altrimenti abbassare il timeout via parametro/costante visibile ai test).

**Done when:** gate ok; i 3 test nuovi e i 32 esistenti passano.

### 1.2 — Client: ristrutturare il control-stream in un attore (prerequisito per offer/punch)

**Problema:** oggi `run_bridge_with_ctrl` (`src/vpn.rs:208-246`) sposta `ctrl` in un
task drainer chiuso. Per il direct path il client deve INVIARE (`UdpCandidateOffer`)
e RICEVERE (`UdpPunch`/`UdpUnavailable`) sul medesimo stream → serve un attore.

**File:** `src/vpn.rs`.

**Implementazione:**
- Nuova struttura nel modulo radice di `vpn.rs`:
  ```rust
  /// Eventi dal control stream verso la logica direct-path.
  enum CtrlEvent {
      Punch { nonce: [u8; UDP_NONCE_LEN], peer: Vec<SocketAddr>, peer_selected_stun: Option<String>, tuning: UdpDirectTuning },
      Unavailable,
  }
  ```
- Funzione `spawn_ctrl_actor(ctrl: Delimited<mux::Stream>) -> (mpsc::Sender<ClientMessage>, mpsc::Receiver<CtrlEvent>, JoinHandle<anyhow::Error>)`:
  - task unico proprietario di `ctrl` (I-1 non riguarda Delimited ma il principio
    "uno stream = un task" resta);
  - loop `select!` su: (a) `out_rx.recv()` → `ctrl.send(msg)`; (b) `ctrl.recv()` →
    `Heartbeat` ignorato, `UdpPunch`/`UdpUnavailable` → `event_tx.send(...)`,
    `Ok(None)`/`Err` → return errore "server closed the vpn control stream"
    (semantica identica all'attuale per la morte del server, I-7).
- `run_bridge_with_ctrl` diventa: spawn dell'attore + `select!` fra bridge e
  JoinHandle dell'attore (comportamento di teardown invariato rispetto a oggi).
- `run_listen`/`run_connect` passano l'`out_tx`/`event_rx` alla logica direct (§1.3/§1.4).

**Test nuovi:** unit in `vpn.rs` con coppia di stream in-memory (riusare il pattern
di `tests/vpn_relay_link_test.rs::mux_connect`):
- `ctrl_actor_forwards_punch_and_detects_close`: invia Heartbeat → nessun evento;
  invia UdpPunch → evento `Punch`; chiudi lo stream → il JoinHandle risolve con errore.

**Done when:** gate ok; `vpn_relay_link_test` e netns Test 1 passano (il refactor
non deve cambiare il comportamento relay-only).

### 1.3 — Client listener: gather → offer → accept_direct → switch

**File:** `src/vpn.rs` (`run_listen`), `src/main.rs` (flag `--relay-only`).

**Implementazione:**

1. **CLI:** aggiungere `--relay-only: bool` a `VpnListenArgs`/`VpnConnectArgs`
   (CLI in `main.rs` + struct in `vpn.rs`). Se attivo, saltare interamente i passi
   seguenti (comportamento odierno).

2. **D3 — wiring degli arg NAT (finalmente usati):** dopo `VpnReady`, spawn del
   task `direct_upgrade` (lato listener):
   ```rust
   // 1. socket (usa nat_udp_preferred_port; 0 = effimera)
   let socket = holepunch::bind_socket(args.nat_udp_preferred_port).await?;
   // 2. STUN chain (usa stun_server come override)
   let host = /* host estratto da transport::Endpoint::parse(&args.to) */;
   let targets = holepunch::resolve_live_stun_targets(host, port, args.stun_server.as_deref()).await?;
   // 3. candidati (usa upnp + try_port_prediction)
   let disc = holepunch::gather_candidates_from_stun_targets(&socket, &targets, args.upnp, args.try_port_prediction).await;
   // 4. offer al server via ctrl actor
   out_tx.send(ClientMessage::UdpCandidateOffer(UdpCandidateOffer {
       candidates: disc.candidates,
       selected_stun: disc.selected_stun.map(|s| s.requested),
   })).await?;
   ```
   (firme esatte verificate: `holepunch.rs:110, 344, 419`; il campo
   `selected_stun` di `UdpCandidateOffer` è `Option<String>` — `shared.rs:362-368`.)

3. **Attesa punch:** il task attende su `event_rx`:
   - `CtrlEvent::Punch { peer, tuning, .. }` →
     `let token = holepunch::derive_token(Some(&args.secret), &session_nonce);`
     `let dl = holepunch::DirectListener::new(socket, peer, tuning).await?;`
     `let conn = timeout(10s, dl.accept(token)).await??;`
   - `CtrlEvent::Unavailable` o timeout complessivo **15 s** senza punch → log
     `info!(path = "relay", "direct path unavailable; staying on relay")` e il task
     termina (relay resta attivo per sempre).
   - **Nota token:** stesso meccanismo dei secret tunnel (`derive_token`,
     `holepunch.rs:84`): entrambi i peer derivano lo stesso valore da
     `(secret, session_nonce)`; il `session_nonce` arriva dalla `VpnReady`.

4. **Switch del bridge (DEC-1):** vedi §1.5 per il meccanismo. Il task invia
   `(LinkSender, LinkRecver)` costruiti con `link::make_direct(conn)` sul canale
   di upgrade del bridge e logga
   `info!(link_id, path = "direct", "vpn path upgraded to direct QUIC")`.

**Test nuovi:** coperti da §1.6 (serve l'intera catena); qui solo
`cargo build` + clippy + test esistenti.

### 1.4 — Client connector: gather → offer → connect_direct → switch

**File:** `src/vpn.rs` (`run_connect`).

**Implementazione:** speculare a §1.3, con due differenze:
- al posto di `DirectListener::new + accept`:
  `let conn = holepunch::connect_direct(socket, punch.peer, token, punch.tuning).await?;`
  (firma: `holepunch.rs:1098`; consuma il socket).
- su `CtrlEvent::Unavailable` → resta su relay (identico al listener).

Fattorizzare il codice comune listener/connector in
`async fn direct_upgrade_task(side: DirectSide, args: DirectArgs, out_tx, event_rx, upgrade_tx)`
con `enum DirectSide { Listener, Connector }` — evita la duplicazione che già
affligge `run_listen`/`run_connect`.

**Done when (1.3+1.4 insieme):** gate ok; con `--relay-only` il comportamento è
identico a prima (netns Test 1 con flag aggiunto allo script in una variante).

### 1.5 — Bridge: upgrade path Relay → Direct (DEC-1)

**File:** `src/vpn.rs` (`bridge::run`).

**Implementazione:**
- Nuova firma:
  ```rust
  pub async fn run(
      dev: Arc<tun_rs::AsyncDevice>,
      sender: LinkSender,
      recver: LinkRecver,
      counters: Arc<BridgeCounters>,
      mtu: u16,
      offload: bool,
      mut upgrade_rx: tokio::sync::mpsc::Receiver<(LinkSender, LinkRecver)>,
  ) -> Result<()>
  ```
  (canale mpsc di capienza 1; i chiamanti relay-only passano un canale il cui
  sender è droppato subito — `recv()` su canale chiuso non risolve mai dentro un
  `select!` con `biased` ordering? NO: `recv()` su canale chiuso risolve `None`
  immediatamente. Gestire: `Some(pair)` → switch; `None` → disabilitare il ramo
  (`let mut upgrade_open = true;` e guard nel select).)
- Loop esterno:
  ```rust
  loop {
      let mut uplink = tokio::spawn(run_uplink(dev_up, sender, cntr_up, mtu, offload));
      let mut downlink = tokio::spawn(run_downlink(dev_dn, recver, cntr_dn, offload));
      tokio::select! {
          res = &mut uplink => { downlink.abort(); return flatten(res); }
          res = &mut downlink => { uplink.abort(); return flatten(res); }
          maybe = upgrade_rx.recv(), if upgrade_open => match maybe {
              Some((new_s, new_r)) => {
                  uplink.abort(); downlink.abort();
                  // attendi l'effettiva terminazione per non avere due lettori TUN
                  let _ = (&mut uplink).await; let _ = (&mut downlink).await;
                  sender = new_s; recver = new_r;
                  info!(path = "direct", "bridge switched to direct path");
                  continue; // respawn con i nuovi halves
              }
              None => { upgrade_open = false; continue; } // mai upgrade
          }
      }
  }
  ```
  Attenzione ai dettagli di ownership: `sender`/`recver` vanno ripresi dal task
  abortito — NON è possibile (il task li possiede). Soluzione: i task uplink/downlink
  restituiscono i loro halves? No: dopo lo switch i vecchi halves vanno **droppati**
  (relay substreams chiusi → il server chiude i relay task). Quindi: il loop tiene
  solo i NUOVI halves ricevuti dal canale; al primo giro usa quelli iniziali.
  Ristrutturare con `let (mut cur_sender, mut cur_recver) = (Some(sender), Some(recver));`
  e `take()` al momento dello spawn.
- Lo `stats_task` (e il warn D1) restano fuori dal loop (contatori cumulativi).
- **Vincolo GSO (importante):** in gateway mode con offload, i segmenti forwarded
  possono superare `max_datagram_size()` del path direct → `TooLarge` → drop
  contati in `tx_drops` (il warn D1 di Fase 0 copre la diagnosi; il clamp MSS
  protegge il TCP). Il test netns §1.6 DEVE coprire gateway-mode su direct.

**Test nuovi:**
- Unit `bridge::tests::upgrade_channel_closed_is_inert`: bridge con canale upgrade
  droppato → si comporta come oggi (usare link relay in-memory, chiudere l'ingress
  → il bridge deve terminare con errore, non hangare).
- `tests/vpn_relay_link_test.rs`: nuovo test `vpn_link_switches_relay_to_direct`
  SENZA TUN: non è possibile usare `bridge::run` senza device... → testare lo
  switch a livello di `LinkSender/LinkRecver`: pompare N pacchetti su relay,
  inviare l'upgrade su un canale fittizio non è testabile senza bridge. In
  alternativa (scelta pragmatica): coprire lo switch con il test netns end-to-end
  (§1.6) e con un unit test della sola logica di select/ownership estratta in una
  funzione `next_bridge_event(...)` se fattibile senza contorsioni. Non forzare
  unit test che richiedono un TUN finto.

**Done when:** gate ok; netns Test 1–5 passano (relay-only e default).

### 1.6 — F2: test end-to-end del direct path (netns)

**File:** `scripts/vpn_netns_test.sh`, `docs/vpn/VPN_TEST_MATRIX.md`.

**Implementazione — nuovi scenari nello script (dopo il Test 5 attuale):**
- **Test 6 (direct host↔host):** topologia attuale; avviare listener+connector
  SENZA `--relay-only`; attendere `path="direct"` nei log di entrambi (grep con
  timeout 20 s); `ping -c 10` overlay (0% loss); `iperf3 -u -b 200M` sul path
  direct; asserire throughput > soglia conservativa (es. ≥ 100 Mbit/s in netns).
- **Test 7 (fallback su relay):** bloccare UDP fra ns1 e ns2 (nft drop udp,
  NON verso il server — il punch fra peer deve fallire ma il control TCP vivere);
  avviare il link; attendere `UdpUnavailable` o timeout → grep `path = "relay"` /
  "staying on relay"; ping deve funzionare via relay.
- **Test 8 (direct gateway mode):** listener advertise `192.168.50.0/24` come il
  Test 2 attuale ma su path direct; verificare ping LAN + `iperf3` TCP attraverso
  il gateway (copre il vincolo GSO/TooLarge di §1.5); controllare che `tx_drops`
  non cresca indefinitamente durante iperf3 TCP (MSS clamp efficace).
- **Test 9 (relay-only flag):** `--relay-only` su entrambi → mai `path="direct"`
  nei log, ping ok.

**Aggiornare:** `VPN_TEST_MATRIX.md` con righe §F2; `docs/vpn/VPN.md` sezione
"Direct path" (come funziona, log attesi, troubleshooting `path=relay` persistente,
flag `--relay-only`, significato di `UdpUnavailable`).

**Done when:** Test 1–9 netns PASS; gate ok; doc aggiornate.

### 1.7 — Chiusura Fase 1

- Regressione completa: `cargo test --all-features` + netns 1–9.
- `CLAUDE.md`: aggiornare la riga sull'invariante UDP/VPN (il direct path VPN ora
  esiste: "VPN: direct QUIC path con fallback relay; switch = bridge restart").
- Commit dedicato.

---

## FASE 2 — `--auto-reconnect` funzionante (A2)

Dipende da: Fase 0 (§0.1 route replace), Fase 1 (struttura ctrl actor + bridge).

### 2.1 — Classificazione errori: fatale vs ritentabile

**File:** `src/vpn.rs`.

**Implementazione:**
- Nuovo tipo marcatore:
  ```rust
  /// Errore di configurazione non ritentabile (il retry darebbe lo stesso esito).
  #[derive(Debug, thiserror::Error)]
  #[error("{0}")]
  pub struct FatalVpnError(pub String);
  ```
  (se `thiserror` non è già dipendenza, implementare Display/Error a mano —
  verificare `Cargo.toml`; non aggiungere dipendenze per 10 righe).
- Avvolgere in `FatalVpnError` SOLO: `VpnError` ricevuti prima del bridge
  (id duplicato, pool esaurito, overlap, mode mismatch, static mismatch,
  "server has no vpn pool", "vpn-max-links"), fallimento `check_root`,
  `'ip' command not found`, errori di parsing argomenti.
- TUTTO il resto (errore di connect TCP/TLS, server chiuso, bridge caduto,
  control stream perso, accept_relay timeout) = ritentabile.
- **Eccezione deliberata:** "vpn id already in use" È ritentabile quando arriva
  durante un reconnect (la vecchia sessione server-side può impiegare qualche
  secondo a morire) → classificarlo ritentabile, MA loggare warn. Documentare.

**Test nuovi:** unit `vpn::tests::fatal_classification` su una funzione pura
`fn is_fatal(err: &anyhow::Error) -> bool` (downcast su `FatalVpnError`).

### 2.2 — Loop di riconnessione (DEC-4, DEC-5)

**File:** `src/vpn.rs` (`run_listen`, `run_connect`), `src/main.rs` (passare
`auto_reconnect` nelle arg struct — campo già presente nel CLI, aggiungerlo a
`VpnListenArgs`/`VpnConnectArgs` di `vpn.rs` che oggi NON lo hanno).

**Implementazione:**
- Rinominare i corpi attuali in `run_listen_once`/`run_connect_once` (invariati).
- Nuovo wrapper (unico, parametrizzato):
  ```rust
  pub async fn run_listen(args: VpnListenArgs) -> Result<()> {
      run_with_reconnect(args.auto_reconnect, || run_listen_once(args.clone())).await
  }

  async fn run_with_reconnect<F, Fut>(auto: bool, mut attempt: F) -> Result<()>
  where F: FnMut() -> Fut, Fut: Future<Output = Result<()>> {
      if !auto { return attempt().await; }
      let mut backoff = crate::reconnect::Backoff::new(); // 1s..32s, già testato
      loop {
          let started = tokio::time::Instant::now();
          match attempt().await {
              Ok(()) => return Ok(()), // uscita pulita (mai, oggi; futuro: shutdown)
              Err(e) if e.downcast_ref::<FatalVpnError>().is_some() => return Err(e),
              Err(e) => {
                  // Un tentativo vissuto >60s è stato "sano": riparti dal backoff minimo.
                  if started.elapsed() > Duration::from_secs(60) { backoff.reset(); }
                  let delay = backoff.next_delay();
                  warn!(error = %e, ?delay, "vpn link lost; reconnecting");
                  tokio::time::sleep(delay).await;
              }
          }
      }
  }
  ```
- **Teardown per-tentativo (DEC-5):** `run_*_once` già possiede TUN e `NetConfig`
  come locali → al ritorno (Ok o Err) il Drop li smonta. Verificare che il drop
  della TUN preceda la `stale_reclaim` del tentativo successivo (sequenziale nel
  loop: garantito). Con §0.1 (`route replace`) un eventuale residuo non blocca.
- **Interazione col direct path:** ogni tentativo riparte da relay e ritenta
  l'upgrade direct (nuovo nonce → nuovo token/chiavi). DEC-2: la morte del direct
  a runtime fa cadere il bridge → questo loop la gestisce.

**Test nuovi:**
- Unit: `run_with_reconnect` con closure contatore: fallisce 3 volte ritentabile
  poi `FatalVpnError` → esattamente 4 tentativi e ritorna Err; con `auto=false`
  → 1 tentativo. Usare `tokio::time::pause()` per non aspettare il backoff reale.

### 2.3 — F1/F3: smoke test reconnect nel netns

**File:** `scripts/vpn_netns_test.sh`.

- **Test 10 (server drop → reconnect):** avviare link con `--auto-reconnect` su
  entrambi i lati; verificare ping; `kill -9` del server in ns0; attendere log
  "vpn link lost; reconnecting" su entrambi; riavviare il server (stesso comando);
  attendere ri-pairing (`vpn link paired` di nuovo); ping di nuovo OK entro 90 s.
  Verificare ASSENZA di errori "File exists" nei log (regressione §0.1) e che
  `ip route` non contenga route duplicate.
- **Test 11 (errore fatale non ritenta):** secondo listener con stesso `--id` e
  `--auto-reconnect` mentre il primo è attivo → il processo DEVE uscire con codice
  ≠ 0 entro pochi secondi (niente loop infinito). *(Nota: "already in use" al primo
  tentativo assoluto è fatale solo se non siamo in un ciclo di reconnect — se la
  classificazione di §2.1 lo rende sempre ritentabile, sostituire questo test con
  un errore certamente fatale: overlap di advertise, come il Test 4 attuale ma con
  `--auto-reconnect`.)*

**Aggiornare:** `VPN_TEST_MATRIX.md` (§F1, §F3), `docs/vpn/VPN.md` (sezione
auto-reconnect: semantica, backoff, errori fatali), `docs/vpn/VPN_USER_FULL_GUIDE.md`.

**Done when:** netns 1–11 PASS; gate ok; doc aggiornate. Commit dedicato.

---

## FASE 3 — Admin page VPN (D2)

Indipendente dal data plane; richiede Fase 1 per il path report. Bassa complessità.

### 3.1 — Ruoli e campi admin

**File:** `src/admin.rs`, `src/vpn_server.rs`, `src/admin_http.rs`,
`src/shared.rs` (un messaggio nuovo), `src/vpn.rs` (invio path report).

**Implementazione:**
1. `admin.rs`: aggiungere varianti `Role::VpnListener` e `Role::VpnConnector`
   (Display: `"vpn-listener"`, `"vpn-connector"`). Aggiungere a `NewEntry`/`Entry`:
   `pub overlay: Option<String>` (es. `"172.30.0.1/30"`) e un
   `pub vpn_direct: AtomicBool` su `Entry` (default false = relay) con metodo
   `Registration::mark_vpn_direct()` (pattern identico a `mark_udp`,
   `admin.rs:224`). Propagare in `EntryView`/`snapshot()`.
2. `vpn_server.rs`: nelle due `admin.register(...)` (righe ~291 e ~557) usare i
   nuovi ruoli e popolare `overlay` (il connettore conosce l'overlay al pairing;
   il listener lo conosce solo dopo — registrare l'entry admin del listener DOPO
   il pairing, spostando la `register` sotto l'invio di `VpnReady`, oppure
   aggiornare il campo via `Registration`: scegliere lo spostamento, più semplice;
   l'intervallo pre-pairing resta visibile col ruolo ma senza overlay → accettabile:
   registrare con `overlay: None` subito e aggiungere
   `Registration::set_overlay(String)` con `Mutex<Option<String>>`. Scelta finale:
   **`set_overlay`** — il listener in attesa DEVE comparire nel pannello).
3. **Path report (wire, additivo — I-8):** nuova variante
   `ClientMessage::VpnPathReport { path: String }` (`"direct"` | `"relay"`).
   Il client la invia via ctrl actor dopo ogni switch (§1.5) e dopo il pairing
   (`"relay"` iniziale). Server: nei loop di `serve_vpn_listener`/`serve_vpn_connector`,
   su `VpnPathReport` → `registration.set_vpn_direct(path == "direct")`. Vecchio
   server: ignora la variante sconosciuta? **NO** — serde_json su enum esterna
   fallisce su variante sconosciuta. Quindi: il client invia `VpnPathReport` SOLO
   se il server ha dichiarato supporto. Veicolo: aggiungere a `VpnReady` il campo
   `#[serde(default)] pub admin_v2: bool` settato `true` dal server nuovo.
   Client: invia il report solo se `admin_v2`. (Vecchio client + nuovo server:
   campo in più ignorato in lettura? `VpnReady` è inviata dal server: il client
   vecchio deserializza una variante con campo extra — serde_json di default
   IGNORA i campi sconosciuti nelle struct varianti → ok.)
4. **Contatori di banda relay (server-side):** in `vpn_relay`
   (`vpn_server.rs:633-652`) sostituire `copy_bidirectional_with_sizes` con la
   stessa funzione MA incrementando l'`active` counter della `Registration`
   (già esiste: `Registration::active()`) e — per i byte — avvolgere gli stream in
   un wrapper `CountingStream` che incrementa due `Arc<AtomicU64>` (tx/rx) passati
   dalla registrazione admin. Esporre i totali in `EntryView` come `relay_tx_bytes`/
   `relay_rx_bytes`. Sul path direct il server non vede traffico: la pagina mostra
   `path=direct, bytes n/a (p2p)` — comportamento corretto e onesto.
5. `admin_http.rs`: render delle nuove colonne per i ruoli VPN: ID (`vpn:{id}`),
   overlay, path (direct/relay), bytes TX/RX relay, uptime.

**Test nuovi:**
- `tests/admin_test.rs` (o `vpn_server_test.rs`): pairing VPN completo →
  `AdminRegistry::snapshot()` contiene 2 entry con ruoli `VpnListener`/`VpnConnector`,
  overlay valorizzato, path `relay`; inviare `VpnPathReport{direct}` dal connector
  → snapshot riflette `direct` (F5).
- Snapshot HTML: estendere il test esistente di `admin_test.rs` per verificare che
  la pagina includa la sezione/righe VPN.

**Done when:** gate ok; F5 verde; `docs/vpn/VPN.md` sezione admin aggiornata;
commit dedicato.

---

## FASE 4 — Performance (C3 → C1 → C2) + benchmark

Ordine interno motivato: C3 introduce l'infrastruttura counter-atomico/multi-stream
che C1 riusa; C2 dipende dal direct path (Fase 1) ed è indipendente dalle altre due.

### 4.1 — C3: carriers multipli sul relay

**Obiettivo:** N coppie di substream relay, round-robin per-datagram (DEC-7),
per superare il limite RTT×finestra del singolo stream TCP del relay.

**File:** `src/vpn.rs` (link, args), `src/main.rs` (CLI), `src/shared.rs` (wire),
`src/vpn_server.rs` (negoziazione), `src/server.rs` (clamp `--max-carriers` se
presente — verificare il nome esatto del flag server con `grep max_carriers src/server.rs src/main.rs`).

**Wire (additivo, I-8):**
- `ConnectVpn` guadagna `#[serde(default = "default_carriers")] pub carriers: u16`
  (default fn → `1`). `HelloVpn.carriers` esiste già.
- `VpnReady` guadagna `#[serde(default = "default_carriers")] pub carriers: u16`:
  il server calcola `effective = min(hello.carriers, connect.carriers, server_max)`
  con `server_max` dal flag server esistente per i carriers (o nuovo
  `--vpn-max-carriers`, default 8, se quello esistente non è accessibile dal
  modulo vpn_server) e lo comunica a entrambi. Vecchio peer → campo assente →
  default 1 → comportamento identico (I-9).

**CLI:** `--carriers <N>` su `bore vpn listen|connect` (default 1, max 16 client-side).

**Client (`link`):**
- `make_relay` → `make_relay_multi(egress: Vec<mux::Stream>, ingress: Vec<mux::Stream>, keys)`:
  - **Egress:** un task `relay_writer` per substream (I-1), ciascuno con il proprio
    `mpsc::Receiver<Bytes>` di capienza `RELAY_QUEUE / n` (min 64). `LinkSender::Relay`
    diventa `{ txs: Vec<mpsc::Sender<Bytes>>, key: [u8;32], counter: Arc<AtomicU64>, rr: usize }`;
    `send_batch` fa seal con `counter.fetch_add(1, Relaxed)` (I-5, DEC-6) e invia
    round-robin (`rr = (rr + 1) % txs.len()`) **per-datagram**. Se UN canale è
    pieno → await su quello (backpressure conservata, I-4; niente skip-to-next:
    semplice e prevedibile).
  - **Ingress:** un task lettore per substream (I-1): ognuno fa il loop
    `take_frame`+`open` e push su un `mpsc::Sender<Bytes>` comune (capienza
    `RELAY_QUEUE`); `LinkRecver::Relay` diventa `{ rx: mpsc::Receiver<Bytes> }` e
    `recv_batch` drena fino a `BATCH_CAP` con `try_recv` dopo il primo `recv().await`.
    Un lettore che incontra EOF/errore chiude tutto (drop del proprio tx; quando
    TUTTI i tx sono droppati `recv()` → `None` → errore "relay ingress closed").
    **Attenzione:** con n=1 questo cambia la struttura interna del path attuale —
    accettato purché i test bulk (`vpn_relay_link_test`) restino verdi e il
    throughput non regredisca (vedi benchmark §4.4); l'alternativa (due code path)
    viola la manutenibilità. I-9 si intende a livello di wire e semantica.
- `connect_relay`/`accept_relay` → versioni `_multi(n)`: il connector apre `n`
  coppie taggate; **nuovo formato tag**: header a 3 byte
  `[STREAM_READY, tag, carrier_idx]` SOLO se `n > 1`; con `n == 1` header attuale
  a 2 byte (compatibilità bit-esatta, I-9). Il listener sa quante coppie aspettare
  da `VpnReady.carriers`. Timeout 60 s complessivo come oggi.
- Server: `vpn_relay` (`vpn_server.rs:633`) è già generico per-substream (ogni
  substream del connector apre uno substream verso il listener) → **zero modifiche
  al data plane server**. Solo negoziazione del campo `carriers` nelle due serve_*.

**Test nuovi:**
- `tests/vpn_relay_link_test.rs::vpn_relay_multi_carrier_bulk`: come il test bulk
  esistente ma con `carriers = 4`; 5 000 pacchetti per direzione; verifica di
  ricezione COMPLETA (set di seq, non sequenza ordinata — DEC-7 ammette riordino).
- `vpn_relay_multi_carrier_one_stream_dies`: chiudere 1 substream su 4 a metà →
  il link DEVE morire pulito con errore (no hang, no perdita silenziosa).
- Unit: counter monotono condiviso — 4 task × 1000 seal concorrenti → 4000 counter
  univoci (asserire con HashSet) (I-5).
- `vpn_server_test.rs::vpn_carriers_negotiation`: hello(4) + connect(2) →
  VpnReady.carriers == 2 su entrambi; hello senza campo (JSON grezzo costruito a
  mano per simulare un peer vecchio) → 1.

**Done when:** gate ok; netns Test 1–11 PASS (default carriers=1); nuovo netns
**Test 12**: host↔host `--relay-only --carriers 4` + iperf3 TCP: throughput ≥ del
caso carriers=1 (non-regressione; il guadagno reale si vede su WAN con RTT alto).

### 4.2 — C1: TUN multi-queue (`--tun-queues N`)

**File:** `src/vpn.rs` (hostcfg::create_tun, bridge), `src/main.rs` (CLI).

**Implementazione:**
- CLI: `--tun-queues <N>` su listen/connect, default 1, clamp [1, 8]; con N>1 su
  OS ≠ Linux → errore esplicito a parse-time (per ora tutto è Linux-only comunque).
- `hostcfg::create_tun` guadagna `queues: usize`:
  - `DeviceBuilder` con `.multi_queue(true)` quando `queues > 1` (API tun-rs 2.8.5:
    `multi_queue` è Linux-only; usare `.with(|opt| { #[cfg(target_os="linux")] opt.multi_queue(true); })`
    se il metodo non è esposto direttamente sul builder async);
  - code aggiuntive via `dev.try_clone()` (un clone = una queue fd aggiuntiva;
    verificare che `AsyncDevice::try_clone` esista in tun-rs 2.8.5 — in caso
    contrario costruire sync + clone + `AsyncDevice::from_fd`);
  - ritorno: `(Vec<AsyncDevice>, offload: bool)`; con `queues == 1` vettore di 1
    (path identico a oggi, I-9).
  - **Interazione offload:** GSO/GRO è per-fd; mantenere il probing attuale sulla
    prima queue e applicare lo stesso modo a tutte (se il probe offload fallisce,
    tutte single-packet).
- `bridge::run`: spawn di `queues` task uplink (uno per device, ciascuno con un
  clone di `LinkSender` — rendere `LinkSender` `Clone`: per Relay è
  `{txs: Vec<Sender>, key, counter: Arc<AtomicU64>}` tutti clonabili (l'`rr` locale
  per task va bene: il round-robin per-task resta valido); per Direct,
  `DirectConn` è già cheap-to-clone) e `queues` task downlink. **Downlink fan-out:**
  il `LinkRecver` è UNO (la fan-in mpsc di §4.1) → un solo task downlink "reader"
  che distribuisce? No: più semplice — `LinkRecver` diventa clonabile? mpsc Receiver
  non è clonabile. Soluzione: il downlink resta **1 task** che scrive sulla prima
  queue (`send_multiple` GRO già batcha; la scrittura TUN non è il collo di
  bottiglia tipico — il kernel RPS distribuisce). I task multipli servono
  sull'**uplink** (lettura TUN = dove il kernel distribuisce i flussi sulle code).
  Documentare questa scelta nel codice. Se il benchmark §4.4 mostrasse il downlink
  come collo, evolvere in seguito (fuori scope).
- Select di supervisione: il primo task (di N+1) che muore abbatte il bridge
  (raccogliere gli handle in `FuturesUnordered` o vettore + `select_all`).
- Upgrade direct (§1.5): lo switch ora deve abortire/respawnare N+1 task — la
  ristrutturazione del loop di §1.5 va parametrizzata su un
  `spawn_pumps(devs, sender, recver) -> Vec<JoinHandle>` per non duplicare logica.

**Test nuovi:**
- Unit: `LinkSender` clonato in 4 task — counter univoci (estende il test §4.1).
- netns **Test 13**: host↔host `--tun-queues 4` (relay e direct): ping + iperf3
  `-P 4` (4 flussi paralleli) OK; throughput ≥ caso single-queue.

**Done when:** gate ok; netns 1–13 PASS; default (`--tun-queues 1`) byte-identico.

### 4.3 — C2: PMTU dinamico sul path direct

**File:** `src/vpn.rs`.

**Implementazione:**
- Funzione pura (testabile):
  ```rust
  /// Decide il nuovo MTU della TUN dato lo storico dei campioni QUIC.
  /// Ritorna Some(new_mtu) solo se: ultimo campione stabile per 3 poll consecutivi,
  /// diverso dall'MTU corrente di almeno 16 byte, e dentro [576, 9000].
  fn pmtu_decision(current_mtu: u16, samples: &[usize]) -> Option<u16>
  ```
- Task `pmtu_monitor`, avviato SOLO dopo lo switch a direct (§1.5 gli passa il
  `DirectConn` clone + `CommandRunner` + tun_name): ogni 5 s legge
  `conn.max_datagram_size()` (`holepunch.rs:1036`), accumula gli ultimi 3 campioni,
  su `Some(new_mtu)` esegue `ip link set <tun> mtu <new_mtu>` via
  `hostcfg_cmd::cmd_link_set_mtu` (esiste già, riga ~808) + `RealRunner`, logga
  `info!(old, new, "tun MTU adjusted to QUIC path MTU")`, aggiorna `current`.
- **Uplink MTU-agnostico:** `run_uplink_single` alloca `buf = vec![0; mtu+4]` una
  volta → con MTU dinamico andrebbe in overflow di lettura. Cambiare l'allocazione
  in `u16::MAX as usize + 4` fisso (costo: 64 KiB per task, trascurabile).
  L'offload path usa già buffer 65535 (ok).
- Il task muore col bridge (abort allo switch/teardown). Niente revert MTU: la TUN
  viene distrutta al teardown (DEC-5).
- MSS clamp: la regola nft usa `rt mtu` → si adatta da sola. Niente da fare.

**Test nuovi:**
- Unit `pmtu_decision`: tabella casi — campioni instabili → None; stabili uguali
  a current → None; stabili maggiori → Some; sotto 576 → clamp/None; delta < 16 → None.
- netns: opzionale/manuale (il PMTU in netns è statico); aggiungere a
  `VPN_TEST_MATRIX.md` come procedura manuale M-3 (WAN reale: verificare log
  "tun MTU adjusted").

**Done when:** gate ok; netns suite invariata.

### 4.4 — Benchmark e tuning pass

**File:** `scripts/vpn_bench.sh` (nuovo), `docs/vpn/VPN.md` (sezione Performance).

**Implementazione:**
- Script netns (riusa la topologia del test harness) che misura e stampa una
  tabella: {relay 1 carrier, relay 4 carriers, direct, direct 4 queues} ×
  {iperf3 TCP, iperf3 UDP 0.5/1/2 Gbit, ping -f loss/latency sotto carico}.
- Confronto con baseline pre-Fase-4 (annotare i numeri attuali PRIMA di iniziare
  la fase: relay ~200 MB/s docker — rimisurare in netns e scriverli nel doc).
- Tuning finale alla luce dei numeri: rivedere `RELAY_QUEUE` (512), `RECV_BUF`
  (128 KiB), `BATCH_CAP` (64), capienza fan-in §4.1. Cambiare SOLO se il benchmark
  mostra un miglioramento ≥ 5% riproducibile; ogni cambiamento documentato nel
  commit con i numeri.
- **Criterio di accettazione fase:** nessuna combinazione regredisce oltre il 5%
  rispetto alla baseline; direct > relay in throughput netns; relay 4-carriers ≥
  relay 1-carrier.

**Done when:** tabella numeri in `docs/vpn/VPN.md`; gate ok; commit dedicato.

---

## FASE 5 — E6: cross-platform (macOS, Windows, Android/Termux)

Scope deliberato (DEC-8): **host-only mode** su piattaforme non-Linux (advertise
vuoto; niente gateway/NAT/MSS-clamp/ip_forward). tun-rs 2.8.5 supporta già
Linux/macOS/Windows/Android/iOS/FreeBSD.

### 5.1 — Refactor di portabilità (nessun cambiamento funzionale su Linux)

**File:** `src/lib.rs`, `src/vpn.rs`, `Cargo.toml`.

**Implementazione:**
- `Cargo.toml`: spostare `tun-rs` da `[target.'cfg(target_os="linux")'.dependencies]`
  a dipendenza per `cfg(any(target_os="linux", target_os="macos", target_os="windows", target_os="android"))`
  (o semplicemente opzionale non-gated: tun-rs compila ovunque serva; scegliere il
  gating esplicito per non rompere FreeBSD/altro).
- `src/lib.rs:37-38`: `#[cfg(all(feature = "vpn", any(target_os = "linux", target_os = "macos", target_os = "windows", target_os = "android")))] pub mod vpn;`
- Dentro `vpn.rs`: il cfg di testa (riga 3) si allarga allo stesso set. I moduli
  `net`, `crypto`, `link`, `bridge` sono GIÀ portabili (nessuna syscall Linux).
  Le parti Linux-only da gating fine:
  - `hostcfg::create_tun`: il probing offload (`.offload(true)`, `tcp_gso`,
    `VIRTIO_NET_HDR_LEN`, `recv_multiple`/`send_multiple`, `GROTable`) →
    `#[cfg(target_os = "linux")]`; altrove `offload = false` sempre e
    `bridge::run_uplink_offload`/`run_downlink_offload` compilate solo su Linux.
  - multi-queue (§4.2): `#[cfg(target_os = "linux")]`, altrove errore CLI.
  - `check_root`: Linux/Android/macOS = `nix::unistd::getuid().is_root()`;
    Windows = check elevazione (token admin) — usare
    `std::process::Command::new("net").args(["session"])` exit-code come check
    pragmatico O la crate `is_elevated` se già nelle dipendenze transitive
    (verificare; preferire zero dipendenze nuove: il check `net session` basta).
  - `stale_reclaim`, `NetConfig`: vedi 5.2/5.3 per i comandi per-OS; su tutte le
    piattaforme la struttura RAII resta identica (I-6), cambiano solo i builder
    argv in `hostcfg_cmd` → introdurre
    `mod hostcfg_cmd { pub mod linux; pub mod macos; pub mod windows; }` con
    selezione `#[cfg]` e re-export, mantenendo i nomi delle funzioni attuali.
- **Guardia host-only:** su OS ≠ Linux, se `advertised` non è vuoto → errore
  fatale a runtime con messaggio chiaro ("gateway mode is Linux-only for now").
- Verificare: `cargo check --features vpn --target x86_64-pc-windows-msvc` (o gnu)
  e `--target aarch64-apple-darwin` da CI o cross (vedi 5.5).

**Test:** l'intera suite Linux DEVE restare identica (questo refactor non cambia
nulla su Linux). Unit test dei moduli portabili girano su macOS/Windows in CI (5.5).

### 5.2 — macOS (utun)

**File:** `src/vpn.rs` (hostcfg macos), `docs/vpn/VPN.md`.

**Implementazione:**
- TUN: su macOS il nome DEVE essere `utunN` o assente (il kernel assegna).
  In `create_tun`: se `tun_name` non matcha `^utun[0-9]+$`, ignorarlo con `warn!`
  e lasciar scegliere il kernel; loggare il nome effettivo (`dev.name()`).
  Builder: `.with(|opt| { #[cfg(target_os="macos")] opt.associate_route(false); })`
  — associate_route(false) perché le route le gestiamo noi via `NetConfig` (RAII,
  I-6; l'auto-route di tun-rs non sarebbe revertibile dal nostro guard).
- `hostcfg_cmd::macos`: `cmd_route_add(subnet, dev)` →
  `["route","-n","add","-net",subnet,"-interface",dev]`; del →
  `["route","-n","delete","-net",subnet,"-interface",dev]`; niente nft/iptables
  (host-only). `check_binary_exists("route")`.
- `stale_reclaim`: no-op (utun sparisce col processo; non esiste `ip link del`).
- MTU: tun-rs `.mtu()` funziona; `cmd_link_set_mtu` per il PMTU dinamico →
  `["ifconfig", dev, "mtu", mtu]`.

**Test nuovi:** unit `hostcfg_cmd::macos` snapshot dei builder (girano ovunque);
smoke manuale documentato (procedura M-4 in VPN_TEST_MATRIX.md: mac↔linux host-only,
ping + iperf3, path direct e relay).

### 5.3 — Windows (wintun)

**File:** `src/vpn.rs` (hostcfg windows), `docs/vpn/VPN.md`, justfile/release.

**Implementazione:**
- TUN: tun-rs su Windows richiede **`wintun.dll`** accanto all'eseguibile (o nel
  PATH) e privilegi admin. Builder: nome libero;
  `.with(|opt| { #[cfg(windows)] opt.ring_capacity(0x40_0000); })` (4 MiB ring,
  default adeguato per throughput).
- Errore amichevole se la dll manca: intercettare l'errore di `build_async` e
  arricchirlo: "wintun.dll not found — download from https://www.wintun.net and
  place it next to bore.exe".
- `hostcfg_cmd::windows`: route → `["route","ADD",net,"MASK",mask,gateway_o_0,"IF",if_index]`
  — più robusto via netsh: `["netsh","interface","ipv4","add","route",cidr,"interface_name"]`
  e delete speculare. MTU → `["netsh","interface","ipv4","set","subinterface",name,"mtu=<m>"]`.
  Scegliere **netsh** (sintassi CIDR nativa, niente if_index).
- `check_root` → check elevazione (5.1). `stale_reclaim`: best-effort no-op
  (wintun rimuove l'adapter alla chiusura del handle).
- Release: aggiungere nota/step che impacchetta `wintun.dll` (licenza permette
  redistribuzione? — **verificare licenza Wintun (MIT/GPL dual)**; in caso di
  dubbio NON ridistribuire e documentare il download manuale).

**Test nuovi:** unit snapshot `hostcfg_cmd::windows`; procedura manuale M-5
(win↔linux host-only). CI: build con feature vpn (5.5).

### 5.4 — Android / Termux

**File:** `docker/Dockerfile.android`, `justfile` (target `android-arm64`),
`docs/vpn/VPN.md` (+ nuova sezione Termux), opzionale `src/main.rs`/`src/vpn.rs`
(`--tun-fd`).

**Implementazione:**
- **Build:** estendere il target `android-arm64` del justfile per compilare con
  `--features vpn` (tun-rs supporta `target_os = "android"`; il kernel è Linux).
  Verificare che `nix`, `procfs` (se usata dal modulo vpn — grep) compilino su
  Android; `procfs` è gated `cfg(target_os="linux")` e Android NON è
  `target_os="linux"` in Rust → verificare ogni uso nel path vpn e gateare.
- **Runtime Termux (root):** documentare il runbook:
  1. dispositivo rootato, `pkg install tsu iproute2`, binario `bore-android-arm64`;
  2. `sudo bore vpn connect --to ... --relay-only|default` — root via `tsu`;
  3. `/dev/net/tun` presente su tutti i kernel Android moderni; `ip` è quello di
     `iproute2` Termux (toybox `ip` di sistema NON basta per route su alcune ROM —
     documentare);
  4. host-only mode (DEC-8); niente nft → nessun problema (host-only non usa NAT).
- **Android `check_root`:** uid 0 via `nix` funziona (5.1 lo copre con il cfg
  `any(linux, android, macos)`).
- **Senza root (opzionale, solo se a costo zero):** aggiungere flag
  `--tun-fd <RAW_FD>` che bypassa `create_tun` con
  `unsafe { tun_rs::AsyncDevice::from_fd(fd) }` — consente a un'app
  VpnService-based di exec-are bore passandogli l'fd. Saltare TUTTA la NetConfig
  (route gestite dalla VpnService). Gating: `cfg(any(target_os="android", target_os="linux"))`.
  Se l'implementazione supera le ~50 righe, rimandare e aprire un TODO in coda al
  piano: il valore c'è ma non deve far deragliare la fase.
- **Niente CI runtime per Android**: solo build (5.5) + procedura manuale M-6
  (Termux rooted ↔ linux: ping overlay, relay e direct).

### 5.5 — CI matrix

**File:** `.github/workflows/ci.yml`.

**Implementazione:**
- Nuovo job `vpn-cross-build`: matrix {`x86_64-pc-windows-msvc` (runner windows),
  `aarch64-apple-darwin` (runner macos), `aarch64-linux-android` (cargo-ndk o il
  Dockerfile esistente)} → `cargo check --features vpn --target ...` (check, non
  build completa, per tempi CI).
- Estendere i job macOS/Windows esistenti (righe 28–57, oggi solo transfer):
  aggiungere `cargo test --features vpn` limitato ai moduli portabili
  (`cargo test --features vpn vpn::crypto vpn::net vpn::link hostcfg_cmd`)
  sui runner macos/windows.
- Linux job: invariato (`--all-features` copre già vpn).

**Done when (fase intera):** CI verde su tutta la matrix; suite Linux invariata;
docs aggiornate (VPN.md: tabella supporto piattaforme con limitazioni esplicite,
runbook Termux, nota wintun.dll); `VPN_TEST_MATRIX.md` con procedure M-4/M-5/M-6;
commit per sottofase.

---

## FASE 6 — Consolidamento, test matrix, documentazione finale

### 6.1 — F6: esecuzione delle procedure manuali pendenti

- **16.5.4** `--no-route-manage`: avviare il link con il flag, copiare i comandi
  stampati, applicarli a mano, verificare ping; documentare l'esito nella matrix.
- **16.6.8** SIGKILL + stale reclaim COMPLETO: dopo `kill -9` verificare che
  `bore0` E la tabella nft siano rimasti; secondo avvio con stesso `--id` deve
  riclamare TUN + route + nft (non solo TUN). Se il reclaim delle route risulta
  incompleto, è già coperto da §0.1 (`route replace`) — verificarlo esplicitamente
  e annotarlo.
- Automatizzare 16.6.8 nel netns harness se fattibile (**Test 14**: estendere il
  Test 5 attuale con i check su route/nft post-SIGKILL).

### 6.2 — Aggiornamento documentazione finale

| Documento | Contenuto |
|---|---|
| `docs/vpn/VPN.md` | Direct path (§A1), auto-reconnect, carriers, multi-queue, PMTU, piattaforme, troubleshooting per `path=relay` persistente; rimuovere la nota "Phase 6.2 GSO/GRO deferred" |
| `docs/vpn/VPN_TEST_MATRIX.md` | Tutte le righe nuove (Test 6–14, M-3..M-6, unit nuovi); stato PASS aggiornato |
| `docs/vpn/VPN_USER_FULL_GUIDE.md` | Esempi CLI completi dei nuovi flag (`--relay-only`, `--carriers`, `--tun-queues`, `--auto-reconnect`, `--tun-fd`) |
| `CLAUDE.md` | Nuovi invarianti: I-5 (counter atomico condiviso), DEC-1/2 (path switch = bridge restart; direct death = reconnect), DEC-10 (replay window vs carriers) |
| `docs/vpn/VPN_FULL_PLAN_TODO.md` | Marcare A1–A4, C1–C3, D1–D5, E6, F1–F6 come risolti con data e riferimento commit |

### 6.3 — Regressione finale e chiusura

- `cargo fmt --check && cargo clippy --all-features --all-targets -- -D warnings && cargo test --all-features`
- `scripts/vpn_netns_test.sh` completo (Test 1–14) PASS.
- `scripts/vpn_bench.sh`: numeri ≥ baseline Fase 4.
- CI completa verde su tutta la matrix.
- Commit finale + tag interno di milestone.

---

## Appendice A — Riepilogo nuovi elementi wire (tutti additivi, I-8)

| Messaggio | Campo nuovo | Default (peer vecchio) | Fase |
|---|---|---|---|
| `ConnectVpn` | `carriers: u16` | 1 | 4.1 |
| `VpnReady` | `carriers: u16` | 1 | 4.1 |
| `VpnReady` | `admin_v2: bool` | false | 3.1 |
| `ClientMessage` | variante `VpnPathReport { path }` | inviata solo se `admin_v2` | 3.1 |
| header substream relay | 3° byte `carrier_idx` | presente solo se carriers>1 | 4.1 |

## Appendice B — Riepilogo nuovi flag CLI

| Flag | Comandi | Default | Fase |
|---|---|---|---|
| `--relay-only` | vpn listen/connect | off | 1.3 |
| `--carriers <N>` | vpn listen/connect | 1 | 4.1 |
| `--tun-queues <N>` | vpn listen/connect | 1 (Linux only) | 4.2 |
| `--tun-fd <FD>` | vpn connect (opzionale) | — | 5.4 |

## Appendice C — Mappa test finali (sintesi)

| Livello | Test | Fase |
|---|---|---|
| unit | route replace snapshot, sysctl fallback builder, should_warn_drops, lease contention, deregister generation | 0 |
| unit | ctrl_actor punch/close, fatal_classification, run_with_reconnect counts, counter atomico multi-task, pmtu_decision, hostcfg_cmd macos/windows snapshots | 1–5 |
| integration (no TUN) | broker both-offers / waits / timeout; carriers negotiation; multi-carrier bulk + 1-stream-dies; admin snapshot VPN + path report | 1, 3, 4 |
| netns (sudo) | Test 6–9 (direct, fallback, gateway-direct, relay-only), 10–11 (reconnect), 12 (carriers), 13 (multiqueue), 14 (SIGKILL reclaim completo) | 1, 2, 4, 6 |
| manuale | M-3 PMTU WAN, M-4 macOS, M-5 Windows, M-6 Termux, 16.5.4 | 4–6 |
| bench | vpn_bench.sh: 4 configurazioni × TCP/UDP/latency | 4.4 |
