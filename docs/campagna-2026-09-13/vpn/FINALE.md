# VPN — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*
> Le misure stanno nei documenti citati in fondo; qui c'è la conclusione e il perché.

## 1. Risultato

**Il deficit del percorso diretto non esisteva: era la radio.** La campagna
precedente aveva costruito un'intera spiegazione (V‑6) attorno a un divario del
37 % fra il tunnel diretto e la banda nuda. Rimisurato via cavo, sulla stessa
riga di codice, il diretto vale il **94,2 % della linea in upload** e l'**89,3 %
in download**. Non è stato corretto niente per ottenerlo: è cambiato il mezzo.

Da qui discende la regola più importante che la VPN ha insegnato a tutta la
campagna: **un rapporto contro un controllo nudo campionato nella stessa
ripetizione è valido ovunque, un valore assoluto no** (V‑9). I documenti WiFi
restano validi nei rapporti e superati in ogni cifra assoluta.

**Quattro scale di tunable sono risultate piatte via cavo** — coda del
dispositivo TUN, buffer di invio dei datagrammi, buffer del socket UDP,
controllore di congestione. Non perché i parametri siano inerti, ma perché una
coda fa male solo quando si **riempie**, e al 95 % di un uplink cablato non
congestionato non si riempie mai. Il campo di validità di V‑10 è quindi «un
collegamento con perdita o a velocità variabile», non «ogni collegamento».

**Il relay crolla con il numero di flussi, il diretto no** (§12, §32,
riprodotto due volte). E i carrier sul relay **non recuperano**: a 4 flussi
`c4` vale il 75 % di `c1`, a 8 sono pari (12 ripetizioni per cella). Quello che
i carrier fanno è **spianare**: l'escursione di `c1` è 3,4×, quella di `c4` 2,0×
— stesso pavimento, soffitto molto più basso. È BW‑F2 letta al contrario.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **M‑1** | una connessione mux si chiude quando nessuno può più usarla (`Liveness`: opener + acceptor + substream, a zero il driver esce) | perdeva **una connessione di controllo per ogni riconnessione**, in `ESTABLISHED` a **entrambi** gli estremi, mai recuperata in 120 s. Misurata dal vivo: 4 connessioni perse su 4 riconnessioni, zero dopo |
| **V‑10** | `txqueuelen` del device TUN portato a **128** (`BORE_VPN_TUN_TXQUEUELEN`, `0` = non toccare, altrimenti `[16,500]`) | su WiFi il default del kernel era il peggior gradino su **entrambi** gli assi: 500 → 248,3 Mbit/s @ 147 ms, 128 → 265,7 @ 97. Via cavo la scala è piatta: il default resta sbagliato solo dove la coda si riempie |
| **V‑14a** | le scritture del relay sono **coalescate** (`recv_many`, `RELAY_WRITE_BATCH` 32) | una `write_all` per pacchetto erano un header yamux e un giro di driver per pacchetto: ~**52 000** di ciascuno al secondo a MTU 1350 e 570 Mbit/s. Il formato sul filo non cambia: il peer non può accorgersene |
| **V‑14b** | l'AEAD sigilla e apre in **una** allocazione e **una** copia (erano tre per verso) | ~53 000 pacchetti/s per verso su entrambi gli estremi. Il filo è **identico byte per byte**, e le funzioni libere restano come **oracolo** del confronto |
| **V‑13** | il buffer di invio dei datagrammi resta **8 MiB**, ma il tetto del tunable sale a 64 MiB (`BORE_DIRECT_DGRAM_SEND_BUF`) | sotto offerta UDP fissa la curva **si ribalta**: 8 MiB → 388 Mbit/s @ 227 ms, 32 → 462 @ 708, 64 → 410 @ 1092. Con un flusso TCP interno — la grandezza che conta davvero — 32 MiB comprano **+3 % per +12 ms**. Il default resta 8 |
| **V‑12** | il controllore resta **`bbr`**, e questa è una decisione, non una dimenticanza (`BORE_DIRECT_QUIC_CC` per chi ha qualificato la propria linea) | `newreno` 280,9 Mbit/s / rtt 108,3 ms · `cubic` 278,6 / 116,0 · `bbr` 272,0 / 123,0. Sono **+3,3 %** contro un divario del 30 %, e `newreno` compra la media **alzando il minimo** di rtt sotto carico (26,1 → 33,4 ms): esattamente la coda che V‑10 aveva appena tolto |
| **V‑11** | `export LC_ALL=C` in `vpnlib.sh` | sotto `it_IT.UTF-8` `sort -n` ordina `{397,46 · 264,01 · 408}` come `408 · 264,01 · 397,46` e la mediana legge **264,01**. Invisibile in un file che stampa solo mediane |

## 3. Rimasto aperto

**V‑15 — sul percorso diretto un datagramma perso *è* un segmento TCP interno
perso.** I pacchetti interni viaggiano su *datagrammi* QUIC, inaffidabili per
scelta (ritrasmetterli sotto un TCP incapsulato è il collasso TCP‑su‑TCP).
Mathis lega allora il flusso interno a `MSS / (RTT · √p)`: 1350 B a 19 ms con
p = 1e‑4 fanno ≈ 69 Mbit/s. **Misurato**: 95‑160 Mbit/s dove due cicli
precedenti leggevano 712, con `lost_pkts_d` 2‑8 ogni 5 s contro **esattamente
zero** prima. Non è correggibile ed è giusto che non lo sia. La conseguenza
operativa è una regola: **una fase che misura throughput sul diretto deve
stampare la perdita del portante accanto al valore** — un braccio lento con
`lost=0` e uno con `lost>0` sono diagnosi opposte e prima erano indistinguibili.

**V‑12 e V‑13 sono state misurate solo su un percorso con `lost_pct = 0,00`.**
Un controllore basato sulla perdita non è stato misurato sul caso in cui è
peggiore, e un default finisce su ogni linea. Da riaprire **con un percorso con
perdita nella matrice**, non su un altro numero di throughput singolo.

**D6/D7 — le due topologie che richiedono un terzo host.** Non aggirabili con
un altro esperimento locale.

**Il pool diretto non viene chiuso alla deregistrazione.** `DirectPool::close_all`
è chiamata **solo** da `src/ssh_jump.rs`: né `PublicDeregister::drop` né il
`Deregister::drop` di vhost chiudono il proprio pool, quindi i carrier QUIC di
un tunnel deregistrato restano finché non scadono per inattività, tenendo un
permesso di `admit_direct`. **Non è stato misurato**, e non va corretto senza
misurarlo prima.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`final_vpn_perf_review.md`](final_vpn_perf_review.md) | la revisione completa in italiano |
| [`VPN_DEEP_DIVE_PLAN.md`](VPN_DEEP_DIVE_PLAN.md) | la matrice di copertura: ogni opzione CLI e ogni `BORE_*` contro la fase che la esercita, e cosa resta scoperto |
| [`VPN_CAMPAIGN_HANDOFF.md`](VPN_CAMPAIGN_HANDOFF.md) | stato della campagna, per riprenderla a freddo |
| [`../evidenze/VPN_EVIDENCE_2026-09-12.md`](../evidenze/VPN_EVIDENCE_2026-09-12.md) | le evidenze VPN su WiFi (superate in ogni cifra assoluta) |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | la riesecuzione cablata — §3, §5‑10, §12‑15, §30‑33, §37, §43, §45, §51 |
