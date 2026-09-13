# Piano di lavoro — finestra Ethernet 2026-09-12 19:30 → 2026-09-13 18:00

**Vincolo:** la workstation è cablata fino a domenica 13 alle 18:00 (~22,4 h).
**Obiettivo costante:** massima banda utilizzata, minima latenza, massima
stabilità. Per il **jump host** il focus è esplicitamente la **latenza**.

**Perché esiste questa finestra.** Ogni campagna che ha usato la workstation come
estremo di traffico è stata misurata in WiFi. Cablata, la linea nuda dà
**930 Mbit/s in download e 737 in upload** (P=1 ≈ P=8, RTT ~19 ms, VM
`c7i-flex.large` con tutti i contatori di allowance ENA a 0). Le campagne avevano
registrato 369–416 in download — e `vpn_direct_deficit.sh` cita **150,7**. Quindi
il download è stato sottostimato fra **2,3× e 6×** e ogni conclusione tratta su
quella direzione descrive la radio, non il prodotto.

---

## Regole che valgono per tutta la finestra

- **Una fase alla volta.** Due fasi si contendono l'unica linea che stiamo
  misurando, e le fasi su netns condividono i nomi ns0/ns1/ns2 — la pulizia
  iniziale di una cancella i namespace dell'altra a metà corsa e fabbrica
  fallimenti.
- **Mai compilare mentre una fase misura.** La CPU è parte dello strumento. Le
  ricompilazioni stanno solo nelle finestre P5 e P8.
- **Risultati isolati** in `out/eth/`: le evidenze WiFi che i documenti pubblicati
  citano restano dove sono e le due si leggono affiancate.
- **Baseline nuda riletta prima di ogni blocco**, così la deriva della linea è un
  numero e non un'impressione.
- **Ripetibilità:** ogni fase gira dal driver `scripts/perf/staging/rerun_eth.sh`
  (marker di ripresa, `timeout` per fase, provenienza commit+binario nel log).
  Niente comandi a mano che non finiscano in uno script.

---

## Calendario — RIVISTO il 12/09 alle 20:30

**Perché è cambiato.** Due fatti misurati la prima sera:

1. **Le fasi girano ~4× più veloci del preventivo.** Sette fasi del Blocco A
   chiuse in 50 minuti (214–577 s ciascuna) contro le ~5 h stimate per tredici.
   Il caso peggiore dei `timeout` non è più il vincolo.
2. **L'ordine originale (P5 build → P6 jump → P7 buchi) era sbagliato.** P5
   prevede la correzione del leak mux e quindi un rebuild di
   `target/release/bore`; ma P7 rimisura `vpn_direct_deficit` per riempire la
   colonna CPU della VM, e quel confronto vale solo **contro lo stesso binario**
   della prima esecuzione. L'attribuzione va quindi **prima** della finestra di
   build. (Il build del jump host non è più in conflitto: ora produce in
   `target/jump` e non tocca il binario della campagna.)

| # | quando | lavoro | contende |
|---|---|---|---|
| **P0** | sab 19:25 → ~22:00 | **Blocco A — VPN**, 13 fasi | rete |
| **P1** | → ~23:00 | **Blocco B — secret**, 5 fasi | rete |
| **P2** | → ~01:30 | **Blocco C — vhost lato workstation**, 11 fasi | rete |
| **P3** | → ~02:30 | **Blocco D — public lato workstation**, 6 fasi | rete |
| **P4** | → ~03:15 | **Blocco E — transfer**, 3 fasi | rete |
| **P7** | → ~04:30 | **Attribuzione** (`rerun_eth_p7.sh`): tetto del relay, latenza sotto carico, ri-esecuzione di `vpn_direct_deficit` col nuovo strumento | rete |
| **P5** | → ~06:00 | **Finestra build (nessuna misura).** (a) red-check del leak mux + correzione + gate cargo/clippy/fmt; (b) build `--features vpn,ssh-gateway` in `target/jump` e deploy sulla **VM di test** | CPU |
| **P6** | → ~09:00 | **Campagna jump host** — focus latenza | rete |
| **P7b** | → ~11:00 | Buchi di topologia VPN rimasti scoperti dopo P0 | rete |
| **P8** | → dom 18:00 | Documenti, gate netns, scansione segreti, commit, push, CI al 100 % verde | CPU |

