# TRANSFER_UPGRADE_PLAN.md

**Audience:** Sonnet (the coding agent who will implement this).
**Author:** Opus (architecture analysis).
**Module:** `src/transfer_v2.rs` (wired as `pub mod transfer` in `src/lib.rs:33`; `src/transfer.rs` is **dead code**, ignore it).

## TL;DR — the one thing that matters

**PRIORITY #1 IS BANDWIDTH. Saturate the link.**

`bore transfer` is slow for one dominant reason: the file data path is a
**synchronous stop-and-wait protocol**. Each worker sends exactly one 256 KiB
chunk and then *blocks waiting for a `ChunkAck` round-trip* before sending the
next one (`send_chunked_files`, `src/transfer_v2.rs:736-787`). Per-worker
throughput is therefore hard-capped at:

```
throughput_per_worker ≈ CHUNK_SIZE / RTT = 256 KiB / RTT
```

At 20 ms RTT that is **~12.8 MB/s per worker**, *no matter how much bandwidth
exists*. And the default worker count is **1** (see "Defaults" below), so the
out-of-the-box transfer is capped at ~12 MB/s on a 20 ms link while the pipe
might be 1 Gbit/s+.

`bore test-udp --test-bandwidth` is fast precisely because it does the opposite:
it opens a stream and **blasts the whole quota in a tight 64 KiB write loop with
a single ack at the very end** (`src/udp_diagnostic.rs:716-764`). Same transport
(native QUIC stream, 64 MiB windows, BBR), completely different application
protocol. That contrast *is* the bug: the transport is fine, the transfer
framing throws the bandwidth away.

**The fix: make the transfer data path stream like the bandwidth test does** —
pipeline/stream chunks back-to-back, no per-chunk round-trip. Everything else in
this document is secondary I/O cleanup that matters once the protocol stops
self-throttling.

---

## How the system is wired (so the changes land in the right place)

### Transport (this part is good — do not rewrite it)

A `transfer sender` is a secret-tunnel **consumer** (`secret::Proxy`,
constructed in `run_sender`, `src/transfer_v2.rs:567`). A `transfer listener`
is a secret-tunnel **provider** (`Client::new_secret_provider`,
`src/transfer_v2.rs:495`). Both talk to the bore server, which rendezvouses them.

Two data paths exist (`DataPath` in `src/secret.rs`):

- **`Direct(DirectConn)`** — native QUIC, hole-punched, bypasses the server.
  Each application connection rides its **own QUIC bidi stream**
  (`conn.open_stream()`), with large flow-control windows
  (`DIRECT_QUIC_STREAM_RECEIVE_WINDOW` 16 MiB, `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW`
  64 MiB, `DIRECT_QUIC_SEND_WINDOW` 64 MiB) and BBR. This is the fast path and is
  what `test-udp` uses.
- **`Relay(CarrierPool)`** — yamux substreams over one or more TCP carriers to
  the server, which splices to the provider. Fallback path.

`Proxy::forward` (`src/secret.rs:~1635`) per application connection: opens a
stream, writes one `STREAM_READY` byte, flushes, then `copy_bidirectional_with_sizes`
with `PROXY_BUFFER_SIZE = 64 KiB`. **There are no per-write round-trips at the
transport layer.** All the latency penalty is in the transfer protocol on top.

### How `transfer` uses the transport (the worker model)

- `run_sender` binds a local loopback `Proxy` and `tokio::spawn(proxy.listen())`.
- `send_chunked_files` (`src/transfer_v2.rs:708`) spawns `parallel` worker tasks.
  **Each worker = one `connect_local()` TCP connection to the loopback proxy =
  one application connection = one data stream** (one QUIC bidi stream on the
  direct path, or one yamux substream on the relay path).
- Workers pull `ChunkTask`s off a shared `Arc<Mutex<VecDeque>>` (work-stealing).
- Receiver side: `receive_filesystem_streams` (`src/transfer_v2.rs:947`)
  pre-accepts exactly `expected_workers` loopback connections and runs
  `handle_worker_connection` (`src/transfer_v2.rs:1005`) per worker.

