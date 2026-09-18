# Web transfer — performance method and baseline

`bore` is chosen for how fast it moves bytes, so the browser path is measured
the same way the rest of the project measures itself: with a harness, against a
control, in ratios, printing every sample.

This document is the method plus the first baseline. It is not a claim about
anyone else's machine.

## The harness

```bash
cargo build --all-features && npm run build --prefix web/transfer
SIZES=8,32 REPS=3 scripts/perf/web_transfer_bench.sh
```

Three arms, each one variable apart from the next, run **interleaved** (one
repetition of every arm, then the next repetition — never all of one arm and
then all of another, because the machine changes under a long run):

| Arm | What it runs | What it bounds |
|---|---|---|
| `pipe` | `t_web_perf_relay`: the real opaque relay, two WebSocket legs, no application crypto, room throttle disabled | the transport ceiling the server can offer |
| `crypto-seal` / `crypto-open` / `crypto-digest` / `crypto-receiver` | `web/transfer/tests/perf/crypto-bench.mjs`: the shipped `crypto.js`/`framing.js` — AES-GCM over 24 KiB fragments and SHA-256 over 1 MiB chunks — with no network and no storage | the CPU ceiling; `crypto-receiver` is `open + digest`, the work the *recipient* must do per byte |
| `browser-<engine>` / `browser-save-<engine>` | `web/transfer/tests/perf/throughput.perf.mjs`: a real page pair — read the file, encrypt, relay, decrypt, verify every chunk, stage into OPFS — and then the explicit save to disk | what a user actually waits for |

The end-to-end rate cannot exceed the smaller of the first two ceilings.
Measuring them separately is the point: when a run gets slower, the three
numbers say **which** of the three moved.

### Rules the harness enforces

- **Every raw sample is printed beside the median.** A summary that shows only
  medians hides the bug that corrupts them — this repository lost a whole
  campaign to a locale-dependent sort (`V-11`), and the harness therefore also
  pins `LC_ALL=C` and sorts numerically.
- **An arm that produced no bytes prints `FAILED` and the driver exits
  nonzero.** A failed arm that prints `0` enters a median as if it had been
  measured; that is how a broken measurement becomes a published number.
- **Ratios travel, absolute numbers do not.** Every figure below belongs to one
  workstation on loopback. Compare arms measured in the SAME run.
- **The room throttle is off in every arm** (`--web-transfer-relay-rate 0`).
  With it on, a fast machine measures the token bucket (100 MiB/s by default)
  and calls it the transport.
- **The control plane is paced outside the measured window.** A repetition
  spends two mutations (request + cancel) against a 4/s bucket with a burst of
  8, so an unpaced loop is refused with `RATE_LIMITED` — which then looks like a
  transport failure and is really a client pacing bug.

## Baseline, 2026-09-15 (loopback, relay path, sub-phase 3.9)

Workstation, Linux 7.0.0, 16 threads, `--all-features` debug build of the
server (a release build is faster; the arms are compared against each other,
not against a release number). Medians of three interleaved repetitions.

| Arm | 8 MiB | 32 MiB | Samples (32 MiB) |
|---|---|---|---|
| `pipe` (relay ceiling) | 472.20 | 469.95 | 484.17 460.41 469.95 |
| `crypto-seal` (sender CPU) | 604.76 | 776.84 | 776.84 787.28 753.95 |
| `crypto-open` | 641.85 | 938.51 | 938.51 983.27 912.78 |
| `crypto-digest` | 1364.96 | 1703.32 | 1703.32 1809.39 1516.34 |
| `crypto-receiver` (`open`+`digest`) | 434.83 | 605.10 | 605.10 637.07 569.79 |
| `browser-chromium` (end to end) | 39.60 | 39.90 | 39.75 40.00 39.90 |
| `browser-save-chromium` (staged → disk) | 156.86 | 188.24 | 188.24 202.53 183.91 |

All rates in MiB/s.

### What the baseline says

1. **Nothing is quadratic any more.** Every arm is flat between 8 and 32 MiB.
   That is the V002-C2 fix holding: before it, the staged download fell from
   22.81 MiB/s at 32 MiB to 8.43 at 128 — throughput falling with size is what
   identifies a quadratic term, and it is gone.
2. **The transport is not the constraint on this machine.** The relay carries
   ~470 MiB/s with the throttle off; the default throttle (100 MiB/s per room)
   is the first thing a fast LAN would meet, and it is a deliberate product
   limit, not an accident.
3. **The cipher is not the constraint either.** The recipient's own AES-GCM
   plus SHA-256 runs at 435–605 MiB/s.
4. **So the browser's number is spent elsewhere.** Sub-phase 3.10 took that
   question, profiled the page per stage, and found that part of the answer was
   the measurement itself — see the next section, which supersedes the browser
   rows above.
5. **Saving is not the problem.** Composing the staged parts into a `Blob` and
   writing it out runs at 157–188 MiB/s, four times the transfer itself.

## 3.10 — profiling the browser path, and the two numbers that changed

### The browser rows above were quantized by the harness, not by the product

The 3.9 browser arm timed the transfer from the click to the moment Playwright
observed `#save-file`. That observation is a POLL, on Playwright's own grid
(100 ms, then +250, then +500), and at 32 MiB the pipeline finished at ~370 ms
— between the 350 ms and the 850 ms check. So the arm reported ~840 ms, i.e.
~39 MiB/s, at BOTH sizes, and the flatness that looked like "a per-byte cost"
was the grid landing on the same rung twice.

