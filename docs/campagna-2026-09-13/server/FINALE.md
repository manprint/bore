# Server, registri e admin API — finale della campagna cablata (13 settembre 2026)

> *Cosa è risultato · cosa è stato cambiato · cosa resta aperto.*

Questo componente non ha una banda da riportare: ha **sei difetti** che a `info`
non si vedevano. Tutti condividono una forma, che è anche il motivo per cui sono
sopravvissuti a lungo: **il tunnel risultava registrato e non stava servendo**,
oppure **un contatore rispondeva un numero che non poteva sbagliare**.

## 1. Risultato

I tre registri di bore — pubblico, vhost, segreto — hanno avuto **lo stesso
difetto tre volte**, perché il canale di controllo è un substream yamux: contro
un peer semiaperto ma vivo a livello TCP, `send` mette in buffer e `recv` blocca
per sempre, quindi la `Registration` RAII non viene mai rilasciata. Il risultato
cambia con il registro, e peggiora: un conteggio gonfiato per i segreti, un
**sottodominio inutilizzabile fino al riavvio** per vhost (misurato bloccato
oltre 5 minuti su staging, mentre il percorso SSH si liberava in 20‑40 s), una
**porta pubblica occupata** fino al riavvio per i tunnel pubblici — e con un
intervallo `--min-port/--max-port` stretto, uno zombie lo esaurisce.

Il tetto configurato poteva essere **irraggiungibile per costruzione**: il
container girava con `--max-conns 1024` e `RLIMIT_NOFILE` soft 1024, e a ~976
connessioni tenute attraverso **un** tunnel loggava
`No file descriptors available (os error 24)` ogni 100 ms e non rispondeva sulla
porta di controllo per ~30 s — mentre `conn_rejections` restava **0**. `EMFILE`
arriva sull'`accept()` di **ogni** listener del processo: la concorrenza di un
tunnel porta giù l'admin API, i frontend vhost e tutti gli altri tunnel.

E un endpoint di **configurazione** pubblicava un **indicatore vivo**:
`udp_direct_slots` scendeva col carico e leggeva 0 su un server saturo,
indistinguibile da «nessun budget configurato». Trovato sul campo: l'endpoint
diceva `30` mentre l'avviso di avvio dello **stesso** server per lo **stesso**
budget diceva `slots=32`.

## 2. Ottimizzazioni apportate

