# VPN_FULL_PLAN_TODO — Tutto ciò che resta da implementare

> Stato aggiornato 2026-06-11 (branch `vpn`). Suite netns Test 1–14 eseguita e
> verde (`PASS=42 FAIL=0`); per il riepilogo "fatto vs manca" vedi la
> **[sezione finale «STATO 2026-06-11»](#stato-2026-06-11--cosa-resta-davvero)**.  
> Priorità: **P1** = blocca funzionalità core, **P2** = importante ma aggirabile, **P3** = nice-to-have / V2.

---

## A0 — RISOLTO (2026-06-10): stallo permanente del relay sotto carico

**Sintomo:** il link funzionava per i primi ~256 KB di traffico, poi si bloccava
per sempre (anche un nuovo SYN non passava più). Nessun log, nessun errore.

**Causa radice (NON il meltdown §R.1):** il relay usava **un solo** substream yamux
condiviso fra task lettore e task scrittore via `tokio::io::split`. `yamux::Stream`
ha **un solo slot waker** sul proprio canale interno (`futures::mpsc::Sender::poll_ready`
in `poll_read` *e* `poll_write`): due task che pollano lo stesso stream si sovrascrivono
il waker a vicenda e il perdente non viene mai più svegliato → scrittore parcheggiato
per sempre → coda piena → drop silenzioso di ogni pacchetto.

**Fix:** due substream relay, uno per direzione (tag `0x01`/`0x02` dopo lo
`STREAM_READY`), ognuno posseduto da un solo task; la coda di uscita ora fa
**backpressure** (await) invece di droppare; il client ora **drena il control
stream** e rileva la morte del server (prima: nessun log possibile); buffer GSO
dimensionati a 65535 (i super-frame GRO inoltrati in gateway mode superano l'MTU
della TUN → panic in `gso_split`). Regression test: `tests/vpn_relay_link_test.rs`.
Invariante: **mai** `tokio::io::split` su un `mux::Stream` condiviso fra due task.

Risultato misurato (docker, 3 nodi): prima = stallo a ~256 KB; dopo = 100 MB in
0,5 s (~200 MB/s), 6 download paralleli da 100 MB ok, ping 0% loss sotto carico.

---

## A. DIRETTO CRITICO — funzionalità mancante che limita l'usabilità

### A1 — Direct QUIC path non cablato `P1` — ✅ RISOLTO (2026-06-11, commit 49783aa; fix panic switch da netns Test 8)

> **Nota fix 2026-06-11 (netns Test 8, direct gateway):** lo switch a direct
> faceva panic (`JoinHandle polled after completion`) perché `stop_pumps!` in
> `bridge::run` ri-attendeva il pump `JoinHandle` già pollato a `Ready` da
> `select_all`. Fix: salta gli handle `is_finished()` (`src/vpn.rs`).

**Stato attuale:** `VpnLink::Direct`, `make_direct()`, `LinkSender::Direct`, `LinkRecver::Direct`
esistono in `vpn::link` ma non vengono mai usati. `run_listen` e `run_connect` vanno
**sempre** su relay AEAD. Il server ha `broker_vpn_udp()` implementato ma i client non
inviano mai `UdpCandidateOffer` dopo `VpnReady`.

**Conseguenza:** nessun percorso peer-to-peer: tutto il traffico passa dal server
(banda e latenza del relay). Nota: lo "stallo TCP" attribuito qui al meltdown
reliable-over-reliable era in realtà il bug A0 (waker perso) — risolto. Il relay
ora regge traffico TCP bulk; il direct path resta utile per latenza/banda.

**Da fare:**

1. In `run_listen`/`run_connect`, dopo aver ricevuto `VpnReady`, avviare il tentativo di
   hole-punch **in parallelo** al relay:
   ```
   after VpnReady:
     spawn hole-punch task → sends UdpCandidateOffer on ctrl
     start relay bridge (fallback)
     on server UdpPunch → attempt connect_direct / accept_direct
     if QUIC conn established → switch VpnLink from Relay to Direct
   ```
2. Il listener usa `holepunch::DirectListener` (esiste già) per il lato server QUIC.
3. Il connector usa `holepunch::connect_direct` (esiste già).
4. `session_nonce` da `VpnReady` → `derive_token(secret, nonce)` → token per l'autenticazione
   QUIC (stesso meccanismo dei tunnel segreti).
5. Path switch: quando la connessione diretta è stabilita, sostituire il relay bridge con
   `link::make_direct(conn)`. Richiedere hot-swap del bridge o riavvio controllato.

**File:** `src/vpn.rs` (`run_listen`, `run_connect`), `src/vpn_server.rs` (`broker_vpn_udp`
già pronto lato server).

**Riferimento piano:** §6.1, §0.2 "transport decision", §16.1 "info!(path="direct")"

---

### A2 — `--auto-reconnect` non funziona `P1` — ✅ RISOLTO (2026-06-11, commit 07598e0; fix reconnect-race da netns Test 10)

> **Nota fix 2026-06-11 (netns Test 10):** la prima esecuzione netns ha mostrato
> che al restart del server connector e listener fanno race per ri-registrarsi;
> se il connector vince riceve `vpn listener '<id>' not found`, che era
> classificato **fatale** → il connector usciva e non si riconnetteva.
> `"not found"` è ora ritentabile come `"already in use"` in
> `vpn_error_is_retryable` (`src/vpn.rs`).

**Stato attuale:** CLI ha `--auto-reconnect` ma `run_listen`/`run_connect` non usano
`reconnect::run()`. Quando il bridge si chiude (server giù, link perso) il processo esce
senza riprovare.

**Da fare:**

1. Wrappare `run_listen`/`run_connect` in `reconnect::run(auto_reconnect, connect, serve)`.
2. Il bridge deve restituire un sentinel distinguibile (link-chiuso vs errore fatale) per
   decidere se riprovare.
3. Su reconnect: il TUN e le route devono persistere (non ricreare il device se esiste già,
   non re-aggiungere route duplicate). Oppure: teardown pulito + ricostruzione.
4. Test: smoke test "server drop → client reconnects with backoff" (richiesto Piano §Phase 7.2
   ma mai scritto).

**File:** `src/main.rs` dispatch, `src/vpn.rs`.

---

### A3 — Route duplicate su reconnect `P1` — ✅ RISOLTO (2026-06-11, commit 351eda7: `ip route replace`)

**Stato attuale:** `ip route add` senza controllo. Se il link si riconnette senza un
teardown pulito, `ip route add 192.168.50.0/24 dev bore0` fallisce con "file exists".
`NetConfig::apply` propaga l'errore e il reconnect fallisce.

**Da fare:**  
Usare `ip route replace ...` invece di `ip route add ...`, oppure ignorare l'errore
`EEXIST` nella `RealRunner`. Aggiornare `cmd_route_add` in `hostcfg_cmd`.

---

### A4 — `ip_forward` revert non affidabile `P2` — ✅ RISOLTO (2026-06-11, commit 351eda7: fallback `sudo -n tee` + doc sudoers)

**Stato attuale:** `NetConfig::Drop` usa `std::fs::write("/proc/sys/net/ipv4/ip_forward", ...)`
direttamente. Se il processo gira senza root (CAP\_NET\_ADMIN senza UID 0), la scrittura
fallisce silenziosamente (log warning, best-effort).

**Da fare:**  
Documentare chiaramente. Se si vuole supportare CAP\_NET\_ADMIN senza root, usare
`sudo tee /proc/sys/net/ipv4/ip_forward` (già nel sudoers di sviluppo) come fallback.

---

## B. SICUREZZA — gap che riducono le garanzie crittografiche

### B1 — Nessuna replay protection sul relay `P2`

**Stato attuale:** il frame relay ha `[u32 len][u64 counter][ciphertext+tag]`. Il
contatore è trasmesso ma non verificato dal ricevitore (nessuna sliding window). Un
attacker con accesso al relay può ritrasmettere frame vecchi; il ricevitore li decifra
e li inietta nel TUN.

**Wire format già compatibile:** il counter è nel frame, basta aggiungere la finestra.

**Da fare:**  
Aggiungere in `vpn::crypto::open()` un parametro `expected_counter: &mut u64` con una
sliding window di N=64 (come WireGuard). Rifiutare frame con counter < window_base - N
o già visti nel bitmap. Aggiornare `LinkRecver::Relay`.

### B2 — AAD vuoto — nessun binding al contesto `P3`

**Stato attuale:** `AEAD seal/open` usa `Aad::empty()`. Un frame intercettato da un link
potrebbe essere reiniettato in un altro se i due link condividono il secret (nonce diverso
protegge le chiavi, ma non lega i frame a link/direzione specifici).

**Da fare:**  
Usare come AAD: `link_id ‖ direction_byte` (0x00 = listener→connector, 0x01 = viceversa).
Cambiamento wire-breaking: richiederebbe versioning del protocollo.

### B3 — Nessuna key rotation `P3`

**Stato attuale:** chiavi HKDF derivate una volta alla connessione, usate per tutta
la vita del link. Counter a 64 bit non si esaurisce in pratica (2^64 pacchetti), ma
le buone pratiche consigliano rekey periodico (es. ogni 2^32 pacchetti o ogni ora).

---

## C. PERFORMANCE — bottleneck noti

### C1 — Single uplink/downlink — CPU-bound su >1 Gbps `P3` — ✅ RISOLTO (2026-06-11, commit 20f7d07: `--tun-queues`, uplink per coda)

**Stato attuale:** un solo task uplink + un solo task downlink. Su link da 10+ Gbps
(server bare-metal con NIC veloci) diventa CPU-bound.

**Da fare (§V2):**  
`IFF_MULTI_QUEUE` sul TUN (`tun_rs::DeviceBuilder::multi_queue(true)`) + N coppie
uplink/downlink, una per coda. Richiede partizionamento del link o N istanze VpnLink.

### C2 — Dynamic PMTU non implementato `P3` — ✅ RISOLTO (2026-06-11, commit 20f7d07: `pmtu_monitor` + `pmtu_decision`)

**Stato attuale:** MTU fisso 1350. `max_datagram_size()` cresce con il QUIC MTU discovery
ma non aggiorna il TUN. Dopo il warm-up il TUN potrebbe usare un MTU più grande.

**Da fare (§V2):**  
Spawn task che monitora `DirectConn::max_datagram_size()` ogni 5s e chiama
`ip link set bore0 mtu <new_mtu>` quando stabile.

### C3 — `--carriers` relay non implementato `P3` — ✅ RISOLTO (2026-06-11, commit 20f7d07: round-robin per-datagram, counter atomico condiviso)

**Stato attuale:** protocollo riserva il campo `carriers` (inviato come 1), ma il relay
usa sempre un singolo yamux stream. Con un solo stream il throughput relay è limitato
dal RTT × finestra del relay TCP.

**Da fare (§V2):**  
Aprire N yamux streams relay, round-robin per-datagram (non per-connessione, a differenza
dei secret tunnel). Gestire riordinamento o accettare out-of-order (IP è già best-effort).

---

## D. ROBUSTEZZA — comportamenti non gestiti

### D1 — `>10s TooLarge warn` non implementato `P2` — ✅ RISOLTO (2026-06-11, commit 351eda7)

**Stato attuale:** `BridgeCounters.tx_drops` conta i `TooLarge`, ma il piano (§6.1)
richiede: "if drops still occurring >10s after link-up, `warn!` once suggesting lower
`--mtu`". Il periodic debug log esiste (10s) ma non fa la distinzione.

**Da fare:**  
In `run_uplink_single`/`run_uplink_offload` registrare `Instant::now()` all'avvio.
Se `tx_drops > 0` dopo 10s, emettere `warn!` una sola volta.

### D2 — Admin page VPN non differenziata `P2` — ✅ RISOLTO (2026-06-11, commit 3910299: ruoli dedicati, overlay, path report, byte relay)

**Stato attuale:** `serve_vpn_listener` usa `Role::SecretProvider` e
`serve_vpn_connector` usa `Role::SecretConsumer`. Sul pannello admin i link VPN appaiono
come provider/consumer segreti, senza info overlay, path, iface, o contatori.

**Da fare:**  
1. Aggiungere `Role::VpnListener` / `Role::VpnConnector` in `admin.rs`.
2. Passare `overlay`, `iface`, `path` alla `NewEntry`.
3. Admin HTTP page: mostrare per i VPN link: ID, overlay, path (direct/relay), Mbps TX/RX
   (dal `BridgeCounters`), drops.

### D3 — NAT/UPnP args non usati `P2` — ✅ RISOLTO (2026-06-11, commit 49783aa: wiring nel direct upgrade)

**Stato attuale:** `VpnListenArgs`/`VpnConnectArgs` hanno `stun_server`, `upnp`,
`try_port_prediction`, `nat_udp_preferred_port`, `nat_udp_release_timeout` ma non vengono
passati a nessuna funzione (dipendono dal direct path, §A1).

**Da fare:** wiring automatico una volta implementato §A1.

### D4 — `VpnLeaseGuard::drop` usa `try_lock` (silenzioso) `P2` — ✅ RISOLTO (2026-06-11, commit 351eda7: std Mutex bloccante)

**Stato attuale:** se il lock è conteso al momento del Drop, il blocco /30 non viene
liberato e il pool "perde" quella entry fino al restart del server. Probabilità bassa
(lock brevissimo), ma possibile sotto carico.

**Da fare:**  
Usare `block_in_place` + `blocking_lock()` nel Drop, oppure passare a un canale oneshot
per il cleanup asincrono (pattern Tokio).

### D5 — `VpnDeregister` rimuove dall'`udp_providers` con chiave `"vpn:{id}"` `P2` — ✅ RISOLTO (2026-06-11, commit 351eda7: generation token)

**Stato attuale:** in `VpnDeregister::drop` viene rimosso anche `udp_providers.remove(&udp_id)`.
Ma se `serve_vpn_listener` esce prima che il connector arrivi (e quindi prima che venga
effettivamente registrata l'entry UDP), la rimozione è no-op. Non è un bug, solo rumore.
Tuttavia se il link viene ripareggiato (reconnect) e l'UDP entry della sessione precedente
è rimasta (race), le candidate vengono servite al connector sbagliato.

---

## E. FUNZIONALITÀ V2 (out of scope v1, priorità futura)

| # | Funzionalità | Complessità | Impatto |
|---|---|---|---|
| E1 | Multi-peer mesh / hub (N nodi, routing dinamico) | Alta | Trasforma bore in overlay network completo |
| E2 | IPv6 overlay + dual-stack + NAT66/NPTv6 | Media | Necessario per deployment moderni |
| ✅ E3 | Overlapping subnets via 1:1 NAT (DNAT/SNAT remap) | Media | **DONE (2026-06-14)** Sblocca siti con LAN private identiche |
| E4 | Relay su path UDP inaffidabile (no TCP meltdown su relay) | Alta | Elimina §C3, parità con direct path |
| E5 | Privilege drop post-setup (setuid/capabilities) | Bassa | Riduzione superficie d'attacco |
| E6 | Windows support (wintun) / macOS (utun) | Media | Cross-platform |
| E7 | PSK per-link indipendente dal bore secret | Bassa | Isolation tra link sullo stesso server |
| E8 | Key rotation / rekey periodico | Media | Hardening crittografico |

---

## F. TEST mancanti o deboli

### F1 — Reconnect smoke test `P1` — ✅ FATTO (netns Test 10 PASS 2026-06-11)

Richiesto dal piano Phase 7.2. Scenario: server crasha mentre il bridge gira →
client riprova con backoff → link si riconnette. **L'esecuzione ha scoperto un
bug** (race connector/listener al re-register: `vpn listener not found`
classificato fatale) ora corretto — vedi §A2.

### F2 — Test direct path end-to-end `P1` — ✅ FATTO (netns Test 6-9 PASS 2026-06-11)

Una volta implementato §A1, aggiungere al netns test:
- `ping` su path direct (verificare `info!(path="direct")` nei log)
- UDP `iperf3` su direct path (nessun TCP meltdown)
- Bloccare UDP → fallback a relay → sbloccare → tornare a direct

### F3 — Test `--auto-reconnect` nel netns harness `P2` — ✅ FATTO (netns Test 10-11 PASS 2026-06-11)

Dipende da §A2 + §F1. Test 11 verifica anche che un errore fatale esca subito
nonostante `--auto-reconnect`.

### F4 — Test replay protection `P2`

Dipende da §B1. Test unitario: ritrasmettere frame relay già visto → `open()` rifiuta.

### F5 — Admin page VPN entries `P2` — ✅ RISOLTO (2026-06-11: `vpn_admin_entries_and_path_report`)

Dipende da §D2. Verificare che link VPN siano visibili e con informazioni corrette.

### F6 — Procedure manuali VPN_TEST_MATRIX.md

- `16.6.8` — SIGKILL + stale reclaim **con route/nft (non solo TUN)**:
  ✅ FATTO — automatizzato come netns Test 14, PASS 2026-06-11 (verifica che
  nft table + route sopravvivano al `kill -9` e siano riclamate al riavvio,
  niente EEXIST).
- `16.5.4` — `--no-route-manage`: ⚠️ ancora manuale (applicare i comandi
  stampati a mano e verificare la connettività). Unica procedura manuale residua.

---

## G. DOCUMENTAZIONE da aggiornare post-implementazione — ✅ FATTO (2026-06-11)

| Documento | Aggiornamento | Stato |
|---|---|---|
| `docs/vpn/VPN.md` | Sezione direct path (§A1) + troubleshooting "path=direct"; nota GSO/GRO ora "Implemented" | ✅ |
| `docs/vpn/VPN_TEST_MATRIX.md` | Copertura §F1–F6; netns Test 1–14 PASS; nota bug-fix | ✅ |
| `docs/vpn/VPN_PHASE_2_STATUS.md` | Esecuzione netns + §2.bis bug-fix | ✅ |
| `docs/vpn/VPN_USER_FULL_GUIDE.md` | Stato test, PMTU dinamico, "not found" ritentabile | ✅ |
| `README.md` / `USER_GUIDE.md` | Sezione VPN allineata; USER_GUIDE §4.10 nuova | ✅ |
| `CLAUDE.md` | Invarianti yamux-split, counter atomico, DEC-*, RAII NetConfig | ✅ (commit precedenti) |

---

## Priorità raccomandata di implementazione

```
1. §A1 (direct QUIC path)    — sblocca TCP sull'overlay, richiede §F2
2. §A2 (--auto-reconnect)    — essenziale per deployment reale, richiede §A3
3. §A3 (route replace)       — prerequisito di §A2
4. §B1 (replay protection)   — sicurezza, bassa complessità
5. §D2 (admin VPN page)      — visibility, media complessità
6. §D1 (TooLarge warn)       — 5 righe di codice
7. §E2 (IPv6)                — richiede kernel IPv6 forwarding + ndp proxy
8. §E1 (mesh)                — cambio architetturale significativo
```

---

## Note su §A1 — come cablare il direct path (sketch)

```rust
// Dopo aver ricevuto VpnReady in run_listen / run_connect:
let session_nonce = /* da VpnReady */;
let token = holepunch::derive_token(Some(&args.secret), &session_nonce);

// 1. Gather candidates (listener side = serve, connector side = connect)
let socket = holepunch::bind_socket(args.nat_udp_preferred_port).await?;
let candidates = holepunch::gather_candidates(&socket, ...).await;

// 2. Send UdpCandidateOffer to server on ctrl stream
ctrl.send(ClientMessage::UdpCandidateOffer(UdpCandidateOffer {
    candidates,
    selected_stun: None,
})).await?;

// 3. Wait for UdpPunch or UdpUnavailable (non-blocking: select! with relay bridge)
tokio::select! {
    punch = ctrl.recv::<ServerMessage>() => {
        match punch? {
            Some(ServerMessage::UdpPunch { nonce, peer, .. }) => {
                // listener: accept_direct(socket, token)
                // connector: connect_direct(socket, peer, token, tuning)
                // on success: replace relay bridge with direct bridge
            }
            Some(ServerMessage::UdpUnavailable) => { /* stay on relay */ }
            _ => { /* heartbeat etc. */ }
        }
    }
    _ = relay_bridge_closed() => { /* relay died, exit/reconnect */ }
}
```

**File da toccare:** `src/vpn.rs` (run\_listen, run\_connect), niente di nuovo
nell'infrastruttura — `holepunch.rs` e `vpn_server.rs` sono già pronti.

---

## STATO 2026-06-11 — cosa resta davvero

Sezione di sintesi: tutto il resto del documento è storia. Qui solo lo stato netto.

### ✅ FATTO e verificato end-to-end (netns Test 1–14 PASS, `PASS=42 FAIL=0`)

- **A1** direct QUIC path · **A2** `--auto-reconnect` · **A3** route replace ·
  **A4** ip_forward revert · **C1** multi-queue · **C2** dynamic PMTU ·
  **C3** carriers relay · **D1** TooLarge warn · **D2** admin page ·
  **D3** NAT/UPnP wiring · **D4** lease guard · **D5** deregister race ·
  **F1/F2/F3** netns reconnect+direct · **F5** admin entries ·
  **F6/16.6.8** SIGKILL full reclaim (Test 14).
- **+2 bug** scoperti dalla prima run netns e corretti: panic allo switch direct
  (Test 8) e race di reconnect `not found` fatale (Test 10) — vedi §A1, §A2.
- Gate CI verdi: `fmt`, `clippy --all-features --all-targets -D warnings`,
  `cargo test --all-features`.

### ⏳ APERTO ma a basso rischio (esecuzione/misura, non codice)

| # | Cosa manca | Tipo | Bloccante? |
|---|---|---|---|
| §4.4 | ✅ **Benchmark `vpn_bench.sh` eseguito** (2026-06-11, tabella in VPN.md): direct ≫ relay. Nessun cambio di tuning (criterio: solo guadagni ≥5% riproducibili). **Residuo:** misurare il beneficio dei `--carriers` su **WAN ad alto RTT** — su netns a ~0.4 ms regrediscono (atteso: round-robin per-datagram → riordino; il tetto RTT×finestra che i carrier rompono non esiste su loopback) | Misura su WAN | No — perf, non correttezza |
| C2/M-3 | `tun MTU adjusted` su **WAN reale** (in netns la PMTU è statica) | Manuale | No |
| 16.5.4 | `--no-route-manage`: applicare a mano i comandi stampati e verificare | Manuale | No |
| V2-5.5 | Job CI `vpn-cross-build` (prima esecuzione: possibili aggiustamenti cargo-ndk/NDK) | CI | No |

### ❌ NON FATTO — Fase 5 cross-platform runtime (E6) `parziale`

Fatto solo il groundwork: builder argv `hostcfg_cmd::{macos,windows}` + CI
cross-build + tabella piattaforme. **Manca il runtime per-OS:**

- **§5.1** refactor dei `cfg` (`lib.rs`/`vpn.rs`), gating fine di
  offload/multiqueue/procfs, `check_root` per-OS, selezione per-OS dei builder.
- **§5.2** runtime utun (macOS) + **§5.3** runtime wintun (Windows) + gestione
  `wintun.dll` + **§5.4** Android `--tun-fd` / target nel justfile.
- **M-4/M-5/M-6** smoke macOS/Windows/Termux (dipendono dal runtime).
- *Motivazione del rinvio:* su questa macchina non esiste toolchain C per
  macOS/Windows (perfino `cargo check --target aarch64-apple-darwin` fallisce
  sulla build C di `ring`). Iterare via CI `vpn-cross-build`.

### ❌ NON FATTO — fuori scope dichiarato del piano V2 (sicurezza / V2)

- **B1** replay protection sul relay (`P2`) — il frame ha già il counter;
  dimensionare la sliding window ≥ 2 × (carriers × RELAY_QUEUE) per DEC-10. + **F4** (test replay) dipende da qui.
- **B2** AAD binding (`P3`, wire-breaking → versioning) · **B3** key rotation (`P3`).
- **E1** mesh · **E2** IPv6/dual-stack · **E3** overlapping subnets via 1:1 NAT ·
  **E4** relay su UDP · **E5** privilege drop post-setup · **E7** PSK per-link ·
  **E8** rekey.
