# bore SSH Gateway — guida utente

`bore server --ssh-gateway` fa da server SSH embedded, così un client `ssh`/`autossh`
**stock** (nessun binario `bore` sul client) può aprire tunnel **public**, **vhost** e
**secret**. Documento completo di analisi/architettura: `docs/SSH_GATEWAY.md`. Questo
file è la guida rapida "come si usa", con esempi per ogni modalità, ogni parametro, e
`autossh`/systemd per ognuna.

---

## 1. Avvio server

```bash
bore server \
    --control-port 7835 \
    --admin-token "$(openssl rand -hex 24)" \
    --vhost-base-domain bore.example.com --vhost-http-port 7835 \
    --ssh-gateway \
    --ssh-host-key-file /etc/bore/ssh/host_key.pem \
    --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d \
    --ssh-passwords-file /etc/bore/ssh/passwords \
    --ssh-banner "Authorized use only"
```

Flag server (tutti `#[cfg(feature = "ssh-gateway")]`):

| Flag | Env | Obbligatorio | Descrizione |
|---|---|---|---|
| `--ssh-gateway` | `BORE_SSH_GATEWAY` | sì (per attivare) | Abilita il gateway. Richiede **almeno uno** tra `--ssh-authorized-keys-dir`/`--ssh-passwords-file` (fail-fast altrimenti). |
| `--ssh-port <PORT>` | `BORE_SSH_PORT` | no | Porta dedicata extra per SSH. Senza, SSH è servito in demux sulla STESSA porta di controllo/vhost (443/7835) — nessuna porta aggiuntiva da aprire. |
| `--ssh-host-key-file <PATH>` | `BORE_SSH_HOST_KEY_FILE` | no (default `bore_ssh_host_key.pem`) | Host key ed25519, generata al primo avvio se assente. Persistere in un volume, altrimenti il fingerprint cambia a ogni riavvio → `StrictHostKeyChecking` rompe tutti gli autossh. |
| `--ssh-authorized-keys-dir <DIR>` | `BORE_SSH_AUTHORIZED_KEYS_DIR` | uno dei due | Directory con file `authorized_keys`-format (uno o più file, riletti a OGNI tentativo di auth — hot-reload gratis). |
| `--ssh-passwords-file <PATH>` | `BORE_SSH_PASSWORDS_FILE` | uno dei due | File `label:$argon2id$...`, una credenziale per riga. Generare righe con `bore hash-password`. |
| `--ssh-banner <TEXT>` | `BORE_SSH_BANNER` | no | Testo mostrato prima dell'auth. |

Senza `--ssh-gateway` il comportamento è bore-nativo, invariato al 100%.

---

## 2. Provisioning credenziali

### 2.1 Chiavi pubbliche (default consigliato)

```bash
ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519_bore -C "laptop"
```

Copiare la pubkey in un file dentro `--ssh-authorized-keys-dir` (un file per operatore/team):

```
# /etc/bore/ssh/authorized_keys.d/laptop
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOM1XMK7LrZvfZ+evuz//FtdfjgeVCphVmy1d95Ze0Ov laptop
```

Il commento finale (`laptop`) diventa l'**identità** (owner nell'admin dashboard, chiave del
takeover a parità di identità — §5). Opzioni per-chiave, prima della chiave, spazio-separate:

```
permit="vhost/laptop-*,secret/ci-*,port/9000-9100",max-conns=50,notes="dev laptop" ssh-ed25519 AAAA... laptop
```

| Opzione per-chiave | Effetto |
|---|---|
| `permit="pattern1,pattern2,..."` | Whitelist dei nomi che questa chiave può richiedere: `vhost/<glob>`, `secret/<glob>`, `port/<N>` o `port/<N1>-<N2>`. Senza `permit=`, la chiave può richiedere qualunque nome libero. |
| `max-conns=<N>` | Cap connessioni concorrenti per tunnel — **vince sempre** sul parametro `max-conns=` passato via exec/env dal client (policy admin > richiesta client). |
| `notes=<testo>` | Note fisse mostrate in admin — vincono anch'esse sull'exec/env del client. |

### 2.2 Password (alternativa/aggiuntiva)

