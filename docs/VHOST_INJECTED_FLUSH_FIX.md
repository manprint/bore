# Vhost injected-response keep-alive stall — missing flush before parking on read

**Date:** 2026-07-07  ·  **Status:** fixed, regression-tested  ·  **Branch:** ssh  ·  **Commit:** `36cd70d`

## Symptom (field report)

A vhost tunnel with **injected response headers** (e.g. `default_response_headers`
in `vhost.yml`: CSP / HSTS / X-Frame-Options) served over HTTPS: pages frequently
failed to finish loading — one or more assets stuck `pending` in the browser dev
tools, the connection apparently stalled. Observed with both native vhost providers
and SSH-gateway (`ssh -R vhost/...`) providers; a manual refresh sometimes healed it.

Only **keep-alive** responses were affected. `Connection: close` responses always
completed.

## Root cause — encrypted bytes parked in the rustls session buffer

`relay_response_injected` (`src/vhost.rs`) is the dedicated splice path used **only
when the vhost entry has response headers to inject**. It hand-rolls the copy loop
(it must intercept and rewrite the response head before splicing), instead of using
`tokio::io::copy_bidirectional_with_sizes` like the no-injection path.

The public side of that splice is (usually) a tokio-rustls `TlsStream`. tokio-rustls
`poll_write` encrypts the plaintext into the rustls session buffer and then *tries*
to drain the resulting TLS records to the socket — but if the socket returns
`Pending` (browser applying TCP backpressure), `poll_write` still returns
`Ready(Ok(n))` as long as the plaintext was accepted. The encrypted remainder stays
buffered inside the session, waiting for the **next** `poll_write` or `poll_flush`
to push it out.

The old loop never issued that next poll:

```text
write_all(last body chunk)   → Ok, but tail records still buffered in rustls
loop → reader.read(...)      → provider has no more data (response complete)
                             → keep-alive: no EOF either  → parked forever
```

- With `Connection: close` the provider EOF triggers `writer.shutdown()`, whose
  `poll_shutdown` flushes the session first — which is why close-mode responses
  never stalled.
- The **no-injection** path was never affected: tokio's `CopyBuffer` (used by
  `copy_bidirectional_with_sizes`) explicitly flushes the writer whenever the read
  side returns `Pending` ("flush-on-pending").
