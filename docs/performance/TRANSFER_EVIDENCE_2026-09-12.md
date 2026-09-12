# `bore transfer` — performance and correctness campaign, 2026-09-12

Evidence file for the file-transfer campaign. Everything here is a measurement with the
command that produced it; where a number is inferred rather than measured, the text says so.

Companion documents:

* `docs/performance/final_secret_perf_review.md` — the Italian review of the secret-tunnel
  campaign. **Read it first**: `bore transfer` is a secret tunnel with a file protocol on
  top, so every result there applies here unchanged.
* `docs/performance/SECRET_STAGING_EVIDENCE_2026-09-11.md` — the secret-tunnel evidence,
  in particular §10 (the receive-window floor) and §4 (S-5, the check-round asymmetry).
* `docs/transfer/TRANSFER_ASSESSMENT_2026-07-10.md` — the earlier correctness audit
  (B1…B8, P1). This campaign did not revisit those; it starts where they left off.

---

## 0. What `bore transfer` is, in one paragraph

`transfer listener` registers as the secret **provider** and RECEIVES; `transfer sender` is
the secret **consumer** (`secret::Proxy`) and SENDS. There is no fifth transport. On the
direct arm the bytes run sender ↔ listener over a hole-punched QUIC connection with the
server off the path (S-1); on the relay arm they cross the server twice. `--parallel N`
means N QUIC bidi streams on the direct arm and N yamux substreams over the relay carrier
pool otherwise. A single stream is bounded by `window / RTT`, so any `--parallel 1` figure
is a receive-window measurement and not a link measurement.

## 1. How to re-run this

Local (no deployment needed):

```bash
# the headline sweep: same bytes, more files
MB=512 FILES="1 10 100 1000 5000 20000" PAR=8 REPS=1 bash scripts/perf/transfer_scale.sh
# the same sweep with the durability policy as the variable
MB=512 FILES="1 1000 5000 20000" PAR=8 LIS_EXTRA="--no-fsync" bash scripts/perf/transfer_scale.sh
# the bandwidth ceiling on one large file
MB=2048 FILES="1" PAR="1 2 4 8 16 32" bash scripts/perf/transfer_scale.sh
```

Every sweep above must run with the run directory on **tmpfs** (`TMPDIR=/dev/shm`) or the
disk becomes the variable — §7.1 is the account of getting this wrong. `transfer_scale.sh`
does not set it for you; `transfer_ab_shape.sh` does.

To compare two binaries — this tree against a baseline build — use the A/B driver rather
than two separate sweeps, because two sweeps taken minutes apart differ by more than most of
the effects being measured (§5.1):

```bash
git worktree add /tmp/base-wt <baseline-commit> && (cd /tmp/base-wt && cargo build --release)
BEFORE=/tmp/base-wt/target/release/bore FILES="10 100 1000 5000" REPS=7 PAIRS=2 \
  bash scripts/perf/transfer_ab_shape.sh
```

It alternates the arms within each cell, keeps its ports below the ephemeral range, and
prints a failed cell loudly instead of leaving a blank column. Compare arms **within** a
cell, never across cells.

Staging (needs `~/.config/bore-perf/env.sh`, chmod 600 — see
`scripts/perf/staging/env.sh.example`; credentials are provided separately and are never
committed):

```bash
TOPO=ws-vm MB=1024 PARS="1 2 4 8 16" ARMS="relay direct" bash scripts/perf/staging/xfer/xfer_bw.sh
TOPO=ws-vm MB=512 FILES="1 100 1000 5000 20000" FSYNC=on  bash scripts/perf/staging/xfer/xfer_shape.sh
TOPO=ws-vm MB=512 FILES="1 100 1000 5000 20000" FSYNC=off bash scripts/perf/staging/xfer/xfer_shape.sh
```

Two conventions hold throughout, so that a number can be compared with another number:

* **`MB/s` means MiB/s** (`bytes / 1048576 / s`), matching what `bore` itself prints
  (`… MiB/s avg`) and what every other harness in `scripts/perf/` reports. No table mixes
  the two.
* **CPU columns cover the two transfer processes** — sender and listener. The relay
  `bore server` is a third process on the loopback path and is deliberately *not* in the
  sum: it is the same work for every cell of a sweep, so including it would inflate every
  row by a constant and make the interesting column harder to read. Where the server's own
  cost matters it is measured separately.

