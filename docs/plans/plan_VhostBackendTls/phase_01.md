# Phase 0 — Wire + entry + TLS-connector scaffolding

> **Intent:** Add the data fields (`backend_tls`, `backend_tls_sni`) to the wire
> message and the registry entry, and a reusable TLS-connector helper — all
> pure-additive, with NO behavior change. Nothing sets `backend_tls` true yet.
> **Shippable alone?** yes — unused fields + a private helper; production
> behavior is unchanged.
> **Preconditions:** none.

All line anchors are as-of-planning; if stale, locate the named symbol.

---

## Sub-phases

### 0.1 Add `backend_tls` / `backend_tls_sni` to `HelloVhost`
- **Model:** Haiku
- **Files:** `src/shared.rs:1173` (the `HelloVhost` variant of the client message enum).
- **Change:**
  - Append two fields to the `HelloVhost` variant, AFTER the existing last field
    (currently `https_policy`), to preserve wire order:
    - `#[serde(default)] backend_tls: bool,`
    - `#[serde(default)] backend_tls_sni: Option<String>,`
  - Mirror exactly the `#[serde(default)]` style already used on `carriers`,
    `udp`, `webserver_log`, `auto_reconnect`, `local_host`, `local_port`,
    `https_policy` in the same variant.
  - Fix every construction site of `HelloVhost` that now fails to compile by
    adding `backend_tls: false, backend_tls_sni: None,` (there is one native
    sender at `src/client.rs:595`; leave its values `false`/`None` in this phase
    — real plumbing lands in Phase 2). If any other constructor exists, set the
    same defaults.
- **Unit tests:** in the existing `#[cfg(test)]` module that covers message
  serde (locate the module that tests `ClientMessage`/`HelloVhost` round-trips;
  if none exists, add tests to `src/shared.rs` `mod tests`):
  - `hello_vhost_backend_tls_serde_roundtrip` — serialize a `HelloVhost` with
    `backend_tls: true, backend_tls_sni: Some("app".into())`, deserialize, assert
    both fields survive.
  - `hello_vhost_old_wire_defaults_backend_tls_off` — deserialize a serialized
    `HelloVhost` value produced WITHOUT the two new fields (construct the JSON/
    byte form omitting them, matching the existing codec) and assert
    `backend_tls == false` and `backend_tls_sni == None`. This proves I-2.
- **e2e tests:** none (no behavior change).
- **Done:** gates green; the two serde tests pass; all pre-existing tests
  unchanged and green.

### 0.2 Add `backend_tls` / `backend_tls_sni` to `VhostEntry`
- **Model:** Sonnet
- **Files:** `src/vhost.rs:374` (the `struct VhostEntry` definition); construction
  sites at `src/vhost.rs:636` (native, in `serve_vhost_provider`) and
  `src/sshgw.rs:903` (SSH, in `tcpip_forward_vhost`).
- **Change:**
  - Add to `struct VhostEntry`, near `https_policy` (`:396`):
    - `pub backend_tls: bool,`
    - `pub backend_tls_sni: Option<String>,`
    Match the existing field visibility/style in the struct.
  - At BOTH construction sites, initialize the new fields to `false` / `None` for
    now (no plumbing yet — Phases 2 and 3 wire real values). This keeps the phase
    behavior-neutral.
  - If a `Debug`/display or admin-serialization of `VhostEntry` enumerates fields
    explicitly, include the new fields there too (grep for `VhostEntry {` matches
    and for any admin/JSON mapping of the entry).
- **Unit tests:**
  - `vhost_entry_backend_tls_defaults_off` — construct a `VhostEntry` via the
    same path a test already uses (or a minimal builder) and assert
    `backend_tls == false`, `backend_tls_sni.is_none()`.
- **e2e tests:** none.
- **Done:** gates green; new test passes; no existing vhost test changed.

### 0.3 Add a reusable insecure backend `TlsConnector` helper
- **Model:** Sonnet
- **Files:** `src/transport.rs` (add near `connect` at `:145` and `client_config`
  at `:161`; reuse `NoVerifier` at `:212` and the `ServerName` import at `:26`).
- **Change:**
  - Add `pub(crate) fn insecure_tls_connector() -> anyhow::Result<tokio_rustls::TlsConnector>`
    that builds `TlsConnector::from(Arc::new(client_config(true)?))`. `client_config(true)`
    already installs `NoVerifier` (accept-any-cert) — reuse it; do NOT duplicate
    the verifier. If `client_config` is currently private (`fn`), keep it private
    and call it from this new helper in the same module; only the new helper needs
    `pub(crate)`.
  - Add `pub(crate) fn backend_server_name(name: &str) -> anyhow::Result<tokio_rustls::rustls::pki_types::ServerName<'static>>`
    wrapping `ServerName::try_from(name.to_owned())` (owned → `'static`), mapping a
    parse error to a clear `anyhow` error (e.g. `"invalid backend TLS SNI: {name}"`).
    Mirror the `ServerName::try_from` usage at `:152`.
- **Unit tests:** in `src/transport.rs` `mod tests`:
  - `insecure_tls_connector_builds` — assert `insecure_tls_connector().is_ok()`.
  - `backend_server_name_valid` — `backend_server_name("localhost").is_ok()`.
  - `backend_server_name_rejects_garbage` — `backend_server_name("").is_err()` (or
    another value rustls rejects); assert it returns `Err`, does not panic.
- **e2e tests:** none.
- **Done:** gates green; three helper tests pass.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway shared:: vhost:: transport::`
  (plus the specific new test names above).
- **Regression guard:** the FULL existing suites `cargo test --features ssh-gateway`
  must remain green — this phase adds only fields/a helper, so there must be zero
  changed assertions in any pre-existing test.

## Phase done criterion

`HelloVhost` and `VhostEntry` carry `backend_tls` + `backend_tls_sni` (defaulting
off), `transport::insecure_tls_connector` / `backend_server_name` exist and are
unit-tested, all gates are green, and no existing test was modified. The three
new unit-test groups (serde round-trip incl. old-wire default, entry default,
connector/servername) pass.

> **STOP.** All gates green? Report status and the new test results, then ASK the
> user for explicit confirmation before starting Phase 1. Do not proceed
> automatically. Update `resume.md` (Phase 0 → DONE, `Next:` → phase_02 § 1.1).
