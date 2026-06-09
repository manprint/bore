# `bore local` / `bore proxy` / `bore test-udp` deep audit

Pre-staging adversarial review of the three non-transfer subcommands, plus the shared
transport/performance core (`mux`, `pool`, `shared`, `edge`, `transport`, `reconnect`).
Goal: stability, no bugs / races / deadlocks / memory leaks, clear and uniform logging,
and maximum transfer/bandwidth performance. The admin frontend page was explicitly out
of scope for this pass.

Each candidate was verified against the code path before acting; speculative findings
were rejected with the reasoning recorded below.

Result: **lib 121 · e2e 13 · carrier 5 · secret 10 · secret_pool 3 · tls 6 · udp 13 ·
mux 2 · basic_auth 3 · auth 2 · control_port 1 · reconnect 2 · transfer 39 · stdin CLI 13
— 0 failures.** `cargo fmt` + `clippy -D warnings` clean. (`admin_test` has 2 failures
unrelated to these subcommands — pre-existing in this environment, `Connection reset by
peer`; the admin page is out of scope.)

## Fixed

| # | Sev | Issue | Fix |
|---|-----|-------|-----|
| 1 | MED | **Stalled TLS handshake pins a connection slot (slowloris).** On a `--https` public tunnel the edge peeks the first byte, sees the TLS content-type, then runs `TlsAcceptor::accept(...).await` with **no timeout** — while the caller holds a `--max-conns` permit. A peer that sends one handshake byte then stalls keeps the permit forever; enough of them exhaust the tunnel's connection limit and stop it serving. The peek, the HTTP redirect, and the basic-auth read were already bounded by `NETWORK_TIMEOUT` — only this branch was not. | Wrap the handshake in `timeout(NETWORK_TIMEOUT, acceptor.accept(stream))`; on expiry return `TLS handshake timed out`. The spawned edge task ends, the permit is released, and the accept loop keeps serving. New regression test `stalled_tls_handshake_is_dropped_and_tunnel_keeps_serving` (`tests/tls_test.rs`). |
| 2 | LOW | **Non-uniform per-connection logging.** The relay client logged a *successful* per-connection close at `info!("connection exited")`, whereas the server (`trace!("proxied connection closed")`) and the secret relay (`trace!("secret relay closed")`) log per-connection closes at `trace`. Under load this is high-frequency, low-value noise on `info`, and inconsistent across roles. | Demote the client's success-close to `trace!`. Connection *arrival* stays at `info` (uniform with the server and the secret consumer), and the error case stays at `warn`. |

## Rejected after verification (NOT bugs)

- **`mux::drive` blocking on `inbound_tx.send().await` (channel cap 32) → deadlock.** The
  acceptor loops (`Client::listen`, `serve_consumer`, `serve_provider`, diagnostic
  `relay_loop`) only `accept()` and immediately `tokio::spawn` the handler, so they drain
  the inbound channel without ever awaiting downstream work — it cannot stay full. For a
  public tunnel the server never accepts inbound substreams (it only *opens* them), so the
  `Acceptor` is dropped and `send` returns instantly rather than blocking. Not reachable.
- **`copy_bidirectional_with_sizes` has no idle timeout → leaked tasks on a stalled peer.**
  Every proxied/relayed/forwarded socket is run through `shared::tune_tcp`, which sets
  `SO_KEEPALIVE` (15 s). A genuinely dead peer trips keepalive and the copy returns; the
  QUIC direct path has its own 10 s idle timeout. An *idle but live* connection (e.g. an
  open SSH session) is legitimately long-lived and must not be torn down. Correct as-is.
- **`CarrierPool::pick` race under concurrent relay tasks.** `pick`/`push`/`len` each take
  the `Mutex` and never `.await` while holding it; `retain` + modulo index are atomic under
  the lock. Round-robin is intentionally best-effort (`Relaxed`). Sound.
- **Public-tunnel `pool.pick()` returning `None` mid-flight.** Carrier 0 is the control
  connection's opener and is never marked dead; if the control connection itself dies, the
  500 ms heartbeat send fails first and tears the whole tunnel down before any `pick`. The
  `None` arm is defensive, not a live path for `--carriers 1`.
- **`--max-conns` permit leaked when `pick` fails / edge redirects.** The owned permit is
  bound to `_permit` inside the spawned task (or dropped on the `continue` before the
  spawn); every exit path drops it. Verified for the redirect, edge-error, open-error, and
  capacity branches.
- **Unbounded `mpsc` channels (carrier pump, re-punch, provider offers).** Each is scoped
  to a connection/tunnel lifetime and drained every `select!` iteration, or bounded
  upstream by the `--max-conns` / `--max-carriers` caps. No unbounded accumulation.
- **`.expect()` panics on the UDP paths.** The three (`provider udp cfg present when a
  socket exists`, CSPRNG, `direct UDP socket available for first attempt`) are all guarded
  by an enclosing condition or are infallible-by-construction crypto/SDK invariants, not
  reachable from untrusted network input.

## Accepted limitations (documented, not changed)

- **No client-side heartbeat deadline.** The server heartbeats every 500 ms; the client /
  consumer *drain* those heartbeats but do not enforce a receive deadline — liveness rests
  on `SO_KEEPALIVE` (15 s) plus, with `--auto-reconnect`, the reconnect loop. A hung-but-
  TCP-alive server is detected within the keepalive window rather than immediately. Adding
  a read-side watchdog is a behavioral change deferred out of this pass.
- **`provider_direct` accept retry.** A single bad QUIC handshake (stray peer / token
  mismatch) is logged and retried after 100 ms so one bad peer can't tear the listener
  down. A *persistently* broken endpoint would re-log every 100 ms until the control
  channel drops (which closes the listener via the `punch_rx` arm). Narrow; left as-is.
- **`bore test-udp` waits and relay tasks.** The diagnostic waits for its peer with no
  overall timeout (interactive, Ctrl-C to abort — intended UX), and the server-side
  `relay_loop` spawns per-stream relay tasks without an extra semaphore. Both are gated by
  the paired `--tcp-secret-id` handshake and backstopped by `SO_KEEPALIVE`; a diagnostic
  session is a single cooperating pair exchanging a handful of streams. Not a production
  traffic path.

## Performance posture (verified, no change needed)

The bandwidth-relevant tuning is already in place and was confirmed sound:

- **Carrier parallelism** (`--carriers`) round-robins proxied substreams across independent
  TCP connections to avoid yamux head-of-line blocking and give each its own congestion
  window. Default `1` for `local`/`proxy` keeps the single-connection path byte-for-byte.
- **Auto-tuned yamux receive window** (`set_max_connection_receive_window(None)`) lets a
  stream's window grow to the bandwidth-delay product instead of capping at the default
  credit.
- **`TCP_NODELAY`** on every socket (latency); **64 KiB** copy buffers per direction.
- **Direct UDP/QUIC:** 16 MiB socket buffers, 16/64 MiB stream/connection windows, BBR
  congestion control, 4096 max bidi streams.
