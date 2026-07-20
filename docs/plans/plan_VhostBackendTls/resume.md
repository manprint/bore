# Vhost Backend TLS — Resume

> **Next:** none — feature COMPLETE (awaiting commit decision)
> **Last updated:** 2026-07-20 (Phase 4 done)

## Phase status

| Phase | File | Status | Notes |
|-------|------|--------|-------|
| 0 — Wire + entry + connector scaffolding | phase_01.md | `DONE` | additive, no behavior; all gates green |
| 1 — Server-side backend-TLS wrap (core) | phase_02.md | `DONE` | wrap at relay seam; red-check recorded; all gates green |
| 2 — Native client plumbing + docs | phase_03.md | `DONE` | `bore vhost --backend-tls`; wire→entry mapped; T-VBT2 + clap tests green; README updated |
| 3 — SSH gateway param plumbing + docs | phase_04.md | `DONE` | `backend-tls=on`/`backend-tls-sni=`; Params→entry; public+secret warn; banner; T-VBT3 green; docs updated |
| 4 — Full regression + netns e2e + doc read | phase_05.md | `DONE` | netns both transports green; full cargo default+ssh green; CLAUDE.md invariant added |

Status values: `TODO` · `IN_PROGRESS` · `DONE` · `SKIPPED` · `BLOCKED`

## Tests

| ID | Type | Status | Notes |
|----|------|--------|-------|
| hello_vhost_backend_tls_serde_roundtrip | unit | `DONE` | fields survive round-trip |
| hello_vhost_old_wire_defaults_backend_tls_off | unit | `DONE` | I-2 wire compat |
| vhost_entry_backend_tls_defaults_off | unit | `DONE` | entry default off |
| insecure_tls_connector_builds | unit | `DONE` | helper builds (http/1.1 ALPN) |
| backend_server_name_valid / _rejects_garbage | unit | `DONE` | no panic on bad SNI |
| backend_tls_wrap_handshakes_with_self_signed | unit/integ | `DONE` | core wrap; red-checked (see log) |
| backend_tls_bad_sni_fails_gracefully | unit/integ | `DONE` | I-5 no hang/panic (empty SNI → Err) |
| backend_tls_against_plaintext_backend_times_out_or_errors | unit/integ | `DONE` | I-4 no hang (EOF → fast Err) |
| backend_tls_off_path_unchanged | unit/integ | `DONE` | I-1 regression (plaintext → 200) |
| vhost_backend_tls_flags_parse / _defaults_off | unit | `DONE` | clap parse (main.rs) |
| serve_vhost_provider_maps_backend_tls | unit | `DONE` | covered by T-VBT2 (full native path exercises HelloVhost→entry mapping) |
| T-VBT2 (vhost_backend_tls_native_end_to_end) | e2e | `DONE` | native real-TLS backend → 200; no-flag plaintext → 200 (I-1) |
| parse_params_backend_tls_on / _default_off | unit | `DONE` | SSH param parse (src/sshgw.rs) |
| t_ssh_vbt_backend_tls_inapplicable_to_public_warns | e2e | `DONE` | I-SSH8 public warn (real ssh) |
| t_ssh_warn_all_params_inapplicable_to_secret_provider | e2e | `DONE` | extended with backend-tls / -sni warn |
| T-VBT3 (t_ssh_vbt3_backend_tls_forward_serves_https_backend) | e2e | `DONE` | real ssh -R vhost + backend-tls=on → 200; no-flag plaintext → 200 |
| T-VBT-NETNS-NATIVE | e2e | `DONE` | vhost_netns_test.sh Test 3b; real netns HTTPS backend → 200 |
| T-VBT-NETNS-SSH | e2e | `DONE` | ssh_gateway_test.sh; backend-tls=on + plaintext both → 200 (16/0) |
| T-VBT-NETNS-UDP (Test 7b) | e2e | `DONE` | UDP/QUIC DIRECT path + --backend-tls → QUIC carrier established + HTTPS backend served |
| T-VBT-NETNS-CARRIERS (Test 7c) | e2e | `DONE` | --backend-tls --carriers 2 (TCP relay) → 6/6 requests served |
| T-VBTBENCH | bench | `DONE` | vhost_bench.sh backend-tls overhead table (plaintext vs --backend-tls, tcp-1c/tcp-4c/udp-1c) |

