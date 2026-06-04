# Adaptive NAT per bore

Documento di analisi, confronto e piano incrementale per portare in bore un motore di NAT traversal piu adattivo, prendendo spunto da frp ma mantenendo i vincoli di questo progetto.

## Scopo del documento

Questo documento risponde a tre domande:

1. Cosa fa oggi bore nel path UDP diretto e cosa gli manca per diventare piu adattivo.
2. Cosa fa frp nella sua modalita P2P/xtcp che bore non fa ancora.
3. Come introdurre una versione adattiva in bore senza cambiare il comportamento e2e attuale, testando tutto e documentando tutto.

L'obiettivo non e copiare frp. L'obiettivo e capire quali idee di frp sono trasferibili in un progetto piu piccolo e piu rigido come bore.

## Vincoli di progetto

Le tre regole che guidano tutto il piano sono queste:

1. Il comportamento e2e attuale non deve cambiare.
2. Ogni cosa nuova deve essere coperta da test.
3. Ogni cosa nuova deve essere documentata.

In pratica questo significa:

- il relay deve rimanere sempre disponibile come fallback;
- il path UDP diretto deve restare opzionale;
- ogni nuova logica di NAT deve essere aggiunta in modo compatibile, con default che preservano il comportamento attuale;
- ogni nuova fase va chiusa con test ripetibili e con aggiornamenti alla documentazione.

## Stato attuale di bore

Il comportamento attuale e gia abbastanza ricco, ma e ancora piu vicino a un sistema di rendezvous e diagnosi che a un motore di traversal adattivo.

### I pezzi gia presenti

- [src/holepunch.rs](src/holepunch.rs) gestisce la scoperta STUN, la raccolta candidati, la logica di fallback e il path diretto QUIC.
- [src/secret.rs](src/secret.rs) orchestra provider e consumer, brokera il path UDP diretto e mantiene il relay come fallback garantito.
- [src/udp_diagnostic.rs](src/udp_diagnostic.rs) esegue la diagnostica paired, misura il direct path e il relay, e produce report operativi molto dettagliati.
- [src/shared.rs](src/shared.rs) contiene i messaggi e i dati condivisi, inclusi candidati, tuning QUIC e opzioni test.
- [TEST_UDP.md](TEST_UDP.md) documenta il path diagnostico, i segnali NAT e i casi operativi.
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md) descrive gia bene il modello NAT e il ragionamento su provider/consumer.
- [README.md](README.md) spiega il direct path UDP, i candidati, il fallback e i flag operativi.

### Comportamento attuale del direct path

Oggi bore fa queste cose:

- scopre gli indirizzi STUN pubblici del peer;
- puo aggiungere candidati locali, UPnP-IGD e porte predette;
- scambia i candidati attraverso il server;
- tenta il punch UDP e, se riesce, apre una connessione QUIC diretta;
- usa stream QUIC nativi per ogni connessione proxata;
- degrada al relay se il direct path non parte;
- puo ritentare e rilanciare il direct path in modo automatico in certe modalita.

Questo e sufficiente per la maggior parte dei casi buoni, ma non e ancora un motore di policy adattiva sul NAT.

## Cosa fa frp nella sua modalita P2P

frp, nella modalita xtcp/P2P, ha un subsystem piu articolato. Non fa solo rendezvous: fa anche classificazione, decisione e controllo del tentativo di hole punching.

### Flusso frp in breve

Il flusso concettuale di frp e questo:

1. Il proxy/visitor si presenta al server.
2. Il server esegue un precheck.
3. I due lati fanno prepare e raccolta NAT.
4. I due lati si scambiano le informazioni di traversal.
5. Il server calcola un comportamento di detect/punch per ciascun lato.
6. I peer eseguono la sequenza di punch suggerita.
7. Se il punch riesce, si apre il canale diretto.
8. Se non riesce, si puo ricadere su una modalita alternativa o su un fallback definito.

### I dettagli importanti di frp

Le cose piu interessanti da frp sono queste:

