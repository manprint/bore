# Server UDP / relay optimization

Questa guida raccoglie le leve lato server per migliorare affidabilita STUN/UDP,
throughput del relay TCP e capacita di gestire molti tunnel. Il punto chiave e
distinguere i due data path:

- **UDP direct path**: dopo il rendezvous, i dati viaggiano peer-to-peer tra
  provider e consumer. Il server resta nel percorso solo per controllo, STUN e
  brokering dei candidati. Ottimizzare il server non aumenta la banda del path
  diretto, ma puo rendere piu affidabile la scoperta STUN e il pairing.
- **TCP relay fallback / tunnel pubblici**: i dati attraversano il server. Qui
  contano CPU, socket, file descriptor, congestion control TCP, carrier paralleli
  e buffering del kernel.

Usa `bore test-udp --tcp-secret-id <id> --test-bandwidth` per misurare entrambi i
path nella coppia reale. Se UDP direct risulta `working`, ma il TCP relay e piu
veloce in throughput single-stream, non e automaticamente un guasto: QUIC e
affidabile/congestion-controlled in user space, mentre TCP usa il kernel e puo
beneficiare di BBR/offload/topologia favorevole del server.

Il binario prova gia a comportarsi bene senza tuning puntuale sui peer: il socket
UDP del direct path richiede buffer send/receive da 16 MiB e QUIC usa BBR come
congestion controller. I sysctl di questa guida servono a rimuovere cap del kernel
o a migliorare il relay TCP/server, non sono prerequisiti per ogni client.
I valori possono anche essere sovrascritti al bootstrap del server con i flag
`--udp-*` omologhi o le env var `BORE_UDP_*`; il server li brokera ai peer del
path diretto e alla diagnostica `test-udp` paired.

## Profilo direct UDP applicato nel codice

Questi valori sono definiti in `src/shared.rs` e usati da `src/holepunch.rs`; valgono
per `bore local --udp`, `bore proxy --udp` e `bore test-udp --tcp-secret-id`:

| Parametro | Valore | Effetto |
|---|---:|---|
| `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` | 16 MiB | Flow-control per singola stream QUIC. |
| `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` | 64 MiB | Flow-control aggregato per connessione QUIC. |
| `DIRECT_QUIC_SEND_WINDOW` | 64 MiB | Byte inviabili senza ACK prima di bloccare il sender. |
| `DIRECT_UDP_SOCKET_RECV_BUFFER` | 16 MiB | Buffer UDP richiesto al kernel in ricezione. |
| `DIRECT_UDP_SOCKET_SEND_BUFFER` | 16 MiB | Buffer UDP richiesto al kernel in invio. |
| `quinn::congestion::BbrConfig` | BBR | Congestion controller QUIC del path diretto. |
| `MAX_DIRECT_STREAMS` | 4096 | Numero massimo di bidi-stream QUIC concorrenti. |
| `QUIC_KEEPALIVE` / `QUIC_MAX_IDLE` | 3 s / 10 s | Keep-alive NAT e rilevamento peer morto. |

### Override lato server

I valori nella tabella sopra sono i default. `bore server` puo sovrascriverli con
questi flag/env:

| Flag / env | Parametro |
|---|---|
| `--udp-stream-receive-window` / `BORE_UDP_STREAM_RECEIVE_WINDOW` | `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` |
| `--udp-connection-receive-window` / `BORE_UDP_CONNECTION_RECEIVE_WINDOW` | `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` |
| `--udp-send-window` / `BORE_UDP_SEND_WINDOW` | `DIRECT_QUIC_SEND_WINDOW` |
| `--udp-socket-recv-buffer` / `BORE_UDP_SOCKET_RECV_BUFFER` | `DIRECT_UDP_SOCKET_RECV_BUFFER` |
| `--udp-socket-send-buffer` / `BORE_UDP_SOCKET_SEND_BUFFER` | `DIRECT_UDP_SOCKET_SEND_BUFFER` |
| `--udp-max-streams` / `BORE_UDP_MAX_STREAMS` | `MAX_DIRECT_STREAMS` |

Con `-v` i log mostrano i buffer UDP effettivi concessi dal kernel (`actual_recv`,
`actual_send`). Se sono molto inferiori a 16 MiB, alza `rmem_max`/`wmem_max`
sull'host; bore ha gia richiesto il valore corretto.