The arm now reports the page's own clock (`dst.e2e`: the recipient's
`startDownload` to the staged file, taken with `performance.now()` inside the
page) and prints the polled wall separately as `browser-wall-<engine>`, so the
two are always visible together. **Nothing about the product changed with this
line; what changed is that the harness stopped measuring its own poll.**

Rule this earns, beside V-9 and V-11: *a harness that times a browser must take
its timestamps inside the page, or publish the poll interval beside the number.*

### Where the time goes (stage decomposition)

Every stage is timed inside the page with `performance.now()` and printed as
`PERF-STAGE … stage=<name> median_ms=… share=… calls=… samples=[…]`. The
accounting is inert unless the harness installs `window.__borePerf` before the
app loads (`web/transfer/src/perf.js`), so the shipped path pays one property
read per mark and nothing else.

32 MiB, chromium, five repetitions, medians (ms):

| Stage | Before 3.10 | After 3.10 | What it is |
|---|---|---|---|
| `dst.e2e` | 630.2 | 372.7 | click → staged file (the number the arm reports) |
| `dst.bytes` | 366.2 | 0.4 | turning each arrived message into bytes |
| `dst.stage` | 251.5 | 247.3 | OPFS part file: create, write, close |
| `dst.open` | 35.1 | 157.8¹ | AES-GCM open per 24 KiB fragment |
| `dst.digest` | 26.9 | 26.7 | SHA-256 per 1 MiB chunk |
| `dst.commit` | 9.5 | 9.7 | the resume record in IndexedDB |
| `src.wall` | 222.5 | 222.5 | the sender's whole read → seal → send loop |
| `src.flush` | 1.6 | 1.6 | bytes still in the socket when the loop ends |
| `dst.setup` | 8.6 | 8.6 | quota + resume record + request round trip + key |

¹ `dst.open` after 3.10 is the SUM of overlapping waits (eight opens are in
flight at once), so it is no longer comparable to a wall clock — `dst.e2e` is.

Two facts fell out of this table immediately, and both were invisible before:

- **The sender is not the bottleneck.** It pushes 32 MiB in 222 ms and its
  socket is empty 1.6 ms later, against a recipient that needs 370. Every
  sender-side item in the 3.10 catalogue is therefore declined below.
- **The recipient is busy essentially all of its own window** (`dst.busy` 341
  of `dst.wall` 348 before the pipeline landed), so it is not waiting for the
  wire either — the cost is its own work.

### Adopted, with the measurement that justified it

**1. Relay frames arrive as `ArrayBuffer`, not `Blob`** (`receiver.js`,
`socket.binaryType`). A WebSocket delivers binary as a `Blob` by default, and
every frame then paid an `await blob.arrayBuffer()`: 366 ms of a 630 ms
transfer, 1377 times, the largest single stage. One line, no wire change.

| Arm (32 MiB, chromium, page clock) | Median MiB/s | Samples |
|---|---|---|
| `blob` (before) | 50.78 | 52.25 50.78 47.89 |
| `arraybuffer` (after) | 84.23 | 80.81 84.23 86.07 |

**1.66×**, one variable, same run conditions.

**2. Frames are opened several at a time, consumed strictly in order**
(`receiver.js`, `FRAME_PIPELINE_DEPTH = 8`). The old receive path awaited one
`decrypt` per fragment inside a promise chain, so an engine that can overlap
AEAD calls had no chance to. Whether an engine rewards that is a question with
an answer, and the in-page probe (`tests/perf/webcrypto.perf.mjs`) asks it
directly — 1376 opens of 24 KiB, one at a time against four and eight:

| Engine | depth 1 | depth 4 | depth 8 |
|---|---|---|---|
| chromium | 1978.53 | 2121.71 | 1990.74 |
| firefox | 921.43 | 1343.75 | 1897.06 |
| webkit | 1007.81 | 2150.00 | 2480.77 |

End to end, 32 MiB, five repetitions, page clock:

| Engine | Before (sequential) | After (depth 8) | Ratio |
|---|---|---|---|
| chromium | 82.77 | 85.86 | 1.04 |
| firefox | 41.13 | 44.63 | 1.09 |
| webkit | 39.60 | 81.84 | **2.07** |

Samples, after: chromium `79.70 92.33 88.54 85.61 85.86`, firefox
`46.58 42.22 44.63 42.78 46.78`, webkit `77.29 92.22 88.89 81.84 77.11`.

The verification does not move: each frame is still opened against its OWN
expected sequence, assigned in arrival order, so a gap, a replay or a reorder
fails exactly as before, and the plaintext is still consumed one frame at a
time in order (`pipelined_open_preserves_frame_order_and_bytes`).

### Measured and DECLINED (each with the number that declined it)

- **One-chunk read-ahead on the sender** (catalogue item 2). `src.read` is
  81 ms of a 222 ms sender loop, and the sender already finishes 150 ms before
  the recipient. Hiding sender I/O behind sender CPU speeds up the side that is
  not the limit.
- **Pipelined seal on the sender** (item 1, send side). `src.seal` is 40 ms of
  the same 222. Same reason; revisit if the recipient ever drops below it.
- **Single-allocation frames** (item 4). `src.seal` + `dst.open` together are
  under 12% of the transfer, and the V-14b win they imitate was on a path
  moving 53 000 packets per second; here it is 1377 allocations per transfer.
  Declined until a profile says otherwise — the frame format must not move for
  a gain this size.
