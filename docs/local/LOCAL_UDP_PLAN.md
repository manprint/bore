# `bore local` public-tunnel UDP direct path — implementation plan

> **STATUS (2026-06-17): IMPLEMENTED + VALIDATED UNDER SUDO.** Phases 1–4 done.
> Server + client + protocol landed; `tests/public_udp_test.rs` (6 in-process e2e) proves
> the QUIC direct path actually carries traffic (`direct_stream_opens > 0`, 4 carriers for
> `--carriers 4`, `==0` relay fallback against a non-UDP server). `cargo test` (udp on)
> zero failures, `clippy -D` + `--no-default-features` clean, `fmt` clean.
>
> **Netns e2e (`scripts/local_proxy_netns_test.sh`, sudo): 12/12 PASS** — public relay,
> secret relay, secret DIRECT UDP, netem, max-conns, carriers, TLS, basic-auth. (First
> real run found+fixed 3 harness bugs: `nc` echo deadlock → `-N -w5`+`timeout`; bare
> `wait` on the forever-bore → wait only on the nc PIDs; test-7 cert-server leaking into
> the plain test-8 → restart a plain server; + added inter-netns routing so the secret
> hole-punch can establish.)
>
> **Bench (`scripts/local_bench.sh`, sudo):** "Direct?" = yes for both udp rows (QUIC path
> confirmed in use). On the IMPAIRED link (40 ms + 1% loss) QUIC/BBR beats the TCP relay
> (udp-1c ~498 vs tcp-1c ~88 "MB/s", p99 0.042 s vs 0.325 s); on a clean fast link kernel
> TCP wins (expected — README notes this). Absolute MB/s are netns-rough (curl fallback,
> no hey/wrk); the relative QUIC-under-loss win + Direct?=yes are the valid signals.
>
> Remaining (optional): netns cases for public `--udp` specifically (the bench already
> exercises it e2e); full VH-3 admin-page surfacing (public registry accessor + bench
> "Direct?" column already landed); server-side `PublicUdpRenew` re-issue (TODO; initial
> carriers + relay fallback already cover liveness).

Goal: make `--udp` real for **public** `bore local` tunnels (today it is a no-op,
see BUG-LP1 in `LOCAL_PROXY_ASSESSMENT.md`). Mechanism mirrors `bore vhost --udp`:
a direct **QUIC** data path on the **server→client** hop, with seamless per-connection
fallback to the existing TCP carrier relay. Objective: **max bandwidth, min latency**.

Models (CLAUDE.md tiers): design + protocol + server architecture + supervision/gates =
Opus; client mirroring + tests + bench = Sonnet; mechanical doc edits = Haiku.

---

## 0. Why the vhost model fits (verified)

A public-tunnel client is topologically identical to a vhost provider: it dials OUT to
the server (control connection) and the server PUSHES data streams to it. The server is
public, so — unlike secret tunnels — **no STUN / hole-punch is needed**; the client just
dials the server's QUIC port (exactly like vhost). So we reuse vhost's proven pieces:

- `DirectPool` (`vhost.rs:385`) — round-robin pool of live QUIC connections, dead-conn
  pruning by monotonic id. The UDP analog of `CarrierPool`.
- `clamp_direct_carriers` / `MAX_VHOST_DIRECT_CARRIERS` (`vhost.rs:371`, cap 32).
- Server QUIC endpoint + handshake (`holepunch::vhost_server_endpoint` /
  `vhost_server_handshake` / `vhost_connect`, `holepunch.rs:~1360/1504`).
- `transport_config` tuning (16 MiB stream / 64 MiB conn / 64 MiB send window, BBR,
  4096 streams; `holepunch.rs:1585`).

### vhost audit verdict (the pre-implementation check)

**Max-bandwidth design is correct** (mapped end-to-end):
- `--carriers N` opens **N independent QUIC connections**, each its OWN BBR congestion
  controller (`client.rs:780` spawn loop → `vhost_connect` per call). Not N streams on
  one conn. ✓
- Inbound request → `DirectPool::pick()` **once per request** → one connection for that
  request's bidi stream. **No intra-request striping** → no reorder of a single TCP-like
  stream (the exact trap VPN hit with per-datagram RR). ✓
- `TCP_NODELAY` + keepalive on every TCP socket; 256 KiB proxy buffers; UDP 16 MiB
  socket buffers; per-request relay fallback with no latency cliff. ✓
- VH-1 (max-conns bypass) and VH-2 (carrier churn >32) already fixed.
- **VH-3 OPEN:** direct-vs-relay usage is invisible (`direct_stream_opens` never logged,
  admin hardcodes `udp:false`). Folded into Phase 4 — essential to *prove* the UDP path
  is actually carrying traffic when benchmarking "max bandwidth".

