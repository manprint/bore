# Admin UI Polling Illegal Invocation Fix — Design & Implementation Plan

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = review gates only).
> **Target:** `/admin/status` auto-refreshes in real browsers with no
> `TypeError: Illegal invocation`; active panel refetches on cadence without
> manual reload. Minimize token usage during implementation (delegate mechanical
> sub-phases to Haiku).

---

## 1. Context & problem

`src/admin_ui/app.js:11-66` owns one app-wide poller: it creates
`createPoller(() => refreshCurrent())`, re-arms on load in `setupPolling()`, and
re-arms again on `hashchange`. `src/admin_ui/poller.js:13-25` is the bug: the
module copies global timer functions into a plain object and then calls them as
`timers.setInterval(...)` / `timers.clearInterval(...)`. In a browser those are
host methods, so the plain-object receiver is wrong and the bootstrap path dies
with `TypeError: Illegal invocation` at `poller.js:24:33`. `src/admin_ui/router.js:51-69`
already has the correct `refreshCurrent()` hook; the refresh wiring is not the
problem.

Current tests miss the browser seam. `test/admin_ui/poller.test.js:9-56` uses
Node-friendly injected timer stubs, so it only proves callback/restart logic.
`test/admin_ui/app-polling.test.js:57-170` also uses permissive arrow stubs, so
it does not reproduce browser receiver semantics. Node timers do not enforce
browser host receivers, so the bug can pass CI and still break in Chrome/Firefox.

### Goal

Fix scheduler default path so `/admin/status` auto-refreshes in real browsers,
with no console `Illegal invocation`, no dead poller, and no manual reload.
Keep injected timer tests working, keep `refreshMs <= 0` static panels static,
and do not touch app/router call sites unless a test forces it.

### Reference scenario (final acceptance test)

Open `/admin/status#/secret` in a real browser with auth token loaded:

```
- console: no `TypeError: Illegal invocation`
- load: one initial GET to /admin/api/v1/secret
- after ~2x refreshMs: same endpoint fetched again without reload
- hashchange to /admin/status#/overview: poller re-arms and GET /admin/api/v1/summary happens
```

If the browser path still throws on bootstrap, poller is still broken. If the
load arm works but hashchange/restart fails, clearInterval path is still wrong.

---

## 2. Approved design decisions

Every non-obvious choice gets a row. Consequence column is what makes it
actionable downstream.

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | Default timers in `poller.js` become browser-safe wrappers around `globalThis.setInterval` / `globalThis.clearInterval`, not copied host methods on plain object. | Browser host timers keep correct receiver; `Illegal invocation` goes away; Node/test injection stays possible. |
| **D2** | `createPoller(refreshFn, timers?)` API stays source-compatible; `app.js` call site stays unchanged. | Fix is centralized in scheduler; no cascade into app/router/bootstrap wiring. |
| **D3** | Regression must simulate browser receiver semantics by monkeypatching global timers, not by passing plain fake functions. | Tests catch the real browser failure and fail on old code, even though Node timers themselves are permissive. |
| **D4** | Full `app.js` bootstrap/import remains the end-to-end smoke. | Load-arm, tick, and hashchange are all covered together; startup regressions cannot hide behind a green unit test. |
| **D5** | Docs record both the fix and the browser-like regression IDs. | Future readers know the contract, the exact tests, and the manual/browser validation rule. |
| **D6** | `refreshMs <= 0` semantics stay byte-identical. | Config/static panels still do not poll; only broken browser receiver binding changes. |

---

## 3. Target architecture

### 3.1 Scheduler ownership

`app.js` still decides *when* to arm polling (`setupPolling()` reads the active
panel `refreshMs`); `router.refreshCurrent()` still decides *what* to refresh;
`poller.js` only owns timer lifecycle. The fix stays in `poller.js` because the
bug is in the default timer adapter, not in route selection.

### 3.2 Browser-safe default timer adapter

```
app.js bootstrap / hashchange
  -> setupPolling()
       -> createPoller(refreshCurrent)
            default timers = browser-safe wrappers
              setInterval: (...args) => globalThis.setInterval(...args)
              clearInterval: (...args) => globalThis.clearInterval(...args)
            start(refreshMs)
              -> clear old handle
              -> if refreshMs > 0: setInterval(() => refreshFn(), refreshMs)
```