- **Opening the next part file while the current chunk arrives** (item 3,
  pipelined open). Implemented and measured at five repetitions per arm:
  87.03 MiB/s with, 86.32 without (`dst.stage` 229.4 against 240.5) — inside
  the noise, so the code was REVERTED rather than kept for a number it did not
  earn. The `createWritable` cost does not hide, because the work that would
  hide it runs on the same thread.

### What is left, and it is sized

The recipient's remaining cost is the OPFS write protocol itself:
`dst.stage` is 247 ms of a 373 ms transfer on chromium (`createWritable` 166,
`write` 66, `close` 23 — per 1 MiB chunk: 5.2 / 2.1 / 0.7 ms) and 449 of 717 on
firefox. `createWritable` writes through a swap file and commits on close; the
API that does not is `createSyncAccessHandle`, which exists ONLY in a worker.
Moving staging into a worker is therefore the next measurable step, worth
roughly 150 ms per 32 MiB on chromium and more on firefox, and it ships a new
asset — so it is its own sub-phase (**3.11**), not a line smuggled into this
one.

### Where the three engines stand (32 MiB, page clock, five repetitions)

| Engine | MiB/s | Dominant stage |
|---|---|---|
| chromium | 85.86 | OPFS staging (247 of 373 ms) |
| webkit | 81.84 | OPFS staging is cheap here (104 ms); the AEAD opens dominate and now overlap |
| firefox | 44.63 | OPFS staging (449 of 717 ms) |

All three engines run the same code and pass the same 60-test browser suite;
the differences are the engines' own OPFS and WebCrypto implementations, which
is why the harness reports per engine and never averages them.

### Baseline after 3.10 (same machine, same driver, 2026-09-16)

`SIZES=8,32 REPS=3 scripts/perf/web_transfer_bench.sh`, medians of three
interleaved repetitions, every sample printed by the driver:

| Arm | 8 MiB | 32 MiB | Samples (32 MiB) |
|---|---|---|---|
| `pipe` (relay ceiling) | 452.62 | 486.29 | 486.29 447.19 486.46 |
| `crypto-receiver` (`open`+`digest`) | 433.18 | 634.34 | 636.89 607.07 634.34 |
| `browser-chromium` (page clock, end to end) | 78.90 | 86.25 | 86.25 90.17 82.71 |
| `browser-wall-chromium` (Playwright poll, quantized) | 70.80 | 38.51 | 38.46 38.51 39.85 |
| `browser-save-chromium` (staged → disk) | 53.69 | 191.62 | 191.62 176.80 237.04 |
| `webcrypto-open-1-chromium` (probe, depth 1) | — | 1864.16 | see driver output |
| `webcrypto-open-8-chromium` (probe, depth 8) | — | 2041.14 | see driver output |

The `browser-wall-*` row is kept deliberately: it is the same transfer read
through the poll grid, and the distance between the two rows is the size of the
measurement error 3.9 published as a product number.

Firefox and webkit are measured the same way
(`ARMS=browser ENGINE=firefox scripts/perf/web_transfer_bench.sh`); their
32 MiB medians after 3.10 are 44.63 and 81.84 MiB/s.

## 3.11 — OPFS staging in a worker (`createSyncAccessHandle`)

3.10 sized the recipient's remaining cost as the OPFS write protocol and
predicted "roughly 150 ms per 32 MiB on chromium and more on firefox" from
moving it into a worker. The prediction was measured, and it is half right.

### Method

Both paths run **in the same page build, in the same run, on the same
machine**, one variable apart: two recipient tabs join the same room, one
shipped (staging worker) and one pinned to the main-thread `createWritable`
path through the harness-only flag `window.__borePerf.noStageWorker`. Each
repetition runs both arms, and **the order alternates** — with a fixed order
the first arm of every repetition wins a systematic advantage, which is
exactly what the first pass of this measurement reported before the order was
alternated (it read +6 / +10 / +14 % and none of it survived).

A second row was added because throughput cannot see it: `mainthread-stall-ms`
is the worst delay observed by a 16 ms timer inside the recipient page — the
stall a progress bar, a cancel click or a scroll would have waited for.

### Result (32 MiB, page clock, eight alternated repetitions, 2026-09-16)

| Engine | staging worker | main thread | worker / main | `dst.stage` worker | `dst.stage` main |
|---|---|---|---|---|---|
| chromium | **78.40** | 74.69 | **1.050** | 250.6 ms | 277.6 ms |
| firefox | **42.36** | 40.56 | **1.044** | 383.5 ms | 485.0 ms |
| webkit | 67.80 | 67.38 | 1.006 | 151.5 ms | 130.5 ms |

Raw samples (MiB/s, in run order):

```text
chromium   worker  78.24 80.81 83.05 84.99 77.93 78.57 76.03 73.83
chromium   main    75.03 79.74 78.82 78.70 71.93 74.35 68.55 73.14
firefox    worker  47.55 42.61 46.78 42.11 40.66 41.61 43.78 39.95
firefox    main    41.34 43.36 41.56 40.10 39.90 39.60 41.03 39.41
webkit     worker  56.94 80.20 70.18 64.65 67.94 71.91 67.65 61.54
webkit     main    70.48 69.72 63.12 70.33 63.12 66.53 61.54 68.23
```

Main-thread stall, median over the same repetitions: chromium 3.9 vs 3.6 ms,
firefox 11.5 vs 9.0, webkit 7.5 vs 7.0 — **no difference**. Staging never
blocked the main thread in the first place; `createWritable` is asynchronous
and the bytes were already crossing to the storage backend off-thread.

### What the measurement changed

