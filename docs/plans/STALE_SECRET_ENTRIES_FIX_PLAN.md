# Stale Secret Admin Entries + Broken Auto-Refresh — Design & Implementation Plan

> **Status:** IMPLEMENTED 2026-06-20 (branch `webserver-log`, uncommitted).
> Outcome notes:
> - **BUG-A (backend leak): FIXED.** Reaper added to `serve_provider`/`serve_consumer`
>   (`last_recv` + heartbeat-tick check — NOT `timeout(recv)`, which the 500 ms
>   heartbeat reset every iteration: caught during impl). Clients send
>   `ClientMessage::Heartbeat`; counts now track the registry. Tests: `T-WIRE1`
>   (`shared` wire roundtrip), `T-REAP-C1/C2`+`T-REAP-P1` (`secret_test.rs`,
>   real-server raw-socket e2e), `T-COUNT1/2` (`admin_test.rs`). All green.
> - **BUG-B (auto-refresh): source was already CORRECT** — `T-FE-POLL1` reproduces
>   the wiring headless and PASSES against current `src/admin_ui`, so the field
>   symptom = a **stale deployed binary** predating `91fa729` ("fe bugfix 01").
>   Action: **redeploy the current build.** Added `T-FE-POLL1`/`T-FE-POLL2` to lock
>   the wiring so it can never silently regress. D7 banner deferred (not a bug).
> - Gates green: `cargo fmt`, `cargo clippy --all-features --all-targets -D warnings`,
>   `cargo test` (lib 332, secret 15, admin 19), `npm test` 68/68.
>
> **Original plan (no code written) below.** Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture + recon synthesis).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = architecture review gates only).
> **Target:** zero zombie entries in the secret admin registry (data-integrity)
> + working live auto-refresh in `/admin/status`. Minimize implementation tokens
> (delegate mechanical sub-phases to Haiku; the recon below removes all re-search).

---

## 1. Context & problem

Two field-reported defects on `/admin/status#/secret` after the server VM
rebooted and clients auto-reconnected:

**BUG-A (backend, data integrity — primary):** the secret admin table shows
zombie entries that never clear. Field snapshot: 1 provider + **8** consumers
from one source IP, but only 2 consumers are real (ports 5007/5008, with
flags/notes). The other **6** are bare (`Local N/A`, no flags, no notes,
`Connections 0`, `TX/RX 0.00 B`) and persist indefinitely. Overview + Metrics
count them → "Secret Tunnels: 9" instead of 3.

Why they leak (verified, not assumed):
- Each secret control connection registers ONE long-lived admin `Entry` via an
  RAII `Registration`. Add: `src/secret.rs:278` (provider), `src/secret.rs:432`
  (consumer). Remove: ONLY `Registration::Drop` at `src/admin.rs:452-455`.
- `Registration` is owned by the `serve_provider`/`serve_consumer` future. The
  entry is removed **only when that future returns**.
- The control loops (`src/secret.rs:341-395` provider, `src/secret.rs:470-545`
  consumer) only return on: server→client `control.send(Heartbeat)` **error**,
  `control.recv()` → `None`, or `recv` error. There is **no recv deadline**.
- The control channel is a **yamux mux substream** (`Delimited<mux::Stream>`),
  not a raw TCP socket. On a half-open / abandoned peer (client process gone but
  TCP not FIN/RST'd, or client keeps the yamux conn alive for carriers but stops
  servicing the control substream), `control.send(Heartbeat)` **succeeds into the
  yamux buffer** (no peer ACK required) and `control.recv()` **blocks forever**.
  → the future never returns → `Registration` never drops → **entry leaks**.
- TCP keepalive (`src/shared.rs:236-237`, 15 s) and yamux config
  (`src/mux.rs:59-66`, `Config::default`) do not reap these in the field
  (zombies observed alive 18+ min). The bare display (`local_proxy_port:0`, all
  flags false) means these were `ConnectSecret` connections from an old/default
  client build — they consumed `serve_consumer` and leaked.
- The client never sends anything periodically: server read loops treat any
  client message that isn't a UDP-broker message as "unexpected"
  (`src/secret.rs:371,510`); there is no `ClientMessage::Heartbeat`. So the
  server currently has **no positive liveness signal** to time out on.

