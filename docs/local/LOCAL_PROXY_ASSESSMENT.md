# `bore local` / `bore proxy` — full assessment

Scope: the public tunnel (`bore local`, no `--tcp-secret-id`) and the secret tunnel
pair (`bore local --tcp-secret-id` provider + `bore proxy` consumer). Shared transport
core: `client.rs`, `server.rs`, `secret.rs`, `shared.rs`, `mux.rs`, `pool.rs`, `edge.rs`,
`reconnect.rs`, `holepunch.rs` (secret direct UDP path), `transport.rs`.

Three questions, in order:
1. Do they fully support **UDP mode**?
2. Is the app configured to **maximise available bandwidth** (TCP and UDP)?
3. **Hardening**: bug hunt, test sufficiency (internal + e2e), documentation freshness.

Verdict summary:

| # | Question | Verdict |
|---|----------|---------|
| 1 | UDP support | **Two different meanings — see below.** UDP *application forwarding*: NOT supported, by design (bore is a TCP forwarder). UDP *transport* (direct hole-punched QUIC): fully supported for **secret tunnels only**; silently ignored on public tunnels. |
| 2 | Bandwidth | TCP path: well tuned, kernel-autotuned, nothing missing. UDP/QUIC secret direct path: well tuned in code BUT throughput-capped for **unprivileged** clients (the `SO_*BUF` clamp), and a single bulk flow cannot be widened (no `--carriers` over QUIC for secret tunnels). |
| 3 | Hardening | Prior adversarial review exists (`LOCAL_PROXY_TESTUDP_AUDIT.md`). In-process e2e coverage is broad. **Gaps:** no out-of-process / netns e2e harness (vpn + vhost both have one), several untested invariants, stale docs, 1 real UX bug. |

---

## 1. UDP support

"UDP mode" is ambiguous; both readings matter and the answers differ.

### 1a. UDP *application* traffic forwarding — NOT supported (by design)

`bore local` and `bore proxy` forward **TCP only**. Evidence:

- Public tunnel local side binds a `TcpListener` and `TcpStream::connect`s the local
  service (`client.rs:1427`, `tune_tcp` on the TCP stream at `client.rs:1433`).
- Secret consumer binds a `TcpListener` (`secret.rs:812`).
- Server tunnel port is a `TcpListener` (`server.rs`), spliced with
  `copy_bidirectional_with_sizes` (byte stream, TCP semantics).
- README line 20: *"unopinionated tool for forwarding **TCP** traffic"*.

A UDP application (DNS, game servers, WireGuard, QUIC servers, SIP/RTP) **cannot** be
tunnelled through `bore local`/`bore proxy`. The product surfaces for UDP datagram /
L3 traffic are separate subcommands: `bore vpn` (L3 IP overlay) and `bore transfer` /
`bore test-udp` (QUIC datagrams). This is a deliberate scope boundary, not a defect —
but it is **not stated** in the `local`/`proxy` help text, so a user may expect `--udp`
to mean "tunnel my UDP service". It does not.

> Recommendation (DOC): one line in README + `--help` clarifying that `--udp` selects a
> UDP *transport* for the tunnel control/data path and does **not** forward UDP
> application traffic; point UDP-service users at `bore vpn`.

### 1b. UDP *transport* (direct hole-punched QUIC) — supported for SECRET tunnels only

The `--udp` flag means "prefer a direct UDP hole-punched path (QUIC), fall back to the
server relay if unavailable". It applies to **secret tunnels only**:

- `bore proxy --udp` (consumer) and `bore local --tcp-secret-id --udp` (provider): direct
  path negotiated via STUN + hole-punch, carried over QUIC, automatic relay fallback.
  This is real, complete, and well covered by `tests/udp_test.rs` (round-trip, many
  concurrent streams, multi-consumer, mixed direct/relay, fallback, upgrade-in-place,
  max-conns, provider-drop detection).
