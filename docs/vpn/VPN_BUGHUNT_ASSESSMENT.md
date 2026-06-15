# VPN Bug-Hunt Assessment — direct / relay / hub / crypto-nonce / reconnect

> Execution of `VPN_BUGHUNT_TODO.md`, 2026-06-15. Areas A–E. Discipline: every
> finding a hypothesis until reproduced; subagent output verified independently
> (Opus). Crypto + race analysis on Opus; static audits fanned to Sonnet 4.6;
> the three subagent-CONFIRMED findings were each re-judged by Opus — two were
> downgraded/refuted (see E2, B5).

## Verdict

The VPN is in **solid shape**. After auditing all five areas, **one real (narrow)
bug** was found and fixed (B3, ip_forward teardown race), plus **two hardening
fixes** (B1 classifier regression-guard, B5 server-death detection) and the
**crypto nonce-uniqueness harness** the TODO flagged as the highest-value
artifact. The crypto core, path-switching state machine, hub data plane, and
relay carriers held up — no nonce reuse, no yamux-split, no silent drops, no
fake-direct paths. Several subagent "CONFIRMED" claims were **false positives**
caught on independent review.

| ID | Sev | Status | One-line |
|----|-----|--------|----------|
| A1 | — | SAFE | nonce counter preserved across in-place fallback; reset only on fresh-key reconnect |
| A2 | low | ACCEPTED | no relay replay window — out of threat model (semi-trusted relay, best-effort IP); documented |
| A3 | — | SAFE | per-peer keys from per-peer CSPRNG nonce, RAW into HKDF, own counter |
| A4 | — | SAFE | u64 counter, bails at `MAX-1` (~58 000 yr @ 10 Mpps); no u32 in the fan-out |
| B1 | low | **FIXED (guard)** | classifier is a bare substring match — currently correct, now pinned by a regression test |
| B2 | — | REFUTED | reconnect is serial; old `NetConfig` Drop reverts before the next apply |
| B3 | **med-low** | **FIXED** | concurrent gateways on one host raced `/proc` ip_forward; refcount added |
| B4 | — | REFUTED | single-path death falls back in place; never a TUN-destroying reconnect |
| B5 | low | **FIXED** | 1:1 ctrl actor had no app-level read timeout (hub did); added — catches a wedged server |
| C1 | — | SAFE | per-peer sender swap is `Mutex`-serialized; router drops a per-peer send error, never dies |
| C2 | — | SAFE | router never restarts on a path switch; swap is in-place |
| C3 | netns | OPEN | spoke isolation `iifname/oifname bore0 drop`; host-only-hub gap is a known v1 doc item |
| C4/C5/C6 | — | REFUTED | monotonic peer_id, bounded pending buffers, no unawaited-future |
| D3/D4/D5 | — | CLEAN | candidate filter correct; PMTU is typed (no `contains("TooLarge")`); broker re-arms per round |
| D-yamux | — | CLEAN | only a comment mentions `io::split`; relay uses two unidirectional substreams |
| E1/E3/E4 | — | SAFE | all enqueues await (backpressure); reorder-tolerant; `carriers=1` byte-identical |
| E2 | — | REFUTED | a dead carrier kills the link cleanly — the bridge `select!` aborts the survivor |

---

## A. Crypto / AEAD nonce  (HIGHEST)

**Conclusion: no nonce-reuse bug.** ChaCha20-Poly1305, 96-bit nonce =
`[0;4] ‖ counter_be(8)` (`vpn.rs` `crypto::nonce_from_counter`). The counter is
transmitted in each frame; the receiver opens with the frame's counter and keeps
**no** counter state of its own. Uniqueness therefore rests entirely on never
sealing twice with the same `(egress_key, counter)`.

- **A1 — in-place fallback (DEC-2).** The relay `LinkSender` holds ONE
  `Arc<AtomicU64>` counter. In the bridge (`vpn.rs:run`) `relay_sender` is cloned
  once at the top and every `FallBackToRelay` re-clones the SAME Arc — the
  counter is **preserved**, never reset, for the link's whole life. The hub
  per-peer task captures `peer.sender.lock().clone()` (same Arc) and swaps back
  to it on direct death. **Verified preserved.** A full reconnect negotiates a
  **fresh** `session_nonce` (`vpn_server.rs::new_nonce`, `ring` CSPRNG) ⇒ fresh
  key ⇒ counter reset to 0 is safe.
