# Phase 07 — HTTP/2 on the vhost edge: spike only, then decide

> **Motivating candidate:** 6 (HTTP/2/3 on the browser side) — the strongest
> *latency* lever for remote browsers, and by far the largest piece of work
> **Kind:** measurement and feasibility. **No implementation in this phase.**
> **Prerequisite:** none (independent of every other phase)

## Why a spike and not an implementation

The prize is real but it is **the client's RTT, not the server's work**, and the
campaign measured exactly how the 45 ms splits:

- Connection setup measured **+45 ms** per new connection from a 21 ms-RTT
  client, but only **+4.9 ms** at 1.84 ms RTT (§2.15 A3: keep-alive p50 2.6 ms
  versus new-connection 7.4 ms, identical on both transports).
- So **~4.9 ms is server time and ~40 ms is the client's TLS round trips.** The
  prize scales with the viewer's RTT, which the server does not control.
- A browser opens ~6 connections per origin, so a 30-asset page pays roughly
  **5 waves × 45 ms** of pure setup at 21 ms RTT, plus per-connection
  head-of-line. h2 collapses that to one connection with N streams.

That arithmetic is an estimate built from a microbenchmark, not a measured page
load. Committing the largest engineering item in the plan to an estimate is the
mistake this phase exists to avoid — especially since the same campaign
retracted four findings that turned out to be measurement artifacts.

The work is also larger than "enable ALPN h2": the tunnel speaks HTTP/1.1 to
the provider and the provider is a deliberately blind byte pipe, so this is an
**h2 ↔ h1 gateway** with request/response multiplexing, per-stream flow control
mapped onto the tunnel, and interaction with the injected-header path.

## What already exists in our favour

The ALPN plumbing is **already there** and already sees `h2` offered. From
`CLAUDE.md` (I-SSH9):

> demux consults the ClientHello ALPN offer FIRST
> (`sshgw::accept_tls_with_alpn` via `LazyConfigAcceptor` +
> `demux_classify_alpn`): any ALPN ≠ `ssh` (browsers `h2`/`http/1.1`, native
> bore `bore`) ⇒ NEVER SSH — routed via `route_connection_known_http`

So browsers are already offering `h2`, the server already classifies it, and it
is already routed to the HTTP path — which then speaks HTTP/1.1. The graft point
is identified and the risky demux work is done. This materially reduces the
estimate and is worth verifying rather than assuming.

---

## 7.1 — Measure the real prize

Build a realistic page — one large asset plus ~30 small ones, the F-15 shape —
and measure full page load through a vhost tunnel from a client at controlled
RTT: **~2 ms** (the same-region VM), **~21 ms** (the workstation), and **60 ms
and 100 ms** synthesized with netem.

Compare, at each RTT:

- HTTP/1.1 through vhost today
- HTTP/2 direct to the origin, bypassing the tunnel entirely — the **upper
  bound** on what an h2 vhost edge could ever deliver

The gap between those two curves is the actual prize, per RTT. Use protocol-
selective netem on the *consumer* leg (`u32 match ip dst X/32 match ip protocol
6 0xff`) and mind the `tc` argument order (§9.6 pitfall 5). Report a full page
load, not a per-request latency — the whole point is the wave structure, which
per-request percentiles hide.

**Deliverable:** a table of page-load time versus RTT with the h2 headroom at
each, appended to the evidence document as a new subsection.

---

## 7.2 — Verify the graft

Read-only investigation, no implementation:

- Confirm `route_connection_known_http` is the single seam where an h2 session
  would be accepted, and that the ALPN offer already reaches it intact.
- Identify what an h2 server needs from the tunnel: N concurrent request/response
  pairs over one client connection, mapped onto proxied connections or onto
  yamux substreams. Note which existing invariant each option collides with —
  in particular the **one stream = one task** yamux waker rule and the
  single-task `split` + `try_join!` shape the injected-response path requires.
- Identify the interaction with the response-header injection path: h2 headers
  are HPACK-encoded, so `relay_response_injected`'s byte-level head rewriting
  does not apply as written. This is the part most likely to be underestimated.
- Decide whether the origin leg stays HTTP/1.1 (almost certainly yes — it is
  usually localhost, where §2.15 measured setup at 4.9 ms, so pooling buys
  little; that is also original candidate 3, listed as *not tested*).

**Deliverable:** a written effort estimate broken into subphases, with the
invariant collisions named.

---

## 7.3 — Go / no-go

Decide with 7.1 and 7.2 in hand. Suggested criteria, to be confirmed by the
operator rather than assumed:

- **Go** if the measured page-load headroom at 60–100 ms RTT is large (a
  substantial fraction of total page time) **and** 7.2 finds no collision with
  the yamux single-task or injected-flush invariants that requires reworking
  them. Those two invariants each have a bug and a fix behind them
  (`[[yamux-stream-split-wedge]]`, `docs/VHOST_INJECTED_FLUSH_FIX.md`); an h2
  design that needs them relaxed is a much larger and riskier piece of work than
  one that sits alongside them.
- **No-go, or defer** if the headroom is modest at realistic RTTs, or if the
  audience is mostly low-RTT. Record the measurement either way so the question
  is not re-asked from zero.

**HTTP/3 is explicitly out of scope** for this phase. It would ride the QUIC
path, whose measured ceiling is 0.96 Gbit/s at 2.5× the CPU per byte (F-16),
and h2 over the existing TLS edge captures the setup-latency prize without
that. Revisit only if h2 ships and proves the prize real.

## Phase acceptance

A written recommendation with numbers, appended to the evidence document, plus
either an approved subphase plan or a recorded no-go with its measurement. **No
production code lands in this phase.**
