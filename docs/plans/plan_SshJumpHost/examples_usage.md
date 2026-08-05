# SSH Jump Host — esempi d'uso e contratto E2E

> **Stato:** contratto operativo aggiornato alla fase 2. I comandi `sshjhost`,
> `jump/` e `BORE_SSH_JUMP_BASE_DOMAIN` sono implementati sul path TCP. `--udp`
> è accettato ma segnala il fallback TCP; il direct QUIC resta lavoro di fase 4.
> Nessuna nuova variabile/porta QUIC è prevista.
>
> **Scopo:** descrivere la configurazione prevista e fornire casi stabili da
> trasformare nei test end-to-end. Non è una prova che la funzionalità esista
> già nel binario corrente.

## 1. Risultato atteso

Un servizio SSH locale può essere registrato in due modi:

| Provider sulla VM | Comando | Autenticazione provider→bore | Trasporto server→VM |
|---|---|---|---|
| client nativo | `bore sshjhost ...` | `BORE_SECRET` attuale | TCP; `--udp` resta in fallback TCP fino alla fase 4 |
| OpenSSH puro | `ssh -R jump/...` | username + chiave/password del gateway | solo TCP dentro la sessione SSH |

Entrambi creano lo stesso nome logico nel deployment di test, per esempio
`vm-test-01.ssh.bore.0912345.xyz`. L'operatore usa OpenSSH standard:

```bash
ssh -J bore.0912345.xyz ubuntu@vm-test-01.ssh.bore.0912345.xyz
```

Il server bore termina soltanto la sessione SSH esterna verso il gateway. La
sessione SSH interna resta end-to-end tra l'operatore e `sshd` sulla VM.

## 2. Modifica al Compose di test reale

Il Compose fornito dall'owner ha già tutto ciò che serve per trasporto, TLS e
autenticazione SSH:

| Socket pubblica | Socket container | Uso attuale e futuro |
|---|---|---|
| `443/tcp` | `7835/tcp` | demux TLS/native bore/SSH gateway |
| `7835/udp` | `7835/udp` | STUN esistente |
| `443/udp` | `443/udp` | endpoint QUIC condiviso vhost/public; fase 4 aggiungerà `sshjhost` |

TCP 443 e UDP 443 sono socket indipendenti. Inoltre terminano su porte container
diverse: il mapping TCP va alla control port 7835, mentre il mapping UDP va
all'endpoint direct 443. Non esiste quindi alcun conflitto.

La sola riga nuova richiesta nel Compose di test è il namespace jump:

```diff
 services:
   bore-server:
     environment:
       - BORE_VHOST_QUIC_PORT=443
       - BORE_SSH_GATEWAY=true
       - BORE_SSH_ADVERTISE_ADDRESS=bore.0912345.xyz
       - BORE_SSH_ADVERTISE_PORT=443
+      - BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.0912345.xyz
```

Note operative:

- non aggiungere `8443/udp` e non rimuovere/modificare nessuna porta esistente;
- mantenere `BORE_VHOST_QUIC_PORT=443`: il nome è storico, ma il codice già usa
  quel listener anche per i tunnel public (`port:<N>`). `sshjhost` aggiungerà
  soltanto il namespace `jump:<alias>` allo stesso accept loop;
- `80/udp`, vhost, VPN, range `9000-9100`, log e tuning restano invariati;
- conservare `BORE_SECRET`, certificati, chiavi e token correnti senza copiarli
  nella documentazione o negli script e2e;
- se i valori di `BORE_SECRET`/`BORE_ADMIN_TOKEN` incollati nel Compose sono
  reali e il contesto in cui sono stati condivisi non è strettamente privato,
  ruotarli; preferire variabili da `.env` protetto o Docker secrets;
- il certificato control deve coprire `bore.0912345.xyz`. Non serve un nuovo
  wildcard per `*.ssh.bore.0912345.xyz`: il nome target viaggia nel messaggio
  SSH `direct-tcpip`, non in una nuova connessione TLS/DNS;
- il firewall/security group ha già le aperture richieste se consente
  `443/tcp`, `443/udp` e l'esistente `7835/udp`;
