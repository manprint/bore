# Analisi di Fattibilità: `bore transfer`

> Stato aggiornato: `bore transfer` e ora implementato nel repo. Oggi il comando
> copre file, directory e `stdin`, tenta UDP diretto con fallback relay, usa il
> protocollo V2 per i transfer filesystem (manifest, chunking, resume,
> `--parallel`, staging, verifica BLAKE3), espone `--overwrite` / `--rename`,
> `--symlinks`, `--devices`, i flag NAT/UPnP e ha suite e2e dedicate. Questo
> documento resta come analisi/progetto storico; per il comportamento corrente
> vedi `README.md`, `USER_GUIDE.md`, `CHANGELOG.md` e `CLAUDE.md`.

## 1. Overview

Nuovo sottocomando `bore transfer` per trasferire file/cartelle/streaming tra due
macchine (A→B) usando l'infrastruttura bore esistente: UDP hole-punching + QUIC
direct path, TCP relay fallback con carrier pool, canale di controllo
autenticato.

### 1.1 Comandi proposta

```shell
# B (riceve)
bore transfer listener --dest-path /home/ubuntu --secret miao \
  --to bore.example.com

# A (invia)
bore transfer sender --source /home/ubuntu/miofile.tar.gz \
  --to bore.example.com --secret miao

# Stdin
tar -cvpzf - myfolder | bore transfer sender --source stdin \
  --to bore.example.com --secret miao --output nomeoutput.tar.gz
```

### 1.2 Schema architetturale

```
SENDER (A)                    SERVER (relay)              LISTENER (B)
   |                             |                            |
   |--- control channel -------->|---> registra listener -----|
   |    (yamux, auth)            |    (TransferListen id)     |
   |                             |                            |
   |--- control channel -------->|---> registra sender -------|
   |    (yamux, auth)            |    (TransferSend id)       |
   |                             |                            |
   |                             |  matcha sender+listener    |
   |                             |  stesso id                 |
   |                             |                            |
   |  == NEGOZIAZIONE UDP ==    |                            |
   |  STUN -> candidates ->      |   brokering candidates     |
   |  server matcha             |                            |
   |                             |                            |
   |  === SE UDP OK ===         |                            |
   |  QUIC direct conn           |  (fuori dal server)       |
   |  A ──────────────────────── B                            |
   |  (native QUIC streams)     |                            |
   |                             |                            |
   |  === SE UDP FALLISCE ===   |                            |
   |  TCP relay via carriers    |   puro relay (no storage)  |
   |  A ───────yamux relay───── B                            |
   |                             |                            |
```

## 2. Modello di Comunicazione

### 2.1 Rendezvous (reuse `HelloSecret`/`ConnectSecret`)

