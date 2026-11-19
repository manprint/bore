# VPN deep review — 2026-11-19

Ambito: `bore vpn listen|connect` su `dev` @ `6368c89` (1.4.0-rc.2). Path relay e diretto, 1:1 e hub (`--max-clients`), carriers, tun-queues, NAT/forward, reconnect, PMTU, broker lato server.
Metodo: lettura integrale di `src/vpn.rs` (Linux), `src/vpn_server.rs`, parti rilevanti di `src/holepunch.rs`, `src/main.rs`, `tun-rs 2.8.18`. Nessuna modifica al codice.

> **Aggiornamento, stessa data:** i finding sono stati corretti nel working tree (non committato). Stato per finding, test aggiunti e punti rimasti aperti: sezione [Stato dei fix](#stato-dei-fix) in fondo. Le sezioni che seguono descrivono il codice PRIMA dei fix; i numeri di riga non valgono più.

Legenda stato:
- **RIPRODOTTO** — dimostrato con un esperimento.
- **LETTO** — verificato rileggendo il codice riga per riga; non riprodotto a runtime.
- **DA VERIFICARE** — visto in prima passata, non riconfermato.

Non coperto: twin macOS/Windows/Android (solo sfiorati), `src/mux.rs` oltre la config yamux, `src/liveness.rs`, harness `scripts/vpn_netns_test*.sh`, nessun e2e (in questa sandbox `sudo -n` non è disponibile).

---

## Priorità alta

### F-T — `ip route replace` distrugge route esistenti dell'host — **RIPRODOTTO**
- Dove: `cmd_route_add` `src/vpn.rs:3179–3193`, loop route `6541–6556`; lato listener `vpn.rs:959`.
- Cosa: le route verso le subnet del peer sono installate con `ip route replace <subnet> dev <tun>`. Se l'host ha già una route con stesso prefisso e metrica (LAN connessa, statica, default a metrica 0) viene sovrascritta. Il revert è `ip route del`, che non ripristina l'originale.
- Prova: in `unshare -Urn`, dopo replace + del di `192.168.50.0/24`, `ip route get 192.168.50.20` risponde `Network is unreachable`.
- Aggravanti:
  - il listener installa TUTTE le route annunciate dal connector senza policy (`let peer_routes = peer_advertised.to_vec();`), mentre il connector è default-deny (`routes::filter_accepted`, `vpn.rs:2410`);
  - il server (`validate_link_params`) non rifiuta `/0` né prefissi anomali;
  - nessuna guardia per route che coprono l'IP del server bore: la connessione di controllo finirebbe dentro il tunnel (flap).
- Impatto: un peer autenticato (o un errore di configurazione) toglie connettività LAN all'altro host, in modo persistente dopo la chiusura della VPN.
- Fix proposto: prima di installare, `ip route show exact <subnet>`; se esiste, rifiutare con errore chiaro (oppure metrica più alta / tabella dedicata). Policy accept/refuse anche sul listener. Rifiutare `/0` salvo flag esplicito. Host route di pin verso il server.

### F-ZOMBIE — le connessioni dirette non vengono mai chiuse — **LETTO**
- Dove: fallback del bridge `vpn.rs:9711–9718`, `9732–9742`; teardown `run_bridge_with_ctrl` `2213–2251`, `run_connect_once` `2484–2542`; task detached `pmtu_monitor`/`direct_diag` `1813–1824`. `DirectConn::close()` esiste (`holepunch.rs:3304`) ma il path VPN non lo chiama.
- Scenari:
  1. Fallback solo locale (errore TUN mentre il QUIC è vivo): il peer continua a spedire sul diretto verso un downlink abortito. Buco nero monodirezionale, tenuto vivo all'infinito dai cloni detenuti dai monitor e dai keepalive (3 s).
  2. Link abbattuto con diretto sano (controllo perso, restart del server): ogni riconnessione lascia una conn QUIC, un socket UDP e due task. Lo zombie `pmtu_monitor` può poi eseguire `ip link set <tun> mtu` sulla NUOVA TUN omonima.
  3. `--carriers N>1` e un carrier muore: i restanti restano aperti.
- Fix proposto: `conn.close()` su tutti i carrier in ogni fallback e a fine link; legare i monitor a un `CancellationToken` del link; `abort()` + `await` di `direct_task`.

### F-D — 1:1: upgrade diretto bloccato in off-by-one permanente — **LETTO**
- Dove: `try_direct_upgrade` step 5 `vpn.rs:1628–1640`; `direct_upgrade_task` `1478–1543`; broker `vpn_server.rs:1812–1971` (offer del listener cancellata solo dopo un punch, `1895–1901`).
- Cosa: il client prende il primo `CtrlEvent` in coda, senza svuotare il canale prima dell'offer e senza controllare la generation. Il server conserva l'offer del listener senza scadenza. Se il connector offre più di ~15 s dopo il listener, il server manda un `UdpPunch` con candidati vecchi a un listener che ha già smesso di aspettare: l'evento resta in coda e al round successivo viene consumato al posto di quello nuovo. Da lì ogni round usa il punch del precedente.
- Trigger: un solo round disallineato (fallback rilevato in modo asimmetrico; il ticker `MissedTickBehavior::Skip` scatta subito dopo un park lungo). Tolleranze: connector dopo listener ≤ 15 s, listener dopo connector ≤ 10 s.
- Impatto: il link resta sul relay per sempre; funziona, ma senza il path diretto.
- L'hub non ne soffre: `per_peer_direct_upgrade_task` svuota tra i round.
- Fix proposto: drain di `event_rx` prima dell'offer; scartare punch con generation più vecchia; TTL sui candidati memorizzati lato server.

---

## Priorità media

### F-TUNERR — un singolo errore TUN è fatale per la pompa — **LETTO**
- Dove: `dev.send(pkt).await?` `vpn.rs:9936`; `dev.send_multiple(...).await?` `9980`; analoghi in lettura.
- Cosa: su relay provoca teardown e riconnessione dell'intero link (TUN e route ricreate); su diretto provoca il fallback solo locale di F-ZOMBIE.
- Cause plausibili su Linux: `EIO` (interfaccia down), `EINVAL` (payload non IP, inviabile da un peer autenticato), `ENOMEM`.
- Non provato: in `tun-rs` `handle_gro` restituisce `Err` solo per "too many packet buffers", "invalid offset" e lunghezza coalescente fuori campo, quindi pacchetti malformati ordinari probabilmente non lo innescano. `gso_split` in lettura ha molti rami `Err`. macOS `EMSGSIZE` con MTU ridotta in modo asimmetrico: da verificare su macOS.
- Fix proposto: errori per-pacchetto = drop + contatore; fatale solo l'errore persistente.

### F-HUB-HOL — una spoke lenta blocca tutto l'hub — **LETTO** (limite noto)
- Dove: router `mod hub` (`run_router_uplink_single/offload`); `send_batch_inner` `vpn.rs:8836–8973`.
- Cosa: su relay `send_batch` ignora `backpressure` (`let _ = backpressure;`) e fa `txs[rr].send(...).await` su coda da 512, tenendo il mutex del peer. Un solo task router per coda TUN: una spoke ferma blocca il traffico verso tutte.
- Nota: `VPN_BUGHUNT_ASSESSMENT.md` lo classifica "design limit"; `VPN_PRODUCTION_ASSESSMENT_2026-07-10.md:63` afferma invece che l'invio è non bloccante, cosa che il ramo relay smentisce.
- Fix proposto: `try_send` + `tx_drops` quando `backpressure == false`, oppure coda di egress per peer.

### F-CAP — errori transitori del server trattati come fatali — **LETTO**
- Dove: `vpn_error_is_retryable` `vpn.rs:210` (solo "already in use" e "not found").
- Cosa: "hub … is at capacity", "vpn pool exhausted", "server vpn-max-links reached", "server connection limit reached" diventano `FatalVpnError`: il processo esce anche con `--auto-reconnect`.
- Scenario: hub pieno, una spoke cambia IP e si riconnette prima che il server liberi il vecchio slot (reap 15 s) → uscita definitiva.
- Fix proposto: renderli retryable con backoff.

### F-S — isolamento spoke assente su hub host-only — **LETTO** (limite v1 documentato, C3)
- Dove: regola installata solo dentro `if is_gateway && !no_route_manage` (`vpn.rs:6559`; nft `6715`, iptables `6811`).
- Cosa: hub senza `--advertise` o con `--no-route-manage` non ha isolamento se l'host ha `ip_forward=1` e FORWARD accetta.
- Fix proposto: installare l'isolamento sempre in modalità hub.

### F-OVL — `Leave` in ritardo rimuove il peer sbagliato — **LETTO** (precondizione rara)
- Dove: `run_hub_coordinator` (`peer_table.remove(&peer.overlay)` incondizionato, `insert` che sovrascrive); server `HubPeerGuard::drop` (`try_send(Leave)` su canale da 32) e `HubState::alloc_peer` (riusa subito l'overlay più basso).
- Cosa: con più di 32 uscite simultanee un `Leave` va perso; il `Leave` auto-riportato dal vecchio peer arriva dopo che l'overlay è stato riassegnato e rimuove la entry del NUOVO peer. Buco nero hub→spoke.
- Fix proposto: rimozione condizionata a `peer_id`; eviction esplicita su `Join` con overlay già presente.

### F-HUBDEAD — il coordinatore hub può bloccarsi — **LETTO** (serve stato asimmetrico)
- Dove: `try_hub_peer_direct` `vpn.rs:11603–11859` (attorno a `11838`); `spawn_hub_ctrl_actor` `11865`; `try_build_peer`.
- Cosa: su diretto il task per-peer smette di leggere `punch_rx` dopo un evento. I "prod" del server si accumulano (cap 16), il coordinatore resta in `punch_tx.send().await` e non gestisce più Join/Leave per nessuna spoke; poi si blocca anche l'actor di controllo (cap 256). In più `try_build_peer` fa una lettura bloccante fino a 10 s per stream dentro il coordinatore.
- Fix proposto: drain in loop durante l'attesa; `try_send` nel coordinatore; build del peer fuori dal coordinatore.

### F-Z5 — ricezione multi-carrier sbilanciata — **LETTO**
- Dove: `LinkRecver::recv_batch` `vpn.rs:8836–8973`.
- Cosa: `select_all` e drain partono sempre dal carrier 0 e possono riempire `BATCH_CAP` (64): i carrier successivi restano indietro sotto carico (overflow del buffer datagram). Allocazioni `Vec<Pin<Box>>` per batch.
- Fix proposto: ruotare l'indice di partenza, quota per carrier.

### F-Z2 — `--carriers N` sul relay non dà parallelismo — **LETTO**
- Dove: `connect_relay_multi` `vpn.rs:8579–8611`.
- Cosa: N coppie di substream sulla STESSA connessione TCP (nessun isolamento cwnd/HOL) con round-robin per datagramma: possibile riordino intra-flusso senza beneficio. Il commento che cita una replay window è obsoleto: nel codice non esiste.
- Fix proposto: flow-pinning anche sul relay, o ridurre a 1 sul relay.

### F-HUBLEAK — leak di subnet hub sul server — **LETTO**
- Dove: `serve_vpn_listener` `vpn_server.rs:673–691`.
- Cosa: `alloc_hub_subnet` (675), poi `check_overlap` esce con `return Ok(())` (678–681) prima che esista `VpnHubLeaseGuard` (691). Un blocco perso per tentativo fallito, fino al restart.
- Fix proposto: creare il guard subito dopo l'allocazione.

---

## Priorità bassa

| ID | Stato | Dove | Problema | Fix |
|---|---|---|---|---|
| F-B6 | LETTO | `vpn.rs:1005`, `1828`, `2476` | Dopo fallback diretto→relay non viene inviato `VpnPathReport{path:"relay"}`: admin mostra "direct". Hub: report senza peer id, vince l'ultimo. | Report a ogni cambio; peer id in hub. |
| F-SIG | LETTO | `main.rs:1867` | `SIGHUP` non gestito: sessione ssh caduta = nessun revert di `NetConfig` fino al prossimo `stale_reclaim` con stesso id. | Gestire `SIGHUP` come `SIGTERM`. |
| F-H | LETTO | `pmtu_monitor` `vpn.rs:1940` | Riparte da `initial_mtu` a ogni upgrade; MTU non ripristinata al fallback; lato hub nessun monitor. | Stato PMTU per link; ripristino al fallback. |
| F-RV | LETTO | `vpn.rs:6642–6727` | Revert nft registrato solo dopo che tutti i comandi riescono: errore a metà lascia `bore_vpn_<id>` fino al prossimo avvio con stesso id. | Push del revert subito dopo `add table`. |
| M-1 | LETTO | `vpn_server.rs:1370` | Hub connector ignora `punch_timeout`, usa 10 s fissi. | Usare il parametro. |
| M-2 | LETTO | router hub | `warn!` per pacchetto su peer morto (log flood). | Rate limit. |
| M-3 | LETTO | `vpn.rs:2484–2542` | `direct_task.abort()` non atteso: possibile `EADDRINUSE` con `--nat-udp-preferred-port`. | `abort` + `await`. |
| M-4 | LETTO | `vpn.rs:9454` | `UPGRADE_GRACE` inutilizzata. | Rimuovere. |

R-1 (design): la perdita esplicita della connessione di controllo abbatte il link anche con diretto sano (il silenzio è tollerato, EOF/errore no). Proposta: riconnessione del controllo make-before-break.

---

## Da verificare (prima passata, non riconfermati)

- Comandi `std::process::Command` bloccanti in contesto async; `stale_reclaim` che scrive `ip_forward` due volte.
- Nessun heartbeat tra `VpnReady` e l'avvio dell'actor di controllo.
- Candidati nell'overlay /30 o locali alla TUN non filtrati.
- `sample_host = base+1` in overflow; interfaccia LAN dedotta solo da `advertised[0]`; MSS clamp nft non limitato alla TUN; retry di `create_tun` su qualsiasi errore.
- Server malevolo che riusa `session_nonce` (nessun contributo del client alla derivazione). Il nonce è comunque fresco per pairing (`vpn_server.rs:1614`, `1637`) e per peer hub (`159`).

## Verificato corretto

Guard di re-poll nel `select!` del bridge; `downgrade_tx.try_send` cap 1; limiti di `take_frame`; contatore nonce condiviso per chiave di egress; teardown `PumpGuard`; drain tra i round nell'hub; `SIGINT`/`SIGTERM` eseguono `Drop`; `panic = unwind`; nonce di sessione da CSPRNG.

---

## Piano di fix proposto

1. **F-T** — guardia route + policy listener + rifiuto `/0`. Gate: test netns con route preesistente.
2. **F-ZOMBIE + F-TUNERR** — stessa area (bridge/teardown), un solo intervento.
3. **F-D** — drain + generation lato client, TTL lato server.
4. **F-CAP, F-HUBLEAK, F-RV, F-SIG** — piccoli e indipendenti.
5. **Hub**: F-HUB-HOL, F-HUBDEAD, F-OVL, F-S.
6. **Perf**: F-Z5, F-Z2 — con misure prima/dopo (V-9).
7. Residui bassi e punti "da verificare".

---

## Stato dei fix

Working tree su `dev`, non committato. Riferimenti normativi aggiornati: `docs/INVARIANTS.md` (voce "VPN review 2026-11-19" e sezione hub), indice in `CLAUDE.md`, `README.md` (sezione VPN).

### Corretti

| ID | Cosa è cambiato | Test |
|---|---|---|
| F-T | `vet_peer_routes`: mai installata una route che copre l'IP del server bore (`routes::guard_protected`) né, su Linux, un prefisso che l'host instrada già altrove (`hostcfg::drop_conflicting_routes`, sonda `ip -4 route show exact`). Policy sul listener 1:1: `--accept-routes`, `--refuse-routes`, `--refuse-all-routes`. Default del listener invariato (accetta tutto). | unit `routes::*`, `foreign_route_owner`, `drop_conflicting_routes` |
| F-ZOMBIE | `close_direct()` su tutti i carrier a ogni fallback, sostituzione di path e fine link; chiusura anche su drop non ordinato del bridge. | unit bridge (2), hub |
| F-D | Drain del canale di controllo prima dell'offer; eventi consumati tra i round, griglia riallineata sul punch ricevuto da idle. Solo lato client. | unit `wait_next_direct_round`, drain |
| F-TUNERR | `TunFaults`: errore TUN = drop per pacchetto; fatale solo una serie di 16 errori consecutivi su 3 s. | unit |
| F-HUB-HOL | Relay: `send_batch` usa `try_reserve` e scarta per la sola spoke; `send_batch_wait` attende. Slot riservato prima del seal (nessun nonce consumato da un pacchetto scartato). | unit; `tests/vpn_relay_link_test.rs` ora usa `send_batch_wait` |
| F-CAP | Errori di capacità ritentabili. | unit `vpn_error_classification_pinned` |
| F-S | Isolamento spoke su ogni hub Linux, anche host-only; iptables con chain propria `bore_<id>_iso` (la vecchia regola non veniva mai rimossa dal suo stesso delete). | unit hostcfg (3) |
| F-OVL | `Leave` rimuove solo se il `peer_id` possiede ancora l'overlay; eviction esplicita su `Join`. | unit hub |
| F-HUBDEAD | Punch consumati per tutta la permanenza su diretto; `try_send` nel coordinatore; build del peer fuori dal coordinatore. | unit hub (2) |
| F-Z5 | Indice di partenza ruotato per batch; prima ciò che è già in coda su ogni carrier. | unit |
| F-Z2 | Relay flow-pinned (`flow_carrier`). **Non misurato** prima/dopo: il punto 6 del piano chiedeva misure (V-9), qui non eseguibili. | unit `relay_carriers_are_flow_pinned` |
| F-HUBLEAK | `lease_hub_subnet` restituisce il guard insieme al blocco; allocazione fallita ⇒ `VpnError` al client. | `hub_subnet_lease_is_released_when_the_registration_is_rejected` |
| F-B6 | 1:1: `VpnPathReport{path:"relay"}` al fallback (`park_on_direct`). Hub: path aggregato (`HubPathGauge`), non per peer: il wire non porta un peer id. | unit (3) |
| F-SIG | `SIGHUP` gestito su Linux salvo quando è già ignorato (`nohup`). | unit `sighup_ignored_reads_the_sigign_mask` |
| F-H | MTU configurata ripristinata alla chiusura del path diretto. | unit `pmtu_restore_target_cases` |
| F-RV | Revert registrato subito dopo ogni passo (nft, chain iptables, forward-accept). | unit `apply_failing_halfway_keeps_the_teardown_registered` |
| M-1 | Il broker hub usa il `punch_timeout` configurato. | `vpn_hub_broker_timeout_honours_the_configured_punch_timeout` |
| M-2 | Warning del router hub limitato in frequenza (`WarnGate`). | unit |
| M-3 | `direct_task` abortito e atteso. | — |
| M-4 | `UPGRADE_GRACE` rimossa. | — |

Dalla lista "Da verificare":

- Comandi host bloccanti: `run_host_cmd` (processo tokio, limite 30 s, `kill_on_drop`). Il `Drop` di `NetConfig` resta bloccante.
- Candidati nell'overlay: filtrati (`tunneled_with_overlay`; `/32` della spoke nell'hub).
- `sample_host` in overflow: corretto per `/31` e `/32`.
- `create_tun` che ritenta su qualsiasi errore: lasciato com'è. I tentativi sono limitati (255) e l'errore finale è quello reale.
- MSS clamp nft non limitato alla TUN: lasciato com'è. Ritenuto innocuo perché il clamp può solo abbassare l'MSS; **non verificato a runtime**.

### Rimasti aperti

- **R-1**: riconnessione make-before-break del controllo. Richiede modifiche al protocollo e la ripresa di sessione lato server.
- **F-D lato server**: l'offer del listener resta memorizzata senza TTL. Il fix client la rende innocua, non la rimuove.
- **F-H nell'hub**: nessun monitor PMTU per le spoke.
- **F-S su macOS/Windows**: un hub host-only non è isolato. Nessun oracolo disponibile qui.
- Nonce di sessione senza contributo del client (modifica di wire).
- Interfaccia LAN dedotta solo da `advertised[0]` (gateway multi-homed con masquerade).
- `stale_reclaim` che scriverebbe `ip_forward` due volte: non controllato.
- Finestra senza heartbeat tra `VpnReady` e avvio dell'actor di controllo: solo ridotta, dal limite sui comandi host.

### Verifica eseguita

- `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`: puliti.
- `cargo test --all-features --lib`: 1018 passati, 0 falliti, 2 ignorati (990 prima dei fix).
- `vpn_liveness_test` 12, `vpn_relay_link_test` 3, `vpn_server_test` 59 (57 + 2 nuovi), `--features vpn --lib` 951, test del binario 60.
- Comando di test della CI (`--lib --bins --examples` + tutti i target `tests/*.rs` tranne `web_transfer_test`, `--skip t_ssh_ --skip t_dmx_`): 40 binari di test, 1473 passati, 0 falliti. Doc-test: 1 passato.
- `cargo clippy --features vpn --all-targets -- -D warnings` su Linux e su `x86_64-pc-windows-gnu`: puliti (su Windows due avvisi introdotti dai fix, corretti).
- Una regressione trovata e corretta durante la verifica: `tests/vpn_relay_link_test.rs` chiamava `send_batch` contando sulla backpressure del relay, che dopo F-HUB-HOL appartiene a `send_batch_wait` (la chiamata dell'uplink 1:1).

### Non eseguito

- Harness netns/sudo (`scripts/vpn_netns_test*.sh`): in questa sandbox `sudo -n` non è disponibile. Tutti i fix su route, firewall e hub sono coperti solo da unit test sulla sequenza di comandi, **non** su un kernel reale.
- `ssh_gateway_test`, `ssh_gateway_spike_test`, `web_transfer_test` (job CI separati, seriali): non eseguiti.
- Android: `cargo check` non eseguibile qui (manca il clang dell'NDK). macOS: nessun toolchain.
- macOS e Windows: nessuna esecuzione dei test; le modifiche a `NetConfig` sono sotto `cfg(target_os = "linux")`.
- Nessuna misura di throughput (F-Z2, F-Z5, F-HUB-HOL).
