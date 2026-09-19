# The direct web-transfer path: its measured limit, and what the code says about it

> **Resume from this file.** It holds one campaign (2026-09-19, night), the
> limit it measured, and a code investigation into the mechanism. Nothing here
> has been acted on: no default was changed, no code was touched. The first
> section says what is settled, the last says what to do next and in what
> order.

Companion documents: `WEB_TRANSFER_PERF.md` §7.6 is the earlier campaign this
one extends (and partly decontaminates); `docs/plans/001_plan-WebTransfer/`
holds the plan state and the bug ledger.

---

## 0. The one-paragraph answer

With the shipped default (`--web-transfer-direct-carriers 4`), a transfer of
**1 GiB falls off the direct path** — measured twice, twice it fell. It does
not stay down: the server's relay→direct probe re-establishes it, so the path
**oscillates** (`direct → relay → direct`, and once
`direct → relay → direct → relay`). Every transfer completed: 1 073 741 824
bytes exactly, `errors: []`, hash verified, no user action. So the fallback is
transparent, which is what it was built to be. What it costs is bandwidth:
**14.00 and 29.53 MiB/s against 43.33 MiB/s for the same file carried entirely
by the relay** — 0.32x and 0.69x. And the direct path is the one that keeps the
operator's bandwidth off the wire, so every byte that reroutes is paid for
twice.

---

## 1. What is settled, and what is not

**Settled** (holds regardless of the caveats in §2, because it is a
structural fact and not a rate):

- **B-A037's fix is proven on the real path.** Three transfers of 512 MiB each,
  at one carrier, ran **64 seconds apiece and completed on a pure direct path**
  — one `path_commit`, `['direct']`, three times out of three. Before the fix
  the server's 10 s negotiation deadline demoted any direct transfer that
  lasted longer than ten seconds, so this outcome was impossible by
  construction.
- **`WEB_TRANSFER_PERF.md` §7.6's `carriers=1` row is decontaminated, and the
  explanation is the fix above.** That row recorded "1 of 3 attempts died". It
  was not the transport failing: it was the server demoting a transfer that had
  simply taken longer than ten seconds. At one carrier the direct path is now
  the *most* reliable configuration measured, not the least.
- **The failure scales with transfer LENGTH, and the old deadline was hiding
  it.** §7.6 measured `carriers=4` surviving 3 of 3 — at 128 MiB, i.e. 5.8 s.
  At 512 MiB the same configuration survived 1 of 6 across two runs. Nobody had
  ever seen this because everything longer than ten seconds was demoted anyway.
- **The carrier count trades throughput against survival, monotonically.** More
  carriers is faster and more fragile; fewer is slower and solid. There is no
  setting that gives both.
- **The fallback and the re-upgrade are bounded and transparent.**
  `upgrade_tries` is never reset (only incremented), so one transfer gets at
  most `WEB_TRANSFER_UPGRADE_TRIES` = 3 probes for its whole life, on a growing
  grid (20 s, 40 s, 80 s). The oscillation cannot run away.

**Not settled.** Every absolute MiB/s below is suspended, for two independent
reasons, either of which alone would be enough:

1. **The workstation was on WiFi.** The repository has already paid for this
   lesson once: V-10's TUN queue ladder moved latency 6x over WiFi and read
   *flat* over Ethernet. A ladder measured on a radio is not the general case,
   and a default changed on one would be a regression on every wired path.
2. **The remote is a `c7i-flex.large`** — 2 vCPU, burstable, delivering a
   baseline fraction of full CPU with credit above it. Arms here last 25–75 s,
   which is exactly the regime where credit runs out. On that instance "bore
   slowed down" and "the instance was throttled" are indistinguishable. This is
   the same shape as the t4g.micro network-allowance confounder that left N-9
   open in `WEB_TRANSFER_PERF.md` §12 and could not be settled locally.

A third possible ceiling could not be separated from the data: the recipient is
a headless browser doing AES-GCM and SHA-256 over the payload on those 2 vCPU.
**Note the trap avoided:** `verifyMs` cannot be used as evidence for it — for
the direct/c1 arm `verifyMs` is 63 999 ms against the source's own
`sendMs` 63 662 ms, i.e. it is the wall time of the whole transfer and not a
hashing cost, so reading a "verification rate" out of it would be circular.

