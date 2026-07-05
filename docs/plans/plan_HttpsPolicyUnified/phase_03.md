# Phase 2 — Vhost wire + per-subdomain router (G2, G5)

> **Intent:** Carry `https_policy` from `HelloVhost` into the vhost provider
> registry and admin entry, and make the HTTP→HTTPS redirect decision
> per-subdomain (D7) instead of global. Fix the hardcoded `https=false`/
> `force_https=false` admin display (G5). Downgrade → warn + fallback (D4).
> **Shippable alone?** yes — with `https_policy = None` the router behaves exactly
> as today (global `VhostMode`).
> **Preconditions:** phase_01 DONE (fields + resolver + Warning exist).

---

## Sub-phases

### 2.1 Carry policy into the vhost provider registry + admin entry (G5)
- **Model:** Sonnet
- **Files:** `src/vhost.rs:538-675` (the `ClientMessage::HelloVhost` handling + `admin.register(NewEntry{..})` with the hardcoded `https:false, force_https:false` at `633-634`; `resolve_mode` per-registration at `661`); the vhost provider registry struct the router looks up by subdomain (find it: grep the lookup near `vhost.rs:1271-1274` and the struct it returns — likely a `VhostProviderEntry`/registry `DashMap` value).
- **Change:**
  1. Read `https_policy` out of the incoming `HelloVhost` message (add it to the destructuring at the `HelloVhost { .. }` match, `vhost.rs:728` and/or the handler at `538-675`).
  2. Add a field `https_policy: Option<HttpsPolicy>` to the vhost provider registry struct (the value the router reads by subdomain) and populate it from the message. This is what the router will consult in 2.2.
  3. Compute vhost capability once at registration: `let vhost_capable = mode.serves_https() && cert_present(&cfg);` (reuse `VhostMode::serves_https` at `vhost.rs:134` and `cert_present` at `vhost.rs:297`; `mode` is the resolved `VhostMode` from `661`).
  4. Compute EFFECTIVE display flags for the admin entry (fix G5 hardcode at `633-634`):
     ```rust
     let (adm_https, adm_force_https) = match https_policy {
         Some(p) => { let (h, f, _dg) = shared::resolve_https_policy(p, vhost_capable); (h, f) }
         None => (mode.serves_https(), mode.redirects_http()), // inherit global mode
     };
     ```
     Set `NewEntry.https = adm_https`, `NewEntry.force_https = adm_force_https` (replacing the two hardcoded `false`s).
- **Unit tests:** `vhost_admin_reflects_global_mode_when_policy_none` — a pure helper test: with `policy=None`, `mode=RedirectHttps` → `(true, true)`; with `mode=Http` → `(false, false)`. Extract the match above into a small pure fn `vhost_display_flags(policy, mode, capable)` in `vhost.rs` and unit-test it (also `Some(Off)`→`(false,false)`, `Some(Redirect)` capable→`(true,true)`, `Some(On)` incapable→`(false,false)`).
- **e2e tests:** covered in 2.4.
- **Done:** admin entry shows effective flags (no more hardcoded `false`); `policy=None` yields exactly the global-mode-derived display; gates green.

### 2.2 Per-subdomain redirect in the router — OPUS REVIEW GATE (hot path)
- **Model:** Sonnet; **Opus reviews** the reorder (correctness + zero-regression).
- **Files:** `src/vhost.rs:1180-1290` (the HTTP accept path: global redirect gate at `1212-1215` via `mode.redirects_http()` → `edge::redirect_to_https`; subdomain extraction + provider lookup at `1222-1275`).
- **Change:**
  1. Read `vhost.rs:1180-1290` fully first — understand where the request head is read and where the subdomain/provider are resolved.
  2. Move the redirect decision to AFTER the subdomain is extracted and the provider entry (2.1) is looked up, reusing the already-read head (do NOT double-read the request head). Compute:
     ```rust
     let effective_redirect = match entry.https_policy {
         Some(p) => matches!(p, HttpsPolicy::Redirect) && vhost_capable_now,
         None => mode.redirects_http(), // global default — byte-identical to today
     };
     if effective_redirect { /* existing edge::redirect_to_https path */ }
     ```
     where `vhost_capable_now` = `mode.serves_https() && <vhost cert present>` evaluated at request time (or stored on the entry from 2.1 — prefer the stored `entry` capability to avoid re-reading cert state on the hot path).
  3. **Unknown-subdomain preservation:** if no provider entry is found for the subdomain, use the global `mode.redirects_http()` for the redirect decision exactly as today (no entry ⇒ no per-tunnel override ⇒ global behavior). Keep the existing not-found handling otherwise unchanged.
  4. Do NOT change TLS termination on :443 — it stays global (shared wildcard cert). Per D10, an `off` subdomain reached over HTTPS is still served over HTTPS.