- `NatHoleVisitor` e `NatHoleResp` portano non solo i candidati, ma anche il comportamento di punching.
- `NatHoleDetectBehavior` contiene campi come `Role`, `Mode`, `TTL`, `SendDelayMs` e `ReadTimeoutMs`.
- Il server analizza sia gli `MappedAddrs` sia gli `AssistedAddrs` per classificare la NAT di entrambi i lati.
- La classificazione guida il modo in cui i candidati vengono tentati.
- I candidati assistiti non sono un extra decorativo: sono parte del piano di traversal.
- Il protocollo e la logica di controller portano piu informazione del semplice elenco di socket address.

In altre parole, frp ha un piccolo motore di politica NAT, non solo un rendezvous.

## Confronto funzionalita per funzionalita

### 1. Discovery STUN

frp:

- fa discovery e classificazione nello stesso sottosistema NAT hole;
- sfrutta le informazioni di mapping per decidere il comportamento successivo;
- usa la discovery come parte di una pipeline di decisione.

bore:

- fa discovery STUN molto bene;
- ha una chain di STUN live con fallback e hint peer-selected;
- usa i risultati soprattutto per selezionare candidati e per diagnosi.

Gap:

- in bore la discovery non diventa ancora un input formale di policy;
- manca una struttura che trasformi il risultato STUN in un piano di punching con timing e modalita di prova.

### 2. Classificazione NAT

frp:

- classifica la NAT dei due peer e usa la classificazione per scegliere `DetectBehavior`;
- distingue i casi in cui il comportamento di punch deve essere diverso;
- considera assisted addrs e mapped addrs come parte della decisione.

bore:

- classifica il NAT in modo diagnostico e utile per l'operatore;
- distingue cone, symmetric, CGNAT, port preservation e segnali correlati;
- pero non converte ancora questa classificazione in una strategia dinamica di traversal.

Gap:

- manca il passaggio da classificazione a plan;
- manca l'uso operativo della classe NAT per alterare ordine, timeout e tipo di candidati.

### 3. Assisted addresses

frp:

- gli assisted addrs sono parte del protocollo NAT hole;
- sono classificati e passati al controller;
- influenzano il comportamento di probe.

bore:

- esiste gia una forma di candidati assistiti: local candidate, UPnP-IGD, port prediction, STUN hint del peer;
- pero oggi sono trattati come lista di candidati o come hint operativi, non come categoria strutturata di assistenza NAT.

Gap:

- manca una tassonomia esplicita fra candidate base, assisted candidate, fallback candidate, predicted candidate e router-mapped candidate.

### 4. Detect behavior / plan di probing

frp:

- il server emette un comportamento di detect per ciascun lato;
- il comportamento include ruolo, modalita, TTL, delay di invio e timeout di lettura;
- il punch e quindi adattivo sul piano temporale e topologico.

bore:

- il direct path ha un ordine di tentativo pratico, ma non esiste un piano separato con parametri di comportamento;
- il timing del retry e concentrato nella logica client e nel timeout di negoziazione;
- il server non detta una politica dettagliata di probe.

Gap:

- manca una struttura equivalente a `DetectBehavior`;
- manca un piano di tentativi per NAT class con parametri espliciti.

### 5. Scelta del trasporto

frp:

- puo scegliere tra KCP e QUIC nella modalita xtcp;
- la scelta e parte della configurazione del visitor;
- il canale diretto non e monolitico.

bore:

- il direct path usa QUIC nativo come scelta unica e coerente;
- questo rende bore piu semplice e piu stabile, ma anche meno flessibile.

Gap:

- bore non ha una via alternativa di traversal o trasporto per casi NAT ostili;
- non esiste un equivalente di una modalita KCP o di una strategia transport-pluggable per il direct path.

### 6. Liveness, retry e persistenza del tunnel

frp:

- ha `keepTunnelOpen`, `MaxRetriesAnHour`, `MinRetryInterval`;
- il visitor puo auto-riprovare la costruzione del tunnel;
- la liveness e gestita come parte della modalita xtcp.

bore:

- ha reconnect automatico e upgrade/fallback nel path UDP diretto;
- monitora la chiusura del direct path e ricade sul relay;
- la persistenza e piu strettamente integrata con la semantica del tunnel secret.

Gap:

- bore non ha ancora un sottosistema esplicito di retry policy NAT-specific;
- la logica di retry esiste, ma non e modellata come comportamento adattivo osservabile.

