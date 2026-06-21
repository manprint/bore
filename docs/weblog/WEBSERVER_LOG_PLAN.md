# Webserver Access Logging (vhost + public tunnels) — Design & Implementation Plan

> **Status:** IMPLEMENTED (branch webserver-log). Phases 0–4 + cargo e2e landed.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Target:** nginx-style access logs for **vhost** and **public** tunnels on
> **both client and server**, with size-based rotation + retention, real caller
> IP, correct HTTP-vs-raw handling — and **zero measurable bandwidth impact**
> (the data path must never block or copy extra for logging). Minimize token
> usage during implementation (delegate mechanical sub-phases to Haiku).

---

## 1. Context & problem

`bore` is an async Rust TCP/UDP tunnel. Two tunnel families are in scope:

- **Public tunnel** (`bore local <port>`): server assigns a public port, accepts
  inbound TCP, opens a data substream to the client, client splices to the local
  service. Traffic is opaque (raw TCP, HTTP, or TLS).
- **Vhost** (`bore vhost`): server terminates HTTP/HTTPS on a wildcard domain,
  routes by `Host` subdomain to the right provider, splices. Post-TLS the server
  sees **plaintext** HTTP.

**Single splice machinery on each side:**
- **Client:** `src/client.rs:1037` `handle_connection<S>(stream)` — generic over
  stream type; **both** vhost and public local tunnels (relay yamux substream
  *and* direct QUIC bidi) funnel through it. Connects local at `client.rs:1057`,
  splices at `client.rs:1062` via `copy_bidirectional_with_sizes(&mut local_conn,
  &mut stream, buf, buf)`, `buf = proxy_buffer_size()` (`shared.rs:64`, 256 KiB).
  Consumes the one-byte `STREAM_READY` at `client.rs:1042-1043` before splicing.
- **Server public:** accept loop `server.rs:1547` yields `(stream2, addr)` (real
  caller `SocketAddr`); public port at `server.rs:1424`; writes `STREAM_READY`
  + splices at `server.rs:1626/1629` (direct QUIC) and `server.rs:1677/1682`
  (relay). Registry `public_udp_registry` keyed `"port:{N}"` (`server.rs:179`,
  key built `:1486`).
- **Server vhost:** HTTP accept `server.rs:682` (`_addr` **discarded**), HTTPS
  `server.rs:728` (`_addr` **discarded**, TLS terminated then `handle_https`);
  subdomain via `extract_subdomain` (`vhost.rs:950` HTTP / `:1008` HTTPS), Host
  via `extract_host_from_head` (`vhost.rs:1081`); relay `relay_vhost`
  (`vhost.rs:714`) writes `STREAM_READY` `:756`, splices `:772`. Registry
  `VhostRegistry = Arc<DashMap<String, Arc<VhostEntry>>>` (`vhost.rs:442`),
  `VhostEntry` `vhost.rs:338`.

**What blocks the goal:**
1. The **client never receives the real caller IP** — the server knows it
   (`accept()` addr) but the wire protocol carries only a bare `STREAM_READY`
   byte (`mux.rs:35`, value `0`). No header forwards it.
2. There is **no HTTP-parse crate** and **no rotating-file writer** in the tree.
3. The splice is a tight `copy_bidirectional_with_sizes`; naive logging
   (buffer-to-disk inline, or replacing the splice) would throttle throughput —
   forbidden by the bandwidth invariant.

### Goal

Add three flags — `--webserver-log <dir>`, `--webserver-log-max-files <N>`,
`--webserver-log-max-file-size <MB>` — to **`bore local`, `bore vhost`, and
`bore server`**. When `--webserver-log` is present, every vhost/public HTTP
request transiting that endpoint is appended (nginx-combined format, real caller
IP) to a per-tunnel log file under `<dir>`, with size-based rotation keeping `N`
files of ≤ `MB` each. Raw/TLS (non-parseable) connections get a single
connection-level record. The data path stays byte-for-byte identical when the
flag is absent and suffers no measurable throughput loss when present.

### Reference scenario (final acceptance test)

