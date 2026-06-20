# Admin Dashboard — Flag/Param Parity + Secret Grouping — Design & Implementation Plan

> **Status: IMPLEMENTED 2026-06-20** (branch `webserver-log`, uncommitted). Gates
> green: `cargo fmt`, `cargo clippy --all-features --all-targets -D warnings`,
> `cargo test --all-features` (all pass), `npm test` (66/66). Implementation
> deviations from the plan (architect calls made against ground truth):
> - **`https`/`force_https` dropped from Secret scope.** Secret tunnels have no
>   public port → no server-side TLS termination → these flags don't apply (D4
>   logic extended). SecretView does not carry them.
> - **`webserver_log_max_files`/`max_file_size` dropped entirely.** Niche unnamed
>   sub-knobs; not worth half-plumbed dead fields. `webserver_log` (the boolean)
>   is shown for secret provider + vhost + public.
> - **VPN section left as-is (no wire/view changes).** It was already the most
>   complete section (the grouping reference); the absent fields (`tun_queues`,
>   `nat_udp_release_timeout`, `stun_server`, `upnp`, `try_port_prediction`) ride
>   through a param-bomb signature — high-risk/low-value, so Phase 4 was reduced
>   to an audit (no change). `max_clients` is already conveyed via `mode`.
> - New display fields cross the wire as additive `#[serde(default)]` (D3);
>   consumer plumbing went through `secret::SecretDisplay` (a display-only bundle).
>   Modal coverage is automatic via `detailRows()` (D5) — confirmed by tests.
> - Tests: `t_wire_secret_default_compat` (shared.rs), extended
>   `t_views_serialize_stable`/`t_secn_notes`/`t_vhost_parity_fields`, JS
>   `secret-group.test.js` + `secret-detail.test.js` + extended `badges`.
> - **e2e `scripts/admin_dashboard_test.sh`: 25 PASS / 0 FAIL** (run via sudo netns),
>   incl. 3 new **T-SEC-PARITY** assertions (consumer carriers=4/auto_reconnect/
>   local_proxy_port=19999/nat_udp_preferred_port=443; provider carriers/local_port;
>   provider+consumer share secret_id).

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Target:** every operational flag applicable to a mode is visible in the admin
> dashboard table/card **and** the detail modal, for **all 4 sections** (Tunnels,
> Secret, Vhost, VPN); Secret section grouped by `--tcp-secret-id` like VPN. Zero
> regressions, zero new bugs. Minimize implementation tokens (Haiku for the bulk
> additive field work; modal auto-renders View fields so render work is small).

---

## 1. Context & problem

The admin dashboard (`src/admin_ui/`, embedded by `build.rs:35-105`) serves 4
sections backed by 4 JSON endpoints (`src/admin_http.rs:108-137`):
`/admin/api/v1/{tunnels,secret,vhost,vpn}`. Each endpoint serializes a *View*
struct (`src/admin_views.rs`) built by a *builder* (`src/admin_api.rs`) from the
long-lived `Entry` (`src/admin.rs:43-103`). The server `Entry` can only display
what the **client put on the wire** at registration.

**Server-side admin can only show flags carried by the registration wire
message.** Today several client flags are local-only, so the dashboard shows
defaults (blank/0/false) for them — the reported "bugs".

### Current wire/Entry/View state (from recon — exact anchors)

- **Wire messages** (`src/shared.rs`):
  - Public tunnel `TunnelOptions` (`:261-296`): `https, force_https, basic_auth,
    notes, carriers(default), udp(default), auto_reconnect(default),
    webserver_log(default)`.
  - `HelloSecret` (`:759-773`): `id, notes, basic_auth, carriers(default)`.
  - `ConnectSecret` (`:778-783`): `id, notes` — **only these two**.
  - `HelloVhost` (`:827-853`): `subdomain, client_id, notes, basic_auth,
    carriers, udp, webserver_log, auto_reconnect` (all additive defaults).
  - `HelloVpn` (`:870-906`) / `ConnectVpn` (`:909-943`): rich — `id, advertised,
    addr, notes, carriers, max_clients(HelloVpn only), relay_only, pin_mtu, mtu,
    forward_accept, nat_masquerade, route_policy, nat_udp_preferred_port`.
- **`Entry`** (`src/admin.rs:43-103`): `role, peer, secret_id, public_port,
  notes, basic_auth, https, force_https, carriers, auto_reconnect, webserver_log,
  since, udp, active, overlay, vpn_direct, relay_tx/rx_bytes, vpn_relay_only,
  vpn_pin_mtu, vpn_mtu, vpn_forward_accept, vpn_nat_masquerade, vpn_route_policy,
  vpn_advertised, vpn_nat_udp_port`. **No** fields for `local_proxy_port`,
  secret-side `nat_udp_preferred_port`/`nat_udp_release_timeout`/`stun_server`/
  `upnp`/`try_port_prediction`/`max_conns`, local target host/port.
