# Tunnel pubblici (`bore local`) — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

## 1. Risultato

**Qui è stato trovato il difetto più grave dell'intera campagna, P‑14**, e la
forma in cui si presentava è quella che questa campagna ha imparato a temere:
il tunnel risultava **registrato e sano a `info`**, e serviva ogni connessione
sul relay per il resto della sua vita.

Un tunnel pubblico `--udp` che si **ri‑registra sulla stessa porta** perdeva il
percorso diretto dopo 12 secondi. Misurato sul percorso reale (§48): una porta
mai usata prima sopravviveva a 45 s di inattività **2 volte su 2**; la stessa
porta ri‑registrata subito dopo la morte di un tunnel perdeva il carrier a
**t = 12 s, 2 volte su 2** — l'idle timeout QUIC della connessione *precedente*
— mentre `tcpdump` sul socket del client mostrava quella connessione **ancora
viva**, con keep‑alive ogni 3 s, ognuno risposto in ~1 ms.

La connessione era viva ed è stata **sfrattata**. È per questo che nessuna
spiegazione basata su timeout regge, ed è per questo che il client non se ne
accorgeva: niente si chiudeva, quindi `spawn_direct` non rilasciava, quindi
nessun `PublicUdpRenew` partiva — e il pool diretto viene ricaricato **solo** da
quel segnale di chiusura (a differenza del pool di carrier TCP, che ha
`carrier_redial.tick()`).

**Il costo in pacchetti del percorso diretto è ora un numero, non un'impressione**
(§47.8). Con la segmentazione disattivata su **entrambi** i protocolli sulla
NIC della VM — premessa verificata prima della misura — il percorso diretto
spende **+27,1 %** di pacchetti per byte consegnato (1,279 · 1,243 · 1,272 sulle
tre ripetizioni). Di questi, solo **5,9 punti** sono byte in più: gli altri ~20
sono **gli stessi byte tagliati più fine** (1252 contro 1502 B per frame). E
l'istanza **conta pacchetti**: `pps_allowance_exceeded` 109 contro 18 714, cioè
**172×**, a payload identico.

**Le fasi pubbliche erano tarate per una radio.** Via cavo, 96 MiB per braccio
sono 0,83 s a 922 Mbit/s, per la maggior parte slow start: gli stessi bracci a
una variabile di distanza davano rapporti da 0,623 a 1,323. È l'origine di V‑19.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **P‑14** | il monitor di chiusura di un carrier diretto agisce sul pool in cui si è **installato**, mai su ciò a cui la chiave si risolve più tardi (`Arc::downgrade` all'installazione, `upgrade()` alla chiusura) | vedi sopra. `ssh-jump` **non** aveva il difetto — rimuove dall'`entry` catturata — ed è il precedente in‑repo. Verificato tre volte: red‑check unitario (legge 0 invece di 1), cancello di campo (`pool=0` con entrambe le premesse verdi), percorso WAN reale |
| **P‑7** | il `select!` di `serve_tunnel` risponde a `PublicUdpRenew` | un tunnel `--udp` il cui percorso diretto moriva mandava **una** richiesta di rinnovo, aspettava una risposta che non poteva arrivare e restava solo‑relay per tutta la vita della connessione di controllo. Misurato ancora degradato a **100 s**; **5 s** dopo la correzione |
| **P‑9** | la scrittura del battito di controllo è **limitata nel tempo** (`beat_once`, 10 s, `BORE_CTRL_HEARTBEAT_SEND_TIMEOUT_MS`) | regressione introdotta da P‑4: contro un peer che non **legge** quel substream il credito da 256 KiB si riempie e il braccio del `select!` blocca **per sempre** — «registrato ma non serve», la forma peggiore. Misurato su staging: servito a t+5 s e t+15 s, **nessuna risposta** da t+30 s, ~15 000 frame non letti. Il red‑check **si blocca** invece di fallire: è il sintomo di produzione |
| **P‑10** | `current_path` non legge mai `unknown` per un tunnel che ha un solo percorso possibile | un tunnel di solo relay non aveva dove registrare un percorso e l'API rispondeva `unknown`, letto da un operatore come «il server non lo sa» — quando invece lo sa. `unknown` resta per un tunnel `--udp` che non ha ancora servito nulla: l'unico caso senza risposta |
| **V‑19** | la dimensione del trasferimento è una **variabile** (`XFER_MB`, default cablato 384 MiB) | un trasferimento più corto della rampa misura la rampa |

## 3. Rimasto aperto

**§54.1 — perché il leg VM → server resta a MTU 1200 su QUIC.** L'aritmetica è
chiusa: 1202,7 B di payload UDP misurati contro i 1200 di `INITIAL_MTU` di
quinn, 0,2 % di errore. Ma la sonda appaiata `test-udp` sullo **stesso** binario
ha misurato **1452 con 4 sonde e 0 perse**, quindi la scoperta del PMTU
*funziona*. La domanda si è **ristretta** invece di chiudersi: cos'ha di diverso
quel leg, o quel percorso di codice. Vale ~17 % dei pacchetti del percorso
diretto, e l'istanza conta pacchetti.

**N‑9 — la coda di concorrenza del relay non è in questo codice.** Su staging
una richiesta nuova dietro 256 connessioni tenute costava 966 ms e 1436 ms
dietro 512, contro 14 ms su QUIC diretto. Su un server privato fissato allo
stesso numero di core **entrambi** i trasporti leggono **11 ms piatti da 16 a
512**, e ogni meccanismo nominato è falsificato uno per uno. L'ipotesi
superstite è il *token bucket* dell'istanza. Resta aperta **di proposito**: non
c'è un meccanismo da correggere, e una correzione speculativa toccherebbe il
percorso di accept che la misura ha appena dimostrato pulito. Si chiude con un
client nella **stessa regione**, non con un altro esperimento locale.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`final_public_perf_review.md`](final_public_perf_review.md) | la revisione completa in italiano |
| [`CARRIER_TUNING.md`](CARRIER_TUNING.md) | la guida ai `--carriers` rivolta all'operatore |
| [`../evidenze/PUBLIC_STAGING_EVIDENCE_2026-09-11.md`](../evidenze/PUBLIC_STAGING_EVIDENCE_2026-09-11.md) | registro difetti, A/B appaiato fra trasporti, concorrenza, netem, CPU s/GiB, soak |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | §21, §35‑36, §39‑42, §46‑50, §52‑54 |
