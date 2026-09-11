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
## 3. The staging deployment used for §4 onwards

| actor | role | what runs there |
|---|---|---|
| server host (t4g.micro, eu-south-1) | broker only | `ghcr.io/manprint/bore:main`, image `bore 1.0.0 - main - 1bb3243a`, restarted 20:01 UTC |
| test VM (same region) | provider or consumer, per topology | `~/bore` = `1bb3243a`, sha256 `f68d74ca…6723` |
| workstation (home, Italy) | the other peer | `target/release/bore` built from the same commit |

Coordinates and credentials live outside the repository (`~/.config/bore-perf/env.sh`,
`chmod 600`); every script reads them from there and none hardcodes a host.

The server image matters for exactly one thing: `current_path` / `path_reason` /
`direct_fallbacks` for a SECRET tunnel come only from `ClientMessage::SecretPathReport`
(S-1), and the capability that makes the consumer send it rides on
`ServerMessage::UdpPunch` (S-2). Against the previous image every arm of the
campaign read `unknown`, which is why the deployment was refreshed before any
number below was taken.

### 3.1 The harness defect that had to be fixed first (H-12 again)

Each stage used to carry its own `start_origin`, and each decided whether the
origin was already up with a REMOTE `pgrep -f "raw_origin.py $RP"`. That check
can never answer "no": the `bash -c` ssh spawns to run it carries the pattern in
its own command line and matches itself. The origin was therefore never started,
the provider logged `could not connect to localhost:5053` for every connection,
and S1 measured **0.00 MB/s on both arms** — a number shaped exactly like a
transport result.

The existence check that cannot lie about a listener is a TCP connect, so
`sec_start_origin` (now the single copy, in `seclib.sh`) uses
`</dev/tcp/127.0.0.1/$RP` and keeps the `sleep 1; true` tail that lets the
detached `setsid nohup` survive the ssh session teardown, followed by a 40×0.25 s
readiness loop that fails the stage loudly instead of measuring nothing.

### 3.2 Two more harness defects, both found by running it (H-13, H-14)

**H-13 — a failed arm entered the median as the ratio 0.** `paired()` in
`sec_ab.sh` pushed `ratio(direct, relay)` unconditionally, and `one_arm`
reports a start failure as the rate `0` with the path `startfail`. Zero is a
legal ratio, so the failure did not stand out; it just pulled the median toward
the bottom of the list. The `s1-vm-ws` `get` stage printed

```
  median ratio direct/relay: 0.965
```

with five rows of which two were `startfail`. The three arms that actually ran
were 0.965 / 1.247 / 1.218 — a true median of **1.218**, i.e. the direct path
winning by 22 %, reported as the direct path losing by 3.5 %. The defect is
therefore not "noisy": it is biased in one direction, because a zero can only
ever be the smallest element.

The fix keeps the row and drops the pair: `arm_is_measurement` rejects
`startfail` / `noprovider` / `noconsumer` **and** a `--udp` arm whose path came
back `relay(fb=N)` (that arm measured the relay against the relay, so its
ratio sits near 1.0 and would report a traversal failure as a performance
result). The excluded row is still printed, with `-` in the ratio column, plus
an explicit `EXCLUDED n of N pairs` line and `n=` beside the median. An
exclusion that is not visible is just a second way to hide a failure.

**H-14 — a non-integer `GIB` produced a real-looking row.** `sec_eff.sh`
computes `PER=$(( GIB * 1073741824 / CONNS ))`. Shell arithmetic on `0.25`
does not abort the script: it prints a syntax error to stderr, leaves `PER`
unset, and every arm then transfers whatever `$PER` expands to — 1 MiB, in
under a second. The stage printed a row with an empty MB/s field and `t0 ==
t1`, which reduces to a division by zero downstream. Both `GIB` and `CONNS`
are now validated as positive integers with `exit 2`, at the top of the
script, before anything is started.

