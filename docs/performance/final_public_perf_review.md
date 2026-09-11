# Revisione finale delle prestazioni dei tunnel pubblici (`bore local`)

**Data:** 11 settembre 2026
**Oggetto:** campagna di misura e di correzione sulla componente *tunnel
pubblici* di `bore`, condotta contro il deployment reale su AWS, con la
macchina virtuale nella stessa regione del server e con la workstation
domestica, su TCP e su UDP, con il binario nativo, con il binario in Docker e
attraverso il gateway SSH.

---

## Come leggere questo documento

È pensato per essere letto **anche da chi non è tecnico**. Ogni prova segue
sempre lo stesso schema:

1. **Che cosa abbiamo provato** — in parole semplici.
2. **Che cosa è successo** — i numeri.
3. **Dov'è il collo di bottiglia** — qual è la cosa che ha impedito di andare
   più veloce, e se dipende da `bore` oppure no.

Il punto 3 è il più importante: **un numero senza il suo limite non dice
nulla.** "200 MB/s" può essere un ottimo risultato o un disastro a seconda di
che cosa avrebbe potuto fare il collegamento.

> **Se hai poco tempo:** la sintesi di una pagina è subito qui sotto, e i
> **comandi pronti da copiare** sono in fondo, nel **Ricettario**. Tutto il
> resto è la dimostrazione.

Il documento gemello in inglese,
[`PUBLIC_STAGING_EVIDENCE_2026-09-11.md`](PUBLIC_STAGING_EVIDENCE_2026-09-11.md),
contiene le trascrizioni integrali di ogni singola prova: qui si riportano i
risultati e il loro significato, lì si trova la prova che i numeri sono veri.

### Quattro parole da sapere prima di cominciare

| termine | in parole semplici |
| --- | --- |
| **tunnel pubblico** | `bore local 8080` chiede al server una **porta pubblica** e inoltra tutto quello che arriva lì verso la porta 8080 del computer locale. Trasporta TCP qualunque, non solo pagine web. |
| **relay (TCP)** | il percorso normale: i dati passano dal server centrale su una connessione TCP. È la modalità predefinita. |
| **direct (QUIC/UDP)** | un percorso alternativo in cui il server parla al client via UDP con il protocollo QUIC. Si attiva con `--udp`. Per i tunnel pubblici **non** c'è nessun "buco nel NAT" da aprire: il server è pubblico ed è il client a chiamarlo. |
| **carrier** | una delle connessioni parallele che il tunnel apre verso il server. `--carriers 4` significa quattro "corsie" invece di una. |

### Il vocabolario dei limiti

Useremo sempre gli stessi tre nomi per i colli di bottiglia:

* **Limite del collegamento** — la rete fisica è satura. Non si può fare di
  meglio senza cambiare linea. *È il risultato migliore possibile.*
* **Limite dell'istanza (AWS)** — il server è una macchina piccola e
  "burstable": Amazon le assegna un credito di banda che si esaurisce.
  Non dipende dal software.
* **Limite dell'applicazione** — il collo di bottiglia è nel programma.
  È l'unico tipo su cui si può lavorare scrivendo codice.

---

## Sintesi in una pagina

**Che cosa è stato fatto.** Una campagna di misura e correzione sui **tunnel
pubblici** di `bore` (`bore local`), condotta contro il server reale su AWS
con una macchina di prova nella stessa regione e con la workstation
domestica; su TCP e su UDP; con il binario nativo, con il binario in Docker e
attraverso il gateway SSH. Dieci fasi di misura, eseguite una alla volta
perché condividono lo stesso server e lo stesso credito di banda.

**I risultati in cinque righe.**

1. **Tredici difetti del prodotto** sono emersi dalle misure e sono stati
   corretti, tre di gravità alta: il server **non leggeva** il canale di
   controllo dei tunnel pubblici (quindi un percorso diretto caduto non
   tornava mai più); il rimedio introdotto per un altro difetto poteva a sua
   volta **bloccare l'intero client** contro un server più vecchio; e il
   socket UDP condiviso del server — quello che riceve il traffico diretto di
   *tutti* i tunnel — girava con il buffer di default del kernel, **208 KiB**,
   in silenzio, contro gli 8 MiB del client all'altro capo della stessa
   connessione (§8.1). Ogni correzione ha un test che fallisce se la si
   annulla.
2. **Su rete pulita il percorso normale (TCP) è più veloce** del percorso UDP
   diretto: 1,51× in scaricamento e 1,34× in caricamento, con dieci coppie su
   dieci concordi. Tutti quei confronti però sono stati misurati contro un
   server che aveva ancora il difetto del punto 1, e quel socket è il lato
   *ricevente* di uno scaricamento. **La misura è stata rifatta dopo la
   correzione** (§8.1): il gonfiore in entrata del percorso diretto è passato
   da 1,78× a 1,009×, cioè è sparito, e la sua velocità è salita da 111 a
   143,43 MB/s (+29 %) mentre quella del relay è rimasta dov'era. La
   classifica **non** cambia — su rete pulita in regione il percorso normale
   resta davanti — ma il margine è **0,71 e non 0,52**: circa due quinti del
   divario erano il difetto. I numeri 1,51× e 1,34× vanno quindi letti come
   quelli *prima* della correzione. I risultati su rete che perde pacchetti,
   quelli di latenza e tutto l'argomento sulla robustezza non erano toccati in
   nessun caso, perché nessuno di quelli dipende dalla banda.
3. **Su rete degradata il verdetto si rovescia**, e non di poco: con il 10 %
   di pacchetti persi il percorso normale consegna 0,39 MB/s — cioè si ferma —
   mentre il diretto ne consegna 51,40. `--udp` non è una manopola della
   velocità: **è una manopola della robustezza**.
4. **Le corsie parallele funzionano, fino a un punto**: il massimo è a 4
   corsie in scaricamento (1,44× rispetto a una) e a 2 in caricamento; a 8 si
   peggiora in entrambe le direzioni.
5. **Binario, Docker e SSH**: nativo e Docker sono indistinguibili; il gateway
   SSH costa circa 0,75 ms per connessione nuova e circa 1,9× di banda in
   scaricamento, ed è l'unico modo per aprire un tunnel senza installare
   niente.

**Che cosa fare, in pratica.** Lasciare `--udp` **spento** e usare
`--carriers 4` se il servizio regge più connessioni insieme, `--carriers 1`
(predefinito) se fa un trasferimento grande per volta; accendere `--udp` solo
se la rete fra il forwarder e il server **perde pacchetti** o se si tengono
aperte moltissime connessioni insieme. I comandi pronti sono nel Ricettario.

**Quanto ci si può fidare.** Ogni confronto è a coppie con ordine alternato,
ogni misura dichiara quale percorso ha davvero usato (letto dal server, non
supposto), e tutte le fasi sono ripetibili con gli script inclusi nel
repository. Durante la campagna sono stati trovati e corretti anche **quattordici
difetti degli strumenti di misura**: due di essi misuravano zero e lo
stampavano nella stessa forma di una misura vera, ed è la ragione per cui i
risultati di quelle due fasi sono stati rifatti da capo — e per cui la fase
dalla workstation ora si rifiuta di misurare un tunnel che non muove byte.


---

## 1. I problemi trovati e risolti

Questa campagna non è nata come una campagna di correzione: è nata come una
campagna di misura. Tredici difetti sono emersi **dalle misure stesse**, non da
una lettura del codice, ed è una distinzione che conta: ognuno di essi si
manifestava su un server vero, con un client vero, in condizioni che un test
in memoria non sa riprodurre.

| id | gravità | che cosa era rotto | stato |
| --- | --- | --- | --- |
| P-13 | ALTA | il socket UDP condiviso del server — quello che riceve il traffico diretto di **tutti** i tunnel, e che è il lato ricevente di ogni scaricamento — girava col buffer di default del kernel (208 KiB) e in silenzio, contro gli 8 MiB del client all'altro capo | risolto, con controprova |
| P-7 | ALTA | il server non leggeva **mai** il canale di controllo di un tunnel pubblico | risolto, con collaudo |
| P-9 | ALTA | un battito cardiaco che il server non legge blocca l'intero client | risolto, con controprova |
| P-12 | ALTA | il limite di connessioni era **più alto** del numero di file che il processo poteva aprire: il kernel rifiutava per primo, e rifiutava **tutto**, compresa la pagina di amministrazione | risolto, con controprova |
| P-4 | MEDIA | un client bloccato tratteneva la porta pubblica fino al riavvio del server | risolto, con tripla controprova |
| P-1 | MEDIA | l'apertura del canale diretto non aveva scadenza | risolto, con collaudo |
| P-2 | MEDIA | nessuna visibilità sul percorso effettivamente usato | risolto, con collaudo |
| P-3 | MEDIA | un gruppo di connessioni dirette incompleto poteva restare tale per sempre | risolto, con collaudo |
| P-6 | MEDIA | `--vhost-quic-port` veniva ignorato su un server senza vhost | risolto, con collaudo |
| P-5 | BASSA | `--carriers` oltre 32 veniva ridotto in silenzio | risolto |
| P-10 | BASSA | ogni tunnel normale dichiarava percorso "sconosciuto" | risolto, con controprova |
| P-11 | BASSA | la pagina di configurazione pubblicava un valore **vivo** al posto di quello impostato | risolto, con controprova |
| P-8 | — | la finestra di perdita durante un blackout UDP **è** il timeout di inattività: misurato, nessun difetto | non applicabile |

A questi si aggiungono **quattordici difetti degli strumenti di misura** (H-1 …
H-14): nove di essi buttavano via dati, si rifiutavano di partire, misuravano
un programma diverso da quello nell'albero dei sorgenti, pubblicavano la somma
di due gradini di una scala come se fosse un gradino solo, oppure — il caso
peggiore — **misuravano zero stampandolo nella stessa forma di una misura
vera**. Quest'ultimo caso si è presentato **due volte** (H-7 e H-12): per
questo la fase dalla workstation ora esegue un controllo preliminare che
**rifiuta di misurare** un tunnel registrato che non muove byte, invece di
affidarsi all'attenzione di chi la lancia. L'ultimo della serie (H-13) è stato
trovato durante le misure "dopo" di questa stessa campagna: lo strumento che
rilancia una fase aspettava **all'infinito** una fase già finita, perché il
comando che contava i processi sulla VM restituiva due righe invece di una e il
confronto numerico diventava un errore di sintassi — che in bash è "falso", non
"errore". Una riesecuzione è rimasta ferma **quattro ore** senza produrre
niente, stampando ogni trenta secondi la stessa riga di avanzamento, cioè
esattamente quello che sembra una fase lunga e sana. Ora quell'attesa ha una
scadenza, e un conteggio che non è un numero viene segnalato invece di essere
scambiato per uno zero. Il quattordicesimo (H-14) non è uno strumento di
misura ma un **test di regressione**, ed è finito in questo elenco per la
stessa ragione: ha mentito. La CI lo ha visto fallire su macOS con il
messaggio `raw log should have content: ` — un messaggio **vuoto** — su una
modifica che non toccava nemmeno una riga di codice Rust. La causa era nel
test stesso: la funzione che aspetta il registro degli accessi si accontentava
che il file **esistesse**, e un file appena creato e ancora vuoto si legge
benissimo. Il server crea il file e ci scrive la riga subito dopo, quindi esiste
una finestra reale in cui il file c'è ed è vuoto; su una macchina che capitava
dentro quella finestra il test leggeva stringa vuota e la riportava come "il
server non ha scritto niente". Ora la funzione aspetta il **contenuto**, e
distingue "il file non è mai stato creato" da "creato ma rimasto vuoto" —
sono due guasti diversi e confonderli manda chi indaga nel posto sbagliato. La
correzione ha il suo collaudo alla rovescia: rimettendo il codice di prima, il
nuovo test fallisce in zero secondi con esattamente il sintomo visto in CI.
Sono elencati con la stessa serietà dei difetti del prodotto nel
documento inglese (§17), perché uno strumento che mente è indistinguibile da
un server che si comporta male. Uno di essi (H-9) è anche la ragione per cui
P-12 è venuto a galla: uno strumento che teneva aperte più connessioni di
quante dichiarasse ha spinto il server oltre un limite che nessuno aveva
messo d'accordo con gli altri.

