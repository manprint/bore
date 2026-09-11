# Revisione finale delle prestazioni di `bore vhost`

**Data:** 11 settembre 2026
**Oggetto:** verifica sul campo dei miglioramenti introdotti dal piano
`plan_VhostEnhancements`, confronto con la campagna di riferimento del
10 settembre 2026, e analisi dei limiti di ogni prova.

---

## Come leggere questo documento

Questo documento è pensato per essere letto **anche da chi non è tecnico**.
Ogni sezione segue sempre lo stesso schema:

1. **Che cosa abbiamo provato** — in parole semplici.
2. **Che cosa è successo** — i numeri, con il confronto prima/dopo.
3. **Dov'è il collo di bottiglia** — qual è la cosa che ha impedito di andare
   più veloce, e se dipende da `bore` oppure no.

Il punto 3 è quello che il committente ha chiesto esplicitamente, ed è anche il
più importante: **un numero senza il suo limite non dice nulla.** "300 Mbit/s"
può essere un ottimo risultato o un disastro a seconda di che cosa avrebbe
potuto fare il collegamento.

> **Se hai poco tempo:** la sintesi di una pagina è subito qui sotto, e i
> **comandi pronti da copiare** per far partire un forwarder alle massime
> prestazioni sono nel **§12, "Ricettario"**, in fondo. Tutto il resto è la
> dimostrazione.

### Tre parole da sapere prima di cominciare

| termine | in parole semplici |
| --- | --- |
| **relay (TCP)** | il percorso normale: i dati passano dal server centrale, su connessioni TCP protette da TLS. È la modalità predefinita. |
| **direct (QUIC/UDP)** | un percorso alternativo in cui client e server si parlano via UDP con il protocollo QUIC, cercando di evitare un passaggio. Si attiva con `--udp`. |
| **carrier** | una delle connessioni parallele che il tunnel apre verso il server. `--carriers 8` significa otto "corsie" invece di una. Serve a evitare che un download pesante blocchi le richieste piccole. |

### Il vocabolario dei limiti

Nel documento useremo sempre gli stessi tre nomi per i colli di bottiglia:

* **Limite del collegamento** — la rete fisica è satura. Non si può fare di
  meglio senza cambiare linea. *È il risultato migliore possibile.*
* **Limite dell'istanza (AWS)** — il server è una macchina piccola e
  "burstable": Amazon le assegna un credito di banda che si esaurisce.
  Non dipende dal software.
* **Limite dell'applicazione** — il collo di bottiglia è nel programma:
  o in `bore`, o nel programma servito (nel nostro caso `dufs`).

Solo il terzo tipo è un problema su cui si può lavorare scrivendo codice, e
solo la parte che riguarda `bore` è nostra.

---

## In sintesi (una pagina)

**Che cosa è stato fatto.** Sono state ripetute integralmente le prove della
campagna del 10 settembre contro il server aggiornato, e sono state aggiunte
tre prove che la campagna precedente non aveva mai fatto: la workstation di
casa come utente finale, un'applicazione vera (`dufs`) al posto del programma
di prova, e tutte e tre le modalità di avvio del client.

**I difetti operativi sono risolti.** Quattro su sei completamente, uno
parzialmente, uno per scelta lasciato disattivo:

* il tunnel bloccato non tiene più occupato il sottodominio per sempre
  (si libera in 60 secondi e si può riprendere subito);
* un servizio spento risponde `502` in 15 millisecondi invece di chiudere la
  connessione senza spiegazioni;
* il log è passato da 718 avvisi inutili su 786 a **3 avvisi in un'ora**;
* il pannello di amministrazione mostra le impostazioni che prima nascondeva.

**Le prestazioni sotto carico misto sono il vero salto.** Con un download
pesante in corso, una richiesta piccola è passata da 3,4 a **2,7
millisecondi** — cioè al livello che si aveva a tunnel completamente
scarico — e il numero di richieste servite è cresciuto fino a **4 volte**. Il
documento precedente concludeva che "nessuna configurazione torna ai 2,5 ms di
riposo". Ora ci torna.

**La banda della linea di casa è saturata in download.** 59 MB/s
(496 Mbit/s) attraverso il tunnel, contro i 46 MB/s del miglior server
pubblico raggiungibile da questa linea. In upload si arriva a 93,7 MB/s
(786 Mbit/s), l'89 % del massimo misurabile, comunque il 18 % più veloce di un
trasferimento `ssh` diretto.

**Il costo non è aumentato.** Il lavoro di smistamento introdotto dal piano
costa **zero** in secondi di CPU per gigabyte trasferito (6,85 contro 6,78
prima).

**Il difetto residuo è stato ridotto del 60 %, con una misura alla mano.**
Quando la rete UDP smette di funzionare, la prima richiesta in corso si perde.
Era una finestra di 10 secondi. Si è dimostrato che quella finestra **coincide
esattamente** con un tempo di attesa interno che prima era fisso nel codice; ora
è configurabile, e portandolo a 4 secondi la finestra scende a 4 secondi,
misurati. Tutte le richieste successive sono servite normalmente in ~12
millisecondi, e il ritorno al percorso veloce è automatico in ~4 secondi. La
finestra non si azzera: una richiesta già affidata al canale può solo aspettare
che il canale venga dichiarato morto.

**La configurazione del server è stata sistemata e documentata riga per riga.**
Due parametri sono stati misurati e decisi: uno è stato rimosso perché non
faceva nulla, l'altro è stato attivato perché riduce la memoria nel caso
peggiore da 340 a 42 MiB senza costare banda. Un blocco di commenti che
diceva il contrario della configurazione attiva è stato riscritto.

**Tutto è ripetibile.** Documentazione e attrezzatura di prova — 41 script, un
compose di riferimento e le istruzioni — sono nel repository, senza un solo
indirizzo o credenziale dentro: per rifare la campagna fra un mese su un'altra
macchina basta compilare un file di configurazione.

**Che cosa resta aperto.** Gli ultimi 4 secondi della finestra di cui sopra, e
un limite della macchina di staging — non del software — che condiziona tutte
le misure di banda ripetute.

**Il collo di bottiglia più frequente non è `bore`.** Nell'ordine: la linea
WiFi di casa, il credito di banda dell'istanza AWS economica, la CPU del
server (2 sole CPU), e l'applicazione servita. Il tunnel è risultato essere il
fattore limitante in **un solo caso** su tutti quelli provati: la prima
richiesta durante un blackout UDP — che ora dura 4 secondi invece di 10.

---

## 1. I problemi operativi: cosa era rotto e cosa è stato risolto

La campagna di riferimento aveva elencato una serie di difetti. Li abbiamo
riprovati **tutti**, sullo stesso ambiente, con la stessa procedura.

| # | Problema | Prima | Dopo | Esito |
| --- | --- | --- | --- | --- |
| **F-1** | Un client bloccato (processo congelato, portatile sospeso) teneva occupato il proprio sottodominio **per sempre**: nessun altro poteva riusarlo fino al riavvio del server. | ancora occupato dopo 3 minuti; nuovo tentativo rifiutato con `subdomain in use` | rilasciato entro 60 secondi; il nuovo tentativo **funziona** | **RISOLTO** |
| **F-12** | Se il programma dietro al tunnel era spento, il browser riceveva una connessione chiusa senza risposta: indistinguibile da "il tunnel è morto" o "internet non va". | `http=000` | **`502` in 15,6 ms**, con messaggio leggibile | **RISOLTO** |
| **F-3** | Il log del server era invaso da avvisi inutili: 718 righe su 786 erano semplicemente "il browser ha chiuso la connessione". | 718 avvisi su 786 | **19 avvisi in tutta la vita del container**, su **53 238 righe** e 0 errori — e tutti e 19 sono eventi reali | **RISOLTO** |
| **F-6** | Il pannello di amministrazione dichiarava assenti impostazioni che invece erano attive. | intestazioni HTTP non mostrate | tutte e 7 presenti in `/admin/api/v1/config` | **RISOLTO** |
| **F-14** | Durante un blackout della rete UDP, la prima richiesta restava appesa ~10 secondi e un contatore diagnostico mentiva (saliva da 1 a 12 pur non avendo aperto nulla). | 9,90 s di attesa, contatore falso | contatore **onesto** (congelato, corretto); dalla seconda richiesta in poi tutto normale in ~12 ms; **la prima richiesta costa ancora 10 s** | **QUASI RISOLTO** — vedi §6 |
| **F-13** | Troppi lettori lenti su un tunnel UDP potevano consumare mezzo gigabyte di RAM sul server e far fallire tunnel estranei. | 536,8 MiB di RAM, richieste altrui in timeout | riprodotto **identico** (483,6 MiB) — perché la protezione esiste ma **non è attiva per impostazione predefinita** | **DISPONIBILE, NON ATTIVO** — vedi §5 |

### Il risultato più netto: il client bloccato

