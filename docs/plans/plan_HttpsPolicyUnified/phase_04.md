# Phase 3 — Native CLI unification (G3)

> **Intent:** Expose `--https [off|on|redirect]` on BOTH `bore local` and
> `bore vhost`, wire it into `TunnelOptions.https_policy` / `HelloVhost.https_policy`,
> and keep `--force-https` as a deprecated alias → `Redirect`.
> **Shippable alone?** yes — server already consumes the policy (phases 1-2).
> **Preconditions:** phase_01, phase_02, phase_03 DONE.

---

## Sub-phases

### 3.1 Add the `--https` value flag to both subcommands
- **Model:** Sonnet
- **Files:** `src/main.rs` — `bore local` args (~71-225; current `--https` bool at `112`, `--force-https` bool at `117`) and `bore vhost` args (~324-413).
- **Change:**
  1. On `bore vhost` (new): add
     ```rust
     /// Per-tunnel HTTPS policy: off | on | redirect. Bare `--https` = on.
     /// Absent = inherit the server default (--vhost-mode). Client value overrides
     /// the server default, bounded by server capability (falls back to HTTP with
     /// a warning if the server cannot serve HTTPS).
     #[clap(long, value_name = "off|on|redirect", value_enum,
            num_args = 0..=1, default_missing_value = "on")]
     https: Option<shared::HttpsPolicy>,
     ```
  2. On `bore local`: REPLACE the existing `--https` bool (`112`) with the SAME
     `https: Option<shared::HttpsPolicy>` field (identical attributes). Keep the
     existing `--force-https` bool (`117`) — it becomes a deprecated alias (3.3).
     > **Behavior change (loud):** `bore local --https` (bare) still resolves to
     > `On` via `default_missing_value = "on"`, so today's `bore local --https`
     > invocations are unchanged. Existing `--https --force-https` still ends up as
     > `Redirect` (via 3.3).
  3. `num_args = 0..=1` + `default_missing_value` is a new clap idiom for this repo
     (none exists today — recon confirmed). Verify the clap version in `Cargo.toml`
     supports `default_missing_value` (clap 4.x does). `value_enum` binds to the
     `clap::ValueEnum` derive from phase_01 §0.1.
- **Unit tests:** in the existing `main.rs` `#[cfg(test)]` CLI-parse tests (recon: `main.rs:3156-3166` parses `--vhost-mode both`), add:
  - `parse_local_https_bare` — `Cli::parse_from(["bore","local","5000","--https"])` → `https == Some(On)`.
  - `parse_local_https_redirect` — `--https redirect` → `Some(Redirect)`.
  - `parse_local_https_off` — `--https off` → `Some(Off)`.
  - `parse_local_https_absent` — no flag → `None`.
  - `parse_vhost_https_redirect` — same on `bore vhost`.
- **e2e tests:** covered by phases 1-2 behavior once wired (3.2).
- **Done:** all parse tests pass; `bore local --help` / `bore vhost --help` show `--https <off|on|redirect>`; gates green.

### 3.2 Wire the flag into the producers
- **Model:** Sonnet
- **Files:** `src/main.rs:1541-1553` (`bore local` builds `TunnelOptions`); `src/client.rs:570-581` (`bore vhost` sends `ClientMessage::HelloVhost { .. }`); the plumbing that passes the parsed `https` value from `main.rs` into `client.rs` for the vhost path.
- **Change:**
  1. `bore local`: set `TunnelOptions { https_policy: https, .. }` (the parsed `Option<HttpsPolicy>`). Leave the legacy bools populated for old-server interop: `https: matches!(https, Some(On)|Some(Redirect)), force_https: matches!(https, Some(Redirect)) || force_https_flag`. (New server prefers `https_policy`; old server reads the bools — D6.)
  2. `bore vhost`: thread the parsed `Option<HttpsPolicy>` through to the `HelloVhost` literal at `client.rs:570-581` and set `https_policy: <value>`.
  3. Grep for any other `TunnelOptions {`/`HelloVhost {` literals touched in phase_01 §0.4 and ensure they now pass the real value (not `None`) where a user flag exists.
- **Unit tests:** none new (parse tests in 3.1; behavior in phases 1-2 e2e).
- **e2e tests:** re-run T-HP-PUB* / T-HP-VH* against a client built from these flags in phase_08 netns.
- **Done:** a native `bore local 5000 --https redirect` and `bore vhost --subdomain x --id x --https redirect` produce the policy on the wire; gates green.

### 3.3 `--force-https` deprecation alias
- **Model:** Sonnet
- **Files:** `src/main.rs` (`bore local` post-parse, where `TunnelOptions` is built, ~1541).
- **Change:** After parsing, if `force_https` (the deprecated bool) is `true`:
  ```rust
  if force_https_flag {
      warn!("--force-https is deprecated; use `--https redirect`");
      https = Some(HttpsPolicy::Redirect); // force-https wins, preserves old semantics
  }
  ```
  Then build `TunnelOptions` from the (possibly overridden) `https`. This preserves
  `bore local --https --force-https` == `Redirect` and `bore local --force-https`
  == `Redirect`. Do NOT add `--force-https` to `bore vhost` (it never had it).
- **Unit tests:**
  - `parse_local_force_https_maps_redirect` — parse `["bore","local","5000","--force-https"]`, run the post-parse mapping, assert resulting `https == Some(Redirect)`.
  - `parse_local_https_and_force_https_redirect` — `--https on --force-https` → `Some(Redirect)` (force wins).
- **e2e tests:** none.
- **Done:** deprecated flag still works, emits one deprecation warning, maps to `Redirect`; tests pass.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --bin bore` (CLI parse tests) and `cargo test --all-features`
- **Regression guard:** existing `--vhost-mode` parse test (`main.rs:3156-3166`) unchanged; `bore local --https` (bare) still yields TLS-on.

## Phase done criterion

`--https off|on|redirect` parses on both subcommands (bare = on), reaches the wire
as `https_policy`, and `--force-https` still works as a deprecated `Redirect` alias
with a one-time warning. All 3.1/3.3 parse tests pass.