Quello che segue spiega i quattro più importanti in parole semplici. Gli
altri, con le trascrizioni, sono nel documento inglese.

### 1.1 Il server non ascoltava (P-7)

**Che cosa era rotto.** Un tunnel pubblico apre verso il server un canale di
controllo, cioè una linea di servizio separata dai dati, sulla quale il client
può dire cose come "il percorso diretto è caduto, rinnovamelo". Il server, per
i tunnel pubblici, **non leggeva quella linea**. Non la leggeva mai: non c'era
proprio il pezzo di programma che la ascolta. Gli altri tre tipi di tunnel di
`bore` (vhost, segreti, jump host SSH) la ascoltavano già.

**Che cosa significava in pratica.** Se il percorso UDP diretto cadeva — un
firewall che si chiude, una rete mobile che cambia — il client se ne accorgeva,
chiedeva il rinnovo, e la richiesta non veniva letta da nessuno. Il tunnel
continuava a funzionare sul percorso normale, quindi l'utente non vedeva un
errore: vedeva solo che il percorso veloce non tornava **mai più**, per tutta
la vita del tunnel. Nella prova sul campo era ancora degradato dopo 100
secondi, e lo sarebbe rimasto indefinitamente.

**Come è stato risolto e come lo sappiamo.** È stato aggiunto al server il ramo
che legge il canale di controllo, con la stessa forma usata dagli altri tre
tipi di tunnel. La prova è `T-PUB-RECOVER`: si spegne il traffico UDP con una
regola del firewall, si verifica che il tunnel ripieghi sul percorso normale, si
toglie la regola e si misura quanto tempo passa prima che il percorso diretto
torni. Prima: mai. Dopo: **4 secondi**.

### 1.2 La porta pubblica che non si liberava (P-4)

**Che cosa era rotto.** Un portatile che viene chiuso, un processo congelato:
il computer resta raggiungibile a livello di rete — il suo sistema operativo
continua a rispondere ai pacchetti — ma il programma non risponde più. Per il
server questo caso è indistinguibile da un tunnel sano e inattivo: scrivere sul
canale funziona (i dati finiscono in un buffer), leggere non ritorna mai. Il
tunnel restava registrato e **la porta pubblica restava occupata fino al
riavvio del server**.

**Perché non bastava aspettare.** Un `kill -9` sul client ha sempre liberato la
porta subito, perché è il sistema operativo a chiudere la connessione. Il caso
problematico è esattamente quello in cui il sistema operativo *non* chiude
nulla.

**Come è stato risolto.** Il client manda ora un battito cardiaco ogni 20
secondi; il server considera morto un tunnel che non batte da 60 secondi e lo
rimuove. La parte delicata è la compatibilità: un client vecchio non sa
battere, e ucciderlo dopo 60 secondi di silenzio sarebbe stato molto peggio del
problema originale. Per questo il battito è **dichiarato**: solo un client che
dice "io batto" viene sorvegliato. Chi non lo dice non viene mai rimosso.

**Come lo sappiamo.** Tre collaudi, non uno: (1) un client bloccato con
`SIGSTOP` viene rimosso e la porta torna libera; (2) un client vecchio che non
dichiara il battito non viene **mai** rimosso; (3) un client vero e sano
sopravvive oltre la scadenza. Sul campo: prima la porta 9089 era ancora
occupata dopo 150 secondi; dopo, la porta 9060 è stata liberata in 60 secondi.

### 1.3 Il battito che bloccava il client (P-9)

Questo difetto è stato **introdotto dalla correzione precedente**, trovato
mentre la si verificava, e corretto prima di chiudere la campagna. Vale la pena
raccontarlo perché è il tipo di problema che si vede solo mettendo insieme
versioni diverse.

**Che cosa era rotto.** Il client manda il battito su una linea di controllo
che ha un credito di scrittura limitato (256 KiB). Se dall'altra parte c'è un
server **vecchio**, che quella linea non la legge mai (P-7), il credito si
esaurisce. A quel punto la scrittura del battito non è "lenta": si ferma per
sempre. E siccome quella scrittura sta dentro il ciclo principale del client,
si ferma **tutto il client**: il tunnel resta registrato sul server ma smette
di servire le connessioni. Un tunnel che c'è ma non funziona.

**Come è stato risolto.** Il battito ha ora una scadenza propria (10 secondi,
regolabile). Se scade, il client **rinuncia al battito per il resto della
sessione**, scrive un avviso esplicito che spiega che il server è vecchio e va
aggiornato, e continua a servire il traffico normalmente.

**Come lo sappiamo.** Il collaudo automatico è costruito in modo che
*togliendo* la scadenza il test non fallisca: si **blocchi**. Ed è anche stato
misurato sul campo, contro il vecchio server: 8 richieste su 8 servite, un solo
avviso di rinuncia, e la connessione successiva accettata 126 microsecondi
dopo.

**La conseguenza operativa**, che è la cosa che interessa a chi gestisce il
servizio: **si può aggiornare in qualunque ordine**. Client nuovo su server
vecchio funziona (rinuncia al battito e avvisa); client vecchio su server nuovo
funziona (non dichiara il battito, non viene mai rimosso).

### 1.4 Il limite che non era un limite (P-12)

**Che cosa era rotto.** Il server ha un tetto al numero di connessioni che
serve contemporaneamente (`--max-conns`, sul nostro server **1024**). Il tetto
serve a comportarsi bene sotto carico: arrivata la millesimoventicinquesima
connessione, il server ne rifiuta **una**, lo scrive in un contatore
(`conn_rejections`) e continua a servire tutte le altre.

Il sistema operativo ha però un *secondo* tetto, che non c'entra con `bore`:
il numero massimo di "file aperti" che il processo può avere. Ogni connessione
servita ne occupa uno. Sul server questo secondo tetto era anch'esso **1024**,
cioè **esattamente uguale** al primo — e siccome il processo usa qualche file
anche per le cose sue (le porte in ascolto, i certificati, i registri), il
tetto del sistema operativo arrivava **sempre prima**.

**Perché è grave.** Quando è `bore` a rifiutare, rifiuta *una connessione*.
Quando è il sistema operativo a rifiutare, l'errore arriva sulla funzione che
accetta le connessioni, e quella funzione è **una sola per tutto il server**:
si fermano i tunnel pubblici, i sottodomini vhost, e anche la pagina di
amministrazione. Un singolo tunnel, con abbastanza connessioni aperte, mette
in ginocchio tutto il resto.

**Come è stato trovato.** Non ragionando, ma misurando. La scala di
concorrenza (§7) è arrivata a tenere aperte circa **976** connessioni su un
solo tunnel, e il registro del server ha cominciato a dire, dieci volte al
secondo:

```
WARN failed to accept tunnel connection
     err=No file descriptors available (os error 24)
```

Per circa mezzo minuto il server **non ha risposto più a niente** — le letture
della pagina di amministrazione fatte dalla prova stessa sono fallite con
"connessione azzerata" e poi con "impossibile collegarsi". E il contatore
`conn_rejections`, quello del rifiuto gentile, è rimasto a **zero**: la
protezione progettata per questo caso non è mai entrata in funzione, perché
non poteva.

**Che cosa è stato corretto.** Un processo può **alzarsi da solo** il proprio
tetto di file aperti, fino al massimo consentito dall'amministratore (sul
nostro server: 524 288). Adesso `bore server`, prima di aprire la prima porta
in ascolto, lo fa: porta il tetto a `--max-conns + 256` e lo scrive nel
registro. Se il massimo consentito è a sua volta troppo basso, alza fin dove
può e **avvisa**, dicendo esattamente che cosa fare (`ulimits: nofile:` in
Docker Compose, `LimitNOFILE=` in systemd, oppure abbassare `--max-conns`). Se
il tetto è già abbastanza alto, non tocca niente e **non dice niente** — un
server che si lamenta di un problema che non esiste insegna a ignorare
l'avviso che conta.

**Un difetto della correzione stessa, trovato dalla CI.** Il tipo con cui il
sistema operativo esprime questo tetto non ha la stessa dimensione su tutte le
architetture: 64 bit quasi ovunque, ma **32** su alcune varianti a 32 bit di
ARM, che questo progetto compila. La prima versione della correzione dava per
scontata la dimensione e ha rotto la compilazione su *una* delle dodici
architetture, mentre tutte le prove su questa macchina restavano verdi. La
decisione resta su un tipo unico e la conversione avviene ora al confine con
la chiamata di sistema, **saturando** e mai troncando: un troncamento
abbasserebbe il tetto in silenzio, che è esattamente il guasto che questa
correzione esiste per impedire. C'è una prova unitaria che lo fissa.

---

---

## 2. Come sono state fatte le misure (e perché ci si può fidare)

Questa sezione è qui, prima dei risultati, per un motivo preciso: **i numeri di
una campagna fatta male sono peggio di nessun numero**, perché sembrano una
risposta. La campagna precedente su `vhost` ha imparato a sue spese quali sono
le trappole; questa le eredita tutte, e le fa rispettare dagli script invece
che dalla buona volontà.

### 2.1 Le macchine

| macchina | che cos'è | ruolo |
| --- | --- | --- |
| **server** | AWS, 2 CPU, 903 MiB di memoria, immagine `ghcr.io/manprint/bore:main` | il server `bore` vero, quello in esercizio |
| **VM di prova** | AWS, 2 CPU, 3,8 GiB, **stessa regione del server** | il client del tunnel e il generatore di carico |
| **workstation** | il computer di casa, fuori da AWS, su linea domestica | la prova "utente reale" |

La VM nella stessa regione **non è un lusso, è un obbligo**: la campagna
precedente ha misurato che il collegamento della workstation si ferma intorno
ai 45 MB/s a causa della radio, qualunque cosa faccia il tunnel. Misurare la
banda da casa significa misurare la casa. La workstation serve quindi per la
**latenza**, per i **rapporti fra trasporti** e per dimostrare che il percorso
funziona davvero dall'esterno — mai per la banda massima.