1. **The prediction held on two engines and inverted on the third.** Staging
   falls 9.7 % on chromium (277.6 → 250.6 ms) and **20.9 % on firefox**
   (485.0 → 383.5 ms), close to what 3.10 estimated. On webkit
   `createSyncAccessHandle` is *slower* than `createWritable`
   (130.5 → 151.5 ms, +16 %): that engine's async path is already cheap
   (`close` is 0–2 ms there, against 183 ms on firefox), so the worker hop is
   pure overhead. The worker stays on all three anyway — end to end webkit
   reads 1.006, inside the noise, and engine-sniffing a storage API is how a
   codebase acquires a branch nobody re-measures.
2. **Transferring the buffer is not an optimisation, it is the difference
   between a win and a loss.** The first implementation *copied* the chunk
   into the worker (structured clone) for retry-safety; measured, that cost
   webkit 4.6 % end to end and erased chromium's gain. The shipped version
   transfers the `ArrayBuffer` when the view owns it, and the worker hands the
   buffer **back** with a handled failure so the main-thread fallback still
   has bytes to write.
3. **A 10–21 % cut in staging buys 4–5 % end to end**, because staging
   overlaps with reception: `dst.stage` is 250 ms of a 410 ms transfer on
   chromium but it is not 250 ms of *serialised* time. The remaining cost is
   spread across the frame path (`dst.open`, `dst.busy`, `dst.join`), which is
   where a further attempt has to look — not at storage.

### What was declined here

- **Engine-specific staging** (main thread on webkit, worker elsewhere): the
  end-to-end difference is 0.6 %, inside the noise, and the branch would be a
  permanent claim about an engine that is free to change.
- **A deeper worker queue.** One chunk is in flight at a time by construction
  (the recipient awaits each `writeChunk`), so a queue would add memory and no
  pace — the same reason 3.10 declined a read-ahead deeper than one chunk.

## 4.6 — the direct path, and the catalogue of what carries over from the rest of bore

Sub-phase 4.6 does two things: it gives the harness a **direct** arm so
`direct / relay` is a ratio measured in the SAME repetition, and it walks the
optimizations the rest of this repository MEASURED on its own data path,
adopting each with a number or refusing it with a reason. Nothing here is
adopted because it sounds right.

### A label that had stopped being true

Since 4.2 the direct path is the DEFAULT, so a recipient that asks for nothing
gets WebRTC. The 3.11 rows above were published as `relay` and were, by then,
measuring the DataChannel. The staging comparison itself stays valid — its two
arms differed by one variable and both were on the same transport — but the
transport in its header was wrong. The harness now makes every arm declare its
transport (`noWebRtc`), and the staging comparison is pinned to the relay so it
continues the lineage of 3.9/3.10.

### `direct / relay`, chromium, loopback, 8 MiB, six alternated repetitions

| arm | median | samples (MiB/s) |
|---|---|---|
| `direct` | 54.15 MiB/s | 46.95 **3.11** 68.73 61.35 **3.71** 81.63 |
| `relay` | 82.96 MiB/s | 70.55 85.29 87.82 84.12 81.80 81.55 |
| `direct / relay` | **0.697x** | 0.665 **0.036** 0.783 0.729 **0.045** 1.001 |

Read the samples, not the median. The relay arm is flat — six samples inside
±10 %, `src.drain` 0.0 ms, the source never once waits for the socket. The
direct arm is **BIMODAL**: a healthy repetition reads 42–82 MiB/s, and roughly
one repetition in two or three collapses to 2–14 MiB/s with `src.drain` at
1.4–3.3 s out of a ~2 s transfer. The source is not slow; it is *parked*,
waiting for a DataChannel that has stopped draining, and 87–98 % of its wall
time in a collapsed repetition is that one wait.

**Two hypotheses were tested and both are falsified.**

1. *The send queue is too deep.* Swept 4 MiB/1 MiB (shipped), 1 MiB/256 KiB and
   256 KiB/64 KiB, six repetitions each: the collapse appears at **every**
   depth (3/6, 2/6 and 4/6 repetitions respectively). Queue depth changes how
   often it happens by nothing that survives six samples.
2. *The public STUN chain is a live network dependency inside the
   measurement.* Re-run with `--web-transfer-no-stun`, so the page has host
   candidates only: still 3/6 collapsed, `src.drain` 2240 ms and 1670 ms.

**What it IS, measured this time (V003-C1, 2026-09-17).** The paragraph that
used to stand here named "an ordered, reliable SCTP association losing a
packet and paying a retransmission timeout" as the cause. Nothing had measured
that, and instrumenting the attempt (V003-C3's trace, printed beside every
rate by the harness) says it is wrong. Six alternated repetitions, chromium,
8 MiB, with the per-attempt trace read AFTER EACH REPETITION — the page's
store keeps eight attempts and a run that reads it at the end silently drops
the oldest, which is how the first instrumented run lost a 3.3 s stall and
published five fast repetitions under six rep numbers that were store indexes:

| rep | direct | `drain_longest` | `discardedOnSend` | selected pair |
|---|---|---|---|---|
| 0 | 55.21 MiB/s | 54 ms | 715 | `host/host` |
| 1 | 62.94 MiB/s | 25 ms | 0 | `host/host` |
| 2 | 40.20 MiB/s | 75 ms | 1209 | `host/host` |
| 3 | 68.09 MiB/s | 34 ms | 0 | `host/host` |
| 4 | **5.87 MiB/s** | **1273 ms** | 965 | `host/host` |
| 5 | **5.18 MiB/s** | **1434 ms** | 38 | `host/host` |