### 7. Fallback

frp:

- prevede fallback a STCP o ad altri visitatori, quando configurato;
- il fallback puo essere parte della topologia del tunnel.

bore:

- il fallback e naturale e garantito: se il direct path non parte, il relay del server continua a funzionare;
- questo e un vantaggio architetturale enorme, perche semplifica l'adozione di logiche sperimentali.

Gap:

- bore non ha ancora un fallback policy-driven come frp, ma ha un fallback piu robusto a livello di servizio.

### 8. Osservabilita

frp:

- porta i segnali di NAT al controller, che decide e logga;
- la diagnostica e piu orientata al success/failure della hole punching.

bore:

- ha una diagnostica UDP molto ricca con RTT, loss, MTU, PLPMTUD, skew banda, tuning consigliato;
- questo rende bore piu forte sul lato operatore e piu debuggabile.

Gap:

- bore non usa ancora questi segnali per una policy adattiva;
- la diagnostica e ricca, ma la decisione e ancora minimalista.

## Conclusione del confronto

frp e piu vicino a un piccolo motore di NAT traversal adattivo.

bore e piu vicino a un tunnel semplice con path diretto molto pulito e una diagnostica operativa eccellente.

La strada giusta per bore non e copiare tutto frp. La strada giusta e prendere da frp il concetto di:

- classificare il NAT in modo operativo;
- convertire la classificazione in un piano di tentativi;
- distinguere candidati base e assisted candidates;
- usare il server come broker di politica, non solo di rendezvous.

## Cosa possiamo adottare in bore senza tradire il progetto

Le idee trasferibili sono queste:

1. Una struttura di profilo NAT, separata dalla semplice lista candidati.
2. Un piano di punching esplicito, con timing e priorita.
3. Assisted candidates come concetto formale.
4. Una decisione server-side che tenga conto di entrambi i peer.
5. Un retry policy leggero, ma NAT-aware.
6. Un sistema di fallback che resta invariato per il comportamento e2e.

Le cose da non fare subito sono queste:

- non introdurre un secondo trasporto solo per imitare frp;
- non cambiare il path relay funzionante;
- non spostare la complessita nel client senza un chiaro vantaggio operativo;
- non rendere il protocollo troppo verboso senza una reale necessita.

## Stato di avanzamento

Nel paired `bore test-udp --tcp-secret-id`, bore ha gia iniziato a applicare questo piano in modo conservativo:

- il server calcola e serializza un `UdpAdaptivePlan` per il pairing;
- il client legge il piano e riordina i candidati del punch diretto secondo quell'ordine;
- il tentativo diretto usa il retry budget e il timeout del piano, ma il relay resta il fallback invariato;
- i report paired mostrano il piano e i ruoli dei candidati, cosi l'operatore vede subito cosa e stato scelto.

Il resto del tunnel runtime resta intatto: questa e ancora una estensione osservabile e controllata del path diagnostico, non un cambio radicale del data plane.

## Architettura proposta per bore adaptive NAT

### Principi di base

1. Il relay resta sempre il fallback sicuro.
2. Il direct path resta opzionale.
3. La logica adattiva vive in un layer di policy, non nel data plane.
4. Il server conserva il ruolo di broker e di arbitro del piano.
5. I client eseguono il piano, ma non devono indovinarlo da soli.
6. La diagnostica resta separata dalla produzione, ma usa gli stessi concetti.

### Modello concettuale

Il sistema puo essere visto come tre livelli:

#### Livello 1 - Discovery

Raccoglie informazioni grezze:

- STUN reflexive address;
- local address;
- eventuale STUN selected hint del peer;
- eventuali candidati assistiti, UPnP, port prediction;
- segnali di NAT preservation e class.

#### Livello 2 - Profile e plan

Trasforma le informazioni in un piano:

- NAT class del peer A;
- NAT class del peer B;
- confidence del path diretto;
- ordering dei candidati;
- tempo di attesa per ciascun tentativo;
- eventuale uso di assisted addresses;
- policy di retry;
- soglie di fallback.

#### Livello 3 - Execution

Esegue il piano:

- invio candidati;
- tentativi simultanei o sequenziali;
- conferma del path diretto;
- degradazione al relay se necessario;
- eventuale retry/adattamento in base ai risultati.

### Modello dati proposto

Ecco una forma concettuale, non ancora codice da implementare:

```text
NatProfile
  mapping_class: Cone | Symmetric | Unknown
  filtering_class: FullCone | Restricted | PortRestricted | Unknown
  port_preserved: bool | unknown
  cgnat_suspected: bool
  hairpin_possible: bool | unknown
  selected_stun: string | none
  candidates: [reflexive, local, assisted, predicted, router_mapped]

NatPlan
  mode: direct_first | direct_with_retry | relay_first | relay_only
  role: sender | receiver
  ttl: duration
  send_delay: duration
  read_timeout: duration
  candidate_order: list
  assisted_policy: enabled | disabled
  retry_budget: integer
  fallback_policy: immediate | delayed | thresholded
```

Questo schema consente di tenere separati tre concetti che oggi sono un po fusi:

- cosa sappiamo del NAT;
- come vogliamo provarlo;
- cosa facciamo se il tentativo fallisce.

### Flusso server-side proposto

Il server dovrebbe diventare il punto di decisione del piano, come in frp, ma in modo leggero.

Flusso concettuale:

1. I due peer inviano i candidati e i segnali base.
2. Il server classifica ciascun peer.
3. Il server costruisce un `NatPlan` compatibile con entrambi.
4. Il server invia a ciascun lato sia i candidati sia i parametri di tentativo.
5. I client eseguono il piano.
6. Il server riceve il risultato del tentativo.
7. Se serve, il server aggiorna il piano per il retry successivo.

### Flusso client-side proposto

Il client deve restare semplice:

1. Raccoglie candidati.
2. Riceve il piano.
3. Esegue il tentativo secondo l'ordine e il timing ricevuto.
4. Apath direct OK -> apre la QUIC session.
5. Apath direct KO -> usa il relay senza interrompere il tunnel.

Il client non dovrebbe decidere da solo tutta la strategia, altrimenti si perde il vantaggio del broker server-side.

## Cosa riusiamo gia oggi in bore

Questa e la parte importante: bore ha gia diversi mattoni che possono diventare il motore adattivo, senza rifare tutto.

### 1. Candidate discovery

In [src/holepunch.rs](src/holepunch.rs) abbiamo gia:

- chain STUN con fallback;
- metadata del STUN selezionato;
- local address;
- UPnP-IGD candidate;
- port prediction;
- un risultato di discovery ricco (`CandidateDiscovery`).

Questo e gia molto vicino al concetto di assisted addresses, solo che oggi e usato come raccolta candidati e non come politica.

### 2. NAT classification

In [src/holepunch.rs](src/holepunch.rs) esiste gia `NatClass` con segnali utili per capire se il mapping e cone, symmetric o altro.

Questa classificazione e la base naturale per un future `NatProfile`.

### 3. Broker e nonce stabile

In [src/secret.rs](src/secret.rs) il server gia:

- registra provider e consumer separatamente;
- mantiene un nonce stabile per il provider;
- brokera i candidati del peer;
- invia `UdpPunch`;
- mantiene il relay come fallback sempre disponibile.

Questo e un ottimo punto di estensione per un piano adattivo.

### 4. Tuning e diagnostica

In [src/udp_diagnostic.rs](src/udp_diagnostic.rs) abbiamo gia:

- report host;
- report QUIC;
- throughput/ping;
- skew banda;
- diagnostica delle cause probabili;
- suggerimenti di tuning.

Il sistema puo usare gli stessi segnali per decidere la strategia di punch.

## Cosa manca davvero per un NAT adattivo completo

La differenza tra quello che c'e e quello che serve si puo riassumere cosi:

1. Manca una struttura di `NatProfile` condivisa dal protocollo.
2. Manca un `NatPlan` esplicito.
3. Manca la distinzione formale tra candidate base e assisted candidates.
4. Manca una policy server-side che scelga timer e ordine di tentativi.
5. Manca una strategia di retry NAT-aware.
6. Manca un set di test che verifichi il piano senza cambiare il comportamento e2e attuale.

