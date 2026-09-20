# `bore transfer link`: guida operativa

Questa guida descrive l'uso del comando `bore transfer link` per pubblicare un
file, una selezione di file/cartelle oppure l'output di un produttore Unix come
link HTTPS scaricabile con browser, `curl` o `wget`.

## Modello operativo

Sul computer **A** il comando prepara una sorgente, registra un vhost
temporaneo e stampa un solo URL su `stdout`. Il processo rimane in foreground e
mantiene il link fino a `Ctrl+C`/`SIGTERM`.

Il link ha questa forma:

```text
https://transfer-<16-caratteri-casuali>.<dominio-vhost>/<nome-file>
```

Il prefisso è sempre `transfer-`; lo slug contiene 16 caratteri casuali
minuscoli (`a-z` e `0-9`). Il nome del file è un solo segmento URL codificato,
quindi spazi e caratteri Unicode non rompono il download.

Il server bore coordina il vhost e inoltra i byte: non salva il payload su
disco. Per un file normale ogni GET apre una nuova lettura; per file/cartelle
il mittente genera lo ZIP mentre lo invia. I download normali possono essere
concorrenti fino a `--max-downloads`.

### Cosa significa “crittografato”

La connessione tra A e bore usa il percorso QUIC preferenziale oppure il relay
TCP/TLS cifrato. Il traffico HTTP è però terminato dal vhost HTTPS di bore: il
server che gestisce il dominio può vedere il plaintext mentre lo inoltra. È il
modello HTTPS standard accettato da questa funzionalità; non è un protocollo
E2E cieco al gestore del dominio.

## Prerequisiti del server

Il vhost deve essere già configurato. Non viene emesso un certificato nuovo per
ogni link: tutti i link usano il certificato wildcard già associato al dominio
vhost.

Esempio di avvio del server:

```shell
bore server --control-port 7835 \
  --cert-file /etc/bore/control-cert.pem \
  --key-file /etc/bore/control-key.pem \
  --vhost-base-domain bore.example.com \
  --vhost-mode https \
  --vhost-cert-file /etc/bore/wildcard-bore.example.com.pem \
  --vhost-key-file /etc/bore/wildcard-bore.example.com.key \
  --udp --vhost-quic-port 443
```

Configurare il DNS `*.bore.example.com` verso il server. Il certificato vhost
deve coprire `*.bore.example.com`; il certificato di controllo deve invece
corrispondere all'host usato da `--to`. L'opzione `--udp` sul server abilita il
percorso QUIC; il relay TCP resta disponibile come fallback.

Se il server è dietro un proxy o usa un dominio diverso, sostituire i nomi
nell'esempio ma mantenere la stessa relazione: l'host del link deve essere
coperto dal wildcard vhost.

## Avvio rapido: un file

Sul computer A:

```shell
bore transfer link \
  --to https://relay.example.net:7835 \
  --secret "$BORE_SECRET" \
  /srv/backup/archive.tar
```

Il comando stampa l'URL su `stdout` e i log su `stderr`. Copiare l'URL senza
modificarlo. Sul computer B:

```shell
curl --fail --location --output archive.tar \
  'https://transfer-k3m8q1z7p4n6c2xd.bore.example.com/archive.tar'
```

Con `wget`:

```shell
wget --output-document=archive.tar \
  'https://transfer-k3m8q1z7p4n6c2xd.bore.example.com/archive.tar'
```

È possibile incollare lo stesso URL nella barra di un browser: la risposta usa
`Content-Disposition: attachment` e avvia il download.

Se il certificato è firmato da una CA pubblica non servono opzioni aggiuntive.
Per una CA privata, il client A usa `--ca-cert` per il collegamento di
controllo e il destinatario usa la stessa CA con `curl --cacert` o
`wget --ca-certificate`:

```shell
curl --fail --cacert /etc/bore/private-ca.pem -o archive.tar \
  'https://transfer-k3m8q1z7p4n6c2xd.bore.example.com/archive.tar'
```

`--ca-cert` aggiunge radici di fiducia senza disabilitare la verifica del nome
host. Non usare un certificato del vhost che non corrisponde al dominio del
link.

## File, cartelle e selezioni miste

- Un solo file regolare viene inviato byte per byte con il suo nome di default.
- Una cartella, più file oppure una selezione mista diventano un unico ZIP.
- Lo ZIP usa il metodo **STORED** (nessuna compressione); `ZIP64` viene usato
  quando dimensione o numero di entry lo richiede.
