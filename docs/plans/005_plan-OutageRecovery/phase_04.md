# Phase 3 — Web-transfer owner + SSH gateway

Intent:
- A web-transfer owner whose server path dies resumes its room within ≈ 16 s instead of ≈ 15 min, before the default 60 s owner grace ends it.
- The SSH gateway reaps a dead OpenSSH session in 15 s instead of 60 s.
- (Revision 1h) A browser tab in a room notices a dead control socket in ≈ 20 s and redials, and the server drops the ghost peer it left behind in ≈ 20 s instead of 60 s.

Superseded values (see STATE §8): `OWNER_HEARTBEAT` is **2 s**, not 5 s (revision 1b's arithmetic). `SSH_KEEPALIVE_INTERVAL` is **1 s** and the recommended OpenSSH options are `ServerAliveInterval=2 ServerAliveCountMax=7`, without `SessionType none` (revision 1f).

Prerequisites: P1.
Phase closure: P3; review by agent-1.

## State and ownership contract
Same as phase_01.md.

## Local design context
Plan revision 1.
- **D8.** Owner messages `CreateWebTransferRoom`/`ResumeWebTransferRoom` gain `#[serde(default)] ctrl_heartbeat_ms: u32`, set to `ctrl_heartbeat_declared_ms_for(OWNER_HEARTBEAT)`, i.e. `OWNER_HEARTBEAT` 5 s.
  - `serve_owner_control(lease, control, ctrl_timeout, liveness: OwnerLiveness)` where `OwnerLiveness { server_heartbeats: bool, transport: Option<(ConnActivity, Duration)> }`.
  - On every `WEB_TRANSFER_REAPER_TICK`, when `server_heartbeats`: send `ServerMessage::Heartbeat`. A send error returns as an EOF outcome. The send is bounded (`timeout(ctrl_heartbeat_send_timeout())`); a timeout is treated as a lost owner, never a wedge (P-9).
  - The transport reap → `timed_out: true` + terminate.
  - The owner (`heartbeat_phase`) tracks `last_server_msg` (updated on any successful recv) and `server_beats_seen` (set on `Heartbeat`).
  - A tick of `liveness_tick(deadline)` checks `server_beats_seen && elapsed >= deadline` → `warn!` + `Ok(HeartbeatEnd::Lost)`.
  - Old server: never sends `Heartbeat`, so the deadline is never armed and behaviour is byte-identical.
- **D9.** `src/sshgw.rs`: `SSH_KEEPALIVE_INTERVAL` 5 s, `SSH_CTRL_TIMEOUT` 15 s; `SSH_KEEPALIVE_MAX_MISSES` stays derived (= 2). Update the doc comments and the test that pins the values (`russh_config_wires_keepalive_reaper` + any pinning 60 s/20 s).
  - The `CLAUDE.md` I-SSH3 text ("keepalive 20s / reaper 60s") is updated in phase 4.3.

## Sub-phases

### 3.1 Web-transfer owner liveness
- **Files:** `src/shared.rs` (owner message fields); `src/web_transfer.rs` (`serve_owner_first_message`, `serve_owner_control`); `src/server.rs` dispatch (activity + floor); `src/web_transfer_cli.rs` (`OWNER_HEARTBEAT`, `heartbeat_phase`, Create/Resume construction).
- **Tests:**
  - `owner_control_sends_heartbeats_only_when_declared` (duplex, generic S; declared → ≥ 2 within 1.2 s; not → 0);
  - `owner_control_transport_reaps_silent_declared_owner` (mux pair; floor 300 ms);
  - `heartbeat_phase_trips_after_server_beats_stop` (duplex fake server sends 2 heartbeats then silence; silence env 400 ms → `Lost` within 1.5 s);
  - `heartbeat_phase_never_trips_without_server_beats` (legacy server: silence 1.5 s with env 400 ms → still running);
  - serde defaults for the new fields;
  - update `assert_eq!(OWNER_HEARTBEAT, 20 s)` → 5 s.
- **Done:** committed.

### 3.2 SSH gateway keepalive
- **Files:** `src/sshgw.rs` constants + docs + tests.
- **Tests:** the pinned config test reads `keepalive_interval == 5 s`, `keepalive_max == 2`. `cargo test --features ssh-gateway --test ssh_gateway_test` passes (it includes the I-SSH10 SIGSTOP pair, which must still evict).
- **Done:** committed.

### 3.3 README + SSH docs
- **Files:**
  - `README.md` web-transfer and SSH gateway sections: owner liveness; gateway reap 15 s; the recommended OpenSSH options `-o ServerAliveInterval=5 -o ServerAliveCountMax=4 -o ExitOnForwardFailure=yes` and autossh `-M 0` with `AUTOSSH_GATETIME=0`.
  - `docs/ssh-gateway/SSH_GATEWAY.md` §6 matching text.
  - `README-SSH-GATEWAY.md`, if it carries example commands.
- **Done:** committed.

### 3.4 Browser control-socket liveness (revision 1h)
- **Why:** the browser peer was the one mode the plan did not cover. A tab whose path died (IP change) waits for the browser to notice the dead TCP connection, which takes minutes, and the server keeps its ghost peer — with its offers — for 60 s. JS timers cannot carry a short deadline on their own: Chrome throttles a hidden tab's timers to one a minute.
- **Server (`src/web_transfer_http.rs` `serve_control_websocket`, `src/web_transfer.rs`):**
  - Send `Message::Ping` every `WEB_TRANSFER_PEER_WS_PING` (1 s), bounded by `WEB_TRANSFER_CTRL_SEND_TIMEOUT` (P-9); the first one a full period after the loop starts (`interval_at`), so it never races the welcome/snapshot frames. The browser's network stack answers below page script, so a throttled tab still answers.
  - `Message::Pong` sets `pong_seen` and refreshes `last_recv` (any inbound frame already did).
  - On the reaper tick: `peer_session_expired(last_recv, now, pong_seen, registry.peer_transport_timeout())` = legacy 60 s window OR (`pong_seen` AND silence ≥ `WEB_TRANSFER_PEER_TRANSPORT_TIMEOUT` 20 s). A session that never answered a Ping keeps 60 s (DEC-VE2). Wire unchanged.
  - Registry seam `set_peer_transport_timeout` (the `set_direct_deadline` pattern). Fix the stale docs of `WEB_TRANSFER_CLIENT_HEARTBEAT` and `WEB_TRANSFER_PEER_PING`.
- **Browser (`web/transfer/src/control.js`):** `PING_INTERVAL_MS` 5 s, `PONG_DEADLINE_MS` 20 s. Pure `pingTickDecision`: a tick gap > 2 × interval (throttled tab) resets the outstanding clock, so a throttled tab never counts its own stall as silence. `abandon(code)` detaches the socket, best-effort closes it, reports `onClose(code, false)` and redials at once — never waiting for a `close` event a dead socket only fires after the closing-handshake timeout. The hello timeout and `cycle()` use the same path. Every listener ignores a socket that is no longer current. Rebuild `dist/` (`npm run build`; `build.rs` embeds it, so `cargo build` follows).
- **Tests:** Rust unit `peer_session_expiry_is_gated_on_an_answered_ping`; integration `a_browser_that_stops_answering_pings_leaves_on_the_transport_deadline` (red-check: Ping arm removed → B leaves only after 60 s); JS `ping_tick_decision_table`, `an_unanswered_ping_abandons_a_dead_socket_and_redials`, `answered_pings_keep_the_session`, `a_hello_timeout_redials_without_waiting_for_close`, `cycle_redials_without_waiting_for_close`, `stop_silences_the_closed_socket`; `npm run test:unit` (incl. `build_is_reproducible`); Playwright reconnect specs after `cargo build`.
- **Docs:** README "Browser transfer rooms" paragraph + the web-transfer drop bullet; `docs/transfer/WEB_TRANSFER_PROTOCOL.md` §8 bounds and §12 cleanup matrix.
- **Done:** committed.

## Phase gates and closure
- G-P3: G-P1 + `cargo test --features ssh-gateway` + `sudo -n scripts/ssh_gateway_test.sh` (serial).
- P3 self-review: the owner close path is unchanged; a legacy owner/server pair is byte-identical.
