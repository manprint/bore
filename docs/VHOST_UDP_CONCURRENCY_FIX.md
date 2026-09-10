# vhost/local `--udp` carriers=1 concurrency stall — assessment & fix

**Date:** 2026-06-27  ·  **Status:** fixed, gated  ·  **Branch:** dev

## Symptom (field report)

`bore vhost localhost:5000 --subdomain df --id ... -s mysecret --udp --auto-reconnect`
(i.e. **default `--carriers 1`**) serving a web app (dufs, with its own basic auth):
after the basic-auth prompt the page **frequently fails to finish loading** — several
assets stay `pending` in the browser console, the connection appears to **stall**.

- `--carriers 4` → **mitigated** (not always fully fixed).
- without `--udp` (plain TCP relay) → **not observed**.
- public `bore local --udp` → seemed fine (lighter test; same latent risk).

## Root cause — it is NOT a deadlock in bore's task code

Accept loops are correctly concurrent on both ends:

- Server vhost frontend: `handle_http` / `handle_https` are **spawned** per inbound
  connection (`server.rs:743,797`).
- Client provider: `spawn_direct` (`client.rs:1222-1279`) runs one accept loop per
  QUIC connection and **spawns per stream** — non-blocking.
- Data path is clean: `relay_vhost` (`vhost.rs:772-902`) = `pick()` → `open_bi` →
  `STREAM_READY` → `copy_bidirectional`. No yamux-split, half-close propagates.

The stall is **resource exhaustion of the single QUIC connection** that `--carriers 1`
gives a tunnel. With `--carriers 1`, *every* proxied connection is a bidi stream on
**one** QUIC connection, sharing **one** connection-level flow-control window, **one**
BBR controller and **one** UDP socket. Two compounding, carrier-sensitive mechanisms:

### (a) QUIC connection-level flow control — the dominant cause (fixed)

For a secret vhost the **response bytes flow provider → server** over the bidi stream,
so the **server is the receiver**. QUIC only returns *connection-level* flow-control
credit as the receiver **drains** a stream. The server drains by writing into the
public socket, so a **slow/paused public reader** (a browser pausing some assets while
it parses render-blocking CSS/JS) stalls that drain. quinn then buffers up to one full
`stream_receive_window` (**16 MiB**) of unread data per stalled stream **against the
shared `connection_receive_window` (64 MiB before the fix)**.

`64 / 16 = 4`: once **~4 streams** stall, the whole connection runs out of credit and
**every other stream starves** — new requests hang `pending` until a stalled reader
drains. `--carriers N` gave each connection its own 64 MiB window, so the bug spread
thinner with more carriers (hence "mitigated"). Plain TCP relay uses yamux per-stream
windows with **no** shared connection cliff, so it was immune.

**Fix:** raise `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` 64 → **256 MiB** (and
`DIRECT_QUIC_SEND_WINDOW` to parity). Per-stream window unchanged (16 MiB → **no
single-stream throughput regression**). Now `256 / 16 = 16` stalled streams are
tolerated on one connection — the headroom four 64 MiB carriers gave, in the
`--carriers 1` default. It is a **ceiling**, not a reservation: a healthy tunnel that
keeps draining buffers ~0. Tunable via `--udp-connection-receive-window`
(`BORE_UDP_CONNECTION_RECEIVE_WINDOW`) for memory-bound servers.

Files: `src/shared.rs` (constants + rationale), `src/main.rs` (flag defaults
`64MiB` → `256MiB`).

### (b) Non-root UDP socket buffer clamp — secondary (operational)

`bind_socket` → `configure_udp_socket_buffers` (`holepunch.rs:119,191`) requests 16 MiB
but tries `SO_*BUFFORCE` first, which needs **CAP_NET_ADMIN**. `bore vhost`/`bore local`
run **unprivileged**, so on EPERM it falls back to the clamped `SO_*BUF` setter and the
kernel caps the buffer at `net.core.{r,w}mem_max` (stock 208 KiB). One UDP socket then
caps the whole connection at ≈ buffer/RTT and drops on burst → BBR backoff. Spreading
over `--carriers N` gives N sockets (N× aggregate) → mitigated.

bore **cannot** exceed the kernel cap without privilege; it already `warn!`s loudly with
the exact remediation (`"UDP socket buffer clamped below request …"`). **Operational
fix** (host running the provider):

```
sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216   # or
sudo setcap cap_net_admin+ep /path/to/bore
```

Check the provider log for the clamp warning to know whether (b) is in play.

## Reproduction & regression gate

`scripts/vhost_udp_concurrency_repro.sh` (netns ns0/nsp/nsc, provider run as **root** so
(b) is removed and (a) is isolated): pins `SLOW_N=8` slow readers (`--limit-rate 8k`) on
a 48 MiB file to exhaust the server connection window, then times a single fast small
request under load. Asserts it completes < 3 s.

| scenario | before fix | after fix |
|---|---|---|
| R1 non-udp (control) | 0.0 s ✅ | 0.0 s ✅ |
| R2 `--udp --carriers 10` | 0.0 s ✅ | 0.0 s ✅ |
| **R3 `--udp carriers=1`** | **rc=28, 30 s timeout (HANG)** ❌ | **0.0 s ✅** |

## Follow-up measurements (staging campaign 2026-09-10)

Two things were measured after this fix shipped, both recorded in
`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md`.

**The memory bill (F-13).** The connection window is a ceiling, so it is only
paid when readers stop draining — but nothing bounded the total:

| condition | server RSS |
| --- | --- |
| 512 concurrent connections, TCP relay | 95.1 MiB |
| 512 concurrent connections, QUIC direct | 424 MiB (**4.5×**) |
| 32 slow readers on **one** `--udp` tunnel | **536.8 MiB** on a 903 MiB host |

At 536.8 MiB the server timed out two requests, failed a registration, and made
an unrelated tunnel lose its control connection and reconnect. No OOM kill,
`RestartCount 0`, RSS back to 33.6 MB afterwards — exhaustion and recovery, not
a leak, but one client's behaviour degrading the whole server. Fixed by
`--udp-memory-budget` (a server-wide admission budget; see `README.md`).

**The stall rung is the RATIO, not the size (F-17).** The G9 ladder
(`scripts/vhost_udp_window_ladder.sh`) walked 4/8/16/24/32 slow readers across
four window profiles. Every profile holding 16:1 — 256/16, 128/8 and 32/2 MiB,
an 8× range — first stalled at **16** readers; an explicit 8:1 profile (64/8 MiB)
first stalled at **8**. So:

- A smaller profile costs **nothing** in stall tolerance while using up to 3.8×
  less memory (85.0 MiB versus 320.6 MiB at the 16-reader rung). This is why
  `--udp-memory-budget` derives both windows and enforces the ratio rather than
  exposing it as a knob.
- The `64 → 256 MiB` change above did **not** remove the cliff; it **moved** it
  from ~4 stalled streams to ~16. That was enough for the reported field
  workload, and it is still reachable by sixteen paused readers on one
  `--carriers 1` tunnel. `--carriers N` multiplies the tolerance by N (each
  carrier brings its own connection window); the TCP relay has no shared
  connection window and was immune at every rung.

## Verification

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo test` (default features): all suites pass, 0 failures.
- `scripts/vhost_netns_test_hard.sh`: H1–H7 PASS (H3/H6 pre-existing KNOWN BUGs,
  informational), no regression.
- `scripts/vhost_udp_concurrency_repro.sh`: 3/3 PASS after fix.
