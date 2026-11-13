# Ottimizzazione bore — novembre 2026

Resoconto della review di `vhost`, `public` e `secret` (path relay TCP e path diretto QUIC/UDP), dei test reali su collaudo e della nuova configurazione del server.

- Versione analizzata: `bore 1.3.1-rc.1 - main - 934aa940` (immagine `ghcr.io/manprint/bore:v1.3.1-rc.1`, binario musl statico).
- Data dei test: 13 novembre 2026.
- Agente: Claude Code, modello Fable 5.2, nessun sottoagente.
- Nessuna modifica al codice, nessun commit e nessun push. Nel repository ci sono due sole variazioni, non committate: questo file e una sezione di cinque righe aggiunta in fondo a `CLAUDE.md` che rimanda qui.

Convenzione usata in tutto il documento: **[misurato]** = numero ottenuto da un test eseguito in questa sessione; **[verificato]** = letto nel codice e, dove indicato, riprodotto; **[ipotesi]** = ragionamento non confermato da una misura.

---

## 1. Sintesi

1. **rfw peggiorava la situazione ed è stato tolto dal percorso dati.** Sul path QUIC dimezzava la banda (367–458 Mbit/s contro 872–1035) e triplicava la CPU dell'host; sul relay TCP costava il 35–60 % di CPU in più e 0,2–0,4 ms di latenza. [misurato]
2. **Collaudo ora gira in `network_mode: host`**, con il vecchio mapping `443:7835` sostituito da un `REDIRECT` del kernel installato da una unit systemd. Il `REDIRECT` non ha un costo misurabile. Tutti i tunnel sono risaliti. La guida per replicare in produzione è al capitolo 5. [misurato]
3. **Host mode batte anche il bridge Docker con port publishing**: upload relay +30–50 %, QUIC +6–15 %, e niente processi `docker-proxy` (circa 5 MB ciascuno, uno per porta pubblicata e per famiglia di indirizzi: un centinaio con il range 30000–30050). [misurato]
4. **Il collo di bottiglia principale del relay è nel codice, non nella rete**: l'allocatore di musl e il modo in cui yamux scrive sul socket. Un prototipo con `mimalloc` + buffer sotto yamux + frame da 256 KiB porta il relay reale da 0,9–1,8 a 3,3–3,7 Gbit/s su una connessione (da 1,6 a 3,5 volte secondo il caso) dimezzando la CPU. Non serve passare a glibc: mimalloc su musl è più veloce di glibc e, con purge immediato, trattiene molta meno memoria. Patch in appendice A. [misurato]
5. **Il path QUIC diretto è limitato dalla CPU a circa 1 Gbit/s per connessione** (un core per lato). Su un link pulito da 1 Gbit/s o più il relay TCP è più veloce; il diretto vince su latenza e carico del server. [misurato]
6. **Trovati due difetti nuovi sul path `--udp` dei tunnel secret**, entrambi riprodotti: il consumer non apre la porta locale finché non finisce il primo tentativo diretto (7,8 s su una rete con UDP bloccato, contro 0,3 s senza `--udp`), e il provider resta bloccato su una risoluzione DNS senza timeout, anche con `--udp-no-stun`. Dettagli al capitolo 8.
7. **La review del codice ha trovato 12 difetti o debolezze** (un possibile esaurimento dei permessi `--max-conns`, un leak di token, uno stallo del driver yamux, una race sul pool diretto, e altri). Elenco al capitolo 9.

Cosa **non** è stato possibile fare: il test P2P reale dietro un NAT domestico. La sandbox blocca tutto l'UDP in uscita. Il path diretto è stato coperto con il test reale tra le due VM e con l'harness netns del repository, che usa NAT veri del kernel (capitolo 8).

---

## 2. Ambiente e metodo

| Macchina | Ruolo | Note |
|---|---|---|
| A (collaudo) | server bore | 2 vCPU, 1,8 GB RAM, kernel 6.8, Docker 29 |
| C | client di test | 6 core, 7,8 GB RAM; tutto in `/tmp/bore-review`, nessun altro servizio toccato |
| Sandbox | client dietro WiFi | RTT verso A circa 45 ms; UDP in uscita bloccato |

RTT A↔C circa 0,7 ms. Le due VM dichiarano banda simmetrica da 1 Gbit/s, ma tra loro il traffico supera abbondantemente quel valore (fino a 3,7 Gbit/s): sono probabilmente nello stesso datacenter. I numeri assoluti qui sotto valgono quindi per un link veloce e pulito; quello che conta è il confronto tra varianti misurate sullo stesso link.

Metodo: un backend TCP sintetico dietro il tunnel, un driver che misura download e upload (1 GiB, con 1 o 4 connessioni parallele), latenza di `connect+echo` e di echo su connessione aperta, CPU dei processi bore e CPU occupata dell'host in secondi per GiB trasferito. Le varianti sono state alternate nelle ripetizioni per non confondere una deriva del link con una differenza reale.

Tutti i confronti sono fatti con il **binario musl**, quello dell'immagine distribuita. Una build glibc in loopback è quasi due volte più veloce (11,5–13,5 contro 6,5–7,1 Gbit/s) e avrebbe falsato le conclusioni.

---

## 3. Verdetto su rfw

Client su C, server su A, 1 GiB per trasferimento. [misurato]

**Relay TCP, `--carriers 1`**

| | down ×1 | up ×1 | down ×4 | up ×4 | CPU host s/GiB | connect+echo p50 | echo p50 |
|---|---|---|---|---|---|---|---|
| con rfw | 1336 | 1431 | 1793 | 975 | 6,9–11,1 | 1,3 ms | 0,8 ms |
| senza rfw | 1674 | 1104 | 1956 | 2086 | 4,6–7,9 | 0,9 ms | 0,6 ms |

**QUIC `--udp`, `--carriers 1`** (Mbit/s)

| | down ×1 | up ×1 | down ×4 | up ×4 | CPU host s/GiB |
|---|---|---|---|---|---|
| con rfw | 436 | 434 | 367 | 401 | 21–35 |
| senza rfw | 936 | 911 | 1003 | 872 | 6,9–9,9 |

Con `--carriers 4` il quadro non cambia: QUIC 361–458 con rfw contro 884–1035 senza.

rfw da solo consumava 16–20 s di CPU per GiB sul path QUIC (più del server bore) e 2,4–4,5 s/GiB sul relay TCP. Con 2 vCPU sul server questo è il limite di banda. La configurazione aveva `use_splice: false` e `udp_gro_gso: false`, quindi ogni byte attraversava lo spazio utente due volte in più.

Durante i test con rfw è comparso **una volta** uno stallo: un upload di 4 × 128 MiB fermo per 406 s a circa 6 KB/s. Gli indizi puntavano al tratto locale bore-server → rfw (11,3 MB in coda di invio sul socket del carrier, 3129 drop sul socket di ricezione di rfw, rfw inattivo). Non si è più ripresentato e non ho una causa certa. [ipotesi] Con rfw fuori dal percorso quel tratto non esiste più.

**Conclusione:** rfw va tolto. La gestione delle porte che offriva si ottiene con host mode più una regola del kernel (capitolo 5).

---

## 4. Docker: bridge con port publishing contro host mode

Stessa immagine reale, porte di test, due ripetizioni (Mbit/s). [misurato]

| | relay down ×1 | relay up ×1 | relay down ×4 | relay up ×4 | udp down ×1 | udp up ×1 | udp up ×4 |
|---|---|---|---|---|---|---|---|
| host mode | 1518 / 1813 | 1100 / 1171 | 1901 / 1682 | 1377 / 1475 | 1034 / 1010 | 902 / 886 | 886 / 929 |
| bridge + DNAT | 1400 / 1491 | 729 / 898 | 1683 / 1806 | 1005 / 845 | 978 / 936 | 790 / 811 | 790 / 741 |

