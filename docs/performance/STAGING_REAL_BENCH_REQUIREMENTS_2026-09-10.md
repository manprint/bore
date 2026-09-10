# Test reali di performance e banda su staging — lato applicativo

Ambiente: `brp.0912345.xyz` su AWS, VM **t4g.micro fissa (2 vCPU ARM, 1 GiB RAM)**. Documento di **intake**: tu compili i
campi `RISPOSTA:` e mi passi i **soli secret applicativi**; io eseguo i test come **client** e produco il report.

## Scope, deciso

**Io non tocco l'ambiente.** Nessun SSH, nessun accesso AWS, nessun `sudo`, nessun sysctl, nessun riavvio del servizio,
nessuna modifica di configurazione o di Security Group. Il server bore resta com'è configurato adesso.

Quello che faccio, solo dal lato applicativo:

- creo tunnel con i tuoi secret: `bore local` (pubblico), `bore vhost`, `bore proxy` + provider secret, `ssh -R`/`-L` se
  l'ssh-gateway è attivo, `bore transfer`, `bore test-udp`;
- vario **solo i flag lato client** (quelli sotto il mio controllo, tabella al §3);
- genero carico e misuro dal lato client (goodput, TTFB, percentili, errori);
- se mi dai le credenziali admin, leggo `/admin/status` e `/admin/api/v1/*` **in sola lettura** per correlare le misure.

Conseguenza da accettare: tutto ciò che è configurazione server (`--max-conns` del server, `--udp`, `--vhost-quic-port`,
`--min-port/--max-port`, `--vhost-mode`, sysctl, buffer del kernel) è **una costante dell'esperimento**, non una variabile.
Alcune cause di un risultato staranno nei log del server: quelle te le chiedo io e me le incolli tu (§7).

---

## 1. Cosa impone la t4g.micro (vincolo fisso, non aggirabile)

Non è un dettaglio: su questa macchina il collo di bottiglia sarà quasi certamente **la macchina**, non il trasporto bore.
Il piano è costruito per distinguere le due cose, non per nasconderle.

