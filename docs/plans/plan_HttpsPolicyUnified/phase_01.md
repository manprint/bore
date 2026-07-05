# Phase 0 — Scaffolding: enum, resolver, wire fields, Warning variant

> **Intent:** Add all shared, pure-additive building blocks (the `HttpsPolicy`
> enum, the capability-bounded resolver, the two wire fields, and the non-fatal
> `ServerMessage::Warning` variant) with ZERO behavior change. Nothing reads the
> new fields yet.
> **Shippable alone?** yes — pure-additive; existing behavior byte-identical.
> **Preconditions:** none.

---

## Sub-phases

### 0.1 Add `HttpsPolicy` enum
- **Model:** Sonnet
- **Files:** `src/shared.rs` (add near `TunnelOptions`, ~line 275, before the struct).
- **Change:** Add a public enum:
  ```rust
  /// Per-tunnel HTTPS behavior requested by the client. `None` (an
  /// `Option<HttpsPolicy>`) means "inherit the server default".
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
  #[serde(rename_all = "lowercase")]
  #[clap(rename_all = "lowercase")]
  pub enum HttpsPolicy {
      /// No TLS termination, no redirect; plain HTTP/raw only.
      Off,
      /// Terminate TLS; serve both HTTP and HTTPS; no redirect.
      On,
      /// Terminate TLS and 308-redirect plain HTTP to HTTPS.
      Redirect,
  }
  ```
  Mirror the derive pattern at `src/transfer.rs:52-72` (`clap::ValueEnum`). Confirm
  `serde::{Serialize, Deserialize}` are already imported in `shared.rs` (they are —
  `TunnelOptions` derives them). Add `use clap::ValueEnum;` only if the crate path
  `clap::ValueEnum` is not usable inline (it is via the derive macro path; no extra
  `use` needed — `transfer.rs` uses the bare `clap::ValueEnum` in the derive).
- **Unit tests:** `https_policy_serde_roundtrip` — assert `serde_json::to_string(&HttpsPolicy::Redirect) == "\"redirect\""` and each variant round-trips; `https_policy_valueenum_parse` — assert `HttpsPolicy::from_str("on", false)` (ValueEnum) yields `On` and `"bogus"` errors. Place in the existing `#[cfg(test)] mod tests` in `shared.rs`.
- **e2e tests:** none (no behavior change).
- **Done:** `cargo build` + `cargo test --lib shared` green; enum is unused elsewhere (warnings allowed only if `#[allow(dead_code)]` NOT needed because tests reference it).

### 0.2 Add the capability-bounded resolver
- **Model:** Sonnet
- **Files:** `src/shared.rs` (immediately after the enum from 0.1).
- **Change:** Add a pure function:
  ```rust
  /// Resolve a requested per-tunnel HTTPS policy against server capability into
  /// the effective `(https, force_https)` edge flags.
  ///
  /// `capable` = the server can terminate TLS for this tunnel family
  /// (public: `self.tls.is_some()`; vhost: cert present and mode serves https).
  /// Returns `(https, force_https, downgraded)` where `downgraded` is true when
  /// the client asked for TLS (`On`/`Redirect`) but the server is not capable,
  /// so the caller must warn and fall back to plain HTTP.
  pub fn resolve_https_policy(policy: HttpsPolicy, capable: bool) -> (bool, bool, bool) {
      match policy {
          HttpsPolicy::Off => (false, false, false),
          HttpsPolicy::On if capable => (true, false, false),
          HttpsPolicy::On => (false, false, true),
          HttpsPolicy::Redirect if capable => (true, true, false),
          HttpsPolicy::Redirect => (false, false, true),
      }
  }
  ```
- **Unit tests:** `resolve_policy_off` → `(false,false,false)` regardless of `capable`; `resolve_policy_on_capable` → `(true,false,false)`; `resolve_policy_on_incapable` → `(false,false,true)`; `resolve_policy_redirect_capable` → `(true,true,false)`; `resolve_policy_redirect_incapable` → `(false,false,true)`.
- **e2e tests:** none.
- **Done:** all five unit tests pass; `cargo clippy --all-targets -D warnings` clean.

