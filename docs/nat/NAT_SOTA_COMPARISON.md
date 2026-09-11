# NAT traversal: bore contro lo stato dell'arte

Confronto del traversal UDP di bore (tunnel segreti `--udp`, e per estensione
VPN 1:1) con le soluzioni che definiscono oggi lo stato dell'arte, per
stabilire due cose: **dove siamo già allineati** e **quali sono le lacune che
vale la pena colmare**, con il costo di ciascuna.

Scritto il 2026-09-11, a valle della Fase 6 (misura del filtering, §19 di
[`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md)) e della matrice A×B verificata su
kernel reale (`scripts/udp_nat_netns_test.sh`).

Ogni affermazione su un prodotto terzo è tratta dalla sua documentazione
pubblica, citata; dove la documentazione non dice, il documento dice che non
dice invece di supporre.

---

## 1. Le soluzioni confrontate

| soluzione | modello | relay | fonte |
|---|---|---|---|
| **Tailscale** (`disco` + DERP) | mesh WireGuard peer-to-peer, coordinamento su control plane | DERP, sempre presente, connessione **nasce** sul relay e viene promossa a caldo | [how NAT traversal works](https://tailscale.com/blog/how-nat-traversal-works), [connection types](https://tailscale.com/kb/1257/connection-types) |
| **frp** (`xtcp`) | reverse proxy con modalità P2P opzionale | `stcp` (relay) come modalità **separata** che l'utente sceglie a mano | [README](https://github.com/fatedier/frp) |
| **libp2p DCUtR** | upgrade di una connessione relay a diretta, senza signaling server dedicato | circuit relay, resta attivo se il punch fallisce | [spec DCUtR](https://github.com/libp2p/specs/blob/master/relay/DCUtR.md) |
| **WebRTC / ICE** | checklist di coppie candidate con priorità, nomination | TURN | RFC 8445 / 8656 |
| **bore** (`--udp`) | tunnel segreto consumer↔provider, server come broker | relay TCP **warm** per tutta la vita del tunnel, fallback per singola connessione | questo repository |

---

## 2. Dove bore è già allo stato dell'arte

**Check di connettività autenticati.** Ogni frame di check porta un HMAC
derivato dal token del tunnel, richiesta e risposta hanno la stessa dimensione,
e un frame con generazione o ruolo sbagliati non viene mai risposto (Fase 2).
È la stessa proprietà che ICE ottiene con `MESSAGE-INTEGRITY` sulle Binding
Request, e che frp `xtcp` — per quanto la sua documentazione descrive — non ha:
lì l'autenticazione è il `secretKey` del proxy, non il singolo probe.

**Candidati tipizzati con priorità e ordinamento server-side.** bore calcola un
*piano adattivo* sul server a partire dai profili NAT di **entrambi** i peer e
lo invia ai due lati (Fase 3). Né frp né DCUtR hanno un equivalente: DCUtR
sincronizza il momento dell'apertura ma non ordina i candidati in base al NAT
dell'altro; ICE ordina per priorità **locale**, non per una policy che conosce
entrambe le coppie. Il piano **ordina e non filtra** (tranne `RelayOnly`), che è
esattamente il modo prudente di usarlo.

**Port mapping gestito.** PCP (RFC 6887) con fallback UPnP-IGD, rinnovo a metà
lifetime, rilevazione della *epoch regression* (riavvio del gateway) e rilascio
al drop. Tailscale documenta la stessa terna (UPnP IGD, NAT-PMP, PCP) come uno
degli strumenti principali; frp non la offre affatto. bore la tiene dietro
`--upnp` (opt-in) mentre Tailscale la prova sempre: vedi §4.2.

**Relay sempre caldo, fallback per connessione.** La connessione nasce sul relay
e il diretto viene tentato in parallelo; se il diretto muore si torna al relay
*in place*. È lo stesso principio che Tailscale descrive ("all connections
start out with DERP preselected… usually, after a few seconds, we'll have found
a better path"). frp `xtcp` invece **non** ha fallback automatico: la sua
documentazione dice esplicitamente «it may not work with all types of NAT
devices. You might want to fallback to stcp if xtcp doesn't work» — cioè è
l'utente a cambiare configurazione.

**Misura del filtering (RFC 5780).** Questa è la voce in cui bore va **oltre**
il comportamento documentato di Tailscale. Il loro articolo afferma
esplicitamente che la dimensione del firewall non interessa al codice di
traversal — «our simultaneous transmission trick will get through all three
variants of firewalls» — e riduce la tassonomia a *easy* (EIM) vs *hard* (EDM).
Per la coppia EIM×EIM è vero. Per la coppia EIM×EDM **non** lo è, ed è
misurato su kernel reale in `scripts/udp_nat_netns_test.sh`:

| provider | consumer | esito misurato |
|---|---|---|
| `eim:apdf` | `edm` | RELAY |
| `eim:adf` | `edm` | **DIRECT** |
| `eim:eif` | `edm` | **DIRECT** |
| `eim:apdf` + porta fissa | `edm` | RELAY (controllo) |

Tre celle con lo stesso mapping su entrambi i lati e esito opposto: l'asse che
decide è il filtering del lato **non** symmetric. Tailscale arriva allo stesso
risultato per un'altra strada (il birthday paradox, §4.1), che è più potente ma
molto più costosa; bore oggi sa **dire** in anticipo in quale cella si trova, e
usa l'informazione per ordinare il piano.

**Anti-crossfire.** Il jitter di invio dei check è derivato dal ruolo, per
rompere il lockstep del conntrack sui router masquerade — un fenomeno che bore
ha diagnosticato con pcap e che non compare nella documentazione di nessuna
delle altre soluzioni.

---

## 3. Tabella di sintesi

| capacità | Tailscale | frp `xtcp` | DCUtR | bore |
|---|---|---|---|---|
| STUN / reflexive | sì | sì | via identify | sì |
| classificazione **mapping** (EIM/EDM) | sì | no | no | sì |
| classificazione **filtering** (RFC 5780) | non documentata | no | no | **sì (Fase 6)** |
| check autenticati | sì (disco, chiavi) | no | — (usa il transport) | **sì (HMAC, Fase 2)** |
| piano adattivo sui profili di **entrambi** | no | no | no | **sì (Fase 3)** |
| sincronizzazione simultanea (RTT/2) | implicita | no | **sì** | no (§4.3) |
| birthday paradox per EDM | **sì (256×256)** | no | no | no (§4.1) |
| port mapping PCP/PMP/UPnP | sì, sempre | no | no | sì, opt-in (§4.2) |
| IPv6 diretto | sì | — | sì | **no (Fase 4)** |
| relay di ultima istanza | DERP | manuale | circuit relay | relay TCP warm |
| promozione a caldo / retry | sì | no | singolo tentativo + retry | sì (VPN: griglia 30 s) |

---

## 4. Le lacune, in ordine di valore

### 4.1 Birthday paradox per la coppia `apdf × symmetric`

**Cos'è.** Quando un lato è EIM+APDF e l'altro è EDM, il lato EIM non sa a quale
porta scrivere. Tailscale apre ~256 porte sul lato *hard* (256 socket che
inviano) e fa provare al lato *easy* ~256 porte a caso: la probabilità di
collisione è

```
P = 1 − (1 − 256/65535)^256 ≈ 63 %
```

contro il ~0,006 % di un singolo probe. La stessa fonte è però esplicita sul
costo: per la coppia **hard × hard** servono ~170 000 probe per il 99,9 %
(28 minuti a 100 pacchetti/s), e i router d'ufficio hanno limiti bassi di
sessioni attive — «what if we have 20 machines doing this behind the same
router? Disaster».

**Cosa costerebbe in bore.** Non è un cambio di lista candidati: le porte
provate **non** viaggiano sul filo (il cap `MAX_UDP_CANDIDATES = 16` e il
worst case di `MAX_FRAME_LENGTH` restano intatti). È un cambio del **loop di
punch**: N socket locali sul lato symmetric, ciascuno con il proprio attore
(il modello a proprietario unico di `UdpTraversalSocket` è un socket = un
lettore), e una regola per decidere quale socket vincente diventa l'endpoint
QUIC. È l'unico intervento di questa lista che tocca il modello di proprietà
dei socket.

**Raccomandazione.** Vale la pena farlo, **limitato alla coppia easy×hard**
(63 % con 256+256 probe in pochi secondi), mai per hard×hard (che resta relay:
28 minuti non sono un percorso diretto, sono un port scan). Prerequisito:
la Fase 6 appena chiusa, che è ciò che permette di sapere *quando* si è in
quella cella invece di provarci sempre.

**FATTO (Fase 7).** Implementato esattamente con quel limite. Dettaglio
completo in [`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md) §20; qui i punti che
cambiano rispetto al preventivo qui sopra:

* i default spediti sono `3 × 256` porte contro 256 socket, cioè **≈ 95 %** e
  non 63 %: i passaggi usano insiemi di porte **diversi** e il filtro del lato
  easy resta aperto per tutte quelle già spruzzate, quindi l'unione conta e
  costa solo tempo, non un secondo meccanismo;
* il modello di proprietà dei socket **non** è cambiato come temuto: i socket
  ausiliari non usano `UdpTraversalSocket`, ognuno è letto da un task solo, e
  solo il vincitore — restituito per valore, con il reader già chiuso — arriva
  a Quinn. Un escape fallito restituisce il socket originale intatto;
* il preventivo non aveva previsto il problema vero, che il banco ha trovato
  subito: con un'estrazione generosa le collisioni sono **più di una** e i due
  lati ne sceglievano due diverse. Risolto con un conferma a transaction id
  già visto (§20.5), che non aggiunge niente al filo;
* il numero di socket è limitato a metà dei descrittori rimasti, perché
  `EMFILE` cade sull'`accept()` di ogni listener del processo (P-12);
* «20 macchine dietro lo stesso router» resta un rischio reale e la risposta è
  che l'escape gira **solo** dopo un round a vuoto, **solo** sulla cella che il
  broker ha già dichiarato relay-first, e **solo** se entrambi i peer
  annunciano `spray-v1`. Su una rete sana non parte mai.

Misura di campo: `escape_ms=51` sul banco netns, con la cella OFF che nello
stesso run resta su relay.

### 4.2 Port mapping automatico quando il piano dice che siamo noi il collo

Oggi `--upnp` è opt-in; Tailscale prova sempre. La matrice mostra che una
mappatura PCP sul lato EIM+APDF trasforma il suo filtro in un forward statico e
ribalta la cella da RELAY a DIRECT. **Non** è stato reso automatico in questo
passaggio: aprire una porta sul router dell'utente senza che l'abbia chiesto è
un effetto sulla sua rete, non solo sul suo processo. È stata invece resa
**azionabile** l'informazione: il piano porta ora il `reason_code` sul filo e il
client stampa il rimedio esatto (`plan_remedy`), per esempio

```
peer-port-restricted: the endpoint-independent side filters per address+port,
which a symmetric peer's unpredictable source port cannot pass. A port mapping
on THAT side (--upnp) or an operator-declared endpoint (--udp-candidate
HOST:PORT) turns its filter into a static forward and makes the pair punchable
```

Se in futuro si vuole il comportamento Tailscale, il posto giusto è un
`--upnp=auto` che si attiva **solo** su quel reason code, non un default nuovo.

### 4.3 Sincronizzazione simultanea in stile DCUtR

DCUtR misura l'RTT del relay con `Connect`/`Connect` e poi fa partire il punch
a **metà RTT** dal `Sync`, così che i due lati aprano davvero nello stesso
istante. bore oggi fa partire i check quando arriva `UdpPunch` dal broker, con
un jitter derivato dal ruolo: i due lati non sono sincronizzati sull'RTT, sono
solo *sfasati apposta* per non sbattere l'uno contro l'altro nel conntrack.

Per un filtro APDF l'ordine conta: il pacchetto in uscita deve precedere quello
in ingresso, altrimenti quello in ingresso viene scartato e non apre nulla. Il
retry di bore copre già questo caso (il secondo passaggio ha quasi sempre il
buco aperto), quindi il guadagno atteso è **latenza di primo aggancio**, non
tasso di successo. Costo basso (il broker già conosce i due RTT di controllo),
valore medio: da fare dopo il 4.1.

### 4.4 IPv6 (Fase 4, esplicitamente esclusa finora)

È la lacuna con il rapporto valore/rischio migliore in assoluto e insieme la
più grande: su IPv6 non c'è NAT, quindi la cella difficile non esiste. Tailscale
lo sfrutta per primo quando disponibile. In bore il traversal è interamente
`SocketAddr`-generico ma il gather, i candidati tipizzati e il probe STUN
assumono IPv4 in più punti; va pianificato come fase a sé.

### 4.5 Cose che NON conviene copiare

* **Una mesh di relay geo-distribuita (DERP).** bore ha un server, che è il suo
  modello di deployment. La latenza del relay è un dato del deployment, non un
  difetto del traversal.
* **Brute force hard × hard.** Vedi 4.1: 28 minuti e un comportamento
  indistinguibile da un port scan.
* **Rinunciare al filtering perché "tanto il simultaneous open passa".** È vero
  per EIM×EIM ed è falso per la cella che ci interessa, come mostra la tabella
  del §2 misurata su kernel reale.

---

## 5. Stato

| voce | stato |
|---|---|
| misura del filtering (RFC 5780) sui due lati | **fatto** (Fase 6) |
| regola di policy corretta per una sola parte symmetric | **fatto** (`peer-port-restricted` / `symmetric-vs-open-filter`) |
| `reason_code` sul filo + rimedio azionabile sul client | **fatto** |
| matrice A×B verificata su kernel reale | **fatto** (`scripts/udp_nat_netns_test.sh`, 27/0) |
| birthday paradox easy×hard | **fatto** (Fase 7, §4.1 + `NAT_TRAVERSAL.md` §20) |
| seconda osservazione di mapping via OTHER-ADDRESS | **fatto** (`NAT_TRAVERSAL.md` §20.7) |
| `--upnp=auto` sul solo reason code | **da valutare** (§4.2) |
| sincronizzazione RTT/2 | **da fare dopo 4.1** (§4.3) |
| IPv6 | **fase a sé** (§4.4) |
