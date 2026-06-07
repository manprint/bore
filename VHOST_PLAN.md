# VHOST Implementation Plan — `bore vhost`

> **Audience:** the implementing agent (Sonnet 4.6).
> **Author:** planning pass (Opus), after a full read of the existing codebase.
> **Status:** design complete, not yet implemented.
> **Goal of this document:** give you everything needed to implement the `vhost`
> feature cleanly, phase by phase, **without regressions and without bugs**, reusing
> existing machinery instead of reinventing it. Every phase tells you exactly which
> tests to write so you never silently break what already works.

Read this whole document once before writing any code. Then implement strictly in
phase order. Do **not** start a phase before the previous one is green (fmt + clippy +
full test suite). This is a hard rule from `CLAUDE.md`: **zero regressions tolerated.**

---

## 0. What we are building (the mental model)

Today `bore` exposes a local service in two ways:

1. **Public TCP port** — `bore local <port>`: the server binds a public TCP port and
   forwards everything on it to one client (`server.rs::serve_tunnel`).
2. **Named secret tunnel** — `bore local --tcp-secret-id <id>` (provider) +
   `bore proxy` (consumer): no public port; the server relays each consumer substream
   to the registered provider (`secret.rs`).

We are adding a **third** mode: a **subdomain-routed HTTP(S) reverse proxy**.

```
bore vhost 127.0.0.1:8080 --subdomain mysubdomain --id client-id --to bore.mydomain.com
    → prints: https://mysubdomain.bore.mydomain.com
```

The server runs a single public **HTTP frontend (port 80)** and/or **HTTPS frontend
(port 443)**. For each incoming request it:

1. reads the **Host** header (and, for HTTPS, terminates TLS first with a **wildcard
   certificate** `*.bore.mydomain.com`),
2. extracts the **subdomain label** (`mysubdomain`),
3. looks it up in a **subdomain registry** to find the connected vhost client,
4. optionally injects configured **headers**,
5. **splices** the connection to that client over a fresh multiplexed substream — the
   client forwards it to its local service (`127.0.0.1:8080`).

**The single most important realization:** a vhost is *structurally identical* to a
**named secret provider**, except:

| | secret provider | vhost provider |
|---|---|---|
| Registry key | `tcp-secret-id` | `subdomain` |
| Who is the "consumer" | a remote `bore proxy` process | the server's own public HTTP(S) frontend |
| How a request is routed | explicit `ConnectSecret { id }` | parsed **Host header / subdomain** |
| Public surface | none | ports 80 / 443, one shared by all subdomains |

So ~80% of what you need already exists and must be **reused**, not rewritten:

- registration + heartbeat + instant cleanup → `secret::serve_provider` (`secret.rs:184`)
- the relay splice → `secret::relay` (`secret.rs:505`)
- the per-connection edge inspection (peek bytes, terminate TLS, redirect, basic-auth)
  → `edge::accept` (`edge.rs:122`)
- the client provider loop → `client::Client::new_secret_provider` (`client.rs:230`) +
  `Client::listen` (`client.rs:393`) + `handle_connection` (`client.rs:614`)
- auto-reconnect → `reconnect::run` (`reconnect.rs:79`)
- carrier pool (parallel TCP for throughput) → `pool::CarrierPool` (`pool.rs`)
- TLS cert parsing → `transport::server_tls_from_pem` (`transport.rs:179`)

The genuinely **new** code is: the public frontend listeners, the subdomain registry,
the `vhost.yml` config + hot reload, the cert hot reload, and header injection.

---

## 1. Decisions already made (do not relitigate)

These were decided with the project owner. Build to them.

1. **QUIC server↔client transport is DEFERRED.** The MVP rides the existing, proven
   **yamux-over-TCP/TLS** tunnel. Use `--carriers N` for throughput. Do **not** build a
   QUIC tunnel transport in this work. (QUIC in this codebase today is only the
   hole-punched peer-to-peer path for secret tunnels; leave it untouched.)
2. **Multi-map per command is DEFERRED.** One `--subdomain` + one target per `bore vhost`
   command. No `--map a=h:p --map b=h:p` yet.
3. **The public frontend is hand-rolled: peek + raw splice.** Reuse the `edge.rs`
   pattern. **Do not add `hyper`/`axum`/`tower`.** Rationale: the body must stream with
   zero framing overhead (multi-GB files), and the codebase already peeks bytes to make
   routing decisions. A full HTTP server would buffer/frame bodies and hurt throughput.
4. **Hot reload is mtime polling + atomic swap.** **Do not add `notify`.** A background
   task stats the files every ~2 s and swaps an `Arc` behind an `RwLock`.
5. **One new dependency only:** a YAML parser for `vhost.yml`. Use `serde_yaml`
   (or the maintained fork `serde_yml` if you prefer; pick one, note it in the PR).
   Everything else uses `std::sync::RwLock<Arc<T>>` — no extra dependency for swapping.

---

## 2. Hard invariants you must not break (from `CLAUDE.md`)

These are load-bearing. Violating any of them is a regression even if tests pass.

