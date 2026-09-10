# Vhost Enhancements — Plan Overview

> **Status:** planning | **Opus authored:** 2026-09-10
> **Folder:** `docs/plans/plan_VhostEnhancements/`
> **Branch:** main (campaign artifacts currently uncommitted)
> **Evidence base:** `docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md`
> (findings F-1…F-16, candidate assessment §6, runbook §9)

## Goal

Act on the measured findings of the vhost staging campaign, in the order the
evidence justifies. Every phase below cites the finding that motivates it and
the measurement that will confirm it. Nothing here is speculative
optimization — the campaign explicitly closed off the tempting-but-worthless
work, and that list is reproduced under *Non-goals* so it stays closed.

The operator's stated goal for the campaign was: *"capire come massimizzare
performance e minimizzare le latenze, ed avere stabilità operativa"* — maximize
performance, minimize latency, and have operational stability. The campaign's
answer is that **performance in the bulk-throughput sense is already solved**
(F-16) and the remaining work is entirely **latency** and **stability**.

## What the evidence already settled, and why it shapes this plan

**F-16 — the application does not limit the link.** The TCP relay needs 0.67 of
one core to saturate the announced 1 Gbit/s IONOS link (5.74 CPU-seconds per
GiB under load), and the *smaller* staging box — a 2-core burstable Graviton2
t4g.micro — already delivers 2.68 Gbit/s at 1.90 of its 2 cores. bore scales
across cores when given them (0.98 → 3.61 cores used as `--carriers` and
parallelism rise). Consequences that shape every phase:

- **No bulk-throughput work is in this plan.** There is nothing to win: the link
  saturates at a third of one core, and 46–55 % of what bore does spend is
  kernel softirq that user-space changes cannot touch (F-7, §2.17.2).
- **The ~1.3 spare cores are the budget** this plan spends on concurrency and
  latency.
- **5 Gbit/s is a sizing note, not a code change**: 3.34 cores at staging's
  efficiency ⇒ 6–8 vCPU with `--carriers 4`. Phase 6 writes it down.
- **`--udp` is the one genuine application limit** (0.96 Gbit/s ceiling, 2.5×
  the CPU per byte, and no flag moves it) — which makes it a *resilience*
  transport, not a bandwidth one. Phase 6 documents that; Phase 2 bounds its
  memory cost; nothing in this plan tries to raise its ceiling.

**Ordering decision (DEC-VE1).** The operator asked to prioritize the
optimizations, and the headline optimization is Phase 3 (isolate small requests
from bulk transfers — the 11× p50 degradation, the worst user-visible result in
the campaign). It is nevertheless scheduled **third**, behind the two stability
defects, for a technical reason and not a cautious one: F-13 states that
*anything that buffers more to smooth latency makes the memory ceiling worse*.
Phase 3 designs buffering and scheduling behaviour on the direct path; doing it
on top of an unbounded receive window (Phase 2) means designing against a
moving target and redoing the work. Phase 1 is placed first because it is the
cheapest item in the plan (a mechanical mirror of code already shipped twice in
this binary) and it removes the only defect that makes a subdomain permanently
unusable.

## Phase map

| phase | subject | finding | kind | why here |
| --- | --- | --- | --- | --- |
| [01](phase_01.md) | vhost control liveness — heartbeat + reaper | F-1, F-10 | stability | cheapest item in the plan; pattern already shipped twice in this binary. A wedged provider currently holds its subdomain **forever** |
| [02](phase_02.md) | bound the direct-path receive window | F-13 | stability | the only finding that degraded the whole server; and Phase 3 must be designed on top of a bounded window, not an unbounded one |
| [03](phase_03.md) | **isolate small requests from bulk transfers** | F-15, §2.16 | **optimization (headline)** | the worst user-visible latency result in the campaign, on the most ordinary workload there is: one big asset alongside many small ones |
| [04](phase_04.md) | the relay's concurrency tail — diagnose first | open question 7 | optimization | 1 436 ms vs QUIC's 14 ms at 512 connections. The relay's only measured weakness. Diagnosis before design |
| [05](phase_05.md) | fast, legible failures | F-14, F-12 | optimization + operability | a dead UDP path costs a 9.9 s stalled request; a dead origin returns nothing at all |
| [06](phase_06.md) | documentation and observability | F-8, F-16, F-6, F-3, F-14 | zero-code | prevents operators from choosing the slower, hungrier transport by default, and fixes counters that currently lie |
| [07](phase_07.md) | HTTP/2 on the vhost edge — **spike only** | candidate 6 | measurement | the strongest latency lever for remote browsers, and by far the biggest piece of work. Measure the prize and the graft before committing |

