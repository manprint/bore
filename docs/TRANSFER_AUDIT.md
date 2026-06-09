# `bore transfer` deep audit

Adversarial review of the transfer sender/receiver: bugs, races, deadlocks, the
send/receive protocol, `--ask-confirm`, and interruption/disconnection on both sides,
across the **relay** and **direct UDP** transports. Each finding was verified against the
code path before acting; speculative findings were rejected.

Result: **lib 121 · transfer_test 39 · stdin CLI 13 — 0 failures**; `cargo fmt` + `clippy
-D warnings` clean. (`admin_test` has 2 failures unrelated to transfer — pre-existing in
this environment, `Connection reset by peer`.)

## Fixed

| # | Sev | Bug | Fix |
|---|-----|-----|-----|
| 1 | HIGH | **Receiver deadlock on sender interruption.** Sender dies after `ManifestAccepted` but before opening all data streams → receiver blocks forever on `incoming.recv()` (no timeout, no control watch). Affects relay *and* UDP. | `accept_worker_stream`: `select!` over worker-arrival vs control-EOF (`peek`, no byte consumed) vs idle timeout. 3 unit tests. |
| 2 | MED | **Truncated data stream = success.** Worker EOF before `WorkerDone` returned `Ok` (verification caught it later as a confusing hash mismatch). | `None` arm now bails `data stream closed before WorkerDone (sender interrupted?)`. |
| 3 | MED | **Stray worker breaks the next transfer** (persistent listener): a late data-stream from a finished transfer was parsed as a control `Begin`. | Typed `StrayWorkerConnection`; `run_listener` skips it and keeps serving. |
| 4 | LOW | **Double `Error` frame** on reject/confirm-timeout (inline send + outer handler). | Single emission via the outer handler. |
| 5 | HIGH | **`bore transfer sender --sources stdin` always failed** (`failed to stat stdin`): `confirm_sources_sync` stat'd the `stdin` sentinel. Pre-existing; the stdin CLI suite wasn't being run. | Skip stat for the `stdin` sentinel; reject `stdin` mixed with file sources with a clear message. |
| 6 | LOW | **Vague logs/errors** (`channel closed`, `transport ended before a sender connected`, persistent-mode failure). | Added transfer-id / counts / accurate cause to the messages. |
| 7 | — | **Idempotency redesign** (chosen over keeping the on-disk marker): see below. | Content-based; full destination cleanup. |

### Idempotent re-completion redesign
The old design wrote a `committed.json` marker into a hidden `dest/.bore-transfer-state-*/`
dir and **left it forever**, conflicting with the "clean up after success" expectation.
Replaced with **content-based idempotency**: a successful transfer removes its entire working
state dir; an identical re-run is detected by comparing the destination's content
(size + BLAKE3) to the manifest (`destination_satisfies_manifest`) and re-acknowledged
without re-sending. Differing content → normal collision per policy. Symlink/device entries
are conservatively treated as "not a match" (they take the normal transfer/collision path).

## Rejected after verification (NOT bugs)
- **`recv_frame` EOF after the length prefix** → suggested "return `Ok(None)`" is *harmful*:
  on a worker stream it would mask a truncated transfer as clean completion. Current `Err`
  is correct.
- **50 ms drain → data loss**: data is hash-verified and `Completed` is exchanged *before*
  the abort; the drain only suppresses spurious "channel closed" warnings.
- **`verify_summary` skip-rehash**: only skips when *all* chunks were written and per-chunk
  verified this run; resumed chunks are always re-hashed. Sound.
- **`flush_pending` ordering / persist races**: data is fsync'd before the resume bitmap is
  persisted; a lost flush only causes a safe redundant resend, never corruption.
- **Resume collision under same transfer-id**: guarded by `manifest_hash` (bail, not corrupt).

## Known limitation (documented, not fixed)
`--ask-confirm` + `--persistent` + repeated `--confirm-timeout` expiries leak the blocking
`/dev/tty` reader thread (a blocking stdin read can't be cancelled portably). Narrow:
interactive one-shot use is unaffected. Prefer `--confirm-timeout 0` (wait forever) or
interactive (non-persistent) use when combining these flags.
