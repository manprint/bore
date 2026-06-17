# Admin Dashboard Revamp — Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Branch:** `frontend`.
> **Target:** a modular, multi-section monitoring dashboard at `/admin/status`
> that surfaces every live subsystem (public tunnels, secret tunnels, vhost, VPN),
> real peer IPs, expandable notes, passed parameters, connection state, TLS cert
> expiry, server startup config, memory + bandwidth — **with zero changes to the
> data plane and zero regressions**. Adding a future section must require only a
> new JS module + one registry line + one API endpoint. Minimize implementation
> tokens (mechanical sub-phases → Haiku).

---

## 1. Context & problem

### Current state (recon anchors)

- **Admin HTTP** is raw-tokio (no web framework), in `src/admin_http.rs`:
  - `serve()` entry — `src/admin_http.rs:50`
  - route match (implicit, `path.split('?').next()`) — `src/admin_http.rs:74`
  - `GET /admin/status` + `/admin/status/` serve embedded HTML — `src/admin_http.rs:75-84`
  - `GET /admin/status/data` serves token-guarded JSON snapshot — `src/admin_http.rs:85-102`
  - HTML body is **one embedded file**, vanilla JS inline — `include_str!("admin_status.html")` at `src/admin_http.rs:19`; file `src/admin_status.html`
  - `ServerStatus` / `StatusView` response structs — `src/admin_http.rs:25-39`
  - auth guard `authorized()` (Bearer **or** `X-Admin-Token`, min-32, constant-time) — `src/admin_http.rs:159-183`; constant-time cmp `src/basicauth.rs:139`; token min-len enforced `src/main.rs:1710-1716`; token source `--admin-token` / `BORE_ADMIN_TOKEN` `src/main.rs:489`
- **Dispatch into admin HTTP** (only when `--admin-token` set, else 404): `serve_admin_http()` `src/server.rs:873`, calls `admin_http::serve()` `src/server.rs:887`; first-byte HTTP discriminator `src/server.rs:819`; vhost-host routing precedes it `src/server.rs:836`.
- **Live state** lives on the `Server` struct — `src/server.rs:86`. Per-subsystem registries:
  | Subsystem | Field (server.rs) | Type / value | Anchor |
  |-----------|-------------------|--------------|--------|
  | Public tunnels | `admin` (`server.rs:141`) | `AdminRegistry` = `Arc<DashMap<u64, Arc<Entry>>>` | `src/admin.rs:140-147` |
  | — `Entry` fields | — | `public_port: Option<u16>`, `peer: SocketAddr` (real client IP), `active: Arc<AtomicUsize>`, `relay_tx_bytes`/`relay_rx_bytes: Arc<AtomicU64>`, notes/flags | `src/admin.rs:43-76` |
  | — snapshot | — | `AdminRegistry::snapshot() -> Vec<EntryView>` | `src/admin.rs:192-220` |
  | — `EntryView` | — | role, secret_id, peer(str), https, basic_auth, notes, udp, overlay, vpn_direct, relay tx/rx, active, uptime_secs | `src/admin.rs:102-136` |
  | Vhost | `vhost_registry` (`server.rs:152`) | `Arc<DashMap<String, Arc<VhostEntry>>>` keyed by subdomain | `src/vhost.rs:439` |
  | — `VhostEntry` | — | `pool: Arc<CarrierPool>`, `request_headers`/`response_headers`, `direct: DirectPool` (cfg udp), `direct_stream_opens: AtomicU64` | `src/vhost.rs:337-353` |
  | Secret (relay) | `providers` (`server.rs:113`) | `Arc<DashMap<String, Arc<CarrierPool>>>` keyed by tcp-secret-id | `src/secret.rs:66` |
  | Secret (UDP) | `udp_providers` (`server.rs:117`) | `Arc<DashMap<String, UdpReg>>`; `UdpReg{candidates, selected_stun, nonce, to_provider}` | `src/secret.rs:72-88` |
  | Public UDP direct | `public_udp_registry` (`server.rs:176`) | `PublicDirectEntry{direct_stream_opens: AtomicU64}` | `src/server.rs:67` |
  | VPN links | `vpn_providers` (`server.rs:192`, cfg vpn) | `Arc<DashMap<String, VpnProviderEntry>>` | `src/vpn_server.rs:27` |
  | — `VpnProviderEntry` | — | `advertised: Vec<Ipv4Net>`, `addr: VpnAddrRequest`, `hub: Option<HubShared>`, `carriers: u16` | `src/vpn_server.rs:184-202` |
  | — `HubState` | — | `subnet`, `hub_overlay`, `advertised`, `peers: HashMap<u32, PeerSlot>` | `src/vpn_server.rs:52-64` |
  | — `PeerSlot` | — | `peer_id: u32`, `overlay: Ipv4Addr`, `nonce`, `hub_candidates: Vec<SocketAddr>`, `hub_selected_stun` | `src/vpn_server.rs:69-81` |
- **TLS certs:** loaded as `CertificateDer` but **expiry is never parsed** (tokio-rustls does not expose validity). Control TLS acceptor `src/server.rs:129`; vhost hot-swap acceptor `src/server.rs:158`; cert/key paths in `VhostConfig{cert_file, key_file}` `src/vhost.rs:48-75`; loader `transport::load_server_tls()` `src/transport.rs:196-200`; hot-reload poll loop `src/server.rs:568-595`.
- **Server config:** parsed `Args` `src/main.rs:35`; `Command::Server{..}` `src/main.rs:49`; applied `src/main.rs:1721-1854` (port_range, max_conns, max_carriers, control_port, udp, udp_tuning, bind_addr, bind_tunnels, vpn_*, vhost_config, tls). **No config snapshot struct is exposed.**
- **Metrics:** per-tunnel `active` + `relay_tx_bytes`/`relay_rx_bytes` atomics (`admin.rs`), `direct_stream_opens` (vhost/public). **No process memory metric, no global/cumulative bandwidth counter, no aggregate.**
- **Version string:** `bore <semver> - <branch> - <sha8>` embedded by `build.rs` (`BORE_GIT_BRANCH`/`BORE_GIT_SHA`).
- **Tests:** gates `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo build --all-features`, `cargo test --all-features`. Inline `#[cfg(test)]` + `tests/` dir; existing `tests/admin_test.rs`. e2e = sudo bash netns scripts in `scripts/`. **No JS toolchain (no npm/node), no headless browser, no frontend test today.** Asset embedding today = `include_str!` only.