```bash
$ echo -n 'correct-horse-battery-staple' | bore hash-password
$argon2id$v=19$m=19456,t=2,p=1$aiFZvnWxcXKlASwnEMrDwQ$SG8n3dO+w9RDJv9poqlI+kGkLfLQVt5dsuxwURkvPno
add to the passwords file as: <label>:$argon2id$v=19$...
```

```
# /etc/bore/ssh/passwords
ci-runner:$argon2id$v=19$m=19456,t=2,p=1$aiFZvnWxcXKlASwnEMrDwQ$SG8n3dO+w9RDJv9poqlI+kGkLfLQVt5dsuxwURkvPno
fabio:$argon2id$...
```

Solo hash argon2id sul disco, mai plaintext. Più righe valide contemporaneamente; la
`label` della riga vincente diventa l'identità. Username SSH (`user@host`) è libero/ignorato
dal server, usato solo come etichetta secondaria in login.

```bash
sshpass -p 'correct-horse-battery-staple' ssh -p 7835 -R 9998:localhost:8080 alice@bore.example.com
```

`autossh` non presidiato + password richiede `sshpass`/`SSH_ASKPASS` (password finisce in
env/file) → **le chiavi restano il default consigliato** per tunnel automatici.

---

## 3. Modalità operative — naming (`-R <bind_address>:<port>`)

Euristica + prefissi espliciti (sempre disambiguano, qualunque porta):

| bind_address:port | Modalità | Prefisso esplicito equivalente |
|---|---|---|
| `<N>` (porta numerica, nessuna label) | **public** — porta pubblica `N` (`0` = assegnata dal server) | — |
| `<label>:80` o `<label>:443` | **vhost** — sottodominio `<label>.<base-domain>` | `vhost/<label>:<qualsiasi porta>` |
| `<label>:0` | **secret provider** — registra id `<label>` | `secret/<label>:0` |
| `<label>:<altra porta>` | ambiguo → **rifiutato** | usare `vhost/`/`secret/` |

Consumer secret (`-L`, sempre secret, la porta finale è un placeholder ignorato — OpenSSH
`-L` non accetta `:0` letterale, quindi usare `:1` o qualunque nonzero):

```
-L <porta_locale>:<id>:1        # o secret/<id>:1
```

---

## 4. Esempi completi per modalità

> **⚠️ Non usare mai `-N`, con o senza parametri.** Vale per **tutte** le modalità sotto —
> vhost, public, secret provider, secret consumer. Motivo: `-N` (`SessionType=none`) impedisce
> a OpenSSH di aprire il canale sessione, quindi il gateway non ha NESSUN modo di scrivere
> né i warning né il banner di stato del tunnel (§5.1/§8) — il terminale resta muto e basta.
> Omettendo `-N`, OpenSSH apre comunque il canale e richiede una shell interattiva di
> default: il gateway la accetta silenziosamente e la usa come canale di stato, senza
> comportarsi come una vera shell (niente prompt, niente comandi) e senza mai chiudere la
> connessione per questo. Vale sia con `exec` params sia senza.

Opzioni di stabilità comuni (client OpenSSH, tutte "gratis" lato gateway):

```bash
OPTS='-o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3
      -o ConnectTimeout=10 -o TCPKeepAlive=yes'
```

### 4.1 VHOST — `mysub.bore.example.com` → `localhost:8080`

```bash
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com
```

Con parametri:

```bash
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- \
    'notes="api prod" max-conns=512 basic-auth=user:pass webserver-log=on'
```

`autossh`:

```bash
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R vhost/mysub:0:localhost:8080 \
    bore.example.com -- 'notes="api prod"'
```

### 4.2 PUBLIC — porta pubblica 9005 → `localhost:8080`

```bash
ssh $OPTS -p 443 -R 9005:localhost:8080 bore.example.com
```

Porta assegnata dal server (`0`):

```bash
ssh $OPTS -p 443 -R 0:localhost:8080 bore.example.com
# OpenSSH stampa: "Allocated port NNNNN for remote forward to localhost:8080"
```

Con parametri:

```bash
ssh $OPTS -p 443 -R 9005:localhost:8080 bore.example.com -- 'notes="staging" max-conns=200'
```