- per ProxyJump basta il record A/AAAA di `bore.0912345.xyz`. Non servono record
  per ogni `*.ssh.bore.0912345.xyz`.

Il `Dockerfile` del repository compila già con `vpn,ssh-gateway`. L'immagine di
test deve essere ricostruita includendo la fase 2; immagini precedenti non
riconoscono la nuova variabile/grammatica.

### 2.1 Preparazione dei volumi

Eseguire dalla root del repository. L'immagine runtime gira come UID/GID 1000 e
deve poter creare la host key del gateway:

```bash
mkdir -p docker/certs docker/ssh/authorized_keys.d
touch docker/ssh/passwords
sudo chown -R 1000:1000 docker/ssh
chmod 0700 docker/ssh
chmod 0750 docker/ssh/authorized_keys.d
chmod 0600 docker/ssh/passwords
```

Copiare il certificato e la chiave TLS nei percorsi montati:

```text
docker/certs/cert.pem
docker/certs/key.pem
```

La chiave privata TLS deve essere leggibile dall'UID 1000 del container. La
directory `docker/ssh` non va montata `:ro`, perché
`docker/ssh/host_key.pem` viene generata al primo avvio. Dopo l'avvio, la host
key deve restare persistente e privata: rigenerarla causerebbe un allarme
`known_hosts` a tutti gli operatori.

Avvio e controllo iniziale:

```bash
docker compose -f docker/docker-compose.server.yml up -d bore-server
docker compose -f docker/docker-compose.server.yml logs --tail=100 bore-server
```

## 3. Account classici del gateway, senza ACL separata

La regola username-bound si applica **solo** alle nuove operazioni jump:

- pubblicazione OpenSSH `-R jump/...`;
- connessione ProxyJump verso `*.ssh.bore.0912345.xyz`.

Tutte le modalità SSH gateway già esistenti continuano a ignorare lo username
come oggi. Non viene introdotto alcun file ACL e il corrente `permit=` delle
chiavi non cambia significato. Perciò ogni account jump autenticato
classicamente può pubblicare qualsiasi alias libero e collegarsi a qualsiasi
jump host registrato.

### 3.1 Account con chiave pubblica

La chiave privata resta sulla macchina dell'utente/provider. Sul server si copia
solo la chiave pubblica in un file il cui nome coincide con lo username SSH:

```bash
# Sulla macchina dell'operatore
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_bore_gateway -C 'fabio@bore-jump'

# Copiare la sola riga .pub sul server, poi sul server:
install -m 0600 /percorso/id_ed25519_bore_gateway.pub \
  docker/ssh/authorized_keys.d/fabio
sudo chown 1000:1000 docker/ssh/authorized_keys.d/fabio
```

Questo permette `fabio@bore.0912345.xyz`. Anche `fabio.pub` è un nome valido per il
file. Il confronto dello username è esatto e case-sensitive. Il commento finale
della chiave non sostituisce lo username.

Per un provider OpenSSH puro usare un account distinto, per esempio:

```text
docker/ssh/authorized_keys.d/vm-provider
```

contenente la public key della VM. La private key corrispondente resta sulla VM.

Compatibilità intenzionale: se la stessa chiave è presentata come
`nome-sbagliato@bore.0912345.xyz`, l'autenticazione legacy può ancora riuscire e i
forward vhost/public/secret esistenti restano utilizzabili, ma publish/connect
jump viene rifiutato genericamente.

### 3.2 Account con password

Il file esistente mantiene il formato `username:$argon2id$...`. Il plaintext
non viene scritto su disco. Con un binario locale:

```bash
read -rsp 'Password gateway per fabio: ' BORE_JUMP_PASSWORD; echo
BORE_JUMP_HASH=$(printf '%s\n' "$BORE_JUMP_PASSWORD" | bore hash-password)
printf 'fabio:%s\n' "$BORE_JUMP_HASH" >> docker/ssh/passwords
unset BORE_JUMP_PASSWORD BORE_JUMP_HASH
chmod 0600 docker/ssh/passwords
sudo chown 1000:1000 docker/ssh/passwords
```

Oppure usando l'immagine Compose già compilata:

