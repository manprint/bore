# SSH Access Gateway per bore — Documento di analisi

> **Stato**: analisi di fattibilità — NESSUNA implementazione.
> **Data**: 2026-07-03 · branch `dev`
> **Scopo**: valutare un gateway SSH di ingresso per creare tunnel **public**, **secret** e
> **vhost** usando un normale client `ssh`/`autossh`, senza binario `bore` sul lato client.

---

## 0. Sintesi esecutiva

**Verdetto: fattibile**, con un'architettura precisa: un server SSH **embedded nel processo
`bore server`** (crate `russh`, feature-gated) che fa **solo da ingress**: interpreta le
richieste SSH standard (`tcpip-forward` per `-R`, `direct-tcpip` per `-L`) e le mappa sul
data path server-side **già esistente** (registry vhost, registry secret, bind delle porte
pubbliche, admin, weblog, `--max-conns`). Nessuna modifica al protocollo wire bore: il
gateway è puramente additivo.

Punti fermi:

| Domanda | Risposta breve |
|---|---|
| 1. Fattibile? | **Sì.** SSH sostituisce il tratto client↔server (canali SSH al posto dei substream yamux). Tutto il resto si riusa. |
| 2. Perdiamo funzionalità? | Sul tratto SSH: **sì** — niente `--udp`/QUIC direct, niente `--carriers>1`, niente hole-punch, niente `bore transfer`. Dettaglio in §2.2. |
| 3. Tutto su 443? | **Sì.** Demux a byte-peek su TCP 443 (SSH / TLS / HTTP / yamux) + QUIC su UDP 443. §2.3. |
| 4. UDP? | **No sul tratto SSH** — il protocollo SSH è TCP-only, limite strutturale non aggirabile con client OpenSSH stock. UDP/QUIC resta disponibile solo col client bore nativo (che convive sullo stesso 443). §2.4. |
| 5. Parametri via SSH? | Sì, 3 canali: stringa comando `exec`, variabili `SetEnv/SendEnv`, opzioni per-chiave nel file authorized_keys. Tabella completa flag→meccanismo in §3. |
| 6/11. Stabilità? | Sì: keepalive SSH bidirezionale (client `ServerAlive*`, server keepalive interno 20 s + reaper 60 s in parità con gli invarianti secret esistenti) + `autossh`/systemd + policy di takeover alla riconnessione. §2.6/§2.11. |
| 7. Comandi d'esempio coerenti? | **Due su quattro sono sintatticamente invalidi** per OpenSSH; correzioni in §2.7. |
| 8. Parametri SSH client? | Sì, tutti — sono lato client, il gateway deve solo rispondere ai keepalive. §2.8. |
| 9/10. Auth chiavi + password con hot-reload? | Sì per entrambe; il reload è "by construction" (lettura a ogni tentativo di auth). §2.9/§2.10. |

**Trade-off centrale (banda/latenza/stabilità)**: il tratto SSH è **una** connessione TCP con
finestre per-canale (~2 MiB su OpenSSH) e HOL-blocking tra canali. Per throughput massimo e
path diretto UDP il client bore nativo resta superiore. Il gateway SSH vale come **ingresso
universale a zero installazione** (qualunque macchina con `ssh`, incluse Windows/macOS/router),
non come sostituto del client nativo. I due mondi convivono sulla stessa porta.

