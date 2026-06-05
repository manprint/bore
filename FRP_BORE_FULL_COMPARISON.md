# FRP vs bore: confronto completo

Documento di analisi architetturale e operativa, pensato come base per un piano implementativo.

Scopo: confrontare FRP e bore su due superfici distinte ma collegate:

1. `test-udp` come strumento di diagnosi e di pairing coordinato.
2. Il tunnel di produzione, cioè il percorso runtime usato da `local`, `proxy` e `server`.

Il documento distingue volutamente i due piani perché in bore oggi convivono due filosofie diverse:

- `test-udp` è già una superficie ricca di segnali, classificazione NAT e pianificazione adattiva.
- il tunnel di produzione resta più semplice, più rigido e più conservativo, con fallback forte al relay.

Riferimenti utili nel repo:

- [src/holepunch.rs](src/holepunch.rs)
- [src/udp_diagnostic.rs](src/udp_diagnostic.rs)
- [src/secret.rs](src/secret.rs)
- [src/client.rs](src/client.rs)
- [src/shared.rs](src/shared.rs)
- [src/main.rs](src/main.rs)
- [ADAPTIVE_NAT.md](ADAPTIVE_NAT.md)
- [TEST_UDP.md](TEST_UDP.md)
- [NAT_TRAVERSAL.md](NAT_TRAVERSAL.md)
- [SERVER_UDP_OPTIMIZATION.md](SERVER_UDP_OPTIMIZATION.md)
- [FIREWALL_LIMITATION.md](FIREWALL_LIMITATION.md)

---

## Sintesi esecutiva

Se si guarda FRP come riferimento, la risposta breve è questa:

- **test-udp**: bore è già vicino a FRP sul piano diagnostico e di policy leggera, soprattutto nella modalità paired `--tcp-secret-id`. Qui il server coordina il pairing, i peer scambiano candidati, si produce un `adaptive_plan`, si legge lo STUN selezionato e si decide un ordine di tentativi. Manca ancora una generalizzazione completa in stile FRP, ma la direzione è corretta.
- **tunnel produzione**: bore è volutamente più semplice di FRP. Ha un relay TCP robusto, un direct UDP opzionale per i tunnel segreti, native QUIC streams sul path diretto e una disciplina di fallback molto forte. Manca la flessibilità di FRP su trasporti e behavior di punching, ma la semplicità è una scelta architetturale reale, non un incidente.

La differenza principale è questa:

- FRP tende a essere un framework di traversal/policy molto ricco.
- bore tende a essere un tunnel minimalista che sta aggiungendo intelligenza solo dove serve davvero.

---

## Sezione test-udp

### 1. Cosa fa FRP in questa area

Nel mondo FRP la parte più vicina al nostro `test-udp` è la famiglia `xtcp` / P2P / NAT hole punching.
Qui il server non si limita a fare rendezvous: produce un comportamento di punching.

I concetti chiave di FRP sono:

- `NatHoleVisitor` e `NatHoleResp` non portano solo indirizzi, ma anche il comportamento di traversal.
- `NatHoleDetectBehavior` contiene campi espliciti come `Role`, `Mode`, `TTL`, `SendDelayMs` e `ReadTimeoutMs`.
- Il controller classifica la NAT partendo da `MappedAddrs` e `AssistedAddrs`.
- Il visitor può usare `kcp` o `quic` nella modalità `xtcp`.
- Il flusso è pensato come una sequenza di detect, punch, retry e fallback.

In altre parole, FRP usa `test-udp`-like logic come una vera policy di attraversamento NAT, non solo come diagnostica.

### 2. Cosa fa bore oggi

In bore la superficie `test-udp` è divisa in due casi:

- **standalone**: `bore test-udp` diagnostica la rete locale, classifica il NAT, misura STUN, segnala hairpin, CGNAT, preservazione della porta e raggiungibilità UDP del server.
- **paired**: `bore test-udp --tcp-secret-id <id>` abbina due peer, scambia candidati, calcola un `adaptive_plan`, prova il direct UDP/QUIC e mantiene il relay TCP come fallback.