```bash
read -rsp 'Password gateway per fabio: ' BORE_JUMP_PASSWORD; echo
BORE_JUMP_HASH=$(printf '%s\n' "$BORE_JUMP_PASSWORD" | \
  docker compose -f docker/docker-compose.server.yml run --rm --no-deps -T \
    bore-server hash-password 2>/dev/null)
printf 'fabio:%s\n' "$BORE_JUMP_HASH" >> docker/ssh/passwords
unset BORE_JUMP_PASSWORD BORE_JUMP_HASH
chmod 0600 docker/ssh/passwords
sudo chown 1000:1000 docker/ssh/passwords
```

Per il jump, login come `fabio` verifica esclusivamente la riga `fabio:`. Un
login con altro username può conservare il comportamento legacy corrente, ma
non ottiene un `jump_principal`.

Le password sono utili per accesso interattivo. Per provider permanenti e test
automatici sono preferibili chiavi dedicate: `sshpass`/`SSH_ASKPASS` richiedono
di tenere il segreto in un file o nell'ambiente del processo.

### 3.3 Host key del gateway

Sono diverse e indipendenti:

| Materiale | Dove risiede | Scopo |
|---|---|---|
| `/etc/bore/ssh/host_key.pem` | volume `docker/ssh` del server | identifica il gateway bore verso OpenSSH |
| public key account gateway | `docker/ssh/authorized_keys.d/<username>` | autentica operatore/provider OpenSSH al gateway |
| private key account gateway | host operatore o VM provider | mai sul server bore |
| `/etc/ssh/ssh_host_*` | VM target | identifica il vero `sshd` interno |
| `~/.ssh/authorized_keys` target | VM target | autentica l'utente finale (`ubuntu`, `root`, ecc.) |
| private key/password target | operatore | attraversa solo la sessione SSH interna cifrata |

Prima di accettare la host key dal client, verificarne il fingerprint attraverso
un canale amministrativo fidato. Dal server:

```bash
ssh-keygen -y -f docker/ssh/host_key.pem | ssh-keygen -lf -
```

Dal client, `ssh-keyscan` può raccogliere la chiave ma non sostituisce la
verifica del fingerprint:

```bash
ssh-keyscan -p 443 bore.0912345.xyz >> ~/.ssh/known_hosts
```

## 4. Configurazione OpenSSH dell'operatore

Configurazione consigliata per mantenere il comando richiesto esattamente
`ssh -J bore.0912345.xyz ...`:

```sshconfig
Host bore.0912345.xyz
    HostName bore.0912345.xyz
    Port 443
    User fabio
    IdentityFile ~/.ssh/id_ed25519_bore_gateway
    IdentitiesOnly yes
    StrictHostKeyChecking yes
    ServerAliveInterval 15
    ServerAliveCountMax 3

Host *.ssh.bore.0912345.xyz
    IdentityFile ~/.ssh/id_ed25519_vm
    IdentitiesOnly yes
    StrictHostKeyChecking yes
```

Il secondo blocco riguarda l'account sul target, non quello gateway. Non è
necessario e non è consigliato `ForwardAgent yes`.

Senza configurazione della porta si può usare la forma esplicita:

```bash
ssh -J fabio@bore.0912345.xyz:443 ubuntu@vm-test-01.ssh.bore.0912345.xyz
```

Per usare una password sul gateway, il blocco `Host bore.0912345.xyz` può invece avere:

```sshconfig
Host bore.0912345.xyz
    HostName bore.0912345.xyz
    Port 443
    User fabio
    PubkeyAuthentication no
    PreferredAuthentications password
    StrictHostKeyChecking yes
```

OpenSSH chiederà prima la password esterna di `fabio@bore.0912345.xyz` e poi, se il
target la richiede, la password interna di `ubuntu@vm-test-01...`. Sono due
account e due verifiche indipendenti.

## 5. Provider nativo `bore sshjhost`

### 5.1 Porta SSH standard, TCP

Sulla VM che espone `localhost:22`:

```bash
BORE_SERVER=https://bore.0912345.xyz \
BORE_SECRET='secret-esistente' \
bore sshjhost localhost:22 \
  --subdomain vm-test-01 \
  --notes 'vm test AWS su zona eu-south-1' \
  --auto-reconnect
```

