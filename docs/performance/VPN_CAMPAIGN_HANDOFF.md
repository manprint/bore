# Campagna VPN — handoff di stato

**Data:** 2026-09-12. **Scopo:** ripartire senza ricominciare da capo.
Le credenziali NON stanno qui: vivono in `~/.config/bore-perf/env.sh` (chmod 600),
nello scratchpad di sessione e in `~/env.sh` sulla VM. Coordinate (IP, host,
utenti) fornite a parte.

---

## 0. Aggiornamento — finestra Ethernet, sera del 12/09

La workstation è cablata fino a **domenica 13 alle 18:00**. Lo sweep gira da
`scripts/perf/staging/rerun_eth.sh` (seriale, riprendibile, output isolato in
`out/eth/`). Piano e calendario rivisti in `ETH_CAMPAIGN_PLAN.md`; evidenze in
`ETH_RERUN_EVIDENCE_2026-09-12.md`.

**Il risultato principale della serata è metodologico, e vale per tutte le
campagne, non solo per la VPN:** il WiFi non ha soltanto ridotto i numeri, ha
*prodotto* conclusioni che sul cablato non esistono. Fino a stasera:

| fase | su WiFi | sul cablato |
|---|---|---|
| `vpn_ab` | direct 62.7 % di bare (premessa di V-6) | direct **89.3 % / 94.2 %**; il relay è il braccio lento |
| `vpn_direct_deficit` | deficit del 37 % da spiegare | **non si riproduce**; direct costa anche meno CPU per GiB |
| `vpn_wire_ceiling` | 540M offerti → 388 @ 227 ms | **1.000 tun/bare fino a 700 Mbit/s**, loss 0.0 %, coda 0.3 ms |
| `vpn_txqueue` | default 500 = peggior gradino, 6× latenza | cinque gradini **piatti** (699.9–706.5 Mbit/s, 37.9–39.1 ms) |
| `vpn_sndbuf` | ottimo a 32 MiB, curva che gira | 512 KiB ≡ 8 MiB: **16× di differenza, zero effetto** |
| `vpn_udpbuf` | — | 256 KiB → 16 MiB tutti al 92–95 % di bare |