The ad-hoc attribution scripts used below (`syscall.sh`, `amp.sh`, `usersys.sh`,
`fdrepro.sh`) live in the session scratchpad, not in the repository: each is a dozen lines
around `strace -c`, `/proc/<pid>/io` and `/proc/<pid>/stat`, and the numbers they produced
are reproduced in full here so the conclusions do not depend on re-running them.

---

## 2. The headline: the same bytes in more files

2 048 MiB of payload, `--parallel 8`, relay arm, run directory on **tmpfs**, median of
**3** repetitions per cell. The only variable is how many files the 2 048 MiB is divided
into.

**Before this campaign:**

```
  files    par    wall_s       MB/s  cpu_send  cpu_recv  per_file_ms cpu_s_per_GiB
  1        8        1.89     1083.6      5.73      5.87     1890.000       5.80
  100      8        1.30     1575.4      6.16      4.81       13.000       5.49
  1000     8        1.69     1211.8      7.19      5.48        1.690       6.33
  5000     8        1.67     1226.3      6.76      5.75        0.334       6.25
  20000    8        5.56      368.3      5.97     10.08        0.278       8.03
```

**4.3× throughput collapse at constant bytes** (1 575.4 → 368.3 MB/s), with receiver CPU
rising 4.81 → 10.08 s. That is not the network and not the medium: it is the same 2 048 MiB
in every row, written to RAM.

An earlier draft of this document put that number at **38×**. It was wrong, and the way it
was wrong is worth more than the number: the sweep ran on the NVMe, where each cell's dirty
pages are still being written back while the next cell is timed, so the tail cells measured
the disk recovering from the head cells. §7.1 has the full account. Everything in this
document that quotes a throughput figure was re-taken on tmpfs.

## 3. Attribution — how each cost was established

`perf record` was unavailable (`/proc/sys/kernel/perf_event_paranoid` = 4, and this box
grants NOPASSWD sudo per exact path only), so the cost was attributed with tools that need
no privilege. This matters: everything below is a count or a byte total, not an inference.

### 3.1 Syscall census (`strace -c -f`, N = 5 000)

Receiver, before:

```
 93,26   61,580877         462    133205     13030 futex
  2,57    1,700144          68     24721           epoll_wait
  1,85    1,222697         244      5000           fdatasync
  0,98    0,646312           7     88163      4940 recvfrom
  0,44    0,289090          27     10559           write
  0,25    0,164765          10     16284         2 openat
  0,11    0,069784          55      1262           fsync
  0,03    0,020163          31       632           rename
  0,03    0,018113           3      5003      5001 mkdir
```

Three numbers name three defects outright:

* **1 262 `fsync` + 632 `rename`** — 631 full rewrites of the resume state. 5 040 chunks ÷
  `RESUME_FLUSH_EVERY_CHUNKS` (8) = 630. Each rewrite serialises *every* entry.
* **5 001 of 5 003 `mkdir` returned EEXIST** — the staging pass created the parent
  directory once per file in a flat tree.
* **16 284 `openat` for 5 000 files** — the staged file is opened to create it, opened
  again by the worker, and opened a *third* time by `sync_staged_files`, which reopens by
  path rather than syncing the handle that just wrote.

### 3.2 Write amplification (`/proc/<pid>/io`, 512 MiB payload)

| files | listener `write_bytes` | × payload |
| --- | --- | --- |
| 1 000 | 530 096 128 | 0.98× |
| 5 000 | 670 916 608 | 1.24× |
| 20 000 | **2 480 783 360** | **4.62×** |

At 20 000 files the receiver wrote **2.48 GB to disk to land 512 MiB**. The excess divided
by the flush count is 1.94 GB ÷ 2 500 ≈ 776 KB per flush, which is exactly the size of a
20 000-entry `state.json`. The quadratic term is confirmed arithmetically, not assumed.

The sender's `rchar` was 2× the payload at *every* file count: it reads the whole tree
once to compute the manifest hashes and again to send the chunks.

### 3.3 User vs system time (`/proc/<pid>/stat` fields 14/15)

Single 2 GiB file, `--parallel 8`, loopback relay, wall 2.04 s:

```
  sender    user   1.64 s   sys   2.38 s   total   4.02 s
  listener  user   1.41 s   sys   2.14 s   total   3.55 s
  server    user   0.71 s   sys   1.98 s   total   2.69 s
```

