# Test end-to-end — modalità UDP hole-punching (`udp` feature)

Guida per testare a mano il percorso diretto UDP/QUIC dei secret tunnel e il
fallback al relay. Segui gli scenari in ordine: il primo è un *smoke test* su una
sola macchina; gli ultimi richiedono due host (dietro NAT) per provare la vera
traversata.

> Per la **teoria** (come funziona l'hole-punch) e la **matrice completa
> provider×consumer** di NAT/firewall con i rimedi da amministratore, vedi
> [`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md).

---

## 0. Build

La modalità UDP è **inclusa di default**. Build normale:

```shell
cargo build --release
```

Binario: `./target/release/bore`. Per comodità, in ogni terminale:

```shell
BORE=./target/release/bore
```

> Per escluderla (es. un target dove `quinn` non compila): `cargo build
> --release --no-default-features`. In quel caso `--udp` viene ignorato con un
> warning.

---

## 1. Ruoli e flag

Tre processi:

| Ruolo | Comando | Note |
|-------|---------|------|
| **server** (rendezvous + STUN) | `bore server --udp` | apre TCP **e** UDP sulla control port (7835) |
| **provider** (espone il servizio locale) | `bore local <PORT> --tcp-secret-id <ID> --to <SRV> --udp` | `bore local` con `--tcp-secret-id` = provider |
| **consumer/proxy** (usa il servizio) | `bore proxy --local-proxy-port <ADDR> --tcp-secret-id <ID> --to <SRV> --udp` | espone il servizio del provider su una porta locale |

Flag rilevanti:

- `--udp` — attiva la modalità diretta (su tutti e tre i processi).
- `--stun-server host:port` — STUN esterno; default = host di `--to` sulla control port (il server stesso fa da STUN).
- `--secret S` — se il server ha `--secret`, provider e consumer devono passare lo stesso. Il token del path diretto è derivato dal secret.
- `--tcp-secret-id ID` — deve combaciare tra provider e consumer.
- `--upnp` (su `local` e `proxy`) — prova ad aprire una porta sul **router casalingo** via UPnP-IGD e la aggiunge come candidato. Aiuta router casalinghi strict con IP WAN pubblico; **inutile dietro CGNAT**. Log: `UPnP-IGD port mapping ENABLED`.
- `--try-port-prediction` (su `local` e `proxy`) — per NAT **simmetrici**: annuncia qualche porta oltre quella reflexive. **Opt-in**, best-effort, **può sembrare un port scan** ai firewall strict. Log: `port prediction ENABLED`.
- `test-udp --tcp-secret-id ID` — modalità diagnostica **a due peer**: due host lanciano lo stesso comando, il server li abbina, prova UDP diretto e TCP relay, e stampa un report A<->B.
- `test-udp --test-bandwidth --test-transfer-quota 500MB` — aggiunge banda e latenza bidirezionali su entrambi i path. `--test-bandwith` (senza la seconda `d`) è accettato come alias.

Variabili d'ambiente equivalenti: `BORE_UDP`, `BORE_PREFER_UDP`, `BORE_STUN_SERVER`, `BORE_SECRET`, `BORE_TCP_SECRET_ID`, `BORE_SERVER`, `BORE_UPNP`, `BORE_TRY_PORT_PREDICTION`.

> **Schema TLS vs plain.** Se il server ha un certificato (control in TLS), `--to`
> deve usare `https://` — anche con porta esplicita: `--to https://host:7835`.
> La forma bare `host:7835` è **plain TCP** e contro un server TLS fallisce con un
> errore fuorviante (`connection closed before authentication — wrong --to
> scheme?`). Nei log del server vedi `TLS handshake failed`.
>
> **STUN default.** Per un `--to https://host` (porta 443) lo STUN punta
> automaticamente alla **control port `7835`/UDP** (lì sta il responder), non alla
> 443. Quindi basta aprire UDP 7835 in ingresso; `--stun-server` serve solo per
> deployment non standard.

---

## 2. Come leggere i log

Attiva i log (default sono quasi muti):

```shell
export RUST_LOG=bore_cli=debug,bore=info
```

Frasi chiave da cercare:

**Server**
- `STUN responder listening` → UDP bindato OK.
- `provider offered udp candidates` → il provider ha mandato i candidati.
- `brokered udp direct path` → il server ha messo in contatto i due peer.
- `no udp-capable provider; consumer will use relay` → il provider non è in modalità udp → relay.

**Provider** (`bore local … --udp`)
- `registered secret tunnel`
- `discovered reflexive address` (debug) → STUN ha risposto.
- `direct udp path ready, accepting connections` → QUIC server su.

**Consumer** (`bore proxy … --udp`)
- `discovered reflexive address` (debug)
- ✅ `using direct udp path` → **percorso diretto attivo**.
- ↩️ `udp unavailable, using relay` oppure `udp negotiation failed, using relay` → **fallback al relay** (il tunnel funziona lo stesso).

Il segnale principale "diretto vs relay" è la riga del **consumer**.

> **Su singola macchina usa `127.0.0.1`, non `localhost`.** `localhost` può
> risolvere prima a IPv6 (`::1`): lo STUN responder del server è su IPv4, quindi
> la scoperta reflexive fallirebbe e vedresti un fallback inatteso. Con
> `127.0.0.1` lo STUN di default punta a `127.0.0.1:7835`.

---

## 3. Scenari

### S0 — Smoke test su una sola macchina (loopback)

Conferma che negoziazione + handshake QUIC + round trip funzionano. Su loopback
STUN restituisce `127.0.0.1` e il "punch" è un no-op, ma tutto il resto è reale.

Servizio locale di prova (terminale 1):
```shell
python3 -m http.server 8000
```

Server (terminale 2):
```shell
RUST_LOG=bore_cli=debug ./target/release/bore server --udp
```

Provider (terminale 3):
```shell
RUST_LOG=bore_cli=debug ./target/release/bore local 8000 \
  --tcp-secret-id svc --to 127.0.0.1 --udp
```

Consumer (terminale 4):
```shell
RUST_LOG=bore_cli=debug ./target/release/bore proxy \
  --local-proxy-port :5555 --tcp-secret-id svc --to 127.0.0.1 --udp
```

Test (terminale 5):
```shell
curl -s http://127.0.0.1:5555/ | head
```

**Atteso:** la pagina dell'indice di `http.server`. Nel log del consumer:
`using direct udp path`.

---

### S1 — Diretto con secret (auth del path)

Come S0 ma con secret condiviso (verifica il token derivato da `secret`+nonce).

```shell
# server
./target/release/bore server --udp --secret hunter2
# provider
./target/release/bore local 8000 --tcp-secret-id svc --to 127.0.0.1 --udp --secret hunter2
# consumer
./target/release/bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to 127.0.0.1 --udp --secret hunter2
```
**Atteso:** `using direct udp path`, `curl` OK.

---

### S2 — Fallback: server senza `--udp`

```shell
./target/release/bore server                 # NIENTE --udp (no STUN responder)
./target/release/bore local 8000 --tcp-secret-id svc --to 127.0.0.1 --udp
./target/release/bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to 127.0.0.1 --udp
```
**Atteso:** il consumer non riesce a fare STUN (nessun responder), va in
`udp unavailable, using relay` o `udp negotiation failed, using relay`. `curl`
**funziona comunque** (relay). Nota: la negoziazione può impiegare ~qualche
secondo (timeout STUN) prima del fallback.

---

### S3 — Fallback: provider senza `--udp`

```shell
./target/release/bore server --udp
./target/release/bore local 8000 --tcp-secret-id svc --to 127.0.0.1          # provider SENZA --udp
./target/release/bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to 127.0.0.1 --udp
```
**Atteso:** server logga `no udp-capable provider; consumer will use relay`; il
consumer logga il fallback; `curl` funziona (relay).

---

### S4 — Due macchine dietro NAT (il test vero)

Questo prova la traversata reale. Servono:
- **SRV**: host pubblico raggiungibile (VPS), con **TCP 7835 e UDP 7835** aperti nel firewall.
- **A** (provider) e **B** (consumer): due reti/NAT diversi (es. due case, o uno in 4G).

Su **SRV**:
```shell
RUST_LOG=bore_cli=info ./bore server --udp --secret hunter2
```
(assicurati che il firewall/security-group permetta **sia** TCP **sia** UDP su 7835)

Su **A** (espone, es., un SSH o http locale sulla 8000):
```shell
RUST_LOG=bore_cli=debug ./bore local 8000 --tcp-secret-id svc --to SRV_IP --udp --secret hunter2
```

Su **B**:
```shell
RUST_LOG=bore_cli=debug ./bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to SRV_IP --udp --secret hunter2
curl -s http://127.0.0.1:5555/ | head     # oppure: ssh -p 5555 user@127.0.0.1
```

**Atteso:** su B `using direct udp path` e il servizio di A risponde. Se uno dei
due NAT è simmetrico, vedrai il fallback al relay (atteso, non è un bug — vedi §5).

---

### S5 — Prova che i dati NON passano dal server (bypass del relay)

Distingue in modo oggettivo diretto da relay osservando il traffico sulla control
port del server durante un trasferimento **grande**.

1. Su A esponi un file grande, es.:
   ```shell
   dd if=/dev/urandom of=/tmp/big.bin bs=1M count=200
   cd /tmp && python3 -m http.server 8000
   ```
2. Avvia server/provider/consumer come in S1 o S4.
3. Sul **server**, durante il download, conta i byte sulla control port:
   ```shell
   sudo tcpdump -ni any 'tcp port 7835 or udp port 7835'
   ```
4. Su B scarica:
   ```shell
   curl -s http://127.0.0.1:5555/big.bin -o /dev/null
   ```

**Atteso (diretto):** sul server vedi solo pacchetti piccoli (heartbeat ogni
500 ms + STUN iniziale), **non** i 200 MB. **In relay** (S2/S3) invece i 200 MB
attraversano la TCP 7835. È la prova netta del bypass.

> Variante "kill server": stabilita la sessione diretta e avviato un download
> lungo, **fermare il server** non deve interrompere il trasferimento già in
> corso (il path QUIC è peer-to-peer). Nuove connessioni invece smettono di
> partire (il control channel è giù). Timing-sensitive: usalo come conferma, non
> come test rigido.

---

### S6 — Stabilità trasferimento lungo/quieto

Verifica che keepalive QUIC tenga viva una connessione lunga e silenziosa
(equivalente diretto del keepalive TCP).

```shell
# su A: servizio che resta aperto e silenzioso poi parla (es. cat)
ncat -lk 9000 --sh-exec 'sleep 30; cat'      # oppure un servizio reale
# provider espone 9000, consumer su :5555
# su B:
ncat 127.0.0.1 5555      # scrivi qualcosa, attendi >30s, deve restare vivo
```
**Atteso:** la connessione non cade durante l'inattività; i dati arrivano.

---

### S7 — Secret sbagliato

```shell
./bore server --udp --secret right
./bore local 8000 --tcp-secret-id svc --to 127.0.0.1 --udp --secret right
./bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to 127.0.0.1 --udp --secret wrong
```
**Atteso:** il consumer viene rifiutato già all'handshake di controllo (prima
ancora dell'UDP): errore tipo `server requires authentication` / `server error`.

---

### S8 — NAT difficili (UPnP + port prediction)

Da provare su **due reti reali** (come S4). Aggiungi i flag su `local` E `proxy`:

```shell
# provider e consumer, su reti diverse:
./bore local 8000 --tcp-secret-id svc --to https://SRV --udp --secret S --upnp --try-port-prediction
./bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to https://SRV --udp --secret S --upnp --try-port-prediction
```

**Atteso nei log** (`RUST_LOG=bore_cli=info`):
- con `--upnp` su un router casalingo con UPnP attivo e IP WAN pubblico:
  `UPnP-IGD port mapping ENABLED — added router-mapped candidate`. Dietro CGNAT
  mobile: nessun effetto (il candidato mappato è comunque privato).
- con `--try-port-prediction`: `port prediction ENABLED — advertising predicted
  symmetric-NAT ports`. **Best-effort**: aiuta solo NAT simmetrici sequenziali;
  può non funzionare e può apparire come uno scan a firewall strict.

Se nemmeno questi bastano (es. CGNAT su entrambi i lati) → **relay** (atteso).

**Porta UDP fissa (`--nat-udp-preferred-port`).** Dietro un firewall che filtra
l'**uscita** per porta: apri una porta UDP in egress su entrambi gli host, usa lo
**stesso** valore sui due peer e passa il flag — il direct usa esattamente quella
porta (su NAT port-preserving fissa anche il mapping pubblico). `41641` = default
Tailscale, scelta sensata. Non aiuta i NAT simmetrici. Verifica prima con
`bore test-udp --nat-udp-preferred-port 41641` (la riga `Local UDP socket` deve
mostrare `:41641 (fixed ...)` e gli STUN devono rispondere).

```shell
./bore local 8000 --tcp-secret-id svc --to https://SRV --udp --secret S --nat-udp-preferred-port 41641
./bore proxy --local-proxy-port :5555 --tcp-secret-id svc --to https://SRV --udp --secret S --nat-udp-preferred-port 41641
```

### S9 — Diagnostica NAT/UDP (`bore test-udp`)

**Prima** di sospettare il tunnel, capisci cosa permette la tua rete. `bore
test-udp` non apre tunnel: sonda STUN pubblici (e, con `--to`, lo STUN del *tuo*
server), classifica il NAT e dà consigli. Lancialo su **entrambi** i peer.

```shell
./bore test-udp                                  # solo STUN pubblici
./bore test-udp --to https://SRV                 # testa anche l'UDP del tuo server
./bore test-udp --stun-server stun.l.google.com:19302   # STUN esplicito extra
```

**Come leggere il verdetto:**
- `STUN probes` tutti `[FAIL]` → **UDP egress bloccato**: solo relay possibile.
- `CONE NAT` → bucabile dal tuo lato; se il diretto fallisce comunque, il blocco
  è il **peer** (symmetric/CGNAT/UDP-bloccato dall'altra parte).
- `SYMMETRIC NAT` → diretto solo se l'altro peer è cone/open; riporta se le porte
  sono **SEQUENTIAL** (allora `--try-port-prediction` ha una chance) o **RANDOM**.
- `PUBLIC IP / no NAT` → provider ideale.
- `CGNAT detected` (`100.64/10`) → P2P improbabile, relay affidabile.
- **Nota hairpin**: se STUN pubblico funziona ma l'UDP del *tuo* server NO →
  provider co-locato col server / UDP non aperto lato server. Lancia il provider
  da una rete diversa, o passa `--stun-server <pubblico>`.
- `UPnP-IGD router: FOUND` → `--upnp` può mappare una porta sul router.

Esempio reale (consumer dietro NAT cone, port-preserving):
```
Verdict
-------
CONE NAT (endpoint-independent mapping): same public port to every server.
Port preservation: YES (local 41991 == public 41991).
```

### S10 — Diagnostica coordinata A<->B (`test-udp --tcp-secret-id`)

Questa modalità verifica davvero **entrambi i lati** senza avviare un tunnel di
servizio. Il server deve essere avviato con `--udp` e raggiungibile su TCP e UDP
della control port. Le due macchine usano lo stesso id; la prima resta in attesa,
la seconda fa partire il pairing.

Su **SRV**:
```shell
./bore server --udp --secret hunter2
```

Su **A**:
```shell
./bore test-udp --to https://SRV --secret hunter2 --tcp-secret-id svc
```

Su **B**:
```shell
./bore test-udp --to https://SRV --secret hunter2 --tcp-secret-id svc
```

**Cosa viene testato:** diagnosi NAT locale, sintesi del NAT del peer, candidate
UDP, hole-punch QUIC diretto, fallback TCP relay, latenza in entrambe le
direzioni. Se il diretto fallisce ma il relay TCP passa, il report lo dichiara e
consiglia il fallback.

Con misure di banda:
```shell
./bore test-udp --to https://SRV --secret hunter2 --tcp-secret-id svc \
  --test-bandwidth --test-transfer-quota 500MB
```

La quota è **per direzione e per path**: `500MB` significa A->B e B->A su UDP
diretto, poi A->B e B->A sul TCP relay. Usa una quota piccola (`16MB`, `64MB`) per
smoke test, più alta per misure realistiche.

Come leggere i risultati di banda: UDP diretto più lento del TCP relay non è, da
solo, un fallimento. Il test misura QUIC affidabile/congestion-controlled sopra UDP,
non UDP raw; il relay TCP può beneficiare del kernel, di BBR/offload e di un server
molto vicino a uno dei peer. Considera il diretto sano quando `UDP direct: working`,
la latenza è coerente e i trasferimenti completano in entrambe le direzioni. Per
tuning bulk, i primi knob sono le costanti `DIRECT_QUIC_*_WINDOW` e
`DIRECT_UDP_SOCKET_*_BUFFER` in `src/holepunch.rs`; il direct path usa gia BBR.

---

## 4. Checklist rapida

- [ ] S0 loopback: `using direct udp path` + curl OK
- [ ] S1 con secret: idem
- [ ] S2 server senza udp: fallback relay, curl OK
- [ ] S3 provider senza udp: fallback relay, curl OK
- [ ] S4 due NAT: diretto OK (o fallback se NAT simmetrico)
- [ ] S5 tcpdump: in diretto i dati NON passano dal server
- [ ] S6 trasferimento lungo: regge l'inattività
- [ ] S7 secret errato: rifiutato
- [ ] S8 NAT difficili: `--upnp` / `--try-port-prediction` loggano l'attivazione
- [ ] S9 `bore test-udp` su entrambi i peer: verdetto NAT coerente + consigli
- [ ] S10 `test-udp --tcp-secret-id`: pairing A<->B, UDP diretto o fallback TCP, report bidirezionale
- [ ] S10 con `--test-bandwidth`: banda/latenza misurate su UDP e TCP

---

## 5. Limiti noti v1 (NON sono bug)

- Hole-punching **solo** per secret tunnel (`--tcp-secret-id`); la modalità a
  porta pubblica (`bore local 8000 --to … -p 1234`) non è interessata.
- **NAT simmetrico** su entrambi i lati → la traversata fallisce → fallback relay.
  Atteso.
- **Consumer multipli / riconnessione: supportati.** Il provider tiene un listener
  QUIC persistente e ri-buca verso ogni nuovo consumer; nonce stabile per-provider
  → tutti i consumer derivano lo stesso token. Un `bore proxy` che si riconnette
  (o un secondo proxy sullo stesso id) ritorna sul path diretto.
- **Resilienza alle cadute (usa `--auto-reconnect` su client e proxy):**
  - **Provider cade/riavvia:** il consumer rileva la morte del path diretto e si
    riconnette → ri-negozia (diretto se il provider è tornato, altrimenti relay).
    Rilevamento immediato su chiusura pulita, entro ~10s (idle timeout QUIC) su
    kill brutale.
  - **Upgrade relay→diretto automatico:** se il consumer è finito in relay
    (provider non raggiungibile al momento), ogni ~10s ritenta il path diretto e,
    appena il provider è raggiungibile, **passa al diretto senza cadere**. Quindi
    converge sempre al diretto entro ~10s, non resta bloccato in relay.
  - **Consumer cade/riavvia:** il provider ri-buca verso il nuovo consumer →
    torna diretto.
  - **Server cade:** entrambi perdono il canale di controllo → riconnessione con
    backoff finché il server torna, poi ri-negoziazione.
- Le connessioni TCP locali **già aperte** quando un peer cade vengono interrotte
  (non c'è migrazione a caldo); le nuove ripartono sul path ristabilito.
- `--stun-server` accetta IPv4; i candidati raccolti sono IPv4.

---

## 6. Se qualcosa non va — cosa mandarmi

Per ogni scenario che fallisce, incolla:

1. I comandi esatti usati (i 3 processi).
2. I log dei tre processi con `RUST_LOG=bore_cli=debug,bore=debug` (almeno dalla
   connessione al fallimento).
3. L'errore lato client (`curl -v`, o messaggio di `bore`).
4. Diagnostica utile:
   ```shell
   # il server ascolta in UDP?
   sudo ss -lunp | grep 7835
   # il server ascolta in TCP?
   sudo ss -ltnp | grep 7835
   # firewall (lato SRV) permette UDP 7835?
   ```
5. Per S4/S5: tipo di NAT se lo conosci, e l'estratto di `tcpdump`.

Domande tipiche da segnalare: il consumer dice `using direct udp path` ma il
`curl` non risponde? il fallback non scatta e resta appeso? il server non logga
`STUN responder listening`?
