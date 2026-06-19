# Admin Dashboard `/admin/status` — Staging Assessment & Fix Plan

> **Status:** assessment + planning (no code written this pass). Handoff doc.
> **Authoring model:** Opus 4.8 (orchestration + verification of 4 parallel Sonnet audits).
> **Scope:** full `/admin/status` SPA + every subpage (Overview, Tunnels, Secret, Vhost,
> VPN, Certs, Config, Metrics), backend views/handlers/counters, build embedding, tests, docs.
> **Goal:** staging-ready, QA-proof. Every operator-passable parameter visible in the
> right place (VPN especially), clean ordered tables + detail modals, a complete
> Metrics debug picture, an Overview that explains how the server was started.
> Branch `frontend`. Builds on round-1/2 (`ADMIN_DASHBOARD_BUGFIX{,2}_PLAN.md`).

---

## 0. Method & audit provenance

Four parallel Sonnet auditors, each adversarial in one lane; Opus verified every
"high" finding against the code + the green test suite before accepting it.

| Audit | Lane | Headline |
|-------|------|----------|
| A | Non-VPN config-surface coverage | Near-complete; 2 minor path gaps |
| B | VPN coverage (priority) | Thin per-link visibility; 3 architectural + 4 surfaceable gaps |
| C | Frontend UX + frontend bugs | No XSS; mostly clean; ordering/label polish |
| D | Backend bugs + Metrics/Overview completeness | Metrics+Overview gaps real; several bug reports were false |

### 0.1 Rejected findings (verified FALSE — recorded so QA does not re-flag)

| Reported | Verdict | Evidence |
|----------|---------|----------|
| "Public tunnel `relay_tx/rx_bytes` always 0" (D BUG-B10) | **FALSE** | `server.rs:1380` binds per-entry counters; `server.rs:1531-1534` increments entry tx/rx on the public path; e2e **T-BUG1** asserts `>0` and passes (18/0). |
| "Vhost table shows header names only — HIGH bug" (C) | **FALSE / by-design** | `vhost.js` table shows names; the **modal** shows `request_header_pairs`. This is the round-2 compact-table + detail-modal design the user requested. |
| "SecretView missing `carriers`" (D BUG-B2) | **FALSE** | `admin_views.rs:91` `pub carriers: u16`. |
| "`vpn_links` absent → frontend NaN on non-vpn builds" (D BUG-B4) | **NON-ISSUE** | `metrics.js:145` gates `if (data.vpn_links !== undefined)`; `overview.js` gates on `vpn_enabled`. Graceful. (A serde default is still tidy — kept as LOW.) |
| "Table rows reshuffle each poll (DashMap unordered)" | **FALSE for admin registry** | `admin.rs:241` `views.sort_by_key(|v| v.id)`. Only the **vhost** registry iteration is unsorted (kept as a real MED item). |

---

## 1. Findings (verified, severity-ranked)

### 1.1 HIGH

