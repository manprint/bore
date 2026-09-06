# bore SSH Jump Host — guida operativa end-to-end

Questa guida è autosufficiente: dalla configurazione del server fino al comando
`ssh` finale dell'operatore, con esempi copiabili sia con il **binario** sia con il
**wrapper Docker**. Non richiede la lettura di altri documenti.

---

## 0. Cosa ottieni

Una macchina (VM, server in ufficio, NAS, Raspberry) che ha un `sshd` locale ma
**non è raggiungibile da Internet** viene pubblicata sul server bore con un nome
proprio. L'operatore la raggiunge con OpenSSH standard, senza VPN e senza il
binario `bore` sul proprio portatile:

```bash
ssh -J fabio@bore.tld:443 ubuntu@vm-test-01.ssh.bore.tld
```

Percorso dei dati:

```text
operatore (OpenSSH)  ──►  bore server (gateway SSH, TCP 443)  ──►  provider  ──►  sshd locale
                                                    QUIC diretto oppure TCP caldo
```

Punti chiave, da tenere a mente per tutta la guida:

- **Due sessioni SSH indipendenti.** Una esterna verso il gateway bore
  (`fabio@bore.tld`), una interna verso la VM (`ubuntu@vm-test-01...`). Due account,
  due autenticazioni, due `known_hosts`. Il server bore **non vede** la password né
  la chiave dell'account target: la sessione interna è cifrata end-to-end.
- **Un solo record DNS.** Serve solo `bore.tld`. Gli alias tipo
  `vm-test-01.ssh.bore.tld` **non** richiedono record DNS: il nome viaggia dentro
  la richiesta SSH.
- **Due modi di pubblicare la VM**:

| Provider sulla VM | Comando | Autenticazione verso bore | Trasporto server→VM |
|---|---|---|---|
| **nativo** (binario `bore` sulla VM) | `bore sshjhost` | `BORE_SECRET` | QUIC diretto opzionale + TCP caldo di riserva |
| **OpenSSH puro** (nessun binario `bore`) | `ssh -R jump/...` | account SSH del gateway (chiave o password) | solo TCP |

- **La porta è esatta.** Se il target ha `sshd` su 2222, l'operatore deve usare
  `ssh -p 2222 ...`. Una richiesta alla 22 su quell'alias viene rifiutata.

---

## 1. Configurazione del server

### 1.1 Prerequisiti

| Requisito | Dettaglio |
|---|---|
| Binario | compilato con la feature `ssh-gateway` (`cargo build --release --features ssh-gateway`, oppure `--all-features`). L'immagine Docker ufficiale la include già. |
| DNS | un record A/AAAA per `bore.tld`. Nessun wildcard richiesto per `*.ssh.bore.tld`. |
| Firewall | **443/tcp** (gateway SSH + control port). In più **443/udp** solo se vuoi il percorso QUIC diretto per i provider nativi. |
| Certificato TLS | quello che già usi per `bore.tld`. Non serve un certificato nuovo per il namespace jump. |

### 1.2 File da preparare sul server

```bash
sudo mkdir -p /etc/bore/ssh/authorized_keys.d
sudo touch /etc/bore/ssh/passwords
sudo chmod 0700 /etc/bore/ssh
sudo chmod 0750 /etc/bore/ssh/authorized_keys.d
sudo chmod 0600 /etc/bore/ssh/passwords
```

| File | A cosa serve |
|---|---|
| `/etc/bore/ssh/host_key.pem` | identità del gateway bore verso i client OpenSSH. **Generata da sola al primo avvio.** Va conservata: se cambia, tutti gli operatori vedono l'allarme `known_hosts`. |
| `/etc/bore/ssh/authorized_keys.d/<username>` | chiavi pubbliche degli account gateway. |
| `/etc/bore/ssh/passwords` | righe `username:$argon2id$...` per gli account a password. |

Il server rifiuta l'avvio con `--ssh-gateway` se non c'è **almeno uno** tra la
directory chiavi e il file password.

### 1.3 Regola fondamentale degli account jump

> Per le operazioni jump (pubblicare un alias e collegarsi a un alias) lo **username
> SSH deve coincidere esattamente** con il nome del file della chiave, o con
> l'etichetta della riga password. Confronto esatto, case-sensitive.

