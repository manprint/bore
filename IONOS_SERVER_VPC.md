# bore su VM IONOS (staging `prx.adiprint.it`) — banda, tuning e bug trovati

Data: 2026-10-10, aggiornato 2026-10-11. VM `ubuntu@217.154.11.34`, deploy in
`/home/ubuntu/docker/bore/app` (`compose.yml`, `config.yml`). Server in produzione: immagine
`ghcr.io/manprint/bore:v1.2.3` (il 2026-10-10 era `sha-da621a1`, 1.2.1 dev).

Due campagne, due client diversi:
- **2026-10-10 (§2, §4–§7):** workstation **cablata** a 1 Gbit/s, RTT verso la VM circa 35 ms.
- **2026-10-11 (§3, §6.4):** la stessa workstation **in Wi-Fi** (porta cablata scollegata), RTT
  35–44 ms a vuoto e 47–55 ms sotto carico. È il caso tipico dei client bore.

L'ICMP verso la VM è bloccato, quindi l'RTT è sempre stimato dalle connessioni TCP (`ss -ti`).

**Cosa devi fare su questa VM, passo per passo, con i comandi: §8** (runbook). In breve: il
tuning TCP e `bbr` sono già attivi; restano da richiudere le porte dei test e, dopo lo
spostamento di seafile, da portare `BORE_UDP_MEMORY_BUDGET` a `1GiB`. Nessun codice da
cambiare, nessuna immagine nuova.

## TL;DR

| Cosa | Prima | Dopo |
|---|---|---|
| **Download verso un client in Wi-Fi, 1 connessione** (vhost, relay) | **31.4 MB/s** (`cubic`) | **57.6 MB/s** (`bbr`, ×1.83) — §3 |
| Download verso un client in Wi-Fi, vhost `--udp` | 27.0 MB/s | **62.3 MB/s** (×2.3) — §3 |
| Download verso un client in Wi-Fi, tunnel pubblico | 32.7 MB/s | **69.2 MB/s** (×2.1) — §3 |
| Upload verso un provider in Wi-Fi (relay) | 32.1 MB/s | **81.4 MB/s** (×2.5) — §3 |
| Tunnel pubblico, `--carriers 1`, download, client cablato | 527–733 Mbit/s | **930–938 Mbit/s** (line rate) — §2 |
| Tunnel pubblico, `--carriers 1`, upload, client cablato non tunato | 575–740 Mbit/s | invariato: il limite è sul **client** — §2.3 |
| Tunnel pubblico, `--carriers 1`, upload, client cablato tunato | — | **716–748 Mbit/s** (line rate in upload) — §2.3 |
| Slot QUIC direct liberi | **0 su 8** (10 664 rifiuti) | bug P-15 corretto e in produzione con v1.2.3 — §4 |
| QUIC: 1 stream da un provider in Wi-Fi verso il server | 51.2 MB/s (finestra stream 4 MiB) | **67.8 MB/s** con 8 MiB (+32%), dopo lo spostamento di seafile — §6.4 |

- **Il "tetto dei 30 MB/s" in download non è di bore.** È `cubic`, il controllo di congestione
  TCP di default del kernel, sul socket con cui il **server** invia a chi scarica, quando l'ultimo
  miglio di chi scarica perde pacchetti (Wi-Fi, 4G). Vale per relay e `--udp` allo stesso modo,
  perché chi scarica parla sempre TCP con il server. **Rimedio: `bbr` nel namespace del server,
  applicato in produzione il 2026-10-11** (§3).
- **Lato server, sul cavo:** tre `sysctls:` TCP nel compose (finestre da 16 MiB), già applicati
  in produzione. Una sola connessione TCP satura il link in download (§2).
- **Lato client:** gli stessi tre valori sulla macchina client, più `net.core.rmem_max`/`wmem_max`
  per QUIC. Così una sola connessione satura anche l'upload (§2.3). `bbr` sul client serve solo
  a un provider che **invia** su un uplink con perdite, e anche lì il guadagno misurato è
  piccolo (§3.8).
- **Bug P-15, corretto e rilasciato in v1.2.3.** Il server perdeva per sempre gli slot QUIC
  direct a ogni ri-registrazione di un tunnel `--udp` (§4).
- **QUIC su Wi-Fi: conta la finestra per stream del server.** Con il budget attuale
  (512 MiB / 8 carrier) ogni stream ha 4 MiB, e su Wi-Fi costa un quarto della banda di un
  provider. Con seafile spostato su un'altra VM: `BORE_UDP_MEMORY_BUDGET=1GiB` (§6.4, runbook §8.4).
  Più slot **e** finestre più grandi insieme richiedono più RAM.
- **QUIC direct contro relay TCP:** sul cavo il relay tunato è più veloce e costa un terzo
  della CPU (§6.1). Su Wi-Fi i due si equivalgono nel verso provider → server, mentre nel
  verso server → client in Wi-Fi il relay TCP con `bbr` è più veloce (§6.4).

---

## 1. Ambiente misurato

### VM IONOS
- 2 vCPU, 1855 MiB di RAM, swap 2 GB (`/swapfile`). Accanto a bore gira `seafile-13-dind`,
  che usa circa 645–690 MB. **Da spostare su un'altra VM** (§8.3): dopo, sulla VM resta solo
  bore, e la RAM liberata va al QUIC direct (§8.4). `minivm-sish` è fermo; `tailscaled` gira
  sull'host. `bore-server` a riposo usa 6.5 MiB.
- Kernel 6.8, Docker 29.8.2. Container `bore-server` su rete bridge (`172.20.0.2`), `user: 0:0`.
- Host:
  - `net.core.rmem_max` / `wmem_max` = 16 MiB (corretto per QUIC: `ss` mostra `rb`/`tb` 32 MiB
    sul socket 443/udp di bore).
  - Congestion control dell'host: `cubic`, ed è rimasto `cubic`. Dal 2026-10-11 il modulo
    `tcp_bbr` è caricato e persistito (`/etc/modules-load.d/bbr.conf`), e **solo il namespace di
    `bore-server`** usa `bbr` (§3.6).
  - qdisc `fq_codel`.
- Network namespace del container **prima** del tuning: `tcp_wmem` max 4 MiB, `tcp_rmem` max 6 MiB,
  `tcp_slow_start_after_idle=1`.
- Firewall cloud IONOS: aperte solo le porte di bore. Per i test sono state aperte anche
  **5201 e 5202 TCP+UDP**: **vanno richiuse** (§8.2).

### Workstation (client)
- `net.ipv4.tcp_wmem` max **4 MiB** (default del kernel). `tcp_rmem` max 32 MiB.
- `net.core.rmem_max` / `wmem_max` = **4 MiB**. Il client bore lo segnala con un warning di
  "UDP buffer clamped" (il kernel concede il doppio: 8 MiB effettivi).
- `cubic`, `tcp_slow_start_after_idle=1`.
- Wi-Fi (campagna del 2026-10-11): 5 GHz 802.11ax, 80 MHz, PHY rx 1200 Mbit/s, segnale −52 dBm.

### Banda grezza (iperf3 senza bore, workstation cablata ↔ VM)

| | Download (VM → ws) | Upload (ws → VM) |
|---|---|---|
| 1 flusso TCP (P1) | 608–753 Mbit/s | 540–643 Mbit/s |
| 4–8 flussi TCP (P4/P8) | **940–941 Mbit/s** | **692–756 Mbit/s** |
| UDP 900 Mbit/s offerti | la VM ne emette al massimo ~603 (CPU) | ~756 consegnati, 15% di perdita |

Il link è quindi circa **940 down / 750 up**, e questi sono i riferimenti "100%" del cavo. A un
solo flusso TCP non si arriva al link: il limite è la finestra (window / RTT), non la rete. Questo
è esattamente il punto su cui interviene il tuning del §2. Per il Wi-Fi i riferimenti sono al §3.3.

---