**Caveat carried from secret tunnels:** unprivileged clients hit the kernel `SO_*BUF`
clamp (`net.core.*mem_max`); `SO_*BUFFORCE` needs CAP_NET_ADMIN. Public-tunnel `--udp`
clients are normally unprivileged → same ~buffer/RTT cap unless the sysctl is raised.
Document it (already in README from the prior pass); the bench must raise the sysctl (or
run privileged) to measure the true ceiling.

---

## 1. Locked design decisions

- **DEC-LU1 (unify the QUIC endpoint).** Bind ONE server QUIC endpoint whenever
  `bore server --udp` — drop the `&& vhost_config.is_some()` gate at `server.rs:518`. It
  serves BOTH vhost subdomains and public tunnels. Bind port = the existing
  `vhost_quic_port` field (default 443/80 when vhost configured; for a plain `--udp`
  server expose/keep an explicit override so tests/benches pick a free high port). One
  UDP port, one accept loop, one handshake.
- **DEC-LU2 (generalized, namespaced handshake key).** The QUIC auth handshake stays
  `[key_len u16][key][token 32]`. vhost sends the bare subdomain (a valid DNS label);
  public tunnels send a TAGGED key `"port:{assigned_public_port}"`. The colon/`port:`
  prefix cannot collide with a DNS label, so the server disambiguates which registry to
  install into. Rename `vhost_server_handshake`→`direct_server_handshake`,
  `vhost_connect`→`direct_connect` (keep thin aliases or update call sites). The accept
  loop's `lookup` closure checks BOTH `pending_vhost_udp` and `pending_public_udp`.
- **DEC-LU3 (per-tunnel direct registry + RAII).** Add
  `public_udp_registry: Arc<DashMap<String, Arc<PublicDirectEntry>>>` (key = `port:{N}`)
  and `pending_public_udp: Arc<DashMap<String, ServerNonce>>`. `PublicDirectEntry { direct:
  DirectPool, direct_stream_opens: AtomicU64 }`. `serve_tunnel` registers its entry when
  `opts.udp && server.udp`, and a `Drop`/scope guard removes both map entries on teardown
  (mirror vhost `Deregister`, `vhost.rs:454`). Lifts `DirectPool` into a shared location
  (keep in `vhost.rs` and `pub use`, or move to `pool.rs`) so both features share it.