- **Public tunnel (`bore local` with no `--tcp-secret-id`): `--udp` is silently
  ignored.** The dispatch falls into the `None` arm (`main.rs:1306`) which builds
  `TunnelOptions` — a struct that has **no `udp` field**. Every direct-path-only flag
  (`--udp`, `--upnp`, `--stun-server`, `--try-port-prediction`, `--nat-udp-preferred-port`,
  `--nat-udp-release-timeout`) is dropped on the floor. Worse, `main.rs:1256` logs
  `"resolved UDP optimization settings"` at `info` **regardless** of whether a secret id
  was given, so a public-tunnel user passing `--udp` sees a log line implying UDP is
  active when it is a complete no-op. **This is BUG-LP1 (below).** A public tunnel cannot
  go direct because the server owns the public port — the relay is intrinsic — so the
  fix is to warn + scope the log, not to add a direct path.

**Q1 answer:** UDP application forwarding — no, by design (TCP forwarder). UDP transport
— yes for secret tunnels (mature, well tested), silently no-op for public tunnels (bug).

---

## 2. Bandwidth posture

### 2a. TCP path (public tunnel + secret relay) — tuned, nothing missing

| Knob | Value | Where |
|------|-------|-------|
| Copy buffer (per direction) | **256 KiB** default, `[4 KiB, 16 MiB]`, env `BORE_PROXY_BUFFER_SIZE` | `shared.rs:43` |
| yamux receive window | **auto-tuned** (`set_max_connection_receive_window(None)`) → grows to BDP | `mux.rs:63` |
| yamux max streams | 65536 (64-bit) / 8192 (32-bit); real bound is `--max-conns` semaphore | `mux.rs:64` |
| `TCP_NODELAY` + `SO_KEEPALIVE` 15 s | every data socket via `tune_tcp` | `shared.rs:156` |
| Carriers (HOL + cwnd isolation) | default 1; round-robin; server cap `--max-carriers` 16 | `pool.rs`, `server.rs:1043` |

All data-path TCP sockets are tuned (server accept, tunnel accept, client local connect,
secret consumer accept) — the inventory found **no** untuned data socket.

There is **no explicit `SO_SNDBUF`/`SO_RCVBUF`** on the TCP data sockets. This is
**correct**: Linux TCP receive/send buffer autotuning (`tcp_moderate_rcvbuf`, scaling up
to `tcp_rmem[2]`/`tcp_wmem[2]`, default 6 MiB/4 MiB) covers high-BDP links without a
manual set, and a manual `setsockopt` would *disable* autotuning. Contrast UDP (2b),
which the kernel does not autotune. No change needed.

A single bulk TCP flow rides one yamux stream on one carrier (Mathis: throughput bound by
one congestion window). `--carriers N` only helps *concurrent* connections. This matches
the documented design (README §carriers) and the `local`/`proxy` `--help`.

### 2b. UDP/QUIC secret direct path — tuned in code, two real ceilings

| Knob | Value | Where |
|------|-------|-------|
| QUIC stream recv window | 16 MiB | `shared.rs:124` |
| QUIC connection recv window | 64 MiB | `shared.rs:127` |
| QUIC send window | 64 MiB | `shared.rs:131` |
| UDP `SO_RCVBUF`/`SO_SNDBUF` | request 16 MiB | `shared.rs:134/137` |
| Congestion control | **BBR** | `holepunch.rs:1600` |
| Max concurrent bidi streams | 4096 | `shared.rs:141` |
| Keep-alive / idle | 3 s / 10 s | `holepunch.rs:76/78` |

Server can override all windows/buffers via `--udp-*` flags (brokered in the tuning
struct). Each proxied connection = its own QUIC bidi stream (concurrency isolated).

**Ceiling 1 — unprivileged socket-buffer clamp (the dominant real-world cap).**
`configure_udp_socket_buffers` (`holepunch.rs:157`, Linux) tries `SO_*BUFFORCE` first
(bypasses `net.core.{r,w}mem_max`), falls back to the clamped `SO_*BUF` on `EPERM`, then
getsockopt-verifies and `warn!`s the remediation. `SO_*BUFFORCE` needs **CAP_NET_ADMIN**.
`bore vpn` has it (runs as root). **`bore local --udp` / `bore proxy --udp` secret
tunnels normally run as an ordinary user → `EPERM` → buffers clamped to `*mem_max`**
(stock Ubuntu/Debian/AWS default 208 KiB) → a single direct flow is capped at
~`buffer/RTT` (≈ 10 MB/s at 20 ms RTT) with the CPU idle, exactly the VPN-doc symptom but
**without the VPN's privilege to force past it**. The `warn!` fires with the
`sysctl net.core.{r,w}mem_max=16777216` remediation — but this is **undocumented** in the
`local`/`proxy` / secret-tunnel docs (only in the VPN bandwidth doc). DOC gap, not a code
bug. (No `--carriers` benefit here either — see Ceiling 2.)

