# Public-tunnel staging campaign — evidence

**Date:** 11 September 2026
**Subject:** `bore local` (public tunnels), TCP relay and QUIC direct, measured
against the real AWS staging deployment from an in-region VM and from a
domestic workstation, with the native binary, the dockerized binary and the
OpenSSH gateway.

This is the raw-evidence companion to
[`final_public_perf_review.md`](final_public_perf_review.md) (Italian, written
for a reader who is not expected to know the codebase). Everything here is a
transcript or a number taken from one; nothing is reconstructed from memory.

It follows the same contract as the vhost campaign
([`VHOST_STAGING_EVIDENCE_2026-09-10.md`](VHOST_STAGING_EVIDENCE_2026-09-10.md)):
every measurement names the build it was taken against, every comparison is
paired, and every arm reports which transport it actually used rather than
which one it asked for.

---

## 0. Coordinates and builds

| actor | what it is | build |
| --- | --- | --- |
| server | AWS `t4g`-class, aarch64, 2 vCPU, 903 MiB, `ghcr.io/manprint/bore:main` | see below |
| test VM | AWS, x86_64, 2 vCPU, 3.8 GiB, same region as the server | native `~/bore` + `ghcr.io/manprint/bore:client` |
| workstation | domestic link, outside AWS | native binary |

Builds under test, all three actors on the SAME commit (printed by the harness
into `BUILD.txt` at the start of the run, and readable at any time from
`/admin/api/v1/config` → `server_version`, which this campaign added):

```
version_field=1.0.0 - main - dbcc645a
bore 1.0.0 - main - dbcc645a
image=ghcr.io/manprint/bore:main started=2026-09-11T03:50:07Z
vm_binary=bore 1.0.0 - main - dbcc645a
client_image=dbcc645a4af5424b0f4f670fa607bb66116f158f
```

The **baseline** build, against which every "before" number was taken, is
`main @ 0771de98` — what the deployment was running when the campaign started.

Server configuration in force (from `/admin/api/v1/config`, not from the
compose file — the endpoint derives the direct-UDP block from the tuning
actually installed):

```
port_range=9000-9100          udp=true                 vhost_quic_port=443
max_conns=1024                max_carriers=1024        proxy_buffer_size=256KiB
udp_stream_receive_window=1MiB            udp_connection_receive_window=16MiB
udp_send_window=16MiB                     udp_max_streams=8192
udp_socket_send_buffer=16777216           udp_socket_recv_buffer=16777216
direct_quic_keepalive_ms=3000             direct_quic_idle_ms=10000
udp_direct_slots=30
```

Three vhost tunnels belonging to the operator (`dufspcloud`, `tennis`,
`tennis1`) were live on this server throughout. They were never touched: every
teardown in the harness matches an exact port or container name, and the
project rule against a blanket `pkill bore` is enforced by construction.

---

## 1. What a public tunnel is, for the purposes of this document

`bore local <port>` asks the server for a PUBLIC PORT out of its configured
range and forwards whatever arrives there to a local port. Three properties
shape every measurement below:

1. **It forwards arbitrary TCP, not HTTP.** Measuring it through an HTTP origin
   folds HTTP parsing on both ends into the result. This campaign therefore
   uses a dedicated raw-TCP origin (`scripts/perf/raw_origin.py`, protocol
   `GET n` / `PUT n` / `PING` / `HOLD s` / `ECHO`) for every transport number,
   and keeps a single HTTP arm, clearly labelled, so the numbers can be lined
   up against the vhost campaign.
2. **There is no TLS on the tunnel port** unless the tunnel asked for
   `--https`. A public tunnel is therefore *cheaper* than a vhost request by
   exactly one TLS session, and the two latency figures must not be compared
   without saying so.
3. **`--udp` on a public tunnel is server→client QUIC**, the same mechanism as
   `bore vhost --udp` and with no STUN or hole punching: the server is public,
   the client dials it. The TCP relay stays warm for the tunnel's life and each
   inbound connection falls back to it in place.

---

## 2. Defect register

Thirteen public-path findings, each one measured rather than reasoned about.
The register with full detail, including the red-check performed on every fix,
is reproduced in §16. Summary:

| id | sev | what it was | status |
| --- | --- | --- | --- |
| P-13 | HIGH | the server's shared QUIC endpoint ran on a 208 KiB UDP socket — the kernel default, silently — and it is the RECEIVING side of every download | fixed, red-checked |
| P-7 | HIGH | `Server::serve_tunnel` never read the control substream at all | fixed + gated |
| P-9 | HIGH | a heartbeat the peer never reads wedges the client's whole listen loop | fixed, red-checked |
| P-12 | HIGH | `--max-conns` was above the process descriptor limit, so `EMFILE` refused first — on every listener, admin API included | fixed, red-checked |
| P-4 | MED | a wedged client holds its public port until the server restarts | fixed, red-checked ×3 |
| P-1 | MED | the public direct open was unbounded | fixed + gated |
| P-2 | MED | no per-tunnel direct-path observability | fixed + gated |
| P-3 | MED | a partially-established direct pool could stay short forever | fixed + unit-gated |
| P-6 | MED | `--vhost-quic-port` was ignored without a vhost config | fixed + gated |
| P-5 | LOW | `--carriers N>32` on a public `--udp` tunnel clamped silently | fixed |
| P-10 | LOW | every relay-only public tunnel reported `current_path: "unknown"` | fixed, red-checked |
| P-11 | LOW | the config endpoint published a live gauge under a configuration name | fixed, red-checked |
| P-8 | — | the blackout loss window IS the QUIC idle timeout — measured, no defect | n/a |

Two deployment findings (D-1, D-3) and one deployment recommendation (D-2)
are in the same register; D-1 was a shipped compose file that enabled the
direct path while documenting that it did nothing.

Fourteen harness defects (H-1 … H-14) were found and fixed along the way; they
are listed in §17 because a harness that lies is indistinguishable from a
server that misbehaves, and nine of them had already silently thrown away data,
refused to run, measured a program older than the one in the tree, published
the sum of two ladder rungs as one rung, or — the worst shape — measured
nothing at all while printing it in the exact format of a real measurement.
That worst shape occurred TWICE (H-7, H-12), which is why the workstation
stage now carries a preflight that refuses to measure a tunnel moving no
bytes rather than a comment asking the operator to check.
One of them (H-9) is also what made P-12 visible: a harness defect that
over-held connections drove the server past a limit nobody had reconciled.

---

## 3. Correctness: before and after, on the real deployment

These are the arms that answer yes/no questions. They are reported first
because a throughput number taken on a build that loses its direct path
forever is not worth reading.

| arm | before (0771de98) | after (dbcc645a) |
| --- | --- | --- |
| **S4 wedged client (SIGSTOP) — P-4** | port 9089 `never (still held after 150s)` → **FAIL: zombie public port** | port 9060 **reaped after 60 s** → PASS |
| **S2 direct-path loss and recovery — P-7** | `path=null opens=null pool=null` throughout (the observability that detects it ships in the same fix); the netns gate `T-PUB-RECOVER` is the oracle: **never recovered**, still degraded at 100 s | fell back to the relay in **12 s**, direct path back **4 s** after the drop rule was removed, `path=direct opens=3 fb=3 pool=1` → PASS |
| **S3 hard client death (SIGKILL)** | port 9009 released in 1 s, re-grantable | port 9036 released in **0 s**, re-grantable → PASS |
| **S5 500-connection churn** | not taken on the old build | 500 connections in 2 s, `active` 0 → 0, `conn_rejections=0`, `direct_budget_refusals=0`, post-churn 8 MiB at **94.82 MB/s** → PASS |

The contrast in the first row is the whole point of P-4: a hard `kill -9`
always released the port, because the kernel closes the socket. It is the
*wedged but TCP-alive* client — a suspended laptop, a frozen process — that the
old build could not distinguish from a healthy idle tunnel, and that held a
public port until the server was restarted.

### 3.1 The mixed-version case (P-9), measured in the field

P-4 gives the client a heartbeat. Every server built before P-7 never reads it.
Clients upgrade independently of servers, so this combination is not
hypothetical, and it had to be measured against the OLD server — once staging
was redeployed, no reachable peer still refused to read the control substream.

Both runs used `BORE_CTRL_HEARTBEAT_MS=2`, which compresses days of production
uptime into seconds (the real wedge needs ~256 KiB of unread frames, which is
what the yamux stream credit holds).

| t | before: unguarded client | after: guarded client |
| --- | --- | --- |
| +15 s | 21.25 MB/s | 28.88 MB/s |
| +30 s | **NO RESPONSE** | 47.17 MB/s |
| +45 s | — | 0.43 MB/s (2.35 s — the single probe that lands while the write is parked) |
| +60 s | **NO RESPONSE** | 19.32 MB/s |
| +75…+120 s | **NO RESPONSE** | 47.49 / 40.26 / 52.25 / 42.96 MB/s |

`served=8 no-response=0` after the fix, and the client log carries the
stand-down warning exactly once:

```
02:43:20.586661Z  WARN bore_cli::client: control heartbeat write blocked: the server
is not reading this tunnel's control substream. Standing the heartbeat down for this
session … upgrade the server. timeout=10s
02:43:20.586787Z  INFO proxy: bore_cli::client: new connection
```

The next accept happens **126 µs** after the warning. The residual cost of the
defect is therefore exactly one deadline for one proxied connection, and never
again for the life of the session.

---

## 4. Method, and the three rules that make the numbers mean anything

The vhost campaign established these the hard way; this campaign inherits them
unchanged, and the harness enforces them rather than relying on discipline.

**Rule 1 — the instance network allowance is the dominant confounder.** One
4-stream 10 s download is roughly a whole inbound burst budget on this
instance class. Consequences:

* every arm waits `COOL` (75 s by default) after its burst;
* comparisons are **paired** — both transports measured back to back, the
  **ratio** reported, and the order alternating within the pair — because the
  control arm drifted 29 % over a few minutes in the vhost campaign, which is
  larger than most of the effects being measured;
* the flavour comparison registers all three forwarders **at once** and hits
  them in rotation with a shift per round, because a sequential ladder charges
  the whole budget to whichever arm runs first (the vhost campaign measured
  exactly that: docker's x4 rung collapsed to 9.90 MB/s with allowance +597 390
  while its own x1/x2 rungs were clean);
* `res/ena_timeline.sh` samples the allowance counters for the whole run on a
  timeline, and every stage stamps its own wall-clock window, so a shaped burst
  can be identified afterwards instead of silently averaged in.

**Rule 2 — MB/s cannot answer "is the server application-limited?"** on a
burstable instance, because the bucket caps the rate long before the CPU does.
`vm_pub_eff.sh` answers it in **CPU seconds per GiB**, which is invariant to
the cap, and prints an epoch window per case so the HOST-side `/proc/stat`
sampler can be matched to it. The container's own CPU% is not enough: the
vhost campaign measured the host softirq the kernel spends on the container's
behalf at 37–40 % of the bill.

**Rule 3 — the path is confirmed, never assumed.** Every arm reads
`current_path`, `direct_stream_opens`, `direct_fallbacks` and `direct_pool`
from `/admin/api/v1/tunnels` around the measurement. An arm that asked for
`--udp` and ran on the relay is reported as a relay number and is never quoted
as a QUIC one. This campaign is the first that *can* do this for public
tunnels — the four fields are P-2.

Two more, specific to this campaign:

**The held connections in a concurrency ladder must move no bytes.** They speak
the origin's `HOLD` verb, which answers once and then goes quiet. An arm whose
held connections were downloading would saturate the link and measure the link.
They are also held from ONE process: 512 Python interpreters on a 2-vCPU /
3.8 GiB VM measures the driver's memory pressure, not the tunnel.

**The SSH leg is TCP-relay-only by design** (I-SSH2: no `--udp`, no
`--carriers>1`). It is compared against the native and docker RELAY arms and
never against their QUIC arms. That is a property of the transport, not a
defect, and the flavour table says so on every line.

**A measurement must be bounded by the thing being measured, and must be
taken against the program in the tree.** Both were learned here rather than
inherited: a run bounded by an external `timeout` prints nothing when it is
killed and reads as zero bytes (H-8), and a long-lived helper process that
`pgrep` calls "already running" can be an older build of itself (H-7). The
harness now caps a transfer in TIME inside the load driver, and restarts any
origin whose process is older than its own script file.

## 5. The measurement stages

Ten stages, run strictly serially by `scripts/perf/staging/pub/pub_driver.sh`
because they share one server, one 2-vCPU VM and one allowance budget.

| stage | script | question |
| --- | --- | --- |
| P1 | `vm_pub_ab.sh p1` | relay TCP vs QUIC direct, paired, download and upload |
| P2 | `vm_pub_ab.sh p2` | does the carrier ladder do anything on a public tunnel? |
| P3 | `vm_pub_ab.sh p3` | what does ONE new connection through the tunnel cost? |
| P4 | `vm_pub_ab.sh p4` | HTTP over a public tunnel, comparable with the vhost campaign |
| P5 | `vm_pub_flavours.sh relay` | native binary vs dockerized binary vs OpenSSH `-R`, rotated, all on the relay |
| P5b | `vm_pub_flavours.sh udp` | native vs dockerized binary on the QUIC direct path |
| P6 | `vm_pub_conc.sh` | what does a fresh connection cost behind N held ones? |
| P8 | `vm_pub_netem.sh` | how does each transport degrade as the path degrades? |
| P9 | `vm_pub_eff.sh` | CPU seconds per GiB — the application-limit question |
| P7 | `vm_pub_stab.sh` | soak, recovery, port release, wedged-client reap, churn |

The workstation topology (`ws_pub.sh`) is run **separately, afterwards**: it
competes with the VM stages for the same server and the same budget.

---

## 6. P1 — relay TCP vs QUIC direct, paired

128 MiB per arm across 4 connections, 5 pairs, 75 s cooldown, order alternating
within the pair. Raw TCP, no HTTP. `o=` is the number of direct QUIC stream
opens the SERVER counted during that arm, so "direct" is confirmed and not
assumed: every QUIC arm below opened exactly one stream per connection.

**Download (`GET`):**

| pair | relay MB/s | quic MB/s | relay/quic | paths |
| --- | --- | --- | --- | --- |
| 1 | 198.65 | 115.20 | 1.724 | relay(o=0) direct(o=4) |
| 2 | 160.57 | 133.27 | 1.205 | relay(o=0) direct(o=4) |
| 3 | 163.25 | 125.65 | 1.299 | relay(o=0) direct(o=4) |
| 4 | 176.81 | 114.65 | 1.542 | relay(o=0) direct(o=4) |
| 5 | 182.91 | 121.16 | 1.510 | relay(o=0) direct(o=4) |
| | | | **median 1.510** | |

**Upload (`PUT`):**

| pair | relay MB/s | quic MB/s | relay/quic | paths |
| --- | --- | --- | --- | --- |
| 1 | 158.44 | 127.26 | 1.245 | relay(o=0) direct(o=4) |
| 2 | 182.79 | 129.63 | 1.410 | relay(o=0) direct(o=4) |
| 3 | 188.70 | 130.11 | 1.450 | relay(o=0) direct(o=4) |
| 4 | 157.38 | 125.86 | 1.250 | relay(o=0) direct(o=4) |
| 5 | 168.77 | 125.71 | 1.343 | relay(o=0) direct(o=4) |
| | | | **median 1.343** | |

**Reading.** On a clean in-region path the **TCP relay is faster than the QUIC
direct path in both directions** — by 51 % on download and 34 % on upload — and
every one of the ten pairs agrees in sign. This is the public-tunnel
confirmation of the vhost campaign's F-8: QUIC direct is not a general
throughput upgrade. It is a userspace congestion controller and a userspace
packet path competing with the kernel's TCP stack plus TLS offload, on an
instance whose CPU is the scarce resource.

