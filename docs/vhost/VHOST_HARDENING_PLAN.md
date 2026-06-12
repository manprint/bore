# VHOST Hardening Plan

> Status: **analysis-only** (no code changed yet). Produced 2026-06-12.
> Goal: harden the `bore vhost` reverse-proxy for **max bandwidth (UDP+TCP data
> paths), min latency, max stability/resilience** to disconnects and faults, and
> grow test coverage (critical / borderline / load, internal + e2e) the way the
> VPN feature is covered.
> Method: read `src/vhost.rs` + `tests/vhost_test.rs` in full; traced the wiring
> across `server.rs`, `client.rs`, `holepunch.rs`, `edge.rs`, `shared.rs`; every
> claim below is verified against source with `file:line`. Candidate findings
> that did **not** survive verification are listed in §3 so nobody re-chases them.

---

## 0. As-is verdict

The vhost feature is **structurally sound**. The hot paths honour the project
invariants: `STREAM_READY` is written before splice (`vhost.rs:727`),
`copy_bidirectional_with_sizes` is used for the zero-inject fast path
(`vhost.rs:740`), the inject path splits then drives both halves with `try_join!`
in **one** task (`vhost.rs:753-768`) — safe under the yamux-single-waker rule —
`tune_tcp` is applied to every accept (`server.rs:413, 452, 592`), and the
registry entry is cloned out before any `await` so no DashMap guard is held
across a yield (`vhost.rs:926, 980`; `server.rs:710`).

Three real defects remain (§1), plus six lower-confidence candidates that need a
test to confirm/refute (§2). The bigger gap is **test coverage**: there is no
load/stress coverage, no fault-injection, no e2e netns harness, and no
bandwidth/latency benchmark — which is exactly what this plan adds (§4–§6).

---

## 1. Confirmed bugs (verified), ranked by priority

### VH-1 — `--max-conns` is bypassed on the unified control port  **[HIGH]** — ✅ FIXED (2026-06-13)

> **Status: DONE.** `serve_control_http` now acquires a `conn_permits` permit
> before `relay_vhost` (503 on exhaustion). Guarded by `TEST-INT-1`
> (`vhost_max_conns_enforced_on_unified_control_port`), negative-verified (without
> the fix: `8x200 0x503`). fmt + clippy + full suite green (zero regressions).
> Field-confirmed in `vhost_netns_test_hard.sh` H6: concurrent connections under
> `--max-conns 4` dropped from **17 → 5**.


- **Where:** `server.rs:699-718` (`serve_control_http`) → `vhost::relay_vhost`
  at `server.rs:711`.
- **Mechanism (verified):** The dedicated vhost frontend listeners acquire a
  `conn_permits` permit before relaying (`server.rs:414-422` HTTP,
  `server.rs:453-461` HTTPS). The **unified** path — control port doubling as the
  vhost frontend, i.e. the *default/recommended single-443 topology* — routes
  through `route_connection` → `serve_control_http` → `relay_vhost` and **never
  touches `conn_permits`**. There is no `try_acquire_owned()` anywhere on that
  branch.
- **Impact:** In the default deployment, `--max-conns` is silently ineffective
  for all browser-facing vhost traffic. Unbounded concurrent relays →
  FD/memory/goroutine exhaustion under load or a deliberate flood; the operator's
  configured bound does nothing. Directly contradicts the invariant
  "`--max-conns` semaphore is the real bound."
- **Fix sketch:** In `serve_control_http`, after the Host→subdomain match and
  registry hit (just before `relay_vhost` at `server.rs:711`), acquire
  `Arc::clone(&self.conn_permits).try_acquire_owned()`; on `Err`, return a 503
  (or drop with a `debug!`) exactly like the frontend listeners; move the permit
  into the relay future so it is held for the connection's life. Leave the
  admin/404 fall-through unmetered (cheap, fixed-size).
- **Regression test:** see TEST-INT-1.