Tutte e tre le macchine erano sulla **stessa identica versione** del programma
(`1.0.0 - main - dbcc645a`), e la versione è stampata dagli script all'inizio
di ogni esecuzione. Questa campagna ha aggiunto al server la possibilità di
dichiarare la propria versione via API (`server_version`): una campagna che non
sa dire contro quale versione è stata presa non è una prova.

### 2.2 Le cinque regole

**Regola 1 — il credito di banda di Amazon è il disturbo più grosso di tutti.**
Un solo scaricamento da 10 secondi su 4 connessioni consuma praticamente tutto
il credito di burst in ingresso di questa istanza. Conseguenze:

* dopo ogni misura si aspettano **75 secondi**;
* i confronti sono **appaiati**: i due trasporti si misurano uno dopo l'altro e
  si riporta il **rapporto**, invertendo l'ordine a ogni coppia. Il motivo è
  che nella campagna precedente il "gruppo di controllo" è derivato del 29 % in
  pochi minuti, cioè più della maggior parte degli effetti da misurare;
* il confronto fra le tre modalità di esecuzione registra **tutti e tre i
  forwarder insieme** e li colpisce a rotazione, perché una scala sequenziale
  addebita tutto il credito a chi corre per primo (nella campagna precedente
  proprio questo fece crollare un gradino a 9,90 MB/s);
* i contatori del credito vengono campionati per tutta la durata della
  campagna, e ogni prova marca la propria finestra temporale, così una misura
  presa a credito esaurito si può **riconoscere e scartare** invece di
  mediarla dentro al risultato.

**Regola 2 — i MB/s non possono rispondere alla domanda "il server è il collo
di bottiglia?"** su una macchina con il credito di banda, perché il credito
limita la velocità molto prima della CPU. La risposta onesta si dà in **secondi
di CPU per GiB trasferito**, che non dipende dal tetto imposto. E va misurata
sull'**host**, non dentro al contenitore: la campagna precedente ha misurato
che il 37–40 % del conto è lavoro che il kernel fa per conto del contenitore e
che il contenitore non si vede addebitare.

**Regola 3 — il percorso si verifica, non si suppone.** Ogni misura legge dal
server quale trasporto ha *effettivamente* usato. Una prova che ha chiesto
`--udp` ma è finita sul percorso normale viene riportata come percorso
normale, mai spacciata per QUIC. Questa campagna è la prima che **può** farlo
sui tunnel pubblici: i quattro campi che lo permettono sono uno dei difetti
corretti (P-2).

**Regola 4 — le connessioni "tenute aperte" non devono muovere byte.** Nella
prova di concorrenza si aprono fino a 512 connessioni e si misura quanto costa
una richiesta nuova con quelle in mezzo. Se quelle connessioni scaricassero
dati satureremmo il collegamento e misureremmo il collegamento. Parlano quindi
un comando apposito (`HOLD`) che risponde una volta e poi tace. Vengono inoltre
tenute aperte da **un solo processo**: 512 interpreti Python su una macchina a
2 CPU misurerebbero la memoria del generatore di carico, non il tunnel.

**Regola 5 — il gateway SSH è solo TCP, per progetto.** La tratta SSH non
supporta né `--udp` né più di una corsia. Viene quindi confrontata con le prove
**su percorso normale** delle altre due modalità, mai con quelle QUIC. È una
proprietà del trasporto, non un difetto, e la tabella lo dichiara su ogni riga.

### 2.3 I segreti non sono mai stati scritti nel repository

Chiave SSH, password del gateway, segreto del tunnel e token di
amministrazione **non compaiono da nessuna parte** nei file versionati. Vivono
in un file di ambiente fuori dal repository (permessi `600`), e la
documentazione rimanda a quello. Prima di ogni `commit` il contenuto in stage
viene passato al setaccio per ciascuno di quei valori.

### 2.4 Tutto è ripetibile

Gli script della campagna stanno nel repository, sotto
`scripts/perf/staging/pub/`, e non contengono nessun indirizzo, nessun nome
host e nessuna credenziale: si ripuntano su un altro deployment cambiando solo
il file di ambiente. Il **ricettario** in fondo elenca i comandi esatti per
rifare qualunque singola prova senza ricominciare da capo.

---

## 3. La domanda principale: conviene il percorso UDP diretto?

È la domanda che la componente `--udp` pone da sola, ed è quella a cui la
campagna dedica più prove. La risposta breve è: **no come impostazione
predefinita, sì in due casi precisi.** Le prossime quattro sezioni sono la
dimostrazione.

### 3.1 Trasferimenti grossi (P1)

**Che cosa abbiamo provato.** 128 MiB per volta su 4 connessioni, cinque
coppie, prima in scaricamento e poi in caricamento. Ogni coppia misura i due
trasporti uno dopo l'altro invertendo l'ordine, e si riporta il rapporto.
Nessun HTTP: TCP grezzo, perché un tunnel pubblico trasporta TCP qualunque e
misurarlo attraverso un server web significherebbe misurare anche il server
web.

**Che cosa è successo.**

| direzione | percorso normale (TCP) | percorso diretto (QUIC) | rapporto mediano |
| --- | --- | --- | --- |
| scaricamento | 160–199 MB/s | 115–133 MB/s | **il normale è 1,51× più veloce** |
| caricamento | 157–189 MB/s | 126–130 MB/s | **il normale è 1,34× più veloce** |

Tutte e dieci le coppie concordano nel segno: non è rumore. E il percorso
diretto era davvero diretto — il server ha contato l'apertura di un canale
QUIC per ognuna delle 4 connessioni in ogni singola prova.

> **Questa tabella è "prima della correzione".** È stata presa sulla versione
> `dbcc645a`, che aveva il difetto P-13 (§8.1) proprio sul socket che riceve i
> byte di uno scaricamento. Dopo la correzione il percorso diretto guadagna il
> 29 % e il rapporto scende da 1,92 a 1,40 sulla prova di riferimento: il
> percorso normale resta davanti, ma di meno. Le dieci coppie complete non sono
> state rifatte — una prova sola basta a stabilire la *causa*, non a riscrivere
> tutte le mediane — e `pub/rerun_stage.sh p1` le rifà in circa un'ora a chi
> servono aggiornate.

**Dov'è il collo di bottiglia.** Non nel tunnel. Le due prove muovono gli
stessi byte sugli stessi due salti; l'unica differenza è chi trasporta la
tratta server→client, e la più lenta è quella che fa il controllo di
congestione e la gestione dei pacchetti **in spazio utente**, su una macchina
in cui la CPU è la risorsa scarsa. La sezione 8 mette un prezzo preciso a questa
differenza.

Da notare: 160–199 MB/s sono 1,3–1,7 Gbit/s, cioè molto sopra la banda
sostenibile dell'istanza. Sono numeri di **burst**: il senso dell'appaiamento
è che le due metà di ogni coppia spendono lo stesso credito, non che quella
velocità sia mantenibile.

### 3.2 Richieste piccole, una alla volta (P3)

**Che cosa abbiamo provato.** Cento connessioni nuove, una dopo l'altra, con
un carico minuscolo. È quello che paga un utente singolo che apre il sito o si
collega al servizio: quasi tutto il tempo è apertura della connessione.

**Che cosa è successo.**

| trasporto | mediana | p95 | p99 | errori |
| --- | --- | --- | --- | --- |
| percorso normale | **4,665 ms** | **6,226 ms** | **8,767 ms** | 0 |
| percorso diretto | 4,899 ms | 10,756 ms | 17,785 ms | 0 |

Le mediane si equivalgono (0,23 ms di differenza). **La coda no**: il percorso
diretto è 1,7× peggio al 95° percentile e 2,0× peggio al 99°. Qui non c'è né
perdita di pacchetti né carico: è la variabilità propria del percorso in
spazio utente.

**Dov'è il collo di bottiglia.** Nei tempi di andata e ritorno, per entrambi.
La distanza VM→server è di circa 2,1 ms e ogni sonda ne paga due: non resta
nessuna attesa da smontare. Per lo stesso motivo, come nella campagna
precedente, **non** sono stati aggiunti cronometri interni: non c'è niente che
possano scoprire.

### 3.3 Richieste piccole, molte insieme (P4) — qui il verdetto si ribalta

**Che cosa abbiamo provato.** Lo stesso tunnel, questa volta verso un vero
server HTTP, con carico crescente: 1, 8 e 32 richieste in parallelo, più una
prova in cui **ogni richiesta apre una connessione nuova**.

**Che cosa è successo.**

| carico | percorso normale p95 / p99 | percorso diretto p95 / p99 |
| --- | --- | --- |
| 1 in parallelo | 3,03 / 3,41 ms | 3,04 / 3,34 ms |
| 8 in parallelo | 2,92 / 3,10 ms | 2,85 / 3,27 ms |
| **32 in parallelo** | 4,58 / 7,27 ms | **3,48 / 3,96 ms** |
| 8, connessione nuova ogni volta | 7,15 ms (p95) | **5,99 ms** (p95) |

E, sullo stesso tunnel, un singolo scaricamento pesante:

| trasporto | MB/s |
| --- | --- |
| percorso normale | **207,74** |
| percorso diretto | 84,04 |

**Che cosa vuol dire.** Fino a 8 richieste in parallelo i due trasporti sono
indistinguibili. A **32 si separano, e vince il percorso diretto**: la coda al
99° percentile passa da 7,27 a 3,96 ms, cioè 1,84× meglio. Il numero di
richieste al secondo invece non cambia: è un effetto puro di **coda**.

Il motivo è quello che l'architettura fa prevedere. Sul percorso normale tutte
e 32 le connessioni viaggiano dentro **una sola** connessione TCP: condividono
una finestra di congestione, un buffer e un solo compito che le incapsula, e
una connessione lenta ritarda quelle accodate dietro. Sul percorso diretto ogni
connessione ha il proprio canale QUIC indipendente.

E il prezzo è sulla riga successiva della stessa tabella: **lo stesso tunnel
che è 1,84× migliore sulla coda con 32 richieste è 2,47× più lento su un
singolo trasferimento pesante.** Non sono due fatti in contraddizione: sono lo
stesso fatto visto due volte. L'indipendenza fra i canali si paga in banda.

**Dov'è il collo di bottiglia.** Con 1 e 8 richieste: i tempi di andata e
ritorno, e nessuno dei due trasporti può farci niente. Con 32: sul percorso
normale è l'ordinamento dentro l'unica connessione TCP — ed è esattamente
quello che le "corsie" parallele sanno alleviare, come mostra la sezione
seguente. Sul trasferimento pesante in QUIC: la CPU del server, che la sezione 8
quantifica.

### 3.4 Le corsie parallele (P2)

**Che cosa abbiamo provato.** Lo stesso trasferimento da 128 MiB su 4
connessioni, aprendo 1, 2, 4 e 8 corsie verso il server.

**Che cosa è successo.**

| corsie | scaricamento MB/s | caricamento MB/s |
| --- | --- | --- |
| 1 | 210,25 | 154,90 |
| 2 | 252,38 | **229,30** |
| 4 | **303,60** | 225,05 |
| 8 | 276,62 | 178,10 |

**Che cosa vuol dire.** La scala sale e poi ridiscende: il massimo è a **4
corsie in scaricamento (1,44× rispetto a una sola) e a 2 in caricamento
(1,48×)**, e 8 è peggio di 4 in entrambe le direzioni.