`autossh`:

```bash
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R 9005:localhost:8080 bore.example.com
```

### 4.3 SECRET provider — registra id `tcp-secret-id` → `localhost:8080`

Con parametri:

```bash
ssh $OPTS -p 443 -R secret/tcp-secret-id:0:localhost:8080 bore.example.com -- \
    'notes="db-primary" max-conns=64'
```

`autossh`:

```bash
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R secret/tcp-secret-id:0:localhost:8080 \
    bore.example.com -- 'notes="db-primary"'
```

### 4.4 SECRET consumer — `localhost:8899` → provider `tcp-secret-id`

```bash
ssh $OPTS -p 443 -L 8899:secret/tcp-secret-id:1 bore.example.com
```

`autossh`:

```bash
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -L 8899:secret/tcp-secret-id:1 bore.example.com
```

> Un consumer SSH può parlare con un provider **nativo bore** e viceversa — provider e
> consumer possono essere su trasporti diversi, la relay lato server è indifferente.
> L'unica funzionalità persa è il path diretto p2p (QUIC), che richiede il client bore
> nativo su ENTRAMBI i lati (§7).

### 4.5 Banner di stato del tunnel

Una volta stabilito il forward, il gateway scrive un report sul canale sessione (lo stesso
canale che una shell interattiva avrebbe usato — §4's box qui sopra spiega perché omettere
`-N` è quello che lo rende possibile). Ogni riga riporta solo fatti che il server conosce
per certo: **non** l'host:porta locale del tuo `-R`/`-L` (`localhost:8080` sopra) — quello
non viaggia mai sul protocollo SSH, resta puramente lato client (RFC4254: `tcpip-forward`/
`direct-tcpip` non hanno un campo per la destinazione locale). Il valore reale può volerci
qualche secondo ad arrivare (registrazione admin + risoluzione parametri), non è instantaneo.

**VHOST:**
```
Vhost tunnel established
  Public URL:       http://mysub.bore.example.com
  Mode:             HTTP only
  Identity:         laptop
  Notes:            (none)
  Basic-auth:       disabled
  Webserver-log:    disabled
  Max-conns:        n/a for vhost (server-wide --max-conns applies; no per-tunnel cap)
  Request headers:  (none)
  Response headers: (none)
```

**PUBLIC:**
```
Public tunnel established
  Public port:      9005
  Identity:         laptop
  Notes:            staging
  Max-conns:        200 (requested)
  Basic-auth:       disabled
  HTTPS:            disabled
  Force-HTTPS:      disabled
  Webserver-log:    disabled
```

**SECRET provider** — nota il comando pronto all'uso per il consumer, con placeholder
espliciti (`<same-port>`/`<same-host>`) invece di un valore indovinato: il gateway non può
sapere con certezza il proprio hostname pubblico, e un valore sbagliato sarebbe peggio di
un placeholder onesto:
```
Secret provider tunnel established
  Secret ID:        tcp-secret-id
  Identity:         laptop
  Notes:            db-primary
  Max-conns:        n/a for secret provider (not enforced per-tunnel)
  Basic-auth:       n/a for secret provider (opaque TCP, no HTTP layer)

Consumer command (run on the other side, same host/port you used here):
  ssh -p <same-port> -L <local-port>:secret/tcp-secret-id:1 <same-host>
```

**SECRET consumer** — mostrato una volta per sessione, non una volta per connessione
proxata:
```
Attached to secret 'tcp-secret-id'
  Secret ID:        tcp-secret-id
  Identity:         laptop
  Notes:            (none)
  Provider identity: laptop
```

`Provider identity` mostra `(unknown — provider may be a native bore client)` quando il
provider non è una sessione SSH di questo gateway (es. un client `bore` nativo) — il
consumer funziona comunque, è solo un dettaglio diagnostico che non è disponibile in quel
caso.

---

## 5. Passaggio parametri (3 canali, precedenza: **chiave** > **exec** > **env**)

### 5.1 Stringa `exec` (dopo `--`, funziona anche con autossh)