| # | cosa | effetto misurato |
|---|---|---|
| **P‑14** | il monitor di chiusura di un carrier diretto agisce sul pool in cui si è **installato** (`Arc::downgrade` all'installazione, `upgrade()` alla chiusura), mai su ciò a cui la chiave si risolve dopo | `DirectPool::default()` riparte dagli **id 0** a ogni registrazione, quindi il monitor del tunnel *precedente* rimuoveva l'**id 0** dal pool *corrente*: il carrier vivo di chi c'è adesso. `Weak` e non `Arc` perché un monitor in attesa su una connessione che sopravvive alla registrazione non deve **ancorare** la entry |
| **P‑13** | i buffer del socket UDP si configurano **dentro** i costruttori `client_endpoint` / `server_endpoint`, e in nessun altro posto | era compito del **chiamante**, e l'unico che costruisce l'endpoint **condiviso** del server non lo faceva: girava su `net.core.rmem_default` — misurato `rb212992` — mentre il client dello stesso percorso QUIC loggava `effective_recv=8388608`. Il braccio diretto **incassava 3,885 GiB per consegnarne 2,279** (1,78×, cioè ritrasmissione) contro un braccio relay dove ingresso e uscita coincidevano allo 0,2 %: metà goodput (111 contro 213 MB/s), doppia CPU per GiB, 2,9× softirq |
| **P‑12** | `fdlimit::reconcile_fd_limit(max_conns)`, chiamata **prima** che il primo listener venga aperto: alza il soft a `max_conns + 256` quando l'hard lo consente, e altrimenti avvisa nominando **entrambi** i rimedi | un limite già sufficiente viene lasciato stare **e tace**: un server che parla di un non‑problema abitua il suo operatore a ignorare la riga che conta. La decisione è la funzione pura `fd_budget`, unit‑testata su ogni confine incluso `RLIM_INFINITY`; le conversioni vivono in `widen`/`narrow` al confine della syscall e **saturano** — `rlim_t` non ha la stessa larghezza su ogni target, e un wrap **abbasserebbe** in silenzio il limite |
| **P‑11** | `/admin/api/v1/config` pubblica il **totale configurato**; l'indicatore vivo sta su `/metrics` come `udp_direct_slots_available` | e la riga del frontend usa un controllo esplicito di `null`, mai la verità logica: **0 libero è il valore allarmante**, ed è esattamente quello che una guardia sulla verità logica nasconde |
| **P‑10** | `current_path` risponde `relay` quando il registro diretto non ha una entry **e** il tunnel non ha chiesto `--udp` | `unknown` resta per un tunnel `--udp` che non ha ancora servito nulla: l'unico caso senza risposta |
| **P‑9** | ogni scrittura di battito è limitata nel tempo (`beat_once`) e degrada al percorso legacy invece di incastrarsi | il red‑check **si blocca** invece di fallire: è il sintomo di produzione |
| **P‑4/F‑1** | il mietitore di entry zombie su tutti e tre i registri, sul **tick** del battito e mai con `timeout(recv)` | il braccio del battito vince il `select!` ogni 500 ms e azzererebbe un futuro `timeout(recv)` prima della sua scadenza. Il gate di compatibilità è l'`Option`: un client che non sa battere non viene **mai** mietuto, perché mieterlo ucciderebbe un tunnel sano e inattivo ogni 60 s |
| **M‑1** | una connessione mux si chiude quando la sua liveness arriva a zero | vedi [`../vpn/FINALE.md`](../vpn/FINALE.md) |

**Sul campo, oggi**: il server di staging è stato aggiornato e il tetto UDP
dell'host alzato a 16 MiB. Prima: `effective_recv=8388608 recv_forced=false` e
**5224 datagrammi scartati** dal kernel sul socket QUIC condiviso. Dopo:
`effective_recv=33554432` e **0 scartati**. Nota che `privileged: true` **c'era
già e non serviva a niente**: l'immagine gira come UID 1000 e Docker azzera ogni
capability per un UID non‑root anche sotto privileged, quindi niente
`SO_*BUFFORCE`.

## 3. Rimasto aperto

**Il pool diretto non viene chiuso alla deregistrazione.** `DirectPool::close_all`
è chiamata **solo** da `src/ssh_jump.rs`: né `PublicDeregister::drop` né il
`Deregister::drop` di vhost chiudono il proprio pool. I carrier QUIC di un
tunnel deregistrato restano finché non scadono per inattività, tenendo nel
frattempo un permesso di `admit_direct`. **Non è stato misurato.** Va misurato
prima di essere corretto — è la regola che questa campagna si è data e l'unica
cosa che distingue una correzione da un'ipotesi.

**N‑9 — la coda di concorrenza del relay** (vedi [`../vhost/FINALE.md`](../vhost/FINALE.md)
e [`../public/FINALE.md`](../public/FINALE.md)): nessun meccanismo da colpire,
e ogni candidato nominato è stato falsificato. Si chiude con un client nella
stessa regione.

**Il TCP resta deliberatamente non simmetrico all'UDP**: `shared::tune_tcp` non
imposta `SO_*BUF`, perché farlo **disattiva l'auto‑tuning** dei buffer TCP e
inchioda il socket a `net.core.*mem_max`. La proposta è stata esaminata e
**respinta come dannosa**. L'UDP non ha auto‑tuning, ed è per questo che la
funzione esiste.

## 4. Dove guardare

| file | cos'è |
|---|---|
| [`../evidenze/PUBLIC_STAGING_EVIDENCE_2026-09-11.md`](../evidenze/PUBLIC_STAGING_EVIDENCE_2026-09-11.md) | §11 (il budget di descrittori), §13.1 (i buffer del socket) |
| [`../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md`](../evidenze/ETH_RERUN_EVIDENCE_2026-09-12.md) | §31, §33, §37, §43 (M‑1), §48, §52‑53 (P‑14) |
| [`../RELAZIONE_FINALE_CABLATO_2026-09-13.md`](../RELAZIONE_FINALE_CABLATO_2026-09-13.md) | il registro completo dei difetti di prodotto e di strumento |
| [`../../../CLAUDE.md`](../../../CLAUDE.md) | gli invarianti da non rompere, uno per difetto |
