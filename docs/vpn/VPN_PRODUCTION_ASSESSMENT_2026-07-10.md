# VPN Production-Readiness Assessment — 2026-07-10

Full audit of the `bore vpn` feature (all modes: 1:1 listen/connect, hub-and-spoke
`--max-clients N`, overlapping-subnet NAT, relay + direct/QUIC paths, macOS/Android/
Windows twins). Method: 7 parallel Sonnet audit agents across 5 dimensions
(races/deadlocks, resource leaks, server/protocol, holepunch/direct, data-plane
perf, liveness/error-paths, NAT/NetConfig/platform), each finding then
adversarially re-verified against the actual code by Opus before any fix.

Branch `ssh`. Baseline before changes: `cargo fmt`/`clippy -D warnings`/`cargo test`
green; `vpn_netns_test.sh` 161/0.

## Fixed (4)

### F1 — HIGH — Hub orphan tasks leak across reconnects
`src/vpn.rs` `run_listen_hub`. The accept task (`spawn_accept_task`) and the hub
coordinator (`run_hub_coordinator`) were bound to `_`-prefixed locals. Dropping a
`tokio::JoinHandle` does **not** abort the task, and the `select!` awaited only the
ctrl task and the router uplinks — so on every hub-link teardown (ctrl close, router
death, or a reconnect) both tasks kept running, holding the TUN acceptor, the shared
per-peer table and the sub/event channels. One orphan set accumulated per reconnect.
(Per-peer downlink/upgrade tasks self-terminate when their relay/direct I/O closes,
so they were not affected — only the two tasks whose inputs outlive the link.)
**Fix:** an `AbortOnDrop<T>` guard wraps both tasks so they are aborted on *every*
return path; the surviving router uplinks are explicitly aborted after the `select!`.

### F2 — MED (perf) — Relay AEAD key schedule rebuilt per packet
`src/vpn.rs` `crypto`/`link`. `seal_with_counter`/`open` rebuilt `UnboundKey` +
`LessSafeKey` (a fresh ChaCha20-Poly1305 key schedule) on *every* relay frame — one
per packet on the relay data path. **Fix:** new `crypto::SealKey` computes the key
schedule once at link setup; `LinkSender::Relay.key` and `relay_reader` now carry
`Arc<SealKey>`. Wire format byte-identical (gate: `sealkey_matches_free_fns`).
Direct/QUIC path unaffected (it uses QUIC's own crypto, never this AEAD). The shared
nonce counter (I-5/DEC-6), per-datagram round-robin and single-task stream ownership
are unchanged.

### F3 — MED (hardening) — Unbounded wire-supplied `id` / `advertised`
`src/vpn_server.rs`. `HelloVpn`/`ConnectVpn` `id: String` and `advertised: Vec<Ipv4Net>`
were used as registry keys, `vpn:{id}` UDP keys, admin labels and log subjects with no
length bound. **Fix:** `validate_link_params` caps id at `MAX_VPN_ID_LEN` (128, parity
with the SSH-gateway secret-id cap I-SSH11) and the advertised list at
`MAX_ADVERTISED_CIDRS` (64), checked at the top of `serve_vpn_listener`/
`serve_vpn_connector` before any allocating work; over-limit → `VpnError` + clean
return. Gate: `validate_link_params_bounds`.

### F4 — LOW (hardening) — Hub-state lock poison cascade
`src/vpn_server.rs`. Hub `state.lock().unwrap()` sites would cascade-panic every
subsequent connector if any lock holder ever panicked, while the pool locks in the
same file already used `unwrap_or_else(|p| p.into_inner())`. **Fix:** aligned all
hub-state locks to `into_inner`; collapsed three sequential locks (hub_overlay/prefix/
advertised) into one.

## Verified and REJECTED (not bugs)

- **Hub peer-alloc "off-by-one"** — `alloc_peer` scans for the lowest free address;
  `next_peer_id` is only a monotonic id, not an address. Capacity check + alloc under
  one lock. Correctly bounded by `--max-clients`.
- **`NetConfig::apply` ip_forward partial-apply leak (agent: "CRITICAL")** —
  `applied_ops.push(AppliedOp::IpForward)` is at vpn.rs:5983, *before* the fallible
  `route_get`; the state file + fwdref refcount marker are also written before it. A
  graceful error reverts via `Drop`; SIGKILL recovers via `stale_reclaim`. Ordering
  correct; agent misread the line order.
- **Zombie-peer during direct upgrade / lock-across-await in hub router** — the peer
  `Arc` keeps state valid; the router's per-peer `Mutex<LinkSender>` critical section
  is tiny and `send_batch` is non-blocking on the hub path by design (a blocking peer
  would HOL every other peer). No data race, no deadlock.
- **Holepunch/direct path** — full read: socket lifecycle (drop-before-bind, no
  `SO_REUSEADDR`), token-auth gate on `DirectListener::accept`, STUN parse bounds,
  `DatagramSend::TooLarge` typed handling, carrier-count abort-on-mismatch, PMTU edge
  cases, argv (no shell interpolation) all correct.
- **Command injection in nft/iptables/ip/pfctl builders** — all argv-vector based; no
  user string interpolated into a shell or nft script.
- **Naive VpnReady-recv timeout (agent: "CRITICAL, blocks release")** — a listener
  legitimately waits indefinitely for a peer to connect; a fixed timeout would break
  normal slow pairing. The post-pairing life is already covered by the ctrl-actor 60 s
  heartbeat timeout. Left as-is by design (see Known limitations).

## Skipped (speculative, risk > reward on a documented-sensitive hot path)

Data-plane micro-opts (GRO header zero-fill pool, relay frame triple-copy, batch Vec
reuse): each <2–10 % and speculative; the agent's "30–50 % / 5000 cycles per packet"
figure for F2 was AES-GCM-shaped and overstated for ChaCha20. F2 (key caching) was the
one clean, contained win and was taken; the rest touch the most invariant-dense module
for marginal gain and were not pursued.

## Known limitations (documented, not regressions)

- Pre-pairing control wait (listener waiting for a connector) is unbounded by design;
  a wedged server before first pairing is undetectable until a peer connects. Proper
  fix would be a pre-pairing heartbeat (protocol change) — out of scope for this pass.
- Host-only hub isolation still relies on host `ip_forward=0` when no `--advertise`
  (pre-existing v1 gap).

## Gates

- `cargo fmt --check`, `cargo clippy --features vpn --all-targets -D warnings`,
  `cargo test --features vpn` — all green (390 lib + all integration; 2 new unit tests).
- `sudo -n scripts/vpn_netns_test.sh` — **161/0**, incl. hub multi-spoke, direct/relay,
  port-clash flap, and a 90 s concurrent mixed-load stability window. Zero regressions.
