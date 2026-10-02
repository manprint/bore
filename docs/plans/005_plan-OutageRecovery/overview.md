# Outage recovery (all modes) — Plan overview

Authored 2026-10-02 by claude-opus-5-5 (strong supervisor and sole implementer).
Folder: `docs/plans/005_plan-OutageRecovery/`.
Execution starts at [STATE.md](STATE.md). All live readiness and progress are recorded there.

## Goal and reference scenario

**Field report (2026-10-02).** The client was `bore vhost 127.0.0.1:5000 --subdomain dufs-pcloud --udp --auto-reconnect --carriers 4` in docker.
- The network dropped at about 01:03:02 for about 20 s; the ISP probably changed the public IP.
- The client reconnected at 01:19:25. That is about **16 minutes** of downtime.

**Requirements** (user, verbatim intent):
- "non può andare down per minuti se la connessione non va per 20 secondi" — a 20 s outage must never cost minutes of downtime.
- This applies to **every** mode: vhost, public, secret provider/consumer, ssh-jump, VPN 1:1 and hub, transfer, transfer-link, web-transfer and the SSH gateway.
- Every change is covered by tests.
- "massima stabilità": brief network flicks must **not** cause disconnections.

**Reference scenarios.** The netns gate `T-OUT-*` in `scripts/outage_netns_test.sh` uses shipped defaults and no env overrides. A shared tunnel set exercises all modes at once.

| ID | Network event | Required observation |
|----|---------------|----------------------|
| T-OUT-FLICK | 4 s blackhole of the client site, same IP | No mode reconnects (one `connected` per client); a rate-limited download spanning the flick completes byte-exact |
| T-OUT-OUTAGE | 20 s blackhole, same IP | Every mode serves again within 15 s of restoration |
| T-OUT-IPCHANGE | 20 s blackhole, then the client's source address changes and the old address stays blackholed forever | Every mode serves again within 15 s of restoration; a fixed `--port` public tunnel gets its port back |
| T-OUT-IPCHANGE0 | Instant source-address change (no outage) | Every mode serves again within 25 s |

On baseline `f7b0745` T-OUT-IPCHANGE does not recover inside its window. That is the red-check: recovery took ≈15 min, i.e. `tcp_retries2`.

**Exclusions.**
- IPv6 traversal.
- Re-architecting carriers.
- Making an unmodified OpenSSH client detect a dead server. That is OpenSSH configuration and gets documented only.
- `bore test-udp` (a short-lived diagnostic with no reconnect loop).

## Root cause (verified in B1)

1. **No client-side liveness deadline.** `Client::listen` (`src/client.rs`) reads the server's 500 ms heartbeats but never notices when they *stop*.
   - After an IP change the old TCP connection is silently dead.
   - The kernel declares it dead only after `tcp_retries2` (≈924 s, R1), because the client's own heartbeats keep unacked data in flight.
   - Unacked data means SO_KEEPALIVE never fires.
   - Measured downtime of ≈16 min = 924 s + backoff.
   - The same shape exists in:
     - `secret::Proxy::listen` (consumer);
     - the web-transfer owner (`heartbeat_phase`);
     - the VPN ctrl actors, which use a 60 s `timeout(recv)` that every outbound send resets.
2. **The server holds the zombie registration** (subdomain / fixed port / alias / secret id / VPN id) until its 60 s control reaper. VPN has no reaper at all. A prompt reconnect is therefore rejected `in use`.
3. **VPN pairing is not torn down.** When one side of a 1:1 pairing (or the hub) dies, its partner's server handler keeps heartbeating its client, so the partner never re-pairs.
4. **Secondary cost: reconnect backoff caps at 32 s.** After a long outage this adds up to 32 s of extra downtime.

The field log's looping "re-dialed a carrier connection" follows from (1) and (2):
- The server reaped the tunnel and forgot its carrier token.
- The zombie main connection kept the client alive.
- Fixing (1) ends the loop within one deadline, so D-R1 records the carrier change as unnecessary.

## Decisions

