# Piano di miglioramento della connessione UDP diretta

Stato: proposta tecnica, 2026-07-16
Modello usato: GPT-5 Codex
Subtask delegati: nessuno

## Decisione sintetica

Si, l'implementazione puo aumentare sia la probabilita di connessione UDP
diretta sia la velocita con cui la trova. Il data plane QUIC e il fallback relay
sono gia solidi; il margine maggiore e nel traversal che precede QUIC.

La priorita consigliata e:

1. rendere i candidati tipizzati, limitati e osservabili;
2. sostituire il punch cieco con connectivity check autenticati e apprendimento
   di candidati peer-reflexive;
3. promuovere la policy adattiva da `test-udp` al live path con una checklist
   ICE-like, mantenendo il relay caldo;
4. aggiungere IPv6 dual stack e candidati per piu interfacce;
5. gestire davvero il ciclo di vita delle mappature UPnP e aggiungere PCP;
6. usare RFC 5780 e un laboratorio NAT deterministico per affinare policy e
   diagnostica.

Non e consigliato sostituire subito tutto con una implementazione ICE completa.
Un sottoinsieme ICE-like, compatibile con il protocollo e con il socket gia usato
da Quinn, porta quasi tutto il beneficio utile a bore con un rischio inferiore.

## Ambito

Il piano riguarda i path peer-to-peer che richiedono NAT traversal:

- tunnel secret: provider `bore local --tcp-secret-id --udp` e consumer
  `bore proxy --udp`;
- VPN 1:1 e hub: upgrade dal relay al direct QUIC;
- diagnostica paired `bore test-udp --tcp-secret-id`.

`bore vhost --udp` e il public tunnel `bore local --udp` non fanno hole
punching: il client chiama un server pubblico. Condividono pero
`holepunch.rs`, socket, tuning e QUIC, quindi ogni modifica al codice comune deve
dimostrare di non regredirli.

Fuori ambito:

- cambiare il data plane QUIC dopo che la connessione e stabilita;
- rimuovere o ritardare il relay TCP gia disponibile;
- usare TURN come se fosse una connessione diretta;
- allargare indiscriminatamente il port scanning;
- cambiare gli invarianti VPN su carrier, nonce, PMTU o fallback caldo.

## Implementazione attuale

### Discovery e candidati

[src/holepunch.rs](src/holepunch.rs) oggi:

- crea un solo socket wildcard IPv4 con `bind_socket`;
- prova una catena STUN: peer hint, Cloudflare, Google e server bore;
- si ferma al primo STUN che risponde nel live path;
- produce un candidato reflexive, fino a quattro porte successive se la port
  prediction e abilitata, un candidato UPnP e un solo indirizzo locale;
- restituisce `CandidateDiscovery` con tipi locali, ma sul wire
  `UdpCandidateOffer` invia solo `Vec<SocketAddr>`, `selected_stun` e `peer_id`.

Il client STUN accetta XOR-MAPPED-ADDRESS IPv4/IPv6, ma il socket, la risoluzione
live e il responder bore sono di fatto IPv4. Il responder implementa solo il
Binding minimo e non RFC 5780.

### Broker e tentativo diretto

[src/secret.rs](src/secret.rs), [src/client.rs](src/client.rs),
[src/vpn.rs](src/vpn.rs) e [src/vpn_server.rs](src/vpn_server.rs):

- scambiano i candidati sul control channel del tunnel, protetto
  dall'autenticazione applicativa quando viene configurato `--secret`;
- usano il nonce di sessione e il token gia previsto dal protocollo per
  autenticare il path UDP;
- inviano cinque volte `bore-punch` a tutti i candidati, ogni 50 ms;
- il consumer/connector avvia in parallelo un handshake QUIC verso tutti i
  candidati sotto un budget totale di 3 secondi;
- il provider/listener accetta la prima connessione con token valido;
- su ogni errore resta disponibile il relay;
- secret e VPN ritentano l'upgrade mentre sono sul relay;
- il listener secret persistente usa QUIC Initial incompleti per riaprire il
  filtro NAT verso consumer successivi.

### Policy adattiva

[src/adaptive_nat.rs](src/adaptive_nat.rs) contiene `NatProfile`, `NatPlan`,
ordine dei tipi, timeout e retry budget. Oggi e collegato solo alla diagnostica
paired in [src/udp_diagnostic.rs](src/udp_diagnostic.rs), non al live path secret
o VPN.

Anche nella diagnostica l'ordine calcolato ha effetto limitato: `connect_direct`
lancia tutti i candidati contemporaneamente. Inoltre un retry crea un nuovo
socket ma riusa i candidati scambiati per il socket precedente. Il piano
adattivo e quindi una buona base di modello, non ancora una policy di produzione
validata.

