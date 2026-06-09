# VHOST `--udp` — Implementation Handoff (QUIC server↔provider data path)

**Audience:** the implementing engineer (gpt) as executor. This document is the
authoritative spec: the architecture, the patterns to reuse, the exact signatures, and
the quality bar are all fixed here. Follow it phase by phase. Where it says "reuse X",
reuse X — do not reinvent. Where it gives a signature, match it. Surface any deviation
explicitly rather than improvising.

**Status:** design approved. No code written yet. Branch `vhost_gpt`.

**Companion docs:** `docs/VHOST.md` (user-facing vhost reference, to be extended in
Phase 6), `src/holepunch.rs` (the QUIC/`--udp` stack this feature reuses).

---

## 1. Objective

Add `bore vhost --udp`. When set, the **server↔provider** hop of a vhost tunnel is
opportunistically upgraded from yamux-over-TCP to **native QUIC streams**, with
**automatic, seamless fallback** to the existing TCP carrier path when UDP is
unavailable. The browser-facing leg is untouched. No regressions; the TCP path stays
byte-for-byte identical when QUIC is not in use.

### Why (value, honestly bounded)

The provider uplink is the bottleneck under concurrency and on lossy/high-RTT links.
yamux-over-one-TCP has head-of-line blocking and a single congestion window. QUIC gives
per-stream loss recovery (BBR) and unlimited independent streams over one connection.

| Scenario | QUIC helps? |
|---|---|
| Aggregate bandwidth under concurrency | Yes — native per-stream, no yamux HOL, no single cwnd |
| Bandwidth + latency on lossy / high-RTT provider uplink | **Yes — the real win** |
| Tail latency under load | Yes — one slow response no longer HOL-blocks others |
| Server file descriptors | Yes — 1 QUIC conn vs N TCP carriers |
| Single bulk flow on a clean low-loss link | ~No — tuned TCP ≈ QUIC |
| Browser-facing baseline RTT | No — that leg is fixed TCP/TLS |
| Server bandwidth offload | No — server still relays every byte |

Do not oversell it in logs, docs, or comments. State what it does.

---

## 2. Topology and the hard constraints

```
Browser ──TCP/TLS──> bore SERVER ──[THIS HOP: TCP→QUIC]──> bore PROVIDER ──TCP──> local svc
        server terminates HTTP,            yamux/TCP today,
        routes by Host                     QUIC with --udp
```

- **Browser↔server stays TCP/TLS.** The browser is a plain HTTP client; it cannot speak
  bore QUIC. Out of scope: HTTP/3 frontend.
- **The server stays in the data path.** It terminates the browser's HTTP/TLS to read
  the `Host` header (`src/vhost.rs` `handle_http`/`handle_https`). Unlike secret-tunnel
  `--udp` (peer-to-peer, bypasses the server), vhost QUIC only changes the
  server↔provider transport. **No server bandwidth offload.**
- **The server is public** (operator opens the firewall). Therefore **no STUN, no
  hole-punch, no broker rendezvous.** The provider dials the server's UDP port directly.
  This is strictly simpler than secret-tunnel UDP. Do not pull in the STUN / candidate /
  `punch()` machinery.

### Invariants that must survive (from CLAUDE.md and the existing code)

- Client sends `Hello`/`HelloVhost` before auth (yamux is lazy; otherwise deadlock).
- `mux::STREAM_READY` (the single `0u8` marker) is written before splice on every
  forwarded stream — including the QUIC path.
- `copy_bidirectional_with_sizes` (half-close aware) is the splice; never replace it.
- `--max-conns` semaphore remains the real connection bound.
- `carriers <= 1` keeps the single-connection TCP path byte-for-byte unchanged.
- `#![forbid(unsafe_code)]` — no unsafe.
- Never hold a `DashMap` guard or a `std::sync` lock across an `.await`.

---

## 3. QUIC roles (read carefully — inverse of secret tunnels)