| Vincolo | Effetto sui test | Come lo gestisco |
| --- | --- | --- |
| **Rete burstable a credito** (baseline nell'ordine di decine di Mbit/s, burst fino a qualche Gbit/s finché i crediti durano — cifra esatta da confermare sulla doc AWS del tipo istanza) | Un bulk test lungo misura **l'esaurimento dei crediti**, non bore: parte veloce e crolla a metà | Burst brevi 10–30 s con cool-down documentato **+** una corsa lunga deliberata per tracciare la curva di crollo (è un risultato utile: è quello che sentirà chi usa staging) |
| **CPU burstable a credito, 2 vCPU ARM** | TLS + AEAD + splice su 2 core piccoli: la CPU può saturare prima della rete, e i crediti CPU finiscono | Confronto TCP puro vs TLS vs QUIC per separare costo crittografico e costo di rete; corse ripetute per vedere se il secondo giro è peggiore del primo (= crediti) |
| **1 GiB di RAM** | È il limite vero della concorrenza. Ogni splice tiene buffer di copia (default 256 KiB per direzione); con `--udp` le finestre QUIC sono soffitti alti (16 MiB per stream, 256 MiB per connessione): pochi lettori lenti possono far crescere molto la memoria | Scala di concorrenza **piccola** (1/4/16/64/128 connessioni, non migliaia) e test lettori-lenti trattato come prova di pressione di memoria, con go/no-go esplicito (§6) |
| **sysctl del kernel fissi (stock)** | `net.core.rmem_max`/`wmem_max` restano al valore di default: i buffer UDP richiesti da bore (16 MiB) vengono **clampati dal kernel**, quindi il percorso QUIC diretto è limitato da `buffer/RTT` a prescindere dalle finestre configurate | Lo dichiaro come costante e non attribuisco a bore un limite che è del kernel. Il clamp è loggato dal server: mi serve quella riga di log (§7) |
| **Istanza in modalità standard o `unlimited`** | Cambia il comportamento al termine dei crediti CPU: crollo alla baseline oppure burst a sovrapprezzo | RISPOSTA: standard / unlimited / non so |

**Dichiarazione che metterò nel report:** i valori assoluti di banda misurati su t4g.micro **non si generalizzano** a
istanze più grandi. Ciò che si generalizza è il confronto relativo (TCP vs QUIC, 1 vs 2 vs 4 carrier, keep-alive vs
connessioni nuove), la forma delle curve di latenza e il punto di ginocchio in funzione della concorrenza.

---

## 2. Cosa mi serve da te (solo secret e parametri applicativi)

I segreti **non li scrivere in questo file** (finisce in git): qui scrivi solo `fornito`, e passameli in chat.

### 2.1 Endpoint e autenticazione

| Campo | Valore |
| --- | --- |
| Host di controllo (es. `brp.0912345.xyz`) | RISPOSTA: brp.0912345.xyz, pagina di admin brp.0912345.xyz/admin/status |
| Porta di controllo (default 7835) | RISPOSTA: 443 - https://to brp.0912345.xyz |
| Schema: TCP semplice oppure TLS (`https://host`) | RISPOSTA: https |
| Se TLS: certificato pubblicamente valido o mi serve `--insecure`? | RISPOSTA: certifiato valido |
| `--secret` (HMAC) | RISPOSTA: fornito SI / NO - fornito in chat |

### 2.2 Tunnel pubblici (`bore local`)

| Campo | Valore |
| --- | --- |
| Range di porte pubbliche assegnabili dal server | RISPOSTA: 9000 - 9100 |
| Posso chiedere porte specifiche o solo `0` (assegnazione automatica)? | RISPOSTA: no |
| Quanti tunnel pubblici contemporanei posso tenere | RISPOSTA: max 10 |

### 2.3 Vhost

| Campo | Valore |
| --- | --- |
| Dominio base dei vhost (es. `*.brp.0912345.xyz`) | RISPOSTA: si, corretto |
| Il wildcard DNS esiste? | RISPOSTA: SI |
| Label/subdomain che posso occupare per i test (es. `bench1..bench4`) | RISPOSTA: ok |
| Label già in uso da non toccare | RISPOSTA: non toccare |
| `--vhost-mode` configurato sul server (http / https / entrambi) | RISPOSTA: si, configurato |

### 2.4 Tunnel secret (provider + consumer)

| Campo | Valore |
| --- | --- |
| Id (`--tcp-secret-id`) liberi per i test | RISPOSTA: si |
| Il server ha `--udp` attivo? Se sì, `--vhost-quic-port` e la porta UDP è raggiungibile? | RISPOSTA: SI, upd disponibile ) |

### 2.5 SSH gateway (se attivo)

| Campo | Valore |
| --- | --- |
| Gateway attivo? Porta SSH | RISPOSTA: si|
| Username da usare | RISPOSTA: fabio |
| Auth: chiave pubblica da autorizzare (te la manda) **oppure** password | RISPOSTA: l'utente di sistema è gia abilitato ad usare ssh, quindi fabio|
| Forward consentiti (public / `vhost/<label>` / `secret/<id>` / jump host alias) | RISPOSTA: si |
| Alias jump host utilizzabile per i test | RISPOSTA: vedi tu, non andare in collisione |

### 2.6 Admin (opzionale ma vale molto)

Con l'accesso admin in lettura ottengo config del server, TX/RX per tunnel, conteggi e metriche: mi evita di dedurre a
occhio e mi fa correlare le misure client con quello che vede il server, **senza toccare la macchina**.

| Campo | Valore |
| --- | --- |
| URL admin (es. `https://.../admin/status`) | RISPOSTA: si |
| Credenziali / token (uso in sola lettura) | RISPOSTA: fornito SI |

### 2.7 Altri modi

| Campo | Valore |
| --- | --- |
| `bore transfer` consentito contro questo server? | RISPOSTA: SI |
| `bore test-udp` (diagnostica + test banda a due peer) consentito? | RISPOSTA: SI |
| `bore vpn` fuori scope? (lato server è solo relay, ma consuma la stessa macchina) | RISPOSTA: consentito |