- **Client sends `Hello*` before auth.** yamux is lazy; the client must write its first
  message to announce the lazily-opened control substream, *then* the server runs the
  auth challenge. `HelloVhost` must follow the exact same ordering as `HelloSecret`
  (see `server.rs::handle_connection` at `server.rs:342`, and `client.rs:264`).
- **Server writes `mux::STREAM_READY` before splicing.** Banner-first protocols need it.
  Your relay must write it exactly like `secret.rs:518`.
- **`copy_bidirectional_with_sizes` propagates half-close.** Use it; never replace with a
  non-half-close variant. Buffer size is `PROXY_BUFFER_SIZE` (`shared.rs:36`, 64 KiB).
- **`shared::tune_tcp` must be applied to every new socket** (`shared.rs:74`,
  `TCP_NODELAY` + `SO_KEEPALIVE` 15 s). Apply it to every connection your frontend
  accepts.
- **`--max-conns` semaphore is the real bound.** Your frontend connections must acquire a
  permit from `conn_permits` exactly like `server.rs:544` (`try_acquire_owned`).
- **The single-connection default path stays byte-for-byte unchanged.** vhost is purely
  additive: if no `--vhost-config` is passed, the server behaves exactly as today.
- **`#![forbid(unsafe_code)]`** — no unsafe anywhere.

---

## 3. Architecture of the existing code you will touch (reference map)

Read these before implementing. File:line are accurate as of this plan.

**Protocol — `src/shared.rs`**
- `ClientMessage` enum at `shared.rs:420` (variants: `Hello`, `HelloSecret`,
  `ConnectSecret`, `Authenticate`, `JoinCarrier`, UDP messages, `TestUdpJoin`).
- `ServerMessage` enum at `shared.rs:501` (`Challenge`, `Hello`, `CarrierToken`, `Ok`,
  `Heartbeat`, `Error`, UDP messages…).
- `TunnelOptions` at `shared.rs:96` (`https`, `force_https`, `basic_auth`, `notes`,
  `carriers`).
- Wire format: null-delimited JSON, max frame `MAX_FRAME_LENGTH` (`shared.rs:25`, 1024
  bytes) — keep `HelloVhost` small (subdomain + id + short notes fit easily).
- `tune_tcp` `shared.rs:74`; `PROXY_BUFFER_SIZE` `shared.rs:36`; `NETWORK_TIMEOUT`.

**Server — `src/server.rs`**
- `Server` struct + builder setters (`set_tls` `server.rs:142`, `set_bind_domain`
  `server.rs:146`, `set_max_conns`, …); fields: `tls: Option<TlsAcceptor>`,
  `providers: Registry`, `conn_permits: Arc<Semaphore>`, `bind_addr`, `bind_tunnels`,
  `control_port`, `admin`, `max_carriers`, `pending_carriers`.
- `listen` `server.rs:191`: binds the control port, accepts, TLS-terminates the control
  connection, calls `route_connection`. **This is where you spawn the frontend tasks.**
- `create_listener` `server.rs:244`: how a public TCP listener is bound (you don't need
  this for vhost — frontends bind fixed 80/443 — but read it for the bind error mapping).
- `route_connection` `server.rs:287` → `handle_connection` `server.rs:326`: reads the
  first `ClientMessage` and dispatches by role (match at `server.rs:352`). **You add a
  `HelloVhost` arm here.**
- `serve_tunnel` `server.rs:433`: the public-port flow and carrier-pool setup — read it as
  the template for how a control loop + carrier pool is wired.

**Secret tunnels — `src/secret.rs`**
- `Registry = Arc<DashMap<String, Arc<CarrierPool>>>` at `secret.rs:66`.
- `Deregister` drop guard `secret.rs:168` — removes the registry entry on disconnect.
  **This is your "subdomain frees within milliseconds" guarantee. Copy the pattern.**
- `serve_provider` `secret.rs:184`: atomic `registry.entry(id)` insert with duplicate
  rejection (`secret.rs:202`), admin registration, `Ok` reply, carrier-pool join, the
  heartbeat `select!` loop. **`serve_vhost_provider` is a near-clone of this.**
- `relay` `secret.rs:505`: read the consumer's `STREAM_READY` marker, clone the pool out of
  the DashMap **without holding the guard across `.await`**, `pool.pick()`, `opener.open()`,
  write `STREAM_READY`, `copy_bidirectional_with_sizes`. **`relay_vhost` is a near-clone.**

**Edge inspection — `src/edge.rs`**
- `accept` `edge.rs:122`: peeks the first 8 bytes (`stream.peek`, non-consuming, with a
  timeout), detects TLS handshake (`0x16`), terminates TLS, or detects HTTP for the 308
  redirect, or runs the basic-auth gate. **Your frontend reuses this shape.**
- `TunnelStream` enum `edge.rs:45` (`Plain` / `Tls` / `Buffered`) implements
  `AsyncRead`/`AsyncWrite` — reuse it as the spliceable stream type.