Account con chiave pubblica per l'operatore `fabio`:

```bash
# Sul portatile dell'operatore
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_bore_gateway -C 'fabio@bore-jump'

# Sul server: SOLO la chiave pubblica, in un file di nome "fabio"
sudo install -m 0600 /percorso/id_ed25519_bore_gateway.pub \
     /etc/bore/ssh/authorized_keys.d/fabio
```

Va bene anche il nome `fabio.pub`. Il commento finale della chiave (`fabio@bore-jump`)
**non** sostituisce lo username.

Account dedicato per una VM che pubblica in modalità OpenSSH pura:

```bash
sudo install -m 0600 /percorso/id_ed25519_vm.pub \
     /etc/bore/ssh/authorized_keys.d/vm-provider
```

Account a password (utile per accesso interattivo; per i servizi permanenti usa
sempre una chiave):

```bash
read -rsp 'Password gateway per fabio: ' PW; echo
printf '%s' "$PW" | bore hash-password
# output: $argon2id$v=19$...
unset PW
# aggiungi al file, una riga per account:
#   fabio:$argon2id$v=19$...
```

Il plaintext non viene mai scritto su disco né passato come argomento.

### 1.4 Flag del server

```bash
bore server \
  --control-port 7835 \
  --udp \
  --vhost-quic-port 443 \
  --ssh-gateway \
  --ssh-jump-base-domain ssh.bore.tld \
  --ssh-advertise-address bore.tld \
  --ssh-advertise-port 443 \
  --ssh-host-key-file /etc/bore/ssh/host_key.pem \
  --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d \
  --ssh-passwords-file /etc/bore/ssh/passwords
```

| Flag | Variabile d'ambiente | Obbligatorio | Descrizione |
|---|---|---|---|
| `--ssh-gateway` | `BORE_SSH_GATEWAY` | sì | Attiva il server SSH interno. Richiede almeno una fonte di credenziali. |
| `--ssh-jump-base-domain <DOMAIN>` | `BORE_SSH_JUMP_BASE_DOMAIN` | sì (per il jump) | Abilita il namespace `<alias>.<DOMAIN>`. Senza questo flag il jump host non esiste. |
| `--ssh-host-key-file <PATH>` | `BORE_SSH_HOST_KEY_FILE` | consigliato | Host key del gateway; creata al primo avvio se assente. **Persistere.** |
| `--ssh-authorized-keys-dir <DIR>` | `BORE_SSH_AUTHORIZED_KEYS_DIR` | uno dei due | Directory con le chiavi pubbliche. Riletta a ogni autenticazione: aggiungere un account non richiede riavvio. |
| `--ssh-passwords-file <PATH>` | `BORE_SSH_PASSWORDS_FILE` | uno dei due | File `username:$argon2id$...`. |
| `--ssh-advertise-address <HOST>` | `BORE_SSH_ADVERTISE_ADDRESS` | no | Hostname pubblico mostrato nei messaggi informativi. |
| `--ssh-advertise-port <PORT>` | `BORE_SSH_ADVERTISE_PORT` | no | Porta pubblica mostrata nei messaggi informativi (es. `443`). |
| `--ssh-port <PORT>` | `BORE_SSH_PORT` | no | Porta dedicata per SSH. **Senza questo flag** SSH viaggia sulla stessa porta di controllo (443): nessuna porta in più da aprire. |
| `--ssh-banner <TEXT>` | `BORE_SSH_BANNER` | no | Testo mostrato prima dell'autenticazione. |
| `--udp` + `--vhost-quic-port <PORT>` | `BORE_UDP`, `BORE_VHOST_QUIC_PORT` | no | Abilitano il percorso QUIC diretto server→provider per i provider **nativi**. Senza, tutto funziona ugualmente su TCP. |

Senza `--ssh-gateway` il comportamento del server resta identico a prima: la
funzionalità è puramente additiva.

### 1.5 Verifica del server

```bash
# 1. Il gateway risponde e presenta la sua host key
ssh -vvv -p 443 fabio@bore.tld

# 2. Fingerprint della host key, da comunicare agli operatori per la verifica
ssh-keygen -y -f /etc/bore/ssh/host_key.pem | ssh-keygen -lf -
```

