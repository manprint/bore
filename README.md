# bore (forked from ekzhang/bore)

[![Build status](https://img.shields.io/github/actions/workflow/status/manprint/bore/ci.yml)](https://github.com/manprint/bore/actions)
[![Crates.io](https://img.shields.io/crates/v/bore-cli.svg)](https://crates.io/crates/bore-cli)

A modern, simple TCP tunnel in Rust that exposes local ports to a remote server, bypassing standard NAT connection firewalls.

![Video demo](https://i.imgur.com/vDeGsmx.gif)

This document is the **single source of truth** for deploying and using every mode of this
fork: public/secret tunnels, vhost subdomain routing, secure file transfer, NAT diagnostics,
the L3 VPN, and the SSH ingress gateway (stock `ssh -R`/`-L`, no `bore` binary needed on the
client). Every CLI flag, every subcommand, and worked `sudo` examples for every mode are
below — nothing is left to another document.

> **This is a hard fork.** VPN (`bore vpn`), the vhost subdomain reverse proxy (`bore vhost`),
> secure file transfer (`bore transfer`), and the SSH ingress gateway (`--ssh-gateway`) do
> **not** exist upstream in [ekzhang/bore](https://github.com/ekzhang/bore) — they are
> features of this repository only. See [Installation](#installation) for which
> distribution channels actually ship them.

```shell
# On your local machine (see "Installation" for how to get a binary of THIS fork)
bore local 8000
```

This exposes local port `8000` to the public internet through the default server
`https://bore.0912345.xyz`, with a public port assigned randomly.

Similar to [localtunnel](https://github.com/localtunnel/localtunnel) and
[ngrok](https://ngrok.io/), except `bore` is intended to be a highly efficient, unopinionated
tool for forwarding TCP/HTTP traffic that is simple to install and easy to self-host, with no
frills attached.

## Table of contents

- [Installation](#installation)
- [Command overview](#command-overview)
- [Detailed usage (`bore local` / `bore proxy`)](#detailed-usage)
  - [Local forwarding](#local-forwarding)
  - [Parallel carriers (`--carriers`)](#parallel-carriers---carriers)
  - [Automatic reconnection](#automatic-reconnection)
  - [HTTPS on the tunnel port](#https-on-the-tunnel-port)
  - [WebSocket support](#websocket-support)
- [Self-hosting (`bore server`)](#self-hosting)
  - [Full server flag reference](#full-server-flag-reference)
  - [Serving over HTTPS/HTTP](#serving-over-httpshttp)
  - [Basic auth on tunnels](#basic-auth-on-tunnels)
  - [Admin status page](#admin-status-page)
- [SSH jump hosts (`bore sshjhost` / OpenSSH provider)](#ssh-jump-hosts)
- [SSH ingress gateway (no `bore` client needed)](#ssh-ingress-gateway)
- [Secret tunnels (no public port)](#secret-tunnels-no-public-port)
  - [Direct UDP path (hole-punching)](#direct-udp-path-hole-punching)
- [Secure file transfer (`bore transfer`)](#secure-file-transfer-bore-transfer)
- [Diagnosing UDP / NAT (`bore test-udp`)](#diagnosing-udp--nat-bore-test-udp)
- [VPN — point-to-point L3 tunnel (`bore vpn`)](#vpn--point-to-point-l3-tunnel)
- [Vhost — subdomain reverse proxy (`bore vhost`)](#vhost--subdomain-reverse-proxy)
- [Access logging](#access-logging)
- [End-to-end deployment recipes](#end-to-end-deployment-recipes)
- [Protocol](#protocol)
- [Authentication](#authentication)
- [Troubleshooting](#troubleshooting)
- [Acknowledgements](#acknowledgements)

## Installation

### This fork's own artifacts (required for VPN / SSH gateway / vhost / transfer)

`cargo install bore-cli`, Homebrew, the AUR, and the Gentoo overlay package below all install
the **upstream** `ekzhang/bore` project from crates.io — a plain TCP tunnel **without** this
fork's VPN, vhost, secure-transfer, or SSH-gateway features. To get everything documented in
this file, install from **this repository** instead, via one of:

**GitHub Releases (recommended, no build toolchain needed).** This fork publishes a release
for **every push** (any branch): named `<branch>-<sha7>` (branch builds are pre-release;
`vX.Y.Z` tags are full releases), with binaries for macOS (x86_64/arm64), Linux (x86_64,
aarch64, arm, armv7, i686), Windows (x86_64/i686), and Android (aarch64) — all built with
`--all-features` (VPN + SSH gateway + UDP direct path). Download from the
[releases page](https://github.com/manprint/bore/releases), unzip, and move the `bore`
executable onto your `PATH`.

**Docker.** Multi-arch images (`linux/amd64` + `linux/arm64`) are pushed to the GitHub
Packages registry on **every** push, tagged by branch, commit and semver:

```shell
docker run -it --init --rm --network host ghcr.io/manprint/bore <ARGS>
```

| Tag | Runs as | Use for |
|---|---|---|
| `latest`, `<version>`, `<major>.<minor>`, `<branch>`, `sha-<sha7>` | uid **1000** (non-root) | server side, and any client that does not need the direct UDP path |
| **`client`**, `client-<version>`, `client-<branch>`, `client-sha-<sha7>` | **root** | client side with `--udp` / `--privileged` (see below) |

The `:client` tag carries the **same binary** as the default image (it is re-tagged from the
same pipeline run, never rebuilt) and is moved on every pipeline run, so it always tracks the
newest build. It only drops the `USER 1000:1000` line, because Docker/runc clears the whole
capability set for a non-root UID — **even under `--privileged`** — and the direct-UDP path
needs an effective `CAP_NET_ADMIN` for `setsockopt(SO_{RCV,SND}BUFFORCE)`. Without it the
kernel clamps the socket buffers to `net.core.{r,w}mem_max`, direct-path throughput is capped
at roughly `buffer/RTT`, and every socket logs `UDP socket buffer clamped below request`:

```shell
# root image + --privileged  ->  forced=true, no clamp warning
docker run -it --init --rm --privileged --network host \
  ghcr.io/manprint/bore:client local 8080 --to bore.example.com --udp
```

The default (non-root) image works fine for the same tunnel — it just runs with the clamped
buffers. The alternative, if you would rather not run as root, is to raise the ceiling on the
host: `sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216` (`--sysctl` on
`docker run` does **not** work here: `net.core.*` is not network-namespaced). Build it
locally with
`docker build -f docker/Dockerfile.client --build-arg BORE_IMAGE=ghcr.io/manprint/bore:latest -t bore:client .`.

The **same non-root constraint has a second face**, and it bites the server rather than the
client: under `--network host` the default image cannot bind a port **below 1024**. It exits
immediately with `Error: Permission denied (os error 13)`, which reads like a Docker
misconfiguration and is not one. The kernel knob that lets an unprivileged process bind a low
port, `net.ipv4.ip_unprivileged_port_start`, is **per network namespace**: Docker sets it to
`0` in every namespace it creates, so a bridge container binds 80/443 as uid 1000 and nobody
ever notices — while host networking shares the *host's* namespace, where it is the stock
`1024`. Measured on Docker 29.8.0 with this image on a free port 81:

| network | user | port | result |
|---|---|---|---|
| bridge | 1000 | 81 | listening |
| host | 1000 | 81 | **Permission denied (os error 13)** |
| host | 1000 | 17835 | listening (any port ≥ 1024 is fine) |
| host | 1000 + `cap_add: NET_BIND_SERVICE` | 81 | **Permission denied** |
| host | `user: "0:0"` | 81 | listening |
| host | 1000, host `ip_unprivileged_port_start=80` | 81 | listening |

`cap_add: [NET_BIND_SERVICE]` is the natural guess and does **not** work, for exactly the
reason above: the capability lands in the bounding and permitted sets, but a non-root
`execve` clears the effective set and Docker does not raise the ambient one. Nor can you
push the sysctl in per-container — the daemon refuses `--sysctl
net.ipv4.ip_unprivileged_port_start=0` with *"not allowed in host network namespace"*, since
with a shared namespace it would be silently changing the host. So, when running the server
with host networking **and** any port below 1024 (typically `BORE_VHOST_HTTP_PORT=80` /
`BORE_VHOST_HTTPS_PORT=443`), pick one: add `user: "0:0"` to the service, or set
`sysctl -w net.ipv4.ip_unprivileged_port_start=80` on the host (persist it in
`/etc/sysctl.d/`), or keep bridge networking for those frontends. Ports ≥ 1024 need none of
this.

Ready-to-run compose files live in [`docker/`](docker/):

| File | Purpose |
|---|---|
| `docker-compose.server.yml` | Basic server, bridge network, control port + tunnel range forwarded explicitly |
| `docker-compose.server.prod.yml` | Full production server: TLS + secret + admin + SSH gateway, every env var documented (optional ones commented) |
| `docker-compose.client.yml` | `bore local` client example |
| `docker-compose.secret-proxy.yml` | `bore proxy` (secret consumer) example |
| `docker-compose-full-yml.yml` | All-in-one server example |

```shell
docker compose -f docker/docker-compose.server.yml up -d
# or the production template:
docker compose -f docker/docker-compose.server.prod.yml up -d
```

For an SSH-gateway **client** container (no `bore` binary, just OpenSSH + autossh), see
[`compose.ssh.yml`](compose.ssh.yml) at the repo root — a fully documented, env-var-driven
`autossh` wrapper image (`ghcr.io/manprint/bore-ssh-client`) with one example service per
tunnel mode (vhost/public/secret provider/secret consumer).

Server-side UDP, relay, Docker networking, carrier, and file-descriptor tuning notes are in
[`docs/server/SERVER_UDP_OPTIMIZATION.md`](docs/server/SERVER_UDP_OPTIMIZATION.md).

**Build from source:**

```shell
cargo install --git https://github.com/manprint/bore --locked bore-cli --features vpn,ssh-gateway
# or, from a checked-out clone:
cargo build --release --all-features
```

**Cross-compilation via Docker** — [`Justfile`](Justfile) builds release binaries into
`./bin/` for several targets (`just --list`):

```shell
just build-amd64       # Linux x86_64
just build-arm64       # Linux aarch64
just macos-m5          # macOS Apple Silicon (aarch64-apple-darwin)
just windows-amd64     # Windows x86_64
just build             # all of the above
just push              # build + push a multi-arch (amd64+arm64) image to Docker Hub
```

### Upstream-only channels (plain tunnel, no VPN/vhost/transfer/SSH-gateway)

```shell
cargo install bore-cli        # crates.io — upstream ekzhang/bore
brew install bore-cli         # Homebrew core formula — upstream
yay -S bore                   # AUR — upstream
```

```shell
# Gentoo (gentoo-zh overlay) — upstream
sudo eselect repository enable gentoo-zh
sudo emerge --sync gentoo-zh
sudo emerge net-proxy/bore
```

Fine if you only need a basic public/secret TCP tunnel and don't need VPN, vhost, transfer,
or the SSH gateway.

## Command overview

Commands and utilities. `-v`/`-vv` (global) raises log verbosity to
debug/trace; `RUST_LOG` overrides. All flags accept the matching `BORE_*` environment
variable shown in each table — handy for Docker/systemd.

| Command | Role | Section |
|---|---|---|
| `bore local <PORT>` | Expose a local port: public tunnel, or secret-tunnel provider with `--tcp-secret-id` | [Detailed usage](#detailed-usage) |
| `bore proxy` | Consume a secret tunnel, exposing it on a local port | [Detailed usage](#detailed-usage) |
| `bore vhost <TARGET>` | Expose local HTTP(S) at a subdomain, no dedicated port | [Vhost](#vhost--subdomain-reverse-proxy) |
| `bore sshjhost <TARGET>` | Publish an SSH daemon as a namespaced stock-OpenSSH ProxyJump target | [SSH jump hosts](#ssh-jump-hosts) |
| `bore server` | Run the relay server (control port, tunnels, vhost, VPN broker, SSH gateway, admin page) | [Self-hosting](#self-hosting) |
| `bore transfer listener` / `bore transfer sender` | Resumable, BLAKE3-verified file transfer over the tunnel transport | [Secure file transfer](#secure-file-transfer-bore-transfer) |
| `bore test-udp` | NAT/UDP diagnostic; two-peer latency/bandwidth test with `--tcp-secret-id` | [Diagnosing UDP/NAT](#diagnosing-udp--nat-bore-test-udp) |
| `bore vpn listen` / `bore vpn connect` | Point-to-point L3 VPN (`--features vpn`, root/`CAP_NET_ADMIN`) | [VPN](#vpn--point-to-point-l3-tunnel) |
| `bore hash-password` | Generate an Argon2id hash line for `--ssh-passwords-file` (`--features ssh-gateway`) | [SSH ingress gateway](#ssh-ingress-gateway) |

The default server for every client/proxy/transfer/test-udp command, if `--to`/`BORE_SERVER`
is omitted, is `https://bore.0912345.xyz`.

The `--to` value selects the control-connection transport:

| `--to` | Scheme | Default port | TLS |
|---|---|---|---|
| `bore.tld` | bare | control port (`7835`) | no |
| `bore.tld:9000` | bare + port | 9000 | no |
| `http://bore.tld` | http | 80 | no |
| `https://bore.tld` | https | 443 | **yes** (`--insecure` for self-signed) |
| `https://bore.tld:7835` | https + port | 7835 | **yes** |

## Detailed usage

This section describes detailed usage for the `bore` CLI command.

### Local forwarding

You can forward a port on your local machine by using the `bore local` command. This takes a
positional argument, the local port to forward. If you omit `--to`, the client defaults to
`https://bore.0912345.xyz`; pass `--to` or `BORE_SERVER` to override it.

```shell
bore local 5000
```

You can optionally pass in a `--port` option to pick a specific port on the remote to expose,
although the command will fail if this port is not available. To expose a different host on
your local area network besides the loopback address `localhost`, either pass
`--local-host`, or embed the host directly in the positional argument as `HOST:PORT` (same
syntax as `bore vhost`'s target):

```shell
bore local 10.10.16.138:5000
```

The two ways of specifying the host must agree: passing both `--local-host` and an embedded
`HOST:PORT` is fine as long as the hosts match, but a mismatch (e.g.
`bore local 10.10.16.138:5000 --local-host 192.168.1.1`) is rejected with a "conflicting
host" error instead of silently picking one.

```text
Starts a local proxy to the remote server

Usage: bore local [OPTIONS] <PORT>

Arguments:
  <PORT>  The local port to expose, or HOST:PORT to target a non-localhost
          service [env: BORE_LOCAL_PORT=]

Options:
  -l, --local-host <HOST>      The local host to expose (default: localhost;
                                must match PORT's embedded host if it has
                                one, else rejected as conflicting)
  -v, --verbose...              Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -t, --to <ADDR>               Address of the remote server [env: BORE_SERVER=] [default: https://bore.0912345.xyz]
  -p, --port <PORT>             Optional port on the remote server to select [default: 0]
  -s, --secret <SECRET>         Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>      Register as a named secret tunnel [env: BORE_TCP_SECRET_ID=]
      --insecure                Skip TLS certificate verification [env: BORE_INSECURE=]
      --https[=<off|on|redirect>]   Per-tunnel HTTPS policy; bare --https = on. off=plain/raw only; on=terminate TLS, serve HTTP+HTTPS; redirect=terminate TLS + 308 HTTP→https. Absent inherits the server default; a request without server TLS falls back to HTTP with a warning [env: BORE_HTTPS=]
      --force-https             Deprecated: use --https=redirect (kept as an alias) [env: BORE_FORCE_HTTPS=]
      --udp                     Prefer a direct UDP/QUIC data path (public: server→client QUIC; secret: hole-punched). Falls back to relay. [env: BORE_PREFER_UDP=]
      --stun-server <HOST:PORT> STUN server for the direct path [env: BORE_STUN_SERVER=]
      --upnp                    Acquire a managed router mapping (PCP first, UPnP-IGD fallback; renewed + released automatically) [env: BORE_UPNP=]
      --udp-candidate <IP:PORT> Manual public endpoint to advertise (repeatable / comma-separated) [env: BORE_UDP_CANDIDATES=]
      --udp-no-stun             Skip STUN discovery; manual/local/port-mapped candidates only [env: BORE_UDP_NO_STUN=]
      --try-port-prediction     Advertise predicted symmetric-NAT ports (opt-in, best-effort) [env: BORE_TRY_PORT_PREDICTION=]
      --nat-udp-preferred-port <PORT> Bind the UDP hole-punch socket to a fixed port (0=random) [env: BORE_NAT_UDP_PORT=]
      --nat-udp-release-timeout <SECS> Re-check interval when the NAT remapped the preferred UDP port (default 600s, 0=disable) [env: BORE_NAT_UDP_RELEASE_TIMEOUT=]
      --max-conns <N>           Max concurrent connections on the direct UDP path (default 1024) [env: BORE_MAX_CONNS=]
      --basic-auth <USER:PASS>  Protect the tunnel with HTTP Basic auth [env: BORE_BASIC_AUTH]
      --notes <TEXT>            Note shown on the server's admin status page [env: BORE_NOTES=]
      --carriers <N>            Parallel TCP carrier connections for the data path (public tunnels; default 1) [env: BORE_CARRIERS=]
      --auto-reconnect          Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
      --webserver-log <DIR>     Write access logs in nginx-combined format to <DIR> [env: BORE_WEBSERVER_LOG=]
      --webserver-log-max-files <N> Max rotated log files per target (default 4) [env: BORE_WEBSERVER_LOG_MAX_FILES=]
      --webserver-log-max-file-size <MB> Max MiB per log file before rotation (default 100) [env: BORE_WEBSERVER_LOG_MAX_FILE_SIZE=]
  -h, --help                    Print help
```

```text
Connects to a named secret tunnel and exposes it on a local port

Usage: bore proxy [OPTIONS] --local-proxy-port <ADDR> --tcp-secret-id <ID>

Options:
      --local-proxy-port <ADDR>  Local address to listen on, e.g. ":5555" or "127.0.0.1:5555" [env: BORE_LOCAL_PROXY_PORT=]
  -v, --verbose...               Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -t, --to <ADDR>                Address of the remote server [env: BORE_SERVER=] [default: https://bore.0912345.xyz]
  -s, --secret <SECRET>          Optional secret for authentication [env: BORE_SECRET]
      --tcp-secret-id <ID>       Identifier of the secret tunnel to connect to [env: BORE_TCP_SECRET_ID=]
      --insecure                 Skip TLS certificate verification [env: BORE_INSECURE=]
      --udp                      Prefer a direct UDP hole-punched path [env: BORE_PREFER_UDP=]
      --stun-server <HOST:PORT>  STUN server for the direct path [env: BORE_STUN_SERVER=]
      --upnp                     Acquire a managed router mapping (PCP first, UPnP-IGD fallback; renewed + released automatically) [env: BORE_UPNP=]
      --udp-candidate <IP:PORT>  Manual public endpoint to advertise (repeatable / comma-separated) [env: BORE_UDP_CANDIDATES=]
      --udp-no-stun              Skip STUN discovery; manual/local/port-mapped candidates only [env: BORE_UDP_NO_STUN=]
      --try-port-prediction      Advertise predicted symmetric-NAT ports (opt-in, best-effort) [env: BORE_TRY_PORT_PREDICTION=]
      --nat-udp-preferred-port <PORT> Bind the UDP hole-punch socket to a fixed port (0=random) [env: BORE_NAT_UDP_PORT=]
      --nat-udp-release-timeout <SECS> Re-check interval when the NAT remapped the preferred UDP port (default 600s) [env: BORE_NAT_UDP_RELEASE_TIMEOUT=]
      --notes <TEXT>             Note shown on the server's admin status page [env: BORE_NOTES=]
      --carriers <N>             Parallel TCP carrier connections for the relay data path (default 1) [env: BORE_CARRIERS=]
      --auto-reconnect           Reconnect automatically with backoff if the connection drops [env: BORE_AUTO_RECONNECT=]
  -h, --help                     Print help
```

### Parallel carriers (`--carriers`)

By default a public tunnel multiplexes **every** proxied connection over a **single** TCP
connection to the server. Under packet loss that causes cross-connection head-of-line
blocking (one flow's lost segment stalls all the others sharing the TCP), and every flow
shares one TCP congestion window.

`--carriers N` opens **N parallel TCP connections** and spreads proxied connections across
them (round-robin). A lost segment then only stalls the ~1/N flows on that carrier, and each
carrier gets its own congestion window:

```shell
bore local 8080 --to bore.tld -p 9000 -s mysecret --carriers 4
```

It applies to **every relay leg**, because the server is always in the relay data path:

- **Public tunnel** (`bore local --carriers`): the server→client leg.
- **Secret provider** (`bore local --tcp-secret-id --carriers`): the server→provider leg
  (the bottleneck shared by *all* consumers of that provider).
- **Secret consumer** (`bore proxy --carriers`): the consumer→server leg.

```shell
bore local 8080 --to bore.tld -p 9000 -s mysecret --carriers 4          # public
bore local 8080 --to bore.tld --tcp-secret-id app -s mysecret --carriers 4   # provider
bore proxy --to bore.tld --tcp-secret-id app -s mysecret --local-proxy-port :5555 --carriers 4
```

When it helps and when it doesn't:

- **Helps** concurrent workloads: parallel `rclone`/S3/WebDAV transfers, browsers (many
  requests), streaming — especially on a lossy or high-latency link to the server.
- **No change** for a single bulk transfer (one flow = one carrier). For single-flow
  loss/high-BDP, tune the **host** instead: `sysctl net.ipv4.tcp_congestion_control=bbr`
  (bore can't set per-socket congestion control without `unsafe`).
- The server is always in the relay data path, so this does **not** add bandwidth or bypass
  the server — it removes the single-TCP bottleneck on the relay leg only.

The server caps `N` at its `--max-carriers` (default 16) for public tunnels and providers; a
larger request is clamped, and `--max-carriers 1` disables the pool (single connection). A
carrier that drops mid-session is re-dialed automatically; the tunnel never breaks (it just
runs with fewer carriers until the re-dial succeeds). Default `1` = unchanged behaviour.

**The UDP direct path needs no `--carriers` for secret tunnels and transfer.** When a secret
tunnel runs over a direct hole-punched path (`--udp`), each proxied connection already rides
its **own native QUIC stream**, which QUIC keeps independently loss-isolated — so there is no
single-stream head-of-line blocking to fix. `--carriers` widens the relay; `--udp` fixes the
direct path. They compose (the relay pool is used whenever a tunnel is on the relay
fallback).

**Exception — `bore vhost --udp` and `bore local --udp` (public tunnel):** there `--carriers
N` *also* sizes the QUIC direct path. The client/provider opens `N` parallel QUIC
**connections** and the server pools them and round-robins proxied connections across them,
parallelizing per-connection crypto/congestion across cores (capped at 32, not by
`--max-carriers`). Both need `bore server --udp`; both fall back to the TCP relay
per-connection when the direct path is unavailable. As always, a single flow over one
connection is not split — see
[`docs/performance/CARRIER_TUNING.md`](docs/performance/CARRIER_TUNING.md).

**Measured recommendation for `bore vhost` (staging, 2026-09-11).** There is no single
best carrier count; pick by workload:

| workload | setting | measured |
| --- | --- | --- |
| web app / file server: small requests alongside occasional bulk | `--carriers 8` | small-request p95 under one bulk transfer 14.06 → **4.92 ms**; 393 → **1162 req/s** under two; costs ~11 % of peak single-transfer bandwidth |
| one big transfer at a time | `--carriers 1` (default) | **59.10 MB/s** down / **93.67** up, against 52.55 / 72.17 at 8 — carriers split one flow's congestion window |
| unknown or mixed | `--carriers 0` (adaptive) | matches static 8 (median ratio 1.033) while holding one carrier; grows only when a small request actually contends with bulk |

On a clean idle path carriers still cost (median c4/c1 = 0.88), which is why `1` is the
default. **`--udp` is not a bandwidth upgrade**: against a real file server the TCP relay was
faster on small requests (93.93 vs 103.74 ms p50), equal or better on sustained bulk, about
half the CPU per GiB, and 36× cheaper in cloud-instance network allowance. Turn it on for
many simultaneously held connections (14 ms vs 1436 ms behind 512) or for lossy paths (2.7×).

**Measured recommendation for a PUBLIC tunnel (`bore local`), staging 2026-09-11.** The
numbers are NOT the same as vhost's above, and the difference is not noise — see
[`docs/performance/PUBLIC_STAGING_EVIDENCE_2026-09-11.md`](docs/performance/PUBLIC_STAGING_EVIDENCE_2026-09-11.md).

| workload | setting | measured |
| --- | --- | --- |
| several concurrent connections (a browser, parallel transfers) | `--carriers 4` | 4 concurrent transfers: **303.60 MB/s** down at 4 carriers against 210.25 at 1 (**1.44×**); upload peaks at 2 (**229.30** against 154.90, 1.48×) |
| one big transfer at a time | `--carriers 1` (default) | a single flow rides one carrier whatever N is; the extra sockets only cost |
| — | `--carriers 8` | worse than 4 in BOTH directions — the server's two vCPUs and the instance allowance become the bound |

Why the sign differs from vhost: the vhost ladder moved ONE bulk flow (one carrier used, the
rest idle overhead), the public ladder moves FOUR concurrent connections, which the server
actually spreads. Match the setting to the concurrency, not to the transport.

**`--udp` on a public tunnel is not a bandwidth upgrade either.** On a clean in-region path
the TCP relay won all ten paired runs — median **1.51×** on download, **1.34×** on upload —
and won the small-request tail as well (p95/p99 **6.23/8.77 ms** relay against 10.76/17.79 ms
direct). It pays off where the relay cannot compete: at 32 concurrent HTTP connections the
direct path took the tail (p95 3.48 vs 4.58 ms, p99 3.96 vs 7.27 ms), and on a lossy path it
is the one that keeps serving.
Full evidence:
[`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md`](docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md),
and in Italian
[`docs/performance/final_vhost_perf_review.md`](docs/performance/final_vhost_perf_review.md) §12.

**Proxy copy buffer:** `BORE_PROXY_BUFFER_SIZE` (default 256 KiB; accepts a
`KB`/`MB`/`GiB`/... suffix, clamped `[4 KiB, 16 MiB]`) sets the per-direction relay/splice
buffer. Set it on the server (relay buffers) and/or a provider (local splice); a larger
buffer helps high-latency, high-BDP links, not single-stream throughput on a fast LAN.
The resolved value is reported as `proxy_buffer_size` by `GET /admin/api/v1/config` and in
the dashboard's Configuration panel, so a deployment can be checked against the default
rather than assumed.

**Direct-path QUIC liveness:** `BORE_DIRECT_QUIC_KEEPALIVE_MS` (default 3000) and
`BORE_DIRECT_QUIC_IDLE_MS` (default 10000, clamped `[1 s, 600 s]`) set the keep-alive
interval and idle timeout of a `--udp` direct connection. The idle timeout is also the
width of the window in which requests already committed to a direct stream are lost when
UDP stops working: with the peer silent, opening the stream and writing `STREAM_READY`
both succeed locally, so the request waits for the connection itself to time out before
the next one falls back to the warm TCP relay (which takes ~12 ms and is transparent).
Lowering the idle timeout shortens that window proportionally. The resolver keeps two
consecutive keep-alive losses survivable by **tightening the keep-alive** when the
requested pair would not (`max_idle >= 3 × keepalive`), and warns when it does — so the
value in force can differ from the value requested, and both `direct_quic_keepalive_ms`
and `direct_quic_idle_ms` are published on `GET /admin/api/v1/config`.

**Reading what is actually in force:** `GET /admin/api/v1/config` derives the whole direct-UDP block from the tuning installed on the server rather than restating the CLI strings, because `--udp-memory-budget` computes the three windows from one number after startup. `udp_direct_slots` reports the aggregate admission bound the budget derived (`null` = no budget, the historical unbounded path) — the CONFIGURED total, which does not move with load; the live "how many are free right now" gauge is `udp_direct_slots_available` on `/admin/api/v1/metrics`.

For bulk transfers, the direct QUIC path is tuned in code with larger flow-control windows
than Quinn's defaults: `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` (16 MiB),
`DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` (256 MiB), and `DIRECT_QUIC_SEND_WINDOW` (256 MiB) in
`src/shared.rs`. The same defaults can now be overridden on `bore server` with
`--udp-stream-receive-window`, `--udp-connection-receive-window`, `--udp-send-window`,
`--udp-socket-recv-buffer`, `--udp-socket-send-buffer`, and `--udp-max-streams` (or the
matching `BORE_...` env vars, see the [full server flag reference](#full-server-flag-reference));
the server brokers the chosen tuning to the direct-path peers. Bore also requests
`DIRECT_UDP_SOCKET_RECV_BUFFER` and `DIRECT_UDP_SOCKET_SEND_BUFFER` (16 MiB each), sets
`MAX_DIRECT_STREAMS` to 4096, keeps QUIC alive every 3s with a 10s idle timeout, and uses
`quinn::congestion::BbrConfig` for the direct path. If `bore test-udp --test-bandwidth` shows
UDP direct with lower latency but less throughput than TCP relay, that is not automatically a
bug: QUIC is reliable and congestion-controlled over UDP, while the relay uses highly
optimized kernel TCP and may sit close to one peer. Tune those constants only after measuring
both directions with a realistic quota.

**Which transport, and how much CPU it costs.** Measured on a fixed staging deployment
(AWS `t4g.micro`, 2 burstable Graviton2 vCPU, TLS, 1500 MTU) with paired A/B runs, because a
29 % drift in the control made unpaired comparisons meaningless:

- **On a clean, low-RTT path the TCP relay is the right transport, not `--udp`.** 8 of 8
  paired runs favoured the relay, **median 1.589×**. The relay also costs about half the
  client CPU, **2.5× less server CPU per byte**, and **4.5× less server memory** under
  concurrency.
- **`--udp` is a remedy for a lossy or long-RTT path**, and a good one: **2.8×** the relay
  at 1 % packet loss and **2.2×** at +40 ms RTT. On a clean path it is 25–110 % *slower*.
- **`--udp` is not a bandwidth transport.** It tops out at **0.96 Gbit/s** — below a 1 Gbit/s
  link — and **no flag moves it**: `--carriers 1→4` and 1→4 parallel streams take it from 99
  to 120 MB/s and no further. It also draws instance-level packet-rate throttling that the
  relay never triggers. Choose it for resilience, never for throughput.
- **Carriers default to 1 and that is correct** for an idle tunnel: the paired median
  `c=4/c=1` ratio is **0.941**. They earn their keep under loss (~1 MB/s per carrier at 1 %
  loss) and under concurrent bulk load (3× p50 at `c=8`) — which is what
  [`--carriers 0`](#adaptive-carriers-carriers-0) tracks automatically on a vhost.

**Sizing a relay deployment.** The transferable number is **CPU-seconds per GiB**, not MB/s:
absolute throughput is a property of one box and one link, while cost per byte survives a
change of hardware. Measured: **5.74 CPU-seconds per GiB** for the TCP relay under load, on
one Graviton2 core with TLS at 1500 MTU. Divide the target rate by it:

| target | relay cores needed |
| --- | --- |
| 1 Gbit/s | 0.67 |
| 2.68 Gbit/s | 1.90 (the measured ceiling on 2 burstable Graviton2 cores) |
| 5 Gbit/s | 3.34 |
| 10 Gbit/s | 6.7 |

So **a 5 Gbit/s deployment wants 6–8 vCPU and `--carriers 4`**: bore was measured using up to
3.61 cores, and 4 vCPU would leave no margin for TLS handshakes, the admin endpoint and the
OS. x86 should be cheaper per byte than Graviton2 at the same clock, so treat 3.34 as the
ceiling of the estimate rather than the middle of it. The three-script measurement sequence
that produces these numbers on any host is documented in `scripts/perf/README.md`.

**Re-running the whole staging campaign.** `scripts/perf/staging/` holds the complete
harness behind
[`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md`](docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10_DEV_RESULT.md)
(English evidence) and
[`docs/performance/final_vhost_perf_review.md`](docs/performance/final_vhost_perf_review.md)
(Italian review): a server host, a same-region measurement VM and a domestic consumer,
driven from one `env.sh` that never enters the repository. Copy
`scripts/perf/staging/env.sh.example`, fill in the coordinates of whichever deployment you
are measuring, run `scripts/perf/staging/provision.sh`, then follow
`scripts/perf/staging/README.md`. Nothing in the tree hardcodes a host, a domain, a key path
or a credential, so pointing it at a different (for example pre-production) server is a
config change, not a code change.

**The browser leg speaks HTTP/1.1, and that is a measured decision, not an omission.** The
vhost edge does not offer `h2` in its TLS ALPN list, so every browser falls back to HTTP/1.1
and opens its usual ~6 connections. Measured with a client at controlled RTT behind netem
(`scripts/vhost_h2_page_load.sh`), on a 31-asset page: **the tunnel contributes only 1–3 ms of
the page at every RTT from 2 ms to 100 ms** — page-load time is the browser's TLS round trips,
not bore's work. HTTP/2 would collapse those round trips and is **1.49–1.74× faster on
small assets alone**, but it loses **half the bulk throughput** (0.51× on a 2 MiB asset at
2 ms RTT) because every stream shares one connection. Net effect on a realistic mixed page:
**0.73× at 2 ms RTT, 1.07× at 21 ms, 1.45× at 100 ms.** So h2 would make a same-region page
slower and only pays for a distant audience serving small-asset pages. Full tables, the graft
study and the recommendation are in
`docs/performance/VHOST_STAGING_EVIDENCE_2026-09-10.md` §10.

**Direct-path memory: the sizing rule.** The QUIC connection receive window is a
per-connection **ceiling, not a reservation** — a healthy tunnel buffers approximately
nothing. It only fills when the public reader stops draining, which is what a browser
pausing assets or a mobile client on a bad link does. The worst case is therefore:

```
tunnels x carriers x connection_receive_window   <=   available RAM
```

At the defaults that is 256 MiB per tunnel-carrier, so one `--udp --carriers 4` tunnel can
ask the server to hold 1 GiB, and ten tunnels have no bound at all. Measured on a 903 MiB
host: 32 slow readers on a *single* `--udp` tunnel took the server to 536.8 MiB, which timed
out two requests, failed a registration, and made an unrelated tunnel reconnect. It
recovered (no OOM kill, RSS back to 33.6 MB afterwards), but one client's behaviour had
degraded the whole server. The equivalent TCP relay load held 95.1 MiB.

`--udp-memory-budget <SIZE>` bounds it with one number instead of four that must stay in a
fixed ratio:

```shell
# A 2 GiB VPS: give the direct path at most 512 MiB, in windows sized for
# 4-carrier tunnels.
bore server --udp --max-carriers 4 --udp-memory-budget 512MiB
#   -> 128 MiB connection window, 8 MiB stream window, 4 admission slots
```

The connection window becomes `budget / --max-carriers`, so one tunnel at its full carrier
count fits inside the budget; the 16:1 connection-to-stream ratio is always preserved (see
below for why it is load-bearing), and the window is never inflated past the tested 256 MiB
default — a generous budget buys **concurrency** (more slots) rather than bigger windows.
Slots are then `budget / connection_window`, which is the exact aggregate bound.

A direct connection beyond the last slot is **refused, not failed**: that carrier stays on
the warm TCP relay, which F-8 measured as the faster transport on a clean path anyway. Each
refusal is logged and counted in `/admin/api/v1/metrics` as `direct_budget_refusals`. That
counter is history — how many carriers have been turned away since the server started — so
the same endpoint also publishes `udp_direct_slots_available`, the number of slots free
**right now** (`null` when no budget is configured). Read the two together: refusals rising
with the gauge at 0 means the budget is the bound; refusals rising with slots free means
they were taken and released. The CONFIGURED total is a separate thing and lives on
`/admin/api/v1/config` as `udp_direct_slots`, where it never moves with load. A
budget too small to hold every carrier is still honoured, and says so at startup with the
figure that would hold them all — note that the figure it names raises the BUDGET, and on a
small host the practical remedy is usually the other side of the same inequality, a lower
`--max-carriers`. The flag conflicts with `--udp-stream-receive-window`,
`--udp-connection-receive-window` and `--udp-send-window`, because honouring both would mean
silently discarding one.

Unset, the behaviour is unchanged: a per-connection ceiling with no aggregate bound. On a
host whose memory cannot hold even one tunnel's worst case, the server warns at startup with
the computed figure and this flag.

**Direct-path throughput on unprivileged hosts.** The 16 MiB UDP socket buffers above are
requested with `SO_{SND,RCV}BUFFORCE`, which bypasses the kernel's `net.core.{r,w}mem_max`
ceiling — but that needs `CAP_NET_ADMIN`. `bore vpn` runs privileged and gets it; an ordinary
`bore local --udp` / `bore proxy --udp` does **not**, so on a host with the stock `*mem_max`
(208 KiB on Ubuntu/Debian/AWS) the buffers are clamped and a single direct flow is capped at
roughly `buffer / RTT` (≈10 MB/s at 20 ms RTT) regardless of the QUIC windows. bore logs a
`warn!` with the remediation when this happens; raise the ceiling with:

```shell
sudo sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216
```

(and the matching `net.core.*mem_default` for it to take effect on new sockets).

**`--udp` selects a *transport*, it does not forward UDP application traffic.** `bore
local`/`bore proxy` forward **TCP** services only; `--udp` chooses a direct **QUIC** data
path for the tunnel itself, with automatic relay fallback. It works for:

- **secret tunnels** (`bore proxy`, `bore local --tcp-secret-id`): peer-to-peer hole-punched
  QUIC between consumer and provider;
- **public tunnels** (`bore local`, no `--tcp-secret-id`): a server→client QUIC path (the
  server is public, so no hole-punch is needed — same model as `bore vhost --udp`).
  `--carriers N` opens N independent QUIC connections (each its own BBR congestion
  controller), exactly like vhost. Requires `bore server --udp`; otherwise the tunnel
  transparently stays on the TCP relay.

The other direct-path flags (`--upnp`, `--stun-server`, `--try-port-prediction`,
`--nat-udp-*`) are hole-punch helpers and apply to **secret tunnels only**; on a public
tunnel they are inert and bore `warn!`s instead of silently ignoring them. To tunnel a **UDP
application** (DNS, game servers, WireGuard, …) use `bore vpn` (L3 overlay), not `bore
local`/`proxy`.

### Automatic reconnection

Both `bore local` and `bore proxy` accept `--auto-reconnect`. When the connection fails to
establish or drops, the client reconnects on its own with a capped exponential backoff of 1,
2, 4, 8, 16, 32 seconds, then every 32 seconds indefinitely; a successful connection resets
the backoff.

### HTTPS on the tunnel port

By default a tunnel port forwards raw TCP. With `--https`, the server terminates TLS on the
tunnel port using its certificate, so the exposed service is reachable over `https://` —
while plain `http://` and raw TCP keep working on the same port:

```shell
# Server has a certificate (see "Serving over HTTPS/HTTP" below).
bore local 8080 --to https://bore.tld -p 9000 -s mysecret --https
# -> https://bore.tld:9000   (TLS, terminated at the server)
# -> http://bore.tld:9000    (plain)
# -> bore.tld:9000           (raw TCP)
```

Add `--https=redirect` (alias `--force-https`) to redirect plain HTTP requests to `https://`
(raw TCP and `https://` keep working):

```shell
bore local 8080 --to https://bore.tld -p 9000 -s mysecret --https=redirect
# -> https://bore.tld:9000   (TLS)
# -> http://bore.tld:9000    (308 redirect to https://bore.tld:9000)
# -> bore.tld:9000           (raw TCP)
```

### WebSocket support

`bore` forwards standard WebSocket connections transparently:

- **Public tunnels** (`bore local`) support `ws://` and, with `--https`, `wss://`.
- **Secret tunnels** (`bore local --tcp-secret-id` + `bore proxy`) support WebSocket traffic
  on both the relay path and the direct UDP path.
- **Vhost** (`bore vhost`) supports standard HTTP/1.1 WebSocket upgrade on both the TCP relay
  and `bore vhost --udp`.

This works because bore only inspects the first bytes needed for routing / TLS / optional
HTTP handling, then switches to a full-duplex byte-stream splice. After the HTTP `101
Switching Protocols` response, WebSocket frames are forwarded unchanged.

Important caveats:

- The supported vhost/browser path is the classic **HTTP/1.1 `Upgrade: websocket`** flow.
  WebSocket over HTTP/2 extended CONNECT is not implemented.
- For `bore vhost --udp`, only the **server->provider** hop uses QUIC; the browser still
  talks HTTP/TLS to the server.
- If a live direct UDP/QUIC path drops, an already-open WebSocket on that path drops too;
  fallback applies to new connections, not migration of an in-flight stream.

End-to-end tests cover public tunnels, secret tunnels, and vhost WebSocket flows.

## Self-hosting

As mentioned in the startup instructions, the CLI defaults to the public server
`https://bore.0912345.xyz`. To self-host `bore` on your own network:

```shell
bore server
```

That's all it takes! After the server starts running at a given address, update the `bore
local` command with `--to <ADDRESS>` to forward a local port to this remote server.

It's possible to specify different IP addresses for the control server and for the tunnels.
This setup is useful for cases where you might want the control server to be on a private
network while allowing tunnel connections over a public interface, or vice versa.

The control port defaults to `7835` but is configurable with `--control-port`; clients then
connect with `--to host:port`.

### Full server flag reference

```shell
bore server \
  --bind-domain bore.tld --control-port 443 \
  --cert-file /etc/bore/cert.pem --key-file /etc/bore/key.pem \
  --secret "$BORE_SECRET" --udp --admin-token "$(openssl rand -hex 24)" \
  --min-port 20000 --max-port 20100
```

```text
Runs the remote proxy server

Usage: bore server [OPTIONS]

Core:
      --min-port <PORT>          Minimum accepted TCP port number [env: BORE_MIN_PORT=] [default: 1024]
      --max-port <PORT>          Maximum accepted TCP port number [env: BORE_MAX_PORT=] [default: 65535]
  -s, --secret <SECRET>          Optional secret for client authentication [env: BORE_SECRET=]
      --max-conns <N>            Max concurrently proxied connections per client [env: BORE_MAX_CONNS=] [default: 1024]
      --max-carriers <N>         Max parallel TCP carriers a public/provider/vhost tunnel may use (1 disables the pool). Does not cap bore proxy's own carriers. [env: BORE_MAX_CARRIERS=] [default: 16]

> **`--max-conns` and the file-descriptor limit.** Each admitted connection
> costs one descriptor, so the bound can only refuse *gracefully* (one
> connection dropped, `conn_rejections` incremented, everything else still
> served) while `RLIMIT_NOFILE` covers it. Above the descriptor limit the
> kernel refuses first, with `EMFILE`, and `EMFILE` lands on `accept()` for
> **every** listener — so one tunnel's concurrency takes the admin API and
> every other tunnel down with it. At startup `bore server` therefore raises
> its own soft limit to `--max-conns + 256` when the hard limit allows
> (`raised the file-descriptor limit …` at `INFO`), and when the hard limit is
> itself too low it raises to the ceiling and **warns** with the remedy. If you
> see that warning, raise the limit for the process — Docker Compose
> `ulimits: { nofile: { soft: 65536, hard: 65536 } }`, systemd
> `LimitNOFILE=65536` — or lower `--max-conns`. Measured: a container with
> `--max-conns 1024` and a soft limit of 1024 stopped answering on its control
> port for ~30 s under ~976 held connections through one public tunnel, with
> `conn_rejections` still at 0.
      --control-port <PORT>      TCP port the control connection listens on [env: BORE_CONTROL_PORT=] [default: 7835]
      --bind-domain <DOMAIN>     Public domain advertised to clients (informational) [env: BORE_BIND_DOMAIN=]
      --cert-file <PATH>        TLS certificate chain (PEM); with --key-file, serves HTTPS [env: BORE_CERT_FILE=]
      --key-file <PATH>          TLS private key (PEM); with --cert-file, serves HTTPS [env: BORE_KEY_FILE=]
      --bind-addr <IP>           IP address to bind to, clients must reach this [default: 0.0.0.0]
      --bind-tunnels <IP>        IP address where tunnels listen, defaults to --bind-addr
      --control-hsts <VALUE|off> HSTS value on HTTPS control-port HTTP responses (admin page, vhost-miss 404); "off" disables [env: BORE_CONTROL_HSTS=] [default: "max-age=31536000; includeSubDomains"]
      --admin-token <TOKEN>      Enable the admin status page at /admin/status (min 32 chars) [env: BORE_ADMIN_TOKEN=]
  -v, --verbose...               Increase log verbosity (-v debug, -vv trace; RUST_LOG overrides)
  -h, --help                     Print help

Direct UDP path (--features udp, on by default):
      --udp                                      Broker UDP direct (hole-punched) paths for secret tunnels + run a STUN responder on the control port [env: BORE_UDP=]
      --no-udp-adaptive-plan                     Kill switch: never compute server-side adaptive traversal plans (clients keep their default check rounds) [env: BORE_NO_UDP_ADAPTIVE_PLAN=]
      --udp-stream-receive-window <SIZE>         QUIC receive window per direct-UDP stream [env: BORE_UDP_STREAM_RECEIVE_WINDOW=] [default: 16MiB]
      --udp-connection-receive-window <SIZE>      Aggregate QUIC receive window per direct-UDP connection [env: BORE_UDP_CONNECTION_RECEIVE_WINDOW=] [default: 256MiB]
      --udp-send-window <SIZE>                    QUIC send window for the direct UDP path [env: BORE_UDP_SEND_WINDOW=] [default: 256MiB]
      --udp-socket-recv-buffer <SIZE>             UDP socket receive buffer requested for direct UDP [env: BORE_UDP_SOCKET_RECV_BUFFER=] [default: 16MiB]
      --udp-socket-send-buffer <SIZE>             UDP socket send buffer requested for direct UDP [env: BORE_UDP_SOCKET_SEND_BUFFER=] [default: 16MiB]
      --udp-max-streams <N>                        Max native QUIC bidi streams per direct UDP connection [env: BORE_UDP_MAX_STREAMS=] [default: 4096]
      --udp-memory-budget <SIZE>                  Worst-case direct-path receive memory for the WHOLE server; derives the windows and a server-wide admission limit. Conflicts with the three window flags [env: BORE_UDP_MEMORY_BUDGET=]
      --stun-alt-port <PORT>                      UDP port of the ALTERNATE STUN socket (RFC 5780 behaviour discovery). Lets clients measure NAT *filtering*, not just mapping. Unset/0 = ephemeral [env: BORE_STUN_ALT_PORT=]
        (SIZE accepts raw bytes or KB/MB/GB/KiB/MiB/GiB suffixes)

Vhost frontend (always available, no feature flag):
      --vhost-config <PATH>       Path to vhost.yml (optional; needed only for reservations/default headers) [env: BORE_VHOST_CONFIG=]
      --vhost-base-domain <DOMAIN> Base domain, e.g. bore.mydomain.com; enables vhost without a config file, overrides base_domain from the file [env: BORE_VHOST_BASE_DOMAIN=]
      --vhost-http-port <PORT>    Override http_port from vhost.yml (default 80) [env: BORE_VHOST_HTTP_PORT=]
      --vhost-https-port <PORT>   Override https_port from vhost.yml (default 443) [env: BORE_VHOST_HTTPS_PORT=]
      --vhost-quic-port <PORT>    Shared UDP port for vhost/public/SSH-jump direct QUIC (default: resolved vhost HTTPS port, on UDP) [env: BORE_VHOST_QUIC_PORT=]
      --vhost-mode <MODE>         Override mode from vhost.yml: http|https|both|redirect-https|auto [env: BORE_VHOST_MODE=]
      --vhost-cert-file <PATH>    TLS cert (PEM) for the vhost HTTPS frontend, overrides vhost.yml's cert_file [env: BORE_VHOST_CERT_FILE=]
      --vhost-key-file <PATH>     TLS key (PEM) for the vhost HTTPS frontend, overrides vhost.yml's key_file [env: BORE_VHOST_KEY_FILE=]

VPN broker (--features vpn):
      --vpn                       Enable VPN link brokering (client must be built with --features vpn) [env: BORE_VPN=]
      --vpn-pool <CIDR>           Overlay address pool for VPN links, e.g. 10.99.0.0/16 (required for pool-mode clients) [env: BORE_VPN_POOL=]
      --vpn-max-links <N>         Maximum concurrent VPN links [env: BORE_VPN_MAX_LINKS=] [default: 32]
      --vpn-hub-prefix <P>        Overlay subnet prefix /P allocated per hub from --vpn-pool [env: BORE_VPN_HUB_PREFIX=] [default: 24]

SSH ingress gateway (--features ssh-gateway; see "SSH ingress gateway" below):
      --ssh-gateway                          Enable the embedded SSH ingress gateway. Requires --ssh-authorized-keys-dir and/or --ssh-passwords-file [env: BORE_SSH_GATEWAY=]
      --ssh-jump-base-domain <DOMAIN>        Exact namespace for ProxyJump targets, e.g. ssh.bore.tld; requires --ssh-gateway [env: BORE_SSH_JUMP_BASE_DOMAIN=]
      --ssh-port <PORT>                      Bind a dedicated extra TCP listener for SSH (default: demuxed on the control port, no extra port to open) [env: BORE_SSH_PORT=]
      --ssh-host-key-file <PATH>             ed25519 host key (OpenSSH PEM); generated on first use if absent [env: BORE_SSH_HOST_KEY_FILE=] [default: bore_ssh_host_key.pem]
      --ssh-authorized-keys-dir <DIR>         Directory of authorized_keys-format files granting public-key auth (hot-reloaded every attempt) [env: BORE_SSH_AUTHORIZED_KEYS_DIR=]
      --ssh-passwords-file <PATH>             Argon2id password file granting password auth (generate lines with `bore hash-password`) [env: BORE_SSH_PASSWORDS_FILE=]
      --ssh-banner <TEXT>                     Banner text shown before authentication [env: BORE_SSH_BANNER=]
      --ssh-advertise-address <HOST>          Externally-reachable hostname, printed in informational banners (e.g. the secret-provider consumer command); placeholder if unset [env: BORE_SSH_ADVERTISE_ADDRESS=]
      --ssh-advertise-port <PORT>             Externally-reachable port, printed in the same banners; placeholder if unset [env: BORE_SSH_ADVERTISE_PORT=]
      --ssh-window-size <BYTES>               Per-channel SSH flow-control window; raise on high-latency links (costs memory per connection) [env: BORE_SSH_WINDOW_SIZE=] [default: 16777216 (16 MiB)]

Access logging (always available):
      --webserver-log <DIR>                   Write access logs in nginx-combined format to <DIR> (off by default) [env: BORE_WEBSERVER_LOG=]
      --webserver-log-max-files <N>            Max rotated log files retained per target [env: BORE_WEBSERVER_LOG_MAX_FILES=] [default: 4]
      --webserver-log-max-file-size <MB>       Max MiB per log file before rotation [env: BORE_WEBSERVER_LOG_MAX_FILE_SIZE=] [default: 100]
```

### Serving over HTTPS/HTTP

Pass a certificate and key to serve the control connection over TLS; clients connect with
`https://`:

```shell
# HTTPS (clients: --to https://bore.tld)
bore server --bind-domain bore.tld --cert-file /var/bore/cert.pem --key-file /var/bore/key.pem

# Plain HTTP addressing, no TLS (clients: --to http://bore.tld)
bore server --bind-domain bore.tld
```

A self-signed certificate requires `--insecure` on the client. `--cert-file` and `--key-file`
must be given **together**; the same certificate is reused for the optional TLS termination
on tunnel ports (`--https`) and for the vhost HTTPS frontend unless
`--vhost-cert-file`/`--vhost-key-file` override it.

### Basic auth on tunnels

Any tunnel — public or secret — can be protected with HTTP Basic auth via `--basic-auth
"user:pass"` on `bore local`. HTTP requests without valid credentials get a `401`; non-HTTP
traffic is forwarded unprotected (Basic auth is HTTP-only). For a **public** tunnel the
server enforces it; for a **secret** tunnel the provider enforces it (covering both the relay
and the direct UDP path), so the credentials never leave the provider. Use it over TLS so the
credentials are not sent in the clear.

```shell
bore local 8080 --to https://bore.tld -p 9000 --https --basic-auth "admin:s3cr3t"
```

### Admin status page

Start the server with `--admin-token <TOKEN>` (at least 32 characters) to enable a read-only
status dashboard at **`/admin/status`** on the control port. It is served over the same
scheme as the control connection — `http://host:7835/admin/status`,
`https://bore.tld/admin/status`, etc. Without `--admin-token` the page is disabled and the
control port speaks only the bore protocol.

```shell
bore server --secret mysecret --admin-token "$(openssl rand -hex 24)"
# open http://your-server:7835/admin/status and paste the token
```

The page lists connected public tunnels, SSH jump hosts, VPN links, vhost providers and,
for secret tunnels, both the provider and all attached `bore proxy` consumers
(SSH-gateway originated tunnels included) — with their client address, options, `--notes`, live
connection count, and uptime. It refreshes automatically (polling every ~2s) and keeps **no**
persistent state: it reflects exactly what is connected right now. The frontend is embedded
in the binary; no external assets are fetched.

SSH jump hosts have a dedicated **Jump Hosts** panel and token-guarded
`/admin/api/v1/ssh-jump` endpoint. It reports operational metadata and counters only;
classic usernames, credential material, the bore secret and private keys are never exposed.

The Configuration panel — and `GET /admin/api/v1/config` — reports `server_version`, the
running binary's own build string (`<semver> - <branch> - <sha8>`, exactly what
`bore --version` prints). Use it to confirm a redeploy actually landed, and to name the
build in a bug report or a benchmark result, without shell access to the host.

Annotate any tunnel with `--notes "..."` (on `bore local`/`bore proxy`/`bore vhost`, or
`notes=` via the SSH gateway) to label it on this page.

## SSH jump hosts

> **Step-by-step operator guide (Italian):** [`docs/README-JUMP-HOST.md`](docs/README-JUMP-HOST.md)
> — server setup, binary startup, Docker wrapper startup and operator access, end to end.

`sshjhost` publishes an SSH daemon under an exact, separate hostname namespace. The
operator uses an unmodified OpenSSH client; the inner SSH session remains end-to-end between
the operator and the target `sshd`:

```text
operator OpenSSH -> bore SSH gateway:443 -> QUIC direct or warm TCP -> target sshd
```

For example, an alias `vm-test-01` under `ssh.bore.tld` is reached with:

```shell
ssh -J fabio@bore.tld:443 ubuntu@vm-test-01.ssh.bore.tld
```

No DNS record is required for every alias: only `bore.tld` must resolve. OpenSSH sends the
target hostname in its `direct-tcpip` request to the gateway. The target port is exact: for
an SSH daemon on 2222 use `ssh -p 2222 -J fabio@bore.tld:443 ...`.

### Server and Docker Compose on port 443

The server requires the existing SSH gateway plus one new namespace setting:

```shell
# UDP is a default feature; ssh-gateway is opt-in for source builds.
cargo build --release --features ssh-gateway
# Repository/release acceptance build:
cargo build --release --all-features
```

```shell
bore server \
  --control-port 7835 \
  --udp \
  --vhost-quic-port 443 \
  --ssh-gateway \
  --ssh-advertise-address bore.tld \
  --ssh-advertise-port 443 \
  --ssh-jump-base-domain ssh.bore.tld \
  --ssh-host-key-file /etc/bore/ssh/host_key.pem \
  --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d \
  --ssh-passwords-file /etc/bore/ssh/passwords
```

For the repository's bridge-network Compose, keep all existing mappings and add only the
environment variable shown below:

```yaml
ports:
  - "443:7835"       # existing TCP control/TLS/SSH demux
  - "7835:7835/udp"  # existing STUN socket
  - "443:443/udp"    # existing shared direct-QUIC socket
environment:
  - BORE_CONTROL_PORT=7835
  - BORE_UDP=true
  - BORE_VHOST_QUIC_PORT=443
  - BORE_SSH_GATEWAY=true
  - BORE_SSH_ADVERTISE_ADDRESS=bore.tld
  - BORE_SSH_ADVERTISE_PORT=443
  - BORE_SSH_JUMP_BASE_DOMAIN=ssh.bore.tld  # new
```

Do not add port 8443. TCP 443 and UDP 443 are distinct sockets; in this Compose they also
map to different container ports. Native `sshjhost --udp`, public `local --udp` and
`vhost --udp` authenticate into separate `jump:<alias>`, `port:<N>` and bare-vhost pools
through this single shared UDP endpoint. STUN stays on container UDP 7835.
For a bare binary with `--control-port 443`, do not also set
`--vhost-quic-port 443`: STUN already owns UDP 443. Use distinct internal UDP ports; the
no-8443 rule above is specific to this bridge-network Compose topology.

Server option:

| Flag | Env | Description |
|---|---|---|
| `--ssh-jump-base-domain <DOMAIN>` | `BORE_SSH_JUMP_BASE_DOMAIN` | Enables exact `<alias>.<domain>` dispatch. Requires `--ssh-gateway` and a server built with `ssh-gateway`. |

### Native `bore sshjhost` provider

On the VM that owns the SSH daemon:

```shell
bore sshjhost localhost:22 \
  --subdomain vm-test-01 \
  --to https://bore.tld \
  --secret "$BORE_SECRET" \
  --notes "vm test AWS su zona eu-south-1" \
  --carriers 4 \
  --udp \
  --auto-reconnect
```

For a nonstandard target, the local and virtual ports are the same in v1:

```shell
bore sshjhost localhost:2222 --subdomain legacy-01 \
  --to https://bore.tld --secret "$BORE_SECRET" --auto-reconnect
ssh -p 2222 -J fabio@bore.tld:443 admin@legacy-01.ssh.bore.tld
```

The native provider authenticates to the bore control plane with the existing
`BORE_SECRET`; it does not use a gateway username. With `--udp`, the provider dials the
shared server QUIC endpoint: no STUN or hole-punch is needed. TCP carriers stay warm.
Each SSH connection uses one QUIC bidi stream; on missing/failed QUIC the same connection
immediately falls back to TCP. `--carriers N` requests N TCP carriers and N independent
QUIC connections (both server-capped); carrier loss renews only the QUIC shortfall.

Complete client reference:

| Argument/flag | Env | Meaning |
|---|---|---|
| `<TARGET>` | — | Required `HOST:PORT` or `[IPv6]:PORT`; the port is also the virtual target port. |
| `--subdomain <LABEL>` | `BORE_SSH_JUMP_SUBDOMAIN` | Required lowercase single DNS label. |
| `--to <ADDR>` | `BORE_SERVER` | Bore control endpoint; use `https://bore.tld` for external TCP 443. |
| `--secret <SECRET>` | `BORE_SECRET` | Existing bore HMAC secret. |
| `--insecure` | `BORE_INSECURE` | Accept a self-signed control TLS certificate. |
| `--notes <TEXT>` | `BORE_NOTES` | Bounded operational note. |
| `--carriers <N>` | `BORE_CARRIERS` | Requested warm TCP carriers and, with `--udp`, independent QUIC connections (default 1; both capped). |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | Re-register with exponential backoff after a disconnect. |
| `--udp` | `BORE_PREFER_UDP` | Prefer server-direct QUIC; requires `bore server --udp`, uses `--vhost-quic-port`, always keeps TCP fallback warm. |

### Pure OpenSSH provider

A VM without the bore binary can publish the same kind of target through a reverse SSH
forward. The `jump/` prefix is mandatory:

```shell
ssh -T -p 443 -i ~/.ssh/id_ed25519_bore_provider \
  -o IdentitiesOnly=yes -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
  -R 'jump/vm-test-01:22:localhost:22' \
  vm-provider@bore.tld -- 'notes="AWS eu-south-1"'
```

Password operation is interactive and keeps the password out of arguments and logs:

```shell
ssh -T -p 443 \
  -o PubkeyAuthentication=no -o PreferredAuthentications=password \
  -o ExitOnForwardFailure=yes \
  -R 'jump/vm-test-01:22:localhost:22' \
  vm-provider@bore.tld -- 'notes="interactive provider"'
```

This provider path is TCP-only and uses the gateway's classic public-key/password auth.
`udp=` and `carriers=` are reported as inapplicable. A reconnect using the same exact
username may replace its previous registration; another username, a native provider, or a
second native provider cannot take the alias. The old syntax remains unchanged:
`ssh -R 22:localhost:22 bore.tld` requests the existing public port 22 and does not register
a jump host.

### Limits, lifecycle and audit

Every jump alias has its own `--max-conns` semaphore. Native providers also use the
server-capped `--carriers` pool; opening a provider channel is bounded to 15 seconds and
retries another live carrier when one dies between selection and open. Permits, activity
counters and the single logical admin row are RAII-owned, so cancellation, timeout and
provider failure cannot leak capacity or duplicate dashboard rows.

For UDP-requesting native providers the gateway picks one QUIC connection round-robin per
SSH channel, writes `STREAM_READY`, then performs the same single-task half-close-safe
splice. A failed pick/open/readiness marker increments the fallback counter and opens the
same channel on warm TCP. Admin exposes live direct carriers, direct opens/fallbacks and
separate relay/direct byte totals. Pure-OpenSSH providers never advertise QUIC.

Native aliases are first-wins. Pure-OpenSSH reconnects may replace only a registration
owned by the same exact classic username; cross-user and cross-transport collisions are
rejected. Native providers heartbeat every 20 seconds and the server reaps an abandoned
control connection after 60 seconds, checked on the 500 ms control tick.

The server emits structured `allow`, `deny`, `open` and `close` events with the outer peer,
operator principal, alias, requested port, provider type/owner class and selected path.
Credentials are never logged; client-visible denials remain generic, detailed reasons stay
server-side, and repeated username-mismatch diagnostics are sampled to bounded logarithmic
volume.

### Classic authentication and key placement

Classic username binding applies only to the new jump publish/connect operations. Existing
SSH-gateway public, vhost and secret forwarding keeps its prior username-ignored behavior.
There is no jump ACL file: any correctly authenticated jump account may reach any registered
alias.

For public keys, put the public key in a file whose basename is the exact, case-sensitive
gateway username (`fabio` or `fabio.pub`):

```text
/etc/bore/ssh/authorized_keys.d/fabio
```

The matching private key stays on the operator/provider host. For passwords, store only an
Argon2id line with the exact username label:

```shell
printf '%s' 'outer-password' | bore hash-password
# add: fabio:$argon2id$... to /etc/bore/ssh/passwords
```

Credential layers are independent:

| Material | Location | Purpose |
|---|---|---|
| Gateway host private key | Persistent bore server volume | Identifies `bore.tld` to OpenSSH. |
| Gateway account public key/hash | Bore server key directory/password file | Authenticates `fabio` or `vm-provider` to the outer gateway. |
| Gateway account private key/password | Operator or pure-provider VM | Opens the outer SSH connection; never copied to the bore server as plaintext. |
| Target host key and `authorized_keys` | Target VM's normal `sshd` paths | Normal inner SSH server identity and account authorization. |
| Target private key/password | Operator | Authenticates directly to target `sshd`; no agent forwarding is required. |

Recommended operator configuration, which also makes `ssh -J bore.tld ...` select port 443:

```sshconfig
Host bore.tld
    Port 443
    User fabio
    IdentityFile ~/.ssh/id_ed25519_bore_gateway
    IdentitiesOnly yes
    StrictHostKeyChecking yes
    ForwardAgent no

Host *.ssh.bore.tld
    IdentityFile ~/.ssh/id_ed25519_target
    IdentitiesOnly yes
    ForwardAgent no
```

Only `bore.tld` needs DNS. Alias names are carried inside the gateway's
`direct-tcpip` request, so wildcard DNS for `*.ssh.bore.tld` is optional and is
not consulted by this ProxyJump flow. Pin the gateway as `[bore.tld]:443`; each
target is pinned independently under its alias (and `[alias]:port` for a
nonstandard port). Reinstalling or rotating a target host key therefore changes
only that target entry. Verify the new fingerprint out of band before removing
the exact stale entry with `ssh-keygen -R <alias>` or
`ssh-keygen -R '[<alias>]:<port>'`. Never delete the whole `known_hosts` file.

Agent forwarding is unnecessary and discouraged: the local OpenSSH process
authenticates separately to the gateway and target. `ForwardAgent no` prevents
the target or an intermediate process from using the operator's agent.

### Native provider as a systemd service

Keep ordinary settings in an environment file and the shared secret in a
root-readable systemd credential. The secret never appears in `ExecStart` or
the process command line:

```ini
# /etc/bore/sshjhost.env (0640 root:bore)
BORE_SERVER=https://bore.tld
BORE_SSH_JUMP_SUBDOMAIN=vm-test-01
BORE_NOTES="vm-test-01 production"
BORE_CARRIERS=4
BORE_PREFER_UDP=true
BORE_AUTO_RECONNECT=true
```

```sh
#!/bin/sh
# /usr/local/libexec/bore-sshjhost (0755 root:root)
set -eu
export BORE_SECRET="$(cat "${CREDENTIALS_DIRECTORY}/bore-secret")"
exec /usr/local/bin/bore sshjhost 127.0.0.1:22
```

```ini
# /etc/systemd/system/bore-sshjhost.service
[Unit]
Description=Bore SSH jump-host provider
After=network-online.target
Wants=network-online.target

[Service]
User=bore
EnvironmentFile=/etc/bore/sshjhost.env
LoadCredential=bore-secret:/etc/bore/credentials/sshjhost.secret
ExecStart=/usr/local/libexec/bore-sshjhost
Restart=always
RestartSec=5s
NoNewPrivileges=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
```

Install `/etc/bore/credentials/sshjhost.secret` as `0600 root:root`, then use
`systemctl enable --now bore-sshjhost`. Do not put `--secret` in the unit or a
shell command. On older systemd without `LoadCredential=`, use a `0600`
`EnvironmentFile` containing `BORE_SECRET` and restrict access to the service
account.

### Trust boundary

The native provider uses the server-wide bore shared secret: any holder can
claim any currently free native alias. Any correctly username-bound classic
gateway account can connect to every registered jump alias; v1 has no per-alias
ACL. First-wins and cross-transport collision checks prevent accidental
takeover, but they do not create tenant isolation. Use separate bore servers or
separate trust domains when providers must not be able to squat each other's
names.

With password authentication, OpenSSH prompts separately for the outer gateway and inner
target credentials. The complete deployment/test matrix is also preserved in
[`examples_usage.md`](docs/plans/plan_SshJumpHost/examples_usage.md).

## SSH ingress gateway

`bore server --ssh-gateway` embeds an SSH server (via `russh`) directly in the relay server,
so a **stock OpenSSH client** (`ssh -R`/`-L`, or `autossh`) can create public, vhost, secret,
and `jump/<alias>` provider tunnels — **no `bore` binary on the client side at all**. Requires building with
`--features ssh-gateway` (included in `--all-features` and in every GitHub Release binary of
this fork).

From the accepted SSH channel inward, the gateway reuses the exact same registries, relay,
admin page, and access-logging data path as a native `bore` client — a native client and an
SSH client can coexist on the same tunnel namespace, and even relay to each other (e.g. a
secret provider that is a native `bore local --tcp-secret-id` and a consumer that is `ssh -L
...secret/<id>:1`).

**What is not available over the SSH leg:** `--udp`/QUIC direct path, `--carriers > 1`,
hole-punch flags (`--stun-server`/`--upnp`/`--try-port-prediction`/`--nat-udp-*`), and `bore
transfer` — these are native-`bore`-client-only features (SSH is TCP-only by protocol). A
client that names one of these via the exec string or `SetEnv` gets an explicit warning, never
a silent no-op. `--secret`/`--insecure` are replaced by SSH's own auth (keys/password) and
host-key pinning.

**Gains over the native client:** zero-install (any OS with OpenSSH ≥ 7.8, including
Windows/macOS/routers), per-identity auth with keys (vs. one shared `--secret`), per-key
restrictions (`permit=`), N tunnels over a single SSH session, `ssh -C` compression.

### Server setup

```shell
bore server \
  --control-port 7835 \
  --admin-token "$(openssl rand -hex 24)" \
  --vhost-base-domain bore.example.com --vhost-http-port 7835 \
  --ssh-gateway \
  --ssh-jump-base-domain ssh.bore.example.com \
  --ssh-host-key-file /etc/bore/ssh/host_key.pem \
  --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d \
  --ssh-passwords-file /etc/bore/ssh/passwords \
  --ssh-banner "Authorized use only"
```

Server flags (all `#[cfg(feature = "ssh-gateway")]`; also listed in the
[full server flag reference](#full-server-flag-reference)):

| Flag | Env | Required? | Description |
|---|---|---|---|
| `--ssh-gateway` | `BORE_SSH_GATEWAY` | yes (to activate) | Enables the gateway. Requires **at least one** of `--ssh-authorized-keys-dir`/`--ssh-passwords-file` (fails fast otherwise). |
| `--ssh-jump-base-domain <DOMAIN>` | `BORE_SSH_JUMP_BASE_DOMAIN` | no | Enables exact `<alias>.<domain>` ProxyJump dispatch. Requires `--ssh-gateway`. |
| `--ssh-port <PORT>` | `BORE_SSH_PORT` | no | Extra dedicated SSH port. Without it, SSH is demuxed on the **same** control/vhost port (443/7835) — no extra port to open. |
| `--ssh-host-key-file <PATH>` | `BORE_SSH_HOST_KEY_FILE` | no (default `bore_ssh_host_key.pem`) | ed25519 host key, generated on first run if absent. **Persist it on a volume** — otherwise the fingerprint changes on every restart and breaks every `autossh` client pinned with `StrictHostKeyChecking`. |
| `--ssh-authorized-keys-dir <DIR>` | `BORE_SSH_AUTHORIZED_KEYS_DIR` | one of two | Directory of `authorized_keys`-format files (one or more), re-read on **every** auth attempt — free hot-reload. |
| `--ssh-passwords-file <PATH>` | `BORE_SSH_PASSWORDS_FILE` | one of two | `label:$argon2id$...` lines, one credential per line. Generate with `bore hash-password`. |
| `--ssh-banner <TEXT>` | `BORE_SSH_BANNER` | no | Text shown before authentication. |
| `--ssh-advertise-address <HOST>` | `BORE_SSH_ADVERTISE_ADDRESS` | no | Public hostname of the gateway (e.g. behind a front proxy); used in informational banners (ready-to-copy consumer command). Without it: placeholder. |
| `--ssh-advertise-port <PORT>` | `BORE_SSH_ADVERTISE_PORT` | no | Public port of the gateway (e.g. `443` when a front proxy terminates 443 onto the control port). Without it: placeholder. |
| `--ssh-window-size <BYTES>` | `BORE_SSH_WINDOW_SIZE` | no (default 16 MiB) | Per-channel SSH flow-control window; raise for high-latency links. |

Without `--ssh-gateway`, behaviour is 100% unchanged bore-native.

### Provisioning credentials

**Public keys (recommended default):**

```shell
ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519_bore -C "laptop"
```

Copy the public key into a file inside `--ssh-authorized-keys-dir` (one file per
operator/team):

```
# /etc/bore/ssh/authorized_keys.d/laptop
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOM1XMK7LrZvfZ+evuz//FtdfjgeVCphVmy1d95Ze0Ov laptop
```

The trailing comment (`laptop`) becomes the **identity** (shown as owner in the admin
dashboard, and the key for same-identity takeover — see below). Per-key options go before the
key, space-separated:

For jump-only classic binding, the file basename must instead equal the login username
exactly (`fabio` or `fabio.pub`); the comment remains the legacy-mode identity.

```
permit="vhost/laptop-*,secret/ci-*,port/9000-9100",max-conns=50,notes="dev laptop" ssh-ed25519 AAAA... laptop
```

| Per-key option | Effect |
|---|---|
| `permit="pattern1,pattern2,..."` | Whitelist of names this key may request: `vhost/<glob>`, `secret/<glob>`, `port/<N>` or `port/<N1>-<N2>`. Without `permit=`, the key may request any free name. |
| `max-conns=<N>` | Cap on concurrent connections per tunnel — **always wins** over a `max-conns=` passed via exec/env by the client (admin policy beats client request). |
| `notes=<text>` | Fixed notes shown in the admin page — also wins over the client's exec/env value. |

**Passwords (alternative/additional):**

```shell
$ echo -n 'correct-horse-battery-staple' | bore hash-password
$argon2id$v=19$m=19456,t=2,p=1$aiFZvnWxcXKlASwnEMrDwQ$SG8n3dO+w9RDJv9poqlI+kGkLfLQVt5dsuxwURkvPno
add to the passwords file as: <label>:$argon2id$v=19$...
```

```
# /etc/bore/ssh/passwords
ci-runner:$argon2id$v=19$m=19456,t=2,p=1$aiFZvnWxcXKlASwnEMrDwQ$SG8n3dO+w9RDJv9poqlI+kGkLfLQVt5dsuxwURkvPno
fabio:$argon2id$...
```

Only Argon2id hashes live on disk, never plaintext. Multiple lines can be valid at once; the
`label` of the winning line becomes the identity. The SSH username (`user@host`) remains
free/ignored for existing public/vhost/secret modes. Jump publish/connect additionally
requires that username to equal the winning password label exactly.

```shell
sshpass -p 'correct-horse-battery-staple' ssh -p 7835 -R 9998:localhost:8080 alice@bore.example.com
```

Unattended `autossh` + password requires `sshpass`/`SSH_ASKPASS` (the password ends up in
env/a file) — **keys remain the recommended default** for automated tunnels.

### Naming heuristic (`-R <bind_address>:<port>`)

Heuristic, plus explicit prefixes that always disambiguate (any port):

| `bind_address:port` | Mode | Explicit prefix equivalent |
|---|---|---|
| `<N>` (numeric port, no label) | **public** — public port `N` (`0` = server-assigned) | — |
| `<label>:80` or `<label>:443` | **vhost** — subdomain `<label>.<base-domain>` | `vhost/<label>:<any port>` |
| `<label>:0` | **secret provider** — registers id `<label>` | `secret/<label>:0` |
| — | **SSH jump provider** — exact nonzero target port | `jump/<alias>:<port>` |
| `<label>:<other port>` | ambiguous → **rejected** | use `vhost/`/`secret/` |

Secret consumer (`-L`, always secret; the trailing port is an ignored placeholder — OpenSSH's
`-L` doesn't accept literal `:0`, so use `:1` or any nonzero):

```
-L <local_port>:<id>:1        # or secret/<id>:1
```

### Examples for every mode

> **⚠️ Never use `-N`, with or without parameters, for any mode below** (vhost, public,
> secret provider, secret consumer). `-N` (`SessionType=none`) stops OpenSSH from ever
> opening a session channel, so the gateway has **no channel** to write warnings or the
> tunnel-info banner to — the terminal just stays silent. Without `-N`, OpenSSH still opens a
> channel and requests an interactive shell by default; the gateway accepts it silently and
> uses it as a status channel (no prompt, no shell, never closes the connection over it).

Common stability options (client-side OpenSSH, all "free" on the gateway):

```shell
OPTS='-o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3
      -o ConnectTimeout=10 -o TCPKeepAlive=yes'
```

**VHOST** — `mysub.bore.example.com` → `localhost:8080`:

```shell
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com

# with parameters
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- \
    'notes="api prod" max-conns=512 basic-auth=user:pass webserver-log=on'

# autossh
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R vhost/mysub:0:localhost:8080 \
    bore.example.com -- 'notes="api prod"'
```

**PUBLIC** — public port 9005 → `localhost:8080`:

```shell
ssh $OPTS -p 443 -R 9005:localhost:8080 bore.example.com

# server-assigned port
ssh $OPTS -p 443 -R 0:localhost:8080 bore.example.com
# OpenSSH prints: "Allocated port NNNNN for remote forward to localhost:8080"

# with parameters
ssh $OPTS -p 443 -R 9005:localhost:8080 bore.example.com -- 'notes="staging" max-conns=200'

# autossh
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R 9005:localhost:8080 bore.example.com
```

**SECRET provider** — registers id `tcp-secret-id` → `localhost:8080`:

```shell
ssh $OPTS -p 443 -R secret/tcp-secret-id:0:localhost:8080 bore.example.com -- \
    'notes="db-primary" max-conns=64'

# autossh
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -R secret/tcp-secret-id:0:localhost:8080 \
    bore.example.com -- 'notes="db-primary"'
```

**SECRET consumer** — `localhost:8899` → provider `tcp-secret-id`:

```shell
ssh $OPTS -p 443 -L 8899:secret/tcp-secret-id:1 bore.example.com

# autossh
AUTOSSH_GATETIME=0 autossh -M0 $OPTS -p 443 -L 8899:secret/tcp-secret-id:1 bore.example.com
```

> An SSH consumer can talk to a **native bore** provider and vice versa — provider and
> consumer can be on different transports; the server-side relay is indifferent. The only
> feature lost is the peer-to-peer direct path (QUIC), which requires the native `bore`
> client on **both** sides.

### Tunnel-info banner

Once the forward is established, the gateway writes a status report to the session channel
(the same channel an interactive shell would have used — hence never using `-N`). Every line
reports only facts the server actually knows for certain: **never** the local `host:port` of
your `-R`/`-L` (that never travels over the SSH protocol; RFC4254's `tcpip-forward`/
`direct-tcpip` have no field for it). The real value can take a few seconds to appear
(admin registration + parameter resolution), not instant.

```text
Vhost tunnel established
  Public URL:       http://mysub.bore.example.com
  Mode:             HTTP only
  Identity:         laptop
  Notes:            (none)
  Basic-auth:       disabled
  Webserver-log:    disabled
  Max-conns:        n/a for vhost (server-wide --max-conns applies; no per-tunnel cap)
```

```text
Public tunnel established
  Public port:      9005
  Identity:         laptop
  Notes:            staging
  Max-conns:        200 (requested)
  Basic-auth:       disabled
  HTTPS:            disabled
  Force-HTTPS:      disabled
  Webserver-log:    disabled
```

**Secret provider** — note the ready-to-use consumer command. By default it uses explicit
placeholders (`<same-port>`/`<same-host>`) instead of a guessed value: the gateway can't be
certain of its own public hostname (a front proxy like Docker/nginx rewrites the port, and
SSH has no Host/SNI equivalent), and a wrong guess would be worse than an honest placeholder.
With `--ssh-advertise-address` + `--ssh-advertise-port` set, the operator declares the public
endpoint and the command comes out ready to copy (each flag independent — the one left unset
stays a placeholder):

```text
Secret provider tunnel established
  Secret ID:        tcp-secret-id
  Identity:         laptop
  Notes:            db-primary
  Max-conns:        n/a for secret provider (not enforced per-tunnel)
  Basic-auth:       n/a for secret provider (opaque TCP, no HTTP layer)

Consumer command (run on the other side):
  ssh -T -p 443 -L <local-port>:secret/tcp-secret-id:1 bore.example.com
```

**Secret consumer** — shown once per session, not once per proxied connection:

```text
Attached to secret 'tcp-secret-id'
  Secret ID:        tcp-secret-id
  Identity:         laptop
  Notes:            (none)
  Provider identity: laptop
```

`Provider identity` reads `(unknown — provider may be a native bore client)` when the
provider isn't an SSH session on this gateway — the consumer still works, it's purely a
diagnostic detail unavailable in that case.

### Passing parameters (3 channels, priority: **key** > **exec** > **env**)

**Exec string** (after `--`, works with `autossh` too):

```shell
ssh $OPTS -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- \
    'notes="two words" max-conns=512 basic-auth=user:pass webserver-log=on id=custom-id'
```

Space-separated `key=value` grammar, shell-style quoting for values with spaces
(`notes="two words"`). A token without `=` (e.g. `https:on` instead of `https=on`) produces
an explicit warning (`malformed parameter "https:on" (expected key=value); ignored`), never a
silent drop.

**Environment** (`~/.ssh/config`, static):

```
Host bore
  HostName bore.example.com
  Port 443
  SetEnv BORE_NOTES=api-prod BORE_MAX_CONNS=512 BORE_BASIC_AUTH=user:pass
```

Mapping: `BORE_<KEY>` → `<key>` (lowercase, `_`→`-`). No server-side `AcceptEnv` config
needed — the gateway reads them directly off the SSH `env` request; only client-side
`SendEnv`/`SetEnv` is required.

**Per-key options** (`authorized_keys`, wins over everything):

```
permit="vhost/mysub",max-conns=256,notes="ci runner" ssh-ed25519 AAAA... ci@runner
```

**Full parameter table:**

| Parameter | Channels | Applies to | Effect |
|---|---|---|---|
| `notes=<text>` | exec, env (`BORE_NOTES`), key | all | Free notes shown in the admin dashboard |
| `max-conns=<N>` | exec, env (`BORE_MAX_CONNS`), key | all | Cap on concurrent connections (gateway-side semaphore) |
| `basic-auth=<user:pass>` | exec, env (`BORE_BASIC_AUTH`) | vhost, public HTTP | Basic auth **enforced by the gateway** (server-side 401) — over SSH the gateway does it, not the provider as with the native client |
| `webserver-log=on` | exec, env (`BORE_WEBSERVER_LOG`) | vhost, public | Enables per-tunnel access logging (existing server-side weblog) |
| `id=<label>` | exec, env (`BORE_ID`) | all | Explicit override of the identity/id shown (default: key fingerprint/label) |
| `https=on` | exec, env (`BORE_HTTPS`) | **public** | Terminates TLS on the public tunnel port using the server's certificate (requires `--cert-file`/`--key-file`; without it, served as plain TCP with a warning). Reuses the same code path as the native client's `bore local --https` |
| `force-https=on` | exec, env (`BORE_FORCE_HTTPS`) | **public** | Redirects plain HTTP on the tunnel port to `https://`. Requires `https=on` on the same request — if absent, disabled with a warning instead of silently applied or ignored |
| `backend-tls=on` | exec, env (`BORE_BACKEND_TLS`) | **vhost** | Gateway originates TLS to a local HTTPS backend (self-signed cert accepted; verification skipped). Same as native `bore vhost --backend-tls`. On public/secret forwards it is inapplicable and warns |
| `backend-tls-sni=<name>` | exec, env (`BORE_BACKEND_TLS_SNI`) | **vhost** | SNI/hostname sent to the TLS backend (default `localhost`); only with `backend-tls=on` |

**Client-transport-only parameters — rejected with an explicit warning, never silence:**
`udp`, `carriers`, `stun-server`, `upnp`, `try-port-prediction`, `nat-udp-preferred-port`,
`auto-reconnect` (use `autossh`/systemd client-side instead — the correct equivalent). Any
other unrecognized key produces `<key>: unknown parameter`, never a silent no-op.

```shell
$ ssh -p 443 -R vhost/mysub:0:localhost:8080 bore.example.com -- 'udp=on carriers=4'
bore ssh-gateway: udp: not available via SSH ingress; use the native bore client
bore ssh-gateway: carriers: not available via SSH ingress; use the native bore client
```

**`https=on`/`force-https=on` — available for public tunnels; automatic for vhost.** Two
distinct cases, don't conflate them:

**VHOST**: HTTPS is governed **server-side** by `vhost.yml`/`--vhost-mode`
(`--vhost-mode http|https|both|redirect-https|auto`, `--vhost-cert-file`/`--vhost-key-file`).
An SSH-originated vhost tunnel automatically inherits the same HTTPS behavior as a native
tunnel on the same host — no per-tunnel parameter to pass. Passing `https=on`/
`force-https=on` (or `max-conns=`, equally inapplicable to vhost) anyway produces an explicit
warning on the channel instead of being silently ignored:

```text
bore ssh-gateway: https: not applicable to vhost tunnels; ignoring
bore ssh-gateway: force-https: not applicable to vhost tunnels; ignoring
```

Same for the **secret provider**: **no** parameter besides `notes=` has any effect there (no
HTTP layer on an opaque TCP relay) — `https=`/`force-https=`/`basic-auth=`/`webserver-log=`/
`max-conns=` each produce their own "not applicable to secret provider tunnels; ignoring"
warning, never a silent no-op.

**PUBLIC**: `https=on`/`force-https=on` **are** per-tunnel parameters over SSH (exactly like
the native client's `bore local --https`/`--force-https`), applied on the public port
assigned to that forward. Requires the server to have a certificate configured
(`--cert-file`/`--key-file` on the control port):

```shell
ssh $OPTS -p 443 -R 9443:localhost:8080 bore.example.com -- 'https=on'
# curl https://bore.example.com:9443/   → local service response, TLS terminated by the server

ssh $OPTS -p 443 -R 9444:localhost:8080 bore.example.com -- 'https=on force-https=on'
# curl http://bore.example.com:9444/    → 308 redirect to https://bore.example.com:9444/
```

Without a server certificate configured, `https=on` is served as plain TCP with an explicit
warning on the channel (`https: server has no TLS certificate configured; serving this tunnel
as plain TCP`) — never a silent failure or a rejected tunnel. `force-https=on` without
`https=on` is disabled with a warning instead of being applied or silently ignored.

### `~/.ssh/config` reference

```
Host bore
    HostName bore.example.com
    Port 443
    User tunnel
    IdentityFile ~/.ssh/id_ed25519_bore
    IdentitiesOnly yes
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ConnectTimeout 10
    ExitOnForwardFailure yes
    SessionType none
```

```shell
ssh -R vhost/myapp:0:localhost:8080 bore                       # vhost
ssh -R vhost/secureapp:0:localhost:3005 bore -- 'backend-tls=on' # vhost, HTTPS backend
ssh -R 9005:localhost:8080 bore                                 # public
ssh -R secret/tcp-id:0:localhost:8080 bore                       # secret provider
ssh -L 8899:secret/tcp-id:1 bore                                 # secret consumer
```

`ExitOnForwardFailure=yes` is **mandatory** with `autossh`: without it, a session whose
forward was rejected (name already taken) stays alive "empty" and autossh never restarts it.

### systemd (a robust alternative to autossh) — one template per mode

```ini
# /etc/systemd/system/bore-tunnel-vhost.service
[Unit]
Description=bore SSH tunnel (vhost myapp)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=AUTOSSH_GATETIME=0
Environment=AUTOSSH_POLL=30
ExecStart=/usr/bin/autossh -M 0 \
    -o "ServerAliveInterval=15" -o "ServerAliveCountMax=3" \
    -o "ExitOnForwardFailure=yes" -o "StrictHostKeyChecking=yes" \
    -i /etc/bore/client_key -p 443 \
    -R vhost/myapp:0:localhost:8080 tunnel@bore.example.com -- 'notes="prod"'
Restart=always
RestartSec=5
User=bore-client

[Install]
WantedBy=multi-user.target
```

Swap the `-R`/`-L` line in `ExecStart` for one of the mode examples above (public/secret
provider/secret consumer) — the rest of the template is identical. `AUTOSSH_GATETIME=0`
matters: without it, autossh won't restart if the **first** attempt fails within 30s
(default "gate" period).

### SSH-over-TLS (DPI/firewalls that only permit outbound TLS)

```shell
ssh -o ProxyCommand='openssl s_client -quiet -verify_quiet -alpn ssh -connect bore.example.com:443' \
    -R vhost/myapp:0:localhost:8080 dummy-host
```

`-alpn ssh` is recommended: the demux classifies the ClientHello's ALPN and routes to the SSH
gateway immediately (without `-alpn` it still works — the server detects `SSH-` after the TLS
handshake, ~2s slower). The ALPN also keeps the SSH session cleanly separated from browser
traffic (`h2`/`http/1.1`) on the same port 443. Drop `-verify_quiet` in production with a
CA-issued certificate (it also accepts self-signed, handy for testing only).

### Same-identity takeover (deterministic reconnection)

A **new** session with the **same** key/identity that already holds a name evicts the
previous one instead of being rejected — this makes `autossh`/network-restart reconnects
deterministic (no flapping while waiting for the 60s reaper to free the name):

```shell
$ ssh -i id_ed25519_bore -p 443 -R vhost/mysub:0:localhost:18080 bore.example.com
Allocated port 1 for remote forward to localhost:18080
```

A **different** identity on the same name ⇒ rejected (`subdomain '<label>' already in use`).
A name protected by `permit=` not in the whitelist ⇒ `remote port forwarding failed for
listen port 0`.

### Fingerprint pinning (production)

```shell
ssh-keygen -l -E sha256 -f /etc/bore/ssh/host_key.pem
# 256 SHA256:3a5zdjovpFe3Y/XtIiDSgigHLPvbB3OekBd1g7QdLJw (ED25519)
```

```shell
ssh-keyscan -p 443 bore.example.com >> ~/.ssh/known_hosts   # once, over a trusted channel
```

Every connection then verifies against the fixed `known_hosts` line instead of default TOFU —
use `StrictHostKeyChecking=yes` in production.

### Harmless client messages

**`PTY allocation request failed on channel 0`** — printed by the OpenSSH client when it
auto-requests an interactive PTY (any `ssh`/`autossh` without `-T`). The gateway is not a
shell, so it refuses the PTY. **The `-R`/`-L` forward runs on a separate channel and is
unaffected** — the tunnel works normally. Pass `-T` to silence it:

```shell
ssh -T -p 443 -R vhost/app:0:localhost:5000 bore.example.com
autossh -M0 -T -p 443 -R vhost/app:0:localhost:5000 bore.example.com
```

**Never use `-N`** (ever): it skips the session channel entirely, so you get neither the
tunnel-info banner nor inapplicable-parameter warnings — the terminal stays silent. Use `-T`
instead.

**`Allocated port 1 for remote forward to ...`** — OpenSSH's RFC4254 placeholder for a
vhost/secret-provider forward (`-R <label>:0:...`). Since vhost/secret don't have a real TCP
port, the server answers with a placeholder (usually `1`). Purely cosmetic, ignore it.

### Troubleshooting the SSH gateway

| Symptom | Cause | Fix |
|---|---|---|
| `remote port forwarding failed for listen port 0` | `permit=` doesn't cover the label, or the name is held by a different identity | Check `permit=`; pick another name, or use the holder's key for a legitimate takeover |
| `subdomain '<label>' already in use` / `tcp-secret-id '<id>' already in use` | Name registered by a different identity | Different name, or same key/label for a legitimate takeover |
| `Permission denied (publickey,hostbased,keyboard-interactive)` | Key not in the directory, or wrong password/hash format | Verify the pubkey is in the file; regenerate the hash with `bore hash-password` |
| `<flag>: not available via SSH ingress; use the native bore client` | Client-transport-only parameter passed via exec/env | Use the native bore client, or ignore if the default is fine |
| `<key>: unknown parameter` | Typo, or unsupported parameter | See the full parameter table above |
| Tunnel disappears after ~60s of network silence | Keepalive reaper (expected behaviour, not a bug) | `ServerAliveInterval`/autossh client-side to survive brief interruptions |
| `connect to host ... port 443: Connection refused` with `ProxyCommand openssl s_client` | Server has no TLS on that port, or `--ssh-gateway` disabled | Check `--cert-file`/`--key-file` and the control port |

Full architecture/analysis doc (including invariants I-SSH1..11):
[`docs/SSH_GATEWAY.md`](docs/SSH_GATEWAY.md).

## Secret tunnels (no public port)

Instead of exposing your service on a public port, you can publish it under a named _secret
id_ and reach it only through a dedicated `bore proxy`. No port is allocated on the server —
the entire path stays internal to the multiplexed connection.

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

`--local-proxy-port :5555` binds all interfaces (so other machines on C's network can reach
it too); use `127.0.0.1:5555` to bind loopback only. The `--tcp-secret-id` on the proxy must
match the one used by the provider. Each id may have a single provider at a time; a second
registration of the same id is rejected — but an id may have **multiple concurrent
consumers** (multiple `bore proxy`/SSH consumers on the same id).

### Direct UDP path (hole-punching)

By default a secret tunnel relays all data through the server. With `--udp` on the server
**and** on both ends, `bore` instead tries to establish a **direct** peer-to-peer path
between the provider and the consumer using UDP hole-punching, with the server acting only as
rendezvous/signaling and stepping out of the data path (lower latency, no server bandwidth).
If the direct path can't be established (e.g. a symmetric NAT, UDP blocked), the tunnel
transparently falls back to the relay.

```shell
bore local 8080 --to https://bore.tld --secret mysecret --tcp-secret-id svc --udp
bore proxy --to https://bore.tld --local-proxy-port :5555 --secret mysecret --tcp-secret-id svc --udp
```

Notes:

- **Requires the `udp` feature**, which is **on by default**. Build `--no-default-features`
  to drop it (and the `quinn` dependency).
- **Reflexive discovery (STUN).** Each peer learns its public address from a STUN chain:
  Cloudflare on the standard `3478/udp` first, then Google, then the server's built-in STUN
  responder on the control port over **UDP** as the final fallback. Open **UDP** on the
  control port too (e.g. `7835/udp`) if you want that self-hosted fallback; override the
  whole chain with `--stun-server host:port`.
  For secret tunnels, the provider also advertises the STUN server that actually produced its
  reflexive candidate. A `bore proxy --udp` consumer asks the server for that
  provider-selected STUN and, when no explicit `--stun-server` override is set, tries it
  first before continuing with Cloudflare, Google, and the bore fallback. A bad or
  unreachable hint is non-blocking; the relay fallback remains available.
- **Authentication.** The direct path is authenticated by a token derived from `--secret` and
  a server-issued nonce, verified before any data flows.
- **Scope & limits.** Only secret tunnels are hole-punchable (not public-port tunnels).
  Reconnecting and multiple consumers are supported (the provider keeps a persistent QUIC
  listener and re-punches toward each one). Both peers behind a symmetric NAT → relay.
- To confirm the direct path is in use, look for `using direct udp path` / `direct udp
  carrier established (… token verified)` in the logs. For the full control-plane story, use
  `-vv` or `RUST_LOG=bore_cli=trace,bore=trace`.

**Hard NATs and firewalls.** Extra, opt-in candidate sources help with difficult networks
(the flags go on `bore local` and `bore proxy`, since both peers punch):

- `--upnp` — acquire a **managed router mapping** and advertise it as a candidate. Tries
  **PCP (RFC 6887 MAP)** against the default gateway first, then falls back to
  **UPnP-IGD**. The mapping is a live *lease*: renewed automatically at half-lifetime,
  retried with backoff on failure (the relay is never affected), re-announced with a fresh
  generation if the gateway reassigns the endpoint or reboots (detected via the PCP Epoch
  Time), and released best-effort on shutdown — bore never leaves a permanent orphan
  mapping behind. Helps strict home routers with a public WAN IP; **no effect behind
  carrier-grade NAT** (mobile/CGNAT).
- `--udp-candidate <IP:PORT>` — declare your own **public endpoint by hand** (repeatable,
  or comma-separated in `BORE_UDP_CANDIDATES`). For static port-forwards, machines with a
  public IP, or port-preserving NATs where you *know* the public mapping. Advertised first,
  alongside anything discovery finds. Pair it with `--nat-udp-preferred-port` so the local
  socket matches the forwarded port.
- `--udp-no-stun` — skip STUN discovery entirely and rely on manual/local/port-mapped
  candidates only (for networks where STUN is blocked by policy). With no `--udp-candidate`
  this almost certainly stays on the relay, and bore says so loudly.
- `--try-port-prediction` — for **symmetric** NATs, advertise a few ports past the
  STUN-observed one. **Strictly opt-in**, best-effort, and **may look like a port scan** to
  strict firewalls.
- `--nat-udp-preferred-port <PORT>` — bind the UDP hole-punch socket to a **fixed** port
  instead of a random one. Set the *same* value on both peers and open it for **egress** in a
  strict firewall. Tip: run `bore test-udp --nat-udp-preferred-port <PORT>` on each host
  first to confirm the port punches through.

Example — both peers know their public endpoints, STUN blocked by policy:

```shell
# provider (static forward 203.0.113.20:41641 → this host :41641)
bore local 8000 --to <server> --secret S --tcp-secret-id svc --udp \
  --udp-no-stun --nat-udp-preferred-port 41641 --udp-candidate 203.0.113.20:41641

# consumer (its own public endpoint, same idea)
bore proxy --to <server> --secret S --tcp-secret-id svc --local-proxy-port :5555 --udp \
  --udp-no-stun --nat-udp-preferred-port 41641 --udp-candidate 198.51.100.7:41641
```

For genuinely untraversable cases (e.g. CGNAT on both ends), the **server relay is the
reliable fallback** — `--udp` never makes a tunnel fail.

For the full theory and an exhaustive NAT/firewall matrix, see
[`docs/nat/NAT_TRAVERSAL.md`](docs/nat/NAT_TRAVERSAL.md) (in Italian).

## Secure file transfer (`bore transfer`)

`bore transfer` builds on the existing secret-tunnel transport: it registers a temporary
secret id, tries the direct UDP path by default, and falls back to the server relay
automatically. Filesystem transfers use a V2 chunked protocol with resume state on the
receiver, multiple worker streams, per-chunk BLAKE3 checks, and a final whole-transfer
verification before the staged tree is committed. The server never stores the payload; it
only brokers the rendezvous or relays the encrypted/plain byte streams when a direct path is
unavailable. If `--to` is omitted, both listener and sender default to
`https://bore.0912345.xyz`.

```shell
# Receiver
bore transfer listener --secret mysecret --transfer-id nightly-backup --dest-path /srv/inbox

# Sender: single file, with 4 parallel worker streams over 4 carriers
bore transfer sender --secret mysecret --transfer-id nightly-backup \
  --sources /home/alice/archive.tar.gz --parallel 4 --carriers 4
```

```shell
# Multiple files and directories in one transfer
bore transfer sender --secret mysecret --transfer-id nightly-backup \
  --sources /home/alice/report.pdf /home/alice/data/ /home/alice/notes.txt \
  --output bundle --parallel 4
```

```shell
# Source list from a file (lines with '#' are comments)
bore transfer sender --secret mysecret --transfer-id nightly-backup \
  --sources /home/alice/extra.tar.gz --source-files /home/alice/backup.list --output bundle
```

```shell
# Sender always prints the source list; --ask-confirm additionally waits for y/N
bore transfer sender --secret mysecret --transfer-id nightly-backup \
  --sources /home/alice/data/ --ask-confirm

# Receiver with --ask-confirm: shows the incoming file list and waits for y/N
bore transfer listener --secret mysecret --transfer-id nightly-backup \
  --dest-path ~/received/ --ask-confirm
```

```shell
# Directory (preserves the directory root and relative layout)
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id nightly-backup --sources /home/alice/project --parallel 4 --symlinks include
```

```shell
# stdin stream (requires an explicit output file name)
tar -cvpzf - project | bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id nightly-backup --sources stdin --output project.tar.gz
```

```shell
# Persistent listener: stays up after each transfer, ready for the next sender
bore transfer listener --secret mysecret --transfer-id nightly-backup \
  --dest-path /srv/inbox --persistent
```

```shell
# Resume a filesystem transfer after an interruption: rerun the same pair with the
# same transfer id, destination root, and unchanged source manifest.
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id nightly-backup --dest-path /srv/inbox
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id nightly-backup --sources /home/alice/archive.tar.gz --parallel 4
```

```shell
# Force relay-only on both sides
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id relay-only-copy --dest-path /srv/inbox --relay-only
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id relay-only-copy --sources /home/alice/archive.tar.gz --relay-only --carriers 4
```

```shell
# Direct UDP path with explicit NAT knobs on both peers; relay is still the
# automatic fallback if hole-punching fails
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id udp-copy --dest-path /srv/inbox \
  --stun-server stun.cloudflare.com:3478 --upnp --try-port-prediction \
  --nat-udp-preferred-port 41641 --nat-udp-release-timeout 120
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id udp-copy --sources /home/alice/archive.tar.gz \
  --stun-server stun.cloudflare.com:3478 --upnp --try-port-prediction \
  --nat-udp-preferred-port 41641 --nat-udp-release-timeout 120
```

```shell
# Control-channel TLS with a self-signed certificate
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id tls-copy --dest-path /srv/inbox --insecure
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id tls-copy --sources /home/alice/archive.tar.gz --insecure
```

```shell
# Existing-destination collision policy lives on the listener
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-fail --dest-path /srv/inbox
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-overwrite --dest-path /srv/inbox --overwrite
bore transfer listener --to https://bore.example.com --secret mysecret \
  --transfer-id collision-rename --dest-path /srv/inbox --rename
```

```shell
# Liveness timeouts: reject if the sender doesn't confirm within 30s; abort stalled
# data within 20s on both sides
bore transfer listener --secret mysecret --transfer-id nightly-backup \
  --dest-path /srv/inbox --confirm-timeout 30 --stall-timeout 20
bore transfer sender --secret mysecret --transfer-id nightly-backup \
  --sources /home/alice/archive.tar.gz --stall-timeout 20
```

```shell
# Symlinks and Unix device files
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id symlink-tree --sources /home/alice/project --symlinks include
bore transfer sender --to https://bore.example.com --secret mysecret \
  --transfer-id device-copy --sources /dev/null --devices include
```

What the transfer command guarantees in V2:

- **Chunked filesystem transfer with resume**: regular files are split into deterministic
  chunks, transferred over one or more worker streams, and can be resumed by re-running the
  same sender/listener pair with the same `--transfer-id` and unchanged manifest. `stdin`
  remains a single-stream, non-resumable byte stream by design.
- **End-to-end verification** with BLAKE3 at three levels: per chunk, per full file, and for
  the final aggregate transfer summary.
- **Staging + commit** on the receiver: files are written into a temporary tree under the
  destination root and published only after the hashes match.
- **Collision policy** is fail-safe by default. Use `--overwrite` or `--rename` on `bore
  transfer listener` to opt into replacement/renaming.
- **Idempotent re-completion** (content-based): if the link drops after the receiver has
  committed the data but before it can send the `Completed` acknowledgement, re-running the
  same sender with unchanged files is safe.
- **Parallel filesystem workers** via `--parallel N`. `--parallel 0` (default) is automatic:
  one worker per CPU core, floored at 4, capped at 32. On the relay path each worker rides
  its own TCP carrier — by default `--carriers 0` (auto) scales the carrier pool to match the
  worker count (capped at the server's `--max-carriers`). Set `--carriers 1` to force the old
  single-connection path, or a fixed `N` to pin it. On direct UDP, carriers are irrelevant:
  each transferred connection already uses an independent native QUIC stream.
- **Cross-platform path fidelity**: Unix raw-byte and Windows UTF-16 path components are
  preserved losslessly on the wire.
- **Live path visibility** in the logs: `direct-udp` or `relay`, plus `quic-encrypted`,
  `tls`, or `plain` transport security.

Notes:

- `--sources stdin` requires `--output`, always uses a single stream, and does not
  participate in chunk resume or `--parallel`.
- `--sources` accepts one or more paths (files or directories). `--source-files FILE…` reads
  additional paths from text files (lines containing `#` are comments). Both flags may be
  combined.
- `--ask-confirm` on the listener is ignored for stdin transfers (data starts right after the
  manifest; there's no safe pause point).
- `--confirm-timeout <secs>` (listener, default `120`, `0` = wait forever): how long to wait
  for `y`/`n` when `--ask-confirm` is active.
- `--stall-timeout <secs>` (both sides, default `60`, `0` = disabled): aborts the transfer if
  no progress is made within the window.
- `--persistent` (listener): stays alive after each transfer; per-transfer errors are logged
  but don't kill the listener.
- Symlinks/devices are opt-in on the sender with `--symlinks include|exclude` and `--devices
  include|exclude`. Unix device transfer is meaningful only on Unix receivers and may need
  elevated privileges to recreate the device node.
- `bore transfer listener` also accepts the legacy `--tcp-secret-id` flag as an alias of
  `--transfer-id`.

## Diagnosing UDP / NAT (`bore test-udp`)

Before blaming the tunnel, find out what *your* network allows. `bore test-udp` opens no
tunnel — it probes public STUN servers and, by default, the bore STUN responder behind
`https://bore.0912345.xyz`. Pass `--to` to probe a different server instead, then classify
the NAT and print advice:

```shell
bore test-udp                                        # public STUN + default bore server
bore test-udp --to https://bore.example.com          # public STUN + another bore server
bore test-udp --stun-server stun.l.google.com:19302  # add an explicit STUN server
```

What it tells you:

- **UDP egress** — whether any STUN server answers at all.
- **NAT class** — `open` (public IP), `cone` (endpoint-independent mapping → hole-punching
  works), or `symmetric` (endpoint-dependent → needs the *other* peer to be cone/open, and
  possibly `--try-port-prediction`). For symmetric it also reports whether the ports look
  **sequential** (prediction has a chance) or random.
- **NAT filtering** — the *other* RFC 4787 axis, and the one that decides the hard cells.
  Two NATs with the same `cone` mapping behave differently depending on whether the filter
  keys on the peer's address only (`address dependent or open` → a symmetric peer CAN reach
  you) or on address *and* port (`address+port dependent` → it cannot). Measured with
  RFC 5780 behaviour discovery, so it needs a server that runs an alternate STUN socket:
  `bore server --udp` does, and `unknown` means nobody could answer the question — never
  that the NAT is restrictive. See [`docs/nat/NAT_TRAVERSAL.md` §19](docs/nat/NAT_TRAVERSAL.md).
- **Plan reason** (paired mode) — the server decides relay-first or direct-first from BOTH
  peers' profiles, and now says *why* with a stable code plus the concrete remedy
  (`peer-port-restricted`, `symmetric-strict-filtering`, `peer-blocked`, `no-candidates`).
  The same line is logged by `bore local`/`bore proxy` when a live tunnel falls back, so the
  machine that went to the relay is the machine that tells you what to change.
  `peer-port-restricted` is also the one cell that now gets an automatic escape — see
  **Sprayed escape** below.
- **Port preservation**, **CGNAT** (`100.64.0.0/10`) / double-NAT detection, and whether a
  **UPnP-IGD** router is present.
- A **co-location/hairpin** note when public STUN works but your own bore server's UDP does
  not.

Run it on **both** peers: a direct path needs each side to be punchable. For a real A↔B
check, run paired mode on two machines with the same id — the server pairs them, exchanges
candidates, tests the direct UDP/QUIC path, then tests the TCP relay fallback:

```shell
# Machine A
bore test-udp --secret mysecret --tcp-secret-id svc

# Machine B, same command/id
bore test-udp --secret mysecret --tcp-secret-id svc

# With bidirectional bandwidth tests (500 MB per direction and per path)
bore test-udp --secret mysecret --tcp-secret-id svc \
  --test-bandwidth --test-transfer-quota 500MB
```

Paired mode also accepts `--upnp`, `--try-port-prediction`, `--nat-udp-preferred-port`,
`--stun-server`, `--udp-candidate`, `--udp-no-stun`, and `--insecure`, mirroring the
direct-path options used by `local`/`proxy` (the standalone probe warns that the
manual-candidate flags only apply to paired mode).

**Retries are full rounds.** When a paired direct attempt fails, each retry binds a fresh
socket, re-runs candidate discovery and re-offers via the server, which waits for BOTH
peers' fresh offers, mints a new nonce, recomputes the adaptive plan, and restarts the
round with an incremented `generation` (shown in the report). Against an older server
that lacks this capability, retries are skipped with an explicit note instead of blindly
re-punching stale candidates from a dead socket. The report also states that the adaptive
candidate *order* is advisory (direct attempts still dial all candidates concurrently
under one budget).

**Sprayed escape (birthday-paradox rendezvous).** One cell of the measured NAT matrix
cannot be won by an ordinary check round: an endpoint-independent side that filters per
address+port, facing a symmetric peer whose source port nobody can predict
(`reason_code=peer-port-restricted` — the common "home router + mobile/CGNAT" pair). For
that cell only, and only after a check round came back dry, bore runs a rendezvous: the
stable side sprays an authenticated probe at many destination ports of the peer's IP
(each one opening its OWN filter for that peer port, which is the blocked direction),
while the symmetric side opens auxiliary sockets, each buying one more external port in
the draw. One collision opens both directions at once. Shipped sizing is 3 × 256 sprayed
ports against 256 sockets — `p = 1 − (1 − 768/64512)^256 ≈ 95%` — for ~2 300 datagrams of
60 bytes across both sides; measured on a kernel NAT harness, the rendezvous closes in
**51 ms**.

It is deliberately narrow. The broker — the only party that sees both NAT profiles —
assigns the two halves, so they are always complementary; it does so only when BOTH peers
advertise the `spray-v1` capability; it never does so for a symmetric × symmetric pair
(there the same trick needs ~170 000 probes for 99.9%, which is a port scan, not a
connection); the relay stays warm throughout; a failed escape falls through to exactly
the fallback that would have run anyway; and the auxiliary sockets are capped at half the
process's remaining file descriptors. Server-side kill switch: `--no-udp-adaptive-plan`
(the role rides the plan). Sizing overrides for harnesses and hostile routers:
`BORE_UDP_SPRAY_PORTS` (256), `BORE_UDP_SPRAY_PASSES` (3), `BORE_UDP_SPRAY_SOCKETS` (256),
`BORE_UDP_SPRAY_PACE_US` (3000), `BORE_UDP_SPRAY_REPEAT_MS` (2500), `BORE_UDP_SPRAY_CAP_MS`
(6000; **0 disables**). Full write-up:
[`docs/nat/NAT_TRAVERSAL.md` §20](docs/nat/NAT_TRAVERSAL.md).

**Mapping classification without a second STUN server.** Classifying the NAT mapping needs
two observations from two different server addresses, and `--stun-server HOST:PORT` builds
a single-element chain by construction — so a private deployment reported `mapping:
unknown` and silently lost every policy that depends on it. bore now falls back to the
server's own RFC 5780 `OTHER-ADDRESS` for the second observation, inside the existing STUN
budget. It is conservative on purpose: a different reflexive proves the mapping is at
least port-dependent (symmetric), while an identical one does NOT prove
endpoint-independence, so this path can conclude "symmetric" and never "endpoint
independent".

**Candidate hardening.** Every candidate list — offered on the wire, brokered by the
server, or punched/dialed — is validated (no port 0/unspecified/multicast/broadcast;
private/CGNAT addresses stay valid for same-LAN), deduplicated, and capped at 16 entries
(`MAX_UDP_CANDIDATES`) before any allocation or per-candidate fan-out. Drops are logged
as one aggregate line. Traversal logs carry stable baseline metrics: `discovery_ms`,
`direct_ready_ms` + `winner`, and a `fallback_reason` enum
(`no-candidates` / `all-candidates-failed` / `budget-exhausted`).

**Budgeted STUN chain, single reader.** Candidate discovery runs on a single-owner
traversal socket: one internal task owns `recv_from` and demultiplexes STUN responses
by transaction id + full source address, so the whole STUN chain is probed concurrently
under ONE 4-second global budget (the old serial chain could burn ~12 s on unreachable
targets before falling back to the relay). The socket is handed to the QUIC stage only
after the reader task has stopped. Offers also carry an optional typed candidate model
v2 (kind/priority/generation/capabilities) alongside the legacy address list — fully
backward compatible; old/new peers interoperate unchanged.

**Authenticated connectivity checks (hole-punch v2).** When BOTH secret-tunnel peers
are new enough (capability-gated over the same brokered exchange), the blind punch is
replaced by paced HMAC-authenticated request/response probes on the punch socket:
an authenticated probe arriving from an un-offered source becomes a *peer-reflexive*
candidate (this is how a cone-side peer now reaches a **symmetric** peer whose real
per-destination port STUN can never see), the first bidirectionally-validated pair is
nominated, and QUIC dials it first. Unauthenticated probes are NEVER answered, and a
response is never larger than a request (no amplification). With any older peer the
legacy punch path runs byte-identical, and the TCP relay stays warm throughout either
way. Measured in the NAT lab: the cone(ADF)-dialer ↔ symmetric-listener profile flips
from relay to direct; symmetric↔symmetric correctly stays on the relay.

**Live adaptive traversal plan (Fase 3).** Each gather now also derives a structured
NAT self-profile with a second STUN observation (the first two chain targets launch
together; the first answer still picks the candidate, a bounded 400 ms confirm window
classifies the mapping as endpoint-independent vs symmetric) and attaches it to the
offer. When BOTH peers report a profile, the server computes a per-pair plan
(`direct-first` / `direct-with-retry` / `relay-first` / `relay-only`, with stable
reason codes in its logs) and delivers it in the punch: the client check round then
probes candidates in staggered kind groups (same-LAN local first, predicted last —
and no predicted probes at all unless predicted candidates were actually offered),
takes its window/retry budget from the plan (hard-capped at 3 s), paces with a
deterministic per-peer jitter that breaks the conntrack-crossfire lockstep of
masquerade routers, and skips the direct attempt entirely on `relay-only`. The
winning remote address is cached for 120 s per tunnel and probed first on
reconnect/upgrade (invalidated on the first failure). VPN 1:1 links adopt the same
offers/checks/plan (the multi-client hub keeps the legacy blind punch). Everything is
capability- and profile-gated: any legacy peer or server degrades field-by-field to
the previous behaviour. Server kill switch: `bore server --no-udp-adaptive-plan`
(env `BORE_NO_UDP_ADAPTIVE_PLAN`) disables plan computation while keeping candidate
exchange and checks active.

**Deterministic NAT lab.** `cargo test --test nat_traversal_test` (Linux) runs the real
traversal stack through userspace-emulated NATs (RFC 4787 mapping/filtering matrices,
port allocation policies) with a pinned per-profile baseline table
([`docs/test/TEST_UDP.md`](docs/test/TEST_UDP.md) §S11); a netns smoke with real kernel
NAT (double masquerade, random-port, UDP-blocked) lives in
`scripts/udp_nat_netns_test.sh` (run as `sudo -n /abs/path/scripts/udp_nat_netns_test.sh`
after `cargo build --release`).

**Secret-tunnel resource gate.** `scripts/perf/secret_leak_hunt.sh` runs a server, a
`bore local --tcp-secret-id` provider and a `bore proxy` consumer inside a *rootless*
network namespace (`unshare -rn` — no sudo, no host port touched) and measures what a
correctness test cannot: descriptors and resident memory across 800–1 000 proxied
connections on both transports, a wedge probe (16 held connections plus bulk waves killed
mid-flight, then a timed fresh round-trip), the control-liveness reaper against a
SIGSTOPped consumer, the punch socket's real `rb`/`tb` from `ss -uapm`, and the
relay→direct upgrade being REPORTED to the server, plus the WORST-CASE time that upgrade
can take (`upgrade-late` holds the provider back for 90 s so the exponential backoff has
climbed, then asserts the tunnel leaves the relay within 75 s — the cap, `UDP_UPGRADE_MAX_SECS`,
is what that number measures). Arms: `all` (default), `churn-relay`,
`churn-direct`, `stall`, `stall-direct`, `reap`, `upgrade`, `upgrade-late`, `udpbuf`. Every
churn verdict is a rate over four identical phases, not a before/after pair — see
[`scripts/perf/README.md`](scripts/perf/README.md).

## VPN — point-to-point L3 tunnel

`bore vpn` establishes a **point-to-point Layer 3 virtual network interface** between two
machines, carrying real IP traffic over bore's NAT-traversing transport. Runs on **Linux**,
**macOS** (Apple Silicon, macOS 13+), **Windows** (10/11, x86_64/i686), and **Android**
(host-only, no gateway mode); the data plane is identical, only the host edge (TUN/WinTun
device + routes/NAT) differs. Requires **root** (`CAP_NET_ADMIN` on Linux; `sudo` on macOS;
an elevated/Administrator shell on Windows), built with `--features vpn`.

On macOS the kernel assigns the interface name (`utunN`) so `--tun-name` is advisory;
`--tun-queues > 1` and the UDP hole-punch helper flags (`--upnp`/`--stun-server`/
`--try-port-prediction`/`--nat-udp-*`) warn and are ignored/advisory (utun has no multi-queue/
offload). Host config uses a per-link PF anchor `bore_vpn/<id>` + `sysctl
net.inet.ip.forwarding` instead of nft/iptables. See
[docs/vpn/VPN_MACOS.md](docs/vpn/VPN_MACOS.md).

On Windows the TUN device is [WinTun](https://www.wintun.net/) (`wintun.dll`, bundled in the
release download); `--tun-queues > 1` warns and is ignored. Host config uses `netsh`/
PowerShell instead of nft/iptables/PF. Overlapping-subnet `real@virtual` netmap and hub-mode
spoke isolation are not yet implemented on Windows. See
[docs/vpn/VPN_WINDOWS.md](docs/vpn/VPN_WINDOWS.md) and
[docs/vpn/VPN_WINDOWS_ACCEPTANCE.md](docs/vpn/VPN_WINDOWS_ACCEPTANCE.md).

On Android, `bore vpn` is **host-only** — no `--advertise`, no `--nat-masquerade`, no
`--forward-accept`, no hub mode, no multi-queue TUN. Enforced by a fail-fast CLI guard. See
[docs/ANDROID.md](docs/ANDROID.md).

### Requirements

- **Linux** (kernel TUN/TAP), **macOS** (Apple Silicon, macOS 13+; utun + PF), **Windows**
  (10/11; WinTun), or **Android** (host-only)
- **Root** (`CAP_NET_ADMIN` on Linux; `sudo` on macOS; elevated shell on Windows)
- Build: `cargo build --release --features vpn`
- **Server:** started with `--vpn --vpn-pool <CIDR>`
- **Shared secret:** `--secret` mandatory (required for E2E encryption on the relay fallback
  path)

### Three topologies

**Host ↔ Host** — neither peer advertises a subnet; each side forwards only its own traffic.

```bash
# Machine A (listener)
sudo bore vpn listen --to bore.example.com --secret S3cret --id mylink

# Machine B (connector)
sudo bore vpn connect --to bore.example.com --secret S3cret --id mylink
```

Both get a `/30` overlay address from the server pool. Ping works immediately. No routes or
IP forwarding involved.

**Site ↔ Host (gateway + roaming client):**

```bash
# Machine A: gateway of LAN 192.168.50.0/24
sudo bore vpn listen --to bore.example.com --secret S3cret --id site \
  --advertise 192.168.50.0/24

# Machine B: roaming client
sudo bore vpn connect --to bore.example.com --secret S3cret --id site
```

Machine B can reach A's LAN. A enables IP forwarding, installs masquerade and MSS-clamp rules
automatically.

**Site ↔ Site (both gateways):**

```bash
# Site A gateway (LAN 192.168.50.0/24)
sudo bore vpn listen --to bore.example.com --secret S3cret --id s2s \
  --advertise 192.168.50.0/24

# Site B gateway (LAN 192.168.60.0/24)
sudo bore vpn connect --to bore.example.com --secret S3cret --id s2s \
  --advertise 192.168.60.0/24
```

Each gateway installs IP forwarding, NAT, and MSS rules. LAN-to-LAN routing requires each
LAN's router to have a route via its bore gateway.

### Security model

- **Direct path:** QUIC datagrams, QUIC-TLS 1.3 end-to-end. Server not in the data path.
- **Relay fallback:** packets sealed with ChaCha20-Poly1305 (key = `HKDF-SHA256(secret,
  nonce)`). Server splices opaque ciphertext — never sees plaintext IP headers. Each
  link/peer derives keys from a fresh per-session CSPRNG nonce with its own monotonic
  counter, so a `(key, nonce)` pair is never reused.
- **Relay replay (known limit):** the relay carries opaque ciphertext and cannot read or
  forge it, but the receiver has no replay window, so a malicious relay could *replay*
  captured frames. Use the direct path (default) for full end-to-end protection.

### Server configuration

```bash
bore server \
  --secret S3cret \
  --vpn --vpn-pool 10.99.0.0/16 --vpn-max-links 32 \
  --webserver-log /var/log/bore
```

- `--vpn`: enable VPN brokering (server must be built with `--features vpn`).
- `--vpn-pool <CIDR>`: allocate `/30` overlay blocks from this pool (required for pool-mode
  clients).
- `--vpn-max-links <N>`: limit concurrent VPN links (default `32`).
- `--vpn-hub-prefix <P>`: overlay subnet prefix `/P` allocated per hub (default `24`).
- `--webserver-log <DIR>`: write access logs (off by default).

### Client options

**Core flags (`listen` and `connect`):**

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `-t, --to <ADDR>` | `BORE_SERVER` | `bore.0912345.xyz` | Server address |
| `-s, --secret <SECRET>` | `BORE_SECRET` | **required** | Shared secret |
| `--id <ID>` | `BORE_VPN_ID` | **required** | Link identifier |
| `--advertise <CIDRs>` | `BORE_VPN_ADVERTISE` | — | Subnets to expose (comma-sep); enables gateway mode |
| `--vpn-addr <IP/PREFIX>` | `BORE_VPN_ADDR` | — | Static overlay address (pool mode if omitted) |
| `--vpn-peer-addr <IP>` | `BORE_VPN_PEER_ADDR` | — | Static peer address (requires `--vpn-addr`) |
| `--tun-name <NAME>` | — | `auto` | TUN interface name; `auto` picks the first free `boreN` |
| `--mtu <N>` | — | `1350` | TUN interface MTU |
| `--pin-mtu` | — | — | Keep `--mtu` fixed; the direct PMTU monitor only warns, never resizes |
| `--no-route-manage` | — | — | Print route/NAT commands instead of running them |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | — | Reconnect with exponential backoff |
| `--relay-only` | `BORE_VPN_RELAY_ONLY` | — | Never attempt the direct UDP path; stay on the relay |
| `--carriers <N>` | — | `1` | Parallel carriers (1–16); flow-pinned, rarely helps a VPN |
| `--tun-queues <N>` | — | `1` | Linux TUN queues (`IFF_MULTI_QUEUE`, 1–8) |
| `--insecure` | `BORE_INSECURE` | — | Skip TLS cert verification |
| `--notes <TEXT>` | `BORE_NOTES` | — | Operator note (logged on link-up) |
| `--max-clients <N>` | `BORE_VPN_MAX_CLIENTS` | `1` | Max concurrent connectors in hub mode (listener side); `1` = legacy 1:1 path |
| `--accept-routes <CIDRs>` | `BORE_VPN_ACCEPT_ROUTES` | — | Explicit accept-list of peer-advertised routes |
| `--accept-all-routes` | `BORE_VPN_ACCEPT_ALL_ROUTES` | — | Accept every peer-advertised route |
| `--refuse-routes <CIDRs>` | `BORE_VPN_REFUSE_ROUTES` | — | Subtract routes from the accept-list (`connect` only) |
| `--skip-split-tunneling` | — | — | Route ALL traffic through the tunnel, not just accept-listed routes (`connect` only) |
| `--nat-masquerade` | — | — | Masquerade NAT'd (`real@exposed`) subnets toward the LAN |
| `--forward-accept` | — | — | Punch an ACCEPT rule for the tun↔LAN pair into a default-deny FORWARD chain |

**NAT traversal flags (shared with `local`/`proxy`):**

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--stun-server <HOST:PORT>` | `BORE_STUN_SERVER` | — | Additional STUN server |
| `--upnp` | `BORE_UPNP` | — | UPnP-IGD router-mapped UDP candidate |
| `--try-port-prediction` | `BORE_TRY_PORT_PREDICTION` | — | Predict symmetric-NAT ports |
| `--nat-udp-preferred-port <PORT>` | `BORE_NAT_UDP_PREFERRED_PORT` | `0` | Fixed UDP hole-punch port |
| `--nat-udp-release-timeout <SECS>` | `BORE_NAT_UDP_RELEASE_TIMEOUT` | `0` | Wait before retrying preferred port |

### Overlapping-subnet NAT and multi-client hub

- `--advertise real@exposed` (e.g. `192.168.1.0/24@10.50.1.0/24`) lets two sites with
  identical real LANs coexist: only the exposed CIDR travels on the wire, mapped 1:1
  (host-bits preserved) to the real subnet locally. See
  [docs/vpn/VPN_NAT_PLAN.md](docs/vpn/VPN_NAT_PLAN.md).
- `--max-clients N>1` on the listener turns it into a hub-and-spoke server for up to `N`
  connectors, each getting its own overlay address and (optionally) its own accepted routes.
  `--max-clients 1` (default) is byte-for-byte the legacy 1:1 path. See
  [docs/vpn/VPN_MULTI_CLIENT.md](docs/vpn/VPN_MULTI_CLIENT.md).

### Performance

TUN I/O uses batch read/write with GSO/GRO offload when the kernel supports `IFF_VNET_HDR`.
Auto-detects on startup; falls back to single-packet if unavailable. Measured iperf3
baseline over loopback: **~13,500 Mbps** (single-packet) → **~14,000 Mbps** (GSO/GRO).
`--tun-queues N` (Linux multi-queue TUN) adds an uplink pump per queue on high-pps links. The
direct path raises the TUN MTU automatically (dynamic PMTU) once QUIC MTU discovery settles.

### Tuning VPN throughput

On a clean LAN/loopback the direct path runs at multi-Gbps and these knobs do nothing. On a
**real Internet path** (RTT + a little loss + a sub-1500 MTU) the dominant limit is almost
always **one inner TCP flow**, not bore. A single TCP connection over any lossy link is
bounded by `≈ MSS / (RTT · √loss)` (Mathis): e.g. at 40 ms RTT and 0.2% loss one flow tops out
near ~25 Mbit regardless of the link rate, while **8 parallel flows over the same tunnel
reach ~170 Mbit** (netns emulation, 250 Mbit cap). So:

- **Parallelise the workload, not the tunnel.** Use a multi-stream tool (`iperf3 -P 8`,
  `rclone --transfers`, parallel `rsync`, multi-connection HTTP). Quick check: `iperf3 -c
  <overlay-ip> -P 8`.
- **`--carriers` rarely helps a VPN — leave it at `1` (default).** bore flow-pins each inner
  connection to one carrier (a single flow is never reordered), but a single bulk flow still
  rides a single carrier. Raise only with **many concurrent heavy flows on a clean,
  high-BDP path**, and measure.
- **`--tun-queues N`** helps only when a single uplink task is CPU-bound at very high packet
  rates.
- **Don't fight the MTU.** For a stable benchmark, pin it: `--mtu 1280 --pin-mtu`.
- **Diagnostics:** run both ends with `RUST_LOG=info,bore_cli::vpn=debug,
  bore_cli::holepunch=debug`. Reproduce locally with `scripts/vpn_bench.sh` (netns + `tc
  netem` WAN emulation).

### Troubleshooting

- **Link pairs but no ping:** check `path=` in logs. If `relay`, run `bore test-udp` to
  diagnose NAT type.
- **Ping ok, TCP slow:** try `--mtu 1280`; verify the MSS-clamp rule: `nft list table inet
  bore_vpn_<id>`.
- **Works from gateway, not from LAN hosts:** the LAN's router needs a route to the peer's
  LAN via the bore gateway.

### Running multiple VPN instances on one host

`--tun-name` auto-selects the first available interface name (`bore0`, then `bore1`,
`bore2`, …), so multiple `bore vpn listen`/`connect` instances can coexist on one host with
no manual configuration or collision:

```bash
# Terminal 1: first connector to listener A
sudo bore vpn connect --to bore.example.com --secret S3cret --id linkA

# Terminal 2: second connector to listener B (same host)
sudo bore vpn connect --to bore.example.com --secret S3cret --id linkB
```

The first instance gets `bore0`, the second `bore1`. To force a specific name, pass
`--tun-name myname`.

### Cleanup

`Ctrl-C` triggers graceful cleanup: routes deleted, IP forwarding restored, nft table
dropped, TUN interface removed. State after exit is identical to before the link started. A
`SIGKILL` leaves stale state; the next `bore vpn --id <same>` reclaims it automatically.

When several gateway links run on the **same host**, `ip_forward` is reference-counted
(`/run/bore-vpn-*.fwdref`): each link restores it only once the last gateway link exits, so
tearing one link down never disables forwarding under another still-running one. Server
liveness is detected within ~15s on a broken socket (TCP keepalive) and within 60s even for a
wedged-but-connected server (control-stream heartbeat timeout), after which
`--auto-reconnect` re-establishes the link with its forwarding/routes intact.

See **[`docs/vpn/VPN_USER_FULL_GUIDE.md`](docs/vpn/VPN_USER_FULL_GUIDE.md)** for the complete
flag reference and use-case guide, and
**[`docs/vpn/VPN.md`](docs/vpn/VPN.md)** for the operator reference and security model.

## Vhost — subdomain reverse proxy

`bore vhost` exposes a local HTTP(S) service at a public subdomain without allocating a
dedicated TCP port:

```shell
bore vhost 127.0.0.1:8080 \
  --subdomain myapp \
  --id client-id \
  --to bore.mydomain.com
# → http://myapp.bore.mydomain.com   (or https:// when a wildcard cert is configured)
```

All subdomains share ports 80 and 443 on the server. The server reads the `Host` header
(after optional TLS termination) and routes each connection to the registered provider for
that subdomain.

### DNS prerequisite

Point a wildcard `A`/`AAAA` record at the server:

```
*.bore.mydomain.com  →  <server IP>
  bore.mydomain.com  →  <server IP>
```

### Server configuration (`vhost.yml`)

Enable the vhost frontend by passing `--vhost-config <path>` to `bore server` (or skip the
file entirely and use `--vhost-base-domain`, see the [full server flag reference](#full-server-flag-reference)):

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

# Optional default headers injected on every routed response.
default_response_headers:
  X-Frame-Options: DENY

# Optional reservations: lock a subdomain to a specific client id.
reservations:
  - subdomain: myapp
    client_id:  my-client-id
    headers:
      X-App-Name: myapp   # merged over default_headers (this key wins)
    response_headers:
      X-Cache: bypass     # merged over default_response_headers
```

The whole merged result — this file with the `--vhost-*` flags applied over it —
is readable at `/admin/api/v1/config` as `vhost_default_request_headers`,
`vhost_default_response_headers` and `vhost_reservations`, and in the
Configuration panel of `/admin/status`. Those fields are **derived from the live
configuration on every read**, so a [hot reload](#hot-reload) is reflected
immediately; the endpoint previously reported the response headers as absent
while the SSH gateway printed all of them to a connecting client.

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

The server polls `vhost.yml`, `cert_file`, and `key_file` every 2 seconds. On a detected
mtime change it reloads atomically — in-flight connections are unaffected.

### `bore vhost` flags

| Flag | Description |
|---|---|
| `<TARGET>` | Local `host:port` to forward to (e.g. `127.0.0.1:8080`) |
| `--subdomain` | Subdomain label to register |
| `--id` | Client identifier for reservation matching |
| `--to` | bore server address |
| `--secret` | Optional server secret |
| `--insecure` | Skip TLS cert verification on `https://` servers |
| `--backend-tls` | Connect to the local backend over TLS — use when the tunnelled service is itself HTTPS (e.g. `https://localhost:3005`). The server originates a TLS session to the backend; a self-signed backend cert is accepted (verification skipped) |
| `--backend-tls-sni NAME` | SNI/hostname sent to the TLS backend (default `localhost`). Only meaningful with `--backend-tls` |
| `--https[=off\|on\|redirect]` | Per-subdomain HTTPS policy (bare = on). Absent inherits the server `--vhost-mode`; falls back to HTTP with a warning if the server has no vhost cert |
| `--carriers N` | Parallel relay connections (default 1). `0` = auto: the server sizes the pool on demand — see [Adaptive carriers](#adaptive-carriers-carriers-0) |
| `--udp` | Try QUIC direct path for the server→provider hop; falls back silently to the TCP relay |
| `--basic-auth user:pass` | Tell the admin page this provider enforces Basic auth |
| `--notes TEXT` | Free-form note on the admin status page |
| `--auto-reconnect` | Reconnect automatically with backoff on disconnect |
| `--webserver-log <DIR>` | Write access logs in nginx-combined format to `<DIR>` |
| `--webserver-log-max-files <N>` | Max rotated log files per target (default 4) |
| `--webserver-log-max-file-size <MB>` | Max MiB per log file before rotation (default 100) |

```shell
# Basic
bore vhost localhost:8080 --subdomain myapp --id myapp

# With explicit server, HTTPS, basic auth, notes, and high concurrency
bore vhost localhost:8080 --subdomain myapp --id myapp --to https://bore.example.com \
  --https on --basic-auth user:password --notes "production api" --carriers 4

# With UDP direct path and authentication secret
bore vhost localhost:8080 --subdomain myapp --id myapp --secret mysecret --udp --to https://bore.example.com

# Reservation (fixed subdomain via ID)
bore vhost localhost:8080 --id myreserved --to https://bore.example.com --subdomain api-reserved

# Plaintext HTTP backend (default) — no flag needed
bore vhost localhost:3000 --subdomain plainapp --id plainapp --to https://bore.example.com

# HTTPS backend with a self-signed cert — the server originates TLS to it
bore vhost localhost:3005 --subdomain secureapp --id secureapp --backend-tls --to https://bore.example.com

# HTTPS backend expecting a specific SNI
bore vhost localhost:3005 --subdomain secureapp --id secureapp --backend-tls --backend-tls-sni app.internal \
  --to https://bore.example.com
```

> **Backend TLS (`--backend-tls`).** By default `bore vhost` sends plain HTTP to
> the local backend, so a backend that only speaks HTTPS (a dev server started
> with a self-signed certificate on `https://localhost:3005`) would reset the
> connection. Add `--backend-tls` and the bore server establishes a TLS session
> to the backend on your behalf (the server is the TLS client), decrypting the
> response so subdomain routing and header injection still work. The same flag
> works with the SSH gateway (`backend-tls=on`, see the SSH gateway section).
>
> **Security:** backend certificate verification is intentionally skipped
> (any certificate is accepted). This is safe for a trusted local backend on
> `localhost`, which is the intended use. Certificate pinning / CA validation
> for the backend leg is not yet supported.

**Server example:**

```shell
# Base domain only, no config file needed
bore server --vhost-base-domain bore.example.com \
  --vhost-cert-file /etc/bore/cert.pem --vhost-key-file /etc/bore/key.pem

# Config file + flag overrides
bore server --vhost-config /etc/bore/vhost.yml --vhost-mode redirect-https
```

### Adaptive carriers (`--carriers 0`)

`bore vhost --carriers 0` hands the carrier count to the server instead of fixing it.
The pool **starts at one** connection and the server adds one, up to **4** and never
past its own `--max-carriers`, when it observes the condition that carriers actually
help with:

> a proxied connection has to be placed on a carrier that is *already* carrying a bulk
> transfer — i.e. every carrier in the pool is occupied.

"Bulk" is decided by **bytes moved**, not by URL, method or content type: a connection
that has moved more than 512 KiB counts as bulk for as long as it is open, and releases
its slot when it closes. Ordinary page assets never trip it.

Why not simply default to a higher `--carriers`? Because on a clean, uncontended path
extra carriers measurably *cost* a little (a paired A/B measured a median `c=4/c=1`
throughput ratio of 0.941), while under concurrent bulk load they are the lever that
spreads work across CPU cores. The right count is regime-dependent, so `0` tracks the
regime and a fixed `N` is still there when you know your own workload.

**Measured** (netns client + provider at 2.1 ms synthetic RTT, private server, two
concurrent bulk transfers on the same tunnel, `scripts/vhost_bulk_isolation.sh`):

| carriers | small-request p50 | small-request p95 | pool |
| --- | --- | --- | --- |
| `1` (default) | 5.92 ms | **15.0–23.5 ms** | 1 |
| `4` (fixed) | 4.41 ms | 5.4–5.6 ms | 4 |
| `0` (auto) | 4.45 ms | 5.4–5.6 ms | **1 → 2 → 3, as the bulk arrived** |

Unloaded, all three sit at p50 4.3 ms / p95 4.5 ms. So the single carrier's tail is
what the feature removes, and the adaptive pool buys it while running the smallest
pool that does.

Three properties worth knowing before you turn it on:

- **It is a ceiling, not a reservation.** Growth stops at 4 or at the server's
  `--max-carriers`, whichever is lower, and an explicit `--carriers N` is never touched
  by any of this.
- **It decays, but it never tears down.** After a quiet minute the server lowers the
  target by one step at a time. Lowering the target only stops the provider from
  *replacing* a carrier that dies; an established carrier is never closed, because a
  proxied connection is pinned to its carrier for its whole life and closing one would
  break live traffic. The pool therefore returns to 1 on the provider's next reconnect.
- **It needs a server that knows about it.** The count is negotiated at registration.
  Against an older server, `--carriers 0` fails immediately with a message telling you
  to use `--carriers N` or upgrade the server — it does not silently run degraded.

With `--udp`, adaptive growth applies to the **TCP relay** pool. On the QUIC direct path
carriers were measured *not* to help (contention there is between streams inside one
connection, not between connections), so that path instead demotes a bulk-classified
stream in the connection's own send scheduler once it passes 512 KiB, so a small request
opened later is not queued behind a large in-flight burst. Neither QUIC receive window
changes — see [Direct-path memory: the sizing rule](#parallel-carriers---carriers).

### Origin failures and path failures

Two failures that used to be silent now say what happened.

**A dead origin answers 502.** When the tunnelled backend is down, its port is
closed, or the provider cannot otherwise reach it, the server synthesizes:

```
HTTP/1.1 502 Bad Gateway
Content-Type: text/plain; charset=utf-8
Content-Length: 43
Connection: close

bore: the tunnelled origin is unreachable.
```

Previously the connection was simply closed with no status line (`curl` reports
`000`), which is indistinguishable from "the tunnel is gone", "the server is down"
and "the network broke". An unknown subdomain still returns **404**, and a
restored origin still serves **200** on the same tunnel — a 502 never poisons the
registration. A failure that happens *mid-response* still closes the connection:
the response head has already been sent, so no status can be substituted.

**A stalled QUIC direct open falls back within 3 seconds.** With `--udp`, opening
the direct stream for a proxied request is bounded; on expiry that **same**
request is served over the warm TCP relay instead. Before this bound a total UDP
blackout destroyed the first request after ~10 s while every subsequent request
was served perfectly — one broken request plus a healthy-looking tunnel, the worst
combination for diagnosis. The deadline is deliberately below the direct path's own
10 s idle timeout, and an *answered* failure (the origin restarting, say) is not
treated as a stalled path.

**The admin page says which path a tunnel is on right now.** `/admin/api/v1/vhost`
and `/admin/api/v1/tunnels` both carry `current_path` (`direct` / `relay` /
`unknown`), `direct_fallbacks`, `direct_stream_opens` and `direct_pool`, and both
the Vhost and the Public Tunnels tables show a **Path** column that reads
`relay (7 fallbacks)` for a `--udp` tunnel currently serving over the relay.
`direct_stream_opens` counts **successful** opens only — as an attempt counter it
climbed during a total UDP blackout, which is precisely when an operator needs it
to be honest. The per-tunnel `direct_fallbacks` matters because the server-wide
metric of the same name cannot answer the question an operator actually asks,
which is whether *this* tunnel is degraded. A tunnel that did **not** ask for
`--udp` reads `relay`, not `unknown`: it has exactly one possible path and the
server knows it. `unknown` means one thing only — a `--udp` tunnel that has not
proxied a connection yet.

**The same 3-second bound applies to a public `--udp` tunnel.** `bore local --udp`
opens the direct stream for each inbound connection under the same deadline and
falls back to the warm TCP relay in place, and a public tunnel that loses its
direct path now re-requests an offer and recovers it without reconnecting. Before
that, a public tunnel whose direct path died stayed relay-only for the whole life
of the control connection — measured still degraded 100 seconds after the path had
healed, against 5 seconds now.

### Control liveness (abandoned registration reaper)

Native `bore local` (public), `bore vhost`, `bore proxy` and `bore sshjhost` clients all
send a control heartbeat every 20 seconds on their control substream. The server tracks
the last frame received and, checked on its own 500 ms control tick, drops a registration
that has been silent for 60 seconds — freeing the **subdomain** for a vhost provider and
the **public port** for a `bore local` tunnel.

This matters because the control substream is multiplexed over the tunnel's TCP
connection: a client whose host was suspended, whose process is wedged, or whose
network went away without closing the socket stays *invisibly* alive — the server's
`send` buffers into the muxer and its `recv` blocks forever. Without the deadline the
subdomain, or the public port, stayed occupied by a dead client until the server was
restarted. For a public tunnel that is the more painful of the two when the server runs
a narrow `--min-port`/`--max-port` range, because a handful of zombies exhaust it.

Two consequences worth knowing:

- **A healthy but idle tunnel is never reaped.** The heartbeat interval (20 s) is far
  below the deadline (60 s), so a tunnel that simply carries no traffic keeps its
  subdomain or port.
- **A client older than this feature is never reaped.** The capability is declared on
  the wire during registration; a client that does not declare it keeps the previous
  heartbeat-free behaviour, because reaping a client that *cannot* heartbeat would kill a
  healthy tunnel every 60 seconds. A stuck subdomain or port held by such an old client
  still needs the client to be restarted manually — upgrade the client to get automatic
  recovery. (Public tunnels gained the capability after vhost and secret tunnels did, so
  an older `bore local` is exactly this case.)

Providers registered through the SSH gateway (`ssh -R vhost/...`) are not covered by this
reaper and do not need to be: the gateway has its own bounded channel-open plus
wedged-session eviction, which releases the label in 20–40 seconds.

**Mixed versions are safe in both directions.** The heartbeat write itself is bounded
(10 seconds, `BORE_CTRL_HEARTBEAT_SEND_TIMEOUT_MS`). A *new* client talking to a server old
enough not to read the control substream would otherwise fill that substream's flow-control
window and stop serving while still appearing registered; instead the client logs a warning
naming the cause, stands its heartbeat down for the rest of the session and keeps forwarding
traffic on the legacy heartbeat-free path. The remedy in that case is to upgrade the server —
the warning says so explicitly.

## Access logging

Access logs record HTTP requests and raw TCP connections in nginx "combined" format, with
real caller IPs, optional size-based rotation, and **zero bandwidth overhead** (logging taps
in-flight bytes without copying, drops records under disk pressure, never blocks the data
path).

| Flag | Default | Description |
|------|---------|--------------|
| `--webserver-log <DIR>` | off | Write logs to this directory |
| `--webserver-log-max-files <N>` | 4 | Rotated log files retained per target |
| `--webserver-log-max-file-size <MB>` | 100 | File size (MiB) before rotation |

Rotation flags without `--webserver-log` are ignored with a warning.

**File naming and layout:**

- Client-side vhost: `<DIR>/<subdomain>.log`. Client-side public tunnel: `<DIR>/<port>.log`.
- Server-side vhost: `<DIR>/<subdomain>/<subdomain>.<domain>.<tld>.log`. Server-side public
  tunnel: `<DIR>/<port>.log`.

**HTTP vs raw/TLS:** HTTP traffic (including keep-alive) is logged per-request in nginx
combined format:

```
IP - - [timestamp] "METHOD PATH HTTP/x.y" status_code bytes "referer" "user-agent"
```

Non-HTTP or TLS-encrypted connections (detected by sniffing the first bytes) are logged at
connection level:

```
IP - - [timestamp] "raw" - bytes_sent+bytes_recv - "-" "-"
```

For **vhost HTTPS**, the server terminates TLS, so per-request logging works. For a public
`local` tunnel carrying TLS end-to-end, the server sees only ciphertext and can only log at
the connection level — a known limitation.

**Real caller IP:** server-side comes directly from the inbound `accept()` address;
client-side is forwarded from the server via a negotiated readiness header
(backward-compatible).

**Examples:**

```bash
# Public tunnel with logging
bore server --webserver-log /var/log/bore
bore local 8080 --to bore.example.com --port 9000 --webserver-log /tmp/bore-logs
# Server: /var/log/bore/9000.log — Client: /tmp/bore-logs/9000.log

# Vhost with logging
bore server --vhost-config /etc/bore/vhost.yml --webserver-log /var/log/bore
bore vhost 127.0.0.1:3000 --subdomain api --to bore.example.com --webserver-log /tmp/bore-logs
# Server: /var/log/bore/api/api.example.com.log — Client: /tmp/bore-logs/api.log
```

```
# Sample log line (HTTP request to a public tunnel)
203.0.113.45 - - [19/Jun/2026:14:23:01 +0000] "GET /path HTTP/1.1" 200 1024 "-" "Mozilla/5.0"
```

## End-to-end deployment recipes

Each recipe below is a **complete, runnable** server + client(s) combination — the "combinatorial"
walkthrough for anyone deploying from scratch. All secrets in these examples are placeholders;
generate real ones with `openssl rand -hex 24`.

### 1. Simplest — plain TCP, no auth (trusted network / testing only)

```shell
# Server
bore server
# Open on the server firewall: control port 7835/tcp + tunnel range 1024-65535/tcp
# (or narrow it: --min-port 20000 --max-port 20100)

# Client — public tunnel, server picks the port
bore local 8080 --to bore.tld

# Client — public tunnel, fixed port
bore local 8080 --to bore.tld --port 9000

# Secret-tunnel provider (no public port opened)
bore local 8080 --to bore.tld --tcp-secret-id my-web

# Consumer (on any other machine)
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555
```

### 2. Authenticated (`--secret`)

```shell
bore server --secret hunter2

bore local 8080 --to bore.tld --port 9000 --secret hunter2
bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2

# Prefer BORE_SECRET over --secret so it never lands in shell history/process list:
export BORE_SECRET=hunter2
bore local 8080 --to bore.tld --port 9000
```

### 3. Clean HTTP addressing on port 80 (still plaintext)

```shell
# Server listens directly on 80 (needs privileges on ports < 1024), or keep 7835
# and forward 80->7835 externally (e.g. Docker `ports: ["80:7835"]`).
sudo bore server --bind-domain bore.tld --control-port 80

bore local 8080 --to http://bore.tld --port 9000
# -> http://bore.tld:9000
```

### 4. TLS/HTTPS (recommended for anything on the public Internet)

```shell
sudo bore server \
  --bind-domain bore.tld --control-port 443 \
  --cert-file /etc/bore/cert.pem --key-file /etc/bore/key.pem \
  --secret hunter2

# Control connection over TLS
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2

# Terminate TLS on the tunnel port too (https/http/raw all work on :9000)
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --https

# ...and force HTTP -> HTTPS redirect
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --https=redirect

# Self-signed cert: add --insecure on client/proxy
bore local 8080 --to https://bore.tld --port 9000 --secret hunter2 --insecure
```

### 5. Direct UDP path for secret tunnels (`--udp` on all three)

```shell
sudo bore server --secret hunter2 --udp
# Also open the control port in UDP (7835/udp) for the self-hosted STUN fallback.

bore local 8080 --to bore.tld --tcp-secret-id my-web --secret hunter2 --udp
bore proxy --to bore.tld --tcp-secret-id my-web --local-proxy-port :5555 --secret hunter2 --udp
```

### 6. Admin dashboard

```shell
bore server --secret hunter2 --admin-token "$(openssl rand -hex 24)"
# open http://your-server:7835/admin/status and paste the token

bore local 8080 --to bore.tld --port 9000 --secret hunter2 --notes "staging web - node A"
```

### 7. Full production (TLS + secret + UDP + admin)

```shell
sudo bore server \
  --bind-domain bore.tld --control-port 443 \
  --cert-file /etc/bore/cert.pem --key-file /etc/bore/key.pem \
  --secret "$BORE_SECRET" --udp \
  --admin-token "$BORE_ADMIN_TOKEN" \
  --min-port 20000 --max-port 20100
# Firewall: 443/tcp + 443/udp (STUN responder on the control port) + 20000-20100/tcp

bore local 8080 --to https://bore.tld --tcp-secret-id my-web \
  --secret "$BORE_SECRET" --udp --notes "internal app - node A" --auto-reconnect

bore local 8080 --to https://bore.tld --port 20001 --secret "$BORE_SECRET" \
  --https=redirect --basic-auth "admin:$WEB_PASS" --notes "protected public dashboard" \
  --auto-reconnect

bore proxy --to https://bore.tld --tcp-secret-id my-web --local-proxy-port 127.0.0.1:5555 \
  --secret "$BORE_SECRET" --udp --notes "office consumer" --auto-reconnect
```

### 8. High-concurrency public/secret tunnel (`--carriers`)

```shell
bore server --secret hunter2 --max-carriers 16

bore local 8080 --to bore.tld --port 9000 --secret hunter2 --carriers 8 --auto-reconnect
# -> http://bore.tld:9000 — flows spread across the 8 carriers

bore server --secret hunter2 --udp --max-carriers 16
bore local 8080 --to bore.tld --tcp-secret-id app --secret hunter2 --carriers 8 --udp --auto-reconnect
bore proxy --to bore.tld --tcp-secret-id app --secret hunter2 --local-proxy-port :5555 --carriers 8 --udp --auto-reconnect
```

### 9. Non-HTTP raw TCP service (e.g. SSH) over a public tunnel

```shell
bore server --secret hunter2
bore local 22 --to bore.tld --port 22000 --secret hunter2 --auto-reconnect

ssh -p 22000 user@bore.tld
```

`--basic-auth` doesn't protect non-HTTP traffic (passed through unchanged): rely on
`--secret` server-side and/or a **secret** tunnel to restrict access.

### 10. SSH ingress gateway, end-to-end (no `bore` binary on the client)

```shell
sudo bore server \
  --control-port 443 --cert-file /etc/bore/cert.pem --key-file /etc/bore/key.pem \
  --vhost-base-domain bore.tld \
  --ssh-gateway --ssh-host-key-file /etc/bore/ssh/host_key.pem \
  --ssh-jump-base-domain ssh.bore.tld \
  --ssh-authorized-keys-dir /etc/bore/ssh/authorized_keys.d \
  --admin-token "$(openssl rand -hex 24)"

# Client machine (only needs OpenSSH):
ssh -T -p 443 -R vhost/myapp:0:localhost:8080 bore.tld

# VM SSH provider (native bore path):
bore sshjhost localhost:22 --subdomain vm-test-01 \
  --to https://bore.tld --secret "$BORE_SECRET" --auto-reconnect

# Operator (gateway account key file is named authorized_keys.d/fabio):
ssh -J fabio@bore.tld:443 ubuntu@vm-test-01.ssh.bore.tld
```

See [SSH jump hosts](#ssh-jump-hosts) and [SSH ingress gateway](#ssh-ingress-gateway) above
for every mode, parameters, `autossh`/systemd templates, and troubleshooting.

### 11. VPN site-to-site, encrypted relay + direct QUIC fallback path

```shell
bore server --secret S3cret --vpn --vpn-pool 10.99.0.0/16 --udp --bind-addr 0.0.0.0

sudo bore vpn listen --to bore.example.com --secret S3cret --id s2s \
  --advertise 192.168.50.0/24
sudo bore vpn connect --to bore.example.com --secret S3cret --id s2s \
  --advertise 192.168.60.0/24
```

See [VPN](#vpn--point-to-point-l3-tunnel) above for host↔host and site↔host topologies, hub
mode, and NAT'd overlapping subnets.

## Protocol

There is a _control port_, `7835` by default (configurable with `--control-port`). The
client opens a single connection to it — plain TCP, or TLS when reached via `https://` — and
[multiplexes](https://github.com/hashicorp/yamux/blob/master/spec.md) everything over that
one connection. At initialization, the client opens a control stream and sends a "Hello"
message asking to proxy a selected remote port. The server responds with an acknowledgement
and begins listening for external TCP connections.

Whenever the server obtains a connection on the remote port, it opens a new multiplexed
stream to the client over the existing connection, and proxies the external connection over
it. This avoids a fresh TCP (and authentication) handshake per proxied connection. The number
of concurrently proxied connections per client is bounded by `--max-conns`.

With `--carriers N` (public tunnels), the client opens `N` connections instead of one: after
the "Hello" the server returns a `CarrierToken`, the client opens `N-1` more connections that
present it (`JoinCarrier`), and the server round-robins each external connection's
multiplexed stream across the pool. The server clamps `N` to `--max-carriers`.

Secret tunnels reuse the same machinery without a public port. A provider (`bore local
--tcp-secret-id`) registers its connection under the id; a consumer (`bore proxy`) opens a
stream per local connection, and the server relays each one to the provider over a freshly
opened stream — splicing the two multiplexed streams together internally.

When a tunnel sets `--https`, the server inspects the first bytes of each connection on the
tunnel port: a TLS `ClientHello` is terminated with the server's certificate (and the
decrypted stream forwarded), a plain HTTP request is redirected to `https://` if
`--https=redirect` is set, and anything else is forwarded as raw TCP.

The SSH ingress gateway demuxes the same control port by peeking the ClientHello ALPN
(`ssh` vs. browser/native-client ALPNs) or, absent ALPN, by a short silence-then-`SSH-`
timeout — see [SSH ingress gateway](#ssh-ingress-gateway) for the full demux story.

## Authentication

On a custom deployment of `bore server`, you can optionally require a _secret_ to prevent the
server from being used by others. The client verifies possession of the secret once, when
establishing the connection, by answering a random challenge in the form of an HMAC code.
(This secret is only used for the initial handshake, and no further traffic is encrypted by
default — use `--cert-file`/`--key-file` for that.)

```shell
# on the server
bore server --secret my_secret_string

# on the client
bore local <LOCAL_PORT> --to <TO> --secret my_secret_string
```

If a secret is not present in the arguments, `bore` will also attempt to read from the
`BORE_SECRET` environment variable.

The SSH ingress gateway replaces this with SSH's own authentication (public keys or Argon2id
passwords) and host-key pinning. Jump operations additionally bind the SSH username to the
matching credential label; existing gateway modes remain username-agnostic. See
[SSH jump hosts](#ssh-jump-hosts) and [SSH ingress gateway](#ssh-ingress-gateway).

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| `connection closed before authentication — wrong --to scheme?` | `--to host:port` (plain) against a TLS server | Use `https://host[:port]`. |
| Client connects but the tunnel doesn't respond from outside | Tunnel port not open on the server's firewall | Open the `--min-port..--max-port` range (or the chosen `--port`). |
| `https://bore.tld` won't connect | The control port isn't 443 | Start the server with `--control-port 443`, forward `443->7835`, or use `https://bore.tld:7835`. |
| Certificate error on a self-signed cert | Cert not trusted | Add `--insecure` on client/proxy. |
| Direct UDP path never comes up, always relay | NAT/firewall, STUN unreachable, `--udp` missing on one side | Run `bore test-udp` on both; ensure `--udp` on server, provider, and proxy, and open `control-port/udp`. See [`docs/nat/NAT_TRAVERSAL.md`](docs/nat/NAT_TRAVERSAL.md). |
| `/admin/status` doesn't respond | `--admin-token` unset or < 32 chars, or wrong port/scheme | Set a token ≥ 32 chars; use the right scheme (`http`/`https`) and control port. |
| Admin token rejected by the page | Wrong token | Re-enter the exact value passed to `--admin-token`. |
| `--carriers N` doesn't seem to open N connections | Server's `--max-carriers` is lower (applies to public tunnels and providers) | Raise `--max-carriers` on the server. The consumer (`bore proxy`) opens its own, not capped by the server. |
| `--carriers` doesn't speed up a single download | One flow uses one carrier (the pool helps concurrency, not a single stream) | Set `bbr` on the host: `sysctl net.ipv4.tcp_congestion_control=bbr`. |
| `tcp-secret-id '<id>' already in use` | A provider with that id already exists | An id has one provider at a time: pick a different id or stop the existing provider (multiple **consumers** are fine). |
| Basic-auth credentials travel in the clear | Tunnel/control not encrypted | Use a TLS server and `--https` on the public tunnel. |
| Connections refused under load | `--max-conns` reached | Raise `--max-conns` (server and/or provider). |
| SSH gateway issues | — | See [Troubleshooting the SSH gateway](#troubleshooting-the-ssh-gateway) above. |

*For NAT/firewall theory, see [`docs/nat/NAT_TRAVERSAL.md`](docs/nat/NAT_TRAVERSAL.md).*

## Acknowledgements

Forked from [ekzhang/bore](https://github.com/ekzhang/bore), created by Eric Zhang
([@ekzhang1](https://twitter.com/ekzhang1)). Licensed under the MIT license.

The author would like to thank the contributors and maintainers of the
[Tokio](https://tokio.rs/) project for making it possible to write ergonomic and efficient
network services in Rust.