- `read_request_head` `edge.rs:221` (reads to `\r\n\r\n`, capped at 8 KiB),
  `host_authority` `edge.rs:253` (extracts the Host header), `redirect_to_https`
  `edge.rs:195`, `looks_like_http` `edge.rs:190`. **Reuse all four** (you may need to make
  some `pub(crate)`).

**TLS — `src/transport.rs`**
- `server_tls_from_pem(cert_pem, key_pem) -> Result<TlsAcceptor>` `transport.rs:179`.
- `load_server_tls(cert_file, key_file)` `transport.rs:196`.
- The client config with the **insecure (no-verify) verifier** at `transport.rs:151`
  (used for `--insecure`). **Your integration tests' HTTP client reuses this to trust the
  self-signed test cert.**

**Client — `src/client.rs`**
- `new_secret_provider` `client.rs:230` (sends `HelloSecret`, expects `Ok`, sets up the
  carrier pool). `listen` `client.rs:393` (the `select!` event loop). `handle_connection`
  `client.rs:614` (reads the readiness marker, optional basic-auth gate, dials the local
  service, splices). **The vhost client reuses all of this with a different first message.**

**CLI — `src/main.rs`**
- `Command` enum `main.rs:40`; `Local` `main.rs:43` (the flag set to model client flags on);
  `Server` `main.rs:263` (where `--cert-file`/`--key-file` already live, `main.rs:311`);
  `Transfer` (nested subcommands — a structural example, but vhost is a *flat* client
  subcommand like `Local`). `parse_proxy_addr` `main.rs:1156` (parses `host:port`).
  Dispatch in `run`/`dispatch` `main.rs:652`+.

**Other reusables:** `pool::CarrierPool` (`pool.rs` — `new`, `push`, `pick`, lazily prunes
dead carriers), `reconnect::run` (`reconnect.rs:79`), `admin::{Role, NewEntry, register}`
(`admin.rs` — add a `Role::Vhost`), `auth::Authenticator` (HMAC challenge, unchanged),
`basicauth::gate` (`basicauth.rs`).

---

## 4. Design specifics (decisions + rationale)

### 4.1 Routing: Host header, not SNI

With a **wildcard certificate** `*.bore.mydomain.com`, the **same cert serves every
subdomain**, so we do **not** need SNI-based certificate selection and we do **not** need
to parse the TLS ClientHello. Therefore:

- **HTTPS path:** terminate TLS with the single wildcard `TlsAcceptor`, then read the
  **Host header** on the decrypted stream → extract subdomain → route.
- **HTTP path:** read the Host header directly → extract subdomain → route.

Both paths route identically (by Host), which is simpler and less bug-prone. SNI-based
multi-certificate routing is explicitly **future work** (document it; do not build it).

### 4.2 Subdomain extraction

`extract_subdomain(host: &str, base_domain: &str) -> Option<String>`:
1. strip an optional `:port` suffix from `host`;
2. lowercase (DNS is case-insensitive);
3. require the host to end with `.<base_domain>`; strip that suffix;
4. the remainder must be a **single label**: non-empty, chars `[a-z0-9-]`, no `.`
   (reject nested labels like `a.b.bore...` in the MVP — document as future work),
   not starting/ending with `-`;
5. return the label, else `None`.

### 4.3 Reservation semantics (`vhost.yml`)

`vhost.yml` maps `client_id ↔ subdomain`. When a vhost client registers with
`(subdomain, client_id)`:
- **subdomain reserved to this client_id** → accept;
- **subdomain reserved to a different client_id** → reject with a clear `Error`
  (`"subdomain 'x' is reserved"`);
- **subdomain not reserved by anyone** → accept if currently free (MVP open policy);
- **subdomain already claimed by a live connection** → reject (`"subdomain 'x' in use"`),
  exactly like the secret duplicate path (`secret.rs:202`).

The reservation is the lightweight identity binding requested by the feature. (Stronger
per-client auth via distinct secrets is future work; note it.)

### 4.4 Modes (resolve the "redundancy" question)

Four **distinct, non-redundant** modes:

| Mode | Port 80 | Port 443 | Requires cert |
|---|---|---|---|
| `http` | serves | — | no |
| `https` | — | serves | yes |
| `both` | serves | serves | yes |
| `redirect-https` | 308 → https | serves | yes |