| ID | Decision and consequence | Authority/source | Supersedes |
|----|--------------------------|------------------|------------|
| D1 | **Liveness is measured at the TRANSPORT, not on control frames.** `mux` wraps every socket in an activity stamp updated by every successful non-empty `poll_read` of the underlying transport. `ConnActivity::inbound_idle()` is the time since the last byte arrived from the peer, on any substream, including yamux frames. A connection carrying bulk data in either direction is therefore never judged dead because its control frames queue behind data, which honours "massima stabilità". A dead path reads nothing. | supervisor 2026-10-02; user stability requirement | — |
| D2 | **Client server-silence deadline.** Every long-lived client loop checks `inbound_idle() >= client_silence_deadline()` on a tick of `clamp(deadline/4, 50 ms, 1 s)`, never with `timeout(recv)` (DEC-VE3). On a trip it logs one `warn!`, calls `terminate()` on the main and every carrier mux connection, then returns `Err`, so `--auto-reconnect` reconnects. Default **15 s**: 30 missed server heartbeats; flicks up to ≈12 s at LAN RTT survive with zero disconnection. Env `BORE_CTRL_SERVER_SILENCE_MS` overrides it per call; `0` disables. Applies to `Client::listen`, `secret::Proxy::listen`, the VPN ctrl actors after `VpnReady`, and the VPN listener waiting phase once the server is seen to heartbeat there. Every server role loop already heartbeats every 500 ms, upstream bore included, so no capability is needed except for the VPN waiting phase (D6) and web transfer (D8). | supervisor 2026-10-02 | VPN `CTRL_HEARTBEAT_TIMEOUT` 60 s `timeout(recv)` |
| D3 | **Declared client heartbeat plus a fast server transport reaper.** `CTRL_CLIENT_HEARTBEAT` drops from 20 s to **5 s**, still overridable by `BORE_CTRL_HEARTBEAT_MS`. The interval is declared in a new additive `#[serde(default)] ctrl_heartbeat_ms: u32` on `TunnelOptions`, `HelloVhost`, `HelloSecret`, `ConnectSecret` (0 on carriers), `HelloSshJump`, `HelloVpn`, `ConnectVpn`, `CreateWebTransferRoom` and `ResumeWebTransferRoom`. The server derives `transport_reap_deadline(declared) = None if 0, else max(3 × declared, floor)` with floor **15 s**, set by the builder `Server::transport_reap_floor(d)` for tests. Each registry loop reaps on its existing heartbeat tick when `inbound_idle() >= deadline`, then calls `terminate()` on that connection. The legacy 60 s control-message reaper is unchanged and still runs. A client that declares nothing (0) is never transport-reaped (DEC-VE2 shape). | supervisor 2026-10-02 | — |
| D4 | **Terminate is used only on liveness trips.** `ConnActivity::terminate()` cancels the driver task. Dropping the yamux `Connection` closes and wakes every stream (R2), so in-flight splices end at once instead of after `tcp_retries2`, and the descriptor is released. Clean exits and legacy reaps keep their current graceful `poll_close`, a byte-identical path. | supervisor | — |
| D5 | **Reconnect backoff cap 32 s → 8 s** (`reconnect::DEFAULT_MAX_BACKOFF_SECS`). This covers `reconnect::run`, the VPN `run_with_reconnect` and transfer-link. One connect attempt per 8 s per client is negligible load, and recovery after a long outage is ≤ 8 s instead of ≤ 32 s. | supervisor | — |
| D6 | **VPN liveness and teardown.** |  |  |
|  | (a) The server heartbeats a WAITING 1:1 listener every 500 ms, accepts `ClientMessage::Heartbeat` while waiting, and transport-reaps it. All of this happens only when `HelloVpn.ctrl_heartbeat_ms > 0`; otherwise the select is byte-identical legacy (an old client bails on an unexpected message while waiting). |  |  |
|  | (b) `VpnReady` gains additive `ctrl_heartbeat: bool`, set iff the client declared > 0. The client beats after pairing only when it is true. A waiting listener beats once it has received a server `Heartbeat`. An old server cannot decode `ClientMessage::Heartbeat` / treats any message while waiting as a disconnect, so beats are never sent to it. |  |  |
|  | (c) The paired, hub and spoke loops transport-reap a declared client. |  |  |
|  | (d) **Pairing teardown.** A 1:1 pairing carries a `CancellationToken` in `VpnPairMsg`; each handler holds a `drop_guard` and breaks on `cancelled()`, so either side's exit releases the other. The hub's token lives in `HubShared` and every spoke handler breaks when the hub handler exits. Clients then reconnect and re-pair. |  |  |
|  | (e) The client deadline (D2) replaces the 60 s `timeout(recv)` in `spawn_ctrl_actor` and the hub ctrl actor. | supervisor | 1:1 waiting select (declared case only) |
| D7 | **Public sticky port.** A public client that asked for port 0 records the server-assigned port in a shared `Arc<AtomicU16>`. Reconnect attempts send it as additive `TunnelOptions.preferred_port: Option<u16>`. The server tries that port first, but only when the request is 0 and the port lies inside `--min-port..=--max-port`; on any bind failure it falls back to the random allocation. An old server ignores the field. This creates no new capability: a client can already request any explicit port. | supervisor | — |
| D8 | **Web-transfer owner.** If the owner declares `ctrl_heartbeat_ms > 0`, `serve_owner_control` sends `ServerMessage::Heartbeat` every 500 ms (`WEB_TRANSFER_REAPER_TICK`) and transport-reaps by D3. `OWNER_HEARTBEAT` drops from 20 s to 5 s. The owner's `heartbeat_phase` trips on **message-level** silence ≥ `client_silence_deadline()`. That deadline is armed only after the first server `Heartbeat`, so an old server that never heartbeats keeps legacy behaviour. Message level is exact here because the owner connection is dedicated (one control stream, no data), and it keeps the function generic over `S` and unit-testable. A trip returns `HeartbeatEnd::Lost`, so the existing resume ladder runs. | supervisor | — |
| D9 | **SSH gateway.** `SSH_KEEPALIVE_INTERVAL` 20 s → **5 s** and `SSH_CTRL_TIMEOUT` 60 s → **15 s** (`keepalive_max` = 2). russh resets `alive_timeouts` on ANY received data (R3), so this is already transport-level liveness: congestion-safe and parity with D3. OpenSSH clients must configure `ServerAliveInterval=5 ServerAliveCountMax=4 ExitOnForwardFailure=yes` (R4) — README + `docs/ssh-gateway/SSH_GATEWAY.md`. | supervisor | I-SSH3 60 s figure |
| D10 | **Secret consumer heartbeat via `client::beat_once`** (P-9), replacing the bare `control.send(Heartbeat)` in a `select!` arm. | CLAUDE.md P-9 | — |
| D-R1 | **Rejected: making a carrier-token rejection fatal.** The field loop is a consequence of the zombie main connection, which D2/D3 end within one deadline. A healthy main connection with an unknown token is unreachable: the token lives exactly as long as the registration. No code change, no test. | supervisor | — |
| D-R2 | **Rejected: an identity/resume-token takeover of zombie registrations.** It would change the trust model and is unnecessary once D3 reaps in ≤ 15.5 s and the client deadline is ≥ 15 s. The residual race (client reconnects before the server reaps) costs one 1 s backoff retry. | supervisor (earlier in session) | — |
| D-R3 | **Rejected: `TCP_USER_TIMEOUT` / lower `TCP_KEEPCNT` in `tune_tcp`.** `TCP_USER_TIMEOUT` also aborts a zero-window peer (a browser pausing a download on a public socket), it is Linux-only, and D2 already bounds detection portably. | supervisor | — |
| D-R4 | **Rejected: control-message silence on the client.** Server heartbeats queue behind bulk data in the server's socket buffer (≈4 MiB at 1 Mbit/s ≈ 32 s), so a message-level deadline would kill healthy congested tunnels. | supervisor | — |

