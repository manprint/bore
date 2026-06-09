# VPN_FULL_PLAN_V1 — `bore vpn` (Linux point-to-point VPN)

> **This document is the single source of truth for the feature.** It is written for an
> implementer who should *not* need to re-derive design rationale. Every non-obvious
> choice is justified inline or in the **Reasoning Appendix (§R)**. When in doubt,
> follow the plan literally; do **not** invent alternatives. If reality contradicts the
> plan (an API differs, a test can't pass), **stop and surface it** — do not paper over
> it (project rule: correctness before cleverness, surface uncertainty explicitly).
> Work **phase by phase, sub-phase by sub-phase**. A sub-phase is *not done* until its
> tests exist and pass — never start the next sub-phase on top of an unverified one.
> Each sub-phase ends with the gate:
> `cargo fmt` → `cargo clippy --all-targets --features vpn -- -D warnings` →
> `cargo test` → `cargo test --features vpn` → `cargo build --no-default-features`
> (note: the `udp` feature is in `default`, so plain `cargo build`/`cargo test`
> already include it; the `--no-default-features` build proves the feature graph
> stays clean). **Zero regressions tolerated.**
>
> **Line numbers in this document were verified against commit `850fbb1`.** If a
> referenced line has drifted, locate the item **by symbol name** (the symbol names
> are authoritative); do not assume the plan is wrong.

---

## §0. Context — why we are building this

`bore` is today a pure **Layer-4 / Layer-7** tool (TCP/UDP stream forwarding + HTTP
vhost). An exhaustive search of `src/` confirms **no Layer-3 code exists**: no
TUN/TAP, no IP-packet handling, no route/forwarding/NAT management.

We are adding a **Linux-only, point-to-point VPN**: bring up a virtual network
interface (`tun`) on two machines and route real IP traffic between them — and between
the LANs behind them — over bore's existing brokered, NAT-traversing transport.
Requirements: "works great, maximum performance," respecting all project invariants
(`#![forbid(unsafe_code)]`, existing paths byte-for-byte unchanged, zero regressions,
docs+tests are part of the deliverable).

**The load-bearing mental model:** *a VPN link is structurally a secret tunnel that
carries IP packets instead of a TCP byte-stream.* Two peers rendezvous through the
server, hole-punch a direct QUIC path, and fall back to a server-relayed yamux
substream. The only genuinely new pieces are: (a) a `tun` device at each end, (b)
moving **IP packets** instead of TCP streams over the transport, and (c) auto-managing
the host's routes/forwarding/NAT around the interface. **~70% of the machinery already
exists** and is reused.

### §0.1 Decisions locked with the user (do **not** relitigate — see §R for the "why")

1. **Topology = point-to-point** (exactly 2 endpoints per link). `host↔host`,
   `site↔host`, `site↔site` are all 2-party. Multi-peer mesh = out of scope (→ §V2).
2. **Addressing = both** server-pool *and* static. Static selected by `--vpn-addr`;
   else server allocates from `--vpn-pool`.
3. **Routing = full auto-manage**, reverted on exit (graceful, signal, panic, and
   stale-reclaim at startup). Requires root / `CAP_NET_ADMIN`.
4. **Overlapping LAN subnets = unsupported in v1** → detect + refuse (`VpnError`).
5. **Relay fallback = E2E always** → clients AEAD-seal each IP packet; the server only
   ever byte-splices ciphertext. Direct path is already E2E (QUIC-TLS).
6. **Performance = batched I/O** target (`IFF_VNET_HDR` GSO/GRO), landed via a
   correctness-first → offload-on sub-sequence inside the data-plane phase (§Phase 6).
7. **IPv4 only** in v1 (IPv6 → §V2).

### §0.2 The transport decision (most important — full reasoning in §R.1)

Two data-plane encodings behind ONE `VpnLink` abstraction:

| Path   | Encoding | Encryption | Reliability | Rationale |
|--------|----------|------------|-------------|-----------|
| **Direct** | one **QUIC unreliable DATAGRAM** per IP packet | QUIC-TLS 1.3 (E2E) | unreliable (correct for IP) | No reliable-over-reliable meltdown; quinn already exposes `max_datagram_size()`. |
| **Relay** | `[u32 len][u64 ctr][AEAD ciphertext+tag]` framed over **one yamux substream** | `ring::aead` ChaCha20-Poly1305 (E2E) | reliable (TCP) — acceptable for a *fallback* | Server splices opaque bytes exactly like a secret tunnel → sees only ciphertext. |

Direct is always preferred; relay is the automatic fallback (reuse the exact trigger
from `secret.rs`). Interface MTU is clamped (default 1350) so a live path switch never
breaks PMTU and a segmented packet always fits one datagram.

---

## §1. Hard invariants you must not break (from `CLAUDE.md`)

1. **Client sends its first control message before auth.** `HelloVpn` / `ConnectVpn`
   go out before the auth handshake (yamux is lazy; skipping deadlocks). Mirror
   `client.rs:167-176` (sends `Hello` first, then `client_handshake` if secret).
2. **`mux::STREAM_READY` written before any splice** on relay substreams. Mirror
   `secret.rs:505-523` (`relay`).
3. **`shared::tune_tcp`** (`shared.rs:150-156`) on every new TCP socket (control,
   carriers, relay). Reuse existing call sites. (yamux substreams are not sockets —
   nothing to tune there; the underlying carrier TCP conns are already tuned.)
4. **`--max-conns`/semaphore is the real bound.** Add a server cap `--vpn-max-links`
   enforced like the existing connection semaphore.
5. **`carriers<=1` keeps the single-connection path unchanged.** VPN v1 **hard-codes
   one relay substream** and does **not** expose `--carriers` on the `vpn` subcommand
   (round-robin per *packet* would reorder; per-connection round-robin doesn't apply
   to an IP link). The `carriers` protocol field exists (`#[serde(default)]`) but is
   always `1` in v1 → §V2.
6. **`#![forbid(unsafe_code)]` stays.** All `unsafe` lives inside the TUN crate.
7. **Existing behavior byte-for-byte unchanged.** VPN is purely additive: new
   subcommand, new message variants (`#[serde(default)]`), a new feature-gated module.
   Touch **no** existing data path. `cargo test` (no features) must be unchanged.

---

## §2. Dependencies & feature gating (exact)

`Cargo.toml`:
```toml
[features]
# existing: udp = ["dep:quinn", "dep:rcgen", ...]
vpn = ["udp", "dep:tun-rs"]      # VPN needs the QUIC direct path

[target.'cfg(target_os = "linux")'.dependencies]
tun-rs = { version = "<pin latest>", optional = true, features = ["async_tokio"] }
# Implementer: confirm the exact feature name for tokio async + the offload/GSO feature
# via `cargo doc -p tun-rs --open` or context7. Pin an exact version. tun-rs encapsulates
# the ioctl unsafe internally → bore stays #![forbid(unsafe_code)].
```
- **Crypto: NO new dep.** Reuse `ring` (already 0.17): `ring::aead` (ChaCha20-Poly1305),
  `ring::hkdf`. Note: the existing `holepunch::derive_token` (`holepunch.rs:84-91`) is
  **HMAC-SHA256, not HKDF** — use it only as a *style* template (how the codebase mixes
  secret+nonce); the VPN keys use real `ring::hkdf` as specified in §6.2.
- **Routes/forwarding/NAT: NO new dep.** Shell out via `tokio::process::Command` to
  `ip` (iproute2) and `nft` (with `iptables` fallback), and read/write
  `/proc/sys/net/ipv4/ip_forward`. This is **control-plane only** (once per link
  up/down) → **zero data-path cost** → does not conflict with the perf goal, and keeps
  the dependency surface minimal (no `rtnetlink` stack). See §R.4.
- **All VPN code** is gated `#[cfg(all(feature = "vpn", target_os = "linux"))]`. The
  `Vpn` clap variant + its dispatch arm are gated the same way (so a no-feature build
  has no `vpn` subcommand; document that VPN requires `--features vpn`). Provide no
  non-Linux stub — the subcommand simply does not exist there.
  **EXCEPTION — protocol types are NOT feature-gated:** the `shared.rs` additions
  (`Ipv4Net`, `VpnAddrRequest`, the four message variants in §5) compile in **every**
  build. They are pure data with no extra deps. Rationale: a server built without
  `vpn` (or running without `--vpn`) must be able to *parse* `HelloVpn`/`ConnectVpn`
  and answer `VpnError("vpn not supported/enabled on this server")` instead of dying
  on an unknown-variant deserialization error (the protocol enums have no
  `#[serde(other)]` — an unparseable variant kills the connection). Only servers
  *older than this release* will still hard-close; the client must therefore map a
  connection-closed-right-after-`HelloVpn` into an actionable hint
  ("server may be too old / not VPN-capable") — see Phase 7.
- **`--secret` is mandatory for `bore vpn`** (both sides) — see §R.2: relay E2E key =
  `HKDF(secret, server_nonce)`; without a secret the server (which issues the nonce)
  could derive the key. Enforce in CLI validation with a clear error.

---

## §3. Architecture reference map — existing code you reuse

| Area | File:symbol | How you use it |
|------|-------------|----------------|
| Protocol | `shared.rs:493-596` `ClientMessage`, `shared.rs:601-706` `ServerMessage` | add VPN variants (§5) |
| Frame codec | `shared.rs:872-890` `Delimited` (null-delimited JSON) | control messages |
| TCP tuning | `shared.rs:150-156` `tune_tcp` | every new socket |
| Constants | `shared.rs` `CONTROL_PORT`,`NETWORK_TIMEOUT`,`DEFAULT_PROXY_BUFFER_SIZE`,`UDP_NONCE_LEN`, `UdpDirectTuning` (372-385) | reuse |
| Direct path | `holepunch.rs:990-1039` `DirectConn` (wraps `quinn::Connection`; already has `closed()`, `max_datagram_size()`) | add datagram methods (§Phase 3) |
| Direct connect | `holepunch.rs:1079-1115` `connect_direct` | consumer side punch+auth |
| Transport tuning | `holepunch.rs:1409-1430` `transport_config` (already sets **BBR** congestion control + stream/conn/send windows — directly serves the max-bandwidth goal; do not remove) | add datagram buffers (§Phase 3) |
| Token derive | `holepunch.rs:84-91` `derive_token` (HMAC-SHA256) | *style* template only; VPN uses `ring::hkdf` |
| Candidate/STUN | `holepunch.rs` `bind_socket`,`gather_candidates`,`PUBLIC_STUN` | reuse unchanged |
| Broker shape | `secret.rs:184` `serve_provider`, `secret.rs:335` `serve_consumer` | clone for VPN |
| Relay splice | `secret.rs:505-523` `relay` (STREAM_READY then `copy_bidirectional_with_sizes`) | **reuse verbatim** (server sees ciphertext) |
| Registry | `server.rs` `Registry`,`UdpRegistry`,`pending_carriers`,`Server` fields | add `vpn_providers` |
| Carrier pool | `pool.rs` `CarrierPool`,`Carrier`,`TokenGuard` | RAII lease pattern + relay carriers |
| Client scaffold | `client.rs` `new_secret_provider` + connect/serve loop | control connection |
| TLS/endpoint | `transport.rs` `Endpoint::parse`,`connect`,`ControlStream` | reuse |
| CLI pattern | `main.rs:599-811` `TransferCommand::{Listener,Sender}` | mirror for `Vpn{Listen|Connect}` |
| Dispatch | `server.rs:714-807` first-message match on `ClientMessage` | add `HelloVpn`/`ConnectVpn` arms |
| Reconnect | `reconnect.rs` `reconnect::run` | `--auto-reconnect` |
| Auth | `auth.rs` `Authenticator` | reuse unchanged |
| Admin | `admin.rs` register entry | show the link on status page |

---

## §4. New module layout — `src/vpn.rs` (skeleton)

All items `#[cfg(all(feature = "vpn", target_os = "linux"))]`. Keep **pure logic**
(testable without root) separate from **I/O**. Suggested internal structure:

```
src/vpn.rs
  // ---- pure (unit-tested in Phase 1, no root) ----
  mod net          // Ipv4Net newtype, overlap detection, /30 pool allocator, MTU calc
  mod crypto       // HKDF key derivation, AEAD seal/open, per-direction nonce/counter
  mod hostcfg_cmd  // pure: build the exact argv vectors for ip/nft/sysctl (+ rollback)
  // ---- I/O (integration / netns tested) ----
  mod hostcfg      // NetConfig RAII guard: runs argv from hostcfg_cmd, reverts on Drop
  mod link         // VpnLink enum (Direct datagrams | Relay AEAD frames), batch send/recv
  mod bridge       // tun<->VpnLink uplink/downlink tasks (the perf core)
  // ---- entry points (called from main.rs dispatch) ----
  pub async fn run_listen(args: VpnListenArgs) -> Result<()>
  pub async fn run_connect(args: VpnConnectArgs) -> Result<()>
```
Server-side VPN code (registry, pairing, pool) lives in `server.rs` + a small
`vpn_server` submodule there (or `secret.rs` alongside the cloned broker), to keep it
next to the existing registries.

---

## §5. Protocol additions (`shared.rs`) — exact

Add a small newtype (no new dep):
```rust
/// An IPv4 CIDR (address + prefix length). Used for overlay + advertised subnets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ipv4Net { pub addr: std::net::Ipv4Addr, pub prefix: u8 }
// impl FromStr ("a.b.c.d/n"), Display, and a `contains`/`overlaps` helper (pure, tested).

/// How a side wants its overlay address assigned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VpnAddrRequest {
    Pool,                                   // server allocates a /30
    Static { addr: std::net::Ipv4Addr, prefix: u8, peer: std::net::Ipv4Addr },
}
```
Add to `ClientMessage` (all new struct fields `#[serde(default)]` where additive):
```rust
HelloVpn {                       // the "listener" registers a link id (provider role)
    id: String,
    advertised: Vec<Ipv4Net>,    // CIDRs this side exposes (empty = host-only)
    addr: VpnAddrRequest,
    notes: Option<String>,
    #[serde(default)] carriers: u16,
},
ConnectVpn {                     // the "connector" dials a link id (consumer role)
    id: String,
    advertised: Vec<Ipv4Net>,
    addr: VpnAddrRequest,
    notes: Option<String>,
},
```
Add to `ServerMessage`:
```rust
VpnReady {
    assigned: std::net::Ipv4Addr,        // this side's overlay address
    prefix: u8,                          // overlay link prefix (30)
    peer_overlay: std::net::Ipv4Addr,    // other end's overlay address
    peer_advertised: Vec<Ipv4Net>,       // install routes toward these
    session_nonce: [u8; UDP_NONCE_LEN],  // seeds AEAD key + reuses direct-path token derive
    #[serde(default)] tuning: UdpDirectTuning,
},
VpnError(String),                        // overlap, pool exhausted, dup id, no-secret, ...
```
The existing `UdpCandidateOffer` / `UdpPunch` / `UdpUnavailable` flow is reused
**unchanged** for the direct-path rendezvous — no new UDP-signaling messages.

**Addressing validation rules (server-side, authoritative — do not improvise):**
1. **Both sides must use the same mode.** `Pool`+`Pool` or `Static`+`Static`;
   a mixed pair → `VpnError("addressing mode mismatch: listener=<mode> connector=<mode>")`.
2. **Static pairs must be mirror-consistent:** `listener.addr == connector.peer`,
   `connector.addr == listener.peer`, both prefixes equal, both addrs inside the same
   network, addrs distinct. Any violation → `VpnError` naming the inconsistency.
3. **Static addrs must not collide** with any live pool lease or live static link.
4. `Pool` requires the server to have `--vpn-pool`; otherwise
   `VpnError("server has no vpn pool; use --vpn-addr/--vpn-peer-addr")`.

---

## §6. Transport deep dive (exact patterns)

### §6.1 Direct path — QUIC datagrams (`holepunch.rs`)
- In `transport_config` (line 1307) add:
  ```rust
  cfg.datagram_receive_buffer_size(Some(8 * 1024 * 1024)); // absorb RX bursts
  cfg.datagram_send_buffer_size(8 * 1024 * 1024);          // absorb TX bursts
  ```
  (Confirm exact method names/signatures against the pinned quinn 0.11 — `cargo doc`.)
- Add methods to `DirectConn`:
  ```rust
  pub fn send_datagram(&self, pkt: bytes::Bytes) -> Result<()> {
      self.conn.send_datagram(pkt).map_err(|e| anyhow!("send_datagram: {e}"))
  }
  pub async fn read_datagram(&self) -> Result<bytes::Bytes> {
      self.conn.read_datagram().await.context("read_datagram")
  }
  ```
  Note: `send_datagram` errors with `TooLarge` if the packet exceeds the current
  datagram limit — we clamp the tun MTU so this should never fire in steady state; if
  it does, drop + `warn!` once (count it). `read_datagram` resolves `Err` when the
  connection closes → use that to detect path death (alongside `DirectConn::closed()`).

  **Known transient — implement exactly this, do not redesign:** quinn starts at its
  initial path MTU (default 1200) and raises it via built-in MTU discovery, usually
  within the first round-trips. So right after connect, `max_datagram_size()` can be
  **below** the tun MTU (1350) and full-size packets get dropped (`TooLarge`) for a
  brief window. This is acceptable: small packets (ping, TCP handshake) pass, and TCP
  retransmits the few lost full-size segments. Required behavior:
  - On connect: `debug!(max_datagram = ?...)` (already specified in Phase 3).
  - In the bridge: count `TooLarge` drops; if drops are still occurring **>10 s after
    link-up**, `warn!` once suggesting a lower `--mtu` (path MTU likely < 1350+overhead).
  - Do **not** change `initial_mtu`/`min_mtu` in the shared `transport_config` — it is
    shared with the existing secret-tunnel QUIC path (regression risk).

  **Send-buffer backpressure:** verify against pinned quinn 0.11 what `send_datagram`
  does when the datagram send buffer is full (older quinn silently drops the *oldest*
  queued datagram; 0.11 also offers `send_datagram_wait`). For v1 use the
  non-blocking `send_datagram` and accept drops — IP is best-effort and the inner
  TCP/QUIC adapts; blocking the uplink task would stall the tun read loop. Count what
  you can observe; record the verified behavior in a code comment.

  **RX drain pattern (recv_batch on Direct) — required, not optional:**
  `read_datagram()` yields **one** datagram per await; a naive loop makes the downlink
  call-bound and caps bandwidth. Implement `recv_batch` as: `await` the first datagram,
  then opportunistically drain already-queued datagrams **without yielding** (e.g.
  `futures_util::FutureExt::now_or_never()` on subsequent `read_datagram()` calls, up
  to a batch cap of 64) and return the whole batch so the tun side can use multi-packet
  write (Phase 6.2).

### §6.2 Relay path — AEAD framing (`vpn::crypto` + `vpn::link`)
- Key derivation (pure, tested): `HKDF-SHA256` with `salt = session_nonce`,
  `ikm = secret.as_bytes()`, expand two 32-byte keys with distinct `info` labels so the
  two directions never share a key:
  - listener→connector: `info = b"bore-vpn l2c v1"`
  - connector→listener: `info = b"bore-vpn c2l v1"`
  Each side uses one key to seal (its egress) and the other to open (its ingress).
- AEAD: `ring::aead::ChaCha20_POLY1305`, wrapped in `ring::aead::LessSafeKey` so we
  control the nonce explicitly. Per-packet 96-bit nonce = `4 zero bytes ‖ u64 counter
  (BE)`. Counter is a per-direction monotonic `u64` starting at 0. (64-bit won't wrap
  in practice; on the impossible wrap, tear the link with an `error!`.)
- Wire frame (the **explicit** counter makes it robust + future-proof for replay
  windows): `[u32 BE total_len][u64 BE counter][ciphertext ‖ 16-byte tag]`.
  AAD = empty in v1 (note in §V2). Receiver reads len, reads counter, rebuilds the
  nonce, `open_in_place`. On `open` failure → `error!` + tear link (tampered/desync).
- The relay substream is opened/accepted exactly like a secret-tunnel data substream
  (write `mux::STREAM_READY` first); the **server runs `secret::relay` verbatim** and
  never sees plaintext.

### §6.3 `VpnLink` abstraction (`vpn::link`)
```rust
enum VpnLink {
    Direct(DirectConn),                         // datagrams
    Relay { send: RelayHalf, recv: RelayHalf, keys: DirectionKeys }, // AEAD frames over substream
}
impl VpnLink {
    // Batched API so the bridge is not call-bound. v1 backs these with the
    // appropriate per-packet primitive; offload batching is wired in Phase 6.2.
    async fn send_batch(&self, pkts: &[Bytes]) -> Result<()>;
    async fn recv_batch(&self, out: &mut Vec<Bytes>) -> Result<()>; // fills out, returns >=1 or Err on close
    async fn closed(&self);                     // resolves when this path dies
}
```

---

## §7. Host network config — `NetConfig` RAII guard (`vpn::hostcfg`)

**Two layers:** `hostcfg_cmd` (pure: builds argv vectors, fully unit-tested) and
`hostcfg` (runs them via `tokio::process::Command`, records what ran, reverts on
`Drop`). Apply order and exact commands (substitute `<...>`):

1. **Preflight (before mutating anything):** verify `geteuid()==0` *or* probe
   `CAP_NET_ADMIN` (attempt is fine, but prefer an explicit check); verify `ip` exists,
   and `nft` (else `iptables`). On failure → actionable `bail!` naming what's missing.
2. **Stale reclaim:** delete any leftover `bore_vpn_<id>` nft table and a leftover tun
   of the same name from a prior crash (idempotent, ignore "not found").
3. **Interface:** prefer the tun crate's builder for addr/up/mtu; otherwise
   `ip addr add <assigned>/<prefix> dev <tun>`; `ip link set <tun> mtu <mtu> up`.
4. **Routes** (per peer-advertised subnet): `ip route add <subnet> dev <tun>`.
   (The overlay /30 link route is implicit from the addr.) Record each for `ip route del`.
5. **Gateway side only (we advertise ≥1 subnet):**
   - Save `/proc/sys/net/ipv4/ip_forward`, write `1`.
   - Determine LAN egress iface: `ip route get <a host inside our advertised subnet>` →
     parse `dev <lan_if>`.
   - nft (preferred), one **dedicated table** so teardown is atomic:
     ```
     nft add table inet bore_vpn_<id>
     nft 'add chain inet bore_vpn_<id> post { type nat hook postrouting priority 100 ; }'
     nft 'add rule  inet bore_vpn_<id> post iif <tun> oif <lan_if> masquerade'
     nft 'add chain inet bore_vpn_<id> fwd  { type filter hook forward priority -10 ; }'
     nft 'add rule  inet bore_vpn_<id> fwd  tcp flags syn tcp option maxseg size set rt mtu'  # MSS clamp
     ```
     Teardown = `nft delete table inet bore_vpn_<id>` (single op).
   - iptables fallback (tag every rule with a unique `--comment bore_vpn_<id>`):
     `iptables -t nat -A POSTROUTING -i <tun> -o <lan_if> -j MASQUERADE -m comment --comment bore_vpn_<id>`
     + a `mangle FORWARD` `TCPMSS --clamp-mss-to-pmtu` rule; teardown deletes by comment.
6. **`--no-route-manage`:** skip execution; **print** every argv from `hostcfg_cmd` so
   the operator runs them. (Still set up the interface itself — only routes/NAT are
   deferred. Document this distinction.)

**Cleanup robustness:** `Drop` reverts in reverse order, **best-effort** (log on
failure, never panic). Also install a `tokio::signal` handler (SIGINT/SIGTERM) that
triggers graceful shutdown so `Drop` runs. Document that a `SIGKILL` cannot clean up;
the Phase-0.2 stale-reclaim handles the next start. Log each mutation at `info` on
apply and each undo at `info` on revert.

---

## §8. Data plane — the tun↔transport bridge (`vpn::bridge`, perf core)

Two tasks per link, sharing the `tun` device (split into read/write halves) and the
`VpnLink`:

- **uplink** (tun → peer): read packet(s) from tun → for each, `link.send_batch`.
- **downlink** (peer → tun): `link.recv_batch` → write packet(s) to tun.

`tokio::select!` over `uplink`, `downlink`, and `link.closed()`. On `closed()` → return
a sentinel so the caller can attempt relay fallback / reconnect. On tun read error →
fatal.

**MTU rule:** interface MTU default **1350** (overridable by `--mtu`). Once a direct
link is up, if `max_datagram_size()` reports **less** than the MTU, `warn!` once; any
egress packet larger than the datagram limit is dropped+counted (the gateway MSS clamp
keeps forwarded TCP healthy, and host-originated traffic obeys the interface MTU).

### §8.1 Phase-6 implementation sub-sequence (honors "batched from start")
- **6.1 Correctness first:** wire `send_batch`/`recv_batch` to **single-packet** tun
  `recv`/`send` (loop). Get `ping` working host↔host and site↔host (netns). This proves
  the whole stack end-to-end with minimal moving parts.
- **6.2 Offload on:** enable `IFF_VNET_HDR` + GSO/GRO via the tun crate's offload API;
  switch the bridge to the crate's multi-packet read (one syscall → a GSO super-buffer +
  `virtio_net_hdr`) and segment to ≤datagram size before send; coalesce RX and use the
  crate's multi-packet write (GRO). **Verify the exact offload API** (`cargo doc` /
  context7); if the crate's offload proves unworkable, **stop and surface it** — fall
  back to 6.1 single-packet for v1 and move full offload to §V2 (record the decision).
  Re-run all Phase-6 tests + an `iperf3` throughput check.