Four facts, each of which removes a candidate explanation:

1. **It is ONE wait, not a slow path.** In a repetition whose marks were
   lowered to 512 KiB/128 KiB so the source waits seven times, ONE wait took
   1654 ms and the other six totalled 47 ms. A transport running slowly would
   spread the cost over every wait.
2. **It is not the queue depth.** At 4 MiB/1 MiB the peak queue is ~4.5 MB and
   at 512 KiB/128 KiB it is ~1.17 MB; the stall is 1.3–1.7 s in both. (The
   first low-mark run came back clean and looked like a fix — the second
   reproduced the collapse. One run is not a measurement.)
3. **It is not the ICE path.** Every repetition, fast and slow, selects
   `host/host`. The slow ones are not a different KIND of path.
4. **It is not packet loss on the path, and this is what falsifies the old
   sentence.** `packetsDiscardedOnSend` does not correlate at all — a fast
   repetition discarded 1209 and the slowest discarded 38 — and, decisively,
   **firefox on the same loopback pair never shows the collapse**: six
   repetitions of 26.40–35.56 MiB/s, longest wait 155 ms, `direct / relay`
   **0.882x** (samples 0.918 0.846 0.938 0.785 1.000 0.713). Two engines share
   the loopback path; only one of them stalls. A path that lost packets would
   lose them for Gecko too.

So the measured statement is: **a ~1.3–1.7 s freeze of chromium's DataChannel
send path, at most once per transfer, independent of the queue depth, of the
selected candidate pair and of the send discards, and absent on Gecko.** Its
duration is the length of a one-second-class timer, which is suggestive of an
SCTP RTO or zero-window probe inside the engine — and that stays a HYPOTHESIS,
because chromium exposes no `sctp-transport` statistics (`cwnd`, `rwnd` and
`unackData` read `?` in every trace above) and a page has no other way to see
inside the association. It is a browser limit, recorded as one, and entry 6
below refuses socket tuning for the same reason: the knob is not reachable
from here.

**And the ratio itself must not be read as a statement about the product.**
Both "paths" here are loopback: the relay arm is a localhost TCP hop through a
Rust server on the same machine, which is about as flattering as a relay can
possibly be, and the direct arm still pays DTLS and SCTP for a peer that is
one process away. V-9's rule applies unchanged — **qualify the link before
quoting an absolute figure**, and this link is not the one the direct path
exists for. A two-host measurement is the only one that can say whether direct
wins in deployment, and it is not in this file yet.

### The catalogue

| # | entry | verdict |
|---|---|---|
| 1 | Backpressure, never a deep queue (BW-F3 + V-13) | **adopted unchanged**, with the sweep that declined moving it |
| 2 | Fragment sized on the peer, not on a constant (RFC 8841) | **adopted**, and 24 KiB measured as the winner on chromium |
| 3 | One connection and one channel per transfer (BW-F2) | **adopted**, now gated both ways |
| 4 | One allocation and one copy per frame (V-14b) | **adopted**, −34 % on the frame-open stage |
| 5 | Coalescing only where it does not become waiting (V-14a) | **adopted as a rule**, gated |
| 6 | Socket buffers, native QUIC reuse, application retransmission, browser carriers | **refused**, with the reason |

#### 1. Backpressure, measured rather than argued

The shipped marks are 4 MiB high / 1 MiB low: above the high mark the source
stops reading the file and awaits `bufferedamountlow`. The sweep (8 MiB, six
repetitions, chromium, arms alternated inside each repetition):

| marks | median | median of the repetitions that did not collapse |
|---|---|---|
| 4 MiB / 1 MiB (shipped) | 35.84 MiB/s | ≈ 59 MiB/s |
| 1 MiB / 256 KiB | 52.16 MiB/s | ≈ 60 MiB/s |
| 256 KiB / 64 KiB | 7.84 MiB/s | ≈ 44.5 MiB/s |

