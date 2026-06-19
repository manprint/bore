# Admin Dashboard — Vhost⇄Tunnels Section Parity & Flag-Completeness — Design & Implementation Plan

> **Status:** ✅ IMPLEMENTED 2026-06-19 (branch `webserver-log`). Gates green
> (`test_gates.sh`: fmt/clippy/build×3/`cargo test --all-features`) + `npm test`
> (48 pass; the 5 failures are pre-existing VPN/config-panel tests, out of scope).
> User doc: `docs/frontend/ADMIN_SECTIONS.md`.
>
> **Implementation deviations from the plan below:**
> - **D1 revised → self-sufficient `VhostEntry` (NOT a join).** Recon showed the
>   admin `Entry(Role::Vhost)` is created but unused for display, lacks per-vhost
>   TX/RX, and its `active` counter isn't incremented (only `VhostEntry.active` is).
>   All rich values (peer/notes/basic_auth/udp/carriers) are already params to
>   `serve_vhost_provider`, so `VhostEntry` (src/vhost.rs) gained
>   `peer/since/notes/basic_auth/udp/auto_reconnect` + per-subdomain
>   `relay_tx_bytes`/`relay_rx_bytes` (incremented in `relay_vhost`). `VhostView`
>   serializes straight from it — no admin-registry join, no match-key fragility.
> - **D6 IMPLEMENTED (not deferred):** `HelloVhost.auto_reconnect` added to the wire
>   (`#[serde(default)]`, backward-compatible) + `ProviderMeta.auto_reconnect`;
>   the Vhost section now shows the Auto-reconnect badge.
> - **`id` omitted from `VhostView`** (subdomain is the stable key; `id` not needed).
> - **Incidental fix:** the committed `cargo build --no-default-features` was already
>   broken (`used_direct` referenced unconditionally but udp-gated, src/server.rs);
>   gated the reference under `#[cfg(feature="udp")]` so the no-udp gate passes.
>
> ---
>
> **Status (original):** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Target:** the Vhost admin section presents the SAME columns/logic as Tunnels
> (subdomain replaces port, vhost-specifics retained) and ALL three sections
> (Tunnel/Secret/Vhost) surface every wire flag + every execution-info field.
> Minimize token usage during implementation (delegate mechanical sub-phases).

---

## 1. Context & problem

The three live-tunnel admin panels diverged. Tunnels is rich; Vhost is a stub.

- **Tunnels** `src/admin_ui/panels/tunnels.js:32-86` — columns
  `['Port','Peer','Flags','Connections','Uptime','TX','RX','Notes']`; flags via
  `tunnelBadges()` `tunnels.js:14-23` (https/force_https/basic_auth/udp/carriers>1/auto_reconnect);
  row-click → `openModal(detailRows(entry))`.
- **Secret** `src/admin_ui/panels/secret.js:16-74` — columns
  `['Role','Secret ID','Peer','Flags','Connections','Uptime','TX','RX']`; flags inline
  `secret.js:28-37` (udp/basic_auth/carriers>1). **No Notes column** though `notes` exists.
- **Vhost** `src/admin_ui/panels/vhost.js:16-74` — columns
  `['Subdomain','Connections','Carriers','Direct Opens','Headers','TLS']`. **Missing
  Peer, Flags, Uptime, TX, RX, Notes.** Flags inline (`tls` only) `vhost.js:28-35`.

Root cause is in the **backend serialization**, not just the JS:

- `VhostView` `src/admin_views.rs:115-137` carries only
  `{subdomain, active, carriers, direct_stream_opens, request_headers,
  response_headers, request_header_pairs, response_header_pairs, direct_pool, tls}`.
- `TunnelView` `src/admin_views.rs:49-83` carries the rich set
  `{id, peer, public_port, notes, basic_auth, https, force_https, carriers,
  auto_reconnect, udp, overlay, vpn_direct, active, uptime_secs,
  relay_tx_bytes, relay_rx_bytes}`.
- The vhost endpoint `src/admin_api.rs:116-194` builds `VhostView` from the **lean
  `VhostEntry`** `src/vhost.rs:339-360` (`pool, request/response_headers, direct,
  direct_stream_opens, active, webserver_log`) — which does **not** track
  peer/uptime/tx-rx/notes/basic_auth.

