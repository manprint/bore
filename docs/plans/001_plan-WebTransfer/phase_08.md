# Phase 7 — Prestazioni del percorso diretto e controllo del percorso

> **Intent:** il percorso diretto deve essere il piu' veloce disponibile, non il piu' lento; il passaggio fra diretto e relay deve essere trasparente in entrambe le direzioni; e chi apre la stanza deve poter imporre il relay.
> **Shippable alone?** yes — nessuna sotto-fase cambia il formato dei file su disco ne' il significato di un'offerta.
> **Preconditions:** Phase 4 e Phase 6 chiuse; misura a due host disponibile (`scripts/perf/web_transfer_wan.sh`).

## State contract (mandatory)

1. Before touching anything: read [STATE.md](STATE.md). If §1 `Status` is `OPEN`, finish or revert that unit first (§6 says how far it got). Run the gate commands in STATE.md **§3** and check the result against what §1, §7, and §11 claim; the repo wins, so correct the file when they disagree.
2. **Open the sub-phase in STATE.md §1 before editing any code**: `Type: sub-phase`, its `ID`, `Status: OPEN`, `Intent`, `Next action:`, and §6 set to `claimed — nothing written yet`. Write or update the listed tests first or alongside production edits; do not defer them to a later unit.
3. **Close it after the gates are green**: append the §4 ledger row, reset §6 to `none — tree consistent`, update §5 §7 §8 §9 §10 and the §11 board, point §1 at the next unit with `Status: none`, bump the timestamp.
4. If the session ends mid-sub-phase, leave §1 `OPEN` and write exactly what is half-finished into §6 before stopping.

---

## La misura che apre la fase (non e' un'assunzione, e' il dato)

Due host reali, sorgente locale su ethernet e destinatario su una VM AWS, RTT 30.3–32.5 ms,
linea qualificata a 747 Mbit/s con P=1 ≈ P=8 (nessun limite per-flusso), UDP 400 Mbit/s a
perdita 0.00 % (`docs/transfer/WEB_TRANSFER_PERF.md` §5):

| braccio | banda | ttfb | verify |
|---|---|---|---|
| diretto (1 DataChannel) | **6.77 MiB/s** | 141 ms | 4942 ms |
| relay (WebSocket) | **37.25 MiB/s** | 175 ms | 1495 ms |

Il diretto e' **5.5 volte piu' lento del relay**, e la traccia dice esattamente dove va il tempo:
`waits=7 longest=771ms total=4131ms peak=5239088` — l'**84 %** del trasferimento e' il mittente
fermo in `waitLow` con 5 MiB gia' accodati nel `bufferedAmount`. La banda non cresce con la
dimensione (32 MiB → 6.77 MiB/s, 64 MiB → 7.19 MiB/s con 86 % di attesa): **finestra fissa, non
rampa**. 7 MB/s × 31 ms ≈ 220 KB in volo, che e' il send buffer SCTP per associazione (256 KiB)
diviso l'RTT. Il conto torna a meno del 15 %.

La verifica decisiva e' la scalabilita' per **associazione**, misurata con N trasferimenti
concorrenti (quindi N `RTCPeerConnection` distinte) fra gli stessi due host:

| associazioni | aggregato |
|---|---|
| 1 | 5.38 MiB/s |
| 2 | 10.57 MiB/s |
| 4 | **41.42 MiB/s** |

Scala lineare e a N=4 il diretto supera gia' il relay (41.4 contro 37.25) con la CPU del
destinatario ferma (load 0.00, nessuna saturazione): **il limite e' per associazione e si
compra con associazioni in piu'**. Questo e' il motivo per cui i carrier di questa fase sono
`RTCPeerConnection` separate e non canali multipli sulla stessa: gli stream di una associazione
SCTP condividono cwnd e rwnd, quindi N canali su una sola PeerConnection non comprerebbero nulla.

## Contratti fissi per questa fase

- **C7-1 — nessun cambio di formato dei frame.** Il numero di sequenza e' gia' nell'header di
  16 byte in chiaro e autenticato dall'AEAD come additional data. I carrier si appoggiano a
  quello; nessun byte nuovo entra nel frame, quindi il relay resta identico al byte.
- **C7-2 — nessun riuso di nonce, mai.** Il nonce e' derivato dal solo numero di sequenza; due
  carrier non possono emettere la stessa sequenza. La sequenza resta unica per trasferimento e
  per tentativo, assegnata dal mittente prima di scegliere il carrier.
- **C7-3 — `carriers = 1` e' identico al percorso di oggi**, byte per byte e messaggio per
  messaggio: una sola PeerConnection, un solo canale, nessun campo nuovo sul filo.
