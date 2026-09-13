# VPN real-path campaign — evidence (2026-09-12)

Campaign 3 of the performance series, after the secret-tunnel and file-transfer
campaigns. Subject: `bore vpn` in every operational mode, over a **real Internet
path**, hunting bugs, races, leaks and optimizations, with the standing goal of
maximizing bandwidth and minimizing latency.

This document is the raw evidence and the method. The conclusions in Italian are
in [`final_vpn_perf_review.md`](final_vpn_perf_review.md).

---

## 0. Why this campaign exists

`scripts/vpn_bench.sh` already measures the VPN thoroughly, over network
namespaces with `netem` shaping. The prior assessment states its own limit, and
that sentence is the whole mandate for this campaign:

> netns + `netem` ≠ a real Internet path (single FIFO, synthetic loss). Numbers
> are directional, not absolute. **Real-path validation still required.**
> — `docs/vpn/VPN_BANDWIDTH_ASSESSMENT.md`

Everything here is therefore measured over a real WAN: a home FTTH connection
behind a real NAT on one end, an AWS instance in the same region on the other,
with real queueing, real loss and a real router.

---

## 1. The setup, and how to rebuild it in a month

### Topology

```
  workstation (home NAT, 16 cores)  <---- real Internet ---->  test VM (AWS, 2 cores)
            |                                                        |
            +-------------- bore server (staging, same region) ------+
                          (relay path only; direct path is peer-to-peer)
```

The workstation is always the **connector** (dialer) and the VM the **listener**.
That places the home NAT on the dialing side, which is the interesting direction
for traversal, and keeps the stable endpoint fixed.

The staging server brokers the link but is **not** on the direct path. Benchmark
load is deliberately kept off it: it carries the operator's own live tunnels.

### Harness

Everything lives under `scripts/perf/staging/vpn/`, driven by `vpnlib.sh`:

| script | what it answers |
|---|---|
| `vpn_ab.sh` | relay vs direct vs bare, both directions, paired |
| `vpn_profile.sh` | flow ladder, latency, bufferbloat, UDP loss, per path |
| `vpn_lat.sh` | latency idle and under load, each direction separately |
| `vpn_sndbuf.sh` | the datagram send-buffer ladder |
| `vpn_hub.sh` | hub mode over the real path, two spokes behind one NAT |
| `vpn_direct_deficit.sh` | teardown of the direct path's upload deficit (V-10) |
| `vpn_udpbuf.sh` | generalised buffer ladder, any knob via `KNOB=` |
| `vpn_cc_matrix.sh` | congestion controller / GSO matrix (V-12) |
| `vpn_txqueue.sh` | the TUN `txqueuelen` ladder that found the fix (V-10) |
| `vpn_wire_ceiling.sh` | offered-rate UDP ladder, with an `ARMS` attribution axis: is ~400 Mbit/s a ceiling, and what sets it? (§11f, open question 2) |
| `vpn_modes.sh` | gateway `--advertise`, netmap `real@virtual`, `--forward-accept` |
| `vpn_stability.sh` | leak and reconnect hunt: RSS, threads, fds across cycles |
| `link_baseline.sh` | qualifies the ACCESS LINK from sources that never touch bore (V-9) |

`link_baseline.sh` is the one to run FIRST on any day whose absolute numbers are
going to be quoted; §11b is what happens when it is not.

Root is needed only to create a TUN, so only that part runs as root:
`scripts/vpn_tun_endpoint.sh` starts/stops one local endpoint and nothing else.
The drivers run unprivileged. This matters on this workstation because sudo is
granted **per exact path** and runs with `env_reset`.

### Prerequisites

* `~/.config/bore-perf/env.sh` (chmod 600) with the deployment coordinates and
  secrets. Secrets are never written into the repository.
* A release binary built with `--features vpn`, deployed to the VM as
  `~/bore-vpn` with a **matching checksum** — both ends must be the same bytes
  or the run measures two different programs.
* Inbound **TCP and UDP 5299** on the test VM's security group, from the
  workstation's address. Without it there is no bare-path control at all: the
  group otherwise admits only TCP/22, and both endpoints sit behind stateful
  filters so neither can reach the other unprompted.

### Leaving the host clean

A VPN endpoint edits the machine: interface, routes, possibly `ip_forward` and
nft/iptables rules. `vpnlib.sh` captures the relevant host state before anything
starts and `vpn_assert_clean` re-reads it on EXIT, reporting any interface,
route or `ip_forward` change that survived. Teardown is SIGTERM so the RAII
revert actually runs — SIGKILL would defer it to the next run's stale-reclaim,
which is a different code path and not one a benchmark should exercise by
accident. Every run in this document ended with `host clean`.

---

## 2. Measurement rules inherited from the earlier campaigns

These were paid for in earlier campaigns and are not re-litigated here:

1. **Interleave the arms.** This workstation drifts 14 % on identical code, and
   the far end is a burstable instance. Two whole sweeps never compare two
   things; arms alternate within each repetition and the quoted figure is the
   median ratio.
2. **A crashed cell must never look like a measurement.** Every arm that failed
   to reach its intended path is printed as `FAILED` and excluded, never
   averaged in.
3. **Read the path back, never assume it.** A VPN link always starts on the
   relay and upgrades in the background, so "the direct arm" is a state to wait
   for and verify, not a flag that was passed.
4. **Never `pkill bore`.** Processes are killed by recorded PID or by a per-run
   unique id. This host and the server carry the operator's live tunnels.

### A new rule this campaign added

5. **Kill remote helpers by PID, never by pattern.** `ssh host "pkill -f 'iperf3
   -s -p 5299'; ... iperf3 -s -p 5299 ..."` puts that exact string in the remote
   shell's own argv, so `pkill -f` matches the launcher and kills the session
   that was about to start the server. The symptom is an iperf3 that never
   listens and a benchmark that reads `connection refused` as a tunnel fault.
   Cost before it was found: one whole A/B sweep that reported 0 Mbit/s on every
   arm.