**Critical recon finding (changes the design):** a vhost provider ALSO registers a
**rich admin `Entry` with `Role::Vhost`** (`Role::Vhost` exists `src/admin.rs:30`;
created at `src/vhost.rs:585-605` via `NewEntry` with role/peer/secret_id(=subdomain)/notes/basic_auth).
So the rich execution data **already exists** in the admin registry (`Entry`
`src/admin.rs:43-99` → `EntryView` `src/admin.rs:146-199`); the vhost endpoint just
never reads it. The fix is a **join**, not new tracking (modulo the 0.1 probe).

Separately, `webserver_log` is on the wire (`TunnelOptions.webserver_log`
`src/shared.rs:173-208`; `HelloVhost.webserver_log` `src/shared.rs:739-759`) and stored
in `VhostEntry` `src/vhost.rs:359`, but is surfaced in **no view** — a flag the operator
passed yet cannot see.

### Goal
Make the Vhost section structurally identical to Tunnels — same columns, same flag
badges, same row-click detail modal — with `Subdomain` in place of `Port` and the
genuinely vhost-only metrics (Direct Opens, Headers, direct_pool, header pairs) retained.
Then audit all three sections so **every flag that reaches the server and every
execution-info field is visible** (as a column, a badge, or in the detail modal).
No wire-protocol break.

### Reference scenario (final acceptance test)

```
# Server with admin + udp
bore server --admin --admin-token T --udp ...

# A vhost tunnel exercising every displayable vhost flag:
bore vhost 127.0.0.1:8080 --subdomain demo --id c1 \
  --basic-auth user:pass --carriers 4 --udp --webserver-log /tmp/log \
  --notes "prod edge" --auto-reconnect            # (auto-reconnect: see D6)

# drive some HTTP traffic through https://demo.<server>/ ...

# GET /admin/api/v1/vhost  (token T)  ⇒ JSON object includes ALL of:
#   subdomain="demo", peer="<ip:port>", notes="prod edge",
#   basic_auth=true, udp=true, carriers=4, webserver_log=true,
#   uptime_secs>0, relay_tx_bytes>0, relay_rx_bytes>0, active>=0, tls=<bool>,
#   direct_stream_opens, direct_pool, request_header_pairs, response_header_pairs
#
# /admin/status Vhost section renders columns IDENTICAL to Tunnels:
#   Subdomain | Peer | Flags | Connections | Uptime | TX | RX | Notes | Direct Opens | Headers
#   Flags badges: TLS, UDP, Basic Auth, x4 carriers, weblog (mirrors Tunnels' badge logic)
#   row-click → modal lists EVERY field (incl direct_pool + header pairs)
#
# Same flag set is visible for an equivalent `bore local` (Tunnels) and the
# Secret section gains its missing Notes column.
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | `VhostView` is built by **joining** the rich admin `EntryView` (`Role::Vhost`) with the lean `VhostEntry`, matched on `subdomain == EntryView.secret_id`. | `src/admin_api.rs:116-194` iterates admin entries (not the vhost registry) as the driver; vhost-specifics looked up from `vhost_registry` by subdomain. No new per-vhost tracking for shared fields. |
| **D2** | Rich fields (peer, notes, basic_auth, udp, carriers, uptime, tx/rx, id) come from the admin `Entry`/`EntryView`; vhost-only fields (subdomain, direct_stream_opens, direct_pool, header lists, tls) from `VhostEntry`. | Single source of truth per field; no field is computed twice. |
| **D3** | `webserver_log` becomes a first-class displayed flag. Added to admin `Entry`+`EntryView`, populated from `TunnelOptions.webserver_log` (public) and `HelloVhost.webserver_log` (vhost); surfaced in `TunnelView` + `VhostView`. | Closes the "passed-but-invisible" gap for the only flag currently dropped. Secret has no `webserver_log` on the wire (`HelloSecret` `src/shared.rs:671-685`) → **not** shown for secret (D7). |
| **D4** | All new view fields are **additive JSON**; field NAMES match `TunnelView` exactly (`peer`, `notes`, `basic_auth`, `udp`, `carriers`, `uptime_secs`, `relay_tx_bytes`, `relay_rx_bytes`, `webserver_log`). Existing `VhostView` names unchanged. | Frontend reads with `?? default`; old field consumers unaffected; `t_legacy_data_compat` (`tests/admin_test.rs:996`) still passes. No wire change (view structs are server→browser JSON, already additive-tolerant). |
| **D5** | Flag badges unified into ONE shared `flagBadges(entry)` helper in `ui.js`; the three panels call it. Both `https` (tunnel) and `tls` (vhost) map to a "TLS"/"HTTPS" badge. | Removes 3-way duplicated badge logic (`tunnels.js:14-23`, `secret.js:28-37`, `vhost.js:28-35`); guarantees consistency by construction. |
| **D6** | `HelloVhost` has **no** `auto_reconnect` field (`src/shared.rs:739-759`). Vhost `--auto-reconnect` is client-display-only and not on the wire. v1: do NOT add it to the wire; `VhostView.auto_reconnect` is omitted and the badge simply never shows for vhost. | Avoids a wire change. Flag-coverage matrix (§Phase 2) records `auto_reconnect` as "client-local, not server-visible" for vhost — an honest gap, not a bug. (Optional future: add to `HelloVhost` behind `#[serde(default)]`.) |
| **D7** | Per-section flag coverage is bounded by what the wire carries. The detail **modal** (`detailRows` `src/admin_ui/modal.js:128-162`, auto-renders every non-`_` key) is the catch-all: any field in the view JSON is visible in the modal even if it has no dedicated column. | "Show ALL flags/info" is satisfiable by ensuring the **view JSON is complete**; the table shows the common columns, the modal shows the rest. |
| **D8** | Keep three separate `render()` panels (no generic mega-renderer). Consistency comes from shared `flagBadges()` + identical column semantics + identical row-click/modal wiring — not from collapsing the panels. | Lower-risk refactor; matches existing arch (`table()`/`openModal()` already shared). |
| **D9** | Vhost table columns = Tunnels columns with `Subdomain`↔`Port`, PLUS two trailing vhost-only columns `Direct Opens`, `Headers`. Remaining vhost-only data (direct_pool, header pairs) lives in the modal. | Satisfies "stesse colonne … campi specifici lasciali per vhost" without an unreadably wide table. |

