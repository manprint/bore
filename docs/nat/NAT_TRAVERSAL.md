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
- ~~**`test-udp` rileva il mapping, non il filtering**~~ — **chiuso dalla Fase 6**
  (§19). `bore test-udp` ora misura anche il *filtering* e stampa
  `NAT filtering : …`; resta il limite che un server STUN **mono-IP** non
  separa EIF da ADF, per cui il verdetto positivo è `adf-or-eif` ("address
  dependent or open"). È la distinzione che conta: entrambi sono raggiungibili
  da un peer symmetric, `apdf` no.
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

## 19. Rilevamento del filtering (Fase 6, RFC 5780)

Fino alla Fase 5 bore misurava **una sola** delle due assi di RFC 4787: il
*mapping* (EIM vs EDM/symmetric), osservato confrontando l'indirizzo riflesso
restituito da due server STUN diversi. Il *filtering* restava `unknown` — ed è
la riga di §13 ora cancellata.

**Perché non era un limite cosmetico.** La matrice §6 dice che un provider
"Restricted Cone (EIM+ADF)" serve un consumer symmetric (✓) mentre un provider
"Port-Restricted Cone (EIM+APDF)" no (✗). I due profili hanno **lo stesso
mapping**: differiscono solo per il filtering, cioè esattamente per l'asse che
il diagnostico non misurava. Questo è ora verificato su kernel reale, non solo
asserito:

| cella (`scripts/udp_nat_netns_test.sh`) | provider | consumer | esito |
|---|---|---|---|
| `T-NAT-APDF-VS-EDM` | `eim:apdf` | `edm` | RELAY |
| `T-NAT-ADF-VS-EDM` | `eim:adf` | `edm` | **DIRECT** |
| `T-NAT-EIF-VS-EDM` | `eim:eif` | `edm` | **DIRECT** |
| `T-NAT-FIXEDPORT-VS-EDM` | `eim:apdf` + porta fissa | `edm` | RELAY |

L'ultima riga è il **controllo**: stessa porta fissa e stesso mapping
port-preserving delle due righe DIRECT, ma senza l'inoltro sul router. Resta
relay ⇒ ciò che ribalta la cella è il *filtering*, non la porta fissa.

**Come si misura.** RFC 5780 §4.4: il client chiede al server STUN di
rispondere **da un'altra porta** (attributo `CHANGE-REQUEST`, flag
change-port). La risposta arriva quindi da un `ip:porta` a cui il client non
ha **mai** scritto:

- risposta ricevuta ⇒ il filtro guarda al massimo l'**indirizzo** →
  `adf-or-eif` ("address dependent or open");
- risposta assente ⇒ il filtro guarda **indirizzo e porta** → `apdf`;
- nessun server sulla catena pubblica `OTHER-ADDRESS` ⇒ `unknown`.

Il terzo caso è il motivo per cui `OTHER-ADDRESS` è obbligatorio nella
decisione: dal socket del client "il server non sa rispondere" e "il mio NAT ha
mangiato la risposta" sono **identici** e significano il contrario. La classe
viene decisa leggendo la **sorgente** del datagramma (il 5-tuple che riporta il
kernel), mai l'attributo `RESPONSE-ORIGIN`: un server che ignora
`CHANGE-REQUEST` risponde dalla porta ordinaria compilando gli attributi come
gli pare, e solo il 5-tuple non è falsificabile dal server.

Separare EIF da ADF richiederebbe che il server risponda anche da un **IP**
diverso: un deployment mono-IP non può, quindi il verdetto positivo resta
`adf-or-eif`. Non è una perdita operativa — **entrambi** sono raggiungibili da
un peer symmetric, `apdf` no, e quella è la distinzione che decide la cella.

**Lato server.** `bore server --udp` apre un **secondo socket UDP** (il
"alternate" di RFC 5780) e vi risponde alle richieste con change-port. Porta
effimera di default; `--stun-alt-port PORT` / `BORE_STUN_ALT_PORT` la fissa,
utile solo dove il firewall in **uscita** del server filtra per porta
sorgente (con porta effimera ogni probe leggerebbe `apdf`, falsamente). Il
fallimento del bind non è fatale: il server continua a servire indirizzi
riflessi come prima e i client riportano `unknown`.

**Lato client.** La misura viaggia in due posti:

- `bore test-udp` la stampa (`NAT filtering : …`, sia locale sia del peer) con
  una frase che dice cosa comporta, non solo la sigla;
