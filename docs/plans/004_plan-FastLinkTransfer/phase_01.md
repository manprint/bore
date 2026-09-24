# Phase 0 — Motore fast link in-process

Intent: modulo `src/fast_link/` completo e testato con stream in memoria: config, parse
HTTP, framing del body, preview, ID, macchina a stati upload/download con handoff,
finestra replay, re-arm, scadenza, stall, metriche. Nessun wiring nel server.
Prerequisites: none (G-BASE verde al baseline).
Phase closure: P0; review by agent-1:opus.

## State and ownership contract
Read STATE.md §0–1 first for scope, ownership, recovery, checks, and commit rules.
Open before edits; checkpoint at a recovery boundary; close with evidence. A
delegated worker follows its assigned OPEN unit. Missing design goes to agent-1:opus.

## Local design context
Plan revision: 2.

- **D1** Upload con HTTP Basic: credenziale `USER:PASS` (entrambi non vuoti) in `FastLinkConfig.auth: BasicAuth`. Download senza credenziali: l'ID è la credenziale.
- **D2** Preview non consumanti: UA bot → `200 text/html` generico; `Range` presente e diverso da `bytes=0-` → `416`; finestra replay `REPLAY_WINDOW_BYTES = 4 MiB`.
- **D3** Un ricevitore. **D5** attesa default 3600 s.
- **D7/D8** Solo transito. Durante lo streaming la pompa (0.3) usa DUE task: R possiede l'uploader (read + decifratura TLS + framing), W possiede il downloader (cifratura TLS + write + flush), collegate da `PUMP_DEPTH = 4` buffer fissi da `crate::shared::proxy_buffer_size()` riciclati (zero allocazioni a regime); chunked passthrough; niente hash; niente `tokio::io::copy`. Fuori dallo streaming la task di sessione U possiede l'uploader.
- **D9** `secure == false`: `GET`/`HEAD` → `308` `Location: https://<host><target>`; ogni altro metodo → `403`.
- **D10** Uploader: `[100 Continue]` + `200` chunked; riga 1 = URL + `\n`; poi righe `# ...\n`; successo → chunk finale `0\r\n\r\n` + shutdown; ogni altro esito → riga `# failed:`/`# expired:` e shutdown SENZA terminatore.
- **D11** Framing: vedi 0.1 `upload_framing` e 0.2 `BodyFramer`.
- **D12** Target: vedi 0.1 `parse_upload_target` / `parse_download_target`.
- **D13** Stati e handoff: vedi 0.3.
- **D14** Risposte downloader: 404 / 409 / 200-preview / 416 / HEAD.
- **D15** Costanti: vedi 0.1.
- **D18** Log: `id_prefix = &id[..4]`; mai ID completo, mai header Authorization.
- **D19/D20** Validazione config: vedi 0.1 `resolve_server_config`.
- **D21** Nessun ritardo su 401; `auth_failures_total += 1`; linger close.
- **I-2** RAM per trasferimento ≤ finestra replay (4 MiB) + `PUMP_DEPTH` × `proxy_buffer_size()` (1 MiB di default) + buffer di attesa. **I-3** re-arm solo con replay integro. **I-4** exit 0 ⟺ completo. **I-5** troncamento mai completo. **I-6** flush dopo ogni write. **I-7** auth prima di 100/slot, solo head. **I-8** slot rimosso su ogni uscita.
- **R1** curl `-T file` → `PUT /<basename>` + CL + Expect; `-T -` → `PUT /` (o path dato) + chunked + Expect; continua a inviare dopo il 200 anticipato. **R2** RFC 9110 §10.1.1. **R3** RFC 9112 §6.3/§7.1: CL+TE errore; grammatica chunked. **R4** Slackbot usa Range.

Esempio byte uploader (successo, CL 5):
```
HTTP/1.1 100 Continue\r\n\r\n
HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n
3a\r\nhttps://fast.bore.local:8443/abcdefghij012345/miofile.tar\n\r\n
...\r\n# waiting for the download (expires in 60 min); nothing is stored on the server\n\r\n
...\r\n# download started\n\r\n
...\r\n# done: 5 bytes in 0.0 s (0.0 MiB/s)\n\r\n
0\r\n\r\n
```
Download (CL):
```
HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: <content_disposition(nome)>\r\nContent-Length: 5\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Robots-Tag: noindex, nofollow\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n<5 byte>
```
Chunked: come sopra ma `Transfer-Encoding: chunked` al posto di `Content-Length`, poi i byte chunked dell'uploader verbatim (incluso `0\r\n\r\n`).

## Sub-phases

