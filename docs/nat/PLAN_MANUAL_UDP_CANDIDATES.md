# Piano: candidati UDP manuali senza STUN

## Obiettivo

Valutare e pianificare una modalita in cui `bore local --udp`, `bore proxy --udp`
e `bore test-udp --tcp-secret-id` possano usare endpoint UDP pubblici inseriti
manualmente, invece di dipendere sempre da STUN per scoprire il candidato
reflexive.

Scenario di riferimento:

- peer provider/client dietro firewall o NAT rigido;
- peer proxy/consumer nella stessa situazione;
- STUN non utilizzabile o bloccato;
- entrambi conoscono il proprio IP pubblico;
- entrambi possono usare una porta UDP nota in uscita, idealmente la stessa porta
  locale/pubblica su entrambi i lati;
- il server bore resta raggiungibile via TCP/TLS per controllo, autenticazione e
  brokering dei candidati.

Questa feature non vuole sostituire il relay TCP: vuole dare una strada diretta
quando la discovery STUN fallisce ma l'operatore conosce gia endpoint UDP pubblici
stabili.

## Risposta breve: vale la pena?

Si, vale la pena implementarla come feature opzionale, ma solo con caveat molto
chiari.

Nel tuo caso specifico, conoscenza dell'IP pubblico e porta UDP in uscita uguale
su entrambi i peer e una base buona, ma non basta da sola. La feature ha valore se
quella porta corrisponde anche all'endpoint pubblico realmente raggiungibile dal
peer remoto, oppure se esiste una mappatura statica/port-preserving affidabile.

In pratica funziona bene quando almeno una di queste condizioni e vera:

- la macchina ha direttamente un IP pubblico e il firewall consente UDP sulla
  porta scelta;
- c'e un port-forward statico UDP dal router/firewall verso la macchina;
- il NAT e port-preserving e mantiene `porta_locale == porta_pubblica` verso il
  peer;
- entrambi i firewall sono stateful e consentono il traffico di ritorno dopo che
  ciascun peer ha inviato UDP verso l'altro endpoint;
- una policy aziendale/cloud blocca STUN pubblici, ma consente UDP tra gli IP dei
  due peer.

Non risolve invece questi casi:

- si conosce solo l'IP pubblico, ma non la porta UDP pubblica effettiva;
- il NAT e simmetrico/random e sceglie una porta pubblica diversa per ogni
  destinazione;
- c'e CGNAT senza port mapping controllabile;
- il firewall permette una generica uscita UDP ma blocca ritorni o destinazioni
  non allow-listate;
- il provider cloud/router non inoltra davvero la porta pubblica verso il peer;
- entrambi i peer sono dietro firewall che non permettono pacchetti UDP verso
  l'endpoint pubblico dell'altro.

Conclusione: e una feature utile, a basso impatto sul protocollo, e coerente con
l'architettura attuale. Pero va presentata come `manual UDP candidates`, non come
"bypass universale" per NAT difficili.

## Concetto operativo

Oggi il path UDP direct funziona cosi:

1. provider e proxy si collegano al server bore via TCP/TLS;
2. ciascun peer apre un socket UDP locale;
3. ciascun peer usa STUN per scoprire il proprio endpoint pubblico;
4. il server scambia le liste di candidati tra provider e proxy;
5. i peer fanno punch UDP verso gli endpoint ricevuti;
6. se QUIC si connette, i dati passano peer-to-peer;
7. se fallisce, resta il relay TCP.

La modalita manuale cambierebbe solo il punto 3:

- invece di ricavare sempre l'endpoint pubblico via STUN, il peer puo dichiarare
  uno o piu candidati pubblici manuali;
- il server continua a brokerizzare una lista di `SocketAddr`;
- il protocollo puo restare invariato;
- il fallback relay resta invariato.

Ogni peer deve dichiarare il proprio endpoint pubblico, non quello del peer
remoto. Il server consegnera poi quell'informazione all'altro lato.

## Esempio di CLI desiderata

Server:

```shell
bore server --udp
```

Il flag `--udp` sul server resterebbe necessario per abilitare il brokering del
path diretto. In modalita manuale non serve che lo STUN UDP del server sia
raggiungibile dai peer, ma il codice server oggi usa comunque `--udp` come
interruttore generale per direct UDP.