## Open questions

None. All product requirements were given by the user; every technical choice above lies inside the delegated authority.

## Architecture

```
            ┌───────────── mux::spawn_driver_inner ─────────────┐
socket ──► ActivityIo<S> (stamps last_inbound on read) ──► yamux::Connection
            │   Activity { base, last_inbound_ms, cancel }  ◄── ConnActivity (Clone)
            │   driver: select { drive(..), cancel.cancelled() }  (terminate = cancel)
            └───────────────────────────────────────────────────┘
Opener::activity() / Acceptor::activity() -> ConnActivity
```

- **Client loops** keep `ConnActivity` handles for the main connection plus every carrier, and run D2 on a liveness tick.
- **Server** (`Server::handle_connection`): computes `transport_reap_deadline(declared)` per message, then passes the `Option<Duration>` and `opener.activity()` into each `serve_*`. Those check on their existing heartbeat tick and terminate on a trip.
- **VPN pairing**: a `CancellationToken` is shared between the paired handlers (D6d).
- **Policy home**: a NEW module `src/liveness.rs` holds the constants, env resolvers and pure functions, so every role uses one implementation.

## Interfaces and compatibility

| Surface | Exact name/type | Default | Errors/conflicts | Compatibility |
|---------|-----------------|---------|------------------|---------------|
| mux API | `ConnActivity { inbound_idle() -> Duration, terminate(), is_terminated() -> bool }`, `Opener::activity()`, `Acceptor::activity()` | — | — | additive |
| env (client) | `BORE_CTRL_SERVER_SILENCE_MS` | 15000 | unparsable → default; 0 → disabled; else clamp [100, 3_600_000] ms | new |
| env (client) | `BORE_CTRL_HEARTBEAT_MS` (existing) | 5000 (was 20000) | unchanged parse | value change only |
| wire C→S | `ctrl_heartbeat_ms: u32` on 9 messages (D3) | 0 when absent | — | `#[serde(default)]`; old server ignores; never re-serialized by the server (W-1 safe) |
| wire C→S | `TunnelOptions.preferred_port: Option<u16>` | None | outside range / bind fail → random | additive |
| wire S→C | `VpnReady.ctrl_heartbeat: bool` | false | — | additive field on an existing variant: old client ignores unknown fields (no `deny_unknown_fields` anywhere, verified) |
| wire S→C | `ServerMessage::Heartbeat` to a waiting VPN listener / web-transfer owner | only when declared | — | gated by the declaration |
| server builder | `Server::transport_reap_floor(Duration)` | 15 s | — | test/tuning hook |
| constants | `reconnect` cap 8 s; `SSH_KEEPALIVE_INTERVAL` 5 s; `SSH_CTRL_TIMEOUT` 15 s; `OWNER_HEARTBEAT` 5 s | — | — | behaviour change, documented |