### Goal

Replace the single-page `admin_status.html` with a **modular vanilla-JS SPA** served from embedded assets, backed by a **versioned per-section JSON API** (`/admin/api/v1/*`). The dashboard has a left sidebar menu; each menu item is an independent **panel** (Overview, Public tunnels, Secret tunnels, Vhost, VPN, Certificates, Config, Metrics). Panels show real peer IPs, expandable notes, passed parameters, live connection state (polling refresh), cert expiry/days-remaining, server startup config, and process memory + bandwidth. Architecture is built so a new section = one new JS panel module + one registry entry + one API endpoint, touching no shared plumbing. The bore data plane and all existing behavior are byte-for-byte unchanged.

### Reference scenario (final acceptance test)

Start a server with admin enabled, a vhost with a TLS cert, and a live public tunnel from a client at a known IP; then drive the dashboard API and assert observable facts.

```
# server (TLS vhost + admin), client tunnel from 10.0.0.2
bore server --admin-token "$TOK" --vhost-config vhost.yaml --udp ...
bore local 8080 --to server ...        # client at 10.0.0.2, public port assigned P

# Acceptance (contract e2e, scripts/admin_dashboard_test.sh):
GET /admin/status                       -> 200 text/html  (new shell)
GET /admin/ui/app.js                    -> 200 text/javascript
GET /admin/api/v1/tunnels   (no token)  -> 401
GET /admin/api/v1/tunnels   (Bearer TOK)-> 200; JSON contains an entry with
                                           public_port==P AND peer starts "10.0.0.2:"
GET /admin/api/v1/certs     (Bearer TOK)-> 200; vhost cert entry has integer
                                           days_remaining AND not_after RFC3339
GET /admin/api/v1/config    (Bearer TOK)-> 200; control_port present, NO admin_token field
GET /admin/api/v1/metrics   (Bearer TOK)-> 200; uptime_secs>=0, mem present-or-null,
                                           bandwidth_tx_bytes>=0
GET /admin/status/data      (Bearer TOK)-> 200; byte-shape identical to pre-change (I-1)
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | New API under `/admin/api/v1/*`, one endpoint per section. | Sections evolve independently; v1 namespace lets fields be added additively forever. Router gains one prefix branch, not N scattered routes. |
| **D2** | `/admin/status/data` is **preserved unchanged** (response shape byte-for-byte). | Existing external scripts keep working. It becomes a thin compat alias over the new `tunnels` view assembler (must serialize identically — pinned by regression test T-COMPAT). |
| **D3** | The old `admin_status.html` HTML is **replaced** by the new SPA shell at `/admin/status` (this replacement is the intended deliverable, not a regression). | `src/admin_status.html` + its `include_str!` are deleted at the end of Phase 3; the UI change is expected. Only the **JSON contract** (D2) is frozen. |
| **D4** | Assets embedded via a **`build.rs` bundler** generating a static `&[(path, bytes, content_type)]` table from `src/admin_ui/` (using `include_bytes!`). **No runtime crate dependency.** | Drop a file into `src/admin_ui/` → next `cargo build` embeds it. No rust-embed, no npm. Reuses the existing `build.rs`. `#![forbid(unsafe_code)]` preserved (generated code is plain slices). |
| **D5** | Frontend = **vanilla ES-module SPA**, hash-router, **panel registry**. Each panel is a self-contained module exporting `{id,title,route,endpoint,refreshMs,render}`. The sidebar + router are generated from the registry array. | Modularity contract (I-6): a new section never edits the router or shell. Zero build step; assets served as-is. |
| **D6** | **Polling** for live data (per-panel `refreshMs`, client-configurable), no SSE/WebSocket. | No new server streaming endpoint, no broadcast state, no extra regression surface. SSE is a future additive extension. |
| **D7** | Auth model unchanged: **all `/admin/api/v1/*` + `/admin/status/data` are token-guarded via `authorized()`**; the **HTML shell + static assets are unguarded** (they contain no secrets). Token entered in a login field, kept in `sessionStorage`, sent as `Authorization: Bearer`. | Sensitive data (real IPs, config) only ever leaves through guarded JSON. 401 → client shows login. Constant-time / min-32 / dual-header behavior reused verbatim (I-2). |
| **D8** | Cert expiry parsed by a new `src/certinfo.rs` using the **`x509-parser` crate** (pure-Rust, no unsafe in our code) over the already-loaded `CertificateDer`. | One new dependency (the only genuinely new dep). Parses `not_before`/`not_after`/subject/SAN → `days_remaining`. Cached by file mtime to avoid per-request parse. |
| **D9** | Process **memory** read via `procfs` (already a Linux-only dep) → `/proc/self/statm` RSS; **`None` / "N/A"** on non-Linux. **Cumulative bandwidth** via new global `AtomicU64` counters incremented alongside the existing per-entry atomics, plus a live sum over registries. | Memory section is Linux-first, degrades gracefully (I-8). Bandwidth survives tunnel teardown (per-entry atomics vanish on drop; globals don't). |
| **D10** | All view assembly is **synchronous snapshotting**: iterate registries, copy primitive/owned values into serde view structs, release DashMap guards, then serialize. **No DashMap guard or lock held across `.await`.** | No deadlock/perf risk on the data plane (I-7). Mirrors `AdminRegistry::snapshot()` `src/admin.rs:192`. |
| **D11** | Config view is **sanitized**: never serialize `admin_token`, auth secret, TLS private-key bytes, or basic-auth credentials. | Security: the config panel is safe to render even though the endpoint is guarded (defense in depth). Enforced by a unit test asserting forbidden keys absent (T-SANITIZE). |
| **D12** | All rendered data is **HTML-escaped** in JS (`escapeHtml` helper); asset server only serves exact keys from the embedded table (no filesystem, no path traversal); responses set a strict `Content-Security-Policy` (no inline-eval, no third-party origins). | Notes/SAN/headers are attacker-influenced strings → XSS-safe. Asset route cannot read arbitrary files. |

---

## 3. Target architecture

### 3.1 Backend: section views + assembler

New module `src/admin_views.rs` holds one `#[derive(Serialize)]` struct per section (mirroring the `EntryView` pattern at `src/admin.rs:102-136`):

- `SummaryView` — version string, control_port, tls, udp, vpn_enabled, vhost_enabled, server uptime_secs, per-section counts.
- `TunnelView` (public) / `SecretView` — reuse `EntryView` fields; split by role.
- `VhostView` — subdomain, active, carriers, direct_stream_opens, injected-header names (not values if sensitive), tls.
- `VpnLinkView` + `VpnPeerView` (cfg vpn) — link id/role, peer real IP, overlay CIDR, advertised routes, direct/relay, carriers, tx/rx; hub peers list.
- `CertView` — label (control/vhost), path, subject, sans, not_before, not_after (RFC3339), days_remaining (i64), expiring (bool, threshold 30d).
- `ConfigView` — sanitized startup config (D11).
- `MetricsView` — uptime_secs, mem_rss_bytes (Option), bandwidth_tx_bytes/rx_bytes (cumulative), live aggregate counts.

A new module `src/admin_api.rs` exposes one builder fn per section, e.g. `fn tunnels(server: &Server) -> Vec<TunnelView>`, each a synchronous snapshot (D10). `admin_http::serve()` routes `/admin/api/v1/<section>` → guard → builder → `serde_json` → 200/JSON.

### 3.2 Backend: config snapshot + uptime + counters

- `Server` gains additive fields (no behavior change):
  - `config_view: Arc<ConfigView>` — built once in `main.rs` where `Server` is constructed (`src/main.rs:1721-1854`), from the resolved `Args` (D11 sanitized).
  - `started_at: Instant` — set at construction; `uptime_secs` = `elapsed()`.
  - `total_tx_bytes: Arc<AtomicU64>`, `total_rx_bytes: Arc<AtomicU64>` — cumulative bandwidth (D9), `fetch_add`'d at the same sites that bump the per-`Entry` `relay_tx_bytes`/`relay_rx_bytes` atomics.
- Cert info: `certinfo::CertStatus` cache keyed by path, refreshed on mtime change (parallels the existing 2s reload poll `src/server.rs:568-595`; can piggyback or be lazily parsed on request with an mtime guard).

### 3.3 Backend: routing

In `admin_http::serve()` (`src/admin_http.rs:74`), extend the path match with three additive branches **before** the existing `/admin/status/data` arm (which stays untouched, D2):
1. `path == "/admin/status"` or `"/admin/"` → serve `index.html` from the asset table (replaces old HTML, D3).
2. `path.starts_with("/admin/ui/")` → asset lookup by exact key → bytes + content-type (D12).
3. `path.starts_with("/admin/api/v1/")` → `authorized()` guard (D7) → section builder (D1).

No change to `src/server.rs` dispatch (`serve_admin_http` `src/server.rs:873`) — the whole admin surface stays gated on `--admin-token` presence (I-3).

```
                 server.rs:887 admin_http::serve()
                          │
            ┌─────────────┼───────────────────────────────┐
   /admin/status,/admin/  /admin/ui/<path>      /admin/api/v1/<section>
   → index.html (asset)   → asset table          → authorized()? ──no→ 401
   (unguarded, no secret) (unguarded, no secret)        │yes
                                                  builder(server) → JSON
                          │
                /admin/status/data  ← UNCHANGED (D2, compat alias over tunnels view)
```

### 3.4 Frontend: panel-registry SPA

File tree (new `src/admin_ui/`, embedded by D4):

```
src/admin_ui/
  index.html        shell: <aside id=menu> + <main id=view> + token-login overlay
  app.js            bootstrap: build menu from registry, init router, token gate, polling
  router.js         hash router (#/<route>) → resolve panel → render
  api.js            fetch(endpoint) with Bearer; 401 → emit login event
  store.js          token store (sessionStorage), global refresh interval
  ui.js             helpers: table(), badge(), notesCell (truncate + expand-on-click),
                    escapeHtml, fmtBytes, fmtDuration, fmtRfc3339
  registry.js       imports every panel module → exports ordered array (THE registry)
  style.css         layout + theme
  panels/
    overview.js  tunnels.js  secret.js  vhost.js  vpn.js  certs.js  config.js  metrics.js
```

**Panel contract** (every panel, default export):
```js
export default {
  id: 'tunnels', title: 'Public tunnels', route: 'tunnels',
  endpoint: '/admin/api/v1/tunnels', refreshMs: 5000,
  render(el, data, ctx) { /* build DOM into el from JSON data */ }
}
```
`registry.js` is the **only** file that imports panels; the sidebar and router derive entirely from the array order. Adding a section: write `panels/foo.js`, add one import+entry to `registry.js`, add the matching API endpoint. No edits to `app.js`/`router.js`/`api.js` (I-6).

Notes expand: `ui.notesCell(text)` renders truncated with a toggle; click expands full text inline (escaped). Connection state, params, real IP are plain table columns fed by the JSON.

### 3.x Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Token auth guard | `authorized()` (Bearer/X-Admin-Token, min-32, constant-time) | `src/admin_http.rs:159-183` |
| Constant-time compare | `constant_time_eq()` | `src/basicauth.rs:139` |
| Public-tunnel snapshot + view | `AdminRegistry::snapshot()` / `EntryView` | `src/admin.rs:192-220`, `src/admin.rs:102-136` |
| Per-entry bytes/active/peer | `Entry` fields | `src/admin.rs:43-76` |
| Vhost registry iteration | `vhost_registry` DashMap | `src/server.rs:152`, `src/vhost.rs:439` |
| VPN links + hub peers | `vpn_providers` / `HubState.peers` | `src/vpn_server.rs:27,52-81,184-202` |
| Secret providers | `providers` / `udp_providers` | `src/server.rs:113,117`, `src/secret.rs:66-88` |
| Loaded cert chain / paths | control TLS `src/server.rs:129`; vhost paths `VhostConfig.cert_file/key_file` | `src/vhost.rs:48-75`; loader `src/transport.rs:196-200` |
| Server config source | resolved `Args` at construction | `src/main.rs:1721-1854` |
| Version string | `BORE_GIT_*` env (build.rs) | existing `build.rs` |
| Asset embed mechanism precedent | `include_str!` admin HTML | `src/admin_http.rs:19` (to be replaced by build.rs table) |
| Response/JSON helpers | existing `/admin/status/data` write path | `src/admin_http.rs:85-102` |
| HTTP first-byte / dispatch (DON'T touch) | `serve_admin_http` | `src/server.rs:819,873,887` |

---

## 4. New interface (endpoints / assets / config)

**HTTP routes** (all under the existing admin listener, gated on `--admin-token`):

| Method/Path | Guard | Returns |
|-------------|-------|---------|
| `GET /admin/status`, `GET /admin/` | none | `index.html` (SPA shell), `text/html` |
| `GET /admin/ui/<path>` | none | embedded asset, content-type by extension |
| `GET /admin/api/v1/summary` | Bearer | `SummaryView` JSON |
| `GET /admin/api/v1/tunnels` | Bearer | `[TunnelView]` |
| `GET /admin/api/v1/secret` | Bearer | `[SecretView]` |
| `GET /admin/api/v1/vhost` | Bearer | `[VhostView]` |
| `GET /admin/api/v1/vpn` | Bearer | `{links:[VpnLinkView]}` (empty when built w/o `--features vpn`) |
| `GET /admin/api/v1/certs` | Bearer | `[CertView]` |
| `GET /admin/api/v1/config` | Bearer | `ConfigView` (sanitized) |
| `GET /admin/api/v1/metrics` | Bearer | `MetricsView` |
| `GET /admin/status/data` | Bearer | **UNCHANGED** legacy snapshot (D2) |

No new CLI flags. The existing `--admin-token` / `BORE_ADMIN_TOKEN` (`src/main.rs:489`) gate the entire surface unchanged. Content-type map (asset server): `.html→text/html; charset=utf-8`, `.js→text/javascript`, `.css→text/css`, `.svg→image/svg+xml`, `.ico→image/x-icon`, fallback `application/octet-stream`. All API + asset responses carry a strict `Content-Security-Policy` header (D12).

---

## 5. New data structures

All additive; no wire/persisted protocol changes (the dashboard is read-only over in-memory state). Serde view structs in `src/admin_views.rs` (shapes summarized in §3.1). Additive `Server` fields (§3.2): `config_view`, `started_at`, `total_tx_bytes`, `total_rx_bytes`. New `Cargo.toml` dependency: `x509-parser` (D8). **Backward-compat:** `/admin/status/data` keeps its exact `ServerStatus`/`StatusView` serialization (`src/admin_http.rs:25-39`) — pinned by T-COMPAT. The `vpn` API field set is feature-gated so a non-`vpn` build returns an empty `links` array rather than failing to compile (I-4).

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase must pass the gates
(`cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
`cargo test --all-features`); **zero regressions**; update docs when behavior/APIs
change; **print the model used per sub-task**.

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Backend scaffolding (pure-additive, no behavior change)

