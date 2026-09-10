# vhost on staging — measured evidence

Living document. Every row is something that was measured, with the setup that
produced it and the reason it is or is not trustworthy. Updated as the campaign
proceeds. Retracted findings stay in, marked as retracted, because knowing what
turned out to be an artifact is as useful as knowing what held.

- Target: `brp.0912345.xyz` (15.161.245.248), staging, **configuration frozen — never modified by these tests**
- Scope of this campaign: the `bore vhost` data plane, both transports (TCP relay and QUIC direct)
- Question being answered: how much bandwidth can be extracted, at what latency, and which of the
  candidate optimizations in `docs/vhost/VHOST_PERFORMANCE_ASSESSMENT_2026-09-07.md` the data supports
- Scope extended on request: operational stability, races and ghost sessions; vhost through the SSH
  ingress gateway; and a full runbook so a repeat does not start from zero (§9)
- Started 2026-09-10

## What the campaign concluded, in one page

**Performance.** The server does **136–335 MB/s** on the TCP relay and
**77–127 MB/s** on QUIC direct, and it is **CPU-bound — genuinely, at 1.90 of
2 cores when pushed — with 46–82 % of that in the kernel** (F-7, §2.17). It sustains **~11 000 req/s at p50 2.8 ms**
with a flat 15.4 MiB RSS over 6.5 M requests (F-11). On a clean low-RTT path the
**TCP relay is 1.59× faster than QUIC (paired, 8/8 pairs) and costs half the client CPU**; under 1 %
loss or +40 ms on the data leg, **QUIC is 2.2–2.8× faster** (F-8, §2.14). So
`--udp` is a remedy, not a default — and nothing in the docs says so today.

**Latency.** Idle p50 is ~1 ms above RTT and stays there at 11 k rps — but
**idle latency is not the number that matters.** With a single bulk transfer in
flight on the same tunnel, small-request p50 goes from 2.6 ms to **28 ms on the
relay** (11×, request rate −91 %) and QUIC develops a **219 ms p99** (F-15).
That is what an ordinary page load looks like — one large asset alongside many
small ones — and no idle benchmark shows it. Connection setup costs ~4.9 ms of
*server* time; the +45 ms measured from a 21 ms-RTT client was TLS round trips,
so HTTP/2 on the browser side is still the strongest latency lever for remote
browsers, but most of its prize is the client's RTT, not server work.

**Stability.** Nothing decays, nothing leaks under any *graceful or hard-kill*
path: 20-cycle reconnect storms, 4-way label races, 10 simultaneous tunnels,
mid-response provider death and origin death all released cleanly in 0–2 s
(§2.13). Two real defects remain, and both are availability rather than speed:

1. **F-1/F-10 — a wedged provider holds its subdomain forever.** A provider
   alive at TCP level but dead at application level keeps the label at
   t+180 s on *both* transports, and re-registration is rejected. The **SSH
   gateway recovers from the identical scenario in 20–40 s** because it has a
   bounded open plus strike eviction (I-SSH10); the native path has neither. The
   fix pattern is already written, tested and shipped twice in this binary.
2. **F-13 — `--udp` memory is unbounded per tunnel.** 32 slow readers on one
   tunnel took the server to 536 MiB on a 903 MiB host, timed out two requests,
   failed a registration and made an unrelated tunnel reconnect. On the
   announced 2 GiB production VPS the same shape needs roughly twice as many
   readers — still reachable by one client on a bad network.

On throughput the paired A/B settles the two open transport questions:
**8 of 8 pairs favour the relay at a median 1.589×**, and **4 carriers do not
help on a clean path** (median c4/c1 ratio 0.941). Carriers earn their keep only
under loss, where they scale aggregate throughput almost linearly — which is the
case for making the count adaptive, not for raising the default.

Plus three cheap, high-value gaps: a dead origin closes the connection instead of
answering 502 (F-12); a UDP path failure costs one ~10 s stalled request
(F-14); and the direct-path counters misreport, so an operator cannot tell from
the admin API whether `--udp` is in use at all (F-14).

**What is *not* worth doing**, each measured rather than assumed: the
response-header injection path (+0.6 %, F-4), yamux window resizing (§2.6),
striping one connection across carriers (N-5), and `SO_*BUF` on the SSH sockets
(F-9).

**Does the application limit the link? No — except on `--udp`** (F-16, §2.17).
This was the campaign's last open input, and it did not need the provider's
provisioning figure: measuring bore's CPU cost per byte and dividing the target
rate by it answers it directly.

- **The relay needs 0.67 of one core to saturate 1 Gbit/s** (5.74 s/GiB under
  load), and the *staging* box — a 2-core burstable Graviton2 t4g.micro —
  already delivers **334.7 MB/s = 2.68 Gbit/s** at 1.90 of its 2 cores. A single
  stream over the real internet did 1.94 Gbit/s. On the announced IONOS box
  (2 cores, non-burstable x86, ~1 Gbit/s) the link saturates at a third of one
  core, leaving ~1.3 cores of headroom. **No bulk-throughput work is justified.**
- **bore does use more cores when it has them** — measured 0.98 → 3.61 cores as
  `--carriers` and parallelism rise — so **5 Gbit/s is a sizing question, not a
  code question**: 3.34 cores at staging's efficiency, i.e. **6–8 vCPU with
  `--carriers 4`**.
- **`--udp` is the one real limit.** Its ceiling is **0.96 Gbit/s** — *below* a
  gigabit link — it costs **2.5× the CPU per byte**, and unlike every other
  limit in this campaign **no flag moves it**: `--carriers 1→4` and 1→4 parallel
  streams take it from 99 to 120 MB/s and no further. It also draws heavy
  instance-level PPS throttling (8 479 `pps_allowance_exceeded`/s at 90 k pps)
  that the relay never triggers at 239 k pps. This does not demote it as a
  *resilience* transport (still 2.8× the relay at 1 % loss) — it means `--udp`
  must never be chosen for bandwidth, and nothing in the docs says so today.
- A useful side result: **`--carriers` is also the lever that unlocks the extra
  cores**, which is a third independent argument for making the count adaptive.

---

## 1. Environment

### 1.1 Server (read-only observation, SSH as `ubuntu`)

| fact | value | how obtained |
| --- | --- | --- |
| instance | t4g.micro, `aarch64`, 2 vCPU, 903 MiB RAM (295 MiB available) | `uname -m`, `nproc`, `free -m` |
| deployment | Docker, `ghcr.io/manprint/bore:latest`, container `bore-server` | `docker ps` |
| container limits | **none** — `NanoCpus=0`, `CpuQuota=0`, `Memory=0` | `docker inspect` |
| container privileges | `privileged=true`, so `CAP_NET_ADMIN` is present | `docker inspect` |
| server argv | `bore server` (all configuration via environment) | `docker inspect` |
| congestion control | `cubic` | `sysctl` |
| default qdisc | `fq_codel` | `sysctl` |
| `net.core.{r,w}mem_max` | 4 MiB | `sysctl` |
| `net.ipv4.tcp_rmem` / `tcp_wmem` max | 7.0 MB / 4.0 MB | `sysctl` |
| UDP socket buffer clamping | **not clamped** — container is privileged, so `SO_*BUFFORCE` succeeds; no clamp warning in the log | `docker logs`, absence of the `configure_udp_socket_buffers` warning |

Effective server parameters, from `/admin/api/v1/config` and the mounted
`/config.yml` (host path `/home/ubuntu/bore-staging-brp/config.yml`):

| parameter | value |
| --- | --- |
| `port_range` | 9000-9100 |
| `control_port` | 7835 |
| `max_conns` / `max_carriers` | 1024 / 1024 |
| `vhost_base_domain` | `brp.0912345.xyz`, `mode: auto`, `reservations: []` |
| `vhost_http_port` / `vhost_https_port` / **`vhost_quic_port`** | 80 / 443 / **443 (UDP)** |
| `udp_socket_send_buffer` / `recv_buffer` | 16 MiB requested, forced (privileged container) |
| `udp_stream_receive_window` | 16 MiB |
| `udp_connection_receive_window` / `udp_send_window` | 256 MiB / 256 MiB |
| `udp_max_streams` | 8192 |
| `proxy_buffer_size` | unset (default 256 KiB) |
| also enabled | `tls`, `ssh_gateway`, `ssh_jump`, `vpn` |

The QUIC windows match the ratio the `--udp` invariant in `CLAUDE.md` requires
(256 MiB connection versus 16 MiB stream), so the carriers=1 stall condition
documented there is not in play here.

### 1.2 Client (my workstation — the constraint that dominates everything below)

| fact | value |
| --- | --- |
| only active path | WiFi 6 `wlp0s20f3`, 5 GHz, 80 MHz, HE-MCS11 NSS2 → PHY **1200.9 Mbit/s** each way, signal −52 dBm |
| wired interfaces | `eno0`, `enx00e04c361fa6` both **DOWN** |
| IPv6 | no AAAA for the domain, no IPv6 default route → **every flow is IPv4** |
| congestion control | `cubic`, qdisc `fq_codel` |
| RTT to server | ≈ 21 ms |
| origin | `scripts/bench_origin.py`, one write per response on a `TCP_NODELAY` socket |
| origin ceiling (loopback) | PUT 32 MiB at **4.6 GB/s**, GET `/stream/512MB` at **10.2 GB/s** |

### 1.3 The structural confound: double transit

Provider and consumer are the **same machine**. A tunnelled download crosses the
client radio twice — origin → provider → **server** (egress) and then
**server** → curl (ingress). WiFi is half-duplex, so both halves compete for the
same airtime.

- 1200.9 Mbit/s PHY → realistically 660–780 Mbit/s of *aggregate* goodput
- a tunnelled byte spends airtime twice → tunnel ceiling ≈ **41–49 MB/s**
- measured peak 48.75 MB/s = 409 Mbit/s → 818 Mbit/s of airtime = 68 % of PHY

Consequences, applied throughout this document:

1. **Absolute bulk numbers are lower bounds on server capability, not measurements of it.**
2. **Relative A/B comparisons remain valid** — the confound is identical across configurations.
3. Latency is essentially immune (1 KiB payloads never saturate airtime).

Confirmed from the server side: during load the server reports
`loadavg 0.00 0.00 0.00` and CPU pressure `avg10=0.00`. The t4g.micro was never
the bottleneck in any measurement below.

### 1.4 Second client: a same-region VM (the apparatus that removed the confound)

Because §1.3 caps every absolute number, the campaign was repeated from a
throwaway EC2 instance in the **same region as the server**. This is the client
used for every result in §2.7 onward, and it is the only client whose absolute
numbers describe the *server*.

| fact | value |
| --- | --- |
| instance | `c7i-flex.large`, x86_64, 2 vCPU, 4 GiB, Ubuntu 24.04 |
| address | `35.152.195.182` (destroyed at end of campaign) |
| RTT to server | **1.84 ms** (versus 21 ms from the workstation) |
| link headroom | iperf-class egress far above the server's ceiling; ~40× the server's measured bulk rate |
| congestion control | `cubic`, qdisc `fq_codel` (unchanged defaults) |
| binaries | my exact local `--release` build of `bore` copied over (needs GLIBC 2.34), `oha` 1.16.0, `scripts/bench_origin.py` |
| origin | same `bench_origin.py` on `127.0.0.1:5052`, patched per §4 |

Provider and consumer are still the same machine here, but that no longer
matters: the VM's link is not the bottleneck at any rate the server can produce,
and there is no half-duplex medium. Double transit costs *link* capacity, of
which there is a large surplus, instead of costing *airtime*, of which there was
none.

**What this changes.** Every workstation bulk number in §2.2–§2.6 is a
measurement of my WiFi. Every VM number in §2.7 onward is a measurement of the
server. The two are not comparable in absolute terms and are never mixed below.

---

---

## 2. Results

### 2.1 Latency

Keep-alive p50 is **35–47 ms in every configuration measured** (TCP relay and
QUIC direct, carriers 1 → 8). With RTT 21 ms the floor is 2×RTT + ~3 ms ≈ 45 ms.

**bore adds ≈ 3 ms over the unavoidable double round trip.** There is nothing to
optimise in the steady-state request path.

| workload | p50 | note |
| --- | --- | --- |
| keep-alive, c=1/8/32 | 35–47 ms | flat across every transport and carrier count |
| **new connection, c=8** | **79–86 ms** | **+45 ms versus keep-alive — one extra RTT + TLS** |
| c=32 throughput | 726–900 rps | |

The +45 ms per new connection is the largest latency lever measured, and it is
independent of the client-link confound.

### 2.2 Sustained bulk, 90 s per case

Computed as total bytes over elapsed span from server-side counters, not as a
mean of per-sample rates (the sampler's integer-second clock makes per-sample
peaks unreliable — one sample read 110 MB/s against a 30 MB/s mean).

| case | GB moved | MB/s | p10 | p50 | p90 |
| --- | --- | --- | --- | --- | --- |
| sustained-tcp-c1 | 2.49 | 28.69 | 17.7 | 33.9 | 40.7 |
| sustained-tcp-c4 | 2.66 | 30.23 | 18.0 | 35.9 | 43.4 |
| sustained-udp-c1 | 2.63 | 30.21 | 19.1 | 38.2 | 43.8 |
| sustained-udp-c4 | 2.65 | 30.20 | 19.3 | 38.8 | 42.8 |

All four converge on **28.7–30.2 MB/s**. Spread 5 %, while an A/B/A repeat of
`tcp-c1` alone drifted 10 %. **On a clean path this vantage point cannot
discriminate the transports or the carrier counts** — it measures the client
uplink. Server RSS stayed 33–39 MiB, `conn_rejections=0`, `direct_fallbacks=0`.

An earlier 30 s run had suggested QUIC direct was clearly ahead (37.2 vs
28.8 MB/s). At 90 s that difference disappears: the short run was window, not signal.

### 2.3 Upload (origin no longer the bottleneck)

| config | PUT 32 MiB, no Expect | PUT 32 MiB, `Expect: 100-continue` | GET 200 MiB |
| --- | --- | --- | --- |
| tcp-c1 | 28.86 | 14.65 | 35.13 |
| tcp-c4 | 23.92 | 12.96 | 27.81 |
| udp-c1 | **33.11** | 14.70 | 27.93 |
| udp-c4 | 30.21 | 14.96 | 28.95 |

Upload ≈ download in every configuration. Concurrent uploads (2, 4 and 8 in
parallel) all complete in under 1.2 s with aggregate 21.8–35.8 MB/s and no errors.

### 2.4 Impaired path (netem, egress only, toward the server IP)

The impaired leg is provider → server, which carries the **response body** of a
tunnelled download — so the download column isolates the tunnel transport. The
upload column rides curl → server, which is plain TLS and identical in both
configurations.

Applied with `scripts/vhost_staging_netem.sh` (prio band 3 + `u32` filter on
`ip dst <server>/32`). Since there is no IPv6 path, the IPv4-only filter catches
every flow.

**Clean control**

| config | download 100 MB | upload 32 MB | lat p50 | rps |
| --- | --- | --- | --- | --- |
| TCP relay c1 | 31.18 | 27.51 | 41.7 | 191.7 |
| QUIC direct c1 | 29.23 | 28.56 | 42.6 | 184.7 |
| TCP relay c4 | 32.83 | 27.03 | 43.6 | 179.6 |
| QUIC direct c4 | 27.93 | 29.31 | 37.3 | 208.1 |

**1 % loss**

| config | download 100 MB | upload 32 MB | lat p50 | rps |
| --- | --- | --- | --- | --- |
| TCP relay c1 | **1.35** | 18.40 | 44.0 | 163.2 |
| QUIC direct c1 | **29.13** | 1.70 | 45.4 | 159.7 |
| TCP relay c4 | **1.84** | 6.92 | 40.1 | 179.0 |
| QUIC direct c4 | **29.93** | 2.25 | 40.7 | 179.6 |

**+40 ms RTT**

| config | download 100 MB | upload 32 MB | lat p50 | rps |
| --- | --- | --- | --- | --- |
| TCP relay c1 | 26.48 | 10.28 | 124.9 | 62.6 |
| QUIC direct c1 | 20.19 | 19.66 | 123.2 | 63.9 |
| TCP relay c4 | 20.45 | 19.51 | 121.2 | 65.6 |
| QUIC direct c4 | 19.00 | 21.57 | 120.8 | 65.6 |

**+40 ms RTT and 1 % loss**

| config | download 100 MB | upload 32 MB | lat p50 | rps |
| --- | --- | --- | --- | --- |
| TCP relay c1 | **0.43** | 0.35 | *test collapsed* | **1** |
| QUIC direct c1 | **21.67** | 1.54 | 123.2 | **62.5** |

### 2.5 Protocol-selective impairment — the clean version of 2.4

Impairing both legs at once left the upload column ambiguous, so the matrix was
repeated impairing **one IP protocol at a time**. The tunnel data plane is TCP
for the relay and UDP for the QUIC direct path; the browser leg (which carries
upload bodies) is always TCP. So `proto=tcp` hits the relay plus the browser leg
and `proto=udp` hits only the direct data plane.

| condition | config | download 100 MB | upload 32 MB | lat p50 | rps |
| --- | --- | --- | --- | --- | --- |
| clean | TCP relay c1 | 28.55 | 29.39 | 38.1 | 209.3 |
| clean | QUIC direct c1 | 29.82 | 28.74 | 38.3 | 205.5 |
| 1 % loss, **TCP only** | TCP relay c1 | **1.16** | 1.43 | 44.8 | 163.2 |
| 1 % loss, **TCP only** | QUIC direct c1 | **25.66** | 1.74 | 40.1 | 186.9 |
| 1 % loss, **UDP only** | TCP relay c1 | **26.68** | 26.44 | 41.0 | 193.5 |
| 1 % loss, **UDP only** | QUIC direct c1 | **23.67** | 31.95 | 38.2 | 206.9 |