## Docs

| File | Status | Notes |
|------|--------|-------|
| README.md (vhost section) | `DONE` | --backend-tls / --backend-tls-sni table rows + examples + security note (verification-skipped) |
| README.md (SSH gateway section) / docs/SSH_GATEWAY.md | `DONE` | exec-param table row + example; SSH_GATEWAY.md §3 row + new §6.14 (security caveat) |
| docs/vhost/ | `TODO` | flag reference if a dedicated doc exists |
| CLAUDE.md | `DONE` | backend-TLS invariant bullet added under vhost notes |

## Open blockers
- none

## Decisions changed at runtime
- Phase 0: backend connector offers ALPN `http/1.1` only (NOT `bore`) — bore
  relays the backend leg as HTTP/1.x, so HTTP/2 negotiation would corrupt head
  parsing/injection; the control-plane `bore` ALPN must not leak to a foreign
  server. Encoded in `transport::insecure_tls_connector`.
- Phase 0: extra edit sites found beyond the plan anchors (all handled):
  `HelloVhost` is destructured with explicit fields at `src/server.rs:1659`
  (added `backend_tls: _, backend_tls_sni: _` — wire in Phase 2.3); two extra
  `HelloVhost` test constructors at `src/shared.rs` (~2489, ~2976); one extra
  `VhostEntry` test constructor `test_entry` at `src/vhost.rs:~1975`.
- Phase 0: `insecure_tls_connector`/`backend_server_name` carry
  `#[allow(dead_code)]` (used only from the relay in Phase 1) — REMOVE the
  attribute in Phase 1.1 once the relay calls them.

## Red-check log
- Phase 1.2 (2026-07-20): DONE. With the `if entry.backend_tls { ... }` wrap
  disabled (`if false && entry.backend_tls`), the plaintext HTTP head reaches the
  TLS backend, rustls rejects it, and `backend_tls_wrap_handshakes_with_self_signed`
  FAILS ("expected 200 from backend: <empty>"). Restoring the wrap → test PASSES.
  Proves the wrap is what makes the HTTPS backend reachable (not a false-pass).

## Post-Phase-4 coverage hardening (UDP path + carriers + perf)
- Gap found in review: all e2e had exercised the TCP relay path only. Closed:
  - vhost_netns_test.sh Test 7b: `bore vhost --backend-tls --udp` → asserts the
    QUIC DIRECT carrier established (fresh server log, unambiguous) AND the HTTPS
    backend's body. Empirically proves the wrap works on the QUIC stream, not just
    TCP carriers.
  - vhost_netns_test.sh Test 7c: `--backend-tls --carriers 2` (TCP relay) → 6/6.
  - Both use a shared `start_https_backend` helper (python ssl, harness cert).
  - vhost netns now 16/0.
- vhost_bench.sh: added an HTTPS origin (`ORIGIN_TLS_PORT`, python
  SimpleHTTPRequestHandler + ssl) + `measure_tput` + `bench_backend_tls_delta` +
  a `T-VBTBENCH` table (plaintext backend vs --backend-tls, tcp-1c/tcp-4c/udp-1c).
  Overhead is REPORTED, not gated (inherent TLS-leg cost; flag opt-in, OFF path
  byte-identical). Numbers pending the background bench run.

## Phase 4 runtime notes
- netns discipline honored: rebuilt `--release --features "ssh-gateway udp"`
  (both harnesses use `--udp`) BEFORE the sudo runs; ran ONE harness at a time
  (never concurrent — shared ns names); invoked via exact `sudo -n /abs/.../scripts/...`.
- vhost harness: added Test 3b (native `--backend-tls` → python-ssl HTTPS backend
  in nsp1 using the harness `*.bore.local` cert; SNI mismatch OK — accept-any).
