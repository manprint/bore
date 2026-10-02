# Phase 0 — Liveness primitives

Intent: provide transport-level liveness, prompt termination, a single policy home, and a faster reconnect backoff. No role uses these yet, so behaviour does not change except for the backoff cap.
Prerequisites: none (baseline `f7b0745`).
Phase closure: P0; review by agent-1 (claude-opus-5-5, explicit self-review).

## State and ownership contract
1. Read STATE.md first. In a fresh session, reconcile the active unit, the actual diff, the step checkpoint, the scope and the commit reference.
2. Open the unit before editing. Checkpoint after each meaningful batch.
3. Close only after tests, review and coherence. Then commit (full-autonomous) with the PEV trailers.
4. Missing design goes to the supervisor; it is not invented.

## Local design context
Plan revision 1.
- **D1.** Liveness = time since the last byte read from the peer's transport.
  - It is stamped in a wrapper around the socket, *below* yamux, so every frame counts: data, control, ping and window update.
  - Rationale: a congested tunnel is never judged dead.
  - yamux keeps reading while writes are blocked (R5).
- **D4.** `terminate()` cancels the driver task. Dropping `yamux::Connection` closes and wakes every stream (R2).
  - Used ONLY by liveness trips.
  - The graceful path (`poll_close`) stays byte-identical.
- **D2/D3 constants.** Defaults and resolvers live in NEW `src/liveness.rs`:
  - client heartbeat 5 s (`BORE_CTRL_HEARTBEAT_MS`);
  - client silence deadline 15 s (`BORE_CTRL_SERVER_SILENCE_MS`, 0 = disabled, clamp [100 ms, 1 h]);
  - server reap = `max(3 × declared, floor)` with floor 15 s, `None` when declared = 0.
- **D5.** The backoff cap drops from 32 to 8 s.
- **I-1.** Any inbound byte refreshes liveness.
- **I-6.** Only liveness trips terminate.

Clock: use `tokio::time::Instant`, so paused-clock tests are deterministic. Store `last_inbound_ms` as `AtomicU64` milliseconds since `base: Instant` (created at spawn); `inbound_idle = base.elapsed() - last`. Ordering: `Relaxed` is enough (a monotonic hint, no data published through it).

## Sub-phases

### 0.1 mux transport activity + terminate
- **Model:** agent-1
- **Assignment:** implement + self-review the lifecycle (driver cancel vs `ClientScope` cancel; M-1 counting untouched).
- **Files:** WRITE `src/mux.rs` — NEW `ActivityIo<S>`, `Activity`, `ConnActivity`, `Opener::activity`, `Acceptor::activity`; `spawn_driver_inner`. READ the yamux drop semantics (R2).
- **Change:**
  - Contract:
    - `ConnActivity` is `Clone + Debug + Send + Sync`.
    - `inbound_idle() -> Duration`.
    - `terminate()` is idempotent.
    - `is_terminated() -> bool`.
    - `reap_due(deadline: Option<Duration>) -> bool`: `deadline.is_some_and(|d| inbound_idle() >= d)`.
  - Steps:
    1. S1 — Define `struct Activity { base: tokio::time::Instant, last_inbound_ms: AtomicU64, cancel: tokio_util::sync::CancellationToken }`, plus `pub struct ConnActivity(Arc<Activity>)` with the methods above. Expected: compiles; the doc comment cites D1/D4.
    2. S2 — Define `ActivityIo<S>`. It implements tokio `AsyncRead` + `AsyncWrite` by delegation. `poll_read` stamps when `buf.filled().len()` grew. Pin projection: `S: Unpin`, so use `Pin::new(&mut self.inner)`. Expected: compiles.
    3. S3 — In `client`, `client_scoped` and `server`, wrap `socket` as `ActivityIo::new(socket, activity.clone())` before `.compat()`. Thread `Arc<Activity>` into `spawn_driver_inner` and store a `ConnActivity` in `Opener` and `Acceptor`.
       - The task `select!`s on `activity.cancel.cancelled()` alongside the existing branches.
       - The scoped branch keeps the scope cancel too.
       - Expected: every existing mux test still passes.
  - Checkpoint after S3.
  - Failure handling: if `Connection<Compat<ActivityIo<S>>>` type changes break `Transport` bounds, keep `ActivityIo<S>: Transport` via the blanket impl (`Unpin + Send + 'static` from `S`).