**Never** log per-packet in the data plane. Maintain atomic counters (tx/rx
packets+bytes, drops, current path) and emit a periodic (e.g. 10 s) `debug!` summary.

---

## §9. CLI (`main.rs`) — exact

Mirror the `Transfer` nested subcommand:
```rust
/// Linux point-to-point VPN (requires --features vpn; needs root / CAP_NET_ADMIN).
Vpn {
    #[command(subcommand)]
    command: VpnCommand,
},

enum VpnCommand {
    /// Register a VPN link id and wait for a connector.
    Listen(VpnListenArgs),
    /// Dial a VPN link id.
    Connect(VpnConnectArgs),
}
```
Shared client args (reuse names + `BORE_*` env exactly as `proxy`/`local` do):
`--to`(`BORE_SERVER`), `--secret`(`BORE_SECRET`, **required**), `--id`(`BORE_VPN_ID`),
`--insecure`, `--auto-reconnect`, and the reused NAT set
(`--stun-server`,`--upnp`,`--try-port-prediction`,`--nat-udp-preferred-port`,
`--nat-udp-release-timeout`). **No `--carriers` on `vpn`** (invariant 5: v1 is one
relay substream; the protocol field is sent as `1`).
VPN-specific:
- `--advertise <CIDR[,CIDR...]>` (`BORE_VPN_ADVERTISE`) — presence ⇒ gateway mode.
- `--vpn-addr <ip/prefix>` (`BORE_VPN_ADDR`) — omit ⇒ server pool.
- `--vpn-peer-addr <ip>` (static mode only).
- `--tun-name <name>` (default `bore0`), `--mtu <n>` (default 1350).
- `--no-route-manage`.
Server flags (on the existing `Server` variant): `--vpn`(`BORE_VPN`),
`--vpn-pool <CIDR>`(`BORE_VPN_POOL`), `--vpn-max-links <N>`(`BORE_VPN_MAX_LINKS`).
Topology is **implicit** from `--advertise` (no `--mode` flag): neither advertises =
host↔host; one = site↔host; both = site↔site.

