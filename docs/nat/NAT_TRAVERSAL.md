# NAT traversal e UDP hole-punching in bore — guida dettagliata

Questo documento spiega **come funziona** il percorso diretto UDP di questo fork di
`bore` e fornisce la **matrice completa** dei casi: dati due host A e B, con ogni
combinazione di tipo di NAT/firewall e di porte, **quando il diretto UDP funziona
e quando no**, con le **azioni di rimedio** per chi amministra la rete.

> TL;DR operativo
> - Il **provider** (`bore local --tcp-secret-id`, lato QUIC **server**) deve
>   essere **raggiungibile**. Il **consumer** (`bore proxy`, lato QUIC **client**)
>   può stare anche dietro NAT difficili/mobile, purché abbia UDP in uscita.
> - Metti quindi il provider sulla parte più aperta: **VPS pubblico** (ottimo),
>   NAT **cone**, o router domestico con **port-forward/UPnP** sulla porta UDP.
> - Se il diretto non è possibile (symmetric↔symmetric, CGNAT su entrambi, UDP
>   bloccato), si usa **il relay del server**: il tunnel **funziona comunque**.
> - Diagnostica su ogni host con `bore test-udp`; per verificare la coppia reale
>   A<->B usa `bore test-udp --tcp-secret-id <id>` su entrambe le macchine.

