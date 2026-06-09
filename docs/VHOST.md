# VHOST — Subdomain Reverse Proxy

`bore vhost` adds a subdomain-routed HTTP(S) reverse proxy to the bore server. All
subdomains share ports 80 and 443; the server reads the `Host` header and routes
each connection to the registered local service.

---

## Quick start

```shell
# 1. Create a vhost.yml on the server
cat > /etc/bore/vhost.yml <<'EOF'
base_domain: bore.mydomain.com
mode: auto
cert_file: /etc/bore/wildcard.crt
key_file:  /etc/bore/wildcard.key
reservations:
  - subdomain: myapp
    client_id: my-id
EOF

# 2. Start the server
bore server --vhost-config /etc/bore/vhost.yml

# 3. On the client machine
bore vhost 127.0.0.1:8080 --subdomain myapp --id my-id --to bore.mydomain.com
# → https://myapp.bore.mydomain.com
```

---

## `vhost.yml` reference

```yaml
# Required. Base domain for the vhost frontend.
base_domain: bore.mydomain.com

# Frontend mode. Default: auto.
#   http           — HTTP only (port 80). No cert required.
#   https          — HTTPS only (port 443). Cert required.
#   both           — HTTP + HTTPS. Cert required.
#   redirect-https — HTTP redirects (308) to HTTPS. Cert required.
#   auto           — 'http' when no cert, 'both' when cert present.
mode: auto

# HTTP frontend port. Default: 80.
http_port: 80

# HTTPS frontend port. Default: 443.
https_port: 443

# TLS certificate chain (PEM). Required for https/both/redirect-https modes.
# Use a wildcard certificate covering *.bore.mydomain.com and bore.mydomain.com.
# cert_file: /path/to/fullchain.pem
# key_file:  /path/to/privkey.pem

# Default request headers injected on every routed request (first request head
# only — see "MVP limitations" below).
# default_headers:
#   X-Forwarded-Proto: https
#   X-Real-IP: ""

# Default response headers injected on every routed response (first response
# head only). Use this for browser-visible security headers.
# default_response_headers:
#   Strict-Transport-Security: max-age=31536000; includeSubDomains
#   X-Frame-Options: SAMEORIGIN

# Static subdomain reservations. An unlisted subdomain is accepted if free.
# If a subdomain is listed, only the matching client_id may register it.
reservations:
  - subdomain: myapp          # DNS label, e.g. myapp.bore.mydomain.com
    client_id: my-client-id  # must match --id on the bore vhost command
    headers:                  # per-subdomain request headers override default_headers
      X-App-Name: myapp
    response_headers:         # per-subdomain response headers override default_response_headers
      X-Frame-Options: SAMEORIGIN
```

---

## Frontend modes

| Mode | HTTP (80) | HTTPS (443) | Cert required |
|---|---|---|---|
| `http` | serves | — | no |
| `https` | — | serves | yes |
| `both` | serves | serves | yes |
| `redirect-https` | 308 → https | serves | yes |
| `auto` | serves | serves if cert present | no |

The server hard-errors at startup if `https`/`both`/`redirect-https` is set but no cert
is configured. It never silently downgrades the mode.

---

## Header injection

Headers are merged at registration time:

1. `default_headers` from the config root apply to the first routed request head.
2. Per-reservation `headers` override `default_headers` (same key → reservation wins).
3. `default_response_headers` from the config root apply to the first routed response head.
4. Per-reservation `response_headers` override `default_response_headers`.
5. If no request or response headers are configured for a route, the connection is **pure-spliced**
   (`copy_bidirectional`) with zero overhead — multi-GB file transfers work at full speed.

**MVP limitation:** headers are injected on the **first request head** and the
**first response head** of each TCP connection. Subsequent keep-alive exchanges are
spliced raw (headers not re-injected). Full per-request / per-response rewriting
is future work.

---

## WebSocket support

`bore vhost` supports standard WebSocket upgrades over **HTTP/1.1**:

- `ws://` over the plain HTTP frontend
- `wss://` over the HTTPS frontend
- both the normal TCP relay path and `bore vhost --udp`

The reason is simple: once the server has read the first HTTP request head to route by
`Host` (and optionally inject request/response headers on those first heads), the
connection is spliced full-duplex to the provider. After the upstream returns
`101 Switching Protocols`, WebSocket frames flow as an opaque byte stream.

What is covered:

- browser/client -> server over HTTP or HTTPS
- server -> provider over relay TCP
- server -> provider over vhost QUIC (`--udp`)
- text, binary, ping/pong, and close frames

Important caveats:

- The browser-facing side stays **HTTP/TLS to the server** even when `--udp` is enabled.
  Only the server->provider hop switches to QUIC.