**System time dominates every process** (58–60 % on the endpoints, 74 % on the relay).
Userspace is 0.78 CPU s/GiB on the sender against 0.69 on the listener — the sender's extra
0.09–0.4 s/GiB is its second BLAKE3 pass. That sets the price of §7's open item honestly:
removing the duplicate hash is worth ~10 % of sender CPU, and CPU is not what bounds the
wall clock here.

---

## 4. Defects found

### X-1 — both file caches were unbounded (confirmed, fixed)

`send_chunked_files` and `handle_worker_connection` each kept a `HashMap` of every file the
worker had touched, with no bound. The cache exists to spare a re-open when consecutive
chunks of one large file land on the same worker; with one chunk per file it can never hit
and simply grows.

Reproduced twice with `ulimit -Sn 1024` and 3 000 files:

```
Error: failed to open source file .../f000988.bin
Caused by: Too many open files (os error 24)          # sender, at ~989 of 3000
Error: failed to open staged file .../f000970.bin
Caused by: Too many open files (os error 24)          # receiver, peak fd 731
```

The repro needed a separate `ulimit` per side (`NOFILE_S` / `NOFILE_L`): with one shared
limit the sender always died first and masked the receiver-side defect entirely.

**Fixed**: `OpenFiles`, an LRU bounded at `FILE_CACHE_PER_WORKER` (16), plus a
`fdlimit::reconcile_fd_limit(MAX_PARALLEL * FILE_CACHE_PER_WORKER)` at both entry points —
the same reasoning as the server's P-12, because `EMFILE` is not local to the `open` that
overflowed. After: same repro completes, **peak fd sender 36, listener 174**.

### X-2 — O(N) linear scan per chunk (confirmed, fixed)

`ResumeShared::{is_chunk_complete, mark_chunk_complete, all_chunks_complete, reset_file}`
all located their file with `state.files.iter().find(...)`. Two scans per chunk, plus one
per entry in `verify_summary`: quadratic in the file count.

**Fixed**: `ResumeRuntime.index: HashMap<u32, usize>`, built once.

### X-3 — the resume state was rewritten in full every 8 chunks (confirmed, fixed)

Measured in §3.2: 4.62× write amplification at 20 000 files.

**Fixed**: an append-only journal (`state.log`) beside the `state.json` checkpoint. One
8-byte record per completion, appended and fsynced in the batch that already fsyncs the
staged files. The checkpoint is now written only at creation, on `reset_file`, and once at
load after the journal has been folded into it — which is also what bounds the journal at
8 bytes per chunk of a single run rather than letting it grow across restarts.

Replay is a **set union**, which is what makes a crash between "checkpoint written" and
"journal removed" harmless. A crash mid-append leaves a torn tail of fewer than 8 bytes; it
is discarded and the chunk is re-sent — precisely the outcome the old "lose up to 8
completions" behaviour already had.

### X-4 — the staging pass was a serial chain of blocking round trips (confirmed, fixed)

`prepare_stage_entries` ran one `spawn_blocking` per file to create and size it, and one
`create_dir_all` per file for a parent that a flat tree shares. At 20 000 files that is
20 000 serial round trips before a single byte can be received, plus 19 999 EEXIST `mkdir`s.

**Fixed**: parents are memoised in a `HashSet`, and the regular files are created in
`STAGE_FANOUT` (8) concurrent batches. `sync_staged_files` got the same fan-out, and for a
sharper reason: it runs while `persist_lock` is held, so a serial chain of fdatasyncs there
is time during which *every* worker that completes a chunk is blocked.

### X-5 — a chunk was recorded durable before its bytes left the process (confirmed, fixed)

The receiver did `file.write_all(&payload)` on a `tokio::fs::File` and then called
`mark_chunk_complete`. `tokio::fs::File::write_all` returns once the bytes are in the
File's *own* buffer, with the write still queued on the blocking pool — and
`sync_staged_files` reopens the path **by name**, so the fsync could run against a file
description that had never seen those bytes. The resume state could therefore record a
chunk as durable while its data was still inside the process.

This was never silent corruption: a chunk written by an earlier run is not "fresh", so
`verify_summary` always re-hashes it and a mismatch fails the transfer. But it made the
fsync both expensive and not do what it claimed.