**Upgrade order.** Server-first is not required.
- A new client against an old server: transport deadline (works: old server heartbeats), no fast server reap (old server, 60 s); public/vhost/secret recover in ≈ 60 s + 1 RTT instead of 15 min.
- A new server with an old client: a 60 s legacy reaper, unchanged.
- Full benefit needs both upgraded. README states this.

## Phase map

| Logical phase | File | Sub-phase IDs | Depends on | Assignment |
|---------------|------|---------------|------------|------------|
| 0 Liveness primitives | [phase_01.md](phase_01.md) | 0.1, 0.2, 0.3, 0.4 | none | agent-1 |
| 1 Public/vhost/secret/ssh-jump | [phase_02.md](phase_02.md) | 1.1 … 1.6 | P0 | agent-1 |
| 2 VPN | [phase_03.md](phase_03.md) | 2.1 … 2.5 | P1 | agent-1 |
| 3 Web-transfer + SSH gateway | [phase_04.md](phase_04.md) | 3.1, 3.2, 3.3 | P1 | agent-1 |
| 4 Outage gate + delivery | [phase_05.md](phase_05.md) | 4.1, 4.2, 4.3 | P2, P3 | agent-1 |

## Reuse map

| Need | Path and symbol | Contract to preserve |
|------|-----------------|----------------------|
| bounded heartbeat write | `src/client.rs` — `beat_once`, `CtrlBeat` | 10 s bound; `PeerNotReading` degrades, never wedges (P-9) |
| tick-based reaper shape | `src/vhost.rs` — `serve_vhost_provider` hb arm; `src/server.rs` — `serve_tunnel` | check on the heartbeat tick, never `timeout(recv)` (DEC-VE3) |
| env-per-call knob | `src/secret.rs` — `ctrl_client_heartbeat()` | read per call, so tests can set it in stages |
| driver ownership | `src/mux.rs` — `spawn_driver_inner`, `drive`, `Liveness` | M-1 handle counting unchanged; scoped driver still cancellable by `ClientScope` |
| VPN generation guard | `src/vpn_server.rs` — `VpnDeregister` | D5 nonce guard unchanged |
| netns harness skeleton | `scripts/local_proxy_netns_test.sh` | staleness guard, trap cleanup, unique ns names |