> Lands API plumbing + additive state with no user-visible change. Safe to ship alone. `/admin/status` still serves the OLD HTML until Phase 3.

#### 0.1 View structs module
- **Model:** Haiku
- **Files:** new `src/admin_views.rs`; register `mod admin_views;` in `src/main.rs` (or lib root next to `mod admin_http;`).
- **Change:** Define the `#[derive(Serialize)]` view structs from §3.1 (`SummaryView`, `TunnelView`, `SecretView`, `VhostView`, `VpnLinkView`, `VpnPeerView`, `CertView`, `ConfigView`, `MetricsView`). Mirror `EntryView` style at `src/admin.rs:102-136`. No logic yet. Feature-gate the VPN structs with `#[cfg(feature = "vpn")]` mirroring `src/vpn_server.rs`.
- **Unit tests:** `t_views_serialize_stable` — each struct serializes to JSON with expected snake_case keys (serde round-trip on a hand-built instance).
- **e2e tests:** none (no behavior).
- **Done:** gates green; structs compile under all feature combos (`--no-default-features`, default, `--features vpn`).

#### 0.2 Additive Server fields: uptime + cumulative bandwidth counters
- **Model:** Sonnet
- **Files:** `src/server.rs:86` (struct), `src/server.rs:1721`-construction path / `src/main.rs:1721-1854` (init), the relay copy sites that bump `relay_tx_bytes`/`relay_rx_bytes` (find via grep on `relay_tx_bytes`).
- **Change:** add `started_at: Instant` (set at construction), `total_tx_bytes`/`total_rx_bytes: Arc<AtomicU64>`. At every site that does `entry.relay_tx_bytes.fetch_add(n,..)` also `server.total_tx_bytes.fetch_add(n,..)` (same for rx). Initialize counters to 0.
- **Unit tests:** `t_total_bytes_accumulate` — a helper increments both per-entry and global; assert global == sum of two entries' contributions.
- **e2e tests:** none.
- **Done:** gates green; **I-1/I-7 hold** — no change to existing data-plane behavior, only extra `fetch_add`s; existing `tests/admin_test.rs` passes unchanged.

