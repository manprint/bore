# CLAUDE.md

## Instructions for CLAUDE and optimizations.

### Model selection per task

Three tiers, each with a clear role. Using the wrong one wastes money or quality.

**Haiku 4.5** (`claude-haiku`) — fast, cheap ($1/$5 per MTok)
- Linting, grep-style code search, syntax checks
- Routing/classification decisions in multi-agent flows
- Extracting structured data from text (parse logs, format JSON)
- Generating short repetitive outputs (commit messages, variable names)
- Sub-agent tasks where the work is mechanical, not reasoning-heavy

**Sonnet 4.6** (`claude-sonnet`) — default for 90%+ of tasks ($3/$15 per MTok)
- Implementing features, refactoring, writing tests
- Debugging non-trivial bugs
- Writing/reviewing documentation
- Code review with explanation
- Agentic loops that need sustained focus but not peak reasoning

**Opus 4.8** — complex tasks where quality delta is worth 5–10× cost
- Architecture decisions across many files
- Multi-step reasoning that Sonnet visibly gets wrong
- Deep research synthesis
- `opusplan` alias: Opus for plan mode only, auto-switches to Sonnet for codegen

**Rule of thumb**: start with Sonnet. Drop to Haiku for bulk/mechanical sub-tasks.
Escalate to Opus only when Sonnet output is concretely insufficient.

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
  `--relay-only`). Path switch = controlled bridge restart (DEC-1: stop pumps, drop relay halves,
  respawn on Direct). Direct death at runtime kills the bridge → handled by reconnect (DEC-2).
  Server brokers `UdpPunch` to BOTH sides only when it holds BOTH offers (DEC-3, 10 s timeout →
  `UdpUnavailable`)
- VPN AEAD nonce counter is ONE shared `Arc<AtomicU64>` per egress key (I-5/DEC-6): carriers
  and multi-queue clones all `fetch_add` on it — never per-producer counters, never two seals
  with the same `(key, counter)`. Relay carriers round-robin per-datagram (DEC-7, reorder OK);
  any future replay window (B1) must size for that reorder: ≥ 2 × (carriers × RELAY_QUEUE)
  (DEC-10)
- VPN `--carriers`/`--tun-queues` default 1 = byte/path-identical to the single configuration
  (I-9). Carrier count negotiated min(listener, connector, server `--max-carriers`); a dead
  carrier kills the whole link cleanly (reconnect re-establishes), never silent degradation
- `NetConfig` RAII: all routes/nft/ip_forward changes revert on exit (SIGINT, SIGTERM, panic handled; SIGKILL requires next-run stale reclaim)
- TUN MTU default 1350: clamps QUIC datagram size; gateway MSS-clamp keeps forwarded TCP healthy
- VPN direct path: a `TooLarge` datagram send is a per-packet DROP, never link death. The TUN MTU
  runs ahead of the QUIC path MTU right after every direct switch, so full-size packets exceed
  `max_datagram_size()` until the PMTU monitor narrows the TUN. `DirectConn::send_datagram` returns
  the typed `DatagramSend::{Sent,TooLarge}` (NOT a stringly error — quinn's `Display` for
  `SendDatagramError::TooLarge` is `"datagram too large"`, so substring-matching `"TooLarge"`
  silently never fired and killed the link). `send_batch` returns the drop count; only genuine link
  death returns `Err`. PMTU monitor shrinks immediately on one below-current sample
  (`pmtu_shrink_now`, fast recovery), grows only on 3 stable samples (`pmtu_decision`, anti-flap)

**Version string:** `bore <semver> - <branch> - <sha8>` — embedded at compile time via `build.rs`
(`BORE_GIT_BRANCH`/`BORE_GIT_SHA` → `GITHUB_REF_NAME`/`GITHUB_SHA` → `git` CLI). Run `cargo build` to regenerate.