È il caso più fastidioso per chi usa il servizio tutti i giorni, perché non
c'è modo di accorgersene se non provando. Abbiamo congelato il processo del
client mentre un trasferimento era in corso (la connessione di rete resta viva
e risponde a livello TCP: è il caso peggiore, perché il server non ha modo di
capire dal socket che l'altro capo è morto).

| momento | prima | dopo |
| --- | --- | --- |
| dopo 20 s | occupato | occupato |
| dopo 40 s | occupato | occupato |
| dopo 60 s | **ancora occupato** | **liberato** |
| dopo 90 s | **ancora occupato** | liberato |
| dopo 180 s | **ancora occupato** | liberato |
| riprovare a registrarsi mentre è bloccato | **rifiutato** | **accettato** |

Prima l'unico rimedio era riavviare il server. Ora il sistema si sblocca da
solo, ed è esattamente il comportamento che il canale SSH aveva già.

### Il log: da 786 avvisi a 19, tutti veri

A fine campagna — dopo ore di traffico reale, trasferimenti interrotti,
richieste parallele e processi uccisi a metà, cioè esattamente il traffico che
prima generava gli avvisi — il log completo del server contiene:

* **53 238 righe**
* **19 avvisi**, **0 errori**
* **0 occorrenze** dell'avviso che prima era il 91 % del totale

E i 19 avvisi sono tutti eventi che un amministratore vuole vedere:

| quanti | avviso | è reale? |
| --- | --- | --- |
| 9 | handshake TLS fallito da indirizzi esterni | sì — scanner di internet, non traffico nostro |
| 3 | tunnel bloccato, sottodominio recuperato dopo 60 s | sì — **è la correzione F-1 che funziona** |
| 3 | sottodominio già in uso | sì — è il nostro test in cui quattro client se lo contendono |
| 2 | sessione SSH non responsiva, chiusa | sì — è la protezione contro i client bloccati |
| 1 | avviso di avvio sulla memoria del percorso UDP | sì — una volta all'avvio |

Il salto pratico: prima trovare un problema vero nel log significava cercarlo
fra centinaia di righe irrilevanti. Ora **tutti gli avvisi di una giornata
stanno in una tabella**.

### Verifica di compatibilità (importante per la messa in produzione)

Il piano ha aggiunto messaggi nuovi al protocollo. Un client **vecchio** che
parla con il server **nuovo** deve continuare a funzionare, altrimenti
l'aggiornamento del server romperebbe tutti i tunnel esistenti.

Provato con il binario precedente al piano: si registra, serve traffico a
129,68 MB/s, **sopravvive oltre i 95 secondi da fermo** (cioè non viene
scollegato dal nuovo meccanismo di pulizia, che si applica solo a chi dichiara
di saperlo gestire) e sotto carico pesante non riceve mai i messaggi nuovi che
non saprebbe interpretare. **Compatibilità confermata.**

### La caduta di UDP: cosa succede davvero, e cosa abbiamo migliorato

Un tunnel `--udp` usa un canale diretto UDP e tiene **sempre acceso** anche il
canale di riserva TCP. Domanda operativa: se UDP smette di funzionare — un
firewall che cambia, una rete mobile, un operatore che filtra — il passaggio al
canale di riserva avviene senza disservizio?

Provato spegnendo UDP di netto, in **entrambe le direzioni**, sotto un tunnel
attivo:

| momento | esito della richiesta |
| --- | --- |
| prima | 113,71 MB/s, canale diretto |
| **1ª richiesta dopo il blackout** | **persa, dopo 10,01 secondi** |
| 2ª richiesta | **servita in 12,5 ms**, sul canale di riserva |
| dalla 3ª in poi | servite in ~12 ms |
| traffico pesante durante il blackout | **170,91 MB/s** — più veloce di prima |
| a UDP ripristinato | torna da solo sul diretto in ~4 secondi |

Quindi: **il ripiego funziona ed è trasparente, ma non è a costo zero.** C'è
una finestra in cui le richieste vengono perse, e quella finestra durava
**10 secondi**.

Il motivo, individuato con precisione: quando il partner tace, aprire il canale
e scrivere il byte di avvio **riescono comunque** in locale (non serve nessun
viaggio di andata e ritorno), quindi la richiesta è già impegnata su quel
canale e può solo aspettare che la connessione venga dichiarata morta. Questo
avviene allo scadere del *timeout di inattività*, che valeva 10 secondi.

**Cosa abbiamo fatto.** Quei due valori erano scritti nel codice e non
modificabili. Ora sono configurabili, e abbiamo misurato la relazione: la
finestra di perdita **è esattamente il timeout**.

| timeout impostato | finestra di perdita misurata |
| --- | --- |
| 10 s (valore di serie) | 10,0019 s |
| 6 s | 6,0020 s |
| 4 s | 4,0011 s |
| 2 s | 2,0021 s |

Tre cose rendono questa una raccomandazione e non un'ipotesi:

1. **Basta cambiarlo sul server.** Le due estremità negoziano il valore più
   basso, quindi l'amministratore che controlla il server ottiene la finestra
   corta anche con client non aggiornati. Verificato: **4,0010 s** con un client
   lasciato ai valori di serie.
2. **Non abbatte connessioni sane.** Con perdita di pacchetti al 10 % e al 30 %
   per un minuto continuo, il canale diretto non è mai caduto, né col valore di
   serie né con quello ridotto (0 ripieghi su 12 rilevazioni in tutti i casi).
   Al 50 % la prova non è più interpretabile perché a quel punto si rompe anche
   tutto il resto dell'apparato di misura, non il canale diretto.
3. **Il ripiego resta identico**: dalla seconda richiesta in poi, ~1 ms.

**Raccomandazione:** `BORE_DIRECT_QUIC_IDLE_MS=4000` sul server porta il caso
peggiore da 10 a 4 secondi, cioè **−60 %**, al prezzo di un pacchetto di
controllo in più ogni 1,3 secondi per connessione inattiva. Lo lasciamo
**disattivato per impostazione predefinita**: cambiare un valore di serie sulla
base di una sola campagna di misure è una decisione che spetta a chi gestisce
il servizio, e il caso che accorcia è comunque raro (un tunnel che non perde
mai UDP non arriva mai a questo codice).

---

## 2. Il miglioramento principale: le richieste piccole non aspettano più i download

### Che cosa abbiamo provato

È la situazione più comune in assoluto e anche quella che funzionava peggio:
mentre è in corso **un download pesante**, arrivano **richieste piccole** (le
immagini di una pagina, una chiamata API, un aggiornamento di stato). Prima del
piano, le richieste piccole restavano incastrate dietro al download.

Il piano ha introdotto uno "smistatore": il server riconosce da solo quali
connessioni stanno muovendo grandi quantità di dati — **contando i byte
effettivamente trasferiti**, non indovinando dall'indirizzo o dal tipo di
file — e manda le richieste nuove su una corsia libera.

### Che cosa è successo

Tempo di risposta di una richiesta piccola, mentre uno o due download pesanti
sono in corso. Numeri più bassi = meglio. `p50` è il tempo tipico, `p95` è il
tempo del 5 % di richieste più sfortunate (quello che l'utente percepisce come
"a volte si impunta").

| configurazione | con 1 download in corso: prima → dopo | con 2 download: prima → dopo |
| --- | --- | --- |
| 2 corsie | 2,95 / 17,5 ms → **2,71 / 4,06 ms** | 60,5 / 109,9 → **23,8 / 104,2 ms** |
| 4 corsie | 7,09 / 37,9 ms → **2,65 / 4,24 ms** | 36,6 / 77,8 → **20,6 / 37,2 ms** |
| **8 corsie** | 3,42 / 14,1 ms → **2,68 / 4,92 ms** | 14,8 / 42,5 → **5,48 / 15,9 ms** |

E le richieste servite al secondo, nello stesso momento:

| configurazione | con 1 download: prima → dopo | con 2 download: prima → dopo |
| --- | --- | --- |
| 4 corsie | 629 → **2 719** (4,3×) | 205 → **390** |
| 8 corsie | 1 464 → **2 458** (1,7×) | 393 → **1 162** (3,0×) |

**Il risultato in una frase:** con un download in corso, una richiesta piccola
ora costa **2,7 millisecondi** — esattamente quanto costerebbe a tunnel
completamente scarico (2,45 ms). Il download pesante è diventato **invisibile**
per il resto del traffico. Il documento di riferimento chiudeva dicendo
"nessuna configurazione torna ai 2,5 ms di riposo; il caso migliore sotto carico
è 7–15 ms". Non è più vero.

### Dov'è il collo di bottiglia

Restano due limiti, entrambi previsti dal progetto e non difetti:

1. **Con una sola corsia non c'è niente da smistare.** Se il tunnel è
   configurato con `--carriers 1` il codice è identico a prima e il problema
   resta. La soluzione *è* avere più corsie.
2. **Con due download contemporanei le corsie si riempiono tutte.** Lo
   smistatore evita una corsia già occupata da un download; se sono occupate
   tutte, non c'è dove andare. Con 8 corsie il costo scende comunque da 14,8 a
   5,5 ms, ma non arriva a zero. È il tetto naturale del meccanismo.

### Un secondo miglioramento, sul percorso UDP

Sul percorso diretto QUIC il meccanismo è diverso (le corsie non aiutano): è
chi *invia* i dati pesanti che si retrocede da solo. L'effetto si vede sulla
coda peggiore: il 1 % di richieste più sfortunate è passato da **219 ms a
66 ms**, un miglioramento di 3,3 volte, a parità di tempo tipico. Tradotto:
sono spariti gli scatti.

---

## 3. La modalità automatica `--carriers 0`

### Che cosa abbiamo provato

Una novità del piano: invece di decidere a mano quante corsie servono, il
tunnel ne apre una e il **server** gli chiede di aggiungerne quando serve.

### Che cosa è successo

| situazione | corsie |
| --- | --- |
| tunnel fermo | resta a **1** (non cresce mai a vuoto) |
| 1 download pesante | cresce **1 → 2** |
| 2 download pesanti | cresce **2 → 3** |
| dopo altri 6 secondi | resta a 3 (non chiude mai una corsia viva) |

Funziona esattamente come progettato. Dalla workstation di casa, in un
confronto appaiato contro `--carriers 8`, ha reso **1,033** — cioè la stessa
banda — **tenendo però una sola corsia aperta**.

### Dov'è il collo di bottiglia

Due limiti da conoscere, entrambi voluti:

* **Cresce di una corsia ogni 2 secondi e si ferma a 4.** Per un tunnel che
  sposta dati in continuazione, dirgli `--carriers 8` una volta per tutte è
  meglio (5,48 ms contro 6,00 ms di tempo tipico, e soprattutto 15,9 ms contro
  46,6 ms sulla coda).
* **Se ci sono *solo* download e nessuna richiesta piccola, non cresce mai.**
  Non è un errore: il server aumenta le corsie quando una richiesta piccola
  **entra effettivamente in conflitto** con un download, non semplicemente
  perché un download è in corso. Un tunnel che scarica e basta non ha nulla da
  proteggere. Vale la pena scriverlo nella documentazione, perché sul campo
  sorprende.

**Consiglio pratico:** `--carriers 0` è l'impostazione giusta quando non si sa
che carico avrà il tunnel. Non sostituisce il saperlo.

---

## 4. La domanda del committente: si riesce a saturare la linea di casa?

### 4.1 Prima di tutto: quanto va davvero questa linea?

Non ha senso dire "il tunnel fa 400 Mbit/s" senza sapere quanto fa la linea.
La workstation è collegata **solo in WiFi** (tutte le schede via cavo sono
spente): WiFi 6, 80 MHz, 2 flussi, segnale −50 dBm, velocità teorica della
radio 1 200 Mbit/s.

Misurata contro server pubblici ben dimensionati, con più flussi in parallelo:

| direzione | risultato migliore |
| --- | --- |
| **download** | **46 MB/s = 390 Mbit/s** (Hetzner, 4 flussi) |
| **upload** | **105 MB/s = 885 Mbit/s** (Cloudflare, 4 flussi) |

Verificato con tre fornitori diversi per non dipendere da un solo riferimento.
Da notare: **questa linea carica più del doppio di quanto scarica.** È
un'asimmetria inversa rispetto al solito, e cambia completamente la lettura dei
risultati.

### 4.2 Il risultato

Con la workstation come *consumatore* e il programma servito sulla VM AWS
(cioè la situazione reale di chi usa il tunnel):

| | download | upload |
| --- | --- | --- |
| miglior risultato del tunnel | **59,10 MB/s = 496 Mbit/s** | **93,67 MB/s = 786 Mbit/s** |
| tetto della linea (§4.1) | 46 MB/s = 390 Mbit/s | 105 MB/s = 885 Mbit/s |
| `ssh` semplice verso la stessa VM | 47 MB/s | 79 MB/s |
| **esito** | **saturata, e oltre il riferimento** | **89 % del tetto** |

**In download il tunnel va più veloce di qualunque riferimento pubblico che
questa linea riesca a raggiungere** (+28 % rispetto a Hetzner, +26 % rispetto a
un trasferimento `ssh` diretto dalla stessa VM). Non c'è più margine da
recuperare: il limite è la linea, e `bore` ci sta sopra.

**In upload il tunnel arriva all'89 % del massimo misurabile, ed è comunque il
18 % più veloce di `ssh`** verso la stessa macchina. L'11 % mancante è
spiegabile con l'incapsulamento (TLS + intestazioni di multiplexing su ogni
blocco da 128 KiB): non serve ipotizzare un difetto.

