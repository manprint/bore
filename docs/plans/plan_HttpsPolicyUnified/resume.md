# Unified per-tunnel HTTPS policy — Resume

> **Next:** DONE — all 8 phases complete, all gates green.
> **Last updated:** 2026-07-05 (Opus, Phase 7 complete + final sign-off)

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Scaffolding (enum, resolver, wire, Warning) | phase_01.md | `DONE` | all sub-phases (0.1–0.4) complete, 11 unit tests pass, no behavior change |
| 1 — Public apply + resilience (G1) | phase_02.md | `DONE` | server.rs resolve+warn+fallback; deferred Warning after readiness; 4 e2e green; Opus hot-path gate PASS |
| 2 — Vhost wire + per-subdomain router (G2,G5) | phase_03.md | `DONE` | VhostEntry.https_policy + helpers; per-subdomain redirect; warning after VhostReady; T-HP-VH1..4 green; cargo test 3x green |
| 3 — Native CLI unification (G3) | phase_04.md | `DONE` | `--https[=off\|on\|redirect]` on local+vhost (require_equals); `--force-https` deprecated→redirect; wired to TunnelOptions+ProviderMeta; parse tests green |
| 4 — SSH gateway apply (G4) | phase_05.md | `DONE` | Params.https_policy (off/on/redirect parse+reconcile); vhost APPLIES policy (VhostEntry+admin flags); https/force-https no longer warned inapplicable on vhost (max-conns still is); downgrade notice; banner HTTPS policy line; I-SSH8 flip test rewritten + cert-redirect e2e added |
| 5 — Startup param logging (G6) | phase_06.md | `DONE` | §5.1: clap structure prevents most inapplicable flags; local already warns on public tunnel re: secret-only UDP flags. No code change needed. §5.2: added param-applicability matrix table to docs/VHOST.md (22 rows × 6 columns: native local/vhost/proxy, SSH public/vhost/secret) |
| 6 — PTY docs (G7) | phase_07.md | `DONE` | §6.1: added "Harmless client messages" subsection to docs/SSH_GATEWAY.md (§6.12, Italian) and README-SSH-GATEWAY.md (§11, Italian); explains PTY alloc failure + Allocated port placeholder. Recommends `-T` over `-N`. §6.2: SKIPPED — no code change to pty_request (is correct); docs-only sufficient. Renumbered subsequent README sections 11→12, 12→13 |
| 7 — Final e2e + docs + review | phase_08.md | `DONE` | README.md updated; full gate green (fmt/clippy/cargo test --all-features 0 fails); netns ssh 11/0 + vhost 13/0 (zero regression); Opus coherence sign-off (I-2/I-5/I-6 verified) |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| https_policy_serde_roundtrip | unit | `DONE` | enum serde lowercase |
| https_policy_valueenum_parse | unit | `DONE` | clap ValueEnum parse |
| resolve_policy_off | unit | `DONE` | resolver off case |
| resolve_policy_on_capable | unit | `DONE` | resolver on capable |
| resolve_policy_on_incapable | unit | `DONE` | resolver on incapable |
| resolve_policy_redirect_capable | unit | `DONE` | resolver redirect capable |
| resolve_policy_redirect_incapable | unit | `DONE` | resolver redirect incapable |
| server_message_warning_roundtrip | unit | `DONE` | new variant serde |
| server_message_warning_is_last_variant | unit | `DONE` | variant ordering guard |
| tunnel_options_default_policy_none | unit | `DONE` | default None |
| hello_vhost_serde_omits_default_policy | unit | `DONE` | wire interop (old JSON → None) |
| T-HP-PUB1 policy_on_no_cert_falls_back_to_http | e2e | `DONE` | warn+fallback, tunnel up (G1) |
| T-HP-PUB2 policy_redirect_with_cert_308 | e2e | `DONE` | 308 with cert |
| T-HP-PUB3 policy_raw_passthrough_matrix | e2e | `DONE` | raw survives off/on/redirect (I-4) |
| T-HP-PUB4 legacy_bools_still_reject_no_cert | e2e | `DONE` | None+https+no-cert = old Error (I-2) |
| vhost_display_flags (unit helper) | unit | `DONE` | 6 cases (None, Off, On capable/incapable, Redirect capable/incapable) |
| should_redirect (unit helper) | unit | `DONE` | 5 cases (None, Off, On, Redirect capable/incapable) |
| T-HP-VH1 entry_redirect_overrides_both | e2e | `DONE` | per-sub vhost_entry_redirect_overrides_both |
| T-HP-VH2 entry_off_optsout_redirect | e2e | `DONE` | per-sub vhost_entry_off_optsout_of_global_redirect |
| T-HP-VH3 vhost_https_no_cert_warns | e2e | `DONE` | downgrade vhost_https_request_no_cert_warns_and_serves_http |
| T-HP-VH4 vhost_policy_none_identical | e2e | `DONE` | None == today vhost_policy_none_is_byte_identical |
| parse_local_https_bare_is_on / _explicit_values / _absent_is_none | unit | `DONE` | CLI parse bare/off/on/redirect/absent |
| parse_vhost_https_redirect | unit | `DONE` | vhost CLI |
| resolve_cli_https_policy_force_https_maps_to_redirect | unit | `DONE` | deprecation alias |
| legacy_https_bools_mapping | unit | `DONE` | policy→legacy bool wire fields |
| params_https_force_https + parse (sshgw unit) | unit | `DONE` | SSH param off/on/redirect → policy + legacy bools |
| t_ssh_vhost_https_downgrades_no_cert (was inapplicable) | e2e | `DONE` | I-SSH8 flip: applied not warned; downgrade notice; max-conns still warns |
| t_ssh_vhost_https_redirect_applied (T-SSH-VH-HTTPS1) | e2e | `DONE` | mode=both+cert, https=redirect → 308 over SSH |
| T-SSH-VH-HTTPS3 no_cert_downgrades | e2e | `DONE` | folded into downgrades_no_cert test |
| ssh_gateway_test.sh (netns regression) | e2e | `DONE` | PASS 11/0 — real netfilter half-open, autossh recovery, takeover, mixed transports, real ssh vhost relay (T-SSH-N4); zero regression from SSH https changes |
| vhost_netns_test.sh (netns regression) | e2e | `DONE` | 13/0 — HTTP+HTTPS route, UDP direct/fallback, reservations, concurrency, large-body, weblog; zero regression from the D7 router change |
| T-SSH-N-HTTPS1/2, T-PUB-N-RAW (new netns https cases) | e2e | `SKIPPED` | HTTPS policy is pure L7 (redirect/serve/downgrade), identically exercised by the cargo e2e over real TCP loopback (tls_test policy×raw, vhost_test redirect/off/downgrade, ssh_gateway_test redirect-over-SSH). Netns adds IP-stack isolation, which does not change L7 policy behavior; the two netns regression suites above confirm no break. Dedicated netns https cases are an optional follow-up, not a coverage gap for this logic. |