Questo percorso non usa un account SSH del gateway: il provider si autentica
con il secret bore attuale. L'operatore deve comunque autenticarsi classicamente
al gateway e poi separatamente a `sshd` sulla VM.

### 5.2 Richiesta QUIC durante la fase TCP

```bash
BORE_SERVER=https://bore.0912345.xyz \
BORE_SECRET='secret-esistente' \
bore sshjhost localhost:22 \
  --subdomain vm-test-01 \
  --notes 'vm test AWS su zona eu-south-1' \
  --auto-reconnect \
  --udp
```

Nella fase 2 questo comando registra l'intenzione UDP, emette un warning e usa
il carrier TCP già connesso. OpenSSH operatore→gateway resta comunque TCP/443.
La fase 4 abiliterà QUIC solo sulla gamba bore server→provider, mantenendo il
carrier TCP caldo come fallback.

### 5.3 Porta target non standard

Se `sshd` ascolta su `localhost:2222`:

```bash
BORE_SERVER=https://bore.0912345.xyz BORE_SECRET='secret-esistente' \
bore sshjhost localhost:2222 \
  --subdomain vm-test-legacy \
  --auto-reconnect --udp
```

La porta virtuale coincide con quella target in v1:

```bash
ssh -p 2222 -J bore.0912345.xyz admin@vm-test-legacy.ssh.bore.0912345.xyz
```

Una richiesta alla porta 22 viene rifiutata, anche se l'alias esiste.

## 6. Provider OpenSSH puro

Questa modalità non richiede il binario `bore` sulla VM. Richiede un account
gateway classicamente associato allo username usato nel comando.

### 6.1 Chiave pubblica

```bash
ssh -T -p 443 \
  -i ~/.ssh/id_ed25519_bore_provider \
  -o IdentitiesOnly=yes \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 \
  -o ServerAliveCountMax=3 \
  -R jump/vm-test-01:22:localhost:22 \
  vm-provider@bore.0912345.xyz -- \
  'notes="vm test AWS su zona eu-south-1"'
```

Sul server la public key deve essere in
`docker/ssh/authorized_keys.d/vm-provider`; la private key resta sulla VM.

Per un servizio permanente, `autossh` può riaprire la sessione mantenendo la
stessa identità:

```bash
AUTOSSH_GATETIME=0 autossh -M 0 -T -p 443 \
  -i ~/.ssh/id_ed25519_bore_provider \
  -o IdentitiesOnly=yes \
  -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 \
  -o ServerAliveCountMax=3 \
  -R jump/vm-test-01:22:localhost:22 \
  vm-provider@bore.0912345.xyz -- \
  'notes="vm test AWS su zona eu-south-1"'
```

Una riconnessione dello stesso username può sostituire la vecchia registrazione;
uno username differente non può sottrarle l'alias. Una collisione tra provider
nativo e provider OpenSSH viene rifiutata.

### 6.2 Password

```bash
ssh -T -p 443 \
  -o PubkeyAuthentication=no \
  -o PreferredAuthentications=password \
  -o ExitOnForwardFailure=yes \
  -R jump/vm-test-01:22:localhost:22 \
  vm-provider@bore.0912345.xyz -- \
  'notes="provider interattivo"'
```

La password deve verificare la riga `vm-provider:` nel file password. Questa
forma è interattiva; per un demone unattended usare una chiave dedicata.

### 6.3 Porta non standard

```bash
ssh -T -p 443 \
  -i ~/.ssh/id_ed25519_bore_provider \
  -o ExitOnForwardFailure=yes \
  -R jump/vm-test-legacy:2222:localhost:2222 \
  vm-provider@bore.0912345.xyz
```

Accesso:

```bash
ssh -p 2222 -J bore.0912345.xyz admin@vm-test-legacy.ssh.bore.0912345.xyz
```

### 6.4 Perché `ssh -R 22:localhost:22 bore.0912345.xyz` non basta

Questo comando resta valido con il significato attuale del gateway:

```bash
ssh -p 443 -R 22:localhost:22 vm-provider@bore.0912345.xyz
```

È un **public remote forward numerico**, non registra un alias jump. Può inoltre
essere negato dal range porte/privilegi o collidere con una porta già occupata.
Per la nuova funzionalità nominata, mantenendo un client OpenSSH puro, serve il
prefisso esplicito:

```bash
ssh -p 443 -R jump/vm-test-01:22:localhost:22 vm-provider@bore.0912345.xyz
```

Questa scelta conserva byte-per-byte la grammatica e il comportamento di tutti
i forward public/vhost/secret esistenti.

## 7. Accesso dell'operatore: combinazioni di credenziali

Le credenziali esterne (gateway) e interne (target) possono essere combinate
liberamente:

| Gateway bore | Target VM | Comportamento |
|---|---|---|
| chiave | chiave | consigliato per automazione; due private key distinte possibili |
| chiave | password | la chiave apre il ProxyJump, poi OpenSSH chiede la password target |
| password | chiave | prima prompt gateway; il target usa `IdentityFile` |
| password | password | due prompt distinti, prima gateway e poi target |

Esempio chiave gateway + chiave target, con la configurazione §4:

```bash
ssh -J bore.0912345.xyz ubuntu@vm-test-01.ssh.bore.0912345.xyz
```

Esempio chiave gateway + password target: mantenere la chiave nel blocco gateway
e disabilitare le chiavi soltanto nel blocco del target.

```sshconfig
Host bore.0912345.xyz
    HostName bore.0912345.xyz
    Port 443
    User fabio
    IdentityFile ~/.ssh/id_ed25519_bore_gateway
    IdentitiesOnly yes

Host vm-test-01.ssh.bore.0912345.xyz
    PubkeyAuthentication no
    PreferredAuthentications password
```

```bash
ssh -J bore.0912345.xyz ubuntu@vm-test-01.ssh.bore.0912345.xyz
```

Evitare `-o PubkeyAuthentication=no` globale: può influenzare anche il jump host.
Nei test, mantenere sempre separate le opzioni dei due blocchi `Host`.

Il target vede l'utente `ubuntu` (o quello indicato prima di `@`); il gateway
vede l'utente esterno configurato per `bore.0912345.xyz`, per esempio `fabio`.
Bore non
riceve la password target e non possiede le private key target.

## 8. Scenari da promuovere nei test E2E

Gli identificatori seguenti sono parte del contratto e devono restare stabili.
Ogni scenario usa un gateway reale OpenSSH/russh, non un mock del solo parser.