6. **In a `set -euo pipefail` harness, an assertion's own `grep` must not be
   able to end the run.** "No match" is grep's exit 1, and that is a perfectly
   ordinary outcome for a check that is allowed to fail — but under `set -e` it
   terminates the script, and because the EXIT trap still runs the cleanup the
   script exits **0**. The suite stops at that assertion and looks like it
   passed. Measured, on the new `T-NAT-DIAG-ROUND` gate: the run printed 26
   PASS lines, no FAIL line, exit code 0 — and had silently skipped the three
   Fase 7 cells and the final tally, which is the line a reader actually
   believes. Every assertion grep is now wrapped `{ grep … || true; }`.
   Corollary worth stating plainly: **a green run that does not end with its
   own PASS/FAIL total has not been read.**

---

## 3. The bare-path reference

Measured with the tunnel down, same hosts, same iperf3:

| direction | Mbit/s |
|---|---|
| upload, workstation → VM | 677 – 705 |
| download, VM → workstation | 369 – 416 |

The path is **asymmetric** and the download side is the narrow one. It also
**moves**: one repetition caught the download at 125 Mbit/s and the upload at
397. That is the home link, not the tunnel, and it is exactly why every arm is
paired against a bare sample taken in the same repetition.

Latency reference: TCP handshake to the VM, median 20.0 – 23.0 ms. ICMP to the
VM is dropped by its security group, so ping is not available as a bare
reference; the direct path's own QUIC carrier RTT (below) is the better control
anyway.

---

## 4. Finding V-1 — hub mode was a full traversal generation behind (FIXED)

**The standing instruction this came from:** *"le ottimizzazioni del nat
traversal devono essere applicate ovunque la tecnica viene utilizzata."*

An audit of every site in the codebase that punches found three peer-to-peer
subsystems and only one of them fully current:

| subsystem | check round (Fase 2) | adaptive plan (Fase 3) | sprayed escape (Fase 7) | pair cache |
|---|---|---|---|---|
| secret tunnels | yes | yes | yes | yes |
| VPN 1:1 | yes | yes | yes | yes |
| **VPN hub** | **no** | **no** | **no** | **no** |
| **`bore test-udp`** | **no** | partial | **no** | **no** |

### Mechanism

The hub client built its candidate offer by hand:

```rust
crate::shared::UdpCandidateOffer {
    candidates: disc.candidates,
    selected_stun: disc.selected_stun.map(|s| s.requested),
    peer_id,
    ..Default::default()          // <-- no typed candidates, no capabilities, no profile
}
```

and the server threw away what little was left, storing only the bare address
list (`set_hub_candidates(peer_id, cands, stun)`), then brokered with an explicit
`v2: None` to both sides.

The consequences chain: no profile on one side means the broker cannot compute
an adaptive plan (by design — a missing profile must never produce `RelayOnly`);
no plan means the spoke's `check_cfg` is `None`; a `None` check config means both
sides fall through to the legacy blind punch. So hub spokes got **no
authenticated check round, no plan ordering, no sprayed escape, no winning-pair
cache — and no S-5**, the fix that lets a listener stop probing an address that
has already gone quiet.

It was self-consistent, which is why it was invisible: both sides degraded
together, so nothing ever failed. It was simply the 2025 traversal path still
running in 2026.

### Fix

* `PeerSlot` stores the hub's **whole** offer, not the address list.
* `set_hub_candidates` → `set_hub_offer(offer)`.
* The broker computes a v2 rider for **each** side from the pair of offers —
  each peer receiving the plan from its own perspective, the same shape the 1:1
  and secret brokers use — and **normalizes the generation** across both riders
  (`max` of the two offers), because check frames carry the generation and a
  mismatch is silent mutual rejection.
* The hub client sends `disc.to_offer(peer_id, generation)` and runs
  `listener_checks_then_quic` when a plan is present, with a per-round
  generation counter.
* Absent a plan, everything is byte-identical to the legacy path — the standing
  rule for every traversal capability on this wire.

Gate: the netns hub suite (`T-HUB*`/`T-HUBD*`, run on both relay and direct).

---

## 5. Finding V-2 — the diagnostic is weaker than the thing it diagnoses

`bore test-udp --tcp-secret-id` is the tool an operator runs to find out whether
the direct path will work. It consumes the adaptive plan (candidate ordering,
retry budget, pacing, `RelayOnly`) but `establish_direct` calls
`DirectListener::new` / `connect_direct` — the **blind punch**. It has no
authenticated check round and no sprayed escape.

So on the one NAT cell that Fase 7 exists to rescue — exactly one symmetric side
against a port-restricted peer — the diagnostic reports **relay** while a real
secret tunnel or VPN link on the same pair now goes **direct**. A diagnostic
that under-reports the product's own capability is worse than a slow one: it is
a lying oracle, and this repository's own notes name `test-udp` as the oracle for
the secret campaign's window-floor result.

### The fix

`establish_direct` now takes `Option<&CheckConfig>` and, when it is `Some`,
enters `listener_checks_then_quic` / `dialer_checks_then_quic` — the **same
functions** the secret and VPN 1:1 paths call, not a reimplementation of them.
The `None` arms are byte-identical to the old blind punch.

Three decisions in that change are policy rather than mechanics, and each one
is a gate:

* **The round is gated on the PEER's capability, not on a version.** The gate
  travels as `UdpTestPeerSummary.checks`, an additive `#[serde(default)]` field
  read off the *other* peer's summary — DEC-VE2's exact shape, for DEC-VE2's
  exact reason. A round nobody answers is indistinguishable from a network that
  ate the frames, which is precisely the false negative this tool exists not to
  produce, so a peer built before the field existed keeps BOTH sides on the
  blind path. The local side asserts the flag from `cfg!(feature = "udp")`: a
  binary compiled without `udp` has no round to run and telling the peer
  otherwise would strand it waiting.