### 3.3 H-15 — the efficiency stage opened its window before the path existed, and S-5 ate the arm

This one is worth stating in full, because it is the clearest field cost of
S-5 in the whole campaign: the defect did not merely make a tunnel slow, it
**destroyed a measurement**.

`sec_eff.sh` started its 2 GiB window straight after the warm-up connection.
`sec_ab.sh` did not — it waits for `current_path == direct` first — and the
difference had never mattered, because the direct path normally comes up in
~40 ms and the warm-up covers it. On the `vm-ws` stage of 2026-09-11 it did
not:

```
consumer (workstation, Dialer)
  21:40:29.684  connectivity-check round finished role=Dialer nominated=Some(<vm-ip>:56698) checks_ms=205
  21:40:29.684  consumer punching UDP peer candidates nominated=Some(<vm-ip>:56698)
  (log ends here — the process is killed at the end of the arm)

provider (test VM, Listener)
  21:40:30.591  connectivity-check round finished role=Listener nominated=None checks_ms=1126
  21:40:30.592  provider check round finished; QUIC listener up
  21:40:30.592  direct udp path ready, accepting connections
```

The dialer nominated at `.684` and began dialing an endpoint that did not
exist until `.592` of the following second — **908 ms** during which the
Initial had nowhere to land. The stage printed

```
  direct       0.00 MB/s  path=unknown fb=0   relay_tx=0  relay_rx=0  window=1789162829-1789162830 gib=2
```

a row with a rate, a path and a window, none of which describe a transfer.

Two fixes, and they are independent:

* the PRODUCT fix is S-5 itself (§4), which removes the 908 ms;
* the HARNESS fix is that a stage must never open a window on a path it has
  not confirmed. `sec_eff.sh` now waits with `sec_time_to_direct` exactly as
  `sec_ab.sh` does, prints the `ttd` beside the rate, and marks any arm whose
  observed path is not the arm's own name — or whose window is empty — as
  `INVALID`, returning non-zero so the stage says out loud that its windows
  must not be reduced. A CPU-per-GiB figure computed from that window would
  have been attributed to the direct path while describing nothing at all.

The `vm-vm` arm of the same stage is unaffected and is the positive control
for §8: `relay 204.03 MB/s` with `relay_tx=2148532224` (the server carried
every byte) against `direct 407.83 MB/s` with `relay_tx=0` (it carried none).

### 3.4 H-16 — the leak gate decided on a coin toss

`secret_leak_hunt.sh`'s churn arm judged RSS on the LAST phase's delta against
1024 KiB, on the stated assumption that "by the last phase the arenas have
stopped moving". Its own data falsified that:

```
T-SECLEAK-CHURN-DIRECT
  warm   rss=…/19076/18148
  phase1 drss=0/2580/6056
  phase2 drss=0/1576/1156
  phase3 drss=0/540/-1136
  phase4 drss=0/-12/1132     <- FAIL: rss-consumer +1132 KiB
```

A leak cannot produce a **negative** phase, and phase 3 is −1136 KiB against
phase 4's +1132 with the provider flat at −12 and every descriptor count at
+0. That is glibc's arena taking and returning a ~1.1 MiB chunk; a
single-phase verdict lets whichever phase the swing lands in decide the
result. The gate was not detecting a leak, it was sampling a sawtooth.

Rewritten as two statements. **Magnitude**: the last phase against
`RSS_PHASE_SLACK` 2048 KiB — ~10 KiB per connection, still 25× below the
smallest retention this path can physically have (one proxy buffer, 256 KiB)
and ~2× above the measured arena swing. **Trend**: a leak is linear in
connections, so it is positive in *every* phase; the arm fails when RSS rose
in every phase after the first AND the total rise exceeds one phase's slack.
The trend check gives back — and exceeds — the sensitivity the wider magnitude
bound gave up: a steady 3.5 KiB/connection drift fails on the trend while
every individual phase of it sits under the magnitude bound, which is exactly
the shape the old gate would have passed four times in a row.