In bridge ogni porta pubblicata avvia un `docker-proxy` per IPv4 e uno per IPv6, ciascuno con 4,9–5,1 MB di RSS. Il range 30000–30050 più le altre porte significa oltre cento processi e circa mezzo gigabyte su una macchina da 1,8 GB.

Host mode è la scelta migliore su entrambi i fronti.

---

## 5. Migrazione a host mode: guida per replicare in produzione

### 5.1 Il problema del mapping `443:7835`

In bridge il compose pubblicava `"443:7835"`: la porta TCP 443 pubblica arrivava alla porta di controllo 7835 del container. È questo che permette ai client di usare `--to https://<host>` e al gateway SSH di rispondere sulla 443. La porta di controllo fa da sola il demultiplexing tra protocollo bore, HTTP/HTTPS dei vhost e SSH.

In host mode Docker **non pubblica e non rimappa nulla**: la sezione `ports:` viene ignorata. Senza un sostituto, sulla 443 TCP risponderebbe il listener HTTPS autonomo dei vhost e i client con `--to https://<host>` non funzionerebbero più.

Il sostituto è una regola `REDIRECT` nella tabella `nat` del kernel: riscrive la porta di destinazione da 443 a 7835 per i pacchetti diretti a un indirizzo della macchina. È l'equivalente esatto del vecchio mapping, senza processi in spazio utente.

Misura del costo (host mode diretto contro host mode + REDIRECT, stessa porta di test): nessuna differenza misurabile su banda e latenza, per esempio relay down ×1 1839/1540 contro 1529/1503 Mbit/s e latenze identiche. [misurato]

### 5.2 Unit systemd

File `/etc/systemd/system/bore-443-redirect.service`:

```ini
[Unit]
Description=bore: redirect tcp/443 to the bore control port 7835 (host-network replacement of the Docker 443:7835 port mapping)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
# Only packets addressed to this host's own addresses (never forwarded container traffic to remote :443).
ExecStart=/usr/sbin/iptables -w -t nat -A PREROUTING -p tcp -m addrtype --dst-type LOCAL --dport 443 -j REDIRECT --to-ports 7835
ExecStart=/usr/sbin/iptables -w -t nat -A OUTPUT -p tcp -m addrtype --dst-type LOCAL --dport 443 -j REDIRECT --to-ports 7835
ExecStop=/usr/sbin/iptables -w -t nat -D PREROUTING -p tcp -m addrtype --dst-type LOCAL --dport 443 -j REDIRECT --to-ports 7835
ExecStop=/usr/sbin/iptables -w -t nat -D OUTPUT -p tcp -m addrtype --dst-type LOCAL --dport 443 -j REDIRECT --to-ports 7835

[Install]
WantedBy=multi-user.target
```

Perché è scritta così:

- **`-m addrtype --dst-type LOCAL`** limita la regola ai pacchetti destinati a un indirizzo di questa macchina. Senza questo filtro, la regola in `PREROUTING` intercetterebbe anche il traffico *inoltrato* dagli altri container verso la 443 di host remoti (per esempio un container che scarica da un sito HTTPS) e lo dirotterebbe su bore. È il dettaglio più importante della unit.
- **`PREROUTING`** copre le connessioni che arrivano dalla rete. **`OUTPUT`** copre quelle generate dalla macchina stessa verso i propri indirizzi (un `curl https://<host>` lanciato sul server, o un client bore sullo stesso host).
- **`-w`** attende il lock di xtables invece di fallire se Docker sta modificando le regole nello stesso momento.
- **`Type=oneshot` + `RemainAfterExit=yes`**: la unit risulta attiva dopo aver inserito le regole e `ExecStop` le rimuove. `systemctl stop` è quindi il rollback pulito.
- Le regole non persistono al riavvio da sole: le rimette la unit perché è `enabled`.

Solo TCP: la 443 **UDP** è l'endpoint QUIC condiviso, su cui bore ascolta direttamente, e non va rediretta.

Solo IPv4: bore ascolta su `0.0.0.0`, come faceva rfw. Se in produzione serve IPv6, vanno aggiunte le stesse quattro righe con `/usr/sbin/ip6tables` e il server deve ascoltare su `::`.

### 5.3 Modifiche al compose

Quattro modifiche, tutte nel servizio `bore-server`:

```yaml
# 1) blocco `networks:` di primo livello: commentato
#networks:
#  bore:
#    driver: bridge
#    ipam:
#      config:
#        - subnet: 172.20.0.0/16
#          gateway: 172.20.0.1

services:
  bore-server:
    # 2) host networking
    network_mode: host

    # 3) rete del servizio: commentata
#    networks:
#      bore:
#        ipv4_address: 172.20.0.10

    # 4) sysctl per container: commentati (Docker li rifiuta in host mode)
#    sysctls:
#      net.ipv4.tcp_wmem: "4096 65536 16777216"
#      net.ipv4.tcp_rmem: "4096 131072 16777216"
#      net.ipv4.tcp_slow_start_after_idle: "0"
#      net.ipv4.tcp_congestion_control: "bbr"

    user: "0:0"   # già presente: serve per fare bind su 80 e 443
```

La sezione `ports:` era già commentata (le porte le gestiva rfw) e resta commentata. Tutte le variabili d'ambiente restano invariate, in particolare `BORE_CONTROL_PORT=7835`, `BORE_VHOST_HTTP_PORT=80`, `BORE_VHOST_HTTPS_PORT=443`, `BORE_VHOST_QUIC_PORT=443`, `BORE_STUN_ALT_PORT=7836`, `BORE_MIN_PORT=30000`, `BORE_MAX_PORT=30050`.

Tre punti a cui fare attenzione:

- **Sysctl.** In host mode il container usa lo stack di rete dell'host, quindi i valori TCP devono stare sull'host. Su collaudo c'erano già, in `/etc/sysctl.d/99-rfw-tunnel.conf` e `99-bore-udp.conf`. **Il file `99-rfw-tunnel.conf` non va cancellato quando si rimuove rfw**: nonostante il nome contiene i sysctl di cui bore ha bisogno. Conviene rinominarlo, per esempio `99-bore-tcp.conf`.
- **Porte privilegiate.** L'immagine gira di default come UID 1000 e in host mode `net.ipv4.ip_unprivileged_port_start` vale 1024: senza `user: "0:0"` il server esce con `Permission denied (os error 13)`. `cap_add: [NET_BIND_SERVICE]` non basta.
- **Firewall dell'host.** Il `REDIRECT` avviene prima della catena `INPUT`, che quindi vede la porta **7835**, non la 443. Un firewall locale (ufw, nftables) deve permettere la 7835/tcp. Il firewall del provider, che sta fuori dalla macchina, deve invece permettere la 443/tcp come prima.

Sysctl richiesti sull'host:

```ini
# /etc/sysctl.d/99-bore-tcp.conf
net.ipv4.tcp_wmem = 4096 65536 16777216
net.ipv4.tcp_rmem = 4096 131072 16777216
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_congestion_control = bbr
# /etc/sysctl.d/99-bore-udp.conf
net.core.rmem_max = 16777216
net.core.wmem_max = 16777216
```

Il modulo `tcp_bbr` deve essere caricato all'avvio (`echo tcp_bbr | sudo tee /etc/modules-load.d/bbr.conf`).

### 5.4 Porte in ascolto dopo la migrazione

| Porta | Protocollo | Uso |
|---|---|---|
| 7835 | TCP | controllo bore (anche HTTP/HTTPS vhost e SSH per demux) |
| 443 | TCP | `REDIRECT` verso 7835 |
| 443 | UDP | endpoint QUIC condiviso (vhost, public e jump `--udp`) |
| 7835 | UDP | STUN |
| 7836 | UDP | STUN alternativo |
| 80 | TCP | vhost HTTP |
| 30000–30050 | TCP | porte dei tunnel public |