* **The winning-pair cache is deliberately not consulted** (`cache_key: None`).
  A diagnostic that recalled a pair from a previous run would answer a question
  about the past. Every run measures a cold pair.
* **The spray role is taken only from a SERVER-brokered plan.** The two halves
  of the Fase 7 escape must be complementary and only the broker sees both
  profiles; two locally computed `plan_for_pair` results can independently
  choose the same role, and two "easy" sides spray at each other and open no
  filter.

The honest-output line moved with the behaviour: the report now prints
`Candidate order    : enforced (authenticated check round, planned kind
groups — the same path a real tunnel takes)` against a capable peer, and the
legacy note — naming the peer as the reason — against one that is not.

Gates, following the project's unit-pins-policy / netns-pins-wiring standard:
`check_config_is_none_for_a_peer_that_cannot_answer_the_round`,
`spray_role_is_taken_only_from_a_server_brokered_plan` and
`typed_peer_candidates_refuses_a_length_mismatch` pin the policy (all three
red-checked: reverting each of the three rules makes exactly its own test
fail), and netns `T-NAT-DIAG-ROUND` pins the wiring by running paired
`bore test-udp` across a real NAT pair and asserting both that the round ran on
both sides and that the diagnostic's verdict agrees with the product's.

Status: **fixed**.

---

## 6. Finding V-3 — the deferred send-buffer item, falsified

The prior assessment deferred right-sizing the direct path's datagram send
buffer with the note that it *"bufferbloats RTT (~116 ms vs a 20 ms link) and
makes backpressure engage late"*. The reasoning was sound: the 1:1 uplink sends
through `send_batch_wait`, which **awaits** room rather than letting quinn drop
the oldest datagram, so this buffer **is** the uplink queue and backpressure
engages exactly when it fills.

It was also deferred as risky because the buffer is "shared with transfer/vhost".
That turns out not to be true for the *send* side: QUIC **datagrams** are used by
no other subsystem — secret, vhost, public and ssh-jump direct paths all carry
bytes on bidirectional *streams*. So the knob is VPN-only and safe to move.

Measured, interleaved, 3 repetitions, throughput and loaded RTT sampled inside
the **same** transfer (`vpn_sndbuf.sh`):

| send buffer | upload | RTT under load |
|---|---|---|
| 8 MiB (shipped) | 468 Mbit/s | 52.3 ms |
| 2 MiB | 481 Mbit/s | 54.5 ms |
| 512 KiB | 468 Mbit/s | 54.0 ms |

**Sixteen times the depth changes neither quantity.** Every rung sits inside the
run-to-run spread of the bare path measured alongside them (552–696 Mbit/s).

The hypothesis is falsified, and the reason is instructive: quinn only buffers
what the congestion window will not yet accept, and on this path the inner TCP
never outruns the window by megabytes — so the buffer never fills and its depth
is not what the loaded RTT is made of.

**Outcome:** the default stays 8 MiB. A knob
(`BORE_DIRECT_DGRAM_SEND_BUF`) was added because the question deserved to be
answerable, and it has a genuine use — hub mode holds one direct connection per
spoke, so an operator with many spokes on a small instance can lower it at no
measured cost. The reasoning and the numbers are recorded on the constant so the
hypothesis is not retried.

Two compile-time assertions pin that the default is also the ceiling.

---

## 7. Finding V-4 — the MTU trap, which cost this campaign a whole result set

The first throughput pass pinned `--mtu 1280`, reasoning from an early log line
reading `max_datagram=Some(1288)`.

That line is the value **fifteen seconds into the link**. quinn starts MTU
discovery low and probes upward; on this path:

```
t+5 s    tun MTU adjusted to QUIC path MTU old=1350 new=1288
t+25 s   tun MTU adjusted to QUIC path MTU old=1288 new=1414
         quic_mtu=1452 max_datagram=Some(1414)   (stable thereafter)
```

So the link settles at **1414**, and pinning 1280 handed the tunnel an MSS of
1240 against the 1460 the bare path was using — a **10.8 % handicap applied to
the tunnel arm only**, and silently, because a pinned MTU produces no churn to
notice. Single-flow TCP throughput is proportional to MSS, so a large part of the
"tunnel is 30 % below bare" result that pin produced **was the pin**.

The harness no longer pins by default. It waits for the MTU to stop moving and
records the value each arm actually ran at.

The quiet window matters too: an 8 s window was tried first and settled on 1288
every time, because a ~20 s plateau separates the two real changes. It is now 24 s.

**This is also the deferred `initial_mtu` item, now quantified:** a link spends
its first ~25 s at an MTU 126 bytes below its working value, and takes two TUN
MTU changes to get there. For a long-lived VPN that is negligible; for a
short-lived link, a benchmark, or anything measured in its first half-minute it
is not.

---

## 8. Finding V-5 — the direct path adds no latency

The control here is better than a bare ping would have been: the direct path's
own QUIC carrier continuously measures the **outer** path RTT, on the same
5-tuple at the same instant as the tunnel RTT it is compared with.

| | idle RTT |
|---|---|
| outer QUIC path (carrier self-report) | 21.3 ms |
| **tunnel, direct** | **20.4 ms** |
| tunnel, relay | 25.3 ms |
| TCP handshake reference | 20.0 – 23.0 ms |

The tunnel RTT is at or below the outer path's own estimate (QUIC's estimate
includes ack delay), so the VPN data plane's latency cost is **not measurable**
on this path. The relay's extra ~5 ms is the second hop through the server,
which is what it is.

---

## 9. Finding V-6 — queueing is direction-specific, and the relay is the worse offender