The overall medians are governed by how many repetitions collapsed, not by the
marks. Comparing only the healthy repetitions, 4 MiB and 1 MiB are
indistinguishable and 256 KiB is **25 % worse** — a queue that shallow starves
the association between refills. The marks stay at 4 MiB / 1 MiB: nothing
measured justifies moving them, and "deeper is better" was already falsified
on the native side (V-13's curve turns over).

#### 2. The fragment is the peer's number, and 24 KiB is the winner here

`min(24576, pc.sctp.maxMessageSize - 64)`, floor 1024, unchanged since 4.2 —
24 KiB is the protocol ceiling (`FRAME_MAX_PLAINTEXT`) and the peer's
`maxMessageSize` is the only other input. What 4.6 adds is the measurement,
because the per-message cost of SCTP is not the same everywhere (8 MiB,
chromium, three repetitions):

| fragment | median | samples |
|---|---|---|
| 8 KiB | 3.99 MiB/s | 3.99 50.79 2.38 |
| 16 KiB | 2.39 MiB/s | 64.05 2.39 2.37 |
| 24 KiB (shipped) | **53.58 MiB/s** | 41.49 53.58 54.09 |

Only the 24 KiB arm produced three repetitions without a collapse. Three
repetitions is weak evidence for a rate that varies this much, but it points
the same way as the hypothesis above: fewer, larger messages mean fewer
chances to lose one. The shipped size is confirmed and nothing changes.

#### 3. One connection, one channel — and no striping either

The plan already forbade sharing an `RTCPeerConnection` across transfers. 4.6
adds the converse, explicitly: **N channels carrying the fragments of ONE
file is forbidden**. SCTP orders per stream, so spreading one file over
several channels reorders it — the same trap that made the native direct path
flow-pinned instead of round-robin (BW-F2) — and on a single ordered channel
it buys nothing anyway. The actor already refuses a second channel; the rule
is now gated from both sides
(`one_channel_per_transfer_and_extra_channels_are_refused`).

#### 4. One allocation and one copy per frame (the only speed-up adopted here)

`openWithKey` used to `slice` the incoming message twice — once for the
16-byte header, once for the body — allocating and copying the WHOLE frame two
extra times, about 1400 times per 32 MiB. WebCrypto takes a `BufferSource`, so
the header can be the AAD and the body the ciphertext exactly where they
already are: both are now `subarray` views.

| stage | before | after | change |
|---|---|---|---|
| `dst.open`, 8 MiB (345 frames) | 42.4 ms | 28.9 ms | **−31.8 %** |
| `dst.open`, 32 MiB (1377 frames) | 141.5 ms | 92.8 ms | **−34.4 %** |
| `dst.busy` (control), 32 MiB | 351.9 ms | 352.5 ms | +0.2 % |
| `dst.e2e`, 8 MiB | 107.2 ms | 101.0 ms | −5.8 % |
| `dst.e2e`, 32 MiB | 368.8 ms | 370.6 ms | +0.5 % |

`dst.busy` is the control: the rest of the receive path did not move, so the
third of `dst.open` that disappeared is the two copies. End to end it is worth
~6 % at 8 MiB and nothing measurable at 32 MiB, for the same reason 3.11 found:
opening overlaps with reception. It is adopted because it is free — the format
does not move (`frame_encode_allocates_once_and_matches_the_fixture_bytes`
compares against the cross-language fixture byte for byte) and the allocation
is simply not made.

The SEAL side is left alone and this is a decision: `subtle.encrypt` always
returns a fresh `ArrayBuffer` and there is no in-place variant, so the framed
output costs one allocation and one copy of the ciphertext no matter how it is
written. `new Uint8Array(arrayBuffer)` wraps rather than copies, so that is
already the floor.

#### 5. Coalescing must never become waiting

Unchanged from V-14a and stated so it cannot drift: a fragment that is ready is
written in the same turn of the event loop, and **no timer is ever armed to see
whether company arrives**. `coalescing_never_waits_on_a_timer` runs a whole
transfer with `setTimeout` replaced by a counter and asserts it stays at zero;
red-checked by adding a single `setTimeout(…, 0)` to the send loop.

#### 6. Refused, each with its reason

- **Socket buffer / window tuning (P-13, F-13).** A page cannot reach the UDP
  socket under an `RTCPeerConnection`, and there is no API for the SCTP send
  buffer. The equivalent knob in a browser is the queue of entry 1, which was
  swept. This is also why the collapse above cannot be fixed from here.
- **Reusing bore's native QUIC / hole punching, or opening a second UDP
  socket.** D5 and a plan invariant: browser data uses the DataChannel or the
  relay, and the page opens no socket of its own.
- **Application-level retransmission.** SCTP is ordered and reliable already;
  a second retransmission layer on top of a reliable one is precisely the
  TCP-over-TCP shape V-15 describes.
- **A browser `--carriers`.** Entry 3: more channels for one file reorder it.
- **Engine-specific branches.** Same reason 3.11 declined one for storage: a
  permanent claim about an engine nobody re-measures.

### What 4.6 leaves open

The bimodal collapse on chromium's DataChannel is **identified, characterised
and not fixed**, because every mechanism reachable from a page was tested and
none of them is it (above). What remains is a two-host run: on loopback the
relay is a localhost hop and the comparison flatters it, and a stall inside an
engine's own send path may behave differently when the association is carrying
a real RTT.

`T-WEB-PERF-LAN` is that run, and it is a harness rather than a paragraph of
advice: `scripts/perf/web_transfer_lan.sh` serves the room over HTTPS on this
host's LAN address, creates it with the SHIPPED `bore transfer web`, drives
the SOURCE with `web/transfer/tests/perf/lan.perf.mjs`, and leaves the
recipient to a browser somebody opens on the other device.

Three decisions inside it are worth stating, because each one was forced:

- **It drives only the source.** An Android phone cannot be driven by
  Playwright, and F01 is a phone report. The source's own view is sufficient
  because the recipient acknowledges only VERIFIED ranges, so the instant the
  transfer leaves `senderState()` is the instant the other side finished
  hashing it — not the instant this side emptied a buffer.
- **HTTPS is not optional.** The server refuses a plaintext base URL off
  loopback and is right to: `crypto.subtle`, OPFS and `RTCPeerConnection` all
  need a secure context, so a phone pointed at `http://192.168.x.y` would not
  have the APIs this product is made of. With no `CERT`/`KEY` the script
  generates a self-signed certificate for the LAN address and says so; that
  shape measures throughput, never TLS overhead.
- **The arm is selected on the SOURCE**, by removing `RTCPeerConnection` from
  that context so the page answers `unsupported` immediately. The recipient
  therefore needs no flag, which is exactly what lets the other host be a
  phone, and `pathCommits` is asserted so an arm whose label and transport
  disagree fails instead of publishing the relay's number under `direct`.

Run:

```bash
npm --prefix web/transfer run build && cargo build --all-features
SIZE_MB=400 REPS=3 scripts/perf/web_transfer_lan.sh
# open the printed URL on the other device, tap Scarica when it says so
```

It prints every raw sample, the per-attempt trace beside it, `direct / relay`
and an explicit `goal=50.00MiB/s … verdict=MET|NOT-MET`. Qualify the link
first (V-9): `iperf3 -c <other host> -t 20`, then `-P 8` — a per-flow limit
opens with parallelism and a policer does not — and take the native baseline
with `bore transfer` over the same route in the same session.

**Not yet run on the reporter's network**, which is why V003-F01 stays open:
that measurement needs the two hosts and the 400 MB file it was reported
from, and it cannot be produced from the development machine. The smoke run
that proves the harness itself works is loopback and is not a result: 8 MiB,
`direct` 3.71 MiB/s with one 2087 ms drain wait, `relay` 69.57 MiB/s — the
same defect this section characterises, found by the new harness on its first
execution.

## Re-running and comparing

```bash
# a quick check of one arm
ARMS=pipe SIZES=32 REPS=3 scripts/perf/web_transfer_bench.sh
# another engine end to end
ARMS=browser ENGINE=firefox SIZES=32 REPS=3 scripts/perf/web_transfer_bench.sh
```

The 4.6 arms are Playwright tests and take their knobs from the environment:

```bash
cd web/transfer
# direct vs relay in the same repetition, plus the ratio line
BORE_PERF_SIZES_MIB=8 BORE_PERF_REPS=6 \
  npx playwright test --config=playwright.perf.config.mjs --project=chromium \
  --grep "direct versus relay"
# host candidates only (one variable: no public STUN chain)
BORE_PERF_NO_STUN=1 ... --grep "direct versus relay"
# the two sweeps
BORE_PERF_FRAGMENTS=8192,16384,24576 ... --grep "direct fragment size sweep"
BORE_PERF_MARKS=4194304:1048576,1048576:262144 ... --grep "direct backpressure sweep"
```

The two-host run is a driver, not a test — it needs a person at the other
device — and it owns its own server, room and summary:

```bash
# both arms, 400 MiB, three repetitions; prints the goal verdict
SIZE_MB=400 REPS=3 scripts/perf/web_transfer_lan.sh
# a real name and a real certificate (the shape an acceptance claim uses)
CERT=/etc/ssl/full.pem KEY=/etc/ssl/key.pem BASE_URL=https://files.example/ \
  scripts/perf/web_transfer_lan.sh
```

A change to the data path re-runs this harness and records the ratio it moved,
in this file, with its samples. "No regression observed" is not a measurement.

## 7.6 — two real hosts over a WAN, and the queue depth that was killing the direct path

`T-WEB-PERF-WAN` — `scripts/perf/web_transfer_wan.sh`, 2026-09-18. Server and
recipient on an AWS host in `eu-central-1`, source on the workstation; 21 ms
RTT, 222 Mbit/s measured on the uplink with `scp` before the first arm (V-9:
qualify the link before quoting any absolute figure). Payload 128 MiB per arm,
arms alternated INSIDE each repetition (V-13), raw samples always printed
(V-11). Bytes travel workstation -> cloud, which is ingress and is not billed;
that direction is a property of the topology, not a convenience.

### The defect the first run found

The direct arm did not merely run slower — it **died**, every time, and the
transfer finished on the relay:

```
direct/c4  rep=0  peak=4462400  reason=channel-closed  commits=['direct','relay']
```

The trace now carries the engine's own verdict (`RTCErrorEvent.error`):
`errorDetail: "sctp-failure"`, `sctpCauseCode: 12`. Before this campaign the
mark read `channel-error` and nothing else, which is the difference between
"the peer went away" and "we overran the SCTP send queue" — opposite remedies.

Two things were wrong, and the second hid the first:

1. **The group's backpressure ignored the per-carrier mark.** `waitLow`
   compared each carrier's `bufferedAmount` against the module constant
   `RTC_HIGH_WATER` instead of against that carrier's own effective mark, so
   a harness that lowered the mark changed nothing. MEASURED: with the knob at
   4 MiB, 1 MiB and 256 KiB the peak queued depth read 4.46 MB, 4.46 MB and
   4.46 MB — three settings, one number, which is what a knob nobody reads
   looks like from the outside.
2. **4 MiB per carrier is past the optimum.** With the mark honoured, the
   depth ladder (five repetitions each, carriers=4):

   | per carrier | median MiB/s | failures in 5 | peak queued | drain waits |
   |-------------|--------------|---------------|-------------|-------------|
   | 2 MiB       | 18.76        | 1 fallback    | 2.36 MB     | 23–25 |
   | 1 MiB       | 20.19        | 1 sctp-failure| 1.32 MB     | 46–51 |
   | **512 KiB** | **21.45**    | **0**         | 0.80 MB     | 78–86 |

   The deepest queue is the SLOWEST **and** the least stable — the same shape
   the native uplink's own depth ladder reached (V-13: "deeper is better" is
   false and there is an optimum). Shipped: `RTC_HIGH_WATER` 512 KiB,
   `RTC_LOW_WATER` 128 KiB.

