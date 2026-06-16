# VPN direct-path bandwidth assessment (2026-06-16)

Investigation into VPN direct-path under-utilisation (real test: ~100 Mbit on a
250 Mbit uplink ≈ 40%). Driven by a netns bench with `tc netem` WAN emulation
(`scripts/vpn_bench.sh`) plus a new read-only DEBUG diagnostics task
(`direct_diag` in `vpn.rs`). A clean netns (RTT ~0, 0 loss) measures multi-Gbps
and hides every effect; the WAN emulation reproduces the real behaviour.

## Method

- Topology: ns1 (listener) — ns0 (server/router) — ns2 (connector). The direct
  path ns1↔ns2 transits ns0, so `netem` on ns0's two egress legs shapes both
  directions.
- `direct_diag`: per-carrier `quinn::ConnectionStats` deltas every 5 s — cwnd,
  rtt, lost_pct, `current_mtu`, black_holes, PLPMTUD probes, datagrams framed
  (`frame_tx.datagram`) vs app-submitted (`tx_pkts`) → `buffer_drop_est`.

## Results

### Run 1 — WAN 20 ms RTT, 250 Mbit, MTU 1400, 0 % loss
| Config | TCP-1 | TCP-P8 | UDP loss | diag |
|---|---|---|---|---|
| relay-1c | 212 | 217 | 2.4 % | — |
| direct-1c | 210 | 218 | 0 % | buffer_drop 1511→0, mtu_chg 4, rtt 116 ms |
| direct-4c | 111 | **2** | **25 %** | black_holes 1 |
| direct-4c4q | 214 | **19** | 6.6 % | — |
| direct-nat (4c) | 94 | **0** | 6.4 % | — |
- direct-1c file 1 GiB: 39 s @ 220 Mbps, **sha256 MATCH**.

### Run 3 — WAN 40 ms RTT, 250 Mbit, MTU 1300, 0.2 % loss
| Config | TCP-1 | TCP-P8 | UDP loss | diag |
|---|---|---|---|---|
| relay-1c | 55 | 16 | 86 % | — |
| relay-4c | 48 | 19 | 87 % | — |
| **direct-1c** | **27** | **168** | 0.72 % | buffer_drop 1496, mtu_chg 4, quic_mtu 1262 |
| direct-2c | 8 | 94 | **28.8 %** | lost_pct 30 % |
| direct-4c | 12 | 73 | **43.9 %** | black_holes 1 |
| direct-4c4q | 17 | **6** | 75 % | black_holes 4 |
| direct-nat (4c) | 34 | 66 | 23.5 % | buffer_drop 275 |
- direct-1c fixed-volume 2 GiB: **did NOT complete in 190 s** (single-flow crawl).

(MTU=1200 first attempt: every direct config SETUP FAILED — QUIC INITIAL needs
~1228 B IP; path MTU < 1280 black-holes the handshake. Harness now clamps ≥1280.)

## Findings (ranked)

### F1 — The 40 % is the SINGLE inner TCP flow (not a bore bug)
direct-1c under 0.2 % loss: **27 Mbit single-stream vs 168 Mbit with `-P 8`**. A
lone TCP flow over a tunnel that adds any loss is Mathis-bound
(`thr ≈ MSS/(RTT·√p)`); parallel flows reach ~67 % of the cap. The real test
almost certainly measured one flow (iperf3 default / single file copy).
→ Confirm with `iperf3 -P 8`; bulk transfer should use a multi-stream tool. Bore
  can only help by NOT adding loss (see F3) and not churning MTU (F4).

### F2 — `--carriers N>1` on the DIRECT path is BROKEN (makes it worse)
Per-datagram round-robin across N QUIC carriers (DEC-7 "reorder OK") shreds the
inner TCP via reordering: UDP loss explodes (run1 4c 25 %, run3 2c 28.8 %/lost 30 %,
4c 43.9 %) and TCP collapses (run1 4c TCP-P8 = 2). Reproduced in BOTH runs.
"reorder OK" is FALSE for a TCP-carrying VPN. Relay carriers, by contrast, HELP
(independent reliable streams). → Do NOT use `--carriers` on direct as shipped.
Fix = flow-pinned steering (hash inner 5-tuple → carrier, in-order per flow) or
gate carriers to the relay path only.

### F3 — QUIC send-buffer datagram drops (congestion-as-loss)
`buffer_drop_est` up to 1496/5 s on direct-1c: when the 8 MB datagram send buffer
fills, quinn silently drops the OLDEST datagram (`holepunch.rs:1131`). The inner
TCP sees this as loss and backs off — bore manufacturing the very loss that F1 is
bound by. Also bufferbloats RTT (116 ms vs a 20 ms link).
Fix = backpressure on the uplink (`send_datagram_wait`) instead of drop-oldest, so
the TUN read paces to the real drain rate. Caveat: per-peer only (hub HOL risk).

