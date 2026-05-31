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
  `README.md` (user-facing). Agent memories also exist under the session memory
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
pushes the multi-arch image to GHCR Packages (tagged by branch + sha). Branch
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

Current test inventory: `e2e_test` (13 runs), `auth_test` (2), `mux_test` (2),
`secret_test` (7), `control_port_test` (1), `tls_test` (5), `reconnect_test` (2),
`udp_test` (5, `#![cfg(feature = "udp")]` — direct round-trip, consumer
reconnect, consumer detects provider drop, **relay→direct upgrade**, relay
fallback on loopback), lib unit tests (14: `transport.rs` 7, `reconnect.rs` 2,
`shared.rs` 1, `holepunch.rs` 4), plus 1 doctest. Baseline before this work was 12 e2e + 2 auth
+ 1 doctest.

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
  { nonce, peer }` / `UdpUnavailable` for direct-path signaling.
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
  consumer. Holds the `providers` registry, `conn_permits` semaphore
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
- **`reconnect.rs`** — `Backoff` (1,2,4,8,16,32 then 32s; reset on success) and
  generic `run(auto, connect, serve)` — single-shot (errors propagate, original
  behaviour) or infinite reconnect loop. Has unit tests.
- **`auth.rs`** — unchanged `Authenticator` (HMAC-SHA256 challenge/response), now
  run **once** on the control substream.
- **`main.rs`** — clap CLI: `local`, `proxy`, `server`. Builds connect/serve
  closures and routes through `reconnect::run`.

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
    See `TEST_UDP.md`.

## CLI flags & env vars (all flags read env where present)

- **server:** `--min-port`/`BORE_MIN_PORT`, `--max-port`/`BORE_MAX_PORT`,
  `-s`/`BORE_SECRET`, `--max-conns`/`BORE_MAX_CONNS`,
  `--control-port`/`BORE_CONTROL_PORT` (default 7835),
  `--bind-domain`/`BORE_BIND_DOMAIN`, `--cert-file`/`BORE_CERT_FILE`,
  `--key-file`/`BORE_KEY_FILE`, `--bind-addr`, `--bind-tunnels` (last two: no env),
  `--udp`/`BORE_UDP` (broker direct paths + STUN responder on the control port/UDP).
- **local:** positional `LOCAL_PORT`/`BORE_LOCAL_PORT`, `--local-host` (no env),
  `--to`/`BORE_SERVER`, `--port` (no env), `-s`/`BORE_SECRET`,
  `--tcp-secret-id`/`BORE_TCP_SECRET_ID`, `--insecure`/`BORE_INSECURE`,
  `--https`/`BORE_HTTPS`, `--force-https`/`BORE_FORCE_HTTPS` (requires `--https`),
  `--udp`/`BORE_PREFER_UDP`, `--stun-server`/`BORE_STUN_SERVER`,
  `--auto-reconnect`/`BORE_AUTO_RECONNECT`.
- **proxy:** `--local-proxy-port`/`BORE_LOCAL_PROXY_PORT` (`:5555` = all
  interfaces), `--to`/`BORE_SERVER`, `-s`/`BORE_SECRET`,
  `--tcp-secret-id`/`BORE_TCP_SECRET_ID`, `--insecure`/`BORE_INSECURE`,
  `--udp`/`BORE_PREFER_UDP`, `--stun-server`/`BORE_STUN_SERVER`,
  `--auto-reconnect`/`BORE_AUTO_RECONNECT`.

## Dependencies added

`yamux`, `tokio-rustls` (**ring** provider, NOT aws-lc-rs), `webpki-roots`,
`tokio-util` `compat` feature, tokio `sync` feature; dev: `rcgen` (self-signed
certs in tls tests). `rustls-pemfile` was deliberately NOT used (unmaintained,
RUSTSEC-2025-0134) — PEM parsing uses `rustls-pki-types`.

Under the **`udp` feature (default-on)**: `quinn` (0.11, `rustls-ring` +
`runtime-tokio`, shares rustls 0.23 with tokio-rustls) and `rcgen` (promoted to an
optional normal dep for the self-signed QUIC cert). `[features] default = ["udp"]`;
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