---

## 2. The measurement

`scripts/perf/web_transfer_wan.sh` (`T-WEB-PERF-WAN`), 2026-09-19 ~01:30 CEST.

**Both ends run the same build**, `bore 1.2.0-rc.3 - dev - f74a14d7`: the
remote binary is both the server and the source of the frontend bundle the
local browser loads, so uploading it updates both halves.

| | |
|---|---|
| source | workstation, Linux, **on WiFi**, Playwright browser |
| server + recipient | `awstest` = `i-0f9ad4cf38050ea46`, **`c7i-flex.large`**, `eu-south-1b`, 2 vCPU Xeon Platinum 8488C, ~4 GB, `/dev/shm` 1.9 G |
| RTT | 22.493 / 23.146 / 24.508 ms (min/avg/max), mdev 0.613, 0 % loss over 10 packets |
| bare uplink | 192 MiB through `ssh` in 4.62 s = **349 Mbit/s = 41.5 MiB/s** |
| direction | workstation → cloud, i.e. cloud **ingress**, which is not billed |

Note for anyone re-reading §7.6: it records the host as `eu-central-1`. This
instance is in `eu-south-1b`. Either that line is wrong or the campaigns ran on
different machines — worth settling before the two sets of numbers are compared
directly.

### 2.1 Carrier sweep, 512 MiB per arm, 3 repetitions, arms alternating

```
SWEEP=1,2,4,8 SIZE_MB=512 REPS=3 bash scripts/perf/web_transfer_wan.sh
```

| carriers | direct samples (MiB/s) | median | **transfers that stayed direct** | relay samples | median |
|---|---|---|---|---|---|
| 1 | 8.04  7.98  8.48 | **8.04** | **3 / 3** | 43.80 43.59 42.14 | 43.59 |
| 2 | 37.66 15.33 14.59 | 15.33 | 2 / 3 | 42.72 43.99 43.93 | 43.93 |
| 4 | 33.44 39.75 39.26 | 39.26 † | **0 / 3** | 44.56 30.85 43.56 | 43.56 |
| 8 | 41.11 41.27 35.75 | 41.11 † | **0 / 3** | 44.81 43.44 43.84 | 43.84 |

† **These medians are not direct-path throughput.** Every repetition at 4 and 8
carriers ended `commits=['direct','relay']`, so the rate mixes both transports
and is pulled up toward the relay's. They are printed because suppressing them
would hide the contamination; they must not be quoted as a direct rate.

Per-attempt reasons and counters (source side — the side that waits on the
queue). `peak` is the group's AGGREGATE queued bytes, not one carrier's:

```
c1 rep0 rtt=19ms discarded=894  waits=511 longest=805ms  total=61644ms  peak=1180344 reason=none
c1 rep1 rtt=21ms discarded=0    waits=511 longest=406ms  total=62287ms  peak=1180972 reason=none
c1 rep2 rtt=21ms discarded=305  waits=511 longest=390ms  total=58371ms  peak=1180692 reason=none
c2 rep0 rtt=21ms discarded=0    waits=15  longest=518ms  total=3233ms   peak=1131312 reason=channel-closed
c2 rep1 rtt=19ms discarded=3925 waits=510 longest=2023ms total=105920ms peak=1049952 reason=none
c2 rep2 rtt=22ms discarded=4553 waits=510 longest=2330ms total=140977ms peak=1049952 reason=none
c4 rep0 rtt=18ms discarded=3161 waits=13  longest=922ms  total=5517ms   peak=858528  reason=channel-closed
c4 rep1 rtt=22ms discarded=285  waits=16  longest=550ms  total=3926ms   peak=897068  reason=send-error
c4 rep2 rtt=24ms discarded=5871 waits=18  longest=299ms  total=2659ms   peak=1330824 reason=none
c8 rep0 rtt=30ms discarded=41   waits=9   longest=590ms  total=2013ms   peak=670000  reason=none
c8 rep1 rtt=17ms discarded=3029 waits=13  longest=709ms  total=4222ms   peak=1186764 reason=channel-closed
c8 rep2 rtt=354ms discarded=512 waits=14  longest=1610ms total=10941ms  peak=949040  reason=channel-closed
```

