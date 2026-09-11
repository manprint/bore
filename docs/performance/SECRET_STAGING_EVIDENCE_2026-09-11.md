# Secret tunnels — evidence, 2026-09-11

Campaign record for the SECRET tunnel path (`bore local --tcp-secret-id` +
`bore proxy`), the third of the three registries to get a full verification
campaign after vhost (2026-09-10) and public (2026-09-11). It covers the same
ground those two did — correctness gates, resource bounds, a real-network
performance A/B, and a race/leak/deadlock hunt — plus the one thing only the
secret path has: a **peer-to-peer** data path with NAT traversal and hole
punching, which is measured and optimised in its own right.

Two documents, deliberately: this one is the EVIDENCE (what was run, on what
build, with what numbers, and what was found), and
`final_secret_perf_review.md` is the review in Italian that reads it.

Everything here is reproducible from the repository. Nothing here contains a
credential: the staging harnesses read `~/.config/bore-perf/env.sh`, which is
provided separately and never committed.

---

## 0. Build provenance

| what | value |
| --- | --- |
| repository | `manprint/bore`, branch `main` |
| baseline commit | `582dec6` — Fase 6 asymmetric policy, `reason_code`, `plan_remedy` (CI green) |
| Fase 7 commit | `aff3343` — sprayed escape, mapping without a second STUN server (CI: 4/5 green, one Windows failure, see §2.2) |
| workstation | 16 threads, 47 GiB RAM, Linux 7.0.0-31-generic; `net.core.rmem_max = net.core.wmem_max = 4194304` |
| local binary | `bore 1.0.0 - main - <sha>`, `cargo build --release` |

A build's self-reported sha is checked by `tests/version_provenance_test.rs`;
provenance of a DEPLOYED artefact is verified by bytes (size + sha256 + mtime),
not by that string — see the staging runbook.

---

## 1. The local resource gate: `scripts/perf/secret_leak_hunt.sh`

`scripts/secret_netns_test.sh` (34 assertions, green on every run in this
campaign) proves the secret path is CORRECT: echo round-trips, no zombie admin
rows, relay/direct labelling, carrier accounting, conflict rejection, chaos
recovery. It does not prove the path is BOUNDED. A tunnel that answers every
request while leaking one descriptor or one allocation per proxied connection
is indistinguishable from a healthy one for the length of a test, and
distinguishable from it only in production — hours later, as `EMFILE` on every
listener of the process (P-12) or as an OOM kill.

So a second harness was written. It runs a server, a provider and a consumer
inside a ROOTLESS network namespace (`unshare -rn`, no sudo, no host port
touched) and measures process and kernel properties, which a single host
measures exactly: descriptors, resident memory, wedges, reaping, socket
buffers. Throughput is deliberately NOT measured there — that false-passes on
loopback and belongs to §4.

### 1.1 Why every churn verdict is a RATE and not a level

The first version sampled descriptors and RSS before and after one churn of 200
connections. It reported the provider and the consumer growing ~16 KiB per
connection, and at 20 connections per phase it reported ~64 KiB per connection —
a leak that got *smaller* the more connections it was given, which no leak does.

The arm now runs four identical phases and asserts the LAST one, printing the
whole series so the decay is visible rather than assumed. A leak is linear in
connections, so its per-phase delta is CONSTANT; warm-up and saturating caches
decay toward zero. Measured, consumer RSS delta per 200-connection phase:

```
phase1 +5508 KiB   phase2 +2152 KiB   phase3 +288 KiB   phase4 +440 KiB   phase5 +284 KiB
```

That is an allocator arena settling, not a leak, and one pair of samples could
not have told the difference. The same run moved **1 000 connections** through
the tunnel with **zero** descriptor growth on all three processes.

### 1.2 The arms