Il pannello admin (`/admin/status`, sezione **Jump Hosts**) elenca gli alias
registrati, il tipo di provider, il target locale e i contatori. Non mostra mai
credenziali.

---

## 2. Avvio con il binario

### 2.1 Provider nativo `bore sshjhost` (consigliato)

Sulla VM che ospita `sshd`, con il binario `bore` installato:

```bash
bore sshjhost localhost:22 \
  --subdomain vm-test-01 \
  --to https://bore.tld \
  --secret "$BORE_SECRET" \
  --notes "VM test AWS eu-south-1" \
  --auto-reconnect
```

Da questo momento l'alias `vm-test-01.ssh.bore.tld` è attivo. Il provider nativo si
autentica con il **secret bore già in uso**; non serve un account SSH del gateway
per la VM.

Con percorso QUIC diretto e più connessioni parallele (richiede `bore server --udp`
e 443/udp aperto):

```bash
bore sshjhost localhost:22 \
  --subdomain vm-test-01 \
  --to https://bore.tld \
  --secret "$BORE_SECRET" \
  --carriers 4 \
  --udp \
  --auto-reconnect
```

Il tratto operatore→gateway resta comunque TCP/443. Se QUIC non è disponibile o
cade, la stessa sessione passa immediatamente al TCP caldo: la sessione SSH **non**
si interrompe.

`sshd` su porta non standard — porta locale e porta virtuale coincidono:

```bash
bore sshjhost localhost:2222 --subdomain legacy-01 \
  --to https://bore.tld --secret "$BORE_SECRET" --auto-reconnect

# accesso: la porta va indicata esplicitamente
ssh -p 2222 -J fabio@bore.tld:443 admin@legacy-01.ssh.bore.tld
```

Riferimento completo dei parametri:

| Argomento | Variabile | Significato |
|---|---|---|
| `<TARGET>` (posizionale) | — | Obbligatorio, `HOST:PORT` o `[IPv6]:PORT`. La porta è anche la porta virtuale dell'alias. |
| `--subdomain <LABEL>` | `BORE_SSH_JUMP_SUBDOMAIN` | Obbligatorio. Una sola etichetta DNS minuscola (`vm-test-01`, non `vm.test`). |
| `--to <ADDR>` | `BORE_SERVER` | Endpoint di controllo bore, es. `https://bore.tld`. |
| `--secret <SECRET>` | `BORE_SECRET` | Secret bore esistente. Preferire la variabile d'ambiente al flag. |
| `--insecure` | `BORE_INSECURE` | Accetta un certificato TLS self-signed sul control. |
| `--notes <TEXT>` | `BORE_NOTES` | Nota mostrata nel pannello admin. |
| `--carriers <N>` | `BORE_CARRIERS` | Connessioni TCP calde richieste (default 1) e, con `--udp`, connessioni QUIC indipendenti. Il server applica un tetto. |
| `--udp` | `BORE_PREFER_UDP` | Preferisce QUIC diretto; mantiene sempre il fallback TCP. |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | Riconnessione automatica con backoff. Consigliato sempre. |

### 2.2 Provider OpenSSH puro (nessun binario sulla VM)

Richiede un account gateway (§1.3) il cui **username coincide** con quello usato nel
comando. Il prefisso `jump/` è obbligatorio:

```bash
ssh -T -p 443 \
  -i ~/.ssh/id_ed25519_bore_provider \
  -o IdentitiesOnly=yes \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
  -R 'jump/vm-test-01:22:localhost:22' \
  vm-provider@bore.tld -- 'notes="VM eu-south-1"'
```

Variante a password (interattiva):

```bash
ssh -T -p 443 \
  -o PubkeyAuthentication=no -o PreferredAuthentications=password \
  -o ExitOnForwardFailure=yes \
  -R 'jump/vm-test-01:22:localhost:22' \
  vm-provider@bore.tld -- 'notes="provider interattivo"'
```

Porta non standard:

```bash
ssh -T -p 443 -i ~/.ssh/id_ed25519_bore_provider \
  -o ExitOnForwardFailure=yes \
  -R 'jump/legacy-01:2222:localhost:2222' \
  vm-provider@bore.tld
```

Servizio permanente con `autossh`:

```bash
AUTOSSH_GATETIME=0 autossh -M 0 -T -p 443 \
  -i ~/.ssh/id_ed25519_bore_provider \
  -o IdentitiesOnly=yes -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
  -R 'jump/vm-test-01:22:localhost:22' \
  vm-provider@bore.tld -- 'notes="VM eu-south-1"'
```

Attenzione, tre regole:

- **Non usare `-N`.** Sopprime il canale su cui il gateway scrive lo stato del
  tunnel e gli avvisi. Usare `-T`, che toglie solo il terminale.
- Questo percorso è **sempre TCP**: `udp=` e `carriers=` vengono segnalati come non
  applicabili.
- `ssh -R 22:localhost:22 bore.tld` **non** registra un jump host: è un normale
  forward pubblico numerico. Serve il prefisso `jump/`.

### 2.3 Provider nativo come servizio systemd

Il secret non deve comparire nella riga di comando:

```ini
# /etc/bore/sshjhost.env   (0640 root:bore)
BORE_SERVER=https://bore.tld
BORE_SSH_JUMP_SUBDOMAIN=vm-test-01
BORE_NOTES="vm-test-01 produzione"
BORE_CARRIERS=4
BORE_PREFER_UDP=true
BORE_AUTO_RECONNECT=true
```

```sh
#!/bin/sh
# /usr/local/libexec/bore-sshjhost   (0755 root:root)
set -eu
export BORE_SECRET="$(cat "${CREDENTIALS_DIRECTORY}/bore-secret")"
exec /usr/local/bin/bore sshjhost 127.0.0.1:22
```

```ini
# /etc/systemd/system/bore-sshjhost.service
[Unit]
Description=Bore SSH jump-host provider
After=network-online.target
Wants=network-online.target

[Service]
User=bore
EnvironmentFile=/etc/bore/sshjhost.env
LoadCredential=bore-secret:/etc/bore/credentials/sshjhost.secret
ExecStart=/usr/local/libexec/bore-sshjhost
Restart=always
RestartSec=5s
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
```

```bash
sudo install -m 0600 -o root -g root secret.txt /etc/bore/credentials/sshjhost.secret
sudo systemctl enable --now bore-sshjhost
sudo systemctl status bore-sshjhost
```

---

## 3. Avvio con il wrapper Docker

### 3.1 Server

Preparazione dei volumi (l'immagine gira come UID/GID 1000 e deve poter creare la
host key):

```bash
mkdir -p docker/certs docker/ssh/authorized_keys.d
touch docker/ssh/passwords
sudo chown -R 1000:1000 docker/ssh
chmod 0700 docker/ssh
chmod 0750 docker/ssh/authorized_keys.d
chmod 0600 docker/ssh/passwords
```

Copiare `cert.pem` e `key.pem` in `docker/certs/`, leggibili dall'UID 1000. La
directory `docker/ssh` **non** va montata in sola lettura: `host_key.pem` viene
generata al primo avvio.

Estratto di Compose (`docker/docker-compose.server.yml`) — porte e ambiente:

```yaml
services:
  bore-server:
    image: ghcr.io/manprint/bore:latest
    command: ["server"]
    restart: unless-stopped
    ports:
      - "443:7835"           # TCP: control port + TLS + gateway SSH (stesso socket)
      - "7835:7835/udp"      # STUN (già esistente)
      - "443:443/udp"        # endpoint QUIC diretto condiviso (solo se usi --udp)
      - "6000-7000:6000-7000"
    environment:
      - BORE_SECRET=${BORE_SECRET}
      - BORE_CONTROL_PORT=7835
      - BORE_UDP=true
      - BORE_VHOST_QUIC_PORT=443
      - BORE_CERT_FILE=/etc/bore/certs/cert.pem
      - BORE_KEY_FILE=/etc/bore/certs/key.pem
      - BORE_SSH_GATEWAY=true
      - BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.tld     # <-- abilita il jump host
      - BORE_SSH_ADVERTISE_ADDRESS=bore.tld
      - BORE_SSH_ADVERTISE_PORT=443
      - BORE_SSH_HOST_KEY_FILE=/etc/bore/ssh/host_key.pem
      - BORE_SSH_AUTHORIZED_KEYS_DIR=/etc/bore/ssh/authorized_keys.d
      - BORE_SSH_PASSWORDS_FILE=/etc/bore/ssh/passwords
    volumes:
      - ./certs:/etc/bore/certs:ro
      - ./ssh:/etc/bore/ssh
```

