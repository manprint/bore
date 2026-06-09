# Vhost Audit — severe, wide-range review

Branch `vhost` (rebased on `transfers`). Scope: `src/vhost.rs` (+1089) and integration
in `server.rs`, `client.rs`, `shared.rs`, `main.rs`, `edge.rs`, `admin.rs`, `secret.rs`.
Baseline: `cargo fmt --check` clean, `cargo clippy --all-targets` zero warnings,
all 16 `vhost_test.rs` + full suite (267 tests) green.

Method: full read of vhost.rs + every integration hunk; auth flow traced end-to-end;
two findings reproduced with standalone PoCs (F1, F3); race/leak/deadlock pass.

Verdict (original): **not ship-ready.** Two HIGH correctness bugs break core features
(header injection corrupts bodies; config ports are dead). Routing, hot-reload, logging,
and the bandwidth story all need work before this matches the rest of the app.

---

## Resolution — APPLIED (all findings fixed)

All findings below are fixed in the working tree. Gate: `cargo fmt --check` clean,
`cargo clippy --all-targets -- -D warnings` clean, full suite green
(154 lib + integration + 18 vhost E2E + doctest), stable across repeated parallel runs.

| ID | Status | Where | Test |
|----|--------|-------|------|
| F1 | fixed | `vhost.rs::rewrite_head` preserves over-read body, byte-based | `rewrite_head_preserves_request_body`, `vhost_post_body_preserved_with_inject` (E2E) |
| F2 | fixed | `main.rs` port flags `Option<u16>`, override only when `Some` | `server_vhost_port_flags_default_to_none/parse_when_present` |
| F3 | fixed | `main.rs::parse_vhost_target` accepts hostnames + `:port` + IPv6 | `parse_vhost_target_accepts_*/rejects_malformed` |
| F4 | fixed | `handle_https` routes via `extract_subdomain(host, base_domain)`; `extract_subdomain_from_registry` deleted | `vhost_https_rejects_foreign_base_domain` (E2E) |
| F5 | fixed | reload forces TLS swap on cert/key **path** change | covered by `vhost_config_hot_reload`; cert-DER inspection deferred (see file note) |
| F6 | fixed (documented + warned) | reload `warn!`s on mode/port change; `docs/VHOST.md` says restart-only | — |
| F7 | fixed | no-terminator head returned unchanged; cap 8→16 KiB | `rewrite_head_without_terminator_is_returned_unchanged` |
| F8 | fixed | single `cert_present(cfg)` helper used everywhere | existing mode tests |
| F9 | fixed | `debug!` on every 502 (no subdomain / no provider) | — |
| F10 | fixed | `debug!` on max-conns connection drop | — |
| F11 | fixed | `handle_http`/`https` single registry lookup; `relay_vhost(entry)` | — |
| F12 | unchanged | no-acceptor branch keeps its `warn!` (rare; HTTPS listener implies a cert) | — |
| P2 | fixed | head scan only the new tail (no O(n²)) | — |
| P1 | documented | `--carriers N` raises relay throughput; default kept at 1 (preserves the byte-for-byte single-connection path). Auto-scale left as a product decision | — |

Also fixed (pre-existing, surfaced by the new tests): `transfer.rs` `receiver_ask_confirm_*`
tests raced on the global `BORE_TEST_CONFIRM_RESPONSE` env var → serialized with a
`CONFIRM_ENV_GUARD` mutex (poison-tolerant). Suite is now deterministic under parallelism.

### Follow-up — env/Docker configurability (F13)

Reported gap: vhost was the only feature not configurable via env vars (the rest of bore
is fully env-driven), and the server compose had no vhost wiring.

- `bore server` now enables vhost from **either** `--vhost-config` **or**
  `--vhost-base-domain` (`BORE_VHOST_BASE_DOMAIN`). A yaml file is needed only for
  `reservations` / `default_headers`; base domain, mode, ports, and cert/key are all
  env/flag-configurable. New flags: `--vhost-base-domain`, `--vhost-cert-file`
  (`BORE_VHOST_CERT_FILE`), `--vhost-key-file` (`BORE_VHOST_KEY_FILE`). Flags/env override
  the file when both are set (`main.rs` dispatch).
- `bore vhost --basic-auth` gained `BORE_BASIC_AUTH` (parity with `bore local`).
- `docker/docker-compose.server.yml`: vhost env block + frontend ports (80/443) + cert
  volume mount, all documented. `docker-compose.client.yml`: `bore vhost` example.
- Tests: `server_vhost_config_via_cli_flags`, `server_vhost_base_domain_via_env`.

What is **clean** (verified, not assumptions):
- Auth: `HelloVhost` runs through the same central `auth.server_handshake` in
  `handle_connection` (server.rs:484) **before** dispatch. No auth bypass.