- **A2 — replay.** `crypto::open` has no replay/dedup window. A relay-path MITM
  (the bore server) could replay captured ciphertext frames; the receiver would
  decrypt and write them to the TUN. **Out of the threat model:** the relay is
  the user's own semi-trusted server, the data plane is best-effort IP, and TCP
  dedups replays. Cross-link replay is impossible (per-link key). **Accepted for
  v1; documented** as a known limitation. (If ever in scope, size the window
  ≥ `2 × carriers × RELAY_QUEUE` for carrier reorder, DEC-10.)
- **A3 — hub per-peer isolation.** Each peer derives keys from its OWN CSPRNG
  `session_nonce` passed RAW (`derive_keys_listener(secret, &nonce)`, `vpn.rs`)
  with its OWN counter (fresh `make_relay_multi`). Distinct nonces ⇒ distinct
  keys, so two spokes never share a keystream though both counters start at 0.
- **A4 — overflow.** `seal`/`seal_with_counter` bail at `MAX_COUNTER = u64::MAX-1`.
  No narrower counter anywhere in the carrier/queue fan-out.

**New artifact:** `vpn::link::tests::nonce_uniqueness_carriers_queues_fallback_reconnect`
drives a real `LinkSender` through multi-carrier + multi-queue (4 cloned
producers on one shared counter) + in-place fallback (counter must continue) +
reconnect (fresh key) and asserts global `(key, counter)` uniqueness. Plus
`crypto::tests::distinct_session_nonces_yield_distinct_keys` (A3).

## B. Reconnect / RAII teardown

- **B1 (LOW, fixed-guard).** `vpn_error_is_retryable` (`vpn.rs:152`) is
  `msg.contains("already in use") || msg.contains("not found")`. The reconnect
  loop is type-based (`is_fatal` downcasts `FatalVpnError`); the substring match
  only decides which *server* `VpnError` becomes fatal. Enumerated every server
  `VpnError` string in `vpn_server.rs`: **none** of the fatal ones contain a
  retryable substring → **currently correct, but fragile**. Pinned every string
  to its class in `vpn::tests::vpn_error_classification_pinned` (fails the
  instant a reword flips a class).
- **B2 (REFUTED).** `run_with_reconnect` is a serial loop; `run_*_once` owns
  `NetConfig` as a local, so its Drop reverts before the next attempt's apply. No
  double-apply path.
- **B3 (MED-LOW, FIXED).** Two concurrent gateway links on one host share
  `/proc/sys/net/ipv4/ip_forward`. The first (observing the original `0`) enabled
  it; a second observing `1` recorded `saved=1`. On teardown the restore wrote
  each link's own `saved` unconditionally (`vpn.rs` Drop) → if the first link
  exited first it restored `0`, **silently killing forwarding under the still-live
  second link**. Single-link reconnect was already safe (serial). **Fix:** a
  per-`(netns,id,role)` `.fwdref` marker + a first-wins
  `/run/bore-vpn-ns<inode>.ipfwd-orig` record; a link restores ip_forward only
  when **no other co-netns** `.fwdref` remains, and the last one out restores the
  true original. `stale_reclaim` is refcount-aware too. **Markers are scoped by
  the `/proc/self/ns/net` inode** because `ip_forward` is per-netns while `/run`
  is shared across netns (the netns harness, containers) — an unscoped refcount
  would wrongly couple independent netns and *break* teardown (caught mid-fix).
  Unit test `other_fwdref_present_detects_concurrent_links` covers same-netns
  coupling AND cross-netns independence; the common single-link `/proc` outcome
  is unchanged.
- **B4 (REFUTED).** `bridge_next_action`: a direct-only death with relay alive →
  `FallBackToRelay` (re-spawn uplinks on the warm relay, keep looping), never
  `LinkDead`. Only both-paths-down breaks out to a reconnect.
- **B5 (LOW, FIXED).** The control socket DOES get `SO_KEEPALIVE` (15 s) via
  `connect_with_timeout`→`tune_tcp` (subagent missed this), so a dead/partitioned
  server is detected in ≲ keepalive window — not 15 min. But the 1:1 ctrl actor
  (`spawn_ctrl_actor`) had no app-level read timeout, while the hub's did; a
  wedged-but-TCP-alive server would go undetected. **Fix:** wrap the recv in a
  60 s `CTRL_HEARTBEAT_TIMEOUT` (server heartbeats every 500 ms ⇒ 120-beat
  margin, no false positives), matching the hub.

## C. Hub multi-client

