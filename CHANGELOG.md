# Changelog

All notable changes to this fork are documented here. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/); this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This is a fork of [ekzhang/bore](https://github.com/ekzhang/bore). The upstream
was a ~400-line TCP tunnel (one connection per proxied connection). The fork
re-architects the transport and adds secret tunnels, a UDP direct path, and NAT
tooling. See `UPSTREAM_CHANGES.md` for the detailed, module-level diff.

## [1.0.0]

First stable release of the fork.

### Added
- **yamux multiplexing** over a single long-lived control connection (TCP, or TLS
  when `--to` is `https://`), replacing the per-connection model. TLS uses the
  rustls **ring** provider (musl/scratch-friendly).
- **Secret tunnels** (`--tcp-secret-id` + `bore proxy`): a provider and consumer
  rendezvous on the server by a shared id, with no public port — the server
  relays substreams between them.
- **UDP direct path** (default `udp` feature): for secret tunnels, provider and
  consumer establish a **direct** peer-to-peer QUIC path via UDP hole-punching +
  STUN, with the server only as signaling/STUN. Automatic, transparent fallback
  to the server relay on any failure — `--udp` never breaks a tunnel. yamux runs
  over one QUIC bidi stream, reusing the whole data path. Direct path is
  authenticated with a token = HMAC(secret, server nonce).
  - **Resilience:** provider keeps a persistent QUIC listener and re-punches for
    each new/reconnecting consumer; the consumer detects a dead direct path and
    reconnects; a relay-mode consumer retries the direct path and **upgrades in
    place** (no dropped session), converging to direct within ~10s.
  - **Hard-NAT options** (opt-in, on `local`/`proxy`): `--upnp` (UPnP-IGD home
    router mapping), `--try-port-prediction` (sequential symmetric NATs),
    `--nat-udp-preferred-port` (fixed UDP port for strict-egress firewalls /
    predictable mapping).
  - **Direct-path concurrency cap:** `--max-conns` on `local` bounds concurrent
    direct substreams (parity with the server relay's cap).
- **`bore test-udp`** — standalone NAT/UDP diagnostic: probes public STUN (and
  your `--to` server's STUN), classifies the NAT (cone/symmetric/CGNAT/blocked),
  checks port preservation and UPnP presence, and prints remediation advice.
- **`--https` / `--force-https`** on a tunnel port (TLS termination / 308 redirect).
- **`--auto-reconnect`** with exponential backoff (`local` / `proxy`).
- **Graceful shutdown**: clean exit on Ctrl-C and SIGTERM (`docker stop` / systemd).
- **`-v`/`-vv`** log-verbosity flags; logs go to stderr with ANSI only on a TTY
  (clean output under Docker/journald/redirection); default level `info`.
- **Docs:** `NAT_TRAVERSAL.md` (hole-punch internals + full provider×consumer NAT
  matrix + admin remediation), `TEST_UDP.md` (manual e2e scenarios),
  `UPSTREAM_CHANGES.md`, updated `README.md` / `CLAUDE.md`.
- **CI/release on every branch**: per-push GitHub Releases with binaries
  (macOS/Linux/Windows/Android) and an amd64 GHCR image; `cargo-audit` gate.

### Changed
- Crate metadata now identifies this fork (`repository`, `authors`); version `1.0.0`.
- The direct-path session nonce and STUN transaction id use the system CSPRNG
  (`ring::rand`); the consumer's QUIC dial tries candidates concurrently under one
  total timeout; the relay→direct upgrade runs off the forwarding loop.

### Security
- Optional HMAC-SHA256 secret auth on the control channel (from upstream), run
  once per connection. The client warns when `--udp` is used without `--secret`.

[1.0.0]: https://github.com/manprint/bore/releases