The relay numbers themselves — 157 to 199 MB/s, i.e. 1.3 to 1.7 Gbit/s — are
well above the instance's sustained allowance, so they are **burst** figures:
the point of the pairing is that both halves of each pair spend the same budget,
not that either half is a sustainable rate. The allowance timeline confirms
the bucket was being touched: **4 of the 141 samples inside P1's window
recorded `bw_in_allowance_exceeded` misses**, 1 843 in total, all of them
inbound and all in the second half of the stage. No `bw_out` miss was
recorded anywhere in P1. Four shaped samples out of 141 is exactly the regime
the 75 s cooldown is designed to produce — the bucket is being touched, not
lived in — and because the pairing is back-to-back, a shaped moment lands on
both halves of a pair rather than on one.

**Where the bottleneck is.** Not in the tunnel. Both arms move the same bytes
over the same two hops; the only difference is which transport carries the
server→client leg, and the slower one is the one that does its congestion
control and packet handling in userspace. §13 (P9) prices that difference in
CPU seconds per GiB, which is the measurement that settles it.

---

## 7. P2 — the carrier ladder on the TCP relay

`--carriers N` opens N parallel TCP connections between server and client and
round-robins **proxied connections** across them. On a public tunnel the data
path opens one substream per INBOUND connection, so the head-of-line behaviour
is not the same shape as vhost's and the ladder had to be measured, not
inherited.

Each rung is one 128 MiB transfer over 4 connections, 75 s cooldown between
rungs. Single unpaired measurements — the ladder is a shape, and the shape is
what has to survive the drift, not any individual number.

| carriers | download MB/s | upload MB/s |
| --- | --- | --- |
| 1 | 210.25 | 154.90 |
| 2 | 252.38 | 229.30 |
| 4 | **303.60** | 225.05 |
| 8 | 276.62 | 178.10 |

Every rung ran on the relay with `opens=0`, i.e. no direct stream was involved
anywhere in this stage. The allowance timeline records **4 shaped samples out
of 56** inside P2's window (540 `bw_in` misses in total, no `bw_out` miss), so
the ladder was not measured against an exhausted bucket — but with unpaired
rungs that is an argument for reading the SHAPE and not the individual
figures, which is how it is read below.

**Reading.** The ladder rises and then falls, peaking at **4 carriers on
download (1.44× over a single carrier) and 2 on upload (1.48×)**, and 8 is
worse than 4 in both directions. This is the opposite sign from the vhost
campaign, where carriers slightly HURT a clean idle path (median c4/c1 =
0.941) — and the difference is explained by what is being carried. The vhost
measurement moved ONE bulk flow, which rides exactly one carrier no matter how
many exist, so the extra carriers bought nothing and cost a little. This
measurement moves FOUR concurrent connections, which the server spreads across
the carriers: at `--carriers 1` all four share one yamux connection and one
kernel socket, and they contend for that socket's congestion window and for
the single yamux framing task.

**Where the bottleneck is.** At 1 carrier it is the single TCP connection —
four streams multiplexed into one cwnd, one send buffer, one framing task. At
8 it is the server's CPU and the instance's allowance: eight sockets on two
vCPUs, each with its own congestion controller ramping into the same shaped
bucket, is measurably worse than four. The useful operator statement is not "4
is the right number" but "the right number tracks the number of CONCURRENT
connections the tunnel actually serves, and past that it costs". A tunnel
serving one transfer at a time should stay at the default 1; §18 gives the
recommendation with the concurrency stage (P6) folded in.

---

## 8. P3 — what one new connection through the tunnel costs

One NEW TCP connection per probe, issued serially, 100 probes. Serial is the
point: a concurrent probe would measure queueing, and the number an operator
actually cares about is what a single fresh client pays. The payload is a tiny
raw-TCP round trip, so almost all of it is connection setup.

| transport | n | p50 ms | p95 ms | p99 / max ms | errors | confirmed path |
| --- | --- | --- | --- | --- | --- | --- |
| relay TCP | 100 | **4.665** | **6.226** | **8.767** | 0 | `relay`, opens=0 |
| QUIC direct | 100 | 4.899 | 10.756 | 17.785 | 0 | `direct`, opens=101, pool=1 |

`opens=101` is the confirmation that matters: 100 probes plus the warm-up each
opened their own direct QUIC stream, so no probe in the QUIC column was
silently served by the relay. The allowance timeline records **no shaped
sample at all** inside P3's window: these probes move almost no bytes, which
is what makes latency the one stage the bucket cannot distort.

**Reading.** The medians are within 0.23 ms of each other — **the transports
cost the same at the median** — but the TAIL is not close: QUIC direct is
**1.7× worse at p95 and 2.0× worse at p99**. Nothing in this stage is lossy and
nothing is loaded; the tail is the direct path's own variance, i.e. userspace
scheduling of a QUIC stream open against a kernel that opens a TCP substream on
an already-established connection.

Put beside P1 this is the complete clean-path picture: on a clean in-region
path the QUIC direct path is **slower in bulk and no better at the median for
small requests, with a worse tail**. That is not an argument that the direct
path is useless — §12 (P8, degraded paths) and §11 (P6, concurrency) are where
it earns its place — it is an argument that it must not be the default.

**Where the bottleneck is.** In the round trips, for both transports. The
VM→server RTT is ≈2.1 ms and a probe pays a TCP handshake to the public port
plus the server→client substream open plus the client's own connect to the
local origin. 4.7 ms at 2.1 ms RTT is about two round trips of unavoidable
work; there is no wait left to decompose, which is the same conclusion the
vhost campaign reached in its §12.4 and the reason no open-path timers were
added here either.

---

## 9. P4 — HTTP through the public tunnel

The same tunnel, now pointed at a real HTTP origin, so these numbers are
directly comparable with the vhost campaign's — **minus exactly one TLS
session**. A public tunnel port carries plain TCP; unless the tunnel asked for
`--https` there is no TLS termination on it at all. Load is `oha`, 6 s per
cell, 1 KiB responses.

| arm | c | rps | p50 ms | p95 ms | p99 ms | ok |
| --- | --- | --- | --- | --- | --- | --- |
| relay, keep-alive | 1 | 374 | 2.634 | 3.032 | 3.408 | 1.0 |
| relay, keep-alive | 8 | 3110 | 2.541 | 2.918 | 3.103 | 1.0 |
| relay, keep-alive | 32 | 10213 | 2.885 | 4.576 | 7.268 | 1.0 |
| relay, new conn each | 8 | 1506 | 4.902 | 7.153 | — | 1.0 |
| quic, keep-alive | 1 | 369 | 2.698 | 3.037 | 3.335 | 1.0 |
| quic, keep-alive | 8 | 3227 | 2.417 | 2.853 | 3.269 | 1.0 |
| quic, keep-alive | 32 | 10777 | 2.928 | **3.476** | **3.960** | 1.0 |
| quic, new conn each | 8 | 1608 | 4.895 | **5.990** | — | 1.0 |

Single-stream bulk over HTTP, same tunnels:

| arm | MB/s |
| --- | --- |
| relay, 128 MiB single stream | **207.74** |
| quic, 128 MiB single stream | 84.04 |

Paths confirmed: relay `opens=0 fb=0`; quic `path=direct opens=9696 fb=0` —
every one of the ~9 700 proxied HTTP connections in the QUIC arms opened its
own direct stream, and not one of them fell back. The allowance timeline
records **no shaped sample inside P4's window** either, including across the
two 128 MiB bulk cells.

**Reading, and the first result in this campaign that favours the direct
path.** At concurrency 1 and 8 the two transports are indistinguishable
(within 0.2 ms at every percentile, rps within 4 %). At **concurrency 32 they
separate at the tail and the direct path wins**: p95 3.476 vs 4.576 ms (1.32×)
and p99 3.960 vs 7.268 ms (1.84×). The same sign appears on the
connection-churn arm, where every request pays a fresh TCP connect: p95 5.990
vs 7.153 ms. Throughput is unchanged in every one of those cells, so this is a
pure tail effect.

The mechanism is the one the architecture predicts. On the relay, all 32
proxied connections are yamux substreams on **one** TCP connection: they share
one congestion window, one send buffer and one framing task, so a slow
substream delays the ones queued behind it in the same frame stream — classic
head-of-line blocking, visible only at the tail and only once there are enough
concurrent streams to queue. On the direct path each proxied connection is its
own QUIC bidirectional stream with its own flow control on a transport that
does not head-of-line-block across streams.

And the price is on the very next line of the same table: **the same tunnel
that is 1.84× better at p99 under 32 concurrent small requests is 2.47× slower
on a single bulk stream** (84.04 vs 207.74 MB/s). The two are not in tension;
they are the same fact seen twice. Userspace per-stream handling buys stream
independence and costs bulk throughput.

**Where the bottleneck is.** For the small-request cells at c=1 and c=8:
round trips, not the tunnel — 2.5 ms at 2.1 ms RTT is one round trip plus
parsing, and neither transport can improve on it. At c=32: the relay is limited
by its single yamux/TCP connection's ordering, which is exactly what P2 showed
carriers can relieve on the relay side. For the bulk cell: the server's CPU on
the QUIC arm — priced in §13.

Note also that `--carriers` is the relay's own answer to the c=32 tail, and
that P2 measured it helping throughput under 4 concurrent connections. The
operator recommendation in §18 therefore does not read "use the direct path
for concurrency": it reads "raise carriers first, because it keeps the relay's
bulk throughput, and reach for the direct path when the path itself is bad".

---

## 10. P5 — the three ways to run a forwarder, on the relay

An operator can expose a public port three ways: the native binary, the
dockerized binary, or a stock OpenSSH `ssh -R` against the SSH gateway. All
three were registered **at once**, against the same origin, each on its own
public port, and hit in rotation with the order shifted every round — because
a sequential ladder on this instance charges the whole burst budget to
whichever arm runs first. Three rounds, 96 MiB over 4 connections per burst,
75 s cooldown. `--carriers 8` for native and docker; the SSH leg is
TCP-relay-only by design (I-SSH2) and therefore runs at one carrier.

Note that 8 is NOT this workload's best carrier count — §7's ladder peaks at 4
on download and 2 on upload, and 8 is the worst rung above 1. The stage is
still internally valid, because both binary flavours run at the same setting
and are compared only with each other; but the native and docker absolute
numbers here are a few per cent below what the same arms reach at 4 carriers,
and the SSH arm's gap to them is correspondingly overstated. The honest
comparison for SSH is against §7's one-carrier rung, and that is how it is
read below.

**Download, MB/s, in the order each round actually ran:**

| round | first | second | third |
| --- | --- | --- | --- |
| 1 | native 274.73 | docker 229.79 | ssh 102.33 |
| 2 | docker 308.18 | ssh 112.68 | native 232.31 |
| 3 | ssh 129.85 | native 259.19 | docker 310.70 |
| **median** | **native 259.19** | **docker 308.18** | **ssh 112.68** |

**Upload, MB/s:**

| round | first | second | third |
| --- | --- | --- | --- |
| 1 | native 240.01 | docker 230.11 | ssh 174.58 |
| 2 | docker 220.20 | ssh 155.25 | native 165.20 |
| 3 | ssh 178.20 | native 224.84 | docker 214.93 |
| **median** | **native 224.84** | **docker 220.20** | **ssh 174.58** |

**Latency, one new connection per probe, 60 probes:**

| flavour | p50 ms | p95 ms | p99 / max ms | errors |
| --- | --- | --- | --- | --- |
| native | 4.691 | 5.521 | 33.767 | 0 |
| docker | 4.658 | 5.355 | 7.655 | 0 |
| ssh | 5.437 | 6.134 | 7.924 | 0 |

All three ended on the relay with `opens=0 fb=0` and no leftover state
(`active=0`). Four of the 126 allowance samples inside P5's window were shaped
(924 `bw_in` misses, no `bw_out`) — the rotation is what keeps those four from
landing on one arm.

**Reading.**

*Native and docker are the same forwarder.* Their download medians differ by
19 % in docker's favour, which looks like a result until the individual rounds
are read: native spans 232–275 and docker 230–311, two heavily overlapping
ranges, and the ordering flips between rounds. Upload puts them 2 % apart
(224.84 vs 220.20) and latency 0.03 ms apart at the median. The honest
statement is that **the dockerized binary costs nothing measurable** — which
is the operationally useful claim, and the one the vhost campaign reached too
(50.03 vs 48.95 MB/s there). The single caveat is not performance but
capabilities: Docker clears every capability for a non-root UID, so the
default uid-1000 image cannot force its UDP socket buffers past
`net.core.*mem_max`; this stage runs the ROOT `:client` image, and P5b is the
arm that checks whether the equivalence survives on the direct path.

*The SSH leg is slower on bulk download, and the comparison needs care.* 112.68
against 259.19 MB/s is 2.3×, but part of that is the carrier count, not SSH:
the SSH arm cannot open carriers and the other two ran at 8. The fairer
reference is P2's own single-carrier relay download, **210.25 MB/s**, against
which SSH is still **1.87× slower** — a real gap, and the known one: an
OpenSSH client's per-channel window caps a single channel, and the gateway's
channel-open path is a second framing layer on top of the relay. On **upload**
the picture is different again: 174.58 MB/s is within 22 % of native's
224.84 and actually above P2's single-carrier relay upload of 154.90, so the
SSH leg's deficit is asymmetric and concentrated in the download direction.

*Latency is where the SSH leg is cheapest to accept.* 5.437 ms p50 against
4.691 is **0.75 ms of extra cost per new connection** — one extra framing
round of work, not an extra round trip. For an operator who cannot install a
binary, that is the whole price for small requests. (Native's 33.767 ms p99 is
a single outlier in 60 probes, with p95 at 5.521; docker and ssh show no such
sample. It is noise, and it is reported rather than trimmed.)

**Where the bottleneck is.** For native and docker on download: the instance,
not the forwarder — both spent the same shaped budget and both land in the
230–310 MB/s band that P2 also reached. For SSH on download: the SSH channel
itself (single carrier plus the OpenSSH window), which is a property of the
transport the gateway exists to support, not a defect.
---

## 11. P6 — concurrency, and what a fresh connection costs behind held ones

`scripts/perf/staging/pub/vm_pub_conc.sh`. The ladder holds 16, 64, 128, 256
and 512 connections open through one public tunnel and, with them held, opens
30 fresh connections and times how long each takes to be answered by the
origin. Held connections are IDLE by construction: they speak the raw origin's
`HOLD` verb, which answers once and then moves no bytes. An arm whose held
connections were downloading would saturate the path and measure the path.

They are held from ONE process (`raw_client.py hold`, asyncio): 512 Python
interpreters on a 2-vCPU / 3.8 GiB VM would measure the driver's own memory
pressure. `active_at_server` is read back from the admin API, so each rung
reports what the SERVER has, not what the driver asked for; when the two
disagree the server's number is the one to trust and the transcript shows both.

This stage was run FOUR times and only the fourth run is quotable. The three
discarded runs are part of the evidence, not an embarrassment to hide:

* run 1 — `pub_conc.INVALID-H7.log`: every rung read `held=N up=0
  active_at_server=0 errs=N`, i.e. nothing was ever held. The `HOLD` verb was
  correct and the VM's origin file was byte-identical to the tree's, but the
  RUNNING origin process had been started before that file was written, so it
  did not know the verb. `start_origins` reused it because `pgrep` cannot
  distinguish "a process matching this name" from "a process running this
  program" (H-7, §17).
* run 2 — `pub_conc.log` in the campaign directory: the rungs were CUMULATIVE
  (`active_at_server` 80, 208, 464 at the 64, 128 and 256 rungs — each the
  running total, H-9), and the relay 512 rung, at ~976 connections actually
  held, is where the SERVER went down: `up=502 active_at_server=? errs=30`,
  with the admin API answering `curl: (35) Send failure` and then `curl: (7)
  Failed to connect … after 2 ms`. That is P-12, and it is the single most
  valuable result this stage produced — see the last paragraph of this section.
