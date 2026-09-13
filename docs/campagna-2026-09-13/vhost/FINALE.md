# `bore vhost` — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

## 1. Risultato

**Via cavo il vhost raggiunge il 95 % della linea**, e attraverso un vero file
server arriva al **rate della linea in entrambe le direzioni** (§19‑20). Il
risultato interessante però non è la velocità: è che **relay e diretto si
separano sul costo, non sulla velocità**. Sulla stessa linea pulita il relay è
uguale o migliore, costa **circa metà della CPU per GiB** consegnato (6,85
contro 13,00 core‑secondi) e genera **36×** meno superamenti di allowance
dell'istanza per gli stessi byte. Il diretto vince in due casi soli, ed è per
quelli che esiste: **molte** connessioni tenute contemporaneamente (14 ms contro
1436 ms dietro 512) e un **percorso con perdita** (2,7× più veloce sotto perdita
iniettata).

La conseguenza pratica sta nel compose del forwarder: il profilo raccomandato è
il **relay**, e `--udp` è l'eccezione motivata.

**I fallimenti erano lenti, invisibili, o entrambi.** Un'origine irraggiungibile
dava `http=000` — indistinguibile da un tunnel morto, un server morto o una rete
morta. Un'apertura diretta non limitata nel tempo produceva `http=000` dopo
**9,90 s** durante un blackout UDP, mentre il fallback aggregato era sano. E i
contatori mentivano: `direct_stream_opens` contava i **tentativi** e saliva 1 → 12
durante una perdita UDP del 100 %.

**718 avvisi su 786** raccolti dalla campagna erano `peer closed connection
without sending TLS close_notify`: un browser che chiude una connessione.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **F‑12** | 502 su origine irraggiungibile, sintetizzato **dentro `poll_shutdown`** sul lato pubblico | è l'ultimo istante in cui la metà di scrittura pubblica è ancora aperta. Non spostabile prima (leggere il primo byte di risposta prima dello splice blocca ogni upload) né dopo (scrivere dopo `copy_bidirectional` produce una risposta **vuota** — misurato, non supposto) |
| **F‑14** | apertura diretta limitata: `DIRECT_OPEN_TIMEOUT` 3 s, che copre `open_stream` **e** `write_stream_ready` **insieme** | un percorso QUIC semiaperto può accettare `open_bi` e non consegnare mai il marcatore: limitare solo l'apertura lascia intatta la prima richiesta non limitata |
| **F‑14b** | contatori onesti: `direct_stream_opens` conta i **successi**, `direct_fallbacks` per ricaduta, `VhostEntry.last_path` pubblica quale trasporto ha usato l'**ultima** connessione | nessun contatore può esprimerlo: un tunnel che ha negoziato diretto ed è ricaduto per ogni connessione era indistinguibile da uno sano |
| **F‑13** | `--udp-memory-budget`: un tetto **a livello di server** sulla memoria che il percorso diretto può impegnare | senza, `tunnel × carrier × connection_receive_window` non ha limite: 32 lettori lenti su **un** tunnel prendevano 536,8 MiB su un host da 903 (relay: 95,1 — **4,5×**). Con il budget, picco 340,4 → **42,2 MiB** e il server continua a servire. Costo misurato sul diretto: **nessuno** (53,16/56,25 contro 52,28/49,72 MB/s) |
| **F‑3/F‑6** | disconnessioni benigne classificate per `std::io::ErrorKind`, **mai** per testo, e loggate a `debug`; `/admin/api/v1/config` **deriva** la sezione vhost dalla configurazione viva a ogni lettura | l'endpoint dichiarava assenti header di risposta che il banner del gateway SSH stampava al client connesso: `ConfigView` è un'istantanea di avvio e quegli header vivono solo in `vhost.yml` |
| **fase 03** | scheduling del *bulk* per **byte mossi** (mai per URL o metodo) e carrier automatici (`--carriers 0`, default resta 1) | con niente da schedulare si riduce al round‑robin di oggi **byte per byte**: la campagna aveva misurato i carrier che *danneggiano* leggermente un percorso pulito (mediana c4/c1 0,941), quindi uno scheduler che cambiasse comportamento a vuoto sarebbe stato lui la regressione |
| **P‑13/P‑14** | valgono anche qui: l'endpoint QUIC è **condiviso**, e vhost costruisce una `VhostEntry` nuova a ogni registrazione del provider sotto la stessa etichetta | vedi [`../server/FINALE.md`](../server/FINALE.md) |

## 3. Rimasto aperto

**N‑9 — la coda di concorrenza non è in questo codice, e non va ri‑bisecata in
locale.** Staging: una richiesta fresca dietro 256 connessioni tenute costa
966 ms, dietro 512 costa 1436 ms, contro 14 ms su QUIC diretto. Su un server
privato fissato allo stesso numero di core, **entrambi** i trasporti leggono
**11 ms piatti da 16 a 512**, e ogni meccanismo nominato cade uno per uno: il
semaforo `--max-conns` non accoda mai (a saturazione `try_acquire_owned`
fallisce, `conn_rejections` incrementa e la connessione viene **scartata** in
4 ms — quindi una richiesta senza permesso è più **veloce** di una servita); i
carrier 1/4/8 sono invarianti; 512 attive contro 512 inattive sono invarianti;
il percorso di controllo unificato non costa niente di misurabile; e rifare il
carico con 512 processi `curl` su un client fissato a 2 core non cambia nulla.
Resta il *token bucket* dell'istanza, che è anche l'unica cosa che spieghi
l'asimmetria fra trasporti. I timer sul percorso di apertura sono stati
**deliberatamente non aggiunti**: la richiesta intera dura 11 ms a 2,10 ms di
RTT, di cui ≈8,4 sono quattro round trip inevitabili — non c'è attesa da
scomporre.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`final_vhost_perf_review.md`](final_vhost_perf_review.md) | la revisione completa in italiano, scritta per essere leggibile da un non specialista |
| [`../evidenze/VHOST_STAGING_EVIDENCE_2026-09-10.md`](../evidenze/VHOST_STAGING_EVIDENCE_2026-09-10.md) | la campagna «prima» |
| [`../evidenze/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md`](../evidenze/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md) | la campagna «dopo»: prima/dopo appaiati, verifica dei bug, collo di bottiglia per test |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | §19‑20, §39 |