Il margine è ora ampio. Se un blocco sfora si interrompe il driver e lo si
rilancia: i marker fanno riprendere da dove era, e le fasi tagliate vengono
dichiarate nel documento finale invece di sparire.

### Consuntivo del Blocco A — misurato, non stimato

Chiuso alle **20:51**, contro le ~22:00 previste: **1 h 27 min** per 13 fasi,
inclusa una ri-esecuzione. Tempi per fase, da `_driver.log`, così che la
prossima campagna preventivi su numeri e non su impressioni:

| fase | durata | fase | durata |
|---|---|---|---|
| `link_baseline` | 68 s | `vpn_udpbuf` | 577 s |
| `vpn_ab` | 774 s | `vpn_cc_matrix` | 981 s |
| `vpn_direct_deficit` | 318 s | `vpn_lat` | 107 s |
| `vpn_wire_ceiling` | 309 s | `vpn_profile` | 261 s |
| `vpn_txqueue` | 214 s | `vpn_modes` | 418 s |
| `vpn_sndbuf` | 256 s | `vpn_hub` | **19 s** ← anomalia, vedi sotto |
| | | `vpn_stability` | 403 s |

**Una durata fuori scala è un dato diagnostico.** `vpn_hub` ha chiuso in 19 s con
`rc=0`: non perché fosse veloce, ma perché la sua misura di banda veniva saltata
in silenzio (indirizzo overlay dell'hub mai risolto, e la guardia
`if [ -n "$HUB" ]` che saltava tutto senza dirlo). Vale come regola operativa:
**confrontare la durata di una fase con il suo preventivo prima di leggerne il
risultato** — 19 s contro un `timeout` di 2400 s era l'unico segnale visibile
dal log, ed è quello che ha portato a riaprirla.

I preventivi dei `timeout` restano generosi di proposito: il caso peggiore di
una fase è un link che non si stabilizza, non la sua durata nominale.

---

## Ripetibilità

Tutto quello che segue è nel repository e non richiede di ricostruire nulla:

| cosa | dove |
|---|---|
| driver dello sweep | `scripts/perf/staging/rerun_eth.sh` |
| driver dell'attribuzione | `scripts/perf/staging/rerun_eth_p7.sh` (si rifiuta di partire se il primo è vivo) |
| runbook completo dell'harness | `scripts/perf/staging/README.md` — §3.8 VPN, §3.9 jump host, §3.10 **qualificare la linea prima di citare qualunque valore assoluto**, e le 13 trappole |
| evidenze del cablato | `docs/performance/ETH_RERUN_EVIDENCE_2026-09-12.md` |
| coordinate e credenziali | `~/.config/bore-perf/env.sh` (fuori dal repo, 600); il template vuoto è `scripts/perf/staging/env.sh.example` |

I risultati vanno in `out/eth/` (ignorato da git): un marker `_done.<fase>` per
fase conclusa, un `<fase>.out` per fase, e `_driver.log` con provenienza
(commit, checksum del binario, velocità della NIC) e baseline per blocco.

---

## P6 — jump host: cosa si misura e perché

Il jump host è l'unico modo d'uso in cui **la banda non è il prodotto**: un
`ssh -J` porta una sessione interattiva e un `direct-tcpip`, quindi quello che
l'utente sente è il **tempo di andata e ritorno**, non il throughput. Le
grandezze sono quindi:

1. **Tempo di apertura sessione** — da `ssh -J` a prompt utilizzabile, scomposto
   in: TCP verso il server, handshake SSH esterno, apertura del canale
   `direct-tcpip`, handshake SSH interno. È il numero che l'utente percepisce
   come "quanto ci mette a entrare".
2. **RTT applicativo a sessione aperta** — latenza di un singolo tasto
   (`ssh -J … true` ripetuto non basta: misura di nuovo il setup). Si misura con
   un comando che rimbalza un byte sulla sessione già stabilita.