So **`--parallel` = number of concurrent application data streams** used to move
file chunks.

---

## `--parallel` vs `--carriers` — what they actually do

This is the second thing you asked to clarify. They operate at **different
layers** and are easy to confuse.

| | `--parallel N` | `--carriers N` |
|---|---|---|
| Layer | Application (transfer worker streams) | Transport (relay TCP connections) |
| Where | `sender` only (`src/transfer_v2.rs:708-802`) | `local`/`proxy`/`transfer` (`pool.rs`, `secret.rs`) |
| Effect | Opens N worker streams; chunks fan across them | Opens N TCP carriers to the **server**; substreams round-robin across them (`CarrierPool::pick`) |
| Direct UDP path | **This is the only knob that helps** — each worker = its own QUIC stream | **Does nothing.** The direct path bypasses the server; there is no relay leg to widen. |
| Relay path | N substreams... | ...but if `carriers=1` they all share **one TCP connection** → single congestion window + head-of-line blocking. `carriers>1` spreads them so each gets its own cwnd and loss isolation. |

**Consequences / current bugs in their interaction:**

1. **On the direct UDP path, `--carriers` is silently ignored.** A user tuning
   `--carriers` for a direct transfer changes nothing. Document this and/or warn.
2. **On the relay path, `--parallel` without matching `--carriers` is a trap:**
   e.g. `--parallel 8 --carriers 1` puts 8 yamux substreams on one TCP socket —
   one cwnd, mutual HOL blocking. You get concurrency in the app but not in the
   network.
3. The auto default couples them in a confusing way (next section).

**What to do (see Task 6):** decouple the user-facing meaning, make the direct
path scale on `--parallel` alone, and on the relay path auto-raise carriers to
track parallel (or clearly document that the user must).

---

## Defaults are wrong (quick win, do this regardless)

`plan_transfer` (`src/transfer_v2.rs:1542`):

```rust
let parallel = if options.parallel == 0 {
    options.carriers.clamp(1, DEFAULT_PARALLEL)   // DEFAULT_PARALLEL = 4
} else {
    options.parallel.clamp(1, MAX_PARALLEL)        // MAX_PARALLEL = 32
};
```

CLI default `carriers = 1` (`src/main.rs:611`) and `parallel = 0`
(`src/main.rs:616`). So the **default effective parallel is `1.clamp(1,4) = 1`**.
One worker. Stop-and-wait. ~12 MB/s @ 20 ms RTT.

`CHUNK_SIZE = 256 * 1024` (`src/transfer_v2.rs:33`) and
`DEFAULT_PARALLEL = 4` (`:34`) are both too small for a bandwidth-first tool.

---

# The plan (ordered by bandwidth impact)

Implement in this order. Tasks 1–3 are the bandwidth fixes. Tasks 4–6 are
amplifiers and cleanups. After each task, validate with the protocol in the
"Validation" section — use `test-udp --test-bandwidth` between the same two hosts
as the ceiling you are trying to reach.

## Task 1 — Kill the per-chunk stop-and-wait (THE fix)

**File:** `src/transfer_v2.rs`. **Functions:** `send_chunked_files`
(`:708`), `handle_worker_connection` (`:1005`), and the `Frame` enum (`:249`).

**Current (broken) per-worker loop** (`:736-787`): pop task → read chunk → hash →
send `ChunkStart` → `write_all(payload)` → **`expect_frame` waits for `ChunkAck`**
→ repeat. The wait is the killer.

**Target design: streaming with end-of-stream confirmation.**

The underlying transport (QUIC bidi / yamux / TLS-TCP) is **reliable and
ordered**. Per-chunk acks buy nothing for correctness — integrity is already
guaranteed by (a) per-chunk blake3 verified on the receiver
(`handle_worker_connection:1039`), (b) per-file `full_hash`, and (c) the final
`TransferSummary` hash compare (`receive_transfer:908`). So the sender does not
need to wait for anything mid-stream.