Il modello segreto esistente è perfetto: due peer si registrano con lo stesso
`id` (autogenerato o fornito dall'utente via `--transfer-id`). Il server li
matcha come già fa `serve_provider`/`serve_consumer` in `secret.rs`.

**Novità**: invece di relay continui (molte connessioni che arrivano e vengono
splice), il transfer ha un ciclo di vita definito:
1. Listener si registra → attesa sender
2. Sender si registra → server matcha
3. Negoziato UDP (o fallback TCP)
4. Trasferimento file → progress → verifica integrità
5. Disconnessione pulita

### 2.2 UDP Direct Path (reuse holepunch + QUIC)

L'infrastruttura `test-udp` esegue già esattamente ciò che serve:

- `udp_diagnostic.rs`: due peer con stesso id, STUN candidate gathering,
  brokering server-side, QUIC direct connect, TCP fallback.
- `holepunch.rs`: `DirectConn`, `DirectListener`, `QuicTransport`,
  `connect_direct`, `derive_token`, `stun::discover_reflexive`.
- `secret.rs`: `broker_udp`, `negotiate_direct_consumer`, `upgrade_task`.
- Trasferimento files multipli via native QUIC streams (una per file? una per
  chunk?), già supportato da `DirectConn::open_stream`/`accept_stream`.

Il percorso UDP va tentato per primo, con fallback immediato al relay TCP.
L'adaptive NAT planning esiste già.

### 2.3 TCP Relay Fallback (reuse carrier pool)

- Il server non deve bufferizzare: `copy_bidirectional_with_sizes` con
  `PROXY_BUFFER_SIZE` (64 KiB) fa da pipe.
- Carrier pool (`--carriers N`) per evitare HOL su una singola TCP.
- Il server è trasparente: non sa che sono file, copia byte.

## 3. Nuovo Protocollo (control messages)

### 3.1 Nuovi messaggi in `shared.rs`

I tipi e messaggi definitivi sono elencati in dettaglio nella sezione A2
(§9). Qui il riepilogo:

**Nuovi tipi**: `TransferId(String)`, `DigestAlgorithm { Sha256 | Blake3 }`,
`TransferSourceKind { File | Directory | Stdin }`.

**Aggiungere a `ClientMessage`**:
```rust
TransferListen { id: TransferId, dest_path: String, overwrite: bool,
    rename: bool, notes: Option<String> },
TransferSend { id: TransferId, source_kind: TransferSourceKind,
    source: Option<String>, notes: Option<String> },
TransferFileMeta { name: String, size: u64, offset: u64 },
TransferDigest { algorithm: DigestAlgorithm, hash: Vec<u8> },
TransferChunk { index: u64, size: u32, hash: [u8; 32] },
TransferChunkRetry { index: u64 },
```

**Aggiungere a `ServerMessage`**:
```rust
TransferReady { direct_available: bool, peer_candidates: Vec<SocketAddr>,
    nonce: [u8; UDP_NONCE_LEN], tuning: UdpDirectTuning, encrypted: bool,
    role: UdpTestRole },  // Listener o Dialer per QUIC
TransferWaiting,
TransferVerified { ok: bool, algorithm: DigestAlgorithm,
    expected_hash: Vec<u8>, actual_hash: Vec<u8>,
    error_message: Option<String> },
TransferError(String),
```

Vedi A2 (§9) per campi completi e attributi serde.

### 3.2 Flusso completo

```
LISTENER                          SERVER                        SENDER
   |                                |                              |
   |---TransferListen{id,path}----->|                              |
   |                                |---TransferWaiting----------->|
   |<---TransferWaiting------------|                              |
   |                                |   (aspetta sender)           |
   |                                |                              |
   |                                |<---TransferSend{id,src}------|
   |                                |                              |
   |                                |  matcha i due peer           |
   |                                |  manda a entrambi:           |
   |<---TransferReady{...}---------|                              |
   |                                |---TransferReady{...}-------->|
   |                                |                              |
   |  == NEGOZIAZIONE UDP ===       |                              |
   |  (stessa dinamica test-udp)   |                              |
   |                                |                              |
   |  == AVVIA TRANSFER ===========|                              |
   |                                |                              |
   |<===== TransferFileMeta ========|===== (o direct) =============|
   |<===== byte dei dati ==========|=====               ==========|
   |<===== TransferChunk  ==========|===== (se streaming)=========|
   |       ...                      |       ...                    |
   |<===== TransferDigest  =========|=======                       |
   |                                |                              |
   |---TransferVerified{ok}-------->|                              |
```

## 4. Modalità di Trasferimento

### 4.1 File Singolo

- `--source /path/file.ext`
- Sender apre file, legge `metadata` (size, name, permissions).
- Invia `TransferFileMeta { name: "file.ext", size: N, ... }`.
- Invia dati in blocchi (default 64 KiB, ma tuneable).
- Calcola hash incrementale (BLAKE3).
- Alla fine invia `TransferDigest`.
- Listener riceve, salva in `dest_path/file.ext`, calcola hash, verifica.
- Invia `TransferVerified`.

### 4.2 Directory Ricorsiva

- `--source /path/dir` → sender walk ricorsivo.
- Due strategie possibili:

**Opzione A: Multiplex per-file (raccomandata)**
- Ogni file apre una nuova substream (yamux o QUIC).
- Vantaggi:  parallelismo, isolamento errori, progress tracking preciso.
- Svantaggi:  overhead per tanti file piccoli.

**Opzione B: Tar-like stream unico**
- Sender serializza su un unico stream: header → file content → header → ...
- Vantaggi:  basso overhead, un solo stream.
- Svantaggi:  no parallelismo, errore corrompe tutto.

**Raccomandazione: Opzione A** per integrità incrementale e progress tracking.
Ma con un `--tar` mode opzionale per pipe shell.

Flusso Opzione A:
```
for each file in walk(dir):
  TransferFileMeta { name, size, relative_path, mode }
  bytes del file
  TransferDigest { algorithm, hash[:8] }   // chunk hash (primi 8 byte)
TransferDigestFinal { algorithm, root_hash }
```

### 4.3 Stdin

- `--source stdin` (rileva automaticamente se stdin non è un TTY).
- `--output FILENAME` **obbligatorio** (nessun auto-generato).
- Size sconosciuta (`0`), hash calcolato incrementalmente.
- `TransferFileMeta { name: <output>, size: 0, ... }`.
- Loop: leggi da stdin → scrivi su stream → aggiorna hash incrementale.
- Alla fine: `TransferDigest`.
- Listener salva con nome indicato.

**Sfida**: lo stdin può durare molto (tar di TB). Necessario:
- Buffer di scrittura adeguato.
- Timeout lato listener (se sender muore).
- Periodic chunk hashing per rilevare errori prima della fine.

## 5. Integrità del Trasferimento

### 5.1 Opzioni a confronto

| Algoritmo | Velocità HW | Velocità SW | Supporto Rust | Incrementale | Collisioni | Note |
|-----------|-------------|-------------|---------------|--------------|------------|------|
| CRC32     |  ~5 GB/s    |  ~1 GB/s    | `crc32fast`   | Sì           | Non sicuro | Solo rilevamento errori casuali |
| SHA-256   |  ~1 GB/s    |  ~300 MB/s  | `sha2`        | Sì           | Sicuro     | Standard, lento in SW puro |
| SHA-512   |  ~700 MB/s  |  ~400 MB/s  | `sha2`        | Sì           | Sicuro     | Meglio su 64-bit |
| BLAKE3    |  ~16 GB/s   |  ~1.5 GB/s  | `blake3`      | Sì (tree)    | Sicuro     | **Raccomandato** |
| XXH3      |  ~30 GB/s   |  ~15 GB/s   | `xxhash-rust` | Sì           | Non sicuro | Solo checksum, non crittografico |

**Analisi**:
- **CRC32**: troppo debole, collisioni facili. Solo per errori di trasmissione,
  non per integrità intenzionale.
- **SHA-256**: sicuro ma lento per file grandi in software puro. `ring` crate
  ha SHA-256 accelerato (~1 GB/s), già dipendenza di bore.
- **BLAKE3**: **scelta migliore** per questo caso d'uso. Supporta:
  - Hashing incrementale (streaming): `blake3::Hasher::update()` via via.
  - **Tree hashing**: dividi il file in chunk da 1 MiB, ogni chunk ha hash,
    root hash finale. Permette verifica incrementale *durante* il trasferimento.
  - Estremamente veloce (1.5+ GB/s software puro, SIMD automatico).
  - Già usato in molteplici tool Rust (cargo, etc.).

### 5.2 Strategia per streaming (tar/stdin)

Problema: non conosci la dimensione finale, non puoi pre-hashare.

**Soluzione proposta: BLAKE3 tree hashing + chunk verification**

1. Sender divide stream in chunk da 1 MiB (o configurabile).
2. Per ogni chunk inviato, sender calcola hash BLAKE3 del chunk.
3. Dopo ogni chunk (o periodicamente ogni N chunk), sender invia
   `TransferChunk { index: N, size: S, hash: [H] }`.
4. Listener verifica l'hash del chunk subito.
5. Alla fine dello stream, sender invia `TransferDigestFinal` con il root hash
   BLAKE3 (che è l'hash cumulativo dell'intero stream).

**Vantaggi**:
- Errore rilevato subito (entro 1 MiB), non alla fine del TB.
- Listener può scartare chunk corrotto e richiedere ritrasmissione.
- Root hash finale garantisce integrità complessiva.

**Alternativa più semplice (consigliata per Fase A)**: hash cumulativo SHA-256
o BLAKE3, verificato solo alla fine. Per file normali si conosce già l'hash
prima di iniziare (sender ha tutto il file), quindi si può verificare a
destinazione. Per streaming, hash cumulativo fine-stream.

### 5.3 Raccomandazione finale

- **File e directory**: BLAKE3 pre-transfer (sender hasha prima di inviare).
  Listener verifica post-transfer. Se mismatch → `TransferVerified{ok: false}`.
  Ritrasmissione automatica con chunk differenziale (fase C).
- **Stdin/streaming**: BLAKE3 incrementale. Chunk hash ogni 1 MiB per
  rilevamento precoce. Root hash alla fine.
- **Non usare CRC32**: non dà garanzie cryptographiche.
- **Keep dependency light**: BLAKE3 è auditato, no-unsafe, single dependency.

## 6. Progress Display

### 6.1 Requisiti

- Velocità istantanea (MB/s) e media.
- Percentuale completata (per file) / totale (per directory).
- File N di M completati (per directory).
- ETA stimato.
- Per stdin: solo byte count + velocità (no % né ETA senza size).

### 6.2 Implementazione

```rust
struct TransferProgress {
    bytes_sent: u64,
    bytes_total: u64,         // 0 = sconosciuto
    files_completed: u32,
    files_total: u32,         // 0 = sconosciuto (stdin)
    speed_bytes_ps: f64,      // calcolato sliding window
    current_file: String,
    elapsed: Duration,
}
```

**Opzioni rendering**:
1. `indicatif` crate (progress bar + spinner) — già usato in molti CLI Rust.
2. Custom `\r`-based per zero dipendenze.

**Raccomandazione**: `indicatif` (leggero, ben mantenuto, multi-progress bar).

```rust
// style proposto
[=>------------------]  23%  45.2 MB/s  ETA 12s  file.tar.gz  (3/5 file)
```

### 6.3 Frequenza aggiornamento

- Ogni 100ms (non più frequente per evitare overhead terminale).
- Via canale di controllo dediato o piggyback sui dati.

## 7. Integrazione con Infrastruttura Esistente

### 7.1 Cosa si riusa

| Componente | Uso in transfer | Modifiche |
|------------|-----------------|-----------|
| `shared.rs` | `Delimited<T>`, `NETWORK_TIMEOUT`, `MAX_FRAME_LENGTH` | Nuovi messaggi ClientMessage/ServerMessage |
| `mux.rs` | Yamux substream per control + TCP relay data | Nessuna |
| `secret.rs` | `serve_provider`/`serve_consumer` pattern per rondezvous | Ispirazione (reimplementato in transfer.rs) |
| `pool.rs` | Carrier pool per TCP relay | Nessuna |
| `holepunch.rs` | `bind_socket`, `gather_candidates`, `DirectConn`, `QuicTransport` | Nessuna (forse tuning windows) |
| `udp_diagnostic.rs` | Paired peer negotiation, STUN chain, candidate ordering | Ispirazione (reimplementato in transfer.rs) |
| `auth.rs` | `Authenticator` per `--secret` | Nessuna |
| `reconnect.rs` | `Backoff`, `run` per auto-reconnect | Nessuna (transfer non usa reconnect) |
| `transport.rs` | `Endpoint::parse`, `connect` | Nessuna |
| `server.rs` | `Server::route_connection` per disambiguare nuovo comando | Nuovo ramo in `dispatch` |
| `client.rs` | Provider pattern per sender | Ispirazione (reimplementato in transfer.rs) |
| `main.rs` | CLI parsing | Nuovo sottocomando `Transfer` |

### 7.2 Cosa si crea

| File | Scopo |
|------|-------|
| `src/transfer.rs` | Logica core di transfer: sender, listener, progress, integrity |

### 7.3 Cosa NON si tocca

- Tutto il codice esistente tunnel/public/secret rimane invariato.
- `--carriers`, `--udp`, `--secret` sono già funzionanti, si riusano.
- Il server esistente non serve transfer-specific logic (salvo matchmaking).

## 8. Domande Aperte / Da Decidere

### Q1: ID transfer — autogenerato o utente?
**Decisione**: se omesso, listener genera UUID e stampa `Transfer ID: <uuid>`.
Sender lo passa con `--transfer-id <ID>`. Se `--transfer-id` passato a
entrambi, usare quello.
- Listener: `--transfer-id <ID>` (opzionale) o autogenerato.
- Sender: `--transfer-id <ID>` obbligatorio (deve matchare listener).

### Q2: Sovrascrittura file esistenti?
**Decisione**: default = **fallimento con errore chiaro**. Log tipo:
```
ERROR: dest-path/output.tar.gz already exists.
Use --overwrite to replace or --rename to auto-rename (output.1.tar.gz).
```
Flag:
- `--overwrite`: sovrascrive senza chiedere.
- `--rename`: auto-renaming con incremento numerico (`file.1.ext`, `file.2.ext`).
- Nessun flag: errore, transfer abortito.

### Q3: Nome file per stdin?
**Decisione**: `--output FILENAME` obbligatorio per stdin. Se omesso → errore
immediato:
```
ERROR: --output is required when --source is stdin.
```
Nessun auto-generato.

### Q4: Compressione lato sender?
`bore transfer sender --compress xz ...` → comprime prima di inviare.
O è meglio lasciare all'utente (tar | xz | bore)?
**Decisione**: l'utente fa pipe esternamente (`tar -I 'xz -9e' -cvpf - | bore ...`).
Bore non si occupa di compress/decompress.

### Q5: Auto-reconnect per transfer?
**Decisione**: nessun resume. Se transfer fallisce (disconnessione, errore),
file parziale cancellato, errore chiaro:
```
ERROR: Transfer failed at 53.4% (1.2 GiB / 2.3 GiB).
Partial file removed: /dest/path/file.tmp
Reason: connection lost to server.
```
Log include bytes trasferiti, file, motivo fallimento.
Resume rimandato a sviluppo futuro — dettagli in sezione 12.

### Q6: Sicurezza — canale crittografato?
**Decisione**: trasparente all'utente — dipende da `--to`.
- `--to bore.example.com` → plain TCP relay.
- `--to https://bore.example.com` → TLS cifrato relay.
- UDP direct path sempre cifrato (QUIC TLS + token auth, indipendentemente
  da `--to`).
Log informativo all'avvio:
```
INFO: transfer path: direct UDP (QUIC, encrypted)
INFO: relay fallback: TCP plain (no encryption)
```
Oppure:
```
INFO: transfer path: direct UDP (QUIC, encrypted)
INFO: relay fallback: TCP over TLS (encrypted)
```
Così utente sa sempre se fallback è cifrato o meno.

### Q7: Directory — preservare struttura?
```shell
--source /home/ubuntu/myfolder/
  ├── docs/
  │   ├── readme.txt
  │   └── manual.pdf
  └── src/
      └── main.rs
```
Listener riceve:
```
dest-path/
  └── myfolder/
      ├── docs/
      │   ├── readme.txt
      │   └── manual.pdf
      └── src/
          └── main.rs
```
**Decisione**: struttura preservata. Root della directory sorgente creata in
`dest-path/` con stesso nome base.

### Q8: File speciali (symlink, device)?
Seguire symlink? Copiare il link come link?
**Decisione**: skip symlink e file speciali (device, fifo, socket) in Fase A.
Opzione `--follow-links` in Fase B (segue symlink come se fossero file normali).

## 9. Step Implementativi

**Tutte le fasi** condividono questi principi:
- **Niente regressioni**: il codice esistente (Hello/HelloSecret/ConnectSecret/
  TestUdpJoin) non viene toccato. Nuovo codice solo in `server.rs` come ramo
  aggiuntivo di `route_connection` e in `shared.rs` come varianti enum nuove.
- **Scrivi test prima** (o subito dopo) il codice di produzione. Ogni sub-fase
  ha il proprio test elencato.
- **Aggiorna doc** ad ogni sub-fase: README.md, CLAUDE.md sezione comandi, help
  del CLI.
- **Atomicità file**: scrivi in `.bore-tmp`, rinomina su verify ok. Mai esporre
  file parziale.

---

### FASE A — Core Protocol + File Singolo (TCP relay)

**Obiettivo**: `bore transfer listener/sender` funzionante per file singolo su
TCP relay. Integrità SHA-256 post-transfer. Nessun UDP, nessuna directory,
nessuno stdin. Log informativo su cifratura canale.

#### A1 — Tipo `TransferId` e strutture dati in `shared.rs`

Prima di toccare i messaggi, definire il tipo portante:

```rust
/// Identificatore unico per una sessione di transfer.
/// UUID v4 generato dal listener (o fornito dall'utente).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferId(pub String);
```

```rust
/// Algoritmo usato per il digest di integrità.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlgorithm {
    Sha256,
    #[serde(alias = "blake3", alias = "BLAKE3")]
    Blake3,
}
```

**Anti-regressione**: `DigestAlgorithm` è nuovo, non collide con nulla.
Serializzazione JSON esplicita (nessun default che possa cambiare).

#### A2 — Nuovi messaggi in `shared.rs`

Aggiungere a `ClientMessage`:

```rust
/// Il listener si registra per un transfer su questo id.
/// Inviato come PRIMO messaggio sul control stream (come Hello/HelloSecret).
TransferListen {
    id: TransferId,
    dest_path: String,
    /// Opzioni sovrascrittura.
    overwrite: bool,
    rename: bool,
    /// Opzionale per admin page.
    notes: Option<String>,
},

/// Il sender si connette per inviare file.
/// Inviato come PRIMO messaggio sul control stream.
TransferSend {
    id: TransferId,
    /// "file", "dir", o "stdin"
    source_kind: TransferSourceKind,
    /// Percorso del file/directory (None per stdin)
    source: Option<String>,
    notes: Option<String>,
},

/// Metadata di un file in arrivo (inviato dal sender DOPO il pairing).
/// Va sul control stream o su una substream dedicata? Decisione in A4/A5.
TransferFileMeta {
    name: String,
    size: u64,
    /// Offset totale già trasferito per resume futuro.
    offset: u64,
},

/// Digest finale per verifica integrità.
TransferDigest {
    algorithm: DigestAlgorithm,
    hash: Vec<u8>,
},
```

Aggiungere a `ServerMessage`:

```rust
/// Il sender è stato abbinato al listener.
TransferReady {
    /// true = percorso UDP disponibile
    direct_available: bool,
    /// Candidate UDP (vuoto se direct_available=false)
    peer_candidates: Vec<SocketAddr>,
    /// Nonce per derivare token UDP
    nonce: [u8; UDP_NONCE_LEN],
    /// Tuning QUIC per UDP
    tuning: UdpDirectTuning,
    /// Informazioni cifratura per log
    encrypted: bool,
},

/// Un peer è in attesa dell'altro.
TransferWaiting,

/// Risultato verifica integrità finale.
TransferVerified {
    ok: bool,
    algorithm: DigestAlgorithm,
    expected_hash: Vec<u8>,
    actual_hash: Vec<u8>,
    /// Se false, messaggio descrittivo
    error_message: Option<String>,
},

/// Errore specifico del transfer (es. "dest-path non scrivibile").
TransferError(String),
```

```rust
/// Natura della sorgente del sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferSourceKind {
    File,
    Directory,
    Stdin,
}
```

**Anti-regressione**:
- Nuove varianti enum, non toccano varianti esistenti.
- `ControlFrameSummary` impl per le nuove varianti.
- `MAX_FRAME_LENGTH` (1024): lasciare a 1024. Se path > 800 byte il sender
  può usare path relativo o accorciare. In futuro eventuale configurazione
  tramite env var (`BORE_MAX_FRAME_LENGTH`) su server e client.

#### A3 — Server-side: `TransferRegistry` + matchmaking

Nuovo file `src/transfer.rs`. Strutture core:

```rust
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};

/// Pending transfer session in attesa di pairing.
struct PendingTransfer {
    id: TransferId,
    /// oneshot per svegliare il primo peer quando arriva il secondo
    peer_tx: oneshot::Sender<()>,
    /// Ruolo del peer che ha creato questo pending
    role: TransferRole,
    timestart: Instant,
}

enum TransferRole {
    Listener { dest_path: String, overwrite: bool, rename: bool },
    Sender { source_kind: TransferSourceKind, source: Option<String> },
}

impl TransferRole {
    fn variant(&self) -> &'static str {
        match self {
            TransferRole::Listener { .. } => "listener",
            TransferRole::Sender { .. } => "sender",
        }
    }
}

/// Registry thread-safe per transfer in pending.
type TransferRegistry = Arc<DashMap<String, PendingTransfer>>;

const TRANSFER_MATCH_TIMEOUT: Duration = Duration::from_secs(60);
```

Server-side dispatch in `server.rs`:


```rust
// Dentro route_connection (o funzione equivalente dove si leggono
// i primi messaggi). Aggiungere righe per i nuovi messaggi.

match first_message {
    ClientMessage::Hello(port, opts) => { /* esistente */ }
    ClientMessage::HelloSecret { .. } => { /* esistente */ }
    ClientMessage::ConnectSecret { .. } => { /* esistente */ }
    ClientMessage::TestUdpJoin { .. } => { /* esistente */ }
    ClientMessage::JoinCarrier { .. } => { /* esistente */ }
    // NUOVI:
    ClientMessage::TransferListen { id, dest_path, overwrite, rename, notes } => {
        // encrypted deriva dal controllo TLS del server
        // (es. is_tls: bool passato a route_connection)
        serve_transfer_listener(control, opener, acceptor, registry,
            id, dest_path, overwrite, rename, peer_addr, encrypted).await?
    }
    ClientMessage::TransferSend { id, source_kind, source, notes } => {
        serve_transfer_sender(control, opener, acceptor, registry,
            id, source_kind, source, peer_addr, encrypted).await?
    }
}
```

**Implementazione** a due funzioni distinte per listener e sender:

**Due funzioni simmetriche** (listener e sender). Schema:

- Primo peer arriva → inserisce pending, aspetta peer_rx con timeout 60s.
- Secondo peer arriva → rimuove pending, verifica ruolo diverso, sveglia
  il primo via peer_tx (inviando solo `()`, le info di relay sono nei rispettivi
  opener/acceptor tenuti dal server). Poi manda TransferReady a sé stesso.
- Il primo peer si sveglia (peer_rx riceve `()`) e manda TransferReady a
  sé stesso.

```rust
/// Matchmaking: due peer stesso id, ruoli diversi.
/// Restituisce true se questo peer è arrivato per primo (attesa).
async fn serve_transfer_listener(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    mut acceptor: mux::Acceptor,
    registry: TransferRegistry,
    id: TransferId,
    dest_path: String,
    overwrite: bool,
    rename: bool,
    peer_addr: SocketAddr,
    encrypted: bool,
) -> Result<()> {

    control.send(ServerMessage::TransferWaiting).await?;

    let (peer_tx, peer_rx) = oneshot::channel();
    let role = TransferRole::Listener { dest_path, overwrite, rename };
    let pending = PendingTransfer {
        id: id.clone(),
        peer_tx,
        role,
        listener: None,
        timestart: Instant::now(),
    };

    let am_first = match registry.entry(id.0.clone()) {
        Entry::Occupied(existing) => {
            let first = existing.remove();
            // Collision check: due peer stesso ruolo
            if first.role.variant() == "listener" {
                bail!("duplicate listener for id '{}'", id.0);
            }
            // Sveglia il sender
            let _ = first.peer_tx.send(());
            false // sono il secondo
        }
        Entry::Vacant(slot) => {
            slot.insert(pending);
            // Attendi sender (timeout 60s)
            timeout(TRANSFER_MATCH_TIMEOUT, peer_rx).await
                .map_err(|_| anyhow::anyhow!("timeout: no sender connected within 60s"))?
                .map_err(|_| anyhow::anyhow!("sender disconnected while waiting"))?;
            true // sono il primo
        }
    };

    // Invia TransferReady a sé stesso
    control.send(ServerMessage::TransferReady {
        direct_available: false,
        peer_candidates: vec![],
        nonce: [0; UDP_NONCE_LEN],
        tuning: UdpDirectTuning::default(),
        encrypted,
        role: if am_first { UdpTestRole::Listener } else { UdpTestRole::Dialer },
    }).await?;

    // Ora avvia ricezione dati (Fase A: TCP relay via acceptor)
    receive_via_relay(&mut control, acceptor, &dest_path,
        overwrite, rename, None).await
}

/// serve_transfer_sender identica ma speculare.
/// Inserisce pending, attende listener, collision check "sender".
async fn serve_transfer_sender(
    mut control: Delimited<mux::Stream>,
    opener: mux::Opener,
    acceptor: mux::Acceptor, // non usato dal sender (solo outbound)
    registry: TransferRegistry,
    id: TransferId,
    source_kind: TransferSourceKind,
    source: Option<String>,
    peer_addr: SocketAddr,
    encrypted: bool,
) -> Result<()> {

    control.send(ServerMessage::TransferWaiting).await?;

    let (peer_tx, peer_rx) = oneshot::channel();
    let role = TransferRole::Sender { source_kind, source };
    let pending = PendingTransfer {
        id: id.clone(), peer_tx, role,
        timestart: Instant::now(),
    };

    let am_first = match registry.entry(id.0.clone()) {
        Entry::Occupied(existing) => {
            let first = existing.remove();
            if first.role.variant() == "sender" {
                bail!("duplicate sender for id '{}'", id.0);
            }
            let _ = first.peer_tx.send(());
            false
        }
        Entry::Vacant(slot) => {
            slot.insert(pending);
            timeout(TRANSFER_MATCH_TIMEOUT, peer_rx).await
                .map_err(|_| anyhow::anyhow!("timeout: no listener within 60s"))?
                .map_err(|_| anyhow::anyhow!("listener disconnected"))?;
            true
        }
    };

    control.send(ServerMessage::TransferReady {
        direct_available: false, peer_candidates: vec![],
        nonce: [0; UDP_NONCE_LEN],
        tuning: UdpDirectTuning::default(),
        encrypted,
        role: if am_first { UdpTestRole::Dialer } else { UdpTestRole::Listener },
    }).await?;

    // Avvia invio dati (Fase A: TCP relay via opener)
    // run_sender continua con send_single_file...
    Ok(())
}
```

**Logica matchmaking** (identica per listener e sender):

1. Primo peer arriva → `registry.insert(id, Pending)` → aspetta peer.
2. Secondo peer arriva → `registry.remove(id)` → match trovato.
3. `TransferReady` inviato a ENTRAMBI con i peer address.
4. Il primo resta in attesa del secondo via `oneshot::Receiver`.
5. Timeout: se dopo 60s il peer non arriva → `TransferError("timeout")`.

**Attenzione**: la registry usa `DashMap`. Il `PendingTransfer` contiene un
`oneshot::Sender` — attenzione a non tenerlo in una entry che muore con lo
scope. Usare `Entry::remove` per estrarre.

**Anti-regressione**:
- `route_connection` è un match su `control.recv_timeout()`. Le nuove varianti
  non interferiscono con quelle esistenti perché il match è esaustivo.
- Il server non tocca `providers`, `udp_tests`, o altre registry esistenti.

**Test A3**:
- Listener arriva prima → attesa → sender arriva → match → TransferReady a
  entrambi.
- Sender arriva prima → attesa → listener arriva → match → TransferReady.
- Timeout: listener aspetta 60s, nessun sender → TransferError.
- Id già in uso con stesso tipo (due listener stesso id) → errore.

#### A4 — Client-side listener (dettaglio implementativo)

```rust
// src/transfer.rs

pub async fn run_listener(
    to: &str,
    transfer_id: Option<&str>,
    dest_path: &str,
    overwrite: bool,
    rename: bool,
    secret: Option<&str>,
    insecure: bool,
    carriers: u16,
    notes: Option<String>,
) -> Result<()> {

    // 1. Genera o usa transfer-id
    let id = match transfer_id {
        Some(s) => TransferId(s.to_string()),
        None => TransferId(Uuid::new_v4().to_string()),
    };
    if transfer_id.is_none() {
        println!("Transfer ID: {}", id.0);
    }

    // 2. Connetti al server
    let endpoint = Endpoint::parse(to);
    let socket = transport::connect(&endpoint, insecure).await?;
    let (opener, acceptor) = mux::client(socket);
    let mut control = Delimited::with_label(
        opener.open().await?,
        "transfer/listener",
    );

    // 3. Log cifratura
    let encrypted = endpoint.tls;
    info!(
        encrypted,
        transfer_id = %id.0,
        "transfer listener connecting — relay fallback will be {}",
        if encrypted { "TLS" } else { "plain TCP" },
    );

    // 4. Invia TransferListen
    control.send(ClientMessage::TransferListen {
        id: id.clone(),
        dest_path: dest_path.to_string(),
        overwrite,
        rename,
        notes,
    }).await?;

    // 5. Auth se richiesto
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control).await?;
    }

    // 6. Attendi risposta
    match control.recv_timeout().await? {
        Some(ServerMessage::TransferWaiting) => {
            info!("waiting for sender to connect...");
        }
        Some(ServerMessage::TransferReady { .. }) => {
            // Sender era già in attesa, procedi subito
        }
        Some(ServerMessage::TransferError(msg)) => {
            bail!("server rejected transfer: {msg}");
        }
        Some(ServerMessage::Challenge(_)) => {
            bail!("server requires --secret");
        }
        Some(other) => bail!("unexpected message: {other:?}"),
        None => bail!("server closed connection"),
    }

    // 7. Attendi TransferReady (se non già ricevuto)
    let ready = match_recv_ready(&mut control).await?;

    // 8. Avvia path di ricezione
    if ready.direct_available {
        info!("direct UDP path available, trying QUIC...");
        receive_via_direct(&mut control, &ready, &dest_path,
            overwrite, rename, secret).await?;
    } else {
        info!("using TCP relay path (carriers={carriers})");
        receive_via_relay(&mut control, acceptor, &dest_path,
            overwrite, rename, secret).await?;
    }

    Ok(())
}
```

**Ricezione via relay**:

```rust
async fn receive_via_relay(
    control: &mut Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    dest_path: &str,
    overwrite: bool,
    rename: bool,
    secret: Option<&str>,
) -> Result<()> {

    // 1. Accetta substream dati dal server (yamux)
    let Some(mut data) = acceptor.accept().await else {
        bail!("server closed data channel");
    };
    let mut marker = [0u8; 1];
    data.read_exact(&mut marker).await?;
    if marker[0] != mux::STREAM_READY {
        bail!("invalid stream marker");
    }

    // 2. Leggi metadata
    let meta: TransferFileMeta = recv_msg(control).await?;

    // 3. Prepara destinazione
    //    - Per file singolo: dest_path/filename
    //    - Per directory: gestito in Fase B
    let final_path = Path::new(dest_path).join(&meta.name);
    let temp_path = format!("{}.bore-tmp", final_path.display());

    // 4. Gestione collisioni
    if final_path.exists() && !overwrite && !rename {
        bail!("{} already exists. Use --overwrite or --rename.", final_path.display());
    }
    if final_path.exists() && rename {
        let renamed = find_available_name(&final_path)?;
        // Sovrascrive il nome meta di destinazione
        // ...
    }

    // 5. Crea file temporaneo
    let mut tmpfile = tokio::fs::File::create(&temp_path).await?;

    // 6. Hash context per verifica
    let mut hasher = sha2::Sha256::new();

    // 7. Copia dati da stream a file, hash incrementale
    let mut buf = vec![0u8; PROXY_BUFFER_SIZE];
    loop {
        let n = data.read(&mut buf).await?;
        if n == 0 { break; } // EOF
        tmpfile.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
    }
    tmpfile.flush().await?;
    drop(tmpfile); // close

    // 8. Ricevi digest finale via control
    let digest: TransferDigest = recv_msg(control).await?;

    // 9. Verifica hash
    let actual = hasher.finalize();
    let actual_bytes: Vec<u8> = actual.to_vec();
    let ok = constant_time_eq(&actual_bytes, &digest.hash);

    // 10. Se ok, rinomina temp → finale
    if ok {
        tokio::fs::rename(&temp_path, &final_path).await?;
        info!("integrity verified: {} matches {}", digest.algorithm,
              hex::encode(&digest.hash));
    } else {
        tokio::fs::remove_file(&temp_path).await?; // pulisci
        bail!("INTEGRITY MISMATCH: expected {} got {}",
              hex::encode(&digest.hash), hex::encode(&actual_bytes));
    }

    // 11. Invia risultato al sender
    control.send(ServerMessage::TransferVerified {
        ok,
        algorithm: digest.algorithm,
        expected_hash: digest.hash,
        actual_hash: actual_bytes,
        error_message: if ok { None } else { Some("hash mismatch".into()) },
    }).await?;

    Ok(())
}
```

**`find_available_name`**:
```rust
fn find_available_name(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().unwrap_or_default().to_str().unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    for i in 1..10000 {
        let name = if ext.is_empty() {
            format!("{stem}.{i}")
        } else {
            format!("{stem}.{i}.{ext}")
        };
        let candidate = parent.join(&name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists,
        "cannot find available name after 10000 attempts"))
}
```

**Helper functions** (condivise tra listener e sender):

```rust
/// Legge un singolo messaggio JSON dal control stream.
async fn recv_msg<T: DeserializeOwned + ControlFrameSummary>(
    control: &mut Delimited<mux::Stream>,
) -> Result<T> {
    match control.recv().await? {
        Some(msg) => Ok(msg),
        None => bail!("control stream closed"),
    }
}

/// Attende TransferReady, scartando heartbeat/Ok intermedi.
async fn match_recv_ready(
    control: &mut Delimited<mux::Stream>,
) -> Result<TransferReady> {
    loop {
        match control.recv().await? {
            Some(ServerMessage::TransferReady { .. } as ready) => return Ok(ready),
            Some(ServerMessage::Heartbeat) | Some(ServerMessage::Ok) => continue,
            Some(ServerMessage::TransferError(msg)) => bail!("server: {msg}"),
            Some(other) => bail!("expected TransferReady, got {other:?}"),
            None => bail!("server closed control stream"),
        }
    }
}

/// Controlla esistenza file e applica flag --overwrite/--rename.
/// Restituisce il path finale da usare.
fn handle_collision(target: &Path, overwrite: bool, rename: bool) -> Result<PathBuf> {
    if !target.exists() {
        return Ok(target.to_path_buf());
    }
    if overwrite {
        // cancella prima di sovrascrivere
        std::fs::remove_file(target)?;
        return Ok(target.to_path_buf());
    }
    if rename {
        return find_available_name(target);
    }
    bail!(
        "{} already exists. Use --overwrite to replace or --rename to auto-rename.",
        target.display()
    );
}
```

**`ListenerState`** (usata dal matchmaking server-side in A3):
```rust
struct ListenerState {
    dest_path: String,
    overwrite: bool,
    rename: bool,
}
```

**Anti-regressione**:
- `.bore-tmp` estensione non collide con altri tool.
- `constant_time_eq` preventisce timing attack sul confronto hash.
- La rename atomica (stesso filesystem) garantisce che file finale non sia mai
  parziale.
- Se hash mismatch, file temp cancellato.

**Test A4**:
- Listener con `--transfer-id` esplicito → deve usare quello, non generare.
- Listener senza `--transfer-id` → genera UUID, stampa, non crasha.
- `--dest-path` inesistente → errore prima di connettersi.
- `--dest-path` non scrivibile → errore.
- File esistente senza flag → errore messaggio chiaro.
- `--overwrite` → file sovrascritto, nessun errore.
- `--rename` → file rinominato con suffisso numerico.
- Ricezione file 0 byte → ok (hash di file vuoto calcolato).
- `constant_time_eq` coverage: match vs mismatch.

#### A5 — Client-side sender (dettaglio implementativo)

```rust
pub async fn run_sender(
    to: &str,
    source: &str,        // path o "stdin"
    transfer_id: &str,
    output: Option<&str>, // obbligatorio se stdin
    secret: Option<&str>,
    insecure: bool,
    carriers: u16,
    notes: Option<String>,
    stdin_timeout: u64,  // seconds, 0 = no timeout
) -> Result<()> {

    // 1. Determina source kind
    let (kind, source_path) = match source {
        "stdin" | "-" => {
            if output.is_none() {
                bail!("--output is required when --source is stdin");
            }
            (TransferSourceKind::Stdin, None)
        }
        path => {
            let md = std::fs::metadata(path)
                .with_context(|| format!("source not found: {path}"))?;
            let kind = if md.is_dir() {
                TransferSourceKind::Directory
            } else if md.is_file() {
                TransferSourceKind::File
            } else {
                bail!("source is neither file nor directory: {path}");
            };
            (kind, Some(path.to_string()))
        }
    };

    // 2. Connetti (stesso pattern listener)
    let endpoint = Endpoint::parse(to);
    let socket = transport::connect(&endpoint, insecure).await?;
    let (opener, _acceptor) = mux::client(socket);
    // _acceptor non serve al sender (solo outbound substream)
    let mut control = Delimited::with_label(
        opener.open().await?, "transfer/sender",
    );

    // 3. Log cifratura
    info!(
        encrypted = endpoint.tls,
        source_kind = ?kind,
        transfer_id,
        "transfer sender connecting — relay will be {}",
        if endpoint.tls { "TLS" } else { "plain TCP" },
    );

    // 4. Invia TransferSend
    control.send(ClientMessage::TransferSend {
        id: TransferId(transfer_id.to_string()),
        source_kind: kind,
        source: source_path,
        notes,
    }).await?;

    // 5. Auth
    if let Some(secret) = secret {
        Authenticator::new(secret)
            .client_handshake(&mut control).await?;
    }

    // 6. Attendi risposta
    let ready = match control.recv_timeout().await? {
        Some(msg @ ServerMessage::TransferReady { .. }) => {
            // Listener era già in attesa
            msg
        }
        Some(ServerMessage::TransferWaiting) => {
            // Listener non ancora arrivato
            info!("waiting for listener...");
            // Attendi TransferReady (scartando heartbeat/Ok)
            match_recv_ready(&mut control).await?
        }
        Some(ServerMessage::TransferError(msg)) => {
            bail!("server: {msg}");
        }
        Some(ServerMessage::Challenge(_)) => {
            bail!("server requires --secret");
        }
        Some(other) => bail!("unexpected: {other:?}"),
        None => bail!("server closed"),
    };
}
```

**Invio file singolo** (core data path):

```rust
async fn send_single_file(
    control: &mut Delimited<mux::Stream>,
    opener: &mux::Opener,
    file_path: &Path,
    output_name: Option<&str>,  // per stdin
    kind: TransferSourceKind,
) -> Result<()> {

    let file_name = match kind {
        TransferSourceKind::File => {
            file_path.file_name().unwrap().to_str().unwrap().to_string()
        }
        TransferSourceKind::Stdin => {
            output_name.unwrap().to_string()
        }
        _ => unreachable!(),
    };

    // 1. Apri file (o stdin)
    let mut source: Box<dyn AsyncRead + Unpin + Send> = match kind {
        TransferSourceKind::File => {
            Box::new(tokio::fs::File::open(file_path).await?)
        }
        TransferSourceKind::Stdin => {
            Box::new(tokio::io::stdin())
        }
        _ => unreachable!(),
    };

    // 2. Dimensione file (non nota per stdin)
    let file_size = if kind == TransferSourceKind::File {
        tokio::fs::metadata(file_path).await?.len()
    } else {
        0
    };

    // 3. Invia metadata su control
    control.send(ClientMessage::TransferFileMeta {
        name: file_name,
        size: file_size,
        offset: 0,
    }).await?;

    // 4. Apri data substream (yamux)
    let mut data = opener.open().await?;
    data.write_all(&[mux::STREAM_READY]).await?;
    data.flush().await?;

    // 5. Copia file → stream
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; PROXY_BUFFER_SIZE];
    loop {
        let n = source.read(&mut buf).await?;
        if n == 0 { break; }
        data.write_all(&buf[..n]).await?;
        hasher.update(&buf[..n]);
    }
    data.flush().await?;
    // Importante: shutdown parziale per segnalare EOF
    use tokio::io::AsyncWriteExt;
    let _ = data.shutdown().await;

    // 6. Invia digest
    let hash = hasher.finalize().to_vec();
    control.send(ClientMessage::TransferDigest {
        algorithm: DigestAlgorithm::Sha256,
        hash,
    }).await?;

    // 7. Ricevi verifica
    match control.recv().await? {
        Some(ServerMessage::TransferVerified { ok: true, .. }) => {
            info!("transfer SUCCESS — integrity verified");
        }
        Some(ServerMessage::TransferVerified { ok: false, actual_hash, expected_hash, .. }) => {
            bail!("INTEGRITY MISMATCH: local {} != remote {}",
                  hex::encode(&expected_hash), hex::encode(&actual_hash));
        }
        Some(ServerMessage::TransferError(msg)) => {
            bail!("transfer error: {msg}");
        }
        Some(_) => bail!("unexpected message after transfer"),
        None => bail!("listener disconnected during verification"),
    }

    Ok(())
}
```

**Nessun pre-hash in Fase A**: l'hash SHA-256 viene calcolato incrementalmente
durante la copia dei dati (unico read del file = sia invio che hash). In Fase C
si passa a BLAKE3, molto più veloce.

**Anti-regressione**:
- `data.shutdown()` manda EOF al listener senza chiudere il control stream.
- Il marker `STREAM_READY` prima dei dati segue il pattern mux esistente.
- Il control stream non viene mai usato per dati bulk.
- Flush esplicito dopo il marker.

**Test A5**:
- File piccolo (1 byte) → trasferito correttamente.
- File 10 GiB simulato (con blob temporaneo) → verifica integrità.
- File vuoto (0 byte) → hash noto (SHA-256 vuoto), verificato.
- `--source stdin` non permesso in Fase A (errore).
- File inesistente → errore subito.
- Sender con `--transfer-id` sbagliato (nessun listener) → timeout o errore.
- Verifica che listener veda hash corretto.
- Connessione interrotta durante transfer → errore listener e sender.

#### A6 — CLI in `main.rs`

```rust
#[derive(Subcommand, Debug)]
enum Command {
    // ... comandi esistenti ...
    
    /// Transfer files between two machines through a bore tunnel.
    #[clap(subcommand)]
    Transfer(TransferCommand),
}

#[derive(Subcommand, Debug)]
enum TransferCommand {
    /// Receive files: listen for an incoming transfer.
    Listener {
        /// Directory where received files are saved.
        #[clap(long, value_name = "PATH")]
        dest_path: String,

        /// Address of the bore server.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER")]
        to: String,

        /// Optional transfer id (auto-generated if omitted on listener).
        #[clap(long, value_name = "ID")]
        transfer_id: Option<String>,

        /// Overwrite existing files without asking.
        #[clap(long)]
        overwrite: bool,

        /// Auto-rename existing files with a numeric suffix.
        #[clap(long)]
        rename: bool,

        /// Optional secret for authentication.
        #[clap(short, long, value_name = "SECRET", env = "BORE_SECRET",
               hide_env_values = true)]
        secret: Option<String>,

        /// Skip TLS certificate verification.
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Number of parallel TCP carrier connections.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Free-form note for admin page.
        #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
        notes: Option<String>,
    },

    /// Send files: start a file/directory/stream transfer.
    Sender {
        /// Source path: file, directory, or "stdin"/"-".
        #[clap(long, value_name = "PATH")]
        source: String,

        /// Address of the bore server.
        #[clap(short, long, value_name = "ADDR", env = "BORE_SERVER")]
        to: String,

        /// Transfer id (must match the listener's id).
        #[clap(long, value_name = "ID")]
        transfer_id: String,

        /// Output filename (required when --source stdin).
        #[clap(long, short, value_name = "NAME")]
        output: Option<String>,

        /// Optional secret for authentication.
        #[clap(short, long, value_name = "SECRET", env = "BORE_SECRET",
               hide_env_values = true)]
        secret: Option<String>,

        /// Skip TLS certificate verification.
        #[clap(long, env = "BORE_INSECURE")]
        insecure: bool,

        /// Number of parallel TCP carrier connections.
        #[clap(long, value_name = "N", default_value_t = 1, env = "BORE_CARRIERS")]
        carriers: u16,

        /// Free-form note for admin page.
        #[clap(long, value_name = "TEXT", env = "BORE_NOTES")]
        notes: Option<String>,

        /// Idle timeout (seconds) for stdin input. 0 = no timeout.
        /// Default 1200 (20 min).
        #[clap(long, value_name = "SECS", default_value_t = 1200,
               env = "BORE_STDIN_TIMEOUT")]
        stdin_timeout: u64,
    },
}
```

```rust
async fn dispatch(command: Command) -> Result<()> {
    match command {
        // ... comandi esistenti ...

        Command::Transfer(cmd) => match cmd {
            TransferCommand::Listener {
                dest_path, to, transfer_id, overwrite, rename,
                secret, insecure, carriers, notes,
            } => {
                // Valida dest-path prima di connetterti
                let dest = Path::new(&dest_path);
                if !dest.is_dir() {
                    // Cerca di crearla
                    tokio::fs::create_dir_all(dest).await
                        .with_context(|| format!("cannot create dest-path: {dest_path}"))?;
                }
                let notes = clamp_notes(notes);
                let connect = move || {
                    let (to, dest_path, secret, notes) =
                        (to.clone(), dest_path.clone(), secret.clone(), notes.clone());
                    async move {
                        transfer::run_listener(
                            &to, transfer_id.as_deref(), &dest_path,
                            overwrite, rename, secret.as_deref(),
                            insecure, carriers, notes,
                        ).await
                    }
                };
                // no auto-reconnect for transfer — one-shot, fail clean
                connect().await?;
            }
            TransferCommand::Sender {
                source, to, transfer_id, output,
                secret, insecure, carriers, notes, stdin_timeout,
            } => {
                let notes = clamp_notes(notes);
                transfer::run_sender(
                    &to, &source, &transfer_id, output.as_deref(),
                    secret.as_deref(), insecure, carriers, notes, stdin_timeout,
                ).await?;
            }
        },
    }
}

// serve_transfer non serve: transfer è one-shot (non resta in ascolto).
// connect().await? esegue run_listener/run_sender e termina.
```

**Nota**: `auto_reconnect` per transfer è disabilitato (non listato nei flag
di transfer). Se transfer fallisce, log chiaro con causa e file falliti,
nessun re-tentativo automatico.

#### A7 — Aggiornamento documentazione

- `README.md`: aggiungere sezione `bore transfer` con esempi:
  ```
  # Terminal B (receiver)
  bore transfer listener --dest-path ./downloads --to bore.example.com
  
  # Terminal A (sender)
  bore transfer sender --source ./backup.tar.gz \
    --transfer-id <ID-da-listener> --to bore.example.com
  ```
- `CLAUDE.md`: aggiungere comandi transfer nei test e nella sezione comandi.
- `--help` del CLI già coperto da clap + doc comment.

#### A8 — Test di sistema per Fase A

Test di integrazione stile `tests/e2e_test.rs`. Nuovo file `tests/transfer_test.rs`.

```rust
use lazy_static::lazy_static;
use std::sync::Mutex;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

/// Test base: sender → server → listener, file piccolo
#[tokio::test]
async fn transfer_single_file_basic() {
    let _guard = SERIAL_GUARD.lock().unwrap();
    // setup: avvia server su porta di test
    // setup: avvia listener in background task
    // setup: avvia sender in background task
    // attendi completamento
    // verifica file esista in dest-path
    // verifica hash matchi
}

/// Test: file vuoto
#[tokio::test]
async fn transfer_empty_file() { /* ... */ }

/// Test: file grande (> 1 MiB)
#[tokio::test]
async fn transfer_large_file() { /* ... */ }

/// Test: --overwrite
#[tokio::test]
async fn transfer_overwrite_existing() { /* ... */ }

/// Test: --rename
#[tokio::test]
async fn transfer_rename_existing() { /* ... */ }

/// Test: file esistente senza flag → errore
#[tokio::test]
async fn transfer_no_clobber_error() { /* ... */ }

/// Test: --transfer-id matcha
#[tokio::test]
async fn transfer_explicit_id() { /* ... */ }

/// Test: integrity mismatch (sender manda hash sbagliato)
#[tokio::test]
async fn transfer_integrity_failure() { /* ... */ }
```

**Anti-regressione nei test**:
- Ogni test usa `SERIAL_GUARD` (come i test esistenti) per evitare race su
  porte TCP.
- Usa `tempfile::TempDir` per `dest-path`.
- Server su porta di test: `CONTROL_PORT + random offset`.
- Timeout a tutti i test: `tokio::time::timeout(Duration::from_secs(30))`.
- Verifica che server non abbia left-over state dopo ogni test (no leak id).
- Verifica che i test esistenti (`basic_proxy`, `secret_*`) passino ancora
  dopo le modifiche a `server.rs` e `shared.rs`.

**Dimensione stimata Fase A**: ~800-1000 nuove linee di Rust (80% transfer.rs,
  10% shared.rs, 5% server.rs, 5% main.rs)

---

### FASE B — UDP Direct Path + Directory + Stdin

**Obiettivo**: supporto completo con UDP hole-punching, directory ricorsive,
stdin, progress display.

#### B1 — UDP direct path per transfer

Il `TransferReady` già contiene `peer_candidates`, `nonce`, `tuning`. Dopo
ready, entrambi i peer avviano negoziazione UDP (stessa dinamica di
`udp_diagnostic.rs` / `secret.rs`).

**Implementazione**:

```rust
/// Dopo TransferReady, se direct_available=true, prova UDP.
async fn try_direct_path(
    control: &mut Delimited<mux::Stream>,
    ready: &TransferReady,
    secret: Option<&str>,
    udp_port: u16,
    stun_server: Option<&str>,
    port_map: bool,
    port_prediction: bool,
) -> Result<Option<holepunch::DirectConn>> {

    if !ready.direct_available {
        return Ok(None);
    }

    // 1. Setup UDP socket
    let socket = holepunch::bind_socket(udp_port).await?;

    // 2. Gathering candidates se non già fatti dal server
    //    Il server ci ha già dato i peer_candidates.
    //    Ma dobbiamo anche mandare i NOSTRI candidati al peer.
    //    Questo richiede un round di scambio — riusare broker_udp?

    // 3. Punch + QUIC connect/listen
    let token = holepunch::derive_token(secret, &ready.nonce);

    // Decide ruolo: chi arriva prima è listener QUIC?
    // Semplice: sender connette, listener accetta
    // (come in test-udp: chi si connette per primo diventa Listener)

    todo!("Fase B implementazione completa")
}
```

**Integrazione con broker UDP esistente**:
- Il broker server-side (`secret::broker_udp`) richiede un provider già
  registrato con candidates. Per transfer, entrambi i peer fanno STUN e
  mandano candidates via control.
- **Approccio**: dopo `TransferReady`, ogni peer fa il proprio STUN gathering
  e invia `ClientMessage::UdpCandidateOffer`. Il server matcha e invia
  `ServerMessage::UdpPunch` come già fa per i secret tunnel.
- **Ruolo QUIC**: il server decide chi è Listener e chi Dialer (stessa logica
  di `test-udp` in `udp_diagnostic.rs`: il primo peer che arriva diventa
  Listener, il secondo Dialer). Il server assegna il ruolo nel
  `TransferReady`.
- **Riutilizzo**: `broker_udp` da `secret.rs` può essere chiamato da
  `serve_transfer_listener`/`serve_transfer_sender`. Oppure
  si generalizza in una funzione `holepunch::broker_pair`.

#### B2 — Directory ricorsiva (multi-file)

**Sender**:

```rust
async fn send_directory(
    control: &mut Delimited<mux::Stream>,
    opener: &mux::Opener,
    dir_path: &Path,
) -> Result<()> {

    // 1. Walk ricorsivo (walkdir) per collezionare file
    let files: Vec<DirEntry> = walkdir::WalkDir::new(dir_path)
        .follow_links(false) // Fase A: no follow
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();

    info!("sending {} files from {}", files.len(), dir_path.display());

    // 2. Calcola root hash BLAKE3 cumulativo
    //    (oppure SHA-256 se BLAKE3 non ancora in Fase A)
    let mut root_hasher = sha2::Sha256::new();

    // 3. Per ogni file
    for entry in &files {
        let abs_path = entry.path();
        let relative = abs_path.strip_prefix(dir_path)
            .expect("walk dir prefix");

        // Skip symlink/special (già filtrato da walkdir)

        // Invia metadata
        let md = tokio::fs::metadata(abs_path).await?;
        control.send(ClientMessage::TransferFileMeta {
            name: relative.to_str().unwrap().to_string(),
            size: md.len(),
            offset: 0,
        }).await?;

        // Apri substream dati
        let mut data = opener.open().await?;
        data.write_all(&[mux::STREAM_READY]).await?;
        data.flush().await?;

        // Stream file → substream, hash cumulativo
        let mut file = tokio::fs::File::open(abs_path).await?;
        let mut buf = vec![0u8; PROXY_BUFFER_SIZE];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 { break; }
            data.write_all(&buf[..n]).await?;
            root_hasher.update(&buf[..n]);
        }
        data.flush().await?;
        let _ = data.shutdown().await;
    }

    // 4. Invia root hash finale
    let root_hash = root_hasher.finalize().to_vec();
    control.send(ClientMessage::TransferDigest {
        algorithm: DigestAlgorithm::Sha256,
        hash: root_hash,
    }).await?;

    // 5. Ricevi verifica
    match control.recv().await? {
        Some(ServerMessage::TransferVerified { ok: true, .. }) => {
            info!("directory transfer SUCCESS — all {} files verified", files.len());
        }
        Some(ServerMessage::TransferVerified { ok: false, .. }) => {
            bail!("directory integrity check failed");
        }
        _ => bail!("unexpected response after directory transfer"),
    }
    Ok(())
}
```

**Listener** (ricezione directory — `.bore-tmp` fino a verifica finale):

```rust
async fn receive_directory(
    control: &mut Delimited<mux::Stream>,
    mut acceptor: mux::Acceptor,
    dest_path: &Path,
    overwrite: bool,
    rename: bool,
) -> Result<()> {

    let mut root_hasher = sha2::Sha256::new();
    // Accumula coppie (temp_path, final_path) per rename atomica finale
    let mut pending_files: Vec<(PathBuf, PathBuf)> = Vec::new();

    loop {
        // 1. Leggi metadata file
        let meta: TransferFileMeta = match control.recv().await? {
            Some(ClientMessage::TransferFileMeta(m)) => m,
            // Se invece arriva TransferDigest, è l'ultimo
            Some(ClientMessage::TransferDigest(d)) => {
                // VERIFICA PRIMA di rinominare
                return verify_and_finalize(d, root_hasher, dest_path, &pending_files).await;
            }
            Some(other) => bail!("expected file meta, got {other:?}"),
            None => bail!("control stream closed"),
        };

        // 2. Path traversal check (unificato con is_path_traversal)
        let rel_path = Path::new(&meta.name);
        let dest_root = dest_path.canonicalize()
            .unwrap_or_else(|_| dest_path.to_path_buf());
        if is_path_traversal(rel_path, &dest_root) {
            bail!("path traversal detected: {}", meta.name);
        }
        let target = dest_root.join(rel_path);

        // 3. Crea directory padre
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await
                .context("create parent directory")?;
        }

        // 4. Gestione collisioni — controlla ma non cancella (rename finale)
        handle_collision(&target, overwrite, rename)?;

        // 5. Accetta substream
        let Some(mut data) = acceptor.accept().await else {
            bail!("data channel closed");
        };
        let mut marker = [0u8; 1];
        data.read_exact(&mut marker).await?;

        // 6. Ricevi file in .bore-tmp
        let temp_path = format!("{}.bore-tmp", target.display());
        let mut tmpfile = tokio::fs::File::create(&temp_path).await?;
        let mut buf = vec![0u8; PROXY_BUFFER_SIZE];
        loop {
            let n = data.read(&mut buf).await?;
            if n == 0 { break; }
            tmpfile.write_all(&buf[..n]).await?;
            root_hasher.update(&buf[..n]);
        }
        tmpfile.flush().await?;
        drop(tmpfile);

        // Accumula: non rinominare ancora
        pending_files.push((PathBuf::from(&temp_path), target));
        info!("received (pending): {}", meta.name);
    }
}

/// Verifica root hash, poi rinomina tutti i .bore-tmp → finali.
async fn verify_and_finalize(
    digest: TransferDigest,
    hasher: sha2::Sha256,
    dest_path: &Path,
    pending: &[(PathBuf, PathBuf)],
) -> Result<()> {
    let actual = hasher.finalize().to_vec();
    if actual != digest.hash {
        // Pulisci temp file prima di fallire
        for (temp, _) in pending {
            let _ = tokio::fs::remove_file(temp).await;
        }
        bail!(
            "directory integrity FAILED — {} files discarded. "\
            "Expected {} got {}",
            pending.len(),
            hex::encode(&digest.hash),
            hex::encode(&actual),
        );
    }
    // Root hash OK → rename atomica di tutti i file
    for (temp, final_path) in pending {
        tokio::fs::rename(temp, final_path).await
            .with_context(|| format!("failed to rename {temp:?} -> {final_path:?}"))?;
    }
    info!("directory integrity OK — {} files finalized", pending.len());
    Ok(())
}
```

**Path traversal protection** (unica funzione, chiamata dal listener directory):

```rust
fn is_path_traversal(rel_path: &Path, dest_root: &Path) -> bool {
    let dest_root = dest_root.canonicalize().unwrap_or_else(|_| dest_root.to_path_buf());
    let target = dest_root.join(rel_path);
    match target.canonicalize() {
        Ok(canon) => !canon.starts_with(&dest_root),
        Err(_) => {
            // Il file non esiste (normale), verifica componenti
            rel_path.components().any(|c| matches!(c, std::path::Component::ParentDir))
        }
    }
}
```

**Test B2**:
- Directory con 1 file → trasferito, struttura preservata.
- Directory con 3 livelli di profondità → struttura preservata.
- Directory con 100 file piccoli → tutti trasferiti.
- Path traversal tentato (`../etc/passwd`) → rifiutato con errore.
- `--follow-links` (B2 enhancement): symlink a file regolare → trasferito.
- Directory vuota → trasferimento completato (0 file).

#### B3 — Stdin support

Sender già descritto in A5 (boxed `tokio::io::stdin()` come sorgente). Punti
implementativi specifici:

```rust
// Rilevamento stdin
fn is_stdin_piped() -> bool {
    use std::io::IsTerminal;
    !std::io::stdin().is_terminal()
}

// Uso nel sender:
let kind = match source {
    "stdin" | "-" => {
        if output.is_none() {
            bail!("--output required with stdin");
        }
        // Opzionale: se stdin non è pipe, warn
        if !is_stdin_piped() {
            warn!("stdin is a terminal, waiting for input...");
        }
        TransferSourceKind::Stdin
    }
    // ...
};
```

**Timeout di lettura stdin**: default 1200 secondi (20 min). Configurabile con
`--stdin-timeout <SECS>` (0 = nessun timeout). Se nessun dato ricevuto per
l'intera durata del timeout, transfer abortisce con errore:
```
ERROR: stdin idle timeout after 1200s (0 bytes received).
```

**Listener per stdin**: nessuna differenza dalla ricezione file singolo.
Salva in `dest_path/output`. Riceve metadata con size=0.

**Test B3**:
- `echo "hello world" | bore transfer sender --source stdin --output hello.txt`
  → verifica hello.txt contenga "hello world".
- Pipe di file grande: `cat largefile.bin | bore transfer ...` → integrità ok.
- Stdin vuoto: file output 0 byte.
- `--source stdin` senza `--output` → errore messaggio.
- Stdin interrotto a metà (kill del pipe) → errore listener.

#### B4 — Progress display

**Dipendenze `Cargo.toml`**:
```toml
[dependencies]
indicatif = "0.17"
```

**Implementazione**:

```rust
use indicatif::{ProgressBar, ProgressStyle, ProgressState};
use std::fmt::Write;

struct TransferUI {
    bar: Option<ProgressBar>,
    multi: Option<indicatif::MultiProgress>,
    bytes_start: Instant,
    last_update: Instant,
    last_bytes: u64,
    speed_window: VecDeque<(Instant, u64)>,
}

impl TransferUI {
    fn new_file(size: u64, name: &str, file_index: usize, total_files: usize) -> Self {
        let bar = ProgressBar::new(size);
        let style = ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] \
             {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {msg}"
        ).unwrap();
        bar.set_style(style);
        bar.set_message(format!("{} ({}/{})", name, file_index, total_files));
        TransferUI {
            bar: Some(bar),
            multi: None,
            bytes_start: Instant::now(),
            last_update: Instant::now(),
            last_bytes: 0,
            speed_window: VecDeque::new(),
        }
    }

    fn new_unknown_size(name: &str) -> Self {
        // Spinner per stdin (size sconosciuta)
        let bar = ProgressBar::new_spinner();
        bar.set_message(format!("{} (unknown size)", name));
        TransferUI {
            bar: Some(bar),
            multi: None,
            bytes_start: Instant::now(),
            last_update: Instant::now(),
            last_bytes: 0,
            speed_window: VecDeque::new(),
        }
    }

    fn update(&mut self, bytes_received: u64) {
        let now = Instant::now();
        self.speed_window.push_back((now, bytes_received));
        // Mantieni ultimo 1 secondo di dati per velocità istantanea
        while let Some(&(t, _)) = self.speed_window.front() {
            if now - t > Duration::from_secs(1) {
                self.speed_window.pop_front();
            } else {
                break;
            }
        }
        if let Some(bar) = &self.bar {
            bar.set_position(bytes_received);
        }
    }

    fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_with_message("done");
        }
    }

    fn finish_with_error(&self, msg: &str) {
        if let Some(bar) = &self.bar {
            bar.finish_with_message(format!("FAILED: {msg}"));
        }
    }
}
```

**Integrazione nel loop di send/receive**:
- Ogni 100ms (o ogni chunk), chiama `ui.update(bytes)`.
- `indicatif` stampa su stderr, non interferisce con stdout dell'app.

**Test B4**: (test visivi, non automatizzabili via CI)
- Avviare transfer e verificare che barra appaia aggiornata.
- Velocità misurata ragionevole (±20% della velocità reale).
- Nessun output su stdout durante progress.

#### B5 — Aggiornamento documentazione

- `README.md`: aggiungere flag `--source` (file/dir/stdin), `--output`,
  esempi con directory e pipe.
- `CLAUDE.md`: aggiornare comandi di test con `--source stdin`.

#### B6 — Test di sistema per Fase B

```rust
/// Test: directory con 5 file
#[tokio::test]
async fn transfer_directory_basic() { /* ... */ }

/// Test: directory con sottodirectory
#[tokio::test]
async fn transfer_directory_nested() { /* ... */ }

/// Test: stdin pipe
#[tokio::test]
async fn transfer_stdin_pipe() { /* ... */ }

/// Test: stdin large pipe (> 1 MiB)
#[tokio::test]
async fn transfer_stdin_large() { /* ... */ }

/// Test: path traversal rifiutato
#[tokio::test]
async fn transfer_path_traversal_rejected() { /* ... */ }

/// Test: UDP fallback (simula STUN finto, fallisce, usa TCP)
#[tokio::test]
async fn transfer_udp_fallback() { /* ... */ }
```

**Attenzione**: test UDP richiedono sudo (raw socket) su alcuni OS. Marcati
`#[cfg(feature = "udp")]` e possibilmente `#[ignore]` in CI standard.

**Dimensione stimata Fase B**: ~500-700 nuove linee di Rust

---

### FASE C — Integrity Avanzata + Polish + Report

**Obiettivo**: robustezza, chunk verification incrementale, metriche finali,
security hardening.

#### C1 — BLAKE3 al posto di SHA-256

**Dipendenze `Cargo.toml`**:
```toml
[dependencies]
blake3 = "1.5"
```

Modificare tutte le occorrenze di `sha2::Sha256` in `src/transfer.rs` con
`blake3::Hasher`:

```rust
use blake3::Hasher;

// Hasher incrementale
let mut hasher = Hasher::new();
hasher.update(&buf[..n]);
let hash: [u8; 32] = hasher.finalize().into();
let hash_bytes: Vec<u8> = hash.to_vec();
```

**Vantaggi**:
- ~5× più veloce di SHA-256 in software puro.
- `Hasher::finalize()` è un'operazione O(1) (copia 32 byte).
- Supporta `update()` incrementale come SHA-256.
- `blake3::hash(path)` per hash diretto di file.

#### C2 — Chunk verification incrementale

Nuovo messaggio:

```rust
// In shared.rs
ClientMessage::TransferChunk {
    /// Indice progressivo del chunk
    index: u64,
    /// Numero di byte in questo chunk
    size: u32,
    /// BLAKE3 hash del chunk
    hash: [u8; 32],
}
```

**Sender** (streaming con chunk hash):

```rust
const CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB

async fn send_with_chunks(
    data: &mut (dyn AsyncRead + Unpin + Send),
    stream: &mut mux::Stream,
    control: &mut Delimited<mux::Stream>,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut chunk_index = 0u64;
    let mut root_hasher = blake3::Hasher::new();

    loop {
        let mut chunk_hasher = blake3::Hasher::new();
        let mut chunk_bytes = 0u32;

        // Leggi fino a CHUNK_SIZE byte
        while chunk_bytes < CHUNK_SIZE as u32 {
            let n = data.read(&mut buf[chunk_bytes as usize..]).await?;
            if n == 0 { break; }
            chunk_hasher.update(&buf[chunk_bytes as usize..][..n]);
            root_hasher.update(&buf[chunk_bytes as usize..][..n]);
            chunk_bytes += n as u32;
        }

        if chunk_bytes == 0 { break; } // EOF

        // Invia chunk hash prima dei dati
        let chunk_hash = chunk_hasher.finalize();
        control.send(ClientMessage::TransferChunk {
            index: chunk_index,
            size: chunk_bytes,
            hash: chunk_hash.into(),
        }).await?;

        // Invia chunk data
        stream.write_all(&buf[..chunk_bytes as usize]).await?;
        stream.flush().await?;

        chunk_index += 1;
    }

    // Root hash finale
    control.send(ClientMessage::TransferDigest {
        algorithm: DigestAlgorithm::Blake3,
        hash: root_hasher.finalize().as_bytes().to_vec(),
    }).await?;
}
```

**Listener** (verifica chunk incrementale):

```rust
async fn recv_with_chunks(
    stream: &mut mux::Stream,
    control: &mut Delimited<mux::Stream>,
    file: &mut tokio::fs::File,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK_SIZE];

    loop {
        // Aspetta o chunk hash o digest finale
        match control.recv().await? {
            Some(ClientMessage::TransferChunk { index, size, hash }) => {
                let mut chunk_hasher = blake3::Hasher::new();
                let mut remaining = size as usize;
                while remaining > 0 {
                    let n = stream.read(&mut buf[..remaining]).await?;
                    if n == 0 { break; }
                    chunk_hasher.update(&buf[..n]);
                    file.write_all(&buf[..n]).await?;
                    remaining -= n;
                }
                let actual_hash = chunk_hasher.finalize();
                if actual_hash.as_bytes() != &hash {
                    // Richiedi ritrasmissione
                    control.send(ClientMessage::TransferChunkRetry { index }).await?;
                    // Attendi nuovo chunk
                    continue;
                }
            }
            Some(ClientMessage::TransferDigest { algorithm, hash }) => {
                // Final digest: verifica root hash
                // (root hash cumulativo)
                return Ok(());
            }
            Some(other) => bail!("unexpected: {other:?}"),
            None => bail!("stream closed"),
        }
    }
}
```

**Nuovo messaggio per retry**:
```rust
ClientMessage::TransferChunkRetry {
    index: u64,
}
```

**Test C2**:
- Chunk hash matcha sempre (test happy path).
- Chunk corruption simulato: altera byte, listener rileva e richiede retry.
- Retry funziona: sender reinvia chunk, listener verifica e prosegue.
- File diviso esattamente in N chunk + resto (edge: ultimo chunk più piccolo).
- File più piccolo di CHUNK_SIZE → un solo chunk.

#### C3 — Metriche finali

Report stile `rsync`:

```
Transferred: 2.4 GiB in 34.2s (71.9 MB/s)
Files: 147 transferred, 0 skipped, 0 failed
Integrity: BLAKE3 verified OK
Path: direct UDP (QUIC, RTT 12ms)
```

Implementazione:

```rust
#[derive(Default)]
struct TransferMetrics {
    bytes_total: u64,
    elapsed: Duration,
    files_completed: u32,
    files_skipped: u32,
    files_failed: u32,
    integrity_ok: bool,
    path: String,     // "relay TCP" o "direct UDP (QUIC)"
    encrypted: bool,
}

impl Display for TransferMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mbps = if self.elapsed.as_secs_f64() > 0.0 {
            self.bytes_total as f64 * 8.0 / 1_000_000.0 / self.elapsed.as_secs_f64()
        } else {
            0.0
        };
        writeln!(f, "Transferred: {} in {:.1}s ({:.1} MB/s)",
            format_bytes(self.bytes_total),
            self.elapsed.as_secs_f64(),
            mbps)?;
        writeln!(f, "Files: {} transferred, {} skipped, {} failed",
            self.files_completed, self.files_skipped, self.files_failed)?;
        writeln!(f, "Integrity: {}",
            if self.integrity_ok { "verified OK (BLAKE3)" } else { "MISMATCH" })?;
        write!(f, "Path: {} ({})",
            self.path,
            if self.encrypted { "encrypted" } else { "plain" })?;
        Ok(())
    }
}
```

**Test C3**: (manuale / snapshot test)

#### C4 — Security hardening

**Temp file + atomic rename** (già in Fase A):
- Scrittura in `.bore-tmp`.
- Verify prima della rename.
- `std::fs::rename` atomica su stesso filesystem.

**Path traversal protection** (già in B2):
```rust
// Blocca assoluti e parent-dir
if rel_path.is_absolute()
    || rel_path.components().any(|c| matches!(c, Component::ParentDir))
{
    bail!("SECURITY: path traversal blocked: {}", rel_path.display());
}
// Verifica canonicalization
let canonical = dest_path.canonicalize()?;
let target = canonical.join(rel_path);
// target deve stare dentro canonical
```

**`--secret` enforcement**: se il server ha auth, transfer fallisce se senza
secret. In Fase C, possiamo rendere `--secret` obbligatorio:
```rust
if secret.is_none() {
    warn!("WARNING: transfer without --secret is insecure. \
           Anyone with the transfer ID can intercept.");
}
```

**Timeout**:
- Attesa peer: 60s → `TransferError("timeout waiting for peer")`.
- Attesa dati su substream: nessun timeout (come i tunnel esistenti).
- Idle stdin: 5 min → errore.

**Test C4**:
- Path traversal nel nome file → rifiutato, errore chiaro.
- `--secret` omesso con server auth → errore.
- Timeout peer → errore.

#### C5 — Aggiornamento documentazione

- `README.md`: aggiungere sezione integrità, spiegare BLAKE3, chunk hash.
- `CLAUDE.md`: aggiornare comandi test con flag avanzati.
- Help del CLI: già coperto da clap + doc comment.

#### C6 — Test di sistema per Fase C

```rust
/// Test: chunk verification ok
#[tokio::test]
async fn transfer_chunk_verify() { /* ... */ }

/// Test: chunk corruption detected + retry
#[tokio::test]
async fn transfer_chunk_retry() { /* ... */ }

/// Test: path traversal blocked
#[tokio::test]
async fn transfer_security_path_traversal() { /* ... */ }

/// Test: --secret required
#[tokio::test]
async fn transfer_secret_required() { /* ... */ }

/// Test: timeout attesa peer
#[tokio::test]
async fn transfer_timeout() { /* ... */ }

/// Test: file da 1 GB benchmark (solo #[ignore])
#[tokio::test]
#[ignore]
async fn transfer_benchmark_1gb() { /* ... */ }
```

**Dimensione stimata Fase C**: ~300-500 nuove linee di Rust

---

## 10. Stima Totale

| Fase | Linee stimate | Dipendenza nuova | Rischio |
|------|---------------|------------------|---------|
| A    | 600-800       | `sha2` (già in tree via ring indiretto) | Basso |
| B    | 400-600       | `indicatif`, `walkdir` | Medio |
| C    | 300-500       | `blake3` (crate singolo, auditato) | Medio-alto |
| **Totale** | **1300-1900** | | |

Dipendenze nuove totali: `blake3`, `indicatif`, `walkdir`.
Nessuna modifica a codice esistente tunnel/public/secret.

---

## 11. Risposte alle tue domande specifiche

### CRC vs tecniche più evolute

**CRC32 non basta.** È per errori casuali di trasmissione (bit flip), non per
integrità intenzionale. Due file diversi possono avere stesso CRC32 con
probabilità ~1/2³² (troppo alta per uso serio).

**SHA-256** va bene, standard, già supportabile via `ring` (dipendenza attuale
di bore). Lento per TB di dati.

**BLAKE3** è la scelta giusta: velocissimo (~1.5 GB/s SW puro), supporto
streaming nativo, tree hashing per verifica incrementale, già auditato e usato
da migliaia di progetti Rust.

### Streaming (tar) integrità

Per streaming senza size nota:

1. **Hash cumulativo** (BLAKE3): sender aggiorna hasher incrementale mentre
   legge da stdin. Alla fine invia digest. Semplice, efficace. **Consigliato
   per Fase A.**

2. **Chunk hashing** (BLAKE3 tree): dividi in chunk 1 MiB, ogni chunk ha
   hash, root hash finale. Permette rilevamento immediate e resume granulare.
   **Consigliato per Fase C.**

3. **Nota**: tar ha checksum per-header (campo `chksum`), ma verifica solo
   l'header, non il contenuto. Affidarsi solo al checksum tar è insufficiente.

### Performance: il server non deve bufferizzare

Nel TCP relay, `copy_bidirectional_with_sizes` usa `PROXY_BUFFER_SIZE` (64 KiB)
per endpoint. I dati non vengono accumulati in RAM lato server, sono piped
direttamente tra i due yamux substream. `--carriers N` evita HOL su singola
TCP. ✅ Già architetturalmente corretto.

---

## 12. TODO Futuro — Transfer Resume

**Non implementato in Fase A/B/C.** Questa sezione documenta il design per
chi voglia aggiungere resume in futuro.

### 12.1 Problema

Sender si disconnette dopo aver trasferito 70% di un file da 10 GiB.
Attualmente: file parziale cancellato, si ricomincia da capo.

Con resume: sender si riconnette, listener ha già chunk 0-7000 su 10000,
sender invia solo chunk 7001-9999.

### 12.2 Stato su disco

Listener salva metadati di resume in file affianco al `.tmp`:
```
dest-path/file.tar.gz.tmp          # dati parziali
dest-path/file.tar.gz.tmp.resume   # JSON metadati
```
Metadati:
```json
{
  "transfer_id": "uuid",
  "file_name": "file.tar.gz",
  "file_size": 10737418240,
  "chunk_size": 1048576,
  "chunks_received": [0, 1, 2, ..., 6999],
  "chunk_hashes": ["ab...", "cd...", ...],
  "root_hash": "ef...",
  "algorithm": "BLAKE3"
}
```

### 12.3 Messaggi nuovi

```rust
ClientMessage::TransferResume {
    id: String,
    /// Last root hash the sender knows (for verifying it's the same file)
    known_root_hash: Vec<u8>,
}

ServerMessage::TransferResumeStatus {
    can_resume: bool,
    chunks_received: Vec<u64>,    // indici chunk già ricevuti
    chunk_size: u32,
    total_size: u64,
}
```

### 12.4 Flusso resume

1. Sender si riconnette con `TransferResume{id, known_root_hash}`.
2. Server matcha con listener ancora in ascolto (o listener rieffettua
   `TransferListen`).
3. Listener risponde con `TransferResumeStatus`.
4. Sender confronta: salta chunk già presenti, invia solo mancanti.
5. Listener scrive solo chunk mancanti, non rilegge il file.
6. Alla fine: root hash BLAKE3 verifica integrità complessiva.

### 12.5 Sfide

- **Timeout**: listener deve tenere stato per quanto tempo? (default 1h?)
- **Cache pulizia**: listener cancella `.tmp.resume` dopo N ore.
- **File modificato**: se sender riprende con file diverso (dimensione o hash
  root diverso) → fallback a transfer completo.
- **Directory resume**: molto più complesso (file a metà, alcuni completi,
  altri no). Richiede `ResumeManifest` per l'intera directory.
- **Sicurezza**: attacker potrebbe enumerare chunk ricevuti. Necessaria auth
  sul resume.
- **Atomicità**: rinomina `.tmp` → file finale solo dopo verifica root hash.
  Se resume fallisce, `.tmp` preservato per altro tentativo.
