# `bore transfer` — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

## 1. Risultato

**Il trasferimento file raggiunge il 99,2 % della linea nuda**, su entrambi i
trasporti, con il percorso verificato dal log del mittente su ogni riga: 87,5
MiB/s = 734 Mbit/s contro un upload nudo di 740. Il modo di trasferimento non
rinuncia praticamente a niente.

**E contro lo stato dell'arte è lo stato dell'arte.** Stessa linea, stessi sei
minuti, 1024 MiB, `--parallel 8`:

| strumento | MB/s | cos'è |
|---|---|---|
| **bore‑direct** | **82,05** | QUIC punzonato, server fuori dal percorso (verificato) |
| **bore‑relay** | **81,59** | attraverso il server — **due** tratte WAN |
| tar‑ssh | 80,19 | il caso migliore dei classici: un flusso, nessun protocollo per file |
| link | 80,06 | `ssh` → `/dev/null`: nessun filesystem, nessun protocollo |

Il relay batte `tar-ssh` **pur attraversando la WAN due volte**.

**Su molti file piccoli le correzioni valgono 2,94×** su 20 000 file: il
pre‑hash del mittente è stato spostato fuori dalla scansione, in parallelo, e
non fa più una lettura dell'intero albero a thread singolo prima del primo byte.

## 2. Ottimizzazioni apportate

Sette difetti corretti, più una riscrittura di prestazioni. I tre che contano di
più:

| # | cosa | perché |
|---|---|---|
| **P1** | il pre‑hash gira in `hash_planned_entries`, in parallelo e dopo la scansione | prima era in linea dentro `scan_entry`: una lettura completa dell'albero, a thread singolo, **prima del primo byte trasferito**. Vale il 2,94× su 20 000 file |
| **B7** | il ciclo di accept dei worker **valida il primo frame da sé** e risponde a un estraneo con un frame di errore, invece di adottarlo come worker | il `Begin` di un secondo mittente veniva adottato, e il suo fallimento **abortiva il trasferimento in volo** |
| **B5** | `commit_stage` è idempotente sul contenuto per ogni figlio | un figlio già committato da un'esecuzione parzialmente fallita veniva ritentato **per sempre** |
| B1/B3/B4/B2/B6 | il ricevitore limita i manifest (mai `with_capacity` su un `u64` scelto dal peer); la fase pre‑manifest è limitata nel tempo; `state.json` viene fsync‑ato prima del rename e, se corrotto, riparte **da zero** invece di restare in errore permanente; `ProgressTracker` ha un `Drop`; `--source-files` tratta come commento solo il `#` iniziale | — |

**E quattro regole di misura**, che in questa campagna hanno intercettato **tre
artefatti già pronti per essere pubblicati**: misurare su `tmpfs` (altrimenti si
misura il disco), interlacciare i bracci dentro la ripetizione (la deriva si
cancella solo se è comune), usare porte **sotto** l'intervallo effimero (una
porta effimera può essere già occupata e il fallimento sembra un risultato), e
far **fallire rumorosamente** una fase che non ha misurato niente.

## 3. Rimasto aperto

**Una cella su venti non torna: il diretto a `--parallel 1` legge 45,8 MiB/s**
mentre `--parallel 2` recupera la linea esattamente. Un limite per flusso che si
dimezza con un flusso e sparisce con due ha la forma di `qualcosa / RTT`, e
l'aritmetica è invitante: `CHUNK_SIZE / RTT` = 1 MiB / 19 ms = 52,6 MiB/s, la
grandezza giusta.

**Quell'ipotesi è falsificata dalla stessa tabella**: il braccio relay gira lo
**stesso** protocollo a chunk allo **stesso** RTT e legge 87,5 MiB/s a
`--parallel 1`, quindi il limite non può stare nel ciclo dei chunk. Anche il
controllo di flusso è escluso, per **lettura** e non per misura:
`holepunch::transport_config` è l'unico posto del crate che costruisce una
`quinn::TransportConfig` e installa una finestra di ricezione per stream da
16 MiB — 842 MB/s a questo RTT.

Resta qualcosa di specifico a **un** singolo stream QUIC sul percorso diretto:
controllo di congestione, pacer di quinn, o CPU in ricezione sul listener a
2 vCPU. **Non è ancora un risultato**: un campione, una cella. È annotato qui
*con la sua ipotesi falsificata* perché la prossima campagna non spenda la
stessa ora a ri‑derivare `CHUNK_SIZE / RTT` e a crederci. Per chiuderla serve
ciò che V‑15 già pretende da ogni fase di throughput sul diretto e questa non fa
ancora — **stampare la perdita e l'RTT del portante accanto al valore** — più
una seconda ripetizione: `REPS=2 PARS="1 2" ARMS=direct`.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`final_transfer_perf_review.md`](final_transfer_perf_review.md) | la revisione completa in italiano |
| [`../evidenze/TRANSFER_EVIDENCE_2026-09-12.md`](../evidenze/TRANSFER_EVIDENCE_2026-09-12.md) | la campagna `bore transfer` |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | §22‑23 |