### 5.5 Procedura

Da eseguire nella cartella del compose. Il fermo del servizio dura circa 15 secondi (14,6 s misurati su collaudo); i client con `--auto-reconnect` rientrano da soli.

```bash
# 0. inventario dei tunnel attivi PRIMA di toccare qualcosa (serve per il confronto finale)
docker logs bore-server 2>&1 | grep -E "registered|new vhost|secret" | tail -50 > ~/bore-tunnels-before.txt

# 1. backup
cp -p compose.yml compose.yml.bak-$(date +%Y%m%d)-pre-hostmode

# 2. sysctl dell'host (se non ci sono già) e modulo bbr
sudo sysctl --system
sysctl net.ipv4.tcp_congestion_control    # deve stampare: bbr

# 3. unit systemd (ancora non avviata)
sudo install -m 644 bore-443-redirect.service /etc/systemd/system/
sudo systemctl daemon-reload

# 4. modificare compose.yml come al punto 5.3

# 5. cambio: fermare rfw, ricreare bore in host mode, attivare il redirect
docker compose -f rfw/compose.yml stop
docker compose up -d
sudo systemctl enable --now bore-443-redirect
```

### 5.6 Verifica

```bash
# regole presenti, una per catena
sudo iptables -t nat -S | grep 7835
#  -A PREROUTING -p tcp -m addrtype --dst-type LOCAL -m tcp --dport 443 -j REDIRECT --to-ports 7835
#  -A OUTPUT     -p tcp -m addrtype --dst-type LOCAL -m tcp --dport 443 -j REDIRECT --to-ports 7835

systemctl is-enabled bore-443-redirect   # enabled
systemctl is-active  bore-443-redirect   # active

# bore ascolta direttamente sull'host
sudo ss -lntup | grep bore

# log: nessun errore, tutti i listener partiti
docker logs bore-server 2>&1 | grep -iE "error|listening|ready" | head -30

# da una macchina ESTERNA
curl -sI https://<host>/                  # risponde con certificato valido
curl -sI https://<un-vhost>.<host>/       # risposta del backend
ssh -p 443 <utente>@<host>                # il gateway SSH risponde (Permission denied senza credenziali)
```

Poi confrontare i tunnel registrati con l'inventario del passo 0.

### 5.7 Rollback

```bash
cp -p compose.yml.bak-<data>-pre-hostmode compose.yml
docker compose up -d
sudo systemctl disable --now bore-443-redirect
docker compose -f rfw/compose.yml start
```

### 5.8 Cosa non ho verificato

- **Il riavvio della macchina.** La unit è `enabled` e ho verificato che `systemctl restart` lascia esattamente una regola per catena, ma non ho riavviato collaudo. Al primo riavvio pianificato conviene controllare con `iptables -t nat -S | grep 7835`.
- **Un flush manuale della tabella nat** (`iptables -t nat -F`) cancellerebbe le regole senza che systemd se ne accorga. In quel caso: `sudo systemctl restart bore-443-redirect`.

### 5.9 Alternativa senza iptables

Si può eliminare del tutto il `REDIRECT` facendo ascoltare il server bore anche sulla 443 TCP in modo nativo (una seconda porta di controllo, estendendo il controllo `https_unified` in `src/server.rs:1588–1606`). È una modifica piccola ma tocca il codice e la documentazione, e non l'ho fatta: host mode funziona oggi senza modifiche.

---

## 6. Stato di collaudo

### 6.1 Cosa è cambiato sul server

| Elemento | Stato |
|---|---|
| `compose.yml` | versione host mode (capitolo 5.3), con un commento che riporta misure e rollback |
| `compose.yml.bak-20261113-pre-hostmode` | backup del compose precedente, da tenere finché non confermi |
| `compose.yml.hostmode` | copia identica al compose attuale, si può cancellare |
| `/etc/systemd/system/bore-443-redirect.service` | installata, `enabled`, `active` |
| container `rfw-tunnel` | fermo, non rimosso (`restart: unless-stopped`: non riparte al riavvio perché è stato fermato a mano) |
| `rfw/compose.yml`, `rfw/config/forwarders.yml` | non toccati |
| `/etc/sysctl.d/*` | non toccati |
| altri servizi (`seafile-13-dind`, tailscale) | non toccati |

Nessuna variabile d'ambiente del server è stata cambiata e l'immagine è la stessa di prima. La configurazione applicata è la migliore ottenibile con l'immagine attuale: ho valutato anche `BORE_PROXY_BUFFER_SIZE=65536` e ho scelto di non applicarlo (capitolo 7.5). Il passo successivo in termini di prestazioni richiede una nuova immagine (capitolo 7.6).

### 6.2 Tunnel risaliti

Dopo il cambio il log del server mostra tutti i listener attivi (controllo 7835, vhost HTTP 80, vhost HTTPS 443, STUN 7835 e 7836, QUIC condiviso 443, fast link, gateway SSH) e zero errori. Si sono registrati di nuovo **4 vhost e 1 provider secret**, ciascuno una volta sola, senza oscillazioni. I nomi sono nel messaggio di chiusura in chat e non in questo file, che sta nel repository.

Controlli funzionali fatti da C: la radice del dominio risponde 404 con certificato valido; i quattro vhost rispondono con i codici dei rispettivi backend (302, 401, 302, 401) sia in HTTPS sia in HTTP; il gateway SSH sulla 443 risponde `Permission denied`; la 7835/tcp è aperta. La radice del dominio in HTTP semplice (porta 80) risponde con un 502 vuoto: è la risposta che il listener vhost dà a un host senza provider (`src/vhost.rs:2105`), la stessa per qualsiasi sottodominio inesistente, e non dipende da host mode. Sulla 443 lo stesso caso dà 404: una piccola incoerenza tra i due percorsi.

**Limite da dichiarare:** non ho salvato l'elenco dei tunnel attivi *prima* di ricreare il container, e i log del vecchio container sono andati persi con la ricreazione. Il confronto è quindi fatto sui tunnel che si sono ripresentati, sull'attività dei weblog e sul numero di connessioni, non su un inventario precedente. Se ti aspetti un tunnel che non compare tra i cinque, il suo client non è rientrato (manca `--auto-reconnect` o era già fermo). Il passo 0 della procedura al capitolo 5.5 evita questo problema in produzione.

Il server stampa all'avvio due avvisi su `--udp-memory-budget`, già presenti prima del cambio: 1 GiB è più di metà della RAM dell'host, e il rapporto tra budget e `--max-carriers` riduce la finestra per connessione. Vedi capitolo 12.

### 6.3 Prima e dopo, sul servizio reale

Client su C, attraverso il nome pubblico del servizio, due ripetizioni (Mbit/s). [misurato]

| | Prima (rfw) | Dopo (host mode) |
|---|---|---|
| relay `--carriers 1`, down ×1 | 1336 | 1575 / 1919 |
| relay, up ×1 | 1431 | 1065 / 979 |
| relay, down ×4 | 1793 | 2147 / 2235 |
| relay, up ×4 | 975 | 1454 / 1920 |
| relay `--carriers 4`, down ×4 | 1635 | 2326 / 2239 |
| relay `--carriers 4`, up ×4 | 1482 | 2037 / 2092 |
| QUIC `--carriers 1`, down ×1 | 436 | 1042 / 1075 |
| QUIC, up ×1 | 434 | 914 / 877 |
| QUIC, down ×4 | 367 | 972 / 994 |
| QUIC, up ×4 | 401 | 916 / 910 |
| CPU host, relay (s/GiB) | 6,9–11,1 | 4,6–8,9 |
| connect+echo p50 | 1,3 ms | 0,9–1,1 ms |
| echo p50 | 0,8 ms | 0,5–0,6 ms |