### 0.1 Config, parse HTTP, target, preview, ID, byte delle risposte (puro)
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus dopo il diff (focus I-7: auth solo sui byte head; validazione config D19/D20).
- **Files:** READ `src/basicauth.rs` (`BasicAuth`, `UNAUTHORIZED`), `src/transfer_link/source.rs` (`validate_filename`, `content_disposition`, `encode_path_segment`, `MIME_OCTET_STREAM`), `src/transfer_link_cli.rs` `generate_link_label`, `src/vhost.rs` `extract_subdomain`. WRITE `src/lib.rs` (aggiungere `pub mod fast_link;` in ordine alfabetico, senza cfg). NEW `src/fast_link/mod.rs`, `src/fast_link/request.rs`, `src/fast_link/response.rs`.
- **Change:**
  Preconditions: G-BASE verde.
  Contract (`mod.rs`):
  ```rust
  pub const FAST_LINK_ID_LEN: usize = 16;
  pub const FAST_LINK_ID_ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
  pub const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 3600;
  pub const MAX_WAIT_TIMEOUT_SECS: u64 = 604_800;
  pub const DEFAULT_MAX_ACTIVE: usize = 32;
  pub const MAX_MAX_ACTIVE: usize = 4096;
  pub const REPLAY_WINDOW_BYTES: usize = 4 * 1024 * 1024;
  pub const STALL_TIMEOUT: Duration = Duration::from_secs(600);
  pub const HANDOFF_RECV_TIMEOUT: Duration = Duration::from_secs(5);
  pub const MAX_HEAD_BYTES: usize = 16 * 1024;
  pub const MAX_HEADERS: usize = 64;
  pub const LINGER_TIMEOUT: Duration = Duration::from_secs(2);
  pub const LINGER_MAX_BYTES: usize = 1024 * 1024;
  pub const DEFAULT_UPLOAD_FILENAME: &str = "upload.bin";

  pub struct FastLinkServerArgs { pub enabled: bool, pub vhost: Option<String>, pub auth: Option<String>,
      pub wait_timeout_secs: u64, pub max_active: usize }
  #[derive(Clone)] pub struct FastLinkConfig { pub host: String, pub label: String, pub auth: BasicAuth,
      pub wait_timeout: Duration, pub max_active: usize }
  pub struct FastLinkResolution { pub config: Option<FastLinkConfig>, pub ignored: Vec<&'static str> }
  pub fn resolve_server_config(args: &FastLinkServerArgs, vhost_base_domain: Option<&str>)
      -> anyhow::Result<FastLinkResolution>;
  pub fn generate_id() -> anyhow::Result<String>;
  ```
  `FastLinkConfig` non implementa `Debug` con la credenziale (se serve `Debug`, implementarlo a mano omettendo `auth`).
  Regole `resolve_server_config`:
  - `enabled == false`: `config: None`; `ignored` elenca i nomi flag (`"--fast-link-transfer-vhost"`, `"--fast-link-transfer-auth"`, `"--fast-link-transfer-wait-timeout"`, `"--fast-link-transfer-max-active"`) di quelli impostati (`Some` o diverso dal default). Mai errore.
  - `enabled`: `vhost_base_domain` None o vuoto → Err `"--fast-link-transfer requires a vhost base domain (--vhost-config or --vhost-base-domain)"`. `vhost` None → Err `"--fast-link-transfer requires --fast-link-transfer-vhost (BORE_FAST_LINK_TRANSFER_VHOST)"`. Normalizzazione host: trim, lowercase, togliere un `.` finale; se contiene `:`, `/`, spazio o è vuoto → Err `"--fast-link-transfer-vhost must be a bare host name such as fast.<base domain>"`. `vhost::extract_subdomain(&host, base)` deve essere `Some(label)` → altrimenti Err `"--fast-link-transfer-vhost '<host>' must be exactly one label under the vhost base domain '<base>' (for example fast.<base>) so the existing wildcard certificate covers it"`. `auth` None → Err `"--fast-link-transfer requires --fast-link-transfer-auth USER:PASS (BORE_FAST_LINK_TRANSFER_AUTH)"`; `split_once(':')` con user o pass vuoti → Err `"--fast-link-transfer-auth must be USER:PASS with a non-empty user and password"` (il messaggio non contiene mai il valore). `wait_timeout_secs` fuori da `1..=MAX_WAIT_TIMEOUT_SECS` → Err; `max_active` fuori da `1..=MAX_MAX_ACTIVE` → Err.
  `generate_id`: `ring::rand::SystemRandom`, riempire byte singoli, scartare `>= 252`, `ALPHABET[b % 36]`, fino a 16 char.
  Contract (`request.rs`):
  ```rust
  pub struct RequestHead<'a> { pub method: &'a str, pub target: &'a str, headers: Vec<(&'a str, &'a str)> }
  impl<'a> RequestHead<'a> { pub fn header(&self, name: &str) -> Option<&'a str>; pub fn header_count(&self, name: &str) -> usize; }
  #[derive(Debug, PartialEq, Eq)] pub enum HeadError { TooLarge, Malformed }
  pub fn head_len(buf: &[u8]) -> Option<usize>;                // indice subito dopo il primo "\r\n\r\n"
  pub fn parse_head(head: &[u8]) -> Result<RequestHead<'_>, HeadError>;
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Framing { ContentLength(u64), Chunked }
  pub fn upload_framing(h: &RequestHead) -> Result<Framing, u16>;   // Err = status HTTP
  pub fn expects_continue(h: &RequestHead) -> Result<bool, u16>;
  pub fn parse_upload_target(target: &str) -> Result<String, u16>;
  pub fn parse_download_target(target: &str) -> Option<String>;
  #[derive(Debug, PartialEq, Eq)] pub enum Preview { Bot, Range }
  pub fn preview_verdict(h: &RequestHead) -> Option<Preview>;
  pub fn host_matches(host_header: &str, configured: &str) -> bool;
  ```
  Regole: `parse_head` richiede UTF-8 (altrimenti `Malformed`), request line `METHOD SP TARGET SP HTTP/1.1|HTTP/1.0` (tre token separati da un solo spazio), righe header `nome:valore` con nome non vuoto senza spazi (spazio prima di `:` → `Malformed`), riga che inizia con SP/HT (obs-fold) → `Malformed`, più di `MAX_HEADERS` header → `Malformed`. Il valore è `trim()`. La head passata termina con `\r\n\r\n`; `head_len` None su un buffer di `MAX_HEAD_BYTES` → il chiamante risponde 431.
  `upload_framing`: `Transfer-Encoding` presente e `Content-Length` presente → `Err(400)`; TE: lista separata da virgole, trim, lowercase, deve essere esattamente `["chunked"]` → `Chunked`, altrimenti `Err(501)`; più di un header TE → `Err(400)`. CL: più di un header → `Err(400)`; valore solo cifre ASCII, 1..=20 caratteri, `parse::<u64>` ok → `ContentLength(n)`, altrimenti `Err(400)`. Nessuno dei due → `Err(411)`.
  `expects_continue`: assente → `Ok(false)`; `eq_ignore_ascii_case("100-continue")` dopo trim → `Ok(true)`; altro → `Err(417)`.
  `parse_upload_target`: deve iniziare con `/`; contiene `?` o `#` → 400; `"/"` → `DEFAULT_UPLOAD_FILENAME`; resto dopo il primo `/` contiene `/` → 400; percent-decode (`%` seguito da 2 hex obbligatori, altrimenti 400; `+` resta `+`), risultato UTF-8 valido altrimenti 400; `transfer_link::validate_filename` Err → 400.
  `parse_download_target`: tagliare da `?` in poi; deve iniziare con `/`; primo segmento = fino al prossimo `/` o fine; `Some(id)` sse lunghezza 16 e tutti i byte in `FAST_LINK_ID_ALPHABET`.
  `preview_verdict`: UA lowercase contiene una di `BOT_UA_MARKERS` → `Some(Bot)`; altrimenti `Range` presente e il valore (trim, lowercase, spazi rimossi) diverso da `bytes=0-` → `Some(Range)`; altrimenti None.
  `const BOT_UA_MARKERS: &[&str] = &["slackbot", "slack-imgproxy", "discordbot", "telegrambot", "whatsapp", "facebookexternalhit", "facebot", "meta-externalagent", "twitterbot", "linkedinbot", "skypeuripreview", "mattermost-bot", "googlebot", "bingbot", "applebot", "embedly", "iframely", "redditbot", "pinterestbot", "bitlybot", "vkshare"];` — mai marker generici come `"bot"` (es. UA di telefoni "Cubot").
  `host_matches`: togliere `:<cifre>` finale se presente, lowercase, togliere `.` finale, confronto esatto con `configured`.
  Contract (`response.rs`, byte puri + due helper async):
  ```rust
  pub fn simple_response(status: u16, content_type: &str, body: &[u8], extra: &[(&str, &str)]) -> Vec<u8>;
  pub fn upload_head() -> &'static [u8];
  pub fn download_head(filename: &str, length: Option<u64>) -> Result<Vec<u8>, FilenameError>;
  pub fn chunk(payload: &[u8]) -> Vec<u8>;              // "{:x}\r\n{payload}\r\n"; payload vuoto vietato (debug_assert)
  pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";
  pub const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";
  pub fn usage_text(authority: &str) -> String;
  pub const PREVIEW_HTML: &[u8];                         // <!doctype html><title>bore fast link</title>... nessun nome file
  pub async fn linger_close<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S);
  pub async fn abort_close<S: AsyncWrite + Unpin>(stream: &mut S);
  ```
  `simple_response`: `HTTP/1.1 <status> <reason>` (reason per 200,308,400,401,403,404,405,409,411,416,417,431,501,503; altrimenti "Error"), poi `Content-Type`, `Content-Length: body.len()`, `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `extra`, `Connection: close`, riga vuota, body. `download_head` come nell'esempio sopra (CL se `Some`, altrimenti `Transfer-Encoding: chunked`). `usage_text` elenca: `curl -u USER:PASS -T file.tar https://<authority>`, `tar -cpf - dir | curl -N -u USER:PASS -T - https://<authority>/dir.tar`, "open the printed link once: curl -fO, wget or a browser", "nothing is stored on the server". `linger_close`: flush, `shutdown()` in `timeout(LINGER_TIMEOUT)`, poi leggere e scartare fino a `LINGER_MAX_BYTES` o `LINGER_TIMEOUT` o EOF, ignorare errori. `abort_close`: flush + `shutdown()` in `timeout(LINGER_TIMEOUT)`, ignorare errori (usato per chiudere SENZA terminatore).
  Steps:
  1. S1 — crea i tre file NEW e `pub mod fast_link;` in `lib.rs`; `mod.rs` dichiara `mod request; mod response;` (pub(crate) o pub use dei simboli sopra) e le costanti; expected `cargo check --all-features` ok.
  2. S2 — implementa `resolve_server_config`, `generate_id`, tutto `request.rs`, tutto `response.rs` con doc comment brevi sui simboli pub; expected unit test verdi.
  Recovery boundary: none.
  Failure handling: una regola non riproducibile coi tipi indicati → agent-1:opus; mai inventare stati HTTP diversi.