Every cell behaves as the model predicts, which is what makes this table
trustworthy where 2.4 was not:

- each transport is hurt only when **its own** protocol is impaired
- `TCP relay` under UDP-only loss is unchanged at 26.68 MB/s — the control that
  proves the filter does what it claims
- both uploads collapse under TCP-only loss and neither moves under UDP-only
  loss, because the browser leg is TCP either way

Stated as degradation of each transport when its own data plane loses 1 % of
packets:

| transport | clean | impaired | loss of throughput |
| --- | --- | --- | --- |
| TCP relay | 28.55 | 1.16 | **−96 %** |
| QUIC direct | 29.82 | 23.67 | **−21 %** |

**This supersedes the upload anomaly recorded in §5.** Under a clean measurement
both configurations collapse to 1.43–1.74 MB/s on upload, which is what Mathis
predicts for cubic at 1 % loss and 21 ms RTT (~0.7 MB/s, same order). The
18.40 MB/s recorded in the combined matrix was noise, not signal.

### 2.6 What exactly collapses on the relay — streams versus carriers

Two mechanisms could produce the −96 %: yamux per-stream flow control (each
stream has its own window, so concurrency would scale aggregate throughput) or
TCP congestion control on the carrier (all streams share one cwnd, so
concurrency changes nothing). Under 1 % loss on TCP only:

| carriers | 1 stream | 8 parallel streams, aggregate |
| --- | --- | --- |
| 1 | 1.52 | **1.00** |
| 4 | 1.69 | **5.00** |
| 8 | 1.28 | **8.00** |

Clean-path control, same shape: 34.10 / 25.00, 34.41 / 26.00, 26.00 / 24.00 —
concurrency does not add throughput because the client link is already saturated.

**The bottleneck is the carrier's TCP congestion window, not yamux flow
control.** Adding streams on one carrier buys nothing (1.00 MB/s across eight of
them); adding carriers buys roughly **1 MB/s each**, linearly.

Two consequences that matter more than the headline number:

1. To match the 23.67 MB/s that QUIC direct delivers over a *single* connection,
   the TCP relay would need on the order of **24 carriers**.
2. **A single large transfer over the relay is capped near 1.5 MB/s under loss no
   matter how many carriers are configured**, because one proxied connection is
   pinned to one carrier (round-robin is per proxied connection, by design — see
   the `--carriers` invariant in `CLAUDE.md`). Only switching path fixes that
   case.

Two results dominate everything else in this campaign:

- **Under 1 % loss the TCP relay download collapses to 1.35–1.84 MB/s while QUIC
  direct holds 29.1–29.9 MB/s — a 17–22× gap, with QUIC losing almost nothing
  against its own clean baseline.**
- **Under loss plus latency the TCP relay stops serving small requests
  altogether** (`rps=1`, percentiles unmeasurable) while QUIC direct keeps
  62.5 rps at p50 123 ms.

The small-object collapse cannot be bandwidth: 1 KiB responses at c=8 on a
carriers=1 relay all share one TCP connection, so a single lost segment blocks
every yamux stream queued behind it. QUIC gives each proxied connection its own
stream with independent loss recovery. This is the textbook head-of-line
argument for QUIC, now measured on bore.

The bulk figure has a second, independent explanation worth separating: 1.35 MB/s
is what a 256 KiB yamux window yields at an RTT inflated to ~200 ms by
retransmission. Whether the relay collapse is TCP congestion control, yamux flow
control, or both, is **not yet resolved** — see §5.

### 2.7 VM calibration (V0) — proving the client is no longer the limit

Run before any tunnel, to establish what the apparatus itself can do:

| leg | result |
| --- | --- |
| origin loopback GET `/stream` | multi-GB/s (same patched origin as §4) |
| origin loopback PUT | multi-GB/s |
| RTT to server | 1.84 ms |
| TLS handshake to server | single-digit ms |

The origin and the VM link are both more than an order of magnitude faster than
anything the tunnel produces, so from here on the tunnel *is* the measurement.

### 2.8 Transport A/B from the VM — the earlier ranking reverses

8 s sustained per case, download measured from the server's own `relay_tx_bytes`
counter (so the number is what the *server* emitted, not what curl chose to
report). `path=` is proven per case: `direct_stream_opens` increments only on the
QUIC direct path.

| case | path | server-side rate | client CPU | client warnings |
| --- | --- | --- | --- | --- |
| tcp c=1 | relay-tcp | **136.37 MB/s** | 21.4 % | 1 |
| udp c=1 | direct-quic | 110.81 MB/s | 39.6 % | 3 |
| tcp c=2 | relay-tcp | **161.67 MB/s** | 24.5 % | 1 |
| udp c=2 | direct-quic | 76.72 MB/s | 26.2 % | 5 |
| tcp c=4 | relay-tcp | **138.45 MB/s** | 22.7 % | 1 |
| udp c=4 | direct-quic | 123.60 MB/s | 46.3 % | 9 |
| tcp c=1 (repeat, control) | relay-tcp | **175.46 MB/s** | 27.7 % | 1 |

Three things come out of this table, and only two of them are conclusions.

1. **On a fast, clean path the TCP relay beats QUIC direct**, by 25–110 %
   depending on the pair. This is the opposite of the workstation result
   (§2.4–§2.5), and both are correct for their own path: kernel TCP with
   TSO/GSO/GRO offload against a userspace QUIC stack doing per-packet AEAD on
   two Graviton cores. QUIC wins when the path is lossy, TCP wins when it is not.
2. **QUIC costs roughly 2× the client CPU for less throughput** here (46.3 % vs
   22.7 % at c=4). On a CPU-constrained provider this is a real operating cost,
   not just a benchmark artifact.
3. **Not a conclusion:** the carrier ordering. The repeat of `tcp c=1` came back
   at 175.46 MB/s against the 136.37 MB/s of the same case ten cases earlier —
   **29 % drift on an identical configuration.** Any effect smaller than 29 %
   in this table is noise. `c=2 > c=1 > c=4` is therefore *not* established, and
   the carrier question is re-opened in §5.

The server-side ceiling this table brackets: **136–175 MB/s TCP relay,
77–124 MB/s QUIC direct**, i.e. 1.1–1.4 Gbit/s and 0.6–1.0 Gbit/s.

### 2.9 Request rate and 10-minute operational stability

`oha -c 32` on `/1k` for **10 minutes** (~6.5 M requests), sampled every 30 s.
This is the test of the user's third goal — "stabilità operativa" — not of peak
bandwidth.

| metric | result over 20 samples |
| --- | --- |
| rps | 10 236 – 11 245, **no downward trend** |
| p50 | 2.78 – 2.88 ms (RTT is 1.84 ms, so the server adds ≈ 1 ms) |
| p99 | 3.87 – 11.79 ms |
| success rate | **1.0 in every sample** |
| server RSS | **15.4 MiB, flat** (15.4 → 15.5 → 15.4) |
| connection rejections | **0** |
| leaked/ghost registrations | 0 |

No rps decay, no memory growth, no rejections, no error responses over 6.5 M
requests. On this axis vhost is production-stable.

### 2.10 Where the server's CPU actually goes

Sampled from `/proc/stat` on the server host during the run above:

| component | share of one core-equivalent |
| --- | --- |
| softirq | **37–40 %** |
| sys | 22–24 % |
| usr | **13–15 %** |
| steal | **0.0 %** |

Total busy ≈ 1.5 of 2 cores at 11 k rps. Two facts follow.

1. **82 % of the CPU cost is kernel, not bore.** Packet processing, TLS syscalls
   and socket wakeups dominate; application code is a seventh of the bill.
   Optimizing bore's own hot path can only attack the `usr` sliver. The levers
   that matter are the ones that reduce *syscalls and packets per request* —
   which is exactly why F-4 (an extra write per response) is worth measuring.
2. **`steal` is 0.0 %, and does not decay over 10 minutes.** The staging
   instance is a burstable t4g.micro but is running in **unlimited mode**: there
   is no credit cliff to plan around. The "CPU credit exhaustion" hypothesis is
   dead for this instance, and irrelevant for the announced IONOS production VPS,
   which is not burstable.

Container CPU during the bulk runs of §2.8 was 92–127 % (i.e. up to 1.3 cores)
for 136–175 MB/s — bulk is cheaper per byte than small requests are per request.

---

### 2.11 vhost through the SSH ingress gateway

The SSH leg is TCP relay only by design — no `--udp`, no `--carriers`. The
comparison that matters is therefore *SSH gateway* against *native TCP relay*,
same origin, same VM, same server, interleaved back to back.

**Establishment and lifecycle (S1).** `ssh -R vhost/<label>:80:127.0.0.1:5052`
registers, serves 200 at 23 ms first-byte, and the label is **released 0 s** after
the client is SIGKILLed. The gateway's banner reports the resolved policy,
including the seven response headers the admin API hides (see F-6):

```
Vhost tunnel established
  Public URL:       http://<label>.brp.0912345.xyz
                    https://<label>.brp.0912345.xyz
  Mode:             HTTP + HTTPS (no redirect)
  HTTPS policy:     inherit (server --vhost-mode)
  Identity:         test
  Max-conns:        n/a for vhost (server-wide --max-conns applies)
  Response headers: 7 configured: Content-Security-Policy, Permissions-Policy, ...
```

**Throughput (S2), two interleaved rounds, download measured server-side:**

| case | round 1 | round 2 | mean |
| --- | --- | --- | --- |
| SSH single stream | 100.98 MB/s | 159.45 MB/s | 130 MB/s |
| SSH 8 parallel, aggregate | 100.98 MB/s | 109.37 MB/s | **105 MB/s** |
| native single stream | 184.60 MB/s | 148.86 MB/s | 167 MB/s |
| native 8 parallel, aggregate | 219.45 MB/s | 178.91 MB/s | **199 MB/s** |
| SSH upload 256 MB | 150.88 MB/s | 169.74 MB/s | 160 MB/s |
| native upload 256 MB | 157.20 MB/s | 60.49 MB/s | (60.49 is an outlier) |

The single-stream spread is again ±30 %, so the single-stream gap is not
resolvable. **The aggregate is a different matter and is a real effect:** eight
parallel downloads over the SSH gateway aggregate to *no more* than one stream
does (105 versus 130 MB/s), while native gains from parallelism
(199 versus 167 MB/s). See F-9.

**Latency and request rate (S3), 6 s per point:**

| load | SSH gateway | native TCP relay |
| --- | --- | --- |
| 1 KiB c=1 | 355 rps, p50 2.65, p99 3.29 ms | 376 rps, p50 2.60, p99 3.13 ms |
| 1 KiB c=8 | 2 891 rps, p50 2.73, p99 3.32 ms | 2 718 rps, p50 2.68, p99 8.88 ms |
| 1 KiB c=32 | 11 426 rps, p50 2.72, p99 3.50 ms | 11 018 rps, p50 2.81, p99 3.88 ms |
| 1 KiB c=64 | **18 448 rps**, p50 3.32, p99 4.73 ms | 14 223 rps, p50 4.21, p99 7.05 ms |
| 100 KiB c=8 | 1 281 rps, 124.92 MB/s, p95 7.11 ms | 1 497 rps, 146.06 MB/s, p95 7.07 ms |
| new conn per req, c=8 | 916 rps, p50 8.58 ms | 1 054 rps, p50 7.46 ms |

Success rate 1.0 everywhere. The SSH gateway is **not** the slow path for small
requests — at c=64 it delivered 30 % more requests per second at a lower p50
than native did. It is the slow path for *bulk*, and only in aggregate.
Connection setup is ~15 % cheaper on native (916 vs 1 054 rps), consistent with
a `forwarded-tcpip` channel open costing more than a yamux substream open.

**Head-of-line blocking (S4).** Six rate-limited readers (200 KiB/s) pinned
against a 100 MB stream each, then fast requests on top:

| transport | fast request TTFB | rps under the slow readers | server-side active |
| --- | --- | --- | --- |
| SSH gateway | 14.5 / 13.9 / 14.3 ms | 3 018 rps, p99 3.22 ms, ok 1.0 | 6 |
| native | 13.2 / 12.3 / 12.7 ms | 3 066 rps, p99 3.73 ms, ok 1.0 | 6 |

**No head-of-line blocking on either transport.** This is the vendored russh
window-backpressure fix doing its job; the two paths are indistinguishable here.

**Parameter handling (S5).** `https=on force-https=on basic-auth=user:pass
max-conns=7 notes=perftest` produced exactly one warning —
`max-conns: not applicable to vhost tunnels; ignoring` — and the banner then
reported `HTTPS policy: redirect`, `Basic-auth: enabled`, `Notes: perftest`.
So `https`/`force-https`/`basic-auth` **are** honoured per-tunnel on vhost now,
and `max-conns` is the only inapplicable one, correctly surfaced. I-SSH8 holds,
narrowed to the parameter that is genuinely a no-op.

**Wedged client (S6) — the sharpest result of the campaign.** Same procedure on
both transports: a live download in flight, then `SIGSTOP` the provider process
(TCP stays alive and acknowledges; the application answers nothing).

| | SSH gateway | native vhost |
| --- | --- | --- |
| t+20 s | registered, `relay_tx_bytes` still rising | registered, `relay_tx_bytes` 52 655 596 |
| t+40 s | **released** | registered, bytes frozen at 52 655 596 |
| t+60 / 90 / 120 s | released | **still registered**, bytes still frozen |
| re-register while wedged | **succeeds** — new tunnel established | **rejected**: `server error: subdomain '…' in use` |
| after SIGKILL | already gone | released after 1 s |

The SSH gateway evicts the wedged session between 20 s and 40 s (I-SSH10: 15 s
bounded channel open, two consecutive timeouts, then session abort). The native
vhost path has no equivalent and holds the subdomain **indefinitely**. This is
F-1, now with its fix already present in the same binary on the neighbouring
code path.

**Same-identity takeover (S7/S7b).** First attempt was inconclusive because the
harness sampled at 4 s and only checked that *a* registration existed. Redone
with the challenger pointed at a *different* origin so the served body names the
holder:

| time | registered peer | uptime | body served |
| --- | --- | --- | --- |
| incumbent only | `…:43782` | 3 s | incumbent's origin |
| t+3 s after challenger | `…:43782` | 3 s | incumbent's origin |
| t+8 s | **`…:50818`** | reset to 2 s | **`CHALLENGER`** |
| t+20 s | `…:50818` | 7 s | `CHALLENGER` |

Exactly **one** registration at every instant, the incumbent's process was
disconnected (`alive=no`), and killing the challenger dropped the label
immediately (`404 not found`). I-SSH5 works; takeover completes in **3–8 s**.

**Cross-trust-domain rejection (S8/S9).** An SSH identity cannot take a label
held by a *native* provider — `remote port forwarding failed for listen port 80`,
one row kept, still serving 200. A wrong password never authenticates
(`Permission denied`, 0 registrations). The two trust domains stay separate.

### 2.12 F-4 A/B — what the response-header injection path actually costs

Run entirely on loopback on the 16-core workstation (private `bore server`, private
vhost client, `bench_origin.py`, `oha`): zero staging traffic and the server is
the only interesting CPU consumer. The metric is **CPU microseconds per request
taken from the server process's own `utime+stime`**, which is precisely what an
extra write syscall and TLS record per response would inflate. Three interleaved
rounds per scheme, 20 s at c=16 each, header policy verified on the wire every
run (`injected_hdrs=7` vs `0`).

| scheme | policy | mean CPU µs/req | mean rps | bulk |
| --- | --- | --- | --- | --- |
| HTTP | no headers | 28.83 (29.7 excluding the first, warm-up run) | 52 955 | ~1.6 GB/s |
| HTTP | 7 headers injected | 29.87 | 52 571 | ~1.6 GB/s |
| HTTPS | no headers | 32.30 | 51 209 | ~1.4 GB/s |
| HTTPS | 7 headers injected | 32.50 | 51 137 | ~1.4 GB/s |

**The injection path costs nothing measurable: +0.6 % CPU on HTTPS, +0.5 % on
HTTP once the warm-up run is excluded, and rps within 0.7 %.** The single 27.1
outlier was the very first case of the whole run.

F-4's *description* stands — every vhost response on this staging server does
take `relay_response_injected` + `copy_one_direction_with_shutdown`. Its
*performance implication does not*: this is not an optimization lever, and the
enhancement plan should not spend effort here. (The flush-after-every-write is a
correctness requirement, not overhead worth removing — see
`docs/VHOST_INJECTED_FLUSH_FIX.md`.)

---

### 2.13 Stability, races and ghost sessions (G1–G9)

Run from the VM against staging. Every case ends by checking that the
registration is actually gone, because a leaked registration is the failure mode
that costs a user their subdomain.

