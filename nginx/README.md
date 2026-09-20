# bore dietro Nginx, senza perdere funzionalità

Questa configurazione fa convivere bore con un Nginx che usa già le porte 80 e
443. Nginx possiede **TCP 443** e legge senza decifrarlo il ClientHello TLS:

- ALPN `bore` e `ssh`, SSH semplice e bore semplice vanno al control port bore;
- `bore.example.com` e `files.bore.example.com` vanno al control port bore;
- `*.bore.example.com` va al frontend HTTPS vhost di bore;
- gli altri nomi continuano verso i virtual host HTTPS Nginx esistenti.

TLS resta end-to-end fino a bore. In questo modo funzionano il protocollo nativo,
SSH normale e SSH-over-TLS, admin, Web Transfer, WebSocket e le policy HTTPS dei
singoli vhost. Nginx non può trasformare tutte le funzioni di bore in solo HTTP:
QUIC/STUN usano UDP e i tunnel pubblici usano porte TCP assegnate.

## Porte

| Porta pubblica | Processo | Scopo |
|---|---|---|
| TCP 80 | Nginx | HTTP/ACME e vhost HTTP |
| TCP 443 | Nginx `stream` | demux HTTPS, bore nativo e SSH |
| UDP 443 | bore | QUIC condiviso per vhost, public `--udp` e `sshjhost --udp` |
| UDP 7835 | bore | STUN sul control port, necessario per direct UDP/diagnostica/VPN |
| UDP 7836 | bore | STUN alternativo per classificare il NAT |
| TCP 20000-20100 | bore | tunnel pubblici e modalità che richiedono una porta pubblica |

I listener 7835/TCP, 8080/TCP, 8443/TCP e 8444/TCP sono backend locali e non
vanno aperti nel firewall. `8443` appartiene a Nginx (i siti HTTPS preesistenti),
gli altri a bore.

Se non apri il range TCP puoi comunque usare secret tunnel, vhost, Web Transfer,
VPN e SSH jump, ma **non** stai usando tutte le funzionalità: `bore local` con
porta pubblica e i flussi equivalenti non saranno raggiungibili.

## 1. DNS e certificato

Sostituisci ovunque `bore.example.com` con il dominio reale. Crea:

```text
bore.example.com        A  <IPv4 VM>
files.bore.example.com  A  <IPv4 VM>
*.bore.example.com      A  <IPv4 VM>
```

Questi esempi pubblicano solo record `A` perché il servizio systemd fornito lega
bore a `0.0.0.0`. Aggiungi record `AAAA` solo dopo aver reso raggiungibili anche
QUIC e STUN su IPv6; altrimenti alcuni client proverebbero prima un percorso UDP
che la VM non sta ascoltando.

Il certificato deve contenere sia `bore.example.com` sia
`*.bore.example.com`. Un wildcard Let's Encrypt richiede normalmente DNS-01.
Copia certificato e chiave in una directory leggibile dal gruppo `bore`:

```shell
sudo install -d -o root -g bore -m 0750 /etc/bore/tls
sudo install -o root -g bore -m 0640 /percorso/fullchain.pem /etc/bore/tls/fullchain.pem
sudo install -o root -g bore -m 0640 /percorso/privkey.pem /etc/bore/tls/privkey.pem
```

Il frontend vhost rilegge i file aggiornati; il TLS del control port viene
caricato all'avvio. Dopo ogni rinnovo esegui `systemctl restart bore`.

## 2. Build e utente di servizio

Per includere VPN e SSH gateway serve la build completa:

```shell
cargo build --release --all-features
sudo install -o root -g root -m 0755 target/release/bore /usr/local/bin/bore
sudo useradd --system --home /var/lib/bore --create-home --shell /usr/sbin/nologin bore
sudo install -d -o bore -g bore -m 0750 /var/log/bore
sudo install -d -o root -g bore -m 0750 /etc/bore/authorized_keys.d
```

Metti almeno una chiave pubblica OpenSSH in
`/etc/bore/authorized_keys.d/operatore`. Senza chiavi o password il gateway SSH
rifiuta correttamente l'avvio.