- Registry races: DashMap `entry()` insert is atomic; `Deregister`/`TokenGuard`/
  `_admin_reg` all clean up on drop; no DashMap guard is held across an `.await`
  (`relay_vhost` clones the pool out first, vhost.rs:483). No leak, no deadlock.
- Data path: client `handle_connection` (client.rs:730) reads `STREAM_READY`, optional
  basic-auth gate, splices with `copy_bidirectional_with_sizes` + `PROXY_BUFFER_SIZE` —
  identical to tunnel/secret. `tune_tcp` applied to frontend accepts (server.rs) and
  local dials (`connect_with_timeout`, client.rs:1121). Heartbeat 500ms matches secret.

---

## HIGH

### F1 — `rewrite_head` drops the request body on the header-injection path  *(PROVEN)*
**Where:** `src/vhost.rs:521-565` (`rewrite_head`), reached from `relay_vhost` (vhost.rs:493-502).
**What:** Head readers (`edge::read_request_head`, edge.rs:226; `read_head_async`,
vhost.rs:690) read in 512-byte chunks and stop when `\r\n\r\n` appears *anywhere* in the
buffer — so the buffer routinely contains body bytes that arrived in the same TCP segment.
`rewrite_head` splits on `\r\n`, copies header lines until the blank line, then `break`s
and appends the injected headers. **Everything after the blank line (the already-read body)
is silently discarded.**
**Impact:** Any `POST`/`PUT`/`PATCH` to a subdomain that has inject headers configured
(per-subdomain or `default_headers`) loses its body. `Content-Length` then mismatches the
delivered bytes → upstream hangs or mis-frames the next request. The no-inject path
(vhost.rs:498-501) forwards `head` verbatim and is correct, so the bug is specific to
injection — the feature's headline use case.
**Proof:**
```
input : POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello
output: POST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\nX-Inj: 1\r\n\r\n
body 'hello' preserved? false
```
**Fix:** Split the head once into `(headers_region, rest)` at the first `\r\n\r\n`.
Rewrite only `headers_region`; re-append the terminator **and `rest` verbatim**. If no
`\r\n\r\n` is present (truncated head, see F7) do not rewrite — splice raw. Operate on
bytes, not `from_utf8_lossy`, to avoid mangling non-ASCII header values.
**Test:** POST with a body in the same segment + inject configured; assert the capturing
stub receives the full body and the injected headers. (Current `vhost_header_injection`
only does `GET /` with no body — blind to this.)

### F2 — CLI port defaults clobber `vhost.yml` ports  *(PROVEN by code)*
**Where:** `src/main.rs:1161-1163` — `cfg.http_port = vhost_http_port;` /
`cfg.https_port = vhost_https_port;` run unconditionally; the flags are
`default_value_t = 80` / `443` (main.rs ~462-486).
**What:** When the user sets `http_port: 8080` in `vhost.yml` and does **not** pass
`--vhost-http-port`, the flag's default `80` overwrites the parsed `8080`. The YAML
`http_port`/`https_port` fields are effectively dead unless they happen to equal the CLI
default. Directly contradicts `parse_config` (which reads them) and its tests.
**Impact:** Server binds the wrong frontend port vs. the documented config; silent
misconfiguration. Stated goal "log chiari" — there is not even a warning.
**Fix:** Make both flags `Option<u16>`; override only when `Some`:
`if let Some(p) = vhost_http_port { cfg.http_port = p; }`. Same for mode (already
`Option`, fine).
**Test:** Start server with YAML `http_port: <N>`, no CLI flag; assert it binds `<N>`.

---

## MED-HIGH