| case | what it does | result |
| --- | --- | --- |
| **G1** wedged provider, both transports, 180 s watch | live download, then `SIGSTOP` the provider | **TCP relay: still registered at t+180 s**, `relay_tx_bytes` frozen since t+30 s, re-register rejected `subdomain in use`. **`--udp`: identical**, and `active` even drops to 0 while the label stays held. Released 0 s after SIGKILL. → F-1 |
| **G2** four providers race one label | 4 simultaneous registrations of the same subdomain | exactly **1** entry, **3 rejected**, serving 200. First-wins is clean; no race, no duplicate row |
| **G3** reconnect storm | 20 register/deregister cycles on one label | **0** failures, **0** releases slower than 3 s, nothing left registered |
| **G4** provider killed mid-response | SIGKILL during a rate-limited 1 GB download | client got 40 189 846 bytes then `curl exit 18` (partial transfer) — a truncated body, correctly detectable. Label released 0 s |
| **G5** origin faults | origin killed under a live tunnel, then restarted; a second tunnel pointed at a closed port; an unknown subdomain | origin dead → **`http=000` (bare connection close, no 502)**; closed port → **`http=000`**; unknown subdomain → **404** correctly; origin restarted → **200 on the same tunnel**, no reconnect needed. → F-12 |
| **G6** UDP blackholed mid-session | first attempt invalid (harness put `dev` in the wrong `tc` position); redone in §2.14 | see §2.14 |
| **G7** ten simultaneous tunnels, mixed transports | 10 tunnels (5 TCP, 5 `--udp`) | **10/10 registered, 10/10 serving 200**, server RSS 21.2 MiB, 0 rejections, **0 still registered 8 s after all providers killed** |
| **G8** concurrency versus server memory | 16 → 512 concurrent slow readers on one tunnel | see the table below — the sharpest transport difference in the campaign |
| **G9** stalled-stream cliff | 4 → 32 slow readers at `--carriers 1` on `--udp` | **no cliff**: fast requests stayed at 12–17 ms at every N up to 32. But RSS reached **536.8 MiB** |

**G8 — server RSS against concurrent connections, one tunnel:**

| concurrent slow readers | TCP relay RSS | TCP relay fresh-request time | QUIC direct RSS | QUIC direct fresh-request time |
| --- | --- | --- | --- | --- |
| baseline | 19.7 MiB | — | 17.0 MiB | — |
| 16 | 21.8 MiB | 12 ms | 32.6 MiB | 13 ms |
| 64 | 42.4 MiB | 16 ms | 71.2 MiB | 13 ms |
| 256 | 87.4 MiB | **966 ms** | 260.3 MiB | 14 ms |
| 512 | 95.1 MiB | **1 436 ms** | **424 MiB** | 121 ms |
| after release | **16.5 MiB** | — | 428.8 MiB *(still 2 connections open)* → **33.6 MiB** once fully drained | — |

Two opposite costs, both real, and they point in different directions:

- **TCP relay is memory-cheap and latency-fragile at high concurrency**: 95 MiB
  at 512 connections, but a fresh request behind them takes **1.4 s**.
- **QUIC direct is latency-robust and memory-expensive**: 14 ms at 256
  connections, but **4.5× the RSS**, and it is the QUIC receive windows that
  cause it (`--udp-connection-receive-window` 256 MiB,
  `--udp-stream-receive-window` 16 MiB — a ceiling, not a reservation, but a
  ceiling a slow reader will happily fill).

The memory *is* returned — RSS fell back to 33.6 MiB, the container never
restarted (`RestartCount 0`) and there was no OOM kill — but the peak is what
sizing must survive.

**Collateral damage worth recording.** During G9 at 32 stalled QUIC streams the
server reached **536.8 MiB RSS on a host with 903 MiB of RAM**. Two subsequent
requests timed out at 10 s, the TCP arm of G9 failed to register at all, and one
of the operator's own unrelated tunnels (`tennis1`) lost its control connection
and reconnected. So on a 1 GiB host, **~32 slow readers on a single `--udp`
vhost tunnel is enough to degrade the whole server.** On the announced 2 GiB
production VPS the same shape needs roughly twice as many — still reachable by
one ordinary client on a bad network. → F-13

### 2.14 Protocol-selective impairment from the VM

`tc` impairs traffic toward the server **by IP protocol**, which separates the
two legs a tunnelled transfer uses:

- **provider → server** is the tunnel data plane: **TCP** for the relay,
  **UDP/QUIC** for the direct path
- **consumer → server** is always **TCP**, and it carries upload bodies

So `tcp` impairment hits the relay's data leg *and* both uploads; `udp`
impairment hits only the direct path's data leg. Each transport therefore
appears twice per condition: once impaired on its own data plane, once as an
unimpaired **control** that proves the filter is doing what it claims.

| condition | transport | download | upload | rps | p50 |
| --- | --- | --- | --- | --- | --- |
| **clean** | relay-tcp | 167.64 MB/s | 152.23 MB/s | 3 036 | 2.56 ms |
| | direct-quic | 115.24 MB/s | 119.80 MB/s | 2 811 | 2.57 ms |
| **1 % loss, TCP only** | relay-tcp *(impaired)* | **45.26 MB/s** | 51.04 MB/s | 1 489 | 2.65 ms |
| | direct-quic *(control)* | **115.40 MB/s** — unchanged | 38.54 MB/s | 1 739 | 2.60 ms |
| **1 % loss, UDP only** | relay-tcp *(control)* | 133.68 MB/s | 143.61 MB/s | 3 064 | 2.57 ms |
| | direct-quic *(impaired)* | **126.70 MB/s** — barely moved | 117.69 MB/s | 3 076 | 2.52 ms |
| **+40 ms, TCP only** | relay-tcp *(impaired)* | **27.10 MB/s** | 31.64 MB/s | 95 | 83.4 ms |
| | direct-quic *(control)* | 81.92 MB/s | 22.84 MB/s | 181 | 43.3 ms |
| **+40 ms, UDP only** | relay-tcp *(control)* | 176.14 MB/s | 144.85 MB/s | 3 054 | 2.56 ms |
| | direct-quic *(impaired)* | **59.41 MB/s** | 103.02 MB/s | 184 | 43.1 ms |
| **+40 ms and 1 % loss, TCP only** | relay-tcp *(impaired)* | **1.14 MB/s** | 0.56 MB/s | 84 | 83.3 ms |
| | direct-quic *(control)* | 81.19 MB/s | 0.92 MB/s | 178 | 43.2 ms |

**Read the impaired rows against each other, never a row against its own
control.** The apples-to-apples pairs, both with the data leg impaired
identically:

| data leg impairment | TCP relay | QUIC direct | ratio |
| --- | --- | --- | --- |
| 1 % loss | 45.26 MB/s | **126.70 MB/s** | QUIC **2.8×** |
| +40 ms RTT | 27.10 MB/s | **59.41 MB/s** | QUIC **2.2×** |
| clean | **167.64 MB/s** | 115.24 MB/s | relay **1.5×** |

The controls are what make this trustworthy, and they are excellent:
QUIC's download under **TCP**-only loss is 115.40 MB/s against 115.24 MB/s clean
(untouched), and the relay's download under **UDP**-only loss is 133.68 MB/s
(untouched). The transports really are riding the protocols claimed, verified by
impairment rather than only by a counter — which matters, see F-14.

Two further readings:

- **Uploads collapse under TCP impairment on both transports** (51.04 and
  38.54 MB/s under 1 % loss; 0.56 and 0.92 MB/s under 40 ms + 1 % loss), because
  the consumer leg carrying the request body is TCP either way. `--udp` does
  nothing for upload on a lossy consumer path. This is the clean version of the
  ambiguous cell in §2.4.
- **`+40 ms and 1 % loss` puts the relay at 1.14 MB/s**, which is the Mathis
  regime: `MSS/(RTT·√p)` ≈ 1448 / (0.042 · 0.1) ≈ 0.34 MB/s per flow. Nothing
  about bore is wrong here; a single cubic flow simply cannot go faster.
- The p50 gap between the impaired and control rows (83 ms vs 43 ms) is an
  **artifact of selective impairment**, not a property of the transports: the
  relay pays the added delay on both legs because both are TCP, while the
  direct path's consumer leg stays fast. On a real bad network both legs are on
  the same path and both pay. Do not quote 43 ms vs 83 ms as a transport result.

**G6 redo — UDP fully blackholed mid-session on a live `--udp` tunnel:**

| step | result |
| --- | --- |
| before | 116.47 MB/s down, 116.51 MB/s up, direct proven |
| UDP toward the server 100 % dropped, single request | **`http=000` after 9.90 s** |
| throughput during the blackhole | 179.09 MB/s down, 153.68 MB/s up — i.e. **full relay speed** |
| after clearing the blackhole | 112.14 MB/s down — back to QUIC speed |
| `direct_stream_opens` | warmup 1 → **12 during the blackhole** → 22 after |
| `direct_fallbacks` (server metric) | **0** |

The fallback itself works and is seamless in aggregate — throughput during a
total UDP outage was *higher* than before, because the relay is the faster
transport on this clean path (F-8). But the **first** request after the blackout
was lost: ~10 s with no response and no status line. And both observability
counters misreport: `direct_stream_opens` kept rising while UDP was 100 %
dropped, and `direct_fallbacks` stayed at 0 through a fallback that plainly
happened. → F-14

### 2.15 Paired A/B — the design that survives the drift

The absolute rate on this server drifts by ~30 % between identical runs (§2.8),
which is larger than most of the effects being measured. Independent means
cannot resolve them. So each comparison is run **paired**: both halves back to
back within seconds, the order alternating between pairs, and the statistic is
the **median of the per-pair ratios**. Slow drift is common to both halves of a
pair and cancels in the ratio.

**A1 — TCP relay against QUIC direct, `--carriers 1`, 12 s per half:**

| pair | TCP relay | QUIC direct | ratio |
| --- | --- | --- | --- |
| 1 | 176.80 MB/s | 117.48 MB/s | 1.505 |
| 2 | 182.43 | 113.15 | 1.612 |
| 3 | 160.04 | 121.90 | 1.313 |
| 4 | 171.42 | 117.55 | 1.458 |
| 5 | 204.50 | 125.95 | 1.624 |
| 6 | 227.29 | 131.49 | 1.729 |
| 7 | 231.82 | 118.66 | 1.954 |
| 8 | 183.45 | 117.11 | 1.566 |
| | | | **median 1.589** |

**8 of 8 pairs favour the TCP relay**, ratios 1.31–1.95, while the absolute TCP
figure wandered from 160 to 232 MB/s across the same eight pairs. That is exactly
what the paired design is for: **the relay is 1.59× faster than QUIC direct on a
clean low-RTT path**, and the number is now robust rather than suggestive. The
path was proven per half (`tcp=relay udp=direct` on all eight).

Incidentally this raises the observed server ceiling: the best single pair hit
**231.82 MB/s (1.86 Gbit/s)** on the relay, and QUIC direct stayed in a narrow
113–131 MB/s band regardless.

**A2 — 4 carriers against 1 carrier on the TCP relay, same design:**

| pair | c=1 | c=4 | ratio |
| --- | --- | --- | --- |
| 1 | 165.90 MB/s | 161.01 MB/s | 0.971 |
| 2 | 150.08 | 191.10 | 1.273 |
| 3 | 239.07 | 125.51 | 0.525 |
| 4 | 265.79 | 177.32 | 0.667 |
| 5 | 167.83 | 157.88 | 0.941 |
| | | | **median 0.941** |

**Carriers do not help on a clean path and slightly hurt** (4 of 5 pairs below
1.0). The default of `--carriers 1` is correct here. This does not contradict
§2.6, where carriers scaled aggregate throughput almost linearly under 1 % loss:
carriers buy loss and cwnd isolation, and on a path with neither there is
nothing to isolate — only extra connections to schedule. It is exactly the case
for making the count **adaptive** rather than raising the default (candidate 4).

**A3 — latency suite, native TCP relay against native QUIC direct:**

| load | TCP relay | QUIC direct |
| --- | --- | --- |
| 1 KiB c=1 | 368 rps, p50 2.60, p99 4.84 ms | 361 rps, p50 2.72, p99 3.28 ms |
| 1 KiB c=8 | 3 062 rps, p50 2.57, p99 3.11 ms | 3 089 rps, p50 2.52, p99 3.21 ms |
| 1 KiB c=32 | **11 287 rps**, p50 2.74, p99 3.86 ms | 9 845 rps, p50 2.80, p99 9.32 ms |
| 1 KiB c=64 | 14 302 rps, p50 4.21, p99 7.00 ms | **16 226 rps**, p50 3.79, p99 5.42 ms |
| 100 KiB c=8 | **1 521 rps, 148.38 MB/s**, p95 6.77 ms | 802 rps, 78.17 MB/s, p95 17.18 ms |
| new connection per request, c=8 | 1 054 rps, p50 **7.47** ms | 1 060 rps, p50 **7.44** ms |
| **1 KiB c=8 while a bulk transfer is in flight** | **288 rps, p50 28.25, p95 58.60, p99 67.13 ms** | **560 rps, p50 9.14, p95 30.25, p99 219.08 ms** |

Three readings, one of which is new and matters more than the rest.

1. **Connection setup costs ~4.9 ms of server time, not 45 ms.** Keep-alive p50
   is 2.6 ms and new-connection p50 is 7.4 ms, *identical on both transports*.
   The +45 ms measured from the workstation was TLS handshake round trips at
   21 ms RTT, not server work. So the HTTP/2 lever's value scales with the
   **client's** RTT: large for real remote browsers, ~5 ms of it attributable to
   the server. Worth correcting in the candidate assessment.
2. **The relay is the better bulk and mid-size transport, QUIC the better
   high-concurrency one.** 148 MB/s vs 78 MB/s at 100 KiB c=8; 16 226 vs 14 302
   rps at c=64.
3. **A bulk transfer in flight wrecks concurrent small-request latency on both
   transports.** This is the case a real page load hits — one large asset or
   download alongside many small ones — and it is the worst latency result in the
   entire campaign:

   | | idle p50 | under bulk p50 | degradation | under bulk p99 |
   | --- | --- | --- | --- | --- |
   | TCP relay | 2.57 ms | **28.25 ms** | **11×** | 67 ms |
   | QUIC direct | 2.52 ms | **9.14 ms** | 3.6× | **219 ms** |

   Request throughput collapses with it: 3 062 → 288 rps on the relay,
   3 089 → 560 on QUIC. Neither transport isolates a latency-sensitive request
   from a bandwidth-hungry one. → F-15

### 2.16 F-15 follow-up — carriers do mitigate the under-bulk latency

The cheap experiment F-15 asked for: the relay pins one proxied connection to
one carrier (round-robin per connection), so if the mechanism is *carrier
saturation*, more carriers should give a small request a better chance of
landing on a carrier the bulk transfer is not filling. One run, all cells
sequential under identical conditions, `oha -c 8` on `/1k`:

| tunnel | idle p50 / p95 | 1 bulk in flight: p50 / p95 / rps | 2 bulk in flight: p50 / p95 / rps |
| --- | --- | --- | --- |
| relay c=1 | 2.51 / 2.89 ms | 2.59 / 27.27 ms / 1 227 | **45.11** / 57.46 ms / 181 |
| relay c=2 | 2.62 / 3.56 ms | 2.95 / 17.46 ms / 1 419 | 60.47 / 109.91 ms / 128 |
| relay c=4 | 2.53 / 2.86 ms | 7.09 / 37.91 ms / 629 | 36.56 / 77.77 ms / 205 |
| relay c=8 | 2.55 / 2.89 ms | 3.42 / **14.06** ms / 1 464 | **14.81** / 42.49 ms / 393 |
| QUIC c=1 | 2.53 / 2.92 ms | 8.07 / 28.59 ms / 710 | **7.07** / 14.82 ms / 1 012 |
| QUIC c=4 | 2.50 / 2.90 ms | 5.64 / 41.64 ms / 690 | 8.04 / 59.11 ms / 494 |

**The mechanism is confirmed and the mitigation works.** On the relay, going from
1 to 8 carriers takes p95 under one bulk transfer from 27.3 ms to 14.1 ms, and
p50 under two bulk transfers from **45.1 ms to 14.8 ms — a 3× improvement** —
with request rate roughly doubling (181 → 393). The `c=4` cells are out of
order in both columns, so treat the magnitudes as single samples on a server
with 29 % drift; the *direction* is consistent across two independent load
levels and two percentiles.

**QUIC needs no carriers for this.** `c=1` already gives 7.07 ms p50 under two
bulk transfers, better than the relay at any carrier count, because its streams
are genuinely independent — and `c=4` makes it slightly *worse*, consistent with
the documented rule that carriers on the direct path apply per-connection
round-robin and buy nothing when there is nothing to isolate.

This changes the shape of the adaptive-carrier candidate. The trigger is not
only packet loss (§2.6); it is **concurrent bulk load on the same tunnel**, which
is a condition the client can observe locally and cheaply — it knows how many
proxied connections it has open and roughly how much data each is moving. A
carrier pool that grows with the number of active high-rate streams would fix
the common case without costing anything on an idle tunnel.

Note also what the table does *not* show: no configuration gets back to the
2.5 ms idle p50. The best case under load is 7–15 ms. Carriers mitigate F-15;
they do not solve it.

---

### 2.17 Application ceiling and core scaling — does bore itself limit the link?

The operator's question: the IONOS production link is ~1 Gbit/s, a future AWS
box at 5 Gbit/s is not excluded, and *the VM's own limit is accepted*. What has
to be established is that **the application is not the limit**.

Answering that needs three measurements, because a single throughput number
cannot distinguish "bore is slow" from "this VM is small":

1. an application ceiling with the network removed (loopback, fast CPU),
2. staging's CPU cost per byte on the real path, as *host-total* CPU including
   the softirq the kernel spends on the container's behalf,
3. a discriminator that says whether staging stalls on CPU or on the link.

#### 2.17.1 Loopback ceiling — the application with no network in the way

A private `bore server` on the 16-core x86 workstation, tunnel and consumer over
loopback, carrying the same `default_response_headers` policy staging uses (so
the F-4 injection path is exercised, not bypassed). Sweep over transport ×
`--carriers` × parallel streams. The server process is pinned with
`taskset -c 0,1` to emulate the announced 2-core production box, then unpinned
to test whether bore uses more cores when it has them.

**Loopback MTU is 65536, so the absolute MB/s column is an upper bound and
nothing else.** The transferable column is `s/GB` — CPU seconds per GiB moved —
and `cores_used`, which says whether the work spreads.

Pinned to 2 cores:

| transport | carriers | parallel | MB/s | Gbit/s | s/GB | cores used | threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| relay-tcp | 1 | 1 | 1380.4 | 11.04 | 0.68 | 0.92 | 4 |
| relay-tcp | 1 | 4 | 1387.4 | 11.10 | 1.08 | 1.47 | 4 |
| relay-tcp | 2 | 4 | 1998.5 | 15.99 | 0.92 | 1.79 | 4 |
| relay-tcp | 4 | 4 | **2060.5** | **16.48** | 0.95 | 1.91 | 4 |
| relay-tcp | 4 | 8 | 1857.8 | 14.86 | 1.03 | 1.87 | 4 |
| direct-quic | 1 | 1 | 674.4 | 5.40 | 1.33 | 0.88 | 4 |
| direct-quic | 1 | 4 | 724.0 | 5.79 | 2.08 | 1.47 | 4 |
| direct-quic | 4 | 4 | 841.5 | 6.73 | 2.39 | 1.96 | 4 |

Unpinned, all 16 cores available:

| transport | carriers | parallel | MB/s | Gbit/s | s/GB | cores used | threads |
| --- | --- | --- | --- | --- | --- | --- | --- |
| relay-tcp | 1 | 1 | 982.3 | 7.86 | 1.02 | 0.98 | 18 |
| relay-tcp | 1 | 4 | 1470.2 | 11.76 | 1.74 | 2.50 | 18 |
| relay-tcp | 4 | 4 | 2187.2 | 17.50 | 1.45 | 3.09 | 18 |
| relay-tcp | 4 | 8 | **2440.8** | **19.53** | 1.51 | 3.61 | 18 |
| relay-tcp | 8 | 8 | 2379.2 | 19.03 | 1.45 | 3.37 | 18 |
| direct-quic | 1 | 4 | 631.8 | 5.05 | 3.36 | 2.07 | 18 |
| direct-quic | 4 | 8 | 1103.7 | 8.83 | 3.41 | 3.67 | 18 |

Three things fall out of this:

- **bore scales across cores.** `cores_used` goes from 0.98 to 3.61 as carriers
  and parallelism rise. It is not a single-threaded design that would cap a
  fast link no matter how large the box.
- **`--carriers` is the mechanism that unlocks the extra cores.** At `c=1` the
  server never passes ~1.0 core on one stream and ~2.5 with four; at `c=4` it
  reaches 3.6. This is a *different* reason to want carriers than F-2 (loss) or
  §2.16 (under-bulk latency), and it points the same way.
- **Two cores are not obviously the constraint on a fast CPU.** 2060 MB/s on two
  pinned cores at 0.95 s/GB. Even divided by the ~6× penalty the real path
  imposes (below), that is far above a gigabit.

The pinned rows are *more* efficient per byte than the unpinned ones (0.95 vs
1.45 s/GB) — pinning removes cross-core migration and cache-line bouncing. Do
not read the unpinned s/GB as the cost of scaling; read it as the cost of
scaling *carelessly*.

**Reproducibility of this table.** A second run of the same sweep on the same
machine, with the workstation otherwise quieter, gave 2405.8 MB/s at 0.84 s/GB
for the `c=4 par=4` relay case against the 2060.5 MB/s at 0.95 s/GB above — a
17 % swing in absolute throughput and 12 % in cost per byte, both in the same
direction, from nothing but background load on the measuring machine. The
*shape* reproduced exactly: same best case, same carrier progression, QUIC at
2.4× the relay's cost per byte in both runs. Quote the ratios from this table;
do not quote its absolute MB/s.

#### 2.17.2 Staging's real cost per byte, host-total

`scripts/perf/server_cpu_sample.sh` sampled `/proc/stat` on the server at 2 s
while the VM ran six alternating 20 s single-stream bulk transfers. CPU seconds
are computed as `user+nice+system+irq+softirq+steal` over the window, minus a
measured host idle baseline (0.01–0.03 cores). This is the whole system, so the
softirq the host kernel spends servicing the container's sockets *is* counted —
container-only `docker stats` misses it, and it is nearly half the bill.

| case | path | MB/s | cores (of 2) | net s/GB | softirq share | steal |
| --- | --- | --- | --- | --- | --- | --- |
| relay-r1 | relay-tcp | 242.1 | 1.51 | 6.33 | 45.8 % | 0.2 % |
| quic-r1 | direct-quic | 116.0 | 1.50 | 12.46 | 54.9 % | 0.0 % |
| relay-r2 | relay-tcp | 196.8 | 1.36 | 7.01 | 47.3 % | 0.3 % |
| quic-r2 | direct-quic | 113.5 | 1.33 | 11.95 | 51.7 % | 0.1 % |
| relay-r3 | relay-tcp | 213.5 | 1.40 | 7.01 | 48.4 % | 0.2 % |
| quic-r3 | direct-quic | 116.7 | 1.31 | 11.36 | 50.2 % | 0.2 % |

`steal` stays at 0.0–0.3 % throughout, which matters on a burstable instance:
**these are not throttled measurements.** t4g.micro CPU credits were not
exhausted, so the numbers are the instance's real capability, not its baseline
allowance.

This also confirms F-7 independently and quantitatively: **46–55 % of the CPU
bore consumes is softirq**, i.e. kernel network processing, not bore's own code.
Optimizing bore's user-space path can only address the other half.

#### 2.17.3 Is staging stalling on CPU or on the link?

At 1.4–1.5 of 2 cores, staging was *not* CPU-saturated in §2.17.2 — so the
196–242 MB/s single-stream figures were not a ceiling of any kind, and any claim
that they were needed testing. Parallel streams plus carriers, with both the
`/proc/stat` sampler and the ENA instance-allowance counters
(`bw_*_allowance_exceeded`, `pps_allowance_exceeded`, read via
`ethtool -S ens5`) sampled across each window:

| case | transport | c | par | MB/s | Gbit/s | cores (of 2) | net s/GB | tx pps | pps_exc/s | bw_out_exc |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| relay-p4 | relay-tcp | 1 | 4 | 258.4 | 2.07 | 1.60 | 6.55 | — | — | — |
| relay-p8 | relay-tcp | 4 | 8 | 328.5 | 2.63 | 1.89 | 5.80 | — | — | — |
| relay-p8b | relay-tcp | 4 | 8 | **334.7** | **2.68** | **1.90** | **5.74** | 239 196 | 57 | 1 630 |
| quic-p4 | direct-quic | 1 | 4 | 99.1 | 0.79 | 1.35 | 13.67 | 70 761 | 3 544 | 0 |
| quic-c4p4 | direct-quic | 4 | 4 | 116.5 | 0.93 | 1.73 | 15.00 | 85 841 | 16 669 | 0 |
| quic-c4p4b | direct-quic | 4 | 4 | **120.1** | **0.96** | 1.68 | 14.11 | 89 587 | 8 479 | 0 |

(The ENA sampler was started after `relay-p4`/`relay-p8` had begun, hence the
blanks; `relay-p8b` and `quic-c4p4b` are the repeat of those two cases with the
sampler covering the full window, and they reproduce the throughput to within
2 %.)

This resolves it:

- **The relay is CPU-bound at 2.68 Gbit/s on this instance.** 1.90 of 2.00 cores
  with 57 pps-allowance events per second and 1 630 bandwidth-allowance events
  across the window — the instance is beginning to complain, but the CPU got
  there first. Efficiency *improves* under load, 7.0 → 5.74 s/GB, because larger
  batches amortize per-wakeup cost. The 2-core arithmetic closes:
  2.00 / 5.74 s/GB = 0.348 GiB/s = 374 MB/s = 2.99 Gbit/s theoretical, measured
  2.68 Gbit/s at 95 % core occupancy.
- **QUIC direct hits a wall at ~0.96 Gbit/s and neither carriers nor parallelism
  move it.** 99 → 116 → 120 MB/s across three configurations. It costs
  14.1 s/GB, 2.5× the relay, and it draws heavy PPS-allowance throttling
  (3 544–16 669 events/s at 70–90 k tx pps) that the relay never triggers at
  239 k tx pps. Mean wire packet is 1 426–1 464 B in both cases, so this is not
  a packet-size artefact; the most likely mechanism is quinn's GSO send batches
  arriving as microbursts against a token bucket the smoother TCP/TSO path
  clears, but that is inference, not measurement. What *is* measured: on a
  1 Gbit/s link, `--udp` is the limit and the relay is not.

#### 2.17.4 What this means for 1 Gbit/s and 5 Gbit/s

Using staging's under-load relay cost of **5.74 s/GiB** on a burstable
Graviton2 core at 1500 MTU with TLS — a deliberately pessimistic basis, since
IONOS is announced as non-burstable x86:

| target | bytes/s | relay cores needed | QUIC cores needed |
| --- | --- | --- | --- |
| 1 Gbit/s | 119 MiB/s | **0.67** | 1.64 (but the wall is 0.96 Gbit/s) |
| 2 Gbit/s | 238 MiB/s | 1.34 | — |
| 2.68 Gbit/s | 320 MiB/s | 1.90 (measured ceiling) | — |
| 5 Gbit/s | 596 MiB/s | **3.34** | 8.2 |
| 10 Gbit/s | 1192 MiB/s | 6.7 | — |

- **IONOS, 2 cores, ~1 Gbit/s: the application is not the limit.** The relay
  needs about a third of one of the two cores to saturate the link, and the same
  box already delivers 2.68 Gbit/s on hardware that is slower and credit-limited.
  The remaining 1.3 cores are headroom for concurrency, TLS handshakes and the
  admin/weblog paths. Bulk throughput work has no payoff here.
- **AWS at 5 Gbit/s: the application is still not the limit, but the core count
  becomes the design input.** 3.34 cores at staging's efficiency, and bore has
  been measured using 3.61. A 4 vCPU box saturates 5 Gbit/s only with no margin;
  **6–8 vCPU with `--carriers 4` is the configuration that does it comfortably.**
  On x86 at the same clock the per-byte cost should be lower than Graviton2's,
  so treat 3.34 as the ceiling of the estimate.
- **`--udp` must not be the transport chosen for bandwidth.** Its measured
  ceiling of 0.96 Gbit/s is *below* a 1 Gbit/s link and 5× below a 5 Gbit/s one,
  and it is the one configuration in this campaign that cannot be improved with
  a flag. It remains the right transport on a lossy path (2.8× the relay at 1 %
  loss, F-2/F-8) — that is a resilience decision, not a throughput one.
- **The single-stream case is fine too.** One relay stream did 242 MB/s
  (1.94 Gbit/s) over the real internet at 21 ms RTT, so a single client
  saturates a gigabit without needing parallelism.

The apparatus is `scripts/perf/vhost_app_ceiling.sh` (loopback sweep),
`scripts/perf/vhost_remote_efficiency.sh` (staging s/GB) and
`scripts/perf/server_ena_sample.sh` (instance allowance counters).

---

## 3. Findings

Ordered by severity, not by discovery order.

### F-1 — a wedged vhost provider holds its subdomain indefinitely (availability) — REPRODUCED

**Trigger: the provider process is alive at the TCP layer but dead at the
application layer.** `SIGSTOP` on the provider mid-transfer reproduces it every
time, at `carriers=1` and `carriers=2`:

```
before freeze:  {"active":1,"carriers":1,"relay_tx_bytes":18709498}
provider frozen (TCP alive, application dead)
t+15s   {"active":1,"relay_tx_bytes":22425580}  present=yes
t+30s   {"active":1,"relay_tx_bytes":22425580}  present=yes
t+60s   {"active":1,"relay_tx_bytes":22425580}  present=yes
t+120s  {"active":1,"relay_tx_bytes":22425580}  present=yes
re-registration while frozen:
  Error: server error: subdomain 'z3f1789039766c1' in use
entry released after 0s        <- only once the process is really killed
```

`relay_tx_bytes` stops advancing, so the in-flight transfer is wedged too, and
the entry never expires on its own. This matches the two leaks seen in the wild
(`bench17890345222`, `bench17890345351`, ≥ 950 s with the provider gone, one
`vhost subdomain already in use` in the server log).

This is the exact shape `CLAUDE.md` documents under "Secret control liveness":
the peer is invisible to `send`/`recv` because the control loop runs over a
yamux substream, so `send` buffers and `recv` blocks forever, and the RAII admin
`Registration` never drops. Secret tunnels solve it with a `last_recv` timestamp
reaped on the 500 ms heartbeat tick plus a client `ClientMessage::Heartbeat`
every 20 s. **vhost was deliberately left on the legacy heartbeat-free path —
that is the gap.**

Operational impact: a suspended laptop, a `SIGSTOP`ed container or a wedged
provider makes the subdomain unusable until the process is genuinely killed,
while the admin panel still reports the tunnel as live.

What does *not* trigger it — 11 scenarios, all released the entry in 0–1 s:
readers frozen with `SIGSTOP`, readers `SIGKILL`ed mid-transfer, six frozen
readers at once, provider `SIGKILL` at carriers 1/2/4 on both transports, and
provider `SIGTERM`. Every one of those lets the kernel deliver a FIN or RST.
The bug needs the connection to stay alive.

Not yet covered: a genuinely half-open TCP connection (peer gone, no FIN, no
RST). The attempt needed a per-source-port blackhole, and raw `tc` is outside
the sudoers grant, so it did not run. The heartbeat fix would cover that shape
too.

### F-2 — TCP relay collapses under packet loss; QUIC direct does not — reconfirmed from the VM

See §2.4 for the workstation measurement (17–22× on bulk download, total loss of
small-request service under loss + latency) and **§2.14 for the clean
confirmation from the VM**, where each transport was impaired on its own data
plane with the other serving as a control:

| data leg impairment | TCP relay | QUIC direct | ratio |
| --- | --- | --- | --- |
| 1 % loss | 45.26 MB/s | 126.70 MB/s | QUIC **2.8×** |
| +40 ms RTT | 27.10 MB/s | 59.41 MB/s | QUIC **2.2×** |
| +40 ms and 1 % loss | **1.14 MB/s** | (control 81.19 MB/s) | Mathis regime |
| clean | 167.64 MB/s | 115.24 MB/s | relay **1.5×** |

So F-2 holds, and F-8 supplies the other half of it: the relay wins on a clean
path by an equally clear margin (1.589× paired, §2.15). Together they arbitrate
candidate #1 (load-aware path selection) as **supported** — the right transport
genuinely depends on the path, which is exactly what makes a static default
wrong for somebody.

### F-3 — Benign client disconnects are logged at WARN

718 of 786 warnings in a 3 h window are
`peer closed connection without sending TLS close_notify` — ordinary browser and
curl behaviour. A further 32 are `TLS handshake eof`. Real problems are buried:
the single `vhost subdomain already in use` from F-1 sits in the same stream.

