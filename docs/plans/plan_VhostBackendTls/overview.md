# Vhost Backend TLS — Plan Overview

> **Status:** planning | **Opus authored:** 2026-07-20
> **Folder:** `docs/plans/plan_VhostBackendTls/`
> **Branch:** nat-adv

## Goal

Let a `vhost` tunnel forward to a LOCAL backend that itself speaks HTTPS/TLS
(including a self-signed certificate), for BOTH the native `bore vhost` client
and the SSH gateway (`ssh -R vhost/<label>:0:localhost:<port>`) path. Today the
vhost server terminates the browser-facing TLS, reads the plaintext HTTP `Host`
header to route the subdomain, then relays raw bytes to the provider, which
opens a plain TCP socket to the local backend — so a TLS backend receives
plaintext HTTP and resets the connection. The fix originates a TLS client
session on the SERVER side, wrapping the provider-facing `LinkStream` in a
`tokio-rustls` client before the relay splices into it. The wrapped stream
carries the (server-decrypted, rewritten) HTTP over TLS to the backend; the
native client and OpenSSH both remain blind byte pipes, so a single server-side
seam covers both transports with no data-plane change on the provider side.

End state / acceptance: a self-signed HTTPS backend is reachable through vhost
via both transports, an HTTP (plaintext) backend keeps working byte-identically,
and the flag/param default OFF leaves the entire vhost path unchanged.

```
# Native — plain HTTP backend (unchanged today)
bore vhost web localhost:3000          # http://localhost:3000
curl https://web.example.com           -> 200

# Native — HTTPS self-signed backend (NEW)
bore vhost app --backend-tls localhost:3005   # https://localhost:3005 self-signed
curl https://app.example.com           -> 200 (served from the TLS backend)

# SSH gateway — HTTPS self-signed backend (NEW)
ssh -T -p 443 -R vhost/app:0:localhost:3005 bore.example.com "backend-tls=on"
curl https://app.example.com           -> 200
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Backend TLS is originated SERVER-SIDE, by wrapping the provider `LinkStream` returned from `open_ready` in a `tokio-rustls` client `TlsStream`, in `vhost.rs`. | One seam covers native + SSH. The native client and OpenSSH stay dumb byte pipes; no provider-side change. Mirrors sish. |
| **D2** | Config is carried PER-TUNNEL: `HelloVhost` gains `backend_tls: bool` + `backend_tls_sni: Option<String>` (native); `Params` gains `backend_tls` + `backend_tls_sni` parsed from the SSH exec/env params (SSH). Both populate `VhostEntry`. | Additive `#[serde(default)]` wire fields keep old/new interop. The relay reads `entry.backend_tls`. |
| **D3** | Detection is EXPLICIT (flag/param), never auto-probe. | No heuristic guessing of backend scheme; predictable, bore-style. |
| **D4** | Certificate verification toward the backend is SKIPPED (accept any cert) when `backend_tls` is on, reusing `transport::NoVerifier`. CA pinning is OUT OF SCOPE for this plan. | Self-signed localhost backends work with no extra config. Documented as a security caveat; future `--backend-tls-ca` noted in the risk register. |
| **D5** | SNI/ServerName defaults to `"localhost"` when `backend_tls_sni` is unset; overridable. | rustls needs a `ServerName` for the ClientHello even with verification skipped; a bad name fails gracefully, never panics. |
| **D6** | `backend_tls == false` produces the EXACT current code path (no wrap). | Zero regression for every existing vhost tunnel; the HTTP-backend case is byte-identical. |
| **D7** | The TLS wrap keeps the relay a SINGLE-TASK splice (`copy_bidirectional_with_sizes` / `relay_response_injected`), never splitting the stream across tasks. | Preserves the yamux single-waker invariant (see Invariants I-3). |

## Architecture summary