## Piano di implementazione incrementale

Questa e la parte centrale del documento.

Le fasi sono cumulative. Ogni fase si costruisce sulla precedente e si chiude con test. La sequenza va letta come:

- A -> TEST -> A+B -> TEST -> A+B+C -> TEST -> ...

La regola e che ogni fase deve essere compatibile con lo stato precedente e non deve rompere i test e2e esistenti.

### Fase A - Formalizzare i dati NAT senza cambiare il comportamento

Obiettivo:

- introdurre un profilo NAT concettuale e un piano concettuale, ma senza alterare il flusso runtime;
- rendere esplicito quale informazione gia possediamo.

Cosa cambierebbe:

- nessun ordine di tentativo nuovo;
- nessun timing nuovo;
- nessun comportamento e2e nuovo.

Cosa si farebbe:

- consolidare i campi gia esistenti in una forma piu ordinata;
- definire i concetti di mapping, filtering, assisted, predicted, router-mapped;
- descrivere chiaramente il confine tra diagnostica e policy.

Test da aggiungere:

- test di serializzazione/deserializzazione dei messaggi gia esistenti;
- test di default compatibili per i nuovi campi, con default che non cambiano il wire;
- test che confermino che l'attuale diagnostica paired produce gli stessi risultati.

Documentazione da aggiornare:

- [TEST_UDP.md](TEST_UDP.md)
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md)

Exit criteria:

- nessun comportamento runtime nuovo;
- nessun e2e rotto;
- struttura concettuale pronta per la fase successiva.

### Fase B - Rendere first-class gli assisted candidates

Obiettivo:

- separare candidati base e candidati assistiti come categorie formali;
- far vedere al server quali candidate sono piu affidabili e perche.

Cosa cambierebbe:

- il peer continuerebbe a funzionare come oggi;
- il server riceverebbe metadata piu ricchi;
- il direct path resterebbe invariato se il plan non usa i nuovi campi.

Cosa si farebbe:

- distinguere almeno queste classi:
  - reflexive candidate;
  - local candidate;
  - UPnP/router-mapped candidate;
  - port-predicted candidate;
  - peer-hinted candidate;
- far entrare il selected STUN nel piano operativo, non solo nel log.

Test da aggiungere:

- unit test per l'ordine e la deduplica delle candidate;
- test per l'aggregazione di candidati assistiti;
- test per la compatibilita del wire format con i campi assenti.

Documentazione da aggiornare:

- [README.md](README.md)
- [TEST_UDP.md](TEST_UDP.md)
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md)

Exit criteria:

- i candidati assistiti sono formalizzati ma non cambiano ancora la semantica del punch;
- tutti i test e2e restano verdi.

### Fase C - Introdurre un NatPlan server-side, ma con comportamento conservativo

Obiettivo:

- far calcolare al server un piano NAT, ma con default conservativi;
- usare il piano solo come metadata osservabile.

Cosa cambierebbe:

- il server emetterebbe un piano, ma il client potrebbe ancora ignorarlo senza rompere nulla;
- il piano sarebbe letto, loggato e validato, ma non obbligatorio per il funzionamento base.

Cosa si farebbe:

- usare la classificazione NAT dei due peer per assegnare un `mode` conservativo;
- definire un `ttl` e un `read_timeout` sensati;
- produrre un `send_delay` iniziale che non peggiori il comportamento attuale.

Test da aggiungere:

- test unitari per la costruzione del piano da due profili NAT;
- test che controllino che il piano di default equivale al comportamento attuale;
- test che verifichino i fallback quando uno dei peer non invia i dati necessari.

Documentazione da aggiornare:

- [src/udp_diagnostic.rs](src/udp_diagnostic.rs) come riferimento interno della diagnostica;
- [TEST_UDP.md](TEST_UDP.md);
- [ADAPTIVE_NAT.md](ADAPTIVE_NAT.md) stesso documento, se la fase viene raffinata.

Exit criteria:

- il piano esiste, ma e ancora conservative-preserving;
- il direct path non cambia nei casi oggi supportati.

### Fase D - Usare il piano per ordinare i tentativi

Obiettivo:

- rendere il piano realmente operativo;
- cambiare l'ordine e il timing dei tentativi in base alla NAT class, ma senza cambiare il fallback finale.

Cosa si farebbe:

- ordinare i candidati in base al profilo:
  - reflexive primo quando ha senso;
  - local primo per same-LAN;
  - assisted prima o dopo a seconda della classe NAT;
  - predicted solo quando la NAT indica una probabilita ragionevole di port sequence;
- introdurre time budget piu piccoli su casi ovviamente negativi;
- fare retry con una politica piu aggressiva solo per i casi plausibili.

Test da aggiungere:

- test deterministici sull'ordine dei candidati per classe NAT;
- test di timeout e retry con clock/transport finti;
- test di regressione sul fallback relay quando il piano fallisce.

Documentazione da aggiornare:

- [README.md](README.md)
- [TEST_UDP.md](TEST_UDP.md)
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md)

Exit criteria:

- il piano altera il tentativo, ma non la affidabilita finale;
- il relay continua a coprire ogni failure.

### Fase E - Aggiungere retry adattivo e state machine leggera

Obiettivo:

- passare da un singolo piano a una piccola state machine;
- far convergere il path diretto in modo piu rapido sui casi buoni e piu prudente sui casi cattivi.

Cosa si farebbe:

- distinguere tentativi iniziali, retry, e fallback;
- aggiungere budgets di retry basati su esito precedente;
- aggiungere memorizzazione temporanea di segnali come:
  - selected STUN che ha funzionato;
  - candidate che ha fallito;
  - classe NAT osservata;
  - eventuale STUN hint del peer.

Test da aggiungere:

- test di transizione di stato;
- test di retry con esito misto;
- test di convergenza al relay quando il direct path e improbabile.

Documentazione da aggiornare:

- [TEST_UDP.md](TEST_UDP.md);
- [SERVER_UDP_OPTIMIZATION.md](SERVER_UDP_OPTIMIZATION.md) se la fase impatta i parametri che il server espone;
- [README.md](README.md).

Exit criteria:

- il retry non peggiora i casi buoni;
- i casi cattivi arrivano al relay prima di consumare troppo tempo.

### Fase F - Hardening, osservabilita e UX operativa

Obiettivo:

- chiudere il motore NAT con una UX comprensibile per l'operatore;
- aggiungere i log giusti e i test finali;
- rendere il sistema leggibile e mantenibile.

Cosa si farebbe:

- aggiungere report sintetici del piano NAT:
  - mapping class;
  - filtering class (se inferibile);
  - assisted policy usata;
  - reason del fallback;
- aggiungere campi di diagnostica nel paired test;
- rendere la documentazione operativa il punto di verita per gli operatori.

Test da aggiungere:

- e2e paired test con scenari cone/cone, cone/symmetric, symmetric/cone e fallback relay;
- test della diagnostica che verifica i messaggi di plan e reason;
- test di compatibilita con le opzioni esistenti per evitare regressioni.

Documentazione da aggiornare:

- [TEST_UDP.md](TEST_UDP.md)
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md)
- [README.md](README.md)
- [SERVER_UDP_OPTIMIZATION.md](SERVER_UDP_OPTIMIZATION.md)

Exit criteria:

- il motore adattivo e completo;
- il comportamento e2e precedente continua a funzionare;
- ogni nuova semantica e coperta da test e documentazione.

## Piano di test per ogni fase

La regola pratica e sempre la stessa:

1. Prima il test piu piccolo e piu locale possibile.
2. Poi il test di integrazione del tratto toccato.
3. Poi il test e2e che garantisce nessuna regressione.

### Test base da non saltare mai

- `cargo test --all-features`
- `cargo fmt -- --check`
- `cargo clippy --all-features -- -D warnings`
- `cargo test --all-features --test udp_test paired_test_udp_diagnostic_exercises_direct_and_relay`
- `cargo test --no-default-features`

### Test nuovi per l'adaptive NAT

In aggiunta ai test base, ogni fase dovrebbe introdurre test dedicati:

- test di serializzazione dei messaggi NAT plan/profile;
- test di classificazione NAT su casi fissati;
- test di selezione candidate e assisted policy;
- test di retry e time budget;
- test di fallback relay non regressivo;
- test paired con scenari reali o simulati.

