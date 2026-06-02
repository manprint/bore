# UPSTREAM_CHANGES.md — fork state & agent memory

Memory file for the coding agent. Reading this should be enough to resume
development of this `bore` fork from where work stopped, without re-deriving the
architecture or repeating mistakes already solved.

## Orientation

- **Fork base (upstream):** `ekzhang/bore`, commit `00a735a` ("updated slab"),
  crate version `0.6.0`. The base is also the local `main` branch (unchanged).
- **Work lives on branch `dev`** (pushed; CI runs on `main` and `dev`). HEAD: see
  `git log --oneline main..HEAD`.
- The upstream was ~400 lines: one TCP control connection on a fixed port `7835`,
  and a **separate TCP connection (re-authenticated) per proxied connection**,
  keyed by UUID, with a `DashMap<Uuid, TcpStream>` and a 10s pending-conn TTL.
  **That per-connection model is gone** — replaced by yamux multiplexing.
- Companion docs: `CLAUDE.md` (architecture cheat-sheet, kept current),
  `README.md` (user-facing), `TEST_UDP.md` (e2e UDP test scenarios),
  `NAT_TRAVERSAL.md` (Italian: hole-punch internals + full provider×consumer NAT
  matrix + admin remediation). Agent memories also exist under the session memory
  dir: `yamux-lazy-open-gotcha`, `tls-uses-ring-for-musl`,
  `e2e-tests-fixed-control-port`.

## Build / test / verify (the no-regression gate)

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings                 # warnings are errors (CI gate)
cargo clippy --no-default-features --all-targets -- -D warnings  # udp-off build must also lint
cargo test                                    # all suites (udp on by default)
cargo audit                                   # must stay clean (0 vulns/warnings)
```

The `udp` feature (UDP hole-punching, pulls `quinn`) is **on by default**, so
plain `cargo build`/`test` include it. Build/test `--no-default-features` to
exercise the lean (relay-only) build. CI `ci.yml` builds & tests `--all-features`
on the host; the cross matrix (`mean_bean_*`) builds **with** default features
(so release binaries include `udp`) but tests **without** it (cross `ci/test.bash`
passes `--no-default-features`) to avoid QUIC flakiness under qemu emulation.

**Workflows run on every branch** (plus `v*` tags). `mean_bean_deploy.yml`
creates a GitHub Release per push (`create-release` job → `softprops/action-gh-release`,
named via `ci/version.bash`: `<branch>-<sha7>` pre-release, or the tag = latest)
and the matrix jobs upload each target binary as a release asset. `docker.yml`
pushes an **amd64-only** image to GHCR Packages (tagged by branch + sha; arm64
dropped — QEMU emulation cost ~20 min, `just push` still builds multi-arch). Branch
builds create a lightweight tag `<branch>-<sha7>` (releases require a tag); it
doesn't match `v*` and `GITHUB_TOKEN` can't re-trigger workflows → no loop, but
tags/releases accumulate per push (prune if noisy).

**Tests bind real ports and the e2e/secret suites use the fixed `CONTROL_PORT`
(7835).** Two hard-won testing rules:

1. **A hung test looks identical to a slow one under `cargo test`** (output is
   buffered, lost on kill). To find a hang, run the test *binary* directly with a
   hard timeout and read the exit code (137 = SIGKILL = hang):
   ```bash
   cargo test --no-run
   BIN=$(ls -t target/debug/deps/e2e_test-* | grep -v '\.d$' | head -1)
   timeout -s KILL 60 "$BIN" 2>&1 | grep "test result"
   ```
2. **`pkill -f e2e_test` kills your own shell** (its argv contains the pattern).
   Don't. Kill test processes via `timeout -s KILL` around the binary instead.
3. The shell snapshot has `set -e`; prefix throwaway scripts with `set +e`.
4. **Never put backticks in a `git commit -m "..."` message** — bash runs them as
   command substitution and the commit aborts. Use `git commit -F -` with a
   heredoc.

Current test inventory includes `e2e_test`, `auth_test`, `mux_test`,
`secret_test`, `control_port_test`, `tls_test`, `reconnect_test`, carrier and
admin/basic-auth suites, plus `udp_test` (`#![cfg(feature = "udp")]`) covering
direct round-trip, many native QUIC streams, multi-consumer direct, reconnect,
provider drop detection, relay→direct upgrade, fallback, max-conns, and the
paired `bore test-udp --tcp-secret-id` diagnostic (`paired_test_udp_diagnostic_exercises_direct_and_relay`).
Lib unit tests cover transport/reconnect/shared/holepunch/udp_diagnostic helpers,
plus doctests.

