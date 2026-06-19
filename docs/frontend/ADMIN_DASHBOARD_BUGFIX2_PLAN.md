# Admin Dashboard Bug-Fix (Round 2) — Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Target:** `/admin/status` SPA shows live, correct, complete data: auto-refresh
> every 30 s; overview counts non-zero; config fully populated (no spurious `null`,
> no missing tuning values); per-row detail modals expose every per-entry field.
> Minimize token usage during implementation (delegate mechanical sub-phases to Haiku).
> Branch `frontend`. Builds on round-1 fixes (`ADMIN_DASHBOARD_BUGFIX_PLAN.md`).

---

## 1. Context & problem

The `/admin/status` SPA (`src/admin_ui/`, vanilla-JS panel-registry) + versioned
API (`/admin/api/v1/*`, `src/admin_api.rs` → views in `src/admin_views.rs`) shipped
in round 0/1. Five user-reported defects remain. Recon anchors (so implementers
never re-search):

**Frontend** (`src/admin_ui/`):
- Poller: `poller.js:9-33` (`createPoller`/`start`/`stop`), wired in `app.js:12`
  (`createPoller(() => refreshCurrent())`), `app.js:55-59` `setupPolling()`,
  `app.js:61` `hashchange→setupPolling`, `app.js:66` bootstrap call. Refresh hook
  `router.js:52` `_refresh = () => renderPanel(getRoute())`, exported
  `router.js:67-69 refreshCurrent()`. Per-panel `refreshMs`: overview 5000
  (`overview.js:12`), tunnels/secret/vhost/vpn 5000, certs 60000, config 0,
  metrics 3000.
- Overview: `panels/overview.js:14-60`. Reads `data.public_tunnels` (l.27),
  `data.secret_tunnels` (l.28), `data.vhost_domains` (l.29), `data.vpn_links`
  (l.33, gated on `data.vpn_enabled`).
- Config: `panels/config.js` — dynamic `Object.entries(data)` (l.23), renders all
  keys; `null` printed literally as `'null'` (l.34-35). **No hardcoded subset** —
  so any field the backend adds renders automatically.
- Tables: `panels/tunnels.js:48-58`, `panels/secret.js:37-46`, `panels/vhost.js:42-50`,
  `panels/vpn.js` (card layout, `hub_peers` toggle l.66-109).
- Shared helpers `ui.js`: `escapeHtml` (9-17), `fmtBytes` (22-32), `fmtDuration`
  (37-49), `fmtDate` (54-62), `badge` (67-72), `table(headers, rows)` (78-107),
  `notesCell` (113-142). **No reusable modal/dialog** — only ad-hoc in-place expands
  (`notesCell` l.131, vpn hub toggle `vpn.js:95-104`).
- API client `api.js:8-28` `apiGet(endpoint)` (Bearer token, 401→`bore:unauthorized`).
- Registry `registry.js:25-34` (ordered panel array; panel contract documented l.4-13).
- Assets embedded at **compile time** by `build.rs` (`ADMIN_ASSETS` table). A new JS
  file is NOT served until added to that table AND the binary rebuilt.

**Backend** (`src/admin_api.rs`, `src/admin_views.rs`, `src/main.rs`):
- Summary: handler `admin_api.rs:12-36`, struct `SummaryView` `admin_views.rs:11-34`.
  Serializes `live_tunnels` (l.28 = `admin.len()`, **all roles**), `live_vhost`
  (l.30), `live_vpn_links` (l.33). Registries: admin `DashMap<u64,Arc<Entry>>`
  (`admin.rs:159`), vhost `VhostRegistry` (`server.rs:155`), vpn `vpn_providers`
  (`server.rs:195`). `Entry.role: Role` (`admin.rs:45`).
- Config: handler `admin_api.rs:345-347`, struct `ConfigView` `admin_views.rs:187-229`.
  `udp_socket_send_buffer: Option<usize>` (l.204), `udp_socket_recv_buffer:
  Option<usize>` (l.206). Built once at startup `main.rs:1856-1880`, stored via
  `server.set_config_view()` (`server.rs:368-369`).
- Server CLI args (source of truth) `main.rs:356-573`. Parses
  `--udp-socket-recv-buffer` (456-464) / `--udp-socket-send-buffer` (466-474) as
  `String`, plus `--udp-stream-receive-window` (426-434), `--udp-connection-receive-window`
  (436-444), `--udp-send-window` (446-454), `--udp-max-streams` (476-479),
  `--bind-domain` (401-403), `--control-hsts` (491-499), `--vhost-mode` (528-531),
  `--vhost-quic-port` (523-526). **`main.rs:1864-1865` hardcodes the two socket
  buffers to `None` with a TODO** — parsed values are discarded.