Se il gateway esisteva già, l'**unica riga nuova** è
`BORE_SSH_JUMP_BASE_DOMAIN`. Non aggiungere altre porte: TCP 443 e UDP 443 sono
socket distinti e non entrano in conflitto.

Avvio e controllo:

```bash
docker compose -f docker/docker-compose.server.yml up -d bore-server
docker compose -f docker/docker-compose.server.yml logs --tail=100 bore-server
docker compose -f docker/docker-compose.server.yml ps
```

Generare una riga password con l'immagine, senza installare il binario:

```bash
read -rsp 'Password gateway per fabio: ' PW; echo
HASH=$(printf '%s' "$PW" | docker compose -f docker/docker-compose.server.yml \
        run --rm --no-deps -T bore-server hash-password 2>/dev/null)
printf 'fabio:%s\n' "$HASH" >> docker/ssh/passwords
unset PW HASH
chmod 0600 docker/ssh/passwords && sudo chown 1000:1000 docker/ssh/passwords
```

Le chiavi pubbliche si aggiungono a caldo, senza riavviare il container:

```bash
install -m 0600 id_ed25519_bore_gateway.pub docker/ssh/authorized_keys.d/fabio
sudo chown 1000:1000 docker/ssh/authorized_keys.d/fabio
```

### 3.2 Provider nativo in Docker (immagine `bore`)

Usa `network_mode: host` così il container raggiunge l'`sshd` della macchina su
`127.0.0.1:22`:

```yaml
# docker-compose.sshjhost.yml
services:
  bore-sshjhost:
    # Tag `:client` = stesso binario dell'immagine default, ma gira come root.
    # Serve solo con --udp: con UID non-root Docker azzera le capability anche
    # in `privileged`, SO_*BUFFORCE fallisce e il path diretto resta limitato a
    # net.core.{r,w}mem_max (warning "UDP socket buffer clamped below request").
    image: ghcr.io/manprint/bore:client
    container_name: bore-sshjhost
    restart: unless-stopped
    network_mode: host
    privileged: true          # oppure: cap_add: [NET_ADMIN]
    command: ["sshjhost", "127.0.0.1:22"]
    environment:
      - BORE_SERVER=https://bore.tld
      - BORE_SECRET=${BORE_SECRET}
      - BORE_SSH_JUMP_SUBDOMAIN=vm-test-01
      - BORE_NOTES=VM test AWS eu-south-1
      - BORE_CARRIERS=4
      - BORE_PREFER_UDP=true
      - BORE_AUTO_RECONNECT=true
```

```bash
BORE_SECRET='...' docker compose -f docker-compose.sshjhost.yml up -d
docker compose -f docker-compose.sshjhost.yml logs -f
```

Senza `--udp` (`BORE_PREFER_UDP` assente) va bene anche `ghcr.io/manprint/bore:latest`
senza `privileged`: il jump host funziona identico, solo sul relay TCP. Con `--udp` e
immagine non-root il tunnel si stabilisce comunque — il path diretto resta solo limitato
in banda. Alternativa a root: alzare il tetto sull'host con
`sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216` (l'opzione `--sysctl`
di Docker non serve: `net.core.*` non è per-netns).

Target su porta non standard: `command: ["sshjhost", "127.0.0.1:2222"]`. Se il
servizio SSH da esporre è a sua volta un container sulla stessa rete, si può
togliere `network_mode: host` e usare `command: ["sshjhost", "nome-servizio:22"]`.

### 3.3 Provider OpenSSH puro in Docker (immagine `bore-ssh-client`)

L'immagine `ghcr.io/manprint/bore-ssh-client` contiene solo `openssh-client`,
`autossh`, `sshpass` e `openssl` — nessun binario bore. Per il jump host si usa la
modalità `raw`, che passa lo spec di forward così com'è:

```yaml
# compose.jump.yml
services:
  jump-provider:
    image: ghcr.io/manprint/bore-ssh-client:latest
    restart: always
    network_mode: host          # per raggiungere l'sshd dell'host su localhost:22
    volumes:
      - ./ssh:/ssh:ro           # id_key + known_hosts
    environment:
      BORE_SSH_HOST: bore.tld
      BORE_SSH_PORT: "443"
      BORE_SSH_USER: vm-provider          # DEVE coincidere con il nome file della chiave sul server
      SSH_KEY_FILE: /ssh/id_key
      TUNNEL_MODE: raw
      FORWARD_SPEC: "-R jump/vm-test-01:22:localhost:22"
      EXEC_PARAMS: 'notes="VM eu-south-1"'
      KNOWN_HOSTS_FILE: /ssh/known_hosts
      STRICT_HOST_KEY_CHECKING: "yes"
```

```bash
mkdir -p ssh
cp ~/.ssh/id_ed25519_bore_provider ssh/id_key && chmod 600 ssh/id_key
ssh-keyscan -p 443 bore.tld > ssh/known_hosts     # verificare il fingerprint prima di fidarsi
docker compose -f compose.jump.yml up -d
docker compose -f compose.jump.yml logs -f        # qui compare lo stato del tunnel
```

Variabili rilevanti del wrapper:

| Variabile | Default | Note per il jump host |
|---|---|---|
| `BORE_SSH_HOST` | — | Obbligatoria. Hostname del server bore. |
| `BORE_SSH_PORT` | `7835` | Metterla a `443` se il gateway è demultiplexato lì. |
| `BORE_SSH_USER` | `tunnel` | **Da impostare sempre**: per il jump lo username è vincolante. |
| `TUNNEL_MODE` | — | `raw` per il jump host. |
| `FORWARD_SPEC` | — | `-R jump/<alias>:<porta>:<host>:<porta>`. |
| `EXTRA_FORWARDS` | vuota | Altri `-R`/`-L` nella stessa sessione, es. più alias insieme. |
| `EXEC_PARAMS` | vuota | `notes="..."` e simili. |
| `SSH_KEY_FILE` | — | Chiave privata dentro il container (copiata e chmod 600 all'avvio). |
| `SSH_PASSWORD_FILE` | — | Alternativa a chiave, via Docker secret. |
| `KNOWN_HOSTS_FILE` | `/ssh/known_hosts` | Pinning della host key del gateway. |
| `STRICT_HOST_KEY_CHECKING` | `accept-new` | In produzione: `yes` con `known_hosts` montato. |
| `SSH_OVER_TLS` | `off` | `on` se dalla rete esce solo traffico TLS. |

Più alias da un unico container, se lo stesso account gateway li possiede tutti:

```yaml
      FORWARD_SPEC: "-R jump/vm-test-01:22:localhost:22"
      EXTRA_FORWARDS: "-R jump/vm-test-02:2222:10.0.0.9:2222"
```

Riconnessione: `autossh` riapre la sessione da solo, e il gateway lascia che la
riconnessione dello **stesso username** sostituisca la registrazione precedente,
senza attese.

---

## 4. Accesso dell'operatore

### 4.1 Comando base

```bash
ssh -J fabio@bore.tld:443 ubuntu@vm-test-01.ssh.bore.tld
```

- `fabio@bore.tld:443` → account **gateway**;
- `ubuntu@vm-test-01.ssh.bore.tld` → account **sulla VM**.

### 4.2 Configurazione consigliata `~/.ssh/config`

Con questa configurazione il comando si accorcia e la porta 443 è implicita:

```sshconfig
Host bore.tld
    HostName bore.tld
    Port 443
    User fabio
    IdentityFile ~/.ssh/id_ed25519_bore_gateway
    IdentitiesOnly yes
    StrictHostKeyChecking yes
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ForwardAgent no

Host *.ssh.bore.tld
    IdentityFile ~/.ssh/id_ed25519_target
    IdentitiesOnly yes
    ForwardAgent no
```

```bash
ssh -J bore.tld ubuntu@vm-test-01.ssh.bore.tld
```

Ancora più corto, un blocco per macchina:

```sshconfig
Host vm-test-01
    HostName vm-test-01.ssh.bore.tld
    User ubuntu
    ProxyJump bore.tld
    IdentityFile ~/.ssh/id_ed25519_target
    IdentitiesOnly yes
```

```bash
ssh vm-test-01
```

Questo blocco funziona anche con VS Code Remote-SSH, `ansible` e qualunque
strumento che usi la configurazione OpenSSH.

### 4.3 Combinazioni di credenziali

Gateway e target sono indipendenti: qualsiasi combinazione è valida.

| Gateway bore | Target VM | Comportamento |
|---|---|---|
| chiave | chiave | Consigliato. Nessun prompt. |
| chiave | password | La chiave apre il jump, poi OpenSSH chiede la password della VM. |
| password | chiave | Prompt per il gateway, poi accesso alla VM con la chiave. |
| password | password | Due prompt distinti: prima gateway, poi VM. |

Per usare la password solo sul target, disabilitare le chiavi **solo** nel blocco
del target:

```sshconfig
Host vm-test-01.ssh.bore.tld
    PubkeyAuthentication no
    PreferredAuthentications password
```

Non usare `-o PubkeyAuthentication=no` globale: influenzerebbe anche il gateway.

### 4.4 Porta non standard

```bash
ssh -p 2222 -J bore.tld admin@legacy-01.ssh.bore.tld
```

La porta deve essere **esattamente** quella pubblicata dal provider. Una richiesta
alla 22 su un alias pubblicato sulla 2222 viene rifiutata.

### 4.5 Copia file e tunnel

```bash
# scp (OpenSSH 8.0+)
scp -J bore.tld ./file.tar.gz ubuntu@vm-test-01.ssh.bore.tld:/tmp/

# rsync
rsync -av -e 'ssh -J bore.tld' ./dir/ ubuntu@vm-test-01.ssh.bore.tld:/srv/dir/

# port forward locale attraverso il jump (es. un DB sulla VM)
ssh -J bore.tld -L 5432:127.0.0.1:5432 -N ubuntu@vm-test-01.ssh.bore.tld
```

Con un blocco `Host vm-test-01` come in §4.2, basta `scp file vm-test-01:/tmp/`.

### 4.6 Host key e `known_hosts`

Sono voci separate e indipendenti:

- il **gateway** viene registrato come `[bore.tld]:443`;
- ogni **target** viene registrato sotto il proprio alias (`vm-test-01.ssh.bore.tld`,
  oppure `[legacy-01.ssh.bore.tld]:2222` per una porta non standard).

Quindi reinstallare una VM invalida solo la sua voce. Verificare il nuovo
fingerprint per un canale attendibile, poi rimuovere **solo** quella voce:

```bash
ssh-keygen -R vm-test-01.ssh.bore.tld
ssh-keygen -R '[legacy-01.ssh.bore.tld]:2222'
```

Non cancellare mai l'intero file `known_hosts`.

`ForwardAgent` non serve e non va abilitato: il client si autentica separatamente
al gateway e al target.

---

## 5. Gestione quotidiana

| Operazione | Come |
|---|---|
| Vedere gli alias attivi | Pannello admin `/admin/status`, sezione **Jump Hosts** (alias, tipo provider, target, carrier, contatori). Solo metadati operativi, mai credenziali. |
| Aggiungere un operatore | Nuovo file in `authorized_keys.d/<username>` o nuova riga password. Nessun riavvio del server. |
| Revocare un operatore | Rimuovere il file/la riga. Ha effetto sulle autenticazioni successive; chiudere le sessioni in corso se necessario. |
| Chiudere un alias | `Ctrl-C` sul processo `bore sshjhost`, oppure terminare la sessione `ssh -R`/`autossh`, oppure fermare il container. La registrazione sparisce. |
| Riconnessione dopo caduta rete | `--auto-reconnect` (nativo) o `autossh` (OpenSSH). Lo stesso identico username riprende il proprio alias; uno username diverso viene rifiutato. |
| Provider morto senza chiusura pulita | Il server se ne accorge da solo (heartbeat 20 s, reaper 60 s) e libera l'alias. Nessuna riga fantasma nel pannello. |

Chi possiede un alias:

- provider **nativi**: primo che arriva vince, finché resta connesso;
- provider **OpenSSH**: solo una riconnessione con lo **stesso identico username**
  può sostituire la propria registrazione;
- una collisione tra provider nativo e provider OpenSSH viene sempre rifiutata.

---

## 6. Risoluzione dei problemi

| Sintomo | Causa più probabile | Cosa fare |
|---|---|---|
| `Permission denied` sul gateway | Username diverso dal nome file/riga password, oppure chiave sbagliata | Verificare che `authorized_keys.d/<username>` esista con quel nome esatto; `ssh -vvv -p 443 <user>@bore.tld` |
| Login al gateway riuscito ma il jump viene rifiutato | Autenticazione valida in modalità legacy, ma username non vincolato | Usare lo username che coincide esattamente con il file chiave/riga password |
| `remote port forwarding failed` | Manca il prefisso `jump/`, alias già occupato, oppure porta/etichetta non valide | Usare `-R jump/<alias>:<porta>:host:<porta>`; verificare nel pannello se l'alias è già registrato |
| `Connection closed` subito dopo il login | Uso di `-N` | Sostituire `-N` con `-T` |
| L'alias non si raggiunge, ma il provider è connesso | Porta target sbagliata nel comando dell'operatore | Usare `-p <porta>` con la porta esatta pubblicata |
| `Permission denied` sul target | Credenziali dell'account VM, non del gateway | Verificare `~/.ssh/authorized_keys` sulla VM e l'utente indicato prima di `@` |
| `Host key changed` sul target | La VM è stata reinstallata, oppure la sua host key è cambiata | Verificare fuori banda, poi `ssh-keygen -R <alias>` (solo quella voce) |
| `Host key changed` sul gateway dopo un riavvio | `host_key.pem` non persistita | Montare/persistere il file host key e ripristinarlo dal backup |
| `sshjhost --udp` continua a usare TCP | Manca `bore server --udp`, o 443/udp chiusa, o `BORE_VHOST_QUIC_PORT` non impostata | Aprire 443/udp e configurare il server; il fallback TCP è comunque corretto e funzionante |
| Il provider OpenSSH non usa mai QUIC | Comportamento previsto | Il client OpenSSH puro è sempre TCP-only; per QUIC usare il provider nativo |
| Il container provider riparte in loop | Chiave non leggibile, host key non fidata, o alias occupato | `docker compose logs -f` sul container: il motivo è nel log del wrapper |

Comandi diagnostici utili:

```bash
ssh -vvv -p 443 fabio@bore.tld                          # autenticazione gateway
ssh -G -J bore.tld ubuntu@vm-test-01.ssh.bore.tld       # configurazione OpenSSH effettiva
docker compose -f docker/docker-compose.server.yml logs --tail=200 bore-server
```

---

## 7. Sicurezza e limiti da conoscere

- Il server bore **non vede** il traffico interno: la sessione SSH operatore↔VM è
  cifrata end-to-end. Password e chiavi del target non transitano mai in chiaro né
  finiscono nei log del server.
- Il provider **nativo** si autentica con il secret bore condiviso: chiunque
  possieda quel secret può registrare un alias libero. Se serve isolamento fra
  team, usare server bore separati.
- **Non esiste una ACL per singolo alias** in questa versione: qualunque account
  gateway correttamente autenticato può collegarsi a qualunque alias registrato. Il
  controllo di accesso vero e proprio resta quello dell'`sshd` della VM
  (`authorized_keys` del target).
- Ogni alias ha il proprio limite di connessioni concorrenti; la perdita di un
  carrier non abbatte la sessione SSH in corso.
- Il log del server registra allow/deny/open/close con alias, peer e principal, mai
  credenziali. Gli errori mostrati al client restano volutamente generici.

---

## 8. Checklist end-to-end

Server:

1. Binario/immagine con `ssh-gateway`; DNS per `bore.tld`; 443/tcp aperta (443/udp solo per QUIC).
2. `/etc/bore/ssh` con `authorized_keys.d/` e/o `passwords`, permessi corretti.
3. Server avviato con `--ssh-gateway` **e** `--ssh-jump-base-domain ssh.bore.tld`.
4. Host key persistita; fingerprint comunicato agli operatori.

Provider (una delle due):

5a. `bore sshjhost localhost:22 --subdomain vm-test-01 --to https://bore.tld --auto-reconnect`
5b. `ssh -T -p 443 -R 'jump/vm-test-01:22:localhost:22' vm-provider@bore.tld`

Operatore:

6. Account gateway con username esatto (`authorized_keys.d/fabio` o riga `fabio:`).
7. `ssh -J fabio@bore.tld:443 ubuntu@vm-test-01.ssh.bore.tld` — se serve, `-p <porta>`.
8. Alias visibile nel pannello admin, sezione **Jump Hosts**.