## Architecture (after the rewrite)

Client and server share **one** long-lived connection on the control port and
multiplex everything over it with yamux. No per-connection TCP/auth handshake.

Modules in `src/`:

- **`shared.rs`** — wire protocol. `ClientMessage` (`Authenticate`, `Hello(u16,
  TunnelOptions)`, `HelloSecret(String)`, `ConnectSecret(String)`),
  `ServerMessage` (`Challenge`, `Hello(u16)`, `Ok`, `Heartbeat`, `Error`),
  `TunnelOptions { https, force_https }`, the null-delimited-JSON `Delimited<U>`
  transport, and constants `CONTROL_PORT=7835`, `MAX_FRAME_LENGTH=1024` (raised
  from 256 to fit udp candidate lists), `NETWORK_TIMEOUT=3s`,
  `PROXY_BUFFER_SIZE=64 KiB`, `UDP_NONCE_LEN=16`. (serde_json is name-tagged, so
  adding enum variants is backward-compatible.) The `udp` feature adds
  `ClientMessage::UdpCandidates(Vec<SocketAddr>)` and `ServerMessage::UdpPunch
  { nonce, peer }` / `UdpUnavailable` for direct-path signaling. Later additions
  include carrier-pool messages, admin/basic-auth/notes fields, and paired
  diagnostics (`TestUdpJoin`, `TestUdpWaiting`, `TestUdpStart`).
- **`mux.rs`** — yamux wrapper, generic over a `Transport` trait (`AsyncRead +
  AsyncWrite + Unpin + Send + 'static`, so TCP or TLS). A single driver task owns
  `yamux::Connection` (its poll API needs `&mut`); `Opener::open()` requests
  outbound substreams over a channel, `Acceptor::accept()` yields inbound.
  `Stream = Compat<yamux::Stream>` (yamux is futures-IO; `tokio_util::compat`
  adapts it). `STREAM_READY` marker byte — see gotchas.
- **`transport.rs`** — control-connection endpoint. `Endpoint::parse(--to)`:
  `https://`→TLS:443, `http://`→plain:80, bare→plain:`CONTROL_PORT`; explicit
  `:port` overrides. `connect()` dials + optionally wraps TLS (rustls, **ring**
  provider; `--insecure` skips verification else webpki-roots). `ControlStream`
  is the plain-or-TLS enum. `load_server_tls`/`server_tls_from_pem` build the
  server `TlsAcceptor` (PEM via `rustls-pki-types`). Has unit tests.
- **`edge.rs`** — per-connection handling on the **public tunnel port** when a
  tunnel set `--https`/`--force-https`. Peeks first bytes (bounded by a timeout;
  **fast-path forwards immediately when no options set** — critical): `0x16` TLS
  ClientHello → terminate with server cert; plain HTTP request + `force_https` →
  `308` redirect to `https://`; else forward plain. `TunnelStream` plain-or-TLS.
- **`server.rs`** — `Server`: accepts the single connection (optional TLS
  handshake off the accept path), dispatches on the first control message into
  three roles: public-port tunnel (`serve_tunnel`), secret provider, secret
  consumer, plus newer carrier-join and paired UDP diagnostic roles. Holds the
  `providers` registry, `udp_tests` registry, `conn_permits` semaphore
  (`--max-conns`), `control_port`, optional `tls` acceptor, `bind_domain`.
- **`client.rs`** — `Client::new` (public-port mode) and
  `Client::new_secret_provider` (secret provider); both share `listen` /
  `handle_connection`. `connect_with_timeout` (pub(crate)) sets TCP_NODELAY.
- **`secret.rs`** — named secret tunnels (no public port). Server-side
  `serve_provider` (register under id in `Registry = Arc<DashMap<String,
  mux::Opener>>`) / `serve_consumer` + `relay` (splice consumer substream to a
  provider substream). Consumer-side `Proxy` (`bore proxy`): binds a local
  listener, opens one substream per local connection. **udp direct path:**
  `UdpRegistry`/`UdpReg` (provider candidates + an `mpsc` back-channel to the
  provider task), `broker_udp` (server brokers candidates + a nonce between the
  two peers), `negotiate_direct_consumer` (consumer gathers/exchanges/connects
  QUIC). `Proxy::is_direct()` reports whether the direct path was selected. The
  server-side brokering compiles **without** the `udp` feature (no quinn).