| # | Area | Finding | Anchor | Fix direction |
|---|------|---------|--------|---------------|
| **F1** | VPN | Per-link view is thin: **no `uptime_secs`** (Tunnel/Secret have it), **`relay: bool` hardcoded `true`** (meaningless — relay is the baseline, direct is the upgrade), no **mode** (1:1 vs hub) indicator, no clear **active-path** signal. | `admin_views.rs:130-156` VpnLinkView; `admin_api.rs` vpn handler (~134-223) | Add `uptime_secs`; replace/clarify `relay` with a derived `path: "direct"|"relay"`; add `mode: "1:1"|"hub"`; surface `auto_reconnect` (already on `Entry`). No wire change. |
| **F2** | VPN | **Connector-side VPN flags invisible** to the server admin: `--relay-only`, `--pin-mtu` (+`--mtu`), `--forward-accept`, `--nat-masquerade`. QA cannot tell *why* a link behaves as it does (e.g. direct disabled by config vs down). | flags in `main.rs` Vpn subcommand; not carried to server `Entry` | Extend `ConnectVpn`/`HelloVpn` with **display-only** fields (`#[serde(default)]`, wire-compat — mirrors round-1 `auto_reconnect` on `TunnelOptions`), store on `Entry`, expose in VpnLinkView. **Do NOT** send NAT *real* CIDRs (see F8/I-NAT2). |
| **F3** | Overview | Overview does not answer "how was this server started?" at a glance: **vhost HTTP/HTTPS/QUIC ports**, tunnel **port range**, **tunnel bind addr** are only in Config, not Overview. User explicitly asked for tls/no-tls (shown ✓), control port (shown ✓), **vhost tcp/udp ports (MISSING)**. | `admin_views.rs:11-34` SummaryView; `panels/overview.js` | Add `vhost_http_port`/`vhost_https_port`/`vhost_quic_port`/`port_range`/`bind_tunnels` to SummaryView; render a "Listeners & Ports" card in overview. |
| **F4** | Metrics | No **complete debug picture**. Missing: aggregate **active connections**, **auth/handshake failure** count, **connection-rejection** (max-conns semaphore exhaustion) count, **direct↔relay fallback** counts. User explicitly wants a full operational debug view. | `admin_views.rs` MetricsView; `admin_api.rs:386-427`; sources `server.rs` (conn permits ~97/230), TLS/Hello accept sites, `vhost.rs` direct opens | `active_connections` = sum of entries' `active` (pure compute, no counter). Add `Arc<AtomicU64>` counters for auth-fail, conn-reject, direct-fallback; increment at the accept/permit/fallback sites; expose in MetricsView. |

### 1.2 MEDIUM

| # | Area | Finding | Anchor | Fix |
|---|------|---------|--------|-----|
| **F5** | Tests | **T-SANITIZE shallow**: checks field *names* against a fixed list, not values, not recursively across nested view structs. A future `tls_key_path` would slip names but the check is brittle. | `admin_test.rs:365` `t_views_serialize_stable` | Serialize every view to JSON, recursively walk all keys, assert none match `secret|key|password|token|admin_token`; keep value-spotchecks for known-secret inputs. |
| **F6** | Security | Vhost **header VALUES** exported (`request/response_header_pairs`) — can include `Authorization`/`X-Api-Key`. Token-gated, but asymmetric risk + no operator warning. | `admin_api.rs` vhost handler; `vhost.rs:342-344` | `warn!` once when an injected header key is sensitive; document as admin-only in security notes. (Keep exposing — operator needs it; just flag it.) |
| **F7** | UX | **Vhost table rows unordered** (vhost registry iteration is not sorted, unlike admin snapshot). Rows can reorder between 30 s polls. | `admin_api.rs` vhost handler (~89-131) | Sort vhost views by `subdomain` before returning. |
| **F8** | VPN | **NAT real→exposed mappings & accept/refuse route policy invisible.** This is **architectural**: `I-NAT2` keeps real subnets gateway-local (never on wire); accept/refuse filtering is connector-local. | `CLAUDE.md` I-NAT2; vpn route filter (connector-side) | **Document as a known limitation.** Optionally surface the connector's *exposed* (virtual) advertise entries (already on wire) and its accept/refuse **policy strings** (not subnets) via the F2 display-field channel. Real CIDRs stay off the wire. |

### 1.3 LOW / polish

| # | Finding | Anchor | Fix |
|---|---------|--------|-----|
| **F9** | "Active" column header ambiguous | `tunnels.js`,`secret.js`,`vhost.js` | rename → "Connections". |
| **F10** | CSP allows `img-src ... data:` (theoretical confused-deputy via a data-URI) | `admin_http.rs:79` CSP | drop `data:` from `img-src`. |
| **F11** | `canon_for_dedup()` swallows canonicalize errors → possible duplicate cert cards | `admin_api.rs:~264` | `warn!` on canonicalize failure. |
| **F12** | ConfigView lacks `vhost_config` path + `vhost_cert_file` path (debug convenience) | `admin_views.rs` ConfigView; `main.rs:1856-1880` | additive `Option<String>` fields (paths, not secrets — `key_file` stays out). |
| **F13** | Vhost table carries 2 header columns (names) that clutter the summary | `vhost.js` | move header columns to modal-only; keep table lean (headers count badge instead). |

