# Future campaign — web transfer performance

Written 2026-09-24, at the end of a CODE review of `bore transfer web` and
`bore transfer link` (bug ledger rows B-A044…B-A056 in
`docs/plans/001_plan-WebTransfer/bugs.md`). **Nothing in this file was
measured.** No campaign was run, no VM or second host was used; the review was
explicitly limited to reading the code and to fixes gated by local tests. Every
item below is a hypothesis with the experiment that would settle it. The
measured baseline these items build on is `WEB_TRANSFER_DIRECT_NEXT.md` (§0–§2)
and `WEB_TRANSFER_PERF.md`; its open experiments E2–E5 still stand and are not
repeated here.

Measurement rules that still apply (they earned their place in earlier
campaigns): loopback is not a path; interleave the arms and sample a bare
control in the same repetition; qualify the access link first
(`scripts/perf/staging/vpn/link_baseline.sh`); a transfer shorter than the ramp
measures the ramp; print raw samples beside every median.

## 1. Changes made in this review that MOVE bandwidth and must be measured

These landed as correctness fixes, each with a red-checked gate. Their effect on
throughput was never measured.

### 1.1 The relay → direct upgrade now actually happens (B-A049)

**Before:** at the upgrade commit the SOURCE detached its relay leg without
closing it, and that leg's `onclose` did not check which leg it was. When the
server then closed the abandoned leg, the source read "relay died before
FINAL", reported `FAILED` and forgot the transfer, tearing down the direct
channels it had just adopted. The new e2e `T-WEB-UPGRADE` red-checks it: without
the fix the row never leaves `relay`. So in the field **no transfer ever
completed an upgrade** — the 20/40/80 s grid either did nothing or broke the
transfer it touched.

**After:** the upgrade switches and finishes on the direct path.

**Why it needs measuring:** on a clean WAN the relay was measured FASTER than
the direct path (43.5 MiB/s against 33.9 at eight carriers,
`WEB_TRANSFER_DIRECT_NEXT.md` §0). An upgrade that now works can therefore
LOWER the rate of a long transfer on such a path, in exchange for taking it off
the server's egress and relay slot. That trade was the design of phase 7.5; it
simply never took effect before.