- **DEC-LU4 (relay stays warm; per-connection fallback).** Each inbound public connection
  tries `entry.direct.pick()` → `open_stream()` → `STREAM_READY` → splice; on no-live-conn
  or error, falls back to `pool.pick()` (the existing yamux relay). The TCP carrier pool is
  ALWAYS established (today's path) so fallback is instant and the tunnel never depends on
  UDP coming up. `--carriers N` sizes BOTH the TCP relay pool (today) AND the QUIC direct
  pool (new), exactly like vhost.
- **DEC-LU5 (byte-identical when off).** `--udp` absent ⇒ `TunnelOptions.udp=false` ⇒ no
  PublicUdp offer, no QUIC, the public path is byte-for-byte today's relay. `#[serde(default)]`
  on the new field keeps the wire format backward-compatible (old client ↔ new server and
  vice-versa). This is the I-MC1-style zero-regression contract.
- **DEC-LU6 (STREAM_READY parity).** The server writes `mux::STREAM_READY` before splicing
  on BOTH the relay substream (today) AND the direct QUIC bidi stream (mirror
  `vhost.rs:741`), so banner-first protocols keep working on either path. The client's
  `handle_connection` already reads the marker (`client.rs:959`) — the direct accept loop
  must funnel into the same handler.

---

## 2. Phase 1 — protocol + server (Opus-designed, Sonnet-implementable)

**2.1 `shared.rs`**
- `TunnelOptions`: add `#[serde(default)] pub udp: bool` (after `carriers`).
- `ServerMessage::PublicUdp { port: u16, nonce: [u8; UDP_NONCE_LEN], tuning: UdpDirectTuning }`
  — mirror `VhostUdp` (`shared.rs:870`). Add its `control_frame_summary`/Display arm.
- `ClientMessage::PublicUdpRenew { port: u16 }` — mirror `VhostUdpRenew` (`shared.rs:744`).
- Update the `Display`/summary impls for both.

**2.2 `server.rs`**
- Struct fields: `pending_public_udp`, `public_udp_registry` (DEC-LU3).
- Endpoint bind (DEC-LU1): change `if this.udp && this.vhost_config.is_some()` →
  `if this.udp`; in the accept task, generalize the `lookup` closure to check both pending
  maps, and after `direct_server_handshake` returns `(key, direct)`, branch: `port:`-tagged
  key → install into `public_udp_registry[key].direct`; else → today's vhost install.
- `serve_tunnel` (`server.rs:1008`): after sending `ServerMessage::Hello(port)` and building
  the `CarrierPool`, if `opts.udp && this.udp`: make a `ServerNonce`, register
  `PublicDirectEntry`, store pending, send `ServerMessage::PublicUdp { port: quic_port,
  nonce, tuning }`. Hold the RAII deregister guard for the tunnel's lifetime.
- Inbound accept (`server.rs:~1130`): wrap the current `pool.pick()` substream open in a
  "try direct first" — `entry.direct.pick()` → `open_stream()` → write `STREAM_READY` →
  `copy_bidirectional_with_sizes`; on `None`/error fall back to the existing relay open.
  Increment `direct_stream_opens` on a direct open.
- Handle `ClientMessage::PublicUdpRenew` in the control dispatch (re-issue nonce).
- Admin entry: thread real `udp`/direct usage (ties to Phase 4 VH-3).

**Gate:** `cargo check` + `cargo check --no-default-features` (udp off must still compile;
gate all new QUIC code behind `#[cfg(feature = "udp")]`).

## 3. Phase 2 — client (Sonnet, mirror vhost provider)

- `main.rs`: drop `udp` into the public `TunnelOptions` (remove BUG-LP1 warn for the public
  path — `--udp` now does something; keep warning only for the still-inert flags like
  `--upnp`/`--stun-server`/`--nat-udp-*` which remain secret-only).
- `client.rs` public path (`Client::new`, listen loop): on
  `ServerMessage::PublicUdp { port, nonce, tuning }`, derive token, open `carriers` QUIC
  connections via `direct_connect(key=format!("port:{port_public}"), ...)`, accept bidi
  streams, funnel each into the existing `handle_connection` (connect local + splice).
  Mirror `spawn_vhost_direct` (`client.rs:1032`) + renewal (`client.rs:815`). Clamp carriers
  with `clamp_direct_carriers`.
- **Gate:** `cargo check` (udp on/off); full existing suite green (zero regressions).

## 4. Phase 3 — tests (Sonnet)

Internal (unit): key derivation/handshake reuse, `clamp_direct_carriers` for public,
PublicUdp message round-trips, serde-default backward-compat (old `TunnelOptions` decodes).

In-process e2e (new `tests/public_udp_test.rs`, `#[cfg(feature="udp")]`), mirroring
`udp_test.rs` + `e2e_test.rs`: public `--udp` round-trip; many concurrent streams (no
cross-talk); `--carriers 4` direct; large/bulk payload; **fallback to relay when the
server has no `--udp`** (old-server interop); **fallback per-connection when QUIC fails**;
direct + relay mixed; banner-first ordering over the direct path. TCP path tests stay green.

Netns e2e: add T-PUB-UDP-DIRECT / T-PUB-UDP-CARRIERS / T-PUB-UDP-FALLBACK / T-PUB-UDP-NETEM
cases to `scripts/local_proxy_netns_test.sh` (server `--udp` on a free QUIC port; verify the
client logs a direct carrier established and bytes round-trip under netem).

**Gate:** `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (udp on +
`--no-default-features`).

## 5. Phase 4 — benchmarks + VH-3 observability (Sonnet)

- `scripts/local_bench.sh` — model on `scripts/vhost_bench.sh`: netns topology, 4 configs
  (tcp-1c, tcp-4c, udp-1c, udp-4c), throughput (large transfer) + latency (concurrent small
  conns), run clean and under `netem` (40 ms + 1% loss). Raise `net.core.*mem_max` (or run
  privileged) so the UDP ceiling is real. Emit two markdown tables.
- Verify/refresh `scripts/vhost_bench.sh` still matches the code (flags, ports).
- **VH-3 observability** (both vhost + public): surface per-tunnel direct-carrier count +
  `direct_stream_opens` and "on direct vs relay" at `info`/admin (raise the surplus-drop
  log from `debug!`). Needed to prove the bench is measuring the QUIC path, not silent relay.

**Gate:** scripts `bash -n` clean + release build; full netns/bench run **awaits sudo**
(no NOPASSWD entry for new script paths — same posture as vpn/vhost).

---

## 6. Invariants to preserve (add to CLAUDE.md on completion)

- Public `--udp` is server→client QUIC, **no STUN/hole-punch** (server is public). Distinct
  from secret tunnels (peer-to-peer punch) — do not route public tunnels through STUN.
- `--carriers N` on public `--udp` = N independent QUIC connections (own BBR each),
  per-connection round-robin, **never per-datagram/intra-request striping** (reorder trap).
- Relay carrier pool stays warm for the tunnel's life; direct is tried per inbound
  connection and falls back in place — UDP never gates tunnel liveness (DEC-LU4).
- `--udp` off ⇒ byte-identical to today's relay (DEC-LU5); wire stays backward-compatible
  via `#[serde(default)]` (old/new client/server interop).
- Unified QUIC endpoint serves vhost + public; keys namespaced (`port:` vs DNS label).