### 1.4 Confirmed-good (no action)

XSS: all server/user data `escapeHtml`'d before `innerHTML` (notes, peer, SANs, headers, secret_id, overlay, routes). Auth/token flow + 401 handling correct. `build.rs ADMIN_ASSETS` complete incl `modal.js`. Snapshot views release DashMap guards before `.await`. Non-VPN config surface (Audit A): 23/23 essential server flags surfaced; `--secret`/`--admin-token`/`key_file` correctly omitted. Modal coverage present on tunnels/secret/vhost/vpn with correct `stopPropagation` on nested controls. 30 JS + Rust + e2e suites green.

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | VPN connector flags reach the server as **display-only** fields appended to `ConnectVpn`/`HelloVpn`, all `#[serde(default)]`. | Wire-compatible old↔new (mirrors round-1 `auto_reconnect`). New `Entry` fields; populated at VPN register; surfaced in VpnLinkView. The server **acts on none of them** (display only). |
| **D2** | **Real NAT CIDRs never go on the wire** (`I-NAT2` preserved). Only already-advertised *exposed/virtual* CIDRs + connector route **policy strings** may be surfaced. | F8 partly addressed; remainder documented as a by-design limitation (a future client-side `/admin` would show its own real mappings). |
| **D3** | New Metrics counters are `Arc<AtomicU64>` incremented **off the hot path** (accept/permit-fail/fallback are once-per-connection or error sites), read via `load(Relaxed)` in the snapshot. | No data-plane throughput impact (same rule as round-1 byte counters). Opus reviews placement. |
| **D4** | `relay: bool` (always-true) is **replaced** by a derived `path: "direct"\|"relay"` + keep `direct: bool`. | Removes a misleading field; frontend renders the active path clearly. This is a view-shape change to `/admin/api/v1/vpn` (no legacy consumer — SPA ships with the binary). |
| **D5** | Overview gains ports/range fields on `SummaryView` (additive). | Overview becomes the at-a-glance "how is this running" card the user asked for; Config stays the full detail. |
| **D6** | Every change is **additive / display-only**; tunnel/VPN data plane untouched; `/admin/status/data` legacy stays byte-identical (I-1). | Zero data-plane regression; churn is admin-scoped. |
| **D7** | Header-value export stays (operator needs it) but emits a `warn!` for sensitive keys + a security-note. | F6 resolved without removing a useful debug feature. |

---

## 3. Target architecture (deltas only)

### 3.1 View-struct additions (`src/admin_views.rs`)
- `SummaryView` += `vhost_http_port: Option<u16>`, `vhost_https_port: Option<u16>`, `vhost_quic_port: Option<u16>`, `port_range: String`, `bind_tunnels: String`.
- `VpnLinkView`: += `uptime_secs: u64`, `path: String` (replaces `relay`), `mode: String`, `auto_reconnect: bool`, `relay_only: bool`, `pin_mtu: bool`, `mtu: Option<u16>`, `forward_accept: bool`, `nat_masquerade: bool`, `route_policy: Option<String>`.
- `MetricsView` += `active_connections: usize`, `auth_failures: u64`, `conn_rejections: u64`, `direct_fallbacks: u64`.
- `ConfigView` += `vhost_config: Option<String>`, `vhost_cert_file: Option<String>`.

### 3.2 Wire (`ConnectVpn`/`HelloVpn`, VPN protocol module)
Append display-only fields (`#[serde(default)]`): `relay_only: bool`, `pin_mtu: bool`, `mtu: Option<u16>`, `forward_accept: bool`, `nat_masquerade: bool`, `route_policy: Option<String>`. Sent **before auth** (lazy-yamux rule, like existing `HelloVpn`/`ConnectVpn`). Server copies into `Entry` (new fields), never acts on them.

### 3.3 New server counters (`src/server.rs`)
`auth_failures`, `conn_rejections`, `direct_fallbacks`: `Arc<AtomicU64>` on `Server`. Increment sites: TLS-accept / yamux-`Hello`-auth failure; `conn_permits.acquire()` failure; per-connection direct→relay fallback (public + vhost). `MetricsView` reads them.

