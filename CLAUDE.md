# CLAUDE.md

## Optimization & model selection

**Target: minimize input/output tokens.** Caveman ultra (`/caveman ultra`); caveman plugin for subtasks; show model used per task/subtask.

- **Haiku 4.5** (`claude-haiku`, $1/$5 MTok) — lint, grep-style search, syntax checks, *codebase exploration*, routing/classification in multi-agent flows, structured-data extraction (parse logs, JSON), short repetitive output (commit msgs, var names), mechanical (non-reasoning) sub-agent tasks.
- **Sonnet 4.6** (`claude-sonnet`, $3/$15 MTok) — DEFAULT 90%+: features, refactor, tests, non-trivial debug, doc write/review (simple → *delegate Haiku*), code review w/ explanation, sustained agentic loops.
- **Opus 4.8** (`claude-opus`) — supervisor, quality-critical (5–10× cost): cross-file architecture, multi-step reasoning Sonnet gets wrong, deep-research synthesis, verify Haiku/Sonnet respect specifics, broker tasks to Haiku/Sonnet.

Rule: start Sonnet. Drop to Haiku for bulk/mechanical. Escalate to Opus only when Sonnet concretely insufficient or task critical.

## Agent workflow

- **Analysis:** structured output files by phase/sub-phase; each entry self-contained (usable downstream w/o extra context); preserve all orchestrator considerations/decisions — nothing implicit/assumed.
- **Implementation:** phase by phase, sub-phase by sub-phase. Per unit: (1) tests first/alongside; (2) CI gates pass (`cargo fmt`, `cargo clippy -- -D warnings`, `cargo test`); (3) full regression before done; (4) **zero regressions** — a sub-phase breaking an existing test is NOT done.
- **Documentation:** every behavior/API/invariant change updates the matching markdown doc (part of deliverable). **README.md = single source of truth:** every change (feature, flag, flow, deploy step, binary or `--ssh-gateway` mode) updates README.md in the SAME change — covers full E2E: deploy + all modes (binary + SSH gateway) + every CLI flag + example/`sudo` commands. Not in README.md ⇒ not done.
- **Quality bar:** correct before clever; surface uncertainty explicitly (never paper over); high quality = baseline; internal+unit+e2e gates 0 fails; authorized to launch sudo tests (sudoers-permitted).

## What this is

`bore` — async Rust TCP/UDP tunnel/proxy/transfer (`#![forbid(unsafe_code)]`). Exposes a local port through a remote server, bypassing NAT/firewalls. Ships `bore_cli` lib + `bore` binary.

**Six subcommands:** `bore local <port>` (public tunnel: server assigns public port → local `<port>`); `bore proxy` (secret consumer: connect named provider → relay to local port); `bore server` (relay server); `bore transfer listener|sender` (file transfer: resume, BLAKE3 verify, parallel streams); `bore test-udp` (NAT/UDP diag; `--tcp-secret-id` → two-peer latency/bandwidth test); `bore vpn listen|connect` (Linux L3 VPN, `--features vpn`, root/CAP_NET_ADMIN). Plus `bore vhost` (subdomain reverse proxy) and `bore hash-password` (argon2id line, `--features ssh-gateway`).

**Core transport:**
- One long-lived yamux-muxed TCP conn per tunnel (control port 7835); plain TCP or TLS (`https://` URL).
- Public: server opens data substreams → client splices to local. Secret: consumer opens substreams → server relays to provider → provider splices to local.
- `--carriers N`: N parallel TCP conns, round-robin per proxied conn (HOL + cwnd isolation).
- `--udp`: UDP hole-punch + QUIC direct for secret tunnels (each proxied conn = own QUIC bidi stream); auto-fallback to relay.
- `--udp` for PUBLIC `bore local` (no `--tcp-secret-id`): server→client QUIC direct, **no STUN/hole-punch** (server public, client dials it — same as `bore vhost --udp`). Client opens N (`--carriers`) QUIC conns to server `--vhost-quic-port`; server round-robins inbound across them (own `PublicDirectEntry`/`DirectPool` per tunnel), writes `mux::STREAM_READY`, splices; per-conn fallback to warm TCP relay. Needs `bore server --udp`.

## Key invariants — INDEX (full entries: `docs/INVARIANTS.md`)

Each line = the rule. `docs/INVARIANTS.md` = why, field measurements, gates, red-checks — read the matching entry BEFORE touching that area. Changing an invariant ⇒ update BOTH the full entry there and its line here.