### 0.3 Add non-fatal `ServerMessage::Warning(String)` — OPUS REVIEW GATE (wire)
- **Model:** Sonnet implements; **Opus reviews** the wire-compat argument before merge.
- **Files:** `src/shared.rs` (`ServerMessage` enum, ~line 1080+, add as the LAST variant); `src/client.rs` (the `ServerMessage` match arms, see `client.rs:212,252,370,410,602,637,789`).
- **Change:**
  1. In `shared.rs`, append `Warning(String)` as the FINAL variant of `ServerMessage` (same wire-compat rule as `ClientMessage::Heartbeat`; do NOT reorder existing variants). Add a doc comment: `/// Non-fatal advisory from the server. The client prints it and CONTINUES. Sent only to policy-aware clients (they sent https_policy = Some). Old clients never receive it.`
  2. First, VERIFY the `ServerMessage` codec: grep how `ServerMessage` is (de)serialized (`Delimited`, serde_json vs bincode). Confirm appending a trailing variant does not shift existing wire encodings (serde_json externally-tagged = safe by name; bincode = safe by trailing index). Record the finding in `resume.md` "Decisions changed at runtime" if the codec forces a different approach.
  3. In `client.rs`, add a match arm for `ServerMessage::Warning(msg)` in EVERY place the client reads server messages (control loop). It must `tracing::warn!("{msg}")` (or print to stderr consistent with the surrounding style) and CONTINUE the loop — never `bail!`/`return Err`. Do NOT treat it like `Error`.
- **Unit tests:** `server_message_warning_roundtrip` — `serde_json` round-trip of `ServerMessage::Warning("x".into())`; `server_message_warning_is_last_variant` — a doc/comment assertion is not testable, so instead add a test that deserializing a payload WITHOUT the Warning variant (an older-style message, e.g. `Ok`) still succeeds (guards against accidental reorder).
- **e2e tests:** deferred to phase_02 (T-HP-PUB2 exercises a real Warning delivery).
- **Done:** gates green; Opus has signed off that (a) the variant is last, (b) the client handles it non-fatally in all read sites, (c) the codec tolerates it. **Behavior note:** no message is ever SENT yet in this phase — only the receive path is wired.

> **Alternative (recorded, not chosen):** if Opus judges the new variant too
> risky for the codec in use, fall back to server-log-only + admin-dashboard
> display of the downgrade, dropping the client-visible warning. Update D5/I-6
> and the affected sub-phases (1.2, 2.3) accordingly.

### 0.4 Add `https_policy` wire fields
- **Model:** Sonnet
- **Files:** `src/shared.rs` — `TunnelOptions` (~line 281-325) and `HelloVhost` (~line 934-966).
- **Change:** Add to BOTH structs, as the LAST field:
  ```rust
  /// Per-tunnel HTTPS policy. `None` = inherit the server default (byte-identical
  /// to the pre-policy behavior). `#[serde(default)]` keeps the wire backward-compatible.
  #[serde(default)]
  pub https_policy: Option<HttpsPolicy>,
  ```
  Because `TunnelOptions` derives `Default`, `Option<HttpsPolicy>` defaults to
  `None` automatically. `HelloVhost` — check whether it derives `Default` or is
  built by explicit struct literal; if built by literal (it is, at
  `client.rs:570-581`), the literal in phase_04 will set it. For this phase, only
  ADD the field; do NOT wire any producer yet. To keep the repo compiling, update
  the `HelloVhost` construction site(s) to add `https_policy: None` (grep every
  `HelloVhost {` literal — `client.rs:570`, `sshgw.rs` vhost path, any test
  builders in `tests/vhost_test.rs`/`tests/ssh_gateway_test.rs`). Same for any
  `TunnelOptions { .. }` literal that does NOT use `..Default::default()` (grep;
  most use `..Default::default()` so are unaffected).
- **Unit tests:** `tunnel_options_default_policy_none` — `TunnelOptions::default().https_policy.is_none()`; `hello_vhost_serde_omits_default_policy` — serialize a `HelloVhost` with `https_policy: None` and assert the JSON round-trips and an OLD-style JSON string WITHOUT the `https_policy` key deserializes to `None` (interop). Reuse the existing wire-compat test style at `shared.rs:1718`/`shared.rs:2303`.
- **e2e tests:** none.
- **Done:** `cargo build` (all features) + `cargo test` green; NO producer sets the field to non-`None` yet (grep confirms only `None`); existing wire-compat tests unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --lib` and `cargo test --all-features`
- **Regression guard:** all existing `shared.rs` wire-compat tests (`shared.rs:1718`, `shared.rs:2303`) still pass unchanged; `tests/tls_test.rs` and `tests/vhost_test.rs` compile and pass.

## Phase done criterion

The enum, resolver, `ServerMessage::Warning` (receive path only), and both
`https_policy` fields exist and compile with all features. Every new unit test in
0.1–0.4 passes. No producer emits a non-`None` policy or a `Warning` message yet,
so a full `cargo test --all-features` shows zero behavioral diffs from `main`.