```
Server:  bore server --vhost-domain bore.example.com --webserver-log /var/log/bore-srv \
                     --webserver-log-max-files 3 --webserver-log-max-file-size 50
Client:  bore vhost --subdomain shop --to <server> --webserver-log /var/log/bore-cli
External caller 203.0.113.7:  GET /api/insert HTTP/1.1  Host: shop.bore.example.com  → 200

EXPECT (client):  /var/log/bore-cli/shop.bore.example.com.log  contains
   203.0.113.7 - - [ts] "GET /api/insert HTTP/1.1" 200 <bytes> "-" "<ua>"
EXPECT (server):  /var/log/bore-srv/shop/shop.bore.example.com.log  contains
   the SAME line with real IP 203.0.113.7  (subdomain-named folder + file)

Public:  bore local 9000 --to <server> --webserver-log /var/log/bore-cli
   HTTP caller → /var/log/bore-cli/9000.log  per-request lines, real IP
   raw TCP / TLS caller → /var/log/bore-cli/9000.log  ONE connection-level line
   Server side → /var/log/bore-srv/9000.log  (flat, real IP from accept addr)

Bandwidth:  iperf/throughput WITH --webserver-log is within 5% of WITHOUT.
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | **Tap-and-forward in place.** A thin `AsyncRead+AsyncWrite` wrapper around the *caller-facing* duplex half inspects bytes **as they already pass**; it never copies payload to a second buffer and never holds the splice. | Single insertion point per side; zero extra payload copy; parser reads the buffer `copy_bidirectional` already moved. |
| **D2** | **Logging is off the hot path:** the tap `try_send`s a finished `AccessRecord` to a **bounded mpsc**; a dedicated writer task drains it. | Disk I/O never touches the data path. |
| **D3** | **Drop-on-full, never block.** If the channel is full (slow disk), the record is dropped and a per-target counter incremented (periodic `warn!`). | Guarantees the bandwidth invariant I-WL2; logging degrades, throughput never does. |
| **D4** | **One writer task + one `RotatingFileWriter` per log target** (per subdomain / per port), lazily created on first record. | Bounded fan-out; per-file rotation; no global lock on the hot path. |
| **D5** | **Custom size-based `RotatingFileWriter`** (rename cascade `x.log`→`x.log.1`→…→`x.log.{N-1}`, drop oldest). No rotation crate added — `tracing-appender` rotates by *time*, not size. | Exact nginx-logrotate semantics; one small self-contained module. |
| **D6** | **Add `httparse` crate** for robust, zero-alloc header parsing; hand-rolled HTTP/1.1 **body-length framing** (Content-Length / chunked) to find keep-alive request/response boundaries. | Safe on malformed input; correct multi-request keep-alive accounting. |
| **D7** | **Real caller IP forwarded to the client via a negotiated extension of the readiness header**: server writes `[STREAM_READY (0x00), ip_len:u8, ip_utf8…]` **only when** the tunnel negotiated logging; else the bare byte. Negotiated by new `TunnelOptions.webserver_log: bool` (`#[serde(default)]`). | Backward-compatible (I-WL4); first byte stays `0x00` so banner-first detection is unaffected (I-WL5). |
| **D8** | **Server reads the real IP directly** from `accept()` (`server.rs:1547` for public; capture the currently-discarded `_addr` at `server.rs:682/:728` for vhost). | No protocol change needed server-side; client side is the only consumer of D7. |
| **D9** | **Raw/TLS detection by sniffing the first request bytes.** TLS handshake (`0x16 0x03 …`) or a first line that is not a valid HTTP request → mark the connection **RAW**: stop HTTP parsing, emit ONE connection-level record (real IP, bytes in/out, duration). | Satisfies "traffic può essere raw oppure http/https" for public tunnels; never tries to parse encrypted bytes. |
| **D10** | **HTTPS vhost is parsed post-TLS-termination** (server already decrypts → plaintext at the tap). **HTTPS over a public tunnel is end-to-end encrypted** → always RAW connection-level (inherent; documented limitation). | Vhost gets full per-request logs even for HTTPS; public-TLS gets connection-level only. |
| **D11** | **File layout.** *Client:* flat — vhost `<fqdn>.log` (e.g. `shop.bore.example.com.log`), public `<port>.log`. *Server:* vhost `<dir>/<subdomain>/<fqdn>.log` (per-subdomain folder), public `<dir>/<port>.log`. Retention flags identical both sides. | Matches the spec's naming/foldering rules verbatim. |
| **D12** | **nginx "combined" line format**, real IP as field 1. Control chars (CR/LF/`"`) in path/UA/referer are escaped. RAW records use a distinct, clearly-marked shape. | Familiar, greppable; log-injection safe. |
| **D13** | **Flag absent ⇒ no wrapper is constructed** — `handle_connection`/`relay_vhost`/public splice take the raw stream exactly as today. | I-WL1 byte-identical legacy path; zero overhead when off. |
| **D14** | **Scope = vhost + public tunnels only.** Secret/proxy tunnels, `transfer`, and `vpn` are **out of scope** (the relay stays AEAD-opaque; no plaintext to log). | No edits to `secret.rs` / `vpn*.rs`; stated as a non-goal. |

---

## 3. Target architecture

### 3.1 New module `src/weblog.rs`

```
AccessLogConfig { dir: PathBuf, max_files: usize, max_file_size_bytes: u64 }
    parsed once from CLI; None ⇒ feature off.

AccessLogger                      // per-endpoint registry (client or server)
    cfg: AccessLogConfig
    targets: DashMap<String, mpsc::Sender<AccessRecord>>   // key = file stem
    fn sender_for(key, path_layout) -> Sender   // lazily spawns writer task

AccessRecord {                    // built by the tap, sent over the channel
    ts, real_ip, method, path, version, status, bytes_sent, referer, ua, kind
}                                 // kind ∈ { Http, Raw }

RotatingFileWriter { path, max_files, max_size, file: BufWriter<File>, written }
    fn write_line(&mut [u8])      // appends; rotates when written + len > max_size

writer_task(rx, path, cfg)        // drains rx, formats, write_line, periodic flush
```

