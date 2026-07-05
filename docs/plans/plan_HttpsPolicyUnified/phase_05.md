# Phase 4 — SSH gateway apply (G4)

> **Intent:** Make the SSH gateway APPLY `https`/`force-https` params to vhost
> tunnels (mapping to `HttpsPolicy`, reusing the phase_02/03 server path) instead
> of warning "not applicable". Report the effective policy in the vhost banner.
> **Shippable alone?** yes — public SSH already applies; this adds vhost.
> **Preconditions:** phase_01, phase_02, phase_03 DONE (wire field + registry
> policy + resolver exist). Phase_04 (native CLI) NOT required.

---

## Sub-phases

### 4.1 Apply policy on the SSH vhost path — OPUS REVIEW GATE (I-SSH8 flip)
- **Model:** Sonnet; **Opus reviews** the behavior flip and its test rewrite.
- **Files:** `src/sshgw.rs` — `Params` struct (`2504-2525`, has `https`/`force_https` parsed `value=="on"` at `2628-2629`); the vhost registration path (`769-801`, hardcoded `https:false, force_https:false` at `776-777`); the vhost `deliver_inapplicable_warnings` call (`803-813`).
- **Change:**
  1. Map the parsed SSH params to a policy at the vhost registration site:
     ```rust
     let https_policy = match (params.force_https, params.https) {
         (true, _) => Some(HttpsPolicy::Redirect),
         (false, true) => Some(HttpsPolicy::On),
         (false, false) => None, // neither set = inherit server default
     };
     ```
     > **Note:** SSH has no explicit `off` today (`https=off` parses to `false` =
     > "not set" = `None` = inherit). To support an explicit SSH `off`, extend the
     > parse at `2628-2629` to distinguish `off` from absent: track
     > `https_seen: bool` (any `https=` key present) and, if `https=off` was
     > explicitly given, set `Some(HttpsPolicy::Off)`. If this adds complexity,
     > ship `off`-via-SSH as a follow-up and document that SSH `https=off` currently
     > equals inherit. **Decide with Opus in review.** (Native CLI has full `off`.)
  2. Populate the vhost provider registry entry's `https_policy` (the field added in
     phase_02 §2.1) from this value, so the router (phase_02 §2.2) applies it — the
     SSH path and native path converge on the same registry field.
  3. REMOVE `https` and `force-https` from the vhost `deliver_inapplicable_warnings`
     checklist at `803-813` (they are now applicable). Leave `max-conns` and any
     other genuinely-inapplicable param warnings intact.
  4. Set the admin `NewEntry` `https`/`force_https` at `776-777` to the EFFECTIVE
     display flags (reuse `vhost_display_flags` from phase_02 §2.1 with vhost
     capability), not hardcoded `false`.
  5. Downgrade warning: if the client asked for `On`/`Redirect` but vhost is not
     capable, deliver a NON-fatal notice via `ConnState::deliver` (the SSH session
     channel, per I-SSH5/I-SSH8 — NOT `ServerMessage`; SSH is a different transport):
     e.g. `"bore ssh-gateway: server not configured for vhost HTTPS; serving <label> over HTTP"`.
- **Unit tests:** the mapping is small; add a pure helper test if factored:
  `ssh_params_to_policy` — `(force_https=true)→Redirect`, `(https=true)→On`,
  `(neither)→None`.
- **e2e tests:** T-SSH-VH-HTTPS1/2/3 (4.3).
- **Done:** Opus signs off on the I-SSH8 flip; the vhost SSH path applies policy via
  the shared registry field; no more inapplicable warning for https/force-https on
  vhost. Gates green.

### 4.2 Vhost banner reports effective policy
- **Model:** Sonnet
- **Files:** `src/sshgw.rs:2769-2796` (`vhost_info_banner`; today prints urls, mode, identity, notes, basic_auth, webserver_log, headers).
- **Change:** Add one line to the vhost banner:
  `HTTPS policy:        <inherit(mode=<VhostMode>) | off | on | redirect> [(active) | (downgraded to HTTP: server has no HTTPS)]`.
  Derive from the entry's `https_policy` + vhost capability. When `None`, show
  `inherit (mode=<mode>)`. When a downgrade occurred, append the downgrade note.
  Keep the existing `Mode:` line. Match the existing banner formatting/style exactly
  (column alignment, `professional English, no emojis` per I-SSH7).
- **Unit tests:** none (string formatting; asserted in 4.3 banner test).
- **e2e tests:** T-SSH-VH-HTTPS1 asserts the banner line.
- **Done:** the banner shows the effective HTTPS policy; existing banner fields unchanged.

### 4.3 SSH vhost HTTPS tests (+ rewrite the flipped test)
- **Model:** Sonnet
- **Files:** `tests/ssh_gateway_test.rs` (reuse `start_gateway_server_vhost` at `306-370`; vhost tests at `1385+`; the existing `t_ssh_warn_https_inapplicable_to_vhost`).
- **Change:**
  - **REWRITE** `t_ssh_warn_https_inapplicable_to_vhost` → `t_ssh_vhost_https_applied`:
    > **Behavior change (loud):** this test previously asserted a "not applicable"
    > warning. It must now assert the policy is APPLIED (no such warning; the entry
    > carries the policy). Update its name and body.
  - **T-SSH-VH-HTTPS1** `t_ssh_vhost_https_redirect_applied`: gateway server with vhost `mode=both` + cert; SSH forward `-R vhost/app:0:...` with exec/env param `https=redirect`. Assert: admin/registry entry for `app` has effective `force_https=true`; the vhost banner contains `HTTPS policy: redirect (active)`; NO inapplicable-warning line is delivered.
  - **T-SSH-VH-HTTPS2** `t_ssh_vhost_https_off_no_redirect`: server `mode=redirect-https`; param `https=off` (or, if 4.1 ships off-as-inherit, use native coverage instead and assert the documented inherit behavior). Assert the subdomain is not force-redirected.
  - **T-SSH-VH-HTTPS3** `t_ssh_vhost_https_no_cert_downgrades`: server `mode=http` (no vhost cert); param `https=on`. Assert the forward succeeds, a downgrade notice is delivered on the channel, and the entry shows `https=false`.
  - Keep public SSH https tests (`public_info_banner` path) passing unchanged.
- **Unit tests:** `ssh_params_to_policy` (4.1) if factored.
- **e2e tests:** T-SSH-VH-HTTPS1..3 (in-process cargo). netns coverage in phase_08.
- **Done:** all new SSH vhost tests pass; the renamed test asserts application; public SSH tests unchanged.

---

## Phase gates

- **Fmt:** `cargo fmt --check`
- **Lint:** `cargo clippy --all-targets --features ssh-gateway -- -D warnings` (and `--all-features`)
- **Test subset:** `cargo test --all-features --test ssh_gateway_test -- --test-threads=1` (matches CI at `.github/workflows/ci.yml:36`)
- **Regression guard:** all other `ssh_gateway_test.rs` tests pass; public SSH banner/apply unchanged; I-SSH5/6/7 behavior intact.

## Phase done criterion

An SSH client passing `https=redirect` on a vhost forward gets a per-subdomain
redirect and a banner line stating the effective policy (T-SSH-VH-HTTPS1); an
HTTPS request against a no-cert server downgrades with a channel notice
(T-SSH-VH-HTTPS3); the former "inapplicable" test now asserts application.