---

## 3. Target architecture

### 3.1 Data sources & the join (backend)

```
                 admin registry (src/admin.rs)                vhost_registry (src/vhost.rs:446)
   ┌─────────────────────────────────────────┐        ┌──────────────────────────────────────┐
   │ Entry{ role=Vhost, peer, secret_id=sub,  │        │ VhostEntry{ pool, req/resp_headers,    │
   │   notes, basic_auth, udp, carriers,      │        │   direct, direct_stream_opens,         │
   │   since→uptime, relay_tx/rx, id,         │        │   active, webserver_log }              │
   │   (+ webserver_log  ← D3) }              │        │   key = subdomain                      │
   └───────────────┬──────────────────────────┘        └─────────────┬────────────────────────┘
                   │  EntryView (snapshot)                            │ lookup by subdomain
                   └──────────────┐                   ┌───────────────┘
                                  ▼                   ▼
                         VhostView  =  rich(EntryView Role::Vhost)  ⨝[secret_id==subdomain]  vhost-only(VhostEntry)
```

`vhost()` endpoint rewrite (`src/admin_api.rs:116-194`): driver becomes "for each
admin `EntryView` with `role==Role::Vhost`", join the matching `VhostEntry`. If a
vhost has an admin Entry but no live `VhostEntry` (or vice-versa), still emit the row
using whatever side exists (defaults for the missing side) — never drop a row.

### 3.2 Unified column model (frontend)

