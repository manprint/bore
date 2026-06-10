# VPN_FULL_PLAN_TODO — Tutto ciò che resta da implementare

> Stato al commit `142fe78` (branch `vpn`).  
> Priorità: **P1** = blocca funzionalità core, **P2** = importante ma aggirabile, **P3** = nice-to-have / V2.

---

## A. DIRETTO CRITICO — funzionalità mancante che limita l'usabilità

### A1 — Direct QUIC path non cablato `P1`

**Stato attuale:** `VpnLink::Direct`, `make_direct()`, `LinkSender::Direct`, `LinkRecver::Direct`
esistono in `vpn::link` ma non vengono mai usati. `run_listen` e `run_connect` vanno
**sempre** su relay AEAD. Il server ha `broker_vpn_udp()` implementato ma i client non
inviano mai `UdpCandidateOffer` dopo `VpnReady`.

**Conseguenza:** traffico TCP sull'overlay → meltdown reliable-over-reliable (§R.1).
`iperf3 -c overlay_addr` in modalità TCP si blocca; UDP funziona. Nessun beneficio QUIC.

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

### A2 — `--auto-reconnect` non funziona `P1`

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

### A3 — Route duplicate su reconnect `P1`

**Stato attuale:** `ip route add` senza controllo. Se il link si riconnette senza un
teardown pulito, `ip route add 192.168.50.0/24 dev bore0` fallisce con "file exists".
`NetConfig::apply` propaga l'errore e il reconnect fallisce.

**Da fare:**  
Usare `ip route replace ...` invece di `ip route add ...`, oppure ignorare l'errore
`EEXIST` nella `RealRunner`. Aggiornare `cmd_route_add` in `hostcfg_cmd`.

---

### A4 — `ip_forward` revert non affidabile `P2`

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

### C1 — Single uplink/downlink — CPU-bound su >1 Gbps `P3`

**Stato attuale:** un solo task uplink + un solo task downlink. Su link da 10+ Gbps
(server bare-metal con NIC veloci) diventa CPU-bound.

**Da fare (§V2):**  
`IFF_MULTI_QUEUE` sul TUN (`tun_rs::DeviceBuilder::multi_queue(true)`) + N coppie
uplink/downlink, una per coda. Richiede partizionamento del link o N istanze VpnLink.

### C2 — Dynamic PMTU non implementato `P3`

**Stato attuale:** MTU fisso 1350. `max_datagram_size()` cresce con il QUIC MTU discovery
ma non aggiorna il TUN. Dopo il warm-up il TUN potrebbe usare un MTU più grande.

**Da fare (§V2):**  
Spawn task che monitora `DirectConn::max_datagram_size()` ogni 5s e chiama
`ip link set bore0 mtu <new_mtu>` quando stabile.

### C3 — `--carriers` relay non implementato `P3`

**Stato attuale:** protocollo riserva il campo `carriers` (inviato come 1), ma il relay
usa sempre un singolo yamux stream. Con un solo stream il throughput relay è limitato
dal RTT × finestra del relay TCP.

**Da fare (§V2):**  
Aprire N yamux streams relay, round-robin per-datagram (non per-connessione, a differenza
dei secret tunnel). Gestire riordinamento o accettare out-of-order (IP è già best-effort).

---

## D. ROBUSTEZZA — comportamenti non gestiti

### D1 — `>10s TooLarge warn` non implementato `P2`

**Stato attuale:** `BridgeCounters.tx_drops` conta i `TooLarge`, ma il piano (§6.1)
richiede: "if drops still occurring >10s after link-up, `warn!` once suggesting lower
`--mtu`". Il periodic debug log esiste (10s) ma non fa la distinzione.

**Da fare:**  
In `run_uplink_single`/`run_uplink_offload` registrare `Instant::now()` all'avvio.
Se `tx_drops > 0` dopo 10s, emettere `warn!` una sola volta.

### D2 — Admin page VPN non differenziata `P2`

**Stato attuale:** `serve_vpn_listener` usa `Role::SecretProvider` e
`serve_vpn_connector` usa `Role::SecretConsumer`. Sul pannello admin i link VPN appaiono
come provider/consumer segreti, senza info overlay, path, iface, o contatori.