- **Unit tests:** if a pure decision helper can be factored (`fn should_redirect(entry_policy: Option<HttpsPolicy>, global: VhostMode, capable: bool) -> bool`), unit-test it: `None + RedirectHttps → true`; `None + Both → false`; `Some(Redirect) + Both + capable → true`; `Some(Off) + RedirectHttps → false` (opt-out); `Some(Redirect) + Http (incapable) → false`.
- **e2e tests:** T-HP-VH1, T-HP-VH2 (2.4).
- **Done:** Opus confirms: (a) head read once, (b) `None` path is byte-identical to the old global gate including the unknown-subdomain case, (c) no change to :443 TLS. `vhost_redirect_mode` (existing test) still passes unchanged.

### 2.3 Vhost downgrade warning + fallback (D4)
- **Model:** Sonnet
- **Files:** `src/vhost.rs:538-675` (registration; where `ServerMessage::VhostReady` is sent to the client).
- **Change:** When `https_policy` is `Some(On|Redirect)` and `vhost_capable` is false (computed in 2.1):
  - `warn!(%subdomain, "vhost server not configured for HTTPS (mode={mode:?}, cert={}); serving this subdomain over HTTP", cert_present(&cfg));`
  - if `https_policy.is_some()` (always true here) send `ServerMessage::Warning(<same message>)` on the control channel (non-fatal). Never bail.

  > **CRITICAL ORDERING (wire, from Opus review of phase_01 §0.3):** the client's
  > one-shot vhost registration read (`client.rs:603` `VhostReady`, `631` carrier
  > token) BAILs on an unexpected `ServerMessage::Warning`; only the main control
  > loop (`client.rs:981`) handles it non-fatally. Send the `Warning` **AFTER**
  > `ServerMessage::VhostReady` and any `CarrierToken`, never before, so the client
  > consumes it on its main loop. The admin entry (2.1) already reflects the
  > downgraded flags, so deferring the message changes no data-path behavior.
- **Unit tests:** none (integration; see 2.4 T-HP-VH3).
- **e2e tests:** T-HP-VH3 (2.4).
- **Done:** an HTTPS-requesting vhost tunnel against a no-HTTPS server comes up on HTTP with a warning; never rejected. Gates green.

### 2.4 Vhost policy tests
- **Model:** Sonnet
- **Files:** `tests/vhost_test.rs` (reuse `spawn_server_vhost` at `143-150`, tests at `273+`; existing `vhost_redirect_mode`).
- **Change:** Add integration tests sending `HelloVhost { https_policy: Some(..), .. }`:
  - **T-HP-VH1** `vhost_entry_redirect_overrides_both`: server `mode=both` (cert present), subdomain registered with `Some(Redirect)`. Assert `GET http://sub.base` → `308` to `https://sub.base` for THAT subdomain; a second subdomain with `None` is NOT redirected (still served on HTTP).
  - **T-HP-VH2** `vhost_entry_off_optsout_of_global_redirect`: server `mode=redirect-https` (cert present), subdomain registered with `Some(Off)`. Assert `GET http://sub.base` is served plain (NO 308); a `None` subdomain on the same server IS redirected (global default preserved).
  - **T-HP-VH3** `vhost_https_request_no_cert_warns_and_serves_http`: server `mode=http` (no vhost cert), subdomain with `Some(On)`. Assert tunnel comes up, HTTP is served, admin shows `https=false`, and (if capturable) the client received a `Warning`.
  - **T-HP-VH4** `vhost_policy_none_is_byte_identical`: regression — a subdomain with `Some(None-equivalent)`… i.e. `https_policy: None` under `mode=redirect-https` behaves exactly like the pre-change `vhost_redirect_mode`.
- **Unit tests:** the pure helpers in 2.1/2.2 as named there.
- **e2e tests:** T-HP-VH1..VH4.
- **Done:** all four pass; `vhost_redirect_mode` and existing vhost tests pass unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --test vhost_test` and `cargo test --all-features`
- **Regression guard:** `vhost_redirect_mode` + all `tests/vhost_test.rs` pass unchanged; T-HP-VH4 proves `None` == today.

## Phase done criterion

Per-subdomain `redirect`/`off`/`on` overrides work against a capable server
(T-HP-VH1/VH2), an HTTPS request to an incapable server downgrades with a warning
(T-HP-VH3), the admin entry shows effective flags (no hardcoded `false`), and a
`None`-policy subdomain is byte-identical to today (T-HP-VH4).