### F3 — `bore vhost` rejects hostnames and the `:port` shorthand  *(PROVEN)*
**Where:** `src/main.rs:1341-1349` (`parse_vhost_target`) — parses target as `SocketAddr`,
which requires a literal IP.
**Proof:** `:8080 → Err`, `localhost:8080 → Err`, `127.0.0.1:8080 → Ok`. The docstring
normalizes `:8080` to `localhost:8080`, which then fails to parse — so the documented
shorthand is broken, and any hostname target is rejected.
**Impact:** Discrepancy with `local`/`proxy`/`transfer`, which accept hostnames; the
downstream `Client::new_vhost_provider` already takes `local_host: &str` and resolves via
`transport::connect`, so the restriction is purely an over-strict parser.
**Fix:** Parse like the other subcommands: split host:port, keep host as a `String`
(don't require an IP), default host to `localhost` for the `:port` form. Reuse the
existing local-target parsing helper rather than `SocketAddr`.
**Test:** unit test `parse_vhost_target(":8080")`, `("localhost:8080")`, `("127.0.0.1:8080")`.

---

## MED

### F4 — HTTPS routing diverges from HTTP (no base-domain check, O(n) scan)
**Where:** `src/vhost.rs:622-687`. `handle_https` routes via
`extract_subdomain_from_registry`, which ignores `base_domain` and linear-scans the
registry, matching any `Host` whose first label equals a registered subdomain.
**What vs. HTTP:** `handle_http` uses `extract_subdomain(host, base_domain)` — strict
suffix + single-label + charset validation. HTTPS:
- accepts a forged `Host: myapp.evil.com` (routes to `myapp` provider) — **base domain
  not pinned**;
- accepts nested labels `app.foo.bore.example.com` that HTTP rejects;
- is O(n) per connection and locks DashMap shards while iterating (`registry.iter()`),
  vs. HTTP's O(1) `get`. Contention + latency grow with subdomain count.
**Impact:** Host-header confusion / inconsistent routing contract between schemes;
per-connection cost scales with the number of live providers. The wildcard TLS cert does
not constrain the `Host` header, so the divergence is reachable.
**Fix:** Pass `&Option<SharedVhostConfig>` into `handle_https` (as `handle_http` already
gets it) and route through the same `extract_subdomain(host, base_domain)` + O(1)
`registry.get`. Delete `extract_subdomain_from_registry`. Single routing function for
both schemes.
**Test:** HTTPS request with `Host: <sub>.evil.com` → 502; nested label → 502; correct
host → 200.

### F5 — Hot-reload misses cert/key **path** changes
**Where:** `src/vhost.rs:758-777`. On `vhost.yml` change it reloads config, sets
`cert_mtime`/`key_mtime` to the **new** paths' current mtimes, then `continue`s. Next tick
compares the new path's mtime to that just-stored value → equal → the cert-reload branch
(vhost.rs:783) never fires.
**Impact:** Repointing `cert_file`/`key_file` in `vhost.yml` to a *different existing*
certificate does not swap the live TLS acceptor — the server keeps serving the old cert
until that file's contents change. Silent.
**Fix:** When the config reload changes `cert_file`/`key_file` paths, reload TLS
immediately (or set the stored mtimes to `None` so the next tick detects a change). Log
the swap.
**Test:** reload task with two valid certs A→B by path; assert acceptor serves B.

### F6 — Frontend listeners are bound once at startup; runtime mode/cert changes are ignored
**Where:** `src/server.rs:257-339`. `mode` is resolved once and the HTTP/HTTPS listeners
are spawned from it. The reload task can swap the TLS acceptor and change config mode, but
**no listener is added or removed at runtime.**
**Impact:** Start with `mode: auto` and no cert → HTTP-only listener. Add a cert later →
reload loads the acceptor, but there is no HTTPS listener bound, so HTTPS never serves.
The TLS hot-swap (F5's sibling) is dead in this case. Conversely, removing a cert leaves
the HTTPS listener up with a stale acceptor.
**Fix (min):** document that mode/port changes need a restart, and `warn!` in the reload
task when a config change implies a listener set the running process can't honor.
**Fix (full):** manage listeners dynamically (bind/drop on mode change). Min fix is
acceptable for now if clearly documented.

---

## LOW-MED

### F7 — 8 KiB head cap corrupts the inject path for oversize heads
**Where:** `read_head_async` (vhost.rs:690-706) and `edge::read_request_head`
(edge.rs:226-240) break at `MAX = 8 KiB` without finding `\r\n\r\n`.
**Impact:** Large heads (many cookies / long auth headers) on the **inject** path →
`rewrite_head` sees no blank line (`found_end = false`), emits no terminator, appends
injected headers after a partial line, and the leftover header bytes splice raw afterward
→ malformed upstream request. No `431` is returned. Pure-splice path is unaffected
(bytes pass through intact).
**Fix:** Tie to F1 — if no `\r\n\r\n` was found, never rewrite (splice raw), or return
`431 Request Header Fields Too Large`. Consider raising the cap to 16 KiB to match common
servers.
**Test:** request with a >8 KiB head + inject configured.

### F8 — `serve_vhost_provider` cert-present check ignores `key_file`
**Where:** `src/vhost.rs:402` — `resolve_mode(&cfg, cfg.cert_file.is_some())`. Server uses
`cfg.cert_file.is_some() && cfg.key_file.is_some()` (server.rs:217, 262).
**Impact:** With `cert_file` set but `key_file` missing, the server binds HTTP-only, but
`serve_vhost_provider` computes `Both` and advertises an `https://` URL nothing serves.
Wrong URL printed to the user.
**Fix:** Use the same `cert_file.is_some() && key_file.is_some()` predicate. Factor a
single `cert_present(cfg)` helper and call it everywhere.

---

## LOW — logging & cleanliness (stated goal: "log chiari e uniformi")

### F9 — Routing failures are invisible at default log level
- `server.rs` wraps `handle_http`/`handle_https` errors at `trace!` ("vhost http
  connection closed"); `send_bad_gateway` (vhost.rs:709) logs nothing. So every 502
  (unknown subdomain, provider unavailable) is silent at the default level, while
  registration uses `info!`/`warn!`. Non-uniform.
- **Fix:** `debug!` (or `info!` once-per-failure-class) on 502 with `%sub` and reason;
  keep successful splices quiet. Align levels with the tunnel/secret edge.

### F10 — `conn_permits` exhaustion silently drops the connection
**Where:** `server.rs` frontend accept loops — `try_acquire_owned() Err(_) => continue`.
The accepted socket is dropped with no log and no `503`; the visitor sees a reset.
**Fix:** `trace!`/`debug!` the drop; optionally write a minimal `503` before closing.

### F11 — `handle_http` does three registry lookups + a TOCTOU window
**Where:** vhost.rs:606-615 — `get` (headers) + `contains_key` + then `relay_vhost`'s
`get`. If the provider deregisters between `contains_key` and `relay_vhost`, the visitor
gets a connection reset instead of a clean 502 (inconsistent with the explicit 502 paths).
**Fix:** Single `registry.get(&sub)` → clone `(pool, headers)` once; pass into a
`relay`-style call. Removes the redundant lookups and the TOCTOU.

### F12 — `handle_https` no-acceptor branch drops silently
**Where:** vhost.rs:632-636 — when the registry routed but `vhost_tls` is `None`, it
`warn!`s and returns `Ok(())` with no response. Rare (HTTPS listener implies a cert at
start) but inconsistent with the 502 contract.

---

## PERF — "massime performance di trasferimento e sfruttamento della banda"

### P1 — vhost defaults to a single carrier → one cwnd, head-of-line blocking
**Where:** CLI `--carriers default_value_t = 1` (main.rs). Every proxied HTTP(S)
connection becomes a yamux substream over **one** TCP carrier. All concurrent transfers
share that one connection's congestion window and suffer yamux HOL blocking — the
throughput ceiling for the relay path.
**Context:** `transfer` defaults `--carriers 0` (auto) and scales the carrier pool;
`local`/`proxy` default 1. Given the explicit bandwidth priority, vhost should not be
stuck at 1.
**Recommendation:** Support `--carriers 0` (auto) for vhost and scale the pool (cap at
server `--max-carriers`), or at minimum document `--carriers N` prominently. Verify the
carrier pool round-robins vhost substreams the same way it does for secret (the plumbing
is already present: `CarrierToken`/`pending_carriers`/`CarrierPool`).

### P2 — O(n²) head scan
**Where:** `buf.windows(4).any(|w| w == b"\r\n\r\n")` re-scans the whole accumulator on
every read (vhost.rs:701, edge.rs:235). Trivial at 8 KiB but wasteful.
**Fix:** Scan only the new tail (last `n+3` bytes) each iteration.

---

## Test gaps (add alongside the fixes)

| Gap | Catches |
|-----|---------|
| POST with body in first segment + inject headers | F1 |
| YAML `http_port`/`https_port` honored with no CLI flag | F2 |
| `parse_vhost_target` unit: `:8080`, `localhost:8080`, IP | F3 |
| HTTPS routing: foreign base domain → 502, nested label → 502 | F4 |
| Reload: cert path A→B swaps acceptor | F5 |
| Oversize (>8 KiB) head + inject | F7 |
| Header-injection test asserts body pass-through + non-injected headers preserved | F1 |

Existing 16 tests are well-structured (per-test unique ports, real provider→server→stub
E2E, hot-reload, concurrency smoke, large-body download). The gaps above are the holes a
"serious" suite must close. None of the current tests exercise the inject path with a
body, which is exactly why F1 shipped.

---

## Suggested fix order (for Sonnet)
1. **F1 + F7** together (head/body handling) — highest correctness impact, one code area.
2. **F2 + F3** (CLI: option-ize ports, fix target parse) — quick, high user impact.
3. **F4** (unify HTTPS routing through `extract_subdomain` + config) — correctness + perf.
4. **F5 + F6** (reload cert paths; document/limit runtime mode changes).
5. **F8** (cert_present helper) — fold into F4/F5 cleanup.
6. **F9–F12** (logging/cleanup pass) — uniform levels, single registry lookup.
7. **P1** (carrier auto-scale for bandwidth) — design decision; confirm pool round-robin first.

Each sub-phase: tests first/alongside, then `cargo fmt && cargo clippy -- -D warnings &&
cargo test`, zero regressions.