* run 3 — ladder clean (H-9 fixed), but the P6b carrier arm ran `1, 4, 8` in a
  fixed order and read `carriers=1` at **9.956 ms** while the ladder above, at
  the same 128 held connections and the same single carrier, read 4.617 ms.
  Two numbers for one configuration means the ORDER was being measured, not
  the configuration: the first arm paid whatever the previous stage had left in
  the instance's allowance bucket. That violates §4's own paired-comparison
  rule, so the arm was rewritten to run TWO rounds in opposite order.
* run 4 — below. Ladder and carrier arm from one run, on one harness, with
  `wait_quiet` proving between rungs that the server's `active` count really
  did return to zero.

```
===== P6 concurrency ladder, relay =====
  baseline (nothing held): n=30 p50=4.704 p95=5.321 p99=7.216 max=7.216 errs=0
  held=16  up=16  active_at_server=16  n=30 p50=4.749 p95=7.282 p99=9.831 errs=0
  held=64  up=64  active_at_server=64  n=30 p50=4.948 p95=5.488 p99=7.620 errs=0
  held=128 up=128 active_at_server=128 n=30 p50=4.762 p95=5.105 p99=7.204 errs=0
  held=256 up=256 active_at_server=256 n=30 p50=4.792 p95=5.131 p99=7.349 errs=0
  held=512 up=512 active_at_server=512 n=30 p50=4.945 p95=8.256 p99=8.542 errs=0
  path=relay opens=0 fb=0

===== P6 concurrency ladder, quic =====
  baseline (nothing held): n=30 p50=4.698 p95=5.708 p99=7.007 max=7.007 errs=0
  held=16  up=16  active_at_server=16  n=30 p50=5.114 p95=7.915 p99=8.180 errs=0
  held=64  up=64  active_at_server=64  n=30 p50=4.711 p95=5.464 p99=7.617 errs=0
  held=128 up=128 active_at_server=128 n=30 p50=4.602 p95=7.461 p99=7.513 errs=0
  held=256 up=256 active_at_server=256 n=30 p50=4.675 p95=5.662 p99=7.449 errs=0
  held=512 up=512 active_at_server=512 n=30 p50=4.700 p95=5.895 p99=7.067 errs=0
  path=direct opens=1157 fb=0
```

| held | relay p50 | relay p95 | relay p99 | quic p50 | quic p95 | quic p99 |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | 4.704 | 5.321 | 7.216 | 4.698 | 5.708 | 7.007 |
| 16 | 4.749 | 7.282 | 9.831 | 5.114 | 7.915 | 8.180 |
| 64 | 4.948 | 5.488 | 7.620 | 4.711 | 5.464 | 7.617 |
| 128 | 4.762 | 5.105 | 7.204 | 4.602 | 7.461 | 7.513 |
| 256 | 4.792 | 5.131 | 7.349 | 4.675 | 5.662 | 7.449 |
| 512 | 4.945 | 8.256 | 8.542 | 4.700 | 5.895 | 7.067 |

All times in milliseconds; 30 fresh connections per rung, `errs=0` everywhere.

**Run 3's ladder is kept as a replication and it agrees.** Its relay rungs read
4.523 / 4.609 / 4.685 / 4.617 / 4.508 / 4.642 and its direct rungs 4.445 /
4.467 / 4.589 / 4.494 / 4.543 / 4.510 — the same flatness at an absolute level
about 0.2 ms lower, and the same 1 157 direct stream opens to the last stream.
Two runs a day apart on the same instance therefore differ by more between
runs (~0.2 ms) than either differs across a 32-fold change in held connections,
which is the cleanest possible statement of "no concurrency knee".

### P6b — carriers under concurrency

The carrier pool exists to break yamux head-of-line blocking. If `--carriers`
does anything for a public tunnel it has to show up here, with 128 connections
held on the relay, and not in the single-stream stages of §8. Two rounds in
opposite order, so that no configuration is permanently first:

```
===== P6b carriers under concurrency (relay only) =====
  round=1 carriers=1 held=128 up=128 active_at_server=128 n=30 p50=5.129 p95=7.577 p99=8.067 errs=0
  round=1 carriers=4 held=128 up=128 active_at_server=128 n=30 p50=4.728 p95=6.344 p99=7.479 errs=0
  round=1 carriers=8 held=128 up=128 active_at_server=128 n=30 p50=4.878 p95=7.189 p99=7.412 errs=0
  round=2 carriers=8 held=128 up=128 active_at_server=128 n=30 p50=4.755 p95=5.357 p99=7.263 errs=0
  round=2 carriers=4 held=128 up=128 active_at_server=128 n=30 p50=4.761 p95=5.506 p99=7.790 errs=0
  round=2 carriers=1 held=128 up=128 active_at_server=128 n=30 p50=4.898 p95=5.532 p99=7.470 errs=0
```

| carriers | round 1 p50 | round 2 p50 | pooled | vs 1 carrier |
| --- | --- | --- | --- | --- |
| 1 | 5.129 | 4.898 | **5.014** | — |
| 4 | 4.728 | 4.761 | **4.744** | 0.946 |
| 8 | 4.878 | 4.755 | **4.816** | 0.961 |

**Carriers change nothing measurable, and the apparent 5 % gain is the same
size as the noise.** The pooled difference between one carrier and four is
0.269 ms. The round-to-round spread of the SINGLE-carrier configuration alone
is 0.231 ms, and the ladder above — the identical configuration, 128 held,
default single carrier — reads 4.762 ms, which sits between the two rounds'
values. A difference that a configuration shows against itself is not a
difference between configurations. The honest statement is that at 128 held
idle connections the carrier pool is neither a win nor a loss for latency,
which matches what the vhost campaign found on a clean path (median c4/c1
0.941 there) and is exactly why `--carriers` defaults to 1.

**The order effect this arm was rewritten to remove is gone.** In round 1 the
single-carrier arm ran first and read 5.129 ms; in round 2 it ran last and read
4.898 ms — a 0.23 ms difference, against the 5.3 ms difference the fixed-order
version produced. The allowance residue a stage leaves behind is real and it
lands on whatever runs next; alternating the order is what keeps it from being
attributed to a flag.

**A fresh connection costs the same behind 512 held connections as behind
none.** Relay p50 moves between 4.704 and 4.948 ms across the whole ladder — a
spread of 0.24 ms, and the two extremes are the ZERO rung and the 64 rung, so
there is not even a monotone trend to interpret. The direct path spans 4.602
to 5.114 ms, and its single widest reading is at the 16 rung, i.e. under the
LIGHTEST load. Neither transport has a concurrency knee anywhere below 512.

**`active_at_server` equals `held` at every rung, and that is load-bearing.**
The column is read back from the admin API rather than trusted from the
driver, and in an earlier run it read 80, 208 and 464 at the 64, 128 and 256
rungs — the running total, because the origin held its half of each connection
after its client had been killed (H-9, §17). A ladder whose rungs overlap is
not a ladder, and the numbers above are only usable because the two counts now
agree at every rung of both transports and of every carrier arm.

**The direct path opened 1 157 streams and fell back zero times.** Every held
connection on the `--udp` arm is its own QUIC bidirectional stream, so the
512 rung alone puts 512 concurrent streams on one connection — and
`direct_fallbacks=0` says the server never had to put one of them on the warm
relay. That answers a question the single-stream stages cannot: the shipped
`--udp-max-streams` default is comfortably above 512 concurrent proxied
connections per tunnel.

**What this does NOT reproduce.** The vhost campaign measured, on this same
instance, a fresh request at 966 ms behind 256 held connections and 1 436 ms
behind 512, against 11 ms flat on a private server (N-9, §12 of the vhost
evidence). The public path shows nothing of the kind: 4.7 ms flat to 512. The
two probes are not the same probe — the vhost one is a new TCP connection plus
a full TLS handshake plus an HTTP GET routed by `Host`, the public one is a new
TCP connection plus one substream open — so this is not a refutation of N-9.
It is one more place where the tail does not appear, and it is consistent with
N-9's surviving hypothesis: whatever produces it is not in the accept path or
in the connection bookkeeping, both of which this ladder exercises hard, 512
connections at a time, with no tail at all.

**And this is where P-12 was found — by the BROKEN version of this stage.**
In run 2 the rungs accumulated, so the relay ladder's 512 rung held
16+64+128+256+512 = 976 connections through one public tunnel instead of 512.
That is 48 short of the deployment's `--max-conns 1024` and, once the server's
own listeners, carrier sockets and the operator's other live tunnels are
counted, right at a soft `RLIMIT_NOFILE` of 1024. The server logged `failed to
accept tunnel connection err=No file descriptors available (os error 24)`
every 100 ms from 06:23:44 UTC, refused 10 of the 512 connections the driver
asked for (`up=502`), and answered NOTHING on its control port for about half
a minute — while `conn_rejections` stayed **0**, because the semaphore's bound
was never reached. §16's P-12 is that event, and §17's H-9 entry is the harness
defect that produced the load.

Two details are worth keeping. First, the direct arm of the SAME run later
reached the same accumulated 976 (`active_at_server=976`, `errs=0`) without
tripping it — so 976 sat exactly at the edge rather than past it, which is
what an unreconciled limit produces: a bound that is reachable on some runs
and not others. Second, this is an argument for keeping invalid runs. A defect
in the harness manufactured a load the corrected harness never reaches, and
that load found a real HIGH defect in the product. Deleting the run would have
deleted the finding.

## 12. P8 — the netem matrix: how each transport degrades

`scripts/perf/staging/pub/vm_pub_netem.sh`, 64 MiB over 4 connections per
cell, both transports registered at once so a cell measures them under the
SAME shaping instance. Shaping is applied with `tc netem` on the VM's egress
interface, i.e. on the **client → server** leg. Because the origin, the
forwarder and the load generator all live on the VM, a `GET` through the
tunnel carries its payload over exactly that leg, so the payload direction is
the shaped one. The return leg (server → load generator) is the server's own
path and this harness cannot touch it — stated explicitly because the numbers
are otherwise read as symmetric.

| condition | relay MB/s | quic MB/s | quic/relay | quic path |
| --- | --- | --- | --- | --- |
| clean | **188.77** | 85.86 | 0.455 | direct |
| loss 1 % | 72.05 | **100.48** | 1.395 | direct |
| loss 3 % | 5.27 | **47.70** | 9.051 | direct |
| loss 10 % | 0.39 | **51.40** | **131.795** | direct |
| delay 40 ms | 4.47 | **12.37** | 2.767 | direct |
| delay 40 ms + loss 1 % | 1.75 | **12.19** | 6.966 | direct |
| delay 100 ms | 3.70 | **4.97** | 1.343 | direct |
| delay 40 ms + reorder 5 %/50 % | **26.03** | 12.29 | 0.472 | direct |

Every cell reports `path=direct`, so no cell silently swapped to the warm
relay and then got quoted as a QUIC number — the column means what it says.
The allowance timeline records **1 shaped sample out of 58** in P8's window
(21 `bw_in` misses, no `bw_out` miss); at these rates the bucket is simply not
the binding constraint.

**Reading.** This is the clearest result in the campaign and it is the mirror
image of §6. On a clean path the relay wins by 2.2×. **From 1 % loss onward
the direct path wins, and the margin explodes**: 1.4× at 1 %, 9× at 3 %,
**132× at 10 %**, where the relay has effectively stopped moving data
(0.39 MB/s) while QUIC still delivers 51 MB/s — more than half its clean-path
rate. The mechanism is the one the carrier pool exists to mitigate and cannot
fully solve: on the relay, four proxied connections are four yamux substreams
inside ONE TCP connection, so a single lost segment stalls **all four** until
it is retransmitted, and the congestion window they share collapses. On the
direct path each proxied connection is its own QUIC stream with per-stream
loss isolation and a single userspace congestion controller that treats the
loss as what it is.

Notice also that QUIC's throughput is nearly **flat** in loss — 100, 48,
51 MB/s at 1 %, 3 % and 10 % — while the relay falls by three orders of
magnitude. A user on a lossy path does not experience "slower"; on the relay
they experience "broken".

**Latency behaves differently from loss.** At 40 ms of one-way delay both
transports collapse to single-digit or low-double-digit MB/s (4.47 relay,
12.37 direct), and at 100 ms they converge (3.70 vs 4.97). This is
bandwidth-delay product, not a transport defect: with four connections on one
carrier the relay is limited by the window of a single TCP connection, and the
direct path by the QUIC stream window. Adding delay multiplies the BDP;
neither side has the window to fill it. The operator remedy at high RTT is
`--carriers N` (more independent windows, §7), not `--udp`.

**The reorder cell must not be over-read.** `netem delay 40ms reorder 5% 50%`
does not add reordering on top of a 40 ms delay: in netem's implementation the
reordered fraction is sent **immediately** instead of being delayed, so the
cell's average delay is materially lower than the `delay 40ms` cell's. The
relay's jump from 4.47 to 26.03 MB/s is mostly that artifact — it is a
BDP effect, not evidence that TCP tolerates reordering better here. The honest
statement the cell supports is narrower: **reordering did not break either
transport**, and the direct path did not fall back.

**What this changes in the recommendation.** §6 said the relay is faster on a
clean path; this section says the clean path is the only place that is true.
The public-tunnel `--udp` switch is therefore not a throughput knob but a
**robustness** knob, and the decision rule is about the network, not about the
workload: if the path between the forwarder and the server loses packets — a
radio link, a congested uplink, a poor VPN, an overseas hop — turn it on and
accept the clean-path cost, because the clean-path cost is 2× and the lossy
path cost is 100×.
---

## 13. P9 — CPU seconds per GiB, per transport

`scripts/perf/staging/pub/vm_pub_eff.sh` plus
`scripts/perf/staging/res/cpu_window.sh`. This is the stage that answers "is
the server application-limited?", and it is the only stage that can: on a
burstable instance the allowance bucket caps the RATE long before the CPU
does, so a rate ceiling proves nothing about the software. CPU seconds per GiB
is invariant to the cap — halve the rate and the same work simply takes twice
as long.

The measurement is bounded IN THE CLIENT (`raw_client.py`'s `window`
argument), so every case observes exactly the same wall-clock window and the
byte count is whatever the path managed inside it. Each case prints
`window=<t0>-<t1> … gib=<n>`; `cpu_window.sh` reduces the server host's
`/proc/stat` samples over the same window to CPU seconds and divides. The
host, not the container: the kernel spends a large part of the networking bill
in `softirq` on the process's behalf, measured at 37-40 % of busy in the vhost
campaign and at about half of busy in a sample window here, and the container's
own accounting does not see it. Steal is reported separately and never counted
as work — on a burstable instance it is the only in-guest evidence that the
credit bucket is throttling, and folding it into "busy" would bill the cloud's
rationing to bore.

The first run of this stage is preserved as `pub_eff.INVALID-H8.log`: nine
cases, every one of them `bytes=0 rate=0.00 MB/s gib=0.000`. H-8 in §17. The
stage below is the re-run against the corrected harness.

```
CASE relay-r1  path=relay  window=1789109464-1789109484 dur=20s bytes=4930488302 rate=235.10 MB/s gib=4.592
CASE quic-r1   path=direct window=1789109497-1789109517 dur=20s bytes=2238972820 rate=106.76 MB/s gib=2.085
CASE relay8-r1 path=relay  window=1789109530-1789109550 dur=20s bytes=7617847983 rate=363.25 MB/s gib=7.095
CASE relay-r2  path=relay  window=1789109563-1789109583 dur=20s bytes=4859414863 rate=231.71 MB/s gib=4.526
CASE quic-r2   path=direct window=1789109596-1789109616 dur=20s bytes=2450618530 rate=116.85 MB/s gib=2.282
CASE relay8-r2 path=relay  window=1789109629-1789109649 dur=20s bytes=7298139625 rate=348.00 MB/s gib=6.797
CASE relay-r3  path=relay  window=1789109661-1789109682 dur=21s bytes=5511400116 rate=250.29 MB/s gib=5.133
CASE quic-r3   path=direct window=1789109694-1789109715 dur=21s bytes=2094349324 rate=95.11  MB/s gib=1.951
CASE relay8-r3 path=relay  window=1789109727-1789109747 dur=20s bytes=6067058741 rate=289.30 MB/s gib=5.650
```

