# Analisi di fattibilita: secure file transfer per bore

## Obiettivo

Introdurre una nuova famiglia di comandi applicativi:

- `bore transfer listener --dest-path /path/...`
- `bore transfer sender --source /path/...`

con i seguenti obiettivi:

- trasferire file singoli, directory ricorsive e stream da `stdin`
- tentare di default il canale diretto UDP
- fare fallback automatico al relay TCP quando il diretto non e disponibile
- mantenere il server come puro broker/relay del flusso, senza persistenza del payload
- garantire correttezza del trasferimento con verifica end-to-end
- mostrare progress, velocita e stato live del trasferimento

## Stato attuale del repo riusabile

La base tecnica gia presente e solida e riduce molto il rischio del progetto:

- La CLI oggi ha `local`, `proxy`, `server`, `test-udp`; non esiste ancora `transfer`.
- I secret tunnel hanno gia il doppio path: relay TCP e diretto UDP con fallback automatico.
- Il consumer richiede gia al server l'hint dello STUN selezionato dal provider prima di raccogliere i candidati (`UdpStunHintRequest`, `UdpCandidateOffer`, `UdpPunch`).
- Il path diretto usa QUIC con stream nativi separati per connessione, quindi niente HOL blocking tra stream distinti.
- Il relay usa gia streaming puro con `copy_bidirectional_with_sizes`, quindi il server non bufferizza l'intero contenuto.
- Sul relay esiste gia il carrier pool, sia lato provider sia lato consumer, per distribuire piu stream su piu connessioni TCP.
- `test-udp` ha gia pairing, report, telemetria e un controllo abbastanza ricco del path UDP, ma e diagnostico, non un protocollo dati di produzione.

Conclusione pratica: il problema non e costruire da zero il trasporto, ma aggiungere un protocollo applicativo di transfer sopra primitive di trasporto gia esistenti.

## Valutazione di fattibilita

| Area | Fattibilita | Note |
| --- | --- | --- |
| File singolo | Alta | Si appoggia bene al path secret esistente |
| Directory ricorsiva | Alta | Serve un manifest applicativo, non basta uno stream raw |
| `stdin` raw | Alta | Corretta a livello byte-stream; piu limitata a livello semantico |
| UDP diretto con fallback relay | Alta | Esiste gia nel path secret |
| Relay senza storage server-side | Alta | Il relay attuale e gia stream-based |
| Progress live | Alta | Facile per file/directory, parziale per `stdin` senza size nota |
| Verifica end-to-end robusta | Alta | Consigliato BLAKE3, non CRC come meccanismo principale |
| Accelerazione relay con carriers | Media | Utile per concorrenza; non accelera da solo un singolo stream grosso |
| Resume/restart | Media | Possibile, ma richiede protocollo a chunk e stato persistente locale |
| Symlink/device | Media | Fattibile, ma con implicazioni di sicurezza e portabilita |

## Architettura consigliata

### Scelta raccomandata

La soluzione migliore non e costruire il transfer sopra `test-udp` come percorso principale, ma:

1. aggiungere una nuova CLI `transfer`
2. riusare internamente le primitive dei secret tunnel per:
   - autenticazione
   - pairing tramite transfer id
   - tentativo diretto UDP
   - fallback relay
   - carrier pool sul relay
3. definire sopra quel trasporto un protocollo applicativo di transfer dedicato

### Perche non usare direttamente `test-udp` come protocollo transfer

`test-udp` e molto utile, ma e orientato a:

- diagnosi
- reportistica umana
- pairing one-shot di test
- misure e telemetria

Non e un protocollo dati di produzione per file transfer. La parte riusabile e la logica di pairing/negoziazione e la visibilita del path, non il flusso applicativo del comando diagnostico.

### Modello dei ruoli

- `listener`: registra una sessione di transfer e riceve i contenuti nella destinazione
- `sender`: si aggancia alla stessa sessione e invia manifest + dati
- `server`: mantiene solo metadati minimi di sessione e fa da relay quando serve

### Transfer ID

Serve comunque un identificatore comune tra i due peer. Senza id condiviso il pairing non e deterministico.

Scelta consigliata:

- esporre un flag applicativo `--transfer-id`
- accettare `--tcp-secret-id` come alias compatibile/interno
- se il listener non riceve un id, generarlo automaticamente e stamparlo chiaramente
- il sender deve poi ricevere quell'id esplicitamente