### 2.8 Limiti operativi

| Campo | Valore |
| --- | --- |
| Finestra temporale utilizzabile | RISPOSTA: vedi tu, accettabile |
| Altri utenti/servizi usano staging nella stessa finestra? Chi avviso | RISPOSTA: no, solo io, è staging univoco per test |
| Tetto di traffico per l'intera campagna (GB) | RISPOSTA: ma direi di non superare i 50 GB |
| Connessioni concorrenti massime che non devo superare | RISPOSTA: vedi tu, in base alla tg4.micro |
| Chi posso chiamare se il servizio resta degradato | RISPOSTA: nessuno, segnala a me|

### 2.9 Il mio lato (provider/generatore)

| Campo | Valore |
| --- | --- |
| Il provider gira sulla mia workstation: banda up/down reale e RTT verso il server | RISPOSTA: ho gigabit sulla mia macchina in up/down |
| Hai un secondo punto di partenza usabile (VPS, altra macchina) per un confronto? Non obbligatorio | RISPOSTA: no |

---

## 3. Cosa posso variare io e cosa no

Distinzione centrale del piano: le variabili dell'esperimento sono solo quelle della prima colonna.

| Variabile **lato client (mia)** | Nota |
| --- | --- |
| `--carriers N` | pool di carrier; su `local`/`proxy`/`vhost` |
| `--udp` | tenta il percorso diretto QUIC; se il server non lo supporta ricade su relay (e questo è già un dato) |
| `--backend-tls` / `--backend-tls-sni` | solo vhost: TLS verso il backend locale |
| `--webserver-log` | per misurare il costo del logging |
| `--https` / `--force-https` | policy per-tunnel |
| `--max-conns` | bound lato client |
| `--auto-reconnect` | test di riconnessione |
| `--insecure` | se il cert non è valido |
| `--to` / `--local-host` / porta locale | endpoint |
| `--stun-server`, `--upnp`, `--udp-candidate`, `--nat-udp-preferred-port` | solo percorso diretto secret |
| tipo di origine locale, dimensione file, concorrenza, keep-alive, RTT/loss simulati con `tc netem` **sul mio host** | il grosso della matrice |

| Costante **lato server (intoccabile)** |
| --- |
| `--max-conns` del server, `--max-carriers`, `--min-port/--max-port`, `--udp` e `--vhost-quic-port`, `--vhost-mode`, TLS del server, sysctl del kernel, tipo istanza, Security Group, versione del binario in esecuzione |

---

## 4. Fase A — ricognizione delle capacità (la eseguo io, mi basta il secret)

Prima di misurare, stabilisco cosa il server accetta davvero. Tutto client-side, traffico trascurabile.

| Sonda | Cosa mi dice |
| --- | --- |
| connessione TCP semplice vs `https://` | se il controllo è in TLS e se il cert è valido |
| `bore local 8000 --to <host>` | il tunnel pubblico funziona, quale porta assegna, quale range |
| `bore local 8000 --udp` e lettura dei log client | il server ha `--udp`: percorso diretto QUIC stabilito, oppure fallback a relay (e per quale motivo) |
| `--carriers 4` | quanti carrier il server concede (negoziazione con il suo `--max-carriers`) |
| `bore vhost --subdomain benchN` | claim del label, `--vhost-mode` effettivo, eventuale downgrade https |
| `ssh -R vhost/benchN:80:localhost:8080 <user>@<host> -p <porta>` (senza `-N`) | il **banner informativo** del gateway riporta stato del forward, https, backend e advertise address: configurazione server dichiarata dal server stesso |
| provider secret + `bore proxy` | il percorso secret funziona; con `--udp` se il diretto si stabilisce |
| `bore test-udp --test-bandwidth` a due peer | riferimento di trasporto UDP/latenza usando il server come broker |
| `/admin/api/v1/config` (se ho il token) | conferma di tutti i punti sopra senza dedurre |

