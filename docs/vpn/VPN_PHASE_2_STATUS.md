# VPN_PHASE_2_STATUS — Stato di esecuzione del piano VPN_FULL_PLAN_V2

> Data: 2026-06-11 · Branch: `vpn` · Commit di partenza: `1a5db0f` → commit finale di fase: vedi tabella
> Piano di riferimento: `docs/vpn/VPN_FULL_PLAN_V2.md` · Confronto: `docs/vpn/VPN_FULL_PLAN_TODO.md`

---

## 1. Riepilogo esecutivo

Delle 7 fasi del piano (0–6), **le fasi 0, 1, 2, 3 e 4 sono implementate al 100%**
sul piano del codice, con tutti i gate di qualità verdi (`cargo fmt --check`,
`clippy --all-features --all-targets -D warnings`, `cargo test --all-features`:
21 suite, ~200 unit + ~120 integrazione, zero regressioni). La **fase 5
(cross-platform) è implementata parzialmente** (groundwork + CI matrix; runtime
per-OS rimandato — motivazione in §4.1). La **fase 6 è completata** per la parte
documentale/consolidamento eseguibile in questa sessione.

**Limite operativo della sessione:** la suite netns (`scripts/vpn_netns_test.sh`)
e il benchmark (`scripts/vpn_bench.sh`) richiedono `sudo` interattivo, non
disponibile. Tutti i test netns nuovi (Test 6–14) sono **scritti e
sintatticamente validati** ma **mai eseguiti**. È il primo passo da fare a mano
(vedi §6).

| Fase | Contenuto | Stato | Commit |
|---|---|---|---|
| 0 | Robustezza (A3, A4, D1, D4, D5) | ✅ Completa | `351eda7` |
| 1 | Direct QUIC path (A1 + D3 + F2) | ✅ Completa (netns da eseguire) | `49783aa` |
| 2 | `--auto-reconnect` (A2 + F1/F3) | ✅ Completa (netns da eseguire) | `07598e0` |
| 3 | Admin page VPN (D2 + F5) | ✅ Completa | `3910299` |
| 4 | Performance (C3, C1, C2) + bench | ✅ Codice completo (bench da eseguire) | `20f7d07` |
| 5 | Cross-platform (E6) | ⚠️ Parziale (groundwork + CI) | `85ad3a4` |
| 6 | Consolidamento + docs | ✅ Completa (procedure manuali pendenti) | questo commit |

---

## 2. Cosa è stato fatto, nel dettaglio

### FASE 0 — Robustezza (commit `351eda7`)

- **A3 — `ip route replace`**: `hostcfg_cmd::cmd_route_add` ora emette `replace`
  (idempotente). Una route stantia da un run crashato o da un reconnect in corso
  non blocca più il setup con `EEXIST`. Test snapshot aggiornati.
- **A4 — revert `ip_forward` senza UID 0**: sia l'enable (in `NetConfig::apply`)
  sia il restore (in `Drop`) ora hanno fallback `sh -c "echo <v> | sudo -n tee
  /proc/sys/net/ipv4/ip_forward"`. L'enable fallisce con errore actionable se
  anche il fallback fallisce; il restore logga `warn!` con il comando manuale.
  Riga sudoers raccomandata documentata in `VPN.md`.
- **D1 — warn `TooLarge` persistente**: funzione pura `should_warn_drops(drops,
  elapsed, warned)` (truth-table testata) + warn one-shot nello `stats_task` del
  bridge dopo 10 s di drop persistenti, con suggerimento `--mtu`.
- **D4 — `VpnLeaseGuard` senza perdita di lease**: `VpnPool` è passato da
  `tokio::sync::Mutex` a `std::sync::Mutex` (nuovo alias `VpnPoolHandle`); il
  Drop ora **blocca** e libera sempre il /30 (il vecchio `try_lock` lo perdeva
  sotto contesa). Gestione del poisoning con `into_inner()`. Test di contesa a
  2 thread (`vpn_lease_guard_frees_under_contention`).
- **D5 — deregistrazione con generation token**: `VpnProviderEntry` ha un campo
  `session` (contatore globale atomico); l'entry UDP è protetta dal nonce di
  pairing. `VpnDeregister::drop` usa `remove_if`: il Drop di un handler stantio
  non può più cancellare la registrazione di una sessione più recente (race di
  reconnect). Due unit test dedicati.

