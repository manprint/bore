# bore (forked from ekzhang/bore)

[![Build status](https://img.shields.io/github/actions/workflow/status/ekzhang/bore/ci.yml)](https://github.com/ekzhang/bore/actions)
[![Crates.io](https://img.shields.io/crates/v/bore-cli.svg)](https://crates.io/crates/bore-cli)

A modern, simple TCP tunnel in Rust that exposes local ports to a remote server, bypassing standard NAT connection firewalls. **That's all it does: no more, and no less.**

![Video demo](https://i.imgur.com/vDeGsmx.gif)

```shell
# Installation (requires Rust, see alternatives below)
cargo install bore-cli

# On your local machine
bore local 8000 --to bore.pub
```

This will expose your local port at `localhost:8000` to the public internet at `bore.pub:<PORT>`, where the port number is assigned randomly.

Similar to [localtunnel](https://github.com/localtunnel/localtunnel) and [ngrok](https://ngrok.io/), except `bore` is intended to be a highly efficient, unopinionated tool for forwarding TCP traffic that is simple to install and easy to self-host, with no frills attached.

(`bore` totals about 400 lines of safe, async Rust code and is trivial to set up — just run a single binary for the client and server.)

## Installation

### macOS

`bore` is packaged as a Homebrew core formula.

```shell
brew install bore-cli
```

### Linux

#### Arch Linux

`bore` is available in the AUR as `bore`.

```shell
yay -S bore # or your favorite AUR helper
```

#### Gentoo Linux

`bore` is available in the [gentoo-zh](https://github.com/microcai/gentoo-zh) overlay.

```shell
sudo eselect repository enable gentoo-zh
sudo emerge --sync gentoo-zh
sudo emerge net-proxy/bore
```

### Binary Distribution

Otherwise, the easiest way to install bore is from prebuilt binaries. These are available on the [releases page](https://github.com/ekzhang/bore/releases) for macOS, Windows, and Linux. Just unzip the appropriate file for your platform and move the `bore` executable into a folder on your PATH.

> **This fork** publishes a GitHub Release for **every push** (any branch): named
> `<branch>-<sha7>` (branch builds are marked pre-release; `vX.Y.Z` tags are full
> releases), with binaries attached for macOS (x86_64/arm64), Linux (x86_64,
> aarch64, arm, armv7, i686), Windows (x86_64/i686) and Android (aarch64). Container
> images are pushed to the GitHub **Packages** registry (`ghcr.io/<owner>/bore`),
> tagged by branch and commit (amd64 + arm64).

### Cargo

You also can build `bore` from source using [Cargo](https://doc.rust-lang.org/cargo/), the Rust package manager. This command installs the `bore` binary at a user-accessible path.

```shell
cargo install bore-cli
```

### Docker

We also publish versioned Docker images for each release. The image is built for an AMD 64-bit architecture. They're tagged with the specific version and allow you to run the statically-linked `bore` binary from a minimal "scratch" container.

```shell
docker run -it --init --rm --network host ekzhang/bore <ARGS>
```

#### Docker Compose

Ready-to-run compose files live in [`docker/`](docker/): `docker-compose.server.yml`
(bridge network, control port + tunnel range forwarded explicitly),
`docker-compose.client.yml` and `docker-compose.secret-proxy.yml` (host network).
All environment variables are present (optional ones commented).

```shell
docker compose -f docker/docker-compose.server.yml up -d
```

### Building from source (cross-compilation)

A [`justfile`](justfile) builds release binaries into `./bin/` via Docker for
several targets (`just --list`):

```shell
just build-amd64       # Linux x86_64
just build-arm64       # Linux aarch64
just macos-m5          # macOS Apple Silicon (aarch64-apple-darwin)
just windows-amd64     # Windows x86_64
just android-arm64     # Android aarch64
just build             # all of the above
just push              # build + push a multi-arch (amd64+arm64) image to Docker Hub
```

## Detailed Usage

This section describes detailed usage for the `bore` CLI command.

### Local Forwarding

You can forward a port on your local machine by using the `bore local` command. This takes a positional argument, the local port to forward, as well as a mandatory `--to` option, which specifies the address of the remote server.

```shell
bore local 5000 --to bore.pub
```

You can optionally pass in a `--port` option to pick a specific port on the remote to expose, although the command will fail if this port is not available. Also, passing `--local-host` allows you to expose a different host on your local area network besides the loopback address `localhost`.

The `--to` value selects the transport for the control connection:

- `bore.pub` — plain TCP on the control port (default `7835`).
- `bore.pub:1000` — plain TCP on an explicit control port.
- `http://bore.tld` — plain TCP, default port `80`.
- `https://bore.tld` — TLS, default port `443`. Use `--insecure` to accept a
  self-signed server certificate.

```shell
Starts a local proxy to the remote server

Usage: bore local [OPTIONS] --to <TO> <LOCAL_PORT>

Arguments:
  <LOCAL_PORT>  The local port to expose [env: BORE_LOCAL_PORT=]

Options:
  -l, --local-host <HOST>      The local host to expose [default: localhost]
  -t, --to <TO>                Address of the remote server [env: BORE_SERVER=]
  -p, --port <PORT>            Optional port on the remote server to select [default: 0]
  -s, --secret <SECRET>        Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>     Register as a named secret tunnel [env: BORE_TCP_SECRET_ID=]
      --insecure               Skip TLS certificate verification [env: BORE_INSECURE=]
      --https                  Terminate TLS on the tunnel port [env: BORE_HTTPS=]
      --force-https            Redirect plain HTTP to https:// (requires --https) [env: BORE_FORCE_HTTPS=]
      --udp                    Prefer a direct UDP hole-punched path (secret tunnels only) [env: BORE_PREFER_UDP=]
      --stun-server <HOST:PORT> STUN server for the direct path [env: BORE_STUN_SERVER=]
      --auto-reconnect         Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
  -h, --help                   Print help
```

#### Automatic reconnection

Both `bore local` and `bore proxy` accept `--auto-reconnect`. When the connection
fails to establish or drops, the client reconnects on its own with a capped
exponential backoff of 1, 2, 4, 8, 16, 32 seconds, then every 32 seconds
indefinitely; a successful connection resets the backoff.

#### HTTPS on the tunnel port

By default a tunnel port forwards raw TCP. With `--https`, the server terminates
TLS on the tunnel port using its certificate, so the exposed service is reachable
over `https://` — while plain `http://` and raw TCP keep working on the same port:

```shell
# Server has a certificate (see "Serving over HTTPS/HTTP" below).
bore local 8080 --to https://bore.tld -p 9000 -s mysecret --https
# -> https://bore.tld:9000   (TLS, terminated at the server)
# -> http://bore.tld:9000    (plain)
# -> bore.tld:9000           (raw TCP)
```

Add `--force-https` to redirect plain HTTP requests to `https://` (raw TCP and
`https://` keep working):

```shell
bore local 8080 --to https://bore.tld -p 9000 -s mysecret --https --force-https
# -> https://bore.tld:9000   (TLS)
# -> http://bore.tld:9000    (308 redirect to https://bore.tld:9000)
# -> bore.tld:9000           (raw TCP)
```

### Self-Hosting

As mentioned in the startup instructions, there is a public instance of the `bore` server running at `bore.pub`. However, if you want to self-host `bore` on your own network, you can do so with the following command:

```shell
bore server
```

That's all it takes! After the server starts running at a given address, you can then update the `bore local` command with option `--to <ADDRESS>` to forward a local port to this remote server.

It's possible to specify different IP addresses for the control server and for the tunnels. This setup is useful for cases where you might want the control server to be on a private network while allowing tunnel connections over a public interface, or vice versa.

The control port defaults to `7835` but is configurable with `--control-port`; clients then connect with `--to host:port`.

#### Serving over HTTPS/HTTP

Pass a certificate and key to serve the control connection over TLS; clients connect with `https://`:

```shell
# HTTPS (clients: --to https://bore.tld)
bore server --bind-domain bore.tld --cert-file /var/bore/cert.pem --key-file /var/bore/key.pem

# Plain HTTP addressing, no TLS (clients: --to http://bore.tld)
bore server --bind-domain bore.tld
```

A self-signed certificate requires `--insecure` on the client. The full options:

```shell
Runs the remote proxy server

Usage: bore server [OPTIONS]

Options:
      --min-port <MIN_PORT>          Minimum accepted TCP port number [env: BORE_MIN_PORT=] [default: 1024]
      --max-port <MAX_PORT>          Maximum accepted TCP port number [env: BORE_MAX_PORT=] [default: 65535]
  -s, --secret <SECRET>              Optional secret for authentication [env: BORE_SECRET]
      --max-conns <MAX_CONNS>        Max concurrently proxied connections per client [env: BORE_MAX_CONNS=] [default: 1024]
      --control-port <CONTROL_PORT>  TCP port the control connection listens on [env: BORE_CONTROL_PORT=] [default: 7835]
      --bind-domain <BIND_DOMAIN>    Public domain advertised to clients [env: BORE_BIND_DOMAIN=]
      --cert-file <CERT_FILE>        TLS certificate chain (PEM); with --key-file, serves HTTPS [env: BORE_CERT_FILE=]
      --key-file <KEY_FILE>          TLS private key (PEM); with --cert-file, serves HTTPS [env: BORE_KEY_FILE=]
      --bind-addr <BIND_ADDR>        IP address to bind to, clients must reach this [default: 0.0.0.0]
      --bind-tunnels <BIND_TUNNELS>  IP address where tunnels will listen on, defaults to --bind-addr
      --udp                          Broker UDP direct paths and run a STUN responder on the control port [env: BORE_UDP=]
  -h, --help                         Print help
```

### Secret tunnels (no public port)

Instead of exposing your service on a public port, you can publish it under a
named _secret id_ and reach it only through a dedicated `bore proxy`. No port is
allocated on the server — the entire path stays internal to the multiplexed
connection.

There are three machines:

```shell
# Machine A — the server (optionally with a shared secret)
bore server --secret mysecret

# Machine B — the service to expose (e.g. on port 8080). Registers the id, no
# public port is opened on the server.
bore local 8080 --to bore.tld --secret mysecret --tcp-secret-id my-8080-secret-service

# Machine C — open the tunnel locally. Now localhost:5555 reaches B's service.
bore proxy --to bore.tld --local-proxy-port :5555 --secret mysecret --tcp-secret-id my-8080-secret-service
```

`--local-proxy-port :5555` binds all interfaces (so other machines on C's network
can reach it too); use `127.0.0.1:5555` to bind loopback only. The `--tcp-secret-id`
on the proxy must match the one used by the provider. Each id may have a single
provider at a time; a second registration of the same id is rejected.

```shell
Connects to a named secret tunnel and exposes it on a local port

Usage: bore proxy [OPTIONS] --local-proxy-port <LOCAL_PROXY_PORT> --to <TO> --tcp-secret-id <TCP_SECRET_ID>

Options:
      --local-proxy-port <HOST>  Local address to listen on, e.g. ":5555" or "127.0.0.1:5555" [env: BORE_LOCAL_PROXY_PORT=]
  -t, --to <TO>                  Address of the remote server [env: BORE_SERVER=]
  -s, --secret <SECRET>          Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>       Identifier of the secret tunnel to connect to [env: BORE_TCP_SECRET_ID=]
      --insecure                 Skip TLS certificate verification [env: BORE_INSECURE=]
      --udp                      Prefer a direct UDP hole-punched path [env: BORE_PREFER_UDP=]
      --stun-server <HOST:PORT>  STUN server for the direct path [env: BORE_STUN_SERVER=]
      --auto-reconnect           Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
  -h, --help                     Print help
```

#### Direct UDP path (hole-punching)

By default a secret tunnel relays all data through the server. With `--udp` on the
server **and** on both ends, `bore` instead tries to establish a **direct**
peer-to-peer path between the provider and the consumer using UDP hole-punching,
carried over [QUIC](https://github.com/quinn-rs/quinn) — the server is then only a
rendezvous/signaling point and steps out of the data path (lower latency, no server
bandwidth). If the direct path can't be established (e.g. a symmetric NAT, UDP
blocked), it **automatically falls back to the relay**, so `--udp` never breaks a
tunnel.

```shell
# Server: broker direct paths + run a STUN responder on the control port (UDP).
bore server --secret mysecret --udp

# Provider and consumer both opt in with --udp:
bore local 8080 --to https://bore.tld --secret mysecret --tcp-secret-id svc --udp
bore proxy --to https://bore.tld --local-proxy-port :5555 --secret mysecret --tcp-secret-id svc --udp
```

Notes:

- **Requires the `udp` feature**, which is **on by default**. Build
  `--no-default-features` to drop it (and the `quinn` dependency).
- **Reflexive discovery (STUN).** Each peer learns its public address from the
  server's built-in STUN responder, bound on the control port over **UDP** — so
  open **UDP** on the control port too (e.g. `7835/udp`), not just TCP. For an
  `https://`/`http://` server address the STUN target defaults to the control
  port `7835`, not `443`/`80`; override with `--stun-server host:port` (any
  standard STUN server works).
- **Authentication.** The direct path is authenticated by a token derived from
  `--secret` and a server-issued nonce, verified before any data flows.
- **Scope & limits.** Only secret tunnels are hole-punchable (not public-port
  tunnels). Reconnecting and multiple consumers are supported (the provider keeps
  a persistent QUIC listener and re-punches toward each one). Both peers behind a
  symmetric NAT → relay.
- To confirm the direct path is in use, look for `using direct udp path` /
  `direct udp carrier established (… token verified)` in the logs
  (`RUST_LOG=bore_cli=info`).

## Protocol

There is a _control port_, `7835` by default (configurable with `--control-port`). The client opens a single connection to it — plain TCP, or TLS when reached via `https://` — and [multiplexes](https://github.com/hashicorp/yamux/blob/master/spec.md) everything over that one connection. At initialization, the client opens a control stream and sends a "Hello" message asking to proxy a selected remote port. The server responds with an acknowledgement and begins listening for external TCP connections.

Whenever the server obtains a connection on the remote port, it opens a new multiplexed stream to the client over the existing connection, and proxies the external connection over it. This avoids a fresh TCP (and authentication) handshake per proxied connection. The number of concurrently proxied connections per client is bounded by `--max-conns`.

Secret tunnels reuse the same machinery without a public port. A provider (`bore local --tcp-secret-id`) registers its connection under the id; a consumer (`bore proxy`) opens a stream per local connection, and the server relays each one to the provider over a freshly opened stream — splicing the two multiplexed streams together internally.

When a tunnel sets `--https`, the server inspects the first bytes of each connection on the tunnel port: a TLS `ClientHello` is terminated with the server's certificate (and the decrypted stream forwarded), a plain HTTP request is redirected to `https://` if `--force-https` is set, and anything else is forwarded as raw TCP.

## Authentication

On a custom deployment of `bore server`, you can optionally require a _secret_ to prevent the server from being used by others. The client verifies possession of the secret once, when establishing the connection, by answering a random challenge in the form of an HMAC code. (This secret is only used for the initial handshake, and no further traffic is encrypted by default.)

```shell
# on the server
bore server --secret my_secret_string

# on the client
bore local <LOCAL_PORT> --to <TO> --secret my_secret_string
```

If a secret is not present in the arguments, `bore` will also attempt to read from the `BORE_SECRET` environment variable.

## Acknowledgements

Created by Eric Zhang ([@ekzhang1](https://twitter.com/ekzhang1)). Licensed under the [MIT license](LICENSE).

The author would like to thank the contributors and maintainers of the [Tokio](https://tokio.rs/) project for making it possible to write ergonomic and efficient network services in Rust.
