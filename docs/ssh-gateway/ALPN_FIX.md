# ALPN_FIX — banner `SSH-2.0-russh_…` nel browser + richieste `pending` sui tunnel ssh-gateway

> Documento di riferimento per il bug risolto il 2026-07-06 (commit `cc7bf82`, branch `ssh`).
> Se si ripresentano instabilità simili sui tunnel, **ripartire da qui**: §6 elenca cosa è
> già escluso e §7 le ipotesi residue con i comandi di diagnosi.

---

## 1. Sintomi riportati (campo)

Tunnel creati via ssh-gateway (vhost/public/secret), es.:

```bash
autossh -M0 -T -p 443 -R vhost/dufsgh:0:localhost:5000 bore.example.xyz -- 'notes="assistenza"'
```

1. **A volte** il browser mostrava, al posto della pagina servita, il testo:
   `SSH-2.0-russh_0.62.1`
2. **A volte** le pagine stallavano: richieste ferme in `pending` nella console di rete,
   asset mancanti; il refresh non risolveva subito; a volte serviva chiudere e riaprire il
   browser.
3. Nessuna riproduzione deterministica (intermittente).

Contesto deploy: `docker-compose.server.yml` con `443:7835` — cioè **tutto il traffico
browser HTTPS entra dalla porta di controllo**, che con `--ssh-gateway` attivo demuxa
SSH / TLS / HTTP / bore sulla stessa porta.

## 2. Root cause (una sola, spiega entrambi i sintomi)

`demux_post_tls` (`src/sshgw.rs`) classificava una connessione TLS come **SSH** se il client
non inviava byte entro 2 s (`SSH_PEEK_TIMEOUT`) dopo l'handshake — euristica sslh-style
"silenzio ⇒ SSH" corretta sul piano plain-TCP ma **sbagliata dentro TLS**:

- I browser aprono **connessioni speculative e di pool** (preconnect, socket extra per gli
  asset paralleli in HTTP/1.1) che completano l'handshake TLS e poi restano **mute** anche
  per 5–10 s, finché non servono.
- Dopo 2 s di silenzio il demux le consegnava a russh → russh scriveva il proprio banner
  `SSH-2.0-russh_0.62.1`.
- Se il browser poi usava quel socket per una richiesta: riceveva il banner come "risposta"
  (→ **sintomo 1**) oppure la richiesta restava appesa finché russh non chiudeva
  (→ **sintomo 2**, `pending`).
- Il pool del browser **riusa** i socket avvelenati → il refresh non guariva; chiudere il
  browser svuotava il pool → "guariva". Da qui l'intermittenza.

Il banner nel browser è la firma univoca: quel testo lo scrive **solo** russh, e russh
riceve la connessione **solo** dal demux.

## 3. Fix (ALPN-first, deterministico) — I-SSH9

Il demux post-TLS ora classifica **prima l'offerta ALPN del ClientHello**, senza aspettare
alcun timeout (`sshgw::accept_tls_with_alpn` via `tokio_rustls::LazyConfigAcceptor` +
`demux_classify_alpn`):

| Offerta ALPN del client | Instradamento |
|---|---|
| presente, ≠ `ssh` (browser: `h2`/`http/1.1`; client bore nativo: `bore`) | **mai SSH** → `route_connection_known_http` |
| letteralmente `ssh` (`openssl s_client -alpn ssh`) | gateway SSH **immediato** (niente attesa 2 s) |
| assente (stock `openssl s_client`, ProxyCommand D4) | peek-silenzio legacy di 2 s (comportamento pre-fix, SSH-over-TLS preservato) |

Componenti del fix:

1. **`src/sshgw.rs`** — `AlpnRoute`, `demux_classify_alpn`, `accept_tls_with_alpn`,
   `HTTP_ALPN_FIRST_REQUEST_TIMEOUT` (60 s).
2. **`src/server.rs`** — il ramo TLS del demux usa l'accept ALPN-aware;
   `route_connection_known_http`: connessione già provata HTTP ⇒ attesa **web-server-style**
   di 60 s per la prima request (un preconnect idla legittimamente oltre il generico
   `NETWORK_TIMEOUT` di 3 s); allo scadere ⇒ **chiusura pulita**, mai consegna al path
   bore-protocol (un garbage-close a metà richiesta è esattamente l'instabilità riportata).
3. **`src/transport.rs`** — il client bore nativo offre ALPN `bore`: elimina la stessa
   classe di rischio su first-flight lente. **Wire-compatibile nei due sensi**: un server
   rustls senza `alpn_protocols` configurati ignora l'offerta (nessun server bore la
   configura).

Vincoli da NON violare in interventi futuri (vedi anche CLAUDE.md, I-SSH9):

- Non ricollassare il demux post-TLS al solo timeout.
- Non consegnare a `handle_connection` una connessione ALPN-http scaduta: chiuderla.
- Il path senza ALPN deve restare identico (SSH-over-TLS via `openssl s_client` stock, D4).

## 4. Percorsi NON affetti (auditati)

- **Client binario nativo TCP**: scrive `Hello` subito dopo l'handshake → i peek vedono
  byte in millisecondi. Ora in più offre ALPN `bore`.
- **UDP/QUIC** (secret `--udp`, vhost `--udp`, public `--udp`): socket UDP separato, il
  demux non esiste su quel percorso.
- **Porta SSH dedicata** (`--ssh-port`) e **porte pubbliche** (9000-9100): nessun demux.
- **Gateway disabilitato / feature non compilata**: accept path byte-identico (I-SSH1).

## 5. Verifica

- **T-SSH-DMX3** (`tests/ssh_gateway_test.rs`): TLS con ALPN `h2,http/1.1`, muto 4 s, poi
  `GET` → deve ricevere HTTP, mai banner. **Rosso sul codice pre-fix** (banner a ~2 s),
  verde col fix (red-check eseguito con stash dei sorgenti).
