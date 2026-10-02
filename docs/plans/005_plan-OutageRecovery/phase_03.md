# Phase 2 — VPN (1:1 and hub)

Intent:
- VPN clients detect a dead server path within 15 s, in the waiting phase (new server) and after pairing (any server).
- The server reaps declared VPN clients within 15.5 s.
- A pairing never outlives either side.

Prerequisites: P1.
Phase closure: P2; review by agent-1.

## State and ownership contract
Same as phase_01.md.

## Local design context
Plan revision 1.
- **D6a (server, waiting 1:1 listener).**
  - If `ctrl_heartbeat_ms > 0`, the waiting `select!` gains:
    - a 500 ms `hb` arm that sends `ServerMessage::Heartbeat` and checks `reap_due`;
    - a recv arm in which `Ok(Some(ClientMessage::Heartbeat))` continues and everything else returns as before.
  - If `== 0`, the original two-arm select is byte-identical.
  - Reason: an old client (`run_listen_once`) bails on any message other than `VpnReady`/errors while waiting.
- **D6b.** `ServerMessage::VpnReady` gains `#[serde(default)] ctrl_heartbeat: bool`.
  - The server sets it to `declared > 0` for listener_ready (1:1 + hub), connector_ready and the hub spoke ready.
  - Clients beat after pairing iff it is true.
  - A waiting listener beats iff it has received ≥ 1 `Heartbeat` while waiting.
  - I-9: never beat at a server that has not proven it can decode `ClientMessage::Heartbeat`.
- **D6c.** The paired 1:1 listener, hub listener, 1:1 connector and hub spoke loops add `activity.reap_due(transport_deadline)` → terminate + break on their 500 ms heartbeat tick.
  - Client `Heartbeat` frames already fall into the `_ => {}` arms of the paired loops. Verify per loop, and add an explicit arm when `_` is absent.
- **D6d (pairing teardown).**
  - `VpnPairMsg` gains `cancel: CancellationToken`, created by the connector handler at pairing.
  - The connector handler keeps `cancel.clone().drop_guard()`; the listener handler keeps its own `drop_guard` of the received token.
  - Each paired loop adds `_ = cancel.cancelled() => break`.
  - Hub: `HubShared` gains `cancel: CancellationToken`. The hub listener handler holds its `drop_guard` for the hub's life, and every spoke loop adds `_ = hub.cancel.cancelled() => break`.
  - Ordering: the drop guard is created BEFORE the first await after pairing, so no exit path is missed.
