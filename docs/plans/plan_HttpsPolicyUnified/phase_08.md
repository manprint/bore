# Phase 7 — Final e2e, docs, coherence review

> **Intent:** Prove the reference scenario end-to-end over a real network (netns),
> complete user-facing docs, and run the Opus coherence review + full gate.
> **Shippable alone?** yes — closes the feature.
> **Preconditions:** phases 0-6 DONE.

---

## Sub-phases

### 7.1 netns e2e coverage
- **Model:** Sonnet
- **Files:** `scripts/ssh_gateway_test.sh` (netns harness, T-SSH-* IDs, `spawn_http_service` helper); any vhost netns script if present (grep `scripts/*vhost*`). Follow the existing harness structure; add tests, do not restructure.
- **Change:** Add real-network cases (invoked via `sudo -n /abs/path/scripts/ssh_gateway_test.sh`):
  - **T-SSH-N-HTTPS1**: SSH `-R vhost/app:0:...` with `https=redirect` against a gateway server with vhost `mode=both`+cert; `curl -sI http://app.<base>` returns `308` with `Location: https://...`; `curl -k https://app.<base>` returns `200`.
  - **T-SSH-N-HTTPS2**: SSH vhost `https=on` against a NO-cert server; forward comes up, `curl http://app.<base>` returns `200`, the SSH channel delivered a downgrade notice (grep the captured SSH client output).
  - **T-PUB-N-RAW**: native `bore local` (built from phase_04 flags) with `--https redirect`, then a RAW TCP client (non-HTTP) round-trips bytes unchanged (I-4 over real sockets).
  - Rebuild caveat: the harness must run against a freshly built binary — `cargo build --release` (and `--features ssh-gateway`) BEFORE the sudo run; state this in the script comment (mirrors the VPN netns rebuild caveat).
- **Unit tests:** n/a (shell e2e).
- **e2e tests:** T-SSH-N-HTTPS1, T-SSH-N-HTTPS2, T-PUB-N-RAW.
- **Done:** the three netns tests pass under `sudo -n`; existing T-SSH-N1..N6 still pass.

### 7.2 User-facing documentation
- **Model:** Haiku (draft) → Sonnet (technical accuracy pass)
- **Files:** `docs/VHOST.md`, `docs/SSH_GATEWAY.md`, top-level `README.md`. Existing files; no new dirs.
- **Change:**
  - `docs/VHOST.md`: document the per-subdomain `--https off|on|redirect`, the
    capability bound (vhost cert + `--vhost-mode`), the D10 `off`-on-shared-:443
    caveat, and the downgrade-warning behavior. Cross-reference the phase_06 §5.2
    param matrix.
  - `docs/SSH_GATEWAY.md`: document `https=on|off|redirect` / `force-https` params
    for vhost forwards (now applied, not warned), the banner "HTTPS policy" line, and
    the PTY note from phase_06.
  - `README.md`: update the `bore local` / `bore vhost` flag list to show
    `--https <off|on|redirect>` and mark `--force-https` deprecated.
  - Note the separate cert requirement (D3): public `--https` needs server
    `--cert-file`; vhost needs the vhost cert.
- **Unit tests:** n/a.
- **e2e tests:** n/a.
- **Done:** all three docs describe the final behavior; examples use `-T` not `-N`;
  the param matrix is linked and accurate.

### 7.3 Opus coherence review + full gate — OPUS REVIEW GATE (final read)
- **Model:** Opus
- **Files:** all changed files; `overview.md` invariants I-1..I-8.
- **Change:** Final read for:
  - I-2 byte-identical: diff the `policy=None` code paths against `main`; confirm no
    reordering in `edge.rs`, the public legacy `Error` branch, and the vhost global
    redirect gate.
  - I-6: confirm `ServerMessage::Warning` is sent ONLY under `https_policy.is_some()`
    in every send site (public 1.2, vhost 2.3).
  - I-5: confirm SSH leg still has no UDP/carriers/hole-punch and SSH notices use the
    channel, not `ServerMessage`.
  - Naming/semantics consistency: `off/on/redirect` mean the same in CLI, SSH, docs,
    banners.
  - Run the FULL gate (see below) with all features.
- **Unit tests:** the whole suite.
- **e2e tests:** the whole netns suite.
- **Done:** Opus sign-off recorded in `resume.md`; full gate green with zero
  regressions; the reference scenario in `overview.md` is demonstrably satisfied by
  named tests (map each of the 6 scenario lines to a T-ID).

---

## Phase gates (FULL, run here)

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --all-features -- -D warnings`
- **Test (default):** `cargo test`
- **Test (all features):** `cargo test --all-features`
- **Test (SSH suite):** `cargo test --all-features --test ssh_gateway_test --test ssh_gateway_spike_test -- --test-threads=1` (matches `.github/workflows/ci.yml:36`)
- **e2e netns:** `sudo -n /abs/path/scripts/ssh_gateway_test.sh` (after a fresh `cargo build --release --features ssh-gateway`)

## Phase done criterion

Every T-ID from the reference scenario passes (unit + netns), all six gate commands
are green, docs are complete, and Opus has recorded final sign-off in `resume.md`.
The feature ships with zero regressions against `main`.