- Large bodies (multiple TLS records, e.g. dufs' ~107 KiB `index.js`) made the
  partial-socket-write window big enough to hit routinely.

## Fix

Two `flush().await` calls in `src/vhost.rs`:

1. `relay_response_injected` — after writing the rewritten response head, before
   the body copy loop starts (a partially-buffered head must reach the browser
   before we park waiting for body bytes).
2. `copy_one_direction_with_shutdown` — after every `write_all` in the splice loop,
   before looping back to `reader.read()`.

This guarantees the rustls session buffer is drained before the task can park on a
read with no other wake-up source.

### Why per-write flush (and not something "cleaner")

- **Flush-on-pending** (what `CopyBuffer` does) is the canonical minimal semantics,
  but hand-rolling it requires poll-level code; two `flush` lines are not worth that
  complexity. When data is flowing the buffer is typically already empty and the
  flush is ~free; under backpressure it merely trades one buffer of read-ahead
  (256 KiB default) for correctness — negligible next to TLS cost.
- **`tokio::io::copy`** would provide flush-on-pending for free but uses a fixed
  8 KiB internal buffer vs `proxy_buffer_size()` (256 KiB default,
  `BORE_PROXY_BUFFER_SIZE`) — a high-BDP throughput regression. Do not swap it in.
- The `tokio::io::split` + `try_join!` **single-task** shape is *required*: the
  provider side is a `mux::LinkStream` (possibly yamux), and yamux streams must
  never be split across two tasks (single parked-task waker — see the yamux
  invariant in CLAUDE.md). Restructuring into two spawned tasks would reintroduce
  the silent-wedge bug that invariant exists to prevent.
- Flushing the provider direction too (`provider_write`: yamux / QUIC / russh
  channel) is a near-no-op and keeps the loop symmetric.

The other raw `write_all` sites in the vhost path are safe: `send_bad_gateway` /
`send_service_unavailable` end with `shutdown()` (which flushes), the basic-auth
401 already flushed explicitly, and the request-head write into the provider is
followed by the flushing copy loop.

## Regression tests

### Enforcing gates — deterministic unit tests (`src/vhost.rs` `mod tests`)

These are the tests that actually go RED if the flushes are removed
(red-checked 2026-07-08: both fail without the fix, pass with it). They use a
`FlushGatedWriter` mock that encodes the tokio-rustls write contract directly —
`poll_write` accepts bytes into a `pending` buffer and returns `Ok`; only
`poll_flush`/`poll_shutdown` publishes them to `visible` — plus a
`ChunksThenPark` reader whose `Pending`-forever tail is the keep-alive shape.
Plain `#[test]`, single manual poll, no runtime, no timing: zero flake surface.

- `copy_loop_flushes_writes_before_parking_on_read` — the splice loop must
  publish every written byte before parking on read (RED without the fix).
- `injected_response_head_and_body_flushed_before_keepalive_park` — the full
  `relay_response_injected` path: rewritten head (flush #1) and body bytes
  (flush #2) must be on the wire before the relay waits for more provider data
  (RED without the fix).
- `copy_loop_eof_propagates_half_close_and_publishes_tail` — the EOF path
  still shuts down (half-close invariant) and shutdown itself publishes the
  tail (green with or without the explicit flushes — documents why
  `Connection: close` responses never stalled).

### Integration coverage (belt-and-braces — NOT red-capable for this bug)

**Finding (2026-07-08): every in-process TLS integration test of this bug
false-passes on loopback** — verified by running them with the flushes
removed: all green. In-process, the rustls session gets drained
opportunistically through other poll paths and kernel buffers absorb the
rest, so the field stall does not reproduce even with a tiny `SO_RCVBUF`
slow reader and a 12 MiB body. The field bug needed a real network path.
These tests still guard adjacent regressions (truncation, framing desync,
demux misrouting), but the unit mocks above are the only enforcing gate —
do not "simplify" them away in favor of the integration tests.

- `vhost_response_header_injection_large_keepalive_body_completes`
  (`tests/vhost_test.rs`) — plain vhost, injected CSP header, 109 648-byte body,
  stub keeps the connection open 30 s after writing (keep-alive: no EOF may mask
  the bug via shutdown-flush). Asserts the complete, byte-exact body arrives.
- `t_ssh_dmx5_unified_tls_vhost_ssh_parallel_assets` /
  `t_ssh_dmx6_unified_tls_vhost_large_keepalive_asset_completes`
  (`tests/ssh_gateway_test.rs`) — the real deployment shape: single control port
  demuxing raw SSH + HTTPS (ALPN) + native bore, vhost provider over `ssh -R`,
  injected default response headers, parallel asset fetches and a large keep-alive
  asset over TLS. Asserts 200s, exact body, and never an `SSH-2.0` banner.
- `t_ssh_dmx7_unified_tls_vhost_slow_reader_backpressure_completes`
  (`tests/ssh_gateway_test.rs`) — 12 MiB keep-alive body (beats kernel
  `tcp_wmem` autotuning) fetched by a slow reader with a 16 KiB `SO_RCVBUF`
  and a pause between reads: sustained real socket backpressure across the
  whole transfer. Asserts the byte-exact body completes within 60 s.
- `t_ssh_dmx8_unified_tls_vhost_keepalive_request_sequence_no_desync`
  (`tests/ssh_gateway_test.rs`) — three sequential requests on ONE TLS
  connection through the injection path. Only the first response head is
  rewritten (MVP contract); asserts each response completes with the exact
  body and no framing desync, and that the first carries the injected headers.
- `vhost_https_response_header_injection_large_keepalive_body_completes`
  (`tests/vhost_test.rs`) — NATIVE provider, dedicated HTTPS frontend port
  (`handle_https` → `relay_response_injected`): the TLS-terminated variant of
  the plain-HTTP keep-alive test.
- `vhost_response_header_injection_keepalive_request_sequence_no_desync`
  (`tests/vhost_test.rs`) — NATIVE provider, three sequential keep-alive
  requests on one connection through the injection path (desync guard).
- `vhost_udp_response_header_injection_large_keepalive_body_completes`
  (`tests/vhost_test.rs`) — NATIVE `--udp` provider: `relay_response_injected`
  with the provider side on a QUIC bidi stream instead of a yamux substream
  (previously untested combination), 1 MiB keep-alive body, asserts the direct
  path was actually used.

## Native-client path audit (2026-07-08) — no further flush-class bugs

The full native `bore vhost` data path was audited for the same bug class
after the fix; every write site is clean:

| Site | Verdict |
|---|---|
| Client splice (`client.rs handle_connection`) | `copy_bidirectional_with_sizes` — flush-on-pending built in |
| `weblog::HttpAccessTap`, `shared::CountingStream` | `poll_flush`/`poll_shutdown` delegate to inner (wrappers can't strand bytes) |
| `basicauth::gate` 401 (client) + `relay_vhost` 401 (server) | explicit `flush` + `shutdown` |
| `mux::write_stream_ready` | explicit `flush` (mux.rs) |
| `edge::write_https_redirect` | explicit `flush` |
| `send_bad_gateway` / `send_service_unavailable` | end with `shutdown()` (flushes) |
| yamux substream writes over TLS control conn | the yamux connection driver flushes the underlying socket after frame batches (upstream behavior); `copy_bidirectional` flush-on-pending covers the substream layer |
| `spawn_direct` QUIC accept loop (client `--udp`) | per-stream spawn, slot released on every exit path; quinn sends eagerly |
| `connect_with_timeout` local service conns | `tune_tcp` applied (invariant held) |
| `edge::read_request_head` / `read_head_async` caps | oversized heads returned partial and forwarded raw — never a desync or an error |

## Rule going forward

Any hand-rolled write path whose peer socket may be TLS-wrapped must either end in
`shutdown()` or `flush()` before the task parks on a read (or returns to a pool).
`write_all(...).await? == Ok` does **not** mean the bytes left the process.
