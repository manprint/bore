# Phase 02 — bound the direct-path receive window

> **Motivating finding:** F-13 (`--udp` vhost memory is unbounded per tunnel and
> can take the server down) | also closes **open question 6**
> **Kind:** stability defect | **Effort:** small operationally, medium properly
> **Prerequisite:** none. **Blocks:** Phase 03 (DEC-VE1/DEC-VE8)

## The defect, precisely

The QUIC direct receive window is a **per-connection ceiling with no
server-wide budget**. Measured (F-13, §2.13 G8/G9):

| condition | server RSS |
| --- | --- |
| 512 concurrent connections, TCP relay | 95.1 MiB |
| 512 concurrent connections, QUIC direct | 424 MiB (**4.5×**) |
| 32 slow readers on **one** `--udp` tunnel | **536.8 MiB** on a 903 MiB host |

At 536.8 MiB the server timed out two requests, failed a registration, and made
an unrelated tunnel — the operator's own `tennis1` — lose its control connection
and reconnect (`uptime_secs` 12953 → 19). No OOM kill, `RestartCount 0`, and RSS
recovered to 33.6 MB afterwards, so this is exhaustion and recovery rather than
a leak. It was nonetheless the only finding in the campaign that degraded the
**whole server** from a single client's behaviour.

The arithmetic: `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` is 256 MiB
(`src/shared.rs:237`), applied per direct QUIC connection
(`src/holepunch.rs:3061`, `cfg.receive_window(...)`). One tunnel at
`--carriers 1` can therefore occupy 256 MiB, `--carriers 4` a full GiB, and
**ten tunnels have no bound at all**. On the announced 2 GiB IONOS VPS the same
shape needs roughly twice as many slow readers as staging — still reachable by
one client on a bad network.

The window is a *ceiling, not a reservation* — a healthy tunnel buffers
approximately zero — which is exactly why this went unnoticed. It only fills
when a public reader stops draining, which is precisely what a browser pausing
assets, or a mobile client on a bad link, does.

## What must not be broken

`CLAUDE.md` records the invariant this window exists to satisfy:

> **Direct `--udp` connection_receive_window MUST stay >> stream_receive_window**
> (256 MiB vs 16 MiB). … `conn/stream` = how many stalled streams starve EVERY
> other stream. 64 MiB → ~4 → the carriers=1 vhost stall; 256 MiB tolerates ~16.
> Do NOT drop conn window toward stream window, do NOT raise stream window to
> meet it (regresses single-stream throughput).

So the ratio (currently 16:1) is load-bearing and the fix cannot simply shrink
the connection window. A second campaign result matters here: **the G9 stall
cliff is gone** — fast requests stayed at 12–17 ms at every slow-reader count up
to 32 — so the 256/16 MiB pair *works*. It just pays for its tolerance in
buffered bytes, with no server-wide limit on the bill.

---

## 2.1 — Fix `ConfigView`'s misreported defaults (found while planning)

**Files:** `src/server.rs` ≈ 436-439

The admin config view is initialized with hardcoded placeholder strings that no
longer match the real constants:

```rust
udp_stream_receive_window: "16MiB".into(),      // DIRECT_QUIC_STREAM_RECEIVE_WINDOW = 16 MiB       ✓
udp_connection_receive_window: "16MiB".into(),  // DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW = 256 MiB  ✗
udp_send_window: "64MiB".into(),                // DIRECT_QUIC_SEND_WINDOW = 256 MiB                ✗
udp_max_streams: 4096,                          // MAX_DIRECT_STREAMS = 4096                        ✓
```

Two of the four are stale (verified against `src/shared.rs:217/237/243/253`).
The consequential one is the connection window: a default-configured server
reports a **1:1** connection-to-stream ratio through `/admin/api/v1/config` —
i.e. the admin API advertises a configuration that violates the invariant above
and that would produce the carriers=1 stall if it were true. Staging reported
256 MiB only because its YAML sets it explicitly.

Note also that staging's `udp_max_streams: 8192` is a YAML override, not the
default; the default really is 4096.

**Fix:** derive these strings from the `DIRECT_QUIC_*` constants (and
`UdpDirectTuning::default()`) rather than restating them. This is a variant of
F-6 (the admin config view does not reflect reality) and is the cheapest item
in the whole plan.

**Gate:** a unit test asserting the `ConfigView` defaults equal the formatted
`UdpDirectTuning::default()` values, so the two can never drift again.
Red-check by restoring one literal.

---

## 2.2 — Ship a small-host default profile

**Files:** `src/shared.rs` (`UdpDirectTuning`), `src/main.rs`
(`parse_udp_tuning`, ≈ 2784), `README.md`

The cheapest real remediation, and the one to do first because it needs no new
mechanism.

- Add a documented sizing rule: the worst-case direct-path memory a server can
  be asked to hold is
  `tunnels × carriers × connection_receive_window`.
  At the defaults that is 256 MiB per tunnel-carrier.
- Provide a way to select a smaller profile without hand-computing four
  numbers. Preferred shape: a single `--udp-memory-budget <SIZE>` that derives
  the per-connection window from the budget and the configured
  `--max-carriers`, **preserving the 16:1 ratio**. Rejected alternative: four
  separate flags, which already exist and which nobody sets correctly.