**Prior art** che dimostra la fattibilità del modello: [sish](https://github.com/antoniomika/sish),
serveo.net, localhost.run, tuns.sh — tutti fanno esattamente "ssh -R come ingress di un tunnel
HTTP/TCP". Nessuno di questi però offre l'equivalente dei tunnel secret bore né la convivenza
con un data path QUIC nativo: è il valore aggiunto di questa proposta.

---

## 1. Architettura proposta

### 1.1 Vista d'insieme

```
                      TCP 443 (un solo socket)
                            │  peek 1° byte (timeout 2s)
        ┌───────────────────┼──────────────────────┬───────────────┐
        │ "SSH-"            │ 0x16 (TLS)           │ verbo HTTP    │ 0x00 (yamux)
        ▼                   ▼                      ▼               ▼
  ┌──────────┐      TLS accept (cert esistente)  vhost HTTP     bore control
  │ SSH  GW  │        │ peek di nuovo:           plain          plain (no TLS)
  │ (russh)  │        ├─ HTTP → vhost/admin
  │          │        ├─ yamux → bore control (path attuale)
  │ auth:    │        └─ "SSH-" → SSH-over-TLS (in v1, D-SSH4)
  │ · pubkey │
  │ · passwd │       UDP 443: endpoint QUIC esistente (vhost + public --udp, invariato)
  └────┬─────┘       UDP 7835: STUN responder (invariato)
       │  richieste SSH → mapping:
       │   tcpip-forward("", 9005)              → tunnel PUBLIC porta 9005
       │   tcpip-forward("mysub", 80)           → tunnel VHOST "mysub"
       │   tcpip-forward("tcp-secret-id", 0)    → SECRET provider "tcp-secret-id"
       │   direct-tcpip → ("tcp-secret-id", 0)  → SECRET consumer (per-connessione)
       ▼
  data path server ESISTENTE (registry, relay, admin, weblog, max-conns)
       │  al posto del substream yamux verso il client:
       └─ canale SSH `forwarded-tcpip` (per -R) / `direct-tcpip` (per -L)
```

### 1.2 Il punto di innesto: astrazione del "link verso il client"

Oggi il lato server parla col client attraverso `mux::Opener/Acceptor/Stream`
(`mux.rs:23` — `Stream = Compat<yamux::Stream>`); tutta la logica di relay/splice opera
già su `AsyncRead + AsyncWrite` generici (`copy_bidirectional_with_sizes`). L'innesto
richiede **una** astrazione nuova:

```
trait ClientLink {                      // nome indicativo
    async fn open_substream(&self) -> io::Result<Box<dyn Duplex>>;
}
impl per CarrierPool (yamux)  → path attuale, byte-identico, scrive STREAM_READY
impl per SshForward (russh)   → apre canale `forwarded-tcpip`; NON scrive STREAM_READY
```

Dettaglio critico: `mux::STREAM_READY` è protocollo bore-client (scritto dal server prima
dello splice, es. `secret.rs:735`, `vhost.rs relay_vhost`). Un client SSH **non lo capisce**:
il marker deve restare **dentro** l'implementazione yamux del link, mai sul canale SSH
(altrimenti 1 byte spurio — `mux.rs:35`, `STREAM_READY: u8 = 0` — in testa a ogni
connessione proxata). Questo è il refactoring più
delicato dell'intera feature — piccolo ma trasversale (`serve_tunnel`, `secret::relay`,
`vhost::relay_vhost`).

Punti di innesto concreti già individuati:
- accept/demux: `server.rs:983-1008` (accept loop) + `route_connection` `server.rs:1055-1085`
  (il byte-peek HTTP-vs-bore esiste già, con `Prefixed` in `prefixed.rs` per il replay del
  byte letto — si estende con i rami SSH e TLS);
- public: bind con `create_listener` (`server.rs:1011`), pattern accept-loop di `serve_tunnel`;
- vhost: registrazione in `VhostRegistry` (`vhost.rs:576`), routing Host già server-side
  (`vhost.rs:160,1168,1236`) → funziona identico per provider SSH;
- secret: `secret::serve_provider` (`secret.rs:254`) / `serve_consumer` (`secret.rs:441`) /
  `relay()` con failover (`secret.rs:688-743`);
- admin: `admin::register()` + `Entry` (nuovo campo `transport: ssh|bore`).

### 1.3 Componenti nuovi

| Componente | Contenuto |
|---|---|
| `src/sshgw.rs` (nuovo) | Server SSH russh: handshake, auth (chiavi/password), parsing `tcpip-forward`/`direct-tcpip`/`exec`/`env`, keepalive/reaper, mapping → registry bore |
| Auth store | Lettura authorized-keys-dir + passwords-file a ogni tentativo (hot reload), cache per mtime |
| Flag server | `--ssh-gateway`, `--ssh-host-key-file`, `--ssh-authorized-keys-dir`, `--ssh-passwords-file`, (opz.) `--ssh-banner` |
| Feature Cargo | `ssh-gateway` (russh + argon2 fuori dal binario di default) |

**Invariante nuovo proposto (I-SSH1, sul modello di I-MC1/DEC-M1)**: `--ssh-gateway` assente
⇒ accept path e data path **byte-identici** a oggi. Il demux esteso si attiva solo col flag.

Nota `#![forbid(unsafe_code)]`: vale per il crate bore, non per le dipendenze — `russh` si
integra come qualunque altra dipendenza (stesso ragionamento di quinn/ring); nessun pattern
"crate ponte" alla `bore-android-tun` necessario, salvo sorprese in audit.

### 1.4 Alternativa scartata: OpenSSH sshd + ForceCommand

Usare un vero `sshd` davanti a bore (ForceCommand/`permitlisten` + helper) è stato valutato e
scartato: sshd **binda lui stesso** i remote forward (nessuna integrazione con registry/admin/
vhost bore), niente semantica "nome = subdomain/secret-id" senza patch, niente password multiple
con hot-reload, un demone esterno in più da orchestrare nel container, niente demux sulla 443
condivisa. L'SSH embedded è più codice nostro ma è l'unico modo di avere l'integrazione vera.

---

## 2. Risposte ai 12 punti

### 2.1 Fattibilità (banda massima / latenza minima / stabilità massima)

**Fattibile.** SSH fa solo da ingress: dal gateway in poi (bind porte pubbliche, routing
Host vhost, relay secret con failover carrier, contatori admin, weblog) si riusa il codice
esistente. Il tratto sostituito è **solo** client↔server: canali SSH multiplexati su una
connessione TCP al posto dei substream yamux (concettualmente identici: entrambi stream
multiplexati con flow-control per-stream su una TCP).