- **Views** (`src/admin_views.rs`):
  - `TunnelView` (`:50-85`): has https/force_https/carriers/auto_reconnect/
    webserver_log/udp/public_port/… — **missing** max_conns, local target.
  - `SecretView` (`:88-114`): `id, role, peer, secret_id, notes, basic_auth,
    carriers, udp, active, uptime, tx, rx` — **missing** auto_reconnect,
    https/force_https, webserver_log, local_proxy_port, nat_udp_preferred_port,
    nat_udp_release_timeout, stun_server, upnp, try_port_prediction, max_conns,
    local target.
  - `VhostView` (`:123-162`): complete vs vhost flags — **missing** only local
    target + weblog rotation params.
  - `VpnLinkView` (`:167-216`): rich (recently hardened) — **missing** only
    `max_clients`, `tun_queues`, `nat_udp_release_timeout`, `stun_server`,
    `upnp`, `try_port_prediction`.
- **Builders** (`src/admin_api.rs`): secret `:93-114`, vhost `:117-204`, vpn
  `:208-312`, tunnels (TunnelView). The secret builder **drops** https,
  force_https, auto_reconnect, webserver_log even where the Entry has them.
- **Frontend** (`src/admin_ui/`):
  - `panels/secret.js:16-63` — **flat table** (Role, Secret ID, Peer, Flags,
    Connections, Uptime, TX, RX, Notes). NOT grouped.
  - `panels/vpn.js:117-137` — **grouping reference**: groups by `link.link_id`
    into a `Map`, partitions `listeners`/`connectors`, renders cards.
  - `panels/tunnels.js`, `panels/vhost.js` — tables.
  - `ui.js:86-97` `flagBadges()` — knows: https, force_https, tls, basic_auth,
    udp, carriers(>1), auto_reconnect, webserver_log. Shared by tunnels/secret/
    vhost via `badgeCell()`.
  - **`modal.js:128-162` `detailRows()` auto-renders EVERY object field**
    (bytes→fmtBytes, secs→fmtDuration, bool→Yes/No, array→join, null→—). ⇒ once
    a field is in the View JSON, the **modal shows it for free**. Render work is
    limited to badges + table/card columns.

### Goal

For each of the 4 sections, surface in the dashboard (card/table **and** modal)
every flag/param **applicable** to that mode, per the breadth decision **D1**
(all operational flags except `--secret`, `--to`, `--insecure`). Regroup the
Secret section by `--tcp-secret-id` into cards mirroring VPN, each card listing
its provider and consumer. Fix the four named consumer bugs as a consequence.

### Reference scenario (final acceptance test)

```
# provider
bore local 8080 --tcp-secret-id db --to SRV --secret K --carriers 4 --udp \
  --auto-reconnect --nat-udp-preferred-port 443 --https
# consumer
bore proxy --to SRV --secret K --tcp-secret-id db --local-proxy-port 5432 \
  --carriers 4 --auto-reconnect --nat-udp-preferred-port 443

# Admin → Secret section:
#   ONE card, header "secret_id: db", containing:
#     PROVIDER row: peer, badges [x4 carriers][UDP][Auto-reconnect][HTTPS][NAT:443]
#     CONSUMER row: peer, "local:5432", badges [x4 carriers][Auto-reconnect][NAT:443]
#   Row click → modal lists ALL fields incl carriers=4, auto_reconnect=true,
#     local_proxy_port=5432, nat_udp_preferred_port=443.
# JSON GET /admin/api/v1/secret: consumer object has carriers:4,
#   auto_reconnect:true, local_proxy_port:5432, nat_udp_preferred_port:443.
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | **Breadth = all operational flags except `--secret`, `--to`, `--insecure`** (user-confirmed). Surface: carriers, auto_reconnect, udp, https, force_https, basic_auth, webserver_log(+rotation), nat_udp_preferred_port, nat_udp_release_timeout, local_proxy_port, max_conns, stun_server, upnp, try_port_prediction, local target host:port, plus mode structural fields. | Each section's wire+Entry+View grows by its applicable subset. `--secret` is NEVER serialized (security); `--to`/`--insecure` excluded (pure dialing). |
| **D2** | **Secret section = grouped cards, mirror `vpn.js`** (user-confirmed). | Rewrite `panels/secret.js` to group `SecretView[]` by `secret_id` (reuse the `vpn.js:117-137` Map pattern), partition by `role` (provider/consumer), render one card per id. |
| **D3** | **All new wire fields are additive `#[serde(default)]`** (mirrors `TunnelOptions`/`HelloVhost`/`HelloVpn` existing pattern). | Old client ↔ new server and new client ↔ old server interop unchanged. Regression test `t_legacy_data_compat` (`admin_test.rs:1177`) must still pass + a new wire-compat test. |
| **D4** | **Holepunch helper flags are mode-scoped.** They apply to **secret** tunnels (provider+consumer) and **vpn** only; on **public** tunnels they `warn!` and are ignored (CLAUDE.md invariant). | Tunnels section does NOT show `stun_server/upnp/try_port_prediction/nat_udp_*`. Secret + VPN do. Avoids displaying flags that have no effect. |
| **D5** | **Modal needs no per-field code** — `detailRows()` (`modal.js:128-162`) auto-renders View fields. | Frontend render work = badges (`flagBadges()`) + table/card columns only. Adding a View field ⇒ modal coverage for free. Tests must assert the field appears in the modal to guard this. |
| **D6** | **`local_proxy_port` (consumer) + local target host:port (provider) cross the wire for display.** User named `--local-proxy-port` as a required field. | `ConnectSecret` gains `local_proxy_port`; `HelloSecret`/`TunnelOptions` gain `local_host`+`local_port`. Minor self-info exposure to the server the client already trusts; acceptable. |
| **D7** | **Reuse existing `Entry` flag fields where present** (https, force_https, carriers, auto_reconnect, webserver_log, udp, basic_auth, public_port). Only add genuinely new fields. | Secret builder must START copying the already-present `Entry.https/force_https/auto_reconnect/webserver_log` it currently drops — partly a builder fix, not only new fields. |
| **D8** | **New generic (non-vpn) Entry fields** named without `vpn_` prefix: `local_proxy_port, local_host, local_port, nat_udp_preferred_port, nat_udp_release_timeout, stun_server, upnp, try_port_prediction, max_conns`. Do NOT reuse `vpn_nat_udp_port` for secret. | Keeps VPN fields isolated; secret/tunnel populate the generic set. VpnLinkView keeps using `vpn_*`. |
| **D9** | **`flagBadges()` extended additively** with: upnp, try_port_prediction, nat_udp_preferred_port(>0). Numeric/string params (max_conns, nat_udp_release_timeout, stun_server, local target) shown as **card/table text or modal rows**, not badges. | Shared helper change benefits all sections at once; existing badges byte-identical (new branches only fire when field present/truthy). |
| **D10** | **Vertical slices per section, each independently shippable.** Phase 0 shared scaffolding first. | Phase 1 Secret (headline + named bugs) ships alone; 2 Tunnels; 3 Vhost; 4 VPN; 5 cross-cutting tests/docs. |