### 3.x Reuse map
| Need | Reuse | Location |
|------|-------|----------|
| Display-only wire field pattern (serde default) | round-1 `auto_reconnect` on `TunnelOptions` | git history / `shared.rs` TunnelOptions |
| Per-entry atomic counter pattern | `relay_tx_bytes` Arc<AtomicU64> | `admin.rs:79-81,195-196,236-237` |
| Stable sort of views | `views.sort_by_key(|v| v.id)` | `admin.rs:241` |
| Detail modal + `detailRows` | round-2 modal | `admin_ui/modal.js`, `ui.js detailRows` |
| Overview card grid | existing card markup | `panels/overview.js:20-44` |
| Sanitize invariant | `t_views_serialize_stable` | `admin_test.rs:365` |

---

## 4. New interface (API shape, additive)

- `GET /admin/api/v1/summary` += `vhost_http_port`, `vhost_https_port`, `vhost_quic_port`, `port_range`, `bind_tunnels`.
- `GET /admin/api/v1/vpn` link objects: += `uptime_secs`, `path`, `mode`, `auto_reconnect`, `relay_only`, `pin_mtu`, `mtu`, `forward_accept`, `nat_masquerade`, `route_policy`; **− `relay`** (replaced by `path`).
- `GET /admin/api/v1/metrics` += `active_connections`, `auth_failures`, `conn_rejections`, `direct_fallbacks`.
- `GET /admin/api/v1/config` += `vhost_config`, `vhost_cert_file`.

No new CLI flags. `/admin/status/data` legacy unchanged.

---

## 5. New protocol / data structures

`ConnectVpn` + `HelloVpn` gain the §3.2 display fields, all `#[serde(default)]` →
old client ↔ new server and new client ↔ old server both decode (missing = default).
No persisted schema. Tunnel/VPN data plane untouched. VPN handshake ORDER unchanged
(fields ride the existing pre-auth message).

---

## 6. Implementation phases

**Global rules:** tests alongside; gates every sub-phase (`cargo fmt --all --check`,
`cargo clippy --all-features --all-targets -- -D warnings`, `cargo build`
{`--all-features`,`--no-default-features`,`--features vpn`}, `cargo test --all-features`,
`npm test`); **rebuild before manual/e2e** (assets compile-time embedded); zero
regressions; docs updated; print model per sub-task.

Each sub-phase: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Additive view/summary fields, NO wire change (ships alone)

> Cheapest wins; frontend tolerates new fields. Fixes F3, F1(partial), F4(active_connections), F7, F12.

#### 0.1 Overview ports (F3)
- **Model:** Sonnet.
- **Files:** `admin_views.rs:11-34` SummaryView, `admin_api.rs:12-47` summary handler, `panels/overview.js`.
- **Change:** add `vhost_http_port`/`vhost_https_port`/`vhost_quic_port`/`port_range`/`bind_tunnels` (source: server config-view / args — already in ConfigView at `:200,:210,:249-253`). Overview renders a "Listeners & Ports" card: control port, port range, bind, vhost HTTP/HTTPS/QUIC (only when vhost enabled).
- **Unit tests:** `T-OVRPORTS` (Rust): SummaryView serializes the 5 fields. JS `T-OVR2`: card renders vhost ports when present, hidden when vhost off.
- **e2e:** `T-OVRPORTS-E2E`: server with `--vhost-*` → `/summary` has the port fields.
- **Done:** gates green; overview shows ports at a glance.

#### 0.2 VPN view no-wire fields (F1)
- **Model:** Sonnet.
- **Files:** `admin_views.rs:130-156` VpnLinkView, `admin_api.rs` vpn handler.
- **Change:** add `uptime_secs` (from `Entry.since`), `mode` ("hub" if `hub_peers.is_some()` else "1:1"), `auto_reconnect` (from Entry), replace `relay` with derived `path` ("direct" if `vpn_direct` else "relay") + keep `direct: bool`.
- **Unit tests:** `T-VPNVIEW` (Rust): VpnLinkView serializes `uptime_secs`/`path`/`mode`/`auto_reconnect`; no `relay` key.
- **e2e:** covered by Phase 5 vpn e2e.
- **Done:** gates green (vpn feature build); no `relay` field remains.