## TUNING UDP

Se vuoi piu banda sul path diretto, la regola non e "alzare tutto": serve
abbastanza finestra da coprire il BDP del percorso, ma non molto di piu.

In prima approssimazione:

- `BDP = banda desiderata * RTT`
- se la finestra e sotto il BDP, il sender si ferma prima di saturare la linea
- se la finestra e molto sopra il BDP, la banda non cresce, ma crescono memoria
  e lavoro di buffering

I riferimenti esterni sono coerenti su questo punto: RFC 9000 dice che il
throughput viene limitato quando il credit di flow control e inferiore al BDP,
RFC 9002 insiste sul pacing e sulle raffiche troppo grandi, e la doc di Quinn
nota che le finestre andrebbero dimensionate rispetto a latenza, throughput e
memoria disponibile.

### Cosa fa davvero ogni leva

| Parametro | Se lo alzi | Se lo abbassi | Memoria | CPU | Nota critica |
|---|---|---|---|---|---|
| `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` | migliora il singolo flusso che sta saturando la sua stream | blocca prima i writer su quella stream | cresce per stream attiva | quasi neutra, salvo piu reassembly e wakeup | e il primo knob quando un singolo stream non riempie il link |
| `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` | permette a piu stream di sommare piu bytes in flight | limita il throughput aggregato della connessione | cresce per connessione | neutra o leggermente peggiore se produce burst | serve quando una stream non basta o ci sono piu flussi concorrenti |
| `DIRECT_QUIC_SEND_WINDOW` | il sender puo tenere piu dati in volo prima dell'ACK | il sender si auto-limita anche se il peer offre credito | cresce sul lato che trasmette | quasi neutra, ma piu buffering lato sender | se e troppo basso, il peer resta con credito ma il sender non lo usa |
| `DIRECT_UDP_SOCKET_RECV_BUFFER` | assorbe burst UDP e riduce drop kernel | aumenta il rischio di drop o riordino persi | cresce per socket | puo scendere perche ci sono meno drop, ma cresce la pressione di memoria | e il primo knob se vedi `actual_recv` molto sotto il richiesto |
| `DIRECT_UDP_SOCKET_SEND_BUFFER` | evita che il sender resti corto di coda nel kernel | limita il burst in uscita | cresce per socket | di solito neutra; troppo alto puo peggiorare la burstiness | utile su path con jitter o scheduling irregolare |
| `MAX_DIRECT_STREAMS` | aumenta la concorrenza per connessione | blocca l'apertura di nuove stream | cresce con il numero di stream attive | cresce il bookkeeping e la scheduler pressure | non aumenta la banda di un singolo flusso; serve solo per tante connessioni |
| `QUIC_KEEPALIVE` / `QUIC_MAX_IDLE` | piu liveness, meno NAT drop | meno traffico di mantenimento, ma piu rischio di idle timeout | cambia poco la memoria | piu keepalive = piu wakeup; meno keepalive = meno CPU | non e una leva di banda; cambia solo se hai problemi di NAT o di timeout |

Due punti sono facili da sbagliare:

- `MAX_DIRECT_STREAMS` non e una leva per un singolo bulk transfer. Se hai una
  sola stream attiva, alzarlo non aumenta la banda.
- le finestre QUIC e i buffer UDP non sono equivalenti: una finestra grande senza
  buffer kernel adeguati produce comunque drop o stalli, e buffer enormi senza
  finestre adeguate non aumentano la banda.

### Lettura pratica dei numeri

Le dimensioni attuali hanno senso come profilo di partenza per link internet
comuni. Per capire se sono abbastanza, conviene confrontarle con il BDP reale.

| Finestra | ~40 ms RTT | ~100 ms RTT |
|---|---:|---:|
| 16 MiB | ~3.3 Gbit/s | ~1.3 Gbit/s |
| 64 MiB | ~13.4 Gbit/s | ~5.4 Gbit/s |

Questa tabella non dice che il tunnel andra davvero a quelle velocita: dice che
quelle finestre smettono di essere il collo di bottiglia solo fino a quel livello.
Se il link e piu lento, la finestra non e il problema; se il link e piu veloce o
la RTT e piu alta, i valori di default diventano presto stretti.

