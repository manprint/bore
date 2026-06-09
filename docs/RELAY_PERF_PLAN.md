# Relay performance fix plan (A + B)

Goal: maximum throughput on **both** transports.
- **Relay (TCP fallback):** eliminate the single-carrier head-of-line bottleneck.
- **Direct UDP/QUIC:** already optimal (per-connection QUIC bidi, BBR, tuned windows
  & socket buffers, `max_direct_streams = 4096`). Must not regress it.

Scope of the diff that triggered this: `a143fdd..HEAD` changed **only** `transfer.rs` +
`main.rs` (+ docs/tests). The relay transport (`server.rs`, `secret.rs`, `client.rs`,
`pool.rs`, `transport.rs`, `mux.rs`, `shared.rs`) is byte-for-byte unchanged. The transfer
data hot path is functionally identical to the previous (`transfer_v2.rs`) version. The
two real issues are below.

---

## Root cause

### Slow relay (pre-existing, not introduced by the rewrite)
Transfer relay default was `--carriers 1` while `--parallel` auto-resolves to
`cpu.clamp(4, 32)`. On the relay path every worker substream is round-robined
(`pool.rs::pick`) across the carrier pool; with a single carrier all N substreams ride
**one** TCP connection → one congestion window + TCP-level head-of-line blocking on any
loss → throughput collapses on real (latency/loss) links. Invisible on loopback tests.
The code already warns about it (`transfer.rs` HOL warning). Direct UDP is unaffected:
each proxied connection gets its own native QUIC bidi stream.

### "Sometimes doesn't work" (real regression from the rewrite)
`with_stall(stall_timeout, fut)` wraps a whole-chunk `read_exact`/`write_all` (up to
1 MiB) in a single `tokio::time::timeout`. That is a **hard per-operation deadline**, not
the "no progress for N s" idle timeout its message claims. Math: 1 MiB / 60 s ≈ 17.5 KiB/s —
any relay slower than that aborts mid-chunk even while bytes are flowing.

---

## Fix A — auto-scale carriers to parallelism (relay throughput)

`--carriers 0` becomes "auto": match the parallelism so each relay worker substream gets
its own TCP carrier (independent congestion window, no cross-stream HOL), capped at the
server's default max (`server::DEFAULT_MAX_CARRIERS = 16`). Explicit values pass through
unchanged — `--carriers 1` still forces the exact single-connection path (invariant
preserved).

Changes:
1. `transfer.rs`: `const AUTO_CARRIER_CAP: u16 = crate::server::DEFAULT_MAX_CARRIERS;`
2. `transfer.rs`: helpers
   - `default_parallel_hint() -> u16` (`cpu.clamp(4, MAX_PARALLEL)`) — extracted from `plan_transfer`.
   - `resolve_parallel(req) -> u16` — `0 ⇒ hint`, else `clamp(1, MAX_PARALLEL)`.
   - `resolve_carriers(req, parallel_hint) -> u16` — `0 ⇒ parallel_hint.clamp(1, AUTO_CARRIER_CAP)`, else `req`.
3. `plan_transfer`: use `resolve_parallel`.
4. `run_sender`: `resolve_carriers(options.carriers, plan.parallel)` → `Proxy::new`.
5. `run_listener`: `resolve_carriers(options.carriers, default_parallel_hint())` →
   `new_secret_provider` (the listener can't see the sender's `--parallel` yet; the
   cpu-based hint matches the sender's auto default in the common case).
6. Update the HOL warning to compare against the resolved carrier count.
7. `main.rs`: transfer **sender + listener** `--carriers` `default_value_t` `1 → 0`; reword
   help. `bore proxy`/`bore local` `--carriers` stay at `1` (out of scope).

Direct-path safety: carriers only act on the relay path (`secret.rs` guards
`DataPath::Relay && carriers > 1`); on direct UDP the value is ignored.

Cost: an auto listener opens up to 16 provider→server control connections (bounded;
server cap = 16; `DEFAULT_MAX_CONNS = 1024`). Persistent listeners amortize this.

## Fix B — true idle stall timeout

Add idle-based I/O helpers that reset the deadline on every byte of progress, so
`stall_timeout` means "no bytes moved for N s", not "whole chunk within N s".

```rust
const STALL_IO_BLOCK // (read/write in COPY_BUFFER-sized steps via read()/write())
async fn write_all_idle(stream, buf, secs)  // timeout per write(); reset on n>0
async fn read_exact_idle(stream, buf, secs)  // timeout per read(); reset on n>0
```

Replace whole-chunk wraps:
- `send_chunked_files` chunk write → `write_all_idle`
- `handle_worker_connection` payload read → `read_exact_idle`
- `send_stdin_stream` data write → `write_all_idle`
- `receive_stdin_stream` data read → `read_exact_idle`

Keep `with_stall` for the **tiny** control/frame waits (`recv_frame`/`expect_frame`):
frames are small, a total deadline is correct there. Default stays 60 s.

---

## Tests
- Unit: `resolve_carriers` / `resolve_parallel` (0=auto, explicit pass-through, clamps).
- Unit: `write_all_idle` / `read_exact_idle` over `tokio::io::duplex` — paced slow writer
  (slices with delays each < timeout, total > timeout) **succeeds**; a true stall
  (no bytes for > timeout) **aborts** with "no progress". `secs == 0` = pass-through.
- Integration: `transfer_*_over_relay` with `carriers: 0` completes (auto multi-carrier).
- Regression: full suite, both transports (relay + direct UDP), zero failures.

## Docs
- `README.md` / `USER_GUIDE.md`: `--carriers 0` auto behavior; relay vs UDP perf notes.
- `CLAUDE.md`: update the carriers invariant (default now `0`/auto; `carriers=1` still the
  byte-for-byte single path).
- `docs/TRANSFER_TEST_MATRIX.md`: new test rows.

## CI gates (per CLAUDE.md)
`cargo fmt` · `cargo clippy -- -D warnings` · `cargo test` (full). Zero regressions.
