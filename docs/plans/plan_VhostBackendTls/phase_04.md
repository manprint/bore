# Phase 3 — SSH gateway param plumbing (`backend-tls=on`)

> **Intent:** Expose the feature over the SSH gateway: a `backend-tls` (and
> `backend-tls-sni=<name>`) exec/env param on an `ssh -R vhost/...` forward,
> parsed into `Params`, mapped into `VhostEntry.backend_tls`, with an explicit
> warning when used on a non-vhost (public/secret) SSH forward, and a banner line.
> After this phase the SSH reference scenario works.
> **Shippable alone?** yes — a new opt-in param; absent it, the SSH gateway is
> unchanged.
> **Preconditions:** Phase 2 DONE (the server seam already honors
> `VhostEntry.backend_tls` from Phase 1).

Feature-gated: this phase compiles only under `--features ssh-gateway`.

---

## Sub-phases

### 3.1 Add `backend_tls` / `backend_tls_sni` to `Params` and parse them
- **Model:** Sonnet
- **Files:** `src/sshgw.rs:2654` (`struct Params`; fields `notes:2659`,
  `basic_auth:2663`, `webserver_log:2665`, `https:2669`, `force_https:2673`,
  `https_policy:2680`); the `parse_params` function (locate by symbol — near the
  `Params` struct).
- **Change:**
  - Add to `Params`: `backend_tls: bool,` and `backend_tls_sni: Option<String>,`.
  - In `parse_params`, parse the tokens (mirror how `https`/`webserver-log`/`notes`
    are parsed):
    - `backend-tls=on` / `backend-tls=true` / bare `backend-tls` → `backend_tls = true`
      (follow the exact truthiness convention the existing boolean params use;
      match their accepted spellings).
    - `backend-tls-sni=<name>` → `backend_tls_sni = Some(name)`.
  - Default: `backend_tls = false`, `backend_tls_sni = None`.
- **Unit tests:** in the existing `sshgw` param-parse test module:
  - `parse_params_backend_tls_on` — `parse_params` on an exec string containing
    `backend-tls=on backend-tls-sni=app` yields `backend_tls == true`,
    `backend_tls_sni == Some("app")`.
  - `parse_params_backend_tls_default_off` — absent the tokens, both default off.
- **e2e tests:** none yet (3.4).
- **Done:** gates green; parse tests pass.

### 3.2 Map `Params` → `VhostEntry` in the SSH vhost handler
- **Model:** Sonnet
- **Files:** `src/sshgw.rs:818` (`tcpip_forward_vhost`), `:903`–`:929` (the
  `VhostEntry { .. }` build; existing maps: `webserver_log:925`, `https_policy:926`,
  `gateway_basic_auth:915`).
- **Change:** set `backend_tls: params.backend_tls` and
  `backend_tls_sni: params.backend_tls_sni.clone()` in the `VhostEntry` built at
  `:903`–`:929`, replacing the `false`/`None` placeholders left in Phase 0.2.
  Follow the existing `params.<field>` → entry mapping pattern.
- **Unit tests:** if a unit test exercises `tcpip_forward_vhost` entry building,
  extend it; otherwise covered by 3.4 e2e.
- **e2e tests:** none yet.
- **Done:** gates green; placeholders replaced.

### 3.3 Warn when `backend-tls` is used on a non-vhost SSH forward (I-SSH8)
- **Model:** Sonnet
- **Files:** `src/sshgw.rs` — `deliver_inapplicable_warnings` (locate by symbol)
  and its call sites in the public and secret `tcpip_forward_*` tasks.
- **Change:** add `backend-tls` (and `backend-tls-sni`) to the inapplicable-param
  checks for PUBLIC and SECRET forward types, so a client that sets them on a
  non-vhost forward gets an explicit warning line (never silent). Mirror the
  existing checks for params that are no-ops on those types. Do NOT warn for the
  vhost type (there it IS applicable).
- **Unit tests:** `backend_tls_inapplicable_to_public_warns` /
  `backend_tls_inapplicable_to_secret_warns` — assert the warning set contains a
  `backend-tls` entry for those forward types and does NOT for vhost. Follow the
  existing I-SSH8 test pattern (`t_ssh_warn_*` in `tests/ssh_gateway_test.rs`).
- **e2e tests:** none.
- **Done:** gates green; warning tests pass.

### 3.4 Banner line + SSH e2e + docs
- **Model:** Haiku (banner + docs) + Sonnet (e2e)
- **Files:** `src/sshgw.rs` `vhost_info_banner` (locate by symbol; I-SSH7);
  `tests/ssh_gateway_test.rs` (real OpenSSH driver, `TestNoVerifier:43`);
  `docs/SSH_GATEWAY.md` if present else `README-SSH-GATEWAY.md` / the README SSH
  section; the netns script `scripts/ssh_gateway_test.sh`.
- **Change:**
  - Banner: when the vhost forward has `backend_tls` on, add a short line to
    `vhost_info_banner` (e.g. `Backend: TLS (certificate verification disabled)`).
    Cosmetic, non-destructive (delivered via `ConnState::deliver`).
  - Test T-VBT3: drive a REAL `ssh -R vhost/app:0:localhost:<port>` (reuse the
    existing OpenSSH e2e driver) with the exec param `backend-tls=on` toward a real
    self-signed HTTPS backend; assert a request to the subdomain returns 200. Add a
    companion assertion that an SSH vhost forward WITHOUT the param to a plaintext
    backend still returns 200 (regression).
  - Docs: document the `backend-tls` / `backend-tls-sni` exec params for SSH vhost
    forwards, with an example command and the same security caveat (skip-verify).
    Note `-N` remains discouraged (banner/warning visibility, existing guidance).
- **Unit tests:** covered by 3.1/3.3.
- **e2e tests:** T-VBT3 (real OpenSSH, real TLS backend).
- **Done:** gates green; T-VBT3 passes; banner shows for backend-TLS forwards; SSH
  docs updated.

---

## Phase gates

- **Fmt:** `cargo fmt --all -- --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings`
- **Test subset:** `cargo test --features ssh-gateway --test ssh_gateway_test backend_tls`
  + the `parse_params` / warning unit tests.
- **Regression guard:** full `cargo test --features ssh-gateway` green; all
  existing SSH gateway tests (takeover, demux, banners, I-SSH suite) unchanged.

## Phase done criterion

`ssh -R vhost/app:0:localhost:<port>` with `backend-tls=on` against a self-signed
HTTPS backend serves 200 through the subdomain (T-VBT3); `backend-tls` on a
public/secret SSH forward emits an explicit warning; the banner reports backend
TLS; SSH gateway docs are updated; existing SSH tests are unchanged.

> **STOP.** All gates green? Report status + T-VBT3 result + warning-test results +
> doc diff, then ASK the user for explicit confirmation before Phase 4. Update
> `resume.md` (Phase 3 → DONE, `Next:` → phase_05 § 4.1).
