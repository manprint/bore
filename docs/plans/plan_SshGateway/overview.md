# SSH Access Gateway — Plan Overview

> **Status:** planning | **Opus authored:** 2026-07-03
> **Folder:** `docs/plans/plan_SshGateway/`
> **Analysis source:** `docs/SSH_GATEWAY.md` (decisions D-SSH1..4, invariants I-SSH1..5 — read it once before Phase 1)

## Goal

Embed an SSH server (russh, feature `ssh-gateway`, off by default) in `bore server` so a
stock OpenSSH client can create public, vhost and secret tunnels with `ssh -R`/`-L` and no
bore binary on the client. SSH is ingress only: from the gateway inward the existing
server data path (registries, relay, admin, weblog, max-conns) is reused. One TCP port
(the control port, deployed as 443) serves SSH + TLS + HTTP + native bore, including
SSH-over-TLS. Acceptance = reference scenario below fully green plus zero regressions on
every existing suite.

```
# Reference scenario (server: bore server --ssh-gateway --ssh-authorized-keys-dir K --vhost-base-domain d --admin-token T)
ssh -p <ctl> -N -R mysub:80:localhost:8080 <srv>          -> curl -H "Host: mysub.d" http://<srv>:<http> == backend body
ssh -p <ctl> -N -R 9005:localhost:8080 <srv>              -> curl http://<srv>:9005 == backend body
ssh -p <ctl> -N -R tcp-id:0:localhost:8080 <srv>          -> bore proxy --tcp-secret-id tcp-id reaches backend
ssh -p <ctl> -N -L 8899:tcp-id:0 <srv>                    -> curl http://localhost:8899 == provider backend
concurrently: native `bore local --udp` tunnel on the same port keeps its QUIC direct path
kill -9 of an ssh client frees its subdomain/id within 75 s; same-key reconnect takes over instantly
```

## Design decisions

| # | Decision | Consequence |
|---|----------|-------------|
| **D1** (=D-SSH1) | `-R` naming: numeric port ⇒ public; label+port 80/443 ⇒ vhost; label+port 0 ⇒ secret provider; explicit `vhost/`/`secret/` prefixes override | One parser, unit-tested matrix (phase 4.2); dotted labels rejected |
| **D2** (=D-SSH2) | Takeover on same auth identity for named resources (vhost subdomain, secret id); public port collision = plain reject | Eviction path in registries; Opus gate on races (phase 5.4) |
| **D3** (=D-SSH3) | Passwords: argon2id hashes only, one `label:hash` per line, any line may match | `bore hash-password` subcommand; concurrency cap on verifies (DoS) |
| **D4** (=D-SSH4) | SSH-over-TLS in v1: second byte-peek after TLS accept | Demux is two-layered (phase 6.2); russh must run over `TlsStream` |
| **D5** | New flat modules `src/sshgw.rs` + `src/sshgw_auth.rs` (project style: flat files, no subdir) | No new directories |
| **D6** | russh API spike = feature-gated cargo integration test driving the real `ssh` CLI (not an `examples/` spike: no sudo/hardware needed) | Runs in normal CI `test` job; skip-guard if `ssh` missing |
| **D7** | `LinkOpener`/`open_ready()` live in `src/mux.rs`; STREAM_READY written only there. Scope: public/vhost/secret client-substream sites ONLY — vpn.rs, vpn_server.rs, udp_diagnostic.rs STREAM_READY sites untouched | Phase 2 refactor is small and greppable; I-4 verifiable by grep |
| **D8** | Gateway enabled solely by `--ssh-gateway`; optional `--ssh-port` adds a dedicated listener; enabling requires ≥1 auth source or startup fails fast | Phases 4–5 test on `--ssh-port`; demux lands later (phase 6) without blocking |
| **D9** | Host key: ed25519, auto-generated at `--ssh-host-key-file` (default `bore_ssh_host_key.pem` in CWD), SHA256 fingerprint logged at startup | Docker volume note in compose (phase 7.3) |
| **D10** | Identity = pubkey comment (else `SHA256:<fp>`) or password label; shown in admin; keys takeover (D2) | Admin `Entry` gains `transport` + `identity` (additive) |
| **D11** | One admin row per SSH consumer session, not per `-L` channel (BUG-S1 parity) | Registration at first direct-tcpip, `active` counter per channel |
| **D12** | Tests never bind literal 443: control port is ephemeral in tests; 443 is a deployment mapping (compose) | e2e portable, unprivileged |

## Architecture summary

Accept path (gateway on): peek 1 byte pre-TLS (`S`⇒SSH, 0x16⇒TLS, HTTP verb⇒HTTP, else bore;
2 s timeout⇒SSH) via existing `Prefixed`; post-TLS second peek for SSH-over-TLS. russh
`Handler` maps `tcpip-forward` ⇒ public/vhost/secret-provider and `direct-tcpip` ⇒ secret
consumer, registering into the existing registries with a `CarrierPool` whose opener is the
SSH session (opens `forwarded-tcpip` channels). `open_ready()` centralizes STREAM_READY so
it never leaks onto SSH channels. Keepalive 20 s / reap 60 s mirrors the secret-tunnel
control invariants.

## Phases

