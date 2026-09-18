# The direct path — what is settled, and the next campaign

Written 2026-09-18, at the end of the WAN campaign that produced B-A028…B-A033
and sub-phases 7.6–7.7. Its purpose is that the **next** campaign starts from
the measurements rather than repeating them: everything below is either a
number that was measured on a real path, or an experiment written so it can be
run without re-deriving anything.

Read `WEB_TRANSFER_PERF.md` §7.6 for the campaign this one continues. Nothing
here is a plan commitment — it is a queue of hypotheses, ordered by expected
return, each with the discriminator that would settle it.

---

## 0. What is SETTLED. Do not re-measure these.

**The link, qualified before the first arm (V-9).** Workstation → AWS host in
`eu-central-1`: RTT 21.0–21.8 ms (`ping`, mdev 0.25 ms), uplink 27.7 MiB/s
(222 Mbit/s) measured with `scp` of 64 MiB before any bore process started.
Every figure below is on that path unless it says loopback.

**Relay, twelve repetitions:** 43.5 MiB/s median, spread 43.1–46.3, **zero**
failures, recipient verification 3.1 s for 128 MiB. Flat across every carrier
count, because carriers do not apply to it: that is **one** kernel TCP
connection.

**Direct, by carrier count, at the shipped 512 KiB queue depth:**

| carriers | median MiB/s | ratio to relay | attempts that aborted (of 3) |
|----------|--------------|----------------|------------------------------|
| 1        | 10.84        | 0.248x         | 1 |
| 2        | 23.05        | 0.528x         | 2 |
| **4**    | **22.08**    | 0.502x         | **0** |
| 8        | 33.85        | 0.780x         | 2 |

**Direct, by send-queue depth, at four carriers, five repetitions each:**

| per carrier | median MiB/s | failures in 5 | peak queued |
|-------------|--------------|---------------|-------------|
| 2 MiB       | 18.76        | 1 fallback    | 2.36 MB |
| 1 MiB       | 20.19        | 1 sctp-failure| 1.32 MB |
| **512 KiB** | **21.45**    | **0**         | 0.80 MB |

**The abort signature** is `RTCError` with `errorDetail: "sctp-failure"` and
`sctpCauseCode: 12`. The trace records both since this campaign; before it, the
mark read `channel-error` and nothing else.

**Where the source's time goes** (one 256 MiB direct arm): `src.wall` 3444 ms,
of which `src.drain` **2844 ms** — the pipeline blocked on the channel.
`src.hash` 242 ms, `src.seal` 343 ms, `src.send` 247 ms. So the transport is
the limit and the crypto/hash pipeline is not; do not go looking there again.

**Where the recipient's time goes** (same arm): `dst.e2e` 6524 ms, `dst.busy`
5294 ms, `dst.stage` 4167 ms of which `opfs.sync` **4146 ms**.

**The diagnosis of the gap.** One TCP connection reaches 43.5 MiB/s where one
SCTP association reaches 10.8 and eight reach 33.9. Scaling with parallelism is
the signature of a **per-association** limit — window and congestion control —
and not of a path limit or a CPU limit. Bandwidth-delay product of the path is
21 ms × 43.5 MiB/s ≈ **0.93 MB**; at 10.8 MiB/s the association's effective
window is ≈ 230 KB, so one association never fills the pipe.

**Loopback is not a path, and this campaign proved it twice.** Phase 5's depth
ladder "confirmed" 4 MiB on loopback while comparing *only the healthy
repetitions* — the repetitions it set aside were the same defect. Re-measured
on loopback, the shipped 512 KiB raises the median (46.27 vs 45.82 MiB/s) and
moves the worst sample from 2.36 to 14.02. **Any conclusion below that is
reached on loopback must be re-run on a real path before it ships.**

---

## 1. The experiments, in order of expected return

Each is one variable. The campaign rules apply to all of them: qualify the line
first (V-9), print raw samples beside every median (V-11), alternate the arms
inside each repetition (V-13), size the payload against the measured line and
not against a round number (V-19), and never call a failed arm a number.

### E1 — Why do eight carriers abort? (largest known win)

**The prize is already measured:** c8 moves 33.85 MiB/s against c4's 22.08,
+54 %, and the only reason 8 is not the default is that two of its three
attempts aborted mid-transfer. The throughput is not in question; the
stability is.

**Hypotheses, in the order they should be tested:**

1. *Aggregate depth.* Eight carriers at 512 KiB is 4 MiB in flight, which is
   the depth that killed four carriers at 1 MiB. Test: scale the per-carrier
   mark by the carrier count (`total / carriers`, floor ~128 KiB) so the GROUP
   budget stays at the measured-good 2 MiB, and re-run c8. If the aborts stop,
   the bound belongs on the group and not on the carrier — which is also the
   shape a user changing `--web-transfer-direct-carriers` would expect.
