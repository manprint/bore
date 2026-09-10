# Phase 06 — documentation and observability

> **Motivating findings:** F-8 and F-16 (transport recommendation + sizing),
> F-6 (admin config does not reflect the YAML), F-3 (benign disconnects at WARN)
> **Kind:** zero code on the data path | **Effort:** small
> **Prerequisite:** none. Recommended **before** Phase 05.

Scheduled early despite being last in severity, because its `--udp` guidance is
what stops operators walking into the Phase 02 memory ceiling in the first
place. Documentation that prevents a failure outranks code that recovers from
it.

---

## 6.1 — The transport recommendation (N-10, F-8)

**Files:** `README.md` (single source of truth), `docs/vhost/`

Nothing in the documentation says any of this today, and the default choice an
operator makes without it is the wrong one.

State plainly, with the measurements:

- **On a clean, low-RTT path the TCP relay is the right transport.** Paired A/B:
  **8 of 8 pairs favour the relay, median 1.589×** (§2.15 A1). It also costs
  about half the client CPU, **2.5× less server CPU per byte** (F-16), and
  **4.5× less server memory** under concurrency (F-13).
- **`--udp` is a remedy for a lossy or long-RTT path**, where it is **2.8×**
  the relay at 1 % loss and **2.2×** at +40 ms (§2.14). On a clean path it is
  25–110 % *slower*.
- **Carriers default to 1 and that is correct** for an idle tunnel: the paired
  median `c=4/c=1` ratio is **0.941** (§2.15 A2). They earn their keep under
  loss (~1 MB/s per carrier at 1 % loss, §2.6) and under concurrent bulk load
  (3× p50 at `c=8`, §2.16).

---

## 6.2 — `--udp` is not a bandwidth transport, and the 5 Gbit/s sizing (N-14, F-16)

**Files:** `README.md`, `docs/vhost/`, `docs/performance/`

Two facts, both measured this campaign, both currently absent from the docs:

- **`--udp` tops out at 0.96 Gbit/s** — *below* a 1 Gbit/s link — costs **2.5×
  the CPU per byte**, and **no flag moves it**: `--carriers 1→4` and 1→4
  parallel streams take it from 99 to 120 MB/s and no further (§2.17.3). It also
  draws instance-level PPS throttling the relay never triggers. So it must never
  be chosen for bandwidth. Pair this sentence with 6.1's resilience case so the
  two halves are read together.
- **Sizing.** The relay costs **5.74 CPU-seconds per GiB** under load on a
  Graviton2 core with TLS at 1500 MTU. Divide the target rate by it:

  | target | relay cores needed |
  | --- | --- |
  | 1 Gbit/s | 0.67 |
  | 2.68 Gbit/s | 1.90 (measured ceiling on 2 burstable Graviton2 cores) |
  | 5 Gbit/s | 3.34 |
  | 10 Gbit/s | 6.7 |

  With the operational recommendation: **a 5 Gbit/s deployment wants 6–8 vCPU
  and `--carriers 4`**, because bore was measured using up to 3.61 cores and
  4 vCPU would leave no margin. Note x86 should be cheaper per byte than
  Graviton2 at the same clock, so treat 3.34 as the ceiling of the estimate.

State the method too, not only the answer: cost per GiB is the transferable
number, absolute MB/s is a property of one box and one link. That is what lets a
future reader re-derive the sizing for hardware nobody has measured yet.
`scripts/perf/README.md` already documents the three-script sequence.

---

## 6.3 — Demote benign disconnect warnings (N-2, F-3)

**Files:** `src/vhost.rs`, and wherever the relay logs a client-side
disconnect

**Measured:** **91 % of the warnings** the server emitted during the campaign
were ordinary client behaviour — a browser closing a connection, a reader going
away mid-response.

At WARN they train operators to ignore the log, which is worse than silence.
Demote to `debug`, keeping WARN for genuinely abnormal terminations.

**Precedent to follow:** `CLAUDE.md`'s secret-path rule — *"benign hole-punch
strays are `debug`, never `WARN`"* (BUG-S3) — including its explicit
instruction not to restore the per-event WARN later. Add the same note here so
this is not "fixed" back.

**Invariant:** demote only *client-initiated* close on a connection that had
already been established and served. A failure before the head is written, or a
provider-side failure, stays WARN.

**Gate:** a test asserting the level for a clean client-side close. Cheap, and
it is what stops the regression.

---

## 6.4 — Surface the merged vhost configuration (N-6, F-6)

**Files:** `src/admin_views.rs`, `src/server.rs`, the admin frontend's config
section

**Measured (F-6):** `/admin/api/v1/config` reports the vhost response headers
as `null` while the SSH gateway's own `vhost_info_banner` reports all seven of
them correctly to a connecting client. The data exists and is resolved; it is
simply not surfaced on the endpoint an operator reads.

Phase 02.1 fixes the adjacent instance of the same class (the `ConfigView`
window defaults, hardcoded and stale). Do both, and pin them with the same kind
of test: the view is **derived** from the live config, never restated.

**Gate:** a unit test that the config view's vhost section equals the resolved
merged configuration, including `default_response_headers`. Red-check by
hardcoding one value.

## Phase acceptance

1. `README.md` states the transport recommendation, the `--udp` bandwidth
   ceiling, and the sizing rule with its method. Per `CLAUDE.md`: not in
   `README.md` ⇒ not done.
2. A default-configured server's `/admin/api/v1/config` matches reality for both
   the QUIC windows and the merged vhost configuration, pinned by tests.
3. Benign client disconnects are `debug`; the note against re-promoting them is
   in place.
4. Full regression green, `npm` frontend tests green if the admin view changed.