---

## 3. Target architecture

### 3.1 Data flow (unchanged shape, widened payload)

```
CLI args (main.rs)  --wire-->  server registers Entry (admin.rs)
   |  (D3 additive)                 |
   v                                v
wire msg (shared.rs)          builder (admin_api.rs) --> View (admin_views.rs)
                                    |
                                    v  serde JSON
                          /admin/api/v1/<section>
                                    |
                                    v
                    panel render (admin_ui/panels/*.js)
                       badges: flagBadges (ui.js)
                       modal: detailRows AUTO (modal.js)  <-- D5
```

### 3.2 Per-section applicable-flag matrix (what MUST be displayable)

Legend: ✓ already on wire+view+rendered · **W** add to wire · **V** add to view/
builder · **B** add badge · **R** add card/table column · **—** N/A for mode.

| Flag / param | Tunnels (public) | Secret provider | Secret consumer | Vhost | VPN |
|---|---|---|---|---|---|
| carriers | ✓ | ✓ | **W**+V+R (ConnectSecret) | ✓ | ✓ |
| auto_reconnect | ✓ | **W**(HelloSecret)+V | **W**(ConnectSecret)+V | ✓ | ✓ |
| udp | ✓ | **W**(HelloSecret)+V | **W**(ConnectSecret)+V | ✓ | — |
| https / force_https | ✓ | **W**(HelloSecret)+V | — | — (tls✓) | — |
| basic_auth | ✓ | ✓ | — | ✓ | — |
| webserver_log | ✓ | **W**(HelloSecret)+V | — | ✓ | — |
| weblog rotation (max_files/size) | **W**+V (modal) | **W**+V (modal) | — | **W**+V (modal) | — |
| nat_udp_preferred_port | — (D4) | **W**+V+B | **W**+V+B | — | ✓ (nat_udp_port) |
| nat_udp_release_timeout | — (D4) | **W**+V (modal) | **W**+V (modal) | — | **V**+modal |
| stun_server | — (D4) | **W**+V (modal) | **W**+V (modal) | — | **V**+modal |
| upnp | — (D4) | **W**+V+B | **W**+V+B | — | **V**+B |
| try_port_prediction | — (D4) | **W**+V+B | **W**+V+B | — | **V**+B |
| local target (host:port) | **W**+V (modal) | **W**+V (modal) | — | **W**+V (modal) | — |
| local_proxy_port | — | — | **W**(ConnectSecret)+V+R | — | — |
| max_conns | **W**+V (modal) | **W**+V (modal) | — | — | — |
| max_clients | — | — | — | — | **V**+modal (listener) |
| tun_queues | — | — | — | — | **V**+modal (listener) |

> VPN `nat_udp_release_timeout/stun_server/upnp/try_port_prediction/tun_queues/
> max_clients` are **V**-only (already on `HelloVpn`/`ConnectVpn` partially — verify
> per 4.x; add to `HelloVpn`/`ConnectVpn` only the genuinely absent ones).