- **T-SSH-DMX4**: `-alpn ssh` → banner SSH immediato.
- `demux_classify_alpn_table` (unit, `src/sshgw.rs`).
- Suite: cargo ssh-gateway 34/0 e2e; default features 511/0; netns
  `scripts/ssh_gateway_test.sh` **14/0**; clippy `-D warnings` su default / ssh-gateway /
  vpn+ssh-gateway.
- CI (commit `cc7bf82`): 5/5 workflow verdi. Unico rosso al primo giro:
  `local_access_log_raw` su Windows — flake pre-esistente NON correlato (test plain-TCP,
  job `--features vpn` senza ssh-gateway → codice del fix nemmeno compilato); verde al
  rerun senza modifiche.

## 6. Cosa è già ESCLUSO come causa (non re-investigare senza nuovi indizi)

| Ipotesi | Verdetto |
|---|---|
| Connessione/disconnessione del tunnel | No — reaper/takeover ok (T-SSH-N1/N3) |
| Riavvio server + mancato recovery | No — autossh recovery ok (T-SSH-N2) |
| Conflitto TLS client/server | No — handshake sempre ok; il problema era il routing DOPO l'handshake |
| Saturazione finestre / concorrenza consumer | No per questo sintomo — HOL per-canale già fixato a parte (commit `2813879`, russh PR#730 vendorizzato) |
| Layer QUIC/UDP | No — non attraversato dal traffico browser in questa topologia |

## 7. Se i problemi si ripresentano — checklist di ripartenza

1. **Banner nel browser?** Se sì con versione ≥ `cc7bf82`: regressione del demux — primo
   sospetto qualunque modifica a `accept_tls_with_alpn` / ramo TLS di `server.rs`.
   Riprodurre con:
   ```bash
   # simula preconnect browser: ALPN http, silenzio 5s, poi GET
   (sleep 5; printf 'GET / HTTP/1.1\r\nHost: <sub>.<dominio>\r\n\r\n'; sleep 3) \
     | openssl s_client -quiet -verify_quiet -alpn h2,http/1.1 -connect <server>:443
   # atteso: risposta HTTP; MAI "SSH-2.0"
   ```
2. **Pending senza banner?** Allora NON è questo bug. **AGGIORNAMENTO 2026-07-06 (follow-up
   report "stalla dopo riavvio dell'app"):** l'intera matrice riavvio-app è stata riprodotta
   in locale su questo scenario (dufs reale + python, TLS/ALPN, keep-alive che attraversa il
   riavvio, kill a metà download/upload, richieste durante il downtime, rekey client e server)
   e il tunnel è risultato SEMPRE resiliente — il wedge richiede un peer che smette di
   RISPONDERE mentre il TCP resta vivo (processo ssh congelato/laptop sospeso/percorso NAT
   mezzo-morto). Quel wedge strutturale è stato chiuso con **I-SSH10** (vedi CLAUDE.md):
   ogni `forwarded-tcpip` open ha timeout 15 s; 2 timeout consecutivi ⇒ eviction dura della
   sessione (`RunningSession::abort`) ⇒ label/porta liberate ⇒ autossh riconnette da solo.
   In più il russh vendorizzato ora sveglia+erra i writer bloccati sulla finestra quando il
   canale muore (`WindowSizeRef::close`) — prima un upload verso un'app morta poteva
   leakare task+permit per sempre. Se rivedi `pending` persistenti con versione ≥ questo fix:
   guarda nel log server le righe `evicting wedged session (I-SSH10)` — se APPAIONO, il
   self-heal sta lavorando e il problema è il client/rete; se NON appaiono, sospetti residui:
   a. finestra SSH lato OpenSSH client (~2 MiB per canale — limite noto, vedi memoria
      "SSH gateway throughput"); b. `--max-conns` esaurito (`conn_rejections` su
      `/admin/status`); c. buffer socket kernel (`BORE_SSH_SOCKBUF`); d. servizio locale
      dietro il tunnel lento/saturato (verificare bypassando il tunnel); e. dufs/app dietro
      docker-proxy con backend morto (accetta e non risponde: dal punto di vista del tunnel
      è un'app lenta — verificare con `curl 127.0.0.1:5000` locale).
3. **Log utili lato server**: `warn` "TLS handshake failed", righe `ssh gateway` con esito
   connessione; su `/admin/status` controllare righe tunnel + TX/RX live.
4. **Isolare il layer**: stesso servizio via tunnel nativo (`bore local`) sulla stessa
   porta demux — se il nativo è sano e ssh-gateway no, il problema è nel tratto
   russh/canale; se entrambi malati, è demux/vhost/relay comune.
5. Test regressione rapidi:
   ```bash
   cargo test --features ssh-gateway --test ssh_gateway_test t_ssh_dmx
   sudo -n /percorso/assoluto/scripts/ssh_gateway_test.sh
   ```

## 8. Note deploy

- Immagine `ghcr.io/manprint/bore:ssh` ricostruita dalla CI → `docker compose pull && up -d`.
- Il comando autossh degli utenti resta invariato (plain TCP su 443 → path pre-TLS, mai
  stato affetto).
- SSH-over-TLS: consigliato aggiungere `-alpn ssh` al ProxyCommand (routing immediato,
  separazione netta dal traffico browser) — vedi `README-SSH-GATEWAY.md` §8.
- Osservazione compose: `80:80/udp` soltanto — la porta 80 **TCP** non è mappata, quindi
  HTTP plain (e l'eventuale redirect verso HTTPS) non è raggiungibile dall'esterno.
  Aggiungere `"80:80"` se lo si vuole.
