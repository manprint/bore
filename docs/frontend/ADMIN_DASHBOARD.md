# Admin Dashboard — Architecture & Operations

**Status:** Phase 5 complete (2026-06-18). All API endpoints live, legacy compat verified, e2e harness ready.

## Overview

The admin dashboard is a **modular, token-guarded, real-time monitoring SPA** served at `/admin/status` and `/admin/` when `--admin-token` is set on the control port. It surfaces all live subsystems (public tunnels, secret tunnels, vhost, VPN, certificates, config, metrics) in independent **panels**, each powered by a versioned REST API under `/admin/api/v1/`.

**Key properties:**
- **Zero data-plane changes** — read-only snapshot over existing registries + atomics.
- **Backward-compatible** — legacy `/admin/status/data` endpoint unchanged (pinned by T-COMPAT).
- **Modular** — adding a new section requires only 3 steps (see "How to add a section").
- **Secure** — all sensitive endpoints token-guarded; HTML shell + assets unguarded; CSP headers; HTML-escaped output.
- **Cross-platform** — Linux memory metric, graceful "N/A" on macOS/Windows; VPN feature-gated.

---

## Architecture

### Backend: API endpoints + views

The server exposes **8 REST endpoints** under `/admin/api/v1/`, all guarded by the `--admin-token` (min-32 chars, Bearer or X-Admin-Token header):

| Endpoint | Returns | Purpose |
|----------|---------|---------|
| `/admin/api/v1/summary` | `SummaryView` JSON | Version, control port, feature flags, server uptime, live section counts |
| `/admin/api/v1/tunnels` | `[TunnelView]` | Public tunnel list: port, real peer IP, status, notes (expandable), tx/rx bytes |
| `/admin/api/v1/secret` | `[SecretView]` | Secret (relay) tunnels: secret ID, peer, role, status, notes, bytes |
| `/admin/api/v1/vhost` | `[VhostView]` | Vhost providers: subdomain, carrier pool, direct/relay metrics |
| `/admin/api/v1/vpn` | `{links:[VpnLinkView]}` | VPN links + hub peers (Linux `--features vpn` only; empty array in non-VPN builds) |
| `/admin/api/v1/certs` | `[CertView]` | TLS certificates: subject, SANs, expiry, days-remaining, path |
| `/admin/api/v1/config` | `ConfigView` | Sanitized startup config (no secrets, tokens, or keys) |
| `/admin/api/v1/metrics` | `MetricsView` | Uptime, RSS memory (Linux), cumulative bandwidth, live counts |

All responses:
- **Content-Type:** `application/json; charset=utf-8`
- **CSP header:** Strict, no inline-eval, no third-party origins
- **No guarding:** `/admin/status` (HTML shell), `/admin/ui/*` (assets) served without token

### Backend: Implementation structure

**Files:**
- `src/admin_views.rs` — view struct definitions (§0.1 Phase 0)
- `src/admin_api.rs` — synchronous snapshot builders for each endpoint (§0.4 Phase 0, 1.x Phase 1, 2.2 Phase 2)
- `src/certinfo.rs` — X.509 cert parsing + expiry calculation (§2.1 Phase 2)
- `src/admin_http.rs` — routing, asset serving, legacy compat (§3.2 Phase 3)
- `src/server.rs` — additive fields: `started_at`, `total_tx_bytes`, `total_rx_bytes`, `config_view` (§0.2 Phase 0)
- `build.rs` — asset bundler: generates `admin_assets.rs` with embedded `src/admin_ui/*` files (§3.1 Phase 3)

**Key invariants:**
- **Synchronous snapshots (D10/I-7):** every builder is synchronous; no DashMap guard held across `.await`. Data-plane untouched.
- **Backward compat (D2/I-1):** `/admin/status/data` serializes identically to pre-change `ServerStatus`/`StatusView`. Pinned by **T-COMPAT** (§5.2).
- **Asset embedding (D4/I-5):** static table from `build.rs`, no runtime asset dependency; reuses `include_bytes!`.
- **Sanitization (D11/T-SANITIZE):** `ConfigView` never exposes `admin_token`, auth secrets, TLS keys, or basic-auth credentials.

### Frontend: SPA shell + panel registry

Embedded under `src/admin_ui/`:

```
src/admin_ui/
  index.html        — SPA root: <aside id=menu> + <main id=view> + login overlay
  app.js            — bootstrap: menu from registry, router, polling, token gate
  router.js         — hash router (#/<route>) → panel resolution + render
  api.js            — fetch() wrapper: Bearer injection, 401 → login event
  store.js          — sessionStorage token store
  ui.js             — helpers: escapeHtml, notesCell, fmtBytes, fmtDuration, fmtRfc3339, table, badge
  registry.js       — THE panel registry: ordered array of all panel objects
  style.css         — layout + theme
  panels/
    overview.js     — version, counts, uptime, feature flags
    tunnels.js      — public tunnels table
    secret.js       — secret tunnels table
    vhost.js        — vhost providers table
    vpn.js          — VPN links + hub peers (feature-gated)
    certs.js        — cert expiry tracking + countdown
    config.js       — startup parameters (key/value, no refresh)
    metrics.js      — memory, bandwidth, aggregate counts
```

**Panel contract** (every panel module exports):
```js
export default {
  id: 'tunnels',              // stable identifier
  title: 'Public tunnels',     // sidebar label
  route: 'tunnels',           // hash route: #/tunnels
  endpoint: '/admin/api/v1/tunnels',  // API URL
  refreshMs: 5000,            // poll interval (0 = no refresh)
  render(el, data, ctx) {     // render(parentElement, jsonData, context)
    // ... build DOM from data
  }
}
```

**The registry** (`registry.js`) is the **only** file that imports panels:
```js
import overview from './panels/overview.js';
import tunnels from './panels/tunnels.js';
// ... (one import per panel)

export default [overview, tunnels, secret, vhost, vpn, certs, config, metrics];
```

The sidebar and router are **generated from the registry array** — no manual route table. Adding a section requires only:
1. Write `src/admin_ui/panels/foo.js` implementing the panel contract
2. Add one import + entry in `registry.js`
3. Add the matching `/admin/api/v1/foo` endpoint in `src/admin_api.rs`

### Auth model (D7/I-2)

- **Token:** min-32 chars, set via `--admin-token` flag or `BORE_ADMIN_TOKEN` env; must be constant-time compared.
- **API routes** (`/admin/api/v1/*`) + **legacy data** (`/admin/status/data`): Bearer `Authorization: Bearer <token>` or `X-Admin-Token: <token>` header required; 401 if missing/wrong.
- **Shell + assets:** unguarded, served without token (no secrets in static HTML/JS/CSS).
- **Client flow:** user opens `/admin/status` → shell loads (unguarded); user enters token in login field; token stored in `sessionStorage` (never localStorage, never URL); all API fetches inject `Authorization: Bearer` header; 401 response emits a login event and shell re-prompts.
- **CSP:** strict — no inline eval, no third-party JS, only self-hosted assets.

---

## How to add a section in 3 steps

### Step 1: Create the API endpoint (`src/admin_api.rs`)

Add a new builder function that returns a serde-serializable view struct:

```rust
use serde::Serialize;

#[derive(Serialize)]
pub struct MyNewView {
    pub field1: String,
    pub field2: u64,
}

pub fn my_new_section(server: &Server) -> MyNewView {
    // Synchronous snapshot over existing state
    MyNewView {
        field1: "example".into(),
        field2: 42,
    }
}
```

Wire it into the router in `admin_http.rs`:
```rust
// In the /admin/api/v1/ branch:
"/admin/api/v1/mynew" => {
    let view = admin_api::my_new_section(&server);
    return Response::json(serde_json::to_vec(&view)?);
}
```

### Step 2: Write the panel (`src/admin_ui/panels/mynew.js`)

```javascript
export default {
  id: 'mynew',
  title: 'My New Section',
  route: 'mynew',
  endpoint: '/admin/api/v1/mynew',
  refreshMs: 5000,
  render(el, data, ctx) {
    el.innerHTML = `<div class="panel">
      <h2>${escapeHtml(data.field1)}</h2>
      <p>Count: ${data.field2}</p>
    </div>`;
  }
};
```

### Step 3: Register in `src/admin_ui/registry.js`

Add one import and entry:
```javascript
import mynew from './panels/mynew.js';
// ... (other imports)

export default [
  overview, tunnels, secret, vhost, vpn, certs, config, metrics,
  mynew  // ← new section
];
```

**That's it.** No router edits, no `app.js` changes, no build step. The sidebar auto-generates from the registry.

---

## Asset pipeline

**Build time (`build.rs`):**
1. Walk `src/admin_ui/` recursively
2. For each file, determine content-type (`.html` → `text/html; charset=utf-8`, `.js` → `text/javascript; charset=utf-8`, etc.)
3. Generate `${OUT_DIR}/admin_assets.rs` defining:
   ```rust
   pub static ADMIN_ASSETS: &[(&str, &[u8], &str)] = &[
       ("/admin/ui/index.html", include_bytes!("..."), "text/html; charset=utf-8"),
       ("/admin/ui/app.js", include_bytes!("..."), "text/javascript; charset=utf-8"),
       // ...
   ];
   ```