Questa e la sola variante coerente con il requisito "autogenerato o utente" senza introdurre discovery lato server molto piu invasiva.

## Protocollo applicativo consigliato

Il transfer non dovrebbe essere uno stream opaco unico per tutti i casi. Serve un envelope applicativo.

### Sessione

La sessione dovrebbe avere almeno questi step logici:

1. handshake iniziale con transfer id, ruolo, path scelto, modalita di cifratura visibile a log
2. preflight del receiver su destinazione e policy (`fail`, `overwrite`, `rename`)
3. invio del manifest o dei metadati di stream
4. trasferimento dei payload
5. verifica hash/dimensioni/contatori
6. `commit` finale del receiver
7. ack finale di successo o errore dettagliato

### Tipi di trasferimento

#### 1. File singolo

Metadati minimi:

- nome file
- size attesa
- permessi opzionali
- mtime opzionale
- hash BLAKE3 del file

Flow consigliato:

- il receiver crea un file temporaneo nella stessa directory finale
- il sender streama i byte
- entrambi calcolano BLAKE3 mentre streamano
- il receiver fa `fsync`
- se size e hash coincidono, fa rename atomico sul nome finale

#### 2. Directory ricorsiva

Qui uno stream raw non basta. Serve un manifest nativo.

Manifest per entry:

- path relativo normalizzato
- tipo (`file`, `dir`, `symlink`, `device` se ammesso)
- size per i file
- permessi opzionali
- mtime opzionale
- hash BLAKE3 per i file
- target del symlink se incluso

Flow consigliato:

- il sender fa una scansione iniziale della directory
- invia il manifest completo prima dei dati
- il receiver fa preflight completo su collisioni e policy
- solo se il preflight passa inizia il data transfer
- i contenuti arrivano in staging root temporanea
- al termine si verifica tutto e si fa commit della root

#### 3. `stdin`

`stdin` e un caso diverso: non esiste un manifest filesystem. Esiste un singolo byte stream opaco.

Metadati minimi:

- `--output` obbligatorio
- tipo transfer = `stdin-stream`
- hash finale del byte stream
- size finale se nota solo a posteriori

Flow consigliato:

- il receiver scrive in file temporaneo con il nome di output richiesto
- il sender legge da `stdin` e calcola hash sul flusso reale letto
- il receiver calcola lo stesso hash sul flusso ricevuto
- dopo EOF, se hash coincide, `fsync` + rename

## Integrita e correttezza: CRC o altro?

## Risposta breve

CRC da solo non e la scelta giusta come garanzia principale di correttezza.

Scelta consigliata:

- BLAKE3 come digest primario end-to-end
- size attesa come controllo addizionale
- staging + `fsync` + rename atomico dove possibile
- manifest aggregato per directory

### Perche il CRC non basta

Il trasporto sottostante gia rileva gran parte degli errori accidentali:

- TCP gestisce ritrasmissione e checksum del segmento
- QUIC aggiunge controlli ancora piu robusti a livello di frame/packet
- TLS/QUIC proteggono anche l'integrita crittografica del payload in transito

Il problema vero non e il bit-flip sulla rete, ma la correttezza end-to-end del contenuto finale:

- file tronco
- scrittura disco incompleta
- stream chiuso prematuramente
- collisioni/overwrite inattesi
- mismatch tra quello che il sender crede di avere inviato e quello che il receiver ha persistito

CRC32/CRC64 sono buoni per errore accidentale veloce, ma come garanzia principale sono inferiori a un digest crittografico.

### Perche BLAKE3 e la scelta migliore qui

Vantaggi:

- molto veloce
- progettato per hashing incrementale e parallelizzabile
- adatto sia a file grandi sia a stream
- piu robusto del CRC per collisioni e verifiche applicative

SHA-256 e comunque una alternativa valida, ma BLAKE3 e piu adatto a un tool di trasferimento orientato a performance.

## Analisi approfondita: correttezza del transfer streaming tipo tar

Questo e il punto piu importante del documento.

### Cosa si puo garantire davvero in `stdin`

Con un comando come:

```bash
tar -cvpzf - myfolder | bore transfer sender --source stdin --output archive.tar.gz
```

`bore` vede solo un flusso di byte su `stdin`.

