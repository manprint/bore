# bore su VM IONOS (staging `prx.adiprint.it`) — banda, tuning e bug trovati

Data: 2026-10-10. VM `ubuntu@217.154.11.34`, deploy in `/home/ubuntu/docker/bore/app`
(`compose.yml`, `config.yml`). Server in produzione: immagine `ghcr.io/manprint/bore:sha-da621a1`
(1.2.1 dev). Client di misura: workstation con collegamento cablato a 1 Gbit/s, RTT verso la VM
circa 35 ms (l'ICMP è bloccato, quindi l'RTT è stimato dai tempi delle connessioni TCP).

## TL;DR

| Cosa | Prima | Dopo |
|---|---|---|
| Tunnel pubblico, `--carriers 1`, download (server → client) | 527–733 Mbit/s | **930–938 Mbit/s** (line rate) |
| Tunnel pubblico, `--carriers 1`, upload (client → server), client non tunato | 575–740 Mbit/s | invariato: il limite è sul **client** |
| Tunnel pubblico, `--carriers 1`, upload, client tunato | — | **716–748 Mbit/s** (line rate in upload) |
| Slot QUIC direct liberi sul server di produzione | **0 su 8** (10 664 rifiuti) | 8 su 8 dopo il riavvio; il bug è corretto nel codice |

- **Lato server, la vittoria più grande:** tre `sysctls:` TCP nel compose, già applicati in produzione.
  Ora una sola connessione TCP satura il link in download.
- **Lato client, la vittoria equivalente:** gli stessi tre valori sulla macchina client, e in più
  `net.core.rmem_max`/`wmem_max` per QUIC. Così una sola connessione satura anche l'upload.
- **Bug P-15, trovato e corretto nel codice.** Il server perdeva per sempre gli slot QUIC direct
  ogni volta che un tunnel `--udp` si ri-registrava. In produzione gli slot erano tutti esauriti
  e ogni nuovo tunnel `--udp` finiva in silenzio sul relay TCP. La correzione è nel working tree
  e non è ancora rilasciata: va fatto commit e build dell'immagine.
- **Slot QUIC: restano 8, ed è una scelta misurata.** Ogni slot sotto carico costa fino a
  ~82 MiB di RAM. Per avere più slot a parità di RAM bisogna rimpicciolire le finestre, e la
  singola connessione QUIC cala del 20–33%. Su una VM da 2 GB con seafile accanto gli 8 slot da
  64 MiB occupano già tutta la RAM libera.
- **QUIC direct contro relay TCP:** su un link pulito e cablato QUIC non è più veloce del relay
  tunato. Su 2 vCPU costa circa 3 volte la CPU per GB e si ferma intorno a 650–700 Mbit/s per
  direzione, limitato dalla CPU. Ha senso su link con perdite o variabili (Wi-Fi, 4G/5G,
  hotspot), non come default su fibra o cavo.

---

## 1. Ambiente misurato

### VM IONOS
- 2 vCPU, 1.8 GB RAM, swap 2 GB (circa 410 MB già in uso). Accanto a bore gira `seafile-13-dind`,
  che usa circa 690 MB.
- Kernel 6.8, Docker 29.8.2. Container `bore-server` su rete bridge (`172.20.0.2`), `user: 0:0`.
- Host:
  - `net.core.rmem_max` / `wmem_max` = 16 MiB (corretto per QUIC: `ss` mostra `rb`/`tb` 32 MiB
    sul socket 443/udp di bore).
  - Congestion control `cubic`. Sono disponibili solo `reno` e `cubic`: il modulo `tcp_bbr`
    esiste su disco ma non è caricato.
  - qdisc `fq_codel`.
- Network namespace del container **prima** del tuning: `tcp_wmem` max 4 MiB, `tcp_rmem` max 6 MiB,
  `tcp_slow_start_after_idle=1`.
- Firewall cloud IONOS: aperte solo le porte di bore. Per i test sono state aperte anche
  **5201 e 5202 TCP+UDP**: **vanno richiuse** (vedi §8).

### Workstation (client)
- `net.ipv4.tcp_wmem` max **4 MiB** (default del kernel). `tcp_rmem` max 32 MiB.
- `net.core.rmem_max` / `wmem_max` = **4 MiB**. Il client bore lo segnala con un warning di
  "UDP buffer clamped".