### FASE 1 — Direct QUIC path (commit `49783aa`) — il cuore del piano

- **§1.1 — Broker UDP server-side completato** (era uno stub, come correttamente
  rilevato dal piano §0.2): `serve_vpn_listener` registra un canale `to_provider`
  REALE e ha un select-arm che inoltra il punch al listener; `serve_vpn_connector`
  implementa la macchina a stati DEC-3 (punch a **entrambi** i lati solo quando
  il server possiede **entrambe** le offer, ciascun peer riceve i candidati
  dell'altro con il nonce di pairing; timeout 10 s → `UdpUnavailable`). Timeout
  configurabile per i test via `Server::set_vpn_punch_timeout`. Zero modifiche
  wire (i messaggi esistevano già).
- **§1.2 — Attore del control stream**: `spawn_ctrl_actor` è l'unico proprietario
  dello stream dopo `VpnReady` (un substream = un task, I-1): drena gli
  heartbeat (I-7), inoltra `UdpPunch`/`UdpUnavailable` come `CtrlEvent`, scrive i
  `ClientMessage` in uscita (offer, path report), e il suo `JoinHandle` risolve
  con l'errore quando il server muore (semantica identica a prima).
- **§1.3/§1.4 — Upgrade direct fattorizzato**: un unico `direct_upgrade_task`
  parametrizzato su `DirectSide::{Listener,Connector}` (niente duplicazione):
  bind socket → catena STUN (override > pubblici > fallback bore server) →
  gather candidati → offer → attesa punch (budget 15 s) → `DirectListener::new`
  + `accept` (10 s) oppure `connect_direct` → consegna degli halves Direct al
  bridge. **D3 chiuso**: `--stun-server`, `--upnp`, `--try-port-prediction`,
  `--nat-udp-preferred-port` finalmente usati. Token = `derive_token(secret,
  nonce)` come i secret tunnel.
- **§1.5 — Switch del bridge (DEC-1)**: `bridge::run` ha un canale di upgrade;
  allo switch ferma le pompe, **attende la loro effettiva terminazione** (mai
  due lettori TUN), droppa gli halves relay (chiudendo i substream) e respawna
  su Direct. **Aggiunta non prevista dal piano**: finestra di grazia di 5 s —
  quando il PEER switcha per primo, i nostri pump relay muoiono mentre il nostro
  upgrade è in volo; senza la grazia, il `select!` poteva uccidere il link al
  50% dei casi (race reale individuata in implementazione).
- **Flag `--relay-only`** (DEC-9) su listen/connect: salta l'intero tentativo
  direct (niente socket UDP, niente STUN).
- **Test**: 3 test di integrazione del broker (entrambe le offer / offer
  ritardata / timeout), unit test dell'attore ctrl (con fix di un hang da
  lazy-open yamux nel test stesso), netns Test 6–9 scritti (direct host↔host,
  fallback con UDP bloccato, direct gateway-mode con copertura GSO/MSS, relay-only).

### FASE 2 — `--auto-reconnect` (commit `07598e0`)

- **§2.1 — Classificazione errori**: `FatalVpnError` (Display/Error a mano, zero
  dipendenze nuove). Fatali: privilegi mancanti, `ip` assente, `VpnError` di
  configurazione (overlap, mode mismatch, static mismatch, pool esaurito, no
  pool, max-links). Tutto il resto ritentabile. **Eccezione deliberata**
  documentata: `vpn id already in use` è ritentabile (la sessione server-side
  precedente può impiegare secondi a morire) con `warn!`.
- **§2.2 — Loop di riconnessione (DEC-4/DEC-5)**: `run_listen`/`run_connect`
  sono wrapper su `run_listen_once`/`run_connect_once` via `run_with_reconnect`
  (loop locale con `reconnect::Backoff` 1→32 s, NON `reconnect::run`). Ogni
  tentativo è teardown completo + rebuild (TUN ricreata, NetConfig riapplicata —
  resa idempotente da A3). Tentativo vissuto >60 s → backoff reset. L'upgrade
  direct viene ritentato a ogni riconnessione con nonce fresco (copre DEC-2).