È il segno **opposto** a quello misurato sulla campagna `vhost`, dove le
corsie facevano un po' di danno su un percorso pulito. La differenza sta in
che cosa si trasporta: là si misurava **un** trasferimento singolo, che viaggia
comunque su una sola corsia, quindi le altre erano solo costo. Qui si misurano
**quattro connessioni insieme**, che il server distribuisce sulle corsie
disponibili.

**Dov'è il collo di bottiglia.** Con 1 corsia è la singola connessione TCP.
Con 8 sono la CPU del server e il credito di banda: otto connessioni su due
CPU, ciascuna con il proprio controllo di congestione che rampa dentro lo
stesso secchio già limitato, vanno misurabilmente peggio di quattro.

La frase utile per chi gestisce il servizio non è "4 è il numero giusto", ma:
**il numero giusto segue quante connessioni contemporanee il tunnel serve
davvero**, e oltre quel punto si paga. Un tunnel che fa un trasferimento alla
volta deve restare a 1, che è il valore predefinito.

---

## 4. Le tre modalità di esecuzione: binario, Docker, SSH

**Che cosa abbiamo provato.** Un tunnel pubblico si può aprire in tre modi:
con il binario nativo, con lo stesso binario dentro a Docker, oppure con un
semplice `ssh -R` verso il gateway SSH del server — cioè **senza installare
niente**. Le tre modalità sono state registrate **tutte e tre insieme** e
colpite a rotazione, cambiando l'ordine a ogni giro, perché su questa macchina
chi corre per primo consuma il credito di banda di tutti.

Il gateway SSH non supporta né UDP né le corsie parallele: è una proprietà del
protocollo, non un difetto. Gira quindi con **una** corsia mentre le altre due
ne usano otto, e il confronto va letto tenendone conto.

Una nota di onestà: otto corsie non sono il valore migliore per questo carico
— la scala della sezione 3.4 ha il massimo a quattro. Il confronto fra binario
nativo e Docker resta valido (girano con la stessa impostazione), ma i loro
numeri assoluti qui sono qualche punto percentuale sotto il loro meglio, e di
conseguenza il distacco dell'SSH appare più grande di quanto sia. Il paragone
corretto per l'SSH è contro la misura a **una** corsia, ed è così che viene
letto qui sotto.

**Che cosa è successo.**

| modalità | scaricamento (mediana) | caricamento (mediana) | latenza p50 |
| --- | --- | --- | --- |
| binario nativo | 259,19 MB/s | 224,84 MB/s | 4,691 ms |
| binario in Docker | 308,18 MB/s | 220,20 MB/s | 4,658 ms |
| `ssh -R` | 112,68 MB/s | 174,58 MB/s | 5,437 ms |

**Che cosa vuol dire.**

*Nativo e Docker sono la stessa cosa.* La differenza del 19 % in scaricamento
sembra un risultato finché non si guardano i singoli giri: nativo va da 232 a
275, Docker da 230 a 311, e chi vince cambia di giro in giro. In caricamento
sono al 2 % l'uno dall'altro, in latenza a 0,03 ms. **L'immagine Docker non
costa niente di misurabile.** L'unica differenza vera non è di prestazioni ma
di permessi: Docker toglie tutte le "capability" a un utente non root, quindi
l'immagine predefinita non riesce ad allargare i buffer UDP del sistema. Per
questo la campagna usa l'immagine **root** `ghcr.io/manprint/bore:client`.

*Il gateway SSH è più lento in scaricamento, ma meno di quanto sembri.* 112,68
contro 259,19 MB/s sono 2,3 volte, però una parte è la corsia singola, non
l'SSH. Il paragone corretto è con la misura a **una** corsia della sezione
3.4: 210,25 MB/s. Contro quella, l'SSH è 1,87 volte più lento — una differenza
vera, e nota: la finestra per canale del client OpenSSH limita il singolo
canale. In **caricamento** invece la differenza quasi sparisce (174,58 contro
224,84).

*Sulla latenza l'SSH costa pochissimo:* 0,75 millisecondi in più per ogni
connessione nuova. Per chi non può installare un binario, è tutto il prezzo.

**Dov'è il collo di bottiglia.** Per nativo e Docker: l'istanza AWS, non il
forwarder — entrambi finiscono nella stessa banda di 230–310 MB/s. Per l'SSH
in scaricamento: il canale SSH stesso (una corsia più la finestra di OpenSSH),
che è una caratteristica del trasporto per cui il gateway esiste.

## 5. Reti che perdono pacchetti: qui il percorso diretto vince, e non di poco

**Che cosa abbiamo provato.** Fin qui il confronto è avvenuto su una rete
pulita, cioè nel caso migliore per il relay TCP. Ma un tunnel serve spesso
proprio dove la rete non è pulita. Abbiamo quindi **degradato la rete apposta**
sulla macchina che ospita il forwarder (`tc netem`), una condizione per volta,
e rimisurato le due strade con gli stessi 64 MB su 4 connessioni. I due tunnel
erano registrati **insieme**, così ogni riga confronta le due strade sotto la
*stessa* identica degradazione.

Le condizioni provate sono due famiglie: **perdita di pacchetti** (l'1 %, il
3 %, il 10 % dei pacchetti sparisce) e **ritardo** (40 e 100 millisecondi in
più per ogni pacchetto). Sono i due modi in cui una rete reale peggiora: la
perdita è tipica delle reti radio e delle linee sature, il ritardo è tipico
delle distanze geografiche.

**Che cosa è successo.**

| condizione della rete | relay TCP | UDP diretto | chi vince |
| --- | --- | --- | --- |
| pulita | **188,77 MB/s** | 85,86 MB/s | relay, 2,2 volte |
| perdita 1 % | 72,05 | **100,48** | diretto, 1,4 volte |
| perdita 3 % | 5,27 | **47,70** | diretto, 9 volte |
| perdita 10 % | 0,39 | **51,40** | **diretto, 132 volte** |
| ritardo 40 ms | 4,47 | **12,37** | diretto, 2,8 volte |
| ritardo 40 ms + perdita 1 % | 1,75 | **12,19** | diretto, 7 volte |
| ritardo 100 ms | 3,70 | **4,97** | diretto, 1,3 volte |
| ritardo 40 ms + riordino | **26,03** | 12,29 | relay (ma vedi sotto) |

**Che cosa vuol dire.**

*Con la perdita di pacchetti il relay non rallenta: si ferma.* Al 10 % di
perdita il relay consegna 0,39 MB/s — in pratica niente — mentre il percorso
diretto ne consegna 51,40, cioè **più della metà di quanto consegna su rete
pulita**. Non è una differenza di velocità, è una differenza fra "funziona" e
"non funziona".

*Il motivo è come sono impacchettate le connessioni.* Sul relay le quattro
connessioni viaggiano dentro a **una sola** connessione TCP: un pacchetto perso
blocca tutte e quattro finché non viene ritrasmesso, e la "finestra" che le
quattro si dividono si richiude di colpo. Sul percorso diretto ogni
connessione ha il suo flusso indipendente: la perdita riguarda solo chi l'ha
subita. È esattamente il problema per cui esistono le corsie parallele
(`--carriers`), che però lo attenuano e non lo eliminano.

*Il ritardo è un'altra storia e non si cura con `--udp`.* A 40 e 100
millisecondi crollano **tutte e due** le strade (da 188 a 4,47 il relay, da 85
a 12,37 il diretto). Non è un difetto: è il prodotto banda×ritardo, cioè
quanti dati possono stare "in volo" contemporaneamente. La cura per la
distanza sono **più corsie** (sezione 3.4), non il cambio di trasporto.

*La riga del riordino va letta con prudenza.* Lo strumento che simula il
riordino non aggiunge disordine sopra il ritardo: manda **subito** una parte
dei pacchetti invece di ritardarli, quindi quella riga ha in media meno
ritardo delle altre. Il salto del relay da 4,47 a 26,03 è in buona parte
questo effetto, non una vittoria del TCP. L'unica cosa che la riga dimostra
davvero è che **il riordino non ha rotto nessuna delle due strade**.

**Dov'è il collo di bottiglia.** Con la perdita: la connessione TCP condivisa
del relay. Con il ritardo: la finestra del protocollo, su entrambe le strade.
Su rete pulita: la CPU del server, come nella sezione 3.

**La conseguenza pratica**, ed è il punto più importante di tutta la
campagna: `--udp` su un tunnel pubblico **non è una manopola della velocità,
è una manopola della robustezza**. La decisione non dipende dal tipo di
applicazione ma dalla rete fra il forwarder e il server. Se quella rete perde
pacchetti, il diretto va acceso e il 2× che costa su rete pulita è un prezzo
irrisorio rispetto al 100× che costa non averlo.

## 6. Che cosa succede quando qualcosa va storto

**Che cosa abbiamo provato.** Le sezioni precedenti misurano quanto va veloce
un tunnel quando tutto funziona. Questa fa l'opposto: rompe qualcosa apposta e
guarda se il sistema si rialza. Sono quattro prove, e tre di esse sono la
verifica sul campo di altrettanti difetti corretti in questa campagna. Non
danno numeri di velocità: danno un sì o un no.

**Che cosa è successo.**

| prova | domanda | esito |
| --- | --- | --- |
| perdita e ritorno del percorso diretto | se il percorso UDP sparisce, torna da solo? | ripiega sul relay in **12 secondi**, e il diretto torna **4 secondi** dopo che la rete guarisce — **superata** |
| morte improvvisa del forwarder | la porta pubblica si libera ed è riutilizzabile? | liberata in **0 secondi**, e un nuovo tunnel l'ha ripresa subito — **superata** |
| forwarder bloccato (non morto) | un client congelato viene sganciato? | porta recuperata dopo **60 secondi**, esattamente la scadenza prevista — **superata** |
| raffica di connessioni | 500 connessioni brevi lasciano residui? | 500 in **2 secondi**, connessioni attive da 0 a 0, **zero** rifiuti del server, e subito dopo il tunnel serviva ancora 84,77 MB/s — **superata** |

**Che cosa vuol dire.**

*La prima prova è la dimostrazione che P-7 è chiuso.* Prima della correzione il
percorso diretto non tornava **mai**: il server non leggeva la richiesta di
rinnovo. Ora torna in 4 secondi, cioè il tempo di un giro di domanda e
risposta. Vale la pena notare l'asimmetria dei due tempi: accorgersi della
caduta costa 12 secondi (è il tempo che il protocollo aspetta prima di
dichiarare morto un percorso silenzioso), rimettersi in piedi ne costa 4.

*La terza prova è la dimostrazione che P-4 è chiuso*, e i 60 secondi non sono
"più o meno un minuto": sono esattamente la scadenza configurata. La prova
aspetta fino a 150 secondi apposta, così un esito positivo a 60 non può essere
scambiato per una coincidenza. Prima della correzione la porta restava
occupata fino al riavvio del server.

*La quarta prova cerca le perdite.* Che le connessioni attive tornino a zero è
la metà debole della risposta; la metà forte è che il server non ha mai dovuto
**rifiutare** una connessione — cosa che avrebbe fatto se i "permessi" interni
non fossero stati restituiti lungo le 500 connessioni.