Puo garantire in modo robusto che:

- il receiver ha ricevuto esattamente gli stessi byte che il sender ha letto
- il file finale scritto dal receiver coincide byte-per-byte con quello stream inviato
- il trasferimento e completo rispetto all'EOF visto dal sender

Non puo garantire da solo che:

- il comando `tar` a monte abbia avuto successo semantico
- l'archivio sia "logicamente corretto" rispetto alla directory sorgente originale
- l'archivio compresso sia estraibile senza errore futuro

### Perche non puo sapere se il producer a monte e andato bene

Quando `bore transfer sender --source stdin` legge da `stdin`, vede solo:

- byte
- eventuale errore di lettura
- EOF

Non vede automaticamente l'exit code del processo a monte nella pipeline.

Quindi:

- se `tar` fallisce dopo avere emesso uno stream parziale, il sender puo vedere solo un EOF anticipato
- il trasferimento puo risultare perfettamente coerente byte-per-byte, ma trasportare un archivio incompleto

Questo non e un bug di `bore`, e un limite strutturale delle pipeline Unix quando il reader non controlla direttamente il producer.

### Implicazione pratica per `stdin`

La semantica corretta deve essere dichiarata in modo esplicito:

- per `stdin`, `bore` garantisce identita byte-per-byte dello stream trasferito
- non garantisce il successo semantico del comando che ha prodotto lo stream

Per avere una garanzia piu forte servono alternative diverse:

- usare `set -o pipefail` lato shell e controllare l'esito della pipeline
- introdurre in futuro una modalita `--source-command ...` in cui `bore` lancia lui il producer e puo verificarne l'exit code
- trasferire directory/file in modalita nativa `bore transfer`, dove il tool possiede il manifest e quindi puo verificare semanticamente il risultato

### Cosa verificare in `stdin`

Per `stdin`, il contratto corretto e:

- hash BLAKE3 finale dello stream
- conteggio byte effettivo ricevuto
- `fsync` del file destinazione
- rename finale solo dopo ack positivo

Questo e sufficiente per garantire il trasferimento corretto dello stream. Non sostituisce la validazione dell'archivio da parte di `tar`, `gzip`, `xz` o del tool che consumera il file dopo.

## Integrita directory/file: strategia consigliata

### File singolo

Garanzia consigliata:

- size attesa
- hash BLAKE3
- staging file
- `fsync`
- rename atomico

### Directory

Garanzia consigliata:

- manifest iniziale completo
- hash per file
- conteggio entry atteso
- byte totali attesi
- hash aggregato del manifest finale
- staging root separata
- commit finale solo se tutto torna

### Hash aggregato directory

Per una directory intera conviene avere sia:

- hash per singolo file
- hash aggregato del transfer

L'hash aggregato puo essere calcolato su una sequenza canonica di record del tipo:

`relative_path + tipo + size + hash_file + metadata_rilevante`

ordinata lessicograficamente.

Questo permette di verificare l'insieme completo anche senza rileggere integralmente tutta la directory dopo il commit.

## Policy su collisioni, overwrite e rename

Richiesta utente corretta: default fail-safe.

Scelta consigliata:

- default: fallire su qualunque collisione
- `--overwrite`: sovrascrivere esplicitamente
- `--rename`: rinominare in modo deterministico ed esplicito
- `--overwrite` e `--rename` devono essere mutualmente esclusivi

### Nota importante sull'atomicita delle directory

Se la directory finale esiste gia, l'atomicita perfetta dell'overwrite dell'intero tree non e banale.

Caso forte e semplice:

- sorgente directory `myfolder`
- destinazione finale `/dest/myfolder`
- `/dest/myfolder` non esiste

Qui si puo fare staging in `/dest/.bore-tmp-<id>/myfolder` e poi rename finale atomico della root.

Caso piu debole:

- `/dest/myfolder` esiste gia
- si vuole `--overwrite`

Qui l'overwrite davvero atomico dell'intero tree e molto piu difficile. Per la prima versione orientata alla correttezza e meglio:

- default fail
- `--rename` pienamente supportato
- `--overwrite` ammesso, ma documentato come piu complesso e da introdurre solo dopo una fase robusta di staging/replace

In pratica, la richiesta utente spinge nella direzione giusta: se c'e ambiguita o rischio di merge parziale, il transfer deve fallire.

