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

## Architecture

The client and server share **one** long-lived connection (on the control port, `7835` by default, `--control-port` to change) and multiplex everything over it with yamux. The connection is plain TCP, or TLS when the client's `--to` is an `https://` URL. There is no longer a separate connection (or auth handshake) per proxied connection.

Modules under `src/`:

- **`shared.rs`** — control-channel protocol. `ClientMessage`/`ServerMessage` enums (serde JSON) and the `Delimited<U>` transport (null-byte-delimited JSON frames via `AnyDelimiterCodec`). Constants `CONTROL_PORT`, `MAX_FRAME_LENGTH = 256`, `NETWORK_TIMEOUT = 3s`, `PROXY_BUFFER_SIZE = 64 KiB`.
- **`mux.rs`** — yamux wrapper, generic over any `Transport` (the `AsyncRead+AsyncWrite+Unpin+Send+'static` blanket trait — TCP or TLS). `mux::client`/`mux::server` spawn a single driver task that owns the `yamux::Connection` (its poll API needs `&mut`, so one owner only). `Opener::open()` requests outbound substreams over a channel; `Acceptor::accept()` yields inbound ones. `Stream` is `Compat<yamux::Stream>` (yamux is `futures`-IO; `tokio_util::compat` adapts it to Tokio traits).
- **`server.rs`** — `Server`: accepts the single connection, dispatches on the first control message into one of three roles (public-port tunnel, secret provider, secret consumer). Holds the `providers` registry and the `--max-conns` `Semaphore`.
- **`client.rs`** — `Client`: dials the server, opens the control substream, accepts data substreams and splices each to a fresh local connection. `Client::new` = public-port mode; `Client::new_secret_provider` = secret-provider mode (shares `listen`/`handle_connection`).
- **`edge.rs`** — per-connection handling on the public tunnel port when a tunnel sets `--https`/`--force-https`. Peeks the first bytes (bounded by a timeout; a no-options tunnel skips peeking entirely and forwards as before): a TLS `ClientHello` (`0x16`) is terminated with the server cert (`TunnelStream::Tls`), a plain HTTP request is answered with a `308` redirect to `https://` when `force_https`, otherwise the connection is forwarded plain. `TunnelOptions` rides in the `Hello` message.
- **`secret.rs`** — named "secret" tunnels (no public port). Server-side `serve_provider` (register under id) / `serve_consumer` + `relay` (splice each consumer substream to a provider substream); `Registry = Arc<DashMap<id, mux::Opener>>`; and the consumer-side `Proxy` (`bore proxy`) which binds a local listener and opens one substream per local connection.
- **`transport.rs`** — control-connection endpoint. `Endpoint::parse` turns `--to` into host/port/tls (`https://`→TLS:443, `http://`→plain:80, bare→plain:control-port; explicit `:port` overrides). `connect` dials and, for TLS, wraps with rustls (**ring** provider, for musl/scratch builds; `--insecure` skips verification, else webpki-roots). `ControlStream` is the plain-or-TLS enum (implements `mux::Transport`); `load_server_tls`/`server_tls_from_pem` build the server `TlsAcceptor`.
- **`reconnect.rs`** — `--auto-reconnect` support. `Backoff` yields 1,2,4,8,16,32 then 32s indefinitely (reset on a successful connect); generic `run(auto, connect, serve)` runs the connect/serve cycle once (errors propagate — the original behaviour) or loops forever reconnecting. Used by `local` (normal + provider) and `proxy` in `main.rs`.
- **`auth.rs`** — `Authenticator`: optional HMAC-SHA256 challenge/response, run **once** on the control substream.
- **`main.rs`** — clap CLI (`local` / `proxy` / `server`). Flags also read env vars (`BORE_SERVER`, `BORE_SECRET`, `BORE_LOCAL_PORT`, `BORE_MIN_PORT`, `BORE_MAX_PORT`, `BORE_MAX_CONNS`, `BORE_CONTROL_PORT`, `BORE_BIND_DOMAIN`, `BORE_CERT_FILE`, `BORE_KEY_FILE`, `BORE_INSECURE`, `BORE_HTTPS`, `BORE_FORCE_HTTPS`, `BORE_AUTO_RECONNECT`, `BORE_TCP_SECRET_ID`, `BORE_LOCAL_PROXY_PORT`).

### Connection protocol (key flow to understand)

1. Client dials `CONTROL_PORT` and opens the **control** substream. It sends `Hello(port)` **first** (this matters — see below), then, if a secret is set, completes the auth challenge/response. Server replies `Hello(actual_port)` (port 0 ⇒ probe up to 150 random ports, see `create_listener`).
2. Server sends `Heartbeat` every 500ms on the control substream; if the send fails the client is gone and the tunnel (and its port) is torn down.
3. For each external connection to the tunnel port, the server acquires a permit and opens a new **data** substream, writes a one-byte readiness marker (`mux::STREAM_READY`), and splices the external socket to the substream with `copy_bidirectional_with_sizes`.
4. The client accepts the data substream, consumes the marker byte, dials the local service, and splices.

**Secret tunnels** (role chosen by the first control message — `HelloSecret(id)` / `ConnectSecret(id)` instead of `Hello(port)`; ack is `ServerMessage::Ok`): the provider connection is registered in `providers[id]` and bound by no port. A consumer (`bore proxy`) opens one substream per local connection; the server reads its readiness marker, looks up the provider, opens a substream to it, and `copy_bidirectional`s the two substreams. Direction is inverted vs. the public-port path: here the **consumer opens** data substreams and the **server accepts** them.

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
  env vars present (optional ones commented).
- Upstream release machinery (`mean_bean_*` workflows, `docker.yml`) is unchanged.
