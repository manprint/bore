# VPN Hardening & Bug-Hunt Plan

Status: **bug-hunt phase — DO NOT modify `src/` in this phase.** Goal is to surface
real-world failures with severe tests and document them. Fixes happen in a later phase.

Design by Opus. Implementation delegated to Sonnet/Haiku. Each section below is a
self-contained handoff: an implementing agent should not need extra context.

---

## Part 1 — Bugs found by code analysis (as-is)

These were confirmed by reading the current code on branch `vpn`. They are the
*expected findings* the new tests must surface. A test that asserts the correct
behavior here is **expected to FAIL on the current code** — that failure IS the bug
report. Mark such asserts clearly (see "expected-fail" tagging in Part 2).

### BUG-1 — direct→relay fallback is NOT seamless (CRITICAL)

- relay→direct upgrade *is* seamless: `direct_upgrade_task` feeds new Direct halves to
  the bridge, which stops the relay pumps and respawns on Direct (`vpn.rs` ~3299-3304).
  TUN stays up. No reconnect. ✓
- On that switch the **relay halves are dropped** (`cur = Some(pair)` overwrites and
  drops the old relay sender/recver, `vpn.rs` ~3301-3302). Nothing warm is retained.
- When the Direct QUIC path dies at runtime (idle timeout ~10 s, network drop), the
  downlink pump's `recv_batch` returns `Err` (`vpn.rs` ~2948-2952, ~3436); the bridge
  `select_all` resolves with that error; after a 5 s `UPGRADE_GRACE` window with no
  pending upgrade it does `break 'outer outcome` (~3278-3297) — **the whole link is torn
  down** and the reconnect loop (`vpn.rs` ~153-179, `Backoff` 1→32 s) re-establishes,
  starting again on relay.
- **Consequence:** every direct→relay transition is a multi-second outage (TUN
  destroyed/recreated, ping gap = grace + backoff + re-pair). The user-visible "link
  stays up, no notice" property holds ONLY for relay→direct, never direct→relay.
- The requested cycle (relay→direct→relay→direct, transparent) cannot be transparent on
  the down-legs **by construction** — the relay path is gone the moment we go direct.

### BUG-2 — SIGKILL poisons `ip_forward` permanently (HIGH)

- `stale_reclaim` (`vpn.rs` ~2064-2071) deletes only the nft table and the TUN device.
  It never restores `/proc/sys/net/ipv4/ip_forward`.
- In gateway mode the first run reads ip_forward (0), saves 0, sets 1. On SIGKILL the
  Drop never runs → ip_forward stays 1. The **next** run reads the current value (now 1)
  and saves *1* as the "original" (`vpn.rs` ~2214-2237). On its clean exit it "restores"
  to 1 (`vpn.rs` ~2345-2377). ip_forward is now stuck at 1 forever, no manual fix done.

### BUG-3 — SIGKILL + iptables fallback leaks NAT/MSS rules forever (HIGH)

- On hosts without `nft`, `NetConfig::apply` installs iptables masquerade
  (nat/POSTROUTING) + MSS-clamp (mangle/FORWARD) rules, comment `bore_vpn_<id>`
  (`vpn.rs` ~2282-2296), with revert commands queued for Drop.
- `stale_reclaim` only runs `nft delete table ...` — it never deletes the iptables
  rules. After a SIGKILL on an iptables-only host the rules persist permanently; the
  next run's stale_reclaim cannot remove them. (The netns harness uses nft, so it has
  never exercised this path.)

### BUG-4 — MTU asymmetry silently drops packets (MEDIUM)

- `--mtu` is purely local; it is not in `HelloVpn`/`ConnectVpn`/`VpnReady`
  (`shared.rs`) and is never negotiated. Two peers with different `--mtu` run mismatched
  with no warning. The smaller-MTU side silently drops oversized packets (TooLarge →
  per-packet drop). Symptom: small pings pass, large payloads vanish — hard to diagnose.

### Coverage gaps (test debt, not bugs)

- G1: routes never asserted *gone* after gateway-mode teardown (only nft + ip_forward).
- G2: `--nat-udp-preferred-port` (pin outbound UDP port) untested — user's critical
  requirement (egress-filtered hosts). Also `--nat-udp-release-timeout`.
- G3: site↔site (both `--advertise`), host↔site (only connector advertises), and
  initiator role-swap (connect side is the advertiser) untested.
- G4: peer-dies-and-returns reconnect — only *server* kill is tested (Test 10), not a
  listener or connector dying and re-pairing.
- G5: broker borderline cases (empty candidates, IPv6 candidate, both offer same addr,
  timeout boundary, >2 re-offers, only-connector-repeatedly).

---