### 4.3 Dov'è il collo di bottiglia

| prova | limite trovato | di chi è |
| --- | --- | --- |
| download dalla workstation | **la linea WiFi di casa** (~390–496 Mbit/s) | del collegamento |
| upload dalla workstation | quasi la linea; il resto è sovraccarico di protocollo | metà collegamento, metà protocollo |
| tutte le prove ripetute a lungo | **il credito di banda dell'istanza AWS** | dell'istanza, vedi §4.4 |

### 4.4 La scoperta metodologica più importante di tutta la campagna

La prima serie di prove dalla workstation sembrava mostrare un crollo
catastrofico: le prime due configurazioni andavano bene (50–59 MB/s), le tre
successive erano ferme a **7 MB/s**. Sembrava una regressione grave.

Non lo era. Il valore di 7 MB/s compariva **in entrambe le direzioni e su
entrambi i protocolli**, cosa che nessun difetto del software può produrre: il
percorso TCP e quello UDP non condividono nulla.

I contatori di rete del server hanno dato la risposta: durante quella serie il
contatore `bw_in_allowance_exceeded` è salito di **4 029 161**. Il server è una
`t4g.micro`, una macchina economica a cui Amazon assegna un **credito di banda
in ingresso**: finché c'è credito va veloce, quando finisce scende alla
velocità base — che è appunto ~7 MB/s. Il credito si ricarica in circa cinque
minuti di inattività.

**Conseguenze, che valgono per tutti i numeri di questo documento:**

1. Una lunga serie di prove consecutive su una macchina "burstable" misura le
   politiche di Amazon, non il software. L'ordine delle prove decideva quale
   configurazione sembrasse migliore.
2. Da quel momento **ogni prova di banda verifica il credito prima di
   partire** e **riporta quanto ne ha consumato**, così una misura falsata si
   vede invece di essere mediata insieme alle altre.
3. I confronti fra configurazioni sono **appaiati**: le due alternative girano
   nello stesso momento e si alternano a raffiche brevi. Se il credito cala,
   cala per entrambe e il rapporto resta valido.

Questo non è un difetto di `bore`. È la macchina scelta per lo staging.

---

## 5. Il caso reale: `dufs`, un vero file server

### Che cosa abbiamo provato

Tutte le prove precedenti usano un programma finto che genera byte dalla
memoria: è la scelta giusta quando si vuole misurare il tunnel, ed è la scelta
sbagliata per rispondere a "funziona con un'applicazione vera?".

Su richiesta del committente abbiamo quindi installato **`dufs`** (un file
server HTTP reale) sulla VM di prova, esposto tramite un forwarder `bore`, e
lo abbiamo usato dalla workstation di casa come farebbe un utente: scaricando e
caricando file grandi, e poi centinaia di file piccoli.

Contenuto messo a disposizione: un file da 1 GiB, dieci file da 20 MiB,
duemila file da 8 KiB. **Attenzione al disco della VM** (richiesta esplicita):
il disco ha ~10,7 GiB totali, l'archivio occupa 1,3 GiB, la cartella di
caricamento viene **svuotata dopo ogni prova**, e ogni prova si rifiuta di
partire se restano meno di 2 GiB liberi. A fine campagna: **6 126 MiB liberi**.

### 5.1 File piccoli: chi paga davvero il conto

Questa è la scoperta più utile dell'intera sezione. Abbiamo misurato le stesse
identiche richieste **anche in locale sulla VM, senza tunnel**, per capire
quanto costa `dufs` da solo.

| operazione | attraverso il tunnel | **`dufs` da solo, senza tunnel** |
| --- | --- | --- |
| 500 file da 8 KiB, uno alla volta | 66,5 ms per file | **41,0 ms per file** |
| 500 file, 8 in parallelo | 8,2 ms per file | 4,8 ms |
| 500 file, 32 in parallelo | 2,8 ms per file (**360 file/s**) | 1,2 ms (832 file/s) |
| 500 caricamenti, uno alla volta | 25,4 ms per file | **0,21 ms** |
| 500 caricamenti, 32 in parallelo | 1,6 ms per file (**618 file/s**) | 0,16 ms |

**Su una richiesta piccola sequenziale, 41 dei 66 millisecondi sono di `dufs`,
non del tunnel.** Il resto — circa 25 ms — è tutto quello che fanno la rete e
`bore` messi insieme, contro un tempo di andata e ritorno misurato di 19,5 ms
verso il server. Il costo proprio di `bore` per una richiesta piccola è quindi
**sotto i 10 millisecondi**.

La prova del nove sono i caricamenti: `dufs` risponde a un caricamento piccolo
in 0,21 ms in locale (lì il suo difetto non si manifesta), e attraverso il
tunnel costa 25,4 ms — cioè un andata e ritorno di rete e poco più. Quello è
il costo vero del tunnel.

*Nota tecnica per chi vuole approfondire:* i 41 ms di `dufs` in locale, su una
connessione dove il ritardo è di microsecondi, sono la firma tipica
dell'interazione fra "delayed ACK" e algoritmo di Nagle nel percorso di
risposta di `dufs`. Non è un problema di `bore` e non è risolvibile da `bore`.

### 5.2 Dov'è il collo di bottiglia (file piccoli)

| in questa condizione | il limite è |
| --- | --- |
| un file alla volta | **l'applicazione servita** (`dufs`, 41 ms su 66) e, per il resto, **la velocità della luce** (19,5 ms di andata e ritorno) |
| 8 o 32 in parallelo | nessuno dei due: si arriva a **360 download/s e 618 caricamenti/s** |

**La lezione operativa:** su un carico fatto di tanti file piccoli, l'unica
leva che conta è il **parallelismo**. Nessuna impostazione del tunnel può
battere il tempo di andata e ritorno; farne 32 alla volta sì.

### 5.3 Le tre modalità di esecuzione sono equivalenti

Il committente ha chiesto di provare tutte e tre: binario nativo, immagine
Docker ufficiale del client, gateway SSH (cioè un client OpenSSH normale, senza
`bore` installato).

| operazione | binario | Docker `:client` | gateway SSH |
| --- | --- | --- | --- |
| 10 file da 20 MiB insieme | 34,26 MB/s | 36,81 MB/s | 33,59 MB/s |
| file piccolo, uno alla volta | 66,49 ms | 67,48 ms | **62,99 ms** |
| file piccolo, 32 in parallelo | 2,77 ms | 2,82 ms | 2,80 ms |
| caricamento, 32 in parallelo | 1,62 ms | 1,58 ms | 1,58 ms |
| richieste al secondo | 124 | 128 | 124 |

**Tutte e tre entro il 3 % l'una dall'altra.** L'immagine Docker (avviata con
`--network host`, necessario perché il file server ascolta solo su
`localhost`) non costa nulla rispetto al binario. Il gateway SSH è
leggermente più veloce sulle richieste sequenziali pur usando **una sola
corsia** — perché con una richiesta alla volta non c'è niente da smistare.

#### E sulla banda? Una trappola di misura, e la risposta vera

Il primo confronto sulla banda sembrava dare un vincitore netto:

```
[1] nativo = 49,82 MB/s
[2] docker = 23,37 MB/s
[3] ssh    = 21,31 MB/s
```

**Era completamente falso.** Una singola raffica da 500 MB esaurisce il credito
di banda dell'istanza (§4.4), quindi il primo sapore misurato trovava la linea
libera e gli altri due la trovavano strozzata. L'ordine decideva il vincitore.

Rifatto correttamente — tutti e tre i forwarder attivi insieme sullo stesso
file server, 75 secondi di pausa fra una raffica e l'altra, e **ordine ruotato
a ogni giro** così che ogni sapore occupi ogni posizione una volta:

| giro | 1ª posizione | 2ª posizione | 3ª posizione |
| --- | --- | --- | --- |
| 1 | nativo **49,71** | docker **50,94** | ssh **47,31** |
| 2 | docker **47,44** | ssh **46,75** | nativo **48,36** |
| 3 | ssh **52,08** | nativo **48,95** | docker **50,03** |

| sapore | mediana |
| --- | --- |
| binario nativo | **48,95 MB/s (411 Mbit/s)** |
| immagine Docker | **50,03 MB/s (420 Mbit/s)** |
| gateway SSH | **47,31 MB/s (397 Mbit/s)** |