**Testing:** netns harnesses run SERIALLY (shared ns names). Sudo is NOPASSWD per exact path: `sudo -n /abs/path/scripts/<x>.sh` (never `sudo bash scripts/...`). Netns harness refuses a stale release binary: rebuild as user first.

### Control liveness / reapers
- **Secret reaper:** `serve_provider`/`serve_consumer` reap on `last_recv.elapsed() >= ctrl_timeout`, checked on the 500 ms heartbeat tick — NEVER `timeout(recv)` (`select!` resets it, DEC-VE3). `ClientMessage::Heartbeat` appended LAST (upgrade server first).
- **Vhost F-1 / Public P-4:** same reaper in `serve_vhost_provider` / `serve_tunnel`. DEC-VE2: `ctrl_heartbeat.then_some(timeout)` IS the compat gate — a legacy client is never reaped. Never set wire `ctrl_heartbeat` without `sends_ctrl_heartbeat` (same constructor). SSH vhost labels use I-SSH10 only.
- **P-9:** every heartbeat write goes through `client::beat_once` (bounded; `PeerNotReading` ⇒ stop beating for the session). Never a bare `send` in a `select!` arm.
- **P-7:** `serve_tunnel` has a `control.recv()` arm; `PublicUdpRenew` re-offered via `send_public_udp_offer` with a FRESH nonce; `Ok(None)`/`Err` ends the tunnel.
- **O-1 (plan 005):** liveness measured at the TRANSPORT (`mux::ConnActivity`), never on control frames. Client trips at 15 s inbound idle (`BORE_CTRL_SERVER_SILENCE_MS`) on a tick, terminates main + every carrier, reconnects; beats 2 s, declared via `ctrl_heartbeat_ms`; server reaps after `max(3×declared, 15 s)` via `liveness::reap_if_due`; backoff cap 8 s; `TunnelOptions.preferred_port`. Secret consumer on DIRECT never trips; VPN pairing teardown + `PumpGuard`; browser WS ping 1 s / reap 20 s; SSH keepalive 1 s / reap 15 s. Rejected: `TCP_USER_TIMEOUT`, fatal carrier-token rejection, zombie identity takeover.
- **M-1:** mux driver closes when the `Liveness` count (every `Opener` + `Acceptor` + every substream via `TrackedStream`) hits 0. Register the waker BEFORE reading the count.
- **S-2:** the consumer's `SecretPathReport` is the only truth for secret `current_path`; re-sent on relay→direct upgrade (`reason: None`), capability learned from any `UdpPunch`, upgrade write `timeout`-bounded.

### Direct QUIC / UDP budget
- **P-13:** `configure_udp_socket_buffers` is called ONLY inside `client_endpoint`/`server_endpoint`. TCP side deliberately sets no `SO_*BUF` (kills autotuning).
- **P-14:** direct-carrier close monitor holds a `Weak` of the entry it INSTALLED into; never re-resolve the registry key at close (ssh-jump is the precedent).
- **P-15:** public/vhost deregistration `Drop` calls `entry.direct.close_all()`, removes the key only if `Arc::ptr_eq`; "arrived after deregistered" branches call `direct.close()`.
- **F-13 `--udp-memory-budget`:** `from_memory_budget` keeps the 16:1 ratio and never inflates the window; `admit_direct` is the ONE admission point, permit released on `direct.closed()`; refusal ⇒ warm relay + `direct_budget_refusals`. Unset ⇒ byte-identical. Conflicts with the window flags.
- **Windows:** `connection_receive_window` (256 MiB) >> `stream_receive_window` (16 MiB). Never shrink conn toward stream, never raise stream (DEC-VE8).
- **P-11:** `udp_direct_slots()` = CONFIGURED total; the gauge is `udp_direct_slots_available` on metrics. FE uses a null check, never truthiness.
- **P-10:** public `current_path` = `relay` when no direct entry and no `--udp`; `unknown` only for a `--udp` tunnel with no connection yet.
- **P-12:** `fdlimit::reconcile_fd_limit(max_conns)` before the first bind; soft → `max_conns + 256`; silent when sufficient. `rlim_t` width varies by target — convert only at the boundary (`widen`/`narrow`, saturating).
- **W-1:** additive `#[serde(default)]` is NOT safe across a party that re-serializes typed structs (server forwarding `UdpTestPeerSummary`). Diagnostics never blame the peer alone.