## Part 2 — Deliverable D1: `scripts/vpn_netns_test_hard.sh` (the truth test)

New file. Standalone, run on demand: `sudo scripts/vpn_netns_test_hard.sh`. Mirror the
conventions of `scripts/vpn_netns_test.sh`:
- Same `set -euo pipefail`, `BORE=` resolution **and the anti-stale-binary guard**
  (copy lines 16-32 verbatim — refuse to run a build older than `src/`/`Cargo.toml`).
- Same `pass`/`fail`/`die`/`cleanup`/`wait_for_log` helpers; `trap cleanup EXIT INT TERM`.
- Same 3-namespace topology (ns0 server, ns1/ns2 peers, fake LAN 192.168.50.0/24 on ns1
  lo; add a second fake LAN 192.168.60.0/24 on ns2 lo for site↔site).
- Print `PASS=$PASS FAIL=$FAIL` at the end and `exit $((FAIL>0))`.
- Each `bore` invocation logs to its own file under `$BORE_LOG.*`.

### Shared helper to add (continuous-connectivity probe)

Implement a bash function `probe_loss <ns> <target_ip> <duration_s> <label>` that:
- runs `ip netns exec <ns> ping -i 0.2 -W 1 -c $((duration*5)) <target>` capturing output,
- parses the `X% packet loss` figure,
- returns the integer loss percent on stdout (echo), and also the max observed
  inter-packet gap if parseable (`rtt .../max` is not gap; instead count missing seq).
  Simpler acceptable form: echo the loss percent only.

And `mtu_of <ns> <dev>` → echoes the device MTU (`ip -o link show`).

A background-ping variant for transition measurement: `bgping_start <ns> <target>`
launches `ping -i 0.2 -D <target>` to a temp file, returns its PID; `bgping_stop <pid>
<file>` kills it and echoes the loss percent over the whole window. Use this to measure
loss *across* a path transition (start before, stop after).

### Test matrix for D1

Run the **full block below for each MODE**, and within each mode run it once with ns1 as
listener / ns2 as connector, and once **role-swapped** (ns2 listener / ns1 connector) to
prove both `listen` and `connect` sides behave identically. Use distinct `--id` per
sub-run to avoid registry collisions.

MODES:
1. **host↔host** — neither advertises (Pool overlay only).
2. **host↔site** — only the *connector* advertises a LAN (G3: connector-as-advertiser).
3. **site↔site** — both advertise (ns1: 192.168.50.0/24, ns2: 192.168.60.0/24);
   assert each side can reach the *other's* LAN host.

For every mode/role combination assert pairing, bidirectional ping 0% loss, and a large
payload ping (`-s 1300`) 0% loss.

### D1 core scenario — relay↔direct cycling (the headline test)