| | bore server | bore vhost provider |
|---|---|---|
| QUIC role | **server** endpoint (accepts) | **client** (dials out) |
| Reuse | `holepunch::server_endpoint` (`src/holepunch.rs:1283`, currently private) | `holepunch::client_endpoint` (`:1266`, private) |
| Punch? | no | **no** (`punch()` is skipped; server is public) |
| Data streams | **opens** one bidi per browser request (`DirectConn::open_stream`, `:998`) | **accepts** them (`DirectConn::accept_stream`, `:1009`) |

In secret tunnels the *consumer* opens streams and the *provider* accepts. Here the
**server opens** and the **provider accepts**. QUIC bidi streams may be opened by either
endpoint, so this needs no new primitive — but **comment it at every relevant site** so a
future reader does not "fix" it to match secret tunnels.

### Reused unchanged (do not modify)

- `DirectConn` and `QuicTransport` (`src/holepunch.rs:974`–`1067`) — `QuicTransport`
  already implements `AsyncRead`/`AsyncWrite`; it is the universal stream type.
- `derive_token(secret: Option<&str>, nonce: &[u8]) -> [u8; TOKEN_LEN]`
  (`src/holepunch.rs:84`); `TOKEN_LEN = 32` (`:60`).
- `UdpDirectTuning` (`src/shared.rs:301`) and all `BORE_UDP_*` env tuning; `UDP_NONCE_LEN
  = 16` (`src/shared.rs:29`).
- `Client::handle_connection<S: AsyncRead + AsyncWrite + Unpin>` (`src/client.rs:730`) —
  **already generic**; its doc already says "a yamux substream … or a native QUIC bidi".
  Reuse it verbatim for the QUIC accept loop.
- `relay_vhost`'s splice tail (`src/vhost.rs:489`–`507`): write `STREAM_READY`, write the
  (optionally rewritten) head, `copy_bidirectional_with_sizes`.

### Must expose (currently private)

- `holepunch::tokens_match` (`:95`) — make `pub(crate)`, or add the two new handshake
  functions (below) inside `holepunch.rs` so they can call it directly. **Prefer the
  latter**: keep `tokens_match` private and put the handshakes in `holepunch.rs`.

---

## 4. Protocol additions (`src/shared.rs`)

All additions are backward-compatible: new enum variants are ignored by older peers in
practice, and the new struct field uses `#[serde(default)]`. **Do not** `#[cfg]`-gate the
wire types — only the *handling* code is gated (mirror `ServerMessage::UdpPunch`, which is
not cfg-gated). Control frames are JSON, capped at `MAX_FRAME_LENGTH = 1024` bytes;
`VhostUdp` (nonce[16] + u16 + `UdpDirectTuning`) is smaller than the already-working
`UdpPunch`, so it fits.

Add to `ClientMessage` (extend the existing `HelloVhost`, `src/shared.rs:501`):