### 3.3 Secret grouping (cards, D2)

Mirror `vpn.js:117-137`:
```
group SecretView[] by view.secret_id (Map)
  per group: provider = entries.filter(role==secretprovider)
             consumers = entries.filter(role==secretconsumer)
  render card: header "secret_id: <id>"
    provider block (badges from flagBadges + https/force_https/nat/upnp/tpp)
    consumer block(s) (badges + "local:<local_proxy_port>")
each row clickable → openModal(detailRows(view))   // D5
```
Role string values confirmed via `admin_api.rs:92` dispatch
(SecretProvider/SecretConsumer); the View `role` field carries the lowercase
string the JS compares (verify exact literal in 1.4 recon-by-read).

### 3.x Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Card grouping by id + role partition | VPN grouping logic | `src/admin_ui/panels/vpn.js:117-137` |
| Modal auto-render of all View fields | `detailRows()` | `src/admin_ui/modal.js:128-162` |
| Flag badge rendering (shared) | `flagBadges()` + `badgeCell()` | `src/admin_ui/ui.js:86-97, 103-110` |
| Additive wire field pattern | `TunnelOptions`/`HelloVhost` serde defaults | `src/shared.rs:261-296, 827-853` |
| Wire field → Entry capture (vhost auto_reconnect precedent) | vhost builder | `src/admin_api.rs:117-204` |
| View serialize-stability test pattern | `t_views_serialize_stable` | `tests/admin_test.rs:415` |
| VPN panel render/group test pattern | `vpn-render.test.js`, `t_vpn_panel_groups_and_fields` | `test/admin_ui/vpn-render.test.js`, `tests/admin_test.rs:655` |
| Section parity precedent (Vhost↔Tunnels) | prior work | `docs/frontend/ADMIN_VHOST_PARITY_PLAN.md` |
| CLI flag → field source of truth | clap structs | `src/main.rs` (anchors in §4) |

---

## 4. New interface (CLI flags / API / config)

**No new CLI flags.** All flags already exist (`src/main.rs`): `bore local`
`:55-200`, `bore proxy` `:210-292`, `bore vhost` `:305-390`, `bore vpn
listen/connect` `:941-1087`. This work transports existing flag values to the
admin view.

**API (JSON) — additive only.** Each endpoint's array objects gain the new
fields per §3.2. Consumers tolerate missing fields (old server) via JS
optional access. Exact new JSON keys:

- `/admin/api/v1/secret` objects gain: `https, force_https, auto_reconnect,
  webserver_log, webserver_log_max_files, webserver_log_max_file_size,
  local_proxy_port, local_host, local_port, nat_udp_preferred_port,
  nat_udp_release_timeout, stun_server, upnp, try_port_prediction, max_conns`.
- `/admin/api/v1/tunnels` objects gain: `local_host, local_port, max_conns,
  webserver_log_max_files, webserver_log_max_file_size`.
- `/admin/api/v1/vhost` objects gain: `local_host, local_port,
  webserver_log_max_files, webserver_log_max_file_size`.
- `/admin/api/v1/vpn` objects gain: `max_clients, tun_queues,
  nat_udp_release_timeout, stun_server, upnp, try_port_prediction` (only those
  absent today — verify in 4.x).

---

## 5. New protocol / data structures

All additive, `#[serde(default)]` (D3). Show the exact field edits.

### 5.1 `src/shared.rs`

- `ConnectSecret` (`:778-783`) — add:
  `carriers: u16` (default), `auto_reconnect: bool` (default),
  `udp: bool` (default), `local_proxy_port: u16` (default),
  `nat_udp_preferred_port: u16` (default), `nat_udp_release_timeout: u64`
  (default), `stun_server: Option<String>` (default), `upnp: bool` (default),
  `try_port_prediction: bool` (default).
- `HelloSecret` (`:759-773`) — add:
  `https: bool, force_https: bool, udp: bool, auto_reconnect: bool,
  webserver_log: bool, webserver_log_max_files: usize,
  webserver_log_max_file_size: u64, nat_udp_preferred_port: u16,
  nat_udp_release_timeout: u64, stun_server: Option<String>, upnp: bool,
  try_port_prediction: bool, max_conns: usize, local_host: Option<String>,
  local_port: u16` (all `#[serde(default)]`).
- `TunnelOptions` (`:261-296`) — add:
  `max_conns: usize, local_host: Option<String>, local_port: u16,
  webserver_log_max_files: usize, webserver_log_max_file_size: u64`
  (all default).
- `HelloVhost` (`:827-853`) — add:
  `local_host: Option<String>, local_port: u16, webserver_log_max_files: usize,
  webserver_log_max_file_size: u64` (all default).
- `HelloVpn`/`ConnectVpn` (`:870-943`) — add ONLY the absent ones (verify): of
  `tun_queues: u8, nat_udp_release_timeout: u64, stun_server: Option<String>,
  upnp: bool, try_port_prediction: bool` (all default). `max_clients` already on
  HelloVpn; `nat_udp_preferred_port` already present.

