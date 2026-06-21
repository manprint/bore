# Concurrent Mixed-Tunnel Stability — UDP Local-Port Collision Fix + Stress Harness

> **Status:** planning (no code written). Implementation handoff doc.
> **Authoring model:** Opus 4.8 (architecture / diagnosis).
> **Implementation models:** annotated per sub-phase (Haiku = mechanical/bulk,
> Sonnet = features/refactor/tests, Opus = review gates only).
> **Target:** the bore process stays stable with MANY concurrent tunnels of ALL
> types (public, vhost, secret, VPN 1:1 + 1:N, with routes/masquerade/forward).
> No direct-path flap. Plus a permanent mixed-load regression harness (never built
> before) and a CLAUDE.md invariant so the class of bug cannot recur.
> Minimize token usage during implementation (delegate bulk/recon to Haiku).

---

## 1. Context & problem

### Confirmed root cause (user-reproduced + code-confirmed)

The VPN direct (QUIC) path drops **only when secret `--udp` tunnels run
concurrently with the VPN on the same host.** It is a **local UDP port
collision between independent direct-path tunnels**:

- VPN binds its punch socket via `holepunch::bind_socket(nat_udp_preferred_port)`
  at **`vpn.rs:1041`**; the secret consumer binds via the **same**
  `holepunch::bind_socket(udp_port)` at **`secret.rs:1502`**. With
  `--nat-udp-preferred-port 443` (or any shared default) **both bind
  `0.0.0.0:443`**.
- `bind_socket` (**`holepunch.rs:119`**) sets **`SO_REUSEADDR` but NOT
  `SO_REUSEPORT`**, and the socket handed to quinn's `Endpoint` is **unconnected**
  (no `connect()` 4-tuple lock; quinn demuxes by QUIC connection-ID, but the
  *kernel* UDP layer does not). Two unconnected wildcard sockets on the same port
  ⇒ the kernel delivers inbound datagrams for that port to **one** socket
  (effectively last-bound-wins), starving the other.
- Both sides **re-bind a fresh socket on every direct retry**: VPN every
  `DIRECT_RETRY_INTERVAL = 30s` (**`vpn.rs:925`** → `bind_socket` `:1041`); the
  secret consumer on its upgrade retry (**`secret.rs:1502`** via
  `gather_consumer_candidates`/`spawn_upgrade_attempt`). Each re-bind makes that
  tunnel the new owner of port 443's inbound traffic → **the live peer's QUIC
  connection black-holes → idle-timeout close (≈ +22 s) → it re-punches and steals
  the port back → mutual lockstep ~30 s flap** seen in the logs (VPN *and*
  `id=dufs` both re-punching every 30 s on the server).

This generalizes: **any** two direct-path tunnels (VPN, secret, **vhost**,
**public `--udp`** — all hole-punch / bind a local UDP port) that share a
preferred local port on one host will steal each other's packets.

### What earlier recon already PROVED (do not re-investigate)

- Direct QUIC path code byte-identical since pre-frontend `3a5c87b` (keepalive
  3 s / idle 10 s correctly applied to all carriers incl `open_sibling`). The
  flap is **not** a frontend code regression in the QUIC layer; the prior
  MTU-grow theory is **demoted to coincidental** (the +10 s grow is unrelated to
  the steal).
- Admin 30 s poll on the shared control port 7835 is harmless (read-only
  snapshot). Not involved.
- VPN and secret do **not** share a `quinn::Endpoint`, STUN socket, or
  process-global state. The shared server-side `udp_providers` registry is
  namespaced (`vpn:{id}` vs raw `id`) — **safe**, not the cause.
- The single SHARED, COLLIDING resource is the **host-local UDP port** each
  direct path binds.

### Why it was never caught

No test has ever run **multiple tunnels of different types concurrently.** Every
e2e (`vpn_netns_test.sh`, `admin_dashboard_test.sh`, etc.) exercises one feature
family at a time. A heavy mixed-load scenario is a coverage gap the user
explicitly wants closed.

### Goal

(1) Fix the local UDP port collision so concurrent direct-path tunnels never
steal each other's inbound datagrams. (2) Build a comprehensive mixed-load netns
stress harness and prove the whole system is stable under many concurrent tunnels
of all types. (3) Lock it in with regression tests + a CLAUDE.md invariant.