Every selected candidate pair was `srflx/srflx` on every repetition: this path
traverses NAT on both sides, it is not host-to-host.

Note `c8 rep2 rtt=354ms`. One repetition of twelve saw an RTT fifteen times the
others'. On a radio that is ordinary; it is also exactly the kind of sample that
makes a WiFi ladder unquotable.

### 2.2 The earlier standalone run at 4 carriers, same night, same size

Run before the sweep, identical configuration, and it matters because it
contains the **one** surviving 4-carrier repetition:

```
direct  39.87  23.04  24.46     relay  43.92  43.89  43.68
c4 rep0 reason=none       commits=['direct','relay']
c4 rep1 reason=timeout    commits=['direct','relay']   longest=10002ms
c4 rep2 reason=none       commits=['direct']           24.46 MiB/s
```

So `carriers=4` is **1 survivor in 6**, not 0 in 6. And `rep1`'s
`longest=10002ms` is a carrier that sat on `DRAIN_TIMEOUT_MS` (10 s, the
per-carrier drain deadline in `webrtc.js`) and missed it — the only repetition
of the whole campaign that ended on that deadline rather than on a channel
event.

### 2.3 The question as asked: 1 GiB at the shipped default

```
SIZE_MB=1024 REPS=2 ARMS=direct,relay bash scripts/perf/web_transfer_wan.sh
```
(no `CARRIERS`, so the binary's own default of 4 applies — this measures the
shipped configuration exactly)

```
direct  14.00   commits=['direct', 'relay', 'direct']            discarded=4744 waits=604 longest=5727ms
direct  29.53   commits=['direct', 'relay', 'direct', 'relay']   discarded=3568 waits=16  longest=693ms
relay   43.83   commits=['relay']
relay   42.84   commits=['relay']
```

All four: `bytes = 1073741824`, `errors: []`.

---

## 3. The number that points at the mechanism

Take only the repetitions that stayed on a **pure** direct path, so no relay
bytes are mixed in:

| carriers | pure-direct rate | per carrier |
|---|---|---|
| 1 | 8.04 MiB/s | 8.04 |
| 2 | 15.33 MiB/s | 7.67 |
| 4 | 24.46 MiB/s (§2.2 rep2, the only one) | **6.12 — or 8.15 if only THREE carried** |

One carrier is 8 MiB/s. Two carriers are twice one carrier. **Four carriers are
three times one carrier.** 24.46 / 8.15 = 3.00.

That is one sample and it must not be over-read, but it is the sharpest lead in
the campaign, and it has a name in the code: a carrier that stalls is silently
excluded from the work and is never reaped (§4, H2). If it is right, then at
four carriers the product is paying for four SCTP associations and being
carried by three — and the one that is not carrying is still holding an
association, ICE keepalives and a share of the tab's CPU.

A second, independent reading of the 8 MiB/s figure: at 23 ms RTT,
8.04 MiB/s is a bandwidth-delay product of **≈ 189 KB**, which has the shape of
a fixed per-association receive window rather than of a path limit. Same
arithmetic as the native side's `window / RTT` floor. See H3.

---

## 4. Code investigation

Read: `web/transfer/src/webrtc.js` (`createCarrierGroup`, `waitLow`, `send`,
`leastLoaded`, `markDead`, `DRAIN_TIMEOUT_MS`), `web/transfer/src/sender.js`
(`waitForLowWater` and the chunk loop), `src/web_transfer.rs`
(`WEB_TRANSFER_UPGRADE_DELAY`, `WEB_TRANSFER_UPGRADE_TRIES`, `upgrade_tries`).

### How the group actually works

`createCarrierGroup` returns a sink that fans one byte stream across N
`RTCPeerConnection`s:

- **`send(bytes)`** picks `leastLoaded()` — the live carrier with the fewest
  bytes queued — and writes the whole frame to it. Work-conserving by design,
  and the comment says why: fixed round-robin would park the writer on a full
  carrier while others idle.
- **`waitLow()`** returns **immediately if ANY live carrier is at or below its
  own high-water mark**, and only otherwise awaits `Promise.any` over every
  carrier's own `waitLow`. The first carrier to drain releases the writer.