Dispatch arm (feature+linux gated): build args struct → `reconnect::run(auto_reconnect,
connect_closure, vpn::run_listen|run_connect)`.

---

## §10. Server — registry, pairing, pool (`server.rs` + `vpn_server`)

- `Server` gains `vpn_providers: Arc<DashMap<String, VpnEntry>>` and an optional
  `VpnPool` (present iff `--vpn-pool`).
- `VpnEntry { control handle / sender, advertised: Vec<Ipv4Net>, carrier pool, lease: VpnLeaseGuard }`.
- `VpnPool` (pure-ish, lease state is I/O-free): from `--vpn-pool` CIDR carve **/30**
  blocks; allocate the next free block per link, assign `.1`→listener, `.2`→connector;
  free on `VpnLeaseGuard` drop (RAII, mirror `TokenGuard`). Track allocated blocks in a
  `HashSet<u32>` (network address as key). Allocation/free/exhaustion are unit-tested.
- **Pairing** (in the connector's serve fn):
  0. If the server runs without `--vpn` (or was built without the feature — §2
     exception), answer any `HelloVpn`/`ConnectVpn` with
     `VpnError("vpn not supported/enabled on this server")` and close.
  1. `HelloVpn` registers the id in `vpn_providers` (reject duplicate id → `VpnError`).
  2. `ConnectVpn` looks up the id; if absent, `VpnError("no such vpn link")`.
  3. **Overlap check (authoritative, pure fn, tested):** reject if any pair among
     `{listener.advertised, connector.advertised, the overlay /30}` overlaps →
     `VpnError("overlapping subnets: ...")`.
  4. Addressing: apply the §5 validation rules (mode match, static mirror-consistency,
     collision, pool presence); pool mode allocates a /30 → build each side's
     `VpnReady`.
  5. Send `VpnReady` to **both** sides.
  6. Broker the UDP punch (reuse the existing broker path) and, on failure, set up the
     relay substream (reuse `secret::relay`).
- Bound live links with a semaphore from `--vpn-max-links` (invariant 4).
- `serve_vpn_listener`/`serve_vpn_connector` clone the **shape** of
  `secret::serve_provider`/`serve_consumer` (heartbeat loop, carrier handling, teardown
  via RAII). Keep them additive — do not modify the secret-tunnel functions.

---

## §11. Phased implementation

> Gate after **every** sub-phase: `cargo fmt` → `cargo clippy --all-targets --features
> vpn -- -D warnings` → `cargo test` → `cargo test --features vpn`. Plus the no-feature
> `cargo build` must stay clean (proves additivity). Zero regressions.

### Phase 0 — Scaffolding, feature, dependency
- **0.1** `Cargo.toml`: add `vpn` feature + `tun-rs` (target-gated, optional). Pin
  versions. Confirm offload feature name.
- **0.2** Create `src/vpn.rs` with the §4 module skeleton (empty fns returning
  `bail!("not yet implemented")`), declare in `lib.rs`, all `#[cfg(all(feature="vpn",
  target_os="linux"))]`.
- **Logging:** none yet.
- **Tests:** `cargo build` (default) unchanged; `cargo build --features vpn` compiles.
- **Docs:** add a one-line "VPN (experimental, Linux)" stub to README so the feature is
  discoverable.
- **Acceptance:** both builds green; no clippy warnings.

### Phase 1 — Pure logic, no I/O, no root (`vpn::net`,`vpn::crypto`,`vpn::hostcfg_cmd`)
- **1.1** `Ipv4Net` (FromStr/Display/`contains`/`overlaps`), `/30` pool allocator
  (alloc/free/exhaustion), MTU calc.
- **1.2** `crypto`: HKDF derivation (two keys), AEAD seal/open with explicit counter,
  nonce builder.
- **1.3** `hostcfg_cmd`: pure builders returning `Vec<Vec<String>>` (argv) for
  apply+revert of addr/route/sysctl/nft/iptables; LAN-iface parse from `ip route get`
  output (feed it a captured sample string).
- **Logging:** n/a (pure).
- **Tests (name them):**
  `overlap_truth_table` (adjacent/contained/disjoint/equal; overlay vs advertised);
  `pool_alloc_assigns_dot1_dot2`, `pool_free_reuses_block`, `pool_exhaustion_errors`;
  `aead_roundtrip_ok`, `aead_tamper_fails`, `aead_wrong_key_fails`,
  `nonce_monotonic_unique`; `hkdf_deterministic`, `hkdf_directions_differ`;
  `cmd_route_add_snapshot`, `cmd_nft_table_snapshot`, `cmd_iptables_fallback_snapshot`,
  `parse_lan_iface_from_ip_route_get`.
- **Docs:** rustdoc on every pure fn (project style: explain the "why").
- **Acceptance:** 100% of pure logic covered, runs as non-root.

### Phase 2 — Protocol messages (`shared.rs`)
- **2.1** Add `Ipv4Net`, `VpnAddrRequest`, the `HelloVpn`/`ConnectVpn`/`VpnReady`/
  `VpnError` variants (§5), `#[serde(default)]` discipline. **NOT feature-gated**
  (§2 exception) — they must compile in the default and `--no-default-features` builds.
- **Tests:** `serde_roundtrip_vpn_messages`; `forward_compat_unknown_fields_default`
  (deserialize a JSON missing the new fields).
- **Docs:** doc-comment each variant (who sends it, when, preconditions).
- **Acceptance:** gates green; existing protocol tests unchanged.

### Phase 3 — QUIC datagrams (`holepunch.rs`)
- **3.1** Enable datagram buffers in `transport_config`. **Caution:** this function is
  shared with the existing secret-tunnel QUIC path — *add* the two datagram-buffer
  calls, change nothing else (BBR + windows stay exactly as they are).
- **3.2** Add `DirectConn::send_datagram`/`read_datagram` (+ size guard) (§6.1).
- **Logging:** `debug!(max_datagram = ?conn.max_datagram_size())` once on connect.
- **Tests:** `quic_datagram_loopback_echo` (two in-proc endpoints, datagram round-trip);
  `datagram_too_large_is_error`. **Existing holepunch/stream tests stay green.**
- **Acceptance:** gates green incl. `--features udp` and `--features vpn`.

### Phase 4 — Server registry, pairing, addressing, relay (`server.rs`/`vpn_server`)
- **4.1** `vpn_providers`, `VpnPool`, `VpnLeaseGuard`, `--vpn`/`--vpn-pool`/
  `--vpn-max-links`.
- **4.2** `serve_vpn_listener`/`serve_vpn_connector` (clone secret shape), overlap
  check, `VpnReady`, reuse `secret::relay`, dispatch wiring (route by first message).
- **Logging:** `info!(link_id, role, assigned, peer_overlay, "vpn link paired")`;
  `warn!` on duplicate id / overlap / pool exhaustion (with the offending CIDRs);
  `info!` on teardown + lease free.
- **Tests (integration, in-process, no TUN):**
  `vpn_pair_assigns_pool_addrs`, `vpn_duplicate_id_rejected`,
  `vpn_overlap_rejected`, `vpn_pool_exhaustion_rejected`,
  `vpn_static_addr_collision_rejected`,
  `vpn_addr_mode_mismatch_rejected` (Pool vs Static → `VpnError`),
  `vpn_static_inconsistent_pair_rejected` (mirror-consistency rules of §5),
  `vpn_disabled_server_rejects` (server without `--vpn` answers `VpnError`, does not
  kill the connection unparsed),
  `vpn_relay_substream_is_opaque` (push AEAD frames through the relay; assert the bytes
  the server splices contain **no** plaintext IP header — i.e. server can't read).
- **Docs:** server-side VPN section notes in `docs/VPN.md` draft.
- **Acceptance:** gates green; no change to secret-tunnel tests.

### Phase 5 — TUN + `NetConfig` (root-gated) (`vpn::hostcfg`)
- **5.1** TUN bring-up (name/addr/mtu/up), preflight cap/binary checks, stale reclaim.
- **5.2** `NetConfig` apply/revert running argv from `hostcfg_cmd`; signal handler;
  `--no-route-manage` printing.
- **Logging:** `info!` each mutation on apply and each undo on revert; `warn!` on
  best-effort revert failures; `error!` + actionable message on missing caps/binaries.
- **Tests:** pure parts already covered in Phase 1; the I/O parts are exercised by the
  netns harness (§13). Mark any test needing root `#[ignore]` with a comment on how to
  run. Add a `netconfig_rollback_is_reverse_order` test against a fake runner that
  records calls (inject the command-runner as a trait so it's unit-testable without
  root).
- **Acceptance:** gates green; netns bring-up/tear-down leaves the host pristine
  (asserted in §13).

### Phase 6 — Data-plane bridge (`vpn::bridge`, `vpn::link`)
- **6.1** `VpnLink` (Direct datagrams + Relay AEAD frames) backed by single-packet tun
  I/O; uplink/downlink tasks; `closed()` handling; relay fallback + reconnect (reuse
  secret-tunnel renegotiation). Get `ping` working in netns.
- **6.2** Enable offload (GSO/GRO) + multi-packet read/write + segmentation/coalescing
  (verify crate API; fallback rule in §8.1).
- **Logging:** path selection (`info!(path="direct"|"relay")`), fallback transitions
  (`warn!`), periodic counter summary (`debug!`), never per-packet at info/debug.
- **Tests:** `segment_gso_buffer` / `coalesce_for_gro` (synthetic buffers, pure);
  `recv_batch_drains_queued_datagrams` (in-proc QUIC pair: queue N datagrams, one
  `recv_batch` call returns >1 — proves the §6.1 drain pattern);
  netns end-to-end: `ping` host↔host; `ping` through an advertised subnet (site↔host);
  kill UDP between peers → assert relay fallback → re-`ping`; `iperf3` throughput sanity
  (assert non-trivial Mbps to catch a syscall-bound regression); large-payload check
  shortly after link-up (e.g. `ping -s 1300`) eventually succeeds once MTU discovery
  settles (§6.1 transient).
- **Acceptance:** all of the above pass; gates green.

### Phase 7 — CLI, reconnect, admin, env (`main.rs`,`admin.rs`)
- **7.1** `Vpn{Listen|Connect}` + dispatch (feature+linux gated) + `--secret` required
  validation + static-vs-pool selection logic + topology-from-`--advertise`.
  Error UX: a `VpnError` from the server is printed verbatim and exits non-zero; a
  connection that closes/errors right after sending `HelloVpn`/`ConnectVpn` (before
  any `ServerMessage`) prints the hint "server may be too old or not VPN-capable
  (needs bore server --vpn, built with --features vpn)" (§2 exception).
- **7.2** `reconnect::run` wrapping; register the link in `admin` (role, id, peer,
  overlay, path).
- **Logging:** `info!` on link up/down with `link_id`, `path`, `overlay`, `iface`.
- **Tests:** `cli_vpn_help_renders`, `cli_vpn_requires_secret`,
  `cli_vpn_static_requires_peer_addr`, `cli_vpn_parses_advertise_list`; a reconnect
  loop smoke test (server drop → client retries with backoff).
- **Acceptance:** gates green.

### Phase 8 — Documentation & test matrix
- **8.1** `README.md` VPN section (3 topologies, copy-paste commands, root requirement,
  `--features vpn` build note). Source of truth for commands + expected behavior: §16.
- **8.2** `docs/VPN.md`: concept; the 3 topologies fully worked; privilege/firewall/UDP
  notes; **overlap limitation**; **IPv4-only**; MTU notes; security model (direct
  QUIC-TLS E2E + relay AEAD E2E, server-sees-ciphertext); troubleshooting via
  `bore test-udp`.
- **8.3** `docs/VPN_TEST_MATRIX.md` + the netns script under `scripts/`. The matrix
  **must** include the §16 traceability table (every §16 bullet → covering test) and
  the manual procedures for the few bullets that cannot be automated (§16 mandate).
- **8.4** Update `CLAUDE.md` ("what this is" + new invariants: HelloVpn-before-auth,
  relay-is-AEAD-opaque, NetConfig-reverts-on-exit, tun MTU clamp).
- **Acceptance:** docs build/read cleanly; every test bullet above exists and passes.

---

## §12. Logging specification (global)

Use `tracing` (already in the project). Rules:
- **Levels:** `error!` = fatal / giving up; `warn!` = fallback to relay, cleanup
  failure, MTU mismatch, missing cap, rejected pairing; `info!` = link up/down, path
  chosen, assigned addr, each NetConfig mutation + revert; `debug!` = candidate
  gathering, datagram size, renegotiation, periodic data-plane counters; `trace!` =
  per-packet (default OFF; behind the existing env-filter only).
- **Never** log inside the per-packet hot loop above `trace!`. Use atomic counters +
  a periodic (10 s) summary at `debug!`.
- **Structured fields** on every link log: `link_id`, `path`, `peer`, `overlay`, `iface`.
- **Redaction:** never log the secret, derived keys, or nonce/counter values. (`--secret`
  already uses `hide_env_values`; keep that discipline.)

---

## §13. Test infrastructure — netns harness (`scripts/vpn_netns_test.sh`)

Provide a root-only bash script (and document it in the matrix). Shape:
```
# ns0 = server, ns1/ns2 = peers; veth pairs ns1<->ns0 and ns2<->ns0 (simulate WAN).
ip netns add ns0; ip netns add ns1; ip netns add ns2
# veths + addresses (10.0.0.0/24 "internet"); ns1 also gets a fake LAN 192.168.50.0/24.
# start server in ns0:  bore server --vpn --vpn-pool 10.99.0.0/16 --secret S --bind-addr ...
# host<->host: ns1 `bore vpn listen --id h --secret S --to ns0`; ns2 `bore vpn connect ...`
#   assert: ip netns exec ns2 ping <ns1 overlay>
# site<->host: ns1 `--advertise 192.168.50.0/24`; ns2 ping a host in 192.168.50.0/24
#   assert ip_forward=1 + nft table present DURING; assert BOTH reverted AFTER exit.
# relay fallback: drop UDP between ns1<->ns2 (nft drop udp on the internet veths),
#   re-run, assert link still pings (path=relay in logs).
# overlap: both --advertise 192.168.1.0/24 → assert connect exits with VpnError.
# cleanup proof: after SIGINT and after a forced panic, assert: no bore0, no routes,
#   ip_forward restored, no bore_vpn_* table.
```
**Test hygiene** (reuse project conventions): no hardcoded ports (ephemeral); serialize
tests that touch global netns (a serial guard); deterministic waits (poll for readiness,
no fixed `sleep`); root-only tests `#[ignore]` + documented.

---

## §14. Coding standards / best practices (project-specific)

- Match surrounding style; comment density and naming like neighboring modules.
- `#![forbid(unsafe_code)]` — all `unsafe` stays inside `tun-rs`.
- Everything VPN behind `#[cfg(all(feature="vpn", target_os="linux"))]`.
- `anyhow::Result` + `.context(...)` on every fallible call (codebase idiom).
- No blocking in async: subprocess via `tokio::process::Command`; file reads of
  `/proc/...` are tiny but still prefer `tokio::fs`.
- **RAII for every resource** (TUN device, `NetConfig`, server `VpnLeaseGuard`) —
  mirror `TokenGuard`. Cleanup must be exception-safe and idempotent.
- Keep **pure logic separate from I/O** (the Phase-1 split) so the hard parts are
  unit-testable without root. Inject the command-runner as a trait for `NetConfig`.
- Constants live in `shared.rs` with doc comments explaining the rationale (project
  style — see `holepunch.rs` keep-alive/idle comments).
- Reuse, don't reinvent: relay = `secret::relay`; candidate/punch = `holepunch`;
  carriers = `pool`; reconnect = `reconnect`; auth = `auth`.

---

## §15. Definition of done

- All 8 phases complete; every named test exists and passes; gates green for default,
  `--features udp`, and `--features vpn` builds.
- Default (no-feature) behavior **byte-for-byte unchanged**; existing tests untouched
  and green.
- netns harness passes all scenarios incl. **cleanup proof** (host pristine after
  SIGINT and after panic).
- `iperf3` over the overlay shows the data path is not syscall-bound.
- Every §16 expected-behavior bullet holds **and is traceably covered by a test**
  (automated where possible, documented manual procedure otherwise) — the §16
  traceability table in `docs/VPN_TEST_MATRIX.md` is complete, no empty rows.
- **Zero regressions on existing functionality**: every pre-existing test passes
  unmodified in all three builds; no existing test was edited, weakened, or removed.
- All §1 invariants hold; `#![forbid(unsafe_code)]` intact; only **one** new Rust dep.
- `docs/VPN.md`, `docs/VPN_TEST_MATRIX.md`, README section, `CLAUDE.md` update all written.

---

## §16. End-to-end usage reference — commands, options, expected behavior

> This section is the **user-facing acceptance contract**. Phase 8 docs (`README`,
> `docs/VPN.md`) are written from it, and the netns harness (§13) asserts it. If the
> implementation cannot satisfy a bullet here, stop and surface it.
>
> **MANDATORY — test coverage of this section (no exceptions):**
> 1. **Every scenario in §16.0–§16.8 must be covered by a detailed, precise,
>    repeatable test** — an automated one wherever technically possible (unit /
>    in-process integration / netns harness §13), and where automation is genuinely
>    impossible, a written manual test procedure in `docs/VPN_TEST_MATRIX.md` with
>    exact commands, exact expected output, and a checkbox. The test matrix must
>    contain a **traceability table**: one row per §16 bullet → the test (file +
>    test name, or matrix procedure id) that covers it. A §16 bullet with no row is
>    a missing deliverable — the feature is **not done** (§15).
> 2. **Zero regressions on existing functionality — not tolerated, ever.** All
>    pre-existing tests (`local`, `proxy`, `server`, `transfer`, `test-udp`, vhost,
>    secret tunnels, holepunch/QUIC, carriers, admin) must pass **unmodified** at
>    every sub-phase gate, in all three builds (default, `--no-default-features`,
>    `--features vpn`). Editing or deleting an existing test to make it pass is
>    itself a regression. If a VPN change breaks an existing test, **revert the
>    change and redesign** — do not patch the test, do not proceed.

### §16.0 Build & server

```bash
# Build (Linux only; VPN is opt-in)
cargo build --release --features vpn

# Server: enable VPN brokering, give it an overlay pool, cap concurrent links
bore server --secret S3cret --vpn --vpn-pool 10.99.0.0/16 --vpn-max-links 32
```
Expected: server starts exactly as today; one extra `info!` line noting VPN enabled,
pool `10.99.0.0/16` (16384 /30 blocks). All existing subcommands unaffected.

### §16.1 Mode A — host↔host (neither side advertises)

```bash
# Machine A (listener) — root required
sudo bore vpn listen  --to bore.example.com --secret S3cret --id mylink

# Machine B (connector) — root required
sudo bore vpn connect --to bore.example.com --secret S3cret --id mylink
```
Expected, both sides, within seconds:
- `info!` "vpn link paired" with `link_id=mylink`, assigned overlay addrs from the
  pool (listener gets `.1`, connector gets `.2` of one /30, e.g. `10.99.0.1` /
  `10.99.0.2`).
- A `bore0` interface exists: `ip addr show bore0` → `10.99.0.1/30`, MTU 1350, UP.
- `info!(path="direct")` if hole-punch succeeded, else `warn!` fallback +
  `info!(path="relay")`. Traffic flows either way.
- `ping 10.99.0.2` from A works (and vice versa). `ping -s 1300` may lose the first
  few packets on the direct path (MTU discovery, §6.1), then succeeds.
- `iperf3 -s` on A, `iperf3 -c 10.99.0.1` on B → sustained throughput (direct path:
  expected same order as `bore test-udp` direct-path bandwidth between the same hosts).
- Only host↔host traffic: **no** `ip_forward` change, **no** nft table created.

### §16.2 Mode B — site↔host (listener advertises its LAN)

```bash
# Machine A: gateway of LAN 192.168.50.0/24
sudo bore vpn listen  --to bore.example.com --secret S3cret --id site \
     --advertise 192.168.50.0/24

# Machine B: roaming client
sudo bore vpn connect --to bore.example.com --secret S3cret --id site
```
Expected:
- On A additionally: `/proc/sys/net/ipv4/ip_forward` = 1 (previous value saved), nft
  table `bore_vpn_site` with masquerade + MSS-clamp rules (`nft list table inet
  bore_vpn_site` shows it). Each mutation logged at `info!`.
- On B additionally: `ip route show` contains `192.168.50.0/24 dev bore0`.
- From B: `ping 192.168.50.10` (a real host on A's LAN) works; `curl
  http://192.168.50.10` works; the LAN host sees the traffic as coming **from A's LAN
  address** (masquerade — no route changes needed on the LAN).
- From B, TCP into the LAN gets MSS ≤ route MTU (clamp) — no PMTU blackholes.

### §16.3 Mode C — site↔site (both advertise)

```bash
# Site A gateway (LAN 192.168.50.0/24)
sudo bore vpn listen  --to bore.example.com --secret S3cret --id s2s \
     --advertise 192.168.50.0/24
# Site B gateway (LAN 192.168.60.0/24)
sudo bore vpn connect --to bore.example.com --secret S3cret --id s2s \
     --advertise 192.168.60.0/24
```
Expected: union of §16.2 on both sides (each installs a route to the *other* LAN, each
NATs inbound tunnel traffic onto its own LAN). A host on LAN A reaches a host on LAN B
**only if** LAN A routes `192.168.60.0/24` via gateway A (or the test runs from the
gateway itself) — document this; bore manages the gateway hosts, not the LANs' routers.
If both sites advertise the same CIDR: connector exits non-zero printing
`VpnError("overlapping subnets: ...")`; listener stays registered and keeps waiting.

### §16.4 Static addressing (no server pool needed)

```bash
sudo bore vpn listen  --to srv --secret S3cret --id st \
     --vpn-addr 172.31.0.1/30 --vpn-peer-addr 172.31.0.2
sudo bore vpn connect --to srv --secret S3cret --id st \
     --vpn-addr 172.31.0.2/30 --vpn-peer-addr 172.31.0.1
```
Expected: link comes up with exactly those addrs. Mixed mode (one side static, one
pool), inconsistent mirrors, or collision with a live lease → `VpnError` (§5 rules),
connector exits non-zero.

### §16.5 `--no-route-manage` (operator-managed routing)

```bash
sudo bore vpn connect --to srv --secret S3cret --id site --no-route-manage
```
Expected: tun device is still created/addressed/up (interface itself is not optional),
but **no** route/sysctl/nft mutations run; every skipped command is printed verbatim
(copy-paste runnable) so the operator applies them. Teardown reverts only what bore
itself applied (the interface).

### §16.6 Path fallback & resilience

- Block UDP between the peers while a direct link is up → within the holepunch
  failure-detection window the client logs `warn!` (fallback) then
  `info!(path="relay")`; pings stall briefly, then resume. **No process exit.**
- Kill the server while `--auto-reconnect` is set → client logs reconnect attempts
  with backoff; on server return the link re-pairs and the same tun keeps working
  (routes re-validated, not duplicated).
- `Ctrl-C` (SIGINT) either side → `info!` per undo: routes deleted, nft table dropped
  (single atomic delete), `ip_forward` restored to its saved value, tun gone. Host
  pristine — `ip route`, `nft list tables`, `cat /proc/sys/net/ipv4/ip_forward`
  identical to before start.
- `kill -9` → no cleanup possible (documented); next start of the same `--id` reclaims
  stale state (Phase 5 stale-reclaim) and proceeds.

### §16.7 Failure messages the user must see (exact situations)

| Situation | Expected outcome |
|---|---|
| No `--secret` | clap-level error, before any connection |
| Not root / no `CAP_NET_ADMIN` | actionable `bail!` naming the missing privilege, **before** any mutation |
| `ip`/`nft`+`iptables` missing | actionable `bail!` naming the missing binary |
| Server without `--vpn` / not VPN-built | server replies `VpnError("vpn not supported/enabled...")`; client prints it, exits non-zero |
| Server **older** than this feature | connection drops after first message → client prints the "server may be too old" hint (Phase 7.1) |
| Duplicate `--id` on listen | `VpnError`, exit non-zero |
| `connect` to unknown id | `VpnError("no such vpn link")` |
| Pool exhausted | `VpnError`, names the pool |
| Overlapping subnets | `VpnError` listing the offending CIDRs |

### §16.8 Quick troubleshooting map (goes into docs/VPN.md)

- Link pairs but no `ping` → check `path=` in logs; if `relay`, run `bore test-udp`
  between the hosts to see why direct failed (NAT type).
- `ping` ok, TCP slow/stalls → MTU: try `--mtu 1280`; check the MSS-clamp rule exists
  on the gateway.
- Works from gateway, not from LAN hosts (site↔site) → the LAN's router lacks the
  route toward the peer LAN via the gateway (§16.3 note).

---

## §V2. V2-TODO (explicitly out of scope for v1)

- **Multi-peer mesh / hub:** one virtual network of N nodes; membership protocol, peer
  discovery, full route distribution, larger overlay pool.
- **Overlapping subnets via 1:1 NAT** (DNAT/SNAT remap per remote site).
- **IPv6 overlay + dual-stack** advertised subnets and NAT66/NPTv6.
- **AEAD replay protection** on the relay path (sliding window over the explicit
  counter; already wire-compatible since the counter is sent) + AAD binding (include
  direction + link id).
- **Relay over an unreliable server datagram path** (avoid reliable-over-reliable on
  fallback) — e.g. a server-side UDP relay carrying QUIC datagrams end-to-end.
- **Dynamic PMTU**: track `max_datagram_size()` and adjust the interface MTU live
  instead of the fixed conservative 1350.
- **Full GSO/GRO offload** if Phase 6.2 had to fall back to single-packet.
- **Multi-queue TUN (`IFF_MULTI_QUEUE`)** + per-queue worker tasks, if a single
  uplink/downlink pair proves CPU-bound on >1 Gbps paths.
- **`--carriers` for the VPN relay path** (multiple relay substreams with a
  reordering-tolerant scheme) — protocol field already reserved.
- **Privilege drop** after interface/route setup (retain only what's needed; or a
  setup-then-drop split).
- **Windows/macOS** support (wintun / utun) — currently Linux-only by design.
- **Key rotation / rekey** on long-lived links; per-link PSK independent of the bore
  secret.
- **Admin page**: per-link throughput, current path, packet drop counters.

---

## §R. Reasoning Appendix — the "why" (so the implementer never second-guesses)

- **§R.1 Datagrams, not QUIC streams, for the direct path.** Carrying IP packets over a
  *reliable* stream causes reliable-over-reliable ("TCP-over-TCP") meltdown: under loss,
  two retransmission timers fight and throughput collapses. IP is already a lossy,
  best-effort layer; upper layers (TCP/QUIC inside the tunnel) handle their own
  retransmission. So the tunnel must be **unreliable** → QUIC DATAGRAM frames. quinn
  supports them and already exposes `max_datagram_size()`; we only enable the buffers.
- **§R.2 Why `--secret` is mandatory + AEAD on relay only.** Relay E2E privacy requires
  a key the **server cannot derive**. The server issues the public `session_nonce`, so
  the key must mix in a client-only secret: `HKDF(secret, nonce)`. No secret ⇒ no
  privacy on relay. The direct path is E2E via QUIC-TLS regardless, so AEAD is applied
  **only** on the relay branch (no wasteful double-encryption on direct).
- **§R.3 Why reuse `secret::relay` unchanged.** The relay is an opaque byte-splice
  between two substreams. Because clients put **ciphertext** on it, the server already
  cannot read VPN traffic — we get E2E-on-relay with **zero** server-side crypto and no
  new server code. This is the elegant core of the design.
- **§R.4 Why shell out for routes/NAT instead of a netlink crate.** It is control-plane
  only (once per link up/down) → **no data-path cost** → does not affect the perf goal.
  Shelling to `ip`/`nft` avoids a heavy `rtnetlink`/`netlink-packet-route` dependency
  stack, keeps the new-dep count at one (the TUN crate), and matches the user's "don't
  overcomplicate" steer. A dedicated tagged nft table makes teardown a single atomic op.
- **§R.5 Why /30 and fixed MTU 1350.** /30 (4 addrs: net/.1/.2/bcast) is universally
  compatible (avoids /31 tooling quirks) and trivially carves from the pool. A fixed
  conservative MTU avoids live-PMTU complexity and guarantees a segmented packet always
  fits one QUIC datagram across a direct↔relay path switch; gateway MSS clamping keeps
  forwarded TCP healthy. Dynamic PMTU is a §V2 refinement.
- **§R.6 Why all three topologies are 2-party.** `host↔host`, `site↔host`, `site↔site`
  each connect exactly two endpoints; "site" just means that endpoint advertises subnets
  and turns on forwarding+NAT. So point-to-point covers all three with one mechanism;
  only multi-peer mesh needs more, hence §V2.
- **§R.7 Why correctness-first then offload in Phase 6.** Offload (GSO/GRO/`vnet_hdr`)
  is the riskiest, most API-specific part. Landing single-packet correctness first gives
  a working `ping` to regression-test against before the optimization, so a broken
  offload can't be mistaken for a broken protocol. The architecture is batched from the
  start (the `send_batch`/`recv_batch` interface); only the backing is swapped.