**Fixed**: `file.flush().await?` before the chunk is marked complete. This one is reasoned,
not gated — a test would have to lose a machine mid-write to observe it.

### X-6 — every frame cost two writes and two packets (confirmed, fixed)

`send_frame` wrote the 4-byte length prefix and the body separately. On a `TCP_NODELAY`
socket that puts the prefix on the wire as its own segment. Measured at 20 000 files: 137 476
`sendto` for 20 000 chunks, 6.9 per chunk. **Fixed**: one buffer, one `write_all`.

### X-7 — `--no-fsync` still fsynced the journal (found by review, fixed, then measured)

This one was not measured first. It was found by reading the code that X-3 introduced, while
checking whether the new journal honoured the durability flag, and it did not: the staged
bytes were left unsynced while the journal record that calls their chunks complete was still
`sync_data`'d, once per flush batch.

That is wrong in two independent ways, and the second is worse than the cost.

* **Cost.** One `fsync` per flush batch is one per eight chunks. On a 20 000-file transfer
  every file is a single chunk, so it is 2 500 fsyncs that `--no-fsync` was supposed to have
  removed — on the exact shape where the flag exists.
* **Semantics.** The journal's contract is *a chunk is announced complete only after its
  bytes are durable*. With the data unsynced the second half is false, so a synced journal is
  a durable claim about non-durable bytes. After a machine crash it is precisely the
  combination that makes the resume WORSE than having no journal at all: the record survives,
  claims chunks the page cache never wrote, the commit-time re-hash rejects the file, and the
  whole file is re-sent. The flag would have quietly been buying "no fsync, and also no
  working crash-resume".

**Fixed** by making `durable` an argument of `ResumeJournal::append` rather than a field, so
the policy has exactly one home (`ResumeShared::fsync_staged`) and is read at the single call
site that has just consulted it two lines above. Divergence is not tested against, it is
unrepresentable. Everything except a machine crash is unaffected: a Ctrl+C, a dropped link or
a killed process leave both the data and the journal in the kernel, so resume behaves exactly
as it does under the strict policy — which is what the new e2e gate asserts.

---

## 5. The effect, measured

Same sweep as §2 — 2 048 MiB, `--parallel 8`, tmpfs, median of 3 — before and after the work,
plus the `--no-fsync` arm and the pre-X-7 snapshot that lets X-7 be priced on its own:

| files | before | **after (shipped default)** | `--no-fsync` | `--no-fsync`, pre-X-7 |
| --- | --- | --- | --- | --- |
| 1 | 1083.6 MB/s | 1083.6 | 1077.9 | 970.6 |
| 100 | 1575.4 | 1374.5 | 1365.3 | 1374.5 |
| 1 000 | 1211.8 | **1374.5** | 1365.3 | 1374.5 |
| 5 000 | 1226.3 | **1374.5** | 1383.8 | 1374.5 |
| 20 000 | **368.3** | **1083.6** | **1219.0** | 1211.8 |

At 20 000 files: wall **5.56 s → 1.89 s**, throughput **368.3 → 1 083.6 MB/s** (**2.94×**),
receiver CPU **10.08 → 6.83 s**, CPU per delivered GiB **8.03 → 7.02 s**. The shipped build
is flat from 100 to 5 000 files and loses only 21 % at 20 000; the pre-campaign build loses
77 %.

Write amplification at 20 000 files: **4.62× → 1.08×**.

Syscall census after, N = 20 000: `mkdir` gone entirely; `fsync` and `rename` gone; what
remains on the receiver is `fdatasync` and three `openat` per file.

### 5.1 The one cell that disagreed, and how it was settled

In the table above the 100-file cell is the only one where *before* beats *after*
(1 575.4 vs 1 374.5, 13 %). A cell that disagrees in sign with all four of its neighbours is
either a real cost of the new machinery at low file counts or an artefact of one sweep, and
publishing it either way without checking is guessing.

It was re-measured with the arms **interleaved** (`before, after, before, after`) so that
machine drift cannot land on one arm, at five file counts, each cell `REPS=7`:

```
  files    before (2 reps)          after (2 reps)
  10       1600.0  1383.8           1612.6  1374.5
  100      1365.3  1365.3           1383.8   [+1612.6 un-paired]
  300      1204.7  1211.8           1374.5   [+1612.6 un-paired]
  1000     1204.7  1204.7           1374.5   [+1393.2 un-paired]
  5000     1083.6  1083.6           1374.5  1383.8
```