### And it is not a WAN-only fix: loopback said the same thing, quietly

Phase 5's own ladder had "confirmed" 4 MiB/1 MiB on loopback — but it compared
"only the healthy repetitions", and the repetitions it set aside were bimodal
in exactly the way a queue that aborts is. Re-measured on loopback with the
same six repetitions at 8 MiB, the shipped 512 KiB reads **46.27 MiB/s**
median against 45.82 for 4 MiB, and the catastrophic samples are gone: the
worst repetition moves from **2.36 MiB/s to 14.02**. The deep queue was never
buying anything; it was producing the outliers the earlier ladder excluded.

### Carriers, measured rather than chosen

Three repetitions per value, at the shipped 512 KiB mark:

| carriers | direct median MiB/s | ratio to relay | direct attempts that died (of 3) |
|----------|---------------------|----------------|----------------------------------|
| 1        | 10.84 †             | 0.248x †       | 1 † |
| 2        | 23.05               | 0.528x         | 2 |
| **4**    | **22.08**           | 0.502x         | **0** |
| 8        | 33.85               | 0.780x         | 2 |

† **This row is contaminated and is the only one that is** — see "The defect
this campaign was one row away from finding" below.

One carrier cannot fill this path and is half of every other value. Eight is
the fastest median but two of its three attempts aborted mid-transfer and
finished on the relay. Four is the only value that completed every attempt on
the path it negotiated, across 11 repetitions in this campaign. The default
stays **4**.