> **Backward-compat (D3):** every added field `#[serde(default)]`; bincode/serde
> framing tolerates absent trailing fields via defaults. Old↔new interop covered
> by `t_legacy_data_compat` + new `t_wire_secret_default_compat`.

### 5.2 `src/admin.rs` `Entry` (`:43-103`) — add (D8)

`local_proxy_port: Option<u16>, local_host: Option<String>, local_port:
Option<u16>, nat_udp_preferred_port: Option<u16>, nat_udp_release_timeout:
Option<u64>, stun_server: Option<String>, upnp: bool, try_port_prediction: bool,
max_conns: Option<usize>, webserver_log_max_files: Option<usize>,
webserver_log_max_file_size: Option<u64>, max_clients: Option<u16>, tun_queues:
Option<u8>`. Non-atomic (set once at registration). Default all to None/false in
the `Entry` constructors.

### 5.3 Views (`src/admin_views.rs`)

Add the §4 fields to `SecretView` (`:88-114`), `TunnelView` (`:50-85`),
`VhostView` (`:123-162`), `VpnLinkView` (`:167-216`). All `#[derive(Serialize)]`
plain fields; `Option`/bool/number to match Entry.

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase passes the gates
(`cargo fmt --all -- --check`, `cargo clippy --all-features --all-targets -- -D
warnings`, `cargo test --all-features`, `npm test`); **zero regressions**; update
docs on behavior/API change; **print the model used per sub-task**.

Each sub-phase: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Shared scaffolding (pure additive, no behavior change)

> Adds Entry fields, View fields, flagBadges branches. Nothing populated/rendered
> yet beyond defaults. Safe to land alone; serialization defaults keep JSON
> backward-identical except new default-valued keys.

#### 0.1 Entry struct new fields
- **Model:** Haiku · **Opus review gate** (data-model, D8)
- **Files:** `src/admin.rs:43-103` (struct + every constructor/`Default`).
- **Change:** add the D8/§5.2 fields; initialize to None/false in ALL Entry
  construction sites (grep `Entry {` across `src/`). No reads yet.
- **Unit tests:** `cargo build --all-features` compiles; `t_entry_defaults`
  (new, `tests/admin_test.rs`) — construct an Entry the legacy way, assert new
  fields are None/false.
- **e2e tests:** none (no behavior).
- **Done:** gates green; `t_legacy_data_compat` (`:1177`) still passes.

#### 0.2 View struct new fields + serialize stability
- **Model:** Haiku · **Opus review gate** (serialized API shape)
- **Files:** `src/admin_views.rs` (`SecretView:88-114`, `TunnelView:50-85`,
  `VhostView:123-162`, `VpnLinkView:167-216`).
- **Change:** add §4 fields per view, `Serialize`. Builders still leave them at
  default (filled in later phases) — set explicit defaults in each builder so it
  compiles.
- **Unit tests:** extend `t_views_serialize_stable` (`:415`) — assert each new
  key present in JSON with default value and correct type.
- **e2e tests:** none.
- **Done:** gates green; existing `t_api_*_shape` tests updated to include new
  keys and pass.

#### 0.3 flagBadges + shared badge helpers extension
- **Model:** Sonnet
- **Files:** `src/admin_ui/ui.js:86-97`.
- **Change (D9):** add badge branches — `upnp`→'UPnP', `try_port_prediction`→
  'Port-Pred', `nat_udp_preferred_port>0`→`NAT:${port}`. Existing branches
  unchanged (new branches only fire when field truthy/present).
- **Unit tests:** `test/admin_ui/badges.test.js` + `flag-coverage.test.js` —
  add cases: object with `upnp:true` yields 'UPnP'; `nat_udp_preferred_port:443`
  yields 'NAT:443'; object without them yields byte-identical output to today.
- **e2e tests:** none.
- **Done:** `npm test` green; existing badge tests unchanged-pass (regression).

---

### Phase 1 — Secret section: grouping + full flag parity (headline; fixes named bugs)

> Vertical slice. Ships the user's primary request + all four named consumer
> bugs. Depends on Phase 0.

#### 1.1 ConnectSecret + HelloSecret wire fields (D3/D6)
- **Model:** Sonnet · **Opus review gate** (protocol, backward-compat)
- **Files:** `src/shared.rs:759-773` (HelloSecret), `:778-783` (ConnectSecret).
- **Change:** add §5.1 fields, all `#[serde(default)]`.
- **Unit tests:** `t_wire_secret_default_compat` (new, `tests/admin_test.rs` or
  a shared-wire test module) — serialize a NEW HelloSecret/ConnectSecret, decode
  with all fields; decode a LEGACY-shaped payload (omit new fields) → defaults.
- **e2e tests:** none yet.
- **Done:** gates green; `t_legacy_data_compat` passes.

