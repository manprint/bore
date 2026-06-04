# Analisi limitazione firewall: Direct UDP hole-punching

## Scenario

Due consumer domestici (A, B) dietro lo stesso NAT/router e un provider
aziendale (C) dietro un firewall che filtra il traffico UDP in uscita.

```
┌─────────────────────────────────────────────────────────┐
│                    CASA (82.54.81.19)                     │
│                                                          │
│  ┌──────────┐                         ┌──────────┐      │
│  │   A      │                         │   B      │      │
│  │192.168.1.2│   ┌─────────────────┐  │192.168.1.11│     │
│  │:3478     │   │   Router NAT    │  │:3478     │      │
│  └────┬─────┘   │                 │  └────┬─────┘      │
│       │         │ Tabella NAT:    │       │            │
│       ├────────►│ :3478 → A       │◄──────┤            │
│       │         │ :1026 → B ◄─────│───────┘            │
│       │         │                 │                    │
│       │         └────────┬────────┘                    │
│       │                  │                             │
│       │                  │ 82.54.81.19:3478 o :1026    │
└───────┼──────────────────┼─────────────────────────────┘
        │                  │
        │    INTERNET      │
        │                  │
┌───────┼──────────────────┼─────────────────────────────┐
│       ▼                  ▼                             │
│  ┌──────────────────────────────────────────┐          │
│  │         AZIENDA C (91.81.116.61)          │          │
│  │                                           │          │
│  │  ┌──────────────────────────────────┐     │          │
│  │  │   Firewall stateful              │     │          │
│  │  │                                  │     │          │
│  │  │  Regola UDP outbound:            │     │          │
│  │  │    DST_PORT == 3478 → ✅ allow   │     │          │
│  │  │    DST_PORT != 3478 → ❌ deny    │     │          │
│  │  │                                  │     │          │
│  │  │  Risposta a :3478 → passa        │     │          │
│  │  │  Risposta a :1026 → bloccata     │     │          │
│  │  └──────────────────────────────────┘     │          │
│  │         │                                 │          │
│  │  ┌──────▼──────┐                          │          │
│  │  │  Provider C  │                          │          │
│  │  │  :3478 → :60365 (NAT aziendale)        │          │
│  │  └─────────────┘                          │          │
│  └───────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────────┘
```

## Topologia di rete

| Elemento | IP pubblico | Porta locale | Porta riflessiva (STUN) |
|----------|-------------|-------------|------------------------|
| A (casa) | 82.54.81.19 | 3478 | 3478 |
| B (casa) | 82.54.81.19 | 3478 | 1026 |
| C (azienda) | 91.81.116.61 | casuale | 60365 |

## Flusso UDP direct path

### Fase 1: Negoziazione candidati

```
Consumer                Server                  Provider
   │                       │                       │
   ├── StunHintRequest ───►│                       │
   │                       ├── StunHint(provider)─►│
   │                       │ (stun.cloudflare.com) │
   │                       │                       │
   ├── bind-socket(3478)   │                       │
   ├── STUN chain          │                       │
   │  stun.cloudflare:3478 │                       │
   ├── discover_reflexive  │                       │
   │  → 82.54.81.19:XXXX   │                       │
   ├── UdpCandidateOffer──►│                       │
   │  [reflexive, local]   │                       │
   │                       ├── UdpPunch(nonce,     │
   │                       │     consumer_cands)──►│
   │◄──── UdpPunch(nonce,  │                       │
   │       provider_cands)─┤                       │
   │                       │                       │
```

### Fase 2: Hole-punching e QUIC handshake

