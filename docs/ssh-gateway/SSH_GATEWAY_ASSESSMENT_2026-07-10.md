# SSH Gateway Production-Readiness Assessment — 2026-07-10

Full adversarial audit of the SSH ingress gateway (`--ssh-gateway`, `src/sshgw.rs` +
integration in `src/server.rs`/`src/mux.rs` + vendored `crates/russh`) across its three
modes (vhost / public / secret), hunting latent bugs, race conditions, deadlocks,
memory leaks, and optimization opportunities.

## Method

Five independent audit passes, each over the full 4000-line `src/sshgw.rs` plus its
integration surface, followed by orchestrator-level verification of every reported
finding against the actual code (no finding accepted on an auditor's word alone):

| Pass | Scope | Result |
|------|-------|--------|
| Concurrency | races, deadlocks, cancellation-safety, lock ordering, yamux/russh waker rules | **0 bugs** |
| Resource lifecycle | Arc cycles (I-SSH3 class), task/permit/listener leaks, unbounded growth | **0 real bugs** (1 style suspicion rejected on verify) |
| Protocol correctness | parse edge cases, splice integrity, RFC 4254 conformance, demux, auth | **2 real bugs** (fixed below) |
| Vendored russh delta | HOL fix (PR#730), close-wake, abort, window accounting | **0 bugs** |
| Performance | allocations, buffers, lock contention, socket tuning | 0 actionable (1 proposal rejected as harmful, see below) |

## Bugs found and fixed

### P1 — `forwarded-tcpip` originator fields lost on the vhost/secret SSH paths (MEDIUM)

`SshOpener::open` (src/sshgw.rs) hardcoded the RFC 4254 originator **port to `0`** and
only carried the originator **IP** when the forward had `webserver-log=on` (the
logging-gated `forward_ip` was the only input available). An SSH provider therefore saw
`0.0.0.0:0` / `<ip>:0` instead of the real visitor address. The public-tunnel inline
open (`run_public_forward`) already sent the true `ip:port` — vhost and secret were the
gap.

**Fix:** `mux::ChannelOpen::open` and `LinkOpener::open_ready` now take a second
parameter `caller: Option<SocketAddr>`:

- **Mux (native) path: byte-identical wire.** `caller` is never serialized; the
  `STREAM_READY` extension is still governed solely by `forward_ip`
  (`Some ⟺ client_wants_logging`), so native client interop and the access-log format
  (`real_ip` is a bare IP) are untouched.
- **SSH path:** `caller` fills the originator address AND port truthfully, regardless of
  any logging option. Fallback order: `caller` → logging-gated `forward_ip` (port 0) →
  `0.0.0.0:0`.
- Call sites threaded: vhost relay ×3 (`src/vhost.rs`), public/vhost server relay
  (`src/server.rs`), `secret::open_with_failover` (+`caller` param) — the native secret
  consumer relay passes the consumer's control-connection `peer` addr; the SSH
  secret-consumer `direct-tcpip` path passes the `-L` client's own originator fields
  (parsed best-effort; unparseable degrades to the placeholder, never fails the open).

Test: `link_open_ready_ssh_writes_no_marker` (mux.rs) extended to assert the caller
address is threaded and never written to the mux wire;
`link_open_ready_writes_single_zero_byte` asserts a `caller` produces zero extra bytes
on the native path.

### P2 — no length bound on vhost labels / secret ids (MEDIUM, DoS surface)

`validate_label` (src/sshgw.rs) checked charset only. A hostile `tcpip-forward` /
`direct-tcpip` could park ~32 KiB (russh packet-bounded) attacker-chosen strings per
request in the registries and the admin dashboard JSON.

**Fix:** `validate_label(label, max_len)`:
- vhost labels: **63 bytes** (`MAX_VHOST_LABEL_LEN`, the DNS single-label limit — a
  longer label could never match a real `Host:` header anyway);
- secret ids: **128 bytes** (`MAX_SECRET_ID_LEN`, looser so an SSH consumer can still
  name a long native `--tcp-secret-id`).

Applied on every entry point: `vhost/`/`secret/` prefixed specs, both bare-label
heuristic branches, and `parse_direct_tcpip_dest`. Test: `label_length_caps`
(boundary 63/64, 128/129, error-message shape, consumer-side symmetry).

## Findings rejected during verification

- **"Set SO_SNDBUF/SO_RCVBUF on accepted SSH sockets" (perf auditor) — REJECTED as
  harmful.** An explicit `setsockopt` is silently clamped to `net.core.{r,w}mem_max`
  (~208 KiB stock) AND disables kernel TCP buffer autotuning, which otherwise grows to
  `tcp_{r,w}mem[2]` (4–6 MiB) — i.e. the "fix" would *cap* throughput below the current
  autotuned ceiling on a stock host. The documented 164→668 Mbit/s aggregate gain
  (memory: ssh-gateway throughput assessment) came from raising the **sysctls**, which
  remains the correct operator-level remediation:
  `sysctl -w net.ipv4.tcp_rmem="4096 131072 16777216" net.ipv4.tcp_wmem="4096 16384 16777216"`.
  Code stays as-is (`tune_tcp` = TCP_NODELAY + SO_KEEPALIVE only).
- **"Missing `drop(state)` in the secret-consumer banner task" (lifecycle auditor) —
  REJECTED.** `state` is used on every loop iteration (`state.deliver(..)`), so the
  proposed early drop is impossible; the task is bounded by `lines.len()` and cannot
  produce the I-SSH3 pending-forever shape. Not a leak.
- **Demux 1–4-byte `Vec` allocations (perf) — NOT ACTED ON.** One-time per-connection
  setup cost, negligible vs. the TLS handshake on the same path; code churn not
  justified.

## Verified-clean list (highlights)

- No std/tokio lock held across `.await`; DashMap `entry()` used atomically (takeover).
- `session_channel` set **before** the pending-message drain; `deliver` can never fall
  in the gap (I-SSH7 ordering).
- Eviction (`ConnState::evict` → `RunningSession::abort`) vs. normal teardown: no
  double-free of registry entries/admin rows/listeners (token-checked `remove_if`
  guards).
- Every `--max-conns` permit path releases on open-timeout, `ChannelOpenFailure`,
  splice error, and mid-splice eviction.
- Vendored russh close-wake has no lost-wakeup (closed flag checked before parking;
  dual `notify_waiters`+`notify_one` covers late registrants); window accounting cannot
  underflow; `JoinHandle::abort` poisons no locks and always fires `ChannelRef::Drop`.
- I-SSH1 held: gateway off ⇒ accept path untouched by this change set.

## Gates (all green, 2026-07-10)

- `cargo fmt` ✓ — `cargo clippy --all-targets -D warnings` ✓ (default, `ssh-gateway`, `vpn`)
- `cargo test --features ssh-gateway`: **610/610** (incl. `tests/ssh_gateway_test.rs` 40/40)
- `cargo test` (default): **518/518**
- `scripts/ssh_gateway_test.sh` (netns chaos, sudo): see run log — required a fresh
  `cargo build --release --features vpn,ssh-gateway` first (harness stale-build guard).

## Verdict

The SSH gateway is production-ready. The two real defects found were
information-quality (P1) and abuse-surface (P2) issues — no crash, data-loss,
deadlock, race, or leak path survived adversarial verification across all five
dimensions. The hard-won invariants (I-SSH1…I-SSH10) all held under audit.