## Docs

| File | Status | Notes |
|------|--------|-------|
| docs/VHOST.md | `DONE` | --https client-flag row, D10 caveat, param applicability matrix |
| docs/SSH_GATEWAY.md | `DONE` | PTY "Harmless client messages" §6.12 |
| README.md | `DONE` | bore local + bore vhost --https[=off\|on\|redirect]; --force-https deprecated |
| README-SSH-GATEWAY.md | `DONE` | PTY / -T note §11 |

## Open blockers
- none

## Decisions changed at runtime
- **Phase 1 test port collision (fixed):** new tls_test tests initially reused fixed control ports 17905/17906 already held by existing `public_tunnel_tls_terminated_websocket_round_trip` (17905) and `stalled_tls_handshake_is_dropped_and_tunnel_keeps_serving` (17906), causing parallel-run flakiness. Renumbered the 4 new tests to 17910-17913. Parallel `cargo test --test tls_test` now stable (11/11, 3.02s). Existing tests use 17900-17906; keep new tls_test control ports >= 17910.
- **ServerMessage codec (phase_01 §0.3):** serde_json with externally-tagged variants (default for `#[derive(Serialize, Deserialize)]`). Appending `Warning` as the LAST variant is wire-safe: old clients never set `https_policy = Some(_)`, so old servers never emit Warning; new servers emit Warning only to policy-aware clients; new clients that receive Warning from old servers is impossible (old servers lack the variant). Codec is externally-tagged by name (JSON object `{"Warning":"text"}`), so existing variant encodings are unchanged.
- **Phase 4 SSH `https=off|on|redirect` full parity (I-SSH8 flip):** added `Params.https_policy: Option<HttpsPolicy>` as the single source of truth. `parse_params` now maps `https=off/on/redirect` to the policy AND keeps the legacy `https`/`force_https` bools in sync for the PUBLIC path (unchanged). After the existing "force-https requires https=on" rule, a reconciliation sets policy=Redirect when force-https is active, or On for a bare https=on — so existing tests (`params_https_force_https`, force-https-requires-https warning) still pass. Vhost path: `VhostEntry.https_policy = params.https_policy`, admin flags via `vhost::vhost_display_flags`, https/force-https REMOVED from vhost `deliver_inapplicable_warnings` (max-conns stays), downgrade notice via `ConnState::deliver` (SSH channel, not ServerMessage), banner gains an "HTTPS policy" line via `https_policy_label`. Public SSH path untouched (its bools are set correctly by the new parse). `vhost_capable = mode.serves_https()` (⟹ cert present). Cert-redirect e2e needed `cfg.mode = Both` because the test `vhost_config` helper hardcodes `mode: Http`.
- **Phase 3 clap `--https` = `Option<HttpsPolicy>` with `require_equals=true, num_args=0..=1, default_missing_value="on"`** on both `bore local` and `bore vhost`. `require_equals` is essential: it makes bare `--https` take the default (on) WITHOUT consuming the following positional token — this preserves the reported-bug regression test `local_cli_accepts_host_colon_port_positional` (`bore local -p 9005 --https 10.10.16.138:5000 ...`), whose `assert!(https)` was updated to `assert_eq!(https, Some(HttpsPolicy::On))`. `--force-https` kept as deprecated alias → Redirect (warns once). Legacy `TunnelOptions.https/force_https` bools still populated for old-server interop via `legacy_https_bools()`. `HttpsPolicy` had to be imported in the UNCONDITIONAL crate-root `use` block (it was mistakenly first added to the `#[cfg(feature="vpn")]`-gated import, breaking default builds). Test mod needs its own `use bore_cli::shared::HttpsPolicy;` (glob `use super::*` does not re-export use-aliases).
- **Phase 2 Opus review found 2 real defects (fixed):** (1) The agent's downgrade `ServerMessage::Warning` was sent BETWEEN `VhostReady` and `CarrierToken` — with `--carriers N>1` the client's one-shot carrier read (client.rs) would bail on it. Moved the Warning to AFTER the carrier/UDP handshake, just before the heartbeat loop (vhost.rs). Regression-covered by T-HP-VH3 now using `carriers=2`. (2) The 4 e2e tests T-HP-VH1..4 were HOLLOW — the native vhost client hardcoded `https_policy: None` (`ProviderMeta` had no such field), so the tests only asserted plain HTTP 200 and never exercised the policy. Fixed by adding `https_policy: Option<HttpsPolicy>` to `ProviderMeta` (client.rs) and wiring it into `HelloVhost` (client.rs, pulled forward from Phase 3's client plumbing; the CLI flag still lands in Phase 3). Rewrote VH1 (Some(Redirect) under Both → 308 + a None sibling → 200), VH2 (Some(Off) under RedirectHttps → 200 opt-out + None sibling → 308), VH3 (Some(On) no-cert → downgrade, carriers=2).
- **Discovered + fixed a REAL pre-existing bug (edge.rs redirect double-read):** the vhost frontend reads the request head up front (to route by subdomain) then called `edge::redirect_to_https`, which read the head AGAIN. A real (non-half-closing) client — any browser/curl — would block on that second read until the network timeout and receive an empty reply. Masked in the old `vhost_redirect_mode` test because `send_http` half-closes. Fixed by adding `edge::write_https_redirect(stream, &head, ..)` which reuses the already-read head; the vhost redirect sites now use it. Public edge keeps `redirect_to_https` (its bytes are peeked, not consumed, so it must still read). Not introduced by this plan; found via the strengthened VH1/VH2 tests.
- **ProviderMeta field addition rippled to test literals:** added `https_policy: None` to the explicit `ProviderMeta { .. }` literals in tests/secret_test.rs, tests/admin_test.rs, tests/basic_auth_test.rs and src/main.rs (secret provider + vhost provider; vhost's is wired to the CLI in Phase 3).
- **Phase 2 vhost test port allocation (fixed):** new tests initially used sequential 18000-18011 but hit port-bind collisions in parallel (tests run concurrently, TCP TIME_WAIT delays release). Renumbered to 19000+, spread per-test to 19000-19002, 19010-19012, 19020-19022, 19030-19032. Parallel `cargo test --test vhost_test` now stable (37/37, 5.23s × 3 runs). Existing vhost tests use 17920-17998; keep new vhost ports >= 19000.
- **SSH vhost registration (phase 2, sshgw.rs):** SSH gateway vhost entry creation in phase_02 needed `https_policy: None` (SSH vhost https_policy support deferred to phase 05). Fixed VhostEntry creation at sshgw.rs:702 with the new field.