```rust
HelloVhost {
    subdomain: String,
    client_id: String,
    notes: Option<String>,
    basic_auth: bool,
    #[serde(default)]
    carriers: u16,
    /// Whether the provider wants the QUIC direct data path (`bore vhost --udp`).
    /// `#[serde(default)]` keeps the wire format backward-compatible (old clients
    /// deserialize as `false`). Honored only when the server has UDP enabled.
    #[serde(default)]
    udp: bool,
},
```

Add a new `ClientMessage` variant:

```rust
/// Ask the server to re-issue a fresh vhost-UDP nonce after the QUIC direct path
/// dropped, so the provider can re-dial. Carries the subdomain to disambiguate.
VhostUdpRenew { subdomain: String },
```

Add a new `ServerMessage` variant:

```rust
/// Offer the vhost QUIC direct path to a provider that set `HelloVhost::udp` (and
/// only when the server has UDP enabled). The provider dials `(control_host,
/// port)/udp`, authenticates with `derive_token(secret, nonce)`, then accepts QUIC
/// bidi streams (one per proxied request). Host is the control host the client
/// already knows; only the UDP port is advertised.
VhostUdp {
    /// Server UDP port to dial for the QUIC direct path.
    port: u16,
    /// Session nonce; server and provider derive the same token from it + secret.
    nonce: [u8; UDP_NONCE_LEN],
    /// Direct-UDP transport tuning the server wants the provider to use.
    #[serde(default)]
    tuning: UdpDirectTuning,
},
```

Add unit tests for serde round-trips (Phase 0): an old-format `HelloVhost` without `udp`
deserializes to `udp == false`; `VhostUdp` round-trips.

---

## 5. QUIC authentication (multi-tenant, no broker)

One server QUIC endpoint serves many providers, so the provider must identify which
subdomain it is before the server can pick the expected token.

**Wire format on the first (auth) QUIC bidi stream, provider → server:**

```
[u16 sub_len big-endian][sub_len bytes: subdomain UTF-8][TOKEN_LEN bytes: token]
```

where `token = derive_token(secret, nonce)`, `secret` = the tunnel secret (the same one
used for control-channel auth; `None`/empty when the server has no secret), `nonce` = the
value the server sent in `VhostUdp`.

**Server verification (constant-time):**

1. Read `sub_len`, then `subdomain`, then the 32-byte token.
2. Look up the nonce the server issued for `subdomain`. If none → reject (`bail!`).
3. Compute `expected = derive_token(self.secret, nonce)`.
4. `tokens_match(&expected, &received)` (constant-time). Mismatch → reject + `warn!`.
5. On success: write the server's own `expected` token back (so the provider confirms it
   reached the right server), then the connection is trusted.

**Security:** self-signed cert + SkipVerify + HMAC token = the existing model
(`src/holepunch.rs`). The nonce travels over the already-authenticated control channel
(confidential when the control connection is TLS, i.e. `https://`). The token also
requires the shared secret, so an eavesdropper without `--secret` cannot forge it. The
subdomain is bound to its own server-issued nonce, so one provider cannot hijack another
subdomain's QUIC path. **Document the weak-auth caveat** for plain-TCP control + no
secret (identical to the secret-tunnel-without-secret caveat).

The server needs the raw secret string to call `derive_token`. Today `Server::new`
consumes it into `auth: Option<Authenticator>` (`src/server.rs:136`). **Store the raw
secret too:** add `secret: Option<String>` to `Server`, set in `new` from the same
argument. Do not log it.

### New functions in `src/holepunch.rs` (all `#[cfg(feature = "udp")]`)

```rust
/// Provider side (QUIC client): dial a public bore server's vhost QUIC endpoint and
/// authenticate for `subdomain`. No hole-punching — the server is public. Returns the
/// authenticated direct connection; the provider then `accept_stream`s on it.
pub async fn vhost_connect(
    socket: UdpSocket,                 // freshly bound local UDP socket
    server_addr: SocketAddr,           // (control_host, advertised port)
    subdomain: &str,
    token: [u8; TOKEN_LEN],
    tuning: UdpDirectTuning,
) -> Result<DirectConn>;

/// Server side (QUIC server): finish one accepted incoming connection's auth handshake.
/// `lookup(subdomain) -> Option<expected_token>` returns the token the server expects
/// for that subdomain (derived from the issued nonce + the server secret), or `None` to
/// reject. Returns the verified subdomain and the direct connection.
pub async fn vhost_server_handshake(
    conn: quinn::Connection,
    endpoint: Endpoint,
    lookup: impl Fn(&str) -> Option<[u8; TOKEN_LEN]>,
) -> Result<(String, DirectConn)>;
```

`vhost_connect` builds a client endpoint via the existing private `client_endpoint`
(skip `punch`), `endpoint.connect(server_addr, "bore")`, opens the auth bi, writes the
framed `[len][sub][token]`, reads + verifies the reply token, returns
`DirectConn { conn, endpoint }`. `vhost_server_handshake` does the inverse with
`accept_bi`. Reuse `tokens_match` internally. Bound both with `NETWORK_TIMEOUT`.

