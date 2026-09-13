# BORE_DERP.md — Feasibility analysis: `bore` as a Tailscale-compatible DERP server

**Status:** analysis / feasibility study (no code written).
**Date:** 2026-06-11.
**Goal stated by user:** run `bore` in a "derp mode" that is wire-compatible with stock
Tailscale clients (drop-in replacement for the `derper` binary / `sparanoid/derp:edge`
image), reusing bore's relay engine to **maximize bandwidth on relayed flows**, and
ideally beat the well-known DERP "TCP-over-TCP" slowness.

> Sources studied: the official implementation at
> `github.com/tailscale/tailscale/tree/main/derp` (`derp.go`, `derp_server.go`,
> `derp_client.go`, `derphttp/`, `cmd/derper/derper.go`), Tailscale docs, and issues
> [#14791](https://github.com/tailscale/tailscale/issues/14791) (TCP perf),
> [#15522](https://github.com/tailscale/tailscale/issues/15522) (drop-to-signal-congestion),
> [#16028](https://github.com/tailscale/tailscale/issues/16028), the
> [peer-relays blog](https://tailscale.com/blog/peer-relays-international-networks).

---

## 0. TL;DR — verdict up front

1. **Wire-compatible DERP in `bore` is feasible.** It is a self-contained protocol
   (one TCP/TLS connection per client, an HTTP upgrade on `/derp`, a small binary frame
   codec, a NaCl-box handshake, an in-memory pubkey→connection hub, plus a standard STUN
   responder on 3478/udp). Moderate effort — most of it is *new* code, not reuse.

2. **bore's signature engine (carriers, QUIC direct path, multipath) gives ~zero bandwidth
   win on the standard relay path.** Hard reason: **the Tailscale client dictates the
   transport.** A stock client speaks DERP-over-TCP/HTTPS (or DERP-over-WebSocket) on a
   *single* connection. We cannot insert carriers/QUIC/multipath between a stock client and
   our server without forking the Tailscale client. The bottleneck (single TCP connection,
   head-of-line blocking, TCP-over-TCP) lives on the client side, outside our reach.

3. **The realistic bandwidth lever on the compatible path is server-side TCP tuning**, not
   bore's mux engine: BBR congestion control, large `SO_SNDBUF`/`SO_RCVBUF`, and a deep,
   tunable per-client send queue. Stock `derper` ships a *shallow* per-client queue and
   default CUBIC. On high-bandwidth-delay-product links (intercontinental, low loss) this
   tuning alone can be a meaningful multiplier. On **lossy** links the TCP-over-TCP meltdown
   is fundamental and **no TCP relay can fix it** — only a UDP/QUIC relay can.

4. **To actually beat TCP-over-TCP you need a UDP/QUIC relay — which bore already has — but
   that is NOT DERP and a stock client will not use it.** Tailscale's own answer to this is
   **Peer Relays** (UDP, end-to-end encrypted, ~12.5× faster than DERP). For the user's real
   goal ("maximize relayed bandwidth on my own server"), **peer relays are the Tailscale-native
   solution and require no custom server at all.** A bore-specific QUIC-DERP would require
   patching the Tailscale client (upstream work, out of "drop-in" scope).

**Recommendation (see §9):** bore-DERP is worth building **as a consolidation play** (one
binary, coexists with bore's other modes, self-host with no public-DERP fairness throttle,
plus BBR/buffer/queue tuning for a modest real win). Do **not** sell it as a bandwidth
breakthrough — that promise is physically blocked by the client transport. If raw relayed
bandwidth is the only objective, evaluate Tailscale Peer Relays first.

---

## 1. What DERP is (and what the user runs today)

DERP = *Designated Encrypted Relay for Packets*. It is Tailscale's **fallback** path: when
two nodes cannot establish a direct WireGuard (UDP) connection through NAT, they relay their
already-encrypted WireGuard packets through a shared DERP server. DERP also runs a **STUN**
server (3478/udp) used for NAT discovery during direct-connection attempts.

Key framing: **DERP is intentionally a fallback, not a fast path.** It carries WireGuard's
UDP datagrams *inside a TCP stream* — the classic "TCP-over-TCP" anti-pattern (§3).

The user's current deployment (`sparanoid/derp:edge` = official `derper`):

```yaml
ports: [ 80:80, 443:443, 3478:3478/udp ]
command: derper -hostname derps1.0912345.xyz -certdir /app/certs -certmode manual
```

- **443/tcp** — HTTPS; the DERP protocol itself (HTTP upgrade on `/derp`) + probe endpoints.
- **80/tcp** — HTTP; ACME challenges and redirects (here, with `-certmode manual`, mostly redirect/health).
- **3478/udp** — in-process STUN server (`stunserver.New` + `ListenAndServe`).

A bore drop-in must occupy the **same three ports** and answer the **same protocol** on each.

---

## 2. How the official `derper` works (the spec a bore implementation must match)

### 2.1 Transport & endpoints (`cmd/derper/derper.go`, `derp/derphttp/`)

- HTTPS on `-a` (default `:443`); HTTP on `-http-port` (default `80`, `-1` disables).
- TLS via `-certmode {manual|letsencrypt|gcp}`, `-certdir`, `-hostname`. Enforces `MinVersion TLS 1.2`.
- DERP protocol served at HTTP path **`/derp`** via **connection hijacking** (not a classic
  `Upgrade:` dance for the raw path). A **WebSocket** transport variant is also supported
  (`AddWebSocketSupport`) for clients/proxies that can't do raw hijack.
- Probe/diagnostic endpoints: **`/derp/probe`** and **`/derp/latency-check`** (latency
  measurement), **`/generate_204`** (captive-portal check, returns 204).
- STUN: in-process when `-stun` is set, on `-stun-port` (default `3478/udp`).
- Meshing: `-mesh-with` hostnames; `-verify-clients` gates who may connect (via local `tailscaled`).

### 2.2 Wire protocol (`derp/derp.go`) — exact constants

- **Magic:** `"DERP🔑"` (8 bytes: `44 45 52 50 f0 9f 94 91`).
- **ProtocolVersion:** `2` (v2 added the source key in `RecvPacket`).
- **Frame header:** `FrameHeaderLen = 1 + 4` → 1-byte type + **4-byte big-endian length**.
- **`MaxPacketSize = 64<<10`** (65536) — max relayed packet payload.
- **`MaxInfoLen = 1<<20`**, **`NonceLen = 24`**, **`KeyLen = 32`**, **`KeepAlive = 60s`**.

Frame types:

| Frame | Hex | Payload |
|---|---|---|
| `ServerKey` | `0x01` | 8B magic + 32B server pub key |
| `ClientInfo` | `0x02` | 32B client pub key + 24B nonce + NaCl-boxed JSON |
| `ServerInfo` | `0x03` | 24B nonce + NaCl-boxed JSON |
| `SendPacket` | `0x04` | 32B **dst** key + packet |
| `RecvPacket` | `0x05` | 32B **src** key + packet (v2) |
| `KeepAlive` | `0x06` | — |
| `NotePreferred` | `0x07` | 1B |
| `PeerGone` | `0x08` | 32B key + 1B reason |
| `PeerPresent` | `0x09` | 32B key + optional 18B ip/port + flags |
| `ForwardPacket` | `0x0a` | 32B src + 32B dst + packet (**mesh**) |
| `WatchConns` | `0x10` | — (mesh peer subscribes to presence) |
| `ClosePeer` | `0x11` | 32B key |
| `Ping` / `Pong` | `0x12`/`0x13` | 8B / 8B echo |
| `Health` | `0x14` | error text |
| `Restarting` | `0x15` | two uint32 ms durations |

### 2.3 Handshake & crypto

1. Server → `ServerKey` (magic + its curve25519 public key).
2. Client → `ClientInfo`: client pub key + nonce + **NaCl box** (`crypto_box`,
   curve25519 + xsalsa20-poly1305) sealed JSON `{Version, MeshKey, ...}`.
3. Server → `ServerInfo`: nonce + NaCl-boxed JSON `{TokenBucketBytesPerSecond, ...}`.

> Crypto note: this is **NaCl `crypto_box`** (curve25519/xsalsa20-poly1305). bore's VPN AEAD
> uses **ChaCha20-Poly1305 + HKDF** — *different primitive*. A new dependency is required
> (pure-Rust `crypto_box` crate; `#![forbid(unsafe_code)]`-clean).

### 2.4 Relay internals — the part that matters for "bandwidth"

This is **not** a byte-stream splice. It is a **frame-switched hub**:

- Server keeps an **in-memory map** `clients: pubkey → *sclient`. One entry per connected node.
- On `SendPacket{dst=B, payload}` from A: server does a **pure in-memory lookup of B**, then
  **enqueues** the frame onto B's per-client send queue. **No second network hop** (single
  server). B's writer goroutine drains the queue and writes `RecvPacket{src=A}` to B's socket.
- Each `sclient` has **two bounded channels**: `sendQueue` and a separate `discoSendQueue`
  (disco/control packets get priority), each of capacity **`perClientSendQueueDepth`** (a
  *shallow* queue — a few dozen packets).
- **Overflow = drop** (`errDropDerpPacket`, recorded). Per issue
  [#15522](https://github.com/tailscale/tailscale/issues/15522), this drop is **intentional**:
  it signals the *inner* (WireGuard-tunneled) TCP to back off. `writeTimeout` ≈ 2s.
- **No explicit per-flow Mbps token bucket in the core relay.** The `ServerInfo` carries a
  `TokenBucketBytesPerSecond` field, but real-world throughput is bounded by (a) TCP-over-TCP
  HOL, (b) the shallow send queue dropping, and (c) **shared-infra contention** on Tailscale's
  *public* DERPs ("fairness throttle"). Self-hosting already removes (c).

### 2.5 Mesh (`ForwardPacket` / `PacketForwarder`)

When A is on server-1 and B is on server-2 (same region, different node), server-1 cannot
find B locally. Mesh nodes subscribe to each other's presence via `WatchConns`
(`PeerPresent`/`PeerGone`), and a `PacketForwarder` sends `ForwardPacket{src,dst}` (0x0a) over
the **server↔server** connection so server-2 delivers it to B. **This is the one hop that is
not client-dictated** → the one place bore's transport could legitimately help (§5.2).

---

## 3. Why DERP is slow — the "TCP-over-TCP" problem (with numbers)

WireGuard is UDP. When relayed via DERP, those UDP datagrams ride **inside a TCP stream**.
Two TCP loss-recovery loops then stack:

- **Head-of-line blocking:** one lost segment on the outer DERP TCP connection stalls *every*
  relayed packet behind it until retransmission — even packets for unrelated peers sharing
  that client's single connection.
- **TCP meltdown:** outer-TCP retransmits + inner-TCP retransmits fight each other; effective
  throughput collapses and RTT spikes.

Measured impact (issue [#14791](https://github.com/tailscale/tailscale/issues/14791) and field reports):

| Scenario | Throughput |
|---|---|
| Direct (UDP/WireGuard) | ~200 Mbps |
| DERP-relayed TCP | **~4–20 Mbps** (≈10% of link) |
| Via exit node over DERP | ~1.6 Mbps, RTT ~992 ms (vs 250 ms direct) |
| Intercontinental public DERP | ~2.2 Mbps (vs ~30–40 Mbps ISP) |
| Tailscale **Peer Relay** (UDP) same link | **~27.5 Mbps (≈12.5×)** |

**The decisive fact:** the limiter is the **single outer TCP connection the client opens**.
Its congestion window, buffers, and loss recovery are owned by the client↔server TCP path.
A relay server can tune *its* side, but it cannot turn one client TCP connection into many,
and it cannot remove HOL blocking from a stream the client insists on using.

---

## 4. bore engine inventory (what exists vs what's needed)

From the bore source (`src/server.rs`, `transport.rs`, `mux.rs`, `pool.rs`, `holepunch.rs`,
`vpn.rs`, `shared.rs`, `Cargo.toml`):

**Reusable / adaptable**

| Capability | Where | Fit for DERP |
|---|---|---|
| rustls TLS acceptor (manual certs) | `transport.rs:178` `load_server_tls` | ✅ direct — DERP 443 frontend, manual certmode |
| TCP socket tuning (`tune_tcp`) | `shared.rs:156` | ⚠️ partial — has NODELAY+keepalive; **needs** big buffers + BBR for DERP |
| STUN responder (UDP) for hole-punch | `server.rs:495`, `holepunch.rs` | ✅ adaptable to a standard RFC-5389 STUN binding server on 3478 |
| Concurrent registry / liveness | `pool.rs`, `dashmap` | ✅ pattern for the `pubkey→client` hub + presence |
| QUIC endpoint (quinn 0.11) + datagrams | `holepunch.rs:990`, `DirectConn` | ✅ only useful for **mesh** hop or a non-compat QUIC variant (§5.2/§6) |
| First-byte HTTP sniff (admin/vhost) | `admin_http.rs:45` | ⚠️ minimal request-line parse only — **not** a real HTTP/1.1 server |

**Missing — must be built**

- **HTTP/1.1 server with connection hijack/upgrade** for `/derp` + `/derp/probe` +
  `/generate_204`. bore has **no** hyper/axum/tungstenite. Options: hand-roll the tiny subset
  DERP needs (request line + headers + 101/hijack), or add `hyper` (heavier, but gives the
  WebSocket-DERP variant for free with `tokio-tungstenite`).
- **NaCl `crypto_box`** (curve25519/xsalsa20-poly1305) — new pure-Rust dependency
  (`crypto_box` crate). bore's existing ChaCha20-Poly1305 AEAD does **not** match.
- **DERP frame codec** (1B type + 4B BE len + payload) — trivial, ~100 LOC.
- **The frame-switched hub**: per-client read loop + bounded send queues + drop accounting +
  presence/`PeerGone`/`WatchConns`. This is **the wrong shape for `copy_bidirectional`** — it
  is a 1-connection-to-N-peers switch, closer to bore's **VPN server broker**
  (`vpn_server.rs`) than to the public-tunnel splice path. **Do not reuse the splice
  abstraction here.**
- **DERP map / region config** generation (the JSON a tailnet admin feeds to Tailscale).
- Optional: **mesh** (`-mesh-with`, `PacketForwarder`), `-verify-clients`.

> Correction to an over-optimistic first pass: bore's `copy_bidirectional_with_sizes`,
> carrier round-robin, and yamux substreams are built for **point-to-point byte tunnels**.
> DERP relay is a **packet hub**. The reusable parts are TLS, STUN, the registry pattern, and
> socket tuning — *not* the splice/mux core.

---

## 5. Feasibility: what bore can and cannot change

### 5.1 The hard constraint — the client owns the transport

A stock Tailscale node connects to a DERP region using the **DERP client library**, which
only speaks **DERP-over-TCP/HTTPS** or **DERP-over-WebSocket** — both single-connection, both
TCP. To be a *drop-in* server we must accept exactly that. Therefore:

- ❌ We cannot give one client multiple parallel connections (no bore `--carriers` on this hop).
- ❌ We cannot move the client↔server hop to QUIC/UDP (client won't negotiate it).
- ❌ We cannot remove HOL blocking from the client's single TCP stream.
- ⇒ **bore's mux engine cannot raise single-flow relayed bandwidth on the compatible path.**

### 5.2 What bore *can* legitimately improve (compatible)

1. **Server-side TCP tuning (the real lever).** On high-BDP, low-loss links the limiter is
   buffers + congestion control, not loss. bore can:
   - set Linux **BBR** (`TCP_CONGESTION`) on DERP sockets,
   - raise `SO_SNDBUF`/`SO_RCVBUF` well above defaults,
   - make the per-client send queue **deep and configurable** (stock `derper` is shallow),
     trading a little RAM and a little extra inner-congestion lag for far higher throughput
     on fat pipes.
   This can be a 2–5× improvement **vs stock derper on BDP-limited paths**. It does **not**
   help loss-induced meltdown.
2. **No public-DERP fairness throttle.** Self-hosting on a dedicated box already removes the
   shared-infra contention that caps Tailscale's public DERPs — bore inherits this for free
   (so does a self-hosted official derper).
3. **Mesh hop over bore's transport.** The `ForwardPacket` server↔server path is **ours** to
   design. A bore mesh could carry inter-server traffic over **QUIC datagrams / multi-carrier**
   instead of plain TCP — removing TCP-over-TCP on that hop. Caveat: it only matters when
   peers land on *different* nodes of a multi-node region; for a single self-hosted node it is
   irrelevant.
4. **Operational consolidation.** One static `#![forbid(unsafe_code)]` Rust binary, no Go
   toolchain, coexists with bore's other modes, unified TLS/metrics/logging.

### 5.3 What "doing better" actually requires (incompatible)

The only way to beat TCP-over-TCP is to stop using TCP for the relayed payload — i.e. a
**UDP/QUIC relay**. bore already has the machinery (quinn datagrams, AEAD-opaque relay, nonce
discipline, PMTU monitor — see CLAUDE.md VPN invariants). But a relay that speaks QUIC to the
endpoints is **not DERP** and a **stock Tailscale client will never choose it**. Two paths
exist, both outside "drop-in":

- **(a) Use Tailscale Peer Relays** (already shipped, UDP, e2e-encrypted, ~12.5×). No custom
  server needed — this is the native answer to the user's bandwidth goal.
- **(b) Contribute a QUIC transport to the DERP client** upstream, or run a patched client. A
  bore QUIC-DERP server would then win big — but this is client-side work, not a server swap.

---

## 6. Can we beat TCP-over-TCP? — three honest scenarios

| Scenario | Compatible w/ stock client? | Bandwidth vs stock derper | Effort |
|---|---|---|---|
| **A. bore-DERP, wire-compatible + TCP tuning** | ✅ yes | = derper on lossy links; **2–5× on BDP-limited links** | medium |
| **B. + bore QUIC/multi-carrier mesh between bore-DERP nodes** | ✅ yes (server↔server only) | helps only multi-node regions; client hop unchanged | medium-high |
| **C. bore QUIC-DERP (UDP relay)** | ❌ needs patched/forked client | potentially ~direct-UDP speeds (10×+), beats TCP-over-TCP | high + upstream |

**Bottom line:** within the compatibility constraint, "better than official derper" means
**better-tuned TCP**, not a new transport. The headline 10×+ wins (peer relays) come from
abandoning TCP relay entirely — which only Tailscale's client can opt into.

---

## 7. Proposed architecture for `bore derp` (Scenario A, the buildable one)

New subcommand: `bore derp` (Linux-first; no special privileges, unlike `bore vpn`).

```
bore derp \
  --hostname derps1.0912345.xyz \
  --cert /app/certs/derps1.0912345.xyz.crt \
  --key  /app/certs/derps1.0912345.xyz.key \
  --https-port 443 --http-port 80 \
  --stun --stun-port 3478 \
  [--mesh-with hostB,hostC] [--verify-clients] \
  [--send-queue-depth 256] [--so-sndbuf 8M --so-rcvbuf 8M --congestion bbr]
```

Drop-in mapping to the user's compose (replace image + command only; ports unchanged):

```yaml
services:
  derp:
    image: <bore-image>
    init: true
    restart: always
    ports: [ "80:80", "443:443", "3478:3478/udp" ]
    volumes:
      - /home/ubuntu/certbot/fullchain1.pem:/app/certs/derps1.0912345.xyz.crt
      - /home/ubuntu/certbot/privkey1.pem:/app/certs/derps1.0912345.xyz.key
    command: >
      bore derp --hostname derps1.0912345.xyz
                --cert /app/certs/derps1.0912345.xyz.crt
                --key  /app/certs/derps1.0912345.xyz.key
                --stun --congestion bbr --send-queue-depth 256
```

**Module layout** (`src/derp/`):

- `frame.rs` — codec (type + BE len + payload), `MaxPacketSize`, magic, version.
- `crypto.rs` — NaCl `crypto_box` handshake (server keypair persisted, like derper's key file).
- `http.rs` — minimal HTTP/1.1: route `/derp` (hijack→raw frames), `/derp/probe`,
  `/derp/latency-check`, `/generate_204`; reuse `transport.rs` rustls acceptor on 443.
  (Optional `ws.rs` WebSocket variant if `hyper`+`tungstenite` are added.)
- `hub.rs` — the switch: `DashMap<NodePublic, ClientHandle>`, per-client `sendQueue` +
  `discoSendQueue` (bounded `mpsc`, configurable depth), drop accounting, presence
  (`PeerPresent`/`PeerGone`), `WatchConns`, `ClosePeer`, keepalive (60s), `Health`/`Restarting`.
- `stun.rs` — RFC-5389 binding responder on 3478/udp (adapt `holepunch` STUN code).
- `mesh.rs` *(phase 2)* — `WatchConns` subscription + `ForwardPacket`; optional bore-QUIC
  carrier for the server↔server hop.
- `derpmap.rs` — emit the region JSON for the tailnet admin.

**Reuse:** `transport.rs` (TLS), `shared.rs::tune_tcp` (extended with buffer/congestion knobs),
the `dashmap`/liveness pattern from `pool.rs`, the STUN bits from `holepunch.rs`.

**New deps:** `crypto_box` (pure-Rust NaCl box); optionally `hyper` + `tokio-tungstenite` for
the WebSocket-DERP variant. Keep `#![forbid(unsafe_code)]` — verify each crate complies.

---

## 8. Implementation phases (handoff-grade; CI gate every sub-phase per CLAUDE.md)

> Per CLAUDE.md: tests first/alongside; `cargo fmt` + `cargo clippy -D warnings` +
> `cargo test` green; zero regressions; docs updated each behavior-changing phase. Plan-with-Opus
> / build-with-Sonnet handoff: every sub-phase below is self-contained.

**Phase 0 — Spec lock & test vectors.**
Capture real `derper`↔client handshake bytes (tcpdump against the existing
`sparanoid/derp:edge` box) as golden vectors for the codec + NaCl handshake. Pin exact
constants from §2. *Deliverable:* `derp/SPEC.md` + fixtures.

**Phase 1 — Frame codec + crypto handshake.**
`frame.rs` encode/decode round-trip tests against Phase-0 vectors; `crypto.rs` `ServerKey`/
`ClientInfo`/`ServerInfo` interop test (decrypt a captured `ClientInfo`). No networking yet.

**Phase 2 — Single-node relay over HTTP/TLS.**
`http.rs` `/derp` hijack + `hub.rs` pubkey switch with bounded queues + drop accounting +
keepalive + `/generate_204` + `/derp/probe`. **Interop gate:** two real `tailscale` nodes,
both forced to relay (block direct), exchange traffic through `bore derp`. Compare against
`derper` for correctness.

**Phase 3 — STUN + DERP map.**
RFC-5389 binding responder on 3478/udp (interop: `bore test-udp` and a Tailscale node see
correct reflexive addr). Emit region JSON; document admin wiring.

**Phase 4 — TCP performance tuning (the bandwidth phase).**
`--congestion bbr`, `--so-sndbuf/--so-rcvbuf`, `--send-queue-depth`. Benchmark relayed
`iperf3` through `bore derp` vs `derper` on (a) clean high-RTT (netem 100 ms, 0% loss) and
(b) lossy (1% loss) links. **Record both** — be explicit that (b) won't improve much.
*Deliverable:* `BORE_DERP_BENCH.md` (mirror the existing `vpn_bench.sh` discipline).

**Phase 5 *(optional)* — Mesh + bore-QUIC inter-server hop.**
`WatchConns`/`ForwardPacket`; optional QUIC-datagram carrier between bore-DERP nodes. Interop
with a 2-node region.

**Phase 6 *(optional, research)* — QUIC-DERP variant (incompatible).**
Prototype UDP/QUIC relay reusing bore's `DirectConn`/datagram path; requires a patched client.
Scope as research; do not ship as "DERP".

---

## 9. Risks & open questions

- **Bandwidth expectation management (top risk).** The engine that makes bore special does not
  apply to the client hop. If the project is pitched as "bore makes DERP fast," it will
  underdeliver on lossy links. Frame it as *compatible relay + TCP tuning + consolidation*.
- **HTTP stack choice.** Hand-roll (small, no deps, but must get hijack + WebSocket right) vs
  `hyper` (correct, heavier, gives WebSocket-DERP). Recommend hand-roll for raw `/derp` first,
  add `hyper`/`tungstenite` only if the WebSocket variant is needed by the fleet.
- **NaCl box dependency** must be unsafe-free and interop-exact (xsalsa20-poly1305, not chacha).
- **Protocol drift.** DERP frames are stable but Tailscale evolves (`PeerPresent` flags, mesh).
  Pin to a tested client version; add a CI interop job against a real `tailscale` binary.
- **`-verify-clients`** needs a local `tailscaled` — likely out of scope for a public relay.
- **Deep send queues** trade memory and add a little inner-congestion lag; expose as a knob,
  default conservatively.
- **Mesh key / certmode** parity (`manual` covered; `letsencrypt`/`gcp` are extra surface).

## 10. Recommendation

**Build Scenario A** (`bore derp`, wire-compatible, Phases 0–4) **if** the goals are: drop-in
replacement, single-binary consolidation with bore's other modes, self-hosting without the
public-DERP fairness throttle, and a **modest, honest** bandwidth gain from BBR + buffers +
deep queues on fat low-loss links.

**Do not promise** a TCP-over-TCP breakthrough on the compatible path — it is blocked by the
client transport. If raw relayed bandwidth is the *sole* objective, **first evaluate Tailscale
Peer Relays** (UDP, native, ~12.5×); reserve a bore QUIC-DERP (Phase 6) for the case where you
control/patch the client.

---

### Sources

- DERP source: `github.com/tailscale/tailscale/tree/main/derp` (`derp.go`, `derp_server.go`, `derphttp/`), `cmd/derper/derper.go`.
- [Issue #14791 — poor TCP bandwidth](https://github.com/tailscale/tailscale/issues/14791)
- [Issue #15522 — drop packets to manipulate inner TCP congestion control](https://github.com/tailscale/tailscale/issues/15522)
- [Issue #16028 — weigh metrics beyond latency for relay routes](https://github.com/tailscale/tailscale/issues/16028)
- [Tailscale docs — DERP servers](https://tailscale.com/kb/1232/derp-servers) and [poor performance troubleshooting](https://tailscale.com/docs/reference/troubleshooting/poor-performance-tailnet)
- [Tailscale blog — Peer Relays](https://tailscale.com/blog/peer-relays-international-networks)
- bore internals: `src/server.rs`, `transport.rs`, `mux.rs`, `pool.rs`, `holepunch.rs`, `vpn.rs`, `vpn_server.rs`, `shared.rs`, `Cargo.toml`.