### F4 — MTU churn + PMTU black-hole flap
4–5 `tun MTU adjusted` per link; `quic_mtu` oscillates (1262↔1200) and
`black_holes` 1–4 under loss. quinn starts at `initial_mtu` 1200 (no MTU config
in `transport_config`) → TUN shrinks then regrows every direct switch. Wastes
throughput at every flap.
Fix = set quinn `initial_mtu` to the TUN MTU; make `--mtu` pinnable (observe-only
PMTU monitor that warns but never resizes). See VPN_HARDENING notes.

### F5 — NAT adds no data-plane cost
direct-nat tracks direct-4c (both carrier-broken); NAT itself is free. Confirms
I-NAT8 (IP-opaque data plane).

## Fixes applied (2026-06-16)

- **F2 — flow-pinned direct carriers** (`flow_carrier` in `vpn.rs`): the direct
  `send_batch` no longer round-robins datagrams per packet; it hashes the inner
  IPv4 5-tuple so one inner connection rides one carrier (in order). `n==1` → 0,
  byte-identical. Relay keeps per-datagram RR (replay window sized for it).
- **F3 — backpressure on the 1:1 uplink** (`send_batch_wait` / `DirectConn::
  send_datagram_wait`): the dedicated 1:1 uplink awaits QUIC send-buffer room
  instead of letting quinn drop the oldest datagram. Hub router keeps non-blocking
  `send_batch` (no cross-peer HOL).
- **F4 — `--pin-mtu`**: PMTU monitor observe-only (warn on shortfall, never resize).
- **`scripts/vpn_bench.sh`** rewritten: `tc netem` WAN emulation + carrier sweep +
  2 GiB transfer + integrity + diag capture. MTU clamped ≥1280 (QUIC handshake floor).

### Before → after (WAN 40 ms / 250 Mbit / MTU 1300 / 0.2 % loss)
| Config | TCP-P8 before | TCP-P8 after | UDP loss before | UDP loss after |
|---|---|---|---|---|
| direct-1c | 168 | 187 | 0.72 % | 0.77 % |
| direct-2c | 94 | (≈1c) | 28.8 % | — |
| direct-4c | **73** | **166** | **43.9 %** | **4.31 %** |
- Flow-pinning removed the multi-carrier reordering collapse: 4 carriers now ≈ 1
  carrier instead of halving throughput and exploding UDP loss.
- Single-flow `TCP-1` is unchanged (~18–27 Mbit): F1 is TCP-over-loss physics, not
  fixable inside the tunnel — parallelise the workload.
- `buffer_drop_est` reads HIGH under backpressure: it estimates app-submitted minus
  wire-framed datagrams, which under `send_datagram_wait` is the in-flight queue
  depth (8 MiB buffer), not drops. Backpressure never drops; treat the metric as a
  drop signal only on the non-blocking (hub) path.

## Remaining / follow-up
- **`initial_mtu` align**: quinn still starts MTU discovery at 1200 (4–5 TUN MTU
  changes per link at startup). Threading the TUN MTU into quinn's `initial_mtu`
  (local, no wire change) would remove the startup churn. Deferred (secondary —
  steady state recovers; black-hole ceiling already caps recurring flap).
- **Send-buffer right-sizing**: the 8 MiB datagram send buffer bufferbloats RTT
  (~116 ms vs a 20 ms link) and makes backpressure engage late. Sizing it to ~1–2×
  BDP would tighten pacing — but it is shared with transfer/vhost; needs care.
- **Single-flow floor**: only a reliable per-flow tunnel mode could hide path loss
  from inner TCP, at the cost of head-of-line blocking. Out of scope (v1).

## Original fix priority (for reference)
- **P0 backpressure** (F3): stop adding loss → single flow ramps, bufferbloat bounded.
- **P0 carriers** (F2): fix flow-pinned steering, or disable `--carriers` on direct.
- **P1 MTU** (F4): `initial_mtu` align + `--mtu` pin → no churn, clean measurement.
- **User-side**: parallel inner flows for bulk; `--carriers 1` (default) on direct.

## Caveats
- netns + `netem` ≠ a real Internet path (single FIFO, synthetic loss). Numbers
  are directional, not absolute. Real-path validation still required.
- A stale root `bore server` can survive an aborted bench (netns deleted out from
  under it); reap with `sudo pkill -f target/release/bore` between runs.
