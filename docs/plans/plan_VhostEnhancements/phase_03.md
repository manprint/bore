# Phase 03 — isolate small requests from bulk transfers  *(headline optimization)*

> **Motivating findings:** F-15 (a bulk transfer in flight destroys concurrent
> request latency), §2.16 (carriers do mitigate it, per transport)
> **Kind:** optimization — the largest measured user-visible win in the campaign
> **Effort:** medium | **Prerequisite:** Phase 02 (DEC-VE1, DEC-VE8)

## The problem, precisely

This is the most ordinary workload there is — one large asset alongside many
small ones, i.e. any page load — and no idle benchmark shows it.

Measured on the same tunnel, small requests while a bulk transfer is in flight
(F-15, §2.16):

| transport | idle p50 | 1 bulk in flight | 2 bulk in flight | request rate |
| --- | --- | --- | --- | --- |
| relay TCP, `c=1` | 2.57 ms | **28.25 ms** (11×) | worse | **−91 %** |
| QUIC direct, `c=1` | ~2.6 ms | p99 **219 ms** | — | −82 % |
| relay TCP, `c=8` | — | — | **14.8 ms** (3× better than `c=1`) | — |
| QUIC direct, `c=4` | — | — | slightly **worse** than `c=1` | — |

§2.16 identified the mechanism separately for each transport, which is what
makes this phase tractable rather than exploratory:

- **Relay: carrier saturation.** `CarrierPool::pick` (`src/pool.rs:109`) is
  round-robin, and a proxied connection is **pinned to one carrier for its
  whole life**. A small request that lands on the carrier a bulk transfer is
  saturating waits behind it. Raising the carrier count to 8 dilutes the odds
  and buys a 3× p50 improvement — evidence that the mechanism is carrier
  occupancy and nothing subtler.
- **QUIC: stream scheduling.** Every proxied connection is a bidi stream on one
  QUIC connection. Carriers do nothing (measured: `c=4` is slightly worse),
  because the contention is between streams inside a connection, not between
  connections.

**DEC-VE7 — the honest target.** §2.16 also measured that *no* configuration
recovers the 2.5 ms idle p50; the best observed under two bulk transfers was
14.8 ms at `c=8`. This phase succeeds if the **default** configuration reaches
that band (7–15 ms) without an operator setting `--carriers 8` by hand. It does
not promise 2.5 ms.

**DEC-VE8 — the constraint.** F-15's own note: *"anything that buffers more to
smooth latency makes the memory ceiling worse"* (F-13). This phase
**redistributes** work; it must not buy latency with buffer memory. Phase 02
bounds the budget this phase must live inside.

---

## 3.1 — Classify a proxied connection as bulk

**Files:** `src/vhost.rs` (the per-connection relay state in `relay_vhost`,
≈ 876-950), `src/shared.rs` (`CountingStream` already exists and counts live
per read/write — see `[[admin-txrx-live-counting]]`)

**DEC-VE4:** classification is *bytes already moved on this proxied connection,
compared against a threshold*. Not Content-Type, not Content-Length, not path
patterns.

Rationale: a single monotonic counter, transport-agnostic, cannot be lied to by
a wrong `Content-Length`, and needs no HTTP parsing on a path that is
deliberately a byte pipe after the first head. Every alternative requires
either trusting the origin's declaration or re-parsing a stream the relay has
gone to some trouble not to touch.

The counter is already there: every splice is wrapped in
`shared::CountingStream`, which increments per read/write (that wrapping is a
standing invariant — "never revert to `copy_bidirectional` return totals").
This subphase exposes the running count to the scheduler rather than adding a
new one.

Threshold: start at a value on the order of a few hundred KiB — large enough
that an ordinary asset never trips it, small enough that a bulk transfer trips
it within the first fraction of a second. The exact number is calibrated in 3.5
against `vhost_bulk_latency.sh`, not guessed here. It must be a named constant
with the calibration recorded beside it.

**Invariant:** classification is one-way (a connection becomes bulk and stays
bulk). A connection oscillating between classes would produce exactly the kind
of scheduling churn that is worse than either steady state.

**Gate:** unit tests over the classifier: below threshold ⇒ not bulk, crossing
⇒ bulk, stays bulk afterwards. Cheap and fully deterministic.

---

## 3.2 — Relay: least-loaded carrier selection