- Views: `TunnelView` `admin_views.rs:37-71` (already 16 fields, near-complete vs
  `Entry`). `SecretView` `admin_views.rs:74-98` — **missing `notes`** (present on
  `Entry` `admin.rs:53`). `VhostView` `admin_views.rs:101-117` — `request_headers`/
  `response_headers` expose **keys only** (`admin_views.rs:112-114`); `VhostEntry`
  holds full `(k,v)` pairs (`vhost.rs:342-344`) + `direct` pool. `VpnLinkView`
  `admin_views.rs:121-146` already rich (+ `hub_peers`).
- Sanitization guard: `tests/admin_test.rs:365 t_views_serialize_stable` forbids any
  serialized field name containing `admin_token`/`secret`/`key`/`password`
  (T-SANITIZE). New fields must respect it (⇒ never serialize `key_file`).

**Tests & deploy:**
- JS: root `package.json` (`"type":"module"`, `npm test` →
  `node --test "test/admin_ui/**/*.test.js"`, zero-dep). DOM stub
  `test/admin_ui/dom-stub.js`. Existing: `smoke`, `poller`, `notes`,
  `metrics-rate`, `badges` `.test.js`.
- Rust: `src/admin.rs:322-409` unit mod (`relay_counters_snapshot`,
  `carriers_and_auto_reconnect_in_snapshot`, …); `tests/admin_test.rs:1-708` e2e-HTTP
  (`t_views_serialize_stable` l.365, `t_api_requires_token` l.415, `t_api_tunnels_shape`
  l.478, `t_legacy_data_compat` l.637).
- Gates `test_gates.sh`: `cargo fmt --all --check`, `cargo clippy --all-features
  --all-targets -- -D warnings`, `cargo build {--all-features,--no-default-features,
  --features vpn}`, `cargo test --all-features`. Plus `npm test`.
- e2e `scripts/admin_dashboard_test.sh` (2-netns, control TLS; `sudo
  scripts/admin_dashboard_test.sh`). Current IDs: T-SHELL, T-ASSET, T-TRAVERSAL,
  T-AUTH, T-REF1..4, T-VPN, T-COMPAT, T-BUG1/3/4.
