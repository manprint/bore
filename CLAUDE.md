# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`bore` is a minimal TCP tunnel: a client exposes a local port to the public internet through a remote server, bypassing NAT/firewalls. The whole thing is ~400 lines of safe async Rust (`#![forbid(unsafe_code)]`). The crate ships both the library (`bore_cli`) and a single `bore` binary that runs as either client or server.

## Commands

```shell
cargo build --all-features        # build (CI builds with --all-features)
cargo test                        # run all tests
cargo test basic_proxy            # run a single test by name
cargo fmt -- --check              # rustfmt check (CI gate)
cargo clippy -- -D warnings       # lint, warnings are errors (CI gate)

cargo run -- local 8000 --to bore.pub      # run client
cargo run -- server                        # run server
```

CI (`.github/workflows/ci.yml`) runs three separate jobs: build+test, `cargo fmt --check`, and `cargo clippy -D warnings`. All three must pass.

### Testing caveats

- **Integration tests bind real ports and must run serially.** `tests/e2e_test.rs` spins up an actual `Server` on `CONTROL_PORT` (7835) plus tunnel ports. Tests share a `SERIAL_GUARD` mutex (`lazy_static`) to avoid port races — any new test that starts a server must take this lock. This means tests fail if port 7835 is already in use.
- Tests use `rstest` for parameterized cases (e.g. `basic_proxy` runs across `None`/`Some("")`/`Some("abc")` secrets).
- Doctests exist (see `auth.rs`) and run under `cargo test`.
- **Multi-consumer is a supported invariant and is tested**: many `bore proxy` consumers may attach to one provider id, on the relay *and* the UDP direct path. Coverage — relay: `secret_multiple_consumers_concurrent` (3 concurrent consumers, distinct payloads, no cross-talk), `secret_single_consumer_many_connections` (16 conns over one consumer); direct: `udp_multiple_consumers_concurrent_direct` (3 direct on one provider endpoint via the stable per-provider nonce), `udp_mixed_direct_and_relay_consumers` (direct + relay against the same provider), `udp_consumer_reconnects_while_others_active`, `udp_multiple_consumers_detect_provider_drop`. The direct path scales because the provider runs one persistent `DirectListener` (one QUIC endpoint) that accepts every consumer's connection; the stable nonce means all derive the same token.

## Architecture

The client and server share **one** long-lived connection (on the control port, `7835` by default, `--control-port` to change) and multiplex everything over it with yamux. The connection is plain TCP, or TLS when the client's `--to` is an `https://` URL. There is no longer a separate connection (or auth handshake) per proxied connection.

Modules under `src/`:

- **`shared.rs`** — control-channel protocol. `ClientMessage`/`ServerMessage` enums (serde JSON) and the `Delimited<U>` transport (null-byte-delimited JSON frames via `AnyDelimiterCodec`). Constants `CONTROL_PORT`, `MAX_FRAME_LENGTH = 256`, `NETWORK_TIMEOUT = 3s`, `PROXY_BUFFER_SIZE = 64 KiB`.
- **`mux.rs`** — yamux wrapper, generic over any `Transport` (the `AsyncRead+AsyncWrite+Unpin+Send+'static` blanket trait — TCP or TLS). `mux::client`/`mux::server` spawn a single driver task that owns the `yamux::Connection` (its poll API needs `&mut`, so one owner only). `Opener::open()` requests outbound substreams over a channel; `Acceptor::accept()` yields inbound ones. `Stream` is `Compat<yamux::Stream>` (yamux is `futures`-IO; `tokio_util::compat` adapts it to Tokio traits).
- **`server.rs`** — `Server`: accepts the single connection, dispatches on the first control message into one of three roles (public-port tunnel, secret provider, secret consumer). Holds the `providers` registry and the `--max-conns` `Semaphore`.
- **`client.rs`** — `Client`: dials the server, opens the control substream, accepts data substreams and splices each to a fresh local connection. `Client::new` = public-port mode; `Client::new_secret_provider` = secret-provider mode (shares `listen`/`handle_connection`). Provider direct-path hardening: a per-provider `Semaphore` (`--max-conns` on `local`, default `DEFAULT_MAX_CONNS`) bounds concurrently served **direct** substreams in `provider_direct` — the direct analog of the relay's server-wide cap (it protects the provider host; over the cap, drop). `listen` also re-offers UDP candidates on a 15s timer (`udp_cfg`) if the initial offer failed, so a transient bootstrap problem doesn't leave the provider relay-only.
- **`edge.rs`** — per-connection handling on the public tunnel port when a tunnel sets `--https`/`--force-https`. Peeks the first bytes (bounded by a timeout; a no-options tunnel skips peeking entirely and forwards as before): a TLS `ClientHello` (`0x16`) is terminated with the server cert (`TunnelStream::Tls`), a plain HTTP request is answered with a `308` redirect to `https://` when `force_https`, otherwise the connection is forwarded plain. `TunnelOptions` rides in the `Hello` message.
- **`secret.rs`** — named "secret" tunnels (no public port). Server-side `serve_provider` (register under id) / `serve_consumer` + `relay` (splice each consumer substream to a provider substream); `Registry = Arc<DashMap<id, mux::Opener>>`; and the consumer-side `Proxy` (`bore proxy`) which binds a local listener and opens one substream per local connection.
- **`transport.rs`** — control-connection endpoint. `Endpoint::parse` turns `--to` into host/port/tls (`https://`→TLS:443, `http://`→plain:80, bare→plain:control-port; explicit `:port` overrides). `connect` dials and, for TLS, wraps with rustls (**ring** provider, for musl/scratch builds; `--insecure` skips verification, else webpki-roots). `ControlStream` is the plain-or-TLS enum (implements `mux::Transport`); `load_server_tls`/`server_tls_from_pem` build the server `TlsAcceptor`.
- **`holepunch.rs`** — optional `udp` feature: UDP hole-punching + STUN with a QUIC carrier for a **direct** consumer↔provider path in secret tunnels (bypassing the relay). Split so the *server* parts (STUN reflexive discovery `discover_reflexive`/`resolve_stun`, STUN responder `run_stun_responder`, token `derive_token`) carry no `quinn` dependency and compile unconditionally; the *client* QUIC parts (`connect_direct`, `DirectListener`, `QuicTransport`, configs) are `#[cfg(feature = "udp")]` and pull `quinn`. `QuicTransport` is just another `mux::Transport`, so `yamux` runs over one QUIC bidi stream unchanged. Both peers authenticate the direct path with a shared token = HMAC(secret, server-issued nonce) exchanged on the first 32 bytes of the QUIC stream (the nonce is from the system CSPRNG via `ring::rand`, not a fast PRNG — it is the token's only entropy when no `--secret` is set; the client also warns when `--udp` is used without `--secret`). `udp` is a **default** feature (on for `cargo build`/`test`); build `--no-default-features` to drop `quinn`/`quinn-udp` (e.g. a target where `quinn` won't compile). The server-side brokering + STUN still compile without the feature. **Hard-NAT extras** in `gather_candidates` (opt-in, both peers): `--upnp`/`BORE_UPNP` adds a UPnP-IGD router-mapped candidate (`igd-next`, gated on `udp`; helps strict *home* routers, useless behind CGNAT); `--try-port-prediction`/`BORE_TRY_PORT_PREDICTION` advertises `PREDICT_RANGE` ports past the reflexive one for sequential symmetric NATs (best-effort, logs a `port prediction ENABLED` warning as it can look like a scan). Both flags are on `local` + `proxy` only (the server doesn't punch); CGNAT-both-ends stays on the relay. **Diagnostic** `diagnose` (behind `bore test-udp`, compiles without the feature): probes `PUBLIC_STUN` (Google×2 + Cloudflare) on one socket plus, with `--to`, the bore server's own STUN; `classify_nat` (pure, unit-tested) reads the mapping variation across servers → `Blocked`/`Open`/`Inconclusive`/`Cone`/`Symmetric{sequential}`; also reports port-preservation, `is_cgnat` (`100.64/10`)/double-NAT, a co-location/hairpin note (public STUN OK but own server's UDP dead), and a UPnP-IGD presence probe (`upnp_external_ip`, gated). Prints a human report via `println!` (not `tracing`). **Fixed UDP port**: `bind_socket(port)` (0 = ephemeral) — a non-zero `port` binds that exact UDP source port (via `socket2` with `SO_REUSEADDR` so an auto-reconnect rebinds cleanly), threaded from `--nat-udp-preferred-port`/`BORE_NAT_UDP_PORT` through `new_secret_provider`/`Proxy::new`/`negotiate_direct_consumer`/`diagnose`; lets a strict egress firewall be opened for that one port and fixes the public mapping on a port-preserving NAT (no help for symmetric NATs). On `local`+`proxy`+`test-udp`.
- **`reconnect.rs`** — `--auto-reconnect` support. `Backoff` yields 1,2,4,8,16,32 then 32s indefinitely (reset on a successful connect); generic `run(auto, connect, serve)` runs the connect/serve cycle once (errors propagate — the original behaviour) or loops forever reconnecting. Used by `local` (normal + provider) and `proxy` in `main.rs`.
- **`auth.rs`** — `Authenticator`: optional HMAC-SHA256 challenge/response, run **once** on the control substream.
- **`main.rs`** — clap CLI (`local` / `proxy` / `server` / `test-udp`). `test-udp` is a standalone NAT/UDP diagnostic (opens no tunnel; `--to`/`--stun-server`) → `holepunch::diagnose`. Flags also read env vars (`BORE_SERVER`, `BORE_SECRET`, `BORE_LOCAL_PORT`, `BORE_MIN_PORT`, `BORE_MAX_PORT`, `BORE_MAX_CONNS`, `BORE_CONTROL_PORT`, `BORE_BIND_DOMAIN`, `BORE_CERT_FILE`, `BORE_KEY_FILE`, `BORE_INSECURE`, `BORE_HTTPS`, `BORE_FORCE_HTTPS`, `BORE_AUTO_RECONNECT`, `BORE_TCP_SECRET_ID`, `BORE_LOCAL_PROXY_PORT`, `BORE_PREFER_UDP`, `BORE_STUN_SERVER`, `BORE_UDP`, `BORE_UPNP`, `BORE_TRY_PORT_PREDICTION`, `BORE_NAT_UDP_PORT`, `BORE_MAX_CONNS` also on `local`). **Logging** (`init_logging`): `tracing_subscriber` with an `EnvFilter` — default `info`, `-v`/`-vv` raise to `debug`/`trace`, `RUST_LOG` overrides; logs to **stderr**, ANSI **only on a TTY** (clean under Docker/journald/redirection). **Graceful shutdown**: `run` races the command against `shutdown_signal` (Ctrl-C + SIGTERM) → clean exit with a log line. All flags use short `value_name`s so `--help` renders the same compact layout across subcommands.

### Connection protocol (key flow to understand)

1. Client dials `CONTROL_PORT` and opens the **control** substream. It sends `Hello(port)` **first** (this matters — see below), then, if a secret is set, completes the auth challenge/response. Server replies `Hello(actual_port)` (port 0 ⇒ probe up to 150 random ports, see `create_listener`).
2. Server sends `Heartbeat` every 500ms on the control substream; if the send fails the client is gone and the tunnel (and its port) is torn down.
3. For each external connection to the tunnel port, the server acquires a permit and opens a new **data** substream, writes a one-byte readiness marker (`mux::STREAM_READY`), and splices the external socket to the substream with `copy_bidirectional_with_sizes`.
4. The client accepts the data substream, consumes the marker byte, dials the local service, and splices.

**Secret tunnels** (role chosen by the first control message — `HelloSecret(id)` / `ConnectSecret(id)` instead of `Hello(port)`; ack is `ServerMessage::Ok`): the provider connection is registered in `providers[id]` and bound by no port. A consumer (`bore proxy`) opens one substream per local connection; the server reads its readiness marker, looks up the provider, opens a substream to it, and `copy_bidirectional`s the two substreams. Direction is inverted vs. the public-port path: here the **consumer opens** data substreams and the **server accepts** them.

### UDP direct path (optional `udp` feature)

Only for **secret tunnels** (provider+consumer both dial the server = rendezvous; the public-port path is not hole-punchable). When both ends pass `--udp` and the server runs `--udp`: each peer opens a UDP socket, learns its reflexive address via STUN (the server's own STUN responder by default, or `--stun-server`), and offers candidates over the control channel (`ClientMessage::UdpCandidates`). The server brokers (`secret::broker_udp`): it mints a nonce, tells the provider to punch (`ServerMessage::UdpPunch` via a per-provider `mpsc` channel held in `UdpRegistry`) and replies to the consumer with the provider's candidates. Both punch; provider = QUIC server (`DirectListener`), consumer = QUIC client (`connect_direct`). On success the consumer routes data over the direct `mux::Opener` (provider serves it via the same `handle_connection` as relay); **any failure falls back to the relay** — the relay path is always available, so `--udp` never breaks a tunnel. The server-side brokering compiles without the feature (no `quinn`), so a lean server can rendezvous for `quinn`-enabled clients. The provider keeps a **persistent** `DirectListener` and re-punches its NAT toward each new/reconnecting consumer (`punch_via_endpoint`, since the raw socket is owned by quinn after setup); a **stable per-provider nonce** (in `UdpReg`) means every consumer derives the same token, so reconnecting and multiple consumers all work. **Reliability:** the consumer keeps the direct mux's `Acceptor` and `select!`s on it in `Proxy::listen` — it yields `None` when the QUIC path dies (provider restart), so `listen` returns and `--auto-reconnect` re-negotiates (direct again, or relay). Detection is immediate on a graceful close (provider teardown calls `DirectListener::close`) and within the QUIC idle timeout (~10s, keep-alive 3s) on a hard kill. Server death is handled by the existing control-channel reconnect on both ends. **Relay→direct upgrade (non-blocking):** a consumer on the relay retries the direct negotiation every `UDP_UPGRADE_INTERVAL` (10s). The slow work (STUN gather, punch, QUIC dial) runs in a spawned `upgrade_task` so `Proxy::listen`'s accept/forward loop **never stalls**; the loop owns `control` and only does the quick control I/O, handing candidates to the task (`cand` channel) and routing the brokered `UdpPunch` back (a `oneshot`), then swapping the data `Opener` in place on success (`done` channel) — no dropped session. (`negotiate_direct_consumer` is the synchronous variant used only at startup in `Proxy::new`, where blocking is fine; it and the task share `gather_consumer_candidates`/`finish_direct_consumer`.) The consumer's QUIC dial (`holepunch::connect_direct`) tries all provider candidates **concurrently under one total `NETWORK_TIMEOUT` budget** (`futures_util::select_ok`), not a full timeout per candidate (was up to N×3s for predicted/UPnP/local lists). So the system always converges to direct within ~10s of the provider becoming reachable.

### Connection stability (long transfers)

No timeout in the code closes an **established** data stream — `recv_timeout`/
`connect_with_timeout`/the edge peek are all setup-only, and `copy_bidirectional`
has no idle timeout. The mux carrier TCP is kept busy by the 500ms control
heartbeat. `shared::tune_tcp` sets `TCP_NODELAY` + `SO_KEEPALIVE` (15s) on every
proxied/control socket so middleboxes don't drop a long but quiet transfer
(e.g. `tar | rclone rcat`). Apply `tune_tcp` to any new accepted/dialed socket.

### Things to preserve when editing

- **Client sends `Hello` before authenticating.** yamux opens substreams *lazily* — the peer sees nothing until the opener writes. The server speaks first during auth, so if the client opened the control substream and waited to read, neither side would ever see it (deadlock). Sending `Hello` first is the eager write that announces the substream. The server still authenticates before binding any port.
- **The data substream's readiness marker is mandatory** for the same lazy-open reason: without it a connection whose local service speaks first (SSH/SMTP banners), or that sends no data, would never be established. Server writes `mux::STREAM_READY`; client reads exactly one byte before splicing.
- **Half-closed streams must keep working** — `copy_bidirectional_with_sizes` propagates EOF/shutdown across the substream (regression tests: `half_closed_tcp_stream`, and `mux_*` in `tests/mux_test.rs`).
- **`--max-conns`** bounds concurrently proxied connections via a semaphore; over the cap, new external connections are dropped. yamux's own stream limit is set generous so the semaphore is the real bound.
- The control channel still caps JSON frames at `MAX_FRAME_LENGTH` (`very_long_frame` test).

## Deployment & builds

- `Dockerfile` produces a static (musl) binary in a `scratch` image. `build-base`
  is installed so `ring` (TLS) compiles on Alpine.
- **`justfile`** (`just --list`): `build-amd64`/`build-arm64` (Linux, via
  `docker buildx --platform`), `macos-m5`/`windows-amd64` (via `cargo-zigbuild`,
  `docker/Dockerfile.cross`), `android-arm64` (via the Android NDK,
  `docker/Dockerfile.android` — zig can't build `ring` for Android). All write to
  `./bin/` (gitignored). `push` builds + pushes a multi-arch (amd64+arm64) image;
  set `repo`. `_builder` creates a docker-container buildx builder; `setup-qemu`
  registers binfmt for arm64 emulation.
- **`docker/docker-compose.{server,client,secret-proxy}.yml`**: ready-to-run
  compose files. Server uses a bridge network with explicit port forwards
  (control port + tunnel range; the scheme depends on the cert, not the port —
  `80`=plain, `443`=TLS); client and secret-proxy use `network_mode: host`. All
  env vars present (optional ones commented). UDP direct paths are enabled
  (`BORE_UDP=true` on the server with a `7835/udp` forward, `BORE_PREFER_UDP=true`
  on client/proxy); see the server file's NAT caveat about bridge vs host.
- **CI/release on every branch.** All four workflows run on **any branch** push
  (plus `v*` tags). `ci.yml` (host: fmt/clippy/build/test `--all-features` + audit)
  and `mean_bean_ci.yml` (cross build with `udp` + test relay-only) gate quality.
  `mean_bean_deploy.yml` produces a **GitHub Release** per push — a `create-release`
  job makes it (named `<branch>-<sha7>`, or the tag; branch builds are pre-releases,
  only tags become "latest") via `softprops/action-gh-release`, then the matrix
  jobs (macOS×2, Linux×7, Windows×2, Android) upload each binary as a release asset
  (`bore-<name>-<target>.{tar.gz,zip}`). `docker.yml` builds+pushes an **amd64-only**
  image to GHCR Packages, tagged by branch and sha (arm64 was dropped — QEMU
  emulation cost ~20 min; `just push` still does multi-arch locally). `ci/version.bash`
  computes the release name/tag. Releases need a git tag, so branch builds create a
  lightweight tag `<branch>-<sha7>` — this accumulates tags/releases per push (prune
  if noisy); the created tag doesn't match `v*` and the `GITHUB_TOKEN` can't
  re-trigger workflows, so there is no trigger loop.