Why this shape: calling `globalThis.setInterval(...)` keeps browser host
receiver correct; calling `timers.setInterval(...)` on a copied plain object does
not. The injectable `timers` override remains available for unit tests.

### 3.3 Test harness shape

```
unit (poller.test.js):
  monkeypatch globalThis.setInterval / clearInterval
    -> receiver-checking host-like fns
  call createPoller() with default timers path
    -> old code throws Illegal invocation
    -> fixed code schedules + restarts + stops cleanly

e2e-ish (app-polling.test.js):
  install same browser-like timer stubs before importing app.js
    -> import must resolve
    -> load arm / tick / hashchange re-arm all still work
```

### 3.x Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Poller lifecycle + restart/stop semantics | `createPoller` / `start` / `stop` | `src/admin_ui/poller.js:13-36` |
| App bootstrap seam | poller singleton + `setupPolling()` | `src/admin_ui/app.js:11-66` |
| Re-fetch hook | `refreshCurrent()` | `src/admin_ui/router.js:51-69` |
| Isolated timer harness | `poller.test.js` | `test/admin_ui/poller.test.js:9-56` |
| Full bootstrap harness | `app-polling.test.js` | `test/admin_ui/app-polling.test.js:57-170` |
| Headless DOM fixture | `dom-stub.js` | `test/admin_ui/dom-stub.js:1-183` |
| Frontend JS test command | npm test script | `package.json:6-8` |
| Stale-build guard for browser/manual smoke | binary newer-than-src check | `scripts/admin_dashboard_test.sh:20-29` |
| Release note / fix log | changelog fixed section | `docs/CHANGELOG.md:184-194` |
| Frontend test docs | frontend test list + rebuild note | `docs/frontend/ADMIN_DASHBOARD.md:292-317` |

---

## 4. New interface (CLI flags / API / config)

None. No new CLI flags, API fields, or config knobs. `createPoller(refreshFn,
timers?)` stays source-compatible; only internal default timer implementation
changes.

---

## 5. New protocol / data structures

N/A — no wire, persisted-schema, or public-API change. Fix is frontend runtime
only.

---

## 6. Implementation phases

