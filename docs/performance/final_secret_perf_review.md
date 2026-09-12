# Tunnel segreti — revisione di performance e traversal, 2026-09-11

> Documento finale della campagna sui **tunnel segreti** (`bore local
> --tcp-secret-id` + `bore proxy`), la sola forma di tunnel di bore il cui
> percorso veloce è **peer-to-peer**: il braccio relay è
> `consumer → server → provider`, il braccio diretto è una connessione QUIC
> bucata (`hole-punched`) `consumer ↔ provider` che il server si limita a
> intermediare.
>
> Le prove grezze stanno in
> [`SECRET_STAGING_EVIDENCE_2026-09-11.md`](SECRET_STAGING_EVIDENCE_2026-09-11.md);
> la meccanica del traversal sta in
> [`../nat/NAT_TRAVERSAL.md`](../nat/NAT_TRAVERSAL.md). Qui si tirano le somme:
> cosa è stato misurato, cosa era rotto, cosa è stato cambiato, e come si
> colloca bore rispetto allo stato dell'arte.

## 0. Come rifare tutto fra un mese

Tre comandi e un file di coordinate. Nessuno script in questa campagna contiene
un host, una porta o una credenziale: tutto arriva da `env.sh`
(`$BORE_PERF_ENV`, poi `~/.config/bore-perf/env.sh`, poi `./env.sh`; `chmod
600`, mai nel repository — il modello è `scripts/perf/staging/env.sh.example`).

```shell
# 1. il cancello locale delle risorse (nessun sudo, nessuna porta host)
scripts/perf/secret_leak_hunt.sh            # tutti i bracci

# 2. i cancelli di rete in netns (uno alla volta, MAI due insieme)
sudo -n /abs/path/scripts/udp_nat_netns_test.sh
sudo -n /abs/path/scripts/secret_netns_test.sh

# 3. la campagna su staging, per topologia
TOPO=vm-ws N=20       scripts/perf/staging/sec/sec_ttd.sh   | tee out/sec/s2-vm-ws.txt
TOPO=vm-ws PAIRS=5    scripts/perf/staging/sec/sec_ab.sh    | tee out/sec/s1-vm-ws.txt
TOPO=vm-ws GIB=8      scripts/perf/staging/sec/sec_eff.sh   | tee out/sec/s3-vm-ws.txt
TOPO=vm-ws PROBES=100 scripts/perf/staging/sec/sec_lat.sh   | tee out/sec/s4-vm-ws.txt
TOPO=vm-vm ACK=10 ACK_DELAY_MS=1 \
                      scripts/perf/staging/sec/sec_ack.sh   | tee out/sec/s5-vm-vm.txt
```

`sec_eff.sh` (S3) vuole i campionatori avviati prima e fermati dopo, ed è
l'unico stadio che li usa:

```shell
scripts/perf/staging/res/start_samplers.sh 3600 2
# ... gli stadi S3 ...
scripts/perf/staging/res/stop_samplers.sh
# poi, per ogni finestra stampata dallo stadio:
scripts/perf/staging/res/cpu_window.sh out/pres_<host>.stat <t0> <t1> <gib> out/pres_<host>.proc
```

`GIB=8` e non `GIB=2`: a 2 GiB i secondi CPU per processo hanno una
quantizzazione di ±1 s e una delle celle è finita dentro il rumore, cosa che è
costata una rigirata (§5.5). Lo stadio stampa anche i contatori di pacchetti
per host e per braccio (§5.6).

Per la banda di un **singolo** stream — la forma che il tunnel a 4 connessioni
non misura — l'oracolo è il diagnostico, non la campagna:

```shell
bore test-udp --tcp-secret-id <id> ...   # stampa rtt, cwnd, loss e le finestre vere
```

È così che è saltato fuori il difetto del §5.7.

`sec_ack.sh` (S5) richiede **entrambe** le variabili: una soglia senza
`ACK_DELAY_MS` viene rifiutata dal binario, e lo stadio si rifiuta di partire
contro un peer troppo vecchio per onorare la seconda (§5.4).

`TOPO` è la prima cosa da decidere perché cambia **cosa** si sta misurando:

| `TOPO` | provider | consumer | misura |
|---|---|---|---|
| `vm-ws` | VM di test | workstation | scaricamento da un provider in regione verso un NAT domestico — la forma d'uso comune |
| `ws-vm` | workstation | VM di test | il gemello asimmetrico: il NAT domestico sta dal lato che invia |
| `vm-vm` | VM di test | VM di test | CONTROLLO: nessuna WAN **fra i peer**. Attenzione: NON isola il costo CPU di QUIC — il braccio diretto è loopback mentre il braccio relay attraversa comunque due volte la WAN fino al broker (§3.2). Quello che isola è il TETTO del percorso diretto su quell'host, e un RTT di classe loopback: è per questo che è la topologia che smaschera S5 (§5.4) |

Tre regole di casa che questa campagna rispetta e che vanno rispettate anche
alla prossima esecuzione:

* **mai `pkill bore`.** Il deployment porta i tunnel veri dell'operatore. I
  processi locali si uccidono per PID, quelli remoti per `--tcp-secret-id <id>`,
  dove l'id è coniato per esecuzione e non esiste altrove sulla macchina;
* **mai due harness netns insieme** (condividono i nomi `ns0`/`ns1`/`ns2`);
* **niente compilazioni locali mentre un braccio di throughput gira.** La metà
  workstation di ogni misura compete con `cargo` per la CPU, e il braccio
  diretto — che cifra in user space — ne risente più del braccio relay, che
  è TCP nel kernel. È un errore commesso una volta in questa campagna e
  costato la ri-esecuzione di uno stadio.

## 1. Il difetto principale: il listener teneva il socket mentre il dialer stava già chiamando (S-5)

Questa è la scoperta che giustifica da sola la campagna, e non sarebbe emersa da
un test di correttezza: **il traversal funzionava sempre**. 20 tunnel su 20
diretti su tutte e tre le topologie, zero fallback, zero fallimenti. Ciò che
non funzionava era *quanto ci metteva*.

`direct_ready_ms`, letto dal log del consumer su 27 stabilimenti `vm-ws`:

```
37 40 41 42 42 43 43 44 44 48 49 49 50 52 52 52 52 53      <- 18 esecuzioni
1036 1043 1043 1044 1044 1045 1050 1052 1162               <-  9 esecuzioni
```

Bimodale, con niente in mezzo. Una distribuzione con un buco così non è mai la
rete: è un timer. Il timer è il PTO iniziale di quinn — `333 ms + 4 × 166 ms =
999 ms` con l'`initial_rtt` di default della RFC 9002 — e il pacchetto che lo
subisce è il **primo Initial QUIC**, perso perché dall'altra parte, in quel
momento, non c'è ancora un endpoint QUIC.

Perché non c'è: il listener consegna il socket a QUIC **solo dopo** il giro di
check, e il suo giro finiva a secco. Il dialer nomina in ~210 ms e *disabilita
il proprio responder* («i frame in ritardo si contano, non si rispondono»: è il
contratto del giro), mentre il listener — il cui piano adattivo sonda per primi
i candidati *locali* del peer e raggiunge il gruppo riflessivo 150 ms più tardi —
si ritrova a interrogare un indirizzo che ha già smesso di rispondere.

La precondizione è contabile nei log: **8 giri su 52** lato VM finivano
`nominated=None`, contro **0 su 22** lato workstation. Stessa asimmetria dello
stallo.

**La correzione**: un listener che ha appena risposto a una richiesta
autenticata chiude lì il proprio giro. Quel peer ha la chiave, è su questa
generazione, gioca il ruolo opposto e ci raggiunge: è tutto quello che la metà
listener del giro può dimostrare, e la risposta appena inviata è esattamente ciò
che farà partire la chiamata. Restare nel giro non migliora il percorso, tiene
solo il socket lontano da QUIC nel momento sbagliato.

