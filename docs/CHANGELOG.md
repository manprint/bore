# Changelog

All notable changes to this fork are documented here. Format based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/); this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This is a fork of [ekzhang/bore](https://github.com/ekzhang/bore). The upstream
was a ~400-line TCP tunnel (one connection per proxied connection). The fork
re-architects the transport and adds secret tunnels, a UDP direct path, and NAT
tooling. See `UPSTREAM_CHANGES.md` for the detailed, module-level diff.

## [Unreleased]

### Added
- **Git metadata in `--version`**: `bore --version` now prints
  `bore <version> - <branch> - <sha8>` (e.g. `bore 1.0.0 - main - a1b2c3d4`).
  Branch and commit SHA are embedded at compile time via `build.rs`. In CI
  (GitHub Actions, Docker buildx) the values come from `GITHUB_REF_NAME` and
  `GITHUB_SHA` env vars, so shallow/detached checkouts work correctly. Local
  `just build-*` targets and the Dockerfiles forward `--build-arg` to set the
  same metadata when `.git` is absent from the build context.
- **Paired `bore test-udp` diagnostics** (`--tcp-secret-id <id>`): two machines
  run the same command against a UDP-enabled server, the server pairs them,
  exchanges candidate addresses, tests the direct UDP/QUIC path, verifies the TCP
  relay fallback, and prints a bidirectional report with local/peer NAT summaries.
  `--test-bandwidth` (alias `--test-bandwith`) plus `--test-transfer-quota <SIZE>`
  adds latency and throughput measurements in both directions on both UDP direct
  and TCP relay paths. Paired diagnostics also support `--upnp`,
  `--try-port-prediction`, `--nat-udp-preferred-port`, `--stun-server`,
  `--secret`, and `--insecure`.
- **Carrier pool on every relay leg** (`--carriers N` on `bore local` and
  `bore proxy`, env `BORE_CARRIERS`; `--max-carriers` on `bore server`, env
  `BORE_MAX_CARRIERS`): open N parallel TCP connections and round-robin proxied
  connections across them instead of multiplexing everything over one TCP. Removes
  yamux's single-connection head-of-line blocking and gives each carrier its own
  congestion window — for **concurrent** workloads (parallel rclone/S3/WebDAV
  transfers, many web requests, streaming). Applies to **all three relay legs**: a
  public tunnel (server→client), a secret provider (server→provider, the leg shared
  by all consumers), and a secret consumer (consumer→server). A single bulk flow is
  unaffected (one flow = one carrier); for single-flow loss/high-BDP, tune the host
  (`sysctl net.ipv4.tcp_congestion_control=bbr`). Default `1` keeps the current
  single-connection behaviour; the server clamps a request to `--max-carriers`
  (public/provider pools), a too-large request degrades gracefully, and a dropped
  carrier is pruned + re-dialed automatically — the tunnel never breaks. The server
  stays in the relay data path (this is not the UDP direct path).
- **UDP direct path now uses native QUIC streams.** A secret tunnel's direct
  (hole-punched) path multiplexes each proxied connection over its **own** QUIC
  bidirectional stream — independently flow-controlled and loss-isolated by QUIC —
  instead of running yamux over a single QUIC stream. This removes head-of-line
  blocking on the direct path with no extra connections (so `--carriers` is for the
  relay; the direct path is fixed automatically). The connection is authenticated
  once (token on a dedicated stream); behaviour, multi-consumer support, reconnect,
  and relay fallback are unchanged.
- **HTTP Basic auth on tunnels** (`--basic-auth "user:pass"` on `bore local`,
  env `BORE_BASIC_AUTH`): HTTP requests without valid credentials get a `401`;
  non-HTTP traffic is forwarded unprotected (Basic auth is HTTP-only). Public
  tunnels are enforced on the **server** (creds ride in `TunnelOptions`); secret
  tunnels on the **provider** (covering relay *and* direct paths; creds never
  leave the provider). Tokens are compared in constant time.
- **`--notes "<text>"`** on `bore local` and `bore proxy` (env `BORE_NOTES`):
  a free-form label associated with the tunnel and shown on the admin page.
- **Admin status page** at `/admin/status` on the control port, enabled with
  `bore server --admin-token <token>` (min 32 chars; env `BORE_ADMIN_TOKEN`).
  Served over the same scheme as the control connection (http/https) and on any
  control port. A token-guarded JSON endpoint (`/admin/status/data`) feeds an
  embedded, dependency-free page that auto-refreshes (~2s polling) and lists every
  connected tunnel — public tunnels, and for secret tunnels both the provider and
  all attached `bore proxy` consumers — with client address, options, notes, live
  connection count, and uptime. **Stateless** (no persistence): it reflects only
  what is connected right now. Disabled (and invisible) without a token, leaving
  the control port's bore-protocol behaviour byte-for-byte unchanged.
- **Secure file transfer V2** (`bore transfer listener|sender`): sends a file,
  directory, or `stdin` stream over the existing secret-tunnel transport. The
  command tries the direct UDP path by default and falls back to the relay
  automatically; `--relay-only` disables the direct attempt. Filesystem mode uses
  a manifest, deterministic chunking, receiver-side staging, persisted resume
  state, `--parallel` workers, and BLAKE3 verification at chunk/file/final-summary
  level before commit. `stdin` requires `--output` and remains single-stream /
  non-resumable by design. Listener-side collision policy is fail by default with
  `--overwrite` / `--rename`; sender-side scanning supports
  `--symlinks include|exclude` and `--devices include|exclude`; path encoding
  preserves Unix raw bytes and Windows UTF-16, sanitizing Windows reserved or
  invalid names to `_bore_utf8_<hex>`.
- **Transfer regression coverage**: `tests/transfer_test.rs` and
  `tests/transfer_stdin_cli_test.rs` cover relay/direct/fallback, resume,
  listener-kill recovery/cleanup, TLS control, collision policies, non-UTF8 path
  handling, size boundaries, multi-frame manifests, symlink/device policies, and
  NAT/UPnP flags.

### Added
- **UDP upgrade retry: exponential backoff** (2→256 s, cap at ~4.3 min) replaces
  the fixed 10 s interval. Uses `reconnect::Backoff::new_with(2, 256)` stored in
  `Proxy::upgrade_backoff`. Log now includes attempt number and next retry delay:
  `starting udp upgrade attempt #3; will retry in 16s on failure`.
- **Per-candidate error collection in `connect_direct`**: each QUIC candidate's
  failure reason is captured and logged in the final `warn!` as a structured
  `errors` field (`["addr1 → TimedOut", "addr2 → ConnectionRefused"]`). The
  timeout case now logs explicitly `none responded — all candidates timed out
  (firewall/UDP blocked)` instead of showing an empty `candidate_errors=[]`.
- **Port-release detection** (`--nat-udp-release-timeout SECS`, env
  `BORE_NAT_UDP_RELEASE_TIMEOUT`, default 600 s, on `local` + `proxy`): when the
  NAT remaps the preferred `--nat-udp-preferred-port` (reflexive port ≠ local port),
  the peer switches to ephemeral ports so the NAT entry expires naturally. A
  periodic check (`check_reflexive_port` in `holepunch.rs`) re-probes the preferred
  port every N seconds; when it becomes PRESERVED, the upgrade backoff is reset and
  the next negotiation uses the preferred port. Applies to both consumer
  (`Proxy::listen` in `secret.rs`) and provider (`Client::listen` in `client.rs`
  via `resolve_stun_and_check()`).
- **`reconnect::Backoff::new_with(initial, max)`** and **`Backoff::peek()`**:
  `Backoff` is now parameterized with `initial_secs` and `max_secs` fields.
  `new_with(2, 256)` is used by the UDP upgrade retry. `peek()` returns the
  current delay without advancing the sequence. `reset()` uses the stored initial
  value. The default `new()` keeps the original 1→32 s sequence for
  `--auto-reconnect`.
- **`holepunch::check_reflexive_port(port, stun_addr) -> Option<bool>`**: lightweight
  single-STUN-probe function that returns `Some(true)` if port was preserved,
  `Some(false)` if remapped, `None` if STUN unreachable. Used by both consumer and
  provider port-release detection.
- **`utils/check-reflexive-ports.py` rewritten**: now supports `--port|-p`,
  `--server|-s`, `--timeout|-t`, `--watch|-w` (repeat interval), `--count|-c` (stop
  after N). In watch mode with REMAPPED detection, switches to ephemeral probes to
  avoid refreshing the NAT entry (the observer effect), and reports `released after
  Xs` when the preferred port becomes PRESERVED again.

### Changed
- **Project-wide default server endpoint**: every client-side `--to` now falls
  back to `https://bore.0912345.xyz` when omitted: `bore local`, `bore proxy`,
  `bore transfer listener`, `bore transfer sender`, and `bore test-udp`.
  Explicit `--to` values and `BORE_SERVER` still override the built-in default.
- **Labeled control-plane trace logging**: `Delimited::with_label` now traces
  `tx`/`rx` control frames at `trace` level with role labels (`server/control`,
  `client/public`, `client/provider`, `proxy/consumer`, `test-udp/peer`, ...)
  and redacted summaries, so a `-vv` / `RUST_LOG=...trace` log can be read as a
  full message exchange between server, client, proxy, and paired diagnostics.
- **Live UDP STUN discovery now defaults to public STUN first.** Secret-tunnel
  provider/consumer direct paths and paired `test-udp` candidate gathering try
  `stun.cloudflare.com:3478` first, then Google STUN, then the bore server's own
  UDP control-port STUN responder as the final fallback. `--stun-server` remains
  an absolute override. Logs now show the STUN chain, selected STUN server,
  local UDP socket, reflexive address, offered candidates, peer candidates, and
  direct QUIC candidate attempts to make firewall/NAT debugging easier.
- **Secret UDP consumers now prefer the provider-selected STUN server.** A
  `bore local --udp --tcp-secret-id` provider sends its selected STUN metadata to
  the server with its candidates. A `bore proxy --udp` consumer asks the server
  for that hint before gathering candidates and tries it first, then falls back
  through the normal public/bore chain if it fails. `--stun-server` remains an
  explicit absolute override.
- **Direct QUIC throughput tuning**: the UDP direct path now sets explicit
  high-throughput flow-control windows in `src/holepunch.rs`:
  `DIRECT_QUIC_STREAM_RECEIVE_WINDOW` = 16 MiB,
  `DIRECT_QUIC_CONNECTION_RECEIVE_WINDOW` = 64 MiB, and
  `DIRECT_QUIC_SEND_WINDOW` = 64 MiB. It also requests
  `DIRECT_UDP_SOCKET_RECV_BUFFER` / `DIRECT_UDP_SOCKET_SEND_BUFFER` = 16 MiB,
  keeps `MAX_DIRECT_STREAMS` at 4096, and uses Quinn's
  `quinn::congestion::BbrConfig` instead of relying on Quinn's conservative
  defaults. The constants are documented in code so future tuning can adjust the
  BDP/memory trade-off in one place.

### Fixed
- Transfer filesystem throughput/resume hot path: the listener no longer forces
  `sync_data()` plus a full `state.json` rewrite on every acknowledged chunk.
  Staged-file syncs and resume-state persistence are now batched and serialized,
  which removes a major throughput bottleneck on fast links and fixes the
  intermittent resume-state race seen in fallback-resume tests.
- Cross-platform transfer builds: optional UDP crates (`quinn`, `rcgen`,
  `igd-next`) stay in the top-level dependency set so Windows `--all-features`
  builds resolve them correctly, and Unix device handling in `src/transfer_v2.rs`
  now uses portable `dev_t`/`major`/`minor`/`makedev` helpers so macOS and
  Android no longer fail in the device-transfer path.

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