#### 0.3 Sanitized config snapshot
- **Model:** Sonnet · **Opus review gate** (data-model + D11 sanitization correctness)
- **Files:** `src/admin_views.rs` (`ConfigView`), `src/main.rs:1721-1854` (build it), `src/server.rs:86` (store `config_view: Arc<ConfigView>`).
- **Change:** build `ConfigView` from resolved server config: port_range, control_port, max_conns, max_carriers, bind_addr, bind_tunnels, udp(+udp_tuning), vpn_enabled/vpn_max_links/vpn_hub_prefix, vhost_enabled, tls(bool only). **Never** include `admin_token`, auth secret, key material, basic-auth creds (D11).
- **Unit tests:** `t_config_sanitized` (**T-SANITIZE**) — serialize `ConfigView` to JSON, assert keys `admin_token`/`secret`/`key`/`password` are absent and `tls` is a bool not a path-to-key.
- **e2e tests:** none.
- **Done:** gates green; Opus confirms no secret reachable from `ConfigView`.

#### 0.4 API router branch + per-section endpoints (wired to real registries)
- **Model:** Sonnet · **Opus review gate** (touches request dispatch + auth)
- **Files:** new `src/admin_api.rs` (builder fns, §3.1); `src/admin_http.rs:74` (add the `/admin/api/v1/` branch with `authorized()` guard); keep `/admin/status/data` arm at `src/admin_http.rs:85-102` untouched.
- **Change:** implement synchronous builders (D10): `summary`, `tunnels` (reuse `AdminRegistry::snapshot()`), `secret`, `vhost` (iterate `vhost_registry`), `metrics` (uptime + counters + procfs mem via D9, `mem_rss_bytes: None` off-Linux), `config` (return stored `config_view`). VPN + certs builders are stubs returning empty here (filled in 1.x/2.x). Route → guard → `serde_json::to_vec` → 200; unauth → 401 (reuse the 401 path at `src/admin_http.rs:86-94`).
- **Unit tests:** in `tests/admin_test.rs`: `t_api_requires_token` (**T-AUTH**) — each `/admin/api/v1/*` returns 401 without token, 200 with valid Bearer **and** with `X-Admin-Token`; `t_api_tunnels_shape` — populated registry → JSON has `peer`, `public_port`, `active`, `notes`.
- **e2e tests:** deferred to 5.1.
- **Done:** gates green; **T-COMPAT** (added in 5.2) and existing admin tests pass; Opus confirms guard wraps every `v1` route and dispatch in `src/server.rs` is unchanged (I-3).