- **Unit tests:** in `mod tests` di ciascun file NEW (G-U0):
  - `resolve_disabled_ignores_and_lists_flags` (disabled + vhost/auth/timeout impostati → `config None`, `ignored` = i 3 nomi); `resolve_enabled_requires_each_input` (una asserzione per ogni Err: base mancante, vhost mancante, auth mancante, auth `":p"`, `"u:"`, `"up"`, timeout 0 e 604801, max_active 0 e 4097); `resolve_rejects_host_not_one_label_under_base` (`a.b.bore.tld`, `bore.tld`, `fast.other.tld`, `fast.bore.tld:443`, `https://fast.bore.tld`); `resolve_normalizes_host` (`" FAST.Bore.TLD. "` → host `fast.bore.tld`, label `fast`); `resolve_error_never_echoes_the_password`.
  - `generate_id_is_16_alphabet_chars_and_varies` (100 ID tutti validi, almeno 99 distinti).
  - `parse_head_*`: valido; obs-fold; spazio prima di `:`; 65 header; non UTF-8; versione `HTTP/2`.
  - `upload_framing_table` (CL ok, CL doppio, CL non numerico, CL 21 cifre, TE chunked, TE `gzip, chunked` → 501, TE+CL → 400, nessuno → 411); `expects_continue_table`.
  - `upload_target_table` (`/`→`upload.bin`, `/miofile.tar`, `/my%20file.tar`→`my file.tar`, `/a/b` 400, `/x?y` 400, `/%zz` 400, `/%c3%28` 400, `/..` 400, `/%2F` 400).
  - `download_target_table` (id valido con e senza `/nome` e `?q`; 15/17 char; maiuscole; carattere fuori alfabeto → None).
  - `preview_table` (UA Slackbot da R4 → Bot; `Mozilla/5.0 ... Cubot` → None; `curl/8.5.0` + `Range: bytes=0-` → None; `Range: bytes=0-1023` → Range; `Range: bytes=10-` → Range).
  - `host_matches_table` (`fast.bore.tld:8443`, `FAST.bore.tld`, `fast.bore.tld.`, `xfast.bore.tld` false, `fast.bore.tld:abc` false).
  - `response_bytes_exact` (asserzione byte-per-byte di `simple_response(404,..)`, `download_head` CL e chunked, `chunk(b"ab")` = `b"2\r\nab\r\n"`).
- **e2e tests:** N/A — nessun comportamento osservabile, modulo non collegato.
- **Done:** file NEW presenti ed esportati; G-U0 verde con i test sopra eseguiti (il conteggio include i nomi elencati); review agent-1:opus registrata; unità chiusa in STATE.md; commit di completamento (full-autonomous).

