# Concurrent Mixed-Tunnel Stability — Assessment & Fix

**Date:** 2026-06-21 · **Branch:** webserver-log · **Status:** fixed (uncommitted)

## Symptom (field report)

A VPN direct (QUIC) path came up, then dropped on a ~30 s cadence forever:
`bridge switched to direct path` → (~+10 s) `tun MTU adjusted 1350→1414` →
(~+22 s) `direct path lost; fell back to relay` → re-punch on the 30 s grid →
repeat. The user reproduced it deterministically: **the flap happens only when a
secret `--udp` tunnel runs concurrently with the VPN on the same host.** Server
logs showed the VPN (`id=ciao`) AND a secret tunnel (`id=dufs`) each re-punching
every ~30 s; both punched from `udp_local_addr=Some(0.0.0.0:443)`.

## Root cause

Concurrent direct-path tunnels bound the **same local UDP port** (443, via
`--nat-udp-preferred-port`). `holepunch::bind_socket` set `SO_REUSEADDR` for any
fixed port. With `SO_REUSEADDR` on both sockets the kernel lets both bind the same
wildcard `0.0.0.0:443` and **delivers inbound datagrams to the last-bound
socket**. Each tunnel re-binds a fresh socket on every direct retry (VPN
`DIRECT_RETRY_INTERVAL` 30 s; secret upgrade retry), so whoever binds last
**steals** the live peer's inbound QUIC → that connection idle-times-out (10 s) →
re-punches → steals the port back → **mutual lockstep flap**. The two tunnels are
**separate processes** (`bore vpn connect` vs `bore proxy`), so it is a
cross-process collision.

### Deterministic kernel proof (no netns)

`docs/plans/udp_flap/udp_reuse_probe.py` on kernel `7.0.0-14-generic`:

| Both sockets | Result |
|---|---|
| `REUSEADDR + REUSEADDR` (old bore) | co-bind; inbound → **last binder = STEAL** |
| `REUSEADDR + plain` | 2nd bind → `EADDRINUSE` (clean refuse) |
| `REUSEADDR + REUSEPORT` | 2nd bind → `EADDRINUSE` |

### Ruled out

- Direct QUIC code byte-identical since pre-frontend `3a5c87b`; keepalive 3 s /
  idle 10 s correctly applied to all carriers incl `open_sibling`.
- The +10 s `1350→1414` MTU grow was **coincidental**, not the killer (it still
  appears in the fixed-and-stable run).
- Admin 30 s poll on the shared control port 7835: harmless read-only snapshot.
- Shared server `udp_providers` registry: namespaced keys (`vpn:{id}` vs `id`).

## Fix

`src/holepunch.rs` `bind_socket`: **do not set `SO_REUSEADDR`**. Bind the
preferred port; on `EADDRINUSE` fall back to an **ephemeral port** and `warn!`
naming the contended port. The first tunnel keeps the firewall-friendly port;
later ones get their own ephemeral port (STUN rediscovers the reflexive port and
they still punch) or stay on relay behind a strict egress firewall. UDP has no
TIME_WAIT, so a same-tunnel `--auto-reconnect` still rebinds the fixed port once
its previous socket has dropped — the direct-upgrade retry only fires after a
fall-back (old socket already dropped), so there is no old/new overlap (audited:
`vpn.rs` `try_direct_upgrade` binds per attempt; `secret.rs` upgrade retry same).

No in-process registry was needed — the OS arbitrates cross-process via
`EADDRINUSE`.

## Tests

- **Unit** (`holepunch.rs`): `bind_socket_ephemeral_gets_a_port`,
  `bind_socket_fixed_port_collision_falls_back_to_ephemeral` (the regression),
  `bind_socket_free_fixed_port_is_honored`.
- **e2e** (`scripts/vpn_netns_test.sh`): `T-STRESS-PORTCLASH` — a VPN connector +
  a secret consumer pinned to the same `--nat-udp-preferred-port` in one netns;
  RED before (switched=2, lost=2), GREEN after (switched=1, lost=0).
  `T-STRESS-MIX` — many concurrent tunnels of all types (public, vhost, secret,
  VPN 1:1 + 1:N with routes/masquerade/forward) held stable for a window; the
  never-before-existed mixed-load stability oracle.
- Gates: `cargo fmt`, `cargo clippy --features vpn,udp -D warnings`,
  `cargo test --features vpn,udp` — all green, zero regressions.

## Invariant added

CLAUDE.md: *"Direct-path UDP punch sockets must NEVER set `SO_REUSEADDR`"* (full
text in CLAUDE.md, holepunch family). Plus the standing rule that all concurrency
across tunnel types is now covered by the `T-STRESS-*` mixed-load harness.