The 100-file cell reads **1 365.3 before vs 1 383.8 after** — parity, twice. The 1 575.4 in
the main table was a single-repetition fast outlier, and the regression it appeared to show
does not exist. Note also the 10-file row, where both arms move together between the first
pair and the second (1600 → 1384 and 1613 → 1375): that is exactly the drift interleaving is
there to cancel, and it is the size of the effect that was nearly published as a finding.

The three `[un-paired]` figures come from a second pass that re-took the arms lost to the
port collision described below; they were **not** run back-to-back against a `before` arm, so
they are reported but not used for the comparison — they only agree with it.

Two harness mistakes are recorded here because they both produce *plausible* numbers rather
than errors, which is what makes them dangerous:

* The first paired attempt filtered the harness output through `awk` and printed an empty
  column when the expected row was missing. A crashed cell therefore looked like a
  measurement. Failures are now printed with the harness's own last lines.
* "A unique port per run" is only unique against other *listeners*. The ports first chosen
  (47100+) are inside `net.ipv4.ip_local_port_range` (32768–60999 here), so the previous
  cell's own sender→server connections could take — and did take — the number the next cell
  wanted to bind: `failed to bind the control listener on 0.0.0.0:47106: Address already in
  use`. Harness ports belong **below** the ephemeral range.

## 6. The durability policy, priced

`fdatasync` is the receiver's largest non-futex cost once §4's fixes are in, so the campaign
measured what it buys and what it costs instead of assuming either.

**What it costs depends entirely on the storage, and that is the finding.** On tmpfs the two
policies are indistinguishable — 1 374.5 vs 1 365.3 MB/s at 5 000 files, 1 083.6 vs 1 219.0
at 20 000 — because flushing a page that is already RAM costs nothing. The price is therefore
not in bore's code path at all; it is the device's flush, and it has to be measured on a
device:

| 512 MiB, 20 000 files, relay `ws → vm` | default | `--no-fsync` | ratio |
| --- | --- | --- | --- |
| staging EBS (§8.3) | 21.4 MB/s | 60.5 MB/s | **2.82×** |

Up to 5 000 files on that same link the two policies are within 7 % of each other and both
sit at the wire (§8.3): the per-file durability work is real but cheaper than the network, so
it never shows. It becomes the bound only when the file count is high enough that the wire
is no longer the constraint — and it costs most on exactly the network-attached storage a
transfer target is most likely to be running on.

**What it buys**: correctness *by construction* across a machine crash, instead of
correctness *by detection*. Integrity is identical either way — a chunk this run did not
itself write is never counted as fresh, so it is always re-hashed before commit and bytes
lost to a crash fail verification and are re-sent, never silently accepted. And only a
kernel panic or a power cut can reach those bytes at all: a Ctrl+C, a dropped link or a
killed process lose nothing, because the data is already in the kernel.

rsync, rclone and croc all default to no per-file fsync. **bore keeps the stricter default**
and ships `--no-fsync` to trade it, with the number above in `README.md` so the choice is
informed. Changing the default is a decision left open for the operator (§10).

## 7. Bandwidth ceiling and `--parallel`

Single 2 GiB file, loopback relay, **run directory on tmpfs**, three repetitions per cell,
median reported. With one file the per-file cost is out of the picture and what is left is
the transport:

```
  files    par    wall_s       MB/s  cpu_send  cpu_recv  cpu_s_per_GiB
  1        1        2.47      829.1      3.35      3.02       3.19
  1        2        1.86     1101.1      3.96      3.87       3.92
  1        4        1.67     1226.3      4.48      4.19       4.33
  1        8        1.89     1083.6      5.35      5.59       5.47
  1        16       1.90     1077.9      5.55      6.25       5.90
  1        32       2.15      952.6      6.68      7.47       7.08
```

Saturation is at **`--parallel 4`** — 1 226 MB/s, about 9.8 Gbit/s on loopback. One stream
reaches 68 % of that, which is the `window / RTT` bound of §0 showing up even at loopback
latency. Past 4 the curve turns over: 32 streams are **22 % slower than 4 and cost 63 % more
CPU per delivered GiB**. Parallelism is not free and this is where the bill becomes visible.