**Experiment:** the WAN harness with a transfer that starts on the relay (the
recipient's WebRTC absent for the first attempt, as `T-WEB-UPGRADE` does) and
runs well past 20 s. Arms: upgrade enabled vs `set_upgrade_delay(0)`
(there is no CLI switch; a harness-only build flag or env seam would be needed).
Report per arm: end-to-end MiB/s, bytes the server relayed, time to switch.
Decide from it whether the grid should stay unconditional, or should only
upgrade when the relay rate is below what a direct attempt measured.

### 1.2 The upgrade commit now carries the recipient's verified ranges (B-A050)

**Before:** when the source was the SECOND side to send `direct_ready`, the
commit was computed on the server's stale `record.resume` and the direct attempt
resent everything the relay had already delivered. `T-WEB-UPGRADE` saw a commit
with no `resumeRanges` at all.

**After:** the recipient's ranges are held from its own ready.

**Expected effect:** an upgraded transfer sends only what the relay had not
delivered. **Experiment:** in the 1.1 run, compare bytes sent on the direct
attempt with `size − verified-at-commit`; they should match within one chunk.

### 1.3 The session outgoing queue scales with carriers (B-A047)

**Before:** 64 messages, against a legal counterpart burst of 172 at eight
carriers (the burst was widened in B-A040, the queue was not). An overflow
dropped a forwarded ICE candidate or SDP silently.

**After:** 64 + 16 × (carriers − 1) = 176 at eight, and every drop is logged
(sampled at powers of two: `web-transfer control queue full`).

**Expected effect:** fewer direct attempts lost to a missing candidate; memory
per session is bounded by small forwarded messages in practice. **Experiment:**
count `control queue full` lines and direct-attempt failures over a many-pair
soak at eight carriers, before and after (the counter did not exist before, so
"before" is the failure rate only).

## 2. Candidates found by reading the code — NOT implemented

Each would change a hot path; none is justified without the experiment.

### 2.1 Recipient staging is serial per chunk and may be the LAN ceiling

`receiver.js` `acceptData` awaits, per 1 MiB chunk: join → SHA-256 →
`writeChunk` (stage worker, `createSyncAccessHandle` + `flush`) →
`saveRecord` (one IndexedDB transaction) — and only then opens the next frame.
The measured 256 MiB direct arm had `dst.busy` 5.29 s (≈ 48 MiB/s) against a
source that finished in 3.44 s, with `opfs.sync` 4.15 s of it: the recipient
drained a backlog for three seconds after the last byte left. Frames keep
arriving meanwhile (DataChannel events are dispatched while the pump awaits), so
this does not throttle the wire — it bounds the END-TO-END rate whenever the
wire is faster than ~50 MiB/s (a LAN, or two tabs on one host).

**Candidate:** overlap staging of chunk N with decrypt/hash of chunk N+1 (at
most two chunks in flight, records still written in order and only after the
part file closed — the invariant `stage-worker.js` documents). Estimated upper
bound from the numbers above: `dst.busy` −20 %. **Risk:** the plan position,
`verifiedRanges` and the stale-attempt checks are all per-chunk today; a
pipelined commit must keep "what is reported is what is on disk" exactly.
**Experiment:** E4 in `WEB_TRANSFER_DIRECT_NEXT.md` first (memory staging vs
OPFS); only if it moves the rate, the pipelined variant.

### 2.2 One IndexedDB transaction per chunk

`saveRecord` runs after every verified chunk (`dst.commit`). Resume already
re-verifies every claimed chunk against the manifest digest (`rehashRecord`),
so a record that lags by a few chunks costs only their re-receipt, never a wrong
resume. **Candidate:** save the record every N chunks or every 250 ms, plus
always at FINAL and on attempt end. **Experiment:** `dst.commit` total on a
256 MiB arm; worth doing only if it is a visible share of `dst.busy`.

### 2.3 Relay socket read buffer 4 KiB for 32 KiB frames

`relay_websocket_config` pins `read_buffer_size` 4 KiB, so each relay frame is
read in ~8 socket reads (≈ 11 k reads/s at 44 MiB/s) on each leg. The relay was
measured at the one-TCP-connection limit with the server not CPU-bound, so this
is CPU per GiB, not throughput. **Candidate:** 32 KiB read buffer on relay legs
only (memory: +28 KiB × 2 legs × `--web-transfer-max-relays`). **Experiment:**
server CPU s/GiB on a relayed transfer, same rate, both buffer sizes.

### 2.4 Relay pump flushes every frame

`run_relay_pair` does `send` (feed + flush) per 32 KiB frame, one frame in
flight. The pipelining is the kernel's socket buffers, and at ~1 400 frames/s
the flush count is not a plausible limit. Recorded so nobody re-derives it:
**do not batch here without a CPU measurement showing the flush matters**, and
never hold more than the bounded frame the pump holds today (that bound is what
keeps a slow recipient from ballooning server memory).

### 2.5 `transfer link`: one copy per 1 MiB chunk

`source.rs` reads into a reused `Vec` and `Bytes::copy_from_slice`s each chunk
into the queue. Reading into a fresh `BytesMut` and `freeze()`ing it would drop
one memcpy per MiB. Link is gated against the vhost baseline by
`scripts/transfer_link_perf.sh` (CI), which it meets; this is CPU, not
bandwidth. **Experiment:** that script's CPU column at a rate where the link
host is CPU-bound, if such a host exists.

## 3. What this review found NOT to be a limit

- **Server relay path:** one kernel TCP per leg, bounded one-frame pump,
  per-room throttle 100 MiB/s by default (`--web-transfer-relay-rate`). Measured at the relay's own
  TCP ceiling in prior campaigns; the code offers no further lever short of
  multiple relay connections per transfer, which the protocol does not have.
- **Direct path:** per-association SCTP window inside the browser engine —
  outside bore's code. Carriers (default 8) are the lever and are already
  sized; queue depth 512 KiB per carrier was measured.
- **Crypto and hashing:** `src.hash` + `src.seal` ≈ 17 % of `src.wall` on the
  measured arm; the source is transport-bound (`src.drain` 83 %).