Output della fase: **scheda capacità** dell'ambiente, allegata al report. Se una capacità non c'è, i test relativi
risultano `non eseguibili`, dichiarati come tali.

---

## 5. Fase B — riferimenti, per non attribuire a bore ciò che non è suo

- **RTT e jitter** verso l'endpoint (handshake TCP ripetuto + ping se ICMP passa).
- **Tetto della mia linea**: misura up/down della workstation verso un punto esterno; se il mio uplink è inferiore alla
  baseline dell'istanza, il numero che leggo è mio, non del server. Lo dico, non lo mascherò.
- **Tetto dell'applicazione**: `curl` direttamente all'origine in locale (senza tunnel) → soffitto lato app.
- **Riferimento di trasporto attraverso il server**: `bore transfer` (bulk con verifica BLAKE3) e `bore test-udp
  --test-bandwidth`, entrambi applicativi, danno un goodput di riferimento sul percorso relay.
- **Nota d'onestà**: senza accesso alla macchina non posso avere un `iperf3` grezzo attraverso di essa. Il riferimento
  migliore disponibile è quello sopra; nel report la differenza tra "limite del percorso" e "limite di bore" sarà
  argomentata, non dichiarata come certezza.

---

## 6. Fase C — matrice di test, dimensionata per 1 GiB / 2 vCPU

Ogni riga: warm-up, N ripetizioni, percentili (non un singolo valore), cool-down tra i blocchi per non falsare i crediti.

| Dimensione | Valori |
| --- | --- |
| Modo | vhost nativo; `local` pubblico; secret provider+consumer; leg ssh-gateway; `transfer` |
| Percorso | relay TCP; diretto QUIC (`--udp`) dove supportato |
| Carrier | 1, 2, 4 (8 solo se la memoria del server lo tollera) |
| Concorrenza | 1, 4, 16, 64, 128 connessioni |
| Payload | 1 KiB, 100 KiB, 10 MiB; bulk 100–500 MiB singolo |
| Connessioni | nuove per richiesta vs keep-alive |
| Direzione | download e upload |
| Rete simulata (sul mio host) | pulita; +40/100/200 ms RTT; +0,5%/1%/2% loss — variati separatamente |
| Opzioni client | `--backend-tls` on/off; `--webserver-log` on/off; `--https` |

Metriche per riga: goodput su intervallo comune; TTFB p50/p95/p99; tempo di completamento; tasso di errore e connessioni
rifiutate; stabilità nel tempo (primo vs ultimo terzo della corsa = crediti); più, se ho l'admin, TX/RX per tunnel e
conteggi lato server.

**Robustezza, con go/no-go esplicito.** Questi test spingono la macchina verso il limite; su 1 GiB alcuni possono
degradare o far morire il processo bore del server, che **io non posso riavviare**.

| Test | Rischio | Autorizzi? |
| --- | --- | --- |
| Saturazione delle connessioni fino al rifiuto | degrado temporaneo | RISPOSTA: SI / NO |
| Molti lettori lenti con `--udp` e 1 carrier (pressione di memoria) | possibile OOM del processo server | RISPOSTA: SI / NO |
| Bulk lungo fino all'esaurimento dei crediti di rete | rallentamento dell'istanza per tutti | RISPOSTA: SI / NO |
| Raffica di riconnessioni (`--auto-reconnect`, autossh) | churn di entry lato server | RISPOSTA: SI / NO |
| Takeover di un vhost/secret già occupato (stessa identità SSH) | sposta un tunnel esistente | RISPOSTA: SI / NO |
| Blocco UDP sul mio host per forzare il fallback a relay | nessuno lato server | RISPOSTA: SI / NO |
| Spingere fino al **guasto** o fermarsi al primo degrado? | decide chi resta in piedi | RISPOSTA: fino al guasto / fermarsi al degrado |
| Se il servizio muore, chi lo riavvia e in quanto tempo | RISPOSTA: |

Condizioni di arresto immediato che applico da solo: errori > 5% su un blocco, p95 fuori scala di un ordine di grandezza,
admin non più raggiungibile, oppure tua segnalazione. Arresto = chiudo i miei client; non tocco altro.