Rewrite the worker as **two decoupled halves**:

- **Sender worker** (`send_chunked_files`):
  1. `connect_local`, send `WorkerHello`.
  2. Loop: pop task, read chunk, hash, send `ChunkStart`, `write_all(payload)`
     **and immediately continue to the next task** (no read in the loop).
     Backpressure is provided naturally by the socket/QUIC send window — when the
     receiver is slow, `write_all` blocks, which is exactly right.
  3. After the queue drains, send `WorkerDone` and then **read exactly one**
     terminal frame: `WorkerComplete` (success) or `Error { message }`.
  4. Progress (`add_bytes`) advances as chunks are written, not on ack.

- **Receiver worker** (`handle_worker_connection`):
  1. Loop reading frames: on `ChunkStart`, read payload, verify hash, write,
     `mark_chunk_complete` — exactly as today **but do NOT send a `ChunkAck`.**
  2. On `WorkerDone`: `flush_pending()`, then send **one** `WorkerComplete`
     frame (or `Error`).

**Frame enum changes:** add `WorkerComplete`. You may keep `ChunkAck` defined for
wire-compat but it is no longer sent on the hot path. (This is a protocol change;
bump `PROTOCOL_VERSION` from `2` to `3` at `:29` so mismatched peers fail loudly
in `receive_transfer:868` rather than hang.)

**Why this alone fixes it:** the worker now behaves like the bandwidth test —
continuous writes, one ack at the end. A single worker can now fill the QUIC
send window (64 MiB) instead of one 256 KiB chunk per RTT. Expected result:
single-stream throughput jumps from `CHUNK_SIZE/RTT` to "as fast as BBR + windows
allow", i.e. into the same range as `test-udp --test-bandwidth`.

**Optional refinement (only if you want bounded sender memory / mid-stream
resume granularity):** instead of fully fire-and-forget, implement a **sliding
window** of `W` in-flight chunks per worker (drain acks on a separate task,
block the sender only when `in_flight >= W`). Set `W` so `W * CHUNK_SIZE >=
BDP` (e.g. `W = 64` ⇒ 16 MiB in flight, covers 1 Gbit/s × 130 ms). This keeps a
backpressure signal independent of socket buffering. **Prefer the simple
streaming version first**; add the window only if profiling shows unbounded
buffering is a problem. Do not reintroduce a per-chunk *blocking* wait.

**Cancellation-safety note:** `receive_filesystem_streams` has a comment
(`:961-963`) that `expect_frame` on the control stream is not cancel-safe. The
new terminal `WorkerComplete` is sent on the **worker** stream, not the control
stream, so this concern does not regress. Keep reading the `TransferSummary` on
the control stream only after all workers joined, as today (`:977`).

## Task 2 — Stop reopening the source file per chunk (sender I/O)

**File:** `src/transfer_v2.rs`. **Function:** `read_chunk_from_file` (`:2375`).

Today every chunk does `std::fs::File::open(path)` + `read_at` + drop, inside
`spawn_blocking`. For a multi-GB file that is thousands of `open`/`close`
syscalls, and the read is **serialized in front of the network write** (worker
reads, *then* sends — network idle during disk read).

**Change:**
- Open each source file **once per worker assignment** and reuse the fd for all
  its chunks (`read_at`/`seek_read` keep working on a shared `std::fs::File`).
  Since workers pull mixed tasks from one queue, either (a) cache the last-opened
  `(PathBuf, File)` in the worker and reopen only on file change, or (b) switch
  the work unit from "chunk" to "file range" so a worker owns a contiguous run of
  one file (better locality; see Task 6).
- **Overlap read and network**: prefetch the next chunk while the current one is
  being written. A small per-worker pipeline (read task → bounded channel →
  write task) removes the read-then-send stall. Once Task 1 makes writes
  continuous, this overlap is what keeps the disk and the NIC both busy.

## Task 3 — Stop reopening + `set_len` the staged file per chunk (receiver I/O)

**File:** `src/transfer_v2.rs`. **Function:** `write_chunk_to_file` (`:2389`).