```
Consumer                NAT casa              Firewall C           Provider C

   │ punch(provider)      │                       │                       │
   ├─ UDP :60365 ────────►│ :3478→C:60365 ───────►│ controlla DST_PORT    │
   │                      │                       │ =60365 ≠ 3478 → ❌   │
   │                      │                       │ (SOLO per C→consumer) │
   │                      │                       │                       │
   │                      │                       │  ...MA il pacchetto   │
   │                      │                       │  è dal consumer a C,  │
   │                      │                       │  non da C al consumer │
   │                      │                       │                       │
   │                      │                       │  Il firewall stateful │
   │  pacchetto arriva ───┼───────────────────────►├─ ✅ allow (è inbound) │
   │                      │                       │                       │
   │                      │                       ▼                       │
   │                      │                       ├─ Provider riceve punch│
   │                      │                       │                       │
   │                      │                       ├─ QUIC handshake ────► │
   │                      │◄── C:60365 → ─────────┤  response             │
   │                      │     consumer:XXXX     │                       │
   │                      │                       │                       │
   │                      │  CONTROLLA DST_PORT:  │                       │
   │                      │  ┌─────────────────┐  │                       │
   │                      │  │ A:3478 → ✅     │  │                       │
   │                      │  │ B:1026 → ❌     │  │                       │
   │                      │  └─────────────────┘  │                       │
   │                      │                       │                       │
```

## Perché A funziona, B no

### Port preservation sul NAT domestico

Entrambi A e B usano `--nat-udp-preferred-port 3478`. La socket locale è
`0.0.0.0:3478` per entrambi.

Il router NAT di casa può assegnare **una sola** porta pubblica `3478` alla volta:

1. **A** apre `0.0.0.0:3478` → NAT assegna `82.54.81.19:3478` **(preservata)**
2. **A** viene spento. Il mapping `:3478 → A` resta nella NAT table del router
   per ~60 secondi (TIME_WAIT / UDP timer).
3. **B** apre `0.0.0.0:3478` entro quella finestra → NAT assegna
   `82.54.81.19:1026` **(non preservata)`, perché :3478` è ancora occupato.

```
Risultato STUN di A:  reflexive = 82.54.81.19:3478
Risultato STUN di B:  reflexive = 82.54.81.19:1026
```

### Firewall aziendale stateful

Il firewall di C permette solo traffico UDP outbound verso **DST_PORT = 3478**
(porta di default STUN Cloudflare, usata come regola whitelist per applicazioni
specifiche).

Quando il provider C risponde al QUIC handshake, invia pacchetti verso la
porta riflessiva del consumer:

| Consumer | Provider → Consumer | DST_PORT | Firewall |
|----------|-------------------|----------|----------|
| A | `C:60365 → 82.54.81.19:3478` | 3478 | ✅ Passa |
| B | `C:60365 → 82.54.81.19:1026` | 1026 | ❌ Bloccato |

Il firewall di C vede il pacchetto uscente da C verso internet, controlla
DST_PORT, e blocca quello verso B.

### Perché il relay TCP funziona sempre

Il relay TCP usa la control connection sulla porta `7835` (o `443` per
`https://`), che è già stata negoziata all'avvio del tunnel. Il firewall
di C non blocca queste porte perché sono esplicitamente aperte per il
servizio bore.

## Miglioramenti implementati

### 1. Backoff esponenziale per UDP upgrade retry (secret.rs)

**Prima**: retry fisso ogni 10 secondi (`UDP_UPGRADE_INTERVAL`).

**Dopo**: backoff esponenziale 2, 4, 8, 16, 32, 64, 128, 256 secondi, poi
ogni 256 secondi (~4.3 min). Il backoff è implementato riutilizzando
`reconnect::Backoff` parametrizzato (`new_with(2, 256)`).

```
Proxy::listen loop:
  ├─ peeks upgrade_backoff.peek() → delay corrente
  ├─ elapsed >= delay? → start upgrade attempt
  │    ├─ upgrade_backoff.next_delay() → avanza per prossimo retry
  │    ├─ upgrade_attempt += 1
  │    └─ log: "attempt #3; next retry in 16s"
  │
  ├─ upgrade successo:
  │    ├─ upgrade_backoff.reset() → torna a 2s
  │    └─ upgrade_attempt = 0
  │
  └─ upgrade fallito:
       ├─ gather fail:  "candidate gathering failed; will retry in Xs"
       └─ punch fail:   "upgrade attempt failed; will retry in Xs"
```

**Log introdotti**:
```
INFO  starting udp upgrade attempt #3; will retry in 16s on failure
INFO  udp upgrade candidate gathering failed; will retry in 32s
INFO  udp upgrade attempt failed; will retry in 64s
```

### 2. Per-candidate error collection in connect_direct (holepunch.rs)