- Lo ZIP viene costruito in streaming durante ogni download: bore non crea un
  archivio temporaneo completo e il server non conserva il payload.
- Un download concorrente genera il proprio stream ZIP e rilegge le sorgenti.

Esempio:

```shell
bore transfer link \
  --to https://relay.example.net:7835 \
  --filename backup.zip \
  /srv/backup/myfolder /srv/backup/manifest.txt
```

Il manifest viene preparato prima che l'URL sia annunciato e viene ricontrollato
durante ogni GET. Le sorgenti devono restare stabili: modifiche, sostituzioni,
file rimossi, errori di lettura, symlink, file speciali, nomi non UTF-8,
collisioni di nomi o percorsi ZIP non sicuri fanno fallire il download e
invalidano la sorgente per la sessione. Non viene creato uno snapshot atomico;
se la cartella cambia, rilanciare il comando.

Il manifest è limitato a 100.000 entry, 32 MiB di nomi/percorso codificati e
profondità 256. Lo ZIP conserva contenuti e nomi; non promette di conservare
identità di hardlink, ACL, xattr o proprietà Unix come farebbe un backup tar.

## Opzioni di `bore transfer link`

| Opzione | Default / ambiente | Uso |
| --- | --- | --- |
| `PATH...` | obbligatorio salvo `--stdin`/`--exec` | File/cartelle da pubblicare. Un file regolare resta raw; directory o più path diventano ZIP. |
| `--to ADDR` | `https://brp.0912345.xyz` / `BORE_SERVER` | Endpoint HTTPS di controllo per registrare il vhost. |
| `--secret SECRET` | vuoto / `BORE_SECRET` | Segreto opzionale per autenticare il mittente al server. Non protegge il link da chi lo possiede. |
| `--ca-cert PATH` | radici WebPKI integrate / `BORE_CA_CERT` | Aggiunge certificati CA PEM mantenendo la verifica hostname. |
| `--relay-only` | disattivato | Salta QUIC e usa direttamente il relay TCP cifrato. |
| `--carriers N` | `1` / `BORE_CARRIERS` | Numero di carrier indipendenti; `0` sceglie automaticamente, `1..32` lo fissa. |
| `--filename NAME` | basename della sorgente o `download.zip` | Nome nel percorso URL e in `Content-Disposition`; obbligatorio con `--stdin` e `--exec`. |
| `--stdin` | disattivato | Legge uno stream binario da `stdin`; il primo GET lo consuma. Mutuamente esclusivo con `--exec`. |
| `--exec -- COMMAND ARG...` | disattivato | Avvia un comando Unix con argv letterale al primo GET; non passa da una shell. Richiede `--filename`. |
| `--max-downloads N` | `8` / `BORE_TRANSFER_MAX_DOWNLOADS` | Massimo di GET contemporanei per file/ZIP, da 1 a 256. Per stdin/exec deve essere 1. |
| `--stats-interval SECS` | `1` / `BORE_TRANSFER_STATS_INTERVAL` | Frequenza dei log di progresso, intero da 1 a 60 secondi. |
| `-v`, `-vv` | logging normale | Aumenta rispettivamente a debug/trace; `RUST_LOG` può sostituire il filtro. |

`--stdin` e `--exec` non accettano contemporaneamente path. Un comando dopo il
separatore `--` richiede esplicitamente `--exec`.

## QUIC, relay e comportamento in caso di errore

Il percorso predefinito prova QUIC UDP tra A e il server. Se non è disponibile,
bore seleziona il relay TCP cifrato prima di iniziare il body HTTP. I carrier
separati riducono l'impatto di una singola congestione e consentono di usare
più banda quando il percorso lo permette.

Per forzare il relay:

```shell
bore transfer link --relay-only \
  --to https://relay.example.net:7835 \
  /srv/backup/archive.tar
```

Il fallback non migra una risposta HTTP già iniziata. Se QUIC cade dopo
l'inizio del download, `curl`/`wget` riceve un errore: ripetere il download
normale sullo stesso URL oppure avviare una nuova sessione con `--relay-only`.
Non sono implementati resume HTTP né range parziali: una richiesta `Range`
riceve una risposta completa `200`.

`HEAD` restituisce i metadati senza consumare la sorgente. Per ZIP, stdin ed
exec è necessario un client HTTP/1.1, perché la lunghezza finale non è nota in
anticipo; curl, wget e i browser moderni lo usano normalmente.