- **`holepunch.rs`** (always compiled; quinn parts gated on `udp`) — UDP
  hole-punching + STUN with a QUIC carrier. No-quinn parts (so the server can
  rendezvous in a lean build): `discover_reflexive`/`resolve_stun`/
  `gather_candidates`, the `stun` submodule (RFC 5389 client + responder),
  `run_stun_responder`, `derive_token` (HMAC(secret, nonce)). Gated quinn parts:
  `connect_direct` (consumer/QUIC-client), `DirectListener` (provider/QUIC-server),
  `QuicTransport` (a `mux::Transport` so yamux runs over one QUIC bidi stream),
  rustls configs (accept-any cert; token authenticates). `resolve_stun` maps a
  443/80 control port to the control port `7835` for the STUN default.
- **`udp_diagnostic.rs`** — paired `bore test-udp --tcp-secret-id` mode: server
  pairs two diagnostic peers, relays TCP diagnostic substreams between them, and
  clients test direct QUIC plus TCP fallback with latency and optional bandwidth
  probes (`--test-bandwidth`, alias `--test-bandwith`).
- **`reconnect.rs`** — `Backoff` (1,2,4,8,16,32 then 32s; reset on success) and
  generic `run(auto, connect, serve)` — single-shot (errors propagate, original
  behaviour) or infinite reconnect loop. Has unit tests.
- **`auth.rs`** — unchanged `Authenticator` (HMAC-SHA256 challenge/response), now
  run **once** on the control substream.
- **`main.rs`** — clap CLI: `local`, `proxy`, `server`, `test-udp`. Builds
  connect/serve closures and routes long-running client/proxy modes through
  `reconnect::run`; `test-udp` dispatches to standalone NAT diagnosis or paired
  A<->B diagnostics depending on `--tcp-secret-id`.

### Connection flow

1. Client dials the control port, opens the control substream, sends its first
   message **before** authenticating (`Hello`/`HelloSecret`/`ConnectSecret`),
   then (if a secret is set) does the auth challenge/response. Server validates,
   then acts.
2. Public-port tunnel: server binds a tunnel port, sends `Hello(actual_port)`,
   heartbeats every 500ms; for each external connection it acquires a permit,
   runs `edge::accept`, opens a data substream, writes `STREAM_READY`, splices.
3. Client accepts data substreams, consumes the marker, dials the local service,
   splices with `copy_bidirectional_with_sizes`.
4. Secret tunnel: provider registers under an id (no port); consumer opens a
   substream per local connection; server relays each to the provider. Direction
   is inverted vs. public-port (consumer opens, server accepts).

## What changed, by feature (chronological)

Each bullet = one or more commits on `perf-hardening`.

1. **Safety-net tests** (`f14a74b`): large-payload (1 MiB) + concurrency e2e
   tests as a regression baseline.
2. **Perf** (`54cd2a4`): `TCP_NODELAY` on all stream endpoints,
   `copy_bidirectional_with_sizes(64 KiB)`, `[profile.release]` lto +
   codegen-units=1 + strip. Bumped tokio→1.41+ (needed for `_with_sizes`).
3. **Robustness** (`03a0c98`): tunnel accept loop no longer dies on a transient
   `accept()` error (logs + backs off); fixed swapped `set_bind_addr/tunnels`
   docs.
4. **DoS bounding** (`3c88bfb`): `--max-conns` semaphore caps concurrent proxied
   connections (was unbounded). Also fixed a pre-existing parallel-test flake via
   `wait_for_control_port` (see gotchas).
5. **CVE remediation** (`ceec988`): bumped clap/fastrand/futures-util; `cargo
   audit` clean.
6. **yamux multiplexing** (`e241c61`): the big rewrite — see Architecture.
7. **Secret tunnels** (`999b9db`, `fe20c84`): `bore local --tcp-secret-id` (provider,
   no public port) + `bore proxy --local-proxy-port :PORT --tcp-secret-id` (consumer).
8. **Configurable control port** (`6f7d797`): `--control-port`; `Endpoint::parse`.
9. **TLS on the control connection** (`a53dd3b`): `https://`/`http://` schemes,
   `--insecure`, server `--cert-file`/`--key-file`/`--bind-domain`. rustls/ring.
10. **TLS termination on the tunnel port** (`a458fcd`, `c052c0a`): `--https`
    (terminate TLS on the public port, plain+raw still work) and `--force-https`
    (308 redirect HTTP→HTTPS). `edge.rs`.