Ping sampled throughout a saturating transfer, each direction separately:

| path | idle | download load | upload load |
|---|---|---|---|
| relay | 25.3 ms | **90.1 ms** (×3.56, max 202) | 58.6 ms (×2.32) |
| direct | 20.4 ms | 28.2 ms (×1.37) | **59.6 ms** (×2.90, min 39.3) |

Two things worth separating:

* **The relay bloats badly on download** — +64.8 ms average, 202 ms peak. This is
  the path the prior assessment's "~116 ms" figure most likely describes; it is
  not a property of the direct path.
* **The direct path bloats on upload**, and the *minimum* under load is 39.3 ms
  against an idle maximum of 22.4 ms. A minimum above the idle maximum is a
  **standing queue**, not jitter — something is holding roughly 19 ms of packets
  continuously.

§6 rules out the QUIC datagram send buffer as that queue. The remaining
candidates are the TUN device queue (`qlen 500`, `fq_codel`) and bore's own
internal uplink channels. **Open — see §11.**

---

## 10. Finding V-7 — a single flow already saturates, and carriers do not change that

Flow ladder, aggregate throughput (per-flow in parentheses):

| flows | relay down | direct down | relay up | direct up |
|---|---|---|---|---|
| 1 | 411.9 | 378.0 | 433.6 | 489.9 |
| 2 | 370.4 (185) | 388.6 (194) | 504.0 (252) | 582.0 (291) |
| 4 | 359.6 (90) | 412.6 (103) | 493.4 (123) | 635.0 (159) |
| 8 | 358.1 (45) | 384.2 (48) | 505.6 (63) | 584.8 (73) |

**Download is flat** across an 8× change in flow count — it is path-limited at
~375 Mbit/s (the bare path reads 369–416), and the tunnel is already at 100 % of
it. **Upload gains ~30 %** from 1 to 4 flows (490 → 635, against a bare 693) and
then stops.

With `--carriers 4 --tun-queues 4`, one flow still reads 467 Mbit/s and four
flows 625 — i.e. carriers add nothing to a single flow, exactly as documented
(direct carriers are flow-pinned; one inner flow rides one carrier), and nothing
beyond what four flows already achieved on a single carrier.

This **qualifies** the prior assessment's F1. Its conclusion — the gap is the
single inner TCP flow, so parallelize the workload — holds for **upload** and is
false for **download** on this path, where a single flow is already at the
ceiling and there is nothing to recover.

UDP at 300 Mbit/s ran at **0 – 0.05 % loss** on both paths, which is consistent:
with almost no loss there is little for the Mathis bound to bite on.

---

## 11. Finding V-8 — neither endpoint is CPU-bound

Measured during a saturating upload:

| | CPU |
|---|---|
| VM (receiver: AEAD open + TUN write), whole host | 32.7 % of 2 cores |
| VM `bore` process | 17.4 % of one core |
| workstation `bore` process | 20 % of one core (2.0 core-seconds / 10 s) |
| VM during *bare* transfer, whole host | 8.0 % |

So the upload gap to bare is **not** CPU on either side, and the standing queue
in §9 is not a busy pump. Both remain open.

---

## 11b. Finding V-9 — the access link must be qualified before any number is quoted

Prompted by a direct challenge: *"ho provato a fare speedtest con Ookla e con
Netflix. La mia workstation satura la connessione in download. Quindi il fatto
che bore non riesca, va verificato."* That is exactly the right objection, and
checking it changed how this campaign reports absolute figures.

The bare-path reference in §3 (download 369–416 Mbit/s) was taken earlier. On
2026-09-12 afternoon the same measurement read 147–160, so the question "is the
tunnel slow or is the line slow?" had to be settled from sources that do not
pass through bore at all:

| source, same 10-minute window | download | upload |
|---|---|---|
| Ookla, Speednetweb IT (6.2 ms) | **132** | 615 |
| Ookla, **TIM** — the subscriber's own ISP (12.6 ms) | **164** | 388 |
| Ookla, Fastweb IT (11.6 ms) | 152 | 334 |
| iperf3 bare → AWS Milan, P=1 | 149 | 82 |
| iperf3 bare → AWS Milan, P=8 | 155–160 | 413–418 |
| CacheFly CDN, 8 parallel streams | 176 | — |

Every independent source agrees, including a server inside the subscriber's own
ISP. **P=1 and P=8 give the same figure**, which is the diagnosis in one line: a
per-flow limit (window, loss, Mathis) opens up with parallelism and a policer
does not. The downlink was genuinely delivering ~150 Mbit/s that afternoon.

Two things were explicitly **excluded** rather than assumed:

- **The WiFi radio.** The workstation is on `wlp0s20f3` (WiFi 6, 80 MHz,
  MCS 11 NSS 2 → 1200.9 Mbit/s PHY, −52 dBm). Across a probe that received
  112 MB it logged **`tx retries +8`, `rx drop misc +0`**, and the same radio
  carries 604 Mbit/s of upload. A slow radio and a lossy one look identical from
  a throughput number and opposite from the error counters.
- **The far end.** The test VM is a `c7i-flex.large`, and all five ENA
  allowance counters (`bw_in`, `bw_out`, `pps`, `conntrack`, `linklocal`) read
  **0**. Egress-from-VM is the download direction, so a shaped instance would
  have been the easiest wrong answer to reach.

**Consequence for this document:** ratios quoted against a bare control sampled
in the same repetition remain valid on any link; absolute Mbit/s figures taken
on 2026-09-12 afternoon are **not** the product's capability. `V0`
(`link_baseline.sh`) now records the line's condition at the start and end of a
campaign so a result file can be re-qualified later instead of re-argued.

An **ethernet** re-run is outstanding (`eno0` had no carrier); it is the control
that removes the last doubt, even though the radio is already exonerated.

---

