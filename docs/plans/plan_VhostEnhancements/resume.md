# Vhost Enhancements — resume state

> Update this file at the end of every working session. It is the only file a
> new session needs to read to know where the work stands.

## Status: phases 01, 02, 03, 05 and 06 complete (code + gates). 04 and 07 need measurement.

Authored 2026-09-10 by Opus, from
`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md`. Branch `main`.

| phase | subject | state |
| --- | --- | --- |
| 01 | vhost heartbeat + reaper (F-1/F-10) | **code + gates done**, in-vivo pending |
| 02 | bound direct-path receive window (F-13) | **code + gates done** |
| 03 | isolate small requests from bulk (F-15) — **headline** | **code + gates done**; 03.5 calibration pending |
| 04 | relay concurrency tail (OQ7) — diagnose first | **not started** — needs staging on the new code |
| 05 | fast, legible failures (F-14/F-12) | **code + gates done**, one documented deviation |
| 06 | documentation + observability (F-8/F-16/F-6/F-3) | **done** |
| 07 | HTTP/2 edge — spike only | **not started** — measurement phase |

Commits: `191a2f1` (01 + 02.1), `d4b6a1e` (02 + 03.1/03.2), then this session's
commit for 03.3, 03.4, 05 and 06.

## Regression at the end of this session

- `cargo test --all-features`: **948 passed / 0 failed** (2 ignored).
- `npm test`: **101 / 101**.
- `cargo clippy --all-targets -D warnings` clean on **four** feature sets:
  default, `--no-default-features`, `ssh-gateway`, `--all-features`.
- `sudo -n /abs/path/scripts/ssh_gateway_test.sh`: **21 / 0**.
- `sudo -n /abs/path/scripts/secret_netns_test.sh`: **29 / 0** (run because
  phase 03.2 edits the shared `CarrierPool`; never run two netns harnesses
  concurrently).

## What landed, phase by phase

### 01 — control liveness (committed `191a2f1`)
`HelloVhost::ctrl_heartbeat` (additive), `serve_vhost_provider` tracks
`last_recv` and reaps on the 500 ms heartbeat tick, client heartbeats every 20 s
against a 60 s deadline, `Server::vhost_ctrl_timeout` for tests. A legacy
provider declares `false` and is never reaped (DEC-VE2).

### 02 — bounded direct-path memory (committed `d4b6a1e`)
`--udp-memory-budget` derives the QUIC windows and the number of admissible
direct carriers; the refusal is counted as `direct_budget_refusals` on
`/admin/api/v1/metrics`. 02.1 stopped the admin config view restating the UDP
windows and derived them from `UdpDirectTuning::default()` instead.

### 03 — isolate small requests from bulk
- **03.1/03.2** (committed `d4b6a1e`): bulk classified by **bytes moved**
  (DEC-VE4, 512 KiB), occupancy released by RAII (`BulkTicket`), and
  `CarrierPool::pick_avoiding_bulk` steers a new proxied connection to the
  least-loaded carrier. `carriers <= 1` stays byte-for-byte (DEC-VE6).
- **03.3 adaptive carriers, `--carriers 0`**: the pool starts at 1 and grows
  when the *least*-loaded carrier already carries bulk (⇒ every carrier is
  occupied), rate-limited to one step per 2 s, capped by
  `min(VHOST_AUTO_CARRIER_CEILING = 4, --max-carriers)`, and decays after a
  60 s quiet period. The server asks with `ServerMessage::SetCarrierTarget`.
  **DEC-VE9: lowering the target never closes a live carrier** — it only stops
  replacing dead ones.
- **03.4 QUIC per-stream scheduling**: whoever *sends* the bulk demotes its own
  stream (`set_priority(-1)`) after 512 KiB and caps writes at 128 KiB, so it
  works for downloads (provider side) and uploads (server side) without
  assuming which side starves. **DEC-VE8 held: neither receive window was
  raised**; the 16:1 conn/stream ratio is untouched.

### 05 — fast, legible failures
- **05.1**: the QUIC direct open is bounded at 3 s (`DIRECT_OPEN_TIMEOUT`,
  `BORE_DIRECT_OPEN_TIMEOUT_MS` for tests) and that *same* request falls back
  to the warm relay.
- **05.2**: an unreachable origin gets a synthesized **502**, produced inside
  `poll_shutdown` of a public-side wrapper (`OriginFailureResponder`).
- **05.3**: `/admin/api/v1/vhost` gained `current_path`, `direct_fallbacks` and
  `carrier_target`; `direct_stream_opens` now counts **successful** opens only.

### 06 — documentation and observability
- **06.1/06.2**: README states the transport recommendation, the `--udp`
  0.96 Gbit/s ceiling, and the 5.74 CPU-s/GiB sizing rule *with its method*.
- **06.3**: benign disconnects are classified by `std::io::ErrorKind`
  (`shared::is_benign_disconnect` / `is_benign_io_error`), never by message
  text, and demoted to `debug`. 718 of 786 staging warnings were
  `peer closed connection without sending TLS close_notify`.
- **06.4**: `/admin/api/v1/config` now **derives** its whole vhost section from
  the live `SharedVhostConfig` on every read
  (`admin_api::overlay_vhost_config`), so `default_response_headers`,
  `default_headers` and the reservations are reported — and a hot reload moves
  the view. `vhost_quic_port` and the config file path stay the startup
  snapshot on purpose: neither is hot-reloadable and neither lives in
  `VhostConfig`.

## Deviations from the plan as written — read before re-litigating

1. **Phase 03 acceptance #1 ("7–15 ms band at the *default* carrier setting")
   is unreachable as written.** `bore vhost --carriers` defaults to **1**, and
   03.2/03.3 are inert with one carrier by DEC-VE6. The implementation adds
   `--carriers 0` (auto) and **leaves the default at 1**, so nobody who does not
   opt in changes behaviour. Flipping the default is an operator decision, not
   an implementation one.