| Phase | File | Model | Shippable alone? |
|-------|------|-------|------------------|
| 1 — Scaffolding + russh spike | [phase_01.md](phase_01.md) | Haiku + Sonnet | yes (additive, feature off) |
| 2 — LinkOpener / STREAM_READY confinement | [phase_02.md](phase_02.md) | Sonnet + Opus gate | yes (zero behavior change) |
| 3 — Auth stores + hash-password | [phase_03.md](phase_03.md) | Sonnet + Haiku | yes (additive, unwired) |
| 4 — Gateway core + public tunnels | [phase_04.md](phase_04.md) | Sonnet + Opus gates | yes (on `--ssh-port`) |
| 5 — Vhost + secret mapping + takeover | [phase_05.md](phase_05.md) | Sonnet + Opus gate | yes |
| 6 — Demux on control port + SSH-over-TLS | [phase_06.md](phase_06.md) | Sonnet + Opus gate | yes |
| 7 — Admin FE, netns harness, CI, docs | [phase_07.md](phase_07.md) | Haiku + Sonnet + Opus final read | yes |

## Reuse map (top candidates)

| Need | Reuse | Location |
|------|-------|----------|
| Public port bind + range check | `create_listener` | `src/server.rs:1011-1025` |
| Public per-conn accept/splice pattern | `serve_tunnel` loop | `src/server.rs:1706-1843` |
| Carrier pool + failover | `CarrierPool`, `Registry` | `src/secret.rs:80` |
| Consumer relay + failover loop | `serve_consumer` relay section | `src/secret.rs:441,~600-743` |
| Vhost entry + registry + relay | `VhostEntry`/`VhostRegistry`/`relay_vhost` | `src/vhost.rs:339-385,471,772-824` |
| Subdomain label validation | `extract_subdomain` | `src/vhost.rs:160` |
| Byte-peek + replay | `Prefixed`, `is_http_first_byte` | `src/prefixed.rs`, `src/admin_http.rs:46` |
| Socket tuning / byte counting | `tune_tcp`, `CountingStream` | `src/shared.rs:263,41-70` |
| Admin RAII registration | `register`/`Registration`/`Entry` | `src/admin.rs:289,413-457,43-122` |
| HTTP basic auth check | `basicauth` module | `src/basicauth.rs` |
| Feature-gate pattern | `vpn` feature wiring | `Cargo.toml:35-42`, `src/lib.rs:40-51` |
| netns e2e pattern | `secret_netns_test.sh` | `scripts/secret_netns_test.sh` |
| FE flag badges | `flagBadges` | `src/admin_ui/ui.js:86-100` |

## Invariants

- **I-1** (=I-SSH1): `--ssh-gateway` absent ⇒ accept path and all data paths byte-identical to today; enforced by keeping the legacy branch untouched and by full-suite regression at every phase.
- **I-2** (=I-SSH2): any parameter unsupported over SSH (udp, carriers, hole-punch flags) produces an explicit warning on the SSH channel — never silently ignored.
- **I-3** (=I-SSH3): gateway keepalive 20 s + reap at 60 s silence; a half-open SSH connection never leaves zombie entries in vhost/secret/admin registries (RAII drop on handler exit).
- **I-4** (=I-SSH4): `mux::STREAM_READY` (`src/mux.rs:35`, single byte 0) is written only by the link layer (`open_ready`), never onto an SSH channel. vpn/udp_diagnostic write sites are out of scope and untouched.
- **I-5** (=I-SSH5): takeover only at identical auth identity; different identity ⇒ reject.

## Risk register

| Risk | Mitigation |
|------|-----------|
| russh API doesn't expose a needed hook (forwarded-tcpip open, env, keepalive) | Phase 1 spike proves every primitive before any wiring; findings file gates design tweaks |
| STREAM_READY refactor drifts behavior on native paths | Phase 2 is standalone, Opus-gated, full cargo + netns regression before anything SSH lands |
| Demux breaks native TLS clients or adds latency | I-1 branch isolation + T-DMX-OFF regression + Opus gate (phase 6.1) |
| argon2 verify used as DoS vector | Concurrency cap semaphore + unit test (phase 3.2) |
| Takeover races (double eviction, insert race) | DashMap atomic entry ops, synchronous evict-then-insert, Opus gate (phase 5.4) |
| SSH clients that wait for server banner | 2 s peek timeout ⇒ SSH fallback, tested by T-SSH-DMX2 |
| CI runner missing `ssh`/`ssh-keygen` | Skip-guard in tests + explicit `openssh-client` install step (phase 7.3) |

## Model-assignment summary

| Model | Sub-phases | Role |
|-------|-----------|------|
| Opus | Review gates: 2.2, 4.3, 4.4, 5.4, 6.1, 7.5 (final read) | Architecture, lifecycle/races, hot accept path, sign-off |
| Sonnet | 1.2, 2.1, 2.2, 3.1, 3.2, 4.1, 4.2, 4.3, 4.4, 5.1, 5.2, 5.3, 5.4, 6.1, 6.2, 6.3, 7.2 | All non-trivial implementation + tests |
| Haiku | 1.1, 3.3, 4.5, 7.1, 7.3, 7.4 | Scaffolding, boilerplate flags/fields, FE badge, CI/compose edits, docs |