### 3.2 HTTP access tap (the hot-path component)

```
HttpAccessTap<S> { inner: S, real_ip, tx: Sender<AccessRecord>,
                   req: ReqParser, resp: RespParser, mode: Http|Raw, dropped }
impl AsyncRead  for HttpAccessTap   // delegates poll_read to inner, then feeds req-parser the filled bytes
impl AsyncWrite for HttpAccessTap   // delegates poll_write/flush/shutdown, feeds resp-parser the written bytes
```

- Wrap the **caller-facing duplex**: server = inbound public socket / vhost
  `public`; client = the server-facing `stream`. `copy_bidirectional` reading
  *from* this wrapper = **requests**; writing *to* it = **responses**. One
  wrapper observes both directions.
- `ReqParser`: `httparse::Request` on the read side → method/path/version/Host/
  referer/UA; then skip body via Content-Length / `Transfer-Encoding: chunked`
  to find the next request (keep-alive). On any parse error or first-byte sniff
  failing HTTP → flip `mode = Raw` for the connection's life.
- `RespParser`: `httparse::Response` on the write side → status; count body
  bytes (`bytes_sent`). Pair responses to pending requests **FIFO** (HTTP/1.1
  ordering); emit one `AccessRecord` per pair.
- `Raw` mode: count total bytes both ways; on connection close emit ONE record
  (`kind=Raw`).
- **Never** mutates bytes; delegates `poll_shutdown`/`poll_flush` so half-close
  propagates (I-WL3). `try_send` only; drop-on-full (I-WL2).

### 3.3 Data plane (where the wrapper goes)

```
SERVER public  server.rs:1626/1629 (direct) & 1677/1682 (relay):
    edge  ──wrap(HttpAccessTap, real_ip=addr, key="<port>")──▶ copy_bidirectional(tap, stream)

SERVER vhost   vhost.rs:772 (relay_vhost), addr threaded from server.rs:682/728:
    public ──wrap(HttpAccessTap, real_ip=addr, key="<sub>/<fqdn>")──▶ copy_bidirectional(tap, carrier)

CLIENT (both)  client.rs:1062 (handle_connection):
    read forwarded ip header (if webserver_log negotiated, D7)
    stream ──wrap(HttpAccessTap, real_ip=fwd_ip, key="<fqdn>" | "<port>")──▶ copy_bidirectional(local_conn, tap)
```

When the flag is absent, **no wrap** — the existing call is untouched (D13/I-WL1).

### 3.4 Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Single client splice point (both tunnel kinds) | `handle_connection<S>` | `src/client.rs:1037`, splice `:1062` |
| Client STREAM_READY consume point (extend for IP) | readiness read | `src/client.rs:1042-1043` |
| Public-port log key / accept addr | `(stream2, addr)`, `port` | `src/server.rs:1547`, `:1424` |
| Server public splice points to wrap | direct / relay | `src/server.rs:1629`, `:1682` |
| Vhost real IP (currently discarded) | `_addr` from accept | `src/server.rs:682`, `:728` |
| Vhost subdomain + Host for filename | `extract_subdomain`, `extract_host_from_head` | `src/vhost.rs:950/1008`, `:1081` |
| Vhost splice point to wrap | `relay_vhost` splice | `src/vhost.rs:772`, STREAM_READY `:756` |
| Buffer size constant | `proxy_buffer_size()` | `src/shared.rs:64` |
| Readiness marker to extend | `STREAM_READY = 0` | `src/mux.rs:35` |
| Negotiation field home | `TunnelOptions` (serde-default fields) | `src/shared.rs:173-204` (e.g. `udp` `:196`) |
| CLI flag patterns to mirror | `Local`/`Vhost`/`Server` clap structs | `src/main.rs:51-177`, `:276-340`, `:356+` |
| Vhost test harness | `spawn_reg_server`, `spawn_http_stub` `:46`, `spawn_capturing_stub` `:85` | `tests/vhost_test.rs` |
| Public-tunnel test harness | `spawn_server` `:45`, `spawn_client` `:66` | `tests/local_proxy_hardening_test.rs` |
| netns e2e harnesses | vhost / local+proxy | `scripts/vhost_netns_test.sh`, `scripts/local_proxy_netns_test.sh` |
| Bench harnesses | with/without flag | `scripts/vhost_bench.sh`, `scripts/local_bench.sh` |
| README flag sections | Local `:105` (`:131-152`), Vhost `:1129` (`:1200-1213`), Server `:1004` | `README.md` |

---

## 4. New interface (CLI flags)

Added identically to `Local` (`main.rs:51`), `Vhost` (`main.rs:276`), `Server`
(`main.rs:356`) clap structs, mirroring the existing `--carriers`/`--udp` style:

| Flag | Type | Default | Meaning |
|------|------|---------|---------|
| `--webserver-log <DIR>` | `Option<PathBuf>` | `None` (off) | Presence activates logging; logs written under `<DIR>`. |
| `--webserver-log-max-files <N>` | `usize` | `4` | Rotated files retained per target (incl. live file). `N>=1`. |
| `--webserver-log-max-file-size <MB>` | `u64` | `100` | Max size (MiB) before rotation. `>=1`. |

Rules:
- Retention flags **only** meaningful with `--webserver-log`; given without it →
  `warn!` and ignore (mirror the secret-only helper-flag warning convention).
- `--webserver-log-max-files` and the parsed `AccessLogConfig` validated at parse
  time (`N>=1`, `MB>=1`, dir creatable) — error early, before serving.
- **Client side** also sets `TunnelOptions.webserver_log = true` so the server
  forwards the real caller IP (D7); independent of whether the **server** logs.

---

## 5. New protocol / data structures

### 5.1 `TunnelOptions` (additive, backward-compatible)

`src/shared.rs:173-204` — add:
```rust
#[serde(default)]
pub webserver_log: bool,   // client requests real-caller-IP forwarding in the readiness header
```
`#[serde(default)]` ⇒ old client ↔ new server and new client ↔ old server both
deserialize cleanly (I-WL4), exactly like `udp` (`shared.rs:196`).

### 5.2 Readiness header extension (D7)

Sent on every data substream/QUIC-bidi for a tunnel whose negotiated
`webserver_log == true`:
```
legacy (webserver_log=false):   [ 0x00 ]                       // unchanged
extended (webserver_log=true):  [ 0x00, ip_len:u8, ip_utf8... ]   // ip = "203.0.113.7:54321"
```
- First byte stays `STREAM_READY (0x00)` → banner-first detection unaffected
  (I-WL5).
- Server writes the extension at all four STREAM_READY sites
  (`server.rs:1626`, `:1677`; `vhost.rs:756`; public direct + relay).
- Client reads it at `client.rs:1042-1043` **iff** it set `webserver_log`
  (it knows its own option) — so framing is unambiguous without a version field.