**Scarto totale fra tutti e tre: 5,7 %**, e la classifica non è stabile — ogni
sapore vince almeno una posizione, e la raffica più veloce di tutte le nove
(52,08 MB/s) è del **gateway SSH**, che per giunta usa una sola corsia.
Conclusione: **nessuno dei tre paga un prezzo in banda.**

In **upload** le due raffiche pulite per sapore danno:

| sapore | mediana upload | corsie |
| --- | --- | --- |
| binario nativo | 80,26 MB/s (673 Mbit/s) | 8 |
| immagine Docker | 79,65 MB/s (668 Mbit/s) | 8 |
| gateway SSH | **88,25 MB/s (740 Mbit/s)** | 1 |

Nativo e Docker sono di nuovo indistinguibili (0,8 % di differenza). Il gateway
SSH è ~10 % più veloce, **ma non perché SSH sia migliore**: il gateway SSH per
progetto usa una sola corsia, gli altri due ne usavano otto. È lo stesso effetto
già misurato in §4: otto corsie costano il 23 % del picco in upload, perché a
19,5 ms di distanza ogni corsia cresce a un ottavo della velocità e il totale ci
rimette. Se un deployment fa soprattutto upload, la corsia singola forzata del
gateway SSH è un **vantaggio**, non un limite.

### E con il canale diretto UDP invece che con il relay?

Tutte le prove qui sopra usano il canale TCP (il *relay*). Per il gateway SSH
non è una dimenticanza — quel percorso è TCP per progetto — ma restava da
misurare il canale diretto UDP proprio sul caso che assomiglia alla produzione.

Due forwarder registrati insieme sullo stesso file server, ordine invertito a
ogni giro. Prima di citare qualunque numero abbiamo verificato che i due
percorsi fossero davvero diversi: l'arma "relay" chiude con 0 aperture dirette,
l'arma "UDP" con **226 aperture dirette e zero ripieghi**.

| | relay TCP (8 corsie) | UDP diretto (1 corsia) |
| --- | --- | --- |
| download (mediana) | 49,82 MB/s | **52,69 MB/s** |
| upload (mediana) | **66,29 MB/s** | 63,88 MB/s |
| richieste piccole, p50 | **93,93 ms** | 103,74 ms |
| richieste piccole, p95 | **105,02 ms** | 135,82 ms |

Il dato interessante è il primo, perché **sembra contraddire** il §4, dove il
canale UDP risultava il 30–40 % più lento. Non è una contraddizione: è la
durata della raffica.

Il conto del credito di banda lo spiega esattamente. Sui tre download l'arma
UDP ha fatto scattare **37 622** volte il limite di banda dell'istanza, contro
**1 054** dell'arma relay: **36 volte tanto**, a parità di byte consegnati. Il
canale diretto consuma il credito molto più in fretta per ogni byte, quindi:

* su una raffica **breve**, che sta dentro il credito, vince;
* su un trasferimento **prolungato**, finisce nella strozzatura e perde
  nettamente.

È una proprietà dell'istanza economica, non di bore — ed è utile dirla perché
smentisce in entrambe le direzioni l'intuizione «il diretto deve per forza
essere più veloce di un relay».

Sulle **richieste piccole** il relay vince del 10 %, e vince di più sulla coda
(p95) che sulla mediana. Stessa classifica di tutte le altre prove.

> I valori assoluti di questa riga (93,93 ms) sono più alti dei 66,49 ms del
> §5 perché qui ogni richiesta apre una connessione nuova e paga un handshake
> completo. Va letto il **confronto fra le due colonne**, non il numero in sé.

**Conclusione operativa, ora verificata anche sul caso reale: per un file
server, lasciare `--udp` spento.** Il relay è più veloce sulle richieste
piccole, pari o migliore sul traffico pesante, costa meno CPU per gigabyte e
molto meno credito di banda. Il canale diretto serve dove il relay non può
arrivare, non come acceleratore.

---

## 6. Consumo di CPU e memoria (richiesta esplicita del committente)

Durante **tutte** le prove un campionatore ha registrato ogni 2 secondi, su
ciascuna macchina, l'uso di CPU (suddiviso fra codice applicativo, kernel e
rete), la memoria, e il consumo del singolo processo `bore`.

### 6.1 Per fase della campagna

Entrambe le macchine hanno 2 CPU, quindi "100 %" significa una CPU intera.

| fase | CPU server (media / picco) | memoria server (picco) | `bore` sul server | CPU VM | `bore` sulla VM |
| --- | --- | --- | --- | --- | --- |
| confronti appaiati di trasporto | 64 % / 86 % | 684 MiB | 31 MiB | 30 % | 107 % di una CPU |
| prove di stabilità | 9 % / 60 % | **863 MiB su 903** | **539 MiB** | 8 % | 4 % |
| prove con rete degradata | 26 % / 82 % | 859 MiB | 533 MiB | 13 % | 10 % |
| confronto gateway SSH | 24 % / 95 % | 506 MiB | 117 MiB | 14 % | 3 % |
| efficienza sui trasferimenti | 52 % / 86 % | 541 MiB | 114 MiB | 25 % | 20 % |

Tre osservazioni:

* **La macchina non è mai stata rallentata da Amazon per mancanza di CPU**
  (l'indicatore `steal` resta sotto l'1,5 %): i numeri di prestazione sono
  quindi genuini.
* **Il picco di memoria — 863 MiB su 903 disponibili — è il dato più
  preoccupante di tutta la campagna**, e riguarda un caso specifico descritto
  al §7 (troppi lettori lenti su un tunnel UDP).
* **Il lato client è economico:** sulla VM `bore` è costato dal 3 % al 20 % di
  una CPU mentre spostava 100–250 MB/s. Il costo sta sul server, che deve
  cifrare e decifrare per il lato pubblico e gestire entrambi i lati della
  connessione.

### 6.2 Il piano ha reso `bore` più costoso?

Domanda importante: una correzione sulle latenze che avesse reso il server
del 20 % più costoso sarebbe stata un pessimo affare su una macchina a 2 CPU.

Costo misurato in **secondi di CPU per gigabyte trasferito**:

| percorso | prima del piano | dopo il piano |
| --- | --- | --- |
| relay TCP | 6,78 | **6,85** |
| diretto QUIC | 11,92 | **13,00** |

Entrambi rientrano nella variabilità naturale di questa macchina. **Il lavoro
di smistamento introdotto dal piano non costa nulla di misurabile.**

Da notare, come informazione a sé: **il 44–52 % della CPU consumata è lavoro
di rete del kernel**, non codice di `bore`. E il percorso UDP costa **quasi il
doppio** del percorso TCP per byte trasferito — un dato che vale la pena
conoscere prima di attivare `--udp` pensando che sia sempre un'ottimizzazione.

### 6.3 Le fasi restanti: workstation, file server, prove sui parametri

Il committente ha chiesto il consumo di **tutti** gli attori durante **tutte**
le prove. Un campionatore ogni 2 secondi ha quindi seguito server, VM di prova
e workstation anche nelle tre fasi finali.

| fase | server (2 core) | VM di prova (2 core) | workstation (16 core) |
| --- | --- | --- | --- |
| confronto fra i tre sapori | **0,17** core in media, picco 1,52 | 0,08 core, picco 0,82 | 0,63 core, picco 5,20 |
| file server relay vs UDP | **0,14** core, picco 1,35 | 0,07, picco 0,91 | 1,30, picco 15,75 |
| prove sui parametri del server | **0,27** core, picco 1,58 | 0,07, picco 0,76 | 1,35, picco 15,74 |

Per processo:

| processo | picco di memoria |
| --- | --- |
| `bore` sul server, traffico normale | 84,5 – 106,6 MiB |
| `bore` sul server, prova dei lettori lenti **senza** tetto di memoria | **340,4 MiB** |
| `bore` sul server, stessa prova **con** tetto di memoria | **42,2 MiB** |
| `dufs` sulla VM (il file server vero) | 4,3 MiB |
| `bore` sulla VM (il forwarder) | 17,3 MiB |

Quattro conclusioni.

