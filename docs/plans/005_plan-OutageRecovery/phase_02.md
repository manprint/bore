# Phase 1 — Public, vhost, secret, ssh-jump (+ transfer, transfer-link)

Intent: every `Client::listen` role and the secret consumer detect a dead server path within 15 s. The server frees a declared client's registration within 15.5 s of its last byte. A public tunnel with port 0 gets its port back.
Prerequisites: P0 (`ConnActivity`, `src/liveness.rs`, backoff cap).
Phase closure: P1; review by agent-1.

## State and ownership contract
Same as phase_01.md: STATE.md first; open/checkpoint/close; commit with the PEV trailers; missing design goes to the supervisor.

## Local design context
Plan revision 1.
- **D2 (client).** On a tick of `liveness_tick(deadline)`, if `activity.inbound_idle() >= deadline`:
  1. `warn!(idle_ms, deadline_ms, "no data from the server for {deadline:?}; the connection is dead — reconnecting")`;
  2. `terminate()` the main connection and every carrier connection;
  3. `return Err(anyhow!("server silent for {idle:?} (connection lost)"))`.
  - `None` deadline means the arm is disabled. Use `std::future::pending` via an `Option` interval helper.
- **D3 (server).** On the EXISTING heartbeat tick, right after the legacy check:
  ```rust
  if activity.reap_due(transport_deadline) {
      warn!(... "control transport silent; reaping (peer gone)");
      activity.terminate();
      return Ok(());
  }
  ```
  - `transport_deadline: Option<Duration>` is computed in `Server::handle_connection` as `liveness::transport_reap_deadline(declared, self.transport_reap_floor)`.
  - An undeclared client gets `None`, i.e. the byte-identical legacy path (I-5).
- **D3 (client declares).** `ctrl_heartbeat_ms = liveness::ctrl_heartbeat_declared_ms()` exactly where `sends_ctrl_heartbeat` is set true.
  - Carriers (`ConnectSecret { carrier: true }`, `JoinCarrier`) declare 0.
  - A client that sets `sends_ctrl_heartbeat = false` declares 0.
- **D7.** Sticky public port (overview).
- **D10.** Secret consumer heartbeat through `client::beat_once`.
- **P-9.** On `CtrlBeat::PeerNotReading` the client stops beating. A declared client that stopped beating may then be transport-reaped by a new server. That is correct: the server stopped reading for 10 s.
- **W-1.** None of the touched structs is re-serialized by the server.
- **I-2.** A silent server is detected within deadline + 1 tick.
- **I-3.** A short flick never disconnects.
- **I-4.** A declared client whose transport goes idle is reaped and its name freed.
- **I-5.** An undeclared client is never transport-reaped.

## Sub-phases

### 1.1 Wire fields + declarations
- **Model:** agent-1 — protocol unit; supervisor (self) reviews after the diff.
- **Files:** WRITE `src/shared.rs`:
  - `TunnelOptions { ctrl_heartbeat_ms: u32, preferred_port: Option<u16> }`;
  - `ClientMessage::{HelloVhost, HelloSecret, ConnectSecret, HelloSshJump}` gain `ctrl_heartbeat_ms: u32`.
  - All of them are `#[serde(default)]`; every struct literal and pattern is updated (compiler-guided; patterns use `..` where they already do).
  - WRITE `src/client.rs` constructors and `src/secret.rs` `Proxy::new` plus the carrier dial, to set the field per D3.
- **Change:**
  1. S1 — Add the fields. Expected: `cargo check --all-targets --features vpn,ssh-gateway` passes after fixing literals.
  2. S2 — Set the declarations in each constructor: `Client::new` (public), `new_secret_provider*`, `new_vhost_provider*`, the ssh-jump constructor, `Proxy::new`. Carriers declare 0.
- **Unit tests** (`src/shared.rs` tests):
  - `tunnel_options_new_fields_default_when_absent` (deserialize legacy JSON → 0/None);
  - `hello_vhost_ctrl_heartbeat_ms_defaults_zero`, `hello_secret_…`, `connect_secret_…`, `hello_ssh_jump_…`;
  - round-trips with a nonzero value.
- **e2e:** N/A (covered in 1.2/1.3).
- **Done:** gates green; committed.

### 1.2 Server transport reapers
- **Model:** agent-1 — lifecycle; self-review the RAII release on every reap path.
- **Files:**
  - WRITE `src/server.rs`: field + builder `transport_reap_floor`, default `liveness::TRANSPORT_REAP_FLOOR`; `handle_connection` computes the deadline per message and passes `(opener.activity(), deadline)`; `serve_tunnel` gets the check.
  - WRITE `src/secret.rs` `serve_provider` and `serve_consumer` (only `!carrier`).
  - WRITE `src/vhost.rs` `serve_vhost_provider`.
  - WRITE `src/ssh_jump.rs` `serve_native_provider`.
  - Signatures gain `transport: Option<(mux::ConnActivity, Duration)>`.