### Perche i default non vanno sempre alzati

Alzare i valori aumenta il margine per la banda, ma il costo non e gratis:

- la memoria cresce quasi linearmente con il numero di connessioni e stream che
  usano davvero quel credito;
- il kernel puo dover trattenere piu pacchetti e piu buffering per socket;
- la CPU non cresce solo per il numero assoluto del buffer, ma cresce quando il
  traffico diventa piu bursty, quando i datagrammi sono piu frammentati o quando
  la reassembly queue si allunga;
- oltre il BDP utile, il guadagno di banda tende a fermarsi mentre la pressione
  di memoria continua a salire.

Quindi il tuning giusto non e "piu grande possibile", ma "abbastanza grande da
non limitare il path".

### Caso MAX_BANDWIDTH

Questo e il profilo per quando vuoi spremere il piu possibile il tunnel diretto
su un path stabile e con memoria disponibile.

Obiettivo:

- tenere la finestra sopra il BDP reale del percorso;
- evitare che una singola stream monopolizzi il credito;
- evitare drop UDP sul kernel;
- lasciare al congestion controller abbastanza spazio per lavorare senza
  strozzarlo con buffer troppo piccoli.

Profilo di partenza ragionevole:

| Parametro | Valore aggressivo | Perche |
|---|---:|---|
| `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` | 32-64 MiB | una singola stream puo riempire anche link ad alta latenza |
| `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` | 128-256 MiB | piu stream o una stream molto larga senza stallo aggregato |
| `DIRECT_QUIC_SEND_WINDOW` | 128-256 MiB | il sender non deve restare indietro rispetto al credit ricevuto |
| `DIRECT_UDP_SOCKET_RECV_BUFFER` | 32-64 MiB | meno drop kernel quando arrivano raffiche o burst di ACK/data |
| `DIRECT_UDP_SOCKET_SEND_BUFFER` | 32-64 MiB | meno blocchi del sender per coda kernel corta |
| `MAX_DIRECT_STREAMS` | lascia 4096, o scendi se hai poche connessioni | non e il primo collo di bottiglia della banda |

Se vuoi massimizzare davvero la banda, la sequenza giusta e questa:

1. Misura il RTT reale del path.
2. Stima il BDP e porta `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` sopra quel valore.
3. Alza `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` e `DIRECT_QUIC_SEND_WINDOW` in
   modo coerente, senza lasciare il sender sotto il receiver.
4. Alza i buffer UDP finche `actual_recv` e `actual_send` non sono vicini ai
   valori richiesti e i drop kernel restano nulli o marginali.
5. Solo dopo valuta se il numero di stream concorrenti richiede piu di 4096.

Il punto piu importante: se il tunnel finisce sul relay TCP invece che sul path
diretto, questo profilo non basta da solo. In quel caso entrano in gioco i
sysctl dell'host, `fq`/BBR e i carrier paralleli descritti piu sotto.

### Quando il profilo MAX_BANDWIDTH peggiora le cose

Il profilo aggressivo puo diventare controproducente se:

- la RTT e bassa e la banda richiesta non giustifica buffer cosi grandi;
- il peer o l'host hanno poca RAM e molte connessioni contemporanee;
- il path perde pacchetti o ha MTU instabile, perche piu credito significa piu
  dati che restano in giro quando si verifica perdita;
- il carico e fatto di tanti stream piccoli, perche il guadagno vero viene dal
  parallelismo, non dal gonfiare i buffer di ogni singola connessione.

In altre parole: il profilo MAX_BANDWIDTH e corretto solo quando il collo di
bottiglia e davvero la finestra, non la CPU dell'applicazione, la RAM del peer o
la qualita del path.

### Riferimenti utili