## 2. La vittoria sul cavo: finestre TCP a entrambi i capi

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
      net.ipv4.tcp_congestion_control: "bbr"   # aggiunto il 2026-10-11, vedi §3
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
sudo nsenter -t $P -n sysctl net.ipv4.tcp_wmem net.ipv4.tcp_rmem net.ipv4.tcp_slow_start_after_idle net.ipv4.tcp_congestion_control
```

**Misura A/B.** Server temporaneo identico a quello di produzione (stessa immagine e config),
cambia solo il blocco `sysctls` (senza `bbr`), 3 ripetizioni alternate, `bore local --carriers 1`:

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
  (§5): più connessioni TCP in parallelo sommano le loro finestre.

---

## 3. La vittoria sul Wi-Fi: BBR sul server — il "tetto dei 30 MB/s" in download

> **In una riga:** il tetto di ~30 MB/s nei download dai vhost **non è di bore**. È `cubic`, il
> controllo di congestione TCP di default del kernel, sul socket con cui il **server** invia a chi
> scarica, quando l'ultimo miglio di chi scarica perde pacchetti (Wi-Fi, 4G). Con `bbr` nel
> namespace di `bore-server` lo stesso download passa da **31.4 a 57.6 MB/s** (mediana di 7 prove
> alternate, ×1.83); con `--udp` da 27.0 a 62.3 MB/s. **Applicato in produzione il 2026-10-11.**
> Il client che scarica non deve cambiare nulla.

### 3.1 Sintomo
- Segnalazione: i download dai vhost si fermano a ~30 MB/s. L'upload invece va bene. Il tetto è
  lo stesso con il relay TCP e con `--udp`, e lo stesso su un altro server bore (AWS).
- Caso misurato: vhost `ass-dufs`, con il provider (`bore vhost --carriers 0`, immagine
  `client-v1.2.3` in Docker) su una VM AWS eu-south-1 e l'origin dufs. Il file è
  `kpop_demon_hunters.mp4` (863 352 961 byte), scaricato in HTTPS con l'iniezione degli header
  attiva (`default_response_headers` in `config.yml`).

### 3.2 Ambiente della misura (2026-10-11, notte UTC)
- **Server:** questa VM, `bore-server` v1.2.3, con le sysctl del §2 già applicate e `cubic`.
- **Chi scarica:** la workstation **in Wi-Fi** (§1). RTT verso la VM: 35–44 ms.
- **Provider di `ass-dufs`:** VM AWS, RTT verso questa VM ~14 ms (letto da `ss` sul server).
  **Sulla VM AWS non è stato eseguito nulla.**
- **Provider di prova** per le altre direzioni (§3.7): `bore local`/`bore vhost` in container
  temporanei, sulla workstation in Wi-Fi oppure sulla VM stessa, con un origin e un sink Python
  che contano i byte. Tutti rimossi a fine prova.
- **Baseline grezza:** `iperf3` in container temporanei su questa VM (porte 5201/5202), con le
  stesse sysctl di `bore-server`. Rimossi a fine prova, insieme all'immagine.

### 3.3 Dove sta il limite: misura tratta per tratta

| Tratta | Strumento | Risultato |
|---|---|---|
| Provider AWS → server, con relay + TLS + iniezione header. Scaricato **sul server stesso** (`curl --resolve …:443:127.0.0.1`) | file intero | **228.6 MB/s** (863 MB in 3.78 s) |
| Server → workstation, **attraverso bore** | `curl` del file, 15 s | **30.96 MB/s** (248 Mbit/s) |
| Server → workstation, **senza bore**, 1 flusso | `iperf3 -R -P1` | 246, 289, 175 Mbit/s (22–36 MB/s) |
| Server → workstation, senza bore, 8 flussi | `iperf3 -R -P8` | 367 Mbit/s |
| Workstation → server, senza bore, 1 flusso | `iperf3 -P1` | 596 Mbit/s |

Come leggere la tabella:
- La tratta del provider e tutto il lavoro di bore sul server (relay, TLS, header) reggono
  **228 MB/s**, cioè 7 volte il tetto. Il collo non è lì.
- Verso la workstation bore fa **esattamente** quello che fa un singolo flusso TCP grezzo sullo
  stesso percorso: 248 Mbit/s contro 246. bore è già al line rate *di un flusso*.
- Il tetto è il singolo flusso TCP che va dal server a chi scarica.

### 3.4 Perché: perdite + `cubic`
`ss -tin` sul socket di invio del server durante un `iperf3 -R -P1` da 10 s:

| Campo | Valore | Significato |
|---|---|---|
| `retrans` | **2720** segmenti | il percorso perde pacchetti |
| `cwnd` / `ssthresh` | 771 / 636 pacchetti (~1.1 MB) | finestra di congestione ridotta dalle perdite |
| `rtt` | 44 ms | |
| `snd_wnd` | 10 MiB | chi riceve offre molta più finestra di quella usata |
| `rwnd_limited` | 1.8 % del tempo | il limite **non** è la finestra di ricezione |

- `cubic` legge ogni perdita come congestione e riduce la finestra. Con perdite continue e 40 ms
  di RTT, un flusso solo resta intorno a ~250 Mbit/s, anche se il link ne porterebbe di più.
- Le finestre del §2 qui non c'entrano, perché sono già abbastanza grandi (`snd_wnd` 10 MiB,
  `rwnd_limited` 1.8 %). Il §2 toglie il limite *di finestra* su un link pulito. Questo è un limite
  *da perdite*, su un link sporco.
- **Perché l'upload va bene:** in upload invia la workstation, e in quel verso il percorso perde
  molto meno. Un flusso fa 596–668 Mbit/s, spesso con 0 ritrasmissioni.
- **Perché `--udp` non cambia nulla:** `--udp` sostituisce solo la tratta provider → server. Chi
  scarica parla sempre TCP con il server, e il socket che gli invia i dati è un socket TCP del
  server, con `cubic`.
- **Perché succede anche sul server AWS:** anche lì il default è `cubic`, e le perdite stanno
  vicino a chi scarica, non vicino al server. Questo non è stato misurato direttamente: è il
  comportamento coerente con le misure sopra.
- **Dove si perdono esattamente i pacchetti** (Wi-Fi, rete domestica, operatore) non è stato
  localizzato. Per la correzione non serve: conta solo come reagisce chi invia.

### 3.5 La prova: stesso percorso, `cubic` contro `bbr`
`bbr` stima banda e RTT invece di leggere ogni perdita come congestione. Una perdita isolata
quindi non dimezza la finestra.

**Grezzo, `iperf3 -R -P1`, 10 s, ripetizioni alternate.** Due server iperf3 identici: cambia solo
`net.ipv4.tcp_congestion_control`. Valori in Mbit/s:

| Ripetizione | cubic | **bbr** |
|---|---|---|
| 1 | 233 | **390** |
| 2 | 230 | **634** |
| 3 | 72.9 | **552** |

**bore reale: download del file da `ass-dufs`, 12 s, ripetizioni alternate.** Valori in MB/s.
- **Prima del deploy**, senza riavviare nulla: una route `/32` verso l'IP pubblico della
  workstation, con `congctl bbr`, dentro il namespace di `bore-server`. Vale solo per quel
  destinatario ed è stata tolta subito dopo.
- **Dopo il deploy**, al contrario: il braccio `cubic` usa una route `/32 congctl cubic`, anche
  questa tolta subito dopo.

| Ripetizione | cubic | **bbr** |
|---|---|---|
| prima del deploy, 1 | 31.4 | **44.2** |
| prima del deploy, 2 | 32.4 | **62.6** |
| prima del deploy, 3 | 29.2 | **57.6** |
| dopo il deploy, 1 | 32.0 | **66.9** |
| dopo il deploy, 2 | 32.3 | **51.2** |
| dopo il deploy, 3 | 31.0 | **69.5** |
| dopo il deploy, 4 | 26.9 | **41.0** |
| **mediana (7)** | **31.4** | **57.6** (×1.83) |

- 7 coppie su 7: `bbr` batte `cubic` in ognuna, e il peggior `bbr` (41.0) supera il miglior
  `cubic` (32.4).
- Con `bbr` **un solo** download (461 Mbit/s di mediana) va oltre **8 flussi** grezzi con `cubic`
  (367).
- La variabilità di `bbr` (41–70 MB/s) è quella del Wi-Fi. `cubic` sembra stabile solo perché è
  fermo al suo tetto.

**Stessa prova sugli altri percorsi di download** (provider di prova sulla VM, chi scarica è la
workstation in Wi-Fi, server di produzione, MB/s):

| Percorso | cubic | **bbr** | Mediane |
|---|---|---|---|
| vhost **`--udp`** (provider → server in QUIC, server → chi scarica in TCP) | 29.4 / 26.9 / 24.0 / 27.1 | **74.1 / 44.7 / 70.6 / 53.9** | 27.0 → **62.3** (×2.3) |
| tunnel pubblico (`bore local`), relay | 33.9 / 32.7 / 29.2 | **68.6 / 75.6 / 69.2** | 32.7 → **69.2** (×2.1) |

Con `--udp` il guadagno è lo stesso: l'ultima tratta, quella con le perdite, è TCP anche lì.

### 3.6 Applicato in produzione (2026-10-11)
1. **Modulo caricato all'avvio dell'host:**
   ```shell
   echo tcp_bbr | sudo tee /etc/modules-load.d/bbr.conf
   ```
   Non è strettamente necessario: quando Docker scrive la sysctl, il kernel carica `tcp_bbr` da
   solo. È stato verificato su un host con il modulo non caricato:
   `docker run --sysctl net.ipv4.tcp_congestion_control=bbr ...` parte e il modulo compare in
   `lsmod`. Il file rende la cosa esplicita e indipendente dall'ordine di avvio.
2. **Una riga in più** nel `compose.yml`, sotto `sysctls:` di `bore-server`:
   ```yaml
       sysctls:
         net.ipv4.tcp_wmem: "4096 65536 16777216"
         net.ipv4.tcp_rmem: "4096 131072 16777216"
         net.ipv4.tcp_slow_start_after_idle: "0"
         net.ipv4.tcp_congestion_control: "bbr"
   ```
   `net.ipv4.tcp_congestion_control` è **per network namespace**. Vale per tutti i socket TCP di
   `bore-server` e per nient'altro sull'host: l'host e seafile restano con `cubic`.
3. **`docker compose up -d bore-server`** (2026-10-10 23:08 UTC), con backup in
   `compose.yml.bak-20261011`. Tutti i tunnel si sono ri-registrati entro pochi secondi
   (`ass-dufs`, `sfl`, `avic`, `jpl-srv`, `dufs-pcloud`), senza errori nei log.

Verifica:

```shell
P=$(docker inspect -f '{{.State.Pid}}' bore-server)
sudo nsenter -t $P -n sysctl net.ipv4.tcp_congestion_control   # deve dire bbr
sudo nsenter -t $P -n ss -tin | grep -c bbr                    # socket già in bbr
```

**Se il kernel non ha `bbr`, la scrittura fallisce e il container NON parte:**
`failed to write sysctl net.ipv4.tcp_congestion_control = "...": ... no such file or directory`
(riprodotto con un nome di algoritmo inesistente). Il rimedio è togliere quella riga. Controllo
preventivo sull'host: `modinfo tcp_bbr`.

**Server con `network_mode: host` o binario nativo:** Docker rifiuta le sysctl `net.*` con la rete
dell'host, quindi l'impostazione va sull'host e cambia il default per tutti i programmi:

```shell
echo tcp_bbr | sudo tee /etc/modules-load.d/bbr.conf
echo 'net.ipv4.tcp_congestion_control = bbr' | sudo tee /etc/sysctl.d/99-bore-bbr.conf
sudo sysctl --system
```

### 3.7 Cosa migliora, funzione per funzione, nei due versi

**La regola:** il controllo di congestione agisce solo su chi **invia** su una tratta TCP. `bbr`
sul server migliora una tratta solo se (1) è il server a inviare, (2) è TCP e (3) quella tratta
perde pacchetti, cioè dall'altra parte c'è un client in Wi-Fi o 4G. Su un percorso cablato e
pulito `cubic` era già al line rate (§2) e `bbr` non cambia nulla.

**Matrice misurata** (server di produzione salvo dove indicato, la workstation in Wi-Fi fa da
client o da provider, 3 ripetizioni alternate, MB/s, mediane):

| Caso | Chi invia sulla tratta Wi-Fi | Trasporto | cubic | bbr | Effetto di `bbr` sul server |
|---|---|---|---|---|---|
| **A.** Download: provider in datacenter → server → **client in Wi-Fi** | server (TCP) | relay | 31.4 | **57.6** | **×1.83** |
| A. idem | server (TCP) | `--udp` | 27.0 | **62.3** | **×2.3** |
| A. idem, tunnel pubblico | server (TCP) | relay | 32.7 | **69.2** | **×2.1** |
| **B.** Upload: client in Wi-Fi → server → provider in datacenter | client | relay | 59.1 | 59.0 | nessuno |
| **C.** Upload verso un **provider in Wi-Fi**: client in datacenter → server → provider | server (TCP carrier) | relay | 32.1 | **81.4** | **×2.5** |
| C. idem | server (QUIC, quinn) | `--udp` | 55.6 | 55.7 | nessuno (non è TCP) |
| **D.** Download da un **provider in Wi-Fi**: provider → server → client in datacenter | provider | relay | 66.4 | 57.2 | nessuno (rumore) |
| D. idem | provider (QUIC) | `--udp` | 59.4 | 63.2 | nessuno (rumore) |

- Il caso C è quello dei provider domestici, per esempio `jpl-srv` e `dufs-pcloud`: chi carica
  file su un servizio esposto da casa ora va **2.5 volte** più veloce.
- Nel caso B una ripetizione `cubic` ha dato 2.9 MB/s. Non si è ripresentata e non è stata
  indagata: le altre due (59.1, 72.0) sono in linea con `bbr`.
- Nei casi D il server riceve. La tratta Wi-Fi la governa il provider (§3.8).

**Tutte le funzioni dell'app.** Le funzioni non misurate usano gli stessi socket TCP del server
dei casi A e C, quindi vale la stessa regola:

| Funzione | Tratte TCP in cui **invia il server** (migliorano verso un client in Wi-Fi/4G) | Tratte su cui `bbr` del server non agisce | Misurato |
|---|---|---|---|
| vhost, relay | server → browser (download); server → provider (upload verso il provider) | provider → server; browser → server | sì (A, C) |
| vhost `--udp` | server → browser (download) | provider ↔ server: QUIC | sì (A, C) |
| tunnel pubblico (`bore local`), relay e `--udp` | come il vhost | come il vhost | sì (A, C) |
| secret tunnel (`bore proxy`), relay | server → consumer; server → provider | consumer → server; provider → server | no: stesso carrier TCP del pubblico |
| secret tunnel `--udp` direct | nessuna | tutto: peer-to-peer in QUIC, il server non è sul percorso | — |
| VPN, relay | server → peer (il carrier TCP esterno) | peer → server; i flussi TCP interni | no |
| VPN direct | nessuna | tutto: peer-to-peer in QUIC | — |
| SSH gateway (`ssh -R`/`-L`) e jump host | server → client ssh | client ssh → server | no. Un canale OpenSSH ha comunque una finestra di 2 MiB, cioè al massimo ~50 MB/s a 40 ms: il guadagno si ferma lì |
| fast link (`curl -T` → `fast.<base>`) | server → chi scarica | chi carica → server | no: è un download HTTPS come quello del vhost |
| `bore transfer` via relay, `transfer link` | server → chi riceve | chi invia → server | no |
| `bore transfer` direct | nessuna | tutto: QUIC | — |
| web transfer, relay (WebSocket) | server → browser che riceve | browser che invia → server | no |
| web transfer, WebRTC direct | nessuna | tutto: browser ↔ browser | — |
| pagina admin, API | server → browser | — | irrilevante (traffico minimo) |

In sintesi: migliora **tutto ciò che il server spedisce in TCP verso un client con perdite**,
cioè quasi tutti i download e gli upload verso un provider domestico. Non cambia nulla per chi
invia verso il server, per i percorsi QUIC e WebRTC, e per i percorsi cablati puliti.

**Rischi considerati:**
- `bbr` (v1) non cede banda ai flussi `cubic` come farebbe un altro `cubic`, quindi su un collo
  di bottiglia condiviso può prenderne di più. Per un server che serve download è una scelta
  diffusa (Google la usa sui propri server dal 2016). Qui l'impostazione vale solo per il
  namespace di bore.
- Su link con buffer molto piccoli `bbr` v1 può ritrasmettere di più. Nelle misure sopra non ha
  mai reso meno di `cubic`.

### 3.8 Lato client: i compose vanno cambiati?
**Per il problema dei download no.** Il collo era il socket del server verso chi scarica, e si
corregge solo sul server. `bbr` agisce solo su chi **invia**: un client che **scarica** non deve
cambiare nulla.

Il client bore invia solo quando fa da provider (le risposte che i visitatori scaricano) o
quando carica:

| Tratta in cui invia il client bore | Serve `bbr` sul client? | Misura |
|---|---|---|
| Provider in datacenter → server (es. `ass-dufs` su AWS) | **No** | 228.6 MB/s con `cubic` (§3.3) |
| Provider in Wi-Fi → server, cioè i download dai servizi di casa (caso D) | **Facoltativo** | relay con `cubic` sul provider: 57–72 MB/s, già vicino al grezzo dell'uplink. Il `--udp` usa il QUIC di bore e non dipende dalle sysctl (§6.4) |
| Upload grezzo dalla workstation in Wi-Fi, 1 flusso | Guadagno piccolo | `cubic` 668/515/646, `bbr` 661/679/675 Mbit/s: +4 % di mediana. `bbr` toglie il calo (515, con 135 ritrasmissioni). Su questo uplink le perdite sono rare |

Su uplink peggiori (4G, Wi-Fi debole) il beneficio può essere maggiore: è la stessa fisica del
§3.4 con i ruoli invertiti. Se si vuole attivarlo su un provider:
- **Client in Docker con rete bridge** (come il compose di `ass-dufs`, che ha già `sysctls:`):
  aggiungere `net.ipv4.tcp_congestion_control: "bbr"` sotto `sysctls:`. Vale lo stesso failure
  mode del §3.6.
- **Client con `network_mode: host`** (come `docker/docker-compose.client.yml`) **o binario
  nativo:** con la rete dell'host Docker rifiuta le sysctl `net.*`, quindi l'impostazione va
  sull'host, con i tre comandi del §3.6. Cambia il default per **tutti** i programmi dell'host.

### 3.9 Riproduzione

```shell
# Sul server: due iperf3 identici, tranne il controllo di congestione.
docker run -d --rm --name bwt-cubic -p 5201:5201 \
  --sysctl net.ipv4.tcp_wmem="4096 65536 16777216" --sysctl net.ipv4.tcp_rmem="4096 131072 16777216" \
  networkstatic/iperf3 -s