Count path (downstream of the leak — no separate bug): `summary()` counts every
`SecretProvider|SecretConsumer` entry at `src/admin_api.rs:27`; `metrics()` the
same at `src/admin_api.rs:479-480`; the table at `src/admin_api.rs:96-128`. None
filter liveness (correctly — an idle live tunnel has `active=0`). Fixing the leak
fixes the counts; we only add regression assertions here.

**BUG-B (frontend, auto-refresh):** the operator must reload the browser to see
new data. Tell: in the snapshot **all 10 rows share the identical uptime
`18m 13s`** — the page is frozen at load time and never re-polled.

Frontend state (verified):
- Poller is correct in isolation: `src/admin_ui/poller.js` (`setInterval`,
  `DEFAULT_REFRESH_MS=30000`), unit-tested at `test/admin_ui/poller.test.js`.
- Wiring: `src/admin_ui/app.js:12` `createPoller(()=>refreshCurrent())`;
  `app.js:55-59` `setupPolling()` reads `activePanel.refreshMs`;
  `app.js:61` re-arms on `hashchange`; `app.js:66` arms at load.
  `src/admin_ui/router.js:52,67-69` `refreshCurrent → renderPanel` re-fetches.
- Each panel carries `refreshMs` (e.g. `src/admin_ui/panels/secret.js:15` =
  `DEFAULT_REFRESH_MS`); `registry.js` exports the panel objects directly, so
  `activePanel.refreshMs` is defined. **The source logic is correct.**
- Assets already served with `Cache-Control: no-store`
  (`src/admin_http.rs:332,369`) → stale browser cache is ruled out *for a current
  binary*.
- ⇒ Highest-probability cause: the **deployed binary predates `91fa729`
  ("fe bugfix 01")** which introduced `poller.js`; assets are embedded at compile
  time (`src/admin_http.rs:20`, `include!(OUT_DIR/admin_assets.rs)` via build.rs),
  so an old binary serves the BUG-0 JS that never auto-refreshed. The wiring path
  (`app.js setupPolling`) is also **not covered by any test** — `poller.test.js`
  tests the timer in isolation only. We treat BUG-B as: confirm empirically, then
  close the test gap + make the running build self-evident so this is diagnosable
  in seconds next time.

### Goal
No zombie can survive in the secret admin registry: a dead/abandoned secret
control connection is reaped within a bounded time (≤ 60 s) and its admin entry
removed, so the table and the Overview/Metrics counts reflect only live tunnels.
And: `/admin/status` auto-refreshes live (uptime ticks without a manual reload),
with a regression test on the wiring and a visible running-build identifier.