### 0.2 `BodyFramer` (Content-Length e chunked passthrough)
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus dopo il diff (grammatica chunked, overflow, split point).
- **Files:** READ `src/fast_link/request.rs` (`Framing`). NEW `src/fast_link/framing.rs` (dichiarato in `mod.rs`).
- **Change:**
  Preconditions: 0.1 DONE.
  Contract:
  ```rust
  #[derive(Debug, PartialEq, Eq)] pub enum FramingError { ChunkSize, ChunkLineTooLong, MissingCrlf, TrailersNotSupported }
  pub struct Progress { pub forward: usize, pub done: bool }
  pub struct BodyFramer { /* stato privato */ }
  impl BodyFramer {
      pub fn new(framing: Framing) -> Self;
      pub fn feed(&mut self, input: &[u8]) -> Result<Progress, FramingError>;
      pub fn is_done(&self) -> bool;
      pub fn is_chunked(&self) -> bool;
      pub fn declared_length(&self) -> Option<u64>;
  }
  pub const MAX_CHUNK_LINE: usize = 4096;
  ```
  Semantica: `forward` è SEMPRE un prefisso di `input` (i byte da inoltrare verbatim); `forward < input.len()` solo quando `done` (byte in eccesso dopo la fine del body, da ignorare). Dopo `done`, `feed` restituisce `forward 0, done true`. Dopo un `Err` il framer resta in errore (ogni `feed` successivo → stesso `Err`).
  CL: `remaining` iniziale = n; `forward = min(remaining, input.len())`; `done` quando `remaining == 0`; `new(ContentLength(0)).is_done() == true`.
  Chunked, macchina a stati (byte per byte nelle righe, salto in blocco nei dati):
  - `SizeDigits{value,digits,line_len}`: HEXDIG → `value = value*16 + d` (checked; overflow o più di 16 cifre → `ChunkSize`); primo byte non HEXDIG con `digits == 0` → `ChunkSize`; con `digits > 0`: `;` o SP/HT → `Ext`; CR → `SizeLf`; altro → `ChunkSize`.
  - `Ext{value,line_len}`: ogni byte tranne CR/LF accettato (byte `< 0x20` diversi da HT → `ChunkSize`); CR → `SizeLf`; LF → `MissingCrlf`.
  - `line_len` conta dall'inizio della riga; `> MAX_CHUNK_LINE` → `ChunkLineTooLong`.
  - `SizeLf`: LF → se `value == 0` → `TrailerStart`, altrimenti `Data{remaining: value}`; altro → `MissingCrlf`.
  - `Data{remaining}`: consuma `min(remaining, avail)` in blocco; a 0 → `DataCr`.
  - `DataCr`: CR → `DataLf`; altro → `MissingCrlf`. `DataLf`: LF → `SizeDigits` nuova riga; altro → `MissingCrlf`.
  - `TrailerStart`: CR → `TrailerLf`; altro → `TrailersNotSupported`. `TrailerLf`: LF → `Done`; altro → `MissingCrlf`.
  Tutti i byte fino a `Done` incluso sono `forward` (passthrough verbatim, incluso `0\r\n\r\n`).
  Steps:
  1. S1 — implementa `framing.rs` e il `mod framing;` + re-export; expected compila.
  2. S2 — test sotto; expected verdi.
  Recovery boundary: none.
  Failure handling: ambiguità grammaticale → agent-1:opus; mai accettare LF nudo.