## Stream da `stdin`: tar esterno

Usare `--stdin` quando il produttore è già gestito dalla shell:

```shell
sudo tar -cpf - myfolder | \
  bore transfer link \
    --to https://relay.example.net:7835 \
    --secret "$BORE_SECRET" \
    --stdin --filename backup.tar
```

Il lettore di stdin usa una coda limitata e applica backpressure: `tar` può
partire subito, ma si blocca quando il destinatario è più lento. Non viene
creato uno spool su disco e non si conserva una copia per un secondo download.

Per questa modalità valgono le regole seguenti:

1. un solo GET può consumare lo stream;
2. `HEAD` non lo consuma;
3. un secondo GET concorrente riceve `409` (`stream is already in use`);
4. dopo completamento, interruzione o errore, i GET successivi ricevono `410`;
5. se il download fallisce, rilanciare **sia** `tar` **sia** bore: i byte già
   letti non sono riproducibili;
6. EOF significa soltanto “la pipe è terminata”. Il processo bore non può
   conoscere il codice di uscita del programma che sta a monte della pipe.

Il `sudo` nell'esempio serve a `tar`, che deve poter leggere tutti i file. Bore
può normalmente restare con l'utente non privilegiato.

## Produttore supervisionato con `--exec`

Per verificare anche il codice di uscita del produttore, farlo avviare e
supervisionare da bore:

```shell
sudo bore transfer link \
  --to https://relay.example.net:7835 \
  --secret "$BORE_SECRET" \
  --filename backup.tar \
  --exec -- tar -cpf - myfolder
```

Questa forma è corretta anche quando `tar` deve leggere directory accessibili
solo a root: il processo figlio eredita UID, GID, gruppi supplementari e
permessi del processo `bore`, quindi `sudo bore ...` avvia `tar` con quei
privilegi. Se `sudo` non trova `tar` nel `PATH` di root, usare il percorso
assoluto, per esempio `--exec -- /usr/bin/tar -cpf - myfolder`.

`--exec`:

- è disponibile su Unix;
- non interpreta il comando con una shell: ogni argomento dopo `--` arriva
  invariato a `execve`;
- avvia il figlio solo quando il primo GET ha reclamato il link;
- legge `stdout` come payload e drena `stderr` nei log (con limite per evitare
  che un produttore rumoroso blocchi il trasferimento);
- richiede codice di uscita zero per dichiarare riuscita la sorgente;
- termina e raccoglie il gruppo di processi del produttore su `Ctrl+C`, errore
  HTTP o cancellazione;
- resta one-shot: un nuovo tentativo richiede un nuovo processo bore e un nuovo
  produttore.

### `tar -p`, proprietari e permessi

Con `tar -cpf - myfolder`, GNU tar registra normalmente nell'archivio i mode,
gli UID/GID e i timestamp che può leggere. L'opzione `-p` è soprattutto
un'opzione di **estrazione** (`--preserve-permissions`); non sostituisce i
privilegi necessari per leggere e registrare i metadati alla creazione.

Per questo `sudo bore ... --exec -- tar ...` è la forma giusta per una copia
root-level. Al ripristino, per applicare anche i proprietari numerici, eseguire
come root:

```shell
sudo tar --numeric-owner --same-owner -xpf backup.tar -C /percorso/restore
```

Qui `-p` preserva i permessi e `--same-owner` chiede di mantenere i proprietari;
senza root il sistema può rimappare o rifiutare gli UID/GID. ACL e attributi
estesi richiedono le opzioni tar esplicite (`--acls`, `--xattrs`) sia in
creazione sia in estrazione. Bore non modifica né inventa questi metadati.

## Statistiche, hash e verifica

Durante ogni GET bore scrive su `stderr` record con id della richiesta, numero
di download attivi, sorgente/percorso, byte trasferiti, tempo trascorso e
velocità. Il record finale include SHA-256 e l'esito della validazione della
sorgente. Aumentare la frequenza o il dettaglio così:

```shell
bore transfer link --stats-interval 5 -v \
  --to https://relay.example.net:7835 /srv/backup/archive.tar
```

Il comando stampa l'URL su `stdout`, quindi è possibile separarlo dai log:

```shell
url=$(bore transfer link --to https://relay.example.net:7835 archive.tar 2>transfer-link.log)
echo "$url"
```

Sul destinatario calcolare l'hash dei byte effettivamente scritti:

```shell
sha256sum archive.tar
```

