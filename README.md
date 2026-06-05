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
      --udp                    Prefer a direct UDP hole-punched path (secret tunnels only) [env: BORE_PREFER_UDP=]
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

**The UDP direct path needs no `--carriers`.** When a secret tunnel runs over a
direct hole-punched path (`--udp`), each proxied connection already rides its **own
native QUIC stream**, which QUIC keeps independently loss-isolated — so there is no
single-stream head-of-line blocking to fix. `--carriers` widens the relay; `--udp`
fixes the direct path. They compose (the relay pool is used whenever a tunnel is on
the relay fallback).

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
  --source /home/alice/archive.tar.gz \
  --parallel 4 \
  --carriers 4
```

```shell
# Directory (preserves the directory root and relative layout)
bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --source /home/alice/project \
  --parallel 4 \
  --symlinks include
```

```shell
# stdin stream (requires an explicit output file name)
tar -cvpzf - project | bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id nightly-backup \
  --source stdin \
  --output project.tar.gz
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
  --source /home/alice/archive.tar.gz \
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
  --source /home/alice/archive.tar.gz \
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
  --source /home/alice/archive.tar.gz \
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
  --source /home/alice/archive.tar.gz \
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
# Special files on Unix.
bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id symlink-tree \
  --source /home/alice/project \
  --symlinks include

bore transfer sender \
  --to https://bore.example.com \
  --secret mysecret \
  --transfer-id device-copy \
  --source /dev/null \
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
- **Parallel filesystem workers** via `--parallel N`. `--parallel 0` is
  automatic and currently resolves from `--carriers`, capped at 4 workers; with
  the default `--carriers 1`, automatic mode starts one worker. Explicit
  `--parallel` values are clamped to 32. On the relay, `--carriers N` widens
  the data path; on direct UDP, each transferred connection already uses an
  independent native QUIC stream.
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
- `--source stdin` verifies the exact byte stream that `bore` reads and the
  receiver writes. It cannot know whether the producer command earlier in the
  shell pipeline succeeded semantically; use shell `pipefail` if you need that.
- `--source stdin` requires `--output`, always uses a single stream, and does
  not participate in chunk resume or `--parallel`.
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