That 3.5 KiB/connection is the instrument's real resolution at 4 phases × 200
connections, and the floor is set by data rather than taste: the HEALTHY relay
server rose in all three trend phases for 1088 KiB total, i.e. 1.8 KiB per
connection. Any bound below that fails a process with nothing wrong with it.
Resolving finer needs more connections, not a smaller number.

After the rewrite, both churn arms are clean and the series show the sawtooth
plainly (negative phases on both sides, descriptor counts flat):

```
T-SECLEAK-CHURN-RELAY   rss-consumer  last phase +624 KiB, trend +1120 KiB over 3 phases, swing 732 KiB
T-SECLEAK-CHURN-DIRECT  rss-provider  last phase -364 KiB, trend +1752 KiB over 3 phases, swing 2068 KiB
T-SECLEAK-CHURN-DIRECT  rss-consumer  last phase -248 KiB, trend  +220 KiB over 3 phases, swing 648 KiB
```

## 4. S-5 — the listener held the socket while the dialer was already dialing

### 4.1 The measurement

S2 (`sec_ttd.sh`, 20 tunnels per topology) reports 20/20 direct on all three
topologies, no relay fallbacks, no failures. The interesting number is not the
success rate but `direct_ready_ms`, which the consumer logs itself and which the
census table did not yet carry. Collected from the 27 consumer logs of the
`vm-ws` runs (provider on the VM, consumer on the workstation behind a home NAT):

```
37 40 41 42 42 43 43 44 44 48 49 49 50 52 52 52 52 53      <- 18 runs
1036 1043 1043 1044 1044 1045 1050 1052 1162               <-  9 runs
```

**Bimodal, with nothing in between.** A distribution with a gap that large is
never a network: it is a timer. The other two topologies confirm it is not the
path — `ws-vm` (provider on the workstation) and `vm-vm` produced 2–4 ms and
37–52 ms clusters with a single 1504 ms outlier in 40 runs.

### 4.2 The mechanism

Every slow run pairs a consumer log that nominated early with a provider log
that nominated never:

```
consumer  20:10:37.375  connectivity-check round finished role=Dialer   nominated=Some(<vm>:50591) checks_ms=213 retry_passes=0
consumer  20:10:38.425  direct QUIC path ready (consumer) winner=<vm>:50591 direct_ready_ms=1050
provider  20:10:38.272  connectivity-check round finished role=Listener nominated=None            checks_ms=1126
```

The provider's round ends 153 ms before the consumer's connection completes, and
1126 ms after it began. Read in order:

1. the dialer's check round validates a pair in ~210 ms, and then **disables its
   responder** — "late frames are counted, never answered" is the round's own
   documented contract;
2. the listener's adaptive plan probes the peer's *local* candidates first and
   reaches the reflexive group only a `CHECK_GROUP_STAGGER` (150 ms) later, by
   which time the address it is probing has stopped answering. Its round is dry,
   so it runs to the full window;
3. the listener's socket becomes a QUIC endpoint only *after* that round
   (`listener_checks_then_quic`: checks, `into_socket`, then
   `DirectListener::from_checked_socket`). For the whole remaining second there
   is nothing on that socket to answer a QUIC Initial;
4. the dialer's first Initial is therefore dropped, and quinn does not retry it
   until the initial PTO — `333 ms + 4 × 166 ms = 999 ms` with the RFC 9002
   default `initial_rtt`. 999 ms + one handshake ≈ the 1036–1162 ms cluster,
   to the millisecond.

The listener-side precondition is directly countable in the logs:
**8 of 52** VM-side listener rounds ended `nominated=None`, against **0 of 22**
workstation-side ones — the same asymmetry as the stall, in the same ratio.

### 4.3 The fix