### The finding that matters most, and it is not a tuning one

**On this WAN the relay is twice as fast as the direct path, and far steadier:**
43.5 MiB/s with a spread of 43.1–46.3 across twelve repetitions, against
10.8–33.9 for direct depending on the carrier count. The relay arm never
failed, never fell back and its verification time is half the direct arm's
(3.1 s vs 4.0–12.0 s for the same 128 MiB). A browser DataChannel is SCTP over
DTLS over UDP implemented in the tab; the relay leg is kernel TCP with the
server applying backpressure, and on a 21 ms path with no packet loss that is
simply the faster machine.

So "direct is the fast path and relay is the fallback" is **false on a clean
WAN**. Direct is the path that does not spend the operator's bandwidth and
does not put the payload through a third party; it is chosen for that, and
this document is the reason the choice is now an informed one. The transfer
falls back to the relay by itself whenever the direct path dies, and this
campaign exercised that fallback dozens of times end to end — every aborted
attempt completed, resuming from the ranges the recipient had already
verified.

### Re-running it

```bash
# one setting, both arms, three repetitions
SIZE_MB=128 REPS=3 ARMS=direct,relay CARRIERS=4 scripts/perf/web_transfer_wan.sh

# the carrier sweep (one server lifetime per value)
SWEEP=1,2,4,8 SIZE_MB=128 REPS=3 scripts/perf/web_transfer_wan.sh

# the queue-depth ladder: the marks reach the page through `__borePerf`
WT_HIGH_WATER=524288 WT_LOW_WATER=131072 SIZE_MB=128 REPS=5 ARMS=direct \
  CARRIERS=4 scripts/perf/web_transfer_wan.sh
```

`REMOTE` names an ssh host (default `awstest`) that holds `bore` and a
`node_modules` with Playwright's chromium under `REMOTE_DIR`
(`/home/ubuntu/wt`). Size it against the LINE, not against a round number
(V-19): 128 MiB is ~6 s of transfer on a 222 Mbit/s uplink, which is well past
the ramp; on a slower link raise it rather than keep the number.

### The defect this campaign was one row away from finding (B-A037)

Reported from the field the day after this campaign, on a LAN: a direct
transfer moving ~124 MB over `succeeded` host-host ICE pairs at 10-15 ms rtt
had **all four carriers closed at t=9999/10000 ms with `reason: null`**,
`drain.timeouts: 0`, and finished on the relay. The server's 10 s NEGOTIATION
deadline (`WEB_TRANSFER_DIRECT_DEADLINE`) was armed at `source_ready`, was
never cancelled, and `fallback_to_relay` accepted `TransferState::ActiveDirect`
— so it demoted a committed, healthy, actively carrying direct path and told
both peers `transfer.direct_failed reason=timeout`, which is what makes the
browser close its carriers. Deterministic, not flaky: **every direct transfer
longer than ten seconds end to end finished on the relay.**

Why 7.6 did not find it, and what that costs these numbers. The arm size was
128 MiB, chosen against the LINE (V-19). At the rates measured here that is
2.9 s on the relay and 3.8-6.8 s on direct — comfortably under ten seconds,
by accident. The one arm that is not under it is **carriers=1 at 10.84 MiB/s,
which needs 11.8 s of carrying alone**: that row crossed the deadline in every
repetition, its single "death" is the server demoting it rather than a
transport failure, and its median is a mixture of a direct start and a relay
finish. It is marked † above and must not be quoted. Every other row here,
the whole queue-depth ladder and the headline relay-vs-direct comparison at
the shipped `carriers=4` (5.8 s of carrying) completed before the deadline and
stand as measured.

The general lesson is V-19's, one turn further: a payload sized against the
line is also sized against every TIMEOUT the path contains, and a campaign
whose arms all sit just under one measures a product that has no such
timeout. An arm deliberately longer than the longest deadline in the path is
now part of re-running this (`SIZE_MB=512` on this link), and the defect
itself is gated without a WAN by `T-WEB-DIRECT-DEADLINE`, which holds a
committed direct transfer quiet for 13 s and asserts exactly one
`path_commit`.

### What this campaign did NOT measure

Two axes were swept here — send-queue depth and carrier count — and three were
not: the fragment size (24 KiB, chosen on LOOPBACK in phase 5, which is the
class of evidence this campaign already overturned once), the channel's
ordering, and whether the recipient's OPFS staging is what stalls the sender
through SCTP's receive window. None of them is closed, so "the direct path has
no margin left" is not something these numbers establish. The queue of
experiments, each with its discriminator, its cost and the bound it must
respect, is `WEB_TRANSFER_DIRECT_NEXT.md`.