| arm | what it pins |
| --- | --- |
| `churn-relay` | 4 × 200 proxied connections over the server relay; last-phase descriptor and RSS growth on server, provider and consumer; all three still alive afterwards |
| `churn-direct` | the same over the QUIC direct path, which leaks differently by construction (a bidi stream per connection between the peers, plus a punch socket that is re-bound periodically) |
| `upgrade` | the consumer registers with NO provider present, so its first negotiation cannot succeed; then the provider starts and the live session upgrades itself. Asserts the upgrade happened, that the SERVER was told, that no stale `path_reason` survives, and that a round-trip still works across the swap |
| `udpbuf` | P-13 on the secret path: `rb`/`tb` of the live punch socket read from `ss -uapm` — the kernel's view, never the log's (P-12's rule) |
| `stall` / `stall-direct` | the deadlock arm on both transports: 16 connections held open with no bytes moving, six overlapping bulk waves each SIGKILLed mid-flight, then three fresh round-trips timed; the admin API polled in the same breath; on a stall it dumps every thread's backtrace with `gdb` |
| `reap` | the control-liveness reaper against a SIGSTOPped consumer (wedged but TCP-alive — the shape `send`/`recv` cannot see), asserting both the row and the descriptors come back |

The direct arms need candidates that are not loopback, because loopback is not
a routable hole-punch candidate. They build one on a `dummy` interface inside
the namespace and pin BOTH ends with `--udp-candidate` on a fixed
`--nat-udp-preferred-port`, STUN off. That is deliberate: the harness is not
testing traversal (`scripts/udp_nat_netns_test.sh` is, against real nftables
NATs), it is testing what the direct path COSTS once it exists — so a `relay`
reading there means the direct path broke, never that discovery was slow.

### 1.3 Results

All arms green on the fixed tree:

```
T-SECLEAK-CHURN-RELAY    10/0   1000 conns, fd +0/+0/+0, last-phase RSS -1056/+256/+284 KiB
T-SECLEAK-CHURN-DIRECT   11/0    800 conns direct, fd +0/+0/+0, server RSS delta exactly 0
T-SECLEAK-UPGRADE         6/0   current_path=direct 1 s after the provider appeared
T-SECLEAK-UDPBUF          4/0   rb=tb=8388608 on both punch sockets (rmem_default 212992)
T-SECLEAK-STALL           4/0   worst fresh round-trip 63 ms behind 16 held connections
T-SECLEAK-STALL-DIRECT    5/0   worst fresh round-trip 41 ms, same load, direct transport
T-SECLEAK-REAP            3/0   row released after 59 s (60 s timeout), server fd 15 -> 14
```

Two readings worth naming. The `churn-direct` server RSS delta is **exactly
zero**, which is the strongest available confirmation that the path really was
direct: the server is not on it and therefore cannot allocate for it. And
`udpbuf` reads 8 MiB against a host `rmem_max` of 4 MiB, i.e. `SO_*BUFFORCE`
succeeded — inside `unshare -rn` the process holds `CAP_NET_ADMIN` in its own
user namespace, which is exactly the privileged case the clamp warning
describes.

---

## 2. Defects found

### 2.1 S-2 — a live relay→direct upgrade was never reported, so `current_path` lied

**Severity: medium (observability, on the one field an operator uses to decide
whether traversal works).** Found by the `upgrade` arm; not reachable by any
in-process test, because it needs a real negotiation to lose a real race.

A secret tunnel's transport reaches the admin API through exactly one channel:
the consumer's own `ClientMessage::SecretPathReport` (S-1 — the direct path runs
consumer↔provider, so the server is not on it and cannot look). That report was
sent ONCE, right after the registration-time negotiation.

Now consider the ordinary order of events for a pair that comes up together:
the consumer registers first, the server has no UDP-capable provider yet and
answers `UdpUnavailable`, the consumer settles on the relay — and then upgrades
itself a few seconds later on its retry backoff, in place, exactly as designed.
Nobody tells the server. Measured on the workstation: the consumer logged
`path=direct-udp` on 200 consecutive connections while `/admin/api/v1/secret`
answered:

```
current_path = relay
path_reason  = "no udp-capable provider registered"
```

for the rest of the session. This is the P-10 shape again — a path field that
cannot express what is happening is worse than an absent one, because an
operator reads it as the truth — and it is worse than P-10 was, because the
stale row also carries a stale EXPLANATION of a failure that no longer applies.

There were two halves to it:

1. **The report was not re-sent on upgrade.** `Proxy::path_report` now retains
   the server's declared capability past registration, and the upgrade arm
   sends a fresh `SecretPathReport { path: "direct", reason: None }` — the
   reason is cleared on purpose, since the field explains why the path is what
   it IS.
2. **The capability could not even be known.** `path_report` rides on
   `ServerMessage::UdpPunch` and on nothing else, so a consumer answered
   `UdpUnavailable` at registration never learned that its server accepts
   reports, and could not have reported the upgrade it was about to complete.
   It is now learned, monotonically, from whichever `UdpPunch` arrives — which
   is by definition the one that makes the upgrade possible.