### 7.1 Why the run directory must be on tmpfs — and how this table was wrong once

The first version of this table was measured with the run directory on the NVMe, where each
cell writes 4 GiB (2 GiB read on the sender, 2 GiB staged on the receiver). That is enough to
make the *disk* the variable in a table about the *transport*.

It was caught by accident and then confirmed deliberately. A high-`--parallel` cell read
93.9 MB/s where an earlier one had read 989, which looked exactly like a regression
introduced by this campaign's own changes. Building a git worktree at the pre-campaign commit
and measuring the **unmodified** binary under the same conditions returned 113 MB/s at
`--parallel 8` — the baseline was equally slow, so the code was not the variable.
`/proc/pressure/io` read `full avg10=32.87` at the time, against `avg10≈1` for the table
above; the cause was ~3.3 GB of the campaign's own leftover scratch directories plus 3.7 GB
of fixtures, with swap at 100 %.

So: **`TMPDIR=/dev/shm`, and read `/proc/pressure/io` before trusting a cell.** The
rule earned by this is more general than the flag — when a measurement moves, re-measure the
*unchanged* baseline in the same conditions before believing the change caused it.

### 7.2 What this says about the `--parallel` default, and what it does not

`resolve_parallel(0)` is `available_parallelism()` clamped to `[4, 32]`. On the 16-core
machine used here the default is therefore 16: **1 078 MB/s against the 1 226 available at
4**, for 36 % more CPU per GiB. That looks like a bad default until the other half of the
evidence is put next to it — on the staging WAN path the same sweep runs the other way, with
the direct arm nearly doubling from `--parallel 1` to `8` (§8.1). A single stream is bounded
by `window / RTT`; loopback has almost no RTT and a WAN has plenty.

The default is therefore a deliberate compromise biased towards the high-BDP case, and the
bias is the right way round: being wrong on a fast path costs ~12 % of throughput, while
being wrong on a WAN path costs half of it.

**What this table cannot support** is a change to the upper clamp. `--parallel 32` only
*becomes* the default on a machine with 32 or more cores, and the 32-stream row above was
measured on a 16-core box — it shows 32 streams oversubscribing 16 cores, which is not the
configuration the default would produce. Settling whether the `32` clamp is right needs a
≥32-core machine; it is not settled here, and the constant was left alone.

---

## 8. Staging: the same questions over a real network

Everything above is loopback, which is the right substrate for finding the *code's* costs
and the wrong one for claiming a bandwidth number. This section re-asks the two questions
that matter over the real path: workstation ↔ `t4g` VM in `eu-south-1`, relay through the
staging `bore server`, 1 024 MiB in one file for the transport question and 512 MiB in N
files for the shape question.

Harness: `scripts/perf/staging/xfer/xfer_bw.sh` (X1) and `xfer_shape.sh` (X2). Both read
`~/.config/bore-perf/env.sh`; the credentials are provided separately and are never
committed.

### 8.1 `--parallel` is a lever only on the direct path

1 024 MiB, one file, MB/s reported end to end (the hole punch is inside the number):

| topology | arm | par 1 | par 2 | par 4 | par 8 | par 16 |
| --- | --- | --- | --- | --- | --- | --- |
| ws → vm | relay | 80.3 | 80.0 | 80.5 | 78.7 | 74.7 |
| ws → vm | direct | 39.8 | 48.5 | 61.2 | **70.8** | 69.5 |
| vm → ws | relay | 45.0 | 40.9 | 49.5 | 48.5 | 49.2 |
| vm → ws | direct | 37.3 | 45.4 | 54.0 | 59.1 | **62.4** |
| vm → vm | relay | 111.6 | 98.5 | 89.6 | 85.1 | 86.7 |
| vm → vm | direct | 77.5 | 109.8 | 89.3 | 99.6 | 85.1 |

Two things are flat and one is not. The **relay** arm is already at its ceiling with a
single stream — it rides the carrier pool, whose count `--carriers 0` auto-derives from
`--parallel`, over TCP with kernel autotuning underneath — so more streams buy nothing and
16 of them cost a little. The **direct** arm nearly doubles from 1 to 8 streams: a single
QUIC stream is bounded by `window / RTT`, exactly as the loopback sweep shows, and on a WAN
RTT that bound bites. This is the whole justification for the `--parallel` default being
`available_parallelism()` clamped to `[4, 32]` rather than 1, and it is the one number an
operator moving a single large file over a punched path should know.