Confrontare questo valore con quello riportato da bore. Il record “completato”
prova che la sorgente è terminata e che la connessione HTTP ha inviato tutti i
byte; non può certificare che il sistema operativo del destinatario li abbia
già scaricati su storage stabile. La verifica lato B deve quindi includere il
codice di uscita di curl/wget e `sha256sum`.

## Uso con Docker

La release `ghcr.io/manprint/bore:client-<version>` usa un runtime Debian slim,
esegue `/bore` come `root` e contiene GNU `tar`, `gzip`, `xz`, `zstd`, `lz4` e
i certificati CA. Il binario resta identico a quello dell'immagine normale; il
runtime aggiunge gli strumenti necessari per eseguire un produttore nel
container. Montare le sorgenti in sola lettura e usare `--workdir` (Docker non
ha l'opzione `--wd`):

```shell
docker run --pull always -i --rm --privileged --network host \
  -v "$PWD:/dir:ro" --workdir /dir \
  ghcr.io/manprint/bore:client-1.2.0-rc.8 \
  transfer link myfile myfolder \
  --to https://relay.example.net:7835
```

Usare il tag immagine che corrisponde alla release installata. L'opzione
Docker corretta è `--workdir`; `--wd` non è un'opzione Docker. `-i` mantiene
aperto stdin. Per una pipe binaria **non** aggiungere `-t`: un pseudo-TTY può
alterare o troncare i byte. `--privileged` non concede permessi di lettura oltre
quelli del filesystem montato, ma consente al client di chiedere buffer UDP più
grandi per il percorso QUIC.

### Le due modalità per un produttore

| Modalità | Comando produttore | Controllo dell'esito | Quando usarla |
| --- | --- | --- | --- |
| `--exec` nel client | Viene avviato da bore al primo GET; nel container sono disponibili `tar` e i compressori | Bore legge `stdout`, drena `stderr` e richiede codice di uscita `0`; un fallimento rende il download fallito | Backup importanti, soprattutto quando serve sapere che il produttore è terminato correttamente |
| `--stdin` | Il produttore è già avviato dalla shell e la sua stdout entra in `docker run -i` | Bore vede solo EOF: non riceve il codice di uscita del processo a monte | Pipe già esistenti o produttori che devono partire prima della richiesta HTTP |

Entrambe sono **one-shot**: un solo GET consuma lo stream. Se il download si
interrompe, rilanciare bore e il produttore; i byte già letti non vengono
conservati. Per `--stdin` usare `-i` senza `-t`.

#### `--exec` nel container (raccomandato per backup)

Il client image è già root, quindi non serve `sudo` dentro il container. Il
comando seguente supervisiona GNU tar e controlla il suo codice di uscita:

```shell
docker run --pull always -i --rm --privileged --network host \
  -v "$PWD:/wdir:ro" --workdir /wdir \
  ghcr.io/manprint/bore:client-1.2.0-rc.8 \
  transfer link --secret "$BORE_SECRET" \
  --filename films.tar --exec -- \
  tar -cpf - Inception_av1.mp4 Inception.mp4 logan_av1.mp4 logan.mp4
```

`-s` è l'abbreviazione di `--secret`, quindi la stessa forma con il segreto
fornito direttamente è:

```shell
docker run --pull always -i --rm --privileged --network host \
  -v "$PWD:/wdir:ro" --workdir /wdir \
  ghcr.io/manprint/bore:client-1.2.0-rc.8 \
  transfer link -s "$BORE_SECRET" --filename films.tar --exec -- \
  tar -cpf - Inception_av1.mp4 Inception.mp4 logan_av1.mp4 logan.mp4
```

`tar -cpf -` crea un archivio TAR senza compressione: il nome corretto è
`films.tar`. Se si vuole davvero gzip, usare `--filename films.tar.gz` insieme a
`tar -czpf -`; l'estensione da sola non abilita la compressione. Per gli altri
formati sono disponibili, per esempio, `tar -cJpf -` + `films.tar.xz`,
`tar --zstd -cpf -` + `films.tar.zst` e `tar --use-compress-program=lz4 -cpf -`
+ `films.tar.lz4`. Il ricevitore deve usare il formato corrispondente; per LZ4
usare `tar --use-compress-program=lz4 -xpf films.tar.lz4`.

L'immagine esegue il processo come root, perciò può leggere file root-only
presenti nel bind mount su una macchina Linux con Docker rootful. Con Docker
rootless o user namespace restano valide le normali regole di mapping degli
UID.

#### `--stdin` con tar eseguito sull'host

Questa forma mantiene `tar` e i suoi privilegi sul computer A e passa i byte
nel container:

```shell
sudo tar -cpf - myfolder | \
  docker run --pull always -i --rm --network host \
    ghcr.io/manprint/bore:client-1.2.0-rc.8 \
    transfer link --stdin --filename backup.tar \
    --to https://relay.example.net:7835
```

Qui `tar` può usare `sudo` e vede esattamente il filesystem dell'host, ma bore
non può sapere se `tar` è terminato con errore: EOF può significare sia fine
corretta sia fallimento del produttore. Per un backup serio preferire
`--exec`, oppure controllare separatamente il codice della pipe e confrontare
lo SHA-256 stampato da bore con quello calcolato sul destinatario.

## Ciclo di vita e sicurezza

- Il link resta valido finché il processo A resta attivo. `Ctrl+C` interrompe
  il vhost, gli stream attivi e gli eventuali produttori.
- File e ZIP possono essere scaricati più volte e in parallelo fino a
  `--max-downloads`.
- `--stdin` e `--exec` sono volutamente one-shot e accettano un solo consumer.
- Il link è un bearer token: chiunque lo possieda può scaricare il contenuto.
  Non inserirlo in log pubblici, issue, screenshot o shell history condivisa.
- `--secret` autentica la registrazione di A al server; non è richiesto al
  destinatario e non sostituisce la segretezza dell'URL.
- Il server non conserva il file, ma nel modello HTTPS standard può osservare il
  plaintext del vhost durante il relay.

## Risoluzione dei problemi

| Sintomo | Controlli e rimedio |
| --- | --- |
| Nessun URL / errore TLS | Verificare DNS wildcard, certificato vhost, nome host di `--to` e `--ca-cert`. Il certificato deve coprire il dominio del link. |
| `404 Not Found` | Copiare l'URL completo; non aggiungere query string o cambiare il segmento del nome file. |
| `409 stream is already in use` | Un altro GET sta consumando stdin/exec. Fermarlo oppure attendere la fine; la sorgente one-shot non è condivisibile. |
| `410 stream is no longer available` | stdin/exec è già terminato o fallito. Rilanciare bore e il produttore. |
| `503 download capacity full` | È stato raggiunto `--max-downloads`; attendere oppure aumentare il limite (massimo 256). |
| `503 source is unavailable` | Una sorgente ZIP è cambiata o non è più leggibile. Ripristinare la stabilità e rilanciare la sessione. |
| `502 producer could not be started` | Controllare `--filename`, il percorso assoluto del comando, permessi e `PATH` di root. |
| Download interrotto a metà | QUIC può essere caduto dopo l'inizio del body oppure una sorgente è cambiata. Ripetere il GET; per reti instabili riavviare A con `--relay-only`. |
| Tar apparentemente riuscito ma backup sospetto | Una pipe esterna non trasporta il codice di uscita di tar. Usare `--exec`, controllare i log e confrontare SHA-256. |
| Proprietari non ripristinati | Estrarre come root con `--numeric-owner --same-owner`; verificare anche user namespace/container e opzioni ACL/xattr. |
| Docker non riceve stdin | Usare `docker run -i` senza `-t`, montare la directory corretta e usare `--workdir`, non `--wd`. |

## Ricette complete

### File singolo con relay forzato

```shell
bore transfer link --relay-only \
  --to https://relay.example.net:7835 \
  --filename nightly.tar \
  /srv/backup/nightly.tar
```

```shell
curl --fail --output nightly.tar \
  'https://transfer-xxxxxxxxxxxxxxxx.bore.example.com/nightly.tar'
sha256sum nightly.tar
```

### Cartella e file in ZIP senza compressione

```shell
bore transfer link \
  --to https://relay.example.net:7835 \
  --filename support-bundle.zip \
  /var/log/myapp /etc/myapp/config.toml
```

```shell
wget --output-document=support-bundle.zip \
  'https://transfer-xxxxxxxxxxxxxxxx.bore.example.com/support-bundle.zip'
```

### Backup tar con controllo del produttore

```shell
sudo bore transfer link \
  --to https://relay.example.net:7835 \
  --filename backup.tar \
  --stats-interval 5 \
  --exec -- /usr/bin/tar -cpf - /srv/dati
```

Il destinatario deve rilanciare il comando completo se interrompe il download;
non esiste un resume per uno stream già consumato.