---

## 7. Log del server: cosa ti chiederò di incollare

Non ho accesso alla macchina, quindi alcune spiegazioni stanno nei log e solo tu puoi prenderle. Su richiesta, poche righe:

- l'eventuale warning di **clamp dei buffer UDP** del kernel (spiega il tetto del percorso diretto);
- righe di rifiuto per `--max-conns` raggiunto;
- reap di entry secret/zombie e churn dei carrier durante la raffica di riconnessioni;
- eventuale OOM / riavvio del processo (`dmesg`, o il restart del container) se un test lo provoca;
- `free -m` e uso CPU durante il blocco di test di memoria, se ti è comodo.

→ Puoi recuperarli su richiesta durante la finestra di test? RISPOSTA: SI

---

## 8. Deliverable

1. **Scheda capacità** dell'ambiente (fase A).
2. `docs/performance/STAGING_REAL_BENCH_RESULTS_<data>.md`: condizioni esatte, matrice eseguita, tabelle con percentili,
   collo di bottiglia identificato per scenario (macchina / rete / crediti / bore), e **cosa non è stato misurato**.
3. Dati grezzi (CSV) + script client riutilizzabili, per rifare identici i test dopo ogni ottimizzazione.
4. Raccomandazione ordinata sulle voci di
   [VHOST_PERFORMANCE_ASSESSMENT_2026-09-07.md](../vhost/VHOST_PERFORMANCE_ASSESSMENT_2026-09-07.md) §1–6, ciascuna con la
   misura che la giustifica; e per ciascuna, se la t4g.micro è in grado di dimostrarne il beneficio o se serve un'istanza
   più grande.
5. Nota finale: cosa ruotare/cancellare dopo la campagna (secret HMAC, token admin, chiavi ssh-gw autorizzate).

---

## 9. Minimo indispensabile per partire

- **M1** §2.1 endpoint + schema + `--secret`.
- **M2** §2.2/§2.3 range porte pubbliche + dominio vhost e label che posso occupare.
- **M3** §2.5 credenziali ssh-gateway (solo se vuoi coperto anche quel leg).
- **M4** §2.8 finestra temporale, tetto GB, connessioni massime.
- **M5** §6 go/no-go sui test al limite + chi riavvia se cade.

Opzionale ad alto valore: **§2.6 accesso admin in lettura**. Con quello il report passa da "dedotto dal client" a
"correlato con il server".

---

## 10. Note libere

Ti metto il compose con cui è avviato il server, senza secret, te li metto in chat

# bore server — bridge network with explicit port forwarding.
#
#   docker compose -f docker-compose.server.yml up -d
#
# The image defaults to $BORE_IMAGE (see the justfile `repo`); override with
#   BORE_IMAGE=youruser/bore:latest docker compose ... up -d

networks:
  bore:
    driver: bridge

services:
  bore-server:
    image: ${BORE_IMAGE:-ghcr.io/manprint/bore:latest}
    #image: fabiop85/boretestdev:latest
    # To build locally instead of pulling, comment `image:` and uncomment:
    # build:
    #   context: ..
    #   dockerfile: Dockerfile
    pull_policy: always
    privileged: true
    container_name: bore-server
    command: ["server"]
    restart: always
