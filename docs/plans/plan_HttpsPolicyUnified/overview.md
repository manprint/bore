# Unified per-tunnel HTTPS policy — Plan Overview

> **Status:** planning | **Opus authored:** 2026-07-05
> **Folder:** `docs/plans/plan_HttpsPolicyUnified/`
> **Branch:** `ssh`

## Goal

Give every tunnel a single, uniform per-tunnel HTTPS control — `off | on | redirect`
— applied identically to PUBLIC tunnels (`bore local`) and VHOST tunnels
(`bore vhost`), and reachable from both the native `bore` client and the
SSH gateway (`ssh -R`/`-L`). When absent, behavior is byte-identical to today
(server default governs). Client-sent policy overrides the server default but is
bounded by server capability: if the client asks for `on`/`redirect` and the
server cannot serve HTTPS, the server logs a warning, tells the client
(non-fatally), and falls back to plain HTTP — never rejecting the tunnel and
never failing to start. Raw (non-HTTP, non-TLS) protocols on public tunnels
(Postgres, MySQL, MongoDB, …) keep working unchanged under every policy. Also
resolves two loose ends: the native public path currently HARD-REJECTS
`https` without a cert (G1), and the `PTY allocation request failed` message is
undocumented (G7).

```
# Reference scenario (acceptance):
# Server: bore server --cert-file apex.pem --key-file apex.key \
#         --vhost-config vhost.yml   # vhost.yml has wildcard *.bore.dom.xyz cert, mode=both

# 1. PUBLIC, redirect:    bore local 5000 --to bore.dom.xyz --https redirect
#    -> http://bore.dom.xyz:PORT  answers 308 -> https://... ; raw TCP still passes.
# 2. PUBLIC, on, NO cert: bore local 5000 --to nocertsrv --https on
#    -> tunnel UP, serves HTTP, client prints "server has no TLS certificate;
#       falling back to HTTP", admin shows https=false. NOT rejected.  (G1)
# 3. VHOST, redirect:     bore vhost --subdomain app --id app --to bore.dom.xyz --https redirect
#    -> http://app.bore.dom.xyz answers 308 -> https://app.bore.dom.xyz (only this sub).
# 4. VHOST, off, server mode=redirect-https:
#    bore vhost --subdomain raw --id raw --https off
#    -> http://raw.bore.dom.xyz served plain, NO redirect (opt-out).
# 5. SSH vhost redirect:  ssh -T -p 443 -R vhost/app:0:localhost:5000 \
#                         -o SetEnv="bore=https=redirect" bore.dom.xyz
#    -> same as #3; banner reports "HTTPS policy: redirect (active)".
# 6. autossh -M0 -T -R vhost/x:0:localhost:5000 bore.dom.xyz
#    -> NO "PTY allocation request failed" message.  (G7)
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | One shared enum `HttpsPolicy { Off, On, Redirect }` in `src/shared.rs`, `#[derive(clap::ValueEnum, Serialize, Deserialize)]`, `#[serde(rename_all="lowercase")]`. `Option<HttpsPolicy>` where `None` = inherit server default. | Single source of truth for CLI + wire + resolver. Mirror `clap::ValueEnum` pattern at `transfer.rs:52-72`. |
| **D2** | Policy semantics identical for public & vhost: `off`=no TLS terminate + no redirect + HTTP served; `on`=TLS on, serve HTTP **and** HTTPS, no redirect; `redirect`=TLS on + 308 HTTP→HTTPS. Raw passthrough always preserved on public. | Public maps to existing `(https, force_https)` bools: `off=(F,F)`, `on=(T,F)`, `redirect=(T,T)`. Vhost applies the same triplet against the shared :80/:443 listeners. |
| **D3** | Capability-bounded resolution. Public capability = `self.tls.is_some()` (`server.rs:195`, from `--cert-file`/`--key-file`). Vhost capability = `vhost_mode.serves_https()` **and** vhost cert present (`vhost_tls`, `server.rs:227`, `vhost.rs:1486`). Public and vhost certs are **separate**. | Resolver takes `(requested: HttpsPolicy, capable: bool)` and returns `(effective_https, effective_force_https, downgraded: bool)`. `downgraded` drives the warning. |
| **D4** | On downgrade (client asked `on`/`redirect`, server incapable): **warn + fallback to HTTP**, never reject, never fail startup. | Fixes G1 (`server.rs:1723` reject → warn+fallback on the policy path). |
| **D5** | New non-fatal `ServerMessage::Warning(String)` variant, **appended LAST** (`shared.rs`, same wire-compat rule as `ClientMessage::Heartbeat`). Client prints it via `warn!` and CONTINUES. Sent **only** when the client sent `https_policy = Some(_)` (policy-aware client). | Old clients never receive it (they send `None`), so zero wire risk. Legacy no-cert path (policy `None`) keeps today's fatal `Error` byte-identical. |
| **D6** | Wire is additive: `https_policy: Option<HttpsPolicy>` added to BOTH `TunnelOptions` (`shared.rs:281`) and `HelloVhost` (`shared.rs:934`), each `#[serde(default)]`. Public keeps its legacy `https`/`force_https` bools too (new client sets both; new server prefers `https_policy` when `Some`). | Interop: old client omits field ⇒ `None` ⇒ legacy path. Old server ignores field ⇒ reads bools. `presence of Some` = "new client" detector for D5. |
| **D7** | Vhost redirect becomes **per-subdomain**: the decision moves from the global `mode.redirects_http()` gate (`vhost.rs:1212`) to a per-entry effective policy resolved AFTER subdomain lookup. When the entry policy is `None`, the effective policy equals the global `VhostMode` (byte-identical to today). | Requires the vhost provider registry entry (router-visible) + the admin entry to carry the policy. Hot-path router change → Opus review gate. |
| **D8** | Native CLI: `--https [off\|on\|redirect]` (`Option<HttpsPolicy>`, `num_args=0..=1`, `default_missing_value="on"`) on BOTH `bore local` and `bore vhost`. `--force-https` (public) kept as a DEPRECATED bool alias → forces `Redirect` + one-time deprecation `warn!`. | Bare `--https` still = `on` (back-compat with today's bool). `bore local --https --force-https` still = redirect. No `default_missing_value` exists yet in the repo — introduces the clap idiom. |
| **D9** | SSH gateway keeps its existing string param syntax (`https=on\|off`, `force-https=on\|off`, `parse value=="on"` at `sshgw.rs:2628-2629`). Vhost SSH STOPS warning "not applicable" and APPLIES the policy via the same D7 path. | **Behavior change**: `t_ssh_warn_https_inapplicable_to_vhost` must be rewritten to assert application, not a warning. Called out loudly in phase_05. SSH warnings/notices stay on the session channel (`ConnState::deliver`), NOT `ServerMessage`. |
| **D10** | Vhost `off` on the shared :443 listener: a request that still arrives over HTTPS for an `off` subdomain is **served over HTTPS anyway** (TLS is terminated globally before the subdomain is known — cannot be un-terminated per-subdomain). `off` means "HTTP served + never force-redirect", not "refuse HTTPS". | Documented honest limitation. Closing/421 rejected as worse UX. Public `off` (dedicated port) truly does not terminate TLS. |
| **D11** | PTY: no code change to `pty_request` (`sshgw.rs:1460` `channel_failure` is correct — the gateway is not a shell). Docs only + optional one-line banner hint. | `-T` silences the client-side PTY message; `-N` remains discouraged (I-SSH7, kills the banner). |

## Architecture summary

A single `HttpsPolicy` enum flows: CLI/SSH-param → wire (`Option<HttpsPolicy>` on
`TunnelOptions`/`HelloVhost`) → server-side capability-bounded resolver →
effective `(https, force_https)` bools consumed by the unchanged `edge.rs`
(public) and by a per-subdomain redirect gate in the vhost router. On downgrade,
a new non-fatal `ServerMessage::Warning` reaches policy-aware clients; SSH clients
get the same via the session channel. `None` policy = today's behavior, proven
byte-identical by regression tests.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|-----------------|
| 0 — Scaffolding: enum, resolver, wire fields, Warning variant | [phase_01.md](phase_01.md) | Opus review → Sonnet | yes (pure additive) |
| 1 — Public path: apply policy + resilience (G1) | [phase_02.md](phase_02.md) | Opus review → Sonnet | yes |
| 2 — Vhost wire + per-subdomain router (G2, G5) | [phase_03.md](phase_03.md) | Opus review → Sonnet | yes |
| 3 — Native CLI unification (G3) | [phase_04.md](phase_04.md) | Sonnet | yes |
| 4 — SSH gateway apply (G4) | [phase_05.md](phase_05.md) | Sonnet | yes |
| 5 — Startup param-applicability logging (G6) | [phase_06.md](phase_06.md) | Haiku | yes |
| 6 — PTY documentation (G7) | [phase_07.md](phase_07.md) | Haiku | yes |
| 7 — Final e2e, docs, coherence review | [phase_08.md](phase_08.md) | Sonnet + Haiku, Opus final | yes |

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| clap `ValueEnum` derive pattern | `CollisionPolicy`/`SymlinkMode` enums | `src/transfer.rs:52-72` |
| Public edge inspection (TLS/redirect/raw) | `edge::accept` | `src/edge.rs:122-179` |
| 308 redirect writer | `edge::redirect_to_https` | `src/edge.rs:200-223` |
| Public TLS acceptor (capability) | `Server.tls` + `set_tls` | `src/server.rs:195,437`; `src/main.rs:1993` |
| Vhost TLS acceptor (capability) | `Server.vhost_tls` | `src/server.rs:227`; `src/vhost.rs:1486` |
| Vhost mode resolve + serves_https | `resolve_mode`, `VhostMode::serves_https` | `src/vhost.rs:263-293,134` |
| Public admin register | `admin.register(NewEntry{..})` | `src/server.rs:1774-1808` |
| Vhost admin register (hardcoded false) | `NewEntry{https:false,force_https:false}` | `src/vhost.rs:626-658` (633-634) |
| Vhost router redirect gate | `mode.redirects_http()` → redirect | `src/vhost.rs:1212-1215` |
| Vhost subdomain extraction (router) | Host parse | `src/vhost.rs:1222-1275` |
| Public wire struct | `TunnelOptions` | `src/shared.rs:281-325` |
| Vhost wire struct | `HelloVhost` | `src/shared.rs:934-966` |
| ServerMessage enum | variants incl. `Error(String)` | `src/shared.rs:1080+` |
| Native client sends HelloVhost | `ClientMessage::HelloVhost{..}` | `src/client.rs:570-581` |
| Native local builds TunnelOptions | struct build | `src/main.rs:1541-1553` |
| `--vhost-mode` clap+parse (mirror) | `Option<String>` + match | `src/main.rs:611-612,2068-2084` |
| SSH Params struct + parse | `Params`, `value=="on"` | `src/sshgw.rs:2504-2525,2628-2629` |
| SSH vhost register (hardcoded false) | `NewEntry{https:false,..}` | `src/sshgw.rs:769-801` (776-777) |
| SSH vhost inapplicable warn (remove) | `deliver_inapplicable_warnings` | `src/sshgw.rs:803-813` |
| SSH banners | `vhost_info_banner`/`public_info_banner` | `src/sshgw.rs:2769-2796,2812-2828` |
| SSH channel-side notice delivery | `ConnState::deliver` | `src/sshgw.rs` (I-SSH7/8 pattern) |
| Public in-process test harness | `self_signed`, server+Client setup | `tests/tls_test.rs:64-94,332-393` |
| Vhost in-process test harness | `spawn_server_vhost` | `tests/vhost_test.rs:143-150,273+` |
| SSH gateway test harness | `start_gateway_server[_vhost]` | `tests/ssh_gateway_test.rs:306-370,1385+` |
| netns e2e harness | helpers + T-SSH-* | `scripts/ssh_gateway_test.sh` |

## Invariants

- **I-1:** Wire is additive only. New fields `#[serde(default)]`; `ServerMessage::Warning` appended LAST. Old↔new client/server interop preserved.
- **I-2:** `https_policy = None` ⇒ behavior byte-identical to today (public raw fast-path at `edge.rs:136-137`; vhost global `VhostMode`; legacy `Error` on public no-cert). Proven by a regression test per family.
- **I-3:** Server never rejects a tunnel and never fails startup because of a per-tunnel HTTPS request it cannot satisfy — it warns and falls back (D4).
- **I-4:** Raw (non-TLS, non-HTTP) traffic on a public tunnel is forwarded byte-for-byte under every policy (Postgres/MySQL/Mongo). Test: policy × raw matrix.
- **I-5:** SSH leg stays TCP-relay-only (no UDP/carriers/hole-punch). Unchanged. SSH notices use the session channel, not `ServerMessage`.
- **I-6:** `ServerMessage::Warning` is sent ONLY to policy-aware clients (`https_policy.is_some()`); old clients never receive it. It is sent AFTER the readiness/carrier handshake so only the client's main control loop (`client.rs:981`) consumes it (warn+continue); one-shot registration reads bail on it by design.
- **I-7:** One logical tunnel = one admin row; zombie-reaper/heartbeat behavior unchanged. `carriers<=1` path immutato.
- **I-8:** `pty_request` unchanged (stays `channel_failure`); PTY handling is docs-only.

## Risk register

| Risk | Mitigation |
|------|-----------|
| Adding `ServerMessage::Warning` breaks old clients | Gated send (I-6/D5): only policy-aware clients receive it; appended LAST. Phase 0 verifies the codec (serde_json/bincode) tolerates a trailing variant. Fallback if deemed risky: server-log-only + admin display (documented alternative in phase_01). |
| Per-subdomain redirect reorder (D7) regresses global redirect-https | `None` policy path keeps exact global behavior; regression test `vhost_redirect_mode` must still pass unchanged. Opus review gate on the router change (phase_03 §3.2). |
| `--force-https` deprecation changes existing scripts | Kept as working alias → `Redirect` + one deprecation `warn!`; old invocations produce identical effective behavior. Regression test asserts equivalence (phase_04). |
| SSH vhost `t_ssh_warn_https_inapplicable_to_vhost` semantics flip | Loud behavior-change callout in phase_05; test rewritten to assert application; docs updated. |
| Public capability uses `--cert-file` cert, not vhost cert (user confusion) | Documented (D3): public `--https` needs server `--cert-file`; vhost needs vhost cert. Warning message names the exact missing flag. |

## Model-assignment summary

| Phase | Sub-phases | Model(s) | Opus review gate |
|-------|-----------|----------|------------------|
| 0 | 0.1 enum · 0.2 resolver · 0.3 Warning variant · 0.4 wire fields | Sonnet (0.1,0.2,0.4), Sonnet+Opus review (0.3) | 0.3 (wire) |
| 1 | 1.1 resolve+apply public · 1.2 reject→warn+fallback · 1.3 tests | Sonnet | 1.1/1.2 (hot path `server.rs`) |
| 2 | 2.1 registry+admin policy · 2.2 per-subdomain router · 2.3 fallback+warn · 2.4 tests | Sonnet | 2.2 (hot-path router) |
| 3 | 3.1 CLI flag · 3.2 wire-in · 3.3 force-https deprecation · 3.4 tests | Sonnet | — |
| 4 | 4.1 vhost apply · 4.2 banner · 4.3 tests | Sonnet | 4.1 (I-SSH8 flip) |
| 5 | 5.1 param logging · 5.2 matrix doc | Haiku | — |
| 6 | 6.1 PTY docs · 6.2 banner hint | Haiku | — |
| 7 | 7.1 netns e2e · 7.2 docs · 7.3 coherence review | Sonnet + Haiku | 7.3 (final read) |