A listener that has just answered an authenticated check request already holds
everything its half of the round can produce: the peer has the key, is on this
generation, plays the other role, and reaches us from that source. Everything
after that answer is the *dialer's* move, and the dialer makes it as soon as its
own nomination completes — which our answer is what causes. So the listener now
ends its round there (`CheckRole::Listener` arm of `inbound_req_rx` in
`run_connectivity_checks`), nominating the source it authenticated.

Two details are load-bearing:

* **the response goes on the wire before the round is torn down.** The actor used
  to announce the request to the driver while still holding the reply; ending the
  round stops the actor, so announcing first could have eaten the one datagram
  the dialer is waiting for. `CheckAction` now carries the reply and the
  announcement separately, and `recv_actor` sends, *then* announces;
* **`nominated` is set, not left empty.** It gates the Fase 7 sprayed escape, and
  spending six seconds spraying for a peer that has just reached us would be the
  same mistake in a larger size. `observed` stays `None` on this path by
  construction — it can only come from a response to our own request, and no
  caller consumes it.

Gates: `listener_hands_off_as_soon_as_the_dialer_proves_it_can_reach_us`
(red-checked: without the arm it does not merely fail, it burns the full 3 s
window and reports `nominated=None`, which is the production shape) plus the
existing `checks_never_answer_unauthenticated_probes`, which now also pins that
**only** the authenticated request ends the round — the three forged probes
before it must not.

## 5. S-7 — a direct QUIC endpoint is never cold, so it should not assume it is

RFC 9002's 333 ms `initial_rtt` is the value for a connection with no information
about the path. A bore direct endpoint is never in that position: it is built
only after a completed authenticated check exchange with this exact peer, or a
TCP control connection to this exact host. The two errors are also not
symmetric — underestimating costs one duplicate Initial on a slower path, after
which the first real sample governs; overestimating costs a full PTO of silence
whenever the first Initial is lost, and the first Initial of a punched path is
precisely the packet most likely to be lost, because it is the first datagram to
cross a mapping the peer has only just created.

`DIRECT_INITIAL_RTT` is therefore 100 ms (PTO ≈ 300 ms), overridable with
`BORE_DIRECT_QUIC_INITIAL_RTT_MS` and clamped to [10 ms, 333 ms] — nothing above
the RFC default can be called informed. It is defence in depth for S-5, not a
substitute: S-5 removes the systematic cause, S-7 bounds what a genuinely lost
packet costs.

## 6. S-8 — the worst case of the relay→direct upgrade was four and a half minutes

A secret consumer that could not negotiate a direct path at registration keeps
trying on an exponential backoff (2, 4, 8, … seconds). The cap on that backoff is
not a tuning parameter in the usual sense: it *is* the worst-case time a tunnel
stays on the relay after whatever was blocking the direct path goes away — a
firewall rule withdrawn, a captive portal cleared, a peer that was still booting.

It was 256 s. The same codebase already ships the same mechanism for the VPN on a
**fixed 30 s grid** (`DIRECT_RETRY_INTERVAL`), in production, so the politeness
argument against retrying often has already been answered here — and answered
more aggressively than what the secret path now does. `UDP_UPGRADE_MAX_SECS` is
60 s: still the more conservative of the two, still growing from 2 s so a path
that is merely slow to settle is not hammered, and the cap decides only how long
the worst case lasts.

The relationship is pinned at COMPILE time (`const _: () = assert!`), not in a
test: a test reports the regression after the build, a const assert refuses to
produce the build. The field gate had to provoke the worst case to measure it,
which is what
`secret_leak_hunt.sh upgrade-late` does: the consumer registers with no provider
(so the first negotiation necessarily fails and the tunnel starts on the relay),
the provider is then withheld for 90 s — past several backoff steps — and the arm
measures the seconds from "the direct path became possible" to "the admin API
says direct". On the old grid (2, 4, 8, 16, 32, 64, 128) a provider appearing at
t=90 s was not noticed until t=190 s; the bound asserted is 75 s.