- Reference compose (operator's expected value set) `docker/docker-compose.server.yml`:
  `BORE_{SECRET, CONTROL_PORT, MIN_PORT, MAX_PORT, MAX_CONNS, MAX_CARRIERS,
  UDP_STREAM_RECEIVE_WINDOW, UDP_CONNECTION_RECEIVE_WINDOW, UDP_SEND_WINDOW,
  UDP_SOCKET_RECV_BUFFER, UDP_SOCKET_SEND_BUFFER, UDP_MAX_STREAMS, UDP, BIND_DOMAIN,
  ADMIN_TOKEN, CONTROL_HSTS, CERT_FILE, KEY_FILE, VHOST_BASE_DOMAIN, VHOST_MODE,
  VHOST_HTTP_PORT, VHOST_HTTPS_PORT, VHOST_QUIC_PORT, VHOST_CERT_FILE, VHOST_KEY_FILE,
  VHOST_CONFIG}`.
- Docs: `docs/frontend/ADMIN_DASHBOARD.md` (arch/ops), `…_PLAN.md`,
  `…_BUGFIX_PLAN.md`.

### Goal

`/admin/status` is correct and complete: (0) auto-refreshes every 30 s with no
manual reload; (1) overview shows real public/secret/vhost/vpn counts; (2) config
shows the real UDP socket buffer values, never `null` when set; (3) config exposes
every operator-tunable server value present in the compose; (4) each row in
tunnels/secret/vhost/vpn tables stays compact but opens a detail modal on click that
shows **all** per-entry fields. All gates + JS + e2e green, docs updated, zero
regressions.

### Reference scenario (final acceptance test)

```
Server: bore server --admin-token <32+> --cert-file … --key-file … --udp \
        --udp-socket-send-buffer 16MiB --udp-socket-recv-buffer 16MiB \
        --udp-stream-receive-window 16MiB --bind-domain bore.example.com \
        --control-hsts "max-age=31536000"
Then: one public `bore local` tunnel + one secret provider/consumer pair connect.

Browser at https://<server>:7835/admin/status , token entered:
  • #/overview      → Public Tunnels = 1, Secret Tunnels = 2 (provider+consumer),
                      Vhost = 0, (VPN hidden unless --vpn). Values update within 30s
                      WITHOUT a page reload.
  • #/config        → udp_socket_send_buffer = 16777216 (not null),
                      udp_socket_recv_buffer = 16777216, udp_stream_receive_window
                      = "16MiB", udp_max_streams, bind_domain, control_hsts, vhost_mode,
                      vhost_quic_port ALL present.
  • #/tunnels       → table unchanged columns; click the row → modal lists every
                      TunnelView field (id, peer, public_port, notes, basic_auth,
                      https, force_https, carriers, auto_reconnect, udp, overlay,
                      vpn_direct, active, uptime_secs, relay_tx_bytes, relay_rx_bytes).
  • #/secret        → modal includes `notes`.
Curlable proof (e2e, no browser): GET /admin/api/v1/summary → public_tunnels>=1 &&
secret_tunnels>=2; GET /admin/api/v1/config → .udp_socket_send_buffer==16777216 &&
has(.udp_stream_receive_window,.bind_domain,.control_hsts,.vhost_mode); GET
/admin/api/v1/secret → first element has key "notes".
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | `SummaryView` exposes per-role counts named **exactly** as the overview panel reads: `public_tunnels`, `secret_tunnels`, `vhost_domains`, `vpn_links`. Replace `live_tunnels`/`live_vhost`/`live_vpn_links`. | Backend iterates admin entries by `Role`; frontend `overview.js` needs **no change**. Must grep & update any test/legacy referencing `live_*` (the `/admin/status/data` legacy struct is separate — `t_legacy_data_compat` must stay byte-identical). |
| **D2** | `secret_tunnels` counts `Role::SecretProvider` **and** `Role::SecretConsumer` (both ends visible to one server). `public_tunnels` = `Role::Public`. `vhost_domains` = `vhost_reg.len()`. `vpn_links` = vpn registry len (vpn feature). | Count semantics match the per-role tables; documented in arch doc. |
| **D3** | Bug 0 is treated as a **runtime defect to reproduce**, not assumed fixed. Static path is wired; primary suspects: (a) stale compile-time-embedded asset (binary not rebuilt after round-1 JS landed), (b) browser-cached `app.js`. Fix = reproduce in a real browser + headless harness, correct the actual cause, AND satisfy the explicit 30 s requirement. | Sonnet must rebuild (`cargo build`) before manual verify; if a code defect is found it is fixed; a JS regression test asserts the poller fires repeatedly; add a no-store cache header for `/admin/ui/*.js` if browser caching is the cause (see D4). |
| **D4** | Unify the polling interval to a single `DEFAULT_REFRESH_MS = 30000` constant; data panels inherit it; `config` stays `0` (static). | One source of truth; overview/tunnels/secret/vhost/vpn → 30 s; certs may stay 60 s; metrics moves to 30 s (was 3 s) to honor the user requirement. If browser caching is the root cause, also set `Cache-Control: no-store` on embedded `/admin/ui/*` responses. |
| **D5** | Wire the already-parsed `--udp-socket-{send,recv}-buffer` values into `ConfigView` (remove the `None` hardcode). Reuse the SAME human-size parser (`"16MiB"→bytes`) already used to apply the values to the socket. `None` (unset) renders as `"auto (OS default)"` frontend-side, not `null`. | Fixes bug 2; `Option<usize>` becomes `Some` when the flag is passed; frontend `config.js` null-case updated to a friendly label. |
| **D6** | `ConfigView` gains the missing operator-tunable fields: `udp_stream_receive_window: String`, `udp_connection_receive_window: String`, `udp_send_window: String`, `udp_max_streams: u32`, `bind_domain: Option<String>`, `control_hsts: String`, `vhost_mode: Option<String>`, `vhost_quic_port: Option<u16>` (+ `vpn_punch_timeout` under `vpn` feature if it exists). UDP windows stored verbatim as their human string (display parity with compose). | Fixes bug 3; config panel is dynamic so renders them automatically. **No secrets**: never add `secret`, `key_file`, `admin_token` (T-SANITIZE). `cert_file`/`key_file` paths stay out; TLS presence already shown via `tls: bool`. |
| **D7** | Per-row **detail modal** is a new reusable component `src/admin_ui/modal.js` (+ CSS), not per-panel ad-hoc. Tables keep their current compact columns; a row-click opens the modal showing **all** fields of that entry via a generic `detailRows(obj)` formatter. | Fixes bug 4 frontend. Modal: overlay + close on X / Esc / click-outside. `detailRows` reuses `fmtBytes` (`*_bytes`), `fmtDuration` (`*_secs`), boolean→badge, arrays→joined. Adds `modal.js` to `build.rs` `ADMIN_ASSETS` + rebuild (else not served). |
| **D8** | Backend view completeness for the modal: add `notes` to `SecretView`. For `VhostView`, add header **values** (`request_header_pairs`/`response_header_pairs: Vec<(String,String)>`) and `direct_pool` size — additive fields, table render unchanged. | Modal has full data. Header values are operator-configured and shown only to the token-authed admin (acceptable; documented as a security note). Field names contain none of the T-SANITIZE substrings. |
| **D9** | All changes are **additive / non-behavioral** to the tunnel data plane. Only admin API shape + SPA assets change. `/admin/status/data` legacy endpoint stays byte-identical (T-COMPAT). | Zero regression on tunneling; the only test churn is admin-scoped. |
| **D10** | Rebuild discipline: any sub-phase that edits `src/admin_ui/*` or `build.rs` is **not done** until `cargo build` is re-run (assets are compile-time-embedded) and `npm test` passes; e2e/manual verify always run against a freshly built binary. | Prevents the round-1 "fixed in source, stale in binary" trap (the leading bug-0 suspect). |

---

## 3. Target architecture

### 3.1 Summary counts (bug 1)

`SummaryView` (`admin_views.rs:11-34`) — replace the three `live_*` fields with
four per-role counts. Compute in `admin_api::summary` (`admin_api.rs:12-36`) with a
single pass over `admin.entries`:

```
let (mut public_tunnels, mut secret_tunnels) = (0, 0);
for e in admin.iter() {
    match e.role {
        Role::Public => public_tunnels += 1,
        Role::SecretProvider | Role::SecretConsumer => secret_tunnels += 1,
        _ => {}
    }
}
vhost_domains = vhost_reg.len();
vpn_links     = vpn_reg.len();           // #[cfg(feature="vpn")]
```

Field names emitted: `public_tunnels`, `secret_tunnels`, `vhost_domains`,
`vpn_links` — exact match to `overview.js`.

### 3.2 Config completeness (bugs 2 + 3)

`ConfigView` struct + the build site `main.rs:1856-1880`. Thread the
already-parsed locals into the struct literal (they exist; just stop discarding
them). Buffer parse: reuse the existing `"16MiB"→usize` size parser used when the
buffers are applied to the UDP socket (locate at the socket-buffer application site;
do not hand-roll a second parser). `udp_socket_*_buffer` become `Some(bytes)` when
the flag is set, `None` otherwise.

### 3.3 Detail modal (bug 4) — control flow

```
table row (tunnels/secret/vhost/vpn)
   └─ click ─► openModal(title, detailRows(entry))
                 ├─ overlay <div class="modal-overlay">  (append to <body>)
                 │    └─ <div class="modal"> title + X + <dl> rows
                 ├─ close on: X click | Esc keydown | overlay click-outside
                 └─ detailRows(obj): Object.entries → {label, value}
                      *_bytes → fmtBytes · *_secs → fmtDuration ·
                      bool → badge(yes/no) · array → join(', ') · null → '—'
```

`modal.js` exports `openModal(title, rows)` and `closeModal()`. One overlay at a
time (close any open one first). Panels import `openModal` + add a click listener per
row (cursor:pointer). VPN keeps its card layout — click the card header → modal of
the full link incl. `hub_peers`.

### 3.x Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Byte / duration formatting in modal | `fmtBytes`, `fmtDuration` | `ui.js:22-32`, `:37-49` |
| Boolean → pill | `badge(text, kind)` | `ui.js:67-72` |
| XSS-safe text | `escapeHtml` | `ui.js:9-17` |
| Table builder (rows already objects) | `table(headers, rows)` | `ui.js:78-107` |
| Poller timer (already injectable for tests) | `createPoller` | `poller.js:9-33` |
| Per-role enum | `Role` | `admin.rs:45` (variants used in `admin_api.rs`) |
| Human size parser ("16MiB") | existing UDP-buffer apply-site parser | locate near socket buffer set (holepunch/server UDP setup) |
| Asset embedding (add modal.js) | `ADMIN_ASSETS` table | `build.rs` |
| Sanitization invariant | `t_views_serialize_stable` | `tests/admin_test.rs:365` |

---

## 4. New interface (CLI flags / API / config)

No new CLI flags. API shape changes (additive / renamed, all under `/admin/api/v1`):

- `GET /admin/api/v1/summary`: **remove** `live_tunnels`, `live_vhost`,
  `live_vpn_links`; **add** `public_tunnels:int`, `secret_tunnels:int`,
  `vhost_domains:int`, `vpn_links:int` (vpn feature).
- `GET /admin/api/v1/config`: **add** `udp_stream_receive_window:str`,
  `udp_connection_receive_window:str`, `udp_send_window:str`, `udp_max_streams:int`,
  `bind_domain:str|null`, `control_hsts:str`, `vhost_mode:str|null`,
  `vhost_quic_port:int|null` (+`vpn_punch_timeout` vpn). `udp_socket_send_buffer` /
  `udp_socket_recv_buffer` now `int|null` (was always `null`).
- `GET /admin/api/v1/secret`: **add** `notes:str|null`.
- `GET /admin/api/v1/vhost`: **add** `request_header_pairs:[[str,str]]`,
  `response_header_pairs:[[str,str]]`, `direct_pool:int`.

No change to `/admin/status/data` (legacy, T-COMPAT byte-identical).

---

## 5. New protocol / data structures

No wire-protocol / persisted-schema change. Only the admin JSON view structs in
`src/admin_views.rs` change (additive fields + the summary rename). Backward compat:
the SPA is served from the same binary as the API, so view/panel versions move
together (no cross-version admin client to support). Tunnel control protocol
untouched (D9).

---

## 6. Implementation phases

**Global rules:** tests alongside; every sub-phase passes the gates
(`cargo fmt --all --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
`cargo build {--all-features,--no-default-features,--features vpn}`,
`cargo test --all-features`, `npm test`); **zero regressions**; rebuild before any
manual/e2e verify (D10); update docs when shape changes; **print the model used per
sub-task**.

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Backend data correctness (additive, ships alone)

> Fixes bugs 1, 2, 3 and the bug-4 backend gaps. Pure API-shape/data changes; no
> tunnel-path behavior change. Independently shippable (frontend already tolerates
> the new fields; overview starts working the moment counts are renamed).

#### 0.1 Per-role summary counts (bug 1) — **Opus review gate**
- **Model:** Opus reviews the count/role mapping → Sonnet implements.
- **Files:** `src/admin_views.rs:11-34` (SummaryView), `src/admin_api.rs:12-36`
  (summary handler), grep `live_tunnels|live_vhost|live_vpn_links` repo-wide
  (update every reference; expect tests + maybe arch doc).
- **Change:** Replace `live_tunnels/live_vhost/live_vpn_links` with
  `public_tunnels/secret_tunnels/vhost_domains/vpn_links` per §3.1 + D2. Single pass
  over admin entries matching `Role`. `vpn_links` stays `#[cfg(feature="vpn")]`.
- **Unit tests:** `T-SUM` (new, in `src/admin.rs` test mod or `admin_api`): build a
  registry with 1 Public + 1 SecretProvider + 1 SecretConsumer + 1 Vhost (+1 Vpn under
  feature); assert `public_tunnels==1`, `secret_tunnels==2`, `vhost_domains==1`,
  `vpn_links==1`. Extend `t_views_serialize_stable` to assert the new field names
  serialize and `live_*` are gone.
- **e2e tests:** `T-SUMCOUNT` (extend `admin_dashboard_test.sh`): with a live public
  tunnel + secret pair, `GET /summary` → `.public_tunnels>=1 && .secret_tunnels>=2`.
- **Done:** gates green; overview counts non-zero against a live server; no orphan
  `live_*` reference remains; `t_legacy_data_compat` still passes (legacy struct
  untouched).

#### 0.2 Wire UDP socket buffers into ConfigView (bug 2)
- **Model:** Haiku (mechanical thread-through; escalate to Sonnet only if the size
  parser is not already reusable).
- **Files:** `src/main.rs:1856-1880` (build site; the `None` hardcode at ~1864-1865),
  `src/main.rs:456-474` (the parsed `String` args), size-parse helper at the
  socket-buffer apply site (reuse — locate via grep for the buffer setsockopt call).
- **Change:** Parse `--udp-socket-send-buffer`/`--udp-socket-recv-buffer` to
  `Option<usize>` bytes with the existing parser and pass into `ConfigView`
  (D5). Remove the TODO/`None`.
- **Unit tests:** `T-BUF` (in `admin_views`/`admin_test`): construct a `ConfigView`
  with buffers set → serialized `udp_socket_send_buffer == 16777216` (16 MiB), not
  null; unset → null.
- **e2e tests:** `T-CFGBUF` (extend script): start server with
  `--udp-socket-send-buffer 16MiB` → `GET /config` `.udp_socket_send_buffer==16777216`.
- **Done:** gates green; config no longer shows `null` for set buffers.

#### 0.3 Extend ConfigView with missing tuning fields (bug 3)
- **Model:** Sonnet (struct + main.rs wiring, sanitize-aware).
- **Files:** `src/admin_views.rs:187-229` (ConfigView), `src/main.rs:1856-1880`
  (thread the parsed locals), `tests/admin_test.rs:365` (extend T-SANITIZE).
- **Change:** Add the D6 fields (UDP windows as `String`, `udp_max_streams:u32`,
  `bind_domain`, `control_hsts`, `vhost_mode`, `vhost_quic_port`, +vpn punch timeout
  if present). All sourced from existing parsed args. **Never** add secret/key/token
  fields.
- **Unit tests:** `T-CFG` (new): `ConfigView` serializes each new field name; values
  round-trip. Extend `t_views_serialize_stable`: no serialized key contains
  `secret|key|password|admin_token` (covers the new fields too).
- **e2e tests:** `T-CFGFIELDS` (extend script): `GET /config` has
  `udp_stream_receive_window`, `udp_max_streams`, `bind_domain`, `control_hsts`,
  `vhost_mode` keys.
- **Done:** gates green; config panel shows the full operator-tunable set matching
  the compose (minus secrets/paths).

#### 0.4 View completeness for modal (bug 4 backend)
- **Model:** Sonnet.
- **Files:** `src/admin_views.rs:74-98` (SecretView +`notes`), `:101-117` (VhostView
  +`request_header_pairs`/`response_header_pairs`/`direct_pool`), `src/admin_api.rs`
  secret handler (66-87) + vhost handler (89-131) to populate them, `src/vhost.rs:338-356`
  (read header pairs + `direct` len).
- **Change:** Additive fields per D8. SecretView reads `Entry.notes` (`admin.rs:53`).
  VhostView reads full `(k,v)` from `VhostEntry.{request,response}_headers`
  (`vhost.rs:342-344`) and `direct` pool size (feature-gated). Table-render fields
  unchanged.
- **Unit tests:** `T-SECN` (new): SecretView snapshot includes `notes`. `T-VHH`
  (new): VhostView serializes `request_header_pairs`/`response_header_pairs`.
- **e2e tests:** extend script `T-SECNOTES`: `GET /secret` first element has key
  `notes`.
- **Done:** gates green; every per-entry field needed by the modal is present in the
  API.

---

### Phase 1 — Frontend (polling + config polish + detail modal)

> Depends on Phase 0 field names. Each sub-phase ends with `cargo build` + `npm test`
> (D10). Independently shippable after Phase 0.

#### 1.1 Polling: reproduce, fix, unify to 30 s (bug 0) — **Opus review gate**
- **Model:** Opus reviews the root-cause finding + 30 s design → Sonnet implements.
- **Files:** `src/admin_ui/poller.js`, `src/admin_ui/app.js:55-66`,
  `src/admin_ui/router.js:52-69`, all `panels/*.js` `refreshMs`, `build.rs` (if a
  cache header is added: the admin asset serving site — locate the `/admin/ui/*`
  response builder, likely `src/admin_http.rs`).
- **Change:** (1) Reproduce the dead-refresh in a real browser against a freshly
  rebuilt binary AND in a headless poller test. (2) Root-cause per D3 — verify it is
  not merely a stale embedded/cached asset; if stale-asset/cache, add
  `Cache-Control: no-store` to `/admin/ui/*` responses and document the rebuild
  requirement; if a code defect, fix it. (3) Introduce `DEFAULT_REFRESH_MS = 30000`
  (single constant, e.g. in `poller.js` or a small `const.js`), apply to
  overview/tunnels/secret/vhost/vpn/metrics; `config` stays 0 (D4).
- **Unit tests:** `T-POLL30` (extend `poller.test.js`): poller with injected fake
  timer fires `refreshFn` N times over N intervals; default interval constant ==
  30000; `start(0)` stops. `T-POLLARM` (new in a small `app`-level test or
  documented manual): the active-panel resolver picks the right `refreshMs`.
- **e2e tests:** none automatable headless for visual refresh; **manual acceptance**:
  load `#/overview`, observe counts/bytes update within 30 s with no reload (record
  in verify notes). Optionally assert `Cache-Control: no-store` on `/admin/ui/app.js`
  via curl (`T-NOSTORE`) if D4's cache fix is applied.
- **Done:** gates + `npm test` green; freshly built binary auto-refreshes the live
  panel every 30 s without manual reload; root cause documented in the arch doc.

#### 1.2 Reusable modal component
- **Model:** Sonnet.
- **Files:** new `src/admin_ui/modal.js`; `src/admin_ui/style.css` (modal CSS);
  `src/admin_ui/ui.js` (+`detailRows(obj)` formatter); `build.rs` (`ADMIN_ASSETS`
  +`modal.js`).
- **Change:** `openModal(title, rows)`/`closeModal()` per §3.3 + D7. `detailRows`
  formats by key suffix/type. Close on X/Esc/click-outside; single overlay. Add
  `modal.js` to the asset table (else not served — D10).
- **Unit tests:** `T-MODAL` (new `modal.test.js`, DOM-stub): `openModal` appends one
  overlay with title + rows; `closeModal`/Esc/overlay-click removes it; re-open
  closes the previous. `T-DETAIL` (new `detail.test.js`): `detailRows` maps
  `relay_tx_bytes`→fmtBytes, `uptime_secs`→fmtDuration, bool→badge, null→`—`, array→join.
- **e2e tests:** none (DOM-only; covered by JS unit + manual).
- **Done:** gates + `npm test` green; `modal.js` embedded (in `ADMIN_ASSETS` and
  served after rebuild).

#### 1.3 Wire row-click detail modals into the four panels
- **Model:** Sonnet.
- **Files:** `src/admin_ui/panels/tunnels.js:48-58`, `panels/secret.js:37-46`,
  `panels/vhost.js:42-50`, `panels/vpn.js` (card header click).
- **Change:** import `openModal`; per data row add `cursor:pointer` + click →
  `openModal(<row title>, detailRows(entry))` showing **all** fields of that entry
  (tables keep current columns). Secret modal shows `notes`; vhost modal shows header
  pairs + `direct_pool`; vpn modal shows full link incl `hub_peers`. Guard the click
  so the existing in-place expanders (`notesCell`, hub toggle) don't double-fire
  (stopPropagation on those controls).
- **Unit tests:** extend panel-level JS tests where present, or `T-ROWCLICK` (new):
  simulate a row click → `openModal` called with the entry's fields (use DOM stub
  dispatch). Assert nested expanders still toggle without opening the modal.
- **e2e tests:** API-shape already covered by Phase 0 (`T-SECNOTES`, `T-VHH`); modal
  itself is manual.
- **Done:** gates + `npm test` green; clicking any row in the 4 panels opens a modal
  listing every per-entry field; nested expanders unaffected.

#### 1.4 Config panel null-label polish + overview verify (bugs 2/3 frontend)
- **Model:** Haiku (mechanical) — Sonnet if regressions appear.
- **Files:** `src/admin_ui/panels/config.js:34-35` (null rendering),
  `panels/overview.js` (verify only — should need no change since backend matched).
- **Change:** Render `null` as `"auto (OS default)"` for `udp_socket_*_buffer` (or a
  generic `"—"` for other nulls) per D5; optionally pretty-print byte-window strings
  verbatim. Confirm overview reads the now-correct field names (no code change
  expected).
- **Unit tests:** `T-CFGNULL` (new in a `config.test.js`): config renderer maps a
  null buffer to the friendly label, a number to its string.
- **e2e tests:** none (covered by Phase 0 config e2e).
- **Done:** gates + `npm test` green; config shows friendly labels, overview counts
  correct against a live server.

---

### Phase 2 — Tests hardening + documentation

> Consolidates and rounds out coverage; docs are a deliverable, not optional.

#### 2.1 JS test sweep
- **Model:** Sonnet (assertions) / Haiku (boilerplate fixtures).
- **Files:** `test/admin_ui/{modal,detail,config,overview}.test.js` (new),
  `poller.test.js` (extend), `test/admin_ui/dom-stub.js` (extend stub if modal needs
  `document.body.appendChild`, `keydown` dispatch, or `removeChild`).
- **Change:** Land T-MODAL, T-DETAIL, T-POLL30, T-CFGNULL, T-ROWCLICK, an overview
  field-mapping test (`T-OVR`: given a summary object, the six/seven cards read
  `public_tunnels`/`secret_tunnels`/`vhost_domains`/`vpn_links`). Ensure the DOM stub
  supports everything the modal uses.
- **Unit tests:** the above are the tests.
- **e2e tests:** n/a.
- **Done:** `npm test` green; ≥6 new JS tests; stub extended as needed.

#### 2.2 Rust unit test sweep
- **Model:** Sonnet.
- **Files:** `src/admin.rs` test mod (322-409), `tests/admin_test.rs`.
- **Change:** Land/confirm T-SUM, T-BUF, T-CFG, T-SECN, T-VHH; extend
  `t_views_serialize_stable` to cover all new ConfigView/SecretView/VhostView field
  names against T-SANITIZE; extend `t_api_tunnels_shape`/summary-shape to assert the
  new summary field names.
- **Unit tests:** the above.
- **e2e tests:** n/a.
- **Done:** `cargo test --all-features` green; sanitization invariant holds for every
  new field.

#### 2.3 e2e netns extension
- **Model:** Sonnet.
- **Files:** `scripts/admin_dashboard_test.sh`.
- **Change:** Add T-SUMCOUNT, T-CFGBUF, T-CFGFIELDS, T-SECNOTES (and T-NOSTORE if D4
  cache fix applied). Start the server with the buffer/window/bind-domain/hsts flags
  from the reference scenario; bring up a public tunnel + a secret provider/consumer
  pair; assert the JSON. Rebuild the binary first (the script must `cargo build`
  current `src/` — mirror the existing rebuild guard).
- **Unit tests:** n/a.
- **e2e tests:** the above IDs.
- **Done:** `sudo scripts/admin_dashboard_test.sh` green (all prior IDs + the new
  ones), 0 failures.

#### 2.4 Documentation — **Opus final read**
- **Model:** Haiku drafts → Opus reads.
- **Files:** `docs/frontend/ADMIN_DASHBOARD.md` (arch/ops), this plan's status line.
- **Change:** Document: the summary per-role counts + field names; the full config
  field set + the buffer null→"auto" semantics; the detail-modal component and
  `detailRows`; the 30 s polling default + the rebuild/cache (`no-store`) note + the
  bug-0 root cause; the vhost header-value exposure security note. Add a "Round-2
  bug-fix" section mirroring round-1's style.
- **Unit tests:** n/a.
- **e2e tests:** n/a.
- **Done:** docs match shipped behavior; Opus confirms no stale claims; closing
  message cites the doc path.

---

## 7. Invariants to preserve / add

- **I-1:** `/admin/status/data` legacy endpoint stays **byte-identical**
  (`t_legacy_data_compat`). The summary rename touches only `/admin/api/v1/summary`.
- **I-2:** Tunnel/secret/vhost/vpn **data plane unchanged** — only admin views +
  SPA assets change (D9). No new hot-path work.
- **I-3 (T-SANITIZE):** no admin JSON field name contains
  `admin_token|secret|key|password`; no secret values (tokens, key paths) ever
  serialized. New fields must comply.
- **I-4:** Compile-time asset embedding — any `src/admin_ui/*` or `build.rs` change
  requires `cargo build` before the change is observable; e2e/manual run on a fresh
  binary (D10).
- **I-5:** Modularity contract (round-0 I-6) preserved: a new SPA file is added to
  `registry.js`/`ADMIN_ASSETS` only; core router/app untouched except the shared
  poller constant.
- **I-6:** One modal overlay at a time; modal/expanders don't leak listeners across
  re-renders (the poll re-render rebuilds the panel — modal lives on `<body>`,
  independent of `#view`).

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Bug 0 is a stale/cached asset, not code — "fix" changes nothing | D3/D10: reproduce on a freshly built binary first; if cache, add `no-store` + document rebuild; JS test proves the timer fires. |
| Summary rename breaks a hidden `live_*` consumer | 0.1 greps repo-wide before editing; `t_legacy_data_compat` guards the legacy struct. |
| Size parser duplicated → drift between displayed and applied buffer | D5: reuse the exact apply-site parser; T-BUF asserts 16MiB==16777216. |
| New ConfigView field leaks a secret/path | I-3/T-SANITIZE extended to new fields; key paths excluded by design (D6). |
| Modal re-render race with 30 s poll (poll rebuilds `#view` under an open modal) | Modal attaches to `<body>`, not `#view` (I-6); re-render leaves it intact; close handlers are idempotent. |
| Vhost header **values** expose injected auth headers | Token-gated admin only; documented security note (D8); no unauth path. |
| DOM stub lacks modal primitives → JS tests can't run | 2.1 extends `dom-stub.js` (body appendChild, keydown, removeChild). |
| e2e can't verify the modal (no browser) | Phase 0 e2e proves the API carries the fields; modal verified by JS unit + manual acceptance (recorded). |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all --check`; `cargo clippy
  --all-features --all-targets -- -D warnings`; `cargo build`
  (`--all-features`, `--no-default-features`, `--features vpn`); `cargo test
  --all-features`; `npm test`. (`test_gates.sh`.)
- **Unit tests — Rust** (`src/admin.rs` mod, `tests/admin_test.rs`): T-SUM, T-BUF,
  T-CFG, T-SECN, T-VHH + extended `t_views_serialize_stable` / summary-shape.
- **Unit tests — JS** (`test/admin_ui/`): T-MODAL, T-DETAIL, T-POLL30, T-CFGNULL,
  T-ROWCLICK, T-OVR.
- **e2e** (`sudo scripts/admin_dashboard_test.sh`, 2-netns control-TLS; rebuild first):
  existing IDs + T-SUMCOUNT, T-CFGBUF, T-CFGFIELDS, T-SECNOTES (+T-NOSTORE if applied).
- **Acceptance:** the §1 reference scenario passes — overview counts non-zero, config
  buffers/windows/bind-domain/hsts/vhost-mode present and non-null when set, row-click
  modals show all per-entry fields, and the page auto-refreshes every 30 s with no
  manual reload (T-SUMCOUNT + T-CFGBUF + T-CFGFIELDS + T-SECNOTES + manual 30 s observe).

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.2 Haiku · 0.1, 0.3, 0.4 Sonnet | Sonnet | **0.1** (per-role count mapping) |
| 1 | 1.4 Haiku · 1.1, 1.2, 1.3 Sonnet | Sonnet | **1.1** (polling runtime root-cause + 30 s) |
| 2 | 2.4 Haiku (draft) · 2.1, 2.2, 2.3 Sonnet | Sonnet | **2.4** (final docs read) |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical/boilerplate (0.2 buffer
> thread-through, 1.4 null-label, 2.4 doc draft), escalate to Opus only at the two
> correctness gates (0.1, 1.1) + the final docs read. Print the model used per
> sub-task during implementation.