### Reference scenario (final acceptance test)

```
ONE netns topology, ONE bore server, run for ≥ 5 minutes, all concurrent:
  • 2–3 public tunnels      (bore local)
  • 2–3 vhost tunnels       (bore local --vhost, some --udp)
  • 2–3 secret tunnels      (bore proxy/--udp consumer + provider, id A/B/C)
  • VPN 1:1                 (bore vpn listen/connect, with routes + masquerade)
  • VPN 1:N hub             (--max-clients>1, ≥2 connectors, route accept/refuse,
                             --forward-accept, NAT real@virtual on one)
  • SEVERAL tunnels pinned to the SAME --nat-udp-preferred-port (e.g. 443) to
    force the collision the user hit.

ACCEPTANCE over the 5-min window:
  - ZERO "direct path lost" / "fell back to relay" flaps on any VPN link.
  - ZERO mid-session secret/vhost "connection exited" reconnect loops.
  - Each --udp tunnel reaches and HOLDS its direct path (no perpetual re-punch).
  - Data flows on every tunnel (iperf/curl/file-xfer probe) the whole window.
  - Clean RAII teardown (no leaked nft/iptables/routes; stale-reclaim OK).
```

---

## 2. Approved design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** | **Build the mixed-load stress harness FIRST and reproduce the collision deterministically** (Phase 0), before any fix. Capture the exact kernel demux behavior (which socket receives after a re-bind) in `EVIDENCE.md`. | The harness is itself a headline deliverable and the regression guard; the fix is validated against a red→green test. |
| **D2 (PRIMARY, confirmed)** | **Stop `SO_REUSEADDR` from enabling a cross-tunnel steal.** Kernel probe (`docs/plans/udp_flap/EVIDENCE.md`) proves: two wildcard UDP sockets that BOTH set `SO_REUSEADDR` co-bind the same port and the kernel delivers inbound to the **last binder** (steal); without `SO_REUSEADDR` the second bind is cleanly refused (`EADDRINUSE`). FIX: bind the preferred port **without** `SO_REUSEADDR`; on `EADDRINUSE` fall back to an **ephemeral port (`:0`)** + `warn!` naming the contended port. | Tiny change in `holepunch.rs:110-133`. **Cross-process safe** — the OS arbitrates; no in-process registry required. The firewall-friendly preferred port serves the FIRST tunnel; later ones go ephemeral (STUN rediscovers their reflexive port — same as today's `effective_udp_port = 0` path). |
| **D3** | **Preserve same-tunnel `--auto-reconnect` rebind without `SO_REUSEADDR`.** UDP has no TIME_WAIT, so a closed socket's port frees immediately — a reconnect must **drop the old socket before binding the new one** (drop-then-bind ordering). Verify the reconnect paths (`vpn.rs`, `secret.rs`) don't hold the old socket across the rebind. | Removing `SO_REUSEADDR` is safe only if no path overlaps old+new sockets on the same port. Phase 1 audits and, if needed, reorders. |
| **D3b (optional)** | In-process port registry as a nicety: clearer diagnostics / deterministic first-wins within one process. NOT required for correctness (D2 covers cross-process via the OS). Implement only if Phase 1 evidence shows value. | Skip unless justified — avoids complexity. |
| **D4** | **Single-tunnel and `--nat-udp-preferred-port`-unset behavior stay byte-for-byte unchanged.** With one direct tunnel (or all ephemeral ports) the registry is a no-op and `bind_socket` returns exactly today's socket. | Regression test asserts the single-tunnel path is bit-identical; no perf/behavior change for the common case. |
| **D5** | **The stress harness must include `--nat-udp-preferred-port` collisions, all tunnel types, and 1:N VPN with routes/masquerade/forward-accept** — the exact mix the user named. | The acceptance scenario (§1) is encoded as named e2e tests, run on BOTH relay and direct. |
| **D6** | **Treat the stress run as a stability oracle, not just a collision test.** Any OTHER instability it surfaces (lock contention, RAII leaks, reaper false-positives, cross-tunnel counter bleed) is filed and fixed in Phase 2 with its own test. | Closes the broader "is the system stable under load" question, not only the one known bug. |

---

## 3. Target architecture

### 3.1 Process-wide local-UDP-port registry (the fix, D2)

```
            ┌──────────────────────────────────────────────┐
            │  static PORT_REGISTRY: DashMap<u16, Weak/guard>│  (holepunch.rs)
            └──────────────────────────────────────────────┘
 bind_socket(preferred):
   if preferred == 0            → bind(:0)            [ephemeral, no registry]
   else if registry.claim(preferred) succeeds → bind(:preferred) + hold guard
   else (already held in-proc)  → warn!; bind(:0)     [ephemeral fallback]
   guard drops with the socket  → release(preferred)  [RAII]
```

- Owner identity = the returned RAII guard tied to the socket's lifetime, so a
  30 s retry by the *same* tunnel (which drops its old socket then re-binds)
  re-acquires the same preferred port; a *different* tunnel never gets it while
  held.
- Cross-process collisions (two bore processes) are out of scope for the
  registry (different address spaces) — those are genuinely two OS sockets and
  the OS rejects the second bind; document as a known limit. The user's repro is
  single-process (one host running both tunnels) — covered.

### 3.2 Mixed-load stress harness (the headline deliverable, D5/D6)

New `scripts/stress_netns_test.sh` (or a mode in `vpn_netns_test.sh`):
one server netns + N client netns; launches the §1 mix; runs a traffic probe per
tunnel; scrapes both peers' logs for flap markers; asserts the §1 acceptance over
a configurable window (default 5 min, short CI mode 90 s). Reuses the existing
netns scaffolding, address allocation, and `sudo -n /abs/path` invocation rule.

### 3.3 Reuse map (do not reinvent)

| Need | Reuse | Location |
|------|-------|----------|
| Bind the punch UDP socket (the fix site) | `bind_socket` | `holepunch.rs:119` |
| VPN punch bind call | — | `vpn.rs:1041` |
| Secret punch bind call | — | `secret.rs:1502` |
| Secret ephemeral-remap precedent (`effective_udp_port = 0`) | `preferred_port_remapped` path | `secret.rs:~1141-1175` |
| VPN direct retry grid | `DIRECT_RETRY_INTERVAL`, `direct_upgrade_task` | `vpn.rs:925-995` |
| Secret upgrade retry / candidate gather | `spawn_upgrade_attempt`, `gather_consumer_candidates` | `secret.rs:~1202,1502` |
| QUIC endpoint (per-tunnel, unconnected socket) | `client_endpoint` / `server_endpoint` | `holepunch.rs:1544,1561` |
| STUN reflexive discovery on the same socket | `discover_reflexive` | `holepunch.rs:687` |
| vhost/public-`--udp` direct pool (also binds ports) | `DirectPool` / `PublicDirectEntry` | per CLAUDE.md / `holepunch.rs`, `server.rs` |
| Direct-path flap markers to grep | `"direct path lost"`, `"fell back to relay"`, `"bridge switched to direct path"` | `vpn.rs:6535,6558,6577` |
| netns harness (scaffold, sudo rule, rebuild caveat) | `scripts/vpn_netns_test.sh` | `scripts/` |
| Socket options helper (REUSEADDR today; add connect/REUSEPORT) | `bind_socket` + `socket2` usage | `holepunch.rs:~110-130` |

---

## 4. New interface (CLI flags / config)

No new user flags required. Behavior change is internal (port arbitration). If
Phase 0 shows users need to force the preferred port for a *specific* tunnel when
several compete, add `--require-udp-port` (fail rather than remap) — only if
demanded by evidence; default remains "first wins, rest ephemeral + warn".

The `warn!` on ephemeral fallback must name the colliding preferred port and the
two tunnel identities so the operator understands why a tunnel is on an ephemeral
port (observability, not silent).

---

## 5. New protocol / data structures

None. The fix is host-local socket arbitration; nothing crosses the wire
differently. Ephemeral fallback already interops (STUN advertises whatever local
port was bound; the broker forwards real reflexive candidates regardless of port).
Confirm no code assumes the local punch port equals `nat_udp_preferred_port`
after bind (grep for direct uses of the preferred port post-bind).

---

## 6. Implementation phases

**Global gates (every sub-phase):** `cargo fmt --all -- --check`,
`cargo clippy --all-targets --features vpn,udp -- -D warnings`,
`cargo test --features vpn,udp`. **Zero regressions.** netns:
`sudo -n /mnt/fabio/dati/Git/Github-manprint/bore-forked/scripts/<harness>.sh`.
**Rebuild release `--features vpn` (as your user, not root) before any sudo netns
run** (harness refuses a binary older than `src/`). Print the model per sub-task.

---

### Phase 0 — Mixed-load stress harness + deterministic repro (NO fix code)

> Builds the never-existed mixed-load test and reproduces the collision red.
> Gate: no fix lands until `EVIDENCE.md` records the exact demux behavior.

#### 0.1 Build the mixed-load netns stress harness
- **Model:** **Opus review (topology design) → Sonnet implements.**
- **Files:** new `scripts/stress_netns_test.sh`; reuse `scripts/vpn_netns_test.sh`
  scaffolding (netns setup, addr alloc, sudo rule).
- **Change:** Stand up the full §1 mix in one server netns + N client netns:
  2–3 public, 2–3 vhost (some `--udp`), 2–3 secret (ids A/B/C, `--udp`), VPN 1:1
  (routes+masquerade), VPN 1:N hub (≥2 connectors, accept/refuse routes,
  `--forward-accept`, one `real@virtual` NAT). **Pin ≥2 `--udp` tunnels to the
  SAME `--nat-udp-preferred-port`** to force the collision. Add a per-tunnel
  traffic probe (curl/iperf3/file-xfer). Scrape both peers' logs; expose a
  `WINDOW` env (90 s CI / 300 s full).
- **Unit tests:** none (harness).
- **e2e tests:** `T-STRESS-MIX` (the §1 acceptance, expected RED here),
  `T-STRESS-PORTCLASH` (≥2 `--udp` tunnels on the same preferred port — RED:
  asserts the flap reproduces).
- **Done:** harness runs end-to-end; `T-STRESS-PORTCLASH` reproduces the
  establish→die→re-punch flap deterministically; logs saved under
  `docs/plans/udp_flap/`.

#### 0.2 Confirm the kernel demux mechanism (Opus review gate)
- **Model:** **Opus review → Sonnet implements.**
- **Files:** temporary instrumentation in `holepunch.rs:119` `bind_socket`
  (log bound local port + a socket id) and at the direct-downlink death
  (`vpn.rs:6526`, log `close_reason`).
- **Change:** Prove that after a second tunnel binds the shared port, inbound
  QUIC for the first stops (idle close), and that with distinct ports it does
  not. Record whether `SO_REUSEADDR` alone allowed the dual wildcard bind on the
  test kernel, and whether a `connect()`-ed socket (D3 probe) demuxes correctly.
- **Unit tests:** none (diagnostic).
- **e2e tests:** reuse `T-STRESS-PORTCLASH` with instrumentation.
- **Done:** `EVIDENCE.md` states the exact mechanism + whether D3 (connected
  socket / REUSEPORT) is needed in addition to D2. **Opus signs off.**

---

### Phase 1 — Fix the local UDP port collision (D2 + D3)

> Independently shippable. Flips `T-STRESS-PORTCLASH` red→green. Mechanism already
> confirmed by the kernel probe (Phase 0 done); this is the code change.

#### 1.1 Drop `SO_REUSEADDR`; EADDRINUSE → ephemeral fallback + warn
- **Model:** **Opus review → Sonnet implements** (socket lifecycle, hot path).
- **Files:** `src/holepunch.rs` `bind_socket` (`:110-133`).
- **Change:** Per D2 — remove the `set_reuse_address(true)` for fixed ports.
  Attempt `bind(preferred)`; on `EADDRINUSE` (and only that errno) retry with
  `bind(:0)` and `warn!` naming the contended preferred port + that the tunnel is
  now on an ephemeral port (so the operator understands the degraded-to-relay
  risk behind a strict egress firewall). `port == 0` path unchanged (D4).
- **Unit tests:** `bind_socket_second_same_port_falls_back_to_ephemeral`
  (bind A on a port, bind B on the same → B gets a different, non-zero port, no
  panic); `bind_socket_ephemeral_unchanged` (`:0` returns a normal socket);
  `bind_socket_fixed_port_when_free` (sole binder keeps the preferred port).
- **e2e tests:** `T-STRESS-PORTCLASH` flips to **GREEN** (both same-port `--udp`
  tunnels hold their direct path ≥ 90 s, zero flap; the demoted tunnel either
  takes an ephemeral port and still reaches direct, or cleanly stays on relay).
- **Done:** gates green; port-clash test green; single-tunnel path bit-identical.

#### 1.2 Audit + fix same-tunnel reconnect ordering (D3)
- **Model:** **Opus review → Sonnet implements.**
- **Files:** `src/vpn.rs` direct retry (`:925-995`, `:1041`), `src/secret.rs`
  upgrade retry (`:1502` and `spawn_upgrade_attempt`), any other `bind_socket`
  caller that reconnects on a fixed port.
- **Change:** Ensure each reconnect **drops its previous socket before** binding
  the new one (no overlap), so removing `SO_REUSEADDR` does not make a legitimate
  same-tunnel rebind fail. Reorder if any path constructs the new socket first.
- **Unit tests:** none new if ordering already correct; otherwise a focused test
  of the reconnect sequence.
- **e2e tests:** `T-RECONNECT-FIXEDPORT` — a single `--udp --nat-udp-preferred-port`
  tunnel survives an `--auto-reconnect` cycle and re-binds the SAME port (no
  ephemeral downgrade when uncontended).
- **Done:** auto-reconnect on a fixed port still reuses that port; gates green.

#### 1.3 (optional, D3b) In-process registry — only if justified
- **Model:** Sonnet. Skip unless 1.1/1.2 evidence shows a same-process need.

---

### Phase 2 — Full mixed-load stability (D6)

> Run the big harness; fix anything else it surfaces. Each finding gets a test.

#### 2.1 Make `T-STRESS-MIX` green and hardening for any other instability
- **Model:** **Opus (triage findings) → Sonnet implements; Haiku for mechanical.**
- **Files:** as dictated by findings (candidates: `secret.rs` reaper false-
  positive under load; DashMap contention in `vpn_server`/`server` registries;
  RAII teardown leaks under many concurrent `NetConfig`s; cross-tunnel counter
  bleed in `shared::CountingStream`).
- **Change:** Triage every flap/leak/error the 5-min mixed run produces; fix at
  the source; add a targeted regression test per fix. If none surface beyond
  Phase 1, record "no further instability under T-STRESS-MIX".
- **Unit tests:** one per finding (named).
- **e2e tests:** `T-STRESS-MIX` GREEN over the full window (relay AND direct);
  clean RAII teardown asserted (no leaked nft/iptables/routes).
- **Done:** `T-STRESS-MIX` green ≥ 5 min; teardown clean; gates green.

---

### Phase 3 — Documentation & invariant (explicit user ask)

#### 3.1 Assessment + CLAUDE.md invariant
- **Model:** Haiku (prose) → **Opus final read.**
- **Files:** `docs/vpn/CONCURRENT_TUNNEL_STABILITY_ASSESSMENT.md` (new),
  `CLAUDE.md` (invariant), `EVIDENCE.md` (final).
- **Change:** Document the root cause, the lockstep-steal signature, the
  registry fix, and the new harness. Add a CLAUDE.md invariant, e.g.:
  *"Concurrent direct-path tunnels (VPN/secret/vhost/public `--udp`) must NEVER
  share a local UDP punch port in one process: `bind_socket` arbitrates via a
  process-wide registry (first claimant keeps `--nat-udp-preferred-port`, others
  get an ephemeral port + warn). Two unconnected wildcard UDP sockets on the same
  port make the kernel deliver inbound to the last-bound socket, starving the
  live peer's QUIC connection → idle close → mutual ~30 s flap. Regression:
  `T-STRESS-PORTCLASH` + `T-STRESS-MIX` in `scripts/stress_netns_test.sh`. The
  direct QUIC layer itself is byte-identical since `3a5c87b` — do not re-bisect
  it."* Note that all multi-type concurrency is now covered by the stress harness.
- **Done:** Opus-read; invariant in CLAUDE.md; assessment committed.

---

## 7. Invariants to preserve / add

- **I-1 (new):** No two live direct-path sockets in one process bind the same
  local UDP port; `bind_socket` enforces it (registry + RAII). (Phase 1 /
  `T-STRESS-PORTCLASH`.)
- **I-2 (new):** A `--nat-udp-preferred-port` collision degrades gracefully to an
  ephemeral port **with a named warning**, never silently and never by stealing a
  peer's packets.
- **I-3 (preserve):** Single direct tunnel and `port==0` paths are byte-for-byte
  unchanged (registry no-op). (D4 regression test.)
- **I-4 (preserve):** Per-tunnel QUIC endpoints/sockets stay independent; no
  process-global endpoint introduced.
- **I-5 (new, coverage):** System stability is verified under concurrent mixed
  load (all tunnel types + 1:N VPN + routes/masquerade/forward) by
  `T-STRESS-MIX`; this scenario is part of the e2e suite from now on.

---

## 8. Risk register

| Risk | Mitigation |
|------|-----------|
| Fix only masks the symptom; another shared resource also collides | Phase 0 stress harness exercises ALL types together (D6); Phase 2 triages anything else it surfaces. |
| Ephemeral fallback breaks hole-punch for the demoted tunnel | STUN rediscovers the reflexive port per punch (existing `effective_udp_port=0` precedent); `T-STRESS-PORTCLASH` asserts the demoted tunnel still reaches direct. |
| Registry RAII race (port released then reclaimed by the wrong owner mid-retry) | Guard tied to socket lifetime + unit tests `port_registry_*`; Opus gate at 1.1. |
| `SO_REUSEADDR` dual-bind behavior is kernel-version-dependent → repro flaky | 0.2 records the exact kernel behavior; D3 connected-socket demux as belt-and-suspenders if needed. |
| Cross-PROCESS port collision (two bore processes) not fixed by an in-proc registry | Documented as out-of-scope (OS rejects the second bind); the user's repro is single-process. |
| Big stress harness is slow/flaky in CI | `WINDOW` env: 90 s CI mode vs 5 min full; run heavy mode under sudo locally. |
| Stale binary under sudo netns | Rebuild `--features vpn` as user before `sudo -n /abs/path/...`. |

---

## 9. Verification summary

- **Gates (every sub-phase):** `cargo fmt --all -- --check`,
  `cargo clippy --all-targets --features vpn,udp -- -D warnings`,
  `cargo test --features vpn,udp`.
- **Unit tests:** `port_registry_first_wins_rest_ephemeral`,
  `port_registry_releases_on_drop`, `port_registry_same_owner_reacquires`,
  `bind_socket_ephemeral_is_unchanged`, (`connected_socket_demux_isolated` if D3),
  plus one per Phase-2 finding.
- **e2e (netns, sudo):** `T-STRESS-PORTCLASH`, `T-STRESS-MIX` in
  `scripts/stress_netns_test.sh` (run on BOTH relay and direct). Rebuild release
  `--features vpn` (as user) before `sudo -n /abs/path/...`.
- **Acceptance:** the §1 reference scenario passes — the full mixed load runs
  ≥ 5 min with **zero direct-path flaps**, data on every tunnel, clean teardown
  (`T-STRESS-MIX`); the same-port collision specifically is gone
  (`T-STRESS-PORTCLASH`).

## 10. Model-assignment summary

| Phase | Sub-phases by model | Primary model | Opus review gate |
|-------|---------------------|---------------|------------------|
| 0 | 0.1 **Opus→Sonnet** · 0.2 **Opus→Sonnet** | Sonnet | **0.1 topology · 0.2 mechanism verdict** |
| 1 | 1.1 **Opus→Sonnet** · 1.2 **Opus→Sonnet** (cond.) | Sonnet | **1.1 (registry/lifecycle) · 1.2** |
| 2 | 2.1 **Opus triage → Sonnet/Haiku** | Sonnet | **2.1 (findings triage)** |
| 3 | 3.1 Haiku → **Opus** read | Haiku | **3.1 (final doc read)** |

> Start Sonnet; Haiku for harness bulk/recon/prose; Opus only at the marked gates
> (topology, mechanism verdict, registry lifecycle, findings triage, final doc).
> Print the model used per sub-task during implementation.