- Header injection still applies only to the **first request head** on the connection.
  That is fine for a normal WebSocket upgrade, which happens on the first request.
- bore does **not** claim support for WebSocket over HTTP/2 extended CONNECT
  (`RFC 8441`). The supported path is the classic HTTP/1.1 `Upgrade: websocket` flow.
- If the vhost UDP direct path drops while a WebSocket is already open, that in-flight
  connection drops like any other live stream. New connections fall back automatically
  to the TCP relay.

The WebSocket paths are covered by end-to-end tests for HTTP relay, HTTPS relay, and
vhost UDP.

---

## DNS prerequisite

Point both a wildcard and an apex record at your server:

```
*.bore.mydomain.com  →  <server-public-IP>
  bore.mydomain.com  →  <server-public-IP>
```

---

## TLS: wildcard certificate

Obtain a wildcard certificate for `*.bore.mydomain.com` (e.g. via Let's Encrypt with
the DNS-01 challenge). The same certificate serves every subdomain — no SNI-based
multi-certificate selection is needed (or supported; that is future work).

---

## Deployment topology (which port serves vhost)

The vhost reverse proxy can be reached two ways. Pick one.

### A) Unified single port (recommended for one public IP)

When vhost is enabled, the **control port itself also serves vhost**: after TLS
termination it inspects each connection — the bore protocol (tunnels) and the admin
page work as before, and an HTTP request whose `Host` is `<sub>.<base-domain>` is
routed to that provider. So a single public `443` serves tunnels + admin + every
subdomain, and clients keep using the default `https://<base-domain>`.

Requirements:
- Expose the control port on 443 (`-p 443:7835`, or run the server with
  `--control-port 443`).
- `BORE_CERT_FILE` / `--secret`'s TLS cert must be a **wildcard** covering
  `*.<base-domain>` **and** the apex `<base-domain>`, since browsers TLS-handshake
  against the control-port certificate for `app.<base-domain>`.
- Set `BORE_VHOST_BASE_DOMAIN` (no separate frontend ports needed).

This is the topology a single-IP Docker host wants; it is exactly what the unified
control port was built for.

### B) Dedicated frontend ports

Keep the control port on its own port (e.g. 7835) and let vhost bind standalone
HTTP/HTTPS frontend listeners on `BORE_VHOST_HTTP_PORT` / `BORE_VHOST_HTTPS_PORT`
(default 80 / 443). Publish those ports. Clients then connect to the control port
explicitly (`--to <host>:7835`). Use this when 443 must stay reserved for the raw
bore protocol, or to serve plain HTTP on 80. The frontend HTTPS listener uses
`BORE_VHOST_CERT_FILE` (also a wildcard cert).

> Do **not** map host `443` to the control port *and* also publish a `443:443`
> vhost frontend — only one service can own a host port. In topology A the control
> port does both jobs; in topology B the frontend port does the HTTP job.

---

## Hot reload (zero downtime)

The server polls `vhost.yml`, `cert_file`, and `key_file` every 2 seconds. When an
mtime change is detected:

- **Config changed:** yaml is re-parsed and the in-memory config is atomically swapped.
  In-flight connections keep their captured `Arc`; new registrations see the new rules.
  On parse failure, the old config is kept and an error is logged (no crash, no downtime).
- **Cert/key changed:** a new `TlsAcceptor` is built and atomically swapped. New
  connections see the new certificate; in-flight TLS streams are unaffected. This fires
  both when the file *contents* change (mtime) **and** when the config repoints
  `cert_file`/`key_file` to a different path.

**Restart required for `mode` and ports.** The frontend listener set (which of
HTTP/HTTPS is bound, and on which ports) is fixed when the server starts. Changing
`mode`, `http_port`, or `https_port` in `vhost.yml` updates the in-memory config and is
logged with a warning, but the running listeners are **not** rebound — restart `bore
server` to apply. (Example: starting with `mode: auto` and no cert binds HTTP only; adding
a cert later reloads the TLS material but does not start an HTTPS listener until restart.)

---

## `bore vhost` CLI flags