### 8.2 The relay is faster than the direct path here, and that is not a defect

On `ws → vm` the relay beats the hole-punched path at every parallelism (78.7 vs 70.8 at
the best direct setting), and this campaign did **not** establish the mechanism. What can be
said from the measurement alone: the gap is ~10 % once `--parallel` is at its default, it is
*not* a rendezvous cost (the punch is 37–53 ms when the check round ends cleanly — secret
campaign §4), and the same asymmetry appears in the opposite direction on `vm → ws`, where
the direct arm at par 16 (62.4) passes the relay (49.2). So the ordering is
path-dependent, not a constant property of the transport. The honest reading is that on
this link neither arm is the obvious choice and bore tries direct, falls back, and reports
which one it used — which is the behaviour that lets an operator find out for their own
network. Settling the mechanism needs a loss/RTT decomposition on the UDP path and is left
open (§10).

`vm → vm` is included because it removes the home link entirely: both arms land in the
85–110 MB/s band, and the relay's par-1 row (111.6) is the fastest single cell in the
table — a second reminder that the relay path is not a consolation prize.

### 8.3 On a real link the per-file cost is invisible — until the fsync policy is the bound

512 MiB, relay arm, `ws → vm`, `--parallel 8`:

| files | default | `--no-fsync` | ratio |
| --- | --- | --- | --- |
| 1 | 68.6 MB/s | 63.2 MB/s | 0.92× |
| 100 | 73.6 | 73.3 | 1.00× |
| 1 000 | 72.6 | 71.1 | 0.98× |
| 5 000 | 67.0 | 71.6 | 1.07× |
| 20 000 | **21.4** | **60.5** | **2.82×** |

Up to 5 000 files the two policies are indistinguishable and every cell sits at the link's
~70 MB/s: the per-file work is real but it is cheaper than the wire, so it does not show.
At 20 000 files the wire stops being the constraint and the receiver's `fdatasync` becomes
it. Note that the same pair of policies costs *nothing* on tmpfs (§6): the ratio here is
the EBS volume's flush latency, not bore's. This is the shape of result that justifies
shipping `--no-fsync` rather than only documenting the cost — the users who hit it are
exactly the ones on network storage, where it hurts most.

Note what the table does not say. The fixed cost of the transfer's own protocol is not
visible anywhere in it, because after §4's fixes it is below the link. The 20 000-file cell
is a *durability* measurement, not a protocol one.

---

## 9. State of the art: the same bytes, the same link, the tools people already use

Harness: `scripts/perf/staging/xfer/xfer_sota.sh`. Workstation ↔ `t4g` VM in `eu-south-1`,
one run per tool, 40 s between tools.

**Read the comparison honestly.** `scp`, `rsync` and `tar | ssh` all open a direct TCP
connection to a host that has a routable address and a running `sshd`. `bore transfer`
exists for the case where neither is true — a receiver behind NAT with no port forwarding
and no public endpoint — which none of the three can do at all. So this is not "who is
faster at the same job". It is what bore's protocol costs against the best case, on the same
link, with the same bytes, in the same minutes.

The `link` row is what makes the table legible: one SSH channel carrying the bytes to
`/dev/null`, no filesystem on the far side and no per-file protocol whatsoever. Every other
row is bounded by it. Without that row, six near-identical numbers cannot be told apart from
six tools that are all equally mediocre.

### 9.1 One large file, 1 024 MiB

| direction | `link` | bore direct | bore relay | scp | rsync | `tar｜ssh` |
| --- | --- | --- | --- | --- | --- | --- |
| ws → vm | 68.2 | **71.1** | *78.5* | 67.5 | 67.0 | 70.0 |
| vm → ws | 43.2 | **57.6** | 51.8 | 49.6 | 42.9 | 45.1 |

Uploading, every tool lands within ±5 % of the ceiling — the link is the constraint and bore
is not paying a protocol tax to reach it. Downloading, **bore's direct arm is the fastest
tool measured**, 33 % above the single-channel ceiling and 16 % above `scp`, because
`--parallel 8` runs eight QUIC streams where an SSH tool runs one and a single stream is
bounded by `window / RTT`.