I default spediti sono tutti confermati. **Ma non va arrotondato a "le manopole
non servono":** la misura di V-10 era una misura vera di una radio vera, e la
radio è la linea che ha una quota grossa degli utenti. Cambia lo *scope* della
frase in CLAUDE.md ("il default del kernel è il peggior gradino su entrambi gli
assi" è vero su link congestionato o lossy, falso su cablato scarico), non il
default.

### Difetti di strumentazione trovati e corretti stasera

1. **La colonna CPU della VM era cieca da tutta la campagna.** Il selettore
   confrontava `/proc/<pid>/comm` con la stringa `bore`, ma il binario è
   *deployato* come `~/bore-vpn`, quindi `comm` legge `bore-vpn`. Ora seleziona
   per **esclusione** (scarta `sudo`/`env`/`sh`/`bash`/vuoto), che scarta anche
   la shell ssh che matcha il proprio argv. Red-check su link vivo: vecchio
   filtro 0 pid, nuovo filtro il pid giusto con tick che si muovono.
   `vpn_direct_deficit` è in coda per la ri-esecuzione in P7.
2. **`vpn_udpbuf` non vedeva la propria variabile indipendente** (`granted ?`).
   Serio: `net.core.wmem_max` qui è 4 MiB, quindi senza quella colonna il
   gradino da 16 MiB poteva essere lo stesso buffer di quello da 4 e la scala
   piatta sarebbe stata un artefatto. Verificato dal kernel: `forced=true`,
   `effective_recv=33554432` — **16 MiB concessi su un host con `rmem_max` 4
   MiB**, quindi il percorso `SO_*BUFFORCE` funziona e i gradini sono distinti.
   La fase ora stampa il valore atteso accanto a quello concesso e dice
   `CLAMPED` se divergono (Linux riporta `SO_SNDBUF` **doppio** del richiesto).
3. **Il build del jump host sovrascriveva `target/release/bore`**, cioè il
   binario con cui è misurata tutta la campagna e il cui checksum finisce nella
   provenienza dei driver. Ora produce in `target/jump` e verifica che
   `--ssh-gateway` sia davvero nel binario risultante.
4. **Un apostrofo dentro un programma `awk` fra apici singoli** chiudeva il
   programma in due script nuovi. `bash -n` non lo vede, `shellcheck` sì
   (SC1011). Corretti e passato tutto `scripts/perf` per la stessa classe.
5. **La guardia del driver P7 matchava la propria shell** — stessa trappola che
   in questa campagna ha già ucciso un driver *e* la shell che l'aveva lanciato.
   Ora scandisce per identità ed esclude sé e tutti gli antenati.
6. **Sei fasi scartavano il valore di ritorno di `wait_mtu_settle`** scrivendo
   `wait_mtu_settle X >/dev/null 2>&1`, che butta via *anche* lo stato d'uscita:
   una MTU mai stabilizzata passava per stabilizzata. Una TUN appena creata sale
   1350 → 1288 → 1414 in ~25 s, quindi misurare dentro la salita dà un MSS corto
   (Mathis) e, se la MTU *cambia* a metà trasferimento, pacchetti scartati
   `TooLarge` in aggiunta. Corretto in `cc_matrix`, `udpbuf`, `direct_deficit`,
   `modes`, `stability`, `txqueue`, `profile`; il gradino `flows=2` di
   `vpn_profile` (536.85 contro 820–874) era esattamente questo.
7. **`vpn_hub` misurava zero banda e dava un verdetto che la sua topologia non
   può sostenere.** L'indirizzo overlay dell'hub veniva estratto da una riga di
   log con un pattern che non matchava nulla: `HUB` restava vuoto e la guardia
   `if [ -n "$HUB" ]` saltava *in silenzio* sia la raggiungibilità sia il
   throughput — la fase chiudeva in 19 s con rc=0 senza aver misurato banda. E
   la sonda di isolamento fra spoke faceva `ping` a un indirizzo **locale**
   (i due spoke girano entrambi su questa workstation), che il kernel risponde
   dalla tabella `local` senza mai passare dall'hub: stampava "isolation
   broken" su un hub che isola perfettamente. Ora legge l'indirizzo dal kernel
   dell'hub, dichiara la sonda NON MISURABILE in questa topologia indicando chi
   la copre (`T-HUB*` in netns), e trasforma il conteggio dei check round
   autenticati in un verdetto esplicito.
8. **Nessun driver registrava il binario dell'estremo remoto.** Ogni fase VPN ha
   due peer e la riga di provenienza ne descriveva uno solo; `rerun_eth_p7.sh`
   ora logga `far bin:` (checksum + versione) prima di qualunque misura.
9. **`vpn_stability` non aveva un controllo nudo dentro il ciclo** — l'unica
   cosa che V-9 vieta. È la fase che ha misurato il crollo di throughput al
   terzo riaggancio (§15 delle evidenze) e il suo stesso output non poteva
   distinguere un tunnel che degrada da una linea che degrada. Ora campiona
   `bare` e il delta di allowance della VM in ogni ciclo.

### Nuovi strumenti, già scritti e non ancora eseguiti

| script | risponde a |
|---|---|
| `vpn/vpn_relay_attrib.sh` + `vpn/attrib_net.py` | di chi è il tetto del relay: delta di allowance attorno al braccio, le due tratte separate, **lo stesso doppio transito senza bore**, CPU dell'host che rilancia |
| `vpn/vpn_rtt_load.sh` | i ~12 ms di pavimento sotto carico: li aggiunge il tunnel o li aggiunge il carico? |
| `vpn/vpn_ctrl_leak.sh` | il leak della connessione di controllo, come stage permanente |
| `vpn/vpn_overhead.sh` | byte sul filo per byte consegnato, **risolvendo** l'overhead per frame invece di assumerlo |
| `rerun_eth_p7.sh` | driver dell'attribuzione — ora **9 fasi**: `vpn_relay_attrib`, `vpn_rtt_load`, `vpn_direct_deficit_r2`, `vpn_ctrl_leak`, `vpn_cc_matrix_r2`, `vpn_udpbuf_r2`, `vpn_profile_r2`, `vpn_hub_r2`, `vpn_stability_r2` |
| `lint.sh` | gate statico mirato alla *scorrettezza silenziosa* (SC1011/SC1012 e affini), non allo stile |

`attrib_net.py` esiste perché il server di staging **non ha né iperf3 né socat**
e installare pacchetti sull'host che porta i tunnel vivi dell'utente non è una
decisione che spetta a un benchmark. Usa `os.splice`, quindi Python non tocca i
byte; e la fase **verifica lo strumento invece di fidarsene** (un braccio
`bare-py` sullo stesso percorso che iperf3 misura, più una tratta a un solo
salto), dichiarando il controllo un *pavimento* invece che un verdetto se una
delle due verifiche fallisce.

---

## 1. Dove siamo

La campagna VPN ha prodotto **tre difetti reali corretti** e **un difetto ancora
aperto**. Tutto il resto misurato è comportamento della linea, non del prodotto.

### Corretti e gated

| id | difetto | correzione | gate |
|----|---------|-----------|------|
| V-1 | — vedi `VPN_EVIDENCE_2026-09-12.md` §3 | — | — |
| V-2 | il diagnostico non attraversava la rete come il prodotto | round di check autenticato in `udp_diagnostic.rs` | `T-NAT-DIAG-ROUND` (netns) + 3 unit |
| V-10 | coda del device TUN: il default kernel (500) è il peggior valore su **entrambi** gli assi | `VPN_TUN_TXQUEUELEN = 128`, override `BORE_VPN_TUN_TXQUEUELEN` | `tun_txqueuelen_resolution` + `vpn_txqueue.sh` |
| V-13 | il "tetto a ~400 Mbit/s" non esiste: è il punto di lavoro di una coda drop-tail sotto sorgente che non rallenta | default resta 8 MiB, **tetto** del knob alzato a 64 MiB | `datagram_send_buffer_unset_is_the_shipped_constant` |

Stato gate al momento dell'handoff: 1025 test unit/integration verdi, netns
`udp_nat` **29/0**, netns `vpn` **161/0**, clippy verde in tre configurazioni,
fmt pulito.

### Aperto — l'unico difetto ancora in piedi

**Una connessione TCP di controllo verso il server viene persa ad ogni
riconnessione del connector VPN.**

Misurato (sonda con finestra di assestamento, `fdprobe2.sh`):

```
fresh link          : ctrl_conns=1 fds=12
after reconnect 1   : ctrl_conns=2 fds=13
after reconnect 2   : ctrl_conns=3 fds=14
after reconnect 3   : ctrl_conns=4 fds=15
t+30s / t+60s / t+90s / t+120s : ctrl_conns=4 fds=15   (mai riassorbite)
```

Le quattro socket restano `ESTABLISHED` verso la porta di controllo del server.
Contate dal **kernel** (`ss`), non dal log — regola P-12.

Una sonda precedente aveva letto `12 → 13 → 14 → 15 → 13` e sembrava non
monotona: quel calo era il campionamento che cadeva dentro la riconnessione, non
un reaper. **Non è stato un leak "smentito e poi riconfermato": la prima misura
non era conclusiva e lo dice il suo stesso campione.**

**Meccanismo sospetto (da confermare, NON ancora provato):** in `src/mux.rs`,
`spawn_driver` mette la `yamux::Connection` — e quindi la socket TCP — dentro un
task `tokio::spawn`. `drive()` esce solo su `Step::Done`, cioè quando è il
**peer** a chiudere. Se `Opener` e `Acceptor` vengono droppati, il task continua
a girare: `openers_gone` ferma solo l'accettazione di nuove richieste di apertura
e il `send` sull'`inbound_tx` morto è `let _ =`. Quindi il lato client non chiude
mai una connessione di cui ha finito di avere bisogno, e resta parcheggiato su
`poll_next_inbound` per sempre.

Perché non chiude nemmeno il server va verificato: se il substream di controllo
si chiude correttamente, il server dovrebbe vedere EOF e chiudere, e allora il
client uscirebbe con `Step::Done`. Il fatto che la socket resti `ESTABLISHED` da
entrambi i lati dice che questa catena **non** si completa.

**Prossimo passo, in questo ordine:**

1. Red-check a livello di `mux`, senza VPN: aprire una coppia client/server su
   `TcpStream`, droppare `Opener` + `Acceptor` sul client, verificare se il
   server vede la chiusura. Se resta appesa, il meccanismo è confermato e il
   test è già il gate.
2. Solo dopo, decidere la correzione. Attenzione: `drive()` **deve** continuare a
   girare finché esistono substream vivi (i substream di relay sopravvivono
   all'`Opener` — è il caso normale), quindi "esci quando l'Opener sparisce" è
   sbagliato e romperebbe il relay. Serve un criterio che consideri anche i
   substream ancora in vita.
3. Gate di campo: la sonda dei descrittori, che è ciò che ha trovato il difetto.

Contesto di stabilità in cui è emerso (3 cicli × 3 round × 20 s): RSS
22 840 → 86 136 KiB con plateau tra i cicli 2 e 3, thread piatti a 18–19, fd
12 → 13 → 14, 3 switch di bridge, 0 fallback su relay, 4 righe WARN. Il criterio
dello stadio è "un leak è RSS che sale **insieme** a thread o fd; RSS da solo che
si assesta è l'allocatore" — thread piatti e RSS in plateau, quindi tutta la
domanda stava sulla colonna fd, e la sonda dedicata ha risposto.

---

## 2. La linea di accesso era il collo di bottiglia — RISOLTO, e invalida i numeri assoluti

Durante la campagna la workstation era in **WiFi**. Il 2026-09-12, collegata in
**Ethernet** (`eno0`, 1000 Mbit/s full duplex, RTT ~19 ms verso la VM), la linea
nuda misura:

| | download | upload |
|---|---|---|
| P=1 | 926 / 865 / 926 | 735 / 735 / 735 |
| P=8 | 935 / 934 / 935 | 741 / 741 / 740 |
| **mediana** | **928 Mbit/s** | **737 Mbit/s** |
| WiFi, stessa sera | 147 / 153 | 376 / 364 |
| "bare" citato dalla campagna | 369–416 (e 150,7 in V-6) | 677–705 (e 414,5 in V-6) |

P=1 ≈ P=8 in entrambe le direzioni: nessun limite per-flusso, la linea consegna
e basta (regola V-9).

### Cosa questo invalida

**La linea vera è veloce in DOWNLOAD (928) e più lenta in UPLOAD (737).** La
campagna aveva registrato l'opposto e io l'avevo spiegato come "asimmetria
propria dell'accesso": era un artefatto del WiFi. In particolare:

- L'**upload** in campagna (677–705) era vicino al vero (737): compresso ~7 %,
  quindi le conclusioni sull'upload reggono in prima approssimazione.
- Il **download** in campagna (369–416, e **150,7** nell'intestazione di
  `vpn_direct_deficit.sh`) era sotto di **2,3×–6×**. Tutto ciò che è stato
  concluso sul download è stato misurato contro la radio, non contro il tunnel.
  In particolare V-7 ("il download è piatto al variare del numero di flussi,
  path-limited a ~375, tunnel già al 100 %") descrive il WiFi.
- Le scale offerte delle ladder UDP (375M / 450M / 540M) erano state scelte
  contro una banda nuda che ora si sa essere molto più alta: il punto di
  ribaltamento di V-13 va ricercato con il margine che ora esiste.

### Cosa NON invalida

I **rapporti** contro un controllo nudo campionato nella **stessa ripetizione**
restano validi su qualunque linea (V-9): è esattamente la ragione per cui la
campagna li misura così. Restano validi anche tutti i risultati che non passano
dall'accesso di questa workstation — le fasi `vm/`, `pub/vm_*` e `srv/` girano
VM↔server dentro la stessa regione AWS.

### Il ri-run

`scripts/perf/staging/rerun_eth.sh` ri-esegue in Ethernet **ogni** fase che usa
la workstation come estremo di traffico (38 fasi: tutto `vpn/`, tutto `ws/`,
`pub/ws_*`, tutto `sec/`, tutto `xfer/`), in serie, con marker di ripresa, sotto
`timeout`, scrivendo in `out/eth/` per non toccare le evidenze WiFi che i
documenti pubblicati citano. Rilegge una baseline nuda prima di ogni blocco, così
la deriva della linea è un numero e non un'impressione.

---|---|---|
| P=1 | 147 Mbit/s | 376 Mbit/s |
| P=8 | 153 Mbit/s | 364 Mbit/s |
| baseline di campagna | 369–416 | 677–705 |

**Il parallelismo non compra nulla** (147 → 153): per la regola V-9 questo è un
tetto rigido, non un limite per-flusso (finestra, perdita, Mathis) — quelli si
aprono con P=8. Entrambe le direzioni sono circa dimezzate rispetto alla
baseline, il download molto peggio.

Stato radio al momento della misura: −50 dBm, HE-MCS 9/10, 80 MHz, NSS 2,
rx bitrate 960,7 Mbit/s, tx 1080,6 — **la radio non è il tetto**.
`Power save: on` sull'interfaccia WiFi: agisce sul lato RX e non tocca il TX, il
che è coerente con "upload accettabile, download peggiore", ma non è stato
provato che sia la causa e non è un'impostazione toccata dalla campagna.

Un'altra workstation sulla stessa WiFi arriva a 800 Mbit/s in download: la linea
li consegna, quindi il problema è di questa stazione.

**Dopo il riavvio, prima di qualsiasi altra misura**, ripetere esattamente le
quattro celle qui sopra e confrontarle. Finché il numero nudo non torna alla
baseline, **nessun risultato assoluto della campagna è confrontabile con quelli
già pubblicati** — i rapporti contro un controllo nudo preso nella stessa
ripetizione restano invece validi (V-9).

---

## 2-bis. Esaminato e RESPINTO — la morte del relay caldo mentre si è su direct

`run_bridge` marca `relay_dead = true` e **non** ritenta mai il relay caldo, quindi
la promessa DEC-2 (caduta su relay caldo *in place*, TUN e contatore nonce
preservati) è vuota da quel momento in poi: se dopo muore anche il direct si fa
una riconnessione completa. Sembra un buco di resilienza. Non lo è, e la ragione
sta nella topologia, non nel codice: i substream di relay e il substream di
controllo vivono sulla **stessa** connessione yamux, quindi se il TCP cade cadono
insieme e l'attore di controllo dichiara il link morto comunque. Il relay può
morire da solo **solo** perché il peer ha chiuso i suoi substream — cioè perché
il peer non c'è più, che è esattamente il caso in cui non esiste niente su cui
ricadere. Misurato nel log della corsa del 12/09 (ciclo 1): il listener sulla VM
riceve SIGTERM, il relay muore alle 18:46:35, il direct muore 11 s dopo (idle
timeout QUIC 10 s) e il connector riconnette.

Resta corretto che la condizione sia un `warn!` e non un `debug!`: descrive una
degradazione reale della prossima caduta. **Non riaprire senza un caso misurato
in cui il relay muore mentre entrambi i peer sono vivi.**

## 3. Cosa manca

1. **Chiudere la questione dei descrittori** (§1) — è l'unico possibile difetto
   aperto. Red-check, meccanismo sospetto e le tre opzioni di correzione (con la
   ragione per cui quella ovvia è sbagliata) stanno in
   `docs/vpn/VPN_CTRL_CONN_LEAK.md`. Da eseguire in P5, mai mentre si misura.
2. **`scripts/perf/staging/vpn/vpn_modes.sh`** — mai eseguito: gateway `--advertise`,
   netmap `real@virtual`, `--forward-accept`. È l'ultimo buco di copertura.
   `LAN_HOST` non è impostato, quindi la sonda sulla catena FORWARD verrà saltata
   rumorosamente (e va bene così, ma va detto nel documento).
3. **Sezioni mancanti di `docs/performance/final_vpn_perf_review.md`**: stabilità
   e leak, modi, domande aperte, chiusura. §0–§6 sono scritte.
4. **Riportare stabilità e modi anche in `VPN_EVIDENCE_2026-09-12.md`.**
5. **Commit + push + CI verde al 100% con `gh`.** Prima del push: grep del diff in
   staging per segreti, IP di staging, IP reflexive di casa, dominio e indirizzi
   VPC — la lista sta nelle regole operative, non qui.
6. ~~**Ri-run completo su Ethernet**~~ — in corso, vedi §0.
7. **Validazione hub su path reale** — richiede TCP 7836 in ingresso sulla VM di
   test, oppure un redeploy dello staging (che riavvia il server e fa cadere i
   tunnel vivi dell'utente: **serve approvazione esplicita**).

---

## 4. Come ripartire

```bash
cd /mnt/fabio/dati/Git/Github-manprint/bore-forked
. ~/.config/bore-perf/env.sh          # coordinate + credenziali, fuori dal repo

# 0. la linea è tornata? (blocca tutto il resto finché non torna)
iperf3 -c "$BORE_VM" -p 5299 -R -P 1 -t 8 -J | jq '.end.sum_received.bits_per_second/1e6'
iperf3 -c "$BORE_VM" -p 5299 -R -P 8 -t 8 -J | jq '.end.sum_received.bits_per_second/1e6'

# 1. gate locali (footprint basso: nice, -j 3, --test-threads=2)
nice -n 19 cargo test --features vpn -- --test-threads=2
nice -n 19 cargo clippy --all-targets --features vpn -- -D warnings

# 2. netns — MAI due harness in parallelo (ns0/ns1/ns2 condivisi)
sudo -n "$PWD/scripts/vpn_netns_test.sh"
sudo -n "$PWD/scripts/udp_nat_netns_test.sh"

# 3. lo sweep cablato (seriale, riprendibile: i marker in out/eth/ fanno saltare il fatto)
nohup ./scripts/perf/staging/rerun_eth.sh & echo "driver pid $!"   # ANNOTA il pid
tail -f out/eth/_driver.log

# 4. le fasi di attribuzione (si rifiutano di partire se lo sweep è ancora vivo)
nohup ./scripts/perf/staging/rerun_eth_p7.sh & echo "driver pid $!"

# 5. l'ultimo stadio mai eseguito
scripts/perf/staging/vpn/vpn_modes.sh            # senza sudo: il glob sudoers non attraversa '/'
```

**Per fermare un driver: per PID annotato, mai `pkill -f rerun_eth.sh`** — quel
pattern matcha la riga di comando della shell che lo contiene, cioè la tua. In
questa campagna ha già ucciso driver e shell insieme.

Regole di misura consolidate (le sei in `VPN_EVIDENCE_2026-09-12.md`): bracci
interlacciati, controllo nudo nella stessa ripetizione, celle FALLITE stampate e
mai mediate, rotta riletta e mai assunta, `LC_ALL=C` per ogni `sort -n`, campioni
grezzi stampati accanto a ogni mediana, e una run verde che non finisce con il
proprio totale PASS/FAIL non è stata letta.

---

## 5. Stato dell'albero git

Branch `main`. Non committato al momento dell'handoff:

- modificati: `CLAUDE.md`, `README.md`, `scripts/udp_nat_netns_test.sh`,
  `scripts/vpn_netns_test.sh`, `src/adaptive_nat.rs`, `src/holepunch.rs`,
  `src/shared.rs`, `src/udp_diagnostic.rs`, `src/vpn.rs`, `src/vpn_server.rs`
- non tracciati: `scripts/vpn_tun_endpoint.sh`, `scripts/perf/staging/vpn/`,
  `docs/performance/VPN_EVIDENCE_2026-09-12.md`,
  `docs/performance/final_vpn_perf_review.md`, questo file,
  `docs/vhost/VHOST_PERFORMANCE_ASSESSMENT_2026-09-07.md` (preesistente)

## 6. Stato della workstation al riavvio

Puliti prima del riavvio: nessun processo `bore` della campagna vivo, nessuna
interfaccia `bore*`, nessuna rotta overlay. Restavano 15 processi
`scripts/bench_origin.py` che ignorano SIGTERM (residuo di campagne precedenti,
CPU 0%, nessun traffico): il riavvio li elimina.

**Non toccati e da non toccare:** il tunnel `vhost` dell'utente in Docker e le
voci vhost vive `dufspcloud`, `tennis`, `tennis1`. `ip_forward=1` è di Docker,
non della campagna.
