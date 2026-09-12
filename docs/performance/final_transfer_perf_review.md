# `bore transfer` — revisione finale di prestazioni e correttezza

**Data:** 2026-09-12 · **Ramo:** `main` · **Scope:** `bore transfer listener` / `bore transfer sender`

Questo documento è la sintesi ragionata della campagna sul trasferimento file. Le misure
complete, con i comandi che le hanno prodotte, stanno in
`docs/performance/TRANSFER_EVIDENCE_2026-09-12.md`; qui c'è ciò che è stato trovato, perché,
cosa è stato cambiato e cosa resta aperto.

---

## 0. Premessa che decide tutto il resto

`bore transfer` **non è un quinto trasporto**. `transfer listener` si registra come
*provider* di un tunnel segreto e RICEVE; `transfer sender` è il *consumer*
(`secret::Proxy`) e INVIA. Di conseguenza ogni risultato della campagna sui tunnel segreti
(`docs/performance/final_secret_perf_review.md`) vale qui senza modifiche:

* sul braccio **diretto** i byte vanno sender ↔ listener su QUIC bucato, con il server
  fuori dal percorso (S-1); sul braccio **relay** attraversano il server due volte;
* `--parallel N` significa N stream bidirezionali QUIC sul diretto e N sottostream yamux
  sul relay;
* **un singolo stream è limitato da `finestra / RTT`**, quindi qualunque numero misurato a
  `--parallel 1` è una misura della finestra di ricezione, non del collegamento. Chi legge
  un `--parallel 1` come "la banda di bore" sta leggendo la finestra.