| | Tunnels (existing) | Secret (after 1.3) | Vhost (after 1.2) |
|---|---|---|---|
| identity | Port | Role, Secret ID | **Subdomain** |
| Peer | ✓ | ✓ | ✓ (new) |
| Flags | `flagBadges` | `flagBadges` | `flagBadges` (new) |
| Connections | ✓ | ✓ | ✓ |
| Uptime | ✓ | ✓ | ✓ (new) |
| TX / RX | ✓ | ✓ | ✓ (new) |
| Notes | ✓ | ✓ (new) | ✓ (new) |
| vhost-only | — | — | **Direct Opens, Headers** |
| modal (all keys) | ✓ | ✓ | ✓ (now rich) |

`flagBadges(entry)` badge set (shown only when truthy/relevant):
`HTTPS`(https), `Force HTTPS`(force_https), `TLS`(tls), `UDP`(udp),
`Basic Auth`(basic_auth), `x{n} carriers`(carriers>1), `auto-reconnect`(auto_reconnect),
`weblog`(webserver_log).

### 3.x Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Build table | `table(headers, rows)` — rows are key=header objects; cell may be HTMLElement | `src/admin_ui/ui.js:78-107` |
| Badge span | `badge(text, kind)` kinds: primary/warning/success/info/default | `src/admin_ui/ui.js:67` |
| Truncating notes cell | `notesCell(text, maxLen=50)` | `src/admin_ui/ui.js:113` |
| Byte/duration fmt | `fmtBytes`, `fmtDuration` (return "N/A" on null) | `src/admin_ui/ui.js:22,37` |
| Detail modal | `openModal(title, rows)` + `detailRows(obj)` (auto-fmt `_bytes`/`_secs`/bool/array; skips `_`-keys) | `src/admin_ui/modal.js:58,128-162` |
| Existing flag-badge logic to fold in | `tunnelBadges(t)` | `src/admin_ui/panels/tunnels.js:14-23` |
| Tunnels row+click pattern to mirror | row build + `_entry` stash + tbody click→openModal | `src/admin_ui/panels/tunnels.js:43-82` |
| Panel registration + endpoint | `{id, endpoint, render(el,data)}` | `src/admin_ui/registry.js:16-34`; per-panel `…:9-14` |
| Rich vhost data already tracked | admin `Entry`(Role::Vhost) | `src/admin.rs:43-99`, created `src/vhost.rs:585-605` |
| Vhost-only data | `VhostEntry` | `src/vhost.rs:339-360` |
| View structs | `TunnelView`/`SecretView`/`VhostView` | `src/admin_views.rs:49-83 / 85-112 / 115-137` |
| View population | `tunnels()/secret()/vhost()` | `src/admin_api.rs:64-88 / 92-112 / 116-194` |
| Public→Entry tx/rx wiring to mirror | tunnel splice relay-byte increments | (locate in 0.1; same path public tunnels use) |

---

## 4. New interface (CLI flags / API / config)

**No new CLI flags.** No new API endpoints. The four `/admin/api/v1/{tunnels,secret,vhost}`
endpoints (`src/admin_http.rs:82-137`) keep their paths; only the `vhost` and `tunnels`
JSON payloads grow additive fields (D3/D4). The dashboard navigation/registry is unchanged.

---

## 5. New protocol / data structures

**No wire-protocol change.** `TunnelOptions`/`HelloVhost`/`HelloSecret` are untouched
(`webserver_log` already exists on `TunnelOptions`+`HelloVhost` as `#[serde(default)]`).

Server-internal struct edits (not on the client↔server wire):

```
// src/admin.rs — Entry (≈ :43-99) + EntryView (≈ :146-199) + NewEntry
+ webserver_log: bool          // populated from TunnelOptions.webserver_log (public)
                               // and HelloVhost.webserver_log (vhost)

// src/admin_views.rs — TunnelView (:49-83)
+ webserver_log: bool

// src/admin_views.rs — VhostView (:115-137)  — additive, names match TunnelView
+ id: u64
+ peer: String
+ notes: Option<String>
+ basic_auth: bool
+ udp: bool
+ uptime_secs: u64
+ relay_tx_bytes: u64
+ relay_rx_bytes: u64
+ webserver_log: bool
// (carriers, active, tls already present; auto_reconnect omitted per D6)
```