2. **504 is deliberately NOT synthesized** (phase 05.2 mentions it). The server
   cannot distinguish a hung origin from a legitimate long-poll or SSE stream,
   so a first-byte deadline would break long-polling. Only the *connect* failure
   (502) is synthesized, which is unambiguous.
3. **DEC-VE9** (above): a lowered carrier target never tears down a live
   carrier.

## Interop hazard found and closed in 03.3

Server→client wire additions are **not** symmetric with client→server: an old
client cannot deserialize an unknown `ServerMessage` variant, and on this wire
that is a hard control-loop error, not a skipped field. `SetCarrierTarget` is
therefore gated on the additive `HelloVhost::auto_carriers` (same precedent as
`Warning`). An *auto* client against an *old* server would otherwise hang
waiting for a `CarrierToken` the old server never sends; closed by (a) always
issuing the token to an auto provider even at `--max-carriers 1`, and (b)
failing fast client-side with the remedy in the message.

## Still open

- **In-vivo, needs the test bore server's Docker image rebuilt from this
  commit** (CI publishes `ghcr.io/manprint/bore:main`; staging runs `:latest`).
  The operator has offered to do it on request. Blocked on it:
  - phase 01 reaper: `scripts/perf/vhost_registration_leak_repro.sh`
  - phase 03.3 adaptive carriers end to end (server-side growth requests)
  - phase 04 g8 concurrency ladder re-measurement (OQ7)
  - phase 05.1 netem blackhole redo (`scripts/perf/vhost_netem_matrix.sh` G6)
- **Phase 03.5 calibration** — bulk threshold, growth/shrink timings and the
  QUIC priority side, with `scripts/perf/vhost_bulk_latency.sh` (paired design,
  server-side `relay_tx_bytes` deltas). The *provider*-side half of 03.4 is a
  client-only change and is measurable against frozen staging today.
- **Phase 07** — HTTP/2 spike: 7.1 page-load measurement versus RTT, 7.2
  read-only graft study, 7.3 go/no-go. No production code in that phase.

## Decisions already locked — do not re-litigate

See `overview.md` for the full statements with rationale.

- **DEC-VE1** stability (01, 02) before the headline optimization (03).
- **DEC-VE2** reap only providers that declare the capability.
- **DEC-VE3** the liveness deadline is checked on the heartbeat tick, never via
  `timeout(recv)`.
- **DEC-VE4** bulk is classified by bytes moved, never by content sniffing.
- **DEC-VE5** relay and QUIC get different Phase 03 treatments (measured).
- **DEC-VE6** `carriers <= 1` stays byte-for-byte identical.
- **DEC-VE7** Phase 03 targets 7–15 ms p50 under bulk, not 2.5 ms.
- **DEC-VE8** no phase buys latency with buffer memory.
- **DEC-VE9** lowering the carrier target never closes a live carrier.

## Things that will bite whoever implements this

1. **`CarrierPool` is shared** by secret (`src/secret.rs`), vhost
   (`src/vhost.rs`) and SSH-jump (`src/ssh_jump.rs`). Phase 03.2 edits it. Run
   `secret_netns_test.sh` and `ssh_gateway_test.sh`, both with exact-path
   `sudo -n /abs/path/...` (`sudo bash scripts/...` prompts and must not be
   used; the sudoers glob does not cross `/`, so `scripts/perf/*.sh` prompts).
2. **In-process async tests false-pass real bugs in this codebase** — three
   times confirmed now. The 502's flush gate false-passed until it asserted on
   the *sender* rather than on a stream a later shutdown would flush anyway.
   Gate at the mock/io-trait level and **always red-check by reverting the
   fix**. (`[[feedback-inprocess-test-false-pass]]`)
3. **Never blanket-`pkill bore`.** Kill explicit PIDs.
4. **Staging (`brp.0912345.xyz`) is frozen by operator decision.**
5. **The control drift on staging is 29 %.** Anything smaller must use the
   paired design (`scripts/perf/vhost_transport_ab.sh`).
6. **Secrets live outside the repo**, in `${BORE_PERF_ENV}` at chmod 600.
7. **Measuring from the WiFi workstation measures the WiFi** (~41–49 MB/s cap,
   it retracted four "bore defects"). A same-region VM is mandatory.
8. **The 502 must be synthesized inside `poll_shutdown`.**
   `copy_bidirectional` shuts the public write half down as soon as the
   provider EOFs, so a post-splice write goes nowhere (measured: empty body),
   and a pre-splice first-byte read deadlocks uploads. Both rejected
   alternatives are recorded in the code.

## Harness inventory

`scripts/perf/` with its own `README.md`; the runbook is §9 of the evidence
document and §9.6 lists the thirteen harness pitfalls that produced wrong
conclusions. This session extended two of them: `vhost_remote_stability.sh` g5
now gates dead-origin→502 / restored→200 / closed-port→502 /
unknown-subdomain→404 and g6 prints per-tunnel `path=` / `fallbacks=`;
`vhost_netem_matrix.sh` G6 measures the FIRST request during a UDP blackout
separately, which is phase 05.1's actual gate.

## Open questions this plan carries

- **OQ6** — does the stall cliff stay away at smaller QUIC windows? Phase 02.4.
- **OQ7** — why does the relay develop a concurrency tail? Phase 04, which may
  find Phase 03 already fixed it.
- **OQ8** — is the 29 % control drift the server, the network or the instance?
  Not scheduled. `steal` is 0.0–0.3 %, so not burstable throttling; the ENA
  counters show the hypervisor's bandwidth token bucket active at the top of
  the range.