---

## 6. Data-structure changes

### `VhostEntry` (`src/vhost.rs:314`)

```rust
pub struct VhostEntry {
    pool: Arc<CarrierPool>,                  // unchanged — always present (TCP fallback)
    headers: /* unchanged */,
    /// Live QUIC direct connection to the provider, or `None` when not (yet)
    /// established / after it dropped. Read on the hot path; cloned out under a brief
    /// lock, never held across an await. `DirectConn` is cheap to clone (handles).
    #[cfg(feature = "udp")]
    direct: std::sync::RwLock<Option<crate::holepunch::DirectConn>>,
}
```

Use `std::sync::RwLock<Option<DirectConn>>` (mirrors `SharedVhostConfig`'s `RwLock`).
**Do not** add the `arc-swap` crate — it is not a dependency. Access pattern on the hot
path: take the read lock, `.clone()` the `Option<DirectConn>`, **drop the guard**, then
`.await`. Clearing on drop uses the write lock for a single store.

When compiled without the `udp` feature the field is absent and `relay_vhost` is exactly
today's code.

### Server-side UDP state

- `Server.secret: Option<String>` (§5).
- `Server.vhost_quic_port: u16` (the configured UDP port; see §8).
- A pending-nonce map, type alias next to `VhostRegistry`:
  `pub type PendingVhostUdp = Arc<DashMap<String /*subdomain*/, [u8; UDP_NONCE_LEN]>>;`
  Created once (like `vhost_registry`), shared between `serve_vhost_provider` (writes the
  nonce, sends `VhostUdp`, handles `VhostUdpRenew`) and the QUIC accept loop (reads it via
  `lookup`).
- The long-lived `quinn::Endpoint` for the server, created in `listen()` (§8), shared with
  the accept loop. Store on the `Arc<Server>` handle the way `vhost_registry`/
  `pending_carriers` are threaded.

`nonce` generation: reuse the existing CSPRNG helper pattern (`secret.rs:158`
`new_nonce()` uses `ring::SystemRandom` → `[u8; UDP_NONCE_LEN]`). Add an equivalent in
`vhost.rs` (or make `secret::new_nonce` `pub(crate)` and reuse). Never all-zero.

**Cleanup:** extend the existing `Deregister` drop guard (`src/vhost.rs:328`) to also
remove the subdomain's entry from `PendingVhostUdp`, so a disconnecting provider leaves no
stale nonce. Give `Deregister` an `Option<PendingVhostUdp>` handle.

---

## 7. The relay branch (`relay_vhost`, `src/vhost.rs:482`)

Keep the signature and the splice tail. Add a transport-selection head. Box the two
transport types to a trait object so the tail is shared (both implement
`AsyncRead + AsyncWrite + Unpin`):

```rust
pub async fn relay_vhost(
    public: impl AsyncRead + AsyncWrite + Unpin,
    entry: &VhostEntry,
    head: Vec<u8>,
) -> Result<()> {
    // Prefer the QUIC direct path; fall back to a TCP carrier per-request.
    let mut provider: Pin<Box<dyn AsyncReadWrite>> = {
        #[cfg(feature = "udp")]
        {
            // Brief lock: clone the handle out, then drop the guard before awaiting.
            let direct = entry.direct.read().unwrap().clone();
            match direct {
                Some(d) => match d.open_stream().await {
                    Ok(s) => Box::pin(s),                         // QUIC bidi
                    Err(err) => {
                        debug!(%err, "vhost QUIC open_stream failed; using TCP carrier");
                        let opener = entry.pool.pick().context("no live vhost carrier")?;
                        Box::pin(opener.open().await.context("vhost provider unavailable")?)
                    }
                },
                None => {
                    let opener = entry.pool.pick().context("no live vhost carrier")?;
                    Box::pin(opener.open().await.context("vhost provider unavailable")?)
                }
            }
        }
        #[cfg(not(feature = "udp"))]
        {
            let opener = entry.pool.pick().context("no live vhost carrier")?;
            Box::pin(opener.open().await.context("vhost provider unavailable")?)
        }
    };

    provider.write_all(&[mux::STREAM_READY]).await?;
    let mut public = public;
    if entry.headers.is_empty() {
        provider.write_all(&head).await?;
    } else {
        let rewritten = rewrite_head(&head, &entry.headers);
        provider.write_all(&rewritten).await?;
    }
    tokio::io::copy_bidirectional_with_sizes(
        &mut public, &mut provider, PROXY_BUFFER_SIZE, PROXY_BUFFER_SIZE,
    ).await?;
    Ok(())
}
```

Define a local sealed helper trait `AsyncReadWrite: AsyncRead + AsyncWrite + Unpin` with a
blanket impl (mirror `mux::Transport`, `src/mux.rs:26`) so the `dyn` works. Confirm the
`#[cfg(not(feature = "udp"))]` arm compiles to today's exact two lines (byte-for-byte
behavior preserved). Per-request fallback does **not** clear `entry.direct`; only the
connection-closed monitor (§9) does.