**Backward-compat:** view structs are server→browser JSON only. Adding fields is
additive; the JS reads each with `?? default`. The existing
`t_legacy_data_compat` (`tests/admin_test.rs:996`) and `t_views_serialize_stable`
(`tests/admin_test.rs:413`) must still pass (extend the latter, do not weaken it).

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase must pass the gates
(`cargo fmt --all --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
`cargo test --all-features`, and `npm test`); **zero regressions**; update docs when
behavior/APIs change; **print the model used per sub-task**.

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Backend: enrich vhost serialization + surface webserver_log

> Pure additive JSON. No frontend change yet; ships safely alone (new fields simply
> go unused by the current JS). Establishes the data the UI will consume.

#### 0.1 Confirm/establish the vhost admin-Entry data source  ⟵ **Opus review gate (data-model/correctness)**
- **Model:** Sonnet (probe + wire-up) → Opus reviews findings before 0.3.
- **Files:** `src/vhost.rs:585-605` (vhost `NewEntry`), `src/admin.rs:43-99` (Entry),
  `src/server.rs:1245-1253` (vhost handshake), tunnel splice relay-byte increment site
  (grep `relay_tx_bytes`/`relay_rx_bytes` `fetch_add`).
- **Change:** Verify and, where missing, fix:
  (a) vhost `NewEntry` sets `peer`, `since`, `notes`, `basic_auth`, **`udp`**, **`carriers`**
  from `HelloVhost` (HelloVhost carries udp+carriers `src/shared.rs:739-759`); add the two if absent.
  (b) `relay_tx_bytes`/`relay_rx_bytes` on the vhost admin `Entry` are incremented on the
  vhost relay/splice path — mirror the public-tunnel increment site. If the vhost splice does
  not touch them, wire it the same way.
  (c) Record a 5-line fact note (in the PR description) of what was already correct vs newly wired.
- **Unit tests:** `src/admin.rs` tests (`:422-517`) — add `t_vhost_entry_has_exec_fields`
  asserting an `EntryView` built from a vhost `NewEntry` exposes peer/notes/basic_auth/udp/carriers.
- **e2e tests:** deferred to 0.3/2.1 (needs the view).
- **Done:** gates green; the three (a)/(b)/(c) confirmed; Opus signs off that the join in 0.3
  has a real, populated source for every parity field (esp. tx/rx are non-zero after traffic).

#### 0.2 Add `webserver_log` to admin `Entry` + `EntryView`
- **Model:** Sonnet (small, but touches NewEntry construction in two call sites).
- **Files:** `src/admin.rs:43-99` (Entry `+ webserver_log: bool`), `:146-199` (EntryView
  `+ webserver_log: bool` + snapshot copy), `NewEntry` struct + both creation sites:
  public-tunnel registration (set from `TunnelOptions.webserver_log`) and
  `src/vhost.rs:585-605` (set from `HelloVhost.webserver_log`).
- **Change:** thread the bool through; default `false` everywhere it isn't supplied
  (secret-provider creation passes `false`).
- **Unit tests:** extend `src/admin.rs` tests — `t_entryview_webserver_log` (true round-trips
  for a vhost entry, false for a secret entry).
- **e2e tests:** none (covered via views in 0.4/2.1).
- **Done:** gates green; `EntryView.webserver_log` reflects the source flag; existing admin.rs
  tests unchanged-green.

#### 0.3 Rework `VhostView` to full Tunnels parity (the join)  ⟵ **Opus review gate (data-model)**
- **Model:** Sonnet implements; Opus reviews the join semantics.
- **Files:** `src/admin_views.rs:115-137` (VhostView field additions per §5),
  `src/admin_api.rs:116-194` (rewrite builder to join — see §3.1).
- **Change:** add the §5 fields (names matching `TunnelView`). Rewrite `vhost()` to drive off
  admin `EntryView` where `role==Role::Vhost`, join `VhostEntry` by `subdomain==secret_id`;
  rich fields from `EntryView`, vhost-only from `VhostEntry`; emit a row even if one side is
  missing (defaults for the absent side, never drop). Keep existing vhost field names
  (`direct_stream_opens`, `direct_pool`, `*_header_pairs`, `tls`). Guard `direct*` fields under
  the existing `#[cfg(feature="udp")]` shape used by `VhostEntry`.
- **Unit tests:** `src/admin_views.rs` tests (`:333-500`) — `t_vhostview_has_parity_fields`
  (serialized keys ⊇ {peer,notes,basic_auth,udp,carriers,uptime_secs,relay_tx_bytes,
  relay_rx_bytes,webserver_log,id}); `src/admin_api.rs` tests (`:500-853`) —
  `t_vhost_join_matches_by_subdomain` (entry+vhostentry with same subdomain → one merged row;
  mismatched → row still emitted with defaults).
- **e2e tests:** extend `tests/admin_test.rs` — in `t_views_serialize_stable` (`:413`) assert the
  new VhostView keys are present and stable; add `t_api_vhost_shape` (mirror of
  `t_api_tunnels_shape` `:799`) asserting the JSON object exposes the parity fields.
- **Done:** gates green; `t_views_serialize_stable` extended (not weakened) and green;
  `GET /admin/api/v1/vhost` JSON contains every §5 field; Opus confirms join correctness +
  no-drop behavior.

#### 0.4 Add `webserver_log` to `TunnelView`; confirm Secret completeness
- **Model:** Sonnet.
- **Files:** `src/admin_views.rs:49-83` (TunnelView `+ webserver_log`), `src/admin_api.rs:64-88`
  (populate from `EntryView`). Secret: `src/admin_views.rs:85-112` already has `notes` — no field
  change; document `webserver_log` N/A for secret (D7).
- **Change:** surface `webserver_log` on TunnelView; no SecretView field change.
- **Unit tests:** extend `t_api_tunnels_shape` (`:799`) for `webserver_log`; add a 1-line note
  test/comment that SecretView intentionally lacks `webserver_log`.
- **e2e tests:** covered by 2.1 matrix assertions.
- **Done:** gates green; TunnelView JSON has `webserver_log`; secret unchanged.

---

### Phase 1 — Frontend: vhost parity + shared flag badges + secret Notes

> Consumes Phase 0's enriched JSON. Each sub-phase independently shippable. Requires
> `cargo build` after JS edits (build.rs `rerun-if-changed=src/admin_ui` re-embeds assets).

#### 1.1 Extract shared `flagBadges(entry)` into `ui.js`
- **Model:** Sonnet.
- **Files:** `src/admin_ui/ui.js` (new exported `flagBadges`), replace callers in
  `src/admin_ui/panels/tunnels.js:14-23` (drop `tunnelBadges`), `secret.js:28-37`, `vhost.js:28-35`.
- **Change:** `flagBadges(e)` returns an HTMLElement (span of spaced badges) built from the §3.2
  badge set; handle both `https`→"HTTPS"/`force_https`→"Force HTTPS" and `tls`→"TLS"; `udp`,
  `basic_auth`, `carriers>1`→`x{n} carriers`, `auto_reconnect`, `webserver_log`→"weblog". Each
  shown only when truthy. Reuse `badge()`.
- **Unit tests:** `test/admin_ui/badges.test.js` — `flagBadges` shows each badge when its flag set,
  hides when absent; `carriers=1` → no carrier badge, `carriers=4` → "x4 carriers"; `tls` and
  `https` both yield a TLS-class badge.
- **e2e tests:** none (JS unit covers).
- **Done:** `npm test` green; all three panels import the one helper; no inline badge arrays remain.

#### 1.2 Rewrite `vhost.js` to mirror `tunnels.js`  ⟵ **Opus review gate (acceptance assertion)**
- **Model:** Sonnet implements; Opus reviews against the §1 reference scenario.
- **Files:** `src/admin_ui/panels/vhost.js:16-74` (full render rewrite, keep `id`/`endpoint`
  metadata `:9-14`).
- **Change:** mirror `tunnels.js:43-82`. Columns
  `['Subdomain','Peer','Flags','Connections','Uptime','TX','RX','Notes','Direct Opens','Headers']`.
  Row: Subdomain=`subdomain`, Peer=`peer`, Flags=`flagBadges(v)`, Connections=`active`,
  Uptime=`fmtDuration(v.uptime_secs)`, TX=`fmtBytes(v.relay_tx_bytes)`, RX=`fmtBytes(v.relay_rx_bytes)`,
  Notes=`notesCell(v.notes,40)`, Direct Opens=`v.direct_stream_opens`,
  Headers=`badge('{req} req / {resp} resp','info')`. Stash `_entry`; tbody row-click →
  `openModal('Vhost '+subdomain, detailRows(v))` (modal auto-shows direct_pool, header pairs, all
  fields). Empty-state message preserved.
- **Unit tests:** replace `test/admin_ui/vhost-lean.test.js` with `vhost-parity.test.js` —
  asserts the 10 column labels in order, row maps the parity fields, `flagBadges` invoked,
  row-click opens modal with the entry.
- **e2e tests:** the §1 scenario assertion lands in 2.1.
- **Done:** `npm test` green; rendered Vhost columns == Tunnels columns (Subdomain↔Port) + 2
  vhost-only trailing; modal shows every field; Opus confirms scenario shape.

#### 1.3 Add `Notes` column to `secret.js`; adopt `flagBadges`
- **Model:** Sonnet.
- **Files:** `src/admin_ui/panels/secret.js:54` (columns), `:39-50` (row), `:28-37` (badges→helper).
- **Change:** append `'Notes'` column → `notesCell(secret.notes,40)`; replace inline badges with
  `flagBadges(secret)` (gains carriers/udp/basic_auth consistently).
- **Unit tests:** update `test/admin_ui/table-labels.test.js` for the secret column set incl Notes.
- **e2e tests:** covered by 2.1.
- **Done:** `npm test` green; Secret shows Notes; badges via shared helper.

---

### Phase 2 — Consistency audit + flag-completeness gate + docs

> Proves the "all flags / all info visible" requirement and locks it with tests.

#### 2.1 Cross-section flag-coverage audit + assertions  ⟵ **Opus review gate (acceptance)**
- **Model:** Opus authors the coverage matrix + acceptance assertions → Sonnet implements tests.
- **Files:** `tests/admin_test.rs` (extend), `test/admin_ui/table-labels.test.js` (extend),
  new `test/admin_ui/flag-coverage.test.js`.
- **Change:** Produce the matrix: for each tunnel type × each wire flag → {Column | Badge | Modal | N/A+reason}. Every wire flag must land in at least one of Column/Badge/Modal (modal is the
  catch-all via complete view JSON, D7). Encode as tests:
  - Rust: assert each view's serialized JSON contains its full field set (vhost parity fields,
    tunnel `webserver_log`); reuse `t_views_serialize_stable`.
  - JS: assert the three panels share column semantics (Peer/Flags/Connections/Uptime/TX/RX/Notes
    present in all three; identity column differs) and that `detailRows` over each view's field set
    yields a row per non-`_` field.
