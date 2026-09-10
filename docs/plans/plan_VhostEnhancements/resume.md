# Vhost Enhancements — resume state

> Update this file at the end of every working session. It is the only file a
> new session needs to read to know where the work stands.

## Status: Phase 01 complete (code + gates), in-vivo validation pending

Authored 2026-09-10 by Opus, from
`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md`. Branch `main`.

Phase 01 landed the same working session: wire field, server plumbing, the
tick-checked reaper, the client heartbeat, five gates (the reaper gate and the
real-client gate both red-checked — each FAILS with its fix reverted), README +
CLAUDE.md. Regression: 613 default / 727 `ssh-gateway` / 792 `vpn`, zero
failures; `ssh_gateway_test.sh` netns 21/0; clippy clean on default,
`ssh-gateway` and `--all-features`.

Still open for Phase 01: `scripts/perf/vhost_registration_leak_repro.sh` against
a real deployment. The reaper is SERVER-side, so this needs the test bore
server's Docker image rebuilt from this commit — the operator has offered to do
it on request.

## Phase state

| phase | subject | state |
| --- | --- | --- |
| 01 | vhost heartbeat + reaper (F-1/F-10) | **code + gates done**, in-vivo pending |
| 02 | bound direct-path receive window (F-13) | not started |
| 03 | isolate small requests from bulk (F-15) — **headline** | not started |
| 04 | relay concurrency tail (OQ7) — diagnose first | not started |
| 05 | fast, legible failures (F-14/F-12) | not started |
| 06 | documentation + observability (F-8/F-16/F-6/F-3) | not started |
| 07 | HTTP/2 edge — spike only | not started |

Recommended order: **01 → 02 → 03 → 06 → 05 → 04 → 07.**

## Decisions already locked — do not re-litigate

See `overview.md` for the full statements with rationale.

- **DEC-VE1** stability (01, 02) before the headline optimization (03), because
  F-13 says buffering interacts, not out of caution.
- **DEC-VE2** reap only providers that declare the capability; a legacy client
  is never reaped.
- **DEC-VE3** the liveness deadline is checked on the heartbeat tick, never via
  `timeout(recv)`.
- **DEC-VE4** bulk is classified by bytes moved, never by content sniffing.
- **DEC-VE5** relay and QUIC get different Phase 03 treatments (measured).
- **DEC-VE6** `carriers <= 1` stays byte-for-byte identical.
- **DEC-VE7** Phase 03 targets 7–15 ms p50 under bulk, not 2.5 ms.
- **DEC-VE8** no phase buys latency with buffer memory.

## Things that will bite whoever implements this

1. **`CarrierPool` is shared** by secret (`src/secret.rs:315`), vhost
   (`src/vhost.rs:603`) and SSH-jump (`src/ssh_jump.rs:473`). Phase 03.2 edits
   it. Run `secret_netns_test.sh` and `ssh_gateway_test.sh`, both with
   exact-path `sudo -n /abs/path/...` (`sudo bash scripts/...` prompts and must
   not be used).
2. **In-process async tests false-pass real bugs in this codebase** — twice
   confirmed, once a leak and once the vhost injected-flush bug, which
   *specifically* false-passes on loopback because rustls drains
   opportunistically. Gate Phases 01, 03 and 05.2 at the mock/io-trait level and
   always red-check by reverting the fix.
   (`[[feedback-inprocess-test-false-pass]]`)
3. **Never blanket-`pkill bore`.** It kills unrelated tunnels on the workstation
   including the operator's own. Kill explicit PIDs. Standing project rule,
   violated once during the campaign.
4. **Staging (`brp.0912345.xyz`) is frozen by operator decision** and must not
   be modified. Every server-side change is A/B tested against a private
   `bore server`; `scripts/perf/vhost_header_injection_ab.sh` and
   `vhost_app_ceiling.sh` show the pattern and need no deployment access.
5. **The control drift on staging is 29 %.** Any A/B smaller than that must use
   the paired design (both halves back to back, alternating order, median of
   per-pair ratios) — `scripts/perf/vhost_transport_ab.sh`.
6. **Secrets live outside the repo**, in the env file `${BORE_PERF_ENV}` at
   chmod 600. No script or doc in the repo may contain one.
7. **Measuring from the WiFi workstation measures the WiFi.** Provider and
   consumer on one host means every tunnelled byte crosses the client link
   twice; it caps at ~41–49 MB/s and it retracted four "bore defects" during the
   campaign. A same-region VM is mandatory — evidence document §9.2 provisions
   one.

## Harness inventory

`scripts/perf/` with its own `README.md`; the runbook is §9 of the evidence
document and §9.6 lists the thirteen harness pitfalls that produced wrong
conclusions. Per-phase acceptance harnesses are tabulated in `overview.md`.

## Open questions this plan carries

- **OQ6** — does the stall cliff stay away at smaller QUIC windows? Answered by
  Phase 02.4, with a measurement rather than an assumption.
- **OQ7** — why does the relay develop a concurrency tail? Answered by Phase 04,
  which may find Phase 03 already fixed it.
- **OQ8** — is the 29 % control drift the server, the network or the instance?
  Not scheduled. `steal` is 0.0–0.3 % so it is not burstable CPU throttling; the
  ENA counters show the hypervisor's bandwidth token bucket active at the top of
  the range, which may explain drift in the high-throughput cases but not at
  30 MB/s. It bounds every A/B in the evidence document.