**Prima**: il warn finale diceva solo "all direct candidates failed" senza
dettaglio per candidato. Le info erano a livello `debug!` e non aggregate.

**Dopo**: ogni candidato colleziona l'errore in `Arc<Mutex<Vec<(SocketAddr,
String)>>>`. Il warn finale include il dettaglio strutturato.

```
WARN  all 2 direct QUIC candidates failed; falling back to relay
      errors=["91.81.116.61:60365 → TimedOut",
              "10.10.16.138:60365 → TimedOut"]
```

### 3. Timeout case distinto da candidate failure

**Prima**: `candidate_errors=[]` nel caso timeout (traeva in inganno —
sembrava un bug di collezione).

**Dopo**: il ramo timeout non tenta di leggere errors (sempre vuoto perché i
future sono cancellati da `select_ok` prima di completare). Logga invece:

```
WARN  direct QUIC connect exhausted 3s budget across 2 candidates;
      none responded — all candidates timed out (firewall/UDP blocked
      on both ends, or peer IP unreachable). Falling back to relay
```

### 4. Parametrizzazione di reconnect::Backoff (reconnect.rs)

Aggiunti `initial_secs`, `max_secs` come campi. Nuovo costruttore
`new_with(initial, max)` e metodo `peek()` per ispezionare il delay senza
avanzare. `reset()` ora usa `self.initial_secs` invece di costante hardcoded.

## Cosa NON risolve il backoff (strutturale vs transiente)

Il backoff esponenziale serve per fallimenti **transienti**:
- Provider riavviato → retry rapido (2s) lo rileva
- Congestione di rete → backoff permette di recuperare
- STUN server momentaneamente giù

Non risolve fallimenti **strutturali**:
- Firewall che blocca la porta di destinazione
- NAT simmetrico incompatibile
- Conflitto di porta tra consumer co-locati

Nel caso B, il mapping `:1026` è stabile per tutta la vita del socket.
Ritentare dopo 256s è identico a ritentare dopo 2s: il socket è sempre su
`:1026`, il firewall blocca sempre.

---

## TODO: Re-bind del socket UDP su fallimento strutturale

### Idea

Quando connect_direct fallisce con timeout su tutti i candidati, il consumer
potrebbe chiudere il socket UDP corrente e riaprirne uno su una porta
diversa, nella speranza di ottenere una porta riflessiva che passi il
firewall.

```
Scenario attuale:
  B apre 0.0.0.0:3478 → NAT assegna 82.54.81.19:1026 → ❌ firewall blocca

Con re-bind:
  B apre 0.0.0.0:3478 → NAT assegna 82.54.81.19:1026 → ❌
  B chiude socket
  B apre 0.0.0.0:0 (ephemeral) → NAT assegna 82.54.81.19:XXXXX → ?
  B apre 0.0.0.0:6000  → NAT assegna 82.54.81.19:6000 → ?
```

### Varianti

| Variante | Meccanismo | Complessità | Efficacia |
|----------|-----------|-------------|-----------|
| **Full-bind** | Chiudi socket, ri-binda su porta random | Media | Alta — cambia porta riflessiva |
| **Port-scan** | Prova N porte fisse in sequenza | Alta | Alta — ma sembra un port scan |
| **Fallback-no-udp** | Disabilita `--udp`, solo relay | Bassa | Bassa — perde direct path |
| **Bind-dopo-timeout** | Riapri con porta diversa e rioffri | Alta | Media — richiede rioffrire candidati |

### Valutazione applicabilità

**Stato attuale del flusso:**

```
connect_direct() fallisce (timeout su tutti i candidati)
  → upgrade_task termina (done_tx droppato)
  → Proxy::listen vede nego_done_rx = None
  → log: "udp upgrade attempt failed; will retry in Xs"
  → dopo Xs: riprova da capo (stun hint → gather → offer → punch)