**Runtime (`admin_http.rs`):**
1. `include!` the generated table
2. Route `/admin/ui/<path>` → exact-key lookup in the table (no filesystem, no `..` joins — D12 path-traversal safe)
3. Missing keys → 404

**When to rebuild:**
- Editing a file in `src/admin_ui/` → run `cargo build` again; `cargo:rerun-if-changed=src/admin_ui` ensures it retriggers.
- Adding a new file → same; just drop it in `src/admin_ui/` and rebuild.

---

## Cross-platform notes

### Memory metric (`MetricsView.mem_rss_bytes`)

- **Linux:** real RSS from `/proc/self/statm` via the `procfs` crate
- **macOS/Windows:** `None` (JSON `null`), displayed as "N/A" on the metrics panel
- **Compilation:** target-gated to avoid `procfs` build failures on non-Linux

### VPN endpoint (`/admin/api/v1/vpn`)

- **With `--features vpn`:** full `VpnLinkView[]` data + hub peers
- **Without (default):** empty `{links:[]}` (no panic, no cfg leak)
- **Metrics panel:** hides the VPN card if the build lacks `--features vpn`

All targets (Linux, macOS, Windows) compile without errors or regressions.

---

## Running the e2e test harness

The script `scripts/admin_dashboard_test.sh` validates the full reference scenario (§1 in `ADMIN_DASHBOARD_PLAN.md`):

```bash
# Build the release binary with VPN enabled (as your user, NOT root):
cargo build --release --features vpn

# Run the e2e harness (requires NOPASSWD sudo):
sudo -n /path/to/scripts/admin_dashboard_test.sh
```

**What it tests:**
- Stale-binary guard (aborts if `src/` is newer than the binary)
- Netns topology: server in `ns0`, nssvc with admin listener in `nssvc`, client in `nscli`
- Fresh self-signed vhost TLS cert generation
- Public tunnel assignment and real peer IP tracking
- T-SHELL: `/admin/status` → 200 text/html
- T-ASSET: `/admin/ui/app.js` → 200 text/javascript
- T-AUTH-E2E: API endpoints 401 without token, 200 with Bearer
- T-REF1: tunnels entry has assigned port + client IP
- T-REF2: certs entry has integer `days_remaining` + RFC3339 `not_after`
- T-REF3: config has `control_port`, lacks `admin_token`
- T-REF4: metrics has `uptime_secs >= 0`, `bandwidth_tx_bytes >= 0`
- T-COMPAT-E2E: legacy `/admin/status/data` shape (server + tunnels)

Exit code 0 = all pass; nonzero = failure(s). Uses `jq` for JSON if available, falls back to `grep`.

**Note:** The harness requires `sudo -n` (no-password sudo) configured in `sudoers`. It is designed for CI/CD; local runs may require password entry or manual `sudo` invocation.

---

## Invariants & guarantees

| # | Guarantee | How verified |
|----|-----------|--------------|
| **I-1** | `/admin/status/data` byte-shape identical (D2) | Unit test **T-COMPAT** (§5.2) |
| **I-2** | Auth unchanged: Bearer/X-Admin-Token, min-32, constant-time | **T-AUTH** unit + T-AUTH-E2E script |
| **I-3** | When `--admin-token` unset, entire admin surface 404 (zero behavior change) | Existing test `admin_disabled_does_not_serve_http()` |
| **I-4** | Non-VPN builds compile; `/admin/api/v1/vpn` returns empty array | Unit tests under both feature configs |
| **I-5** | Zero new runtime asset dependencies (only `build.rs` embedding) | `build.rs` uses only `include_bytes!` |
| **I-6** | Adding a section: panel + registry line + endpoint only (no router edits) | Modularity contract + docs above |
| **I-7** | Dashboard is read-only, no locks held across `.await` | Code review; data plane untouched |
| **I-8** | Memory metric best-effort (Linux real RSS, `None` elsewhere) | CI cross-compile jobs stay green |
| **I-9** | `#![forbid(unsafe_code)]` preserved in bore | Zero `unsafe` blocks in new modules |

---

## Test suite

