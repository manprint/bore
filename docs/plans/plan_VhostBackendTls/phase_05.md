# Phase 4 — Full regression, real-backend e2e/netns, final doc read

> **Intent:** Prove the feature end-to-end on both transports against a real
> self-signed backend outside the in-process harness (guarding the loopback
> false-pass risk), run the full regression suite, and do a final consolidated
> documentation read.
> **Shippable alone?** yes — test/doc hardening only, no production code change
> (unless a real-network run surfaces a defect, which is then fixed here).
> **Preconditions:** Phase 3 DONE.

---

## Sub-phases

### 4.1 Real-subprocess / netns e2e for both transports
- **Model:** Sonnet
- **Files:** the existing vhost/SSH netns harness scripts under `scripts/`
  (locate the vhost and `scripts/ssh_gateway_test.sh` harnesses; do NOT create a
  new harness if one covers vhost/ssh — extend it). If netns does not cover the
  native vhost path, add a real-subprocess e2e (spawn `bore server` + `bore vhost
  --backend-tls` + a small self-signed HTTPS backend as actual processes) in the
  existing test layout.
- **Change:** add scenarios:
  - T-VBT-NETNS-NATIVE: native `bore vhost --backend-tls` → self-signed HTTPS
    backend → `curl`/client through the subdomain returns 200.
  - T-VBT-NETNS-SSH: real `ssh -R vhost/app:0:localhost:<port>` with
    `backend-tls=on` → self-signed HTTPS backend → 200.
  - Each scenario also runs its plaintext-backend counterpart WITHOUT the
    flag/param to confirm no regression.
  - Follow the CLAUDE.md netns discipline: NEVER run two netns harnesses
    concurrently (shared ns names); rebuild the release binary before a sudo run;
    invoke via the exact NOPASSWD sudo path (`sudo -n /abs/path/scripts/...`), not
    `sudo bash scripts/...`.
- **Unit tests:** none.
- **e2e tests:** T-VBT-NETNS-NATIVE, T-VBT-NETNS-SSH.
- **Done:** both scenarios pass on a real network stack (not loopback in-process);
  results recorded in `resume.md`.

### 4.2 Full regression
- **Model:** Sonnet
- **Files:** none (verification).
- **Change:** run and record:
  - `cargo fmt --all -- --check`
  - `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
  - `cargo test` (default features)
  - `cargo test --features ssh-gateway`
  - the vhost + SSH gateway netns harnesses (per 4.1 discipline).
  Confirm ZERO regressions: no pre-existing test changed its assertions; the only
  new/changed tests are the ones this plan introduced.
- **Unit tests:** the entire suite.
- **e2e tests:** the entire netns suite for vhost + ssh gateway.
- **Done:** every gate green; a regression summary recorded in `resume.md`.

### 4.3 Final consolidated documentation read (Opus)
- **Model:** Opus
- **Files:** `README.md` (vhost section `:325`–`:441`, and the SSH gateway
  section), `docs/vhost/`, `docs/SSH_GATEWAY.md` / `README-SSH-GATEWAY.md`,
  `CLAUDE.md` (add a one-line invariant note for backend-TLS if warranted).
- **Change:** verify the docs match the shipped behavior exactly (flag/param
  names, default OFF, self-signed skip-verify caveat, both reference examples,
  the SSH warning behavior, the deferred CA-pinning note). Fix any drift. Confirm
  README is the single source of truth (CLAUDE.md rule): both transports, every
  new flag/param, and example commands are present.
  - Add a concise invariant to `CLAUDE.md` under the vhost notes, e.g.: "vhost
    `--backend-tls` / `backend-tls=on` wraps the provider `LinkStream` in a
    server-side rustls client (accept-any-cert) so an HTTPS self-signed local
    backend works; `false` path is byte-identical; single-task splice preserves the
    yamux waker invariant."
- **Unit tests:** none.
- **e2e tests:** none.
- **Done:** docs accurate and complete; CLAUDE.md invariant added; final read
  signed off.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
- **Test subset:** the FULL suites (`cargo test`, `cargo test --features
  ssh-gateway`) + vhost/ssh netns harnesses.
- **Regression guard:** entire suite green; zero pre-existing assertions changed.

## Phase done criterion

Both transports serve a self-signed HTTPS backend through vhost on a real network
stack; the full cargo + netns regression is green with zero regressions; all
documentation (README, vhost docs, SSH gateway docs, CLAUDE.md invariant) matches
the shipped behavior. The feature is complete.

> **STOP.** All gates green? Report the full regression summary and the final doc
> status, then ASK the user whether to commit. Update `resume.md` (Phase 4 → DONE,
> `Next:` → none / feature complete).
