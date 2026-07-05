# Phase 6 — PTY documentation (G7)

> **Intent:** Document that `PTY allocation request failed on channel 0` is a
> harmless client-side (OpenSSH) message and how to silence it. NO code change to
> `pty_request` (staying `channel_failure` is correct — the gateway is not a shell).
> **Shippable alone?** yes — docs only.
> **Preconditions:** none (independent of the HTTPS work).

---

## Sub-phases

### 6.1 Document the PTY message
- **Model:** Haiku
- **Files:** `docs/SSH_GATEWAY.md` (§6 operational guide) and `README-SSH-GATEWAY.md`
  (§4 examples box). Follow the existing structure; do NOT create new files.
- **Change:** Add a short subsection, e.g. "Harmless client messages":
  - Explain: `PTY allocation request failed on channel 0` is printed by the OpenSSH
    CLIENT when it auto-requests an interactive PTY (any `ssh`/`autossh` invocation
    without `-T`). The gateway is not a shell, so it declines the PTY
    (`pty_request` → `channel_failure`, `src/sshgw.rs:1460-1473`). The `-R`/`-L`
    forward runs on a separate channel and is UNAFFECTED — the tunnel works.
  - Recommend `-T` to silence it: `autossh -M0 -T -p 443 -R vhost/app:0:localhost:5000 bore.host`.
  - Reiterate I-SSH7: `-N` is discouraged (it skips the session channel entirely, so
    the client never sees the tunnel-info banner or any warning).
  - Also note `Allocated port 1 ...` printed by OpenSSH for a vhost `-R ...:0:...` is
    the RFC4254 forward-reply port placeholder (no real TCP port is bound for a
    vhost/secret forward); it is cosmetic.
  - Update every doc example that used `-N` to use `-T` instead (consistency with
    the I-SSH7 note already in `SSH_GATEWAY.md §6.4a`).
- **Unit tests:** none.
- **e2e tests:** none.
- **Done:** both docs explain the PTY and `Allocated port 1` messages and recommend
  `-T`; no `-N` remains in the primary examples.

### 6.2 Optional one-line banner hint
- **Model:** Haiku
- **Files:** `src/sshgw.rs` — the session-channel banner/notice path (`ConnState::deliver`
  / where the shell_request info line is sent, `sshgw.rs:1485-1524` + banner builders).
- **Change (optional, only if trivially safe):** When a session channel is opened and
  the client did not pass `-T` (i.e. a PTY request was seen), append ONE informational
  line to the banner: `"Note: the 'PTY allocation request failed' message above is
  harmless; pass -T to ssh/autossh to silence it."` Must be non-destructive
  (`ConnState::deliver`, never closes the channel — respects I-SSH6). If detecting
  "PTY was requested" is not cleanly available, SKIP this sub-phase (docs in 6.1 are
  sufficient). Do NOT modify `pty_request` itself (I-8).
- **Unit tests:** none (banner string; covered by existing banner tests if added).
- **e2e tests:** none.
- **Done:** either a harmless hint line is delivered when a PTY was requested, or the
  sub-phase is explicitly SKIPPED in `resume.md` with a one-line reason. `pty_request`
  is unchanged either way.

---

## Phase gates

- **Fmt:** `cargo fmt --check` (only if 6.2 touched code)
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings` (if 6.2 touched code)
- **Test subset:** `cargo test --all-features --test ssh_gateway_test -- --test-threads=1` (must stay green)
- **Regression guard:** `pty_request` unchanged; I-SSH6/I-SSH7 tests pass; `shell_request` still non-closing.

## Phase done criterion

`docs/SSH_GATEWAY.md` and `README-SSH-GATEWAY.md` explain the PTY / `Allocated port 1`
messages and recommend `-T`; `pty_request` is untouched; 6.2 is either delivered
harmlessly or explicitly skipped.