### VH-2 — QUIC direct-carrier churn when `vhost --carriers > 32`  **[MEDIUM]** — ✅ FIXED (2026-06-13)

> **Status: DONE** (chosen fix: local clamp, not wire negotiation). The client now
> clamps its QUIC direct-carrier target to `MAX_DIRECT_CARRIERS` via
> `vhost::clamp_direct_carriers` and warns once when clamping, so it never opens
> more than the server keeps → no open/close churn. Guarded by unit test
> `clamp_direct_carriers_caps_at_max` (the discriminator) + integration
> `vhost_udp_carriers_clamped_to_cap` (pool caps at 32, routing intact) + netns H3
> (now runs the server with `RUST_LOG=bore=debug`, so the "pool full" churn log is
> a real detector — reports clean post-fix). fmt + clippy + full suite green.
> **Deferred enhancement:** aligning the QUIC cap with the server's runtime
> `--max-carriers` (currently QUIC uses the fixed `MAX_DIRECT_CARRIERS=32`, TCP
> uses `--max-carriers`) would need the wire-negotiation option and is left as a
> separate task.


- **Where:** client target `client.rs:568` (`vhost_udp_carriers = if udp {
  carriers.max(1) } else { 0 }` — **not** clamped to any server cap) and
  `client.rs:758-776` (opens `need = target - live` each offer); server cap
  `vhost.rs:359` (`MAX_DIRECT_CARRIERS = 32`) enforced at `server.rs:550-571`
  (surplus → `direct.close()`, `debug!` only); client close→renew loop at
  `client.rs:1043-1083` (`spawn_vhost_direct`: on `accept_stream` error →
  `release()` = `live.fetch_sub` + `renew_tx.send`).
- **Mechanism (verified, traced end-to-end):** With `--carriers 40 --udp`: client
  opens 40 QUIC connections; server installs 32, **closes 8**; the 8 closes wake
  the client's `accept_stream` loops → `release()` decrements `live` and fires a
  renewal; the renewal makes the server re-offer; client recomputes
  `need = 40 - 32 = 8` and reopens 8; server closes them again → **permanent
  churn** (bounded only by the renew backoff, capped at 32 s — `client.rs:616`
  `Backoff::new_with(2, 32)`). Never converges.
- **Secondary semantic mismatch:** the **TCP** carrier pool is clamped to the
  server's `max_carriers` (default 16, `server.rs:1014`,
  `vhost.rs:591`), but the **QUIC direct** pool target is client-driven and
  capped at a *different* number (32) with **no negotiation** — so `--carriers`
  means two different things on the two data paths.
- **Impact:** wasted CPU + UDP handshake storms + log spam, indefinitely, on a
  valid (if unusual) flag value. Silent — only visible at `debug!`.
- **Fix sketch (pick one, prefer A):**
  - **A (negotiate):** advertise the effective direct cap to the provider (extend
    `ServerMessage::VhostUdp` with an `max_direct_carriers` field, or reuse the
    existing carrier-token negotiation) and have the client clamp
    `vhost_udp_target` to `min(carriers, advertised_cap)`. Mirrors how TCP
    carriers already negotiate `min(client, server max_carriers)`.
  - **B (defensive):** client tracks consecutive "established then immediately
    closed" events per offer and stops topping up once it detects the server is
    refusing the surplus.
  - Either way: align the documented meaning of `--carriers` across TCP and QUIC,
    and raise the server-side surplus drop to `warn!` once (VH-3).
- **Regression test:** see TEST-INT-9.

### VH-3 — Silent degradation: operator is blind to TCP fallback / UDP state  **[LOW–MEDIUM]**

- **Where:** admin entry hardcodes `udp: false` at `vhost.rs:570` even when the
  provider negotiated `--udp`; surplus-carrier drop is `debug!` only
  (`server.rs:568`); `direct_stream_opens` counter (`vhost.rs:705`) is incremented
  but never surfaced anywhere (no admin field, no log).