**Il processore del server non è mai stato il collo di bottiglia, e non ci è
andato neanche vicino.** Mentre muoveva ~50 MB/s in discesa e ~80 MB/s in
salita su tre tunnel contemporanei, l'intera macchina a 2 core stava in media
allo **0,17 di un core**. È questo che rende credibile la spiegazione del §4
(il limite è il credito di banda dell'istanza): non è la spiegazione comoda, è
l'unica rimasta in piedi dopo aver escluso la CPU con la misura.

**Anche il credito di CPU era intatto.** L'indicatore di "CPU sottratta"
(*steal*) è rimasto fra lo 0,29 % e lo 0,71 %. Un'istanza economica a corto di
credito CPU mostrerebbe decine di punti percentuali. Quindi le due risorse a
credito si comportano in modo opposto: quella di **rete** si esaurisce con una
sola raffica di 10 secondi, quella di **CPU** non si scalfisce in un'ora.

**La memoria è dove il tetto di memoria UDP fa la differenza**, e la fa in
modo netto: 340 MiB contro 42 MiB per lo stesso identico scenario, su una
macchina che di RAM ne ha circa 950. Un solo tunnel arrivava a occupare più di
un terzo della macchina.

**Il lato di chi pubblica il servizio è gratis.** Il file server vero ha
servito tutta la campagna con il 3–4 % di un core e 4,3 MiB di memoria; il
forwarder con il 2 %. Non c'è nulla da dimensionare da quella parte.

> I numeri della workstation includono l'attività normale di un computer di
> lavoro, non sono un banco di prova dedicato: i picchi al 98 % non sono la
> misura. La parte affidabile è quella per processo, e lì `curl` e `bore`
> stanno sotto l'1 % di un core.

### 6.4 L'ultima fase, quella delle prove sui parametri

È l'unica fase in cui il server è stato **riavviato sette volte**, una per ogni
configurazione provata. Serve anche a rispondere a una domanda pratica: un
riavvio lascia strascichi?

| macchina | CPU media | picco | CPU sottratta | memoria del processo `bore` |
| --- | --- | --- | --- | --- |
| server (2 CPU) | **0,19 CPU su 2** | 1,23 CPU | 0,47 % | 96,3 MiB |
| VM di prova (2 CPU) | 0,05 CPU su 2 | 0,77 CPU | 0,00 % | 74,6 MiB |
| workstation (16 CPU) | 0,60 CPU su 16 | 2,29 CPU | 0,00 % | 7,9 MiB |

Due cose da portarsi via:

* **con il tetto di memoria attivo il processo `bore` sul server occupa 95–96
  MiB**, contro i 340 MiB della stessa prova senza tetto. Il parametro fa
  esattamente quello che promette, e lo fa su un impianto vero, non solo in una
  prova costruita apposta;
* **i sette riavvii non hanno lasciato traccia**: CPU, memoria e CPU sottratta
  nell'arco dei 22 minuti sono indistinguibili dalle fasi senza riavvii, e i tre
  tunnel del committente si sono riregistrati da soli ogni volta.

---

## 7. Che cosa resta aperto (e perché non l'abbiamo chiuso)

Onestà sui limiti: non tutto è risolto. Ecco che cosa resta, in ordine di
importanza pratica.

### 7.1 La prima richiesta durante un blackout UDP (ridotta da 10 s a 4 s, non azzerata)

**Sintomo.** Se un tunnel usa il percorso UDP e la rete UDP smette di
funzionare (firewall, cambio di rete, operatore mobile), **la prima** richiesta
resta appesa circa 10 secondi prima di ripiegare sul percorso TCP. Dalla
seconda in poi tutto è corretto e veloce (~12 ms).

**Che cosa è stato risolto.** Il piano ha aggiunto un timeout di 3 secondi
sull'apertura del canale diretto, e ha reso onesti i contatori diagnostici:
prima il contatore delle "aperture riuscite" saliva da 1 a 12 durante un
blackout totale, cioè contava tentativi falliti come successi. Ora resta
correttamente fermo.

**Perché il timeout non basta.** Quando l'altro capo è *silenzioso* (i
pacchetti spariscono senza risposta, invece di essere rifiutati), l'apertura
del canale **riesce localmente**: il nostro lato crede di aver aperto. Il
timeout quindi non scatta, e la richiesta aspetta la scadenza di inattività di
QUIC, che è 10 secondi.

**Aggiornamento: non è più aperto, è diventato una manopola.** Dopo aver
scritto quanto sopra abbiamo costruito una prova dedicata (che gira in un
minuto su qualunque macchina Linux, senza server e senza privilegi) e
verificato che la finestra di perdita **coincide esattamente** con la scadenza
di inattività: 10 s → 10,0019 s, 6 s → 6,0020 s, 4 s → 4,0011 s, 2 s →
2,0021 s. I due valori, prima scritti nel codice, sono ora configurabili;
basta impostarli **sul server** (le due estremità negoziano il più basso, quindi
i client non vanno toccati); e con perdita di pacchetti fino al 30 % la
scadenza ridotta **non abbatte** una connessione sana. Il dettaglio completo è
nel §1.

Resta un residuo vero, ma molto più piccolo: **la finestra non può essere
azzerata**, solo accorciata, perché una richiesta già impegnata su un canale
diretto silenzioso deve comunque aspettare che quel canale venga dichiarato
morto. Azzerarla richiederebbe un secondo timeout sulla *prima risposta*, che
è la correzione che continuiamo a non fare senza una verifica dedicata: mal
tarata, abbandonerebbe un'applicazione lenta ma sana.

**Nel frattempo:** il problema riguarda solo i tunnel avviati con `--udp`, e
solo la prima richiesta dopo l'interruzione. Con
`BORE_DIRECT_QUIC_IDLE_MS=4000` sul server il caso peggiore scende da 10 a 4
secondi.

### 7.2 Il consumo di memoria con molti lettori lenti (F-13)

**Sintomo.** 32 lettori lenti su un solo tunnel UDP hanno portato la memoria
del server a **483,6 MiB** (su 903 MiB totali della macchina), facendo scadere
richieste di **altri** tunnel.

**Non è una regressione: è una protezione presente ma non attiva.** Il piano
ha introdotto il parametro `--udp-memory-budget`, che impone un tetto
complessivo alla memoria del percorso diretto. Non è attivo per impostazione
predefinita — quindi il comportamento predefinito è, correttamente, identico a
prima. Le prove con il parametro attivo sono al §9.

### 7.3 La coda con 512 connessioni aperte

Con 256 connessioni tenute aperte il problema è **sparito**: una nuova
richiesta costava 966 ms, ora ne costa **12,7**. Con 512 resta (≈1,5 s).

I contatori dell'istanza indicano la causa: il superamento del limite di
**pacchetti al secondo** è a 16,7 milioni cumulativi, tre ordini di grandezza
sopra i contatori di banda. È una caratteristica della macchina, non del
codice, e la campagna precedente era arrivata alla stessa conclusione. Resta
volutamente non corretto: non c'è un meccanismo da correggere, e una modifica
"a sentimento" toccherebbe il percorso di accettazione delle connessioni che
le misure hanno appena mostrato essere pulito.

### 7.4 Il percorso UDP non è più veloce del relay TCP

Va detto chiaramente perché è controintuitivo: **`--udp` non è
un'ottimizzazione generale.**

| | relay TCP | diretto QUIC |
| --- | --- | --- |
| banda su rete pulita | **1,5× più veloce** | — |
| costo in CPU per gigabyte | 6,85 s | **13,00 s (quasi il doppio)** |
| dalla workstation di casa | riferimento | **0,61–0,72 volte** |
| con 512 connessioni aperte | 1 469 ms | **14 ms** |

Il valore del percorso diretto **non è la banda**: è il comportamento sotto
molte connessioni contemporanee, e il non dover passare da un intermediario.
Andrebbe scritto così nella documentazione, perché oggi `--udp` si legge come
un miglioramento senza compromessi.

### 7.5 Un contatore che mente per omissione

Il contatore `direct_fallbacks` esposto in `/admin/api/v1/metrics` è rimasto a
zero durante un blackout in cui il contatore per-tunnel saliva correttamente
da 1 a 5. Entrambi funzionano come sono scritti — quello globale conta solo i
tunnel pubblici — ma il nome promette un totale. Chi guarda solo la pagina
delle metriche concluderebbe che il ripiego non è mai avvenuto. È un problema
di nome e documentazione, non di comportamento.

---

## 8. Riepilogo: per ogni prova, dove si satura e perché

Questa è la tabella che il committente ha chiesto esplicitamente. Per ogni
prova: il risultato, il muro contro cui si è fermata, e **di chi è quel muro**.

### 8.1 Prove di banda

| prova | risultato | collo di bottiglia | di chi è | si può migliorare? |
| --- | --- | --- | --- | --- |
| download workstation ← tunnel | **59,10 MB/s (496 Mbit/s)** | linea WiFi di casa (riferimento pubblico migliore: 46 MB/s) | **collegamento** | no, è già oltre il riferimento |
| upload workstation → tunnel | **93,67 MB/s (786 Mbit/s)** | 89 % del tetto della linea; il resto è incapsulamento TLS + multiplexing | collegamento + protocollo | marginalmente |
| download di un file grande via `dufs` | 51,13 MB/s (429 Mbit/s) | idem, linea di casa | **collegamento** | no |
| upload di un file grande via `dufs` | 80,57 MB/s (676 Mbit/s) | idem | collegamento | marginalmente |
| trasferimento VM → server (stessa regione) | 150–250 MB/s su TCP | **CPU del server**: 1,2–1,6 CPU su 2 per un solo flusso | **applicazione + kernel** | solo con una macchina più grande |
| trasferimento VM → server via UDP/QUIC | 110–127 MB/s | idem, ma il doppio del costo per byte | applicazione | è il limite noto del percorso UDP |
| download `dufs` sul percorso diretto UDP (raffiche da 10 s) | 52,69 MB/s (442 Mbit/s) | linea di casa; ma il percorso UDP **consuma 36 volte più credito d'istanza per byte** | collegamento, poi **istanza** | no: su trasferimenti lunghi peggiora, non migliora |
| upload `dufs` sul percorso diretto UDP | 63,88 MB/s (536 Mbit/s) | credito d'istanza già esaurito su tutte le raffiche | **istanza** | differenza col relay non significativa |
| carico di CPU del server durante tutte le prove | media **0,17 CPU su 2** | nessuno: la CPU non è mai stata il limite dal lato consumatore domestico | — | — |
| memoria del server con 48 lettori lenti su UDP | da **340,4 MiB a 42,2 MiB** con `BORE_UDP_MEMORY_BUDGET` | il tetto imposto dal parametro | **scelta di configurazione** | è il rimedio, non il limite |
| serie lunghe di prove ripetute | crollo a **7 MB/s** | **credito di banda in ingresso dell'istanza AWS esaurito** | **istanza** | solo cambiando tipo di istanza |

**Il punto chiave:** in nessuna prova di banda il collo di bottiglia è risultato
essere una scelta di progetto di `bore`. O era la linea di casa (saturata), o
era la CPU della macchina (2 CPU per cifrare e instradare 250 MB/s), o era il
credito di banda di Amazon.

### 8.2 Prove di latenza

| prova | risultato | collo di bottiglia | di chi è |
| --- | --- | --- | --- |
| richiesta piccola, tunnel scarico | **2,45 ms** | tempo di andata e ritorno di rete (2,1 ms) | **fisica** |
| richiesta piccola durante 1 download | **2,68 ms** (era 3,42) | nessuno: è tornata al livello di riposo | risolto |
| richiesta piccola durante 2 download | 5,48 ms (era 14,81) | tutte le corsie occupate da download | **limite noto del meccanismo** |
| file piccolo via `dufs`, uno alla volta | 66,5 ms | **41 ms sono di `dufs`**, 19,5 ms di rete, **~6 ms di `bore`** | **applicazione servita** |
| file piccolo via `dufs`, 32 in parallelo | 2,8 ms per file (360 file/s) | nessuno rilevante | — |
| 512 connessioni tenute aperte, nuova richiesta | 1 469 ms su TCP, 14 s su UDP | **pacchetti al secondo dell'istanza** (contatore a 16,7 milioni di superamenti) | **istanza** |
| prima richiesta durante un blackout UDP, valori di serie | **10,0 secondi** | la richiesta è già affidata al canale QUIC e può solo aspettare che la connessione muoia: la finestra **coincide** con il timeout di inattività | **`bore`** — vedi §7.1 |
| prima richiesta durante un blackout UDP, con `BORE_DIRECT_QUIC_IDLE_MS=4000` | **4,0 secondi** (−60 %) | lo stesso meccanismo, con il timeout abbassato; misurato, non stimato | **scelta di configurazione** | sì: una riga nel compose |
| richieste successive durante lo stesso blackout | **~12 millisecondi** | nessuno: il ripiego sul relay è già caldo | risolto | — |