> **⚠️ Vale la regola di §4: mai `-N`.** Con `-N` il comando dopo `--` non viene MAI
> inviato (OpenSSH non apre alcun canale sessione — `man ssh_config` → `SessionType`),
> quindi `notes=`/`max-conns=`/`https=on`/ecc. restano ai default **senza alcun avviso
> visibile**. Verificare sempre i parametri applicati o nel banner di stato (§4.5) o nella
> dashboard admin dopo la connessione.

```bash
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- \
    'notes="two words" max-conns=512 basic-auth=user:pass webserver-log=on id=custom-id'
```

Grammatica `chiave=valore` spazio-separata, quoting stile shell per valori con spazi
(`notes="due parole"`). Un token senza `=` (es. `https:on` invece di `https=on`) produce un
warning esplicito (`malformed parameter "https:on" (expected key=value); ignored`), non
viene scartato in silenzio.

### 5.2 Variabili d'ambiente (`~/.ssh/config`, static)

```
Host bore
  HostName bore.example.com
  Port 443
  SetEnv BORE_NOTES=api-prod BORE_MAX_CONNS=512 BORE_BASIC_AUTH=user:pass
```

Mappatura: `BORE_<KEY>` → `<key>` (minuscolo, `_`→`-`). Richiede `AcceptEnv BORE_*` lato
sshd... **non applicabile qui**: il gateway bore le accetta direttamente via richiesta SSH
`env`, nessuna configurazione server esterna necessaria — solo `SendEnv`/`SetEnv` lato
client OpenSSH.

### 5.3 Opzioni per-chiave (authorized_keys, vince su tutto — policy admin)

```
permit="vhost/mysub",max-conns=256,notes="ci runner" ssh-ed25519 AAAA... ci@runner
```

### 5.4 Tabella parametri completa

| Parametro | Canali | Applicabile a | Effetto |
|---|---|---|---|
| `notes=<testo>` | exec, env (`BORE_NOTES`), chiave | tutti | Note libere mostrate in admin dashboard |
| `max-conns=<N>` | exec, env (`BORE_MAX_CONNS`), chiave | tutti | Cap connessioni concorrenti (semaforo lato gateway) |
| `basic-auth=<user:pass>` | exec, env (`BORE_BASIC_AUTH`) | vhost, public HTTP | Basic-auth **enforced dal gateway** (401 server-side) — via SSH la fa il gateway, non il provider come nel client nativo |
| `webserver-log=on` | exec, env (`BORE_WEBSERVER_LOG`) | vhost, public | Abilita access log per-tunnel (weblog server-side esistente) |
| `id=<label>` | exec, env (`BORE_ID`) | tutti | Override esplicito dell'id/identità mostrata (default: fingerprint/label della chiave) |
| `https=on` | exec, env (`BORE_HTTPS`) | **public** | Termina TLS sulla porta del tunnel pubblico, usando il certificato del server (richiede `--cert-file`/`--key-file` sul server; senza, la richiesta è servita come TCP semplice con un warning). Riusa `edge::accept`, lo stesso codice del client nativo `bore local --https` |
| `force-https=on` | exec, env (`BORE_FORCE_HTTPS`) | **public** | Redirige le richieste HTTP semplici sulla porta del tunnel a `https://`. Richiede `https=on` sulla stessa richiesta — se assente, viene disabilitato con un warning invece di essere applicato o ignorato in silenzio |

### 5.5 Parametri client-transport-only: **rifiutati con warning esplicito**, mai silenzio

Questi hanno significato solo per il client `bore` nativo (path UDP/QUIC diretto,
multi-connessione). Sul tratto SSH producono un warning sul canale, il tunnel resta comunque
attivo con il comportamento di default:

```bash
$ ssh -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- 'udp=on carriers=4'
bore ssh-gateway: udp: not available via SSH ingress; use the native bore client
bore ssh-gateway: carriers: not available via SSH ingress; use the native bore client
```

