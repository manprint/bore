# Phase 05 — fast, legible failures

> **Motivating findings:** F-14 (the UDP-to-relay fallback loses the first
> request, and its metrics lie), F-12 (an origin failure closes the connection
> instead of returning 502)
> **Kind:** latency + operability | **Effort:** small, self-contained
> **Prerequisite:** none

Three independent items, grouped because they share a theme: a failure that is
currently either slow, invisible, or both. Original candidates N-11, N-8, N-12.

---

## 5.1 — Bound the direct-open deadline (N-11, F-14)

**Files:** `src/vhost.rs` (`relay_vhost`, the `entry.direct.pick()` site ≈ 914),
`src/holepunch.rs`

**Measured:** during a total UDP blackout the fallback is seamless *in
aggregate* — 179 MB/s, actually **higher** than the 116 MB/s QUIC was managing —
but the **first request is lost**: `http=000` after **9.90 s** (F-14, §2.14).

So the fallback works; only its first attempt is unbounded. One user-visible
request is destroyed by a path failure that the system then handles correctly,
which is the worst combination for diagnosis: the operator sees one broken
request and a healthy tunnel.

**Fix:** bound the direct stream open with a deadline and fall back to the warm
TCP relay for the **same** request on expiry. The pattern is already written,
tested and shipped on the SSH path — I-SSH10's `ssh_open_timeout()` (15 s with a
`BORE_SSH_OPEN_TIMEOUT_MS` test override), and `CLAUDE.md` records the
established rule for the direct path: *"Missing/dead/open-failed QUIC falls back
for the SAME channel to warm TCP, never kills the outer session."* This is that
rule applied to the *timeout* case rather than the *error* case.

**Deadline choice:** far below 15 s. The QUIC keepalive on this path is already
3 s and the idle timeout 10 s (`transport_config`, byte-identical since
`3a5c87b` per `CLAUDE.md`). A deadline on the order of the keepalive is the
right scale — a direct open that has not completed in a few keepalive intervals
is not going to. Calibrate against `vhost_netem_matrix.sh`'s UDP-blackhole case;
do not exceed the idle timeout, or the deadline can never fire first.

**Invariants**
- Falling back must not kill the tunnel or the request. Same-request fallback,
  as `CLAUDE.md` requires.
- A *answered* open — including a failure — is not a timeout and must not be
  conflated. I-SSH10 makes exactly this distinction (an answered
  `ChannelOpenFailure` resets its counter) and it exists because an origin that
  is merely restarting must not be treated as a wedged path.
- `--udp` off ⇒ no new code on the path.

**Gate:** the UDP-blackhole case of `scripts/perf/vhost_netem_matrix.sh` — the
first request must succeed, against the measured `http=000` after 9.90 s. Use
protocol-selective netem (`u32 match ip dst X/32 match ip protocol 17 0xff`) so
only the QUIC data plane is impaired and the always-TCP consumer leg is not;
and mind the `tc` argument order (`tc qdisc add dev <if> root …`), which
silently failed once and produced a healthy-looking fallback that had never been
impaired (§9.6 pitfall 5).

---

## 5.2 — Answer origin failures with 502/504 (N-8, F-12)

**Files:** `src/vhost.rs` (`relay_vhost` error paths)

**Measured (§2.13 G5):** a dead origin, or a closed origin port, yields
`http=000` — a bare connection close with no status line. An unknown subdomain
correctly returns 404, and a restored origin serves 200 on the same tunnel, so
the routing layer is fine; it is specifically the provider-side connect failure
that produces nothing.

`http=000` is indistinguishable from "the tunnel is gone", "the server is
down" and "the network broke". It is the single cheapest diagnostic improvement
in the plan.

**Fix:** synthesize a response when the provider cannot reach the origin —
**502** for connection refused/reset, **504** for a timeout.

**Invariants**
- The response must go through the **injected-response path**
  (`relay_response_injected` + `copy_one_direction_with_shutdown`), which is the
  path that flushes after every write. `CLAUDE.md` is emphatic: on keep-alive
  there is no EOF to flush, and a synthesized response that parks on `read()`
  without flushing leaves the client hanging — the exact bug fixed in `36cd70d`.
  A synthesized error page is a *small* write, which is precisely the shape that
  sits in the TLS session buffer unnoticed.
- Do not replace the hand-rolled copy loop with `tokio::io::copy`, and keep the
  split + `try_join!` single-task shape (yamux waker invariant).
- Only the provider-connect failure is in scope. A failure *mid-response*
  cannot become a 502 — the head is already sent — and must keep today's
  behaviour of closing.

**Gate:** an integration test per case (origin down, origin port closed, origin
timing out) asserting the status code and, critically, that the **body and
headers actually arrive** on a keep-alive connection. Gate at the
`FlushGatedWriter` mock level as `CLAUDE.md` requires: *"every in-process TLS
integration test of this bug FALSE-PASSES on loopback (rustls drains
opportunistically; verified 2026-07-08)"*. Also extend
`scripts/perf/vhost_remote_stability.sh g5`, which already exercises
origin-alive → origin-dead → origin-restored on a dedicated port.

---

## 5.3 — Fix the direct-path counters (N-12, F-14)

**Files:** `src/vhost.rs` (`VhostEntry.direct_stream_opens` ≈ 374-400),
`src/admin_views.rs`, the admin frontend's vhost section

**Measured:** both observability counters misreport during a UDP blackout
(F-14):

- `direct_stream_opens` **rose from 1 to 12 during a 100 % UDP drop** — it
  counts *attempts*, so it climbs while the direct path is entirely dead.
- `direct_fallbacks` **stayed at 0** throughout that same blackout.

The consequence is stated in §6 (N-12): an operator cannot tell from the admin
API whether `--udp` is in use at all. It also cost the campaign real time —
§9.5's rule "prefer an impairment control over a counter" exists because of
this finding.

**Fix, three parts:**

1. Increment `direct_stream_opens` only on a **successful** open, or rename it
   to `direct_stream_open_attempts` and add a success counter. Renaming is a
   frontend change; the campaign's own preference is for the counter to mean
   what its name says.
2. Make `direct_fallbacks` actually increment on every fallback, including the
   5.1 timeout case.
3. Add a field stating which path the tunnel is **on right now** — the thing an
   operator actually wants and which no counter can express. A tunnel that
   negotiated direct and has since fallen back to relay for every connection
   currently looks identical to a healthy direct one.

**Invariants**
- Counters are on the hot path; use relaxed atomics, as the existing ones do.
- Wire and admin-view additions are additive `#[serde(default)]` per project
  convention, so an old frontend against a new server still renders.
- Per `[[admin-txrx-live-counting]]`, do not compute these from
  `copy_bidirectional` return totals.

**Gate:** an integration test that a forced fallback increments
`direct_fallbacks` and leaves the success counter unchanged, plus the
current-path field flipping relay→direct→relay. Red-check against the current
behaviour, which is the measured F-14 result: attempts climbing while fallbacks
stay 0.

## Phase acceptance

1. A total UDP blackout costs **zero** failed requests, against the measured one
   lost request after 9.90 s.
2. A dead origin returns 502, a timing-out origin 504, both fully flushed on a
   keep-alive connection, against the measured `http=000`.
3. `/admin/api/v1/vhost` lets an operator answer "is this tunnel using `--udp`
   right now, and has it been falling back?" — which it currently cannot.
4. Full regression green; the `g5` and netem harnesses updated in the repo.