| Flag | Env var | Default | Description |
|---|---|---|---|
| `<TARGET>` | — | — | Local `host:port` (`127.0.0.1:8080`, `localhost:8080`, `:8080`, `[::1]:8080`) |
| `--subdomain` | `BORE_VHOST_SUBDOMAIN` | — | Subdomain label to register |
| `--id` | `BORE_VHOST_ID` | — | Client id for reservation matching |
| `--to` | `BORE_SERVER` | `https://bore.0912345.xyz` | bore server address |
| `--secret` | `BORE_SECRET` | — | Server authentication secret |
| `--insecure` | `BORE_INSECURE` | false | Skip TLS cert verification |
| `--carriers N` | `BORE_CARRIERS` | 1 | Parallel relay TCP connections (see note below) |
| `--udp` | `BORE_VHOST_UDP` | false | Try a QUIC direct path for the server→provider hop; fall back silently to TCP relay |
| `--basic-auth user:pass` | — | — | Reports Basic auth to admin page (display only) |
| `--notes TEXT` | `BORE_NOTES` | — | Free-form note on the admin page |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | false | Reconnect with backoff on disconnect |

---

## Server-side vhost flags

These flags extend `bore server`:

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--vhost-config <path>` | `BORE_VHOST_CONFIG` | — | Path to `vhost.yml` (optional) |
| `--vhost-base-domain <d>` | `BORE_VHOST_BASE_DOMAIN` | — | Base domain; enables vhost without a file |
| `--vhost-http-port N` | `BORE_VHOST_HTTP_PORT` | (from config / 80) | Override `http_port` |
| `--vhost-https-port N` | `BORE_VHOST_HTTPS_PORT` | (from config / 443) | Override `https_port` |
| `--vhost-quic-port N` | `BORE_VHOST_QUIC_PORT` | (active vhost frontend port, UDP) | UDP port for the vhost QUIC direct path |
| `--vhost-mode <mode>` | `BORE_VHOST_MODE` | (from config / auto) | Override `mode` |
| `--vhost-cert-file <path>` | `BORE_VHOST_CERT_FILE` | (from config) | Override `cert_file` |
| `--vhost-key-file <path>` | `BORE_VHOST_KEY_FILE` | (from config) | Override `key_file` |

**The vhost frontend is enabled by either `--vhost-config` *or* `--vhost-base-domain`.**
A config file is only needed for `reservations` and `default_headers`; everything else
(base domain, mode, ports, cert/key) is fully env-configurable, so a Docker/compose
deployment needs no mounted file for the common case:

```bash
bore server \
  --vhost-base-domain bore.mydomain.com \
  --vhost-cert-file /certs/fullchain.pem \
  --vhost-key-file  /certs/privkey.pem
# or purely via env: BORE_VHOST_BASE_DOMAIN, BORE_VHOST_CERT_FILE, BORE_VHOST_KEY_FILE, ...
```

When both a file and flags/env are set, the flags/env **override** the file's
`base_domain`, ports, `mode`, and cert/key (yaml defaults: `http_port` 80, `https_port`
443, `mode` auto). The `vhost.yml` is still hot-reloaded; env/flag overrides are applied
once at startup.

---

## UDP / QUIC data path

`bore vhost --udp` opportunistically upgrades only the **server→provider** data hop
from yamux-over-TCP to **native QUIC streams**. The browser-facing side is unchanged:

```text
browser -- TCP/TLS --> bore server -- QUIC (optional) --> bore vhost provider -- TCP --> local app
```

What it does:

| Scenario | Effect |
|---|---|
| Many concurrent requests through one provider | Better throughput and tail latency: each proxied request gets its own QUIC bidi stream |
| Lossy / high-RTT provider uplink | Usually better than one yamux-over-TCP carrier because QUIC avoids the single TCP congestion window / HOL issue |
| Server FD pressure | Lower: one QUIC connection can carry many proxied requests |
| Single clean bulk flow | Usually little to no gain over tuned TCP |
| Browser RTT baseline | No change; the browser still talks plain HTTP/TLS to the server |
| Server bandwidth offload | No change; the server still relays every byte |

Important constraints:

- The **server stays in the data path**. This is not the peer-to-peer secret-tunnel `--udp` mode.
- There is **no STUN or hole-punching** for vhost UDP. The provider dials the server's public UDP port directly.
- If UDP is blocked or the QUIC path drops, bore **falls back automatically and silently** to the existing TCP carrier relay.
- `--carriers` still matters for the TCP fallback path; QUIC is only used when the direct server→provider hop is up.

### Firewall / port requirements

If you enable `bore vhost --udp`, open one extra UDP port on the server:

- `BORE_VHOST_QUIC_PORT` / `--vhost-quic-port`
- default: the active vhost frontend port on **UDP**: `https_port` when the resolved mode serves HTTPS, otherwise `http_port`
- distinct from the secret-tunnel STUN responder on `BORE_CONTROL_PORT/udp`

Examples:

- no cert / `mode: auto|http`: `80/tcp` for HTTP frontend + `80/udp` for vhost QUIC
- dedicated frontend: `443/tcp` for HTTPS frontend + `443/udp` for vhost QUIC
- custom QUIC port: `8443/tcp` for HTTPS frontend + `9443/udp` for vhost QUIC

### Authentication model

The direct QUIC path uses the same self-signed-cert + shared-secret model as the
existing direct UDP stack:

- the server sends a per-session nonce on the authenticated control channel
- server and provider derive the same HMAC token from `nonce + --secret`
- the provider authenticates the QUIC connection with that token before any data streams are trusted

Weak-auth caveat: if the control channel itself is plain TCP and the server has **no**
`--secret`, the vhost QUIC auth is correspondingly weak, exactly like the existing
direct-UDP modes. Use `https://...` control and a shared `--secret` in production.