Elenco completo: `udp`, `carriers`, `stun-server`, `upnp`, `try-port-prediction`,
`nat-udp-preferred-port`, `auto-reconnect` (quest'ultimo: usare `autossh`/systemd lato
client — è l'equivalente corretto). Qualunque altra chiave non riconosciuta produce
`<key>: unknown parameter`, mai un no-op silenzioso.

### 5.6 `https=on`/`force-https=on`: disponibili per i tunnel **public**; automatici per **vhost**

Due casi distinti, non confonderli:

**VHOST**: HTTPS è governato **lato server** dal `vhost.yml`/`--vhost-mode` (flag
`--vhost-mode http|https|both|redirect-https|auto`, `--vhost-cert-file`/`--vhost-key-file`).
Un tunnel vhost SSH-originato eredita automaticamente lo stesso comportamento HTTPS di un
tunnel nativo sullo stesso host — nessun parametro per-tunnel da passare:

```bash
bore server --ssh-gateway ... \
    --vhost-mode both \
    --vhost-cert-file /etc/bore/vhost/fullchain.pem \
    --vhost-key-file /etc/bore/vhost/privkey.pem
```

**PUBLIC**: `https=on`/`force-https=on` **sono** parametri per-tunnel via SSH (esattamente
come sul client nativo `bore local --https`/`--force-https`), applicati sulla porta pubblica
assegnata a quel forward. Richiede che il server abbia un certificato configurato
(`--cert-file`/`--key-file` sul control port — lo stesso usato per demux/SSH-over-TLS):

```bash
ssh $OPTS -p 443 -R 9443:localhost:8080 bore.example.com -- 'https=on'
# curl https://bore.example.com:9443/   → risposta del servizio locale, TLS terminato dal server

ssh $OPTS -p 443 -R 9444:localhost:8080 bore.example.com -- 'https=on force-https=on'
# curl http://bore.example.com:9444/    → 308 redirect verso https://bore.example.com:9444/
```

Senza certificato server configurato, `https=on` viene servito come TCP semplice con un
warning esplicito sul canale (`https: server has no TLS certificate configured; serving this
tunnel as plain TCP`) — mai un fallimento silenzioso né un rifiuto dell'intero tunnel.
`force-https=on` senza `https=on` viene disabilitato con un warning invece di essere applicato
o ignorato in silenzio.

---

## 6. `~/.ssh/config` di riferimento

```
Host bore
    HostName bore.example.com
    Port 443
    User tunnel
    IdentityFile ~/.ssh/id_ed25519_bore
    IdentitiesOnly yes
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ConnectTimeout 10
    ExitOnForwardFailure yes
    SessionType none
```

```bash
ssh -R vhost/myapp:0:localhost:8080 bore                       # vhost
ssh -R 9005:localhost:8080 bore                                 # public
ssh -R secret/tcp-id:0:localhost:8080 bore                      # secret provider
ssh -L 8899:secret/tcp-id:1 bore                                 # secret consumer
```

`ExitOnForwardFailure=yes` è **obbligatorio** con `autossh`: senza, una sessione con forward
rifiutato (nome occupato) resta viva "vuota" e autossh non la riavvia mai.

---

## 7. systemd (alternativa robusta ad autossh) — un template per modalità

```ini
# /etc/systemd/system/bore-tunnel-vhost.service
[Unit]
Description=bore SSH tunnel (vhost myapp)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=AUTOSSH_GATETIME=0
Environment=AUTOSSH_POLL=30
ExecStart=/usr/bin/autossh -M 0 \
    -o "ServerAliveInterval=15" -o "ServerAliveCountMax=3" \
    -o "ExitOnForwardFailure=yes" -o "StrictHostKeyChecking=yes" \
    -i /etc/bore/client_key -p 443 \
    -R vhost/myapp:0:localhost:8080 tunnel@bore.example.com -- 'notes="prod"'
Restart=always
RestartSec=5
User=bore-client

[Install]
WantedBy=multi-user.target
```

Sostituire la riga `-R`/`-L` di `ExecStart` con una delle §4 per public/secret provider/
secret consumer — resto del template identico. `AUTOSSH_GATETIME=0` è importante: senza,
autossh non riavvia se il PRIMO tentativo fallisce entro 30s (default "gate" period).

---

## 8. SSH-over-TLS (DPI/firewall che permette solo TLS in uscita)

```bash
ssh -o ProxyCommand='openssl s_client -quiet -verify_quiet -connect bore.example.com:443' \
    -R vhost/myapp:0:localhost:8080 dummy-host
```

Il server rileva `SSH-` dopo l'handshake TLS e instrada al gateway automaticamente. Rimuovere
`-verify_quiet` in produzione con certificato CA-emesso (accetta anche self-signed, comodo
solo per test).

---

## 9. Takeover a parità di identità (riconnessione deterministica)

Una NUOVA sessione con la STESSA chiave/identità che detiene già un nome sfratta la
precedente invece di essere rifiutata — questo rende `autossh`/riavvii di rete deterministici
(niente flap in attesa che il reaper da 60s liberi il nome):

```bash
$ ssh -i id_ed25519_bore -p 443 -R vhost/mysub:0:localhost:18080 bore.example.com
Allocated port 1 for remote forward to localhost:18080
```

Identità **diversa** sullo stesso nome ⇒ rifiuto (`subdomain '<label>' already in use`).
Nome protetto da `permit=` non nella whitelist ⇒ `remote port forwarding failed for listen
port 0`.

---

## 10. Fingerprint pinning (produzione)

```bash
ssh-keygen -l -E sha256 -f /etc/bore/ssh/host_key.pem
# 256 SHA256:3a5zdjovpFe3Y/XtIiDSgigHLPvbB3OekBd1g7QdLJw (ED25519)
```

```bash
ssh-keyscan -p 443 bore.example.com >> ~/.ssh/known_hosts   # una volta, su canale fidato
```

Poi ogni connessione verifica contro la riga fissa in `known_hosts` invece del TOFU di
default — `StrictHostKeyChecking=yes` in produzione.

---

## 11. Cosa NON è disponibile via SSH (usare il client `bore` nativo)

| Funzionalità | Motivo |
|---|---|
| `--udp` / QUIC direct (public, vhost, secret) | SSH è TCP-only, limite di protocollo |
| `--carriers > 1` | Una sola connessione SSH; i canali multiplexano ma senza isolamento cwnd/HOL |
| Hole-punch (`--stun-server`, `--upnp`, `--try-port-prediction`, `--nat-udp-*`) | Ha senso solo col path UDP |
| `bore transfer` | Protocollo applicativo del client bore, non del tunnel SSH |
| Consumer secret con path diretto p2p | Richiede client bore nativo su entrambi i lati |
| `--secret` (HMAC), `--insecure` | Sostituiti da auth SSH (chiavi/password) e host-key pinning |

Guadagni rispetto al client nativo: zero-install (qualunque OS con OpenSSH ≥ 7.8, incluse
Windows/macOS/router), auth per-identità con chiavi (vs un solo `--secret` condiviso),
restrizioni per-chiave (`permit=`), N tunnel su una sola sessione SSH, compressione `ssh -C`.

---

## 12. Troubleshooting

| Sintomo | Causa | Rimedio |
|---|---|---|
| `remote port forwarding failed for listen port 0` | `permit=` non copre l'etichetta, o nome già preso da identità diversa | Controllare `permit=`; scegliere un altro nome o usare la stessa chiave del detentore |
| `subdomain '<label>' already in use` / `tcp-secret-id '<id>' already in use` | nome registrato da identità diversa | nome diverso, o stessa chiave/label per takeover legittimo |
| `Permission denied (publickey,hostbased,keyboard-interactive)` | chiave non nel dir, o password/formato hash errato | verificare pubkey nel file; rigenerare hash con `bore hash-password` |
| `<flag>: not available via SSH ingress; use the native bore client` | parametro client-transport-only (§5.5) passato via exec/env | usare il client bore nativo, o ignorare se il default va bene |
| `<key>: unknown parameter` | typo, o parametro non supportato | vedi tabella §5.4 |
| Tunnel sparisce dopo ~60s di silenzio di rete | reaper keepalive (comportamento corretto, non un bug) | `ServerAliveInterval`/autossi lato client per attraversare interruzioni brevi |
| `connect to host ... port 443: Connection refused` con `ProxyCommand openssl s_client` | server senza TLS su quella porta, o `--ssh-gateway` disabilitato | verificare `--cert-file`/`--key-file` e la porta del control port |

Guida di analisi/architettura completa (incl. invarianti I-SSH1..5): `docs/SSH_GATEWAY.md`.