docker run -d --rm --name bwt-bbr -p 5202:5201 --sysctl net.ipv4.tcp_congestion_control=bbr \
  --sysctl net.ipv4.tcp_wmem="4096 65536 16777216" --sysctl net.ipv4.tcp_rmem="4096 131072 16777216" \
  networkstatic/iperf3 -s

# Dal client che scarica, alternando i due bracci.
iperf3 -c <server> -p 5201 -t 10 -O 2 -R -P1   # cubic
iperf3 -c <server> -p 5202 -t 10 -O 2 -R -P1   # bbr

# A/B sul bore reale, per un solo destinatario e senza riavviare.
P=$(docker inspect -f '{{.State.Pid}}' bore-server)
sudo nsenter -t $P -n ip route replace <ip_client>/32 via <gateway_container> dev eth0 congctl cubic   # oppure bbr
sudo nsenter -t $P -n ip route del <ip_client>/32
```

- La route con `congctl` vale anche per i socket **accettati** dal server, ma solo per quelli
  creati dopo. È il modo per confrontare i due algoritmi sul servizio vero, per un solo client,
  senza toccare gli altri.
- Un carrier già aperto tiene l'algoritmo con cui è nato. Per le prove sulle tratte server →
  provider (casi C e D) il provider di prova va quindi riavviato a ogni braccio.

---

## 4. Bug trovato e corretto: P-15, gli slot QUIC direct non venivano mai restituiti

### 4.1 Sintomo in produzione
`/admin/api/v1/metrics` mostrava `udp_direct_slots_available: 0` su 8 e
`direct_budget_refusals: 10 664`, eppure i membri dei pool direct ancora vivi erano solo 2
(`jpl-srv` 1, `sfl` 1).

Dai log:
- **10 856 rifiuti in 8 giorni**, circa 500 all'ora, tutti per `key=dufs-pcloud`.
- Il primo rifiuto è arrivato **0.5 s dopo la prima ri-registrazione** di quel vhost
  (2026-10-02 09:38:23 → 09:38:24).
- Da quel momento **ogni** nuovo carrier QUIC di **ogni** tunnel `--udp` su questo server veniva
  rifiutato e girava in silenzio sul relay TCP.

### 4.2 Causa
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

### 4.3 Correzione (commit `4fb9041`, rilasciata in v1.2.3)
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
- Documentazione aggiornata: invariante P-15 in `CLAUDE.md` e paragrafo sugli slot in `README.md`.

### 4.4 Stato in produzione
- In produzione gira **v1.2.3**, che contiene la correzione. Il 2026-10-11, dopo il riavvio per
  `bbr`: `direct_budget_refusals = 0`, `udp_direct_slots_available = 7` su 8.
- Se `udp_direct_slots_available` torna a 0 con pochi carrier direct vivi, è un'altra causa da
  indagare: il leak di P-15 non c'è più.

---

## 5. Come regolare `--carriers` per avere le prestazioni migliori

### 5.1 Cosa fa davvero un carrier
- `--carriers N` apre **N connessioni TCP parallele** verso il server.
- **Ogni connessione proxata** (ogni connessione TCP che arriva al tunnel) viene assegnata a
  **un solo** carrier, in round-robin, e ci resta per tutta la sua vita.

Ne seguono due conseguenze:
1. **Un singolo trasferimento non va mai più veloce di un carrier.** Un solo download grosso
   (un `scp`, un `iperf -P1`, un file da un browser) è limitato dalla finestra TCP di quel
   carrier. Per lui conta il tuning del §2 (e su Wi-Fi il §3), non `--carriers`.
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

### 5.2 Regole pratiche

| Situazione | Raccomandazione |
|---|---|
| Client **e** server tunati (§2), link ≤ 1 Gbit, RTT ≤ ~50 ms | `--carriers 1` (default) basta: un carrier satura il link. Usa `--carriers 2` se servono molte connessioni concorrenti, per isolarle tra loro. |
| Client **non** tunabile (niente root, macchina altrui, Windows con policy) | `--carriers 4`: restituisce il line rate sul traffico parallelo (misurato: 934/747). |
| RTT alto (≥ 80–100 ms) o link > 1 Gbit | Prima il tuning (§2). Poi stima `carriers ≈ banda_desiderata × RTT / (0.7 × tcp_wmem_max)` e arrotonda per eccesso. Esempio: 1 Gbit a 120 ms con 16 MiB → 125 MB/s × 0.12 s / 11.7 MiB ≈ 1.3 → `--carriers 2`. |
| `bore vhost` (sito web, molte richieste piccole + qualche download) | `--carriers 0` = **auto**: il server aggiunge carrier (fino a min(4, `--max-carriers`)) solo quando un trasferimento bulk occupa tutti quelli esistenti e arriva un'altra richiesta. Se non vuoi pensarci, è la scelta giusta. |
| `bore transfer` | Lascia il default (`0` = auto, segue `--parallel`). |
| `--udp` (QUIC direct) | `--carriers 1`, al massimo 2. Vedi §6: su questo server ogni carrier QUIC occupa **uno degli 8 slot globali** e 4 carrier QUIC non vanno più veloci di 1. |

Il server limita i carrier per tunnel con `BORE_MAX_CARRIERS=8`: un client che ne chiede di più
riceve 8.

### 5.3 Cose che NON aiutano
- Alzare `--carriers` per un singolo file grande: il file viaggia su un solo carrier.
- `--carriers` alto insieme a `--udp`: consuma gli slot direct di tutti (§6.3).
- Caricare via HTTPS su un vhost con scritture molto piccole (per esempio `cat | curl -T -`
  tramite busybox). Il server paga per ogni record TLS: misurati ~33 CPU-s/GiB e ~240 Mbit/s
  (un core saturo) contro ~4.5 CPU-s per 512 MiB (circa 620–670 Mbit/s) con `curl -T file` o
  `dd bs=1M | curl -T -`. Carica un **file** o usa blocchi grandi. È un limite del pattern di
  scrittura del client, non della banda.

---

## 6. QUIC direct (`--udp`): prestazioni, costo e quanti slot regge il server

### 6.1 QUIC contro relay (cavo)
Server temporaneo con la stessa config di produzione (`--max-carriers 8 --udp-memory-budget
512MiB`), `bore local` dalla workstation cablata. Valori in Mbit/s; tra parentesi i secondi di
CPU del server per ~14 s di test.

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
- **Quando usare `--udp`:** `--udp` sostituisce solo la tratta **provider ↔ server**. Ha senso
  quando è il **provider** a stare su un link con perdite o variabile (Wi-Fi, 4G/5G, hotspot),
  dove QUIC evita che una perdita blocchi tutte le connessioni del carrier TCP (head-of-line).
  Non aiuta chi **scarica** in Wi-Fi, che parla sempre TCP con il server: per lui la leva è
  `bbr` sul server (§3). Su fibra o cavo il relay tunato è più veloce e costa meno CPU alla VM.
  Su Wi-Fi vedi il §6.4.

### 6.2 Quanta memoria costa uno slot
Con `BORE_UDP_MEMORY_BUDGET=512MiB` e `BORE_MAX_CARRIERS=8` il server deriva:
- finestra di connessione = 512/8 = **64 MiB**;
- finestra per stream = 64/16 = **4 MiB**;
- **slot = 8**.

In generale `slot = budget / finestra_di_connessione`, e la finestra di connessione è
`budget / BORE_MAX_CARRIERS`: quindi, finché la finestra non tocca i limiti 16–256 MiB, **gli
slot sono uguali a `BORE_MAX_CARRIERS`** e il budget decide solo quanto è grande ogni finestra.

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

### 6.3 Si possono aumentare gli slot? Misurato: non a costo zero
Per avere più slot senza più RAM bisogna dividere lo stesso budget in finestre più piccole.
Singolo stream QUIC, `--carriers 1`, client cablato, 3 ripetizioni alternate:

| `BORE_MAX_CARRIERS` (budget 512MiB) | Finestra conn / stream | Slot | P1 down | P1 up | P4 down |
|---|---|---|---|---|---|
| **8 (attuale)** | 64 MiB / 4 MiB | 8 | **621–698** | **560–670** | 648–781 |
| 12 | 42.7 MiB / 2.67 MiB | 12 | 516–548 | 490–534 | 708–761 |
| 16 | 32 MiB / 2 MiB | 16 | 435–485 | 411–456 | 653–725 |

- 16 slot → il singolo stream perde circa il **30%**. È esattamente `2 MiB / 35 ms ≈ 480 Mbit/s`:
  il limite è la finestra.
- 12 slot → circa il **−20%**.
- L'aggregato con più stream (P4) non cambia, perché ogni stream ha la sua finestra.
- Più lontano è il client (RTT più alto), più pesa la differenza. Sul Wi-Fi l'RTT sotto carico
  sale a ~50 ms e già i 4 MiB attuali costano un quarto della banda (§6.4).

**Decisione del 2026-10-10: restano 8 slot da 64/4 MiB**, con seafile sulla VM. Rivista al §6.4
per lo scenario Wi-Fi e per lo spostamento di seafile.

**Domanda reale.** Il 2026-10-10, dopo il riavvio, i tunnel `--udp` di produzione occupavano
**tutti gli 8 slot**: `dufs-pcloud` 4, `clinic88` 2, `jpl-srv` 1, `sfl` 1. Il 2026-10-11 ne era
occupato **1**. La leva più efficace per liberare slot non è lato server:
- portare **`dufs-pcloud` a `--carriers 1`** (o 2) libera 2–3 slot senza perdere banda, perché
  come visto al §6.1 4 carrier QUIC non sono più veloci di 1;
- valutare lo stesso per `clinic88`.

Il warning all'avvio *"--udp-memory-budget divided by --max-carriers is below the tested default
window…"* è atteso con la configurazione attuale. Sul cavo a 35 ms i 4 MiB per stream valgono
~650–700 Mbit/s, cioè il limite di CPU della VM, e lì non costano nulla. **Su Wi-Fi invece
costano** (§6.4).

### 6.4 Approfondimento QUIC su Wi-Fi (2026-10-11)
Lo scenario tipico: il provider (o chi riceve) è su Wi-Fi. Workstation in Wi-Fi (§1), RTT sotto
carico 47–55 ms (letto da `ss -ti` sulla connessione di controllo durante il trasferimento).

**Cosa NON limita QUIC su questo percorso** (misurato durante un trasferimento server → provider
in Wi-Fi da 10 s):
- **CPU del server:** `bore-server` ha usato 0.43 core (0.26 con il relay), l'host è rimasto
  libero per 1.55 core su 2.
- **Buffer UDP:** `RcvbufErrors`/`SndbufErrors` invariati a zero sul server e invariati sulla
  workstation, nonostante i soli 8 MiB effettivi lato workstation.
- **MTU:** il 98% dei pacchetti QUIC è ≥ 1200 byte, massimo 1452: la path MTU è già piena.
- **Controllo di congestione di QUIC** (`BORE_DIRECT_QUIC_CC`), in entrambi i versi, MB/s:

  | Chi invia | `bbr` (default) | `cubic` | `newreno` |
  |---|---|---|---|
  | provider in Wi-Fi → server (env sul provider, server di produzione) | 58.2 / 60.1 / 61.2 | 60.6 / 47.1 / 52.9 | 59.7 / 62.3 / 59.9 |
  | server → provider in Wi-Fi (env sul server, server di prova) | 61.4 / 49.2 / 65.1 | 63.8 / 63.2 / 47.3 | — |

  Nessuna differenza oltre il rumore del Wi-Fi. È la prova "su un percorso con perdite" che la
  decisione V-12 del progetto chiedeva: **`bbr` resta il default**.

**Cosa limita: la finestra per stream del server.** Nel verso provider → server è il server a
ricevere, quindi conta la **sua** finestra per stream, che con il budget attuale è 4 MiB.
- Prova rapida sul server di produzione: 1 stream 56.3 MB/s, 3 stream insieme 68.8 MB/s in
  totale. Con più stream si guadagna, quindi c'è un limite per stream.
- A/B con un `bore server` di prova sulla stessa VM (porte 5201/5202, stessa immagine e sysctl
  di produzione, produzione mai toccata), provider sulla workstation in Wi-Fi, chi scarica sulla
  VM, 6 ripetizioni alternate in due ordini diversi, MB/s:

  | Server | Finestra conn / stream | Slot | Ripetizioni | **Mediana** |
  |---|---|---|---|---|
  | `--udp-memory-budget 512MiB --max-carriers 8` (**produzione**) | 64 / **4 MiB** | 8 | 61.9, 60.6, 54.1, 47.1, 48.2, 45.1 | **51.2** |
  | `--udp-memory-budget 512MiB --max-carriers 4` | 128 / **8 MiB** | 4 | 68.0, 67.6, 70.4, 70.0, 67.0, 66.0 | **67.8** |
  | nessun budget (default) | 256 / **16 MiB** | illimitati | 62.0, 49.9, 70.8, 69.7, 69.8, 68.3 | **69.0** |

- Con 4 MiB a ~50 ms il singolo stream perde circa un **quarto** della banda (−25% rispetto a
  8 MiB). 8 MiB bastano: oltre non si guadagna, perché a quel punto il limite è il Wi-Fi.
- Per confronto, il relay TCP sullo stesso uplink fa 57–72 MB/s (caso D del §3.7): con 8 MiB il
  QUIC lo eguaglia.
- Nel verso **server → client** è il client a ricevere, con la sua finestra di default (16 MiB):
  il budget del server non c'entra.

**Aperto, non indagato oltre:** nel verso server → provider in Wi-Fi QUIC fa ~55–61 MB/s, mentre
il relay TCP con `bbr` fa ~81 MB/s (caso C del §3.7). Non è CPU, non sono buffer UDP, non è il
controllo di congestione, non è la finestra. Per chi riceve in Wi-Fi da questo server, oggi il
relay TCP è la scelta più veloce.

**Raccomandazione per la produzione** (va applicata **dopo** lo spostamento di seafile, perché
raddoppia la memoria nel caso peggiore):

```yaml
      - BORE_MAX_CARRIERS=8
      - BORE_UDP_MEMORY_BUDGET=1GiB     # era 512MiB → finestre 128/8 MiB, 8 slot