Valutazione onesta sui tre assi:

| Asse | Tratto SSH | Note |
|---|---|---|
| **Banda** | Buona ma inferiore al nativo | 1 sola conn TCP (niente `--carriers`); finestra per-canale OpenSSH ~2 MiB ⇒ tetto teorico ≈ 2 MiB/RTT per connessione proxata (≈200 MB/s @10 ms, ≈20 MB/s @100 ms); crypto AES-GCM/chacha20 a line-rate su CPU moderne (multi-Gbps/core, non è il collo) |
| **Latenza** | Invariata | Nessun hop in più: il canale SSH termina nello stesso processo server; +1 framing trascurabile |
| **Stabilità** | Equivalente al nativo | Con la ricetta §2.6: keepalive interno + `ServerAlive*` + autossh/systemd + takeover. La riconnessione è "ricrea tunnel" esattamente come `--auto-reconnect` |

Limiti strutturali del tratto SSH (non aggirabili):
- **HOL-blocking**: un pacchetto perso sulla TCP unica stalla *tutti* i canali (è il problema
  che `--carriers` risolve per il client nativo);
- **cwnd condivisa**: tutte le connessioni proxate condividono una congestion window;
- niente QUIC/UDP direct (→ §2.4).

Conclusione: per "banda massima" il client bore nativo resta lo strumento; l'SSH gateway è la
porta d'ingresso universale. Sono complementari, non alternativi.

### 2.2 Funzionalità perse (solo per i tunnel originati via SSH)

| Funzionalità | Via SSH | Perché / mitigazione |
|---|---|---|
| `--udp` / QUIC direct (public, vhost, secret) | ❌ | SSH è TCP-only (§2.4). Sempre relay TCP |
| `--carriers > 1` | ❌ | Una sola connessione SSH. I canali danno già multiplexing, ma senza isolamento cwnd/HOL |
| Hole-punch (`--stun-server`, `--upnp`, `--try-port-prediction`, `--nat-udp-*`) | ❌ | Ha senso solo col path UDP |
| `bore transfer` (resume, BLAKE3, `--parallel`) | ❌ | Protocollo applicativo del client bore. Via SSH si usa il normale `scp`/`rsync`… che però non passa dal tunnel bore |
| `--https`/`--force-https` (terminazione TLS lato client) | ❌ v1 | Il client SSH non può terminare TLS. Per HTTP il vhost copre già HTTPS lato server (wildcard cert) |
| `--basic-auth` | ⚠️ diverso | In bore la enforce il *provider*; via SSH deve farla il **gateway** (401 server-side). Fattibile per vhost/public HTTP perché l'head HTTP è già parsato server-side; per secret/TCP generico non applicabile |
| `--auto-reconnect` | ✅ equivalente | Lo fa `autossh`/systemd invece del client |
| Consumer secret con path diretto p2p | ❌ | Il direct QUIC p2p richiede il client bore su *entrambi* i lati. Provider nativo + consumer SSH = sempre relay |
| Compressione | ➕ bonus | `ssh -C` (zlib) — il client bore non ce l'ha |
| Weblog / admin / `--max-conns` / notes | ✅ | Tutto server-side, si riusa (weblog vhost già server-side) |

Cose che si **guadagnano**: zero-install (qualunque OS con OpenSSH ≥ 7.8, incluse Windows 10+,
router, NAS), auth per-identità con chiavi (oggi bore ha un solo `--secret` condiviso HMAC),
opzioni/restrizioni per-chiave, N tunnel su una sola sessione SSH (più `-R`/`-L` insieme).

### 2.3 Tutto sulla 443: sì

Oggi il compose mappa già `443:7835` (topologia "unified": il control port fa anche da
frontend vhost HTTPS, con routing per Host header — `server.rs:707-714`). Il demux esistente
è un byte-peek HTTP-vs-bore (`route_connection`, `admin_http::is_http_first_byte`). I primi
byte dei quattro protocolli sono **disgiunti**:

| Protocollo | Primo byte | Chi parla per primo |
|---|---|---|
| SSH | `S` (0x53, banner `SSH-2.0-…`) | entrambi (OpenSSH manda subito) |
| TLS | 0x16 (ClientHello) | client |
| HTTP plain | `G/P/H/D/O/T/C` | client |
| bore plain (yamux) | 0x00 | client (Hello immediato, yamux lazy) |

Modifica: spostare il peek **prima** dell'eventuale `TlsAcceptor::accept` (oggi il TLS accept
avviene subito, `server.rs:992` — un client SSH non parla TLS e farebbe fallire l'handshake).
Nuovo ordine: `accept → peek(1 byte, timeout 2 s) → dispatch`; il byte peekato si replay-a col
wrapper `Prefixed` già esistente.