**Da fare:**  
1. Aggiungere `Role::VpnListener` / `Role::VpnConnector` in `admin.rs`.
2. Passare `overlay`, `iface`, `path` alla `NewEntry`.
3. Admin HTTP page: mostrare per i VPN link: ID, overlay, path (direct/relay), Mbps TX/RX
   (dal `BridgeCounters`), drops.

### D3 — NAT/UPnP args non usati `P2`

**Stato attuale:** `VpnListenArgs`/`VpnConnectArgs` hanno `stun_server`, `upnp`,
`try_port_prediction`, `nat_udp_preferred_port`, `nat_udp_release_timeout` ma non vengono
passati a nessuna funzione (dipendono dal direct path, §A1).

**Da fare:** wiring automatico una volta implementato §A1.

### D4 — `VpnLeaseGuard::drop` usa `try_lock` (silenzioso) `P2`

**Stato attuale:** se il lock è conteso al momento del Drop, il blocco /30 non viene
liberato e il pool "perde" quella entry fino al restart del server. Probabilità bassa
(lock brevissimo), ma possibile sotto carico.

**Da fare:**  
Usare `block_in_place` + `blocking_lock()` nel Drop, oppure passare a un canale oneshot
per il cleanup asincrono (pattern Tokio).

### D5 — `VpnDeregister` rimuove dall'`udp_providers` con chiave `"vpn:{id}"` `P2`

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
| E3 | Overlapping subnets via 1:1 NAT (DNAT/SNAT remap) | Media | Sblocca siti con LAN private identiche |
| E4 | Relay su path UDP inaffidabile (no TCP meltdown su relay) | Alta | Elimina §C3, parità con direct path |
| E5 | Privilege drop post-setup (setuid/capabilities) | Bassa | Riduzione superficie d'attacco |
| E6 | Windows support (wintun) / macOS (utun) | Media | Cross-platform |
| E7 | PSK per-link indipendente dal bore secret | Bassa | Isolation tra link sullo stesso server |
| E8 | Key rotation / rekey periodico | Media | Hardening crittografico |

---

## F. TEST mancanti o deboli

### F1 — Reconnect smoke test `P1`

Richiesto dal piano Phase 7.2. Non scritto. Scenario: server crasha mentre il bridge
gira → client riprova con backoff → link si riconnette.

### F2 — Test direct path end-to-end `P1`

Una volta implementato §A1, aggiungere al netns test:
- `ping` su path direct (verificare `info!(path="direct")` nei log)
- UDP `iperf3` su direct path (nessun TCP meltdown)
- Bloccare UDP → fallback a relay → sbloccare → tornare a direct

### F3 — Test `--auto-reconnect` nel netns harness `P2`

Dipende da §A2 + §F1.

### F4 — Test replay protection `P2`

Dipende da §B1. Test unitario: ritrasmettere frame relay già visto → `open()` rifiuta.

### F5 — Admin page VPN entries `P2`

Dipende da §D2. Verificare che link VPN siano visibili e con informazioni corrette.

### F6 — Procedure manuali VPN_TEST_MATRIX.md non eseguite `P2`

- `16.5.4` — `--no-route-manage`: applicare comandi stampati manualmente e verificare
  che la connettività funzioni
- `16.6.8` — SIGKILL + stale reclaim: confermato nel netns (Test 5), ma manca il
  check che `bore0` sia effettivamente rimasto dopo il SIGKILL e che il secondo avvio
  riclami correttamente **con** le route/nft (non solo il TUN)

---

## G. DOCUMENTAZIONE da aggiornare post-implementazione

| Documento | Aggiornamento necessario |
|---|---|
| `docs/VPN.md` | Aggiungere sezione direct path (§A1 completato); aggiornare troubleshooting con "path=direct" |
| `docs/VPN.md` | Rimuovere nota "Phase 6.2 GSO/GRO deferred" (ora implementato) |
| `docs/VPN_TEST_MATRIX.md` | Aggiungere copertura §F1–F6; aggiornare stato |
| `CLAUDE.md` | Aggiungere invariante: VPN bridge NOT to carry TCP at high rate over relay (meltdown) |

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
