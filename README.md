# bore (forked from ekzhang/bore)

[![Build status](https://img.shields.io/github/actions/workflow/status/manprint/bore/ci.yml)](https://github.com/manprint/bore/actions)
[![Crates.io](https://img.shields.io/crates/v/bore-cli.svg)](https://crates.io/crates/bore-cli)

A modern, simple TCP tunnel in Rust that exposes local ports to a remote server, bypassing standard NAT connection firewalls. **That's all it does: no more, and no less.**

![Video demo](https://i.imgur.com/vDeGsmx.gif)

```shell
# Installation (requires Rust, see alternatives below)
cargo install bore-cli

# On your local machine
bore local 8000
```

This will expose your local port at `localhost:8000` to the public internet through the default server `https://bore.0912345.xyz`, with a public port assigned randomly.

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

Otherwise, the easiest way to install bore is from prebuilt binaries. These are available on the [releases page](https://github.com/manprint/bore/releases) for macOS, Windows, and Linux. Just unzip the appropriate file for your platform and move the `bore` executable into a folder on your PATH.

> **This fork** publishes a GitHub Release for **every push** (any branch): named
> `<branch>-<sha7>` (branch builds are marked pre-release; `vX.Y.Z` tags are full
> releases), with binaries attached for macOS (x86_64/arm64), Linux (x86_64,
> aarch64, arm, armv7, i686), Windows (x86_64/i686) and Android (aarch64). Container
> images are pushed to the GitHub **Packages** registry (`ghcr.io/manprint/bore`),
> tagged by branch and commit (amd64; build `just push` locally for multi-arch).



```shell
cargo install bore-cli
```

### Docker

We also publish versioned Docker images for each release. The image is built for an AMD 64-bit architecture. They're tagged with the specific version and allow you to run the statically-linked `bore` binary from a minimal "scratch" container.

```shell
docker run -it --init --rm --network host ghcr.io/manprint/bore <ARGS>


Ready-to-run compose files live in [`docker/`](docker/): `docker-compose.server.yml`
(bridge network, control port + tunnel range forwarded explicitly),
All environment variables are present (optional ones commented). Server-side UDP,
relay, Docker networking, carrier and file-descriptor tuning notes are in
[`SERVER_UDP_OPTIMIZATION.md`](SERVER_UDP_OPTIMIZATION.md).

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
just build             # all of the above
just push              # build + push a multi-arch (amd64+arm64) image to Docker Hub
```

## Detailed Usage

This section describes detailed usage for the `bore` CLI command.

### Local Forwarding

You can forward a port on your local machine by using the `bore local` command. This takes a positional argument, the local port to forward. If you omit `--to`, the client defaults to `https://bore.0912345.xyz`; pass `--to` or `BORE_SERVER` to override it.

```shell
bore local 5000
```

You can optionally pass in a `--port` option to pick a specific port on the remote to expose, although the command will fail if this port is not available. Also, passing `--local-host` allows you to expose a different host on your local area network besides the loopback address `localhost`.

The `--to` value selects the transport for the control connection. When omitted, `bore` uses `https://bore.0912345.xyz` (TLS on port `443`):

- `bore.0912345.xyz` — plain TCP on the control port (default `7835`).
- `bore.0912345.xyz:1000` — plain TCP on an explicit control port.
- `http://bore.tld` — plain TCP, default port `80`.
- `https://bore.tld` — TLS, default port `443`. Use `--insecure` to accept a
  self-signed server certificate.

```shell
Starts a local proxy to the remote server

Usage: bore local [OPTIONS] <PORT>

Arguments:
  <PORT>  The local port to expose [env: BORE_LOCAL_PORT=]

Options:
  -l, --local-host <HOST>      The local host to expose [default: localhost]
  -v, --verbose...             Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -t, --to <ADDR>              Address of the remote server [env: BORE_SERVER=] [default: https://bore.0912345.xyz]
  -p, --port <PORT>            Optional port on the remote server to select [default: 0]
  -s, --secret <SECRET>        Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>     Register as a named secret tunnel [env: BORE_TCP_SECRET_ID=]
      --insecure               Skip TLS certificate verification [env: BORE_INSECURE=]
      --https                  Terminate TLS on the tunnel port [env: BORE_HTTPS=]
      --force-https            Redirect plain HTTP to https:// (requires --https) [env: BORE_FORCE_HTTPS=]
      --udp                    Prefer a direct UDP/QUIC data path (public: server→client QUIC; secret: hole-punched). Falls back to relay. [env: BORE_PREFER_UDP=]
      --stun-server <HOST:PORT> STUN server for the direct path [env: BORE_STUN_SERVER=]
      --upnp                   Map a router port via UPnP-IGD for the direct path [env: BORE_UPNP=]
      --try-port-prediction    Advertise predicted symmetric-NAT ports (opt-in, best-effort) [env: BORE_TRY_PORT_PREDICTION=]
      --nat-udp-preferred-port <PORT> Bind the UDP hole-punch socket to a fixed port (0=random) [env: BORE_NAT_UDP_PORT=]
      --nat-udp-release-timeout <SECS> Re-check interval when the NAT remapped the preferred UDP port (default 600s, 0=disable) [env: BORE_NAT_UDP_RELEASE_TIMEOUT=]
      --max-conns <N>          Max concurrent connections on the direct UDP path (default 1024) [env: BORE_MAX_CONNS=]
      --basic-auth <USER:PASS> Protect the tunnel with HTTP Basic auth [env: BORE_BASIC_AUTH]
      --notes <TEXT>           Note shown on the server's admin status page [env: BORE_NOTES=]
      --carriers <N>           Parallel TCP carrier connections for the data path (public tunnels; default 1) [env: BORE_CARRIERS=]
      --auto-reconnect         Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
  -h, --help                   Print help
```

#### Parallel carriers (`--carriers`)

By default a public tunnel multiplexes **every** proxied connection over a
**single** TCP connection to the server. Under packet loss that causes
cross-connection head-of-line blocking (one flow's lost segment stalls all the
others sharing the TCP), and every flow shares one TCP congestion window.

`--carriers N` opens **N parallel TCP connections** and spreads proxied connections
across them (round-robin). A lost segment then only stalls the ~1/N flows on that
carrier, and each carrier gets its own congestion window:

```shell
bore local 8080 --to bore.tld -p 9000 -s mysecret --carriers 4
```

It applies to **every relay leg**, because the server is always in the relay data path:

- **Public tunnel** (`bore local --carriers`): the server→client leg.
- **Secret provider** (`bore local --tcp-secret-id --carriers`): the server→provider
  leg (the bottleneck shared by *all* consumers of that provider).
- **Secret consumer** (`bore proxy --carriers`): the consumer→server leg.

```shell
bore local 8080 --to bore.tld -p 9000 -s mysecret --carriers 4          # public
bore local 8080 --to bore.tld --tcp-secret-id app -s mysecret --carriers 4   # provider
bore proxy --to bore.tld --tcp-secret-id app -s mysecret --local-proxy-port :5555 --carriers 4
```

When it helps and when it doesn't:

- **Helps** concurrent workloads: parallel `rclone`/S3/WebDAV transfers, browsers
  (many requests), streaming — especially on a lossy or high-latency link to the
  server.
- **No change** for a single bulk transfer (one flow = one carrier). For single-flow
  loss/high-BDP, tune the **host** instead: `sysctl net.ipv4.tcp_congestion_control=bbr`
  (bore can't set per-socket congestion control without `unsafe`).
- The server is always in the relay data path, so this does **not** add bandwidth or
  bypass the server — it removes the single-TCP bottleneck on the relay leg only.

The server caps `N` at its `--max-carriers` (default 16) for public tunnels and
providers; a larger request is clamped, and `--max-carriers 1` disables the pool
(single connection). A carrier that drops mid-session is re-dialed automatically; the
tunnel never breaks (it just runs with fewer carriers until the re-dial succeeds).
Default `1` = unchanged behaviour.

**The UDP direct path needs no `--carriers` for secret tunnels and transfer.** When
a secret tunnel runs over a direct hole-punched path (`--udp`), each proxied
connection already rides its **own native QUIC stream**, which QUIC keeps
independently loss-isolated — so there is no single-stream head-of-line blocking to
fix. `--carriers` widens the relay; `--udp` fixes the direct path. They compose (the
relay pool is used whenever a tunnel is on the relay fallback).

**Exception — `bore vhost --udp` and `bore local --udp` (public tunnel):** there
`--carriers N` *also* sizes the QUIC direct path. The client/provider opens `N`
parallel QUIC **connections** and the server pools them and round-robins proxied
connections across them, parallelizing per-connection crypto/congestion across cores
(capped at 32, not by `--max-carriers`). Both need `bore server --udp`; both fall back
to the TCP relay per-connection when the direct path is unavailable. As always, a
single flow over one connection is not split — see `CARRIER_TUNING.md`.

**Proxy copy buffer:** `BORE_PROXY_BUFFER_SIZE` (default 256 KiB; accepts a
`KB`/`MB`/`GiB`/... suffix, clamped `[4 KiB, 16 MiB]`) sets the per-direction relay/
splice buffer. Set it on the server (relay buffers) and/or a provider (local splice);
a larger buffer helps high-latency, high-BDP links, not single-stream throughput on a
fast LAN.

For bulk transfers, the direct QUIC path is tuned in code with larger flow-control
windows than Quinn's defaults: `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` (16 MiB),
`DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` (64 MiB), and `DIRECT_QUIC_SEND_WINDOW`
(64 MiB) in `src/shared.rs`. The same defaults can now be overridden on
`bore server` with `--udp-stream-receive-window`, `--udp-connection-receive-window`,
`--udp-send-window`, `--udp-socket-recv-buffer`, `--udp-socket-send-buffer`, and
`--udp-max-streams` (or the matching `BORE_...` env vars); the server brokers the
chosen tuning to the direct-path peers. Bore also requests
`DIRECT_UDP_SOCKET_RECV_BUFFER` and `DIRECT_UDP_SOCKET_SEND_BUFFER` (16 MiB each),
sets `MAX_DIRECT_STREAMS` to 4096, keeps QUIC alive every 3s with a 10s idle
timeout, and uses `quinn::congestion::BbrConfig` for the direct path. If
`bore test-udp --test-bandwidth` shows UDP direct with lower latency but less
throughput than TCP relay, that is not automatically a bug: QUIC is reliable and
congestion-controlled over UDP, while the relay uses highly optimized kernel TCP
and may sit close to one peer. Tune those constants only after measuring both
directions with a realistic quota.

**Direct-path throughput on unprivileged hosts.** The 16 MiB UDP socket buffers
above are requested with `SO_{SND,RCV}BUFFORCE`, which bypasses the kernel's
`net.core.{r,w}mem_max` ceiling — but that needs `CAP_NET_ADMIN`. `bore vpn` runs
privileged and gets it; an ordinary `bore local --udp` / `bore proxy --udp` does
**not**, so on a host with the stock `*mem_max` (208 KiB on Ubuntu/Debian/AWS) the
buffers are clamped and a single direct flow is capped at roughly `buffer / RTT`
(≈10 MB/s at 20 ms RTT) regardless of the QUIC windows. bore logs a `warn!` with the
remediation when this happens; raise the ceiling with
`sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216` (and the
matching `net.core.*mem_default` for it to take effect on new sockets).

**`--udp` selects a *transport*, it does not forward UDP application traffic.**
`bore local`/`bore proxy` forward **TCP** services only; `--udp` chooses a direct
**QUIC** data path for the tunnel itself, with automatic relay fallback. It works for:
- **secret tunnels** (`bore proxy`, `bore local --tcp-secret-id`): peer-to-peer
  hole-punched QUIC between consumer and provider;
- **public tunnels** (`bore local`, no `--tcp-secret-id`): a server→client QUIC path
  (the server is public, so no hole-punch is needed — same model as `bore vhost --udp`).
  `--carriers N` opens N independent QUIC connections (each its own BBR congestion
  controller), exactly like vhost. Requires `bore server --udp`; otherwise the tunnel
  transparently stays on the TCP relay.

The other direct-path flags (`--upnp`, `--stun-server`, `--try-port-prediction`,
`--nat-udp-*`) are hole-punch helpers and apply to **secret tunnels only**; on a public
tunnel they are inert and bore `warn!`s instead of silently ignoring them. To tunnel a
**UDP application** (DNS, game servers, WireGuard, …) use `bore vpn` (L3 overlay), not
`bore local`/`proxy`.

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

#### WebSocket support

`bore` forwards standard WebSocket connections transparently:

- **Public tunnels** (`bore local`) support `ws://` and, with `--https`, `wss://`.
- **Secret tunnels** (`bore local --tcp-secret-id` + `bore proxy`) support WebSocket
  traffic on both the relay path and the direct UDP path.
- **Vhost** (`bore vhost`) supports standard HTTP/1.1 WebSocket upgrade on both the
  TCP relay and `bore vhost --udp`.

This works because bore only inspects the first bytes needed for routing / TLS /
optional HTTP handling, then switches to a full-duplex byte-stream splice. After the
HTTP `101 Switching Protocols` response, WebSocket frames are forwarded unchanged.

Important caveats:

- The supported vhost/browser path is the classic **HTTP/1.1 `Upgrade: websocket`** flow.
  WebSocket over HTTP/2 extended CONNECT is not implemented.
- For `bore vhost --udp`, only the **server->provider** hop uses QUIC; the browser still
  talks HTTP/TLS to the server.
- If a live direct UDP/QUIC path drops, an already-open WebSocket on that path drops too;
  fallback applies to new connections, not migration of an in-flight stream.

End-to-end tests now cover public tunnels, secret tunnels, and vhost WebSocket flows.

### Self-Hosting

As mentioned in the startup instructions, the CLI now defaults to the public server `https://bore.0912345.xyz`. However, if you want to self-host `bore` on your own network, you can do so with the following command:

```shell
bore server
```

That's all it takes! After the server starts running at a given address, you can then update the `bore local` command with option `--to <ADDRESS>` to forward a local port to this remote server.

It's possible to specify different IP addresses for the control server and for the tunnels. This setup is useful for cases where you might want the control server to be on a private network while allowing tunnel connections over a public interface, or vice versa.

The control port defaults to `7835` but is configurable with `--control-port`; clients then connect with `--to host:port`.

#### Serving over HTTPS/HTTP

Pass a certificate and key to serve the control connection over TLS; clients connect with `https://`:
 Try these transfer modes:
```shell
# HTTPS (clients: --to https://bore.tld)
bore server --bind-domain bore.tld --cert-file /var/bore/cert.pem --key-file /var/bore/key.pem

# Plain HTTP addressing, no TLS (clients: --to http://bore.tld)
bore server --bind-domain bore.tld
```

A self-signed certificate requires `--insecure` on the client. The full options:

```shell
Runs the remote proxy server
```shell

Usage: bore server [OPTIONS]

Options:
      --min-port <PORT>      Minimum accepted TCP port number [env: BORE_MIN_PORT=] [default: 1024]
      --max-port <PORT>      Maximum accepted TCP port number [env: BORE_MAX_PORT=] [default: 65535]
  -v, --verbose...           Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -s, --secret <SECRET>      Optional secret for authentication [env: BORE_SECRET]
```
      --max-conns <N>        Max concurrently proxied connections per client [env: BORE_MAX_CONNS=] [default: 1024]
      --max-carriers <N>     Max parallel TCP carriers a tunnel may use (1 disables the pool) [env: BORE_MAX_CARRIERS=] [default: 16]
      --control-port <PORT>  TCP port the control connection listens on [env: BORE_CONTROL_PORT=] [default: 7835]
      --bind-domain <DOMAIN> Public domain advertised to clients [env: BORE_BIND_DOMAIN=]
      --cert-file <PATH>     TLS certificate chain (PEM); with --key-file, serves HTTPS [env: BORE_CERT_FILE=]
      --key-file <PATH>      TLS private key (PEM); with --cert-file, serves HTTPS [env: BORE_KEY_FILE=]
      --bind-addr <IP>       IP address to bind to, clients must reach this [default: 0.0.0.0]
      --bind-tunnels <IP>    IP address where tunnels will listen on, defaults to --bind-addr
      --udp                  Broker UDP direct paths and run a STUN responder on the control port [env: BORE_UDP=]
      --admin-token <TOKEN>  Enable the admin status page at /admin/status (min 32 chars) [env: BORE_ADMIN_TOKEN]
  -h, --help                 Print help
```shell
```

#### Basic auth on tunnels

Any tunnel — public or secret — can be protected with HTTP Basic auth via
`--basic-auth "user:pass"` on `bore local`. HTTP requests without valid
credentials get a `401`; non-HTTP traffic is forwarded unprotected (Basic auth is
HTTP-only). For a **public** tunnel the server enforces it; for a **secret** tunnel
the provider enforces it (covering both the relay and the direct UDP path), so the
credentials never leave the provider. Use it over TLS so the credentials are not
sent in the clear.

```shell
bore local 8080 --to https://bore.tld -p 9000 --https --basic-auth "admin:s3cr3t"
```
```
```shell

#### Admin status page

Start the server with `--admin-token <TOKEN>` (at least 32 characters) to enable a
read-only status dashboard at **`/admin/status`** on the control port. It is served
over the same scheme as the control connection — `http://host:7835/admin/status`,
`https://bore.tld/admin/status`, etc. (the control port is configurable, the path
is the same). Without `--admin-token` the page is disabled and the control port
speaks only the bore protocol.

```shell
bore server --secret mysecret --admin-token "$(openssl rand -hex 24)"
# open http://your-server:7835/admin/status and paste the token
```

```
The page lists every connected tunnel — public tunnels and, for secret tunnels,
```shell
both the provider and all attached `bore proxy` consumers — with their client
address, options, `--notes`, live connection count, and uptime. It refreshes
automatically (polling every ~2s) and keeps **no** persistent state: it reflects
exactly what is connected right now. The frontend is embedded in the binary; no
external assets are fetched.

Annotate any tunnel with `--notes "..."` (on `bore local` or `bore proxy`) to label
it on this page.

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
```
# public port is opened on the server.
```shell
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

```
Usage: bore proxy [OPTIONS] --local-proxy-port <ADDR> --tcp-secret-id <ID>
```shell

Options:
      --local-proxy-port <ADDR>  Local address to listen on, e.g. ":5555" or "127.0.0.1:5555" [env: BORE_LOCAL_PROXY_PORT=]
  -v, --verbose...               Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -t, --to <ADDR>                Address of the remote server [env: BORE_SERVER=] [default: https://bore.0912345.xyz]
  -s, --secret <SECRET>          Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>       Identifier of the secret tunnel to connect to [env: BORE_TCP_SECRET_ID=]
      --insecure                 Skip TLS certificate verification [env: BORE_INSECURE=]
      --udp                      Prefer a direct UDP hole-punched path [env: BORE_PREFER_UDP=]
```
      --stun-server <HOST:PORT>  STUN server for the direct path [env: BORE_STUN_SERVER=]
```shell
      --upnp                     Map a router port via UPnP-IGD for the direct path [env: BORE_UPNP=]
      --try-port-prediction      Advertise predicted symmetric-NAT ports (opt-in, best-effort) [env: BORE_TRY_PORT_PREDICTION=]
      --nat-udp-preferred-port <PORT> Bind the UDP hole-punch socket to a fixed port (0=random) [env: BORE_NAT_UDP_PORT=]
      --nat-udp-release-timeout <SECS> Re-check interval when the NAT remapped the preferred UDP port (default 600s) [env: BORE_NAT_UDP_RELEASE_TIMEOUT=]
      --notes <TEXT>             Note shown on the server's admin status page [env: BORE_NOTES=]
      --carriers <N>             Parallel TCP carrier connections for the relay data path (default 1) [env: BORE_CARRIERS=]
      --auto-reconnect           Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
  -h, --help                     Print help
```

#### Direct UDP path (hole-punching)

By default a secret tunnel relays all data through the server. With `--udp` on the
server **and** on both ends, `bore` instead tries to establish a **direct**
```
peer-to-peer path between the provider and the consumer using UDP hole-punching,
rendezvous/signaling point and steps out of the data path (lower latency, no server
bandwidth). If the direct path can't be established (e.g. a symmetric NAT, UDP
tunnel.
- Cross-platform path handling is explicit, not lossy: Unix raw-byte path
  components and Windows UTF-16 path components are encoded on the wire; on
  Windows, reserved or otherwise invalid names are sanitized to
  `_bore_utf8_<hex>` instead of being lossy-decoded.

- Unix device transfer is meaningful only on Unix receivers and may require
  elevated privileges to recreate the device node.
bore local 8080 --to https://bore.tld --secret mysecret --tcp-secret-id svc --udp
bore proxy --to https://bore.tld --local-proxy-port :5555 --secret mysecret --tcp-secret-id svc --udp
```

Notes:

- **Requires the `udp` feature**, which is **on by default**. Build
  `--no-default-features` to drop it (and the `quinn` dependency).
- **VPN (Linux)** — point-to-point L3 tunnel; build with `--features vpn`.
  Feature-complete and netns-validated on Linux; macOS/Windows are groundwork only.
  Direct-path throughput: a single QUIC flow is bounded by `socket buffer / RTT`.
  bore requests 16 MiB UDP buffers and, running with `CAP_NET_ADMIN`, **forces past**
  the `net.core.{w,r}mem_max` clamp (`SO_*BUFFORCE`) so a stock 208 KiB ceiling
  (~10 MB/s at 20 ms RTT) does not cap it — startup logs `forced=true`. Unprivileged?
  raise `net.core.{r,w}mem_max` on both ends. The biggest real-world limiter is a
  **single inner TCP flow over a lossy path** (Mathis-bound): parallelise the
  workload, not bore. `--carriers` rarely helps a VPN and defaults to `1` — see
  [Tuning VPN throughput](#tuning-vpn-throughput).
- **Reflexive discovery (STUN).** Each peer learns its public address from a STUN
  chain: Cloudflare on the standard `3478/udp` first, then Google, then the
  server's built-in STUN responder on the control port over **UDP** as the final
  fallback. Open **UDP** on the control port too (e.g. `7835/udp`) if you want
  that self-hosted fallback; override the whole chain with `--stun-server
  host:port`.
  For secret tunnels, the provider also advertises the STUN server that actually
  produced its reflexive candidate. A `bore proxy --udp` consumer asks the server
  for that provider-selected STUN and, when no explicit `--stun-server` override
  is set, tries it first before continuing with Cloudflare, Google, and the bore
  fallback. A bad or unreachable hint is non-blocking; the relay fallback remains
  available.
- **Authentication.** The direct path is authenticated by a token derived from
  `--secret` and a server-issued nonce, verified before any data flows.
- **Scope & limits.** Only secret tunnels are hole-punchable (not public-port
  tunnels). Reconnecting and multiple consumers are supported (the provider keeps
  a persistent QUIC listener and re-punches toward each one). Both peers behind a
  symmetric NAT → relay.
- To confirm the direct path is in use, look for `using direct udp path` /
  `direct udp carrier established (… token verified)` in the logs. For the full
  control-plane story, use `-vv` or `RUST_LOG=bore_cli=trace,bore=trace`: the
  trace log includes labeled `tx`/`rx` frames for the server/client/proxy/test-udp
  control channels.

**Hard NATs and firewalls.** Two extra, opt-in candidate sources help with
difficult networks (both flags go on `bore local` and `bore proxy`, since both
peers punch):

- `--upnp` — ask the local **home** router to open a port via UPnP-IGD and
  advertise it as a candidate. Helps strict home routers that have a public WAN
  IP; **no effect behind carrier-grade NAT** (mobile/CGNAT), where the mapped
  address is itself private. When active you'll see `UPnP-IGD port mapping
  ENABLED` in the logs.
- `--try-port-prediction` — for **symmetric** NATs (which use a different
  external port per destination), advertise a few ports just past the
  STUN-observed one. **Strictly opt-in**, best-effort, and **may look like a port
  scan to strict firewalls** — so it is off unless you set the flag, and logs a
  clear `port prediction ENABLED` line when used. Often won't help random-port
  NATs.
- `--nat-udp-preferred-port <PORT>` — bind the UDP hole-punch socket to a **fixed**
  port instead of a random one (0 = random, the default). Set the *same* value on
  both peers and open it for **egress** in a strict firewall, and the direct path
  uses exactly that port. On a port-preserving NAT it also fixes the public
  mapping to that port (predictable). Does **not** help symmetric NATs (they remap
  per destination regardless of the local port). Tip: run `bore test-udp
  --nat-udp-preferred-port <PORT>` on each host first to confirm the port punches
  through.

For the genuinely untraversable cases (e.g. CGNAT on both ends), the **server
relay is the reliable fallback** and the tunnel keeps working over it — `--udp`
never makes a tunnel fail.

For the full theory and an exhaustive **A×B (provider × consumer) matrix** of NAT
/ firewall combinations — when the direct path works, when it doesn't, and the
admin fixes (which ports to open, where) — see **[`NAT_TRAVERSAL.md`](NAT_TRAVERSAL.md)**
(in Italian).

#### Secure file transfer (`bore transfer`)

`bore transfer` builds on the existing secret-tunnel transport: it registers a
temporary secret id, tries the direct UDP path by default, and falls back to the
server relay automatically. Filesystem transfers use a V2 chunked protocol with
resume state on the receiver, multiple worker streams, per-chunk BLAKE3 checks,
and a final whole-transfer verification before the staged tree is committed. The
server never stores the payload; it only brokers the rendezvous or relays the
encrypted/plain byte streams when a direct path is unavailable. If `--to` is
omitted, both listener and sender default to `https://bore.0912345.xyz`;
explicit `--to` or `BORE_SERVER` overrides that.

Common receiver:

```shell
bore transfer listener \
  --secret mysecret \
  --transfer-id nightly-backup \
  --dest-path /srv/inbox
```

Try these transfer modes:

```shell
# Single file
bore transfer sender \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/archive.tar.gz \
  --parallel 4 \
  --carriers 4
```

```shell
# Multiple files and directories in one transfer
bore transfer sender \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/report.pdf /home/alice/data/ /home/alice/notes.txt \
  --output bundle \
  --parallel 4
```

```shell
# Source list from file (lines with '#' are comments)
bore transfer sender \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/extra.tar.gz \
  --source-files /home/alice/backup.list \
  --output bundle
```

```shell
# Sender always shows source list; --ask-confirm additionally waits for y/N
bore transfer sender \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/data/ \
  --ask-confirm
```

```shell
# Receiver with --ask-confirm: shows incoming file list and waits for y/N
bore transfer listener \
  --secret mysecret \
  --transfer-id nightly-backup \
  --dest-path ~/received/ \
  --ask-confirm
```

```shell
# Directory (preserves the directory root and relative layout)
bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/project \
  --parallel 4 \
  --symlinks include
```

```shell
# stdin stream (requires an explicit output file name)
tar -cvpzf - project | bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources stdin \
  --output project.tar.gz
```

```shell
# Persistent listener: stays up after each transfer, ready for the next sender
bore transfer listener \
  --secret mysecret \
  --transfer-id nightly-backup \
  --dest-path /srv/inbox \
  --persistent
```

```shell
# Resume a filesystem transfer after an interruption: rerun the same pair with
# the same transfer id, destination root, and unchanged source manifest.
bore transfer listener \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --dest-path /srv/inbox

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/archive.tar.gz \
  --parallel 4
```

```shell
# Force relay-only on both sides.
bore transfer listener \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id relay-only-copy \
  --dest-path /srv/inbox \
  --relay-only

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id relay-only-copy \
  --sources /home/alice/archive.tar.gz \
  --relay-only \
  --carriers 4
```

```shell
# Try the direct UDP path with explicit NAT knobs on both peers; relay remains
# the automatic fallback if hole-punching fails.
bore transfer listener \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id udp-copy \
  --dest-path /srv/inbox \
  --stun-server stun.cloudflare.com:3478 \
  --upnp \
  --try-port-prediction \
  --nat-udp-preferred-port 41641 \
  --nat-udp-release-timeout 120

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id udp-copy \
  --sources /home/alice/archive.tar.gz \
  --stun-server stun.cloudflare.com:3478 \
  --upnp \
  --try-port-prediction \
  --nat-udp-preferred-port 41641 \
  --nat-udp-release-timeout 120
```

```shell
# Control-channel TLS with a self-signed certificate.
bore transfer listener \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id tls-copy \
  --dest-path /srv/inbox \
  --insecure

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id tls-copy \
  --sources /home/alice/archive.tar.gz \
  --insecure
```

```shell
# Existing destination policy lives on the listener.
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-fail --dest-path /srv/inbox

bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-overwrite --dest-path /srv/inbox --overwrite

bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-rename --dest-path /srv/inbox --rename
```

```shell
# Liveness timeouts: reject if the sender doesn't confirm in 30 s; abort stalled
# data within 20 s on both sides.
bore transfer listener \
  --secret mysecret \
  --transfer-id nightly-backup \
  --dest-path /srv/inbox \
  --confirm-timeout 30 \
  --stall-timeout 20

bore transfer sender \
  --secret mysecret \
  --transfer-id nightly-backup \
  --sources /home/alice/archive.tar.gz \
  --stall-timeout 20
```

```shell
# Special files on Unix.
bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id symlink-tree \
  --sources /home/alice/project \
  --symlinks include

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id device-copy \
  --sources /dev/null \
  --devices include
```

What the transfer command guarantees in V2:

- **Chunked filesystem transfer with resume**: regular files are split into
  deterministic chunks, transferred over one or more worker streams, and can be
  resumed by re-running the same sender/listener pair with the same
  `--transfer-id` and unchanged manifest. `stdin` remains a single-stream,
  non-resumable byte stream by design.
- **End-to-end verification** with BLAKE3 at three levels: per chunk, per full
  file, and for the final aggregate transfer summary.
- **Staging + commit** on the receiver: files are written into a temporary tree
  under the destination root and published only after the hashes match.
- **Collision policy** is fail-safe by default. Use `--overwrite` or `--rename`
  on `bore transfer listener` to opt into replacement/renaming.
- **Idempotent re-completion** (content-based): if the link drops after the receiver
  has committed the data but before it can send the `Completed` acknowledgement,
  re-running the same sender with unchanged files is safe — the receiver compares the
  existing destination's content against the manifest and, on a full match, re-sends the
  acknowledgement without re-writing any data. No hidden marker is left behind: a
  successful transfer removes all of its working state from the destination.
- **Parallel filesystem workers** via `--parallel N`. `--parallel 0` (the
  default) is automatic: one worker per CPU core, floored at 4 and capped at 32.
  On the relay path each worker rides its own TCP carrier, so by default
  `--carriers 0` (auto) scales the carrier pool to match the worker count
  (capped at the server's `--max-carriers`, default 16) — every worker gets an
  independent congestion window and there is no single-connection head-of-line
  blocking. Set `--carriers 1` to force the old single-connection path, or a
  fixed `N` to pin it. On direct UDP, carriers are irrelevant: each transferred
  connection already uses an independent native QUIC stream.
- **Cross-platform path fidelity**: the wire format preserves Unix raw-byte path
  components and Windows UTF-16 path components losslessly, so Linux/macOS raw
  names and Windows names survive relay/direct transfer without forcing UTF-8.
- **Live path visibility** in the logs: the sender and listener report whether
  the transfer is on `direct-udp` or `relay`, plus `quic-encrypted`, `tls`, or
  `plain` transport security.

Notes:

- Resume state lives under the destination root's staging directory until the
  transfer commits. If the source manifest changes between attempts, the
  listener rejects the resume and asks for a fresh transfer.
- The listener batches staged-file syncs and resume-state persistence instead of
  forcing one file sync plus one `state.json` rewrite per 256 KiB chunk, so
  resume safety does not throttle large filesystem transfers on fast links.
- `--sources stdin` verifies the exact byte stream that `bore` reads and the
  receiver writes. It cannot know whether the producer command earlier in the
  shell pipeline succeeded semantically; use shell `pipefail` if you need that.
- `--sources stdin` requires `--output`, always uses a single stream, and does
  not participate in chunk resume or `--parallel`.
- `--sources` accepts one or more paths (files or directories) separated by
  spaces. `--source-files FILE…` reads additional paths from text files (lines
  containing `#` are ignored as comments). Both flags may be combined.
- The sender **always** prints the source list (name and size) before connecting.
  `--ask-confirm` on the sender additionally waits for a `y/N` prompt before
  the transfer starts. Works with `/dev/tty` so it is safe to invoke via
  `curl | bash`.
- `--ask-confirm` on the **listener** shows the incoming file list and waits for
  `y/N` after the manifest is received but before any data is written.  If the
  receiver types `n`, a clean rejection message is sent to the sender.
  `--ask-confirm` on the listener is ignored for stdin transfers (the data stream
  starts immediately after the manifest; there is no safe pause point).
- `--confirm-timeout <secs>` on the **listener** (default `120`, `0` = wait
  forever) sets how long to wait for the operator to type `y`/`n` when
  `--ask-confirm` is active. On timeout the transfer is rejected and the sender
  receives a clear error.
- `--stall-timeout <secs>` on both **listener** and **sender** (default `60`, `0`
  = disabled) aborts the transfer if no progress is made on any data read or write
  within the given window. Use `0` on very slow links or when your operating
  system keepalives are sufficient.
- `--persistent` on the listener keeps the listener alive after each transfer;
  errors from individual transfers are logged but do not kill the listener.
- Cross-platform path handling is explicit, not lossy: Unix raw-byte path
  components and Windows UTF-16 path components are encoded on the wire; on
  Windows, reserved or otherwise invalid names are sanitized to
  `_bore_utf8_<hex>` instead of being lossy-decoded.
- Symlinks and Unix device nodes are opt-in/opt-out on the sender with
  `--symlinks include|exclude` and `--devices include|exclude`.
- Unix device transfer is meaningful only on Unix receivers and may require
  elevated privileges to recreate the device node.
- `bore transfer listener` also accepts the legacy `--tcp-secret-id` flag as an
  alias of `--transfer-id`, so existing tooling can reuse the same identifier.

#### Diagnosing UDP / NAT (`bore test-udp`)

Before blaming the tunnel, find out what *your* network allows. `bore test-udp`
opens no tunnel — it probes public STUN servers and, by default, the bore STUN
responder behind `https://bore.0912345.xyz`. Pass `--to` to probe a different
server instead, then classify the NAT and print advice:

```shell
bore test-udp                                 # public STUN + default bore server
bore test-udp --to https://bore.example.com   # public STUN + another bore server
bore test-udp --stun-server stun.l.google.com:19302  # add an explicit STUN server
```

What it tells you:

- **UDP egress** — whether any STUN server answers at all (if none do, UDP is
  blocked outbound and only the relay can work).
- **NAT class** — `open` (public IP), `cone` (endpoint-independent mapping →
  hole-punching works), or `symmetric` (endpoint-dependent → needs the *other*
  peer to be cone/open, and possibly `--try-port-prediction`). For symmetric it
  also reports whether the ports look **sequential** (so prediction has a chance)
  or random.
- **Port preservation**, **CGNAT** (`100.64.0.0/10`) / double-NAT detection, and
  whether a **UPnP-IGD** router is present (so `--upnp` would do something).
- A **co-location/hairpin** note when public STUN works but your own bore
  server's UDP does not — the classic "provider runs on the same host/LAN as the
  server" case, where you should run the provider from a different network or
  pass `--stun-server`.

Run it on **both** peers: a direct path needs each side to be punchable. A cone
consumer that can't reach a provider almost always means the *provider's* host
is the blocker (symmetric/CGNAT/UDP-blocked), not the consumer's.

For a real A<->B check, run paired mode on two machines with the same id. The
server pairs them, exchanges candidates, tests the direct UDP/QUIC path, then
tests the TCP relay fallback. Add `--test-bandwidth` (alias:
`--test-bandwith`) to measure bidirectional throughput and latency on both paths:

```shell
# Machine A
bore test-udp --secret mysecret --tcp-secret-id svc

# Machine B, same command/id
bore test-udp --secret mysecret --tcp-secret-id svc

# With bidirectional bandwidth tests (500 MB per direction and per path)
bore test-udp --secret mysecret --tcp-secret-id svc \
  --test-bandwidth --test-transfer-quota 500MB
```

Paired mode also accepts `--upnp`, `--try-port-prediction`,
`--nat-udp-preferred-port`, `--stun-server`, and `--insecure`, mirroring the
direct-path options used by `local`/`proxy`.

## VPN — Point-to-Point L3 Tunnel (Linux)

`bore vpn` establishes a **point-to-point Layer 3 virtual network interface** between two Linux machines, carrying real IP traffic over bore's NAT-traversing transport. Requires **root** or `CAP_NET_ADMIN`, built with `--features vpn`.

### Requirements

- **Linux only** (kernel TUN/TAP support)
- **Root or `CAP_NET_ADMIN`**
- Build: `cargo build --release --features vpn`
- **Server:** started with `--vpn --vpn-pool <CIDR>`
- **Shared secret:** `--secret` mandatory (required for E2E encryption on the relay fallback path)

### Three Topologies

#### Host ↔ Host

Neither peer advertises a subnet; each side forwards only its own traffic.

```bash
# Machine A (listener)
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id mylink

# Machine B (connector)
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id mylink
```

Both get a `/30` overlay address from the server pool. Ping works immediately. No routes or IP forwarding involved.

#### Site ↔ Host (gateway + roaming client)

```bash
# Machine A: gateway of LAN 192.168.50.0/24
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id site \
  --advertise 192.168.50.0/24

# Machine B: roaming client
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id site
```

Machine B can reach A's LAN. A enables IP forwarding, installs masquerade and MSS-clamp rules automatically.

#### Site ↔ Site (both gateways)

```bash
# Site A gateway (LAN 192.168.50.0/24)
sudo bore vpn listen \
  --to bore.example.com \
  --secret S3cret \
  --id s2s \
  --advertise 192.168.50.0/24

# Site B gateway (LAN 192.168.60.0/24)
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id s2s \
  --advertise 192.168.60.0/24
```

Each gateway installs IP forwarding, NAT, and MSS rules. LAN-to-LAN routing requires each LAN's router to have a route via its bore gateway.

### Security Model

- **Direct path:** QUIC datagrams, QUIC-TLS 1.3 end-to-end. Server not in the data path.
- **Relay fallback:** packets sealed with ChaCha20-Poly1305 (key = `HKDF-SHA256(secret, nonce)`). Server splices opaque ciphertext — never sees plaintext IP headers. Each link/peer derives keys from a fresh per-session CSPRNG nonce with its own monotonic counter, so a `(key, nonce)` pair is never reused — across carriers, multi-queue, an in-place direct↔relay fallback, or a reconnect.
- **Relay replay (known limit):** the relay (your own server) carries opaque ciphertext and cannot read or forge it, but the receiver has no replay window, so a malicious relay could *replay* captured frames. The data plane is best-effort IP and TCP discards duplicates; cross-link replay is impossible (per-link keys). Use the direct path (default) for full end-to-end protection.

### Server Configuration

```bash
bore server \
  --secret S3cret \
  --vpn \
  --vpn-pool 10.99.0.0/16 \
  --vpn-max-links 32
```

- `--vpn`: enable VPN brokering (server must be built with `--features vpn`).
- `--vpn-pool <CIDR>`: allocate `/30` overlay blocks from this pool (required for pool-mode clients).
- `--vpn-max-links <N>`: limit concurrent VPN links (default `32`).

### Client Options

**Core flags (`listen` and `connect`):**

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `-t, --to <ADDR>` | `BORE_SERVER` | `bore.0912345.xyz` | Server address |
| `-s, --secret <SECRET>` | `BORE_SECRET` | **required** | Shared secret |
| `--id <ID>` | `BORE_VPN_ID` | **required** | Link identifier |
| `--advertise <CIDRs>` | `BORE_VPN_ADVERTISE` | — | Subnets to expose (comma-sep); enables gateway mode |
| `--vpn-addr <IP/PREFIX>` | `BORE_VPN_ADDR` | — | Static overlay address (pool mode if omitted) |
| `--vpn-peer-addr <IP>` | `BORE_VPN_PEER_ADDR` | — | Static peer address (requires `--vpn-addr`) |
| `--tun-name <NAME>` | — | `auto` | TUN interface name; `auto` picks the first free `boreN` (bore0, bore1, …) |
| `--mtu <N>` | — | `1350` | TUN interface MTU |
| `--pin-mtu` | — | — | Keep `--mtu` fixed; the direct PMTU monitor only warns on a shortfall, never resizes (tests/benchmarks) |
| `--no-route-manage` | — | — | Print route/NAT commands instead of running them |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | — | Reconnect with exponential backoff |
| `--relay-only` | `BORE_VPN_RELAY_ONLY` | — | Never attempt the direct UDP path; stay on the relay |
| `--carriers <N>` | — | `1` | Parallel carriers (1–16); effective = min(both sides, server `--max-carriers`). **Flow-pinned** (one inner connection → one carrier). Rarely helps a VPN — see [Tuning VPN throughput](#tuning-vpn-throughput) before raising it |
| `--tun-queues <N>` | — | `1` | Linux TUN queues (`IFF_MULTI_QUEUE`, 1–8); one uplink pump per queue |
| `--insecure` | `BORE_INSECURE` | — | Skip TLS cert verification |
| `--notes <TEXT>` | `BORE_NOTES` | — | Operator note (logged on link-up) |

**NAT traversal flags (shared with `local`/`proxy`):**

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--stun-server <HOST:PORT>` | `BORE_STUN_SERVER` | — | Additional STUN server |
| `--upnp` | `BORE_UPNP` | — | UPnP-IGD router-mapped UDP candidate |
| `--try-port-prediction` | `BORE_TRY_PORT_PREDICTION` | — | Predict symmetric-NAT ports |
| `--nat-udp-preferred-port <PORT>` | `BORE_NAT_UDP_PREFERRED_PORT` | `0` | Fixed UDP hole-punch port |
| `--nat-udp-release-timeout <SECS>` | `BORE_NAT_UDP_RELEASE_TIMEOUT` | `0` | Wait before retrying preferred port |

### Performance

TUN I/O uses batch read/write with GSO/GRO offload when the kernel supports `IFF_VNET_HDR`. Auto-detects on startup; falls back to single-packet if unavailable. Measured iperf3 baseline over loopback: **~13,500 Mbps** (single-packet) → **~14,000 Mbps** (GSO/GRO). `--tun-queues N` (Linux multi-queue TUN) adds an uplink pump per queue on high-pps links. The direct path raises the TUN MTU automatically (dynamic PMTU) once QUIC MTU discovery settles.

Large packets drop transiently during the first 1–2 seconds (QUIC MTU discovery), then stabilize.

### Tuning VPN throughput

On a clean LAN/loopback the direct path runs at multi-Gbps and these knobs do
nothing. On a **real Internet path** (RTT + a little loss + a sub-1500 MTU) the
dominant limit is almost always **one inner TCP flow**, not bore. A single TCP
connection over any lossy link is bounded by `≈ MSS / (RTT · √loss)` (Mathis):
e.g. at 40 ms RTT and 0.2 % loss one flow tops out near ~25 Mbit regardless of the
link rate, while **8 parallel flows over the same tunnel reach ~170 Mbit** (netns
emulation, 250 Mbit cap). So:

- **Parallelise the workload, not the tunnel.** Use a multi-stream tool
  (`iperf3 -P 8`, `rclone --transfers`, parallel `rsync`, multi-connection HTTP).
  A single `scp`/`iperf3` (one flow) is the worst case and will *look* like bore is
  slow when it is TCP physics. Quick check: `iperf3 -c <overlay-ip> -P 8`.
- **`--carriers` rarely helps a VPN — leave it at `1` (default).** Carriers open N
  parallel QUIC connections on the direct path. bore now **flow-pins** each inner
  connection to one carrier (so a single flow is never reordered), but a single
  bulk flow still rides a single carrier, and many flows already share one carrier
  fine (one QUIC connection delivers in order). Raise `--carriers` only with **many
  concurrent heavy flows on a clean, high-BDP path**, and measure — on a lossy path
  more carriers usually make it *worse*, not better.
  > Earlier builds round-robined every datagram across carriers, which reordered a
  > single flow and the tunnelled TCP read that as loss — `--carriers 4` could
  > *halve* throughput. Fixed by flow-pinning; carriers are now safe but still
  > seldom useful for a VPN.
- **`--tun-queues N`** helps only when a single uplink task is CPU-bound at very
  high packet rates; otherwise leave at `1`.
- **Don't fight the MTU.** The direct path auto-tunes the TUN MTU to the QUIC path
  MTU. For a stable benchmark, pin it: `--mtu 1280 --pin-mtu` (the monitor then only
  *warns* if the path can't carry it, instead of resizing under your test).
- **Diagnostics:** run both ends with
  `RUST_LOG=info,bore_cli::vpn=debug,bore_cli::holepunch=debug`. The `direct_diag`
  lines report per-carrier `cwnd`, `rtt_ms`, `lost_pct`, `buffer_drop_est` (QUIC
  send-buffer drops), `black_holes` and MTU changes every 5 s — the fastest way to
  tell a single-flow limit (`lost_pct` low, throughput still low → add flows) from a
  bore-side issue. Reproduce locally with `scripts/vpn_bench.sh` (netns + `tc netem`
  WAN emulation); see `docs/vpn/VPN_BANDWIDTH_ASSESSMENT.md`.

### Troubleshooting

- **Link pairs but no ping:** Check `path=` in logs. If `relay`, run `bore test-udp` to diagnose NAT type.
- **Ping ok, TCP slow:** Try `--mtu 1280`; verify MSS-clamp rule: `nft list table inet bore_vpn_<id>`.
- **Works from gateway, not from LAN hosts:** LAN's router needs a route to the peer's LAN via the bore gateway.

### Running Multiple VPN Instances on One Host

By default, `--tun-name` auto-selects the first available interface name (`bore0`, then `bore1`, `bore2`, …). This allows multiple `bore vpn listen` and/or `bore vpn connect` instances to coexist on the same physical host with no manual configuration or collision:

```bash
# Terminal 1: first connector to listener A
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id linkA

# Terminal 2: second connector to listener B (on the same host)
sudo bore vpn connect \
  --to bore.example.com \
  --secret S3cret \
  --id linkB
```

The first instance gets `bore0`, the second `bore1`. To force a specific name, pass `--tun-name myname`; otherwise, auto-naming handles arbitrary instance counts with zero configuration.

### Cleanup

`Ctrl-C` triggers graceful cleanup: routes deleted, IP forwarding restored, nft table dropped, TUN interface removed. State after exit is identical to before the link started. A `SIGKILL` leaves stale state; the next `bore vpn --id <same>` reclaims it automatically.

When several gateway links run on the **same host**, `ip_forward` is reference-counted (`/run/bore-vpn-*.fwdref`): each link restores it only once the last gateway link exits, so tearing one link down never disables forwarding under another still-running one. Server liveness is detected within ~15 s on a broken socket (TCP keepalive) and within 60 s even for a wedged-but-connected server (control-stream heartbeat timeout), after which `--auto-reconnect` re-establishes the link with its forwarding/routes intact.

See **[`docs/vpn/VPN_USER_FULL_GUIDE.md`](docs/vpn/VPN_USER_FULL_GUIDE.md)** for the complete flag reference and use-case guide.

## Vhost — Subdomain Reverse Proxy

`bore vhost` exposes a local HTTP(S) service at a public subdomain without allocating a dedicated TCP port:

```shell
bore vhost 127.0.0.1:8080 \
  --subdomain myapp \
  --id client-id \
  --to bore.mydomain.com
# → http://myapp.bore.mydomain.com   (or https:// when a wildcard cert is configured)
```

All subdomains share ports 80 and 443 on the server. The server reads the `Host` header (after optional TLS termination) and routes each connection to the registered provider for that subdomain.

### DNS prerequisite

Point a wildcard `A`/`AAAA` record at the server:
```
*.bore.mydomain.com  →  <server IP>
  bore.mydomain.com  →  <server IP>
```

### Server configuration (`vhost.yml`)

Enable the vhost frontend by passing `--vhost-config <path>` to `bore server`:

```yaml
base_domain: bore.mydomain.com

# Frontend mode: http | https | both | redirect-https | auto (default)
# 'auto' selects 'http' when no cert is provided, 'both' when one is.
mode: auto

http_port: 80     # default
https_port: 443   # default

# Optional TLS for HTTPS. Use a wildcard certificate (*.bore.mydomain.com).
cert_file: /etc/bore/wildcard.crt
key_file:  /etc/bore/wildcard.key

# Optional default headers injected on every routed request.
default_headers:
  X-Forwarded-Proto: https

# Optional reservations: lock a subdomain to a specific client id.
reservations:
  - subdomain: myapp
    client_id:  my-client-id
    headers:
      X-App-Name: myapp   # merged over default_headers (this key wins)
```

Start the server:
```shell
bore server --vhost-config /etc/bore/vhost.yml
```

### Frontend modes

| Mode | HTTP (port 80) | HTTPS (port 443) | Cert required |
|---|---|---|---|
| `http` | serves | — | no |
| `https` | — | serves | yes |
| `both` | serves | serves | yes |
| `redirect-https` | 308 → https | serves | yes |
| `auto` | serves | serves if cert present | no |

### Hot reload

The server polls `vhost.yml`, `cert_file`, and `key_file` every 2 seconds. On a detected mtime change it reloads atomically — in-flight connections are unaffected.

### `bore vhost` flags

| Flag | Description |
|---|---|
| `<TARGET>` | Local `host:port` to forward to (e.g. `127.0.0.1:8080`) |
| `--subdomain` | Subdomain label to register |
| `--id` | Client identifier for reservation matching |
| `--to` | bore server address |
| `--secret` | Optional server secret |
| `--insecure` | Skip TLS cert verification on `https://` servers |
| `--carriers N` | Parallel relay connections (default 1) |
| `--basic-auth user:pass` | Tell the admin page this provider enforces Basic auth |
| `--notes TEXT` | Free-form note on the admin status page |
| `--auto-reconnect` | Reconnect automatically with backoff on disconnect |

### Server-side overrides

`bore server` accepts these flags in addition to `--vhost-config`:

| Flag | Default | Description |
|---|---|---|
| `--vhost-http-port N` | 80 | Override `http_port` from the config |
| `--vhost-https-port N` | 443 | Override `https_port` from the config |
| `--vhost-mode <mode>` | (from config) | Override `mode` from the config |

## Protocol

There is a _control port_, `7835` by default (configurable with `--control-port`). The client opens a single connection to it — plain TCP, or TLS when reached via `https://` — and [multiplexes](https://github.com/hashicorp/yamux/blob/master/spec.md) everything over that one connection. At initialization, the client opens a control stream and sends a "Hello" message asking to proxy a selected remote port. The server responds with an acknowledgement and begins listening for external TCP connections.

Whenever the server obtains a connection on the remote port, it opens a new multiplexed stream to the client over the existing connection, and proxies the external connection over it. This avoids a fresh TCP (and authentication) handshake per proxied connection. The number of concurrently proxied connections per client is bounded by `--max-conns`.

With `--carriers N` (public tunnels), the client opens `N` connections instead of one: after the "Hello" the server returns a `CarrierToken`, the client opens `N-1` more connections that present it (`JoinCarrier`), and the server round-robins each external connection's multiplexed stream across the pool. The proxied data path is identical — only *which* connection carries each stream changes — so a lost segment stalls only the streams on that one TCP, and each carrier has its own congestion window. The server clamps `N` to `--max-carriers`.

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
