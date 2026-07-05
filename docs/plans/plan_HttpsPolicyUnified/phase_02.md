# Phase 1 — Public path: apply policy + resilience (G1)

> **Intent:** Make the server apply a public tunnel's `https_policy` through the
> capability-bounded resolver, and replace the hard-reject of `https`-without-cert
> with warn + fallback (G1). Legacy clients (`https_policy = None`) keep exact
> current behavior.
> **Shippable alone?** yes — new behavior only triggers when a client sends a
> policy; the CLI to produce it lands in phase_04, but in-process tests exercise
> it directly.
> **Preconditions:** phase_01 DONE.

---

## Sub-phases

### 1.1 Resolve effective public flags from policy — OPUS REVIEW GATE (hot path)
- **Model:** Sonnet; **Opus reviews** the branch that preserves the legacy path.
- **Files:** `src/server.rs:1714-1808` (the `ClientMessage::Hello(port, opts)` handler: the reject block at `1723`, the `edge::accept` call site at `1946` with `opts` cloned at `1939`, and the `admin.register(NewEntry{..})` at `1774-1808`).
- **Change:**
  1. Right after `opts` is available and BEFORE the reject block at `1723`, compute the effective flags:
     ```rust
     let capable = self.tls.is_some();
     let (eff_https, eff_force_https, downgraded) = match opts.https_policy {
         Some(policy) => shared::resolve_https_policy(policy, capable),
         None => (opts.https, opts.force_https, false), // legacy path, byte-identical
     };
     ```
  2. Replace the reading of `opts.https` / `opts.force_https` downstream (the `NewEntry` fields `https`/`force_https` at `1781-1782`, and the `TunnelOptions` handed to `edge::accept`) with `eff_https` / `eff_force_https`. Concretely: before the `edge::accept` spawn, set `opts.https = eff_https; opts.force_https = eff_force_https;` so the existing `edge::accept(stream2, opts, ...)` consumes the resolved values (edge.rs stays unchanged). Confirm `opts` is owned/mutable at that scope; if it is cloned per-connection at `1939`, mutate the base `opts` once, before the accept loop, so every clone carries the resolved flags.
  3. `NewEntry.https = eff_https`, `NewEntry.force_https = eff_force_https` (admin shows the EFFECTIVE, post-fallback values — consistent with G5's intent for public).
- **Unit tests:** none new here (logic covered by 0.2 + the e2e in 1.3). Keep the change minimal.
- **e2e tests:** covered in 1.3.
- **Done:** with `https_policy = None`, the code path is byte-identical to today (Opus confirms by diff: legacy branch reads the same bools, no reordering). Gates green.

### 1.2 Replace hard-reject with warn + fallback (G1) — OPUS REVIEW GATE
- **Model:** Sonnet; **Opus reviews** the wire-safety of the Warning send.
- **Files:** `src/server.rs:1723` (reject block); the control `Delimited`/sender used to send `ServerMessage` in this handler.
- **Change:** Rewrite the `1723` block:
  ```rust
  // OLD: if opts.https && self.tls.is_none() { send Error; return Ok(()); }
  // NEW:
  if downgraded {
      let msg = "server has no TLS certificate (--cert-file/--key-file); \
                 falling back to plain HTTP for this tunnel";
      warn!(%port, "{msg}");
      // Only policy-aware clients understand ServerMessage::Warning (I-6/D5).
      if opts.https_policy.is_some() {
          let _ = control.send(ServerMessage::Warning(msg.into())).await;
      }
      // eff_https/eff_force_https are already false (resolver), so we continue.
  } else if opts.https_policy.is_none() && opts.https && self.tls.is_none() {
      // LEGACY path: an OLD client asked for https without a cert. Preserve the
      // exact fatal behavior (old client cannot decode Warning). Byte-identical.
      control.send(ServerMessage::Error(
          "server has no TLS certificate configured".into())).await?;
      return Ok(());
  }
  ```
  This guarantees: new client (policy `Some`) → warn+fallback+continue; old client
  (policy `None`, https bool set, no cert) → today's fatal `Error`. Verify the
  exact sender symbol name (`control`, `conn`, `stream`?) at the call site and use it.

  > **CRITICAL ORDERING (wire, from Opus review of phase_01 §0.3):** the client's
  > one-shot registration reads (`client.rs:210` public `Hello(port)`, `229`
  > carrier token) BAIL on an unexpected `ServerMessage::Warning` by design; only
  > the main control loop (`client.rs:981`) handles it non-fatally (warn+continue).
  > Therefore the server MUST send `ServerMessage::Warning` **AFTER** the tunnel
  > readiness handshake — i.e. after `ServerMessage::Hello(port)` and after any
  > `CarrierToken` — so the client consumes it on its main loop, never at a
  > one-shot read. Do NOT send the Warning before `Hello(port)`. Concretely:
  > compute `downgraded` early (1.1) but DEFER the `control.send(Warning)` to
  > immediately after the readiness/carrier messages are sent, just before the
  > relay/accept loop. `eff_https`/`eff_force_https` are already `false`, so
  > deferring the message does not change any data-path behavior.
- **Unit tests:** none (integration-level; see 1.3).
- **e2e tests:** T-HP-PUB1 (see 1.3).
- **Done:** Opus confirms the legacy branch is only reachable for `policy=None` and is byte-identical to the removed block. Gates green.

### 1.3 Public policy + raw regression tests
- **Model:** Sonnet
- **Files:** `tests/tls_test.rs` (reuse `self_signed` at `64-66`, server+Client setup at `76-94`, and the `force_https_redirects_plain_http` shape at `332-393`).
- **Change:** Add tests that set `TunnelOptions { https_policy: Some(..), ..Default::default() }` and drive a real in-process server+client:
  - **T-HP-PUB1** `policy_on_no_cert_falls_back_to_http`: server started WITHOUT `set_tls`; client sends `https_policy: Some(On)`. Assert: tunnel establishes (no error/bail), a plain HTTP request is served (forwarded to the local echo), and the admin entry / effective flags show `https=false`. If the client-side Warning capture is feasible in-process, assert the client observed a `Warning`; otherwise assert non-fatal continuation only.
  - **T-HP-PUB2** `policy_redirect_with_cert_308`: server WITH `set_tls(self_signed)`; client `https_policy: Some(Redirect)`. Assert a plain `GET / HTTP/1.1` gets `308` with `Location: https://...` (mirror `force_https_redirects_plain_http`).
  - **T-HP-PUB3** `policy_raw_passthrough_matrix`: for each of `Off`, `On`, `Redirect` (cert present), open a RAW TCP connection that sends non-HTTP, non-TLS bytes (`b"\x00\x01POSTGRES\x00"` — must NOT start with a TLS `0x16` or an HTTP method) and assert the bytes arrive at the local service unchanged and the echo returns them (proves I-4 under every policy).
  - **T-HP-PUB4** `legacy_bools_still_reject_no_cert`: client with `https_policy: None`, `https: true`, no server cert → assert the client receives the fatal `Error` (byte-identical legacy behavior, I-2).
- **Unit tests:** the four T-HP-PUB* are integration tests in `tests/tls_test.rs`.
- **e2e tests:** T-HP-PUB1..PUB4 as above.
- **Done:** all four pass; the pre-existing `force_https_redirects_plain_http` still passes unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --test tls_test` and `cargo test --all-features`
- **Regression guard:** `force_https_redirects_plain_http` and all `tests/tls_test.rs` pass; T-HP-PUB4 proves the legacy no-cert path is unchanged.

## Phase done criterion

A public client sending `https_policy = Some(On|Redirect)` to a server without the
matching cert gets a live HTTP tunnel plus a non-fatal warning (T-HP-PUB1); with a
cert, `Redirect` produces a 308 (T-HP-PUB2); raw traffic survives every policy
(T-HP-PUB3); and a legacy `None`+`https:true`+no-cert client still gets the old
fatal error (T-HP-PUB4).
