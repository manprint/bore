# Phase 3 — Hardening, observability and admin surface

> **Planning model:** GPT-5/Codex
> **Intent:** make the TCP feature operable under reconnects, abuse and production
> monitoring before adding QUIC.
> **Precondition:** Phase 2 TCP vertical slice green and owner approval.

## 3.1 Connection bounds and timeout discipline

- Make a per-entry semaphore the real `--max-conns` bound; one permit per live
  `direct-tcpip` channel, released on every error/cancel/drop path.
- Bound provider stream open with the existing SSH open/provider timeout policy;
  never block russh's sequential handler dispatch loop.
- Validate native carrier caps, alias lengths, notes, port and registry counts before
  allocation. Add rejection counters without high-cardinality metrics labels.
- Ensure a wedged provider fails one connection promptly without wedging other
  channels on the same outer SSH session.

## 3.2 Reconnect/collision behavior

- Keep v1 duplicate semantics explicit: a native provider's shared bore secret is
  not an identity, so first registration wins and reconnect retries until clean
  teardown/reaper. A pure-SSH provider is owned by its classic username:
  same-username reconnect can take over; different username and cross-transport
  collisions reject.
- Test clean SIGTERM, process kill, TCP half-open/netfilter drop, server restart,
  carrier death between pick/open, and reconnect storms.
- Assert one logical alias always produces one provider row regardless of
  carriers and that stale RAII guards cannot remove a replacement entry.

## 3.3 Admin API/dashboard

- Add `Role::SshJumpHost` and a dedicated view/panel or clearly separated Jump
  Hosts section; do not inflate Secret Tunnels or Vhost counts.
- Expose only sanitized operational fields: hostname, SSH port, peer, uptime,
  notes, requested/effective carriers, UDP requested/active, active connections,
  relay/direct byte counters and local target metadata already intentionally
  exposed by current tunnel views.
- Never expose `BORE_SECRET`, password hashes, key material or
  target account names.
- Add server config parity for base domain, classic-auth requirement, direct QUIC
  port and feature status.
- Update frontend tests for rendering/escaping and overview counts.

## 3.4 Audit logging

- Structured logs for allow/deny/open/close with outer peer, classic username, alias,
  requested port, provider type (`native`/`ssh`) and selected path; never log
  credentials. Native registrations have no classic username; report their
  owner class as shared-secret, not a fabricated account.
- Denial messages visible to the SSH client are intentionally generic; detailed
  policy reason remains server-side.
- Add bounded/rate-limited logging for repeated jump username mismatches.

## Phase 3 gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features admin
cargo test --all-features --test ssh_jump_test
cargo test --all-features
```

Run the dedicated SSH gateway netns/chaos harness only after rebuilding the
release binary, never concurrently with another netns harness. Phase done means
zero leaked permits/rows across every failure path and full admin/frontend parity.