- **Test**: truth table `fatal_classification`; `run_with_reconnect_counts` con
  tempo virtuale tokio (`start_paused`, aggiunta feature dev `test-util`);
  netns Test 10 (kill -9 server → reconnect → re-pair → ping, no EEXIST, no
  route duplicate) e Test 11 (errore fatale esce subito anche con il flag).

### FASE 3 — Admin page VPN (commit `3910299`)

- Ruoli dedicati `Role::VpnListener`/`VpnConnector` (prima riusavano
  SecretProvider/Consumer).
- `Entry` admin esteso: `overlay` (via `Registration::set_overlay`, così
  `NewEntry` resta invariato e nessun call site esistente è stato toccato),
  `vpn_direct` (AtomicBool), contatori `relay_tx_bytes`/`relay_rx_bytes`.
- **Wire additivo (I-8)**: variante `ClientMessage::VpnPathReport{path}` +
  campo `VpnReady.admin_v2` con `#[serde(default)]`. Il client invia il report
  SOLO se il server ha dichiarato `admin_v2` (un server vecchio fallirebbe la
  deserializzazione della variante sconosciuta — gestito esattamente come da
  piano). Report `"relay"` al pairing + `"direct"` dopo lo switch.
- **Contatori relay**: `CountingStream` (AsyncRead/AsyncWrite wrapper) sul
  substream del connector + `ActiveGuard` per substream. Il server conta solo
  ciphertext (I-3); sul path direct la pagina mostra `n/a (p2p)`.
- Pagina HTML: nuova sezione "VPN links" (ruolo, id, client, overlay, path,
  TX/RX relay, conns, uptime) con `fmtBytes`.
- Test F5 end-to-end via HTTP admin (`vpn_admin_entries_and_path_report`).

### FASE 4 — Performance (commit `20f7d07`)

- **§4.1 — C3, carriers multipli sul relay**:
  - Wire additivo: `ConnectVpn.carriers` e `VpnReady.carriers` con
    `#[serde(default = 1)]`; il server negozia `min(listener, connector,
    --max-carriers)`; peer vecchio → 1 → path identico (I-9, testato con JSON
    grezzo senza campo).
  - `LinkSender::Relay` → `{txs: Vec<Sender>, key, counter: Arc<AtomicU64>, rr}`:
    **counter nonce unico condiviso** (I-5/DEC-6), round-robin **per-datagram**
    (DEC-7), backpressure su coda piena (I-4, niente skip-to-next).
  - `LinkRecver::Relay` → fan-in `mpsc<Result<Bytes>>`: un reader task per
    substream (I-1) che decifra e pusha; un reader che muore pusha **l'errore**
    nel fan-in → il link muore pulito, mai degradazione silenziosa.
  - Header substream: 2 byte con n=1 (bit-esatto col v1), 3° byte
    `carrier_idx` solo con n>1.
  - Server data-plane: **zero modifiche** (il relay per-substream era già
    generico), solo negoziazione.
  - `LinkSender` è ora `Clone` (counter condiviso, cursore rr per-clone) —
    prerequisito del multiqueue.
- **§4.2 — C1, TUN multi-queue**: `--tun-queues N` (1–8, validato da clap);
  `create_tun` con `IFF_MULTI_QUEUE` + `try_clone()` per le code extra
  (API tun-rs 2.8.5 verificata sul sorgente); bridge ristrutturato con
  `spawn_pumps` (un uplink per coda + un downlink sulla prima coda, scelta
  documentata nel codice) e supervisione `select_all`; lo switch direct
  aborta/respawna N+1 pompe.
- **§4.3 — C2, PMTU dinamico**: funzione pura `pmtu_decision` (3 campioni
  stabili, delta ≥16, range [576, 9000], truth-table con 10 casi) + task
  `pmtu_monitor` avviato solo dopo lo switch a direct (campiona
  `max_datagram_size()` ogni 5 s, applica `ip link set mtu`, muore con la
  connessione QUIC). Uplink single-packet reso MTU-agnostico (buffer fisso
  64 KiB). MSS clamp invariato (`rt mtu` si adatta da solo).
- **§4.4 — Benchmark**: `scripts/vpn_bench.sh` (4 configurazioni × TCP/UDP/
  latenza, output markdown). **Non eseguito** (sudo); il tuning pass è
  rimandato finché non ci sono numeri (criterio del piano: cambi solo con
  miglioramento ≥5% riproducibile).