### 6.1 L'ora di fila: il tunnel perde memoria?

C'è una quinta prova, che non rompe niente e per questo risponde a una domanda
diversa: **un tunnel lasciato aperto a lungo consuma memoria che non
restituisce?** È la domanda che conta per chi lascia un tunnel attivo giorni.

Un solo tunnel pubblico sul percorso diretto, tenuto aperto **un'ora intera**,
che trasferisce 8 MiB ogni 25 secondi. A ogni giro si rilegge dal server la sua
opinione sul tunnel: che porta ha, che percorso sta usando, quante volte è
ripiegato sul relay, quanta memoria occupa il server.

| che cosa abbiamo guardato | su 143 campioni in 62 minuti |
| --- | --- |
| porta pubblica | 9057 al primo campione, 9057 all'ultimo: **non si è mai spostata** |
| percorso | `direct` su **tutti** i campioni; zero rientri sul relay |
| ripieghi sul relay | **zero** per tutta l'ora |
| corsie dirette attive | sempre 1, quella richiesta: mai scesa |
| connessioni diritte aperte | da 2 a 144, cioè **+142 per 143 connessioni**: una connessione diretta per ogni connessione servita, nessuna passata dal relay |
| memoria del server | da 15 433 728 a 15 437 824 byte: **+4 096 byte**, una pagina, dopo circa 1,14 GiB trasferiti |

**Il risultato è la riga della memoria.** Una perdita si vede come una memoria
che sale insieme al lavoro fatto; qui, dopo 143 connessioni e 1,14 GiB, il
server è cresciuto di **una singola pagina** — che è rumore dell'allocatore,
non una tendenza. Il numero è riportato e non messo a confronto con una soglia,
per un motivo onesto: su questo server girano anche i tunnel dell'operatore, e
una soglia stretta misurerebbe anche quelli. Quello che rende significativa una
crescita di una pagina è che *tutto il resto* si è mosso — 143 connessioni
aperte e chiuse, 142 flussi diretti creati — e la memoria no.

**La riga delle connessioni dirette è il secondo risultato.** Dice che il
percorso diretto non è stato negoziato una volta all'inizio e poi
silenziosamente abbandonato: **tutte** le 143 connessioni ci sono passate. È
esattamente la distinzione che i contatori per-tunnel aggiunti in questa
campagna (P-2) servono a rendere visibile, e che un contatore globale del
server non può fare.

Una precisazione sulla velocità, per non farla leggere male: quegli 8 MiB per
campione viaggiavano a circa 37 MB/s, ma **non è un tetto**. Un trasferimento
di 8 MiB su una sola connessione finisce prima che il protocollo abbia finito
di accelerare: il tetto vero, sullo stesso percorso, è quello della §3
(115-133 MB/s). Qui quella colonna serve solo a dimostrare che il tunnel ha
continuato a funzionare, e la sua oscillazione (18-40 MB/s) è il credito di
banda di AWS che respira — lo stesso motivo per cui ogni confronto in questo
documento è appaiato.
---

## 7. Molte connessioni insieme: quanto costa aprirne una nuova?

**Che cosa abbiamo provato.** Un tunnel pubblico serve spesso più di una
connessione alla volta. La domanda pratica è: *mentre il tunnel tiene aperte
molte connessioni, quanto tempo ci vuole perché una connessione **nuova**
cominci a funzionare?* È la cosa che l'utente percepisce come "il sito è
lento a partire", ed è indipendente dalla banda.

La prova tiene aperte 16, 64, 128, 256 e infine 512 connessioni
**completamente inattive** — parlano un verbo dell'origine (`HOLD`) che
risponde una volta sola e poi non muove più un byte — e, mentre sono aperte,
apre una connessione nuova trenta volte di seguito misurando quanto tempo
passa prima che risponda. Le connessioni inattive sono una scelta precisa: se
stessero scaricando, saturerebbero il collegamento e misureremmo il
collegamento, non il tunnel.

> **Nota di onestà sul metodo: questa prova è stata rifatta quattro volte.**
> Le tre esecuzioni scartate non sono un incidente da nascondere, sono parte
> del risultato.
>
> 1. La **prima** non ha misurato nulla: il processo dell'origine sulla
>    macchina di prova era più vecchio del programma da cui era stato avviato,
>    e quindi non conosceva il verbo `HOLD`. Il risultato era `held=N ...
>    active_at_server=0`: una tabella di zeri con la forma esatta di una misura
>    vera (difetto H-7).
> 2. La **seconda** sommava i gradini invece di sostituirli — al gradino da 512
>    c'erano in realtà 976 connessioni aperte, non 512 (difetto H-9) — ed è
>    **l'esecuzione che ha fatto cadere il server**: è lì che abbiamo trovato
>    il difetto più importante della campagna, quello raccontato in §1.4.
> 3. La **terza** aveva la scala corretta ma provava le "corsie" sempre nello
>    stesso ordine, e la prima delle tre leggeva 9,956 ms contro 4,6 ms delle
>    altre due — cioè misurava *l'ordine*, non la configurazione (v. sotto).
> 4. La **quarta**, qui sotto, ha scala e corsie nella stessa esecuzione, con
>    un controllo esplicito fra un gradino e l'altro che verifica sul server
>    che le connessioni precedenti siano davvero state chiuse.

**Che cosa è successo — percorso relay (TCP).**

| connessioni tenute aperte | mediana (p50) | 95° percentile | 99° percentile |
| --- | --- | --- | --- |
| nessuna (riferimento) | 4,704 ms | 5,321 ms | 7,216 ms |
| 16 | 4,749 ms | 7,282 ms | 9,831 ms |
| 64 | 4,948 ms | 5,488 ms | 7,620 ms |
| 128 | 4,762 ms | 5,105 ms | 7,204 ms |
| 256 | 4,792 ms | 5,131 ms | 7,349 ms |
| **512** | **4,945 ms** | 8,256 ms | 8,542 ms |

Trenta connessioni nuove per riga, **zero errori** in tutte.

**Che cosa è successo — percorso diretto (QUIC/UDP).**

| connessioni tenute aperte | mediana (p50) | 95° percentile | 99° percentile |
| --- | --- | --- | --- |
| nessuna (riferimento) | 4,698 ms | 5,708 ms | 7,007 ms |
| 16 | 5,114 ms | 7,915 ms | 8,180 ms |
| 64 | 4,711 ms | 5,464 ms | 7,617 ms |
| 128 | 4,602 ms | 7,461 ms | 7,513 ms |
| 256 | 4,675 ms | 5,662 ms | 7,449 ms |
| **512** | **4,700 ms** | 5,895 ms | 7,067 ms |

Sul percorso diretto ogni connessione tenuta aperta è un **flusso QUIC** a sé:
al gradino da 512 ci sono 512 flussi simultanei su una sola connessione. Il
server ne ha aperti 1 157 in tutta la prova e **non è mai ripiegato sul
relay** (`fb=0`).

La terza esecuzione, quella scartata solo per l'ordine delle corsie, aveva una
scala perfettamente valida e **concorda**: sul relay leggeva fra 4,508 e
4,685 ms, sul diretto fra 4,445 e 4,589 ms, con gli stessi 1 157 flussi aperti
fino all'ultimo. Due esecuzioni in giorni diversi sulla stessa macchina
differiscono fra loro di circa 0,2 ms: **più** di quanto ciascuna delle due
cambi passando da nessuna a 512 connessioni aperte. È il modo più pulito di
dire che un ginocchio non c'è.

**Che cosa è successo — le "corsie" (`--carriers`) sotto carico.**

Il pool di carrier esiste per evitare che una connessione lenta blocchi le
altre sulla stessa connessione TCP multiplexata. Se serve a qualcosa per i
tunnel pubblici, deve vedersi **qui** e non nelle prove a flusso singolo. Due
giri in ordine opposto, così che nessuna configurazione sia sempre la prima:

| corsie | giro 1 | giro 2 | valore unito | rispetto a 1 corsia |
| --- | --- | --- | --- | --- |
| 1 | 5,129 ms | 4,898 ms | **5,014 ms** | — |
| 4 | 4,728 ms | 4,761 ms | **4,744 ms** | 0,946 |
| 8 | 4,878 ms | 4,755 ms | **4,816 ms** | 0,961 |

Tutte le righe con 128 connessioni tenute aperte, trenta misure ciascuna, zero
errori.

**Le corsie non cambiano niente di misurabile, e il 5 % di guadagno apparente
è grande quanto il rumore.** La differenza fra una corsia e quattro è
0,269 ms. Ma la *stessa* configurazione a una corsia, misurata due volte,
varia da sola di 0,231 ms — e la scala qui sopra, che è esattamente la stessa
configurazione (128 tenute, una corsia), legge 4,762 ms, cioè un valore in
mezzo ai due giri. Una differenza che una configurazione mostra **contro se
stessa** non è una differenza fra configurazioni. La frase onesta è: con 128
connessioni inattive aperte le corsie non sono né un guadagno né una perdita
sulla latenza. È lo stesso risultato della campagna vhost su percorso pulito
(mediana c4/c1 0,941 lì), ed è esattamente il motivo per cui `--carriers`
vale 1 per default.

**L'effetto dell'ordine, che è il motivo per cui questa prova è stata
rifatta.** Nella versione precedente le tre configurazioni giravano sempre
nell'ordine 1, 4, 8 e la prima leggeva 9,956 ms: più del doppio delle altre
due. Non era la configurazione, era la posizione. Ogni prova consuma il
"credito di banda" che l'istanza AWS accumula quando è ferma, e chi arriva
subito dopo la prova precedente paga il residuo. Con due giri in ordine
opposto quel residuo colpisce una volta ciascuna configurazione e si vede per
quello che è: nel giro 1 la singola corsia (prima) legge 5,129 ms, nel giro 2
la singola corsia (ultima) legge 4,898 ms — 0,23 ms di differenza contro i
5,3 ms della versione a ordine fisso.

**Dov'è il collo di bottiglia.**

**Aprire una connessione nuova costa lo stesso con 512 connessioni aperte e
con nessuna.** Sul relay la mediana si muove fra 4,704 e 4,948 ms su tutta la
scala — una forbice di 0,244 ms — e i due estremi sono il gradino da **zero**
e quello da 64, quindi non c'è nemmeno un andamento da interpretare. Sul
percorso diretto va da 4,602 a 5,114 ms, e il valore più alto è quello del
gradino da 16, cioè il carico **più leggero**. **Non c'è nessun ginocchio**
prima di 512 connessioni: il collo di bottiglia non è la concorrenza.

Una nota che vale più dei numeri: la colonna «connessioni attive **secondo il
server**» è letta dal server stesso, non dichiarata dal programma di prova, e
adesso coincide esattamente con quelle richieste a ogni gradino, su entrambi i
trasporti e in tutti i bracci delle corsie. Nella seconda esecuzione leggeva
80, 208 e 464 ai gradini da 64, 128 e 256 — cioè il **totale progressivo**,
perché l'origine teneva aperta la propria metà delle connessioni del gradino
precedente (difetto H-9). Una scala i cui gradini si sovrappongono non è una
scala, e questi numeri sono utilizzabili solo perché ora i due conteggi
concordano.

