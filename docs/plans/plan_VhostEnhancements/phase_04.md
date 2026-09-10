# Phase 04 — the relay's concurrency tail: diagnose before designing

> **Motivating finding:** open question 7 (§2.13 G8)
> **Kind:** optimization | **Effort:** small to diagnose, unknown to fix
> **Prerequisite:** Phase 03 may already fix it — run 4.1 *after* Phase 03

## The observation

At high concurrency the TCP relay develops a tail that QUIC direct does not
(§2.13 G8):

| concurrent connections | relay: fresh-request time | QUIC direct |
| --- | --- | --- |
| 16 / 64 | normal | normal |
| 256 | **966 ms** | 14 ms |
| 512 | **1 436 ms** | 14 ms |

This is the relay's **only** measured weakness, and it matters because the relay
is otherwise the faster and far cheaper transport (F-8 median 1.589×; F-16 2.5×
cheaper per byte). Note explicitly: S4 showed **no** head-of-line blocking with
6 slow readers, so this is a *scale* effect and not the same phenomenon as
F-15 — do not conflate the two.

## Why this phase is diagnosis-first

Three candidate causes, with materially different fixes, and the evidence does
not yet distinguish them:

- **(a) The `--max-conns` semaphore.** 1024 on staging. At 512 concurrent
  connections plus carriers the pool may be near enough to saturation that
  permit acquisition queues. If so the "fix" is documentation and a default,
  not code.
- **(b) yamux substream open serialization.** All 512 opens funnel through one
  carrier's yamux session. Substream establishment is a control-frame exchange;
  if it serializes, latency grows with the queue depth. If so, Phase 03's
  carrier work may already have fixed it — which is why this phase runs after.
- **(c) Head-of-line on the single carrier TCP connection.** Distinct from (b):
  not open serialization but the byte stream itself. S4's negative result at 6
  slow readers argues against this, but 512 is two orders of magnitude away.

Designing against the wrong one is the expensive mistake here, and the campaign
already retracted four findings that were apparatus artifacts. So:

---

## 4.1 — Re-measure after Phase 03, before anything else

**Harness:** `scripts/perf/vhost_remote_stability.sh g8`

Run the 16/64/256/512 ladder on both transports with Phase 03 merged. Two
possible outcomes and they lead to different places:

- **The tail is gone.** Record it as a Phase 03 side effect, close open question
  7 in the evidence document, and stop. This is a plausible outcome: if the
  cause is (b), least-loaded selection plus pool growth addresses it directly.
- **The tail remains.** Continue to 4.2.

Do not skip this. It is one script invocation and it can retire the whole phase.

---

## 4.2 — Discriminate the three candidates

Each has a cheap, decisive experiment. Run them against a **private** server
(staging is frozen), which also lets `--max-conns` be varied — impossible on
staging.

| candidate | experiment | discriminating signal |
| --- | --- | --- |
| (a) `--max-conns` | repeat the 512 rung with `--max-conns` at 512, 1024, 4096 | tail scales with the limit ⇒ it is permit queueing |
| (b) yamux open serialization | repeat at `--carriers 1` vs `4` vs `8`, holding total connections fixed | tail divides by roughly the carrier count ⇒ it is per-session open serialization |
| (c) carrier byte-stream HOL | hold carriers at 1 and compare 512 *idle* connections against 512 *active* ones | tail present only when they carry data ⇒ it is the byte stream, not the open path |

Instrument the open path with timing so the wait is attributed rather than
inferred: time from "proxied connection accepted" to "substream open returned",
separately from "permit acquired". Those two numbers alone separate (a) from
(b)/(c).

**Deliverable of 4.2 is a written attribution**, appended to §5 open question 7
of the evidence document, with the measurements. Not a fix.

---

## 4.3 — Fix, scoped by the attribution

Do not pre-commit to a design. Sketches only, so the reader knows the shape:

- **If (a):** a default-`--max-conns` recommendation plus a `warn!` when live
  connections approach the limit, and documentation of the sizing. The 2 GiB
  IONOS box has room; the current 1024 may simply be low for a busy vhost.
- **If (b):** the fix is likely Phase 03's mechanism extended — spread *opens*
  across carriers even for non-bulk connections once a depth threshold is
  crossed. Reuse, do not re-invent.
- **If (c):** the honest answer may be "prefer QUIC above N concurrent
  connections", which is a documentation change and contradicts nothing: F-8's
  ranking is for throughput on a clean path, and this would be a concurrency
  criterion. It would **not** be a reopening of the rejected load-aware *path
  selection* (a bandwidth argument, closed by F-16) — a concurrency-based
  recommendation is a different claim and should be labelled as such to avoid
  future confusion.

**Invariant regardless of outcome:** any change to the open path is on the
~11 000 req/s hot path (F-11). Measure the idle case as well as the loaded one,
and hold idle p50 at ~2.5 ms.

## Phase acceptance

Either:

1. 4.1 shows the tail gone, open question 7 is closed in the evidence document
   with the measurement, and the phase closes with no code; **or**
2. 4.2 produces a written, measured attribution to (a), (b) or (c), and 4.3
   ships the correspondingly scoped fix with the 512-connection fresh-request
   time brought materially below the measured 1 436 ms, with idle p50 unchanged.

A phase outcome of "diagnosed, attributed, and deliberately not fixed" is
acceptable and should be recorded as such — that is the correct result if the
attribution is (c) and the answer is a documented transport recommendation.