Il QUIC è 2,2–2,4 volte più veloce. L'unico valore peggiore è l'upload relay su una singola connessione (1431 → 979–1065): con rfw il server bore riceveva da un socket locale, ora riceve dalla rete e il limite è il suo stesso core. È lo stesso collo di bottiglia che il prototipo del capitolo 7 rimuove (3466–3745 Mbit/s).

Tunnel secret attraverso il servizio reale in host mode (provider su C, consumer su A, una ripetizione, Mbit/s). [misurato]

| | down ×1 | up ×1 | down ×4 | up ×4 | connect+echo / echo p50 |
|---|---|---|---|---|---|
| relay `--carriers 1` | 1075 | 919 | 1145 | 1258 | 1,5 / 0,8 ms |
| relay `--carriers 4` | 1170 | 931 | 1479 | 1332 | 1,4 / 0,8 ms |
| diretto `--udp` | 998 | 929 | 1048 | 859 | 1,0 / 0,6 ms |

In questa topologia il consumer e il driver di carico girano sulla stessa macchina del server (2 vCPU), quindi il relay secret è limitato dalla CPU di A e rende meno del relay public del capitolo precedente. Rispetto alla stessa prova fatta con rfw (capitolo 8.2) la banda è equivalente e la latenza un po' più bassa.

---

## 7. Ottimizzazioni del codice misurate con un prototipo

### 7.1 Dove va il tempo sul relay

Il relay è yamux 0.13 sopra TCP/TLS. Tre costi si sommano per ogni frame:

1. **Frame da 16 KiB.** `split_send_size` non è impostato, quindi resta il default di yamux. Un GiB sono 65 536 frame.
2. **Header e corpo scritti con due syscall separate**, senza un buffer tra yamux e il socket.
3. **Un'allocazione per frame su entrambi i lati**, servita dall'allocatore di musl (mallocng), che è lento su questo schema di allocazioni.

L'immagine distribuita è `FROM rust:alpine` → `FROM scratch`, quindi musl statico con l'allocatore di default.

### 7.2 Prototipo

Tre modifiche, in un worktree separato (patch in appendice A):

- `mimalloc` come allocatore globale, con `MIMALLOC_PURGE_DELAY=0`;
- un `tokio::io::BufStream` tra yamux e il socket (256 KiB);
- `split_send_size` di yamux a 256 KiB.

### 7.3 Risultati sul link reale (A↔C, TLS, stessa variante su client e server)

Mbit/s per ripetizione; CPU in s/GiB (mediana) host A / client C / server A; RSS del server. [misurato]

| Relay `--carriers 1` | base | + mimalloc | + mimalloc + buffer/split 256 KiB |
|---|---|---|---|
| down ×1 | 1731, 1797, 1468 | 2467, 2282, 2192 | **3441, 3562, 3350** |
| up ×1 | 1087, 1026, 917 | 2173, 1914, 1840 | **3745, 3569, 3466** |
| down ×4 | 2119, 2177, 1703 | 3304, 2676, 2716 | **3878, 3334, 3011** |
| up ×4 | 1874, 1292, 1390 | 2087, 1912, 1956 | **3607, 3150, 2501** |
| CPU down ×1 | 5,09 / 5,34 / 4,48 | 3,77 / 2,50 / 3,54 | 2,76 / 1,27 / 2,37 |
| CPU up ×1 | 8,59 / 5,15 / 8,92 | 4,44 / 2,62 / 4,44 | 2,66 / 1,55 / 2,39 |
| RSS server | 12 MiB | 28 MiB | 39 MiB |

| QUIC `--carriers 1` | base | + mimalloc | + mimalloc + buffer/split |
|---|---|---|---|
| down ×1 | 999, 1020 | 1142, 1148 | 1158, 1183 |
| up ×1 | 809, 856 | 949, 1014 | 994, 942 |
| down ×4 | 974, 998 | 1154, 1108 | 1021, 964 |
| up ×4 | 871, 800 | 959, 975 | 1007, 976 |
| RSS server | 14 MiB | 35–54 MiB | 41–56 MiB |

La latenza non peggiora: connect+echo p50 0,8–1,0 ms ed echo 0,5–0,6 ms in tutte le varianti. yamux fa il flush alla fine di ogni giro, quindi il buffer non trattiene i byte interattivi.