Due dettagli portano il peso, ed entrambi sono documentati in
`NAT_TRAVERSAL.md` §21: la risposta va sul filo **prima** che il giro venga
smontato (finire il giro ferma l'attore che la spedirebbe), e `nominated` viene
valorizzato perché è ciò che disattiva l'escape spray della Fase 7 — spendere
sei secondi di spray per un peer che ci ha appena raggiunto sarebbe lo stesso
errore in formato più grande.

## 2. Gli altri tre interventi

| id | cosa | perché |
|---|---|---|
| **S-7** | `initial_rtt` del percorso diretto: 333 ms → **100 ms** (`BORE_DIRECT_QUIC_INITIAL_RTT_MS`, clamp [10, 333]) | un endpoint diretto di bore non è mai «freddo»: nasce solo dopo uno scambio di check autenticato con quel peer o una connessione TCP di controllo verso quell'host. Sottostimare costa **un** Initial duplicato; sovrastimare costa un PTO intero di silenzio proprio sul pacchetto con la probabilità di perdita più alta della connessione |
| **S-8** | cap del backoff di upgrade relay→diretto: 256 s → **60 s** | il cap *è* il tempo peggiore in cui un tunnel resta sul relay dopo che il percorso diretto è tornato possibile. Il VPN dello stesso codice riprova su una griglia **fissa** da 30 s, in produzione: un tunnel segreto che riprova a metà di quel ritmo è il più conservativo dei due, non un rischio nuovo |
| **S-9** | `pair_cache` limitata (sweep in `remember` + tetto 256 voci) | la scadenza girava solo dentro `recall` e solo per la chiave richiamata: una chiave mai più richiamata non veniva mai più esaminata. Una voce per id di tunnel, per tutta la vita del processo — piccola abbastanza da non farsi notare mai |

A questi si aggiungono i tre difetti trovati prima della campagna su staging e
già documentati nell'evidence: **S-2** (un upgrade relay→diretto vivo non veniva
mai riportato, quindi `current_path` mentiva per il resto della sessione),
**S-3** (l'escape spray abbandonava il socket al primo errore ICMP — difetto
visibile solo su Windows, per una proprietà del kernel) e **S-4** (il test
dell'escape spray sparava sugli altri test).

## 3. I numeri

Tutti i numeri di questo capitolo vengono dalla campagna su staging con il
binario **precedente** alle correzioni (`1bb3243a`): sono la base di confronto,
e sono anche il posto in cui due dei quattro difetti si vedono mentre fanno
danno invece che mentre esistono. Le tabelle grezze stanno in
[`SECRET_STAGING_EVIDENCE_2026-09-11.md`](SECRET_STAGING_EVIDENCE_2026-09-11.md)
§8.

### 3.1 Il traversal funziona: 60 tunnel su 60 diretti

20 tunnel indipendenti per topologia, ciascuno acceso, sollecitato con una
connessione, misurato e spento.

| topologia | diretti | relay | falliti | ttd mediano | min | max |
|---|---|---|---|---|---|---|
| `vm-ws` | 20 | 0 | 0 | 107 ms | 87 | **1150** |
| `ws-vm` | 20 | 0 | 0 | 104 ms | 83 | 110 |
| `vm-vm` | 20 | 0 | 0 | 96 ms | 82 | 452 |

Zero fallback, zero fallimenti, attraverso un NAT domestico, una VM in regione
e in entrambe le direzioni. **La correttezza del traversal non è mai stata in
discussione in questa campagna**, ed è esattamente per questo che serviva
misurare i tempi: un difetto che non fa fallire nulla non ha modo di emergere
da una suite di correttezza.

La colonna `max` è il difetto S-5 visto dall'esterno. Le distribuzioni intere:

```
ws-vm   83  83  85  94  96  97  97  97  98 104 104 104 105 106 106 106 106 107 108 110
vm-ws   87  88  91  95 102 103 104 104 106 107 107 107 112 788 790 792 800 804 1149 1150
vm-vm   82  82  83  85  88  93  93  94  94  96  96  98  98  98 105 106 110 200 205 452
```

`ws-vm` sta tutta in 27 ms. `vm-ws` ha gli stessi 13 campioni in quella banda e
poi **7 su 20 — il 35 % — a 788 ms o peggio**, senza niente in mezzo. Il
discriminante non è la direzione, non è il NAT e non è la tratta WAN: è **quale
host faceva da Listener** (cioè quale host ospitava il provider). Con la
workstation listener, 0 lenti su 20; con la VM listener, 7 su 20 in `vm-ws` e 3
su 20 in `vm-vm` — e `vm-vm` esclude la WAN, perché fra quei due peer non c'è
WAN.

### 3.2 Relay contro diretto: a decidere è l'host che INVIA

Mediana dei rapporti diretto/relay, 5 coppie per cella, 128 MiB su 4
connessioni per braccio, ordine alternato dentro la coppia.

| topologia | verso | host che INVIA | mediana diretto/relay |
|---|---|---|---|
| `vm-ws` | get | VM | **1.218** |
| `vm-ws` | put | workstation | 0.757 |
| `ws-vm` | get | workstation | 0.828 |
| `ws-vm` | put | VM | 1.167 |
| `vm-vm` | get | VM | 2.298 |
| `vm-vm` | put | VM | 1.456 |

Letta per direzione o per topologia, questa tabella sembra rumore. Letta per la
terza colonna, non lo è più:

```
mittente = VM             1.218   1.167   2.298   1.456    -> il diretto vince, sempre
mittente = workstation    0.757   0.828                    -> il diretto perde, sempre
```

**Il merito relativo del percorso diretto è una proprietà dell'host che INVIA**,
non della direzione né della topologia. La workstation invia QUIC più
lentamente di quanto invii TCP; la VM no. La spiegazione candidata è la CPU: il
percorso diretto cifra e paced-invia in user space, mentre il braccio relay è
TCP nel kernel con segmentation offload. I buffer socket sono esclusi come
spiegazione (`net.core.rmem_max`/`wmem_max` valgono 4 MiB su tutti e tre gli
host); la verifica è S3, che misura secondi di CPU per GiB consegnato — ed è
arrivata: **§5.3 la conferma**, con il mittente che paga circa 3.5× e il
ricevente che non paga quasi niente.

Una precisazione onesta sul controllo `vm-vm`: **non** isola il costo CPU di
QUIC, malgrado quanto diceva il commento dell'harness. Il suo braccio diretto è
loopback mentre il suo braccio relay attraversa comunque due volte la WAN fino
al broker, quindi 2.298 è un effetto di topologia. Quello che stabilisce è il
**tetto** del percorso diretto su quell'host: 387 MB/s.

### 3.3 Quanto costa ciascun trasporto — e la sola affermazione che conta

`vm-vm`, 2 GiB per braccio:

```
relay    204.03 MB/s  path=relay   relay_tx=2148532224  relay_rx=68
direct   407.83 MB/s  path=direct  relay_tx=0           relay_rx=0
```

`relay_tx` è il contatore di byte **del server** per quel tunnel. Sul braccio
relay vale 2 GiB più framing: ogni byte consegnato è passato dal broker. Sul
braccio diretto vale **zero**: il server ha intermediato la bucatura e poi non
ha portato niente.

È la differenza strutturale fra il tunnel segreto e ogni altro trasporto di
bore — nelle campagne vhost e public il braccio «diretto» terminava comunque
sul server, che pagava i byte in entrambi i casi — ed è detta in una forma
falsificabile invece che retorica: se il percorso diretto non fosse davvero
peer-to-peer, quel contatore non sarebbe zero.

### 3.4 Latenza di nuova connessione e scala

100 sonde per gradino, gradini a 0/16/64/256 connessioni tenute aperte, p50 in
millisecondi.

| topologia | trasporto | held=0 | 16 | 64 | 256 |
|---|---|---|---|---|---|
| `vm-ws` | relay | 26.99 | 26.71 | 26.87 | 26.80 |
| `vm-ws` | diretto | *(persa: S-5)* | 21.70 | 21.75 | 21.63 |
| `vm-vm` | relay | 3.33 | 3.43 | 3.46 | 3.31 |
| `vm-vm` | diretto | 0.745 | 0.767 | 0.805 | 0.776 |

Una connessione nuova costa **21.7 ms contro 26.9 ms** sulla tratta domestica e
**0.78 ms contro 3.4 ms** con entrambi i peer in regione — il transito in più
del relay è tutta la differenza. E **nessuno dei due trasporti degrada con la
concorrenza**: 256 connessioni tenute aperte spostano la p50 di meno di 0.2 ms
su ogni riga. È il comportamento che il semaforo `--max-conns` e il modello «una
connessione proxata = uno stream QUIC» devono dare, e qui — a differenza della
coda di concorrenza che la campagna vhost non ha mai chiuso (N-9) — lo danno.

## 4. Quello che ha rotto l'harness, non il prodotto

Quattro difetti dell'harness, tutti trovati eseguendolo e tutti della stessa
famiglia: **una misura sbagliata che ha la forma di una misura giusta**. Sono
documentati per esteso nell'evidence (§3.1-§3.3); qui contano perché chi
rifarà questi test fra un mese li incontrerebbe di nuovo.

| id | cosa | come si manifestava |
|---|---|---|
| **H-12** | il controllo «origine già avviata?» era un `pgrep` remoto, che combacia con la propria riga di comando | l'origine non partiva mai, S1 misurava **0.00 MB/s su entrambi i bracci** |
| **H-13** | un braccio fallito entrava nella mediana come rapporto 0 | `s1-vm-ws get` riportava 0.965 con due righe `startfail`; i tre bracci reali erano 0.965/1.247/1.218, mediana vera **1.218** |
| **H-14** | `GIB` non intero non fermava lo script | riga stampata con MB/s vuoto e finestra `t0 == t1`, cioè una divisione per zero a valle |
| **H-15** | lo stadio di efficienza apriva la finestra prima che il percorso esistesse | S-5 si è mangiato il braccio diretto di S3 `vm-ws` e il primo gradino di S4 `vm-ws` (`errs=100`) |

Lo zero di H-13 merita una riga in più, perché non è «rumore»: uno zero può
solo essere il minimo della lista, quindi l'errore è **orientato**. Ha
trasformato «il diretto vince del 22 %» in «il diretto perde del 3.5 %».

La regola che ne esce, e che i tre stadi ora applicano allo stesso modo: un
braccio entra in una mediana solo se ha misurato qualcosa, la riga esclusa
resta **stampata** con `-` al posto del rapporto, e lo stadio dichiara quante
coppie ha escluso. Un'esclusione invisibile è solo un secondo modo di
nascondere un fallimento.

## 5. Dopo le correzioni

Entrambi i peer ricostruiti e ridistribuiti (`bore 1.0.0 - main - 89ef490d`
sulla VM di test e sulla workstation), stesse tre topologie, stessi 20 tunnel
per topologia, stesso harness — con i difetti di riduzione del §4 già corretti,
quindi questi sono anche i primi numeri con una mediana onesta.

**Il SERVER non è stato toccato di proposito: gira ancora `1bb3243a`**, la
stessa build contro cui è girata la fase 1 (verificato da `server_version` su
`/admin/api/v1/config`, non dal log di deploy). Non è una dimenticanza e non è
una lacuna: tutte e quattro le correzioni vivono nei peer, e il percorso
diretto di un tunnel segreto va da consumer a provider con il server fuori
(S-1). Tenerlo fermo fa sì che fra fase 1 e fase 2 cambi **una** variabile
sola, che vale più di una stringa di versione ordinata.

### 5.1 S2 — il censimento dei percorsi

| topologia | listener | lenti (≥200 ms) prima | dopo | mediana/max prima | dopo |
|---|---|---|---|---|---|
| `vm-ws` | VM | **7/20** | **1/20** | 107 / 1150 ms | 96 / 466 ms |
| `vm-vm` | VM | **3/20** | **0/20** | 96 / 452 ms | 98 / 112 ms |
| `ws-vm` | workstation | 0/20 | 0/20 | 104 / 110 ms | 95 / 113 ms |

Con la VM come listener — la configurazione che portava tutto il difetto —
**10 tunnel su 40 erano lenti prima, 1 su 40 dopo.**

```
vm-ws prima   87  88  91  95 102 103 104 104 106 107 107 107 112 | 788 790 792 800 804 1149 1150
vm-ws dopo    85  85  87  90  90  90  92  93  95  95  96  96  97  97  99 100 109 109 110 | 466

vm-vm prima   82 … 110 (17 campioni) | 200 205 452
vm-vm dopo    85  88  90  95  95  95  95  96  98  98  98  98  99  99  99 100 101 103 108 112
```

I due gruppi che avevano dato il nome al difetto **non ci sono più**: niente a
~800 ms, niente a ~1150 ms. `vm-vm` — la topologia dove i due peer stanno sullo
stesso host e non c'è WAN da incolpare — passa da 3 valori anomali fino a
452 ms a 20 campioni dentro una banda di 27 ms.

Due cose contano quanto il miglioramento.

**Il controllo non si è mosso.** `ws-vm`, dove il listener è la workstation, era
già pulita (0/20 prima) ed è ancora pulita (0/20 dopo), mediana 104 → 95 ms. Se
la correzione avesse comprato il proprio miglioramento spostando una costante di
tempo globale, anche quella riga si sarebbe mossa. Non si è mossa: è questo che
rende «abbiamo corretto il giro del listener, non l'orologio» una misura e non
un'affermazione.

**Un valore anomalo sopravvive, e va detto invece che arrotondato via.** Il
censimento `vm-ws` ha ancora un campione a 466 ms. Non è il gruppo del PTO —
999 ms è la costante di quinn, e 466 non ne è né un multiplo né una frazione —
e una bucatura è una gara, quindi una coda è attesa. Quello che si può dire
onestamente è che il modo di fallire con un **meccanismo identificato** è
sparito, e ciò che resta è alla scala del passo di ritentativo
(`CHECK_TOTAL_CAP` vale 3 s e un passo di ritentativo raddoppia il ritmo).
Inseguirlo richiede un censimento più grande di 20 tunnel: non è dichiarato
risolto.

### 5.2 S1b — relay contro diretto, rifatta con il riduttore corretto

`vm-ws`, 5 coppie per verso, 128 MiB su 4 connessioni per braccio, ordine
alternato, 75 s di raffreddamento. È la cella che il difetto H-13 aveva
corrotto nella fase 1 (§3.2) ed è quella che S-5 aveva più probabilità di
rovinare: è quindi quella che valeva la pena rifare.

| verso | host che INVIA | fase 1 | fase 2 |
|---|---|---|---|
| `get` | VM | 1.218 (n=3, corretta a mano) | **1.337** (n=5 su 5) |
| `put` | workstation | 0.757 | **0.741** (n=5 su 5) |

```
get   1.019  1.092  1.337  1.372  1.355   mediana 1.337   n=5 su 5
put   0.741  0.730  0.778  0.715  0.759   mediana 0.741   n=5 su 5
```

Tre cose da portarsi via.

**Tutte le coppie hanno misurato.** `n=5 su 5` in entrambi i versi: nessun
`startfail`, nessun ripiego sul relay, niente escluso. Nella fase 1 questa
cella produceva tre bracci utilizzabili su cinque e lo stadio stampava comunque
una mediana. È la correzione del riduttore (§4) a rendere `n=` visibile, ed è
la differenza fra un numero e un numero difendibile.

**Ogni braccio diretto era sul percorso diretto prima che si aprisse la sua
finestra**, con tempo-al-diretto fra 82 e 110 ms su tutti e dieci i bracci. È
la correzione H-15 che fa il suo lavoro.

**La regola dell'host che invia sopravvive alla ripetizione.** La VM invia sul
`get` e il diretto vince (1.337); la workstation invia sul `put` e il diretto
perde (0.741). I due numeri si sono mossi di +0.12 e −0.016 e nessuno dei due
ha attraversato 1.0. Il §5.3 è la misura del **perché**.

### 5.3 S3b — il conto della CPU, per processo: il diretto addebita all'endpoint invece che al relay

`vm-ws`, 2 GiB per braccio su 4 connessioni, campionatori attivi su tutti e tre
gli host. Verso `get`, quindi **la VM di test è il mittente** (lato provider) e
**la workstation è il ricevente** (lato consumer).

```
relay    48.55 MB/s  path=relay   relay_tx=2148532224  finestra 1789170488-1789170530
direct   55.03 MB/s  path=direct  relay_tx=0           finestra 1789170609-1789170646
```

Secondi di CPU del processo `bore` stesso, differenziati sulla finestra
(`ps -eo cputimes`: l'unità è il secondo intero, quindi la quantizzazione è
±1 s):

| processo `bore` | ruolo nel braccio | relay | diretto | per GiB consegnato |
|---|---|---|---|---|
| server | broker del relay / della bucatura | **11 s** | **0 s** | 5.5 → **0** |
| VM di test | provider, **invia** 2 GiB | 2 s | **7 s** | 1.0 → **3.5** |
| workstation | consumer, riceve 2 GiB | 3 s | 4 s | 1.5 → 2.0 |
| *somma dei tre* | | 16 s | 11 s | 8.0 → **5.5** |

**Il conto del server va a zero, e ci va due volte.** Il processo ha bruciato
11 secondi di CPU per rilanciare 2 GiB e **0** mentre gli stessi 2 GiB andavano
diretti; la sua RSS si è mossa di +4 KiB su tutta la finestra diretta. In modo
indipendente, `relay_tx` vale 2 148 532 224 sul braccio relay e **0** su quello
diretto. Due strumenti scorrelati — la contabilità CPU del kernel per quel pid
e il contatore di byte del server per quel tunnel — concordano nel dire che il
broker non ha portato niente. È l'affermazione che il §3.3 faceva su `vm-vm`, e
ora regge anche sulla topologia con NAT domestico.

**Chi invia paga un sovrapprezzo di circa 3.5× per QUIC; chi riceve non paga
quasi niente.** 2 s → 7 s sull'host mittente sono 5 secondi di differenza, ben
fuori dalla quantizzazione di ±1 s; 3 s → 4 s sull'host ricevente ci stanno
dentro e non viene dichiarato come variazione. Il meccanismo plausibile è il
segmentation offload: il mittente del braccio relay parla TCP, dove kernel e
scheda segmentano 2 GiB per lui, mentre il mittente del braccio diretto cifra e
impagina ogni pacchetto in user space. Nominare un meccanismo non è misurarlo:
chiuderlo richiede i contatori di segmenti (`nstat`, `ethtool -S`) sui due
bracci, e non è stato fatto.

**Questa è la spiegazione del §3.2.** Il merito relativo del percorso diretto è
legato all'host che **invia** perché è l'host che invia a pagare il
sovrapprezzo di QUIC. Un host con CPU da spendere invia più in fretta in
diretto (si risparmia un transito WAN); un host già stretto perde, perché fa
3.5× il lavoro per byte. Il sistema nel suo insieme guadagna comunque — 16
secondi di CPU diventano 11 — ma i 5 secondi risparmiati sono del server e i 5
aggiunti sono del mittente. Per chi gestisce un server è tutto qui: **il
percorso diretto addebita all'endpoint invece che al relay.** È il motivo per
cui conviene a chi ospita il broker anche quando non conviene al singolo peer.

#### La trappola che ci è quasi costata la conclusione sbagliata (H-17)

La riduzione a livello di **host** per le stesse due finestre dice:

```
relay   srv busy=13.24 (6.62 s/GiB)   vm busy=4.63 (2.31)   ws busy=41.07 (20.54)
direct  srv busy= 0.85 (0.42 s/GiB)   vm busy=9.34 (4.67)   ws busy=76.89 (38.45)
```

Server e VM di test sono istanze dedicate e le loro cifre di host concordano
con quelle di processo (srv 13.24 contro 11 s + 4.48 s di softirq; vm 9.34
contro 7+1 s): per quei due l'host è il numero **migliore**, perché include il
softirq che il processo non vede.

**Le cifre di host della workstation sono invece rumore e non vanno citate.**
`busy` passa da 41.07 a 76.89 (+35.8 s di CPU, `user` +30.0) mentre tutti i
processi che il campionatore guarda ne spiegano 1 secondo. La workstation è un
desktop condiviso con una trentina di altri processi; la regex del campionatore
(`bore|dufs|curl|oha|python3`) non li vede e `/proc/stat` li conta tutti. Prese
alla lettera, quelle due righe dicono «il percorso diretto ha quasi raddoppiato
il conto CPU della workstation», che è una conclusione su un browser e non su
bore — ed è la conclusione che avevo tratto prima di ridurre i campioni per
processo.

Regola per la ripetizione: **su host dedicato si cita `busy`, su host condiviso
si cita il delta per processo e lo si dice.** `cpu_window.sh` ora stampa
entrambi e avvisa quando il conto dell'host supera di più di 4× quello dei
processi campionati — la forma che dice «su questa macchina stava lavorando
qualcos'altro». Verificato in rosso: l'avviso scatta sulle due righe della
workstation e tace sulle quattro degli host dedicati, compresa `srv relay` che
è la più vicina alla soglia (13.24 contro 11 s, cioè 1.2×).

### 5.4 S5 — diradare gli ACK: la trappola era la manopola, non l'idea

L'estensione QUIC ACK Frequency permette di chiedere al peer di confermare al
massimo una volta ogni `soglia + 1` pacchetti ack-eliciting. Su un percorso
diretto saturo toglierebbe quasi tutto il traffico di ritorno, e i due estremi
di un percorso diretto di bore sono entrambi bore, quindi l'estensione è sempre
negoziabile. `BORE_DIRECT_QUIC_ACK_THRESHOLD` esisteva esattamente perché la
decisione si potesse misurare invece che discutere.

Bracci appaiati, entrambi `--udp`, unica differenza la variabile d'ambiente;
128 MiB su 4 connessioni, 5 coppie, ordine alternato, soglia 10, topologia
`vm-vm`:

```
get   0.539  0.964  0.622  0.020  1.035    mediana 0.622   n=5 su 5
put   0.908  0.088  0.910  0.908  0.918    mediana 0.908   n=5 su 5
```

**Il verdetto è scarto, e la ragione è la FORMA.** Il `put` è la vista più
pulita: quattro coppie su cinque stanno fra 0.908 e 0.918, cioè una perdita
riproducibile di circa il 9 %, e la quinta crolla a 0.088 — da 268.32 a
23.49 MB/s. Il `get` racconta la stessa storia con più dispersione: due coppie
alla pari (0.964, 1.035) e una a 0.020, da 393.86 a **7.91 MB/s**. Diradare gli
ACK non rende il percorso un po' più lento: lo rende **bimodale**, di solito
qualche punto percentuale peggio, ogni tanto 10–50 volte peggio.

**Il meccanismo, ed è il motivo per cui questa misura ha misurato la cosa
sbagliata.** `AckFrequencyConfig` ha tre campi e la manopola ne impostava uno.
`max_ack_delay` restava a `None`, che quinn documenta come «viene usato il
`max_ack_delay` originale del peer, preso dai suoi transport parameter»: **25
ms**. Il ricevente manda quindi un ACK quando ha raccolto 11 pacchetti *oppure*
quando sono passati 25 ms, quello che viene prima.

Su `vm-vm` l'RTT è di classe loopback, ben sotto il millisecondo: un ricevente
che non ha ancora raccolto 11 pacchetti si siede sull'ACK per **centinaia di
round trip**. Che una connessione finisca in quello stato dipende da quanto le
sue fasi tengono più di 11 pacchetti in volo — le fasi limitate dalla finestra
di congestione, la coda di una raffica e l'inizio di uno stream no — ed è
esattamente la bimodalità vista sopra. È anche il motivo per cui la sola
mediana l'avrebbe sottostimata.

Quindi questa misura non ha misurato ACK più radi: **ha misurato un ritardo di
ACK da 25 ms.** È un difetto della manopola, non dell'idea, ed è corretto nel
binario e non nell'harness: `resolve_ack_frequency` restituisce ora tre stati,
e una soglia fornita senza `BORE_DIRECT_QUIC_ACK_MAX_DELAY_MS` è
`ThresholdWithoutDelay` — rifiutata, con un `warn!` che nomina i 25 ms,
lasciando intatta la politica di quinn. Con entrambe le variabili non
impostate il comportamento resta identico byte a byte a ogni release
precedente alla manopola.

`sec_ack.sh` ora imposta entrambe le metà (`ACK_DELAY_MS`, default 1 ms, lo
stesso ordine di grandezza dell'RTT del percorso diretto) e **si rifiuta di
partire** se il binario di uno dei due peer non contiene la variabile del
ritardo, letta dal binario stesso e non da una stringa di versione. Senza quel
preflight un peer vecchio ignorerebbe la seconda variabile, installerebbe la
soglia da sola, e lo stadio stamperebbe una tabella completa della trappola
sotto il titolo di un esperimento sugli ACK: la stessa forma di fallimento di
H-13, H-15 e H-16, per la quarta volta.

**Perché la tratta `vm-ws` è un esperimento diverso e non una ripetizione.**
Con i 25 ms capiti, la divisione fra le due topologie è più netta di quanto
fosse stata progettata: **lo stesso limite fisso di 25 ms è un multiplo diverso
dell'RTT su ciascuna tratta.** Misurato dalla workstation verso la VM di test,
handshake TCP, 7 campioni: `min 18.09 ms, mediana 22.31 ms, max 23.37 ms`.

```
vm-vm    limite 25 ms ≈ diverse centinaia di RTT   -> la trappola, misurata sopra
vm-ws    limite 25 ms ≈ 1.1 RTT                    -> un limite di ritardo legittimo
```

La tratta `vm-ws`, girata con la stessa identica variabile, finisce quindi
vicina all'esperimento per cui la manopola completata esiste: ACK più radi con
un limite di ritardo dello stesso ordine dell'RTT. Non è esattamente quello —
i 25 ms non li ha scelti nessuno, sono quello che il peer ha annunciato — e
viene riportata per quello che è invece che promossa a quello che avrebbe fatto
comodo.

#### La tratta `vm-ws`, e cosa dimostrano le due tratte insieme

Stessa variabile, stessa soglia 10, stessi 5 bracci appaiati:

```
get   0.979  1.011  0.909  1.064  1.144    mediana 1.011   n=5 su 5
put   1.029  0.986  1.088  1.015  1.045    mediana 1.029   n=5 su 5
```

| tratta | limite 25 ms, in RTT | mediana `get` | mediana `put` | coppia peggiore |
|---|---|---|---|---|
| `vm-vm` | diverse centinaia | 0.622 | 0.908 | **0.020** |
| `vm-ws` | ≈ 1.1 | **1.011** | **1.029** | **0.909** |

**Sulla tratta dove il limite di ritardo è dell'ordine dell'RTT, diradare gli
ACK non costa niente — e non è crollata nemmeno una coppia.** Il peggiore dei
dieci bracci `vm-ws` è 0.909; il peggiore dei dieci `vm-vm` è 0.020. La
bimodalità non è una proprietà del diradare gli ACK: è una proprietà di un
limite di ritardo che vale centinaia di RTT, e sparisce esattamente dove
sparisce quel rapporto.

È un risultato più forte di ciascuna delle due tratte da sola, e vale la pena
essere precisi su cosa autorizza e cosa no:

* **Non autorizza ad accendere la manopola di default.** Un default deve essere
  sicuro su ogni percorso che un utente ha, e 25 ms *fissi* non lo sono:
  innocui a 22 ms di RTT, catastrofici a 0.05 ms. Stessa costante, stesso
  codice, esito opposto — che è la definizione di un valore che non può essere
  un default.
* **Non mostra un guadagno.** Il `put` legge 1.029 con quattro coppie su cinque
  sopra 1.0, il che suggerisce qualche punto percentuale, ma la dispersione
  (0.986–1.088) copre 1.0 e n vale 5. Questo sostiene «nessun danno
  misurabile», non «un guadagno».
* **Riabilita l'idea.** L'estensione non è smentita. È smentito il chiedere una
  soglia lasciando che il limite di ritardo prenda il default, che è l'unica
  cosa che la manopola sapesse esprimere prima di questa campagna.

L'esperimento che resta aperto è quindi stretto e ben definito: una soglia con
un limite di ritardo scelto **dall'RTT del percorso** invece che ereditato da un
transport parameter. Adesso `sec_ack.sh` lo sa girare (`ACK_DELAY_MS`), il
binario lo sa esprimere, e il preflight impedisce di girarlo per sbaglio contro
un peer che ricadrebbe in silenzio nella trappola. Qui non è stato fatto.

### 5.5 S3c — il conto della CPU a 8 GiB, che corregge il §5.3

Il §5.3 girava 2 GiB per braccio. `ps -eo cputimes` conta secondi interi, quindi
a quella taglia i delta per processo erano a una cifra e il `3 s → 4 s` del
ricevente stava *dentro* la quantizzazione: il §5.3 lo dichiarava e si
asteneva. S3c rifà lo stesso stadio con `GIB=8`, che porta i bracci `vm-ws` a
176 s (relay) e 131 s (diretto) e mette ogni delta nelle decine di secondi.

`vm-ws`, 8 GiB per braccio su 4 connessioni, soli secondi CPU del processo
`bore`:

| host | ruolo su questa tratta | relay | diretto | rapporto |
|---|---|---|---|---|
| srv | salto di relay | **49** | **< 1** | — (è tutto il punto) |
| vm | provider = mittente | 11 | 31 | 2.82× |
| ws | consumer = ricevente | 13 | 22 | **1.69×** |
| **totale** | | **73** | **53** | 0.73× |
| goodput | | 46.33 MB/s | 62.54 MB/s | 1.35× |

Sulla riga del server in diretto il riduttore non stampa nemmeno la sezione
`process:`: ogni delta campionato di `bore` è ≤ 0 su 131 s, cioè sotto il tick
di un secondo del campionatore. La lettura onesta è «sotto il secondo», non
«esattamente zero», e in ogni caso la conclusione è la stessa — **il percorso
diretto addebita agli endpoint invece che al relay**, e glielo addebita *meno
in totale* (73 → 53 secondi CPU per gli stessi 8 GiB) consegnando il 35 % di
goodput in più.

**Questo corregge il §5.3.** A 2 GiB il ricevente leggeva 3 s → 4 s ed era
riportato come «piatto, dentro il rumore». A 8 GiB legge 13 s → 22 s, cioè
nove secondi contro una quantizzazione di ±1 s. Il ricevente **non** è piatto:
paga 1.69×. Il 2.82× del mittente sopravvive al cambio di risoluzione (2 → 7 a
2 GiB, 11 → 31 a 8 GiB: 3.5× e 2.82×, stesso ordine, e va citato il campione
più grande). La *direzione* della conclusione del §5.3 non cambia; cambia che
una delle sue due celle «non dichiarate» ora è dichiarata, e con il segno
opposto a quello comodo.

`vm-vm`, stesso stadio, entrambi gli endpoint sulla VM — quindi la riga `vm`
porta mittente *e* ricevente:

| host | relay | diretto |
|---|---|---|
| srv | **70** | **1** |
| vm (entrambi gli estremi) | 25 | 26 |
| **totale** | **95** | **27** |
| goodput | 209.54 MB/s | 431.07 MB/s |

È l'enunciato più pulito di S-1 di tutta la campagna: mettere i due endpoint
sullo stesso host e togliere il server dal percorso costa agli endpoint **un**
secondo di CPU (25 → 26) e ne fa risparmiare **sessantanove** al server,
raddoppiando il goodput. Un tunnel segreto diretto non è «più economico per il
server perché i byte sono un problema di qualcun altro»: su questa tratta i
byte sono letteralmente un problema dello stesso processo in entrambi i bracci,
e il totale scende lo stesso di 3.5×.

**H-17 sul campo.** Il cancello di attribuzione del riduttore è scattato esatto
sulle due righe `ws` (`busy=81.11` contro 19 secondi CPU campionati;
`busy=73.70` contro 29) ed è rimasto zitto sulle quattro righe di host
dedicati, inclusa `srv direct` dove `busy` vale 3.06 e il totale campionato è
0. È il comportamento per cui la regola era stata riscritta nel §5.3, ora
confermato su una misura contro cui non era stata tarata.

Una riga sul costo del generatore di carico, perché non venga scambiato per
costo del tunnel: `python3` su `vm-ws` legge 3 s / 3 s (vm) e 6 s / 7 s (ws),
cioè invariante. Su `vm-vm` legge 15 s (relay) contro 7 s (diretto) per gli
stessi 8 GiB, e la spiegazione più probabile è la velocità di consegna — a
431 MB/s ogni `recv()` restituisce più byte che a 209 MB/s, quindi lo stesso
trasferimento costa meno syscall. È una lettura, non una misura.

### 5.6 I contatori di pacchetti — e l'unico buco che questa campagna non chiude

`sec_eff.sh` ora delimita ogni finestra anche con i contatori di driver di
`ip -s link`, oltre a `/proc/net/snmp`. Serve perché i due contatori del kernel
**non sono confrontabili fra trasporti**: `Tcp: OutSegs` viene incrementato per
segmento vero (`tcp_skb_pcount()`), `Udp: OutDatagrams` una volta per
`sendmsg` — e con GSO attivo una `sendmsg` sono dieci pacchetti. Solo i
contatori di driver contano i pacchetti che vanno davvero sul filo. Per GiB
consegnato:

| tratta | braccio | NIC tx mittente / GiB | NIC rx ricevente / GiB | mittente ÷ ricevente |
|---|---|---|---|---|
| `vm-ws` | relay | 755 235 | 749 109 | **1.008** |
| `vm-ws` | diretto | 1 136 178 | 922 985 | **1.231** |

Due fatti: uno chiuso e uno no.

**Chiuso: il percorso diretto mette sul filo del mittente 1.50× i pacchetti del
relay per GiB consegnato.** 755 235 → 1 136 178. Il meccanismo è la *taglia*
del pacchetto, non la ritrasmissione: su questo percorso il TCP usa segmenti da
1 448 byte, il rapporto di quinn sulla stessa coppia di host dice `mtu 1.42
KiB, max datagram 1.38 KiB`, e in più il mittente emette sulla propria scheda
anche i pacchetti di ACK QUIC della direzione inversa. **Questa è la misura che
il §7 punto 4 chiedeva**, ed è la risposta: il sovrapprezzo di CPU del mittente
non è *solo* crittografia in user space, è anche metà pacchetti in più da
costruire, cifrare e consegnare allo stack.

**Non chiuso: il 19 % dei pacchetti che il mittente trasmette nel braccio
diretto non compare nel contatore del ricevente.** Le liquidazioni facili sono
state verificate e non reggono:

* *«Le finestre non sono confrontabili fra host.»* Lo sono. Il braccio
  **relay**, stessi due host, stesse finestre, stesso riduttore, concorda allo
  **0.8 %**. Una metodologia che concorda allo 0.8 % su un braccio e sbaglia
  del 19 % sull'altro non è ciò che produce il 19 %.
* *«La workstation è condivisa, i suoi contatori sono sporchi.»* Il traffico
  estraneo *gonfia* la rx della ws, cioè muove il buco nella direzione
  sbagliata.
* *«La VM è strozzata.»* I contatori di allowance ENA
  (`bw_out_allowance_exceeded` e fratelli) sono tutti **zero** sulla finestra.

La lettura più coerente con il resto della campagna è perdita sul link di
accesso del **ricevente**: il braccio diretto consegna 62.54 MB/s ≈ 500 Mbit/s
a una linea residenziale trasmettendo ~600 Mbit/s di pacchetti, quinn gira
**BBR** (`holepunch.rs`, `BbrConfig::default()`), e BBR è tollerante alla
perdita per progetto — che è esattamente come il braccio può perdere un quinto
dei pacchetti e battere lo stesso il relay del 35 %. Se è così è una
caratterizzazione, non un difetto, ed è il throttling del relay a essere
superato.

**Non è confermato**, e il motivo per cui non si può confermare da questa
campagna è concreto e risolvibile: `bore test-udp` stampa le statistiche di
percorso di quinn (`loss N pkts / N B`, `sent N pkts`), un tunnel vero no. Una
esecuzione separata di `test-udp` sulla stessa coppia, a ritmo più basso e su
singolo stream, ha riportato `loss 0 pkts / 0 B` — coerente sia con «la perdita
compare solo quando si supera il link di accesso» sia con «non c'è perdita e il
buco nei contatori è altro». Chiuderlo richiede che il tunnel esponga gli
stessi contatori che il diagnostico già calcola. È l'unico esperimento che
questa sezione lascia deliberatamente sul tavolo.

### 5.7 Il difetto del *window floor*: un default che strozza in silenzio ogni singolo stream diretto

Questo difetto è saltato fuori inseguendo il buco del §5.6, lanciando
`bore test-udp --tcp-secret-id …` sulla stessa coppia di host per avere i
contatori di perdita di quinn. I contatori di perdita erano puliti. Il resto
del rapporto no.

```
UDP direct path : sent 1.86 GiB in 42.46s (376.79 Mbit/s)
UDP direct path QUIC   : rtt 19.13 ms, cwnd 10.31 MiB, loss 0 pkts / 0 B
UDP direct path tuning : stream recv 1.00 MiB (default 16.00 MiB),
                         conn recv 16.00 MiB (default 256.00 MiB)
TCP relay fallback : sent 1.86 GiB in 24.34s (657.25 Mbit/s)
```

Tre righe decidono:

1. **Il diretto è più lento del relay** — 376.79 contro 657.25 Mbit/s — cioè
   l'opposto di quello che ogni braccio di tunnel dei §3 e §5 ha misurato sulla
   stessa coppia.
2. **`loss 0 pkts / 0 B`.** Non è congestione, non è la rete.
3. **`cwnd 10.31 MiB` contro `stream recv 1.00 MiB`.** Il controllo di
   congestione ha aperto dieci volte la finestra che al flusso è *permesso*
   usare. Un flusso con cwnd un ordine di grandezza sopra la propria finestra
   di ricezione è limitato dal controllo di flusso, per definizione.

`1 MiB / 19.13 ms` = 54.8 MB/s = **438 Mbit/s**, e la misura è 376.79 — l'86 %
del limite, dove il resto è il giro di boa che il mittente passa ad aspettare
un aggiornamento di finestra che il ricevente può mandare solo dopo aver
drenato. Il numero non è vicino al limite per caso: **è** il limite.

**Il meccanismo.** `UdpDirectTuning::from_memory_budget` (`src/shared.rs`,
F-13) divide il budget dell'operatore per `max_carriers` e poi clampa:

```rust
let raw = budget / carriers;
let mut conn = raw.clamp(floor, ceiling);   // floor 16 MiB, ceiling 256 MiB
```

Il divisore è `--max-carriers`: il **tetto assoluto** ai carrier che un tunnel
può aprire, non il numero che un tunnel apre davvero. Il default di fabbrica è
**16**; staging gira a **1024**, impostato per la concorrenza. Quindi su
staging `raw = 512 MiB / 1024` = 512 KiB, molto sotto il pavimento di 16 MiB,
a scegliere la finestra è il clamp, e siccome il rapporto 16:1 è preservato per
costruzione (DEC-VE8, giustamente) la finestra di stream diventa 16 MiB / 16 =
**1 MiB**: un sedicesimo del default collaudato.

Il pavimento è il caso più acuto, non tutto il caso. Con il `--max-carriers 16`
di fabbrica un budget da 512 MiB dà `raw` = 32 MiB, che il pavimento non lo
tocca, e produce comunque una finestra di stream da **2 MiB**: un ottavo del
default. Per arrivare al default servono `16 × 256 MiB` = **4 GiB** di budget.
L'enunciato generale è semplicemente `stream = budget / max_carriers / 16`,
clampato in [1 MiB, 16 MiB] — e **un budget compra concorrenza a spese della
banda di singolo stream**, che è un compromesso ingegneristico legittimo, non
un bug. Il bug è che non lo diceva nessuno.

L'avviso di avvio del server di staging pubblica la conseguenza da sempre —
`udp_stream_receive_window = 1MiB`, `udp_connection_receive_window = 16MiB`,
`udp_direct_slots = 32` per `--udp-memory-budget 512MB` — ma come riga `info!`
di numeri. L'unico `warn!` riguardava gli **slot**. Nessuno nominava la
conseguenza sulla finestra, ed è quella a strozzare la banda.

**Perché i bracci di tunnel non lo mostravano.** Perché il tetto è **per
stream**, e ogni stadio della campagna gira quattro connessioni concorrenti =
quattro stream bidirezionali:

| tratta | diretto misurato | per stream | RTT | `1 MiB / RTT` per stream |
|---|---|---|---|---|
| `vm-ws` | 62.54 MB/s | 15.6 MB/s | ≈ 22 ms | 47.7 MB/s — non vincolante |
| `vm-vm` | 431.07 MB/s | 107.8 MB/s | ≈ 0.05 ms | ~20 GB/s — non vincolante |
| `test-udp` | **47.1 MB/s** | **47.1 MB/s** | 19.13 ms | **54.8 MB/s — vincolante** |

Uno stream solo su una tratta WAN è esattamente la forma che ci sbatte contro:
per questo l'ha trovato il diagnostico e non la campagna. Ed è anche la forma
che ha un utente vero: un `scp`, un download HTTP grande, il restore di un
database.

**Cosa è stato messo in campo.** `UdpBudgetPlan` guadagna
`window_at_floor: bool`, alzato quando `budget / carriers` stava *sotto* il
pavimento e non semplicemente clampato da esso — la distinzione conta, perché
atterrare sul pavimento dall'alto è il comportamento progettato, mentre
atterrarci dal basso significa che a scegliere la finestra è stato il divisore,
non il budget. `report_udp_budget` (`src/main.rs`) avvisa ogni volta che la
finestra di stream derivata è **sotto il default collaudato** — non solo sul
pavimento, proprio perché il `--max-carriers 16` di fabbrica produce una
finestra da un ottavo senza mai toccarlo — e usa `window_at_floor` solo per
nominare la causa. Dichiara la conseguenza nell'unità che serve
(`stream_bandwidth_mb_s`, funzione pura: `finestra × 1000 / (rtt_ms × 1 MiB)`)
a due RTT di riferimento e accanto ai valori del default, dice esplicitamente
che ogni stream concorrente ha la propria finestra — quindi il limite riguarda
un trasferimento grande, non l'aggregato del tunnel — e dà tre rimedi:
abbassare `--max-carriers`, alzare il budget a `carrier × 256 MiB`, oppure
impostare esplicitamente i tre flag di finestra (che vanno in conflitto con il
flag di budget per progetto, quindi l'operatore è costretto a scegliere). Due
test unitari coprono entrambe le metà:
`a_budget_divided_by_the_carrier_cap_decides_the_stream_window` (che pin-na
anche il caso di fabbrica 512 MiB / 16, quello che obbliga la condizione a
essere «sotto il default» e non «sul pavimento») e
`a_receive_window_bounds_one_flow_at_window_over_rtt`.

Il default **non** è cambiato. Abbassare `--max-carriers` cambia un limite
esposto all'operatore; alzare il pavimento cambia il comportamento di memoria
di ogni installazione esistente; sono decisioni di chi conosce il proprio host,
e il difetto era che il server non diceva che c'era una decisione da prendere.
Adesso la dice. Per staging la scelta giusta è probabilmente la prima:
`--max-carriers 1024` è un tetto che nessun tunnel reale avvicina, e pagarlo in
banda di singolo stream su ogni tunnel è il peggiore dei due errori.

### 5.8 H-18 — il cancello anti-leak misurava sotto il proprio rumore

L'ultima esecuzione dei cancelli della campagna è fallita:

```
FAIL: T-SECLEAK-CHURN-RELAY/rss-consumer: RSS rose in every phase after the
      first, 4152 KiB in total (slack 2048, 200 connections per phase)
```

È esattamente la forma che il cancello esiste per catturare: quattro fasi,
tutte in salita, 6.9 KiB per connessione, sopra i 3.5 KiB/connessione che il
commento del cancello dichiara di risolvere. Preso per buono è un leak per
connessione nel percorso relay del consumer, e si è ripetuto su una seconda
esecuzione.

Non lo è. Lo stesso braccio con `SECLEAK_PHASES=8 SECLEAK_CONNS=400` — 3 200
connessioni invece di 800, stesso binario, stessa macchina:

```
p1 21776  p2 20208  p3 24228  p4 23332
p5 24008  p6 23964  p7 23836  p8 23964
```

**Si appiattisce.** Le ultime quattro fasi concordano entro 200 KiB su altre
1 600 connessioni, e il trend sulle sette fasi dopo il warm-up è 2 188 KiB,
cioè 0.8 KiB per connessione. Un leak è lineare nelle connessioni: un leak non
si appiattisce. Nella stessa esecuzione lunga è poi stato il **server** a
fallire l'altra metà della regola (`+2172 KiB nell'ultima fase`) partendo da un
livello fermo da cinque fasi. Due processi, due esecuzioni, due falsi allarmi.

**Che cos'è il rumore.** Il relay alloca un `proxy_buffer_size` — 256 KiB di
default — per direzione e per connessione. A quella taglia glibc serve
l'allocazione con `mmap` e la `free` la restituisce al sistema, quindi l'RSS
non dovrebbe crescere affatto. Solo che glibc **alza la propria soglia dinamica
di mmap** dopo aver visto liberare qualche blocco del genere, e da lì in poi la
stessa allocazione viene dallo heap, dove la `free` non restituisce nulla.
L'RSS quindi sale finché l'arena copre la concorrenza di picco, e poi si ferma.
Il braccio di churn gira ondate da dieci connessioni concorrenti, quindi quel
tetto è `10 × 256 KiB × 2 direzioni = 5120 KiB`: la scala esatta di ogni deriva
e di ogni oscillazione misurata sopra. Il vecchio limite era 2 048 KiB, cioè
**sotto il rumore che lo strumento stesso produce**.

**La correzione, che è più sensibile e non meno.** `SECLEAK_PHASES` passa a 8
(quattro fasi non distinguono «sale» da «sale verso un plateau», ed è tutta lì
la domanda); `RSS_PHASE_SLACK` passa a 5120 KiB *derivati* da
`concorrenza × buffer × 2` invece che scelti — un numero che non cresce con le
connessioni, mentre il leak sì, quindi più connessioni separano sempre i due
casi; e ogni verdetto positivo stampa la **risoluzione raggiunta** in byte per
connessione (3.2 KiB a 8 × 200, 1.6 KiB a 8 × 400). Un leak sotto quella soglia
è dichiarato sotto la risoluzione dello strumento, mai assente.

**La regola ha ora un proprio red-check**: `scripts/perf/rss_verdict_check.sh`
estrae `rss_verdict` dall'harness a tempo di esecuzione (così non può divergere
dal codice che controlla) e lo prova contro serie di cui si conosce la risposta
giusta: i due plateau misurati qui, che devono passare; leak lineari sopra la
risoluzione dichiarata, che devono fallire comunque cada il jitter; leak sotto,
che devono passare *ed essere letti come sotto-risoluzione*; e oscillazioni a
media nulla da 3 MiB, che devono passare. Undici casi, tutti verdi — e le due
righe misurate sono esattamente quelle che la regola precedente sbagliava,
quindi il check è rosso contro il codice che sostituisce.

È il quarto difetto d'harness che la campagna trova facendo girare i propri
cancelli, e il secondo in cui a essere rotto era lo strumento e non il prodotto.
La lezione è quella che H-16 aveva già scritto, qui con un meccanismo diverso:
**un cancello che non sa dichiarare la propria risoluzione non è una misura.**

## 6. Confronto con lo stato dell'arte

Il confronto va fatto per **meccanismo**, non per slogan, perché è l'unico modo
per dire cosa manca davvero.

| meccanismo | bore | Tailscale | frp (`xtcp`) |
|---|---|---|---|
| scoperta candidati STUN | sì, catena di 2+ server con budget | sì, STUN in ogni regione DERP | sì, un server STUN |
| classificazione NAT (EIM/EDM, port-preserving) | sì, profilo strutturato sulle offerte | sì (netcheck) | no |
| piano adattivo calcolato dal broker | sì, e solo quando entrambi i profili esistono | sì, lato client | no |
| check di connettività **autenticati** (HMAC) | sì, richiesta==risposta, mai rispondere a non autenticati | sì (DISCO) | no |
| candidati peer-reflexive appresi | sì | sì | no |
| birthday paradox per NAT simmetrici | sì (Fase 7) | sì | no |
| port mapping gestito (PCP/UPnP) con rinnovo | sì (`--upnp`, lease PCP RFC 6887 con rilevamento del riavvio del gateway) | sì | no |
| relay sempre caldo, fallback per connessione | sì | sì (DERP) | fallback a `stcp` con timeout |
| upgrade relay→diretto su sessione viva | sì, backoff 2…60 s | sì, continuo | no |
| cache della coppia vincente | sì, TTL 120 s, invalidata al primo fallimento | sì | no |
| **IPv6** | **no** | sì | no |

Due letture di questa tabella.

La prima: rispetto a frp — il termine di paragone più citato nel mondo dei
tunnel self-hosted — non c'è partita, e non per una questione di qualità di
implementazione ma di meccanismi presenti. frp fa STUN e buca; se il NAT è
simmetrico, ripiega. bore classifica il NAT, ordina le sonde di conseguenza,
impara i candidati peer-reflexive, e sul caso duro tira il birthday paradox.

La seconda, più utile: rispetto a Tailscale il divario residuo è **uno solo**,
ed è IPv6. Tailscale dichiara ~94% di connessioni dirette, e una parte
sostanziale di quel numero non viene dal bucare NAT: viene dal non doverlo fare,
perché i due peer hanno indirizzi IPv6 globali e si parlano direttamente. Ogni
altro meccanismo della colonna è presente in bore, in alcuni casi con
raffinatezze che Tailscale non documenta (il jitter deterministico legato al
ruolo, che rompe il lockstep del conntrack sui router che mascherano — misurato
in pcap, non ipotizzato).

**Il gap IPv6 resta aperto e va dichiarato tale.** È una feature, non
un'ottimizzazione: tocca il modello dei candidati (che oggi sanitizza e scarta
gli indirizzi non-IPv4), il wire delle offerte, la classificazione del profilo e
il dual-stack del socket di punch. Non è stato fatto in questa campagna e non
andrebbe fatto di corsa: è il prossimo pezzo di lavoro, con il suo piano.


## 7. Cosa resta aperto

Elenco esplicito di ciò che **non** è stato chiuso, con che cosa servirebbe per
chiuderlo. Sta qui perché fra un mese la domanda giusta non è «cosa abbiamo
fatto» ma «cosa non abbiamo fatto e perché».

**1. IPv6 — l'unico divario vero verso Tailscale.** È una feature, non
un'ottimizzazione: tocca il modello dei candidati (che oggi sanitizza e scarta
gli indirizzi non-IPv4), il wire delle offerte, la classificazione del profilo
NAT e il dual-stack del socket di punch. Una parte sostanziale del ~94 % di
connessioni dirette dichiarato da Tailscale non viene dal bucare meglio i NAT:
viene dal non doverlo fare. Va pianificato a parte; non va fatto di corsa.

**2. Il valore anomalo a 466 ms nel censimento `vm-ws`.** Uno su venti, dopo le
correzioni. Non è il gruppo del PTO (999 ms è la costante di quinn, 466 non ne
è né multiplo né frazione), ed è alla scala del passo di ritentativo
(`CHECK_TOTAL_CAP` 3 s, il ritentativo raddoppia il ritmo). Serve un censimento
più grande di 20 tunnel per sapere se è una coda o un secondo meccanismo: con
1 su 20 non si distinguono le due cose. Non è dichiarato risolto.

**3. L'esperimento ACK che NON è stato fatto.** Il §5.4 ha misurato la
trappola — soglia senza limite di ritardo, quindi i 25 ms del transport
parameter del peer — e l'ha scartata come default. Le due tratte insieme dicono
però qualcosa di più preciso: dove quei 25 ms valgono ≈1.1 RTT (`vm-ws`) il
diradamento non costa nulla e non fa crollare nessuna coppia; dove ne valgono
centinaia (`vm-vm`) crolla fino a 50 volte. Quello che resta da provare è una
soglia con un `max_ack_delay` esplicito e *scelto dall'RTT del percorso*.
Adesso si può condurre correttamente (`ACK=10 ACK_DELAY_MS=1 sec_ack.sh`) e il
preflight impedisce di rifarlo per sbaglio contro un binario vecchio. Su una
tratta ad alto BDP potrebbe essere neutro o utile; oggi non lo sappiamo, e il
`put` su `vm-ws` (mediana 1.029, quattro coppie su cinque sopra 1.0) è
esattamente il tipo di segnale che merita un campione più grande e non una
conclusione.

**4. CHIUSO — il meccanismo del sovrapprezzo di chi invia.** Il §5.6 ha
portato i contatori di driver che questo punto chiedeva: il mittente mette sul
filo **1.50× i pacchetti** del relay per GiB consegnato (755 235 →
1 136 178/GiB), perché i datagrammi QUIC valgono 1.38 KiB contro i 1 448 byte
dei segmenti TCP e perché il mittente emette anche i propri ACK QUIC. Quindi
non è *solo* crittografia in user space. Il rapporto misurato al §5.5 a 8 GiB
è 2.82×, non 3.5× (quello veniva dal campione a 2 GiB, più rumoroso).

**5. CHIUSO — la risoluzione del campionatore di CPU.** S3c ha rigirato lo
stadio con `GIB=8` (§5.5). Il ricevente **non** era piatto: 13 s → 22 s, cioè
1.69×, contro il `3 s → 4 s` che a 2 GiB stava dentro la quantizzazione. È il
tipo di correzione che vale la rigirata: la cella non dichiarata è risultata
diversa da zero e nel verso meno comodo.

**6. APERTO — il 19 % di pacchetti che il ricevente non vede (§5.6).** Sul
braccio diretto il mittente trasmette 1 136 178 pacchetti/GiB e il ricevente ne
conta 922 985; sul braccio relay gli stessi due contatori concordano allo
0.8 %, quindi non è la metodologia. La lettura più coerente è perdita sul link
di accesso residenziale con BBR che la tollera (e infatti il diretto vince lo
stesso del 35 %), ma **non è confermata**. Per chiuderlo serve che il tunnel
esponga le statistiche di percorso di quinn che `bore test-udp` già stampa
(`loss N pkts`, `sent N pkts`): oggi il diagnostico le ha e il tunnel no. È una
modifica piccola e ben delimitata, non è stata fatta qui.

**7. DECISIONE LASCIATA APERTA — il default del *window floor* (§5.7).**
L'avviso c'è, il difetto è nominato e testato, ma `--max-carriers 1024` e il
pavimento a 16 MiB restano come sono. Cambiare il divisore o il pavimento
cambia il comportamento di memoria di ogni installazione esistente: è una
decisione dell'operatore, e quello che mancava era che il server non diceva che
una decisione ci fosse. Se si volesse chiuderla nel prodotto, la strada più
difendibile è derivare `max_carriers` effettivo dal numero di tunnel attesi
invece che dal massimo ammesso.

**8. La coda di concorrenza della campagna vhost (N-9) resta fuori scope.** Qui
non si è manifestata — S4 mostra p50 piatte a 256 connessioni tenute su
entrambi i trasporti — ma questo non la chiude per il vhost: è una topologia
diversa con il server sul percorso dati. Vedere il §12 dell'evidenza vhost.