11. **Auto-reconnect** (`f73ee0a`, `1da7d4c`): `--auto-reconnect` for `local` and
    `proxy` with the backoff above. `reconnect.rs`.
12. **Docker/justfile** (`a159bec`, `06ed233`, `3eaf109`): see below.
13. **Connection stability / keepalive**: `shared::tune_tcp` (socket2) sets
    `TCP_NODELAY` + `SO_KEEPALIVE` (15s idle/interval) on every accepted/dialed
    socket (client/server control, public tunnel external socket, secret-proxy
    local socket, local dial). No code path times out an established data
    stream; this protects long, quiet transfers from middlebox idle-drops.
14. **UDP hole-punching direct path** (`udp` feature, default-on): for secret
    tunnels, provider and consumer establish a **direct** peer-to-peer QUIC path
    via UDP hole-punching + STUN, with the server only as signaling/STUN and
    automatic fallback to the relay on any failure. New `holepunch.rs`, signaling
    messages, server STUN responder + brokering, `--udp`/`--stun-server` flags.
    yamux runs over one QUIC bidi stream (`QuicTransport: mux::Transport`), so the
    per-connection data path is reused unchanged. Token = HMAC(secret, **stable
    per-provider** nonce) verified on the first 32 bytes of the QUIC stream. The
    provider keeps a persistent `DirectListener` and re-punches (`punch_via_endpoint`)
    toward each new/reconnecting consumer, so reconnects and multiple consumers
    work. **Resilience:** the consumer detects the direct path dying (provider
    restart) via the direct mux acceptor and reconnects; a relay-mode consumer
    retries the direct negotiation every 10s and upgrades in place when the
    provider becomes reachable, so it always converges to direct without dropping.
    Only secret tunnels (not public-port); both peers symmetric-NAT → relay.
    **Hard-NAT extras** (opt-in, on `local` + `proxy`): `--upnp` (UPnP-IGD router
    port mapping via `igd-next`; home routers only, not CGNAT) and
    `--try-port-prediction` (advertise predicted symmetric-NAT ports; best-effort,
    logged, may look like a scan). See `TEST_UDP.md`.