## 11c. Finding V-10 — the direct path's upload deficit is the TUN transmit queue (FIXED)

**The user's framing, which turned out to be the important one:** *"direct lose
37%. Questo dobbiamo ottimizzare. E nota che questo problema lo abbiamo avuto in
tutte le campagne."* Correct on both counts — P-13 (vhost) and the secret
campaign both found the direct path behind the relay, and a deficit that recurs
across four independent subsystems is a property of the transport, not four
coincidences.

### What was measured

Paired A/B, bare control in the same repetition:

| direction | bare | relay | direct |
|---|---|---|---|
| upload | 414.5 | 415.7 (**100.3 %**) | 259.7 (**62.7 %**) |
| download | 150.7 | 132.1 (87.6 %) | 138.7 (92.0 %) |

The relay *reaches* the bare path. That single fact exonerates everything the
two transports share — the link, the AEAD seal, the TUN, the inner TCP, the far
end — and localises the fault to the QUIC datagram path.

### What it is not

Each of these was measured and rejected, in this order:

| hypothesis | evidence against |
|---|---|
| CPU | 2.44 s over 20 s = 12 % of one core; **busiest thread 3.1 %** |
| packet loss | `lost_pct=0.00`, `cong_events_d=0` throughout |
| the far end | VM CPU ~0; ENA allowance counters all 0 |
| the link | bare UDP does **537–547 Mbit/s at ~0 % loss**, *faster* than bare TCP |
| QUIC datagram send buffer | ladder 8 MiB → 256 KiB: 265.9 / 262.8 / 249.2 / 260.1 Mbit/s — flat (reproduces §6's falsification) |
| UDP socket send buffer alone | ladder 16 MiB → 256 KiB moved the **carrier's** rtt 163 → 153 ms but not goodput |
| congestion controller | see V-10b |
| GSO bursts | segmentation offload off made it *worse* (251.1 Mbit/s) |

### The mechanism

The tunnel's own wire capacity is ~375–380 Mbit/s (inner UDP delivers 376 of
500 offered; `tx_mbit_s` independently reports 374–382). Inner TCP gets only
~250. The gap is **latency**, and the arithmetic closes exactly:
`net.ipv4.tcp_wmem` max is 4 MiB here, and 4 MiB / 145 ms = 231 Mbit/s.

So the RTT had to be explained. Sampled by TCP handshake to a port *outside*
the tunnel, so the tunnel cannot flatter itself:

| load | RTT avg |
|---|---|
| idle | 22 ms |
| **bare UDP pushing 375 Mbit/s** | **19 ms** |
| **bore direct pushing the same 375 Mbit/s** | **190–320 ms** |

Same link, same instant, same wire rate, 100× the queue.

The location took three attempts because **latency is conserved across stages in
series**: bounding the UDP socket buffer alone took the carrier's rtt from 89 ms
to 35 ms and left the tunnel rtt at 134 ms; bounding the datagram buffer *and*
the socket buffer together took the carrier to 30–45 ms and **still** left the
tunnel at 134–198 ms. Each bound simply re-formed the queue one stage earlier.

`tc -s qdisc show dev boreN` reported `backlog 0b 0p` throughout, which looks
like proof of no queue and is the opposite. **A TUN has two queues in series:**
the qdisc, and then the device's own skb queue bounded by `txqueuelen`, which
the application reads from. The qdisc only builds a backlog once the *device*
queue is full, so a deep device queue makes the fq_codel the system installs by
default completely inert.

That bound is in **packets**, and this path runs with TUN offload: `ip -s link`
showed 603 MB carried in 17 827 entries, **~34 KB each**. So the kernel default
`txqueuelen 500` is not 500 × 1414 B = 707 KB — it is up to ~17 MB, or ~360 ms
at 375 Mbit/s. bore creates this TUN and never set the value.

### The ladder

One link held across the whole ladder, so only this value changed (medians of
2–3 interleaved repetitions, inner TCP upload):

| txqueuelen | goodput | rtt avg |
|---:|---:|---:|
| **500 (kernel default)** | **248.3** | **147.0 ms** |
| 256 | 272.3 | 110.6 ms |
| 192 | 277.6 | 114.5 ms |
| 128 | 265.7 | 97.0 ms |
| 64 | 258.2 | 92.3 ms |

The kernel default is the worst rung on **both** axes. The default is 128 rather
than 64 or 192 because an earlier sweep that changed the value while the inner
flow was already in loss recovery collapsed at 32 (91.5 Mbit/s) and at 8
(5.6 Mbit/s): the useful region has a cliff below it, so the shipped value sits
in the middle of the good region, not at its edge.

### The fix

`hostcfg::VPN_TUN_TXQUEUELEN` = 128, applied in the Linux `create_tun` twin,
overridable with **`BORE_VPN_TUN_TXQUEUELEN`**; `0` leaves the kernel value
untouched, which is the escape hatch for a path where this measurement does not
hold. Best-effort by design: a kernel that refuses the write leaves a working
tunnel with the old latency rather than failing to bring the link up over a
tuning value. Gate: `tun_txqueuelen_resolution`.

**Result**, same A/B re-run after the change: direct upload **62.7 % → 70.6 %**
of bare (259.7 → 272.5 Mbit/s), and direct now beats the relay in both
directions (`direct/relay` upload 1.03, download 1.31).

This does **not** close the gap to bare. Inner TCP at 272 against a tunnel wire
capacity of ~375 means latency is still the binding constraint, and ~375 against
540 Mbit/s of bare UDP is a second, separate deficit that is still open.

---

## 11d. Finding V-11 — a locale bug that silently corrupted this campaign's medians