Provider/client:

```shell
bore local 8000 \
  --to https://bore.example.com \
  --secret mysecret \
  --tcp-secret-id svc \
  --udp \
  --udp-no-stun \
  --nat-udp-preferred-port 41641 \
  --udp-candidate A_PUBLIC_IP:41641
```

Proxy/consumer:

```shell
bore proxy \
  --to https://bore.example.com \
  --secret mysecret \
  --tcp-secret-id svc \
  --local-proxy-port :5555 \
  --udp \
  --udp-no-stun \
  --nat-udp-preferred-port 41641 \
  --udp-candidate B_PUBLIC_IP:41641
```

Diagnostica paired:

```shell
bore test-udp \
  --to https://bore.example.com \
  --secret mysecret \
  --tcp-secret-id svc \
  --udp-no-stun \
  --nat-udp-preferred-port 41641 \
  --udp-candidate MY_PUBLIC_IP:41641 \
  --test-bandwidth \
  --test-transfer-quota 500MB
```

Nota importante: `--nat-udp-preferred-port` e la porta locale da bindare. Il
candidato manuale e l'endpoint da pubblicizzare al peer. Se il firewall/router
mappa `10.0.0.10:41641` in `203.0.113.20:50000`, il comando corretto e:

```shell
--nat-udp-preferred-port 41641 --udp-candidate 203.0.113.20:50000
```

Se invece il mapping e port-preserving:

```shell
--nat-udp-preferred-port 41641 --udp-candidate 203.0.113.20:41641
```

## Flag proposti

### `--udp-candidate <IP:PORT>`

Candidato UDP manuale da pubblicizzare al peer. Ripetibile.

Esempi:

```shell
--udp-candidate 203.0.113.10:41641
--udp-candidate 198.51.100.20:41641 --udp-candidate 10.8.0.2:41641
```

Env proposta:

```shell
BORE_UDP_CANDIDATES=203.0.113.10:41641,10.8.0.2:41641
```

### `--udp-no-stun`

Salta completamente la risoluzione STUN e usa solo candidati manuali/locali/UPnP
se abilitati.

Env proposta:

```shell
BORE_UDP_NO_STUN=true
```

Comportamento consigliato:

- se `--udp-no-stun` e attivo e non ci sono candidati manuali utili, loggare un
  warning chiaro e restare sul relay;
- se `--udp-no-stun` non e attivo, aggiungere i candidati manuali a quelli STUN;
- se STUN fallisce ma sono presenti candidati manuali, continuare comunque;
- se STUN fallisce e non ci sono candidati manuali, comportamento attuale:
  candidato locale quando possibile e fallback relay.

### Nome alternativo valutabile

`--udp-public-endpoint <IP:PORT>` sarebbe piu esplicito per il caso singolo.
Pero `--udp-candidate` e piu aderente all'architettura attuale, perche il sistema
lavora gia con liste di candidati e in futuro puo accettarne piu di uno.

Scelta consigliata: `--udp-candidate`, documentando che per il caso normale e il
proprio endpoint pubblico.

## Limiti tecnici e problematiche

### 1. IP pubblico non basta

Per UDP serve `IP:porta`. Se si conosce solo l'IP pubblico ma non la porta
pubblica verso il peer, la connessione diretta non ha un bersaglio affidabile.

### 2. Porta locale e porta pubblica possono divergere

`--nat-udp-preferred-port 41641` forza il bind locale, ma non forza il NAT a usare
41641 come porta pubblica. Lo fa solo se il NAT/firewall e port-preserving o se
esiste una regola statica.

### 3. NAT simmetrico/random

Un NAT simmetrico puo assegnare porte pubbliche diverse per destinazioni diverse.
In quel caso una porta nota o scoperta verso una destinazione non garantisce che
sia valida verso l'altro peer.

La modalita manuale funziona solo se l'operatore conosce la porta pubblica valida
per il traffico verso l'altro peer, oppure se il NAT non cambia mapping per
destinazione.

### 4. Firewall stateful e allow-list

Molti firewall accettano risposte UDP solo dalla stessa destinazione verso cui e
stato inviato un pacchetto. Il punch simultaneo aiuta, ma se le policy richiedono
allow-list esplicita, entrambi i lati devono consentire UDP verso l'IP pubblico
dell'altro peer sulla porta indicata.