**Ceiling 2 — secret direct path is single-QUIC-connection.** `--carriers` for secret
tunnels widens the **relay** TCP pool only; the direct QUIC path is one connection per
consumer (README line 200; `local --help`: *"Direct UDP ignores it"*). VPN got
multi-carrier direct QUIC (one congestion controller each); secret tunnels did **not**.
So a single bulk transfer over the secret direct path rides one BBR controller and cannot
be parallelised at the connection level (only by running multiple application
connections, each on its own stream). This is a documented design limit, consistent with
the Mathis reasoning in the VPN bandwidth assessment. Widening it (per-flow carrier
steering as in VPN BW-F2) is a **possible future enhancement**, not a regression.

**Q2 answer:** TCP is maximised (kernel autotune + correct tuning). UDP/QUIC is correctly
tuned in code but the realised throughput for an unprivileged secret tunnel is capped by
the kernel `*mem_max` clamp (mitigation = sysctl, must be documented) and by the
single-connection direct path (by design).

---

## 3. Hardening — bugs, tests, docs

### 3.1 Bug findings

| ID | Sev | Status | Issue |
|----|-----|--------|-------|
| BUG-LP1 | LOW (UX/correctness) | **fixed, then superseded** | Public `bore local` silently dropped every direct-path-only flag and still logged `"resolved UDP optimization settings"`. First fixed with a `warn!` + scoped log. **Now superseded:** `bore local --udp` on a public tunnel is a real feature (server→client QUIC direct path, mirrors vhost) — see `docs/LOCAL_UDP_PLAN.md`. `--udp` works; only the hole-punch helper flags (`--upnp`/`--stun-server`/`--try-port-prediction`/`--nat-udp-*`) remain secret-only and still `warn!` on a public tunnel. |
| (prior) | — | already fixed | Slowloris TLS-handshake permit pin (timeout) and non-uniform per-conn logging — see `LOCAL_PROXY_TESTUDP_AUDIT.md`. |

Re-verified as **NOT bugs** (carried from the prior audit, still valid): `mux::drive`
channel-full deadlock (acceptors drain immediately), missing copy idle-timeout (keepalive
+ QUIC idle cover dead peers; idle-but-live must survive), `CarrierPool::pick` race (lock
held without `.await`), public `pool.pick()→None` (defensive arm), `--max-conns` permit
leak (every exit path drops it), unbounded mpsc growth (connection-scoped + capped),
`.expect()` on UDP paths (guarded/infallible-by-construction).

Accepted limitations (unchanged, documented): no client-side heartbeat *deadline* (rests
on `SO_KEEPALIVE` 15 s + `--auto-reconnect`); `provider_direct` accept retry re-logs a
persistently broken peer every 100 ms; `test-udp` waits unbounded (interactive).

No new race / deadlock / leak found in this pass beyond BUG-LP1.

### 3.2 Test coverage

In-process e2e coverage is **broad** (≈68 integration + 67 src unit tests across
`e2e_test.rs`, `secret_test.rs`, `secret_pool_test.rs`, `udp_test.rs`, `carrier_test.rs`,
`tls_test.rs`, `basic_auth_test.rs`, `reconnect_test.rs`, `control_port_test.rs`,
`mux_test.rs`, `auth_test.rs`). Covered: public happy path, TLS/https/force-https,
basic-auth (public + secret), max-conns (public + direct), carriers round-robin (public +
secret relay, provider/consumer/both), secret relay round-trip + large payload, secret
direct UDP (round-trip, 30 concurrent streams, multi-consumer, mixed, fallback,
upgrade-in-place, provider-drop), reconnect, half-close, WebSocket (public/relay/direct),
admin registry, custom control port, invalid address, frame-size limit, mismatched/dup
secrets.