3. **Relay contro diretto (QUIC).** Il jump host nativo (`sshjhost --udp`)
   riusa l'unico endpoint `--vhost-quic-port` con chiave `jump:<alias>`. Le due
   strade vanno misurate **appaiate nella stessa ripetizione**, perché è l'unico
   modo in cui la deriva si annulla.
4. **Effetto dei carrier** sul percorso diretto: ogni canale SSH usa esattamente
   uno stream bidi, quindi i carrier qui comprano isolamento, non banda — va
   verificato che non costino latenza.
5. **Stabilità**: sessione tenuta aperta, rekey attraversato, comportamento al
   ritorno su relay warm quando l'UDP muore.

**Prerequisito già verificato:** il binario in uso NON ha la feature
`ssh-gateway` (`--ssh-gateway` assente; `sshjhost` c'è perché è lato client).
Serve un rebuild `--features vpn,ssh-gateway` su entrambi gli estremi — fatto in
P5, sulla **VM di test**, mai sullo staging (un redeploy dello staging riavvia il
server e fa cadere i tunnel vivi dell'utente: richiede approvazione esplicita e
non è in questo piano).

---

## Cosa NON viene rimisurato, e perché

Le fasi `vm/`, `pub/vm_*` e `srv/` girano VM↔server dentro la stessa regione AWS:
l'accesso di questa workstation non è mai stato nel loro percorso dati, quindi il
WiFi non può averle distorte. Restano valide come sono.

Restano validi anche tutti i **rapporti** contro un controllo nudo campionato
nella stessa ripetizione: è precisamente la ragione per cui le campagne li
misurano così (V-9).

---

## Difetto aperto che attraversa la finestra

Una connessione TCP di controllo verso il server viene persa **ad ogni
riconnessione** del connector VPN (misurato 1→2→3→4 `ESTABLISHED`, mai
riassorbite in 120 s, contate da `ss`). Meccanismo sospetto in `src/mux.rs`
(`drive()` esce solo quando chiude il *peer*), **non ancora confermato**: il
red-check e le tre opzioni di correzione, con la ragione per cui quella ovvia è
sbagliata, stanno nella nota di lavoro e vanno eseguiti in P5 prima di decidere.

### Consuntivo dei blocchi B–E — misurato

Il Blocco A aveva chiuso in 1 h 27 min contro le ~2 h 35 preventivate. Il resto,
letto da `_driver.log` e non stimato:

| blocco | da → a | durata | fasi |
|---|---|---|---|
| B — secret | 20:51 → 21:53 | **1 h 02** | 5 |
| C — vhost lato workstation | 21:53 → 23:57 | **2 h 04** | 9 |
| D — public lato workstation | 23:57 → 00:56 | **0 h 59** | 6 |
| E — transfer | 00:56 → 01:13 | **0 h 17** | 3 |

Fasi più lunghe, utili per preventivare: `sec_ack` 1635 s, `sec_ab` 1604 s,
`ws_flavours_rot` 1607 s, `ws_flavours` 1596 s, `ws_dufs_rq` 1090 s,
`ws_tunnel_paired` 2112 s. Tutte e sei sono fasi a **bracci multipli con
raffreddamento**, e il raffreddamento — non il trasferimento — è la maggior
parte del tempo: `ws_conns` muove 460 MiB per gradino in ~5 s e spende 75 s
fra un braccio e l'altro.

**Il totale della prima passata è 5 h 49 per 36 fasi**, contro le ~14 h del
piano originale. Il vincolo reale della finestra non è stata la misura: è stata
la *scrittura* dei difetti trovati mentre girava.

Fasi aggiunte durante la finestra, non previste dal piano (tutte nate da una
domanda che una fase esistente non poteva chiudere):

| fase | perché | durata |
|---|---|---|
| `asym_qualify` | di chi è il tetto: della linea o della destinazione | 213 s |
| `ws_tunnel` | mai girata: `$B` non inizializzato sotto `set -u` | 620 s |
| `pub_ws_first_conn` | attribuire il costo della prima connessione | 477 s |
| `pub_origin_cpu` | escludere la CPU su origine, VM e client | 348 s |
| `pub_ws_conns_procs` | l'ultima variabile lato client: 1 processo × n contro n × 1 | in corso |