**Files:** `src/pool.rs` (`CarrierPool::pick`, ≈ 109)

Current implementation:

```rust
pub fn pick(&self) -> Option<mux::LinkOpener> {
    let mut carriers = self.carriers.lock().expect("carrier pool mutex");
    carriers.retain(|c| c.alive.load(Ordering::Relaxed));
    if carriers.is_empty() { return None; }
    let idx = self.next.fetch_add(1, Ordering::Relaxed) % carriers.len();
    Some(carriers[idx].opener.clone())
}
```

Change: choose the carrier with the fewest **bulk-classified** connections
currently assigned, breaking ties round-robin so the existing behaviour is
preserved when no connection is bulk.

**This is the highest-risk edit in the plan** and needs stating plainly:
`CarrierPool` is shared by the **secret**, **vhost** and **SSH-jump** paths
(`src/secret.rs:315`, `src/vhost.rs:603`, `src/ssh_jump.rs:473`). Constraints:

- **DEC-VE6:** with `carriers <= 1` the function must be byte-for-byte
  equivalent to today — a one-element pool has nothing to choose. Assert this
  with a test, because it is a standing project invariant, not a nicety.
- With no bulk connections the selection must reduce to today's round-robin, so
  every existing path is unchanged in the common case. The campaign measured
  that carriers slightly *hurt* on a clean idle path (median c4/c1 ratio 0.941),
  so a scheduler that changes behaviour when there is nothing to schedule would
  be a regression.
- The counters must be cheap. This is on the per-proxied-connection open path,
  which F-11 measured at ~11 000 req/s. An `AtomicUsize` per carrier
  incremented on bulk classification and decremented on close — not a lock, not
  a scan of live connections.
- Prefer adding a `pick_for(&self, hint)` alongside `pick` over changing
  `pick`'s signature, so the secret and SSH-jump call sites stay literally
  untouched and the blast radius is visible in the diff.

**Gates**
- `pick_single_carrier_is_unchanged` — one-element pool, N picks, same result as
  today.
- `pick_without_bulk_is_round_robin` — the tie-break path reproduces the current
  sequence exactly.
- `pick_avoids_carrier_holding_bulk` — with one carrier marked bulk and three
  idle, N picks never select the bulk one until the idle ones are equally
  loaded.
- Red-check each by reverting the selection change.
- Full `cargo test` plus `secret_netns_test.sh` and `ssh_gateway_test.sh`,
  since those two paths share the pool.

---

## 3.3 — Relay: bulk-aware pool growth (adaptive carriers)

**Files:** `src/vhost.rs` (`serve_vhost_provider`, the `recv_carrier` arm),
`src/client.rs` (carrier dialer), `src/shared.rs` (wire message)

This is where original **candidate 4 (adaptive carrier count)** lands. §2.16
supplied the trigger that makes it implementable: **concurrent bulk load on the
same tunnel**, which is directly observable server-side, rather than packet
loss, which is not.

Behaviour: when a tunnel has bulk-classified connections occupying every
carrier and a non-bulk connection arrives, request an additional carrier from
the provider, up to the negotiated `--carriers` maximum. Shrink back after a
quiet period.

**Why adaptive and not simply a higher default:** the paired A/B measured a
median `c=4/c=1` ratio of **0.941** on a clean path with 4 of 5 pairs below 1.0
(§2.15 A2). Carriers cost something when they are not needed. §2.17.1 adds that
they are also the lever that unlocks extra cores (1.0 → 3.6 cores used), so the
count is genuinely regime-dependent, and a default of 1 is right for an idle
tunnel.

**Invariants**
- The maximum stays the operator's `--carriers` value and the server's
  `--max-carriers`. Adaptive means *up to*, never beyond.
- Growth must be rate-limited. A carrier is a TCP connection plus a yamux
  session; churning them under bursty load would cost more than the head-of-line
  it avoids. `CLAUDE.md` already records `VH-2 carrier churn` as a known hazard.
- An existing pinned connection is **never migrated** between carriers. It
  cannot be: `CLAUDE.md` closes off striping a connection across carriers
  (N-5, reorder trap). New connections benefit; in-flight ones do not.
- A provider that does not support on-demand carrier growth must keep working
  unchanged — the existing `PendingCarriers` / `JoinCarrier` handshake is
  already the mechanism, so this is a *trigger* change rather than a protocol
  change if the client already dials carriers on demand. **Verify this before
  designing a new message**; if the client dials all carriers up front, an
  additive request message is needed and it follows the DEC-VE2 pattern.

