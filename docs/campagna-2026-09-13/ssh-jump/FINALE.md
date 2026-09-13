# Jump host SSH — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

Il jump host è l'unico modo d'uso in cui **la banda non è il prodotto**: `ssh -J`
porta una sessione interattiva, quindi la grandezza è il tempo di andata e
ritorno. Tutto quello che segue misura millisecondi, non Mbit/s.

## 1. Risultato

**L'82 % dell'apertura di una sessione era un `sleep`.**

`russh::server::Config::auth_rejection_time` vale **1 secondo** di default, e
`auth_rejection_time_initial` di default è `None`, che **ricade** su quello.
bore non impostava né l'uno né l'altro, quindi il gateway dormiva un secondo
intero prima di dire a un client **quali metodi di autenticazione esistono**.
Ogni client OpenSSH apre con una richiesta `none`: RFC 4252 §5.2 ne fa la sonda
di **enumerazione** dei metodi, e il rifiuto del server è ciò che porta la lista
`Authentications that can continue` senza la quale il client non può offrire
nulla.

Misurato sul percorso reale, `ssh -v` con timestamp per riga, 24 ms di RTT:

```
scambio di chiavi concluso   0,133 s
lista dei metodi             1,155 s   <- 1,022 s in quel solo intervallo
apertura del canale          1,202 s
banner interno               1,252 s
```

Di un'apertura da 1,25 s il lavoro del gateway era ~46 ms e un'attesa fissa era
**l'82 %**, pagata da **ogni** sessione su **ogni** ingresso (jump, vhost
`ssh -R`, public, secret). Dopo la correzione lo stesso intervallo è **20 ms**,
un RTT: `wchan` 1248,6 → **250,8 ms**, `open` 1592,7 → **657,8 ms**.

**E il resto del termine si scompone**, il che è importante perché per un anno
il commento della fase aveva chiamato «costo del gateway» l'intero
`wchan - tcp`. Con il secondo tolto:

| termine | ms | giri | di chi è |
|---|---|---|---|
| scambio versione + chiavi | 86 | ~4 RTT | RFC 4253 — non nostro |
| autenticazione (`none` + publickey) | 61 | ~3 RTT | RFC 4252 — non nostro |
| apertura canale + dial del provider + banner | **46** | ~2 RTT | **nostro** |

Di ~210 ms, ~147 sono l'handshake SSH esterno che nessuna riga di questo
repository può accorciare.

**Per un jump host `--udp` non serve alla latenza**, e ora lo dicono **due**
fasi disegnate contro bias opposti: `jump_lat`, che interlaccia relay e diretto
dentro la **stessa** ripetizione e li trova indistinguibili allo 0,4 %; e
`jump_stab`, che campiona una **seconda baseline dopo il ritorno al diretto** —
`recovered` 0,922 contro `blackout` 0,923, coincidenti a un millesimo. Se il
relay costasse latenza, `recovered` tornerebbe verso 1,00 quando il percorso
torna diretto. Non torna: quel −7,7 % **è deriva**, non il prezzo del relay.

**La stabilità dà due volte lo stesso verdetto**, che è l'unica prova che il
verdetto non sia fortuna: `PASS 16 / FAIL 0 / SKIP 0` in entrambe le esecuzioni
— caduta sul relay in **11 s** (budget 30), ritorno al diretto in **15 e 16 s**
(budget 90), sessione fresca aperta in 600,8 e 608,7 ms con UDP buttato via,
`direct_fallbacks` 0 → 1 con i carrier 1 → 0, **5 rekey** attraversati, **una
sola riga** admin durante tutto il blackout, sessione ancora viva dopo 130 s.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **I‑SSH12** | `SSH_AUTH_REJECTION_TIME_INITIAL` = `Duration::ZERO`; `SSH_AUTH_REJECTION_TIME` resta 1 s per una credenziale **sbagliata**. Entrambi sovrascrivibili (`BORE_SSH_AUTH_REJECT_MS`, `BORE_SSH_AUTH_REJECT_INITIAL_MS`) attraverso un risolutore **puro** che su valore illeggibile tiene il default | vedi sopra. Ritardare `none` non rallenta nessun attacco — non porta un segreto da indovinare, e l'alternativa per l'attaccante è una connessione TCP nuova più uno scambio di chiavi, già molto più caro del secondo tolto. Ritardare una chiave o una password sbagliata invece sì. Un knob malformato non deve mai diventare **zero**, che qui significherebbe rispondere istantaneamente a ogni password sbagliata |

Le tre correzioni precedenti dell'ingresso SSH restano il fondamento su cui
questa fase ha potuto misurare: **I‑SSH10** (apertura di canale limitata a 15 s
e sfratto di una sessione incastrata dopo 2 timeout consecutivi), **I‑SSH9**
(demux che consulta l'ALPN **prima** del silenzio, altrimenti una preconnessione
speculativa del browser riceveva un banner SSH come corpo della pagina), e
**I‑SSH6/7** (una `ssh -R` senza `-N` non viene più uccisa dalla negazione della
shell, e riceve un banner di stato).

**Sui carrier**: ogni canale SSH usa **esattamente uno** stream bidirezionale,
quindi qui i carrier comprano **isolamento, non banda**. La domanda sensata era
se **costassero** latenza, e ha richiesto il braccio `direct4` per essere
risposta: non costano.

## 3. Rimasto aperto

**I ~147 ms di handshake SSH esterno non sono nostri** e non sono accorciabili
da questo repository: sono RFC 4253 e RFC 4252 a 24 ms di RTT. Un client che
riusa una sessione (`ControlMaster`) li paga **una volta**: un canale nuovo su
una sessione stabilita è la colonna `chan`, e non contiene handshake.

**Il costo del gateway, 46 ms, è a sua volta per lo più due attraversamenti
WAN**, perché il provider sta dall'altra parte. Ridurlo significa cambiare la
topologia, non il codice.

**Il difetto dello strumento veniva prima di quello del prodotto**, ed è la nota
di metodo che questa fase lascia: la fase pubblicava `wchan=1270,3 ms` come
latenza mentre `open` falliva onestamente, perché il suo helper finiva con
`... | head -c 1 >/dev/null || { echo FAILED; return; }` sotto il commento
«leggere un byte dimostra che il canale trasporta dati». **Non lo dimostrava:
una pipeline che produce zero byte esce comunque 0.** 1270,3 ms era il tempo che
ci metteva a fallire. Oggi `jump_wchan_ms` **ispeziona i byte** (legge 4
caratteri e pretende `SSH-`, che RFC 4253 impone) e `jump_require_inner_target`
verifica la **premessa** prima della misura — perché un estremo interno assente
non fa perdere una colonna, ne **corrompe un'altra** per sottrazione.

## 4. I documenti di questo componente

| file | cos'è |
|---|---|
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | **§44** (la sezione lunga: difetto dello strumento, poi quello del prodotto), §51.2 |
| [`../ETH_CAMPAIGN_PLAN.md`](../ETH_CAMPAIGN_PLAN.md) | §P6: le cinque domande che il blocco jump doveva rispondere |
| [`../../SSH_GATEWAY.md`](../../ssh-gateway/SSH_GATEWAY.md) | la guida operativa dell'ingresso SSH |
| [`../../README-JUMP-HOST.md`](../../ssh-gateway/README-JUMP-HOST.md) | il jump host lato utente |
| [`../../../scripts/perf/staging/jump/`](../../../scripts/perf/staging/jump/) | le tre fasi: `jump_lat`, `jump_hol`, `jump_stab` |
