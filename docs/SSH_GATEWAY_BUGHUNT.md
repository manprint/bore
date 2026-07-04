# SSH Gateway — Full Bug-Hunt Assessment (branch `ssh`)

> Severe bug-hunt + hardening pass over the `ssh-gateway` feature (public / vhost /
> secret tunnels served to stock `ssh -R`/`-L` clients). Date: 2026-07-04.
> Scope: `src/sshgw.rs`, `src/sshgw_auth.rs`, the `server.rs` demux/accept wiring,
> the `main.rs` CLI, and the two test suites (`tests/ssh_gateway_test.rs`,
> `scripts/ssh_gateway_test.sh`). Companion to `docs/SSH_GATEWAY.md`.

## Verdict

The feature is **fundamentally sound**: the demux/accept wiring is byte-identical when
disabled (I-SSH1), the zombie-entry reaper works against a real netfilter half-open
(T-SSH-N1), takeover survives a live partition (T-SSH-N3), auth hot-reloads by mtime,
and the RAII teardown discipline (guards + `ConnState::drop`) is correct. The
ref-cycle zombie bug that a real netns half-open exposed was already found and fixed
before this pass.

This pass found **5 real defects** (2 dead CLI-surface features, 1 performance cap
with no tuning knob, 1 dispatch-blocking stability risk, 1 uncancelable-forward
gap) plus test flakiness, and fixed all of them with zero regression to the native
(non-SSH) data paths.

## Findings & fixes

| # | Severity | Finding | Fix |
|---|----------|---------|-----|
| F1 | **Medium** | `--ssh-banner` was parsed and stored but **never wired to russh** — `GatewayHandler` had no `authentication_banner`, so the flag was a silent no-op. | Implemented `Handler::authentication_banner` returning the configured banner. Unit test `authentication_banner_reflects_config` + netns `T-SSH-N7`. |
| F2 | **Medium** | **Per-channel SSH window was russh's 2 MiB default with no tuning knob.** Each proxied connection is one SSH channel, throughput-capped at `window/RTT` (≈20 MiB/s @100 ms) — well under native yamux, whose per-stream window auto-tunes to the BDP. bore's whole selling point is throughput. | Raised the server window default to **16 MiB** and added `--ssh-window-size` / `BORE_SSH_WINDOW_SIZE` (clamped to ≥ one packet). Governs the *server-receives* direction (downloads — the dominant web case). Unit tests + docs §2.1. |
| F3 | **Medium** | `channel_open_direct_tcpip` (secret consumer `ssh -L`) awaited `open_with_failover` **inline on the russh dispatch loop, with no timeout**. russh dispatches handler callbacks sequentially, so a wedged-but-TCP-alive provider stalled the consumer's *entire* SSH session (keepalives + every other channel) until the provider's 60 s reaper fired. | Moved the provider open into the spawned relay task (off the dispatch loop) and bounded it with a 15 s `SSH_DIRECT_OPEN_TIMEOUT`. The channel is accepted and closed on failure — an `ssh -L` client sees an immediately-closed forwarded connection, the correct signal. |
| F4 | **Low** | `cancel-tcpip-forward` for a **vhost/secret forward requested with port 0** could silently no-op: the forward is registered under a synthesized placeholder port (RFC 4254 §7.1 echo-back), but the client's cancel may carry the original `0`, missing the exact-key lookup. Forward then lived until session teardown. | New pure `cancel_target` helper: exact `(address,port)` match for public (unchanged); port-agnostic address fallback for vhost/secret. Unit test `cancel_target_matches`. |
| F5 | **Low (contract)** | The `id=` exec param was parsed into a dead `Params.id` field and **silently ignored** (violates I-SSH2 "nothing silently dropped"). Docs claimed it set the vhost client-id. | Over SSH the tunnel identity IS the authenticated key/label — honoring a client `id=` would let any key claim another identity's reserved routes. Now emits a warning; dead field removed; docs corrected. |

### Documented, intentionally not code-changed

- **`--max-conns` permit accounting for the SSH *control* connection differs by
  path**: the dedicated `--ssh-port` listener consumes a permit for the control
  connection's lifetime; the shared control-port demux path does not (matching
  native bore control connections, which are likewise unmetered). The demux path
  is the primary deployment and is consistent with native; only the misleading
  code comment was corrected. Either way the semaphore still bounds proxied traffic.
- **`KeyStore::check` does synchronous filesystem I/O (holding a std `Mutex`) on the
  async auth path**, unlike `PasswordStore` which uses `spawn_blocking`. The key
  path has no argon2 CPU cost and touches only a small directory, so the blocking
  is sub-millisecond; noted for symmetry, not fixed.