15. **`bore test-udp` diagnostic** (`holepunch::diagnose`, compiles without the
    `udp` feature): opens no tunnel; probes public STUN (Google×2 + Cloudflare) on
    one socket plus, with `--to`, the bore server's own STUN responder, and prints
    a verdict. `classify_nat` (pure, unit-tested) reads the mapping variation
    across servers → `Blocked`/`Open`/`Inconclusive`/`Cone`/`Symmetric{sequential}`;
    also reports port-preservation, CGNAT (`100.64/10`)/double-NAT, a
    co-location/hairpin note (public STUN OK but own server's UDP dead), and a
    UPnP-IGD presence probe. Companion to the elevated STUN-failure log in
    `gather_candidates` (now `warn!`, so a missing public candidate is visible).
16. **Fixed UDP hole-punch port** (`--nat-udp-preferred-port`/`BORE_NAT_UDP_PORT`,
    0=random): `bind_socket(port)` binds an exact UDP source port (`socket2` +
    `SO_REUSEADDR` for clean reconnect rebind). Open that one port for egress in a
    strict firewall (same value on both peers) and the direct path uses it; on a
    port-preserving NAT it also fixes the public mapping. No help for symmetric
    NATs. On `local`+`proxy`+`test-udp`. (41641 = Tailscale's default, a sane pick.)
17. **Direct-path hardening** (from a third-party review, `COPILOT_ANALISYS.md`):
    (a) provider `--max-conns`/`BORE_MAX_CONNS` on `local` bounds concurrent
    **direct** substreams (`Semaphore` in `provider_direct`) — parity with the
    relay's server-wide cap, protecting the provider host. (b) The consumer's QUIC
    dial (`connect_direct`) now tries all candidates **concurrently under one total
    `NETWORK_TIMEOUT` budget** (`futures_util::select_ok`) instead of N×3s serial.
    (c) The relay→direct upgrade runs in a spawned `upgrade_task` so `Proxy::listen`
    never stalls the accept/forward loop (control I/O stays in the loop via
    `cand`/`oneshot`/`done` channels). (d) The provider re-offers UDP candidates on
    a 15s timer if the initial offer failed (was one-shot). (e) re-punch channel is
    now `unbounded` (was `mpsc(8)` + `try_send`, could silently drop under a burst).
    (f) the direct-path session nonce + STUN txid use the system CSPRNG (`ring::rand`,
    was `fastrand`); the client warns on `--udp` without `--secret`. IPv6 dual-stack
    (the review's remaining item) is deliberately deferred.
18. **Production-grade pass** (toward 1.0.0): version `1.0.0`, crate metadata now
    identifies the fork (`repository = manprint/bore`, `authors`, no stale
    `documentation`). **Logging** rebuilt (`init_logging`): `EnvFilter` default
    `info` with `-v`/`-vv` (debug/trace) and `RUST_LOG` override, to **stderr**,
    ANSI **only on a TTY** (no escape-code junk in Docker/journald/files); the
    per-attempt "consumer offering udp candidates" line dropped to `debug` (the
    10s upgrade loop was spamming `info`). **Graceful shutdown** on Ctrl-C +
    SIGTERM (`tokio::signal`) → clean exit with a log line. **Help** made uniform:
    short `value_name`s on every flag so all subcommands render the same compact
    layout. New `CHANGELOG.md` + `CONTRIBUTING.md`; README help blocks synced.
19. **Basic auth, notes, and an admin status page** (new modules `admin.rs`,
    `admin_http.rs`, `basicauth.rs`, `prefixed.rs`; `admin_status.html` embedded):
    - **`--basic-auth user:pass`** (`local`, `BORE_BASIC_AUTH`) — HTTP Basic auth.
      Public tunnels enforced **server-side** in `edge.rs` (creds travel in
      `TunnelOptions`); secret tunnels enforced **provider-side** in
      `client::handle_connection` (relay + direct; creds stay local). `basicauth::
      gate` reads the HTTP head, returns `401` or forwards it via `Prefixed`; non-
      HTTP is passed through unprotected. Hand-rolled base64 + constant-time compare
      (no new dep). `ClientMessage` grew struct variants (`HelloSecret`/
      `ConnectSecret`) and `TunnelOptions` grew `basic_auth`/`notes`; `MAX_FRAME_
      LENGTH` is 1024 (notes clamped to `MAX_NOTES_LEN = 256`).
    - **`--notes`** (`local` + `proxy`, `BORE_NOTES`) — operator label for the page.
    - **`--admin-token`** (`server`, `BORE_ADMIN_TOKEN`, ≥32 chars) — enables a
      read-only dashboard at `/admin/status` on the control port (http/https per the
      control scheme). `server::route_connection` peeks the first byte (HTTP →
      `admin_http`, else bore protocol via `Prefixed`); **disabled = exact original
      path**. `AdminRegistry` is in-memory/stateless with RAII deregistration;
      `serve_*`/`serve_tunnel` register entries and count live connections. The page
      polls JSON every ~2s, is embedded (`include_str!`), fetches no external assets.
20. **Carrier pool for public tunnels** (`--carriers N` on `local`, `BORE_CARRIERS`;
    `--max-carriers` on `server`, `BORE_MAX_CARRIERS`, default
    `DEFAULT_MAX_CARRIERS = 16`): open `N` parallel TCP connections and round-robin
    proxied connections across them instead of multiplexing all over one TCP —
    removing yamux's single-connection head-of-line blocking and giving each carrier
    its own congestion window. For **concurrent** workloads (parallel rclone/S3/WebDAV,
    many web requests, streaming); a single bulk flow is unchanged. The server stays
    in a public tunnel's data path, so this is **not** the secret UDP direct path (no
    bypass, no bandwidth gain) — it only fixes the server↔client carrier bottleneck.
    - Protocol (additive, backward-compatible): `TunnelOptions.carriers: u16`
      (`#[serde(default)]`); `ServerMessage::CarrierToken { token, extra }` (after
      `Hello` when `carriers > 1`); `ClientMessage::JoinCarrier { token }` (first
      message on each extra connection, authenticated like `Hello`). Token is a random
      `Uuid`; an unknown token is rejected. **The data path is unchanged — only which
      connection opens each substream** — so `carriers <= 1` (incl. the default) is the
      original path byte-for-byte and the whole existing suite is the regression guard.
    - Server (`serve_tunnel`/`serve_carrier`): `pending_carriers` `DashMap<token,
      Sender<Carrier>>` matches `JoinCarrier`s to the tunnel; `pick_carrier`
      round-robins live `Opener`s (dead ones pruned via `AtomicBool`); a `TokenGuard`
      frees the token on teardown. Client (`open_carrier`/`spawn_carrier_pump`/
      `maybe_redial_carriers`): opens the extra connections, pumps each acceptor into
      a shared channel the listen loop drains like the main one, and re-dials a dropped
      carrier on a 15s timer (non-blocking, `inflight`-guarded).
    - Tests: `tests/carrier_test.rs` (round-trip × 4 carriers/40 conns, half-close,
      1 MiB payload, request-above-cap clamp, cap-1 degrade-to-single).
    - Not done: per-socket congestion control (`TCP_CONGESTION` needs `unsafe`
      `setsockopt`; use host `sysctl net.ipv4.tcp_congestion_control=bbr`).
21. **Carrier pool extended to secret tunnels + native-QUIC direct path.** The pool
    primitives moved to a shared `src/pool.rs` (`Carrier`, `CarrierPool` with
    thread-safe round-robin `pick()`, `PendingCarriers`, `TokenGuard`, `recv_carrier`);
    `server::serve_tunnel` was refactored onto it. `--carriers` now applies to all
    three relay legs:
    - **Secret provider** (`bore local --tcp-secret-id --carriers`): `HelloSecret`
      gained `carriers: u16`; `Registry` is now `DashMap<id, Arc<CarrierPool>>`;
      `serve_provider` issues a `CarrierToken` (reusing the same `pending_carriers`/
      `serve_carrier` path) and adds joined provider connections to the pool; `relay`
      round-robins (`pool.pick()`). The provider client reuses `open_carrier`/
      `spawn_carrier_pump`/re-dial.
    - **Secret consumer** (`bore proxy --carriers`): opens N `ConnectSecret`
      connections (`open_consumer_carrier`, each drained of heartbeats + pruned on
      drop) into a client-side `CarrierPool`; `Proxy`'s new `DataPath::Relay(pool)`
      round-robins `forward` across them. No server change (reuses multi-consumer).
    - **Native QUIC direct streams** (leg 3): `holepunch::connect_direct`/
      `DirectListener::accept` now return an authenticated `DirectConn` (token on a
      dedicated stream) instead of a single-stream `QuicTransport`; each proxied
      connection rides its **own** QUIC bidi (`open_stream`/`accept_stream`), removing
      HOL on the direct path (was yamux-over-one-stream). `Client::handle_connection`
      is now generic over the stream type; `provider_direct` loops `accept_stream`.
      `Proxy`'s `DataPath` is `Relay(CarrierPool)` | `Direct(DirectConn)`; `DataStream`
      is the `AsyncRead`/`AsyncWrite` enum; direct death is detected via
      `DirectConn::closed()`; the relay→direct upgrade swaps the `DataPath` in place
      (a `DirectUpgrade`/`Infallible` alias keeps the upgrade channels nameable without
      the `udp` feature). `transport_config` raises `max_concurrent_bidi_streams` to
      `MAX_DIRECT_STREAMS`.
    - Tests: `tests/secret_pool_test.rs` (provider pool, consumer pool, both pools);
      `tests/udp_test.rs::udp_direct_many_concurrent_streams`; all existing `udp_test`
      cases still pass over the native-stream path.

## CLI flags & env vars (all flags read env where present)

- **server:** `--min-port`/`BORE_MIN_PORT`, `--max-port`/`BORE_MAX_PORT`,
  `-s`/`BORE_SECRET`, `--max-conns`/`BORE_MAX_CONNS`,
  `--control-port`/`BORE_CONTROL_PORT` (default 7835),
  `--bind-domain`/`BORE_BIND_DOMAIN`, `--cert-file`/`BORE_CERT_FILE`,
  `--key-file`/`BORE_KEY_FILE`, `--bind-addr`, `--bind-tunnels` (last two: no env),
  `--udp`/`BORE_UDP` (broker direct paths + STUN responder on the control port/UDP),
  `--max-carriers`/`BORE_MAX_CARRIERS` (cap on a tunnel's carrier pool, default 16),
  `--admin-token`/`BORE_ADMIN_TOKEN` (admin status page, ≥32 chars).
- **local:** positional `LOCAL_PORT`/`BORE_LOCAL_PORT`, `--local-host` (no env),
  `--to`/`BORE_SERVER`, `--port` (no env), `-s`/`BORE_SECRET`,
  `--tcp-secret-id`/`BORE_TCP_SECRET_ID`, `--insecure`/`BORE_INSECURE`,
  `--https`/`BORE_HTTPS`, `--force-https`/`BORE_FORCE_HTTPS` (requires `--https`),
  `--udp`/`BORE_PREFER_UDP`, `--stun-server`/`BORE_STUN_SERVER`,
  `--upnp`/`BORE_UPNP`, `--try-port-prediction`/`BORE_TRY_PORT_PREDICTION`,
  `--nat-udp-preferred-port`/`BORE_NAT_UDP_PORT` (fixed UDP hole-punch port, 0=random),
  `--max-conns`/`BORE_MAX_CONNS` (direct-path concurrency cap, default 1024),
  `--basic-auth`/`BORE_BASIC_AUTH`, `--notes`/`BORE_NOTES`,
  `--carriers`/`BORE_CARRIERS` (parallel TCP relay carriers, default 1),
  `--auto-reconnect`/`BORE_AUTO_RECONNECT`.
- **proxy:** `--local-proxy-port`/`BORE_LOCAL_PROXY_PORT` (`:5555` = all
  interfaces), `--to`/`BORE_SERVER`, `-s`/`BORE_SECRET`,
  `--tcp-secret-id`/`BORE_TCP_SECRET_ID`, `--insecure`/`BORE_INSECURE`,
  `--udp`/`BORE_PREFER_UDP`, `--stun-server`/`BORE_STUN_SERVER`,
  `--upnp`/`BORE_UPNP`, `--try-port-prediction`/`BORE_TRY_PORT_PREDICTION`,
  `--nat-udp-preferred-port`/`BORE_NAT_UDP_PORT` (fixed UDP hole-punch port, 0=random),
  `--notes`/`BORE_NOTES`, `--carriers`/`BORE_CARRIERS` (parallel relay carriers, default 1),
  `--auto-reconnect`/`BORE_AUTO_RECONNECT`.
- **test-udp:** `--to`/`BORE_SERVER` (optional), `--stun-server`/`BORE_STUN_SERVER`,
  `--nat-udp-preferred-port`/`BORE_NAT_UDP_PORT`.

## Dependencies added

`yamux`, `tokio-rustls` (**ring** provider, NOT aws-lc-rs), `webpki-roots`,
`tokio-util` `compat` feature, tokio `sync` feature; dev: `rcgen` (self-signed
certs in tls tests). `rustls-pemfile` was deliberately NOT used (unmaintained,
RUSTSEC-2025-0134) — PEM parsing uses `rustls-pki-types`.

Under the **`udp` feature (default-on)**: `quinn` (0.11, `rustls-ring` +
`runtime-tokio`, shares rustls 0.23 with tokio-rustls), `rcgen` (promoted to an
optional normal dep for the self-signed QUIC cert), and `igd-next` (0.16,
`aio_tokio`; UPnP-IGD for `--upnp`). `[features] default = ["udp"]`;
`udp = ["dep:quinn", "dep:rcgen"]`. quinn/quinn-udp internally use `unsafe`, which
is fine — `#![forbid(unsafe_code)]` constrains only our crate, not deps. quinn
cross-compiles on all CI targets (verified: arm-gnueabi, arm-musleabi, i686-musl,
aarch64-musl, android via NDK).

## Docker & justfile

- `docker/docker-compose.{server,client,secret-proxy}.yml`: server uses a bridge
  network with explicit port forwards (control port + tunnel range; commented
  80/443 lines — scheme depends on the cert, not the port); client and
  secret-proxy use `network_mode: host`. All env vars present (optional ones
  commented). `image: ${BORE_IMAGE:-yourusername/bore:latest}`.
- `Dockerfile` (root): unchanged scratch/musl build, now also installs
  `build-base` so `ring` compiles on Alpine.
- `docker/Dockerfile.cross`: `cargo-zigbuild` builder for non-Linux targets
  (macOS, Windows). NOTE: zig cannot build `ring` for Android, so Android uses a
  separate `docker/Dockerfile.android` with the Android NDK as the C toolchain.
- `justfile` (repo `repo := "fabiop85/bore"`): `build-amd64`/`build-arm64`
  (Linux, `docker buildx --platform`), `macos-m5` (aarch64-apple-darwin) and
  `windows-amd64` (x86_64-pc-windows-gnu) via `cargo-zigbuild`, and `android-arm64`
  (aarch64-linux-android, via the NDK Dockerfile; `android_api` var), all output
  to `./bin/` (gitignored). `push` builds + pushes a multi-arch (amd64+arm64) image.
  `_builder` creates a `docker-container` buildx builder; `setup-qemu` registers
  binfmt for arm64 emulation.

## Critical gotchas (do not regress these)

- **yamux opens substreams lazily** — the peer sees nothing until the opener
  writes. Two rules depend on this: (a) the client sends `Hello` *before* auth
  (server speaks first during auth), and (b) the data-substream opener writes a
  `STREAM_READY` marker byte that the acceptor consumes before splicing.
  Removing either reintroduces a deadlock that passes simple echo tests but hangs
  on EOF/half-close or no-initial-data paths.
- **`edge::accept` fast-path** — when neither `--https` nor `--force-https` is
  set, forward immediately WITHOUT peeking. Peeking blocks until the remote peer
  sends data, deadlocking server-speaks-first protocols (it hung the e2e suite
  once). Even the `--https` peek is timeout-bounded for the same reason.
- **TLS uses the rustls `ring` provider** (configs built with
  `builder_with_provider(ring::default_provider())`), so the musl/scratch Docker
  build keeps working. Do not switch to the default provider or `install_default`.
- **e2e/secret tests share the fixed control port** — any new server-spawning
  test must gate on `wait_for_control_port(false/true)` (free, then up) or it
  flakes under the parallel runner.
- **`udp` STUN responder binds the control port over UDP** — a TLS control
  server on `:443` (`https://`) still runs STUN on `7835/udp`, so `resolve_stun`
  maps 443/80 → `CONTROL_PORT`. Open **UDP** on the control port, not just TCP.
- **plain `host:port` vs TLS server** — a bare `--to host:port` is plaintext; a
  TLS control server drops it (the client now reports "connection closed before
  authentication — wrong --to scheme?"). Use `https://host[:port]`.
- **cross CI tests run `--no-default-features`** (relay-only) to avoid QUIC-over-
  qemu flakiness; the cross *build* still compiles `udp` and the host CI tests
  `--all-features`. Don't "fix" a missing udp_test on cross — it's intentional.
- **Multi-consumer per provider id is a supported invariant** — many `bore proxy`
  consumers may attach to one provider, on the relay and the direct path. Two
  things keep it working: the **stable per-provider nonce** (`UdpReg.nonce`, minted
  once when the provider offers) so every consumer derives the *same* direct-path
  token, and the **single persistent `DirectListener`** whose `accept` loop takes
  one QUIC connection per consumer. Do not regress to a per-consumer nonce or a
  per-consumer endpoint — it breaks the 2nd consumer and reconnects. Covered by
  `secret_multiple_consumers_concurrent`, `udp_multiple_consumers_concurrent_direct`,
  `udp_mixed_direct_and_relay_consumers`, `udp_consumer_reconnects_while_others_active`,
  `udp_multiple_consumers_detect_provider_drop`.
- **The admin page must never change the protocol path when disabled** — with no
  `--admin-token`, `server::route_connection` is a pure pass-through to
  `handle_connection`; only a configured token enables the first-byte peek. Keep it
  that way (the peek uses a single `read` + `Prefixed` replay so no bytes are lost,
  and `0x00`/non-HTTP always falls through to the bore protocol). Basic auth is
  **HTTP-only by design**: non-HTTP connections are forwarded unprotected, so do not
  rely on `--basic-auth` to gate raw-TCP services.

## Known limitations / candidate next steps

- `Endpoint::parse` splits on the last `:`, so **bare IPv6 literal hosts are not
  handled** (e.g. `https://[::1]:443`). Add bracket handling if needed.
- macOS cross-build CPU: no toolchain ships `apple-m5` yet, so
  `macos_target_cpu` defaults to `apple-m4`; bump when toolchains support it.
- No graceful shutdown / connection draining on the server; no metrics.
- `--max-conns` bounds concurrency per client connection, not globally.
- Possible next work: IPv6 in endpoint parsing, per-tunnel auth scopes,
  observability (counts of active tunnels/streams), SNI-based cert selection if
  multiple domains are ever needed.

## Conventions

- Commit style: Conventional Commits (`feat:`, `fix:`, `test:`, `chore:`,
  `docs:`, `deps:`), body via `git commit -F -` heredoc (no backticks inline),
  ending with the `Co-Authored-By` trailer.
- Every change keeps `cargo fmt --check`, `cargo clippy -- -D warnings`, the full
  test suite, and `cargo audit` green. Work in phases, commit per phase, add/keep
  tests for each. No regressions vs. the baseline.