---

## Throughput: `--carriers`, `--udp`, and buffers

The data path multiplexes every proxied connection over the provider's tunnel.
With the default `--carriers 1`, all concurrent requests share a single TCP
connection's congestion window and are subject to yamux head-of-line blocking.
Raise `--carriers N` on `bore vhost` to spread proxied connections round-robin
across `N` parallel carriers, isolating congestion windows and removing HOL
blocking. `1` preserves the single-connection path byte-for-byte.

`--carriers` sizes **both** transports:

- **TCP relay:** `N` parallel TCP carrier connections, capped by the server's
  `--max-carriers`.
- **`--udp` direct path:** the provider opens `N` parallel **QUIC connections**;
  the server pools them and round-robins proxied requests across them, so
  per-connection AEAD/congestion work parallelizes across cores instead of
  funneling through a single QUIC connection (capped at 32 per subdomain).

> **One transfer = one connection.** Carriers parallelize *across* concurrent
> proxied connections, never *within* one. A single large file fetched over a
> single HTTP connection rides exactly one carrier/QUIC stream and is bounded by
> single-stream throughput no matter how high `--carriers` is. To saturate a link
> with one file, split it across many connections (e.g. `aria2c -x16`, ranged
> requests) — or use `bore transfer --parallel N` for native multi-stream file
> transfer.

### Proxy copy buffer: `BORE_PROXY_BUFFER_SIZE`

Each relay hop copies bytes through a per-direction buffer (default **256 KiB**,
tuned for large files on high-latency links). Override it with the
`BORE_PROXY_BUFFER_SIZE` environment variable (raw bytes or a
`KB`/`MB`/`GB`/`KiB`/`MiB`/`GiB` suffix), clamped to `[4 KiB, 16 MiB]`:

- Set it on the **server** to size the relay-side buffers (server↔provider and
  server↔visitor copies, shared with public and secret tunnels).
- Set it on the **`bore vhost` provider** to size that side's local-service splice.

```bash
# server: 1 MiB relay buffers
BORE_PROXY_BUFFER_SIZE=1MiB bore server --vhost-base-domain bore.example.com ...
# provider: match it for the local splice
BORE_PROXY_BUFFER_SIZE=1MiB bore vhost 127.0.0.1:8383 --subdomain upload --id up-1 ...
```

A larger buffer trades memory (≈ `size × 2 directions × concurrent connections`)
for fewer wakeups on high-throughput, high-latency paths. It does **not** raise
single-stream throughput on a low-latency link — see the one-transfer note above.

---

## Reservation semantics

| State | Result |
|---|---|
| Subdomain reserved for this `client_id` | Accepted |
| Subdomain reserved for a different `client_id` | Rejected: `"subdomain 'x' is reserved"` |
| Subdomain not in `reservations` | Accepted if currently free |
| Subdomain already live (another connected client) | Rejected: `"subdomain 'x' in use"` |

A subdomain is freed within milliseconds when the client connection drops (RAII drop
guard removes the registry entry synchronously).

---

## MVP limitations and future work

- **Single label only:** nested subdomains (`a.b.bore.mydomain.com`) are rejected.
  Future: allow configurable nesting depth.
- **Header injection is first-request-only** on a keep-alive connection (see above).
  Future: full per-request HTTP/1.1 framing parser.
- **WebSocket support is HTTP/1.1 Upgrade based.** HTTP/2 extended CONNECT is not
  implemented.
- **SNI-based multi-certificate:** not implemented. All subdomains share one wildcard cert.
  Future: SNI dispatch with per-subdomain certificates.
- **Multi-map per command:** one `--subdomain` per `bore vhost` invocation.
  Future: `--map sub1=host:port --map sub2=host:port`.
- **Vhost QUIC is only the server→provider hop.** The browser-facing side is still
  HTTP/TLS to the server, and the server still relays every byte.
- **Per-client distinct secrets:** reservation identity is by `client_id` string only.
  Future: per-reservation secrets or mTLS.
