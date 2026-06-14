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

**Key invariants to never break:**
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
- Relay path is AEAD-opaque: server splices ciphertext, never plaintext IP packets
- **Never `tokio::io::split` a `mux::Stream` across two tasks.** `yamux::Stream` keeps a
  single parked-task waker on its internal channel (`poll_read` and `poll_write` both call
  `sender.poll_ready`); two tasks overwrite each other's waker and the loser is never woken
  — the stream wedges silently under load. One stream = one task. The VPN relay uses two
  unidirectional substreams (tags `0x01`/`0x02`) for exactly this reason. Single-task
  bidirectional use (`copy_bidirectional`, `try_join!` in one task) is safe.
- VPN relay queue applies backpressure (await on full), never silent drops; VPN clients
  must keep draining the control stream after `VpnReady` (heartbeats + server-death detection;
  the ctrl actor in `vpn.rs` is the stream's single owner — route new control messages through it)
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
- `NetConfig` RAII: all routes/nft/ip_forward changes revert on exit (SIGINT, SIGTERM, panic handled; SIGKILL requires next-run stale reclaim via /run state file to restore ip_forward and remove leaked iptables/nft rules — BUG-2/BUG-3 fixed)
- TUN MTU default 1350: clamps QUIC datagram size; gateway MSS-clamp keeps forwarded TCP healthy
- VPN direct path: a `TooLarge` datagram send is a per-packet DROP, never link death. The TUN MTU
  runs ahead of the QUIC path MTU right after every direct switch, so full-size packets exceed
  `max_datagram_size()` until the PMTU monitor narrows the TUN. `DirectConn::send_datagram` returns
  the typed `DatagramSend::{Sent,TooLarge}` (NOT a stringly error — quinn's `Display` for
  `SendDatagramError::TooLarge` is `"datagram too large"`, so substring-matching `"TooLarge"`
  silently never fired and killed the link). `send_batch` returns the drop count; only genuine link
  death returns `Err`. PMTU monitor shrinks immediately on one below-current sample
  (`pmtu_shrink_now`, fast recovery), grows only on 3 stable samples (`pmtu_decision`, anti-flap)
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

**Version string:** `bore <semver> - <branch> - <sha8>` — embedded at compile time via `build.rs`
(`BORE_GIT_BRANCH`/`BORE_GIT_SHA` → `GITHUB_REF_NAME`/`GITHUB_SHA` → `git` CLI). Run `cargo build` to regenerate.