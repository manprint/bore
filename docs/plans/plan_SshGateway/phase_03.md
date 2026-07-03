# Phase 3 — Auth stores (hot-reload) + `bore hash-password`

> **Intent:** implement the two SSH credential stores — authorized-keys directory and argon2id password file — with per-attempt hot reload, plus the operator utility to generate password hashes. Additive: nothing is wired to the network yet.
> **Shippable alone?** yes — new code only, feature-gated, unused at runtime.
> **Preconditions:** Phase 1 DONE (feature + `src/sshgw_auth.rs` stub + russh dep for key types).

Context (self-contained): the SSH gateway (Phase 4) authenticates clients against
(a) a directory of authorized_keys-format files and/or (b) a password file with argon2id
hashes, `label:hash` per line, multiple valid lines (D3). Both stores re-read the
filesystem at EVERY auth attempt (hot reload by construction) with an mtime cache so bot
storms do not re-parse unchanged files. Identity string (D10): pubkey comment if present,
else `SHA256:<base64 fingerprint>`; for passwords, the matching line's label. All code in
`src/sshgw_auth.rs` (`#[cfg(feature = "ssh-gateway")]` via lib.rs gate from Phase 1).

---

## Sub-phases

### 3.1 `KeyStore` — authorized-keys directory
- **Model:** Sonnet
- **Files:** `src/sshgw_auth.rs`
- **Change:** implement:
  - `pub struct KeyGrant { pub identity: String, pub permit: Option<Vec<String>>, pub max_conns: Option<usize>, pub notes: Option<String> }`
  - `pub struct KeyStore { dir: PathBuf, cache: Mutex<HashMap<PathBuf, (SystemTime, Vec<ParsedKey>)>> }`
  - `pub fn new(dir: PathBuf) -> Self`; `pub fn check(&self, offered: &<russh public key type from SPIKE_FINDINGS.md>) -> Option<KeyGrant>`.
  `check` lists `*.pub` and extensionless files in `dir` (non-recursive), re-parses any file whose mtime changed (cache hit otherwise), and compares keys by key-blob equality. Line format = standard authorized_keys: optional leading options field (comma-separated, quoted values allowed), then `<type> <base64> [comment]`. Supported options (subset, D1/D10): `permit="csv"` (values like `vhost/<glob>`, `secret/<glob>`, `port/<n>` or `port/<a>-<b>`), `max-conns=<n>`, `notes="..."`. Unknown options: parse, keep the line valid, and record the option name so Phase 4 can log a warning once (do not fail the line — forward compat). Malformed line or unreadable file: skip it, `tracing::warn!` once per file mtime, never abort the scan.
  Use russh's OpenSSH-format key parsing (exact API per `docs/plans/plan_SshGateway/SPIKE_FINDINGS.md`); write the options tokenizer by hand (quoted-string state machine ~40 lines; no new dependency).
- **Unit tests:** (in `src/sshgw_auth.rs` `#[cfg(test)]` — project keeps unit tests in-file)
  `keystore_matches_known_key` — write a tempdir file with a generated key line, assert `check` returns grant with identity = comment;
  `keystore_identity_falls_back_to_fingerprint` — key line without comment ⇒ identity starts with `SHA256:`;
  `keystore_options_parsed` — line with `permit="vhost/my-*,port/9000-9010",max-conns=3,notes="ci"` ⇒ grant fields exact;
  `keystore_hot_reload_add_and_remove` — check misses; write file; check hits; remove file; check misses (no restart);
  `keystore_mtime_cache` — parse counter (test hook or file re-write detection) does not increase when mtime unchanged;
  `keystore_malformed_line_skipped` — garbage line + valid line in one file ⇒ valid key still matches;
  `keystore_unknown_option_kept` — line with `no-touch-required` (unsupported) still matches, option surfaced.
- **e2e tests:** none (wired in Phase 4; then covered by T-SSH-PUB1).
- **Done:** gates green; all listed unit tests pass; no network code in this file.

### 3.2 `PasswordStore` — argon2id file, multiple credentials, DoS cap
- **Model:** Sonnet
- **Files:** `src/sshgw_auth.rs`
- **Change:** implement:
  - `pub struct PasswordStore { path: PathBuf, cache: Mutex<(SystemTime, Vec<(String, String)>)>, verify_permits: Arc<Semaphore> }` — semaphore initialized with 2 permits (constant `PASSWORD_VERIFY_CONCURRENCY: usize = 2`, doc comment: argon2id is deliberately slow; the cap bounds CPU under credential-stuffing).
  - `pub async fn check(&self, password: &str) -> Option<String>` — reload file if mtime changed; lines are `label:$argon2id$...`; `#`-prefix and blank lines ignored; a line whose hash does not start with `$argon2id$` is skipped with a one-time `warn!` (D3: no plaintext, no other algorithms); acquire a permit, verify against EACH line (argon2 crate `Argon2::default().verify_password`), first match returns its label. Verify inside `tokio::task::spawn_blocking` (argon2 is CPU-bound; do not block the runtime).
- **Unit tests:**
  `password_any_of_multiple_lines_matches` — two labeled hashes, both passwords accepted, labels correct;
  `password_wrong_rejected`;
  `password_hot_reload` — add a line to the file, next check accepts it;
  `password_non_argon2_line_skipped` — `label:plain:x` and `label:$2b$...` lines never match, valid line still works;
  `password_verify_concurrency_capped` — spawn 8 concurrent `check` calls with an instrumented counter (wrap the semaphore acquire in a test-visible gauge or assert `available_permits` never observed < 0 and peak concurrent verifies <= 2 via an AtomicUsize toggled around the blocking section).
- **e2e tests:** none here (Phase 4 wires; netns T-SSH-N-series exercises password login).
- **Done:** gates green; unit tests pass; `cargo tree` shows argon2 only under the feature.

### 3.3 `bore hash-password` subcommand
- **Model:** Haiku
- **Files:** `src/main.rs:66-67` (Command enum; add a variant), plus a small `pub fn hash_password(password: &str) -> Result<String>` in `src/sshgw_auth.rs`
- **Change:** add `#[cfg(feature = "ssh-gateway")]` Command variant `HashPassword` (clap doc: "Generate an argon2id hash line for --ssh-passwords-file; reads the password from stdin"). Behavior: read one line from stdin (trim trailing newline; empty ⇒ error "empty password"), hash with argon2id default params and a fresh random salt, print the bare hash string to stdout plus a hint line to stderr: `add to the passwords file as: <label>:<hash>`. Follow the existing subcommand match structure in main.rs (see how `Server`/`Local` arms dispatch). No password on argv (leaks via ps) — stdin only.
- **Unit tests:** `hash_password_roundtrip` (in `src/sshgw_auth.rs`) — hash then `PasswordStore::check` on a temp file line accepts it.
- **e2e tests:** none.
- **Done:** `printf 'pw' | cargo run --features ssh-gateway -- hash-password` prints a `$argon2id$` string; default build does not expose the subcommand (`cargo run -- hash-password` fails with unknown subcommand).

---

## Phase gates

- **Fmt:** `cargo fmt`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway sshgw_auth` (module filter) + full `cargo test` (default features, must be untouched)
- **Regression guard:** none beyond full default suite (no existing file touched except main.rs additive arm).

## Phase done criterion

Both stores pass their unit-test matrices including hot-reload and the concurrency cap; `bore hash-password` produces hashes the store accepts; default build unchanged.