**Unit tests** (run with `cargo test --all-features`):
- `t_views_serialize_stable` — serde round-trip
- `t_config_sanitized` — **T-SANITIZE**: admin_token absent
- `t_api_requires_token` — **T-AUTH**: 401/200 guard
- `t_api_tunnels_shape` — JSON has expected keys
- `t_vpn_view_*` — feature-gated VPN tests
- `t_metrics_mem_optional` — memory field optional on non-Linux
- `t_cert_parse_fixture` — cert parsing works
- `t_asset_exact_key_only` — path-traversal safe
- `t_shell_served` — shell 200 text/html
- `t_legacy_data_compat` — **T-COMPAT**: legacy shape pinned (§5.2)

**Frontend unit tests** (zero npm deps — Node's built-in test runner + a tiny
DOM stub; see `test/admin_ui/`):
```bash
npm test          # === node --test "test/admin_ui/**/*.test.js"
```
- `smoke.test.js` — harness/import sanity (`fmtBytes`, `badge`)
- `poller.test.js` — `T-FE-POLL0`: browser-safe timer binding + restart/stop semantics
- `app-polling.test.js` — `T-FE-POLL1`: real `app.js` bootstrap/hashchange smoke under browser-like timer receivers
- `metrics-rate.test.js` — **BUG-5**: `rateFromSamples` delta math + NaN/Inf guards
- `notes.test.js` — **BUG-2**: short notes plain, long notes click-to-expand
- `badges.test.js` — **BUG-3**: all flags surface as badges; only CSS-defined kinds
- `dom-stub.js` — minimal headless DOM (not a real DOM; only what the code touches)

> The DOM stub + tests live in `test/` (NOT `src/admin_ui/`), so `build.rs` does
> not embed them. The repo-root `package.json` only sets `"type":"module"` so
> Node treats the existing ES-module `.js` as ESM; it is dev-only.

**e2e test** (run with `sudo -n /path/scripts/admin_dashboard_test.sh`):
- T-REF1 .. T-REF4 — reference scenario assertions
- T-AUTH-E2E, T-COMPAT-E2E — contract-level acceptance
- **T-BUG1** — per-tunnel TX/RX > 0 after a real transfer (was always 0)
- **T-BUG3** — all-flags tunnel exposes `carriers`/`force_https`/`auto_reconnect`/`notes`
- **T-BUG4** — certs are deduped (single entry, no duplicate card)

> **Rebuild caveat:** the served JS is embedded at build time. After editing any
> `src/admin_ui/*` file, rebuild (`cargo build --release --features vpn`) before
> running the netns e2e — the harness refuses a binary older than `src/`.

All gates:
```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all-features
npm test                     # frontend unit tests
cargo audit  # for new x509-parser dep
```

---

## Changelog entry

See `docs/CHANGELOG.md` for the Phase 5 entry added to the unreleased section.

---

## Bug fixes (2026-06-18)

A bug-hunt pass over `/admin/status` found and fixed seven defects (plan:
`docs/frontend/ADMIN_DASHBOARD_BUGFIX_PLAN.md`).

| Bug | Layer | Root cause | Fix |
|-----|-------|-----------|-----|
| **BUG-0** auto-refresh dead | frontend | `app.js` dispatched a `panel:refresh` event **no one listened to** | `poller.js` owns the timer and calls `router.refreshCurrent()` directly; the dead event is gone |
| **BUG-1** TX/RX always `0` | backend | per-tunnel `relay_tx_bytes`/`relay_rx_bytes` were **never incremented** (only the global counters were) | `fetch_add(Relaxed)` at the existing post-splice sites (`server.rs`, `secret.rs`) — see I-PERF |
| **BUG-2** notes fake link | frontend | `notesCell` set the clickable class unconditionally but only attached the handler when truncated | clickable affordance + handler **only** for truncated notes; short notes are plain text (`.notes-plain`) |
| **BUG-3** flags missing | both | `force_https` wasn't rendered; `carriers` was dropped at registration; `auto_reconnect` was client-only | `carriers`+`auto_reconnect` threaded into the admin record (the latter added to `TunnelOptions`, `#[serde(default)]`); `tunnelBadges()` renders every flag |
| **BUG-4** cert shown twice | backend | `certs()` pushed the control **and** vhost cert even when they are the same file | `dedup_merge_label()` merges into one card (label `control+vhost`) by canonical path |
| **BUG-5** metrics wrong/stale | both | "stale" = BUG-0; "wrong" = cumulative totals mislabeled "Bandwidth" | relabeled **Total TX/RX** + a derived **Rate TX/RX** (`rateFromSamples`, Δbytes/Δt across polls) |
| **BUG-6** other panels | both | flag-sweep gap | `SecretView` now carries + renders `carriers` |

### TX/RX semantics & the performance guarantee (I-PERF)