- il gather del profilo (`UdpNatProfile`) la porta nell'offerta, in due campi
  additivi: il legacy `filtering` (enum) viene messo a `AddressDependent` —
  il cui significato documentato è proprio "address- **o** port-dependent" —
  **solo** quando il probe è stato bloccato, e il nuovo `filtering_probe`
  porta la lettura precisa. Un probe passato NON scrive `Eif` sul campo
  legacy: proverebbe solo che non è APDF, e dichiarare "full cone" su quella
  evidenza sarebbe una supposizione.

Regola di compatibilità applicata: **campo** nuovo sì, **variante** nuova di un
enum esistente no. Un peer vecchio ignora un campo che non conosce, ma va in
errore duro su una variante sconosciuta di un campo che conosce. Per lo stesso
motivo `filtering_probe` ha esattamente i due stati che un probe mono-IP può
produrre e "non misurato" è l'`Option::None`, non una terza variante.

**Costo.** Due datagrammi su un socket già aperto, dentro il gather esistente,
solo quando un indirizzo riflesso è già stato trovato: senza mapping non c'è
nulla da misurare, e un probe verso un server irraggiungibile scadrebbe e
verrebbe letto come NAT restrittivo — la direzione costosa in cui sbagliare,
perché è quella che spinge il piano verso il relay.

**Gate**: unit `change_request_flags_round_trip`,
`a_response_without_the_new_attributes_is_the_legacy_encoding` (zero-regressione
sul wire), `a_response_with_the_new_attributes_still_carries_the_mapping`,
`only_a_blocked_probe_sets_the_legacy_filtering_field`; integrazione su socket
reali `a_server_with_an_alternate_socket_answers_from_the_other_port` e
`a_server_without_an_alternate_socket_is_unsupported_not_blocked`; campo
`T-NAT-FILTER-APDF` / `T-NAT-FILTER-ADF` in `scripts/udp_nat_netns_test.sh`
(kernel reale: il diagnostico deve **riportare** il profilo del router che gli
sta davanti).

---

*Documenti correlati: `README.md` (uso e flag), `TEST_UDP.md` (scenari di test
end-to-end, incl. `bore test-udp`), `ADAPTIVE_NAT.md` (policy),
`PLAN_MANUAL_UDP_CANDIDATES.md` (piano candidati manuali — implementato),
`CLAUDE.md` / `UPSTREAM_CHANGES.md` (architettura).*

**Dove viene stampata.** `bore test-udp` ha due report: quello **standalone**
(host singolo) e quello **appaiato** (`--tcp-secret-id`, che mostra anche il
peer). La riga `NAT filtering` compare in entrambi e passa per un'unica
funzione (`FilterProbe::describe`), perché un operatore che li legge in
sequenza non deve imparare due vocabolari per la stessa misura. Nel report
standalone il probe usa, in ordine, il server `--to` e poi `--stun-server`:
l'indirizzo **già risolto** di chi ha risposto, mai una nuova risoluzione del
nome — un nome in round-robin risolverebbe su un altro IP e misurerebbe un
percorso NAT diverso da quello appena classificato.

**Isolamento delle celle del banco (trappola trovata sul campo).** Le due
celle diagnostiche giravano verdi da sole e `T-NAT-FILTER-ADF` falliva subito
dopo `T-NAT-FILTER-APDF`, riportando `apdf` su un router ADF. Causa: una
catena nat viene attraversata **solo dal primo pacchetto di un flusso**, e con
una porta sorgente fissa per tutta la matrice il flusso STUN della cella N+1
riusava la entry conntrack della cella N — le regole appena installate non
giravano mai, il set `@punched` restava vuoto e il banco riportava il router
*precedente*. `flush_conntrack` avrebbe dovuto impedirlo e non poteva: il
binario `conntrack` non è installato ovunque e la chiamata falliva in silenzio
(`2>/dev/null || true`). Ora ogni cella prende la propria coppia di porte
(`cell_ports`, numerata sulla posizione nel file, quindi stabile anche
rieseguendo una cella sola) e il flush è dichiaratamente best-effort. Regola
generale: un banco che *crede* di aver ripulito lo stato è peggio di uno che
non ci prova, perché fabbrica risultati invece di fallire.

**La misura cambia la policy, e non nel modo intuitivo.** Con il filtering
misurato, `adaptive_nat.rs` ha finalmente un input non-`Unknown` per la Fase 6
— e la regola che c'era codificava l'asse sbagliato. Guardava il filtering del
lato *symmetric*; la matrice su kernel reale dice che decide quello dell'**altro
lato**:

| local | peer | modo | `reason_code` |
|---|---|---|---|
| `eim` + `apdf` | symmetric | RelayFirst | `peer-port-restricted` |
| `eim` + `adf/eif` | symmetric | DirectWithRetry | `symmetric-vs-open-filter` |
| symmetric + `apdf` | symmetric | RelayFirst | `symmetric-strict-filtering` |
| qualunque, filtering non misurato | symmetric | *come prima della Fase 6* | `symmetric-escape` / `symmetric-relay` |

Il perché è asimmetrico e vale scriverlo, perché l'intuizione sbaglia. Devono
passare **due** pacchetti. Quello `symmetric → altro` arriva da una porta
sorgente che l'altro lato non ha mai potuto scrivere: quindi il filtro
dell'**altro** lato deve essere più largo di APDF. Quello `altro → symmetric`
arriva invece dal mapping stabile e endpoint-independent dell'altro lato, a cui
il symmetric **ha** scritto: quindi anche un filtro APDF sul lato symmetric lo
lascia entrare. Il lato che deve accettare una porta sorgente imprevedibile è
quello non-symmetric, ed è l'unico il cui filtering decide la cella.

`None` (mai misurato) non è mai trattato come `Some(true)`: dal socket un
filtro non misurato e uno restrittivo sono identici e significano il contrario,
e sbagliare verso "restrittivo" spinge sul relay una coppia che avrebbe
bucato. Un peer senza Fase 6 prende esattamente la decisione pre-Fase-6
(gate `an_unmeasured_filter_keeps_the_legacy_symmetric_decision`).

**Confronto con lo stato dell'arte.** Dove bore sta rispetto a Tailscale, frp,
libp2p DCUtR e ICE — e quali lacune restano, con il costo di ciascuna — è in
[`NAT_SOTA_COMPARISON.md`](NAT_SOTA_COMPARISON.md).

---

## 20. L'escape spray: birthday paradox per `eim:apdf × symmetric` (Fase 7)

### 20.1 La cella che nessun round ordinario può vincere

La matrice del §19 misura una sola cella RELAY con esattamente un lato
symmetric: `eim:apdf × edm`. Un lato con mapping stabile ma filtro
address+port-dependent, di fronte a un lato la cui porta sorgente nessuno può
prevedere. Nessun meccanismo ordinario la raggiunge:

* la *port prediction* non ha niente da predire contro un `fully-random`;
* ogni probe del check round va, per costruzione, sulla porta sbagliata;
* il lato symmetric non può essere mirato, e il lato `apdf` scarta tutto ciò
  che arriva da una `(ip, porta)` a cui non ha scritto.

È la coppia "home router + mobile/CGNAT", cioè la più comune fra quelle che
falliscono.

### 20.2 Il meccanismo: un rendez-vous nello spazio delle porte

L'escape funziona perché i due lati sono asimmetrici in modo **complementare**:

* il lato **easy** (mapping endpoint-independent) spruzza una check request
  autenticata su molte porte di destinazione dell'IP pubblico del peer. Ognuno
  di quei pacchetti apre il **proprio** filtro per quella `(ip peer, porta)` —
  esattamente la direzione che era bloccata — e non gli costa nulla, perché la
  sua porta sorgente resta la stessa per tutte;
* il lato **hard** (mapping symmetric) apre molti socket ausiliari e da ognuno
  scrive all'indirizzo stabile del lato easy. Ogni socket compra un biglietto
  in più nell'estrazione.

