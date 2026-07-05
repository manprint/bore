# Phase 5 — Startup param-applicability logging (G6)

> **Intent:** On the native `bore` client, emit a clear log line for every
> parameter that is not applicable to the chosen subcommand/mode, at parity with
> the SSH gateway's I-SSH8 behavior. No silent ignores.
> **Shippable alone?** yes — logging only, no data-path change.
> **Preconditions:** phase_04 DONE (so the flag set is final).

---

## Sub-phases

### 5.1 Native client inapplicable-param warnings
- **Model:** Haiku
- **Files:** `src/main.rs` — the dispatch points for `bore local`, `bore vhost`,
  `bore proxy`, `bore transfer` where parsed args are consumed (grep the subcommand
  match arms; local builds `TunnelOptions` ~1541, vhost sends `HelloVhost` via
  `client.rs:570`).
- **Change:** After parsing each subcommand, before connecting, check the flags that
  do NOT apply to that subcommand and `warn!` once each, in professional English,
  no emojis. Concretely at minimum:
  - `bore vhost`: `--force-https` is not accepted by the parser (do nothing) —
    but if any transport-only flags that the vhost path ignores are present that are
    currently silently dropped, warn. (Scope this to flags that EXIST on the
    subcommand's clap group but are no-ops for it; do not invent cross-subcommand
    flags.)
  - General rule: for any flag whose value is set but whose code path ignores it in
    the selected mode, print `warn!("--<flag>: not applicable to `bore <sub>`; ignoring")`.
  - Keep it minimal and correct — only warn for genuinely-ignored set flags. Do NOT
    warn for flags left at default.
- **Unit tests:** none required (logging). Optionally, if a pure
  `inapplicable_flags(sub, parsed) -> Vec<&str>` helper is factored, unit-test it
  with one applicable and one inapplicable flag.
- **e2e tests:** none.
- **Done:** running a subcommand with an inapplicable-but-set flag prints exactly one
  warning per such flag; running with only applicable flags prints none. Gates green.

### 5.2 Param-applicability matrix doc
- **Model:** Haiku
- **Files:** `docs/VHOST.md` (append a section) and/or a new small table referenced
  from `docs/SSH_GATEWAY.md`. Follow the existing docs structure — do NOT create a
  new directory; these files already exist.
- **Change:** Add one table: rows = every user-facing param (https, force-https,
  basic-auth, webserver-log, max-conns, carriers, udp, auto-reconnect, notes,
  stun/upnp/port-prediction/nat-udp flags); columns = {native local, native vhost,
  native proxy/secret, SSH public, SSH vhost, SSH secret}. Cells = `yes` / `n/a
  (warns)` / `transport-only (SSH warns)`. Base it on the phase_01-05 outcomes and
  the recon anchors (SSH transport-only list at `sshgw.rs:2491-2498`; SSH
  inapplicable at `803-813`,`1002-1014`).
- **Unit tests:** none.
- **e2e tests:** none.
- **Done:** the matrix table renders in `docs/VHOST.md`; every cell matches actual
  code behavior after phases 1-5.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test subset:** `cargo test --all-features` (no new tests, ensure no breakage)
- **Regression guard:** no behavior change to any data path; existing tests pass.

## Phase done criterion

The native client warns for each set-but-inapplicable flag (parity with SSH
I-SSH8), and `docs/VHOST.md` carries an accurate param-applicability matrix.