- **Unit tests:** `mod tests` in `framing.rs` (G-U0):
  - `cl_forwards_exactly_the_declared_length_and_reports_excess` (CL 10 con input 25 → forward 10, done).
  - `cl_zero_is_done_at_construction`.
  - `chunked_every_split_point_matches_a_whole_feed` (body `"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\n\r\n"`: per ogni coppia di split `(i,j)` alimentare 3 pezzi; somma `forward` = lunghezza totale, `done` all'ultimo pezzo, byte inoltrati == input).
  - `chunked_random_sizes_roundtrip` (seed fisso, 200 body con chunk casuali 1..70000 byte, spezzati a caso; il decoder di riferimento nel test ricostruisce il payload uguale all'originale; nessun `Err`).
  - `chunked_rejects` (una asserzione per: `"g\r\n"`, `"\r\n"` senza cifre, 17 cifre, overflow `ffffffffffffffff1`, LF nudo dopo size, dati non seguiti da CRLF, trailer `"0\r\nX: y\r\n\r\n"` → `TrailersNotSupported`, riga ext di 5000 byte → `ChunkLineTooLong`).
  - `chunked_excess_after_done_is_not_forwarded`.
  - `error_is_sticky`.
- **e2e tests:** N/A — codice puro.
- **Done:** test verdi in G-U0; review agent-1:opus registrata; unità chiusa; commit di completamento.


### 0.3 Pompa di streaming a due task (percorso dati, banda massima)
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus dopo il diff (focus D8/I-6/I-11: parallelismo reale, zero allocazioni a regime, cancellazione, nessun terminatore su abort).
- **Files:** READ `src/mux.rs` `Transport`, `src/shared.rs` `proxy_buffer_size`, `src/vhost.rs` mock `FlushGatedWriter` in `mod tests`, `tokio_util::sync::CancellationToken` (dipendenza già presente, usata in `src/client.rs`). NEW `src/fast_link/pump.rs` (dichiarato `mod pump;` in `mod.rs`, API `pub(crate)`).
- **Change:**
  Preconditions: 0.2 DONE.
  Perché (D8): con una task sola decifratura TLS dell'uploader e cifratura TLS del downloader stanno sullo stesso core e la banda massima è ~metà di quella a due core; il riciclo di buffer fissi evita malloc/munmap di blocchi da 256 KiB (soglia mmap di glibc: trappola già misurata nel progetto, H-18).
  Contract:
  ```rust
  pub(crate) const PUMP_DEPTH: usize = 4;          // buffer in volo tra lettore e scrittore
  pub(crate) struct PumpState { pub framer: BodyFramer, pub replay: Option<Vec<u8>>, pub consumed: u64 }
  pub(crate) struct PumpConfig { pub buffer: usize, pub depth: usize, pub stall: Duration, pub replay_window: usize }
  #[derive(Clone)] pub(crate) struct PumpCounters { pub total_rx: Arc<AtomicU64>, pub total_tx: Arc<AtomicU64>, pub bytes_total: Arc<AtomicU64> }
  #[derive(Debug, PartialEq, Eq)] pub(crate) enum PumpEnd { Completed, DownloaderGone, UploaderFailed(&'static str) }
  pub(crate) struct PumpResult<S> { pub uploader: S, pub state: PumpState, pub end: PumpEnd, pub written: u64 }
  pub(crate) async fn pump<S: crate::mux::Transport>(uploader: S, downloader: Box<dyn crate::mux::Transport>,
      state: PumpState, cfg: PumpConfig, counters: PumpCounters) -> PumpResult<S>;
  ```
  Semantica di `PumpEnd`: `Completed` = framer `done`, tutti i byte scritti sul downloader, `flush` e `shutdown` (bounded `LINGER_TIMEOUT`) eseguiti. `DownloaderGone` = write/flush sul downloader fallita o in stallo; il downloader è stato droppato; l'uploader è restituito intatto con `state` aggiornato (ogni byte prelevato è contato in `consumed` e, se `replay` è ancora `Some`, contenuto in `replay`). `UploaderFailed(r)` = EOF prematuro (`"upload ended before the body was complete"`), errore di read, stallo (`"upload stalled"`) o `FramingError` (`"malformed upload body"`); il downloader è stato chiuso con `abort_close` SENZA scrivere altro (il client vede CL corto o chunked senza last-chunk, I-5).
  Algoritmo:
  - Canali: `full: mpsc::channel::<(Vec<u8>, usize)>(depth)`, `free: mpsc::channel::<Vec<u8>>(depth)`; pre-caricare `free` con `depth` buffer `vec![0u8; buffer]` (unica allocazione di buffer della pompa). `cancel = CancellationToken::new()`.
  - Task lettore R (`tokio::spawn`, possiede `uploader` e `state`): loop — se `state.framer.is_done()` → `break Ok`. `buf = select!{biased; _ = cancel.cancelled() => break Err(Cancelled), b = free_rx.recv() => b}` (`None` impossibile). `n = select!{biased; _ = cancel.cancelled() => { rimetti buf; break Err(Cancelled) }, r = timeout(stall, uploader.read(&mut buf[..])) => ...}`: timeout → errore `"upload stalled"`; `Ok(Ok(0))`/`Ok(Err)` → `"upload ended before the body was complete"`. `total_rx += n`. `p = state.framer.feed(&buf[..n])` (Err → `"malformed upload body"`). Replay: se `Some(r)` e `consumed + forward <= replay_window` → `r.extend_from_slice(&buf[..forward])`, altrimenti `replay = None`. `consumed += forward`. Se `forward > 0` → `select!{biased; cancel → break Err(Cancelled), res = full_tx.send((buf, forward)) => res.is_err() → break Err(Cancelled)}`; se `forward == 0` → rimetti `buf` in una variabile locale riusata al giro successivo invece di `free_rx.recv()`. **In ogni ramo d'errore NON-cancellazione R chiama `cancel.cancel()` PRIMA di droppare `full_tx`** (così W abortisce invece di chiudere pulito). R restituisce `(uploader, state, Result<(), ReaderError>)`.
  - Task scrittore W (`tokio::spawn`, possiede `downloader`): loop — `m = select!{biased; _ = cancel.cancelled() => { abort_close(&mut downloader).await; return Err(Aborted) }, m = full_rx.recv() => m}`. `None` → `flush` + `timeout(LINGER_TIMEOUT, shutdown())` → `return Ok(written)`. `Some((buf, len))` → `timeout(stall, async { downloader.write_all(&buf[..len]).await?; downloader.flush().await })` → Err/timeout → `return Err(Gone)` (drop del downloader); ok → `written += len`, `total_tx += len`, `bytes_total += len`, `let _ = free_tx.try_send(buf)`.
  - Coordinatore (nella task chiamante): `select!` su `&mut w_handle` e `&mut r_handle`:
    - W `Ok(written)` → `r = r_handle.await`; R `Ok` → `Completed`; R `Err(reason)` → `UploaderFailed(reason)` (caso limite: W ha visto `None` prima del cancel; con CL/chunked il troncamento resta visibile).
    - W `Err(Gone)` → `cancel.cancel()`; `r = r_handle.await` → `DownloaderGone` (R `Ok` incluso: body finito ma downloader caduto).
    - W `Err(Aborted)` → `r = r_handle.await` → `UploaderFailed(reason di R)`.
    - R finisce per primo: `Ok` → `w = w_handle.await` → `Ok` → `Completed`, `Err(Gone)` → `DownloaderGone`; `Err(reason)` → `w_handle.await` (abortirà) → `UploaderFailed(reason)`.
    - `JoinError` con panic → `std::panic::resume_unwind(e.into_panic())` (la task di connessione muore, lo `SlotGuard` pulisce); la pompa non usa mai `abort()`.
  - Nessun `Mutex` e nessuno stato condiviso tra R e W oltre ai canali, al token e ai contatori atomici `Relaxed`. Uploader e downloader non sono mai divisi (`split`) né condivisi: ciascuno appartiene a una sola task (I-6: W fa `flush` dopo ogni `write_all`).
  Steps:
  1. S1 — `pump.rs` con tipi, R, W, coordinatore; expected compila.
  2. S2 — test sotto; expected verdi; red-check di S13-P (rimuovere il `flush` in W → il test fallisce; ripristinare).
  Recovery boundary: none.
  Failure handling: test instabili per tempi → `tokio::time::pause` o sincronizzazione esplicita; due tentativi falliti → agent-1:opus.
- **Unit tests:** `mod tests` in `pump.rs` (G-U0), `tokio::io::duplex(1 MiB)` per entrambi i lati, runtime `flavor = "multi_thread", worker_threads = 2` dove indicato:
  - `pump_completes_cl_body_byte_exact` — CL 32 MiB pseudo-random, `buffer` 256 KiB: `Completed`, `written == 32 MiB`, byte identici, `bytes_total` e `total_tx` = 32 MiB.
  - `pump_completes_chunked_passthrough` — body chunked casuale: byte al downloader == byte chunked inviati (incluso `0\r\n\r\n`).
  - `pump_reuses_its_buffers` — 64 MiB con `depth 4`: contatore di allocazioni NON disponibile → verificare invece che `free_rx` riceva sempre gli stessi `depth` buffer (test-only: `PumpConfig` con hook `#[cfg(test)]` che registra `buf.as_ptr()`; l'insieme dei puntatori visti ha cardinalità ≤ `depth`).
  - `pump_downloader_drop_returns_uploader_and_state` — finestra 1 MiB, downloader chiude dopo 100 KiB: `DownloaderGone`, `state.replay.is_some()`, `replay.len() == consumed`, l'uploader restituito è ancora leggibile (i byte successivi arrivano).
  - `pump_downloader_drop_past_window_clears_replay` — finestra 64 KiB, chiusura dopo 1 MiB: `DownloaderGone`, `replay.is_none()`.
  - `pump_uploader_eof_aborts_without_completion` — CL 1 MiB, uploader invia 300 KiB poi chiude: `UploaderFailed("upload ended before the body was complete")`; il downloader legge < 1 MiB e poi EOF.
  - `pump_stall_is_bounded` — `start_paused`, stall 5 s, uploader fermo: dopo `advance(6 s)` → `UploaderFailed("upload stalled")`.
  - `pump_writes_are_flushed_before_waiting` (S13-P) — downloader = `FlushGatedWriter`; uploader invia 100 KiB e si ferma: entro 1 s (tempo reale, `timeout`) il downloader vede 100 KiB. Red-check documentato.
  - `pump_uses_two_worker_threads` (multi_thread 2): R e W registrano `std::thread::current().id()` al primo giro in un hook `#[cfg(test)]`; il test NON asserisce id diversi (lo scheduler non lo garantisce) ma asserisce che entrambe le task sono state spawnate (hook chiamato 2 volte) — il parallelismo reale è misurato da T-FL-PERF.
- **e2e tests:** N/A — coperti da T-FL-PERF (2.2).
- **Done:** G-U0 verde con i test sopra; red-check S13-P registrato; review agent-1:opus; unità chiusa; commit di completamento.

### 0.4 Sessione: slot, dispatch, upload, download, handoff, re-arm, scadenza, metriche
- **Model:** agent-2:sonnet
- **Assignment:** implementa; review agent-1:opus dopo il diff (focus D13, I-3, I-4, I-7, I-8; nessun lock attraverso `.await`).
- **Files:** READ `src/fast_link/pump.rs`, `src/basicauth.rs` `UNAUTHORIZED`. WRITE `src/fast_link/mod.rs`. NEW `src/fast_link/session.rs`.
- **Change:**
  Preconditions: 0.1, 0.2, 0.3 DONE.
  Contract (in `mod.rs`, implementazione in `session.rs`):
  ```rust
  pub struct FastLink { /* config, slots: DashMap<String, Arc<Slot>>, active: Arc<Semaphore>,
      metrics: FastLinkMetrics, total_rx: Arc<AtomicU64>, total_tx: Arc<AtomicU64>,
      wait_timeout: Duration, stall_timeout: Duration, replay_window: usize */ }
  impl FastLink {
      pub fn new(config: FastLinkConfig, total_rx: Arc<AtomicU64>, total_tx: Arc<AtomicU64>) -> Self;
      pub fn host(&self) -> &str;  pub fn label(&self) -> &str;
      pub fn matches_host(&self, host_header: &str) -> bool;          // request::host_matches
      pub async fn serve<S: crate::mux::Transport>(self: &Arc<Self>, stream: S, buffered: Vec<u8>,
          peer: Option<SocketAddr>, secure: bool, permit: Option<OwnedSemaphorePermit>);
      pub fn config_view(&self) -> FastLinkConfigView;
      pub fn metrics_view(&self) -> FastLinkMetricsView;
      #[doc(hidden)] pub fn set_timeouts_for_test(&mut self, wait: Duration, stall: Duration);
      #[doc(hidden)] pub fn set_replay_window_for_test(&mut self, bytes: usize);
      #[doc(hidden)] pub fn slots_len(&self) -> usize;
  }
  #[derive(Serialize, Clone, Debug, PartialEq)] pub struct FastLinkConfigView { pub host: String,
      pub wait_timeout_seconds: u64, pub max_active: u64, pub replay_window_bytes: u64 }
  #[derive(Serialize, Clone, Debug, PartialEq)] pub struct FastLinkMetricsView { pub waiting: u64,
      pub streaming: u64, pub uploads_total: u64, pub completed_total: u64, pub failed_total: u64,
      pub expired_total: u64, pub rearmed_total: u64, pub previews_blocked_total: u64,
      pub auth_failures_total: u64, pub rejected_busy_total: u64, pub bytes_total: u64 }
  ```
  `admin_views::ConfigView`/`MetricsView` derivano solo `Serialize, Clone`: le viste fast link derivano `Serialize, Clone, Debug, PartialEq` (nessun `Deserialize`). Mai credenziali nelle viste. `bytes_total` è un `Arc<AtomicU64>` condiviso con `PumpCounters`.
  Stato interno (`session.rs`):
  ```rust
  struct Slot { filename: String, length: Option<u64>, state: std::sync::Mutex<SlotState>,
                handoff: mpsc::Sender<Handoff> }
  enum SlotState { Waiting, Streaming, Closed }
  struct Handoff { stream: Box<dyn crate::mux::Transport>, permit: Option<OwnedSemaphorePermit>,
                   peer: Option<SocketAddr> }
  enum Taken { Closed, InFlight }
  ```
  Regole di concorrenza (D13): il `Mutex` di stato è tenuto solo per leggere/scrivere lo stato, MAI attraverso un `.await`; il guard `DashMap` è rilasciato prima di qualsiasi `.await` (`slots.get(id).map(|e| Arc::clone(e.value()))`). Transizioni: D `Waiting→Streaming`; U `Streaming→Waiting` (re-arm), `Waiting→Closed`, `Streaming→Closed`. Un unico helper `transition(slot, to)` aggiorna i gauge `waiting`/`streaming` (decrementa l'uscente, incrementa l'entrante; `Closed` senza gauge). `SlotGuard` (posseduto da U), nel `Drop`: se lo stato non è `Closed` → `transition(Closed)`; `slots.remove_if(id, |_, v| Arc::ptr_eq(v, &slot))`.
  `serve` (dispatch):
  1. `head_len(&buffered)` None → 431 se `buffered.len() >= MAX_HEAD_BYTES`, altrimenti 400; `simple_response` + `linger_close`; return. `parse_head` Err → 400 + linger.
  2. `authority` = host CONFIGURATO (`config.host`) più `:<porta>` solo se l'header Host termina con `:` seguito da ≥ 1 cifra ASCII (rev 2: il link stampato non deve mai riflettere byte arbitrari dell'header). Il chiamante ha già verificato `matches_host`.
  3. `!secure`: `GET`/`HEAD` → 308 con `Location: https://<authority><target>`; altrimenti 403 body `"fast link transfer requires HTTPS\n"`; linger; return.
  4. `PUT` → `serve_upload`; `GET`/`HEAD` con path `/` (query tolta) → 200 `usage_text(authority)` (HEAD: stessi header, niente body); altri `GET`/`HEAD` → `serve_download`; altri metodi → 405 con `Allow: GET, HEAD, PUT`.
  `serve_upload` (task U, possiede l'uploader e il permit `--max-conns` ricevuto):
  1. `framing = upload_framing` (Err(s) → s + linger); `expect = expects_continue` (Err 417); `filename = parse_upload_target` (Err 400).
  2. Auth SOLO su `&buffered[..head_len]`: `!config.auth.authorized(head)` → `auth_failures_total += 1`, scrivi `basicauth::UNAUTHORIZED`, `linger_close`, return. Mai `100` prima di qui (I-7).
  3. `active_permit = active.clone().try_acquire_owned()` → Err → `rejected_busy_total += 1`, 503 body `"fast link busy: too many active uploads, retry later\n"`, extra `Retry-After: 30`, linger, return.
  4. Loop: `id = generate_id()`; `slots.entry(id)` vacante → inserisci `Arc<Slot>` (`Waiting`, `mpsc::channel(1)`, U tiene `rx`); occupato → rigenera. `transition` iniziale conta `waiting += 1`; `uploads_total += 1`; crea `SlotGuard`.
  5. Scrivi `CONTINUE` se `expect`, `upload_head()`, `chunk(link + "\n")` con `link = format!("https://{authority}/{id}/{}", encode_path_segment(&filename))`, `chunk("# waiting for the download (expires in {m} min); nothing is stored on the server\n")` (`m = ceil(wait_timeout / 60)`), `flush`. Errore → return (guard pulisce).
  6. `state = PumpState { framer: BodyFramer::new(framing), replay: Some(Vec::new()), consumed: 0 }`; alimenta `&buffered[head_len..]` (`feed`, estendi replay, `consumed`; `FramingError` → `fail("malformed upload body")`). `buf = vec![0u8; proxy_buffer_size()]` solo per la fase di attesa. `deadline = Instant::now() + wait_timeout`.
  7. Loop attesa (`tokio::select! { biased; ... }`):
     - `h = rx.recv()` → `Some(h)` → streaming(h).
     - `sleep_until(deadline)` → `take_or_close`: `Closed` → `expired_total += 1`, scrivi `chunk("# expired: nobody downloaded the link within {m} min\n")`, `abort_close(uploader)`, return. `InFlight` → `timeout(HANDOFF_RECV_TIMEOUT, rx.recv())` → `Some(h)` → streaming(h); altrimenti come `Closed`.
     - prefill: `uploader.read(&mut buf[..min(buf.len(), replay_window - replay_len)])`, abilitato solo se `!framer.is_done() && replay_len < replay_window` → `Ok(0)`/`Err` → `take_or_close`; `InFlight` → ricevi l'handoff (timeout come sopra) e rispondi al suo stream `simple_response(404, ..)` + `abort_close`; `failed_total += 1`; return. `Ok(n)` → `total_rx += n`; `feed` (Err → fail); estendi replay; `consumed += forward`.
     `take_or_close(slot) -> Taken`: lock; `Waiting` → `transition(Closed)` → `Closed`; `Streaming` → `InFlight`; `Closed` → `Closed`.
  8. streaming(h) (stato già `Streaming`): `t0 = Instant::now()`; scrivi su `h.stream` `download_head(filename, length)` e poi `replay` (se non vuoto), `flush` — errore → re-arm (punto 9). Scrivi all'uploader `chunk("# download started\n")` + flush — errore → `fail`. Poi `result = pump(uploader, h.stream, state, PumpConfig { buffer: proxy_buffer_size(), depth: PUMP_DEPTH, stall: stall_timeout, replay_window }, counters)`; `uploader = result.uploader; state = result.state`; il permit `h.permit` è droppato qui (fine connessione downloader).
     - `Completed` → `completed_total += 1`; `transition(Closed)`; scrivi all'uploader `chunk("# done: {consumed} bytes in {secs:.1} s ({mib_s:.1} MiB/s)\n")` + `LAST_CHUNK` + flush + `timeout(LINGER_TIMEOUT, shutdown())`; return.
     - `DownloaderGone` → punto 9.
     - `UploaderFailed(r)` → `fail(r)`.
  9. re-arm: se `state.replay.is_some()` (TUTTI i byte prelevati sono ancora in RAM, I-3) → `transition(Waiting)`, `rearmed_total += 1`, scrivi all'uploader `chunk("# download interrupted before the first 4 MiB; the link is still valid, waiting again\n")` + flush (errore → fail); torna al punto 7 con la STESSA `deadline`. Altrimenti `fail("download interrupted after {consumed} bytes; a stream cannot be replayed")`.
  `fail(reason)`: `failed_total += 1`; best-effort `timeout(LINGER_TIMEOUT, write chunk("# failed: {reason}\n") + flush)`; `abort_close(uploader)` SENZA `LAST_CHUNK` (curl esce 18, I-4); return (guard pulisce).
  `serve_download` (task D):
  1. `id = parse_download_target` → None → 404 body `"unknown or expired link\n"`, linger, return.
  2. `slot` = lookup (clone Arc, guard rilasciato) → None → 404.
  3. `HEAD` → `download_head(filename, length)`, shutdown, return (mai consumante).
  4. `preview_verdict`: `Bot` → `previews_blocked_total += 1`, 200 `text/html; charset=utf-8` `PREVIEW_HTML`; `Range` → `previews_blocked_total += 1`, 416 (extra `Content-Range: bytes */<len>` solo se `length` noto); return.
  5. Claim sotto lock: `Waiting` → `transition(Streaming)`; `Streaming` → 409 `"download already in progress\n"`; `Closed` → 404.
  6. `slot.handoff.try_send(Handoff { stream: Box::new(stream), permit, peer })` → Ok → return. `Err(Full(h)|Closed(h))` (impossibile col protocollo) → `debug_assert!(false)`, 503 su `h.stream`, return.
  Log `info!` a fine upload con `id_prefix = &id[..4]`, esito, byte, durata, peer; `debug!` per preview/409/404. Mai l'ID completo, mai Authorization (D18).
  Steps:
  1. S1 — tipi, `FastLink::new`, viste, `transition`, `SlotGuard`, `take_or_close`; expected compila.
  2. S2 — `serve`, `serve_download`; expected S4, S5, S11, S15 verdi.
  3. S3 — `serve_upload` completo su `pump`; expected tutti i test verdi.
  Recovery boundary: dopo S2 (checkpoint §6 se la sessione rischia di interrompersi).
  Failure handling: deadlock o dipendenza da tempi reali → `tokio::time::pause`/`advance` o sincronizzazione esplicita; due tentativi falliti → agent-1:opus.
- **Unit tests:** `mod tests` in `session.rs` (G-U0), `tokio::io::duplex(8 MiB)`; helper: `upload_request(framing, name, auth, expect) -> Vec<u8>`, `read_link(&mut client) -> String`, `decode_chunked(bytes) -> (Vec<u8>, bool terminated)`. `FastLink` di test: host `fast.bore.local`, auth `u:p`. Ogni scenario terminato asserisce `slots_len() == 0`, `waiting == 0`, `streaming == 0` (I-8).
  - T-FL-S1 `upload_prints_link_then_streams_to_one_downloader` — CL 10 MiB (seed fisso): link `^https://fast\.bore\.local/[a-z0-9]{16}/file\.bin$`; download 200 + `Content-Length: 10485760` + SHA-256 identico; uploader termina con `# done:` + `0\r\n\r\n`; `completed_total 1`, `bytes_total 10485760`.
  - T-FL-S2 `chunked_upload_is_passed_through_verbatim` — download `Transfer-Encoding: chunked`, nessun CL, byte == chunked inviati, decodifica == payload.
  - T-FL-S3 `auth_failure_answers_401_before_any_continue` — con Expect e senza/errata auth: risposta inizia `HTTP/1.1 401`, niente `100 Continue`; `auth_failures_total 1`.
  - T-FL-S4 `head_and_previews_never_consume` — HEAD (200, CL, disposition), GET UA Slackbot (200 html), GET `Range: bytes=0-1023` (416); poi GET normale completa.
  - T-FL-S5 `second_get_while_streaming_is_409`.
  - T-FL-S6 `a_download_dropped_inside_the_window_rearms` — finestra 64 KiB, CL 1 MiB; downloader 1 legge 10 KiB e chiude; uploader riceve `# download interrupted before the first`; downloader 2 riceve tutto identico; `rearmed_total 1`, `completed_total 1`.
  - T-FL-S7 `a_download_dropped_past_the_window_fails_both` — finestra 64 KiB, chiusura dopo 200 KiB: uploader contiene `# failed:` e NON termina con `0\r\n\r\n`; GET successivo 404; `failed_total 1`.
  - T-FL-S8 `wait_timeout_expires_without_terminator` — `start_paused`, wait 2 s, `advance(3 s)`: `# expired:`, niente terminatore; GET 404; `expired_total 1`.
  - T-FL-S9 `uploader_abort_truncates_the_download` — CL 1 MiB, uploader 100 KiB poi chiude: downloader < 1 MiB poi EOF; `failed_total 1`.
  - T-FL-S10 `max_active_rejects_with_503` — `max_active 1`: secondo upload 503 + `Retry-After`; `rejected_busy_total 1`.
  - T-FL-S11 `plain_connections_are_refused_or_redirected` — `secure=false`: PUT 403, GET 308 `Location: https://fast.bore.local/<target>`.
  - T-FL-S12 `expiry_racing_a_claim_serves_the_claimant` — deterministico: slot portato a `Streaming` e handoff inviato a mano; `take_or_close` → `InFlight`; il ramo di scadenza riceve l'handoff e completa.
  - T-FL-S14 `excess_bytes_after_the_body_are_not_forwarded` — CL 5 + 7 byte extra: download esattamente 5 byte.
  - T-FL-S15 `usage_and_method_table` — `GET /` 200 con `curl -u`; `DELETE /x` 405 con `Allow`; head > 16 KiB → 431.
  - T-FL-S16 `authorization_in_the_body_prefix_is_ignored` — head senza auth, prefisso body con `Authorization: Basic dTpw` → 401.
- **e2e tests:** N/A in questa fase — coperti da 1.3 e 2.1.
- **Done:** G-U0 verde con S1–S16 (S13 è in 0.3); review agent-1:opus (checklist D13, I-3, I-4, I-7, I-8); unità chiusa; commit di completamento.

## Phase gates and closure
- Required gates: G-FMT, G-CLIPPY, G-U0, G-NODEF; assertions: nessun warning; tutti i test `fast_link::` elencati in 0.1–0.4 eseguiti (conteggio ≥ numero elencato).
- README obligation: nessuna sezione cambia (modulo non collegato, nessun comportamento visibile); verificarlo a P0 e registrarlo.
- Ogni sottofase DONE. Aprire P0, eseguire i gate, review agent-1:opus su tutto `src/fast_link/`, registrare, fase DONE, commit di chiusura.