- **Impact:** When a vhost silently falls back from QUIC-direct to TCP-relay (the
  exact thing that tanks latency on a lossy link), nothing in the admin page or
  default-level logs shows it. Debugging a "why is my vhost slow" report is
  guesswork. Also masks VH-2.
- **Fix sketch:** set the admin `udp` flag from the negotiated value; expose
  `direct_carriers` (`entry.direct.len()`) and `direct_stream_opens` in
  `ServerStatus`/admin JSON; raise the VH-2 surplus drop to a one-shot `warn!`.
- **Test:** assert admin status reports `udp=true` and `direct_carriers>=1` once
  a direct path is up (extend an existing UDP integration test).

---

## 2. Candidates — need a test to confirm or refute (lower confidence)

Do not "fix" these blind. Write the test first; if it reproduces, promote to §1.

- **VH-C1 — provider death mid-request → 502 (TOCTOU).** Between cloning the
  entry (`server.rs:710` / `vhost.rs:926,980`) and `relay_vhost`, the provider can
  vanish; `entry.pool.pick()` then errors (`vhost.rs:711,716,723`) → 502.
  Functionally correct, but untested: confirm no **hang** and no permit leak when
  a provider dies under load. Test: TEST-INT-3.
- **VH-C2 — rapid `VhostUdpRenew` overwrites the pending nonce.**
  `send_vhost_udp_offer` overwrites `pending_vhost_udp[subdomain]`
  (`vhost.rs:475`); a QUIC dial still in flight with the *old* nonce then fails
  the token check (`holepunch.rs:1401-1404`) → rejected → client retries. Failure
  mode looks graceful; confirm it actually re-converges under a renew burst.
  Test: TEST-INT-10.
- **VH-C3 — request head > 16 KiB is truncated, forwarded, and the provider may
  reject it; no server log.** `read_head_async` caps at 16 KiB (`vhost.rs:991`);
  `edge::read_request_head` caps at the same 16 KiB (`edge.rs:36`). Big-cookie /
  many-header requests get a truncated head spliced through. Confirm behavior and
  decide whether to log. Test: TEST-INT-5.
- **VH-C4 — slow/partial provider can pin a `conn_permits` slot.** On the frontend
  path the permit is held for the whole `relay_vhost` (`server.rs:425,464`). If
  the provider accepts the substream but stalls forever, there is no body-level
  timeout (only `NETWORK_TIMEOUT` on the *head* read, `vhost.rs:897,961`). A
  slowloris-style provider or client could pin permits. Test: TEST-INT-4.
- **VH-C5 — keep-alive / pipelined: only the first head is injected**
  (documented MVP limit, `vhost.rs:800-801`). Once spliced, the connection is
  bound to one provider, so a second request with a different `Host` cannot reach
  a *different* tenant (no cross-tenant smuggling) — but confirm that explicitly,
  and confirm injection-on-first-only is the only consequence. Test: TEST-INT-6.
- **VH-C6 — QUIC stream exhaustion.** `open_bi` (`holepunch.rs:1013`) awaits stream
  credit; `max_direct_streams` default is **4096** (`shared.rs:141`). Above 4096
  concurrent in-flight requests on one carrier, `relay_vhost`'s `open_stream`
  **blocks** instead of falling back to TCP — a latency cliff, not a crash.
  Remote, but load-test it. Test: TEST-LOAD-2.

---

## 3. False positives — ruled out (do not re-investigate)

- **`DirectPool::pick` modulo-by-zero panic** (`vhost.rs:404-411`): the
  `is_empty()` check and the modulo both run **under the same `read()` guard**, so
  `len` cannot change between them. Not reachable.
- **`relay_response_injected` splits a `mux::Stream` "across two tasks"**
  (`vhost.rs:753-768`): the two halves are driven by `try_join!` inside **one**
  task → one waker → safe per the documented invariant. Not a wedge.