### Reference scenario (final acceptance test)
```
1. Start `bore server --udp` (admin enabled).
2. Start consumer A: `bore proxy --tcp-secret-id X --local-port 5007 ...`.
3. Kill consumer A with SIGKILL (no clean FIN) AND black-hole its socket
   (drop its TCP via netns/iptables) so the server sees a half-open peer.
4. Within 60 s: GET /admin/api/v1/secret no longer lists A; /overview and
   /metrics secret_tunnels count drops by 1. (BUG-A acceptance.)
5. Load /admin/status#/secret in a browser, do nothing for 35 s:
   the "Uptime" column increases on its own (page re-polled). (BUG-B acceptance.)
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Add a server-side **recv deadline** to both secret control loops, mirroring the proven VPN pattern (`CTRL_HEARTBEAT_TIMEOUT` 60 s, `src/vpn.rs:745`). | `control.recv()` wrapped in `tokio::time::timeout(SECRET_CTRL_TIMEOUT)`; on `Elapsed` → `warn!` + `return Ok(())` → `Registration` drops → entry removed. This is the authoritative reaper; the server never trusts the client to behave. |
| **D2** | Client provider+consumer control loops send a periodic `ClientMessage::Heartbeat` (every `HEARTBEAT_INTERVAL`-class tick, ≪ timeout). | Gives the server a positive liveness signal so D1's deadline only fires on a genuinely wedged/abandoned peer, never on a healthy idle tunnel. |
| **D3** | `SECRET_CTRL_TIMEOUT = 60 s` (parity with VPN). | Client beats well under it; tolerates loss. New shared const near `src/secret.rs:45`. |
| **D4** | New `ClientMessage::Heartbeat` is appended at the **end** of the enum (`src/shared.rs:758`); server handles it as a no-op (recv resets the deadline by returning). | Wire-compat: appended variant → newer decoders fine; an *old server* decoding a *new client*'s `Heartbeat` errors (unknown variant) and drops the conn. Operator controls all peers → **upgrade server first or both together**. Recorded as a risk; no capability-gate complexity for a single-operator deployment. |
| **D5** | Do **not** filter the Overview/Metrics counts by `active>0`. | An idle live tunnel legitimately has `active=0`; filtering would hide real tunnels. Counts stay a pure entry count; correctness comes from D1 removing zombies. Add regression tests only. |
| **D6** | BUG-B is diagnosed before being "fixed": a headless wiring test reproduces the deployed symptom against current source. If current source passes, the production fix is **redeploy current build**; we still land the wiring test + a build/version banner. | Avoids shipping a speculative code change to already-correct JS; closes the real gap (untested wiring + unknown running build). |
| **D7** | Add a visible **running-build + server-uptime** indicator to the admin UI header/overview (version string already plumbed: `src/admin_api.rs`, used by `src/admin_ui/panels/overview.js`). | Operator confirms which binary is live and sees uptime tick = auto-refresh working, in one glance. Turns "is it stale?" into a 2-second check. |
| **D8** | `carriers==1` / legacy single-connection secret path stays byte-identical; the only behavioral change is the bounded recv + a periodic tiny heartbeat frame. | Preserves CLAUDE.md secret-path invariants; regression-tested. |

---

## 3. Target architecture

### 3.1 Liveness state machine (per secret control connection)
```
serve_consumer / serve_provider loop:
  select! {
    _ = heartbeat.tick()            => send ServerMessage::Heartbeat (unchanged)
    msg = timeout(60s, control.recv()) =>
        Ok(Ok(Some(Heartbeat)))     => no-op (deadline implicitly reset)   [NEW]
        Ok(Ok(Some(other)))         => existing handling
        Ok(Ok(None)) | Ok(Err(_))   => return Ok(())   (existing clean/death exit)
        Err(Elapsed)                => warn! + return Ok(())               [NEW: reaper]
    ... (other existing branches: offers, carriers, acceptor)
  }