Setup: ns1 listen + ns2 connect, both `--stun-server <server>:7835` (so direct is
possible), `--auto-reconnect`. UDP between peers is gated by an nft drop rule on ns0's
forward hook that the test toggles (`block_udp` / `unblock_udp` functions; reuse the
existing harness's table `bore_test_block`).

Drive this sequence and assert at each step. Use a continuous background ping
(`bgping_start` before each transition, `bgping_stop` after) AND check logs:

1. Start with UDP **blocked** → link pairs on **relay**; assert ping 0% loss over relay,
   assert log shows `staying on relay, will retry`.
2. **Unblock** UDP → background retry upgrades. Assert within 45 s both logs show
   `upgraded to direct`, assert **no** `reconnecting` between pair and upgrade, assert
   the relay→direct transition ping window loss is **0%** (seamless up-leg). [PASS today]
3. **Block** UDP again for a parametrized duration `D` (run the cycle for
   D ∈ {2, 8, 16} seconds — "vari timeout fino a 16 s"):
   - For **D=2 s** (< QUIC keepalive*… < idle 10 s): direct should **survive** the brief
     drop. Assert NO `reconnecting`, NO transition, ping recovers with only minor loss.
     This proves QUIC keepalive resilience.
   - For **D=16 s** (> idle 10 s): direct dies. Assert the link recovers to a working
     state (ping eventually 0%). **Measure and record the outage gap and whether a
     `reconnecting` log appears.**
     - **EXPECTED-FAIL assert (BUG-1):** assert the direct→relay transition is seamless,
       i.e. loss across the block→recover window is 0% and **no** `reconnecting` log.
       Tag this assert `xfail_bug1` (see tagging). On current code it WILL fail (reconnect
       blip). The test must still continue and the suite must distinguish xfail from a
       real regression (see "xfail handling").
4. After recovery, **unblock** and assert it climbs back to direct (`upgraded to direct`
   again) → proving the cycle relay→direct→relay→direct actually repeats.
5. Repeat steps 3-4 at least **3 full cycles** to catch state-machine drift (broker
   re-arm leaks, stale candidates, carrier exhaustion). Assert the link is ping-healthy
   at the end of every cycle.

Throughput continuity (optional, behind `--skip-iperf`): run a long `iperf3 -u` across
one up-leg transition and assert it does not error out.

### D1 fault-injection & cleanup scenarios

For each, assert recovery AND full host cleanup on **both** ns1 and ns2.

- **F1 — connector dies and returns.** Pair (relay-only, auto-reconnect). `kill -9` the
  connector. Assert listener logs `vpn link lost; reconnecting`. Restart the connector
  (same id). Assert re-pair (≥2 `vpn link paired` in listener) and ping 0%.
- **F2 — listener dies and returns.** Symmetric to F1, kill the listener instead.
- **F3 — server dies and returns** (already in base Test 10, include a relay+direct
  variant here): kill server, both reconnect, restart server, re-pair, ping 0%.
- **F4 — SIGKILL both peers, gateway mode, assert FULL cleanup.** ns1 advertises a LAN
  (gateway mode → ip_forward + nft). Pair. `kill -9` BOTH peers. Then **WITHOUT** a
  second run, assert on the killed namespace:
  - `ip link show bore0` absent (note: SIGKILL leaves it; stale_reclaim only runs on the
    *next* start — so assert it is **present** here, then start a fresh run with the same
    id and assert it becomes count=1, mirroring base Test 14). Document the SIGKILL→next-run
    reclaim contract in the assert messages.
  - **EXPECTED-FAIL assert (BUG-2):** capture ip_forward before the very first run; after
    SIGKILL + a fresh clean run + clean exit, assert ip_forward returns to the original
    (0). Tag `xfail_bug2`. On current code it stays 1 (poisoned).
  - Routes: after a *clean* teardown (not SIGKILL) of gateway mode, assert the advertised
    route is **gone** from the connector's table (closes G1).
- **F5 — clean teardown leaves nothing.** After a normal SIGTERM of a gateway-mode pair,
  assert on both sides: no `bore0`, no `bore_vpn_<id>` nft table, no route to the
  advertised CIDR, ip_forward back to pre-run value. (This is the positive cleanup proof.)

### D1 — UDP port-pinning flag tests (G2, user-critical)

These verify a peer can be forced onto a fixed outbound UDP port (egress-filtered hosts).

- **P1 — both sides pin the same `--nat-udp-preferred-port <P>`.** Use a port the netns
  allows (e.g. 51820). Assert the link upgrades to direct AND that the chosen UDP socket
  is actually bound to P: `ip netns exec nsX ss -u -a -n | grep ':51820'` while direct is
  up. Assert ping 0% over direct.
- **P2 — egress allow-list emulation.** On ns0 forward hook, DROP all forwarded UDP
  EXCEPT udp dport == P (nft rule). With both peers pinned to P, assert direct still
  comes up (proves pinning works through a port-restricted middlebox). Without pinning
  (control sub-case) assert it falls back to relay (random port blocked).
- **P3 — asymmetric pin (listener pins P1, connector pins P2).** Assert the link still
  establishes (direct or relay) and stays ping-healthy — pinning is per-side, must not
  require agreement. Record whether direct succeeds.
- **P4 — `--nat-udp-release-timeout`** smoke: set a small value on one side, assert the
  link is healthy and no crash/log error over ~30 s.

### D1 — flag-matrix cross-product (most critical combos)

Test these flag combinations set from BOTH sides (assert pair + ping 0%, and the
documented interaction):
- `--carriers` mismatch (listen 4 / connect 2) over relay → both must use min=2; assert
  pairing + ping; grep server/admin or logs for negotiated carrier count if surfaced.
- `--mtu` mismatch (listen 1400 / connect 1200): **EXPECTED-FAIL (BUG-4)** — assert a
  large-payload ping (`-s 1350`) succeeds both directions; tag `xfail_bug4` (smaller side
  drops). Also assert `mtu_of` shows the two different MTUs (proving no negotiation).
- `--relay-only` on ONE side only (listen relay-only, connect not): assert the link stays
  on relay (one side refusing direct must keep both on relay) and never logs `upgraded to
  direct`. Record actual behavior.
- `--tun-name` custom on each side (different names) + `--tun-queues` mismatch
  (4 vs 1): assert pair + ping 0%.

### xfail handling (so the suite is the regression gate)

Provide two counters in addition to PASS/FAIL: `XFAIL` (expected failure observed — the
known bug reproduced) and `XPASS` (an xfail assert unexpectedly passed — the bug may be
fixed; surface loudly). Helper:

```
xassert <tag> <condition-cmd...>   # runs the condition; if it FAILS -> XFAIL (known bug
                                   # <tag> reproduced, print as XFAIL not FAIL); if it
                                   # PASSES -> XPASS (print loudly: "XPASS <tag>: bug may
                                   # be fixed, promote to hard assert").
```

Final line: `PASS=$PASS FAIL=$FAIL XFAIL=$XFAIL XPASS=$XPASS`. The script exits non-zero
only if `FAIL>0` (genuine regressions). xfail/xpass never fail the build but are reported.
This lets the hard suite run alongside the rest and guarantee **zero regressions on
channel switch** while still documenting the four known bugs as XFAIL until fixed.

---

## Part 3 — Deliverable D2: additions to `scripts/vpn_netns_test.sh`

Keep these in the *base* suite (cheap, deterministic, always-run). Add as new numbered
tests after the current last test:

- **T-new-1 (G1):** gateway-mode clean-teardown route check. After a `--advertise` pair
  is torn down with SIGTERM, assert the advertised route is absent in the connector ns
  AND the nft table absent AND ip_forward restored. (Positive cleanup proof; pure addition.)
- **T-new-2 (G4):** connector dies and returns (auto-reconnect, relay-only). kill -9 the
  connector, assert listener logs reconnecting, restart connector, assert re-pair + ping.
- **T-new-3 (G4):** listener dies and returns (symmetric).
- **T-new-4 (host↔site, G3):** only the *connector* advertises a LAN; assert listener can
  reach the connector's LAN host over the tunnel + clean teardown.

Do NOT add the heavy cycling/xfail logic here — that lives in D1.

---

## Part 4 — Deliverable D3: broker unit tests in `tests/vpn_server_test.rs`

Add these test fns (mirror the existing `vpn_broker_*` helpers `offer()`,
`recv_until_punch()`, `recv_until_unavailable()`, default punch_timeout). All are pure
in-process broker tests, no netns:

- `vpn_broker_empty_candidate_offer_times_out` — connector offers `candidates: vec![]`;
  listener offers real candidates; assert broker does NOT punch on the empty offer and
  eventually sends UdpUnavailable / waits for a non-empty re-offer. (Document actual.)
- `vpn_broker_ipv6_candidates_relayed` — offers carry IPv6 `SocketAddr`s; assert broker
  relays them unchanged in the UdpPunch to the peer (no panic, no IPv4 assumption).
- `vpn_broker_both_offer_same_address` — listener and connector advertise the *same*
  `SocketAddr`; assert broker still punches both sides with the peer's candidate set (no
  dedup crash). Document whether it filters self.