The per-tunnel and global byte counters are summed **off the per-byte hot path**:
a single `AtomicU64::fetch_add(.., Relaxed)` runs **once per closed proxied
connection**, from the `(rx, tx)` totals that `copy_bidirectional_with_sizes`
already returns — never per byte, never under a lock. Measurement therefore does
**not** reduce tunnel throughput (verified: the data path is unchanged; the add is
post-splice). Direction (server's perspective): `relay_rx_bytes` = ingress from
the visitor, `relay_tx_bytes` = egress to the visitor — matching the global
`grx`/`gtx` mapping. The metrics **rate** is computed on the frontend from two
cumulative samples, so the server keeps no rate state.

`auto_reconnect` is sent over the wire purely so the admin page can show it; the
server takes no action on it. `#[serde(default)]` keeps the `TunnelOptions` wire
format backward-compatible (old client ↔ new server).

---

## Round-2 bug-fix (2026-06-19)

A second bug-hunt pass applied fixes for overview counts, config completeness, and
detail modals (plan: `docs/frontend/ADMIN_DASHBOARD_BUGFIX2_PLAN.md`).

| Fix | Layer | Root cause | Result |
|-----|-------|-----------|--------|
| **Summary counts renamed** | backend | `SummaryView` exposed `live_tunnels`/`live_vhost`/`live_vpn_links` — overview panel had no matching field names | replaced with `public_tunnels`, `secret_tunnels`, `vhost_domains`, `vpn_links` (exact role-based counts); overview displays non-zero counts |
| **Config buffers hardcoded null** | backend | `/admin/api/v1/config` hardcoded `udp_socket_send_buffer` / `udp_socket_recv_buffer` to `None` despite CLI flags being parsed | wired parsed values to `ConfigView` as `Option<usize>`; `None` → frontend label "auto (OS default)" |
| **Config missing tuning fields** | backend | operator-visible startup parameters (`--udp-stream-receive-window`, `--bind-domain`, `--control-hsts`, etc.) never reached the API | added `udp_stream_receive_window`, `udp_connection_receive_window`, `udp_send_window` (human-size strings), `udp_max_streams`, `bind_domain`, `control_hsts`, `vhost_mode`, `vhost_quic_port`, `vpn_punch_timeout` (vpn feature) to `ConfigView`; config panel renders them auto-magically |
| **Detail modals missing** | frontend | tunnels/secret/vhost/vpn table rows were compact; clicking a row showed nothing | added reusable `modal.js` component (`openModal`/`closeModal`); row-click opens modal showing all per-entry fields via `detailRows(obj)` formatter (byte/duration/bool/array/null handling) |
| **Backend gaps for modal** | backend | detail views lacked some per-entry fields needed by the modal | added `notes` to `SecretView`; added `request_header_pairs`/`response_header_pairs` (full key+value) and `direct_pool` size to `VhostView` |
| **Polling interval fragmented** | frontend | overview/tunnels/secret/vhost/vpn had per-panel hardcoded `refreshMs` (5000, 3000, etc.); data stale unless manually reloaded | unified to single `DEFAULT_REFRESH_MS = 30000` (30 s) across all data panels; config stays 0 (static); **rebuild required after editing `src/admin_ui/*`** — assets are compile-time embedded |

### Key implementation details

- **Summary:** `public_tunnels` = count of `Role::Public`; `secret_tunnels` = count of `Role::SecretProvider` + `Role::SecretConsumer`; `vhost_domains` = vhost registry len; `vpn_links` = vpn registry len (vpn feature).
- **Config:** socket buffers parse via existing human-size parser ("16MiB" → bytes); stored as `Option<usize>` (null when flag unset). Window strings stored verbatim. **Sanitization invariant:** no field name contains `secret|key|password|admin_token` (test **T-SANITIZE** enforces on all new fields).
- **Modal:** `detailRows(obj)` uses key suffixes (`*_bytes` → `fmtBytes`, `*_secs` → `fmtDuration`), type checks (bool → badge, array → join, null → "—"), and HTML escaping. Modal attaches to `document.body` so poll re-renders of `#view` don't destroy it.
- **Vhost headers:** `request_header_pairs` / `response_header_pairs` expose full header VALUES (not just keys); admin-token-guarded, no unauth path — documented security note.
- **Polling & rebuild:** `/admin/ui/*.js` are embedded at compile time via `build.rs`. A freshly rebuilt binary auto-refreshes every 30 s. **Operator note:** after editing any `src/admin_ui/*` file, rebuild the binary (`cargo build`) before serving — the JavaScript in the running process is from the last build, not from disk.

### Test IDs

**JS unit** (`test/admin_ui/**/*.test.js`):
- `T-MODAL` — `openModal`/`closeModal` create/destroy modal overlay
- `T-DETAIL` — `detailRows` formats each field type correctly
- `T-POLL30` — poller fires repeatedly at 30 s intervals
- `T-CFGNULL` — config null values render as friendly label
- `T-OVR` — overview panel reads correct field names from summary

**Rust unit** (`src/admin.rs` mod, `tests/admin_test.rs`):
- `T-SUM` — per-role count mapping; old field names gone
- `T-BUF` — socket buffers serialize to bytes, not null when set
- `T-CFG` — new ConfigView fields present + no secrets in names
- `T-SECN` — SecretView carries `notes`
- `T-VHH` — VhostView has `request_header_pairs`/`response_header_pairs`

**e2e** (`scripts/admin_dashboard_test.sh`):
- `T-SUMCOUNT` — server with public + secret tunnels → `/api/v1/summary` has non-zero counts
- `T-CFGBUF` — server with `--udp-socket-send-buffer 16MiB` → config reports `16777216`
- `T-CFGFIELDS` — `/api/v1/config` includes `udp_stream_receive_window`, `bind_domain`, `control_hsts`, `vhost_mode`, `vhost_quic_port`
- `T-SECNOTES` — `/api/v1/secret` first entry has key `notes`

---

## Round-3 staging hardening (2026-06-19)

A third hardening pass applied operator-facing visibility improvements: control-plane
clarity, per-link VPN state surfacing, debug metrics, and security tightening
(plan: `docs/frontend/ADMIN_DASHBOARD_ASSESSMENT.md`, phases 0–5).

| Fix | Layer | Root cause | Result |
|-----|-------|-----------|--------|
| **Overview: Listeners & Ports card** | backend + frontend | operator cannot see at a glance how the server was started (vhost HTTP/HTTPS/QUIC ports, tunnel bind address, port range) — must read Config | `SummaryView` += `vhost_http_port`/`vhost_https_port`/`vhost_quic_port`/`port_range`/`bind_tunnels` (all `Option`; vhost fields `None` when vhost disabled); new card renders control port, range, bind address, vhost listening ports when vhost enabled |
| **VPN per-link path & mode** | backend + frontend | per-link view shows `relay: true` (always, hardcoded) — tells operator nothing about whether direct is active/available; no mode indicator (1:1 vs hub); no uptime | `VpnLinkView` replaces `relay: bool` with `path: "direct"\|"relay"` (derived from `vpn_direct`); adds `uptime_secs`, `mode: "1:1"\|"hub"` (from `hub_peers`), keeps `direct: bool`; frontend renders path as badge + uptime in card + full display in modal |
| **VPN connector flags visible to server** | wire + backend | server admin cannot tell why a link behaves as it does (direct disabled by config, UDP unavailable, etc.); flags `--relay-only`/`--pin-mtu`/`--forward-accept`/`--nat-masquerade` are connector-local, not sent over wire | `ConnectVpn`/`HelloVpn` gain display-only fields (`#[serde(default)]`): `relay_only`, `pin_mtu`, `mtu` (Option), `forward_accept`, `nat_masquerade`, `route_policy` (Option, policy string only — no real NAT CIDRs); sent **before auth** (lazy-yamux invariant); server stores on `Entry`; never acts on them; frontend renders flags as badges + detail in modal |
| **VPN direct fallback counter** | backend | operator cannot measure UDP-direct stability — silent fallbacks to relay are invisible | `MetricsView` += `direct_fallbacks: u64` (counts per-connection public-tunnel UDP→relay fallback); `Server` increments on fallback; exposed for debugging |
| **Active connections & auth/reject counts** | backend | operator sees no live connection census or auth failure rate (debug troubleshooting); only global bandwidth | `MetricsView` += `active_connections` (pure compute: sum of all entries' `active`), `auth_failures` (handshake/auth rejects), `conn_rejections` (max-conns semaphore exhaustion); `Server` carries three `Arc<AtomicU64>` incremented off the hot path (accept/permit-fail/fallback sites); frontend renders with friendly labels |
| **Vhost table leaner** | frontend | vhost table rows clutter with header name columns (compact view) | table header columns moved to detail-modal-only; summary table carries a count badge instead; modal preserves full `request_header_pairs`/`response_header_pairs` key+value display |
| **Config detail completeness** | backend | operator cannot see at a glance whether vhost is configured (only in Config details); no vhost certificate path reference | `ConfigView` += `vhost_config` and `vhost_cert_file` paths (Option; display info only); config panel renders them in the grid |
| **CSP hardening** | backend | CSP allows `img-src ... data:` (theoretical confused-deputy risk) | tightened `img-src` to `'self'` only (data: removed from CSP header); **T-CSP** confirms |
| **Cert dedup warning** | backend | cert card duplicate-merging silently swallows canonicalize errors → possible latent bugs | `canon_for_dedup()` now `warn!`s when path canonicalization fails (e.g. symlink resolve); operator sees the issue in logs |
| **Vhost header-value security flag** | backend | header values (Authorization, X-Api-Key) are exported to the admin page — no operator warning despite token gate | `warn!` emitted at register time when an injected header key is sensitive; security note added to docs stating values are admin-only by design (operator needs them to debug); see **Security considerations** below |

### Key implementation details

- **Listeners & Ports card:** `SummaryView` now exposes the five control-plane parameters from `ServerConfig` (already available in `ConfigView` — now surfaced in `SummaryView` for at-a-glance overview). Card shows: control port (always), port range (if public tunnels enabled), bind address (if non-default), vhost HTTP/HTTPS/QUIC ports (only when `vhost_enabled && vhost_http_port.is_some()`).
- **VPN path:** replaces the misleading `relay: bool` with a derived string. Frontend logic: if `vpn_direct` → `"direct"`, else `"relay"`. The `direct: bool` field is retained for backward-compat (legacy consumers, if any); new code uses `path`. Modal shows full direct-upgrade history (uptime → uptime since path last changed TBD; uptime_secs is entry-wide for now).
- **VPN flags wire extension:** all six new fields are `#[serde(default)]`, so old client ↔ new server and vice versa both work (missing fields decode to default). Sent on `ConnectVpn`/`HelloVpn` messages **before auth** — respects the lazy-yamux rule (prevents deadlock if server tries to write before receiving client Hello). Server reads them into `Entry` once and never changes them; they are display-only. `route_policy` is a summary string (`"accept-all"`, `"refuse-all"`, or `"accept:N refuse:M"`) — **real NAT subnets are never on the wire** (by-design limitation I-NAT2; future client-side `/admin` would show a connector's own real mappings).
- **Metrics counters:** `auth_failures` incremented at TLS-accept / Hello-auth-reject sites; `conn_rejections` at semaphore-acquire failure; `direct_fallbacks` at the fallback-to-relay sites (public-tunnel direct path only for now; vhost direct fallback not yet counted). All are `Arc<AtomicU64>` incremented with `Relaxed` ordering (no hot-path overhead — sites are error/once-per-conn).
- **Vhost table:** moved header-name columns (`Request Headers`, `Response Headers`) to the detail modal under the `request_header_pairs`/`response_header_pairs` full key+value display; table header now shows a count badge instead of clogging the row.
- **Config paths:** `vhost_config` and `vhost_cert_file` are new optional string fields pointing to the config file and cert file used at startup. Not secrets — purely for operator reference (e.g., "where do I edit vhost rules?").
- **CSP tightening:** the admin shell `index.html` served with `Content-Security-Policy: ... img-src 'self'; ...` (removed `data:`). Eliminates a theoretical attack surface (confused-deputy via a malicious `<img src="data:...">` in a header value or note).
- **Sanitization invariant strengthened (F5):** `T-SANITIZE` now recursively walks all view JSON keys (not just names, but values recursively) and asserts no match to `secret|key|password|token|admin_token`. Newly added fields (`vhost_config`, `vhost_cert_file`, route_policy) pass the check (no secrets).

### VPN display-flag wire invariants

The round-3 VPN extension respects all legacy invariants:
- **Backward compat (D1/I-4):** `#[serde(default)]` on all six new fields; old client sends Hello/ConnectVpn without them → server fills defaults; new client ↔ old server same logic.
- **Pre-auth send (D1/I-4):** flags ride the existing pre-auth `HelloVpn`/`ConnectVpn` messages; no new message type; same lazy-yamux rule applies.
- **Server acts on none (D1/I-4):** all six fields are **read-only display** — the server stores them for dashboard export but **never conditionally acts on them** (e.g., no `if relay_only { disallow direct }`). Connector behavior controlled by its own CLI flags, not by server decisions.
- **No real NAT CIDRs (D2/I-NAT2):** the `route_policy` field is a **summary string** only — the real `accept`/`refuse` route CIDR list is connector-local and never on the wire. Limitation documented below (F8).

### Known limitation (F8)

NAT real→exposed subnet mappings and per-connector accept/refuse route SUBNETS are not surfaced in the admin dashboard:
- **Why:** real subnets are gateway-local by design (`I-NAT2`) and stay off the wire (security: no IP leakage, no peer visibility into gateway LAN structure).
- **What is visible:** only the connector's advertised *exposed/virtual* subnet entries (already on wire) and a `route_policy` summary string (e.g., `"accept-all"`, `"accept:3 refuse:1"`).
- **Future:** a future client-side `/admin` endpoint would show a connector node's own real↔virtual mappings (out of scope for the server dashboard v1).

### Test IDs

**Rust unit** (`src/admin.rs`, `tests/admin_test.rs`):
- `T-OVRPORTS` — `SummaryView` serializes the five new port/bind fields
- `T-VPNVIEW` — `VpnLinkView` has `uptime_secs`/`path`/`mode`; no `relay` field
- `T-VHOSTSORT` — vhost views returned in stable subdomain order
- `T-CFGPATHS` — `ConfigView` has `vhost_config`/`vhost_cert_file`; `T-SANITIZE` passes
- `T-METACTIVE` — `active_connections` = sum of entries' `active`
- `T-METCOUNT` — new counters (auth, reject, fallback) start at 0; unit harness increments them correctly
- `T-VPNWIRE` — old `HelloVpn`/`ConnectVpn` bytes (missing new fields) deserialize with defaults; round-trip with fields set
- `T-VPNFLAGS` — `VpnLinkView` exposes all six display flags; backend stores, frontend renders
- `T-CSP` — served CSP header has no `data:` in img-src

**Frontend unit** (`test/admin_ui/**/*.test.js`):
- `T-OVR2` — overview card renders Listeners & Ports when vhost present; hidden when disabled
- `T-VPNRENDER` — given a link object with path/mode/uptime/flags, badges and detail modal render correctly
- `T-LABELS` — column header renamed "Connections" (was "Active")
- `T-VHOSTLEAN` — vhost table omits header columns; modal preserves full pairs

**e2e** (`scripts/admin_dashboard_test.sh` + `scripts/vpn_netns_test.sh`):
- `T-OVRPORTS-E2E` — server with `--vhost-http-port 8080` → `/admin/api/v1/summary` has `vhost_http_port: 8080`
- `T-METACTIVE-E2E` — after a transfer, `/admin/api/v1/metrics` has `active_connections >= 0` and increments under load
- `T-AUTHFAIL-E2E` — N control connections with a WRONG `--secret` (server started with `--secret`) → handshake rejects → `/admin/api/v1/metrics.auth_failures >= N`
- `T-VPNFLAGS-E2E` — **deferred this pass** (extending `vpn_netns_test.sh` to enable `--admin-token` + curl is invasive). VPN-flag display is covered by the Rust unit test `T-VPNFLAGS` + manual; netns e2e tracked for v2.

### Security notes

- **Vhost header values:** `request_header_pairs` and `response_header_pairs` expose full key+value pairs visible to the admin. Keys can include `Authorization`, `X-Api-Key`, etc. — this is **intentional** (operator needs them to debug gateway behavior). Token gates the entire `/admin/*` surface. A `warn!` is emitted when a sensitive key is injected, and the docs note the visibility. **No change to behavior — operator needs the data; just flagged.**
- **CSP tightening:** removed `data:` from `img-src` to eliminate a theoretical confused-deputy surface.
- **Token in transit:** always use TLS (`--tls`, `https://`) in production; the admin token is never logged or exposed in query strings.

---

## Future extensions

The modular design enables:
- **Custom panels:** any new subsystem's snapshot → new endpoint → new panel module
- **WebSocket/SSE:** replace polling with server-sent events (additive, doesn't break existing panels)
- **Export:** add a button to download JSON snapshots or streaming logs
- **Alerts:** thresholds for cert expiry, bandwidth, memory (stored in serverState, not in the dashboard)
- **macOS/Windows parity:** memory metric implementation for other OSes; Windows IOCTL-based UDP buffer tuning

---

## Security considerations

- **Token in transit:** always use TLS (`--tls`, `https://`) in production; token never logged, never in query strings
- **HTML injection:** all user-influenced data (notes, SANs, headers) HTML-escaped in JS before DOM insertion
- **Path traversal:** asset route only serves exact keys from the embedded table; no filesystem join
- **Config leak:** `ConfigView` is **sanitized** — never exposes secrets; enforced by unit test **T-SANITIZE**
- **DashMap races:** all views are **synchronous snapshots** — no data-plane lock contention