```

Con 1 GiB di budget si sceglie **una** delle due cose, non entrambe:

| Opzione | `BORE_UDP_MEMORY_BUDGET` / `BORE_MAX_CARRIERS` | Finestra conn / stream | Slot | Singolo stream da un provider in Wi-Fi | Caso peggiore di RAM (stima) |
|---|---|---|---|---|---|
| attuale | 512MiB / 8 | 64 / 4 MiB | 8 | 51 MB/s (misurato) | ~650 MiB (misurato) |
| **A — consigliata** | 1GiB / 8 | 128 / 8 MiB | 8 | **~68 MB/s** (misurato con 128/8) | ~1.3 GiB |
| B — più slot | 1GiB / 16 | 64 / 4 MiB | **16** | 51 MB/s, come oggi | ~1.3 GiB |

- La A è consigliata perché il 90% dei client è in Wi-Fi e oggi gli slot sono poco usati (1 su 8
  il 2026-10-11). Si passa alla B se `direct_budget_refusals` cresce mentre
  `udp_direct_slots_available` è a 0.
- Il caso peggiore di RAM è stimato scalando gli 82 MiB misurati per una finestra da 64 MiB (§6.2).
  Richiede tutti gli slot saturi di lettori lenti nello stesso momento. Con seafile spostato la VM
  ha circa 1.2 GiB disponibili, più 2 GB di swap. Bilancio completo della RAM, comandi e
  verifiche: §8.4–§8.6.
- **Più slot e finestre da 8 MiB insieme** (16 slot × 128 MiB = 2 GiB di budget) richiedono una
  VM da 4 GB.
- `BORE_MAX_CARRIERS` limita anche i carrier TCP per tunnel; nessun tunnel di produzione ne usa
  più di 4.

---

## 7. Vhost (HTTP/HTTPS) — misure

Relay TCP, misurato **prima** del tuning delle sysctl. `curl` sulla VM verso `bore-server`, in
risposta da nginx sulla workstation (quindi i dati vanno dalla workstation al server):

| | N=1 | N=4 connessioni |
|---|---|---|
| `--carriers 1` | 614–661 Mbit/s (~7.5 CPU-s/GiB) | 636 Mbit/s |
| `--carriers 4` | 573–614 Mbit/s | 731 Mbit/s |

- Il percorso dati del vhost usa gli stessi carrier TCP del tunnel pubblico, quindi beneficia
  allo stesso modo del tuning: lato server per il traffico verso il client, lato client per il
  traffico verso il server. I download dai vhost verso client in Wi-Fi sono misurati al §3.
- Alcune misure vhost (c4 N=4 rep1: 247; c1 rep2: ~135) sono **invalide**: sulla workstation
  girava una compilazione `cargo` concorrente e sono state scartate.
- L'iniezione degli header di risposta (`default_response_headers`) non è un collo: lo stesso
  download con gli header iniettati fa 228.6 MB/s misurato sul server (§3.3).
- Per l'upload HTTPS vedi la nota sul pattern di scrittura al §5.3.

## 8. Runbook: cosa fare su questa VM (solo bore, dopo lo spostamento di seafile)

Tutto quello che resta da fare su `217.154.11.34`, in ordine. **Non c'è codice da cambiare né
un'immagine nuova da distribuire**: è solo configurazione della VM e del `compose.yml`. I comandi
vanno lanciati sulla VM come `ubuntu`. Il lato client è ai §2.3 e §3.8.

| Passo | Quando | Sezione |
|---|---|---|
| Verificare che il tuning già applicato sia attivo | quando vuoi | §8.1 |
| Richiudere le porte 5201/5202 nel firewall IONOS | **subito** | §8.2 |
| Spostare seafile | quando la nuova VM è pronta | §8.3 |
| `BORE_UDP_MEMORY_BUDGET=1GiB` | **solo dopo** seafile | §8.4 |
| Riavvio di prova | facoltativo, subito dopo §8.4 | §8.8 |
| Pulizia | facoltativa | §8.9 |

### 8.1 Già applicato: niente da fare

| Cosa | Dove sta | Verifica |
|---|---|---|
| Finestre TCP 16 MiB e `tcp_slow_start_after_idle=0` | `compose.yml` → `sysctls:` | comando sotto |
| `bbr` nel namespace di `bore-server` | `compose.yml` → `sysctls:` | comando sotto |
| Modulo `tcp_bbr` caricato al boot | `/etc/modules-load.d/bbr.conf` | `lsmod \| grep tcp_bbr` |
| `net.core.rmem_max`/`wmem_max` = 16 MiB (per QUIC) | `/etc/sysctl.d/99-bore-udp.conf` | `sysctl net.core.rmem_max net.core.wmem_max` |
| Correzione P-15 | immagine `v1.2.3` | `docker inspect -f '{{.Config.Image}}' bore-server` |
| Limite dei file descriptor | automatico (P-12): oggi 6400, ne servono 512 + 256 | `grep 'open files' /proc/$(docker inspect -f '{{.State.Pid}}' bore-server)/limits` |

```shell
pid=$(docker inspect -f '{{.State.Pid}}' bore-server)
sudo nsenter -t "$pid" -n sysctl net.ipv4.tcp_congestion_control net.ipv4.tcp_rmem \
  net.ipv4.tcp_wmem net.ipv4.tcp_slow_start_after_idle