- **Empty-secret QUIC token "hijack"** (`holepunch.rs:84`): token = HMAC(secret,
  per-subdomain nonce); cross-subdomain requires the victim's nonce, which travels
  over the (ideally TLS) control channel. This is the *already-documented* weak-
  auth caveat and the client emits an explicit warning at `client.rs:358`. Known
  limitation, not a new bug.
- **HTTP vs HTTPS head-cap inconsistency:** both cap at 16 KiB
  (`edge.rs:36` `MAX_REQUEST_HEAD = 16*1024`; `vhost.rs:991` `MAX = 16*1024`).
- **Missing `tune_tcp` on the frontend:** applied at `server.rs:413, 452` and on
  the control accept at `server.rs:592`.

---

## 4. Test-coverage hardening (internal)

Model selection (per CLAUDE.md): write these with **Sonnet**; use **Haiku** only
for mechanical fixture/boilerplate generation. Escalate to Opus only if a test
keeps failing for non-obvious reasons.

### 4a. Unit tests — `src/vhost.rs` (#[cfg(test)])
Pure functions, fast, high value for borderline parsing:
- `extract_subdomain`: trailing dot (`sub.bore.local.`), label length 63 vs 64,
  multiple colons / IPv6-bracket host, uppercase base domain, non-numeric
  ":port", empty host.
- `extract_host_from_head`: **multiple `Host` headers** (pick-first must be
  deterministic — security-relevant), obsolete line-folding, `Host:` with empty
  value, absolute-form request line (`GET http://x/ HTTP/1.1`), LF-only line
  endings, no `Host` (HTTP/1.0).
- `rewrite_head`: header with no space after colon, duplicate inject names, empty
  inject value, **CRLF-injection attempt in an inject value** (header smuggling —
  must not split into two headers), request line with extra spaces, body byte
  boundary exactly at terminator, two `\r\n\r\n` (only first is the terminator).
- `resolve_route`: registry key casing vs reservation casing (registry inserts the
  raw client subdomain at `vhost.rs:526` but `resolve_route` case-folds at
  `vhost.rs:216`) — pin the intended semantics with a test.

### 4b. Integration tests — `tests/vhost_test.rs`
- **TEST-INT-1 (VH-1 regression):** server with `max_conns` small on the **unified
  control port**; register a vhost; open `max_conns + K` concurrent slow requests;
  assert the cap is actually enforced (excess dropped/503), not unbounded.