=> on ANY return, Registration::Drop removes the admin entry. (src/admin.rs:452)
```
Client side (provider + consumer control loops): add a `heartbeat_tx.tick()`
branch that sends `ClientMessage::Heartbeat`; keep draining server heartbeats.

### 3.2 Why recv-timeout (not write-timeout / not keepalive-only)
- A write-timeout on `control.send(Heartbeat)` cannot work: the mux send
  completes into the yamux buffer instantly even on a dead peer.
- TCP/yamux keepalive is necessary but **insufficient** (field zombies survived
  18+ min; an abandoned-but-alive yamux conn keeps the substream open). The
  recv-deadline is the only signal that catches a client that stops servicing
  the control substream while the TCP stays up.

### 3.3 Reuse map (do not reinvent)
| Need | Reuse | Location |
|------|-------|----------|
| Bounded ctrl read + 60 s const | VPN `CTRL_HEARTBEAT_TIMEOUT` + its `timeout(recv)` pattern | `src/vpn.rs:745` (+ its ctrl actor read) |
| RAII entry removal (already correct) | `Registration` Drop | `src/admin.rs:452-455` |
| Heartbeat send cadence | `HEARTBEAT_INTERVAL` + `interval()` + `MissedTickBehavior::Delay` | `src/secret.rs:45,339-340,468-469` |
| Wire enum to extend | `ClientMessage` | `src/shared.rs:758` |
| Server dispatch (where loops are entered) | `serve_provider`/`serve_consumer` call sites | `src/server.rs:1222,1267` |
| Poller (correct, reuse as-is) | `createPoller` | `src/admin_ui/poller.js` |
| Wiring under test | `setupPolling`/`refreshCurrent` | `src/admin_ui/app.js:55-66`, `router.js:67` |
| DOM stub for headless JS tests | `test/admin_ui/dom-stub.js` | existing |
| Version string for banner | admin version field | `src/admin_api.rs` → `src/admin_ui/panels/overview.js` |
| Rust integration harness | secret integration tests | `tests/secret_test.rs` |
| e2e dashboard harness | admin dashboard e2e | `scripts/admin_dashboard_test.sh` |

---

## 4. New interface (CLI flags / API / config)
None. No new flags, endpoints, or config. (Internal const `SECRET_CTRL_TIMEOUT`
only.) The admin JSON for `/secret`, `/overview`, `/metrics` is unchanged in
shape; an optional `server_uptime_secs`/`build` field for the D7 banner is the
only additive API change — see §5.

---

## 5. New protocol / data structures

**Wire (D4) — additive, append-only:**
```rust
// src/shared.rs, enum ClientMessage (≈ line 758), append LAST variant:
/// Periodic liveness ping from a secret provider/consumer control loop so the
/// server can time out a wedged/abandoned control substream (the mux stream
/// hides a half-open peer from send/recv). No payload.
Heartbeat,
```
Backward-compat: append-only; old clients never send it; an old **server**
decoding it errors → operators must upgrade the server first or in lockstep
(D4 risk row). New server treats `ClientMessage::Heartbeat` as a no-op
(replace the `Some(_) => warn!` arm so Heartbeat is matched before the catch-all
in both `serve_provider` and `serve_consumer`).

**Admin API (D7) — additive field(s):** add `build: String` (and optionally
`server_uptime_secs: u64`) to the overview/summary view struct in
`src/admin_api.rs`; `#[serde(default)]` so older frontends ignore it. No
breaking change.

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase must pass the gates
(`cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`, and for JS
`npm test`); **zero regressions**; update docs on behavior change; **print the
model used per sub-task.**

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — Wire + const scaffolding (pure additive, no behavior change)

> Adds the `Heartbeat` variant and the timeout const. Safe to land alone:
> nobody sends/enforces yet.

#### 0.1 Add `ClientMessage::Heartbeat` + `SECRET_CTRL_TIMEOUT`
- **Model:** Haiku (mechanical enum + const).
- **Files:** `src/shared.rs:758` (append variant), `src/shared.rs:~1357`
  (its `Display`/label arm if `ClientMessage` has one — mirror `ServerMessage::Heartbeat` at `src/shared.rs:1357`), `src/secret.rs:45` (add `const SECRET_CTRL_TIMEOUT: Duration = Duration::from_secs(60);`).
- **Change:** append `Heartbeat,` to `ClientMessage`; add the const. No call sites yet.
- **Unit tests:** `T-WIRE1` (in `tests/` or `src/shared.rs` mod tests) —
  round-trip serialize/deserialize `ClientMessage::Heartbeat`; assert it decodes
  back equal and that decoding it does not disturb existing variants.
- **e2e tests:** none (no behavior).
- **Done:** gates green; existing `shared` tests unchanged.

---

### Phase 1 — Server-side reaper (closes the leak)

> The data-integrity fix. Independently shippable AFTER Phase 2's client
> heartbeat is deployed, OR shippable alone if relying on the deadline only
> (a healthy idle tunnel would then be reaped at 60 s — so DO NOT ship Phase 1
> without Phase 2). **Order: deploy Phase 2 clients, then Phase 1 server.**

> **Behavior change (loud):** a secret control loop now returns on a 60 s recv
> stall. Healthy tunnels must send heartbeats (Phase 2) to stay under it.

#### 1.1 Bound `control.recv()` in `serve_consumer`
- **Model:** Opus review gate → Sonnet implements (concurrency/lifecycle on a
  hot control path; must preserve all existing `select!` branches and the
  half-close/relay spawn at `src/secret.rs:514-543`).
- **Files:** `src/secret.rs:470-545`.
- **Change:** wrap the `message = control.recv()` branch in
  `timeout(SECRET_CTRL_TIMEOUT, control.recv())`; map `Err(Elapsed)` →
  `warn!(%id, "secret consumer control idle >60s; reaping"); return Ok(())`.
  Add a `Some(ClientMessage::Heartbeat) => {}` arm **before** the
  `Some(_) => warn!` catch-all (`src/secret.rs:510`). Leave `heartbeat.tick()`,
  `acceptor.accept()`, UDP-broker branches untouched.
