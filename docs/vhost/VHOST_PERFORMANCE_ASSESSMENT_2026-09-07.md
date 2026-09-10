# Vhost: margini di ottimizzazione di banda e latenza

Analisi del 7 settembre 2026, commit `f15a3de`, branch `main`. Modello: GPT-6. Esame dei sorgenti correnti e della documentazione delle versioni effettivamente bloccate in `Cargo.lock`: Quinn 0.11.9, quinn-proto 0.11.15, quinn-udp 0.5.14, rustls 0.23.40, yamux 0.13.10.

## Valutazione

**Sì, esistono margini concreti**, soprattutto per la latenza sotto carico e per i siti con molte connessioni brevi. Le ottimizzazioni fondamentali del trasporto sono però già presenti. Le priorità nuove sono: selezionare carrier e percorso in base al carico, limitare le attese prima del fallback, riutilizzare configurazioni TLS e, con un intervento più ampio, connessioni backend.

Per un singolo download lungo, i candidati cambiano: copie dei dati, costo crittografico, controllo della congestione e limiti effettivi di rete. Aumentare i carrier non divide quel download fra più connessioni.

Qui “banda” distingue **velocità utile** e **byte trasferiti**; “latenza” distingue apertura, tempo al primo byte (TTFB) e completamento, includendo p95/p99 sotto carico. Non sono stati eseguiti nuovi benchmark: i punti verificati sono proprietà del codice; entità e frequenza dei benefici restano da misurare sul carico reale. Nessuna modifica al comportamento dell'applicazione.

## Percorso attuale e lavoro già fatto

Il browser raggiunge il server bore, che inoltra al provider e quindi al servizio locale. Il server rimane nel percorso anche con `--udp`: non nasce un collegamento browser-provider. Nel vhost nativo TCP il tratto server-provider usa yamux su TCP, eventualmente TLS; in UDP usa stream affidabili QUIC. Ogni connessione pubblica occupa un substream, con le successive richieste HTTP/1.1 keep-alive sulla stessa connessione. Il percorso HTTP del vhost non implementa attualmente HTTP/2 o HTTP/3.