## 7. S-9 — the winning-pair cache had no ceiling

`pair_cache` is a process-global map from tunnel id to the last remote address
that produced a working direct path. Expiry ran only inside `recall`, and only
for the key being recalled — so a key that is never recalled again is never
examined again. One entry per tunnel id, for the life of the process.

Per entry it is a short string and 24 bytes, which is precisely what makes it
the kind of growth nobody notices: a `bore proxy` handling one tunnel keeps one
entry forever and is fine, while a harness, a multi-link VPN or a long-lived box
reconnecting under fresh ids grows without bound and never reaches a size that
shows up anywhere.

`remember` now sweeps expired entries — it is the one call guaranteed to happen
for every new key — and enforces a 256-entry ceiling by evicting the oldest.
Everything still in the map is inside its TTL, so which older entry goes is a
tie-break and not a contract; the gate asserts the ceiling and the survival of
the entry just written, nothing else. The two cache gates share a mutex, because
the cache is global and the bounded one evicts exactly what the other has just
stored.


## 8. The campaign, phase 1 (binary `1bb3243a`, i.e. BEFORE S-5/S-7/S-8/S-9)

All of §8 was taken with the pre-fix binary on all three hosts. It is the
baseline the phase-2 re-run (§9) is compared against, and it is also where two
of the four defects were caught doing damage rather than merely existing.

### 8.1 S2 — the path census, and the cleanest statement of S-5 in the campaign

20 independent tunnels per topology, each brought up, poked with one 64 KiB
proxied connection, measured and torn down.

| topology | direct | relay | failed | median ttd | min | max |
|---|---|---|---|---|---|---|
| `vm-ws` | 20 | 0 | 0 | 107 ms | 87 | **1150** |
| `ws-vm` | 20 | 0 | 0 | 104 ms | 83 | 110 |
| `vm-vm` | 20 | 0 | 0 | 96 ms | 82 | 452 |

**Traversal itself is not in question: 60 of 60 tunnels went direct, with no
fallback and no failure**, across a home NAT, a same-region VM and both
directions. What the medians hide is in the full distributions:

```
ws-vm   83  83  85  94  96  97  97  97  98 104 104 104 105 106 106 106 106 107 108 110
vm-ws   87  88  91  95 102 103 104 104 106 107 107 107 112 788 790 792 800 804 1149 1150
vm-vm   82  82  83  85  88  93  93  94  94  96  96  98  98  98 105 106 110 200 205 452
```

`ws-vm` is 20 samples inside a 27 ms band. `vm-ws` is 13 samples in the same
band and then **7 of 20 — 35 % — at 788 ms or worse**, with nothing in between.

The discriminator is not the direction, not the NAT and not the WAN leg: it is
**which host played Listener**. The provider is the listener, so

| topology | listener | slow runs |
|---|---|---|
| `ws-vm` | workstation | 0 / 20 |
| `vm-ws` | test VM | 7 / 20 |
| `vm-vm` | test VM | 3 / 20 (200, 205, 452 ms) |

which is the same asymmetry §4.1 found in the logs (8 of 52 VM-side listener
rounds dry against 0 of 22 workstation-side) seen from the other end — and the
`vm-vm` row rules out the WAN as the cause, because there is no WAN between
those two peers at all.

### 8.2 S1 — relay vs direct, paired, and what actually decides the winner

Median of the ratios direct/relay, five pairs per cell, 128 MiB over 4
connections per arm, order alternating inside the pair, 75 s cooldown.

| topology | direction | host SENDING the bulk | median direct/relay |
|---|---|---|---|
| `vm-ws` | get | test VM | **1.218** (n=3, see below) |
| `vm-ws` | put | workstation | 0.757 |
| `ws-vm` | get | workstation | 0.828 |
| `ws-vm` | put | test VM | 1.167 |
| `vm-vm` | get | test VM | 2.298 |
| `vm-vm` | put | test VM | 1.456 |

