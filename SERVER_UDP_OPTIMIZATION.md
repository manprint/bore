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
- apri la stessa control port in UDP per STUN (`7835/udp` o la tua control port);
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
