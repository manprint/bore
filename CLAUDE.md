# CLAUDE.md

## Instructions for CLAUDE and optimizations.

### Model selection per task

Three tiers, each with a clear role. Using the wrong one wastes money or quality.
**Target**: **minimize input/output tokens usage**.

- Use caveman in ultra mode (`/caveman ultra`)
- Use caveman plugin for subtsk
- Show to user the model used for every task/subtask

**Haiku 4.5** (`claude-haiku`) — fast, cheap ($1/$5 per MTok)
- Linting, grep-style code search, syntax checks, *codebase exploration*
- Routing/classification decisions in multi-agent flows
- Extracting structured data from text (parse logs, format JSON)
- Generating short repetitive outputs (commit messages, variable names)
- Sub-agent tasks where the work is mechanical, not reasoning-heavy

**Sonnet 4.6** (`claude-sonnet`) — default for 90%+ of tasks ($3/$15 per MTok)
- Implementing features, refactoring, writing tests
- Debugging non-trivial bugs
- Writing/reviewing documentation ( if simple, *delegate to haiku* )
- Code review with explanation
- Agentic loops that need sustained focus but not peak reasoning

**Opus 4.8** (`claude-opus`)— The supervisor. Complex tasks where quality delta is worth 5–10× cost
- Architecture decisions across many files
- Multi-step reasoning that Sonnet visibly gets wrong
- Deep research synthesis
- Check if Sonnet and Haiku works respect the specifics.
- Borker task to Haiku and Sonnet.

**Rule of thumb**: start with Sonnet. Drop to Haiku for bulk/mechanical sub-tasks.
Escalate to Opus only when Sonnet output is concretely insufficient or the task is critical.

## Agent workflow

### Analysis phase
Every repository analysis must produce structured output files organized by phase and
sub-phase. Each entry must contain clear, self-contained implementation details usable
by downstream agents without additional context. Preserve all considerations and
decisions made by the orchestrating agent — nothing implicit, nothing assumed.

### Implementation phase
Work phase by phase, sub-phase by sub-phase. For each unit:
1. Write tests first or alongside implementation.
2. Verify all CI gates pass (`cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`).
3. Run the full regression suite before marking the sub-phase done.
4. **Zero regressions tolerated.** A sub-phase that breaks an existing test is not done.

### Documentation
Every phase that changes behavior, APIs, or invariants must produce or update the
corresponding markdown documentation. Docs are part of the deliverable, not optional.

### Quality bar
- Code must be correct before it is clever.
- If a detail is uncertain, surface it explicitly — do not paper over it.
- High quality is the baseline, not a stretch goal.
- Gates and test (internal, unit and e2e) must present 0 fails.
- You are autorized to launch so tests (permitted by sudoers)

## What this is

`bore` — async Rust TCP/UDP tunnel/proxy/transfer app. (`#![forbid(unsafe_code)]`). Exposes a local port to the internet through a remote server, bypassing NAT/firewalls. Ships `bore_cli` lib + `bore` binary.

**Six subcommands:**
- `bore local <port>` — public tunnel: server assigns a public port, forwards traffic to local `<port>`
- `bore proxy` — secret consumer: connects to a named provider, relays traffic to local port
- `bore server` — runs the relay server
- `bore transfer listener|sender` — file transfer over tunnel (resume, BLAKE3 verify, parallel streams)
- `bore test-udp` — NAT/UDP diagnostic; with `--tcp-secret-id` runs a two-peer latency/bandwidth test
- `bore vpn listen|connect` — Linux L3 VPN (requires `--features vpn`; root/CAP_NET_ADMIN)

**Core transport stack:**
- One long-lived yamux-multiplexed TCP connection per tunnel (control port 7835)
- Plain TCP or TLS (`https://` URL to server)
- Public tunnels: server opens data substreams → client splices to local service
- Secret tunnels: consumer opens data substreams → server relays to provider → provider splices to local
- `--carriers N`: N parallel TCP connections, round-robin per proxied connection (HOL + cwnd isolation)
- `--udp`: UDP hole-punching + QUIC direct path for secret tunnels (each proxied conn = own QUIC bidi stream); falls back to relay automatically
- `--udp` for PUBLIC `bore local` tunnels (no `--tcp-secret-id`): server→client QUIC direct
  path, **no STUN/hole-punch** (server is public, client dials it — same model as
  `bore vhost --udp`). Client opens N (`--carriers`) QUIC connections to the server's
  `--vhost-quic-port`; server round-robins inbound public connections across them (its own
  `PublicDirectEntry`/`DirectPool` per tunnel), writes `mux::STREAM_READY`, splices; falls
  back per-connection to the warm TCP relay. Needs `bore server --udp`