Recommended execution: 01 → 02 → 03 → 06 → 05 → 04 → 07. Phase 06 is pulled
forward of 05 because it is zero-code and its `--udp` guidance is what stops
operators reaching the Phase 2 memory ceiling in the first place.

## Design decisions (locked before implementation)

- **DEC-VE1 — stability before the headline optimization.** As argued above:
  F-13 interaction, not caution. Phase 3's acceptance criteria are measured
  against Phase 2's bounded window.
- **DEC-VE2 — reap only providers that opted in.** Phase 1 adds an additive
  `#[serde(default)]` capability flag to `HelloVhost`. A provider that does not
  declare it keeps **today's exact behaviour and is never reaped**. Reaping a
  legacy client that cannot send heartbeats would kill healthy idle tunnels
  every 60 s — including the operator's own long-lived ones. F-1 therefore
  persists for un-upgraded clients, which is strictly better than the
  alternative. This mirrors how every other wire change in this codebase has
  been shipped.
- **DEC-VE3 — the liveness deadline is checked on the heartbeat tick, never via
  `timeout(recv)`.** The heartbeat branch wins the `select!` every 500 ms and
  would reset a `timeout(recv)` future before its deadline could ever be
  reached. This is recorded in `CLAUDE.md` as a hard invariant of the secret
  reaper and it applies verbatim here.
- **DEC-VE4 — bulk is classified by bytes already moved on the proxied
  connection, never by content sniffing.** A single monotonic counter per
  proxied connection, compared against a threshold. No Content-Type, no
  Content-Length parsing, no path heuristics. Cheap, transport-agnostic, and it
  cannot be wrong about a response whose declared length lies.
- **DEC-VE5 — the two transports get different treatments in Phase 3.** §2.16
  measured that carriers mitigate the relay's problem (3× p50 at `c=8`) and do
  **nothing** for QUIC's. So: least-loaded carrier selection plus bulk-aware
  pool growth on the relay, stream priority plus a send-burst cap on QUIC. One
  shared mechanism would be wrong for one of them.
- **DEC-VE6 — `carriers <= 1` stays byte-for-byte identical.** Existing project
  invariant. Phase 3 touches `CarrierPool::pick`, which is shared by the secret,
  vhost and SSH-jump paths, so this constraint is load-bearing and is re-checked
  per subphase.
- **DEC-VE7 — Phase 3 targets 7–15 ms p50 under bulk, not 2.5 ms.** §2.16
  measured that *no* configuration recovers the 2.5 ms idle p50. Promising it
  would set an unreachable acceptance bar. The measured best case under two
  concurrent bulk transfers was 14.8 ms at `c=8`; the phase succeeds if the
  default configuration reaches that without needing `c=8` set by hand.
- **DEC-VE8 — no phase raises a default window, buffer or carrier count to buy
  latency.** Every such change is paid for in the F-13 memory budget. Phase 3
  redistributes work; it does not buy headroom with memory.

## Non-goals — closed off by measurement, do not reopen

Each was a plausible optimization that the campaign measured and rejected. They
are listed here so a later reader does not spend effort re-deriving them.