`sort -n` is locale-dependent. Under `it_IT.UTF-8` the decimal separator is a
comma, so `{397.46, 264.01, 408}` sorts to `408, 264.01, 397.46` and the median
reads **264.01**. The shared `scripts/perf/staging/lib.sh` `med()` has always
pinned `LC_ALL=C`; the VPN stages written during this campaign each defined
their own local `med`/`med2` and did not.

Caught because a reported median (relay upload 264.01) was not any plausible
centre of the three values printed directly above it in the same file. Every
median from the VPN stages was recomputed from the per-repetition raw values,
which were never affected. Exactly one published figure changed: relay upload
after the txqueuelen fix, **68.4 % → 103.0 % of bare**. No conclusion depended
on it.

`export LC_ALL=C` now sits in `vpnlib.sh`, which every VPN stage sources, so a
helper defined locally in a future stage cannot reintroduce it; the local
helpers pin it as well.

**Rule added:** a harness that prints both raw samples and a summary statistic
must print the raw samples, precisely so the summary can be checked against
them. This bug is invisible in a file that reports only medians.

---

## 11e. V-12 — the congestion controller, re-measured after the locale bug

The controller matrix was first read as "not the lever". That read was produced
by the sorting bug of §11d: with the medians recomputed, **BBR was the worst of
the three arms**, which is the opposite conclusion. Because a default was at
stake, the matrix was re-run at 4 repetitions rather than argued from the
recomputed 2.

`scripts/perf/staging/vpn/vpn_cc_matrix.sh`, 4 interleaved repetitions, direct
path, TUN queue already bounded to 128, inner TCP upload. The bare control was
sampled in the same repetitions and reads 423.3 Mbit/s (raw 419 447 411 428).

| arm | median | raw samples | rtt avg | rtt **min** | cwnd |
|---|---:|---|---:|---:|---:|
| `bbr` | 272.0 | 273 273 272 264 272 | 123.0 ms | 26.1 ms | 11 MB |
| `cubic` | 278.6 | 287 279 267 282 279 | 116.0 ms | 26.8 ms | 37 MB |
| `newreno` | 280.9 | 286 281 281 292 281 | 108.3 ms | 33.4 ms | 37 MB |

The separation is clean and is not this workstation's drift: `bbr`'s **best**
repetition (273) is below `newreno`'s **worst** (281), and the arms are
interleaved inside each repetition precisely so drift cannot produce that.

**The default stays `bbr` anyway.** Recorded here as a decision, not an
oversight:

- The prize is **+3.3 %**. The deficit this campaign is chasing is 30 %; a
  controller swap is not where it lives.
- `newreno` buys the average rtt by raising the **minimum** under load
  (26.1 → 33.4 ms). The minimum is the standing queue — the same quantity §11c
  spent the campaign removing — so part of the "win" is queue this path had just
  been cleared of.
- The measurement is on a bottleneck with `lost_pct=0.00` (§11b: the radio's own
  counters show `rx drop misc +0` across 112 MB). A loss-based controller
  measured only where nothing is lost has not been measured on the case it is
  worst at, and a default ships to every path, not to this one.

`BORE_DIRECT_QUIC_CC` exists exactly so an operator who has qualified their link
(§11b) can take the 3 %. Raw output: `out/vpn/cc-matrix-4rep.txt`.

---

## 11f. V-13 — the second deficit, measured with the right instrument

### The instrument was the problem first

Every earlier measurement of "the tunnel's wire capacity" was taken through an
inner TCP flow. That is the wrong instrument for a ceiling: inner TCP REACTS to
the quantity being measured, so a pipe with a hard limit and a pipe that is
merely being congestion-controlled produce the same throughput number. The
ladder in `scripts/perf/staging/vpn/vpn_wire_ceiling.sh` drives the tunnel with
UDP at a FIXED OFFERED RATE instead. A pipe with a ceiling delivers the ceiling
and loses the rest; a pipe with headroom delivers what it is offered.