---

### Phase 1 — VPN section + metrics completion

> Fills the subsystem views that need feature gating / aggregation. Independently shippable (API-only).

#### 1.1 VPN view builder (links + hub peers)
- **Model:** Sonnet · **Opus review gate** (feature-gate correctness + VPN data model)
- **Files:** `src/admin_api.rs` (`vpn` builder, `#[cfg(feature="vpn")]`), `src/admin_views.rs` (`VpnLinkView`/`VpnPeerView`), reads `vpn_providers` `src/vpn_server.rs:27`, `HubState.peers` `src/vpn_server.rs:52-81`, `VpnProviderEntry` `src/vpn_server.rs:184-202`.
- **Change:** snapshot each VPN link → link id/role, peer real IP (`SocketAddr`), overlay CIDR, advertised routes, carriers, direct/relay state, tx/rx (relay bytes). For hub links, list peers (`peer_id`, overlay, real IP, advertised). Non-`vpn` build: builder returns `{links:[]}` (I-4) — compile both ways.
- **Unit tests:** `t_vpn_view_empty_without_feature` (cfg-gated) and `t_vpn_view_shape` (under `--features vpn`, a fake registry entry → expected fields).
- **e2e tests:** covered by 5.1 (run under `--features vpn` build).
- **Done:** gates green under default **and** `--features vpn`; Opus confirms no panic when hub/peers empty.

#### 1.2 Metrics finalize (mem + aggregate + bandwidth)
- **Model:** Sonnet
- **Files:** `src/admin_api.rs` (`metrics` builder), uses `procfs` (Linux) for RSS, `started_at`, `total_tx/rx_bytes`, registry counts.
- **Change:** `MetricsView{uptime_secs, mem_rss_bytes: Option<u64>, bandwidth_tx_bytes, bandwidth_rx_bytes, live_tunnels, live_vhost, live_vpn_links}`. RSS via `procfs::process::Process::myself()?.statm()` *Linux-gated*; `None` elsewhere (D9/I-8).
- **Unit tests:** `t_metrics_mem_optional` — on non-Linux target the field is `None`; `t_metrics_counts` — counts equal registry sizes.
- **e2e tests:** 5.1 asserts `uptime_secs>=0`, `bandwidth_tx_bytes>=0`.
- **Done:** gates green on Linux; compiles on macOS/Windows targets (CI `vpn-cross-build`/`transfer-paths` jobs stay green).

---

### Phase 2 — TLS certificate inspection

> Adds the only new dependency. Independently shippable (API-only).

#### 2.1 `certinfo` module — parse expiry
- **Model:** Sonnet · **Opus review gate** (new dep + security-adjacent parsing)
- **Files:** new `src/certinfo.rs`; `Cargo.toml` (add `x509-parser`).
- **Change:** `fn inspect(der: &CertificateDer, label: &str, path: Option<&Path>) -> CertView` — parse with `x509-parser`, extract subject CN, SANs, `not_before`/`not_after` → RFC3339, `days_remaining` (signed; negative = expired), `expiring = days_remaining <= 30`. Cache by `(path, mtime)` to avoid re-parse each request. No `unsafe` in our code (I confirm under `#![forbid(unsafe_code)]`).
- **Unit tests:** `t_cert_parse_fixture` — a checked-in test cert (`tests/fixtures/test_cert.pem`, far-future expiry) parses to expected `days_remaining>0`, correct CN; `t_cert_expired_fixture` — an expired fixture → `days_remaining<0`, `expiring==true`.
- **e2e tests:** 5.1 asserts integer `days_remaining` for the vhost cert.
- **Done:** gates green; `cargo audit` clean for the new dep; Opus reviews the dep + parse error handling (malformed cert → graceful error entry, not panic).