| ID | Preparazione/azione | Risultato obbligatorio |
|---|---|---|
| `E-JH-COMPOSE` | Avviare il Compose invariato nelle porte, aggiungendo solo `BORE_SSH_JUMP_BASE_DOMAIN`. | Gateway raggiungibile su 443/TCP; endpoint condiviso presente su 443/UDP; riavvio container non cambia fingerprint; modalità correnti ancora disponibili. |
| `E-JH-NATIVE-TCP` | Provider `bore sshjhost localhost:22 --subdomain vm-test-01`; operatore esegue un comando via `ssh -J`. | Comando eseguito sul vero target; path registrato come relay TCP. |
| `E-JH-NATIVE-UDP` (fase 4) | Stesso provider con `--udp`, endpoint condiviso UDP 443 raggiungibile. | Contatore/prova `jump:` direct incrementa; sessione SSH funziona senza alterare vhost/public direct. |
| `E-JH-SHARED-UDP443` (fase 4) | Attivare contemporaneamente un vhost `--udp`, un public `local --udp` e un `sshjhost --udp`, tutti sul listener 443/UDP. | Le chiavi bare-vhost, `port:<N>` e `jump:<alias>` alimentano solo il proprio pool; tutti e tre i path direct funzionano e il server ha un solo listener QUIC. |
| `E-JH-NATIVE-FALLBACK` (fase 4) | Bloccare UDP 443 per il provider prima dell'apertura di una nuova sessione. | Alias resta disponibile; nuova sessione usa il carrier TCP caldo. |
| `E-JH-SSH-KEY` | Provider `-R jump/...` autenticato come `vm-provider` con file chiave omonimo. | Registrazione TCP e accesso ProxyJump riusciti; nessun direct QUIC dichiarato. |
| `E-JH-SSH-PASSWORD` | Stesso provider con password legata alla riga `vm-provider:`. | Registrazione e accesso riusciti, sempre TCP. |
| `E-JH-OUTER-INNER-AUTH` | Eseguire le quattro combinazioni chiave/password della §7. | Gateway e target autenticano indipendentemente; nessun segreto target compare nei log bore. |
| `E-JH-NONSTANDARD` | Registrare target 2222; connettere prima con `-p 2222`, poi senza. | 2222 riesce; richiesta 22 rifiutata genericamente. |
| `E-JH-KEY-USER-MISMATCH` | Chiave presente in file `fabio`, login come `wrong`. | Sessione legacy eventualmente autenticata, ma publish e connect jump rifiutati prima del lookup alias. |
| `E-JH-PASS-USER-MISMATCH` | Password della riga `fabio:`, login come `wrong`. | Stesso comportamento jump-only fail-closed. |
| `E-JH-LEGACY-COMPAT` | Con le credenziali mismatch precedenti, ripetere public/vhost/secret già supportati. | Risultato identico al server precedente; lo username continua a essere ignorato solo per le vecchie modalità. |
| `E-JH-NUMERIC-R-COMPAT` | Eseguire `-R 9005:localhost:22`, quindi `-R jump/vm:22:localhost:22`. | Il primo resta public port 9005; solo il secondo registra `vm.ssh...`. |
| `E-JH-SSH-TAKEOVER` | Riconnettere stesso alias con stesso username, poi con username diverso. | Stesso username sostituisce la vecchia sessione senza zombie; username diverso rifiutato. |
| `E-JH-CROSS-COLLISION` | Registrare alias nativo, poi SSH e viceversa. | Il secondo provider viene rifiutato; nessun trust domain sottrae l'altro. |
| `E-JH-NO-ALIAS-ACL` | Due account classicamente validi aprono lo stesso alias registrato. | Entrambi possono collegarsi: non esiste policy per-alias in v1. |
| `E-JH-REAPER` | Rendere half-open il provider nativo; chiudere brutalmente quello SSH. | Entrambe le entry spariscono entro la rispettiva disciplina di liveness, senza righe admin zombie. |

Per le prove password interattive usare un ambiente e2e isolato con credenziali
effimere e un driver `expect`/`SSH_ASKPASS`; non inserire password reali nei
comandi, nei log o nel repository.

## 9. Verifiche e risoluzione problemi

Comandi utili:

```bash
# Porta gateway
ssh -vvv -p 443 fabio@bore.0912345.xyz

# Configurazione OpenSSH effettiva del target
ssh -G -J bore.0912345.xyz ubuntu@vm-test-01.ssh.bore.0912345.xyz

# Porte pubblicate dal container
docker compose -f docker/docker-compose.server.yml ps

# Log server senza materiale di autenticazione
docker compose -f docker/docker-compose.server.yml logs --tail=200 bore-server
```

Diagnosi attese:

| Sintomo | Verifica |
|---|---|
| `Permission denied` sul gateway | username uguale al file/riga; public key corretta; modalità auth OpenSSH selezionata |
| Login gateway riesce ma jump è rifiutato | credenziale valida solo secondo la semantica legacy o username mismatch; manca `jump_principal` |
| `remote port forwarding failed` | usare `jump/<alias>:<porta>`; alias occupato; username non classic-bound; porta/label non valida |
| Target `Permission denied` | credenziali dell'account VM, non quelle del gateway |
| Target host key changed | verificare davvero la VM/rotazione; la chiave target è indicizzata dal nome virtuale |
| `sshjhost --udp` usa TCP | comportamento corretto della fase 2; il direct `jump:` arriva nella fase 4 |
| Provider OpenSSH non usa QUIC | comportamento previsto: il client OpenSSH puro è sempre TCP-only |

## 10. Pulizia degli esempi E2E

La chiusura normale del provider deve rimuovere la registrazione:

- `Ctrl-C` sul processo `bore sshjhost`;
- uscita/terminazione della sessione `ssh -R` o `autossh`;
- rimozione delle sole chiavi/password e host key effimere create dal test.

Non cancellare la host key del gateway di produzione. Nei test, usare una
directory temporanea dedicata e verificare che dopo ogni scenario non restino
alias, permit di connessione o righe admin zombie.