2. *Simultaneous setup.* All carriers negotiate at once; eight DTLS handshakes
   and eight SCTP associations open inside the same few hundred ms on one
   renderer thread. Test: stagger the opens by 50–100 ms and re-run.
3. *A per-process or per-PeerConnection resource.* Test: c6 as a midpoint. A
   clean break between 4 and 6 says something structural; a gradient says
   load.

**Instrumentation to add first:** the attempt trace already carries
`errorDetail`/`sctpCauseCode`; add which carrier index died and at what
`bufferedAmount`, so "the one that was carrying" is distinguishable from "one
that was idle".

**Cost:** ~1 h of runs. **Risk:** low — the default only moves if a
configuration completes every attempt across at least five repetitions.

### E2 — Fragment size, re-measured on a real path

`MAX_FRAGMENT_BYTES` is **24 KiB**, chosen in phase 5 on **loopback** against
8 and 16 KiB. That is the same class of evidence B-A032 overturned, and the
mechanism points the other way on a WAN: fewer, larger SCTP messages mean
fewer trips through usrsctp, which is the slow component.

**Bounds to respect, and one of them will bite:**
- `pc.sctp.maxMessageSize` (Chromium publishes 256 KiB; the code already
  derives from it in `fragmentBytesFor`).
- **`MAX_INBOUND_BYTES` is 64 KiB** and the receive path rejects anything
  larger (`webrtc.js`, the `data.byteLength > MAX_INBOUND_BYTES` guard). A
  fragment above ~64 KiB − headroom therefore needs that constant raised in
  the same change, and it is a peer-facing bound: raising it widens what a
  hostile peer can make the page allocate per message. Raise it deliberately,
  with the number written down, or cap the ladder at 48 KiB.
- The relay path uses the same 24 KiB and the manifest/receive code is written
  for it. Change the DIRECT fragment only, or measure both.

**The ladder:** 24 (control), 48, 64 KiB — and, only if `MAX_INBOUND_BYTES` is
raised deliberately, 128 and 192 KiB. Three repetitions each, direct arm,
c4, 128 MiB.

**The knob already exists:** `perfFragmentBytes()` reads
`globalThis.__borePerf.fragmentBytes`, and `wan-source.mjs` already injects
`__borePerf` (that is how `WT_HIGH_WATER`/`WT_LOW_WATER` reach the page) — add
`WT_FRAGMENT` beside them, three lines.

**Discriminator:** if throughput rises with fragment size and `src.drain`
falls, the per-message cost in usrsctp is real and the fragment is the cheap
win. If it is flat, the association's window is the whole story and E1/E3 are
where the remaining margin is.

**Cost:** ~30 min. **Risk:** low for 48/64 KiB, medium above (peer-facing
bound).

### E3 — An UNORDERED channel

SCTP delivers in order by default, so one lost packet holds every later
message in that association — head-of-line blocking *inside* the carrier. The
campaign did record `discardedPackets` in the thousands on some repetitions
(0, 491, 1059, 2325, 2376, 3904, 15542 across the runs), so loss on this path
is real and not hypothetical.

**The receiver is already ready for it.** `receiver.js` reorders on the `seq`
in each frame's own header, and `transfer.reorderWindow` is armed whenever
`carriers > 1` — that machinery exists because N associations already deliver
in N independent orders. What blocks the experiment is one defensive check:
`webrtc.js` fails an attempt whose `channel.ordered === false`, and the
channel is created with `ordered: true`.

**The experiment:** create the channel `ordered: false`, keep the reorder
window armed **unconditionally** (not only at `carriers > 1`), relax that
guard, and run the same c1/c4 arms. Do NOT ship it on the strength of one
green run: an unordered channel with a single carrier is a new path for
`reorderWindow`, so the resume/verify gates must all be re-run.

**Discriminator:** unordered helps only where there is loss. Run it in the
same repetition as the ordered control; if the two are within noise on a clean
path, keep it for a lossy-path arm (see E5) rather than shipping it.

**Cost:** ~2 h including the gate re-runs. **Risk:** medium — it changes what
the receive path must tolerate at `carriers == 1`, which today is the
byte-identical legacy path.

### E4 — Does the RECIPIENT throttle the sender?

`dst.busy` 5.29 s and `opfs.sync` 4.15 s on a 256 MiB direct arm. When the
recipient stops reading, SCTP's receive window closes and the sender stalls —
and the sender's own trace cannot tell that apart from congestion. On the
**relay** arm the same staging work happens, but the server absorbs the burst
in its own buffers, so the browser source never feels it. That asymmetry is a
candidate explanation for part of the 2× gap and it is **unquantified**.

