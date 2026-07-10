# `bore transfer` production assessment — 2026-07-10

Full adversarial audit of every transfer operating mode (single file, directory tree,
multi-source flat, multi-source `--output`, stdin stream, resume, persistent listener,
collision policies, relay + direct-UDP transports). Method: 5 parallel audit agents
(concurrency/protocol, filesystem/resume crash-consistency, transport integration,
perf + edge inputs, test-coverage mapping), every finding re-verified by the supervisor
against the actual code paths before acting. Builds on the prior audit
(`TRANSFER_AUDIT.md`) and the F1-F10 hardening (`TRANSFER_TODO.md`, commit `cb44b39`) —
everything below is NEW relative to those.

Result: **8 bugs fixed (B1-B8) + 1 performance fix (P1)**, ~20 agent findings rejected
after verification. Gates: `cargo fmt` + `clippy --all-targets -D warnings` clean,
full test suite green (lib 287 · transfer_test 39 · stdin CLI 13 · all other
integration suites 0 fail). 12 new regression tests.

## Fixed

| # | Sev | Bug | Fix |
|---|-----|-----|-----|
| B1 | HIGH | **Hostile `total_entries` aborts the receiver.** `receive_manifest` did `Vec::with_capacity(begin.total_entries as usize)` — a peer-controlled `u64::MAX` attempts a huge allocation (process abort) before any validation; the manifest loop also accumulated entries unboundedly until `ManifestDone` (memory pinning). Authenticated-only, but a single bad frame killed a persistent listener. | `MAX_MANIFEST_ENTRIES` (2 M) checked before the allocation; entry count enforced *while receiving* (bail as soon as `entries.len() > total_entries`). Tests: `receive_manifest_rejects_oversized_total_entries`, `receive_manifest_rejects_more_entries_than_declared`. |
| B2 | MED | **`ProgressTracker` tick-task leak on every error path.** `finish()` is only called on success; on any bail the tracker was dropped without stopping the spawned 250 ms renderer — it re-rendered a stale progress line to stderr for the process lifetime, accumulating one task per failed transfer under an interactive persistent listener. | `impl Drop for ProgressTracker` (set `finished` + `abort()`). Test: `progress_tracker_drop_stops_tick_task`. |
| B3 | MED | **Pre-manifest phase had no stall bound.** `Begin` + `ManifestChunk` reads were bare `expect_frame` — an alive-but-silent sender (SIGSTOP'd process, wedged peer; TCP keepalive never fires because the loopback leg stays healthy) pinned the listener forever. A persistent listener serves control connections sequentially, so one silent client blocked all future transfers. | `with_stall(stall_timeout, …)` around the `Begin` read and every manifest-frame read (`receive_manifest` now takes `stall_timeout`). The `--ask-confirm` wait keeps its own `--confirm-timeout` (user thinking time is not a stall). |
| B4 | MED | **`state.json` not fsynced before rename + corrupt state fatal.** Crash/power-loss could leave an empty/truncated `state.json` (rename metadata durable before data blocks); `load_resume_state` then failed to parse and every retry died with the same error until the user removed the state dir by hand. | `write_json_atomic` now writes via `spawn_blocking` with `sync_all()` on the tmp file before the rename + best-effort parent-dir fsync (Unix). `load_resume_state` returns `Option`: corrupt/missing ⇒ fresh start (all chunks re-sent and re-verified — the safe direction) with a `warn!`. Tests: `load_resume_state_treats_corrupt_file_as_fresh`, `write_json_atomic_roundtrips_resume_state`. |
| B5 | MED | **Multi-source partial commit + `Fail`/`Rename` = permanently stuck.** `commit_stage` moves each top-level child into `dest_root`; if child N of M failed, children 1..N-1 were already committed. On retry the resume state re-transferred them into staging, but commit then hit "destination already exists" on the committed children — every retry failed forever (manual cleanup required). | Per-child content idempotency: when `dst` exists, rebase the child's manifest entries (`entries_under_child`) and run `destination_satisfies_manifest`; a proven match is skipped as already-committed, anything else keeps today's collision behavior. Tests: `commit_stage_multi_source_skips_already_committed_child`, `commit_stage_multi_source_still_fails_on_real_collision`, `entries_under_child_rebases_paths`. |
| B6 | MED | **`--source-files` silently dropped any line containing `#`.** `line.contains('#')` treated `#` *anywhere* as a comment — `/data/project#backup/file.zip` vanished from the transfer with no error. | Comment ⇔ trimmed line *starts with* `#`. Test: `source_files_keep_paths_containing_hash`. |
| B7 | MED | **A second sender killed the in-flight transfer.** During the worker-accept phase every incoming loopback conn was spawned as a worker; a second sender's control connection (its first frame is `Begin`, not `WorkerHello`) made that worker bail, and the `JoinSet` error aborted the whole in-flight transfer. Realistic trigger: an impatient/auto-retrying sender restarted while the previous run is still streaming. | The first frame is now read and validated **in the accept loop**: a matching `WorkerHello` is spawned as a worker; anything else gets an `Error` frame ("listener is busy…") and is dropped without failing the transfer, bounded by `STRAY_CONNECTION_LIMIT` (64) so a hostile peer can't spin the loop. `handle_worker_connection` starts at the chunk loop. Test: `stray_begin_during_accept_phase_does_not_kill_transfer`. |
| B8 | LOW | **Stdin sender waited for `StreamVerified` without a stall bound** (receiver verifies incrementally and replies immediately after `StreamEnd`, so a missing reply means a wedged peer). | `with_stall(stall_timeout, …)` on that wait. The final `Completed` wait stays deliberately unbounded (receiver-side re-hash + commit of huge resumed trees is legitimate long work). |
| P1 | PERF | **Sender pre-hashed every file inline during the scan** — a full, single-threaded read of the entire tree before the first byte hit the wire (double read I/O overall; minutes of silent startup on big trees). | Scan leaves `full_hash` unset; `hash_planned_entries` fills it afterwards from a small thread pool (`available_parallelism` capped at 8, work-stealing over an atomic index; scan order — and thus manifest determinism — unchanged). Tests: `hash_planned_entries_matches_sequential_hash`, `scan_fills_full_hashes_after_parallel_pass`. |

## Rejected after verification (NOT bugs)

- **"Parallel workers double-write the same chunk (TOCTOU)"** — each chunk is a queue
  task popped exactly once on the sender; duplicate `ChunkStart`s require a malicious
  peer, and the `is_chunk_complete` guard makes even that a benign same-bytes rewrite.
- **"Duplicate multi-source basenames silently overwrite"** — `validate_manifest`
  rejects duplicate rel-paths (`duplicate manifest entry`), so the transfer fails
  cleanly at manifest time; nothing is overwritten.
- **"Symlink target swapped between resumed runs is kept"** — `manifest_hash` covers
  `symlink_target`; a changed target changes the hash and the resume bails.
- **"Same size + same full_hash but different content corrupts resume"** — requires a
  BLAKE3 collision.
- **"Missing directory fsync ⇒ partial file accepted"** — losing `state.json` to a
  crash degrades to a *fresh* transfer (all chunks re-sent, all fresh-verified), never
  to acceptance of a partial file. (Dir fsync added anyway as cheap belt-and-braces.)
- **"`sync_data` insufficient for file size"** — `fdatasync` persists metadata needed
  to retrieve the data, including size.
- **"Server silently clamps `--carriers` ⇒ throughput collapse"** — the server's
  `CarrierToken { extra }` carries the *effective* count and the client opens exactly
  that many; the pool is consistent. Residual: no client-side `warn!` when
  `effective < requested` (observability only, listed below).
- **"stall_timeout=0 can hang forever"** — 0 is an explicit user opt-out (default 60 s).
- **"Unbounded loopback conn channel = FD exhaustion"** — connections only arrive via
  the authenticated tunnel and the server's `--max-conns` semaphore bounds substreams.
- **"recv_frame EOF-after-length-prefix should be Ok(None)"** — re-rejected (see prior
  audit: it would mask truncation).
- **Concurrent same-id listeners sharing a state dir** — a second provider with the
  same secret id is refused by the server registry; a single listener serves transfers
  sequentially.

## Deliberate design points (documented, unchanged)

- Sender's `recv_manifest_accepted` and final `Completed` waits are unbounded: the
  receiver may legitimately sit in `--ask-confirm` (bounded by the *receiver's*
  `--confirm-timeout`) or in a long verify/commit of a huge resumed tree.
- Provider death mid-transfer fails the transfer (resume state preserved); the
  transfer listener intentionally has no auto-reconnect wrapper.
- `ResumeState.completed: Vec<bool>` JSON encoding: a 100 GiB file is ~102 k bools
  (~600 KiB JSON rewritten every 8 received chunks ≈ every 8 MiB). Works, but see
  recommendations.

## Recommendations (not implemented)

1. **P2 — compact resume bitmap:** encode `completed` as a hex/base64 bitmap (12.8 KiB
   instead of ~600 KiB per flush for a 100 GiB file). Local-only format, but gate it on
   `protocol_version` anyway.
2. **P3 — buffer reuse:** per-worker reusable chunk buffer instead of `vec![0u8; len]`
   per chunk on both sides (1 M allocations on a 1 TiB transfer).
3. **Observability:** client-side `warn!` when the carrier pool is server-clamped below
   the request; progress indication during the (now parallel) pre-hash phase.
4. **Test gaps worth closing** (from the coverage sweep): e2e where the stall timeout
   actually fires; chunk-corruption injection (hash mismatch path); sender-SIGKILL
   resume e2e; `--symlinks include` e2e; `--carriers N` load test; the two `#[ignore]`d
   Windows reserved-name stdin tests (sender stack overflow never root-caused).
5. **Known limitation carried over:** `--ask-confirm` + `--persistent` +
   `--confirm-timeout` leaks one blocking `/dev/tty` reader thread per expiry (prior
   audit; unchanged).