Due accortezze:
1. **Client SSH che aspettano il banner del server** (alcuni client storici, PuTTY in certi
   casi): non mandano nulla finché il server non si presenta. Soluzione standard (è quella di
   `sslh`): **timeout sul peek ⇒ assume SSH** e manda il banner. Sicuro: TLS/HTTP/bore parlano
   tutti per primi entro millisecondi.
2. **SSH-over-TLS** (DECISO: in v1 — D-SSH4): dopo il TLS accept si ri-peeka e si accetta
   `SSH-` anche *dentro* TLS → il tunnel passa i firewall/DPI che vogliono vero TLS sulla 443.
   Lato client: `ProxyCommand openssl s_client -quiet -connect %h:443` (o `stunnel`/`socat`).
   Costo marginale visto il demux a strati; il secondo peek riusa lo stesso `Prefixed`.

Lato UDP niente da fare: l'endpoint QUIC (`vhost_quic_port`, default già 443) e lo STUN su
7835/udp restano invariati. **Risultato: TCP 443 = SSH + TLS(control/vhost/admin) + HTTP + bore
plain; UDP 443 = QUIC.** La 7835 può restare per retro-compatibilità o sparire dal mapping.

Zero-regression: il nuovo demux si attiva solo con `--ssh-gateway` (I-SSH1); senza flag il
path attuale resta byte-identico.

### 2.4 UDP: no sul tratto SSH (limite di protocollo), sì nel resto del sistema

Il protocollo SSH (RFC 4254) non ha forwarding UDP. Opzioni esaminate:

| Opzione | Verdetto |
|---|---|
| Canali SSH standard | Solo stream TCP-like. UDP impossibile |
| `ssh -w` (tun layer 3) | ❌ Root su entrambi i lati, TCP-over-TCP (meltdown sotto perdita), è una VPN non un port-forward. Fuori scope: per L3 c'è già `bore vpn` |
| Incapsulare UDP nei canali (à la `ssh -R` + socat) | ❌ Richiede logica applicativa lato client ⇒ perde il vantaggio "zero-install" |

**Conclusione netta**: un tunnel creato via SSH viaggia **sempre e solo sul relay TCP**.
Se per un certo tunnel il path UDP/QUIC è fondamentale, quel tunnel deve usare il client
bore nativo. La coesistenza è totale: sullo stesso server (e sulla stessa 443) i client
nativi continuano ad avere `--udp` con hole-punch/QUIC; l'endpoint QUIC su UDP 443 non è
toccato dal gateway. Il gateway deve **avvisare, mai ignorare silenziosamente** (filosofia
bore): se l'utente passa `udp=on` nei parametri → `warn` sul canale stderr SSH.

### 2.5 Passaggio parametri: 3 canali complementari

Priorità proposta: **opzioni per-chiave** (authorized_keys) > **stringa comando exec** >
**variabili d'ambiente** > default server.

1. **Stringa comando** (consigliato; funziona anche con autossh):
   ```bash
   ssh -p 443 -R mysub:80:localhost:8080 bore.mydomain.tld -- \
       'notes="api di prod" max-conns=512 basic-auth=user:pass'
   ```
   La stringa arriva al gateway come richiesta `exec`; parsing `chiave=valore` (quoting
   shell-like). Senza comando (`-N`) valgono i default. Il gateway tiene il canale aperto
   (equivale a `-N`) e ci scrive gli esiti (URL vhost, porta assegnata, warning).
2. **Variabili d'ambiente** (per config statica in `~/.ssh/config`):
   ```
   Host bore
     SetEnv BORE_NOTES=api-prod BORE_MAX_CONNS=512
   ```
   Il gateway accetta env `BORE_*` (richiesta `env` SSH; russh la espone).