C'è un ultimo fatto che vale la pena dire, perché è un confronto con la
campagna precedente. Su questa stessa macchina, la campagna `vhost` aveva
misurato una richiesta nuova a **966 ms** dietro 256 connessioni tenute
aperte, e 1 436 ms dietro 512, contro 11 ms su un server privato. Sul percorso
pubblico **non si vede niente di simile**: 4,7 ms piatti fino a 512. Le due
prove non sono la stessa prova — quella vhost apre una connessione TCP *più*
un handshake TLS completo *più* una richiesta HTTP instradata per nome, questa
apre una connessione TCP più un canale — quindi non è una smentita. È un
posto in più in cui quella coda non compare, e la cosa è coerente con la
spiegazione rimasta in piedi: qualunque sia la causa, **non è** nel codice che
accetta le connessioni né nella contabilità che le tiene, che è esattamente
ciò che questa scala mette sotto sforzo, 512 alla volta, senza produrre alcuna
coda.
---

## 8. Quanta CPU costa un gigabyte

**Che cosa abbiamo provato.** Questa è l'unica misura che risponde davvero
alla domanda «il server è al limite del software?». La velocità in MB/s non
può rispondere: su un'istanza "burstable" come questa il credito di banda di
Amazon taglia la velocità molto prima della CPU, quindi un tetto di MB/s non
dimostra nulla. I **secondi di CPU per gigabyte trasferito** invece non
dipendono dal credito: se il credito dimezza la velocità, il lavoro dura il
doppio e il conto per gigabyte resta lo stesso.

Il conto è fatto sull'**host**, non sul container: il kernel spende una parte
importante del lavoro di rete in `softirq`, che il contatore del container non
vede. Nella campagna vhost quella parte era il 37-40 % del conto totale;
leggere solo il container sottostima il costo di circa un terzo, ed è proprio
l'errore che cambia la risposta alla domanda.

> **Nota di onestà sul metodo.** Anche questa prova, alla prima esecuzione,
> ha prodotto soltanto zeri: il generatore di carico veniva ucciso da un
> `timeout` esterno prima di poter stampare il risultato, e la shell leggeva
> il campo vuoto `bytes=` come `0`. Nove casi di niente, stampati nel formato
> esatto di una misura vera. È il difetto H-8 di §1. Ora è il **client** a
> chiudere la finestra di misura per conto proprio, accreditando a ogni
> connessione i byte che ha effettivamente mosso, e la prova è stata rifatta.

**Che cosa è successo.**

Tre giri per ciascun caso, finestre di 20 secondi, sempre la stessa prova:

| caso | GiB spostati | **secondi di CPU per GiB** | core occupati (su 2) |
| --- | --- | --- | --- |
| relay, 1 corsia | 4,59 / 4,53 / 5,13 | 6,84 / 7,70 / **7,04** | 1,57 – 1,81 |
| relay, 8 corsie | 7,10 / 6,80 / 5,65 | 5,13 / **5,35** / 6,20 | 1,84 – 1,91 |
| diretto (QUIC) | 2,09 / 2,28 / 1,95 | 12,76 / 13,64 / **13,58** | 1,39 – 1,56 |

(in grassetto la mediana di ciascuna riga)

**Dov'è il collo di bottiglia.**

**Il percorso diretto costa 1,93 volte la CPU del relay, a parità di byte
spostati** — 13,58 contro 7,04 secondi di CPU per gigabyte, con la stessa
singola corsia. Questo è il *meccanismo* dietro il risultato della §3: il
relay non è più veloce perché QUIC sia lento sul cavo, è più veloce perché
**questa macchina non può permettersi** il costo per byte di QUIC alla
velocità che il relay raggiunge.

**E il conto si chiude.** Un core a 13,58 s/GiB consegna 0,63 Gbit/s; i giri
sul percorso diretto hanno usato 1,39–1,56 core, cioè 0,88–0,99 Gbit/s, cioè
**105–118 MB/s** — e le velocità misurate sono 95,11, 106,76 e 116,85 MB/s.
Stesso conto sul relay a 8 corsie: 5,35 s/GiB sono 1,61 Gbit/s per core, 1,91
core danno 3,07 Gbit/s = 366 MB/s, misurati 363,25. Due grandezze indipendenti
che concordano entro l'uno per cento: è questo che rende il risultato solido, e
la conclusione è netta — **il tetto del percorso diretto, su questa macchina,
è la CPU, non la rete.**

**Le 8 corsie costano MENO per byte di una sola** (5,35 contro 7,04 s/GiB)
*e* consegnano di più (348–363 contro 232–250 MB/s). Più parallelismo a costo
unitario più basso è la firma di un lavoro fisso per trasferimento che si
distribuisce su più byte in volo, ed è la conferma dal lato CPU di quello che
la scala delle corsie (§3.4) diceva dal lato velocità.

**Circa metà del conto è lavoro del kernel** (19,42 secondi su 36,37 nel giro
relay, 18,37 su 31,12 in quello diretto: 53 % e 59 %). È esattamente per
questo che la misura è fatta sull'host e non dentro al container: leggere solo
il container avrebbe sottostimato il costo di più della metà, e avrebbe dato la
risposta **sbagliata** alla domanda «è il software il collo di bottiglia?».

**La CPU è di `bore`.** Nella finestra del relay a 8 corsie l'host ha
consumato 36,37 secondi di CPU e il processo `bore` ne ha consumati 35: il
96 %. Non c'è nessun «vicino rumoroso» a cui dare la colpa e nessun resto
inspiegato: a 350 MB/s questa macchina a 2 core sta facendo girare `bore`, a
tavoletta.

**Dov'è il collo di bottiglia.** *Limite dell'applicazione*, e per la prima
volta in questa campagna lo si può dire con un numero: i core occupati stanno
fra 1,39 e 1,91 su 2, cioè la macchina è **satura**. Non è l'istanza a
razionare (il tempo «rubato» dall'hypervisor è 0,01 secondi in quasi tutte le
finestre) e non è la rete: è il costo per byte del software su due core
piccoli.

**Come usarlo per dimensionare un server.** Per sostenere una certa velocità
sul percorso relay servono circa `GiB/s × 5,4` core con 8 corsie, o
`GiB/s × 7,0` con una; sul percorso diretto `GiB/s × 13,6`. In pratica: 10
Gbit/s di tunnel pubblici sul relay chiedono circa **6 core** di questa
classe, gli stessi 10 Gbit/s sul percorso diretto ne chiedono circa **16**.

### 8.1 P-13: il percorso diretto pagava per byte che buttava via

Tutto quello che c'è sopra è scritto come se 13,58 secondi di CPU per
gigabyte fossero **il prezzo di QUIC**. Non lo sono. Quel divario ha una
causa, la causa è un difetto di una riga, e per trovarla è bastato fare la
domanda successiva: il tempo di kernel si spende **per pacchetto**, quindi il
percorso diretto muove davvero tre volte i pacchetti a parità di byte? Non
dovrebbe: il pacchetto più grande che la rete accetta è 1500 byte in entrambi
i casi.

Contatori letti sull'interfaccia del server, una finestra di 20 secondi per
trasporto, consumatore dentro la regione:

| braccio | consegnato | byte in **uscita** | pacchetti | byte in **entrata** |
| --- | --- | --- | --- | --- |
| relay | 212,70 MB/s | 4,360 GiB | 3 331 128 | **4,352 GiB** |
| diretto | 111,26 MB/s | 2,279 GiB | 1 685 178 | **3,885 GiB** |

I pacchetti per gigabyte consegnato sono quasi identici (764 075 contro
739 363), quindi la risposta alla domanda è **no** e il costo per pacchetto
non era l'anomalia. L'anomalia è nell'ultima colonna. Il relay riceve 4,352
GiB per consegnarne 4,360: entrata e uscita coincidono allo 0,2 %, come deve
essere per un relay. Il diretto riceve **3,885 GiB per consegnarne 2,279**:
**1,78 volte i byte in entrata rispetto a quelli in uscita**. Scomponendo il
traffico in entrata fra pacchetti di dati e riscontri, sono circa 2,85
milioni di pacchetti di dati ricevuti per consegnarne 1,69 milioni.

Una cosa sola ha quella forma: **ritrasmissione**. E spiega da sola tutti e
tre i numeri del percorso diretto — metà della velocità utile, il doppio di
CPU per gigabyte *consegnato*, e 2,9 volte il tempo di kernel. Il lavoro era
vero. Veniva speso su byte buttati via e rispediti.

**Dove venivano buttati.** Nel codice esiste una funzione che allarga i buffer
dei socket UDP, e nel suo stesso commento c'è scritto da sempre che un socket
non allargato limita un flusso QUIC a circa «buffer diviso tempo di andata e
ritorno». Chiamarla era compito di chi creava il socket — e l'**unico** punto
che crea il socket condiviso del server non la chiamava. Non è un socket
secondario: è quello che riceve il traffico diretto di *tutti* i tunnel del
processo (vhost, pubblici e jump host), ed è il lato **ricevente** di ogni
scaricamento, perché i byte arrivano dal forwarder via QUIC ed escono verso
chi si è collegato via TCP. Tutti gli altri socket UDP del file chiedevano 16
MiB. Quello chiedeva niente, e restava al valore di default del kernel:

```
   skmem:(r0,rb212992,t0,tb212992,f0,w0,o0,bl0,d0)
```

**208 KiB**, e il server non lo diceva — non aveva niente da dire, perché non
aveva chiesto niente. Per confronto, il *client* della stessa connessione
scrive nel log `effective_recv=8388608`: i due capi dello stesso percorso QUIC
differivano di **quaranta volte**, e quello piccolo era il ricevente. A 111
MB/s, 208 KiB contengono **1,9 millisecondi** di traffico: qualunque ritardo
più lungo di così è un pacchetto perso. E il container non ha i privilegi di
rete (`caps=[]`), quindi nemmeno la scorciatoia per forzare il kernel era
disponibile.

**La correzione** sposta la chiamata **dentro** i due costruttori di endpoint
QUIC e la toglie da tutti e quattro i punti che la facevano. È deliberato, ed
è il punto: aggiungere un quinto punto avrebbe corretto *questo* caso, mentre
metterla nei costruttori rende la garanzia **strutturale** — in questo codice
non esiste un modo di ottenere un endpoint QUIC che non passi da lì, quindi
nessun endpoint futuro potrà nascere con i buffer stretti. Dopo, sulla stessa
macchina e con lo stesso comando: `rb8388608 tb8388608`, quaranta volte tanto,
e il server ora **dice** quello che ha ottenuto, indicando il rimedio per la
parte che non può risolvere da solo (`sysctl -w net.core.rmem_max=16777216
net.core.wmem_max=16777216`).

Il controllo `T-PUB-UDPBUF` avvia un server vero con `--udp` e rilegge i
buffer **dal kernel**, non dal log, e fallisce tutte e tre le asserzioni se la
chiamata viene rimossa.