## 3. Configurazione bore e systemd

```shell
sudo install -o root -g bore -m 0640 nginx/vhost.yml.example /etc/bore/vhost.yml
sudo install -o root -g bore -m 0600 nginx/bore-server.env.example /etc/bore/server.env
sudo install -o root -g root -m 0644 nginx/bore-server.service /etc/systemd/system/bore.service
sudo install -o root -g root -m 0644 nginx/99-bore-udp.conf /etc/sysctl.d/99-bore-udp.conf
```

Modifica `/etc/bore/vhost.yml` e `/etc/bore/server.env`. Genera due valori
distinti per `BORE_SECRET` e `BORE_ADMIN_TOKEN`; non lasciare i placeholder.
`BORE_ADMIN_TOKEN` deve avere almeno 32 caratteri.

```shell
openssl rand -base64 48
openssl rand -hex 32
sudo sysctl --system
sudo systemctl daemon-reload
sudo systemctl enable --now bore
```

Le opzioni principali sono:

- `BORE_SECRET`: autenticazione condivisa dei client bore nativi;
- `BORE_ADMIN_TOKEN`: protegge `/admin/status` e `/admin/api/v1/*`;
- `BORE_MIN_PORT`/`BORE_MAX_PORT`: range dei tunnel pubblici;
- `BORE_MAX_CONNS`: limite globale gestito; il servizio alza anche `NOFILE`;
- `BORE_MAX_CARRIERS`: limite dei carrier paralleli gestiti dal server;
- `BORE_UDP=true`: direct UDP, STUN e endpoint QUIC condiviso;
- `BORE_UDP_MEMORY_BUDGET`: tetto globale per le finestre QUIC; adegua `2GiB`
  alla RAM disponibile;
- `BORE_VHOST_*`: frontend dinamico HTTP/HTTPS e UDP 443;
- `BORE_SSH_*`: gateway OpenSSH e namespace ProxyJump;
- `BORE_WEB_TRANSFER_BASE_URL`: UI/relay WebSocket per i trasferimenti browser;
- `BORE_VPN*`: broker VPN e pool degli indirizzi overlay.

Non impostare `BORE_SSH_PORT`: il gateway SSH viene demultiplexato sul control
port e Nginx lo pubblica su 443. Non impostare `BORE_VHOST_QUIC_PORT` uguale al
control port interno 7835: lo stesso processo non può legare due socket UDP
diversi sulla stessa porta; qui STUN usa UDP 7835 e QUIC usa UDP 443.

## 4. Inserimento in Nginx

Il pacchetto deve includere il modulo stream con `ssl_preread` (su Debian/Ubuntu
è normalmente `libnginx-mod-stream`). Verifica:

```shell
nginx -V 2>&1 | grep -E 'stream|dynamic'
```

1. Sposta **tutti** gli attuali listener HTTPS Nginx da `443` a
   `127.0.0.1:8443`. Per esempio, cambia `listen 443 ssl;` in
   `listen 127.0.0.1:8443 ssl;` e rimuovi il corrispondente listener IPv6
   pubblico. Certificati, `server_name`, HTTP/2 e applicazioni restano uguali.
2. Disabilita gli eventuali `listen 443 quic`: UDP 443 appartiene al QUIC di
   bore, quindi HTTP/3 Nginx non può coesistere sullo stesso IP/porta.
3. Aggiungi il contenuto di `nginx.conf.fragment` al livello principale di
   `/etc/nginx/nginx.conf`, accanto a `http {}` e non dentro di esso.
4. Installa i due snippet, dopo aver sostituito il dominio:

```shell
sudo install -d -o root -g root -m 0755 /etc/nginx/stream-conf.d
sudo install -o root -g root -m 0644 nginx/stream-conf.d/bore.conf /etc/nginx/stream-conf.d/bore.conf
sudo install -o root -g root -m 0644 nginx/http-conf.d/bore.conf /etc/nginx/conf.d/bore.conf
sudo nginx -t
sudo systemctl reload nginx
```