---

## 8. Server QUIC endpoint lifecycle (`src/server.rs`)

Endpoint created in `Server::listen` (`src/server.rs:247`), next to where the STUN
responder binds (`:368`). Gate on `self.udp == true` **and** vhost configured.

```rust
// After the STUN-responder block, when vhost + udp are both enabled:
#[cfg(feature = "udp")]
if this.udp && this.vhost_config.is_some() {
    match tokio::net::UdpSocket::bind((this.bind_addr, this.vhost_quic_port)).await {
        Ok(udp) => {
            let endpoint = holepunch::vhost_server_endpoint(udp, &this.udp_tuning)?; // thin pub wrapper over server_endpoint
            // store endpoint on `this` (shared) so serve_vhost_provider can read the port,
            // and spawn the accept loop:
            let registry = this.vhost_registry.clone();
            let pending = this.pending_vhost_udp.clone();
            let secret = this.secret.clone();
            let ep = endpoint.clone();
            tokio::spawn(async move {
                while let Some(incoming) = ep.accept().await {
                    let (registry, pending, secret, ep) =
                        (registry.clone(), pending.clone(), secret.clone(), ep.clone());
                    tokio::spawn(async move {
                        let conn = match incoming.await { Ok(c) => c, Err(_) => return };
                        let lookup = |sub: &str| {
                            pending.get(sub).map(|n| holepunch::derive_token(secret.as_deref(), &*n))
                        };
                        match holepunch::vhost_server_handshake(conn, ep, lookup).await {
                            Ok((sub, direct)) => {
                                if let Some(entry) = registry.get(&sub) {
                                    *entry.direct.write().unwrap() = Some(direct.clone());
                                    info!(subdomain = %sub, "vhost QUIC direct path established");
                                    // closed-monitor: clear when this conn dies
                                    let registry = registry.clone();
                                    let direct2 = direct.clone();
                                    tokio::spawn(async move {
                                        direct2.closed().await;
                                        if let Some(entry) = registry.get(&sub) {
                                            let mut g = entry.direct.write().unwrap();
                                            // compare-and-clear: don't stomp a newer conn
                                            if matches!(&*g, Some(d) if d.same_conn(&direct2)) {
                                                *g = None;
                                                debug!(subdomain = %sub, "vhost QUIC direct path closed; reverting to TCP");
                                            }
                                        }
                                    });
                                }
                            }
                            Err(err) => debug!(%err, "vhost QUIC handshake rejected"),
                        }
                    });
                }
            });
        }
        Err(err) => warn!(%err, "failed to bind vhost QUIC endpoint; vhost --udp disabled"),
    }
}
```