**The experiment:** run the direct arm with the recipient staging to memory
instead of OPFS (a harness-only mode, never a shipped one), same size, same
repetitions. If the direct rate rises materially, the receiver's drain is a
real share of the gap and the fix is on the receive path — larger staging
batches, or staging off the read path — not on the transport.

**Instrumentation that makes it readable:** sample `bytesReceived` on the
recipient's transport and the inbox depth on the same 1 s tick the stats
already use, so "the sender stalled" and "the receiver stopped reading" are
two curves instead of one.

**Cost:** ~1 h. **Risk:** none to the product — the memory-staging mode is a
harness arm.

### E5 — A LOSSY arm, because every number so far is from a clean path

Both this campaign and the native ones (V-12, V-13) measured on paths with
`lost_pct` 0.00, and both said the same thing in their own words: a default
chosen on a clean path has not been measured on the case it is worst at. The
direct path's whole claim — that it survives where it is chosen for cost and
privacy — is untested under loss.

**The experiment:** the same arms behind `netem` with 0.1 % and 1 % loss and
±5 ms jitter. This is what would justify E3 (unordered), and it is the arm
that decides whether the shipped queue depth is still right when a
retransmission is in flight.

**Cost:** ~1 h with the netns harness already in the repository.

---

## 2. Observability gaps to close first (cheap, and they make the rest readable)

- **`availableOutgoingBitrate` reads `?`** in every trace this campaign
  produced (`out_bitrate=?`). It lives on the candidate pair and Chromium does
  publish it, so either `summarizeStats` does not read it or it reads the
  wrong report. It is the closest thing a browser exposes to the congestion
  window, and it is exactly the number that would turn "per-association limit"
  from an inference into a measurement. Fix this **before** E1.
- **Which carrier died.** The trace names the attempt, not the role of the
  carrier that failed.
- **Sender stall vs receiver stop.** See E4.

---

## 3. Dead ends — priced, with the reason. Do not retry without new evidence.

- **Socket or window tuning from the page.** A page cannot reach the UDP
  socket under an `RTCPeerConnection` and no API exposes the SCTP buffer. The
  equivalent knob is the send queue depth, which this campaign swept.
- **A second UDP socket, or reusing the native QUIC/hole-punch stack.** Plan
  invariant D5. Not a performance question.
- **Application-level retransmission over the DataChannel.** SCTP is already
  reliable and ordered; a second reliability layer on top is the TCP-over-TCP
  meltdown V-15 documents for the VPN.
- **Per-engine branches.** Rejected in 3.11 for the staging path and the
  reason has not changed: three engines, one code path, differences measured
  rather than special-cased.
- **Deepening the send queue.** Measured, twice, in both directions: deeper is
  slower AND less stable (§0).

---

## 4. The honest ceiling

A browser DataChannel is usrsctp running in the renderer. It will probably not
match a kernel TCP connection on a clean path, whatever is done here. A
realistic target for the work above is closing 0.50× → ~0.8× of the relay, not
parity: c8 already reads 0.780× when it survives.

That does not make the direct path the loser. It spends **no server bandwidth**
and puts the payload through **nobody else** — which is why it is the default
and why `--relay-only` exists for the operator who wants the opposite. The
purpose of this queue is to make that choice cost less, not to win an argument
with TCP.

---

## 5. Running any of it

```bash
# the harness, and the three axes it already knows
SIZE_MB=128 REPS=3 ARMS=direct,relay CARRIERS=4 scripts/perf/web_transfer_wan.sh
SWEEP=1,2,4,8 SIZE_MB=128 REPS=3 scripts/perf/web_transfer_wan.sh
WT_HIGH_WATER=524288 WT_LOW_WATER=131072 SIZE_MB=128 REPS=5 ARMS=direct \
  CARRIERS=4 scripts/perf/web_transfer_wan.sh
```

`REMOTE` names an ssh host (default `awstest`) holding `bore` and a
`node_modules` with Playwright's chromium under `REMOTE_DIR`
(`/home/ubuntu/wt`); `scripts/perf/web_transfer_lan.sh` is the same experiment
between two machines on one LAN, at no bandwidth cost. Bytes travel
workstation → cloud, which is ingress and is not billed — that direction is a
property of the topology and the reason a 128 MiB arm is affordable.

**Rebuild before every campaign:** `npm run build --prefix web/transfer`, then
`cargo build --release --all-features`, then copy the binary to the remote. The
server embeds the page at compile time, so a stale binary measures a stale
page — and the harness's own freshness check only covers the local side.