- **Unit tests:** `T-REAP-C1` (`tests/secret_test.rs`) — drive a consumer whose
  client sends no heartbeats and stops reading; assert the admin registry entry
  is gone within `>60s` (use a tokio paused-time / injected short timeout const
  via `#[cfg(test)]` override to keep the test fast). `T-REAP-C2` — a consumer
  that DOES send periodic heartbeats is NOT reaped over 3× the interval.
- **e2e tests:** `T-SEC-REAP` (extend `scripts/admin_dashboard_test.sh` or
  `tests/secret_test.rs` netns) — SIGKILL + black-hole a consumer (half-open),
  poll `/admin/api/v1/secret` until the entry disappears (≤ 60 s); assert
  `/overview` secret count decremented.
- **Done:** gates green; `T-REAP-C1/C2` + `T-SEC-REAP` pass; the legacy
  single-connection happy path (`tests/secret_test.rs` existing) unchanged.

#### 1.2 Bound `control.recv()` in `serve_provider`
- **Model:** Sonnet (mirror 1.1; same pattern, simpler loop).
- **Files:** `src/secret.rs:341-395`.
- **Change:** same `timeout(...)` wrap on the `message = control.recv()` branch;
  `Heartbeat` no-op arm before `Some(_) => warn!` (`src/secret.rs:371`);
  `Elapsed` → warn + `return Ok(())`. Leave `heartbeat`, `recv_offer`,
  `recv_carrier` branches untouched.
- **Unit tests:** `T-REAP-P1` — provider with no client heartbeats + stalled
  recv is reaped ≤ timeout; `T-REAP-P2` — heartbeating provider survives.
- **e2e tests:** covered by extending `T-SEC-REAP` to the provider side
  (half-open the provider; assert its admin entry clears).
- **Done:** gates green; tests pass; carrier-pool join path unaffected
  (`tests/secret_pool_test.rs` green).

---

### Phase 2 — Client heartbeat senders (must deploy before Phase 1)

> Makes healthy tunnels survive the new deadline. Additive on the wire (Phase 0).
> Ship to all clients first.

#### 2.1 Provider client sends periodic `ClientMessage::Heartbeat`
- **Model:** Sonnet.
- **Files:** client provider control loop — the secret client `listen`/control
  loop that reads `ServerMessage::Heartbeat` at `src/secret.rs:1130` (find its
  enclosing `select!`/loop; add a sibling `interval` tick branch that sends
  `ClientMessage::Heartbeat`).
- **Change:** add a `tokio::time::interval` (e.g. 15–20 s, comfortably <60 s);
  on tick `control.send(ClientMessage::Heartbeat).await` (return/break on error,
  matching existing death handling). Keep `MissedTickBehavior::Delay`.
- **Unit tests:** `T-HB-P1` — assert the provider client emits a `Heartbeat`
  within one interval (mock/inspect the control sink, mirroring existing secret
  client tests).