## Research and evidence

| ID | Question / required or optional | Evidence state | Affected decisions / units / tests |
|----|---------------------------------|----------------|------------------------------------|
| R1 | How long does Linux keep a dead TCP connection that has unacked data? (required) | CONFIRMED | D2, D-R3, T-OUT-IPCHANGE |
| R2 | Does dropping a yamux 0.13 `Connection` close its streams? (required) | CONFIRMED | D4, 0.1 tests |
| R3 | Does russh reset its keepalive miss counter on any received data? (required) | CONFIRMED | D9 |
| R4 | OpenSSH client liveness defaults (required for docs) | CONFIRMED | D9, 3.3 |
| R5 | Does yamux keep reading the socket while its write side is blocked? (required) | CONFIRMED | D1 |

### R1 — tcp_retries2
- Sources: Linux `Documentation/networking/ip-sysctl.rst`, `tcp_retries2`: "The default value of 15 yields a hypothetical timeout of 924.6 seconds". `tcp(7)` (keepalive applies only to idle connections).
- Applicability: every Linux kernel shipped in the last decade; docker shares the host kernel.
- Dates: accessed 2026-10-02 (from knowledge of the stable document).
- Documented fact: retransmission of unacked data gives up after ≈924.6 s. Keepalive probes are sent only when the connection is idle (no unacked data).
- Supervisor inference: bore's heartbeats keep unacked data in flight, so keepalive never fires and detection = 924.6 s. That matches the measured 16 min.
- Local check: the field log (01:03:02 → 01:19:25 = 983 s ≈ 924.6 s + backoff + reconnect).
- Decision impact: an application-level deadline is mandatory (D2); kernel tuning is rejected (D-R3).
- Unresolved/replacement: none.

### R2 — yamux drop semantics
- Sources: `~/.cargo/registry/src/*/yamux-0.13.10/src/connection.rs`, `impl Drop for Connection` → `Active::drop_all_streams` (sets `State::Closed`, wakes reader+writer).
- Applicability: the pinned `yamux = "0.13.10"` (Cargo.toml:98).
- Documented fact (source): dropping an Active connection closes every stream and wakes their tasks.
- Decision impact: `terminate()` = cancel the driver future, so `conn` is dropped and the streams error out promptly (D4). Test 0.1 `terminate_closes_live_substreams_promptly` pins it.
- Unresolved/replacement: none.

### R3 — russh keepalive accounting
- Sources: vendored `crates/russh/src/server/session.rs`.
  - The run loop's `keepalive_timer` arm increments `alive_timeouts`.
  - Every iteration with `received_data` resets it to 0 ("we assume that the client is still alive if we receive any data from it").
- Decision impact: the gateway is already transport-level; only the constants change (D9).