The project already holds this principle elsewhere ("benign hole-punch strays are
`debug`, never `WARN`", `CLAUDE.md`). The same treatment applies here.

### F-4 — Every vhost response takes the header-injection copy path — CONFIRMED, but it is free

`/config.yml` sets seven `default_response_headers` and `reservations: []`, so
the merge applies to every unreserved subdomain, which includes every benchmark
label. Verified on the wire against a fresh subdomain:

```
Content-Security-Policy, Permissions-Policy, Referrer-Policy,
Strict-Transport-Security, X-Content-Type-Options, X-Frame-Options,
X-XSS-Protection      <- all seven present
```

So every response goes through `relay_response_injected` and the hand-rolled
`copy_one_direction_with_shutdown`, which flushes after every write, rather than
`copy_bidirectional_with_sizes`. **In this deployment the non-injection fast path
is unreachable**, and every number in this document was measured on the injected
path.

**Resolved by A/B, §2.12.** A private `bore server` on loopback, same binary,
same config except `default_response_headers`, three interleaved rounds per
scheme, measuring the server process's own CPU time per request:
**+0.6 % on HTTPS, +0.5 % on HTTP, rps within 0.7 %.** The cost is below the
noise floor.

Consequence for the enhancement plan: **do not touch this path for performance
reasons.** The flush-after-every-write is a correctness requirement
(`docs/VHOST_INJECTED_FLUSH_FIX.md`), it is not overhead. F-4 is retained only
as an accurate statement of which code path staging exercises — which matters
because it means every other number in this document was measured on the
injected path, i.e. on the slower of the two, and is therefore conservative.

### F-5 — `Expect: 100-continue` costs 21–29 % on upload

With an origin that answers the interim response, PUT 32 MiB runs at 21.52 MB/s
with `Expect` versus 30.48 MB/s without (TCP relay), and 26.55 versus 33.46
(QUIC direct).

**The vhost relays the interim `100 Continue` correctly** — verified through the
tunnel on both transports. So this is not a 1xx-handling bug. The residual cost
is larger than one extra round trip explains (+0.44 s on a 1.05 s transfer, where
2×RTT is 45 ms) and rests on single measurements. Needs repeats before it is
treated as real.

---

### F-6 — the admin API's config view does not reflect the vhost YAML

`/admin/api/v1/config` reports `default_response_headers: null` while
`/config.yml` sets seven of them, and reports `vhost_domain: null` while
`vhost_base_domain` is set. The endpoint mirrors CLI flags and environment, not
the effective merged configuration.

An operator reading the admin API cannot see the response-header policy actually
being applied. This cost me a wrong conclusion mid-campaign: on the CLI view
alone F-4 looked disproven.

### F-7 — the server is kernel-CPU-bound, not application-CPU-bound

At 11 k rps the server host runs at ≈ 1.5 of 2 cores, split
**softirq 37–40 %, sys 22–24 %, usr 13–15 %** (§2.10). 82 % of the cost is the
kernel: packet processing, TLS syscalls, socket wakeups. `steal` is 0.0 % and
does not decay, so the burstable-credit hypothesis is dead for this instance and
irrelevant for the non-burstable IONOS production VPS.

This reorders the whole optimization space. Anything that shortens bore's own
code path competes for a seventh of the bill. The levers that matter are the
ones that reduce **syscalls and packets per unit of work**:

- larger reads/writes per syscall on the relay splice (`proxy_buffer_size`, already 256 KiB)
- fewer wakeups per request (batching, `MSG_MORE`-style coalescing of head + body)
- offload-friendly TCP over userspace crypto — which is exactly what F-8 measures
- **not** micro-optimizing the injection path (F-4 measured it at zero)

### F-8 — the transport ranking is path-dependent, and reverses

| client | RTT | loss | faster transport | margin |
| --- | --- | --- | --- | --- |
| workstation, WiFi (§2.4–2.6) | 21 ms | present under netem | **QUIC direct** | relay collapses under 1 % loss |
| same-region VM (§2.8) | 1.84 ms | none | **TCP relay** | 25–110 % |

Both are correct for their own path, and the mechanism is understood: kernel TCP
with TSO/GSO/GRO offload beats a userspace QUIC stack doing per-packet AEAD on
two Graviton cores — until loss appears, at which point cubic's
`MSS/(RTT·√p)` ceiling collapses and QUIC's loss recovery wins outright.

QUIC also costs roughly **2× the client CPU for less throughput** on the clean
path (46.3 % vs 22.7 % at c=4).

**Operational consequence: `--udp` is not a default, it is a remedy.** Recommend
TCP relay for clean/low-RTT paths, `--udp` for lossy or long-RTT paths. F-2 is
narrowed accordingly, and the earlier reading of it as a general recommendation
is retracted in §4.

### F-9 — SSH-gateway bulk throughput does not scale with parallelism

Eight parallel downloads over the SSH gateway aggregate to **105 MB/s mean**,
which is *no more than a single stream* (130 MB/s mean, same rounds). Native
over the same server and origin gains from parallelism: 167 → 199 MB/s (§2.11).

Small requests are unaffected — at c=64 the SSH gateway did **18 448 rps**
against native's 14 223, at a *lower* p50 — and there is no head-of-line
blocking (§2.11 S4). So this is specifically an aggregate-bandwidth ceiling on
the one SSH TCP connection, consistent with the previously measured cause:
`shared::tune_tcp` sets `TCP_NODELAY` and `SO_KEEPALIVE` but no `SO_SNDBUF`/
`SO_RCVBUF`, so the SSH carrier gets whatever autotuning gives it, and the SSH
leg cannot use `--carriers` to work around it (SSH leg is single-connection by
design).

Note the prior assessment (`docs/SSH_GATEWAY_ASSESSMENT_2026-07-10.md`) rejected
setting `SO_*BUF` on SSH sockets as *harmful* — it clamps to `net.core.*mem_max`
and disables autotuning. That rejection stands. The remediation is sysctl-level
on the server host, and at 1.84 ms RTT the effect is small; it is a real ceiling
mainly at high RTT. **Priority: low for a same-region deployment, and it is a
documentation item, not a code change.**

### F-10 — the fix for F-1 already exists in this binary, on the SSH path

S6 ran the identical wedged-provider procedure on both transports (§2.11):

- SSH gateway: label **released between t+20 s and t+40 s**, re-registration
  succeeds while the frozen session is still TCP-alive.
- native vhost: label **held at t+120 s** with `relay_tx_bytes` frozen since
  t+20 s — the server can already see the counter has stopped — and
  re-registration rejected with `subdomain '…' in use`.

So the codebase contains two working instances of the missing mechanism:

1. **I-SSH10** — a bounded (15 s) channel open, two consecutive timeouts,
   then session abort and RAII teardown.
2. **the secret-tunnel zombie reaper** — `last_recv` tracked in
   `serve_provider`/`serve_consumer`, reaped on the 500 ms heartbeat tick, with
   the client sending `ClientMessage::Heartbeat` every 20 s.

vhost has neither: it keeps the legacy heartbeat-free server loop. The
enhancement is to give the vhost provider loop the secret-tunnel treatment, or
at minimum an open-timeout strike counter as on the SSH path. **This is the
highest-value fix in the campaign**: it is an availability defect (a user's
subdomain becomes unusable until an operator intervenes), it is deterministically
reproducible, and the fix pattern is already written, tested and shipped twice in
the same binary.

### F-11 — operational stability is not in question

6.5 M requests over 10 minutes at ~11 k rps: no rps decay, RSS flat at 15.4 MiB,
0 connection rejections, success rate 1.0 in every 30 s sample (§2.9). No ghost
registrations were produced by any *graceful* or *hard-kill* path — every
scenario released the label in 0–2 s. The only leak shape found in the whole
campaign is F-1's wedged-but-TCP-alive provider, and only on the native path.

### F-12 — an origin failure closes the connection instead of returning 502

With the tunnel up and healthy, killing the origin makes vhost answer nothing at
all: `curl` reports `http=000` — the connection is closed without a status line.
Same for a tunnel pointed at a port nothing listens on (§2.13 G5). An unknown
subdomain correctly returns 404, and once the origin comes back the *same*
tunnel serves 200 again with no reconnect, so the routing and lifecycle are
right; only the error surface is missing.

Every conventional reverse proxy answers this case with **502 Bad Gateway**
(origin refused / died) or **504** (origin timeout). A bare close is worse for
three reasons: a browser shows a generic "connection was reset" instead of a
gateway error, an HTTP client cannot distinguish "tunnel gone" from "origin
down", and a health check or uptime monitor records a transport failure rather
than an application failure.

Low severity, low effort, high diagnostic value. Should be part of the plan.

### F-13 — `--udp` vhost memory is unbounded per tunnel and can take the server down

§2.13 G8/G9, one tunnel, one `--carriers 1` QUIC connection:

| slow readers | server RSS |
| --- | --- |
| 16 | 32.6 MiB |
| 64 | 71.2 MiB |
| 256 | 260.3 MiB |
| 512 | 424 MiB |
| 32 (G9, larger bodies) | **536.8 MiB** |

The mechanism is the documented one and is working as designed: the direct QUIC
path sets `connection_receive_window` 256 MiB against
`stream_receive_window` 16 MiB precisely so that stalled streams cannot starve
each other (`docs/VHOST_UDP_CONCURRENCY_FIX.md`). G9 confirms the fix — **there
is no stall cliff at 4, 8, 16, 24 or 32 slow readers; fast requests stayed at
12–17 ms throughout.** The cost is that the tolerance is paid in buffered bytes,
and 256 MiB is *per QUIC connection*, i.e. per tunnel per carrier.

On the 903 MiB staging host that was enough to cause real damage: two requests
timed out at 10 s, a subsequent registration failed, and an unrelated operator
tunnel lost its control connection and reconnected. No OOM kill and no container
restart, and RSS recovered to 33.6 MiB, so this is memory *pressure*, not a
leak.

Sizing arithmetic for the announced production VPS (2 GiB, 2 cores):

- one `--udp` tunnel with a stalled reader set can occupy up to 256 MiB
- `--carriers 4` multiplies that by 4 — **1 GiB from a single tunnel**
- ten such tunnels have no defined bound at all

Candidate remedies, in increasing order of effort:

1. **Operational, immediately available:** size the windows for the host.
   `--udp-connection-receive-window 64MiB --udp-stream-receive-window 8MiB`
   keeps the 8:1 ratio that prevents the cliff while capping a tunnel at a
   quarter of the memory. This is a documentation and default-tuning item, and it
   needs a measurement pass to confirm the cliff stays away at the smaller ratio.
2. **A server-wide budget:** a global cap on aggregate direct-path receive
   window, allocated across tunnels, so N tunnels cannot each claim 256 MiB.
3. **Backpressure instead of buffering:** stop granting connection-level credit
   once a tunnel's buffered bytes cross a threshold, which converts memory
   pressure back into flow control on the slow reader that caused it.

Note the trade-off G8 exposes: TCP relay is memory-cheap (95 MiB at 512
connections) but a fresh request behind 256–512 concurrent connections took
**0.97–1.44 s**, where QUIC served it in 14 ms. Neither transport is strictly
better under concurrency; the plan should treat this as a two-axis choice and
state the recommended default explicitly.

### F-14 — the UDP-to-relay fallback loses the first request, and its metrics lie

§2.14 G6 redo, on a live `--udp` vhost tunnel with UDP toward the server dropped
100 %:

- aggregate throughput during the outage was **179.09 MB/s**, higher than the
  116.47 MB/s the tunnel was doing on QUIC before it — the warm relay took over
  and the tunnel never went down. **The fallback design works.**
- the **first** request after the blackout returned **`http=000` after 9.90 s**:
  no response, no status line, a ten-second stall. That is one lost request and
  a browser-visible hang per path failure, and it compounds F-12 (there is no
  502 to show either).
- `direct_stream_opens` went **1 → 12 while UDP was 100 % dropped**, so it counts
  *attempts*, not successful direct streams.
- `direct_fallbacks` stayed at **0** through a fallback that demonstrably
  occurred.

Three separable items for the plan:

1. **Make the first failure cheap.** ~10 s is the QUIC idle timeout being waited
   out by a stream that was already assigned to a dead connection. A connection
   whose direct open is in flight should fall back on a much shorter deadline —
   the SSH path already does exactly this with a 15 s bounded open plus strike
   counter (I-SSH10), and the direct path's own keepalive is 3 s / idle 10 s, so
   a 1–2 s open deadline is consistent with the design.
2. **Fix the counters.** `direct_stream_opens` should count opens that
   *succeeded*, or be renamed to `direct_stream_open_attempts` and joined by a
   success counter. `direct_fallbacks` must actually increment. As it stands an
   operator cannot tell from the admin API whether a `--udp` tunnel is using the
   direct path at all — which is also why every transport attribution in this
   document is backed by impairment controls (§2.14) and not by the counter
   alone.
3. **Surface the current path per tunnel** in the admin API and the dashboard:
   one field saying `direct` or `relay` right now, with the timestamp of the last
   transition. This is the single most useful piece of vhost observability that
   is missing, and it is cheap.

### F-15 — a bulk transfer in flight destroys concurrent request latency

§2.15 A3, one bulk download running while `oha -c 8` hits `/1k` on the same
tunnel:

| | idle | under bulk | degradation |
| --- | --- | --- | --- |
| TCP relay p50 | 2.57 ms | **28.25 ms** | **11×** |
| TCP relay p95 / p99 | 2.92 / 3.11 ms | 58.60 / 67.13 ms | 20× |
| TCP relay rps | 3 062 | **288** | −91 % |
| QUIC direct p50 | 2.52 ms | **9.14 ms** | 3.6× |
| QUIC direct p95 / p99 | 2.97 / 3.21 ms | 30.25 / **219.08** ms | 68× at p99 |
| QUIC direct rps | 3 089 | **560** | −82 % |

Neither transport isolates a latency-sensitive request from a bandwidth-hungry
one on the same tunnel. The relay is uniformly bad (11× p50, tight tail); QUIC
keeps a better median but develops a **219 ms p99** tail.

This is the single most user-visible latency result in the campaign, because it
is not a synthetic condition — it is what an ordinary page load looks like: one
large asset or download alongside many small requests. And it is invisible in
every idle-latency measurement, which is why §2.9's flawless 11 k rps at 2.8 ms
does not contradict it.

Both mechanisms are plausible and they are different:

- **relay:** all substreams share one carrier TCP connection, so a full-rate
  bulk stream keeps that connection's send buffer and cwnd saturated and small
  requests queue behind it. Note this is *not* the head-of-line case tested in
  §2.11 S4, which used six *rate-limited* readers and showed no effect — the
  difference is a stream that is actually filling the pipe.
- **QUIC:** streams are independent, hence the better median, but the shared
  connection-level congestion controller and send window still couple them, and
  a stream scheduled behind a large burst waits — hence the 219 ms tail.

This promotes the *stream priorities* half of candidate 5, which the earlier
clean-path data did not support:

1. ~~Measure first: does `--carriers N` fix the relay case?~~ **Done, §2.16 —
   yes, partially.** 8 carriers take p95 under one bulk transfer from 27.3 ms to
   14.1 ms and p50 under two from 45.1 ms to **14.8 ms** (3×), with request rate
   doubling. So the relay mechanism really is carrier saturation. QUIC needs no
   carriers for this — `c=1` already beats the relay at any count — and `c=4`
   makes QUIC slightly worse. **No configuration recovers the 2.5 ms idle p50**;
   the best case under load is 7–15 ms.
2. **Relay:** consider steering by request shape — a small request should not be
   pinned to a carrier that is currently saturated. Carrier selection is already
   per-proxied-connection, so the hook exists.
3. **QUIC:** set stream priorities so a newly opened stream is not scheduled
   behind a large in-flight burst, and cap the per-stream send burst.
4. Whatever is done must be checked against F-13: anything that buffers more to
   smooth latency makes the memory ceiling worse.

### F-16 — the application does not limit the link; `--udp` does — MEASURED

**Severity: none for the relay, high for `--udp` as a bandwidth choice.**
**Evidence: §2.17.**

This is the answer to §5 open question 5, obtained without needing the operator
to look up the IONOS provisioning: measure bore's CPU cost per byte and the
core-count scaling, then divide any target link rate by it.

| claim | measurement |
| --- | --- |
| the relay saturates a 1 Gbit/s link with ~⅓ of one core | 5.74 s/GiB under load → 0.67 cores for 119 MiB/s |
| the relay already exceeds 1 Gbit/s by 2.7× on the staging instance | 334.7 MB/s = **2.68 Gbit/s** at 1.90 of 2 burstable Graviton2 cores |
| a single stream alone exceeds 1 Gbit/s over the real internet | 242 MB/s = 1.94 Gbit/s, one curl, 21 ms RTT |
| bore uses more than one core when given more | `cores_used` 0.98 → 3.61 as `--carriers`/parallelism rise |
| 5 Gbit/s is reachable, and needs cores rather than code | 3.34 cores at staging efficiency; measured usage reaches 3.61 |
| **`--udp` cannot reach 1 Gbit/s at all** | **0.79 / 0.93 / 0.96 Gbit/s across three configurations** |

Three separate facts make `--udp` the exception, and they compound:

1. **It costs 2.5× the CPU per byte.** 14.11 s/GiB against the relay's 5.74. A
   2-core box spends 1.64 cores on a single gigabit of QUIC against 0.67 for the
   relay.
2. **Its ceiling does not respond to any flag.** `--carriers 1 → 4` and 1 → 4
   parallel streams move it from 99 to 120 MB/s and no further. Every other
   limit found in this campaign has a configuration lever; this one does not.
3. **It draws instance-level PPS throttling the relay does not.** 3 544–16 669
   `pps_allowance_exceeded` events per second at 70–90 k tx pps, while the relay
   pushes 239 k tx pps with 57. Mean wire packet is ~1 440 B on both paths, so
   it is not a packet-size effect. The plausible mechanism is quinn's GSO send
   batching arriving as microbursts against a token bucket that the paced
   TCP/TSO path clears; that inference is **not** measured and should not be
   quoted as fact. On a cloud instance with a published PPS allowance it is a
   real operational risk regardless of mechanism.

Note this does **not** demote `--udp`. F-2 and F-8 stand: on a lossy path QUIC
direct delivers 2.8× the relay at 1 % loss and 2.2× at +40 ms, and F-14 shows
its fallback to the relay is seamless in aggregate. The correct framing is that
**`--udp` is a resilience transport, not a bandwidth transport**, and any
documentation or default that implies otherwise is misleading. On a clean fast
link the relay is both faster (F-8, median 1.589×) and 2.5× cheaper per byte.

Two secondary results worth keeping:

- **`--carriers` is also a core-scaling lever, not only a loss and latency
  lever.** At `c=1` the server does not pass ~1.0 core on a single stream; at
  `c=4` it reaches 3.6. This is a third independent argument for the adaptive
  carrier work in §6, and the cheapest one to act on.
- **46–55 % of the CPU bore consumes is softirq** (§2.17.2), measured
  host-total rather than container-only. This confirms F-7 quantitatively and
  bounds what user-space optimization can ever achieve: at most the other half.

**Implication for the plan.** No bulk-throughput work is justified for the
announced IONOS target — the link saturates at a third of one core. The
remaining 1.3 cores are headroom, and the campaign's real defects are all in
availability (F-1), memory (F-13), concurrent latency (F-15) and observability
(F-14, F-12). For a future 5 Gbit/s box the deliverable is a sizing note
(6–8 vCPU, `--carriers 4`), not an optimization.

## 4. Retracted findings

Kept deliberately: each one was a plausible bore defect that turned out to be
measurement apparatus.

| retracted claim | actual cause |
| --- | --- |
| "upload through vhost is 6–7× slower than download" (4–5 MB/s) | the origin's `asyncio` `StreamReader` used the default 64 KiB `limit`, so the transport paused and resumed on every refill. Raising it to 4 MiB took loopback PUT from 33 MB/s to 4.6 GB/s. Through the tunnel upload is now **94 % of the local ceiling** |
| "4 concurrent uploads on one carrier wedge the tunnel for >200 s" | same origin defect under concurrency. With the fix, 2/4/8 parallel PUTs all finish in <1.2 s |
| "QUIC direct beats TCP relay on sustained bulk (37.2 vs 28.8 MB/s)" | 30 s window. At 90 s all four configurations converge on 28.7–30.2 MB/s |
| "the vhost swallows `100 Continue`" | the origin did not implement it. Once it did, the interim response was observed arriving through the tunnel on both transports |
| "41 ms p50 latency through the tunnel" | dufs as origin: Nagle plus delayed-ACK on a split head/body write. `bench_origin.py` measures 0.038 ms p50 on loopback |

---

### 2.18 — The stall rung is the window RATIO, not the window size (G9 ladder, private server)

Run with `scripts/vhost_udp_window_ladder.sh` (root, netns `ns0`/`nsp`/`nsc`,
provider run as root so the non-root socket-buffer clamp is out of the picture)
against a **private** server — staging is frozen, and this is precisely the test
shape that knocked an unrelated staging tunnel offline once already.

Method per rung: N slow readers (`curl --limit-rate 8k`) pinned on a 48 MiB file
— larger than one stream window at every profile, so a stalled stream can hold a
full per-stream buffer — then one fast 64 KiB request timed under that load.
`STALL` means it was served but took ≥ 3 s (the same criterion as the R3 gate);
`HUNG` means it did not complete inside 30 s.

Fast-request latency under load:

| profile | conn/stream | 4 | 8 | 16 | 24 | 32 |
| --- | --- | --- | --- | --- | --- | --- |
| default | 256/16 MiB (16:1) | 0.01 s | 0.01 s | **HUNG** | HUNG | HUNG |
| budget512 | 128/8 MiB (16:1) | 0.01 s | 0.01 s | **23.2 s** | 24.2 s | HUNG |
| budget128 | 32/2 MiB (16:1) | 0.01 s | 0.01 s | **24.2 s** | 22.2 s | 25.2 s |
| ratio8 | 64/8 MiB (8:1) | 0.01 s | **24.2 s** | HUNG | 18.2 s | 23.2 s |

Server RSS at the same rungs (MiB):

| profile | 4 | 8 | 16 | 24 | 32 |
| --- | --- | --- | --- | --- | --- |
| default | 83.5 | 183.4 | 320.6 | 361.9 | 481.4 |
| budget512 | 51.1 | 100.1 | 170.9 | 240.2 | 268.0 |
| budget128 | 26.8 | 46.7 | 85.0 | 86.1 | 106.4 |
| ratio8 | 49.6 | 83.6 | 109.5 | 161.2 | 151.1 |

Two independent points fix the relationship. Every 16:1 profile — across an
8× range of absolute window size — first stalls at **16** readers. The 8:1
profile first stalls at **8**. The first stalling rung is
`connection_window / stream_window`, which is exactly the arithmetic
`CLAUDE.md` states, now measured rather than reasoned.

### F-17 — the small-host profile costs nothing in stall tolerance, and the default's cliff is reachable — MEASURED

Two conclusions, one welcome and one not.

**Welcome.** Shrinking both windows together is free in the dimension that
matters. `budget128` (32/2 MiB) tolerated the same 8 slow readers as the shipped
256/16 MiB default and used **85.0 MiB instead of 320.6 MiB at the 16-reader
rung — 3.8× less** — and it degraded more gracefully above it (`STALL` at every
rung where the default `HUNG`). So `--udp-memory-budget` is not a
performance-for-memory trade at all, provided the 16:1 ratio is preserved, which
is why the derivation enforces it rather than exposing it.

**Not welcome, and it corrects §2.13.** The default profile HUNG at 16 slow
readers here, while G9 on staging reported 12–17 ms fast requests at every count
up to 32. Both measurements are real; they differ in how hard the readers pin
windows. This harness uses 8 kB/s readers on a 48 MiB file over a fast netns
path, which holds a full 16 MiB per-stream buffer for minutes; the staging run
did not sustain that. The correct statement is therefore **not** "the cliff is
gone" but "the cliff moved from ~4 stalled streams to ~16" — enough for the
originally reported field workload, and still reachable by sixteen paused
readers on one `--carriers 1` tunnel.

What this does *not* change: the 64 → 256 MiB commit was still the right fix (it
bought 4× the tolerance for the reported bug), the ratio still must not be
reduced, and `--carriers N` still multiplies the tolerance by N because each
carrier brings its own connection window. What it adds is that an operator
expecting a `--carriers 1` `--udp` tunnel to survive arbitrary numbers of paused
readers is expecting something no window size delivers — only carriers, or the
TCP relay, which has no shared connection window and was immune at every rung
of the original R1 control.

---

## 5. Open questions

Resolved during the campaign:

0. ~~What triggers the vhost registration leak?~~ **Resolved** — a provider that
   is frozen rather than killed, on *both* transports. F-1, G1.
1. ~~Upload under loss-only is inconsistent.~~ **Resolved by §2.5** — the
   combined matrix impaired both legs at once and the 18.40 MB/s cell was noise.
2. ~~Is the relay collapse under loss TCP or yamux?~~ **Resolved by §2.6** — the
   carrier's TCP congestion window, not yamux flow control.
3. ~~Does anything change above ~30 MB/s?~~ **Resolved by §2.8–§2.13** — yes, and
   it changes the conclusions: the transport ranking reverses (F-8), the server
   turns out to be kernel-CPU-bound (F-7), and two concurrency limits appear that
   are invisible at 30 MB/s (F-13, and the TCP relay's 1.4 s tail at 512
   connections).
4. ~~Does the header-injection path cost anything?~~ **Resolved by §2.12** — no,
   +0.6 %. F-4.
6. ~~Does the cliff stay away at smaller QUIC windows?~~ **Answered by §2.18 and
   F-17, and the answer is better than the question assumed** — and it also
   corrects this document. The first stalling slow-reader count is exactly
   `connection_window / stream_window`, so every profile that keeps the 16:1
   ratio stalls at the *same* rung as the shipped default while using up to
   3.8× less memory. The correction: the 64 → 256 MiB change did not remove the
   cliff, it **moved** it from ~4 to ~16 stalled streams, and §2.13's G9 run
   (no stall to 32 readers) did not reproduce a reachable one.
5. ~~What network bandwidth does the IONOS production VPS guarantee, and does
   bulk-throughput work matter?~~ **Answered by §2.17 and F-16, and it turned out
   not to need the provisioning figure.** Measuring bore's CPU cost per byte
   (5.74 s/GiB under load) and dividing the target rate by it answers the
   question directly: the relay saturates 1 Gbit/s with 0.67 of one core and
   already delivers 2.68 Gbit/s on the staging instance, so **bulk-throughput
   work has no payoff at the announced target**. For a future 5 Gbit/s box the
   answer is a sizing note (3.34 cores needed, bore measured using 3.61, so
   6–8 vCPU with `--carriers 4`), not an optimization. The one genuine limit is
   `--udp`, whose 0.96 Gbit/s ceiling is *below* a gigabit and moves for no flag.

Still open:

7. **Why does the TCP relay develop a 1 s tail at 256+ concurrent connections
   while QUIC does not?** (§2.13 G8: 966 ms and 1 436 ms fresh-request time
   versus 14 ms.) Candidate causes: yamux substream open serialization on one
   carrier, the `--max-conns` semaphore, or head-of-line on the single carrier
   TCP connection. Note S4 showed *no* HOL with 6 slow readers, so this is a
   scale effect, not the same phenomenon. Worth isolating because it is the
   TCP relay's only measured weakness, and the relay is otherwise the faster
   transport (F-8).
8. **Is the 29 % control drift the server, the network or the instance?**
   `steal` is 0.0–0.3 % across every window measured in §2.17 too, so it is
   confirmed *not* burstable CPU throttling. §2.17.3 adds a candidate the
   earlier sections could not see: the ENA counters show the instance logging
   1 630 `bw_out_allowance_exceeded` events during a 20 s relay run at
   2.68 Gbit/s, so at the top of the range the hypervisor's bandwidth token
   bucket is active and its refill is not something the guest can observe or
   pace against. That plausibly accounts for drift in the *high*-throughput
   cases; it does not explain drift at 30 MB/s. Still worth knowing whether a
   non-burstable production host is quieter.

---

## 6. Assessment of the candidate optimizations

Against `docs/vhost/VHOST_PERFORMANCE_ASSESSMENT_2026-09-07.md`.

| # | candidate | verdict | evidence |
| --- | --- | --- | --- |
| 6 | HTTP/2/3 on the browser side | **supported, but the size of the prize is the client's RTT, not the server's work** | +45 ms per new connection at 21 ms RTT, but only **+4.9 ms at 1.84 ms RTT** (§2.15 A3: keep-alive p50 2.6 ms vs new-connection 7.4 ms, identical on both transports), so most of the 45 ms was TLS round trips rather than server cost. Still the strongest latency lever for real remote browsers, where RTT is 20–100 ms; the frontend is HTTP/1.1 only. A browser opens ~6 connections per origin, so a 30-asset page pays roughly 5 waves × 45 ms of pure setup plus per-connection HOL. h2 collapses that to one connection with N streams. Immune to the client-link confound |
| 1 | load-aware carrier/path selection | **supported, promoted to first place among the throughput items** | F-2 and §2.5/§2.6. Impaired on its own transport the relay loses 96 % and QUIC direct 21 %. Matching QUIC's single-connection 23.67 MB/s would take ~24 TCP carriers, and no carrier count at all rescues a single pinned connection. Path selection is the only lever that covers the single-transfer case |
| 2 | bounded open plus safe retry | **supported, for correctness** | F-1. The value is availability, not throughput |
| 4 | adaptive carrier count | **supported, and now with a paired measurement on both sides** | on a clean path carriers buy nothing and slightly hurt — the paired A/B gives a median c=4/c=1 ratio of **0.941**, 4 of 5 pairs below 1.0 (§2.15 A2) — so the default of 1 is right. Under 1 % loss on the relay they scale aggregate throughput almost linearly, ~1 MB/s per carrier (1.00 / 5.00 / 8.00 at c=1/4/8, §2.6). A count that is right in every regime has to be adaptive, and §2.16 adds a second trigger that is easier to detect than loss: **concurrent bulk load on the same tunnel**, where 8 carriers cut small-request p50 by 3×. Caveat: carriers never rescue a *single* connection, which is pinned to one carrier |
| 5 | fewer-copy QUIC path, stream priorities | **split verdict: the copy work is not supported, the stream priorities now are** | the fewer-copy half stays unsupported — 82 % of the server's CPU is kernel, so application copies are a seventh of the bill at most (F-7). The **stream-priority half is now supported by F-15**: a bulk transfer in flight takes concurrent request p50 from 2.5 ms to 28 ms on the relay and produces a 219 ms p99 on QUIC |
| 3 | backend TLS connector reuse, then HTTP pooling | **not tested** | same mechanism as the +45 ms front-side cost, but on the origin leg. Worth ~0 for a localhost origin; matters for a remote backend |

### Additional candidates, not in the original document

| id | candidate | rationale |
| --- | --- | --- |
| N-1 | heartbeat and reaper for vhost registrations | F-1, now reproduced deterministically. Parity with the existing secret-tunnel reaper: `last_recv` checked on the heartbeat tick, plus a client `Heartbeat` every 20 s. Highest-priority item in this document |
| N-2 | demote benign client disconnects to `debug` | F-3. 91 % of warnings are normal client behaviour |
| ~~N-3~~ | ~~revisit yamux window sizing on the relay path~~ | **withdrawn.** §2.6 shows the collapse is the carrier's congestion window, not yamux flow control. Resizing the window would not have moved it |
| N-5 | do **not** stripe one proxied connection across carriers | the tempting fix for §2.6's single-transfer cap is to spread one connection over N carriers. `CLAUDE.md` already records why that fails: per-datagram round-robin on the public `--udp` path is called out as a reorder trap, and flow pinning on the VPN direct path exists because striping one flow across carriers *halved* throughput and pushed UDP loss to 25–44 %. A tunnelled TCP flow reads reordering as loss. Recorded here so the wrong fix is closed off explicitly |
| N-4 | make header injection conditional, or free | F-4. The fast path is currently unreachable in this deployment |
| N-6 | surface the effective merged vhost configuration in the admin API | F-6. The config endpoint shows flags, not the YAML that is actually in force |
| N-7 | bound the direct-path receive window per server, not per tunnel | F-13. One `--udp` tunnel can occupy 256 MiB, `--carriers 4` a full GiB, and ten tunnels have no bound. On a 2 GiB production VPS this is the only finding that can take the whole server down. Cheapest first step is operational — ship smaller defaults for small hosts and document the sizing rule — but a server-wide budget is the real fix |
| N-8 | answer origin failures with 502/504 instead of closing the connection | F-12. Currently a dead origin produces `http=000`, indistinguishable from the tunnel being gone. Low effort, high diagnostic value |
| N-9 | investigate the TCP relay's concurrency tail | open question 7. 966–1 436 ms for a fresh request behind 256–512 concurrent connections, against QUIC's 14 ms. This is the relay's only measured weakness and the relay is otherwise the faster transport |
| N-11 | bound the direct-open deadline so a dead UDP path costs one fast retry, not a 10 s stall | F-14. The pattern exists on the SSH path (I-SSH10) and the QUIC keepalive is already 3 s |
| N-13 | isolate latency-sensitive requests from bulk transfers | F-15/§2.16. 11× p50 degradation on the relay, 219 ms p99 on QUIC, request rate down 82–91 %. **Carriers mitigate it on the relay (3× at c=8) and are useless for it on QUIC**, so the two transports need different treatments: a bulk-aware carrier pool on the relay, stream priorities on QUIC |
| N-12 | fix and extend the direct-path observability counters | F-14. `direct_stream_opens` counts attempts, `direct_fallbacks` never increments, and no field says which path a tunnel is on right now |
| N-10 | document the transport recommendation explicitly | F-8. `--udp` is a remedy for lossy or long-RTT paths, not a default: on a clean low-RTT path it is 25–110 % *slower*, costs ~2× client CPU, and costs 4.5× server memory under concurrency. Nothing in the docs says this today |

### The plan built from this ranking

`docs/plans/plan_VhostEnhancements/` turns the ranking below into seven phases
with per-subphase files, gates and locked decisions. The mapping is not
one-to-one: the plan reorders on one technical ground (F-13 says buffering
interacts, so the direct-window budget precedes the bulk/latency work that would
otherwise be designed against a moving target) and it demotes HTTP/2 to a
measure-then-decide spike. `overview.md` records the eight locked decisions and
reproduces the *not worth doing* list below so it stays closed.

### Priority, given the evidence

Ordered by (severity × confidence) ÷ effort, not by how interesting they are.

**F-16 settles the one input this ranking was waiting on** (§5 question 5): the
relay saturates the announced 1 Gbit/s link with 0.67 of one core and already
delivers 2.68 Gbit/s on the smaller staging instance. So **nothing on this list
is justified by peak bandwidth**, the 1.3 spare cores are available to spend on
concurrency and latency, and the two items that were conditional on that answer
(candidates 1 and 4) are re-ranked below on their latency merits alone.

1. **N-1 — vhost heartbeat and reaper (F-1/F-10).** Availability defect,
   deterministically reproducible on both transports, and the fix pattern is
   already implemented, tested and shipped **twice** in the same binary (the
   secret-tunnel reaper and I-SSH10's bounded-open-plus-strike eviction). The
   SSH path recovers in 20–40 s from exactly the scenario the native path never
   recovers from.
2. **N-7 — bound the direct-path receive window (F-13).** The only finding that
   degraded the whole server, and it did so with one tunnel and 32 slow readers
   on a 1 GiB host. Start with defaults and documentation; validate with a rerun
   of G9.
3. **N-10 — write down the transport recommendation (F-8).** Zero code, and it
   prevents operators from choosing the slower, hungrier transport by default.
4. **N-13 — isolate small requests from bulk transfers (F-15, §2.16).** The
   worst user-visible latency result in the campaign, on the most ordinary
   workload there is, and invisible in idle-latency benchmarks. The mechanism is
   already identified per transport: **carrier saturation on the relay**, where
   8 carriers buy a 3× p50 improvement, and **stream scheduling on QUIC**, where
   carriers do nothing. Merges with candidate 4: the adaptive carrier trigger
   should be *concurrent bulk load*, not only packet loss.
5. **6 — HTTP/2 on the browser side.** Still the strongest *latency* lever for
   real remote browsers, though §2.15 shows only ~4.9 ms of the 45 ms is server
   work — the rest is the client's TLS round trips. Untouched by every confound
   in this document.
6. **N-11 + N-8 together — make failures fast and legible (F-14, F-12).** A
   dead UDP path currently costs a 10 s stall on one request and a bare
   connection close with no status; a dead origin costs the same bare close.
   Both are small, self-contained changes with immediate operator value, and the
   bounded-open pattern is already written on the SSH path.
7. **N-12 — fix the direct-path counters (F-14).** Small, and until it is done
   nobody can tell from the admin API whether `--udp` is actually being used.
8. **N-2 — demote benign disconnect warnings (F-3).** Log hygiene; 91 % of
   warnings are normal client behaviour.
9. **N-6 — expose the merged vhost config in the admin API (F-6).** The SSH
   banner already reports the seven response headers the admin API reports as
   `null`; the data exists, it is just not surfaced.
10. **4 — adaptive carrier count.** Promoted out of the conditional bucket by
   F-16, on latency and core-scaling grounds rather than bandwidth: carriers are
   now known to be the lever for *three* distinct problems — loss (F-2),
   under-bulk small-request latency (§2.16, 3× p50 at c=8) and core scaling
   (§2.17.1, 1.0 → 3.6 cores used). It merges with N-13 above, and the trigger
   should be concurrent bulk load, which §2.16 showed is easier to detect than
   loss. The cost on a clean idle path is real (median c4/c1 ratio 0.941), so it
   must be adaptive rather than a raised default.
11. **N-9 — the relay concurrency tail.** Investigate before designing anything;
   it may turn out to be `--max-conns` tuning rather than code.
12. **N-14 (new, from F-16) — document `--udp` as a resilience transport rather
   than a bandwidth one, and write down the 5 Gbit/s sizing.** Zero code. Two
   sentences in the README and `docs/vhost/`: `--udp` tops out at 0.96 Gbit/s
   and costs 2.5× the CPU per byte, so it is the right choice on a lossy path
   and the wrong one on a fast clean link; a 5 Gbit/s deployment needs 6–8 vCPU
   with `--carriers 4`. Folds naturally into N-10.

**Explicitly not worth doing:** N-4 (F-4 measured the injection path at +0.6 %),
N-3 (withdrawn, §2.6), N-5 (striping one connection across carriers — closed off
by prior measurement), `SO_*BUF` on the SSH sockets (F-9; the previous assessment
rejected it as harmful and that rejection stands), **candidate 1 — load-aware
path selection** (F-16 removes its premise: on a clean link the relay is both
faster and cheaper per byte, so there is no bandwidth reason to switch paths
under load, and F-14's automatic fallback already covers the failure case), and
**any bulk-throughput optimization of the relay data path** (F-16: 0.67 of one
core saturates the announced link, and 46–55 % of what bore does spend is kernel
softirq that user-space changes cannot touch).

One caveat on chasing `--udp` throughput specifically: its 0.96 Gbit/s ceiling is
worth *understanding* — the PPS-allowance mechanism in §2.17.3 is inferred, not
measured — but it is not worth *fixing*, because the relay is the correct
transport on every path where a gigabit matters.

---

## 7. Traffic budget

Initial cap agreed with the operator was **50 GB**; the operator later lifted it
explicitly ("non ti preoccupare per la banda dei 50GB, puoi sforare"), which is
what made the VM phase possible at all — the VM saturates the server, so a
single 12 s bulk case moves ~2 GB. Server-side `bandwidth_tx_bytes` is the meter.

| checkpoint | cumulative egress |
| --- | --- |
| before the sustained ladder | 8.68 GB |
| after the sustained ladder | 21.9 GB |
| before netem plus zombie | 22.69 GB |
| after netem, zombie R1–R15, protocol-selective matrix | 26.79 GB |
| after the VM transport A/B, stability suite and SSH gateway suite | 70.15 GB |
| after the paired A/B, F-15 sweep and latency suite | 164.17 GB |
| **end of campaign** (incl. the §2.17 efficiency and saturation runs) | **223.86 GB tx / 7.90 GB rx** |

For planning a repeat: the workstation phase costs ~27 GB, the VM phase ~140 GB,
and the §2.17 application-ceiling work ~60 GB — of which the loopback sweep
(`vhost_app_ceiling.sh`, ~200 GB moved) costs **nothing**, because it never
leaves the measuring machine. Only `vhost_remote_efficiency.sh` touches the
deployment, at ~20 GB for both modes. Cutting `DUR` in `vhost_transport_ab.sh`
from 12 s to 6 s roughly halves the largest single item at the cost of more
drift per pair.

**End state verified clean.** The server reported only the operator's own three
tunnels (`dufspcloud`, `tennis`, `tennis1`), no leftover registrations from any
suite, `RestartCount 0`, RSS back to **19.4 MiB** at 0.13 % CPU. The measurement
VM had no leftover `bore` or `sshpass` processes and its root qdisc was back to
`mq` with no netem attached. On the workstation the only remaining `bore` process
was the operator's own `tennis` tunnel.

---

## 8. Apparatus

The campaign harness is in `scripts/perf/`, which has its own `README.md`.
Section 9 is the runbook.

| file | role |
| --- | --- |
| `scripts/bench_origin.py` | artifact-free origin: single write per response, `TCP_NODELAY`, 4 MiB reader buffer, raised write high-water mark, `/stream/<n>` for unbounded bulk, answers `100 Continue`. **Calibrate it on loopback before trusting any tunnel number** (§9.3) |
| `scripts/perf/vhost_remote_bench.sh` | VM-side calibration, transport A/B and latency suite (`v0` / `v1` / `v2`) |
| `scripts/perf/vhost_transport_ab.sh` | the **paired** transport and carrier A/B plus the native latency suite — the design that survives the 29 % drift (§2.15) |
| `scripts/perf/vhost_bulk_latency.sh` | request latency with 0/1/2 bulk transfers in flight, per carrier count, per transport (§2.16) |
| `scripts/perf/vhost_remote_stability.sh` | G1–G9: leaks, races, reconnect storms, origin faults, concurrency and memory (§2.13) |
| `scripts/perf/vhost_ssh_gateway_bench.sh` | S1–S8: the SSH ingress gateway against native, same origin and server (§2.11) |
| `scripts/perf/vhost_ssh_takeover_probe.sh` | I-SSH5 same-identity takeover with a discriminating origin (§2.11) |
| `scripts/perf/vhost_netem_matrix.sh` | protocol-selective loss and RTT per transport, plus the UDP-blackhole fallback case (§2.14) |
| `scripts/perf/vhost_registration_leak_repro.sh` | the F-1 reproducer: a provider alive at TCP level, dead at application level |
| `scripts/perf/vhost_header_injection_ab.sh` | F-4 on a private loopback server — needs no deployment access (§2.12) |
| `scripts/perf/server_cpu_sample.sh` / `server_cpu_report.sh` | server `/proc/stat` split and `steal`, sampled over ssh and rendered (§2.10) |
| `scripts/vhost_staging_bench.sh` | the original workstation harness; readiness from the server's own registry, path confirmed via `direct_stream_opens`, per-second server-side sampling |
| `scripts/vhost_staging_netem.sh` | egress impairment toward one destination IP, `apply` / `clear` / `show`. The only sudo-permitted impairment path on the workstation |
| `scripts/vhost_staging_report.sh` | renders the harness JSONL as markdown |

Raw data: `target/vhost-staging-bench/results-*.jsonl`, `*.samples.jsonl`,
`*.client.log`; VM-side logs under `~/out/` on the measurement VM.

---

## 9. Runbook — how to re-run this campaign from scratch

Written so that a repeat in a month starts from this section, not from zero.
Everything below is committed in the repository except the credentials.
The harness lives in `scripts/perf/`, which has its own `README.md`.

### 9.1 What you must have

| thing | why | notes |
| --- | --- | --- |
| bore secret for the staging server | register vhost tunnels | provided separately, never in the repo |
| admin bearer token | read `/admin/api/v1/*`, which is how every server-side number is taken | read-only endpoints only |
| SSH-gateway username + password | the `ssh -R vhost/...` suite | provided separately |
| read-only SSH to the server host | `/proc/stat` sampling, `docker logs`, reading the frozen `config.yml` | never write anything |
| a same-region VM | **mandatory** — see §1.3, a distant or WiFi client measures itself, not the server | destroy it afterwards |

Server configuration is a **constant of the experiment** and must not be
touched. Anything that needs a config change is A/B tested against a *private*
`bore server` instead (§2.12 is the worked example).

### 9.2 Provisioning the measurement VM

Requirements, in order of importance:

1. **Same region / same AZ as the bore server.** The campaign's whole point is
   that RTT must be ~2 ms, not ~20 ms. Verify: `ping -c5 <server>` should show
   1–3 ms.
2. **Link capacity well above the server's ceiling.** The server tops out around
   175 MB/s; the VM needs several times that so it never becomes the limit. A
   `c7i-flex.large` (2 vCPU, 4 GiB) was ample and cheap.
3. **x86_64 if you copy a locally built binary**, or build on the VM. The
   release build used here needs GLIBC ≥ 2.34, so Ubuntu 24.04 works.

Then:

```bash
# on the VM
sudo apt-get update && sudo apt-get install -y jq sshpass python3 iperf3
# oha (load generator), 1.16.0 was used
curl -sSLO https://github.com/hatoo/oha/releases/download/v1.16.0/oha-linux-amd64 \
  && mv oha-linux-amd64 oha && chmod +x oha

# from the workstation
scp target/release/bore            ubuntu@<vm>:bore
scp scripts/bench_origin.py        ubuntu@<vm>:bench_origin.py
scp scripts/perf/vhost_remote_*.sh ubuntu@<vm>:
scp scripts/perf/vhost_ssh_*.sh    ubuntu@<vm>:
scp scripts/perf/vhost_netem_matrix.sh ubuntu@<vm>:
```

Credentials go in `~/env.sh` on the VM, **`chmod 600`**, and nowhere else:

```bash
export BORE_HOST=<base domain>          # e.g. brp.example.xyz
export BORE_TO=<server URL>             # e.g. https://brp.example.xyz
export BORE_SECRET=<bore secret>
export ADMIN_URL=<https://.../admin/api/v1>
export ADMIN_TOKEN=<admin bearer token>
SSHGW_USER=<ssh gateway user>
SSHGW_PASS=<ssh gateway password>
export SSHGW_USER SSHGW_PASS
```

`sudo -n tc` must work on the VM (it does on stock Ubuntu AMIs) for the netem
suite. On the *workstation* it does not: sudoers there only grants NOPASSWD for
`<repo>/scripts/*`, so impairment must go through
`scripts/vhost_staging_netem.sh`, never raw `sudo tc`.

### 9.3 The origin, and why it needed patching

`scripts/bench_origin.py` serves `/ping`, `/1k`, `/100k`, `/stream/<bytes>` and
accepts `PUT`/`POST` to any path as a sink. Two asyncio defaults made it, not
bore, the bottleneck, and each one cost a retracted "finding" (§4):

```python
# limit= sizes the StreamReader buffer; at the 64 KiB default the transport
# pauses and resumes reading on every buffer refill
server = await asyncio.start_server(handle, "127.0.0.1", port,
                                    backlog=1024, limit=4 << 20)
```

```python
# default high-water is 64 KiB, so drain() after every 1 MiB write
# would block until the socket buffer is nearly empty
tr = writer.transport
if tr is not None:
    tr.set_write_buffer_limits(high=4 << 20, low=1 << 20)
```

It also answers `Expect: 100-continue` immediately; without that, `curl` waits
out its own 1 s timeout before sending an upload body (F-5).

**Always calibrate the origin on loopback before trusting any tunnel number.**
Patched, it does GET at ~10 GB/s and PUT at ~4.6 GB/s on the workstation. If it
reports tens of MB/s, the harness is broken, not bore.

### 9.4 The suites, in the order they should be run

| script | where it runs | what it answers | rough duration |
| --- | --- | --- | --- |
| `scripts/perf/vhost_remote_bench.sh v0` | VM | calibration: is the apparatus faster than the tunnel? | 2 min |
| `scripts/perf/vhost_remote_bench.sh v1` | VM | transport A/B, TCP relay vs QUIC direct, carriers 1/2/4 (§2.8) | 5 min |
| `scripts/perf/vhost_transport_ab.sh` | VM | **paired** transport A/B and carrier A/B — the design that survives the 29 % drift — plus the native latency suite (§2.15) | 25 min |
| `scripts/perf/vhost_bulk_latency.sh` | VM | F-15: request latency with 0/1/2 bulk transfers in flight, per carrier count, per transport (§2.16) | 12 min |
| `scripts/perf/vhost_remote_bench.sh v2` | VM | latency and request-rate suite (§2.9) | 15 min |
| `scripts/perf/vhost_ssh_gateway_bench.sh all` | VM | S1–S9, SSH ingress gateway vs native (§2.11) | 25 min |
| `scripts/perf/vhost_ssh_takeover_probe.sh` | VM | I-SSH5 same-identity takeover, with a discriminating origin (§2.11) | 2 min |
| `scripts/perf/vhost_registration_leak_repro.sh` | any host with a tunnel | the F-1 reproducer on its own | 5 min |
| `scripts/perf/vhost_remote_stability.sh all` | VM | G1–G9: leaks, races, reconnect storms, concurrency, memory (§2.13) | 40 min |
| `scripts/perf/vhost_netem_matrix.sh` | VM | protocol-selective loss and RTT per transport, plus the UDP-blackhole fallback case (§2.14) | 25 min |
| `scripts/perf/vhost_header_injection_ab.sh` | anywhere with ≥ 8 cores | F-4, on a private server — no staging access needed (§2.12) | 15 min |
| `scripts/perf/vhost_app_ceiling.sh` | anywhere with ≥ 4 cores | the application ceiling with the network removed, and whether bore scales across cores — no deployment access needed (§2.17.1) | 20 min |
| `scripts/perf/vhost_remote_efficiency.sh` | VM | staging's CPU seconds per GiB, relay vs QUIC, three alternating rounds (§2.17.2) | 6 min |
| `scripts/perf/vhost_remote_efficiency.sh saturate` | VM | whether throughput stalls on the guest CPU or the instance allowance (§2.17.3) | 6 min |
| `scripts/perf/server_cpu_sample.sh N IV` | workstation | server `/proc/stat` split and `steal`, sampled over ssh | run alongside the others |
| `scripts/perf/server_cpu_report.sh <file>` | workstation | renders the above as busy/usr/sys/softirq/STEAL per interval | — |
| `scripts/perf/server_ena_sample.sh N IV` | workstation | ENA instance-allowance counters — the only way to see hypervisor throttling from inside the guest (§2.17.3) | run alongside the others |

Run the CPU sampler **concurrently** with whatever suite you care about; it is
the only way to see that 82 % of the server's cost is kernel (F-7), and the only
way to compute cost per byte as host-total rather than container-only.

**The three-script sequence for "is the application the limit?"** is
`vhost_app_ceiling.sh` (code, no network), then `vhost_remote_efficiency.sh`
with `server_cpu_sample.sh` (cost per byte on the real path), then
`vhost_remote_efficiency.sh saturate` with `server_ena_sample.sh` (CPU wall
versus instance wall). Divide the target link rate by the measured s/GB to get
the core count it needs. Neither the loopback sweep nor a single deployment
throughput figure can answer the question alone: the first has no network and
the second cannot tell the code from the box.

### 9.5 How the results are made trustworthy

- **Never trust `curl`'s own rate for a tunnelled download.** Take the delta of
  the server's `relay_tx_bytes` from `/admin/api/v1/vhost` over wall time. That
  is what the server actually emitted.
- **Prove the data path per case.** `direct_stream_opens` increments only on the
  QUIC direct path. A `--udp` tunnel that silently fell back to relay looks
  identical otherwise, and would corrupt the whole A/B.
- **Interleave A and B and repeat.** The control drift on this server is **29 %**
  (§2.8): the identical case re-run ten cases later came back 29 % higher. Any
  claimed effect below that is noise, and several early "findings" died this way.
- **Watch for ghost registrations after every case**, not at the end. A leak is
  the failure mode that matters and it is invisible in throughput numbers.
- **Check the operator's own tunnels before and after.** G9 pushed the server
  into memory pressure and one unrelated tunnel reconnected (F-13). Cap
  concurrency tests on a 1 GiB host at well under 512 connections.

### 9.6 Harness pitfalls that cost real time

Each of these produced a wrong conclusion before it was found:

1. **`pgrep -f <pattern>` matches your own shell's command line.** It killed the
   harness mid-run three times, once silently. Enumerate with
   `ps -eo pid,args | grep -E '[v]m_ssh'` (bracket the first character) or
   `pgrep -x <exact program name>`, then kill explicit PIDs.
2. **Never blanket-`pkill bore`.** It matches unrelated tunnels on the same
   machine, including the operator's. Kill only PIDs you started. (This is a
   standing project rule, and it was violated once in this campaign.)
3. **A bare `wait` blocks on *every* child**, which includes the long-lived
   origin and the tunnel provider. Collect the PIDs you care about and
   `wait "$p"` on each. This hung the SSH suite for minutes and looked like a
   gateway stall.
4. **`kill -9` on `( sleep | ssh ... )` kills the subshell, not `ssh`.** The
   orphaned `ssh` kept the label, which read as a 60 s registration leak in the
   SSH gateway. Background `ssh -T ... < /dev/null` directly and kill *its* PID.
   Never pass `-N`: it opens no session channel, so the gateway's banner and its
   parameter warnings can never be delivered (I-SSH7).
5. **`tc` argument order:** `tc qdisc add dev <if> root ...`. A helper that
   appended `dev <if>` at the end silently failed, and G6 reported a healthy
   fallback that had never been impaired.
6. **Sample takeover and eviction on a schedule, with a discriminator.** The
   first same-identity-takeover test checked at 4 s and only asked whether *a*
   registration existed — takeover actually completes at 3–8 s, and telling the
   two sessions apart needs them pointed at *different* origins.
7. **`awk` prints `25,00` under an Italian locale.** Prefix `LC_ALL=C`.
8. **jq: `(...)/10` inside an object value needs full parentheses.**
9. **Verify the routes you benchmark exist.** `/50m` did not exist in the origin,
   so a set of leak scenarios killed providers *after* the transfer had already
   404'd and finished. Use `/stream/<bytes>`.
10. **A stale server holding the control port invalidates the run without
    failing it.** The first application-ceiling sweep produced real throughput
    at the wrong pinning with `cpu=0.00s threads=0`: a leftover server still
    owned the port and served every request, while the correctly pinned process
    had already exited with "address in use". The sweep was discarded. The fix
    is two guards — pre-check the ports, then confirm *your* PID logged
    `server listening` and is readable in `/proc` before measuring.
11. **A sweep spec separated by `:` with an empty leading field silently shifts
    the values.** `IFS=: read -r flags carriers parallel <<< "::1:1"` yields four
    fields for three variables, so `carriers` ends up empty and the client dies
    on `a value is required for '--carriers <N>'`. Use `|`.
12. **Cumulative counters need deltas, and a window.** `/proc/stat` and the ENA
    allowance counters both count since boot; a raw
    `pps_allowance_exceeded: 9205094` on an instance that has been up for weeks
    says nothing at all about the run in progress. Have the measuring script
    print the epoch window of each case, sample the server on a fixed interval
    from a separate process, and match them afterwards.
13. **Do not infer a ceiling from an unsaturated resource.** The single-stream
    figures of 196–242 MB/s looked like a ceiling and were quoted as one in
    working notes; the server was using 1.4 of 2 cores at the time, so nothing
    was saturated. Parallel streams then reached 334.7 MB/s at 1.90 cores. A
    plateau is only a ceiling once some resource is measurably full.

---

## 10. HTTP/2 on the vhost edge — the phase 07 spike

Phase 07 of `docs/plans/plan_VhostEnhancements/` exists so the largest
engineering item in the plan is not committed on the back of an estimate. The
estimate it was built on: connection setup measured **+45 ms** from a 21 ms-RTT
client but only **+4.9 ms** at 1.84 ms RTT (§2.15 A3), so ~4.9 ms is server time
and ~40 ms is the client's TLS round trips; a browser opens ~6 connections per
origin, so a 30-asset page pays several waves of that, and h2 collapses the
waves onto one connection.

This section measures it. **No production code was written for it.**

### 10.1 Apparatus (`scripts/vhost_h2_page_load.sh`)

The impaired leg must be the **client leg only** — the provider→server and
server→origin legs are localhost in a real deployment and must stay undelayed.
Loopback cannot express that: the kernel picks source `127.0.0.1` for every
loopback destination, so a `tc` filter on `dst 127.0.0.1/32` delays *every* hop.
So the client runs in its own network namespace behind a veth pair and netem
sits on the veth, which is the only leg it can touch. netem delay is applied at
RTT/2 on both ends; calibrated on a bare veth it is accurate to ~0.15 ms, and
every table below carries the *measured* RTT.

Three arms, same 31-asset page, same self-signed cert, same response bytes:

| arm | what it is |
| --- | --- |
| `tunnel-h1` | the product as it ships: HTTP/1.1 through the vhost TLS edge, `--parallel-max 6` |
| `direct-h1` | the same protocol with the tunnel removed — a node `https` server on the host |
| `direct-h2` | one multiplexed connection — a node `http2` server, same handler, same bytes |

`direct-h1` exists so the tunnel's own contribution can be separated from the
protocol's. Connection counts are reported from `curl`'s `%{num_connects}`
summed over the page, so multiplexing is proved rather than assumed (6 for the
h1 arms, 1 for h2 in every run).

Two apparatus caveats, stated because they bound what may be read off the
tables. First, `direct-h2` and `direct-h1` are node, `tunnel-h1` is bore, so any
tunnel-versus-direct row crosses implementations — that is visible in the
bulk-only table below, where bore's edge out-sends node's `https` by 300 ms at
100 ms RTT. The *protocol* comparison (`direct-h2` versus `direct-h1`) is
within one implementation and is the trustworthy one. Second, `curl -Z` is a
browser-shaped client, not a browser: no preconnect, no priority tree, no
render-blocking.

### 10.2 Measured — the prize depends entirely on what is on the page

**A realistic mixed page: 30 × ~1 KiB assets + one 2 MiB asset.**

| measured RTT | tunnel-h1 | direct-h1 | direct-h2 | h2 vs tunnel | h2 vs h1 (protocol alone) |
| --- | --- | --- | --- | --- | --- |
| 2.08 ms | 46.0 ms | 43.0 ms | 63.2 ms | **0.73×** | 0.68× |
| 21.18 ms | 348.0 ms | 327.7 ms | 326.7 ms | **1.07×** | 1.00× |
| 60.22 ms | 1036.0 ms | 1033.9 ms | 732.9 ms | **1.41×** | 1.41× |
| 100.14 ms | 1615.7 ms | 1613.3 ms | 1113.6 ms | **1.45×** | 1.45× |

**The same page with the large asset removed: 30 × ~1 KiB only.** This is the
wave structure on its own.

| measured RTT | tunnel-h1 | direct-h1 | direct-h2 | h2 vs tunnel | h2 vs h1 |
| --- | --- | --- | --- | --- | --- |
| 2.14 ms | 24.8 ms | 25.5 ms | 16.7 ms | **1.49×** | 1.53× |
| 21.16 ms | 161.0 ms | 158.2 ms | 94.5 ms | **1.70×** | 1.67× |
| 60.15 ms | 432.9 ms | 431.9 ms | 250.3 ms | **1.73×** | 1.73× |
| 100.14 ms | 713.5 ms | 712.2 ms | 410.2 ms | **1.74×** | 1.74× |

**Bulk on its own: the 2 MiB asset with no small assets.**

| measured RTT | tunnel-h1 | direct-h1 | direct-h2 | h2 vs h1 |
| --- | --- | --- | --- | --- |
| 2.11 ms | 38.9 ms | 38.0 ms | 73.8 ms | **0.51×** |
| 100.19 ms | 1210.5 ms | 1513.3 ms | 1110.1 ms | 1.36× |

Three results, and the third is the one that decides the phase.

1. **The tunnel is not the page-load problem.** `tunnel-h1` and `direct-h1` are
   within **1–3 ms of each other on a 31-asset page at every RTT** — 348.0 vs
   327.7 ms at 21 ms, 1036.0 vs 1033.9 ms at 60 ms. Whatever a page costs, bore
   is contributing single-digit milliseconds of it. The 45 ms per new connection
   §2.15 measured is the client's TLS round trips, and it is paid identically
   with or without the tunnel.
2. **On the wave structure alone, h2 wins everywhere and by a lot** — 1.49× at
   2 ms rising to 1.74× at 100 ms, saving 8 ms and 303 ms respectively. The
   estimate that motivated the phase is confirmed for this half.
3. **On bulk over one multiplexed connection, h2 loses half the throughput**
   — 0.51× at 2 ms RTT (2 MiB in 73.8 ms against 38.0 ms, i.e. 28 MB/s against
   55 MB/s on the same path, same server, same bytes). One connection carrying
   every stream is exactly what h2 *is*, so this is not a tuning oversight: it
   is the trade. It is also why the mixed page **reverses to 0.73× at 2 ms** and
   only reaches 1.07× at 21 ms.

So the prize is real, but it is a function of the viewer's RTT *and* of the
page's composition, and at the RTT of the same-region VM (the configuration
bore's throughput numbers are quoted at) an h2 edge would make the page
**slower**.

### 10.3 The graft (phase 07.2, read-only)

The plan expected the ALPN plumbing to be "already there". It is — but only on
one of the two listeners, and it deliberately does not negotiate.

- **Unified topology** (the staging shape: the control port *is* 443). The
  browser's TLS lands in `sshgw::accept_tls_with_alpn`, whose
  `demux_classify_alpn` sees `h2` offered, classifies it as "not SSH", and hands
  it to `Server::route_connection_known_http` → `serve_control_http`, which
  reads the request head and routes by `Host`. So the offer *does* arrive
  intact and the seam is a single function.
- **Standalone topology** (`--vhost-https-port` on its own listener).
  `vhost::handle_https` calls `acceptor.accept(stream)` on a plain rustls
  acceptor. There is no ALPN classification on this path at all.
- **Neither TLS config sets `alpn_protocols`** (`transport::load_server_tls`
  leaves it empty; only the *client* config advertises `bore` and the backend
  connector advertises `http/1.1`). A rustls server with no ALPN list ignores
  the offer, so every browser silently and correctly falls back to HTTP/1.1
  today. Nothing is half-enabled.

An h2 edge therefore needs **two** seams, not one, or a refactor that gives the
standalone frontend the same LazyConfigAcceptor treatment as the control port.

**Invariant collisions, named.**

1. **The response-header injection path is byte-level.**
   `vhost::relay_response_injected` reads the h1 response head with
   `read_head_async` and rewrites it with `rewrite_head`. h2 response headers
   are HPACK-encoded on the wire, so none of that applies as written; injection
   would have to move into the h2 header-frame encoder. This is the piece most
   likely to be underestimated, and it is also the piece with a bug and a fix
   behind it (`docs/VHOST_INJECTED_FLUSH_FIX.md`) — the flush-before-parking
   invariant is a property of the hand-rolled copy loop that an h2 body writer
   would replace entirely.
2. **The bulk path is a 256 KiB splice, and h2 would replace it with a frame
   layer.** `shared::proxy_buffer_size()` defaults to 256 KiB and `CLAUDE.md`
   already records that dropping to `tokio::io::copy`'s 8 KiB was a high-BDP
   regression. h2 frames at 16 KiB with per-stream flow control on top; the
   0.51× above is that cost, measured.
3. **Fixing (2) the obvious way is the trade DEC-VE8 forbids.** Raising h2's
   stream and connection windows to recover bulk throughput is "buy latency
   with buffer memory" on the edge, per connected browser — the same shape as
   the QUIC receive-window ceiling F-13 already priced at 4.5× the relay's
   memory under concurrency.
4. **The yamux single-task rule is *not* a collision.** Each h2 stream would map
   onto one proxied connection, which is already one task with one substream —
   the shape `mux` requires (`[[yamux-stream-split-wedge]]`). Mapping N h2
   streams onto N proxied connections needs no change to the muxer and no
   `tokio::io::split` across tasks. This is the reassuring finding.
5. **The origin leg stays HTTP/1.1.** It is usually localhost, where §2.15
   measured connection setup at 4.9 ms, so pooling or upgrading it buys almost
   nothing. That was original candidate 3 and remains untested-because-pointless.

**Effort, in subphases, if it were approved:**

| subphase | work | size |
| --- | --- | --- |
| 1 | negotiate `h2` in ALPN on both frontends (unified + standalone), behind a per-tunnel opt-in flag, default off | small |
| 2 | terminate h2 on the edge (hyper server), map each stream to one proxied connection, preserve `--max-conns` accounting per stream | **large** |
| 3 | re-express request/response header injection and the access log on encoded header frames | medium |
| 4 | flow-control and buffer policy so bulk does not regress; gate with `vhost_h2_page_load.sh` + the bulk-only arm | medium, and the risky one |
| 5 | 502/504 synthesis, keep-alive, upgrade/WebSocket and `CONNECT` fall-back to h1 for anything h2 cannot carry | medium |

Subphase 2 alone is larger than every phase of this plan except 03.

### 10.4 Recommendation (phase 07.3): **no-go for now, and the measurement is recorded so it is not re-asked from zero**

The plan's own criteria: *go* if the measured headroom at 60–100 ms is a
substantial fraction of page time **and** no invariant needs reworking; *no-go
or defer* if the headroom is modest at realistic RTTs, or if the audience is
mostly low-RTT.

- The headroom at 60–100 ms **is** substantial: 1.41–1.45× on a mixed page,
  303–502 ms off a page load.
- But it **reverses below ~20 ms RTT** (0.73× at 2 ms) because of a 0.51× bulk
  penalty that is intrinsic to single-connection multiplexing, and the whole
  campaign's throughput case is built on the low-RTT configuration.
- And it collides with the two vhost invariants that each have a shipped bug
  behind them (the injected-flush path and the 256 KiB splice), plus DEC-VE8 if
  the bulk penalty is bought back with window memory.
- Meanwhile the thing an h2 edge would fix is **not bore's cost**: the tunnel
  contributes 1–3 ms of a 31-asset page. Phase 03's carrier and bulk scheduling
  work on the same page shape is cheaper and does not touch the edge protocol.

Revisit if, and only if, a deployment shows a **predominantly >60 ms audience**
serving **small-asset-heavy** pages. In that case subphase 1 plus a *header-only*
h2 path (leaving bulk responses on h1 by content-length) would capture most of
the prize without subphase 4's risk — that hybrid is worth a spike of its own
before subphase 2 is funded.

**HTTP/3 stays out of scope**, as the phase specified: it would ride the QUIC
path, whose ceiling is 0.96 Gbit/s at 2.5× the CPU per byte (F-16).

---

## 11. Phase 03 verified in vivo — the bulk/small isolation actually works

Phase 03.5 asked for the calibration of the bulk threshold and the growth
timings. What it produced first is more valuable: **the acceptance measurement
for the whole of phase 03**, on an apparatus that removes the 29 % control drift
instead of fighting it.

### 11.1 Apparatus (`scripts/vhost_bulk_isolation.sh`)

The leg that queues under bulk is the **carrier** leg (server↔provider): bulk
bytes fill the yamux carrier and a small request's substream waits behind them.
So the provider, the origin *and* the measuring client all live in one network
namespace, the bore server lives on the host, and netem on the veth stands in
for the WAN — the campaign's own shape (the VM ran client + provider + origin,
the server was remote), with the RTT under our control and a private server
instead of frozen staging.

Small-request latency is `oha -c 1` keep-alive on a 1 KiB asset over a 6–8 s
window; bulk is 1 or 2 looped `/stream` transfers on the **same** tunnel, so each
one is a proxied connection pinned to a carrier for its life (N-5). Every point
also reports the live carrier count, the published `carrier_target`, and
`direct_stream_opens` — a `--udp` case that silently served over the relay must
not be readable as a direct-path result, which is the pitfall that cost a
retracted finding in §4.

### 11.2 Measured at 2.1 ms RTT — the DEC-VE7 regime

Two independent runs (6 s and 8 s windows); both are given where they differ.

| arm | bulk 0 | bulk 1 | bulk 2 | carriers observed |
| --- | --- | --- | --- | --- |
| relay `--carriers 1` | p50 4.26 / p95 4.46 | p50 4.78 / p95 6.21 | **p50 5.92 / p95 15.0–23.5** | 1 |
| relay `--carriers 4` | p50 4.29 / p95 4.51 | p50 4.33 / p95 5.40 | **p50 4.41 / p95 5.4–5.6** | 4 |
| relay `--carriers 0` (auto) | p50 4.29 / p95 4.52 | p50 4.44 / p95 5.69 | **p50 4.45 / p95 5.4–5.6** | **1 → 2 → 3** |
| direct `--udp --carriers 1` | p50 4.33 / p95 4.63 | p50 5.31 / p95 8.86 | p50 5.10 / p95 6.7–6.9 | 1 |
| direct `--udp --carriers 4` | p50 4.31 / p95 4.57 | p50 4.26 / p95 5.9 | p50 4.41 / p95 6.8–7.1 | 4 |

Four results.

1. **F-15 reproduces exactly on the legacy single-carrier path.** With two bulk
   transfers in flight, `--carriers 1` takes p50 from 4.26 to 5.92 ms and p95
   from 4.46 to **15.0–23.5 ms** — a 3.4–5.3× tail. Nothing about that is a
   staging artefact: it is the same shape on a private server with 2 ms of
   synthetic WAN.
2. **Phase 03.2's bulk-aware selection removes it.** `--carriers 4` holds p50 at
   4.41 ms and p95 at 5.4–5.6 ms under the same two bulk transfers — a **2.7–4.3×
   better tail** than `--carriers 1`, and only 1.2 ms above its own unloaded p95.
3. **Phase 03.3's adaptive pool matches the fixed pool while sizing itself.**
   `--carriers 0` starts at **one** carrier, grows to **two** under one bulk
   transfer and **three** under two, and lands on the same latency as
   `--carriers 4` (p50 4.45, p95 5.4–5.6). The growth is *observed* through the
   admin API's `carriers` and `carrier_target`, not inferred: the mechanism
   works end to end over a real control loop, which is what the unit and
   integration gates cannot prove on their own.
4. **DEC-VE7's target is met with room.** The target was a 7–15 ms p50 under
   bulk rather than the unloaded 2.5 ms. Measured p50 under two bulk transfers:
   **4.41 ms** (`--carriers 4`) and **4.45 ms** (`--carriers 0`), i.e. inside the
   band and near the unloaded figure. Phase 03.4's QUIC stream demotion shows
   the same direction on the direct path: `--udp --carriers 1` under two bulk
   transfers is p95 6.7–6.9 ms against the relay's 15.0–23.5 ms at the same
   carrier count.

### 11.3 Measured at 21.2 ms RTT — and one honest negative

At browser RTT everything is round-trip-dominated: p50 is 42.4–42.7 ms in every
arm (two round trips), and no carrier setting moves it, because there is no
queue to remove — the wait is the network. The tails still separate, and not in
the direct path's favour:

| arm | p95, bulk 0 | p95, bulk 1 | p95, bulk 2 |
| --- | --- | --- | --- |
| relay `--carriers 1` | 42.83 | 43.88 | **44.22** |
| relay `--carriers 4` | 42.90 | 42.83 | **42.80** |
| relay `--carriers 0` (auto) | 42.78 | 43.73 (grew to 2) | **42.76** (2) |
| direct `--udp --carriers 1` | 43.08 | 45.11 | **65.14** |
| direct `--udp --carriers 4` | 43.06 | 44.61 | **112.68** |

**The QUIC direct path's tail under concurrent bulk is worse than the relay's at
21 ms RTT, and worse still with four carriers** (112.7 ms against the relay's
42.8 ms). Phase 03.4's per-stream demotion caps a bulk sender's burst at 128 KiB
and drops its priority, which is enough at 2 ms but not at 21 ms, where a
demoted stream's next burst is a full round trip away. This is consistent with
the campaign's own transport ranking (§2.8, F-8: the relay is the right
transport on a clean path) and it is now also true of the *latency* tail under
load, not only of throughput. It is a reason to keep `--udp` for lossy and
long-RTT paths and not to reach for it under concurrency.

The adaptive pool reached target 2 rather than 3 at this RTT: growth is
rate-limited to one step per `CARRIER_TARGET_MIN_INTERVAL` (2 s) and the
measurement window was 6 s, of which the first seconds carry the warm-up. That
is the rate limit behaving as designed, not a failure to grow.

### 11.4 What is still not calibrated, and why it was not guessed

The 512 KiB bulk threshold, the 2 s growth interval and the 60 s quiet period
are **not** swept here. `pool::BULK_CLASSIFY_BYTES`,
`CARRIER_TARGET_MIN_INTERVAL` and `CARRIER_QUIET_PERIOD` are compile-time
constants with no environment override, so a sweep means a rebuild per value —
and adding an override to production code purely to sweep it would be a change
in a phase whose acceptance is already met. The measurement above says the
chosen values work at both RTTs; a sweep is worth doing the day one of them is
suspected, and this harness is where it goes.