Una sola collisione apre **entrambe** le direzioni insieme: il pacchetto del
lato easy raggiunge il socket ausiliario (il NAT del lato hard ha un mapping su
quella porta la cui reply tuple è proprio l'indirizzo stabile del lato easy) e
la risposta raggiunge il lato easy (il cui filtro è stato aperto da quello
stesso pacchetto). Per questo non servono né un terzo attore, né un secondo
round trip, né un protocollo di retry: il primo frame che arriva da qualche
parte *è* la risposta.

### 20.3 La matematica e la taratura

Con `S` porte spruzzate su uno spazio `P` e `Q` socket ausiliari:

```
p = 1 − (1 − S/P)^Q
```

I default spediti — `S = 3 × 256 = 768` su `P = 64512`, `Q = 256` — danno
**≈ 95 %**, al costo di circa 2 300 datagrammi da 60 byte fra i due lati e di
pochi secondi di un budget che altrimenti sarebbe stato speso a cadere sul
relay. I passaggi usano insiemi di porte **diversi**: il filtro del lato easy
resta aperto per ogni porta già spruzzata (conntrack tiene l'entry ben più a
lungo della finestra), quindi il passaggio `n` si somma ai precedenti invece di
ripeterli, e più passaggi assorbono anche lo sfasamento di partenza fra i due
lati.

Non viene usato per una coppia **hard × hard**: lì la porta stabile del lato
easy non esiste, entrambi gli insiemi sono estratti, e arrivare al 99,9 %
richiede dell'ordine di 170 000 probe (≈ 28 minuti a 100 pkt/s) — costo
calcolato e rifiutato in [`NAT_SOTA_COMPARISON.md`](NAT_SOTA_COMPARISON.md)
§4.1.

### 20.4 Chi decide i ruoli, e perché sul server

`UdpAdaptivePlan.spray_role` (`"easy"` / `"hard"`) è calcolato dal broker —
l'unico che vede **entrambi** i profili — e solo per `reason_code ==
"peer-port-restricted"`, cioè esattamente la cella del §20.1. I due rider della
stessa coppia portano quindi valori **complementari**, e ogni lato sceglie la
propria metà senza un round trip in più e senza conoscere il profilo del peer.

Due gate lo tengono onesto:

* è una `Option<String>`, non un enum: un broker più nuovo deve poter nominare
  un ruolo che questo binario non conosce senza che sia un errore di protocollo
  (stessa regola di `reason_code`);
* è subordinato al fatto che **entrambi** i peer annuncino la capability
  `spray-v1`. L'escape è un rendez-vous: un ruolo dato a un peer il cui partner
  non sa giocarlo è peggio di nessun ruolo, perché quel peer spenderebbe tutto
  il budget di fallback spruzzando verso un lato che non sta estraendo.

Il kill switch resta `--no-udp-adaptive-plan` lato server: il ruolo viaggia sul
piano, quindi togliere il piano toglie anche l'escape.

### 20.5 Il conferma-uno-solo (bug trovato dal gate, non ragionato)

Con un'estrazione generosa i due lati collidono su **più** porte insieme. Se
ognuno tenesse la prima che gli arriva, terrebbero due prime diverse: due
coppie diverse, e una dial verso un socket su cui nessuno ascolta. Misurato
subito dal gate su loopback, dove il numero di collisioni è alto per
costruzione.

Quindi il lato easy decide da solo e **lo dice**: risponde al frame che lo ha
raggiunto e poi manda tre request che portano il transaction id che il peer ha
**già** visto. Una request spruzzata porta sempre un id fresco a 96 bit, quindi
"un id che ho già visto" è un discriminante che lo spray stesso non può
contraffare, e un solo socket ausiliario può riceverlo.

### 20.6 Il socket promosso

Sul lato hard il socket vincente **sostituisce** quello del round, ma solo se
l'escape ha davvero vinto: un escape fallito restituisce il socket originale
intatto, così il fallback che segue è esattamente quello che sarebbe girato
comunque. Il socket promosso è restituito per valore, con il suo reader già
terminato, e va a Quinn sotto la stessa regola *un socket = un reader* di ogni
altro percorso diretto; i buffer glieli configura il costruttore dell'endpoint
(P-13), non il chiamante. I perdenti vengono abortiti e i loro socket rilasciati
**prima** che il vincitore venga consegnato.

Il numero di socket ausiliari è limitato a metà dei descrittori rimasti
(`fdlimit::fd_headroom`): sono un esperimento transitorio dentro un processo che
sta anche servendo un tunnel vivo, e `EMFILE` cade sull'`accept()` di **ogni**
listener del processo (P-12). Sotto 32 socket l'escape rinuncia invece di
spendere la finestra.

### 20.7 Seconda osservazione di mapping via OTHER-ADDRESS (RFC 5780 §4.3)

Classificare il mapping richiede due osservazioni da **due indirizzi server
diversi**, e spesso solo uno risponde: `--stun-server HOST:PORT` costruisce per
costruzione una catena di **un solo** elemento, e un deployment privato non ha
STUN pubblici su cui ripiegare. Una sola osservazione ⇒ `mapping: Unknown` ⇒
ogni policy che dipende dal mapping è disattivata, in silenzio, proprio sui
deployment che hanno configurato il proprio server. Era il caso del banco netns
prima della Fase 7.

Il secondo indirizzo è già pubblicato e già usato: l'OTHER-ADDRESS del server
stesso, lo stesso attributo che serve alla probe di filtering. Una binding
request in più verso di esso, dentro il budget già esistente della catena,
risponde alla domanda.

Due cautele, entrambe necessarie:

1. **È deliberatamente conservativa.** Il socket alternato differisce dal
   primario per la sola **porta**, quindi un reflexive diverso prova che il
   mapping è almeno port-dependent (symmetric, in questo modello) mentre uno
   **identico** non prova l'endpoint-independence — anche un NAT
   address-dependent risponderebbe identico qui e darebbe comunque una porta
   diversa a un peer su un altro IP. Perciò questo fallback può concludere
   `Symmetric` e non può mai concludere `Eim`: sbagliare verso "symmetric"
   costa un budget di retry, sbagliare verso "endpoint-independent" costa un
   percorso diretto pianificato che non può esistere.
2. **L'ordine è portante.** Gira **dopo** la probe di filtering, mai prima.
   Quella probe chiede al server di rispondere dalla porta alternata e misura
   se la risposta passa — e il fallback di mapping **scrive** proprio a quel
   `(ip, porta)`, aprendo un filtro address+port-dependent per esso. Misurato,
   non ragionato: con i due invertiti, il router `masquerade` semplice del banco
   riportava `adf-or-eif` invece di `apdf`, cioè la misura descriveva l'impronta
   della probe stessa.

Un server bound su wildcard pubblica il socket alternato come `0.0.0.0:porta`,
che è onesto (non conosce il proprio IP pubblico) e irraggiungibile: il client
ripara l'indirizzo con l'IP di chi ha appena risposto invece di saltare la
misura.

### 20.8 Gate

| Gate | Dove | Cosa prova |
|---|---|---|
| `the_collision_probability_is_the_one_the_documentation_quotes` | `src/holepunch.rs` | i numeri del §20.3 sono quelli che il codice usa |
| `sprayed_ports_are_distinct_and_allocatable` | `src/holepunch.rs` | nessun doppione, nessuna porta privilegiata (abbasserebbero `p` sotto quella dichiarata) |
| `the_sprayed_escape_rendezvous_finds_a_pair` | `src/holepunch.rs` | il rendez-vous completo su loopback: i due lati si trovano, il vincitore è promosso per valore, i perdenti vengono puliti |
| `the_escape_never_nominates_a_peer_that_claims_its_own_role` | `src/holepunch.rs` | su loopback il lato easy spruzza prima o poi la **propria** porta: senza la guardia un host nominerebbe sé stesso |
| `the_escape_is_skipped_without_a_role_and_on_a_nominated_round` | `src/holepunch.rs` | costo zero fuori dalla cella |
| `the_two_sides_of_the_escape_cell_get_complementary_spray_roles` | `src/adaptive_nat.rs` | i due rider della stessa coppia sono complementari (se no non si incontrano mai) |
| `no_other_cell_is_given_a_spray_role` | `src/adaptive_nat.rs` | nessun'altra cella paga la finestra |
| `the_spray_role_reaches_the_wire_only_when_both_peers_can_play_it` | `src/secret.rs` | il gate di capability |
| `T-NAT-SPRAY-OFF` | `scripts/udp_nat_netns_test.sh` | la cella **resta** non vincibile dal round ordinario, rimisurata a ogni run |
| `T-NAT-SPRAY-DIALER` | `scripts/udp_nat_netns_test.sh` | l'escape la ribalta su kernel vero, con promozione lato dialer |
| `T-NAT-SPRAY-LISTENER` | `scripts/udp_nat_netns_test.sh` | idem con i ruoli invertiti: promuovere un socket ausiliario lato **listener** (un server QUIC costruito su un socket che non esisteva a inizio round) è un percorso di codice diverso |

Le tre celle netns sono **un solo esperimento con una sola variabile**: la cella
OFF rimisura la baseline a ogni run invece di fidarsi di un messaggio di commit,
le due ON la ribaltano. Se un cambiamento futuro rompe l'escape, OFF passa e ON
fallisce, e lo dice con precisione. Se rompe la **cella** — per esempio facendola
vincere al round ordinario — fallisce OFF, ed è il fallimento più interessante.

Misura di campo (banco netns, taratura deterministica): `escape_ms=51`, cioè il
rendez-vous si chiude in **51 ms** dal momento in cui il round ordinario si è
dichiarato a vuoto.

### 20.9 Tarature

| Variabile | Default | Cosa cambia |
|---|---|---|
| `BORE_UDP_SPRAY_PORTS` | 256 | porte distinte spruzzate per passaggio |
| `BORE_UDP_SPRAY_PASSES` | 3 | passaggi (insiemi di porte diversi, l'unione conta) |
| `BORE_UDP_SPRAY_SOCKETS` | 256 | socket ausiliari del lato hard |
| `BORE_UDP_SPRAY_PACE_US` | 3000 | intervallo fra due pacchetti spruzzati |
| `BORE_UDP_SPRAY_REPEAT_MS` | 2500 | ogni quanto un socket ausiliario ripete |
| `BORE_UDP_SPRAY_CAP_MS` | 6000 | tetto dell'intero escape; **0 disattiva** |

Esistono perché l'unico modo onesto di gatare questo meccanismo è portare la
probabilità di collisione a ~1 su un banco e osservare il path ribaltarsi, e i
default giusti su una rete vera (burst limitato, impronta conntrack limitata)
non sono quelli che rendono un test deterministico. Stesso precedente di
`BORE_CTRL_HEARTBEAT_MS` e `BORE_DIRECT_OPEN_TIMEOUT_MS`.

### 20.10 Un errore di `recv` sul socket dello spray è TRANSITORIO, mai fatale

Lo spray manda a centinaia di porte di cui al massimo una è aperta: per
costruzione quasi ogni pacchetto si guadagna un ICMP port-unreachable. Il
punto è **dove** arriva quell'errore. Windows lo consegna sul socket che ha
*inviato*, come `WSAECONNRESET` sulla successiva `recv_from`; Linux fa la
stessa cosa con `ECONNREFUSED`, ma solo su un socket connesso — e un socket
UDP **non connesso**, come tutti quelli dello spray, su Linux non viene
informato affatto degli errori ICMP.

Conseguenza: il datagramma che dimostra che l'escape ha funzionato arriva su
un socket la cui coda di ricezione è piena di errori causati dalle sonde
dell'escape stesso. La prima versione di `spray::listen` e di
`aux_socket_round` trattava `Ok(Err(_))` come fine dell'escape
(`return None`), che su Windows lo rompeva del tutto: il primo ICMP di ritorno
chiudeva il giro prima che qualunque risposta potesse essere letta.

Misurato, non ragionato: `the_sprayed_escape_rendezvous_finds_a_pair` passava
in locale e su ogni runner Linux, e falliva su `windows-latest` e sul cross
check `x86_64-pc-windows-msvc` con *«the easy side found no pair»*
(`src\holepunch.rs:6911`).

La correzione segue il precedente già presente nel modulo — `recv_actor`, il
lettore unico del socket di traversal, il cui commento dice esattamente perché
non muore su un errore di `recv`: entrambi i loop dello spray dormono 5 ms e
continuano, e la scadenza (il `cap`) è ciò che li termina. La pausa serve a
non trasformare una raffica di errori già in coda in un busy-spin.

Regola operativa che ne deriva, valida per qualunque loop futuro su un socket
di punch: **l'unico errore che può terminare un giro è la scadenza**, non un
errore per datagramma. E l'oracolo di questa classe di difetti è il job
`windows-latest` della CI, perché su Linux il difetto è irreproducibile per
proprietà del kernel — stessa regola già in vigore per il backend macOS.

## 21. La consegna del socket al QUIC: perché il *listener* non deve aspettare (S-5)

Questa sezione non descrive una tecnica di traversal nuova. Descrive il
**confine** fra il giro di check e il QUIC, che è dove, misurando, si nascondeva
il secondo di latenza più costoso dell'intero percorso diretto.

### 21.1 La forma del problema

`listener_checks_then_quic` fa tre cose *in sequenza*: esegue il giro di
connectivity check, riprende il socket dall'attore (`into_socket`), e solo a
quel punto costruisce l'endpoint QUIC. Il socket è uno solo e ha un solo
proprietario alla volta — è l'invariante di `UdpTraversalSocket`, ed è giusta.
Ne segue però una conseguenza che non era stata prezzata: **finché il giro dura,
su quel socket non c'è nulla che sappia rispondere a un Initial QUIC.**

Dall'altra parte il dialer fa il contrario: appena una coppia è validata,
nomina, *disabilita il proprio responder* («i frame in ritardo si contano, non si
rispondono» — è il contratto del giro) e chiama. Il dialer, cioè, smette di
rispondere esattamente quando il listener avrebbe più bisogno di una risposta.

Se il piano adattivo del listener mette i candidati *locali* del peer nel primo
gruppo — cosa corretta, perché una coppia in LAN si chiude lì — il gruppo
riflessivo arriva un `CHECK_GROUP_STAGGER` (150 ms) più tardi, e a quel punto
l'indirizzo che sta sondando ha già smesso di rispondere. Il suo giro finisce
*a secco*, quindi consuma tutta la finestra.

### 21.2 La misura (staging, 2026-09-11)

`direct_ready_ms` sul consumer, 27 stabilimenti, topologia `vm-ws`:

```
37 40 41 42 42 43 43 44 44 48 49 49 50 52 52 52 52 53      <- 18
1036 1043 1043 1044 1044 1045 1050 1052 1162               <-  9
```

Bimodale, senza nulla in mezzo. Una distribuzione con un buco così non è mai la
rete: è un timer. E il timer si identifica in una riga: `333 ms + 4 × 166 ms =
999 ms` è il PTO iniziale di quinn con l'`initial_rtt` di default della RFC
9002. Il primo Initial viene perso perché arriva mentre il listener è ancora nel
giro, e non viene ritrasmesso prima di un secondo.

I log lo dicono in chiaro, appaiati:

```
consumer  role=Dialer   nominated=Some(...)  checks_ms=213   -> direct_ready_ms=1050
provider  role=Listener nominated=None       checks_ms=1126
```

La precondizione lato listener è contabile: **8 giri su 52** lato VM sono finiti
`nominated=None`, contro **0 su 22** lato workstation. Stessa asimmetria dello
stallo, stessa proporzione.

### 21.3 La correzione

Un listener che ha appena risposto a una richiesta autenticata possiede già
tutto ciò che la sua metà del giro può produrre: il peer ha la chiave, è su
questa generazione, gioca il ruolo opposto e **ci raggiunge** da quel `src`.
Tutto ciò che viene dopo è una mossa del dialer, e il dialer la fa non appena la
propria nomina si completa — cosa che la nostra risposta è ciò che provoca.
Restare nel giro non migliora il percorso: tiene solo il socket lontano dal QUIC
proprio mentre il primo Initial sta arrivando.

Quindi il listener nomina la sorgente autenticata ed esce. Due dettagli portano
il peso:

* **la risposta va sul filo prima che il giro venga smontato.** L'attore
  annunciava la richiesta al driver mentre teneva ancora i byte di risposta;
  siccome finire il giro ferma l'attore, annunciare per primo poteva mangiarsi
  l'unico datagramma che il dialer sta aspettando. `CheckAction` porta `reply` e
  `announce` separati, e `recv_actor` **prima invia, poi annuncia**;
* **`nominated` viene valorizzato**, non lasciato vuoto: è ciò che disattiva
  l'escape spray della Fase 7, e spendere sei secondi di spray per un peer che
  ci ha appena raggiunto sarebbe lo stesso errore in formato più grande.
  `observed` invece resta `None` per costruzione — si può imparare solo da una
  *risposta* a una nostra richiesta — e nessun chiamante lo consuma.

### 21.3b Perché NON c'è un periodo di grazia

L'obiezione naturale è: il listener potrebbe continuare a rispondere ancora per
qualche decina di millisecondi, per coprire il caso in cui la *sua* risposta si
perda e il dialer debba richiedere. L'obiezione si respinge con l'aritmetica del
caso comune, non con una preferenza di stile.

Se il dialer riceve la risposta, nomina e chiama: il suo primo Initial arriva
circa **un RTT** dopo. Su questo percorso l'RTT è ~20 ms. Una grazia di 100 ms
lascerebbe quindi il socket fuori dal QUIC proprio mentre l'Initial arriva — cioè
reintrodurrebbe esattamente il difetto, per *tutti* i giri, allo scopo di
proteggere il sottoinsieme in cui una risposta si perde.

E quel sottoinsieme non resta scoperto: un dialer che non nomina chiama comunque
la lista di target del giro (compresi i candidati peer-reflexive appresi), e
trova l'endpoint QUIC già in ascolto. Degrada a «chiama senza nomina», che
funziona; la grazia degraderebbe il percorso normale, che oggi funziona in 40 ms.

### 21.4 E un endpoint diretto non è mai «freddo» (S-7)

I 333 ms di `initial_rtt` della RFC 9002 sono il valore per una connessione che
non sa **nulla** del percorso. Un endpoint diretto di bore non è mai in quella
posizione: viene costruito solo dopo uno scambio di check autenticato con quel
peer, oppure dopo una connessione TCP di controllo verso quell'host.

I due errori non sono simmetrici. Sottostimare costa **un** Initial duplicato su
un percorso più lento, dopodiché governa il primo campione vero. Sovrastimare
costa un PTO intero di silenzio ogni volta che il primo Initial si perde — ed è
il pacchetto con la probabilità di perdita più alta di tutta la connessione,
perché è il primo datagramma che attraversa una mappatura che il peer ha appena
creato.

`DIRECT_INITIAL_RTT` vale quindi 100 ms (PTO ≈ 300 ms), con override
`BORE_DIRECT_QUIC_INITIAL_RTT_MS` e clamp in [10 ms, 333 ms]: sopra il default
della RFC non esiste nessun valore che si possa chiamare «informato». È una
difesa in profondità, non un sostituto: S-5 toglie la causa sistematica, S-7
limita il costo di una perdita genuina.

### 21.5 Diradare gli ACK: misurato e scartato (S5)

L'estensione QUIC ACK Frequency (draft-ietf-quic-ack-frequency-04) permette di
chiedere al peer di confermare al massimo una volta ogni `soglia + 1` pacchetti
ack-eliciting. Su un percorso diretto saturo toglierebbe quasi tutto il traffico
di ritorno, quindi valeva la misura: entrambi gli estremi di un percorso diretto
di bore sono bore, e l'estensione è sempre negoziabile.

**Il risultato è negativo e la manopola resta spenta.** `sec_ack.sh`, `vm-vm`
(nessuna WAN di mezzo, quindi il conto è tutto CPU e pacchetti), 128 MiB su 4
connessioni, 5 coppie appaiate, soglia 10 contro il default:

```
  coppia    default     ack=10    rapporto
  1         383.46     206.86     0.539
  2         402.16     387.85     0.964
  3         403.65     251.14     0.622
  4         393.86       7.91     0.020      <- 393.86 -> 7.91 MB/s
  5         379.64     392.92     1.035
  mediana 0.622  (n=5 su 5)
```

Non è «un po' peggio»: è **bimodale**. Due coppie stanno alla pari (0.964,
1.035), una crolla di 50 volte. La forma della distribuzione è il risultato,
non la mediana.

**Il meccanismo, ed è il motivo per cui la manopola oggi richiede due valori.**
Impostando solo la soglia, `AckFrequencyConfig::max_ack_delay` resta a `None`,
che quinn documenta come «viene usato il `max_ack_delay` originale del peer,
preso dai suoi transport parameter» — cioè **25 ms** di default. Su un percorso
diretto in regione l'RTT sta sotto il millisecondo, quindi un ricevente che non
ha ancora accumulato `soglia + 1` pacchetti si siede sull'ACK per **centinaia di
RTT**. Che una data connessione finisca o no in quello stato dipende da quanto
le sue fasi stanno sopra la soglia: è esattamente la bimodalità misurata.

Di conseguenza `BORE_DIRECT_QUIC_ACK_THRESHOLD` da solo viene **rifiutato** con
un `warn!` e la politica ACK resta quella di quinn; serve anche
`BORE_DIRECT_QUIC_ACK_MAX_DELAY_MS`. Una soglia senza un limite di ritardo non
misura ACK più radi: misura un ritardo di ACK da 25 ms.

**Cosa resta non misurato.** Questa campagna ha misurato la trappola, non l'idea:
una soglia con un `max_ack_delay` esplicito e piccolo (dell'ordine dell'RTT del
percorso) non è stata provata, e potrebbe benissimo essere neutra o utile su una
tratta ad alto BDP. È un esperimento successivo, con la manopola che adesso
permette di condurlo correttamente.

### 21.6 La banda del percorso diretto non è decisa dal traversal (window floor)

Chiude un equivoco che questa campagna ha visto nascere: «il percorso diretto è
più lento del relay, quindi il traversal ha scelto una coppia scadente».
Misurato il 2026-09-12 sulla coppia `ws ↔ vm`, con il buco riuscito e la coppia
giusta nominata:

```
UDP direct path : sent 1.86 GiB in 42.46s (376.79 Mbit/s)
UDP direct path QUIC : rtt 19.13 ms, cwnd 10.31 MiB, loss 0 pkts / 0 B
TCP relay fallback : sent 1.86 GiB in 24.34s (657.25 Mbit/s)
```

`loss 0` esclude la rete; `cwnd 10.31 MiB` contro una finestra di ricezione di
stream da 1 MiB dice che il controllo di congestione ha aperto dieci volte
quello che al flusso è permesso usare. Il limite è `finestra / RTT` = 438
Mbit/s, e la misura ne è l'86 %.

La finestra da 1 MiB non viene dal traversal: viene da
`UdpDirectTuning::from_memory_budget`, cioè da `--udp-memory-budget` diviso
`--max-carriers` (dettagli e rimedi nel README e in
[`../performance/final_secret_perf_review.md`](../performance/final_secret_perf_review.md)
§5.7). **Nessuna manopola di traversal la cambia**, e nessun difetto di
traversal la produce.

La regola operativa per chi diagnostica: prima di sospettare il buco, leggere
la riga `UDP direct path tuning` di `bore test-udp`, che stampa le finestre
davvero in vigore accanto ai default. Se `stream recv` è sotto il default, la
banda di **un** flusso è già spiegata dall'aritmetica e il traversal non
c'entra.