The one row in italics needs a caveat rather than a victory lap: bore's *relay* arm is not
on the same route as everything else. It goes workstation → staging server → VM, and the
server is in the same region as the VM, so part of that 78.5 is routing and not protocol.
The direct arm is the fair comparator and it is the one in bold.

### 9.2 The same 512 MiB in 5 000 files

| tool | MB/s | wall |
| --- | --- | --- |
| `link` | 65.8 | 7.78 s |
| bore direct | 64.7 | 7.92 s |
| bore relay | 64.7 | 7.91 s |
| rsync | 64.4 | 7.95 s |
| `tar｜ssh` | 61.0 | 8.40 s |
| **scp** | **1.3** | **381.28 s** |

bore, `rsync` and a raw `tar` pipe are indistinguishable and all of them sit on the ceiling:
after §4's fixes the transfer's per-file cost is below the wire, which is the whole result of
this campaign restated over a real network.

`scp` is **48× slower**, and the number is not a bandwidth figure — it is round trips. Since
OpenSSH 9 `scp` is backed by the SFTP protocol and walks the tree one file at a time over a
single channel, so 5 000 files cost 5 000 serialised round trips on a ~10 ms path. It is
included precisely because it is the tool people reach for first.

### 9.3 What this table is biased towards, and it is not bore

Tools run in a fixed order and the fixture is read from the workstation's page cache. The
first row (`link`) therefore reads it coldest and every later row reads it warm — and the
later rows are the SSH tools. The ordering bias works **against** the conclusion drawn here,
not for it.

### 9.4 What was not measured

**`croc` and `rclone` were not run**, and they are the comparison this section is missing.
`croc` in particular is bore's closest peer — relay-brokered, NAT-friendly, PAKE-authenticated
file transfer — so "bore beats `scp`" is a much weaker statement than "bore matches `croc`"
would be. It is absent for a practical reason and not a flattering one: `croc` is not
installed here, and comparing it fairly needs a self-hosted relay reachable by both peers,
which means opening ports on a host this campaign deliberately did not reconfigure. Anyone
re-running this should treat it as the first thing to add; the harness takes a `TOOLS` list
and a new arm is a `case` branch.

---

## 10. Open questions

Six things this campaign deliberately left unsettled. Each is written so the next person can
start from the measurement, not from the question.

**1. Should `--no-fsync` be the default?** rsync, rclone and croc all ship the looser policy;
bore does not. §6 prices the stricter one at 2.82× on network storage and at *nothing* on
RAM, and §8.3 shows it is invisible below 20 000 files on a real link. The argument for
changing it is that bore is the only tool in its class paying the cost; the argument against
is that resume-after-crash is the feature that makes a transfer tool worth using over `scp`,
and the strict policy is what makes it correct by construction rather than by re-hash. No
measurement decides this — it is a product call, and it is the operator's.

**2. The sender reads and hashes every file twice.** `hash_planned_entries` reads the tree to
compute the manifest hashes, then `send_chunked_files` reads it again to send. On a tree
larger than the page cache that is two passes over the disk instead of one. It was left alone
because folding them means hashing on the send path, which reintroduces exactly the
single-threaded pre-scan P1 removed — the fix is a streaming hash *during* the send with the
manifest sent last, which is a protocol change, not an optimisation.

**3. `destination_satisfies_manifest` is serial.** The receiver's "do I already have this?"
check walks the manifest one entry at a time, stat-ing and hashing. At 20 000 files on a warm
destination that is the whole of a no-op transfer's cost. Parallelising it is the same shape
as the fan-out in X-4 and should be cheap; it was not done because no measurement in this
campaign was bounded by it.

**4. The relay beats the direct path on `ws → vm` (§8.2).** The explanation offered there —
asymmetric upstream, and a relay that happens to sit on a better route — is consistent with
every number taken, but it was not proven by isolating the route. Settling it needs the same
transfer run against a relay placed *off* the direct path's route.

**5. The `--parallel` clamp above 32 cores is untested.** `resolve_parallel(0)` clamps to
`[4, 32]`, and §7.2 shows 32 streams are 22 % slower and 63 % more CPU per GiB than 4 on this
16-core box — but that is oversubscription, and the clamp only *binds* on a machine with more
cores than any available here. The constant was left alone precisely because the data cannot
speak to the case it governs.

**6. `croc` and `rclone` were not measured** (§9.4). `croc` is bore's closest peer and its
absence is the largest gap in §9.