`DirectConn::same_conn(&other)` does not exist yet — add a small helper (compare
`self.conn.stable_id()` via quinn, or store a generation counter). If a clean identity
check is awkward, alternative: store a `u64` generation in `VhostEntry` alongside
`direct`, increment on each new conn, and have the monitor capture its generation and only
clear if unchanged. **Use the generation approach if `stable_id` comparison is not
straightforward** — it is simpler and unambiguous.

`vhost_server_endpoint` is a thin `pub(crate)` wrapper over the existing private
`server_endpoint` so `server.rs` can build it.

---

## 9. Provider side (`src/client.rs`)

`new_vhost_provider` (`:395`): add a `udp: bool` parameter; send it in `HelloVhost`
(`:418`). Store it on the `Client` (add `#[cfg(feature = "udp")] vhost_udp: bool`, set in
the constructor; the struct already carries `#[cfg(udp)] secret` and `udp_socket`).

The QUIC setup happens during `listen()` (`:509`), not in the constructor — mirroring how
secret tunnels handle `UdpPunch` inside `listen`. In the control-message `select` loop,
handle the new message:

```rust
#[cfg(feature = "udp")]
Some(ServerMessage::VhostUdp { port, nonce, tuning }) if self.vhost_udp => {
    // server is public → dial directly, no punch.
    let server_addr = /* resolve control host + port to SocketAddr */;
    let token = holepunch::derive_token(secret.as_deref(), &nonce);
    let socket = bind_udp_socket(...).await?;     // ephemeral local UDP socket
    match holepunch::vhost_connect(socket, server_addr, &subdomain, token, tuning).await {
        Ok(direct) => {
            info!("vhost direct udp connection established");
            // spawn the accept loop (below)
        }
        Err(err) => {
            // UDP blocked / unreachable → stay on TCP carriers, no user-visible error.
            warn!(%err, "vhost direct udp unavailable; using TCP relay");
        }
    }
}
```

Accept loop (one task): `loop { let s = direct.accept_stream().await?; let this =
Arc::clone(&self); tokio::spawn(async move { let _ = this.handle_connection(s).await; }); }`.
`handle_connection` (`:730`) is already generic and already reads `STREAM_READY` + applies
the Basic-auth gate + splices — **reuse it unchanged**. The provider's existing per-stream
spawn pattern (`spawn_handle`, `:768`) is the model.

**Renewal:** when `accept_stream` errors (the QUIC connection dropped), the accept loop
ends; the provider sends `ClientMessage::VhostUdpRenew { subdomain }` on the control
channel and the server replies with a fresh `VhostUdp`, which re-enters the handler above.
Add exponential backoff between renew attempts (mirror `secret.rs` `upgrade_task`,
`:1507` — 2s, 4s, 8s … cap). The yamux control + carrier connection is **never** torn down
for UDP; it remains the control channel and the fallback data path.

Resolving `server_addr`: the provider knows the control host from `to`/`Endpoint::parse`.
Resolve `(host, port)` to a `SocketAddr` (DNS if needed). The advertised `port` is
authoritative; do not guess.

---

## 10. Configuration & flags

### Client (`src/main.rs` `Vhost` command)

