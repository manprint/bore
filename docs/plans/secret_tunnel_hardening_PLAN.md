# Secret Tunnel — Bug-Fix & Hardening Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture + verification gate; recon via 4× Sonnet hunters + 1× Haiku explorer).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk, Sonnet = features/refactor/tests, Opus = review gates only).
> **Target:** correctness + resilience of secret tunnels (provider/consumer, relay + direct UDP, `--carriers`, `--auto-reconnect`) under real load and random kill/restart; **zero spurious / zombie admin rows**; diagnostic logs that explain exactly what is happening. Minimize implementation tokens (delegate mechanical sub-phases to Haiku).

---

## 1. Context & problem

A real-world session exposed two user-visible defects and several latent resilience gaps. Provider: `bore local 5000 --tcp-secret-id dufs --udp --carriers 4`. Consumers: T1 (`--udp --carriers 4 --nat-udp-preferred-port 80`), T2 (`--udp --carriers 4`), T3 (`--carriers 4`, TCP, no `--udp`).

**Verified findings (all confirmed by reading the code, not just recon):**

| ID | Sev | Symptom / mechanism | Anchor |
|----|-----|----|----|
| **BUG-S1** | **CRIT** | TCP consumer `--carriers 4` ⇒ 1 correct row + 3 spurious `N/A` rows in /admin "Consumers". Each extra relay carrier dials the server and re-sends a full `ClientMessage::ConnectSecret` with sentinel fields (`notes:None, carriers:0, local_proxy_port:0`); the server dispatcher routes it to `serve_consumer`, which **unconditionally registers a fresh admin entry** with `local_proxy_port:None` → renders "N/A". | sender `src/secret.rs:1434-1448`; dispatch `src/server.rs:1269-1282`; register `src/secret.rs:463-493` |
| **BUG-S2** | **HIGH** | Those same carrier connections **never send `ClientMessage::Heartbeat`** — their client-side task only *drains* server frames. So `serve_consumer`'s liveness reaper sees no client frame and **reaps each carrier after `ctrl_timeout` (60 s)**. The consumer pool silently degrades **N→1** (no redial path for consumer carriers), losing HOL isolation, and the spurious rows churn (appear → reaped → maybe re-created on reconnect). | drain-only `src/secret.rs:1464-1469`; reaper `src/secret.rs:505-516`; no redial: pool seeded once at `src/secret.rs:962-982` |
| **BUG-S3** | **MED** | Provider log `WARN direct udp accept failed (will retry in 100ms) err=QUIC handshake failed` alarmed the user. It is **benign**: `provider_direct` accepts in a loop; the real connection (`peer=…:80`, token verified) succeeded and is served — the WARN is a *second, incomplete incoming from hole-punch crossfire* (`incoming.await` never completed TLS). WARN is indistinguishable from a real failure. | loop `src/client.rs:1492-1543`; handshake `src/holepunch.rs:1498`; "token verified" `src/holepunch.rs:1515` |
| **BUG-S4** | **MED** | `relay()` calls `pool.pick()` then `opener.open().await`; if the picked carrier died between pick and open, the forwarded connection **drops once with no failover** to another live carrier. Resilience gap under carrier churn. | `src/secret.rs:676-677` |
| **BUG-S5** | **LOW→MED** | `--carriers N>1` on a secret **UDP** consumer that establishes a direct path is a **silent no-op**: the relay-carrier loop is gated `matches!(data_path, Relay) && carriers>1` and direct uses a single QUIC connection. Flag silently ineffective (violates the repo's "no silent caps" rule). | gate `src/secret.rs:966`; direct = 1 conn `src/secret.rs:951` |
| **OBS-S6** | LOW | Direct-path TX/RX is always `0.00 B` on the relay admin page **by design** (data flows peer-to-peer, off the server). Not a bug, but misleading next to relay tunnels. The screenshot's `0.00 B` is also simply *idle* (no traffic was sent). | `mark_udp` note `src/secret.rs:526-529`; relay counting (works) `src/secret.rs:685` |

**Explicitly REJECTED finding (do not implement):** a hunter proposed disabling QUIC connection migration / filtering the accepted source address against the offered candidate list. The accepted `peer=100.100.0.2:54496` (a CGNAT `100.64/10` egress that was never *offered* as a STUN candidate) is **legitimate** — token auth is the real gate, and many NAT/CGNAT consumers' actual QUIC 5-tuple differs from their reflexive candidate. Filtering by source would **break NAT traversal**. Recorded as **D7**.

### Goal
Make secret tunnels stable and resilient: each logical tunnel = **exactly one** admin row regardless of `--carriers`/transport; carriers stay alive for the tunnel's lifetime; benign hole-punch noise is logged honestly; dead carriers fail over; ineffective flags warn instead of silently no-op; and a netns hardening harness proves all of it under load + chaos. No regressions; all quality gates green.

### Reference scenario (final acceptance test)
```
1 bore server --udp on ns0 (relay + QUIC).
5 providers, distinct ids (s1..s5), mixed: s1/s2 --udp, s3/s4 tcp, s5 --udp --carriers 4.
Per provider: 5–10 proxies, mixed tcp/udp, mixed --carriers {1,4}, mixed flags
  (--nat-udp-preferred-port, --notes, --auto-reconnect, --basic-auth on a subset).
Drive echo + bulk traffic through every proxy.
Chaos: randomly SIGKILL/SIGTERM providers & proxies on a loop for 2 min; --auto-reconnect on.
Conflict: start a 2nd provider with an already-registered id; start a proxy for a missing id.

ASSERT (via GET /admin/api/v1/secret):
  - consumer rows == number of LIVE logical proxies (NOT × carriers); zero rows with local_proxy_port==null.
  - provider rows == number of LIVE providers; zero zombie rows after reaper window.
  - after each kill+reconnect, the old entry is gone and exactly one new entry exists (no duplicate/zombie).
  - every proxy relays bytes correctly (echo + blake3 of bulk matches); carrier pool stays at N for the tunnel's life.
  - duplicate-id provider rejected with a clear server error; orphan proxy closes with a clear message.
  - logs: zero alarming WARNs for benign punch strays; every reap/degrade/fallback has a one-line reason.
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | A secret **consumer carrier** connection must NOT create an admin entry and must NOT be reaped by the consumer control reaper. | Add a `carrier` flag to the consumer-connect message; on the server, `carrier=true` skips `admin.register`, skips UDP brokering, and runs a **non-reaping** accept-and-relay loop. Fixes BUG-S1 **and** BUG-S2 with one change. |
| **D2** | Implement D1 as an **additive `carrier: bool` field on `ClientMessage::ConnectSecret`** (`#[serde(default)]`), mirroring the repo's established additive-serde-default wire pattern (e.g. notes/id added earlier). NOT a new enum variant. | One field, one server branch. Old server decoding a new client defaults `carrier=false` → current (buggy) behavior = graceful degradation, no panic. Implementer MUST verify the encoder tolerates a trailing defaulted field; if it does not, fall back to a new variant `JoinConsumerCarrier` appended LAST (wire-compat rule, like `Heartbeat`). |
| **D3** | The logical tunnel's liveness is owned by the consumer's **main** control connection (which already sends `Heartbeat` every `CTRL_CLIENT_HEARTBEAT`). Carriers need no heartbeat and are not reaped. | A wedged carrier no longer inflates the count (it has no admin entry) and cannot false-trip a 60 s reap. Carrier death is detected by the data layer (yamux keepalive / drain task `alive=false` → pool prune). |
| **D4** | **Frontend defense-in-depth:** the secret panel dedups consumer rows by `(secret_id, peer_ip, local_proxy_port)` and drops/aggregates entries with `local_proxy_port==null`. | Even against an *old* server (pre-D1) the UI shows one row per logical consumer. Backend fix is primary; FE dedup is belt-and-suspenders. |
| **D5** | `relay()` retries `pool.pick()`→`open()` across live carriers (up to pool size) before failing the forwarded connection. | Carrier churn no longer drops in-flight proxied connections; resilience target. |
| **D6** | Classify direct-path accept outcomes: an **unauthenticated/incomplete incoming** (no token seen, handshake never completed) logs at `debug` ("stray punch incoming"); only an **authenticated-but-failed** or **endpoint-level** error logs at `warn`. Do **not** `sleep(100ms)` on a stray (it stalls accepting the real carriers). | Removes the alarming false WARN (BUG-S3) without hiding genuine failures. |
| **D7** | Do NOT disable QUIC migration and do NOT filter accepted source against offered candidates. Token auth is the gate. | Preserves CGNAT/asymmetric-NAT consumers (the `100.100.0.2` case). Recorded so a future "fix" does not regress it. Add a regression assertion that a token-valid conn from an un-offered source is accepted. |
| **D8** | `--carriers N>1` on a secret UDP consumer that goes **direct** must `warn!` once that the direct path uses a single QUIC connection (N applies only to the relay fallback). | No silent no-op (BUG-S5). Multi-connection direct (sibling QUIC conns, VPN-style) is **out of scope v1** — documented as a known limitation, not silently dropped. |
| **D9** | All new diagnostics are **additive logging only** in Phase 0 (no behavior change) so they land safely and aid debugging the rest of the work. | Phase 0 is pure-additive and independently shippable. |
| **D10** | New e2e harness `scripts/secret_netns_test.sh` follows the existing `local_proxy_netns_test.sh` topology + `pass()/fail()` convention and asserts on `GET /admin/api/v1/secret`. Run via `sudo -n /abs/path/...`. | Reuses proven scaffolding; no new test framework. |

---

## 3. Target architecture

### 3.1 Consumer carrier lifecycle (the core fix)
Today (broken):
```
consumer main:    ConnectSecret{full meta} → serve_consumer → REGISTER entry + reaper + broker + accept/relay   ✓ wanted
consumer carrier: ConnectSecret{sentinels} → serve_consumer → REGISTER entry + reaper + broker + accept/relay   ✗ extra row + reaped@60s
provider carrier: JoinCarrier{token}       → serve_carrier  → deliver opener to pool (NO entry, NO reaper)        ✓ correct model
```
Target:
```
consumer carrier: ConnectSecret{carrier:true} → serve_consumer(carrier=true)
                     → NO admin.register, NO broker_udp, NON-REAPING accept/relay loop (heartbeat-drain only)
```
The carrier still **accepts data substreams and relays each via `relay()`** (that is the whole point of a relay carrier — spread forwarded substreams across TCP connections to avoid single-connection HOL). Only registration, brokering, and reaping are skipped.

### 3.2 Direct-path accept classification (BUG-S3)
`DirectListener::accept` (`src/holepunch.rs:1492-1522`) and the provider loop (`src/client.rs:1492-1543`) must distinguish:
- **stray/incomplete** (`incoming.await` failed, or token never read) → `debug!`, no sleep, continue accepting.
- **authenticated failure** (token read & matched but stream/handshake then failed) or **endpoint closed** → `warn!` with peer + phase, keep listener alive.

### 3.3 Relay carrier failover (BUG-S4)
`relay()` (`src/secret.rs:657-688`) wraps the pick→open in a bounded retry over live carriers; logs each skipped-dead carrier at `trace`, fails the connection only when no live carrier remains.

### 3.x Reuse map (do not reinvent)
| Need | Reuse | Location |
|------|-------|----------|
| Non-registering carrier server handler pattern | `serve_carrier` (provider model) | `src/server.rs:1906-1931` |
| Consumer server handler to branch | `serve_consumer` | `src/secret.rs:443-…` (register 463-493; reaper 499-516; broker 520-…) |
| Consumer carrier dialer to flag | `open_consumer_carrier` | `src/secret.rs:1417-1471` |
| Consumer-connect message to extend | `ClientMessage::ConnectSecret` | `src/shared.rs` (variant def; see also redaction `:1268`) |
| Server dispatch of ConnectSecret | dispatcher arm | `src/server.rs:1269-1282` |
| Carrier pool (push/pick/prune/alive) | `CarrierPool` | `src/pool.rs:88 (push), 109-117 (pick)` |
| Live byte counting (relay) | `shared::CountingStream` | `src/secret.rs:685` |
| Direct accept + token verify | `DirectListener::accept` | `src/holepunch.rs:1492-1522` |
| Provider direct accept loop | `provider_direct` | `src/client.rs:1480-1558` |
| Admin entry create/drop (RAII) | `AdminRegistry::register` / `Registration::drop` | `src/admin.rs:289, 452` |
| Admin secret JSON view | `SecretView` + `secret()` builder | `src/admin_views.rs`; `src/admin_api.rs:96-128` |
| FE consumer render | secret panel | `src/admin_ui/panels/secret.js:29-38, 128-131` |
| In-proc server+client test helpers | `spawn_server`, `spawn_secret_tunnel`, `wait_role`, `secret_ctrl_timeout` | `tests/secret_test.rs:40, 214, 746`; `Server::secret_ctrl_timeout` |
| e2e topology + assert helpers | netns harness | `scripts/local_proxy_netns_test.sh`; chaos patterns `scripts/vpn_netns_test.sh:99,71-80,525` |
| Admin entry enumeration in tests | `AdminRegistry` by role / `GET /admin/api/v1/secret` | `tests/secret_test.rs:746`; `src/admin_http.rs:109-111` |

---

## 4. New interface (CLI flags / API / config)
**No new user-facing CLI flags.** Behavior changes only:
- `--carriers N` semantics clarified for secret UDP-direct consumers via a one-shot `warn!` (D8).
- New diagnostic log lines (Phase 0) — see §6 Phase 0.
- Admin JSON `SecretView` may gain an optional `direct_active: bool` (or reuse existing `udp`) so the UI can mark "direct (bytes off-relay)" for OBS-S6 — **additive, `#[serde(default)]`**, FE-only consumer.

---

## 5. New protocol / data structures
**Wire change (additive, backward-compatible):**
```rust
// src/shared.rs — ClientMessage::ConnectSecret
ConnectSecret {
    id: String,
    // … existing fields unchanged …
    #[serde(default)]            // ← NEW: false on the wire from old clients
    carrier: bool,
}
```
Back-compat (D2): trailing `#[serde(default)]` field. New client + new server → carrier=true honored. New client → **old** server: field ignored, carrier treated as a normal consumer (current behavior — graceful, no panic). Client and server ship in the same image, so the mixed case is transient. **If the active codec does not tolerate a trailing defaulted struct field**, instead add `ClientMessage::JoinConsumerCarrier { id, /*auth follows*/ }` appended LAST in the enum (the `Heartbeat`-style wire-compat rule) and route it to the new handler. Update the redaction match (`src/shared.rs:1268`) accordingly.

No persisted-schema or public-API breakage.

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase must pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (+ `cargo build --release` for binary used by netns e2e); **zero regressions**; update docs on behavior/API change; **print the model used per sub-task**. Build line for secret+udp: `cargo build --release` (udp is default; `--features vpn` only for VPN tests).

---

### Phase 0 — Observability (pure additive, no behavior change)
> Land first, alone. Makes the rest debuggable. Closes LOG-GAP1..5. (D9)

#### 0.1 Admin entry create/drop tracing
- **Model:** Haiku
- **Files:** `src/admin.rs:289` (register), `src/admin.rs:452` (Registration::drop)
- **Change:** `info!(id, %role, peer, "admin entry registered")` on register; `info!(id, %role, "admin entry dropped")` on drop. Lets ops correlate /admin churn with reaps/reconnects.
- **Unit tests:** none (log-only). Compile + existing `admin_registry_reflects_connections` (`tests/secret_test.rs:604`) still green.
- **e2e tests:** none.
- **Done:** gates green; logs show paired register/drop on a connect+disconnect.

#### 0.2 Carrier-pool & reap diagnostics
- **Model:** Haiku
- **Files:** `src/secret.rs:981` (pool established log — add `requested=carriers, opened=pool.len()` and `warn!` if `opened<carriers`), `src/pool.rs:109-117` (`trace!(pruned=n)` on prune), `src/secret.rs:512` (reaper already warns — add `role`/`peer`).
- **Change:** make degraded pools and reaps explicit and attributable.
- **Unit tests:** none (log-only).
- **e2e tests:** none.
- **Done:** gates green; a degraded pool emits a WARN naming requested vs opened.

#### 0.3 Direct-path phase logging
- **Model:** Sonnet
- **Files:** `src/holepunch.rs:1498-1515`, `src/client.rs:1539`
- **Change:** add structured context (`peer`, `phase=handshake|token|stream`) to direct accept logs so a failure says *which* phase failed and *which* peer. No reclassification yet (that is Phase 2, behavior-adjacent).
- **Unit tests:** none.
- **e2e tests:** none.
- **Done:** gates green; accept failure log names the phase + peer.

---

### Phase 1 — Fix spurious rows + carrier reap (BUG-S1, BUG-S2)
> The core correctness slice. **Opus review gate** (data-model / lifecycle / wire). Independently shippable.

#### 1.1 Add `carrier` flag to ConnectSecret (wire)
- **Model:** Opus review → Haiku implements
- **Files:** `src/shared.rs` (ConnectSecret variant + `:1268` redaction), `src/secret.rs:1434-1448` (send `carrier:true` from `open_consumer_carrier`), the main consumer send site `src/secret.rs:~880-902` (send `carrier:false`).
- **Change:** additive `#[serde(default)] carrier: bool` per §5/D2. Verify codec tolerance; else pivot to `JoinConsumerCarrier` variant (D2 fallback).
- **Unit tests:** `connectsecret_carrier_serde_default` — decode a payload lacking the field → `carrier==false` (back-compat). Round-trip with `carrier=true`.
- **e2e tests:** none (covered by 1.3 + Phase 4).
- **Done:** gates green; wire round-trips both values; old-format decode defaults false.

#### 1.2 Server: non-registering, non-reaping carrier branch
- **Model:** Opus review → Sonnet implements
- **Files:** `src/server.rs:1269-1282` (pass `carrier` into the call), `src/secret.rs:443-…` (`serve_consumer`: when `carrier`, **skip** `admin.register` (463-493), **skip** `broker_udp`/`mark_udp`, and run a loop that drains client frames + accepts/relays data substreams but **never** evaluates the `ctrl_timeout` reap).
- **Change:** smallest branch that reuses the existing accept/relay path; only registration/brokering/reaping are conditional. Keep `carrier=false` path **byte-identical** (I-1).
- **Unit tests (in `tests/secret_test.rs`):**
  - `secret_consumer_carrier_creates_no_admin_entry` — provider + 1 consumer `--carriers 4` (relay) ⇒ `wait_role(SecretConsumer)==1` (not 4).
  - `secret_consumer_carrier_not_reaped` — with `secret_ctrl_timeout(500ms)`, after 2 s the consumer still has 4 live carriers (pool len stays 4) and exactly 1 admin entry; relay still works.
  - `secret_consumer_main_still_reaped_when_wedged` — existing `secret_consumer_reaped_when_control_wedges` (`tests/secret_test.rs:770`) still passes (main connection reaping unchanged).
- **e2e tests:** T-SEC-CARRIER-COUNT, T-SEC-NOSPURIOUS (Phase 4).
- **Done:** gates green; 4-carrier relay consumer = 1 admin row, 4 live carriers, survives past `ctrl_timeout`.

#### 1.3 Frontend dedup (defense-in-depth) (D4)
- **Model:** Sonnet
- **Files:** `src/admin_ui/panels/secret.js:29-38` (build/group), `:128-131` (render)
- **Change:** group consumer entries by `(secret_id, peer_ip, local_proxy_port)`; drop entries whose `local_proxy_port==null` when a sibling with a real port exists, else show one "carrier" aggregate. Provider grouping already correct — do not touch.
- **Unit tests (Node, `test/admin_ui/secret-carrier-dedup.test.js`, run `npm test`):** given a payload of 1 primary + 3 `local_proxy_port:null` carrier entries with same secret_id/peer_ip → renders exactly 1 consumer row; flags/notes from the primary preserved. Existing `secret-group.test.js`/`secret-detail.test.js` still pass.
- **e2e tests:** covered by T-SEC-NOSPURIOUS asserting via API (backend) — FE test covers render.
- **Done:** gates green; `npm test` green; old-server payload still renders 1 row.

---

### Phase 2 — Direct-path log honesty (BUG-S3) + migration regression guard (D7)
> Behavior-adjacent (log levels + removing a 100 ms stall). **Opus review gate** (must not hide real failures or regress NAT).

#### 2.1 Classify accept outcomes
- **Model:** Opus review → Sonnet implements
- **Files:** `src/holepunch.rs:1492-1522` (return/annotate a typed outcome distinguishing incomplete-handshake / token-absent vs token-verified-then-failed), `src/client.rs:1536-1541` (log `debug!` + **no sleep** for stray; `warn!` only for authenticated/endpoint failure).
- **Change:** per D6. Keep the listener alive in all non-fatal cases (unchanged invariant). The real connection's accept + serve path is untouched.
- **Unit tests:** `direct_accept_stray_is_debug_not_warn` (if outcome is exposed as a testable enum) — a connection that never sends a token yields the "stray" classification; a token-verified one yields "accepted".
- **e2e tests:** T-SEC-UDP-CLEANLOG (Phase 4): provider log over a punch storm has **zero** `WARN …accept failed` for strays while the tunnel serves traffic.
- **Done:** gates green; benign strays no longer WARN; genuine token/endpoint failures still WARN.

#### 2.2 Migration/source-acceptance regression guard
- **Model:** Sonnet
- **Files:** `tests/secret_test.rs` (new test), no production change.
- **Change:** lock in D7 — a token-valid direct connection arriving from a source address that was never in the offered candidate list is **accepted** (asserts we never add source filtering / disable migration).
- **Unit tests:** `direct_accepts_token_valid_from_unoffered_source`.
- **e2e tests:** none.
- **Done:** gates green; test documents and protects the NAT-traversal behavior.

---

### Phase 3 — Resilience: carrier failover + flag honesty (BUG-S4, BUG-S5)
> Resilience hardening. Independently shippable.

#### 3.1 Relay dead-carrier failover (D5)
- **Model:** Sonnet
- **Files:** `src/secret.rs:676-677` (relay), `src/pool.rs:109-117` (pick — confirm it prunes dead before returning)
- **Change:** loop `pool.pick()`→`open()` up to `pool.len()` times, skipping carriers whose `open()` errors (mark dead), failing only when none succeed. `trace!` each skip.
- **Unit tests (`tests/secret_test.rs` / `tests/secret_pool_test.rs`):** `relay_fails_over_to_live_carrier` — pool of 2, kill carrier picked first → forwarded stream still succeeds via the second.
- **e2e tests:** exercised by T-SEC-CHAOS (Phase 4).
- **Done:** gates green; a forwarded connection survives the death of one carrier when another is live.

#### 3.2 `--carriers` honesty on secret UDP-direct (D8)
- **Model:** Haiku
- **Files:** `src/secret.rs:948-953` (direct established branch)
- **Change:** when `carriers>1` and the path resolved to `Direct`, `warn!` once: "secret --udp direct path uses a single QUIC connection; --carriers N applies only to the relay fallback (multi-connection direct not supported)". No behavior change.
- **Unit tests:** none (log-only).
- **e2e tests:** none.
- **Done:** gates green; warning emitted exactly once when carriers>1 on a direct UDP consumer.

#### 3.3 (Optional, gated) Direct-path indicator for admin (OBS-S6)
- **Model:** Sonnet
- **Files:** `src/admin_views.rs` (+ `#[serde(default)] direct_active`), `src/admin_api.rs:96-128`, `src/admin_ui/panels/secret.js`
- **Change:** mark direct consumers in the UI as "direct (bytes counted off-relay)" so `0.00 B` is not read as broken. Additive field; FE display only.
- **Unit tests:** Node test asserts the badge renders when `direct_active|udp` true.
- **e2e tests:** none.
- **Done:** gates green; UI distinguishes direct from relay TX/RX semantics. *(Drop if time-boxed; OBS-S6 is cosmetic.)*

---

### Phase 4 — Hardening test suite (load + conflict + chaos)
> The acceptance harness. **Opus review gate** on the assertion design (4.4/4.5). Heavy; runs under `sudo` netns.

#### 4.1 Harness scaffold
- **Model:** Sonnet
- **Files:** new `scripts/secret_netns_test.sh` (model on `scripts/local_proxy_netns_test.sh`: netns topology, `pass()/fail()` counters, EXIT trap cleanup `pkill -9 -f target/release/bore` + `ip netns del`), rebuild guard (refuse to run if `src/` newer than the release binary — pattern from `vpn_netns_test.sh`).
- **Change:** boot 1 server `--udp`; helper `spawn_provider <id> <flags>` and `spawn_proxy <id> <port> <flags>`; helper `count_secret_role <provider|consumer>` via `curl GET /admin/api/v1/secret | jq`.
- **Unit tests:** n/a (shell).
- **e2e tests:** harness self-check T-SEC-SMOKE (1 provider + 1 proxy echo).
- **Done:** `sudo -n /abs/scripts/secret_netns_test.sh` runs T-SEC-SMOKE green; cleanup leaves no netns/process.

#### 4.2 Load matrix
- **Model:** Sonnet
- **Files:** `scripts/secret_netns_test.sh`
- **Change:** 5 providers (s1..s5; mix udp/tcp, one with `--carriers 4`); per provider 5–10 proxies (mix tcp/udp, `--carriers {1,4}`, `--notes`, `--nat-udp-preferred-port`, `--basic-auth` subset). Drive echo + bulk (reuse the bulk/blake3 pattern from `local_proxy_netns_test.sh`).
- **Unit tests:** n/a.
- **e2e tests:** **T-SEC-LOAD** (all proxies relay correctly), **T-SEC-CARRIER-COUNT** (consumer rows == live proxies, not ×carriers), **T-SEC-NOSPURIOUS** (zero rows with `local_proxy_port==null`).
- **Done:** all three IDs green.

#### 4.3 Conflict scenarios
- **Model:** Haiku
- **Files:** `scripts/secret_netns_test.sh`
- **Change:** second provider with an existing id; proxy for a non-existent id; proxy with wrong `--secret`.
- **Unit tests:** mirror exists (`secret_duplicate_id_rejected` `tests/secret_test.rs:86`, `secret_proxy_without_provider_closes` `:538`, `secret_proxy_requires_correct_secret` `:577`) — e2e confirms end-to-end messaging.
- **e2e tests:** **T-SEC-CONFLICT** — duplicate-id rejected with clear server error in log; orphan/bad-secret proxy exits with a clear message; server entry count unchanged.
- **Done:** T-SEC-CONFLICT green; error messages present and human-actionable.

#### 4.4 Chaos: random kill/restart + autoreconnect
- **Model:** Opus review → Sonnet implements
- **Files:** `scripts/secret_netns_test.sh`
- **Change:** 2-minute loop randomly `SIGKILL`/`SIGTERM` providers & proxies (all with `--auto-reconnect`); after the storm, settle and assert.
- **Unit tests:** `secret_reconnect_no_duplicate_entry` (`tests/reconnect_test.rs` / `secret_test.rs`) — kill+reconnect a consumer ⇒ exactly 1 entry afterward (no old+new overlap).
- **e2e tests:** **T-SEC-CHAOS** (no panics/deadlocks; all survivors relay after settle), **T-SEC-RECONNECT** (each reconnected tunnel = exactly 1 fresh entry), **T-SEC-NOZOMBIE** (after the reaper window, provider/consumer counts equal live processes; zero zombies).
- **Done:** all three green across **both** relay and UDP modes; zero spurious/zombie rows post-settle.

#### 4.5 UDP fallback & mixed transport
- **Model:** Opus review → Sonnet implements
- **Files:** `scripts/secret_netns_test.sh`
- **Change:** force a UDP consumer to fall back to relay (block QUIC via netem/drop) and confirm it then opens N carriers **without** spurious rows (this is the BUG-S1 path that *also* affects UDP-on-fallback). Mixed tcp+udp consumers on one provider.
- **Unit tests:** n/a (netns).
- **e2e tests:** **T-SEC-UDP-FALLBACK** (udp→relay fallback consumer: 1 row, N carriers, no `null`-port rows), **T-SEC-MIXED** (tcp+udp consumers coexist cleanly), **T-SEC-UDP-CLEANLOG** (Phase 2.1 — no benign WARN spam).
- **Done:** all green; fallback path shows the same clean accounting as native relay.

---

### Phase 5 — Docs & final gate
> **Opus review gate** (final read).

#### 5.1 Docs
- **Model:** Haiku (draft) → Opus read
- **Files:** new/updated `docs/SECRET_HARDENING_ASSESSMENT.md` (findings BUG-S1..S6 + verdicts incl. rejected D7), update `CLAUDE.md` "Secret control liveness" block with the **consumer-carrier invariant** (carriers do not register/reap; heartbeat is main-connection-only), document `scripts/secret_netns_test.sh` usage + T-IDs.
- **Unit/e2e:** n/a.
- **Done:** docs match shipped behavior; CLAUDE.md invariant added.

#### 5.2 Full regression + acceptance
- **Model:** Opus
- **Files:** —
- **Change:** run all gates + full `cargo test` + `npm test` + `sudo -n /abs/scripts/secret_netns_test.sh` + existing `local_proxy_netns_test.sh` + `admin_dashboard_test.sh`.
- **Done:** **0 fails** everywhere; §1 reference scenario passes (T-SEC-LOAD, -CARRIER-COUNT, -NOSPURIOUS, -CONFLICT, -CHAOS, -RECONNECT, -NOZOMBIE, -UDP-FALLBACK, -MIXED, -UDP-CLEANLOG).

---

## 7. Invariants to preserve / add
- **I-1:** `carrier=false` (every existing consumer/provider path) stays **byte-identical**. Regression: `secret_tunnel_round_trip` (`tests/secret_test.rs:265`) + all existing secret tests unchanged.
- **I-2 (new):** A secret **consumer carrier** connection creates **no** admin entry and is **never** reaped by the control reaper. Liveness is owned solely by the consumer's main control connection's `Heartbeat`. (Mirrors the provider `serve_carrier` model.)
- **I-3:** One logical secret tunnel (provider or consumer) ⇒ **exactly one** admin row, independent of `--carriers` and transport.
- **I-4:** Direct path never gates tunnel liveness (existing). Benign hole-punch strays are non-fatal **and** non-alarming (D6).
- **I-5 (D7):** Token-authenticated direct connections are accepted regardless of source address; no migration disable, no candidate-source filtering. Regression: `direct_accepts_token_valid_from_unoffered_source`.
- **I-6:** No `mux::Stream` split across tasks; `STREAM_READY` before splice; `CountingStream` wraps relay splices — all unchanged by this work.

## 8. Risk register
| Risk | Mitigation |
|------|-----------|
| Wire change breaks old↔new interop | D2 additive `#[serde(default)]`; explicit back-compat unit test (1.1); enum-variant fallback documented. |
| `serve_consumer` branch accidentally alters the main path | Conditional only around register/broker/reap; I-1 byte-identical + full existing secret-test regression. |
| Removing the 100 ms sleep changes accept timing | D6 sleep removed only for *stray* outcomes; authenticated/endpoint errors keep backoff; T-SEC-UDP-CLEANLOG + manual punch-storm verify. |
| FE dedup hides a genuinely distinct consumer | Dedup key includes `peer_ip`+`local_proxy_port`; only `null`-port siblings are folded; Node test covers. |
| Carrier failover masks a fully-dead pool | Bounded to `pool.len()` attempts; fails (with log) when none live; T-SEC-CHAOS exercises. |
| netns harness flakiness under chaos | Settle window before asserts; rebuild guard; reuse proven `local_proxy`/`vpn` netns patterns; counts read from authoritative `/admin/api/v1/secret`. |

## 9. Verification summary
- **Gates (every sub-phase):** `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` · `cargo test`. Binary for e2e: `cargo build --release` (udp default).
- **Unit tests:** `tests/secret_test.rs` (carrier accounting, reap, failover, migration guard, serde), `tests/secret_pool_test.rs`, `tests/reconnect_test.rs`; FE `test/admin_ui/*.test.js` via `npm test`.
- **e2e:** `sudo -n /abs/path/scripts/secret_netns_test.sh` (new); regression: `local_proxy_netns_test.sh`, `admin_dashboard_test.sh`. Rebuild `--release` before sudo-running (harness refuses a stale binary).
- **Acceptance:** §1 scenario ⇔ T-SEC-LOAD, T-SEC-CARRIER-COUNT, T-SEC-NOSPURIOUS, T-SEC-CONFLICT, T-SEC-CHAOS, T-SEC-RECONNECT, T-SEC-NOZOMBIE, T-SEC-UDP-FALLBACK, T-SEC-MIXED, T-SEC-UDP-CLEANLOG.

## 10. Model-assignment summary
| Phase | Sub-phases by model | Primary | Opus review gate |
|-------|---------------------|---------|------------------|
| 0 | 0.1 Haiku · 0.2 Haiku · 0.3 Sonnet | Haiku | — |
| 1 | 1.1 Haiku · 1.2 Sonnet · 1.3 Sonnet | Sonnet | **1.1, 1.2** (wire + lifecycle) |
| 2 | 2.1 Sonnet · 2.2 Sonnet | Sonnet | **2.1** (log honesty / no hidden failures) |
| 3 | 3.1 Sonnet · 3.2 Haiku · 3.3 Sonnet (optional) | Sonnet | — |
| 4 | 4.1 Sonnet · 4.2 Sonnet · 4.3 Haiku · 4.4 Sonnet · 4.5 Sonnet | Sonnet | **4.4, 4.5** (chaos/fallback assertions) |
| 5 | 5.1 Haiku · 5.2 Opus | Opus | **5.1, 5.2** (final docs + acceptance read) |

> Start Sonnet; drop to Haiku for log-only/boilerplate/docs (0.1, 0.2, 3.2, 4.3, 5.1 draft); escalate to Opus only at the marked gates. Print the model used per sub-task during implementation.