**Global rules:** tests first or alongside; every sub-phase passes
`cargo fmt --all --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
`cargo test --all-features`, and `npm test`. Because `src/admin_ui/*` is embedded
at build time, run `cargo build --release --features vpn` before any
browser/manual validation. Zero regressions. Print model used per sub-task.

Each sub-phase lists: **Model · Files · Change · Unit tests · e2e tests · Done.**

---

### Phase 0 — N/A

> No pure-additive scaffold needed. Bug is localized; fix + regressions land in
> one shot.

---

### Phase 1 — Browser-safe poller + regressions

> Runtime bugfix. Independently shippable after 1.1 + 1.2. No app/router call
> site change.

#### 1.1 Make default timer adapter browser-safe
- **Model:** Sonnet
- **Files:** `src/admin_ui/poller.js:13-36`, `test/admin_ui/poller.test.js:9-56`
- **Change:** Replace copied-host-function default `timers` object with browser-
  safe wrappers around `globalThis.setInterval` and `globalThis.clearInterval`
  (or equivalent helper) so the default path no longer depends on plain-object
  receiver binding. Keep `timers` injection path and `refreshMs <= 0` semantics
  unchanged. Add a new regression that monkeypatches `globalThis.setInterval`
  and `globalThis.clearInterval` to receiver-checking host-like functions; the
  old code must fail with `Illegal invocation`, the fixed code must schedule,
  restart, and stop cleanly.
- **Unit tests:** `T-FE-POLL0` — default path with browser-like global timers
  does not throw, `start(3000)` arms, restart clears previous handle, `stop()`
  clears current handle.
- **e2e tests:** none.
- **Done-criteria:** `npm test` green; regression fails on old code and passes
  after fix; injected-timer restart/disable behavior still passes; no
  `app.js`/`router.js` edits required.

#### 1.2 Harden app bootstrap smoke against browser receiver semantics
- **Model:** Haiku
- **Files:** `test/admin_ui/app-polling.test.js:57-170`
- **Change:** Update `installHarness()` timer stubs from permissive arrow fns to
  receiver-checking host-like fns, and restore original globals in cleanup.
  Import real `src/admin_ui/app.js` under those stubs and keep existing asserts:
  load arm with active panel `refreshMs`, tick refetch of same endpoint,
  hashchange re-arm with new endpoint. This proves `setupPolling()` +
  `refreshCurrent()` still work when browser timer receivers are enforced.
- **Unit tests:** none new; isolated receiver bug is covered by `T-FE-POLL0`.
- **e2e tests:** `T-FE-POLL1` — full `app.js` bootstrap/hashchange smoke under
  browser-like timer receiver semantics. `M-FE-POLL2` — optional/manual browser
  smoke after fresh build: open `/admin/status#/secret`, wait one interval, no
  console `Illegal invocation`, network shows repeat fetch.
- **Done-criteria:** importing `app.js` no longer throws/rejects under the host-
  like timer stubs; tick and route-change re-arm still pass; browser/manual smoke
  only after `cargo build --release --features vpn` because assets are embedded.

---

### Phase 2 — Docs + release note

> **Opus review gate:** final docs read. Docs are part of deliverable, not
> optional.

#### 2.1 Update changelog + frontend test docs
- **Model:** Haiku drafts · **Opus review gate**
- **Files:** `docs/CHANGELOG.md:184-194`, `docs/frontend/ADMIN_DASHBOARD.md:292-317`
- **Change:** Add one `Fixed` changelog entry summarizing the browser-safe timer
  adapter and the `Illegal invocation` regression. Update frontend docs so the
  test list names `T-FE-POLL0` and `T-FE-POLL1`, and the rebuild note/manual
  smoke note call out the embedded-JS rebuild rule and the browser receiver
  contract. Keep wording explicit: no public interface or protocol changed.
- **Unit tests:** none (docs only).
- **e2e tests:** none (docs only); doc text must reference `M-FE-POLL2` if manual
  browser smoke is kept.
- **Done-criteria:** changelog and frontend docs mention the fix, the test IDs,
  and the rebuild caveat; Opus final read approves wording; no behavior claims
  drift from implementation.

---

## 7. Invariants to preserve / add

- **I-1:** `refreshMs <= 0` still means no polling, byte-identical.
- **I-2:** `createPoller(refreshFn, timers?)` stays source-compatible; tests may
  override timers, app.js does not need to.
- **I-3:** both `setInterval` and `clearInterval` paths are covered; restart/stop
  must still clear prior handle.
- **I-4:** default path must be browser-safe; Node permissiveness is not enough.
- **I-5:** `src/admin_ui/*` edits require a fresh build before any browser/manual
  smoke because assets are compile-time embedded.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Node test runner still masks browser-only receiver bug | Patch global timers with receiver-checking host-like fns in both regression tests; old code throws immediately. |
| Fix only covers load arm, not restart/clear path | Unit test restarts/stops; app smoke re-arms on hashchange too. |
| Global timer monkeypatch leaks across tests | Restore originals in `finally` / cleanup in `app-polling.test.js` and keep patch scope local. |
| Manual browser validation uses stale embedded JS bundle | Rebuild with `cargo build --release --features vpn` before browser smoke; harness already enforces stale-build guard. |
| Overfitting to one route | Bootstrap smoke uses `#/secret` and `#/overview`, so both same-panel refresh and route-change re-arm are covered. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all --check`, `cargo clippy --all-features --all-targets -- -D warnings`, `cargo test --all-features`, `npm test` (`node --test "test/admin_ui/**/*.test.js"`).
- **Unit tests:** `test/admin_ui/poller.test.js` (`T-FE-POLL0`) for browser-
  safe default timers and restart/stop semantics.
- **e2e / integration:** `test/admin_ui/app-polling.test.js` (`T-FE-POLL1`) for
  full app bootstrap, tick refetch, and hashchange re-arm under browser-like
  timer receivers.
- **Browser smoke:** `M-FE-POLL2` — after `cargo build --release --features vpn`,
  open `/admin/status#/secret`, confirm no console `Illegal invocation`, and
  confirm a repeat fetch without reload.
- **Acceptance:** §1 reference scenario is proven by `T-FE-POLL0`, `T-FE-POLL1`,
  and `M-FE-POLL2`.

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | N/A | — | — |
| 1 | 1.1 Sonnet · 1.2 Haiku | Sonnet | — |
| 2 | 2.1 Haiku | Haiku | 2.1 (final docs read) |

> Rule of thumb: start Sonnet, drop to Haiku for mechanical/browser-harness and
> docs sub-phases, escalate to Opus only for the final docs read. Print the
> model used per sub-task during implementation.