- **e2e tests:** none beyond Phase 1's `T-REAP-P2` (which depends on this).
- **Done:** gates green; provider stays registered indefinitely while idle
  (paired with 1.2's `T-REAP-P2`).

#### 2.2 Consumer client sends periodic `ClientMessage::Heartbeat`
- **Model:** Sonnet.
- **Files:** consumer client control loop (the `listen` loop around
  `src/secret.rs:990`+, and the read sites `src/secret.rs:1545,1620`; add the
  heartbeat tick to the **main** persistent consumer control loop, not the
  one-shot negotiation helpers).
- **Change:** same interval-send pattern as 2.1.
- **Unit tests:** `T-HB-C1` — consumer client emits `Heartbeat` within one
  interval.
- **e2e tests:** none beyond `T-REAP-C2`.
- **Done:** gates green; idle consumer survives (paired with `T-REAP-C2`).

---

### Phase 3 — Count regression guards (no behavior change)

> Locks in that counts follow the registry (D5).

#### 3.1 Assert Overview/Metrics counts exclude reaped entries
- **Model:** Haiku (assertion-only tests; logic already correct).
- **Files:** `tests/admin_test.rs` (reuse the registry-drop test pattern at
  `src/admin.rs:528`).
- **Change:** none to production code. Add `T-COUNT1`: register N secret
  entries, drop k, assert `summary().secret_tunnels == N-k` and
  `metrics()` secret count matches; `T-COUNT2`: a dropped `Registration`
  immediately disappears from `snapshot()` (guards `src/admin.rs:452`).
- **Unit tests:** `T-COUNT1`, `T-COUNT2`.
- **e2e tests:** assertion folded into `T-SEC-REAP` (count decrements).
- **Done:** gates green.

---

### Phase 4 — Frontend auto-refresh: diagnose, then close the gap

#### 4.1 Reproduce the symptom headless (diagnosis, D6)
- **Model:** Sonnet (must reason about the wiring, not just assert).
- **Files:** new `test/admin_ui/app-polling.test.js` using
  `test/admin_ui/dom-stub.js`.
- **Change:** load the `app.js` wiring path with injected fake timers + a stubbed
  `apiGet`; set hash `#/secret`; advance time by `DEFAULT_REFRESH_MS`; assert
  `apiGet` (or `refreshCurrent`) was called **again** (≥2 total). This is the
  test that the current `poller.test.js` does NOT cover (it tests the timer in
  isolation, not `setupPolling`+`router`).
- **Unit tests:** `T-FE-POLL1` (the above). Expected: **passes on current
  source** → proves the deployed binary is stale, not the code. If it FAILS,
  it has localized a real wiring bug → fix in 4.2.
- **e2e tests:** none.
- **Done:** test exists and runs under `npm test`; its result recorded in the
  PR description (pass ⇒ "redeploy current build"; fail ⇒ proceed to 4.2).

#### 4.2 Fix wiring IF 4.1 fails (conditional)
- **Model:** Sonnet.
- **Files:** `src/admin_ui/app.js`, `src/admin_ui/router.js` (whichever 4.1
  pins).
- **Change:** only if 4.1 reproduces a real defect (e.g. `setupPolling` not
  invoked on initial load, or `_refresh` unset ordering). Otherwise SKIP and
  state "no source bug; fix is redeploy" in the PR.
- **Unit tests:** `T-FE-POLL1` flips to green.
- **e2e tests:** none.
- **Done:** `T-FE-POLL1` green; gates green.

#### 4.3 Running-build + uptime banner (D7) — make staleness self-evident
- **Model:** Sonnet (Rust additive field) + Haiku (JS render).
- **Files:** `src/admin_api.rs` (add `build`/`server_uptime_secs` to the
  overview/summary view, `#[serde(default)]`), `src/admin_ui/panels/overview.js`
  (render build string + uptime).
- **Change:** surface the compiled version string (`bore <semver> - <branch> -
  <sha8>`, already embedded via build.rs) and server uptime in the Overview
  header. With auto-refresh working, uptime visibly ticks every 30 s.
- **Unit tests:** `T-FE-BUILD1` (`test/admin_ui/overview.test.js`) — overview
  renders the build string when present; tolerates absent field (old API).
  Rust: `T-API-BUILD1` (`tests/admin_test.rs`) — summary JSON includes `build`.
- **e2e tests:** extend `scripts/admin_dashboard_test.sh` (`T-DASH-BUILD`) —
  `/admin/api/v1/overview` (or summary) contains a non-empty `build`.
- **Done:** gates green; `npm test` green; e2e green.

---

### Phase 5 — Docs

#### 5.1 Document the liveness contract + frontend diagnosis
- **Model:** Haiku.
- **Files:** `docs/frontend/ADMIN_SECTIONS.md` (note auto-refresh + build banner),
  new short section in `docs/frontend/ADMIN_DASHBOARD.md` or a
  `docs/SECRET_LIVENESS.md`; add a CLAUDE.md invariant line (see §7 I-1/I-2).
- **Change:** record D1–D8, the 60 s reaper, the client-heartbeat requirement,
  the deploy-order constraint (clients→server, D4), and the BUG-B diagnosis
  outcome.
- **Unit/e2e:** none.
- **Done:** docs reflect shipped behavior; Opus final read (§F gate).

---

## 7. Invariants to preserve / add

- **I-1 (new):** every secret control loop (`serve_provider`/`serve_consumer`)
  MUST bound `control.recv()` with `SECRET_CTRL_TIMEOUT`; a mux control substream
  hides a half-open peer, so the recv-deadline — not send-error or TCP keepalive —
  is the authoritative liveness signal. Reaping returns the loop → RAII removes
  the admin entry.
- **I-2 (new):** secret provider+consumer **clients** MUST send periodic
  `ClientMessage::Heartbeat` (interval ≪ 60 s) or the server will reap them. The
  two must be deployed in order: clients first (D4 wire-compat).
- **I-3:** Overview/Metrics secret counts are a pure live-entry count; never
  filter by `active>0` (idle live tunnels have `active=0`) — D5.
- **I-4:** `carriers==1`/legacy secret path stays byte-identical except the added
  heartbeat frame + bounded recv (D8); guarded by existing `secret_test.rs`.
- **I-5:** consumer `--carriers` extra connections use `JoinCarrier`→
  `serve_carrier` (`src/server.rs:1203`) and MUST NOT register admin entries
  (only the primary `ConnectSecret` does) — preserve this; it is why 4 carriers
  ≠ 4 table rows.
- **I-6:** admin assets stay `Cache-Control: no-store` (`src/admin_http.rs:332,
  369`) so a fixed binary is never masked by a cached old bundle.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| **Wire break (D4):** old server + new client → decode error on `Heartbeat`. | Append-only variant; deploy order **clients then server** (or lockstep); single-operator deployment; documented in 5.1 + I-2. `T-WIRE1` proves round-trip. |
| Reaping a **healthy idle** tunnel at 60 s if Phase 1 ships before Phase 2 clients. | Phase ordering enforced (Phase 2 deploy precedes Phase 1); `T-REAP-C2/P2` assert heartbeating peers survive; phase note + I-2. |
| 60 s timeout too aggressive under heartbeat loss. | Client interval 15–20 s → ≥3 chances before deadline; const tunable in one place (D3). |
| BUG-B is actually a live source bug, not stale deploy. | 4.1 `T-FE-POLL1` reproduces headless and decides 4.2 vs redeploy — no speculative edit. |
| Fast tests vs real 60 s. | `#[cfg(test)]`-overridable timeout const / tokio paused time in `T-REAP-*`; real value asserted by e2e `T-SEC-REAP`. |
| netns/e2e needs a fresh binary (CLAUDE.md caveat). | `T-SEC-REAP` rebuilds `--release` (+`--features` as needed) before sudo-running, per existing harness rule. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt`, `cargo clippy -- -D warnings`,
  `cargo test`; JS: `npm test` (`node --test test/admin_ui/**/*.test.js`).
- **Unit tests:** Rust in `tests/secret_test.rs` (`T-REAP-*`, `T-HB-*`),
  `tests/admin_test.rs` (`T-COUNT*`, `T-API-BUILD1`), `src/shared.rs`/tests
  (`T-WIRE1`); JS in `test/admin_ui/` (`T-FE-POLL1`, `T-FE-BUILD1`).
- **e2e:** `scripts/admin_dashboard_test.sh` extended with `T-SEC-REAP`,
  `T-DASH-BUILD` (rebuild release first; NOPASSWD sudo per exact path).
- **Acceptance:** the §1 reference scenario passes — half-open consumer/provider
  vanish from `/secret` and the counts within 60 s (`T-SEC-REAP`), and the
  browser uptime ticks unattended (`T-FE-POLL1` + D7 banner).

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 Haiku | Haiku | — |
| 1 | 1.1 Sonnet, 1.2 Sonnet | Sonnet | **1.1 (recv-deadline on hot control path)** |
| 2 | 2.1 Sonnet, 2.2 Sonnet | Sonnet | — |
| 3 | 3.1 Haiku | Haiku | — |
| 4 | 4.1 Sonnet, 4.2 Sonnet (cond.), 4.3 Sonnet+Haiku | Sonnet | — |
| 5 | 5.1 Haiku | Haiku | **5.1 final docs read** |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical/boilerplate
> sub-phases, escalate to Opus only for the gates above. Print the model used
> per sub-task during implementation.