- **Change:** steps per file. Each check sits in the heartbeat-tick arm after the legacy deadline check. Callers in tests are updated (`None`).
- **Unit/integration tests** (NEW `tests/outage_liveness_test.rs`, gate G-U12, one binary, a file-local `static SERIAL: Mutex<()>`):
  - The fixture `BlackholeProxy` (in the test file) is a TCP forwarder `client ↔ proxy ↔ server` with `set_blackhole(bool)`. While blackholed it stops reading both sides and discards nothing (bytes stay in kernel buffers), which emulates a dead path while TCP stays ESTABLISHED.
  - `server_transport_reaps_a_silent_declared_vhost_provider`: env `BORE_CTRL_HEARTBEAT_MS=200`, `BORE_CTRL_SERVER_SILENCE_MS=0` (the client must NOT trip, isolating the server); server `transport_reap_floor(1 s)`; register vhost label through the proxy, blackhole, then within 4 s a direct second client registers the SAME label successfully.
  - Same shape for: public fixed port (`server_transport_reaps_a_silent_declared_public_tunnel`, the port is grantable again); secret provider id; secret consumer (admin row gone); ssh-jump alias, if a native jump registration is constructible in-process — else covered by netns T-OUT.
  - `undeclared_client_is_never_transport_reaped` (I-5): raw protocol client sends `Hello(port, opts)` with `ctrl_heartbeat_ms` absent (legacy JSON), sends nothing else for 3 s with floor 1 s; the port stays held (a second request is refused).
  - Red-check: deleting one reaper check makes its test fail (a second registration is refused `in use`).
- **e2e:** phase 4 T-OUT-IPCHANGE.
- **Done:** gates green; committed.

### 1.3 Client::listen deadline (+ carriers)
- **Model:** agent-1 — lifecycle; self-review.
- **Files:** WRITE `src/client.rs` `Client::listen`: hold `main_activity = acceptor.activity()` and carrier activities (`carrier_acceptors` before pumping + every re-dialed carrier). Add the liveness arm.
- **Change:**
  1. S1 — `let silence = liveness::client_silence_deadline();` plus a tick `interval(liveness_tick(d))` (Delay missed-tick) or a never-ready future when `None`.
  2. S2 — The arm `_ = tick` does the D2 check against `main_activity`. On trip, terminate main + carriers and return `Err`.
  3. S3 — Collect carrier `ConnActivity` into a `Vec`, pushing on every successful (re-)dial.
  - Expected: listen still returns `Ok` on server close as before.
- **Integration tests** (`tests/outage_liveness_test.rs`):
  - `public_client_returns_when_the_server_goes_silent`: env silence 1500 ms, heartbeat 200 ms; blackhole → `listen()` returns `Err` within 4 s (`timeout` 6 s); the error text contains "silent". Same for `vhost_client_…` and `secret_provider_client_…`.
  - `client_survives_a_short_flick` (I-3): silence 3000 ms; blackhole 1 s, release; listen still running 4 s later and a proxied request succeeds.
  - `client_terminates_carrier_connections_on_trip`: public `--carriers 2`; after the trip, the proxy observes all its upstream sockets closed within 3 s.
  - Red-check: without the arm, the first test times out (the production symptom).
- **e2e:** phase 4.
- **Done:** gates green; committed.

### 1.4 Secret consumer (Proxy) deadline + beat_once
- **Model:** agent-1
- **Files:** WRITE `src/secret.rs`: a `Proxy` field `ctrl_activity: mux::ConnActivity` captured from the opener in `Proxy::new` before it moves into the pool; carrier activities from the carrier dial (around line 1932); `listen`: liveness arm + heartbeat arm via `crate::client::beat_once` (`Sent` → continue, `Closed` → `return Ok(())`, `PeerNotReading` → warn once, stop beating).
- **Tests:**
  - `secret_consumer_returns_when_the_server_goes_silent` (as in 1.3);
  - `secret_consumer_heartbeat_uses_bounded_write` — unit at the `beat_once` level is already pinned; add an integration assertion that consumer beats arrive at the server (admin `last` field or a server-side counter if exposed; else a fake server counting `Heartbeat` frames on a raw control stream).
- **Done:** gates green; committed.

### 1.5 Public sticky port
- **Model:** agent-1
- **Files:**
  - WRITE `src/server.rs` `serve_tunnel` / `create_listener`: when `port == 0` and `opts.preferred_port = Some(p)` with p in range → try `bind_public_listener(p)` first; on `Err` fall back.
  - WRITE `src/client.rs` `Client::new`: an optional preferred port parameter or a setter.
  - WRITE `src/main.rs` `bore local` connect closure: an `Arc<AtomicU16>` remembered across attempts, set after a successful connect from `client.remote_port()`.
- **Tests:**
  - `public_port_zero_reconnect_gets_the_same_port`: first client port 0 → P; drop it; the server frees P; the second client with `preferred_port=Some(P)` gets P.
  - `preferred_port_out_of_range_is_ignored`.
  - `preferred_port_taken_falls_back_to_random`.
- **Done:** gates green; committed.

### 1.6 README
- **Files:** `README.md` — a new subsection "Connection liveness and outage recovery" near `--auto-reconnect`:
  - defaults (5 s heartbeat, 15 s silence, 15 s server reap, 8 s backoff cap);
  - the env knobs;
  - stability (flicks < ~12 s survive);
  - the upgrade note (both sides for the full benefit);
  - the sticky public port.
- **Done:** README accurate; committed.

## Phase gates and closure
- G-P1: fmt; clippy default + `--features vpn,ssh-gateway`; full `cargo test`; `cargo test --test outage_liveness_test` ≥ 10 tests run.
- P1 self-review: every reap path still releases registration RAII; carriers and consumer carriers are not reaped.