#### 2.2 certs endpoint wiring
- **Model:** Sonnet
- **Files:** `src/admin_api.rs` (`certs` builder), reads control TLS `src/server.rs:129` + vhost cert paths `VhostConfig.cert_file/key_file` `src/vhost.rs:48-75`.
- **Change:** assemble `[CertView]` for the control cert (if TLS on) and each vhost cert; surface the leaf cert of each chain. Missing/unreadable → entry with `error` field, never a 500.
- **Unit tests:** `t_certs_endpoint_lists_loaded` — server with a fixture cert → endpoint returns ≥1 entry with `days_remaining`.
- **e2e tests:** 5.1 (reference scenario cert assertion).
- **Done:** gates green; endpoint never panics on absent/garbage certs.

---

### Phase 3 — Frontend shell + asset embedding

> The big swap: build.rs bundler, asset serving, SPA shell. After this `/admin/status` serves the new UI (D3). Opus review on the modularity contract.

#### 3.1 build.rs asset bundler
- **Model:** Sonnet · **Opus review gate** (build system; affects every build)
- **Files:** existing `build.rs`; new dir `src/admin_ui/` (start with a placeholder `index.html`).
- **Change:** in `build.rs`, walk `src/admin_ui/` recursively, emit `${OUT_DIR}/admin_assets.rs` defining `pub static ADMIN_ASSETS: &[(&str, &[u8], &str)]` — each tuple `(url_path, include_bytes!(abs_path), content_type)`. `url_path` = `/admin/ui/<relpath>` (and the index mapped for `/admin/status`). Emit `cargo:rerun-if-changed=src/admin_ui` so edits retrigger. Keep the existing version-string logic intact.
- **Unit tests:** `t_assets_table_nonempty` — `include!`-d table contains the index entry and known content-types.
- **e2e tests:** none (covered indirectly by 3.2).
- **Done:** gates green; `cargo build` regenerates the table; adding a file to `src/admin_ui/` appears in the table after rebuild (manual check noted in docs). **I-5** (no runtime asset crate) holds.

#### 3.2 Asset + shell serving in admin_http
- **Model:** Sonnet · **Opus review gate** (request dispatch + path-traversal safety, D12)
- **Files:** `src/admin_http.rs` (`include!(concat!(env!("OUT_DIR"),"/admin_assets.rs"))`; add the `/admin/status`+`/admin/` shell arm and `/admin/ui/<path>` asset arm at the `src/admin_http.rs:74` match).
- **Change:** serve `index.html` for `/admin/status`,`/admin/`,`/admin`; serve assets by **exact key lookup** in `ADMIN_ASSETS` (no filesystem, no `..` join — D12) with the table's content-type; 404 on miss. Add the CSP header to these + the API responses. Delete the old `include_str!("admin_status.html")` (`src/admin_http.rs:19`) and `src/admin_status.html` once the shell renders (D3).
- **Unit tests:** `t_asset_exact_key_only` — request `/admin/ui/../secret` (and `%2e%2e`) → 404, never serves a non-table path; `t_shell_served` — `/admin/status` → 200 `text/html`.
- **e2e tests:** 5.1 (`/admin/status`→200 html, `/admin/ui/app.js`→200 js).
- **Done:** gates green; **behavior change called out:** `/admin/status` now serves the SPA shell instead of the legacy page (intended, D3); `/admin/status/data` still returns the legacy JSON (I-1, T-COMPAT).

#### 3.3 SPA core: shell, router, api, store, ui, registry
- **Model:** Sonnet · **Opus review gate** (the modularity contract I-6 + auth/token handling D7)
- **Files:** new `src/admin_ui/{index.html,app.js,router.js,api.js,store.js,ui.js,registry.js,style.css}`.
- **Change:** implement per §3.4 — sidebar generated from `registry.js`; hash router resolves the panel and calls `render`; `api.js` injects `Authorization: Bearer <token>` from `store.js` (`sessionStorage`), on 401 emits a login event and the shell shows the token field; `ui.js` provides `escapeHtml`, `notesCell` (truncate+expand), `fmtBytes`, `fmtDuration`, `fmtRfc3339`, `table`, `badge`; polling driven by each panel's `refreshMs`. `registry.js` starts with only the `overview` panel.
- **Unit tests:** none (JS, no node toolchain — covered by contract/DOM-free assertions in e2e). The Rust-side `t_shell_served` (3.2) covers serving.
- **e2e tests:** 5.1 asserts the shell loads and `app.js` is served; deeper UI flows are contract-level (token → 200, no-token → login state inferred from 401).
- **Done:** gates green; Opus confirms the registry is the **only** panel-aware file (router/app/api panel-agnostic) — I-6; token never logged, never placed in URL/localStorage (D7).

---

### Phase 4 — Section panels (one per menu item)

> Each panel is independently shippable; ship Overview first, then the rest in any order. All consume the Phase 0–2 endpoints. Mostly mechanical → Haiku where a panel mirrors an earlier one.

#### 4.1 Overview panel
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/overview.js`, register in `registry.js`.
- **Change:** consume `/admin/api/v1/summary`; show version, ports, feature flags, server uptime, per-section live counts as cards. `refreshMs: 5000`.
- **Unit tests:** none (JS).
- **e2e tests:** 5.1 — `/admin/api/v1/summary` 200 + keys present.
- **Done:** menu shows Overview; renders counts from live server.

#### 4.2 Public tunnels panel (real IP + notes expand)
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/tunnels.js`, `registry.js`.
- **Change:** table of public tunnels: public_port, **peer real IP:port**, https/basic_auth/udp badges, active, uptime, tx/rx (via `fmtBytes`), **notes via `ui.notesCell` (click-to-expand)**. `refreshMs: 5000`.
- **Unit tests:** none.
- **e2e tests:** **T-REF1** (reference scenario): tunnels JSON has entry with `public_port==P` and `peer` starting the client IP.
- **Done:** real IPs + expandable notes render; matches reference scenario.

