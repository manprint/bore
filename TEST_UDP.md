# Test end-to-end — modalità UDP hole-punching (`udp` feature)

Guida per testare a mano il percorso diretto UDP/QUIC dei secret tunnel e il
fallback al relay. Segui gli scenari in ordine: il primo è un *smoke test* su una
sola macchina; gli ultimi richiedono due host (dietro NAT) per provare la vera
traversata.

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

Variabili d'ambiente equivalenti: `BORE_UDP`, `BORE_PREFER_UDP`, `BORE_STUN_SERVER`, `BORE_SECRET`, `BORE_TCP_SECRET_ID`, `BORE_SERVER`.

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

## 4. Checklist rapida

- [ ] S0 loopback: `using direct udp path` + curl OK
- [ ] S1 con secret: idem
- [ ] S2 server senza udp: fallback relay, curl OK
- [ ] S3 provider senza udp: fallback relay, curl OK
- [ ] S4 due NAT: diretto OK (o fallback se NAT simmetrico)
- [ ] S5 tcpdump: in diretto i dati NON passano dal server
- [ ] S6 trasferimento lungo: regge l'inattività
- [ ] S7 secret errato: rifiutato

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
- Se la connessione QUIC cade a metà sessione, non c'è re-fallback "a caldo" per le
  connessioni già aperte: cadono, le nuove ripartono (diretto o relay con
  `--auto-reconnect`).
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