Reduced against the server host's `/proc/stat` over exactly those windows
(`res/cpu_window.sh`, joined automatically by `pub/rerun_stage.sh`):

| case | path | GiB | busy s | **CPU s/GiB** | cores busy (of 2) | steal s |
| --- | --- | --- | --- | --- | --- | --- |
| relay-r1 | relay | 4.592 | 31.42 | **6.84** | 1.57 | 0.01 |
| relay-r2 | relay | 4.526 | 34.86 | **7.70** | 1.74 | 0.91 |
| relay-r3 | relay | 5.133 | 36.16 | **7.04** | 1.81 | 0.01 |
| relay8-r1 | relay | 7.095 | 36.37 | **5.13** | 1.91 | 0.01 |
| relay8-r2 | relay | 6.797 | 36.37 | **5.35** | 1.91 | 0.73 |
| relay8-r3 | relay | 5.650 | 35.01 | **6.20** | 1.84 | 0.02 |
| quic-r1 | direct | 2.085 | 26.61 | **12.76** | 1.48 | 0.04 |
| quic-r2 | direct | 2.282 | 31.12 | **13.64** | 1.56 | 0.01 |
| quic-r3 | direct | 1.951 | 26.49 | **13.58** | 1.39 | 3.25 |

Medians: **relay one carrier 7.04**, **relay eight carriers 5.35**, **QUIC
direct 13.58** CPU seconds per GiB.

Two windows were also reduced with the per-process stream, to answer "whose
CPU is it?":

```
relay8-r1  busy=36.37 user=8.20 sys=8.75 softirq=19.42 steal=0.01 cores_busy=1.91
           cpu_s_per_gib=5.13   process: bore_cpu_s=35 bore_peak_rss_kb=20252
quic-r2    busy=31.12 user=6.78 sys=5.97 softirq=18.37 steal=0.01 cores_busy=1.56
           cpu_s_per_gib=13.64  process: bore_cpu_s=23 bore_peak_rss_kb=19048
```


**The direct path costs 1.93× the CPU of the relay, per byte** — 13.58 against
7.04 CPU seconds per GiB at the same single carrier. That is the mechanism
behind §6: the relay is not faster because QUIC is slow on the wire, it is
faster because this host cannot afford QUIC's per-byte cost at the rate the
relay reaches.

**And the prediction closes.** One core at 13.58 s/GiB delivers
1/13.58 GiB/s = 0.63 Gbit/s; the direct arms ran at 1.39-1.56 cores, which
predicts 0.88-0.98 Gbit/s, i.e. **105-117 MB/s** — and the measured rates are
95.11, 106.76 and 116.85 MB/s. The same arithmetic on the relay: 5.35 s/GiB is
1.61 Gbit/s per core, 1.91 cores predicts 3.07 Gbit/s = 366 MB/s, measured
363.25. Two independent quantities agreeing to within a percent is what makes
this the campaign's load-bearing result: **the public direct path's ceiling on
this instance is the CPU, not the network, and it is a factor of two away from
the relay's.**

**`cores_busy` 1.39-1.91 of 2 says the server was at or near saturation in
every window**, so these are not idle-server microbenchmarks: they are the
cost at the rate the path actually delivers.

**Eight carriers cost LESS per byte than one** (5.35 against 7.04 s/GiB) while
delivering more — 348-363 MB/s against 232-250. More parallelism at a lower
unit cost is the signature of fixed per-transfer work being amortised across
more in-flight bytes, and it is the CPU-side confirmation of §7's carrier
ladder: the ladder says 4 carriers is 1.44× faster, this says it is also
cheaper.

**Roughly half the bill is kernel softirq** (19.42 of 36.37 on the relay arm,
18.37 of 31.12 on the direct arm — 53 % and 59 %), which is exactly why this
stage measures the HOST and not the container: a container-only reading would
have understated the cost by more than half and would have answered the
application-limit question wrongly.

**The CPU is bore's own.** In the `relay8-r1` window the host was busy 36.37
CPU-seconds and the `bore` process' own cputime advanced 35 — 96 % of it. The
two accountings are not disjoint by mistake: Linux charges softirq processed
in a task's context to that task's system time *and* to the CPU's softirq
bucket, so the process figure properly includes the kernel networking done on
its behalf. There is no noisy neighbour here to blame and no unexplained
remainder: at 350 MB/s this 2-vCPU instance is running bore, flat out.

**Steal is reported and excluded from `busy` on purpose.** It is the only
in-guest evidence of the credit bucket throttling, and the one window where it
is large (`quic-r3`, 3.25 s) is also that arm's slowest rate, 95.11 MB/s — so
the exclusion is not cosmetic: folding steal into `busy` would have billed
Amazon's rationing to bore and inflated that case's CPU s/GiB by 12 %.

**Practical reading for sizing.** To sustain a target rate on the public
relay path, budget `rate_GiB_per_s × 5.4` cores at eight carriers or
`× 7.0` at one; on the direct path, `× 13.6`. A 10 Gbit/s public relay needs
about 6.2 cores of this class; the same rate on the direct path needs about
16.

### 13.1 P-13 — the direct path was paying for bytes it threw away

The reading above was written as if 13.58 CPU s/GiB were the price of QUIC.
It is not. The gap has a cause, the cause is a one-line defect, and it was
found by asking the obvious follow-up question: **softirq is per-PACKET kernel
work, so does the direct path really move three times the packets for the same
bytes?** The path MTU is 1500 in both directions, so it should not have to.

Server-side accounting from the server's own interface (`/proc/net/dev` on
`ens5`, read across one 20 s window per transport, in-region consumer, four
connections, `pub/pktrate.sh`):

| arm | delivered | tx bytes | tx packets | avg tx | **rx bytes** | rx packets |
| --- | --- | --- | --- | --- | --- | --- |
| relay | 212.70 MB/s | 4.360 GiB | 3 331 128 | 1405 B | **4.352 GiB** | 3 484 122 |
| direct | 111.26 MB/s | 2.279 GiB | 1 685 178 | 1452 B | **3.885 GiB** | 3 547 519 |

Packets per delivered GiB are nearly identical — 764 075 on the relay against
739 363 on the direct path — so the answer to the question is no, and the
per-packet cost was never the anomaly. The anomaly is in the last two columns.
The relay takes in 4.352 GiB to hand out 4.360: inbound and outbound agree to
0.2 %, as a relay's must. The direct arm takes in **3.885 GiB to hand out
2.279** — 1.78 times as many bytes IN as OUT. Decomposing the inbound mix at
1452 B for data and ~50 B for acknowledgements puts it at about 2.85 million
data packets received to deliver 1.69 million. The server was being sent
roughly 1.8 data packets for every one it delivered.

Retransmission is the only thing that shape can be, and it explains all three
of the direct path's numbers at once: half the goodput, twice the CPU per
*delivered* GiB, and 2.9× the softirq per delivered GiB (8.05 s/GiB against
2.74). The work was real. It was being spent on bytes that were dropped and
sent again.

**Where they were dropped.** `holepunch::configure_udp_socket_buffers` exists
precisely for this, and its own doc comment has warned since it was written
that an untuned UDP socket caps a congestion-controlled QUIC flow at roughly
`buffer / RTT`. Calling it was the CALLER's job — and the one caller that
builds the server's shared QUIC endpoint never did. That endpoint is not a
minor one: it is the single UDP socket that receives the direct-path bytes of
every vhost, public and ssh-jump tunnel in the process, and on a download it
is the RECEIVING side, because the bytes arrive from the provider over QUIC
and leave over the public TCP socket. Every other UDP socket in the file asked
for 16 MiB. This one ran on `net.core.rmem_default`.

Read from the kernel, not from a log, on a server built from the campaign's
own tree:

```
$ ss -uapm | grep -A1 ':17902'
UNCONN 0 0 0.0.0.0:17902 0.0.0.0:* users:(("bore",pid=3478904,fd=11))
   skmem:(r0,rb212992,t0,tb212992,f0,w0,o0,bl0,d0)
```

**208 KiB**, and the server said nothing about it — there was nothing to say,
because it never asked for more. For comparison, the client end of the very
same connection logs `effective_recv=8388608`: the two ends of one QUIC path
differed by a factor of forty, and the small one was the receiver. At the
measured 111 MB/s a 208 KiB socket buffer holds **1.9 ms** of traffic, so any
scheduling delay longer than that is a drop, and the staging container carries
no `CAP_NET_ADMIN` (`caps=[]`) so `SO_RCVBUFFORCE` was never an option either.

**The fix** moves the call INSIDE `client_endpoint` and `server_endpoint` and
removes it from all four call sites. That is deliberate, and it is the point:
a third call site would have fixed this instance, while putting it in the
constructors makes the invariant structural — there is no path to a QUIC
`Endpoint` in this codebase that does not pass through one of those two
functions, so no future endpoint can be built over an untuned socket. Measured
after, same command, same host:

```
   skmem:(r0,rb8388608,t0,tb8388608,f0,w0,o0,bl0,d0)
```

and the server now says so, with the remedy for the part it cannot fix itself:

```
WARN bore_cli::holepunch: UDP socket buffer clamped below request — direct-path
throughput will be limited to roughly buffer/RTT. Run with CAP_NET_ADMIN
(privileged) for SO_*BUFFORCE, or raise net.core.rmem_max and net.core.wmem_max
(e.g. sysctl -w net.core.rmem_max=16777216 net.core.wmem_max=16777216)
requested_recv=16777216 effective_recv=8388608 ... recv_forced=false
```

Gate `T-PUB-UDPBUF` in `scripts/perf/public_idle_window.sh` starts a real
server with `--udp` in a namespace and reads `rb`/`tb` back out of `ss -uapm`,
asserting both are well clear of `net.core.rmem_default` and that the server
reported what it got. It is red-checked: with the call removed it reports
`rb=212992 tb=212992`, `server said: nothing`, and fails all three assertions.
The threshold is expressed against the host's own default rather than a fixed
number, because the effective size is whatever `net.core.rmem_max` allows.

**What was proven before the redeploy, and what was left open.** Proven: the
socket ran at the kernel default, the fix raises it forty-fold, the server was
silent and now is not, and the gate fails without the fix. Also proven: the
direct arm received 1.78× the bytes it delivered while the relay arm's agreed
to 0.2 %. NOT proven at that point: that the first causes the second. The
inference was strong — a receive buffer holding 1.9 ms of traffic is the kind
of thing that drops datagrams under load, and nothing else in the measurement
moves — but the staging server still ran the pre-fix build, so the "after"
figures were recorded as PENDING rather than predicted, which is the only
honest thing to do with a causal claim that has not been tested.

### 13.2 The "after": the inference held

The server was redeployed on `1.0.0 - main - 062a1095` and the same probe was
run again, unchanged, on the same path with the same 20 s windows and the same
four connections. `srv/verify_fixes.sh` first confirmed the fix was actually in
force in the running process, read from the kernel and not from a log line:

```
### P-13 — the shared QUIC endpoint's socket buffers were configured
  --vhost-quic-port: 443
  socket: rb=8388608 tb=8388608   net.core.rmem_default=212992
PASS: P-13: receive buffer 8388608 is far above the untuned default 212992
PASS: P-13: send buffer 8388608 is far above the untuned default 212992
```

8 MiB rather than the 16 MiB requested, because this host's
`net.core.rmem_max` is 8 MiB and an unprivileged process cannot pass it — the
clamp warning says exactly that and names the sysctl. It is still **forty
times** what the socket had. Then `pub/pktrate.sh`:

```
=== server-side packet accounting on ens5, 20 s windows, 4 conns ===
  relay path=relay   bytes=4212265221 secs=20.000 MBs=200.85 conns=4 errs=0
  relay   tx: 4.119 GiB  3164995 pkt  avg 1397 B/pkt  -> 768463 pkt/GiB
  relay   rx: 4.142 GiB  3358732 pkt  avg 1324 B/pkt
  direct path=direct bytes=3008083245 secs=20.000 MBs=143.43 conns=4 errs=0
  direct  tx: 2.934 GiB  2129157 pkt  avg 1480 B/pkt  -> 725688 pkt/GiB
  direct  rx: 2.960 GiB  2779571 pkt  avg 1143 B/pkt
```

| quantity | before (`dbcc645a`) | after (`062a1095`) |
| --- | --- | --- |
| direct, bytes in ÷ bytes out | **1.78×** | **1.009×** |
| relay, bytes in ÷ bytes out | 1.002× | 1.006× |
| direct goodput, 4 conns | 111 MB/s | **143.43 MB/s** (+29 %) |
| relay goodput, 4 conns | 213 MB/s | 200.85 MB/s |

Both ends of this probe are the same in-region VM, so a healthy server takes
in almost exactly what it puts out: **1.0 is the correct value and the relay
arm is the control that says so**. The direct arm now reads 1.009 against the
relay's 1.006 — the 1.78× is gone, not reduced. The inference held: the
untuned receive socket was dropping datagrams and QUIC was resending them, and
that single mechanism was paying for the missing goodput.

What the fix did NOT do is reverse the transport ranking. The relay still
moves more on this clean in-region path (143.43 against 200.85, a ratio of
0.714) — but the gap is now 0.71 where it was 0.52, so roughly **two fifths of
the deficit the campaign attributed to QUIC was this defect** and the rest is
QUIC. The `--udp` recommendation therefore stands on its merits rather than on
a defect, which is what the PROVISIONAL mark existed to check; §18 records the
re-measured form.

One number is worth keeping in view for whoever reads the packet columns: the
direct path's average on-wire packet is now **1480 B**, essentially the full
1500-byte MTU, against the relay's 1397 — QUIC is not sending small datagrams,
and the remaining CPU difference is not fragmentation.

The sizing advice above (`× 13.6` cores for the direct path) was the cost of
the DEFECT, not the cost of QUIC; §13.3 carries the re-measured figure.

**What changed between the two measurements, and what did not.** Exactly one
thing changed: the SERVER build. `BUILD.txt` of the re-run records it —

```
version_field=1.0.0 - main - 062a1095
bore 1.0.0 - main - 062a1095
vm_binary=bore 1.0.0 - main - dbcc645a
```

— the forwarder on the VM is still the build the "before" numbers were taken
with. That is deliberate and it is the strongest form this comparison can
take: the client end of the QUIC path already configured its own socket before
P-13 (it logged `effective_recv=8388608` in the "before" run), so leaving it
untouched isolates the server-side socket as the only variable. Same VM, same
instance, same origin, same probe, same windows.

### 13.3 The "after": CPU per GiB

The §13 efficiency stage was re-run twice against the fixed server, through
`pub/rerun_stage.sh eff`, which starts the host samplers, runs the stage
serially on the VM and joins each case's window to the host's own `/proc/stat`
samples — so the figure includes the softirq the host kernel spends on the
container's behalf, which reading the container alone understates by about a
third.

| run | direct, CPU s/GiB | relay, CPU s/GiB | relay `--carriers 8` |
| --- | --- | --- | --- |
| A (r1/r2/r3) | 9.63 / 10.55 / 10.70 | 7.67 / 7.83 / 6.69 | 5.42 / 6.38 / 5.54 |
| B (r1/r2/r3) | 10.77 / 9.04 / 10.64 | 6.97 / 5.90 / 7.89 | 5.41 / 4.64 / 5.28 |
| median of all six | **10.60** | **7.32** | **5.35** |

| quantity | before (`dbcc645a`) | after (`062a1095`) |
| --- | --- | --- |
| direct, CPU s/GiB | 13.58 | **10.60** (−22 %) |
| relay, CPU s/GiB | 5.35 – 7.04 | 7.32 |
| direct ÷ relay | ≈ 2.1× | **1.45×** |
| direct goodput, this stage | 111 MB/s | **150.74 MB/s** (+36 %) |
| relay goodput, this stage | 213 MB/s | 238.39 MB/s |