#### 4.3 Secret tunnels panel
- **Model:** Haiku (mirrors 4.2)
- **Files:** `src/admin_ui/panels/secret.js`, `registry.js`.
- **Change:** like 4.2 over `/admin/api/v1/secret`; columns role, secret_id, peer, udp, basic_auth, notes(expand), active, uptime, bytes.
- **Unit tests:** none.
- **e2e tests:** 5.1 — endpoint 200 + shape.
- **Done:** secret tunnels listed with state.

#### 4.4 Vhost panel
- **Model:** Haiku (mirrors 4.2)
- **Files:** `src/admin_ui/panels/vhost.js`, `registry.js`.
- **Change:** table over `/admin/api/v1/vhost`: subdomain, active, carriers, direct opens, injected-header names, tls badge.
- **Unit tests:** none.
- **e2e tests:** 5.1 — endpoint 200 + shape.
- **Done:** vhost providers listed.

#### 4.5 VPN panel
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/vpn.js`, `registry.js`.
- **Change:** over `/admin/api/v1/vpn`: per link role, peer real IP, overlay CIDR, advertised routes, **direct/relay badge**, carriers, tx/rx; expandable hub-peer sub-table (peer_id, overlay, real IP, routes). Empty-state when no links / non-vpn build.
- **Unit tests:** none.
- **e2e tests:** 5.1 (run under `--features vpn`) — endpoint 200 + `links` array.
- **Done:** VPN links + hub peers render with path state.

#### 4.6 Certificates panel
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/certs.js`, `registry.js`.
- **Change:** over `/admin/api/v1/certs`: label, subject, SANs, not_after, **days_remaining with color state** (green / amber ≤30d / red expired), path. `refreshMs: 60000`.
- **Unit tests:** none.
- **e2e tests:** **T-REF2** — certs JSON vhost entry has integer `days_remaining` + RFC3339 `not_after`.
- **Done:** cert expiry + countdown visible; expiring/expired highlighted.

#### 4.7 Config panel
- **Model:** Haiku
- **Files:** `src/admin_ui/panels/config.js`, `registry.js`.
- **Change:** render `/admin/api/v1/config` as a key/value list (startup params). No refresh (`refreshMs: 0`). Relies on backend sanitization (D11).
- **Unit tests:** none.
- **e2e tests:** **T-REF3** — config JSON has `control_port`, lacks `admin_token`.
- **Done:** startup parameters visible; no secret rendered.