## Matrice di compatibilita e non regressione

Il documento deve essere letto con questo vincolo sempre attivo:

| Area | Permesso | Non permesso |
|---|---|---|
| Relay TCP | mantenere il comportamento attuale | ridurre affidabilita o disponibilita |
| Direct UDP | aggiungere intelligenza | rompere i casi oggi funzionanti |
| Messaggi di control plane | aggiungere campi compatibili | cambiare i default in modo distruttivo |
| Diagnostica | aggiungere segnali | trasformare la diagnostica in comportamento obbligatorio |
| Docs | espandere e precisare | lasciare il comportamento non documentato |

## Piccoli snippet concettuali

Questi snippet non sono codice da incollare. Servono per fissare il modello mentale.

### Snippet 1 - stato attuale semplificato

```text
peer -> STUN -> candidates -> server broker -> punch -> QUIC direct
           \-> if failed -> relay
```

### Snippet 2 - stato desiderato

```text
NatProfile(A) + NatProfile(B) -> NatPlan
NatPlan -> candidate order + timing + assisted policy
NatPlan -> execution
execution -> direct OK or relay fallback
```

### Snippet 3 - separazione tra dati e policy

```text
Discovery data: what we know about the network
Policy plan: how we will try
Execution result: what happened
```

### Snippet 4 - principio di compatibilita

```text
new fields default to old behavior
new policy is opt-in by capability or conservative default
relay remains available at every failure point
```

## Rischi principali

### 1. Eccesso di complessita

Il rischio piu grosso e aggiungere una macchina di policy troppo complicata per un progetto che oggi e volutamente semplice.

Mitigazione:

- tenere la policy piccola;
- separare discovery, plan ed execution;
- lasciare il relay come rete di sicurezza.

### 2. Regresso e2e

Il rischio e che il nuovo motore alteri il path oggi funzionante.

Mitigazione:

- default conservativi;
- test di regressione prima di ogni fase;
- no change al relay.

### 3. Messaggi troppo verbosi o fragili

Se il protocollo diventa troppo ricco, si rischia di superare il limite pratico dei frame o di rendere il debug piu difficile.

Mitigazione:

- mantenere i messaggi compatti;
- usare solo i campi che servono davvero;
- documentare ogni nuovo campo.

### 4. Policy troppo smart e poco osservabile

Un motore NAT senza spiegazioni chiare diventa difficile da operare.

Mitigazione:

- log di decisione;
- reason sintetiche;
- report paired leggibile;
- docs sempre aggiornate.

## Decisione consigliata

Si, bore puo implementare un motore di NAT traversal adattivo ispirato a frp, ma deve farlo in modo diverso:

- frp usa una policy PIU completa e PIU invasiva;
- bore deve usare una policy PIU piccola, PIU leggibile e PIU compatibile con il suo design.

La strategia giusta e questa:

1. Formalizzare cio che gia sappiamo sul NAT.
2. Trasformarlo in un piano di tentativi.
3. Lasciare il relay come fallback totale.
4. Introdurre i nuovi comportamenti solo con test e documentazione.

## Relazioni con i documenti esistenti

Questo file deve essere letto insieme a:

- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md) per la teoria NAT e la matrice di compatibilita.
- [SERVER_UDP_OPTIMIZATION.md](SERVER_UDP_OPTIMIZATION.md) per tuning, buffer e capacità server.
- [TEST_UDP.md](TEST_UDP.md) per il comportamento della diagnostica e i casi operativi.
- [README.md](README.md) per la user experience attuale e il contratto pubblico del progetto.

## Conclusione finale

Il confronto con frp ci dice che il pezzo mancante non e la hole punching in se, ma la trasformazione della hole punching in una decisione guidata dal NAT.

bore ha gia:

- discovery;
- candidati;
- hint STUN;
- relay affidabile;
- direct path QUIC;
- diagnostica forte.

Quello che manca e un layer di policy adattiva che usi queste informazioni per decidere in modo piu intelligente come provare il punch.

La strada migliore e una evoluzione a piccoli passi, dove ogni fase aggiunge valore senza alterare il comportamento esistente.