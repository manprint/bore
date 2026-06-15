# VPN Bug-Hunt TODO — direct / relay / hub / crypto-nonce / reconnect

> Follow-up to the NAT hunt (`VPN_NAT_ASSESSMENT.md`). That pass covered NAT + the gateway
> data-plane only. This TODO scopes a **bug-hunter pass over the rest of the VPN**. Planned
> 2026-06-15, to execute next session.
>
> **Discipline (same as the NAT pass — non-negotiable):** every finding is a *hypothesis* until
> reproduced empirically. A render-check / "it compiles + unit tests pass" is NOT a behavior-check.
> For each confirmed bug: (1) reproduce in netns or a targeted harness, (2) fix, (3) add a
> **differential** e2e/test that FAILS on the bug and PASSES on the fix. Verify any subagent's
> kernel-rule / protocol output independently (the NAT pass caught a subagent dropping `-o <tun>`
> from an SNAT rule).

## Method & tooling (reuse)
- **netns harness:** `scripts/vpn_netns_test.sh` (server ns0 + peers ns1..ns4, direct + `--relay-only`).
  Rebuild `cargo build --release --features vpn` (as user, not root) before `sudo -n <abs path>`.
- **Differential e2e pattern:** prove the bug exists (assert the broken behavior) AND the fix works,
  in one test — guards against both regression and false-OK. (See T-NAT-MASQ / T-NAT3.)
- **Identity probe:** socat `SYSTEM:'echo HIT-$SOCAT_SOCKADDR'` + client read = ground truth on the
  exact endpoint actually reached (not just "something answered").
- **Force a path:** `--relay-only` forces relay; `BORE_VPN_FORCE_IPTABLES=1` forces the iptables
  data plane. Add similar hooks if a path is otherwise unreachable in netns.
- **Agent tiers (per CLAUDE.md):** Haiku = mechanical location/grep; Sonnet = static review + harness
  authoring; **Opus = reasoning-heavy hunt, crypto, race analysis, verifying subagent output.**
  Crypto + reconnect race work stays on Opus.

## Priority order (do in this sequence)
1. **Crypto / AEAD nonce** — security-critical; a nonce reuse silently breaks confidentiality/integrity.
2. **Reconnect / RAII teardown** — we already found host-config leaks (iptables); audit the rest.
3. **Hub multi-client** — newest code, most invariants, most surface.
4. **Direct ↔ relay path switching** — race-prone (bridge restart, in-place fallback, PMTU).
5. **Relay carriers** — reorder / partial-failure / framing.

---

## A. Crypto / AEAD nonce  (HIGHEST — security)
Invariants (CLAUDE.md): ONE shared `Arc<AtomicU64>` per egress key; carriers + multi-queue clones all
`fetch_add` (I-5/DEC-6) — never per-producer counters, never two seals with same `(key, counter)`.
Hub: each peer derives keys from its OWN `session_nonce` (passed RAW, `UDP_NONCE_LEN`, never
padded/resized) with its OWN counter (I-MC4). Relay carriers round-robin per-datagram (DEC-7,
reorder OK); any replay window (B1) must size ≥ `2 × (carriers × RELAY_QUEUE)` (DEC-10).

Hypotheses to test:
- **A1 — nonce reuse across reconnect / path switch.** DEC-2 says the counter is *preserved* across
  the in-place direct→relay fallback. **Verify it is actually preserved** (not reset to 0) — a reset
  + same derived key = catastrophic `(key, counter)` reuse. Check full reconnect too: new
  `session_nonce` ⇒ new keys ⇒ counter reset is safe; same nonce + reset is not. **TO VERIFY:** where
  the counter lives (search the `link` mod / `LinkSender` seal path; not found under a quick grep —
  locate `AtomicU64` / the seal call) and its lifecycle across `bridge` restart.
- **A2 — replay protection exists at all?** CLAUDE.md phrases the replay window as "*any future*
  replay window (B1)" ⇒ likely **NOT implemented**. If the receiver has no replay/dedup, a relay MITM
  can replay datagrams. Decide: is that in-scope for the threat model? If yes → finding. If a window
  exists, verify it sizes for carrier reorder (DEC-10).
- **A3 — per-peer key isolation in hub.** Two connectors must never share a counter or derive equal
  keys. Verify each peer's `session_nonce` is unique and passed RAW (no pad/truncate) into HKDF
  (`derive_keys_*` at `src/vpn.rs:1625`); a length normalization would silently align keys.
- **A4 — counter overflow / wrap.** `AtomicU64` wrap is astronomically far, but confirm no narrower
  (u32) counter anywhere in the carrier/queue fan-out.

