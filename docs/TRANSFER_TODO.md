# TRANSFER_TODO.md — Production-Grade Hardening of `bore transfer`

> **Audience:** Sonnet 4.6 (implementer).
> **Author of architecture/decisions:** Opus 4.8 (this document is the contract).
>
> **Rules for the implementer:**
> - Do **not** make architectural choices. Every design decision is already fixed below.
>   If you hit an ambiguity not covered here, **stop and surface it** — do not improvise.
> - Work **phase by phase, sub-phase by sub-phase**, in order. Do not start Phase N+1 until
>   Phase N is fully green.
> - For every sub-phase: write tests first/alongside, then run the full CI gate
>   (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`).
> - **Zero regressions.** A sub-phase that breaks an existing passing test is not done.
> - Update documentation in the same sub-phase that changes behaviour (Phase 6 consolidates,
>   but flags/behaviours you add in earlier phases must be documented as you go too).
> - Keep `carriers<=1` / single-stream paths byte-for-byte unchanged where the plan says so.

---

## 0. Baseline state (read before touching anything)

- Feature lives in `src/transfer.rs` (~3816 lines), dispatched from `src/main.rs`
  (`TransferCommand::Listener` / `TransferCommand::Sender`).
- Tests: `tests/transfer_test.rs` (35), `tests/transfer_stdin_cli_test.rs` (15), in-module
  `#[cfg(test)]` units (~36). Docs: `README.md` transfer section, `docs/TRANSFER_FEASIBILITY.md`,
  `docs/TRANSFER_TEST_MATRIX.md`, `docs/TRANSFER_UPGRADE_PLAN.md`.
- **Known pre-existing flaky/failing tests (NOT introduced by you, do not "fix" by hiding):**
  `admin_test`, `secret_test`, and stdin-injected `receiver_ask_confirm_*`. Confirm the
  baseline with `git stash` if a failure looks new; the bar is "no NEW regressions".

### Protocol recap (do not change the happy path semantics)

JSON `Frame`s, `u32`-LE length prefix, `FRAME_LIMIT` = 16 MiB, `PROTOCOL_VERSION` = 3.
Raw chunk/stream bytes follow their describing frame **un-framed** (length carried in the
frame). Data path is **pipelined** (no per-chunk ACK).

```
SENDER (control conn)                         RECEIVER (listener)
  Begin ───────────────────────────────────────▶ parse, version check
  ManifestChunk* / ManifestDone ───────────────▶ validate, build resume plan
                                                  [--ask-confirm gate; stdin auto-accepts]
  recv_manifest_accepted ◀──── ManifestAccepted{final_name,parallel,resumed_bytes,resume}
  build_chunk_tasks(resume)                       expected_worker_connections(plan)
  if tasks.empty → 0 workers   ── SYMMETRIC ──    if pending==0 → expect 0 workers
  per worker(N): WorkerHello{id} ─▶               accept N worker conns (one mpsc), validate id
     ChunkStart{id,idx,off,len,blake3}+bytes ─▶   verify blake3, seek+write@off, mark complete
     WorkerDone ─▶                                flush, ◀── WorkerComplete
  TransferSummary ─────────────────────────────▶ verify_summary (re-hash only resumed files)
  expect Completed ◀──────────────────────────── commit_stage (atomic rename + rollback)
                                                  ◀── Completed{final_path,hash}; rm stage+state
STDIN: …ManifestAccepted → StreamChunk*+bytes → StreamEnd{hash} → StreamVerified (non-resumable)
```

### What is already correct (do not "improve" / do not regress)
- No deadlock on empty/all-resumed transfers (`expected_worker_connections==0` matches
  sender skipping `send_chunked_files`).
- Durability ordering: `flush_pending` fsyncs data (`sync_staged_files`) **before**
  `persist_resume_state` (atomic tmp+rename). Keep this order.
- `reset_file` persists the reset. Keep it.
- Pipelined data path (no `ChunkAck` wait). Keep it.
- `all_chunks_fresh` re-hash-skip optimisation. Keep it.

---

## Findings → fixes (the 11 items this document implements)

| ID | Sev | Fix in phase |
|----|-----|--------------|
| F1 | HIGH | P1.1 — apply `tune_tcp` to all transfer sockets |
| F2 | HIGH | P1.2 — validate peer-controlled payload lengths/offsets |
| F7 | LOW | P1.3 — delete dead `ChunkAck` variant |
| F3 | HIGH | P2.1 — idempotent committed-marker re-completion |
| F5 | MED | P2.2 — clean stdin temp stage dir on failure |
| F6 | MED | P3.1 — receiver `--confirm-timeout` |
| F10 | LOW | P3.2 — `--stall-timeout` on data reads |
| F4 | MED | P4.1 — drain-until-empty + documented single-sender persistent mode |
| F8 | LOW | P5.1 — centralise/guard test seams |
| F9 | LOW | P5.2 — sender file-count seeding on resume |
| F11 | DOC | P6 — docs + test matrix |

---

# PHASE 1 — Invariants & input hardening (NO behaviour change)

## P1.1 — Apply `tune_tcp` to every transfer socket (F1)

**Why:** CLAUDE.md invariant: `shared::tune_tcp` (TCP_NODELAY + SO_KEEPALIVE 15 s) must be
applied to every new socket. `transfer.rs` applies it to none. The control channel
ping-pongs tiny frames; without `TCP_NODELAY`, Nagle + delayed-ACK can inject ~40 ms stalls.

**Code:**
1. Add import: in the `use crate::shared::{…}` group at the top of `src/transfer.rs`, add
   `tune_tcp`.
2. In `connect_local` (`src/transfer.rs:2564`), after a successful `TcpStream::connect`,
   call `tune_tcp(&stream)` before returning `Ok(stream)`. This covers the sender control
   socket (`:704`) and all worker sockets (`:843`).
3. In the listener accept task (`src/transfer.rs:537-545`), after `internal.accept()`
   yields `(stream, _)`, call `tune_tcp(&stream)` before `conn_tx.send(stream)`. This covers
   the receiver-side control + worker sockets.

**Tests** (`tests/transfer_test.rs`, new):
- `transfer_sockets_have_nodelay` — start a listener + sender on relay, and assert the
  transfer completes (the existing `transfer_single_file_over_relay` already exercises the
  path). Add a focused unit test in `src/transfer.rs` `#[cfg(test)]`:
  `connect_local_sets_nodelay` — bind a `TcpListener` on loopback, `connect_local` to it,
  accept the peer, assert `stream.nodelay().unwrap() == true` on the connected socket.

**Gate:** fmt + clippy + `cargo test`. No regressions.

## P1.2 — Validate peer-controlled payload lengths & offsets (F2)

**Why:** Receiver does `vec![0u8; len as usize]` with `len: u32` taken verbatim from the
peer (`:1216` ChunkStart, `:1307` StreamChunk). A buggy/hostile sender can request up to
4 GiB per allocation → OOM/DoS. No bound today.

**Code (receiver side, `handle_worker_connection`, around `:1201-1217`):**
Before allocating `payload`, validate the `ChunkStart` fields against the known manifest
entry (`entry` is already fetched at `:1208`):
```rust
let size = entry.size.context("regular file manifest entry missing size")?;
let expected_chunks = entry.chunk_count;
if chunk_index >= expected_chunks {
    bail!("chunk index {chunk_index} out of range ({expected_chunks}) for {}", display_rel_path(&entry.rel_path));
}
let expected_off = chunk_index as u64 * CHUNK_SIZE as u64;
let expected_len = chunk_len(size, chunk_index);
if offset != expected_off || len as u64 != expected_len {
    bail!("chunk geometry mismatch for {} chunk {chunk_index}: got off={offset} len={len}, expected off={expected_off} len={expected_len}", display_rel_path(&entry.rel_path));
}
// len is now provably <= CHUNK_SIZE.
```
This makes the allocation bounded by `CHUNK_SIZE` (1 MiB) and also catches protocol desync.
Put the validation **before** `stream.read_exact`.

**Code (receiver side, `receive_stdin_stream`, `:1306`):**
```rust
Frame::StreamChunk { len } => {
    if len == 0 || len as usize > STREAM_CHUNK_MAX {
        bail!("stdin stream chunk length {len} out of bounds (max {STREAM_CHUNK_MAX})");
    }
    let mut buf = vec![0u8; len as usize];
    …
}
```
Add near the other consts (top of file, by `CHUNK_SIZE`/`COPY_BUFFER`):
```rust
/// Upper bound on a single stdin StreamChunk payload the receiver will allocate.
/// The sender uses COPY_BUFFER (64 KiB); allow up to CHUNK_SIZE for headroom.
const STREAM_CHUNK_MAX: usize = CHUNK_SIZE;
```
On any violation the surrounding `receive_transfer` already sends a `Frame::Error` to the
sender via the outer error arm (`:1113`), so the sender gets a clear `peer reported an
error: …`. Do not add a second error frame.

**Tests** (`tests/transfer_test.rs`, new — drive the receiver functions directly or via a
crafted in-memory peer; prefer a small protocol-level unit test in `#[cfg(test)]` that feeds
hand-built frames into `handle_worker_connection` over a `tokio::io::duplex` pair):
- `chunk_start_oversized_len_rejected` — feed `ChunkStart{len = CHUNK_SIZE*4, …}`; assert the
  worker returns an error mentioning "geometry mismatch" (or out of range) and never panics
  / never allocates the huge buffer (the validation rejects before allocation).
- `chunk_start_offset_mismatch_rejected` — correct len, wrong offset → error.
- `chunk_start_index_out_of_range_rejected`.
- `stdin_stream_chunk_oversized_rejected` — `StreamChunk{len = STREAM_CHUNK_MAX+1}` → error.

> Note: if wiring `handle_worker_connection` to a `duplex` is awkward because of the
> `ResumeShared` setup, instead factor the geometry check into a small pure helper
> `fn validate_chunk_geometry(entry: &ManifestEntry, chunk_index: u32, offset: u64, len: u32) -> Result<()>`
> and unit-test that helper directly. **This is allowed and preferred** — it keeps the
> validation testable without a full socket harness.

**Gate:** fmt + clippy + test. No regressions.

## P1.3 — Remove dead `ChunkAck` variant (F7)

**Why:** `Frame::ChunkAck { entry_id, chunk_index }` is never sent and never awaited (the
data path is pipelined). Dead protocol surface.

**Code:** delete the `ChunkAck { … }` arm from the `Frame` enum (`src/transfer.rs:260-304`
region). Run clippy; remove any now-unreachable match arms the compiler flags (there should
be none beyond exhaustive `match` wildcards). **Do not** bump `PROTOCOL_VERSION` — removing
an unused variant does not change the wire format of any frame that is actually sent.

**Tests:** none new; the suite compiling + passing is the proof. Confirm `grep -n ChunkAck
src/` returns nothing after the change.

**Gate:** fmt + clippy + test.

---

# PHASE 2 — End-state coherence

## P2.1 — Idempotent committed-marker re-completion (F3)

**Why:** `receive_transfer` commits (moves files to final, deletes the resume-state dir) at
`:1077` **before** sending `Completed` at `:1078`. If the control link drops in that window:
the receiver succeeds, but the sender's `expect_frame` (`:798`) sees EOF → hard error. A
retry finds no resume state → fresh transfer → `CollisionPolicy::Fail` → false "destination
already exists", even though the data on disk is correct and verified.

**Decision (fixed): leave a small committed marker so a retry is idempotent.**

### Data types
Add near `ResumeState` (`:245`):
```rust
const COMMITTED_MARKER_FILE: &str = "committed.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CommittedMarker {
    protocol_version: u32,
    transfer_id: String,
    manifest_hash: String,
    final_name: String,
    final_path: String,     // encode_native_path(final_path)
    total_bytes: u64,
    regular_files: u64,
    transfer_hash: String,
}
```

### `commit_stage` change (`:1717`)
At the **very end** of a successful single-source **and** multi-source commit, instead of
`remove_dir_all(stage_dir)`, do:
1. Remove the staged tree and `state.json` (everything under `stage_dir` **except** the
   marker you are about to write). Simplest: remove `stage_root` and the `state.json` file
   explicitly, keep the `stage_dir` directory itself.
2. Write `stage_dir/committed.json` atomically (reuse the tmp+rename pattern from
   `persist_resume_state`; factor a generic `write_json_atomic(path, &value)` helper and use
   it for both).

`commit_stage` needs the summary data for the marker. Pass the already-computed
`local_summary` (a `TransferSummary`) and `plan.final_path`/`plan.final_name` into
`commit_stage` (extend its signature: `commit_stage(plan, collision, summary: &TransferSummary)`).

> Multi-source note: `final_path` is `dest_root` itself for multi-source. The marker still
> lives in `stage_dir` (the hidden `.bore-transfer-state-<digest>` dir under dest_root), so
> it is keyed by transfer-id as usual. Record `final_path = encode_native_path(dest_root)`.

### `receive_manifest` change (`:1393`)
Before the `state_file` existence check, check for the marker:
```rust
let marker_file = stage_dir.join(COMMITTED_MARKER_FILE);
if fs::try_exists(&marker_file).await? {
    let marker: CommittedMarker = read_json(&marker_file).await?;
    if marker.protocol_version == PROTOCOL_VERSION
        && marker.transfer_id == begin.transfer_id
        && marker.manifest_hash == manifest_hash
    {
        // Return a plan flagged already-committed, carrying the stored CompletedFrame.
        return Ok(ReceiverPlan { /* … */ already_committed: Some(marker), resume: None, … });
    }
    // Mismatched marker (different content under same id): treat as stale, fall through and
    // start fresh (remove the marker first so a new state.json can be written).
    let _ = fs::remove_file(&marker_file).await;
}
```
Add field `already_committed: Option<CommittedMarker>` to `ReceiverPlan` (default `None`
everywhere it is constructed — stdin path, fresh path, resume path).

### `receive_transfer` change (`:988`)
Right after `receive_manifest` returns and **before** the `--ask-confirm` block, short-circuit:
```rust
if let Some(marker) = &plan.already_committed {
    // Idempotent re-completion: the data is already committed and verified.
    // Reply on the normal protocol so the sender finishes cleanly.
    send_frame(&mut control, &Frame::ManifestAccepted {
        final_name: marker.final_name.clone(),
        parallel: 1,
        resumed_bytes: marker.total_bytes,
        resume: all_chunks_complete_plan(&plan.entries), // every regular-file chunk marked done
    }).await?;
    // Sender computes 0 tasks → opens 0 workers → sends TransferSummary.
    match expect_frame(&mut control).await? {
        Frame::TransferSummary(summary) => {
            if summary.transfer_hash != marker.transfer_hash {
                bail!("re-sent transfer hash does not match the committed copy");
            }
        }
        other => bail!("unexpected frame during idempotent re-completion: {other:?}"),
    }
    send_frame(&mut control, &Frame::Completed(CompletedFrame {
        final_path: marker.final_path.clone(),
        total_bytes: marker.total_bytes,
        regular_files: marker.regular_files,
        transfer_hash: marker.transfer_hash.clone(),
    })).await?;
    let _ = control.shutdown().await;
    return Ok(TransferOutcome { /* from marker */ });
}
```
Add helper:
```rust
fn all_chunks_complete_plan(entries: &[ManifestEntry]) -> Vec<ResumeFilePlan> {
    entries.iter().filter(|e| e.kind.is_regular_file()).map(|e| ResumeFilePlan {
        entry_id: e.id,
        completed_chunks: (0..e.chunk_count).collect(),
    }).collect()
}
```
This reuses the **existing** empty-task path on the sender (it already sends 0 workers when
`build_chunk_tasks` is empty), so no sender change is required for the happy idempotent case.

### Sender clarity (the residual window, e.g. marker GC'd manually)
In `send_transfer` (`:798`), when `expect_frame` returns the EOF error after the summary was
sent, wrap it with a clearer message. Concretely, replace the bare `expect_frame(&mut
control).await?` for the `Completed` step with a match that maps the EOF case:
```rust
let completed = match recv_frame(&mut control).await {
    Ok(Some(Frame::Completed(done))) => done,
    Ok(Some(Frame::Error { message })) => bail!("peer reported an error: {message}"),
    Ok(Some(other)) => bail!("unexpected final frame from receiver: {other:?}"),
    Ok(None) => bail!(
        "transfer data fully sent, but the listener closed before confirming completion. \
         The destination may already be complete — re-run the same command to confirm \
         (it will re-verify idempotently)."
    ),
    Err(err) => return Err(err).context("failed reading final completion frame"),
};
```

**Tests** (`tests/transfer_test.rs` + possibly `tests/transfer_stdin_cli_test.rs` subprocess):
- `transfer_idempotent_recompletion_after_commit` — complete a transfer; assert
  `committed.json` exists in the state dir; re-run the **same** sender command to the same
  dest with the same transfer-id; assert it succeeds (no false collision) and the file is
  unchanged (hash equal).
- `transfer_committed_marker_mismatch_starts_fresh` — write a marker with a different
  `manifest_hash`; run a transfer with the real manifest; assert it ignores the stale marker
  and completes normally.
- (Optional, harder) `transfer_drop_control_before_completed_then_retry` — use the existing
  subprocess listener-kill pattern in `transfer_stdin_cli_test.rs` to drop the control conn
  in the commit→Completed window; assert retry idempotently succeeds. If reliably injecting
  that exact window is infeasible, document it as a manual test in the matrix instead of a
  flaky automated one (**do not** add a flaky test).

**Docs:** note in README/`docs/TRANSFER.md` that a successful transfer leaves a small
`<dest>/.bore-transfer-state-<digest>/committed.json` breadcrumb enabling idempotent re-runs,
and that it is safe to delete.

**Gate:** fmt + clippy + test. No regressions.

## P2.2 — Clean stdin temp stage dir on failure (F5)

**Why:** stdin uses `temp_stage_dir` (`:1369`) with a fresh UUID; it is non-resumable. On a
mid-stream failure the dir is never removed (only the ask-confirm-reject path cleans up), so
each failed/interrupted stdin transfer leaks a `.bore-transfer-<id>-<uuid>` dir under dest.

**Code (`receive_transfer`, error arm `:1113`):** the inner `async { … }.await` builds
`plan` inside the block, so `stage_dir` is not visible in the outer error arm. Restructure
minimally: lift the cleanup into the inner block, OR return the `stage_dir` for cleanup.
Simplest: inside the inner block, wrap the post-manifest body so that on `Err` for a stdin
source it removes `plan.stage_dir`. Concretely, after `receive_manifest` succeeds, capture
`let cleanup_dir = (plan.begin.root_source == RootSourceKind::Stdin).then(|| plan.stage_dir.clone());`
and in the outer arm:
```rust
if let Err(err) = &outcome {
    let _ = send_frame(&mut control, &Frame::Error { message: err.to_string() }).await;
    if let Some(dir) = &cleanup_dir { let _ = tokio::fs::remove_dir_all(dir).await; }
}
```
You will need to move `cleanup_dir` out of the inner async (e.g. compute it inside the inner
block but store into a `mut` captured before the block, or change the inner block to return
`(TransferOutcome, Option<PathBuf>)`-style). Pick the least invasive shape that keeps the
existing control flow; **do not** change filesystem-transfer cleanup (those keep their state
dir for resume).

**Tests** (`tests/transfer_stdin_cli_test.rs`, subprocess):
- `transfer_stdin_failure_cleans_temp_dir` — start a stdin transfer that fails mid-stream
  (e.g. kill the listener subprocess after some bytes), then assert no `.bore-transfer-*`
  temp dir remains under dest. (Reuse the kill pattern from
  `transfer_filesystem_listener_kill_resumes_and_cleans_up_cli`.)

**Gate:** fmt + clippy + test. No regressions.

---

# PHASE 3 — Liveness / timeouts

## P3.1 — Receiver `--confirm-timeout` (F6)

**Why:** with `--ask-confirm`, the receiver blocks on the tty indefinitely while the sender
sits in `recv_manifest_accepted` (`:761`) with no timeout. An away operator hangs the sender
forever.

**Code:**
1. CLI (`src/main.rs`, `TransferCommand::Listener`): add
   ```rust
   /// Seconds to wait for --ask-confirm input before rejecting (0 = wait forever).
   #[clap(long, default_value_t = 120)]
   confirm_timeout: u64,
   ```
   Thread it into `TransferListenerOptions` → `ListenerOptions.confirm_timeout: u64`.
2. `receive_transfer` signature gains `confirm_timeout: u64`; pass from `run_listener`.
3. The confirmation runs in `spawn_blocking`. Wrap it:
   ```rust
   let confirm_fut = spawn_blocking(move || display_and_confirm_manifest_sync(...));
   let accepted = if confirm_timeout == 0 {
       confirm_fut.await.context("manifest confirmation task failed")??
   } else {
       match tokio::time::timeout(Duration::from_secs(confirm_timeout), confirm_fut).await {
           Ok(joined) => joined.context("manifest confirmation task failed")??,
           Err(_) => {
               let _ = send_frame(&mut control, &Frame::Error {
                   message: "transfer rejected by receiver (confirmation timed out)".into(),
               }).await;
               let _ = tokio::fs::remove_dir_all(&plan.stage_dir).await;
               bail!("transfer confirmation timed out after {confirm_timeout}s");
           }
       }
   };
   ```
   > Caveat: `tokio::time::timeout` on a `spawn_blocking` join handle does not cancel the
   > blocking read itself (the OS read on `/dev/tty` keeps running on the blocking pool), but
   > it **does** unblock the async task so the receiver sends the reject frame and frees the
   > sender. That is the goal. Document this limitation in a code comment.
   Only applies when `ask_confirm` is true and source is not stdin; for the auto-accept paths
   the timeout is irrelevant (returns immediately).

**Tests** (`tests/transfer_test.rs`):
- `transfer_confirm_timeout_rejects` — listener with `ask_confirm=true`,
  `confirm_timeout=1`, no `BORE_TEST_CONFIRM_RESPONSE` set (so the blocking read would
  block); assert the sender gets a clear "rejected … timed out" error within a few seconds
  and the stage dir is cleaned. Use the direct `run_listener`/`run_sender` harness; ensure
  the test does not itself hang (wrap in a generous `tokio::time::timeout`).

**Docs:** document the flag + default.

**Gate:** fmt + clippy + test. No regressions.

## P3.2 — `--stall-timeout` on data reads (F10)

**Why:** no application-level timeout on `read_exact` of chunk payloads (`:1217`) or on
active-transfer control reads. A peer that stops sending mid-chunk hangs the transfer
indefinitely. (Tunnel `SO_KEEPALIVE` detects peer *death*, not application *stall*.)

**Code:**
1. CLI: add to **both** Listener and Sender:
   ```rust
   /// Abort if no transfer data is received/sent for this many seconds (0 = disabled).
   #[clap(long, default_value_t = 60)]
   stall_timeout: u64,
   ```
   Thread into both options structs.
2. Add a small helper in `transfer.rs`:
   ```rust
   async fn with_stall<T>(secs: u64, fut: impl Future<Output = Result<T>>) -> Result<T> {
       if secs == 0 { return fut.await; }
       match tokio::time::timeout(Duration::from_secs(secs), fut).await {
           Ok(r) => r,
           Err(_) => bail!("transfer stalled: no progress for {secs}s"),
       }
   }
   ```
3. Wrap the **blocking data reads/writes** with it:
   - Receiver `handle_worker_connection`: wrap `recv_frame(&mut stream)` (the per-iteration
     frame read) and the `stream.read_exact(&mut payload)` in `with_stall(stall_timeout, …)`.
   - Receiver `receive_stdin_stream`: wrap `expect_frame(control)` and `read_exact`.
   - Sender `send_chunked_files` worker: wrap `stream.write_all(&chunk)` and the terminal
     `expect_frame`. Sender `send_stdin_stream`: wrap the `write_all`.
   Pass `stall_timeout` down through the call chain (`send_transfer`/`send_chunked_files`,
   `receive_filesystem_streams`/`handle_worker_connection`).
   > **Cancellation-safety:** the existing comment at `:1139` warns `expect_frame` is not
   > cancellation-safe inside `select!`. `tokio::time::timeout` cancels the future on
   > expiry, which on timeout **drops** the partially-read frame and aborts the whole
   > connection (we `bail!`), so the stream is discarded — this is safe because we never
   > resume reading that stream after a stall. Add a comment making this explicit. Do **not**
   > wrap reads that are expected to legitimately block for a long time without being a
   > stall (e.g. the listener's `conn_rx.recv()` waiting for the next sender — leave that
   > alone).

**Tests** (`tests/transfer_test.rs`, protocol-level via `duplex` or a stub peer):
- `transfer_stall_timeout_aborts` — build a peer that sends `ChunkStart` then never sends the
  payload bytes; with `stall_timeout=1`, assert the receiver worker aborts with a "stalled"
  error within ~2 s. Keep the test bounded by an outer `tokio::time::timeout`.
- `transfer_stall_timeout_zero_disables` — assert `with_stall(0, fut)` is a pass-through
  (unit test on the helper).

**Docs:** document the flag + default on both sides.

**Gate:** fmt + clippy + test. No regressions.

---

# PHASE 4 — Persistent-mode robustness (F4)

**Why:** all connections (control + workers) funnel through one `conn_rx` mpsc (`:536`);
control = first popped, workers = next N, relying on arrival order. In persistent mode a
late/stray worker conn from a finished transfer, or a concurrent second sender, can be
misrouted. Worker conns validate `transfer_id` (`:1190`); the control conn validates nothing
beyond "first frame is `Begin`".

**Decision (fixed): bounded hardening now; full first-frame dispatcher is future work.**
1. **Drain-until-empty** instead of timed drains. Replace the timed `timeout_at` drain loops
   in `run_listener` (`:596` and `:615`) with a non-blocking drain:
   ```rust
   while conn_rx.try_recv().is_ok() {} // discard any leftover conns from the finished transfer
   ```
   (Keep the existing 50 ms relay-drain `sleep` that precedes abort in the non-persistent
   path; that is unrelated and must stay.)
2. **Keep `Begin`-first enforcement** (already at `:996`). A stray worker conn presenting
   `WorkerHello` as the first control frame will hit `bail!("unexpected first control
   frame…")`; in persistent mode that already logs a warn and continues. Improve the message
   to name the likely cause:
   `bail!("unexpected first control frame (a stray worker connection from a previous transfer?): {other:?}")`.
3. **Document** in README/`docs/TRANSFER.md`: *persistent listener accepts one sender at a
   time; overlapping senders to the same listener are not supported.*

**Tests** (`tests/transfer_test.rs`):
- `transfer_persistent_drains_leftover_worker_conn` — run a persistent listener; complete
  one transfer; **manually** open an extra loopback connection to the (now-known) internal
  port sending a `WorkerHello`, then run a second normal transfer; assert the second transfer
  succeeds. (If reaching the internal loopback port from the test is impractical, instead add
  a unit test that the drain loop empties a pre-filled mpsc, and rely on the existing
  `transfer_persistent_listener_*` tests for end-to-end coverage. **Do not** craft a flaky
  timing test.)

**Gate:** fmt + clippy + test. No regressions.

---

# PHASE 5 — Polish

## P5.1 — Centralise & guard test seams (F8)

**Why:** `BORE_TRANSFER_TEST_MAX_CHUNKS` (`injected_fail_after_chunks`, `:3139`) and
`BORE_TEST_CONFIRM_RESPONSE` (`read_confirmation_line`) are read unconditionally in the
release binary.

**Code:**
1. Group both behind one module: `mod test_seam { … }` with clear `// TEST SEAM — not for
   production use` doc comments and the two reader functions.
2. Keep the runtime env reads (the subprocess integration tests need them at runtime, so a
   `#[cfg(test)]` gate is insufficient — those tests spawn the real binary). Instead, on
   process start in `run_listener`/`run_sender`, if any `BORE_*TEST*` var is set, emit a
   one-time `warn!("bore test seam active via env: {var}; not for production")`. Implement a
   single `test_seam::warn_if_active()` called once at the top of both entry points.
3. No behaviour change to the seams themselves.

**Tests:** existing tests that rely on the seams must still pass (that is the proof). Add a
trivial unit test that `test_seam::warn_if_active()` does not panic when vars are
set/unset.

**Gate:** fmt + clippy + test.

## P5.2 — Sender file-count seeding on resume (F9, cosmetic)

**Why:** sender's `add_file()` fires only at `chunk_index==0` (`:863`); fully-resumed files
(whose chunk 0 is not re-sent) are never counted, so the progress line under-reports files
on resume.

**Code (`send_transfer`, after computing `tasks`/`resume_plan`):** count files that are
fully complete in the resume plan (all chunks present) and seed the progress counter:
```rust
let resumed_files = plan.entries.iter()
    .filter(|e| e.manifest.kind.is_regular_file())
    .filter(|e| {
        resume_plan.iter().find(|r| r.entry_id == e.manifest.id)
            .map(|r| r.completed_chunks.len() as u32 == e.manifest.chunk_count)
            .unwrap_or(false)
    })
    .count() as u64;
handle.add_files(resumed_files); // add a ProgressHandle::add_files(n) that bumps files_done by n
```
Add `ProgressHandle::add_files(&self, n: u64)` next to `add_file` (just
`files_done.fetch_add(n, …)`).

**Tests:** unit test for `add_files` increment; the visual count is otherwise cosmetic — no
brittle assertion on rendered output.

**Gate:** fmt + clippy + test.

---

# PHASE 6 — Documentation (F11)

Update in lockstep with the behaviours added above. Sub-phases:

## P6.1 — README transfer section
- Document receiver `--ask-confirm` semantics explicitly (always shows the manifest; gates on
  y/N; **ignored for stdin sources** and why).
- Document the **3-level BLAKE3** model: per-chunk (on the wire), per-file (`full_hash`),
  per-transfer (`transfer_hash`).
- Document `--carriers` × `--parallel` interaction (relay HOL-blocking; match carriers to
  parallel on relay; carriers ignored on direct UDP).
- Document the new flags: `--confirm-timeout`, `--stall-timeout`.
- Document the **resume breadcrumb** (`committed.json`) and that successful transfers leave
  it; safe to delete; enables idempotent re-runs.
- Document **persistent listener = one sender at a time**.

## P6.2 — `docs/TRANSFER.md` (consolidated user guide)
Create or update a single canonical guide mirroring the VHOST docs style: quick start, full
flag tables (both sides, including new flags + defaults), resume semantics + breadcrumb,
collision policies, stdin limitations, idempotent re-completion, timeouts, persistent mode
constraint, and the protocol sequence diagram from the top of this file.

## P6.3 — `docs/TRANSFER_TEST_MATRIX.md`
Add rows for every test added in P1–P5; move the now-closed items out of "coverage gaps"
(oversized-len, stdin temp cleanup, confirm timeout, stall timeout, idempotent re-completion,
persistent drain). Re-state remaining gaps honestly (real-tty input, >10 GiB files,
bandwidth/perf, full first-frame dispatcher for concurrent persistent senders).

**Gate:** docs build/read cleanly; final full `cargo fmt --check && cargo clippy
--all-targets -- -D warnings && cargo test`.

---

## Final acceptance (definition of "production grade")
- All of P1–P6 merged; full gate green; zero new regressions vs the documented baseline.
- Manual smoke tests performed and recorded in the test matrix:
  1. Large-file resume across a killed listener.
  2. Oversized/malformed `ChunkStart` rejected (crafted peer).
  3. Commit→Completed window drop → idempotent retry succeeds.
  4. `--confirm-timeout` rejects an unattended `--ask-confirm` listener.
  5. `--stall-timeout` aborts a stalled transfer.
- Every transfer socket carries `TCP_NODELAY` (F1) — verify with the P1.1 test.