- [RFC 9000 - QUIC: A UDP-Based Multiplexed and Secure Transport](https://www.rfc-editor.org/rfc/rfc9000)
- [RFC 9002 - QUIC Loss Detection and Congestion Control](https://www.rfc-editor.org/rfc/rfc9002)
- [Quinn TransportConfig](https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html)
- [Linux ip-sysctl](https://docs.kernel.org/networking/ip-sysctl.html)

Se vuoi un ordine di tuning pratico: prima sistema il BDP con le finestre QUIC,
poi verifica i buffer UDP reali con i log `-v`, e solo alla fine tocca i limiti
di concorrenza o il fallback relay.

## Docker networking: bridge vs host

Il compose server usa di default una rete bridge con port forwarding esplicito:

```yaml
ports:
  - "7835:7835"        # control TCP
  - "7835:7835/udp"    # STUN UDP
  - "6000-7000:6000-7000"
```

### Bridge mode

Pro:

- isolamento Docker piu prevedibile;
- porte pubblicate esplicitamente;
- facile convivenza con altri container.

Contro:

- Docker NAT puo alterare o nascondere dettagli del source address;
- per STUN e hole punching, il server deve vedere il vero indirizzo sorgente UDP
  del client. Se tutti i client cadono sul relay anche con `BORE_UDP=true`, il
  bridge e uno dei primi sospetti;
- un piccolo overhead in piu sul relay TCP.

### Host network

Su Linux puoi usare `network_mode: host` per far ascoltare bore direttamente nel
namespace di rete dell'host.

Pro:

- STUN vede il source address reale con meno sorprese;
- niente port publishing Docker e meno NAT locale;
- spesso migliore per server UDP/rendezvous pubblici.

Contro:

- funziona davvero come host networking soprattutto su Linux;
- devi commentare `ports:` e `networks:` nel compose: con `network_mode: host` non
  si usano i mapping `7835:7835`;
- il container condivide la rete dell'host, quindi l'isolamento e minore;
- firewall/security group vanno gestiti sull'host o sul cloud provider.

Regola pratica: tieni il bridge se STUN e relay funzionano bene; passa a host
network se la diagnostica paired mostra STUN incoerente, source address sospetti o
fallback relay sistematico non spiegato dai NAT dei peer.

## `BORE_MAX_CONNS`

`BORE_MAX_CONNS` limita quante connessioni proxate possono essere attive sul
server per i path che attraversano il relay: tunnel pubblici e fallback/relay dei
tunnel secret. Quando il limite e raggiunto, nuove connessioni vengono rifiutate o
chiuse presto invece di saturare il processo.

Non aumenta la banda da solo: e un limite di capacita. Alzarlo ha senso solo se il
server ha CPU, RAM e file descriptor sufficienti.

Valori di partenza:

```text
1024   default conservativo, buono per piccoli VPS
4096   server medio con ulimit aumentato
8192+  server dedicato, dopo test di carico e osservabilita
```

Ogni connessione relay consuma piu file descriptor: socket esterno, stream/control
carrier, eventuali carrier extra, piu overhead del runtime. Come stima prudente,
dimensiona `nofile` ad almeno 4-8 volte `BORE_MAX_CONNS`, poi aggiungi margine per
tunnel, carrier, TLS e admin.

Nota: il provider `bore local --udp --max-conns` ha un limite analogo per le
connessioni servite sul **direct UDP path**, ma quello vive sulla macchina provider,
non sul server compose.

## `BORE_MAX_CARRIERS`

`BORE_MAX_CARRIERS` e il tetto imposto dal server alle connessioni TCP carrier
parallele richieste da un tunnel pubblico o da un provider secret con
`--carriers N`.

Aiuta quando il workload ha **molte connessioni concorrenti** e il relay TCP e il
collo di bottiglia: rclone parallelo, S3/WebDAV, browser, stream multipli. Non
migliora un singolo trasferimento bulk: un flusso usa un solo carrier.

Valori di partenza:

```text
1      disabilita il pool lato server
16     default ragionevole
32     server robusto, molti tunnel concorrenti
64+    solo se misurato: aumenta socket, memoria e lavoro scheduler
```

Il `bore proxy --carriers N` consumer apre le proprie connessioni verso il server;
questo non e clamped da `BORE_MAX_CARRIERS` nello stesso modo dei pool public/provider,
ma consuma comunque socket e file descriptor sul server.

`BORE_MAX_CARRIERS` non riguarda il direct UDP: quando il diretto riesce, i dati
non passano dal server e ogni connessione proxata usa una stream QUIC nativa.

## File descriptor e `nofile`

Per tanti tunnel, carrier e connessioni concorrenti, il limite file descriptor e
spesso il primo collo di bottiglia pratico. Nel compose puoi aggiungere:

```yaml
ulimits:
  nofile:
    soft: 1048576
    hard: 1048576
```

Implicazioni:

- il limite alto non forza bore a usare piu risorse, permette solo di farlo;
- il sistema host deve consentire quel limite (`/etc/security/limits.conf`, systemd,
  Docker daemon, distro/cloud image);
- con `BORE_MAX_CONNS` o `BORE_MAX_CARRIERS` alti, senza `nofile` adeguato vedrai
  errori di accept/connect o connessioni chiuse sotto carico.

## Sysctl host per relay TCP

Questi parametri contano per il **relay TCP** e i tunnel pubblici. Sono meglio
impostati sull'host, non nel compose, perche il supporto ai sysctl network dentro
container varia e con `network_mode: host` alcuni non sono namespaced.

Per il direct UDP, bore richiede autonomamente buffer UDP piu ampi sul socket. Se
il kernel limita `rmem_max`/`wmem_max`, il valore effettivo puo essere piu basso:
con `-v` i log mostrano i buffer richiesti e quelli concessi. In quel caso questi
sysctl diventano un modo per alzare il tetto del sistema, non una configurazione
manuale obbligatoria per far funzionare il tunnel.

Profilo di partenza per Linux moderno:

```shell
sudo sysctl -w net.core.default_qdisc=fq
sudo sysctl -w net.ipv4.tcp_congestion_control=bbr
sudo sysctl -w net.core.rmem_max=134217728
sudo sysctl -w net.core.wmem_max=134217728
sudo sysctl -w net.ipv4.tcp_rmem="4096 87380 134217728"
sudo sysctl -w net.ipv4.tcp_wmem="4096 65536 134217728"
```

Persistenza tipica:

```shell
cat >/etc/sysctl.d/99-bore-relay.conf <<'EOF'
net.core.default_qdisc=fq
net.ipv4.tcp_congestion_control=bbr
net.core.rmem_max=134217728
net.core.wmem_max=134217728
net.ipv4.tcp_rmem=4096 87380 134217728
net.ipv4.tcp_wmem=4096 65536 134217728
EOF
sudo sysctl --system
```

Verifica prima che BBR sia disponibile:

```shell
sysctl net.ipv4.tcp_available_congestion_control
sysctl net.ipv4.tcp_congestion_control
```

Se BBR non compare, serve un kernel/modulo che lo supporti. Su alcuni VPS basta
`modprobe tcp_bbr`; su altri non e disponibile.

## Porte e firewall

Per server UDP-enabled:

- apri la control port in TCP (`7835/tcp` o `443/tcp` se usi TLS su 443);
- apri la stessa control port in UDP per lo STUN self-hosted/fallback (`7835/udp`
  o la tua control port). I peer provano prima STUN pubblici comuni
  (`stun.cloudflare.com:3478`, poi Google) e usano quello del server come ultimo
  fallback;
- apri il range tunnel TCP (`BORE_MIN_PORT`-`BORE_MAX_PORT`) se usi tunnel
  pubblici;
- per Docker bridge, i mapping `ports:` devono corrispondere alle env
  `BORE_CONTROL_PORT`, `BORE_MIN_PORT`, `BORE_MAX_PORT`.

Il direct UDP data path tra provider e consumer non entra nel server, quindi non
devi aprire sul server porte UDP per i dati peer-to-peer oltre allo STUN/control.

## Procedura di tuning consigliata

1. Parti dal compose default e verifica:
   `bore test-udp --to https://server --secret ... --tcp-secret-id test`.
2. Se STUN/pairing e incoerente sotto Docker bridge, prova `network_mode: host`.
3. Se il relay TCP e collo di bottiglia, abilita BBR/fq e buffer host.
4. Se hai molte connessioni, aumenta `nofile`, poi `BORE_MAX_CONNS`.
5. Se hai molti flussi concorrenti per tunnel relay, usa `--carriers N` sui client
   e aumenta `BORE_MAX_CARRIERS` solo quanto serve.
6. Ripeti `test-udp --test-bandwidth` e un test applicativo reale: le metriche
   sintetiche aiutano, ma il traffico reale decide.