One link is held across the whole ladder (V-10's rule), the bare path is
measured in the same repetition, and every rung reads three counters so a lost
datagram can be ATTRIBUTED: `UdpSndbufErrors` (the kernel refused the send),
quinn's `lost_pct` (it left and was never acknowledged) and the TUN's
`tx_dropped` (it never reached bore at all).

**A harness correction worth recording**, in the same family as V-11: `iperf3
-u -b R` reports `bits_per_second` as the rate it OFFERED, not the rate that
arrived — at 540 Mbit/s offered with 22 % loss it still prints 539.9. The first
run of this ladder quoted that column and made the tunnel look like it kept up
with every rung it was actually failing. Delivered is `offered × (1 − loss)`,
and the stage now prints both.

### The result

Direct path, workstation → AWS eu-south-1, TUN queue at the shipped 128,
MTU 1414. Delivered Mbit/s (loss %):

| offered | bare | tunnel, rep 1 | tunnel, rep 2 | TUN `tx_dropped` |
|---:|---:|---:|---:|---:|
| 200 | 200.0 (0) | 200.0 (0) | 200.0 (0) | 0 |
| 300 | 300.0 (0) | 300.0 (0) | 300.0 (0) | 0 |
| 375 | 375.0 (0) | 375.0 (0.01) | 375.0 (0.01) | 0 |
| 450 | 449.9 (0) | 415.8 (7.59) | 414.5 (7.88) | 31 294 / 32 476 |
| 540 | 539.9 (0) | 419.4 (22.33) | 412.8 (23.56) | 110 626 / 116 733 |

The tunnel carries **every offered rate up to 375 Mbit/s with no loss at all**,
and above that it tops out at roughly **400–420 Mbit/s delivered** while the
bare path on the same 5-tuple in the same repetition carries 540 cleanly.

Two properties of that table matter more than the numbers:

* **All of the loss is `tx_dropped` on the TUN.** `UdpSndbufErrors` is 0 or 1
  across the whole ladder and quinn's `lost_pct` is 0.00 at every rung. The
  packets are not lost on the wire, not refused by the socket and not
  unacknowledged by QUIC — they never reach bore. The kernel could not hand
  them to the device queue because the device queue was full, which means the
  application was not draining it fast enough.
* **Delivered is NOT monotone in offered.** Pushing 540 delivers *less* than
  pushing 450 (376–403 against 388–427). That is the signature of a drop-tail
  queue in front of a fixed-rate drain, not of a path running out of capacity.

### What it is not

The ladder was re-run at 375/450/540 with per-rung CPU sampling and across two
congestion controllers, one link per controller:

| controller | 375 | 450 | 540 | process CPU | busiest thread |
|---|---:|---:|---:|---:|---:|
| `bbr` | 370.0 / 374.9 | 426.8 / 387.7 | 376.8 / 389.2 | 13.7–18.6 % | 4.2–4.6 % |
| `newreno` | 375.0 / 375.0 | 400.0 / 396.2 | 403.2 | 13.8–18.9 % | 4.0–5.3 % |

* **Not CPU.** 13–19 % of ONE core moves 400 Mbit/s through AEAD and QUIC. With
  a caveat about the instrument, recorded because it nearly misled: on a
  multi-threaded tokio runtime a single serialised task MIGRATES across worker
  threads, so per-thread CPU cannot detect a serialisation bottleneck and the
  4–5 % "busiest thread" column proves nothing on its own. The process total is
  the figure that carries the conclusion, and it is low.
* **Not the congestion controller.** Two controllers with completely different
  models land on the same ceiling. A limit both of them independently respect
  is below what either of them would impose.
* **Not the UDP socket send buffer** (`UdpSndbufErrors` ≈ 0), **not QUIC loss**
  (`lost_pct` 0.00), and **not the access link** (bare carries 540 in the same
  repetition).

### What it is: a drop-tail queue under a source that does not back off

The uplink is not busy and is not being paced, yet it is not draining the TUN.
The remaining candidate was the depth of quinn's datagram send buffer — the
queue the uplink awaits room in (BW-F3) — and the way to test a queue is to
walk it. `vpn_wire_ceiling.sh` grew an `ARMS` axis for that
(`label|env|ws-flags|streams|vm-flags`), and the stage now samples the tunnel
RTT **inside** each tunnel rung, because every knob on this list is a queue and
a queue bought with throughput is paid for in delay.

First, the two structural candidates, both **falsified**. Same ladder, same
day, bare control in every repetition, at 540 Mbit/s offered:

| arm | delivered (both samples) | process CPU |
|---|---:|---:|
| base — 1 flow, 1 TUN queue, 1 carrier | 435.7 / 434.8 | 15.7–18.6 % |
| 4 inner flows, 1 TUN queue | 407.9 / 422.4 | 19.8–19.9 % |
| 4 inner flows, **4 TUN queues** | 415.5 / 417.3 | 21.9–22.5 % |
| 4 inner flows, **4 QUIC carriers** | 390.3 / 359.0 | 27.2–34.5 % |

**Four uplink tasks reading four TUN queues deliver no more than one**, so the
strictly serial read-batch-then-send-batch loop is *not* the wall. Four
carriers deliver *less* at nearly twice the CPU, which is the existing
`--carriers` invariant measured again rather than assumed. Deepening the TUN
device queue (`txqueuelen` 500) does not raise the ceiling either — it moves
where the drops happen, not how much arrives, which is the signature of a
drop-tail queue in front of a fixed-rate drain.

Now the queue itself. 540 Mbit/s offered, two repetitions, RTT sampled through
the same transfer:

| datagram send buffer | delivered (both samples) | RTT avg | RTT min |
|---|---:|---:|---:|
| 1 MiB | 389.2 / 388.4 | — | — |
| **8 MiB (shipped)** | 388.3 / 402.1 | 227 ms | 21 ms |
| 16 MiB | 431.7 / 445.4 | 427 ms | 23 ms |
| 32 MiB | 462.0 / 452.6 | 708 ms | 25 ms |
| 64 MiB | 409.8 / 428.4 | 1092 ms | 23 ms |

Three things in that table, and the third is the one that decides the default.

* **The buffer is on the critical path**, which §6 had concluded it was not.
  Both measurements are right and the difference between them is the
  instrument: §6 drove the ladder with an inner TCP flow, and inner TCP paces
  itself to the congestion window, so it never hands the uplink more than the
  window will take and the buffer never fills. A fixed UDP offer does not back
  off. A knob can be invisible under one load and load-bearing under another,
  and "we measured it once" is not the same as "we measured it".
* **The curve turns over.** 64 MiB is worse than 32 on delivered *and* worse on
  latency. This is not "deeper is better"; there is an optimum, and past it the
  extra queue buys nothing but delay.
* **Latency rises faster than throughput.** From 8 MiB to 32 MiB, delivered
  gains ~16 % and the loaded RTT triples. The RTT *minimum* stays at 21–25 ms
  throughout, which says the extra milliseconds are a standing queue and not a
  worse path — exactly the quantity V-10 had just removed.

### The decision: the default stays 8 MiB

The deliverable quantity for a VPN is inner TCP throughput, and inner TCP is
`window / RTT`. So the trade above has to be priced in the currency the user
actually spends it in. Re-measured with an inner TCP flow, 3 interleaved
repetitions, throughput and loaded RTT sampled in the **same** transfer
(`vpn_sndbuf.sh`, bare control 400.8 Mbit/s that afternoon):

| send buffer | upload | % of bare | RTT under load |
|---|---:|---:|---:|
| 32 MiB | 247.88 Mbit/s | 61.8 % | 141.3 ms |
| **8 MiB (shipped)** | 240.55 | 60.0 % | 129.5 ms |
| 2 MiB | 238.26 | 59.4 % | 129.0 ms |

**+3 % of throughput for +12 ms of latency.** On the workload that matters the
32 MiB rung is not a win, and on the workload where it *is* a win it costs half
a second of standing queue. The default stays where it is — now because it was
measured at the deep rung, not because the deep rung was never tried.

What *did* change is the range of the knob. `DIRECT_DATAGRAM_SEND_BUFFER_MAX`
used to equal the default, so `BORE_DIRECT_DGRAM_SEND_BUF` could only lower the
depth and this question could not be asked without editing the source — which
is the wrong place for an experiment to live. It is now 64 MiB: the operator
who is pushing bulk UDP through the tunnel and does not care about interactive
latency has the rung, and the evidence above says exactly what it costs.

### What this means for open question 2

There is no "~400 Mbit/s wire ceiling" in the sense the question assumed. The
tunnel does not stop at 400: it delivers whatever the queue depth lets it
deliver, and the price list is the table above. The residual gap to bare is a
**latency** gap, not a capacity one, which puts it in the same account as V-10
and makes the next useful experiment a latency experiment rather than another
throughput ladder.

One honest limit on all of it: `bbr` was the controller in every arm here, and
a queue-depth ladder on a `lost_pct = 0.00` path has not seen the case where a
loss-based controller behaves differently. Same caveat as V-12, same remedy —
re-open it with a lossy path in the matrix.

---

## 12. Open questions

1. ~~What is the direct path's upload standing queue made of?~~ **ANSWERED**
   (§11c): the TUN device transmit queue, `txqueuelen 500` × ~34 KB GSO entries.
   Fixed; `BORE_VPN_TUN_TXQUEUELEN` defaults to 128.
2. ~~What sets the tunnel's ~400 Mbit/s delivery ceiling?~~ **ANSWERED, and
   the question dissolved** (§11f). It is not a capacity limit: it is the
   operating point of a drop-tail queue under a source that does not back off.
   Delivered rises with the datagram send buffer (8 MiB → 388, 16 → 431,
   32 → 462 Mbit/s) and turns over at 64 MiB, while the loaded RTT triples
   (227 → 708 ms) with the RTT *minimum* flat — a standing queue, not a worse
   path. Falsified along the way: CPU (16–22 % of one core), the congestion
   controller, the UDP socket send buffer, QUIC loss, the access link, the TUN
   device queue depth, the serialisation of the uplink task (4 TUN queues and
   4 flows deliver no more than 1) and `--carriers` (4 carriers deliver *less*,
   at 1.7× the CPU). The default stays 8 MiB because under inner TCP — the
   workload that matters — 32 MiB buys +3 % for +12 ms. **The residual gap to
   bare is a latency gap, not a capacity one.**
2b. **Inner TCP is window-limited by the residual tunnel latency.** At 272
   Mbit/s and ~100 ms, `tcp_wmem` 4 MiB is the binding constraint. Lowering the
   tunnel's remaining latency should convert directly into throughput up to the
   ~375 ceiling.
3. ~~`bore test-udp` traversal alignment~~ **ANSWERED** (§5): the diagnostic
   now runs the authenticated check round through the same entry points the
   product uses, gated on the peer's advertised capability. Gated by three
   red-checked units and the netns cell `T-NAT-DIAG-ROUND`.
4. **Hub direct path on a real WAN.** The alignment fix is gated in netns; it has
   not yet been measured over the real path with multiple spokes.
5. **Per-peer PMTU in hub mode.** The TUN MTU is shared across peers (v1
   limitation); with peers on different paths, one peer's MTU governs all.
6. **Relay download bufferbloat** (§9, +64.8 ms / 202 ms peak) — the relay queue
   is bounded (`RELAY_QUEUE = 512`) and applies backpressure; whether 512
   datagrams is the right depth has not been measured.

---

## 13. Raw output

Under `out/vpn/`:

| file | stage |
|---|---|
| `v1-single.txt` | first paired A/B (pinned MTU 1280 — **superseded**, see §7) |
| `v2-profile.txt` | flow ladder, latency, UDP loss (pinned MTU — partially superseded) |
| `v3-lat.txt` | latency idle/loaded, both directions |
| `v4-sndbuf.txt` | send-buffer ladder |
| `v5-ab-realmtu.txt` | paired A/B with bare control, auto-tuned MTU |
| `udpbuf-ladder.txt`, `dgrambuf-ladder.txt`, `both-buffers.txt` | buffer ladders (§11c, rejected hypotheses) |
| `direct-deficit.txt` | V-6 direct-deficit teardown |
| `ab-after-txqueue.txt` | paired A/B re-run after the txqueuelen fix (§11c) |
| `txqueue-ladder.txt` | V-10 TUN txqueuelen ladder (§11c) |
| `cc-matrix.txt`, `cc-matrix-4rep.txt` | V-12 congestion-controller matrix (§11e) |
| `ab-degraded-link.txt` | A/B taken while the access link was degraded (§11b) |
| `netns-after-hub.txt` | netns regression gate after the hub traversal alignment |
| `wire-ceiling.txt` | V-13 offered-rate ladder, 200→540 Mbit/s (§11f) |
| `wire-ceiling-attrib.txt` | V-13 ceiling attribution: CPU columns × two controllers (§11f) |
| `wire-ceiling-arms.txt` | V-13 attribution arms: buffer, TUN queue depth, flows, TUN queues (§11f) |
| `wire-ceiling-deep.txt` | V-13 deep-buffer + carriers arms, with the RTT column (§11f) |
| `sndbuf-deep-tcp.txt` | V-13 deep rungs re-measured under INNER TCP — the workload the default is chosen for (§11f) |

Plus, under `out/` (the link is a property of the site, not of one stage):

| file | stage |
|---|---|
| `link-baseline-*.txt` | V-9 access-link qualification (§11b) |

The superseded files are kept deliberately: §7 is only legible next to the
numbers that produced the mistake.