Read the RATIO, not the absolute relay figure. This is a burstable instance
whose allowance state and steal time differ from day to day — the steal column
of the joined table ranges from 0.00 to 3.47 s across the eighteen cases — and
the relay arm moved within that band while the direct arm moved outside it.
The honest statement is the one the ratio makes: the direct path used to cost
about **twice** what the relay cost per delivered GiB, and now costs about
**one and a half times**. The remaining 1.45× is the real price of doing
congestion control and packet handling in user space; the rest was the defect.

Two runs rather than one on purpose: the first was collected by hand after
`rerun_stage.sh` wedged (H-13, §17), and a single run of a stage this exposed
to instance noise is not a result. The two agree to within the spread of their
own rounds, which is what makes the −22 % readable.

One thing worth stating so it is not mistaken for the same defect: the TCP
sockets are correct as they are. `shared::tune_tcp` sets `TCP_NODELAY` and
`SO_KEEPALIVE` and deliberately does NOT set `SO_SNDBUF`/`SO_RCVBUF`, because
setting them disables the kernel's TCP buffer auto-tuning and clamps the
socket to `net.core.*mem_max` — that proposal was examined and rejected as
HARMFUL in the SSH-gateway assessment. UDP has no equivalent auto-tuning,
which is the whole reason `configure_udp_socket_buffers` exists. The asymmetry
is correct; the missing call was not.

## 14. P7 — stability, recovery and leaks

`scripts/perf/staging/pub/vm_pub_stab.sh`. Throughput is not the subject
here: every arm asks a yes/no question that a benchmark cannot answer, and
three of them are the field gates for defects this campaign fixed.

These are the same four arms §3 used for the before/after comparison, re-run
inside the campaign proper against the deployed build. The verdicts are
identical; the absolute times differ by a second here and there because they
are a different run on a shared instance, which is exactly why the arms assert
on the deadline and not on a stopwatch reading.

| arm | question | result |
| --- | --- | --- |
| S2 recover | does a tunnel that loses the UDP path get it back, in place? | fell back to the relay after **12 s**, direct path back **4 s** after the block was lifted — PASS |
| S3 reconnect | is the public port released and re-grantable after a hard client death? | released after **0 s**, and a fresh tunnel took the same port — PASS |
| S4 zombie | is a wedged-but-TCP-alive client reaped? | port reaped after **60 s**, exactly the production deadline — PASS |
| S5 churn | do permits and connections come back after 500 short connections? | 500 connections in **2 s**, `active` 0 → 0, `conn_rejections=0`, `direct_budget_refusals=0`, and the tunnel still served 84.77 MB/s afterwards — PASS |

**S2 is the P-7 gate on the real deployment.** The pre-fix behaviour is
recorded in §3: the direct path never came back, for the life of the tunnel.
Here it comes back in 4 s, which is one renewal round trip. Note the shape of
the two numbers: falling back takes 12 s (the client has to notice, which
costs about one QUIC idle timeout) and recovering takes 4 s (the client asks
and the server — now that it reads the control substream at all — answers).
`fb=3` afterwards confirms the three fallbacks were counted, which is the P-2
observability fix doing its job in the same arm.

**S4 is the P-4 gate.** 60 s is not "about a minute": it is
`public_ctrl_timeout`'s default, and the arm waits up to 150 s precisely so
that a PASS at 60 s cannot be confused with a lucky timing. Before P-4 the
port was held until the server restarted.

**S5 says there is no permit or fd leak on the connection path.** `active`
returning to 0 is the weak half of the claim; the strong half is
`conn_rejections=0` — the `--max-conns` semaphore never had to refuse, which
it would have done if permits had been leaking across 500 connections.

### S1 — the 60-minute soak

The soak is the arm that answers the leak question over time rather than over
a burst. One `--carriers 1 --udp` public tunnel, held for a full hour, moving
8 MiB every 25 s and reading the server's own view of the tunnel back from the
admin API on every sample.

```
  t   port opens fb  pool path  active  8MiB_MBs
  1    9057 2 0 1 direct 0 92.13
  2    9057 3 0 1 direct 0 32.33
  …
  142  9057 143 0 1 direct 0 39.28
  143  9057 144 0 1 direct 0 36.73
  samples=143 on_relay=0 port_start=9057 port_end=9057
  server RSS start=15433728 end=15437824 delta=4096 bytes
  PASS: the public port never moved
```

| what was watched | over 143 samples / 62 minutes |
| --- | --- |
| public port | 9057 at the first sample, 9057 at the last — never moved |
| path | `direct` on **every** sample; `on_relay=0` |
| `direct_fallbacks` | **0** for the whole hour |
| `direct_pool` | 1 (the requested carrier) on every sample, never short |
| `direct_stream_opens` | 2 → 144, i.e. **+142 for 143 proxied connections** — one direct stream per connection, none of them relayed |
| server RSS | 15 433 728 → 15 437 824 bytes = **+4 096 bytes**, one page, after ~1.14 GiB moved |
| throughput | p50 37.21 MB/s, mean 36.42, min 18.41, max 92.13 |

**The RSS line is the result.** A leak on this path would show as RSS climbing
with the work done; 143 connections and 1.14 GiB later the server had grown by
a single page, which is allocator noise and not a trend. The number is quoted
rather than judged against a threshold, for one honest reason: this server also
carries the operator's own unrelated tunnels, so a tight threshold here would
be measuring them too. What makes the page-sized delta meaningful anyway is
that everything else moved — 143 connections opened and closed, 142 QUIC
streams opened — while RSS did not.

**`direct_stream_opens` +142 with `direct_fallbacks` 0 is the second result.**
It says the direct path was not merely negotiated once at the start and then
quietly abandoned: every single one of the 143 connections rode it. That
distinction is exactly what P-2's per-tunnel counters were added to make
visible, and exactly what a server-wide counter cannot say.

The throughput column is NOT a throughput measurement and must not be read as
one: 8 MiB on one connection is over before TCP or QUIC has finished ramping,
so p50 37 MB/s here is the cost of a short transfer, not a ceiling — the
ceiling is §6's 115-133 MB/s on the same path. Its job in this table is to
prove the tunnel kept working, and its spread (18-40 MB/s, with the 92 MB/s
first sample warm from the registration probe) is the instance's allowance
bucket breathing, the same drift §4 requires paired comparisons for.
---

## 15. The workstation topology

`scripts/perf/staging/pub/ws_pub.sh`. Every stage up to here has the consumer
inside AWS, in the server's own region, which is what makes the tunnel and not
the network the thing being measured. It is not, however, how a public tunnel
is used: in the field the forwarder runs on somebody's machine and the consumer
is outside AWS on an ordinary link. This stage keeps the forwarder on the test
VM and moves the CONSUMER to this workstation, on a domestic wireless link.

These numbers must never be quoted alongside the VM-side ones: here the link
is the bottleneck, and the measurements below prove it rather than assume it —
a single connection already saturates it. The vhost campaign put this
workstation's radio link at ~45 MB/s; that figure needs correcting in one
direction, because this stage measured 68-72 MB/s UPLOAD against 26-46 MB/s
download, with the instance's allowance counters reading zero in both
directions. The cap is the radio's and it is asymmetric, in the opposite sense
to what a domestic line usually is.

What this topology is good for is LATENCY and proving the path works at all
from outside AWS. It turned out NOT to be good for the transport RATIO either,
which the earlier version of this section claimed: see the reading below, where
two candidate mechanisms for a ratio difference were proposed and both were
falsified, and the difference itself then disappeared when the measurement was
isolated at one connection.

The RTT line is a TCP handshake to the control port, not `ping`: ICMP to this
gateway is filtered, and the first version of this stage printed an empty
string and then `n/a` (H-12). The preflight is parsed rather than discarded —
a registered tunnel proves the CONTROL path and says nothing about the DATA
path, which is how the first run of this stage published a whole table of
zeros in the exact format of a real measurement.

```
  origin on the VM: 313849
### workstation consumer -> AWS server -> VM forwarder — 2026-09-11T09:49:12+02:00
  TCP handshake RTT to the gateway: min=37.36ms median=40.15ms max=46.28ms
  preflight port=9021: bytes=1048576 secs=0.184 MBs=5.44 Mbit=46 conns=1 errs=0
  preflight port=9022: bytes=1048576 secs=0.279 MBs=3.58 Mbit=30 conns=1 errs=0
  relay=9021 quic=9022 path=direct pool=1

===== W1 paired transport A/B from the workstation, download =====
  pair   relay_MBs   quic_MBs    ratio  quic path
  1          30.60      38.36    1.254  direct
  2          27.46      33.84    1.232  direct
  3          25.51      27.34    1.072  direct
  4          23.71      29.50    1.244  direct
  median quic/relay: 1.238

===== W2 upload =====
  pair   relay_MBs   quic_MBs    ratio
  1          71.64      67.81    0.947
  2          70.43      62.89    0.893
  3          72.43      68.46    0.945
  4          67.74      74.50    1.100
  median quic/relay: 0.946

===== W3 latency, one new connection per probe =====
  relay: n=80 p50=63.512 p95=72.751 p99=95.371 max=95.371 errs=0
  quic : n=80 p50=63.483 p95=71.827 p99=73.419 max=73.419 errs=0

===== W4 server view =====
  port=9021 path=relay carriers=1 opens=0 fb=0 pool=0
  port=9022 path=direct carriers=1 opens=113 fb=0 pool=1
DONE
```

Four more download pairs were then run on their own (`pub/ws_dl.sh`), because
four pairs all pointing one way is a sign test at p=0.0625 — suggestive, not
settled:

```
  pair    relay_MBs   quic_MBs    ratio  quic path
  1           26.76      18.58    0.694  direct/o=4/fb=0
  2           16.71      43.57    2.607  direct/o=8/fb=0
  3           25.50      27.95    1.096  direct/o=12/fb=0
  4           17.72      32.16    1.815  direct/o=16/fb=0
  median quic/relay: 1.456
```


**What this topology can and cannot measure.** It cannot measure throughput,
and the stage now carries the evidence for that rather than the assumption.
Pooling both download runs gives eight pairs: ratios 1.254, 1.232, 1.072,
1.244, 0.694, 2.607, 1.096, 1.815 — median 1.238, but a spread of **3.76×**
between the smallest and the largest. The same relay configuration, measured
seven times across one hour, read 16.71, 17.72, 23.71, 25.50, 25.51, 27.46 and
30.60 MB/s, a 1.83× spread, and later in the same hour 33.70, 36.16 and
38.83 MB/s on the same code and the same tunnel. The pairing is what makes any
of it usable — both arms measured back to back, 45–75 s apart, order
alternating — and even then it buys only the sign, not the size.

**Two mechanisms were proposed for the download reversal and both were
falsified by measurement.** The in-region verdict is that the relay wins
(§6); here the direct path led in seven of the eight pairs, so something had
to be different about a distant consumer.

* *Single-carrier head-of-line.* On the relay path all four proxied
  connections ride ONE TCP carrier as yamux substreams, so a 40 ms consumer
  draining slowly could make them share one congestion window. Prediction:
  `--carriers 4` closes the gap. Measured (`pub/ws_carr.sh`, both tunnels up
  simultaneously, order alternating, three rounds): c4/c1 = 0.754, 0.889,
  1.027 — **median 0.889**. Carriers do not close the gap, they cost. Same
  sign as the in-region measurement and as the vhost campaign's 0.941.
* *A per-connection window.* The bytes cross a yamux substream whose window
  auto-tunes to the BDP of the yamux link — the in-region 1 ms hop — while the
  40 ms RTT sits on the LAST hop, downstream of it, so the window has no
  reason to grow for a consumer 40 ms away. Prediction: the AGGREGATE rate
  scales with the NUMBER of connections while the per-connection rate stays
  flat. Measured (`pub/ws_conns.sh`, 24 MiB per connection held constant so
  every cell pays the same ramp):

  | conns | relay aggregate | relay per conn | quic aggregate | quic per conn |
  | --- | --- | --- | --- | --- |
  | 1 | 35.23 | 35.23 | 33.38 | 33.38 |
  | 2 | 36.13 | 18.07 | 45.58 | 22.79 |
  | 4 | 27.92 | 6.98 | 19.21 | 4.80 |
  | 8 | 29.30 | 3.66 | 31.94 | 3.99 |

  The aggregate is flat — falling, if anything — and the per-connection rate
  falls as 1/N. **One connection already saturates the link**, so there is no
  per-connection bound in the tunnel to find, and the link is the bottleneck
  in every cell.

**And that falsifies the reversal itself.** If one connection saturates the
link, and if the consumer-facing hop is plain TCP for BOTH transports — which
it is; the transport choice only affects the in-region forwarder↔server hop —
then the transport cannot matter here, and a difference at four connections is
a property of the four-connection regime on a variable radio link, not of the
tunnel. Isolated at ONE connection, six pairs, 96 MiB each, order alternating
(`pub/ws_dl1.sh`): ratios 1.039, 0.690, 1.121, 1.130, 0.847, 0.837 — **median
0.943**, three above one and three below. The reversal disappears. It is
recorded here as an artifact, and it must not be quoted as a transport verdict
for distant consumers.

**The link's own asymmetry is the link's.** Pulling reads ~26-35 MB/s and
pushing reads ~68-72 MB/s through the same tunnel in the same minutes. The
obvious suspect was the instance's outbound allowance, since a download is
server-OUTBOUND and that bucket is the confounder this whole campaign is built
around — so it was measured instead of assumed (`pub/ws_asym.sh`, the server's
`bw_in_allowance_exceeded` and `bw_out_allowance_exceeded` read immediately
before and after each arm):

```
  download  bytes=100663296 secs=2.777 MBs=34.57 conns=4 errs=0
            allowance delta: bw_in_exceeded=0 bw_out_exceeded=0
  upload    bytes=100663296 secs=1.583 MBs=60.64 conns=4 errs=0
            allowance delta: bw_in_exceeded=0 bw_out_exceeded=0
```

**Zero in both directions, in both arms.** The instance never ran out of
credit here, because this radio link (≈290–510 Mbit/s) stays below what the
bucket shapes — the dominant confounder of every VM-side stage is simply
absent in this topology. The asymmetry belongs to the workstation's own link.

**What this topology does measure, cleanly, is latency.** Eighty probes per
transport, each one a NEW connection:

| transport | p50 | p95 | p99 | errors |
| --- | --- | --- | --- | --- |
| relay | 63.512 ms | 72.751 ms | **95.371 ms** | 0 of 80 |
| direct | 63.483 ms | 71.827 ms | **73.419 ms** | 0 of 80 |

The medians agree to **0.03 ms** — at a 40 ms RTT the transport choice is
invisible at the median, which is the useful half of the result: a public
tunnel to a distant consumer costs what the network costs. The tails do not
agree: the relay's p99 is 21.95 ms worse. That is one probe in a hundred and
is reported as an observation, not a recommendation — the direct path's own
in-region tail (§8) points the other way.

The 63.5 ms median against a 40.15 ms handshake RTT is the whole cost
accounted for: one TCP handshake to the server, then one substream or stream
open to the forwarder and back, which is the second round trip. There is no
TLS on a public tunnel port unless the tunnel asked for `--https`.

**And the path works from outside AWS, confirmed rather than assumed.** 113
direct stream opens, **zero** fallbacks, `pool=1`, `path=direct` on every read
of the QUIC tunnel throughout, while the relay tunnel reported `opens=0` — so
each arm above ran on the transport its label claims.


---

## 16. The defect register in full

One row per finding, in the order of the summary table in §2. Every row
names the evidence that produced it and the gate that keeps it closed; the
"red-checked" rows are the ones where the fix was reverted and the gate
confirmed to fail (or, for P-9, to HANG, which is the honest failure mode of
an unbounded write).