## Symlink e file speciali

Questa area richiede policy esplicite.

### Raccomandazione

- `--symlinks include|exclude`
- `--devices include|exclude`

Default consigliato:

- symlink: `include` solo se preservati come link, mai dereferenziati implicitamente
- device: `exclude`

Vincoli di sicurezza obbligatori:

- rifiutare path assoluti
- rifiutare `..` che escono dalla root di destinazione
- validare i target dei symlink per evitare traversal non voluto
- documentare che i device sono Unix-specifici e spesso richiedono privilegi elevati

## Sicurezza del canale

### Relay

Il requisito utente e coerente con l'architettura attuale:

- se il controllo/relay usa `https://` o TLS, il transfer relay risulta cifrato in transito
- se il server e usato in plain TCP, il relay resta plain

Questa scelta puo restare trasparente all'utente, ma deve essere loggata chiaramente.

### UDP diretto

Il path diretto attuale usa QUIC. QUIC implica cifratura del canale.

Quindi:

- relay: cifrato o plain in base alla configurazione bore gia esistente
- direct UDP: cifrato per costruzione del path QUIC

### Logging minimo obbligatorio

Lato sender e listener:

- `path=direct-udp` oppure `path=relay`
- `relay_security=tls` oppure `relay_security=plain`
- `direct_security=quic-encrypted`
- motivo del fallback, se c'e
- transfer id

## Performance e carriers: punto critico

Il server puo gia restare un puro relay streaming. Questo requisito e fattibile.

Serve pero chiarire un punto tecnico importante: i carriers non accelerano automaticamente un singolo file grosso se il protocollo applicativo usa un solo stream dati.

I carriers aiutano quando ci sono:

- molti file in parallelo
- piu stream concorrenti
- chunk striping su piu stream

Per un unico file di grandi dimensioni trasferito su un solo stream:

- il carrier pool da solo non crea parallelismo reale sul payload
- il throughput resta legato a quel singolo stream/logical flow

### Implicazione progettuale

Per la prima versione:

- usare un singolo stream per file e accettare che i carriers aiutino soprattutto directory con piu file o futuri trasferimenti concorrenti

Per una fase successiva ad alte prestazioni:

- introdurre chunking del singolo file su piu stream paralleli
- oppure parallelismo per file nella directory

Il primo approccio e piu semplice e corretto. Il secondo e quello che sblocca davvero il beneficio dei carriers su file molto grandi.

## Progress e UX

### File singolo

Mostrare:

- bytes inviati/ricevuti
- bytes totali
- percentuale
- throughput istantaneo
- throughput medio
- ETA

### Directory

Mostrare:

- file correnti e file totali
- bytes cumulativi e bytes totali
- file corrente
- throughput e ETA

Per avere percentuali affidabili, il sender deve fare una pre-scan della directory prima di iniziare il payload.

### `stdin`

Se la size non e nota a priori, si puo mostrare solo:

- bytes trasferiti
- throughput
- tempo trascorso

La percentuale non e disponibile senza una size attesa. Questo va documentato chiaramente.

## Failover e comportamento operativo

Comportamento consigliato:

1. tentare il diretto UDP quando richiesto di default dal comando transfer
2. se il diretto fallisce o scade il timeout, passare a relay
3. loggare sempre il motivo del fallback
4. nessun auto-reconnect nella prima versione
5. errore chiaro e finale se il transfer si interrompe

Questo e coerente con la richiesta utente.

## Scelta consigliata per la V1

Per massimizzare qualita e ridurre rischio, la V1 dovrebbe essere cosi:

- subcomando `transfer` dedicato
- reuse del trasporto secret esistente
- relay o diretto scelto automaticamente
- protocollo nativo con manifest per file/directory
- `stdin` come byte-stream opaco con hash end-to-end
- default fail su collisioni
- `--rename` supportato prima di `--overwrite`
- nessun resume
- nessuna compressione gestita da bore

## Piano implementativo per fasi

## Fase A - Design e protocolli

### A1 - CLI e semantica operativa

Definire:

- `bore transfer listener`
- `bore transfer sender`
- `--transfer-id` con alias dell'id esistente
- `--dest-path`, `--source`, `--output`, `--overwrite`, `--rename`
- flag special files (`--symlinks`, `--devices`)