- **C1/C2 (SAFE).** The router (`run_router_uplink_*`) routes by dst IPv4 to a
  per-peer `Mutex<LinkSender>`; a per-peer `send_batch` error is **logged and the
  packet dropped, loop continues** (`vpn.rs:5823/5889`) — one peer's failure
  never kills the router or restarts it. The direct↔relay swap is in-place under
  the Mutex; the warm relay sender (same Arc counter) is restored on direct death.
- **C3 (OPEN, netns).** Spoke isolation is `iifname bore0 oifname bore0 drop`
  added in gateway mode. The host-only-hub (no `--advertise`) gap — relying on
  host `ip_forward=0` with no nft table — is the documented v1 limitation; left
  as-is, loudly documented.
- **C4/C5/C6 (REFUTED).** `peer_id` is monotonic (never reused in a session),
  addresses freed by overlay; the 6-byte injected header is `read_exact`'d with
  pending buffers bounded; no `async fn` hub helper is left unawaited (the
  not-`async` `spawn_accept_task` pattern is intact).
- **Observation (design limit, not a bug):** the single router task serializes
  per-peer sends under each peer's Mutex; a peer whose relay queue fills applies
  backpressure that head-of-line-blocks the router for other peers. Acceptable
  for v1 (one router by design); noted for a future per-peer egress queue.

## D. Direct ↔ relay path switching

All clean (verified against the already-hardened code):
- **D3** `filter_tunneled_candidates` drops peer candidates inside locally
  tunneled subnets via correct `Ipv4Net::contains`; IPv6 excluded; well-tested.
- **D4** PMTU uses the **typed** `DatagramSend::{Sent,TooLarge}` everywhere
  (`holepunch::send_datagram`, `link::send_batch`); zero `contains("TooLarge")`
  in the tree; `TooLarge` is a per-packet drop, never link death.
- **D5** the server broker re-arms per round (`punched=false` + new deadline) and
  clears stored candidates after each punch, both 1:1 and hub;
  `DIRECT_PUNCH_WAIT`(15 s) < `DIRECT_RETRY_INTERVAL`(30 s) keeps peers aligned.
- **D-yamux** only a comment references `io::split`; the relay uses two
  unidirectional substreams per carrier — the wedge invariant holds.

## E. Relay carriers

- **E1/E3/E4 (SAFE).** Every relay enqueue is `.send().await` (backpressure, no
  `try_send` drop); the receiver decrypts per-frame using the frame's own counter
  (reorder-tolerant, no dedup to drop legit reorders); `carriers=1` writes the
  2-byte header and is byte-identical to the pre-carrier path.
- **E2 (REFUTED — subagent false positive).** Claim: a dead carrier leaves a
  one-way "limping" link. **Wrong:** a dead egress writer makes the next
  `send_batch` to that carrier `Err` → uplink dies → `UplinkDied` → `LinkDead`; a
  dead ingress reader pushes `Err` into the fan-in → `recv_batch` returns `Err` →
  downlink dies → `RelayDownlinkDied` → `LinkDead`. Either way the bridge
  `select!` breaks and `abort_await!` tears down the survivor → clean whole-link
  reconnect. The "a dead carrier kills the link cleanly" invariant holds.

---

## Deliverables produced

- This assessment.
- Fixes: **B3** (ip_forward refcount), **B5** (1:1 ctrl read timeout).
- Regression guards: **A** nonce-uniqueness harness, **A3** per-peer key test,
  **B1** classifier pinning table, **B3** refcount helper unit test.
- Gates: `cargo fmt`, `cargo clippy --features vpn` clean, `cargo test --features
  vpn --lib` = 251 passed / 0 failed.

## CLAUDE.md invariant updates

No invariant was found wrong. Two are now **stronger / clarified**:
- The ip_forward RAII note gains: "concurrent gateway links on one host
  refcount ip_forward via `/run/bore-vpn-*.fwdref` + a first-wins
  `/run/bore-vpn.ipfwd-orig`; a link restores only when the last marker is gone
  (B3 fixed)."
- The heartbeat/server-death note gains: "the 1:1 ctrl actor reads with a 60 s
  `CTRL_HEARTBEAT_TIMEOUT` (parity with the hub) on top of `SO_KEEPALIVE`."

## Still requiring netns (run on both relay + direct)

C-addr-router under join/leave storms, C3 spoke-isolation enforcement, and a
regression pass over B3/B5 teardown — covered by the existing
`scripts/vpn_netns_test.sh`; the data-plane behavior of these fixes is host-rule
/ timing only and does not change the wire protocol.