- **C7-4 — il relay resta caldo.** Il diretto non e' mai un prerequisito: ogni fallimento di un
  carrier degrada in luogo, e la perdita di TUTTI i carrier degrada al relay senza far fallire
  il trasferimento.
- **C7-5 — `--relay-only` e' una proprieta' della STANZA**, dichiarata da chi la apre, e il
  server la fa rispettare. Un server che non la conosce non deve poterla ignorare in silenzio:
  il client che l'ha chiesta e non la vede confermata esce con un errore esplicito.
- **C7-6 — il numero di carrier e' un tetto, non una riserva.** Un tentativo che ne stabilisce
  meno del richiesto usa quelli che ha; solo zero carrier e' un fallimento del diretto.

## Sub-phases

### 7.1 `--relay-only`: la stanza che usa solo il relay — `DONE`

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** chi apre la stanza con `bore transfer web --relay-only` ottiene una stanza in cui
  nessun trasferimento tenta il percorso diretto.
- **Deliverables:** flag CLI; campo additivo su `CreateWebTransferRoom`; conferma obbligatoria
  nella risposta del server; `relayOnly` nella configurazione che il browser legge al join; il
  server salta la negoziazione diretta e va direttamente al relay; la pagina lo dice.
- **Tests:** `t_web_relay_only_room_never_negotiates_direct` (Rust e2e), `relay-only` nel gate
  browser, unit sul rifiuto contro un server che non conferma.

### 7.2 Riordino in ricezione sulla sequenza autenticata — `DONE`

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** abilitare i carrier senza toccare il filo: il destinatario smette di assegnare la
  sequenza in ordine di arrivo e legge quella dell'header, riordinando in una finestra limitata.
- **Deliverables:** finestra di riordino limitata in frame e in byte; un frame oltre la finestra
  fa fallire il tentativo (non il trasferimento); il percorso relay, che arriva gia' in ordine,
  non alloca nulla e resta invariato.
- **Tests:** unit sul riordino, sul duplicato, sulla sequenza stantia e sul superamento della
  finestra; il gate relay esistente resta verde senza modifiche.

### 7.3 Segnalazione dei carrier — `DONE`

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** portare un indice di carrier su `rtc.offer`, `rtc.answer` e `rtc.ice`, e il numero
  di carrier su `transfer.direct_start`.
- **Deliverables:** parser Rust estesi con campo additivo; la macchina a stati del server passa
  da booleani a maschere di bit per carrier; budget candidati per lato invariato come totale;
  `carriers` deciso dal server e limitato da un massimo di configurazione.
- **Tests:** unit sui parser e sulla macchina a stati (offerta doppia sullo stesso carrier
  rifiutata, offerte su carrier distinti accettate), gate e2e di negoziazione.

### 7.4 Carrier nel browser — `DONE`

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** N `RTCPeerConnection` per tentativo, con il mittente che scrive sul carrier meno
  carico.
- **Deliverables:** l'attore del tentativo diventa un insieme di carrier; pronto quando almeno
  uno lo e'; il sink sceglie il carrier con meno byte accodati, cosi' nessun carrier fa da testa
  di coda agli altri; la diagnostica riporta per carrier.
- **Tests:** unit sulla scelta del carrier e sulla degradazione a carrier singolo; gate e2e sui
  tre motori; `carriers = 1` dimostrato identico.

### 7.5 Trasparenza del percorso nelle due direzioni

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** la caduta del diretto non deve costare il trasferimento, e il ritorno del diretto
  non deve costare la banda per il resto del trasferimento.
- **Deliverables:** perdita di un carrier su N: si continua sui restanti; perdita di tutti:
  degrado al relay in luogo; risalita al diretto quando torna possibile, con chiave e sequenza
  nuove come gia' fa il degrado.
- **Tests:** e2e che uccide il diretto a meta' e verifica i byte esatti; e2e che parte sul relay
  e risale; unit sul contatore dei tentativi.

### 7.6 Misura sul campo e scelta del default

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** il numero di carrier di default esce da una misura a due host, non da una stima.
- **Deliverables:** `scripts/perf/web_transfer_wan.sh` esteso con l'asse dei carrier; la tabella
  in `docs/transfer/WEB_TRANSFER_PERF.md`; il default scritto accanto al numero che lo giustifica.
- **Tests:** `T-WEB-PERF-WAN` registrato come gate di campo.

### 7.7 Coerenza del frontend e documentazione

- **Model:** `agent-1:Claude-Opus-5`
- **Intent:** quello che la pagina dice deve essere quello che il trasporto fa.
- **Deliverables:** badge del percorso che regge i carrier; messaggi utente nel posto giusto;
  README aggiornato su `--relay-only`, sui carrier e sulla velocita' misurata.
- **Tests:** `t_web_readme`, gate frontend esistenti.
