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

## Implementato: Port-release detection (porta preferita)

### Observer effect

Un bind socket sulla porta preferita (`--nat-udp-preferred-port`) e la
conseguente sonda STUN **rinnovano il timeout NAT** per quella porta,
impedendone il rilascio. Misurato: ~620s di attesa se la porta non viene
toccata, vs MAI rilasciata se viene toccata ogni 15-30s.

```
Scenario:
  B bind 0.0.0.0:3478 → STUN probe → NAT rinnova timeout :3478 → ❌ mai rilasciata
  B bind porta ephemeral → STUN probe → NAT non vede :3478 → :3478 rilasciata dopo ~10 min
```

### Soluzione implementata

Quando il NAT rimappa la porta preferita (reflexive ≠ preferred), il peer
passa automaticamente a **porte ephemeral** per tutti i successivi tentativi
STUN/offerta, in modo che la NAT entry per la porta preferita scada
naturalmente. Ogni `--nat-udp-release-timeout` secondi (default 600 = 10 min),
riprova la porta preferita con una sonda STUN leggera (`check_reflexive_port`).

Se la porta è tornata PRESERVED, il backoff di upgrade viene resettato e il
prossimo tentativo userà la porta preferita. Se ancora REMAPPED, continua con
porte ephemeral.

### Nuovo flag

| Flag | Default | Env | Dove |
|------|---------|-----|------|
| `--nat-udp-release-timeout SECS` | 600 | `BORE_NAT_UDP_RELEASE_TIMEOUT` | `local` + `proxy` |

### Dettaglio implementazione

**Consumer side (`secret.rs` - Proxy::listen):**
- `preferred_port_remapped: bool` flag nello stato del loop
- Calcolo `effective_udp_port` a ogni upgrade attempt: se remappato → 0
- Rilevamento remap quando `offer.candidates.first().port() != udp_port`
- Timer `release_check` (interval = `nat_udp_release_timeout`) con sonda
  `check_reflexive_port()`
- Quando preserved: reset backoff, `last_upgrade` forzato nel passato →
  upgrade immediato

**Provider side (`client.rs` - Client::listen):**
- Stessa logica nel `udp_retry.tick()` (timer 15s di re-offer)
- `resolve_stun_and_check()` helper function: risolve STUN, bind, probe, update flag
- Quando preserved: disabilita flag → prossimo re-offer usa porta preferita

**Nuova funzione `holepunch::check_reflexive_port(port, stun_addr) -> Option<bool>`:**
- Bind porta, unica sonda STUN, return `Some(true)` se preserved,
  `Some(false)` se remapped, `None` se STUN unreachable

**Modifiche protocollo:** 0 (nessun nuovo messaggio server/client)

### Log

```
INFO  port :3478 was REMAPPED to :1026 by NAT; switching to ephemeral probes.
      Will re-check in 600s
WARN  port :3478 is now PRESERVED on NAT! Scheduling immediate direct path upgrade
INFO  port :3478 still REMAPPED on NAT; will re-check in 600s
```

### Limiti

- Il primo tentativo usa sempre la porta preferita (potrebbe rinnovare il NAT
  una volta prima di rilevare il remap)
- `nat_udp_release_timeout=0` disabilita la funzionalità (default test)
- Non risolve il caso di NAT simmetrico (la porta cambia comunque)
- Provider side: dopo la rilevazione preserved, il prossimo re-offer (max 15s)
  deve propagarsi al consumer via upgrade retry (backoff 2→256s)
