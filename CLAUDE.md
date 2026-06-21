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
- Client sends `Hello` before auth (yamux is lazy; without it, deadlock)
- `HelloVpn`/`ConnectVpn` sent **before** auth (same lazy-yamux rule as `Hello`)
- Server writes `mux::STREAM_READY` before splice (banner-first protocols need it)
- `copy_bidirectional_with_sizes` propagates half-close; do not replace with a non-half-close variant
- `shared::tune_tcp` (`TCP_NODELAY` + `SO_KEEPALIVE 15s`) must be applied to every new socket
- `--max-conns` semaphore is the real bound; yamux stream limit is set generous intentionally
- `carriers<=1` keeps the single-connection path byte-for-byte unchanged. Default is `1`
  for `local`/`proxy`, but `0` (auto) for `bore transfer` — auto scales the relay carrier
  pool to the worker `--parallel` count (capped at server `--max-carriers`); `transfer.rs`
  resolves it via `resolve_carriers`. Explicit `--carriers 1` still forces the single path.
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

**VPN macOS port (groundwork only, runtime PENDING a Mac):**
- The `vpn` module + `Vpn` subcommand are gated `cfg(all(feature="vpn", target_os="linux"))` — VPN
  is **Linux-only today**. macOS port plan: `docs/vpn/VPN_MACOS_PORT_PLAN.md`; backend ref:
  `docs/vpn/VPN_MACOS.md`. Decisions LOCKED: Apple Silicon macOS 13+, `--forward-accept`=PF `pass`,
  GitHub macos CI runner, Windows deferred.
- DEC-M1 (zero-regression contract): the Linux `NetConfig::apply`/`Drop`/`stale_reclaim` + ALL
  `cmd_nft_*`/`cmd_iptables_*`/`cmd_*` builders stay BYTE-FOR-BYTE under `#[cfg(target_os="linux")]`.
  macOS is an additive `#[cfg(target_os="macos")]` twin (compile-time split, NOT a runtime trait).
  Reuse the generic `CommandRunner` + `revert_cmds` argv stack + `NetConfig` fields + the
  platform-agnostic data plane (bridge/AEAD/carriers/relay/QUIC/PMTU).
- LANDED (2026-06-16): `Cargo.toml` makes `tun-rs` available on the macOS target (`procfs` stays
  Linux-only); `hostcfg_cmd::macos` has the full PURE builder set + `pf_ruleset` composer (macOS twin
  of `gateway_nft_cmds`) + `parse_lan_iface`, snapshot-tested on the Linux CI. PF mapping: `binat`=1:1
  netmap (host-bit preserving), `nat`=masquerade, `scrub max-mss`=MSS clamp, `block`=spoke isolation.
  PF syntax is PROVISIONAL until the Phase 0 Mac spike. The module `cfg` gate is NOT flipped — Linux
  build/runtime byte-identical, macOS still has no `vpn` subcommand until the runtime lands.

**Version string:** `bore <semver> - <branch> - <sha8>` — embedded at compile time via `build.rs`
(`BORE_GIT_BRANCH`/`BORE_GIT_SHA` → `GITHUB_REF_NAME`/`GITHUB_SHA` → `git` CLI). Run `cargo build` to regenerate.