### R4 — OpenSSH liveness
- Sources: `ssh_config(5)`: `ServerAliveInterval` default 0 (disabled), `ServerAliveCountMax` default 3; `ExitOnForwardFailure` default no.
- Supervisor inference: a stock `ssh -R` never detects a dead server path on its own. Recommended `ServerAliveInterval=5`, `ServerAliveCountMax=4` (20 s > the server's 15 s reap, so the reconnect finds the forward freed).
- Decision impact: documentation in 3.3; no code.

### R5 — yamux read/write interleaving
- Sources: yamux 0.13.10 `Active::poll`. The socket is read whenever `pending_read_frame.is_none()`; only a pending Pong/GoAway blocked behind a write-blocked socket pauses reading, until the socket accepts writes again.
- Decision impact: the activity stamp keeps moving under heavy upload, so D1 is congestion-safe.

## Invariants

| ID | Meaning | Guarding test |
|----|---------|---------------|
| I-1 | Any inbound byte on a mux connection, on any substream, refreshes its liveness | `mux::tests::inbound_idle_is_refreshed_by_data_substream_bytes` |
| I-2 | A client whose server path goes silent returns from its serve loop within deadline + 1 tick, in every mode | `tests/outage_liveness_test.rs` `*_client_returns_when_the_server_goes_silent`; T-OUT-IPCHANGE |
| I-3 | A flick shorter than the deadline never disconnects | `*_survives_a_short_flick`; T-OUT-FLICK |
| I-4 | A declared client whose transport is idle ≥ deadline is reaped by the server, and its label/port/alias/id is free again | `server_transport_reaps_*`; T-OUT-IPCHANGE |
| I-5 | A client that declares nothing is never transport-reaped (legacy compat) | `undeclared_client_is_never_transport_reaped` |
| I-6 | The 60 s legacy reaper and clean exits keep the graceful close; only liveness trips terminate | review + `terminate_*` units |
| I-7 | A VPN pairing never outlives either handler | `vpn_pair_teardown_*` |
| I-8 | New wire fields default to legacy values when absent | `*_ctrl_heartbeat_ms_defaults_zero` serde units |
| I-9 | No heartbeat is ever sent to a peer that has not proven it can decode it (VPN waiting/paired, web-transfer owner) | `vpn_*_never_beats_*`, owner units |

## Risks

| Risk | Design mitigation | Verification |
|------|-------------------|--------------|
| False trip on a congested link | transport-level liveness (D1, R5); 15 s = 30 server heartbeats | I-1 unit; T-OUT-FLICK with a download spanning the flick |
| Server reaps before the client reconnects too slowly → `in use` | server ≤ 15.5 s, client ≥ 15 s; reconnect retries after 1 s (backoff reset after a long session) | T-OUT-IPCHANGE recovery ≤ 15 s |
| Transfer-link stops on a long rejection | `REGISTRATION_RETRY_GRACE` 75 s ≫ the ≤ 1 s race | review |
| Env var leakage across parallel cargo tests | new tests live in their own integration binary, serialized by a file-local mutex, and set env before each client | test file design |
| Old VPN server decoding `ClientMessage::Heartbeat` | beats gated by `VpnReady.ctrl_heartbeat` / an observed waiting heartbeat (I-9) | `vpn_*_never_beats_*` |
| CI flake from timing | integration tests use 0.5–2 s deadlines with ≥ 3× margins; netns uses defaults with 15 s windows | CI run |

## Verification strategy

STATE.md §3 is the command registry.
- **Unit gates per sub-phase:** fmt + clippy + targeted tests.
- **Phase gates:** the full `cargo test` (default features), `--features vpn`, `--features ssh-gateway`, and `--no-default-features` check.
- **Final gates:**
  - the new netns gate (`T-OUT-*`), red-checked against baseline `f7b0745`;
  - regression netns harnesses `local_proxy_netns_test`, `secret_netns_test`, `vhost_netns_test`, `vpn_netns_test` and `ssh_gateway_test`, run serially;
  - push to `dev` and the full GitHub CI, all green.

## Model-assignment summary

| Unit(s) | Implementer | Required supervisor review |
|---------|-------------|----------------------------|
| all | agent-1 = claude-opus-5-5 (this session; single-agent roster ⇒ explicit self-review) | per sub-phase after diff (lifecycle/protocol units marked); per phase at P<N> |