Verification: mostly **code audit + targeted unit/integration tests** (crypto reuse is hard to netns).
Add a test harness that intercepts every sealed `(key, nonce)` and asserts global uniqueness across:
multi-carrier fan-out, multi-queue, a direct→relay fallback, and a full reconnect. This is the single
highest-value new test to build.

## B. Reconnect / path-churn / RAII teardown
Invariants: `run_with_reconnect` (`src/vpn.rs:413`) stops on `FatalVpnError` (non-retryable),
loops on retryable; classifier `vpn_error_is_retryable` (`src/vpn.rs:152`). Duplicate-id at first
attempt is the deliberate retryable exception. `NetConfig` RAII reverts routes/nft/ip_forward on
SIGINT/SIGTERM/panic; SIGKILL via `stale_reclaim`. Full reconnect only if BOTH paths down.

Hypotheses:
- **B1 — error misclassification.** `vpn_error_is_retryable` matches on a *message string*
  (`fn(msg: &str)`). String-matching is the exact bug class that hid the NAT `prefix` bug. Enumerate
  every error that should be fatal vs retryable; a wording change anywhere flips the class silently.
  Add a unit test pinning each known error → expected class.
- **B2 — teardown leak on reconnect (not just SIGKILL).** On a retryable reconnect, is the old
  `NetConfig` dropped (routes/nft/ip_forward reverted) BEFORE the new one applies? If the new apply
  runs first, rules double-apply or the revert clobbers the fresh ones. Reproduce: force a mid-link
  reconnect in netns, assert (a) no duplicate routes/nft rules, (b) ip_forward correct, (c) TUN sane.
- **B3 — ip_forward restore race.** Two overlapping links (or reconnect) save/restore the same
  `/proc` value; last-writer-wins could restore 1 when it should be 0 or vice-versa. Check the
  per-id state file logic.
- **B4 — premature full reconnect.** "Full reconnect only if BOTH paths down" — verify a single-path
  death (direct only) does NOT trigger a TUN-destroying reconnect (should fall back in place, DEC-2).
- **B5 — heartbeat / server-death detection.** Missed heartbeat → false death (needless reconnect) or
  zombie link (death not detected). The ctrl actor is the stream's single owner — verify no second
  reader. Reproduce: kill server mid-link, assert client detects within the heartbeat window.

Verification: netns — `--relay-only` and direct, inject drops/kills, assert route/nft/ip_forward state
via `ip route` / `nft list` / `iptables -S` before+after (extend the T-NAT-IPT teardown-assert style).

## C. Hub multi-client  (`mod hub` in vpn.rs + vpn_server.rs)
Invariants: `--max-clients 1` is byte-for-byte the legacy 1:1 path (I-MC1) — NEVER edit 1:1 to add
hub behavior. Shared router uplink routes by dst IPv4 → per-peer swappable `Mutex<LinkSender>`; one
downlink per peer writes the shared TUN (packet-atomic). Router NEVER restarts on a path switch
(per-peer sender swapped IN PLACE, relay kept WARM). Server→hub framing `[STREAM_READY, peer_id u32
BE]` then verbatim `[tag, idx?, payload]`; connector→server bytes UNCHANGED (I-MC2). Spoke isolation
(D2) `iifname bore0 oifname bore0 drop`. **Known v1 gap:** host-only hub (no `--advertise`) relies on
host `ip_forward=0` for isolation — no nft table created.

Hypotheses:
- **C1 — per-peer sender swap race.** The router holds `Mutex<LinkSender>` per peer and swaps on
  direct upgrade. Audit: is a datagram ever sent on a half-swapped/closed sender? Does the swap drop
  in-flight packets or, worse, send on the OLD relay sender after the direct one is live (reorder/dup)?
- **C2 — router restart on switch.** Invariant says the router NEVER restarts. Verify a per-peer path
  switch does not stall/restart the shared router (which would hiccup ALL peers). Reproduce: 3 spokes,
  upgrade one to direct, assert the other two see zero packet loss during the swap.
- **C3 — spoke isolation holes.** Test `iifname bore0 oifname bore0 drop` actually blocks spoke↔spoke.
  **Confirm the known host-only gap** (no `--advertise`, host forwards by default) and decide fix
  (add an explicit drop even without nft table, or document loudly).
- **C4 — peer_id / addr-pool reuse & leak.** Join/leave/rejoin churn: are `peer_id` (monotonic) and
  host addresses reused while still in flight? leaked on abrupt leave? Reproduce join/leave storms.
- **C5 — framing parse.** Fuzz/boundary the `[STREAM_READY, peer_id u32 BE]` prefix injection
  (`vpn_relay_hub`): truncated prefix, peer_id for a departed peer, tag/idx boundary.