The upgrade's write is `timeout`-bounded (`ctrl_heartbeat_send_timeout`, 10 s)
because it lives in a `select!` arm: that is P-9's exact shape, where an
unbounded `send` against a peer that has stopped reading its half of the
substream parks forever and the consumer stops accepting connections while
still looking perfectly registered. Observability must never cost the data
path, so a timeout there is a `debug!` and nothing more.

The server's own provisional verdict in `broker_secret_udp` stays as it was —
it is the one refusing, so it is entitled to write the row — and the consumer's
later report overwrites it. The reverse direction (direct→relay) needs nothing:
it tears the session down and reconnects, which re-reports at registration.

It also mattered for THIS campaign specifically, and that is worth stating
plainly: `sec_time_to_direct` in `scripts/perf/staging/sec/seclib.sh` — the
primitive behind stage S2, the punch success rate and the time-to-direct
distribution — polls exactly this field. Run against the pre-fix build, every
tunnel whose first negotiation lost the race would have been recorded as
`never`, and the campaign's headline traversal number would have been a
measurement of the reporting bug rather than of the NAT. A harness that reads a
field is only as good as the field.

Red-check, captured before the fix:

```
FAIL: T-SECLEAK-UPGRADE/upgrade-reported: the admin API still reports relay while the data path moved
FAIL: T-SECLEAK-UPGRADE/upgrade-reason-cleared: the row still explains the path with: no udp-capable provider registered
```

and after:

```
MEASURE upgrade current_path=direct after=1s
MEASURE upgrade path_reason=""
PASS: T-SECLEAK-UPGRADE/upgrade-reported: the admin API reports direct 1s after the upgrade became possible
```

### 2.2 S-3 — the sprayed escape abandoned its socket on the first ICMP error (Windows)

**Severity: high on Windows, none on Linux.** Found by CI on commit `aff3343`:
`the_sprayed_escape_rendezvous_finds_a_pair` passed on every Linux runner and
failed on `windows-latest` and on the `x86_64-pc-windows-msvc` cross check with
*"the easy side found no pair"*.

The escape sprays hundreds of destination ports of which at most one is open,
so by construction almost every packet earns an ICMP port-unreachable. The
question is where that error surfaces. Windows delivers it **on the sending
socket**, as `WSAECONNRESET` on the next `recv_from`; Linux does the same with
`ECONNREFUSED`, but only for a CONNECTED socket — and an unconnected UDP
socket, which every spray socket is, is never told about ICMP errors at all.

So the one datagram that proves the escape worked arrives on a socket whose
receive queue is full of errors caused by the escape's own probes, and both
spray loops treated `Ok(Err(_))` as the end of the escape. On Windows the first
returning ICMP closed the round before any answer could be read.

The fix follows a precedent that was already in the same module:
`recv_actor`, the single reader of the traversal socket, whose comment says
exactly why it does not die on a recv error. Both spray loops now sleep 5 ms
and continue, and the deadline (`cap`) is what ends them — the pause is there so
a burst of already-queued errors cannot become a busy-spin.

The operational rule that follows, for any future loop on a punch socket: **the
only error that may end a round is the deadline**, never a per-datagram error.
And the oracle for this class of defect is the `windows-latest` CI job, because
on Linux the defect is irreproducible as a matter of kernel behaviour — the
same standing rule already in force for the macOS backend.

### 2.3 S-4 — the sprayed-escape test was shooting the other tests (CI flake risk)

**Severity: none in production, high for the gates.** `--lib` failed roughly one
run in eight under load, always on
`holepunch::tests::checks_never_answer_unauthenticated_probes`, and always on
the assertion that a forged probe earns **silence**.

The cause is the Fase 7 test itself. `the_sprayed_escape_rendezvous_finds_a_pair`
fires up to 12 000 frames at randomly drawn LOOPBACK ports, in the same process
as every other unit test, and the auth test's prober socket holds one ephemeral
loopback port — so with probability ≈ 6 000/64 512 per pass it eventually gets
sprayed. The auth test then saw a datagram it had not asked for and declared a
security regression.

The assertion was wrong, not the spray. "Answered" means *a frame this key
authenticates*, not *a datagram arrived* — which is exactly the rule the
production code already follows for benign punch strays (they are `debug`, and
authentication is the gate; BUG-S3). Both reads in that test now skip anything
`check::parse` rejects under its own key, and the positive read is bounded
generously (5 s) while the negative one stays tight (300 ms): a violation would
be immediate, so a short bound costs nothing, whereas a tightly-bounded POSITIVE
assertion measures the machine rather than the code.

Verified 10/10 green afterwards, under the same load that produced the failure.

---