#### 0.3 Vhost ordering + Config paths (F7, F12)
- **Model:** Haiku.
- **Files:** `admin_api.rs` vhost handler (sort by `subdomain`), ConfigView + `main.rs` build site (add `vhost_config`, `vhost_cert_file` paths).
- **Change:** `views.sort_by(|a,b| a.subdomain.cmp(&b.subdomain))`; add the two path fields (NOT `key_file`).
- **Unit tests:** `T-VHOSTSORT` (Rust): two vhosts returned subdomain-sorted. `T-CFGPATHS`: ConfigView has `vhost_config`/`vhost_cert_file`; T-SANITIZE still passes.
- **e2e:** none.
- **Done:** gates green; vhost rows stable-ordered.

#### 0.4 Metrics active_connections (F4 part 1)
- **Model:** Sonnet.
- **Files:** `admin_views.rs` MetricsView, `admin_api.rs:386-427` metrics handler.
- **Change:** `active_connections = sum(entry.active)` over the snapshot (pure compute, no new counter).
- **Unit tests:** `T-METACTIVE` (Rust): metrics builder sums active across entries.
- **e2e:** `T-METACTIVE-E2E`: after a transfer, `/metrics.active_connections >= 0` and increments under load (best-effort).
- **Done:** gates green.

---

### Phase 1 — New debug counters (server instrumentation) — **Opus review gate (1.1)**

> Fixes F4 (rest). Touches accept/permit/fallback sites — Opus reviews placement to keep them off the hot path (D3).

#### 1.1 Auth-fail / conn-reject / direct-fallback counters
- **Model:** Opus reviews increment sites → Sonnet implements.
- **Files:** `server.rs` (add 3 `Arc<AtomicU64>` to `Server`; increment at TLS/Hello-auth failure, `conn_permits.acquire()` failure, public+vhost direct→relay fallback), `admin_views.rs` MetricsView (+3 fields), `admin_api.rs` metrics handler (read).
- **Change:** per §3.3. Increments only on error/once-per-connection paths (D3).
- **Unit tests:** `T-METCOUNT` (Rust): a unit harness incrementing each counter is reflected in MetricsView; counters start at 0.
- **e2e:** `T-AUTHFAIL-E2E`: send N bad-token control connections → `/metrics.auth_failures >= N`.
- **Done:** gates green; counters move only on the intended events; no throughput regression (spot-check the existing transfer e2e timing is unchanged).

---

### Phase 2 — VPN display-flag wire extension — **Opus review gate (2.1)**

> Fixes F2, F8(partial). Touches the VPN pre-auth handshake + invariants — Opus reviews.

#### 2.1 ConnectVpn/HelloVpn display fields → Entry → VpnLinkView
- **Model:** Opus reviews wire+invariant compliance → Sonnet implements.
- **Files:** VPN protocol structs (`ConnectVpn`/`HelloVpn` — locate in `shared.rs`/vpn protocol mod), `admin.rs` Entry (+display fields), VPN register path (`vpn_server.rs`/server VPN accept), `admin_views.rs` VpnLinkView, `admin_api.rs` vpn handler, client VPN send sites (`vpn.rs`).
- **Change:** append `relay_only`, `pin_mtu`, `mtu`, `forward_accept`, `nat_masquerade`, `route_policy` as `#[serde(default)]` to the pre-auth VPN message; client fills from its parsed flags; server stores on Entry; VpnLinkView exposes. **Real NAT CIDRs excluded** (D2/I-NAT2). Server acts on none of them.
- **Unit tests:** `T-VPNWIRE` (Rust): old-struct bytes (without the fields) deserialize with defaults; round-trip with fields set. `T-VPNFLAGS`: VpnLinkView carries the flags.
- **e2e:** `T-VPNFLAGS-E2E` (vpn netns): a connector started with `--relay-only`/`--forward-accept` → `/admin/api/v1/vpn` link shows `relay_only:true`, `forward_accept:true`, `path:"relay"`.
- **Done:** gates green incl `--features vpn`; old↔new interop test passes; VPN invariants (pre-auth send, serde-default, no real CIDRs) preserved.