In loopback (musl, in chiaro, mediane) la stessa scala: base 6,5–7,1 Gbit/s; solo buffer/split 6,6 (il buffer da solo non basta, il limite resta l'allocatore); solo mimalloc 12,5–15,4; tutto insieme 21,2. Le varianti da 64 KiB rendono meno di quelle da 256 KiB (11,3 Gbit/s con mimalloc).

**Lettura:** l'allocatore vale da solo +30–90 % sul relay e +13–18 % sul QUIC (mediane). Buffer e frame grandi aggiungono un altro +25–85 % sul relay ma solo dopo aver tolto il limite dell'allocatore. Il QUIC guadagna poco perché il suo limite è la crittografia e l'elaborazione dei pacchetti in quinn, non yamux.

### 7.4 glibc o musl

Non serve cambiare immagine base. Una build glibc è più veloce del musl di default (11,5 contro 6,5–7,1 Gbit/s in loopback), ma mimalloc su musl le batte entrambe (12,5–15,4) e mantiene il binario statico su `scratch`. Inoltre glibc trattiene molta più memoria dopo un picco (tabella sotto).

### 7.5 Memoria RSS

Test a onde: 5 onde da 500 connessioni × 1 MiB, RSS di server / client in MiB. [misurato]

| Variante | con connessioni aperte | dopo la chiusura |
|---|---|---|
| glibc | fino a 362 / 261 | 264 / 104 (ancora lì dopo 20 s) |
| glibc, `MALLOC_ARENA_MAX=2` | 288 / 206 | — |
| **musl di default (immagine attuale)** | 145 / 33 | **11 / 10** |
| musl + `BORE_PROXY_BUFFER_SIZE=65536` | **53 / 34** | 11 / 10 |
| musl + buffer/split 256 KiB | 144 / 140 | 10 / 10 |
| mimalloc di default | fino a 352 / 286 | 210 / 283 |
| mimalloc + buffer/split 256 KiB | 464 / 377 | 151 / 377 |
| **mimalloc + purge 0** | 273 / 273 | **33 / 32** |
| mimalloc + purge 0 + buffer/split 64 KiB + proxy buffer 64 KiB | 120 / 92 | 71 / 57 |

Tre osservazioni:

- L'immagine attuale è già ottima sulla memoria a riposo: musl restituisce tutto al sistema.
- **mimalloc senza `MIMALLOC_PURGE_DELAY=0` trattiene centinaia di MiB.** Chi lo adotta deve impostare il purge immediato (variabile d'ambiente nell'immagine oppure opzione a compile time).
- Il consumo con le connessioni aperte è dominato dai due buffer di copia da 256 KiB per connessione (`DEFAULT_PROXY_BUFFER_SIZE`, `src/shared.rs`). Con 64 KiB il server passa da 145 a 53 MiB a parità di carico.

Ho verificato sul link reale se il buffer da 64 KiB costa banda (binario attuale, relay `--carriers 1`, stessa impostazione su client e server, tre ripetizioni alternate, Mbit/s). [misurato]

| | 256 KiB (default) | 64 KiB |
|---|---|---|
| down ×1 | 1468, 1451, 1526 | 1494, 1341, 1346 |
| up ×1 | 1015, 991, 1185 | 1099, 1224, 1197 |
| down ×4 | 2146, 1959, 2111 | 2026, 2101, 1766 |
| up ×4 | 2106, 2370, 1311 | 1641, 2132, 2010 |
| CPU server s/GiB (down ×1 / up ×1) | 5,24 / 8,98 | 5,66 / 8,20 |
| latenza connect+echo / echo p50 | 1,0 / 0,6 ms | 1,0 / 0,6 ms |

Le differenze stanno dentro la dispersione tra ripetizioni: il download singolo è forse il 5 % più lento, l'upload singolo forse il 10 % più veloce. Non è un guadagno di prestazioni, è uno scambio: quasi nessun effetto sulla banda, un terzo della memoria quando ci sono centinaia di connessioni aperte.

**Non l'ho applicato su collaudo.** Con cinque tunnel e 12 MiB di RSS non c'è memoria da recuperare, il risultato sulla banda non è abbastanza netto da chiamarlo miglioramento, e avrebbe richiesto un altro riavvio del server. Ha senso su un server con poca RAM e molte connessioni contemporanee: basta aggiungere `BORE_PROXY_BUFFER_SIZE=65536` all'ambiente del container.

### 7.6 Raccomandazione

In ordine di rapporto tra beneficio e rischio:

1. **mimalloc su musl con purge 0.** Una dipendenza e tre righe in `main.rs`. Da solo: relay +30–90 %, QUIC +13–18 %, CPU dell'host −25–50 %. Costo: circa 20 MiB di RSS in più a riposo.
2. **Buffer sotto yamux + `split_send_size` 256 KiB.** Porta il relay oltre i 3,3 Gbit/s. Tocca `src/mux.rs`, quindi va validato contro gli invarianti O-1 (liveness misurata sul trasporto: `ActivityIo` finirebbe sopra il buffer), M-1, P-9 e V-14a, e contro tutta la suite, harness netns compresi. Costo: fino a 512 KiB in più per carrier.
3. **Buffer di copia da 64 KiB** dove la memoria conta (vedi sopra): si può fare già oggi con una variabile d'ambiente.

Non ho portato queste modifiche nel repository. Sono un cambiamento del percorso dati che attraversa diversi invarianti documentati e richiede i gate completi del progetto (fmt, clippy, test, harness netns, aggiornamento di README e `docs/INVARIANTS.md`), più una nuova immagine, che non posso pubblicare senza un push. La patch è piccola e i numeri sono qui: quando vuoi la implemento su un branch.

---

## 8. Path diretto QUIC/UDP

### 8.1 Public e vhost `--udp` (client → endpoint QUIC del server)

Dopo la rimozione di rfw: 870–1075 Mbit/s, sostanzialmente piatto rispetto al numero di connessioni parallele e di carrier (capitolo 6.3). Il costo è 7–10 s di CPU per GiB sull'host, cioè circa un core per lato a 1 Gbit/s. **Il limite è la CPU, non la rete**: sullo stesso link il relay TCP supera i 2 Gbit/s.

`--carriers 4` sul QUIC non aumenta la banda aggregata in questi test (1013–1040 down, 890–1021 up): con 2 vCPU sul server i quattro flussi si contendono gli stessi core.

### 8.2 Secret diretto P2P tra le due VM (IP pubblici, nessun NAT)

Provider su C, consumer su A. [misurato]

| | down ×1 | up ×1 | down ×4 | up ×4 | connect+echo / echo p50 |
|---|---|---|---|---|---|
| relay `--carriers 1` (con rfw) | 1063 | 1352 | 1255 | 1217 | 1,6 / 1,0 ms |
| relay `--carriers 1` (host mode) | 1075 | 919 | 1145 | 1258 | 1,5 / 0,8 ms |
| diretto `--udp` (prima prova) | 1007 | 864 | 1012 | 884 | 1,0 / 0,6 ms |
| diretto `--udp` (seconda prova) | 998 | 929 | 1048 | 859 | 1,0 / 0,6 ms |

Il path diretto si stabilisce in 4–5 ms (`direct_ready_ms`), toglie il server dal percorso (CPU del server circa zero) e abbassa la latenza del 40 %. La banda **non scala con le connessioni parallele** perché il diretto secret usa una sola connessione QUIC: un core per lato, circa 1 Gbit/s. `--carriers` sul secret vale solo per il fallback relay, e il client lo segnala con un warning.

Avvio con UDP funzionante: il consumer è utilizzabile dopo 0,06 s (0,01 s senza `--udp`); la discovery del provider dura 12–18 ms. [misurato]

### 8.3 Secret dalla sandbox dietro NAT WiFi

Provider su C, consumer nella sandbox. **La sandbox blocca l'UDP in uscita** (nessuna risposta STUN da tre server diversi), quindi il diretto non si è mai stabilito e tutte le misure sono in realtà sul relay. Non ho potuto testare il P2P dietro un NAT domestico reale.

Quello che il test ha comunque mostrato [misurato]:

- Il fallback è corretto e spiegato: il log dice che un lato non ha raggiungibilità STUN, suggerisce `bore test-udp`, e il tunnel funziona sul relay.
- Banda sul relay attraverso il WiFi: 300–580 Mbit/s (per lo più 420–550), latenza connect+echo p50 39–50 ms, cioè l'RTT del link. `--carriers 4` non aiuta su questo link.
- **Ritardo all'avvio: vedi 8.5.**

### 8.4 NAT reali del kernel (harness netns del repository)

Per coprire il traversal ho eseguito `scripts/udp_nat_netns_test.sh` nella sandbox: server, provider e consumer veri attraverso due NAT netfilter. [misurato]

| Scenario | Atteso | Esito |
|---|---|---|
| EIM+APDF × EIM+APDF (due router domestici tipici) | diretto | ok |
| EDM × EDM (due NAT simmetrici) | relay | ok |
| UDP bloccato | relay | ok |
| EIM+APDF × EDM | relay | ok |
| EIM+ADF × EDM | diretto | ok |
| EIM+EIF × EDM (port-forward) | diretto | ok |
| `--udp-candidate` + `--udp-no-stun` | diretto | **fallito nella sandbox, vedi 8.5** |
| `--udp-no-stun` da solo | relay | **fallito nella sandbox, vedi 8.5** |
| porta fissa × EDM | relay | ok |
| diagnostica `test-udp` (filtering APDF, ADF, round autenticato) | — | ok |
| spray: dialer, listener, spento | diretto, diretto, relay | ok |
| kill switch del piano adattivo | diretto senza piano | ok |

36 controlli passati e 5 falliti alla prima esecuzione; i 5 falliti appartengono ai due scenari con `--udp-no-stun`. Con un resolver DNS che fallisce subito dentro i namespace gli stessi due scenari passano (7 su 7). La causa è il difetto descritto qui sotto.

### 8.5 Due difetti nuovi sul path `--udp` dei secret

**A. Il consumer non apre la porta locale finché non finisce il primo tentativo diretto.** [verificato + misurato]

`src/secret.rs:1318`: `negotiate_direct_consumer` viene atteso *prima* che il listener locale inizi ad accettare. Con UDP funzionante sono pochi millisecondi. Su una rete con UDP bloccato si sommano il budget STUN (3,6 s), il round di check (0,75 s) e il budget di connessione QUIC (3 s):

| Consumer | relay | `--udp` |
|---|---|---|
| rete con UDP bloccato (sandbox) | 0,30–0,40 s | **7,74–7,78 s** |
| rete con UDP funzionante (A) | 0,01 s | 0,06 s |

Il piano ricevuto dal server dice `relay-first`, ma riguarda l'ordine dei candidati, non l'apertura del listener. La macchina per fare l'upgrade in background esiste già (`starting udp upgrade attempt #1` parte dopo): basta aprire subito il listener sul relay e far girare anche il primo tentativo in background. Severità media: nessuna perdita di dati, ma quasi 8 secondi di servizio non disponibile a ogni avvio e a ogni riconnessione su reti aziendali o filtrate.

**B. Il provider resta bloccato su una risoluzione DNS senza timeout, anche con `--udp-no-stun`.** [verificato + riprodotto]

`src/client.rs:734` chiama `offer_provider_candidates` dentro il costruttore del client, dopo che il tunnel è già registrato sul server e prima che il provider inizi a servire. La funzione (`src/client.rs:2566`) risolve sempre i nomi della catena STUN (`stun.cloudflare.com`, due `stun.l.google.com`, il server bore) con `resolve_live_stun_targets` (`src/holepunch.rs:2305`), in sequenza e senza timeout. Tre conseguenze:

- `--udp-no-stun` promette di saltare STUN, ma le risoluzioni DNS avvengono comunque;
- con un DNS lento o irraggiungibile il provider risulta registrato ma non serve nessuna connessione, **neanche sul relay**, per decine di secondi. Nell'harness il provider non ha mai superato la riga `provider UDP candidate discovery configured`;
- in quel periodo non partono gli heartbeat, quindi oltre la soglia di liveness il server può considerare morto il provider (questo terzo punto è dedotto dal codice, non riprodotto). [ipotesi]

Nella sandbox il resolver è un indirizzo instradato ma non raggiungibile dai namespace, quindi ogni query va in timeout: è l'ambiente che ha fatto emergere il difetto. Su una macchina con il resolver su loopback la query fallisce subito e l'harness passa, il che spiega perché non è mai stato visto.

Correzione proposta: non risolvere nulla quando `no_stun` è attivo; mettere un timeout complessivo alla risoluzione (il budget STUN di 4 s esiste già, va esteso al DNS); spostare la discovery fuori dal costruttore, in background, come per il consumer. Severità media. Nel frattempo, su host con DNS inaffidabile: `--stun-server <IP>:<porta>` evita la risoluzione dei nomi pubblici.

Lo stesso schema vale per il rinnovo (`src/client.rs:1982`), che gira dentro il loop che accetta le connessioni: è il punto 4 del capitolo 9.

### 8.6 Nota sul fallback STUN verso il server

La catena STUN del client è: server STUN pubblici, poi il server bore all'indirizzo di `--to` come ultimo fallback (`src/holepunch.rs:2227–2254`). Con `--to https://<host>` quel fallback punta alla **443 UDP**, che in questa installazione è l'endpoint QUIC e non STUN (STUN è su 7835 e 7836). Il fallback quindi non può rispondere. Con i server pubblici raggiungibili non ha effetto; dove sono filtrati, il client resta senza candidato riflessivo pur avendo un server STUN funzionante a disposizione. Severità bassa. Rimedio lato client: `--stun-server <host>:7835`. [verificato nel codice; l'impatto sul probe di filtering non è stato misurato]

---

## 9. Review del codice: bug, race, leak, stalli

Tutti i punti sono stati letti nel codice. Quelli marcati *riprodotto* sono stati anche osservati in un test.

| # | Severità | Difetto | Dove |
|---|---|---|---|
| 1 | Alta | **Handshake TLS dei vhost senza timeout mentre tiene un permesso `--max-conns`.** Chi apre la connessione HTTPS e non completa l'handshake occupa uno slot e un file descriptor per sempre. Con 512 connessioni di questo tipo (il limite configurato su collaudo) il server rifiuta ogni nuovo ingresso, per tutti i tipi di tunnel. L'edge public ha già il timeout sullo stesso passo (`edge.rs:157`). Correzione: `timeout(NETWORK_TIMEOUT, acceptor.accept(stream))`. | `vhost.rs:2027`, permesso in `server.rs:1675` |
| 2 | Media | **Leak del token dei carrier.** Il `TokenGuard` viene costruito dopo `control.send(CarrierToken).await?`: se l'invio fallisce, token e sender restano in `pending_carriers` per tutta la vita del processo. `server.rs:3062–3067` e `ssh_jump.rs:575–576` usano già l'ordine corretto. | `secret.rs:462–470`, `vhost.rs:1126–1139` |
| 3 | Media | **Il driver yamux si ferma se il canale delle connessioni in ingresso è pieno.** `drive()` attende `inbound_tx.send` (capacità 32) senza continuare a fare polling della connessione. Se chi possiede l'`Acceptor` smette di leggerlo, alla trentatreesima connessione in attesa si blocca **ogni** stream di quel carrier, non solo i nuovi. | `mux.rs:623` |
| 4 | Media | **L'accettazione del piano dati condivide il loop con lavoro di controllo lento**, ed è ciò che rende raggiungibile il punto 3. Nel client, `listen` attende la discovery STUN, risoluzioni DNS a ogni offerta o rinnovo del path diretto, e invii di rinnovo senza limite di tempo; nel server, `serve_consumer` attende `broker_udp`; nel consumer secret, `Proxy::listen` attende `check_reflexive_port`. Durante ognuna di queste attese non vengono servite nuove connessioni. *Riprodotto* nelle forme descritte al capitolo 8.5. Correzione strutturale: accettazione degli stream in un task proprio, lavoro di controllo nel loop di controllo. | `client.rs:1508–2067`, `client.rs:734`, `client.rs:1982`, `secret.rs:1318` |
| 5 | Bassa | **Race install-dopo-close su `DirectPool`.** L'accept QUIC fa `registry.get` e poi `entry.direct.install`; una deregistrazione che cade in mezzo lascia la connessione installata in una entry morta, tenuta viva dai monitor e dai keep-alive, con uno slot di `--udp-memory-budget` occupato. Finestra stretta. Correzione: un flag impostato da `close_all()` che fa rifiutare `install`. | `vhost.rs:580` e i tre rami del registry QUIC |
| 6 | Media | **Nessun failover tra carrier sull'apertura relay di public e vhost**, mentre il secret ha `open_with_failover`. Il public inoltre chiama `pool.pick()` prima di sapere se userà il path diretto e scarta la connessione se nessun carrier TCP è vivo, anche quando esiste una connessione diretta. | `server.rs:3512`, `open_relay_carrier` in `vhost.rs` |
| 7 | Bassa | **Log sincrono per connessione sui percorsi caldi**: `info!("new connection")` nel loop di accept di `serve_tunnel` e in `spawn_handle`, `info!` nell'accept del consumer secret, `warn!` per ogni connessione scartata a capacità piena. Il writer su stderr è sincrono: sotto raffica aggiunge latenza a ogni accept. Portarli a `debug` o campionarli. | `server.rs`, `client.rs`, `secret.rs` |
| 8 | Bassa | **Il dial diretto risolve di nuovo il DNS** invece di usare l'indirizzo del peer della connessione di controllo. Con più record A può raggiungere un server che non possiede il nonce del tunnel. | `resolve_direct_server_addr` |
| 9 | Media | **Nessun limite agli handshake QUIC concorrenti non autenticati** e nessuna validazione dell'indirizzo (retry). Un mittente può far spendere CPU al server sulla 443 UDP senza credenziali. | accept dell'endpoint QUIC condiviso |
| 10 | Bassa | **Dettagli vhost:** `head.clone()` incondizionato; `insecure_tls_connector()` ricostruito a ogni connessione con `--backend-tls`; marker e testa della richiesta inviati in due frame yamux separati; il percorso con header iniettati usa due buffer da 256 KiB azzerati in anticipo e un flush per chunk. | `vhost.rs` |
| 11 | Media | **`max_conns` per tunnel è solo mostrato**, non applicato. Il limite reale è il semaforo globale del server: un'ondata su un tunnel public consuma la capacità di tutti gli altri. | `server.rs` |
| 12 | Media | **Avvio lento e blocco DNS sul path `--udp` dei secret** (capitolo 8.5, A e B). *Riprodotto.* | `secret.rs:1318`, `client.rs:734`, `client.rs:2566` |

Deadlock veri e propri non ne ho trovati. I punti 3 e 4 insieme sono la cosa che ci si avvicina di più: uno stallo dell'intero carrier che si risolve da solo quando il lavoro di controllo termina, ma che con un DNS bloccato può durare a lungo.

Leak di memoria in senso stretto: solo il punto 2 (cresce di un elemento per ogni invio fallito) e il punto 5 (una connessione QUIC per ogni race persa). Entrambi lenti, entrambi senza limite superiore.

---

## 10. Semplificazioni possibili

- **Un solo helper di splice** al posto dei quattro blocchi duplicati in `serve_tunnel` e dei quattro punti di chiamata in `handle_connection` del client.
- **Un solo helper install/monitor** per i tre rami del registry QUIC (vhost, public, jump), fondendo l'admission watcher nel monitor: un task per connessione diretta invece di due. Chiude anche la race del punto 5.
- **Il pump dei carrier avvia direttamente gli handler**, eliminando un passaggio su canale.
- **`listen` del client** (circa 690 righe, 12 rami di `select!`) diviso in un task di accept e un attore di controllo. Risolve i punti 3, 4 e 12 alla radice.
- **Un adattatore che riscrive la testa della risposta** permetterebbe al percorso con header iniettati di riusare `copy_bidirectional_with_sizes`. Va controllato contro l'invariante sul flush (`docs/vhost/VHOST_INJECTED_FLUSH_FIX.md`) prima di proporlo: lì il comportamento attuale è il risultato di un bug già corretto una volta.

---

## 11. Come avviare i client per le migliori prestazioni

Regole valide per tutti:

- **`--to https://<host>`**: TLS sulla 443, passa ovunque e sul server arriva direttamente alla porta di controllo.
- **`--auto-reconnect`** sempre, per i servizi che devono restare su. È ciò che ha fatto rientrare da soli i tunnel durante il cambio a host mode.
- **Segreto da variabile d'ambiente** (`BORE_SECRET`) e non da riga di comando, dove è visibile in `ps`.
- **Relay o `--udp`?** Su un link pulito da 1 Gbit/s o più il relay TCP è più veloce (1,5–2,2 Gbit/s contro circa 1) e costa meno CPU. `--udp` conviene quando conta la latenza, quando si vuole togliere carico al server (secret diretto) o quando il link perde pacchetti: ogni connessione ha il suo stream QUIC e una perdita non blocca le altre. Quest'ultimo vantaggio discende dal progetto e dalle misure storiche in `docs/INVARIANTS.md`; in questa sessione non ho potuto misurarlo perché l'unico link con perdite a disposizione, il WiFi della sandbox, aveva l'UDP bloccato.
- **Prima di usare `--udp` su una rete nuova: `bore test-udp --to https://<host>`.** Dice subito se l'UDP esce e che tipo di NAT c'è.

### 11.1 Public (`bore local`)

```bash
# default consigliato: relay, traffico con più connessioni contemporanee
BORE_SECRET=... bore local 8080 --to https://<host> --carriers 4 --auto-reconnect

# servizio con poche connessioni o un solo flusso: un carrier basta
BORE_SECRET=... bore local 8080 --to https://<host> --auto-reconnect

# link con perdite o priorità alla latenza per connessione
BORE_SECRET=... bore local 8080 --to https://<host> --udp --carriers 2 --auto-reconnect
```

- `--carriers 4` aiuta quando ci sono più connessioni in parallelo: sul servizio reale l'upload ×4 passa da 1454–1920 a 2037–2092 Mbit/s. Su un flusso singolo non cambia nulla, perché una connessione proxata viaggia su un solo carrier. Il server accetta al massimo `--max-carriers` (8 su collaudo).
- `--udp` sul public non usa STUN né hole punching: il client apre le connessioni QUIC verso il server. Richiede `--udp` sul server. I flag di NAT (`--upnp`, `--udp-candidate`, …) qui non servono e producono un warning.
- Con `--udp`, più di 2 carrier non hanno aumentato la banda aggregata nei test (limite di CPU sul server a 2 vCPU).

### 11.2 Vhost (`bore vhost`)

```bash
# default consigliato: relay con carrier automatici
BORE_SECRET=... bore vhost --subdomain app --id app-prod http://127.0.0.1:8080 \
  --to https://<host> --carriers 0 --auto-reconnect

# backend HTTPS
BORE_SECRET=... bore vhost --subdomain app --id app-prod https://127.0.0.1:8443 \
  --to https://<host> --backend-tls --carriers 0 --auto-reconnect
```

- **`--carriers 0` (auto)** è la scelta giusta per il web: parte con un carrier e il server ne aggiunge fino a 4 quando vede trasferimenti grossi, così un download non rallenta le richieste piccole. Per un servizio che fa soprattutto trasferimenti pesanti in parallelo, `--carriers 4` fisso evita la fase di salita.
- `--udp` sui vhost ha gli stessi limiti del public: circa 1 Gbit/s per connessione QUIC. Usarlo quando il lato client ha un uplink con perdite; altrimenti il relay è più veloce.
- `--https=redirect` se il servizio deve essere solo HTTPS; `--basic-auth` aggiunge un controllo sul server senza toccare il backend.

### 11.3 Secret (`bore local --tcp-secret-id` + `bore proxy`)

```bash
# provider (dove gira il servizio)
BORE_SECRET=... bore local 5432 --to https://<host> --tcp-secret-id db-prod \
  --udp --carriers 4 --auto-reconnect

# consumer (chi lo usa)
BORE_SECRET=... bore proxy --local-proxy-port 127.0.0.1:5432 --to https://<host> \
  --tcp-secret-id db-prod --udp --carriers 4 --auto-reconnect
```

- **`--udp` su entrambi i lati** quando l'UDP esce da entrambe le reti: il traffico va da peer a peer, il server non vede più i dati, e la latenza diventa quella del percorso diretto (−40 % nei test tra le due VM; molto di più quando i due peer sono vicini tra loro e lontani dal server). È il caso d'uso in cui il diretto vale di più.
- **`--carriers 4` insieme a `--udp` ha senso**: sul diretto viene ignorato (una sola connessione QUIC, con warning), ma governa il fallback relay, che è quello che si usa quando il NAT non collabora.
- **Il diretto secret non scala oltre circa 1 Gbit/s** e non scala con le connessioni parallele. Per spostare grandi volumi tra due macchine con link veloce e pulito, il relay con `--carriers 4` (senza `--udp`) è più veloce; per i trasferimenti di file c'è `bore transfer`, che ha stream paralleli propri.
- **Se la rete del consumer blocca l'UDP, togliere `--udp` dal consumer** finché il difetto 8.5-A non è corretto: altrimenti ogni avvio e ogni riconnessione costano quasi 8 secondi.
- **NAT domestico sul provider, risultato deterministico:** una regola di port-forward UDP sul router e poi

  ```bash
  bore local 5432 --to https://<host> --tcp-secret-id db-prod --udp \
    --nat-udp-preferred-port 41000 --udp-candidate <IP-WAN>:41000
  ```

  Nell'harness questa configurazione ottiene il diretto anche contro un consumer dietro NAT simmetrico. `--upnp` chiede al router di creare la regola da solo (PCP o UPnP). `--try-port-prediction` è l'ultima risorsa per i NAT simmetrici.
- **Cosa aspettarsi dai NAT** (capitolo 8.4): due router domestici tipici → diretto; un lato simmetrico e l'altro con filtering stretto → relay; due simmetrici → relay. Il fallback è automatico e il motivo è scritto nel log del consumer.
- **DNS inaffidabile sul provider:** aggiungere `--stun-server <IP>:7835` (difetto 8.5-B).
- **Server raggiunto sulla 443:** `--stun-server <host>:7835` rende utilizzabile anche lo STUN del server (nota 8.6).

### 11.4 Variabili d'ambiente utili

| Variabile | Dove | Effetto |
|---|---|---|
| `BORE_PROXY_BUFFER_SIZE` | client e server | dimensione dei buffer di copia per connessione (default 262144, limiti 4 KiB – 16 MiB). `65536` riduce a un terzo la memoria con molte connessioni aperte, con banda invariata entro il rumore (capitolo 7.5) |
| `BORE_SECRET` | client e server | segreto senza esporlo in `ps` |

---

## 12. Suggerimenti per il server

1. **Host mode + unit di redirect** (capitolo 5): fatto su collaudo, da replicare in produzione.
2. **Rimuovere rfw definitivamente** quando confermi: `docker compose -f rfw/compose.yml down`. Rinominare `/etc/sysctl.d/99-rfw-tunnel.conf` senza perderne il contenuto.
3. **`BORE_UDP_MEMORY_BUDGET=1GiB` su un host da 1,8 GB** produce due avvisi a ogni avvio. Il budget è un tetto, non una prenotazione, ma con molte connessioni dirette contemporanee può mettere sotto pressione la macchina. Un valore intorno a `512MiB` toglierebbe l'avviso sulla RAM; l'effetto sulla finestra per connessione va misurato prima di adottarlo, non l'ho provato. [ipotesi]
4. **2 vCPU sono il limite del QUIC.** Il path `--udp` costa circa un core per Gbit/s sul server. Se in produzione `--udp` è molto usato, i core contano più della RAM.
5. **Adottare mimalloc e il buffer sotto yamux** (capitolo 7.6) è l'intervento con il ritorno più alto: più banda sul relay e metà CPU, cioè più capacità sulla stessa macchina.
6. **Correggere il punto 1 del capitolo 9** prima di esporre il servizio a traffico non fidato: è l'unico difetto che permette a un esterno senza credenziali di bloccare tutti i tunnel.

---

## 13. Limiti di questo lavoro

- **P2P dietro NAT reale non testato**: la sandbox blocca l'UDP in uscita. Coperto con NAT del kernel in netns e con il diretto reale tra due IP pubblici.
- **Link di test molto veloce e senza perdite** (stesso datacenter, RTT 0,7 ms). Le conclusioni su relay contro QUIC valgono per link puliti. Su link con perdite o RTT alto il rapporto può invertirsi, e non l'ho misurato.
- **Server con 2 vCPU**: i tetti assoluti dipendono da questo.
- **Inventario dei tunnel prima del cambio non salvato** (capitolo 6.2).
- **Riavvio di collaudo non eseguito** (capitolo 5.8).
- **Prototipi misurati ma non integrati**: nessun gate del progetto è stato eseguito su di essi.
- **Lo stallo osservato una volta con rfw** resta senza causa confermata.
- **Latenza in loopback dei prototipi**: una sola esecuzione rumorosa, non conclusiva. Sul link reale la latenza è invariata.
- Due ripetizioni per la maggior parte dei confronti, tre per i prototipi. Le differenze riportate come significative sono molto più grandi della dispersione tra ripetizioni; quelle entro il 10 % vanno lette come rumore.

---

## 14. Pulizia e cose rimaste

**Server A (collaudo)**

- Rimossa la cartella di test `/tmp/bore-review` (binari, script, certificato di test, copia del segreto). Nessun processo, container o rete di test rimasti; le porte di test (5201, 3478/udp, 31000–31050) sono chiuse.
- Restano, per tua decisione: `compose.yml.bak-20261113-pre-hostmode` (è il rollback, tienilo finché non confermi), `compose.yml.hostmode` (copia, cancellabile), il container `rfw-tunnel` fermo e la cartella `rfw/`.
- Stato finale verificato dopo la pulizia: `bore-server` attivo in host mode, 0 errori nel log, unit di redirect `enabled` e `active` con una regola per catena (anche dopo `systemctl restart`), 4 vhost e 1 provider secret registrati e funzionanti, gateway SSH sulla 443 raggiungibile. RSS del container a riposo: circa 4 MiB.

**VM C**

- Rimossa `/tmp/bore-review`. Nessun processo di test rimasto. I servizi in ascolto sulla macchina non sono stati toccati.

**Sandbox**

- Rimossi il worktree del prototipo, le cartelle di build `target/review-bench` e `target/review-proto`, i binari di test e la copia locale del segreto.
- Restano installati nella sandbox: `musl-tools`, il target rustup `x86_64-unknown-linux-musl`, `netcat-openbsd`, `socat` e `nftables` (servivano all'harness netns) e due voci in `~/.ssh/known_hosts` per A e C.
- Repository: `git status` mostra solo `CLAUDE.md` modificato e questo file nuovo. Nessun commit, nessun push.

**Decisioni che restano a te**

1. Confermare la configurazione host mode su collaudo, poi cancellare il backup del compose e rimuovere rfw (`docker compose -f rfw/compose.yml down`), rinominando `99-rfw-tunnel.conf`.
2. Replicare in produzione con la procedura del capitolo 5.
3. Decidere se implementare le ottimizzazioni del capitolo 7.6 e le correzioni dei capitoli 8.5 e 9. Ordine che suggerisco: punto 1 del capitolo 9 (sicurezza), mimalloc, difetti 8.5-A e 8.5-B, buffer sotto yamux, il resto.
4. Decidere se committare questo file e la modifica a `CLAUDE.md`.

---

## Appendice A — Patch del prototipo

Applicata a `934aa94`. Le variabili `BORE_MUX_BUF` e `BORE_MUX_SPLIT` servivano solo a confrontare le varianti con lo stesso binario: in una versione definitiva diventano costanti.

```diff
--- a/Cargo.toml
+++ b/Cargo.toml
@@ [features]
+proto-mimalloc = ["dep:mimalloc"]
 default = ["udp"]
@@ [dependencies]
+mimalloc = { version = "0.1", default-features = false, optional = true }
--- a/src/main.rs
+++ b/src/main.rs
@@
+#[cfg(feature = "proto-mimalloc")]
+#[global_allocator]
+static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
+
 use std::ffi::OsString;
--- a/src/mux.rs
+++ b/src/mux.rs
@@
+/// PROTOTYPE (review bench): optional buffering layer under yamux.
+/// `BORE_MUX_BUF=<bytes>`; unset or 1 = pass-through.
+fn buffered<S: Transport>(socket: S) -> tokio::io::BufStream<S> {
+    let cap = std::env::var("BORE_MUX_BUF")
+        .ok()
+        .and_then(|v| v.parse::<usize>().ok())
+        .unwrap_or(1)
+        .max(1);
+    tokio::io::BufStream::with_capacity(cap, cap, socket)
+}
+
 fn config() -> Config {
     let mut cfg = Config::default();
+    if let Some(n) = std::env::var("BORE_MUX_SPLIT")
+        .ok()
+        .and_then(|v| v.parse::<usize>().ok())
+    {
+        cfg.set_split_send_size(n);
+    }
@@ pub fn client / pub(crate) fn client_scoped / pub fn server
     let io = ActivityIo {
-        inner: socket,
+        inner: buffered(socket),
         activity: Arc::clone(&activity),
     };
```

Build usata per i test:

```bash
cargo build --release --features proto-mimalloc --target x86_64-unknown-linux-musl
# a runtime
MIMALLOC_PURGE_DELAY=0 BORE_MUX_BUF=262144 BORE_MUX_SPLIT=262144 bore ...
```

I binari dei test sono stati compilati con LTO `thin` e 16 codegen unit per accorciare i tempi; il profilo release del progetto (`lto = true`, `codegen-units = 1`) è più ottimizzato, quindi i numeri assoluti dell'immagine vera possono essere un po' migliori. Il confronto tra varianti resta valido perché tutte e tre, base compresa nel test sul link reale, sono state misurate nelle stesse condizioni.
