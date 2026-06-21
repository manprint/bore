# Secret Tunnel — Hardening Assessment & Fixes

**Date:** 2026-06-21 · **Branch:** `webserver-log` · **Plan:** `docs/plans/secret_tunnel_hardening_PLAN.md`

Max-severity bug hunt + hardening of secret tunnels (provider/consumer, relay + direct UDP,
`--carriers`, `--auto-reconnect`), triggered by a field session that showed (a) spurious `N/A`
rows in the /admin Consumers table after a TCP consumer with `--carriers 4`, and (b) an alarming
`WARN … direct udp accept failed … QUIC handshake failed` on the provider.

## Findings & verdicts

| ID | Sev | Status | Mechanism (verified in code) | Fix |
|----|-----|--------|------------------------------|-----|
| **BUG-S1** | CRIT | **FIXED** | Each extra relay carrier (`open_consumer_carrier`, `secret.rs`) re-sent a full `ConnectSecret` with sentinel fields; the server dispatched it to `serve_consumer`, which unconditionally registered an admin entry with `local_proxy_port=None` → rendered "N/A". `--carriers 4` ⇒ 1 real + 3 spurious rows. (UDP consumers skipped it only because the relay-carrier loop is gated on the relay path; a UDP consumer that *falls back* to relay leaked too.) | Additive `carrier: bool` on `ConnectSecret` (`#[serde(default)]`); carrier dialer sends `true`; `serve_consumer(carrier=true)` registers **no** entry. |
| **BUG-S2** | HIGH | **FIXED** | The same carrier connections never sent `ClientMessage::Heartbeat` (their client task only *drained* server frames), so `serve_consumer`'s liveness reaper saw no client frame and reaped each carrier after `ctrl_timeout` (60 s) → the consumer's relay carrier pool silently degraded **N→1** (no redial), and the spurious rows churned. | `serve_consumer(carrier=true)` skips the reap check entirely (`if !carrier && …`). Carrier liveness is owned by the consumer's main control connection (which heartbeats every 20 s). |
| **BUG-S3** | MED | **FIXED** | `DirectListener::accept` surfaced every benign hole-punch artifact (incipient QUIC connections from punch crossfire that never complete TLS, or carry no/again-wrong token) as an `Err`, which `provider_direct` logged at **WARN** "accept failed". The *real*, token-verified connection succeeded alongside it — the WARN was pure noise that alarmed the operator. | `accept()` now loops internally: benign strays are logged at `debug` and skipped; only an endpoint-level failure propagates as `Err` (logged at `debug` in `provider_direct`, no longer WARN). Improves all callers (VPN/diagnostic get a real connection within their timeout instead of aborting on the first stray). |
| **BUG-S4** | MED | **FIXED** | `relay()` did `pool.pick()` then a single `opener.open().await`; if the picked carrier died between pick and open, the forwarded connection dropped with no failover. | `relay()` retries `pick`→`open` across live carriers up to the pool size before failing (D5). |
| **BUG-S5** | LOW→MED | **FIXED** | `--carriers N>1` on a secret **UDP** consumer that established a direct path was a silent no-op (direct = a single QUIC connection; the relay-carrier loop is skipped). | One-shot `warn!` (D8): direct uses a single QUIC connection; `--carriers` applies only to the relay fallback. Multi-connection direct is out of scope (documented, not silently ignored). |
| **OBS-S6** | LOW | **By design** | Direct-path TX/RX is always `0.00 B` on the relay admin page — direct traffic is peer-to-peer, off the server, so the relay genuinely cannot count it. The field screenshot's `0.00 B` was also simply *idle* (no traffic had been sent). | No code change. The relay path **does** count live bytes (`CountingStream` in `relay()`). |
| **REJECTED (D7)** | — | **Not done (guard added)** | A hunter proposed disabling QUIC connection migration / filtering the accepted source against the offered candidate list (the field log accepted `peer=100.100.0.2` — a CGNAT egress never offered as a STUN candidate). | **Rejected:** token auth is the gate; CGNAT/asymmetric-NAT consumers legitimately connect from an un-offered source. Filtering would break NAT traversal. Recorded so a future "fix" cannot regress it. |