- `ip_len == 0` permitted (server couldn't determine IP) → client logs `-`.

**Backward-compat:** flag off ⇒ neither side touches the framing ⇒ byte-identical
to today (I-WL1/I-WL4). Tested by `readiness_legacy_plain`.

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase must pass the gates
(`cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -D
warnings`, `cargo test --all-features`); **zero regressions**; update docs when
behavior/APIs change; **print the model used per sub-task**.

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Scaffolding (pure additive, no behavior change)

> Lands safely alone: flags parse, module exists, nothing is wired into the data
> path yet. Feature stays inert.

#### 0.1 CLI flags + config struct + serde field
- **Model:** Haiku (boilerplate mirroring existing flags).
- **Files:** `src/main.rs:51-177` (Local), `:276-340` (Vhost), `:356+` (Server);
  `src/shared.rs:173-204` (TunnelOptions).
- **Change:** add the three flags (§4) to all three clap structs mirroring
  `--carriers`/`--udp`; add `webserver_log: bool` `#[serde(default)]` to
  `TunnelOptions` (§5.1); add `AccessLogConfig` constructor that validates
  `N>=1, MB>=1` and `warn!`s when retention flags appear without `--webserver-log`.
- **Unit tests:** `tunnel_options_serde_default_webserver_log` (deserialize JSON
  *without* the field → `false`); `access_log_config_rejects_zero` (N=0 / MB=0
  → error); `access_log_config_warns_orphan_retention`.
- **e2e tests:** none (no behavior).
- **Done:** gates green; `bore local/vhost/server --help` shows the flags; all
  existing tests pass unchanged.

#### 0.2 `RotatingFileWriter` (standalone)
- **Model:** Sonnet.
- **Files:** `src/weblog.rs` (new); register `mod weblog;` in `src/main.rs` /
  lib root.
- **Change:** implement D5 — open/append `<stem>.log`, track bytes written, on
  `write_line` that would exceed `max_size` perform the rename cascade
  (`.log.{N-1}` dropped, shift down, current → `.log.1`, fresh `.log`). Create
  parent dirs. `BufWriter` + explicit flush API.
- **Unit tests:** `rotate_cascade_keeps_n` (write past size 3× with N=3 → exactly
  files `.log`,`.log.1`,`.log.2` exist, oldest content gone);
  `rotate_at_exact_boundary`; `writer_creates_parent_dirs`;
  `no_rotation_under_limit`.
- **e2e tests:** none.
- **Done:** gates green; tests use `tempfile`; module compiles unused (allow
  dead_code only within this sub-phase, removed by 0.3).

#### 0.3 `AccessRecord` + nginx formatter + writer task + bounded channel
- **Model:** Sonnet.
- **Files:** `src/weblog.rs`.
- **Change:** define `AccessRecord` (§3.1) + `AccessLogger` registry (lazy
  `sender_for(key, layout)` spawns `writer_task` with a bounded `mpsc`, capacity
  e.g. 1024). `writer_task` formats combined-format lines (D12, CR/LF/`"`
  escaped), `write_line` to the `RotatingFileWriter`, flush every N records or
  on idle. Drop-on-full counter + periodic `warn!` (D3). RAW record shape.
  Timestamp: reuse an existing date dep if present; else add `time` (offset
  format) — confirm at impl time.
- **Unit tests:** `format_combined_http_line` (exact string incl. real IP,
  method, path, status, bytes); `format_raw_line`; `format_escapes_crlf`
  (path with `\r\n` → escaped, single line out); `writer_drops_on_full`
  (saturate channel → record dropped, counter++, no panic, no block);
  `logger_one_writer_per_key`.
- **e2e tests:** none (still not wired to data path).
- **Done:** gates green; `weblog.rs` fully unit-covered in isolation.

---

### Phase 1 — HTTP access tap (hot path) — **Opus review gate**

> The performance- and correctness-critical piece. Reviewed before merge.

#### 1.1 `HttpAccessTap<S>` adapter + HTTP/1.1 framing
- **Model:** **Opus design review → Sonnet implements.**
- **Files:** `src/weblog.rs` (tap + parsers); `Cargo.toml` (`httparse`).
- **Change:** implement §3.2. `AsyncRead`/`AsyncWrite` delegating to `inner`,
  feeding `ReqParser` (read side) and `RespParser` (write side). `httparse` for
  headers; hand-rolled Content-Length / chunked body skipping to advance to the
  next message on keep-alive. First-byte sniff: TLS (`0x16 0x03`) or non-HTTP
  request-line → `mode=Raw`. On any parse error → degrade to `Raw` (I-WL6).
  FIFO request↔response pairing → `AccessRecord` via `try_send`. Faithful
  `poll_shutdown`/`poll_flush`/EOF passthrough (I-WL3). No payload copy beyond
  the borrow `poll_read` already filled.
- **Unit tests** (mock duplex via `tokio_test::io` or in-mem pipe):
  `tap_http_single_request`; `tap_http_keepalive_multi` (3 requests, 3 records,
  correct paths/status); `tap_pipelined_requests`; `tap_chunked_body_boundary`
  (chunked request then a 2nd request parsed correctly);
  `tap_content_length_skip`; `tap_partial_header_across_reads` (header split
  over two `poll_read`s); `tap_raw_first_bytes` (binary → 1 raw record);
  `tap_tls_handshake_detected_raw`; `tap_malformed_degrades_to_raw`;
  `tap_half_close_preserved` (writer shutdown reaches inner);
  `tap_byte_identical_passthrough` (bytes out == bytes in, both directions);
  `tap_never_blocks_on_full_channel`.
- **e2e tests:** none yet (integrated in Phases 2/4).
- **Done:** gates green; Opus signs off that (a) no second payload copy, (b)
  half-close preserved, (c) all error paths degrade to Raw, (d) `try_send` only.

---

### Phase 2 — Server-side integration

> Server already has the real IP (D8). Independently shippable: server logs work
> even against an old client (no protocol dependency here).

#### 2.1 Server logger registry + public-tunnel wrap
- **Model:** Sonnet.
- **Files:** `src/server.rs:179` (state), `:646` (`listen` — build
  `Option<AccessLogger>` from server flags), `:1547` (have `addr`), `:1629`,
  `:1682` (splice sites).
- **Change:** store `Option<AccessLogger>` in server state. At each public splice
  site, if `Some`, wrap `edge` in `HttpAccessTap{ real_ip=addr, key="<port>",
  tx=logger.sender_for(port, FlatLayout) }` then pass the tap to
  `copy_bidirectional_with_sizes` (keep buf sizes). If `None`, call unchanged
  (I-WL1).
- **Unit tests:** `server_public_key_is_port` (key/path = `<port>.log`).
- **e2e tests:** `tests/local_proxy_hardening_test.rs::server_access_log_http`
  (server `--webserver-log <tmp>`, drive an HTTP GET through a public tunnel,
  assert `<tmp>/<port>.log` contains the request + the test client's real IP).
- **Done:** gates green; `stream_ready_banner_arrives_before_client_writes`
  (existing) still passes with flag **off** (I-WL1).

#### 2.2 Vhost wrap + capture real IP + folder layout
- **Model:** Sonnet.
- **Files:** `src/server.rs:682`, `:728` (capture `_addr`→`addr`, thread it),
  `src/vhost.rs:714` (`relay_vhost` signature gains `addr` + `Option<AccessLogger>`
  + `fqdn`/`subdomain`), `:772` (wrap `public`).
- **Change:** pass the accepted `addr` and the FQDN (`extract_host_from_head`
  result, already parsed) + subdomain (`extract_subdomain`) into `relay_vhost`.
  If logger `Some`, wrap `public` in `HttpAccessTap{ real_ip=addr,
  key="<subdomain>/<fqdn>", tx=logger.sender_for(..., SubdomainFolderLayout) }`.
  `SubdomainFolderLayout` writes `<dir>/<subdomain>/<fqdn>.log` (D11).
- **Unit tests:** `server_vhost_path_is_subfolder` (key `shop` + fqdn
  `shop.bore.example.com` → `<dir>/shop/shop.bore.example.com.log`);
  `vhost_addr_threaded_not_discarded`.
- **e2e tests:** `tests/vhost_test.rs::vhost_server_access_log_layout` (HTTP +
  HTTPS via existing stubs; assert subdomain-folder file exists with request +
  real IP).
- **Done:** gates green; existing vhost tests pass with flag off (I-WL1).

#### 2.3 Server flag wiring + retention
- **Model:** Haiku (wiring; mirrors 2.1 plumbing).
- **Files:** `src/server.rs:356+` (already parsed in 0.1) → construct
  `AccessLogConfig`/`AccessLogger` in `listen`.
- **Change:** thread retention (`max_files`, `max_file_size`) into the logger;
  ensure one `AccessLogger` shared across public + vhost paths.
- **Unit tests:** covered by 2.1/2.2 path tests; add
  `server_logger_shared_across_paths`.
- **e2e tests:** rotation asserted in 5.x.
- **Done:** gates green.

---

### Phase 3 — Real caller IP forwarding (protocol) — **Opus review gate**

> Wire-format change. Backward-compat is the whole risk; reviewed before merge.

#### 3.1 Extend readiness header (negotiated)
- **Model:** **Opus design review → Sonnet implements.**
- **Files:** `src/mux.rs:35` (doc the framing), `src/server.rs:1626`, `:1677`
  (public STREAM_READY writes), `src/vhost.rs:756` (vhost STREAM_READY write),
  `src/client.rs:1042-1043` (read).
- **Change:** §5.2. Server, when the tunnel's negotiated `webserver_log==true`,
  writes `[0x00, ip_len, ip_utf8]` (ip = the accepted `addr`); else bare `0x00`.
  Client, when it set `webserver_log`, reads the length-prefixed IP after the
  marker and stashes it for the tap; else reads one byte as today. Helper fns
  `write_ready_with_ip` / `read_ready_with_ip` in `mux.rs` or `shared.rs`.
- **Unit tests:** `readiness_header_roundtrip` (write→read recovers IP);
  `readiness_legacy_plain` (flag off both sides → exactly one byte on the wire,
  byte-identical); `readiness_empty_ip` (`ip_len=0` → client logs `-`);
  `readiness_interop_old_client` (old opts deserialize, server writes bare byte).
- **e2e tests:** exercised by 4.x client tests.
- **Done:** gates green; Opus confirms framing is unambiguous without a version
  byte (driven solely by the negotiated option) and the off-path is byte-identical.

---

### Phase 4 — Client-side integration

> Depends on Phase 1 (tap) + Phase 3 (IP). One insertion point covers vhost,
> public, relay, and direct-QUIC (all funnel through `handle_connection`).

#### 4.1 Client logger + tap wrap in `handle_connection`
- **Model:** Sonnet.
- **Files:** `src/client.rs:1037` (`handle_connection`), `:1042-1043`
  (read forwarded IP), `:1062` (splice).
- **Change:** if the client has an `AccessLogger`, after reading the readiness
  header (with IP per 3.1) wrap `stream` in `HttpAccessTap{ real_ip=fwd_ip,
  key=<file stem>, tx=... }` and splice `copy_bidirectional_with_sizes(&mut
  local_conn, &mut tap, buf, buf)`. If no logger, unchanged (I-WL1). Stem:
  vhost `<fqdn>` (flat), public `<port>` (flat) — D11.
- **Unit tests:** `client_key_vhost_fqdn`; `client_key_public_port`;
  `client_tap_off_when_no_logger` (no wrapper constructed).
- **e2e tests:** `tests/local_proxy_hardening_test.rs::local_access_log_real_ip_forwarded`
  (client `--webserver-log`; assert `<port>.log` shows the *external* caller IP,
  not the server/loopback);
  `tests/local_proxy_hardening_test.rs::local_access_log_raw` (send non-HTTP
  bytes → exactly one Raw line).
- **Done:** gates green; both relay and direct-QUIC inbound paths log (they share
  `handle_connection`).

#### 4.2 Client flag wiring + FQDN sourcing + direct/local paths
- **Model:** Sonnet (FQDN sourcing has a real uncertainty — see note).
- **Files:** `src/client.rs:579` (subdomain), `:60/182/614` (`remote_port`),
  `:1113` (`spawn_direct`), `:1374` (`provider_direct`).
- **Change:** build `Option<AccessLogger>` from client flags; set
  `TunnelOptions.webserver_log=true` when logging is on. **FQDN for the vhost
  filename:** the client knows its subdomain (`client.rs:579`) but the base
  domain may not be locally available — derive the FQDN from subdomain + the
  server's vhost domain; **if the base domain is not known client-side, source
  it from the vhost-registration `ServerMessage` or fall back to
  `<subdomain>.log`** (confirm exact availability at impl time and record the
  choice). Ensure `spawn_direct`/`provider_direct` accepted streams reach the
  same logging `handle_connection` (they already call it — no fork).
- **Unit tests:** `client_fqdn_from_subdomain_and_domain`;
  `client_webserver_log_sets_tunnel_option`.
- **e2e tests:** acceptance §1 covered by 5.x.
- **Done:** gates green; the §1 client-side files materialize with correct names.

---

### Phase 5 — e2e, bench, docs

#### 5.1 Cargo e2e (vhost + public) — **Opus review gate (acceptance assertions)**
- **Model:** **Opus review (assertions) → Sonnet implements.**
- **Files:** `tests/vhost_test.rs` (helpers `:46`, `:85`),
  `tests/local_proxy_hardening_test.rs` (helpers `:45`, `:66`).
- **Change:** add the §1 reference-scenario assertions end-to-end in-process:
  log file path, real-IP field, request line, status, and rotation.
- **Unit/e2e tests:** `vhost_access_log_written`, `vhost_access_log_real_ip`,
  `vhost_access_log_rotation` (drive > max_file_size → `.log.1` appears, line
  count preserved), `vhost_server_folder_layout`; `local_access_log_http`,
  `local_access_log_raw`, `local_access_log_real_ip_forwarded` (from 2.x/4.x,
  consolidated here).
- **Done:** gates green; Opus confirms each asserts an observable condition (no
  bare "file exists" without content check).

#### 5.2 netns e2e (real IP across hosts, raw vs http, layout, rotation)
- **Model:** Sonnet.
- **Files:** `scripts/vhost_netns_test.sh`, `scripts/local_proxy_netns_test.sh`.
- **Change:** add cases (stable IDs) run on the existing netns topologies:
  - **T-WLOG-VHOST-IP** — caller in `nsc` with a known IP → both client & server
    log files contain that exact IP (not the relay/loopback).
  - **T-WLOG-VHOST-SRV-LAYOUT** — server file is `<dir>/<sub>/<fqdn>.log`.
  - **T-WLOG-VHOST-ROTATE** — flood past `--webserver-log-max-file-size` →
    exactly `--webserver-log-max-files` files remain.
  - **T-WLOG-PUB-HTTP** — HTTP through public tunnel → per-request lines + real IP.
  - **T-WLOG-PUB-RAW** — `nc`/raw bytes → exactly one Raw line, tunnel still works.
  - **T-WLOG-PUB-IP** — server-side public `<port>.log` shows caller IP from
    accept addr.
  - **T-WLOG-CLIENT-IP-FWD** — client public `<port>.log` shows the **forwarded**
    external IP (proves Phase 3).
- **Done:** scripts pass under `sudo -n /abs/path/scripts/<script>.sh`; **rebuild
  release first** (`cargo build --release`) if a script runs the release binary.

#### 5.3 Bandwidth bench (the invariant proof) — **Opus review gate**
- **Model:** **Opus review → Sonnet implements.**
- **Files:** `scripts/vhost_bench.sh`, `scripts/local_bench.sh`.
- **Change:** add a with/without `--webserver-log` comparison (bulk HTTP
  download + raw throughput). **T-WLBENCH:** assert measured throughput with
  logging is within **5%** of without (and the dropped-record counter is allowed
  to rise under saturation without throughput loss — proves drop-on-full).
- **Done:** bench prints both numbers + delta; delta within threshold on the
  netns harness; Opus signs off the methodology (warm-up, repeats, raw + http).

#### 5.4 Documentation (README + this plan)
- **Model:** Haiku (prose) → **Opus final read**.
- **Files:** `README.md` — Local `:105` (`:131-152`), Vhost `:1129`
  (`:1200-1213`), Server `:1004`; add a dedicated **"Access logging"** section.
- **Change:** document all three flags exhaustively per command (defaults, units,
  file-naming rules client vs server, folder layout, rotation semantics, real-IP
  behavior, raw-vs-HTTP/HTTPS handling incl. the public-TLS connection-level
  limitation D10, drop-on-full guarantee). Add 2 example invocations + a sample
  log line. Cross-link this plan.
- **Done:** gates green (doctests if any); Opus final read confirms accuracy vs
  shipped behavior and that every flag/limitation is covered.

---

## 7. Invariants to preserve / add

- **I-WL1:** `--webserver-log` absent ⇒ **no tap wrapper constructed**; the data
  path (`client.rs:1062`, `server.rs:1629/1682`, `vhost.rs:772`) is byte-for-byte
  today's. Regression: `tap_byte_identical_passthrough` + existing splice tests
  pass with flag off.
- **I-WL2:** The tap **never blocks** the splice and **never alters bytes**;
  channel backpressure ⇒ **drop + counter**, never await. (`writer_drops_on_full`,
  `tap_never_blocks_on_full_channel`, T-WLBENCH.)
- **I-WL3:** The wrapper preserves **half-close** and the `proxy_buffer_size()`
  buffers — delegates `poll_shutdown`/`poll_flush`/EOF. (`tap_half_close_preserved`.)
- **I-WL4:** `TunnelOptions.webserver_log` is `#[serde(default)]`; old↔new
  client/server interop. (`tunnel_options_serde_default_webserver_log`,
  `readiness_interop_old_client`.)
- **I-WL5:** Readiness first byte stays `STREAM_READY (0x00)`; banner-first
  detection unaffected. (`readiness_legacy_plain`, `readiness_header_roundtrip`.)
- **I-WL6:** Any parse error / malformed HTTP / TLS / raw first bytes ⇒ degrade
  to **Raw** connection-level logging; the tunnel never stalls or breaks.
  (`tap_malformed_degrades_to_raw`, `tap_tls_handshake_detected_raw`,
  T-WLOG-PUB-RAW.)
- **I-WL7 (scope):** secret/proxy tunnels, `transfer`, and `vpn` are untouched;
  the secret relay stays AEAD-opaque (no plaintext to log). No edits to
  `secret.rs`/`vpn*.rs`.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Tap throttles throughput (defeats the whole constraint) | In-place parse, no 2nd copy, `try_send` + drop-on-full (D1–D3); proven by **T-WLBENCH** (≤5% delta) + I-WL2 tests. **Opus gates** Phase 1 & 5.3. |
| Keep-alive body framing bug mis-parses later requests | Hand-rolled Content-Length/chunked skip with unit coverage (`tap_chunked_body_boundary`, `tap_pipelined_requests`); on ambiguity → degrade to Raw (I-WL6). |
| Wire-compat break from readiness extension | Negotiated, serde-default, off-path byte-identical (I-WL4/I-WL5); interop + legacy tests; **Opus gate** Phase 3. |
| Slow/full disk stalls the writer → backpressure to data path | Bounded channel + drop-on-full (D3, I-WL2); writer task isolated; counter `warn!`. |
| FD/writer exhaustion with many subdomains | One writer per active target, lazily created; documented; idle-close deferred (note in 4.2). |
| Log injection via CRLF/quotes in path/UA | Formatter escapes control chars (D12, `format_escapes_crlf`). |
| Client FQDN unknown for vhost filename | Source from registration `ServerMessage` or fall back to `<subdomain>.log` — explicit decision recorded in 4.2. |
| Public-tunnel TLS can't be parsed | By design Raw connection-level (D10); documented limitation, not a bug; T-WLOG-PUB-RAW asserts it. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all -- --check` ·
  `cargo clippy --all-features --all-targets -- -D warnings` ·
  `cargo test --all-features` (CI `.github/workflows/ci.yml:24`).
- **Unit tests:** in `src/weblog.rs` (`#[cfg(test)]`) — rotation, formatter,
  drop-on-full, tap framing (HTTP/keep-alive/chunked/pipelined/raw/TLS/partial/
  half-close/byte-identical), readiness roundtrip/legacy/interop, config
  validation, serde default, path layouts.
- **e2e (cargo):** `tests/vhost_test.rs` + `tests/local_proxy_hardening_test.rs`
  — files written, real IP, request line, rotation, server folder layout, raw,
  forwarded-IP. Run: `cargo test --all-features`.
- **e2e (netns, sudo):** `scripts/vhost_netns_test.sh`,
  `scripts/local_proxy_netns_test.sh` — T-WLOG-* (rebuild release first if the
  script runs the release binary). Run via `sudo -n /abs/path/scripts/<s>.sh`.
- **Bench:** `scripts/vhost_bench.sh`, `scripts/local_bench.sh` — **T-WLBENCH**
  ≤5% throughput delta with logging on.
- **Acceptance:** the §1 reference scenario passes —
  vhost: `vhost_access_log_real_ip` + `vhost_server_access_log_layout` +
  **T-WLOG-VHOST-IP/-SRV-LAYOUT**; public: `local_access_log_http` +
  `local_access_log_raw` + `local_access_log_real_ip_forwarded` +
  **T-WLOG-PUB-HTTP/-RAW/-IP/-CLIENT-IP-FWD**; bandwidth: **T-WLBENCH**.

---

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 Haiku · 0.2 Sonnet · 0.3 Sonnet | Haiku/Sonnet | — |
| 1 | 1.1 Opus-review → Sonnet | Sonnet | **1.1** (hot path) |
| 2 | 2.1 Sonnet · 2.2 Sonnet · 2.3 Haiku | Sonnet | — |
| 3 | 3.1 Opus-review → Sonnet | Sonnet | **3.1** (protocol) |
| 4 | 4.1 Sonnet · 4.2 Sonnet | Sonnet | — |
| 5 | 5.1 Opus-review → Sonnet · 5.2 Sonnet · 5.3 Opus-review → Sonnet · 5.4 Haiku → Opus read | Sonnet/Haiku | **5.1** (acceptance) · **5.3** (bench) · **5.4** (final docs) |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical/boilerplate
> (0.1, 2.3, 5.4), escalate to Opus only for the gates above. **Print the model
> used per sub-task during implementation.**
