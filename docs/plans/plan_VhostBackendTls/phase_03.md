# Phase 2 — Native client plumbing (`bore vhost --backend-tls`)

> **Intent:** Expose the feature on the native client: a `--backend-tls` flag (and
> optional `--backend-tls-sni <NAME>`) on `bore vhost`, threaded through the
> provider constructor into `HelloVhost`, and mapped by the server into
> `VhostEntry.backend_tls`. After this phase the native reference scenario works.
> **Shippable alone?** yes — a new opt-in flag; default off leaves `bore vhost`
> unchanged.
> **Preconditions:** Phase 1 DONE.

---

## Sub-phases

### 2.1 Add CLI flags to the `bore vhost` subcommand
- **Model:** Haiku
- **Files:** `src/main.rs:369` (the `Vhost { ... }` subcommand variant; fields
  `target:371`, `subdomain:375`, `id:380`, `to:387`).
- **Change:** add two `clap` fields to the `Vhost` variant, mirroring the style of
  the existing bool/optional flags in that variant:
  - `/// Connect to the local backend over TLS (accepts a self-signed cert).`
    `#[clap(long)] backend_tls: bool,`
  - `/// SNI/hostname sent to the TLS backend (default: localhost).`
    `#[clap(long, value_name = "NAME")] backend_tls_sni: Option<String>,`
- **Unit tests:** if `main.rs` has clap-parse unit tests, add
  `vhost_backend_tls_flags_parse` asserting `--backend-tls --backend-tls-sni app`
  parse into `backend_tls == true` / `Some("app")`. If no such test module exists,
  rely on the compile + the e2e in 2.4; do not invent a new test harness.
- **e2e tests:** none yet (behavior verified in 2.4).
- **Done:** gates green; flags parse; `bore vhost --help` shows them.

### 2.2 Thread the flags through the native provider constructor into `HelloVhost`
- **Model:** Sonnet
- **Files:** `src/client.rs:556` (`new_vhost_provider_with_udp`), `:595`
  (`HelloVhost { .. }` send); the call site in `src/main.rs` that invokes the
  vhost provider (locate where `Command::Vhost { .. }` is handled and the provider
  constructor is called).
- **Change:**
  - Extend the provider-construction path to accept `backend_tls: bool` and
    `backend_tls_sni: Option<String>` (add parameters, or extend the options
    struct the constructor already takes — follow whatever shape `new_vhost_provider_with_udp`
    currently uses; do not restructure it).
  - Pass the CLI values from the `Command::Vhost` handler into that path.
  - Populate the two new `HelloVhost` fields at `:595` from these values instead of
    the `false`/`None` placeholders left in Phase 0.1.
- **Unit tests:** none new required (covered by 2.4 e2e); if a client-side unit
  test constructs `HelloVhost`, extend it to assert the fields propagate.
- **e2e tests:** none yet.
- **Done:** gates green; the placeholder `false`/`None` from Phase 0.1 are replaced
  by real CLI values.

### 2.3 Map `HelloVhost` → `VhostEntry` on the server
- **Model:** Sonnet
- **Files:** `src/vhost.rs:581` (`serve_vhost_provider`), `:636` (the
  `VhostEntry { .. }` construction) — where the received `HelloVhost` is decoded.
- **Change:** set `backend_tls: <hello>.backend_tls` and
  `backend_tls_sni: <hello>.backend_tls_sni` in the `VhostEntry` built at `:636`,
  replacing the `false`/`None` placeholders left in Phase 0.2. Follow the existing
  pattern used to map other `HelloVhost` fields (e.g. `https_policy`,
  `webserver_log`) into the entry.
- **Unit tests:** `serve_vhost_provider_maps_backend_tls` (or extend an existing
  serve-provider test) — send a `HelloVhost` with `backend_tls: true,
  backend_tls_sni: Some("app".into())`, assert the resulting registry
  `VhostEntry` carries both.
- **e2e tests:** none yet.
- **Done:** gates green; mapping test passes.

### 2.4 Native end-to-end + docs
- **Model:** Sonnet (test) + Haiku (docs)
- **Files:** `tests/vhost_test.rs` (reuse `self_signed_for:172`,
  `write_pem_files:178`, `http_config:156`, `to_reg:211`); `README.md` vhost
  section (`:325`–`:441`); any vhost doc under `docs/vhost/`.
- **Change:**
  - Test T-VBT2: full native path — start an in-process `bore server` with vhost,
    a real self-signed HTTPS backend (from 1.1's helper), and a native vhost
    provider configured with `backend_tls = true` (drive the actual client
    constructor / `HelloVhost`, not a hand-built entry). Issue an HTTP request to
    the subdomain and assert a 200 with the backend body. Add a companion assertion
    that a plaintext-backend provider WITHOUT the flag still returns 200
    (regression, I-1).
  - Docs: in the README vhost section document `--backend-tls` and
    `--backend-tls-sni`, with the two reference examples (plain
    `http://localhost:3000` vs self-signed `https://localhost:3005`), and a
    SECURITY note: backend certificate verification is skipped (accept-any) — safe
    for trusted localhost backends; CA pinning is not yet supported. Update
    `docs/vhost/` if a dedicated flags/reference doc exists there.
- **Unit tests:** covered by T-VBT2.
- **e2e tests:** T-VBT2 (in-process real-TLS backend, native transport).
- **Done:** gates green; T-VBT2 passes; README + vhost docs updated and accurate.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test vhost_test backend_tls`
  + the mapping test in `vhost::`.
- **Regression guard:** full `cargo test --features ssh-gateway` green; existing
  `bore vhost` tests unchanged.

## Phase done criterion

`bore vhost --backend-tls [--backend-tls-sni NAME] <label> localhost:<port>`
against a self-signed HTTPS backend serves 200 through the subdomain (T-VBT2);
without the flag the vhost path is unchanged; README and vhost docs document the
flags and the security caveat.

> **STOP.** All gates green? Report status + T-VBT2 result + the doc diff summary,
> then ASK the user for explicit confirmation before Phase 3. Update `resume.md`
> (Phase 2 → DONE, `Next:` → phase_04 § 3.1).