- **Unit tests:** `flag-coverage.test.js` — for a mock entry of each type, every documented flag is
  rendered somewhere (badge label present OR modal label present).
- **e2e tests:** `tests/admin_test.rs::t_all_flags_visible` — build one entry per type with all
  flags on, hit each endpoint, assert JSON exposes every documented flag; T-IDs:
  `T-VHOST-PARITY`, `T-FLAG-COVERAGE`.
- **Done:** gates + `npm test` + `cargo test --test admin_test` green; matrix has no unexplained
  "invisible" flag; Opus signs the matrix.

#### 2.2 Docs
- **Model:** Haiku.
- **Files:** `docs/frontend/ADMIN_DASHBOARD_PLAN.md` (or a new `ADMIN_SECTIONS.md`).
- **Change:** document the unified column model (§3.2), the flag-coverage matrix (2.1), the
  vhost join (§3.1), and the D6 `auto_reconnect` vhost gap.
- **Unit/e2e:** none (docs).
- **Done:** doc committed; matches shipped behavior; Opus final read.

---

## 7. Invariants to preserve / add

- **I-1:** All view changes are additive JSON; `t_legacy_data_compat`
  (`tests/admin_test.rs:996`) and the no-default-features build stay green.
- **I-2:** No client↔server wire change — `TunnelOptions`/`HelloVhost`/`HelloSecret` untouched;
  `webserver_log` stays `#[serde(default)]`.
