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

Five modules under `src/`:

- **`shared.rs`** — the protocol. Defines `ClientMessage`/`ServerMessage` enums (serde JSON), the `Delimited<U>` transport (null-byte-delimited JSON frames over any `AsyncRead+AsyncWrite`, via `AnyDelimiterCodec`), and constants `CONTROL_PORT = 7835`, `MAX_FRAME_LENGTH = 256`, `NETWORK_TIMEOUT = 3s`. Start here to understand any change touching the wire format.
- **`server.rs`** — `Server`: listens on the control port, manages a `DashMap<Uuid, TcpStream>` of pending incoming connections, allocates tunnel ports.
- **`client.rs`** — `Client`: connects to the server, requests a port, and opens a fresh proxy stream per forwarded connection.
- **`auth.rs`** — `Authenticator`: optional HMAC-SHA256 challenge/response handshake gating the control connection.
- **`main.rs`** — clap CLI (`local` / `server` subcommands) wiring args into the library. All flags also read from env vars (`BORE_SERVER`, `BORE_SECRET`, `BORE_LOCAL_PORT`, `BORE_MIN_PORT`, `BORE_MAX_PORT`).

### Connection protocol (key flow to understand)

The control connection is long-lived; each proxied TCP connection gets its own short-lived stream:

1. Client opens control connection to `CONTROL_PORT`, optionally completes the auth handshake, then sends `Hello(port)` (port 0 = "any").
2. Server binds a tunnel listener and replies `Hello(actual_port)`. For port 0, it probes up to 150 random ports in range (see the probability comment in `create_listener`).
3. Server sends periodic `Heartbeat` (every 500ms) on the control connection to detect a dead client.
4. On an external connection to the tunnel port, the server stores the stream in `conns` under a fresh `Uuid` and sends `Connection(uuid)` to the client. **Stored connections are dropped after 10s** if the client never accepts (prevents memory leaks).
5. Client opens a *new* stream to the control port, (re-)authenticates, sends `Accept(uuid)`. The server matches the UUID, then `copy_bidirectional` splices the external stream to the client's stream.

### Things to preserve when editing

- **Drain framed buffers before splicing.** Both `server.rs` and `client.rs` call `Delimited::into_parts()` and write the leftover `read_buf` to the peer before `copy_bidirectional`. Skipping this drops buffered bytes. There's a `debug_assert!` that `write_buf` is empty.
- **Half-closed TCP must keep working.** `copy_bidirectional` (not a hand-rolled copy) is used deliberately so one direction closing doesn't tear down the other — covered by the `half_closed_tcp_stream` test.
- **Frame length is capped** at `MAX_FRAME_LENGTH` to reject malicious oversized frames (`very_long_frame` test).
- Auth, when enabled, runs on *every* control stream (initial Hello and each Accept), not just once.

## Deployment

- `Dockerfile` produces a static binary in a `scratch` image (AMD64), published per release.
- Releases (binaries via `mean_bean_*` workflows, Docker via `docker.yml`) are tagged from version bumps in `Cargo.toml`.