Front TLS termination, `Host` routing (`extract_host_from_head`), and request/
response header injection are unchanged. Immediately after the `provider`
`LinkStream` is bound (`vhost.rs:879` block, fed by `open_ready` at 900/908/918)
and BEFORE the first `provider.write_all(request_head)` (`vhost.rs:932`), if
`entry.backend_tls` is set, the provider is replaced by
`Box::new(connector.connect(server_name, provider).await?)` (re-boxed back into a
`mux::LinkStream`). The relay then operates on the TLS-wrapped stream exactly as
before, because it is generic over `AsyncRead + AsyncWrite`.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|-----------------|
| 0 — Wire + entry + TLS-connector scaffolding (additive, no behavior) | [phase_01.md](phase_01.md) | Haiku + Sonnet | yes |
| 1 — Server-side backend-TLS wrap at the relay seam (core) | [phase_02.md](phase_02.md) | Opus review → Sonnet | yes |
| 2 — Native client plumbing (`bore vhost --backend-tls`) + docs | [phase_03.md](phase_03.md) | Sonnet + Haiku | yes |
| 3 — SSH gateway param plumbing (`backend-tls=on`) + warnings + docs | [phase_04.md](phase_04.md) | Sonnet | yes |
| 4 — Full regression, real-backend e2e/netns, final doc read | [phase_05.md](phase_05.md) | Sonnet + Opus final read | yes |

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| rustls client config that accepts any cert | `client_config(insecure)` + `NoVerifier` | `src/transport.rs:161`, `:212` |
| Build a `tokio_rustls::TlsConnector` + `ServerName` | pattern in `transport::connect` | `src/transport.rs:151`, `:152`; import `:26` |
| Provider stream type (boxed, transport-erased) | `mux::LinkStream = Box<dyn Duplex>` | `src/mux.rs:100` |
| Additive wire field pattern | `#[serde(default)]` fields on `HelloVhost` | `src/shared.rs:1173` |
| Native vhost entry build | `serve_vhost_provider` → `VhostEntry {..}` | `src/vhost.rs:581`, `:636` |
| SSH vhost entry build from `Params` | `tcpip_forward_vhost` → `VhostEntry {..}` | `src/sshgw.rs:818`, `:903` |
| Param parse pattern (mirror `https`/`notes`) | `Params` + `parse_params` | `src/sshgw.rs:2654`, `parse_params` |
| Inapplicable-param warning | `deliver_inapplicable_warnings` | `src/sshgw.rs` (locate by symbol) |
| Self-signed cert for tests | `self_signed_for`, `write_pem_files` | `tests/vhost_test.rs:172`, `:178` |
| In-process vhost harness | `http_config`, `reg_cfg_no_reservations`, `to_reg` | `tests/vhost_test.rs:156`, `:207`, `:211` |
| Real `ssh -R` e2e harness | OpenSSH CLI driver, `TestNoVerifier` | `tests/ssh_gateway_test.rs:43` |

> Line anchors are as-of-planning (branch nat-adv, 2026-07-20). Numbers drift as
> edits land across phases; if an anchor is stale, locate the symbol by name.

## Invariants

- **I-1 (zero regression):** `backend_tls == false` is byte-identical to today.
  Every existing vhost test (native + SSH) must still pass, unchanged.
- **I-2 (wire compat):** `backend_tls` / `backend_tls_sni` are additive
  `#[serde(default)]` fields. An old client omits them (decode → `false`/`None`);
  an old server ignores an unknown field only if the codec tolerates it — verify
  the round-trip test. New fields are appended, never reordered.
- **I-3 (yamux single-task):** the TLS-wrapped provider is spliced in ONE task
  (`copy_bidirectional_with_sizes` / `try_join!` shape). Never `tokio::io::split`
  the provider `LinkStream` (or its `TlsStream`) across two tasks.
- **I-4 (no hang):** the backend TLS handshake is bounded by a timeout; a
  handshake against a non-TLS or dead backend fails cleanly and closes the
  proxied connection, never pins it open (parity with the SSH open-timeout
  discipline).