```

Il re-bind si innesterebbe a livello `gather_consumer_candidates()`: quando
la discovery fallisce (o il passo successivo fallisce), si può chiudere il
socket e rioffrire.

**Problemi e rischi:**

1. **Il socket UDP è già stato offerto al server.** Se lo chiudiamo e
   riapriamo, dobbiamo rioffrire i candidati — ma l'offerta è già stata
   inviata e il server ha già risposto con `UdpPunch`. Il server ha uno
   stato (nonce, peer candidates). Un cambio a metà negoziazione richiederebbe
   una nuova negoziazione completa — cosa che l'upgrade retry già fa.

2. **Il binding del socket è in `Proxy::new` / `Proxy::listen`.** Nel flusso
   attuale, il socket è creato da `gather_consumer_candidates()` e
   consumato da `finish_direct_consumer()`. Se `finish_direct_consumer`
   fallisce, il socket viene droppato con lo scope. Il retry successivo
   creerà un nuovo socket. Questo **già accade** — è gratis!

3. **Solo l'upgrade task soffre del problema.** All'avvio (`Proxy::new`),
   il socket è creato una volta sola. Se il primo tentativo fallisce, si va
   su relay e l'upgrade task periodico è l'unico meccanismo di retry.
   **Bindare una porta diversa a ogni retry** dell'upgrade è banale:
   basta usare `udp_port = 0` invece di `udp_port` nel retry, cambiando
   così la porta riflessiva a ogni ciclo.

4. **Regressioni potenziali:**
   - Con `--nat-udp-preferred-port N`, l'utente ha scelto ESATTAMENTE quella
     porta (es. per aprire un firewall). Cambiare porta al retry vanifica
     la scelta dell'utente.
   - La porta riflessiva assegnata dal NAT è imprevedibile — potremmo
     finire su una porta che SEMBRA funzionare (non bloccata dal firewall)
     ma è in conflitto con altre applicazioni.
   - Il NAT simmetrico assegna porta diversa per OGNI destinazione,
     quindi re-bindare non cambia nulla se il problema è simmetrico.

### Proposta di implementazione (basso rischio)

Aggiungere un flag `upgrade_use_ephemeral_port` in `Proxy`: dopo N fallimenti
consecutivi (es. 3) dell'upgrade task, passare `udp_port = 0` invece di
`udp_port` in `gather_consumer_candidates()`. Così:

- Primi 3 tentativi: usano la porta preferita dall'utente (3478)
- Tentativi successivi: usano porta ephemeral (0), sperando in una porta
  riflessiva diversa

Il server accetta qualsiasi nuova offerta di candidati — non c'è invalidazione
dello stato server. Basta inviare `UdpCandidateOffer` di nuovo.

**Dettaglio implementazione:**

```rust
// In Proxy::listen, nel ramo upgrade:
#[cfg(feature = "udp")]
let upgrade_use_ephemeral = upgrade_attempt > 3;

// spawn_upgrade_attempt prende udp_port
let effective_udp_port = if upgrade_use_ephemeral { 0 } else { udp_port };
```

Il socket viene chiuso e riaperto automaticamente perché
`gather_consumer_candidates()` chiama `bind_socket(effective_udp_port)`
creando un nuovo socket.

**Vantaggi:**
- 0 modifiche al server
- 0 modifiche al protocollo
- Nessun messaggio aggiuntivo sul control channel (si rioffrono candidati)
- Compatibile con `--auto-reconnect` (lo stato è nel Proxy, non globale)
- Non rompe la porta preferita dell'utente (primi 3 tentativi la usano)

**Svantaggi:**
- Porta ephemeral può NON avere port preservation (sempre 1024-65535 random)
- Aggiunge latenza (ogni retry = STUN + offer + punch)
- Il log deve chiarire perché si sta cambiando porta

### Conclusione

Il re-bind su porta diversa è **fattibile** e **a basso rischio di
regressione**. La complessità è bassa. L'efficacia dipende dallo scenario:

| Scenario | Re-bind aiuta? |
|----------|---------------|
| Conflitto porta tra A e B sullo stesso NAT | ✅ Sì — porta diversa risolve |
| Firewall blocca una specifica DST_PORT range | ✅ Forse — se la nuova porta è permessa |
| NAT simmetrico | ❌ No — porta cambia comunque per destinazione |
| Firewall blocca tutto UDP fuori dalle porte note | ❌ No — non c'è porta che passi |

Implementazione rimandata per mancanza di priorità. Il workaround attuale
(relay TCP + `--carriers`) è funzionante e stabile.