| rejected | why, with the measurement |
| --- | --- |
| any bulk-throughput optimization of the relay data path | F-16: 0.67 of one core saturates the announced link, and 46–55 % of the cost is kernel softirq |
| load-aware **path** selection (original candidate 1) | F-16 removes the premise — on a clean link the relay is both faster (F-8, median 1.589×) and 2.5× cheaper per byte, so there is no bandwidth reason to switch paths under load. F-14's automatic fallback already covers the failure case |
| raising `--udp`'s 0.96 Gbit/s ceiling | F-16. The relay is the correct transport on every path where a gigabit matters. The PPS-allowance mechanism (§2.17.3) is worth *understanding* — it is inferred, not measured — but not fixing |
| making header injection conditional (N-4) | F-4 measured the injection path at **+0.6 % CPU**. The flush-after-every-write remains a correctness requirement (`CLAUDE.md`) |
| resizing yamux windows on the relay (N-3) | §2.6: the collapse under loss is the carrier's TCP congestion window, not yamux flow control. Withdrawn |
| striping one proxied connection across carriers (N-5) | a tunnelled TCP flow reads reordering as loss. `CLAUDE.md` already records that per-datagram round-robin *halved* VPN throughput and pushed UDP loss to 25–44 % |
| fewer-copy QUIC path (half of candidate 5) | F-7: 82 % of server CPU is kernel, so application copies are at most a seventh of the bill |
| `SO_*BUF` on the SSH sockets | F-9 and the prior `SSH_GATEWAY_ASSESSMENT_2026-07-10.md`: it clamps to `net.core.*mem_max` and kills autotuning. Remediation is sysctl-level |

## Global gates

Per `CLAUDE.md`, every subphase is done only when all of the following pass —
a subphase that breaks an existing test is **not** done:

1. `cargo fmt --all`
2. `cargo clippy --all-targets -- -D warnings` (and `--features vpn`,
   `--features ssh-gateway` where the phase touches those paths)
3. `cargo test` (default features) and the feature-gated suites the phase touches
4. Full regression before the phase is called complete — zero regressions
5. **Red-check**: every new gate must be shown to FAIL with the fix reverted.
   `[[feedback-inprocess-test-false-pass]]` applies with force here: in-process
   async tests have false-passed both a real leak and a real flush bug in this
   codebase. Phases 1 and 3 must gate at the mock/io-trait level, and the
   staging-style harness in `scripts/perf/` is the belt-and-braces check
6. Documentation in the same change (`README.md` is the single source of truth;
   not in `README.md` ⇒ not done)

## Acceptance harnesses that already exist

The campaign left a working measurement apparatus. No phase needs to build one
from scratch — see §9 of the evidence document for the runbook and §9.6 for the
nine harness pitfalls that produced wrong conclusions.

| phase | harness | acceptance signal |
| --- | --- | --- |
| 01 | `scripts/perf/vhost_registration_leak_repro.sh` | label released and re-registrable within the timeout, on both transports |
| 02 | `scripts/vhost_udp_concurrency_repro.sh`, plus `vhost_remote_stability.sh g8`/`g9` | RSS bounded server-wide; no stall cliff |
| 03 | `scripts/perf/vhost_bulk_latency.sh` | p50 under one/two bulk transfers, per transport, per carrier count |
| 04 | `scripts/perf/vhost_remote_stability.sh g8` | fresh-request latency behind 256/512 concurrent connections |
| 05 | `scripts/perf/vhost_netem_matrix.sh` (UDP-blackhole case), `g5` | first-request survival; 502/504 instead of `http=000` |
| 06 | — | `README.md` and `docs/vhost/` state the transport recommendation and the sizing rule |
| 07 | new, spike-local | measured page-load delta from a remote client at 20–100 ms RTT |

## Handoff

Phases are written to be implemented by Sonnet, one subphase at a time, without
needing this overview in context: each subphase names its files, its symbols,
its invariants and its gate. `resume.md` tracks state across sessions.