| id | sev | status | finding |
|----|-----|--------|---------|
| P-13 | **HIGH** | FIXED, red-checked | **The server's shared QUIC endpoint ran on an untuned UDP socket.** `holepunch::configure_udp_socket_buffers` — whose own doc comment warns that an untuned socket caps a congestion-controlled QUIC flow at roughly `buffer / RTT` — was the CALLER's responsibility, and the one caller that builds the server's shared endpoint (`vhost_server_endpoint`, which serves the direct path of every vhost, public and ssh-jump tunnel in the process) never called it. Every other UDP socket in the file asks for 16 MiB; that one ran on `net.core.rmem_default`. Read from the kernel on a server built from this tree: `skmem:(r0,rb212992,t0,tb212992,...)` — **208 KiB, silently**, because it never asked for more, while the CLIENT end of the same QUIC path logs `effective_recv=8388608`. A factor of forty between the two ends of one connection, with the small end being the RECEIVER: on a download the bytes arrive from the provider over QUIC and leave over the public TCP socket. At the measured 111 MB/s, 208 KiB is **1.9 ms** of traffic, and the staging container carries `caps=[]` so `SO_RCVBUFFORCE` was unavailable too. Found by following the §13 CPU gap to its cause: server-side interface accounting shows the direct arm taking in **3.885 GiB to deliver 2.279** (1.78×) while the relay arm's inbound and outbound agree to 0.2 %, with packets-per-delivered-GiB nearly identical between transports (739 363 vs 764 075) — so the cost was never per-packet overhead, it was ~1.8 data packets arriving for every one delivered. That single shape accounts for all three of the direct path's numbers: half the goodput (111 vs 213 MB/s), twice the CPU per delivered GiB (13.58 vs 5.35-7.04) and 2.9× the softirq per delivered GiB (8.05 vs 2.74). FIX: the call moves INSIDE `client_endpoint` and `server_endpoint` and is removed from all four call sites — a third call site would have fixed this instance, whereas the constructors make the invariant structural, since no path to a QUIC `Endpoint` bypasses them. AFTER: `rb8388608 tb8388608` (40×) plus the clamp warning naming the `net.core.rmem_max` remedy for the part the process cannot fix without `CAP_NET_ADMIN`. Gate `T-PUB-UDPBUF` reads `rb`/`tb` out of `ss -uapm` on a real `--udp` server in a namespace — the kernel's view, never the log, per P-12's rule — and is red-checked: without the call it reports `rb=212992`, `server said: nothing`, and fails all three assertions. AFTER THE REDEPLOY (`062a1095`, §13.2, the same probe unchanged): the inbound inflation is **gone, not reduced** — direct now takes in 2.960 GiB to deliver 2.934 (**1.009×**) against the relay's 1.006×, and direct goodput rose from 111 to **143.43 MB/s** (+29 %) while the relay's stayed at 200.85. The causal claim that the untuned receive socket was the retransmission is therefore tested, not inferred. It did NOT reverse the transport ranking: direct/relay went from 0.52 to 0.714, so roughly two fifths of the deficit the campaign had attributed to QUIC was this defect and the rest is QUIC. The TCP side is deliberately NOT symmetric: `tune_tcp` sets no `SO_*BUF` because that disables TCP auto-tuning (examined and rejected as harmful in the SSH-gateway assessment); UDP has no auto-tuning, which is why this function exists. |
| P-7 | **HIGH** | FIXED + gated | **`Server::serve_tunnel` never read the control substream at all.** Its `select!` had only `heartbeat.tick()`, `recv_carrier` and `listener.accept()` — no `control.recv()` arm — so every `ClientMessage` a public client sent was left unread in the yamux buffer. Visible effect: a public `--udp` tunnel that lost its direct path sent exactly one `PublicUdpRenew`, waited for an answer that could not come, and **stayed on the TCP relay for the rest of the control connection's life** (measured: still degraded 100 s after the UDP path healed). The vhost provider loop (`vhost.rs:1211`) and the SSH-jump loop (`ssh_jump.rs:624`) both answered their own renewal already. FIX: added the recv arm + `send_public_udp_offer()` shared by the first offer and the renewal. MEASURED AFTER: direct carrier back **5 s** after the path heals. Gate `T-PUB-RECOVER`. |
| P-9 | **HIGH** | FIXED, red-checked | **A heartbeat the peer never reads wedges the whole client listen loop.** Introduced by P-4 itself and caught by measuring it rather than reasoning about it. `control.send()` lives in a `select!` arm, so once the yamux stream's flow-control credit is exhausted — which happens after roughly 256 KiB of unread frames — the arm blocks forever and the client stops accepting proxied connections while still looking perfectly registered. This is exactly the mixed-version case: a NEW client against a server that predates P-7's read arm. MEASURED on the real staging server (0771de98) with the beat compressed to 2 ms: served at t+5 s and t+15 s, **NO RESPONSE at t+30 s** (~15 000 frames), client log shows no further "new connection". At the production 20 s interval that is days of uptime, and any reconnect resets it — which is what makes it the kind of defect that reaches production. FIX: `client::beat_once` bounds the write with `ctrl_heartbeat_send_timeout()` (10 s, `BORE_CTRL_HEARTBEAT_SEND_TIMEOUT_MS`); on expiry the client warns and stands its heartbeat down for the session, degrading to the legacy heartbeat-free path instead of wedging. Correct against both peers: an old server has no reaper, and a current server's deadline then reaps a control path that is genuinely broken. |
| P-12 | **HIGH** | FIXED, red-checked | **`--max-conns` was never reconciled with the process file-descriptor limit, so the graceful bound was unreachable and the kernel refused first — on every listener at once.** Found by the concurrency ladder against the real deployment (§11): at the rung where ~976 connections were genuinely held through ONE public tunnel, the server logged `failed to accept tunnel connection err=No file descriptors available (os error 24)` every 100 ms from 06:23:44 UTC and answered nothing at all on its control port for about half a minute — the ladder's own admin-API reads failed first with `curl: (35) Send failure: Connection reset by peer` and then with `curl: (7) Failed to connect … after 2 ms`. The container ran `--max-conns 1024` against a soft `RLIMIT_NOFILE` of **1024** (hard 524288), so the semaphore could not reach its bound: `conn_rejections` stayed **0** throughout. That is the whole defect — `EMFILE` is not the semaphore's per-connection refusal, it lands on `accept()` for every listener the process owns, so one tunnel's concurrency took the admin API, the vhost frontends and every other tunnel with it. FIX: `fdlimit::reconcile_fd_limit(max_conns)` at startup, before the first listener is bound: raise the soft limit to `max_conns + 256` when the hard limit allows (a process may always raise its own soft limit up to its hard limit, unprivileged), raise to the ceiling and `warn!` with both remedies when the hard limit is itself short, and say NOTHING when the limit is already sufficient. Gates: `fd_budget` unit tests across every boundary (`RLIM_INFINITY`, exact-fit, overflow) plus the field gate `T-PUB-FDBUDGET`, which starts a real server under a chosen `ulimit` and reads `/proc/<pid>/limits` back — the log line alone would only prove the server talked about it. Red-checked: with the startup call removed the limit stays where it was and the arm fails. One portability defect of the FIX itself is worth recording, because the local sweep could not have caught it: `rlim_t` is 64 bits on most targets but **32** on 32-bit glibc ABIs, so passing `getrlimit`'s output straight into the `u64` decision broke the `arm-unknown-linux-gnueabi` cross job — one job out of the whole matrix, while every gate on this workstation stayed green. The decision stays on `u64` and the two conversions now live at the syscall boundary (`fdlimit::widen`/`narrow`), saturating rather than wrapping: a wrap would silently LOWER the limit, which is the exact storm the module exists to prevent. Pinned by `the_limit_survives_the_round_trip_through_the_platform_type`. |
| P-4 | MED | FIXED, red-checked x3 | **A wedged-but-TCP-alive public client holds its PUBLIC PORT until the server restarts.** The F-1 zombie shape vhost closed, now on the public path, and actionable for the first time only because P-7's fix gave `serve_tunnel` a read arm. A frozen process or a suspended laptop is invisible to both `send` (buffers into yamux) and `recv` (blocks forever). FIX: the client sends `ClientMessage::Heartbeat` every 20 s and the server reaps at 60 s, **checked on the heartbeat tick** and never as `timeout(control.recv())` (DEC-VE3: the 500 ms heartbeat branch wins the `select!` and would reset a `timeout` future before its deadline). **The `Option<Duration>` IS the compat gate** (DEC-VE2): `TunnelOptions.ctrl_heartbeat` is an additive `#[serde(default)]` capability flag, so a client that cannot beat is NEVER reaped — reaping one would kill a healthy idle tunnel every 60 s. MEASURED: before, port 9089 still held 150 s after `SIGSTOP`; after, port 9060 reaped at 60 s. Gates (all three needed, and all three red-checked): `public_wedged_client_is_reaped_and_port_freed`, `public_legacy_client_without_capability_is_never_reaped`, `public_real_client_survives_past_the_reap_deadline`. |
| P-1 | MED | FIXED + gated | **The public direct open was unbounded.** vhost bounds `open_stream` + `write_stream_ready` together with `direct_open_timeout()` (3 s) and ssh-jump with `SSH_DIRECT_OPEN_TIMEOUT`; `serve_tunnel` had no deadline. CORRECTION to the first analysis: this does NOT shorten a total-blackout loss window — on a silent peer both operations succeed LOCALLY, so the window is the QUIC idle timeout either way (confirmed, see P-8). It bounds an open that genuinely BLOCKS, i.e. when the peer's concurrent-stream limit is taken. MEASURED AFTER: fallback at 1.00 / 3.00 / 20.00 s against deadlines of 1000 / 3000 / 20000 ms; before, the connection waited for whoever held the stream. Gates `T-PUB-DEADLINE`, `T-PUB-LADDER`. |
| P-2 | MED | FIXED + gated | **No per-tunnel direct-path observability for public tunnels.** `TunnelView` carried none of `direct_stream_opens` / `direct_fallbacks` / `direct_pool` / `current_path`; `PublicDirectEntry.direct_stream_opens` was incremented and never surfaced, and a public fallback only bumped the SERVER-WIDE `direct_fallbacks` metric, which cannot say WHICH tunnel is degraded. FIX: per-entry `direct_fallbacks` + `last_path`, `Server::public_direct_stats()`, four `TunnelView` fields (all `#[serde(default)]`), and a `Path` column in the admin UI. Gates: `T-PUBPATH` (5 FE tests), `direct_stream_opens`/`current_path` JSON assertions, and every transcript line of `public_idle_window.sh`. |
| P-3 | MED | FIXED + unit-gated | **A partially-established direct pool could stay short forever.** `client.rs`'s `up` arm cancelled the pending renewal and drained the renewal queue unconditionally, so the ordinary mixed outcome — one carrier fails fast (a connect error is immediate, renewal scheduled), another succeeds a round trip later (renewal cancelled) — left the pool below target with nothing to top it up: unlike the TCP carrier pool there is no periodic redial tick. FIX: stand the renewal down only once `live >= target` (`direct_renewal_stands_down`). Unit gates pin the policy; the wiring is exercised by `T-PUB-RECOVER`. |
| P-6 | MED | FIXED + gated | **`--vhost-quic-port` was ignored without a vhost config.** It was applied inside `if let Some(cfg) = vhost_cfg` (`main.rs`), but the shared direct QUIC endpoint is bound whenever `--udp` is on and serves vhost, public AND ssh-jump. A public-tunnels-only server therefore silently kept the 443 default — root-only, and taken by any real HTTPS service — and on a bind failure the warning named only vhost. MEASURED: server logged `shared QUIC direct endpoint listening port=443` while `--vhost-quic-port 17846` was on the command line. FIX: applied unconditionally, before `set_vhost`; warning rewritten to name all three tunnel kinds. Gate `T-PUB-QUICPORT`. |
| P-5 | LOW | FIXED | **`bore local --udp --carriers N>32` clamped silently.** The vhost and ssh-jump constructors both `warn!` on exactly this condition (`client.rs:603`, `:824`); the public one did not. Silent degradation is the one thing this codebase does not do (I-2 / BUG-S5). FIX: the same warning, naming that only the DIRECT pool is clamped. |
| P-10 | LOW | FIXED, red-checked | **Every ordinary public tunnel reported `current_path: "unknown"`.** `last_path` lives on `PublicDirectEntry`, which only exists for a `--udp` tunnel, so a plain relay tunnel had nowhere to record its path and the admin API answered "unknown" — which reads as "the server cannot tell". It can: a tunnel that never asked for `--udp` has exactly ONE possible path. The Path column was therefore useless for the majority of tunnels, which is the opposite of what F-3's `last_path` was added to do. Vhost never had this bug because `VhostEntry.last_path` is on the always-present entry. FIX in `admin_api::tunnels`: with no direct registry entry, answer `relay` unless the tunnel asked for `--udp`, in which case `unknown` is still the honest answer (a direct tunnel that has not yet proxied a connection genuinely has no last path). No hot-path change and no wire change. Gate `a_relay_only_public_tunnel_reports_the_relay_path_not_unknown`, red-checked: making the unknown arm unconditional again fails it with `left: "unknown" right: "relay"`. |
| P-11 | LOW | FIXED, red-checked | **`/admin/api/v1/config` published a live gauge under a configuration name.** `Server::udp_direct_slots()` returned `Semaphore::available_permits()`, so the config endpoint's `udp_direct_slots` fell as direct connections were admitted and rose as they closed. Found on the deployed server, where the field read `30` while the server's own startup advisory for the same budget said `slots=32` — two different numbers for one setting, in the same server, at the same time. On a saturated server it reads 0, which is indistinguishable from "no budget configured". FIX: `Server` keeps `udp_direct_slots_total` beside the semaphore (tokio's `Semaphore` cannot report its own initial permit count), `udp_direct_slots()` returns the CONFIGURED total, and the live gauge moves to `/admin/api/v1/metrics` as the additive `udp_direct_slots_available` — beside `direct_budget_refusals`, which it complements: refusals say the budget was exceeded in the past, the gauge says it is full NOW. Gates: `configured_direct_slots_do_not_move_with_load` (red-checked: pointing the accessor back at `available_permits()` fails it with `Some(5)` vs `Some(8)`) and FE `T-SLOTSFREE` (red-checked: a truthiness guard instead of an explicit null check hides the saturated `0` case, which is the one that matters). |
| P-8 | — | measured, no defect | **The public-path blackout loss window IS the QUIC idle timeout, exactly** — the same law the vhost campaign established, now confirmed on the public path: 9.99 s at the shipped 10 s default and 3.99 s with `BORE_DIRECT_QUIC_IDLE_MS=4000`. It is a deployment tunable, not a bug, and no open deadline can shorten it. Gate `T-PUB-IDLE`. |
| D-1 | MED | FIXED | **`docker/docker-compose.client.yml` both enables the public direct path and tells the operator it does nothing.** The file's `command` is `["local"]` — a PUBLIC tunnel — and its environment carries an ACTIVE `BORE_PREFER_UDP=true`, directly above a comment reading "Takes effect only together with BORE_TCP_SECRET_ID (secret-tunnel provider); ignored for a public-port tunnel." That was true before public `--udp` existed; `BORE_PREFER_UDP` is the env of `--udp` on `bore local` too (`main.rs:131`), so the shipped compose silently turns the QUIC direct path ON for every operator who copies it, while documenting the opposite. Whether that default is right is exactly what this campaign measures — the vhost campaign concluded OFF for an ordinary workload — but the contradiction is a defect either way. FIXED with the measurement in hand: the line is commented out, the comment now describes BOTH mechanisms the flag drives, and a measured recommendation block quotes §6, §9 and §12 (off by default; on for many held connections or a lossy path). |
| D-2 | LOW | open (deployment, not code) | **The staging server runs `--max-carriers 1024` on a 903 MiB host, which makes its own `--udp-memory-budget` advisory unsatisfiable.** Startup logs two warnings: that the 512 MiB budget is more than half of host RAM, and that it "cannot hold every carrier at the smallest usable window: a `--udp --carriers 1024` tunnel will get 32 direct carriers and relay the rest; raise the budget to 16384 MiB to hold them all". Both are the F-13 advisory working exactly as designed — but the remedy it names (16 GiB of budget) is impossible on this host, and the real remedy is the other side of the same inequality: `--max-carriers` 1024 is far above anything the campaign measured as useful (P2 peaks at 4 and is already worse at 8). No client can currently reach 1024 anyway — the direct pool is separately capped at `MAX_DIRECT_CARRIERS` — so this costs nothing today; it is a configuration that makes the server shout about a case that cannot happen. Recommend `--max-carriers 16` (the documented default) on this host. |
| D-3 | LOW | OBSERVED, no code change | **The deployed server runs with the FLOOR direct-path windows, and that is a consequence of `--max-carriers 1024`.** `UdpDirectTuning::from_memory_budget(512 MiB, 1024)` derives `conn = clamp(512 MiB / 1024, 16 MiB, 256 MiB)` = 16 MiB and `stream = conn / 16` = 1 MiB, i.e. sixteen times below the tested defaults (256 MiB / 16 MiB). Nothing is silent about it: the server logs `direct UDP memory budget applied budget_mib=512 slots=32 connection_window_mib=16 stream_window_mib=1` and warns that a `--carriers 1024` tunnel gets 32 direct carriers and relays the rest, and the floor clamp is unit-pinned (`budget_below_the_carrier_count_reports_a_shortfall`). This campaign could NOT show the floor binding: at the ~1 ms in-region RTT a 1 MiB stream window allows on the order of 1 GB/s per stream while the direct path measured 115-133 MB/s (§6), and the 40-100 ms netem cells move 16 MiB per connection in about 5 s, which is congestion ramp, not window exhaustion (§12). Recorded as an operator recommendation (§18), not as a defect. |

### 16.1 Checked and cleared

Things that looked like defects and are not. They are listed because "we
looked at it" is only evidence if it says what was looked at.

- `/admin/api/v1/config` reports the public range as a single `port_range` key (`9000-9100`), not as `min_port`/`max_port`. A harness that reads the wrong key names sees an unconfigured server; `res/cfgkeys.sh` prints the real key set.
- `bind_public_listener` (`server.rs:113`): a requested port is validated against the range; `port == 0` draws 150 random ports. Sound.
- Public QUIC auth: the `port:N` key is bound to a per-tunnel nonce delivered only over that tunnel's authenticated control channel, so knowing the shared secret is not enough to join another tunnel's direct pool. The renewal mints a FRESH nonce, so a renewed dial cannot replay the previous one.
- `PublicDeregister::drop` removes both the registry entry and the pending nonce.
- The `--max-conns` permit is released on every `continue` in the accept loop (owned permit, lexical scope).
- **The SERVER's own heartbeat send in `serve_tunnel` is a bare `send` in a `select!` arm — the P-9 shape — and it is nevertheless correct here, by arithmetic.** A peer that never reads the substream fills its 256 KiB yamux credit and the send blocks forever, which would also block the reap check that sits after it in the same arm. But `ServerMessage::Heartbeat` serializes to ~15 bytes including its length delimiter, so exhausting the credit takes ~17 000 frames = ~2.4 h at the 500 ms tick, against a 60 s reap deadline: a 145x margin, and the reaper always fires first. For a LEGACY client (no `ctrl_heartbeat`, therefore no reaper) the two outcomes are the same anyway — the port is held either way — and a wedged client cannot accept data substreams either, so a wedged accept loop costs nothing additional. Bounding the send would add a failure mode without removing one. Recorded as a comment at the call site so that raising `public_ctrl_timeout` past an hour, or growing the message, is caught.

---

## 17. Harness defects

A harness that lies is indistinguishable from a server that misbehaves, so
these are reported with the same weight as the product defects. Nine of
them (H-4 … H-12) had already silently thrown data away, refused to run at all,
measured the wrong program, measured the sum of two rungs while labelling it as
one, or measured nothing at all while printing in the exact format of a real
measurement, before they were caught. Two of the nine (H-7 and H-12) are the
SAME failure shape — a whole stage of zeros in the format of a real result —
found twice, which is why the workstation stage now carries a preflight that
refuses to measure rather than a comment asking the operator to be careful.

| id | status | finding |
|----|--------|---------|
| H-1 | FIXED | Harness: `provision.sh` never installed `scripts/bench_origin.py`, which every `vm_*` harness starts. A re-provision of a fresh VM produced scripts that all failed at "origin failed". |
| H-2 | FIXED | Harness: `srv/setenv.sh` and `srv/logcheck.sh` called `$SSH "cmd"`, but `$SSH` in `lib.sh` carries the ssh OPTIONS only with no host, so ssh tried to connect to a host named by the command string. `setenv.sh` also used `$VM_HOME` — the TEST VM's home — for the compose file, which lives on the SERVER, and anchored its edit on one hardcoded variable name. Both rewritten against `srv` and `$BORE_SRV_COMPOSE`. |
| H-3 | FIXED | Harness: `res/start_samplers.sh` read `$SRV`, `$V`, `$SSH` and `$VM_HOME` without sourcing `lib.sh`, so it only worked when the caller had sourced it first. Rewritten against the `srv`/`vm` helpers. |
| H-4 | FIXED | Harness: `res/start_samplers.sh` wrote the remote samples to `~/wsres.*` while `res/stop_samplers.sh` pulled `~/pres.*`. The two never met, so every phase driven by `start_samplers.sh` collected NOTHING from the server and the VM — and it failed silently, because `scp` of a missing file was swallowed by `2>/dev/null`. Both ends now share one `SAMPLER_PREFIX`. |
| H-5 | FIXED | Harness (found by the first public smoke run): `TunnelView` calls the public port `public_port` and the live connection count `active`, not `port`/`active_conns`. The first version of `publib.sh` guessed, so `present()` never matched a tunnel that was in fact registered and every arm reported REGISTRATION FAILED. Field names are now taken from `src/admin_views.rs`. |
| H-6 | FIXED | Harness: `res/cfgkeys.sh` sourced `../env.sh`, a path that does not exist (the real env file lives outside the repository and every other script finds it through `lib.sh`). It therefore ran with an empty `$ADMIN_URL` and died with `curl: (3) URL rejected: No host part in the URL` — the one script whose job is to tell you what the server's config keys are ACTUALLY called. Rewritten against `lib.sh`, given an optional regex argument and an `all` mode, and its default filter widened to include `port` and `version` (it previously excluded `port_range`, the very key whose name is easiest to guess wrong). |
| H-7 | FIXED | Harness: the P6 concurrency ladder reported `held=N up=0 active_at_server=0 errs=N` on every rung — a ladder that holds nothing. The `HOLD` verb was correct (a local reproduction against a fresh origin answered `held=8 up=8 errs=0`, and the VM's copy of `raw_origin.py` is byte-identical to the tree's), but `ps -eo pid,lstart` showed the LIVE origin process had started at 02:30:48 while the file it was started from was written at 03:10:41: the running program predated the verb. `start_origins`'s `pgrep` idempotency check saw a matching process and reused it. Fixed with `reap_stale_origin`, which kills THAT PID (never a pattern-wide `pkill`, project rule) when the process is older than its own script file. `git log -p --follow -- scripts/perf/raw_origin.py` confirms the only change in the commit that added `HOLD` was the additive arm, so the P1-P5 GET/PUT/PING numbers taken against the same origin are unaffected; only the ladder had to be re-run. |
| H-8 | FIXED | Harness: the whole P9 CPU-efficiency stage measured zero. `vm_pub_eff.sh` asked for 8 GiB and cut the run off with an external `timeout`, but `raw_client.py` prints its single result line only when it finishes — an external kill produces no output, and the empty `bytes=` parsed as 0, so every case printed `rate=0.00 MB/s gib=0.000` in the same shape a real case prints. The window was also being passed in the per-socket TIMEOUT position. Fixed by giving `raw_client.py` an additive `window` argument that cuts the transfers itself and credits each connection with the bytes it had moved, and by passing it from the stage. Red-checked both ways: with a window a 5 s run of an 8 GiB request returns a real count at 5.001 s; without one the legacy call is unchanged. A windowed PUT is credited with locally-written bytes rather than the origin's ack — documented at the call site, and under 1 % at the window sizes this option exists for. |
| H-9 | FIXED | Harness: the P6 ladder's rungs were CUMULATIVE, not concurrent. `raw_origin.py`'s `HOLD` verb parked on `asyncio.sleep(secs)`, so when a rung ended by killing its driver the origin kept ITS half of every connection open for the rest of the hold: the server's proxied connections stayed half-open and the next rung measured the sum. It is legible in the transcript — `active_at_server` read 80 at the 64 rung, 208 at 128, 464 at 256, each exactly the running total, and the harness printed the server's own count beside the requested one precisely so that a divergence would be visible. The latencies themselves were real (that many connections genuinely were open), but they are not the measurement the rung labels claim. FIX at both ends: `HOLD` now parks on `reader.read(1)`, which returns `b""` the instant the peer goes away, so a connection dies with its driver; and `vm_pub_conc.sh` gained `wait_quiet`, which waits for the server's `active` count to return to zero between rungs and SAYS SO when it does not. Worth recording that this defect is what exposed P-12: without the accumulation the ladder would never have reached ~976 concurrent connections against a 1024-descriptor server. |
| H-10 | FIXED | Harness: a stage re-run after the campaign had finished produced measurement windows with no samples behind them. `run_campaign.sh` stops the resource samplers at the end — correct for a campaign — but `vm_pub_eff.sh` emits `window=<t0>-<t1>` lines whose only meaning comes from a host `/proc/stat` stream covering the same window, so the re-run reduced to `no samples in the window (need at least two)` for all nine cases. The reducer was honest — it refused rather than inventing a number — but the stage had already been paid for. FIX: `pub/rerun_stage.sh`, a first-class re-run entry point that pushes the harness and proves the copies match by md5, verifies the samplers are alive (starting them when they are not) BEFORE the stage runs, collects the logs and the samples afterwards, and joins the `eff` windows to the CPU samples itself so that the CPU s/GiB table is produced rather than assembled by hand. |
| H-11 | FIXED | Harness: `T-PUB-HEALTHY` put `netem loss` on the WHOLE loopback, so the loss it applied to the QUIC path under test also hit the CLIENT'S DIAL OF THE LOCAL ORIGIN — a dial `connect_with_timeout` bounds at `NETWORK_TIMEOUT` (3 s). At 30 % loss a dial whose SYN is dropped past the third retry exceeds that bound, the client closes the substream and the public connection ends with no response at all, which the arm counted as a failure of the direct-open deadline. Found as a one-in-four flake — `000 3.205574` and `000 4.084193` in two of four cells, each with `direct_stream_opens` incrementing and `fb=0`, i.e. the direct path perfectly healthy — and settled by keeping the logs (`BORE_PUB_KEEP_RUN=1`, added for it): a cell with three failures carried `WARN could not connect to localhost:18090` at exactly the three failing timestamps. The client was working as designed; the ARM was asserting on something it does not claim. FIX: the loss is now scoped to the QUIC port with a `prio` qdisc plus two `u32` filters, so the TCP relay, the public connection and the origin dial stay clean, and the arm additionally asserts netem's OWN counters (`sent_pkt` above zero on every cell, `dropped` above zero on the lossy cells) — a loss that silently failed to apply would otherwise make the arm vacuously green, which is H-7's shape. Whole-path loss belongs to the netem matrix stage against a real server (§12), and is measured there. The arm also gained the COMPLEMENT of T-PUB-DEADLINE while it was being fixed: on the clean cell it now asserts `open timeouts logged: 0` and `fb=0`, because a deadline that fired with nothing wrong would put every connection on the relay while the served count still read 12 of 12. Those two assertions are deliberately NOT claimed as red-checked: at `BORE_PUB_HEALTHY_DEADLINE=1` the clean cell still reports zero timeouts — a loopback `open_stream` plus `write_stream_ready` costs well under a millisecond, and `direct_open_timeout` rejects 0 — so their positive control is T-PUB-DEADLINE and T-PUB-LADDER, which run the same server and the same log format and do produce a nonzero count. Incidentally measured: the 3 s default bounds an operation that costs microseconds locally. |
| H-12 | FIXED | Harness: the workstation stage measured `0.00 MB/s` on every arm, with both tunnels correctly registered and the server reporting `path=direct pool=1`. `ws_pub.sh` starts its raw origin on the VM through `vm "pgrep -f 'raw_origin.py $RP' >/dev/null || (start it)"`, and that `pgrep` runs REMOTELY: the `bash -c` carrying the whole one-liner contains the string being searched for, so the pattern matched its own command line, the origin was reported as already running, and it was never started. The forwarder then tunnelled a local port with nothing behind it — every connection was accepted by the server, forwarded, and closed with zero bytes, which the harness printed as `0.00 MB/s` in the exact format of a real measurement. The same trap had already been found twice in this campaign (`rerun_stage.sh`'s wait loop, `start_samplers.sh`'s pkill) and the usual remedy — bracketing the pattern as `raw_origin.p[y]` — is only HALF a fix here and was measured to be: bracketing stops the pattern from matching its own text, but the start command in the same one-liner still contains the plain `raw_origin.py`, so the `pgrep` keeps self-matching and the origin still never starts. FIX, two parts. The existence check no longer asks "is there a process with this name" but "is something SERVING on that port", with a real connection (`timeout 2 bash -c '</dev/tcp/127.0.0.1/$RP'`) — the question the stage actually needs answered, and immune to the class of defect entirely. And the warm-up became a PARSED PREFLIGHT: it reads `bytes=` back from both transports and `exit 2`s with the diagnosis and the command to check, because a registered tunnel proves the CONTROL path and says nothing about the DATA path. Red-checked by construction — the broken state is what produced the transcript above, and the fixed stage refuses it: `PREFLIGHT FAILED on port 9021: registered, but it moved no bytes.` The reaper in the same block still needs a PID and still uses a bracketed `pgrep`, which is safe there because its own command text contains `raw_origin.py)` from the `stat` and never `raw_origin.py $RP`. Incidental fix in the same pass: the stage's RTT line was `ping | cut`, and ICMP to this gateway is filtered, so it printed an empty string and then `n/a` — replaced by a TCP-handshake probe to the control port, which is the same first round trip every arm below pays and cannot come back blank without saying so. |
| H-13 | FIXED | Harness: `rerun_stage.sh` waited FOREVER for a stage that had already finished. Its wait loop polls the VM for a live driver with `n=$(vm 'pgrep -c -f "pub_drive[r].sh" || echo 0')` and breaks on `[ "${n:-0}" -lt 1 ]`. `pgrep -c` prints the count — **including `0`** — and ALSO exits 1 when nothing matched, so the `|| echo 0` fallback fires on top of the zero already printed and `n` comes back as the two lines `0\n0`. That is not a comparison that returns false, it is a syntax error in `test`: `[: 0\n0: integer expression expected`, rc=2 — falsy, so `&& break` never fires. Every 30 s it printed the same progress line, which is exactly what a healthy long stage looks like. MEASURED: a `conc` re-run sat in this loop for **four hours** after its own stage had returned `rc=0`, producing nothing, and was found only by listing processes while investigating something else. The bracketing lesson of H-12 was already applied here and was not enough — the pattern was right and the PARSING was wrong. FIX, three parts, all needed: drop the `|| echo 0` and take the first line, so the count is one token; refuse a NON-NUMERIC count loudly and keep waiting, because an ssh that fails mid-run must never look like "the stage finished"; and bound the whole wait with a deadline (`RERUN_MAX_WAIT`, 3 h) and say so when it fires, because no amount of parsing care covers a VM that dies with its driver still registered. Red-checked against the real deployment: the same `rerun_stage.sh eff` that had to be killed and collected by hand now runs to completion, collects, and prints the joined CPU table by itself. |
| H-14 | FIXED | **A regression TEST**, not a measurement harness — recorded here because the register's whole premise is that an instrument which lies is indistinguishable from a server that misbehaves, and this one lied in CI. `local_access_log_raw` failed on `aarch64-apple-darwin` with `raw log should have content: ` — an EMPTY message, on a commit that changed no Rust at all. The cause is the suite's own `poll_file` helper: it returned the moment `std::fs::read_to_string` succeeded, and that succeeds on a **zero-byte** file. The access-log writer creates the file and appends the line afterwards, so there is a real window in which the path exists and is empty; on a runner that scheduled the read inside it the helper handed back `""` and the test reported it as the server having logged nothing. All three call sites assert on CONTENT, so the other two carry the same latent race and only lose it less often, their write being further down a longer chain. FIX: poll for NON-EMPTY, and keep "never created" and "created but never written" as distinct errors — they are different bugs and a helper that merges them sends the next reader to the wrong place. Gate `poll_file_waits_for_content_and_does_not_accept_an_empty_file` reproduces the race deterministically (empty file, writer after 300 ms, no server, no port, no serial guard) and is RED-CHECKED: against the previous body it fails in 0.00 s with `poll_file returned before the line was written: ""`, which is the CI symptom exactly. |

---

## 18. Recommendations

Every line here is backed by a measurement in this document, named in
parentheses. Nothing is inherited from the vhost campaign without being
re-measured on the public path, because three of the results came out with
the opposite sign.

### 18.1 For whoever runs the forwarder

| situation | setting | why |
| --- | --- | --- |
| several connections at once (a browser, a small web app, parallel transfers) | `--carriers 4` | 303.60 MB/s against 210.25 at one carrier, 1.44× (§7); 8 is worse than 4 in both directions |
| one bulk transfer at a time | `--carriers 1`, the default | a single flow rides one carrier whatever N is; extra carriers only cost |
| clean, in-region path | **no `--udp`** — but see the note below | the relay wins every one of ten paired runs, median 1.51× down / 1.34× up (§6), and wins the small-request tail (§8) |
| lossy path (radio, congested uplink, poor VPN) | **`--udp`** | 1.4× at 1 % loss, 9× at 3 %, **132× at 10 %**, where the relay has effectively stopped (§12) |
| high RTT | `--carriers N`, not `--udp` | at 40–100 ms both transports are bandwidth-delay-product bound and converge (§12) |
| many connections held open at once (a busy app, long polls, websockets) | either transport, no change needed | a fresh connection costs 4.6 ms behind 512 held connections and 4.5 ms behind none, on both paths (§11); the direct path held 512 concurrent QUIC streams with zero fallbacks |
| nothing may be installed on the machine | `ssh -R` | costs ~0.75 ms per new connection and about 1.9× on single-carrier download (§10) — and nothing else |

**The `--udp` throughput row was provisional until the redeploy, and it was
re-measured.** Every paired throughput comparison in §6, §7 and §10 was taken
against a server whose shared QUIC receive socket was 208 KiB — the kernel
default, because of P-13 (§13.1) — and that socket is the RECEIVING side of a
download, so the relay's win was partly a measurement of the defect. §13.2
re-ran the packet-accounting probe against the fix on the same path: the
inbound inflation went from **1.78× to 1.009×**, and direct goodput rose from
111 to **143.43 MB/s** at four connections while the relay's stayed at
200.85. The ranking did not change — the relay still wins on a clean in-region
path — but the margin is **0.71, not 0.52**, so roughly two fifths of the gap
was the defect. Read the 1.51× and 1.34× medians in §6 as the pre-fix numbers
they are; the direction of the recommendation holds, the size of the advantage
is smaller. The LOSS results (§12), the latency results (§8, §11) and the whole
robustness argument for `--udp` were never affected, because none of them is
bandwidth-bound.

What has NOT been re-run against the fix is the full ten-pair §6 ladder and the
concurrency sweep — one probe on one path is enough to settle the CAUSE, not
enough to restate every median. Those tables stay labelled with the build they
were taken on (`dbcc645a`), and `pub/rerun_stage.sh p1 conc` reproduces them on
the new build in about an hour when someone wants the updated medians.

Docker is free: native and dockerized binaries are within the round-to-round
scatter on throughput and within 0.03 ms on latency (§10). Use the ROOT image
(`ghcr.io/manprint/bore:client`) when you pass `--udp`: Docker clears every
capability for a non-root UID, so the default image cannot widen the kernel's
UDP socket buffers and logs a warning on every socket.

### 18.2 For whoever runs the server

* **Size `--max-carriers` to the host, not to the maximum.** The staging
  server runs `--max-carriers 1024` on a 903 MiB host (D-2). Nothing breaks —
  the direct-path budget refuses politely and the tunnel stays on the warm
  relay — but two consequences follow: the per-tunnel worst case a single
  `--udp --carriers N` tunnel may request is `N × connection_window`, and
  `--udp-memory-budget` derives its windows as `budget / max_carriers`, so a
  large carrier cap pushes the derived windows to their FLOOR (D-3): this
  deployment runs at `connection_window=16 MiB`, `stream_window=1 MiB`,
  sixteen times below the tested defaults. The server says so at startup and
  the clamp is unit-pinned, and at this campaign's ~1 ms RTT the floor was not
  the binding constraint — but an operator who wants the tested windows should
  either lower `--max-carriers` (16–64 is plenty: the measured optimum for a
  single tunnel is 4) or size the budget as `max_carriers × 256 MiB`.
* **Raise `net.core.rmem_max` and `net.core.wmem_max` on the HOST, and read
  the line the server logs about them.** bore asks for a 16 MiB UDP socket
  buffer on every direct-path socket, including the shared QUIC endpoint that
  receives the direct traffic of every tunnel (P-13, §13.1). The kernel
  silently clamps that request to `net.core.*mem_max`, which is 212992 bytes
  (208 KiB) on a stock Debian/Ubuntu and 4 MiB on this deployment's host, and
  an unprivileged process cannot bypass the clamp — `SO_*BUFFORCE` needs
  `CAP_NET_ADMIN`, which a container without `NET_ADMIN` (`caps=[]`, as here)
  does not have. A QUIC flow through a clamped socket is bounded at roughly
  `buffer / RTT` and pays for every datagram the socket drops twice: once to
  send it, once to send it again. `sysctl -w net.core.rmem_max=16777216
  net.core.wmem_max=16777216` (persist it in `/etc/sysctl.d/`) is the whole
  remedy, and these are GLOBAL, not per-netns, so setting them inside a
  container's `sysctls:` does not work. The server now says which it got:
  `configured UDP socket buffers … effective_recv=16777216` when the request
  was honoured, and the `clamped below request` warning naming both remedies
  when it was not.
* **Keep `--udp-memory-budget` set.** It is the only aggregate bound on
  direct-path memory, it costs nothing when it is not hit, and a refusal is a
  fallback to the relay, never a failed request.
* **Watch `udp_direct_slots_available` on `/admin/api/v1/metrics`** beside
  `direct_budget_refusals` (P-11). The counter tells you the budget was
  exceeded in the past; the gauge tells you it is full now. On this
  deployment the pair reads 30 free of 32 configured with the operator's own
  two vhost tunnels holding a carrier each.
* **Upgrade the server before the clients.** The control-liveness work (P-4)
  is additive on the wire in both directions, but the P-9 shape — a new
  client heartbeating at a server whose `serve_tunnel` does not read the
  control substream — is exactly a client-newer-than-server deployment. The
  client now survives it (it stands its heartbeat down after 10 s and warns),
  and that graceful degradation is precisely what should not be relied upon
  as a deployment strategy.
* **Give the server descriptors, and read the startup line that says whether
  it got them.** `--max-conns` can only refuse gracefully while the process
  can open a descriptor per admitted connection; above that the kernel refuses
  first with `EMFILE`, on every listener at once, which is how one public
  tunnel took this server's admin API down for half a minute (P-12, §11). The
  server now raises its own soft limit to `--max-conns + 256` at startup and
  logs `raised the file-descriptor limit …`; if instead it logs the warning
  about the hard limit being short, raise it for the process — Docker Compose
  `ulimits: { nofile: { soft: 65536, hard: 65536 } }`, systemd
  `LimitNOFILE=65536` — or lower `--max-conns`. This deployment ran with both
  numbers at 1024, which is the one combination that cannot work.
* **Budget the CPU, not the bandwidth, when sizing the host.** The public
  relay path costs **5.35 CPU s/GiB** at eight carriers and **7.04** at one;
  the QUIC direct path costs **13.58** — 1.93× the relay for the same bytes
  (§13). One core of this instance class therefore carries about 1.6 Gbit/s of
  relay traffic and 0.63 Gbit/s of direct traffic, so 10 Gbit/s of public
  tunnels needs ~6 cores on the relay and ~16 on the direct path. Both direct
  arms ran at 1.39-1.56 of 2 cores with steal at 0.01 s, i.e. the ceiling
  measured in §6 is the CPU and not the network — which is also why `--udp`
  costs the SERVER more than it costs the client.

### 18.3 What is deliberately not recommended

* **Do not turn `--udp` on "for speed".** On a clean path it is the slower
  transport in every arm this campaign ran except the 32-connection tail
  (§9) — and the two places it wins, it wins by two orders of magnitude, so
  the decision is about the network and not about the throughput.
* **Do not raise carriers past the ladder's peak.** 8 was worse than 4 on
  download and worse than 2 on upload (§7). A carrier is a socket, a
  congestion window and a share of two vCPUs.
* **Do not read a single unpaired measurement on a burstable instance.**
  Every comparison here is paired and order-alternating for one reason: the
  allowance bucket makes the second measurement pay for the first (§4).

---

## 19. Repeating this campaign

Nothing in this document depends on a host name, a port or a credential that
lives in the repository. The whole harness is re-pointed at another deployment
by editing one `env.sh`, and
[`scripts/perf/staging/pub/README.md`](../../scripts/perf/staging/pub/README.md)
is its reference — layout, the two origins, and the traps that make results
wrong.

```bash
# 0. coordinates (never committed; mode 600)
cp scripts/perf/staging/env.sh.example ~/.config/bore-perf/env.sh && chmod 600 $_
$EDITOR ~/.config/bore-perf/env.sh

# 1. bring the test VM to the state the campaign expects (idempotent)
scripts/perf/staging/provision.sh

# 2. deploy the build under test on the server, and verify all three actors
#    (redeploy.sh ends by running verify_fixes.sh; run it alone at any time —
#     it restarts nothing and moves no traffic)
scripts/perf/staging/srv/redeploy.sh
scripts/perf/staging/srv/verify_fixes.sh

# 3. the whole VM-side campaign, strictly serial (hours)
scripts/perf/staging/res/start_samplers.sh
ssh <vm> '~/pub/pub_driver.sh'          # or a subset: ~/pub/pub_driver.sh p1 conc eff
scripts/perf/staging/res/stop_samplers.sh

# 4. the real-world topology, from the workstation
scripts/perf/staging/pub/ws_pub.sh

# 5. reduce one collected run to the tables in this document
scripts/perf/staging/pub/summarize.sh out/pub-<timestamp>
```

Two reducers do the arithmetic that is easy to get wrong by hand:

| reducer | what it produces |
| --- | --- |
| `pub/summarize.sh <dir>` | every table in §6–§14, plus the allowance timeline filtered to its non-zero deltas |
| `res/cpu_window.sh <prefix.stat> <t0> <t1> [gib] [prefix.proc]` | CPU seconds a host spent inside one window, and CPU s/GiB when told how much moved. `vm_pub_eff.sh` prints the matching `window=<t0>-<t1> … gib=` line per case, so the two are joined by copy-paste and not by eye. |

One more script is neither a measurement nor a reducer but a field gate:

| gate | what it proves |
| --- | --- |
| `srv/verify_fixes.sh` | that P-12's descriptor reconciliation and P-13's socket buffers actually took effect **in the deployed process**, read from the kernel through the container's PID (`/proc/<pid>/limits`, and `nsenter … ss -uapm` for the QUIC socket's `rb`/`tb`) rather than from a log line — the same two quantities the lab gates `T-PUB-FDBUDGET` and `T-PUB-UDPBUF` assert. Both fixes are invisible in normal operation, which is why neither was noticed for as long as it existed; a version string changing is not evidence that they are in force. |