#    network_mode: host
    networks:
      - bore
    # The control port and the tunnel port range must be forwarded explicitly,
    # and MUST match BORE_CONTROL_PORT and BORE_MIN_PORT/BORE_MAX_PORT below.
    ports:
      - "7835:7835"            # control port
      - "7835:7835/udp"        # udp
      - "9000-9100:9000-9100"  # tunnel port range
      - "443:7835"             # expose the control port over standard HTTPS
      - "80:80/udp"            # quic udp
      - "80:80/tcp"            # http vhost
      - "443:443/udp"          # quic udp https 
    environment:
      - BORE_SECRET=<REDACTED — provided in chat, kept out of the repo>
      - BORE_CONTROL_PORT=7835
      - BORE_MIN_PORT=9000
      - BORE_MAX_PORT=9100
      - BORE_MAX_CONNS=1024
      # Public domain advertised to clients (enables http(s):// addressing):
      - BORE_BIND_DOMAIN=brp.0912345.xyz
      - BORE_SSH_JUMP_BASE_DOMAIN=ssh.brp.0912345.xyz
      - BORE_ADMIN_TOKEN=<REDACTED — provided in chat, kept out of the repo>
      - BORE_CERT_FILE=/certs/cert.pem
      - BORE_KEY_FILE=/certs/key.pem
      - BORE_UDP=true
      - BORE_MAX_CARRIERS=1024
      - BORE_UDP_MAX_STREAMS=8192
      - BORE_PROXY_BUFFER_SIZE=128KiB
# leave commented for max performance (sono i default ottimali)
#      - BORE_UDP_STREAM_RECEIVE_WINDOW=16MiB
#      - BORE_UDP_CONNECTION_RECEIVE_WINDOW=256MiB
#      - BORE_UDP_SEND_WINDOW=256MiB
#      - BORE_UDP_SOCKET_RECV_BUFFER=16MiB
#      - BORE_UDP_SOCKET_SEND_BUFFER=16MiB
#      - BORE_UDP_MAX_STREAMS=4096
      - BORE_VHOST_CONFIG=/config.yml
      - BORE_VHOST_BASE_DOMAIN=brp.0912345.xyz
      - BORE_VHOST_MODE=auto
      - BORE_VHOST_HTTP_PORT=80
      - BORE_VHOST_HTTPS_PORT=443
      - BORE_VHOST_CERT_FILE=/certs/cert.pem
      - BORE_VHOST_KEY_FILE=/certs/key.pem
      - BORE_VHOST_QUIC_PORT=443
      - BORE_CONTROL_HSTS=max-age=31536000; includeSubDomains
      - BORE_VPN=true
      - BORE_VPN_POOL=10.99.0.0/16
      - BORE_VPN_MAX_LINKS=32
      - BORE_WEBSERVER_LOG=/logs
      - BORE_WEBSERVER_LOG_MAX_FILES=5
      - BORE_WEBSERVER_LOG_MAX_FILE_SIZE=50
      - BORE_SSH_GATEWAY=true
      - BORE_SSH_HOST_KEY_FILE=/etc/bore/ssh/host_key.pem
      - BORE_SSH_AUTHORIZED_KEYS_DIR=/etc/bore/ssh/authorized_keys.d
      - BORE_SSH_PASSWORDS_FILE=/etc/bore/ssh/passwords
      - BORE_SSH_ADVERTISE_ADDRESS=brp.0912345.xyz
      - BORE_SSH_ADVERTISE_PORT=443
#      - BORE_SSH_WINDOW_SIZE=16777216
#      - BORE_SSH_SOCKBUF=33554432
    volumes:
      - ./certs:/certs:ro
      - ./config.yml:/config.yml:ro
      - ./logs:/logs:rw
      - ./ssh:/etc/bore/ssh:rw

# Mount this file into the server container and point BORE_VHOST_CONFIG to it.
# Example:
#   - ./config.yml:/vhost/config.yml:ro
#   - BORE_VHOST_CONFIG=/vhost/config.yml

base_domain: brp.0912345.xyz
mode: auto

# Optional when you want HTTPS on the vhost frontend.
# cert_file: /certs/wildcard.crt
# key_file: /certs/wildcard.key

default_response_headers:
  X-Frame-Options: "SAMEORIGIN"
  X-XSS-Protection: "1; mode=block"
  Referrer-Policy: "no-referrer-when-downgrade"
  Strict-Transport-Security: "max-age=31536000; includeSubDomains"
  Permissions-Policy: "geolocation=(self), microphone=(self), camera=(self), fullscreen=(self)"
  Content-Security-Policy: "default-src * 'unsafe-inline' 'unsafe-eval' data: blob:;"
  X-Content-Type-Options: "nosniff"

reservations: []