The `vm-ws get` cell is the one H-13 corrupted: the stage printed `0.965`
because two `startfail` rows entered the median as the ratio 0. The three arms
that ran were 0.965 / 1.247 / 1.218, so the corrected median is **1.218**. The
stage will be re-taken in phase 2 with the fixed reducer; the corrected value
is quoted here because it is arithmetic on rows that are in the file, not a
new measurement.

Read the table by the third column and it stops looking noisy:

```
sender = test VM        direct/relay = 1.218   1.167   2.298   1.456   -> direct wins, always
sender = workstation    direct/relay = 0.757   0.828                   -> direct loses, always
```

**The relative merit of the direct path is a property of the SENDING host, not
of the direction, the topology or the NAT.** The workstation is slower at
sending QUIC than at sending TCP; the VM is not. The mechanism to test is CPU:
the direct path encrypts and paces in user space, while the relay arm is TCP
in the kernel with segmentation offload — and that is exactly what S3 measures
in CPU seconds per delivered GiB. Both hosts have `net.core.rmem_max` /
`wmem_max` at 4 MiB, so socket buffers are ruled out as the explanation.

The `vm-vm` control is NOT a CPU isolation, despite the harness comment that
says so: its direct arm is loopback while its relay arm still crosses the WAN
to the broker twice, so 2.298 is a topology effect. What it does establish is
the direct path's CEILING on that host — **387 MB/s** — which is the number
S3 prices.

### 8.3 S3 — what each transport costs, and the claim that makes the case

`vm-vm`, 2 GiB per arm over 4 connections:

```
relay    204.03 MB/s  path=relay   fb=0  relay_tx=2148532224  relay_rx=68
direct   407.83 MB/s  path=direct  fb=0  relay_tx=0           relay_rx=0
```

`relay_tx` is the SERVER's own byte counter for that tunnel. On the relay arm
it reads 2 147 483 648 + framing — every delivered byte crossed the broker. On
the direct arm it reads **zero**: the server brokered the punch and then
carried nothing. That is the structural difference between a secret tunnel and
every other bore transport, stated in a form that is falsifiable rather than
rhetorical, and it is why a secret direct path is the only one of bore's fast
paths that reduces the server's bill to nothing.

The `vm-ws` arm of this stage is INVALID — S-5 ate it; see §3.3 — and is
re-taken in phase 2.

### 8.4 S4 — new-connection latency and the concurrency ladder

100 probes per rung, rungs at 0 / 16 / 64 / 256 held connections, p50 in ms.

| topology | transport | held=0 | 16 | 64 | 256 |
|---|---|---|---|---|---|
| `vm-ws` | relay | 26.99 | 26.71 | 26.87 | 26.80 |
| `vm-ws` | direct | *(errs=100, S-5)* | 21.70 | 21.75 | 21.63 |
| `vm-vm` | relay | 3.33 | 3.43 | 3.46 | 3.31 |
| `vm-vm` | direct | 0.745 | 0.767 | 0.805 | 0.776 |

Two results and one scar.

The results: a new connection over the direct path costs **21.7 ms against
26.9 ms** on the home-NAT leg (the relay's extra transit is the whole
difference) and **0.78 ms against 3.4 ms** with both peers in-region — and
**neither transport degrades with concurrency**. 256 held connections move the
p50 by less than 0.2 ms on every row, which is what the secret path's
`--max-conns` semaphore and per-connection QUIC stream model are supposed to
deliver and, unlike the vhost campaign's N-9 tail, here they do.

The scar is the `vm-ws direct held=0` cell: `n=0 p50=nan errs=100`. Its 100
probes all ran inside the S-5 window against a peer whose QUIC listener came
up 908 ms later. It is the third stage the defect damaged, and the harness
printed `path=unknown` in the header while the ladder in fact ended on the
direct path — both now fixed (§3.3).