The correctness gates are separate from the measurement harness and need no
deployment at all — they run in a rootless network namespace on any Linux
workstation:

```bash
cargo build --release --features ssh-gateway,vpn
scripts/perf/public_idle_window.sh          # the full public-path matrix
scripts/perf/public_idle_window.sh relaypath   # one arm
```

Two environment knobs exist for investigating an arm rather than running it:

| knob | effect |
| --- | --- |
| `BORE_PUB_KEEP_RUN=1` | keeps each arm's server and client logs instead of deleting the run directory, and prints the path. The namespace is gone by then but the directory is a host path, so the logs survive it — the only way to attribute a failure that happens once in dozens of requests. |
| `BORE_PUB_HEALTHY_LOSS="30"` | narrows `T-PUB-HEALTHY` to the named loss cells, so the interesting one can be run on its own, repeatedly. |
| `BORE_GATES_JOBS`, `BORE_GATES_TEST_THREADS` | bound the sweep's compile and run parallelism (default 4 and 4, plus `nice -n 19`). Do not remove them: `cargo test` otherwise compiles AND runs every suite across all cores at once, and each suite of the `vpn,ssh-gateway` set starts real servers, namespaces and QUIC endpoints with multi-MiB socket buffers. On a 16-core workstation that took the machine to 100 % CPU and out of usable memory and had to be killed mid-run — twice. Whoever runs this sweep is usually also using the machine. |