---

### Phase 3 — Frontend render

#### 3.1 VPN panel render (F1/F2)
- **Model:** Sonnet.
- **Files:** `panels/vpn.js`.
- **Change:** render `path` (Direct/Relay badge), `mode`, `uptime`, and the connector flags (`relay_only`/`pin_mtu`+`mtu`/`forward_accept`/`nat_masquerade`/`route_policy`) in the card + full set in the detail modal. Keep hub_peers toggle.
- **Unit tests:** JS `T-VPNRENDER`: given a link object, badges/flags render; `path` shown; absent flags (non-vpn/default) don't crash.
- **e2e:** Phase 2 covers API; render is manual + JS unit.
- **Done:** `npm test` green; VPN card shows the full configured behavior.

#### 3.2 Overview card + 3.3 polish (F3, F9, F13)
- **Model:** Sonnet (3.2) / Haiku (3.3).
- **Files:** `panels/overview.js` (Listeners & Ports card), `tunnels.js`/`secret.js`/`vhost.js` ("Active"→"Connections"; vhost header columns → modal-only, replace with a count badge).
- **Unit tests:** JS `T-OVR2` (card), `T-LABELS` (header text), `T-VHOSTLEAN` (table omits header columns, modal still has pairs).
- **e2e:** none.
- **Done:** `npm test` green; tables lean + clearly labelled.

---

### Phase 4 — Security / polish (F6, F10, F11)
- **Model:** Haiku (Sonnet if a site is non-trivial).
- **Files:** `admin_http.rs` CSP (`img-src 'self'`), `admin_api.rs` `canon_for_dedup` (`warn!`), vhost handler (`warn!` on sensitive injected header key).
- **Unit tests:** `T-CSP` (Rust): served CSP has no `data:` in img-src. (warns are log-only.)
- **e2e:** extend an existing assertion to check the CSP header value.
- **Done:** gates green.

---

### Phase 5 — Tests hardening + documentation

#### 5.1 Rust test sweep (incl F5)
- **Model:** Sonnet.
- **Files:** `admin_test.rs`, `admin_views.rs`/`admin_api.rs` test mods.
- **Change:** land/confirm T-OVRPORTS, T-VPNVIEW, T-VHOSTSORT, T-CFGPATHS, T-METACTIVE, T-METCOUNT, T-VPNWIRE, T-VPNFLAGS, T-CSP. **Strengthen T-SANITIZE (F5):** recursively walk every view's JSON keys, assert none match `secret|key|password|token`; spot-check secret-bearing inputs don't echo.
- **Done:** `cargo test --all-features` green.

#### 5.2 JS test sweep
- **Model:** Sonnet.
- **Files:** `test/admin_ui/*`.
- **Change:** T-OVR2, T-VPNRENDER, T-LABELS, T-VHOSTLEAN; extend dom-stub if needed.
- **Done:** `npm test` green.

#### 5.3 e2e extension
- **Model:** Sonnet.
- **Files:** `scripts/admin_dashboard_test.sh` (+ `scripts/vpn_netns_test.sh` for the VPN link assertions, or add a vpn link to the admin script if feasible).
- **Change:** T-OVRPORTS-E2E, T-METACTIVE-E2E, T-AUTHFAIL-E2E, T-VPNFLAGS-E2E. Rebuild first.
- **Done:** `sudo -n scripts/admin_dashboard_test.sh` green (all prior + new); VPN assertions green under the vpn harness.

#### 5.4 Documentation — **Opus final read (5.4)**
- **Model:** Haiku drafts → Opus reads.
- **Files:** `docs/frontend/ADMIN_DASHBOARD.md` (new "Round-3 staging hardening" section: the new fields per section, the VPN display-flag wire extension + that the server acts on none of them, the new Metrics counters, the Overview ports card, the F8 NAT limitation, the F6 header-value security note), this file's status line.
- **Done:** docs match shipped behavior; Opus confirms no stale claims; closing message cites paths.