### FASE 5 — Cross-platform (commit `85ad3a4`) — PARZIALE

- **Fatto**: builder argv portabili `hostcfg_cmd::macos` (route/ifconfig per
  utun) e `hostcfg_cmd::windows` (netsh, sintassi CIDR nativa come da piano)
  con snapshot test; job CI `vpn-cross-build` (windows-msvc, apple-darwin,
  android via cargo-ndk) + unit test portabili sui runner nativi; tabella di
  supporto piattaforme in `VPN.md`.
- **Non fatto**: vedi §3.

### FASE 6 — Consolidamento (questo commit)

- Netns **Test 14** (16.6.8 completo: SIGKILL → verifica che nft/route stantii
  esistano → secondo avvio riclama TUN+route+nft, niente EEXIST, ping ok).
- `CLAUDE.md`: nuovi invarianti (counter atomico condiviso I-5, DEC-1/2/3,
  DEC-7/DEC-10 replay window vs riordino carriers, I-9 per carriers/queues).
- `VPN_FULL_PLAN_TODO.md`: item A1–A4, C1–C3, D1–D5, F1–F3, F5 marcati risolti
  con data e commit.
- Questo documento.

---

## 3. Cosa è rimasto fuori

1. **Esecuzione della suite netns (Test 1–14) e del benchmark** — richiedono
   `sudo` interattivo, non disponibile nella sessione. I Test 6–14 sono nuovi e
   **mai stati eseguiti**: vanno considerati non verificati end-to-end finché
   non girano. Le righe della test matrix sono marcate `PENDING (needs sudo run)`.
2. **Fase 5 runtime cross-platform (§5.1–§5.4)**: il refactor di portabilità
   (allargamento dei `cfg` di `lib.rs`/`vpn.rs`, gating fine di offload/
   multiqueue/procfs, `check_root` per-OS, selezione per-OS dei builder
   `hostcfg_cmd`), il supporto runtime utun/wintun, la gestione `wintun.dll`,
   il target Android con `--features vpn` nel justfile e il flag `--tun-fd`.
   **Motivazione**: su questa macchina non esiste un toolchain C per
   macOS/Windows — perfino `cargo check --target aarch64-apple-darwin` fallisce
   sulla build C di `ring`. Un refactor invasivo di ~3300 righe senza alcuna
   possibilità di compilarlo per i target di destinazione violerebbe il vincolo
   "zero regressioni" del piano. Il job CI `vpn-cross-build` aggiunto è il
   veicolo con cui iterare quella fase in sicurezza.
3. **Tuning pass §4.4** (`RELAY_QUEUE`, `RECV_BUF`, `BATCH_CAP`, capienza
   fan-in): rimandato per definizione — il piano consente cambi solo a fronte
   di benchmark riproducibili, che richiedono il punto 1.
4. **Procedure manuali** 16.5.4 (`--no-route-manage` a mano), M-3 (PMTU su WAN
   reale), M-4/M-5/M-6 (smoke macOS/Windows/Termux — dipendono dal punto 2).
5. **Fuori scope dichiarato dal piano** (per completezza): B1 replay
   protection, B2 AAD, B3 key rotation, E1 mesh, E2 IPv6, E3 NAT 1:1,
   E4 relay-over-UDP, E5 privilege drop, E7 PSK per-link, E8 rekey.

---

## 4. Criticità

### 4.1 Criticità di processo

- **Test netns mai eseguiti** (vedi §3.1): è la criticità principale. Il data
  plane direct, lo switch del bridge, il reconnect e il multiqueue sono coperti
  da unit/integration test a livello di protocollo e di link, ma il
  comportamento con TUN reale, NAT e iperf3 è verificato solo "by design".
- **Fase 5 non verificabile localmente**: qualunque futura iterazione
  cross-platform deve passare dalla CI (o da una macchina con i toolchain).

### 4.2 Criticità tecniche note (decisioni e trade-off)