Add `--udp` (bool, `env = "BORE_VHOST_UDP"`), defaulting false. Plumb into
`Client::new_vhost_provider(..., udp)`. When the binary is built **without** the `udp`
feature and `--udp` is set, log a single `warn!` ("built without udp support; ignoring
--udp") and proceed on TCP.

### Server

Add `--vhost-quic-port <u16>` (`env = "BORE_VHOST_QUIC_PORT"`), `Option<u16>`.
**Default = the vhost `https_port` (443) on UDP** when unset; resolve the default in the
dispatch after the vhost config is built (`src/main.rs:1206`+). Thread into
`Server::set_vhost_quic_port(port)` (add the setter next to `set_udp`, `src/server.rs:155`).
The server-wide `--udp` flag already exists (`server.set_udp(udp)`, `main.rs:1180`) and
gates the whole feature. Follow the exact plumbing pattern already used for
`vhost_http_port`/`vhost_https_port` (destructure in the `Server { .. }` match arm at
`main.rs:1120`, set on the builder at `:1147`+).

This UDP port is **distinct** from the secret-tunnel STUN responder on
`control_port/udp`, so there is no socket conflict. Document the one extra UDP firewall
rule.

---

## 11. Phased plan (gate every sub-phase)

CI gate after **every** sub-phase, no exceptions: `cargo fmt --check` ·
`cargo clippy --all-targets -- -D warnings` · `cargo test`. Also build the udp feature:
`cargo build --features udp` and `cargo clippy --all-targets --features udp -- -D
warnings`. Zero regressions against the current **306-test** baseline.

- **Phase 0 — protocol + config plumbing.** §4 message changes; §10 client `--udp` and
  server `--vhost-quic-port` (+ `Server.secret`, `set_vhost_quic_port`, `PendingVhostUdp`
  type + field, `vhost_udp` client field). No runtime behavior yet. Unit tests: serde
  defaults + round-trips. Acceptance: builds with and without `udp`; new flags parse;
  existing tests green.

- **Phase 1 — holepunch handshakes + server endpoint.** §5 `vhost_connect` /
  `vhost_server_handshake` / `vhost_server_endpoint`; §8 endpoint boot + accept loop +
  closed-monitor (generation approach). §6 `VhostEntry.direct` field. The accept loop runs
  but no provider connects yet. Acceptance: server with vhost+udp binds the QUIC port and
  logs it; nothing breaks if no provider dials.

- **Phase 2 — provider dial + accept loop.** §9 handle `VhostUdp`, `vhost_connect`, spawn
  the `handle_connection` accept loop; send `VhostUdpRenew` on drop with backoff.
  `serve_vhost_provider` (`src/vhost.rs:343`): when `udp` negotiated, generate a nonce,
  insert into `PendingVhostUdp`, send `ServerMessage::VhostUdp`; handle incoming
  `VhostUdpRenew` in its control loop (`:453`) by re-issuing. Acceptance: with QUIC up,
  the server's `entry.direct` becomes `Some`; logs on both ends confirm establishment.

- **Phase 3 — relay branch.** §7 `relay_vhost` transport selection + the local
  `AsyncReadWrite` trait. Acceptance: a request is served over QUIC when `direct` is
  `Some`; the `#[cfg(not(udp))]` arm and the `direct == None` arm are byte-for-byte the
  old path.

- **Phase 4 — resilience.** Verify: UDP-blocked dial falls back silently; server-without-udp
  never offers `VhostUdp`; mid-session QUIC drop clears `direct` (compare-by-generation)
  and the next request uses the pool; renewal re-establishes. Audit for guards held across
  awaits and stale-`DirectConn` leaks. Acceptance: the fallback/drop tests (Phase 5) pass.

- **Phase 5 — tests** (§12).

- **Phase 6 — docs** (§13).

Each phase that changes behavior updates the relevant docs in the same phase (CLAUDE.md
rule: docs are part of the deliverable).

---

## 12. Tests

Unit (in `src/`):
- `derive_token`/handshake framing for the subdomain-keyed token (a known nonce+secret
  produces a stable token; wrong subdomain/secret fails `tokens_match`).
- `HelloVhost` serde default `udp == false` for the old wire format; `VhostUdp` round-trip
  (extend the existing serde tests around `src/shared.rs:860`+).

Integration (`tests/vhost_test.rs`, gate the udp ones with `#[cfg(feature = "udp")]`).
Reuse the existing harness: `spawn_server_vhost`, the HTTP stubs, `self_signed_for`,
`send_http`/`send_http_post`.
- **Happy path:** server with vhost + udp on loopback; provider `--udp`; browser `GET`
  **and** `POST` (with body) → correct response, body preserved. Assert the QUIC path was
  used (e.g. via `DirectConn::stats()` bytes > 0 exposed through a test hook, or a debug
  counter on `VhostEntry`). 
- **Fallback:** server with udp **disabled** (or an unreachable QUIC port) + provider
  `--udp` → served over TCP, browser still gets `200`, **no error surfaced**.
- **Drop/recover:** establish QUIC, force the connection closed, assert `entry.direct`
  clears and the next request is served via the pool; then assert renewal re-establishes.
- **Regression:** the full suite stays green; the existing 306 tests unchanged; assert the
  `carriers <= 1` non-udp path is unaffected.

Tests must be deterministic (loopback, bounded timeouts). No reliance on external STUN or
the public internet.

---

## 13. Documentation

- `docs/VHOST.md`: new "UDP / QUIC data path" section — the honest value-add table (§1),
  the "server stays in the path; only the provider hop upgrades; no offload" topology, the
  one extra UDP firewall port and that it is distinct from `control_port/udp`, the
  weak-auth-without-secret caveat. Add a `--udp` row to the client flag table and a
  `--vhost-quic-port` row to the server flag table. Move "QUIC server↔client relay path"
  out of "MVP limitations and future work".
- `docker/docker-compose.server.yml`: add `BORE_VHOST_QUIC_PORT` to the vhost env block, a
  commented UDP `ports:` line (e.g. `"443:443/udp"`), and a firewall note.
- `docker/docker-compose.client.yml`: a `bore vhost ... --udp` example.

---

## 14. Quality bar & style (non-negotiable)

- **Logging uniform with the rest of bore.** `tracing` macros; structured fields
  (`%subdomain`, `%err`), not string interpolation. `info!` for lifecycle milestones
  ("established", "listening"), `debug!` for fallback/recoverable events, `warn!` for
  genuine problems (rejected handshake, bind failure). Match the existing phrasing style
  in `holepunch.rs`/`secret.rs` ("direct udp connection established", "falling back to
  relay"). Never log secrets, tokens, or nonces.
- **Errors** carry context via `.context("…")`/`anyhow`. Recoverable network failures
  must not bubble up as user-visible errors — they fall back.
- **Feature gating:** all QUIC code behind `#[cfg(feature = "udp")]`; the binary must
  build, clippy-clean, and pass tests **both** with and without the feature.
- **Concurrency:** no `DashMap`/`std::sync` guard across `.await`. Clone handles out, drop
  the guard, then await. `DirectConn` clones are cheap.
- **No new dependencies.** Use `std::sync::RwLock`, not `arc-swap`.
- **Comments at the inversion points** (server opens / provider accepts) and at every
  fallback branch, so the control flow is legible.
- **Correct before clever.** If a detail here proves wrong against the code, stop and
  surface it rather than papering over it.

---

## 15. Risks / resolved decisions

- **QUIC port vs STUN responder:** resolved — dedicated UDP port (default `https_port`,
  443/udp), distinct from `control_port/udp`. Revisit only if a single-UDP-port
  deployment is ever required.
- **`MAX_DIRECT_STREAMS = 4096`** (`src/shared.rs`) caps concurrent proxied connections
  per QUIC connection; document the interplay with `--max-conns`. For vhost this is per
  provider and generous.
- **Inverse stream direction** (server opens, provider accepts): valid in QUIC; comment it
  so it is not "corrected".
- **Keep-alive / idle timeout** (`QUIC_KEEPALIVE 3s`, `QUIC_MAX_IDLE 10s`,
  `src/holepunch.rs:76`/`:78`) are tuned for P2P. They should suit a long-lived
  public-server connection; if providers see spurious drops, raise the idle timeout
  (config, not hardcode).
- **Connection identity for the closed-monitor:** prefer a per-entry generation counter
  over comparing quinn `stable_id`; it is unambiguous and avoids depending on quinn
  internals.