- **C6 — unawaited-future trap (regression class).** CLAUDE.md notes a past bug: a hub helper that
  only `tokio::spawn`s must not be `async fn` unless the caller awaits it (an unawaited future never
  runs — once silently killed the relay accept path). Grep for `async fn` helpers whose call sites
  don't `.await`.

Verification: extend the existing T-HUB*/T-HUBD*/T-SCEN-* netns tests; run on BOTH relay and direct.

## D. Direct ↔ relay path switching (QUIC / holepunch / PMTU)
Invariants: links start on relay; background task upgrades to direct (DEC-1: stop pumps, switch uplink
set, respawn on Direct). Relay stays WARM; direct death → fall back to warm relay IN PLACE (DEC-2:
TUN + nonce counter preserved). `filter_tunneled_candidates` drops peer candidates inside locally-
tunneled subnets (else QUIC loops through relay, succeeds, then dies at idle timeout). PMTU: `TooLarge`
= per-packet DROP not link death; `DirectConn::send_datagram` returns typed `DatagramSend::{Sent,
TooLarge}` (NOT a stringly error — a past bug substring-matched `"TooLarge"` which never fired);
`pmtu_shrink_now` immediate, `pmtu_decision` grows on 3 stable. `direct_upgrade_task` retries on a 30s
grid (`should_retry_direct`); server re-arms per round + clears listener candidates after each punch.

Hypotheses:
- **D1 — switch-time packet loss / reorder / yamux-split.** The bridge restart (stop pumps → switch →
  respawn) is the prime race. Re-confirm the never-split-a-yamux-Stream rule holds on every new path.
  Reproduce: sustained traffic across an up-switch and a down-fallback, assert zero corruption (BLAKE3
  a stream through it), bounded loss, no wedge.
- **D2 — fallback NOT seamless.** DEC-2 claims in-place fallback preserves TUN + nonce. Kill the direct
  path under load; assert the link survives on relay WITHOUT a full reconnect and WITHOUT nonce reset
  (ties to A1). This is the one BUG-1 the earlier hardening flagged as architectural — re-examine.
- **D3 — `filter_tunneled_candidates` gaps.** Construct a peer candidate inside a locally-tunneled
  subnet; assert it's dropped (no fake-direct that dies at idle timeout). Try a more-specific connected
  route that the conservative filter over-drops — confirm it still falls back to relay (acceptable).
- **D4 — PMTU substring-match regression.** Confirm the typed `DatagramSend::TooLarge` path is used
  everywhere (no `.to_string().contains("TooLarge")`). Reproduce: TUN MTU ahead of QUIC path MTU right
  after a switch (full-size packet) → assert per-packet drop, link survives, PMTU monitor narrows.
- **D5 — upgrade retry desync.** Server must re-arm per round (reset deadline + clear `punched`) AND
  clear the listener's stored candidates after each punch (else round N+1 re-punches a dead socket →
  connector times out against a closed port). Both peers anchored on the same 30s grid. Reproduce on a
  UDP-hostile-then-open network; assert in-place upgrade after N rounds, no reconnect.

Verification: netns with and without `--relay-only`; a UDP-block toggle to force relay→direct upgrade.

## E. Relay carriers
Invariants: relay queue applies backpressure (await on full), NEVER silent drops. `--carriers N` round-
robin per-datagram (reorder OK, DEC-7). A dead carrier kills the whole link cleanly (reconnect re-
establishes), never silent degradation. `carriers<=1` is byte-identical to the single path.

Hypotheses:
- **E1 — silent drop on full queue.** Audit every relay enqueue: is it `await`-on-full (backpressure)
  or a `try_send`/bounded-channel that drops? A drop = silent corruption for the VPN.
- **E2 — partial carrier failure → silent degradation.** Kill ONE of N carriers mid-link; assert the
  link fails cleanly + reconnects (not limps at reduced throughput with losses).
- **E3 — receiver reorder tolerance.** Per-datagram round-robin reorders; confirm the receive side
  (and any future replay window, A2) tolerates it without dropping legitimate datagrams.
- **E4 — `carriers=1` byte-identity.** Pin a test that the single-carrier path is unchanged.

Verification: netns with `--carriers N`, kill-a-carrier injection, throughput + integrity assert.

---

## Deliverables for the hunt session
- `VPN_BUGHUNT_ASSESSMENT.md` — findings table (sev | file:line | problem | fix | verify), one section
  per area, each finding empirically reproduced.
- Fixes for confirmed bugs + a **differential** regression test per fix (netns or unit).
- The crypto nonce-uniqueness test harness (A) — highest-value new artifact.
- Update CLAUDE.md invariants if any are found wrong/incomplete.

## Open scoping question for tomorrow
- Threat model for **A2 (replay)**: is relay-path replay in scope? Drives whether "no replay window"
  is a finding or accepted. Decide before hunting A.