- **Race switch relay→direct**: i due lati switchano in modo indipendente; chi
  switcha per primo chiude i substream relay e fa morire le pompe del peer.
  Mitigata con la finestra di grazia di 5 s in `bridge::run` (attesa
  dell'upgrade in volo prima di dichiarare morto il link). Costo: un teardown
  su link relay con tentativo direct ancora in corso può ritardare fino a 5 s.
  Non prevista dal piano; da osservare nel netns Test 6.
- **Niente retry direct in-sessione**: se l'upgrade fallisce, si resta su relay
  per sempre (il retry avviene solo via reconnect, DEC-2). È fedele al piano,
  ma un link long-lived dietro un firewall temporaneamente ostile non tornerà
  mai direct senza un drop del link.
- **Reconnect con pool**: l'overlay /30 può cambiare a ogni riconnessione
  (DEC-5 accettato dal piano). Client di lunga durata con applicazioni legate
  all'IP overlay devono usare lo static addressing.
- **Carriers: un carrier morto = link morto** (scelta deliberata per evitare
  degradazione silenziosa). Con `--auto-reconnect` il costo è un ciclo di
  reconnect completo anche per il fallimento di 1 substream su N.
- **Multiqueue: downlink single-pump**: la scrittura TUN resta su una sola coda
  (scelta documentata; il kernel RPS distribuisce). Se il benchmark mostrasse
  il downlink come collo di bottiglia, va evoluto (fuori scope, annotato).
- **PMTU monitor**: usa `max_datagram_size()` come MTU del TUN direttamente.
  Non sottrae overhead aggiuntivi (sul path direct i datagrammi QUIC portano
  l'IP packet raw, quindi è corretto), ma su path con MTU molto dinamico
  l'isteresi è solo il "3 campioni stabili + delta ≥16".
- **Contatori relay admin solo sull'entry del connector**: i byte relay sono
  misurati sul lato connector del relay (dove i substream vengono accettati);
  l'entry del listener mostra 0. Onesto ma asimmetrico; documentato in VPN.md.
- **`mem::forget` nel test multi-carrier** (`pair_multi`): leak deliberato nel
  processo di test per tenere vivo il control plane; innocuo ma da non imitare
  in codice di produzione.

### 4.3 Debito pre-esistente non toccato

- Nessuna replay protection sul relay (B1) — con i carriers il riordino
  per-datagram è ora strutturale: la futura sliding window DEVE essere
  dimensionata ≥ 2 × (carriers × RELAY_QUEUE) (DEC-10, ora anche in CLAUDE.md).
- AAD vuoto (B2) e nessuna key rotation (B3).

---

## 5. Possibili ulteriori ottimizzazioni

1. **Skip-to-next sul round-robin egress**: oggi una coda carrier piena blocca
   (semplice e prevedibile, da piano). Un fallback "prova la coda successiva se
   piena" potrebbe ridurre la latenza di coda sotto carico asimmetrico — da
   valutare SOLO con benchmark (rischio: riordino ancora maggiore).
2. **Downlink multi-pump** (§4.2): se `vpn_bench.sh` mostra il downlink
   CPU-bound, distribuire le scritture TUN su più code (richiede un fan-out
   dal `LinkRecver` unico o N recver — il fan-in attuale lo renderebbe
   relativamente semplice).
3. **Batching delle scritture relay**: `relay_writer` scrive frame-per-frame;
   coalescere più frame in una sola `write_all` (vectored I/O) ridurrebbe i
   syscall sul path relay ad alto pps.
4. **Retry periodico dell'upgrade direct** in-sessione (es. ogni 5 min con
   backoff) invece che solo al reconnect — utile per firewall UDP transitori;
   richiede attenzione al ciclo di vita di socket/nonce.
5. **Hot path AEAD**: `crypto::seal/open` ricostruiscono `LessSafeKey` per ogni
   pacchetto (`UnboundKey::new` per frame). Cache della chiave per direzione
   (costruita una volta) è probabilmente il singolo guadagno CPU più facile del
   data plane relay.
6. **Buffer pooling**: uplink/downlink allocano `Vec`/`BytesMut` per pacchetto
   in vari punti; un pool (o `bytes::BytesMut::reserve` riusato) ridurrebbe la
   pressione sull'allocatore a pps alti.
7. **Replay window (B1)** ora che il frame ha già il counter: implementabile
   senza cambi wire, dimensionata secondo DEC-10.
8. **Admin: throughput live** (Mbps istantanei calcolati client-side dalla
   pagina, che già polla ogni 2 s) — i contatori cumulativi ci sono già.

---

## 6. Confronto con VPN_FULL_PLAN_TODO.md — cosa manca per completare la Fase 2

Stato item per item del TODO (aggiornato anche inline nel TODO stesso):

| Item TODO | Stato | Cosa manca |
|---|---|---|
| A1 direct QUIC path `P1` | ✅ Risolto (`49783aa`) | Solo esecuzione netns Test 6–9 |
| A2 `--auto-reconnect` `P1` | ✅ Risolto (`07598e0`) | Solo esecuzione netns Test 10–11 |
| A3 route replace `P1` | ✅ Risolto (`351eda7`) | — |
| A4 ip_forward revert `P2` | ✅ Risolto (`351eda7`) | — |
| B1 replay protection `P2` | ❌ Fuori scope del piano V2 (annotato DEC-10) | Implementazione completa |
| B2 AAD binding `P3` | ❌ Fuori scope (wire-breaking) | Versioning protocollo + implementazione |
| B3 key rotation `P3` | ❌ Fuori scope | Implementazione |
| C1 multi-queue `P3` | ✅ Risolto (`20f7d07`) | Esecuzione netns Test 13 + benchmark |
| C2 dynamic PMTU `P3` | ✅ Risolto (`20f7d07`) | Procedura manuale M-3 su WAN reale |
| C3 carriers relay `P3` | ✅ Risolto (`20f7d07`) | Esecuzione netns Test 12 + benchmark WAN |
| D1 TooLarge warn `P2` | ✅ Risolto (`351eda7`) | — |
| D2 admin page `P2` | ✅ Risolto (`3910299`) | — |
| D3 NAT/UPnP args `P2` | ✅ Risolto (`49783aa`) | — |
| D4 lease guard `P2` | ✅ Risolto (`351eda7`) | — |
| D5 deregister race `P2` | ✅ Risolto (`351eda7`) | — |
| E6 cross-platform | ⚠️ Parziale (`85ad3a4`) | §5.1 refactor cfg, §5.2 utun runtime, §5.3 wintun runtime, §5.4 Android/`--tun-fd`; iterare via CI |
| E1–E5, E7, E8 | ❌ Fuori scope V2 (per piano) | — |
| F1 reconnect smoke `P1` | ✅ Scritto (Test 10) | **Esecuzione con sudo** |
| F2 direct e2e `P1` | ✅ Scritto (Test 6–9) | **Esecuzione con sudo** |
| F3 reconnect netns `P2` | ✅ Scritto (Test 10–11) | **Esecuzione con sudo** |
| F4 replay test `P2` | ❌ Dipende da B1 | Con B1 |
| F5 admin entries `P2` | ✅ Risolto (test automatico) | — |
| F6 procedure manuali `P2` | ⚠️ 16.6.8 automatizzato (Test 14, da eseguire); 16.5.4 manuale | Esecuzione manuale |
| G documentazione | ✅ Aggiornata (VPN.md, TEST_MATRIX, USER_GUIDE, CLAUDE.md, TODO) | — |

### Checklist di chiusura (in ordine)

1. **`cargo build --release --features vpn` + `sudo scripts/vpn_netns_test.sh`**
   → Test 1–14 PASS. È l'unico gate mancante per dichiarare chiuse le fasi 1, 2
   e 4 anche sul piano della verifica end-to-end.
2. **`sudo scripts/vpn_bench.sh`** → incollare la tabella in `VPN.md` §Performance
   e fare il tuning pass §4.4 (criterio: cambi solo con ≥5% riproducibile;
   verificare relay-4c ≥ relay-1c e direct > relay).
3. **Push del branch e run CI** → verificare il job `vpn-cross-build` (è la
   prima esecuzione: possibili aggiustamenti su cargo-ndk/NDK env).
4. **Procedura manuale 16.5.4** (`--no-route-manage`) e **M-3** (PMTU su WAN).
5. **Fase 5 completa** (se/quando voluta): eseguire §5.1→§5.4 del piano
   iterando sulla CI; i builder per-OS e la guardia host-only sono già pronti
   come base.
6. Aggiornare le righe `PENDING` di `VPN_TEST_MATRIX.md` con gli esiti.