- **`edge::accept` (TLS handshake / basic-auth) and `channel_open_forwarded_tcpip`
  are awaited inline in the public accept loop** — deliberately (the channel-open
  must stay inline for prompt liveness detection; see `run_public_forward`'s doc).
  This serializes *new-connection setup* per public tunnel but not per-connection
  bandwidth, and never blocks the russh dispatch loop (it is a per-forward task).

### Reviewed and cleared (not bugs)

- **Takeover races** (`peek_takeover` optimistic → `apply_takeover` authoritative):
  the only residual race is two brand-new registrations landing in the same instant
  — identical to the registry's own pre-existing vacant-insert race, and a different
  identity is rejected synchronously. Lock ordering (registry → owners) is
  consistent everywhere, so no deadlock. Guard `remove_if` token/ptr checks make an
  evicted guard's late drop a safe no-op.
- **`forwards`-map key collision** (two forwards colliding on `(address, port)`):
  a collision requires the same address string ⇒ the same registry name ⇒ that is
  exactly the same-identity takeover path, which aborts the incumbent via the
  *owners* map independently of the forwards map; a different identity is rejected
  before any insert. The dropped `JoinHandle` is therefore redundant, not a leak.
- **Reaper / connection liveness**: russh-native `keepalive_interval`/`keepalive_max`,
  with the fatal probe landing exactly at `SSH_CTRL_TIMEOUT` (unit-tested). Validated
  against a **real** netfilter half-open, not a process kill (T-SSH-N1).

## The 9 questions, answered

1. **Hidden bugs?** Yes — F1–F5 above, all fixed. No crash/panic/UB found
   (`#![forbid(unsafe_code)]` holds; no `unwrap` on attacker-controlled input).
2. **Stable / races / leaks?** Stable. No unbounded leak (the one real leak — a
   `ConnState` ref-cycle — was already fixed and is covered by T-SSH-N1). Races
   reviewed and either benign/pre-existing-scope or fixed (F3).
3. **SSH connection stable?** Yes — bidirectional keepalive + 60 s reaper, validated
   against a real half-open. F3 removes a dispatch-stall that could have cascaded a
   consumer-session reap.
4. **All exec/config flags implemented?** After this pass, yes: `notes`, `max-conns`,
   `basic-auth`, `webserver-log`, `https`, `force-https` work; `id=` now warns
   (identity is the SSH key); transport-only keys warn; `--ssh-banner` now works (F1).
5. **autossh / sshpass OK?** Yes — T-SSH-N2 (autossh reconnect across a server
   restart) and T-SSH-N6 (sshpass password auth + wrong-password reject). Their
   initial-echo checks were hardened with a retry loop (pre-existing flake, present
   on baseline too — not a code regression).
6. **Keys/passwords hot-read from files?** Yes — both stores re-stat on every auth
   attempt and re-parse only on mtime change (hot reload by construction), covered by
   `keystore_hot_reload_*` / `password_hot_reload`. argon2 verification is
   `spawn_blocking` + concurrency-capped.
7. **Default SSH params optimal for stability + performance?** Keepalive 20 s / reaper
   60 s are sound (parity with the native secret invariant). The window default was
   **not** optimal (2 MiB) → raised to 16 MiB (F2).
8. **Tuning env/flags for SSH params?** There were **none** for the throughput-
   critical window → added `--ssh-window-size` + `BORE_SSH_WINDOW_SIZE` (F2).
   Keepalive/reaper are intentionally fixed (the `keepalive_max` math is derived to
   land the fatal probe exactly at the reaper deadline; exposing it risks that
   invariant with little upside).
9. **Deterministic reconnect + takeover stable and tested?** Yes — same-identity
   takeover (`t_ssh_take1`, netns T-SSH-N3 under a live partition), different-identity
   rejected (`t_ssh_take2`), autossh reconnect (T-SSH-N2). F4 additionally makes an
   explicit mid-session `cancel` of a port-0 vhost/secret forward actually work.

## Test results

- `cargo test --features ssh-gateway --lib` — sshgw/sshgw_auth **42 passed, 0 failed**
  (incl. new `params_id_warns_not_silently_ignored`, `authentication_banner_reflects_config`,
  `russh_config_uses_configured_window_and_beats_russh_default`,
  `window_size_below_floor_is_clamped_up`, `cancel_target_matches`).
- `cargo test --features ssh-gateway --test ssh_gateway_test` — **21 passed, 0 failed**.
- `cargo test --features vpn,ssh-gateway --lib` — **403 passed, 0 failed** (full
  cross-feature regression: udp/vpn/secret/vhost/mux/sshgw).
- `cargo test` (default features) + doc-tests — green (zero regression to prior features).
- `cargo clippy --features ssh-gateway --all-targets` — clean; `cargo fmt` applied.
- `scripts/ssh_gateway_test.sh` (sudo netns chaos) — **11 PASS / 0 FAIL**; new
  `T-SSH-N7` (banner delivered) added; N2/N6 initial-echo hardened with a retry loop
  (their single-shot form flaked on the pre-bug-hunt baseline too — confirmed not a
  code regression by running the netns suite against stashed/original sources).