- **I-3:** `VhostView` keeps its existing field names; only adds fields (frontend reads with `??`).
- **I-4:** Exactly ONE flag-badge code path (`flagBadges`); no per-panel badge duplication (D5).
- **I-5:** The vhost endpoint never drops a row when only one of {admin Entry, VhostEntry} exists
  for a subdomain (D1).
- **I-6:** Secret has no `webserver_log` on the wire → it is correctly absent from SecretView, not
  a coverage gap (D7).
- **I-7:** `t_views_serialize_stable` is *extended*, never weakened — serialization stays stable.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Vhost admin `Entry` doesn't increment `relay_tx_bytes`/`relay_rx_bytes` on its splice path → TX/RX always 0. | 0.1 explicitly verifies + wires it mirroring the public path; 2.1 e2e asserts TX/RX > 0 after driving traffic. |
| Join key mismatch — `EntryView.secret_id` may not equal the vhost subdomain. | Recon confirms vhost `NewEntry` stores `secret_id = subdomain` (`src/vhost.rs:585-605`); 0.3 unit test `t_vhost_join_matches_by_subdomain` pins it; no-drop fallback (D1/I-5) prevents data loss on any mismatch. |
| `VhostEntry.direct*` fields are `#[cfg(feature="udp")]` → view field cfg drift / build-without-udp break. | 0.3 guards the direct fields under the same cfg; gates run `cargo build --no-default-features` (test_gates.sh) to catch it. |
| Forgetting to rebuild after JS edit → stale embedded assets, tests pass on old JS. | §9 calls out `cargo build` re-runs build.rs (`rerun-if-changed=src/admin_ui`); 1.x done-criteria require `npm test` (runs against source JS directly). |
| Wider vhost table (10 cols) overflows on small screens. | Two vhost-only columns are the only additions over Tunnels' 8; existing CSS table is horizontally scrollable; non-blocking, note in 1.2. |
| `auto_reconnect` for vhost looks "missing" in the matrix. | D6 documents it as client-local / not-on-wire; 2.2 records the honest gap, not a bug. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `bash test_gates.sh` →
  `cargo fmt --all --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
  `cargo build --all-features`, `cargo build --no-default-features`, `cargo build --features vpn`,
  `cargo test --all-features`. Plus frontend: `npm test` (`node --test test/admin_ui/**/*.test.js`).
- **Unit tests:** Rust in `src/admin.rs:422-517`, `src/admin_api.rs:500-853`,
  `src/admin_views.rs:333-500`; JS in `test/admin_ui/*.test.js`.
- **e2e:** `cargo test --test admin_test` (`tests/admin_test.rs`, serial via `SERIAL_GUARD` on
  CONTROL_PORT 7835). **No netns / no sudo** for admin. After editing `src/admin_ui/*.js`, run
  `cargo build` (or any `cargo test`) so build.rs re-embeds assets before the Rust e2e checks asset bytes.
- **Acceptance:** the §1 reference scenario passes — proven by `T-VHOST-PARITY` + `T-FLAG-COVERAGE`
  (`tests/admin_test.rs`), `t_api_vhost_shape`, `t_vhostview_has_parity_fields`, and the JS
  `vhost-parity.test.js` + `flag-coverage.test.js`.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 Sonnet, 0.2 Sonnet, 0.3 Sonnet, 0.4 Sonnet | Sonnet | **0.1** (data source/correctness), **0.3** (data-model join) |
| 1 | 1.1 Sonnet, 1.2 Sonnet, 1.3 Sonnet | Sonnet | **1.2** (acceptance shape) |
| 2 | 2.1 Opus-authored matrix → Sonnet tests, 2.2 Haiku | Sonnet/Haiku | **2.1** (acceptance), **2.2** final docs read |

> Rule of thumb: start Sonnet; the work here is feature/refactor + tests (no pure-mechanical bulk
> except 2.2 docs → Haiku). Escalate to Opus only at the four gates above. Print the model used per
> sub-task during implementation.
