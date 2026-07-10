# vhost / public / secret production-readiness assessment (2026-07-10)

Native-client audit of the **vhost**, **public** (`bore local`), and **secret**
(`bore proxy` + `--tcp-secret-id` provider) tunnel features across BOTH the TCP
relay path and the UDP/QUIC direct path. Goal: latent bugs, races, leaks,
deadlocks, performance bugs, flag inconsistencies → production grade.

Method: 5 parallel area audits (server/public+mux+pool, vhost, secret,
holepunch/QUIC, client/flags/reconnect), then **every** finding verified against
the actual code by the supervisor before any fix. Most reported findings were
false positives — recorded here so they are not re-hunted.

## Confirmed and fixed

| ID | Sev | Location | Defect | Fix |
|----|-----|----------|--------|-----|
| S-1 | LOW-MED | `server.rs` carrier-token offer | `pending_carriers.insert(token)` then `control.send(CarrierToken).await?` — a send failure returns via `?` **before** `TokenGuard` is built, orphaning the token in the registry forever (resource leak on the error path). | Build the `TokenGuard` RAII **before** the fallible send so the `?` unwind runs its `Drop` and removes the token. Byte-identical on success. Regression: `pool.rs::tests::token_guard_drop_removes_token`. |
| H3 | LOW | `holepunch.rs` `DirectListener::accept` | After token verification, `send.write_all(&token).await?` / `flush().await?` propagate a peer reset as a fatal `accept()` error; callers (`provider_direct`) then mislabel it as "endpoint closed" and take a 100 ms accept hiccup for an unrelated peer. | Treat a verified peer that vanishes before the token reply as a benign stray: log at `debug` and `continue` (matching the other stray arms). Only genuine `accept()`/`accept_bi()` endpoint errors stay fatal. |
| C3 | LOW (doc) | `main.rs` `local --carriers` help | Help said "Direct UDP ignores it", but `bore local --udp` sizes the direct QUIC pool from `--carriers` (`client.rs` `clamp_direct_carriers`, matching CLAUDE.md). Misleading flag documentation. | Corrected help text to describe the direct-QUIC-pool behavior, mirroring the `proxy` subcommand's help. |

## Rejected findings (verified NOT bugs — do not re-hunt)

- **Carrier/DirectPool `pick()` TOCTOU** (reported CRIT/HIGH by 3 separate
  agents): claimed the `Mutex`/`RwLock` guard is dropped before `.len()`/indexing.
  FALSE — the guard binds to a local (`carriers`/`conns`) held for the whole fn
  body; `.len()` and indexing run under the still-held lock. No race.
- **`CarrierPool::push` cap race** (secret): the len-check and push are both under
  the single held mutex in `push()`. Atomic. No over-cap.
- **Registry-snapshot use-after-free in `relay()`** (secret): `registry.get(id)`
  returns an `Arc` strong clone; the pool cannot deallocate while the relay holds
  it. Rust ownership guarantees it.
- **`spawn_closed_monitor` pends forever if already closed** (secret): quinn's
  `Connection::closed()` is level-triggered and resolves immediately on an
  already-closed connection. No leak.
- **`upgrade_task` / `spawn_direct` accumulate across reconnect** (secret/client):
  `spawn_direct` has no retry loop; it self-terminates when the QUIC endpoint
  drops. The endpoint is owned by the serve closure and recreated per reconnect,
  so old direct tasks die with it. Bounded by endpoint lifetime.
- **vhost `DirectPool` never prunes dead conns / `remove()` has no caller**
  (vhost): the removal path lives in `server.rs` (a `direct.closed().await`
  monitor calls `entry.direct.remove(id)` at install time), not in `vhost.rs`.
  The vhost-only auditor missed it. Dead conns are pruned on close.
- **`relay_response_injected` missing read timeout** (vhost): the non-injected
  `copy_bidirectional` path has none either — a proxy must not time out a legit
  slow-first-byte backend. Consistent by design.
- **global `grx`/`gtx` counters leak per-tunnel bytes** (secret): those are the
  server-wide `total_rx/tx_bytes` totals by design; per-tunnel accounting uses the
  per-entry `CountingStream` (`erx`/`etx`).
- **`--max-conns` accepted on public** / **`--https` ignored on secret**
  (client): the client-side `--max-conns` semaphore only governs the secret
  provider direct path (as the help states); secret tunnels have no HTTPS
  frontend. Help is accurate; no behavioral bug.
- STUN responder busy-loop, hardcoded datagram buffer, token-handshake nonce
  model, endpoint-lifetime-via-clone: all either documented-by-design or benign
  (bounded, self-correcting).

## Verdict

The three paths were already largely production-grade — the documented invariants
in CLAUDE.md match the code, and the RAII/lifecycle discipline is sound. The audit
surfaced three genuine but low-severity issues (one error-path resource leak, one
error-path observability/robustness bug, one stale flag doc), all fixed with
zero behavior change on success paths.
