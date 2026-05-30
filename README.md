# bore

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

## Detailed Usage

This section describes detailed usage for the `bore` CLI command.

### Local Forwarding

You can forward a port on your local machine by using the `bore local` command. This takes a positional argument, the local port to forward, as well as a mandatory `--to` option, which specifies the address of the remote server.

```shell
bore local 5000 --to bore.pub
```

You can optionally pass in a `--port` option to pick a specific port on the remote to expose, although the command will fail if this port is not available. Also, passing `--local-host` allows you to expose a different host on your local area network besides the loopback address `localhost`.

The full options are shown below.

```shell
Starts a local proxy to the remote server

Usage: bore local [OPTIONS] --to <TO> <LOCAL_PORT>

Arguments:
  <LOCAL_PORT>  The local port to expose [env: BORE_LOCAL_PORT=]

Options:
  -l, --local-host <HOST>  The local host to expose [default: localhost]
  -t, --to <TO>            Address of the remote server to expose local ports to [env: BORE_SERVER=]
  -p, --port <PORT>        Optional port on the remote server to select [default: 0]
  -s, --secret <SECRET>    Optional secret for authentication [env: BORE_SECRET]
  -h, --help               Print help
```

### Self-Hosting

As mentioned in the startup instructions, there is a public instance of the `bore` server running at `bore.pub`. However, if you want to self-host `bore` on your own network, you can do so with the following command:

```shell
bore server
```

That's all it takes! After the server starts running at a given address, you can then update the `bore local` command with option `--to <ADDRESS>` to forward a local port to this remote server.

It's possible to specify different IP addresses for the control server and for the tunnels. This setup is useful for cases where you might want the control server to be on a private network while allowing tunnel connections over a public interface, or vice versa.

The full options for the `bore server` command are shown below.

```shell
Runs the remote proxy server

Usage: bore server [OPTIONS]

Options:
      --min-port <MIN_PORT>          Minimum accepted TCP port number [env: BORE_MIN_PORT=] [default: 1024]
      --max-port <MAX_PORT>          Maximum accepted TCP port number [env: BORE_MAX_PORT=] [default: 65535]
  -s, --secret <SECRET>              Optional secret for authentication [env: BORE_SECRET]
      --bind-addr <BIND_ADDR>        IP address to bind to, clients must reach this [default: 0.0.0.0]
      --bind-tunnels <BIND_TUNNELS>  IP address where tunnels will listen on, defaults to --bind-addr
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
  -h, --help                     Print help
```

## Protocol

There is an implicit _control port_ at `7835`. The client opens a single TCP connection to it and [multiplexes](https://github.com/hashicorp/yamux/blob/master/spec.md) everything over that one connection. At initialization, the client opens a control stream and sends a "Hello" message asking to proxy a selected remote port. The server responds with an acknowledgement and begins listening for external TCP connections.

Whenever the server obtains a connection on the remote port, it opens a new multiplexed stream to the client over the existing connection, and proxies the external connection over it. This avoids a fresh TCP (and authentication) handshake per proxied connection. The number of concurrently proxied connections per client is bounded by `--max-conns`.

Secret tunnels reuse the same machinery without a public port. A provider (`bore local --tcp-secret-id`) registers its connection under the id; a consumer (`bore proxy`) opens a stream per local connection, and the server relays each one to the provider over a freshly opened stream — splicing the two multiplexed streams together internally.

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