`resolve_mode(configured, cert_present)`:
- no cert present → force `http` (this is the documented default "http only when started
  without certificates");
- `https` / `both` / `redirect-https` configured but **no cert** → **hard startup error**
  with a clear message (fail fast, do not silently downgrade).

### 4.5 Header injection (throughput-preserving)

Header injection is **opt-in per route** (default headers ∪ per-subdomain headers). If a
route has **no** configured headers, the frontend uses **pure `copy_bidirectional`** →
zero overhead, full throughput (this is the multi-GB path — keep it pristine).

When headers **are** configured: you have already read the request head via
`read_request_head`. Rewrite it (insert/override the configured headers, keeping the
request line and the other headers, terminate with `\r\n\r\n`), write the rewritten head to
the provider substream, then splice the rest of the connection raw.

**MVP limitation (document it explicitly):** headers are injected on the **first request
head** of the connection; on HTTP keep-alive, subsequent requests on the *same* TCP
connection are raw-spliced without re-injection. Full per-request rewriting (a minimal
HTTP/1.1 framing parser) is **future work**. This keeps the MVP correct and simple while
covering the overwhelmingly common case.

### 4.6 Scale, big files, fast free (how the requirements are met)

- **1000+ subdomains:** all share ports 80/443 → no per-tunnel port allocation, no port
  exhaustion. 1000 subdomains = 1000 `DashMap` entries + tasks. The `DashMap` is sharded
  and lock-free for distinct keys.
- **Big files without server saturation:** data is **spliced** (`copy_bidirectional`, 64
  KiB buffers), never buffered whole; backpressure flows via TCP + yamux windows. Server
  memory per connection is bounded.
- **Subdomain freed within ms:** the `Deregister` drop guard removes the registry entry
  synchronously when the client connection ends (~µs). Reuse it verbatim.
- **No races/deadlocks:** reuse the lock-free `DashMap` and the `Semaphore`; never hold a
  `DashMap` guard or a `Mutex` across an `.await` (see `secret.rs:510` for the pattern of
  cloning the `Arc` out first).

### 4.7 Atomic hot-swap without a new dependency

`Server` holds:
```rust
vhost_config: Arc<RwLock<Arc<VhostConfig>>>,     // std::sync::RwLock
vhost_tls:    Arc<RwLock<Option<Arc<TlsAcceptor>>>>,
```
A reader does: `let cfg = self.vhost_config.read().unwrap().clone();` — take the read lock,
**clone the inner `Arc`, drop the lock immediately** (never hold it across `.await`). The
reload task does `*self.vhost_config.write().unwrap() = Arc::new(new_cfg);`. In-flight
connections keep their captured `Arc`; new connections see the new value → **no downtime**.

---

## 5. Phased implementation

Implement in order. Each phase ends with a **mandatory gate**:

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test            # full suite — zero regressions
```

(If a phase adds the `udp`-gated code paths, also run `cargo clippy --all-features` and
`cargo test --all-features`.)

Each subphase lists **what to build** and **what to test**. Write tests *with* the code,
not after.

---

### Phase 0 — Scaffolding & dependency

**0.1 Add the YAML dependency.**
- `Cargo.toml`: add `serde_yaml = "0.9"` (or `serde_yml`). Note the choice in the commit.

**0.2 Create the module.**
- Add `src/vhost.rs` and `pub mod vhost;` in `src/lib.rs`.
- Add `Role::Vhost` to the admin role enum (`admin.rs`) and handle it in any `match`/
  display over `Role` (grep for existing `Role::` arms and extend them — the compiler will
  point out the non-exhaustive matches; fix each).

**Tests for Phase 0:** none new, but `cargo test` must stay green after adding the enum
variant (this catches every place `Role` is matched — fix them all). **Gate must pass.**

**Acceptance:** project compiles, all existing tests green, `vhost` module exists empty.

---

### Phase 1 — Config types + pure logic (no I/O)

This is the highest-value phase to get right because it is the most testable. Everything
here is pure functions over plain data — exhaustively unit-test it.

**1.1 Config data types** (`vhost.rs`):
```rust
#[derive(Clone, Debug, serde::Deserialize)]
pub struct VhostConfig {
    pub base_domain: String,
    #[serde(default)] pub mode: VhostModeCfg,        // default: derive from cert
    #[serde(default = "default_http_port")]  pub http_port: u16,   // 80
    #[serde(default = "default_https_port")] pub https_port: u16,  // 443
    #[serde(default)] pub cert_file: Option<PathBuf>,
    #[serde(default)] pub key_file:  Option<PathBuf>,
    #[serde(default)] pub default_headers: BTreeMap<String, String>,
    #[serde(default)] pub reservations: Vec<Reservation>,
}
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Reservation {
    pub client_id: String,
    pub subdomain: String,
    #[serde(default)] pub headers: BTreeMap<String, String>,
}
pub enum VhostMode { Http, Https, Both, RedirectHttps }   // resolved (runtime)
// VhostModeCfg = Option-like config enum (Http|Https|Both|RedirectHttps|Auto)
```
Provide `parse_config(yaml: &str) -> Result<VhostConfig>`.

**1.2 Pure functions:**
- `extract_subdomain(host, base_domain) -> Option<String>` (§4.2).
- `resolve_route<'a>(cfg, subdomain, client_id) -> RouteDecision` where `RouteDecision`
  is `Accept { headers: Vec<(String,String)> }` or `Reject { reason: String }` (§4.3).
- `merge_headers(default, per_subdomain) -> Vec<(String,String)>` (per-subdomain wins).
- `resolve_mode(cfg, cert_present) -> Result<VhostMode>` (§4.4).
- `public_urls(subdomain, base_domain, mode, http_port, https_port) ->
  (Option<String> http_url, Option<String> https_url)` — used for the `VhostReady` reply.
  Omit the `:port` when it's the scheme default (80/443).

**Tests for Phase 1 (unit, in `vhost.rs #[cfg(test)]`):**
- `extract_subdomain`: ✓ `mysub.bore.example.com` → `mysub`; ✓ strips `:443`;
  ✓ case-insensitive (`MySub.Bore...` → `mysub`); ✗ wrong base domain → `None`;
  ✗ nested label `a.b.bore...` → `None`; ✗ empty label → `None`; ✗ illegal chars
  (`my_sub`, `my.sub`) → `None`; ✗ leading/trailing hyphen → `None`.
- `parse_config`: a representative `vhost.yml` round-trips into the struct; missing
  optional fields use defaults; an unknown `mode` value errors.
- `resolve_route`: reserved→matching id = Accept (with merged headers); reserved→other id
  = Reject with the reserved reason; unreserved = Accept.
- `merge_headers`: per-subdomain overrides a default of the same name; disjoint keys union.
- `resolve_mode`: no cert → `Http`; `https`/`both`/`redirect-https` without cert → `Err`;
  with cert, configured value is honored; `auto` + cert → `Both` (pick and document the
  auto default).
- `public_urls`: default ports omit `:port`; non-default ports include it; mode gates which
  URLs are `Some`.

**Acceptance:** all pure logic unit-tested and green; no I/O yet.

---

### Phase 2 — Protocol messages

**2.1 Add `ClientMessage::HelloVhost`** (`shared.rs`, in the enum at `shared.rs:420`):
```rust
HelloVhost {
    subdomain: String,
    client_id: String,
    notes: Option<String>,
    basic_auth: Option<String>,
    #[serde(default)] carriers: u16,
},
```
**2.2 Add `ServerMessage::VhostReady`** (`shared.rs:501`):
```rust
VhostReady { http_url: Option<String>, https_url: Option<String> },
```

**Tests for Phase 2 (unit):**
- Serialize→deserialize round-trip for both new variants (mirror any existing message
  round-trip test; if none exists, add a small one). Assert the encoded form stays within
  `MAX_FRAME_LENGTH` for a realistic subdomain/id/notes.
- A `cargo test` run confirms the new variants don't break the existing dispatch matches
  (the compiler forces you to handle them — see Phase 3).

**Acceptance:** protocol compiles; new messages round-trip; suite green.

---

### Phase 3 — Server registry + provider + relay

**3.1 Registry types** (`vhost.rs`):
```rust
pub struct VhostEntry {
    pub pool: Arc<CarrierPool>,
    pub headers: Vec<(String, String)>,   // resolved at registration time
}
pub type VhostRegistry = Arc<DashMap<String, Arc<VhostEntry>>>;   // key = subdomain
```
Add `vhost_registry: VhostRegistry` to `Server` and initialize it in the constructor.

**3.2 `serve_vhost_provider`** — clone `secret::serve_provider` (`secret.rs:184`) and adapt:
- validate against the live config: take `cfg = vhost_config.read().clone()`, call
  `resolve_route(&cfg, &subdomain, &client_id)`; on `Reject` send
  `ServerMessage::Error(reason)` and return `Ok(())`;
- atomic insert into `vhost_registry` with duplicate rejection (mirror `secret.rs:202`);
- a `Deregister`-style drop guard keyed by subdomain (mirror `secret.rs:168`);
- admin `register` with `Role::Vhost`, `secret_id: Some(subdomain.clone())`;
- compute `public_urls(...)` and send `ServerMessage::VhostReady { http_url, https_url }`;
- carrier-pool setup + heartbeat `select!` loop: copy from `serve_provider` (you can drop
  the UDP arms — vhost has no direct UDP path in the MVP; keep the heartbeat + carrier-join
  arms).

**3.3 `relay_vhost`** — clone `secret::relay` (`secret.rs:505`):
```rust
async fn relay_vhost(public: impl AsyncRead+AsyncWrite+Unpin, reg: &VhostRegistry, sub: &str,
                     inject: Option<&[(String,String)]>, head: Vec<u8>) -> Result<()>
```
- look up the entry, clone the `Arc<CarrierPool>` out of the DashMap (no guard across
  `.await`), `pool.pick()`, `opener.open()`, write `mux::STREAM_READY`;
- if `inject` is `Some`, write the (already rewritten) `head` first, then
  `copy_bidirectional_with_sizes(public, provider)`; if `None`, just splice (the head was
  never consumed — see Phase 4 for how the head is handled in each case).
- Consider extracting the shared splice tail into a helper reused by `secret::relay` to
  avoid divergence (optional; only if it stays clean).

**3.4 Dispatch wiring** (`server.rs::handle_connection`, match at `server.rs:352`):
```rust
Some(ClientMessage::HelloVhost { subdomain, client_id, notes, basic_auth, carriers }) =>
    vhost::serve_vhost_provider(control, opener, self.vhost_registry.clone(),
        self.vhost_config.clone(), subdomain, client_id, self.admin.clone(), peer,
        notes, basic_auth, self.pending_carriers.clone(), self.max_carriers, carriers).await,
```

**Tests for Phase 3 (integration, `tests/vhost_test.rs`):**
- **Registration + duplicate:** start a server (no frontend yet needed — test the registry
  directly via a vhost client + a hand-driven substream, *or* defer the full path to Phase
  3.5 below). Minimal here: a vhost client registers and receives `VhostReady` with the
  expected URLs; a second client claiming the same subdomain gets an `Error`.
- **Reservation:** with a `vhost.yml` reserving `sub→clientA`, client A is accepted, client
  B (`--id clientB`) is rejected with the reserved message.
- **Instant free:** after the first client disconnects, a re-registration of the same
  subdomain succeeds immediately (assert within a tight timeout, e.g. 200 ms).

> Note for the implementer: full request routing needs the frontend (Phase 4). If wiring a
> raw substream by hand is awkward this early, you may move the *routing* assertions into
> Phase 4 and keep Phase 3 tests focused on registration/reservation/free. Whichever you
> choose, **every bullet above must be covered by the end of Phase 4.**

**Acceptance:** vhost clients register/deregister/reserve correctly; suite green.

---

### Phase 4 — Public frontend (HTTP + HTTPS) + routing + header injection

This is the most delicate phase. Build it incrementally.

**4.0 Make edge helpers reusable.** Change visibility of `read_request_head`,
`host_authority`, `looks_like_http`, `redirect_to_https` (and `TunnelStream`) to
`pub(crate)` as needed. Do **not** change their behavior.

**4.1 Server frontend fields + builder.** Add to `Server`:
`vhost_config: Arc<RwLock<Arc<VhostConfig>>>`, `vhost_tls: Arc<RwLock<Option<Arc<TlsAcceptor>>>>`,
and a `set_vhost(config_path, mode_override, http_port, https_port)` builder that loads the
config + cert once at startup (reusing `transport::server_tls_from_pem`), runs
`resolve_mode` (failing fast on cert-less https), and stores the `Arc`s.

**4.2 Spawn frontend tasks in `Server::listen`** (`server.rs:191`), only when a vhost
config is present, after the control listener is up:
- if mode serves HTTP (`http`/`both`/`redirect-https`): bind `(bind_tunnels, http_port)`,
  spawn an accept loop;
- if mode serves HTTPS (`https`/`both`/`redirect-https`): bind `(bind_tunnels, https_port)`,
  spawn an accept loop.
- Each accepted socket: `tune_tcp(&socket)`; acquire a `conn_permits` permit
  (`try_acquire_owned`, drop the connection if exhausted — mirror `server.rs:544`); spawn a
  per-connection task holding the permit.

**4.3 HTTP connection handler:**
1. `read_request_head` (timeout-bounded);
2. if mode is `redirect-https` → `redirect_to_https` (reuse `edge.rs:195`) and return;
3. `host_authority`/Host parse → `extract_subdomain(host, base_domain)`;
4. registry lookup → if missing, write a clean `502 Bad Gateway` (or `404`) with
   `Connection: close` and return (never hang);
5. resolve injection headers from the entry; if present, **rewrite the head** and pass it +
   `inject` into `relay_vhost`; if absent, pass the **unmodified head** so `relay_vhost`
   writes it through then pure-splices. (Either way the bytes already read must be
   forwarded — do not lose the request head.)

**4.4 HTTPS connection handler:**
1. `acceptor = self.vhost_tls.read().unwrap().clone()` → if `None`, drop (mis-config);
2. `acceptor.accept(socket).await` → decrypted stream (wildcard cert; no SNI needed);
3. from here identical to 4.3 steps 1,3,4,5 on the decrypted stream (no redirect on 443).

**4.5 Header rewrite function** (pure, unit-testable):
`rewrite_head(head: &[u8], inject: &[(String,String)]) -> Vec<u8>` — keep the request line,
drop any existing headers whose names match injected ones (case-insensitive), append the
injected headers, preserve the terminating `\r\n\r\n` and any already-read body bytes that
followed it in `head`.

**4.6 Basic-auth (optional, reuse).** If `--basic-auth` was set on the vhost client, gate
the (decrypted) HTTP stream with `basicauth::gate` exactly as `edge.rs:169` does, before
relaying. (Carry the `basic_auth` flag through registration as `serve_provider` already
does.)

**Tests for Phase 4 (integration, `tests/vhost_test.rs`):**
Set up: a local stub HTTP server (a `tokio::net::TcpListener` that returns a known body and
echoes received request headers), a `bore server` with a vhost config, a `bore vhost` client
pointing at the stub, and a test HTTP client.
- **HTTP routing:** GET `http://<sub>.<base>:<http_port>/` (Host header set) → stub receives
  it, body byte-exact at the client. Subdomain correctly extracted.
- **HTTPS routing:** generate a self-signed wildcard cert for `*.<base>` (see §6), configure
  the server with it, connect a `tokio-rustls` client using the **no-verify verifier**
  (`transport.rs:151`), Host set → routed, body byte-exact.
- **Redirect mode:** HTTP request returns `308` with `Location: https://<sub>.<base>...`.
- **Unknown subdomain:** returns a clean `502`/`404`, connection closes, no hang (assert
  within a timeout).
- **Header injection:** configure default + per-subdomain headers; assert the stub received
  exactly the merged set (per-subdomain overriding default).
- **No-header fast path:** a route with no headers still routes correctly (pure splice).
- **Large body integrity:** stream a multi-MB body through and assert byte-exact (throughput
  sanity + half-close correctness).
- **Concurrency smoke:** register N (e.g. 20) subdomains on one server, route to each,
  assert isolation (each gets its own body).

**Test hygiene (reuse the patterns the transfer tests already use):**
- Serialize tests that bind shared ports with a `lazy_static! SERIAL_GUARD: Mutex<()>` and
  lock it at the top of each test (the transfer suite does this).
- Bind frontends to `127.0.0.1` and **ephemeral ports in tests** (make `http_port`/
  `https_port` configurable so tests use `0`/high ports — do not require 80/443 in CI).
- If any code path prompts on a TTY, gate it behind a clearly-named `BORE_TEST_*` env var
  read **without** `#[cfg(test)]` (integration tests compile the lib *without* `cfg(test)`;
  this bit us before). vhost likely has no prompt, but keep the rule in mind.

**Acceptance:** end-to-end HTTP + HTTPS routing, redirect, header injection, unknown-host,
and large-body all green; existing suite green.

---

### Phase 5 — Hot reload (config + cert), zero downtime

**5.1 Reload task.** In `Server::listen`, when a vhost config is present, spawn a task:
- `interval(Duration::from_secs(2))` (`MissedTickBehavior::Delay`);
- on each tick, `fs::metadata(...).modified()` for `vhost.yml`, `cert_file`, `key_file`;
- if `vhost.yml` mtime changed: reparse; on success swap `*vhost_config.write() = Arc::new(new)`;
  on parse error, **log and keep the old config** (never crash, never serve a broken
  config);
- if cert/key mtime changed: rebuild the `TlsAcceptor` via `server_tls_from_pem`; on success
  swap `vhost_tls`; on error, log and keep the old acceptor.

**5.2 Readers already swap-safe** (you built them swap-safe in Phase 4: clone the `Arc`
under a short read lock). Verify no `.await` happens while holding the lock.

**Tests for Phase 5 (integration):**
- **Config reload:** start with `vhost.yml` reserving `sub→A`; at runtime rewrite the file
  to reserve `sub→B`; after >2 s, a new registration with `--id B` is accepted and `--id A`
  is rejected. (Use a `tempfile`-style path the test owns.)
- **Cert reload:** start with cert1; swap the cert files to cert2 (different SAN or serial);
  after >2 s, a fresh HTTPS connection presents cert2. **In-flight connection survives:**
  open a long-lived HTTPS request, trigger a cert swap mid-stream, assert the in-flight
  body completes byte-exact (no downtime).
- **Bad config ignored:** write malformed YAML; assert the server keeps serving with the
  previous config (no crash, no dropped traffic) and logs an error.

> Reload-timing note: keep the 2 s poll interval but allow tests to wait `> interval`
> deterministically (e.g. poll the observable behavior in a loop with a timeout rather than
> a fixed sleep) to avoid flakiness.

**Acceptance:** config + cert reload work with zero downtime; bad input is survived; green.

---

### Phase 6 — CLI

**6.1 Client subcommand** (`main.rs`, add to `Command` `main.rs:40`, model on `Local`
`main.rs:43`):
```
bore vhost <target>            # positional host:port, parsed via parse_proxy_addr
  --subdomain <s>  --id <client-id>
  [--secret <s>] [--to <server>] [--insecure] [--auto-reconnect]
  [--carriers <n>] [--basic-auth <user:pass>] [--notes <text>]
```
Build a `VhostOptions` struct (model on `ListenerOptions` `transfer.rs:67`) and call
`vhost::run_client(opts)`. Wrap the connect+serve in `reconnect::run` when
`--auto-reconnect` is set (mirror the `Local` dispatch).

**6.2 `vhost::run_client`** — reuse the client provider path:
- connect like `Client::new_secret_provider` (`client.rs:230`) but send
  `ClientMessage::HelloVhost{..}` and expect `ServerMessage::VhostReady{..}`;
- print the returned public URL(s) to the user;
- run `Client::listen` (`client.rs:393`); each accepted substream → `handle_connection`
  (`client.rs:614`) splices to the local `host:port` target.
- The cleanest implementation generalizes the existing provider so the only differences are
  the first message and the success reply; avoid copy-pasting the whole `listen` loop if a
  small parameterization suffices.

**6.3 Server flags** (`main.rs::Server` `main.rs:263`; cert flags already exist `main.rs:311`):
```
--vhost-config <path>    # presence enables the vhost frontend
--http-port <n>          # default 80
--https-port <n>         # default 443
--vhost-mode <http|https|both|redirect-https>   # overrides vhost.yml mode
```
Wire them via `Server::set_vhost(...)` in the server dispatch (`main.rs:976`+). `base_domain`
comes from `vhost.yml`, falling back to the existing `--bind-domain` (`server.rs:146`).

**Tests for Phase 6:**
- A CLI-level smoke test (or extend Phase 4 tests to drive through `VhostOptions` /
  `run_client` instead of hand-built clients), confirming the public path works through the
  real entry points.
- `--auto-reconnect`: kill the server, bring it back, assert the client re-registers and the
  subdomain works again (mirror any existing reconnect test pattern).
- Argument validation: `--vhost-mode https` with no cert → the server exits with the clear
  error from `resolve_mode`.

**Acceptance:** `bore vhost` works end-to-end from the CLI; server flags wired; green.

---

### Phase 7 — Documentation & test matrix

Per `CLAUDE.md`, docs are part of the deliverable.

**7.1 `README.md`** — add a `vhost` section: the command, the produced URL, a `vhost.yml`
example, the mode table, and the wildcard-cert/DNS prerequisite
(`*.bore.mydomain.com` + `bore.mydomain.com` → public IP).
**7.2 `docs/`** — add `docs/VHOST.md` (user guide) and a `vhost.yml` reference (every field,
defaults, hot-reload behavior, header precedence, modes).
**7.3 Test matrix** — add `docs/VHOST_TEST_MATRIX.md` mirroring the style of
`docs/TRANSFER_TEST_MATRIX.md`: one row per scenario with the covering test name and status,
plus a "coverage gaps / future work" section listing: QUIC tunnel transport, multi-map per
command, full per-request header injection on keep-alive, SNI-based multi-cert, nested
subdomain labels, per-client distinct secrets.
**7.4** Update the version/architecture notes if the project keeps them
(the `bore <semver> - <branch> - <sha8>` string is auto-generated by `build.rs`; no action).

**Acceptance:** docs complete and accurate; matrix lists every test and the known gaps.

---

## 6. Test infrastructure notes (read before Phase 4)

- **Generating a wildcard test cert.** `rcgen` is already a dependency (optional, under the
  `udp` feature). Easiest robust approach for tests: add `rcgen` to `[dev-dependencies]`
  and generate a self-signed cert for SAN `*.bore.local` + `bore.local` at test setup, write
  the PEM to a tempfile, point the server at it. Alternatively commit static PEM fixtures
  under `tests/fixtures/`. Pick one; the generated approach avoids committing key material.
- **HTTPS client in tests.** Reuse the codebase's insecure (no-verify) rustls client config
  (`transport.rs:151`) so the test client trusts the self-signed cert. Do not disable
  verification anywhere in non-test code.
- **Ports in CI.** Never hardcode 80/443 in tests — they need root and conflict. Make the
  frontend ports configurable and use ephemeral/high ports in tests.
- **Serialization.** Reuse the `SERIAL_GUARD: Mutex<()>` pattern from `tests/transfer_test.rs`
  for any test that binds a shared server/control port.
- **No `#[cfg(test)]` for test-only hooks read by integration tests.** Integration tests
  compile the library **without** `cfg(test)`; any env-var injection hook must be a plain
  runtime check (use a `BORE_TEST_`-prefixed name). (This caused a real bug in a prior
  feature.)
- **Determinism over sleeps.** For reload tests, poll the observable outcome in a bounded
  loop instead of a single fixed `sleep` to avoid flakiness.

---

## 7. Definition of done

- All 8 phases implemented in order, each gate green at the time it was completed.
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` all pass on the final tree (also `--all-features` for the udp-gated paths).
- The default (no `--vhost-config`) server behavior is **byte-for-byte unchanged** — verify
  by running the full pre-existing suite with zero modifications to existing tests' meaning.
- `README.md`, `docs/VHOST.md`, the `vhost.yml` reference, and `docs/VHOST_TEST_MATRIX.md`
  are written and accurate.
- Every "Tests for Phase N" bullet has a corresponding, passing test.
- All invariants in §2 hold. No `unsafe`. No new dependency beyond the one YAML crate.

---

## 8. Quick reference — files you will create or modify

| File | Action |
|---|---|
| `Cargo.toml` | add YAML dep; add `rcgen` to dev-deps (for test certs) |
| `src/lib.rs` | `pub mod vhost;` |
| `src/vhost.rs` | **NEW** — config, pure logic, registry, provider, relay, frontend, reload, client |
| `src/shared.rs` | add `ClientMessage::HelloVhost`, `ServerMessage::VhostReady` |
| `src/server.rs` | vhost fields + `set_vhost`; spawn frontend + reload tasks in `listen`; dispatch arm in `handle_connection` |
| `src/edge.rs` | widen visibility of `read_request_head`/`host_authority`/`looks_like_http`/`redirect_to_https`/`TunnelStream` to `pub(crate)` |
| `src/client.rs` | reuse/generalize the provider path for the vhost client |
| `src/admin.rs` | add `Role::Vhost` and extend all `Role` matches |
| `src/main.rs` | `bore vhost` subcommand + `VhostOptions`; server `--vhost-*` flags |
| `tests/vhost_test.rs` | **NEW** — all integration tests above |
| `README.md`, `docs/VHOST.md`, `docs/VHOST_TEST_MATRIX.md` | **NEW/updated** docs |

Implement deliberately, test as you go, keep every gate green. Good luck.