**Key invariants to never break:**
- **Secret control liveness (zombie-entry reaper):** the secret provider/consumer
  control loop is a yamux substream, so a half-open/abandoned peer is invisible to
  `send`/`recv` (send buffers into yamux, recv blocks forever) → the RAII admin
  `Registration` never drops → a zombie admin entry persists (inflates the "Secret
  Tunnels" count). FIX: `serve_provider`/`serve_consumer` track `last_recv` and
  reap (return → drop entry) when `last_recv.elapsed() >= ctrl_timeout`, **checked
  on the 500 ms heartbeat tick** — NOT via `timeout(recv)` (the heartbeat branch
  wins the `select!` every 500 ms and would reset a `timeout(recv)` future before
  it ever reaches the deadline). The secret-provider client (shared `client::listen`,
  gated by `is_secret_provider`) and consumer client (`secret::Proxy` loop) send
  `ClientMessage::Heartbeat` every `CTRL_CLIENT_HEARTBEAT` (20 s ≪ 60 s) so a
  healthy idle tunnel never trips it. `ClientMessage::Heartbeat` is appended LAST
  (wire-compat: old server can't decode it → upgrade server before/with clients).
  `Server::secret_ctrl_timeout()` lowers the 60 s default for tests. Public/vhost
  tunnels keep the legacy heartbeat-free path (their server loops are unchanged).
- **Secret consumer CARRIERS (`--carriers N` on `bore proxy`) must NOT register an
  admin entry and must NOT be reaped.** An extra relay carrier dials the server and
  sends `ClientMessage::ConnectSecret { carrier: true, .. }` (additive
  `#[serde(default)]` field, serde_json wire — old client omits it ⇒ `false` ⇒ legacy
  path). `serve_consumer(carrier=true)` skips `admin.register` (else `--carriers N`
  showed N-1 spurious `local_proxy_port=None` "N/A" rows — BUG-S1) AND skips the
  `ctrl_timeout` reap check (carriers send no `Heartbeat` by design — only the
  consumer's MAIN control connection does, every 20 s — so reaping them degraded the
  pool N→1 after 60 s — BUG-S2). A carrier still accepts+relays its data substreams.
  One logical tunnel = exactly ONE admin row regardless of `--carriers`/transport
  (I-3). The `carrier == false` path is byte-identical. FE (`secret.js`) also dedups
  port-less carrier rows defensively (folds rows sharing a real consumer's peer IP)
  so even an OLD server can't show spurious rows. Provider carriers already used the
  leak-free `JoinCarrier`/`serve_carrier` path — do not fork them.
- **Secret direct-path benign hole-punch strays are `debug`, never `WARN`.**
  `DirectListener::accept` (holepunch.rs) loops internally: incipient QUIC incomings
  from punch crossfire that never finish TLS / carry no/again-wrong token are logged
  at `debug` and skipped; only an endpoint-level close propagates as `Err`. The real
  token-verified connection succeeds alongside them. Do NOT restore a per-stray WARN
  (BUG-S3) and do NOT filter the accepted source against the offered candidates /
  disable QUIC migration — token auth is the gate and CGNAT consumers legitimately
  connect from an un-offered source (e.g. a `100.64/10` egress; D7).
- `relay()` (secret.rs) fails over across live provider carriers (retry `pick`→`open`
  up to pool size) — a carrier dying between pick and open must not drop the forwarded
  connection (BUG-S4). `--carriers N>1` on a secret `--udp` consumer that goes DIRECT
  is a single QUIC connection; it `warn!`s once (N applies only to the relay fallback,
  BUG-S5) — never silently ignored. See docs/SECRET_HARDENING_ASSESSMENT.md.
- **Transfer hardening (audit 2026-07-10, `docs/transfer/TRANSFER_ASSESSMENT_2026-07-10.md`):**
  receiver bounds manifests (`MAX_MANIFEST_ENTRIES`, count enforced mid-loop — never
  `with_capacity` a peer-controlled u64); the pre-manifest phase (Begin + manifest frames) is
  `with_stall`-bounded (a silent sender must not pin a persistent listener); the worker-accept
  loop validates the FIRST frame itself and answers strays (a 2nd sender's `Begin`) with an
  Error frame instead of spawning them as workers (whose failure aborted the in-flight
  transfer, B7); `commit_stage` multi-source is per-child content-idempotent (a child
  committed by a partially-failed run is skipped on retry — without it Fail/Rename retried
  forever, B5); `write_json_atomic` fsyncs tmp before rename and a corrupt `state.json`
  degrades to a FRESH start, never a permanent error (B4); `ProgressTracker` has a `Drop`
  (error paths leaked the 250 ms tick task, B2); `--source-files` comments are leading-`#`
  only (B6); sender pre-hash runs in `hash_planned_entries` (parallel, post-scan) — do not
  move hashing back inline into `scan_entry` (single-threaded full-tree read before the
  first byte, P1).
- Client sends `Hello` before auth (yamux is lazy; without it, deadlock)
- `HelloVpn`/`ConnectVpn` sent **before** auth (same lazy-yamux rule as `Hello`)
- Server writes `mux::STREAM_READY` before splice (banner-first protocols need it)
- `copy_bidirectional_with_sizes` propagates half-close; do not replace with a non-half-close variant
- **Vhost injected-response path must `flush()` after every write, before parking on read**
  (`relay_response_injected` + `copy_one_direction_with_shutdown`, vhost.rs; fixed 36cd70d).
  tokio-rustls `poll_write` can return `Ok` with encrypted records still buffered in the
  session (socket returned Pending); on a keep-alive connection there is no EOF/`shutdown`
  to flush them and the loop parks on `read()` forever → response tail never sent → browser
  asset stuck `pending`. ONLY the response-header-injection path is affected — the
  no-injection path uses `copy_bidirectional_with_sizes`, whose `CopyBuffer` already
  flushes-on-pending. Do NOT remove these flushes as "redundant", and do NOT replace the
  hand-rolled loop with `tokio::io::copy` (8 KiB internal buffer vs `proxy_buffer_size()`
  256 KiB default — high-BDP regression). The split + `try_join!` single-task shape is
  REQUIRED by the yamux waker invariant (provider halves must stay in one task — never
  "clean it up" into two spawned tasks). ENFORCING gates (red-checked: fail without the
  flushes) are the FlushGatedWriter mock unit tests in vhost.rs `mod tests`
  (`copy_loop_flushes_writes_before_parking_on_read`,
  `injected_response_head_and_body_flushed_before_keepalive_park`) — every in-process TLS
  integration test of this bug FALSE-PASSES on loopback (rustls drains opportunistically
  via other poll paths; verified 2026-07-08), so the integration tests
  (`vhost_response_header_injection_large_keepalive_body_completes` in vhost_test.rs,
  `t_ssh_dmx5`..`t_ssh_dmx8` in ssh_gateway_test.rs) are belt-and-braces for truncation/
  desync only. See docs/VHOST_INJECTED_FLUSH_FIX.md.
- `shared::tune_tcp` (`TCP_NODELAY` + `SO_KEEPALIVE 15s`) must be applied to every new socket
- `--max-conns` semaphore is the real bound; yamux stream limit is set generous intentionally
- `carriers<=1` keeps the single-connection path byte-for-byte unchanged. Default is `1`
  for `local`/`proxy`, but `0` (auto) for `bore transfer` — auto scales the relay carrier
  pool to the worker `--parallel` count (capped at server `--max-carriers`); `transfer.rs`
  resolves it via `resolve_carriers`. Explicit `--carriers 1` still forces the single path.
- **Direct `--udp` connection_receive_window MUST stay >> stream_receive_window** (256 MiB
  vs 16 MiB, `shared.rs` `DIRECT_QUIC_*` + the `--udp-*-window` flag defaults in `main.rs`).
  At `--carriers 1` every proxied connection is a bidi stream on ONE QUIC connection; the
  RECEIVER (server, for vhost/secret response bytes) only returns connection-level credit as
  it DRAINS a stream into its public socket. A slow/paused public reader (browser pausing
  assets) lets quinn buffer a full per-stream window (16 MiB) of unread data per stalled
  stream against the SHARED connection window. `conn/stream` = how many stalled streams it
  takes to starve EVERY other stream on that connection (requests hang `pending`). At 64 MiB
  that was ~4 → the carriers=1 vhost stall; 256 MiB tolerates ~16 (the headroom four 64 MiB
  carriers gave, in the default). It is a CEILING not a reservation (healthy tunnel buffers
  ~0). Do NOT drop conn window back toward stream window, and do NOT raise stream window to
  meet it (regresses single-stream throughput). Repro/gate: `scripts/vhost_udp_concurrency_repro.sh`
  (R3 carriers=1 must serve a fast request <3 s while 8 slow readers pin the window).
  Note (b): unprivileged `bore vhost`/`local` can't beat the kernel `net.core.*mem_max`
  clamp on the per-socket UDP buffer (CAP_NET_ADMIN-only `SO_*BUFFORCE`); it `warn!`s the
  sysctl/setcap remediation — orthogonal throughput cap, `--carriers N` also mitigates.
  See docs/VHOST_UDP_CONCURRENCY_FIX.md.
- **Public-tunnel `--udp` (server→client QUIC, mirrors vhost; `docs/LOCAL_UDP_PLAN.md`):**
  `--udp` off ⇒ public path byte-for-byte the TCP relay (DEC-LU5); `TunnelOptions.udp`
  is `#[serde(default)]` so old/new client↔server interop. ONE server QUIC endpoint binds
  whenever `bore server --udp` (NOT gated on vhost config) and serves both vhost subdomains
  and public tunnels; the auth handshake key is namespaced (`port:{public_port}` for public,
  bare DNS label for vhost) so the accept loop installs into the right registry. `--carriers N`
  on public `--udp` = N independent QUIC connections (own BBR each), per-connection
  round-robin — NEVER per-datagram/intra-request striping (reorder trap). The TCP carrier
  pool stays warm for the tunnel's life; direct is tried per inbound connection and falls
  back in place — UDP never gates tunnel liveness (DEC-LU4). Server writes `STREAM_READY`
  on the direct bidi stream too (DEC-LU6); the client funnels accepted streams into the same
  `handle_connection` as the relay path. `spawn_direct`/`direct_*` client state + the
  holepunch `vhost_connect`/`vhost_server_handshake`/`DirectPool` are SHARED with vhost — do
  not fork them. Hole-punch helper flags (`--upnp`/`--stun-server`/`--try-port-prediction`/
  `--nat-udp-*`) stay secret-tunnel-only and `warn!` (not silently ignored) on a public tunnel
- Relay path is AEAD-opaque: server splices ciphertext, never plaintext IP packets
- **Never `tokio::io::split` a `mux::Stream` across two tasks.** `yamux::Stream` keeps a
  single parked-task waker on its internal channel (`poll_read` and `poll_write` both call
  `sender.poll_ready`); two tasks overwrite each other's waker and the loser is never woken
  — the stream wedges silently under load. One stream = one task. The VPN relay uses two
  unidirectional substreams (tags `0x01`/`0x02`) for exactly this reason. Single-task
  bidirectional use (`copy_bidirectional`, `try_join!` in one task) is safe.
- VPN relay queue applies backpressure (await on full), never silent drops; VPN clients
  must keep draining the control stream after `VpnReady` (heartbeats + server-death detection;
  the ctrl actor in `vpn.rs` is the stream's single owner — route new control messages through it).
  Server heartbeats every 500 ms; the 1:1 ctrl actor reads with a 60 s `CTRL_HEARTBEAT_TIMEOUT`
  (parity with the hub's 60 s) on top of `SO_KEEPALIVE` 15 s, so a wedged-but-TCP-alive server is
  detected — not just a broken socket (B5)
- VPN: links start on relay; a background task attempts the direct QUIC upgrade (skipped with
  `--relay-only`). Path switch = controlled bridge restart (DEC-1: stop pumps, switch uplink set,
  respawn on Direct). Relay stays WARM for link lifetime; on direct death the bridge falls back to
  warm relay IN PLACE (no reconnect, TUN preserved, nonce counter preserved — DEC-2: seamless fallback).
  Full reconnect only if BOTH paths down. Server brokers `UdpPunch` to BOTH sides only when it holds
  BOTH offers (DEC-3, 10 s timeout → `UdpUnavailable`)
- VPN AEAD nonce counter is ONE shared `Arc<AtomicU64>` per egress key (I-5/DEC-6): carriers
  and multi-queue clones all `fetch_add` on it — never per-producer counters, never two seals
  with the same `(key, counter)`. Relay carriers round-robin per-datagram (DEC-7, reorder OK);
  any future replay window (B1) must size for that reorder: ≥ 2 × (carriers × RELAY_QUEUE)
  (DEC-10)
- VPN `--carriers`/`--tun-queues` default 1 = byte/path-identical to the single configuration
  (I-9). Carrier count negotiated min(listener, connector, server `--max-carriers`); a dead
  carrier kills the whole link cleanly (reconnect re-establishes), never silent degradation
- VPN `--carriers` applies to BOTH paths (Fix #3a). Relay: N AEAD substream pairs. Direct: N
  parallel QUIC connections over the ONE punched socket (`DirectConn::open_sibling` reuses the open
  5-tuple — no extra punch), each its OWN congestion controller. The single downlink task
  `select_all`s `read_datagram` across all carriers (one task, many conns — safe: `read_datagram`/
  `send_datagram` are `&self` + cancel-safe, no stream split). Establishment requires the FULL
  negotiated count on both sides (connector dials siblings to `conn0.remote_address()`, listener
  accepts N) — any carrier failing aborts the upgrade → stay on relay + retry (never a mismatched/
  silently-degraded count). `carriers==1` = legacy single conn, byte-identical. Hub per-peer direct
  path stays single-conn (v1)
- VPN DIRECT carrier steering is FLOW-PINNED, not per-datagram round-robin (BW-F2). `flow_carrier`
  hashes the inner IPv4 5-tuple → one inner connection always rides ONE carrier (in order); distinct
  flows spread. CRITICAL: per-datagram RR across carriers reordered a single flow and the tunnelled
  TCP read the reorder as loss — `--carriers 4` could HALVE throughput / explode UDP loss to 25-44 %
  (netns+netem). NEVER restore per-datagram RR on the direct path. `n==1` → idx 0, byte-identical.
  RELAY keeps per-datagram RR (reliable streams; replay window sized for it, DEC-10) — do not flow-pin
  it without resizing the window. A single bulk flow gains nothing from carriers (one carrier); the
  real VPN bottleneck is the single inner TCP flow (Mathis) — parallelise the workload. `--carriers`
  default stays 1; rarely helps a VPN (see docs/vpn/VPN_BANDWIDTH_ASSESSMENT.md)
- VPN 1:1 uplink uses `send_batch_wait` (BACKPRESSURE, BW-F3): on a full QUIC datagram send buffer it
  AWAITS room instead of quinn silently dropping the OLDEST queued datagram — drop = congestion the
  tunnelled TCP reads as loss (cwnd collapse) + bufferbloat. Awaiting pauses the TUN read so the
  kernel TUN queue backpressures the inner senders. ONLY the dedicated 1:1 uplink task may block here;
  the SHARED hub router keeps non-blocking `send_batch` (a blocking peer would HOL every other peer).
  Relay branch ignores backpressure (bounded channel already blocks)
- VPN direct-path throughput is bounded by the UDP socket buffer / RTT. The kernel SILENTLY clamps
  `SO_SNDBUF`/`SO_RCVBUF` to `net.core.{w,r}mem_max` (stock Ubuntu/AWS default 208 KiB → ~10 MB/s at
  20 ms RTT regardless of Quinn's windows, CPU idle). `configure_udp_socket_buffers` (holepunch.rs,
  Linux) forces past it with `SO_{SND,RCV}BUFFORCE` (nix `*BufForce`, needs CAP_NET_ADMIN which VPN
  has) → falls back to the clamped setter on EPERM → getsockopt-verifies and `warn!`s with the
  remediation when a clamp survives (was a silent `debug!`). Requested 16 MiB (Fix #1)
- **Direct-path UDP punch sockets must NEVER set `SO_REUSEADDR`** (`holepunch::bind_socket`,
  holepunch.rs). Two wildcard UDP sockets that BOTH set `SO_REUSEADDR` co-bind the same
  `0.0.0.0:port` and the kernel delivers inbound to the **last binder**. So when two direct-path
  tunnels (VPN + secret, vhost, public `--udp`) share a `--nat-udp-preferred-port` on one host —
  even in **separate processes** — each ~30 s re-punch rebinds the port and **steals** the other's
  inbound QUIC, idle-closing the live connection → the establish→die→re-punch ~30 s LOCKSTEP FLAP
  (both tunnels flap; only-with-concurrent-secret repro). FIX: bind the preferred port WITHOUT
  `SO_REUSEADDR`; the kernel then refuses the 2nd binder (`EADDRINUSE`) and `bind_socket` falls back
  to an **ephemeral port + `warn!`** (so the 1st tunnel keeps the firewall-friendly port; the 2nd
  gets its own port and still punches, or stays on relay behind a strict egress firewall). UDP has
  no TIME_WAIT, so a same-tunnel `--auto-reconnect` still rebinds the fixed port — but callers MUST
  drop the old socket BEFORE binding the new one (no overlap), else the rebind hits `EADDRINUSE` and
  downgrades to ephemeral. Regression: `bind_socket_*` unit tests + `T-STRESS-PORTCLASH` /
  `T-STRESS-MIX` in `vpn_netns_test.sh`. Mechanism proof: `docs/plans/udp_flap/`. The direct QUIC
  layer (keepalive 3 s / idle 10 s, transport_config) is byte-identical since `3a5c87b` — the flap
  was NEVER the QUIC layer; do not re-bisect it
- `NetConfig` RAII: all routes/nft/ip_forward changes revert on exit (SIGINT, SIGTERM, panic handled; SIGKILL requires next-run stale reclaim via /run state file to restore ip_forward and remove leaked iptables/nft rules — BUG-2/BUG-3 fixed). Concurrent gateway links in ONE netns refcount ip_forward via per-`(netns,id,role)` `/run/bore-vpn-ns<inode>-*.fwdref` markers + a first-wins `/run/bore-vpn-ns<inode>.ipfwd-orig` record: a link restores ip_forward only when NO other co-netns `.fwdref` remains, and the last one out restores the true original — never disables forwarding under a still-live co-netns peer (B3 fixed); `stale_reclaim` is refcount-aware too. CRITICAL: markers are scoped by the `/proc/self/ns/net` inode because `ip_forward` is per-netns while `/run` is shared across netns (the netns harness, containers) — an unscoped refcount would wrongly couple independent netns and break teardown
- TUN MTU default 1350: clamps QUIC datagram size; gateway MSS-clamp keeps forwarded TCP healthy
- `--pin-mtu` (BW-F4): the PMTU monitor runs OBSERVE-ONLY — it `warn!`s when the path max_datagram
  drops below the pinned TUN MTU (full-size packets being TooLarge-dropped) and `info!`s on recovery,
  but NEVER calls `ip link set`. For tests/benchmarks that need a fixed MTU. Default off = dynamic
  auto-tune (the existing `pmtu_monitor` resize path). `pmtu_monitor(.., pin)` carries the flag
- VPN direct path: a `TooLarge` datagram send is a per-packet DROP, never link death. The TUN MTU
  runs ahead of the QUIC path MTU right after every direct switch, so full-size packets exceed
  `max_datagram_size()` until the PMTU monitor narrows the TUN. `DirectConn::send_datagram` returns
  the typed `DatagramSend::{Sent,TooLarge}` (NOT a stringly error — quinn's `Display` for
  `SendDatagramError::TooLarge` is `"datagram too large"`, so substring-matching `"TooLarge"`
  silently never fired and killed the link). `send_batch` returns the drop count; only genuine link
  death returns `Err`. PMTU monitor shrinks immediately on one below-current sample
  (`pmtu_shrink_now`, fast recovery), grows only on 3 stable samples (`pmtu_decision`, anti-flap).
  Black-hole hysteresis (Fix #2): a grow followed by a shrink back within 30 s marks the grown size
  as a ceiling (`pmtu_decision`'s `ceiling` arg blocks GROWING into it again — shrinks are never
  blocked), so the TUN stops chasing quinn's doomed re-probe of an MTU the WAN path can't carry
  (the ~70 s 1162↔1414 oscillation + periodic `TooLarge` drop bursts). Ceiling clears after 5 min of
  a stable MTU so a genuinely improved path is rediscovered
- VPN direct-path candidates must NEVER include an address routed into the TUN. A peer candidate
  inside a locally-tunneled subnet (`peer_routes`, e.g. connector routes `10.10.0.0/19 → bore0`
  and the peer offers `10.10.16.138`) makes the QUIC handshake loop through the relay: it
  succeeds, the bridge switches to direct + drops the relay halves, then the looped path dies at
  the QUIC idle timeout (`read_datagram: timed out` ~10 s; provider sees the peer as the *overlay*
  IP `10.99.x.x`). `filter_tunneled_candidates` drops these before punching → fall back to relay,
  never a fake-direct path that silently dies. Conservative by design (drops even if a
  more-specific connected route would reach it off-tunnel)
- VPN direct upgrade is NOT one-shot: `direct_upgrade_task` retries on a fixed 30 s grid
  (`DIRECT_RETRY_INTERVAL`, `should_retry_direct`) while on relay, so a link that came up on a
  UDP-hostile network upgrades to direct in-place (no reconnect) once the path opens. Relay stays
  stable through every failed attempt. Stops on success or upgrade-channel close. Both peers stay
  aligned because the grid is anchored at pairing and the interval > worst-case attempt
  (`DIRECT_PUNCH_WAIT` 15 s). Server broker MUST re-arm per round (reset deadline + clear `punched`
  on each repeated `UdpCandidateOffer`) or retries never re-punch, AND clear the listener's stored
  candidates right after each punch (else round N+1 re-punches round N's dead socket → connector
  times out against a closed port). `--relay-only` skips it entirely. Also: the netns harness
  (`vpn_netns_test.sh`) refuses to run against a release binary older than `src/` — rebuild with
  `cargo build --release --features vpn` (as your user, not root) before `sudo`-running it

**VPN multi-client (hub-and-spoke, `--max-clients N>1`) — `mod hub` in `vpn.rs` + `vpn_server.rs`:**
- I-MC1: `--max-clients 1` (default) is byte-for-byte the legacy 1:1 path. Hub mode is a SEPARATE
  early branch (`run_listen_hub`); never edit the 1:1 path to add hub behavior. Hub requires server
  pool addressing (no static /30); connector `--advertise` is rejected by the server (D4).
- Server keeps the listener registry entry ALIVE in hub mode (`VpnProviderEntry.hub: Option<HubShared>`,
  `pair_tx` is None); each connector allocates a host addr + monotonic `peer_id` from `HubState`,
  pushes `HubPeerEvent::Join/Leave/Punch` to the hub via an mpsc, and is relayed with a `peer_id`
  injected: server→hub framing is `[STREAM_READY, peer_id u32 BE]` then the connector's verbatim
  `[tag, idx?, payload]` (`vpn_relay_hub`). Connector→server bytes are UNCHANGED (I-MC2).
- Hub data plane: ONE TUN; a shared **router uplink** routes by dst IPv4 → per-peer swappable
  `Mutex<LinkSender>`; one downlink per peer writes the shared TUN (writes are packet-atomic).
  The router NEVER restarts on a path switch — the per-peer direct upgrade swaps the sender IN PLACE
  and keeps the relay downlink WARM for seamless fallback (I-MC5/DEC-2), exactly per-peer.
- Each peer derives its OWN keys from its OWN `session_nonce` (passed RAW, UDP_NONCE_LEN bytes — never
  padded/resized, or HKDF inputs diverge and the AEAD keys won't match) with its OWN shared nonce
  counter (I-MC4 — never shared across peers).
- Spoke isolation (D2): `iifname bore0 oifname bore0 drop`, added by `NetConfig::apply(.., hub=true)`
  in gateway mode. A HOST-ONLY hub (no `--advertise`) currently relies on the host `ip_forward=0`
  for isolation (no nft table is created) — a known v1 gap if the host forwards by default.
- Connector route policy is DEFAULT-DENY (I-MC8): `routes::filter_accepted(advertised, accept_all,
  refuse_all, accept, refuse)` with exact-or-subset matching (a flag CIDR must equal or be a SUPERNET
  of an advertised CIDR — `flag.prefix <= adv.prefix && flag.contains(adv.network())`). This also
  changed the 1:1 connector default: existing netns site-to-host tests pass `--accept-all-routes`.
- TRAP: a hub helper that only `tokio::spawn`s must NOT be an `async fn` unless the call site awaits
  it — an unawaited future never runs (this silently killed the whole relay accept path once).
- Full 5-host scenario + per-peer direct/relay/fallback are covered by T-HUB*/T-HUBD*/T-SCEN-* in
  `vpn_netns_test.sh` (run on BOTH relay and direct). NOPASSWD sudo is per-EXACT-path: invoke
  `sudo -n /abs/path/scripts/vpn_netns_test.sh` (NOT `sudo bash scripts/...`, which prompts).

**VPN overlapping-subnet NAT (E3) — stateless 1:1 netmap for identical LANs:**
- **I-NAT1:** No `@` in advertise ⇒ `NetConfig::apply` byte-for-byte today's blanket masquerade (zero regression, mirrors I-MC1).
- **I-NAT2:** Only **exposed (virtual)** CIDR serialized (`HelloVpn`/`ConnectVpn`/`VpnReady`); real subnets gateway-local, never on wire. NAT client interops with unmodified server.
- **I-NAT3:** Netmap stateless 1:1, host-bits preserved; real & exposed equal prefix length (validated parse). No conntrack.
- **I-NAT4:** Each gateway maps only its own real↔exposed; no per-peer or global state. Identical relay/direct (kernel-side).
- **I-NAT5:** NAT'd subnets never masqueraded (source already peer virtual). When NAT present, masquerade scoped to plain subnets by destination.
- **I-NAT6:** Server overlap check on virtuals; real subnets may overlap freely (feature purpose).
- **I-NAT7:** `NetConfig` RAII reverts netmap rules + prerouting chain on SIGINT/SIGTERM/panic; SIGKILL via `stale_reclaim` (nft: table delete; iptables: explicit rule deletes).
- **I-NAT8:** Bore data plane unchanged — IP packets opaque, no Rust header rewrite; all NAT kernel nft/iptables.
- **I-NAT9:** LAN-egress iface + `ip_forward` use real subnet (virtual has no local route).
- **I-NAT10:** Every link logs at `info`: advertise entries (real→exposed), NAT rules, peer routes, canonical route-table summary. No ALG — embedded IPs not translated.

**VPN FORWARD default-deny gap (`--forward-accept`):**
- On a **default-deny FORWARD** host (Docker daemon `-P FORWARD DROP`, ufw, hardened) a gateway
  reaches ONLY itself; every host BEHIND it is stranded. bore's nft NAT rules live in a SEPARATE
  table and **cannot override a terminal FORWARD `DROP`** from another chain (accept is not terminal
  across base chains; drop is). The Docker DAEMON's rule persists on the host even when bore runs
  natively (not in a container) — `docker0`/`br-*` in `ip route` is the tell.
- `--forward-accept` (gateway/listen side) punches an `ACCEPT` for the tun↔LAN pair into the
  iptables `filter` FORWARD chain via a per-link custom chain `bore_<id>_fwd` (F3/F4 pattern:
  `-N` + `-I FORWARD -j` at TOP + two `-A ... ACCEPT`), torn down by id alone (SIGKILL `stale_reclaim`
  safe). **iptables, NOT nft** — the real-world deny lives in `ip filter FORWARD` regardless of bore's
  NAT backend; a hand-rolled `nft inet filter forward` policy-drop is NOT covered (out of scope, v1).
  Off (default) ⇒ bore PROBES `iptables -S FORWARD` and `warn!`s the exact remediation when policy is
  DROP/REJECT (`forward_policy_is_deny`). Detection-vs-install is mutually exclusive (no probe when
  punching). RAII-reverted. Covered by T-FWD in `vpn_netns_test.sh` + `apply_*` unit tests.
- NOTE: `--forward-accept` only fixes the FORWARD hop. NAT'd (`real@virtual`) subnets ALSO need
  `--nat-masquerade` for the return path when the gateway is not the LAN router (I-NAT5) — the two
  are orthogonal; the field repro needed BOTH.

**VPN macOS port (runtime LANDED 2026-06-29; macOS-compile + Mac spike validate on CI/hardware):**
- The `vpn` module + `Vpn` subcommand are now gated `cfg(all(feature="vpn",
  any(target_os="linux", target_os="macos")))` — VPN runs on **Linux AND macOS** (Apple Silicon,
  macOS 13+; root/`sudo`). Plan: `docs/plans/plan_VpnMacosCompletion/`; backend ref:
  `docs/vpn/VPN_MACOS.md`; spike findings: `docs/vpn/VPN_MACOS_SPIKE_FINDINGS.md`; manual acceptance:
  `docs/vpn/VPN_MACOS_ACCEPTANCE.md`. Decisions LOCKED: `--forward-accept`=PF `pass`, `macos-14` CI
  runner, Windows deferred.
- DEC-M1 (zero-regression contract, HELD): every Linux runtime fn (`create_tun`, `NetConfig::apply`,
  the `Drop` ip_forward branch via `restore_ip_forward_op`, `stale_reclaim`) and the bridge offload
  pumps (`run_uplink_offload`/`run_downlink_offload`/`run_router_uplink_offload`) keep their bodies
  BYTE-FOR-BYTE under `#[cfg(target_os="linux")]`; macOS is an additive `#[cfg(target_os="macos")]`
  twin (compile-time split, NOT a runtime trait). `cmd_nft_*`/`cmd_iptables_*` stay un-gated but
  unused on macOS (`#![allow(dead_code)]` on `hostcfg_cmd`). Shared, un-gated: `pick_tun_name`,
  `check_root`, `check_binary_exists`, `CommandRunner`/`RealRunner`, `NetConfig` fields, the
  `revert_cmds` argv stack, the `/run`-vs-`/var/run` state-path helpers (via `run_dir()`), and the
  whole data plane (bridge/AEAD/carriers/relay/QUIC/PMTU). Proof: `vpn_netns_test.sh` stays green +
  `git diff` shows no semantic edit inside any `cfg(linux)` body. Attribute order: a doc comment must
  be followed by `#[allow(...)]` THEN `#[cfg(...)]` on a cfg-twinned `pub` method, else `missing_docs`
  misfires.
- macOS runtime: `create_tun` makes a single-queue, no-offload utun, kernel-assigns the `utunN` name
  and reads it back via `dev.name()` (D7/I-M8); `--tun-name` is advisory (`macos_tun_request` maps
  `auto`/`boreN` → kernel-assign, explicit `utunN` → passthrough); `--tun-queues>1` + the UDP
  hole-punch helper flags warn (`macos_flag_warnings`, I-M4/I-M7) — never silently ignored. TUN
  offload is always OFF on macOS ⇒ the bridge single-packet path (I-M3; the `*_offload` pumps use
  Linux-only `tun-rs` `recv_multiple`/`GROTable`/`VIRTIO_NET_HDR_LEN`, so macOS gets `unreachable!`
  twins). `NetConfig::apply` (macOS) = `route -n` routes + `sysctl net.inet.ip.forwarding` + ONE
  per-link PF anchor `bore_vpn/<id>` composed by `pf_ruleset` and loaded with `pfctl -a … -f`; RAII
  flushes the anchor + restores forwarding; `stale_reclaim` flushes a leaked anchor by id + restores
  forwarding from the `/var/run` state file (D5, no netns inode → single ns0 scope; I-M6). BSD tools
  are never `--version`-probed (D8; the `ip` preflight is `cfg(linux)`). PF mapping: `binat`=1:1
  netmap (host-bit preserving), `nat`=masquerade, `scrub max-mss`=MSS clamp (1310 = default MTU
  1350−40), `block`=spoke isolation, `pass`=`--forward-accept`.
- VALIDATED ON macos-14 CI (2026-06-29, branch `macos`, full run 100% green): the `macos-vpn-build`
  job (`cargo build` + `clippy --all-targets -D warnings` + `cargo test --features vpn`) AND a gating
  `macos-vpn-e2e` job that runs `examples/macos_vpn_spike.rs` (spike/create-teardown/apply-revert/
  leak-then-reclaim) under sudo on the hosted runner. Real `pfctl` ACCEPTS the `pf_ruleset` grammar
  (no longer PROVISIONAL); `create_tun("auto")`→kernel `utun4` read back via `dev.name()`; apply/RAII-
  revert + SIGKILL `stale_reclaim` pass. Hosted macos-14 permits utun+PF+sysctl under sudo. The dev box
  is Linux (blake3+ring C/asm block cross-compile clippy to aarch64-apple-darwin) → macos-14 CI is the
  ONLY macOS oracle; iterate via CI. macOS-only clippy bites that Linux can't see: gate Linux-only
  imports `cfg(linux)`, avoid `Iterator::last`/`filter().next()`, doc-list `doc_lazy_continuation`,
  and on a cfg-twinned `pub` method put `#[allow]` before `#[cfg]`.
- REMAINING: only the two-host LAN gateway **manual acceptance** (`VPN_MACOS_ACCEPTANCE.md`, T-MAC-MANUAL)
  — CI is single-host (the single-host PF rules are already CI-validated). Windows deferred. Any PF
  correction lands in `pf_ruleset`/the `cmd_pf_*` builders + their snapshots, not in `apply`.

**VPN Android port (runtime LANDED + VALIDATED on rooted emulator 2026-07-03,
branch `android`; Phases 1-5 done — only manual physical-device acceptance remains):**
- Plan: `docs/plans/plan_AndroidSupport/` (overview + phase_0{1..5}.md +
  resume.md + `SPIKE_FINDINGS.md`). Same twin pattern as the macOS port
  (DEC-M1/I-A1): every OS-specific fn/type gets a `#[cfg(target_os =
  "android")]` sibling; Linux/macOS/Windows bodies stay byte-identical.
  `cfg(any(...))` gates extended to include android everywhere needed to
  compile: `Cargo.toml` tun-rs dep, `lib.rs`/`main.rs` module+subcommand
  gate, both vpn test files, `check_root` (android hint message, body otherwise
  identical), the `ip --version` probe cfg (toybox supports it, same as Linux
  iproute2), and the 3 offload-pump `unreachable!` twins (android has no
  Linux-only `tun-rs` GSO/GRO offload path, same as macOS).
- `crates/bore-android-tun` (new, mirrors `crates/bore-wintun`'s isolation
  pattern): `tun-rs::DeviceBuilder` doesn't cover android and its `from_fd`
  constructor is `unsafe`, which the workspace's `#![forbid(unsafe_code)]`
  can't take directly — the unsafe fd-open + `TUNSETIFF` ioctl is isolated in
  this tiny standalone crate (own `Cargo.toml`, no `forbid(unsafe_code)`) so
  `src/vpn.rs` calls it as a safe API. `nix` dep needed `features = ["fs"]`;
  it was initially (wrongly) placed under `[target.'cfg(windows)'.dependencies]`
  by copy-paste from `bore-wintun`'s Cargo.toml — moved to plain
  `[dependencies]`.
- **D-A4/D-A6/D-A9 (host-only scope, HARD invariant):** android VPN is NEVER a
  gateway — no `--advertise`, no `--nat-masquerade`, no `--forward-accept`, no hub
  mode (`--max-clients>1`), no multi-queue TUN (`--tun-queues>1`). Enforced TWICE:
  a fail-fast CLI guard (`validate_android_host_only`, called at the top of
  `run_listen`/`run_connect` before `run_with_reconnect` — config errors are not
  retryable) is the PRIMARY gate; `NetConfig::apply`'s `hostcfg_cmd::android::
  check_host_only` is defense-in-depth at the apply layer. Both are pure/un-gated
  (`target_is_android`/inputs are plain bools, not `cfg`) so the full matrix is
  unit-tested on the Linux host without an android cross-compile target.
- Android `NetConfig::apply` twin: no ip_forward, no nft/iptables, no PF — only
  `ip route add` per accepted peer route via toybox's `ip` applet, which supports
  the same basic `route add/del`/`link set mtu` grammar as Linux iproute2 but
  **NOT `ip route replace`** (unlike the Linux twin's idempotent `replace`) — the
  android argv builders (`hostcfg_cmd::android`) use `add`, not `replace`.
  `stale_reclaim` (android) has nothing to restore beyond its own marker files
  (host-only ⇒ never creates an ip_forward value or fwdref refcount marker in the
  first place). `restore_ip_forward_op` (android) is `unreachable!()` — apply never
  pushes an `AppliedOp::IpForward`, mirroring the offload-pump `unreachable!` twins.
- `run_dir()` (android) → `/data/local/tmp` (D-A8: no writable `/run`/`/var/run`
  under SELinux + the app sandbox, unlike Linux/macOS/Windows).
- **Android `netd` policy routing eats the implicit `lookup main` fallback rule**
  (found via emulator e2e, not predictable from docs): stock Linux always has a
  kernel-default `32766: from all lookup main` rule; android's `netd` deletes it
  and replaces it with per-UID/fwmark policy rules (e.g. `15000: from all fwmark
  0x0/0x10000 lookup legacy_system`), so a locally-generated reply packet
  (mark=0) for the TUN's own connected route never reaches `main` and gets
  dropped. Fix (android `create_tun` twin, `src/vpn.rs`): explicit
  `ip rule add to <subnet> lookup main priority 100` — low enough priority (100
  < netd's ~10000+ range) to win regardless of mark/uid/iif, scoped to just this
  link's subnet so normal app traffic routing is untouched. `rp_filter` is also
  relaxed to 0 (on `all` + the TUN iface) as defense-in-depth, though it was NOT
  the actual root cause of the original ping failure.
- **`ip rule add` DOES error on an exact duplicate** (`RTNETLINK answers: File
  exists`) — unlike `ip addr add`. Because the rule above lives in the kernel's
  routing-policy DB, not attached to the TUN device, it survives that link's
  teardown; a second `listen`/`connect` reusing the same overlay subnet (e.g. a
  test harness restarting its address pool from the same first address) hits
  the identical rule and, if added via the shared `run_ip` helper (which
  `bail!`s on any non-zero exit), kills the whole `connect`/`listen` process
  before the TUN finishes coming up. Fixed by adding this one rule via
  `std::process::Command` directly and tolerating a `stderr` containing
  "File exists" as success (the rule is idempotent by construction) while still
  failing hard on any other error.
- No local android cross-compile tooling on the Linux dev box (no `cargo-ndk`, no
  `rustup` android target) — same situation as the macOS port; verify via CI
  (`cargo ndk clippy` for x86_64/arm64-v8a) after push, not locally.
- Status: Phase 1 (CI build+clippy matrix) and Phase 2 (non-VPN android-emu-e2e,
  `scripts/android_emu_test.sh`) both green. Phase 3 (VPN compile-port + host-only
  guards) done Linux-side, `vpn_netns_test.sh` 161/0 zero-regression. **Phase 4
  (runtime validation) DONE and CI-GREEN 2026-07-03**: `examples/android_vpn_spike.rs`
  (spike/create-teardown/apply-revert/leak-then-reclaim) + the `android-vpn-e2e` CI
  job (`scripts/android_vpn_test.sh`, T-AND-S1..S3 + T-AND-L1..L5) both pass on a
  rooted x86_64 emulator — `PASS: 8 FAIL: 0`, including a genuine bidirectional
  DIRECT-path link (T-AND-L2) alongside the forced-relay link (T-AND-L1). Findings
  in `docs/plans/plan_AndroidSupport/SPIKE_FINDINGS.md`. The android VPN path is now
  **proven to run** on-device, not just compiled/unit-tested. **Phase 5 (docs) DONE
  2026-07-03**: `docs/ANDROID.md` (install/feature-matrix/CLI-guard-table/VPN backend
  reference), `docs/vpn/limits_win_mac/VPN_ANDROID_ACTUAL_LIMIT.md` (hard-wall vs
  v1-scoped-deferred vs unverified, 12 gaps), and `docs/vpn/VPN_ANDROID_ACCEPTANCE.md`
  (T-AND-M1..M5 manual procedure, no gateway scenario since Android is host-only)
  are all written and cross-checked against `src/vpn.rs`. Release pipeline verified:
  `docker/Dockerfile.android` already builds with `--all-features`, so the shipped
  `bore-aarch64-linux-android` release binary already ships the `vpn` feature —
  no pipeline change needed. The only remaining work on this plan is running
  T-AND-M1..M5 by hand on physical hardware (tracked in `resume.md`).

**SSH ingress gateway (`--ssh-gateway`, feature `ssh-gateway`, branch `ssh`, IMPLEMENTED
2026-07-04 — Phases 1-7 done, `docs/SSH_GATEWAY.md` §6 is the operational guide):** embeds a
`russh` server in `bore server` so a stock OpenSSH client (`ssh -R`/`-L`, no `bore` binary) can
create public/vhost/secret tunnels. Ingress-only: from the accepted SSH channel inward, reuses
the existing registries/relay/admin/weblog/`--max-conns` data path unmodified.
- **D1 (naming heuristic, `parse_forward_spec`, `src/sshgw.rs`):** bare numeric port ⇒ public;
  bare label + port 80/443 ⇒ vhost; bare label + port 0 ⇒ secret provider; any other bare-label
  port is ambiguous and rejected. `vhost/<label>`/`secret/<id>` prefixes in the bind address
  override the heuristic explicitly, any port. `direct-tcpip` (`-L`) to `<id>`/`secret/<id>`
  is always a secret consumer (port is an ignored nonzero placeholder — OpenSSH's `-L` CLI
  rejects a literal `0`, unlike `-R`).
- **SSH leg = TCP relay only, never UDP/carriers.** No `--udp`/QUIC direct path, no
  `--carriers>1`, no hole-punch flags on the SSH leg — those are native-bore-client-only
  features; a client naming one via `exec`/env gets an explicit warning (I-SSH2), never silent
  drop. A native client and an SSH client can still coexist on the same tunnel name space and
  even the same demuxed port simultaneously.
- **I-SSH1:** `--ssh-gateway` off (or the `ssh-gateway` feature compiled out) ⇒ the control-port
  accept path is BYTE-IDENTICAL to before this feature existed — confirmed by `git diff` showing
  zero removed lines in the legacy accept loop (`src/server.rs`), not just "no visible bugs".
- **I-SSH2:** every client-transport-only parameter named via `exec`/`SetEnv`/authorized-keys
  options gets an explicit warning line, never silently ignored or misapplied.
- **I-SSH3:** keepalive 20s / reaper 60s in parity with the native secret-tunnel zombie-entry
  invariant — no ghost admin rows. A REAL netfilter half-open (not a process kill) exposed a
  genuine bug here: the vhost/secret finalize task's own strong `Arc<ConnState>` clone (held
  for the task's entire `pending()`-forever lifetime) created a reference cycle that kept
  `Drop for ConnState` (which aborts every task in `self.forwards`) from ever firing on an
  ungraceful connection death. Fixed with an explicit `drop(state)` before each `pending()` tail
  in `src/sshgw.rs` — found and verified by `scripts/ssh_gateway_test.sh`'s T-SSH-N1, which
  cargo tests structurally cannot reproduce (no real network stack to netfilter-DROP). The
  SAME reference-cycle shape (`state` captured for `await_params`/`queue_message`, never used
  again, then a long-lived tail future) existed in the PUBLIC-tunnel `tcpip_forward` task too
  — missed by that first pass because its tail is `run_public_forward`'s real accept loop, not
  a `pending()`, so it wasn't grepped alongside the other two. Symptom: Ctrl+C on an `ssh -R
  <port>:...` client left the bound listener (and the admin row) alive forever — the next
  attempt to reuse that port failed to bind. Fixed the same way (`drop(state)` right after its
  last use, before the accept loop starts). Whenever a NEW `tcpip_forward_*`/exec-consuming
  task is added, it needs the same explicit `drop(state)` if `state` is unused past setup and
  the task tail outlives the connection — the compiler will NOT drop it early on its own
  (lexical scope, not liveness, governs async drop timing here).
- **I-SSH4:** `mux::LinkOpener::Ssh`/`SshOpener::open` never writes the yamux `STREAM_READY`
  marker byte — SSH has no equivalent framing; the caller IP travels as the channel-open
  request's own originator-IP field instead.
- **I-SSH5 (D-SSH2):** same-identity takeover — a new SSH session authenticated with the SAME
  identity (authorized-keys comment, or password label) as the incumbent holder of a vhost
  label/secret id evicts and replaces it (useful for deterministic `autossh` reconnection); a
  DIFFERENT identity, or a name held by a *native* (non-SSH) tunnel, is always rejected — SSH
  identities and the HMAC secret are different trust domains, never mixed.
- **I-SSH6 (`shell_request` must never force-close, bug-hunt 2026-07-05):** a bare `ssh -R`/
  `-L` (no `-N`, no `exec` command — the ordinary invocation) still gets OpenSSH's *default*
  behavior of ALSO requesting an interactive shell on the session channel. The gateway used to
  deny it with `exit_status(1)` + `eof` + `close`; OpenSSH treats its "primary session"
  exiting nonzero as reason to disconnect the WHOLE connection, tearing down every active
  `-R`/`-L` forward on it too — a real report: a bare `-R secret/id:0:localhost:8080` got
  "interactive shells are not supported" and the just-granted forward died with it. Fixed by
  NEVER closing this channel with a nonzero exit from `shell_request` — it is held open
  instead (this is also what makes I-SSH7 possible). Deliberately does NOT special-case "zero
  forwards yet" into a hard rejection either: a secret *consumer* (`-L <port>:secret/<id>:1`)
  has no `tcpip-forward`-equivalent to announce itself in advance — the server only learns
  about it when a real proxied connection opens a `direct-tcpip` channel, which can be well
  after the shell request fires. Closing for "nothing YET" would silently reintroduce the same
  bug for a consumer whose first connection hasn't happened yet. Only a best-effort,
  NON-closing informational line (`NO_FORWARD_YET_MESSAGE`) is printed when nothing is known
  (`ConnState::has_forwards`/`has_secret_consumers` both empty) — never destructive either way.
- **I-SSH7 (tunnel info banner, same bug-hunt):** once a vhost/public/secret-provider/
  secret-consumer forward finishes establishing, the gateway writes a short, professional,
  English status report to the session channel (`vhost_info_banner`/`public_info_banner`/
  `secret_provider_info_banner`/`secret_consumer_info_banner` + `ConnState::deliver`, all in
  `src/sshgw.rs`) — this is the fix above's actual payoff, not just a side effect. **Never**
  reports the client's own `-R`/`-L` local host:port: RFC4254's `tcpip-forward`/`direct-tcpip`
  wire messages have no field for it — it is pure client-local state the server cannot know,
  and guessing would be actively misleading in the one place a user is checking for the truth.
  Delivery is via `Handle::data(channel_id, ..)` — works from any task holding a cloned
  `Handle`, no `Session`/dispatch-loop access needed — which is essential because a forward's
  *final* state (bound port, cert-missing https downgrade, resolved headers) isn't known until
  well after `channel_open_session` already fired and did its one-shot drain (crosses
  `PARAMS_GRACE`). `ConnState::session_channel` (set by `channel_open_session`, ordered BEFORE
  its drain so a racing `deliver` can never fall in the gap) is what lets `deliver` target the
  channel directly instead of only queueing. Secret-provider's banner includes the exact
  consumer command with `<same-host>`/`<same-port>` placeholders (never a GUESSED hostname);
  `--ssh-advertise-address HOST` + `--ssh-advertise-port PORT` (env
  `BORE_SSH_ADVERTISE_ADDRESS`/`BORE_SSH_ADVERTISE_PORT`, 2026-07-08) let the OPERATOR
  declare the public endpoint (a front proxy rewrites the port and SSH has no Host/SNI, so
  the server can't derive it) and the command prints ready-to-copy — the two flags are
  independent; whichever is unset keeps its placeholder (zero-regression default). Unit:
  `secret_provider_banner_consumer_command_advertise`. Related: NO_FORWARD_YET_MESSAGE
  explicitly notes the `-L` secret-consumer case is normal (the forward activates on the
  first local connection — the server cannot see a `-L` earlier; `-T` only skips the PTY,
  the session+shell request still happens, so that message is expected even with `-T`).
  Secret-consumer's banner fires exactly ONCE per session (`consumer_entry` now returns
  `(entry, is_new)`), not once per proxied connection (D11 parity). **Corollary — `-N` is now
  universally discouraged, not just when passing `exec` params:** `-N` (`SessionType=none`)
  was confirmed empirically (not just per RFC text) to skip opening a channel AT ALL, so a
  `-N` client can never see this banner (or any warning) regardless of exec params — every
  doc example was updated to drop `-N` (`docs/SSH_GATEWAY.md` §6.4a, `README-SSH-GATEWAY.md`
  §4's box). Test coverage: `t_ssh_banner_vhost_no_n_survives_and_reports` /
  `t_ssh_banner_public_no_n_survives_and_reports` / `t_ssh_banner_public_https_reports_enabled`
  / `t_ssh_banner_secret_provider_no_n_survives_and_reports` /
  `t_ssh_banner_secret_consumer_fires_once` / `t_ssh_nokill_zero_bare_interactive_still_rejected`
  in `tests/ssh_gateway_test.rs`.
- **I-SSH8 (inapplicable params must warn, real report 2026-07-05):** `https=on
  force-https=on` on a VHOST forward (HTTPS there is governed server-side by `--vhost-mode`,
  never per-tunnel) was silently swallowed — `parse_params`'s own `warnings` can't catch this
  class of bug because it's built generically, before the caller knows which forward TYPE the
  exec string even applies to. Secret provider had the same gap for
  `https`/`force-https`/`basic-auth`/`webserver-log`/`max-conns` (ALL hardcoded `false`/`None`
  in its admin entry regardless of what was requested — only `notes` was ever applied there).
  Fixed with `deliver_inapplicable_warnings` (`src/sshgw.rs`), called from each
  `tcpip_forward_*` task after it resolves its own `params` copy, checking exactly the fields
  that ARE no-ops for THAT type and warning once per set field via `ConnState::deliver` (same
  mechanism as I-SSH7's banner) — never silent, matching I-2. Test coverage:
  `t_ssh_warn_https_inapplicable_to_vhost` / `t_ssh_warn_all_params_inapplicable_to_secret_provider`.
- **Control-port demux (D8, Phase 6):** with the gateway enabled, `sshgw::demux_pre_tls` peeks
  the first byte (2s timeout — a real SSH client waits for the server's own banner and sends
  nothing first, sslh-style) and 3-way classifies it: `Ssh` (timeout or `b'S'`) dispatches
  straight to the gateway; `Tls` (0x16) goes through the configured TLS acceptor, then a second
  4-byte peek (`demux_post_tls`) checks for a literal `SSH-` prefix (SSH-over-TLS via a
  `ProxyCommand openssl s_client` tunnel, D4); anything else (`Direct`: HTTP/bore) routes
  STRAIGHT to `route_connection`, BYPASSING any configured TLS acceptor entirely — this is what
  lets a plain HTTP/bore client keep working on a port that also serves TLS. `SshGateway::
  serve_connection` is generic over `mux::Transport` so it runs identically over `TcpStream`,
  `Prefixed<TcpStream>`, and a `TlsStream`.
- **I-SSH9 (ALPN-first post-TLS demux — the browser-preconnect misroute, field bug 2026-07-06):**
  inside TLS, "silence ⇒ SSH" ALONE is WRONG: a browser's speculative/pool HTTPS connections
  (preconnect, spare sockets for parallel assets) complete the TLS handshake then idle PAST the
  2s `SSH_PEEK_TIMEOUT` before their first request — the old post-TLS fallback handed them to
  russh, whose `SSH-2.0-russh_…` banner rendered as the page body, and the poisoned sockets sat
  in the browser pool (requests stuck `pending`, missing assets, refresh not healing). FIX: the
  demux consults the ClientHello ALPN offer FIRST (`sshgw::accept_tls_with_alpn` via
  `LazyConfigAcceptor` + `demux_classify_alpn`): any ALPN ≠ `ssh` (browsers `h2`/`http/1.1`,
  native bore `bore`) ⇒ NEVER SSH — routed via `route_connection_known_http` (60s
  `HTTP_ALPN_FIRST_REQUEST_TIMEOUT` for the first request since ALPN already proved HTTP;
  timeout ⇒ clean close, NEVER the bore-protocol path); ALPN literally `ssh` ⇒ gateway
  immediately (no 2s wait; document `-alpn ssh` for ProxyCommand users); NO ALPN (stock
  `openssl s_client`) ⇒ the legacy silence peek (D4 preserved). The native client offers ALPN
  `bore` (`transport.rs::client_config`) — wire-compatible both ways (a rustls server with no
  `alpn_protocols` configured ignores the offer; bore servers never set it). Do NOT re-collapse
  the post-TLS demux to a pure timeout, and do NOT hand an ALPN-http connection that idles out
  to `handle_connection` (garbage-close mid-request is exactly the reported instability).
  Regression: T-SSH-DMX3 (ALPN http + idle 4s ⇒ HTTP response, never a banner — red on the old
  code at ~2s) + T-SSH-DMX4 (`-alpn ssh` ⇒ banner) + `demux_classify_alpn_table` unit.
- **I-SSH10 (bounded channel-open + wedged-session eviction, resilience hunt 2026-07-06):** RFC
  4254 has no deadline for a CHANNEL_OPEN reply, so a wedged-but-TCP-alive OpenSSH client (frozen
  process/suspended laptop: kernel ACKs+answers nothing; or zero-window peer that jams the session
  loop in `flush_into`, where keepalives are never even SENT so `keepalive_max` reaping is blind)
  left every proxied connection `pending` FOREVER holding its `--max-conns` permit — field symptom:
  stalled pages that only a manual ssh-client restart healed. FIX, two independent layers: (1) EVERY
  server-initiated `forwarded-tcpip` open is bounded by `ssh_open_timeout()` (15 s;
  `BORE_SSH_OPEN_TIMEOUT_MS` test override) — `SshOpener::open` (vhost+secret, timeout INSIDE the
  opener so native-consumer relays through an SSH provider are covered too) and
  `run_public_forward`'s inline open (which otherwise froze the whole PUBLIC accept loop, one conn
  wedging all). An ANSWERED open — even `ChannelOpenFailure` (app down/restarting) — resets the
  counter; `SSH_OPEN_TIMEOUT_EVICT=2` consecutive TIMEOUTS ⇒ `ConnState::evict()` ⇒
  `serve_connection`'s `select!` calls `RunningSession::abort()` (vendored russh: tokio
  `JoinHandle::abort`, run_stream SPAWNS — dropping the future does NOT stop the session; and never
  `Handle::disconnect`, which rides the same possibly-wedged dispatch loop) ⇒ RAII teardown frees
  label/port/admin row ⇒ autossh reconnects = the manual fix, automated. (2) vendored russh:
  `WindowSizeRef` carries a `closed` flag; `Drop for ChannelRef` (single owner, session channel map
  — covers close_with/drain/session-death/abort) calls `close()` which wakes+errors
  (`BrokenPipe`/`SendError`) any writer parked on the window `Notify` — before this, a
  `ChannelTx`/`send_bytes` blocked on an exhausted window whose channel then closed parked FOREVER
  (splice-task + permit leak; upload-shaped). The opener state handle is `Weak<ConnState>` — a
  strong clone inside the pool/accept-loop would resurrect the I-SSH3 reference cycle. Full
  app-restart matrix (dufs+python, TLS/ALPN, keep-alive across restart, mid-flight kill, uploads,
  client & server-timed rekey crossings) was reproduced RESILIENT on localhost pre-fix — the wedge
  needs a peer that stops answering while TCP stays alive, which is exactly what the regression
  tests do with SIGSTOP. Regression: `t_ssh_i10_wedged_client_vhost_evicts_and_recovers` /
  `t_ssh_i10_wedged_client_public_evicts_and_frees_port` (SIGSTOP the real ssh; assert fast-fail,
  eviction, port/label release, reconnect serves again) + russh units
  `data_bytes_window_wait_errors_on_close` / `channel_tx_window_wait_errors_on_channel_ref_drop`.
  Vendored-delta log: `crates/russh/HOL_FIX.md`. Rekey diagnostics env: `BORE_SSH_REKEY_BYTES`
  / `BORE_SSH_REKEY_SECS` (server-side russh never initiates count/time rekey in practice — the
  OpenSSH client's own RekeyLimit fires first under load; both directions verified non-wedging).
- **I-SSH11 (originator truth + label caps, prod-readiness audit 2026-07-10):**
  `mux::ChannelOpen::open`/`LinkOpener::open_ready` carry `caller: Option<SocketAddr>`
  — SSH-ONLY input for the RFC 4254 `forwarded-tcpip` originator ip+port (previously
  hardcoded port 0, ip only when webserver-log). The mux wire is governed SOLELY by
  `forward_ip` (`Some ⟺ client_wants_logging`, bare IP — access-log format) and stays
  byte-identical whether/what `caller` is passed — never serialize it there. Native
  secret relay passes the consumer control-conn `peer`; SSH direct-tcpip passes the
  `-L` client's own originator fields (best-effort parse, degrade to `0.0.0.0:0`,
  never fail the open). Also `validate_label(label, max_len)`: vhost ≤ 63 (DNS label
  limit), secret id ≤ 128 (roomier for native `--tcp-secret-id` interop) — bounds
  attacker-chosen registry/admin-JSON strings. Full audit (5 dimensions, rejected
  findings incl. why SO_*BUF on SSH sockets would HURT — clamps to `net.core.*mem_max`
  and kills autotuning; remediation is sysctl-level):
  `docs/SSH_GATEWAY_ASSESSMENT_2026-07-10.md`.
- Regression/e2e: `tests/ssh_gateway_test.rs` (cargo, 36 tests incl. takeover, demux, SSH-over-TLS,
  the I-SSH6/I-SSH7/I-SSH8 shell-request-fix/banner/inapplicable-param-warning suite, the I-SSH10
  wedged-client pair) + `sudo -n /abs/path/scripts/ssh_gateway_test.sh` (netns chaos: T-SSH-N1..N6
  — real netfilter half-open, autossh recovery across a server restart, takeover under partition,
  mixed transports on one port, throughput report, password auth). Exact-path sudo invocation only
  (`sudo bash scripts/...` prompts and must not be used).

**Version string:** `bore <semver> - <branch> - <sha8>` — embedded at compile time via `build.rs`
(`BORE_GIT_BRANCH`/`BORE_GIT_SHA` → `GITHUB_REF_NAME`/`GITHUB_SHA` → `git` CLI). Run `cargo build` to regenerate.