### Secret tunnels / NAT traversal
- Secret consumer carriers (`ConnectSecret{carrier:true}`): no admin row, no reap. One logical tunnel = ONE admin row. Provider carriers use `JoinCarrier`/`serve_carrier` — do not fork.
- Hole-punch strays (`DirectListener::accept`) = `debug`, never WARN; token auth is the gate; never filter by offered candidates (CGNAT).
- `relay()` fails over across provider carriers; `--carriers N>1` on a DIRECT consumer warns once.
- **N-4:** `Proxy::new` waits `INITIAL_DIRECT_PROBE_GRACE` (1.5 s) for a SIGN UDP leaves the network (`GatherOptions::reflexive_seen`, fired on the first STUN answer — never after the filtering probe), then `INITIAL_DIRECT_VERDICT_GRACE` (4 s) for the verdict; otherwise returns on the relay (`control: None` + `initial`) and `Proxy::listen` adopts the verdict via the same `adopt_initial_outcome`. Never a flat deadline on the verdict (healthy negotiation ≈ 1–4 s).
- **N-5:** bounded by construction — TLS handshakes 10 s (`TLS_HANDSHAKE_TIMEOUT`: control port both branches + HTTPS vhost), ≤256 unauthenticated QUIC handshakes (Retry past half), STUN/direct-endpoint DNS 2 s in parallel and none with `--udp-no-stun`, `broker_udp` offer `NETWORK_TIMEOUT`.
- **N-6:** `TokenGuard` before the `CarrierToken` send; `DirectPool::install` refuses after `close_all`; relay opens fail over across carriers on public + vhost; public tunnel capped by its client's `--max-conns` (per-tunnel semaphore, clamped to the server's).
- **S-5/S-7:** the listener ends the check round on the dialer's authenticated request (reply on the wire BEFORE announce, `nominated` set) and must NOT exit on `validated_rx`. `DIRECT_INITIAL_RTT` 100 ms. **S-8:** `UDP_UPGRADE_MAX_SECS` ≤ 60 s (const-asserted). **S-9:** `pair_cache` swept in `remember`, capped 256.
- **Fase 0–3/5:** candidates capped 16; single-owner `UdpTraversalSocket`; HMAC check round only when both advertise `check-v1`; broker plan only with BOTH profiles (missing ⇒ never RelayOnly); normalized generation; `MAX_FRAME_LENGTH` 8192 fits the worst-case `UdpPunch`; never add a `UdpCandidateKind` variant (use RouterMapped); `--upnp` = managed PCP/UPnP lease.
- **Fase 6–7:** the NON-symmetric side's FILTERING decides; `port_restricted()` never treats `None` as restrictive. Spray roles broker-assigned, complementary, a `String`; easy side decides + `CONFIRM_COPIES`; aux sockets ≤ half `fd_headroom()`; spray recv errors are transient (Windows `WSAECONNRESET`). OTHER-ADDRESS observation runs AFTER `probe_filtering`; may conclude `Symmetric`, never `Eim`.
- **V-2:** `test-udp --tcp-secret-id` runs the SAME check round (`establish_direct(Option<&CheckConfig>)`), gated on the peer's `UdpTestPeerSummary.checks`; no pair cache; spray role only from a server-brokered plan.

### Data path / framing
- Client sends `Hello` (and `HelloVpn`/`ConnectVpn`) BEFORE auth (lazy yamux, else deadlock).
- Server writes `mux::STREAM_READY` before splice (direct bidi streams too).
- `copy_bidirectional_with_sizes` propagates half-close — never replace it.
- Never `tokio::io::split` a `mux::Stream` across two tasks (single yamux waker). One stream = one task.
- `shared::tune_tcp` (`TCP_NODELAY` + keepalive 15 s) on every socket. The `--max-conns` semaphore is the real bound.
- `carriers<=1` = byte-for-byte single path. Defaults: 1 for local/proxy/vhost, 0 (auto) for transfer.
- Relay path is AEAD-opaque (server splices ciphertext).
- **V-14a:** `relay_writer` coalesces via `recv_many` (cap 32) and never waits for company. **V-14b:** `SealKey` seals/opens in place, one allocation; the free fns `crypto::seal_with_counter`/`open` are the ORACLE — never rewrite them to call the cached path.