- **`markDead`** counts deaths and fires `events.onFailed` only when
  `gone >= count`, where `count` is the **configured** carrier count.
- Each carrier's own `waitLow` is bounded by `DRAIN_TIMEOUT_MS` = 10 s; missing
  it fails that carrier, not the attempt.

### H1 — the aggregate queue bound scales with N, and the mark was calibrated at one N. **Weakened by this campaign's own data.**

Because `waitLow` blocks only when *every* carrier is above its own mark, the
writer's effective in-flight bound is **N × `RTC_HIGH_WATER`** = N × 512 KiB.
`RTC_HIGH_WATER` was chosen by B-A032's depth ladder, which swept the mark at a
**fixed** `carriers=4` and never swept N — so raising the carrier count
silently re-enters the aggregate depth region that ladder proved dangerous
(at 4 carriers, 1 MiB per carrier produced an `sctp-failure` in 1 of 5).

**But the measured peaks refute it as the primary cause**: aggregate `peak` is
~1.0–1.3 MB at *every* carrier count, including 8 (where the bound would allow
4 MiB). The group never actually filled. Keep H1 on the list as a latent
hazard — the bound genuinely is N-proportional and genuinely was tuned at one
N — but it is not what happened here.

### H2 — a stalled carrier is excluded from work and never reaped. **Best supported.**

`leastLoaded()` picks the *minimum* queue, so a carrier whose queue is stuck
full is simply never chosen again. It is not dead: `member.dead` stays false,
`gone` does not advance, no event fires, no trace mark is written. And because
`waitLow()` short-circuits whenever any *other* carrier is below its mark, the
stuck carrier's own `waitLow` — the only thing that arms its 10 s drain
deadline — **may never be called at all**. It can therefore sit out the entire
transfer holding an SCTP association without ever being declared dead.

There is no carrier renewal on this path: nothing re-dials a lost carrier for
the life of an attempt, unlike the native side's carrier pool, which tops itself
up. The attempt silently degrades from N to N−1 and reports N.

Supporting evidence: §3's 24.46 = 3 × 8.15. Contradicting evidence: none found,
but only one pure 4-carrier sample exists.

**How to confirm or refute it cheaply, and this is tomorrow's first
experiment:** the trace already carries per-carrier `stats`; assert that every
carrier's `channel.bytesSent` is non-zero and within a factor of two of the
others at the end of a pure-direct 4-carrier transfer. A carrier that carried
nothing is H2, proven, in one run. If all four carried evenly, H2 is dead and
the 3.00 is a coincidence.

### H3 — the per-association window, not the path, sets the 8 MiB/s floor

8.04 MiB/s × 23 ms ≈ 189 KB of bandwidth-delay product. A browser's SCTP
association has its own receive window, and if that is the binding constraint
then **carriers are not a tuning knob, they are the only way to scale at all**,
and the real fix is a larger window rather than more associations. This also
explains the near-perfect linearity from 1 to 2 carriers.

`availableOutgoingBitrate` reads `?` in every trace this campaign produced
(`out_bitrate=?`) — it is the closest thing a browser exposes to a congestion
window, and it is the one measurement that would turn "per-association limit"
from an inference into a number. Closing that observability gap is already
recorded as the first item in `STATE.md` §9.

### H4 — N congestion controllers competing over one bottleneck

N independent SCTP associations, each with its own congestion control, share
one uplink. They collectively overshoot, the link drops, SCTP retransmits, and
past some point the association aborts. `packetsDiscardedOnSend` is the visible
symptom and it is large on the failing repetitions (3161, 3029, 4553, 5871)
— though not monotonically: `c8 rep0` failed nothing with `discarded=41`, and
`c2 rep1` completed on direct with `discarded=3925`. So discards alone do not
predict the outcome.

H4 predicts a WiFi/Ethernet split: a radio with variable rate and deep driver
queues is where N-flow overshoot hurts most. **The wired re-run is the
discriminator**, and until it runs H4 cannot be separated from "the radio did
it".

### H5 — CPU, on both ends