> **Che cosa era dimostrato prima del nuovo deploy.** Dimostrato: il socket era
> al valore di default, la correzione lo moltiplica per quaranta, il server era
> muto e ora non lo è, e il controllo va rosso senza la correzione. Dimostrato
> anche: il braccio diretto riceveva 1,78 volte i byte che consegnava, mentre
> quello relay coincideva allo 0,2 %. **Non** dimostrato, a quel punto: che il
> primo fatto causasse il secondo. L'inferenza era forte — un buffer che
> contiene 1,9 ms di traffico è esattamente la cosa che perde pacchetti sotto
> carico, e nella misura non si muoveva niente altro — ma il server girava
> ancora con la versione precedente, quindi i numeri "dopo" erano registrati
> come **in attesa**, non previsti. È l'unica cosa onesta da fare con una
> spiegazione causale che non è ancora stata messa alla prova.

#### Il "dopo": l'inferenza ha retto

Il server è stato aggiornato alla versione `1.0.0 - main - 062a1095` e la
stessa prova è stata rifatta, identica, sullo stesso percorso, con le stesse
finestre da 20 secondi e le stesse quattro connessioni.

Prima di misurare, `srv/verify_fixes.sh` ha verificato che la correzione fosse
davvero in vigore nel processo in esecuzione, leggendola **dal kernel** e non
da una riga di registro:

```
  socket: rb=8388608 tb=8388608   net.core.rmem_default=212992
```

Sono 8 MiB e non i 16 richiesti, perché il tetto di sistema di questa macchina
è 8 MiB e un processo senza privilegi non lo può superare — l'avviso lo dice e
indica il comando. Restano **quaranta volte** quello che il socket aveva prima.

Poi la misura:

| grandezza | prima (`dbcc645a`) | dopo (`062a1095`) |
| --- | --- | --- |
| diretto: byte entrati ÷ byte usciti | **1,78×** | **1,009×** |
| relay: byte entrati ÷ byte usciti | 1,002× | 1,006× |
| velocità diretto, 4 connessioni | 111 MB/s | **143,43 MB/s** (+29 %) |
| velocità relay, 4 connessioni | 213 MB/s | 200,85 MB/s |

In questa prova chi manda e chi riceve sono la stessa macchina in regione, per
cui un server sano fa uscire quasi esattamente quello che gli entra: **1,0 è il
valore giusto, e il braccio relay è il testimone che lo conferma**. Il braccio
diretto adesso segna 1,009 contro l'1,006 del relay: l'1,78× non è ridotto, è
**sparito**. L'inferenza ha retto — il socket di ricezione troppo piccolo
perdeva pacchetti, QUIC li rimandava, e quel singolo meccanismo pagava tutta la
velocità mancante.

Quello che la correzione **non** ha fatto è ribaltare la classifica: sul
percorso pulito in regione il relay resta davanti (143,43 contro 200,85, cioè
0,71). Ma il divario era 0,52 e ora è 0,71, quindi circa **due quinti** di
quello che la campagna aveva attribuito a QUIC erano questo difetto, e il resto
è QUIC per davvero. La raccomandazione su `--udp` regge quindi sui suoi meriti
e non su un difetto — che è esattamente ciò che la marcatura PROVVISORIA serviva
a verificare.

Una cosa va detta perché non venga confusa con lo stesso difetto: i socket
**TCP** vanno bene così come sono. Il codice imposta `TCP_NODELAY` e il
keepalive e **non** imposta i buffer, perché farlo disattiva la taratura
automatica del kernel e blocca il socket al tetto di sistema — proposta già
esaminata e **respinta come dannosa** nella verifica del gateway SSH. UDP non
ha nessuna taratura automatica, ed è esattamente per questo che quella
funzione esiste.
---

## 9. Dalla workstation: la topologia del mondo reale

**Che cosa abbiamo provato.** Tutte le prove precedenti hanno il consumatore
dentro AWS, nella stessa regione del server: è la topologia che serve per
misurare *il tunnel* senza che la rete domestica nasconda il risultato. Ma non
è come si usa un tunnel pubblico. Nell'uso reale il programma da esporre gira
su una macchina qualunque e chi si collega alla porta pubblica sta **fuori**
da AWS, su una linea domestica.

Questa prova ribalta la parte finale: il forwarder resta sulla macchina di
prova in AWS, il consumatore è questa workstation su collegamento domestico
senza fili.

> **Questi numeri non vanno mai citati insieme ai precedenti.** Qui il collo
> di bottiglia è **la linea**, e più sotto è dimostrato invece che supposto:
> una sola connessione la satura già. La campagna vhost dava il collegamento
> radio di questa workstation intorno ai 45 MB/s; va corretto in una
> direzione, perché questa prova ha misurato **68-72 MB/s in caricamento**
> contro 26-46 in scaricamento, con i contatori di credito dell'istanza a
> zero in entrambe le direzioni. Il limite è della radio, ed è asimmetrico al
> contrario di come lo è normalmente una linea di casa.
>
> Quello che questa topologia misura bene è la **latenza** e il fatto che il
> percorso funzioni davvero da fuori AWS. **Non** misura bene il rapporto fra
> i due trasporti — la versione precedente di questa sezione lo sosteneva, e
> più sotto è spiegato perché è falso.

**Che cosa è successo.**

Prima di tutto, il tempo di andata e ritorno verso il server, misurato con
una vera stretta di mano TCP (il ping ICMP verso questo indirizzo è filtrato,
e la prima versione della prova stampava una riga vuota): **minimo 37,36 ms,
mediana 40,15 ms, massimo 46,28 ms**. È la distanza, e sotto quella non si va.

*Scaricamento, quattro coppie, ordine alternato, 96 MiB su 4 connessioni:*

| coppia | relay MB/s | diretto MB/s | rapporto |
| --- | --- | --- | --- |
| 1 | 30,60 | 38,36 | 1,254 |
| 2 | 27,46 | 33,84 | 1,232 |
| 3 | 25,51 | 27,34 | 1,072 |
| 4 | 23,71 | 29,50 | 1,244 |
| **mediana** | | | **1,238** |

*Caricamento, stesse condizioni:*

| coppia | relay MB/s | diretto MB/s | rapporto |
| --- | --- | --- | --- |
| 1 | 71,64 | 67,81 | 0,947 |
| 2 | 70,43 | 62,89 | 0,893 |
| 3 | 72,43 | 68,46 | 0,945 |
| 4 | 67,74 | 74,50 | 1,100 |
| **mediana** | | | **0,946** |

*Latenza, ottanta prove per trasporto, ogni prova una connessione nuova:*

| trasporto | mediana | 95° | 99° | errori |
| --- | --- | --- | --- | --- |
| relay | 63,512 ms | 72,751 ms | **95,371 ms** | 0 su 80 |
| diretto | 63,483 ms | 71,827 ms | **73,419 ms** | 0 su 80 |

E la vista del server, che è quella che dice su quale strada è passato
davvero ogni braccio: il tunnel diretto ha aperto **113 flussi diretti, zero
ricadute sul relay**, percorso `diretto` a ogni lettura; il tunnel relay ha
aperto zero flussi diretti. Nessun braccio ha misurato una strada diversa da
quella che dichiara.


**Dov'è il collo di bottiglia.**

**Nella linea, e la prova è che una sola connessione la satura già.** Questa
è la conclusione importante della prova, ed è arrivata solo perché il primo
risultato non è stato creduto.

Il primo risultato diceva: in scaricamento il percorso diretto **vince** di
1,238×. È il contrario di quello che dice la misura dentro AWS (§3: vince il
relay di 1,51×), quindi qualcosa doveva essere diverso con un consumatore
lontano. Sono state proposte due spiegazioni e **misurate entrambe. Sono
cadute entrambe.**

1. *Le connessioni si ostacolano a vicenda sul relay.* Sul percorso normale
   tutte e quattro le connessioni viaggiano dentro **una sola** connessione
   TCP, quindi un consumatore a 40 ms che svuota lentamente potrebbe
   costringerle a condividere una sola finestra. Se è così, `--carriers 4`
   deve chiudere il divario. Misurato, tre giri con ordine alternato:
   **0,754 / 0,889 / 1,027, mediana 0,889**. Le corsie non chiudono il
   divario: costano. Stesso segno della misura in regione e della campagna
   vhost (0,941).
2. *C'è un limite per singola connessione.* Se il limite fosse una finestra
   per connessione, allora aggiungendo connessioni il totale deve **salire**
   mentre il valore per connessione resta fermo. Misurato, con i byte per
   connessione tenuti costanti:

   | connessioni | relay totale | relay per conn. | diretto totale | diretto per conn. |
   | --- | --- | --- | --- | --- |
   | 1 | 35,23 | 35,23 | 33,38 | 33,38 |
   | 2 | 36,13 | 18,07 | 45,58 | 22,79 |
   | 4 | 27,92 | 6,98 | 19,21 | 4,80 |
   | 8 | 29,30 | 3,66 | 31,94 | 3,99 |

   Il totale è **piatto** (se cambia, scende) e il valore per connessione
   scende come 1/N. **Una connessione sola satura già il collegamento.**

E qui cade anche il ribaltamento di prima. Se una connessione satura la
linea, e se l'ultimo tratto — quello verso la workstation — è **TCP normale
per entrambi i trasporti** (la scelta del trasporto riguarda solo il tratto
forwarder→server, dentro AWS), allora il trasporto non può fare differenza
qui. Rifatta a **una sola connessione**, sei coppie: **1,039 / 0,690 / 1,121 /
1,130 / 0,847 / 0,837, mediana 0,943** — tre sopra uno e tre sotto. Il
vantaggio del diretto **spariste**. Era il regime a quattro connessioni su una
linea radio variabile, non una proprietà del tunnel; è registrato come
artefatto e non va citato come verdetto.

