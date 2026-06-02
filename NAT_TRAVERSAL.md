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

Il percorso diretto UDP esiste **solo per i tunnel "secret"**:

- **provider** = `bore local <porta> --tcp-secret-id <id> --udp` (espone un servizio);
- **consumer** = `bore proxy --local-proxy-port :<porta> --tcp-secret-id <id> --udp`
  (consuma il servizio su una porta locale).

Entrambi si collegano in **uscita** al **server** `bore server --udp`, che fa da
**rendezvous** (signaling) e da **STUN responder**. La modalità a porta pubblica
(`bore local 8000 --to … -p 1234`, browser → `server:porta`) **non** è
hole-punchabile (i client esterni sono arbitrari) ed è fuori ambito.

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
   (RFC 5389) al server (di default sulla porta di controllo UDP) o al
   `--stun-server` indicato. La risposta contiene l'**indirizzo riflessivo** =
   l'`IP:porta` pubblico come visto da fuori. Se lo STUN non risponde → niente
   indirizzo pubblico → di norma solo relay.

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
   `yamux` gira su **una** bidi-stream QUIC, identico al relay → tutta la logica
   per-connessione è riusata.

**Robustezza.**
- Il provider tiene un `DirectListener` **persistente** e **ri-buca** verso ogni
  nuovo consumer (nonce stabile → stesso token per tutti).
- Il consumer **rileva** la morte del path diretto (restart del provider) e si
  riconnette; un consumer **sul relay** ritenta il diretto ogni **10 s** e fa
  **upgrade in place** appena il provider diventa raggiungibile (nessuna sessione
  persa). Il sistema **converge** sempre al diretto entro ~10 s.
- **Keep-alive QUIC ogni 3 s** (idle 10 s): tiene viva la mappatura NAT durante
  trasferimenti lunghi e quieti, e rileva un peer sparito entro ~10 s.
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
| `bore test-udp [--to … --stun-server … --nat-udp-preferred-port …]` | — | **Diagnostica**: egress UDP, classe NAT (cone/symmetric), CGNAT/doppio-NAT, hairpin, UPnP. Lancialo su **entrambi** i peer. |
| `bore test-udp --to <srv> --secret <s> --tcp-secret-id <id>` | test-udp paired | **Diagnostica coordinata A<->B**: il server abbina due peer, scambia candidati, prova UDP diretto e TCP relay, e stampa un report bidirezionale. Con `--test-bandwidth --test-transfer-quota 500MB` misura anche banda e latenza su entrambi i path. |

Procedura consigliata: `bore test-udp` su provider **e** consumer → se serve una
prova end-to-end lancia la modalità paired con lo stesso id sui due host → applica
il rimedio della sezione 7 corrispondente.

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

---

*Documenti correlati: `README.md` (uso e flag), `TEST_UDP.md` (scenari di test
end-to-end, incl. `bore test-udp`), `CLAUDE.md` / `UPSTREAM_CHANGES.md`
(architettura).*