#### 1.2 Populate wire fields from CLI (provider + consumer)
- **Model:** Sonnet · **Opus review gate** (correctness — right flag → right field)
- **Files:** consumer `ConnectSecret` build site `src/secret.rs:771-774`;
  provider `HelloSecret` build site `src/secret.rs:~223-225`; CLI args
  `src/main.rs` (`bore proxy :210-292`, `bore local :55-200`).
- **Change:** thread the existing clap fields into the new wire fields. Consumer:
  carriers, auto_reconnect, udp, local_proxy_port, nat_udp_preferred_port,
  nat_udp_release_timeout, stun_server, upnp, try_port_prediction. Provider:
  https, force_https, udp, auto_reconnect, webserver_log(+rotation),
  nat_udp_preferred_port, nat_udp_release_timeout, stun_server, upnp,
  try_port_prediction, max_conns, local_host, local_port.
- **Unit tests:** none new at unit level (covered by e2e 1.6 + builder 1.3).
- **e2e tests:** none yet.
- **Done:** gates green; no behavior change to traffic path (display-only fields).

#### 1.3 Capture into Entry + fix Secret builder (D7)
- **Model:** Sonnet
- **Files:** server-side secret registration (where Entry is built from
  HelloSecret/ConnectSecret — `src/secret.rs` provider `~:223`, consumer
  `~:771`); builder `src/admin_api.rs:93-114`.
- **Change:** store new wire fields into the new Entry fields; **start copying
  the already-present** `Entry.https/force_https/auto_reconnect/webserver_log`
  the builder currently drops (D7), plus all new fields, into `SecretView`.
- **Unit tests:** `t_secret_view_full_fields` (new) — build an Entry for a
  consumer with carriers=4, auto_reconnect, local_proxy_port=5432,
  nat_udp_preferred_port=443 → assert SecretView JSON carries them; provider with
  https/webserver_log → present.
- **e2e tests:** deferred to 1.6.
- **Done:** gates green.

#### 1.4 Secret panel → grouped cards (D2)
- **Model:** Sonnet · **Opus review gate** (UI structure mirrors vpn.js correctly)
- **Files:** `src/admin_ui/panels/secret.js:16-63` (rewrite render);
  reference `src/admin_ui/panels/vpn.js:117-137`.
- **Change:** group `SecretView[]` by `secret_id` into cards; partition by
  `role` (verify exact role literal by reading `admin_api.rs:92` + a sample JSON);
  render provider + consumer blocks with `flagBadges()` + extra
  https/force_https badges + consumer `local:${local_proxy_port}`; each row →
  `openModal(detailRows(view))` (D5). Ungrouped fallback if `secret_id` null
  (mirror vpn.js `#${id}` fallback).
- **Unit tests:** `test/admin_ui/secret-group.test.js` (new) — feed 2 secret_ids
  ×(provider+consumer); assert 2 cards, each with one provider + one consumer row,
  consumer shows `local:5432` and `x4 carriers`+`Auto-reconnect`+`NAT:443` badges;
  null secret_id → fallback group. Add `secret-detail.test.js` — row click modal
  lists carriers/auto_reconnect/local_proxy_port/nat_udp_preferred_port (D5).
- **e2e tests:** 1.6.
- **Done:** `npm test` green.

#### 1.5 Rust API-shape test for secret grouping fields
- **Model:** Sonnet
- **Files:** `tests/admin_test.rs` (extend `t_api_*`, pattern of `t_api_vhost_shape:979`).
- **Change:** `t_api_secret_shape_full` (new) — live server, register provider+
  consumer over the wire with the reference-scenario flags, GET
  `/admin/api/v1/secret`, assert both objects + grouping key + all new fields.
- **Unit tests:** the test itself.
- **e2e tests:** complemented by 1.6.
- **Done:** `cargo test --all-features` green.

#### 1.6 e2e netns: secret consumer flags visible
- **Model:** Sonnet · **Opus review gate** (acceptance assertion = §1 scenario)
- **Files:** `scripts/admin_dashboard_test.sh` (extend).
- **Change:** add a secret provider+consumer pair with the §1 reference flags;
  curl the admin secret endpoint over TLS; assert JSON contains consumer
  `"carriers":4`, `"auto_reconnect":true`, `"local_proxy_port":5432`,
  `"nat_udp_preferred_port":443` and provider `"https":true`; assert grouping
  (both share `secret_id`).
- **Unit tests:** n/a (shell asserts).
- **e2e tests:** **T-SEC-PARITY** — the above assertions = §1 acceptance.
- **Done:** `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/admin_dashboard_test.sh`
  exits 0; T-SEC-PARITY assertions pass.

---

### Phase 2 — Tunnels section parity

> Public tunnel. D4: NO holepunch flags. Add local target, max_conns, weblog
> rotation. Depends on Phase 0.

#### 2.1 TunnelOptions wire + populate + capture + view
- **Model:** Sonnet
- **Files:** `src/shared.rs:261-296` (add §5.1 fields); CLI `bore local` non-secret
  path `src/main.rs:55-200`; build site (where TunnelOptions is sent / Hello
  public path); Entry capture; `TunnelView:50-85`; builder.