- **N-1 (1.4.0):** mux transport = `BufStream<ActivityIo<S>>` (`mux::mux_io`), `split_send_size` = `mux_buffer_size()` (256 KiB, `BORE_MUX_BUFFER_SIZE`, clamp 16 KiB–1 MiB = yamux frame limit). Activity counter stays inside the buffer; each side sizes only what it sends (never negotiate).
- **N-2:** mux `drive` never awaits the acceptor: `PollSender` + local backlog (256) inside the `poll_fn`.
- **N-3:** musl x86_64/aarch64 = mimalloc via `crates/bore-alloc` only; `bore_alloc::configure()` first statement of `main` (purge delay 0 through the C shim). Other targets = `System`.

### Vhost
- Injected-response path: `flush()` after every write before parking on read; no `tokio::io::copy`; split + `try_join!` in ONE task. Gates are FlushGatedWriter mocks (loopback TLS false-passes).
- Backend TLS: server-side rustls client, ALPN `http/1.1` only, 10 s handshake bound, rebinds the same `provider` var; `false` path byte-identical.
- Phase 03: bulk classified by BYTES (`BulkTicket`, 512 KiB); `choose_carrier` reduces to round-robin with no bulk; `--carriers 0` = auto (ceiling `min(4, --max-carriers)`), `SetCarrierTarget` only to `auto_carriers` clients; lowering the target never closes a carrier. QUIC `BulkSendState` demotes priority, never touches windows.
- Phase 05: 502 synthesized in `OriginFailureResponder::poll_shutdown` (not earlier, not later); `DIRECT_OPEN_TIMEOUT` 3 s covers open + STREAM_READY; `direct_stream_opens` counts successes; `last_path` published.
- Phase 06: benign disconnects classified by `ErrorKind`, logged `debug`; `/admin/api/v1/config` vhost section derived from the live `SharedVhostConfig`.
- Phase 04: the concurrency tail is NOT in this code (instance network allowance) — do not re-bisect locally.
- Fast link (`fast.<base>`): HTTPS only, never stored, label via `ReservedVhostLabel` (never a `VhostConfig` field), success = chunked terminator, two-task pump, `extract_host_from_head` parses the head only.

### Public `--udp`
- Off ⇒ byte-identical TCP relay. ONE server QUIC endpoint for vhost + public + jump; auth keys bare label / `port:<N>` / `jump:<alias>`. Per-connection round-robin across N QUIC conns, never per-datagram. TCP pool stays warm, fallback in place. Shared `spawn_direct`/`DirectPool` — do not fork. Hole-punch flags secret-only (warn on public).

### SSH jump host (`docs/plans/plan_SshJumpHost/`)
- One alias = one `Role::SshJumpHost` row; first-wins; per-entry `max_conns`; 15 s bounded provider open with carrier retry; admin API never exposes credentials; `--udp` reuses the shared endpoint (`jump:<alias>`), never another UDP socket; QUIC failure ⇒ same channel to warm TCP. Gate `T-SSH-JUMP`.

### Transfer
- Bounded manifests, `with_stall` pre-manifest, first-frame validation on worker accept, idempotent `commit_stage`, fsync before rename, `ProgressTracker` `Drop`, hashing in `hash_planned_entries` (never inline in `scan_entry`).

### VPN (Linux; macOS/Android twins)
- Relay queue applies backpressure (never drops); the ctrl actor is the single owner of the control stream; server heartbeat 500 ms; client beats only after the server proves it decodes them.
- Links start on relay; direct upgrade in background, retried on a 30 s grid (`DIRECT_RETRY_INTERVAL`); relay stays WARM, fallback in place (DEC-1/2); broker re-arms per round and clears listener candidates after each punch.
- ONE shared `Arc<AtomicU64>` nonce counter per egress key (I-5/DEC-6). Any replay window ≥ 2 × carriers × RELAY_QUEUE.
- `--carriers`/`--tun-queues` default 1 = byte-identical; carriers apply to both paths; DIRECT is FLOW-PINNED (`flow_carrier`), relay per-datagram RR. Full negotiated count or stay on relay.
- 1:1 uplink uses `send_batch_wait` (backpressure); the hub router keeps non-blocking `send_batch`.
- UDP socket buffers forced (`SO_*BUFFORCE`), warn on a surviving clamp. Punch sockets NEVER `SO_REUSEADDR`; drop the old socket before rebinding.
- `NetConfig` RAII revert + `stale_reclaim`; ip_forward refcount markers scoped by netns inode.
- **V-10:** `VPN_TUN_TXQUEUELEN` 128 (`BORE_VPN_TUN_TXQUEUELEN`). **V-12:** CC stays `bbr`. **V-13:** datagram send buffer 8 MiB, max 64 MiB. **V-15/V-19:** direct throughput stages print loss + NIC drops; transfer size must exceed the ramp (`XFER_MB`). **V-9:** qualify the link (P=1 vs P=8, `asym_qualify.sh`) before quoting absolute figures. **V-11:** `LC_ALL=C` for `sort -n`; print raw samples beside medians.
- MTU 1350 + MSS clamp; `--pin-mtu` observe-only; `TooLarge` = per-packet drop (typed `DatagramSend`); PMTU shrinks fast, grows on 3 samples, black-hole ceiling.
- Peer candidates inside tunneled subnets are dropped (`filter_tunneled_candidates`).