**GAPS (to add):**

1. **No out-of-process / netns e2e harness.** vpn has `scripts/vpn_netns_test.sh`, vhost
   has `scripts/vhost_netns_test.sh`; **local/proxy has none**. All tests are in-process
   on loopback — no separate processes, no real NIC, no kernel routing, no
   latency/loss/reorder, no `--carriers` benefit demonstrable (loopback has ~0 RTT). This
   is the single biggest hardening gap and the most likely place a real-world bug hides
   (slow-loss links, partial writes, MTU, buffer clamp under load). **→ build
   `scripts/local_proxy_netns_test.sh`.**
2. **STREAM_READY banner-first ordering** — invariant ("server writes `mux::STREAM_READY`
   before splice; banner-first protocols need it") is only implicitly exercised. Add an
   explicit test: server-pushed banner arrives before any client byte.
3. **`Hello`-before-auth lazy-yamux ordering** — invariant enforced in code, never
   isolated. Add a negative test (no Hello → deadlock/timeout) at unit level if feasible.
4. **TLS + carriers>1** — TLS tests are single-connection; no `--https` + `--carriers N`.
5. **TLS + basic-auth** — no combined test (public or secret).
6. **Rapid connect/close churn vs max-conns** — `concurrent_connections_are_bounded`
   holds conns open; no rapid-cycle test that the permit is released and the limit
   recovers (the slowloris fix area).
7. **Public-tunnel `--udp` no-op behaviour** — assert BUG-LP1's fixed behaviour (warn,
   no UDP attempt) so it cannot silently regress.
8. **Unprivileged UDP buffer clamp path** — assert `configure_udp_socket_buffers` does
   not error and the warn path is taken when forcing fails (unit, mock/getsockopt).

### 3.3 Documentation freshness

| Doc | Issue | Action |
|-----|-------|--------|
| `docs/LOCAL_PROXY_TESTUDP_AUDIT.md` | Says **"64 KiB copy buffers"** — actual default is **256 KiB** (`shared.rs:43`). Stale since the buffer was bumped. | Fix to 256 KiB. |
| README + `local`/`proxy` `--help` | `--udp` described as "secret tunnels only" but no note that it is a **no-op on public tunnels**, nor that `--udp` is a *transport*, not UDP-app forwarding. | Add a one-line clarification (ties to BUG-LP1). |
| README / secret-tunnel docs | Non-root `SO_*BUF` clamp (Ceiling 1) and its `sysctl` remediation are documented only for VPN, not for secret `--udp` tunnels. | Add a "direct-path throughput on unprivileged hosts" note. |
| `docs/CARRIER_TUNING.md` / README | Re-confirm the "single flow ⇒ one carrier" + "direct ignores carriers for secret" statements still match code (they do) — no change, just verify on edit. | Verify. |

---

## Hardening plan (phased, model-tiered per CLAUDE.md)

**Phase A — code fix (Sonnet, supervised by Opus).** BUG-LP1: warn + scope log in
`main.rs` public-tunnel arm. Unit test in `main.rs` `#[cfg(test)]` or a new
`tests/*` asserting the public path does not attempt UDP. Gate: `fmt` + `clippy -D` +
`cargo test`.

**Phase B — netns e2e harness (Sonnet).** `scripts/local_proxy_netns_test.sh` modelled on
`scripts/vhost_netns_test.sh`: server + client + proxy as **separate processes** in
isolated network namespaces; cases = public TCP relay, secret relay, secret direct UDP
(with the harness granting CAP_NET_ADMIN so the forced buffers path is also exercised),
`netem` latency/loss/reorder, `--carriers N` throughput delta, `--max-conns` enforcement,
TLS, basic-auth. NOPASSWD-sudo invocation parity (`sudo -n /abs/path/script.sh`). Full run
awaits sudo (same posture as the vpn/vhost hardening efforts).

**Phase C — in-process tests (Sonnet).** Gaps 2–8 above.

**Phase D — docs (Haiku).** Section 3.3 edits.

**Quality bar:** zero regressions; every phase runs `cargo fmt`, `cargo clippy -- -D
warnings`, `cargo test` (+ `--features udp`). Docs updated alongside behaviour changes.