- On a host with less RAM than the worst case, `warn!` at startup with the
  computed figure and the flag that fixes it. Never silently clamp — the
  project's convention throughout (`I-2`, and the `configure_udp_socket_buffers`
  clamp warning) is to warn with concrete remediation.

**Decision:** a budget flag rather than new smaller defaults. Changing the
default would regress the carriers=1 stall fix that `CLAUDE.md` pins and that
commit 04aac76 shipped, on every existing well-provisioned deployment.

**Gate:** unit tests over the budget → window derivation, including that the
16:1 ratio is preserved at every budget and that the result is clamped to a
floor below which the ratio can no longer hold.

---

## 2.3 — Server-wide budget (the real fix)

**Files:** `src/vhost.rs` (`DirectPool`, ≈ 459-525), `src/shared.rs` (`UdpDirectTuning`, 751-776), `src/holepunch.rs`
(≈ 3055-3075 where the transport config is built), `src/server.rs`

The per-connection ceiling is correct; what is missing is an aggregate bound.
Two candidate mechanisms, to be chosen by measurement in 2.4:

- **(a) Admission control.** A server-wide semaphore of "direct connection
  slots" sized from the budget. A provider requesting `--udp --carriers 4` on a
  server with two slots left gets two direct carriers and falls back to the warm
  TCP relay for the rest. This is already the *established* behaviour pattern —
  `CLAUDE.md`: "Missing/dead/open-failed QUIC falls back for the SAME channel to
  warm TCP, never kills the outer SSH session" — so the fallback path exists and
  is tested.
- **(b) Dynamic window division.** Divide the budget across live direct
  connections, shrinking each connection's window as more appear. Rejected as
  the primary mechanism: quinn's `receive_window` is set at connection
  construction, so this means either re-creating connections or accepting that
  early connections keep large windows — and it silently degrades the
  concurrency tolerance the 16:1 ratio buys.

**Recommendation: (a).** It bounds the worst case exactly, it degrades along a
path that already exists and is tested, and it is observable — a counter of
"direct admissions refused for budget" is a number an operator can act on,
whereas a silently shrinking window is not.

**Invariants**
- `--udp` off ⇒ no new code on the path. The relay is untouched.
- Refusing a direct slot must **never** fail a tunnel or a request; it falls
  back to relay. F-8 says the relay is the faster transport on a clean path
  anyway, so this degradation is not even a performance loss there.
- The refusal must be logged and counted, not silent (project convention).

**Gates**
- A unit test that N+1 direct admissions against a budget of N yields exactly N
  admissions and one relay fallback.
- An integration test that the fallback tunnel still serves correct responses.
- Red-check: with the budget check removed, the admission count exceeds N.

---

## 2.4 — Validate, and answer open question 6

**Harness:** `scripts/vhost_udp_concurrency_repro.sh` (existing gate R3),
`scripts/perf/vhost_remote_stability.sh g8` and `g9`

Open question 6 as written in the evidence document:

> **Does the cliff stay away at smaller QUIC windows?** F-13's remedy 1 assumes
> `64 MiB / 8 MiB` keeps the 8:1 ratio that prevents the stall cliff. That has
> to be measured, not assumed — rerun G9 against a private server with the
> smaller windows.

Run G9's slow-reader ladder (4/8/16/24/32) at 256/16, 128/8 and 64/8 against a
**private** server, since staging is frozen and must not be touched. Record
per rung: fast-request latency, server RSS, and whether a stall appears.
Acceptance is that the chosen small-host profile shows no stall cliff at the
same rungs where the default profile shows none.

Two campaign rules apply to this run specifically (§9.5, §9.6):
- Cap the concurrency to what the host can hold. G9 at 32 slow readers is what
  caused the collateral damage on a 903 MiB host in the first place.
- Check the operator's own tunnels before and after. G9 knocked `tennis1`
  offline once already.

**Gate:** the G9 ladder is added to the repo as a scripted regression with the
private-server bootstrap `vhost_header_injection_ab.sh` already demonstrates,
so it needs no deployment access.

---

## 2.5 — Documentation

**Files:** `README.md`, `docs/VHOST_UDP_CONCURRENCY_FIX.md`, `CLAUDE.md`

- The sizing rule, stated as arithmetic:
  `tunnels × carriers × connection_receive_window` ≤ available RAM, with the
  2 GiB IONOS worked example.
- The new budget flag, in `README.md` (single source of truth) with an example.
- `CLAUDE.md`: extend the existing `--udp` window invariant with the aggregate
  bound and the reason the per-connection ratio still may not be reduced.
- `docs/VHOST_UDP_CONCURRENCY_FIX.md` gains the F-13 measurements: the 4.5×
  relay-versus-QUIC RSS ratio and the 536.8 MiB single-tunnel figure.

## Phase acceptance

1. Worst-case direct-path memory is bounded by an operator-visible number, and
   exceeding it produces relay fallback plus a counter — never a failed request.
2. `/admin/api/v1/config` reports the windows actually in force on a
   default-configured server (2.1 regression test pins it).
3. The G9 ladder is a repo regression, runs against a private server, and shows
   no stall cliff on the small-host profile — open question 6 answered with a
   measurement rather than an assumption.
4. Full regression green, including `--features vpn` (the direct path is shared
   with VPN and secret — `spawn_direct`/`DirectPool` must not fork).