Puoi anche ripetere il controllo sintattico con l'immagine Nginx ufficiale,
senza installare Nginx sulla macchina di sviluppo:

```shell
docker run --rm \
  -v "$PWD:/work:ro" \
  nginx:stable-alpine \
  nginx -t -c /work/nginx/test/nginx.conf
```

`proxy_half_close on` richiede Nginx 1.21.4 o successivo; su una versione più
vecchia elimina solo quella direttiva (gli altri instradamenti restano validi).

La configurazione stream non usa PROXY protocol perché bore non lo decodifica.
Di conseguenza bore vedrà `127.0.0.1` come peer TCP dei flussi passati da Nginx;
per i vhost HTTP su porta 80 sono comunque aggiunti `X-Real-IP` e
`X-Forwarded-For`. Non abilitare `proxy_protocol on`: corromperebbe il primo
frame bore/SSH/TLS.

## 5. Firewall

Esempio UFW (adatta il range se lo cambi nell'env):

```shell
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw allow 443/udp
sudo ufw allow 7835/udp
sudo ufw allow 7836/udp
sudo ufw allow 20000:20100/tcp
```

Non pubblicare 7835/TCP, 8080/TCP, 8443/TCP o 8444/TCP. Verifica anche il
firewall del provider cloud: UFW da solo non apre una security group.

## 6. Verifica end-to-end

```shell
sudo systemctl status bore --no-pager
sudo nginx -t
ss -lntup | grep -E ':(80|443|7835|7836|8080|8443|8444)\\b'
```

Control TLS e tunnel pubblico:

```shell
bore local 8080 --to https://bore.example.com --secret "$BORE_SECRET" --auto-reconnect
```

Vhost HTTPS/QUIC:

```shell
bore vhost 127.0.0.1:8080 --subdomain prova --id prova \
  --to https://bore.example.com --secret "$BORE_SECRET" --https=redirect --udp
curl -I http://prova.bore.example.com
curl -I https://prova.bore.example.com
```

Admin e Web Transfer:

```shell
curl -H "Authorization: Bearer $BORE_ADMIN_TOKEN" \
  https://bore.example.com/admin/api/v1/config
bore transfer web --to https://bore.example.com
```

SSH gateway e SSH-over-TLS:

```shell
ssh -N -p 443 -i ~/.ssh/id_ed25519_bore \
  -R vhost/prova:0:localhost:8080 bore.example.com
ssh -o "ProxyCommand=openssl s_client -quiet -alpn ssh -connect bore.example.com:443" \
  -N -i ~/.ssh/id_ed25519_bore \
  -R vhost/prova-tls:0:localhost:8080 dummy-host
```

Diagnostica UDP e VPN:

```shell
bore test-udp --to https://bore.example.com
sudo bore vpn listen --to https://bore.example.com --secret "$BORE_SECRET" --id sede-a
```

Per le sintassi complete di provider/consumer secret, transfer, VPN e SSH jump,
usa il README principale. La pagina browser dei trasferimenti è
`https://files.bore.example.com/transfer/` e l'admin è
`https://bore.example.com/admin/status`.

## Limiti intenzionali

- Nginx HTTP/3 e bore QUIC non possono entrambi possedere UDP 443 sullo stesso IP.
- TLS-ALPN-01 per i domini bore non arriva al client ACME di Nginx; usa DNS-01
  per il wildcard (raccomandato) o HTTP-01 sulla porta 80 per il solo nome base.
- Non mettere un CDN/proxy HTTP davanti al control hostname: il protocollo bore
  nativo e SSH non sono HTTP. Un load balancer L4 trasparente è compatibile.
- Il salto L4 fa vedere `127.0.0.1` sia a bore sia ai virtual host HTTPS del
  secondo Nginx. Rivaluta ACL, rate limit e log basati sull'IP sorgente. Per
  conservarlo senza alterare il protocollo serve un secondo IP pubblico oppure
  un proxy trasparente con policy routing; PROXY protocol non è compatibile con
  bore e non va abilitato.
- I flussi QUIC diretti già aperti non migrano su TCP: il fallback vale per le
  connessioni successive.