Riferimenti: [relay_vhost](../../src/vhost.rs#L876), [handle_connection](../../src/client.rs#L1463), [comportamento browser e WebSocket](../../README.md#L446).

| Ottimizzazione | Stato verificato |
| --- | --- |
| Connessioni persistenti e multiplexing | Già presenti: nessun nuovo handshake TCP/TLS del **carrier** per ogni richiesta. [mux.rs](../../src/mux.rs#L1) |
| Carrier TCP e QUIC multipli | Già presenti; distribuzione per connessione, non per pacchetto. Default vhost: 1. [main.rs](../../src/main.rs#L438) |
| QUIC con BBR | Già configurato esplicitamente; non è una novità da implementare. [holepunch.rs](../../src/holepunch.rs#L3052) |
| Finestre QUIC ampie | Ricezione: 16 MiB per stream, 256 MiB per connessione; invio: 256 MiB. Limite: 4096 stream. [shared.rs](../../src/shared.rs#L217) |
| Buffer UDP | Richiesti 16 MiB per direzione; gestione del limite kernel e segnalazione del clamp già presenti. [holepunch.rs](../../src/holepunch.rs#L1199) |
| Copia e socket TCP | Buffer di copia configurabile, default 256 KiB per direzione; `TCP_NODELAY` e keepalive già applicati. [shared.rs](../../src/shared.rs#L157), [tune_tcp](../../src/shared.rs#L268) |
| Correzioni di concorrenza e parsing | Già risolti la finestra QUIC condivisa troppo piccola, i flush TLS mancanti e la scansione ripetuta dell'intero header. [fix QUIC](../VHOST_UDP_CONCURRENCY_FIX.md), [fix flush](../VHOST_INJECTED_FLUSH_FIX.md), [parser](../../src/vhost.rs#L1468) |
| Offload UDP | Quinn abilita GSO per default; quinn-udp espone offload in invio/ricezione. Disponibilità reale dipendente dall'ambiente. [TransportConfig](https://docs.rs/quinn-proto/0.11.15/quinn_proto/struct.TransportConfig.html#method.enable_segmentation_offload), [quinn-udp](https://docs.rs/quinn-udp/0.5.14/quinn_udp/) |

QUIC elimina l'obbligo di consegna ordinata **fra stream** imposto dal TCP del carrier; rimangono condivisi congestione, capacità del collegamento e credito di connessione. Non significa banda illimitata o isolamento completo da un consumatore lento.

## Miglioramenti non ancora implementati

### 1. Selezione dei carrier e del percorso in base al carico

**Evidenza:** [CarrierPool::pick](../../src/pool.rs#L109) e [DirectPool::pick](../../src/vhost.rs#L506) usano round-robin. Non considerano stream attivi per carrier, pressione sulle code, RTT o tempo di apertura. Inoltre, [relay_vhost](../../src/vhost.rs#L914) preferisce sempre QUIC quando una connessione diretta è disponibile, senza confrontarne le prestazioni con TCP.

**Proposta:** introdurre contatori per carrier con rilascio RAII e una selezione inizialmente semplice: confrontare pochi candidati usando carico e tempi di apertura. Per QUIC, integrare le statistiche di connessione già esposte da [DirectConn::stats](../../src/holepunch.rs#L2322). Successivamente valutare una politica TCP/QUIC basata su misure, con isteresi e sonde limitate per evitare oscillazioni.

**Beneficio atteso:** p95/p99 inferiori con download lunghi, WebSocket, client lenti e richieste brevi concorrenti; migliore utilizzo dei carrier disponibili. Il numero di stream da solo non basta: uno stream inattivo pesa diversamente da un trasferimento saturo. La politica sceglie il percorso delle **nuove connessioni**; non sposta automaticamente quelle già aperte e non duplica richieste HTTP per confrontare le prestazioni. Priorità alta; complessità media.

### 2. Attesa limitata e retry sicuro nell'apertura del percorso

**Evidenza:** [relay_vhost](../../src/vhost.rs#L916) attende `direct.open_stream()` senza un timeout locale. Se fallisce prova un carrier TCP; se l'apertura di quel carrier fallisce, non prova gli altri. La scrittura del marker QUIC usa `?` e non entra nel ramo di fallback. [open_stream](../../src/holepunch.rs#L2289) delega a `open_bi()`, che può attendere disponibilità di stream.

**Proposta:** applicare un budget complessivo all'apertura, saltare temporaneamente i carrier problematici, provare un altro carrier idoneo e poi il relay caldo. Il budget deve adattarsi al RTT osservato: un limite fisso aggressivo penalizzerebbe collegamenti sani ma lontani.

**Beneficio atteso:** soprattutto latenza di coda durante saturazione e guasti parziali, oltre a meno errori transitori. Non aumenta il picco di banda su un collegamento sano. Il retry deve avvenire prima dell'invio della richiesta applicativa; dopo scritture HTTP parziali non è sicuro ripetere genericamente POST o upload. La cancellazione deve chiudere le risorse già aperte. Priorità alta; complessità media.

### 3. Riutilizzo del client TLS backend; poi pooling HTTP

**Evidenza:** con `--backend-tls`, [relay_vhost](../../src/vhost.rs#L960) chiama `insecure_tls_connector()` per ogni nuova connessione pubblica. Il [costruttore](../../src/transport.rs#L195) crea un nuovo `ClientConfig`, quindi non conserva fra queste connessioni la cache di sessione. Il provider apre inoltre una nuova connessione al servizio locale per ogni nuovo substream ([client.rs](../../src/client.rs#L1482)). Il keep-alive della stessa connessione pubblica è già riutilizzato.

**Intervento contenuto:** conservare il connector/config TLS per vhost e destinazione backend, isolando identità e impostazioni. Rustls supporta già la ripresa di sessione; condividere correttamente la configurazione permette di sfruttarla quando il backend emette ticket. Riduce costruzioni e lavoro dell'handshake. Con TLS 1.3, la ripresa ordinaria non garantisce di eliminare un RTT: non equivale al riuso di una connessione aperta. [Documentazione rustls](https://docs.rs/rustls/0.23.40/rustls/client/struct.ClientConfig.html#sharing-resumption-between-clientconfigs).

**Intervento strutturale:** terminazione HTTP per richiesta e pool di connessioni backend persistenti, riusabili anche fra connessioni browser diverse. Questo può evitare aperture e handshake ripetuti, compreso l'handshake backend TLS che oggi attraversa il tunnel WAN. Serve però gestire framing, body, cancellazione, header hop-by-hop, autenticazione e destinazioni; un semplice riciclo di socket nel byte-splice attuale non è sufficiente. WebSocket e upgrade richiedono un percorso dedicato.

Priorità alta per il riuso del connector quando `--backend-tls` è frequente; pooling da valutare per carichi web con molte connessioni brevi. Guadagno ridotto per download lunghi o connessioni già persistenti.

### 4. Numero di carrier adattivo

**Evidenza:** il vhost ha un numero configurato, default 1. Il ripristino dei carrier mancanti mantiene quel numero; non lo ridimensiona in funzione del traffico. Per QUIC, [clamp_direct_carriers](../../src/vhost.rs#L459) tratta anche 0 come 1. L'idea di un valore automatico era già proposta nel [precedente audit](../VHOST_AUDIT.md#L245), ma non risulta implementata.

**Proposta:** una modalità automatica esplicita, con minimo, massimo, crescita su pressione persistente e riduzione a inattività prolungata. Controllare separatamente la necessità di carrier QUIC attivi e il costo del relay mantenuto caldo; rispettare i limiti di entrambi i pool e mantenere invariato `--carriers 1`.

**Beneficio atteso:** adattamento della concorrenza senza una scelta manuale unica per tutti i carichi. Non promette di accelerare un singolo flusso; molti carrier aggiungono handshake, memoria, CPU e competizione sulla stessa rete. Da implementare dopo la telemetria del punto 1, non semplicemente impostando un default alto.

### 5. Percorso QUIC con meno copie e priorità applicative

**Evidenza:** [QuicTransport](../../src/holepunch.rs#L2427) espone `AsyncRead`/`AsyncWrite`; la copia passa attraverso buffer intermedi. Non utilizza le API a chunk o le priorità degli stream.

**Proposta per la banda:** misurare un percorso specializzato che legga chunk ordinati Quinn e usi scritture aggregate dove vantaggiose. `read_chunk(..., true)`/`read_chunks()` possono evitare una copia in ricezione; non rendono l'intera catena TLS/TCP “zero-copy”. Vanno confrontati costo delle copie, frammentazione e numero di chiamate, in particolare sul provider. [API RecvStream](https://docs.rs/quinn/0.11.9/quinn/struct.RecvStream.html#method.read_chunks).

**Proposta per la latenza:** poche classi esplicite di priorità, o carrier riservati alle connessioni interattive. Quinn offre `set_priority`, ma questa agisce sull'invio locale: per privilegiare le risposte occorre applicarla anche al provider. Non risolve da sola un credito di connessione già esaurito; serve evitare starvation. [API SendStream](https://docs.rs/quinn/0.11.9/quinn/struct.SendStream.html#method.set_priority).

Priorità media e condizionata al profiling. Se la rete è già satura e la CPU ha margine, eliminare copie potrebbe non cambiare la velocità osservata. Sul percorso condiviso con yamux deve restare un solo task proprietario dello stream.

### 6. HTTP/2 e HTTP/3 sul lato browser

Il proxy vhost attuale instrada header HTTP/1.x e poi inoltra byte. HTTP/2 consentirebbe richieste concorrenti su una connessione browser; HTTP/3 estenderebbe al tratto browser-server l'indipendenza degli stream rispetto alla consegna TCP. È un miglioramento potenziale del caricamento delle pagine, distinto dal QUIC interno già disponibile. [RFC 9113, stream](https://www.rfc-editor.org/rfc/rfc9113.html#section-5), [RFC 9114, panoramica](https://www.rfc-editor.org/rfc/rfc9114.html#section-2).

Non basta aggiungere `h2` all'ALPN: servono terminazione HTTP e mappatura delle richieste sui percorsi backend. Inoltrare tutte le richieste HTTP/2 dentro un unico stream ordinato QUIC perderebbe parte dell'isolamento cercato. Un reverse proxy davanti a bore può già offrire HTTP/2 o HTTP/3 ai browser; il supporto nativo richiede un progetto più ampio. Priorità legata all'uso come front-end web, non al solo trasferimento bulk.

## Altri interventi, con beneficio più circoscritto

- **Allocazioni sul percorso frequente:** rendere condizionale `head_for_logging = head.clone()` quando il logger è assente ([vhost.rs](../../src/vhost.rs#L979)); valutare buffer di copia riutilizzabili o dimensionati dinamicamente. Con i default, i due buffer di una copia bidirezionale valgono circa 512 KiB per endpoint della connessione. È soprattutto un tema di memoria, allocazioni e richieste al secondo; ogni riduzione va verificata anche sul bulk.
- **Controllo della congestione selezionabile:** QUIC forza BBR, mentre `tune_tcp` non imposta un algoritmo TCP e lascia la politica al sistema. Un'opzione per confrontare controllori è nuova; “abilitare BBR QUIC” è già fatto. Scegliere sulla base di throughput e p99 insieme, preservando i comportamenti degli altri modi che condividono `transport_config`.
- **Cache e compressione HTTP:** una cache sul server può evitare di attraversare il tunnel per asset riutilizzabili; contenuti precompressi dal backend possono ridurre i byte nel tunnel già oggi. La compressione applicata soltanto all'uscita browser non riduce i byte server-provider. La cache nativa non è presente nel relay esaminato e richiederebbe regole HTTP corrette per autenticazione, varianti e invalidazione. Compressione generica dei byte del tunnel avrebbe benefici molto dipendenti dal contenuto e costi CPU: priorità bassa.

## Cosa non proporrei come nuova ottimizzazione

Aumentare indiscriminatamente finestre, buffer o carrier; rimuovere i flush; togliere `STREAM_READY`; spezzare yamux in due task; distribuire pacchetti di una connessione affidabile fra carrier; introdurre QUIC 0-RTT come rimedio principale alla latenza delle richieste su carrier già persistenti. Queste scelte possono essere inutili o violare invarianti già protette.

Le finestre configurabili vanno rapportate al prodotto banda-RTT: per esempio, 1 Gbit/s a 50 ms richiede circa 6,25 MB in volo; 10 Gbit/s allo stesso RTT circa 62,5 MB. Sono dimensionamenti di protocollo, non prove del collo di bottiglia. I buffer dei socket UDP assorbono burst e ritardi del processo: non sono la finestra di flow control QUIC e `buffer_socket/RTT` non è un limite universale rigoroso.

Nei vhost forniti da OpenSSH puro, il tratto provider è SSH/TCP; `--udp` del provider nativo non si applica automaticamente. Migliorie del frontend HTTP e del TLS backend restano pertinenti; le modifiche al trasporto devono rispettare questa differenza.

## Verifica necessaria prima di scegliere l'implementazione

Esistono già [vhost_bench.sh](../../scripts/vhost_bench.sh) e il [repro di concorrenza QUIC](../../scripts/vhost_udp_concurrency_repro.sh). Non ho avviato il primo: il suo cleanup contiene `pkill` su processi bore e server HTTP non limitati ai processi creati dal benchmark ([righe interessate](../../scripts/vhost_bench.sh#L37)). È da isolare prima di usarlo sulla workstation.

Anche la misurazione va resa uniforme: lo script riporta richieste/s con hey/wrk e MB/s con curl; il fallback di latenza usa nuove connessioni e `time_total`; l'origine è `python -m http.server`. Questi elementi non consentono di attribuire direttamente un risultato al solo trasporto. [Misurazione](../../scripts/vhost_bench.sh#L156), [origine](../../scripts/vhost_bench.sh#L119).

La matrice utile comprende TCP e QUIC con 1/2/4/8 carrier; un flusso bulk e traffico misto; richieste piccole, asset e client lenti; connessioni nuove e keep-alive; RTT e perdita variati separatamente. Confrontare con un'origine diretta equivalente, controllare che origine e generatore non saturino, misurare upload e download, con/senza TLS backend e logging. Eseguire gli harness netns serialmente.

Registrare goodput su un intervallo comune, byte sulla rete, TTFB e completamento p50/p95/p99, errori, CPU per processo/core, RSS, perdite e code per carrier. Le nuove metriche devono separare attesa del pool, apertura dello stream, connessione locale e handshake TLS. Servono warm-up e ripetizioni; un solo valore di MB/s non decide quale percorso funzioni meglio per un sito.

**Ordine consigliato:** misure affidabili; selezione/apertura dei carrier e riuso del connector TLS; poi pooling HTTP o copia QUIC secondo il collo di bottiglia osservato. HTTP/2/3 e cache hanno senso come evoluzione del reverse proxy, con un investimento maggiore.

## Metodo e limiti

L'indice TokenSave segnalava due file non aggiornati: è stato usato per orientamento, senza sincronizzarlo, e le conclusioni sono state verificate sui sorgenti. Le tre ricerche con contatore hanno riportato circa 2.800 token risparmiati, rettificando il conteggio intermedio. Le skill caveman e context-mode hanno contenuto la comunicazione e selezionato gli estratti; non hanno modificato il codice. Gli audit precedenti sono stati confrontati con l'implementazione attuale, evitando di riproporre correzioni già presenti. Nessun risultato di prestazione quantitativo è stato inventato o dedotto da test di sola correttezza.