- **Change:** add `max_conns, local_host, local_port, webserver_log_max_files,
  webserver_log_max_file_size` (all default), populate from CLI, capture to Entry,
  copy to TunnelView.
- **Unit tests:** extend `t_api_tunnels_shape` (`:811`) — assert new keys; new
  `t_tunnel_view_local_target` — Entry with local_host/port/max_conns → present.
- **e2e tests:** 2.3.
- **Done:** gates green; `t_public_tunnel_relay_bytes_*` (`:868,930`) still pass.

#### 2.2 Tunnels panel render (modal auto via D5; table optional column)
- **Model:** Haiku
- **Files:** `src/admin_ui/panels/tunnels.js:23-70`.
- **Change:** modal already covers new fields (D5). Optionally add a "Local"
  column (`local_host:local_port`); no new badges (flags already badged).
- **Unit tests:** `test/admin_ui/table-labels.test.js` / a tunnels test — assert
  modal detail includes `max_conns`, `local_port`.
- **e2e tests:** 2.3.
- **Done:** `npm test` green.

#### 2.3 e2e: tunnels new fields
- **Model:** Haiku
- **Files:** `scripts/admin_dashboard_test.sh`.
- **Change:** assert tunnels endpoint JSON carries `local_port`, `max_conns` for
  a live public tunnel.
- **e2e tests:** **T-TUN-PARITY**.
- **Done:** script exits 0.

---

### Phase 3 — Vhost section parity

> Vhost View is already near-complete. Add only local target + weblog rotation.
> Depends on Phase 0.

#### 3.1 HelloVhost wire + populate + capture + view
- **Model:** Sonnet
- **Files:** `src/shared.rs:827-853`; vhost client build site; Entry capture
  (vhost uses `VhostEntry` per recon — `src/admin_api.rs:117-204`); `VhostView:123-162`.
- **Change:** add `local_host, local_port, webserver_log_max_files,
  webserver_log_max_file_size` (default), populate, capture, copy to view.
- **Unit tests:** extend `t_api_vhost_shape` (`:979`) — new keys present.
- **e2e tests:** 3.2.
- **Done:** gates green; `vhost-parity.test.js` / `vhost-lean.test.js` pass.

#### 3.2 Vhost panel render + e2e
- **Model:** Haiku
- **Files:** `src/admin_ui/panels/vhost.js:23-77`; `scripts/admin_dashboard_test.sh`.
- **Change:** modal auto (D5); optional "Local" column. e2e assert vhost JSON
  has `local_port`.
- **Unit tests:** vhost JS test asserts modal includes `local_port`.
- **e2e tests:** **T-VHOST-PARITY**.
- **Done:** `npm test` + script green.

---

### Phase 4 — VPN section parity (audit + small additions)

> VPN was recently hardened (`frontend-vpn-panel-bugfix`). Add only the absent
> applicable fields. Depends on Phase 0.

#### 4.1 Audit VPN wire/view for absent applicable fields
- **Model:** Sonnet · **Opus review gate** (confirm which fields truly absent)
- **Files (read):** `src/shared.rs:870-943`, `src/admin_views.rs:167-216`,
  `src/admin_api.rs:208-312`, `src/admin_ui/panels/vpn.js`, `src/main.rs:941-1087`.
- **Change:** determine the absent set among `max_clients(view?), tun_queues,
  nat_udp_release_timeout, stun_server, upnp, try_port_prediction`. Produce the
  exact add-list for 4.2 (record in this doc as a checklist when implementing).
- **Unit tests:** none (audit).
- **Done:** add-list confirmed; no over-adding fields already present.

#### 4.2 Add absent VPN fields (wire if needed + view + render)
- **Model:** Sonnet
- **Files:** per 4.1 — `src/shared.rs` HelloVpn/ConnectVpn (only if absent),
  `src/admin_views.rs:167-216` VpnLinkView, `src/admin_api.rs:208-312` builder,
  `src/admin_ui/panels/vpn.js` (badges for upnp/try_port_prediction; modal auto).
- **Change:** additive fields + populate + render badges; `max_clients`/
  `tun_queues` listener-only modal rows.
- **Unit tests:** extend `t_vpn_panel_groups_and_fields` (`:655`) +
  `vpn-render.test.js` — assert new fields/badges; listener shows `max_clients`.
- **e2e tests:** **T-VPN-PARITY** (extend the vpn netns harness
  `scripts/vpn_netns_test.sh` admin assertion, or `admin_dashboard_test.sh` if it
  covers vpn) — assert vpn JSON carries the added fields.
- **Done:** `cargo test --all-features` (vpn feature) + `npm test` green; vpn
  netns admin assertion passes.

---

### Phase 5 — Cross-cutting verification + docs

#### 5.1 Full regression sweep
- **Model:** Sonnet · **Opus review gate** (final correctness sign-off)
- **Files:** all test suites.
- **Change:** run `cargo fmt --all -- --check`, `cargo clippy --all-features
  --all-targets -- -D warnings`, `cargo test --all-features`, `npm test`, and the
  netns harness. Fix any regression.