- **Unit tests** (`src/mux.rs mod tests`, gate G-U01):
  - `inbound_idle_grows_while_the_peer_is_silent_and_resets_on_a_frame`: real TCP pair; idle > 200 ms after a 250 ms silence; < 100 ms right after the peer writes on a substream.
  - `inbound_idle_is_refreshed_by_data_substream_bytes` (I-1): the peer streams bytes on a data substream every 50 ms for 1 s with no control traffic; idle never exceeds 300 ms.
  - `terminate_closes_live_substreams_promptly`: a substream held by a reader task; `terminate()` → the read returns `Ok(0)`/`Err` within 2 s; the PEER's acceptor `accept()` returns `None` within 2 s; `is_terminated()` true; a second `terminate()` is a no-op.
  - `reap_due_none_never_fires` + `reap_due_fires_at_deadline` (paused clock not needed: `Some(Duration::ZERO)` → true, `None` → false).
  - Red-check: removing the stamp in `poll_read` fails the first two; removing the cancel arm fails `terminate_*`.
- **e2e tests:** N/A — no role consumes it yet.
- **Done:** tests pass; existing `mux` tests pass; G-U01 green; committed.

### 0.2 liveness policy module
- **Model:** agent-1
- **Files:** NEW `src/liveness.rs`, registered `pub mod liveness;` in `src/lib.rs`. WRITE `src/secret.rs` — `CTRL_CLIENT_HEARTBEAT` and `ctrl_client_heartbeat()` re-exported from / delegating to `liveness`, preserving their public names for existing callers.
- **Change:**
  - Contract (constants):
    - `CTRL_CLIENT_HEARTBEAT = 5 s`
    - `CLIENT_SILENCE_DEADLINE = 15 s`
    - `TRANSPORT_REAP_FLOOR = 15 s`
    - `TRANSPORT_REAP_MULTIPLIER = 3`
  - Contract (functions):
    - `ctrl_client_heartbeat()` keeps its existing parse semantics, moved verbatim.
    - `ctrl_heartbeat_declared_ms() -> u32` saturates `ctrl_client_heartbeat().as_millis()` to u32, min 1.
    - `client_silence_deadline() -> Option<Duration>` reads `BORE_CTRL_SERVER_SILENCE_MS` per call through pure `parse_silence_ms(Option<&str>) -> Option<Duration>`:
      - absent or unparsable → `Some(15 s)`;
      - `0` → `None`;
      - otherwise clamp [100 ms, 3 600 000 ms].
    - `liveness_tick(deadline) -> Duration` = `(deadline / 4).clamp(50 ms, 1 s)`.
    - `transport_reap_deadline(declared_ms: u32, floor: Duration) -> Option<Duration>` = `None` if 0, else `Some(max(declared × 3, floor))`, saturating, capped at 1 h.
  - Steps:
    1. S1 — Create the module with docs that cite D1–D3 and the field report. Expected: compiles.
    2. S2 — Make `secret::CTRL_CLIENT_HEARTBEAT` / `ctrl_client_heartbeat` delegate. Update the existing test that pins 20 s, if any (find with grep `CTRL_CLIENT_HEARTBEAT`), to the new value, and record it as a deliberate D3 change.
- **Unit tests** (`src/liveness.rs`): parse table (absent, garbage, `0`, `50` → 100 ms clamp, `15000`, huge → 1 h); reap-deadline table (0 → None; 5000 + floor 15 s → 15 s; 10 000 → 30 s; u32::MAX → 1 h; floor 1 s + 200 → 1 s); tick clamp table; `ctrl_heartbeat_declared_ms` ≥ 1.
- **e2e tests:** N/A.
- **Done:** gates green; committed.

### 0.3 reconnect backoff cap 8 s
- **Model:** agent-1
- **Files:** WRITE `src/reconnect.rs` — `DEFAULT_MAX_BACKOFF_SECS`, module doc, the test `backoff_follows_capped_doubling_sequence`. READ `src/vpn.rs` `run_with_reconnect` and `src/transfer_link_cli.rs` (both use `Backoff::new()`) and fix their comments that cite 32 s.
- **Change:** set the constant to 8. The doc sequence becomes 1, 2, 4, 8, then every 8 s. The VPN comment `// 1 s .. 32 s` becomes `1 s .. 8 s`.
- **Unit tests:** the updated sequence test expects `[1,2,4,8,8,8]`; add `default_cap_is_eight_seconds`.
- **e2e tests:** N/A (exercised by phase 4).
- **Done:** gates green; committed.

### 0.4 README
- **Model:** agent-1
- **Files:** `README.md` — the `--auto-reconnect` description(s).
- **Change:**
  - S1 — Replace "1, 2, 4, … 32 s" with the 8 s cap wherever it is documented (grep `32` near `reconnect`).
  - S2 — No liveness text yet (phase 1 ships the behaviour); record that in STATE.
- **Unit tests:** N/A (docs). **e2e:** N/A.
- **Done:** README accurate for shipped behaviour; committed.

## Phase gates and closure
- Gates G-P0: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (default), `cargo clippy --all-targets --features vpn,ssh-gateway -- -D warnings`.
- P0: self-review of M-1 interaction (the cancel arm must not change the zero-handle close path); commit the closure.