Questa distinzione è importante:

- nel caso standalone bore è uno strumento di diagnosi;
- nel caso paired bore è già un piccolo motore di traversal coordinato.

Nel codice questo si vede bene in [src/main.rs](src/main.rs) e in [src/udp_diagnostic.rs](src/udp_diagnostic.rs):

- `test-udp` senza `--tcp-secret-id` va in `holepunch::diagnose`;
- con `--tcp-secret-id` va in `udp_diagnostic::run_peer_test`;
- il paired flow calcola `NatProfile`/`NatPlan` in [src/adaptive_nat.rs](src/adaptive_nat.rs) e passa un `UdpAdaptivePlan` nel control plane.

### 3. Differenze chiave FRP vs bore nel test-udp

#### 3.1 Ruolo del server

FRP:

- il server è parte della policy di NAT hole punching;
- decide il comportamento di detect;
- usa la classificazione NAT per influenzare il tentativo successivo.

bore:

- nel caso standalone il server non c'entra: il peer diagnostica da solo;
- nel caso paired il server coordina il pairing e sintetizza un adaptive plan;
- nella produzione il server brokera e basta, senza diventare un policy engine generale.

Questa è la differenza più importante.

#### 3.2 Struttura dei messaggi

FRP:

- porta in wire protocol indirizzi mapped/assisted e `DetectBehavior`;
- il protocollo è più vicino a una piccola macchina di stato NAT.

bore:

- usa messaggi più compatti e più specializzati;
- `UdpCandidateOffer`, `UdpPunch`, `UdpStunHint`, `TestUdpJoin`, `TestUdpStart`;
- nel paired `test-udp` aggiunge `adaptive_plan`, `candidate_kinds` e `selected_stun`;
- il framing è tracciato con label e summary redatti in [src/shared.rs](src/shared.rs).

Questo rende bore più leggibile e più sicuro da operare, ma meno generico di FRP.

#### 3.3 Modellazione della NAT

FRP:

- estrae la NAT feature in modo esplicito e la usa per scegliere il comportamento.

bore:

- classifica la NAT in [src/holepunch.rs](src/holepunch.rs#L732) con `classify_nat`;
- nel paired diagnostic costruisce un profilo locale/peer e decide un piano adattivo;
- in standalone la classificazione serve soprattutto a spiegare l’operatore, non a pilotare una policy globale.

#### 3.4 Trasporto

FRP:

- può usare `kcp` o `quic` per `xtcp`;
- il trasporto è un grado di libertà della policy.

bore:

- il direct path usa QUIC nativo e basta;
- il relay usa TCP/yamux;
- non c’è scelta di trasporto al volo.

#### 3.5 Fallback

FRP:

- il fallback è parte della politica del visitor e del controller;
- può essere configurato come comportamento di percorso.

bore:

- il relay TCP resta sempre disponibile;
- il fallback non viene “negoziato” come un trasporto alternativo, ma rimane un pilastro architetturale del servizio.

### 4. Cosa manca a bore rispetto a FRP nel test-udp

Qui conviene distinguere tra **mancanze vere** e **mancanze solo rispetto alla ricchezza di FRP**.

#### Mancanze vere

- Non c’è una policy universale stile `DetectBehavior` riutilizzata ovunque.
- La logica adaptiva è ancora più forte nella diagnostica paired che nella produzione.
- Non esiste una scelta di trasporto alternativo per il direct path.
- La semantica dei candidati assistiti è ancora meno formalizzata di FRP.

#### Mancanze relative alla filosofia FRP

- FRP ha un controller NAT hole più esplicito e più verboso.
- FRP espone più knob per modalità, timeout e fallback.
- FRP tratta il P2P come una feature di prima classe del framework, mentre bore lo tratta come estensione mirata del tunnel.

### 5. Punti di forza di bore nel test-udp

#### 5.1 Osservabilità più alta

Bore oggi è molto forte sul piano della diagnosi leggibile:

- traccia `tx`/`rx` del control plane con [src/shared.rs](src/shared.rs#L720);
- summary redatti e label di canale;
- `selected_stun`, `candidate_kinds`, `port_preserved`, `stun_aligned`, `adaptive_plan`.

Questo è un vantaggio pratico rispetto a un sistema più generico ma meno leggibile.

#### 5.2 Diagnostica più onesta

`test-udp` separa in modo chiaro:

- cosa sa la macchina locale;
- cosa sa il peer;
- cosa riesce il direct path;
- cosa resta al relay.

In FRP la policy è più integrata; in bore è più trasparente.

#### 5.3 Minor rischio di regressione

Il path diagnostico usa gli stessi pezzi del path reale, ma senza dover forzare una feature da framework.
Questo aiuta a tenere il comportamento stabile.

#### 5.4 Port-release detection già utile operativamente

La detection della porta preferita è un vantaggio reale per `test-udp` e per i casi in cui la NAT preserva la porta solo se non la tocchi.
È una soluzione molto concreta che FRP, nel confronto architetturale, non espone con la stessa evidenza operativa nel nostro contesto.

### 6. Debolezze di bore nel test-udp

#### 6.1 Policy ancora poco generalizzata

Il path paired ha già un `adaptive_plan`, ma la policy non è ancora una primitive unica che vive al centro del prodotto.

#### 6.2 Nessun trasporto alternativo

Se QUIC non parte, bore non prova KCP o altre famiglie di trasporto. Il fallback è il relay.

#### 6.3 Più distribuzione della logica

Parte della logica vive in:

- [src/holepunch.rs](src/holepunch.rs)
- [src/udp_diagnostic.rs](src/udp_diagnostic.rs)
- [src/client.rs](src/client.rs)
- [src/secret.rs](src/secret.rs)

Questa distribuzione è sana finché resta piccola; diventa un problema se la policy cresce senza una home chiara.

#### 6.4 Standalone test-udp non è policy-driven

La modalità senza `--tcp-secret-id` resta una diagnosi locale, non una negoziazione tra due peer.
Questa è una scelta corretta, ma rispetto a FRP significa meno “intelligenza di traversal” in quella modalità.

### 7. TODO da fare per test-udp

Ordine consigliato, dal più importante al meno urgente:

1. **Unificare la policy adattiva in un modulo condiviso e documentato**.
   - Oggi [src/adaptive_nat.rs](src/adaptive_nat.rs) contiene la semantica più vicina a un policy engine.
   - Va tenuto come fonte unica di verità per il paired `test-udp`.

2. **Formalizzare meglio i ruoli dei candidati**.
   - `reflexive`, `local`, `router-mapped`, `predicted`, `relay` sono già concetti presenti.
   - Il passo successivo è rendere più chiaro quando e perché un candidato cambia priorità.

3. **Separare sempre i motivi del fallback**.
   - Distinguere meglio `STUN failed`, `peer blocked`, `candidate exhausted`, `relay forced`, `adaptive plan says relay-first`.

4. **Aggiungere test di regressione più mirati**.
   - NAT cone/cone.
   - Cone/symmetric.
   - Symmetric sequential vs random.
   - CGNAT/doppio NAT.
   - Port preservation yes/no.
   - Port-release detection e re-offer.

5. **Tenere `test-udp` distinguibile dal tunnel di produzione**.
   - Il test deve restare un laboratorio, non un’altra copia del runtime.

6. **Valutare solo dopo se introdurre una semantica stile `DetectBehavior` più esplicita**.
   - Se serve davvero al runtime, va usata anche fuori da `test-udp`.
   - Se serve solo al test, va tenuta confinata lì.

---

## Sezione tunnel produzione

Per “tunnel produzione” considero tutto il runtime bore fuori da `test-udp`:

- public tunnel (`local` + server public port);
- secret tunnel relay (`local --tcp-secret-id` + `proxy`);
- secret tunnel direct UDP (`--udp`);
- edge HTTPS / force-HTTPS / basic-auth;
- carrier pools sul relay;
- admin/status e logging operativo.

### 1. Cosa fa FRP in produzione

FRP in produzione è più largo come superficie funzionale:

- supporta più trasporti (`tcp`, `kcp`, `quic`, `websocket`, `wss`);
- ha visitor/xtcp con policy di punching e fallback configurabili;
- porta un set di knob per retry, liveness e comportamento del tunnel;
- usa `NatHoleDetectBehavior` come parte della negoziazione P2P.

In pratica FRP è più vicino a un framework per costruire topologie di tunnel, non solo a un reverse tunnel minimalista.

### 2. Cosa fa bore in produzione oggi

Bore è volutamente più piccolo e più opinionated.

#### 2.1 Public tunnel

Il public tunnel è il caso classico:

- il server accetta il traffico in ingresso;
- il client riceve i data substream;
- il traffico resta nel percorso relay TCP/yamux;
- il server può aggiungere `--carriers` per distribuire il carico su più connessioni di carrier.

Questo è semplice da capire e facile da operare.

#### 2.2 Secret tunnel relay

Nel tunnel segreto:

- il provider si registra;
- il consumer si collega;
- il server fa da broker;
- se il direct path non è disponibile, il relay rimane il percorso normale.

#### 2.3 Secret tunnel direct UDP

Quando `--udp` è abilitato su server, provider e consumer:

- i peer fanno STUN;
- il server passa l’hint STUN del provider;
- i candidati vengono scambiati;
- il consumer prova il direct path con native QUIC streams;
- se il direct path fallisce, il tunnel continua via relay.

Questa parte è già molto forte ed è una delle differenze più interessanti rispetto a FRP: bore non ha solo “un altro trasporto”, ma un direct path con native QUIC streams e liveness separata dal relay.

### 3. Differenze chiave FRP vs bore nel tunnel produzione

#### 3.1 Ampiezza di feature

FRP:

- più trasporti;
- più modalità di tunnel;
- più knob per behavior di attraversamento;
- più configurazione per proxy/visitor.

bore:

- una struttura molto più compatta;
- relay TCP come base stabile;
- direct UDP soltanto per secret tunnel e solo se tutto il contesto lo supporta.

#### 3.2 Modello di controllo

FRP:

- il controller NAT hole è parte della storia di produzione.

bore:

- la produzione è più separata dalla diagnostica;
- la classificazione NAT e il piano adattivo sono forti in `test-udp`, ma non sono ancora il cuore del tunnel runtime;
- il runtime si appoggia su decisioni più semplici e più conservative.

#### 3.3 Trasporto

FRP:

- può scegliere KCP o QUIC per il visitor P2P;
- il trasporto è un parametro di progetto.

bore:

- relay TCP/yamux per il percorso normale;
- direct path QUIC nativo per il path UDP segreto;
- niente selezione dinamica del trasporto.

#### 3.4 Fallback

FRP:

- il fallback è parte del modello di visitor e controller.

bore:

- il fallback è architetturale: il relay c’è sempre;
- il direct path non sostituisce il relay, lo affianca.

#### 3.5 Concorrenza e scaling

FRP:

- ha pool, liveness e modalità di connessione più numerose.

bore:

- il path relay può usare `--carriers` su public tunnel, provider e consumer;
- il direct UDP bypassa il carrier pool e usa stream QUIC nativi;
- questo rende il scaling più prevedibile sul direct e più elastico sul relay.

### 4. Cosa manca a bore rispetto a FRP in produzione

#### 4.1 Nessuna selezione di trasporto generalizzata

FRP può provare o configurare KCP/QUIC e altri trasporti.
bore no.

Questo è il gap più evidente.

#### 4.2 Nessun policy engine di produzione equivalente a FRP

Il runtime bore non usa ancora una policy NAT centrale per decidere il comportamento di punching dei tunnel normali.

#### 4.3 Meno knob per proxy/visitor

FRP espone più leve per comportamento, retry, fallback e transport choice.
Bore ha leve più mirate:

- `--udp`
- `--carriers`
- `--nat-udp-preferred-port`
- `--nat-udp-release-timeout`
- `--max-conns`

#### 4.4 Public tunnel ancora totalmente relay-based

Il public tunnel non ha un equivalente P2P/direct.
Questo è coerente con l’architettura, ma è comunque una differenza forte rispetto a un framework più ampio come FRP.

#### 4.5 Nessuna espansione virtual network/TUN-style

FRP ha un orizzonte più largo su networking e modalità di esposizione.

bore resta nel perimetro del reverse tunnel e del direct UDP segreto.

### 5. Punti di forza di bore nel tunnel produzione

#### 5.1 Architettura più semplice

Questo è il vantaggio più grande.

- Un control connection principale.
- Un modello relay chiaro.
- Un direct path opzionale e ben delimitato.
- Meno combinazioni di trasporto da testare e mantenere.

#### 5.2 Fallback fortissimo

Se il direct path non parte, il tunnel continua a funzionare via relay.
Questo rende bore più robusto in ambienti difficili e più adatto a scenari operativi reali.

#### 5.3 Direct UDP moderno e pulito

Il direct path usa native QUIC streams, quindi:

- niente HOL dovuto a un singolo stream QUIC sovrapposto a tutto;
- una connessione per flusso applicativo, con isolamento migliore;
- il fallimento di un flusso non blocca gli altri nello stesso modo di un multiplexing monolitico.

#### 5.4 Carrier pools sul relay

`--carriers` è una risposta pratica ai colli di bottiglia del relay:

- public tunnel: server→client;
- secret provider: server→provider;
- secret consumer: consumer→server.

Questo riduce l’effetto di una singola connessione TCP “stretta” e aiuta la concorrenza.

#### 5.5 Port-release detection

Per i casi in cui la porta preferita viene rimappata, bore ha una soluzione concreta e operativa.
È una caratteristica molto forte per ambienti con firewall stretti o NAT port-preserving.

#### 5.6 Osservabilità del control plane

Il logging con label e summary rende il runtime molto più leggibile:

- server/control;
- client/public;
- client/provider;
- proxy/consumer;
- test-udp/peer.

Questo è un plus serio per il debug di produzione.

### 6. Debolezze di bore nel tunnel produzione

#### 6.1 Meno flessibilità di FRP

La rigidità è anche una debolezza:

- niente KCP;
- niente scelta di trasporto al volo;
- niente policy di punching generalizzata.

#### 6.2 Production tunnel meno “adaptive” del test-udp

Paradossalmente, la parte più intelligente di bore oggi è più visibile in `test-udp` che nel runtime del tunnel.

#### 6.3 Più difficoltà a coprire casi estremi senza aggiungere complessità

Se si vuole inseguire FRP su tutte le possibilità, il rischio è aumentare la complessità più velocemente della qualità.

#### 6.4 Potenziale drift tra diagnostica e runtime

Se il policy engine resta più forte nella diagnostica che nel tunnel reale, nel tempo si può creare un disallineamento tra ciò che bore sa spiegare e ciò che bore applica davvero.

### 7. TODO da fare per il tunnel produzione

Anche qui, in ordine consigliato:

1. **Decidere se la policy adattiva di `test-udp` deve essere riusata nel runtime**.
   - La risposta più probabile è: sì, ma in forma ridotta e con default conservativi.
   - Il candidato naturale è il tunnel segreto UDP diretto, non il public tunnel.

2. **Unificare i segnali NAT tra diagnostica e produzione**.
   - `selected_stun`.
   - `candidate_kinds`.
   - `port_preserved`.
   - `NatClass` / `NatProfile`.

3. **Definire se serve una vera `DetectBehavior` di produzione**.
   - Se il comportamento di punching diventa più complesso, serve una singola astrazione.
   - Se no, meglio mantenere il runtime piccolo.

4. **Migliorare i motivi di fallback nei log**.
   - Non basta sapere che è caduto al relay.
   - Serve capire se il motivo è NAT, STUN, porta, provider, consumer, o timeout.

5. **Mantenere il public tunnel fuori dalla complessità del direct path**.
   - Il public tunnel deve restare semplice, stabile e prevedibile.

6. **Non introdurre trasporti aggiuntivi per imitare FRP**.
   - KCP o altri trasporti vanno introdotti solo se c’è un caso operativo reale che giustifica il costo.

7. **Rafforzare i test di regressione del direct path e del relay**.
   - Provider drop.
   - Consumer reconnect.
   - Port release.
   - CGNAT e symmetric NAT.
   - Concorrenza multi-consumer.

---

## Raccomandazione architetturale

Se l’obiettivo è avvicinarsi a FRP senza perdere l’identità di bore, la strada consigliata è questa:

1. **Tenere la diagnostica come laboratorio di policy**.
   - `test-udp` è il posto giusto per sperimentare classificazione, piano e motivi.

2. **Promuovere solo i concetti che servono davvero al runtime**.
   - `NatProfile`.
   - `NatPlan`.
   - `selected_stun`.
   - `port_preserved`.

3. **Non copiare la complessità di FRP se non crea valore operativo**.
   - Il punto di bore non è vincere in feature count.
   - È essere più semplice, più solido e più facile da operare.

4. **Lasciare il relay come garanzia di servizio**.
   - Questo è il tratto più importante da non sacrificare mai.

5. **Usare il direct UDP come acceleratore, non come dipendenza**.
   - Direct path dove funziona.
   - Relay quando serve.

---

## Tabelle riepilogative

### Tabella 1 - `test-udp`: FRP vs bore

| Asse | FRP | bore oggi | Gap principale | TODO consigliato |
|---|---|---|---|---|
| Ruolo del server | Controller NAT hole che decide il comportamento di punch | Coordinatore del pairing; nel paired `test-udp` sintetizza un `adaptive_plan` | Non c’è ancora una policy universale di produzione | Consolidare la policy adattiva in un modulo condiviso |
| Messaggi | `NatHoleVisitor`, `NatHoleResp`, `NatHoleDetectBehavior`, `MappedAddrs`, `AssistedAddrs` | `UdpCandidateOffer`, `UdpPunch`, `UdpStunHint`, `TestUdpJoin`, `TestUdpStart` | Meno semantica di traversal nel wire generale | Formalizzare meglio candidate e motivi di fallback |
| Classificazione NAT | Parte del controller e del detect behavior | `classify_nat` + `NatProfile` + `NatPlan` nel paired diagnostic | La classificazione non è ancora la policy unica del prodotto | Riunire classificazione e piano in una singola sorgente di verità |
| Trasporto | `kcp` o `quic` per `xtcp` | Direct path QUIC nativo, relay TCP/yamux | Nessuna alternativa di trasporto | Non introdurre altri trasporti senza caso d’uso reale |
| Fallback | Configurabile nel visitor/controller | Relay sempre presente | Bore è meno configurabile ma più semplice | Mantenere relay come fallback assoluto |
| Osservabilità | Buona ma più orientata alla macchina di policy | Molto forte: label, trace `tx/rx`, summary redatti | Il runtime non sfrutta ancora tutta la ricchezza del test | Portare i motivi di decisione anche nel runtime |
| Stato complessivo | Framework P2P più ricco e policy-heavy | Laboratorio diagnostico molto buono, già vicino a FRP nel paired mode | Manca la generalizzazione al runtime | Reusare solo i concetti che servono davvero |

### Tabella 2 - `test-udp`: backlog operativo

| Priorità | Attività | Impatto atteso | Rischio se non fatta |
|---|---|---|---|
| Alta | Consolidare `NatProfile` / `NatPlan` come semantica unica della diagnostica paired | Meno drift tra logica e report | Policy frammentata e difficile da mantenere |
| Alta | Rendere più espliciti i motivi di fallback | Debug più veloce | Difficile capire perché il direct path non parte |
| Alta | Coprire con test i casi cone/symmetric/CGNAT/port-preserving | Più confidenza sui casi reali | Regresioni silenziose sul traversal |
| Media | Raffinare la tassonomia dei candidati | Ordering più chiaro | Candidati corretti ma poco spiegati |
| Media | Tenere il standalone diagnostic separato dalla policy peer-to-peer | Chiarezza architetturale | Mischiare laboratorio e runtime |
| Bassa | Aggiungere ulteriori knob di tipo FRP | Più flessibilità | Crescita inutile della superficie di config |

### Tabella 3 - tunnel produzione: FRP vs bore

| Asse | FRP | bore oggi | Gap principale | TODO consigliato |
|---|---|---|---|---|
| Superficie funzionale | Molto ampia: proxy, visitor, xtcp, più trasporti | Più piccola: public tunnel, secret tunnel relay, secret UDP direct | Meno feature e meno combinazioni | Mantenere la semplicità, aggiungere solo ciò che serve davvero |
| Trasporti | `tcp`, `kcp`, `quic`, `websocket`, `wss` | Relay TCP/yamux + direct QUIC nativo | Nessuna scelta di trasporto al volo | Non introdurre nuovi trasporti senza driver operativo |
| Policy di punch | `NatHoleDetectBehavior` in produzione | Policy forte soprattutto in `test-udp`, meno nel runtime | Policy non ancora centrale nel runtime | Valutare una forma ridotta di policy condivisa |
| Fallback | Parte della configurazione del visitor | Relay sempre garantito | Bore è più rigido ma più affidabile | Proteggere il relay come fallback assoluto |
| Scalabilità relay | Pool e comportamenti multipli | `--carriers` per public/provider/consumer relay legs | No carrier pool sul direct path | Tenere separate le due strategie |
| Operabilità | Ampia ma più complessa | Più semplice, più leggibile, log più chiari | Meno configurabilità | Portare i motivi decisionali nei log del runtime |
| Stato complessivo | Più framework, più scelta | Più minimalista, più controllato | Manca flessibilità di trasporto/policy | Riusare la policy solo dove migliora davvero il servizio |

### Tabella 4 - tunnel produzione: backlog operativo

| Priorità | Attività | Impatto atteso | Rischio se non fatta |
|---|---|---|---|
| Alta | Decidere se riusare il piano adattivo del diagnostic nel tunnel segreto UDP | Allineamento tra laboratorio e runtime | Diagnostica e produzione divergono |
| Alta | Unificare i segnali NAT tra `test-udp`, `secret` e `client` | Meno drift di implementazione | Ogni modulo “sa” cose diverse |
| Alta | Migliorare i log di fallback/direct choice nel runtime | Debug di produzione più veloce | Difficile capire perché si è andati su relay |
| Media | Valutare una `DetectBehavior` ridotta per il runtime | Policy più chiara | La logica resta sparsa |
| Media | Rafforzare test di provider drop, consumer reconnect e port-release | Più affidabilità | Regresioni su casi reali |
| Bassa | Aggiungere trasporti alternativi solo per parity con FRP | Più flessibilità | Complessità non giustificata |

---

## Conclusione finale

Rispetto a FRP, bore è messo così:

- **su `test-udp`**: abbastanza vicino sul piano della policy adattiva, ma con un taglio più piccolo, più leggibile e più focalizzato sulla diagnostica reale;
- **sul tunnel di produzione**: più semplice, più robusto nel fallback, ma meno flessibile e meno “framework-like” di FRP.

Se la domanda è “dove siamo messi?”, la risposta più onesta è:

- bore ha già preso le idee giuste da FRP nella parte di diagnosi e adaptive NAT;
- nel runtime di produzione ha scelto consapevolmente di rimanere più minimalista;
- il passo successivo sensato non è copiare FRP, ma promuovere solo i concetti che portano valore reale al tunnel segreto senza rompere la semplicità del progetto.