3. **Opzioni per-chiave** nel file chiavi (vince su tutto — è policy dell'amministratore):
   ```
   permit="vhost/mysub,secret/ci-*",max-conns=256,notes="ci runner" ssh-ed25519 AAAA... ci@runner
   ```

**Tabella completa flag binario → SSH** (per i 3 tipi di tunnel; "param" = canale 1/2/3):

| Flag bore | Via SSH | Come |
|---|---|---|
| `local_port` / `--local-host` | ✅ | Dalla spec `-R …:host:porta` (li sceglie il client SSH) |
| `--to` / `-p` (porta remota) | ✅ | Host/porta del comando ssh; porta pubblica nel campo port di `-R` |
| `--tcp-secret-id` | ✅ | bind_address di `-R` (provider) / host di `-L` (consumer) |
| `--port` (public) | ✅ | Campo port di `-R` (`0` = assegnata dal server, riportata via SSH e stampata da OpenSSH: "Allocated port …") |
| `--subdomain` (vhost) | ✅ | bind_address di `-R` |
| `--id` (vhost client-id) | ✅ | param `id=` (default: fingerprint della chiave SSH) |
| `--notes` | ✅ | param `notes=` |
| `--max-conns` | ✅ | param `max-conns=` (semaforo lato gateway, parità col nativo) |
| `--basic-auth` | ⚠️ | param `basic-auth=` — enforcement lato gateway, solo tunnel HTTP (vhost/public-http). §2.2 |
| `--webserver-log*` | ✅ | param `webserver-log=on` (weblog server-side esistente) |
| `--local-proxy-port` (proxy) | ✅ | bind locale di `-L` (lato client SSH) |
| `--auto-reconnect` | ✅ | autossh/systemd (client-side) |
| `--secret` (HMAC) | n/a | Sostituito dall'auth SSH (chiavi/password) |
| `--insecure` | n/a | Sostituito da known_hosts/host-key pinning SSH |
| `--udp`, `--stun-server`, `--upnp`, `--try-port-prediction`, `--nat-udp-*`, `--carriers` | ❌ | Impossibili sul tratto SSH → `warn` esplicito sul canale, mai silenzio |
| `--https` / `--force-https` | ❌ v1 | Terminazione TLS client-side impossibile; vhost copre HTTPS server-side. `force-https=on` fattibile come param v2 (redirect lato gateway) |

Requisito "devono essere tutti disponibili": **tutti i parametri con significato server-side
sono disponibili**; quelli che descrivono il *trasporto client* (UDP/carriers/hole-punch) non
possono esistere senza il client bore — il gateway li rifiuta rumorosamente con il perché.

### 2.6 Stabilità (con §2.11): difesa a 4 strati

1. **TCP keepalive**: `shared::tune_tcp` (SO_KEEPALIVE 15 s + TCP_NODELAY) su ogni socket
   accettato — invariante bore esistente, si applica anche alle connessioni SSH.
2. **Keepalive SSH lato server (interno)**: il gateway manda `keepalive@openssh.com` (global
   request con want_reply) ogni **20 s** e chiude+reappa la connessione dopo **60 s** senza
   traffico — parità deliberata con `CTRL_CLIENT_HEARTBEAT`/`SECRET_CTRL_TIMEOUT` e con
   l'invariante del "zombie-entry reaper": una connessione SSH half-open non deve mai lasciare
   entry fantasma nei registry (vhost/secret/admin). Le registrazioni sono RAII come oggi:
   il drop del handler SSH rilascia subdomain/secret-id/porta.
3. **Keepalive lato client**: `ServerAliveInterval/CountMax` (§2.8) — rileva il server morto
   e fa uscire ssh ⇒ autossh/systemd riavvia.
4. **Supervisione client**: `autossh -M0` (il monitoraggio via porte -M è legacy: meglio
   affidarsi a ServerAlive) oppure unità systemd con `Restart=always` (§2.7).

**Race di riconnessione (il glitch classico)**: il client riparte prima che il server abbia
reappato la vecchia sessione ⇒ "subdomain already live" ⇒ loop di flap. Soluzione: **takeover
per stessa identità** — se la nuova sessione autentica con la *stessa chiave/identità* che
possiede l'entry esistente, il gateway sfratta la vecchia sessione e insedia la nuova
(comportamento alla sish). Identità diversa ⇒ rifiuto come oggi. Questo rende la riconnessione
deterministica invece che dipendente dal timing del reaper.

### 2.7 Critica dei comandi d'esempio

Due comandi su quattro **non passano il parser di OpenSSH**, e mancano le opzioni di stabilità.

| Originale | Problema |
|---|---|
| `ssh -p 443 -R mysubdomin:80:localhost:8080 bore.mydomain.tld` | ✅ Sintassi valida (bind_address non numerico + port). Manca `-N`/opzioni keepalive |
| `ssh -p 443 -R tcp-secret-id:localhost:8080 bore.mydomain.tld` | ❌ **Invalido**: forma a 3 campi ⇒ il 1° campo deve essere una *porta numerica* → "Bad remote forwarding specification". Serve la forma a 4 campi con porta `0` |
| `ssh -p 443 -L localhost:8899:tcp-secret-id bore.mydomain.tld` | ❌ **Invalido**: a `-L` manca la porta di destinazione → forma `-L 8899:host:porta`. Serve `:0` finale |
| `autossh … proxy secret` | ❌ Copia/incolla: ripete la riga `-R` del provider invece della `-L` del consumer |

**Comandi corretti** (convenzione: porta `80`/`443` + label ⇒ vhost; porta `0` + label ⇒
secret; porta numerica senza label ⇒ public — con prefissi espliciti `vhost/`, `secret/`
come override anti-ambiguità):

```bash
# Opzioni di stabilità comuni (o in ~/.ssh/config, vedi sotto)
OPTS='-o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3
      -o ConnectTimeout=10 -o TCPKeepAlive=yes'

# VHOST: mysub.bore.mydomain.tld → localhost:8080
ssh $OPTS -p 443 -N -R mysub:80:localhost:8080 bore.mydomain.tld
autossh -M0 $OPTS -p 443 -N -R mysub:80:localhost:8080 bore.mydomain.tld

# PUBLIC: porta pubblica 9005 → localhost:8080   (porta 0 = assegnata dal server)
ssh $OPTS -p 443 -N -R 9005:localhost:8080 bore.mydomain.tld

# SECRET provider: registra "tcp-secret-id" → localhost:8080   (nota il ":0")
ssh $OPTS -p 443 -N -R tcp-secret-id:0:localhost:8080 bore.mydomain.tld

# SECRET consumer: localhost:8899 → provider "tcp-secret-id"   (nota il ":0")
ssh $OPTS -p 443 -N -L 8899:tcp-secret-id:0 bore.mydomain.tld
autossh -M0 $OPTS -p 443 -N -L 8899:tcp-secret-id:0 bore.mydomain.tld

# Con parametri (al posto di -N):
ssh $OPTS -p 443 -R mysub:80:localhost:8080 bore.mydomain.tld -- 'notes="demo" max-conns=128'
```

`~/.ssh/config` equivalente (consigliato — i comandi si riducono a `ssh bore-vhost`):

```
Host bore-vhost
  HostName bore.mydomain.tld
  Port 443
  RemoteForward mysub:80 localhost:8080
  ExitOnForwardFailure yes
  ServerAliveInterval 15
  ServerAliveCountMax 3
  ConnectTimeout 10
  SessionType none          # = -N (OpenSSH ≥ 8.7)
```

Unità systemd (alternativa robusta ad autossh):

```ini
[Unit]
Description=bore ssh tunnel (vhost mysub)
After=network-online.target
[Service]
ExecStart=/usr/bin/ssh -F /etc/bore/ssh_config bore-vhost
Restart=always
RestartSec=3
[Install]
WantedBy=multi-user.target
```

Note ulteriori:
- `autossh -M0` corretto (niente porte di echo; si affida a ServerAlive). Aggiungere
  `AUTOSSH_GATETIME=0` in ambiente, altrimenti autossh NON riavvia se la prima connessione
  fallisce (default 30 s di "gate").
- `ExitOnForwardFailure=yes` è **obbligatorio**: senza, una sessione con forward rifiutato
  (es. subdomain occupato) resta viva "vuota" e autossh non riavvia mai.
- bind_address di `-R 9005:…` (OpenSSH manda `localhost` se omesso): il gateway lo ignora e
  binda secondo la semantica bore (`--bind-tunnels`) — divergenza documentata da sshd
  (`GatewayPorts`), che qui non ha senso.

### 2.8 Parametri SSH lato client: sì, tutti

`ServerAliveInterval`, `ServerAliveCountMax`, `ConnectTimeout`, `TCPKeepAlive`,
`ExitOnForwardFailure`, `Compression`, `Ciphers`, `IdentityFile`, `BatchMode`, ecc. sono
implementati interamente dal client OpenSSH: il gateway li "supporta" gratis. Unico requisito
server-side: **rispondere** alle global request di keepalive (qualunque risposta, anche
failure, azzera il contatore ServerAlive del client — russh lo gestisce). Profilo consigliato
per tunnel non presidiati: `ServerAliveInterval=15`, `CountMax=3` (rilevazione morte ≤ 45 s,
sotto il reaper server di 60 s), `BatchMode=yes` (mai prompt interattivi), `Compression` solo
per payload comprimibili (per stream già compressi/cifrati è CPU sprecata).

### 2.9 Autenticazione a chiavi pubbliche con hot-reload

- Flag: `--ssh-authorized-keys-dir /etc/bore/authorized_keys.d/`.
- Formato: file in formato `authorized_keys` standard (uno o più per file — un file per
  identità/team è comodo per il provisioning), con colonna opzioni supportata (subset):
  `permit="vhost/mysub,secret/ci-*,port/9005-9020"`, `max-conns=`, `notes=`, `deny-public`,
  ecc. Le opzioni per-chiave **vincono** sui parametri passati dal client (policy admin).
- **Hot-reload by construction**: la directory viene ri-scandita **a ogni tentativo di
  auth** (aggiungere/togliere una chiave = efficace alla connessione successiva, zero
  restart, zero watcher). Cache per `(path, mtime)` per non ri-parsare sotto gli scan dei
  bot. Nessuna dipendenza da inotify (portabilità).
- Revoca: la rimozione della chiave blocca le **nuove** sessioni; le sessioni vive restano
  (documentato). Opzione v2: sweep periodico che termina le sessioni la cui chiave è sparita.
- L'identità (commento della chiave o nome file) finisce nell'admin dashboard come owner
  del tunnel.

### 2.10 Autenticazione a password multiple con hot-reload

- Flag: `--ssh-passwords-file /etc/bore/passwords` — una credenziale per riga:
  ```
  # label:hash
  ci-runner:$argon2id$v=19$m=65536,t=3,p=1$…
  fabio:$argon2id$…
  ```
- **Più password valide contemporaneamente**: il tentativo è verificato contro tutte le
  righe; la `label` della riga vincente diventa l'identità (admin/notes/takeover). Lo
  username SSH è libero (usato solo come etichetta secondaria).
- Hash **argon2id obbligatorio** consigliato (mai plaintext su disco); un sottocomando
  `bore hash-password` per generare le righe.
- Hot-reload identico alle chiavi: rilettura a ogni tentativo + cache mtime.
- Avvertenze esplicite da documentare:
  - `autossh` non presidiato + password ⇒ serve `sshpass`/`SSH_ASKPASS` (la password finisce
    in ENV o file) → **le chiavi restano il default raccomandato**; le password sono il
    ripiego per ambienti dove distribuire chiavi è impraticabile.
  - argon2 è volutamente costoso ⇒ vettore DoS sul thread di auth: cap di verifiche
    concorrenti + rate-limit per IP (§2.12-sicurezza).

### 2.11 Gestione glitch / cadute / keepalive interno

Coperto in §2.6 (4 strati + takeover). Aggiunte specifiche:

- **Glitch brevi (< keepalive)**: TCP assorbe; i canali SSH riprendono da soli. Nessuna
  azione.
- **Half-open (NAT reboot, cavo staccato)**: rilevato dal lato client in ≤ 45 s
  (ServerAlive 15×3) e dal lato server in ≤ 60 s (reaper). Il reaper server è
  indispensabile per liberare subdomain/secret-id/porta pubblica: senza, un client
  morto silenziosamente terrebbe occupato il nome fino al timeout TCP del kernel (ore) —
  stessa lezione del reaper secret esistente.
- **Caduta server**: il client esce (ServerAlive) → autossh/systemd ritenta con backoff
  (`RestartSec`), riconnessione = ri-registrazione completa (idempotente grazie al takeover).
- **Caduta del servizio locale** (il `localhost:8080` dietro il tunnel): connessioni al
  target rifiutate ⇒ il canale si chiude per-connessione; il tunnel resta su. Parità col
  comportamento nativo.
- **Ordine di spegnimento pulito**: alla SIGTERM del server, inviare
  SSH_MSG_DISCONNECT ai client prima di chiudere — i client autossh ripartono subito verso
  il server nuovo invece di aspettare il timeout.

### 2.12 Cose non chieste ma rilevanti

1. **Host key del server**: generare ed25519 al primo avvio e **persisterla**
   (`--ssh-host-key-file`, default nella dir dati; nel container → volume). Senza
   persistenza ogni riavvio cambia fingerprint ⇒ `StrictHostKeyChecking` fa fallire tutti
   gli autossh (MITM warning). Documentare il pinning in known_hosts lato client.
2. **Superficie d'attacco**: un banner SSH sulla 443 attira scanner ovunque. Contromisure:
   niente shell/pty/sftp (si accettano SOLO `tcpip-forward`, `direct-tcpip`, `exec` per i
   parametri, `env`, keepalive — tutto il resto → disconnect), timeout pre-auth 30 s,
   max 3 tentativi per connessione, cap connessioni pre-auth per IP, solo algoritmi moderni
   (ed25519, rsa-sha2-*; niente ssh-rsa/SHA-1), log strutturato dei tentativi (compatibile
   fail2ban), opzionale banner vuoto/neutro.
3. **Auth bore esistente**: `--secret` (HMAC) resta per i client nativi; l'auth SSH è
   indipendente e per-identità. Un server può esigere entrambi i mondi con policy diverse.
4. **Admin dashboard**: badge `transport: ssh` + identità (chiave/label) per riga; contatori
   TX/RX riusano `CountingStream` come oggi. Nessun cambiamento wire.
5. **Ambiguità naming**: `-R foo:80` (vhost) vs `-R foo:0` (secret) è un'euristica; i
   prefissi espliciti `vhost/foo`, `secret/foo` nel bind_address sono l'override che
   elimina ogni ambiguità (e permettono futuri namespace, es. `tls/foo` per SNI-passthrough).
6. **UX degli esiti**: il gateway scrive sul canale (stderr della sessione) le informazioni
   vitali: URL vhost assegnati (`https://mysub.bore.mydomain.tld`), porta pubblica assegnata,
   warning parametri ignorati, motivo dei rifiuti. Con `-N` OpenSSH le mostra comunque.
7. **Multi-tunnel per sessione**: più `-R`/`-L` sulla stessa connessione = N tunnel con un
   solo processo autossh. Da supportare esplicitamente (registrazioni multiple per sessione,
   teardown parziale su `cancel-tcpip-forward`).
8. **Windows/macOS/router**: OpenSSH è ovunque ⇒ il gateway estende di fatto la copertura
   client di bore a piattaforme dove il binario non arriva (o non è compilato), gratis.
9. **Crate `russh`**: puro Rust, tokio-native, mantenuto, server API completa
   (auth pubkey/password, global request, canali direct/forwarded-tcpip, exec/env). Da
   verificare in fase di spike: performance del path dati (throughput canale vs yamux),
   audit dipendenze. Piano B: `libssh2` via FFI (scartato di default: C, blocking).
10. **Testing** (bozza): unit per parser spec/params/authorized-options; e2e
    `scripts/ssh_gateway_test.sh` in netns con client OpenSSH reale (vhost/public/secret
    provider+consumer, takeover, reaper half-open via `iptables DROP`, password+chiavi,
    hot-reload a caldo, demux 443 con tutti e 4 i protocolli mescolati); gate CI dedicato.
11. **Fuori scope dichiarato (v1)**: `ssh -D` (SOCKS dinamico), `ssh -w` (tun), SFTP,
    forwarding UDP simulato, `bore transfer` via SSH, terminazione `--https` per public
    tunnel via gateway. (SSH-over-TLS invece È in v1 — D-SSH4.)

---

## 3. Superficie di implementazione stimata (per il piano futuro — NON in questo task)

| Area | File | Natura |
|---|---|---|
| Demux 443 pre-TLS | `src/server.rs` (accept loop + `route_connection`), `src/prefixed.rs` | Estensione peek: `SSH-`/0x16/HTTP/yamux, gated da flag |
| Gateway SSH | `src/sshgw.rs` (nuovo, ~grosso) | russh server, auth stores hot-reload, parser -R/-L/exec/env, keepalive+reaper, mapping → registry |
| Astrazione link | `src/server.rs`, `src/secret.rs`, `src/vhost.rs`, `src/mux.rs` | `ClientLink` (yamux vs canale SSH), STREAM_READY confinato nell'impl yamux |
| Enforcement gateway | `src/vhost.rs`/`src/sshgw.rs` | basic-auth 401 server-side per tunnel HTTP via SSH |
| Admin | `src/admin.rs`, frontend | Campo `transport`, identità |
| CLI | `src/main.rs` | Flag `--ssh-gateway*`, sottocomando `bore hash-password` |
| Build | `Cargo.toml` | feature `ssh-gateway` (russh, argon2) |
| Test | `tests/`, `scripts/ssh_gateway_test.sh` | Unit + e2e netns + CI |
| Docs | `docs/SSH_GATEWAY.md` (questo file → guida utente), compose | Esempi, security notes |

Invarianti da sancire nel piano: **I-SSH1** (flag off ⇒ byte-identico), **I-SSH2** (parametri
non supportati via SSH ⇒ warn esplicito, mai silenzio), **I-SSH3** (keepalive 20 s/reaper 60 s
in parità con i tunnel secret; nessuna entry fantasma), **I-SSH4** (STREAM_READY mai sul
canale SSH), **I-SSH5** (takeover solo a parità di identità).

---

## 4. Decisioni prese (2026-07-03, confermate dall'owner)

| ID | Decisione |
|---|---|
| **D-SSH1** | Naming `-R`: **euristica + prefissi**. Porta 80/443+label ⇒ vhost, porta 0+label ⇒ secret, porta numerica ⇒ public; `vhost/`/`secret/` nel bind_address come override esplicito anti-ambiguità |
| **D-SSH2** | **Takeover a parità di identità**: una nuova sessione autenticata con la stessa chiave/label sfratta la sessione esistente che detiene il nome (riconnessione autossh deterministica); identità diversa ⇒ rifiuto |
| **D-SSH3** | Password file: **solo hash argon2id** (`label:$argon2id$…`, generati con `bore hash-password`); niente plaintext su disco; cap sulle verifiche concorrenti anti-DoS |
| **D-SSH4** | **SSH-over-TLS in v1**: demux a doppio strato (peek plain → TLS accept → secondo peek); i client dietro DPI-only-TLS entrano via `ProxyCommand openssl s_client` |

Domanda residua (non bloccante per il piano): **porta 7835** — mantenerla esposta per
retro-compatibilità dei client nativi esistenti o consolidare tutto su 443? Default proposto:
mantenerla nel compose finché i client in campo non sono migrati, poi rimuoverla dal mapping.

---

## 5. Riferimenti

- Prior art: sish (https://github.com/antoniomika/sish), serveo, localhost.run, tuns.sh
- RFC 4254 (SSH Connection Protocol: `tcpip-forward`, `forwarded-tcpip`, `direct-tcpip`)
- sslh (demux multi-protocollo su una porta; strategia timeout-⇒-SSH)
- Codice bore citato: `server.rs:983-1085` (accept/demux), `mux.rs:23-112` (astrazione
  stream), `secret.rs:254/441/688` (provider/consumer/relay), `vhost.rs:160/536/772/1168`
  (routing/registrazione/relay), `admin_http.rs:46` (byte-peek HTTP), `prefixed.rs` (replay),
  `auth.rs` (HMAC nativo), `shared.rs` (CONTROL_PORT, heartbeat/timeout)