### VPN hub / NAT / FORWARD / ports
- Hub (`--max-clients N>1`): separate `run_listen_hub` branch (I-MC1); per-peer keys from the RAW `session_nonce` + own counter; router never restarts; nft spoke isolation; connector routes default-deny (`filter_accepted`). Trap: a spawn-only helper must not be an unawaited `async fn`.
- NAT E3 (`real@virtual`): no `@` ⇒ byte-identical masquerade; only the virtual CIDR on the wire; stateless kernel netmap; RAII + `stale_reclaim`.
- `--forward-accept`: iptables `filter` FORWARD custom chain `bore_<id>_fwd`; off ⇒ probe + warn. NAT'd subnets may also need `--nat-masquerade`.
- macOS (DEC-M1): Linux bodies byte-identical under `cfg(linux)`, macOS twins additive; utun + PF anchor `bore_vpn/<id>`; doc comment, then `#[allow]`, then `#[cfg]`; macos-14 CI is the only oracle.
- Android: twins + `crates/bore-android-tun`; HOST-ONLY (no advertise/NAT/forward/hub/multi-queue), enforced twice; toybox `ip route add` (no `replace`); `ip rule add to <subnet> lookup main priority 100` tolerating "File exists"; `run_dir()` = `/data/local/tmp`.

## SSH ingress gateway (`--ssh-gateway`; operational guide `docs/ssh-gateway/SSH_GATEWAY.md` §6)

Stock OpenSSH `-R`/`-L` → public/vhost/secret tunnels; SSH leg = TCP relay only (no UDP/carriers).
- D1 naming: numeric port ⇒ public; label + 80/443 ⇒ vhost; label + 0 ⇒ secret provider; `vhost/`/`secret/` prefixes override; `-L` to an id ⇒ secret consumer.
- I-SSH1: gateway off ⇒ control accept path byte-identical. I-SSH2/I-SSH8: inapplicable params always warn, never silent. I-SSH3: keepalive 1 s / reap 15 s; explicit `drop(state)` before any long-lived task tail (else `Arc<ConnState>` cycle). I-SSH4: SSH opener never writes `STREAM_READY`. I-SSH5: same-identity takeover only. I-SSH6: `shell_request` never closes nonzero. I-SSH7: info banners via `Handle::data`, never report client-local host:port; doc examples avoid `-N`. I-SSH9: post-TLS demux is ALPN-first (non-`ssh` ALPN is never SSH). I-SSH10: every server-initiated open bounded 15 s, 2 timeouts ⇒ `RunningSession::abort()`; vendored russh `WindowSizeRef.closed` (`crates/russh/HOL_FIX.md`); opener holds `Weak<ConnState>`. I-SSH11: `caller` never serialized on the mux wire; label caps 63/128. I-SSH12: `auth_rejection_time_initial = 0`, wrong credential stays 1 s.
- Tests: `tests/ssh_gateway_test.rs` + `sudo -n /abs/path/scripts/ssh_gateway_test.sh`.

## Version string

`bore <semver> - <branch> - <sha8>` — embedded at compile time via `build.rs` (`BORE_GIT_BRANCH`/`BORE_GIT_SHA` → `GITHUB_REF_NAME`/`GITHUB_SHA` → `git` CLI). Run `cargo build` to regenerate.

## Deployment / perf review (2026-11)

Staging server: `network_mode: host` + systemd `bore-443-redirect.service` (kernel `REDIRECT` tcp/443 → control port 7835, `--dst-type LOCAL`); rfw removed from the data path. Replication guide, measurements, open findings (allocator + yamux buffering prototype, secret `--udp` startup/DNS defects, review items): `OPTIMIZATION_NOVEMBER_2026.md`. Netns NAT harness needs a fast-failing resolver inside the namespaces (see that file, §8.5).
