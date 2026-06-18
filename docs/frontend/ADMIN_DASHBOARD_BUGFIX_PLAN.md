# Admin Dashboard (`/admin/status`) — Bug-Fix Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture + recon synthesis).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = review gates only).
> **Target (correctness):** every `/admin/status` panel auto-refreshes, shows
> correct per-tunnel TX/RX, shows every operator-visible flag, renders certs
> once, and shows correct bandwidth. **Target (perf):** bandwidth measurement
> stays **off the per-byte data hot path** — zero tunnel-throughput regression
> (the app's prime invariant: max bandwidth for all tunnel types).
> **Token target:** delegate mechanical sub-phases to Haiku; backend and
> frontend tracks are independent and can run in parallel.

---

## 1. Context & problem

The `/admin/status` SPA (vanilla JS, `src/admin_ui/`, embedded into the binary
by `build.rs:34-104` via `include_bytes!`, served under `/admin/ui/*`) is fed by
`/admin/api/v1/*` JSON endpoints (`src/admin_api.rs`). One pass over the UI
surfaced seven confirmed bugs across both layers.

Recon traced each to root cause (file:line anchors below). Two reported symptoms
collapse onto one defect (BUG-0 polling), and the "probes hurt bandwidth?"
question resolved to **no** — see I-PERF.

| # | Symptom (user) | Layer | Root cause (confirmed) |
|---|----------------|-------|------------------------|
| **BUG-0** | Panels never auto-refresh; manual reload needed | Frontend | `app.js:59` dispatches `CustomEvent('panel:refresh')` to **no listener** — dead event. Interval fires, nothing re-fetches/re-renders. |
| **BUG-1** | `#/tunnels` TX/RX always `0.00 B` | Backend | Per-entry `relay_tx_bytes`/`relay_rx_bytes` (admin.rs:73-75) are **never incremented** for public/secret tunnels — only the **global** `grx`/`gtx` are (server.rs:1513-1514, 1555-1557). Frontend field names + formatter are correct. |
| **BUG-2** | Notes render as a clickable link, click does nothing | Frontend | `notesCell` (ui.js:113-137) sets clickable class `.notes-cell` **unconditionally** (line 115) but attaches the click handler **only** in the truncated branch (line 125). Short notes (≤ 40 chars) look clickable but have no handler. |
| **BUG-3** | `#/tunnels` flags incomplete: `carriers`, `force_https`, `auto_reconnect` missing | Both | `force_https` exists end-to-end (admin.rs:59,96,121) but tunnels.js renders only `https`/`basic_auth`/`udp` (tunnels.js:27-29). `carriers` is sent on the wire (TunnelOptions) but **dropped** at registration (absent from `Entry`/`NewEntry`/`EntryView`). `auto_reconnect` is **client-only**, never sent to the server. |
| **BUG-4** | `#/certs` shows the same cert twice | Backend | `certs()` (admin_api.rs:223-306) pushes the control cert (229-263) **and** the vhost cert (266-303) independently; when both paths resolve to the same file, two `CertView`s (`control` + `vhost`) describe one cert. No dedup. |
| **BUG-5** | `#/metrics` bandwidth numbers wrong + stale | Both | "Stale" = BUG-0 (polling). "Wrong" = `bandwidth_tx_bytes`/`bandwidth_rx_bytes` (admin_views.rs:233-235, populated admin_api.rs:338-339) are **cumulative totals** mislabeled as "Bandwidth" (implies a rate). |
| **BUG-6** (sweep) | Other panels also miss flags | Both | Flag-completeness sweep (Task B): SecretView missing `carriers`; VhostView missing `udp`; VpnLinkView missing `mtu`/`tun_queues`/policy flags. |

### Goal
Every panel auto-refreshes on its `refreshMs` cadence; per-tunnel TX/RX reflect
real bytes; all operator-visible flags appear in every panel; certs render once;
metrics show both cumulative totals and a derived rate; **and** none of the byte
accounting touches the per-byte splice hot path. Add unit (JS + Rust) and e2e
tests; update docs.

### Reference scenario (final acceptance test)
Run the exact client from the bug report against a `bore server --udp` and open
`/admin/status`:

```
client:  bore local 5353 -p 9000 -s mysecret --udp --carriers 4 \
         --https --force-https --auto-reconnect --notes "superdufs lenovo lavoro 5353"

/admin/status#/tunnels  ⇒ row shows: Port 9000, peer, badges {UDP, HTTPS, Force-HTTPS, x4 carriers, Auto-reconnect, Basic Auth?}, Notes as plain text (≤40 ⇒ not a fake link), TX>0 and RX>0 after traffic, and the table updates every 5 s with NO manual reload.
/admin/status#/certs    ⇒ exactly ONE card for a cert configured once (even if control+vhost point to it).
/admin/status#/metrics  ⇒ Total TX/RX (cumulative) AND a TX/RX rate that changes between 3 s refreshes; numbers plausible.
perf                    ⇒ tunnel throughput identical to a build without the counters (local_bench.sh, <1% delta).
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Wire per-entry TX/RX by adding `fetch_add(Relaxed)` to the **existing post-splice sites** that already update the global counters (server.rs:1513-1514, 1555-1557; secret.rs:537-538; vhost.rs:766-767). | One extra atomic per *connection close* (not per byte/chunk). Off hot path. Reuses the `(rx_bytes, tx_bytes)` already returned by `copy_bidirectional_with_sizes`. No new stream wrapper. |
| **D2** | Direction mapping fixed once: `copy_bidirectional_with_sizes(edge, stream, …)` returns `(rx, tx)` = `(edge→stream, stream→edge)`. Map `relay_rx_bytes += rx`, `relay_tx_bytes += tx` (server's perspective: rx = ingress from public client, tx = egress to public client) — same mapping the global counters already use. | Consistent semantics across global + per-entry; acceptance test asserts BOTH > 0 so a swapped mapping still passes — Opus reviews the mapping label, docs state it. |
| **D3** | `carriers` becomes a first-class admin field threaded `TunnelOptions → NewEntry → Entry → EntryView → TunnelView → JSON`. | Additive struct fields; `carriers==1` default ⇒ JSON gains `"carriers":1`, no other behavior change. |
| **D4** | `auto_reconnect` is sent to the server as a **new additive `TunnelOptions` field** with `#[serde(default)]` (mirrors the existing `TunnelOptions.udp` compat pattern, per CLAUDE.md). | Protocol change (additive, backward-compatible). Old client ↔ new server and vice-versa interop: missing field deserializes `false`. Lets the admin show a client-only flag. |
| **D5** | `force_https` needs **no backend change** (already in JSON); fix is purely adding the badge in tunnels.js. | Frontend-only sub-phase; do not touch the Rust side for it. |
| **D6** | Certs dedup by **canonicalized file path**; when control and vhost resolve to the same path, emit ONE `CertView` and merge labels (e.g. `"control+vhost"`). | Covers the field repro (single cert used for both). Stronger DER-fingerprint dedup noted as a future option but not required v1 (path dedup is deterministic and sufficient). |
| **D7** | Metrics: keep cumulative totals (rename label to **Total TX/RX**); compute the **rate on the frontend** as `Δbytes / Δt` across successive polls (now that polling works). No backend rate state. | No server-side history/snapshot needed. Rate is a pure function of two samples → unit-testable. Backend stays a dumb cumulative counter (cheap, off hot path). |
| **D8** | Polling fix: the interval calls an exported `refreshCurrent()` from `router.js` directly; delete the dead `CustomEvent`. | Single source of truth for "render the active route"; no event indirection. Testable by spying the scheduler. |
| **D9** | JS tests use Node's built-in `node:test` (zero npm deps) + a ~40-line hand-rolled DOM stub. Add a repo-root `package.json {"type":"module"}` so Node treats the existing ES-module `.js` as ESM. Tests live in `test/admin_ui/` (OUTSIDE `src/admin_ui/`, so `build.rs` does not embed them). | No browser/Playwright dependency. Browser-interactive paths (click, live polling render) get unit coverage via the DOM stub + an OPTIONAL manual checklist; API contracts get netns e2e. Accepted gap stated in §8. |
| **D10** | Notes: apply the clickable affordance (class + handler) **only** when the text is actually truncated; short notes render as plain text. | Removes the fake-link. Keeps the existing expand/collapse for long notes (the toggle code itself is correct). |

---

## 3. Target architecture

### 3.1 Data flow (unchanged shape, fixed wiring)
```
data path (HOT):   public client ──TCP──► server edge ──copy_bidirectional_with_sizes──► carrier substream ──► client ──► local svc
metering (COLD):   on copy() return → grx/gtx.fetch_add (exists)  +  entry.relay_{rx,tx}_bytes.fetch_add  (NEW, D1/D2)
admin read (COLD): GET /admin/api/v1/tunnels → AdminRegistry::snapshot() reads atomics (Relaxed load) → EntryView → TunnelView JSON
frontend:          setInterval(refreshMs) → refreshCurrent() → fetch endpoint → panel.render()  (D8 fixes the dead link)
```
The hot path gains **nothing**; counters are summed once when a proxied
connection finishes. This is why probes do not degrade throughput (I-PERF).

### 3.2 Reuse map (do not reinvent)
| Need | Reuse | Location |
|------|-------|----------|
| Per-entry TX/RX atomics (already declared) | `Entry.relay_tx_bytes` / `relay_rx_bytes` (`Arc<AtomicU64>`) | `src/admin.rs:73-75` |
| Counter accessor on the live handle | `Registration::relay_bytes()` | `src/admin.rs:271-276` |
| Post-splice increment pattern to copy | global `grx.fetch_add` / `gtx.fetch_add` | `src/server.rs:1513-1514`, `1555-1557` |
| Clone-into-task pattern for counters | `let grx = Arc::clone(&self.total_rx_bytes);` | `src/server.rs:1471-1472` |
| Secret-tunnel splice site | `copy_bidirectional_with_sizes` + global add | `src/secret.rs:535-538` |
| Vhost splice site | `copy_bidirectional_with_sizes` + global add | `src/vhost.rs:764-767` |
| VPN counting reference (already correct) | `CountingStream` | `src/vpn_server.rs:1553-1592` |
| Admin view structs to extend | `Entry`/`NewEntry`/`EntryView` | `src/admin.rs:55-99`, `103-136` |
| JSON view structs | `TunnelView`/`SecretView`/`VhostView`/`VpnLinkView`/`MetricsView` | `src/admin_views.rs:38-140`, `233-235` |
| JSON builders | `admin_api.rs` `tunnels()`/`certs()`/`metrics()` | `src/admin_api.rs:~45`, `223`, `314-339` |
| Serde-stability test to extend | `t_views_serialize_stable` | `src/admin_views.rs:250` |
| Byte/duration formatters (correct) | `fmtBytes`, `fmtDuration` | `src/admin_ui/ui.js:22-32`, near it |
| Notes cell (fix in place) | `notesCell` | `src/admin_ui/ui.js:113-137` |
| Badge builder | `badge` | `src/admin_ui/ui.js` |
| Panel render lists | `panels/{tunnels,secret,vhost,vpn,metrics}.js` | tunnels.js:25-46, secret.js:25-44, vhost.js:25-49, vpn.js:25-61, metrics.js:48,57 |
| Router render-on-route | `setupRouter`/`renderPanel` | `src/admin_ui/router.js` |
| Polling scheduler (broken) | `setupPolling` | `src/admin_ui/app.js:53-66` |
| e2e harness (extend) | `scripts/admin_dashboard_test.sh` (netns ns0+nscli, `aget`, jq/grep fallbacks, stale-build guard) | whole file |

### 3.3 CLI flag → panel coverage (target, from Task B sweep)
| Subcommand | Panel | Add to view + render |
|------------|-------|----------------------|
| `local` | tunnels | `carriers`, `auto_reconnect` (+ render `force_https`) |
| `proxy`/secret | secret | `carriers` |
| `vhost` | vhost | `udp` |
| `vpn` | vpn | `mtu`, `tun_queues` (informational); policy flags `relay_only`/`forward_accept`/`nat_masquerade` if cheaply available |

---

## 4. New interface (API / config / no new CLI flags)

No new CLI flags. JSON additions (additive, backward-compatible):

- `GET /admin/api/v1/tunnels[]`: `+ "carriers": u16` (default 1), `+ "auto_reconnect": bool` (default false). `force_https` already present.
- `GET /admin/api/v1/secret[]` (or equivalent): `+ "carriers": u16`.
- `GET /admin/api/v1/vhost[]`: `+ "udp": bool`.
- `GET /admin/api/v1/vpn.links[]`: `+ "mtu": u16`, `+ "tun_queues": u8` (and policy bools if available).
- `GET /admin/api/v1/certs[]`: deduped; `label` may be a merged string (`"control+vhost"`).
- `GET /admin/api/v1/metrics`: fields **unchanged on the wire** (`bandwidth_tx_bytes`/`bandwidth_rx_bytes` stay = cumulative totals); only the **frontend label** changes to "Total" and a derived rate is shown. (Optional rename deferred — keeps `T-REF4` green.)

---

## 5. New protocol / data structures

Only **D4** changes the wire:

```rust
// src/shared.rs — TunnelOptions (~575-600). ADD, with serde default for compat.
pub struct TunnelOptions {
    // … existing: udp, carriers, https, force_https, basic_auth, notes …
    #[serde(default)]                 // I-COMPAT: old peer omits ⇒ false
    pub auto_reconnect: bool,
}
```
- **Backward-compat:** `#[serde(default)]` ⇒ new server reads old client (`false`),
  old server ignores the field, new client → new server carries it. Mirrors the
  documented `TunnelOptions.udp` compat rule (CLAUDE.md).
- Client populates it where it builds `TunnelOptions` (grep `TunnelOptions {` in
  `src/client.rs`); the value is the same `auto_reconnect` already parsed for the
  reconnect loop (`main.rs` ~176/339/888 → ~1312/1422/1927).
- `carriers` (D3) is **already on the wire** in `TunnelOptions`; no protocol
  change — only the admin-registry plumbing is new.

---

## 6. Implementation phases

**Global rules:** tests alongside; every sub-phase passes gates `cargo fmt`,
`cargo clippy --features vpn -- -D warnings`, `cargo test --features vpn`, and
(JS) `node --test test/admin_ui/`; **zero regressions**; update docs when
behavior/APIs change; **print the model used per sub-task**. Backend phases (1-3)
and frontend phases (4-5) are independent tracks after Phase 0.

Each sub-phase: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — JS test harness scaffolding

> Pure additive. No app behavior change. Safe to land alone. Unblocks all JS unit tests.

#### 0.1 Add zero-dep Node test harness
- **Model:** Haiku (mechanical) → Opus review (toolchain choice only)
- **Files:** new `package.json` (repo root), new `test/admin_ui/dom-stub.js`, new `test/admin_ui/smoke.test.js`.
- **Change:** root `package.json` = `{"type":"module","scripts":{"test":"node --test test/admin_ui/"}}` (no deps). `dom-stub.js` exports a minimal `document` with `createElement` (returns `{tagName, className, textContent, innerHTML, children, classList:{add/remove/contains}, appendChild, addEventListener(map), dispatch(type)}`) and `escapeHtml`-safe text. `smoke.test.js` imports `../../src/admin_ui/ui.js` and asserts `fmtBytes` is a function. Confirm `build.rs` (walks only `src/admin_ui`, build.rs:36/104) does NOT embed `test/` or root `package.json`.
- **Unit tests:** `smoke.test.js` — `fmtBytes(0)==='0.00 B'`, `fmtBytes` typeof function (proves ESM import works under node).
- **e2e tests:** none (no behavior).
- **Done:** `node --test test/admin_ui/` exits 0; `cargo build --features vpn` still embeds the same asset set (diff `build.rs` output unchanged); `/admin/ui/package.json` returns 404 in the netns harness.

---

### Phase 1 — Backend: per-tunnel TX/RX counters (BUG-1)

> Additive. Independently shippable. **Hot-path/concurrency — Opus review gate.**

#### 1.1 Increment per-entry relay_tx/rx at post-splice sites
- **Model:** Sonnet implements · **Opus review gate** (hot-path + direction mapping D2)
- **Files:** `src/server.rs:1471-1472` (clone counters into task), `:1512-1514` (direct), `:1555-1557` (relay); `src/secret.rs:535-538`; `src/vhost.rs:764-767`; counter source `src/admin.rs:73-75`, `:271-276`.
- **Change:** Where the spawned proxy task already clones `grx`/`gtx` (server.rs:1471-1472), also clone the entry's `relay_tx_bytes`/`relay_rx_bytes` (`Registration::relay_bytes()`, admin.rs:271-276). At each post-splice `if let Ok((rx,tx))` block that does `grx/gtx.fetch_add`, add `etx.fetch_add(tx, Relaxed); erx.fetch_add(rx, Relaxed);` per D2. Same at secret.rs:537-538 and vhost.rs:766-767. **No per-byte work; no Mutex; Relaxed.** Do NOT introduce a CountingStream (the post-splice return value already has the totals).
- **Unit tests:** `src/admin.rs` `t_relay_counters_snapshot` — register an entry, `relay_bytes().0.fetch_add(N)`, `snapshot()` reports `relay_tx_bytes==N` (currently always 0 — this proves the read path was correct and only the write was missing).
- **e2e tests:** `T-BUG1-TXRX` in `admin_dashboard_test.sh` — after the client registers, push ≥1 MB through the public port (`curl`/`nc` to `PUB_PORT` against a local echo/responder in `nscli`), poll `/admin/api/v1/tunnels`, assert `relay_tx_bytes>0 && relay_rx_bytes>0` (jq + grep fallback).
- **Done:** gates green; `T-BUG1-TXRX` passes; existing `T-REF1` still passes; `local_bench.sh` throughput within <1% of pre-change baseline (I-PERF guard, see §9).

---

### Phase 2 — Backend: flag completeness (BUG-3, BUG-6)

> Additive JSON fields. Independently shippable.

#### 2.1 Thread `carriers` into the admin registry
- **Model:** Sonnet
- **Files:** `src/admin.rs:55-99` (`Entry`+`NewEntry`), `:103-136` (`EntryView`), `:163-188` (`register`), `:192+` (`snapshot`); `src/admin_views.rs:38-67` (`TunnelView`); `src/admin_api.rs:~45` (`tunnels()` builder); public-tunnel registration call site (grep `NewEntry {` in `src/server.rs`).
- **Change:** add `carriers: u16` to `Entry`/`NewEntry`/`EntryView`/`TunnelView`; populate from `opts.carriers` (TunnelOptions already carries it, server.rs:1469 scope) at the `NewEntry { … }` construction; surface in `snapshot()` + `tunnels()`.
- **Unit tests:** extend `t_views_serialize_stable` (admin_views.rs:250) — a `TunnelView{carriers:4,..}` serializes `"carriers":4`.
- **e2e tests:** `T-BUG3-CARRIERS` — start client with `--carriers 4`, assert JSON `carriers:4`.
- **Done:** gates green; `T-COMPAT` (`/admin/status/data`) shape still valid (additive only).

#### 2.2 Add `auto_reconnect` to the wire + admin (protocol)
- **Model:** Sonnet implements · **Opus review gate** (protocol/back-compat D4/§5)
- **Files:** `src/shared.rs:~575-600` (`TunnelOptions`, `#[serde(default)]`); client build site (grep `TunnelOptions {` in `src/client.rs`); `main.rs` (pass the parsed `auto_reconnect` into the options); admin thread-through same as 2.1 (`Entry`/`NewEntry`/`EntryView`/`TunnelView`/`tunnels()`).
- **Change:** add `auto_reconnect: bool` per §5; client sets it from the already-parsed flag; server stores it into `NewEntry.auto_reconnect`.
- **Unit tests:** `src/shared.rs` `t_tunnelopts_compat` — deserialize a JSON/bincode payload WITHOUT `auto_reconnect` ⇒ `auto_reconnect==false` (I-COMPAT). `t_views_serialize_stable` gains `"auto_reconnect"`.
- **e2e tests:** `T-BUG3-AR` — client with `--auto-reconnect`, assert JSON `auto_reconnect:true`; a mixed old/new interop note (manual) documented.
- **Done:** gates green; old-client/new-server interop test green; no change when flag absent.

#### 2.3 Close the cross-panel flag gaps
- **Model:** Haiku (mechanical view-field additions)
- **Files:** `src/admin_views.rs:70-92` (SecretView `+carriers`), `:96-111` (VhostView `+udp`), `:116-140` (VpnLinkView `+mtu,+tun_queues`[, policy bools]); matching builders in `admin_api.rs`; their registries if the source value isn't already stored (grep).
- **Change:** add the fields + populate. If a value is not currently tracked server-side (e.g. vpn `tun_queues`), wire it from the existing config/registration; if genuinely unavailable, OMIT and note it in docs (no fake zeros).
- **Unit tests:** extend `t_views_serialize_stable` per added field.
- **e2e tests:** `T-BUG6-VHOST-UDP` (vhost `udp` present) — add if the harness already exercises vhost; else assert via the secret/vpn endpoints already covered by `T-VPN`.
- **Done:** gates green; each added field appears in its endpoint JSON.

---

### Phase 3 — Backend: certs dedup (BUG-4)

> Additive/bugfix. Independently shippable.

#### 3.1 Dedup certs by canonical path, merge labels
- **Model:** Sonnet
- **Files:** `src/admin_api.rs:223-306` (`certs()`).
- **Change:** before pushing the vhost cert (line ~268), if its canonicalized path equals an already-pushed cert's path, merge the label of the existing entry (`"control"` → `"control+vhost"`) instead of pushing a duplicate. Use `std::fs::canonicalize` with a fallback to the raw path string on error. Keep behavior identical when paths differ.
- **Unit tests:** `src/admin_api.rs` `t_certs_dedup_same_path` — a stubbed `Server` whose control + vhost cert resolve to the same temp file ⇒ `certs().len()==1` and label contains both roles; `t_certs_distinct_paths` ⇒ `len()==2`.
- **e2e tests:** `T-BUG4-CERT-DEDUP` — start server with `--cert-file X --key-file Y` and a vhost config pointing `cert_file` at the same `X`; assert `/admin/api/v1/certs` array length `==1`. (Falls back to the existing single-cert `T-REF2` when vhost isn't configured.)
- **Done:** gates green; `T-REF2` (cert shape) still passes; `T-BUG4` passes.

---

### Phase 4 — Frontend: polling + metrics rate (BUG-0, BUG-5)

> User-visible. Independently shippable. Fixes the "nothing updates" class of bugs.

#### 4.1 Make polling actually re-render (BUG-0)
- **Model:** Sonnet implements · **Opus review gate** (core render/lifecycle)
- **Files:** `src/admin_ui/router.js` (export `refreshCurrent()`), `src/admin_ui/app.js:53-66` (call it; delete dead `CustomEvent`).
- **Change:** add `export function refreshCurrent()` to router.js that re-runs `renderPanel(getRoute())` (re-fetch + re-render the active route). In `setupPolling`, replace the `window.dispatchEvent(new CustomEvent('panel:refresh'))` (app.js:59) with a direct `refreshCurrent()` call; import it. Preserve focus/scroll where cheap (note in docs if not). Keep the `clearInterval` on route change (app.js:56,62).
- **Unit tests:** `test/admin_ui/polling.test.js` — inject a fake scheduler/`refreshCurrent` spy; assert that for a panel with `refreshMs>0` the scheduled callback invokes `refreshCurrent` (and is cleared on route change). (Refactor `setupPolling` to accept injectable deps if needed for testability.)
- **e2e tests:** OPTIONAL Playwright/manual checklist item `M-POLL` (documented in §9): open `#/metrics`, observe a value change within 2×`refreshMs` with no reload. Automated proxy = the unit test above (D9 accepted gap).
- **Done:** gates green; unit test proves the interval calls refresh; manual `M-POLL` verified once; the dead `panel:refresh` event no longer exists (grep returns nothing).

#### 4.2 Metrics: total + derived rate (BUG-5)
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/metrics.js:~48,57` (labels + rate); pure helper added to `metrics.js` or `ui.js`.
- **Change:** relabel the cumulative fields "Total TX/RX". Add `rateFromSamples(prev, cur)` = `{txbps: (cur.tx-prev.tx)/dt, rxbps: …}`, store the last `{tx,rx,t}` sample on the panel module between refreshes, render a "Rate TX/RX" (bits/s, `fmtBits`/`fmtBytes`+"/s"). First sample shows "—" (no prior). Guard `dt<=0`.
- **Unit tests:** `test/admin_ui/metrics-rate.test.js` — `rateFromSamples({tx:0,t:0},{tx:1_000_000,t:1})` ⇒ ~1 MB/s; `dt==0` ⇒ no NaN/Infinity.
- **e2e tests:** covered by `M-POLL` (rate changes between refreshes) + the unit test.
- **Done:** gates green; rate unit test passes; labels updated; `T-REF4` still green (wire fields unchanged, D7).

---

### Phase 5 — Frontend: tunnels render — notes + badges (BUG-2, BUG-3, BUG-6)

> User-visible. Depends on Phase 2 JSON for new badge data (TX/RX needs Phase 1; field names already correct).

#### 5.1 Fix notes fake-link (BUG-2)
- **Model:** Sonnet
- **Files:** `src/admin_ui/ui.js:113-137` (`notesCell`); `src/admin_ui/style.css` (`.notes-cell` affordance, ~line 159).
- **Change:** apply the clickable class + click handler **only** in the truncated (`text.length > maxLen`) branch (D10). Short text → plain `<span>` with no `notes-cell` clickable styling. Keep the existing expand/collapse for long notes (it works). Optionally add `title=full text` for hover.
- **Unit tests:** `test/admin_ui/notes.test.js` (DOM stub) — `notesCell('short',40)` → node has NO clickable class and NO click listener; `notesCell('x'.repeat(60),40)` → has class + listener, simulated click toggles `innerHTML` between truncated and full.
- **e2e tests:** manual `M-NOTES` (short note not a link; long note expands) — documented.
- **Done:** gates green; notes unit test passes; reference scenario's `--notes` (≤40) renders as plain text.

#### 5.2 Render all flags as badges (BUG-3, BUG-6)
- **Model:** Haiku (mechanical, per-panel)
- **Files:** `src/admin_ui/panels/tunnels.js:26-35` (+`force_https`,`carriers`,`auto_reconnect`); `secret.js:25-44` (+`carriers`); `vhost.js:25-49` (+`udp`); `vpn.js:25-61` (+`mtu`/`tun_queues`/policy).
- **Change:** extend each badge list. Suggest: `force_https`→`badge('Force-HTTPS','primary')`, `carriers>1`→`badge('x'+carriers,'info')` (or always show), `auto_reconnect`→`badge('Auto-reconnect','success')`. Factor a pure `tunnelBadges(t)` returning the label set for unit-testing (no DOM).
- **Unit tests:** `test/admin_ui/badges.test.js` — `tunnelBadges({https:true,force_https:true,udp:true,carriers:4,auto_reconnect:true,basic_auth:false})` returns labels containing `HTTPS, Force-HTTPS, UDP, x4, Auto-reconnect` and NOT `Basic Auth`.
- **e2e tests:** the reference-scenario client flags are asserted at the JSON layer by `T-BUG3-*`; badge rendering verified by the unit test + manual `M-BADGES`.
- **Done:** gates green; badge unit test passes; all reference-scenario flags visible.

---

### Phase 6 — Documentation

> **Opus final review gate** (docs are a deliverable, CLAUDE.md rule).

#### 6.1 Update dashboard docs + test docs
- **Model:** Haiku drafts · **Opus review gate**
- **Files:** `docs/frontend/ADMIN_DASHBOARD.md`; this plan's §9 referenced.
- **Change:** add a "Bug fixes" section documenting: TX/RX semantics + direction mapping (D2), the off-hot-path I-PERF guarantee, new fields (`carriers`, `auto_reconnect`, rendered `force_https`, vhost `udp`, vpn `mtu`/`tun_queues`), certs dedup (D6), metrics Total-vs-Rate (D7), polling fix (D8). Document the JS test harness: `node --test test/admin_ui/`, the DOM stub, the **rebuild caveat** (edit JS ⇒ `cargo build --release --features vpn` before the netns e2e, enforced by the harness stale-guard), and the manual checklist `M-*`.
- **Unit/e2e:** none.
- **Done:** docs match shipped behavior; Opus read confirms every D-row and new field is documented; manual checklist present.

---

## 7. Invariants to preserve / add

- **I-PERF (new, prime):** byte accounting stays off the per-byte hot path — only `fetch_add(Relaxed)` on the `(rx,tx)` totals returned once per closed connection (D1/D2). Never a per-byte/per-chunk Mutex, syscall, or channel send on the splice path. Guarded by the `local_bench.sh` throughput-delta check (§9).
- **I-COMPAT (new):** `TunnelOptions.auto_reconnect` is `#[serde(default)]` ⇒ old↔new client/server interop (D4). Proven by `t_tunnelopts_compat`.
- **I-ADDITIVE:** all JSON changes are additive; `/admin/status/data` legacy shape and `T-COMPAT` stay green; `carriers==1`/`auto_reconnect==false` defaults keep output equivalent to today plus the new keys.
- **I-NOREGRESS:** existing `T-REF1..4`, `T-VPN`, `T-AUTH`, `T-TRAVERSAL`, `T-SHELL`, `T-ASSET`, `T-COMPAT` all still pass.
- **I-EMBED:** `build.rs` embeds exactly `src/admin_ui/**`; `test/` and root `package.json` are NOT embedded (D9). `/admin/ui/package.json`→404.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Counter adds creep onto the hot path / hurt throughput | D1 places them at the post-splice return only; I-PERF + `local_bench.sh` <1% delta gate in Phase 1 Done. |
| TX/RX direction swapped (tx↔rx) | D2 fixes mapping once + Opus review; e2e asserts both >0; docs state semantics. |
| `auto_reconnect` protocol break with mixed versions | `#[serde(default)]` (D4) + `t_tunnelopts_compat` interop unit test. |
| Frontend interactive bugs (click/live render) not auto-tested (no browser dep) | D9: DOM-stub unit tests for logic + documented manual `M-*` checklist; API contracts fully covered by netns e2e. Gap stated, not silent. |
| `build.rs` accidentally embeds `package.json`/tests | Tests live in `test/` (outside `src/admin_ui`); Phase 0 Done asserts `/admin/ui/package.json`→404 + unchanged asset set. |
| Certs path dedup misses different-path-same-cert | Acceptable v1 (D6); DER-fingerprint dedup noted as future option. |
| Editing JS without rebuild ⇒ stale served assets in e2e | Harness already refuses a binary older than `src/` (admin_dashboard_test.sh:25-29); documented in Phase 6. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt`, `cargo clippy --features vpn -- -D warnings`, `cargo test --features vpn`, `node --test test/admin_ui/`.
- **Rust unit tests:** `t_relay_counters_snapshot` (admin.rs), `t_views_serialize_stable` extended (admin_views.rs:250), `t_tunnelopts_compat` (shared.rs), `t_certs_dedup_same_path`/`t_certs_distinct_paths` (admin_api.rs).
- **JS unit tests:** `test/admin_ui/{smoke,polling,metrics-rate,notes,badges}.test.js` via `node --test` + `dom-stub.js`.
- **e2e (netns, sudo):** extend `scripts/admin_dashboard_test.sh` — new `T-BUG1-TXRX`, `T-BUG3-CARRIERS`, `T-BUG3-AR`, `T-BUG4-CERT-DEDUP`, `T-BUG6-VHOST-UDP`; client invocation extended with `--carriers 4 --force-https --auto-reconnect --notes "…"`. Run: `sudo -n /abs/path/scripts/admin_dashboard_test.sh` (per sudoers; rebuild `cargo build --release --features vpn` as your user first — harness enforces).
- **Perf guard:** `scripts/local_bench.sh` before/after Phase 1 — tunnel throughput delta < 1% (I-PERF).
- **Manual checklist (browser, documented):** `M-POLL` (auto-refresh no reload), `M-NOTES` (short=plain, long=expand), `M-BADGES` (all flags visible).
- **Acceptance:** §1 reference scenario passes — `T-BUG1-TXRX`, `T-BUG3-CARRIERS`, `T-BUG3-AR`, `T-BUG4-CERT-DEDUP` + unit `badges`/`notes`/`metrics-rate`/`polling` + manual `M-POLL`/`M-NOTES`/`M-BADGES`.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary | Opus review gate |
|-------|---------------------|---------|------------------|
| 0 — JS harness | 0.1 Haiku | Haiku | toolchain sign-off |
| 1 — TX/RX backend | 1.1 Sonnet | Sonnet | **1.1** (hot-path/direction) |
| 2 — flags backend | 2.1 Sonnet · 2.2 Sonnet · 2.3 Haiku | Sonnet | **2.2** (protocol/compat) |
| 3 — certs dedup | 3.1 Sonnet | Sonnet | — |
| 4 — polling/metrics FE | 4.1 Sonnet · 4.2 Sonnet | Sonnet | **4.1** (render/lifecycle) |
| 5 — tunnels render FE | 5.1 Sonnet · 5.2 Haiku | Sonnet/Haiku | — |
| 6 — docs | 6.1 Haiku | Haiku | **6.1** (final docs read) |

> Start Sonnet; drop to Haiku for the mechanical view-field/badge/doc work
> (0.1, 2.3, 5.2, 6.1); escalate to Opus only at the four gates above. Print the
> model used per sub-task during implementation.
