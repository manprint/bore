# UDP Direct-Path Flap — Evidence

## Symptom
VPN direct (QUIC) path establishes, then dies ≈ +22 s, falls back to relay, and
re-punches on the 30 s grid — endless flap. Reproduced by the user **only when a
secret `--udp` tunnel runs concurrently with the VPN on the same host**. Server
logs show the VPN (`id=ciao`) *and* the secret (`id=dufs`) each re-punching every
~30 s. Both punch from `udp_local_addr=Some(0.0.0.0:443)`.

## Root cause — confirmed
Concurrent direct-path tunnels bind the **same local UDP port** (here 443, via
`--nat-udp-preferred-port` / a shared default). `holepunch::bind_socket`
(`src/holepunch.rs:118-120`) sets `SO_REUSEADDR` for any non-zero port. With
`SO_REUSEADDR` set on both sockets, the kernel lets **both** bind the same
wildcard `0.0.0.0:port` and delivers inbound datagrams to the **last-bound**
socket. Each tunnel re-binds a fresh socket on every direct retry (VPN
`DIRECT_RETRY_INTERVAL` 30 s at `vpn.rs:1041`; secret upgrade retry at
`secret.rs:1502`), so whoever binds last **steals** the live peer's inbound QUIC
→ that connection idle-times-out (10 s) → re-punches → steals the port back →
**mutual lockstep ~30 s flap**.

The tunnels are **separate processes** (`bore vpn connect` vs `bore proxy`), so
this is a **cross-process** collision — an in-process registry alone cannot fix
it.

## Kernel probe (deterministic, no netns)
`docs/plans/udp_flap/udp_reuse_probe.py` on kernel `7.0.0-14-generic`:

```
A=REUSEADDR, B=REUSEADDR (current bore behavior)
  both A and B bound 0.0.0.0:54545 simultaneously
  A received: []
  B received: ['pkt0'..'pkt4']
  => delivered to B (LAST binder) => STEAL confirmed.

A=REUSEADDR, B=plain (no opts)
  socket B bind FAILED (EADDRINUSE) => second tunnel CANNOT steal; ephemeral fallback. GOOD.

A=REUSEADDR, B=REUSEPORT
  socket B bind FAILED (EADDRINUSE) => GOOD.
```

Conclusion: the `SO_REUSEADDR` on the punch socket is the enabler. Without it the
second binder is cleanly refused (EADDRINUSE) and can fall back to an ephemeral
port instead of stealing.

## Ruled out (do not re-investigate)
- Direct QUIC path code byte-identical since pre-frontend `3a5c87b`; keepalive
  3 s / idle 10 s correctly applied to all carriers incl `open_sibling`.
- Earlier MTU-grow (1350→1414 at +10 s) theory: **coincidental**, not the killer.
- Admin 30 s poll on shared control port 7835: harmless read-only snapshot.
- Shared server `udp_providers` registry: namespaced keys (`vpn:{id}` vs `id`),
  safe.

## Fix (chosen)
Do not let `SO_REUSEADDR` enable a cross-tunnel steal. Bind the preferred port
**without** `SO_REUSEADDR`; on `EADDRINUSE` fall back to an **ephemeral port
(`:0`)** and `warn!` naming the contended port. UDP has no TIME_WAIT, so a
same-tunnel `--auto-reconnect` rebind still succeeds once its previous socket is
dropped (drop-then-bind ordering) — verify in Phase 1. Optional in-process port
registry only as a nicety for clearer same-process diagnostics.