### 5. CGNAT

Se uno dei peer e dietro CGNAT e non ha un port-forward controllabile, il candidato
manuale non puo rendere raggiungibile una porta che non e instradata verso quel
peer. In quel caso serve relay, VPN, TURN-like relay o un endpoint pubblico vero.

### 6. Provider cloud e security group

Nei cloud non basta aprire la porta sul sistema operativo: security group, network
ACL, firewall host e container networking devono essere coerenti. Per Docker, il
socket UDP del peer deve bindare davvero la porta locale scelta.

### 7. Ambiguita tra provider e proxy

Il provider `bore local` e il consumer `bore proxy` devono passare ciascuno il
proprio candidato. Un errore comune sarebbe configurare su entrambi l'IP del peer
remoto. La documentazione deve essere molto esplicita.

### 8. Rischio di uso come port scan involontario

Un candidato manuale fa si che l'altro peer invii UDP verso quell'indirizzo.
Il rischio e limitato dall'autenticazione/secret e dal fatto che si tratta di peer
coordinati, ma conviene:

- limitare il numero massimo di candidati manuali;
- deduplicare la lista;
- rifiutare porta 0;
- evitare broadcast/multicast;
- loggare chiaramente i candidati usati;
- mantenere piccolo il frame JSON di controllo.

### 9. Limite `MAX_FRAME_LENGTH`

Il protocollo di controllo ha frame JSON piccoli. La lista candidati non deve
crescere troppo. Per l'MVP bastano pochi candidati manuali, per esempio massimo 8.

### 10. IPv4/IPv6

Il codice direct path attuale e centrato su socket UDP IPv4. L'MVP dovrebbe
supportare IPv4 manuale e rinviare IPv6 a una fase separata, salvo verifica
specifica del supporto end-to-end.

## Impatto sul codice

### Protocollo

Non serve cambiare il wire protocol per l'MVP.

Oggi i candidati viaggiano gia come `Vec<SocketAddr>`:

- provider: `ClientMessage::UdpCandidates(Vec<SocketAddr>)`;
- broker server: inoltro verso consumer/provider;
- paired test: `TestUdpJoin { candidates, ... }`.

I candidati manuali possono essere semplicemente aggiunti alla lista lato client.
Il server non deve sapere se un candidato viene da STUN, UPnP, porta predetta o
configurazione manuale.

### `src/main.rs`

Aggiungere i flag a:

- `local`;
- `proxy`;
- `test-udp`.

Parsing consigliato:

- `Vec<SocketAddr>` per `--udp-candidate` ripetibile;
- parser comma-separated per `BORE_UDP_CANDIDATES`;
- boolean per `--udp-no-stun`.

### `src/holepunch.rs`

Rifattorizzare la raccolta candidati:

```text
gather_candidates(socket, stun: Option<SocketAddr>, options)
```

Dove `options` contiene:

- candidati manuali;
- `port_map`;
- `port_prediction`;
- `no_stun` o STUN opzionale gia risolto.

Comportamento:

1. aggiungi candidati manuali validi;
2. se STUN e abilitato, prova reflexive discovery;
3. se STUN riesce, aggiungi reflexive e predizioni eventuali;
4. se UPnP e attivo, aggiungi mapping router;
5. aggiungi candidato locale se disponibile;
6. deduplica mantenendo ordine stabile;
7. ritorna lista anche se STUN fallisce, purche ci siano manuali.

### `src/client.rs`

`offer_provider_candidates()` deve ricevere e passare le nuove opzioni. Se STUN e
saltato ma ci sono candidati manuali, deve inviare comunque `UdpCandidates`.

### `src/secret.rs`

`gather_consumer_candidates()` deve ricevere e passare le nuove opzioni sia nella
negoziazione iniziale sia nel task di upgrade relay-to-direct.

### `src/udp_diagnostic.rs`

`test-udp --tcp-secret-id` deve supportare la stessa modalita. Il report dovrebbe
stampare in modo chiaro:

- candidati manuali configurati;
- STUN saltato o fallito;
- direct QUIC riuscito/fallito;
- relay TCP riuscito;
- differenza di latenza e bandwidth.