Indice:
1. [Ambito](#1-ambito)
2. [Come funziona, passo per passo](#2-come-funziona-passo-per-passo)
3. [Porte e flussi di rete](#3-porte-e-flussi-di-rete)
4. [Teoria: NAT e firewall](#4-teoria-nat-e-firewall)
5. [La regola d'oro di bore (asimmetria provider/consumer)](#5-la-regola-doro-di-bore-asimmetria-providerconsumer)
6. [Matrice completa A×B](#6-matrice-completa-ab)
7. [Rimedi per amministratori, caso per caso](#7-rimedi-per-amministratori-caso-per-caso)
8. [Reti cellulari (4G/5G) e CGNAT](#8-reti-cellulari-4g5g-e-cgnat)
9. [IPv6](#9-ipv6)
10. [Casi speciali](#10-casi-speciali)
11. [Strumenti e flag](#11-strumenti-e-flag)
12. [Checklist amministratore](#12-checklist-amministratore)
13. [Limiti noti](#13-limiti-noti)

---

## 1. Ambito

Il percorso diretto UDP con **NAT traversal/hole-punch** descritto in questo
documento esiste solo per i tunnel "secret":

- **provider** = `bore local <porta> --tcp-secret-id <id> --udp` (espone un servizio);
- **consumer** = `bore proxy --local-proxy-port :<porta> --tcp-secret-id <id> --udp`
  (consuma il servizio su una porta locale).

Entrambi si collegano in **uscita** al **server** `bore server --udp`, che fa da
**rendezvous** (signaling) e da **STUN responder**. I percorsi server-direct di
vhost, tunnel public e provider nativo `sshjhost --udp` sono fuori ambito: il
provider/client diala direttamente l'endpoint pubblico condiviso
`--vhost-quic-port`, rispettivamente con chiavi bare, `port:<N>` e
`jump:<alias>`. Non fanno STUN né hole-punch. La modalità a porta pubblica
(`bore local 8000 --to … -p 1234`, browser → `server:porta`) non è
hole-punchabile perché i client esterni sono arbitrari; il QUIC opzionale copre
solo la gamba server→provider.

Se il diretto non si stabilisce, i dati passano dal **relay** del server (il
comportamento classico di bore): è sempre disponibile, quindi `--udp` non rompe
mai un tunnel.

---

## 2. Come funziona, passo per passo

```
         (1) controllo TCP/TLS            (1) controllo TCP/TLS
 PROVIDER ───────────────────►  SERVER  ◄─────────────────── CONSUMER
 (QUIC server)                (rendezvous + STUN)            (QUIC client)
     │                             │                              │
     │  (2) STUN: scopre il proprio indirizzo riflessivo pubblico │
     ├────────────UDP────────────► │ ◄────────────UDP─────────────┤
     │                             │                              │
     │  (3) offre i candidati      │   (3) offre i candidati      │
     ├──ClientMessage::UdpCandidates──►│◄──ClientMessage::UdpCandidates─┤
     │                             │                              │
     │  (4) broker: nonce condiviso│  (4) broker: candidati del   │
     │◄─ServerMessage::UdpPunch────┤   provider ──UdpPunch───────►│
     │   {nonce, candidati cons.}  │     {nonce, candidati prov.} │
     │                             │                              │
     │  (5) PUNCH: datagrammi UDP simultanei verso i candidati    │
     │◄═══════════════════ UDP diretto P2P ══════════════════════►│
     │                                                            │
     │  (6) QUIC: il consumer (client) connette il provider       │
     │      (server). Token = HMAC(secret, nonce) sui primi 32 B  │
     │◄════════════ QUIC + yamux + dati ═════════════════════════►│
     │                                                            │
     │  Se 2–6 falliscono → RELAY via SERVER (sempre disponibile) │
```

1. **Canale di controllo.** Provider e consumer aprono **una** connessione (TCP, o
   TLS se `--to` è `https://`) verso il server e si registrano (`HelloSecret(id)`
   / `ConnectSecret(id)`), con auth opzionale (HMAC challenge/response).

2. **Scoperta STUN.** Ogni peer apre un **socket UDP** (porta effimera, oppure
  fissa con `--nat-udp-preferred-port`) e invia una **STUN binding request**
  (RFC 5389). Senza override prova una chain pensata per firewall reali:
  Cloudflare `stun.cloudflare.com:3478`, poi Google `19302`, poi lo STUN del
  server bore sulla porta di controllo UDP. Con `--stun-server` usa solo
  l'endpoint indicato. La risposta contiene l'**indirizzo riflessivo** =
  l'`IP:porta` pubblico come visto da fuori. Se nessuno STUN risponde → niente
  indirizzo pubblico → di norma solo relay.

  Nei tunnel secret live, il provider allega ai suoi candidati anche lo STUN che
  ha selezionato. Il server conserva questo metadata e lo dà ai consumer
  `bore proxy --udp` prima che raccolgano i propri candidati: il proxy prova
  quello STUN come primo target, poi continua con Cloudflare, Google e fallback
  bore se non risponde. Un `--stun-server` esplicito resta un override assoluto.

3. **Raccolta e offerta dei candidati.** Ogni peer compone la lista:
   - **riflessivo** (pubblico, da STUN) — il candidato principale per il traversal;
   - **locale** (es. `192.168.x.y:porta`) — per due peer sulla **stessa LAN**;
   - opzionale **UPnP-IGD** (`--upnp`) — porta mappata dal router domestico;
   - opzionale **porte predette** (`--try-port-prediction`) — qualche porta oltre
     quella riflessiva, per NAT simmetrici sequenziali.

   I candidati vengono inviati al server (`ClientMessage::UdpCandidates`).

4. **Brokeraggio.** Il server abbina provider e consumer per `id`, **conia un
   nonce** stabile per provider e inoltra a ciascuno i candidati dell'altro
   (`ServerMessage::UdpPunch { nonce, peer }`).

5. **Hole-punch.** Entrambi inviano alcuni piccoli datagrammi UDP verso **tutti**
   i candidati dell'altro (`punch()`), per **aprire le mappature/i filtri** del
   proprio NAT verso il peer. Lo fanno **entrambi i lati** (sia il provider in
   `DirectListener::new`, sia il consumer in `connect_direct`).

6. **QUIC + autenticazione.** Il **consumer è il client QUIC**: prova i candidati
   del provider (riflessivo per primo) finché uno completa l'handshake. Il
  **provider è il server QUIC** (`DirectListener`). Sui primi 32 byte i due si
  scambiano un **token = HMAC(secret, nonce)**: se non combacia, si chiude. Poi
  ogni connessione proxata usa una **bidi-stream QUIC nativa** indipendente,
  mantenendo isolamento da perdita e flow-control per flusso.

**Robustezza.**
- Il provider tiene un `DirectListener` **persistente** e **ri-buca** verso ogni
  nuovo consumer (nonce stabile → stesso token per tutti).
- Il consumer **rileva** la morte del path diretto (restart del provider) e si
  riconnette; un consumer **sul relay** ritenta il diretto ogni **10 s** e fa
  **upgrade in place** appena il provider diventa raggiungibile (nessuna sessione
  persa). Il sistema **converge** sempre al diretto entro ~10 s.
- **Keep-alive QUIC ogni 3 s** (idle 10 s): tiene viva la mappatura NAT durante
  trasferimenti lunghi e quieti, e rileva un peer sparito entro ~10 s.
- **Finestre QUIC high-throughput:** il direct path usa costanti in
  `src/holepunch.rs` (16 MiB per stream, 64 MiB aggregate/send) più alte dei
  default Quinn, così un singolo trasferimento non viene limitato troppo presto
  dal flow-control su link high-BDP. Aumentarle consuma più memoria.
- **Buffer UDP e BBR applicativi:** bore richiede buffer UDP send/receive da 16
  MiB sul socket usato da QUIC (`DIRECT_UDP_SOCKET_RECV_BUFFER` /
  `DIRECT_UDP_SOCKET_SEND_BUFFER`) e imposta `quinn::congestion::BbrConfig` come
  congestion controller del direct path. Le finestre QUIC sono
  `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` = 16 MiB,
  `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` = 64 MiB e `DIRECT_QUIC_SEND_WINDOW` =
  64 MiB. I cap del kernel possono comunque limitare il valore effettivo dei
  buffer UDP.
- **Fallimento di qualsiasi passo → relay.** Mai un tunnel rotto.

---

## 3. Porte e flussi di rete

| Flusso | Protocollo | Direzione | Porta tipica | Obbligatorio? |
|---|---|---|---|---|
| Controllo + signaling | TCP / TLS | peer → server (uscita) | 7835 / 443 / 80 | **Sì** (anche per il relay) |
| STUN (scoperta indirizzo) | UDP | peer → server o STUN pubblico (uscita) | 7835 / 19302 / 3478 | per il diretto |
| Hole-punch + QUIC (dati diretti) | UDP | provider ↔ consumer (uscita + ritorno) | effimera alta, o fissa (`--nat-udp-preferred-port`) | per il diretto |
| Relay (fallback dati) | TCP / TLS | dentro la connessione di controllo | 7835 / 443 / 80 | fallback |

Note:
- Lo **STUN del server** vive sulla **porta di controllo UDP**. Se `--to` usa
  `https://` (443) o `http://` (80), quelle porte frontano solo il controllo TCP:
  lo STUN di default ricade sulla **porta di controllo well-known 7835**. Per
  deployment non standard usa `--stun-server`.
- I firewall **stateful** lasciano passare il **ritorno** dei flussi UDP iniziati
  dall'interno: per questo il punch (che parte dall'interno) apre il varco.

---

## 4. Teoria: NAT e firewall

Due comportamenti **indipendenti** di un NAT (terminologia RFC 4787):

**A) Mapping (come assegna la porta esterna).**
- **EIM — Endpoint-Independent Mapping** ("cone"): stessa `IP:porta` esterna verso
  **qualsiasi** destinazione. → La porta vista da STUN è quella **valida anche
  verso il peer**. **Bucabile.**
- **APDM — Address-and-Port-Dependent Mapping** ("symmetric"): porta esterna
  **diversa per ogni destinazione**. → La porta vista da STUN **non** è quella
  verso il peer. **Difficile/impossibile da bucare** (il peer non sa dove
  bussare). Se le porte sono **sequenziali**, la *port prediction* può indovinarle.

**B) Filtering (chi può entrare).**
- **EIF — Endpoint-Independent Filtering** (full cone): una volta aperta la
  mappatura, accetta da **chiunque**.
- **ADF — Address-Dependent Filtering** (restricted cone): accetta da un **IP** a
  cui hai inviato (qualsiasi porta di quell'IP).
- **APDF — Address-and-Port-Dependent Filtering** (port-restricted cone): accetta
  **solo** dall'`IP:porta` esatto a cui hai inviato.

**Tipi classici** (mapping + filtering):
| Nome classico | Mapping | Filtering | Bucabile |
|---|---|---|---|
| Full Cone | EIM | EIF | facilissimo |
| Restricted Cone | EIM | ADF | facile |
| **Port-Restricted Cone** (router domestico tipico, Linux/`MASQUERADE`) | EIM | APDF | sì tra cone, **no** verso symmetric |
| **Symmetric** | APDM | APDF | quasi mai |

Altri concetti:
- **Port preservation**: il NAT mantiene la porta locale come porta esterna
  (es. `:41641`→`:41641`). Comodo: rende l'esterno prevedibile/stabile.
- **Hairpinning**: due host dietro lo **stesso** NAT che si parlano via l'IP
  pubblico del NAT. Spesso non supportato → bore usa il **candidato locale** per
  la stessa LAN.
- **CGNAT** (RFC 6598, `100.64.0.0/10`): NAT del **carrier**. Spesso **symmetric**.
  Tipico su mobile e su molte connessioni "economiche"/starlink. L'host vede un
  indirizzo privato e **non** ha un vero IP pubblico proprio.
- **Doppio NAT**: NAT dentro NAT; lo STUN può restituire un indirizzo **privato**
  (un altro NAT a monte) → non instradabile.

**Cosa rileva `bore test-udp`:** il **mapping** (cone vs symmetric, confrontando
le porte su più STUN) e CGNAT/doppio-NAT. **Non** rileva il **filtering**
(full/restricted/port-restricted): servirebbe uno STUN con IP/porta alternativi
(CHANGE-REQUEST), che Google/Cloudflare non offrono. Quindi un host marcato
"cone" può essere full, restricted **o** port-restricted: la differenza conta
quando il **peer è symmetric** (vedi sotto).

---

## 5. La regola d'oro di bore (asimmetria provider/consumer)

In bore i ruoli QUIC sono **fissi**: **provider = server**, **consumer = client**.
Quindi è il **consumer che compone (dial) la connessione** verso i **candidati del
provider**. Da qui due conseguenze:

1. **Il provider deve essere RAGGIUNGIBILE** dal consumer:
   - mapping **EIM** (la porta annunciata è valida), e
   - il **filtro** del provider deve accettare il **sorgente reale** del consumer.
     Il provider buca verso il candidato **annunciato** del consumer:
     - se il consumer è **EIM**, sorgente reale = annunciato → ogni filtro
       (EIF/ADF/APDF) si apre correttamente → **OK**;
     - se il consumer è **symmetric**, sorgente reale ≠ annunciato → si apre solo
       con filtro **EIF (full)** o **ADF (restricted)**; con **APDF
       (port-restricted)** → **NO**.

2. **Il consumer può essere quasi qualsiasi cosa** (anche symmetric/CGNAT/mobile),
   purché abbia **UDP in uscita**: è lui che inizia, e il suo NAT lascia passare il
   ritorno. L'unico limite è il punto 1b (un consumer symmetric esige un provider
   full/restricted **o** pubblico, **non** port-restricted).

**In pratica:**
- **Provider pubblico / full cone / restricted cone** → funziona con **qualsiasi**
  consumer, **incluso mobile/symmetric**.
- **Provider port-restricted cone** (il caso domestico più comune) → funziona con
  consumer cone/pubblici; **fallisce** con consumer **symmetric/CGNAT/mobile** (a
  meno di port prediction, best-effort).
- **Provider symmetric / CGNAT-symmetric / UDP-bloccato** → **non** raggiungibile
  → relay. **Non ospitare il provider dietro CGNAT/mobile.**

> `bore test-udp` segnala il provider "cone" ma non distingue il filtering: se il
> tuo consumer è mobile/symmetric e il diretto non parte pur essendo il provider
> "cone", quasi certamente il provider è **port-restricted** → **port-forward/UPnP**
> della porta UDP, oppure sposta il provider su un **VPS pubblico**.

---

## 6. Matrice completa A×B

**A = PROVIDER** (righe, lato QUIC server, deve essere raggiungibile)
**B = CONSUMER** (colonne, lato QUIC client, deve avere UDP in uscita)

Legenda: **✓** diretto UDP · **✗** relay (diretto impossibile) · **⚠** forse
(solo con accorgimenti: prediction se symmetric *sequenziale*, oppure UPnP/port-
forward) · tutte le righe richiedono UDP in uscita su entrambi.

| Provider ↓ \ Consumer → | Pubblico / Full Cone | Restricted Cone | Port-Restricted Cone | Symmetric | CGNAT (mobile) | UDP egress bloccato |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| **Pubblico aperto** (UDP ingresso aperto) | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ |
| **Full Cone** (EIM+EIF) | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ |
| **Restricted Cone** (EIM+ADF) | ✓ | ✓ | ✓ | ✓ | ✓ | ✗ |
| **Port-Restricted Cone** (domestico tipico) | ✓ | ✓ | ✓ | ✗ (⚠ seq) | ✗ (⚠ seq) | ✗ |
| **Pubblico con firewall stateful** (ingresso NEW bloccato) | ✓ | ✓ | ✓ | ✗ (⚠ seq) | ✗ (⚠ seq) | ✗ |
| **Symmetric** (APDM) | ✗ (⚠ seq) | ✗ (⚠ seq) | ✗ (⚠ seq) | ✗ | ✗ | ✗ |
| **CGNAT symmetric** (mobile tipico) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| **Doppio NAT (reflexive privato)** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |
| **UDP egress bloccato** | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ |

Lettura della matrice:
- **Le prime tre righe (+ "pubblico aperto") vincono con tutto**, mobile/symmetric
  inclusi: se il provider è pubblico/full/restricted, qualunque consumer con UDP
  in uscita si connette.
- **La riga "Port-Restricted Cone" è il caso domestico tipico**: ok con consumer
  cone/pubblici, **ko** con consumer symmetric/CGNAT (la combinazione classica
  *port-restricted × symmetric* non è bucabile). `⚠ seq` = recuperabile con
  `--try-port-prediction` **solo** se il lato symmetric ha porte sequenziali.
- **Pubblico con firewall stateful** si comporta come *port-restricted* (il
  conntrack apre il varco verso l'indirizzo a cui ha bucato): per servire un
  consumer symmetric serve **aprire staticamente** l'ingresso (→ diventa "pubblico
  aperto").
- **Righe symmetric / CGNAT-symmetric / doppio-NAT / UDP-bloccato = provider non
  ospitabile** → relay.
- **`⚠ seq`** vuol dire: prova `--try-port-prediction` sul lato symmetric; è
  best-effort e spesso non basta. **Non** è una soluzione affidabile.

**Stessa LAN (caso trasversale):** se A e B sono dietro lo **stesso** NAT, il
**candidato locale** (`192.168.x.y`) li fa connettere direttamente a prescindere
dalla riga/colonna (serve solo che la LAN permetta UDP tra gli host). → **✓**.

> La matrice non è simmetrica: scambiare provider e consumer **cambia** l'esito.
> Esempio: home **port-restricted** che fa da **provider** verso un mobile
> **symmetric** = ✗; ma lo stesso mobile come **consumer** verso un provider
> **pubblico** = ✓. Scegli i ruoli di conseguenza.

---

## 7. Rimedi per amministratori, caso per caso

Per ogni situazione "✗/⚠" della matrice, ecco cosa fare. In **tutti** i casi, se
il traversal resta impossibile, **il relay funziona**: spesso "non fare nulla" è
una risposta legittima.

### 7.1 Provider Port-Restricted (domestico) + consumer symmetric/mobile → ✗
Il provider è "cone" ma port-restricted; il consumer mobile cambia porta. Soluzioni
(in ordine di preferenza):
1. **Sposta il provider su un VPS pubblico** e apri **in ingresso** la porta UDP
   (vedi 7.3). Diventa riga "Pubblico aperto" → ✓ con qualsiasi consumer.
2. **Port-forward sul router** del provider: inoltra una **porta UDP fissa** (es.
   41641) all'host del provider, e avvia con
   `--nat-udp-preferred-port 41641`. Il provider diventa raggiungibile su quella
   porta (≈ full cone) → ✓.
3. **UPnP** (`--upnp`): se il router domestico ha UPnP-IGD attivo **e** un IP WAN
   pubblico, bore chiede in automatico la mappatura. Inutile dietro CGNAT.
4. **Port prediction** (`--try-port-prediction`) sul **lato symmetric**: solo se le
   sue porte sono sequenziali. Best-effort.

### 7.2 Provider symmetric / CGNAT-symmetric / mobile → ✗ (qualsiasi consumer)
Un provider non raggiungibile non è ospitabile in P2P.
- **Inverti i ruoli** se possibile: chi sta sulla rete più aperta faccia da
  provider.
- Oppure **provider su VPS pubblico** (7.3).
- Altrimenti → **relay** (il tunnel funziona comunque).

### 7.3 Provider su host con IP pubblico ma **ingresso UDP chiuso** → ✗ finché chiuso
È il caso del VPS con firewall (cloud Security Group, `ufw`, `nftables`). Apri **in
ingresso** la porta UDP del punch, fissandola:
- avvia il provider con `--nat-udp-preferred-port 41641`;
- **cloud Security Group**: consenti `UDP 41641` in ingresso da `0.0.0.0/0`;
- **ufw**: `sudo ufw allow 41641/udp`;
- **nftables/iptables**: `... -p udp --dport 41641 -j ACCEPT`.
Risultato: riga "Pubblico aperto" → ✓ con **qualsiasi** consumer, mobile incluso.
(Stessa porta serve solo lato provider; il consumer esce e basta.)

### 7.4 UDP in **uscita** bloccato (corporate) → ✗
Lo STUN non risponde → niente candidati. Verifica con `bore test-udp` (tutte le
righe STUN `[FAIL]`). Soluzioni:
- Far **aprire l'egress UDP** verso lo STUN (server `7835`, o STUN pubblico
  `3478`/`19302`) **e** verso il peer (porte alte, o la porta fissa). In `nftables`
  lato client basta consentire l'**uscita** UDP (il ritorno è stateful).
- Se l'egress è filtrato **per porta sorgente**, usa `--nat-udp-preferred-port`
  con una porta consentita.
- Se l'egress UDP è vietato del tutto e non modificabile → **relay** (passa tutto
  sul controllo TCP/TLS, che in genere è permesso su 443).

### 7.5 STUN del server non risponde solo al provider (hairpin/co-locazione) → ✗
Sintomo (in `test-udp`): STUN **pubblici OK**, ma "bore server UDP did NOT answer".
Capita se il **provider gira sulla stessa macchina/LAN del server** (niente
hairpin verso l'EIP). Soluzioni:
- `--stun-server stun.l.google.com:19302` sul provider (STUN pubblico esterno);
- oppure esegui il provider da una **rete diversa** dal server.

### 7.6 Doppio NAT con reflexive privato → ✗
`test-udp` segnala "Double-NAT: the 'public' address … is itself private".
- Metti l'host in **DMZ** / disabilita un livello di NAT, oppure
- **port-forward** end-to-end della porta UDP fissa attraverso entrambi i NAT,
  oppure usa un **VPS pubblico** come provider.

### 7.7 Symmetric × symmetric, o CGNAT su entrambi → ✗
Non bucabile con questa implementazione (nessun TURN-over-UDP, nessun IPv6 sul
path diretto). → **relay**. È il comportamento atteso anche di soluzioni mature
quando entrambi i lati sono CGNAT.

---

## 8. Reti cellulari (4G/5G) e CGNAT

- Quasi tutte le SIM dati stanno dietro **CGNAT** (`100.64.0.0/10` o privato del
  carrier). Spesso il mapping è **symmetric** (varia per operatore/APN; alcuni
  sono cone).
- **Mobile come CONSUMER → ottimo:** un telefono/SIM può connettersi **in diretto**
  a un provider **pubblico / full / restricted cone** (matrice: colonna "CGNAT
  (mobile)" sulle prime righe = ✓). È il caso d'uso più comune e funziona.
- **Mobile come PROVIDER → quasi sempre no:** dietro CGNAT-symmetric non sei
  raggiungibile → relay. Non ospitare il provider su mobile.
- **Mobile ↔ mobile (entrambi CGNAT) → relay.** Nessun rimedio P2P su IPv4 (vedi
  IPv6).
- **Test:** lancia `bore test-udp` sulla SIM. Se vedi `CGNAT detected` o
  `SYMMETRIC` → il diretto dipende dal **provider** (rendilo pubblico/cone).

---

## 9. IPv6

L'IPv6 è **la leva più forte** contro il CGNAT: con IPv6 ogni host ha (di norma)
un indirizzo **globale**, niente NAT — al più un firewall stateful, già aperto dal
punch. Due peer IPv6 si connettono in diretto anche da reti mobile.

> **Stato attuale di questo fork:** il path diretto è **IPv4-only**
> (`bind_socket` lega `0.0.0.0`; i candidati locali/riflessivi sono IPv4). Quindi
> l'IPv6 del cellulare **non** è sfruttato e due peer CGNAT-mobile cadono sul
> **relay**. L'aggiunta di candidati IPv6 è l'evoluzione naturale per i casi
> CGNAT-su-entrambi; il control channel/relay funziona già su IPv6 se il DNS del
> server risolve in AAAA.

---

## 10. Casi speciali

- **Stessa LAN:** il candidato locale connette i due peer direttamente, senza STUN
  né hairpin. → ✓.
- **Provider co-locato col server:** vedi 7.5 (hairpin). Usa STUN pubblico o
  un'altra rete.
- **Più consumer / consumer che si riconnette:** il provider tiene il listener
  persistente e ri-buca; nonce stabile → stesso token. Funziona.
- **Restart del server:** il reconnect del canale di controllo (su entrambi)
  ri-negozia (diretto o relay).
- **Trasferimenti lunghi e quieti:** keep-alive QUIC 3 s + `SO_KEEPALIVE`/
  `TCP_NODELAY` sui socket → le mappature NAT non scadono.
- **Timeout mappatura NAT:** i NAT chiudono le mappature UDP inattive (spesso
  30 s–2 min). Il keep-alive le mantiene; senza traffico per >idle (10 s) un peer
  morto viene rilevato e si ri-negozia.

---

## 11. Strumenti e flag

| Flag (env) | Su | A cosa serve nella matrice |
|---|---|---|
| `--udp` (`BORE_PREFER_UDP`) | local, proxy | Abilita il tentativo diretto (server con `--udp`/`BORE_UDP`). |
| `--stun-server` (`BORE_STUN_SERVER`) | local, proxy, test-udp | STUN esterno: risolve hairpin/co-locazione (7.5) o server UDP irraggiungibile. |
| `--upnp` (`BORE_UPNP`) | local, proxy, test-udp paired | Mappa una porta sul **router domestico** (IP WAN pubblico): rende il provider raggiungibile (7.1). Inutile su CGNAT. |
| `--try-port-prediction` (`BORE_TRY_PORT_PREDICTION`) | local, proxy, test-udp paired | Annuncia porte predette sul lato **symmetric sequenziale** (i casi `⚠ seq`). Best-effort, può sembrare uno scan. |
| `--nat-udp-preferred-port` (`BORE_NAT_UDP_PORT`) | local, proxy, test-udp | Porta UDP **fissa** (0=random): da aprire in egress/ingress nel firewall (7.3, 7.4); su NAT port-preserving rende l'esterno prevedibile. |
| `--nat-udp-release-timeout` (`BORE_NAT_UDP_RELEASE_TIMEOUT`) | local, proxy | Secondi tra re-check dopo rimappatura NAT della porta preferita (default 600, 0=disabilita). Quando la porta è rimappata il peer usa porte effimere per non rinnovare la NAT entry. Utile quando due host sullo stesso NAT competono per la stessa porta fissa. |
| `bore test-udp [--to … --stun-server … --nat-udp-preferred-port …]` | — | **Diagnostica**: egress UDP, classe NAT (cone/symmetric), CGNAT/doppio-NAT, hairpin, UPnP. Lancialo su **entrambi** i peer. |
| `bore test-udp --to <srv> --secret <s> --tcp-secret-id <id>` | test-udp paired | **Diagnostica coordinata A<->B**: il server abbina due peer, scambia candidati, prova UDP diretto e TCP relay, e stampa un report bidirezionale. Con `--test-bandwidth --test-transfer-quota 500MB` misura anche banda e latenza su entrambi i path. |

Procedura consigliata: `bore test-udp` su provider **e** consumer → se serve una
prova end-to-end lancia la modalità paired con lo stesso id sui due host → applica
il rimedio della sezione 7 corrispondente.

Se il report mostra UDP diretto con latenza più bassa ma throughput inferiore al
TCP relay, non significa automaticamente che il diretto sia guasto: QUIC sopra UDP
resta affidabile e congestion-controlled, mentre TCP del kernel può essere più
veloce su single-stream e il server relay può essere topologicamente vicino a un
peer. Confronta sempre entrambe le direzioni e ripeti con quote realistiche.

---

## 12. Checklist amministratore

Per ottenere il **diretto** in modo affidabile:

1. **Server**: `bore server --udp`, con la **porta di controllo UDP** (7835)
   aperta in **ingresso** dal mondo (per lo STUN). Il client del bug iniziale
   raggiungeva `7835/udp` dal mondo: assicurati che sia così.
2. **Provider sul lato più aperto.** Ideale: **VPS pubblico** con
   `--nat-udp-preferred-port 41641` e **UDP 41641 aperto in ingresso** (7.3).
   In alternativa: router domestico con **port-forward/UPnP** della porta UDP.
3. **Consumer**: basta **UDP in uscita** (verso STUN e verso il provider). Mobile
   ok.
4. **Egress UDP** consentito su entrambi verso STUN (7835 o 3478/19302).
5. **Verifica** con `bore test-udp` su entrambi (provider deve risultare
   pubblico/cone e il suo STUN raggiungibile).
6. Se un lato è **symmetric/CGNAT** e non lo puoi cambiare → accetta il **relay**
   (tunnel comunque funzionante) o rendi l'**altro** lato pubblico/cone.

---

## 13. Limiti noti

- **Solo tunnel secret** (`--tcp-secret-id` + `bore proxy`); la modalità a porta
  pubblica non è interessata.
- **IPv4-only** sul path diretto (vedi §9): niente sfruttamento dell'IPv6 mobile.
- **Niente TURN-over-UDP**: per i casi non bucabili (symmetric×symmetric, CGNAT su
  entrambi) il fallback è il **relay del server bore**, non un relay UDP esterno.
- **`test-udp` rileva il mapping, non il filtering** (full vs restricted vs
  port-restricted): per i provider domestici "cone" che falliscono verso un
  consumer symmetric, assumi **port-restricted** e applica 7.1.
- **Port prediction**: best-effort, aiuta solo NAT simmetrici sequenziali, può
  apparire come uno scan a firewall stringenti (per questo è opt-in e loggato).
- **Throughput UDP vs TCP**: il path diretto elimina il relay e spesso riduce RTT,
  ma non promette più banda di TCP in ogni scenario. Path UDP filtrati/shapati,
  CPU user-space QUIC, MTU e topologia del server possono rendere il relay TCP più
  veloce in un benchmark single-stream.

---

## 14. Hardening e osservabilità del traversal (Fase 0)

Implementati come base misurabile del piano di miglioramento UDP
(`UDP_CONNECTION_IMPROVE.md`):

- **Limite e validazione candidati (`holepunch::MAX_UDP_CANDIDATES` = 16).**
  Ogni lista di candidati peer-controlled viene validata, deduplicata
  (order-preserving) e cappata PRIMA di qualsiasi allocazione o fan-out di
  task: lato mittente (fine della discovery), lato broker server (offer secret
  provider/consumer, VPN 1:1/hub/spoke, `TestUdpJoin`) e come ultima difesa nei
  punti d'ingresso (`connect_direct`, `DirectListener::new`,
  `punch_via_endpoint`). Vengono rifiutati: porta 0, indirizzi unspecified,
  multicast, broadcast. Gli indirizzi **privati/CGNAT restano validi** (servono
  per same-LAN; il token — non la lista candidati — autentica la sorgente,
  invariante I-6/D7). I drop sono loggati in **una riga aggregata**
  (`dropped unusable UDP candidates (aggregate)` con contatori
  invalid/duplicate/overflow), mai un warning per singolo elemento.
- **Metriche baseline nei log** (campi strutturati stabili):
  - `discovery_ms` — durata dell'intera gather (catena STUN + UPnP + local),
    anche in `CandidateDiscovery`;
  - `direct_ready_ms` + `winner` — tempo punch→QUIC autenticato del consumer
    (`direct QUIC path ready (consumer)`);
  - `fallback_reason` — enum stabile sul fallimento del direct
    (`no-candidates` | `all-candidates-failed` | `budget-exhausted`).
- **Retry del diagnostico paired = round ri-brokerato** (fix P1): socket nuovo ⇒
  discovery nuova ⇒ re-`TestUdpJoin` ⇒ il server attende entrambi i re-offer,
  conia nonce nuovo, ricalcola il piano e invia `TestUdpStart` con
  `generation`+1 (`recandidate: true` annuncia la capability; con un server
  vecchio i retry vengono saltati con nota esplicita, non eseguiti su candidati
  stantii). Wire backward-compatible: campi `#[serde(default)]`.
- **Ordine adattivo dichiarato advisory**: `connect_direct` resta un fan-out
  concorrente sotto budget; il report paired lo dice esplicitamente
  (`Candidate order: advisory only …`) finché la checklist della Fase 3 non lo
  renderà operativo.
- **NAT lab deterministico** (`tests/nat_traversal_test.rs` +
  `tests/support/natlab.rs`, solo Linux) e smoke netns con NAT kernel reale
  (`scripts/udp_nat_netns_test.sh`): baseline per-profilo in
  `docs/test/TEST_UDP.md` §S11 — una nuova tecnica entra solo flippando una
  riga RED del lab.

## 15. Traversal socket + candidate model v2 (Fase 1)

- **`holepunch::UdpTraversalSocket` — un solo owner di `recv_from`** (I-5).
  Un actor interno possiede la lettura durante la discovery e demultiplexa le
  risposte STUN per transaction id **e sorgente completa `ip:port`**: risposte
  duplicate, fuori ordine, con txid sbagliato o da sorgente diversa dal server
  interrogato vengono contate come stray e mai consegnate a un waiter; i
  datagrammi non-STUN (punch del peer, QUIC Initial precoci) sono contati e
  MAI consumati da una transazione (la Fase 2 li instraderà ai connectivity
  check). `into_socket()` ferma l'actor e SOLO DOPO rilascia il socket a
  Quinn — un socket, un lettore, sempre.
- **Catena STUN a budget globale (`STUN_CHAIN_BUDGET` = 4 s).** Le transazioni
  della catena corrono in parallelo (lanci scaglionati di 300 ms per
  conservare la preferenza d'ordine come vantaggio di partenza); il worst case
  legacy con N target morti era N × 3 s seriali (~12 s con 4 target) prima
  della decisione di relay. Tutti i path live (provider secret, consumer
  secret, VPN 1:1 e hub, `test-udp` paired + retry round) usano il traversal
  socket; il gather seriale legacy resta per i tool diagnostici single-shot e
  come oracolo di equivalenza nei test.
- **Candidate model v2 sul wire, observe-only.** `UdpCandidateOffer` porta
  (accanto alla lista legacy, che resta la fonte di verità) i campi
  `#[serde(default)]`: `typed_candidates` (addr + kind + priority advisory),
  `generation`, `capabilities` (`cand-v2`), `profile_hint`. `UdpPunch` porta
  un rider opzionale `v2` (`UdpPunchV2`: generation, peer_typed,
  peer_capabilities, `plan` sempre `None` fino alla Fase 3). Il server lo
  inoltra pass-through per i tunnel secret e lo logga; la VPN lo adotta in
  Fase 3 (`v2: None` oggi). Coppie legacy: il frame resta byte-identico
  (nessuna chiave `v2` serializzata). Metadata mancanti non implicano MAI
  `RelayOnly`.
- **Decisione crate STUN:** valutata e rimandata. Serve solo il Binding
  (RFC 5389 subset) e l'utente ha escluso IPv6/dual-stack; il transaction
  layer è ora nostro (demux per txid+sorgente) e testato con vettori
  avversariali. Una crate completa RFC 8489 entra in valutazione solo se la
  Fase 6 (RFC 5780) verrà attivata.

## 16. Connectivity check autenticati + peer-reflexive (Fase 2)

Per le coppie secret in cui ENTRAMBI i peer avvertono la capability
`check-v1` (ogni coppia new/new; il gate è il rider v2 dell'`UdpPunch`), il
round di check autenticati SOSTITUISCE il punch cieco. Peer legacy ⇒ path
vecchio byte-identico.

- **Frame** (`holepunch::check`, 60 byte fissi, richiesta e risposta della
  STESSA dimensione — il responder non è mai un amplificatore):
  `magic "bcc1" | kind | role | generation | txid(12) | observed ip:port |
  HMAC-SHA256`. Chiave = `derive_check_key(token)` (HKDF-style domain
  separation dal token del direct path). **Nessuna risposta, MAI, a un frame
  non autenticato** (HMAC errato, generation diversa, ruolo uguale, txid
  sconosciuto, sorgente diversa dal target interrogato): solo contatore
  aggregato `invalid_checks`. Risposte cappate per round
  (`CHECK_MAX_RESPONSES`).
- **Round** (`run_connectivity_checks`, budget `CHECK_WINDOW` = 1 s, pacing
  50 ms round-robin sulle coppie): una richiesta autenticata in arrivo da una
  sorgente NON offerta diventa **candidato peer-reflexive** (validato,
  dedupato, cappato come ogni lista) + triggered check immediato (throttle
  200 ms per sorgente); la prima coppia provata BIDIREZIONALE (nostra
  richiesta → risposta del peer) è **nominata**. Ogni richiesta è essa stessa
  un datagramma in uscita ⇒ il round È il punch.
- **Dopo il round**: il dialer consegna il socket a Quinn e dial la coppia
  nominata per prima (gli altri candidati partono dopo 500 ms, Happy
  Eyeballs); senza nomination dial della lista finale del round (che include
  i prflx appresi) SENZA punch ridondante. Il listener avvia il QUIC listener
  sullo stesso socket appena il proprio round chiude (break anticipato alla
  prima validazione). Relay intoccato per tutta la durata.
- **Risultato misurato (NAT lab):** la riga 3 della baseline (dialer EIM+ADF
  vs listener simmetrico) passa da RELAY a **DIRECT** con `learned_prflx`
  asserito; APDM×APDM resta RELAY (nessun falso positivo); tutte le righe
  verdi invariate. Costo worst-case sul fully-blocked: +~0,75 s prima della
  decisione di relay (1 s di round − 250 ms di punch risparmiato), pagato
  solo dalle coppie check-capable il cui UDP p2p è morto mentre STUN
  funzionava.
- La VPN 1:1 adotta i check nella Fase 3 (vedi §17); `bore test-udp` paired
  resta sul path legacy come strumento diagnostico del comportamento di base.

## 17. Checklist e policy adattiva live (Fase 3)

La policy NAT (`src/adaptive_nat.rs`) è ora usata LIVE da secret e VPN 1:1,
non più solo dal report di `bore test-udp`.

- **Profilo NAT strutturato sul wire** (`UdpNatProfile` in
  `UdpCandidateOffer.profile`, serde-default ⇒ frame legacy byte-identici):
  `mapping` (`unknown|eim|symmetric`), `filtering` (sempre `unknown` fino alla
  Fase 6 — un gather live non può osservare il filtering senza server STUN a
  due IP), `port_preserved`, `observations` (confidenza). Derivato dal gather:
  i primi DUE target della catena STUN partono INSIEME; la prima risposta
  vince il candidato (p50 invariato), una SECONDA risposta da un server
  DIVERSO — attesa bounded `PROFILE_CONFIRM_WAIT` (400 ms, mai oltre il budget
  globale) — classifica il mapping: mapped identici ⇒ EIM, diversi ⇒
  simmetrico. STUN morto ⇒ profilo con `observations: 0` (mai omesso).
- **Piano server-side** (`plan_for_pair`, kill switch
  `--no-udp-adaptive-plan` / `BORE_NO_UDP_ADAPTIVE_PLAN`): calcolato dal
  broker SOLO quando ENTRAMBE le offer portano un profilo; riempie
  `UdpPunchV2.plan` per ciascun lato (prospettiva propria). Nessun parsing di
  label testuali (`NatProfile::from_wire`); `from_summary` (label) sopravvive
  solo per il report test-udp. Metadata assenti/parziali ⇒ MAI `RelayOnly`
  (assenza = peer legacy, non NAT ostile). **Reason code stabili** nei log del
  broker (`computed adaptive traversal plan`): `both-direct-friendly`
  (DirectFirst), `symmetric-escape` (DirectWithRetry), `symmetric-relay`,
  `symmetric-strict-filtering` (APDM+APDF ⇒ RelayFirst), `peer-blocked`
  (RelayFirst), `inconclusive` / `default` (DirectWithRetry),
  `no-candidates` (RelayOnly).
- **Checklist client a gruppi staggered** (`plan_check_groups` +
  `CheckPlan`): i candidati del rider sono raggruppati per kind nell'ordine
  del piano (default data-driven: local → reflexive → router-mapped →
  predicted; allineato a `candidate_priority`); il gruppo *g* parte a
  `g × CHECK_GROUP_STAGGER` (150 ms) — né fan-out illimitato né
  serializzazione. Il piano ORDINA, non filtra: kind non citati probano in un
  gruppo finale, e nessun check predicted esiste se nessun candidato
  predicted è stato offerto (prediction off ⇒ zero probe predicted, per
  costruzione). Un prflx appreso salta in TESTA all'ordine.
- **Window/retry dal piano**: `read_timeout_ms` → window del round (clamp
  500–1500 ms, `plan_check_window`); `send_delay_ms` → delay iniziale;
  `retry_budget` → pass aggiuntive su round asciutto con pacing RADDOPPIATO
  (backoff), tutto dentro il cap duro `CHECK_TOTAL_CAP` (3 s) — il piano
  governa UN round bounded; lo scheduler esterno resta il grid VPN 30 s / il
  backoff secret. `mode: relay-only` ⇒ il client salta del tutto il tentativo
  diretto (`fallback_reason=plan-relay-only`), relay già caldo.
- **Jitter di pacing deterministico** (`check_jitter`, 0–15 ms < pace 50 ms,
  seed = chiave HMAC ⊕ ruolo ⇒ sequenze DIVERSE per i due peer senza byte sul
  wire): rompe il lockstep che innesca la crossfire race di conntrack sui
  router masquerade (pcap Fase 0).
- **Generation di round normalizzata dal broker**: i frame di check rifiutano
  generation diverse, quindi il broker (unico a vedere entrambe le offer)
  impone `max(gen_a, gen_b)` su entrambi i rider; i retry (upgrade secret,
  grid VPN) offrono generation crescenti ⇒ le reply di round vecchi vengono
  scartate. Client vecchi offrono sempre 0 ⇒ pass-through legacy.
- **Cache della coppia vincente** (`holepunch::pair_cache`, TTL 120 s,
  process-local, solo lato dialer): al reconnect/upgrade la remote che ha
  completato l'ultimo handshake QUIC viene provata per PRIMA (gruppo di testa
  extra); il primo fallimento diretto la invalida subito. Advisory: la
  membership resta l'offer fresca + sanitizer. Chiavi: `secret:<id>`,
  `vpn:<link_id>`.
- **Adozione VPN (1:1)**: offer via `CandidateDiscovery::to_offer` (typed +
  capabilities + profilo), rider v2 + piano brokerati da
  `serve_vpn_connector`, check round in `try_direct_upgrade` (listener =
  `listener_checks_then_quic`, connector = `dialer_checks_then_quic` +
  cache). L'HUB per-peer resta legacy v1 (punch cieco, `v2: None`), come il
  suo direct path single-conn.
- Peer o server legacy in QUALSIASI punto ⇒ ogni pezzo degrada al
  comportamento Fase 2/legacy (capability-gated, campo per campo).

## 18. Port mapping gestito e candidati manuali (Fase 5)

I mapping espliciti del router sono ora RISORSE VIVE (`src/portmap.rs`), non
indirizzi best-effort che scadono; e l'operatore può dichiarare endpoint
pubblici a mano quando STUN è bloccato.

- **Candidati manuali** (`--udp-candidate IP:PORT`, ripetibile /
  `BORE_UDP_CANDIDATES` comma-separated; `--udp-no-stun` /
  `BORE_UDP_NO_STUN`): su `bore local` (provider secret), `bore proxy` e
  `bore test-udp` paired. Il proprio endpoint PUBBLICO (port-forward statico,
  IP pubblico, NAT port-preserving) viene pubblicizzato PER PRIMO, sul wire
  come kind `router-mapped` (nessuna variante enum nuova ⇒ i peer vecchi
  continuano a deserializzare; un port-forward statico È un router mapping).
  `--udp-no-stun` salta l'intera catena STUN (gather in millisecondi):
  profilo `observations: 0`, e la policy NON classifica blocked un peer con
  candidato router-mapped (⇒ `DirectWithRetry`, mai relay-first per assenza
  di STUN). Senza `--udp-candidate` il no-stun logga un warn esplicito
  (quasi certamente relay). Su tunnel PUBBLICI le flag sono inapplicabili e
  warnate; sul diagnostico standalone pure. Riga NAT lab:
  `manual_candidates_no_stun_direct` (cone/cone port-preserving, STUN mai
  interrogato ⇒ DIRECT).
- **Lease gestito** (`--upnp`, stesso opt-in di prima — ora significa
  "mapping gestito"): prova **PCP (RFC 6887) MAP** verso il default gateway
  (Linux: `/proc/net/route`; altrove si passa dritti a UPnP) e in fallback
  **UPnP-IGD**. Il `LeaseHandle` rinnova a metà lifetime (richiesto 120 s),
  ritenta con backoff cappato su errore (il RELAY non è mai toccato: il
  mapping è un candidato extra, non una dipendenza), rileva il reboot del
  gateway dall'**Epoch Time** PCP (epoch regredito ⇒ stato perso ⇒
  re-acquire) e pubblica su un canale `watch` l'endpoint corrente quando
  CAMBIA. Drop dell'handle ⇒ release best-effort (PCP lifetime-0 / UPnP
  `remove_port`) — mai mapping orfani permanenti, e il RAII rilascia solo il
  PROPRIO mapping (nonce PCP per-lease; porta esterna propria su UPnP).
- **Re-offer su cambio**: il provider secret tiene il lease per tutta la vita
  del tunnel e osserva il canale; se l'endpoint esterno cambia (reboot/
  riassegnazione) ri-offre i candidati con `generation` incrementata (il
  broker la normalizza — §17). Un mapping scaduto/cambiato non viene mai
  ripubblicato a consumer nuovi: l'offer successiva porta sempre l'endpoint
  CORRENTE. Consumer/VPN/test-udp tengono il lease per la durata del
  tentativo: i loro retry ri-gatherano (e ri-acquisiscono) comunque.
- **Ordine**: PCP → UPnP → (candidati manuali sempre inclusi) → discovery
  implicita (STUN/local). Nessun mapping automatico senza l'opt-in `--upnp`;
  un eventuale `--port-map auto` separato resta rimandato (piano).
- NAT-PMP: non implementato (PCP è il successore; adapter valutabile poi).
- Gate: unit PCP wire (fake gateway loopback: acquire/renew/reboot-epoch/
  delete, frame tamper rejection), lease manager fake-clock (rinnovi oltre
  2× lifetime, cambio pubblicato, failure→backoff, release-on-drop), riga
  NAT lab manuale.

---

*Documenti correlati: `README.md` (uso e flag), `TEST_UDP.md` (scenari di test
end-to-end, incl. `bore test-udp`), `ADAPTIVE_NAT.md` (policy),
`PLAN_MANUAL_UDP_CANDIDATES.md` (piano candidati manuali — implementato),
`CLAUDE.md` / `UPSTREAM_CHANGES.md` (architettura).*