### The regression sweep this campaign was gated on

Every suite below was run on the campaign's final tree, on this workstation,
after the last code change. The netns harnesses were run one at a time: they
share the namespace names `ns0`/`ns1`/`ns2`, so two of them in parallel wipe
each other's namespaces mid-run and fabricate failures.

| gate | command | result |
| --- | --- | --- |
| unit + integration, default features | `cargo test --release` | 680 passed, 0 failed |
| unit + integration, `vpn,ssh-gateway` | `cargo test --release --features vpn,ssh-gateway` | 973 passed, 0 failed, 2 ignored |
| lints, both feature sets | `cargo clippy --all-targets -- -D warnings` | clean |
| admin frontend | `npm test` | 109 passed, 0 failed |
| admin dashboard e2e | `sudo -n scripts/admin_dashboard_test.sh` | 26 passed, 0 failed |
| public path, netns | `scripts/perf/public_idle_window.sh all` | 38 passed, 0 failed |
| local/proxy hardening, netns | `sudo -n scripts/local_proxy_netns_test.sh` | 16 passed, 0 failed |
| secret tunnels, netns | `sudo -n scripts/secret_netns_test.sh` | 29 passed, 0 failed |
| vhost, netns | `sudo -n scripts/vhost_netns_test.sh` | 16 passed, 0 failed |
| SSH gateway, netns | `sudo -n scripts/ssh_gateway_test.sh` | 21 passed, 0 failed |

`public_idle_window.sh all` grew from 25 assertions to 38 during this
campaign: `T-PUB-FDBUDGET` (P-12) contributes 6, `T-PUB-UDPBUF` (P-13) 3, and
`T-PUB-HEALTHY` gained the netem-counter and clean-path assertions that H-11
exposed. The count is in the table because a matrix that silently loses an arm
looks exactly like a matrix that passes.

The two ignored tests in the `vpn` feature set are the two that require root
(`vpn::hostcfg::tests::check_root_accepts_uid_zero` and `tun_bring_up_and_down`);
what they cover is exercised for real by the netns harnesses, which do run as
root.

The two code areas this campaign changed outside the public path — the admin
API (P-10, P-11) and the client's heartbeat (P-9) — are shared with the vhost,
secret and ssh-jump registries, which is exactly why the sweep covers all four
netns harnesses and not only the public one.