- **TEST-INT-2:** same, on the **dedicated frontend** listener (this path already
  meters — lock it in so a refactor can't regress it).
- **TEST-INT-3 (VH-C1):** start a large streamed response; abort the provider
  mid-body; assert the client sees a clean close/error within a bound (no hang)
  and the permit count returns to baseline.
- **TEST-INT-4 (VH-C4):** provider accepts the substream then never responds;
  assert the server-side connection is eventually reclaimed and does not pin a
  permit forever (drives a decision on a body/idle timeout).
- **TEST-INT-5 (VH-C3):** request with > 16 KiB of headers; assert defined
  behavior (documented truncation or rejection) — no silent stream desync.
- **TEST-INT-6 (VH-C5):** keep-alive connection, two requests; assert both reach
  the same provider and only the first head is injected; assert a different `Host`
  on the second request does **not** cross to another tenant.
- **TEST-INT-7:** HTTP/1.0 / missing-Host → 502; malformed request line → 502; no
  panic.
- **TEST-INT-8 (scale):** register ~100 subdomains concurrently, route one request
  each, assert no cross-talk and bounded registration latency.
- **TEST-INT-9 (VH-2 regression, `feature=udp`):** `--carriers 40 --udp`; assert
  the direct pool **stabilizes at 32** and that handshake/`direct_stream_opens`
  churn stops (sample the counter twice ≥1 s apart — it must plateau).
- **TEST-INT-10 (VH-C2, `feature=udp`):** fire several `VhostUdpRenew` back-to-back;
  assert the direct path re-converges and traffic still routes.
- **TEST-INT-11 (`feature=udp`):** large **request** body (POST 1 MiB) over the
  direct path (mirror the existing large-*response* test `vhost_udp_large_body_
  integrity`) — byte-exact at the origin.
- **TEST-INT-12:** the deferred **cert hot-reload** — at minimum the cert/key
  **path-swap** branch (`vhost.rs:1115-1118`); generate two wildcard certs, swap
  the path in the yaml, poll until new TLS handshakes use the new cert (compare
  served leaf DER; wire `tokio-rustls` peer-cert inspection as a dev-dep if
  needed). Closes the `vhost_cert_hot_reload` gap noted in `VHOST_TEST_MATRIX.md`.

### 4c. Load / stress (internal, `multi_thread` flavor)
- **TEST-LOAD-1:** sustained N (e.g. 500) concurrent connections to one subdomain
  for a few seconds; assert zero permit leak (final available permits == initial)
  and no error under the cap.
- **TEST-LOAD-2 (VH-C6):** drive > `max_direct_streams` concurrent requests on a
  single direct carrier; observe whether `open_stream` blocks vs falls back; record
  p99.

---

## 5. E2E netns harness (scripts/) — multi-user, real conditions

Mirror the VPN scripts' structure (binary-freshness guard, `cleanup()` trap on
`EXIT INT TERM`, `wait_for_log`, `pass`/`fail` counters, 3-namespace topology with
veth pairs). vhost is HTTP(S) over TCP / UDP-QUIC, **not** a TUN — so replace the
`ip addr show bore0` / `ping` / `iperf3` data-plane bits with an origin HTTP
server + `curl`/`hey`/`wrk` driven by `Host` header (or `--resolve`).

Topology: `ns0` = bore server (gateway); `ns1..nsK` = providers (each runs a local
origin + `bore vhost --subdomain subK`); `nsC` = browser/client(s). Client reaches
`https://subK.bore.local:PORT` via `curl --resolve subK.bore.local:PORT:<ns0-ip>`.
Reuse the VPN `nft` UDP-block helper to test fallback, and `tc qdisc … netem` for
loss/latency/bandwidth.

### `scripts/vhost_netns_test.sh` — acceptance (functional)
Cases: HTTP route; HTTPS route (self-signed wildcard `*.bore.local`, `curl -k`);
unknown subdomain → 502; duplicate subdomain rejected; reservation accept/reject
(seed `vhost.yml`); request+response header injection (assert at origin and at
client); large body integrity (download a file, compare `sha256`); concurrency (M
parallel `curl` to one sub, all 200); **multi-user** (K providers/subdomains in
parallel, each request lands on its own origin — no cross-talk); auto-reconnect
(kill+restart `bore server`, assert routing recovers); config hot-reload (rewrite
`vhost.yml`, poll until new reservation applies); UDP direct path (`--udp`, assert
"vhost QUIC direct carrier established" in logs); UDP→TCP fallback (nft-drop UDP,
assert traffic still flows over relay).

### `scripts/vhost_netns_test_hard.sh` — fault injection / hardening gate
Cases: `tc netem` loss 1–5% + RTT 20–100 ms on the provider veth (correctness
under loss); provider flap (kill/restart provider repeatedly under load, assert
recovery + no stuck 502s); **VH-2** surplus-carrier churn (`--carriers 40 --udp`,
grep logs to assert no repeating open/close storm); slowloris client (partial
head, assert server times out and reclaims); oversized headers; `SIGKILL` the
server then assert no leaked listeners/ports on the next run; **max-conns
saturation** on the unified port (VH-1 — drive > cap, assert the cap holds once
fixed).

### `scripts/vhost_debug.sh` — minimal repro
Single provider + origin in one ns pair, `bore -v`, one `curl`, dump server +
provider logs. For fast iteration (like `vpn_debug.sh`).

