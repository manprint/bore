# Guida utente di `bore`

Questa guida è orientata a chi **usa** `bore`: spiega, in modo esaustivo e con
esempi pronti all'uso, come avviare il **server** in ogni sua modalità di deploy e
come collegarvi i **client** (`bore local`) e i **proxy** (`bore proxy`).

La struttura è **combinatoria**: per ogni modalità di avvio del server (semplice,
con autenticazione, HTTP, TLS/HTTPS, UDP, pagina admin, …) trovi sempre le stesse
sottosezioni — *Avvio dei client* e *Avvio dei proxy* — così da vedere come cambia
il collegamento al variare della configurazione del server.

> Per la teoria di NAT/firewall e hole-punching UDP, vedi
> [`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md). Per i dettagli di progetto, vedi
> [`README.md`](README.md) e [`CHANGELOG.md`](CHANGELOG.md).

---

## Indice

1. [Concetti di base](#1-concetti-di-base)
2. [Riferimento rapido dei comandi](#2-riferimento-rapido-dei-comandi)
3. [Come leggere questa guida](#3-come-leggere-questa-guida)
4. [Modalità di deploy del server](#4-modalità-di-deploy-del-server)
   - [4.1 Server semplice (TCP in chiaro, senza autenticazione)](#41-server-semplice-tcp-in-chiaro-senza-autenticazione)
   - [4.2 Server con autenticazione (`--secret`)](#42-server-con-autenticazione---secret)
   - [4.3 Server con dominio e indirizzamento HTTP (porta 80, in chiaro)](#43-server-con-dominio-e-indirizzamento-http-porta-80-in-chiaro)
   - [4.4 Server con TLS / HTTPS (certificato)](#44-server-con-tls--https-certificato)
   - [4.5 Server con path diretto UDP (`--udp`)](#45-server-con-path-diretto-udp---udp)
   - [4.6 Server con pagina di amministrazione (`--admin-token`)](#46-server-con-pagina-di-amministrazione---admin-token)
   - [4.7 Configurazione di rete avanzata](#47-configurazione-di-rete-avanzata)
   - [4.8 Deploy "production" completo (TLS + secret + UDP + admin)](#48-deploy-production-completo-tls--secret--udp--admin)
5. [Funzionalità trasversali](#5-funzionalità-trasversali)
6. [Scenari end-to-end completi](#6-scenari-end-to-end-completi)
7. [Risoluzione dei problemi](#7-risoluzione-dei-problemi)

---

## 1. Concetti di base

`bore` è un tunnel TCP minimale. Espone una porta locale (dietro NAT/firewall)
attraverso un server pubblico. Tre ruoli:

| Ruolo | Comando | Cosa fa |
|-------|---------|---------|
| **Server** | `bore server` | Gira su una macchina raggiungibile (VPS, cloud). Accetta i client e instrada il traffico. |
| **Client** | `bore local` | Gira sulla macchina che ospita il servizio da esporre. Apre il tunnel verso il server. |
| **Proxy** | `bore proxy` | Gira sulla macchina che vuole *consumare* un tunnel **segreto**. Espone il servizio remoto su una porta locale. |

### Tunnel pubblico vs tunnel segreto

- **Tunnel pubblico**: `bore local` chiede al server una **porta pubblica**. Chiunque
  raggiunga `server:porta` parla con il tuo servizio. Non serve `bore proxy`.
- **Tunnel segreto**: `bore local --tcp-secret-id <id>` si registra sul server con un
  **identificativo** invece di una porta pubblica. Il servizio è raggiungibile **solo**
  da chi avvia `bore proxy --tcp-secret-id <id>`. Nessuna porta pubblica viene aperta.
  - Un `id` ha **un solo provider** alla volta (una seconda registrazione dello stesso
    `id` viene rifiutata), ma può avere **più proxy contemporaneamente**: più utenti
    possono agganciarsi allo stesso `id` (sia in relay che in UDP diretto).

### La control port e lo schema di `--to`

Il server ascolta su una **control port** (default `7835`). Tutto il traffico
(controllo + dati) è multiplexato su quell'unica connessione.

Il valore di `--to` sul client/proxy determina **host, porta e se usare TLS**:

| `--to` | Schema | Porta di default | TLS |
|--------|--------|------------------|-----|
| `bore.tld` | nudo | **control port** (7835) | no |
| `bore.tld:9000` | nudo con porta | 9000 | no |
| `http://bore.tld` | http | 80 | no |
| `https://bore.tld` | https | 443 | **sì** |
| `https://bore.tld:7835` | https con porta | 7835 | **sì** |

> **Importante**: lo schema decide la porta di default. Se la tua control port **non**
> è 80/443, indicala esplicitamente: es. `https://bore.tld:7835`. In alternativa avvia
> il server direttamente su 443/80 (`--control-port 443`) o inoltra la porta (Docker:
> `443:7835`).

### Variabili d'ambiente

**Ogni** opzione che vedi come flag può essere passata anche via variabile
d'ambiente (comodo per Docker/systemd). Le trovi nelle tabelle del
[Riferimento rapido](#2-riferimento-rapido-dei-comandi). Esempio equivalente:

```shell
bore local 8080 --to https://bore.tld --secret hunter2
# equivale a:
BORE_SERVER=https://bore.tld BORE_SECRET=hunter2 bore local 8080
```

### Verbosità e log

- `-v` → log a livello `debug`, `-vv` → `trace`. `RUST_LOG` ha la precedenza.
- I log vanno su **stderr**; i colori ANSI sono attivi solo su terminale (output
  pulito sotto Docker/journald/redirezioni).

### Arresto pulito

Server, client e proxy gestiscono `Ctrl-C` e `SIGTERM` (es. `docker stop`,
systemd): chiudono in modo ordinato con una riga di log, senza troncare i trasferimenti.

---

## 2. Riferimento rapido dei comandi

### `bore server`

| Flag | Env | Default | Descrizione |
|------|-----|---------|-------------|
| `--min-port <PORT>` | `BORE_MIN_PORT` | 1024 | Porta minima assegnabile ai tunnel pubblici. |
| `--max-port <PORT>` | `BORE_MAX_PORT` | 65535 | Porta massima assegnabile. |
| `-s, --secret <SECRET>` | `BORE_SECRET` | — | Richiede autenticazione ai client/proxy. |
| `--max-conns <N>` | `BORE_MAX_CONNS` | 1024 | Connessioni proxate concorrenti per client. |
| `--control-port <PORT>` | `BORE_CONTROL_PORT` | 7835 | Porta di controllo. |
| `--bind-domain <DOMAIN>` | `BORE_BIND_DOMAIN` | — | Dominio pubblico annunciato ai client. |
| `--cert-file <PATH>` | `BORE_CERT_FILE` | — | Certificato TLS (PEM). Con `--key-file` ⇒ HTTPS. |
| `--key-file <PATH>` | `BORE_KEY_FILE` | — | Chiave privata TLS (PEM). |
| `--bind-addr <IP>` | — | 0.0.0.0 | IP su cui ascolta la control port. |
| `--bind-tunnels <IP>` | — | = `--bind-addr` | IP su cui ascoltano le porte dei tunnel. |
| `--udp` | `BORE_UDP` | off | Abilita i path diretti UDP + responder STUN. |
| `--max-carriers <N>` | `BORE_MAX_CARRIERS` | 16 | Cap sui carrier paralleli per tunnel (1 = pool disabilitato). |
| `--admin-token <TOKEN>` | `BORE_ADMIN_TOKEN` | — | Abilita la pagina admin (min 32 caratteri). |
| `-v`, `-vv` | — | — | Verbosità log. |

### `bore local` (client)

| Flag | Env | Descrizione |
|------|-----|-------------|
| `<PORT>` | `BORE_LOCAL_PORT` | Porta locale da esporre (argomento posizionale). |
| `-l, --local-host <HOST>` | — | Host locale da esporre (default `localhost`). |
| `-t, --to <ADDR>` | `BORE_SERVER` | Indirizzo del server (vedi schema `--to`). |
| `-p, --port <PORT>` | — | Porta pubblica desiderata (0 = assegnata dal server). |
| `-s, --secret <SECRET>` | `BORE_SECRET` | Secret di autenticazione. |
| `--tcp-secret-id <ID>` | `BORE_TCP_SECRET_ID` | Registra come **tunnel segreto** (ignora `--port`). |
| `--insecure` | `BORE_INSECURE` | Accetta certificati self-signed (per `https://`). |
| `--https` | `BORE_HTTPS` | Termina TLS sulla porta del tunnel (server con cert). |
| `--force-https` | `BORE_FORCE_HTTPS` | Reindirizza HTTP→HTTPS sul tunnel (richiede `--https`). |
| `--udp` | `BORE_PREFER_UDP` | Preferisci il path diretto UDP (solo tunnel segreti). |
| `--stun-server <HOST:PORT>` | `BORE_STUN_SERVER` | Override STUN per il path diretto (default: Cloudflare, Google, poi server bore). |
| `--upnp` | `BORE_UPNP` | Mappa una porta sul router via UPnP-IGD. |
| `--try-port-prediction` | `BORE_TRY_PORT_PREDICTION` | Annuncia porte predette per NAT simmetrici. |
| `--nat-udp-preferred-port <PORT>` | `BORE_NAT_UDP_PORT` | Porta UDP fissa per il punch (0 = casuale). |
| `--max-conns <N>` | `BORE_MAX_CONNS` | Cap connessioni concorrenti sul path diretto. |
| `--basic-auth <USER:PASS>` | `BORE_BASIC_AUTH` | Protegge il tunnel con HTTP Basic auth. |
| `--notes <TEXT>` | `BORE_NOTES` | Nota mostrata nella pagina admin del server. |
| `--carriers <N>` | `BORE_CARRIERS` | Connessioni TCP parallele per la tratta relay (tunnel pubblico server→client o provider server→provider; default 1). |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | Riconnessione automatica con backoff. |

### `bore proxy` (consumatore di tunnel segreto)

| Flag | Env | Descrizione |
|------|-----|-------------|
| `--local-proxy-port <ADDR>` | `BORE_LOCAL_PROXY_PORT` | Indirizzo locale su cui esporre (`:5555` = tutte le interfacce). |
| `-t, --to <ADDR>` | `BORE_SERVER` | Indirizzo del server. |
| `-s, --secret <SECRET>` | `BORE_SECRET` | Secret di autenticazione. |
| `--tcp-secret-id <ID>` | `BORE_TCP_SECRET_ID` | Id del tunnel segreto (deve combaciare col provider). |
| `--insecure` | `BORE_INSECURE` | Accetta certificati self-signed. |
| `--udp` | `BORE_PREFER_UDP` | Preferisci il path diretto UDP. |
| `--stun-server <HOST:PORT>` | `BORE_STUN_SERVER` | Override STUN per il path diretto (default: Cloudflare, Google, poi server bore). |
| `--upnp` | `BORE_UPNP` | Mappa una porta sul router via UPnP-IGD. |
| `--try-port-prediction` | `BORE_TRY_PORT_PREDICTION` | Porte predette per NAT simmetrici. |
| `--nat-udp-preferred-port <PORT>` | `BORE_NAT_UDP_PORT` | Porta UDP fissa per il punch. |
| `--notes <TEXT>` | `BORE_NOTES` | Nota mostrata nella pagina admin. |
| `--carriers <N>` | `BORE_CARRIERS` | Connessioni TCP parallele per la tratta relay consumer→server (default 1). |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | Riconnessione automatica con backoff. |

### `bore test-udp` (diagnostica NAT/UDP — non espone tunnel)

| Flag | Env | Descrizione |
|------|-----|-------------|
| `-t, --to <ADDR>` | `BORE_SERVER` | Testa anche lo STUN del tuo server bore; richiesto per la modalità a due peer. |
| `-s, --secret <SECRET>` | `BORE_SECRET` | Secret del server e token del path diretto nella modalità a due peer. |
| `--tcp-secret-id <ID>` | `BORE_TCP_SECRET_ID` | Abbina due istanze `test-udp` con lo stesso id e coordina il test A<->B. |
| `--insecure` | `BORE_INSECURE` | Accetta certificati self-signed per `https://`. |
| `--stun-server <HOST:PORT>` | `BORE_STUN_SERVER` | STUN extra in diagnostica standalone; override della chain live in modalità paired. |
| `--upnp` | `BORE_UPNP` | Aggiunge un candidato UPnP-IGD nella modalità a due peer. |
| `--try-port-prediction` | `BORE_TRY_PORT_PREDICTION` | Aggiunge porte predette per NAT simmetrici nella modalità a due peer. |
| `--nat-udp-preferred-port <PORT>` | `BORE_NAT_UDP_PORT` | Testa esattamente quella porta UDP. |
| `--test-bandwidth` | — | Misura anche banda e latenza bidirezionali su UDP diretto e TCP relay (`--test-bandwith` è accettato come alias). |
| `--test-transfer-quota <SIZE>` | — | Quota per direzione/per path (`500MB`, `1GiB`, byte raw; default `64MB`). |

---

## 3. Come leggere questa guida

La [Sezione 4](#4-modalità-di-deploy-del-server) elenca le modalità di deploy del
server. **Per ognuna** trovi:

- **Avvio del server** — il comando lato server.
- **Avvio dei client** (`bore local`) — sia il **tunnel pubblico** sia il
  **provider segreto**.
- **Avvio dei proxy** (`bore proxy`) — il consumatore del tunnel segreto.

Quello che cambia tra una modalità e l'altra è soprattutto **come si scrive `--to`**
e quali flag servono (`--secret`, `--insecure`, …). Le funzionalità che valgono
in *tutte* le modalità (Basic auth, note, auto-reconnect, log, Docker) sono raccolte
nella [Sezione 5](#5-funzionalità-trasversali).

In tutti gli esempi:

- Il **server** è `bore.tld` (sostituisci col tuo host/IP).
- Il **servizio da esporre** è un'app web locale su `8080`.

---

## 4. Modalità di deploy del server

### 4.1 Server semplice (TCP in chiaro, senza autenticazione)

La configurazione minima: nessun secret, nessun TLS, control port `7835`. Adatta a
reti fidate o a test. Chiunque conosca host e porta può usare il server.

#### Avvio del server

```shell
bore server
```

Apri sul firewall del server: la **control port** `7835/tcp` e l'**intervallo di
porte dei tunnel** (default `1024-65535`, restringibile con `--min-port`/`--max-port`).

#### Avvio dei client (`bore local`)

**Tunnel pubblico — esposizione base** (il server assegna una porta libera):

```shell
bore local 8080 --to bore.tld
# -> in output trovi la porta pubblica assegnata, es. bore.tld:38271
```

**Tunnel pubblico — porta pubblica fissa** (deve rientrare in `--min-port..--max-port`):

```shell
bore local 8080 --to bore.tld --port 9000
# -> bore.tld:9000
```

**Esporre un host diverso da localhost** (es. un altro nodo della LAN):

```shell
bore local 8080 --local-host 192.168.1.50 --to bore.tld --port 9000
```

**Con riconnessione automatica** (consigliata per servizi a lunga vita):

```shell
bore local 8080 --to bore.tld --port 9000 --auto-reconnect
```

**Provider di tunnel segreto** (nessuna porta pubblica; si raggiunge solo via proxy):

```shell
bore local 8080 --to bore.tld --tcp-secret-id my-web
```

#### Avvio dei proxy (`bore proxy`)

Sulla macchina che vuole usare il servizio segreto `my-web`:

```shell
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555
# Ora http://<questa-macchina>:5555 raggiunge l'app remota.
```

Per ascoltare **solo** su loopback (nessun altro sulla LAN può collegarsi):

```shell
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port 127.0.0.1:5555
```

**Più proxy sullo stesso id** (più utenti, anche su macchine diverse): basta
lanciare lo stesso comando su ogni macchina. Sulla stessa macchina, usa porte locali
diverse:

```shell
# utente 1
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555
# utente 2 (stessa macchina)
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5556
```

---

### 4.2 Server con autenticazione (`--secret`)

Identica alla 4.1, ma il server richiede un **secret condiviso**: ogni client e
proxy deve passare lo **stesso** `-s/--secret`, altrimenti la connessione è rifiutata.
È il modo più semplice per impedire l'uso non autorizzato del server.

#### Avvio del server

```shell
bore server --secret hunter2
```

#### Avvio dei client (`bore local`)

**Tunnel pubblico:**

```shell
bore local 8080 --to bore.tld --port 9000 --secret hunter2
```

**Provider segreto:**

```shell
bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2
```

#### Avvio dei proxy (`bore proxy`)

```shell
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2
```

> Suggerimento: usa `BORE_SECRET` invece di `--secret` per non lasciare il secret
> nella cronologia della shell o nella lista dei processi.

```shell
export BORE_SECRET=hunter2
bore local 8080 --to bore.tld --port 9000
```

---

### 4.3 Server con dominio e indirizzamento HTTP (porta 80, in chiaro)

Quando vuoi che i client usino indirizzi puliti tipo `http://bore.tld`. Il traffico
resta **in chiaro** (nessun TLS): è solo una questione di indirizzamento. Lo schema
`http://` implica la **porta 80**, quindi la control port deve essere raggiungibile
su 80 (avvia il server con `--control-port 80`, oppure inoltra `80 -> 7835`).

#### Avvio del server

```shell
# Il server ascolta direttamente sulla 80 (richiede privilegi sulla porta < 1024).
bore server --bind-domain bore.tld --control-port 80
```

oppure mantenendo la 7835 e inoltrando la 80 (es. con Docker `ports: ["80:7835"]`,
vedi i compose in `docker/`).

`--bind-domain` è informativo: viene usato, ad esempio, per costruire i redirect
HTTP→HTTPS sui tunnel `--force-https`.

#### Avvio dei client (`bore local`)

**Tunnel pubblico:**

```shell
bore local 8080 --to http://bore.tld --port 9000
# -> http://bore.tld:9000
```

**Provider segreto:**

```shell
bore local 8080 --to http://bore.tld --tcp-secret-id my-web
```

#### Avvio dei proxy (`bore proxy`)

```shell
bore proxy --to http://bore.tld --tcp-secret-id my-web --local-proxy-port :5555
```

> Se invece tieni la control port su 7835 (senza inoltro), indirizza esplicitamente
> la porta: `--to bore.tld:7835` (forma nuda, in chiaro).

---

### 4.4 Server con TLS / HTTPS (certificato)

La connessione di controllo (e quindi tutto il traffico multiplexato) viaggia **cifrata**
con TLS. È la configurazione consigliata su Internet. Lo schema `https://` implica la
**porta 443**: avvia il server su 443 (`--control-port 443`) o inoltra `443 -> 7835`,
altrimenti indica la porta esplicitamente (`https://bore.tld:7835`).

#### Avvio del server

```shell
bore server \
  --bind-domain bore.tld \
  --control-port 443 \
  --cert-file /etc/bore/cert.pem \
  --key-file  /etc/bore/key.pem \
  --secret hunter2
```

`--cert-file` e `--key-file` vanno forniti **insieme**. Lo stesso certificato è
riutilizzato per la terminazione TLS opzionale sulle porte dei tunnel (`--https`).

#### Avvio dei client (`bore local`)

**Tunnel pubblico (controllo su TLS):**

```shell
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2
```

**Terminazione TLS sulla porta del tunnel** (`--https`): il servizio diventa
raggiungibile via `https://` sulla porta pubblica, mentre `http://` e TCP grezzo
continuano a funzionare sulla stessa porta.

```shell
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --https
# -> https://bore.tld:9000   (TLS terminato dal server)
# -> http://bore.tld:9000    (in chiaro)
# -> bore.tld:9000           (TCP grezzo)
```

**Forzare HTTPS** (redirect 308 da HTTP a HTTPS; richiede `--https`):

```shell
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --https --force-https
# http://bore.tld:9000 -> 308 -> https://bore.tld:9000
```

**Provider segreto su controllo TLS:**

```shell
bore local 8080 --to https://bore.tld --tcp-secret-id my-web --secret hunter2
```

**Certificato self-signed**: aggiungi `--insecure` su client e proxy.

```shell
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --insecure
```

#### Avvio dei proxy (`bore proxy`)

```shell
bore proxy --to https://bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2
# con certificato self-signed:
bore proxy --to https://bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2 --insecure
```

> Nota: `--https`/`--force-https` riguardano la **porta del tunnel pubblico** e
> richiedono che il server abbia un certificato. Non hanno effetto sui tunnel segreti
> (lì il traffico è già interno alla connessione cifrata).

---

### 4.5 Server con path diretto UDP (`--udp`)

Per i **tunnel segreti**, `bore` può stabilire un percorso **diretto peer-to-peer**
tra provider e proxy via UDP hole-punching (carrier QUIC), usando il server solo come
rendezvous/STUN. Se il path diretto non si stabilisce, si **ricade automaticamente sul
relay**: `--udp` non rompe mai un tunnel. Vale **solo** per i tunnel segreti (il
percorso pubblico non è hole-punchabile).

Serve `--udp` su **tutti e tre**: server, provider (`bore local`) e proxy (`bore proxy`).

#### Avvio del server

```shell
bore server --secret hunter2 --udp
```

Apri **anche** la control port in **UDP** (`7835/udp`) se vuoi usare il responder
STUN self-hosted come fallback finale. Di default i peer provano prima STUN
pubblici comuni (`stun.cloudflare.com:3478`, poi Google), poi il server bore. Con
Docker, aggiungi il forward `7835:7835/udp` per mantenere disponibile il fallback
del server.

#### Avvio dei client (`bore local`) — provider

```shell
bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2 --udp
```

Opzioni utili per NAT difficili (vedi [`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md)):

```shell
# STUN esplicito (override della chain Cloudflare -> Google -> server):
bore local 8080 --to bore.tld --tcp-secret-id my-web --udp --stun-server stun.cloudflare.com:3478

# Porta UDP fissa (apri quella in uscita sul firewall; stesso valore sui due peer):
bore local 8080 --to bore.tld --tcp-secret-id my-web --udp --nat-udp-preferred-port 41641

# Router domestico restrittivo: mappa una porta via UPnP-IGD:
bore local 8080 --to bore.tld --tcp-secret-id my-web --udp --upnp

# NAT simmetrico sequenziale: prova la predizione di porta (best-effort):
bore local 8080 --to bore.tld --tcp-secret-id my-web --udp --try-port-prediction
```

#### Avvio dei proxy (`bore proxy`) — consumatore

```shell
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2 --udp
```

Le stesse opzioni NAT (`--stun-server`, `--nat-udp-preferred-port`, `--upnp`,
`--try-port-prediction`) sono disponibili anche qui.

#### Diagnostica (`bore test-udp`)

Prima di indagare un fallimento del path diretto, esegui la diagnostica su **entrambi**
i peer. Classifica il NAT e indica cosa è raggiungibile:

```shell
# Solo STUN pubblici:
bore test-udp

# Includi lo STUN del tuo server bore e testa una porta UDP fissa:
bore test-udp --to bore.tld --nat-udp-preferred-port 41641
```

Per testare davvero **entrambi i lati insieme**, usa lo stesso comando su A e B
con lo stesso `--tcp-secret-id`. La prima istanza resta in attesa, la seconda fa
partire il test coordinato dal server:

```shell
# Macchina A
bore test-udp --to https://bore.tld --secret hunter2 --tcp-secret-id svc

# Macchina B, stesso id e stesso secret
bore test-udp --to https://bore.tld --secret hunter2 --tcp-secret-id svc
```

Il report indica NAT locale e peer, candidate UDP, esito del direct QUIC,
fallback TCP relay e latenza in entrambe le direzioni. Con la banda:

```shell
bore test-udp --to https://bore.tld --secret hunter2 --tcp-secret-id svc \
  --test-bandwidth --test-transfer-quota 500MB
```

La quota è per direzione e per path: con `500MB` vengono trasferiti 500 MB A->B e
500 MB B->A su UDP diretto, poi lo stesso sul TCP relay fallback.

Interpretazione delle prestazioni: il diretto UDP/QUIC dovrebbe spesso ridurre la
latenza e togliere il server dal data path, ma non è garantito che superi sempre il
fallback TCP in throughput single-stream. QUIC è un trasporto affidabile e
congestion-controlled in user space; il relay usa TCP del kernel, molto ottimizzato,
e il server può essere vicino a uno dei peer. Il direct path usa finestre QUIC
high-throughput definite in `src/holepunch.rs` (`DIRECT_QUIC_STREAM_RECEIVE_WINDOW`,
`DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW`, `DIRECT_QUIC_SEND_WINDOW`): sono i primi
valori da ritoccare se test reali su link high-BDP mostrano stalli di flow-control.
Bore inoltre richiede `DIRECT_UDP_SOCKET_RECV_BUFFER` e
`DIRECT_UDP_SOCKET_SEND_BUFFER` da 16 MiB, usa `quinn::congestion::BbrConfig`, tiene
`MAX_DIRECT_STREAMS` a 4096 e usa keep-alive/idle QUIC di 3 s / 10 s, cosi il
comportamento di base non dipende dai default piu piccoli del sistema.

> Regola d'oro: il **provider** è il lato che deve essere *raggiungibile* (fa da
> server QUIC); il **proxy** è il lato che *contatta*. Se il provider è dietro un NAT
> simmetrico/CGNAT il path diretto può non riuscire e si resta sul relay.

---

### 4.6 Server con pagina di amministrazione (`--admin-token`)

Abilita una **dashboard di sola lettura** su `/admin/status`, servita sulla control
port con lo stesso schema del server (`http`/`https`). Mostra in tempo reale tutti i
tunnel collegati. Senza `--admin-token` la pagina è disabilitata (e la control port
parla solo il protocollo bore).

Il token deve essere lungo **almeno 32 caratteri**.

#### Avvio del server

```shell
# Genera un token robusto e abilita la pagina.
bore server --secret hunter2 --admin-token "$(openssl rand -hex 24)"
```

La pagina è poi raggiungibile su:

- `http://bore.tld:7835/admin/status` (server in chiaro su 7835)
- `http://bore.tld/admin/status` (control port su 80)
- `https://bore.tld/admin/status` (server TLS su 443)

Cambiando la control port, l'URL segue: `http://bore.tld:9000/admin/status`, ecc.

Apri l'URL nel browser, incolla il token nel form: la tabella si popola e si aggiorna
da sola (polling ogni ~2 secondi). Mostra, per ogni tunnel: tipo (pubblico /
provider / consumer), porta o id, indirizzo del client, opzioni (TLS, basic auth,
UDP), le **note** (`--notes`), il numero di connessioni attive e l'uptime. Per i
tunnel segreti vedi **sia** il provider **sia** tutti i `bore proxy` agganciati.

Lo stato è **volatile** (nessuna persistenza): riflette solo ciò che è collegato in
quel momento; aggiunte, rimozioni e riconnessioni si aggiornano automaticamente.

#### Avvio dei client (`bore local`) — con note

Le note non cambiano il comportamento: servono a **identificare** il tunnel nella
pagina admin.

```shell
bore local 8080 --to bore.tld --port 9000 --secret hunter2 --notes "web di staging - macchina A"
# provider segreto annotato:
bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2 --notes "DB interno"
```

#### Avvio dei proxy (`bore proxy`) — con note

```shell
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2 --notes "consumer su laptop di Mario"
```

> La pagina admin si serve sulla control port: tienila raggiungibile **solo** da chi
> di dovere (firewall/VPN) e usala preferibilmente su un server TLS, così token e
> dati viaggiano cifrati.

---

### 4.7 Configurazione di rete avanzata

Queste opzioni del server si combinano con qualunque modalità vista sopra.

**Control port personalizzata** — i client devono indicare la porta in `--to`:

```shell
bore server --control-port 9000
bore local 8080 --to bore.tld:9000              # forma nuda con porta
bore local 8080 --to https://bore.tld:9000      # con TLS
```

**Restringere l'intervallo delle porte dei tunnel** (apri esattamente queste sul firewall):

```shell
bore server --min-port 20000 --max-port 20100
bore local 8080 --to bore.tld --port 20001
```

**Interfacce separate per controllo e tunnel** — utile se vuoi il controllo su una
rete privata e i tunnel su una pubblica (o viceversa):

```shell
bore server --bind-addr 10.0.0.5 --bind-tunnels 0.0.0.0
```

**Limite di connessioni concorrenti** (per client, contro flood):

```shell
bore server --max-conns 256
```

Lo stesso `--max-conns` su `bore local` (provider segreto) limita le connessioni
servite sul **path diretto UDP** (analogo lato provider del cap del relay):

```shell
bore local 8080 --to bore.tld --tcp-secret-id my-web --udp --max-conns 64
```

---

### 4.8 Deploy "production" completo (TLS + secret + UDP + admin)

Tutte le funzionalità insieme: connessione cifrata, autenticazione, path diretto UDP
e dashboard amministrativa.

#### Avvio del server

```shell
bore server \
  --bind-domain bore.tld \
  --control-port 443 \
  --cert-file /etc/bore/cert.pem \
  --key-file  /etc/bore/key.pem \
  --secret "$BORE_SECRET" \
  --udp \
  --admin-token "$BORE_ADMIN_TOKEN" \
  --min-port 20000 --max-port 20100
```

Firewall del server: `443/tcp` (controllo + STUN su 443? no — STUN è sulla control
port in **UDP**), quindi apri `443/tcp` **e** `443/udp` (responder STUN sulla control
port), più `20000-20100/tcp` (tunnel pubblici, se ne usi).

#### Avvio dei client (`bore local`)

**Provider segreto, cifrato, con UDP e nota:**

```shell
bore local 8080 \
  --to https://bore.tld \
  --tcp-secret-id my-web \
  --secret "$BORE_SECRET" \
  --udp \
  --notes "app interna - nodo A" \
  --auto-reconnect
```

**Tunnel pubblico HTTPS con Basic auth e nota:**

```shell
bore local 8080 \
  --to https://bore.tld \
  --port 20001 \
  --secret "$BORE_SECRET" \
  --https --force-https \
  --basic-auth "admin:$WEB_PASS" \
  --notes "dashboard pubblica protetta" \
  --auto-reconnect
```

#### Avvio dei proxy (`bore proxy`)

```shell
bore proxy \
  --to https://bore.tld \
  --tcp-secret-id my-web \
  --local-proxy-port 127.0.0.1:5555 \
  --secret "$BORE_SECRET" \
  --udp \
  --notes "consumer ufficio" \
  --auto-reconnect
```

Poi apri `https://bore.tld/admin/status` per vedere provider e proxy collegati, con
le rispettive note e statistiche.

---

## 5. Funzionalità trasversali

Valgono in tutte le modalità della Sezione 4.

### 5.1 Basic auth (`--basic-auth "user:pass"`)

Protegge un tunnel con HTTP **Basic auth**: le richieste HTTP senza credenziali valide
ricevono `401`. È **solo per traffico HTTP**: il traffico non-HTTP (TCP grezzo) viene
inoltrato **senza** protezione.

- **Tunnel pubblico**: l'autenticazione è imposta dal **server**. Le credenziali
  viaggiano nel canale di controllo, quindi usa un server **TLS** per non esporle.
- **Tunnel segreto**: l'autenticazione è imposta dal **provider** (`bore local`) e
  copre sia il relay sia il path diretto UDP. Le credenziali **non lasciano** il provider.

```shell
# Pubblico (sul server TLS):
bore local 8080 --to https://bore.tld --port 9000 --https --basic-auth "admin:s3cr3t"

# Segreto (imposto dal provider):
bore local 8080 --to https://bore.tld --tcp-secret-id my-web --basic-auth "admin:s3cr3t"
```

Il flag sta su `bore local` (non su `bore proxy`): a decidere l'autenticazione è chi
espone il servizio.

### 5.2 Note (`--notes`)

Etichetta libera mostrata nella [pagina admin](#46-server-con-pagina-di-amministrazione---admin-token).
Disponibile su `bore local` e `bore proxy`. Senza effetti sul traffico. Viene troncata
a 256 caratteri.

### 5.3 Carrier paralleli (`--carriers`)

Di default un tunnel fa passare **tutte** le connessioni proxate su **una sola**
connessione TCP (multiplexing yamux). Sotto perdita di pacchetti questo causa
*head-of-line blocking* tra connessioni (la perdita di un flusso blocca tutti gli
altri che condividono quella TCP) e un unico controllo di congestione per tutti.

`--carriers N` apre **N connessioni TCP parallele** e distribuisce le connessioni
proxate su di esse (round-robin): la perdita su un carrier blocca solo ~1/N dei
flussi e ogni carrier ha la propria finestra di congestione.

Si applica a **tutte le tratte relay** (il server è sempre nel data path del relay):

```shell
# Tunnel pubblico (tratta server→client)
bore local 8080 --to bore.tld --port 9000 --secret hunter2 --carriers 4
# Provider segreto (tratta server→provider, condivisa da tutti i consumer)
bore local 8080 --to bore.tld --tcp-secret-id app --secret hunter2 --carriers 4
# Consumer segreto (tratta consumer→server)
bore proxy --to bore.tld --tcp-secret-id app --secret hunter2 --local-proxy-port :5555 --carriers 4
```

Quando conviene:

- **Conviene** con tanti flussi concorrenti: upload/download paralleli con `rclone`,
  S3/WebDAV, browser (molte richieste), streaming — soprattutto su un link verso il
  server con perdita o alta latenza.
- **Non cambia nulla** per un singolo trasferimento bulk (un flusso = un carrier).
  Per il caso flusso-singolo su link con perdita/alta-BDP, agisci sull'**host**:
  `sysctl net.ipv4.tcp_congestion_control=bbr` (bore non imposta il congestion
  control per-socket — richiederebbe codice `unsafe`).
- Il server resta **sempre** nel data path del relay: questo **non** aggiunge banda
  né lo bypassa — rimuove solo il collo di bottiglia della singola TCP sulla tratta.

Il server limita `N` al proprio `--max-carriers` (default 16) per pubblici e provider;
una richiesta più grande viene troncata, e `--max-carriers 1` disabilita il pool. Un
carrier che cade viene ri-aperto automaticamente: il tunnel non si interrompe mai.
Default `1` = comportamento invariato.

> **Il path diretto UDP non usa `--carriers`.** Quando un tunnel segreto gira su path
> diretto (`--udp`), ogni connessione proxata viaggia già su una **stream QUIC
> nativa** indipendente (niente HOL). `--carriers` ottimizza il **relay**; `--udp`
> ottimizza il **diretto**. Si combinano: il pool relay serve da fallback.

Il path diretto QUIC usa finestre interne più ampie dei default Quinn:
`DIRECT_QUIC_STREAM_RECEIVE_WINDOW` = 16 MiB,
`DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` = 64 MiB, `DIRECT_QUIC_SEND_WINDOW` =
64 MiB. Richiede anche `DIRECT_UDP_SOCKET_RECV_BUFFER` e
`DIRECT_UDP_SOCKET_SEND_BUFFER` da 16 MiB, usa `quinn::congestion::BbrConfig`,
`MAX_DIRECT_STREAMS` = 4096 e keep-alive/idle 3 s / 10 s. Sono costanti in
`src/holepunch.rs`, non flag CLI: alzale solo dopo misure
`test-udp --test-bandwidth` e tenendo conto della memoria peggiore per
connessione/stream.

### 5.4 Parametri server/host per throughput

Sul **relay TCP**, i parametri più importanti sono fuori dal protocollo:

- abilita BBR/fq sull'host o nel namespace che ospita il server, se supportato:
  `sysctl -w net.core.default_qdisc=fq` e
  `sysctl -w net.ipv4.tcp_congestion_control=bbr`;
- alza i buffer massimi se fai relay ad alta banda/RTT:
  `net.core.rmem_max`, `net.core.wmem_max`, `net.ipv4.tcp_rmem`,
  `net.ipv4.tcp_wmem`;
- imposta `--max-conns` in base a CPU/RAM/file descriptor e aumenta `nofile`
  (`ulimit -n`) se ospiti molti tunnel;
- alza `--max-carriers` solo se i client usano davvero `--carriers N`; più carrier
  significano più socket e più lavoro sul server;
- per STUN/UDP affidabile in Docker, preferisci `network_mode: host` o verifica che
  `7835/udp` esponga il vero source address dei client.

Questi parametri migliorano il relay/fallback. Il direct UDP, quando riesce, bypassa
il server per i dati: il server resta solo rendezvous/STUN/control.

### 5.5 Riconnessione automatica (`--auto-reconnect`)

Su `bore local` e `bore proxy`. Se la connessione cade o non si stabilisce, il client
riprova da solo con backoff esponenziale (1, 2, 4, 8, 16, 32 s, poi ogni 32 s); una
connessione riuscita azzera il backoff. Per un tunnel segreto su UDP, dopo un riavvio
del provider il proxy si riconnette e rinegozia (di nuovo diretto, o relay).

### 5.6 Log e verbosità

```shell
bore local 8080 --to bore.tld -v       # debug
bore server --udp -vv                   # trace
RUST_LOG=warn bore server               # filtro esplicito
```

### 5.7 Variabili d'ambiente / Docker

Ogni flag ha un equivalente `BORE_*` (vedi [Riferimento rapido](#2-riferimento-rapido-dei-comandi)).
Nella cartella `docker/` trovi compose pronti per server, client e secret-proxy con
tutte le variabili documentate. Esempio server via env:

```shell
BORE_SECRET=hunter2 BORE_UDP=true BORE_ADMIN_TOKEN="$(openssl rand -hex 24)" bore server
```

---

## 6. Scenari end-to-end completi

### 6.1 Esporre un'app web in chiaro (tunnel pubblico)

```shell
# Server (VPS)
bore server --secret hunter2

# Macchina con l'app su :8080
bore local 8080 --to bore.tld --port 9000 --secret hunter2 --auto-reconnect

# Chiunque: http://bore.tld:9000
```

### 6.2 Servizio privato condiviso con pochi utenti (tunnel segreto, niente porte pubbliche)

```shell
# Server
bore server --secret hunter2

# Provider (macchina con il servizio su :8080)
bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2 --notes "app interna" --auto-reconnect

# Ogni utente (anche più di uno, in contemporanea)
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2
# -> il servizio è su http://localhost:5555 della macchina dell'utente
```

### 6.3 Tunnel segreto cifrato, diretto P2P, con dashboard

```shell
# Server (443, TLS, UDP, admin)
bore server --bind-domain bore.tld --control-port 443 \
  --cert-file /etc/bore/cert.pem --key-file /etc/bore/key.pem \
  --secret hunter2 --udp --admin-token "$(openssl rand -hex 24)"

# Provider
bore local 8080 --to https://bore.tld --tcp-secret-id my-web \
  --secret hunter2 --udp --notes "nodo A" --auto-reconnect

# Proxy
bore proxy --to https://bore.tld --tcp-secret-id my-web \
  --local-proxy-port :5555 --secret hunter2 --udp --notes "ufficio" --auto-reconnect

# Amministrazione: https://bore.tld/admin/status
```

### 6.4 Esporre un servizio non-HTTP (es. SSH) — tunnel pubblico TCP grezzo

```shell
# Server
bore server --secret hunter2

# Provider (SSH locale su :22)
bore local 22 --to bore.tld --port 22000 --secret hunter2 --auto-reconnect

# Client SSH
ssh -p 22000 utente@bore.tld
```

> Per i servizi non-HTTP, `--basic-auth` non protegge nulla (passa inalterato): usa
> `--secret` lato server e/o un tunnel **segreto** per limitarne l'accesso.

### 6.5 Tunnel pubblico ad alta concorrenza (download/upload paralleli, web, streaming)

Per workload con molte connessioni simultanee — `rclone` con upload/download
paralleli, S3/WebDAV, browser, streaming — usa il **pool di carrier** per evitare
l'head-of-line blocking della singola TCP (vedi [§5.3](#5-funzionalità-trasversali)).

```shell
# Server (alza il cap se vuoi pool più larghi)
bore server --secret hunter2 --max-carriers 16

# Client: 8 connessioni TCP parallele per i dati
bore local 8080 --to bore.tld --port 9000 --secret hunter2 --carriers 8 --auto-reconnect

# Chiunque: http://bore.tld:9000 — i flussi vengono distribuiti sugli 8 carrier
```

> Per un **singolo** trasferimento bulk il pool non cambia nulla (un flusso = un
> carrier); su link con perdita imposta `bbr` sull'host:
> `sysctl -w net.ipv4.tcp_congestion_control=bbr`.

Stessa cosa per un **tunnel segreto** ad alta concorrenza — metti `--carriers` su
provider e consumer (e, se vuoi, `--udp` per il path diretto P2P che già usa stream
QUIC indipendenti):

```shell
bore server --secret hunter2 --udp --max-carriers 16
bore local 8080 --to bore.tld --tcp-secret-id app --secret hunter2 --carriers 8 --udp --auto-reconnect
bore proxy --to bore.tld --tcp-secret-id app --secret hunter2 --local-proxy-port :5555 --carriers 8 --udp --auto-reconnect
```

---

## 7. Risoluzione dei problemi

| Sintomo | Causa probabile | Rimedio |
|---------|-----------------|---------|
| `connection closed before authentication — wrong --to scheme?` | `--to host:port` (in chiaro) verso un server TLS | Usa `https://host[:port]`. |
| Il client si connette ma il tunnel non risponde dall'esterno | Porta del tunnel non aperta sul firewall del server | Apri l'intervallo `--min-port..--max-port` (o la `--port` scelta). |
| `https://bore.tld` non si collega | La control port non è 443 | Avvia il server su `--control-port 443`, inoltra `443->7835`, o usa `https://bore.tld:7835`. |
| Errore certificato su self-signed | Cert non fidato | Aggiungi `--insecure` su client/proxy. |
| Il path diretto UDP non parte, resta sempre relay | NAT/firewall, STUN non raggiungibile, `--udp` mancante su un lato | Esegui `bore test-udp` su entrambi; assicura `--udp` su server, provider e proxy e apri `control-port/udp`. Vedi [`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md). |
| `/admin/status` non risponde | `--admin-token` non impostato o < 32 caratteri, o porta/schema sbagliati | Imposta un token ≥ 32 char; usa lo schema corretto (`http`/`https`) e la giusta control port. |
| Token admin non accettato dalla pagina | Token errato | Reinserisci il valore esatto passato a `--admin-token`. |
| `--carriers N` non sembra aprire N connessioni | Server con `--max-carriers` più basso (vale per pubblici e provider) | Alza `--max-carriers` sul server. Il consumer (`bore proxy`) apre le sue da sé, non è limitato dal server. |
| `--carriers` non migliora un singolo download | Un flusso usa un solo carrier (il pool aiuta la concorrenza) | Imposta `bbr` sull'host (`sysctl net.ipv4.tcp_congestion_control=bbr`). |
| `tcp-secret-id '<id>' already in use` | Esiste già un provider con quell'id | Un id ha un solo provider: scegli un id diverso o chiudi il provider esistente (i **proxy** multipli sono invece consentiti). |
| Le credenziali Basic auth passano in chiaro | Tunnel/controllo non cifrato | Usa un server TLS e `--https` sul tunnel pubblico. |
| Connessioni rifiutate sotto carico | Raggiunto `--max-conns` | Aumenta `--max-conns` (server e/o provider). |

---

*Per la teoria di rete (NAT, firewall, hole-punching) consulta
[`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md). Per le note di rilascio,
[`CHANGELOG.md`](CHANGELOG.md).*