- `vpn_broker_timeout_boundary` — set punch_timeout small (e.g. 200 ms); connector offers,
  listener silent; assert UdpUnavailable fires once, after the deadline, not before.
- `vpn_broker_triple_reoffer` — connector offers 3 times in succession with changing
  candidates; assert the broker always punches the FRESHEST candidate set and re-arms each
  round (extends the existing 2-round `rebrokers_on_repeated_offer`).
- `vpn_broker_connector_only_no_listener_offer` — connector offers repeatedly, listener
  registers but never offers candidates; assert UdpUnavailable (not an infinite wait, not
  a punch with empty peer set).

If any of these reveal a *panic* or clearly-wrong behavior, that is a NEW bug — record it
in Part 1, do NOT fix in this phase.

---

## Part 5 — Verification gates (run by the implementing agents, NOT under sudo)

After D3 (and any test-only helper additions), the implementing agent must run and report:
- `cargo fmt --all -- --check`
- `cargo clippy --features vpn --tests -- -D warnings` and `cargo clippy --tests -- -D
  warnings` (default features) and `cargo clippy --no-default-features --tests -- -D
  warnings`
- `cargo test --features vpn` (full) and `cargo test` (default) — all green.
- `bash -n scripts/vpn_netns_test_hard.sh` and `bash -n scripts/vpn_netns_test.sh`
  (syntax check; the netns runs themselves require sudo and are run by the user).
- `shellcheck` the two scripts if available (warnings acceptable, no errors).

The netns suites (D1, D2) are executed by the user via sudo against a freshly built
`--release --features vpn` binary. The agent must NOT run sudo and must NOT build under
sudo.

---

## Delegation summary

- D1 (`vpn_netns_test_hard.sh`): substantial bash → **Sonnet**.
- D2 (base-suite additions): moderate bash → **Sonnet** (can share the D1 agent).
- D3 (broker unit tests): Rust, follows existing patterns → **Sonnet**; mechanical
  scaffolding → **Haiku** acceptable.
- Verification (Part 5): **Haiku/Sonnet** runs cargo + bash -n, reports output.
- Opus reviews the produced files against this spec and confirms xfail tagging + that the
  four bugs are reproduced as XFAIL.