---

## 7. Invariants to preserve / add

- **I-1:** `/admin/status/data` legacy endpoint byte-identical (`t_legacy_data_compat`).
- **I-2:** Tunnel/VPN **data plane unchanged**; all additions are display-only/read-only (D6).
- **I-3 (T-SANITIZE, strengthened):** no serialized key matches `secret|key|password|token`; no secret values echoed. Enforced recursively (F5).
- **I-4:** VPN display fields are `#[serde(default)]` and the server **acts on none** of them; real NAT CIDRs never on the wire (D1/D2/I-NAT2).
- **I-5:** New counters increment only off the hot path (D3); `--carriers 1`/single-path throughput unchanged.
- **I-6:** Compile-time asset embedding — rebuild before verify; new SPA files go in `registry.js`/`ADMIN_ASSETS` only.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| VPN wire field breaks old↔new interop | `#[serde(default)]` + T-VPNWIRE round-trip with missing fields. |
| Counter increments creep onto the hot path | Opus gate 1.1 reviews each site; sites are error/once-per-conn; throughput spot-check. |
| Surfacing NAT real CIDRs violates I-NAT2 | D2: only exposed/virtual + policy strings on wire; real mappings documented as a limitation. |
| `relay`→`path` shape change breaks a consumer | Only consumer is the bundled SPA (ships with binary); JS updated same PR; no legacy VPN admin client. |
| Header-value export flagged by QA as a leak | F6 `warn!` + explicit security-note documents it as intentional, admin-only. |
| e2e for VPN flags needs the vpn netns harness (sudo) | 5.3 uses `scripts/vpn_netns_test.sh`; rebuild `--features vpn` first (harness refuses stale binary). |

---

## 9. Verification summary

- **Gates:** `cargo fmt --all --check`; `cargo clippy --all-features --all-targets -D warnings`; `cargo build` ×3 profiles; `cargo test --all-features`; `npm test`.
- **Rust unit:** T-OVRPORTS, T-VPNVIEW, T-VHOSTSORT, T-CFGPATHS, T-METACTIVE, T-METCOUNT, T-VPNWIRE, T-VPNFLAGS, T-CSP + strengthened T-SANITIZE.
- **JS unit:** T-OVR2, T-VPNRENDER, T-LABELS, T-VHOSTLEAN.
- **e2e:** `scripts/admin_dashboard_test.sh` (T-OVRPORTS-E2E, T-METACTIVE-E2E, T-AUTHFAIL-E2E) + `scripts/vpn_netns_test.sh` (T-VPNFLAGS-E2E). Rebuild first.
- **Acceptance:** Overview shows tls + control port + vhost HTTP/HTTPS/QUIC ports + range; VPN card shows path/mode/uptime + connector flags; Metrics shows active_connections + auth/reject/fallback counters; tables ordered + lean + modal-complete; T-SANITIZE recursive green.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary | Opus gate |
|-------|---------------------|---------|-----------|
| 0 | 0.3 Haiku · 0.1, 0.2, 0.4 Sonnet | Sonnet | — |
| 1 | 1.1 Sonnet | Sonnet | **1.1** (counter placement) |
| 2 | 2.1 Sonnet | Sonnet | **2.1** (VPN wire + invariants) |
| 3 | 3.3 Haiku · 3.1, 3.2 Sonnet | Sonnet | — |
| 4 | 4 Haiku | Haiku | — |
| 5 | 5.4 Haiku (draft) · 5.1, 5.2, 5.3 Sonnet | Sonnet | **5.4** (final docs read) |

> Start Sonnet; Haiku for mechanical (0.3 sort/paths, 3.3 labels, 4 polish, 5.4 draft);
> Opus only at gates 1.1 (hot-path counters), 2.1 (VPN wire/invariants), 5.4 (docs).
> Print the model per sub-task during implementation.