#### 4.8 Metrics panel
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/metrics.js`, `registry.js`.
- **Change:** over `/admin/api/v1/metrics`: server uptime, **memory RSS** (or "N/A (non-Linux)"), **cumulative bandwidth tx/rx** (`fmtBytes`), live aggregate counts. `refreshMs: 3000`.
- **Unit tests:** none.
- **e2e tests:** **T-REF4** — metrics JSON has `uptime_secs>=0`, `bandwidth_tx_bytes>=0`.
- **Done:** memory + bandwidth + uptime render and update on poll.

---

### Phase 5 — e2e harness, regression tests, docs

#### 5.1 Contract e2e script
- **Model:** Sonnet · **Opus review gate** (acceptance assertions define "done")
- **Files:** new `scripts/admin_dashboard_test.sh` (mirror the structure/guards of `scripts/local_proxy_netns_test.sh` — stale-binary check, `sudo -n`, pass/fail counter).
- **Change:** boot a server (`--admin-token`, a TLS vhost using a fixture cert, `--udp`), a client public tunnel from a known netns IP, then curl-assert the full reference-scenario table in §1 (HTML 200, asset 200+type, every `v1` endpoint 401-without/200-with token, `tunnels` peer+port, `certs` days_remaining, `config` no-admin_token, `metrics` fields, **`/admin/status/data` legacy shape**). Build under `--features vpn` so the VPN endpoint is exercised. Print model used.
- **Unit tests:** n/a (this is the e2e harness).
- **e2e tests:** **T-REF1..T-REF4** + T-AUTH-E2E + T-COMPAT-E2E live here.
- **Done:** script exits 0, all assertions pass against a freshly built `--release --features vpn` binary; documented `sudo -n /abs/path/scripts/admin_dashboard_test.sh` invocation.

#### 5.2 Regression + compat unit tests
- **Model:** Sonnet
- **Files:** `tests/admin_test.rs` (extend), `tests/fixtures/` (test certs).
- **Change:** add **T-COMPAT** — assert `/admin/status/data` JSON keys/types match the pre-change `ServerStatus`/`StatusView` (`src/admin_http.rs:25-39`) exactly (pin with an inline expected key set); ensure T-AUTH (0.4), T-SANITIZE (0.3), cert fixtures (2.1) are wired.
- **Unit tests:** T-COMPAT, plus the ones referenced in earlier phases collected/green.
- **e2e tests:** n/a.
- **Done:** `cargo test --all-features` green; **I-1 proven** by T-COMPAT.

#### 5.3 Documentation
- **Model:** Haiku · **Opus final read gate**
- **Files:** new `docs/frontend/ADMIN_DASHBOARD.md`; update `docs/CHANGELOG.md`; note in `CLAUDE.md` invariants if warranted.
- **Change:** document architecture (API map, asset pipeline, panel contract), **"how to add a new section in 3 steps"** (panel module → registry line → API endpoint), the auth model, the cross-platform memory caveat, and how to run `scripts/admin_dashboard_test.sh`.
- **Unit tests:** n/a.
- **e2e tests:** n/a.
- **Done:** Opus reads the doc; a reader can add a section without reading source; CHANGELOG updated.

---

## 7. Invariants to preserve / add

- **I-1:** `/admin/status/data` response is **byte-shape identical** to pre-change (D2). Pinned by **T-COMPAT** (5.2).
- **I-2:** Auth unchanged — constant-time, min-32, both `Authorization: Bearer` and `X-Admin-Token`; **every** `/admin/api/v1/*` + `/admin/status/data` guarded; HTML shell + assets unguarded (no secrets). Reuses `authorized()` `src/admin_http.rs:159-183`. Pinned by **T-AUTH**.
- **I-3:** When `--admin-token` is unset, the entire admin HTTP surface returns 404 exactly as today (dispatch `src/server.rs:873` unchanged).
- **I-4:** A build **without** `--features vpn` compiles and the `vpn` endpoint returns an empty `links` array (no panic, no cfg leak).
- **I-5:** **Zero new runtime asset dependency** — embedding is a `build.rs`-generated static table (D4). The only new crate dep is `x509-parser` (D8).
- **I-6 (modularity contract):** Adding a section requires only (a) a new `src/admin_ui/panels/*.js`, (b) one line in `registry.js`, (c) one new `/admin/api/v1/*` endpoint + view. **No edits** to `app.js`/`router.js`/`api.js`/the dispatch core. Verified by Opus review at 3.3 and documented in 5.3.
- **I-7:** Dashboard is **read-only** over existing registries/atomics; **no DashMap guard / lock held across `.await`** in any builder (D10). Data plane untouched.
- **I-8:** Memory metric is best-effort: real RSS on Linux (procfs), `None`/"N/A" elsewhere; never a build break on macOS/Windows targets (CI cross jobs stay green).
- **I-9:** `#![forbid(unsafe_code)]` preserved — no `unsafe` in any new module; deps may use it internally.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Holding a `DashMap` ref across `await` → stall/deadlock on the live data plane | D10/I-7: synchronous snapshot into owned view structs before any serialize/await; mirrors `AdminRegistry::snapshot()`. Opus review at 0.4. |
| XSS via attacker-controlled notes / SAN / header names | D12: `escapeHtml` on every rendered string; strict CSP; no third-party JS. |
| Path traversal in the asset route | D12: exact-key lookup in the embedded table only, no filesystem join. Unit test `t_asset_exact_key_only` (3.2). |
| Config endpoint leaking a token/secret | D11 sanitization + **T-SANITIZE** (0.3) asserting forbidden keys absent; Opus review gate. |
| Breaking the legacy `/admin/status/data` contract | D2 + **T-COMPAT** (5.2) pins exact JSON shape. |
| New `x509-parser` dep flagged by `cargo audit` / pulls unsafe | D8: pure-Rust crate; `cargo audit` runs in the gate; our code stays `unsafe`-free (I-9); graceful parse-error path (2.1). |
| macOS/Windows build break from procfs/vpn cfg | I-4/I-8: feature- and target-gated; CI `vpn-cross-build` + `transfer-paths` jobs validate cross-compile. |
| `build.rs` asset table missing a newly added file | `cargo:rerun-if-changed=src/admin_ui` (3.1) + documented rebuild note (5.3); `t_assets_table_nonempty`. |
| Frontend regressions hard to catch without a browser | Decision D6 + contract e2e (5.1) assert the API + serving layer (the regression-prone surface); UI logic kept thin and data-driven. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo build --all-features`, `cargo test --all-features`, `cargo audit` (for the new dep).
- **Unit tests:** inline `#[cfg(test)]` + `tests/admin_test.rs`. Key IDs: `t_views_serialize_stable` (0.1), `t_total_bytes_accumulate` (0.2), **T-SANITIZE** `t_config_sanitized` (0.3), **T-AUTH** `t_api_requires_token` + `t_api_tunnels_shape` (0.4), `t_vpn_view_*` (1.1), `t_metrics_*` (1.2), `t_cert_parse_fixture`/`t_cert_expired_fixture` (2.1), `t_certs_endpoint_lists_loaded` (2.2), `t_assets_table_nonempty` (3.1), `t_asset_exact_key_only`/`t_shell_served` (3.2), **T-COMPAT** `t_status_data_compat` (5.2). Cert fixtures in `tests/fixtures/`.
- **e2e:** `scripts/admin_dashboard_test.sh` (5.1) — netns + curl, requires a freshly built `cargo build --release --features vpn` binary, invoked `sudo -n /abs/path/scripts/admin_dashboard_test.sh`. Carries **T-REF1..T-REF4**, T-AUTH-E2E, T-COMPAT-E2E.
- **Acceptance:** the §1 reference scenario passes — **T-REF1** (tunnels real IP+port), **T-REF2** (cert days_remaining), **T-REF3** (config sanitized), **T-REF4** (metrics fields), plus 401/200 auth and legacy `/admin/status/data` shape.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 Haiku · 0.2 Sonnet · 0.3 Sonnet · 0.4 Sonnet | Sonnet | 0.3, 0.4 |
| 1 | 1.1 Sonnet · 1.2 Sonnet | Sonnet | 1.1 |
| 2 | 2.1 Sonnet · 2.2 Sonnet | Sonnet | 2.1 |
| 3 | 3.1 Sonnet · 3.2 Sonnet · 3.3 Sonnet | Sonnet | 3.1, 3.2, 3.3 |
| 4 | 4.1 Sonnet · 4.2 Sonnet · 4.3 Haiku · 4.4 Haiku · 4.5 Sonnet · 4.6 Sonnet · 4.7 Haiku · 4.8 Sonnet | Sonnet/Haiku | — |
| 5 | 5.1 Sonnet · 5.2 Sonnet · 5.3 Haiku | Sonnet | 5.1, 5.3 (final read) |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical panels/prose
> (4.3/4.4/4.7/0.1/5.3), escalate to Opus only for the review gates above
> (dispatch/auth, data-model, build system, modularity contract, acceptance
> assertions, final docs). Print the model used per sub-task during implementation.