Quanto è variabile quella linea: la **stessa** configurazione relay, misurata
sette volte in un'ora, ha dato 16,71 / 17,72 / 23,71 / 25,50 / 25,51 / 27,46 /
30,60 MB/s, e più tardi nella stessa ora 33,70 / 36,16 / 38,83. Un fattore
1,83 sulla stessa cosa. Per questo l'unica quantità utilizzabile è il
**rapporto appaiato** (i due bracci misurati uno dopo l'altro, a 45-75 s di
distanza, con l'ordine che si alterna) — e anche quello dà il segno, non la
misura: gli otto rapporti in scaricamento vanno da 0,694 a 2,607.

**L'asimmetria fra scaricare e caricare è della linea, non del tunnel.** Si
scarica a 26-35 MB/s e si carica a 68-72, nello stesso tunnel e negli stessi
minuti. Il sospetto ovvio era il credito di banda dell'istanza, perché lo
scaricamento è traffico *in uscita* dal server ed è il disturbo dominante di
tutta la campagna. È stato misurato invece che supposto, leggendo i due
contatori del server subito prima e subito dopo ogni braccio:
**zero eccedenze in entrambe le direzioni, in entrambi i bracci**. Questa
linea radio (circa 290-510 Mbit/s) resta sotto la soglia oltre la quale
l'istanza limita, quindi in questa topologia il disturbo principale di tutte
le altre prove semplicemente **non c'è**. L'asimmetria è della linea di casa.

**Quello che invece questa topologia misura bene è la latenza, e il risultato
è pulito**: le due mediane coincidono a **0,03 ms**. A 40 ms di distanza la
scelta del trasporto è invisibile nel caso tipico — un tunnel pubblico verso
un consumatore lontano costa quello che costa la rete, e niente di più. Le
code no: il 99° percentile del relay è 21,95 ms peggiore. È una prova su
cento, ed è riportata come osservazione e non come consiglio, perché la coda
del percorso diretto *dentro* la regione (§5) punta nella direzione opposta.

Dei 63,5 ms di mediana, 40,15 sono la stretta di mano verso il server: il
resto è il secondo giro di andata e ritorno, quello che apre il flusso verso
il forwarder. Sono due giri, ed è tutto lì — su una porta pubblica non c'è
TLS, a meno che il tunnel non abbia chiesto `--https`.


## 10. Conclusioni e raccomandazioni

### 10.1 Le tre cose da ricordare

1. **Il percorso UDP diretto non è un acceleratore.** Su rete pulita è la
   scelta più lenta, in tutte le prove tranne una (molte richieste
   contemporanee). Diventa indispensabile quando la rete perde pacchetti,
   dove il divario arriva a 132 volte a favore del diretto. La domanda
   giusta non è "quanto traffico faccio" ma "quanto è buona la mia rete".
2. **Le corsie parallele hanno un punto ottimale, e non è il massimo.**
   Quattro corsie danno il 44 % in più di una; otto danno meno di quattro.
3. **Tredici difetti del prodotto sono stati trovati misurando, non leggendo
   il codice.** Due erano gravi, e nessuno dei due si sarebbe visto in un test
   in memoria: uno richiedeva un server vero che non leggesse un canale,
   l'altro richiedeva di far scorrere abbastanza traffico da riempire un
   buffer di controllo. Lo stesso vale per gli strumenti: dei quattordici difetti
   dell'harness, due **misuravano zero stampandolo come una misura vera** — e
   il secondo è la ragione per cui la fase dalla workstation ora si rifiuta di
   misurare un tunnel registrato che non muove byte, invece di fidarsi di chi
   legge il risultato.

### 10.2 Raccomandazioni per chi gestisce il server

* **Dimensionare `--max-carriers` sull'host, non al massimo.** Il server di
  prova gira con 1024 su una macchina da 903 MiB. Non si rompe niente, ma il
  budget di memoria del percorso diretto viene diviso per quel numero e le
  finestre finiscono al **minimo** consentito (16 MiB per connessione,
  1 MiB per flusso, contro i 256/16 MiB di riferimento). Su questa rete non
  è stato un limite, ma su collegamenti lenti lo diventerebbe. Un valore fra
  16 e 64 è più che sufficiente: l'ottimo misurato per un singolo tunnel è 4.
* **Tenere impostato `--udp-memory-budget`.** È l'unico tetto complessivo
  alla memoria del percorso diretto, non costa nulla quando non viene
  raggiunto, e un rifiuto significa "questa connessione usa il relay", mai
  "questa richiesta fallisce".
* **Guardare `udp_direct_slots_available` accanto a
  `direct_budget_refusals`** nella pagina delle metriche: il contatore dice
  che il tetto è stato superato in passato, l'indicatore dice che è pieno
  adesso. È la correzione P-11 di questa campagna.
* **Aggiornare il server prima dei client.** La correzione P-4 è compatibile
  in entrambe le direzioni, ma la combinazione "client nuovo, server
  vecchio" è esattamente quella che ha fatto emergere P-9. Oggi il client si
  difende da solo — dopo 10 secondi smette di battere e lo scrive nel log —
  e proprio per questo non va usato come strategia di aggiornamento.

### 10.3 Che cosa resta aperto

* **Le finestre al minimo (sopra) non sono state misurate ad alto ritardo.**
  A ~1 ms di distanza non limitano nulla; su un collegamento intercontinentale
  potrebbero. Serve una prova dedicata, non un'altra lettura del codice.
* **Il gateway SSH resta a una corsia e senza UDP.** È una proprietà del
  protocollo SSH, non un difetto di `bore`: chi ha bisogno di più corsie o del
  percorso diretto deve installare il binario.
* **La finestra di perdita durante un blackout UDP è il timeout di
  inattività QUIC** (circa 10 secondi, configurabile): misurata, non è un
  difetto, e nessuna scadenza sull'apertura può accorciarla.
---

## 11. Come rifare queste misure fra un mese

Niente in questa campagna dipende da un nome di macchina, da una porta o da
una credenziale scritta nel repository: le coordinate stanno in **un solo
file** fuori dal repository (`~/.config/bore-perf/env.sh`, permessi `600`), e
tutti gli script le trovano da lì. Per ripuntare l'intera campagna su un altro
deployment si modifica quel file e nient'altro.

```bash
# 0. coordinate (mai nel repository; permessi 600)
cp scripts/perf/staging/env.sh.example ~/.config/bore-perf/env.sh && chmod 600 $_
$EDITOR ~/.config/bore-perf/env.sh

# 1. porta la VM di prova nello stato che la campagna si aspetta (idempotente)
scripts/perf/staging/provision.sh

# 2. metti in opera sul server la build da misurare, e verifica i tre attori
#    (redeploy.sh termina eseguendo verify_fixes.sh, che legge dal kernel del
#     server se le correzioni P-12 e P-13 sono davvero in vigore nel processo
#     in esecuzione: non riavvia nulla e non muove traffico, quindi si può
#     lanciare da solo in qualsiasi momento)
scripts/perf/staging/srv/redeploy.sh
scripts/perf/staging/srv/verify_fixes.sh

# 3. tutta la campagna lato VM, rigorosamente in serie (ore)
scripts/perf/staging/res/start_samplers.sh
ssh <vm> '~/pub/pub_driver.sh'        # oppure un sottoinsieme: '~/pub/pub_driver.sh p1 conc eff'
scripts/perf/staging/res/stop_samplers.sh

# 4. la topologia del mondo reale, dalla workstation
scripts/perf/staging/pub/ws_pub.sh

# 5. riduci una corsa raccolta alle tabelle di questo documento
scripts/perf/staging/pub/summarize.sh out/pub-<data>
```

I **controlli di correttezza** sono un'altra cosa e non hanno bisogno di alcun
deployment: girano in uno *network namespace* su qualunque Linux, in pochi
minuti.

```bash
cargo build --release --features ssh-gateway,vpn
scripts/perf/public_idle_window.sh        # tutta la matrice del percorso pubblico
```

Tre avvertenze che valgono più di qualunque numero, perché sono le cose che
hanno prodotto dati falsi in questa campagna:

1. **Mai due armamentari `netns` in parallelo.** Condividono i nomi
   `ns0`/`ns1`/`ns2`: la pulizia iniziale di uno cancella i namespace
   dell'altro a metà corsa e fabbrica fallimenti che non esistono.
2. **Sempre confronti appaiati e alternati.** Il credito di banda di AWS
   deriva nel tempo: due misure prese a dieci minuti di distanza non sono
   confrontabili, due misure appaiate e alternate sì.
3. **Un armamentario che stampa zeri non è un risultato, è un guasto.** Due
   volte in questa campagna una prova ha prodotto tabelle di zeri con la forma
   esatta di una misura vera (H-7 e H-8 in §1). Prima di leggere un numero,
   guarda se la prova ha davvero fatto quello che dice.

## 12. Ricettario: come far partire un tunnel pubblico

Questa sezione non dice "che cosa abbiamo misurato" ma **quali comandi dare**.
Ogni riga è giustificata da una misura di questa campagna, indicata fra
parentesi.

Un tunnel pubblico si apre con `bore local`: il server assegna (o ti concede)
una porta pubblica, e tutto ciò che arriva su quella porta finisce sulla porta
locale che hai indicato. Ci sono tre modi per far partire il forwarder —
binario nativo, immagine Docker, `ssh -R` verso il gateway SSH — e le prime
due **rendono uguale** (sezione 4), quindi si sceglie per comodità.

### 12.1 La regola in una riga

> **Le corsie (`--carriers`) si scelgono in base a quante connessioni
> viaggiano insieme. Il percorso diretto (`--udp`) si sceglie in base a
> quanto è buona la rete, non in base all'applicazione.**

| il tuo caso | corsie | UDP |
| --- | --- | --- |
| un servizio usato da più persone o da un browser (molte connessioni insieme) | **4** | spento |
| un solo trasferimento grande per volta | **1** (predefinito) | spento |
| rete che perde pacchetti (radio, 4G, linea satura, VPN scadente) | **1** | **acceso** |
| moltissime connessioni tenute aperte insieme | **4** | **acceso** |
| non puoi installare niente sulla macchina | `ssh -R` | non disponibile |

### 12.2 I comandi, pronti da copiare

Sostituisci `8080` con la porta della tua applicazione, `SERVER` con
l'indirizzo del server e `PORTA` con la porta pubblica che vuoi (oppure `0`
per farla scegliere al server). **Il segreto non va mai scritto in un file
versionato**: mettilo in una variabile d'ambiente o in un file con permessi
`600`.

**A. Servizio web o API usati da più persone — binario nativo**

```bash
export BORE_SECRET='...'          # letto da --secret
bore local 8080 --to SERVER --port PORTA --carriers 4
```

**B. Un solo trasferimento grande per volta (backup, sincronizzazione)**

```bash
bore local 8080 --to SERVER --port PORTA        # una corsia: è il default
```

**C. Rete che perde pacchetti**

```bash
bore local 8080 --to SERVER --port PORTA --udp
# il server deve girare con --udp e avere la porta QUIC raggiungibile
```

**D. Con Docker (stesse prestazioni del binario)**

```bash
docker run -d --name bore-client --network host \
  -e BORE_SERVER=SERVER -e BORE_LOCAL_PORT=8080 -e BORE_SECRET="$BORE_SECRET" \
  -e BORE_CARRIERS=4 \
  ghcr.io/manprint/bore:client local --port PORTA
```
Con `--udp` serve **l'immagine root** (`:client`, quella usata qui): Docker
toglie tutte le capability a un utente non root e il client non riesce ad
allargare i buffer UDP di sistema.

**E. Senza installare niente, con OpenSSH**

```bash
ssh -R PORTA:localhost:8080 utente@SERVER
```
Il gateway SSH non supporta né `--udp` né le corsie: è una proprietà del
protocollo. Costa circa 0,75 ms in più per connessione nuova e, in
scaricamento, è circa 1,9 volte più lento di una corsia nativa (sezione 4).
Non usare `-N`: senza sessione il server non può mostrarti il riepilogo del
tunnel.

### 12.3 Che cosa NON fare

* **Non alzare le corsie "a caso".** Oltre il punto migliore peggiorano: a 8
  corsie questa campagna misura meno banda che a 4, in entrambe le direzioni
  (sezione 3.4).
* **Non accendere `--udp` per andare più veloce.** Su rete pulita è la scelta
  più lenta, di 1,5 volte in scaricamento (sezione 3.1).
* **Non giudicare un tunnel da una singola misura.** Su queste istanze il
  credito di banda si esaurisce e la misura successiva ne paga il conto: per
  questo tutti i confronti di questa campagna sono a coppie alternate
  (sezione 2).