### sudoers
Add a `scripts/dev/bore-vhost-sudoers` analogous to the VPN one (only needs `ip`,
`nft`, `tc` — **no** TUN/ip_forward). Build as the user, run the harness with
`sudo`; reuse the VPN freshness guard so a stale binary aborts the run.

---

## 6. Benchmark + debug (UDP + TCP data paths, saturation)

### `scripts/vhost_bench.sh` — modeled on `vpn_bench.sh`
Configuration matrix (each row = a `bore vhost` provider config):

| Row        | Provider flags        | Data path                |
|------------|-----------------------|--------------------------|
| `tcp-1c`   | (default)             | TCP relay, 1 carrier     |
| `tcp-Nc`   | `--carriers 4`        | TCP relay, 4 carriers    |
| `udp-1c`   | `--udp`               | QUIC direct, 1 carrier   |
| `udp-Nc`   | `--udp --carriers 4`  | QUIC direct, 4 carriers  |

Per row, measure against `https://sub.bore.local`:
1. **Throughput / saturation** — download a large body (e.g. 256 MiB) and/or run
   `hey -z 10s -c <high>` (or `wrk -t4 -c<high> -d10s`); report sustained MB/s and
   error %. Drive concurrency high enough to **saturate the link** (that is where
   `--carriers N` and QUIC multi-stream should show their isolation win).
2. **Latency** — `hey`/`wrk` p50/p99, or `curl -w '%{time_total}'` over many small
   requests.
3. **Under impairment** — repeat the matrix with `tc netem delay 40ms loss 1%` to
   surface the documented tradeoff: tuned **TCP wins bulk throughput on a clean
   link** (kernel TSO/GSO, ~110 MB/s) while **QUIC wins on lossy/high-RTT** and on
   tail latency under concurrency (no yamux HOL, no single cwnd). Bench must make
   this visible, not assume it.

Output a markdown table (throughput | p50 | p99 | err%) per row × {clean, impaired}.
Note explicitly: a single `curl` will **not** benefit from multi-carrier/QUIC —
the bench must use concurrency.

### Debug knobs to exercise
`BORE_PROXY_BUFFER_SIZE` (default 256 KiB, clamp [4 KiB, 16 MiB]) on both server
(relay buffers) and provider (local splice); `--carriers`, `--udp`,
`--vhost-quic-port`; `RUST_LOG=bore=debug` to see fallback / carrier events
(after VH-3 lands, the key events are visible at default/`warn`).

---

## 7. Execution order (priority)

Fix and test in this order; each fix ships with its regression test and zero
tolerated regressions (`cargo fmt`, `cargo clippy -D warnings`, `cargo test`, full
suite) per CLAUDE.md.

1. **Build the e2e + bench scaffold first** (`vhost_netns_test.sh`,
   `vhost_debug.sh`, `vhost_bench.sh`) against the **as-is** binary — establishes
   the baseline and reproduces VH-1/VH-2 before touching code. (Sonnet; mechanical
   parts Haiku.)
2. **VH-1** (max-conns bypass) — highest impact, default topology, security/
   stability. + TEST-INT-1/2. (Sonnet.)
3. **VH-2** (carrier churn) — pick fix A (negotiate cap). + TEST-INT-9. (Sonnet;
   the message-schema change touches `shared.rs`/`client.rs`/`server.rs` — if it
   sprawls, escalate the design to Opus.)
4. **VH-3** (observability) — unblocks diagnosing everything else. + admin test.
5. **Candidates VH-C1…C6** — write each test (§4b/§4c); promote to a fix only if
   the test reproduces a real defect.
6. **Coverage fill** — remaining §4a unit tests, TEST-INT-11/12, `vhost_netns_
   test_hard.sh`. Update `docs/VHOST_TEST_MATRIX.md` as rows go green.

Docs are part of the deliverable: every behavior/API/invariant change updates
`docs/VHOST.md` / `VHOST_PLAN_UDP.md` and this file.