Non e obbligatorio cambiare il protocollo per mostrare il conteggio manuale: il
peer locale puo stamparlo nel proprio report. Se in futuro vogliamo mostrare anche
il tipo dei candidati remoti, allora servirebbe estendere il summary diagnostico.

## Piano di implementazione futura

1. Aggiungere parsing CLI/env per `--udp-candidate` e `--udp-no-stun`.
2. Introdurre una piccola struttura interna, per esempio `UdpCandidateOptions`,
   con candidati manuali, STUN opzionale, UPnP e port prediction.
3. Rifattorizzare `holepunch::gather_candidates()` senza cambiare il protocollo.
4. Propagare le opzioni in provider, proxy e paired diagnostic.
5. Aggiungere log chiari: manual candidates, STUN skipped, STUN failed but manual
   candidates available, final candidate list.
6. Cap massimo candidati manuali e validazioni base.
7. Aggiungere test unitari per parsing, deduplica e no-STUN.
8. Aggiungere test integrazione su loopback/fixed port per direct path manual-only.
9. Aggiungere test fallback relay quando il candidato manuale e sbagliato.
10. Aggiornare README, USER_GUIDE, NAT_TRAVERSAL, TEST_UDP,
    SERVER_UDP_OPTIMIZATION, CLAUDE e CHANGELOG.
11. Eseguire check completi all-features e no-default-features.

## Test da prevedere

### Unit test

- parsing singolo candidato;
- parsing candidati multipli da env comma-separated;
- rifiuto porta 0;
- rifiuto indirizzi multicast/broadcast;
- deduplica mantenendo ordine;
- `--udp-no-stun` con manuali ritorna candidati senza tentare STUN;
- STUN fallito piu manuali non produce errore.

### Integration/e2e

- provider e proxy con candidati manuali loopback e porta fissa: direct path
  funzionante;
- candidato manuale errato: direct fallisce e relay resta funzionante;
- `test-udp --tcp-secret-id` manual-only: report contiene direct e relay;
- `--udp-no-stun` senza candidati manuali: non crasha, fallback relay chiaro;
- build `--no-default-features`: flag accettati ma direct UDP disabilitato come
  oggi, senza rompere relay-only.

## Validazione consigliata

```shell
cargo fmt -- --check
cargo check --all-features
cargo check --no-default-features
cargo clippy --all-features -- -D warnings
cargo clippy --no-default-features -- -D warnings
cargo test --all-features --test udp_test
cargo test --all-features
cargo test --no-default-features
```

Test manuale reale su due peer:

1. avviare server con `bore server --udp`;
2. aprire/allow-listare UDP tra `A_PUBLIC_IP:PORT` e `B_PUBLIC_IP:PORT`;
3. avviare provider con il candidato pubblico di A;
4. avviare proxy con il candidato pubblico di B;
5. eseguire `bore test-udp --tcp-secret-id ... --test-bandwidth` con gli stessi
   candidati;
6. verificare che il report mostri direct UDP working e relay TCP fallback working.

## Decisione consigliata

Implementare la feature e consigliabile.

Motivi:

- il costo tecnico e basso: la pipeline usa gia liste di candidati;
- il protocollo puo restare compatibile;
- il fallback relay limita il rischio operativo;
- copre un caso reale: STUN bloccato ma operatori con endpoint pubblici noti;
- migliora anche `test-udp`, perche permette di distinguere "STUN non funziona"
  da "il direct UDP tra questi due endpoint non funziona".

Condizione: la documentazione deve essere molto onesta. La feature non garantisce
il direct path se l'unica informazione nota e l'IP pubblico. Serve un endpoint
UDP pubblico stabile e raggiungibile: `IP:porta`.

Per il caso che hai descritto, la risposta pratica e:

- se la porta in uscita uguale sui due peer rimane uguale anche come porta pubblica
  e i firewall consentono UDP verso l'altro peer, allora si, questa feature puo
  far funzionare il direct path senza STUN;
- se invece la porta e solo locale o viene rimappata dal NAT, serve conoscere la
  porta pubblica reale o configurare port-forward/static NAT;
- se c'e CGNAT o NAT simmetrico non controllabile, probabilmente restera necessario
  il relay.
