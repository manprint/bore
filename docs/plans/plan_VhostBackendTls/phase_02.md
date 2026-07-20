# Phase 1 — Server-side backend-TLS wrap at the relay seam (core)

> **Intent:** When `entry.backend_tls` is set, wrap the provider `LinkStream` in a
> `tokio-rustls` client `TlsStream` immediately after it is opened and BEFORE the
> relay writes the request head, so the HTTP flowing to the backend rides a TLS
> session originated by the server. This is the whole feature; Phases 2 and 3 only
> flip the flag on.
> **Shippable alone?** yes — the wrap is gated on `entry.backend_tls`, which is
> still `false` in production (no CLI/param plumbing yet); tests drive it directly
> by constructing an entry with the flag set.
> **Preconditions:** Phase 0 DONE.

> **BEHAVIOR-CHANGE NOTE:** this phase introduces a new runtime code path, but it
> is reachable ONLY when `backend_tls == true`. With the flag off the code path is
> byte-identical to today (I-1/D6). The reviewer must confirm the `false` branch
> is unchanged.

This phase has an **Opus design-review gate** (hot-path relay, yamux single-task
invariant, no-hang, no-panic) before Sonnet implements.

---

## Sub-phases

### 1.1 Insert the TLS wrap after the provider binding
- **Model:** Opus design review → Sonnet implements
- **Files:** `src/vhost.rs` — the `let mut provider: mux::LinkStream = { ... };`
  block at `:879` (fed by `open_ready` at `:900` / `:908` / `:918`), and the first
  use of `provider` at `:932` (`provider.write_all(&request_head).await?`).
- **Change:**
  - Immediately AFTER the `provider` binding block ends (after `:928`, the closing
    `};`) and BEFORE `:932`, insert a guarded wrap:
    ```
    if entry.backend_tls {
        let connector = crate::transport::insecure_tls_connector()?;
        let sni = entry
            .backend_tls_sni
            .as_deref()
            .unwrap_or("localhost");
        let server_name = crate::transport::backend_server_name(sni)?;
        let tls = tokio::time::timeout(
            BACKEND_TLS_HANDSHAKE_TIMEOUT,
            connector.connect(server_name, provider),
        )
        .await
        .map_err(|_| /* elapsed */ std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "backend TLS handshake timed out",
        ))?
        .map_err(|e| std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("backend TLS handshake failed: {e}"),
        ))?;
        provider = Box::new(tls);
    }
    ```
    - `provider` must remain typed `mux::LinkStream` (= `Box<dyn Duplex>`).
      `TlsStream<Box<dyn Duplex>>` implements `AsyncRead + AsyncWrite + Unpin + Send`
      hence `mux::Duplex`, so `Box::new(tls) as mux::LinkStream` type-checks
      (Risk: boxed-stream bounds — verify it compiles; if the blanket
      `impl Duplex` needs an explicit cast, write `Box::new(tls) as mux::LinkStream`).
    - Because ALL THREE `open_ready` branches (900/908/918) resolve into the single
      `provider` binding at `:879`, this one wrap covers every branch. Do NOT
      duplicate the wrap per branch.
    - Error handling: on timeout OR handshake error, propagate the `?` (the
      surrounding relay function already returns `Result` and closes the proxied
      connection on `Err`). Confirm the enclosing fn returns a compatible error
      type; if it returns `anyhow::Result`, `std::io::Error` converts via `?`. Do
      NOT `unwrap`/`expect` (I-5).
  - Add the constant near the other vhost timeouts/consts at the top of `vhost.rs`:
    `const BACKEND_TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);`
  - Leave the rest of the relay (`provider.write_all(&request_head)` at `:932`, the
    `copy_bidirectional_with_sizes` calls at `:997`/`:1000`, and
    `relay_response_injected` at `:1039`) UNCHANGED — they now operate on the
    TLS-wrapped `provider` transparently.
- **Opus review checklist (must pass before Sonnet commits):**
  1. The `entry.backend_tls == false` path is textually unchanged (diff shows only
     an added `if` block, nothing removed/reordered).
  2. `provider` is spliced in ONE task; the wrap does not `tokio::io::split` it
     (I-3).
  3. Handshake is timeout-bounded (I-4); failure closes the conn, never hangs.
  4. No `unwrap`/`panic` on SNI or handshake (I-5).
  5. Header injection still works: the server holds plaintext (it is the TLS
     client endpoint), so `rewrite_head` on request and `relay_response_injected`
     on response operate on decrypted bytes.
- **Unit tests:** in `tests/vhost_test.rs`, using `self_signed_for` (`:172`) +
  `write_pem_files` (`:178`) to stand up a REAL local TLS backend (a minimal
  `tokio-rustls` server accepting the self-signed cert and replying a fixed HTTP
  response), and the existing in-process vhost harness (`reg_cfg_no_reservations`
  `:207`, `to_reg` `:211`, `http_config` `:156`) with a `VhostEntry` constructed
  with `backend_tls: true, backend_tls_sni: Some("localhost".into())`:
  - `backend_tls_wrap_handshakes_with_self_signed` — a request routed through
    vhost to the self-signed TLS backend returns the backend's 200 body.
  - `backend_tls_bad_sni_fails_gracefully` — set `backend_tls_sni: Some("".into())`
    (or a value rustls rejects); assert the proxied connection is closed with an
    error and the test does NOT hang (wrap the assertion in a `tokio::time::timeout`
    shorter than `BACKEND_TLS_HANDSHAKE_TIMEOUT`).
  - `backend_tls_against_plaintext_backend_times_out_or_errors` — point
    `backend_tls: true` at a PLAINTEXT HTTP backend; assert the connection fails
    within the handshake timeout, does not hang.
  - `backend_tls_off_path_unchanged` — an entry with `backend_tls: false` against a
    plaintext backend still serves 200 (guards I-1 in-process).
- **e2e tests:** T-VBT1 (deferred full-transport e2e to Phase 4) — here the
  in-process real-TLS-backend test above is the truth gate for the wrap itself.
- **Done:** gates green; the four wrap tests pass; the `false`-path test proves no
  regression; Opus review checklist signed off.

### 1.2 Red-check the gate
- **Model:** Sonnet
- **Files:** none (verification step).
- **Change:** none. Temporarily revert the `if entry.backend_tls { ... }` wrap
  (comment it out) and confirm `backend_tls_wrap_handshakes_with_self_signed`
  FAILS (backend resets / plaintext-to-TLS mismatch), then restore the wrap and
  confirm it PASSES. This follows the memory `feedback-inprocess-test-false-pass`:
  a test that stays green with the fix reverted proves nothing. Record the
  red→green observation in `resume.md` notes.
- **Unit tests:** reuse `backend_tls_wrap_handshakes_with_self_signed`.
- **e2e tests:** none.
- **Done:** documented red-check: test red without the wrap, green with it.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test vhost_test backend_tls`
- **Regression guard:** full `cargo test --features ssh-gateway` green; every
  pre-existing vhost/ssh test unchanged and passing (I-1).

## Phase done criterion

With a `VhostEntry.backend_tls == true` pointing at a real self-signed HTTPS
backend, an HTTP request routed through the vhost relay returns the backend's
response; the same relay with `backend_tls == false` is byte-identical to today
and all existing tests pass; failure modes (bad SNI, non-TLS backend) close the
connection within the timeout without hanging or panicking; the wrap has been
red-checked (test fails when the wrap is reverted).

> **STOP.** All gates green + red-check recorded? Report status and results, then
> ASK the user for explicit confirmation before Phase 2. Update `resume.md`
> (Phase 1 → DONE, `Next:` → phase_03 § 2.1).