- `cubic`, `tcp_slow_start_after_idle=1`.

### Banda grezza (iperf3 senza bore, workstation ↔ VM)

| | Download (VM → ws) | Upload (ws → VM) |
|---|---|---|
| 1 flusso TCP (P1) | 608–753 Mbit/s | 540–643 Mbit/s |
| 4–8 flussi TCP (P4/P8) | **940–941 Mbit/s** | **692–756 Mbit/s** |
| UDP 900 Mbit/s offerti | la VM ne emette al massimo ~603 (CPU) | ~756 consegnati, 15% di perdita |

Il link è quindi circa **940 down / 750 up**, e questi sono i riferimenti "100%". A un solo
flusso TCP non si arriva al link: il limite è la finestra (window / RTT), non la rete. Questo è
esattamente il punto su cui interviene il tuning descritto sotto.

---

## 2. La vittoria principale: finestre TCP a entrambi i capi

### 2.1 Perché
Una connessione TCP non può avere in volo più dati della sua finestra, quindi va al massimo a
circa `finestra / RTT`.

- Con `tcp_wmem` max 4 MiB, la finestra effettiva è circa 2.7–3.1 MiB (il kernel usa una parte
  del buffer per l'overhead). A 35 ms questo fa **610–720 Mbit/s**, esattamente quello che bore
  misurava con `--carriers 1`.
- Il relay di bore non è il collo di bottiglia: con più carrier, cioè più connessioni TCP in
  parallelo, raggiungeva già il 99–100% del link.

Il relay usa una connessione TCP lunga (il carrier) tra client e server, e in essa multiplexa
tutte le connessioni proxate:
- **Download** (server → client): conta il buffer di **invio** del server (`tcp_wmem` nel netns
  del container) e quello di ricezione del client (`tcp_rmem`).
- **Upload** (client → server): conta il buffer di **invio** del client (`tcp_wmem` della
  workstation) e quello di ricezione del server (`tcp_rmem` nel container).

Va quindi tunato **ogni lato che invia**.

### 2.2 Lato server — APPLICATO in produzione
Nel `compose.yml` del servizio `bore-server`:

```yaml
    sysctls:
      net.ipv4.tcp_wmem: "4096 65536 16777216"
      net.ipv4.tcp_rmem: "4096 131072 16777216"
      net.ipv4.tcp_slow_start_after_idle: "0"
```

- Le chiavi `net.ipv4.tcp_*` sono **per network namespace**, quindi un container bridge può
  impostarle da sé. Non vale invece per `net.core.rmem_max`/`wmem_max`, che sono globali e
  vanno impostati sull'host (già fatto su questa VM).
- I valori sono **tetti, non prenotazioni**: l'autotuning del kernel fa crescere il buffer solo
  per le connessioni che ne hanno bisogno (quelle con alto bandwidth-delay product). Una
  connessione idle o lenta resta piccola.
- `tcp_slow_start_after_idle=0`: un carrier rimasto fermo qualche secondo riparte alla finestra
  che aveva, invece di rifare lo slow start. Questo conta per il traffico a raffiche (pagine
  web, sync), tipico di un tunnel.

Verifica nel container in esecuzione:

```shell
P=$(docker inspect -f '{{.State.Pid}}' bore-server)
sudo nsenter -t $P -n sysctl net.ipv4.tcp_wmem net.ipv4.tcp_rmem net.ipv4.tcp_slow_start_after_idle
```

**Misura A/B.** Server temporaneo identico a quello di produzione (stessa immagine e config),
cambia solo il blocco `sysctls`, 3 ripetizioni alternate, `bore local --carriers 1`:

| Ripetizione | Download P1 base | Download P1 **tunato** | Download P4 base | Download P4 **tunato** | Upload P1 base | Upload P1 tunato |
|---|---|---|---|---|---|---|
| 1 | 733 | **931** | 714 | **937** | 740 | 705 |
| 2 | 527 | **930** | 526 | **938** | 523 | 683 |
| 3 | 687 | **938** | 670 | **937** | 658 | 652 |

- Il download passa da 527–733 a **930–938 Mbit/s**: è il line rate, ottenuto con **un solo
  carrier**. Le prove senza tuning variano anche molto tra loro (527 contro 733), perché una
  finestra piccola rende il flusso sensibile a ogni fluttuazione dell'RTT.
- L'upload non cambia: lì invia la workstation, quindi è il client a dover essere tunato (§2.3).

**Conferma sul server di produzione** dopo l'applicazione, `--carriers 1`, porta 30049:

| | rep 1 | rep 2 |
|---|---|---|
| Download P1, client non tunato | **934** | **937** |
| Upload P1, client non tunato | 616 | 733 |
| Upload P1, client **tunato** | **748** | **748** |
| `--carriers 4`, download P4 | 938 | 938 |
| `--carriers 4`, upload P4 | 744 | 747 |

Prima del tuning lo stesso tunnel di produzione con `--carriers 1` faceva circa 610–720 Mbit/s
in entrambe le direzioni.

### 2.3 Lato client — DA FARE su ogni macchina client
Su Linux, gli stessi valori e in più i tetti globali, che servono a QUIC:

```shell
sudo tee /etc/sysctl.d/99-bore-client.conf <<'EOF'
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.tcp_rmem = 4096 131072 16777216
net.ipv4.tcp_slow_start_after_idle = 0
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
EOF
sudo sysctl --system
```

- Le prime tre righe sono per il relay TCP. Le ultime due sono il tetto dei socket UDP:
  `bore local/vhost --udp` chiede 16 MiB e il kernel lo riduce in silenzio a `rmem_max`. Sulla
  workstation era 4 MiB e il client lo segnalava con il warning "clamped".
- **Client in Docker:**
  - Con `--network host` valgono le sysctl dell'host, quindi va usato il file sopra.
  - Con rete bridge si può fare per singolo container:
    `docker run --sysctl net.ipv4.tcp_wmem='4096 65536 16777216' --sysctl net.ipv4.tcp_rmem='4096 131072 16777216' --sysctl net.ipv4.tcp_slow_start_after_idle=0 ...`.
    Questo è il modo in cui è stata fatta la misura qui sotto, senza toccare l'host.
  - `net.core.*` non si può impostare dal container: va sempre impostato sull'host.

**Misura A/B lato client.** Server già tunato, client bore in container bridge, `--carriers 1`,
3 ripetizioni alternate:

| Ripetizione | Upload P1 default | Upload P1 **tunato** | Download P1 default | Download P1 tunato |
|---|---|---|---|---|
| 1 | 575 | **747** | 938 | 938 |
| 2 | 623 | **716** | 815 | **938** |
| 3 | 625 | **734** | 883 | **937** |

- L'upload passa da 575–625 a **716–747 Mbit/s**. È il line rate in upload (raw P8: 756).
- Anche il download diventa stabile (815–938 → 937–938), perché il client riceve con
  `tcp_rmem` più ampio e non perde tempo a riaprire la finestra dopo le pause.

**Con entrambi i capi tunati, un solo carrier satura il link in entrambe le direzioni su questo
percorso.**

- **Windows e macOS:** i buffer TCP sono già autotunati con tetti alti (Windows) oppure si
  regolano con `sysctl kern.ipc.maxsockbuf`/`net.inet.tcp.*buf_max` (macOS). Qui non sono stati
  misurati.
- Se il client non si può tunare (macchina altrui, niente root), la leva è **`--carriers`**
  (§4): più connessioni TCP in parallelo sommano le loro finestre.

---

## 3. Bug trovato e corretto: P-15, gli slot QUIC direct non venivano mai restituiti

### 3.1 Sintomo in produzione
`/admin/api/v1/metrics` mostrava `udp_direct_slots_available: 0` su 8 e
`direct_budget_refusals: 10 664`, eppure i membri dei pool direct ancora vivi erano solo 2
(`jpl-srv` 1, `sfl` 1).

Dai log:
- **10 856 rifiuti in 8 giorni**, circa 500 all'ora, tutti per `key=dufs-pcloud`.
- Il primo rifiuto è arrivato **0.5 s dopo la prima ri-registrazione** di quel vhost
  (2026-10-02 09:38:23 → 09:38:24).
- Da quel momento **ogni** nuovo carrier QUIC di **ogni** tunnel `--udp` su questo server veniva
  rifiutato e girava in silenzio sul relay TCP.

### 3.2 Causa
Con `--udp-memory-budget`, ogni connessione QUIC direct ammessa (`admit_direct`) tiene un permesso
del semaforo fino a quando la connessione si chiude.

- Solo la de-registrazione dei jump host SSH chiudeva il pool direct (`close_all`). Quelle dei
  tunnel pubblici (`PublicDeregister`) e dei vhost (`Deregister`) no.
- Le connessioni QUIC non scadevano per inattività: i task direct del client sopravvivono alla
  sessione di controllo, ed **entrambi** i capi continuano a mandarsi keep-alive.
- Il risultato è che ogni ri-registrazione di un tunnel `--udp` lasciava aperte le sue vecchie
  connessioni QUIC, con i loro permessi, **per sempre**.

C'era un secondo buco dello stesso tipo. Nei tre rami "connessione QUIC arrivata dopo la
de-registrazione" (jump, pubblico, vhost) la connessione veniva solo lasciata cadere (drop). Il
task che sorveglia l'ammissione però ne tiene un clone, quindi la connessione e il suo permesso
restavano vivi.

### 3.3 Correzione (working tree, non ancora committata)
- `src/vhost.rs`, `Deregister`: tiene il **proprio** `Arc<VhostEntry>`. Nel `Drop` chiama
  `entry.direct.close_all()` e rimuove la chiave dal registro **solo se** punta ancora alla
  propria entry (`Arc::ptr_eq`). È la regola di P-14: non toccare mai l'entry che la chiave
  risolve *adesso*, che può essere quella di una registrazione più recente.
- `src/server.rs`, `PublicDeregister`: stessa correzione, con `Arc<PublicDirectEntry>`.
- `src/server.rs`, i tre rami "arrived after deregistered": ora chiamano `direct.close()`.
- Test di regressione: `an_ended_tunnel_returns_its_direct_budget_slot`
  (`tests/public_udp_test.rs`) e `vhost_udp_an_ended_provider_returns_its_direct_budget_slot`
  (`tests/vhost_test.rs`).
  - Il server ha **un solo** slot. Il primo tunnel lo prende e poi termina; il secondo deve
    ottenere un carrier direct.
  - Con la correzione passano in 0.11–0.12 s.
  - **Red-check:** togliendo la correzione falliscono entrambi dopo 20 s con "4 refusals".
- Gate superati: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-features` (38 suite, **1510 test passati, 0 falliti**).
- Documentazione aggiornata: invariante P-15 in `CLAUDE.md` (la nota "RELATED AND STILL OPEN"
  sotto P-14 ora punta qui) e paragrafo sugli slot in `README.md`.

### 3.4 Stato in produzione
- Il container `bore-server` è stato **ricreato** (2026-10-10 01:13 UTC) per applicare le
  sysctl, e questo ha anche azzerato i permessi persi: subito dopo `direct_budget_refusals = 0`.
- **Il leak si ripresenterà** finché in produzione gira `sha-da621a1`, a ogni ri-registrazione
  di un tunnel `--udp`, e `dufs-pcloud` si ri-registra spesso.
- Rimedio definitivo: commit della correzione → build CI → aggiornare il tag immagine nel
  compose.
- Fino ad allora, se `udp_direct_slots_available` torna a 0 con pochi carrier direct vivi,
  `docker compose up -d --force-recreate` lo azzera.

---

## 4. Come regolare `--carriers` per avere le prestazioni migliori

### 4.1 Cosa fa davvero un carrier
- `--carriers N` apre **N connessioni TCP parallele** verso il server.
- **Ogni connessione proxata** (ogni connessione TCP che arriva al tunnel) viene assegnata a
  **un solo** carrier, in round-robin, e ci resta per tutta la sua vita.

Ne seguono due conseguenze:
1. **Un singolo trasferimento non va mai più veloce di un carrier.** Un solo download grosso
   (un `scp`, un `iperf -P1`, un file da un browser) è limitato dalla finestra TCP di quel
   carrier. Per lui conta il tuning del §2, non `--carriers`.
2. **Più connessioni concorrenti si sommano su più carrier.** Le finestre si sommano, e una
   connessione lenta o con perdite non blocca le altre (niente head-of-line blocking tra
   carrier).

Misure (server di produzione **prima** del tuning, relay TCP, Mbit/s):

| Configurazione | P1 down | P1 up | P4 down | P4 up |
|---|---|---|---|---|
| `--carriers 1` | 619–723 | 620–683 | 613–699 | 611–697 |
| `--carriers 4` | 636–653 | 665–727 | **934–937** | **739–755** |
| raw (senza bore) | 608–739 | 595–597 | 940–941 | 692–756 |

- Con `--carriers 4` e 4 flussi il relay arriva al 99–100% del link grezzo, quindi l'overhead di
  bore è trascurabile.
- Con 1 flusso, 4 carrier non aiutano: il flusso usa comunque un solo carrier.

### 4.2 Regole pratiche

| Situazione | Raccomandazione |
|---|---|
| Client **e** server tunati (§2), link ≤ 1 Gbit, RTT ≤ ~50 ms | `--carriers 1` (default) basta: un carrier satura il link. Usa `--carriers 2` se servono molte connessioni concorrenti, per isolarle tra loro. |
| Client **non** tunabile (niente root, macchina altrui, Windows con policy) | `--carriers 4`: restituisce il line rate sul traffico parallelo (misurato: 934/747). |
| RTT alto (≥ 80–100 ms) o link > 1 Gbit | Prima il tuning (§2). Poi stima `carriers ≈ banda_desiderata × RTT / (0.7 × tcp_wmem_max)` e arrotonda per eccesso. Esempio: 1 Gbit a 120 ms con 16 MiB → 125 MB/s × 0.12 s / 11.7 MiB ≈ 1.3 → `--carriers 2`. |
| `bore vhost` (sito web, molte richieste piccole + qualche download) | `--carriers 0` = **auto**: il server aggiunge carrier (fino a min(4, `--max-carriers`)) solo quando un trasferimento bulk occupa tutti quelli esistenti e arriva un'altra richiesta. Se non vuoi pensarci, è la scelta giusta. |
| `bore transfer` | Lascia il default (`0` = auto, segue `--parallel`). |
| `--udp` (QUIC direct) | `--carriers 1`, al massimo 2. Vedi §5: su questo server ogni carrier QUIC occupa **uno degli 8 slot globali** e 4 carrier QUIC non vanno più veloci di 1. |

Il server limita i carrier per tunnel con `BORE_MAX_CARRIERS=8`: un client che ne chiede di più
riceve 8.

### 4.3 Cose che NON aiutano
- Alzare `--carriers` per un singolo file grande: il file viaggia su un solo carrier.
- `--carriers` alto insieme a `--udp`: consuma gli slot direct di tutti (§5.3).
- Caricare via HTTPS su un vhost con scritture molto piccole (per esempio `cat | curl -T -`
  tramite busybox). Il server paga per ogni record TLS: misurati ~33 CPU-s/GiB e ~240 Mbit/s
  (un core saturo) contro ~4.5 CPU-s per 512 MiB (circa 620–670 Mbit/s) con `curl -T file` o
  `dd bs=1M | curl -T -`. Carica un **file** o usa blocchi grandi. È un limite del pattern di
  scrittura del client, non della banda.

---

## 5. QUIC direct (`--udp`): prestazioni, costo e quanti slot regge il server

### 5.1 QUIC contro relay
Server temporaneo con la stessa config di produzione (`--max-carriers 8 --udp-memory-budget
512MiB`), `bore local` dalla workstation. Valori in Mbit/s; tra parentesi i secondi di CPU del
server per ~14 s di test.

| | P1 down | P1 up | P4 down | P4 up |
|---|---|---|---|---|
| relay `--carriers 1` | 612–637 (8–9) | 579 / 344* (8 / 4) | 609–639 (8) | 584 / 187* (10 / 4) |
| **udp `--carriers 1`** | **652–676** (10) | **635–651** (14) | **659–667** (11–12) | **671–689** (14) |
| relay `--carriers 4` | 678–682 (8–11) | 638–730 (5–8) | **934–938** (4–5) | **748–749** (10) |
| udp `--carriers 4` | 654–691 (10) | 640–649 (14) | 752–759 (12) | 581–599 (17) |

\* Le misure relay-c1 upload della ripetizione 2 sono outlier. Il client era quello non tunato
e la finestra da 4 MiB rende quel caso molto sensibile alle fluttuazioni (§2.2).

Queste misure sono state fatte prima del tuning TCP. Con le sysctl del §2 il relay a 1 carrier
fa 930–938 down e 716–748 up, **sopra** QUIC.

Conclusioni:
- **Su un link pulito e cablato, QUIC direct non batte il relay tunato.** Con un solo flusso
  QUIC fa 650–690 Mbit/s, contro i 930/748 del relay tunato.
- **Costo CPU circa 3 volte quello del relay.**
  - Relay c4 download: ~4–5 CPU-s per ~1.4 GB, cioè ~3.3 CPU-s/GB.
  - QUIC download: ~10–12 CPU-s per ~1.1 GB, cioè ~10 CPU-s/GB.
  - QUIC upload (il server riceve QUIC): ~14 CPU-s per ~1 GB, cioè **un core intero su 2**. È
    questo il tetto di ~650–700 Mbit/s.
- **4 carrier QUIC non servono:** P4 down migliora di poco (752–759 contro 659–667), P4 up
  **peggiora** (581–599 contro 671–689), con più CPU e 4 slot occupati invece di 1.
- **Quando usare `--udp`:** su link con perdite o variabili (Wi-Fi, 4G/5G, hotspot, reti
  congestionate), dove QUIC evita che una perdita blocchi tutte le connessioni del carrier
  TCP (head-of-line), e dove il relay TCP soffre di più. Su fibra o cavo il relay tunato è più
  veloce e costa meno CPU alla VM.

### 5.2 Quanta memoria costa uno slot
Con `BORE_UDP_MEMORY_BUDGET=512MiB` e `BORE_MAX_CARRIERS=8` il server deriva:
- finestra di connessione = 512/8 = **64 MiB**;
- finestra per stream = 64/16 = **4 MiB**;
- **slot = 8**.

La finestra di connessione è il massimo che il server può dover bufferizzare per un carrier
QUIC quando il lettore pubblico è lento.

Misura con K lettori lenti (50 KB/s ciascuno) che scaricano 1 GiB attraverso un tunnel `--udp`.
Il server riceve a piena velocità dal client via QUIC ma può consegnare solo a 50 KB/s, quindi
accumula:

| Lettori lenti | Carrier QUIC | Δ RSS del server | Dopo il rilascio |
|---|---|---|---|
| 8 | 1 | +52 MiB | torna alla base |
| 16 | 1 | +87 MiB | torna alla base |
| 32 | 1 | +85 MiB (saturato: tetto della finestra da 64 MiB) | torna alla base |
| 32 | 2 | +160 MiB | torna alla base |
| 64 | 4 | +326 MiB | torna alla base |

- **Un carrier saturo costa circa 82 MiB di RSS** (finestra da 64 MiB più l'overhead
  dell'allocatore). Il tetto funziona: oltre 16 lettori su 1 carrier la memoria non cresce più.
  Alla fine tutto viene restituito.
- **8 slot × 82 MiB ≈ 650 MiB**, cioè **tutta la RAM disponibile** della VM (~670 MB liberi con
  seafile e 410 MB già in swap). Il caso peggiore richiede tutti gli slot saturi nello stesso
  momento, quindi è raro, ma è proprio il caso che il budget deve coprire.

### 5.3 Si possono aumentare gli slot? Misurato: non a costo zero
Per avere più slot senza più RAM bisogna dividere lo stesso budget in finestre più piccole.
Singolo stream QUIC, `--carriers 1`, 3 ripetizioni alternate:

| `BORE_MAX_CARRIERS` (budget 512MiB) | Finestra conn / stream | Slot | P1 down | P1 up | P4 down |
|---|---|---|---|---|---|
| **8 (attuale)** | 64 MiB / 4 MiB | 8 | **621–698** | **560–670** | 648–781 |
| 12 | 42.7 MiB / 2.67 MiB | 12 | 516–548 | 490–534 | 708–761 |
| 16 | 32 MiB / 2 MiB | 16 | 435–485 | 411–456 | 653–725 |

- 16 slot → il singolo stream perde circa il **30%**. È esattamente `2 MiB / 35 ms ≈ 480 Mbit/s`:
  il limite è la finestra.
- 12 slot → circa il **−20%**.
- L'aggregato con più stream (P4) non cambia, perché ogni stream ha la sua finestra.
- Più lontano è il client (RTT più alto), più pesa la differenza.

**Decisione: restano 8 slot da 64/4 MiB.** Il server "regge" 8 slot da 64 MiB; più slot a pari
finestra richiedono RAM che la VM non ha.

**Domanda reale attuale.** Dopo il riavvio i tunnel `--udp` di produzione occupano già **tutti
gli 8 slot**: `dufs-pcloud` 4, `clinic88` 2, `jpl-srv` 1, `sfl` 1. La leva più efficace non è
lato server:
- portare **`dufs-pcloud` a `--carriers 1`** (o 2) libera 2–3 slot senza perdere banda, perché
  come visto al §5.1 4 carrier QUIC non sono più veloci di 1;
- valutare lo stesso per `clinic88`.

Per avere davvero più slot da 64 MiB serve più RAM. Esempio: con 4 GB,
`BORE_UDP_MEMORY_BUDGET=1GiB` + `BORE_MAX_CARRIERS=16` dà 16 slot da 64/4 MiB (~1.3 GB nel caso
peggiore).

Il warning all'avvio *"--udp-memory-budget divided by --max-carriers is below the tested default
window…"* è atteso: con 4 MiB per stream il singolo stream QUIC è limitato a ~650–700 Mbit/s a
35 ms, che coincide con il limite di CPU della VM. Quindi qui non costa nulla.

---

## 6. Vhost (HTTP/HTTPS) — misure

Relay TCP, misurato **prima** del tuning delle sysctl. `curl` sulla VM verso `bore-server`, in
risposta da nginx sulla workstation (quindi i dati vanno dalla workstation al server):

| | N=1 | N=4 connessioni |
|---|---|---|
| `--carriers 1` | 614–661 Mbit/s (~7.5 CPU-s/GiB) | 636 Mbit/s |
| `--carriers 4` | 573–614 Mbit/s | 731 Mbit/s |

- Il percorso dati del vhost usa gli stessi carrier TCP del tunnel pubblico, quindi beneficia
  allo stesso modo del tuning: lato server per il traffico verso il client, lato client per il
  traffico verso il server. Qui il vhost non è stato ri-misurato dopo il tuning.
- Alcune misure vhost (c4 N=4 rep1: 247; c1 rep2: ~135) sono **invalide**: sulla workstation
  girava una compilazione `cargo` concorrente e sono state scartate.
- Per l'upload HTTPS vedi la nota sul pattern di scrittura al §4.3.

## 7. Raccomandazioni per il server su questa VM IONOS

**Applicate:**
1. **Sysctl TCP del container** (§2.2): `tcp_wmem`/`tcp_rmem` max 16 MiB,
   `tcp_slow_start_after_idle=0`. È la vittoria principale.
2. **Commenti del compose corretti:**
   - `BORE_MAX_CARRIERS` descriveva ancora il vecchio valore 1024; ora c'è la formula
     budget/slot con le misure.
   - `BORE_UDP_MAX_STREAMS` era descritto come "doubled"/8192, ma vale 4096 come il default.

**Da fare / consigliate:**
3. **Deploy della correzione P-15** (§3): commit → immagine CI → nuovo tag nel compose. Fino ad
   allora, ricreare il container se `udp_direct_slots_available` torna a 0.
4. **Tenere `BORE_UDP_MEMORY_BUDGET=512MiB` e `BORE_MAX_CARRIERS=8`** (§5.3). Alzarli solo con
   più RAM: la regola è *slot × ~82 MiB ≤ RAM libera*.
5. **RAM:** la VM è al limite (seafile ~690 MB, ~410 MB in swap). Se il QUIC direct deve servire
   più tunnel, la prima cosa da aggiungere è RAM, non CPU.
6. **CPU:** con 2 vCPU un solo flusso QUIC in upload consuma un core. Molti tunnel `--udp`
   attivi insieme saturano la VM prima del link. Preferire il relay TCP tunato su client cablati.
7. **Host `net.core.rmem_max`/`wmem_max` = 16 MiB:** già a posto e persistito in
   `/etc/sysctl.d/99-bore-udp.conf`. Verificato anche da `ss -uapm` (`rb`/`tb` 32 MiB sul
   socket QUIC).
8. **BBR non serve qui:** con `cubic` e le finestre giuste si arriva al line rate. Il modulo
   `tcp_bbr` è presente su disco ma non caricato. Non è stato misurato, quindi non è stato
   cambiato.
9. **Igiene:**
   - `BORE_FAST_LINK_TRANSFER_AUTH` contiene una credenziale in chiaro nel compose, come
     `BORE_SECRET`/`BORE_ADMIN_TOKEN`. Valutare un `env_file:` con permessi 600, fuori dalle
     copie e dai backup del compose.
   - `BORE_WEBSERVER_LOG=/logs` è globale, quindi ogni tunnel scrive access log su disco. Va bene,
     ma tienilo presente per lo spazio (40 GB).
10. **Firewall IONOS:** richiudere **5201/tcp, 5201/udp, 5202/tcp, 5202/udp**, aperte solo per
    questi test.

## 8. Valori cambiati e stato finale della VM

| Elemento | Prima | Dopo |
|---|---|---|
| `compose.yml` → `sysctls:` | assente | `tcp_wmem`/`tcp_rmem` `4096 … 16777216`, `tcp_slow_start_after_idle 0` |
| Commenti `BORE_MAX_CARRIERS`, `BORE_UDP_MAX_STREAMS` | errati (1024, "doubled"/8192) | corretti |
| Variabili d'ambiente bore | — | **nessuna cambiata** (budget 512MiB, max carriers 8, max streams 4096 invariati) |
| Container `bore-server` | in esecuzione da 7 giorni | ricreato 2026-10-10 01:13 UTC, stessa immagine `sha-da621a1` |
| Backup | — | `~/docker/bore/app/compose.yml.bak-20261010` |

Pulizia verificata rispetto allo snapshot iniziale:
- Immagini, volumi, reti Docker, `iptables-save` e `nft list ruleset`: **identici**, a parte
  l'ID del container ricreato.
- `/tmp` e la home: invariati.
- Rimossi i container di test (`bwt-srv`, `bwt-slow`), le immagini `networkstatic/iperf3` e
  `curlimages/curl`, e i log di test `logs/30049.log` e `logs/bwt1/`.

Sulla workstation sono stati rimossi i container di test. Le immagini `networkstatic/iperf3`,
`nginx:alpine` e `curlimages/curl` restano in cache locale.

## 9. Cosa NON è stato misurato
- SSH gateway, fast link, `bore transfer`, secret tunnel e VPN. Valgono le stesse regole TCP: il
  relay usa gli stessi socket e le stesse sysctl. Il secret direct è peer-to-peer e la VM non è
  sul percorso.
- Vhost `--udp` con HTTP reale su produzione: gli slot erano già tutti occupati dalla domanda
  reale. Il QUIC direct è stato misurato sul server temporaneo con config identica.
- Link con perdite o Wi-Fi: sono il caso in cui `--udp` dovrebbe rendere di più. Qui il link era
  cablato e pulito.

## 10. Riproduzione
I comandi di misura erano tutti in container Docker usa-e-getta: `networkstatic/iperf3`,
`curlimages/curl`, `nginx:alpine`, più un `bore server` temporaneo sulle porte 5201/5202 con
la stessa immagine e config di produzione. Schema delle prove:
- **sysctl:** stesso server temporaneo con e senza `--sysctl ...`, ripetizioni alternate, client
  con `--network host` (default) oppure bridge + `--sysctl` (tunato).
- **slot e memoria:** K lettori `curl --limit-rate 50k` sulla VM, lettura di `VmRSS` del
  processo server da `/proc/<pid>/status` prima, durante e dopo.
- **costo CPU:** delta di `utime+stime` da `/proc/<pid>/stat` del processo server intorno a ogni
  prova.