### 8.3 Le tre modalità di esecuzione

| modalità | banda | latenza | limite specifico |
| --- | --- | --- | --- |
| binario nativo | massima | ottima | nessuno |
| immagine Docker `:client` | identica al binario | identica | richiede `--network host` per raggiungere un servizio su `localhost` |
| gateway SSH | identica su un flusso singolo | identica | **non guadagna nulla dal parallelismo**: 8 download insieme rendono quanto uno solo (limite della finestra da 2 MiB del client OpenSSH, non correggibile lato server) |

Il limite del gateway SSH è l'unico che vale la pena ricordare in fase di
scelta: per un singolo trasferimento va come gli altri, per otto trasferimenti
insieme rende meno della metà del binario nativo (103 MB/s contro 237 MB/s).
In compenso è **la via più veloce per i caricamenti** (179–190 MB/s) e sotto
carico di molte richieste piccole serve il 40 % di richieste in più.

---

## 9. Coerenza dei parametri: una incoerenza trovata e corretta

Su richiesta esplicita ("se trovi delle incoerenze sui parametri tra quelli del
compose, i default e quelli visualizzati, correggi") è stato fatto un confronto
sistematico fra tre cose che devono coincidere: **cosa è scritto nel file di
configurazione del server**, **quali sono i valori predefiniti del programma**,
e **cosa mostra il pannello di amministrazione**.

### 9.1 La incoerenza principale, corretta nel codice

Il file di configurazione imposta:

```yaml
- BORE_PROXY_BUFFER_SIZE=128KiB
```

cioè **la metà** del valore predefinito del programma (256 KiB). Questo
parametro regola la dimensione del blocco di memoria usato per copiare i dati
da una connessione all'altra.

**Il problema non era il valore: era che non si poteva verificarlo.** Il
programma legge questa variabile una sola volta all'avvio e la registra nel log
solo al livello di dettaglio più basso (`trace`, normalmente disattivato). Il
pannello di amministrazione **non la mostrava affatto**, pur mostrando tutti i
parametri vicini:

```
udp_socket_send_buffer        = 16777216      ✓ mostrato
udp_stream_receive_window     = 16MiB         ✓ mostrato
udp_connection_receive_window = 256MiB        ✓ mostrato
udp_send_window               = 256MiB        ✓ mostrato
udp_max_streams               = 8192          ✓ mostrato
proxy_buffer_size             = ?             ✗ ASSENTE
```

Chi amministra il server non aveva modo di sapere se l'impostazione fosse
attiva, né di accorgersi che il server girava a metà del valore predefinito.

**Correzione applicata.** Il valore viene ora calcolato e pubblicato a ogni
lettura di `/admin/api/v1/config` e compare nel pannello. È esattamente lo
stesso tipo di correzione che il piano aveva già fatto per le intestazioni
HTTP del vhost (difetto F-6): non un valore copiato all'avvio — che potrebbe
divergere — ma **derivato ogni volta**.

Verifiche: nuovo test automatico che fallisce se la correzione viene rimossa
(controllo effettuato, il test diventa rosso), campo aggiunto anche al test di
integrazione del pannello, documentazione aggiornata in tre punti. **Tutti i
test passano: 378 unitari + tutte le suite di integrazione, 0 fallimenti;
101/101 test del frontend.**

### 9.2 Le altre differenze fra configurazione e valori predefiniti

Confronto sistematico di tutte le 35 variabili impostate nel file di
configurazione. Tre si discostano dai valori predefiniti **senza che nulla lo
dicesse**:

| variabile | nel compose | predefinito | rapporto |
| --- | --- | --- | --- |
| `BORE_MAX_CARRIERS` | 1024 | 16 | **64 volte** |
| `BORE_UDP_MAX_STREAMS` | 8192 | 4096 | 2 volte |
| `BORE_PROXY_BUFFER_SIZE` | 128KiB | 256KiB | **metà** |

Inoltre, il file conteneva un blocco di righe commentate introdotto da
*"leave commented for max performance (sono i default ottimali)"* che includeva
`BORE_UDP_MAX_STREAMS=4096` — cioè **contraddiceva** la riga attiva poco sopra
che imposta 8192. Chi legge il file non poteva capire quale valore fosse in
vigore né perché.

**Perché `BORE_MAX_CARRIERS=1024` merita attenzione particolare.** Questo
valore interagisce in modo non ovvio con la protezione di memoria
`--udp-memory-budget`: il programma calcola la finestra per connessione come
`budget ÷ max_carriers`, con un minimo di 16 MiB. Con `max_carriers` a 1024,
**qualunque budget realistico** finisce sotto il minimo, quindi le finestre
crollano al valore minimo indipendentemente dal budget scelto. Il
comportamento è documentato ("un budget più grande compra più posti, non
finestre più grandi") ma sorprende, e sul server non lo dice nulla.

**Tutte queste differenze sono ora annotate nel file di configurazione**, con
valore, valore predefinito e motivazione riga per riga.

#### Che cosa è stato misurato, e come è stato lasciato il file

Il committente ha autorizzato a variare i parametri del server
(*"puoi variare i parametri del server per fare i test… se li lasci sul compose
del server commentali in modo chiaro"*). Ecco l'esito di ciascuna prova e lo
stato finale del file.

| parametro | prova fatta | risultato | come è stato lasciato |
| --- | --- | --- | --- |
| `BORE_PROXY_BUFFER_SIZE` | tre giri alternati 128 → 256 → 128 KiB su un percorso lento da 40 ms, cioè dove un buffer di copia dovrebbe contare di più | **nessun effetto misurabile**: i due giri identici da 128 KiB danno 25,57 e 29,67 MB/s, il giro da 256 KiB sta in mezzo con 28,53 | **riga rimossa**: torna il valore predefinito di 256 KiB, con il commento che spiega la misura |
| `BORE_UDP_MEMORY_BUDGET` | quattro giri alternati acceso/spento/acceso/spento, misurati **dalla workstation di casa**, che è l'unico punto in cui il parametro potrebbe far male | **nessun costo**: acceso 53,16 e 56,25 MB/s, spento 52,28 e 49,72 | **acceso a 512 MiB**, con il commento che riporta sia il beneficio (da 340 a 42 MiB di memoria nel caso peggiore) sia la misura del costo |
| `BORE_MAX_CARRIERS=1024` | nessuna variazione: è una scelta deliberata | — | **lasciato**, ma ora la riga dichiara il valore predefinito (16) e l'avvertenza sull'interazione col tetto di memoria |
| `BORE_UDP_MAX_STREAMS=8192` | nessuna variazione | — | **lasciato**, con il valore predefinito (4096) dichiarato sulla riga |
| il blocco commentato contraddittorio | — | — | **riscritto**: ora dice che quei valori *sono* i predefiniti, che la riga attiva sopra ne sovrascrive uno, e che tre di essi sono incompatibili col tetto di memoria |

Una nota di onestà. Dopo la modifica, la pagina di amministrazione del server
**continua a mostrare i valori vecchi** per le finestre UDP e non mostra affatto
i campi nuovi. Non è la correzione che non funziona: il contenitore in
esecuzione usa ancora un'immagine costruita prima dell'11 settembre 2026. Le
correzioni e i loro collaudi sono nel codice sorgente (§9.1 e §9.3); appariranno
sul server alla prossima ricostruzione dell'immagine. Va detto esplicitamente,
perché altrimenti un operatore che guarda quella pagina oggi concluderebbe che
il tetto di memoria non è attivo.

### 9.3 Una seconda incoerenza, trovata dalle prove stesse

Attivando sul server il tetto di memoria UDP (`BORE_UDP_MEMORY_BUDGET=512MiB`)
e rileggendo la configurazione dall'API, il server rispondeva con le finestre
**predefinite** (16 MiB e 256 MiB) invece di quelle che il tetto aveva appena
ricalcolato (1 MiB e 16 MiB).

In altre parole: **l'interfaccia dichiarava una configurazione, il processo ne
eseguiva un'altra.** Alla domanda «il tetto di memoria è attivo?» rispondeva
con sicurezza, e sbagliava.

La causa è la stessa delle precedenti: il tetto ricalcola le finestre *dopo*
che i valori di partenza sono già stati fotografati per l'interfaccia. La
fotografia veniva scattata un passo troppo presto.

**Correzione.** Ora tutto il blocco UDP viene ricavato dai valori realmente
installati sul server, e c'è un campo nuovo che dice quanti posti simultanei il
tetto concede (`null` = nessun tetto, comportamento storico). Il test che lo
protegge costruisce un server, gli applica lo stesso piano che applicherebbe la
riga di comando, e verifica che l'interfaccia riporti i valori **derivati** —
oltre a controllare esplicitamente che siano *diversi* da quelli di partenza,
così un'interfaccia che smettesse di aggiornarsi non potrebbe passare il test
per caso. Verificato anche al contrario: togliendo la correzione, il test
diventa rosso.

Con questa e la precedente, **ogni parametro regolabile su questo server è ora
rileggibile dall'interfaccia nel valore effettivamente in vigore.** È
esattamente la proprietà richiesta: configurazione, valori predefiniti e valori
mostrati coincidono per costruzione, non per disciplina.

### 9.4 Il codice modificato è coperto da collaudi

Domanda esplicita del committente: *"vedo che ci sono file di codice
modificati. hai fatto i test a copertura?"*.

Sì. Sono stati toccati sette file di codice e due script di collaudo. Ognuno
ha il suo controllo automatico, e **ogni controllo nuovo è stato verificato al
contrario**: si è rimessa dentro la vecchia versione del codice, si è
controllato che il collaudo fallisse davvero, e poi si è rimessa la correzione.
È l'unico modo per sapere che un collaudo verde sta effettivamente verificando
qualcosa.

| che cosa è stato modificato | come è collaudato |
| --- | --- |
| i due tempi del percorso UDP, ora configurabili | 4 controlli automatici sulla regola di calcolo + una prova completa che riproduce il difetto e la sua correzione senza bisogno di alcun server (`scripts/perf/vhost_idle_window.sh`) |
| i quattro nuovi valori mostrati dalla pagina di amministrazione | 3 controlli automatici, uno per gruppo di valori, ciascuno verificato al contrario |
| il nuovo accesso alla configurazione UDP realmente installata | lo stesso controllo che verifica i valori derivati dal tetto di memoria |
| la pagina di amministrazione dal vivo | il collaudo `T-CFGFIELDS` interroga un server vero e pretende che tutti i nuovi campi ci siano |
| la documentazione | regola di progetto: se non è nel `README.md`, non è finito |

C'è una proprietà che conta più di tutte le altre e che è stata bloccata da un
collaudo apposito: **se non si imposta nessuna delle nuove variabili, il
comportamento è identico a quello di prima della campagna.** Senza questa
garanzia, ogni numero contenuto in questo documento smetterebbe di valere.

**Tutti i controlli automatici, rieseguiti dopo l'ultima modifica:**

| controllo | esito |
| --- | --- |
| formattazione e analisi statica del codice (due configurazioni) | pulite |
| collaudi automatici del programma | **955 superati, 0 falliti** |
| collaudi dell'interfaccia web | **101 superati, 0 falliti** |
| collaudo del pannello di amministrazione su rete reale | **25 superati, 0 falliti** |
| collaudo del difetto §7.1 e della sua correzione | **10 superati, 0 falliti** |

---

## 10. Come sono state fatte le misure (e perché ci si può fidare)

Questa sezione serve a rendere verificabile tutto il resto.

### 10.1 Le macchine

| ruolo | macchina | note |
| --- | --- | --- |
| server sotto esame | AWS `t4g.micro`, 2 CPU, **903 MiB di RAM** | è una macchina volutamente piccola ed economica |
| VM di prova | AWS `c7i-flex.large`, 2 CPU, stessa regione | ospita l'applicazione e il forwarder; tempo di andata e ritorno verso il server: **2,1 ms** |
| workstation | 16 CPU, 47 GiB, **solo WiFi 6** | l'utente finale; tempo di andata e ritorno verso il server: **19,5 ms** (mediana di 12 handshake TCP) |

### 10.2 Le tre regole metodologiche

Sono le tre cose che distinguono una misura utile da un numero qualsiasi su
questo tipo di macchina.

**1. Confronti appaiati, mai due numeri presi a distanza di minuti.** La
velocità assoluta di questa macchina varia del ~30 % fra due esecuzioni
identiche. Ogni confronto fa girare le due alternative una dopo l'altra,
alternando l'ordine, e il risultato è la **mediana dei rapporti**, non la
media delle velocità. Il rapporto sopravvive alla variabilità; la velocità
assoluta no.

**2. Verifica del credito di banda prima e durante ogni prova.** Come spiegato
al §4.4, l'istanza ha un credito che si esaurisce. Ogni prova di banda:
controlla il credito prima di partire, e riporta quanto ne ha consumato.
Una misura fatta a credito esaurito viene **segnalata**, non mediata insieme
alle altre.

**3. Un controllo interno in ogni prova.** Quando si degrada la rete
artificialmente (ritardo, perdita di pacchetti), il degrado viene applicato
**solo a un protocollo** (solo TCP oppure solo UDP), così l'altro percorso
resta pulito e dimostra che il filtro ha fatto davvero quello che dichiara. Se
il percorso di controllo rallenta anche lui, la prova è sbagliata e va
buttata. Questo controllo ha effettivamente scoperto un errore
nell'attrezzatura: un filtro scritto male non filtrava nulla, e il segnale è
stato che la velocità *saliva* durante il presunto blackout.

### 10.3 Che cosa è stato raccolto durante ogni prova

* CPU di ciascuna macchina ogni 2 secondi, divisa fra codice applicativo,
  kernel, elaborazione di rete e **tempo rubato dall'hypervisor** (per
  dimostrare che i risultati non sono falsati da un rallentamento imposto da
  Amazon);
* memoria totale e memoria del solo processo `bore`;
* i contatori di rete della scheda del server, che è ciò che ha permesso di
  identificare il limite dell'istanza;
* per ogni tunnel: numero di corsie aperte, percorso realmente usato
  (TCP o UDP), numero di ripieghi da UDP a TCP.

### 10.4 Attenzione allo spazio su disco (richiesta esplicita)

Il disco della VM di prova è stato sorvegliato per tutta la campagna: ogni
prova che scrive file si rifiuta di partire con meno di **2 GiB liberi**, la
cartella dei caricamenti viene svuotata dopo ogni fase, e i file di prova
grandi non vengono mai lasciati sul disco.

**Stato finale: 6 126 MiB liberi su 10 742 totali.** Nessun rischio corso.

### 10.5 I segreti non sono mai stati scritti nel repository

Come da regola: la chiave del server, il token di amministrazione e la
password del gateway SSH vivono in file separati con permessi ristretti, fuori
dal repository. Nella documentazione compaiono come "forniti separatamente".

### 10.6 Tutto è ripetibile: gli script stanno nel repository

Richiesta esplicita del committente: *"se li dobbiamo rifare tra un mese,
magari con altra vm server (pre produzione vera), deve essere tutto pronto per
rieseguirli"*.

L'intera attrezzatura è stata portata dentro il repository, in
`scripts/perf/staging/`: **41 script**, un compose di riferimento e un `README.md` che contiene la
tabella delle macchine, la procedura ordinata, come si legge un risultato e
l'elenco delle trappole già evitate.

La regola che rende il tutto riutilizzabile su un'altra macchina è una sola:
**nessuno script contiene un indirizzo, un nome di dominio, un percorso di
chiave o una credenziale.** Tutte queste informazioni stanno in un unico file
`env.sh` che l'operatore scrive una volta, partendo dal modello
`env.sh.example` incluso, e che resta fuori dal repository.

Per rifare la campagna su una macchina diversa bastano tre passi:

1. copiare `env.sh.example` in `~/.config/bore-perf/env.sh`, riempirlo con le
   coordinate della nuova macchina e proteggerlo (`chmod 600`);
2. lanciare `scripts/perf/staging/provision.sh`, che installa binario,
   immagine Docker, corpus di file di prova e tutti gli script sulle due
   macchine remote — è idempotente e si rifiuta di installare un binario di
   architettura sbagliata;
3. seguire l'ordine indicato nel `README.md` della cartella.

Una sola prova non richiede nemmeno il deployment:
`scripts/perf/vhost_idle_window.sh ladder` riproduce per intero il difetto
descritto al §7.1 e la sua correzione **su un portatile, in un minuto, senza
privilegi di amministratore**. È il collaudo di non-regressione di quel
difetto.

---

## 11. Conclusioni e raccomandazioni operative

### 11.1 Il piano ha fatto quello che doveva

| obiettivo del piano | esito |
| --- | --- |
| liberare il sottodominio di un client bloccato | **fatto**, entro 60 s, verificato su entrambi i trasporti |
| rispondere in modo comprensibile quando il servizio è spento | **fatto**, `502` in 15 ms |
| ripulire il log dagli avvisi inutili | **fatto**, da 718/786 a 3 in un'ora |
| mostrare nel pannello le impostazioni reali | **fatto** (e in questa campagna è stato chiuso un secondo caso dello stesso tipo) |
| non far aspettare le richieste piccole dietro ai download | **fatto**, ed è il risultato migliore: si torna al livello di riposo |
| non far pagare tutto questo in CPU | **fatto**, costo invariato |
| non rompere i client vecchi | **fatto**, verificato con il binario precedente |

### 11.2 Che cosa impostare, in pratica

**Sul client (`bore vhost`):**

| se il tuo caso è… | usa | perché |
| --- | --- | --- |
| un solo trasferimento grande alla volta | `--carriers 1` | massima banda: 59 MB/s in download, 94 in upload |
| un'applicazione web (richieste piccole + qualche download) | `--carriers 8` | è il caso per cui il piano è stato scritto: tempi di risposta al livello di riposo |
| non lo sai | `--carriers 0` | si adatta da solo, costa quanto `--carriers 1` |
| tanti file piccoli | qualunque, ma **parallelizza il client** | 32 richieste insieme: da 66 ms a 2,8 ms per file |

**`--udp` (percorso diretto QUIC): tenerlo spento**, salvo un caso preciso.
Costa il 30–40 % della banda da una linea domestica e quasi il doppio della CPU
per byte. Vale la pena accenderlo solo se il carico ha **moltissime connessioni
aperte contemporaneamente**, dove è 100 volte più veloce, oppure su reti con
perdita di pacchetti, dove è 2,7 volte più veloce.

**Sul server:** la configurazione è stata sistemata al termine della campagna
ed è documentata riga per riga nel file stesso. In sintesi:

| riga | stato finale | motivo |
| --- | --- | --- |
| `BORE_PROXY_BUFFER_SIZE` | **rimossa** (torna il predefinito 256 KiB) | misurato: nessun effetto, nemmeno dove dovrebbe averlo |
| `BORE_UDP_MEMORY_BUDGET=512MiB` | **aggiunta** | misurato: memoria nel caso peggiore da 340 a 42 MiB, e nessun costo di banda |
| `BORE_MAX_CARRIERS=1024`, `BORE_UDP_MAX_STREAMS=8192` | **lasciate**, ora annotate | scelte deliberate, ma prima nulla diceva che erano fuori standard |
| il blocco commentato che diceva il contrario | **riscritto** | contraddiceva la riga attiva |

Resta una sola raccomandazione: **ricostruire l'immagine del server**, così le
correzioni del §9.1 e del §9.3 entrano in produzione e i valori realmente in
vigore diventano leggibili dal pannello di amministrazione. Finché l'immagine
resta quella vecchia, la pagina mostra i valori richiesti e non quelli
applicati.

**Due parametri nuovi, opzionali, da conoscere.** `BORE_DIRECT_QUIC_IDLE_MS` e
`BORE_DIRECT_QUIC_KEEPALIVE_MS` non sono impostati e il comportamento
predefinito è identico a prima. Impostare `BORE_DIRECT_QUIC_IDLE_MS=4000` sul
**solo server** riduce da 10 a 4 secondi la finestra in cui una richiesta può
andare persa quando il canale UDP smette di funzionare (§7.1). Non è stato
attivato di serie perché è un compromesso: tempi più bassi reagiscono prima a un
guasto ma tollerano meno perdita di pacchetti. A 4 secondi la tolleranza
misurata arriva almeno al 30 % di pacchetti persi.

### 11.3 Se un giorno servisse più banda

In ordine di efficacia, con il motivo:

1. **Cambiare tipo di istanza per il server.** La `t4g.micro` ha 2 CPU e un
   credito di banda limitato: è il collo di bottiglia in praticamente tutte le
   prove ripetute. È la leva più grande e non richiede toccare il software.
2. **Parallelizzare il carico**, non il tunnel. Un singolo trasferimento su
   una rete con ritardo è limitato dalla fisica; otto trasferimenti no.
3. **Sul download verso casa non c'è niente da fare:** la linea è già satura.

---

## 12. Ricettario: come far partire i forwarder per avere le massime prestazioni

Questa sezione è la richiesta esplicita del committente: non "cosa abbiamo
misurato", ma **quali comandi dare**. Ogni riga qui sotto è giustificata da una
misura di questa campagna, indicata fra parentesi.

Un forwarder (o *provider*) è il processo che gira accanto alla tua
applicazione e la pubblica attraverso il server. Ci sono tre modi per farlo
partire — binario nativo, immagine Docker, gateway SSH — e in questa campagna
**rendono la stessa banda** (differenza fra il migliore e il peggiore: 5,7 % in
download), quindi la scelta è una questione di comodità, non di prestazioni.
La scelta che conta davvero è un'altra: **quante corsie** aprire.

### 12.1 La regola in una riga

> **Il numero di corsie (`--carriers`) va scelto in base al tipo di traffico,
> non "più alto possibile". Il percorso UDP diretto (`--udp`) va lasciato
> spento salvo due casi precisi.**

| il tuo caso | corsie | UDP |
| --- | --- | --- |
| applicazione web, pannello, API, file server usato da persone | **8** | spento |
| un solo trasferimento grande per volta (backup, sincronizzazione) | **1** | spento |
| non lo sai, o cambia nel tempo | **0** (automatico) | spento |
| moltissime connessioni tenute aperte insieme | 8 | **acceso** |
| rete con perdita di pacchetti (radio, satellite, VPN scadente) | 1 | **acceso** |

Le evidenze dietro questa tabella:

* **8 corsie** su carico misto: il tempo di risposta di una richiesta piccola
  mentre è in corso un download pesante passa da 14,06 a **4,92 millisecondi**
  (p95), e le richieste servite al secondo da 393 a **1 162**. Costa circa
  l'11 % della banda di punta di un singolo trasferimento (§2).
* **1 corsia** su trasferimento singolo: **59,10 MB/s** in download e
  **93,67 MB/s** in upload, contro 52,55 e 72,17 con 8 corsie. Otto corsie
  dividono in otto la finestra di congestione di un unico flusso: è la fisica
  del protocollo, non un difetto (§4.3).
* **0 corsie (automatico)**: rende quanto 8 fisse (rapporto mediano 1,033)
  tenendone aperta una sola, e ne apre altre solo quando una richiesta piccola
  **entra davvero in competizione** con un trasferimento pesante. Su un tunnel
  che muove solo file grandi resta a una corsia: è voluto (§3).
* **UDP spento**: sul file server vero il relay TCP è più veloce sulle
  richieste piccole (93,93 contro 103,74 ms), pari o meglio sui trasferimenti
  lunghi, costa **metà CPU per gigabyte** (6,85 contro 13,00 secondi di CPU) e
  consuma **36 volte meno** credito di banda dell'istanza a parità di byte
  consegnati (§5.4, §6.2).
* **UDP acceso**: con 512 connessioni tenute aperte una nuova richiesta impiega
  **14 millisecondi** sul percorso diretto contro **1 436** sul relay; su una
  rete con perdita artificiale il percorso diretto è **2,7 volte** più veloce
  (§7.3, §2).

### 12.2 I comandi, pronti da copiare

Sostituisci `app` con l'etichetta del tuo servizio, `8080` con la porta della
tua applicazione e `SERVER` con l'indirizzo del server (`https://…`). Il
segreto e il token **non vanno mai scritti dentro a un file versionato**: usa
una variabile d'ambiente o un file con permessi ristretti.

**A. Applicazione web (il caso più comune) — binario nativo**

```bash
bore vhost 127.0.0.1:8080 \
    --subdomain app --id app \
    --to "$BORE_SERVER" --secret "$BORE_SECRET" \
    --carriers 8 \
    --auto-reconnect
```

**B. Lo stesso, con l'immagine Docker pubblicata**

```bash
docker run -d --name bore-app --restart unless-stopped \
    --network host \
    -e BORE_SECRET="$BORE_SECRET" \
    ghcr.io/manprint/bore:client \
    vhost 127.0.0.1:8080 --subdomain app --id app \
    --to "$BORE_SERVER" --carriers 8 --auto-reconnect
```

`--network host` **non è opzionale** se l'applicazione ascolta su
`127.0.0.1` della macchina: senza, il contenitore vede il proprio localhost e
non il tuo. È l'unica differenza operativa fra immagine e binario che la
campagna abbia trovato; la banda è identica (50,03 contro 48,95 MB/s).

Un esempio completo e commentato, con anche il profilo UDP alternativo, è nel
repository: `scripts/perf/staging/vm/docker-compose.forwarder.yml`.

**C. Lo stesso, senza installare nulla: gateway SSH**

```bash
ssh -T -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 \
    -R vhost/app:80:127.0.0.1:8080 \
    -p 443 utente@SERVER
```

Due avvertenze misurate:

* **non usare `-N`.** Salta l'apertura del canale di sessione, quindi il
  server non può recapitare né il riepilogo del tunnel né gli avvisi: se
  qualcosa è configurato male, non lo saprai mai.
* **il gateway SSH non accetta `--carriers`**: usa sempre una sola connessione,
  per scelta di progetto. Ne consegue che è **la via più veloce per gli
  upload** (88,25 contro 80,26 MB/s del binario nativo con 8 corsie, proprio
  perché non divide il flusso) ma **non guadagna nulla dal parallelismo**: otto
  download insieme rendono quanto uno solo (§5.3).

**D. Un solo trasferimento grande (backup, sincronizzazione)**

```bash
bore vhost 127.0.0.1:8080 --subdomain backup --id backup \
    --to "$BORE_SERVER" --secret "$BORE_SECRET" \
    --carriers 1 --auto-reconnect
```

E soprattutto: **parallelizza il programma, non il tunnel.** Un solo flusso su
una rete con ritardo è limitato dalla fisica. Con 32 richieste in parallelo il
tempo per file è passato da 66 a **2,8 millisecondi** (360 file al secondo);
con `rclone --transfers 8`, `rsync` multipli o un client HTTP multi-connessione
si ottiene lo stesso effetto (§5.1).

**E. I due casi in cui accendere il percorso diretto UDP**

```bash
bore vhost 127.0.0.1:8080 --subdomain app --id app \
    --to "$BORE_SERVER" --secret "$BORE_SECRET" \
    --udp --carriers 1 --auto-reconnect
```

Il server deve avere `BORE_UDP=true` e la porta QUIC aperta in UDP. Se il
percorso diretto non si stabilisce, o cade, il traffico passa **da solo** sul
relay TCP già caldo, senza che tu debba fare nulla: è stato verificato
interrompendo la rete UDP a metà campagna (§7.1).

### 12.3 Come verificare che sia andata bene

Dopo aver avviato il forwarder, la pagina di amministrazione del server
(`/admin/api/v1/vhost`) dice la verità su tre cose:

| campo | che cosa devi vedere |
| --- | --- |
| `carriers` / `carrier_target` | il numero di corsie che hai chiesto (con `--carriers 0` parte da 1 e cresce solo sotto competizione) |
| `current_path` | `relay` oppure `direct`. Se hai chiesto `--udp` e leggi `relay`, il percorso diretto non è disponibile: il servizio funziona lo stesso |
| `direct_fallbacks` | quante volte si è ripiegato sul relay. Se cresce di continuo, la rete UDP verso il server ha un problema |

Un comando pronto è nel repository: `scripts/perf/staging/srv/verify.sh`.

### 12.4 Che cosa NON fare (errori che costano prestazioni)

* **Non alzare le corsie "per sicurezza".** Su un percorso pulito e scarico le
  corsie *costano*: rapporto mediano 0,88 fra 4 corsie e 1. Il valore
  predefinito è 1 proprio per questo.
* **Non accendere `--udp` per avere più banda.** Non ne dà: costa il doppio di
  CPU per byte e, su un'istanza cloud a credito, brucia il budget di rete molto
  più in fretta. Va acceso per la *concorrenza* e per le *reti con perdita*.
* **Non rimpicciolire il buffer di copia** (`BORE_PROXY_BUFFER_SIZE`). Fra
  128 KiB e 256 KiB non c'è differenza misurabile, ma scendere molto sotto
  riporta un problema noto sulle reti con ritardo alto. Lascialo com'è.
* **Non abbassare la finestra di ricezione per connessione del percorso UDP.**
  Il rapporto 16:1 fra finestra di connessione e finestra di flusso è quello che
  impedisce a pochi lettori lenti di bloccare tutti gli altri: a 64 MiB il
  problema si ripresenta (è un caso già capitato e già corretto in passato).
* **Non usare `-N` con il gateway SSH.** Vedi sopra: perdi ogni diagnostica.
* **Non misurare due configurazioni a distanza di minuti su un'istanza cloud a
  credito.** Su questa macchina la stessa identica prova varia del 30 % e, a
  credito esaurito, crolla a un settimo. Ogni confronto va fatto alternando le
  due configurazioni e guardando il rapporto, non la velocità assoluta (§10.2).