- **Unit/e2e:** the whole suite, incl. `t_legacy_data_compat`, all `*-parity`
  T-IDs.
- **Done:** every gate green; zero regressions.

#### 5.2 Docs
- **Model:** Haiku · **Opus final read**
- **Files:** `docs/frontend/ADMIN_SECTIONS.md` (update per-section field tables);
  new short note in this plan's "done" + memory pointer.
- **Change:** document the per-section displayed fields + secret grouping; note
  the D3 additive-wire backward-compat contract and D4 mode-scoping.
- **Done:** docs match implemented fields; Opus read OK.

---

## 7. Invariants to preserve / add

- **I-1 (D3):** every new wire field is `#[serde(default)]`; old↔new client/server
  interop unchanged. Guarded by `t_legacy_data_compat` + `t_wire_secret_default_compat`.
- **I-2:** `--secret` is NEVER serialized into any View/wire-display field (security).
- **I-3 (D4):** public tunnels do not display holepunch helper flags; they remain
  `warn!`-and-ignore per the existing CLAUDE.md invariant (no behavior change).
- **I-4 (D5):** adding a field to a View is sufficient for modal coverage; every
  new field gets a test asserting modal presence so this stays true.
- **I-5:** display-only additions never touch the data/traffic path — relay
  bytes, splice, carriers behavior byte-identical. Guarded by
  `t_public_tunnel_relay_bytes_*` + relay e2e.
- **I-6:** `flagBadges()` output for an object lacking the new fields is
  byte-identical to pre-change (regression test in 0.3).

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| New wire field breaks bincode/serde decode of old peers | All `#[serde(default)]` (D3); `t_wire_secret_default_compat` decodes a legacy payload; `t_legacy_data_compat` kept green. |
| Secret role literal mismatch (JS compares wrong string) | 1.4 verifies exact `role` value by reading `admin_api.rs:92` + sample JSON before coding the partition; `secret-group.test.js` asserts partition. |
| Over-adding fields already present (VPN) → dup/clippy | Phase 4.1 audit gate produces the exact absent-list first. |
| local_proxy_port/local target info exposure | D6: only to the server the client already authenticates to; never logged elsewhere; `--secret` still excluded (I-2). |
| Modal clutter from many new fields | detailRows groups them; acceptable — user explicitly wants all flags visible. |
| Entry constructor sites missed → compile fail or wrong default | 0.1 greps all `Entry {` sites; `t_entry_defaults` asserts defaults. |
| netns harness run needs fresh release build | Per CLAUDE.md: rebuild `cargo build --release --features vpn` as user before `sudo -n` running the script (noted in 1.6/4.2 Done). |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all -- --check` ·
  `cargo clippy --all-features --all-targets -- -D warnings` ·
  `cargo test --all-features` · `npm test`.
- **Unit tests (Rust):** `tests/admin_test.rs` — `t_views_serialize_stable`,
  `t_api_{tunnels,vhost}_shape`, new `t_entry_defaults`,
  `t_wire_secret_default_compat`, `t_secret_view_full_fields`,
  `t_api_secret_shape_full`, `t_tunnel_view_local_target`,
  extended `t_vpn_panel_groups_and_fields`.
- **Unit tests (JS):** `test/admin_ui/` — extended `badges`, `flag-coverage`;
  new `secret-group`, `secret-detail`; extended vhost/vpn/table-labels tests.
  Run: `npm test`.
- **e2e:** `scripts/admin_dashboard_test.sh` (+ vpn assertion in
  `scripts/vpn_netns_test.sh`). Run:
  `sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/admin_dashboard_test.sh`.
  **Rebuild release as your user before sudo-running** (CLAUDE.md netns rule).
- **Acceptance:** §1 reference scenario passes via **T-SEC-PARITY** (1.6) plus
  **T-TUN-PARITY**, **T-VHOST-PARITY**, **T-VPN-PARITY**.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 Haiku · 0.2 Haiku · 0.3 Sonnet | Haiku/Sonnet | 0.1, 0.2 |
| 1 | 1.1–1.5 Sonnet · 1.6 Sonnet | Sonnet | 1.1, 1.2, 1.4, 1.6 |
| 2 | 2.1 Sonnet · 2.2 Haiku · 2.3 Haiku | Sonnet/Haiku | — |
| 3 | 3.1 Sonnet · 3.2 Haiku | Sonnet/Haiku | — |
| 4 | 4.1 Sonnet · 4.2 Sonnet | Sonnet | 4.1 |
| 5 | 5.1 Sonnet · 5.2 Haiku | Sonnet/Haiku | 5.1, 5.2 (final read) |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical additive field/render
> work (the modal auto-renders, D5), escalate to Opus only at the marked gates
> (protocol, data-model, UI-structure, acceptance, final read). Print the model
> used per sub-task during implementation.
</content>
</invoke>