**Gate:** an integration test where a bulk transfer occupies the single carrier,
a small request arrives, and the pool grows; plus the negative test that an
idle tunnel never grows past 1.

---

## 3.4 — QUIC: stream priority and a send-burst cap

**Files:** `src/holepunch.rs` (≈ 3055-3075, where the quinn transport config is
built), `src/vhost.rs` (`relay_vhost`, the `entry.direct.pick()` site at ≈ 914)

This is the half of original **candidate 5** the campaign *did* support (the
fewer-copy half stays rejected — F-7: 82 % of the cost is kernel).

Two changes, both quinn-native:

- **Stream priority.** quinn exposes per-stream priority; a newly opened stream
  should not be scheduled behind a large in-flight burst. Set a lower priority
  on bulk-classified streams so small ones interleave. Note that priority
  affects the **send** side of whichever peer sets it: for a vhost download the
  server is the receiver of provider data and the sender toward the public
  reader, so it must be verified experimentally which side's scheduling
  produces the 219 ms p99 before choosing where to set it. Do not assume.
- **Per-stream send burst cap.** Bound how much a single stream may hand to the
  connection before yielding, so one bulk stream cannot monopolize the
  congestion window between the scheduler's decisions.

**Invariants**
- **DEC-VE8:** neither change may raise `stream_receive_window` or
  `connection_receive_window`. The tempting fix for a starved stream is more
  window; F-13 is what that costs, and Phase 02 has just bounded it.
- The 16:1 connection-to-stream ratio stays exactly as `CLAUDE.md` pins it.
- Carriers are **not** the lever here (measured: `c=4` is slightly worse), so
  the relay's 3.2/3.3 work must not be generalized onto this path.
- `--udp` off ⇒ zero new code executed.

**Gate:** `scripts/perf/vhost_bulk_latency.sh` p99 under one bulk transfer,
against the measured 219 ms baseline. This one needs a real network path with a
real congestion controller; an in-process loopback test will false-pass it
(`[[feedback-inprocess-test-false-pass]]` — this codebase has two confirmed
cases of exactly that, one of them a flush bug on this very vhost path).

---

## 3.5 — Calibrate and accept

**Harness:** `scripts/perf/vhost_bulk_latency.sh` (built for F-15; sweeps
0/1/2 bulk transfers in flight × carrier count × transport)

Calibration to perform, not to guess:

1. The 3.1 bulk threshold — the smallest value at which no ordinary asset in the
   suite trips it and every bulk transfer trips it promptly.
2. The 3.3 growth and shrink timings.
3. Where in 3.4 the QUIC priority must be set to move the p99.

**Trustworthiness rules that apply** (§9.5, and they are not optional here —
they are what makes the acceptance number believable):

- Take bulk rates from the server's own `relay_tx_bytes` delta, never from
  `curl`'s reported speed.
- Use the **paired** design: both halves back to back, alternating order, and
  quote the **median of per-pair ratios**. The control drift on the staging
  server is **29 %**, so an unpaired before/after comparison of a 3× effect is
  believable but of anything smaller is not.
- Prove the data path per case. `direct_stream_opens` counts *attempts*, not
  successes (F-14), so prefer an impairment control over the counter.
- Staging is frozen and must not be modified; all server-side changes are A/B
  tested against a private `bore server`, the pattern
  `vhost_header_injection_ab.sh` and `vhost_app_ceiling.sh` already implement.

## Phase acceptance

1. **Relay:** small-request p50 with two bulk transfers in flight lands in the
   7–15 ms band at the **default** carrier setting, against the measured
   baseline of 28.25 ms at `c=1` and 14.8 ms at a hand-set `c=8`.
2. **Relay:** request rate under bulk recovers substantially from the measured
   −91 %.
3. **QUIC:** p99 under bulk improves materially against the measured 219 ms.
4. **No regression on the idle path.** Idle p50 stays at ~2.5 ms and the paired
   clean-path ratio does not drop below the measured 0.941 baseline — i.e. the
   scheduler must be inert when there is nothing to schedule.
5. **No regression in memory.** Phase 02's server-wide budget figure is
   unchanged by this phase (DEC-VE8).
6. `carriers <= 1` byte-identical; secret and SSH-jump netns suites green.