### Test esistenti

La copertura attuale verifica bene:

- round trip, stream concorrenti, reconnect, fallback e upgrade secret;
- brokering e re-arm dei round VPN;
- collisione della porta UDP preferita senza `SO_REUSEADDR`;
- timeout globale dei candidati;
- direct path in netns con routing diretto;
- stress con VPN e secret concorrenti;
- regressioni vhost/public UDP e tuning QUIC.

Manca un gate deterministico che emuli EIM, ADM/APDM, EIF/ADF/APDF, hairpin,
doppio NAT, cambio mapping e lease di un port mapper. Il test netns secret
attuale instrada i due peer direttamente e non esercita due NAT reali.

## Problemi trovati

### P0 - Il punch non apprende nulla

`punch()` invia una stringa fissa e non legge risposte. Un datagramma ricevuto da
un indirizzo diverso da quello annunciato viene perso come segnale di traversal.
Mancano quindi:

- conferma che una coppia di candidati sia realmente bidirezionale;
- candidato peer-reflexive appreso dalla sorgente effettiva;
- triggered check verso la sorgente appena osservata;
- nomination della coppia prima di avviare QUIC;
- RTT e reason code del tentativo.

ICE apprende i peer-reflexive candidate proprio dai connectivity check e li
preferisce ai server-reflexive. Riferimento: [RFC 8445, sezioni 7.2.5.3.1 e
7.3.1.3](https://www.rfc-editor.org/rfc/rfc8445.html).

Questo non rende magicamente bucabile APDM+APDF su entrambi i lati. Migliora pero
i casi in cui almeno un primo probe arriva, il mapping osservato differisce da
STUN, il NAT usa ADF/EIF, una porta predetta e corretta o piu interfacce offrono
una coppia valida.

### P0 - Il live path non usa il profilo NAT

Il primo STUN riuscito basta a iniziare il tentativo. `selected_stun` e quasi
solo metadata; non dimostra che il mapping verso il peer sara uguale. In
particolare, allineare i peer sullo stesso STUN non risolve un mapping APDM,
perche la destinazione peer resta diversa dalla destinazione STUN.

La classificazione multi-STUN e il `NatPlan` sono confinati a `test-udp`. Il
runtime non sa se prediction e plausibile, non distingue mapping da filtering e
non modifica pacing, candidate pair o retry.

### P0 - Candidati peer-controlled non limitati

Il broker inoltra un `Vec<SocketAddr>` e il ricevente crea invii e future QUIC
per ogni elemento. Devono essere introdotti:

- `MAX_UDP_CANDIDATES` e `MAX_UDP_CHECKS_PER_ROUND`;
- deduplica prima del wire e dopo la ricezione;
- rifiuto di porta zero, unspecified, multicast e broadcast;
- rate limit e budget byte/pacchetti per round;
- dimensione massima dei nuovi frame;
- log aggregati, non un warning per ogni stray.

Gli indirizzi privati non possono essere vietati: servono per same-LAN. Anche la
sorgente accettata non va filtrata rispetto ai candidati offerti: un peer CGNAT
legittimo puo arrivare da una sorgente non annunciata e il token resta il gate di
autenticazione.

### P1 - IPv4 e una sola interfaccia

`bind_socket` crea `Domain::IPV4`, la risoluzione preferisce IPv4 e
`primary_local_ip()` offre un solo host candidate. Questo perde:

- IPv6 globale, spesso disponibile proprio sulle reti mobili dietro CGNAT IPv4;
- una seconda scheda Wi-Fi/Ethernet/VPN valida;
- una route migliore di quella scelta dal solo indirizzo primario;
- la possibilita di correre IPv4 e IPv6 in stile Happy Eyeballs.

ICE usa piu indirizzi anche per multihoming e dual stack; RFC 8421 raccomanda di
intercalare IPv6 e IPv4 nella checklist. Riferimenti: [RFC
8445](https://www.rfc-editor.org/rfc/rfc8445.html) e [RFC
8421](https://www.rfc-editor.org/rfc/rfc8421.html).

### P1 - Lease UPnP senza proprietario

`upnp_candidate()` richiede un mapping di 120 secondi, restituisce solo
`SocketAddr` e perde l'oggetto che dovrebbe gestirne il ciclo di vita. Non c'e
refresh, delete RAII o re-offer se il router cambia porta. Un provider secret
persistente puo continuare a pubblicare un candidato scaduto a consumer nuovi.

### P1 - STUN lento e minimale

Ogni target STUN puo consumare tre timeout da un secondo; quattro target non
raggiungibili possono aggiungere circa 12 secondi prima del relay. Le query sono
seriali e `discover_reflexive` possiede direttamente `recv_from`, quindi non si
possono demultiplexare in sicurezza piu transazioni o probe peer sullo stesso
socket.

Il parser non implementa l'intero modello di [RFC
8489](https://www.rfc-editor.org/rfc/rfc8489.html), il responder e IPv4-only e
non fornisce OTHER-ADDRESS/CHANGE-REQUEST. Non e necessario implementare tutto
STUN, ma serve un transaction layer robusto prima di aggiungere concorrenza.

### P1 - Retry diagnostico con candidati stantii

In `run_udp_path` il secondo tentativo binda un socket nuovo ma non riesegue
discovery e signaling. Se il NAT assegna una nuova porta, il peer continua a
provare quella vecchia. Questo va corretto prima di usare i risultati della
diagnostica per scegliere policy live.

### P1 - Mancano test NAT realistici

Senza un NAT lab ripetibile non si puo dimostrare che una nuova tecnica aumenti
il direct rate. I loopback test possono solo provare orchestration e QUIC; non
provano il comportamento di un middlebox.

## Tecniche valutate

| Tecnica | Aumenta il direct rate | Costo/rischio | Decisione |
|---|---:|---:|---|
| Connectivity check autenticati + peer-reflexive | Alto nei casi parzialmente bucabili | Medio | Implementare per prima |
| Checklist ICE-like, pacing e nomination | Alto per affidabilita e latenza | Medio | Implementare, senza full ICE iniziale |
| Trickle candidate/check | Medio, soprattutto sulla latenza | Medio | Implementare dopo il transaction engine |
| IPv6 dual stack | Molto alto su mobile/CGNAT IPv4 | Alto, cross-platform | Priorita alta |
| Piu host candidate/interfacce | Medio | Medio, privacy/routing | Implementare con filtri e priorita |
| Refresh UPnP | Medio sui router domestici | Basso | Correggere presto |
| PCP MAP/PEER | Medio dove il gateway lo supporta | Medio | Aggiungere dopo il lease manager |
| NAT-PMP | Basso/medio, utile su alcuni gateway | Medio | Opzionale dopo PCP |
| Port prediction misurata | Basso/medio su NAT sequenziali | Alto rischio scan/falsi positivi | Solo opt-in e bounded |
| RFC 5780 mapping/filtering discovery | Non da sola; migliora la policy | Infrastruttura con 2 IP | Implementare come segnale, non prerequisito |
| QUIC migration/path validation | Migliora la continuita dopo il connect | Medio | Fase successiva, non traversal iniziale |
| TURN o relay UDP | No, e un relay | Alto costo server | Solo fallback opzionale |
| Aumentare il range di porte alla cieca | Poco prevedibile | Alto rischio operativo | Non fare |
| Sostituire QUIC con KCP/custom UDP | Non cambia il NAT | Alto | Non fare |

PCP crea mapping espliciti con lifetime e rinnovo dichiarati: [RFC
6887](https://www.rfc-editor.org/rfc/rfc6887.html). RFC 5780 puo distinguere
mapping e filtering ma richiede normalmente due indirizzi pubblici del server:
[RFC 5780](https://www.rfc-editor.org/rfc/rfc5780.html). TURN va classificato
correttamente come relay per i casi non bucabili: [RFC
8656](https://www.rfc-editor.org/rfc/rfc8656.html).

## Architettura obiettivo

### Candidate tipizzato

Modello concettuale:

```text
UdpCandidate
  addr: SocketAddr
  kind: Host | ServerReflexive | PeerReflexive | RouterMapped | Manual | Predicted
  base: SocketAddr | none
  family: V4 | V6
  priority: u32
  foundation: compact-id
  interface_id: opaque | none
  generation: u32
```

`interface_id` non deve esporre nomi di schede sul wire. I candidati manuali
restano la proposta descritta in
[docs/nat/PLAN_MANUAL_UDP_CANDIDATES.md](docs/nat/PLAN_MANUAL_UDP_CANDIDATES.md),
ma entrano nello stesso modello.

### Profilo e piano

```text
NatProfile
  mapping: EIM | ADM | APDM | Unknown
  filtering: EIF | ADF | APDF | Unknown
  port_allocation: Preserved | Sequential(delta, confidence) | Random | Unknown
  hairpin: Yes | No | Unknown
  ipv6_reachable: Yes | No | Unknown
  explicit_mapping: Upnp | Pcp | Manual | None
  confidence: u8

NatPlan
  generation: u32
  mode: DirectFirst | DirectWhileRelay | RelayOnly
  pairs: ordered candidate pairs
  check_interval_ms: bounded
  max_checks: bounded
  total_budget_ms: bounded
  prediction_window: bounded
  reason: stable enum
```

Il relay non e un candidate da connettere in coda. E gia attivo e resta caldo
mentre il direct path viene provato. `DirectWhileRelay` descrive quindi meglio il
comportamento reale di `RelayFirst`.

### Traversal socket actor

Un solo task deve possedere il socket durante discovery e connectivity check:

```text
UDP recv loop
  -> STUN transaction by txid
  -> authenticated peer check by session/generation/txid
  -> QUIC handoff only after pair nomination
```

Questo evita che due `recv_from` concorrenti si rubino pacchetti. Prima della
fase 2 serve uno spike su Quinn:

1. VPN e diagnostica possono consegnare il socket a Quinn dopo la nomination.
2. Il provider secret persistente deve continuare a servire consumer successivi
   quando Quinn possiede gia il socket.
3. Va verificato se un wrapper `quinn::AsyncUdpSocket` puo demultiplexare probe e
   QUIC senza cambiare 5-tuple.
4. Se il wrapper non e sostenibile, i consumer successivi mantengono
   `punch_via_endpoint` come fallback compatibile; non si usa un secondo socket
   fingendo che abbia lo stesso mapping NAT.

### Connectivity check bore

Non serve copiare tutto il framing ICE/STUN. Il probe minimo deve avere:

```text
magic | version | session_id | generation | transaction_id | role | HMAC
```

La chiave deriva dal token gia ottenuto da `HMAC(secret, nonce)`. La risposta
include transaction id, indirizzo sorgente osservato e HMAC. Proprieta:

- nessuna risposta a pacchetti non autenticati;
- risposta non piu grande della richiesta;
- replay isolato da generation e transaction id;
- triggered check verso una nuova sorgente autenticata;
- nomination solo dopo request/response valida;
- il QUIC token handshake resta obbligatorio come secondo gate.

## Invarianti non negoziabili

1. `--udp` off mantiene il path TCP byte-for-byte.
2. Il relay resta disponibile durante discovery, check, retry e direct failure.
3. Un errore UDP non termina il tunnel o la VPN se il relay e vivo.
4. Nessun `SO_REUSEADDR` sui socket di punch.
5. Un socket/5-tuple ha un solo proprietario di lettura; niente race tra STUN,
   probe e Quinn.
6. Non filtrare una sorgente accettata in base ai candidati offerti; autenticare
   con token.
7. In VPN restano attivi `filter_tunneled_candidates`, relay warm, carrier count
   negoziato, nonce condiviso e PMTU corrente.
8. Nessuna modifica riduce il connection receive window QUIC verso lo stream
   receive window.
9. Public/vhost UDP continuano a condividere il QUIC endpoint senza acquisire
   semantica hole-punch.
10. I nuovi campi wire hanno `#[serde(default)]`; un peer senza capability usa il
    comportamento legacy.
11. Ogni lista peer-controlled ha un limite verificato prima di allocare o
    generare task.
12. Prediction resta opt-in, rate-limited e spiegata nei log.

## Piano per fasi

### Fase 0 - Baseline, hardening e laboratorio NAT

Obiettivo: misurare il comportamento attuale e creare gate che distinguano un
miglioramento reale da un test loopback verde.

Implementazione:

- definire metriche `discovery_ms`, `checks_ms`, `direct_ready_ms`, tipo della
  coppia vincente, numero probe, fallback reason e generation;
- limitare e deduplicare candidati e check senza cambiare l'ordine legacy;
- validare indirizzi e porte in un helper comune;
- correggere `test-udp`: ogni retry con socket nuovo deve rieseguire discovery,
  scambio candidati e piano;
- rendere esplicito nei log che l'ordine adattivo non governa ancora
  `connect_direct` concorrente;
- costruire un NAT lab deterministico in userspace con profili EIM/ADM/APDM e
  EIF/ADF/APDF, port preservation, sequenziale, random, hairpin e remap;
- aggiungere netns smoke per doppio NAT, UDP bloccato e routing reale;
- catturare una baseline di almeno: open/open, EIM+APDF/EIM+APDF,
  EIM+ADF/APDM, APDM/APDM, same-LAN e IPv4 blocked.

File probabili:

- `src/holepunch.rs`, `src/udp_diagnostic.rs`, `src/shared.rs`;
- `tests/udp_test.rs`, nuovo `tests/nat_traversal_test.rs`;
- nuovo `scripts/udp_nat_netns_test.sh`.

Test e gate:

- candidate count sopra limite rifiutato senza allocazione proporzionale;
- multicast, unspecified e porta zero rifiutati;
- candidati privati validi preservati;
- retry diagnostico pubblica una nuova generation e nuovi candidati;
- NAT lab ripetibile, nessuna dipendenza da STUN pubblico;
- baseline salvata come tabella nel documento di test, non come soglia inventata.

Documentazione nella stessa fase:

- `README.md` per limiti e diagnostica;
- `docs/nat/NAT_TRAVERSAL.md` per includere VPN e correggere le parti ormai
  superate;
- `TEST_UDP.md` per il nuovo laboratorio.

Exit criteria:

- zero regressioni;
- ogni scenario produce direct/relay e reason deterministici;
- nessuna tecnica successiva entra senza un test rosso sul profilo che dovrebbe
  migliorare.

### Fase 1 - Candidate model v2 e transaction engine

Obiettivo: portare sul wire tipi, priorita, generation e capability senza
alterare il comportamento dei peer legacy.

Implementazione:

- estendere `UdpCandidateOffer` con campi opzionali `typed_candidates`,
  `generation`, `capabilities` e `profile_hint`;
- mantenere `candidates: Vec<SocketAddr>` come compatibilita old/new;
- estendere `UdpPunch` con `plan: Option<UdpTraversalPlan>` e generation;
- introdurre `UdpTraversalSocket` come unico owner pre-QUIC;
- demultiplexare STUN per transaction id e sorgente completa `IP:port`;
- applicare un budget globale alla catena STUN e query staggered/paced, evitando
  il worst case seriale di circa 12 secondi;
- valutare una crate STUN mantenuta prima di estendere il parser manuale. La
  scelta deve supportare socket gia bindato, RFC 8489, IPv6 e test vector;
- conservare esattamente il gather legacy se la capability v2 manca.

Compatibilita wire richiesta, da dimostrare nella matrice di test:

- new client + old server: i campi extra devono essere ignorati e la lista
  legacy deve restare utilizzabile;
- old client + new server: default vuoti, `UdpPunch` legacy;
- new/new: v2 attivo solo se entrambi annunciano la capability;
- nessun `RelayOnly` dedotto da metadata mancanti.

Test e gate:

- matrice old/new per secret, VPN 1:1 e hub;
- demux con risposte STUN fuori ordine, duplicate, txid errato e pacchetto peer
  intercalato;
- timeout globale rispettato con tutti gli STUN irraggiungibili;
- property/fuzz test del parser e dei limiti frame;
- default v2 produce la stessa lista e lo stesso direct/relay del legacy.

Documentazione:

- `README.md`, `docs/nat/ADAPTIVE_NAT.md`, `docs/nat/NAT_TRAVERSAL.md`;
- commenti wire in `src/shared.rs`.

Exit criteria:

- v2 osservabile ma non operativo;
- matrix old/new tutta verde;
- un solo recv owner per socket dimostrato dai test.

### Fase 2 - Connectivity check autenticati e peer-reflexive

Obiettivo: validare coppie reali prima di QUIC e apprendere l'endpoint effettivo
del peer.

Implementazione:

- aggiungere request/response HMAC con generation e transaction id;
- eseguire check paced sulle candidate pair compatibili per address family;
- su request valida da una sorgente nuova, creare peer-reflexive e accodare un
  triggered check;
- nominare la prima coppia bidirezionale valida;
- consegnare il medesimo socket a Quinn e dialare solo la coppia nominata, con
  fallback Happy Eyeballs sulle successive coppie valide;
- mantenere `connect_direct` legacy per peer senza capability;
- per provider secret successivi usare il risultato dello spike Quinn. Se non
  e disponibile il demux post-handoff, mantenere la ri-bucatura QUIC esistente e
  segnalarne la modalita nei log;
- non rispondere mai a probe non autenticati.

Test e gate:

- EIM+ADF/APDM nel NAT lab apprende la sorgente e stabilisce direct quando il
  primo check puo arrivare;
- APDM+APDF/APDM+APDF resta relay senza prediction: nessun falso positivo;
- porta predetta corretta diventa peer-reflexive e viene nominata;
- txid/HMAC/generation errati non producono risposta;
- response size <= request size e rate limit rispettato;
- sorgente CGNAT non offerta ma autenticata accettata;
- primo consumer, reconnect e consumer concorrenti del provider persistente;
- VPN 1:1/hub non cambia carrier count e resta sul relay dopo check falliti.

Documentazione:

- `README.md` con il nuovo flow end-to-end;
- `docs/nat/NAT_TRAVERSAL.md` con peer-reflexive e limiti reali;
- `TEST_UDP.md` con scenari e packet trace.

Exit criteria:

- aumento misurato del direct rate in almeno un profilo prima fallente;
- nessun peggioramento statisticamente significativo su EIM/EIM;
- time-to-relay non supera il budget legacy.

### Fase 3 - Checklist e policy adattiva live

Obiettivo: usare `NatProfile` e `NatPlan` in secret e VPN, con relay gia attivo.

Implementazione:

- rinominare/estendere la policy in termini di mapping, filtering, confidence e
  pair priority, evitando parsing di label testuali;
- calcolare il piano server-side solo quando entrambi i profili v2 sono presenti;
- usare una checklist con gruppi staggered, non serializzazione completa e non
  fan-out simultaneo illimitato;
- provare presto host/same-LAN, IPv6 globale, router/manual, peer-reflexive,
  server-reflexive e infine predicted; l'ordine esatto va fissato dai dati della
  fase 0;
- avviare check non appena arrivano candidati utilizzabili, in stile Trickle ICE,
  invece di attendere tutta la catena STUN;
- usare backoff con jitter e generation nuove; scartare reply di round vecchi;
- memorizzare per breve tempo la coppia vincente e provarla prima al reconnect,
  invalidandola subito al primo fallimento;
- mantenere il retry grid VPN e il backoff secret come outer scheduler; il piano
  governa solo un singolo round bounded.

Trickle ICE consente gathering e check in parallelo per ridurre il setup time:
[RFC 8838](https://www.rfc-editor.org/rfc/rfc8838.html).

Test e gate:

- test tabellari puri per ogni coppia di profili;
- fake clock per pacing, jitter bounded, budget e stale generation;
- nessun check predicted quando prediction e off;
- candidate order realmente osservato dal transport mock;
- direct buono disponibile prima che un ultimo STUN lento termini;
- relay continua a inoltrare traffico durante ogni retry;
- server vecchio o metadata incompleti mantengono il comportamento legacy.

Documentazione:

- `README.md` come source of truth per flow, flag e fallback;
- `docs/nat/ADAPTIVE_NAT.md` aggiornato dallo stato preview allo stato live;
- `docs/nat/NAT_TRAVERSAL.md` con policy e reason code.

Exit criteria:

- direct-ready p50 migliore della baseline;
- fallback p95 bounded;
- nessun aumento non limitato di probe o memoria;
- policy disabilitabile con kill switch server-side.

### Fase 4 - IPv6 dual stack e multihoming

Obiettivo: usare il path diretto IPv6 quando evita NAT/CGNAT IPv4 e provare piu
interfacce valide.

Implementazione:

- introdurre socket/candidate per famiglia, evitando l'assunzione di un solo
  wildcard IPv4;
- aggiungere XOR-MAPPED-ADDRESS e responder bore IPv6 completi;
- raccogliere indirizzi globali IPv6 e host IPv4/IPv6 per interfaccia;
- escludere loopback, unspecified, multicast, link-local senza scope valido,
  interfacce down e candidate instradate dentro la TUN;
- assegnare priorita senza esporre il nome della scheda;
- intercalare IPv6 e IPv4 con delay bounded, non attendere il fallimento totale
  di una famiglia;
- gestire privacy address e cambio rete come generation nuova;
- verificare Quinn su Linux, macOS, Windows e Android prima di rendere il default
  cross-platform.

Privacy:

- piu host candidate rivelano piu indirizzi al peer e al broker;
- inviarli solo sul control channel e legarli al token di sessione; raccomandare
  `--secret` quando si abilita la condivisione di piu host candidate;
- aggiungere policy per disabilitare host candidate o limitare le interfacce;
- non loggare indirizzi completi a livello `info` se non necessario.

Test e gate:

- IPv6-only direct;
- dual stack con IPv6 buono/IPv4 rotto e viceversa;
- IPv6 black-hole non ritarda IPv4 oltre il budget;
- due interfacce, prima irraggiungibile e seconda valida;
- candidate VPN dentro `peer_routes` sempre scartata;
- responder STUN round trip IPv4 e IPv6;
- CI macOS/Windows/Android compile + test disponibili, Linux netns e2e.

Documentazione:

- `README.md` con deploy firewall IPv4/IPv6 e tutti i modi;
- `docs/nat/NAT_TRAVERSAL.md` elimina il limite IPv4-only;
- guide piattaforma interessate.

Exit criteria:

- due peer IPv6-only possono stabilire direct;
- dual stack converge senza regressione IPv4;
- nessun routing loop VPN.

### Fase 5 - Port mapping gestito: UPnP, PCP e manuale

Obiettivo: trasformare i mapping espliciti in risorse vive e preferibili, non in
indirizzi best-effort che scadono.

Implementazione:

- introdurre trait/enum interno `PortMappingLease` con candidate, lifetime,
  refresh, changed e delete-on-drop best-effort;
- mantenere e rinnovare il lease UPnP prima della scadenza;
- re-offer con generation nuova se IP o porta cambiano;
- aggiungere PCP MAP; valutare PEER solo dopo test reali di compatibilita;
- rilevare Epoch Time PCP e ricreare il mapping dopo reboot/reset del gateway;
- provare in ordine configurabile PCP, UPnP, manuale e infine implicit mapping;
- integrare `--udp-candidate` e `--udp-no-stun` del piano manuale nel candidate v2;
- valutare NAT-PMP solo come adapter opzionale, senza bloccare PCP;
- nessun mapping automatico nuovo senza opt-in iniziale. Dopo telemetria e
  documentazione si puo valutare un `--port-map auto` separato da `--upnp`.

Test e gate:

- fake gateway con lease, refresh, cambio porta, reboot epoch e delete;
- provider secret resta raggiungibile oltre due lifetime;
- mapping scaduto mai pubblicato a consumer nuovi;
- refresh failure mantiene relay e ritenta con backoff;
- RAII non cancella il mapping di un altro processo/tunnel;
- collisione `--nat-udp-preferred-port` continua a degradare a ephemeral senza
  `SO_REUSEADDR`;
- manual candidate funziona con STUN bloccato.

Documentazione:

- `README.md` con flag, sicurezza router, deploy e esempi completi;
- `docs/nat/PLAN_MANUAL_UDP_CANDIDATES.md` marcato implementato per le parti
  concluse;
- `docs/nat/NAT_TRAVERSAL.md` con PCP/UPnP lifetime.

Exit criteria:

- lease osservabile e rinnovato;
- direct resta disponibile a nuovi peer dopo il lifetime originale;
- nessun mapping orfano permanente creato da bore.

### Fase 6 - RFC 5780, prediction misurata e policy finale

Obiettivo: migliorare la decisione sui NAT difficili senza aumentare alla cieca
il traffico di probe.

Implementazione:

- rendere opzionale sul server una topologia RFC 5780 con due IP e due porte;
- implementare OTHER-ADDRESS, RESPONSE-ORIGIN e CHANGE-REQUEST necessari;
- classificare separatamente mapping e filtering, con confidence e timestamp;
- misurare direzione e delta di allocazione usando lo stesso source port;
- generare prediction in entrambe le direzioni solo se le osservazioni sono
  coerenti e recenti;
- mantenere un massimo piccolo di porte e un budget globale;
- disabilitare automaticamente prediction dopo risultati random o ripetuti
  timeout;
- usare la classificazione per evitare round inutili, mai per rimuovere il relay;
- aggiungere test hairpin e binding lifetime solo in diagnostica, senza tenere
  occupato il setup live.

RFC 5780 avverte che il comportamento NAT puo cambiare sotto carico e nel tempo.
Il profilo e quindi un hint con scadenza, non una verita persistente.

Test e gate:

- mapping EIM/ADM/APDM e filtering EIF/ADF/APDF classificati separatamente;
- server con un solo IP dichiara `Unknown`, non inventa il filtering;
- NAT sequenziale crescente e decrescente genera il range corretto;
- NAT random non genera prediction;
- port prediction non supera limite o packet budget;
- test di carico del responder e anti-amplification.

Documentazione:

- `README.md` con requisiti deploy dei due IP e modalita opzionale;
- `docs/nat/NAT_TRAVERSAL.md` con confidence e interpretazione;
- `TEST_UDP.md` con output diagnostico stabile.

Exit criteria:

- prediction migliora solo gli scenari sequenziali del NAT lab;
- nessun aumento del direct false-positive o delle scansioni;
- comportamento con server STUN minimale invariato.

### Fase 7 opzionale - Fallback UDP relay e continuita QUIC

Questa fase non aumenta il numero di connessioni dirette. Migliora il trasporto
quando il direct e impossibile o quando cambia la rete.

Possibili lavori separati:

- relay UDP/QUIC bore o integrazione TURN come candidate relayed;
- mantenere TCP relay come fallback universale;
- valutare QUIC path migration per NAT rebinding o cambio rete dopo il connect;
- usare path validation prima di spostare traffico;
- mantenere il relay caldo durante ogni migrazione.

QUIC supporta path validation e migration, ma non scopre da solo un path iniziale
attraverso due NAT: [RFC 9000](https://www.rfc-editor.org/rfc/rfc9000.html).

Questa fase va avviata solo con un requisito misurato: per esempio reti che
consentono UDP verso il server ma non peer-to-peer, dove il relay TCP causa HOL o
latenza inaccettabile.

## Ordine di rollout

Ogni nuova semantica usa capability negotiation e kill switch:

1. `observe`: calcolo e log, esecuzione legacy;
2. `diagnostic`: attiva solo in `test-udp`;
3. `secret-canary`: percentuale o allowlist di tunnel secret;
4. `secret-default`: solo dopo confronto metriche;
5. `vpn-1to1-canary`;
6. `vpn-hub-canary`;
7. default generale.

Non partire dalla VPN hub: ha il blast radius maggiore e i suoi round per peer
devono restare indipendenti.

## Metriche di accettazione

Per ogni profilo NAT e address family raccogliere:

- `direct_success_rate`;
- `time_to_first_candidate`, `time_to_nominated_pair`, `time_to_quic`;
- `time_to_working_relay`;
- numero candidate, pair e probe;
- byte di traversal inviati/ricevuti;
- tipo della coppia vincente;
- fallback reason stabile;
- direct stabilito e morto entro 10/30/120 secondi;
- numero retry e stale generation scartate;
- lease mapping creati, rinnovati, cambiati e falliti.

Soglie iniziali:

- zero calo su open/open ed EIM/EIM rispetto alla baseline con intervallo di
  confidenza dichiarato;
- nessun aumento del p95 time-to-relay oltre il budget esplicito del round;
- packet count sempre <= budget;
- nessuna allocazione proporzionale a un count non validato;
- zero falso direct: il path e direct solo dopo QUIC autenticato.

## Gate CI e regressione

Per ogni fase completata:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo test --test udp_test
cargo test --test public_udp_test
```

Quando viene toccata VPN:

```bash
cargo build --release --features vpn
sudo -n /abs/path/scripts/vpn_netns_test.sh
```

Quando viene toccato il traversal secret:

```bash
sudo -n /abs/path/scripts/local_proxy_netns_test.sh
sudo -n /abs/path/scripts/udp_nat_netns_test.sh
```

Usare il path assoluto realmente autorizzato da sudoers. Il nuovo NAT lab deve
fallire se il binary release e piu vecchio di `src/`, come il gate VPN.

Gate invarianti aggiuntivi:

- `scripts/vhost_udp_concurrency_repro.sh` resta verde;
- stress preferred-port VPN+secret resta verde;
- nessun warning per stray benigni in `DirectListener::accept`;
- nessuna modifica a `SO_REUSEADDR` o ai window default QUIC;
- packet capture dimostra che una richiesta non autenticata non riceve risposta;
- old/new wire matrix eseguita in CI almeno per una release di transizione.

## Rischi e mitigazioni

### Complessita simile a ICE

Rischio: ricostruire male un protocollo complesso.

Mitigazione: usare solo candidate pair, paced checks, peer-reflexive, triggered
checks e nomination; ruoli bore gia fissi; valutare prima una crate mantenuta;
tenere il relay fuori dalla state machine direct.

### Demux con Quinn

Rischio: perdere pacchetti o cambiare 5-tuple quando il socket passa a Quinn.

Mitigazione: spike bloccante nella fase 1, un solo recv owner, test del provider
persistente, nessun secondo socket presentato come equivalente.

### Latenza aggiunta dai check

Rischio: un passaggio in piu rallenta i casi facili.

Mitigazione: check staggered, nomination immediata, STUN trickle, budget globale e
QUIC avviato appena la prima coppia e valida. Il fixed sleep attuale di circa 250
ms puo essere rimosso, compensando il round trip del check.

### Esposizione indirizzi locali

Rischio: il multihoming rivela piu topologia.

Mitigazione: token di sessione, autenticazione applicativa con `--secret`, filtri
per interfaccia, policy opt-out, log ridotti e identificatori opachi.

### Abuso come scanner o amplificatore

Rischio: candidati arbitrari causano traffico verso terzi.

Mitigazione: HMAC, no response a probe invalidi, limiti candidate/check, pacing,
response non amplificativa, rate limit per tunnel/IP e prediction opt-in.

### Policy basata su NAT class stantia

Rischio: un NAT cambia comportamento sotto carico o dopo un cambio rete.

Mitigazione: confidence, TTL breve, generation, invalidazione al fallimento e
connectivity check come verita finale.

## Raccomandazione finale

La sequenza con miglior rapporto beneficio/rischio e:

1. Fase 0 e 1 come prerequisiti obbligatori;
2. Fase 2 per il primo aumento reale del direct rate;
3. Fase 3 per latenza, retry e uso live della policy;
4. Fase 4 per il maggiore ampliamento di copertura su mobile e CGNAT IPv4;
5. Fase 5 per affidabilita domestica e mapping espliciti;
6. Fase 6 solo con infrastruttura RFC 5780 e dati che giustifichino prediction;
7. Fase 7 separata, perche migliora il relay ma non il direct path.

Il criterio guida deve restare semplice: un profilo NAT sceglie quali prove vale
la pena fare, ma solo un connectivity check autenticato e poi QUIC autenticato
dimostrano che la connessione diretta esiste.