## What changed

**Wire (`src/shared.rs`):** `ClientMessage::ConnectSecret` gains `#[serde(default)] carrier: bool`.
Codec is `serde_json`, so the field is fully backward-compatible — an old client omits it ⇒ decodes
as `false` ⇒ legacy behaviour. Covered by `t_wire_secret_default_compat` (legacy decode defaults
false; full round-trip preserves `true`).

**Server (`src/secret.rs`, `src/server.rs`):** the dispatcher forwards `carrier` to `serve_consumer`;
when `true`, `serve_consumer` registers no admin entry, skips UDP brokering, and never reaps — but
still accepts and relays the carrier's data substreams (the carrier's whole purpose). The
`carrier == false` path is byte-identical to before.

**Direct path (`src/holepunch.rs`, `src/client.rs`):** `accept()` swallows benign strays internally.

**Resilience (`src/secret.rs`):** `relay()` fails over across live carriers; `--carriers`-on-direct warns.

**Observability (`src/admin.rs`, `src/secret.rs`):** admin entry register/drop logged at info (id, role,
peer, secret_id); carrier pool logs requested-vs-opened and warns on degradation; reaper logs peer;
direct accept logs the failing phase + peer.

**Frontend (`src/admin_ui/panels/secret.js`):** defensive dedup (D4) folds port-less carrier rows that
share a real consumer's peer IP into that consumer — so even an *older* server cannot produce spurious
rows. Two distinct consumers from the same host are preserved.

## Tests

- **Rust unit/integration** (`cargo test`, default + `--features vpn`, all green, zero regressions):
  - `shared::tests::t_wire_secret_default_compat` — wire back-compat + `carrier` round-trip.
  - `secret_consumer_carriers_make_one_admin_entry` (T-CARRIER1) — 4-carrier relay consumer ⇒ exactly
    1 admin entry, stays 1 over time, relay works.
  - `secret_consumer_carrier_not_reaped` (T-CARRIER2) — a silent carrier connection is heartbeated and
    never reaped past a short `ctrl_timeout` (inverse of `secret_consumer_reaped_when_control_wedges`).
  - All pre-existing secret/carrier/pool/reconnect/vpn tests unchanged and passing.
- **Frontend** (`npm test`, 72/72): `secret-carrier-dedup.test.js` (T-CARRIER-DEDUP) + existing suites.
- **e2e netns** (`scripts/secret_netns_test.sh`, run `sudo -n /abs/path/scripts/secret_netns_test.sh`
  after `cargo build --release`): T-SEC-SMOKE/LOAD/CARRIER-COUNT/NOSPURIOUS/CONFLICT/CHAOS/RECONNECT/
  NOZOMBIE/UDP-FALLBACK/MIXED/UDP-CLEANLOG. **Result: 29/29 PASS**, and the host is left exactly as
  before (idempotent startup reclaim + RAII trap; socat uses `PIPE` so no `cat` child can orphan even
  on SIGKILL mid-run). The open-error carrier failover (D5/BUG-S4) is validated here under real
  kill/restart churn (deterministic unit isolation isn't possible without exposing `Proxy`/seed-carrier
  internals). T-SEC-LOAD proves the core fix under load: 40 proxies (many `--carriers 4`) ⇒ exactly 40
  consumer rows, zero null-port rows. T-SEC-RECONNECT restarts the server to exercise real
  client-side `--auto-reconnect` (killing a client process cannot reconnect itself).

## Invariants established
- **I-2/I-3:** a secret consumer **carrier** creates no admin entry and is never reaped; liveness is
  the main control connection's job; one logical tunnel = exactly one admin row regardless of
  `--carriers`/transport.
- **I-5 (D7):** token-authenticated direct connections are accepted regardless of source address (no
  migration disable, no candidate-source filtering).
- The `carrier == false` path and every non-secret path remain byte-identical.