Every received chunk does `OpenOptions::open` + **`file.set_len(size)`** +
`write_at` + drop, in `spawn_blocking`. The `set_len` is pure waste: the staged
file is already sized once in `prepare_regular_file` (`:1330`, called from
`prepare_stage_entries:1299`). Reopening per chunk is the same syscall storm as
Task 2 on the write side.

**Change:**
- Drop the per-chunk `set_len` entirely (file is pre-sized).
- Keep one open `File` per staged file (cache by `entry_id`, or per-worker
  last-file cache) and `write_at` into it; flush/`sync_data` on the existing
  batched cadence (`flush_pending`/`RESUME_FLUSH_EVERY_CHUNKS`, `:1916`).
- Consider doing the blake3 verify and the write concurrently rather than
  read-payload → hash → write strictly serial (`:1037-1059`).

## Task 4 — Reduce redundant hashing passes

Current hashing work per transfer:
- **Sender:** full-file `hash_file_sync` during scan (`scan_entry:1680`, fills
  `full_hash`) **plus** a blake3 of every chunk in the worker loop (`:748`).
- **Receiver:** blake3 of every chunk (`:1039`) **plus** a **full re-read +
  re-hash of every file** in `verify_summary` (`:1484`, `hash_file_async`).

That is up to **two full data passes on each side**. blake3 is fast (~GB/s/core)
but on a 10 GbE / fast-NVMe setup, or on a CPU-bound host, this caps throughput
and steals cores from the transfer.