At 43 MiB/s and 24 KiB fragments the tab handles ~1 830 frames/s, each sealed
with AES-GCM, while the recipient hashes at the same rate — on 2 vCPU that are
themselves credit-limited. This is not a hypothesis about the algorithm; it is
a reason the *measurement* may be reporting the instance. Removing it means a
non-flex instance, not a code change.

### Not a defect, but a real cost: the group's hot path allocates per frame

`live()` is `members.filter(...)` — a **fresh array on every call** — and it is
called by `leastLoaded()` (hence by every `send`), and again by the
`bufferedAmount`, `highWater` and `fragmentBytes` getters, each of which also
iterates every member and reads a native WebRTC property. `sender.js`'s
`waitForLowWater` reads `bufferedAmount` and `highWater` before every chunk;
the chunk loop reads `fragmentBytes` once per chunk; `send` runs per fragment.

At ~1 830 fragments/s that is on the order of 2 000 array allocations per
second plus O(N) native property reads per fragment — roughly 15 000
`bufferedAmount` reads per second at 8 carriers — inside the same thread doing
the encryption. It is O(N), so it grows in exactly the direction the failure
grows. **This has not been measured and is not being claimed as the cause.** It
is a straightforward optimisation: cache the live set and invalidate it on
ready/death, and read each carrier's `bufferedAmount` once per decision instead
of once per getter.

### A robustness smell worth a test, found while reading

`markDead` announces the attempt dead when `gone >= count`, and `count` is the
**configured** carrier count, while the group explicitly tolerates an attempt
that establishes fewer ("the count is a ceiling and not a reservation"). A
carrier that never reaches `onReady` and never fails — stuck in ICE checking,
say — is neither live nor dead, so `gone` can never reach `count` and
`onFailed` can never fire. There is a second path out (`send` throws
`no direct carrier is open` when `live()` is empty, and the writer treats that
as an abandoned attempt), so this is not a proven hang. It is untested, and the
cheap fix is to compare against the number of carriers that ever became ready
rather than against the configured count.

---

## 5. What was deliberately NOT done

- **No default was changed.** Changing `direct_carriers` on WiFi + flex data is
  the exact mistake V-10 documents: that ladder moved 6x on a radio and was
  flat on Ethernet.
- **No code was touched.** Every item in §4 is a hypothesis with a stated way
  to confirm or refute it. H2 is the only one close to actionable, and it needs
  its one-run check first.
- **No conclusion was drawn from `verifyMs`** — see §1.

---

## 6. Tomorrow, in order

1. **Re-qualify the link, wired.** `ping` plus a bare `ssh` transfer, recorded
   in this file next to tonight's 23.1 ms / 41.5 MiB/s. Per V-19, re-derive the
   payload size from the new line *before* choosing `SIZE_MB` — a byte count is
   calibrated against a rate, so when the rate changes every size silently
   becomes a different experiment.
2. **Settle H2 in one run**, because it is cheap and it is either true or dead:
   a pure-direct 4-carrier transfer, then read each carrier's
   `channel.bytesSent` out of the trace. A carrier at zero proves it.
3. **Repeat §2.1's sweep wired**, same sizes, same repetitions, so the rows sit
   beside tonight's with one variable changed.
4. **If a non-flex instance is available, repeat on it too** (same region,
   same size). `c7i.large` is the obvious control for `c7i-flex.large`: same
   CPU, no credit.
5. **Only then** consider whether the default carrier count should move, and in
   which direction — which depends entirely on whether wired reproduces the
   1/6 survival at 4 carriers or reads flat.

Raw samples from tonight are under the session scratchpad and are NOT in the
repository; the tables above carry every number they contained.

## 7. Reproduction

```bash
# the binary must be the one under test at BOTH ends: the remote serves the
# frontend bundle the local browser runs
cargo build --release --all-features
scp target/release/bore awstest:/home/ubuntu/wt/bore

# link qualification first, always (V-9)
ping -c 10 <remote-ip>
dd if=/dev/zero bs=1M count=192 | ssh awstest 'cat > /dev/null'

# the sweep
SWEEP=1,2,4,8 SIZE_MB=512 REPS=3 bash scripts/perf/web_transfer_wan.sh

# the shipped default, at the size the question was asked about
SIZE_MB=1024 REPS=2 ARMS=direct,relay bash scripts/perf/web_transfer_wan.sh
```
