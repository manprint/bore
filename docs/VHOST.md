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

# Default headers injected on every routed request (first request head only —
# see "MVP limitations" below).
# default_headers:
#   X-Forwarded-Proto: https
#   X-Real-IP: ""

# Static subdomain reservations. An unlisted subdomain is accepted if free.
# If a subdomain is listed, only the matching client_id may register it.
reservations:
  - subdomain: myapp          # DNS label, e.g. myapp.bore.mydomain.com
    client_id: my-client-id  # must match --id on the bore vhost command
    headers:                  # per-subdomain headers override default_headers
      X-App-Name: myapp
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

1. `default_headers` from the config root apply to every subdomain.
2. Per-reservation `headers` override `default_headers` (same key → reservation wins).
3. If no headers are configured for a route, the connection is **pure-spliced**
   (`copy_bidirectional`) with zero overhead — multi-GB file transfers work at full speed.

**MVP limitation:** headers are injected on the **first request head** of each TCP
connection. Subsequent requests on the same HTTP keep-alive connection are spliced
raw (headers not re-injected). Full per-request rewriting is future work.

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
| `--basic-auth user:pass` | — | — | Reports Basic auth to admin page (display only) |
| `--notes TEXT` | `BORE_NOTES` | — | Free-form note on the admin page |
| `--auto-reconnect` | `BORE_AUTO_RECONNECT` | false | Reconnect with backoff on disconnect |

---

## Server-side vhost flags

These flags extend `bore server`:

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--vhost-config <path>` | `BORE_VHOST_CONFIG` | — | Enables vhost frontend |
| `--vhost-http-port N` | `BORE_VHOST_HTTP_PORT` | (from config) | Override `http_port` |
| `--vhost-https-port N` | `BORE_VHOST_HTTPS_PORT` | (from config) | Override `https_port` |
| `--vhost-mode <mode>` | `BORE_VHOST_MODE` | (from config) | Override `mode` |

The port/mode flags **override** `vhost.yml` only when passed. When omitted, the values
from `vhost.yml` are used (yaml defaults: `http_port` 80, `https_port` 443, `mode` auto).

---

## Throughput: `--carriers`

The relay data path multiplexes every proxied connection over the provider's bore tunnel.
With the default `--carriers 1`, all concurrent requests share a single TCP connection's
congestion window and are subject to yamux head-of-line blocking. For high-throughput or
highly-concurrent workloads, raise `--carriers N` on `bore vhost`: proxied connections are
spread round-robin across `N` parallel TCP carriers (capped by the server's
`--max-carriers`), isolating congestion windows and removing HOL blocking. `1` preserves
the single-connection path byte-for-byte.

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
- **SNI-based multi-certificate:** not implemented. All subdomains share one wildcard cert.
  Future: SNI dispatch with per-subdomain certificates.
- **Multi-map per command:** one `--subdomain` per `bore vhost` invocation.
  Future: `--map sub1=host:port --map sub2=host:port`.
- **QUIC server↔client transport:** the relay uses yamux-over-TCP (same as other tunnels).
  Future: QUIC on the relay path for improved throughput.
- **Per-client distinct secrets:** reservation identity is by `client_id` string only.
  Future: per-reservation secrets or mTLS.