# atteso: bbr | 4096 131072 16777216 | 4096 65536 16777216 | 0
```

### 8.2 Subito: richiudere il firewall dei test
`ufw` è inattivo, quindi l'unico firewall è quello cloud di IONOS. Nel Cloud Panel, dalla policy
del firewall di questa VM, togliere **5201/tcp, 5201/udp, 5202/tcp, 5202/udp**: sono state
aperte solo per le prove del 10–11 ottobre. Oggi non ascolta più nessuno su quelle porte, ma non
devono restare aperte.

### 8.3 Spostamento di seafile
1. Fotografia prima:
   ```shell
   free -m; swapon --show
   docker stats --no-stream --format '{{.Name}} {{.MemUsage}}'
   ```
   Il 2026-10-11: available 664 MiB, swap usata 409 MiB, `seafile-13-dind` 645 MiB,
   `bore-server` 6.5 MiB a riposo.
2. Dopo aver migrato i dati (stanno in `~/docker/seafile-dind/data`):
   ```shell
   cd ~/docker/seafile-dind && docker compose down
   ```
   `down` rimuove il container e la sua rete, non la cartella `data`. Cancellala solo quando
   seafile sulla nuova VM è verificato.
3. Fotografia dopo: `free -m; swapon --show`. Atteso: available intorno a 1.2–1.3 GiB, e la
   swap usata in calo (le pagine di un processo che esce vengono liberate).

### 8.4 Tuning di bore dopo seafile: finestre QUIC da 8 MiB
Si cambia **una sola** variabile: il budget QUIC passa da 512 MiB a 1 GiB. `BORE_MAX_CARRIERS`
resta 8. Effetto: la finestra per stream passa da 4 a 8 MiB, cioè +32% su un singolo stream da
un provider in Wi-Fi (§6.4), con gli stessi 8 slot.

`docker compose up -d` ricrea il container: i tunnel attivi cadono e i client con
`--auto-reconnect` rientrano in pochi secondi. Meglio farlo in un momento tranquillo.

```shell
cd ~/docker/bore/app
cp compose.yml compose.yml.bak-$(date +%Y%m%d)
sed -i 's/BORE_UDP_MEMORY_BUDGET=512MiB/BORE_UDP_MEMORY_BUDGET=1GiB/' compose.yml
grep -n 'BORE_UDP_MEMORY_BUDGET=\|BORE_MAX_CARRIERS=' compose.yml   # 1GiB e 8, non commentate
docker compose up -d bore-server
docker logs bore-server 2>&1 | grep 'budget applied'
```

Riga attesa:
```
direct UDP memory budget applied budget_mib=1024 slots=8 connection_window_mib=128 stream_window_mib=8
```

Subito dopo resta il `WARN` *"--udp-memory-budget divided by --max-carriers is below the tested
default window…"*. È atteso: 8 MiB sono ancora sotto i 16 MiB di default. Su Wi-Fi bastano
(16 MiB non hanno dato di più, §6.4), e per farlo sparire servirebbero 2 GiB di budget, che
questa VM non regge (§8.5).

Verifica dalla API di amministrazione. Il token si legge dal container senza stamparlo:
```shell
T=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' bore-server | sed -n 's/^BORE_ADMIN_TOKEN=//p')
curl -s -H "Authorization: Bearer $T" http://127.0.0.1:7835/admin/api/v1/config | python3 -c '
import sys, json; d = json.load(sys.stdin)
print({k: d[k] for k in ("udp_stream_receive_window", "udp_connection_receive_window", "udp_send_window", "udp_direct_slots")})'
# atteso: 8MiB, 128MiB, 128MiB, 8   (prima: 4MiB, 64MiB, 64MiB, 8)
```

Poi ripetere la verifica di `bbr` del §8.1: la ricreazione del container deve aver riapplicato
le `sysctls:`.

### 8.5 Perché 1 GiB e non di più: il bilancio della RAM
VM da 1855 MiB di RAM più 2 GiB di swap, con solo bore:

| Voce | Quanto | Da dove viene |
|---|---|---|
| Sistema (kernel, Docker, `tailscaled`, sshd, journald…) più `bore-server` a riposo | ~550 MiB | misurato il 2026-10-11: `used` 1190 MiB meno i 645 MiB di seafile |
| QUIC direct, caso peggiore con budget 1 GiB | ~1.3 GiB | stima: 8 slot × ~164 MiB. Misurati 82 MiB di RSS per un carrier saturo con finestra da 64 MiB (§6.2), scalati a 128 MiB |
| TCP, tetto del kernel | 164 MiB | `net.ipv4.tcp_mem` max = 42018 pagine. Il kernel lo calcola dalla RAM al boot; vale per tutto l'host, non per il container |
| **Totale nel caso peggiore** | **~2.0 GiB** | circa 170 MiB oltre la RAM, che finiscono in swap |

- Il caso peggiore richiede tutti gli 8 slot saturi di lettori lenti **e** tutto il TCP al suo
  tetto nello stesso momento. È raro, e la swap lo assorbe: il server rallenta, non muore.
- Con 1.5 GiB di budget il caso peggiore salirebbe a ~2.6 GiB: troppo per questa VM.
- **La swap da 2 GB va tenuta**: è la rete di sicurezza proprio di questo caso.
- CPU: con 2 vCPU un solo flusso QUIC in upload consuma un core, quindi molti tunnel `--udp`
  attivi insieme saturano la CPU prima del link (§6.1). Seafile a riposo usava il 4%: toglierlo
  libera RAM, non CPU.

### 8.6 Si possono avere più slot?
Con questa RAM il budget massimo sensato è 1 GiB, e con 1 GiB **gli slot sono uguali a
`BORE_MAX_CARRIERS`** (§6.2). Quindi si sceglie tra finestre grandi e tanti slot:

| `BORE_MAX_CARRIERS` (budget 1GiB) | Slot | Finestra per stream | Singolo stream da un provider in Wi-Fi |
|---|---|---|---|
| **8 (consigliato)** | 8 | 8 MiB | ~68 MB/s (misurato) |
| 12 | 12 | ~5.3 MiB | non misurato, sta tra le altre due righe |
| 16 | 16 | 4 MiB | ~51 MB/s, come oggi (misurato) |

- Il caso peggiore di RAM è lo stesso in tutte e tre le righe, perché il budget è sempre 1 GiB.
- **16 slot e 8 MiB insieme** richiedono 2 GiB di budget, cioè una VM da 4 GB.
- `BORE_MAX_CARRIERS` limita anche i carrier TCP per tunnel. Nessun tunnel di produzione ne usa
  più di 4, quindi 8, 12 o 16 non cambiano nulla sul relay.
- Il modo più economico per liberare slot non costa RAM: portare `dufs-pcloud` (4 carrier QUIC)
  a `--carriers 1` o 2 sul suo client (§6.3).
- Quando passare a 12 o 16: quando `direct_budget_refusals` cresce **mentre**
  `udp_direct_slots_available` è a 0 (§8.7). Il 2026-10-11 era in uso al massimo 1 slot su 8.
  Il cambio è `sed -i 's/BORE_MAX_CARRIERS=8/BORE_MAX_CARRIERS=16/' compose.yml` più
  `docker compose up -d bore-server`, con log atteso `slots=16 connection_window_mib=64
  stream_window_mib=4`.

### 8.7 Cosa tenere d'occhio
```shell
T=$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' bore-server | sed -n 's/^BORE_ADMIN_TOKEN=//p')
curl -s -H "Authorization: Bearer $T" http://127.0.0.1:7835/admin/api/v1/metrics | python3 -c '
import sys, json; d = json.load(sys.stdin)
print({k: d.get(k) for k in ("udp_direct_slots_available", "direct_budget_refusals", "conn_rejections")})'
free -m; swapon --show
```
I contatori ripartono da zero a ogni riavvio del container.

| Segnale | Cosa significa | Cosa fare |
|---|---|---|
| `direct_budget_refusals` cresce e `udp_direct_slots_available` è 0 | più carrier QUIC che slot: i nuovi restano sul relay TCP (funzionano, ma non in direct) | prima ridurre i `--carriers` dei tunnel grossi, poi `BORE_MAX_CARRIERS=12` o `16` (§8.6) |
| `udp_direct_slots_available` è 0 con pochi carrier vivi | non è P-15, che è corretto: è un'altra causa | indagare |
| `conn_rejections` > 0 | raggiunto `BORE_MAX_CONNS=512` | alzare `BORE_MAX_CONNS`. Il limite dei file descriptor si adegua da solo (P-12) |
| swap usata alta per giorni, con bore unico servizio | la memoria QUIC lavora vicino al caso peggiore | tornare a `BORE_UDP_MEMORY_BUDGET=512MiB` |
| il container non parte con `failed to write sysctl` | modulo `tcp_bbr` assente (per esempio dopo un cambio di kernel) | §3.6 |

### 8.8 Riavvio di prova (facoltativo, consigliato)
Lo spostamento di seafile è un buon momento per verificare che tutto torni da solo dopo un
reboot: il modulo `tcp_bbr` da `modules-load.d`, `net.core` da `sysctl.d` e `bore-server` con
`restart: always`.
```shell
sudo reboot
# al rientro:
lsmod | grep tcp_bbr
sysctl net.core.rmem_max net.core.wmem_max
docker ps --format '{{.Names}} {{.Status}}'
# poi la verifica di bbr del §8.1 e la riga 'budget applied' del §8.4
```

### 8.9 Pulizia facoltativa
- **Immagini Docker:** `docker system df` segnalava 7.9 GB recuperabili (vecchie
  `dind-official-full-minimal`, `bore:v1.2.1`, `bore:sha-da621a1`). Dopo il `down` di seafile,
  `docker image prune -a` rimuove le immagini che nessun container usa. Restano comunque
  scaricabili da ghcr.io. `minivm-sish`, fermo da 9 giorni, tiene la sua immagine da 3.2 GB
  finché esiste il container: `docker rm minivm-sish` se non serve più.
- **Journal di sistema:** occupa 2.4 GB. `sudo journalctl --vacuum-size=500M`, e
  `SystemMaxUse=500M` in `/etc/systemd/journald.conf` per tenerlo lì.
- **Credenziali in chiaro nel compose** (`BORE_SECRET`, `BORE_ADMIN_TOKEN`,
  `BORE_FAST_LINK_TRANSFER_AUTH`): spostarle in un `env_file:` con permessi 600. Attenzione:
  anche le copie `compose.yml.bak-*` le contengono.
- **`BORE_WEBSERVER_LOG=/logs`** scrive un access log su disco per ogni tunnel. Va bene, con
  40 GB liberi; tienilo presente.
- **Resto della VM:** `tailscaled` gira sull'host (lascialo se lo usi per entrare). Le cartelle
  `~/docker/dind-tp20`, `~/docker/minivm-sish` e `~/docker/tailscale` non hanno container in
  esecuzione.

### 8.10 Cosa NON fare
- **Non alzare `BORE_UDP_MEMORY_BUDGET` oltre 1 GiB** su questa VM (§8.5).
- **Non alzare `net.ipv4.tcp_mem`:** è il tetto con cui il kernel tiene il TCP dentro la RAM, e
  la RAM libera serve al budget QUIC. Non è mai risultato un limite.
- **Non togliere la swap.**
- **Non toccare `netdev_max_backlog`, `rmem_default`, `swappiness`:** nelle misure i buffer UDP
  non hanno mai perso pacchetti (§6.4), e il resto non è stato misurato.
- **Non impostare `SO_SNDBUF`/`SO_RCVBUF` sui socket TCP:** spegne l'autotuning del kernel.
  Valutato e scartato come dannoso in `docs/ssh-gateway/SSH_GATEWAY_ASSESSMENT_2026-07-10.md`.
- **`bbr` come default dell'host non serve:** bore ha il suo namespace, e lì `bbr` c'è già.
- **Non passare a `network_mode: host`** senza prima spostare le sysctl sull'host (README,
  "Host tuning for bandwidth"): con la rete host Docker rifiuta `sysctls: net.*`.
- **Non cambiare `BORE_DIRECT_QUIC_CC`:** su Wi-Fi `bbr`, `cubic` e `newreno` danno lo stesso
  risultato (§6.4).

### 8.11 Rollback
```shell
cd ~/docker/bore/app
ls compose.yml.bak-*   # 20261010 = prima delle sysctl, 20261011 = prima di bbr, la data di §8.4 = prima del budget
cp compose.yml.bak-<data> compose.yml
docker compose up -d bore-server
```

## 9. Valori cambiati e stato finale della VM

| Elemento | Prima | Dopo |
|---|---|---|
| `compose.yml` → `sysctls:` | assente | `tcp_wmem`/`tcp_rmem` `4096 … 16777216`, `tcp_slow_start_after_idle 0` (2026-10-10), `tcp_congestion_control bbr` (2026-10-11) |
| `/etc/modules-load.d/bbr.conf` | assente | `tcp_bbr` (2026-10-11) |
| Congestion control dell'host | `cubic` | `cubic`, invariato: cambia solo il namespace di `bore-server` |
| Commenti `BORE_MAX_CARRIERS`, `BORE_UDP_MAX_STREAMS` | errati (1024, "doubled"/8192) | corretti |
| Variabili d'ambiente bore | — | **nessuna cambiata** (budget 512MiB, max carriers 8, max streams 4096 invariati) |
| Immagine | `sha-da621a1` | `v1.2.3` |
| Container `bore-server` | in esecuzione da 7 giorni | ricreato 2026-10-10 01:13 UTC (sysctl) e 2026-10-10 23:08 UTC (`bbr`) |
| Backup | — | `~/docker/bore/app/compose.yml.bak-20261010`, `compose.yml.bak-20261011` |

Pulizia verificata:
- 2026-10-10: immagini, volumi, reti Docker, `iptables-save` e `nft list ruleset` **identici**
  allo snapshot iniziale, a parte l'ID del container ricreato. Rimossi i container di test
  (`bwt-srv`, `bwt-slow`), le immagini `networkstatic/iperf3` e `curlimages/curl`, e i log di
  test `logs/30049.log` e `logs/bwt1/`.
- 2026-10-11: rimossi i container di prova (`bwt-srv`, `bwt-udp-client`, `bwt-pub-client`,
  `bwt-pub-sink`, i server iperf3 e la loro immagine), i processi e i file Python in `/tmp`.
  Nel namespace di `bore-server` restano solo le due route originali (nessuna `/32` di prova).
  Sulla VM girano `bore-server` e `seafile-13-dind`, come prima (`minivm-sish` è fermo da
  prima delle prove).

Sulla workstation sono stati rimossi i container di test. Le immagini `networkstatic/iperf3`,
`nginx:alpine`, `curlimages/curl` e `ghcr.io/manprint/bore:v1.2.3` restano in cache locale.

## 10. Cosa NON è stato misurato
- SSH gateway, fast link, `bore transfer`, secret tunnel, web transfer e VPN. Il §3.7 dice quali
  tratte usano gli stessi socket TCP del server e quindi beneficiano di `bbr`. Il secret direct,
  il VPN direct e il WebRTC sono peer-to-peer e la VM non è sul percorso.
- Vhost `--udp` con HTTP reale su produzione: gli slot erano già tutti occupati dalla domanda
  reale. Il QUIC direct è stato misurato sul server temporaneo con config identica.
- 4G/5G e Wi-Fi con segnale debole: il Wi-Fi misurato era buono (−52 dBm). Su link peggiori le
  perdite aumentano e il vantaggio di `bbr` dovrebbe crescere, ma non è stato misurato.
- La causa del divario QUIC contro TCP+`bbr` nel verso server → client in Wi-Fi (§6.4).
- L'opzione A del §6.4 sul server di produzione: è misurata sul server di prova con le stesse
  finestre, e va verificata dopo lo spostamento di seafile (§8.4).

## 11. Riproduzione
I comandi di misura erano tutti in container Docker usa-e-getta: `networkstatic/iperf3`,
`curlimages/curl`, `nginx:alpine`, più un `bore server` temporaneo sulle porte 5201/5202 con
la stessa immagine e config di produzione. Schema delle prove:
- **sysctl:** stesso server temporaneo con e senza `--sysctl ...`, ripetizioni alternate, client
  con `--network host` (default) oppure bridge + `--sysctl` (tunato).
- **`bbr`:** vedi §3.9 (iperf3 gemelli, route `/32 congctl` nel namespace del server).
- **finestre QUIC:** `bore server --udp --control-port 5201 --vhost-quic-port 5202` in un container
  con le stesse sysctl di produzione e il porto pubblico pubblicato solo su `127.0.0.1`, riavviato a
  ogni braccio con il budget da provare; provider `bore local --udp` sulla workstation, `curl`
  sulla VM verso `127.0.0.1`.
- **slot e memoria:** K lettori `curl --limit-rate 50k` sulla VM, lettura di `VmRSS` del
  processo server da `/proc/<pid>/status` prima, durante e dopo.
- **costo CPU:** delta di `utime+stime` da `/proc/<pid>/stat` del processo server intorno a ogni
  prova.