- **D6e (client).**
  - `spawn_ctrl_actor(ctrl, activity, beats: bool)`: the `timeout(CTRL_HEARTBEAT_TIMEOUT, recv)` becomes a plain `recv` plus a tick arm.
    - The tick checks `activity.inbound_idle() >= client_silence_deadline()`; on a trip it terminates and returns the error `"no data from the vpn server for …"`.
    - When `beats`, the tick also beats every `ctrl_client_heartbeat()` via `beat_once`, routed inside the actor (the actor is the stream's single owner, I-7).
  - The hub ctrl actor gets the same change.
  - Listener waiting phase (`run_listen_once`): the single `recv` becomes a loop until `VpnReady`/error.
    - `Heartbeat` → `seen_hb = true`.
    - tick: if `seen_hb` and idle ≥ deadline → bail with a retryable error; if `seen_hb`, beat on schedule.
  - The connector has no waiting phase.
  - Remove `CTRL_HEARTBEAT_TIMEOUT` once it is unused; else keep it documented.
- **D5** already applied to `run_with_reconnect` (P0).

## Sub-phases

### 2.1 Wire
- **Files:** `src/shared.rs`: the `HelloVpn`/`ConnectVpn` `ctrl_heartbeat_ms`, the `VpnReady.ctrl_heartbeat`; all literals and patterns.
- **Tests:** serde defaults when absent (legacy JSON) + round-trip.
- **Done:** gates (incl. `--features vpn`) green; committed.

### 2.2 Server: waiting heartbeats + transport reapers + VpnReady flag
- **Files:** `src/vpn_server.rs` (`serve_vpn_listener`, `serve_vpn_connector`, hub paths); `src/server.rs` dispatch passes the deadline and activity.
- **Tests** (`src/vpn_server.rs` tests or NEW `tests/vpn_liveness_test.rs`; in-process `Server` with `--vpn`, no TUN needed for the server side, fake clients speaking raw protocol over `mux::client` + `Delimited`):
  - `waiting_listener_gets_heartbeats_only_when_declared`: declared → ≥ 2 `Heartbeat` within 1.5 s while waiting; undeclared → none in 1.5 s.
  - `waiting_listener_accepts_client_heartbeats_when_declared`: send `Heartbeat` while waiting → the entry is still registered (a second listener with the same id is refused `already in use`).
  - `declared_waiting_listener_is_transport_reaped`: floor 1 s, the fake client goes silent (no reads/writes; TCP alive through `BlackholeProxy`) → the id is free within 3 s.
  - `vpn_ready_carries_ctrl_heartbeat_flag`: declared → true; undeclared → false.
- **Done:** committed.

### 2.3 Server: pairing teardown
- **Files:** `src/vpn_server.rs` (`VpnPairMsg`, both 1:1 loops, `HubShared`, the hub listener, spoke loops).
- **Tests:**
  - `vpn_pair_teardown_listener_exit_closes_connector`: pair fake listener + fake connector; drop the listener's connection → the connector's control yields `None` within 3 s.
  - `vpn_pair_teardown_connector_exit_closes_listener`: the reverse.
  - `vpn_hub_exit_closes_spokes`: hub (`max_clients 4`) + 2 spokes; drop the hub → both spokes' controls end within 3 s.
  - Red-check: without the cancel arms the tests time out.
- **Done:** committed.

### 2.4 Client: deadline + gated beats
- **Files:** `src/vpn.rs` (`run_listen_once` waiting loop, `spawn_ctrl_actor`, hub ctrl actor, connector call site).
- **Tests** (`src/vpn.rs` `mod tests`, mux pair over a TCP loopback as at the existing `vpn.rs` tests ~9804):
  - `ctrl_actor_trips_when_the_server_goes_silent`: silence env 600 ms; the fake server sends nothing after setup → the actor returns an error containing "no data" within 2 s.
  - `ctrl_actor_survives_while_server_heartbeats`: the fake server heartbeats every 100 ms for 1.5 s → no error.
  - `ctrl_actor_never_beats_when_not_allowed` (I-9): beats=false, heartbeat env 50 ms → the fake server receives 0 `ClientMessage` in 500 ms.
  - `ctrl_actor_beats_when_allowed`: beats=true → ≥ 3 frames in 500 ms.
  - Waiting-phase logic is extracted to a testable fn `await_vpn_ready(ctrl, activity) -> Result<ServerMessage>`, with tests: legacy server (no heartbeat, sends `VpnReady` after 300 ms) → ok and no client frame was sent; new server (heartbeats then `VpnReady`) → ok and ≥ 1 client heartbeat seen; new server going silent after heartbeats → `Err` within deadline.
- **Done:** gates `--features vpn` green; existing `vpn` unit tests pass; committed.

### 2.5 README + docs
- **Files:** `README.md` VPN reconnect/liveness lines; `docs/vpn/` file only if it states the 60 s heartbeat timeout (grep `60 s`/`CTRL_HEARTBEAT_TIMEOUT`).
- **Done:** committed.

## Phase gates and closure
- G-P2: G-P1 + `cargo test --features vpn` + `sudo -n scripts/vpn_netns_test.sh` (serial, release `--features vpn` build) with no regressions.
- P2 self-review: the I-MC1 hub branch is still separate; the D5 generation guard is intact; the waiting legacy path is byte-identical for undeclared.