- ssh harness: added `spawn_https_service` + T-VBT-NETNS-SSH. `ssh_cmd` can't
  append a trailing command (destination goes last) → invoked ssh inline with
  `... "gwtest@$SERVER_IP" 'backend-tls=on'`. First run caught a real flake: the
  no-flag `-N` companion 404'd because the two sessions register asynchronously —
  fixed with a `vbt_probe` retry loop (30×0.5s), NOT a longer fixed sleep.
- Full gates GREEN: fmt 0, clippy 0, `cargo test` (default) 27 groups/0 fail,
  `cargo test --features ssh-gateway` 27 groups/0 fail, vhost netns 14/0, ssh
  netns 16/0. Zero pre-existing assertions changed.

## Phase 3 runtime notes
- `Params` (src/sshgw.rs) +`backend_tls: bool` / `backend_tls_sni: Option<String>`;
  derives `Default` so `parse_params` (only constructor) needs no other edit.
- `parse_params` arm accepts `backend-tls=on`/`=true`/`=` (empty) as true. A BARE
  `backend-tls` token (no `=`) is classified malformed upstream by
  `parse_kv_tokens` → the documented form is `backend-tls=on`.
- Mapped into the SSH `VhostEntry` at the `tcpip_forward_vhost` build site.
- I-SSH8: PUBLIC had NO existing `deliver_inapplicable_warnings` call — added one
  (context "public") for backend-tls/-sni; SECRET extended its existing check
  array. VHOST intentionally does NOT warn (applicable there).
- Banner: `VhostBannerInfo` +`backend_tls`; `vhost_info_banner` emits
  `Backend: TLS (certificate verification disabled)` only when set.
- docs/SSH_GATEWAY.md: §6.11 is the banner section and is referenced elsewhere —
  did NOT renumber; added backend-tls as new §6.14 and pointed the §3 table row
  at §6.14.

## Phase 2 runtime notes
- `ProviderMeta` gained `backend_tls: bool` + `backend_tls_sni: Option<String>`.
  It derives `Default`, so the three `..Default::default()` sites in
  `tests/vhost_test.rs` need no edit; the four explicit constructors
  (`src/main.rs:1596` secret provider, `tests/{basic_auth,secret,admin}_test.rs`)
  got the two fields added (all `false`/`None`).
- `Command::Vhost` in `src/main.rs` is destructured with explicit fields (no
  `..`) at the handler — added `backend_tls`/`backend_tls_sni` there and to the
  `ProviderMeta` it builds. Two OTHER `Command::Vhost` destructures at
  `src/main.rs` use `.., ..` and needed no change.
- `serve_vhost_provider` (src/vhost.rs) gained the two params AFTER
  `https_policy`; the single caller `src/server.rs:1681` passes them (and its
  `HelloVhost` destructure now binds them instead of `_`).
- clap flags carry env fallbacks `BORE_BACKEND_TLS` / `BORE_BACKEND_TLS_SNI`;
  parse tests take `ENV_GUARD` like the sibling flag tests.
- 2.3 unit test folded into T-VBT2: the full native path constructs a real
  `HelloVhost` (backend_tls=true) → server maps it into the `VhostEntry` and the
  wrap serves the HTTPS backend's 200, proving the mapping end-to-end.

## Phase 1 runtime notes
- Constant `BACKEND_TLS_HANDSHAKE_TIMEOUT = 10s` added near `HEARTBEAT_INTERVAL`
  in `src/vhost.rs`.
- Wrap inserted in `relay_vhost` immediately after the `provider` binding block
  and before the request head is written; gated on `entry.backend_tls`, rebinds
  `provider = Box::new(tls) as mux::LinkStream` (single-task splice preserved).
- Handshake-error mapping uses `std::io::Error::other(..)` (clippy
  `io_other_error` under `-D warnings` rejects `Error::new(ErrorKind::Other, ..)`).
- Removed `#[allow(dead_code)]` from `transport::insecure_tls_connector` /
  `backend_server_name` (now called by the relay).
- Test backend must send TLS `close_notify` (`shutdown()`), else rustls returns
  `UnexpectedEof` and the relay surfaces it as an error — noted as a separate
  robustness concern, not in scope for Phase 1.