**Changes (bandwidth-first, keep integrity):**
- **Receiver:** when every chunk hash already verified during receipt, **skip the
  final full-file re-hash** in `verify_summary`. The per-chunk hashes + ordered
  writes already prove the file content; recompute the *summary* hash from the
  manifest's `full_hash` values (which the sender computed and sent), not by
  re-reading staged files. Only fall back to a full re-hash if a chunk hash was
  ever missing (e.g. resumed-from-disk chunks whose hash wasn't checked this run).
- **Sender:** the per-chunk hash and the scan-time `full_hash` are redundant
  *information* (chunk hashes compose to the file). Keep per-chunk hashing (it is
  what the receiver verifies incrementally) but do it **off the write path**
  (in the read/prefetch task of Task 2, or via `spawn_blocking`), so hashing
  never stalls the socket. Avoid hashing the same bytes twice on the sender:
  derive `full_hash` for the manifest from a streaming hash during scan only
  (already the case) and do **not** also re-hash whole files anywhere else.
- Make hashing optional/tiered later if needed (`--verify=full|chunk|none`), but
  **do not** ship `none` as default — integrity is a feature here.

## Task 5 — Tune chunk size, buffers, and windows for bulk

- `CHUNK_SIZE` (`:33`): 256 KiB is fine as a *resume/hash granularity* but it is
  also the framing unit. Once streaming (Task 1), raise it to **1–4 MiB** to cut
  the number of JSON `ChunkStart` frames and `write_all` calls by 4–16×. Keep it
  a power of two and a multiple of the resume granularity. (If you adopt
  file-range work units in Task 6, chunk size becomes hashing granularity only.)
- The transfer payload `write_all` already hands large buffers to the transport;
  the relevant socket/QUIC windows are set in `holepunch::transport_config`
  (16/64/64 MiB) — leave them, they are already bandwidth-class.
- `PROXY_BUFFER_SIZE` (`src/shared.rs:36`, 64 KiB) governs the loopback
  `copy_bidirectional`. With streaming this is usually fine, but verify it is not
  the limiter on very high-BDP links; if it is, raise it (it costs memory per
  connection).

## Task 6 — Fix `--parallel` / `--carriers` semantics and defaults

- **Defaults:** change effective default `parallel` to something bandwidth-class.
  Recommend `parallel` default = `min(MAX_PARALLEL, max(4, num_cpus))` when the
  user passes `0`, and **decouple it from `carriers`** (the current
  `carriers.clamp(1,4)` coupling at `:1543` is surprising). Raise `DEFAULT_PARALLEL`
  accordingly.
- **Direct path:** make sure `parallel > 1` opens that many QUIC streams (it
  already does — each worker connection → one `conn.open_stream()`), and confirm
  `MAX_DIRECT_STREAMS` (4096) is not the bound. After Task 1, even `parallel=1`
  should be fast on the direct path; multiple streams then add loss-isolation and
  multi-core hashing headroom.
- **Relay path:** when `parallel > carriers`, **auto-raise carriers** to
  `min(parallel, max_carriers)` (or at least log a warning) so worker substreams
  do not all pile onto one TCP connection (single cwnd + HOL). Otherwise relay
  parallelism is an illusion.
- **Docs/UX:** update `--parallel` / `--carriers` help text (`src/main.rs:610-617`,
  `:154-160`, `:244-250`) and `CLAUDE.md` to state plainly: *parallel = data
  streams; carriers = relay TCP connections, ignored on the direct UDP path.*

---

## Validation (mandatory — this is a bandwidth change, prove it)

1. **Establish the ceiling.** Between hosts A and B run
   `bore test-udp --tcp-secret-id <id> --test-bandwidth --test-transfer-quota 1GiB`
   on both ends. Record MB/s. This is the transport's real bandwidth; the
   transfer should approach it on the direct path.
2. **Baseline before changes.** Transfer a single large file (e.g. 2–4 GiB of
   incompressible data, `head -c 4G /dev/urandom > big.bin`) with default flags
   and with `--parallel 8`. Record MB/s from the progress line (`render_progress`,
   `:2685`) — it already prints `speed`. Confirm it is far below the test-udp
   ceiling.
3. **After each task**, rerun step 2. Task 1 should produce the large jump.
4. **Correctness gates (must stay green):** the full `tests/transfer_test.rs` and
   `tests/transfer_stdin_cli_test.rs` suites — they cover relay/direct/fallback,
   resume, TLS, collisions, non-UTF8 names, size boundaries, multi-frame
   manifests, NAT flags, and listener-kill resume/cleanup. These bind the fixed
   control port (7835) and **must run serially** (`SERIAL_GUARD`); do not
   parallelize them.
5. **Resume still works:** interrupt mid-transfer (the
   `BORE_TRANSFER_TEST_MAX_ACKED_CHUNKS` injection hook, `:2664`, will need
   renaming/adapting since acks are gone — make it "max chunks written" on the
   receiver or "max chunks sent" on the sender) and confirm restart resumes from
   receiver-tracked state, since the sender no longer drives completion via acks.
6. **Add a regression/throughput test** (gated, not in the default CI gate if it
   is timing-sensitive) that asserts a large transfer over a loopback/relay
   completes in roughly `bytes / expected_floor` time, to catch a future
   reintroduction of stop-and-wait.
7. `cargo fmt -- --check`, `cargo clippy --all-features --all-targets -- -D warnings`,
   `cargo test` all green (CI gates, per `CLAUDE.md`).

## Hard constraints (do not break)

- `#![forbid(unsafe_code)]` — no `setsockopt`/raw syscalls.
- `STREAM_READY` marker and "client sends `Hello` before auth" invariants are in
  the transport, untouched by this work — leave them.
- Half-close / EOF propagation must keep working (relevant if you touch any
  splice/copy logic).
- Integrity must remain end-to-end verified (per-chunk + final summary). Faster,
  not weaker.
- Bump `PROTOCOL_VERSION` for any wire change so old/new peers fail cleanly
  instead of hanging.

## Expected outcome

After Task 1 alone, a single-stream direct-UDP transfer of a large file should
move from `~CHUNK_SIZE/RTT` (tens of MB/s) to within the same order of magnitude
as `test-udp --test-bandwidth` on the same link. Tasks 2–5 close the remaining
gap (disk syscall storms, hashing stalls, framing overhead). Task 6 makes the
defaults and the relay path live up to it. **Target: file transfer throughput
within ~10–20% of the `test-udp --test-bandwidth` ceiling on the direct path.**