- **I-5 (no panic):** an invalid `backend_tls_sni` (or handshake failure) returns
  an error and closes the connection; it never `unwrap()`/panics.
- **I-6 (SSH param hygiene, I-SSH8):** `backend-tls` on a non-vhost SSH forward
  (public/secret) is a no-op there and MUST emit an explicit warning, never be
  silently swallowed.

## Risk register

| Risk | Mitigation |
|------|-----------|
| In-process loopback TLS tests false-pass (see memory `feedback-inprocess-test-false-pass`). | Phase 1 gates at the relay level with a REAL self-signed backend; Phase 4 adds a real-subprocess/netns e2e for both transports. Red-check by reverting the wrap. |
| Boxed `LinkStream` fails to satisfy `TlsConnector::connect` bounds (`Unpin`/`Send`). | `Box<dyn Duplex>` is `Unpin + Send`; `TlsStream<Box<dyn Duplex>>` re-implements `Duplex` and re-boxes. Compile-check in Phase 1.1; documented in the sub-phase. |
| Skip-verify weakens trust for non-localhost backends. | Documented security caveat (Phase 2/3 docs). Future `--backend-tls-ca` pinning noted as deferred; not implemented here. |
| Line anchors drift across phases. | Each phase instructs locate-by-symbol if an anchor is stale; resume.md is updated after every sub-phase. |

## Implementer protocol (read before starting)

1. Execute phases in order. Within a phase, do sub-phases in order.
2. Follow existing repo folder/file conventions. Do NOT create new directories or
   files when an existing one serves the same purpose (tests go in the existing
   `tests/vhost_test.rs` / `tests/ssh_gateway_test.rs`; docs update existing
   README/`docs/` files). New files only where the phase explicitly says so.
3. Tests are mandatory, named, and must assert what the phase states.
4. Update the relevant documentation IN THE SAME PHASE (README is the single
   source of truth — see CLAUDE.md).
5. **At the end of EACH phase, once ALL gates are green (fmt, clippy -D warnings,
   the phase test subset, and the regression guard), STOP and ASK the user for
   explicit confirmation before starting the next phase.** Do not chain phases.
6. Update `resume.md` after every sub-phase (status + `Next:` pointer).

## Model-assignment summary

| Phase / sub-phase | Model | Rationale |
|-------------------|-------|-----------|
| 0.1 HelloVhost fields | Haiku | Mechanical struct/serde field add. |
| 0.2 VhostEntry fields | Sonnet | Touches multiple construction sites; must stay consistent. |
| 0.3 `insecure_tls_connector` helper | Sonnet | Small but correctness-sensitive TLS config. |
| 1.1 Relay TLS wrap | Opus design review → Sonnet | Hot-path relay, yamux invariant, no-hang/no-panic. |
| 1.2 Timeout + graceful errors | Sonnet | Bounded handshake, error mapping. |
| 2.1 CLI flags | Haiku | Flag parsing boilerplate. |
| 2.2 Client → HelloVhost | Sonnet | Thread option through provider construction. |
| 2.3 Server map → VhostEntry | Sonnet | Wire→entry mapping. |
| 2.4 Docs (README/vhost) | Haiku | Doc prose. |
| 3.1 Params + parse | Sonnet | Param parsing, mirror existing pattern. |
| 3.2 Params → VhostEntry | Sonnet | Mapping. |
| 3.3 Inapplicable warnings | Sonnet | I-SSH8 correctness. |
| 3.4 Banner line | Haiku | Cosmetic status text. |
| 3.5 Docs (SSH gateway) | Haiku | Doc prose. |
| 4.1 Real-backend e2e (both transports) | Sonnet | Test authoring, real network. |
| 4.2 netns scenarios | Sonnet | Harness scripting. |
| 4.3 Full regression + final doc read | Opus | Acceptance verification. |