Test:

- unit test di parsing CLI
- test di conflitto flag (`overwrite` vs `rename`)
- test casi obbligatori (`stdin` richiede `--output`)

Documentazione:

- help CLI
- sezione README/USER_GUIDE

### A2 - State machine del transfer

Definire i messaggi applicativi del transfer:

- hello
- manifest begin/end
- entry header
- payload chunk o stream associato
- commit
- ack/fail

Test:

- unit test sulla serializzazione/deserializzazione dei messaggi
- test su transizioni valide/non valide della state machine

Documentazione:

- protocol overview
- tabella stati/errore

## Fase B - Relay MVP corretto

### B1 - File singolo su relay

Implementare il caso piu semplice sopra il relay TCP.

Test:

- e2e file piccolo
- e2e file grande
- hash mismatch simulato
- interruzione a meta trasferimento
- destinazione esistente -> fail

Documentazione:

- quick start file singolo
- note su staging e atomicita

### B2 - Directory ricorsiva su relay

Implementare manifest + staging root + commit finale.

Test:

- e2e directory profonda
- permessi basilari
- collisione path
- symlink include/exclude
- nomi strani/spazi
- ordine non rilevante ma verifica manifest finale coerente

Documentazione:

- semantica preservazione struttura
- limiti e policy su symlink/device

### B3 - `stdin` su relay

Implementare stream opaco con `--output` obbligatorio.

Test:

- pipe semplice da `stdin`
- stream grande
- chiusura anticipata del producer
- nome output obbligatorio

Documentazione:

- garanzia reale: byte-stream identity
- nota esplicita su `pipefail`

## Fase C - Integrita e commit robusto

### C1 - Hash end-to-end

Introdurre BLAKE3 incrementale per tutti i modi.

Test:

- digest coerente sender/receiver
- mismatch intenzionale
- dimensione corretta ma hash errato

Documentazione:

- perche BLAKE3
- differenza tra integrita del trasporto e integrita end-to-end

### C2 - Staging, `fsync`, rename

Rendere robusto il commit finale.

Test:

- crash simulato prima del rename
- pulizia temporanei
- nessuna esposizione del file finale prima della verifica

Documentazione:

- semantica del commit
- atomicita supportata e non supportata

### C3 - Hash aggregato directory

Aggiungere verifica dell'insieme completo.

Test:

- file mancante
- file extra in staging
- ordine entry differente ma hash aggregato canonico stabile

Documentazione:

- formato canonico del manifest hashato

## Fase D - Integrazione UDP diretto

### D1 - Reuse del path secret esistente

Usare il tentativo diretto UDP e fallback relay gia presenti.

Test:

- e2e transfer diretto UDP riuscito
- e2e fallback relay quando il diretto fallisce
- timeout di negoziazione

Documentazione:

- come viene scelto il path
- log attesi del fallback

### D2 - Log di path e cifratura

Rendere trasparente la modalita effettiva usata.

Test:

- snapshot test o assert sui log principali
- direct path -> log `quic-encrypted`
- relay TLS/plain -> log coerente

Documentazione:

- esempi log sender/listener

### D3 - Limiti e performance

Validare l'effetto reale di carriers e stream nativi.

Test:

- molti file piccoli in parallelo
- file singolo grande
- confronto relay 1 carrier vs N carriers

Documentazione:

- chiarire dove i carriers aiutano e dove no

## Fase E - UX e osservabilita

### E1 - Progress live

Implementare progress coerente nei tre modi.

Test:

- percentuale file singolo
- percentuale directory
- solo bytes/speed su `stdin`

Documentazione:

- significato dei contatori mostrati

### E2 - Errori chiari

Rendere i fallimenti autoesplicativi.

Test:

- destinazione esistente
- hash mismatch
- stream interrotto
- producer pipe terminato presto
- fallback relay fallito

Documentazione:

- catalogo errori frequenti e remediation

## Fase F - Policy filesystem avanzate

### F1 - Symlink e device

Implementare le policy scelte.

Test:

- symlink inclusi
- symlink esclusi
- symlink con traversal vietato
- device esclusi/inclusi su piattaforme supportate

Documentazione:

- implicazioni di sicurezza e portabilita

### F2 - `--rename` e poi `--overwrite`

Introdurre prima la policy piu sicura.

Ordine consigliato:

1. `fail` di default
2. `--rename`
3. `--overwrite`

Test:

- collisione nome -> fail
- collisione nome -> rename
- overwrite esplicito

Documentazione:

- priorita delle policy

## Aggiornamento stato implementazione - V2 completata

La parte sopra resta valida come ragionamento architetturale, ma non descrive piu lo stato reale del repository. La V2 e ora implementata.

### Stato A - Protocollo e CLI

Completato.

- subcomando `bore transfer listener|sender` attivo
- `--transfer-id` come identificatore esplicito del rendezvous
- alias legacy `--tcp-secret-id` mantenuto lato transfer
- `--source stdin` + `--output NAME` supportati

### Stato B - Correttezza del transfer filesystem

Completato.

- manifest applicativo dedicato
- staging sotto la destinazione e commit finale solo dopo verifica
- collision policy `fail` di default, con `--overwrite` e `--rename`
- hash BLAKE3 finale per file e transfer

### Stato C - Portabilita path Linux / macOS / Windows

Completato sul wire format.

- i path non passano piu come UTF-8 obbligatorio
- componenti Unix trasportati come byte raw codificati
- componenti Windows trasportati come UTF-16LE codificato
- il receiver ricostruisce `OsString`/`PathBuf` nativi
- resta intenzionale il rifiuto di nomi non validi sul filesystem di destinazione

### Stato D - Chunking, resume e parallelismo reale

Completato per i transfer filesystem.

- chunking deterministico dei file regolari
- resume state persistito lato receiver
- handshake di resume tra sender e listener
- worker multipli su stream distinti
- `--parallel N` per parallelismo applicativo vero
- integrazione con `--carriers N` sul relay

Nota importante:

- il beneficio su file grossi in relay arriva dal chunking su piu stream, non dai carriers da soli
- il path diretto UDP usa gia stream QUIC nativi indipendenti, quindi non ha bisogno di carrier pool

### Stato E - `stdin`

Completato con vincoli espliciti.

- `stdin` e trattato come byte-stream opaco verificato end-to-end
- richiede `--output`
- usa un solo stream
- non fa resume
- non usa `--parallel`

### Stato F - Test e regressione

Completato con copertura dedicata sia library-level sia subprocess reale.

Copertura principale:

- relay singolo file
- directory con struttura preservata
- diretto UDP
- file zero-byte
- file grande con `parallel + carriers`
- resume dopo interruzione forzata
- nomi/path non UTF-8 su Unix
- `stdin` via subprocess reale: vuoto, piccolo, grande, overwrite, rename, errore senza `--output`, diretto UDP, output non UTF-8 su Unix

### Stato G - Limiti residui della V2

Ancora intenzionali:

- niente resume per `stdin`
- resume valido solo se `transfer-id` e manifest restano coerenti tra i tentativi
- device nodes restano Unix-only
- il receiver non fa traduzione automatica di nomi invalidi cross-OS; fallisce esplicitamente

## Fase H - Lavoro futuro sensato

La prossima fase non e piu "aggiungere resume", perche ormai c'e. Le evoluzioni sensate sono:

### H1 - Scheduling e throughput

- scheduler chunk piu sofisticato
- bilanciamento migliore tra file grandi e molti file piccoli
- benchmark comparativi relay/direct con varie combinazioni di `--parallel` e `--carriers`

### H2 - Progress e osservabilita

- progress piu ricco per chunk/file/throughput istantaneo
- metriche di resume piu esplicite nei log
- eventuale esposizione admin/telemetria del transfer

### H3 - Hardening cross-platform

- suite dedicata Windows/macOS in CI per casi path-edge
- coverage piu ampia su collisioni e differenze di filesystem

## Raccomandazione finale aggiornata

La direzione architetturale si e rivelata corretta. Il progetto oggi ha una V2 coerente con il design di bore, a condizione di mantenere questi principi anche nelle prossime iterazioni:

- riusare il trasporto secret esistente invece di reinventare UDP/relay
- tenere il protocollo di transfer esplicito e verificabile
- mantenere staging + hash + commit come barriera di correttezza
- trattare `stdin` come identita byte-stream, non come semantica del producer
- distinguere chiaramente il valore dei carriers da quello del chunking parallelo
- preferire failure esplicite a rinomini o adattamenti impliciti non richiesti