La conseguenza pratica: prima di sospettare il trasferimento, si legge la riga
`UDP direct path tuning` del server (§10 dell'evidence dei segreti). Un budget di memoria
basso diviso per un `--max-carriers` alto porta la finestra di stream al pavimento di 1 MiB,
e 1 MiB / 20 ms sono 50 MB/s **per stream** qualunque cosa faccia la rete.

---

## 1. Il risultato in una riga

A parità di byte trasferiti (2 048 MiB su tmpfs, mediana di tre ripetizioni per cella),
passando da 100 a 20 000 file il throughput crollava di **4,3 volte**: da 1 575,4 a
368,3 MB/s. Non era la rete e non era il disco — i byte non toccavano mai un disco: erano
tre strutture dati e una politica di durabilità dentro il ricevitore.

Dopo gli interventi lo stesso trasferimento a 20 000 file passa da **5,56 s a 1,89 s** con
il default invariato, cioè da **368,3 a 1 083,6 MB/s (2,94×)**, con la CPU del ricevitore
che scende da 10,08 a 6,83 s. La curva del binario spedito è **piatta da 100 a 5 000 file**
e perde il 21 % a 20 000; quella di prima ne perdeva il 77 %.

> **Una bozza precedente di questo documento diceva «38 volte».** Era sbagliato, e come era
> sbagliato vale più del numero: quella misura girava sull'NVMe, dove le pagine sporche di
> ogni cella vengono ancora scritte mentre la cella successiva viene cronometrata. Le celle
> in coda misuravano il disco che si riprendeva dalle celle in testa, non il codice. Tutto
> ciò che in questo documento è un numero di banda è stato ri-misurato su tmpfs (§4.1).

---

## 2. I difetti trovati

Sei su sette sono stati **misurati** prima di essere corretti; X-7 è stato trovato leggendo
il codice e misurato dopo, e il testo lo dice. Dove manca una misura, si vede.

| # | Difetto | Come è stato provato | Stato |
| --- | --- | --- | --- |
| X-1 | Le cache di file aperti, su entrambi i lati, non avevano limite | Riprodotto: `Too many open files (os error 24)` a ~989 file su 3 000 con `ulimit -Sn 1024`, picco 731 descrittori sul ricevitore | **CHIUSO** |
| X-2 | Scansione lineare dello stato di resume per ogni chunk (due volte) | Lettura del codice + effetto sul profilo CPU | **CHIUSO** |
| X-3 | Lo stato di resume veniva riscritto **per intero** ogni 8 chunk | `/proc/<pid>/io`: 2,48 GB scritti su disco per consegnare 512 MiB (4,62× di amplificazione) | **CHIUSO** |
| X-4 | La preparazione dell'area di staging era una catena seriale di round trip bloccanti, con un `mkdir` ridondante per file | `strace -c`: 5 001 `mkdir` su 5 003 restituivano EEXIST | **CHIUSO** |
| X-5 | Un chunk veniva dichiarato durevole prima che i suoi byte uscissero dal processo | Analisi del contratto di `tokio::fs::File::write_all` + `sync_staged_files` che riapre **per nome** | **CHIUSO** (non gated, §8) |
| X-6 | Ogni frame costava due `write` e due pacchetti | `strace -c`: 137 476 `sendto` per 20 000 chunk, 6,9 per chunk | **CHIUSO** |
| X-7 | Con `--no-fsync` il **giornale** di resume continuava a essere sincronizzato: un record durevole di byte non durevoli | Lettura del codice, poi **misurato** dopo la correzione (§4.2) | **CHIUSO** |

### 2.1 Perché X-3 era il più grave

Lo stato di resume è un JSON con una voce per file. Veniva riscritto integralmente ogni 8
chunk completati: con N file da un chunk ciascuno sono N/8 riscritture di N voci, cioè
**costo quadratico nel numero di file**. La verifica non è stata un ragionamento ma
un'aritmetica: 1,94 GB di scritture in eccesso ÷ 2 500 flush = 776 KB per flush, che è
esattamente la dimensione di uno `state.json` da 20 000 voci.

La sostituzione è un **giornale append-only** (`state.log`) accanto al checkpoint
`state.json`: 8 byte per completamento, aggiunti e sincronizzati nello stesso lotto che già
sincronizza i file in staging. Il checkpoint ora si scrive solo alla creazione, su
`reset_file` e una volta al caricamento dopo aver assorbito il giornale — ed è proprio
questo che tiene il giornale a 8 byte per chunk di **una** esecuzione invece di farlo
crescere tra un riavvio e l'altro.

Due proprietà portano il peso, ed è bene che restino esplicite:

* **la riproduzione del giornale è un'unione insiemistica**, quindi riprodurre due volte lo
  stesso giornale non cambia nulla: è ciò che rende innocuo un crash fra "checkpoint
  scritto" e "giornale rimosso";
* **una coda troncata** (crash a metà append) lascia meno di 8 byte finali: vengono scartati
  e il chunk viene rinviato — esattamente ciò che già accadeva con il vecchio "si perdono
  fino a 8 completamenti".

---

## 3. La politica di durabilità, con il prezzo scritto

Chiusi X-1…X-6, il costo dominante rimasto sul ricevitore è `fdatasync`. La campagna ha
quindi **misurato** cosa compra e quanto costa, invece di deciderlo a tavolino.

**Quanto costa dipende interamente dal supporto, ed è questo il risultato.** Su tmpfs le due
politiche sono indistinguibili — 1 374,5 contro 1 365,3 MB/s a 5 000 file, 1 083,6 contro
1 219,0 a 20 000 — perché sincronizzare una pagina che è già RAM non costa niente. Il prezzo
quindi **non è nel percorso di codice di bore**: è la latenza di flush del dispositivo, e va
misurato su un dispositivo.

| 512 MiB, 20 000 file, relay `ws → vm` | default | `--no-fsync` | rapporto |
| --- | --- | --- | --- |
| EBS di staging (§5) | 21,4 MB/s | 60,5 MB/s | **2,82×** |

Sullo stesso collegamento, fino a 5 000 file le due politiche stanno entro il 7 % l'una
dall'altra e tutte e due sono al limite del filo: il lavoro di durabilità per file è reale
ma costa meno della rete, quindi non si vede. Diventa il vincolo solo quando il numero di
file è alto abbastanza da togliere di mezzo la rete — e costa di più proprio sullo storage
di rete, che è quello su cui gira la maggior parte dei bersagli di un trasferimento.

**Cosa compra:** correttezza *per costruzione* attraverso un crash di macchina, invece di
correttezza *per rilevamento*. L'integrità è identica nei due casi — un chunk che questa
esecuzione non ha scritto di persona non è mai "fresco", quindi viene sempre ri-hashato
prima del commit, e i byte persi in un crash fanno fallire la verifica e vengono rinviati,
mai accettati in silenzio. E soltanto un panic del kernel o un'interruzione di corrente può
arrivare a quei byte: un Ctrl+C, una rete che cade o un processo ucciso non perdono nulla,
perché i dati sono già nel kernel.

`rsync`, `rclone` e `croc` **non** fanno fsync per file per impostazione predefinita. bore
mantiene il default più severo e aggiunge `--no-fsync` per scambiarlo, con il numero qui
sopra scritto nel `README.md`. **Cambiare il default è una decisione lasciata aperta**
(§9): è un cambio di garanzia, e i cambi di garanzia non si fanno di nascosto.

---

## 4. Banda e `--parallel`

Un solo file da 2 GiB, loopback, **directory di lavoro su tmpfs**, tre ripetizioni per cella,
mediana. Con un file solo il costo per-file esce di scena e resta il trasporto:

| `--parallel` | 1 | 2 | 4 | 8 | 16 | 32 |
| --- | --- | --- | --- | --- | --- | --- |
| MB/s | 829,1 | 1101,1 | **1226,3** | 1083,6 | 1077,9 | 952,6 |
| CPU s / GiB | 3,19 | 3,92 | 4,33 | 5,47 | 5,90 | 7,08 |

Saturazione a **`--parallel 4`** (~9,8 Gbit/s in loopback). Uno stream solo arriva al 68 %:
è il limite `finestra / RTT` del §0 che si vede perfino alla latenza del loopback. Oltre 4 la
curva gira: **32 stream sono il 22 % più lenti di 4 e costano il 63 % di CPU in più per GiB
consegnato.** Il parallelismo non è gratis, e qui il conto si vede.

### 4.1 Perché tmpfs, e come questa tabella è stata sbagliata una volta

La prima versione di questa tabella aveva la directory di lavoro sull'NVMe, dove ogni cella
scrive 4 GiB. Basta e avanza a rendere il **disco** la variabile di una tabella che parla del
**trasporto**.

È saltato fuori per caso e poi è stato confermato di proposito. Una cella ad alto
`--parallel` leggeva 93,9 MB/s dove prima leggeva 989: sembrava una regressione introdotta
da questa campagna. Ho costruito un worktree git sul commit pre-campagna e ho misurato il
binario **non modificato** nelle stesse condizioni: 113 MB/s a `--parallel 8`. Il baseline
era lento uguale, quindi la variabile non era il codice. `/proc/pressure/io` segnava
`full avg10=32,87` contro `avg10≈1` della tabella qui sopra; la causa erano ~3,3 GB di
directory di scarto lasciate da questa stessa campagna più 3,7 GB di fixture, con lo swap al
100 %.

Quindi: **`TMPDIR=/dev/shm`, e si legge `/proc/pressure/io` prima di credere a una cella.**
La regola che ne esce vale più del flag: *quando una misura si sposta, si rimisura il
baseline immutato nelle stesse condizioni prima di credere che sia stata la modifica.*

### 4.3 La cella che non tornava, e come è stata chiusa

Nella tabella del §1 la cella a 100 file è l'unica in cui il binario *prima* batte quello
*dopo* (1 575,4 contro 1 374,5 MB/s, 13 %). Una cella che è in disaccordo **di segno** con
tutte e quattro le sue vicine è o un costo reale della macchina nuova a basso numero di file
o un artefatto di una singola passata: pubblicarla in un senso o nell'altro senza verificare
è tirare a indovinare.

È stata ri-misurata con i bracci **alternati** (`prima, dopo, prima, dopo`), così che una
deriva della macchina non possa cadere su un braccio solo, a cinque numeri di file, sette
ripetizioni per cella:

```
  file     prima (2 rip.)           dopo (2 rip.)
  10       1600,0  1383,8           1612,6  1374,5
  100      1365,3  1365,3           1383,8
  300      1204,7  1211,8           1374,5
  1000     1204,7  1204,7           1374,5
  5000     1083,6  1083,6           1374,5  1383,8
```

A 100 file si legge **1 365,3 prima contro 1 383,8 dopo**: pari, due volte. Il 1 575,4 della
tabella principale era un singolo campione fortunato, e la regressione che sembrava mostrare
non esiste. Si guardi anche la riga a 10 file, dove i due bracci si muovono **insieme** tra
la prima e la seconda coppia (1600 → 1384 e 1613 → 1375): è esattamente la deriva che
l'alternanza serve a cancellare, ed è grande quanto l'effetto che stava per essere
pubblicato come risultato.

Due errori di banco vanno registrati qui perché producono **numeri plausibili**, non errori —
ed è questo che li rende pericolosi:

* Il primo tentativo appaiato filtrava l'output del banco con `awk` e stampava una colonna
  vuota quando la riga attesa mancava. Una cella andata in crash sembrava quindi una misura.
  Adesso i fallimenti vengono stampati con le ultime righe del banco.
* «Una porta unica per esecuzione» è unica solo rispetto ad altri *listener*. Le porte scelte
  all'inizio (47100+) stanno **dentro** `net.ipv4.ip_local_port_range` (qui 32768–60999),
  quindi le connessioni in uscita della cella precedente potevano prendere — e hanno preso —
  il numero che la cella successiva voleva mettere in ascolto: `failed to bind the control
  listener on 0.0.0.0:47106: Address already in use`. Le porte di un banco vanno **sotto**
  l'intervallo effimero.

Entrambe le lezioni sono dentro `scripts/perf/transfer_ab_shape.sh`, che è il modo in cui
questo confronto va rifatto: alterna i bracci dentro ogni cella, tiene le porte sotto
l'intervallo effimero e stampa ad alta voce una cella fallita invece di lasciare una colonna
vuota. Si confrontano i bracci **dentro** una cella, mai tra celle diverse.

### 4.2 Cosa dice sul default di `--parallel`, e cosa non dice

`resolve_parallel(0)` è `available_parallelism()` limitato a `[4, 32]`. Sulla macchina a 16
core usata qui il default è quindi 16: **1078 MB/s contro i 1226 disponibili a 4**, con il
36 % di CPU per GiB in più. Sembra un default sbagliato finché non ci si mette accanto
l'altra metà delle prove — sul percorso WAN di staging lo stesso sweep va nel verso opposto,
con il braccio diretto che quasi raddoppia da `--parallel 1` a `8` (§5). Uno stream solo è
limitato da `finestra / RTT`: il loopback non ha praticamente RTT, una WAN sì.

Il default è dunque un compromesso deliberato sbilanciato verso il caso ad alto BDP, e lo
sbilanciamento è dalla parte giusta: sbagliare su un percorso veloce costa ~12 % di banda,
sbagliare su una WAN ne costa la metà.

**Quello che questa tabella non può sostenere** è un cambio del limite superiore.
`--parallel 32` diventa il default solo su una macchina con 32 o più core, e la riga a 32
stream qui sopra è stata misurata su una macchina a 16 — mostra 32 stream che sovraccaricano
16 core, che non è la configurazione che quel default produrrebbe. Per decidere se il `32` è
giusto serve una macchina con ≥32 core; qui non è deciso, e la costante non è stata toccata.

---

## 5. Staging: le stesse domande su una rete vera

Tutto quello che precede è loopback, che è il substrato giusto per trovare i costi del
**codice** e quello sbagliato per dichiarare una banda. Qui le due domande si rifanno sul
percorso reale: workstation ↔ VM `t4g` in `eu-south-1`, relay attraverso il `bore server` di
staging. Harness: `scripts/perf/staging/xfer/xfer_bw.sh` e `xfer_shape.sh`.

**`--parallel` è una leva solo sul percorso diretto.** 1 024 MiB in un file, MB/s end-to-end
(il hole punch è dentro il numero):

| topologia | arm | par 1 | par 2 | par 4 | par 8 | par 16 |
| --- | --- | --- | --- | --- | --- | --- |
| ws → vm | relay | 80,3 | 80,0 | 80,5 | 78,7 | 74,7 |
| ws → vm | direct | 39,8 | 48,5 | 61,2 | **70,8** | 69,5 |
| vm → ws | relay | 45,0 | 40,9 | 49,5 | 48,5 | 49,2 |
| vm → ws | direct | 37,3 | 45,4 | 54,0 | 59,1 | **62,4** |
| vm → vm | relay | 111,6 | 98,5 | 89,6 | 85,1 | 86,7 |
| vm → vm | direct | 77,5 | 109,8 | 89,3 | 99,6 | 85,1 |

Il braccio **relay** è già al suo tetto con un solo stream — viaggia sul pool di carrier, che
con `--carriers 0` si dimensiona su `--parallel`, sopra TCP con l'autotuning del kernel
sotto — quindi altri stream non comprano nulla e sedici costano un poco. Il braccio
**direct** quasi raddoppia da 1 a 8: un singolo stream QUIC è limitato da `finestra / RTT`,
esattamente come in loopback, e su un RTT WAN quel limite morde. È tutta qui la
giustificazione del default `available_parallelism()` limitato a `[4, 32]` invece di 1.

**Il relay batte il percorso diretto su `ws → vm`, e non è un difetto.** 78,7 contro 70,8 al
miglior `--parallel`, e il meccanismo questa campagna **non** l'ha stabilito. Quello che la
misura dice da sola: il divario è ~10 % col default, non è un costo di rendezvous (il punch
sta in 37–53 ms quando il check round chiude pulito — campagna segreti §4), e nella direzione
opposta l'ordine si inverte (`vm → ws`, direct par 16 a 62,4 contro relay 49,2). Quindi
l'ordine dipende dal percorso, non è una proprietà del trasporto. La lettura onesta è che su
questo link nessuno dei due bracci è la scelta ovvia: bore prova diretto, ricade, e
**dichiara quale ha usato** — che è ciò che permette a un operatore di scoprirlo sulla
propria rete. Chiudere il meccanismo richiede una scomposizione perdita/RTT sul percorso UDP
ed è lasciato aperto (§9).

**Su una rete vera il costo per-file è invisibile — finché il bound non diventa l'fsync.**
512 MiB, braccio relay, `ws → vm`, `--parallel 8`:

| file | default | `--no-fsync` | rapporto |
| --- | --- | --- | --- |
| 1 | 68,6 MB/s | 63,2 MB/s | 0,92× |
| 100 | 73,6 | 73,3 | 1,00× |
| 1 000 | 72,6 | 71,1 | 0,98× |
| 5 000 | 67,0 | 71,6 | 1,07× |
| 20 000 | **21,4** | **60,5** | **2,82×** |

Fino a 5 000 file le due politiche sono indistinguibili e ogni cella sta ai ~70 MB/s del
link: il lavoro per-file esiste ma costa meno del filo, quindi non si vede. A 20 000 il filo
smette di essere il vincolo e lo diventa l'`fdatasync` del ricevente. Si noti che la stessa
coppia di politiche costa **zero** su tmpfs (§3): questo rapporto è la latenza di flush del
volume EBS, non un costo di bore. È esattamente la forma di risultato che giustifica
*spedire* `--no-fsync` invece di limitarsi a documentarne il costo: chi ci sbatte è chi sta
su storage di rete, dove fa più male.

---

## 6. Confronto con lo stato dell'arte

Harness: `scripts/perf/staging/xfer/xfer_sota.sh`. Workstation ↔ VM `t4g` in `eu-south-1`,
una esecuzione per strumento, 40 s di pausa fra uno e l'altro.

**Il confronto va letto onesto.** `scp`, `rsync` e `tar | ssh` aprono una connessione TCP
diretta verso un host che ha un indirizzo raggiungibile e un `sshd` acceso. `bore transfer`
esiste per il caso in cui non c'è né l'uno né l'altro — un ricevente dietro NAT, senza port
forwarding e senza endpoint pubblico — che nessuno dei tre sa fare. Quindi la domanda non è
"chi è più veloce a fare la stessa cosa", ma **quanto costa il protocollo di bore rispetto al
caso migliore**, sullo stesso link, con gli stessi byte, negli stessi minuti.

La riga `link` è quella che rende leggibile la tabella: un solo canale SSH che porta i byte
su `/dev/null`, senza filesystem dall'altra parte e senza nessun protocollo per-file. Ogni
altra riga è limitata da quella. Senza, sei numeri quasi uguali non si distinguono da sei
strumenti tutti ugualmente mediocri.

**Un file grande, 1024 MiB:**

| direzione | `link` | bore diretto | bore relay | scp | rsync | `tar｜ssh` |
| --- | --- | --- | --- | --- | --- | --- |
| ws → vm | 68,2 | **71,1** | *78,5* | 67,5 | 67,0 | 70,0 |
| vm → ws | 43,2 | **57,6** | 51,8 | 49,6 | 42,9 | 45,1 |

In salita tutti gli strumenti stanno entro il ±5 % del tetto: il vincolo è il link e bore non
paga una tassa di protocollo per arrivarci. In discesa **il braccio diretto di bore è lo
strumento più veloce misurato**, il 33 % sopra il tetto a canale singolo e il 16 % sopra
`scp`, perché `--parallel 8` fa otto stream QUIC dove uno strumento SSH ne fa uno, e uno
stream solo è limitato da `finestra / RTT`.

La riga in corsivo vuole un'avvertenza, non un applauso: il braccio **relay** non fa la stessa
strada delle altre. Va workstation → server di staging → VM, e il server è nella stessa
regione della VM, quindi parte di quel 78,5 è instradamento e non protocollo. Il comparatore
corretto è il braccio diretto, quello in grassetto.

**Gli stessi 512 MiB in 5000 file:**

| strumento | MB/s | tempo |
| --- | --- | --- |
| `link` | 65,8 | 7,78 s |
| bore diretto | 64,7 | 7,92 s |
| bore relay | 64,7 | 7,91 s |
| rsync | 64,4 | 7,95 s |
| `tar｜ssh` | 61,0 | 8,40 s |
| **scp** | **1,3** | **381,28 s** |

bore, `rsync` e una pipe `tar` grezza sono indistinguibili e stanno tutti sul tetto: dopo le
correzioni del §2 il costo per-file del trasferimento è sotto il filo, che è il risultato di
questa campagna ridetto su una rete vera.

`scp` è **48 volte più lento**, e il numero non è una banda: sono round trip. Da OpenSSH 9
`scp` viaggia sul protocollo SFTP e cammina l'albero un file alla volta su un canale solo,
quindi 5000 file costano 5000 round trip in serie su un percorso da ~10 ms. È in tabella
proprio perché è lo strumento a cui si pensa per primo.

**Il bias della tabella non è a favore di bore.** Gli strumenti girano in ordine fisso e la
fixture viene letta dalla page cache della workstation: la prima riga (`link`) la legge
fredda, tutte le successive calda — e le successive sono gli strumenti SSH. Il bias lavora
*contro* la conclusione, non a favore.

**Cosa manca.** `croc` e `rclone` non sono stati misurati, ed è il confronto che a questa
sezione manca davvero: `croc` è il parente più stretto di bore (relay, NAT-friendly,
autenticazione PAKE), quindi "bore batte `scp`" è un'affermazione molto più debole di quanto
sarebbe "bore sta alla pari con `croc`". È assente per un motivo pratico e poco lusinghiero:
`croc` non è installato qui, e confrontarlo in modo corretto richiede un relay self-hosted
raggiungibile da entrambi i peer, cioè aprire porte su un host che questa campagna ha scelto
di non riconfigurare. Chi ripete le misure lo aggiunga per primo: l'harness prende una lista
`TOOLS` e un braccio nuovo è un ramo `case`.

---

## 7. Gate aggiunti, e verifica in rosso

Ogni gate nuovo è stato verificato **rimuovendo la correzione** e controllando che fallisca:

| Gate | Cosa fissa | Verifica in rosso |
| --- | --- | --- |
| `transfer_resume_carries_completed_chunks_across_the_interruption` (e2e) | Il resume trasporta davvero i chunk completati | Giornale reso no-op → **rosso** |
| `replaying_a_journal_is_a_set_union_and_replaying_it_twice_changes_nothing` | Idempotenza della riproduzione | Rimosso il guard `!*slot` → **rosso** |
| `a_journal_torn_by_a_crash_mid_append_keeps_every_whole_record` | Coda troncata non fatale | EOF trattato come errore → **rosso** |
| `a_journal_record_outside_this_manifest_is_ignored_not_fatal` | Record estranei ignorati | — |
| `the_resume_index_answers_by_entry_id_not_by_position` | L'indice indicizza per id | Indice sostituito dalla posizione → **rosso** |
| `the_open_file_cache_evicts_the_least_recently_used_and_never_exceeds_its_cap` | Il limite dei descrittori | Limite disattivato → **rosso** |
| `transfer_resume_carries_completed_chunks_with_no_fsync` (e2e) | Con `--no-fsync` il resume continua a trasportare i chunk: il flag resta "niente fsync", non diventa "niente resume" | Giornale reso no-op quando `!durable` → **rosso** |
| `the_journal_record_format_does_not_depend_on_the_fsync_policy` | `durable` può decidere **solo** se si fa `fsync`, mai cosa si scrive | Record saltati quando `!durable` → **rosso** |
| `transfer_filesystem_no_fsync_listener_cli` (CLI) | Il flag arriva davvero da riga di comando a `ListenerOptions` | `--no-fsync` rimosso dal parser → **rosso** (clap rifiuta l'argomento) |

Nota importante su un gate che **esisteva già e non discriminava**: il test e2e di resume
storico verifica solo che i byte finali siano corretti, cosa che un ricevitore che non
ricorda nulla soddisfa comunque (rinvia tutto). Il gate nuovo limita la seconda esecuzione a
due chunk su un file da tre: passa **solo** se almeno un chunk è sopravvissuto
all'interruzione.

Regressione completa: **725 test passati, 0 falliti** (724 fra unit e integrazione più un
doctest); `clippy --all-targets -D warnings` senza avvisi; `cargo fmt --check` pulito.

---

## 8. Ciò che non è coperto da un gate

`X-5` (il `flush` prima di dichiarare durevole un chunk) è **ragionato, non testato**: per
osservarlo servirebbe perdere la macchina a metà scrittura. È documentato nel codice con il
motivo per cui l'ordine dev'essere quello e non un altro.

---

## 9. Questioni aperte

1. **Il default di `--no-fsync`.** Misurato: **2,82×** a 20 000 file su EBS, e
   **zero** su tmpfs (§3) — il costo è del dispositivo, non del codice. Il default resta
   severo; la decisione di invertirlo è dell'operatore.
2. **Il sender legge e hasha l'albero due volte.** `rchar` è 2× il payload a ogni
   dimensione: una passata per gli hash del manifest, una per inviare i chunk. Prezzo
   misurato: ~0,4 CPU s/GiB su ~4,0 CPU s/GiB totali del sender, cioè **~10 % della sua
   CPU**, su un percorso dove la CPU non è il vincolo. Non toccato di proposito: eliminarlo
   richiede o tenere in memoria un hash per chunk (O(chunk) di memoria aggiuntiva) o
   spostare l'hash del file in coda al protocollo (cambio di versione del protocollo).
3. **`destination_satisfies_manifest` è seriale.** Sul ri-lancio idempotente di un albero
   grande rilegge e ri-hasha tutto un file alla volta. Costo reale ma circoscritto al solo
   caso di ri-esecuzione.
4. **Perché su `ws → vm` il relay batte il percorso diretto** (§5). Misurato e riproducibile,
   meccanismo non stabilito: serve una scomposizione perdita/RTT sul socket UDP, non un'altra
   misura di banda. Nota che nella direzione opposta l'ordine si inverte, quindi non è una
   proprietà costante del trasporto e non c'è un difetto da "aggiustare" al buio.
5. **Il limite superiore di `--parallel` sopra i 32 core non è testato.** `resolve_parallel(0)`
   limita a `[4, 32]`; la riga a 32 stream del §4 è stata misurata su una macchina a 16 core,
   cioè in sovraccarico, che non è la configurazione che quel default produrrebbe. La
   costante non è stata toccata proprio perché i dati non parlano del caso che governa.
6. **`croc` e `rclone` non sono stati misurati** (§6). `croc` è il concorrente più vicino a
   bore — relay, NAT-friendly, autenticazione PAKE — e la sua assenza è la lacuna più grande
   del confronto con lo stato dell'arte.
